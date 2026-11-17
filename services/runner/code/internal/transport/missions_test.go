package transport

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/snippets"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func taskOf(t *testing.T, m *pb.Mission, id string) *pb.MissionTask {
	t.Helper()
	for _, task := range m.Tasks {
		if task.Id == id {
			return task
		}
	}
	t.Fatalf("task %s missing from %s", id, m.Id)
	return nil
}

// missionWithTask creates a mission with one task, as the owner.
func missionWithTask(t *testing.T, f *terminalFixture, owner context.Context) (string, string) {
	t.Helper()
	m, err := f.client.CreateMission(owner, &pb.CreateMissionRequest{RequestKey: "m1", Title: "Fix the parser", Brief: "Split parser.go"})
	if err != nil {
		t.Fatal(err)
	}
	added, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: m.Id, OperationId: "t1", Change: &pb.UpdateMissionRequest_AddTask{AddTask: &pb.TaskCreate{Title: "Split the parser", Brief: "Three files, tests green."}}})
	if err != nil || added.CreatedId == "" {
		t.Fatal(added, err)
	}
	return m.Id, added.CreatedId
}

func getMission(t *testing.T, f *terminalFixture, owner context.Context, id string) *pb.Mission {
	t.Helper()
	m, err := f.client.GetMission(owner, &pb.GetMissionRequest{MissionId: id})
	if err != nil {
		t.Fatal(err)
	}
	return m
}

// Solo pair end to end through the authenticated Runner: the person assigns
// a task to a conversation; the agent's own `lazybox run` and a run from the
// workspace terminal attach as evidence; a commit through the PR loop pins a
// newer head; the person's report is claimed until a run passes, and only the
// person's acceptance makes the task done.
func TestMissionEvidenceAttachesAutomatically(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	owner := bearer(ctx, ownerToken)
	missionID, taskID := missionWithTask(t, f, owner)

	assigned, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "assign", Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, SessionId: f.conversation.ID, SendBrief: true}}})
	if err != nil {
		t.Fatal(err)
	}
	task := taskOf(t, assigned, taskID)
	if task.Attempt == nil || task.Attempt.ConversationId != f.conversation.Saved.ConversationID || task.Attempt.CollectionId != f.mine.Id || task.Attempt.Title != "Fix the parser" || assigned.SessionId != f.conversation.ID {
		t.Fatalf("attempt: %+v", task.Attempt)
	}
	// The brief went to the conversation (the prompt log records only what
	// the agent accepted), labeled as the mission's.
	history, err := f.client.ListPromptHistory(owner, &pb.ListPromptHistoryRequest{ConversationId: f.conversation.ID})
	if err != nil || len(history.Prompts) != 1 || history.Prompts[0].Source.GetKind() != work.SourceMission || history.Prompts[0].Source.GetTaskId() != taskID || !strings.Contains(history.Prompts[0].Text, "Three files, tests green.") || !strings.Contains(history.Prompts[0].Text, "lazybox run") {
		t.Fatalf("brief in the prompt log: %+v %v", history, err)
	}
	// Retried with the same operation: no second attempt, no second brief.
	again, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "assign", Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, SessionId: f.conversation.ID, SendBrief: true}}})
	if err != nil || len(taskOf(t, again, taskID).Attempts) != 1 || again.Seq != assigned.Seq {
		t.Fatal("a retried assignment applied twice", err)
	}

	// The agent's own failing `lazybox run`, through its scoped credential.
	agentCtx := bearer(ctx, f.agentToken)
	finish := func(caller context.Context, register *pb.RegisterPipeRequest, report *pb.CommandReport) {
		t.Helper()
		pipe, err := f.client.RegisterPipe(caller, register)
		if err != nil {
			t.Fatal(err)
		}
		if _, err = f.client.FinishPipe(caller, &pb.FinishPipeRequest{CommandId: pipe.Id, Report: report}); err != nil {
			t.Fatal(err)
		}
	}
	finish(agentCtx, &pb.RegisterPipeRequest{SessionId: f.conversation.ID, Title: "go test ./..."}, &pb.CommandReport{ExitCode: 1, OutputBytes: 4096, AgentBytes: 512, Filter: "go-test", OutputPath: "/tmp/out.log"})
	m := getMission(t, f, owner, missionID)
	task = taskOf(t, m, taskID)
	if len(task.Evidence) != 1 || task.Evidence[0].Kind != work.EvidenceRun || task.Evidence[0].Outcome != work.OutcomeFail || task.Evidence[0].GetExitCode() != 1 || task.Evidence[0].SessionId != f.conversation.ID || task.Evidence[0].OutputPath != "/tmp/out.log" || !strings.Contains(task.Evidence[0].Detail, "go-test") {
		t.Fatalf("run evidence: %+v", task.Evidence)
	}
	if task.Status != work.StatusWorking && task.Status != work.StatusReady {
		t.Fatalf("status before a report: %s", task.Status)
	}
	// Reported done after a failing run: still only claimed.
	if m, err = f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "report", Change: &pb.UpdateMissionRequest_Report{Report: &pb.TaskReport{TaskId: taskID, Summary: "Split into three files"}}}); err != nil {
		t.Fatal(err)
	}
	task = taskOf(t, m, taskID)
	if task.Status != work.StatusNeedsYou || task.Gate == "" || task.Trust != work.TrustClaimed || !contains(task.Flags, work.FlagFailing) || task.Report != "Split into three files" {
		t.Fatalf("a report after a failing run is claimed: %+v", task)
	}
	// A passing run from the workspace terminal is observed evidence too.
	finish(bearer(ctx, f.terminalToken), &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "go test ./..."}, &pb.CommandReport{ExitCode: 0, OutputBytes: 100, AgentBytes: 100})
	task = taskOf(t, getMission(t, f, owner, missionID), taskID)
	if len(task.Evidence) != 2 || task.Evidence[0].CollectionId != f.mine.Id || task.Trust != work.TrustObserved || task.EvidenceTrust != work.TrustObserved {
		t.Fatalf("a passing workspace run is observed: %+v", task)
	}
	// A run in another workspace does not reach this task.
	_, theirToken := f.openTerminal(t, f.theirs.Id)
	finish(bearer(ctx, theirToken), &pb.RegisterPipeRequest{CollectionId: f.theirs.Id, Title: "make"}, &pb.CommandReport{ExitCode: 0})
	if task = taskOf(t, getMission(t, f, owner, missionID), taskID); len(task.Evidence) != 2 {
		t.Fatalf("another workspace's run attached: %+v", task.Evidence[0])
	}
	// A commit through the PR loop attaches to the attempt's workspace.
	collection, err := f.bench.Collections.Get(f.mine.Id)
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(collection.Members[0].Path, "parser.go"), []byte("package api\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	committed, err := f.client.CommitWorkspaceChanges(owner, &pb.CommitWorkspaceChangesRequest{CollectionId: f.mine.Id, Member: "api", Message: "Split the parser"})
	if err != nil {
		t.Fatal(err)
	}
	task = taskOf(t, getMission(t, f, owner, missionID), taskID)
	if task.Evidence[0].Kind != work.EvidenceCommit || task.Evidence[0].Head != committed.Commit || task.Evidence[0].Title != "Split the parser" {
		t.Fatalf("commit evidence: %+v", task.Evidence[0])
	}
	// Checks passing on that head verify it; checks on an older head would not.
	f.service.observePullRequest(f.mine.Id, "api", &pb.PullRequestStatus{Found: true, GithubRepository: "team/api", Number: 7, HeadSha: committed.Commit, State: "open", ChecksState: "success", Checks: []*pb.PullRequestCheck{{Name: "test", Status: "completed", Conclusion: "success"}}})
	task = taskOf(t, getMission(t, f, owner, missionID), taskID)
	if task.Trust != work.TrustVerified || task.Evidence[0].Kind != work.EvidencePR || task.Evidence[0].Number != 7 {
		t.Fatalf("checks on the newest head verify: %+v", task)
	}
	before := getMission(t, f, owner, missionID).Seq
	f.service.observePullRequest(f.mine.Id, "api", &pb.PullRequestStatus{Found: true, GithubRepository: "team/api", Number: 7, HeadSha: committed.Commit, State: "open", ChecksState: "success", Checks: []*pb.PullRequestCheck{{Name: "test", Status: "completed", Conclusion: "success"}}})
	if getMission(t, f, owner, missionID).Seq != before {
		t.Fatal("an unchanged pull request was recorded twice")
	}
	// The person accepts at the gate: done, accepted.
	m, err = f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "accept", Change: &pb.UpdateMissionRequest_Accept{Accept: &pb.GateDecision{TaskId: taskID, Note: "Ship it"}}})
	if err != nil {
		t.Fatal(err)
	}
	if task = taskOf(t, m, taskID); task.Status != work.StatusDone || task.Trust != work.TrustAccepted || task.EvidenceTrust != work.TrustVerified || task.DecisionNote != "Ship it" {
		t.Fatalf("accepted: %+v", task)
	}
	if len(m.Events) == 0 || m.Events[0].Type != work.GatePassed || m.Events[0].ActorKind != work.ActorHuman || !strings.Contains(m.Events[0].Summary, "You accepted") {
		t.Fatalf("history: %+v", m.Events[0])
	}
	// A late run from the agent no longer changes a done task.
	finish(agentCtx, &pb.RegisterPipeRequest{SessionId: f.conversation.ID, Title: "go test ./..."}, &pb.CommandReport{ExitCode: 2})
	if after := getMission(t, f, owner, missionID); after.Seq != m.Seq {
		t.Fatal("evidence was recorded on a done task")
	}
}

func contains(list []string, value string) bool {
	for _, v := range list {
		if v == value {
			return true
		}
	}
	return false
}

// Every mission RPC is the owner's: an agent cannot accept its own task, a
// terminal cannot report one, and neither can read the board.
func TestMissionRPCsAreOwnerOnly(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	owner := bearer(ctx, ownerToken)
	missionID, taskID := missionWithTask(t, f, owner)
	if _, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, SessionId: f.conversation.ID}}}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, Change: &pb.UpdateMissionRequest_Report{Report: &pb.TaskReport{TaskId: taskID}}}); err != nil {
		t.Fatal(err)
	}
	for name, token := range map[string]string{"agent": f.agentToken, "terminal": f.terminalToken} {
		caller := bearer(ctx, token)
		calls := map[string]func() error{
			"ListMissions": func() error { _, err := f.client.ListMissions(caller, &pb.ListMissionsRequest{}); return err },
			"GetMission": func() error {
				_, err := f.client.GetMission(caller, &pb.GetMissionRequest{MissionId: missionID})
				return err
			},
			"CreateMission": func() error {
				_, err := f.client.CreateMission(caller, &pb.CreateMissionRequest{Title: "Mine"})
				return err
			},
			"ListPromptHistory": func() error { _, err := f.client.ListPromptHistory(caller, &pb.ListPromptHistoryRequest{}); return err },
			"Accept": func() error {
				_, err := f.client.UpdateMission(caller, &pb.UpdateMissionRequest{MissionId: missionID, Change: &pb.UpdateMissionRequest_Accept{Accept: &pb.GateDecision{TaskId: taskID}}})
				return err
			},
			"Report": func() error {
				_, err := f.client.UpdateMission(caller, &pb.UpdateMissionRequest{MissionId: missionID, Change: &pb.UpdateMissionRequest_Report{Report: &pb.TaskReport{TaskId: taskID}}})
				return err
			},
		}
		for call, invoke := range calls {
			if err := invoke(); status.Code(err) != codes.PermissionDenied {
				t.Errorf("%s credential %s: %v, want PermissionDenied", name, call, err)
			}
		}
	}
	task := taskOf(t, getMission(t, f, owner, missionID), taskID)
	if task.Status != work.StatusNeedsYou || task.Gate == "" {
		t.Fatalf("the gate moved without the owner: %+v", task)
	}
	if list, _ := f.client.ListMissions(owner, &pb.ListMissionsRequest{}); len(list.Missions) != 1 {
		t.Fatal("a scoped credential created a mission")
	}
}

// Refusals say why and write nothing.
func TestMissionRefusalsAreReadable(t *testing.T) {
	f := newTerminalFixture(t)
	owner := bearer(timeout(t), ownerToken)
	missionID, taskID := missionWithTask(t, f, owner)
	before := getMission(t, f, owner, missionID).Seq
	for _, change := range []*pb.UpdateMissionRequest{
		{MissionId: missionID, Change: &pb.UpdateMissionRequest_Accept{Accept: &pb.GateDecision{TaskId: taskID}}},
		{MissionId: missionID, Change: &pb.UpdateMissionRequest_AddTask{AddTask: &pb.TaskCreate{Title: " "}}},
		{MissionId: missionID, Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, SessionId: "missing"}}},
		{MissionId: missionID, Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, Open: &pb.OpenSessionRequest{ProfileId: "claude-default"}}}},
	} {
		_, err := f.client.UpdateMission(owner, change)
		if err == nil || strings.Contains(err.Error(), "refused:") {
			t.Errorf("%T: %v", change.Change, err)
		}
	}
	if _, err := f.client.GetMission(owner, &pb.GetMissionRequest{MissionId: "m_missing"}); status.Code(err) != codes.NotFound {
		t.Fatal(err)
	}
	if got := getMission(t, f, owner, missionID).Seq; got != before {
		t.Fatalf("refused changes were written: %d → %d", before, got)
	}
}

// A new conversation opened for a task works in the chosen workspace and
// receives the brief; deleting it fails the unreported task and forgets what
// was said in it.
func TestAssigningToANewConversationAndDeletingIt(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	owner := bearer(ctx, ownerToken)
	missionID, taskID := missionWithTask(t, f, owner)
	m, err := f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "new", Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, Open: &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: f.theirs.Id}, SendBrief: true}}})
	if err != nil {
		t.Fatal(err)
	}
	attempt := taskOf(t, m, taskID).Attempt
	if attempt.CollectionId != f.theirs.Id || m.SessionId == "" || attempt.SessionId != m.SessionId {
		t.Fatalf("new conversation: %+v", attempt)
	}
	opened, err := f.bench.Get(m.SessionId)
	if err != nil {
		t.Fatal(err)
	}
	if prompts, _ := f.client.ListPromptHistory(owner, &pb.ListPromptHistoryRequest{CollectionId: f.theirs.Id}); len(prompts.Prompts) != 1 {
		t.Fatalf("brief not recorded: %+v", prompts)
	}
	if _, err = f.client.DeleteConversation(owner, &pb.DeleteConversationRequest{SessionId: opened.ID, StopSessions: true}); err != nil {
		t.Fatal(err)
	}
	task := taskOf(t, getMission(t, f, owner, missionID), taskID)
	if task.Status != work.StatusFailed || task.Attempt.Outcome != "deleted" {
		t.Fatalf("a deleted attempt without a report fails the task: %+v", task)
	}
	if prompts, _ := f.client.ListPromptHistory(owner, &pb.ListPromptHistoryRequest{CollectionId: f.theirs.Id}); len(prompts.Prompts) != 0 {
		t.Fatal("a deleted conversation's prompts are kept")
	}
	// Assigned again: a fresh attempt.
	if m, err = f.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: missionID, Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, SessionId: f.conversation.ID}}}); err != nil {
		t.Fatal(err)
	}
	if task = taskOf(t, m, taskID); len(task.Attempts) != 2 || task.Status == work.StatusFailed {
		t.Fatalf("reassigned: %+v", task)
	}
}

// Messages go in the prompt log with their source; a message that is exactly
// a catalog snippet is recorded as that snippet, with its next: chain.
func TestPromptLogAndSnippetChains(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	owner := bearer(ctx, ownerToken)
	f.bench.RememberSnippets([]snippets.Entry{{Key: "review", Body: "Review the current change carefully.", Next: []string{"fixall"}}, {Key: "fixall", Body: "Fix every finding."}})
	send := func(text string, source *pb.PromptSource) {
		t.Helper()
		if _, err := f.client.Apply(owner, &pb.ApplyRequest{SessionId: f.conversation.ID, OperationId: text[:4] + time.Now().String(), Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: text, Source: source}}}); err != nil {
			t.Fatal(err)
		}
	}
	send("Fix the login bug", nil)
	send("Review the current change carefully.", nil)
	history, err := f.client.ListPromptHistory(owner, &pb.ListPromptHistoryRequest{ConversationId: f.conversation.ID})
	if err != nil || len(history.Prompts) != 2 {
		t.Fatal(history, err)
	}
	if p := history.Prompts[0]; p.Source.GetKind() != work.SourceSnippet || p.Source.GetSnippetKey() != "review" || history.LastSnippet != "review" || strings.Join(history.NextSnippets, ",") != "fixall" {
		t.Fatalf("snippet: %+v", history)
	}
	if p := history.Prompts[1]; p.Source.GetKind() != work.SourceTyped || p.Text != "Fix the login bug" {
		t.Fatalf("typed: %+v", p)
	}
	send("Fix every finding.\n\nAlso the docs.", &pb.PromptSource{Kind: work.SourceSnippet, SnippetKey: "fixall"})
	history, _ = f.client.ListPromptHistory(owner, &pb.ListPromptHistoryRequest{CollectionId: f.mine.Id})
	if history.Prompts[0].Source.GetSnippetKey() != "fixall" || strings.Join(history.RecentSnippets, ",") != "fixall,review" {
		t.Fatalf("labeled snippet and workspace memory: %+v", history)
	}
}

// A restarted Runner reads its missions back and derives the same board.
func TestMissionsSurviveARunnerRestart(t *testing.T) {
	state := t.TempDir()
	open := func() *Service {
		b, err := runner.New(runner.Config{State: state, OutputFallbackRoot: t.TempDir(), Address: "http://runner.invalid"})
		if err != nil {
			t.Fatal(err)
		}
		return &Service{Bench: b}
	}
	ctx := context.Background()
	s := open()
	m, err := s.CreateMission(ctx, &pb.CreateMissionRequest{Title: "Survive", Brief: "a restart"})
	if err != nil {
		t.Fatal(err)
	}
	added, err := s.UpdateMission(ctx, &pb.UpdateMissionRequest{MissionId: m.Id, Change: &pb.UpdateMissionRequest_AddTask{AddTask: &pb.TaskCreate{Title: "Persist"}}})
	if err != nil {
		t.Fatal(err)
	}
	if err = s.Bench.Close(); err != nil {
		t.Fatal(err)
	}
	s = open()
	t.Cleanup(func() { _ = s.Bench.Close() })
	again, err := s.GetMission(ctx, &pb.GetMissionRequest{MissionId: m.Id})
	if err != nil || again.Seq != added.Seq || len(again.Tasks) != 1 || again.Tasks[0].Status != work.StatusReady || again.Brief != "a restart" {
		t.Fatalf("after restart: %+v %v", again, err)
	}
	next, err := s.UpdateMission(ctx, &pb.UpdateMissionRequest{MissionId: m.Id, Change: &pb.UpdateMissionRequest_AddTask{AddTask: &pb.TaskCreate{Title: "Continue"}}})
	if err != nil || next.Seq != added.Seq+1 {
		t.Fatal("the log did not resume its sequence", err)
	}
}
