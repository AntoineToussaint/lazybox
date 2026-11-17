package transport

import (
	"context"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// An idle conversation's stream carries nothing, so a reader could not tell it
// from one that never connected and said "reconnecting" forever. A reader
// that asks for heartbeats hears one as soon as it has every record there was
// (at once when there was nothing to replay), then one per quiet interval. A
// reader that does not ask (an older client, the CLI) gets records only.
func TestWatchEventsHeartbeats(t *testing.T) {
	saved := watchHeartbeat
	watchHeartbeat = 50 * time.Millisecond
	t.Cleanup(func() { watchHeartbeat = saved })
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
	for range 3 {
		if err = b.Record(e, map[string]any{"type": "message_delta", "text": "history"}); err != nil {
			t.Fatal(err)
		}
	}
	history := e.Retained()
	watch := func(request *pb.WatchEventsRequest) (func() *pb.JournalEvent, func()) {
		ctx, cancel := context.WithCancel(context.Background())
		stream := &eventStream{ctx: ctx, sent: make(chan *pb.JournalEvent, 64)}
		done := make(chan error, 1)
		go func() { done <- (&Service{Bench: b}).WatchEvents(request, stream) }()
		next := func() *pb.JournalEvent {
			select {
			case event := <-stream.sent:
				return event
			case <-time.After(5 * time.Second):
				t.Fatal("no event")
				return nil
			}
		}
		return next, func() { cancel(); <-done }
	}
	for _, includePrevious := range []bool{true, false} {
		// Replaying: the records, then a heartbeat at the last of them.
		next, stop := watch(&pb.WatchEventsRequest{SessionId: e.ID, IncludePrevious: includePrevious, Heartbeats: true})
		for cursor := uint64(1); cursor <= history; cursor++ {
			if event := next(); event.Heartbeat || event.Cursor != cursor {
				t.Fatalf("includePrevious=%v: replay sent %+v, want record %d", includePrevious, event, cursor)
			}
		}
		if event := next(); !event.Heartbeat || event.Cursor != history || event.Event != nil {
			t.Fatalf("includePrevious=%v: after the replay %+v, want a heartbeat at cursor %d", includePrevious, event, history)
		}
		// Quiet: a heartbeat per interval, at the same cursor.
		if event := next(); !event.Heartbeat || event.Cursor != history {
			t.Fatalf("includePrevious=%v: an idle stream sent %+v, want a heartbeat", includePrevious, event)
		}
		stop()

		// Nothing to replay: the heartbeat comes at once, not after an interval.
		saved := watchHeartbeat
		watchHeartbeat = time.Hour
		next, stop = watch(&pb.WatchEventsRequest{SessionId: e.ID, After: history, IncludePrevious: includePrevious, Heartbeats: true})
		if event := next(); !event.Heartbeat || event.Cursor != history {
			t.Fatalf("includePrevious=%v: a caught-up reader got %+v, want a heartbeat", includePrevious, event)
		}
		stop()
		watchHeartbeat = saved
	}

	// A reader that did not ask hears records only, however long it waits.
	ctx, cancel := context.WithCancel(context.Background())
	stream := &eventStream{ctx: ctx, sent: make(chan *pb.JournalEvent, 64)}
	done := make(chan error, 1)
	go func() {
		done <- (&Service{Bench: b}).WatchEvents(&pb.WatchEventsRequest{SessionId: e.ID, After: history, IncludePrevious: true}, stream)
	}()
	select {
	case event := <-stream.sent:
		t.Fatalf("a reader that asked for no heartbeats got %+v", event)
	case <-time.After(watchHeartbeat * 4):
	}
	if err = b.Record(e, map[string]any{"type": "message_delta", "text": "live"}); err != nil {
		t.Fatal(err)
	}
	select {
	case event := <-stream.sent:
		if event.Heartbeat || event.Cursor != history+1 {
			t.Fatalf("live record %+v", event)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("no live record")
	}
	cancel()
	<-done
}
