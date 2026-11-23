package work

import (
	"maps"
	"slices"
	"strings"
	"testing"
)

// deliveredPR returns the link that authorizes subsequent checks for this attempt.
func deliveredPR(check Recorded) Recorded {
	return Recorded{Evidence: Evidence{Kind: EvidencePR, Attempt: check.Attempt, Repository: check.Repository, Number: check.Number, Ref: PullRequestLinkRef(check.Repository, check.Number), Outcome: OutcomeUnknown}}
}

// An agent review is evidence: while its latest record has open blockers the
// task is never verified, even with green checks on its head; once they are
// settled the checks verify it again.
func TestReviewBlockersKeepATaskFromVerified(t *testing.T) {
	pass := 0
	run := Recorded{Seq: 1, Evidence: Evidence{Kind: EvidenceRun, Ref: "run:e/r1", Title: "go test ./...", Outcome: OutcomePass, ExitCode: &pass, Revisions: map[string]string{"api": "abc"}}}
	pr := Recorded{Seq: 2, Evidence: Evidence{Kind: EvidencePR, Ref: "pr:o/api#1@abc", Repository: "o/api", Member: "api", Number: 1, Head: "abc", Outcome: OutcomePass}}
	blocked := Recorded{Seq: 3, Actor: Actor{Kind: ActorAgent, Role: "reviewer"}, Evidence: Evidence{Kind: EvidenceReview, Ref: "review:rv-1", Title: "Codex review of turn 2", Outcome: OutcomeFail, Detail: "2 blockers, 1 minor", Blockers: 2, Findings: 3}}
	settled := Recorded{Seq: 4, Actor: Actor{Kind: ActorAgent, Role: "reviewer"}, Evidence: Evidence{Kind: EvidenceReview, Ref: "review:rv-1", Title: "Codex review of turn 2", Outcome: OutcomePass, Detail: "1 minor", Findings: 1}}

	trust, flags, reason := judge(&Task{Evidence: []Recorded{deliveredPR(pr), run, pr}})
	if trust != TrustVerified || slices.Contains(flags, FlagBlockers) {
		t.Fatalf("green checks without a review: %s %v", trust, flags)
	}
	trust, flags, reason = judge(&Task{Evidence: []Recorded{deliveredPR(pr), run, pr, blocked}})
	if trust != TrustObserved || !slices.Contains(flags, FlagBlockers) || !strings.Contains(reason, "Codex review of turn 2 has open blockers: 2 blockers, 1 minor") {
		t.Fatalf("open blockers: %s %v %q", trust, flags, reason)
	}
	trust, flags, _ = judge(&Task{Evidence: []Recorded{deliveredPR(pr), pr, blocked}})
	if trust != TrustObserved || !slices.Contains(flags, FlagBlockers) {
		t.Fatalf("open blockers, checks only: %s %v", trust, flags)
	}
	trust, flags, _ = judge(&Task{Evidence: []Recorded{deliveredPR(pr), run, pr, blocked, settled}})
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

func TestUnboundReviewCannotClearPinnedCandidateBlockers(t *testing.T) {
	checks := Recorded{Seq: 1, Evidence: Evidence{Kind: EvidencePR, Repository: "o/api", Number: 1, Member: "api", Head: "abc", Outcome: OutcomePass}}
	bound := Recorded{Seq: 2, Evidence: Evidence{Kind: EvidenceReview, Ref: "review:candidate", ContractDigest: "spec", Revisions: map[string]string{"api": "abc"}, Outcome: OutcomeFail, Blockers: 1}}
	turn := Recorded{Seq: 3, Evidence: Evidence{Kind: EvidenceReview, Ref: "review:turn", Outcome: OutcomePass}}
	task := Task{Evidence: []Recorded{deliveredPR(checks), checks, bound, turn}}
	if trust, flags, _ := judge(&task); trust == TrustVerified || !slices.Contains(flags, FlagBlockers) {
		t.Fatalf("unrelated turn review cleared candidate blockers: %s %v", trust, flags)
	}
	settled := bound
	settled.Seq, settled.Outcome, settled.Blockers = 4, OutcomePass, 0
	task.Evidence = append(task.Evidence, settled)
	if trust, flags, _ := judge(&task); trust != TrustVerified || slices.Contains(flags, FlagBlockers) {
		t.Fatalf("settled candidate blockers still active: %s %v", trust, flags)
	}
}

func TestFreshPinnedAttemptDoesNotInheritVerifiedEvidence(t *testing.T) {
	pr := Recorded{Seq: 1, Evidence: Evidence{Attempt: "first", Kind: EvidencePR, Repository: "o/api", Number: 1, Member: "api", Head: "abc", Outcome: OutcomePass}}
	task := Task{Attempts: []*Attempt{{ID: "next", Dispatch: &Dispatch{}}}, Evidence: []Recorded{deliveredPR(pr), pr}}
	if trust, flags, _ := judge(&task); trust != TrustClaimed || !slices.Contains(flags, FlagNoEvidence) {
		t.Fatalf("fresh worker inherited checks: %s %v", trust, flags)
	}
	pr.Attempt = "next"
	task.Evidence = append(task.Evidence, deliveredPR(pr), pr)
	if trust, _, _ := judge(&task); trust != TrustVerified {
		t.Fatalf("current worker checks ignored: %s", trust)
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

func TestIncompleteVerificationCannotInheritGreenCITrust(t *testing.T) {
	pr := Recorded{Seq: 1, Evidence: Evidence{Kind: EvidencePR, Ref: "pr:repo#1", Repository: "repo", Number: 1, Head: "current", Outcome: OutcomePass}}
	for _, outcome := range []string{OutcomeUnknown, OutcomePending, OutcomePass} {
		review := Recorded{Seq: 2, Evidence: Evidence{Kind: EvidenceReview, Ref: "review:1", State: "verification", Outcome: outcome}}
		trust, _, reason := judge(&Task{Evidence: []Recorded{deliveredPR(pr), pr, review}})
		if (trust == TrustVerified) != (outcome == OutcomePass) {
			t.Fatalf("%s produced %s: %s", outcome, trust, reason)
		}
	}
}

func TestPRDeliveryLinkPreservesExistingChecksAndReview(t *testing.T) {
	pr := Recorded{Seq: 1, Evidence: Evidence{Kind: EvidencePR, Repository: "team/api", Member: "api", Head: "abc", Outcome: OutcomePass, Number: 1}}
	review := Recorded{Seq: 2, Evidence: Evidence{Kind: EvidenceReview, ContractDigest: "spec", Outcome: OutcomePass, Revisions: map[string]string{"api": "abc"}}}
	link := Recorded{Seq: 3, Evidence: Evidence{Kind: EvidencePR, Ref: PullRequestLinkRef("team/api", 2), Repository: "team/api", Member: "api", Number: 2, Outcome: OutcomeUnknown}}
	if trust, flags, _ := judge(&Task{Evidence: []Recorded{deliveredPR(pr), pr, review, link}}); trust != TrustVerified || slices.Contains(flags, FlagStale) {
		t.Fatalf("delivery changed existing checks/review: %s %v", trust, flags)
	}
}

func TestTrustFollowsDeliveredPRIdentity(t *testing.T) {
	first := Recorded{Evidence: Evidence{Kind: EvidencePR, Repository: "team/api", Member: "api", Number: 12, Head: "aaa", Outcome: OutcomePass}}
	latest := first
	latest.Head = "bbb"
	second := latest
	second.Number, second.Outcome = 13, OutcomeFail
	local := Recorded{Evidence: Evidence{Kind: EvidenceCommit, Member: first.Member, Head: first.Head}}
	stale := second
	stale.Outcome = OutcomePass
	pending := second
	pending.Outcome = OutcomePending
	unknown := second
	unknown.Outcome = OutcomeUnknown
	for _, tc := range []struct {
		name        string
		evidence    []Recorded
		trust, flag string
	}{
		{"legacy workspace observation", []Recorded{first}, TrustClaimed, FlagNoEvidence},
		{"link cannot retroactively validate old checks", []Recorded{first, deliveredPR(first)}, TrustClaimed, FlagNoEvidence},
		{"current head without a local revision", []Recorded{deliveredPR(first), first, latest}, TrustVerified, ""},
		{"same repository different PRs", []Recorded{deliveredPR(second), second, deliveredPR(first), first}, TrustClaimed, FlagCIFailing},
		{"another PR failed despite locally matching pass", []Recorded{local, deliveredPR(second), second, deliveredPR(first), first}, TrustClaimed, FlagCIFailing},
		{"another PR stale", []Recorded{local, deliveredPR(second), stale, deliveredPR(first), first}, TrustClaimed, FlagStale},
		{"another PR pending", []Recorded{deliveredPR(second), pending, deliveredPR(first), first}, TrustClaimed, ""},
		{"another PR without checks", []Recorded{deliveredPR(second), unknown, deliveredPR(first), first}, TrustClaimed, ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			trust, flags, reason := judge(&Task{Evidence: tc.evidence})
			if trust != tc.trust || (tc.flag != "" && !slices.Contains(flags, tc.flag)) || (tc.flag == "" && len(flags) > 0) {
				t.Fatalf("got %s %v: %s", trust, flags, reason)
			}
		})
	}
}

func TestCurrentEvidenceSeqs(t *testing.T) {
	task := Task{Attempts: []*Attempt{{ID: "first"}, {ID: "second"}}}
	check := Recorded{Seq: 1, Evidence: Evidence{Kind: EvidencePR, Attempt: "first", Repository: "team/api", Number: 12, Head: "aaa", Outcome: OutcomePass}}
	task.Evidence = append(task.Evidence, check)
	check.Seq, check.Attempt = 2, "second"
	task.Evidence = append(task.Evidence, check)
	link := deliveredPR(check)
	link.Seq = 3
	check.Seq = 4
	task.Evidence = append(task.Evidence, link, check, Recorded{Seq: 5, Evidence: Evidence{Kind: EvidenceRun, Attempt: "second"}})
	if got := task.CurrentEvidenceSeqs(); !maps.Equal(got, map[uint64]bool{3: true, 4: true, 5: true}) {
		t.Fatalf("current evidence seqs: %v", got)
	}
}
