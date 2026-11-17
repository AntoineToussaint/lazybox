package runner

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

// The Runner follows a tracked file: an edit on disk becomes a version, a
// removed file is reported without losing any.
func TestRunnerRecordsTrackedFileChanges(t *testing.T) {
	repo := t.TempDir()
	for _, args := range [][]string{{"init", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		if out, err := exec.Command("git", append([]string{"-C", repo}, args...)...).CombinedOutput(); err != nil {
			t.Fatal(err, string(out))
		}
	}
	b, err := New(Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), WorktreeRoot: t.TempDir(), Repositories: []collections.Repository{{ID: "api", Path: repo}}, ArtifactPoll: 10 * time.Millisecond, ArtifactDebounce: 30 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	c, err := b.Collections.Create(context.Background(), "artifacts", collections.Spec{Name: "Plans", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(c.Members[0].Path, "PLAN.md")
	if err = os.WriteFile(file, []byte("# Plan\nv1"), 0o644); err != nil {
		t.Fatal(err)
	}
	member, _ := filepath.Rel(c.Root, c.Members[0].Path)
	doc, err := b.Artifacts.Track(c.ID, member+"/PLAN.md", "", "plan", "# Plan\nv1", artifacts.Author{Kind: "person"})
	if err != nil {
		t.Fatal(err)
	}
	await := func(what string, ok func(artifacts.Document) bool) artifacts.Document {
		t.Helper()
		deadline := time.Now().Add(10 * time.Second)
		for time.Now().Before(deadline) {
			if d, err := b.Artifacts.Get(c.ID, doc.ID, 0); err == nil && ok(d) {
				return d
			}
			time.Sleep(10 * time.Millisecond)
		}
		t.Fatal("timed out waiting for", what)
		return artifacts.Document{}
	}
	if err = os.WriteFile(file, []byte("# Plan\nv2 by an agent"), 0o644); err != nil {
		t.Fatal(err)
	}
	got := await("the edit", func(d artifacts.Document) bool { return d.ContentVersion == 2 })
	if got.Content != "# Plan\nv2 by an agent" || got.Versions[1].Author.Kind != "file" {
		t.Fatalf("%+v", got)
	}
	if err = os.Remove(file); err != nil {
		t.Fatal(err)
	}
	got = await("the removal", func(d artifacts.Document) bool { return d.TrackingProblem != "" })
	if got.ContentVersion != 2 || got.TrackingProblem != "the file was removed; the last version is kept" {
		t.Fatalf("%+v", got)
	}
	if err = os.WriteFile(file, []byte("# Plan\nv2 by an agent"), 0o644); err != nil {
		t.Fatal(err)
	}
	await("the return", func(d artifacts.Document) bool { return d.TrackingProblem == "" && d.ContentVersion == 2 })
	if ws, err := b.SessionWorkspace("missing"); err == nil {
		t.Fatal("unknown execution has a workspace", ws)
	}
}
