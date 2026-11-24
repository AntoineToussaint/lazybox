package transport

import (
	"context"
	"io"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Editing a message replaces part of the conversation and stops its agent:
// it is the owner's alone. The agent cannot rewind itself, nor can a paired
// phone; the owner's request reaches the Runner (and is refused there for a
// cursor that is no message).
func TestEditingAMessageIsTheOwnersAlone(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	fake := &agenttest.Adapter{}
	f.bench.Adapters["codex"] = fake
	e, err := f.bench.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	rewind := &pb.RewindSessionRequest{SessionId: e.ID, BeforePromptCursor: 1}
	_, err = f.client.RewindSession(bearer(ctx, agentToken(t, fake.Session.Open.Env)), rewind)
	expectCode(t, "agent rewound its own conversation", err, codes.PermissionDenied)
	_, err = f.client.RewindSession(bearer(ctx, ownerToken), rewind)
	expectCode(t, "a cursor that is no message", err, codes.FailedPrecondition)
	if e.Engine.Snapshot().Closed {
		t.Fatal("a refused edit stopped the agent")
	}
}

func TestDeviceCannotEditAMessage(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	e, _, _ := f.open(t)
	_, phone := f.pair(t, devices.GrantRespond, devices.GrantStart)
	_, err := f.client.RewindSession(bearer(ctx, phone), &pb.RewindSessionRequest{SessionId: e.ID, BeforePromptCursor: 1})
	expectCode(t, "device rewound the conversation", err, codes.PermissionDenied)
}

// The attempt that continues a conversation is recorded before its agent has
// started. A client that learned of it then (its predecessor's successor) and
// read it would be given a finished conversation: the stream of an attempt
// with no agent ends, and the window never shows what the agent says once it
// runs. Until the agent has started, the attempt is not announced, and
// reading it says to come back.
func TestAnAttemptThatIsStillStartingIsNotReadAsFinished(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	owner := bearer(ctx, ownerToken)
	fake := &agenttest.Adapter{}
	f.bench.Adapters["codex"] = fake
	e, err := f.bench.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	native := fake.Session
	send := func(id, text, turn string) {
		t.Helper()
		if _, err := f.client.Apply(owner, &pb.ApplyRequest{SessionId: e.ID, OperationId: id, Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: text}}}); err != nil {
			t.Fatal(err)
		}
		<-native.Applied
		native.Events <- agent.Event{Type: "turn_started", SessionID: "thread", TurnID: turn}
		native.Events <- agent.Event{Type: "turn_ended", SessionID: "thread", TurnID: turn, Status: "completed"}
		for e.Engine.Snapshot().Busy {
			time.Sleep(5 * time.Millisecond)
		}
	}
	send("one", "one", "t1")
	send("two", "two", "t2")
	var two uint64
	tail, err := f.client.ReadSessionTail(owner, &pb.ReadSessionTailRequest{SessionId: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	for _, record := range tail.Events {
		if record.Event != nil && record.Event.Type == "operation_accepted" && record.Event.OperationId == "two" {
			two = record.Cursor
		}
	}
	fake.Gate = make(chan struct{})
	rewound := make(chan error, 1)
	go func() {
		_, err := f.client.RewindSession(owner, &pb.RewindSessionRequest{SessionId: e.ID, BeforePromptCursor: two})
		rewound <- err
	}()
	// The successor's record exists while its agent is still starting.
	var next string
	for next == "" {
		if successor, err := f.bench.History.Successor(ctx, e.ID); err == nil {
			next = successor.ID
		}
		select {
		case err := <-rewound:
			t.Fatalf("the edit returned before its agent started: %v", err)
		case <-time.After(5 * time.Millisecond):
		}
	}
	watch, cancel := context.WithTimeout(owner, 2*time.Second)
	stream, err := f.client.WatchEvents(watch, &pb.WatchEventsRequest{SessionId: next, IncludePrevious: true})
	if err == nil {
		for err == nil {
			_, err = stream.Recv()
		}
	}
	cancel()
	if err == io.EOF {
		t.Fatal("the stream of an attempt that is still starting ended as if the conversation were over")
	}
	expectCode(t, "watching an attempt that is still starting", err, codes.Unavailable)
	_, err = f.client.ReadSessionTail(owner, &pb.ReadSessionTailRequest{SessionId: next})
	expectCode(t, "reading an attempt that is still starting", err, codes.Unavailable)
	state, err := f.client.GetSession(owner, &pb.SessionRequest{SessionId: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	if state.Session.SuccessorExecutionId != "" {
		t.Fatal("an attempt whose agent has not started was announced as the conversation's next")
	}
	close(fake.Gate)
	if err := <-rewound; err != nil {
		t.Fatal(err)
	}
	// Started: announced, and its stream stays open for what the agent says.
	if state, err = f.client.GetSession(owner, &pb.SessionRequest{SessionId: e.ID}); err != nil || state.Session.SuccessorExecutionId != next {
		t.Fatalf("the started attempt is not announced: %v, %v", state.GetSession().GetSuccessorExecutionId(), err)
	}
	watch, cancel = context.WithTimeout(owner, 5*time.Second)
	defer cancel()
	if stream, err = f.client.WatchEvents(watch, &pb.WatchEventsRequest{SessionId: next, IncludePrevious: true}); err != nil {
		t.Fatal(err)
	}
	fake.Session.Events <- agent.Event{Type: "message_delta", Text: "said after the edit"}
	for {
		record, err := stream.Recv()
		if err != nil {
			t.Fatalf("the started attempt's stream ended before the agent spoke: %v", err)
		}
		if record.Event != nil && record.Event.Text == "said after the edit" {
			return
		}
	}
}
