package collections

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

func TestCreateFileWritesNewFilesInsideRepositoriesOnly(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "create", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member, err := filepath.Rel(c.Root, c.Members[0].Path)
	if err != nil {
		t.Fatal(err)
	}
	created, err := m.CreateFile(ctx, c.ID, member+"/docs/plans/PLAN.md", "# Plan\n")
	if err != nil || created.Text != "# Plan\n" || created.Version == "" || created.Path != member+"/docs/plans/PLAN.md" {
		t.Fatal(created, err)
	}
	info, err := os.Stat(filepath.Join(c.Members[0].Path, "docs", "plans", "PLAN.md"))
	if err != nil || info.Mode().Perm() != 0o644 {
		t.Fatal(info, err)
	}
	if _, err = m.CreateFile(ctx, c.ID, member+"/docs/plans/PLAN.md", "again"); !errors.Is(err, ErrFileExists) {
		t.Fatal("replaced an existing file", err)
	}
	data, _ := os.ReadFile(filepath.Join(c.Members[0].Path, "docs", "plans", "PLAN.md"))
	if string(data) != "# Plan\n" {
		t.Fatal(string(data))
	}
	outside := t.TempDir()
	if err = os.Symlink(outside, filepath.Join(c.Members[0].Path, "linked")); err != nil {
		t.Fatal(err)
	}
	for _, reference := range []string{"PLAN.md", "unknown/PLAN.md", "../PLAN.md", "/tmp/PLAN.md", member, member + "/.git/PLAN.md", member + "/linked/PLAN.md", member + "/source.txt/PLAN.md"} {
		if _, err = m.CreateFile(ctx, c.ID, reference, "x"); err == nil {
			t.Fatalf("accepted %s", reference)
		}
	}
	if entries, _ := os.ReadDir(outside); len(entries) != 0 {
		t.Fatal("wrote through a symlink", entries)
	}
	if _, err = m.CreateFile(ctx, c.ID, member+"/binary.md", "a\x00b"); err == nil {
		t.Fatal("accepted a NUL byte")
	}
}
