package transport

import (
	"context"
	"encoding/json"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type eventStream struct {
	grpc.ServerStream
	ctx  context.Context
	sent chan *pb.JournalEvent
}

func (s *eventStream) Context() context.Context { return s.ctx }
func (s *eventStream) Send(e *pb.JournalEvent) error {
	s.sent <- e
	return nil
}

// A reader replaying a conversation learns where the replay ends, so it can
// render once when caught up; records appended after it asked are live.
func TestWatchEventsSaysWhereTheRetainedHistoryEnds(t *testing.T) {
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	for range 5 {
		if err = b.Record(e, map[string]any{"type": "message_delta", "text": "history"}); err != nil {
			t.Fatal(err)
		}
	}
	for _, includePrevious := range []bool{true, false} {
		history := e.Retained()
		ctx, cancel := context.WithCancel(context.Background())
		stream := &eventStream{ctx: ctx, sent: make(chan *pb.JournalEvent, 64)}
		done := make(chan error, 1)
		go func() {
			done <- (&Service{Bench: b}).WatchEvents(&pb.WatchEventsRequest{SessionId: e.ID, IncludePrevious: includePrevious}, stream)
		}()
		next := func() *pb.JournalEvent {
			select {
			case event := <-stream.sent:
				return event
			case <-time.After(5 * time.Second):
				t.Fatal("no event")
				return nil
			}
		}
		for cursor := uint64(1); cursor <= history; cursor++ {
			event := next()
			if event.Cursor != cursor || event.Retained != history {
				t.Fatalf("replayed record %d says retained %d, want cursor %d of %d", event.Cursor, event.Retained, cursor, history)
			}
		}
		if err = b.Record(e, map[string]any{"type": "message_delta", "text": "live"}); err != nil {
			t.Fatal(err)
		}
		if live := next(); live.Cursor != history+1 || live.Retained != live.Cursor {
			t.Fatalf("live record %d says retained %d", live.Cursor, live.Retained)
		}
		cancel()
		<-done
	}
}

// A Runner that restarts hands its live agents to the next Runner, and their
// journals go on growing there. The stream of one of them must not end as if
// the conversation were over: a reader that saw a clean end stopped following
// for good, and a window opened (or reloaded) just before a Runner restart
// kept showing the conversation as it was at the restart while the agent went
// on working. It ends Unavailable, so the reader reconnects from its cursor.
func TestWatchEventsAskToReconnectWhenTheRunnerRestarts(t *testing.T) {
	for _, includePrevious := range []bool{true, false} {
		b, err := runner.New(runner.Config{State: t.TempDir()})
		if err != nil {
			t.Fatal(err)
		}
		b.Adapters["codex"] = &agenttest.Adapter{Durable: json.RawMessage(`{"host":"fixture-host"}`)}
		e, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
		if err != nil {
			t.Fatal(err)
		}
		if err = b.Record(e, map[string]any{"type": "message_delta", "text": "history"}); err != nil {
			t.Fatal(err)
		}
		history := e.Retained()
		stream := &eventStream{ctx: context.Background(), sent: make(chan *pb.JournalEvent, 64)}
		done := make(chan error, 1)
		go func() {
			done <- (&Service{Bench: b}).WatchEvents(&pb.WatchEventsRequest{SessionId: e.ID, IncludePrevious: includePrevious}, stream)
		}()
		for seen := uint64(0); seen < history; {
			select {
			case event := <-stream.sent:
				seen = event.Cursor
			case <-time.After(5 * time.Second):
				t.Fatal("history not replayed")
			}
		}
		if err = b.Detach(); err != nil {
			t.Fatal(err)
		}
		select {
		case err = <-done:
		case <-time.After(10 * time.Second):
			t.Fatal("the stream outlived its Runner")
		}
		if status.Code(err) != codes.Unavailable {
			t.Fatalf("includePrevious=%v: a stream cut by a Runner restart ended with %v, want Unavailable so the reader reconnects", includePrevious, err)
		}
	}
}

// A conversation that is over still ends its stream cleanly: nothing will be
// added to it, and its reader stops following.
func TestWatchEventsEndsWhenTheConversationIsOver(t *testing.T) {
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if err = e.Engine.Close(); err != nil {
		t.Fatal(err)
	}
	stream := &eventStream{ctx: context.Background(), sent: make(chan *pb.JournalEvent, 64)}
	if err = (&Service{Bench: b}).WatchEvents(&pb.WatchEventsRequest{SessionId: e.ID, IncludePrevious: true}, stream); err != nil {
		t.Fatalf("a finished conversation's stream ended with %v, want a clean end", err)
	}
}
