package runner

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/claude"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// unconfirmedAdapter is the Claude adapter whose sessions stop their agent
// but cannot confirm it: a close that ends "native_exit_unconfirmed".
type unconfirmedAdapter struct{ claude.Adapter }

func (a unconfirmedAdapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	s, err := a.Adapter.Open(ctx, o)
	if err != nil {
		return nil, err
	}
	return &unconfirmedSession{s}, nil
}

type unconfirmedSession struct{ agent.Session }

func (s *unconfirmedSession) Close() error {
	_ = s.Session.Close()
	return errors.New("native exit remains unconfirmed: fixture")
}
func (s *unconfirmedSession) DurableState() json.RawMessage {
	return s.Session.(agent.Durable).DurableState()
}
func (s *unconfirmedSession) Commit(ev agent.Event) { s.Session.(agent.Committer).Commit(ev) }
func (s *unconfirmedSession) Detach() error         { return s.Session.(agent.Detacher).Detach() }

// A conversation ended with its agent's exit unconfirmed is not stuck: once
// the person confirms the agent is stopped, Resume continues it.
func TestResumeAfterAnUnconfirmedExitOnceThePersonConfirmsIt(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	b := fixtureBench(t, state)
	self, _ := os.Executable()
	b.Adapters["claude"] = unconfirmedAdapter{claude.Adapter{Node: self, Worker: "fixture-claude-worker"}}
	t.Cleanup(func() { _ = b.Close() })
	e, err := openAwaitingApproval(b)
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	if err = b.StopExecution(ctx, e.ID); err == nil {
		t.Fatal("the fixture's close confirmed the exit")
	}
	if s := e.Engine.Snapshot(); !s.Closed || s.Fault != "native_exit_unconfirmed" {
		t.Fatalf("not ended unconfirmed: %+v", s)
	}
	if _, err = b.Resume(ctx, e.ID, false); err == nil {
		t.Fatal("resumed without the person confirming the agent stopped")
	}
	next, err := b.Resume(ctx, e.ID, true)
	if err != nil {
		t.Fatalf("Resume refused although the person confirmed the agent stopped: %v", err)
	}
	if next.ID == e.ID || next.Previous != e.ID {
		t.Fatalf("not resumed as a continuation: %s after %s", next.ID, next.Previous)
	}
}

// An agent the next Runner could not re-attach (for a reason other than its
// host being gone) is stopped, and its conversation ends as interrupted and
// resumable, with a notice saying why: it is never left open.
func TestAnAgentThatCouldNotBeReadoptedEndsItsConversation(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	first := fixtureBench(t, state)
	e, err := openAwaitingApproval(first)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, first, e.ID))
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	// A record this Runner cannot adopt from.
	if err = os.WriteFile(filepath.Join(state, "sessions", e.ID+".attach.json"), []byte(`{"version":99,"provider":{"x":1}}`), 0600); err != nil {
		t.Fatal(err)
	}
	second := fixtureBench(t, state)
	t.Cleanup(func() { _ = second.Close() })
	if err = second.Adopt(context.Background()); err == nil || !strings.Contains(err.Error(), "unsupported attachment") {
		t.Fatalf("the failed adoption was not reported: %v", err)
	}
	stoppedEventually(t, pid)
	saved, err := second.History.Get(context.Background(), e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if !saved.Snapshot.Closed || saved.Snapshot.Status != agent.Interrupted {
		t.Fatalf("the conversation was left open: %+v", saved.Snapshot)
	}
	restored, err := second.Get(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if restored.RecoveryRequired {
		t.Fatal("the conversation asks for recovery although its agent was stopped")
	}
	events := recorded(t, second, e.ID)
	last := events[len(events)-1]
	if last.Type != AgentLostEvent || last.Status != "not_reattached_after_restart" || !strings.Contains(last.Text, "unsupported attachment") {
		t.Fatalf("no notice says why: %+v", last)
	}
}

// A mission's task is not "closed" because its conversation's last turn
// ended with a provider error or at a limit: it waits for the next message,
// or for a switch.
func TestLivenessOfAProviderFaultIsNotClosed(t *testing.T) {
	for fault, want := range map[string]string{
		"provider_error": work.LiveIdle,
		"exhausted":      work.LiveWaiting,
		"auth_required":  work.LiveWaiting,
		"provider stream ended: provider supervisor ended": work.LiveClosed,
	} {
		e := &Execution{ID: "x", Engine: &archivedRuntime{state: engine.Snapshot{Status: agent.Failed, Fault: fault}}}
		if got := liveness(e).State; got != want {
			t.Errorf("%s: %s, want %s", fault, got, want)
		}
	}
}
