package github

import (
	"context"
	"errors"
	"fmt"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"time"

	sdk "github.com/google/go-github/v93/github"
)

// WorkItem is an issue or a pull request as the start-from-an-item flow reads
// it: enough to show it in a picker and to brief an agent.
type WorkItem struct {
	Repository  string // owner/name
	Number      int64
	PullRequest bool
	Title, URL  string
	State       string // open or closed
	StateReason string // completed, not_planned, reopened… (closed issues)
	Merged      bool
	Draft       bool
	Author      string
	Body        string
	Assignees   []string
	Labels      []string
	Comments    int
	UpdatedAt   time.Time
}

// Comment is one comment on an issue or pull request.
type Comment struct {
	Author, Body, URL string
	CreatedAt         time.Time
}

// IssueReader reads issues and pull requests across the connected account. It
// is separate from API so a client (or a test double) need not offer it.
type IssueReader interface {
	// SearchIssues runs a GitHub issue search (the search syntax, qualifiers
	// included) and returns one page, newest update first, and whether more follow.
	SearchIssues(ctx context.Context, token, query string, page int) ([]WorkItem, bool, error)
	// Issue reads one issue or pull request of owner/name.
	Issue(ctx context.Context, token, fullName string, number int64) (WorkItem, error)
	// IssueComments returns at most last of the most recent comments, oldest first.
	IssueComments(ctx context.Context, token, fullName string, number int64, last int) ([]Comment, error)
}

const searchPageSize = 30

// SearchIssues runs one page of an issue search.
func (c *Client) SearchIssues(ctx context.Context, token, query string, page int) ([]WorkItem, bool, error) {
	query = strings.TrimSpace(query)
	if query == "" || len(query) > 256 {
		return nil, false, errors.New("a GitHub search needs 1–256 characters")
	}
	if page < 1 || page > 34 {
		return nil, false, errors.New("GitHub search pages run from 1 to 34")
	}
	client, err := c.client(token)
	if err != nil {
		return nil, false, err
	}
	result, response, err := client.Search.Issues(ctx, query, &sdk.SearchOptions{Sort: "updated", Order: "desc", ListOptions: sdk.ListOptions{Page: page, PerPage: searchPageSize}})
	if err != nil {
		return nil, false, readError(err, response)
	}
	out := make([]WorkItem, 0, len(result.Issues))
	for _, issue := range result.Issues {
		if item, ok := mapIssue(issue, ""); ok {
			out = append(out, item)
		}
	}
	// The search API serves at most 1000 results.
	more := hasMore(response) && page*searchPageSize < 1000
	return out, more, nil
}

// Issue reads one issue or pull request.
func (c *Client) Issue(ctx context.Context, token, fullName string, number int64) (WorkItem, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return WorkItem{}, err
	}
	if number <= 0 || number > 1<<31-1 {
		return WorkItem{}, errors.New("an issue number is a positive number")
	}
	client, err := c.client(token)
	if err != nil {
		return WorkItem{}, err
	}
	issue, response, err := client.Issues.Get(ctx, owner, name, int(number))
	if err != nil {
		return WorkItem{}, readError(err, response)
	}
	item, ok := mapIssue(issue, fullName)
	if !ok {
		return WorkItem{}, errors.New("GitHub returned an issue without its number")
	}
	return item, nil
}

// IssueComments reads the tail of an issue's comments: the page that holds the
// last one and, when that page is short, the one before it.
func (c *Client) IssueComments(ctx context.Context, token, fullName string, number int64, last int) ([]Comment, error) {
	owner, name, err := splitName(fullName)
	if err != nil {
		return nil, err
	}
	if last <= 0 {
		return nil, nil
	}
	if last > 100 {
		last = 100
	}
	client, err := c.client(token)
	if err != nil {
		return nil, err
	}
	const per = 100
	read := func(page int) ([]Comment, int, error) {
		comments, response, err := client.Issues.ListComments(ctx, owner, name, int(number), &sdk.IssueListCommentsOptions{ListOptions: sdk.ListOptions{Page: page, PerPage: per}})
		if err != nil {
			return nil, 0, readError(err, response)
		}
		out := make([]Comment, 0, len(comments))
		for _, comment := range comments {
			out = append(out, Comment{Author: comment.GetUser().GetLogin(), Body: comment.GetBody(), URL: comment.GetHTMLURL(), CreatedAt: comment.GetCreatedAt().Time})
		}
		lastPage := 0
		if response != nil {
			lastPage = response.LastPage
		}
		return out, lastPage, nil
	}
	// The first page says where the last one is; most items have only one.
	out, lastPage, err := read(1)
	if err != nil {
		return nil, err
	}
	if lastPage > 1 {
		tail, _, err := read(lastPage)
		if err != nil {
			return nil, err
		}
		if len(tail) < last && lastPage > 2 {
			before, _, err := read(lastPage - 1)
			if err != nil {
				return nil, err
			}
			tail = append(before, tail...)
		} else if len(tail) < last {
			tail = append(out, tail...)
		}
		out = tail
	}
	if len(out) > last {
		out = out[len(out)-last:]
	}
	return out, nil
}

var issuePath = regexp.MustCompile(`^/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/(?:issues|pull)/([0-9]+)$`)

// RepositoryOfURL returns owner/name and the number of a GitHub issue or pull
// request page (https://github.com/owner/name/issues/12, …/pull/12).
func RepositoryOfURL(raw string) (string, int64, bool) {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil || (u.Scheme != "https" && u.Scheme != "http") || !strings.EqualFold(u.Host, "github.com") && !strings.EqualFold(u.Host, "www.github.com") {
		return "", 0, false
	}
	m := issuePath.FindStringSubmatch(strings.TrimSuffix(u.EscapedPath(), "/"))
	if m == nil {
		// A pull request's tab (/pull/12/files) is the pull request.
		parts := strings.Split(strings.Trim(u.EscapedPath(), "/"), "/")
		if len(parts) > 4 && parts[2] == "pull" {
			m = issuePath.FindStringSubmatch("/" + strings.Join(parts[:4], "/"))
		}
	}
	if m == nil {
		return "", 0, false
	}
	number, err := strconv.ParseInt(m[3], 10, 64)
	if err != nil || number <= 0 {
		return "", 0, false
	}
	return m[1] + "/" + m[2], number, true
}

func mapIssue(issue *sdk.Issue, fullName string) (WorkItem, bool) {
	if issue == nil || issue.GetNumber() <= 0 {
		return WorkItem{}, false
	}
	if fullName == "" {
		if repo, _, ok := RepositoryOfURL(issue.GetHTMLURL()); ok {
			fullName = repo
		} else if issue.Repository != nil {
			fullName = issue.Repository.GetFullName()
		}
	}
	if fullName == "" {
		return WorkItem{}, false
	}
	item := WorkItem{Repository: fullName, Number: int64(issue.GetNumber()), PullRequest: issue.IsPullRequest(), Title: issue.GetTitle(), URL: issue.GetHTMLURL(), State: issue.GetState(), StateReason: issue.GetStateReason(), Draft: issue.GetDraft(), Author: issue.GetUser().GetLogin(), Body: issue.GetBody(), Comments: issue.GetComments(), UpdatedAt: issue.GetUpdatedAt().Time}
	if issue.PullRequestLinks != nil && issue.PullRequestLinks.MergedAt != nil {
		item.Merged = true
	}
	if item.URL == "" {
		kind := "issues"
		if item.PullRequest {
			kind = "pull"
		}
		item.URL = fmt.Sprintf("https://github.com/%s/%s/%d", fullName, kind, item.Number)
	}
	for _, user := range issue.Assignees {
		if login := user.GetLogin(); login != "" {
			item.Assignees = append(item.Assignees, login)
		}
	}
	for _, label := range issue.Labels {
		if name := label.GetName(); name != "" {
			item.Labels = append(item.Labels, name)
		}
	}
	return item, true
}

// readError is apiError for reads a person can act on: a missing item and a
// refused search say so in words.
func readError(err error, response *sdk.Response) error {
	if response != nil && response.Response != nil {
		switch response.StatusCode {
		case 404, 410:
			return errors.New("GitHub could not find this issue with the current connection; check the link, and that the connection can read the repository's issues")
		case 422:
			return errors.New("GitHub could not run this search; simplify it and try again")
		}
	}
	return apiError(err, response)
}
