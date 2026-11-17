package runner

import (
	"context"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// An agent whose native process exited is not a lost stream: its fault is
// agent.ProcessExitFault, which no re-attachment or later event settles and
// no message continues. Resume ends its session and continues the provider's
// conversation in a new process.
func TestAnAgentThatExitedIsResumedInANewProcess(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	b := fixtureBench(t, state)
	t.Cleanup(func() { _ = b.Close() })
	e, err := openAwaitingApproval(b)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, b, e.ID))
	if err = syscall.Kill(pid, syscall.SIGKILL); err != nil {
		t.Fatal(err)
	}
	for end := time.Now().Add(10 * time.Second); e.Engine.Snapshot().Fault != agent.ProcessExitFault; time.Sleep(10 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatalf("the agent's exit was not its fault: %+v", e.Engine.Snapshot())
		}
	}
	if fault := e.Engine.Snapshot().Fault; agent.TransportFault(fault) || agent.TurnFault(fault) {
		t.Fatalf("%q is settled by a re-attachment or continued by a message", fault)
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "late", Kind: agent.Message, Text: "hello?"}); err == nil {
		t.Fatal("a message was accepted for an agent that exited")
	}
	resumed, err := b.Resume(context.Background(), e.ID, false)
	if err != nil {
		t.Fatalf("Resume refused a conversation whose agent exited: %v", err)
	}
	if resumed.ID == e.ID {
		t.Fatal("Resume re-attached to an agent that exited instead of starting a new process")
	}
	if s := e.Engine.Snapshot(); !s.Closed {
		t.Fatalf("the exited agent's session was not ended: %+v", s)
	}
	if s := resumed.Engine.Snapshot(); s.Closed || s.Fault != "" {
		t.Fatalf("the resumed conversation is not live: %+v", s)
	}
}
