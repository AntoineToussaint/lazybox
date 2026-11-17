package github

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"sort"
	"strings"
	"time"

	sdk "github.com/google/go-github/v93/github"
)

// Tracking reads what Lazybox keeps of the pull requests a workspace
// delivers, with conditional requests: GitHub answers an unchanged resource
// with 304 Not Modified, which costs no rate limit.

// RateLimitError is GitHub asking the client to wait (a primary or secondary
// rate limit, HTTP 403 or 429). Reset is when it may read again, when known.
type RateLimitError struct{ Reset time.Time }

func (e *RateLimitError) Error() string {
	return "GitHub's rate limit was reached; reading again later"
}

// ErrActionsLogs is GitHub refusing a job's logs to this connection.
var ErrActionsLogs = errors.New("GitHub refused the job's logs: the connection needs Actions read access (a fine-grained token: Repository permissions → Actions → Read-only; the GitHub App: Actions read)")

// ErrLogsGone is a job whose logs GitHub no longer keeps.
var ErrLogsGone = errors.New("GitHub no longer has this job's logs")

// trackError keeps a rate limit apart, so tracking can wait for its reset.
func trackError(err error, response *sdk.Response) error {
	var limit *sdk.RateLimitError
	if errors.As(err, &limit) {
		return &RateLimitError{Reset: limit.Rate.Reset.Time}
	}
	var abuse *sdk.AbuseRateLimitError
	if errors.As(err, &abuse) {
		out := &RateLimitError{}
		if abuse.RetryAfter != nil {
			out.Reset = time.Now().Add(*abuse.RetryAfter)
		}
		return out
	}
	if response != nil && response.Response != nil && response.StatusCode == http.StatusTooManyRequests {
		return &RateLimitError{}
	}
	return apiError(err, response)
}

// get reads path into out, conditionally when etag is set. notModified means
// GitHub answered 304 and out is untouched.
func get(ctx context.Context, client *sdk.Client, path, etag string, out any) (newETag string, notModified bool, err error) {
	req, err := client.NewRequest(ctx, http.MethodGet, path, nil)
	if err != nil {
		return "", false, errors.New("invalid GitHub request")
	}
	if etag != "" {
		req.Header.Set("If-None-Match", etag)
	}
	response, err := client.Do(req, out)
	if response != nil && response.Response != nil && response.StatusCode == http.StatusNotModified {
		return etag, true, nil
	}
	if err != nil {
		return "", false, trackError(err, response)
	}
	return response.Header.Get("ETag"), false, nil
}

// PullETags are the validators of a pull request's parts, and the head they
// were read on (the head's checks are other resources once it moves).
type PullETags struct {
	Pull     string `json:"pull,omitempty"`
	Runs     string `json:"runs,omitempty"`
	Statuses string `json:"statuses,omitempty"`
	Head     string `json:"head,omitempty"`
}

// PullRead is one read of a pull request by number.
type PullRead struct {
	Status PullRequestStatus
	// Runs and Statuses are the head's check runs and commit statuses: a
	// later read that finds one of them unchanged reuses it.
	Runs, Statuses       []Check
	CreatedAt, UpdatedAt time.Time
	ClosedAt             time.Time
	ETags                PullETags
	// NotModified: nothing changed since previous.
	NotModified bool
}

// ReadPullRequest reads a pull request, its reviews and its head's checks.
// With previous and its validators, each unchanged part costs nothing and
// is taken from previous; reviews are read again only when the pull request
// itself changed (a submitted review changes it).
func (c *Client) ReadPullRequest(ctx context.Context, token, fullName string, number int64, previous *PullRead) (PullRead, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return PullRead{}, err
	}
	if number <= 0 {
		return PullRead{}, errors.New("pull request number required")
	}
	client, err := c.client(token)
	if err != nil {
		return PullRead{}, err
	}
	var etags PullETags
	if previous != nil {
		etags = previous.ETags
	}
	out := PullRead{}
	var pr sdk.PullRequest
	pullTag, pullSame, err := get(ctx, client, fmt.Sprintf("repos/%s/%s/pulls/%d", owner, name, number), etags.Pull, &pr)
	if err != nil {
		return PullRead{}, err
	}
	out.ETags.Pull = pullTag
	if pullSame {
		out.Status, out.CreatedAt, out.UpdatedAt, out.ClosedAt = previous.Status, previous.CreatedAt, previous.UpdatedAt, previous.ClosedAt
		out.Status.Checks, out.Status.ChecksState = nil, ""
	} else {
		out.Status = pullStatus(&pr, out.Status)
		out.CreatedAt, out.UpdatedAt = pr.GetCreatedAt().Time, pr.GetUpdatedAt().Time
		out.ClosedAt = pr.GetClosedAt().Time
		reviews, response, err := client.PullRequests.ListReviews(ctx, owner, name, int(number), &sdk.ListOptions{PerPage: 100})
		if err != nil {
			return PullRead{}, trackError(err, response)
		}
		out.Status.ReviewDecision = reviewDecision(reviews, len(pr.RequestedReviewers)+len(pr.RequestedTeams))
	}
	head := out.Status.HeadSHA
	out.ETags.Head = head
	sameHead := previous != nil && etags.Head == head && head != ""
	runsSame, statusesSame := false, false
	if head != "" {
		runsTag, statusesTag := "", ""
		if sameHead {
			runsTag, statusesTag = etags.Runs, etags.Statuses
		}
		var runs sdk.ListCheckRunsResults
		if out.ETags.Runs, runsSame, err = get(ctx, client, fmt.Sprintf("repos/%s/%s/commits/%s/check-runs?per_page=100", owner, name, url.PathEscape(head)), runsTag, &runs); err != nil {
			return PullRead{}, err
		}
		if runsSame {
			out.Runs = previous.Runs
		} else {
			out.Runs = checkRuns(runs.CheckRuns)
		}
		var combined sdk.CombinedStatus
		if out.ETags.Statuses, statusesSame, err = get(ctx, client, fmt.Sprintf("repos/%s/%s/commits/%s/status?per_page=100", owner, name, url.PathEscape(head)), statusesTag, &combined); err != nil {
			return PullRead{}, err
		}
		if statusesSame {
			out.Statuses = previous.Statuses
		} else {
			out.Statuses = commitStatuses(combined.Statuses)
		}
	}
	out.Status.Checks = append(append([]Check{}, out.Runs...), out.Statuses...)
	sort.SliceStable(out.Status.Checks, func(i, j int) bool { return checkRank(out.Status.Checks[i]) < checkRank(out.Status.Checks[j]) })
	out.Status.ChecksState = checksState(out.Status.Checks)
	out.NotModified = pullSame && runsSame && statusesSame
	return out, nil
}

// pullStatus is a pull request's own fields, as describe reads them.
func pullStatus(pr *sdk.PullRequest, out PullRequestStatus) PullRequestStatus {
	out.Found = true
	out.Number = int64(pr.GetNumber())
	out.Title = pr.GetTitle()
	out.URL = pr.GetHTMLURL()
	out.State = pr.GetState()
	if pr.GetMerged() || pr.MergedAt != nil {
		out.State = "merged"
		out.MergeCommit = pr.GetMergeCommitSHA()
	}
	out.Draft = pr.GetDraft()
	out.Base = pr.GetBase().GetRef()
	out.Head = pr.GetHead().GetRef()
	out.HeadSHA = pr.GetHead().GetSHA()
	out.MergeableState = pr.GetMergeableState()
	out.Mergeable = pr.GetMergeable()
	out.Author = pr.GetUser().GetLogin()
	return out
}

func checkRuns(runs []*sdk.CheckRun) []Check {
	var out []Check
	for _, run := range runs {
		link := run.GetHTMLURL()
		if link == "" {
			link = run.GetDetailsURL()
		}
		summary := ""
		if run.Output != nil {
			summary = run.Output.GetTitle()
		}
		out = append(out, Check{Name: run.GetName(), Status: run.GetStatus(), Conclusion: run.GetConclusion(), URL: link, Summary: summary, Source: "run", ID: run.GetID(), DetailsURL: run.GetDetailsURL()})
	}
	return out
}

func commitStatuses(statuses []*sdk.RepoStatus) []Check {
	var out []Check
	for _, status := range statuses {
		check := Check{Name: status.GetContext(), Status: "completed", URL: status.GetTargetURL(), Summary: status.GetDescription(), Source: "status"}
		switch status.GetState() {
		case "success":
			check.Conclusion = "success"
		case "failure", "error":
			check.Conclusion = "failure"
		default:
			check.Status, check.Conclusion = "in_progress", ""
		}
		out = append(out, check)
	}
	return out
}

// BranchPull is a pull request found by its head branch.
type BranchPull struct {
	Number int64
	State  string
	Head   string
	// HeadSHA is the commit the pull request's head is on.
	HeadSHA   string
	CreatedAt time.Time
}

// PullRequestsForBranch lists the pull requests whose head is branch in the
// repository (open and closed, newest first), conditionally on etag.
func (c *Client) PullRequestsForBranch(ctx context.Context, token, fullName, branch, etag string) ([]BranchPull, string, bool, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return nil, "", false, err
	}
	if branch == "" {
		return nil, "", false, errors.New("branch required")
	}
	client, err := c.client(token)
	if err != nil {
		return nil, "", false, err
	}
	var list []*sdk.PullRequest
	query := url.Values{"head": {owner + ":" + branch}, "state": {"all"}, "sort": {"created"}, "direction": {"desc"}, "per_page": {"10"}}
	tag, same, err := get(ctx, client, fmt.Sprintf("repos/%s/%s/pulls?%s", owner, name, query.Encode()), etag, &list)
	if err != nil || same {
		return nil, tag, same, err
	}
	var out []BranchPull
	for _, pr := range list {
		if pr.GetHead().GetRef() != branch {
			continue
		}
		state := pr.GetState()
		if pr.MergedAt != nil {
			state = "merged"
		}
		out = append(out, BranchPull{Number: int64(pr.GetNumber()), State: state, Head: branch, HeadSHA: pr.GetHead().GetSHA(), CreatedAt: pr.GetCreatedAt().Time})
	}
	return out, tag, false, nil
}

// JobLogLimit bounds a job's log as read: the full log is saved, so a
// person or an agent can read every line of it.
const JobLogLimit = 32 << 20

// JobLog reads a GitHub Actions job's log. GitHub answers with a redirect to
// short-lived storage, which is followed without the connection's credential.
func (c *Client) JobLog(ctx context.Context, token, fullName string, jobID int64) ([]byte, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return nil, err
	}
	if jobID <= 0 {
		return nil, errors.New("job required")
	}
	base, err := url.Parse(strings.TrimRight(c.apiURL, "/") + "/")
	if err != nil {
		return nil, errors.New("invalid GitHub API address")
	}
	target := base.JoinPath("repos", owner, name, "actions", "jobs", fmt.Sprint(jobID), "logs")
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, target.String(), nil)
	if err != nil {
		return nil, errors.New("invalid GitHub request")
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("User-Agent", "Lazybox-V2")
	noFollow := &http.Client{Timeout: 30 * time.Second, Transport: c.http.Transport, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	response, err := noFollow.Do(req)
	if err != nil {
		return nil, errors.New("GitHub request failed; check the network connection")
	}
	defer response.Body.Close()
	switch response.StatusCode {
	case http.StatusOK:
		return readLog(response.Body)
	case http.StatusFound, http.StatusMovedPermanently, http.StatusTemporaryRedirect, http.StatusSeeOther:
	case http.StatusUnauthorized:
		return nil, ErrUnauthorized
	case http.StatusForbidden:
		if response.Header.Get("X-RateLimit-Remaining") == "0" {
			return nil, &RateLimitError{}
		}
		return nil, ErrActionsLogs
	case http.StatusNotFound, http.StatusGone:
		return nil, ErrLogsGone
	default:
		return nil, fmt.Errorf("GitHub returned HTTP %d for the job's logs", response.StatusCode)
	}
	location, err := base.Parse(response.Header.Get("Location"))
	// The storage link is HTTPS; a test server's own address is the one exception.
	if err != nil || (location.Scheme != "https" && location.Host != base.Host) {
		return nil, errors.New("GitHub returned an invalid logs location")
	}
	storage, err := http.NewRequestWithContext(ctx, http.MethodGet, location.String(), nil)
	if err != nil {
		return nil, errors.New("invalid logs location")
	}
	storage.Header.Set("User-Agent", "Lazybox-V2")
	fetched, err := (&http.Client{Timeout: 60 * time.Second, Transport: c.http.Transport}).Do(storage)
	if err != nil {
		return nil, errors.New("the job's logs could not be downloaded; check the network connection")
	}
	defer fetched.Body.Close()
	if fetched.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("the job's logs could not be downloaded (HTTP %d)", fetched.StatusCode)
	}
	return readLog(fetched.Body)
}

func readLog(body io.Reader) ([]byte, error) {
	data, err := io.ReadAll(io.LimitReader(body, JobLogLimit))
	if err != nil {
		return nil, errors.New("the job's logs could not be read")
	}
	return data, nil
}

// ReviewComment is one inline comment of a pull request review.
type ReviewComment struct {
	ID        int64
	ReviewID  int64
	InReplyTo int64
	Path      string
	// Line is the last line the comment covers, StartLine the first (0 for
	// one line), on Side: RIGHT (the change's new lines) or LEFT (removed).
	Line, StartLine int
	Side            string
	// Outdated: the lines changed since; GitHub keeps only the original ones.
	Outdated     bool
	OriginalLine int
	Body, Author string
	// Association is the author's relation to the repository, as GitHub
	// states it: OWNER, MEMBER, COLLABORATOR, CONTRIBUTOR, NONE, ….
	Association string
	URL         string
	DiffHunk    string
	CommitID    string
	CreatedAt   time.Time
}

// Review is one submitted review.
type Review struct {
	ID     int64
	Author string
	// Association is as ReviewComment's.
	Association string
	State       string
	Body        string
	URL         string
	SubmittedAt time.Time
}

// ReviewComments lists a pull request's inline review comments, oldest first
// (at most 300), and its submitted reviews.
func (c *Client) ReviewComments(ctx context.Context, token, fullName string, number int64) ([]ReviewComment, []Review, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return nil, nil, err
	}
	client, err := c.client(token)
	if err != nil {
		return nil, nil, err
	}
	var comments []ReviewComment
	for page := 1; page <= 3; page++ {
		list, response, err := client.PullRequests.ListComments(ctx, owner, name, int(number), &sdk.PullRequestListCommentsOptions{Sort: "created", Direction: "asc", ListOptions: sdk.ListOptions{Page: page, PerPage: 100}})
		if err != nil {
			return nil, nil, trackError(err, response)
		}
		for _, c := range list {
			comments = append(comments, ReviewComment{ID: c.GetID(), ReviewID: c.GetPullRequestReviewID(), InReplyTo: c.GetInReplyTo(), Path: c.GetPath(),
				Line: c.GetLine(), StartLine: c.GetStartLine(), Side: c.GetSide(), Outdated: c.Line == nil, OriginalLine: c.GetOriginalLine(),
				Body: c.GetBody(), Author: c.GetUser().GetLogin(), Association: c.GetAuthorAssociation(), URL: c.GetHTMLURL(), DiffHunk: c.GetDiffHunk(), CommitID: c.GetCommitID(), CreatedAt: c.GetCreatedAt().Time})
		}
		if !hasMore(response) {
			break
		}
	}
	list, response, err := client.PullRequests.ListReviews(ctx, owner, name, int(number), &sdk.ListOptions{PerPage: 100})
	if err != nil {
		return nil, nil, trackError(err, response)
	}
	var reviews []Review
	for _, r := range list {
		reviews = append(reviews, Review{ID: r.GetID(), Author: r.GetUser().GetLogin(), Association: r.GetAuthorAssociation(), State: r.GetState(), Body: r.GetBody(), URL: r.GetHTMLURL(), SubmittedAt: r.GetSubmittedAt().Time})
	}
	return comments, reviews, nil
}

// PullRequestByNumber reads a pull request by its number, as the PR loop
// reads a branch's: for merging a tracked pull request on any branch.
func (c *Client) PullRequestByNumber(ctx context.Context, token, fullName string, number int64) (PullRequestStatus, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return PullRequestStatus{}, err
	}
	client, err := c.client(token)
	if err != nil {
		return PullRequestStatus{}, err
	}
	repo, response, err := client.Repositories.Get(ctx, owner, name)
	if err != nil {
		return PullRequestStatus{}, apiError(err, response)
	}
	return c.describe(ctx, client, owner, name, int(number), PullRequestStatus{DefaultBranch: repo.GetDefaultBranch()})
}
