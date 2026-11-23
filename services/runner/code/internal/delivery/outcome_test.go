package delivery

import (
	"strings"
	"testing"
)

const example = `{"schema":"spec.v2","problem":"catalog","requirements":[],"acceptanceCriteria":[{"id":"storage","expected":"SQL assertions pass","required":true},{"id":"generate","expected":"service exists","required":true},{"id":"api","expected":"authenticated RPC succeeds","required":true}]}`

func TestParseSpecV2(t *testing.T) {
	cs, err := ParseSpecV2(example)
	if err != nil || len(cs) != 3 || cs[0].ID != "storage" {
		t.Fatalf("%v %v", cs, err)
	}
	for _, bad := range []string{strings.Replace(example, "generate", "storage", 1), strings.Replace(example, `,"required":true`, "", 1), strings.Replace(example, "expected", "unexpected", 1)} {
		if _, err := ParseSpecV2(bad); err == nil {
			t.Fatalf("accepted %s", bad)
		}
	}
}

func TestCatalogDelivery(t *testing.T) {
	cs, err := ParseSpecV2(example)
	if err != nil {
		t.Fatal(err)
	}
	s := Scope{"contract", "candidate", "attempt"}
	obs := func(id, result string) Observation { return Observation{s, id, "check", "command-" + id, result} }
	history := []Observation{obs("generate", "blocked"), obs("storage", "passed")}
	expect := func(verdict string) {
		t.Helper()
		if a := Assess(s, cs, history); a.Verdict != verdict {
			t.Fatalf("wanted %s, got %+v", verdict, a)
		}
	}
	expect("blocked")
	a := Assess(s, cs, history)
	if a.Criteria["api"] != "unrun" || a.Criteria["storage"] != "passed" {
		t.Fatal(a)
	}
	history = append(history, obs("generate", "passed"))
	expect("incomplete")
	wrong := obs("api", "passed")
	wrong.Candidate = "other"
	history = append(history, wrong)
	expect("incomplete")
	history = append(history, obs("api", "failed"))
	expect("failed")
	history = append(history, obs("api", "passed"))
	expect("passed")
	if a := Assess(Scope{"contract", "candidate", "fresh"}, cs, history); a.Verdict != "incomplete" {
		t.Fatal(a)
	}
	if a := Assess(s, nil, history); a.Verdict != "unverified" {
		t.Fatal(a)
	}
}

func TestAssessGuards(t *testing.T) {
	s := Scope{"contract", "candidate", "attempt"}
	required := []Criterion{{"storage", "SQL assertions pass", true}}
	optional := []Criterion{{"docs", "guide updated", false}}
	pass := Observation{s, "storage", "check", "command-1", "passed"}
	for _, tc := range []struct {
		name     string
		scope    Scope
		criteria []Criterion
		history  []Observation
		verdict  string
		state    string
	}{
		{"no source", s, required, []Observation{{s, "storage", "", "command-1", "passed"}}, "incomplete", "unrun"},
		{"no reference", s, required, []Observation{{s, "storage", "check", "", "passed"}}, "incomplete", "unrun"},
		{"unknown result", s, required, []Observation{{s, "storage", "check", "command-1", "exit 0"}}, "incomplete", "unrun"},
		{"unknown criterion", s, required, []Observation{{s, "api", "check", "command-1", "passed"}}, "incomplete", "unrun"},
		{"empty attempt", Scope{"contract", "candidate", ""}, required, []Observation{{Scope{"contract", "candidate", ""}, "storage", "check", "command-1", "passed"}}, "unverified", "unrun"},
		{"unverified observation", s, required, []Observation{pass, {s, "storage", "check", "command-2", "unverified"}}, "incomplete", "unverified"},
		{"only optional criteria", s, optional, []Observation{{s, "docs", "check", "command-1", "passed"}}, "unverified", ""},
		{"optional failure", s, append(required, optional...), []Observation{pass, {s, "docs", "check", "command-2", "failed"}}, "passed", "passed"},
		{"failed outranks blocked", s, append(required, Criterion{"api", "RPC succeeds", true}), []Observation{{s, "storage", "check", "command-1", "blocked"}, {s, "api", "check", "command-2", "failed"}}, "failed", "blocked"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			a := Assess(tc.scope, tc.criteria, tc.history)
			if a.Verdict != tc.verdict || tc.state != "" && a.Criteria["storage"] != tc.state {
				t.Fatalf("got %+v", a)
			}
		})
	}
}
