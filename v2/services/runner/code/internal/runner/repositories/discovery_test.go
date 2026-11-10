package repositories

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func discoveryRepo(t *testing.T, path string, bare bool) {
	t.Helper()
	if err := os.MkdirAll(path, 0700); err != nil {
		t.Fatal(err)
	}
	args := []string{"init", path}
	if bare {
		args = append(args, "--bare")
	}
	cmd := exec.Command("git", args...)
	for _, entry := range os.Environ() {
		if !strings.HasPrefix(entry, "GIT_") {
			cmd.Env = append(cmd.Env, entry)
		}
	}
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatal(string(out), err)
	}
}

type countingScanner struct {
	calls int
	err   error
}

func (s *countingScanner) Scan(context.Context, string) (ScanResult, error) {
	s.calls++
	return ScanResult{}, s.err
}

type brokenCatalogStore struct{ CatalogStore }

func (brokenCatalogStore) Save(Catalog) error { return errors.New("save failed") }

func TestDiscoveryPersistsOnceAndListsWithoutRescanning(t *testing.T) {
	root := t.TempDir()
	root, _ = filepath.EvalSymlinks(root)
	store, err := NewCatalogStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	scanner := &countingScanner{}
	d, err := NewDiscovery(store, scanner)
	if err != nil {
		t.Fatal(err)
	}
	if len(d.List().Roots) != 0 || scanner.calls != 0 {
		t.Fatal("discovery scanned on construction")
	}
	first, err := d.Scan(context.Background(), root)
	if err != nil || len(first.Roots) != 1 {
		t.Fatal(first, err)
	}
	for i := 0; i < 5; i++ {
		d.List()
	}
	if scanner.calls != 1 {
		t.Fatal("listing rescanned filesystem")
	}
	restored, err := NewDiscovery(store, scanner)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(first, restored.List()) || scanner.calls != 1 {
		t.Fatal("lost saved discovery")
	}
	first.Roots[0].Path = "mutated"
	if restored.List().Roots[0].Path != root {
		t.Fatal("caller mutated catalog")
	}
	scanner.err = errors.New("scan failed")
	if _, err := restored.Scan(context.Background(), root); err == nil {
		t.Fatal("scan failure hidden")
	}
	if restored.List().Roots[0].Path != root {
		t.Fatal("scan failure discarded catalog")
	}
	scanner.err = nil
	restored.store = brokenCatalogStore{store}
	if _, err := restored.Scan(context.Background(), t.TempDir()); err == nil || len(restored.List().Roots) != 1 {
		t.Fatal("save failure changed catalog")
	}
}

func TestDirectoryDiscoveryBoundariesAndReadOnlyGit(t *testing.T) {
	root := t.TempDir()
	root, _ = filepath.EvalSymlinks(root)
	api := filepath.Join(root, "team", "api")
	bare := filepath.Join(root, "bare.git")
	discoveryRepo(t, api, false)
	discoveryRepo(t, bare, true)
	discoveryRepo(t, filepath.Join(root, "node_modules", "ignored"), false)
	discoveryRepo(t, filepath.Join(root, "vendors", "package", "v1.0.0"), false)
	discoveryRepo(t, filepath.Join(root, ".hidden", "ignored"), false)
	discoveryRepo(t, filepath.Join(api, "nested"), false)
	outside := t.TempDir()
	discoveryRepo(t, filepath.Join(outside, "outside"), false)
	if err := os.Symlink(outside, filepath.Join(root, "shortcut")); err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(api, "uncommitted.txt")
	os.WriteFile(file, []byte("keep"), 0600)
	config := filepath.Join(api, ".git", "config")
	original, _ := os.ReadFile(config)
	t.Setenv("GIT_DIR", "/must-not-use-another-git-directory")
	result, err := (DirectoryScanner{}).Scan(context.Background(), root)
	if err != nil || len(result.Candidates) != 2 {
		t.Fatal(result, err)
	}
	if result.Candidates[0].Path != bare || result.Candidates[1].Path != api {
		t.Fatal(result.Candidates)
	}
	current, _ := os.ReadFile(config)
	content, _ := os.ReadFile(file)
	info, _ := os.Stat(file)
	if string(current) != string(original) || string(content) != "keep" || info.Mode().Perm() != 0600 {
		t.Fatal("scan modified original checkout")
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := (DirectoryScanner{}).Scan(ctx, root); !errors.Is(err, context.Canceled) {
		t.Fatal(err)
	}
	deep := filepath.Join(root, "one", "two", "three", "four", "five", "six", "seven")
	discoveryRepo(t, deep, false)
	result, err = (DirectoryScanner{}).Scan(context.Background(), root)
	if err != nil || !result.Truncated {
		t.Fatal("depth limit not surfaced", err)
	}
}

func TestDiscoveryRejectsInvalidPathsAndCatalogs(t *testing.T) {
	if _, err := NewCatalogStore("relative"); err == nil {
		t.Fatal("accepted relative state")
	}
	store, _ := NewCatalogStore(t.TempDir())
	d, _ := NewDiscovery(store, &countingScanner{})
	for _, path := range []string{"relative", filepath.Join(t.TempDir(), "missing")} {
		if _, err := d.Scan(context.Background(), path); err == nil {
			t.Fatal("accepted invalid root")
		}
	}
	os.WriteFile(store.path, []byte(`{"Version":99}`), 0600)
	if _, err := NewDiscovery(store, DirectoryScanner{}); err == nil {
		t.Fatal("accepted unknown catalog")
	}
	os.Remove(store.path)
	os.Symlink(filepath.Join(t.TempDir(), "elsewhere"), store.path)
	if _, err := store.Load(); err == nil {
		t.Fatal("followed catalog symlink")
	}
}

func TestDiscoveryGroupsWorktreesAndResolvesPrimaryOutsideSearchRoot(t *testing.T) {
	root := t.TempDir()
	root, _ = filepath.EvalSymlinks(root)
	primary := filepath.Join(root, "original", "api")
	linkedRoot := filepath.Join(root, "features")
	discoveryRepo(t, primary, false)
	run := func(args ...string) {
		t.Helper()
		if out, err := exec.Command("git", args...).CombinedOutput(); err != nil {
			t.Fatal(string(out), err)
		}
	}
	run("-C", primary, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial")
	for _, name := range []string{"feature-one", "feature-two"} {
		run("-C", primary, "worktree", "add", "-b", name, filepath.Join(linkedRoot, name))
	}
	result, err := (DirectoryScanner{}).Scan(context.Background(), linkedRoot)
	if err != nil || len(result.Candidates) != 1 {
		t.Fatal("linked worktrees were listed as repositories", result, err)
	}
	repo := result.Candidates[0]
	if repo.Path != primary || repo.Name != "api" || repo.GitIdentity == "" {
		t.Fatal("primary repository was not resolved", repo)
	}
	// Overlapping roots must carry the same stable identity, including a scan
	// that encounters the main checkout after its linked worktrees.
	all, err := (DirectoryScanner{}).Scan(context.Background(), root)
	if err != nil || len(all.Candidates) != 1 || all.Candidates[0].GitIdentity != repo.GitIdentity {
		t.Fatal(all, err)
	}
	store, _ := NewCatalogStore(t.TempDir())
	d, _ := NewDiscovery(store, DirectoryScanner{})
	if _, err := d.Scan(context.Background(), linkedRoot); err != nil {
		t.Fatal(err)
	}
	restored, err := NewDiscovery(store, DirectoryScanner{})
	if err != nil || restored.List().Roots[0].Candidates[0].Path != primary {
		t.Fatal("resolved primary outside search root did not persist", err)
	}
	// Repositories without a recognized GitHub remote remain local sources.
	clone := filepath.Join(root, "separate-clone")
	run("clone", primary, clone)
	all, err = (DirectoryScanner{}).Scan(context.Background(), root)
	if err != nil || len(all.Candidates) != 2 || all.Candidates[0].GitIdentity == all.Candidates[1].GitIdentity {
		t.Fatal("independent clones were conflated", all, err)
	}
	bare := filepath.Join(root, "bare-source.git")
	run("clone", "--bare", primary, bare)
	run("--git-dir="+bare, "worktree", "add", "--detach", filepath.Join(linkedRoot, "bare-feature"))
	linked, err := (DirectoryScanner{}).Scan(context.Background(), linkedRoot)
	if err != nil || len(linked.Candidates) != 2 {
		t.Fatal(linked, err)
	}
	found := false
	for _, candidate := range linked.Candidates {
		if candidate.Path == bare && candidate.Name == "bare-source" {
			found = true
		}
	}
	if !found {
		t.Fatal("bare-backed worktree did not resolve bare source", linked)
	}
}

func TestDiscoveryResolvesBareSourceAndMigratesCheckoutCache(t *testing.T) {
	root := t.TempDir()
	root, _ = filepath.EvalSymlinks(root)
	primary := filepath.Join(root, "api.git")
	discoveryRepo(t, primary, true)
	result, err := (DirectoryScanner{}).Scan(context.Background(), root)
	if err != nil || len(result.Candidates) != 1 || result.Candidates[0].Name != "api" || result.Candidates[0].Path != primary {
		t.Fatal(result, err)
	}
	store, _ := NewCatalogStore(t.TempDir())
	if err := store.Save(Catalog{Version: 1, Roots: []DiscoveryRoot{{Path: root, ScannedAt: 1, ScanResult: result}}}); err != nil {
		t.Fatal(err)
	}
	scanner := &countingScanner{}
	d, err := NewDiscovery(store, scanner)
	if err != nil {
		t.Fatal(err)
	}
	c := d.List()
	if c.Version != 3 || len(c.Roots) != 1 || c.Roots[0].ScannedAt != 0 || len(c.Roots[0].Candidates) != 0 || scanner.calls != 0 {
		t.Fatal("old checkout rows survived migration or implicitly rescanned", c)
	}
}

func TestDiscoveryGroupsGitHubRepositoryAcrossClonesAndWorktrees(t *testing.T) {
	root := t.TempDir()
	root, _ = filepath.EvalSymlinks(root)
	first := filepath.Join(root, "arbitrary-folder")
	second := filepath.Join(root, "different-copy")
	fork := filepath.Join(root, "fork")
	for _, path := range []string{first, second, fork} {
		discoveryRepo(t, path, false)
		gitTest(t, path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial")
	}
	gitTest(t, first, "remote", "add", "origin", "git@github.com:Team/API.git")
	gitTest(t, second, "remote", "add", "origin", "https://github.com/team/api.git")
	gitTest(t, fork, "remote", "add", "origin", "https://github.com/fork/api.git")
	gitTest(t, fork, "remote", "add", "upstream", "https://github.com/team/api.git")
	gitTest(t, first, "worktree", "add", "-b", "feature", filepath.Join(root, "feature-tree"))
	before, _ := os.ReadFile(filepath.Join(first, ".git", "config"))
	result, err := (DirectoryScanner{}).Scan(context.Background(), root)
	if err != nil || len(result.Candidates) != 2 {
		t.Fatal(result, err)
	}
	seen := map[string]bool{}
	for _, c := range result.Candidates {
		seen[c.Name] = true
		if c.GitIdentity != "github:"+c.Name {
			t.Fatal(c)
		}
	}
	if !seen["team/api"] || !seen["fork/api"] {
		t.Fatal(result)
	}
	after, _ := os.ReadFile(filepath.Join(first, ".git", "config"))
	if string(before) != string(after) {
		t.Fatal("discovery modified origin/auth configuration")
	}
	// The previous local-Git-store catalog must also be invalidated.
	store, _ := NewCatalogStore(t.TempDir())
	if err := store.Save(Catalog{Version: 2, Roots: []DiscoveryRoot{{Path: root, ScannedAt: 1, ScanResult: result}}}); err != nil {
		t.Fatal(err)
	}
	d, err := NewDiscovery(store, &countingScanner{})
	if err != nil || d.List().Roots[0].ScannedAt != 0 || len(d.List().Roots[0].Candidates) != 0 {
		t.Fatal("local identity cache retained", err)
	}
}

func TestRemoveDiscoveryPersistsWithoutTouchingCheckout(t *testing.T) {
	root := t.TempDir()
	root, _ = filepath.EvalSymlinks(root)
	path := filepath.Join(root, "repo")
	discoveryRepo(t, path, false)
	store, err := NewCatalogStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	d, err := NewDiscovery(store, DirectoryScanner{})
	if err != nil {
		t.Fatal(err)
	}
	catalog, err := d.Scan(context.Background(), root)
	if err != nil || len(catalog.Roots[0].Candidates) != 1 {
		t.Fatal(catalog, err)
	}
	identity := catalog.Roots[0].Candidates[0].GitIdentity
	removed, err := d.Remove(identity)
	if err != nil || len(removed.Roots[0].Candidates) != 0 {
		t.Fatal(removed, err)
	}
	d, err = NewDiscovery(store, DirectoryScanner{})
	if err != nil || len(d.List().Roots[0].Candidates) != 0 {
		t.Fatal(err)
	}
	if _, err = os.Stat(filepath.Join(path, ".git")); err != nil {
		t.Fatal("checkout removed", err)
	}
}
