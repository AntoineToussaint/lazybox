package workitems

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"strconv"
	"strings"
	"time"
	"unicode"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// GitHub is the built-in provider: issues and pull requests the connected
// GitHub account can read. It reads only; it never changes an issue.
type GitHub struct {
	Reader github.IssueReader
	// WithCredential runs fn with the Runner's GitHub token (githubconnection);
	// nil when GitHub is not set up.
	WithCredential func(ctx context.Context, fn func(token string) error) error
	Now            func() time.Time
}

func (GitHub) ID() string   { return "github" }
func (GitHub) Name() string { return "GitHub" }

var githubKey = regexp.MustCompile(`^([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)#([0-9]{1,10})$`)

// GitHubKey is an item's key: owner/name#number.
func GitHubKey(fullName string, number int64) string { return fmt.Sprintf("%s#%d", fullName, number) }

// ParseGitHubKey splits owner/name#number.
func ParseGitHubKey(key string) (string, int64, bool) {
	m := githubKey.FindStringSubmatch(strings.TrimSpace(key))
	if m == nil {
		return "", 0, false
	}
	number, err := strconv.ParseInt(m[3], 10, 64)
	if err != nil || number <= 0 {
		return "", 0, false
	}
	return m[1] + "/" + m[2], number, true
}

// Resolve accepts an issue or pull request URL on github.com, or owner/name#N.
func (GitHub) Resolve(reference string) (string, bool) {
	reference = strings.TrimSpace(reference)
	if full, number, ok := github.RepositoryOfURL(reference); ok {
		return GitHubKey(full, number), true
	}
	if full, number, ok := ParseGitHubKey(reference); ok {
		return GitHubKey(full, number), true
	}
	return "", false
}

func (g GitHub) credential(ctx context.Context, fn func(token string) error) error {
	if g.Reader == nil || g.WithCredential == nil {
		return ErrNotConnected
	}
	return g.WithCredential(ctx, fn)
}

func (g GitHub) now() time.Time {
	if g.Now != nil {
		return g.Now()
	}
	return time.Now()
}

// SearchQuery is the GitHub search a picker query runs: open issues, assigned
// to you (mine) or involving you (recent), with the person's words.
func SearchQuery(query, scope string) (string, error) {
	words := strings.Map(func(r rune) rune {
		if unicode.IsControl(r) {
			return ' '
		}
		return r
	}, strings.TrimSpace(query))
	words = strings.Join(strings.Fields(words), " ")
	if len(words) > 200 {
		return "", errors.New("search for at most 200 characters")
	}
	q := []string{"is:issue", "is:open", "archived:false"}
	switch scope {
	case "", ScopeMine:
		q = append(q, "assignee:@me")
	case ScopeRecent:
		q = append(q, "involves:@me")
	case ScopeAll:
		if words == "" {
			return "", nil
		}
	default:
		return "", fmt.Errorf("unknown search scope %q: mine, recent or all", scope)
	}
	if words != "" {
		q = append(q, words)
	}
	return strings.Join(q, " "), nil
}

func (g GitHub) Search(ctx context.Context, query, scope, cursor string) (*pb.ItemPage, error) {
	// A query that names one item is that item, whatever the scope.
	if key, ok := g.Resolve(query); ok {
		detail, err := g.Get(ctx, key, 0)
		if err != nil {
			return nil, err
		}
		return &pb.ItemPage{Items: []*pb.Item{detail.Item}}, nil
	}
	q, err := SearchQuery(query, scope)
	if err != nil || q == "" {
		return &pb.ItemPage{}, err
	}
	page := 1
	if cursor != "" {
		if page, err = strconv.Atoi(cursor); err != nil || page < 1 {
			return nil, errors.New("invalid search cursor")
		}
	}
	out := &pb.ItemPage{}
	err = g.credential(ctx, func(token string) error {
		items, more, err := g.Reader.SearchIssues(ctx, token, q, page)
		if err != nil {
			return err
		}
		now := g.now()
		for _, item := range items {
			out.Items = append(out.Items, GitHubItem(item, now))
		}
		if more {
			out.Next = strconv.Itoa(page + 1)
		}
		return nil
	})
	return out, err
}

func (g GitHub) Get(ctx context.Context, key string, comments int) (Detail, error) {
	full, number, ok := ParseGitHubKey(key)
	if !ok {
		return Detail{}, fmt.Errorf("%q is not a GitHub issue: write owner/name#number or paste its link", key)
	}
	if comments < 0 || comments > 50 {
		comments = 50
	}
	var out Detail
	err := g.credential(ctx, func(token string) error {
		item, err := g.Reader.Issue(ctx, token, full, number)
		if err != nil {
			return err
		}
		out = Detail{Item: GitHubItem(item, g.now()), Body: item.Body, CommentCount: item.Comments}
		if comments == 0 || item.Comments == 0 {
			return nil
		}
		read, err := g.Reader.IssueComments(ctx, token, full, number, comments)
		if err != nil {
			return err
		}
		for _, c := range read {
			comment := &pb.ItemComment{Author: c.Author, Body: c.Body, Url: c.URL}
			if !c.CreatedAt.IsZero() {
				comment.CreatedAtUnixMillis = c.CreatedAt.UnixMilli()
			}
			out.Comments = append(out.Comments, comment)
		}
		return nil
	})
	return out, err
}

// GitHubItem is a GitHub issue or pull request as the protocol's Item.
func GitHubItem(w github.WorkItem, observed time.Time) *pb.Item {
	item := &pb.Item{Ref: &pb.ItemRef{Provider: "github", Key: GitHubKey(w.Repository, w.Number)}, Kind: "issue", Title: w.Title, Url: w.URL,
		Repository: "github:" + strings.ToLower(w.Repository), Assignees: w.Assignees, Labels: w.Labels, Author: w.Author, ObservedAtUnixMillis: observed.UnixMilli()}
	if !w.UpdatedAt.IsZero() {
		item.UpdatedAtUnixMillis = w.UpdatedAt.UnixMilli()
	}
	switch {
	case w.PullRequest && w.Merged:
		item.State = &pb.ItemState{Category: "merged", Label: "Merged"}
	case w.PullRequest && w.State != "open":
		item.State = &pb.ItemState{Category: "closed", Label: "Closed"}
	case w.PullRequest && w.Draft:
		item.State = &pb.ItemState{Category: "started", Label: "Draft"}
	case w.PullRequest:
		item.State = &pb.ItemState{Category: "review", Label: "Open"}
	case w.State == "open":
		item.State = &pb.ItemState{Category: "open", Label: "Open"}
	case w.StateReason == "not_planned":
		item.State = &pb.ItemState{Category: "closed", Label: "Closed as not planned"}
	case w.StateReason == "duplicate":
		item.State = &pb.ItemState{Category: "closed", Label: "Closed as duplicate"}
	default:
		item.State = &pb.ItemState{Category: "done", Label: "Closed"}
	}
	if w.PullRequest {
		item.Kind = "pull_request"
	} else {
		// The workspace works in the issue's own repository, so the short form links it.
		item.ClosingReference = fmt.Sprintf("Fixes #%d", w.Number)
	}
	return item
}
