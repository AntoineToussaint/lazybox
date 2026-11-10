package runner

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/processguard"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
	"os"
	"strings"
	"testing"
	"time"
)

func TestWorkspaceDeletionStopsAgentAndOwnedCommandsBeforeRemovingFiles(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), WorktreeRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	c, err := b.Collections.Create(context.Background(), "cleanup", collections.Spec{Name: "Disposable"})
	if err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenCollection("codex-default", c.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	cmd, err := b.Start(e, "long-running child", []string{"/bin/sh", "-c", "sleep 60 & wait"})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err = b.DeleteWorkspace(ctx, c.ID, c.Name, false, true); err != nil {
		t.Fatal(err)
	}
	if !e.Engine.Snapshot().Closed || !cmd.Status().Done || b.Active(e.ID) {
		t.Fatal("resource still active after cleanup")
	}
	if _, err = b.Start(e, "late command", []string{"/bin/true"}); err == nil {
		t.Fatal("closed agent started a command")
	}
	if _, err = b.History.Get(ctx, e.ID); err != nil {
		t.Fatal("history lost", err)
	}
}

func TestUnconfirmedCommandCleanupCannotReleaseWorkspaceOrDeleteHistory(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), WorktreeRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	c, err := b.Collections.Create(context.Background(), "uncertain-cleanup", collections.Spec{Name: "Keep until reconciled"})
	if err != nil {
		t.Fatal(err)
	}
	e, err := b.OpenCollection("codex-default", c.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	command, err := b.Commands.Register(commands.Spec{SessionID: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	_ = command.FinishPipe(processguard.ErrCleanupUnconfirmed)
	if err = e.Engine.Close(); err != nil {
		t.Fatal(err)
	}
	if !b.Active(e.ID) {
		t.Fatal("uncertain cleanup appeared inactive")
	}
	if err = b.DeleteConversation(context.Background(), e.ID, true); err == nil || !strings.Contains(err.Error(), "unconfirmed") {
		t.Fatal("history deleted despite unresolved resources", err)
	}
	if _, err = b.DeleteWorkspace(context.Background(), c.ID, c.Name, true, true); err == nil {
		t.Fatal("workspace deleted despite unresolved resources")
	}
	if _, err = os.Stat(c.Root); err != nil {
		t.Fatal("workspace files removed", err)
	}
	if _, err = b.History.Get(context.Background(), e.ID); err != nil {
		t.Fatal("history removed", err)
	}
}
func TestClosedSessionsReleaseExecutionCapacity(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	for i := 0; i < 35; i++ {
		e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
		if err != nil {
			t.Fatalf("session %d: %v", i, err)
		}
		if err = b.StopExecution(context.Background(), e.ID); err != nil {
			t.Fatal(err)
		}
	}
}
