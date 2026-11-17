package transport

import (
	"context"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// A provider error that ends one turn (Claude's "[ede_diagnostic] …
// stop_reason=tool_use") must not leave the conversation unable to take a
// message: the next one, sent through the Runner's API, continues it.
func TestNextMessageThroughTheAPIContinuesAfterAProviderError(t *testing.T) {
	service, _ := collectionService(t)
	adapter := twoSubscriptions(t, service.Bench)
	ctx := context.Background()
	opened, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "a", Cwd: t.TempDir(), Options: &pb.RunnerOptions{NoRotation: true}})
	if err != nil {
		t.Fatal(err)
	}
	id := opened.Session.Id
	first := <-adapter.opened
	send := func(op *pb.ApplyRequest) error { op.SessionId = id; _, err := service.Apply(ctx, op); return err }
	if err := send(message("ask", "Start the baseline composition")); err != nil {
		t.Fatal(err)
	}
	_ = delivered(t, first)
	first.Events <- agent.Event{Type: "turn_started"}
	first.Events <- agent.Event{Type: "failure", Status: "provider_error", Text: "[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use"}
	first.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
	e, err := service.Bench.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	until(t, "provider error not observed", func() bool { s := e.Engine.Snapshot(); return s.Fault != "" && !s.Busy })
	if err := send(message("next", "what is running?")); err != nil {
		t.Fatalf("the next message was refused after a provider error: %v", err)
	}
	if op := delivered(t, first); op.ID != "next" {
		t.Fatalf("delivered %+v", op)
	}
	if fault := e.Engine.Snapshot().Fault; fault != "" {
		t.Fatalf("fault kept after continuing: %q", fault)
	}
}

// Send now through the Runner's API, on Claude: the stop Lazybox asked for
// ends the turn with Claude's "error during execution" report, which is no
// failure; the message is delivered as the next turn without anyone sending
// another one.
func TestSendNowThroughTheAPIDeliversAfterClaudeReportsTheStop(t *testing.T) {
	service, _ := collectionService(t)
	adapter := twoSubscriptions(t, service.Bench)
	ctx := context.Background()
	opened, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "a", Cwd: t.TempDir(), Options: &pb.RunnerOptions{NoRotation: true}})
	if err != nil {
		t.Fatal(err)
	}
	id := opened.Session.Id
	first := <-adapter.opened
	send := func(op *pb.ApplyRequest) error { op.SessionId = id; _, err := service.Apply(ctx, op); return err }
	if err := send(message("ask", "Run the build")); err != nil {
		t.Fatal(err)
	}
	_ = delivered(t, first)
	first.Events <- agent.Event{Type: "turn_started"}
	if err := send(message("queued", "Use port 4000 instead")); err != nil {
		t.Fatal(err)
	}
	if err := send(&pb.ApplyRequest{OperationId: "deliver-queued", Operation: &pb.ApplyRequest_DeliverQueued{DeliverQueued: &pb.DeliverQueued{OperationId: "queued", How: "now"}}}); err != nil {
		t.Fatal(err)
	}
	if op := delivered(t, first); op.Kind != agent.Interrupt {
		t.Fatalf("send now did not interrupt: %+v", op)
	}
	first.Events <- agent.Event{Type: "failure", Status: "provider_error", Text: "[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use", Account: &agent.AccountObservation{State: "unknown", Reason: "error_during_execution"}}
	first.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
	if op := delivered(t, first); op.ID != "queued" {
		t.Fatalf("delivered %+v", op)
	}
	e, err := service.Bench.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	if fault := e.Engine.Snapshot().Fault; fault != "" {
		t.Fatalf("the requested stop faulted the conversation: %q", fault)
	}
}
