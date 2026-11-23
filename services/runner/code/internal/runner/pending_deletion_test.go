package runner

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

func TestPendingDeletionSurvivesRestartAndCanBeUndone(t *testing.T) {
	ctx := context.Background()
	cfg := Config{State: t.TempDir(), WorktreeRoot: t.TempDir()}
	b, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	workspace, err := b.Collections.Create(ctx, "pending-deletion", collections.Spec{Name: "Undoable workspace"})
	if err != nil {
		b.Close()
		t.Fatal(err)
	}
	e, err := b.OpenCollection("codex-default", workspace.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		b.Close()
		t.Fatal(err)
	}
	file := filepath.Join(e.Cwd, "keep.txt")
	if err := os.WriteFile(file, []byte("keep this until deletion"), 0600); err != nil {
		b.Close()
		t.Fatal(err)
	}
	workspaceRequest := PendingDeletion{WorkspaceID: workspace.ID, Title: workspace.Name, Discard: true, Stop: true, KeepArtifacts: true}
	workspaceDeletion, err := b.ScheduleDeletion(ctx, workspaceRequest)
	if err != nil {
		b.Close()
		t.Fatal(err)
	}
	conversationRequest := PendingDeletion{SessionID: e.ID, Stop: true}
	conversationDeletion, err := b.ScheduleDeletion(ctx, conversationRequest)
	if err != nil {
		b.Close()
		t.Fatal(err)
	}
	if err := b.Close(); err != nil {
		t.Fatal(err)
	}

	b, err = New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if entries := b.PendingDeletions(); len(entries) != 2 {
		t.Fatalf("lost entries on restart: %+v", entries)
	}
	if _, err := os.Stat(file); err != nil {
		t.Fatal(err)
	}
	restored, err := b.ScheduleDeletion(ctx, workspaceRequest)
	if err != nil || !restored.KeepArtifacts {
		t.Fatal("lost artifact retention choice", restored, err)
	}
	retry, err := b.ScheduleDeletion(ctx, conversationRequest)
	if err != nil || !retry.Expires.Equal(conversationDeletion.Expires) {
		t.Fatalf("retry moved the deadline: %+v, %v", retry, err)
	}
	for _, id := range []string{workspaceDeletion.ID, conversationDeletion.ID} {
		if err := b.UndoDeletion(id); err != nil {
			t.Fatal(err)
		}
	}
	b.finishPendingDeletions(ctx, time.Now().Add(deletionGrace))
	if _, err := b.Get(e.ID); err != nil {
		t.Fatal("undo lost the conversation", err)
	}
	if _, err := os.Stat(file); err != nil {
		t.Fatal("undo lost the workspace file", err)
	}

	conversationDeletion, err = b.ScheduleDeletion(ctx, conversationRequest)
	if err != nil {
		t.Fatal(err)
	}
	b.finishPendingDeletions(ctx, conversationDeletion.Expires.Add(-time.Millisecond))
	if _, err := b.Get(e.ID); err != nil {
		t.Fatal("purged before deadline", err)
	}
	b.finishPendingDeletions(ctx, conversationDeletion.Expires)
	if _, err := b.Get(e.ID); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("conversation was not purged at the deadline", err, b.PendingDeletions())
	}
	if err := b.UndoDeletion(conversationDeletion.ID); err == nil {
		t.Fatal("undo accepted after purge")
	}

	workspaceDeletion, err = b.ScheduleDeletion(ctx, workspaceRequest)
	if err != nil {
		t.Fatal(err)
	}
	b.finishPendingDeletions(ctx, workspaceDeletion.Expires)
	if _, err := os.Stat(file); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("workspace was not purged at the deadline", err, b.PendingDeletions())
	}
	if entries := b.PendingDeletions(); len(entries) != 0 {
		t.Fatalf("purged entries remain visible: %+v", entries)
	}
}

func TestExpiredWorkspaceDeletionPurgesEntireRootAndGitRegistration(t *testing.T) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	if err := os.WriteFile(filepath.Join(source, ".gitignore"), []byte(".terraform/\nnode_modules/\n"), 0600); err != nil {
		t.Fatal(err)
	}
	gitIn(t, source, "add", ".gitignore")
	gitIn(t, source, "commit", "-m", "Ignore fixture build output")
	b := freshBench(t, collections.Repository{ID: "fixture", Name: "fixture", Path: source})
	workspace, err := b.Collections.Create(ctx, "trash-purge", collections.Spec{
		Name: "Trash purge", Members: []collections.MemberSpec{{Name: "fixture", RepositoryID: "fixture", BaseRef: "main"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	member := workspace.Members[0]
	for _, file := range []string{
		filepath.Join(member.Path, ".terraform", "providers", "fixture"),
		filepath.Join(member.Path, "node_modules", "fixture", "index.js"),
	} {
		if err := os.MkdirAll(filepath.Dir(file), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(file, []byte("generated"), 0600); err != nil {
			t.Fatal(err)
		}
	}
	request := PendingDeletion{WorkspaceID: workspace.ID, Title: workspace.Name}
	if _, err := b.ScheduleDeletion(ctx, request); err == nil {
		t.Fatal("ignored build output did not require discard confirmation")
	}
	outside := filepath.Join(workspace.Root, "outside-worktree")
	if err := os.WriteFile(outside, []byte("workspace file"), 0600); err != nil {
		t.Fatal(err)
	}
	request.Discard = true
	entry, err := b.ScheduleDeletion(ctx, request)
	if err != nil {
		t.Fatal(err)
	}
	b.finishPendingDeletions(ctx, entry.Expires.Add(-time.Millisecond))
	if _, err := os.Stat(outside); err != nil {
		t.Fatal("workspace data lost during Undo window", err)
	}
	b.finishPendingDeletions(ctx, entry.Expires)
	if _, err := os.Lstat(workspace.Root); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("workspace root remains: %v; pending: %+v", err, b.PendingDeletions())
	}
	if strings.Contains(gitIn(t, source, "worktree", "list", "--porcelain"), member.Path) {
		t.Fatal("Git registration remains")
	}
	if got := gitIn(t, source, "rev-parse", "refs/heads/"+member.Branch); got != member.BaseCommit {
		t.Fatal("workspace branch was changed")
	}
}

func TestFailedDeletionCanBeDismissedAfterDeadline(t *testing.T) {
	path := filepath.Join(t.TempDir(), "pending-deletions.json")
	store, err := openPendingDeletions(path)
	if err != nil {
		t.Fatal(err)
	}
	failed := PendingDeletion{ID: "failed", Expires: time.Now().Add(-time.Minute), Failure: "cleanup failed"}
	expired := PendingDeletion{ID: "expired", Expires: failed.Expires}
	store.entries[failed.ID], store.entries[expired.ID] = failed, expired
	b := &Bench{pendingDeletions: store}
	if err := b.UndoDeletion(expired.ID); err == nil {
		t.Fatal("undo accepted after deadline while cleanup could still run")
	}
	if err := b.UndoDeletion(failed.ID); err != nil {
		t.Fatal(err)
	}
	reopened, err := openPendingDeletions(path)
	if err != nil {
		t.Fatal(err)
	}
	if _, found := reopened.entries[failed.ID]; found || len(reopened.entries) != 1 {
		t.Fatalf("unexpected pending deletions after reopening: %+v", reopened.entries)
	}
}
