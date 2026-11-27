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
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func applied(t *testing.T, fake *agenttest.Adapter, want string) agent.Operation {
	t.Helper()
	ctx := timeout(t)
	for {
		select {
		case op := <-fake.Session.Applied:
			if strings.Contains(op.Text, want) {
				return op
			}
		case <-ctx.Done():
			t.Fatalf("no message with %q reached the agent", want)
		}
	}
}

func runUntil(t *testing.T, f *reviewFixture, check func(*pb.ReviewRun) bool) *pb.ReviewRun {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		tray, err := f.client.ListReviewComments(f.owner, &pb.ListReviewCommentsRequest{CollectionId: f.mine.Id})
		if err == nil && len(tray.Runs) > 0 && check(tray.Runs[0]) {
			return tray.Runs[0]
		}
		if time.Now().After(deadline) {
			t.Fatalf("the review never got there: %+v %v", tray, err)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

// A closed loop over two rounds: the blocker reaches the author as filed,
// the author's fixing turn starts a re-review of only that turn, the reviewer
// resolves the blocker, and the loop ends clean. On the way, the reviewer
// asks the author a question and gets the reply.
func TestReviewLoopTwoRoundsEndsClean(t *testing.T) {
	f := newReviewFixture(t)
	settings, err := f.client.GetRunnerSettings(f.owner, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	settings.Review.Loop = "ask"
	if _, err = f.client.SaveRunnerSettings(f.owner, settings); err != nil {
		t.Fatal(err)
	}
	ctx := timeout(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1, Provider: "codex"})
	if err != nil {
		t.Fatal(err)
	}
	if r := started.Run; r.Loop != "ask" || r.MaxRounds != 3 || r.Round != 1 || r.Phase != "reviewing" || len(r.Rounds) != 1 {
		t.Fatalf("loop at start: %+v", r)
	}
	applied(t, f.codex, "lazybox ask author")
	reviewer, author := bearer(ctx, agentToken(t, f.codex.Session.Open.Env)), bearer(ctx, f.agentToken)
	reviewerSession := started.Session.Id

	// The reviewer asks the author; the author's reply comes back.
	asked, err := f.client.AskAgent(reviewer, &pb.AskAgentRequest{SessionId: reviewerSession, To: f.conversation.ID, Question: "Should Handle ever log?"})
	if err != nil || asked.State != "pending" || asked.AskerLabel != "Codex" || asked.AnswererLabel != "Claude" {
		t.Fatal(asked, err)
	}
	question := applied(t, f.claude, "[Lazybox] Codex (reviewing your work) asks you:")
	if !strings.Contains(question.Text, "> Should Handle ever log?") || question.ID == "" {
		t.Fatalf("question: %+v", question)
	}
	// Each side refuses what would deadlock or flood.
	if _, err = f.client.AskAgent(author, &pb.AskAgentRequest{SessionId: f.conversation.ID, To: "reviewer", Question: "And you?"}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("a question back while one waits: %v", err)
	}
	if _, err = f.client.AskAgent(reviewer, &pb.AskAgentRequest{SessionId: reviewerSession, To: "author", Question: "Again?"}); status.Code(err) != codes.ResourceExhausted {
		t.Fatalf("a second question before the reply: %v", err)
	}
	f.claude.Session.Events <- agent.Event{Type: "message_delta", Text: "No: return the error."}
	f.claude.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	answer, err := f.client.WaitAgentAsk(reviewer, &pb.WaitAgentAskRequest{AskId: asked.Id, TimeoutSeconds: 5})
	if err != nil || answer.State != "answered" || answer.Answer != "No: return the error." {
		t.Fatal(answer, err)
	}
	if _, err = f.client.WaitAgentAsk(author, &pb.WaitAgentAskRequest{AskId: asked.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("another agent waited on the question: %v", err)
	}
	for _, session := range []string{reviewerSession, f.conversation.ID} {
		seen := events(t, f.bench, session, "review_exchange")
		if !strings.HasPrefix(seen["review_exchange"].Text, "Claude answered Codex: No: return the error.") {
			t.Fatalf("%s shows %q", session, seen["review_exchange"].Text)
		}
	}

	// Round 1: a blocker; the person accepts and sends it to the author.
	filed, err := f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: reviewerSession, File: "api/handler.go", StartLine: 4, Severity: "blocker", Title: "Logged, not returned"})
	if err != nil || filed.Finding.Round != 1 {
		t.Fatal(filed, err)
	}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Phase == "triage" })
	if _, err = f.client.SetReviewDisposition(f.owner, &pb.SetReviewDispositionRequest{CollectionId: f.mine.Id, Ids: []string{filed.Finding.Id}, Disposition: "open"}); err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.SetReviewDisposition(f.owner, &pb.SetReviewDispositionRequest{CollectionId: f.mine.Id, Ids: []string{filed.Finding.Id}, Disposition: "sent", SessionId: f.conversation.ID, SessionTitle: "Fix the parser"}); err != nil {
		t.Fatal(err)
	}
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Phase == "fixing" })

	// The author fixes it: its turn changed files, so round 2 starts on that turn alone.
	worktree := f.mine.Members[0].Path
	turn(t, f.service, f.claude, f.conversation.ID, "op-fix", func() {
		if err := os.WriteFile(filepath.Join(worktree, "handler.go"), []byte("package api\n\nfunc Handle() error {\n\treturn errors.New(\"x\")\n}\n"), 0o600); err != nil {
			t.Fatal(err)
		}
	})
	rereview := applied(t, f.codex, "[Lazybox] Round 2 of 3.")
	for _, want := range []string{"Claude changed files in turn 3", "git -C ", filed.Finding.Id + " [blocker] api/handler.go:4 — Logged, not returned", "lazybox review resolve <id>", "lazybox review reopen <id>"} {
		if !strings.Contains(rereview.Text, want) {
			t.Errorf("re-review lacks %q:\n%s", want, rereview.Text)
		}
	}
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Round == 2 && r.Phase == "reviewing" && r.State == "running" })

	// Only the reviewer settles its findings.
	if _, err = f.client.SetReviewFindingState(author, &pb.SetReviewFindingStateRequest{SessionId: f.conversation.ID, FindingId: filed.Finding.Id, State: "resolved"}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("the author resolved a finding: %v", err)
	}
	if _, err = f.client.SetReviewFindingState(reviewer, &pb.SetReviewFindingStateRequest{SessionId: reviewerSession, FindingId: filed.Finding.Id, State: "resolved", Note: "returns the error"}); err != nil {
		t.Fatal(err)
	}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	done := runUntil(t, f, func(r *pb.ReviewRun) bool { return r.StopReason != "" })
	if done.StopReason != "clean" || done.Phase != "done" || len(done.Rounds) != 2 || done.Rounds[1].Resolved != 1 || done.Rounds[1].Blockers != 0 || done.Blockers != 0 {
		t.Fatalf("clean: %+v", done)
	}
	var kinds []string
	for _, e := range done.Timeline {
		kinds = append(kinds, e.Kind)
	}
	if got := strings.Join(kinds, " "); !strings.Contains(got, "asked answered filed round_ended triage sent fixing round_started resolved round_ended stopped") {
		t.Fatalf("timeline: %s", got)
	}
}

// Auto: the reviewer's findings go to the author on their own, exactly as
// the tray would send them, and are marked sent; a blocker the author does
// not fix stops the loop at its limit and tells the person.
func TestReviewLoopAutoSendsFindingsAndEscalatesAtTheLimit(t *testing.T) {
	f := newReviewFixture(t)
	settings, err := f.client.GetRunnerSettings(f.owner, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	settings.Review.Loop, settings.Review.MaxRounds = "auto", 2
	if settings, err = f.client.SaveRunnerSettings(f.owner, settings); err != nil || settings.Review.Loop != "auto" || settings.Review.MaxRounds != 2 {
		t.Fatal(settings, err)
	}
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1})
	if err != nil {
		t.Fatal(err)
	}
	applied(t, f.codex, "What to review: Turn 1")
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	filed, err := f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: started.Session.Id, File: "api/handler.go", StartLine: 4, Severity: "blocker", Title: "Logged, not returned", Body: "Return it."})
	if err != nil {
		t.Fatal(err)
	}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	sent := applied(t, f.claude, "Please address this review comment")
	if !strings.Contains(sent.Text, "**Blocker: Logged, not returned** (finding "+filed.Finding.Id+", from Codex)") || !strings.Contains(sent.Text, "lazybox ask reviewer") {
		t.Fatalf("findings as filed:\n%s", sent.Text)
	}
	run := runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Phase == "fixing" })
	tray, _ := f.client.ListReviewComments(f.owner, &pb.ListReviewCommentsRequest{CollectionId: f.mine.Id})
	if c := commentByID(t, tray, filed.Finding.Id); c.Disposition != "sent" || c.SentTo != f.conversation.ID {
		t.Fatalf("auto-sent finding: %+v", c)
	}
	// The author's turn answers the findings message and changes a file.
	f.claude.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	worktree := f.mine.Members[0].Path
	turn(t, f.service, f.claude, f.conversation.ID, "op-try", func() {
		_ = os.WriteFile(filepath.Join(worktree, "other.go"), []byte("package api\n"), 0o600)
	})
	applied(t, f.codex, "[Lazybox] Round 2 of 2.")
	// The reviewer says nothing about the blocker: silence is still open.
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	run = runUntil(t, f, func(r *pb.ReviewRun) bool { return r.StopReason != "" })
	if run.StopReason != "max_rounds" || !strings.Contains(run.Detail, "1 blocker still open after 2 rounds") {
		t.Fatalf("max rounds: %+v", run)
	}
	seen := events(t, f.bench, f.conversation.ID, "review_escalated")
	if !strings.Contains(seen["review_escalated"].Text, "decide how to proceed") {
		t.Fatalf("escalation: %+v", seen["review_escalated"])
	}

	// The person's controls.
	if _, err = f.client.UpdateReviewRun(f.owner, &pb.UpdateReviewRunRequest{CollectionId: f.mine.Id, ReviewId: run.Id, Action: "pause"}); err == nil {
		t.Fatal("an ended loop was paused")
	}
	if _, err = f.client.UpdateReviewRun(bearer(context.Background(), f.agentToken), &pb.UpdateReviewRunRequest{CollectionId: f.mine.Id, ReviewId: run.Id, Action: "stop"}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("an agent controlled the loop: %v", err)
	}
}

func TestDefaultReviewDeliversMinorFindingWithoutAnotherRound(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1})
	if err != nil {
		t.Fatal(err)
	}
	if started.Run.Loop != "auto" {
		t.Fatal(started.Run.Loop)
	}
	applied(t, f.codex, "What to review")
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	filed, err := f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: started.Session.Id, File: "api/handler.go", StartLine: 4, Severity: "minor", Title: "Clarify the error", Body: "Include the input name."})
	if err != nil {
		t.Fatal(err)
	}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	op := applied(t, f.claude, "Please address this review comment")
	if !strings.Contains(op.Text, filed.Finding.Id) || !strings.Contains(op.Text, "Include the input name.") {
		t.Fatal(op.Text)
	}
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Round == 1 && r.Phase == "done" && r.StopReason == "clean" })
}

func TestAskSelectsOneActiveReview(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.bench.Reviews.Update(f.mine.Id, func(rv *reviews.Review) error {
		second := rv.Runs[0]
		second.ID = "rv-another"
		rv.Runs = append(rv.Runs, second)
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	ctx := bearer(timeout(t), f.agentToken)
	req := &pb.AskAgentRequest{SessionId: f.conversation.ID, To: "reviewer", Question: "Which file?"}
	if _, err := f.client.AskAgent(ctx, req); err == nil || !strings.Contains(err.Error(), "multiple active reviews") {
		t.Fatalf("ambiguous role: %v", err)
	}
	req.To = started.Run.Id
	if _, err := f.client.AskAgent(ctx, req); err != nil {
		t.Fatal(err)
	}
}

// A Runner may start with an answer on disk but no waiter or notification timer.
func TestPersistedReviewAnswerReachesAsker(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.bench.Reviews.UpdateRun(f.mine.Id, started.Run.Id, func(r *reviews.Run) error {
		r.Asks = append(r.Asks, reviews.Ask{ID: "ask-saved", FromSession: f.conversation.ID, Question: "Which file?", Answer: "handler.go", State: reviews.AskAnswered, AnsweredAt: time.Now().Add(-time.Minute).Unix()})
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	op := applied(t, f.claude, "answered your question")
	if op.ID != "review-answer-saved" || op.Origin == nil || op.Origin.Kind != agent.OriginAgentAnswer {
		t.Fatalf("recovered answer: %+v", op)
	}
}

func TestDefaultReviewReturnsCleanSummaryToAuthor(t *testing.T) {
	f := newReviewFixture(t)
	_, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1})
	if err != nil {
		t.Fatal(err)
	}
	applied(t, f.codex, "What to review")
	f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "I checked the handler."}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	summary := applied(t, f.claude, "Review complete · no open findings")
	if !strings.Contains(summary.Text, "I checked the handler.") {
		t.Fatal(summary.Text)
	}
}
