package github_test

import (
	"context"
	"fmt"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
)

// passing is n completed, successful check runs with ids from 1.
func passing(n int) []githubtest.Check {
	out := make([]githubtest.Check, n)
	for i := range out {
		out[i] = githubtest.Check{ID: int64(i + 1), Name: fmt.Sprintf("matrix-%d", i+1), Status: "completed", Conclusion: "success"}
	}
	return out
}

func pagedFixture(t *testing.T) (*githubtest.Server, *github.Client) {
	t.Helper()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["team/api"] = "main"
	fake.Update(func(s *githubtest.Server) {
		s.Pulls["team/api"] = []*githubtest.PR{{Number: 7, Title: "Big matrix", Head: "feature/matrix", Base: "main", HeadSHA: "abc", State: "open", MergeableState: "clean"}}
	})
	return fake, github.NewAt(fake.URL)
}

// A check that fails on a later page fails the pull request: in the PR
// loop and in tracking, for check runs and for commit statuses alike.
func TestChecksAreReadToTheLastPage(t *testing.T) {
	ctx := context.Background()
	fake, client := pagedFixture(t)
	fake.Update(func(s *githubtest.Server) {
		s.Checks["abc"] = passing(150)
		s.Checks["abc"][120].Conclusion = "failure"
	})
	status, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/matrix")
	if err != nil || status.ChecksState != "failure" || len(status.Checks) != 150 || status.Checks[0].Name != "matrix-121" {
		t.Fatalf("PR loop: %s, %d checks, %v", status.ChecksState, len(status.Checks), err)
	}
	read, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, nil)
	if err != nil || read.Status.ChecksState != "failure" || len(read.Runs) != 150 || len(strings.Fields(read.ETags.Runs)) != 2 {
		t.Fatalf("tracking: %s, %d runs, tags %q, %v", read.Status.ChecksState, len(read.Runs), read.ETags.Runs, err)
	}

	fake.Update(func(s *githubtest.Server) {
		s.Checks["abc"] = passing(10)
		statuses := make([]githubtest.Status, 130)
		for i := range statuses {
			statuses[i] = githubtest.Status{Context: fmt.Sprintf("ci/%d", i), State: "success"}
		}
		statuses[129].State = "error"
		s.Statuses["abc"] = statuses
	})
	status, err = client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/matrix")
	if err != nil || status.ChecksState != "failure" || len(status.Checks) != 140 {
		t.Fatalf("PR loop statuses: %s, %d checks, %v", status.ChecksState, len(status.Checks), err)
	}
	read, err = client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, nil)
	if err != nil || read.Status.ChecksState != "failure" || len(read.Statuses) != 130 {
		t.Fatalf("tracking statuses: %s, %d statuses, %v", read.Status.ChecksState, len(read.Statuses), err)
	}
}

// Every page of a list is read conditionally: unchanged pages cost nothing,
// and a change on a later page alone is still seen.
func TestPagedChecksAreConditionalPerPage(t *testing.T) {
	ctx := context.Background()
	fake, client := pagedFixture(t)
	fake.Update(func(s *githubtest.Server) { s.Checks["abc"] = passing(150) })
	first, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, nil)
	if err != nil || first.Status.ChecksState != "success" || len(first.Runs) != 150 {
		t.Fatalf("first: %s %d %v", first.Status.ChecksState, len(first.Runs), err)
	}
	again, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, &first)
	if err != nil || !again.NotModified || again.Status.ChecksState != "success" || len(again.Runs) != 150 {
		t.Fatalf("unchanged: %+v %v", again.Status, err)
	}
	// The pull request, both pages of runs and the statuses.
	if fake.NotModified != 4 {
		t.Fatalf("every page answered 304: %d", fake.NotModified)
	}
	fake.Update(func(s *githubtest.Server) { s.Checks["abc"][140].Conclusion = "timed_out" })
	changed, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, &again)
	if err != nil || changed.NotModified || changed.Status.ChecksState != "failure" || len(changed.Runs) != 150 {
		t.Fatalf("a change on page 2 only: %s %d %v", changed.Status.ChecksState, len(changed.Runs), err)
	}
}

// Past the read's bound, checks are incomplete: never success, so
// pr.checks-passed does not fire on checks nobody read. A failure among the
// ones read is still a failure.
func TestTooManyChecksAreIncompleteNeverSuccess(t *testing.T) {
	ctx := context.Background()
	fake, client := pagedFixture(t)
	fake.Update(func(s *githubtest.Server) { s.Checks["abc"] = passing(github.MaxListed + 1) })
	status, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/matrix")
	if err != nil || status.ChecksState != "incomplete" || len(status.Checks) != github.MaxListed {
		t.Fatalf("PR loop: %s, %d checks, %v", status.ChecksState, len(status.Checks), err)
	}
	read, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, nil)
	if err != nil || read.Status.ChecksState != "incomplete" || read.ETags.Runs != "" {
		t.Fatalf("tracking: %s, tags %q, %v", read.Status.ChecksState, read.ETags.Runs, err)
	}
	// No validators were kept: the next read is a full one, not a 304 that
	// would carry a partial read forward.
	again, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, &read)
	if err != nil || again.NotModified || again.Status.ChecksState != "incomplete" {
		t.Fatalf("again: %s %v", again.Status.ChecksState, err)
	}
	fake.Update(func(s *githubtest.Server) { s.Checks["abc"][3].Conclusion = "failure" })
	if status, err = client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/matrix"); err != nil || status.ChecksState != "failure" {
		t.Fatalf("a failure read: %s %v", status.ChecksState, err)
	}
}

// A reviewer's latest review decides, however many came before it.
func TestReviewDecisionReadsEveryReview(t *testing.T) {
	ctx := context.Background()
	fake, client := pagedFixture(t)
	fake.Update(func(s *githubtest.Server) {
		reviews := make([]string, 0, 151)
		for range 150 {
			reviews = append(reviews, "ana:APPROVED")
		}
		s.Pulls["team/api"][0].Reviews = append(reviews, "bo:CHANGES_REQUESTED")
	})
	status, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/matrix")
	if err != nil || status.ReviewDecision != "changes_requested" {
		t.Fatalf("PR loop: %q %v", status.ReviewDecision, err)
	}
	read, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, nil)
	if err != nil || read.Status.ReviewDecision != "changes_requested" {
		t.Fatalf("tracking: %q %v", read.Status.ReviewDecision, err)
	}
}

// Review comments are read from their newest end; past the read's bound
// the oldest are left out, and the read says so.
func TestReviewCommentsAreReadFromTheNewestEnd(t *testing.T) {
	ctx := context.Background()
	fake, client := pagedFixture(t)
	comments := func(n int) []githubtest.Comment {
		out := make([]githubtest.Comment, n)
		for i := range out {
			out[i] = githubtest.Comment{ID: int64(i + 1), Author: "ana", Body: fmt.Sprintf("comment %d", i+1), Path: "api/otter.go", Line: 1, Side: "RIGHT"}
		}
		return out
	}
	fake.Update(func(s *githubtest.Server) { s.Pulls["team/api"][0].Comments = comments(350) })
	feedback, err := client.ReviewComments(ctx, "ghp_fixture", "team/api", 7)
	if err != nil || feedback.Truncated() || len(feedback.Comments) != 350 || feedback.Comments[349].ID != 350 {
		t.Fatalf("350 comments: %d, truncated %v, %v", len(feedback.Comments), feedback.Truncated(), err)
	}
	fake.Update(func(s *githubtest.Server) {
		s.Pulls["team/api"][0].Comments = comments(github.MaxListed + 50)
		s.Pulls["team/api"][0].Reviews = []string{"ana:COMMENTED:first"}
	})
	feedback, err = client.ReviewComments(ctx, "ghp_fixture", "team/api", 7)
	if err != nil || !feedback.CommentsTruncated || feedback.ReviewsTruncated || len(feedback.Comments) != github.MaxListed || feedback.Comments[0].ID != 51 || feedback.Comments[len(feedback.Comments)-1].ID != int64(github.MaxListed+50) {
		t.Fatalf("too many comments: %d, %+v, %v", len(feedback.Comments), feedback, err)
	}
	if len(feedback.Reviews) != 1 || feedback.Reviews[0].Body != "first" {
		t.Fatalf("reviews: %+v", feedback.Reviews)
	}
	// Each list says for itself whether it was cut.
	fake.Update(func(s *githubtest.Server) {
		s.Pulls["team/api"][0].Comments = comments(2)
		reviews := make([]string, github.MaxListed+1)
		for i := range reviews {
			reviews[i] = fmt.Sprintf("ana:COMMENTED:review %d", i+1)
		}
		s.Pulls["team/api"][0].Reviews = reviews
	})
	feedback, err = client.ReviewComments(ctx, "ghp_fixture", "team/api", 7)
	if err != nil || feedback.CommentsTruncated || !feedback.ReviewsTruncated || len(feedback.Reviews) != github.MaxListed || feedback.Reviews[len(feedback.Reviews)-1].Body != fmt.Sprintf("review %d", github.MaxListed+1) {
		t.Fatalf("too many reviews: %d reviews, comments cut %v, reviews cut %v, %v", len(feedback.Reviews), feedback.CommentsTruncated, feedback.ReviewsTruncated, err)
	}
}

// A branch's open pull request is found behind any number of newer closed
// ones, and the lookup stays conditional across its pages.
func TestBranchLookupReadsEveryPullRequestOfTheBranch(t *testing.T) {
	ctx := context.Background()
	fake, client := pagedFixture(t)
	fake.Update(func(s *githubtest.Server) {
		pulls := []*githubtest.PR{{Number: 1, Title: "Still open", Head: "renovate/deps", Base: "main", HeadSHA: "open-head", State: "open", MergeableState: "clean"}}
		for n := 2; n <= 120; n++ {
			pulls = append(pulls, &githubtest.PR{Number: n, Head: "renovate/deps", Base: "release", HeadSHA: fmt.Sprintf("old-%d", n), State: "closed"})
		}
		s.Pulls["team/api"] = pulls
	})
	status, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "renovate/deps")
	if err != nil || !status.Found || status.Number != 1 || status.State != "open" {
		t.Fatalf("PR loop: #%d %s %v", status.Number, status.State, err)
	}
	found, tag, same, err := client.PullRequestsForBranch(ctx, "ghp_fixture", "team/api", "renovate/deps", "")
	if err != nil || same || len(found) != 120 || found[119].Number != 1 || found[119].State != "open" || len(strings.Fields(tag)) != 2 {
		t.Fatalf("lookup: %d found, tag %q, %v", len(found), tag, err)
	}
	if _, _, same, err = client.PullRequestsForBranch(ctx, "ghp_fixture", "team/api", "renovate/deps", tag); err != nil || !same {
		t.Fatalf("unchanged branch: %v %v", same, err)
	}
}
