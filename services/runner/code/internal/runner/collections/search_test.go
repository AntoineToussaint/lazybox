package collections

import (
	"context"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"
)

func writeTest(t *testing.T, path, text string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(text), 0600); err != nil {
		t.Fatal(err)
	}
}

// twoRepositories is a workspace of api and web, each a worktree of its own
// repository, with the files link resolution and search are tested against.
func twoRepositories(t *testing.T) (*Manager, Collection) {
	t.Helper()
	a, b := repoTest(t), repoTest(t)
	a.ID, b.ID = "api", "web"
	writeTest(t, filepath.Join(a.Path, ".gitignore"), "ignored/\n*.log\n")
	writeTest(t, filepath.Join(a.Path, "scripts", "undeclared-routes.json"), "{}")
	writeTest(t, filepath.Join(a.Path, "pkg", "deep", "thing.go"), "package deep")
	writeTest(t, filepath.Join(b.Path, "src", "routes.ts"), "export {}")
	gitTest(t, a.Path, "add", ".")
	gitTest(t, a.Path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "files")
	gitTest(t, b.Path, "add", ".")
	gitTest(t, b.Path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "files")
	m := newTest(t, t.TempDir(), a, b)
	c, err := m.Create(context.Background(), "two", Spec{Name: "Two", Members: []MemberSpec{{Name: "api", RepositoryID: "api"}, {Name: "web", RepositoryID: "web"}}})
	if err != nil {
		t.Fatal(err)
	}
	return m, c
}

func paths(matches []PathMatch) []string {
	out := []string{}
	for _, match := range matches {
		out = append(out, match.Path)
	}
	return out
}

func TestResolvePathFindsWhatAgentsMean(t *testing.T) {
	ctx := context.Background()
	m, c := twoRepositories(t)
	outside := filepath.Join(t.TempDir(), "secret.txt")
	writeTest(t, outside, "outside")
	if err := os.Symlink(outside, filepath.Join(c.Root, "api", "secret.txt")); err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		reference string
		want      []string
		exact     bool
	}{
		// The reported failure: a bare name that lives below a repository.
		{"undeclared-routes.json", []string{"api/scripts/undeclared-routes.json"}, false},
		// Relative to the repository, without its name.
		{"scripts/undeclared-routes.json", []string{"api/scripts/undeclared-routes.json"}, true},
		{"./scripts/undeclared-routes.json", []string{"api/scripts/undeclared-routes.json"}, true},
		{"api/scripts/undeclared-routes.json", []string{"api/scripts/undeclared-routes.json"}, true},
		// An absolute path from another checkout is looked up by its tail.
		{"/Users/someone/dev/api/scripts/undeclared-routes.json", []string{"api/scripts/undeclared-routes.json"}, true},
		{filepath.Join(c.Root, "web", "src", "routes.ts"), []string{"web/src/routes.ts"}, true},
		// The same repository-relative path in both repositories is a choice.
		{"source.txt", []string{"api/source.txt", "web/source.txt"}, true},
		// A path suffix below a repository.
		{"deep/thing.go", []string{"api/pkg/deep/thing.go"}, false},
		{"scripts", []string{"api/scripts"}, true},
		{"pkg/deep", []string{"api/pkg/deep"}, true},
		{"deep", []string{"api/pkg/deep"}, false},
		{"web", []string{"web"}, true},
	} {
		matches, exact, _, err := m.ResolvePath(ctx, c.ID, test.reference)
		if err != nil || !reflect.DeepEqual(paths(matches), test.want) || exact != test.exact {
			t.Errorf("%s: got %v exact=%v err=%v; want %v exact=%v", test.reference, paths(matches), exact, err, test.want, test.exact)
		}
	}
	directory, _, _, _ := m.ResolvePath(ctx, c.ID, "scripts")
	if len(directory) != 1 || !directory[0].Directory {
		t.Fatalf("folder not marked: %+v", directory)
	}
	// Nothing outside the workspace is ever a match: not by traversal, not by
	// an absolute path, not through a symlink out of it.
	for _, reference := range []string{"../../secret.txt", outside, "secret.txt", "api/secret.txt", "missing.json", "/etc/passwd"} {
		matches, _, _, err := m.ResolvePath(ctx, c.ID, reference)
		if err != nil || len(matches) != 0 {
			t.Errorf("%s: got %v %v; want no match", reference, paths(matches), err)
		}
	}
	for _, reference := range []string{"", strings.Repeat("a", referenceMaxBytes+1), "a\x00b"} {
		if _, _, _, err := m.ResolvePath(ctx, c.ID, reference); err == nil {
			t.Errorf("accepted %q", reference)
		}
	}
	if _, _, _, err := m.ResolvePath(ctx, "missing", "README.md"); err == nil {
		t.Fatal("resolved in an unknown workspace")
	}
}

func TestSearchFilesRespectsGitignoreAndStaysInside(t *testing.T) {
	ctx := context.Background()
	m, c := twoRepositories(t)
	api := filepath.Join(c.Root, "api")
	writeTest(t, filepath.Join(api, "notes.md"), "untracked, not ignored")
	writeTest(t, filepath.Join(api, "debug.log"), "ignored by pattern")
	writeTest(t, filepath.Join(api, "ignored", "routes-cache.ts"), "ignored folder")
	outside := filepath.Join(t.TempDir(), "routes-outside.ts")
	writeTest(t, outside, "outside")
	if err := os.Symlink(outside, filepath.Join(api, "routes-link.ts")); err != nil {
		t.Fatal(err)
	}
	search := func(query string, limit int) []FileMatch {
		t.Helper()
		matches, _, _, err := m.SearchFiles(ctx, c.ID, query, limit)
		if err != nil {
			t.Fatal(err)
		}
		return matches
	}
	// Not the ignored routes-cache.ts nor the routes-link.ts symlink out of the
	// workspace; the exact file name ranks first.
	found := search("routes", 0)
	if len(found) != 2 || found[0].Path != "web/src/routes.ts" || found[1].Path != "api/scripts/undeclared-routes.json" {
		t.Fatalf("ignored, escaping or missing results: %+v", found)
	}
	if want := []uint32{8, 9, 10, 11, 12, 13}; !reflect.DeepEqual(found[0].Positions, want) {
		t.Fatalf("positions %v, want %v", found[0].Positions, want)
	}
	if found := search("notes", 0); len(found) == 0 || found[0].Path != "api/notes.md" {
		t.Fatalf("untracked file missing: %+v", found)
	}
	if found := search("debug", 0); len(found) != 0 {
		t.Fatalf("ignored file listed: %+v", found)
	}
	// File names rank above directories that merely contain the letters.
	if found := search("thing", 0); len(found) != 1 || found[0].Path != "api/pkg/deep/thing.go" {
		t.Fatalf("%+v", found)
	}
	if found := search("src/rou", 0); len(found) != 1 || found[0].Path != "web/src/routes.ts" {
		t.Fatalf("path query: %+v", found)
	}
	if found := search("t", 1); len(found) != 1 {
		t.Fatalf("limit ignored: %d", len(found))
	}
	if found := search("", 0); len(found) != 0 {
		t.Fatalf("empty query: %+v", found)
	}
	if _, _, _, err := m.SearchFiles(ctx, c.ID, strings.Repeat("a", queryLimit+1), 0); err == nil {
		t.Fatal("unbounded query accepted")
	}
}

func TestFileIndexIsBoundedAndRefreshes(t *testing.T) {
	ctx := context.Background()
	m, c := twoRepositories(t)
	limit, ttl := indexLimit, indexTTL
	t.Cleanup(func() { indexLimit, indexTTL = limit, ttl })
	indexLimit, indexTTL = 3, time.Hour
	_, truncated, indexed, err := m.SearchFiles(ctx, c.ID, "x", 0)
	if err != nil || !truncated || indexed != 3 {
		t.Fatalf("index not bounded: truncated=%v indexed=%d err=%v", truncated, indexed, err)
	}
	indexLimit = limit
	writeTest(t, filepath.Join(c.Root, "web", "fresh.txt"), "new")
	// Within the cache lifetime the old index answers; a stale index expires.
	if found, _, _, _ := m.SearchFiles(ctx, c.ID, "fresh", 0); len(found) != 0 {
		t.Fatalf("cache not used: %+v", found)
	}
	indexTTL = 0
	if found, _, _, _ := m.SearchFiles(ctx, c.ID, "fresh", 0); len(found) != 1 {
		t.Fatalf("index not refreshed: %+v", found)
	}
}

func TestFuzzyScorePrefersFileNamesAndWordStarts(t *testing.T) {
	rank := func(query string, candidates ...string) string {
		best, top := "", -1<<31
		for _, candidate := range candidates {
			if score, _, ok := fuzzyScore(candidate, []rune(query)); ok && score > top {
				best, top = candidate, score
			}
		}
		return best
	}
	if got := rank("readme", "docs/readers/media.txt", "api/README.md"); got != "api/README.md" {
		t.Fatal(got)
	}
	if got := rank("ws", "packages/surfaces/src/Workspace.tsx", "packages/surfaces/src/aws-users.ts"); got != "packages/surfaces/src/Workspace.tsx" {
		t.Fatal(got)
	}
	if got := rank("wm", "a/workspace-model.ts", "a/swimmer.ts"); got != "a/workspace-model.ts" {
		t.Fatal(got)
	}
	if _, _, ok := fuzzyScore("api/source.txt", []rune("zz")); ok {
		t.Fatal("matched absent characters")
	}
	if got := utf16Positions("é😀a", []int{0, 2}); !reflect.DeepEqual(got, []uint32{0, 3}) {
		t.Fatalf("utf16 positions %v", got)
	}
}
