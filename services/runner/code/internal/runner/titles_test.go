package runner

import (
	"context"
	"errors"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
)

const titlePrompt = "I want to start a plan, we want to look at SOTA but the goal is to write down a design for conversation titles"

// openTitled opens a conversation whose agent names it with name, and says
// its first message.
func openTitled(t *testing.T, name func(string) (string, error)) (*Bench, *Execution, *agenttest.Adapter) {
	t.Helper()
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { b.Close() })
	fake := &agenttest.Adapter{Titles: name}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "m1", Kind: agent.Message, Text: titlePrompt}); err != nil {
		t.Fatal(err)
	}
	applied(t, fake.Session)
	return b, e, fake
}

func endTurn(fake *agenttest.Adapter) {
	fake.Session.Events <- agent.Event{Type: "turn_started"}
	fake.Session.Events <- agent.Event{Type: "message_delta", Text: "Here is the plan for titles."}
	fake.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
}

func shownTitle(t *testing.T, b *Bench, e *Execution, want string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		record, err := b.History.Get(context.Background(), e.ID)
		if err == nil && record.DisplayTitle == want {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("title %q, want %q (%v)", record.DisplayTitle, want, err)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

func TestFirstTurnNamesTheConversation(t *testing.T) {
	var asked atomic.Value
	b, e, fake := openTitled(t, func(description string) (string, error) {
		asked.Store(description)
		return `"Conversation title design."`, nil
	})
	// Before the turn ends: the first message, cut at a word.
	shownTitle(t, b, e, "Start a plan, we want to look at SOTA but the goal…")
	endTurn(fake)
	shownTitle(t, b, e, "Conversation title design")
	description, _ := asked.Load().(string)
	if !strings.Contains(description, titlePrompt) || !strings.Contains(description, "Here is the plan for titles.") {
		t.Fatalf("the agent was not told what the conversation is about: %q", description)
	}
}

func TestARenameIsNeverReplacedByAGeneratedTitle(t *testing.T) {
	var calls atomic.Int32
	release := make(chan struct{})
	b, e, fake := openTitled(t, func(string) (string, error) {
		calls.Add(1)
		<-release // the person renames it while the title is generated
		return "Generated name", nil
	})
	endTurn(fake)
	deadline := time.Now().Add(5 * time.Second)
	for calls.Load() == 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if err := b.History.Rename(context.Background(), e.ID, "My name"); err != nil {
		t.Fatal(err)
	}
	close(release)
	time.Sleep(100 * time.Millisecond)
	shownTitle(t, b, e, "My name")
	// Later turns do not ask again: the person named it.
	endTurn(fake)
	time.Sleep(100 * time.Millisecond)
	shownTitle(t, b, e, "My name")
	if calls.Load() != 1 {
		t.Fatalf("asked for a title %d times", calls.Load())
	}
}

func TestGeneratedTitlesCanBeTurnedOff(t *testing.T) {
	var calls atomic.Int32
	b, e, fake := openTitled(t, func(string) (string, error) { calls.Add(1); return "Generated", nil })
	d := b.Preferences.Snapshot()
	d.PlainTitles = true
	if err := b.Preferences.Save(d); err != nil {
		t.Fatal(err)
	}
	endTurn(fake)
	time.Sleep(150 * time.Millisecond)
	shownTitle(t, b, e, "Start a plan, we want to look at SOTA but the goal…")
	if calls.Load() != 0 {
		t.Fatal("a title was generated with generated titles off")
	}
}

func TestAFailedTitleKeepsTheFallbackAndRetriesOnALaterTurn(t *testing.T) {
	var calls atomic.Int32
	b, e, fake := openTitled(t, func(string) (string, error) {
		if calls.Add(1) == 1 {
			return "", errors.New("provider unavailable")
		}
		return "Second try", nil
	})
	endTurn(fake)
	deadline := time.Now().Add(5 * time.Second)
	for calls.Load() == 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	shownTitle(t, b, e, "Start a plan, we want to look at SOTA but the goal…")
	time.Sleep(50 * time.Millisecond)
	endTurn(fake)
	shownTitle(t, b, e, "Second try")
}

func TestAProviderThatCannotNameIsNotAskedAgain(t *testing.T) {
	b, e, fake := openTitled(t, nil)
	endTurn(fake)
	endTurn(fake)
	time.Sleep(100 * time.Millisecond)
	shownTitle(t, b, e, "Start a plan, we want to look at SOTA but the goal…")
	b.titling.mu.Lock()
	tries := b.titling.tries[conversationOf(e)]
	b.titling.mu.Unlock()
	if tries != titleAttempts {
		t.Fatalf("an unsupported provider is asked again: %d", tries)
	}
}
