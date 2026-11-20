package transport

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func TestVerificationUnverifiedEvidenceIsUnknown(t *testing.T) {
	f := newReviewFixture(t)
	mission, task := missionWithTask(t, f.terminalFixture, f.owner)
	_, err := f.client.UpdateMission(f.owner, &pb.UpdateMissionRequest{MissionId: mission, OperationId: "assign", Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: task, SessionId: f.conversation.ID}}})
	if err != nil {
		t.Fatal(err)
	}
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: reviews.VerificationTemplate})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Review coverage: no commands could be executed."}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.StopReason == "unverified" })
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		for _, e := range taskOf(t, getMission(t, f.terminalFixture, f.owner, mission), task).Evidence {
			if e.Kind == work.EvidenceReview && e.Outcome == work.OutcomeUnknown {
				t.Logf("zero-check run %s stays unknown", started.Run.Id)
				return
			}
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("unverified evidence never became unknown")
}

func TestVerificationInterruptedPreparationResumesCandidate(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: reviews.VerificationTemplate})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	base, err := f.bench.Commands.Start(commands.Spec{SessionID: started.Session.Id, Kind: "verification", Action: started.Run.Id + "/1/check/base", Cwd: t.TempDir(), Argv: []string{"true"}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = base.Wait(timeout(t)); err != nil {
		t.Fatal(err)
	}
	_, err = f.bench.Reviews.UpdateRun(f.mine.Id, started.Run.Id, func(r *reviews.Run) error {
		r.Verification = []reviews.VerificationCheck{{Name: "check", Member: "api", Expected: "passes", Argv: []string{"true"}, Runs: []reviews.VerificationExecution{{IsolationVersion: 1, Round: 1, Base: r.Input[0].Base, Candidate: r.Input[0].Commit, State: "running", BaseCommand: base.Status().ID}}}}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	for i := 0; i < 2; i++ {
		c, err := f.client.RunReviewVerification(reviewer, &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Name: "check", Member: "api", Expected: "passes", Argv: []string{"true"}})
		if err != nil {
			t.Fatal(err)
		}
		if c.Runs[0].CandidateCommandId == "" {
			t.Fatalf("unexpected repair: %+v", c)
		}
	}
	t.Log("missing candidate resumed")
}

func TestVerificationRejectsPausedUncapturedRound(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: reviews.VerificationTemplate})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	previousCandidate := ""
	// This is the state AuthorTurnEnded creates while a verification loop is paused:
	// round 2 is reviewing, its Input has not been captured, and Run.Input is still round 1.
	_, err = f.bench.Reviews.UpdateRun(f.mine.Id, started.Run.Id, func(r *reviews.Run) error {
		previousCandidate = r.Input[0].Commit
		r.Phase = reviews.PhaseFixing
		r.Paused = true
		r.FixingSince = 1
		rv := reviews.Review{Runs: []reviews.Run{*r}}
		_, err := reviews.AuthorTurnEnded(&rv, r.ID, 2, 2, 1, 3)
		*r = rv.Runs[0]
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(f.mine.Members[0].Path, "broken.txt"), []byte("regression"), 0600); err != nil {
		t.Fatal(err)
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	_, err = f.client.RunReviewVerification(reviewer, &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Name: "no regression", Member: "api", Expected: "no broken.txt", Argv: []string{"sh", "-c", "test ! -e broken.txt"}})
	if err == nil {
		t.Fatal("accepted a check against the previous capture while paused")
	}
	if _, err = f.bench.ControlReview(f.mine.Id, started.Run.Id, reviews.ControlResume, "", 0); err != nil {
		t.Fatal(err)
	}
	// Either preparation is still refused, or the watcher has captured the new
	// revision. Never allow the previous capture to acquire a round-2 receipt.
	check, err := f.client.RunReviewVerification(reviewer, &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Name: "no regression", Member: "api", Expected: "no broken.txt", Argv: []string{"true"}})
	if err == nil && check.Runs[0].Candidate == previousCandidate {
		t.Fatal("accepted previous capture for new round")
	}
}

func TestVerificationRetriesTransientCheckoutFailure(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: reviews.VerificationTemplate})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	req := &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Name: "retry", Member: "api", Expected: "command runs", Argv: []string{"true"}}
	path := os.Getenv("PATH")
	t.Setenv("PATH", t.TempDir())
	failed, err := f.client.RunReviewVerification(reviewer, req)
	if err != nil || failed.Runs[0].BaseCommandId != "" {
		t.Fatalf("did not interrupt preparation: %+v %v", failed, err)
	}
	t.Setenv("PATH", path)
	recovered, err := f.client.RunReviewVerification(reviewer, req)
	if err != nil || recovered.Runs[0].BaseCommandId == "" || recovered.Runs[0].CandidateCommandId == "" {
		t.Fatalf("retry failed: %+v %v", recovered, err)
	}
	again, err := f.client.RunReviewVerification(reviewer, req)
	if err != nil || again.Runs[0].BaseCommandId != recovered.Runs[0].BaseCommandId || again.Runs[0].CandidateCommandId != recovered.Runs[0].CandidateCommandId {
		t.Fatal("retry duplicated commands", err)
	}
}

func TestVerificationReconcilesLegacyPassingEvidence(t *testing.T) {
	f := newReviewFixture(t)
	mission, task := missionWithTask(t, f.terminalFixture, f.owner)
	_, err := f.client.UpdateMission(f.owner, &pb.UpdateMissionRequest{MissionId: mission, OperationId: "assign", Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: task, SessionId: f.conversation.ID}}})
	if err != nil {
		t.Fatal(err)
	}
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: reviews.VerificationTemplate})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Review coverage: no commands executed."}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	runUntil(t, f, func(r *pb.ReviewRun) bool { return r.StopReason == "unverified" })
	_, err = f.bench.Reviews.UpdateRun(f.mine.Id, started.Run.Id, func(r *reviews.Run) error {
		r.StopReason = reviews.StopClean
		r.Verification = []reviews.VerificationCheck{{Name: "legacy", Member: "api", Runs: []reviews.VerificationExecution{{Round: r.Round, Base: r.Input[0].Base, Candidate: r.Input[0].Commit, State: "passed"}}}}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.bench.Work.Record(work.Origin{Conversation: f.conversation.ID}, work.Actor{Kind: work.ActorAgent, Role: "reviewer", Conversation: started.Session.Id}, work.Evidence{Kind: work.EvidenceReview, Ref: "review:" + started.Run.Id, Outcome: work.OutcomePass})
	if err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(7 * time.Second)
	for time.Now().Before(deadline) {
		rv, err := f.bench.Reviews.Load(f.mine.Id)
		if err != nil {
			t.Fatal(err)
		}
		if rv.Runs[0].StopReason == reviews.StopUnverified {
			taskState := taskOf(t, getMission(t, f.terminalFixture, bearer(timeout(t), ownerToken), mission), task)
			hasPass := false
			for _, evidence := range taskState.Evidence {
				if evidence.Ref == "review:"+started.Run.Id && evidence.Outcome == work.OutcomePass {
					hasPass = true
				}
			}
			if !hasPass {
				return
			}
		}
		time.Sleep(20 * time.Millisecond)
	}
	rv, _ := f.bench.Reviews.Load(f.mine.Id)
	t.Fatalf("legacy success was not retracted: %+v", rv.Runs[0])
}
