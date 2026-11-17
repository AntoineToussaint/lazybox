package process

import (
	"slices"
	"strings"
	"testing"
	"time"
)

func TestEventCatalog(t *testing.T) {
	all := Events()
	if len(all) < 40 || all[0].Name != "mission.created" {
		t.Fatalf("catalog: %d events, first %q", len(all), all[0].Name)
	}
	seen := map[string]bool{}
	for _, e := range all {
		if seen[e.Name] {
			t.Fatalf("%s listed twice", e.Name)
		}
		seen[e.Name] = true
	}
	if e, ok := LookupEvent("verdict.recorded"); !ok || !slices.Equal(e.Vars, []string{"task", "verdict"}) {
		t.Fatalf("verdict.recorded: %+v", e)
	}
	if e, ok := LookupEvent(Stall); !ok || !e.Synthetic {
		t.Fatalf("stall: %+v", e)
	}
	if _, ok := LookupEvent("task.done"); ok {
		t.Fatal("found an event that does not exist")
	}
	// Every event a scope can cause is in the catalog.
	for scope, caused := range scopes {
		for _, e := range caused {
			if !seen[e] {
				t.Errorf("scope %s causes unknown event %s", scope, e)
			}
		}
	}
	if slices.ContainsFunc(Scopes(), func(s string) bool { return strings.HasPrefix(s, "gate.pass") }) {
		t.Fatal("a scope passes a gate")
	}
}

func TestHasScope(t *testing.T) {
	r := &Role{Scopes: []string{"goal.*", "task.create:fix", "session.spawn:worker", "evidence.read"}}
	for scope, want := range map[string]bool{
		"goal.create":           true,
		"assertion.add":         false,
		"task.create:fix":       true,
		"task.create:feature":   false,
		"task.create":           false,
		"session.spawn:worker":  true,
		"session.spawn:planner": false,
		"evidence.read":         true,
		"holdout.read":          false,
	} {
		if got := r.HasScope(scope); got != want {
			t.Errorf("HasScope(%s) = %v, want %v", scope, got, want)
		}
	}
	if (&Role{Scopes: []string{"task.create"}}).HasScope("task.create:fix") != true {
		t.Error("an unparameterized task.create holds every kind")
	}
	var none *Role
	if none.HasScope("evidence.read") {
		t.Error("a missing role holds a scope")
	}
}

func TestAgentFileCandidates(t *testing.T) {
	want := ".claude/agents/critic.md .github/agents/critic.agent.md .github/agents/critic.md .factory/droids/critic.md"
	if got := strings.Join(AgentFileCandidates("critic"), " "); got != want {
		t.Fatalf("got %s", got)
	}
}

func TestValues(t *testing.T) {
	for d, want := range map[time.Duration]string{10 * time.Hour: "10h", 90 * time.Minute: "1h30m", 45 * time.Second: "45s", 0: ""} {
		if got := Duration(d).String(); got != want {
			t.Errorf("%v: %q, want %q", d, got, want)
		}
	}
	if _, err := parseDuration("-5m"); err == nil {
		t.Error("negative duration accepted")
	}
	var m Map[int]
	m.Set("a", 1)
	m.Set("b", 2)
	m.Set("a", 3)
	m.Delete("missing")
	m.Delete("a")
	if v, ok := m.Get("b"); !ok || v != 2 || m.Len() != 1 || strings.Join(m.Keys, ",") != "b" {
		t.Fatalf("map: %+v", m)
	}
	if DefaultCeiling().Autonomy != 4 {
		t.Fatal("the default ceiling is reviewed-team's level")
	}
	issues := Issues{{Severity: Warning, Message: "w"}, {Severity: Error, Path: "a", Message: "e", Source: "f.yaml", Line: 2, Column: 3}}
	if len(issues.Errors()) != 1 || issues.Err() == nil || issues[1].String() != "f.yaml:2:3: a: e" || issues[0].String() != "warning: w" {
		t.Fatalf("issues: %v", issues.Err())
	}
	if (Issues{{Severity: Warning}}).Err() != nil {
		t.Fatal("a warning is not an error")
	}
}
