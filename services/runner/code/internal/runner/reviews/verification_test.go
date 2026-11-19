package reviews

import "testing"

func TestVerificationRequiresCurrentExecutedChecks(t *testing.T) {
	for _, state := range []string{"", "preparing", "running", "failed", "unverified", "passed"} {
		run := Run{Round: 2, Verification: []VerificationCheck{{Runs: []VerificationExecution{{Round: 2, State: state}}}}}
		if VerificationComplete(run) != (state == "passed") {
			t.Errorf("state %q", state)
		}
		run.Round = 3
		if VerificationComplete(run) {
			t.Fatal("accepted stale execution")
		}
	}
	if VerificationComplete(Run{}) {
		t.Fatal("accepted zero checks")
	}
	run := Run{Round: 1, Input: []ReviewInput{{Skipped: 1}}, Verification: []VerificationCheck{{Runs: []VerificationExecution{{Round: 1, State: "passed"}}}}}
	if VerificationComplete(run) {
		t.Fatal("accepted an incomplete snapshot")
	}
}

func TestVerificationMissingChecksEscalates(t *testing.T) {
	f := newLoop(t, LoopAuto, 3)
	f.update(func(rv *Review) error { rv.Runs[0].TemplateID = VerificationTemplate; return nil })
	action := f.roundEnded()
	if action.Escalate == "" || f.run.StopReason != StopUnverified {
		t.Fatalf("missing escalation: %+v %+v", action, f.run)
	}
}

func TestVerificationRecipeValidation(t *testing.T) {
	valid := VerificationCheck{Name: "recovery", Member: "api", Expected: "state survives restart", Argv: []string{"go", "test", "./..."}}
	if err := valid.Validate(); err != nil {
		t.Fatal(err)
	}
	valid.Argv = nil
	if valid.Validate() == nil {
		t.Fatal("accepted empty command")
	}
	valid.Argv = []string{"sh", "\x00"}
	if valid.Validate() == nil {
		t.Fatal("accepted NUL")
	}
}

func TestVerificationPausedRepairKeepsPendingRound(t *testing.T) {
	run := Run{ID: "review", TemplateID: VerificationTemplate, Phase: PhaseFixing, Loop: LoopAuto, Round: 1, MaxRounds: 3, Paused: true, FixingSince: 10}
	rv := Review{Runs: []Run{run}}
	action, err := AuthorTurnEnded(&rv, run.ID, 2, 11, 1, 12)
	if err != nil || action.Rereview || rv.Runs[0].Round != 2 || rv.Runs[0].Phase != PhaseReviewing || rv.Runs[0].Rounds[0].Sent {
		t.Fatalf("paused repair lost: %+v %+v %v", rv.Runs[0], action, err)
	}
	if err := Control(&rv, run.ID, ControlResume, "", 0, 13); err != nil || rv.Runs[0].Paused {
		t.Fatalf("resume: %v", err)
	}
}

func TestVerificationRepairWithoutChangesEscalates(t *testing.T) {
	run := Run{ID: "review", TemplateID: VerificationTemplate, Phase: PhaseFixing, Loop: LoopAuto, Round: 1, FixingSince: 10}
	rv := Review{Runs: []Run{run}}
	action, err := AuthorTurnEnded(&rv, run.ID, 2, 11, 0, 12)
	if err != nil || action.Escalate == "" || rv.Runs[0].StopReason != StopStuck {
		t.Fatalf("empty repair stalls: %+v %v", action, err)
	}
}
