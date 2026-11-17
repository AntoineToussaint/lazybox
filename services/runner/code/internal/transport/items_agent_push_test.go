package transport

import (
	"context"
	"encoding/json"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// The real failure (here acme/platform#1121): a workspace repository is a
// worktree of a bare source with no fetch refspec. The agent ran
// `git push -u origin <branch> && gh pr create`; git recorded the upstream
// but no refs/remotes/origin/<branch>, so the branch was never asked about,
// and the pull request was opened before tracking existed, so no detector
// saw its URL either.

const realBranch = "feature/quietly-brave-otter"

// agentPushed makes the fixture's source look like a workspace's bare source
// (origin names a private org repository on GitHub, no fetch refspec), and
// has the agent commit on realBranch and push it with -u to a local bare
// repository standing in for GitHub.
func (f *itemsFixture) agentPushed(t *testing.T, repository string) {
	t.Helper()
	source := f.bench.Collections.Repositories()[0].Path
	remote := filepath.Join(t.TempDir(), "remote.git")
	runGit(t, source, "init", "--bare", "-q", "-b", "main", remote)
	runGit(t, source, "remote", "set-url", "origin", "https://github.com/"+repository+".git")
	runGit(t, source, "config", "--unset-all", "remote.origin.fetch")
	runGit(t, source, "config", "remote.origin.pushurl", remote)
	runGit(t, f.worktree, "checkout", "-q", "-b", realBranch)
	runGit(t, f.worktree, "-c", "user.name=Agent", "-c", "user.email=agent@example.invalid", "commit", "-q", "--allow-empty", "-m", "Retry failed uploads")
	runGit(t, f.worktree, "push", "-q", "-u", "origin", realBranch)
	if err := exec.Command("git", "-C", f.worktree, "rev-parse", "--verify", "--quiet", "refs/remotes/origin/"+realBranch).Run(); err == nil {
		t.Fatal("the push recorded a remote-tracking ref: the fixture is not the shape of a workspace's bare source")
	}
}

// openPullRequest opens a pull request from realBranch, 18 hours ago: before
// tracking first saw the branch. Its head is the worktree's commit, so it is
// the workspace's own work.
func (f *itemsFixture) openPullRequest(repository string, number int) {
	f.openPullRequestAt(repository, number, f.head())
}

func (f *itemsFixture) head() string {
	out, _ := exec.Command("git", "-C", f.worktree, "rev-parse", "HEAD").Output()
	return strings.TrimSpace(string(out))
}

func (f *itemsFixture) openPullRequestAt(repository string, number int, head string) {
	f.github.Update(func(s *githubtest.Server) {
		s.Repos[repository] = "main"
		s.Pulls[repository] = append(s.Pulls[repository], &githubtest.PR{Number: number, Title: "Retry failed uploads from the worker", Head: realBranch, Base: "main", HeadSHA: head,
			State: "open", MergeableState: "blocked", Requested: 1, CreatedAt: time.Now().Add(-18 * time.Hour)})
		s.Checks[head] = []githubtest.Check{{ID: 1, Name: "build", Status: "completed", Conclusion: "success"}, {ID: 2, Name: "test", Status: "completed", Conclusion: "success"}}
	})
}

// An open pull request opened before the workspace was on the branch, whose
// head is not on the workspace's branch, is someone else's work on a branch
// of the same name: it is not adopted (audit: stale-branch adoption).
func TestBranchDoesNotAdoptAnOlderStrangersPullRequest(t *testing.T) {
	f := newItemsFixture(t)
	b, ctx := f.bench, context.Background()
	f.agentPushed(t, "acme/platform")
	f.openPullRequestAt("acme/platform", 1121, strings.Repeat("ab", 20))
	b.TrackItems(ctx)
	if pr := f.links(t)["acme/platform#1121"]; pr != nil {
		t.Fatalf("an older pull request whose head is not on the branch was adopted: %+v", pr)
	}
	// A pull request opened from it since is the workspace's, wherever its head is.
	f.github.Update(func(s *githubtest.Server) {
		s.Pulls["acme/platform"] = append(s.Pulls["acme/platform"], &githubtest.PR{Number: 1122, Title: "New", Head: realBranch, Base: "main", HeadSHA: strings.Repeat("cd", 20), State: "open", CreatedAt: time.Now()})
	})
	b.WorkItems().Now = func() time.Time { return time.Now().Add(-time.Hour) }
	b.TrackItems(ctx)
	if pr := f.links(t)["acme/platform#1122"]; pr == nil || pr.Role != "delivers" {
		t.Fatalf("a pull request opened since: %v", f.links(t))
	}
}

func TestBranchAnAgentPushedItselfFindsItsPullRequest(t *testing.T) {
	f := newItemsFixture(t)
	b, ctx := f.bench, context.Background()
	f.agentPushed(t, "acme/platform")
	f.openPullRequest("acme/platform", 1121)

	b.TrackItems(ctx)
	pr := f.links(t)["acme/platform#1121"]
	if pr == nil {
		t.Fatalf("the pull request opened from the branch the agent pushed is not tracked: %v", f.links(t))
	}
	if pr.Role != "delivers" || pr.Source != "branch" || pr.Member != "api" || pr.Item.PullRequest.Checks != "success" || pr.Item.PullRequest.Review != "review_required" || pr.Item.State.Category != "review" {
		t.Fatalf("tracked as %+v", pr)
	}
	listed, err := f.service.ListLinks(ctx, &pb.ListLinksRequest{CollectionId: f.mine.Id})
	if err != nil || listed.TrackingError != "" {
		t.Fatalf("tracking error: %+v %v", listed, err)
	}
}

// A branch no local state says was pushed (a push without -u) is still
// asked about, at the unfocused interval.
func TestBranchNotKnownToBePushedIsStillAskedAbout(t *testing.T) {
	f := newItemsFixture(t)
	b, ctx := f.bench, context.Background()
	f.agentPushed(t, "acme/platform")
	runGit(t, f.worktree, "branch", "--unset-upstream")
	f.openPullRequest("acme/platform", 1121)
	b.TrackItems(ctx)
	if f.links(t)["acme/platform#1121"] == nil {
		t.Fatal("a branch whose push left no trace locally is never asked about")
	}
}

// A lookup GitHub refuses is said in ListLinks for that workspace, and in
// no other, until a lookup answers.
func TestFailedBranchLookupIsSaid(t *testing.T) {
	f := newItemsFixture(t)
	b, ctx := f.bench, context.Background()
	// The connection cannot see the private repository: GitHub answers 404.
	f.agentPushed(t, "acme/platform")
	b.TrackItems(ctx)
	mine, err := f.service.ListLinks(ctx, &pb.ListLinksRequest{CollectionId: f.mine.Id})
	if err != nil || !strings.Contains(mine.TrackingError, "acme/platform") || !strings.Contains(mine.TrackingError, "404") {
		t.Fatalf("the failed lookup is not said: %q %v", mine.GetTrackingError(), err)
	}
	if !strings.Contains(mine.TrackingError, realBranch) {
		t.Fatalf("names the branch: %q", mine.TrackingError)
	}
	if all, _ := f.service.ListLinks(ctx, &pb.ListLinksRequest{All: true}); !strings.Contains(all.TrackingError, "acme/platform") {
		t.Fatalf("every workspace's list: %q", all.TrackingError)
	}
	// The other workspace (on the same repository) says its own branch.
	if theirs, _ := f.service.ListLinks(ctx, &pb.ListLinksRequest{CollectionId: f.theirs.Id}); strings.Contains(theirs.TrackingError, realBranch) {
		t.Fatalf("another workspace says this one's: %q", theirs.TrackingError)
	}
	// Access granted: the next lookup answers, and the note goes.
	f.openPullRequest("acme/platform", 1121)
	b.WorkItems().Now = func() time.Time { return time.Now().Add(-time.Hour) }
	b.TrackItems(ctx)
	mine, _ = f.service.ListLinks(ctx, &pb.ListLinksRequest{CollectionId: f.mine.Id})
	if mine.TrackingError != "" || len(mine.Links) != 1 {
		t.Fatalf("after access: %+v", mine)
	}
}

// What an agent's gh pr create printed before tracking ran is found once,
// on start, from the conversation's journal.
func TestStartBackfillsPullRequestsAgentsOpened(t *testing.T) {
	f := newItemsFixture(t)
	b, ctx := f.bench, context.Background()
	f.openPullRequest("team/api", 1121)
	// Recorded without the detector: as before tracking existed.
	record := func(data string) {
		if err := b.Record(f.session, agent.Event{Type: "tool_activity", Data: json.RawMessage(data)}); err != nil {
			t.Fatal(err)
		}
	}
	command, _ := json.Marshal(map[string]any{"command": "cat > /tmp/pr-body.md <<'EOF'\n## Summary\nSee https://github.com/team/api/pull/1000\nEOF\ngh pr create --base main --head " + realBranch + " --title x --body-file /tmp/pr-body.md"})
	record(`{"type":"tool_call","id":"toolu_1","name":"Bash","status":"running","input":` + string(command) + `}`)
	record(`{"type":"tool_call","id":"toolu_2","name":"Bash","status":"completed","input":{"command":"gh pr view 1000"},"output":"https://github.com/team/api/pull/1000"}`)
	output, _ := json.Marshal("remote:      https://github.com/team/api/pull/new/" + realBranch + "        \nremote: \nhttps://github.com/team/api/pull/1121")
	record(`{"type":"tool_call","id":"toolu_1","status":"completed","output":` + string(output) + `}`)
	if len(f.links(t)) != 0 {
		t.Fatal("recorded without a detector")
	}

	b.BackfillItems(ctx)
	links := f.links(t)
	pr := links["team/api#1121"]
	if pr == nil || pr.Role != "delivers" || pr.Source != "agent-output" || pr.SessionId != f.session.ID {
		t.Fatalf("backfill: %v", links)
	}
	if len(links) != 1 {
		t.Fatalf("only what a PR-creating command printed is backfilled: %v", links)
	}
	// Once: a person who unlinks it does not see it come back on the next start.
	if _, err := f.service.UnlinkItem(ctx, &pb.UnlinkItemRequest{CollectionId: f.mine.Id, Ref: pr.Item.Ref}); err != nil {
		t.Fatal(err)
	}
	b.BackfillItems(ctx)
	if len(f.links(t)) != 0 {
		t.Fatal("the backfill read the same journal again")
	}
	// What came after is read on the next start.
	record(`{"type":"tool_call","id":"toolu_3","name":"Bash","status":"completed","input":{"command":"lazybox run -- gh pr create --fill"},"output":"[lazybox] gh pr create --fill → exit 0 (2.1s)\nCreating pull request for agent/two into main in team/api\n\nhttps://github.com/team/api/pull/1122\n"}`)
	b.BackfillItems(ctx)
	if l := f.links(t)["team/api#1122"]; l == nil || l.Role != "delivers" {
		t.Fatalf("after the last read: %v", f.links(t))
	}
}
