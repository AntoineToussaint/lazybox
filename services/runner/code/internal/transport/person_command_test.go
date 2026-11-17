package transport

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// A person's "Run myself" from a conversation: StartCommand is the owner's, so
// a read-only conversation runs it (the mode bounds the agent, not the
// person), in a folder inside the conversation's workspace and nowhere else.
// The agent's own start in that conversation is still refused.
func TestPersonStartsCommandInReadOnlyConversationInsideItsWorkspace(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	b.Adapters["codex"] = &agenttest.Adapter{}
	workspace := t.TempDir()
	repo := filepath.Join(workspace, "api")
	if err = os.Mkdir(repo, 0o700); err != nil {
		t.Fatal(err)
	}
	outside := t.TempDir()
	if err = os.Symlink(outside, filepath.Join(workspace, "escape")); err != nil {
		t.Fatal(err)
	}
	e, err := b.Open("codex-default", workspace, "", accounts.Options{Permission: "read-only"}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.Start(e, "agent", []string{"/bin/sh", "-c", "true"}); err == nil {
		t.Fatal("the agent's start ran in a read-only conversation")
	}
	s := &Service{Bench: b}
	status, err := s.StartCommand(ctx, &pb.StartCommandRequest{SessionId: e.ID, Title: "pwd", Argv: []string{"/bin/sh", "-c", "pwd -P"}, Cwd: repo})
	if err != nil {
		t.Fatal("the person's command was refused:", err)
	}
	resolved, err := filepath.EvalSymlinks(repo)
	if err != nil {
		t.Fatal(err)
	}
	if status.Cwd != resolved {
		t.Fatalf("status folder %q, want %q", status.Cwd, resolved)
	}
	command, err := b.Commands.Get(status.Id)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = command.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	output, _, err := command.Read(ctx, 0, 4096)
	if err != nil {
		t.Fatal(err)
	}
	if strings.TrimSpace(string(output)) != resolved {
		t.Fatalf("ran in %q, want %q", output, resolved)
	}
	// No folder: the conversation's own.
	if status, err = s.StartCommand(ctx, &pb.StartCommandRequest{SessionId: e.ID, Title: "root", Argv: []string{"/bin/sh", "-c", "true"}}); err != nil || status.Cwd != workspace {
		t.Fatalf("default folder %q (%v), want %q", status.GetCwd(), err, workspace)
	}
	for name, cwd := range map[string]string{"outside": outside, "parent": filepath.Join(workspace, ".."), "symlink out": filepath.Join(workspace, "escape"), "relative": "api", "missing": filepath.Join(workspace, "nope")} {
		if _, err = s.StartCommand(ctx, &pb.StartCommandRequest{SessionId: e.ID, Title: name, Argv: []string{"/bin/sh", "-c", "true"}, Cwd: cwd}); err == nil {
			t.Fatalf("%s folder %q accepted", name, cwd)
		}
	}
}
