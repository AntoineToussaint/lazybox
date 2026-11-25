package transport

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func approveWorkflow(t *testing.T, s *Service, id, grant string) {
	t.Helper()
	ctx := context.Background()
	if _, err := s.InstallExampleExtension(ctx, &pb.InstallExampleExtensionRequest{Example: id}); err != nil {
		t.Fatal(err)
	}
	if _, err := s.ApproveExtension(ctx, &pb.ApproveExtensionRequest{Id: id, Grants: []string{grant}}); err != nil {
		t.Fatal(err)
	}
}

func TestCloneMeRemembersOnlyConfirmedPreferences(t *testing.T) {
	s, a := collectionService(t)
	ctx := context.Background()
	ws := createTestCollection(t, s)
	if _, err := s.GetCloneProfile(ctx, &pb.GetCloneProfileRequest{}); err == nil {
		t.Fatal("profile accessible before enabling")
	}
	approveWorkflow(t, s, "clone-me", power.GrantClone)
	if _, err := s.SaveCloneProfile(ctx, &pb.SaveCloneProfileRequest{Profile: &pb.CloneProfile{Style: "Prefer explicit errors.", AskWhen: "Always ask before pushing."}}); err != nil {
		t.Fatal(err)
	}
	opened, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: ws.Id})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(a.Session.Open.Context, "Prefer explicit errors.") {
		t.Fatal("missing confirmed profile")
	}
	e, _ := s.Bench.Get(opened.Session.Id)
	s.Bench.ObserveHookEvents()
	request := agent.Event{Type: "question", RequestID: "first", Data: []byte(`{"questions":[{"id":"style","question":"Naming convention?"}]}`)}
	waitFor := func(check func() bool) {
		t.Helper()
		deadline := time.Now().Add(3 * time.Second)
		for !check() {
			if time.Now().After(deadline) {
				t.Fatal("timed out waiting for question")
			}
			time.Sleep(time.Millisecond)
		}
	}
	lookup := func(native string) string {
		for id := range e.Engine.Snapshot().Pending {
			if strings.HasSuffix(id, ":"+native) || id == native {
				return id
			}
		}
		return ""
	}
	a.Session.Events <- request
	waitFor(func() bool { return lookup("first") != "" })
	s.Bench.ObserveHookEvents()
	_, err = s.Apply(ctx, &pb.ApplyRequest{SessionId: e.ID, OperationId: "teach-clone", Operation: &pb.ApplyRequest_Answer{Answer: &pb.Answer{RequestId: lookup("first"), Answers: map[string]*pb.Answers{"style": {Values: []string{"camelCase"}}}, RememberForClone: true}}})
	if err != nil {
		t.Fatal(err)
	}
	<-a.Session.Applied
	a.Session.Events <- agent.Event{Type: "request_resolved", RequestID: "first"}
	waitFor(func() bool { return lookup("first") == "" })
	// A second workspace may confirm a different answer to the identical question.
	if err := s.Bench.RememberCloneAnswer("other-workspace", request, map[string][]string{"style": {"snake_case"}}); err != nil {
		t.Fatal(err)
	}

	consult := archiveAdapter{make(chan *agenttest.Session, 1)}
	s.Bench.Adapters["claude"] = consult
	request.RequestID = "again"
	a.Session.Events <- request
	waitFor(func() bool { return lookup("again") != "" })
	s.Bench.ObserveHookEvents()
	select {
	case assistant := <-consult.opened:
		if !strings.Contains(assistant.Open.Context, "Always ask before pushing.") {
			t.Fatal("missing ask-when rule")
		}
		prompt := <-assistant.Applied
		if !strings.Contains(prompt.Text, "authorization, disclosure, deployment, spending or destructive changes") {
			t.Fatal("missing exclusions")
		}
		assistant.Events <- agent.Event{Type: "message_delta", Text: "REUSE"}
		assistant.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	case <-time.After(3 * time.Second):
		t.Fatal("remembered answer bypassed consultation")
	}
	select {
	case op := <-a.Session.Applied:
		if op.Kind != agent.Answer || op.RequestID != "again" || op.Origin == nil || op.Origin.From != "Clone Me" || op.Answers["style"][0] != "camelCase" {
			t.Fatalf("wrong response %+v", op)
		}
	case <-time.After(time.Second):
		t.Fatal("clone did not answer")
	}
	profile, err := s.GetCloneProfile(ctx, &pb.GetCloneProfileRequest{})
	if err != nil || len(profile.RememberedAnswers) != 2 {
		t.Fatalf("remembered answer list: %v %v", profile, err)
	}
	var remove string
	for _, a := range profile.RememberedAnswers {
		if a.Workspace == ws.Name && a.Answer == "camelCase" {
			remove = a.Id
		}
	}
	if remove == "" {
		t.Fatal("missing workspace-scoped camelCase answer")
	}
	if _, err = s.SaveCloneProfile(ctx, &pb.SaveCloneProfileRequest{Profile: profile, ForgetAnswerIds: []string{remove}}); err != nil {
		t.Fatal(err)
	}
	profile, err = s.GetCloneProfile(ctx, &pb.GetCloneProfileRequest{})
	if err != nil || len(profile.RememberedAnswers) != 1 || profile.RememberedAnswers[0].Answer != "snake_case" {
		t.Fatalf("answer not forgotten: %v %v", profile, err)
	}
	request.Type = "approval"
	request.RequestID = "approve"
	a.Session.Events <- request
	waitFor(func() bool { return lookup("approve") != "" })
	s.Bench.ObserveHookEvents()
	select {
	case op := <-a.Session.Applied:
		t.Fatalf("unexpected operation %+v", op)
	default:
	}
}

type archiveAdapter struct{ opened chan *agenttest.Session }

func (a archiveAdapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	var fake agenttest.Adapter
	s, err := fake.Open(ctx, o)
	if err == nil {
		a.opened <- fake.Session
	}
	return s, err
}

func TestCompoundEngineeringReviewsAnArchiveOnce(t *testing.T) {
	for _, scope := range []string{"conversation", "workspace"} {
		t.Run(scope, func(t *testing.T) {
			s, _ := collectionService(t)
			ctx := context.Background()
			ws := createTestCollection(t, s)
			approveWorkflow(t, s, "compound-engineering", power.GrantCompound)
			for dir, rule := range map[string]string{"checks": "Run the narrow verifier first.", "unrelated": "Unrelated directory rule."} {
				folder := filepath.Join(ws.Root, "api", dir)
				if err := os.MkdirAll(folder, 0755); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(filepath.Join(folder, "AGENTS.md"), []byte(rule), 0644); err != nil {
					t.Fatal(err)
				}
			}
			a := archiveAdapter{make(chan *agenttest.Session, 4)}
			s.Bench.Adapters["claude"] = a
			opened, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: ws.Id})
			if err != nil {
				t.Fatal(err)
			}
			source := <-a.opened
			e, _ := s.Bench.Get(opened.Session.Id)
			if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "lesson", Kind: agent.Message, Text: "Always verify changes to api/checks/verify.go with the repository gate."}); err != nil {
				t.Fatal(err)
			}
			<-source.Applied
			if scope == "workspace" {
				if err := e.Engine.Close(); err != nil {
					t.Fatal(err)
				}
			}
			for range 2 {
				if scope == "conversation" {
					_, err = s.ArchiveConversation(ctx, &pb.ArchiveConversationRequest{SessionId: e.ID, Archived: true})
				} else {
					_, err = s.ArchiveCollection(ctx, &pb.CollectionRequest{CollectionId: ws.Id})
				}
				if err != nil {
					t.Fatal(err)
				}
			}
			var reviewer *agenttest.Session
			select {
			case reviewer = <-a.opened:
			case <-time.After(3 * time.Second):
				t.Fatal("archive did not start review")
			}
			if reviewer.Open.Permission != "read-only" || !strings.Contains(reviewer.Open.Context, "repository gate") {
				t.Fatalf("unexpected reviewer context: %s", reviewer.Open.Context)
			}
			if scope == "conversation" && (!strings.Contains(reviewer.Open.Context, "Current api/checks/AGENTS.md") || !strings.Contains(reviewer.Open.Context, "Run the narrow verifier first.") || strings.Contains(reviewer.Open.Context, "Unrelated directory rule.")) {
				t.Fatalf("archive review missing scoped evidence: %s", reviewer.Open.Context)
			}
			<-reviewer.Applied
			reviewer.Events <- agent.Event{Type: "message_delta", Text: "Add a verification instruction to AGENTS.md."}
			reviewer.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
			deadline := time.Now().Add(3 * time.Second)
			for {
				docs, err := s.Bench.Artifacts.List(ws.Id)
				if err != nil {
					t.Fatal(err)
				}
				if len(docs) == 1 && strings.Contains(docs[0].Title, "Compound Engineering") {
					break
				}
				if time.Now().After(deadline) {
					t.Fatalf("missing proposal: %+v", docs)
				}
				time.Sleep(time.Millisecond)
			}
			select {
			case <-a.opened:
				t.Fatal("duplicate review")
			default:
			}
		})
	}
}

func TestCloneMeInterviewAndNewQuestionConsultation(t *testing.T) {
	s, _ := collectionService(t)
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	ws := createTestCollection(t, s)
	approveWorkflow(t, s, "clone-me", power.GrantClone)
	if _, err := s.SaveCloneProfile(ctx, &pb.SaveCloneProfileRequest{Profile: &pb.CloneProfile{Style: "Use snake_case for variable names."}}); err != nil {
		t.Fatal(err)
	}
	a := archiveAdapter{make(chan *agenttest.Session, 4)}
	s.Bench.Adapters["claude"] = a
	opened, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: ws.Id})
	if err != nil {
		t.Fatal(err)
	}
	source := <-a.opened
	e, _ := s.Bench.Get(opened.Session.Id)
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: "pref", Kind: agent.Message, Text: "I prefer table-driven tests."}); err != nil {
		t.Fatal(err)
	}
	<-source.Applied
	finish := func(reply string) {
		t.Helper()
		var reviewer *agenttest.Session
		select {
		case reviewer = <-a.opened:
		case <-ctx.Done():
			t.Fatal("missing assistant")
		}
		select {
		case <-reviewer.Applied:
		case <-ctx.Done():
			t.Fatal("assistant did not get a question")
		}
		reviewer.Events <- agent.Event{Type: "message_delta", Text: reply}
		reviewer.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	}
	result := make(chan *pb.CloneProfile, 1)
	go func() {
		p, _ := s.GetCloneProfile(ctx, &pb.GetCloneProfileRequest{LearnFromHistory: true})
		result <- p
	}()
	finish(`{"suggestions":["Prefer table-driven tests."],"questions":["Which paths should tests cover?"]}`)
	p := <-result
	if p == nil || len(p.Questions) != 1 || len(p.Suggestions) != 1 || strings.Contains(p.Style, "table-driven") {
		t.Fatalf("interview should propose before saving: %v", p)
	}
	s.Bench.ObserveHookEvents()
	source.Events <- agent.Event{Type: "question", RequestID: "naming", Data: []byte(`{"questions":[{"id":"style","question":"Which variable naming convention?","options":[{"label":"snake_case"},{"label":"camelCase"}]}]}`)}
	finish("snake_case")
	select {
	case op := <-source.Applied:
		if op.Kind != agent.Answer || op.Answers["style"][0] != "snake_case" || op.Origin.From != "Clone Me" {
			t.Fatalf("unexpected response %+v", op)
		}
	case <-ctx.Done():
		t.Fatal("clone did not return the consulted answer")
	}
}

func TestCloneNeedsInputHookSurvivesShutdownDuringConsultation(t *testing.T) {
	f := newPowerFixture(t, "hooks:\n  - id: notify\n    on: agent.needs-input\n    run: [/usr/bin/true]\n")
	b := f.service.Bench
	approveWorkflow(t, f.service, "clone-me", power.GrantClone)
	if _, err := f.service.SaveCloneProfile(context.Background(), &pb.SaveCloneProfileRequest{Profile: &pb.CloneProfile{Style: "Use snake_case"}}); err != nil {
		t.Fatal(err)
	}
	consult := archiveAdapter{make(chan *agenttest.Session, 1)}
	b.Adapters["claude"] = consult
	b.ObserveHookEvents()
	f.fake.Session.Events <- agent.Event{Type: "question", RequestID: "pending", Data: []byte(`{"questions":[{"id":"style","question":"Which naming convention?","options":[{"label":"snake_case"}]}]}`)}
	select {
	case assistant := <-consult.opened:
		<-assistant.Applied
	case <-time.After(5 * time.Second):
		t.Fatal("consultation did not start")
	}
	runs := f.runs(t, settled(1))
	if runs[0].Event != power.EventNeedsInput || runs[0].SessionId != f.session {
		t.Fatal(runs)
	}
	if err := b.Close(); err != nil {
		t.Fatal(err)
	}
}
