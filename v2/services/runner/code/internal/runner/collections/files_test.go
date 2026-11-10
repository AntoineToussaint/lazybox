package collections

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestFilePreviewConfinesPathsAndBoundsContent(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "files", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(c.Members[0].Path, "preview.md")
	if err = os.WriteFile(path, []byte("# Preview\nsecond line"), 0600); err != nil {
		t.Fatal(err)
	}
	for _, reference := range []string{path, filepath.Join(c.Members[0].Name, "preview.md"), "preview.md"} {
		result, err := m.ReadFile(ctx, c.ID, reference)
		if err != nil || result.Text != "# Preview\nsecond line" || result.Binary {
			t.Fatal(result, err)
		}
	}
	outside := filepath.Join(t.TempDir(), "outside.txt")
	if err = os.WriteFile(outside, []byte("outside"), 0600); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(outside, filepath.Join(c.Root, "escape.txt")); err != nil {
		t.Fatal(err)
	}
	for _, reference := range []string{outside, "../outside.txt", "escape.txt"} {
		if _, err = m.ReadFile(ctx, c.ID, reference); err == nil {
			t.Fatalf("accepted %s", reference)
		}
	}
	if err = os.WriteFile(path, []byte(strings.Repeat("a", previewLimit+100)), 0600); err != nil {
		t.Fatal(err)
	}
	result, err := m.ReadFile(ctx, c.ID, path)
	if err != nil || !result.Truncated || len(result.Text) != previewLimit {
		t.Fatal(result.Truncated, err)
	}
	if err = os.WriteFile(path, []byte{0, 1, 2}, 0600); err != nil {
		t.Fatal(err)
	}
	result, err = m.ReadFile(ctx, c.ID, path)
	if err != nil || !result.Binary || result.Text != "" {
		t.Fatal(result, err)
	}
}

func TestSaveFilePreservesModeRejectsStaleVersionsAndUnsafePaths(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "edit", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(c.Members[0].Path, "script.sh")
	if err = os.WriteFile(path, []byte("old"), 0750); err != nil {
		t.Fatal(err)
	}
	loaded, err := m.ReadFile(ctx, c.ID, path)
	if err != nil || loaded.Version == "" {
		t.Fatal(loaded, err)
	}
	saved, err := m.SaveFile(ctx, c.ID, path, "new", loaded.Version)
	if err != nil || saved.Text != "new" || saved.Version == loaded.Version {
		t.Fatal(saved, err)
	}
	mode, _ := os.Stat(path)
	if mode.Mode().Perm() != 0750 {
		t.Fatal("mode changed", mode.Mode())
	}
	if _, err = m.SaveFile(ctx, c.ID, path, "stale", loaded.Version); !errors.Is(err, ErrFileChanged) {
		t.Fatal("stale edit accepted", err)
	}
	if _, err = m.SaveFile(ctx, c.ID, path, "new", loaded.Version); err != nil {
		t.Fatal("retry failed", err)
	}
	os.WriteFile(path, []byte("agent edit"), 0750)
	if _, err = m.SaveFile(ctx, c.ID, path, "overwrite", saved.Version); !errors.Is(err, ErrFileChanged) {
		t.Fatal("agent edit overwritten", err)
	}
	current, _ := m.ReadFile(ctx, c.ID, path)
	link := filepath.Join(c.Members[0].Path, "link.sh")
	os.Symlink("script.sh", link)
	if _, err = m.SaveFile(ctx, c.ID, link, "symlink write", current.Version); err == nil {
		t.Fatal("accepted symlink editing")
	}
	os.Chmod(path, 0440)
	current, _ = m.ReadFile(ctx, c.ID, path)
	if _, err = m.SaveFile(ctx, c.ID, path, "read-only write", current.Version); err == nil {
		t.Fatal("changed read-only file")
	}
	data, _ := os.ReadFile(path)
	if string(data) != "agent edit" {
		t.Fatal(string(data))
	}
	matches, _ := filepath.Glob(filepath.Join(filepath.Dir(path), ".lazybox-save-*"))
	if len(matches) != 0 {
		t.Fatal("temporary files leaked", matches)
	}
	for _, reference := range []string{"../outside", filepath.Join(t.TempDir(), "outside")} {
		if _, err = m.SaveFile(ctx, c.ID, reference, "bad", current.Version); err == nil {
			t.Fatal("escaped", reference)
		}
	}
}

func TestDirectoryPreviewAndWorkspacePrefix(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "folder", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	if err = os.WriteFile(filepath.Join(member.Path, "readme.md"), []byte("# Hello"), 0600); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(t.TempDir(), filepath.Join(member.Path, "outside")); err != nil {
		t.Fatal(err)
	}
	for _, ref := range []string{member.Name, filepath.Join(filepath.Base(c.Root), member.Name), member.Path} {
		result, err := m.ReadFile(ctx, c.ID, ref)
		if err != nil || !result.Directory {
			t.Fatal(result, err)
		}
		found := false
		for _, entry := range result.Entries {
			if entry.Name == ".git" || entry.Name == "outside" {
				t.Fatalf("unsafe entry: %s", entry.Name)
			}
			if entry.Name == "readme.md" {
				found = true
			}
		}
		if !found {
			t.Fatal("missing file")
		}
	}
	file, err := m.ReadFile(ctx, c.ID, filepath.Join(filepath.Base(c.Root), member.Name, "readme.md"))
	if err != nil || file.Text != "# Hello" {
		t.Fatal(file, err)
	}
	if _, err = m.ReadFile(ctx, c.ID, filepath.Join("wrong-workspace", member.Name, "readme.md")); err == nil {
		t.Fatal("guessed an unknown prefix")
	}
	if _, err = m.SaveFile(ctx, c.ID, member.Name, "", "revision"); err == nil {
		t.Fatal("directory accepted as editable file")
	}
}
