package runner

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
)

// startFixture is requestFixture's workspace and agent, with the asking
// agent's own native session kept (a started conversation opens another).
func startFixture(t *testing.T) (*requestFixture, *agenttest.Session) {
	t.Helper()
	f := newRequestFixture(t)
	f.b.conversationStarts.grace = 20 * time.Millisecond
	return f, f.fake.Session
}

func (f *requestFixture) askStart(t *testing.T, ask ConversationStartAsk) ConversationStart {
	t.Helper()
	ask.Session = f.agent.ID
	r, err := f.b.RequestConversationStart(context.Background(), ask)
	if err != nil {
		t.Fatal(err)
	}
	return r
}

// appliedOp waits for the operation a native session receives with id.
func appliedOp(t *testing.T, s *agenttest.Session, id string) agent.Operation {
	t.Helper()
	deadline := time.After(5 * time.Second)
	for {
		select {
		case op := <-s.Applied:
			if op.ID == id {
				return op
			}
		case <-deadline:
			t.Fatalf("no operation %q", id)
		}
	}
}

// Every coding agent is told, in one line of its briefing, how to start
// another conversation and that the person approves it in one click.
func TestEveryAgentIsToldHowToStartAConversation(t *testing.T) {
	f, _ := startFixture(t)
	brief := f.b.briefing()
	if !strings.Contains(brief, "lazybox conversation new --title") || !strings.Contains(brief, "the person approves it in one click") {
		t.Fatalf("briefing: %s", brief)
	}
	if !strings.Contains(brief, "## Your Lazybox workspace") {
		t.Fatal("the command briefing is gone")
	}
}

// Without a grant, asking starts nothing: the request waits in the asking
// conversation. Start opens a peer conversation with the brief and who asked
// for it; the waiting agent collects the answer. Decline tells the agent why.
func TestAnAgentAsksThePersonToStartAConversation(t *testing.T) {
	f, asker := startFixture(t)
	ctx := context.Background()
	r := f.askStart(t, ConversationStartAsk{Title: "Review every module", Brief: "Review each module; done when each has a findings list.", Reason: "the person asked for it in chat", Model: "fable-5.2"})
	if r.State != StartPending || r.Mode != "same" || r.Profile != "claude-default" || r.Provider != "claude" || r.Model != "claude-fable-5-2" || r.SharedWith != 1 || r.From == "" {
		t.Fatalf("request: %+v", r)
	}
	if pending := f.b.PendingConversationStarts(f.agent); len(pending) != 1 || pending[0].ID != r.ID {
		t.Fatalf("pending: %+v", pending)
	}
	before := len(f.b.List())

	// The agent waits; the person starts it, with a better title.
	waited := make(chan ConversationStart, 1)
	go func() {
		got, err := f.b.WaitConversationStart(ctx, r.ID, 5*time.Second)
		if err != nil {
			t.Error(err)
		}
		waited <- got
	}()
	until(t, "the agent waits", func() bool {
		f.b.conversationStarts.mu.Lock()
		defer f.b.conversationStarts.mu.Unlock()
		return f.b.conversationStarts.waiting[r.ID] == 1
	})
	started, err := f.b.AnswerConversationStart(ctx, r.ID, ConversationStartAnswer{Decision: "start", Title: "Review all modules"})
	if err != nil {
		t.Fatal(err)
	}
	if started.State != StartStarted || started.StartedSession == "" || started.StartedTitle != "Review all modules" || started.StartedWorkspace != f.workspace.ID {
		t.Fatalf("started: %+v", started)
	}
	if len(f.b.List()) != before+1 {
		t.Fatal("no conversation was opened")
	}
	peer := f.fake.Session
	if peer == asker {
		t.Fatal("the peer is the asker")
	}
	brief := appliedOp(t, peer, "conversation-start-"+r.ID)
	if brief.Origin == nil || brief.Origin.Kind != agent.OriginConversationBrief || brief.Origin.Conversation != conversationOf(f.agent) ||
		!strings.Contains(brief.Text, "Review each module") || !strings.Contains(brief.Text, "at the person's request: the person asked for it in chat") || !strings.Contains(brief.Text, "not its sub-agent") {
		t.Fatalf("brief: %+v", brief)
	}
	e, err := f.b.Get(started.StartedSession)
	if err != nil {
		t.Fatal(err)
	}
	if e.CollectionID != f.workspace.ID || e.Options.Permission != f.agent.Options.Permission || e.Options.Model != "claude-fable-5-2" {
		t.Fatalf("peer: workspace %s permission %q model %q", e.CollectionID, e.Options.Permission, e.Options.Model)
	}
	// A peer, not a child: no lineage.
	if f.b.Work != nil {
		if _, child := f.b.Work.ParentOf(conversationOf(e)); child {
			t.Fatal("a peer conversation has a parent")
		}
	}
	got := <-waited
	if got.State != StartStarted || !got.Told {
		t.Fatalf("waited: %+v", got)
	}
	if _, err = f.b.AnswerConversationStart(ctx, r.ID, ConversationStartAnswer{Decision: "decline"}); err == nil || !strings.Contains(err.Error(), "already started") {
		t.Fatalf("answered twice: %v", err)
	}
	if len(f.b.PendingConversationStarts(f.agent)) != 0 {
		t.Fatal("still pending")
	}

	// Declined while the agent does not wait: it is told, with the reason.
	declined := f.askStart(t, ConversationStartAsk{Title: "Rewrite the parser", Brief: "From scratch."})
	if _, err = f.b.AnswerConversationStart(ctx, declined.ID, ConversationStartAnswer{Decision: "decline", Reason: "not now"}); err != nil {
		t.Fatal(err)
	}
	note := appliedOp(t, asker, "conversation-note-"+declined.ID)
	if note.Origin == nil || note.Origin.Kind != agent.OriginConversationDeclined || !strings.Contains(note.Text, "declined starting the conversation “Rewrite the parser”. Their reason: not now") {
		t.Fatalf("note: %+v", note)
	}
	if got, _ := f.b.GetConversationStart(declined.ID); got.State != StartDeclined || got.DeclineReason != "not now" || !got.Told {
		t.Fatalf("declined: %+v", got)
	}
}

// At most three requests wait per conversation; the same title asked again
// is the same request; a malformed one is refused; an unanswered one expires.
func TestConversationStartRequestsAreBounded(t *testing.T) {
	f, _ := startFixture(t)
	ctx := context.Background()
	first := f.askStart(t, ConversationStartAsk{Title: "One", Brief: "b"})
	if again := f.askStart(t, ConversationStartAsk{Title: "one", Brief: "other words"}); again.ID != first.ID {
		t.Fatal("the same title asked twice is two requests")
	}
	f.askStart(t, ConversationStartAsk{Title: "Two", Brief: "b"})
	f.askStart(t, ConversationStartAsk{Title: "Three", Brief: "b"})
	if _, err := f.b.RequestConversationStart(ctx, ConversationStartAsk{Session: f.agent.ID, Title: "Four", Brief: "b"}); !errors.Is(err, ErrConversationStartLimit) {
		t.Fatalf("a fourth: %v", err)
	}
	for _, bad := range []ConversationStartAsk{
		{Title: "", Brief: "b"},
		{Title: "No brief"},
		{Title: "Two\nlines", Brief: "b"},
		{Title: "Where", Brief: "b", Workspace: "elsewhere"},
		{Title: "Who", Brief: "b", Provider: "nobody"},
		{Title: "Which", Brief: "b", Artifact: "missing"},
		{Title: "Long", Brief: strings.Repeat("x", maxStartBrief+1)},
	} {
		bad.Session = f.agent.ID
		if _, err := f.b.RequestConversationStart(ctx, bad); !errors.Is(err, ErrInvalidConversationStart) {
			t.Errorf("%+v: %v", bad, err)
		}
	}
	if _, err := f.b.AnswerConversationStart(ctx, first.ID, ConversationStartAnswer{Decision: "maybe"}); !errors.Is(err, ErrInvalidConversationStart) {
		t.Fatalf("a decision that is neither: %v", err)
	}
	f.b.conversationStarts.now = func() time.Time { return time.Now().Add(conversationStartTTL + time.Minute) }
	if got, _ := f.b.GetConversationStart(first.ID); got.State != StartExpired {
		t.Fatalf("not expired: %+v", got)
	}
	if _, err := f.b.AnswerConversationStart(ctx, first.ID, ConversationStartAnswer{Decision: "start"}); err == nil {
		t.Fatal("started an expired request")
	}
}

// A brief published as an artifact goes with the conversation: in this
// workspace by reference, in a new workspace as a copy there.
func TestABriefArtifactGoesWithTheConversation(t *testing.T) {
	f, _ := startFixture(t)
	ctx := context.Background()
	doc, err := f.b.Artifacts.Put(f.workspace.ID, artifacts.Put{Title: "Module review spec", Kind: "spec", Format: "json", Content: `{"schema":"spec.v1","problem":"Review","requirements":["Every module"],"acceptanceCriteria":["Coverage"]}`, Author: artifacts.Author{Kind: "agent", SessionID: f.agent.ID}})
	if err != nil {
		t.Fatal(err)
	}
	r := f.askStart(t, ConversationStartAsk{Title: "Review modules", Artifact: doc.ID, Workspace: "new"})
	if r.ArtifactTitle != "Module review spec" || r.ArtifactVersion != 1 || r.Mode != "new" {
		t.Fatalf("request: %+v", r)
	}
	if _, err := f.b.Artifacts.Put(f.workspace.ID, artifacts.Put{ID: doc.ID, Content: strings.Replace(doc.Content, "Every module", "One module", 1)}); err != nil {
		t.Fatal(err)
	}
	started, err := f.b.AnswerConversationStart(ctx, r.ID, ConversationStartAnswer{Decision: "start"})
	if err != nil {
		t.Fatal(err)
	}
	if started.StartedWorkspace == "" || started.StartedWorkspace == f.workspace.ID {
		t.Fatalf("not a new workspace: %+v", started)
	}
	copies, err := f.b.Artifacts.List(started.StartedWorkspace)
	if err != nil || len(copies) != 1 || copies[0].Title != "Module review spec" {
		t.Fatalf("copied: %+v %v", copies, err)
	}
	copy, err := f.b.Artifacts.Get(started.StartedWorkspace, copies[0].ID, 1)
	if err != nil || copy.Format != "json" || copy.Content != doc.Content {
		t.Fatalf("approved typed brief: %+v %v", copy, err)
	}
	brief := appliedOp(t, f.fake.Session, "conversation-start-"+r.ID)
	if !strings.Contains(brief.Text, "lazybox artifact get "+copies[0].ID+" --version 1") {
		t.Fatalf("brief does not name the copy: %s", brief.Text)
	}
}

func until(t *testing.T, what string, ok func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for !ok() {
		if time.Now().After(deadline) {
			t.Fatal(what)
		}
		time.Sleep(5 * time.Millisecond)
	}
}
