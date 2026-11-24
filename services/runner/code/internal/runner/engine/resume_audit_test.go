package engine

import (
	"context"
	"errors"
	"fmt"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

// Resume re-attaches a conversation in place: the engine it replaces is
// detached and its journal released. A reader of the old engine is told to
// reconnect (ErrDetached), never io.EOF, which tells it the conversation is
// over and makes it stop following.
func TestReadersOfADetachedEngineAreToldToReconnect(t *testing.T) {
	e, native, _, _ := durableEngine(t)
	native.Events <- agent.Event{Type: "session_ready", Cursor: 1}
	await(t, e, "session_ready")
	ctx := contextFor(t)
	end := e.journal.Count()
	read := make(chan error, 1)
	go func() {
		_, err := e.Events(ctx, end) // waits for the next record
		read <- err
	}()
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-read:
		if !errors.Is(err, ErrDetached) || errors.Is(err, io.EOF) {
			t.Fatalf("a waiting reader of a detached engine got %v, want ErrDetached", err)
		}
	case <-ctx.Done():
		t.Fatal("the waiting reader never ended")
	}
	// Released under a reader replaying it: the same.
	if err := e.journal.Dispose(); err != nil {
		t.Fatal(err)
	}
	if _, err := e.Events(ctx, 0); !errors.Is(err, ErrDetached) {
		t.Fatalf("a replaying reader of a released journal got %v, want ErrDetached", err)
	}
}

// Every operation used to stay in the engine's table, refilled from the whole
// journal on adoption, and at 4096 every Apply failed: messages, stops and
// approvals alike. Identities are a window of the latest operations.
func TestALongConversationNeverRunsOutOfOperations(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	e.mu.Lock()
	for i := range 4096 {
		e.ops[fmt.Sprintf("old-%d", i)] = accepted{}
	}
	e.mu.Unlock()
	if _, err := e.Apply(ctx, agent.Operation{ID: "next", Kind: agent.Message, Text: "go on"}); err != nil {
		t.Fatalf("operation 4097 was refused: %v", err)
	}
	_ = applied(t, native)
}

// 64 messages waiting behind a turn refuse a 65th, never the approval the
// running turn waits for.
func TestAFullQueueStillTakesAnswers(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started"}
	native.Events <- agent.Event{Type: "approval", RequestID: "r1"}
	await(t, e, "approval")
	for i := range 64 {
		if _, err := e.Apply(ctx, agent.Operation{ID: fmt.Sprintf("queued-%d", i), Kind: agent.Message, Text: "later"}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := e.Apply(ctx, agent.Operation{ID: "one-more", Kind: agent.Message, Text: "later"}); err == nil {
		t.Fatal("a 65th queued message was taken")
	}
	if _, err := e.Apply(ctx, agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: "r1", Decision: "accept"}); err != nil {
		t.Fatalf("a full queue refused the approval the turn waits for: %v", err)
	}
}

func TestOperationIdentitiesKeepTheLatestAndWhatIsQueued(t *testing.T) {
	previous := opWindow
	opWindow = 16
	t.Cleanup(func() { opWindow = previous })
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	await(t, e, "turn_started")
	// Queued behind the turn: kept however many operations follow.
	if _, err := e.Apply(ctx, agent.Operation{ID: "queued", Kind: agent.Message, Text: "then this"}); err != nil {
		t.Fatal(err)
	}
	for i := range 40 {
		if _, err := e.Apply(ctx, agent.Operation{ID: fmt.Sprintf("list-%d", i), Kind: agent.ListMcpServers}); err != nil {
			t.Fatal(err)
		}
		_ = applied(t, native)
	}
	e.mu.Lock()
	size, queuedKept := len(e.ops), e.ops["queued"].receipt.Delivery != ""
	e.mu.Unlock()
	if size > opWindow+opWindow/8 || !queuedKept {
		t.Fatalf("operation table: %d entries (window %d), queued kept %v", size, opWindow, queuedKept)
	}
	// A retry of a recent operation is still recognised, not applied again.
	if _, err := e.Apply(ctx, agent.Operation{ID: "list-39", Kind: agent.ListMcpServers}); err != nil {
		t.Fatal(err)
	}
	select {
	case op := <-native.Applied:
		t.Fatalf("a retried operation was applied again: %+v", op)
	case <-time.After(50 * time.Millisecond):
	}
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, _, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	next.mu.Lock()
	size = len(next.ops)
	next.mu.Unlock()
	if size > opWindow+opWindow/8 {
		t.Fatalf("adoption refilled %d operations (window %d)", size, opWindow)
	}
}

// A Runner that died between journaling a turn's end and saving the
// snapshot left one that said busy, with the turn's approval pending. The
// adopting Runner reads both from the journal: idle, nothing pending.
func TestAdoptionReadsBusyAndPendingFromTheJournal(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 2}
	await(t, e, "approval")
	awaitReceipt(t, e, "first")
	stale, _ := st.saved()
	if !stale.Busy || len(stale.Pending) != 1 {
		t.Fatalf("setup: %+v", stale)
	}
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 3}
	await(t, e, "turn_ended")
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	st.mu.Lock()
	st.snapshot = stale // the save after the turn's end never happened
	st.mu.Unlock()
	next, _, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if s := next.Snapshot(); s.Busy || len(s.Pending) != 0 {
		t.Fatalf("adopted a finished turn as busy=%v pending=%v", s.Busy, s.Pending)
	}
	// And the next message is delivered, not queued behind a turn that ended.
	if _, err = next.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "go on"}); err != nil {
		t.Fatal(err)
	}
}

// A running turn whose snapshot lagged (saved before turn_started) is busy.
func TestAdoptionKeepsATurnTheJournalSaysRuns(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	native.Events <- agent.Event{Type: "session_ready", Cursor: 1}
	await(t, e, "session_ready")
	stale, _ := st.saved()
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 2}
	native.Events <- agent.Event{Type: "question", RequestID: "q1", Cursor: 3}
	await(t, e, "question")
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	st.mu.Lock()
	st.snapshot = stale
	st.mu.Unlock()
	next, _, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if s := next.Snapshot(); !s.Busy || s.Pending["q1"].Type != "question" {
		t.Fatalf("adopted a running turn as busy=%v pending=%v", s.Busy, s.Pending)
	}
}

// An approval answered before a restart, whose answer the adapter gives the
// agent again, is not offered to the person a second time; the adapter's
// request_resolved clears it.
func TestAnAnsweredApprovalIsNotAnsweredTwiceAfterAdoption(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 1}
	await(t, e, "approval")
	if _, err := e.Apply(ctx, agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: "r1", Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = next.Apply(ctx, agent.Operation{ID: "again", Kind: agent.Approve, RequestID: "r1", Decision: "decline"}); err == nil {
		t.Fatal("an answered approval was answered again after adoption")
	}
	a.Session.Events <- agent.Event{Type: "request_resolved", RequestID: "r1", Status: "answered_after_restart", Cursor: 2}
	await(t, next, "request_resolved")
	if s := next.Snapshot(); len(s.Pending) != 0 {
		t.Fatalf("the re-answered approval still shows: %v", s.Pending)
	}
}

// The agent asks again (the adapter could not give the recorded answer):
// the person can answer it.
func TestARequestAskedAgainAfterAdoptionCanBeAnswered(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 1}
	await(t, e, "approval")
	if _, err := e.Apply(ctx, agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: "r1", Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	a.Session.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 1}
	eventually(t, "the request asked again is answerable", func() bool {
		_, err := next.Apply(ctx, agent.Operation{ID: "again", Kind: agent.Approve, RequestID: "r1", Decision: "accept"})
		return err == nil
	})
}

// An answer that did not reach the agent leaves the request answerable.
func TestAFailedAnswerCanBeGivenAgain(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	native.Events <- agent.Event{Type: "approval", RequestID: "r1"}
	await(t, e, "approval")
	released := make(chan struct{})
	close(released)
	native.Hold(agent.Approve, released, errors.New("could not write the reply"))
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Approve, RequestID: "r1", Decision: "accept"}); err == nil {
		t.Fatal("setup: the answer did not fail")
	}
	native.Hold(agent.Approve, released, nil)
	if _, err := e.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Approve, RequestID: "r1", Decision: "accept"}); err != nil {
		t.Fatalf("the request could not be answered again: %v", err)
	}
}

// A message the provider refused (a JSON-RPC error, a reply with no turn) is
// a turn's failure: the next message goes through. Only an unknown outcome
// holds the conversation.
func TestARefusedDeliveryIsATurnFailure(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	released := make(chan struct{})
	close(released)
	native.Hold(agent.Message, released, agent.Rejected(errors.New("turn/start: invalid params")))
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	if r := awaitReceipt(t, e, "first"); r.Receipt.Delivery != "failed" {
		t.Fatalf("receipt: %+v", r)
	}
	if s := e.Snapshot(); s.Fault != agent.DeliveryRejectedFault || s.Busy {
		t.Fatalf("after a refusal: fault %q busy %v", s.Fault, s.Busy)
	}
	native.Hold(agent.Message, released, nil)
	if _, err := e.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "try again"}); err != nil {
		t.Fatalf("the next message was refused: %v", err)
	}
	if op := applied(t, native); op.ID != "second" {
		t.Fatalf("delivered %q", op.ID)
	}
	if s := e.Snapshot(); s.Fault != "" {
		t.Fatalf("the refusal was not cleared: %q", s.Fault)
	}
}

func TestAnUnknownDeliveryOutcomeStillHoldsTheConversation(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	released := make(chan struct{})
	close(released)
	native.Hold(agent.Message, released, errors.New("connection reset"))
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	awaitReceipt(t, e, "first")
	if s := e.Snapshot(); !strings.HasPrefix(s.Fault, "provider delivery failed") {
		t.Fatalf("fault %q", s.Fault)
	}
}

// The stream from the agent was lost while a delivery was in progress: that
// fault (which Resume settles by re-attaching) stays, the delivery's failure
// does not replace it.
func TestAFailedDeliveryKeepsALostStreamsFault(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	release := make(chan struct{})
	native.Hold(agent.Message, release, errors.New("connection reset"))
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	eventually(t, "the message is being delivered", func() bool {
		s := e.Snapshot()
		return s.Busy && s.Queued == 0
	})
	loseStream(t, e, native, "host connection lost")
	if s := e.Snapshot(); !agent.TransportFault(s.Fault) {
		t.Fatalf("setup: fault %q", s.Fault)
	}
	close(release)
	awaitReceipt(t, e, "first")
	if s := e.Snapshot(); !agent.TransportFault(s.Fault) {
		t.Fatalf("the lost stream's fault was replaced by %q", s.Fault)
	}
}

// One failed Detach is not the answer forever: Resume reconnects through
// Detach, and a second try succeeds.
func TestAFailedDetachCanBeRetried(t *testing.T) {
	e, native, _, _ := durableEngine(t)
	fail := true
	native.BeforeDetach = func() error {
		if fail {
			return errors.New("host did not answer")
		}
		return nil
	}
	if err := e.Detach(); err == nil {
		t.Fatal("setup: detach did not fail")
	}
	if !e.Streaming() {
		t.Fatal("a failed detach stopped reading the agent")
	}
	fail = false
	if err := e.Detach(); err != nil {
		t.Fatalf("detach could not be retried: %v", err)
	}
	if !native.Detached {
		t.Fatal("the retry never reached the provider")
	}
}

// The receive loop stops recording while the engine detaches. If the detach
// then fails, the agent runs with nobody reading it: the conversation says
// its stream is lost (Resume re-attaches it), never stays silently stalled.
func TestADetachThatFailsAfterTheLoopStoppedIsALostStream(t *testing.T) {
	e, native, _, _ := durableEngine(t)
	native.BeforeDetach = func() error {
		native.Events <- agent.Event{Type: "message_delta", Text: "late", Cursor: 9}
		for end := time.Now().Add(5 * time.Second); e.Streaming(); time.Sleep(5 * time.Millisecond) {
			if time.Now().After(end) {
				return errors.New("the loop kept reading")
			}
		}
		return errors.New("host did not answer")
	}
	if err := e.Detach(); err == nil {
		t.Fatal("setup: detach did not fail")
	}
	if s := e.Snapshot(); !agent.TransportFault(s.Fault) {
		t.Fatalf("a supervisor that stopped reading says nothing: fault %q", s.Fault)
	}
}

// Adoption reads nothing until Start: the Runner installs its turn hooks
// first, so a turn the agent ends at once still gets its checkpoint.
func TestAdoptedTurnEndedAtOnceReachesTheTurnHooks(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	await(t, e, "turn_started")
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a := adoptStopped(t, path, st)
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 2}
	ended := make(chan struct{})
	next.SetTurnHooks(TurnHooks{After: func(string, time.Time) { close(ended) }})
	next.Start()
	select {
	case <-ended:
	case <-ctx.Done():
		t.Fatal("the adopted turn's end never reached the hooks")
	}
}

func adoptStopped(t *testing.T, path string, st *store) (*Engine, *agenttest.Adapter) {
	t.Helper()
	spool, err := journal.Open(path, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	snapshot, record := st.saved()
	a := &agenttest.Adapter{Durable: record.Provider}
	e, err := AdoptStopped(context.Background(), a, agent.Open{ID: "execution", Profile: "pinned", Host: &agent.Hosting{Root: "/hosts"}}, spool, snapshot, record, st.capture, st.attach)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = e.Close(); _ = spool.Dispose() })
	return e, a
}
