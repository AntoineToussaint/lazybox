package transport

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// reviewFixture: the terminal fixture's author conversation (Claude, "Fix
// the parser") has done one turn that changed api/handler.go; Codex is the
// other provider, fake, to review it.
type reviewFixture struct {
	*terminalFixture
	codex  *agenttest.Adapter
	claude *agenttest.Adapter
	owner  context.Context
}

func newReviewFixture(t *testing.T) *reviewFixture {
	t.Helper()
	f := &reviewFixture{terminalFixture: newTerminalFixture(t), codex: &agenttest.Adapter{}, claude: &agenttest.Adapter{}}
	f.bench.Adapters["codex"] = f.codex
	ctx := timeout(t)
	f.owner = bearer(ctx, ownerToken)
	// The author: a Claude conversation of its own in the fixture's workspace.
	f.bench.Adapters["claude"] = f.claude
	opened, err := f.service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: f.mine.Id})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.service.RenameConversation(ctx, &pb.RenameConversationRequest{SessionId: opened.Session.Id, Title: "Fix the parser"}); err != nil {
		t.Fatal(err)
	}
	if f.conversation, err = f.bench.Get(opened.Session.Id); err != nil {
		t.Fatal(err)
	}
	f.agentToken = agentToken(t, f.claude.Session.Open.Env)
	worktree := f.mine.Members[0].Path
	turn(t, f.service, f.claude, f.conversation.ID, "op-handler", func() {
		if err := os.WriteFile(filepath.Join(worktree, "handler.go"), []byte("package api\n\nfunc Handle() {\n\tlog.Print(\"x\")\n}\n"), 0o600); err != nil {
			t.Fatal(err)
		}
	})
	waitTurns(t, f.service, f.conversation.ID, 1)
	return f
}

// events waits until a conversation's journal holds an event of each type.
func events(t *testing.T, b *runner.Bench, session string, types ...string) map[string]agent.Event {
	t.Helper()
	ctx := timeout(t)
	found := map[string]agent.Event{}
	for {
		var cursor uint64
		for {
			read, cancel := context.WithTimeout(ctx, 50*time.Millisecond)
			record, err := b.Timeline(read, session, cursor)
			cancel()
			if err != nil {
				break
			}
			cursor = record.Sequence
			var ev agent.Event
			if json.Unmarshal(record.Value, &ev) == nil {
				found[ev.Type] = ev
			}
		}
		missing := false
		for _, kind := range types {
			if _, ok := found[kind]; !ok {
				missing = true
			}
		}
		if !missing {
			return found
		}
		select {
		case <-ctx.Done():
			t.Fatalf("%s never recorded %v; saw %v", session, types, found)
		case <-time.After(20 * time.Millisecond):
		}
	}
}

// One click: a read-only Codex conversation in the same workspace, named for
// what it reviews, linked to the author, briefed from what the Runner
// recorded. Its findings arrive through its own credential only, anchored and
// labeled; the author's conversation shows the review starting and its
// outcome.
func TestStartReviewBriefsAReadOnlyReviewerWhoseFindingsLandInTheTray(t *testing.T) {
	f := newReviewFixture(t)
	ctx := timeout(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 1})
	if err != nil {
		t.Fatal(err)
	}
	run, session := started.Run, started.Session
	if run.ReviewerLabel != "Codex" || run.SourceLabel != "Claude" || run.ScopeLabel != "Turn 1" || run.State != "running" || run.SourceTitle != "Fix the parser" || run.ReviewerSessionId != session.Id || run.SourceSessionId != f.conversation.ID {
		t.Fatalf("run: %+v", run)
	}
	if session.Title != "Review · Turn 1 · Codex" || session.Permission != "read-only" || session.CollectionId != f.mine.Id || session.Provider != "codex" || f.codex.Session.Open.Permission != "read-only" {
		t.Fatalf("reviewer conversation: %+v (opened with %q)", session, f.codex.Session.Open.Permission)
	}
	var brief agent.Operation
	select {
	case brief = <-f.codex.Session.Applied:
	case <-ctx.Done():
		t.Fatal("the brief never reached the reviewer")
	}
	for _, want := range []string{"Do op-handler", "## What to review: Turn 1", "git -C ", "api/handler.go (", "lazybox_review_finding", "lazybox review add --file", "You are read-only"} {
		if !strings.Contains(brief.Text, want) {
			t.Errorf("brief lacks %q:\n%s", want, brief.Text)
		}
	}
	history, err := f.client.ListPromptHistory(f.owner, &pb.ListPromptHistoryRequest{ConversationId: session.Id})
	if err != nil || len(history.Prompts) != 1 || history.Prompts[0].Source.GetKind() != runner.PromptSourceReview {
		t.Fatalf("the brief is recorded as Lazybox's: %+v %v", history, err)
	}

	reviewer, author := bearer(ctx, agentToken(t, f.codex.Session.Open.Env)), bearer(ctx, f.agentToken)
	finding := &pb.AddReviewFindingRequest{SessionId: session.Id, File: "api/handler.go", StartLine: 3, EndLine: 4, Severity: "critical", Title: "The error is logged, not returned", Body: "Return it so callers can retry."}
	filed, err := f.client.AddReviewFinding(reviewer, finding)
	if err != nil {
		t.Fatal(err)
	}
	c := filed.Finding
	if filed.Updated || c.Severity != "blocker" || c.Author != "agent" || c.AuthorLabel != "Codex" || c.AuthorSessionId != session.Id || c.ReviewId != run.Id || c.Disposition != "proposed" ||
		c.Member != "api" || c.Path != "handler.go" || strings.Join(c.Snippet, "\n") != "func Handle() {\n\tlog.Print(\"x\")" || c.Head == "" {
		t.Fatalf("finding: %+v", c)
	}
	// Filed again (a retry, a second look): the same finding, updated.
	finding.Severity, finding.Body = "major", "Return it; callers retry on error."
	again, err := f.client.AddReviewFinding(reviewer, finding)
	if err != nil || !again.Updated || again.Finding.Id != c.Id || again.Finding.Severity != "major" {
		t.Fatal(again, err)
	}
	args := json.RawMessage(`{"file":"api/handler.go","startLine":3,"endLine":4,"severity":"major","title":"The error is logged, not returned","body":"Return it so callers can retry."}`)
	result, err := f.codex.Session.Open.Tools(ctx, agent.ReviewFindingTool, args)
	var saved struct {
		FindingID string `json:"findingId"`
		ReviewID  string `json:"reviewId"`
		Updated   bool   `json:"updated"`
	}
	if err != nil || json.Unmarshal([]byte(result), &saved) != nil || saved.FindingID != c.Id || saved.ReviewID != run.Id || !saved.Updated {
		t.Fatalf("direct finding tool: %s (%v)", result, err)
	}
	finding.Severity, finding.Title = "blocker", "Handle never reports failure"
	if _, err = f.client.AddReviewFinding(reviewer, finding); err != nil {
		t.Fatal(err)
	}
	for name, bad := range map[string]*pb.AddReviewFindingRequest{
		"an unknown severity":      {SessionId: session.Id, File: "api/handler.go", StartLine: 1, Severity: "urgent", Title: "x"},
		"a line past the file":     {SessionId: session.Id, File: "api/handler.go", StartLine: 99, Severity: "nit", Title: "x"},
		"a file not in the repo":   {SessionId: session.Id, File: "api/missing.go", StartLine: 1, Severity: "nit", Title: "x"},
		"a path outside the repos": {SessionId: session.Id, File: "../etc/passwd", StartLine: 1, Severity: "nit", Title: "x"},
	} {
		if _, err = f.client.AddReviewFinding(reviewer, bad); err == nil {
			t.Errorf("%s accepted", name)
		}
	}
	// Only the reviewer, only for itself: the author reviews nothing, and the
	// reviewer cannot speak for the author.
	if _, err = f.client.AddReviewFinding(author, &pb.AddReviewFindingRequest{SessionId: f.conversation.ID, File: "api/handler.go", StartLine: 1, Severity: "nit", Title: "self review"}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("the author filed a finding: %v", err)
	}
	if _, err = f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: f.conversation.ID, File: "api/handler.go", StartLine: 1, Severity: "nit", Title: "x"}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("the reviewer filed as another conversation: %v", err)
	}
	if _, err = f.client.ListReviewFindings(reviewer, &pb.ListReviewFindingsRequest{SessionId: f.conversation.ID}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("the reviewer listed as another conversation: %v", err)
	}
	if _, err = f.client.StartReview(reviewer, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("an agent started a review: %v", err)
	}
	listed, err := f.client.ListReviewFindings(reviewer, &pb.ListReviewFindingsRequest{SessionId: session.Id})
	if err != nil || len(listed.Findings) != 2 || listed.Run.Id != run.Id || listed.Run.Blockers != 1 || listed.Run.Majors != 1 {
		t.Fatal(listed, err)
	}

	// The tray: the findings with their author and severity, the review with
	// its live counts. Accept one, dismiss the other with a reason.
	tray, err := f.client.ListReviewComments(f.owner, &pb.ListReviewCommentsRequest{CollectionId: f.mine.Id})
	if err != nil || len(tray.Runs) != 1 || tray.Runs[0].Id != run.Id || len(tray.Comments) != 2 {
		t.Fatal(tray, err)
	}
	tray, err = f.client.SetReviewDisposition(f.owner, &pb.SetReviewDispositionRequest{CollectionId: f.mine.Id, Ids: []string{c.Id}, Disposition: "open"})
	if err != nil || commentByID(t, tray, c.Id).Disposition != "open" {
		t.Fatal(tray, err)
	}
	other := tray.Comments[1].Id
	tray, err = f.client.SetReviewDisposition(f.owner, &pb.SetReviewDispositionRequest{CollectionId: f.mine.Id, Ids: []string{other}, Disposition: "dismissed", Note: "Handle cannot fail here"})
	if d := commentByID(t, tray, other); err != nil || d.Disposition != "dismissed" || d.Note != "Handle cannot fail here" || tray.Runs[0].Blockers != 0 || tray.Runs[0].Majors != 1 {
		t.Fatal(tray, err)
	}

	// The reviewer ends its turn: the review is done, and the author's
	// conversation shows the review start and its outcome.
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	deadline := time.Now().Add(5 * time.Second)
	for {
		tray, err = f.client.ListReviewComments(f.owner, &pb.ListReviewCommentsRequest{CollectionId: f.mine.Id})
		if err == nil && tray.Runs[0].State == "done" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("the review never finished: %+v %v", tray, err)
		}
		time.Sleep(20 * time.Millisecond)
	}
	// The blocker was dismissed: no blocker is open, so the loop ends clean.
	if r := tray.Runs[0]; r.StopReason != "clean" || r.Detail != "No blocker open after 1 round; 1 major still open." || r.Majors != 1 {
		t.Fatalf("finished: %+v", r)
	}
	seen := events(t, f.bench, f.conversation.ID, "review_started", "review_finished")
	if ev := seen["review_started"]; ev.RequestID != run.Id || ev.Text != "Codex is reviewing turn 1" {
		t.Fatalf("started event: %+v", ev)
	}
	var data runner.ReviewEvent
	if ev := seen["review_finished"]; json.Unmarshal(ev.Data, &data) != nil || data.ReviewerSession != session.Id || data.Majors != 1 || data.Blockers != 0 || ev.Text != "Codex reviewed turn 1 · 1 major" {
		t.Fatalf("finished event: %+v %+v", ev, data)
	}

	// A review of a review is refused; so is an agent nobody signed in for.
	if _, err = f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: session.Id}); err == nil {
		t.Fatal("a review conversation was reviewed")
	}
	if _, err = f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, ProfileId: "missing"}); err == nil {
		t.Fatal("an unknown subscription reviewed")
	}
	if _, err = f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Scope: "turn", Turn: 7}); err == nil || !strings.Contains(err.Error(), "no turn 7") {
		t.Fatalf("a missing turn: %v", err)
	}
}

// A review of a conversation that is an attempt on a mission task is
// evidence on that task: pending while it runs, failing while blockers are
// open, and the task is not verified because of it.
func TestAReviewIsMissionEvidence(t *testing.T) {
	f := newReviewFixture(t)
	missionID, taskID := missionWithTask(t, f.terminalFixture, f.owner)
	if _, err := f.client.UpdateMission(f.owner, &pb.UpdateMissionRequest{MissionId: missionID, OperationId: "assign", Change: &pb.UpdateMissionRequest_Assign{Assign: &pb.TaskAssign{TaskId: taskID, SessionId: f.conversation.ID}}}); err != nil {
		t.Fatal(err)
	}
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, Provider: "codex"})
	if err != nil {
		t.Fatal(err)
	}
	if started.Run.MissionId != missionID || started.Run.TaskId != taskID || started.Run.ScopeLabel != "Changes" {
		t.Fatalf("run: %+v", started.Run)
	}
	<-f.codex.Session.Applied
	evidence := func() *pb.MissionEvidence {
		for _, e := range taskOf(t, getMission(t, f.terminalFixture, f.owner, missionID), taskID).Evidence {
			if e.Kind == work.EvidenceReview {
				return e
			}
		}
		return nil
	}
	if e := evidence(); e == nil || e.Outcome != work.OutcomePending || e.Ref != "review:"+started.Run.Id {
		t.Fatalf("pending review evidence: %+v", e)
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	if _, err = f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: started.Session.Id, File: "api/handler.go", StartLine: 4, Severity: "blocker", Title: "Logged, not returned"}); err != nil {
		t.Fatal(err)
	}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	deadline := time.Now().Add(5 * time.Second)
	for e := evidence(); e == nil || e.Outcome != work.OutcomeFail; e = evidence() {
		if time.Now().After(deadline) {
			t.Fatalf("review evidence never failed: %+v", e)
		}
		time.Sleep(20 * time.Millisecond)
	}
	task := taskOf(t, getMission(t, f.terminalFixture, f.owner, missionID), taskID)
	if !contains(task.Flags, work.FlagBlockers) || task.EvidenceTrust == work.TrustVerified {
		t.Fatalf("task with open blockers: %+v", task)
	}
}

func TestStartReviewTemplateCapturesInputAndPublishesFindings(t *testing.T) {
	f := newReviewFixture(t)
	path := filepath.Join(f.mine.Members[0].Path, "untracked.go")
	if err := os.WriteFile(path, []byte("package api\n// captured\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	request := &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: "security-review-v1"}
	started, err := f.client.StartReview(f.owner, request)
	if err != nil {
		t.Fatal(err)
	}
	brief := <-f.codex.Session.Applied
	if started.Run.TemplateId != request.TemplateId || started.Session.Permission != "read-only" ||
		!strings.Contains(brief.Text, "defensive security review") || !strings.Contains(brief.Text, "api/untracked.go") {
		t.Fatalf("run %+v; brief %s", started.Run, brief.Text)
	}
	if err := os.WriteFile(path, []byte("package api\n// changed afterward\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	filed, err := f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{
		SessionId: started.Session.Id, File: "api/untracked.go", StartLine: 2, Severity: "major", Title: "Example finding", Body: "Review the evidence.",
	})
	if err != nil || strings.Join(filed.GetFinding().GetSnippet(), "") != "// captured" {
		t.Fatalf("finding: %+v, %v", filed, err)
	}
	f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Review coverage: captured API changes."}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	events(t, f.bench, f.conversation.ID, "review_finished")
	report, err := f.client.GetArtifact(f.owner, &pb.GetArtifactRequest{CollectionId: f.mine.Id, ArtifactId: started.Run.Id})
	if err != nil {
		t.Fatal(err)
	}
	if len(report.Versions) != 1 || report.Versions[0].Schema != "lazybox.review-findings/v1" {
		t.Fatalf("result version: %+v", report.Versions)
	}
	for _, want := range []string{"lazybox.review-findings/v1", "captured API changes", filed.Finding.Id, "// captured"} {
		if !strings.Contains(report.Content, want) {
			t.Errorf("result lacks %q: %s", want, report.Content)
		}
	}
}

func TestReviewPRCISendsReportToOriginalAuthor(t *testing.T) {
	for _, withFinding := range []bool{false, true} {
		name := "clean"
		if withFinding {
			name = "with finding"
		}
		t.Run(name, func(t *testing.T) {
			f := newReviewFixture(t)
			start, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: reviews.ReviewPRCITemplate})
			if err != nil {
				t.Fatal(err)
			}
			<-f.codex.Session.Applied
			if withFinding {
				reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
				_, err = f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: start.Session.Id, File: "api/handler.go", StartLine: 3, Severity: "major", Title: "Check handler behavior", Body: "Exercise the changed handler."})
				if err != nil {
					t.Fatal(err)
				}
			}
			f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Review coverage: inspected the changed handler."}
			f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
			select {
			case op := <-f.claude.Session.Applied:
				if op.Origin == nil || op.Origin.Kind != agent.OriginReviewFindings || !strings.Contains(op.Text, "lazybox artifact get "+start.Run.Id) || !strings.Contains(op.Text, "latest PR head") {
					t.Fatalf("wrong author handoff: %+v", op)
				}
			case <-time.After(12 * time.Second):
				t.Fatal("completed review did not reach the original author")
			}
		})
	}
}

func TestStartReviewTemplateRequiresReportedCoverage(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: "code-review-v1"})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	events(t, f.bench, f.conversation.ID, "review_finished")
	result, err := f.client.ListReviewFindings(bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env)), &pb.ListReviewFindingsRequest{SessionId: started.Session.Id})
	if err != nil || result.GetRun().GetState() != "failed" || result.GetRun().GetResultArtifactId() != "" {
		t.Fatalf("did not distinguish missing coverage from zero findings: %+v, %v", result, err)
	}
}

func TestOpenSessionAppliesBehaviorProfile(t *testing.T) {
	f := newReviewFixture(t)
	opened, err := f.service.OpenSession(f.owner, &pb.OpenSessionRequest{
		ProfileId: "codex-default", CollectionId: f.mine.Id,
		Options: &pb.RunnerOptions{BehaviorProfile: "balanced-v1", Permission: "read-only", Instructions: "Extra session guidance"},
	})
	if err != nil {
		t.Fatal(err)
	}
	opts := f.codex.Session.Open
	if opened.Session.BehaviorProfile != "balanced-v1" || opts.Permission != "read-only" ||
		!strings.Contains(opts.Context, "Inspect available context before asking") || !strings.Contains(opts.Context, "Extra session guidance") {
		t.Fatalf("profile did not apply alongside the session configuration: %+v", opts)
	}
}

func TestCheckInFirstStartsReadOnlyAcrossNativeHarnesses(t *testing.T) {
	for _, provider := range []string{"codex", "claude"} {
		t.Run(provider, func(t *testing.T) {
			f := newReviewFixture(t)
			opened, err := f.service.OpenSession(f.owner, &pb.OpenSessionRequest{
				ProfileId: provider + "-default", CollectionId: f.mine.Id,
				Options: &pb.RunnerOptions{BehaviorProfile: "check-in-first-v1", Permission: "yolo"},
			})
			if err != nil {
				t.Fatal(err)
			}
			adapter := f.codex
			if provider == "claude" {
				adapter = f.claude
			}
			opts := adapter.Session.Open
			if opened.Session.Permission != "read-only" || opts.Permission != "read-only" ||
				!strings.Contains(opts.Context, "then wait for approval") {
				t.Fatalf("session permission %q, adapter permission %q, context %q", opened.Session.Permission, opts.Permission, opts.Context)
			}
		})
	}
}

func TestBehaviorVerificationCapturesInputAndRetainsEvidence(t *testing.T) {
	f := newReviewFixture(t)
	path := filepath.Join(f.mine.Members[0].Path, "untracked.go")
	if err := os.WriteFile(path, []byte("package api\n// captured\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	request := &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: "behavior-verification-v1"}
	started, err := f.client.StartReview(f.owner, request)
	if err != nil {
		t.Fatal(err)
	}
	brief := <-f.codex.Session.Applied
	if started.Run.TemplateId != request.TemplateId || started.Session.Permission != "read-only" ||
		!strings.Contains(brief.Text, "Independently verify the behavior") || !strings.Contains(brief.Text, "api/untracked.go") {
		t.Fatalf("run %+v; brief %s", started.Run, brief.Text)
	}
	if err := os.WriteFile(path, []byte("package api\n// changed afterward\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	filed, err := f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{
		SessionId: started.Session.Id, File: "api/untracked.go", StartLine: 2, Severity: "major", Title: "Example finding", Body: "Review the evidence.",
	})
	if err != nil || strings.Join(filed.GetFinding().GetSnippet(), "") != "// captured" {
		t.Fatalf("finding: %+v, %v", filed, err)
	}
	f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Criterion: unverified. Captured runtime unavailable.\nReview coverage: captured API changes."}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	events(t, f.bench, f.conversation.ID, "review_finished")
	report, err := f.client.GetArtifact(f.owner, &pb.GetArtifactRequest{CollectionId: f.mine.Id, ArtifactId: started.Run.Id})
	if err != nil {
		t.Fatal(err)
	}
	if len(report.Versions) != 1 || report.Versions[0].Schema != "lazybox.review-findings/v1" {
		t.Fatalf("result version: %+v", report.Versions)
	}
	for _, want := range []string{"lazybox.review-findings/v1", "Criterion: unverified. Captured runtime unavailable.", "verification", "captured API changes", filed.Finding.Id, "// captured"} {
		if !strings.Contains(report.Content, want) {
			t.Errorf("result lacks %q: %s", want, report.Content)
		}
	}
}

func TestVerificationExecutesCapturedTreesAndRecordsResults(t *testing.T) {
	f := newReviewFixture(t)
	path := filepath.Join(f.mine.Members[0].Path, "acceptance.txt")
	if err := os.WriteFile(path, []byte("correct"), 0600); err != nil {
		t.Fatal(err)
	}
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: "behavior-verification-v1"})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	if err := os.WriteFile(path, []byte("later edit"), 0600); err != nil {
		t.Fatal(err)
	}
	call := f.codex.Session.Open.Tools
	raw, err := call(timeout(t), "lazybox_command", json.RawMessage(`{"action":"verify","name":"preserves captured content","member":"api","expected":"acceptance.txt contains correct","argv":["sh","-c","test \"$(cat acceptance.txt)\" = correct"]}`))
	if err != nil {
		t.Fatal(err)
	}
	var check reviews.VerificationCheck
	if err := json.Unmarshal([]byte(raw), &check); err != nil {
		t.Fatal(err)
	}
	if len(check.Runs) != 1 {
		t.Fatalf("missing execution: %s", raw)
	}
	receipt := check.Runs[0]
	if receipt.CandidateCommand == "" {
		t.Fatalf("execution not started: %s", raw)
	}
	for _, id := range []string{receipt.BaseCommand, receipt.CandidateCommand} {
		args, _ := json.Marshal(map[string]string{"action": "wait", "id": id})
		if _, err := call(timeout(t), "lazybox_command", args); err != nil {
			t.Fatal(err)
		}
	}
	// Simulate a restart between command registration and saving its receipt.
	// The managed command's durable identity must restore both output links.
	if _, err := f.bench.Reviews.UpdateRun(f.mine.Id, started.Run.Id, func(run *reviews.Run) error {
		run.Verification[0].Runs[0].BaseCommand = ""
		run.Verification[0].Runs[0].CandidateCommand = ""
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Review coverage: captured content verified."}
	f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	events(t, f.bench, f.conversation.ID, "review_finished")
	report, err := f.client.GetArtifact(f.owner, &pb.GetArtifactRequest{CollectionId: f.mine.Id, ArtifactId: started.Run.Id})
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{`"state": "passed"`, `"baseExit": 1`, `"candidateExit": 0`, receipt.BaseCommand, receipt.CandidateCommand} {
		if !strings.Contains(report.Content, want) {
			t.Errorf("missing %s in %s", want, report.Content)
		}
	}
	content, err := os.ReadFile(path)
	if err != nil || string(content) != "later edit" {
		t.Fatalf("author tree changed: %q %v", content, err)
	}
}

func TestVerificationRepairsAndRerunsFrozenChecks(t *testing.T) {
	for _, pinned := range []bool{false, true} {
		name := "workspace"
		if pinned {
			name = "mission-candidate"
		}
		t.Run(name, func(t *testing.T) {
			f := newReviewFixture(t)
			started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: "behavior-verification-v1"})
			if err != nil {
				t.Fatal(err)
			}
			if pinned {
				_, err = f.bench.Reviews.UpdateRun(f.mine.Id, started.Run.Id, func(r *reviews.Run) error {
					r.ContractDigest = "approved-contract"
					r.ReviewWorkspace = f.mine.Id
					r.MemberHeads = map[string]string{"api": git(t, f.mine.Members[0].Path, "rev-parse", "HEAD")}
					return nil
				})
				if err != nil {
					t.Fatal(err)
				}
			}
			<-f.codex.Session.Applied
			call := f.codex.Session.Open.Tools
			args := json.RawMessage(`{"action":"verify","name":"repair result","member":"api","expected":"acceptance.txt contains correct","argv":["sh","-c","test \"$(cat acceptance.txt)\" = correct"]}`)
			waitCheck := func() reviews.VerificationCheck {
				t.Helper()
				raw, err := call(timeout(t), "lazybox_command", args)
				if err != nil {
					t.Fatal(err)
				}
				var check reviews.VerificationCheck
				if err := json.Unmarshal([]byte(raw), &check); err != nil {
					t.Fatal(err)
				}
				receipt := check.Runs[len(check.Runs)-1]
				for _, id := range []string{receipt.BaseCommand, receipt.CandidateCommand} {
					wait, _ := json.Marshal(map[string]string{"action": "wait", "id": id})
					if _, err := call(timeout(t), "lazybox_command", wait); err != nil {
						t.Fatal(err)
					}
				}
				return check
			}
			first := waitCheck()
			reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
			filed, err := f.client.AddReviewFinding(reviewer, &pb.AddReviewFindingRequest{SessionId: started.Session.Id, File: "api/handler.go", StartLine: 1, Severity: "blocker", Title: "Missing acceptance output", Body: "Reproduce with the recorded repair result check. Retain a regression test."})
			if err != nil {
				t.Fatal(err)
			}
			f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Review coverage: candidate check failed."}
			f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
			applied(t, f.claude, "Missing acceptance output")
			runUntil(t, f, func(r *pb.ReviewRun) bool { return r.Phase == "fixing" })
			record, err := f.bench.Checkpoints.List(f.conversation.ID)
			if err != nil {
				t.Fatal(err)
			}
			rv, err := f.bench.Reviews.Load(f.mine.Id)
			if err != nil {
				t.Fatal(err)
			}
			for _, turn := range record.Turns {
				if turn.OperationID == "review-"+strings.TrimPrefix(started.Run.Id, "rv-")+"-findings-1" && rv.Runs[0].FixingSince > turn.Before.At.Unix() {
					t.Fatalf("repair starts at %d before its findings boundary %d", turn.Before.At.Unix(), rv.Runs[0].FixingSince)
				}
			}
			if err := os.WriteFile(filepath.Join(f.mine.Members[0].Path, "acceptance.txt"), []byte("correct"), 0600); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(f.mine.Members[0].Path, "recovery_test.sh"), []byte("test \"$(cat acceptance.txt)\" = correct\n"), 0600); err != nil {
				t.Fatal(err)
			}
			if pinned {
				git(t, f.mine.Members[0].Path, "add", ".")
				git(t, f.mine.Members[0].Path, "commit", "-m", "repair acceptance")
			}
			f.claude.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
			applied(t, f.codex, "Round 2")
			second := waitCheck()
			if pinned {
				rv, err := f.bench.Reviews.Load(f.mine.Id)
				if err != nil {
					t.Fatal(err)
				}
				if got, want := rv.Runs[0].MemberHeads["api"], git(t, f.mine.Members[0].Path, "rev-parse", "HEAD"); got != want {
					t.Fatalf("repair evidence still names the old candidate: got %s want %s", got, want)
				}
			}
			regression := &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Name: "retained regression", Member: "api", Expected: "the retained recovery test passes", Argv: []string{"sh", "recovery_test.sh"}, Regression: "recovery_test.sh"}
			retained, err := f.client.RunReviewVerification(reviewer, regression)
			if err != nil {
				t.Fatal(err)
			}
			for _, id := range []string{retained.Runs[0].BaseCommandId, retained.Runs[0].CandidateCommandId} {
				wait, _ := json.Marshal(map[string]string{"action": "wait", "id": id})
				if _, err := call(timeout(t), "lazybox_command", wait); err != nil {
					t.Fatal(err)
				}
			}

			if len(second.Runs) != 2 || first.Runs[0].Candidate == second.Runs[1].Candidate || first.Runs[0].CandidateCommand == second.Runs[1].CandidateCommand {
				t.Fatalf("check not rerun on repaired input: %+v", second)
			}
			if _, err := f.bench.ReviewVerdict(started.Session.Id, filed.Finding.Id, "resolved", "Recorded check passes"); err != nil {
				t.Fatal(err)
			}
			f.codex.Session.Events <- agent.Event{Type: "message_delta", Text: "Review coverage: repaired candidate passed."}
			f.codex.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
			runUntil(t, f, func(r *pb.ReviewRun) bool { return r.StopReason == "clean" && r.Round == 2 })
			report, err := f.client.GetArtifact(f.owner, &pb.GetArtifactRequest{CollectionId: f.mine.Id, ArtifactId: started.Run.Id})
			if err != nil {
				t.Fatal(err)
			}
			if len(report.Versions) != 2 || !strings.Contains(report.Content, "repaired candidate passed") {
				t.Fatalf("round evidence missing: %+v", report)
			}
		})
	}
}

func TestVerificationRPCScopesRecipesAndCancelsProcesses(t *testing.T) {
	f := newReviewFixture(t)
	started, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, TemplateId: "behavior-verification-v1"})
	if err != nil {
		t.Fatal(err)
	}
	<-f.codex.Session.Applied
	request := &pb.RunReviewVerificationRequest{SessionId: started.Session.Id, Name: "long check", Member: "api", Expected: "command completes", Argv: []string{"sh", "-c", "sleep 30"}, Regression: "Retain the timeout scenario in the repository tests"}
	author := bearer(timeout(t), f.agentToken)
	if _, err := f.client.RunReviewVerification(author, request); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("author ran reviewer recipe: %v", err)
	}
	reviewer := bearer(timeout(t), agentToken(t, f.codex.Session.Open.Env))
	check, err := f.client.RunReviewVerification(reviewer, request)
	if err != nil {
		t.Fatal(err)
	}
	again, err := f.client.RunReviewVerification(reviewer, request)
	if err != nil || again.Runs[0].CandidateCommandId != check.Runs[0].CandidateCommandId {
		t.Fatalf("retry duplicated execution: %+v %v", again, err)
	}
	request.Argv = []string{"true"}
	if _, err := f.client.RunReviewVerification(reviewer, request); err == nil {
		t.Fatal("replaced frozen check")
	}
	if _, _, err := f.bench.CancelReview(timeout(t), f.mine.Id, started.Run.Id, "You"); err != nil {
		t.Fatal(err)
	}
	for _, id := range []string{check.Runs[0].BaseCommandId, check.Runs[0].CandidateCommandId} {
		command, err := f.bench.Commands.Get(id)
		if err != nil {
			t.Fatal(err)
		}
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		status, err := command.Wait(ctx)
		cancel()
		if err != nil || !status.Done {
			t.Fatalf("verification still running after cancel: %+v %v", status, err)
		}
	}
}

func TestConcurrentReviewStartsShareOneActiveScope(t *testing.T) {
	f := newReviewFixture(t)
	type result struct {
		response *pb.StartReviewResponse
		err      error
	}
	results := make(chan result, 2)
	for _, key := range []string{"from-chat", "from-changes"} {
		go func() {
			response, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID, RequestKey: key})
			results <- result{response, err}
		}()
	}
	var id string
	for range 2 {
		r := <-results
		if r.err != nil {
			if !strings.Contains(r.err.Error(), "already") {
				t.Fatal(r.err)
			}
			continue
		}
		if id != "" {
			t.Fatal("two reviewers started")
		}
		id = r.response.Run.Id
	}
	if id == "" {
		t.Fatal("neither reviewer started")
	}
	if _, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID}); err == nil || !strings.Contains(err.Error(), "in progress") {
		t.Fatalf("active review was not reported: %v", err)
	}
	if _, err := f.client.CancelReview(f.owner, &pb.CancelReviewRequest{CollectionId: f.mine.Id, ReviewId: id}); err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID}); err != nil {
		t.Fatal(err)
	}
}

func TestMissingReviewerDoesNotPreventNewReview(t *testing.T) {
	f := newReviewFixture(t)
	_, err := f.bench.Reviews.CreateRun(f.mine.Id, reviews.Run{
		ReviewerSession: "missing-reviewer", ReviewerLabel: "Codex",
		SourceSession: f.conversation.ID, Scope: reviews.ScopeChanges,
		State: reviews.RunRunning, Loop: reviews.LoopAsk, Phase: reviews.PhaseReviewing,
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := f.client.StartReview(f.owner, &pb.StartReviewRequest{CollectionId: f.mine.Id, SourceSessionId: f.conversation.ID}); err != nil {
		t.Fatal(err)
	}
}
