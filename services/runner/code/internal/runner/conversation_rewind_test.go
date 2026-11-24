package runner

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// Editing a message replaces the conversation from it on, in place: the same
// conversation goes on in a successor whose native session is a fork that
// ends before the message, and the timeline no longer shows what was
// replaced. A turn is a boundary in whichever native session recorded it, so
// a message can be edited again, and so can any earlier or later one.
func TestEditingAMessageRewindsTheConversationInPlace(t *testing.T) {
	for _, provider := range []string{"claude", "codex"} {
		t.Run(provider, func(t *testing.T) { editingRewindsInPlace(t, provider) })
	}
}

func editingRewindsInPlace(t *testing.T, provider string) {
	b, err := New(Config{State: t.TempDir(), CLIDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { b.Close() })
	fake := &agenttest.Adapter{}
	b.Adapters[provider] = fake
	first, err := b.OpenEmptyWorkspace(provider+"-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	// texts are the prompts and replies the conversation shows, in order.
	texts := func(e *Execution) string {
		t.Helper()
		timeline, err := b.OpenTimeline(e.ID)
		if err != nil {
			t.Fatal(err)
		}
		var shown []string
		for cursor, end := uint64(0), timeline.Retained(); cursor < end; {
			record, err := timeline.Next(ctx, cursor)
			if err != nil {
				t.Fatal(err)
			}
			cursor = record.Sequence
			var event agent.Event
			if json.Unmarshal(record.Value, &event) == nil && (event.Type == "operation_accepted" || event.Type == "message_delta") {
				shown = append(shown, event.Text)
			}
		}
		return strings.Join(shown, "|")
	}
	// send sends a prompt, and with an entry lets the agent answer in session; it returns the prompt's cursor.
	send := func(e *Execution, text, session, entry string) uint64 {
		t.Helper()
		native := fake.Session
		if _, err := e.Engine.Apply(ctx, agent.Operation{ID: "op-" + text, Kind: agent.Message, Text: text}); err != nil {
			t.Fatal(err)
		}
		applied(t, native)
		if entry != "" {
			native.Events <- agent.Event{Type: "turn_started", SessionID: session}
			native.Events <- agent.Event{Type: "message_delta", Text: "re " + text}
			native.Events <- agent.Event{Type: "turn_ended", SessionID: session, TurnID: entry, Status: "completed"}
			waitHistory(t, func() bool { return !e.Engine.Snapshot().Busy })
		}
		timeline, err := b.OpenTimeline(e.ID)
		if err != nil {
			t.Fatal(err)
		}
		for cursor := timeline.Retained(); cursor > 0; cursor-- {
			record, err := timeline.Next(ctx, cursor-1)
			if err != nil {
				t.Fatal(err)
			}
			var event agent.Event
			if json.Unmarshal(record.Value, &event) == nil && event.Type == "operation_accepted" && event.Text == text {
				return record.Sequence
			}
		}
		t.Fatalf("prompt %q was not recorded", text)
		return 0
	}
	opened := func(thread, turn string) {
		t.Helper()
		if open := fake.Session.Open; open.ForkThread != thread || open.ForkAfterTurn != turn || open.Resume != "" {
			t.Fatalf("opened with thread %q after %q (resume %q), want %q after %q", open.ForkThread, open.ForkAfterTurn, open.Resume, thread, turn)
		}
	}
	send(first, "one", "s1", "a1")
	two := send(first, "two", "s1", "a2")
	send(first, "three", "s1", "a3")

	// Edit "two": the conversation continues from a fork of s1 after "one".
	second, err := b.RewindConversation(ctx, first.ID, two)
	if err != nil {
		t.Fatal(err)
	}
	opened("s1", "a1")
	if second.Previous != first.ID || second.Saved.ConversationID != first.Saved.ConversationID || second.Saved.ForkedFrom != "" {
		t.Fatalf("the rewind is not the conversation's next attempt: %+v", second.Saved)
	}
	if !first.Engine.Snapshot().Closed {
		t.Fatal("the replaced continuation is still running")
	}
	if got := texts(second); got != "one|re one" {
		t.Fatalf("after the edit the conversation shows %q", got)
	}
	// A repeated request finds the same attempt; another boundary is refused.
	if again, err := b.RewindConversation(ctx, first.ID, two); err != nil || again.ID != second.ID {
		t.Fatalf("repeated edit: %v, %v", again, err)
	}
	if _, err := b.RewindConversation(ctx, first.ID, two-1); err == nil {
		t.Fatal("an attempt that was already replaced was replaced again")
	}

	// Edit the revised message again, before it was answered and after.
	revised := send(second, "two, revised", "", "")
	third, err := b.RewindConversation(ctx, second.ID, revised)
	if err != nil {
		t.Fatal(err)
	}
	opened("s1", "a1")
	revised = send(third, "two, again", "s2", "b1")
	later := send(third, "four", "s2", "b2")
	if got := texts(third); got != "one|re one|two, again|re two, again|four|re four" {
		t.Fatalf("the conversation shows %q", got)
	}
	// A later message is cut in the session that recorded the turn before it.
	fourth, err := b.RewindConversation(ctx, third.ID, later)
	if err != nil {
		t.Fatal(err)
	}
	opened("s2", "b1")
	if got := texts(fourth); got != "one|re one|two, again|re two, again" {
		t.Fatalf("the conversation shows %q", got)
	}
	// An earlier one, again: back in the first session.
	fifth, err := b.RewindConversation(ctx, fourth.ID, revised)
	if err != nil {
		t.Fatal(err)
	}
	opened("s1", "a1")
	// The first message follows no turn: a new native session.
	send(fifth, "two, once more", "", "")
	sixth, err := b.RewindConversation(ctx, fifth.ID, firstPrompt(t, b, fifth))
	if err != nil {
		t.Fatal(err)
	}
	opened("", "")
	if got := texts(sixth); got != "" {
		t.Fatalf("the conversation shows %q", got)
	}
	if sixth.Saved.ConversationID != first.Saved.ConversationID {
		t.Fatal("the conversation changed identity")
	}
}

// firstPrompt is the cursor of the conversation's first message.
func firstPrompt(t *testing.T, b *Bench, e *Execution) uint64 {
	t.Helper()
	timeline, err := b.OpenTimeline(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	for cursor, end := uint64(0), timeline.Retained(); cursor < end; {
		record, err := timeline.Next(context.Background(), cursor)
		if err != nil {
			t.Fatal(err)
		}
		cursor = record.Sequence
		if turnStart(record.Value) {
			return cursor
		}
	}
	t.Fatal("no prompt")
	return 0
}

// rewindBench is a Runner with one conversation of a fake Claude, and say:
// a message the agent answers in session s1, ending the turn with entry
// (none: a turn recorded without its fork point). It returns the message's cursor.
func rewindBench(t *testing.T) (b *Bench, fake *agenttest.Adapter, first *Execution, say func(e *Execution, text, entry string) uint64) {
	t.Helper()
	b, err := New(Config{State: t.TempDir(), CLIDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { b.Close() })
	fake = &agenttest.Adapter{}
	b.Adapters["claude"] = fake
	if first, err = b.OpenEmptyWorkspace("claude-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	sent := 0
	say = func(e *Execution, text, entry string) uint64 {
		t.Helper()
		native := fake.Session
		sent++
		id := "op-" + string(rune('a'+sent))
		if _, err := e.Engine.Apply(context.Background(), agent.Operation{ID: id, Kind: agent.Message, Text: text}); err != nil {
			t.Fatal(err)
		}
		applied(t, native)
		native.Events <- agent.Event{Type: "turn_started", SessionID: "s1"}
		native.Events <- agent.Event{Type: "turn_ended", SessionID: "s1", TurnID: entry, Status: "completed"}
		waitHistory(t, func() bool { return !e.Engine.Snapshot().Busy })
		timeline, err := b.OpenTimeline(e.ID)
		if err != nil {
			t.Fatal(err)
		}
		for cursor := timeline.Retained(); cursor > 0; cursor-- {
			record, err := timeline.Next(context.Background(), cursor-1)
			if err != nil {
				t.Fatal(err)
			}
			var event agent.Event
			if json.Unmarshal(record.Value, &event) == nil && event.Type == "operation_accepted" && event.OperationID == id {
				return record.Sequence
			}
		}
		t.Fatalf("message %q was not recorded", text)
		return 0
	}
	return b, fake, first, say
}

// A fork the provider cannot start replaces nothing: the conversation is as
// it was, stopped, and resumes like any other; the edit can be tried again.
func TestAnEditTheProviderCannotStartLeavesTheConversationWhole(t *testing.T) {
	b, fake, first, say := rewindBench(t)
	ctx := context.Background()
	say(first, "one", "a1")
	two := say(first, "two", "a2")
	fake.Fail = errors.New("Claude's transcript no longer holds this message")
	if _, err := b.RewindConversation(ctx, first.ID, two); err == nil || !strings.Contains(err.Error(), "no longer holds") {
		t.Fatalf("the failed start was not reported: %v", err)
	}
	failed, err := b.History.Successor(ctx, first.ID)
	if err != nil {
		t.Fatal(err)
	}
	if failed.Cut != 0 || failed.Snapshot.Reference != first.Engine.Snapshot().Reference || !failed.Snapshot.Closed {
		t.Fatalf("the failed attempt still replaces the conversation: cut %d, reference %q, closed %v", failed.Cut, failed.Snapshot.Reference, failed.Snapshot.Closed)
	}
	// Nothing is hidden: the conversation is everything its first attempt recorded.
	if retained, err := b.Retained(failed.ID); err != nil || retained != first.Retained() {
		t.Fatalf("the conversation shows %d records, %v; its first attempt recorded %d", retained, err, first.Retained())
	}
	// The same request again is not answered with the attempt that never started.
	if again, err := b.RewindConversation(ctx, first.ID, two); err == nil {
		t.Fatalf("the failed attempt was returned as the edited conversation: %v", again.ID)
	}
	fake.Fail = nil
	resumed, err := b.Resume(ctx, failed.ID, false)
	if err != nil {
		t.Fatal(err)
	}
	if fake.Session.Open.Resume != first.Engine.Snapshot().Reference || fake.Session.Open.ForkThread != "" {
		t.Fatalf("resumed with %+v", fake.Session.Open)
	}
	if _, err := b.RewindConversation(ctx, resumed.ID, two); err != nil {
		t.Fatalf("the edit could not be tried again: %v", err)
	}
	if open := fake.Session.Open; open.ForkThread != "s1" || open.ForkAfterTurn != "a1" {
		t.Fatalf("the second try opened %+v", open)
	}
}

// What can refuse the successor without starting it is checked before the
// agent is stopped.
func TestAnEditThatCannotContinueDoesNotStopTheAgent(t *testing.T) {
	b, _, first, say := rewindBench(t)
	ctx := context.Background()
	say(first, "one", "a1")
	two := say(first, "two", "a2")
	block := func(r *sessionstore.Record) { r.ResumeBlocked = "this conversation cannot be continued" }
	if err := b.History.Amend(ctx, first.ID, block); err != nil {
		t.Fatal(err)
	}
	block(&first.Saved)
	if _, err := b.RewindConversation(ctx, first.ID, two); err == nil || !strings.Contains(err.Error(), "cannot be continued") {
		t.Fatalf("a blocked conversation was edited: %v", err)
	}
	if first.Engine.Snapshot().Closed {
		t.Fatal("the agent was stopped for an edit that was refused")
	}
	if _, err := b.History.Successor(ctx, first.ID); err == nil {
		t.Fatal("a refused edit left a successor")
	}
}

// A turn recorded without its fork point (a Claude worker from before they
// were recorded) still names the provider's id of each message it wrote: the
// edit is cut after the turn's last one.
func TestATurnWithoutItsForkPointIsCutAfterItsLastMessage(t *testing.T) {
	b, fake, first, say := rewindBench(t)
	ctx := context.Background()
	native := fake.Session
	say(first, "one", "a1")
	// "two": a turn that ends without an id, after two messages of the main
	// agent and one of a sub-agent.
	if _, err := first.Engine.Apply(ctx, agent.Operation{ID: "op-two", Kind: agent.Message, Text: "two"}); err != nil {
		t.Fatal(err)
	}
	applied(t, native)
	usage := func(agentID, request string) agent.Event {
		data, _ := json.Marshal(map[string]string{"requestId": request})
		return agent.Event{Type: "usage", AgentID: agentID, SessionID: "s1", Data: data}
	}
	native.Events <- agent.Event{Type: "turn_started", SessionID: "s1"}
	native.Events <- usage("", "msg_first")
	native.Events <- usage("", "msg_last")
	native.Events <- usage("claude-tool:call", "msg_child")
	native.Events <- agent.Event{Type: "turn_ended", SessionID: "s1", Status: "completed"}
	waitHistory(t, func() bool { return !first.Engine.Snapshot().Busy })
	three := say(first, "three", "")
	if _, err := b.RewindConversation(ctx, first.ID, three); err != nil {
		t.Fatal(err)
	}
	if open := fake.Session.Open; open.ForkThread != "s1" || open.ForkAfterTurn != "" || open.ForkAfterMessage != "msg_last" {
		t.Fatalf("cut after a turn without an id: thread %q, turn %q, message %q", open.ForkThread, open.ForkAfterTurn, open.ForkAfterMessage)
	}
}

// A turn that names neither is no boundary: the edit is refused, and nothing is stopped.
func TestATurnThatNamesNothingIsNoBoundary(t *testing.T) {
	b, _, first, say := rewindBench(t)
	ctx := context.Background()
	say(first, "one", "")
	two := say(first, "two", "a2")
	if _, err := b.RewindConversation(ctx, first.ID, two); err == nil {
		t.Fatal("a message was edited after a turn the provider's session cannot be cut after")
	}
	if first.Engine.Snapshot().Closed {
		t.Fatal("the agent was stopped for an edit that was refused")
	}
}
