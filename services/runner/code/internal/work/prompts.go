package work

import (
	"encoding/json"
	"strings"
	"sync"
	"time"
)

// Prompt sources (ORCHESTRATION.md §6): who told the agent to do this.
const (
	SourceTyped   = "typed"
	SourceSnippet = "snippet"
	SourceConsole = "console" // Send to agent from a run
	SourceInbox   = "inbox"   // a quick reply
	SourceMission = "mission" // a task brief sent when a task was assigned
	SourceHandoff = "handoff"
	// SourceAgent: another agent's message, through Lazybox (an
	// orchestrator's brief or note to its child, or a child's to its parent).
	SourceAgent = "agent"
	// SourceRepository: the Runner told an agent the answer to its request
	// for a repository (runner/repository_requests.go).
	SourceRepository = "repository_request"
)

type PromptSource struct {
	Kind    string `json:"kind"`
	Snippet string `json:"snippet,omitempty"`
	// Digest identifies the snippet's text as sent, to tell a changed catalog apart.
	Digest  string `json:"digest,omitempty"`
	Command string `json:"command,omitempty"`
	Mission string `json:"mission,omitempty"`
	Task    string `json:"task,omitempty"`
	// Matched: the client named no source and the text is exactly a catalog
	// snippet's body, so the Runner recorded it as that snippet.
	Matched bool `json:"matched,omitempty"`
}

type PromptPayload struct {
	Conversation string       `json:"conversation"`
	Workspace    string       `json:"workspace,omitempty"`
	Execution    string       `json:"execution,omitempty"`
	Text         string       `json:"text"`
	Source       PromptSource `json:"source"`
	// Operation is the client's operation id: a retried send is recorded once.
	Operation string `json:"operation,omitempty"`
}

type SnippetPayload struct {
	Conversation string   `json:"conversation"`
	Workspace    string   `json:"workspace,omitempty"`
	Key          string   `json:"key"`
	Digest       string   `json:"digest,omitempty"`
	Next         []string `json:"next,omitempty"`
}

// Bounds of the prompt log. Prompt text stays in Runner state (missions
// exports leave it out); the log is compacted to its newest half when full.
const (
	maxPromptText  = 16 << 10
	maxPromptBytes = 16 << 20
)

// maxPromptEvents is a variable so a test can bound the log without
// thousands of fsyncs.
var maxPromptEvents = 4000

// Prompts is the Runner-wide conversation log: every message a person (or a
// rule) sent to an agent and every snippet sent, newest last. Recall and the
// snippet chain are derived from it.
type Prompts struct {
	mu     sync.Mutex
	path   string
	log    *file
	events []Event
	Now    func() time.Time
}

// OpenPrompts reads the prompt log, compacting it when it is over its bounds.
func OpenPrompts(path string) (*Prompts, error) {
	log, events, err := openFile(path, 0)
	if err != nil {
		return nil, err
	}
	p := &Prompts{path: path, log: log, events: events, Now: func() time.Time { return time.Now().UTC() }}
	if len(events) > maxPromptEvents || log.size > maxPromptBytes {
		if err := p.compact(); err != nil {
			_ = log.close()
			return nil, err
		}
	}
	return p, nil
}

func (p *Prompts) compact() error {
	keep := p.events
	if len(keep) > maxPromptEvents/2 {
		keep = keep[len(keep)-maxPromptEvents/2:]
	}
	if err := p.log.rewrite(p.path, keep); err != nil {
		return err
	}
	p.events = append([]Event(nil), keep...)
	return nil
}

func (p *Prompts) Close() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.log.close()
}

// Submit records a prompt and, when it is a snippet, the snippet sent with
// the chain it offers next.
func (p *Prompts) Submit(actor Actor, prompt PromptPayload, next []string) error {
	if prompt.Conversation == "" || strings.TrimSpace(prompt.Text) == "" {
		return nil
	}
	if len(prompt.Text) > maxPromptText {
		// Recall is for what a person would retype; a pasted log is not kept.
		return nil
	}
	if prompt.Source.Kind == "" {
		prompt.Source.Kind = SourceTyped
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	at := p.Now().UTC()
	seq := uint64(0)
	if n := len(p.events); n > 0 {
		seq = p.events[n-1].Seq
	}
	payload, err := json.Marshal(prompt)
	if err != nil {
		return err
	}
	subject := "conversation:" + prompt.Conversation
	key := ""
	if prompt.Operation != "" {
		key = prompt.Conversation + "/" + prompt.Operation
		for i := len(p.events) - 1; i >= 0 && i >= len(p.events)-256; i-- {
			if p.events[i].Idempotency == key {
				return nil
			}
		}
	}
	events := []Event{{ID: newID(at), Seq: seq + 1, At: at, Subject: subject, Type: PromptSubmitted, V: Versions[PromptSubmitted], Actor: actor, Idempotency: key, Payload: payload}}
	if prompt.Source.Kind == SourceSnippet && prompt.Source.Snippet != "" {
		snippet, err := json.Marshal(SnippetPayload{Conversation: prompt.Conversation, Workspace: prompt.Workspace, Key: prompt.Source.Snippet, Digest: prompt.Source.Digest, Next: next})
		if err != nil {
			return err
		}
		events = append(events, Event{ID: newID(at), Seq: seq + 2, At: at, Subject: subject, Type: SnippetSent, V: Versions[SnippetSent], Actor: actor, CausedBy: events[0].ID, Payload: snippet})
	}
	if err := p.log.append(events); err != nil {
		return err
	}
	p.events = append(p.events, events...)
	if len(p.events) > maxPromptEvents || p.log.size > maxPromptBytes {
		return p.compact()
	}
	return nil
}

// Forget removes a deleted conversation's prompts.
func (p *Prompts) Forget(conversations ...string) error {
	gone := map[string]bool{}
	for _, c := range conversations {
		gone["conversation:"+c] = true
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	kept := make([]Event, 0, len(p.events))
	for _, e := range p.events {
		if !gone[e.Subject] {
			kept = append(kept, e)
		}
	}
	if len(kept) == len(p.events) {
		return nil
	}
	if err := p.log.rewrite(p.path, kept); err != nil {
		return err
	}
	p.events = kept
	return nil
}

// PromptEntry is one recalled prompt.
type PromptEntry struct {
	At time.Time
	PromptPayload
}

// HistoryQuery scopes recall: one conversation, one workspace, or all.
type HistoryQuery struct {
	Conversation string
	Workspace    string
	Search       string
	Limit        int
}

// History is recall, newest first, without repeats of the same text.
func (p *Prompts) History(q HistoryQuery) []PromptEntry {
	if q.Limit <= 0 || q.Limit > 500 {
		q.Limit = 200
	}
	search := strings.ToLower(strings.TrimSpace(q.Search))
	p.mu.Lock()
	defer p.mu.Unlock()
	seen := map[string]bool{}
	var out []PromptEntry
	for i := len(p.events) - 1; i >= 0 && len(out) < q.Limit; i-- {
		e := p.events[i]
		if e.Type != PromptSubmitted || !Known(e) {
			continue
		}
		prompt, ok := decode[PromptPayload](e)
		if !ok || (q.Conversation != "" && prompt.Conversation != q.Conversation) || (q.Workspace != "" && prompt.Workspace != q.Workspace) {
			continue
		}
		if search != "" && !strings.Contains(strings.ToLower(prompt.Text), search) {
			continue
		}
		if seen[prompt.Text] {
			continue
		}
		seen[prompt.Text] = true
		out = append(out, PromptEntry{At: e.At, PromptPayload: prompt})
	}
	return out
}

// SnippetMemory is what the log says about snippets in one conversation and
// its workspace (V1's per-workspace memory and `next:` chains).
type SnippetMemory struct {
	// Last is the snippet last sent to the conversation, and Next the chain it offers.
	Last   string
	LastAt time.Time
	Next   []string
	// Recent is the workspace's last distinct snippets, newest first (at most 12).
	Recent []string
}

func (p *Prompts) Snippets(conversation, workspace string) SnippetMemory {
	p.mu.Lock()
	defer p.mu.Unlock()
	var memory SnippetMemory
	seen := map[string]bool{}
	for i := len(p.events) - 1; i >= 0; i-- {
		e := p.events[i]
		if e.Type != SnippetSent || !Known(e) {
			continue
		}
		s, ok := decode[SnippetPayload](e)
		if !ok {
			continue
		}
		if conversation != "" && s.Conversation == conversation && memory.Last == "" {
			memory.Last, memory.LastAt, memory.Next = s.Key, e.At, s.Next
		}
		if workspace != "" && s.Workspace == workspace && !seen[s.Key] && len(memory.Recent) < 12 {
			seen[s.Key] = true
			memory.Recent = append(memory.Recent, s.Key)
		}
	}
	return memory
}

// Events returns the log, for tests and exports.
func (p *Prompts) Events() []Event {
	p.mu.Lock()
	defer p.mu.Unlock()
	return append([]Event(nil), p.events...)
}
