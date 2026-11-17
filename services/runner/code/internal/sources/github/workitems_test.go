package github_test

import (
	"context"
	"fmt"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
)

func TestSearchIssuesSendsTheQueryAndMapsResults(t *testing.T) {
	fake := githubtest.New(t, "ghp_fixture")
	fake.AddIssue("acme/api", githubtest.Issue{Number: 812, Title: "Fix refund rounding", Body: "Refunds round down.", Assignees: []string{"fixture"}, Labels: []string{"bug", "payments"}, UpdatedAt: "2026-11-10T09:00:00Z"})
	fake.AddIssue("acme/api", githubtest.Issue{Number: 9, Title: "Somebody else's", Assignees: []string{"other"}, UpdatedAt: "2026-11-11T09:00:00Z"})
	fake.AddIssue("acme/web", githubtest.Issue{Number: 3, Title: "Closed one", State: "closed", StateReason: "completed", Assignees: []string{"fixture"}})
	client := github.NewAt(fake.URL)
	var reader github.IssueReader = client
	items, more, err := reader.SearchIssues(context.Background(), "ghp_fixture", "is:issue is:open assignee:@me", 1)
	if err != nil || more || len(items) != 1 {
		t.Fatal(items, more, err)
	}
	got := items[0]
	if got.Repository != "acme/api" || got.Number != 812 || got.Title != "Fix refund rounding" || got.URL != "https://github.com/acme/api/issues/812" || got.PullRequest || got.State != "open" ||
		strings.Join(got.Labels, ",") != "bug,payments" || strings.Join(got.Assignees, ",") != "fixture" || got.UpdatedAt.IsZero() {
		t.Fatalf("mapped %+v", got)
	}
	if fake.Issues.Searches[0] != "is:issue is:open assignee:@me" {
		t.Fatalf("sent %q", fake.Issues.Searches)
	}
	if _, _, err = reader.SearchIssues(context.Background(), "ghp_fixture", "  ", 1); err == nil {
		t.Fatal("an empty search was sent")
	}
	if _, _, err = reader.SearchIssues(context.Background(), "wrong", "is:open", 1); err != github.ErrUnauthorized {
		t.Fatalf("bad token: %v", err)
	}
}

func TestIssueAndTheTailOfItsComments(t *testing.T) {
	fake := githubtest.New(t, "ghp_fixture")
	comments := []githubtest.IssueComment{}
	for i := 1; i <= 205; i++ {
		comments = append(comments, githubtest.IssueComment{Author: fmt.Sprintf("user%d", i), Body: fmt.Sprintf("comment %d", i), CreatedAt: "2026-11-01T00:00:00Z"})
	}
	fake.AddIssue("acme/api", githubtest.Issue{Number: 812, Title: "Long thread", Body: "Body", Author: "alice", Comments: comments})
	fake.AddIssue("acme/api", githubtest.Issue{Number: 2, Title: "Short", Comments: comments[:3]})
	fake.AddIssue("acme/api", githubtest.Issue{Number: 13, Title: "A pull request", PullRequest: true, Merged: true, State: "closed"})
	client := github.NewAt(fake.URL)
	ctx := context.Background()
	issue, err := client.Issue(ctx, "ghp_fixture", "acme/api", 812)
	if err != nil || issue.Author != "alice" || issue.Body != "Body" || issue.Comments != 205 {
		t.Fatal(issue, err)
	}
	// 205 comments are three pages of 100: the last ten span the last two.
	tail, err := client.IssueComments(ctx, "ghp_fixture", "acme/api", 812, 10)
	if err != nil || len(tail) != 10 || tail[0].Body != "comment 196" || tail[9].Body != "comment 205" || tail[9].Author != "user205" || tail[9].CreatedAt.IsZero() || !strings.Contains(tail[9].URL, "#issuecomment-205") {
		t.Fatalf("tail %+v %v", tail, err)
	}
	if tail, err = client.IssueComments(ctx, "ghp_fixture", "acme/api", 2, 10); err != nil || len(tail) != 3 || tail[0].Body != "comment 1" {
		t.Fatalf("short thread %+v %v", tail, err)
	}
	pr, err := client.Issue(ctx, "ghp_fixture", "acme/api", 13)
	if err != nil || !pr.PullRequest || !pr.Merged || pr.URL != "https://github.com/acme/api/pull/13" {
		t.Fatal(pr, err)
	}
	if _, err = client.Issue(ctx, "ghp_fixture", "acme/api", 404); err == nil || !strings.Contains(err.Error(), "could not find this issue") {
		t.Fatalf("missing issue: %v", err)
	}
}

func TestRepositoryOfURL(t *testing.T) {
	for raw, want := range map[string]string{
		"https://github.com/acme/api/issues/812":                "acme/api#812",
		"https://github.com/acme/api/issues/812/":               "acme/api#812",
		"https://github.com/acme/api/issues/812#issuecomment-1": "acme/api#812",
		"https://github.com/acme/api/pull/77":                   "acme/api#77",
		"https://github.com/acme/api/pull/77/files":             "acme/api#77",
		"https://www.github.com/acme/my.repo/issues/1?x=y":      "acme/my.repo#1",
		"https://github.com/acme/api":                           "",
		"https://github.com/acme/api/issues":                    "",
		"https://github.com/acme/api/issues/0":                  "",
		"https://gitlab.com/acme/api/issues/1":                  "",
		"https://github.com.evil.example/acme/api/issues/1":     "",
		"javascript:alert(1)":                                   "",
		"acme/api#1":                                            "",
	} {
		full, number, ok := github.RepositoryOfURL(raw)
		got := ""
		if ok {
			got = fmt.Sprintf("%s#%d", full, number)
		}
		if got != want {
			t.Errorf("%s: got %q, want %q", raw, got, want)
		}
	}
}
