package collections

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

func gitTest(t *testing.T, path string, args ...string) string {
	t.Helper()
	c := exec.Command("git", append([]string{"-C", path}, args...)...)
	c.Env = append(os.Environ(), "GIT_CONFIG_GLOBAL=/dev/null", "GIT_CONFIG_NOSYSTEM=1")
	b, err := c.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v: %s", args, err, b)
	}
	return strings.TrimSpace(string(b))
}
func repoTest(t *testing.T) Repository {
	t.Helper()
	path := t.TempDir()
	gitTest(t, path, "init", "-b", "main")
	if err := os.WriteFile(filepath.Join(path, "source.txt"), []byte("original"), 0600); err != nil {
		t.Fatal(err)
	}
	gitTest(t, path, "add", ".")
	gitTest(t, path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "initial")
	return Repository{ID: "repo", Path: path}
}
func newTest(t *testing.T, root string, repos ...Repository) *Manager {
	t.Helper()
	m, err := New(root, repos)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = m.Close() })
	return m
}
func specTest(repo string) Spec {
	return Spec{Name: "Feature", Members: []MemberSpec{{Name: "api", RepositoryID: repo}}}
}

func TestRenameKeepsGeneratedSlugAndWorktreesAcrossRestart(t *testing.T) {
	repo := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, repo)
	c, err := m.Create(context.Background(), "named", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	if c.Slug == "" || c.Slug == c.ID || len(strings.Split(c.Slug, "-")) != 3 || filepath.Base(c.Root) != c.Slug {
		t.Fatalf("not a readable slug: %s", c.Slug)
	}
	renamed, err := m.Rename(c.ID, "A new label / with punctuation")
	if err != nil {
		t.Fatal(err)
	}
	if renamed.Root != c.Root || renamed.Members[0].Path != c.Members[0].Path || renamed.Members[0].Branch != c.Members[0].Branch {
		t.Fatal("rename moved work")
	}
	second, err := m.Create(context.Background(), "second", specTest(repo.ID))
	if err != nil || second.Slug == c.Slug {
		t.Fatal("same display names must have independent folders", err)
	}
	if _, err = m.Rename(c.ID, "\n"); err == nil {
		t.Fatal("invalid name accepted")
	}
	m.Close()
	m = newTest(t, root, repo)
	loaded, err := m.Get(c.ID)
	if err != nil || loaded.Name != renamed.Name || loaded.Root != c.Root {
		t.Fatal("rename not durable", err)
	}
	retried, err := m.Create(context.Background(), "named", specTest(repo.ID))
	if err != nil || retried.Root != c.Root {
		t.Fatal("retry changed renamed workspace", err)
	}
}

func TestMultiRepositoryAndSameRepositoryWorktreesAreIsolated(t *testing.T) {
	a, b := repoTest(t), repoTest(t)
	a.ID, b.ID = "api", "web"
	if err := os.WriteFile(filepath.Join(a.Path, "source.txt"), []byte("dirty original"), 0600); err != nil {
		t.Fatal(err)
	}
	m := newTest(t, t.TempDir(), a, b)
	spec := Spec{Name: "Release", Members: []MemberSpec{{Name: "api", RepositoryID: "api"}, {Name: "api-next", RepositoryID: "api"}, {Name: "web", RepositoryID: "web"}}}
	c, err := m.Create(context.Background(), "request", spec)
	if err != nil {
		t.Fatal(err)
	}
	if c.State != "ready" || c.WorkID != c.ID || len(c.Members) != 3 || len(m.Repositories()) != 2 {
		t.Fatalf("bad collection: %+v", c)
	}
	for _, member := range c.Members {
		if !member.Ready || gitTest(t, member.Path, "branch", "--show-current") != member.Branch {
			t.Fatal("worktree not on its own branch")
		}
		if content, err := os.ReadFile(filepath.Join(member.Path, "source.txt")); err != nil || string(content) != "original" {
			t.Fatal("worktree inherited uncommitted checkout contents")
		}
	}
	second, err := m.Create(context.Background(), "second", specTest("api"))
	if err != nil {
		t.Fatal(err)
	}
	if c.Root == second.Root || c.Members[0].Branch == second.Members[0].Branch {
		t.Fatal("collections share checkout or branch")
	}
	if content, _ := os.ReadFile(filepath.Join(a.Path, "source.txt")); string(content) != "dirty original" || gitTest(t, a.Path, "branch", "--show-current") != "main" {
		t.Fatal("source checkout was modified")
	}
	// Returned snapshots cannot mutate the owner.
	c.Members[0].Path = "changed"
	snapshot, err := m.Get(c.ID)
	if err != nil || snapshot.Members[0].Path == "changed" {
		t.Fatal("mutable snapshot")
	}
}

func TestIdempotencyRestartAndWorkRecordUniqueness(t *testing.T) {
	r := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, r)
	spec := specTest(r.ID)
	spec.WorkID = "existing-tracker-record"
	a, err := m.Create(context.Background(), "key", spec)
	if err != nil {
		t.Fatal(err)
	}
	b, err := m.Create(context.Background(), "key", spec)
	if err != nil || a.ID != b.ID {
		t.Fatal("retry duplicated collection", err)
	}
	changed := spec
	changed.Name = "different"
	if _, err := m.Create(context.Background(), "key", changed); err == nil {
		t.Fatal("changed idempotent request accepted")
	}
	if _, err := m.Create(context.Background(), "another", spec); err == nil {
		t.Fatal("second workspace for work record accepted")
	}
	if err := m.Close(); err != nil {
		t.Fatal(err)
	}
	m = newTest(t, root, r)
	b, err = m.Create(context.Background(), "key", spec)
	if err != nil || a.ID != b.ID || a.Members[0].BaseCommit != b.Members[0].BaseCommit {
		t.Fatal("restart changed collection", err)
	}
	if len(m.List()) != 1 {
		t.Fatal("duplicate collection record")
	}
}

func TestPreflightRejectsUnsafeAndConflictingRequestsWithoutWorktrees(t *testing.T) {
	r := repoTest(t)
	m := newTest(t, t.TempDir(), r)
	cases := []Spec{
		{Name: "bad", Members: []MemberSpec{{Name: "../escape", RepositoryID: r.ID}}},
		{Name: "bad", Members: []MemberSpec{{Name: "api", RepositoryID: "unknown"}}},
		{Name: "bad", Members: []MemberSpec{{Name: "api", RepositoryID: r.ID, BaseRef: "no-such-ref"}}},
		{Name: "bad", Members: []MemberSpec{{Name: "api", RepositoryID: r.ID, Branch: "main"}}},
		{Name: "bad", Members: []MemberSpec{{Name: "api", RepositoryID: r.ID, Branch: "bad..branch"}}},
		{Name: "bad", Members: []MemberSpec{{Name: "api", RepositoryID: r.ID}, {Name: "API", RepositoryID: r.ID}}},
		{Name: "bad", Members: []MemberSpec{{Name: "api", RepositoryID: r.ID, Branch: "same"}, {Name: "other", RepositoryID: r.ID, Branch: "same"}}},
	}
	for _, spec := range cases {
		if _, err := m.Create(context.Background(), "key", spec); err == nil {
			t.Fatalf("accepted %+v", spec)
		}
	}
	if len(m.List()) != 0 || strings.Count(gitTest(t, r.Path, "worktree", "list", "--porcelain"), "worktree ") != 1 {
		t.Fatal("preflight created work")
	}
	if _, err := m.Create(context.Background(), "", specTest(r.ID)); err == nil {
		t.Fatal("missing request key accepted")
	}
	if _, err := m.Get("../unknown"); !os.IsNotExist(err) {
		t.Fatal("unknown record did not return not found")
	}
}

func TestPartialProvisionIsPreservedAndResumedAfterRestart(t *testing.T) {
	r := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, r)
	// Exercise the real on-disk crash boundary: durable plan, first worktree
	// created by Git, no completion marker yet, second worktree still absent.
	id := "RECOVERY"
	commit := gitTest(t, r.Path, "rev-parse", "HEAD")
	c := Collection{Version: 1, ID: id, Root: filepath.Join(m.root, id), Name: "Recovery", WorkID: id, State: "creating", RequestKey: "key", Fingerprint: "fixture"}
	spec := Spec{Name: "Recovery", Members: []MemberSpec{{Name: "first", RepositoryID: r.ID, BaseRef: "HEAD"}, {Name: "second", RepositoryID: r.ID, BaseRef: "HEAD"}}}
	encoded, _ := json.Marshal(spec)
	sum := sha256.Sum256(encoded)
	c.Fingerprint = hex.EncodeToString(sum[:])
	for _, name := range []string{"first", "second"} {
		c.Members = append(c.Members, Member{MemberSpec: MemberSpec{Name: name, RepositoryID: r.ID, Branch: "lazybox/recovery/" + name, BaseRef: "HEAD"}, Path: filepath.Join(c.Root, name), BaseCommit: commit, CommonDir: m.common[r.ID]})
	}
	if err := m.save(c); err != nil {
		t.Fatal(err)
	}
	gitTest(t, r.Path, "worktree", "add", "-b", c.Members[0].Branch, c.Members[0].Path, commit)
	if err := os.WriteFile(filepath.Join(c.Members[0].Path, "untracked"), []byte("keep"), 0600); err != nil {
		t.Fatal(err)
	}
	// A failed attempt must retain the first checkout, not recursively roll it back.
	if err := os.MkdirAll(c.Members[1].Path, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(c.Members[1].Path, "blocker"), []byte("keep too"), 0600); err != nil {
		t.Fatal(err)
	}
	failed, err := m.provision(context.Background(), c)
	if err == nil || failed.State != "failed" || !failed.Members[0].Ready {
		t.Fatal("partial failure not retained")
	}
	if _, err := os.Stat(filepath.Join(c.Members[0].Path, "untracked")); err != nil {
		t.Fatal("existing work deleted")
	}
	// Remove only the fixture's blocker, as an operator would after inspection.
	if err := os.Remove(filepath.Join(c.Members[1].Path, "blocker")); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(c.Members[1].Path); err != nil {
		t.Fatal(err)
	}
	m.Close()
	m = newTest(t, root, r)
	ready, err := m.Create(context.Background(), "key", spec)
	if err != nil || ready.State != "ready" {
		t.Fatal("resume failed", err)
	}
	if _, err := os.Stat(filepath.Join(ready.Members[0].Path, "untracked")); err != nil {
		t.Fatal("resume lost untracked work")
	}
	if strings.Count(gitTest(t, r.Path, "worktree", "list", "--porcelain"), "worktree ") != 3 {
		t.Fatal("recovery duplicated worktree")
	}
}

func TestEmptyWorkspaceAddsRepositoriesWithoutMovingFilesAndRetainsMultipleOwners(t *testing.T) {
	r := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, r)
	ctx := context.Background()
	c, err := m.Create(ctx, "empty", Spec{Name: "Research"})
	if err != nil {
		t.Fatal(err)
	}
	notes := filepath.Join(c.Root, "notes.txt")
	if err = os.WriteFile(notes, []byte("keep"), 0600); err != nil {
		t.Fatal(err)
	}
	for _, id := range []string{"one", "two"} {
		if _, err = m.Acquire(ctx, c.ID, id, func(string) bool { return true }); err != nil {
			t.Fatal(err)
		}
	}
	spec := []MemberSpec{{RepositoryID: r.ID, Name: "api", BaseRef: "HEAD"}}
	added, err := m.AddRepositories(ctx, c.ID, "add-api", spec)
	if err != nil || added.Root != c.Root || len(added.Members) != 1 || len(added.OwnerExecutionIDs) != 2 {
		t.Fatal("add changed workspace", err)
	}
	if _, err = m.AddRepositories(ctx, c.ID, "add-api", spec); err != nil {
		t.Fatal("retry", err)
	}
	if _, err = m.AddRepositories(ctx, c.ID, "add-api", []MemberSpec{{RepositoryID: r.ID, Name: "other"}}); err == nil {
		t.Fatal("changed retry accepted")
	}
	if data, err := os.ReadFile(notes); err != nil || string(data) != "keep" {
		t.Fatal("scratch file changed", err)
	}
	copy, _ := m.Get(c.ID)
	copy.OwnerExecutionIDs[0] = "mutated"
	copy.AddRequests["add-api"] = "mutated"
	retained, _ := m.Get(c.ID)
	if retained.OwnerExecutionIDs[0] != "one" || retained.AddRequests["add-api"] == "mutated" {
		t.Fatal("mutable state escaped")
	}
	if err = m.Close(); err != nil {
		t.Fatal(err)
	}
	m = newTest(t, root, r)
	retained, _ = m.Get(c.ID)
	if !retained.OwnershipUncertain || len(retained.OwnerExecutionIDs) != 2 {
		t.Fatal("lost persisted owners")
	}
	if _, err = m.Reconcile(c.ID, "one", "stopped fixture", true); err != nil {
		t.Fatal(err)
	}
	if _, err = m.Acquire(ctx, c.ID, "three", func(string) bool { return false }); err == nil {
		t.Fatal("unreconciled sibling allowed")
	}
	if _, err = m.Reconcile(c.ID, "two", "stopped fixture", true); err != nil {
		t.Fatal(err)
	}
	if _, err = m.Acquire(ctx, c.ID, "three", func(string) bool { return false }); err != nil {
		t.Fatal(err)
	}
}

func TestLegacyRootSurvivesMultiSessionOwnershipMigration(t *testing.T) {
	r := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, r)
	ctx := context.Background()
	c, err := m.Create(ctx, "legacy", specTest(r.ID))
	if err != nil {
		t.Fatal(err)
	}
	legacyRoot := filepath.Join(m.root, c.ID)
	if err = os.MkdirAll(legacyRoot, 0700); err != nil {
		t.Fatal(err)
	}
	for i := range c.Members {
		next := filepath.Join(legacyRoot, c.Members[i].Name)
		gitTest(t, r.Path, "worktree", "move", c.Members[i].Path, next)
		c.Members[i].Path = next
	}
	c.Root = legacyRoot
	c.Version = 1
	if err = m.save(c); err != nil {
		t.Fatal(err)
	}
	m.Close()
	m = newTest(t, root, r)
	if _, err = m.Acquire(ctx, c.ID, "new-session", func(string) bool { return false }); err != nil {
		t.Fatal(err)
	}
	if err = m.Release(c.ID, "new-session"); err != nil {
		t.Fatal(err)
	}
	m.Close()
	m = newTest(t, root, r)
	loaded, err := m.Get(c.ID)
	if err != nil || loaded.Root != legacyRoot || !loaded.LegacyRoot {
		t.Fatal("legacy root lost", err)
	}
}

func TestOwnershipArchiveAndTamperDetection(t *testing.T) {
	r := repoTest(t)
	m := newTest(t, t.TempDir(), r)
	ctx := context.Background()
	c, err := m.Create(ctx, "key", specTest(r.ID))
	if err != nil {
		t.Fatal(err)
	}
	active := func(string) bool { return true }
	inactive := func(string) bool { return false }
	if _, err := m.Acquire(ctx, c.ID, "a", active); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Acquire(ctx, c.ID, "b", active); err != nil {
		t.Fatal("second workspace session rejected", err)
	}
	m.Release(c.ID, "wrong-owner")
	if _, err := m.Archive(c.ID, active); err == nil {
		t.Fatal("archived active collection")
	}
	m.Release(c.ID, "a")
	if _, err := m.Archive(c.ID, active); err == nil {
		t.Fatal("closing one session released the other")
	}
	if _, err := m.Acquire(ctx, c.ID, "b", inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Archive(c.ID, inactive); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Acquire(ctx, c.ID, "c", inactive); err == nil {
		t.Fatal("attached archived collection")
	}
	if _, err := os.Stat(filepath.Join(c.Members[0].Path, "source.txt")); err != nil {
		t.Fatal("archive deleted files")
	}
	other, err := m.Create(ctx, "other", Spec{Name: "Other", Members: specTest(r.ID).Members})
	if err != nil {
		t.Fatal(err)
	}
	path := other.Members[0].Path
	if err := os.Rename(path, path+"-moved"); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(path+"-moved", path); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Acquire(ctx, other.ID, "x", inactive); err == nil {
		t.Fatal("accepted replaced checkout")
	}
}

func TestRootLockConfigurationAndCorruptRecord(t *testing.T) {
	r := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, r)
	if _, err := New(root, []Repository{r}); err == nil {
		t.Fatal("two Runners own root")
	}
	if _, err := New("relative", nil); err == nil {
		t.Fatal("relative root accepted")
	}
	if _, err := New(t.TempDir(), []Repository{{ID: "bad", Path: "relative"}}); err == nil {
		t.Fatal("relative repo accepted")
	}
	if _, err := New(t.TempDir(), []Repository{r, r}); err == nil {
		t.Fatal("duplicate repo accepted")
	}
	m.Close()
	if err := m.Release("unused", "unused"); err == nil {
		t.Fatal("closed manager can still write after releasing root lock")
	}
	if err := os.WriteFile(filepath.Join(root, ".lazybox-collections", "records", "BROKEN.json"), []byte("{"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := New(root, []Repository{r}); err == nil {
		t.Fatal("corrupt record silently discarded")
	}
}

func TestConcurrentIdempotentRequestsProvisionOnce(t *testing.T) {
	r := repoTest(t)
	m := newTest(t, t.TempDir(), r)
	var wg sync.WaitGroup
	ids := make(chan string, 4)
	for range 4 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			c, err := m.Create(context.Background(), "key", specTest(r.ID))
			if err != nil {
				t.Error(err)
				return
			}
			ids <- c.ID
		}()
	}
	wg.Wait()
	close(ids)
	id := ""
	for next := range ids {
		if id != "" && id != next {
			t.Fatal("multiple checkouts for a request")
		}
		id = next
	}
	if len(m.List()) != 1 {
		t.Fatal("duplicate records")
	}
}

func TestGitOutputBoundAndCancellation(t *testing.T) {
	var out capOutput
	input := []byte(strings.Repeat("x", 100<<10))
	if n, err := out.Write(input); n != len(input) || err != nil || len(out.data) != 64<<10 {
		t.Fatal("unbounded output")
	}
	m := newTest(t, t.TempDir(), repoTest(t))
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := m.git(ctx, m.root, "version"); err == nil {
		t.Fatal("canceled Git operation succeeded")
	}
}

func TestPersistedOwnerRequiresExplicitReconciliationAfterUncleanShutdown(t *testing.T) {
	r := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, r)
	ctx := context.Background()
	c, err := m.Create(ctx, "key", specTest(r.ID))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := m.Acquire(ctx, c.ID, "previous-execution", func(string) bool { return false }); err != nil {
		t.Fatal(err)
	}
	// Simulate losing the execution registry while its durable lease remains.
	// There is no real agent process in this storage-boundary fixture.
	m.Close()
	m = newTest(t, root, r)
	loaded, err := m.Get(c.ID)
	if err != nil || !loaded.OwnershipUncertain || loaded.OwnerExecutionID != "previous-execution" {
		t.Fatal("lost owner silently became idle", err)
	}
	if _, err := m.Acquire(ctx, c.ID, "new", func(string) bool { return false }); err == nil {
		t.Fatal("missing registry used as evidence of termination")
	}
	if _, err := m.Archive(c.ID, func(string) bool { return false }); err == nil {
		t.Fatal("archived uncertain writer")
	}
	if _, err := m.Reconcile(c.ID, "wrong-execution", "fixture has no processes", true); err == nil {
		t.Fatal("stale owner accepted")
	}
	if _, err := m.Reconcile(c.ID, "previous-execution", "fixture has no processes", false); err == nil {
		t.Fatal("missing attestation accepted")
	}
	if _, err := m.Reconcile(c.ID, "previous-execution", "", true); err == nil {
		t.Fatal("missing reason accepted")
	}
	if _, err := m.Reconcile(c.ID, "previous-execution", "fixture has no processes", true); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Acquire(ctx, c.ID, "new", func(string) bool { return false }); err != nil {
		t.Fatal(err)
	}
	if err := m.Release(c.ID, "new"); err != nil {
		t.Fatal(err)
	}
	m.Close()
	m = newTest(t, root, r)
	loaded, err = m.Get(c.ID)
	if err != nil || loaded.OwnershipUncertain || loaded.OwnerExecutionID != "" || loaded.LastOwnershipResolution == "" {
		t.Fatal("clean release not durable", err)
	}
}

func TestHandoffEvidenceInspectsCurrentRevisionAndDirtyWorktree(t *testing.T) {
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(context.Background(), "evidence", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	if err = os.WriteFile(filepath.Join(member.Path, "source.txt"), []byte("changed"), 0600); err != nil {
		t.Fatal(err)
	}
	evidence, err := m.Evidence(context.Background(), c.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(evidence) != 1 || evidence[0].Head != gitTest(t, member.Path, "rev-parse", "HEAD") || !strings.Contains(evidence[0].Status, "source.txt") || evidence[0].Error != "" {
		t.Fatalf("%+v", evidence)
	}
}

func TestInspectionPathRejectsUnregisteredOrReplacedWorktree(t *testing.T) {
	root := t.TempDir()
	source := filepath.Join(root, "source")
	if err := os.Mkdir(source, 0700); err != nil {
		t.Fatal(err)
	}
	gitTest(t, source, "init")
	gitTest(t, source, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial")
	m, err := New(filepath.Join(root, "workspaces"), []Repository{{ID: "repo", Path: source}})
	if err != nil {
		t.Fatal(err)
	}
	defer m.Close()
	c, err := m.Create(context.Background(), "inspect", Spec{Name: "Review", Members: []MemberSpec{{Name: "repo", RepositoryID: "repo"}}})
	if err != nil {
		t.Fatal(err)
	}
	path, err := m.InspectionPath(context.Background(), c.ID, "repo")
	if err != nil || path != c.Members[0].Path {
		t.Fatal(path, err)
	}
	if _, err = m.InspectionPath(context.Background(), c.ID, "../source"); err == nil {
		t.Fatal("unregistered member accepted")
	}
	moved := path + "-moved"
	if err = os.Rename(path, moved); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(source, path); err != nil {
		t.Fatal(err)
	}
	if _, err = m.InspectionPath(context.Background(), c.ID, "repo"); err == nil {
		t.Fatal("replaced worktree accepted")
	}
}

// A Runner that re-attached an owner's live agent knows that owner is real.
// Owners nobody adopted stay uncertain and are reconciled first.
func TestAdoptedOwnersAreCertainAndUnadoptedOnesStillNeedReconciliation(t *testing.T) {
	root := t.TempDir()
	m := newTest(t, root)
	ctx := context.Background()
	c, err := m.Create(ctx, "adopt", Spec{Name: "Adopted"})
	if err != nil {
		t.Fatal(err)
	}
	for _, id := range []string{"adopted", "orphaned"} {
		if _, err = m.Acquire(ctx, c.ID, id, func(string) bool { return true }); err != nil {
			t.Fatal(err)
		}
	}
	_ = m.Close()
	m = newTest(t, root)
	if err = m.Adopted(c.ID, "adopted"); err != nil {
		t.Fatal(err)
	}
	loaded, _ := m.Get(c.ID)
	if !loaded.OwnershipUncertain || loaded.OwnerExecutionID != "orphaned" {
		t.Fatalf("unadopted owner not first in line for reconciliation: %+v", loaded.OwnerExecutionIDs)
	}
	if _, err = m.Reconcile(c.ID, "orphaned", "stopped fixture", true); err != nil {
		t.Fatal(err)
	}
	loaded, _ = m.Get(c.ID)
	if loaded.OwnershipUncertain || len(loaded.OwnerExecutionIDs) != 1 || loaded.OwnerExecutionID != "adopted" {
		t.Fatalf("adopted owner left uncertain after reconciling the orphan: %+v", loaded)
	}
	if _, err = m.Acquire(ctx, c.ID, "new", func(id string) bool { return id == "adopted" }); err != nil {
		t.Fatalf("workspace with only a re-attached owner refused new sessions: %v", err)
	}
}
