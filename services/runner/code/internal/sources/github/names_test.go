package github_test

import (
	"context"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
)

// "." and ".." are never sent as a repository: in an API path
// (repos/owner/../pulls) they would name another endpoint with the token.
func TestDotRepositoryNamesNeverReachGitHub(t *testing.T) {
	fake := githubtest.New(t, "ghp_fixture")
	client := github.NewAt(fake.URL)
	ctx := context.Background()
	for _, name := range []string{"acme/..", "acme/.", "../api", "./api"} {
		if _, _, _, err := client.PullRequestsForBranch(ctx, "ghp_fixture", name, "fix", ""); err == nil {
			t.Errorf("%s: branch lookup accepted", name)
		}
		if _, err := client.ReadPullRequest(ctx, "ghp_fixture", name, 1, nil); err == nil {
			t.Errorf("%s: read accepted", name)
		}
		if _, err := client.ReviewComments(ctx, "ghp_fixture", name, 1); err == nil {
			t.Errorf("%s: review comments accepted", name)
		}
	}
	if len(fake.Requests) != 0 {
		t.Fatalf("requests reached GitHub: %v", fake.Requests)
	}
}
