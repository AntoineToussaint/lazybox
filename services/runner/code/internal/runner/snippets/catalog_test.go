package snippets

import (
	"context"
	"os"
	"path/filepath"
	"testing"
)

func TestCommandCatalog(t *testing.T) {
	p := filepath.Join(t.TempDir(), "catalog")
	if err := os.WriteFile(p, []byte("#!/bin/sh\nprintf '%s\\n' '- key: review' '  body: Review the change.' '  origin: global'\n"), 0700); err != nil {
		t.Fatal(err)
	}
	rows, err := (Command{Binary: p}).List(context.Background())
	if err != nil || len(rows) != 1 || rows[0].Key != "review" {
		t.Fatal(rows, err)
	}
	if _, err = (Command{}).List(context.Background()); err == nil {
		t.Fatal("missing command accepted")
	}
}
func TestBoundedCatalog(t *testing.T) {
	b := &boundedOutput{}
	if _, err := b.Write(make([]byte, (4<<20)+1)); err == nil {
		t.Fatal("unbounded output")
	}
}

func TestPackIsDataAndCannotReplaceCatalogEntry(t *testing.T) {
	dir := t.TempDir()
	helper := filepath.Join(dir, "catalog")
	if err := os.WriteFile(helper, []byte("#!/bin/sh\necho '[]'\n"), 0700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "release.yaml")
	if err := os.WriteFile(path, []byte("- key: release\n  body: Verify\n  origin: built-in\n"), 0600); err != nil {
		t.Fatal(err)
	}
	cmd := Command{Binary: helper, Packs: []string{path}}
	entries, err := cmd.List(context.Background())
	if err != nil || len(entries) != 1 || entries[0].Origin != "pack: release.yaml" {
		t.Fatal(entries, err)
	}
	cmd.Packs = append(cmd.Packs, path)
	if _, err = cmd.List(context.Background()); err == nil {
		t.Fatal("duplicate accepted")
	}
}
