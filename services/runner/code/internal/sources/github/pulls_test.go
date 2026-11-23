package github_test

import (
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
)

func TestPullRequestCreateStatusAndMerge(t *testing.T) {
	ctx := context.Background()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["team/api"] = "main"
	fake.SetHead("team/api", "feature/otter", "abc123")
	client := github.NewAt(fake.URL)

	none, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/otter")
	if err != nil || none.Found || none.DefaultBranch != "main" {
		t.Fatalf("branch without a pull request: %+v %v", none, err)
	}
	created, err := client.CreatePullRequest(ctx, "ghp_fixture", "team/api", github.NewPullRequest{Head: "feature/otter", Title: " Add otter ", Body: "Why", Draft: true})
	if err != nil || !created.Found || created.Number != 1 || created.State != "open" || !created.Draft || created.Base != "main" || created.HeadSHA != "abc123" || created.ChecksState != "none" {
		t.Fatalf("created: %+v %v", created, err)
	}
	if pr := fake.Pull("team/api", 1); pr.Title != "Add otter" || pr.Body != "Why" || pr.Base != "main" {
		t.Fatalf("sent %+v", pr)
	}
	if _, err = client.CreatePullRequest(ctx, "ghp_fixture", "team/api", github.NewPullRequest{Head: "feature/otter", Title: "Again"}); err == nil || !strings.Contains(err.Error(), "A pull request already exists") {
		t.Fatalf("duplicate not explained: %v", err)
	}
	if _, err = client.CreatePullRequest(ctx, "ghp_fixture", "team/api", github.NewPullRequest{Head: "main", Title: "Self"}); err == nil || !strings.Contains(err.Error(), "base branch") {
		t.Fatalf("pull request from the base branch: %v", err)
	}

	fake.Update(func(s *githubtest.Server) {
		pr := s.Pulls["team/api"][0]
		pr.Draft, pr.MergeableState = false, "blocked"
		pr.Reviews = []string{"ana:CHANGES_REQUESTED", "bo:APPROVED", "ana:APPROVED"}
		s.Checks["abc123"] = append([]githubtest.Check{}, githubtest.Check{Name: "build", Status: "completed", Conclusion: "success", URL: "https://github.com/team/api/runs/1"}, githubtest.Check{Name: "test", Status: "completed", Conclusion: "failure", URL: "https://github.com/team/api/runs/2"}, githubtest.Check{Name: "lint", Status: "in_progress"})
		s.Statuses["abc123"] = []githubtest.Status{{Context: "ci/legacy", State: "error", URL: "https://ci.example/1"}}
	})
	status, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/otter")
	if err != nil || !status.Found || status.ReviewDecision != "approved" || status.ChecksState != "failure" || status.MergeableState != "blocked" || len(status.Checks) != 4 {
		t.Fatalf("status: %+v %v", status, err)
	}
	if !github.Failing(status.Checks[0]) || !github.Failing(status.Checks[1]) || status.Checks[2].Status != "in_progress" || status.Checks[3].Name != "build" {
		t.Fatalf("failing checks must lead: %+v", status.Checks)
	}
	if status.Checks[0].URL == "" || status.Checks[1].URL == "" {
		t.Fatal("failing checks lost their log links")
	}

	fake.Update(func(s *githubtest.Server) { s.MergeRefusal = "Required status check \"test\" is failing." })
	if err = client.MergePullRequest(ctx, "ghp_fixture", "team/api", 1, "squash", "abc123"); err == nil || !strings.Contains(err.Error(), "Required status check") {
		t.Fatalf("branch protection not explained: %v", err)
	}
	fake.Update(func(s *githubtest.Server) { s.MergeRefusal = "" })
	if err = client.MergePullRequest(ctx, "ghp_fixture", "team/api", 1, "squash", "stale"); err == nil || !strings.Contains(err.Error(), "Head branch was modified") {
		t.Fatalf("a moved head was merged: %v", err)
	}
	if err = client.MergePullRequest(ctx, "ghp_fixture", "team/api", 1, "octopus", "abc123"); err == nil {
		t.Fatal("unknown merge method accepted")
	}
	if err = client.MergePullRequest(ctx, "ghp_fixture", "team/api", 1, "squash", "abc123"); err != nil {
		t.Fatal(err)
	}
	if pr := fake.Pull("team/api", 1); !pr.Merged || pr.MergeMethod != "squash" {
		t.Fatalf("merge sent %+v", pr)
	}
	merged, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/otter")
	if err != nil || merged.State != "merged" || merged.MergeCommit == "" {
		t.Fatalf("merged: %+v %v", merged, err)
	}
}

func TestPullRequestWriteErrorsAreActionableAndRedacted(t *testing.T) {
	ctx := context.Background()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["team/api"] = "main"
	fake.SetHead("team/api", "feature/otter", "abc123")
	fake.ReadOnly = true
	client := github.NewAt(fake.URL)
	_, err := client.CreatePullRequest(ctx, "ghp_fixture", "team/api", github.NewPullRequest{Head: "feature/otter", Title: "Add"})
	if !errors.Is(err, github.ErrWriteForbidden) {
		t.Fatalf("read-only connection: %v", err)
	}
	_, err = client.PullRequestStatus(ctx, "ghp_wrong", "team/api", "feature/otter")
	if !errors.Is(err, github.ErrUnauthorized) || strings.Contains(err.Error(), "ghp_") {
		t.Fatalf("bad token: %v", err)
	}
	if _, err = client.PullRequestStatus(ctx, "ghp_fixture", "team/missing", "feature/otter"); err == nil || strings.Contains(err.Error(), fake.URL) {
		t.Fatalf("missing repository leaked the request: %v", err)
	}
}

func TestReviewRequestOutstandingIsReviewRequired(t *testing.T) {
	ctx := context.Background()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["team/api"] = "main"
	fake.SetHead("team/api", "feature/otter", "abc123")
	client := github.NewAt(fake.URL)
	if _, err := client.CreatePullRequest(ctx, "ghp_fixture", "team/api", github.NewPullRequest{Head: "feature/otter", Title: "Add"}); err != nil {
		t.Fatal(err)
	}
	fake.Update(func(s *githubtest.Server) { s.Pulls["team/api"][0].Requested = 1 })
	status, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/otter")
	if err != nil || status.ReviewDecision != "review_required" {
		t.Fatalf("%+v %v", status, err)
	}
}

// A repository that requires up-to-date branches refuses to merge one behind
// its base, though GitHub calls it mergeable (no conflicts). Updating the
// branch, pinned to the head the person saw, is what makes it mergeable.
func TestPullRequestBehindItsBaseIsUpdatedThenMerged(t *testing.T) {
	ctx := context.Background()
	fake := githubtest.New(t, "ghp_fixture")
	fake.Repos["team/api"] = "main"
	fake.SetHead("team/api", "feature/otter", "abc123")
	client := github.NewAt(fake.URL)
	if _, err := client.CreatePullRequest(ctx, "ghp_fixture", "team/api", github.NewPullRequest{Head: "feature/otter", Title: "Add otter"}); err != nil {
		t.Fatal(err)
	}
	fake.Update(func(s *githubtest.Server) { s.Pulls["team/api"][0].MergeableState = "behind" })
	behind, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/otter")
	if err != nil || behind.MergeableState != "behind" || !behind.Mergeable {
		t.Fatalf("behind: %+v %v", behind, err)
	}
	if err = client.MergePullRequest(ctx, "ghp_fixture", "team/api", 1, "squash", "abc123"); err == nil || !strings.Contains(err.Error(), "out of date") {
		t.Fatalf("a branch behind its base was merged, or the refusal was lost: %v", err)
	}
	if err = client.UpdatePullRequestBranch(ctx, "ghp_fixture", "team/api", 1, "stale"); err == nil {
		t.Fatal("updated a branch whose head moved since the person saw it")
	}
	if err = client.UpdatePullRequestBranch(ctx, "ghp_fixture", "team/api", 1, "abc123"); err != nil {
		t.Fatalf("GitHub's 202 (updating in the background) is a success: %v", err)
	}
	updated, err := client.PullRequestStatus(ctx, "ghp_fixture", "team/api", "feature/otter")
	if err != nil || updated.MergeableState != "clean" || updated.HeadSHA == "abc123" || fake.Pull("team/api", 1).BranchUpdates != 1 {
		t.Fatalf("updated: %+v %v", updated, err)
	}
	if err = client.MergePullRequest(ctx, "ghp_fixture", "team/api", 1, "squash", updated.HeadSHA); err != nil {
		t.Fatalf("merge after the update: %v", err)
	}
}
