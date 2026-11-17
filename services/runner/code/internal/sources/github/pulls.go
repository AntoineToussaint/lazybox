package github

import (
	"context"
	"errors"
	"sort"
	"strings"
	"unicode"

	sdk "github.com/google/go-github/v93/github"
)

// ErrWriteForbidden is a GitHub refusal of a write the connection may read but
// not change: a read-only App installation or token, or an SSO policy.
var ErrWriteForbidden = errors.New("GitHub refused this change; the connection needs Contents and Pull requests write access (and SSO authorization for the organization)")

// Check is one CI signal on a commit: a check run or a commit status.
type Check struct {
	Name, Status, Conclusion, URL, Summary string
	// Source is "run" (a check run) or "status" (a commit status). A check
	// run's ID is a GitHub Actions job's ID when Actions ran it.
	Source     string
	ID         int64
	DetailsURL string
}

// PullRequestStatus is everything the PR loop shows about a branch's pull
// request. Found is false when the branch has none; DefaultBranch is still set.
type PullRequestStatus struct {
	DefaultBranch string
	Found         bool
	Number        int64
	Title, URL    string
	State         string // open, closed or merged
	Draft         bool
	Base, HeadSHA string
	// Head is the pull request's head branch.
	Head           string
	MergeableState string
	Mergeable      bool
	ReviewDecision string // approved, changes_requested, review_required or empty
	Checks         []Check
	ChecksState    string // success, failure, pending or none
	MergeCommit    string
	Author         string
}

// NewPullRequest is a pull request to open from a pushed branch of the same repository.
type NewPullRequest struct {
	Head, Base, Title, Body string
	Draft                   bool
}

// PullRequests is the provider boundary of the PR loop.
type PullRequests interface {
	PullRequestStatus(ctx context.Context, token, fullName, branch string) (PullRequestStatus, error)
	CreatePullRequest(ctx context.Context, token, fullName string, pr NewPullRequest) (PullRequestStatus, error)
	MergePullRequest(ctx context.Context, token, fullName string, number int64, method, expectedHead string) error
}

func splitName(fullName string) (string, string, error) {
	parts := strings.Split(fullName, "/")
	// "." and ".." are no repository: in an API path they would name another
	// endpoint, with the connection's token.
	if len(parts) != 2 || parts[0] == "" || parts[1] == "" || parts[0] == "." || parts[0] == ".." || parts[1] == "." || parts[1] == ".." {
		return "", "", errors.New("repository owner/name required")
	}
	return parts[0], parts[1], nil
}

// writeError keeps GitHub's own explanation for refusals a person can act on
// (branch protection, a pull request that already exists, a moved head). Only
// the message text is kept: never headers, URLs or the request.
func writeError(err error, response *sdk.Response) error {
	if err == nil {
		return nil
	}
	if response != nil && response.Response != nil {
		switch response.StatusCode {
		case 401:
			return ErrUnauthorized
		case 403:
			return ErrWriteForbidden
		case 404:
			return errors.New("GitHub could not find this repository or pull request with the current connection")
		case 405, 409, 422:
			var detail *sdk.ErrorResponse
			if errors.As(err, &detail) {
				if text := explain(detail); text != "" {
					return errors.New("GitHub: " + text)
				}
			}
		}
	}
	return apiError(err, response)
}

func explain(detail *sdk.ErrorResponse) string {
	parts := []string{}
	if detail.Message != "" && detail.Message != "Validation Failed" {
		parts = append(parts, detail.Message)
	}
	for _, e := range detail.Errors {
		if e.Message != "" {
			parts = append(parts, e.Message)
		}
	}
	text := strings.Map(func(r rune) rune {
		if unicode.IsControl(r) {
			return ' '
		}
		return r
	}, strings.Join(parts, "; "))
	if len(text) > 300 {
		text = text[:300] + "…"
	}
	return strings.TrimSpace(text)
}

func (c *Client) client(token string) (*sdk.Client, error) {
	client, err := c.apiClient(token)
	if err != nil {
		return nil, errors.New("could not initialize GitHub client")
	}
	return client, nil
}

// PullRequestStatus finds the branch's pull request (an open one first, else
// the most recent) and reads its mergeability, reviews and checks.
func (c *Client) PullRequestStatus(ctx context.Context, token, fullName, branch string) (PullRequestStatus, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return PullRequestStatus{}, err
	}
	if branch == "" {
		return PullRequestStatus{}, errors.New("branch required")
	}
	client, err := c.client(token)
	if err != nil {
		return PullRequestStatus{}, err
	}
	repo, response, err := client.Repositories.Get(ctx, owner, name)
	if err != nil {
		return PullRequestStatus{}, apiError(err, response)
	}
	out := PullRequestStatus{DefaultBranch: repo.GetDefaultBranch()}
	list, response, err := client.PullRequests.List(ctx, owner, name, &sdk.PullRequestListOptions{Head: owner + ":" + branch, State: "all", Sort: "created", Direction: "desc", ListOptions: sdk.ListOptions{PerPage: 10}})
	if err != nil {
		return out, apiError(err, response)
	}
	var chosen *sdk.PullRequest
	for _, pr := range list {
		if pr.GetHead().GetRef() != branch {
			continue
		}
		if chosen == nil || pr.GetState() == "open" && chosen.GetState() != "open" {
			chosen = pr
		}
	}
	if chosen == nil {
		return out, nil
	}
	return c.describe(ctx, client, owner, name, chosen.GetNumber(), out)
}

func (c *Client) describe(ctx context.Context, client *sdk.Client, owner, name string, number int, out PullRequestStatus) (PullRequestStatus, error) {
	// The list omits mergeability; the single read computes it.
	pr, response, err := client.PullRequests.Get(ctx, owner, name, number)
	if err != nil {
		return out, apiError(err, response)
	}
	out = pullStatus(pr, out)
	reviews, response, err := client.PullRequests.ListReviews(ctx, owner, name, number, &sdk.ListOptions{PerPage: 100})
	if err != nil {
		return out, apiError(err, response)
	}
	out.ReviewDecision = reviewDecision(reviews, len(pr.RequestedReviewers)+len(pr.RequestedTeams))
	if out.HeadSHA == "" {
		out.ChecksState = "none"
		return out, nil
	}
	runs, response, err := client.Checks.ListCheckRunsForRef(ctx, owner, name, out.HeadSHA, &sdk.ListCheckRunsOptions{ListOptions: sdk.ListOptions{PerPage: 100}})
	if err != nil {
		return out, apiError(err, response)
	}
	out.Checks = checkRuns(runs.CheckRuns)
	combined, response, err := client.Repositories.GetCombinedStatus(ctx, owner, name, out.HeadSHA, &sdk.ListOptions{PerPage: 100})
	if err != nil {
		return out, apiError(err, response)
	}
	out.Checks = append(out.Checks, commitStatuses(combined.Statuses)...)
	sort.SliceStable(out.Checks, func(i, j int) bool { return checkRank(out.Checks[i]) < checkRank(out.Checks[j]) })
	out.ChecksState = checksState(out.Checks)
	return out, nil
}

// reviewDecision mirrors GitHub's: each reviewer's latest approving or
// blocking review counts; a pending request with neither is review_required.
func reviewDecision(reviews []*sdk.PullRequestReview, requested int) string {
	latest := map[string]string{}
	for _, review := range reviews {
		switch state := review.GetState(); state {
		case "APPROVED", "CHANGES_REQUESTED", "DISMISSED":
			latest[review.GetUser().GetLogin()] = state
		}
	}
	approved := false
	for _, state := range latest {
		if state == "CHANGES_REQUESTED" {
			return "changes_requested"
		}
		approved = approved || state == "APPROVED"
	}
	if requested > 0 {
		return "review_required"
	}
	if approved {
		return "approved"
	}
	return ""
}

// Failing checks sort first, then running ones, so the list leads with what needs attention.
func checkRank(c Check) int {
	switch {
	case Failing(c):
		return 0
	case c.Status != "completed":
		return 1
	default:
		return 2
	}
}

// Failing is a completed check whose result needs a code or CI change.
func Failing(c Check) bool {
	switch c.Conclusion {
	case "failure", "timed_out", "action_required", "startup_failure":
		return true
	}
	return false
}

func checksState(checks []Check) string {
	if len(checks) == 0 {
		return "none"
	}
	state := "success"
	for _, c := range checks {
		if Failing(c) {
			return "failure"
		}
		if c.Status != "completed" {
			state = "pending"
		}
	}
	return state
}

// CreatePullRequest opens a pull request from head, a branch already pushed to
// the same repository, and returns its status. An empty base is the default branch.
func (c *Client) CreatePullRequest(ctx context.Context, token, fullName string, pr NewPullRequest) (PullRequestStatus, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return PullRequestStatus{}, err
	}
	if strings.TrimSpace(pr.Title) == "" || pr.Head == "" {
		return PullRequestStatus{}, errors.New("a pull request needs a title and a pushed branch")
	}
	client, err := c.client(token)
	if err != nil {
		return PullRequestStatus{}, err
	}
	repo, response, err := client.Repositories.Get(ctx, owner, name)
	if err != nil {
		return PullRequestStatus{}, apiError(err, response)
	}
	base := pr.Base
	if base == "" {
		base = repo.GetDefaultBranch()
	}
	if base == pr.Head {
		return PullRequestStatus{}, errors.New("this branch is the base branch; work on a feature branch to open a pull request")
	}
	title, body, draft := strings.TrimSpace(pr.Title), pr.Body, pr.Draft
	created, response, err := client.PullRequests.Create(ctx, owner, name, sdk.CreatePullRequest{Title: &title, Head: pr.Head, Base: base, Body: &body, Draft: &draft})
	if err != nil {
		return PullRequestStatus{}, writeError(err, response)
	}
	return c.describe(ctx, client, owner, name, created.GetNumber(), PullRequestStatus{DefaultBranch: repo.GetDefaultBranch()})
}

// MergePullRequest merges only if the head is still expectedHead, so a push
// after the person looked is never merged unseen. Branch protection refusals
// come back as GitHub states them.
func (c *Client) MergePullRequest(ctx context.Context, token, fullName string, number int64, method, expectedHead string) error {
	owner, name, err := splitName(fullName)
	if err != nil {
		return err
	}
	switch method {
	case "squash", "merge", "rebase":
	default:
		return errors.New("merge method must be squash, merge or rebase")
	}
	if number <= 0 || expectedHead == "" {
		return errors.New("pull request number and reviewed head required")
	}
	client, err := c.client(token)
	if err != nil {
		return err
	}
	result, response, err := client.PullRequests.Merge(ctx, owner, name, int(number), "", &sdk.PullRequestOptions{MergeMethod: method, SHA: expectedHead})
	if err != nil {
		return writeError(err, response)
	}
	if !result.GetMerged() {
		if text := strings.TrimSpace(result.GetMessage()); text != "" {
			return errors.New("GitHub did not merge: " + text)
		}
		return errors.New("GitHub did not merge this pull request")
	}
	return nil
}
