package github_test

import (
	"context"
	"errors"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
)

// A tracked pull request is read with conditional requests: an unchanged
// pull request and unchanged checks cost nothing, and a change to any part is
// read again, reusing the parts that did not change.
func TestReadPullRequestIsConditional(t *testing.T) {
	ctx := context.Background()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["team/api"] = "main"
	fake.Update(func(s *githubtest.Server) {
		s.Pulls["team/api"] = []*githubtest.PR{{Number: 7, Title: "Add otter", Head: "feature/otter", Base: "main", HeadSHA: "abc", State: "open", MergeableState: "clean"}}
		s.Checks["abc"] = []githubtest.Check{{ID: 11, Name: "test", Status: "in_progress", URL: "https://github.com/team/api/actions/runs/1/job/11"}}
		s.Statuses["abc"] = []githubtest.Status{{Context: "ci/legacy", State: "success"}}
	})
	client := github.NewAt(fake.URL)
	first, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, nil)
	if err != nil || first.NotModified || first.Status.Head != "feature/otter" || first.Status.ChecksState != "pending" || len(first.Runs) != 1 || len(first.Statuses) != 1 || first.ETags.Pull == "" || first.ETags.Runs == "" || first.ETags.Head != "abc" {
		t.Fatalf("first read: %+v %v", first, err)
	}
	if first.Runs[0].ID != 11 || first.CreatedAt.IsZero() {
		t.Fatalf("check run id and times: %+v", first)
	}

	again, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, &first)
	if err != nil || !again.NotModified || again.Status.ChecksState != "pending" || again.Status.Title != "Add otter" {
		t.Fatalf("unchanged: %+v %v", again, err)
	}
	if fake.NotModified != 3 {
		t.Fatalf("the pull request, its runs and statuses are each 304: %d", fake.NotModified)
	}

	// Only a check finished: the pull request is reused, the checks read again.
	fake.Update(func(s *githubtest.Server) {
		s.Checks["abc"][0].Status, s.Checks["abc"][0].Conclusion = "completed", "failure"
	})
	changed, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, &again)
	if err != nil || changed.NotModified || changed.Status.ChecksState != "failure" || changed.Status.Title != "Add otter" || len(changed.Status.Checks) != 2 {
		t.Fatalf("checks changed: %+v %v", changed, err)
	}

	// A review changes the pull request (GitHub moves updated_at): read again.
	fake.Update(func(s *githubtest.Server) { s.Pulls["team/api"][0].Reviews = []string{"ana:CHANGES_REQUESTED"} })
	reviewed, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, &changed)
	if err != nil || reviewed.Status.ReviewDecision != "changes_requested" || reviewed.Status.ChecksState != "failure" {
		t.Fatalf("reviewed: %+v %v", reviewed, err)
	}

	// A push moves the head: its checks are new resources, read in full.
	fake.SetHead("team/api", "feature/otter", "def")
	fake.Update(func(s *githubtest.Server) {
		s.Checks["def"] = []githubtest.Check{{Name: "test", Status: "completed", Conclusion: "success"}}
	})
	pushed, err := client.ReadPullRequest(ctx, "ghp_fixture", "team/api", 7, &reviewed)
	if err != nil || pushed.Status.HeadSHA != "def" || pushed.Status.ChecksState != "success" || pushed.ETags.Head != "def" {
		t.Fatalf("pushed: %+v %v", pushed, err)
	}
}

func TestPullRequestsForBranchAndRateLimit(t *testing.T) {
	ctx := context.Background()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["team/api"] = "main"
	fake.Update(func(s *githubtest.Server) {
		s.Pulls["team/api"] = []*githubtest.PR{{Number: 3, Head: "feature/otter", HeadSHA: "abc", State: "closed", Merged: true}, {Number: 9, Head: "feature/otter", HeadSHA: "abc", State: "open"}, {Number: 4, Head: "other", State: "open"}}
	})
	client := github.NewAt(fake.URL)
	found, tag, same, err := client.PullRequestsForBranch(ctx, "ghp_fixture", "team/api", "feature/otter", "")
	if err != nil || same || tag == "" || len(found) != 2 || found[0].Number != 9 || found[0].State != "open" || found[1].State != "merged" {
		t.Fatalf("found %+v %q %v %v", found, tag, same, err)
	}
	if _, _, same, err = client.PullRequestsForBranch(ctx, "ghp_fixture", "team/api", "feature/otter", tag); err != nil || !same {
		t.Fatalf("unchanged branch: %v %v", same, err)
	}
	fake.Update(func(s *githubtest.Server) { s.RateLimited = true })
	_, _, _, err = client.PullRequestsForBranch(ctx, "ghp_fixture", "team/api", "feature/otter", "")
	var limit *github.RateLimitError
	if !errors.As(err, &limit) || limit.Reset.IsZero() {
		t.Fatalf("a spent rate limit is told apart, with its reset: %v", err)
	}
}

func TestJobLogsAndReviewComments(t *testing.T) {
	ctx := context.Background()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["team/api"] = "main"
	fake.Update(func(s *githubtest.Server) {
		s.Jobs[11] = "2026-11-17T10:00:00.0000000Z ##[group]Run go test ./...\n"
		s.Pulls["team/api"] = []*githubtest.PR{{Number: 7, Head: "feature/otter", HeadSHA: "abc", State: "open",
			Reviews:  []string{"ana:CHANGES_REQUESTED:Please handle the empty case."},
			Comments: []githubtest.Comment{{ID: 100, Author: "ana", Body: "Nil here?", Path: "api/otter.go", Line: 12, Side: "RIGHT", Hunk: "@@ -1,2 +10,3 @@\n a\n+b\n+c"}, {ID: 101, InReplyTo: 100, Author: "bo", Body: "Agreed", Path: "api/otter.go", Line: 12, Side: "RIGHT"}}}}
	})
	client := github.NewAt(fake.URL)
	log, err := client.JobLog(ctx, "ghp_fixture", "team/api", 11)
	if err != nil || string(log) != "2026-11-17T10:00:00.0000000Z ##[group]Run go test ./...\n" {
		t.Fatalf("job log through its storage redirect: %q %v", log, err)
	}
	if _, err = client.JobLog(ctx, "ghp_fixture", "team/api", 12); !errors.Is(err, github.ErrLogsGone) {
		t.Fatalf("missing logs: %v", err)
	}
	fake.Update(func(s *githubtest.Server) { s.LogsForbidden = true })
	if _, err = client.JobLog(ctx, "ghp_fixture", "team/api", 11); !errors.Is(err, github.ErrActionsLogs) {
		t.Fatalf("a token without Actions read access is explained: %v", err)
	}
	comments, reviews, err := client.ReviewComments(ctx, "ghp_fixture", "team/api", 7)
	if err != nil || len(comments) != 2 || comments[0].Line != 12 || comments[0].Side != "RIGHT" || comments[1].InReplyTo != 100 || comments[0].Outdated {
		t.Fatalf("comments %+v %v", comments, err)
	}
	if len(reviews) != 1 || reviews[0].State != "CHANGES_REQUESTED" || reviews[0].Body != "Please handle the empty case." {
		t.Fatalf("reviews %+v", reviews)
	}
}
