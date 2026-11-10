package runner

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/attachments"
	"os"
	"path/filepath"
	"testing"
)

func TestDeleteConversationStopsResourcesAndSurvivesRestart(t *testing.T) {
	ctx := context.Background()
	cfg := Config{State: t.TempDir()}
	b, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	workspaceFile := filepath.Join(e.Cwd, "keep.txt")
	os.WriteFile(workspaceFile, []byte("workspace remains"), 0600)
	files, err := b.Attachments.Put(e.ID, []attachments.Input{{Name: "delete.txt", Data: []byte("uploaded")}})
	if err != nil {
		t.Fatal(err)
	}
	cmd, err := b.Start(e, "child", []string{"/bin/sh", "-c", "sleep 60 & wait"})
	if err != nil {
		t.Fatal(err)
	}
	if err = b.DeleteConversation(ctx, e.ID, false); err == nil {
		t.Fatal("deleted running resources without confirmation")
	}
	if err = b.DeleteConversation(ctx, e.ID, true); err != nil {
		t.Fatal(err)
	}
	if !cmd.Status().Done || !e.Engine.Snapshot().Closed {
		t.Fatal("resources leaked")
	}
	if _, err = b.Get(e.ID); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("deleted session is accessible", err)
	}
	for _, path := range []string{files[0].Path, filepath.Join(cfg.State, "sessions", e.ID+".jsonl")} {
		if _, err = os.Stat(path); !errors.Is(err, os.ErrNotExist) {
			t.Fatal("data retained", path, err)
		}
	}
	if _, err = os.Stat(workspaceFile); err != nil {
		t.Fatal("workspace lost", err)
	}
	if err = b.DeleteConversation(ctx, e.ID, true); err != nil {
		t.Fatal("idempotent deletion", err)
	}
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	b, err = New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	records, err := b.History.List(ctx, 100, 0)
	if err != nil || len(records) != 0 {
		t.Fatal(records, err)
	}
	if _, err = b.Resume(ctx, e.ID, false); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("deleted conversation resumed", err)
	}
}
