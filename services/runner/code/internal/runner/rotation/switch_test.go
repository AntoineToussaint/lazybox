package rotation

import (
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
)

func applied(t *testing.T, n *nativeSession) agent.Operation {
	t.Helper()
	select {
	case op := <-n.Applied:
		return op
	case <-time.After(5 * time.Second):
		t.Fatal("nothing delivered")
	}
	return agent.Operation{}
}

func switcher(t *testing.T, s agent.Session) agent.ProfileSwitcher {
	t.Helper()
	sw, ok := s.(agent.ProfileSwitcher)
	if !ok {
		t.Fatal("rotation session cannot switch on request")
	}
	return sw
}

// A conversation with no automatic switching stops at a limit; the person's
// switch continues the interrupted request on the chosen subscription, in the
// same execution, without replaying the operation.
func TestRequestedSwitchContinuesTheInterruptedRequest(t *testing.T) {
	s, a, state := fixture(t, nil)
	first := <-a.opened
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "one", Kind: agent.Message, Text: "Finish the build"}); err != nil {
		t.Fatal(err)
	}
	applied(t, first)
	reset := time.Now().Add(time.Hour).Unix()
	first.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: reset}}
	receive(t, s, "failure")
	if first.closed.Load() || len(a.opened) != 0 {
		t.Fatal("a conversation without automatic switching rotated")
	}
	if err := switcher(t, s).SwitchProfile("b", true); err != nil {
		t.Fatal(err)
	}
	receive(t, s, "profile_switching")
	changed := receive(t, s, "profile_changed")
	second := <-a.opened
	if !first.closed.Load() || changed.Text != "b" || changed.Status != "checkpoint_handoff" || second.Open.ID != "stable" || second.Open.Env[0] != "ACCOUNT=b" {
		t.Fatalf("wrong switch: %+v %+v", changed, second.Open)
	}
	var data struct {
		From, FromState string
		ResetAt         int64
		Requested       bool
	}
	if err := json.Unmarshal(changed.Data, &data); err != nil || data.From != "a" || data.FromState != "exhausted" || data.ResetAt != reset || !data.Requested {
		t.Fatalf("switch does not say where it came from: %s %v", changed.Data, err)
	}
	handoff := applied(t, second)
	if handoff.ID == "one" || !strings.Contains(handoff.Text, "Finish the build") || !strings.Contains(handoff.Text, "Continue the latest user request") {
		t.Fatalf("handoff lost the request or replayed the operation: %+v", handoff)
	}
	if len(second.Applied) != 0 || len(first.Applied) != 0 {
		t.Fatal("request delivered twice")
	}
	if state.Eligible("a", time.Now()) {
		t.Fatal("the exhausted subscription lost its observation")
	}
	// Later events come from the new subscription, as attempt 1.
	second.Events <- agent.Event{Type: "message_delta", Text: "continuing"}
	if ev := receive(t, s, "message_delta"); ev.Attempt != 1 {
		t.Fatalf("event attributed to attempt %d", ev.Attempt)
	}
}

// Switching between requests starts nobody's turn: the checkpoint travels with
// the next message, once.
func TestIdleSwitchCarriesTheCheckpointWithTheNextMessageOnce(t *testing.T) {
	s, a, _ := fixture(t, nil)
	first := <-a.opened
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "one", Kind: agent.Message, Text: "Rename the module"}); err != nil {
		t.Fatal(err)
	}
	applied(t, first)
	first.Events <- agent.Event{Type: "message_delta", Text: "Renamed."}
	receive(t, s, "message_delta")
	if err := switcher(t, s).SwitchProfile("b", false); err != nil {
		t.Fatal(err)
	}
	if changed := receive(t, s, "profile_changed"); changed.Status != "checkpoint_next_message" {
		t.Fatalf("idle switch started a turn: %+v", changed)
	}
	second := <-a.opened
	if len(second.Applied) != 0 {
		t.Fatal("idle switch delivered a prompt")
	}
	var state durable
	if err := json.Unmarshal(s.(*session).stateJSON(), &state); err != nil || !state.Checkpoint || state.Profile != "b" {
		t.Fatalf("pending checkpoint not durable: %+v %v", state, err)
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "two", Kind: agent.Message, Text: "Now update the docs"}); err != nil {
		t.Fatal(err)
	}
	next := applied(t, second)
	if next.ID != "two" || next.Text != "Now update the docs" || !strings.Contains(next.Context, "Rename the module") || !strings.Contains(next.Context, "Renamed.") || !strings.Contains(next.Context, "next message follows") {
		t.Fatalf("checkpoint missing from the next message: %+v", next)
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "three", Kind: agent.Message, Text: "Thanks"}); err != nil {
		t.Fatal(err)
	}
	if after := applied(t, second); after.Context != "" {
		t.Fatal("checkpoint sent twice")
	}
}

// After a reset the person can continue on the same subscription: the same
// native conversation continues, no new process starts.
func TestSwitchToTheCurrentSubscriptionRetriesInPlace(t *testing.T) {
	s, a, _ := fixture(t, nil)
	first := <-a.opened
	if err := switcher(t, s).SwitchProfile("a", false); err == nil {
		t.Fatal("a conversation with nothing interrupted retried")
	}
	if err := switcher(t, s).SwitchProfile("a", true); err != nil {
		t.Fatal(err)
	}
	if changed := receive(t, s, "profile_changed"); changed.Status != "retry_same_profile" || changed.Text != "a" {
		t.Fatalf("%+v", changed)
	}
	if retry := applied(t, first); !strings.Contains(retry.Text, "Continue the latest user request") {
		t.Fatalf("%+v", retry)
	}
	if first.closed.Load() || len(a.opened) != 0 {
		t.Fatal("retry restarted the agent")
	}
}

func TestSwitchRefusesAnotherProviderAndAnUnknownProfile(t *testing.T) {
	s, a, _ := fixture(t, nil)
	first := <-a.opened
	s.(*session).adapter.Profiles = append(s.(*session).adapter.Profiles, accounts.Profile{ID: "codex", Provider: "codex", Isolation: "environment"})
	for _, id := range []string{"codex", "missing"} {
		if err := switcher(t, s).SwitchProfile(id, true); err == nil {
			t.Fatalf("switched to %s", id)
		}
	}
	if first.closed.Load() || len(a.opened) != 0 {
		t.Fatal("a refused switch stopped the agent")
	}
}

// Turning automatic switching on for a running conversation takes effect at
// its next limit; the pool admits only isolated profiles of its provider.
func TestSetRotationEnablesAutomaticSwitchingLater(t *testing.T) {
	s, a, _ := fixture(t, nil)
	first := <-a.opened
	s.(*session).adapter.Profiles = append(s.(*session).adapter.Profiles, accounts.Profile{ID: "shared", Provider: "claude", Isolation: "inherited"})
	if err := switcher(t, s).SetRotation([]string{"shared"}); err == nil {
		t.Fatal("a shared-credential profile joined the pool")
	}
	if err := switcher(t, s).SetRotation([]string{"b"}); err != nil {
		t.Fatal(err)
	}
	first.Events <- agent.Event{Type: "failure", Status: "throttled", Account: &agent.AccountObservation{State: "throttled"}}
	if changed := receive(t, s, "profile_changed"); changed.Text != "b" {
		t.Fatalf("%+v", changed)
	}
}

// A Runner that re-attaches after an idle switch still owes the new
// subscription its checkpoint, rebuilt from the journal's transcript.
func TestPendingCheckpointSurvivesReattachment(t *testing.T) {
	state, err := accounts.OpenState(filepath.Join(t.TempDir(), "availability.json"))
	if err != nil {
		t.Fatal(err)
	}
	native := &agenttest.Adapter{Durable: json.RawMessage(`{"host":"h2"}`)}
	adapter := Adapter{Native: native, Profiles: []accounts.Profile{{ID: "a", Provider: "claude", Isolation: "environment"}, {ID: "b", Provider: "claude", Isolation: "environment"}}, State: state, Resolve: func(p accounts.Profile, o agent.Open) (agent.Open, error) { return o, nil }}
	saved := json.RawMessage(`{"attempt":1,"profile":"b","checkpoint":true,"native":{"host":"h2"}}`)
	s, err := adapter.Open(context.Background(), agent.Open{ID: "stable", Profile: "a", Host: &agent.Hosting{Root: "/hosts", Attach: &agent.Attachment{State: saved, Transcript: "\nUSER: Rename the module\nRenamed."}}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "next", Kind: agent.Message, Text: "Update the docs"}); err != nil {
		t.Fatal(err)
	}
	op := <-native.Session.Applied
	if !strings.Contains(op.Context, "Rename the module") || op.Text != "Update the docs" {
		t.Fatalf("checkpoint lost across the restart: %+v", op)
	}
}

// stateJSON is the rotation layer's own durable state, without the native's.
func (s *session) stateJSON() json.RawMessage {
	s.stateMu.Lock()
	defer s.stateMu.Unlock()
	data, _ := json.Marshal(s.state)
	return data
}

// A subscription signed in after the conversation started can still be chosen:
// the switch looks up the subscriptions signed in now, not the start's snapshot.
func TestSwitchReachesASubscriptionAddedAfterTheConversationStarted(t *testing.T) {
	s, a, _ := fixture(t, nil)
	first := <-a.opened
	snapshot := append([]accounts.Profile(nil), s.(*session).adapter.Profiles...)
	late := snapshot[0]
	late.ID = "late"
	s.(*session).adapter.Current = func() []accounts.Profile { return append(append([]accounts.Profile(nil), snapshot...), late) }
	if err := switcher(t, s).SwitchProfile("late", true); err != nil {
		t.Fatal(err)
	}
	receive(t, s, "profile_switching")
	if changed := receive(t, s, "profile_changed"); changed.Text != "late" {
		t.Fatalf("%+v", changed)
	}
	<-a.opened
	if !first.closed.Load() {
		t.Fatal("the switch kept the old agent")
	}
}

// Automatic switching can move to a subscription signed in after the
// conversation started, once the pool names it.
func TestAutomaticSwitchingReachesASubscriptionAddedLater(t *testing.T) {
	s, a, _ := fixture(t, nil)
	first := <-a.opened
	snapshot := append([]accounts.Profile(nil), s.(*session).adapter.Profiles...)
	late := snapshot[0]
	late.ID = "late"
	s.(*session).adapter.Current = func() []accounts.Profile { return append(append([]accounts.Profile(nil), snapshot...), late) }
	if err := switcher(t, s).SetRotation([]string{"late"}); err != nil {
		t.Fatal(err)
	}
	first.Events <- agent.Event{Type: "failure", Status: "throttled", Account: &agent.AccountObservation{State: "throttled"}}
	if changed := receive(t, s, "profile_changed"); changed.Text != "late" {
		t.Fatalf("%+v", changed)
	}
}
