package transport

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// interrupted waits for an Interrupt to reach the agent, skipping messages.
func interrupted(t *testing.T, fake *agenttest.Adapter) {
	t.Helper()
	ctx := timeout(t)
	for {
		select {
		case op := <-fake.Session.Applied:
			if op.Kind == agent.Interrupt {
				return
			}
		case <-ctx.Done():
			t.Fatal("the reviewer's turn was never interrupted")
		}
	}
}

// nothingApplied checks no message with want reaches the agent for a while.
func nothingApplied(t *testing.T, fake *agenttest.Adapter, want string) {
	t.Helper()
	deadline := time.After(400 * time.Millisecond)
	for {
		select {
		case op := <-fake.Session.Applied:
			if strings.Contains(op.Text, want) {
				t.Fatalf("%q reached the agent: %s", want, op.Text)
			}
		case <-deadline:
			return
		}
	}
}

// Cancel mid-round 2: the reviewer's turn is interrupted and its session
// ended, the loop stops there (the author's next fix starts no round 3), the
// findings stay in the tray, and the review says it was cancelled, by whom
// and when, on the review and in the author's conversation. Only the owner
// may cancel.
func TestCancelReviewMidRoundStopsTheLoopAndKeepsTheFindings(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1, Provider: "codex"})
	if err != nil {
		t.Fatal(err)
	}
	applied(t, f.codex, "What to review: Turn 1")
	reviewerSession := started.Session.Id
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))

	// Round 1: a blocker, sent to the author, who fixes it: round 2 starts.
	filed, err := f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: reviewerSession, File: "api/handler.go", StartLine: 4, Severity: "blocker", Title: "Logged, not returned"})
	if err != nil {
		t.Fatal(err)
	}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Phase == "triage" })
	if _, err = f.client.SetReviewDisposition(f.owner, &pb.SetReviewDispositionRequest{CollectionId: f.mine.Id, Ids: []string{filed.Finding.Id}, Disposition: "sent", SessionId: f.conversation.ID, SessionTitle: "Fix the parser"}); err != nil {
		t.Fatal(err)
	}
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Phase == "fixing" })
	worktree := f.mine.Members[0].Path
	turn(t, f.service, f.claude, f.conversation.ID, "op-fix", func() {
		if err := os.WriteFile(filepath.Join(worktree, "handler.go"), []byte("package api\n\nfunc Handle() error {\n\treturn nil\n}\n"), 0o600); err != nil {
			t.Fatal(err)
		}
	})
	applied(t, f.codex, "[Lazybox] Round 2 of 3.")
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Round == 2 && r.Phase == "reviewing" && r.State == "running" })
	if _, err = f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: reviewerSession, File: "api/handler.go", StartLine: 3, Severity: "minor", Title: "Name the error"}); err != nil {
		t.Fatal(err)
	}

	// Only the person cancels.
	if _, err = f.client.CancelReview(reviewer, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: started.Run.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("the reviewer cancelled its own review: %v", err)
	}
	if _, err = f.client.CancelReview(bearer(context.Background(), f.agentToken), &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: started.Run.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("the author cancelled the review: %v", err)
	}

	// Cancel, naming only the review: the Runner finds its workspace.
	tray, err := f.client.CancelReview(f.owner, &pb.CancelReviewRequest{ReviewId: started.Run.Id})
	if err != nil {
		t.Fatal(err)
	}
	interrupted(t, f.codex)
	if !f.codex.Session.WasClosed() {
		t.Fatal("the reviewer's session was not ended")
	}
	run := tray.Runs[0]
	if run.Id != started.Run.Id || run.State != "cancelled" || run.StopReason != "cancelled" || run.Phase != "done" || run.StoppedBy != "You" || run.StoppedAtUnix == 0 || run.Round != 2 {
		t.Fatalf("cancelled: %+v", run)
	}
	if run.Detail != "You cancelled the review in round 2; 2 findings kept." || run.Blockers != 1 || run.Minors != 1 || run.Timeline[len(run.Timeline)-1].Kind != "cancelled" {
		t.Fatalf("cancelled review: %q %+v", run.Detail, run)
	}
	if c := commentByID(t, tray, filed.Finding.Id); c.Disposition != "sent" {
		t.Fatalf("finding kept as it was: %+v", c)
	}
	// The conversation is kept: its session ended, it can be read and continued.
	session, err := f.client.GetSession(f.owner, &pb.SessionRequest{SessionId: reviewerSession})
	if err != nil || !session.Session.Closed {
		t.Fatalf("reviewer conversation: %+v %v", session, err)
	}
	// The author's conversation records it, with who and when.
	finished := events(t, f.bench, f.conversation.ID, "review_finished")["review_finished"]
	if finished.Status != "cancelled" || finished.Text != "Review cancelled · 2 findings kept" || !strings.Contains(string(finished.Data), `"stoppedBy":"You"`) || !strings.Contains(string(finished.Data), `"stoppedAt":`) {
		t.Fatalf("author's journal: %+v %s", finished, finished.Data)
	}

	// The author fixes again: no round 3 starts, nothing reaches the reviewer.
	turn(t, f.service, f.claude, f.conversation.ID, "op-fix-2", func() {
		if err := os.WriteFile(filepath.Join(worktree, "other.go"), []byte("package api\n"), 0o600); err != nil {
			t.Fatal(err)
		}
	})
	waitTurns(t, f.service, f.conversation.ID, 3)
	nothingApplied(t, f.codex, "Round 3")
	again := runUntil(t, f, func(r *pb.ReviewRun) bool { return true })
	if again.Round != 2 || again.State != "cancelled" || len(again.Rounds) != 2 {
		t.Fatalf("after the author's next fix: %+v", again)
	}
	// Cancelling again is harmless; a review that is over cannot be cancelled.
	if _, err = f.client.CancelReview(f.owner, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: started.Run.Id}); err != nil {
		t.Fatalf("cancelled twice: %v", err)
	}
	if _, err = f.client.CancelReview(f.owner, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: "rv-missing"}); status.Code(err) != codes.NotFound {
		t.Fatalf("an unknown review: %v", err)
	}
}

// Ending the reviewer by hand — End session, Stop while it reviews, or
// Delete — moves its review out of "reviewing": it reads stopped, never
// stuck on a reviewer that will not report.
func TestEndingTheReviewerByHandStopsItsReview(t *testing.T) {
	f := newReviewFixture(t)
	start := func() *pb.StartReviewResponse {
		t.Helper()
		started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1, Provider: "codex"})
		if err != nil {
			t.Fatal(err)
		}
		applied(t, f.codex, "What to review: Turn 1")
		return started
	}
	stoppedAs := func(id, detail string) {
		t.Helper()
		run := runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Id == id && r.State != "running" })
		if run.State != "stopped" || run.StopReason != "ended" || run.Phase != "done" || run.StoppedBy != "You" || run.Detail != detail {
			t.Fatalf("stopped by hand: %+v", run)
		}
	}

	// End session.
	first := start()
	if _, err := f.client.StopSessionResources(f.owner, &pb.SessionRequest{SessionId: first.Session.Id}); err != nil {
		t.Fatal(err)
	}
	stoppedAs(first.Run.Id, "You ended Codex's conversation; the review stopped.")
	finished := events(t, f.bench, f.conversation.ID, "review_finished")["review_finished"]
	if finished.Status != "stopped" || finished.Text != "Review stopped · 0 findings kept" {
		t.Fatalf("author's journal: %+v", finished)
	}

	// Stop: the reviewer's turn interrupted mid-review; the turn's end is not a round's.
	second := start()
	if _, err := f.client.Apply(f.owner, &pb.ApplyRequest{SessionId: second.Session.Id, OperationId: "stop-1", Operation: &pb.ApplyRequest_Interrupt{Interrupt: &pb.Interrupt{}}}); err != nil {
		t.Fatal(err)
	}
	interrupted(t, f.codex)
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
	stoppedAs(second.Run.Id, "You stopped Codex's turn; the review stopped.")
	// The turn's end is followed off the event path, after its checkpoint.
	waitTurns(t, f.service, second.Session.Id, 1)
	for deadline := time.Now().Add(500 * time.Millisecond); time.Now().Before(deadline); time.Sleep(25 * time.Millisecond) {
		stoppedAs(second.Run.Id, "You stopped Codex's turn; the review stopped.")
	}

	// Delete.
	third := start()
	if _, err := f.client.DeleteConversation(f.owner, &pb.DeleteConversationRequest{SessionId: third.Session.Id, StopSessions: true}); err != nil {
		t.Fatal(err)
	}
	stoppedAs(third.Run.Id, "You deleted Codex's conversation; the review stopped.")
}
