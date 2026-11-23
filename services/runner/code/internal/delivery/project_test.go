package delivery

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestProjectLeadsWithTheCurrentFilesAndKeepsResolvedFailures(t *testing.T) {
	contract, err := ContractFromSpec("spec-catalog", 3, "digest", example)
	if err != nil {
		t.Fatal(err)
	}
	zero, one := 0, 1
	obs := func(n int, candidate, criterion string, exit *int, assertion, prerequisite string) Observation {
		ref := "run:e/c" + string(rune('0'+n))
		return Observation{Schema: ObservationSchema, ID: ref, Scope: Scope{"digest", candidate, "attempt"}, Subject: Subject{Conversation: "c"}, CriterionID: criterion, Source: SourceCommand, Reference: ref, ExitCode: exit, Assertion: assertion, Prerequisite: prerequisite}
	}
	history := []Observation{
		obs(1, "candidate:old", "storage", &one, Failed, ""),
		obs(2, "candidate:old", "storage", &zero, Passed, ""),
		obs(3, "candidate:mid", "generate", &one, "", Failed),
		obs(4, "candidate:old", "api", &zero, Passed, ""),
	}
	// Another attempt and another contract never count.
	stray := obs(5, "candidate:old", "generate", &zero, Passed, "")
	stray.Attempt = "other"
	history = append(history, stray)

	o := Project(contract, "attempt", Subject{Conversation: "c"}, "candidate:new", "accepted", history)
	if len(o.Candidates) != 3 || o.Candidates[0].Candidate != "candidate:new" || !o.Candidates[0].Current || o.Candidates[0].Verdict != Incomplete {
		t.Fatalf("the unchecked current files do not lead: %+v", o.Candidates)
	}
	if o.Candidates[1].Candidate != "candidate:old" || o.Candidates[2].Candidate != "candidate:mid" {
		t.Fatalf("candidates are not newest first: %+v", o.Candidates)
	}
	old := o.Candidates[1]
	storage := old.Criteria[0]
	if storage.State != Passed || len(storage.Evidence) != 2 || storage.Evidence[0].State != Failed || storage.Evidence[0].Command != "c1" {
		t.Fatalf("the resolved failure is not kept: %+v", storage)
	}
	if old.Criteria[1].State != Unrun || len(old.Criteria[1].Evidence) != 0 {
		t.Fatalf("another attempt's run counted: %+v", old.Criteria[1])
	}
	if blocking := old.Blocking(); len(blocking) != 1 || blocking[0].ID != "generate" {
		t.Fatalf("blocking: %+v", blocking)
	}
	if o.Decision != Accepted {
		t.Fatal(o.Decision)
	}

	// The projection is the body of a published snapshot.
	data, _ := json.Marshal(o)
	back, err := ParseOutcome(string(data))
	if err != nil || len(back.Candidates) != 3 {
		t.Fatal(back, err)
	}
	for _, bad := range []string{
		strings.Replace(string(data), OutcomeSchema, "delivery-outcome.v0", 1),
		strings.Replace(string(data), `"attempt":"attempt"`, `"attempt":""`, 1),
		strings.Replace(string(data), `"decision"`, `"verdictOverride"`, 1),
		string(data) + `{}`,
	} {
		if _, err := ParseOutcome(bad); err == nil {
			t.Fatalf("accepted %.120s", bad)
		}
	}
	if CommandOf("review:r@1") != "" || CommandOf("run:e/c9") != "c9" {
		t.Fatal("command references")
	}
}
