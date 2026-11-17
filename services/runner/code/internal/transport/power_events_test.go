package transport

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// A pull request's failed checks and a person's acceptance fire hooks in the
// attempt's workspace, once each.
func TestHooksFollowChecksAndMissionAcceptance(t *testing.T) {
	out := filepath.Join(t.TempDir(), "hook.log")
	t.Setenv("HOOK_OUT", out)
	f := newTerminalFixture(t)
	b := f.bench
	actions := "hooks:\n  - id: record\n    on: [pr.checks-failed, mission.accepted]\n    shell: 'printf \"%s|%s|%s\\n\" \"$LAZYBOX_EVENT\" \"$LAZYBOX_EVENT_WORKSPACE_NAME\" \"$LAZYBOX_EVENT_REASON\" >> \"$HOOK_OUT\"'\n"
	if err := os.WriteFile(b.Actions().Path(), []byte(actions), 0o600); err != nil {
		t.Fatal(err)
	}
	if snap, _ := b.Actions().Reload(); len(snap.File.Hooks) != 1 {
		t.Fatalf("actions: %+v", snap)
	}
	owner := bearer(timeout(t), ownerToken)
	missionID, taskID := missionWithTask(t, f, owner)
	if _, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "assign", Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, SessionId: f.conversation.ID}}}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "report", Change: &pb.UpdateMissionRequest_Report{Report: &pb.TaskReport{TaskId: taskID, Summary: "done"}}}); err != nil {
		t.Fatal(err)
	}
	b.ObserveHookEvents() // the baseline
	failing := &pb.PullRequestStatus{Found: true, GithubRepository: "team/api", Number: 7, HeadSha: "abc", State: "open", ChecksState: "failure", Checks: []*pb.PullRequestCheck{{Name: "lint", Status: "completed", Conclusion: "failure"}}}
	f.service.observePullRequest(f.mine.Id, "api", failing)
	pf := &powerFixture{client: f.client}
	pf.runs(t, settled(1))
	f.service.observePullRequest(f.mine.Id, "api", failing) // the same head: no second run
	if _, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "accept", Change: &pb.UpdateMissionRequest_Accept{Accept: &pb.GateDecision{TaskId: taskID, Note: "Ship it"}}}); err != nil {
		t.Fatal(err)
	}
	b.ObserveHookEvents()
	b.ObserveHookEvents()
	runs := pf.runs(t, settled(2))
	for _, r := range runs {
		if r.State != power.RunSucceeded || r.CollectionId != f.mine.Id {
			t.Fatalf("run: %+v", r)
		}
	}
	log, _ := os.ReadFile(out)
	if want := "pr.checks-failed|Parser|checks failed: lint (1 of 1)\nmission.accepted|Parser|accepted\n"; string(log) != want {
		t.Fatalf("hooks saw:\n%s\nwant:\n%s", log, want)
	}
}
