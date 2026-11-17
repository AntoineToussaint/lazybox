package runner

import (
	"context"
	"errors"
	"log"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/titles"
)

// Conversation titles. A conversation shows the name the person gave it,
// else a title generated after its first turn, else a title made from its
// first message (titles.Fallback, resolved by the session store). After a
// turn ends, a conversation with neither of the first two asks its own agent
// for a title (agent.Titler: the provider's cheapest way, such as Claude
// Code's own session-title request). It runs beside the conversation, never
// delays a turn, and a failure only leaves the fallback title. Settings →
// Agents & workspaces turns it off (preferences.Data.PlainTitles).

// titleTimeout bounds one title request.
const titleTimeout = 45 * time.Second

// titleAttempts is how many turns may try to name a conversation while this
// Runner runs; a provider that cannot name one is not asked again.
const titleAttempts = 3

// titling is the naming in progress and the attempts made, by conversation.
type titling struct {
	mu      sync.Mutex
	tries   map[string]int
	running map[string]bool
}

func (t *titling) begin(conversation string) bool {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.tries == nil {
		t.tries, t.running = map[string]int{}, map[string]bool{}
	}
	if t.running[conversation] || t.tries[conversation] >= titleAttempts {
		return false
	}
	t.running[conversation] = true
	t.tries[conversation]++
	return true
}

func (t *titling) end(conversation string, retry bool) {
	t.mu.Lock()
	defer t.mu.Unlock()
	delete(t.running, conversation)
	if !retry {
		t.tries[conversation] = titleAttempts
	}
}

// conversationTitler is a runtime that can ask its provider for a title
// (engine.Engine).
type conversationTitler interface {
	Title(ctx context.Context, description string) (string, error)
}

// nameConversation gives e's conversation a generated title, unless the
// person named it, it already has one, or generated titles are off.
func (b *Bench) nameConversation(e *Execution) {
	if b.History == nil || (b.Preferences != nil && b.Preferences.Snapshot().PlainTitles) {
		return
	}
	runtime, ok := e.Engine.(conversationTitler)
	if !ok {
		return
	}
	conversation := conversationOf(e)
	if !b.titling.begin(conversation) {
		return
	}
	retry := true
	defer func() { b.titling.end(conversation, retry) }()
	b.mu.Lock()
	closed := b.closed
	b.mu.Unlock()
	if closed {
		return
	}
	ctx, cancel := context.WithTimeout(b.ctx, titleTimeout)
	defer cancel()
	record, err := b.History.Get(ctx, e.ID)
	if err != nil || record.UserTitle != "" || record.GeneratedTitle != "" {
		retry = err != nil
		return
	}
	snapshot := e.Engine.Snapshot()
	opening := strings.TrimSpace(firstOf(record.Opening, snapshot.Opening, snapshot.Title))
	if opening == "" {
		return // nothing said yet: a later turn tries
	}
	raw, err := runtime.Title(ctx, titles.Description(opening, snapshot.LastMessage))
	if errors.Is(err, agent.ErrUnsupported) {
		retry = false
		return
	}
	if err != nil {
		log.Printf("conversation title for %s: %v", conversation, err)
		return
	}
	title := titles.Tidy(raw)
	if title == "" {
		return
	}
	// A rename made while the title was generated still wins: this never
	// touches the person's own name for the conversation.
	if err = b.History.SetGeneratedTitle(ctx, e.ID, title); err != nil {
		log.Printf("conversation title for %s: %v", conversation, err)
		return
	}
	retry = false
}
