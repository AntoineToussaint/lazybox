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

func TestBoundReviewOnlyInvalidatedByLaterRevisionObservations(t *testing.T) {
	old := Recorded{Seq: 1, Evidence: Evidence{Kind: EvidenceCommit, Member: "api", Head: "old"}}
	verdict := Recorded{Seq: 2, Evidence: Evidence{Kind: EvidenceReview, Outcome: OutcomePass, ContractDigest: "spec", Revisions: map[string]string{"api": "reviewed"}}}
	checks := Recorded{Seq: 3, Evidence: Evidence{Kind: EvidenceRun, Revisions: map[string]string{"api": "reviewed"}}}
	_, flags, _ := judge(&Task{Evidence: []Recorded{old, verdict, checks}})
	if slices.Contains(flags, FlagStale) {
		t.Fatal("old observation invalidated the newer review")
	}
	checks.Revisions["api"] = "next"
	trust, flags, _ := judge(&Task{Evidence: []Recorded{old, verdict, checks}})
	if trust == TrustVerified || !slices.Contains(flags, FlagStale) {
		t.Fatal("review survived a subsequent revision change")
	}
}
