package integration

import (
	"context"
	"encoding/json"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
)

// Editing a message: a Claude conversation rewound to its second message
// knows the first turn and not the second, in the same conversation, and so
// does a second edit of the revised message.
func TestRealClaudeEditedMessageKeepsOnlyEarlierTurns(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_REAL") != "1" {
		t.Skip("requires explicit live-provider opt-in")
	}
	if selected := os.Getenv("LAZYBOX_V2_REAL_PROVIDER"); selected != "" && selected != "claude" {
		t.Skip("different provider selected")
	}
	b, err := runner.New(runner.Config{State: t.TempDir(), Worker: os.Getenv("LAZYBOX_V2_WORKER")})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()
	// ended is the provider's id of the last message of each turn, by prompt.
	ended := map[string]string{}
	// turn sends a prompt and returns its cursor in the conversation and the reply.
	turn := func(e *runner.Execution, text string) (uint64, string) {
		t.Helper()
		id := commands.ID()
		if _, err := e.Engine.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: text}); err != nil {
			t.Fatal(err)
		}
		var prompt uint64
		var reply strings.Builder
		for cursor := uint64(0); ; {
			r, err := e.Engine.Events(ctx, cursor)
			if err != nil {
				t.Fatal(err)
			}
			cursor = r.Sequence
			var event agent.Event
			if err := json.Unmarshal(r.Value, &event); err != nil {
				t.Fatal(err)
			}
			switch {
			case event.Type == "operation_accepted" && event.OperationID == id:
				prompt = cursor
			case prompt == 0:
			case event.Type == "failure":
				t.Fatalf("provider failed: %s %s", event.Status, event.Text)
			case event.Type == "message_delta" && (event.AgentID == "" || event.AgentID == "root"):
				reply.WriteString(event.Text)
			case event.Type == "usage" && (event.AgentID == "" || event.AgentID == "root"):
				var usage struct {
					RequestID string `json:"requestId"`
				}
				if json.Unmarshal(event.Data, &usage) == nil && usage.RequestID != "" {
					ended[text] = usage.RequestID
				}
			case event.Type == "turn_ended":
				if event.Status != "completed" || event.TurnID == "" {
					t.Fatalf("turn ended %q with fork point %q", event.Status, event.TurnID)
				}
				// The cursor in the conversation's timeline, which earlier attempts precede.
				whole, err := b.Retained(e.ID)
				if err != nil {
					t.Fatal(err)
				}
				return whole - e.Retained() + prompt, reply.String()
			}
		}
	}
	source, err := b.OpenEmptyWorkspace("claude-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	kept, dropped := "KEPT-"+commands.ID(), "DROPPED-"+commands.ID()
	firstPrompt := "Remember this word: " + kept + ". Reply only with OK."
	turn(source, firstPrompt)
	second, _ := turn(source, "Remember this second word: "+dropped+". Reply only with OK.")
	const ask = "Reply only with every word I asked you to remember in this conversation, or NONE."
	reference := source.Engine.Snapshot().Reference
	check := func(e *runner.Execution, label string) uint64 {
		t.Helper()
		if e.Engine.Snapshot().Reference == reference || e.Saved.ConversationID != source.Saved.ConversationID || e.Saved.ForkedFrom != "" {
			t.Fatalf("%s is not the same conversation on a session of its own: %+v", label, e.Saved)
		}
		cursor, reply := turn(e, ask)
		if !strings.Contains(reply, kept) || strings.Contains(reply, dropped) {
			t.Fatalf("%s answered %q: want the first turn only", label, reply)
		}
		return cursor
	}
	edited, err := b.RewindConversation(ctx, source.ID, second)
	if err != nil {
		t.Fatal(err)
	}
	// The edited conversation can be stopped and resumed before its revised
	// message is sent (a Runner restart, a failed send): the fork is a session
	// of its own from the start.
	if err := b.StopExecution(ctx, edited.ID); err != nil {
		t.Fatal(err)
	}
	if edited, err = b.Resume(ctx, edited.ID, false); err != nil {
		t.Fatalf("the edited conversation could not be resumed before its first message: %v", err)
	}
	revised := check(edited, "the edited conversation")
	again, err := b.RewindConversation(ctx, edited.ID, revised)
	if err != nil {
		t.Fatal(err)
	}
	check(again, "the second edit")
	// A turn recorded without its fork point (a worker from before they were
	// recorded): the adapter is given the id of the turn's last message
	// instead, and the worker finds where it ended in Claude's own transcript.
	located, err := b.Adapters["claude"].Open(ctx, agent.Open{ID: commands.ID(), Cwd: source.Cwd, Env: source.Env, Profile: source.Profile.ID, Permission: "read-only",
		ForkThread: reference, ForkAfterMessage: ended[firstPrompt]})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = located.Close() })
	if _, err := located.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: ask}); err != nil {
		t.Fatal(err)
	}
	var reply strings.Builder
	for ended := false; !ended; {
		event, err := located.Receive(ctx)
		if err != nil {
			t.Fatal(err)
		}
		switch event.Type {
		case "failure":
			t.Fatalf("provider failed: %s %s", event.Status, event.Text)
		case "message_delta":
			reply.WriteString(event.Text)
		case "turn_ended":
			ended = true
		}
	}
	if !strings.Contains(reply.String(), kept) || strings.Contains(reply.String(), dropped) {
		t.Fatalf("the fork located by its prompt answered %q: want the first turn only", reply.String())
	}
}
