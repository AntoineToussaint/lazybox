package work

import (
	"slices"
	"strings"
	"testing"
)

// An agent review is evidence: while its latest record has open blockers the
// task is never verified, even with green checks on its head; once they are
// settled the checks verify it again.
func TestReviewBlockersKeepATaskFromVerified(t *testing.T) {
	pass := 0
	run := Recorded{Seq: 1, Evidence: Evidence{Kind: EvidenceRun, Ref: "run:e/r1", Title: "go test ./...", Outcome: OutcomePass, ExitCode: &pass, Revisions: map[string]string{"api": "abc"}}}
	pr := Recorded{Seq: 2, Evidence: Evidence{Kind: EvidencePR, Ref: "pr:o/api#1@abc", Repository: "o/api", Member: "api", Number: 1, Head: "abc", Outcome: OutcomePass}}
	blocked := Recorded{Seq: 3, Actor: Actor{Kind: ActorAgent, Role: "reviewer"}, Evidence: Evidence{Kind: EvidenceReview, Ref: "review:rv-1", Title: "Codex review of turn 2", Outcome: OutcomeFail, Detail: "2 blockers, 1 minor", Blockers: 2, Findings: 3}}
	settled := Recorded{Seq: 4, Actor: Actor{Kind: ActorAgent, Role: "reviewer"}, Evidence: Evidence{Kind: EvidenceReview, Ref: "review:rv-1", Title: "Codex review of turn 2", Outcome: OutcomePass, Detail: "1 minor", Findings: 1}}

	trust, flags, reason := judge(&Task{Evidence: []Recorded{run, pr}})
	if trust != TrustVerified || slices.Contains(flags, FlagBlockers) {
		t.Fatalf("green checks without a review: %s %v", trust, flags)
	}
	trust, flags, reason = judge(&Task{Evidence: []Recorded{run, pr, blocked}})
	if trust != TrustObserved || !slices.Contains(flags, FlagBlockers) || !strings.Contains(reason, "Codex review of turn 2 has open blockers: 2 blockers, 1 minor") {
		t.Fatalf("open blockers: %s %v %q", trust, flags, reason)
	}
	trust, flags, _ = judge(&Task{Evidence: []Recorded{pr, blocked}})
	if trust != TrustObserved || !slices.Contains(flags, FlagBlockers) {
		t.Fatalf("open blockers, checks only: %s %v", trust, flags)
	}
	trust, flags, _ = judge(&Task{Evidence: []Recorded{run, pr, blocked, settled}})
	if trust != TrustVerified || slices.Contains(flags, FlagBlockers) {
		t.Fatalf("blockers settled: %s %v", trust, flags)
	}
}

// The log accepts a review as evidence, and records the same state once.
func TestReviewEvidenceIsRecordedOnTheAttempt(t *testing.T) {
	ev := Evidence{Kind: EvidenceReview, Ref: "review:rv-1", Outcome: OutcomeFail, Blockers: 1, Findings: 1}
	if !sameEvidence(ev, ev) {
		t.Fatal("identical review evidence must be recorded once")
	}
	other := ev
	other.Blockers, other.Outcome = 0, OutcomePass
	if sameEvidence(ev, other) {
		t.Fatal("a review whose blockers were settled must be recorded again")
	}
}

func TestInvalidateReviewPreservesAttemptOrderingAndReplay(t *testing.T) {
	dir := t.TempDir()
	s := newStore(t, dir)
	m := mustOK[*State](t)(s.Create(Human, "create", "Review", ""))
	_, task, err := s.AddTask(m.ID, Human, "task", "", "Task", "", nil)
	if err != nil {
		t.Fatal(err)
	}
	_, _, err = s.Assign(m.ID, task, Human, "assign", AttemptPayload{Conversation: "old"})
	if err != nil {
		t.Fatal(err)
	}
	for _, ref := range []string{"review:old", "review:new"} {
		_, err = s.Record(Origin{Conversation: "old"}, Actor{Kind: ActorLazybox}, Evidence{Kind: EvidenceReview, Ref: ref, Outcome: OutcomePass})
		if err != nil {
			t.Fatal(err)
		}
	}
	state := mustOK[*State](t)(s.Mission(m.ID))
	prior := state.Task(task).Evidence[0]
	if err = s.EndAttempts("old", "done"); err != nil {
		t.Fatal(err)
	}
	_, _, err = s.Assign(m.ID, task, Human, "reassign", AttemptPayload{Conversation: "new"})
	if err != nil {
		t.Fatal(err)
	}
	if err = s.InvalidateReview("review:old", "not verified"); err != nil {
		t.Fatal(err)
	}
	state = mustOK[*State](t)(s.Mission(m.ID))
	seq := state.Seq
	if err = s.InvalidateReview("review:old", "not verified"); err != nil {
		t.Fatal(err)
	}
	state = mustOK[*State](t)(s.Mission(m.ID))
	if state.Seq != seq {
		t.Fatal("correction is not idempotent")
	}
	check := func(s *Store) {
		state := mustOK[*State](t)(s.Mission(m.ID))
		ev := state.Task(task).Evidence
		if len(ev) != 2 || ev[0].Outcome != OutcomeUnknown || ev[0].Seq != prior.Seq || ev[0].Attempt != prior.Attempt || ev[1].Ref != "review:new" || ev[1].Outcome != OutcomePass {
			t.Fatalf("correction changed evidence ownership/order: %+v", ev)
		}
	}
	check(s)
	if err = s.Close(); err != nil {
		t.Fatal(err)
	}
	check(newStore(t, dir))
}
