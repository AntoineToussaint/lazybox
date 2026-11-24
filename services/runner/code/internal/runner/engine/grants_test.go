package engine

import (
	"encoding/json"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

// An "always allow" the provider reports is kept with the conversation and
// saved with its snapshot; a new engine starts with the grants it is opened
// with; taking one back reaches the provider and removes it, and only a grant
// the conversation holds can be taken back.
func TestGrantsAreKeptWithTheConversationAndCanBeTakenBack(t *testing.T) {
	ctx := contextFor(t)
	spool, err := journal.Open(filepath.Join(t.TempDir(), "session"), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	var saved Snapshot
	restored := agent.Grant{ID: "g-old", Label: "`make test`", Tool: "Bash", Spec: json.RawMessage(`{"id":"g-old","label":"` + "`make test`" + `","tool":"Bash","exact":"make test"}`)}
	a := &agenttest.Adapter{}
	e, err := NewPersisted(ctx, a, agent.Open{ID: "execution", Grants: []agent.Grant{restored}}, spool, func(s Snapshot, _ json.RawMessage) error { saved = s; return nil })
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = e.Close(); _ = spool.Dispose() })
	if got := e.Snapshot().Grants; len(got) != 1 || got[0].ID != "g-old" {
		t.Fatalf("a resumed conversation's grants: %+v", got)
	}
	spec := `{"id":"g-new","label":"commands starting with ` + "`npm run`" + `","tool":"Bash","prefix":["npm","run"]}`
	a.Session.Events <- agent.Event{Type: agent.GrantEvent, Data: json.RawMessage(spec)}
	// A sub-agent's or an unreadable grant is not the conversation's.
	a.Session.Events <- agent.Event{Type: agent.GrantEvent, AgentID: "claude-tool:x", Data: json.RawMessage(`{"id":"g-child","label":"x","tool":"Bash"}`)}
	a.Session.Events <- agent.Event{Type: agent.GrantEvent, Data: json.RawMessage(`{"label":"no id"}`)}
	a.Session.Events <- agent.Event{Type: "message_delta", Text: "done"}
	deadline := time.Now().Add(5 * time.Second)
	for e.Snapshot().LastMessage != "done" && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	got := e.Snapshot().Grants
	if len(got) != 2 || got[1].ID != "g-new" || got[1].Label != "commands starting with `npm run`" || got[1].Tool != "Bash" || string(got[1].Spec) != spec || got[1].GrantedAt.IsZero() {
		t.Fatalf("grants after the provider reported one: %+v", got)
	}
	if len(saved.Grants) != 2 {
		t.Fatalf("grants not saved with the snapshot: %+v", saved.Grants)
	}
	if _, err := e.Apply(ctx, agent.Operation{ID: "r0", Kind: agent.RevokeGrant, GrantID: "g-child"}); err == nil {
		t.Fatal("a grant the conversation does not hold was taken back")
	}
	if _, err := e.Apply(ctx, agent.Operation{ID: "r1", Kind: agent.RevokeGrant, GrantID: "g-old"}); err != nil {
		t.Fatal(err)
	}
	if op := applied(t, a.Session); op.Kind != agent.RevokeGrant || op.GrantID != "g-old" {
		t.Fatalf("revoke delivered as %+v", op)
	}
	if got := e.Snapshot().Grants; len(got) != 1 || got[0].ID != "g-new" {
		t.Fatalf("after taking one back: %+v", got)
	}
	if len(saved.Grants) != 1 {
		t.Fatalf("the taken-back grant is still saved: %+v", saved.Grants)
	}
}

// A grant the provider was opened with but no longer honours (the Claude
// worker drops an old "every Monitor request") is no longer listed, saved or
// handed on; a sub-agent's report, or one naming no grant, changes nothing.
func TestAGrantTheProviderDroppedIsNoLongerKept(t *testing.T) {
	ctx := contextFor(t)
	spool, err := journal.Open(filepath.Join(t.TempDir(), "session"), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	var saved Snapshot
	wide := agent.Grant{ID: "g-wide", Label: "every Monitor request", Tool: "Monitor", Spec: json.RawMessage(`{"id":"g-wide","label":"every Monitor request","tool":"Monitor","all":true}`)}
	exact := agent.Grant{ID: "g-exact", Label: "`make test`", Tool: "Bash", Spec: json.RawMessage(`{"id":"g-exact","label":"` + "`make test`" + `","tool":"Bash","exact":"make test"}`)}
	a := &agenttest.Adapter{}
	e, err := NewPersisted(ctx, a, agent.Open{ID: "execution", Grants: []agent.Grant{wide, exact}}, spool, func(s Snapshot, _ json.RawMessage) error { saved = s; return nil })
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = e.Close(); _ = spool.Dispose() })
	a.Session.Events <- agent.Event{Type: agent.GrantDroppedEvent, AgentID: "claude-tool:x", Data: json.RawMessage(`{"id":"g-exact"}`)}
	a.Session.Events <- agent.Event{Type: agent.GrantDroppedEvent, Data: json.RawMessage(`{}`)}
	a.Session.Events <- agent.Event{Type: agent.GrantDroppedEvent, Data: json.RawMessage(`{"id":"g-wide"}`)}
	a.Session.Events <- agent.Event{Type: "message_delta", Text: "done"}
	deadline := time.Now().Add(5 * time.Second)
	for e.Snapshot().LastMessage != "done" && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if got := e.Snapshot().Grants; len(got) != 1 || got[0].ID != "g-exact" {
		t.Fatalf("grants after the provider dropped one: %+v", got)
	}
	if len(saved.Grants) != 1 || saved.Grants[0].ID != "g-exact" {
		t.Fatalf("the dropped grant is still saved: %+v", saved.Grants)
	}
}

// A provider's notice (the Claude worker's settings_not_in_effect while the
// agent goes on) is one line in the conversation: it never faults it, as the
// failure it once was did.
func TestAProviderNoticeNeverFaultsTheConversation(t *testing.T) {
	ctx := contextFor(t)
	spool, err := journal.Open(filepath.Join(t.TempDir(), "session"), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	a := &agenttest.Adapter{}
	e, err := New(ctx, a, agent.Open{ID: "execution"}, spool)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = e.Close(); _ = spool.Dispose() })
	a.Session.Events <- agent.Event{Type: "provider_notice", Status: "settings_not_in_effect", Text: "Claude did not apply some settings."}
	a.Session.Events <- agent.Event{Type: "message_delta", Text: "done"}
	deadline := time.Now().Add(5 * time.Second)
	for e.Snapshot().LastMessage != "done" && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if fault := e.Snapshot().Fault; fault != "" {
		t.Fatalf("a notice faulted the conversation: %q", fault)
	}
	// The failure it replaces would have.
	a.Session.Events <- agent.Event{Type: "failure", Status: "provider_error", Text: "Claude did not apply some settings."}
	for e.Snapshot().Fault == "" && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if fault := e.Snapshot().Fault; fault != "provider_error" {
		t.Fatalf("a failure left the fault %q", fault)
	}
}

func TestGrantListIsBounded(t *testing.T) {
	var grants []agent.Grant
	for i := range agent.GrantLimit + 5 {
		grants = agent.WithGrant(grants, agent.Grant{ID: string(rune('A' + i)), Label: "x"})
	}
	if len(grants) != agent.GrantLimit || grants[0].ID != string(rune('A'+5)) {
		t.Fatalf("%d grants, oldest %q", len(grants), grants[0].ID)
	}
	if again := agent.WithGrant(grants, agent.Grant{ID: grants[3].ID, Label: "y"}); len(again) != agent.GrantLimit || again[len(again)-1].Label != "y" {
		t.Fatal("a grant reported again replaces the one of its id")
	}
}
