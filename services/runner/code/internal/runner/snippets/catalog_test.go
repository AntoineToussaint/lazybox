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
