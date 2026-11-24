package rotation

import (
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
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
	return fixtureWith(t, eligible, always)
}

// always is an account policy that is on (the Bench's comes from an enabled extension).
func always() bool { return true }

func fixtureWith(t *testing.T, eligible []string, switching func() bool) (agent.Session, nativeAdapter, *accounts.State) {
	t.Helper()
	state, err := accounts.OpenState(filepath.Join(t.TempDir(), "availability.json"))
	if err != nil {
		t.Fatal(err)
	}
	native := nativeAdapter{opened: make(chan *nativeSession, 8)}
	adapter := Adapter{Native: native, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}, {ID: "c", Provider: "claude", Isolation: "environment"}, {ID: "d", Provider: "claude", Isolation: "environment"}}, Eligible: eligible, State: state, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) {
		o.Env = []string{"ACCOUNT=" + p.ID}
		return o, nil
	}, Evidence: func() string { return `{"retainedCommandIds":["build-1"]}` }, Switching: switching}
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

// A grant the provider dropped is not handed to the next subscription attempt.
func TestADroppedGrantIsNotHandedToTheNextAttempt(t *testing.T) {
	s, a, _ := fixture(t, []string{"a", "b"})
	first := <-a.opened
	first.Events <- agent.Event{Type: agent.GrantEvent, Data: json.RawMessage(`{"id":"g-wide","label":"every Monitor request","tool":"Monitor","all":true}`)}
	first.Events <- agent.Event{Type: agent.GrantEvent, Data: json.RawMessage(`{"id":"g-exact","label":"x","tool":"Bash","exact":"make"}`)}
	first.Events <- agent.Event{Type: agent.GrantDroppedEvent, Data: json.RawMessage(`{"id":"g-wide"}`)}
	first.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	receive(t, s, "profile_changed")
	second := <-a.opened
	if got := second.Open.Grants; len(got) != 1 || got[0].ID != "g-exact" {
		t.Fatalf("the next attempt holds %+v", got)
	}
}

// A Keychain that refused a renewed login leaves the stored login intact: the
// Claude worker reports it as a provider error naming the cause, which neither
// switches the subscription nor marks the account as needing a sign-in.
func TestAKeychainRefusalNeitherRotatesNorMarksTheAccount(t *testing.T) {
	s, a, state := fixture(t, []string{"a", "b"})
	first := <-a.opened
	first.Events <- agent.Event{Type: "failure", Status: "unknown", Text: "oauth_refresh_store_refused", Account: &agent.AccountObservation{State: "unknown", Reason: "oauth_refresh_store_refused"}}
	if ev := receive(t, s, "failure"); ev.Text != "oauth_refresh_store_refused" {
		t.Fatalf("the failure lost its cause: %+v", ev)
	}
	if first.closed.Load() || len(a.opened) != 0 {
		t.Fatal("a Keychain refusal switched the subscription")
	}
	if got := state.Observation("a"); got.State == "auth_required" || !state.Eligible("a", time.Now()) {
		t.Fatalf("a Keychain refusal marked the account: %+v", got)
	}
}

// Without an account policy (no Switching, or one that is off now) a limit
// stops the conversation on its account, whatever pool it started with: the
// person switches. Turning the policy on applies at the next limit.
func TestNoAccountPolicyNeverSwitchesByItself(t *testing.T) {
	var on atomic.Bool
	for name, switching := range map[string]func() bool{"no policy": nil, "policy off": on.Load} {
		t.Run(name, func(t *testing.T) {
			s, a, _ := fixtureWith(t, []string{"b"}, switching)
			first := <-a.opened
			first.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
			if failure := receive(t, s, "failure"); failure.Status != "exhausted" {
				t.Fatalf("the limit was not reported: %+v", failure)
			}
			if first.closed.Load() || len(a.opened) != 0 {
				t.Fatal("switched to another account without an account policy")
			}
		})
	}
	on.Store(true)
	s, a, _ := fixtureWith(t, []string{"b"}, on.Load)
	first := <-a.opened
	first.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	receive(t, s, "profile_changed")
	if second := <-a.opened; second.Open.Env[0] != "ACCOUNT=b" {
		t.Fatalf("the policy did not move it to b: %v", second.Open.Env)
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

func TestStopAgentReachesTheCurrentAttemptsSubAgentOnly(t *testing.T) {
	s, a, _ := fixture(t, []string{"b"})
	first := <-a.opened
	first.Events <- agent.Event{Type: "agent_status", AgentID: "claude-tool:x", NativeID: "task-x", Status: "running"}
	child := receive(t, s, "agent_status")
	if child.AgentID == "claude-tool:x" || !strings.HasSuffix(child.AgentID, ":claude-tool:x") {
		t.Fatalf("sub-agent id not scoped to its attempt: %q", child.AgentID)
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "stop", Kind: agent.StopAgent, AgentID: child.AgentID, NativeID: "task-x"}); err != nil {
		t.Fatal(err)
	}
	if op := <-first.Applied; op.Kind != agent.StopAgent || op.AgentID != "claude-tool:x" || op.NativeID != "task-x" {
		t.Fatalf("stop reached the native session as %+v", op)
	}
	first.Events <- agent.Event{Type: "failure", Status: "auth_required", Account: &agent.AccountObservation{State: "auth_required"}}
	receive(t, s, "profile_changed")
	second := <-a.opened
	<-second.Applied
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "stale", Kind: agent.StopAgent, AgentID: child.AgentID, NativeID: "task-x"}); err == nil {
		t.Fatal("a sub-agent of an earlier attempt was stopped on the new subscription")
	}
	if len(second.Applied) != 0 {
		t.Fatal("stale stop delivered")
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
	adapter := Adapter{State: state, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}, {ID: "c", Provider: "claude", Isolation: "environment"}, {ID: "d", Provider: "claude", Isolation: "environment"}}, Eligible: []string{"b"}, Switching: always, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) { return o, nil }, Native: adapterFunc(func(ctx context.Context, o agent.Open) (agent.Session, error) {
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
	s, err := adapter.Open(context.Background(), agent.Open{ID: "stable", Profile: "a", Host: &agent.Hosting{Root: "/hosts", Attach: &agent.Attachment{State: saved, Responded: map[string]agent.Operation{"attempt-2:current": {Kind: agent.Approve, RequestID: "attempt-2:current", Decision: "decline"}, "attempt-1:stale": {Kind: agent.Approve}}}}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	open := native.Session.Open
	if open.Profile != "b" || open.Env[0] != "ACCOUNT=b" {
		t.Fatalf("re-attached to the wrong account: %+v", open)
	}
	if responded := open.Host.Attach.Responded; len(responded) != 1 || responded["current"].RequestID != "current" || responded["current"].Decision != "decline" {
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
	adapter := Adapter{Native: native, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}, {ID: "c", Provider: "claude", Isolation: "environment"}, {ID: "d", Provider: "claude", Isolation: "environment"}}, Eligible: []string{"a", "b"}, Switching: always, State: state, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) {
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

// Text of a line recorded in part before the restart is already in the
// excerpt; when the host replays that line, the excerpt a later rotation hands
// over holds it once.
func TestReplayedReplyTextIsHandedOverOnce(t *testing.T) {
	state, err := accounts.OpenState(filepath.Join(t.TempDir(), "availability.json"))
	if err != nil {
		t.Fatal(err)
	}
	native := nativeAdapter{opened: make(chan *nativeSession, 8)}
	adapter := Adapter{Native: native, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}, {ID: "c", Provider: "claude", Isolation: "environment"}, {ID: "d", Provider: "claude", Isolation: "environment"}}, Eligible: []string{"a", "b"}, Switching: always, State: state, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) {
		o.Env = []string{"ACCOUNT=" + p.ID}
		return o, nil
	}}
	saved := json.RawMessage(`{"attempt":0,"profile":"a","native":{"host":"h1"}}`)
	recorded := agent.Event{Type: "message_delta", Text: "Halfway-unique-phrase. ", Cursor: 8, Partial: true}
	attach := &agent.Attachment{State: saved, Cursor: 7, Transcript: "\nUSER: Go\n" + recorded.Text, Replayed: map[string]int{agent.ReplayKey(recorded): 1}}
	s, err := adapter.Open(context.Background(), agent.Open{ID: "stable", Profile: "a", Cwd: t.TempDir(), Host: &agent.Hosting{Root: "/hosts", Attach: attach}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	adopted := <-native.opened
	// The host replays line 8 whole: the recorded delta, then the rest.
	adopted.Events <- recorded
	adopted.Events <- agent.Event{Type: "message_delta", Text: "Done.", Cursor: 8}
	receive(t, s, "message_delta")
	receive(t, s, "message_delta")
	adopted.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	receive(t, s, "profile_changed")
	next := <-native.opened
	handoff := <-next.Applied
	if n := strings.Count(handoff.Text, "Halfway-unique-phrase"); n != 1 || !strings.Contains(handoff.Text, "Done.") {
		t.Fatalf("replayed text handed over %d times:\n%s", n, handoff.Text)
	}
}

func TestRotationPrefersReportedCapacityAndSkipsFullWindows(t *testing.T) {
	s, native, state := fixture(t, []string{"b", "c", "d"})
	first := <-native.opened
	now := time.Now()
	// b has an elapsed immediate limit, but its longer window is still full.
	err := state.Observe("b", agent.AccountObservation{State: "exhausted", ResetAt: now.Add(-time.Minute).Unix(), Windows: []agent.UsageWindow{
		{Name: "weekly", UsedPercent: 100, ResetAt: now.Add(time.Hour).Unix()},
	}})
	if err != nil {
		t.Fatal(err)
	}
	// c has no quota report. d is confirmed available and should precede it.
	if err = state.Observe("d", agent.AccountObservation{State: "available", Windows: []agent.UsageWindow{{Name: "weekly", UsedPercent: 70}}}); err != nil {
		t.Fatal(err)
	}
	first.Events <- agent.Event{Type: "failure", Account: &agent.AccountObservation{State: "exhausted"}}
	if ev := receive(t, s, "profile_changed"); ev.Text != "d" {
		t.Fatalf("rotation selected %q, want d", ev.Text)
	}
	second := <-native.opened
	second.Events <- agent.Event{Type: "failure", Account: &agent.AccountObservation{State: "exhausted"}}
	if ev := receive(t, s, "profile_changed"); ev.Text != "c" {
		t.Fatalf("rotation selected %q, want c", ev.Text)
	}
}
