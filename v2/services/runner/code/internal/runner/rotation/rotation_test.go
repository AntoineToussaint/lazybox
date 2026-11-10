package rotation

import (
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
)

type nativeAdapter struct{ opened chan *nativeSession }
type nativeSession struct {
	*agenttest.Session
	closed atomic.Bool
}

func (a nativeAdapter) Open(_ context.Context, o agent.Open) (agent.Session, error) {
	s := &nativeSession{Session: &agenttest.Session{Open: o, Ref: o.Profile + "-native", Events: make(chan agent.Event, 16), Applied: make(chan agent.Operation, 16)}}
	a.opened <- s
	return s, nil
}
func (s *nativeSession) SessionSettings() json.RawMessage {
	return json.RawMessage(`{"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"},"model":"pinned-model"}`)
}
func (s *nativeSession) Close() error { s.closed.Store(true); return nil }
func fixture(t *testing.T, eligible []string) (agent.Session, nativeAdapter, *accounts.State) {
	t.Helper()
	state, err := accounts.OpenState(filepath.Join(t.TempDir(), "availability.json"))
	if err != nil {
		t.Fatal(err)
	}
	native := nativeAdapter{opened: make(chan *nativeSession, 8)}
	adapter := Adapter{Native: native, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}}, Eligible: eligible, State: state, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) {
		o.Env = []string{"ACCOUNT=" + p.ID}
		return o, nil
	}, Evidence: func() string { return `{"retainedCommandIds":["build-1"]}` }}
	s, err := adapter.Open(context.Background(), agent.Open{ID: "stable", Profile: "a", Cwd: t.TempDir(), Permission: "read-only", Model: "selected-model"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	return s, native, state
}
func receive(t *testing.T, s agent.Session, kind string) agent.Event {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), time.Second*5)
	defer cancel()
	for {
		ev, err := s.Receive(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if ev.Type == kind {
			return ev
		}
	}
}
func TestRotationUsesAuthenticatedProfileAndRetainsExecution(t *testing.T) {
	s, a, state := fixture(t, []string{"a", "b"})
	first := <-a.opened
	_, err := s.Apply(context.Background(), agent.Operation{ID: "one", Kind: agent.Message, Text: "Finish the build"})
	if err != nil {
		t.Fatal(err)
	}
	<-first.Applied
	first.Events <- agent.Event{Type: "message_delta", Text: "Build command build-1 is already running."}
	first.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	receive(t, s, "profile_switching")
	second := <-a.opened
	changed := receive(t, s, "profile_changed")
	if !first.closed.Load() || changed.Text != "b" || changed.SessionID != "b-native" {
		t.Fatalf("overlapping writer or wrong profile: %+v", changed)
	}
	if second.Open.ID != "stable" || second.Open.Permission != "read-only" || second.Open.Model != "selected-model" || second.Open.Resume != "" || second.Open.Env[0] != "ACCOUNT=b" || len(second.Open.ProviderSettings) == 0 {
		t.Fatalf("changed execution options: %+v", second.Open)
	}
	handoff := <-second.Applied
	if !strings.Contains(handoff.Text, "Finish the build") || !strings.Contains(handoff.Text, "build-1") || !strings.Contains(handoff.Text, "Previous approvals are invalid") || handoff.ID == "one" {
		t.Fatal("handoff lost context or replayed operation")
	}
	if state.Eligible("a", time.Now()) {
		t.Fatal("exhausted account selected again")
	}
}
func TestContextLimitsAndNoOptInNeverRotate(t *testing.T) {
	for _, tc := range []struct {
		pool  []string
		state string
	}{{nil, "exhausted"}, {[]string{"b"}, "context_full"}, {[]string{"b"}, "budget_exceeded"}} {
		t.Run(tc.state+strings.Join(tc.pool, ""), func(t *testing.T) {
			s, a, _ := fixture(t, tc.pool)
			first := <-a.opened
			first.Events <- agent.Event{Type: "failure", Status: tc.state, Account: &agent.AccountObservation{State: tc.state}}
			receive(t, s, "failure")
			if first.closed.Load() || len(a.opened) != 0 {
				t.Fatal("unrequested rotation")
			}
		})
	}
}
func TestNoEligibleProfileReportsFailureAndClosesWithoutHanging(t *testing.T) {
	s, a, state := fixture(t, []string{"b"})
	first := <-a.opened
	if err := state.Observe("b", agent.AccountObservation{State: "auth_required"}); err != nil {
		t.Fatal(err)
	}
	first.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted"}}
	ev := receive(t, s, "failure")
	if !strings.Contains(ev.Text, "no enabled subscription") || len(a.opened) != 0 {
		t.Fatalf("%+v", ev)
	}
}

func TestRequestsAreScopedToNativeAttemptAndNotReplayed(t *testing.T) {
	s, a, _ := fixture(t, []string{"b"})
	first := <-a.opened
	first.Events <- agent.Event{Type: "approval", RequestID: "n:1"}
	request := receive(t, s, "approval")
	_, err := s.Apply(context.Background(), agent.Operation{ID: "approve-a", Kind: agent.Approve, RequestID: request.RequestID, Decision: "decline"})
	if err != nil {
		t.Fatal(err)
	}
	if (<-first.Applied).RequestID != "n:1" {
		t.Fatal("native correlation changed")
	}
	first.Events <- agent.Event{Type: "failure", Status: "auth_required", Account: &agent.AccountObservation{State: "auth_required"}}
	receive(t, s, "profile_changed")
	second := <-a.opened
	<-second.Applied
	second.Events <- agent.Event{Type: "approval", RequestID: "n:1"}
	next := receive(t, s, "approval")
	if request.RequestID == next.RequestID {
		t.Fatal("native request counter collided across accounts")
	}
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "stale", Kind: agent.Approve, RequestID: request.RequestID, Decision: "accept"}); err == nil {
		t.Fatal("old approval accepted by new account")
	}
	if len(second.Applied) != 0 {
		t.Fatal("stale approval delivered")
	}
	_, err = s.Apply(context.Background(), agent.Operation{ID: "approve-b", Kind: agent.Approve, RequestID: next.RequestID, Decision: "decline"})
	if err != nil {
		t.Fatal(err)
	}
	if (<-second.Applied).RequestID != "n:1" {
		t.Fatal("new native correlation lost")
	}
}

type adapterFunc func(context.Context, agent.Open) (agent.Session, error)

func (f adapterFunc) Open(ctx context.Context, o agent.Open) (agent.Session, error) { return f(ctx, o) }
func TestCloseCancelsAccountStartupWithoutWaitingForRotationMutex(t *testing.T) {
	state, err := accounts.OpenState(filepath.Join(t.TempDir(), "state.json"))
	if err != nil {
		t.Fatal(err)
	}
	first := &nativeSession{Session: &agenttest.Session{Ref: "first", Events: make(chan agent.Event, 4), Applied: make(chan agent.Operation, 4)}}
	starting := make(chan struct{})
	var calls atomic.Int32
	adapter := Adapter{State: state, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}}, Eligible: []string{"b"}, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) { return o, nil }, Native: adapterFunc(func(ctx context.Context, o agent.Open) (agent.Session, error) {
		if calls.Add(1) == 1 {
			return first, nil
		}
		close(starting)
		<-ctx.Done()
		return nil, ctx.Err()
	})}
	s, err := adapter.Open(context.Background(), agent.Open{Profile: "a"})
	if err != nil {
		t.Fatal(err)
	}
	first.Events <- agent.Event{Type: "failure", Account: &agent.AccountObservation{State: "exhausted"}}
	<-starting
	done := make(chan error, 1)
	go func() { done <- s.Close() }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("close blocked behind provider startup")
	}
}

// Re-attachment restores the rotation attempt: request IDs keep their prefix,
// the native layer sees only its own unprefixed answered requests, and commits
// from an attempt that was rotated away never reach the current host.
func TestReattachRestoresAttemptAndScopesRequests(t *testing.T) {
	state, err := accounts.OpenState(filepath.Join(t.TempDir(), "availability.json"))
	if err != nil {
		t.Fatal(err)
	}
	native := &agenttest.Adapter{Durable: json.RawMessage(`{"host":"h2"}`)}
	adapter := Adapter{Native: native, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}}, State: state, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) {
		o.Env = []string{"ACCOUNT=" + p.ID}
		return o, nil
	}}
	saved := json.RawMessage(`{"attempt":2,"profile":"b","native":{"host":"h2"}}`)
	s, err := adapter.Open(context.Background(), agent.Open{ID: "stable", Profile: "a", Host: &agent.Hosting{Root: "/hosts", Attach: &agent.Attachment{State: saved, Responded: map[string]bool{"attempt-2:current": true, "attempt-1:stale": true}}}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	open := native.Session.Open
	if open.Profile != "b" || open.Env[0] != "ACCOUNT=b" {
		t.Fatalf("re-attached to the wrong account: %+v", open)
	}
	if responded := open.Host.Attach.Responded; len(responded) != 1 || !responded["current"] {
		t.Fatalf("native responded set: %v", responded)
	}
	native.Session.Events <- agent.Event{Type: "approval", RequestID: "native-1", Cursor: 7}
	ev := receive(t, s, "approval")
	if ev.RequestID != "attempt-2:native-1" {
		t.Fatalf("attempt not restored: %s", ev.RequestID)
	}
	s.(agent.Committer).Commit(ev)
	s.(agent.Committer).Commit(agent.Event{Cursor: 99, Attempt: 1})
	if commits := native.Session.Commits(); len(commits) != 1 || commits[0] != 7 {
		t.Fatalf("commits reached the wrong host: %v", commits)
	}
	if string(s.(agent.Durable).DurableState()) != `{"attempt":2,"profile":"b","native":{"host":"h2"}}` {
		t.Fatalf("durable state: %s", s.(agent.Durable).DurableState())
	}
	if err = s.(agent.Detacher).Detach(); err != nil || !native.Session.Detached {
		t.Fatalf("detach: %v", err)
	}
}

// A Runner restart must not leave a later rotation without the conversation:
// the re-attached session rebuilds its handoff excerpt from the journal.
func TestRotationAfterReattachHandsOverTheRecordedConversation(t *testing.T) {
	state, err := accounts.OpenState(filepath.Join(t.TempDir(), "availability.json"))
	if err != nil {
		t.Fatal(err)
	}
	native := nativeAdapter{opened: make(chan *nativeSession, 8)}
	adapter := Adapter{Native: native, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}}, Eligible: []string{"a", "b"}, State: state, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) {
		o.Env = []string{"ACCOUNT=" + p.ID}
		return o, nil
	}}
	saved := json.RawMessage(`{"attempt":0,"profile":"a","native":{"host":"h1"}}`)
	transcript := "\nUSER: Finish the build\nBuild command build-1 is already running."
	s, err := adapter.Open(context.Background(), agent.Open{ID: "stable", Profile: "a", Cwd: t.TempDir(), Host: &agent.Hosting{Root: "/hosts", Attach: &agent.Attachment{State: saved, Transcript: transcript}}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	adopted := <-native.opened
	adopted.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	receive(t, s, "profile_changed")
	next := <-native.opened
	handoff := <-next.Applied
	if !strings.Contains(handoff.Text, "Finish the build") || !strings.Contains(handoff.Text, "build-1 is already running") {
		t.Fatalf("handoff after a restart lost the conversation:\n%s", handoff.Text)
	}
}
