package main

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// pendingStart is the asking conversation's waiting request to start one, or nil.
func (r *testRunner) pendingStart(session string) *pb.AgentEvent {
	for _, p := range r.session(session).Pending {
		if p.Type == "conversation_request" {
			return p
		}
	}
	return nil
}

// nextBrief is the first message of the next conversation the Runner opens.
func (r *testRunner) nextBrief(t *testing.T) (*agenttest.Session, string) {
	t.Helper()
	select {
	case native := <-r.provider.opened:
		select {
		case op := <-native.Applied:
			return native, op.Text
		case <-time.After(5 * time.Second):
			t.Fatal("the new conversation got no brief")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("no conversation was opened")
	}
	return nil, ""
}

// What the user hit in the field: the person asks an agent, in its chat, to
// start a conversation. The agent asks; the person approves in one click and
// a peer conversation starts with the brief; or declines; or has not answered
// yet. `agents spawn` without a grant asks the same way instead of failing.
func TestAnAgentStartsAConversationThePersonApproves(t *testing.T) {
	r := newTestRunner(t)
	asker, native := r.open("Planner")
	env := agentEnv(t, native, asker.Id)

	if out := r.cli(env, "", "do", "conversation.new"); out.code != exitDenied || !strings.Contains(out.err.Error(), "lazybox conversation new --title") || r.pendingStart(asker.Id) != nil {
		t.Fatalf("agent window action should point to the request command without starting a conversation: %+v", out)
	}

	if out := r.cli(env, "", "conversation", "new", "--title", "Review all modules"); out.code != exitUsage || !strings.Contains(out.err.Error(), "--brief") {
		t.Fatalf("no brief: %+v", out)
	}

	// Approved: the command prints the new conversation and exits 0.
	done := make(chan result, 1)
	go func() {
		done <- r.cli(env, "", "conversation", "new", "--title", "Review all modules", "--brief", "Review every module; done when each has a findings list.", "--reason", "you asked in chat", "--timeout", "15s")
	}()
	until(t, "the request waits in the conversation", func() bool { return r.pendingStart(asker.Id) != nil })
	if out := r.cli(r.owner(), "", "inbox"); !strings.Contains(out.stdout, "Conversation: Start a conversation: Review all modules") {
		t.Fatalf("inbox: %+v", out)
	}
	if out := r.cli(r.owner(), "", "approve", r.pendingStart(asker.Id).RequestId); out.code != 0 || !strings.Contains(out.stdout, "Started “Review all modules”") {
		t.Fatalf("approve: %+v", out)
	}
	out := <-done
	if out.code != 0 || !strings.Contains(out.stdout, "Asked the person to start “Review all modules”") || !strings.Contains(out.stdout, "Started Review all modules (conversation ") || !strings.Contains(out.stdout, "in this workspace") {
		t.Fatalf("started: %+v", out)
	}
	peer, brief := r.nextBrief(t)
	if !strings.Contains(brief, "Review every module") || !strings.Contains(brief, "The conversation “Planner” asked to start this one, at the person's request: you asked in chat") {
		t.Fatalf("brief: %s", brief)
	}
	if peer.Open.Cwd != native.Open.Cwd {
		t.Fatal("a peer in the same workspace works elsewhere")
	}

	// Declined: exit 4 with the reason, read from stdin this time.
	go func() {
		done <- r.cli(env, "Rewrite the parser from scratch.", "conversation", "new", "--title", "Rewrite the parser", "--brief", "-", "--timeout", "15s")
	}()
	until(t, "the second request", func() bool { return r.pendingStart(asker.Id) != nil })
	if out := r.cli(r.owner(), "", "deny", "Planner"); out.code != 0 {
		t.Fatalf("deny: %+v", out)
	}
	if out = <-done; out.code != exitDenied || !strings.Contains(out.err.Error(), "the person declined starting “Rewrite the parser”") {
		t.Fatalf("declined: %+v", out)
	}

	// No answer in time: exit 5; the request stays open.
	if out = r.cli(env, "", "conversation", "new", "--title", "Write the docs", "--brief", "Docs.", "--timeout", "1s"); out.code != exitTimeout || !strings.Contains(out.err.Error(), "no answer yet") {
		t.Fatalf("no answer: %+v", out)
	}
	// `agents spawn` without a grant asks too, instead of failing.
	if out = r.cli(env, "", "agents", "spawn", "--title", "Index the corpus", "--brief", "An index.", "--no-wait"); out.code != 0 || !strings.Contains(out.stderr, "No Orchestrator grant") || !strings.Contains(out.stdout, "Asked the person to start “Index the corpus”") {
		t.Fatalf("spawn without a grant: %+v", out)
	}
	// At most three wait at once.
	if out = r.cli(env, "", "conversation", "new", "--title", "Fix the build", "--brief", "b", "--no-wait"); out.code != 0 {
		t.Fatalf("third: %+v", out)
	}
	if out = r.cli(env, "", "conversation", "new", "--title", "One more", "--brief", "b", "--no-wait"); out.code == 0 || !strings.Contains(out.err.Error(), "already wait for the person") {
		t.Fatalf("fourth: %+v", out)
	}
}

// With an Orchestrator grant in force, nothing is asked: the agent starts at
// once under the grant, as `lazybox agents spawn` does.
func TestConversationNewUnderAGrantStartsAnAgentOfTheMission(t *testing.T) {
	r := newTestRunner(t)
	ctx := context.Background()
	orch, native := r.open("Orchestrator")
	env := agentEnv(t, native, orch.Id)
	mission, err := r.service.CreateMission(ctx, &pb.CreateMissionRequest{Title: "Ship search"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = r.service.UpdateMission(ctx, &pb.UpdateMissionRequest{MissionId: mission.Id, Change: &pb.UpdateMissionRequest_Grant{Grant: &pb.OrchestratorGrantChange{SessionId: orch.Id, Ceiling: &pb.OrchestratorCeiling{MaxChildren: 2}}}}); err != nil {
		t.Fatal(err)
	}
	out := r.cli(env, "", "conversation", "new", "--title", "Index the corpus", "--brief", "An index.")
	if out.code != 0 || !strings.Contains(out.stdout, "Started Index the corpus (conversation ") || !strings.Contains(out.stdout, "as an agent of your mission “Ship search”") {
		t.Fatalf("under a grant: %+v", out)
	}
	if r.pendingStart(orch.Id) != nil {
		t.Fatal("asked the person although the grant allows it")
	}
	if _, brief := r.nextBrief(t); !strings.Contains(brief, "An index.") || !strings.Contains(brief, "from your orchestrator") {
		t.Fatalf("child brief: %s", brief)
	}
}

func TestConversationStartOnRunnerWithoutRequestEndpoint(t *testing.T) {
	server := httptest.NewServer(http.NotFoundHandler())
	defer server.Close()
	endpoint := filepath.Join(t.TempDir(), "endpoint.json")
	if err := os.WriteFile(endpoint, []byte(`{"url":"`+server.URL+`","token":"t"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	out := runPower(context.Background(), t, t.TempDir(), map[string]string{
		"LAZYBOX_V2_ENDPOINT": endpoint, "LAZYBOX_V2_SESSION_ID": "fixture",
	}, "", "conversation", "new", "--title", "TODO", "--brief", "Maintain the workspace TODO list.", "--no-wait")
	if out.code != exitUnavailable || !strings.Contains(out.err.Error(), "same Lazybox release") {
		t.Fatalf("unavailable request endpoint: %+v", out)
	}
}
