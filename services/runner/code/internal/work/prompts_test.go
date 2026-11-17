package work

import (
	"path/filepath"
	"strings"
	"testing"
)

func TestPromptHistoryRecallAndSnippetChains(t *testing.T) {
	path := filepath.Join(t.TempDir(), "prompts.jsonl")
	p, err := OpenPrompts(path)
	if err != nil {
		t.Fatal(err)
	}
	send := func(conversation, workspace, text string, source PromptSource, next ...string) {
		t.Helper()
		if err := p.Submit(Human, PromptPayload{Conversation: conversation, Workspace: workspace, Text: text, Source: source}, next); err != nil {
			t.Fatal(err)
		}
	}
	send("c1", "ws", "fix the login bug", PromptSource{})
	send("c2", "ws", "write the docs", PromptSource{})
	send("c1", "ws", "Review the current change carefully.", PromptSource{Kind: SourceSnippet, Snippet: "review", Digest: "d1"}, "fixall", "test")
	send("c1", "ws", "fix the login bug", PromptSource{})
	send("c1", "ws", "   ", PromptSource{})
	// A retried send (same operation) is recorded once.
	for range 2 {
		if err := p.Submit(Human, PromptPayload{Conversation: "c2", Workspace: "ws", Text: "retried", Operation: "op-1"}, nil); err != nil {
			t.Fatal(err)
		}
	}
	send("c1", "ws", strings.Repeat("x", maxPromptText+1), PromptSource{})

	got := p.History(HistoryQuery{Conversation: "c1"})
	if len(got) != 2 || got[0].Text != "fix the login bug" || got[1].Source.Snippet != "review" || got[0].Source.Kind != SourceTyped {
		t.Fatalf("recall: newest first, no repeats, no blanks or pasted logs: %+v", got)
	}
	retried := 0
	for _, e := range p.Events() {
		if e.Idempotency == "c2/op-1" {
			retried++
		}
	}
	if retried != 1 {
		t.Fatalf("a retried send was recorded %d times", retried)
	}
	if all := p.History(HistoryQuery{}); len(all) != 4 {
		t.Fatalf("all conversations: %d", len(all))
	}
	if found := p.History(HistoryQuery{Search: "DOCS"}); len(found) != 1 || found[0].Conversation != "c2" {
		t.Fatalf("search: %+v", found)
	}
	memory := p.Snippets("c1", "ws")
	if memory.Last != "review" || strings.Join(memory.Next, ",") != "fixall,test" || strings.Join(memory.Recent, ",") != "review" {
		t.Fatalf("snippet memory: %+v", memory)
	}
	if other := p.Snippets("c2", "ws"); other.Last != "" || len(other.Recent) != 1 {
		t.Fatalf("another conversation has no chain of its own: %+v", other)
	}
	// snippet.sent follows its prompt.submitted.
	events := p.Events()
	var sent int
	for i, e := range events {
		if e.Type == SnippetSent {
			sent++
			if events[i-1].Type != PromptSubmitted || e.CausedBy != events[i-1].ID {
				t.Fatal("snippet.sent is not caused by its prompt")
			}
		}
	}
	if sent != 1 {
		t.Fatalf("%d snippet events", sent)
	}
	// History survives a restart, and a deleted conversation's prompts go.
	p.Close()
	if p, err = OpenPrompts(path); err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	if len(p.History(HistoryQuery{})) != 4 {
		t.Fatal("history lost on reopen")
	}
	if err := p.Forget("c1"); err != nil {
		t.Fatal(err)
	}
	if left := p.History(HistoryQuery{}); len(left) != 2 || left[0].Conversation != "c2" {
		t.Fatalf("forget: %+v", left)
	}
	if memory := p.Snippets("c1", "ws"); memory.Last != "" || len(memory.Recent) != 0 {
		t.Fatal("a deleted conversation's snippets are remembered")
	}
	send("c3", "ws", "after forget", PromptSource{})
	if len(p.History(HistoryQuery{})) != 3 {
		t.Fatal("append after a rewrite failed")
	}
}

func TestPromptLogIsBounded(t *testing.T) {
	defer func(n int) { maxPromptEvents = n }(maxPromptEvents)
	maxPromptEvents = 40
	path := filepath.Join(t.TempDir(), "prompts.jsonl")
	p, err := OpenPrompts(path)
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	for i := 0; i < maxPromptEvents+10; i++ {
		if err := p.Submit(Human, PromptPayload{Conversation: "c", Text: strings.Repeat("y", 8) + string(rune('a'+i%26)) + strings.Repeat("z", i%7)}, nil); err != nil {
			t.Fatal(err)
		}
	}
	if n := len(p.Events()); n > maxPromptEvents || n < maxPromptEvents/2 {
		t.Fatalf("%d events kept", n)
	}
	// The newest are the ones kept.
	last, _ := decode[PromptPayload](p.Events()[len(p.Events())-1])
	if got := p.History(HistoryQuery{Limit: 1}); len(got) != 1 || got[0].Text != last.Text {
		t.Fatal("compaction dropped the newest prompt")
	}
}
