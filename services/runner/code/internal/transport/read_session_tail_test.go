package transport

import (
	"context"
	"fmt"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// A window reads a conversation's latest turns in one response, then follows
// it from the page's last cursor: while the agent keeps recording, nothing is
// missed and nothing is delivered twice.
func TestTailHandsOverToWatchEventsWithoutGapOrDuplicate(t *testing.T) {
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
	record := func(value map[string]any) {
		if err := b.Record(e, value); err != nil {
			t.Error(err)
		}
	}
	for turn := range 20 {
		record(map[string]any{"type": "operation_accepted", "kind": "message", "text": fmt.Sprintf("Question %d", turn)})
		for range 5 {
			record(map[string]any{"type": "message_delta", "text": "answer "})
		}
	}
	// The agent goes on recording while the window opens the conversation.
	recording := make(chan struct{})
	go func() {
		defer close(recording)
		for range 200 {
			record(map[string]any{"type": "message_delta", "text": "live "})
		}
	}()
	service := &Service{Bench: b}
	tail, err := service.ReadSessionTail(context.Background(), &pb.ReadSessionTailRequest{SessionId: e.ID, Turns: 3})
	if err != nil {
		t.Fatal(err)
	}
	if len(tail.Events) == 0 || tail.Start != tail.Events[0].Cursor || tail.Start == 1 {
		t.Fatalf("tail of %d records from %d", len(tail.Events), tail.Start)
	}
	for i, event := range tail.Events {
		if event.Cursor != tail.Start+uint64(i) || event.Retained != max(tail.Retained, event.Cursor) {
			t.Fatalf("tail record %d: cursor %d retained %d (page retained %d)", i, event.Cursor, event.Retained, tail.Retained)
		}
	}
	if first := tail.Events[0].Event; first.Type != "operation_accepted" || first.Text != "Question 17" {
		t.Fatalf("the tail starts with %s %q, want the person's 18th message", first.Type, first.Text)
	}
	last := tail.Events[len(tail.Events)-1].Cursor
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	stream := &eventStream{ctx: ctx, sent: make(chan *pb.JournalEvent, 512)}
	done := make(chan error, 1)
	go func() {
		done <- service.WatchEvents(&pb.WatchEventsRequest{SessionId: e.ID, After: last, IncludePrevious: true}, stream)
	}()
	<-recording
	end := e.Retained()
	for want := last + 1; want <= end; want++ {
		select {
		case event := <-stream.sent:
			if event.Cursor != want {
				t.Fatalf("after the tail, WatchEvents sent %d, want %d", event.Cursor, want)
			}
		case <-time.After(5 * time.Second):
			t.Fatalf("record %d of %d never came", want, end)
		}
	}
	select {
	case event := <-stream.sent:
		t.Fatalf("an extra record %d", event.Cursor)
	case <-time.After(50 * time.Millisecond):
	}
	cancel()
	<-done
}

// Earlier pages read backward reach the first record; a cursor past the
// timeline is refused like WatchEvents refuses it, and a deleted conversation
// is gone.
func TestReadSessionTailPagesAndRefusals(t *testing.T) {
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
	for turn := range 4 {
		if err = b.Record(e, map[string]any{"type": "operation_accepted", "kind": "message", "text": fmt.Sprintf("Question %d", turn)}); err != nil {
			t.Fatal(err)
		}
		if err = b.Record(e, map[string]any{"type": "message_delta", "text": "answer"}); err != nil {
			t.Fatal(err)
		}
	}
	service := &Service{Bench: b}
	ctx := context.Background()
	seen := map[uint64]bool{}
	before := uint64(0)
	for pages := 0; ; pages++ {
		page, err := service.ReadSessionTail(ctx, &pb.ReadSessionTailRequest{SessionId: e.ID, Before: before, Turns: 1})
		if err != nil {
			t.Fatal(err)
		}
		for _, event := range page.Events {
			if seen[event.Cursor] {
				t.Fatalf("record %d read twice", event.Cursor)
			}
			seen[event.Cursor] = true
		}
		if page.Start == 1 {
			break
		}
		if pages > 10 {
			t.Fatal("paging never reached the first record")
		}
		before = page.Start
	}
	if uint64(len(seen)) != e.Retained() {
		t.Fatalf("pages read %d of %d records", len(seen), e.Retained())
	}
	forward, err := service.ReadSessionTail(ctx, &pb.ReadSessionTailRequest{SessionId: e.ID, FromCursor: 2, Before: e.Retained() + 1, MaxBytes: 1})
	if err != nil || forward.Start != 2 || len(forward.Events) != 1 || forward.Events[0].Cursor != 2 {
		t.Fatalf("oversized forward page: %v, %v", forward, err)
	}
	if _, err = service.ReadSessionTail(ctx, &pb.ReadSessionTailRequest{SessionId: e.ID, FromCursor: 2, Turns: 1}); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("forward turn alignment: got %v", err)
	}
	if _, err = service.ReadSessionTail(ctx, &pb.ReadSessionTailRequest{SessionId: e.ID, Before: e.Retained() + 5}); status.Code(err) != codes.OutOfRange {
		t.Fatalf("a cursor past the timeline: %v, want OutOfRange", err)
	}
	if _, err = service.ReadSessionTail(ctx, &pb.ReadSessionTailRequest{SessionId: "missing"}); status.Code(err) != codes.NotFound {
		t.Fatalf("an unknown conversation: %v, want NotFound", err)
	}
	if err = b.DeleteConversation(ctx, e.ID, true); err != nil {
		t.Fatal(err)
	}
	if _, err = service.ReadSessionTail(ctx, &pb.ReadSessionTailRequest{SessionId: e.ID}); status.Code(err) != codes.NotFound {
		t.Fatalf("a deleted conversation: %v, want NotFound", err)
	}
}
