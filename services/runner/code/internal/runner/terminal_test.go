package runner

import (
	"context"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

func TestWorkspaceDeletionHangsUpItsTerminals(t *testing.T) {
	t.Setenv("SHELL", "/bin/sh")
	t.Setenv("HOME", t.TempDir())
	b, err := New(Config{State: t.TempDir(), WorktreeRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	c, err := b.Collections.Create(context.Background(), "terminal-cleanup", collections.Spec{Name: "Disposable"})
	if err != nil {
		t.Fatal(err)
	}
	term, err := b.OpenTerminal(c.ID, "", 80, 24, "")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if _, err = b.DeleteWorkspace(ctx, c.ID, c.Name, false, true); err != nil {
		t.Fatal(err)
	}
	if !term.Status().Done || len(b.Terminals.List(c.ID)) != 0 {
		t.Fatal("a shell kept running in a deleted workspace")
	}
	if _, err = b.OpenTerminal(c.ID, "", 80, 24, ""); err == nil {
		t.Fatal("a deleted workspace must not get a terminal")
	}
}
