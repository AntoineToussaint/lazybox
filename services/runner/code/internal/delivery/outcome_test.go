package delivery

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
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

// fixture is one versioned delivery scenario under testdata/v1. Each
// checkpoint assesses the first After observations for one scope.
type fixture struct {
	Schema      string          `json:"schema"`
	Description string          `json:"description"`
	Spec        json.RawMessage `json:"spec"`
	Contract    Contract        `json:"contract"`
	Candidates  []struct {
		ID        string            `json:"id"`
		Revisions map[string]string `json:"revisions"`
	} `json:"candidates"`
	Observations []Observation `json:"observations"`
	Checkpoints  []struct {
		After    int               `json:"after"`
		Scope    Scope             `json:"scope"`
		Verdict  string            `json:"verdict"`
		Decision string            `json:"decision,omitempty"`
		Criteria map[string]string `json:"criteria"`
	} `json:"checkpoints"`
}

func TestFixtures(t *testing.T) {
	paths, _ := filepath.Glob("testdata/v1/*.json")
	names := map[string]bool{}
	for _, path := range paths {
		names[strings.TrimSuffix(filepath.Base(path), ".json")] = true
	}
	for _, want := range []string{"conversation-without-mission", "approved-mission-contract", "candidate-change", "legacy-text-spec"} {
		if !names[want] {
			t.Fatalf("missing fixture %s", want)
		}
	}
	for _, path := range paths {
		t.Run(filepath.Base(path), func(t *testing.T) {
			raw, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			var f fixture
			dec := json.NewDecoder(bytes.NewReader(raw))
			dec.DisallowUnknownFields()
			if err := dec.Decode(&f); err != nil || f.Schema != "delivery-fixture.v1" {
				t.Fatalf("fixture %v %q", err, f.Schema)
			}
			// The contract is what pinning the fixture's spec revision yields.
			pinned, err := ContractFromSpec(f.Contract.Artifact, f.Contract.Version, f.Contract.Digest, string(f.Spec))
			if err != nil || !reflect.DeepEqual(pinned, f.Contract) {
				t.Fatalf("pinned contract %+v, %v; fixture %+v", pinned, err, f.Contract)
			}
			// Candidate identity is derived from the revisions, never chosen.
			for _, c := range f.Candidates {
				if got := CandidateID(c.Revisions); got != c.ID {
					t.Fatalf("candidate %v is %s, fixture says %s", c.Revisions, got, c.ID)
				}
			}
			// Round trip preserves every identity and fact.
			for _, o := range f.Observations {
				if err := o.Validate(); err != nil {
					t.Fatalf("%s: %v", o.ID, err)
				}
				data, err := json.Marshal(o)
				if err != nil {
					t.Fatal(err)
				}
				var back Observation
				if err := json.Unmarshal(data, &back); err != nil || !reflect.DeepEqual(back, o) {
					t.Fatalf("round trip %+v -> %+v (%v)", o, back, err)
				}
			}
			for _, cp := range f.Checkpoints {
				a := Assess(cp.Scope, f.Contract, f.Observations[:cp.After])
				if a.Verdict != cp.Verdict || a.Decision != cp.Decision || !reflect.DeepEqual(a.Criteria, cp.Criteria) {
					t.Fatalf("after %d for %+v: got %+v, want %s %s %v", cp.After, cp.Scope, a, cp.Verdict, cp.Decision, cp.Criteria)
				}
			}
		})
	}
}

func TestCandidateIDKeepsMembersAndRevisionsApart(t *testing.T) {
	if CandidateID(map[string]string{"a=b": "c"}) == CandidateID(map[string]string{"a": "b=c"}) {
		t.Fatal("two workspaces share a candidate")
	}
}

// Only a decisive observation (an assertion or a failed prerequisite) replaces
// a criterion's state: a met prerequisite clears a blocked one, and nothing
// that proves nothing erases a failure.
func TestAssessKeepsResultsAgainstObservationsThatProveNothing(t *testing.T) {
	zero, one, stopped := 0, 1, 130
	s := Scope{"contract", "candidate", "attempt"}
	contract := Contract{Schema: "spec.v2", Digest: "contract", Criteria: []Criterion{{"tests", "tests pass", true}}}
	n := 0
	run := func(exit *int, assertion, prerequisite string) Observation {
		n++
		return Observation{Schema: ObservationSchema, ID: fmt.Sprint("run:", n), Scope: s, Subject: Subject{Conversation: "c"}, CriterionID: "tests", Source: SourceCommand, Reference: "run", ExitCode: exit, Assertion: assertion, Prerequisite: prerequisite}
	}
	failed, passed := run(&one, Failed, ""), run(&zero, Passed, "")
	blocked, met, bare, halted := run(&one, "", Failed), run(&zero, "", Passed), run(&zero, "", ""), run(&stopped, "", "")
	for name, tc := range map[string]struct {
		history []Observation
		want    string
	}{
		"failure, then a met prerequisite": {[]Observation{failed, met}, Failed},
		"failure, then a stopped rerun":    {[]Observation{failed, halted}, Failed},
		"pass, then a stopped rerun":       {[]Observation{passed, halted}, Passed},
		"pass, then a bare exit 0":         {[]Observation{passed, bare}, Passed},
		"blocked, then a met prerequisite": {[]Observation{blocked, met}, Unverified},
		"met prerequisite, then its check": {[]Observation{blocked, met, failed}, Failed},
		"pass, then a failed prerequisite": {[]Observation{passed, blocked}, Blocked},
		"nothing proven on an unrun one":   {[]Observation{bare}, Unverified},
	} {
		for _, o := range tc.history {
			if err := o.Validate(); err != nil {
				t.Fatalf("%s: %v", name, err)
			}
		}
		if got := Assess(s, contract, tc.history).Criteria["tests"]; got != tc.want {
			t.Errorf("%s: %s, want %s", name, got, tc.want)
		}
	}
}

func TestCandidateIDFollowsRevisions(t *testing.T) {
	a := CandidateID(map[string]string{"api": "1", "web": "2"})
	if a != CandidateID(map[string]string{"web": "2", "api": "1"}) {
		t.Fatal("member order changed the candidate")
	}
	if a == CandidateID(map[string]string{"api": "1", "web": "3"}) || a == CandidateID(map[string]string{"api": "1"}) {
		t.Fatal("a different candidate kept its identity")
	}
}

func TestObservationValidation(t *testing.T) {
	zero, one := 0, 1
	scope := Scope{"contract", "candidate", "attempt"}
	base := Observation{Schema: ObservationSchema, ID: "run:e/c", Scope: scope, Subject: Subject{Conversation: "c"}, CriterionID: "api", Source: SourceCommand, Reference: "run:e/c", ExitCode: &zero, Assertion: Passed}
	if err := base.Validate(); err != nil || base.Classify() != Passed {
		t.Fatal(err, base.Classify())
	}
	for name, change := range map[string]func(*Observation){
		"old schema":                   func(o *Observation) { o.Schema = "delivery-observation.v0" },
		"no ID":                        func(o *Observation) { o.ID = "" },
		"no reference":                 func(o *Observation) { o.Reference = "" },
		"no attempt":                   func(o *Observation) { o.Attempt = "" },
		"no conversation":              func(o *Observation) { o.Subject.Conversation = "" },
		"task without Mission":         func(o *Observation) { o.Subject.Task = "t" },
		"unknown source":               func(o *Observation) { o.Source = "ci" },
		"unknown assertion":            func(o *Observation) { o.Assertion = "ok" },
		"no criterion":                 func(o *Observation) { o.CriterionID = "" },
		"agent decision":               func(o *Observation) { o.Decision = Accepted },
		"assertion that could not run": func(o *Observation) { o.Prerequisite = Failed },
		"passing assertion, exit 1":    func(o *Observation) { o.ExitCode = &one },
		"review with an exit":          func(o *Observation) { o.Source = SourceReview },
		"human with a result":          func(o *Observation) { o.Source, o.Decision = SourceHuman, Waived },
		"human decision on one criterion": func(o *Observation) {
			o.Source, o.Decision, o.ExitCode, o.Assertion = SourceHuman, Waived, nil, ""
		},
		"failed prerequisite, exit 0": func(o *Observation) { o.Assertion, o.Prerequisite = "", Failed },
		"met prerequisite, exit 1":    func(o *Observation) { o.Assertion, o.Prerequisite, o.ExitCode = "", Passed, &one },
		"met prerequisite, no exit":   func(o *Observation) { o.Assertion, o.Prerequisite, o.ExitCode = "", Passed, nil },
		"prerequisite with assertion": func(o *Observation) { o.Prerequisite = Passed },
		"review as a prerequisite": func(o *Observation) {
			o.Source, o.ExitCode, o.Assertion, o.Prerequisite = SourceReview, nil, "", Failed
		},
	} {
		o := base
		change(&o)
		if o.Validate() == nil {
			t.Errorf("%s: accepted %+v", name, o)
		}
	}
	for name, tc := range map[string]struct {
		change func(*Observation)
		want   string
	}{
		"bare exit 0":          {func(o *Observation) { o.Assertion = "" }, Unverified},
		"piped pass":           {func(o *Observation) { o.ExitCode = nil }, Unverified},
		"failed assertion":     {func(o *Observation) { o.Assertion, o.ExitCode = Failed, &one }, Failed},
		"failed prerequisite":  {func(o *Observation) { o.Assertion, o.Prerequisite, o.ExitCode = "", Failed, &one }, Blocked},
		"review pass, no exit": {func(o *Observation) { o.Source, o.ExitCode = SourceReview, nil }, Passed},
	} {
		o := base
		tc.change(&o)
		if err := o.Validate(); err != nil || o.Classify() != tc.want {
			t.Errorf("%s: %v %s", name, err, o.Classify())
		}
	}
}

func TestAssessGuards(t *testing.T) {
	zero := 0
	s := Scope{"contract", "candidate", "attempt"}
	contract := Contract{Schema: "spec.v2", Digest: "contract", Criteria: []Criterion{{"storage", "SQL assertions pass", true}, {"docs", "guide updated", false}}}
	check := func(id, criterion, assertion string) Observation {
		return Observation{Schema: ObservationSchema, ID: id, Scope: s, Subject: Subject{Conversation: "c"}, CriterionID: criterion, Source: SourceCommand, Reference: id, ExitCode: &zero, Assertion: assertion}
	}
	invalid := check("run:e/1", "storage", Passed)
	invalid.Reference = ""
	for _, tc := range []struct {
		name     string
		scope    Scope
		contract Contract
		history  []Observation
		verdict  string
		storage  string
	}{
		{"invalid observation is ignored", s, contract, []Observation{invalid}, Incomplete, Unrun},
		{"unknown criterion", s, contract, []Observation{check("run:e/1", "api", Passed)}, Incomplete, Unrun},
		{"empty attempt", Scope{"contract", "candidate", ""}, contract, nil, Unverified, Unrun},
		{"another contract digest", Scope{"other", "candidate", "attempt"}, contract, nil, Unverified, Unrun},
		{"optional failure", s, contract, []Observation{check("run:e/1", "storage", Passed), check("run:e/2", "docs", Failed)}, Passed, Passed},
		{"only optional criteria", s, Contract{Schema: "spec.v2", Digest: "contract", Criteria: contract.Criteria[1:]}, []Observation{check("run:e/1", "docs", Passed)}, Unverified, ""},
		{"no criteria", s, Contract{Schema: "spec.v2", Digest: "contract"}, nil, Unverified, ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			a := Assess(tc.scope, tc.contract, tc.history)
			if a.Verdict != tc.verdict || a.Criteria["storage"] != tc.storage {
				t.Fatalf("got %+v", a)
			}
		})
	}
}

func TestContractFromSpec(t *testing.T) {
	c, err := ContractFromSpec("spec", 2, "digest", example)
	if err != nil || !c.Bound() || c.Lines()[0] != "[storage, required] SQL assertions pass" {
		t.Fatal(c, err)
	}
	legacy, err := ContractFromSpec("spec", 1, "digest", `{"schema":"spec.v1","problem":"p","requirements":["r"],"acceptanceCriteria":["One row"]}`)
	if err != nil || legacy.Bound() || len(legacy.Lines()) != 1 || legacy.Lines()[0] != "One row" {
		t.Fatal(legacy, err)
	}
	for _, bad := range []string{`{"schema":"spec.v1","acceptanceCriteria":[]}`, `{"schema":"spec.v3"}`, `not json`} {
		if _, err := ContractFromSpec("spec", 1, "digest", bad); err == nil {
			t.Fatalf("accepted %s", bad)
		}
	}
}
