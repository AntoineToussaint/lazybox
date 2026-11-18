package runner

import (
	"context"
	"errors"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// onBranch commits on a branch of upstream (made from its current HEAD when
// new) and returns to the default branch, as development on a long-lived
// branch does.
func onBranch(t *testing.T, upstream, def, branch, message string) string {
	t.Helper()
	if exec.Command("git", "-C", upstream, "rev-parse", "--verify", "--quiet", "refs/heads/"+branch).Run() != nil {
		gitIn(t, upstream, "branch", branch)
	}
	gitIn(t, upstream, "switch", "--quiet", branch)
	commit := advance(t, upstream, message)
	gitIn(t, upstream, "switch", "--quiet", def)
	return commit
}

func hasBranch(list []*pb.RepositoryBranch, name string) bool {
	for _, b := range list {
		if b.Name == name {
			return true
		}
	}
	return false
}

// A base other than origin's default is fetched first and used: the
// workspace starts from origin/<branch> as it is now, never from the source
// copy's stale copy of it. The choice is remembered for the repository: the
// next workspace (no choice given, as `lazybox new` or an agent's request
// sends) starts there too, fetched again; choosing origin's default forgets it.
func TestChosenBaseIsFetchedAndRememberedPerRepository(t *testing.T) {
	upstream := upstreamOn(t, "main")
	onBranch(t, upstream, "main", "feat/v2-command-output", "v2 starts")
	source := sourceCopy(t, upstream)
	stale := gitIn(t, source, "rev-parse", "refs/heads/feat/v2-command-output")
	fresh := onBranch(t, upstream, "main", "feat/v2-command-output", "v2 moves on")
	if fresh == stale {
		t.Fatal("fixture: the branch did not advance")
	}
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	api := b.Collections.RepositoryIdentity("api")

	// Before any choice: origin's default, nothing remembered.
	branches, err := b.RepositoryBranches(context.Background(), api, true)
	if err != nil {
		t.Fatal(err)
	}
	if branches.DefaultBranch != "main" || branches.StartFrom != "" || !branches.Remote || len(branches.Branches) != 2 {
		t.Fatalf("branches before a choice: %+v", branches)
	}
	if branches.Branches[0].Name != "feat/v2-command-output" {
		t.Fatalf("the most recently committed branch is not first: %+v", branches.Branches)
	}

	var p progressLog
	ws, err := b.PrepareWorkspaceFrom(context.Background(), "chosen", "", "Chosen", "", []string{api}, map[string]string{api: "feat/v2-command-output"}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "api", fresh)
	if !strings.Contains(p.text(), "Updating api from origin (feat/v2-command-output)") || strings.Contains(p.text(), "Couldn't update") {
		t.Fatalf("progress: %s", p.text())
	}
	// origin/HEAD is left alone: only the chosen branch's remote-tracking ref moved.
	if head, err := exec.Command("git", "-C", source, "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD").Output(); err == nil {
		t.Fatalf("choosing a branch recorded origin/HEAD: %s", head)
	}

	// Remembered: the next workspace without a choice starts from the branch, fetched again.
	later := onBranch(t, upstream, "main", "feat/v2-command-output", "and again")
	ws, err = b.PrepareWorkspace(context.Background(), "remembered", "", "Remembered", "", []string{api}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "api", later)
	branches, err = b.RepositoryBranches(context.Background(), api, false)
	if err != nil {
		t.Fatal(err)
	}
	if branches.StartFrom != "feat/v2-command-output" || len(branches.Recent) != 1 || branches.Recent[0] != "feat/v2-command-output" || branches.Remote {
		t.Fatalf("remembered choice: %+v", branches)
	}
	// Adding the repository to an existing workspace (Add repository) starts there as well.
	gitIn(t, upstream, "commit", "--allow-empty", "-m", "main moves")
	ws, err = b.PrepareWorkspace(context.Background(), "add", ws.ID, "", "", []string{api}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "api-2", later)

	// Choosing origin's default branch forgets the choice; the recent list stays.
	main := gitIn(t, upstream, "rev-parse", "refs/heads/main")
	ws, err = b.PrepareWorkspaceFrom(context.Background(), "default", "", "Default", "", []string{api}, map[string]string{api: "main"}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "api", main)
	if start := b.Collections.StartFrom("api"); start.Branch != "" || len(start.Recent) != 1 {
		t.Fatalf("choosing the default did not forget the choice: %+v", start)
	}
	ws, err = b.PrepareWorkspace(context.Background(), "after-default", "", "After", "", []string{api}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "api", main)
}

// The remembered choice outlives the Runner: it is kept beside the
// workspace records, by repository identity.
func TestRememberedStartSurvivesRestart(t *testing.T) {
	upstream := upstreamOn(t, "main")
	source := sourceCopy(t, upstream)
	root := t.TempDir()
	open := func() *collections.Manager {
		m, err := collections.New(root, []collections.Repository{{ID: "api", Name: "api", Path: source}})
		if err != nil {
			t.Fatal(err)
		}
		return m
	}
	m := open()
	if err := m.RememberStartFrom("api", "feat/x", false); err != nil {
		t.Fatal(err)
	}
	if err := m.RememberStartFrom("api", "feat/y", false); err != nil {
		t.Fatal(err)
	}
	if err := m.Close(); err != nil {
		t.Fatal(err)
	}
	m = open()
	defer m.Close()
	start := m.StartFrom("api")
	if start.Branch != "feat/y" || strings.Join(start.Recent, ",") != "feat/y,feat/x" {
		t.Fatalf("after a restart: %+v", start)
	}
}

// Offline, a chosen branch starts from its last fetch and progress says so
// once, as for the default branch. A branch never fetched cannot start a
// workspace offline: the preparation says so instead of guessing.
func TestOfflineChosenBaseStartsFromLastFetch(t *testing.T) {
	upstream := upstreamOn(t, "main")
	source := sourceCopy(t, upstream)
	fetched := onBranch(t, upstream, "main", "feat/offline", "fetched once")
	onBranch(t, upstream, "main", "feat/never", "never fetched")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	api := b.Collections.RepositoryIdentity("api")
	var online progressLog
	if _, err := b.PrepareWorkspaceFrom(context.Background(), "online", "", "Online", "", []string{api}, map[string]string{api: "feat/offline"}, online.record); err != nil {
		t.Fatal(err)
	}

	onBranch(t, upstream, "main", "feat/offline", "not seen: offline")
	gitIn(t, source, "remote", "set-url", "origin", filepath.Join(t.TempDir(), "gone.git"))
	var offline progressLog
	ws, err := b.PrepareWorkspaceFrom(context.Background(), "offline", "", "Offline", "", []string{api}, map[string]string{api: "feat/offline"}, offline.record)
	if err != nil {
		t.Fatalf("offline creation failed: %v", err)
	}
	startsAt(t, ws, "api", fetched)
	if want := "Couldn't update api; started from feat/offline as of "; strings.Count(offline.text(), want) != 1 {
		t.Fatalf("want one %q notice, got:\n%s", want, offline.text())
	}
	// The picker still lists the branches as last fetched, and says why.
	branches, err := b.RepositoryBranches(context.Background(), api, true)
	if err != nil {
		t.Fatal(err)
	}
	if branches.Remote || branches.Error == "" || !hasBranch(branches.Branches, "feat/offline") {
		t.Fatalf("offline listing: %+v", branches)
	}

	_, err = b.PrepareWorkspaceFrom(context.Background(), "never", "", "Never", "", []string{api}, map[string]string{api: "feat/never"}, offline.record)
	if err == nil || !strings.Contains(err.Error(), "never fetched feat/never") {
		t.Fatalf("a never-fetched branch offline: %v", err)
	}
}

// A branch origin does not have fails a choice made now, with what to do;
// a remembered one that origin deleted since (merged) falls back to the
// default branch, says so, and is forgotten.
func TestMissingStartBranch(t *testing.T) {
	upstream := upstreamOn(t, "main")
	onBranch(t, upstream, "main", "feat/merged", "work")
	source := sourceCopy(t, upstream)
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	api := b.Collections.RepositoryIdentity("api")
	var p progressLog
	_, err := b.PrepareWorkspaceFrom(context.Background(), "typo", "", "Typo", "", []string{api}, map[string]string{api: "feat/typo"}, p.record)
	if !errors.Is(err, gitpublish.ErrNoBranch) || !strings.Contains(err.Error(), "api has no branch feat/typo on origin") {
		t.Fatalf("a branch origin lacks: %v", err)
	}
	if _, err = b.PrepareWorkspaceFrom(context.Background(), "merged", "", "Merged", "", []string{api}, map[string]string{api: "feat/merged"}, p.record); err != nil {
		t.Fatal(err)
	}
	gitIn(t, upstream, "branch", "-D", "feat/merged")
	main := gitIn(t, upstream, "rev-parse", "refs/heads/main")
	ws, err := b.PrepareWorkspace(context.Background(), "after-merge", "", "After merge", "", []string{api}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "api", main)
	if !strings.Contains(p.text(), "feat/merged is no longer on origin; api starts from its default branch") {
		t.Fatalf("progress: %s", p.text())
	}
	if start := b.Collections.StartFrom("api"); start.Branch != "" {
		t.Fatalf("a deleted branch is still remembered: %+v", start)
	}
}

// `--workspace new --start-from B` (an agent's conversation request): every
// repository of the new workspace starts from origin's B, fetched first,
// instead of the asking workspace's branches.
func TestChildWorkspaceStartsFromChosenBranch(t *testing.T) {
	upstream := upstreamOn(t, "main")
	onBranch(t, upstream, "main", "feat/base", "base")
	source := sourceCopy(t, upstream)
	fresh := onBranch(t, upstream, "main", "feat/base", "fresh")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	api := b.Collections.RepositoryIdentity("api")
	var p progressLog
	parent, err := b.PrepareWorkspace(context.Background(), "parent", "", "Parent", "", []string{api}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	id, err := b.childWorkspace(context.Background(), parent.ID, "Child", "child", "feat/base")
	if err != nil {
		t.Fatal(err)
	}
	child, err := b.Collections.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, child, "api", fresh)
}
