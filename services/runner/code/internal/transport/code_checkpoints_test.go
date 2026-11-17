package transport

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"
)

// turn sends a message, waits for the fake agent to receive it (the
// checkpoint before the turn is taken by then), lets work change files, and
// ends the turn.
func turn(t *testing.T, s *Service, a *agenttest.Adapter, session, operation string, work func()) agent.Operation {
	t.Helper()
	ctx := timeout(t)
	if _, err := s.Apply(ctx, &pb.ApplyRequest{SessionId: session, OperationId: operation, Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "Do " + operation}}}); err != nil {
		t.Fatal(err)
	}
	var applied agent.Operation
	select {
	case applied = <-a.Session.Applied:
	case <-ctx.Done():
		t.Fatal("message never reached the agent")
	}
	if work != nil {
		work()
	}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	return applied
}

func waitTurns(t *testing.T, s *Service, session string, ended int) *pb.TurnCheckpoints {
	t.Helper()
	ctx := timeout(t)
	for {
		list, err := s.ListTurnCheckpoints(ctx, &pb.TurnCheckpointsRequest{SessionId: session})
		if err != nil {
			t.Fatal(err)
		}
		done := 0
		for _, turn := range list.Turns {
			if turn.EndedAtUnixMillis != 0 {
				done++
			}
		}
		if done >= ended {
			return list
		}
		select {
		case <-ctx.Done():
			t.Fatalf("turns never ended: %v", list)
		case <-time.After(20 * time.Millisecond):
		}
	}
}

func TestTurnCheckpointsRevertAndUndoAcrossAWorkspace(t *testing.T) {
	s, a := collectionService(t)
	ctx := timeout(t)
	c := createTestCollection(t, s)
	api, next := c.Members[0].Path, c.Members[1].Path
	opened, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: c.Id})
	if err != nil {
		t.Fatal(err)
	}
	id := opened.Session.Id
	write := func(path, text string) {
		if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(text), 0600); err != nil {
			t.Fatal(err)
		}
	}
	write(filepath.Join(api, "notes.txt"), "the person's untracked notes\n")
	turn(t, s, a, id, "op-1", func() {
		write(filepath.Join(api, "parser.go"), "package api\n")
		write(filepath.Join(next, "web/app.ts"), "export {}\n")
	})
	list := waitTurns(t, s, id, 1)
	if len(list.Turns) != 1 || list.Turns[0].OperationId != "op-1" || list.Turns[0].Files != 2 || len(list.Turns[0].Before) != 2 {
		t.Fatalf("turns = %v", list)
	}
	changes, err := s.ListTurnChanges(ctx, &pb.TurnChangesRequest{SessionId: id, Turn: 1})
	if err != nil || changes.InProgress || len(changes.Repositories) != 2 || changes.Repositories[0].Files[0].Path != "parser.go" || changes.Repositories[1].Files[0].Path != "web/app.ts" {
		t.Fatalf("changes = %v %v", changes, err)
	}
	diff, err := s.ReadTurnDiff(ctx, &pb.TurnDiffRequest{SessionId: id, Turn: 1, Member: "api", Path: "parser.go"})
	if err != nil || !strings.Contains(diff.Text, "+package api") {
		t.Fatalf("diff = %v %v", diff, err)
	}

	// Mid-turn: the preview says why, and the revert is refused, files untouched.
	if _, err = s.Apply(ctx, &pb.ApplyRequest{SessionId: id, OperationId: "op-2", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "Do more"}}}); err != nil {
		t.Fatal(err)
	}
	<-a.Session.Applied
	write(filepath.Join(api, "more.go"), "package api // more\n")
	preview, err := s.PreviewRevertCode(ctx, &pb.RevertCodeRequest{SessionId: id, Turn: 1})
	if err != nil || !strings.Contains(preview.Blocked, "agent is working") || len(preview.Repositories[0].Files) != 2 {
		t.Fatalf("preview mid-turn = %v %v", preview, err)
	}
	if _, err = s.RevertCode(ctx, &pb.RevertCodeRequest{SessionId: id, Turn: 1}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("revert mid-turn = %v", err)
	}
	if _, err = os.Stat(filepath.Join(api, "parser.go")); err != nil {
		t.Fatal("a refused revert changed files")
	}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	waitTurns(t, s, id, 2)

	preview, err = s.PreviewRevertCode(ctx, &pb.RevertCodeRequest{SessionId: id, Turn: 1})
	if err != nil || preview.Blocked != "" {
		t.Fatalf("preview = %v %v", preview, err)
	}
	revert, err := s.RevertCode(ctx, &pb.RevertCodeRequest{SessionId: id, Turn: 1})
	if err != nil || revert.Kind != "revert" || revert.Files != 3 || len(revert.Problems) != 0 {
		t.Fatalf("revert = %v %v", revert, err)
	}
	for _, gone := range []string{filepath.Join(api, "parser.go"), filepath.Join(api, "more.go"), filepath.Join(next, "web")} {
		if _, err = os.Stat(gone); err == nil {
			t.Fatalf("%s survived the revert", gone)
		}
	}
	if data, _ := os.ReadFile(filepath.Join(api, "notes.txt")); string(data) != "the person's untracked notes\n" {
		t.Fatal("untracked file from before the turn lost")
	}
	// The conversation says what happened to the files.
	e, err := s.Bench.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for cursor := uint64(0); cursor < e.Retained(); cursor++ {
		record, err := e.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		if strings.Contains(string(record.Value), `"code_reverted"`) && strings.Contains(string(record.Value), "before turn 1") {
			found = true
		}
	}
	if !found {
		t.Fatal("revert not recorded in the conversation")
	}
	// The agent hears about it with the next message, once.
	list, _ = s.ListTurnCheckpoints(ctx, &pb.TurnCheckpointsRequest{SessionId: id})
	if !list.NotePending {
		t.Fatal("no note pending for the agent")
	}
	applied := turn(t, s, a, id, "op-3", nil)
	if !strings.Contains(applied.Context, "reverted the workspace's files to how they were before turn 1 (“Do op-1”)") {
		t.Fatalf("agent was not told: %q", applied.Context)
	}
	waitTurns(t, s, id, 3)
	if list, _ = s.ListTurnCheckpoints(ctx, &pb.TurnCheckpointsRequest{SessionId: id}); list.NotePending {
		t.Fatal("note still pending after delivery")
	}

	undo, err := s.UndoRevertCode(ctx, &pb.UndoRevertCodeRequest{SessionId: id, RevertId: revert.Id})
	if err != nil || undo.Kind != "undo" || undo.UndoOf != revert.Id {
		t.Fatalf("undo = %v %v", undo, err)
	}
	for _, back := range []string{filepath.Join(api, "parser.go"), filepath.Join(api, "more.go"), filepath.Join(next, "web/app.ts")} {
		if _, err = os.Stat(back); err != nil {
			t.Fatalf("%s not restored by undo", back)
		}
	}
	list, _ = s.ListTurnCheckpoints(ctx, &pb.TurnCheckpointsRequest{SessionId: id})
	if len(list.Reverts) != 2 || list.Reverts[0].UndoneBy != undo.Id {
		t.Fatalf("reverts = %v", list.Reverts)
	}
	if _, err = s.ListTurnChanges(ctx, &pb.TurnChangesRequest{SessionId: id, Turn: 99}); status.Code(err) != codes.NotFound {
		t.Fatalf("unknown turn = %v", err)
	}
	// Branch history is the person's: no commit was made on the branch.
	for _, member := range []string{api, next} {
		out, err := exec.Command("git", "-C", member, "rev-list", "--count", "HEAD").Output()
		if err != nil || strings.TrimSpace(string(out)) != "1" {
			t.Fatalf("branch history changed in %s: %s %v", member, out, err)
		}
	}

	// Deleting the conversation removes its checkpoint refs.
	if _, err = s.CloseSession(ctx, &pb.SessionRequest{SessionId: id}); err != nil {
		t.Fatal(err)
	}
	if _, err = s.DeleteConversation(ctx, &pb.DeleteConversationRequest{SessionId: id, StopSessions: true}); err != nil {
		t.Fatal(err)
	}
	if out, _ := exec.Command("git", "-C", api, "for-each-ref", "refs/lazybox/").Output(); len(out) != 0 {
		t.Fatalf("checkpoint refs left after deleting the conversation: %s", out)
	}
}

func TestCheckpointRPCsAreTheOwnersAlone(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	e, _, token := f.open(t)
	calls := map[string]proto.Message{
		pb.RunnerService_ListTurnCheckpoints_FullMethodName: &pb.TurnCheckpointsRequest{SessionId: e.ID},
		pb.RunnerService_ListTurnChanges_FullMethodName:     &pb.TurnChangesRequest{SessionId: e.ID, Turn: 1},
		pb.RunnerService_ReadTurnDiff_FullMethodName:        &pb.TurnDiffRequest{SessionId: e.ID, Turn: 1, Member: "m", Path: "p"},
		pb.RunnerService_PreviewRevertCode_FullMethodName:   &pb.RevertCodeRequest{SessionId: e.ID, Turn: 1},
		pb.RunnerService_RevertCode_FullMethodName:          &pb.RevertCodeRequest{SessionId: e.ID, Turn: 1},
		pb.RunnerService_UndoRevertCode_FullMethodName:      &pb.UndoRevertCodeRequest{SessionId: e.ID, RevertId: "r1"},
	}
	for method, request := range calls {
		if _, listed := agentRPCs[method]; listed {
			t.Errorf("%s is on the agent allowlist", method)
		}
		if _, listed := terminalRPCs[method]; listed {
			t.Errorf("%s is on the terminal allowlist", method)
		}
		// The agent's own session is not enough: reverting files is the person's decision.
		if err := f.conn.Invoke(bearer(ctx, token), method, request, &pb.CodeRevert{}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: agent credential got %v, want PermissionDenied", method, err)
		}
		err := f.conn.Invoke(bearer(ctx, ownerToken), method, request, &pb.CodeRevert{})
		if code := status.Code(err); code == codes.PermissionDenied || code == codes.Unauthenticated {
			t.Errorf("%s: owner got %v", method, err)
		}
	}
}

func TestTurnStartsAreCheckpointedOnlyForCodingConversations(t *testing.T) {
	s, a := collectionService(t)
	ctx := timeout(t)
	c := createTestCollection(t, s)
	opened, err := s.OpenAssistant(ctx, &pb.OpenAssistantRequest{ProfileId: "claude-default", Purpose: "help", CollectionId: c.Id})
	if err != nil {
		t.Fatal(err)
	}
	turn(t, s, a, opened.Session.Id, "ask", nil)
	if _, err = s.ListTurnCheckpoints(ctx, &pb.TurnCheckpointsRequest{SessionId: opened.Session.Id}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("assistant checkpoints = %v", err)
	}
	if out, _ := exec.Command("git", "-C", c.Members[0].Path, "for-each-ref", "refs/lazybox/").Output(); len(out) != 0 {
		t.Fatalf("an assistant turn was checkpointed: %s", out)
	}
}
