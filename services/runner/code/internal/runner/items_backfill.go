package runner

import (
	"context"
	"encoding/json"
	"errors"
	"log"
	"os"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// The agent-output detector reads tool results as the engine records them.
// A pull request an agent opened while it did not run — before tracking
// existed, or while the Runner was down and the agent kept working — is
// still in the conversation's journal. On start, the Runner reads the recent
// end of each workspace conversation's journal once for the pull requests a
// PR-creating command printed (mentions are not backfilled), and remembers
// how far it read, so the next start reads only what is new. Linking is
// idempotent, so reading a record twice changes nothing.

const (
	// backfillEvents bounds what is read of one conversation's journals.
	backfillEvents = 5000
	// backfillBytes bounds what is read of one journal (its end).
	backfillBytes = 32 << 20
	// backfillConversations bounds the conversations read on one start.
	backfillConversations = 500
	// backfillAttempts bounds how far back one conversation's lineage is read.
	backfillAttempts = 10
	// backfillTimeout bounds the whole read.
	backfillTimeout = 2 * time.Minute
)

// BackfillItems reads the recent journal of every live workspace's
// conversations, updated this week, for pull requests their agents opened,
// and links them. The Runner runs it once when it starts; tests call it.
func (b *Bench) BackfillItems(ctx context.Context) {
	store := b.workItems.store
	if store == nil || b.History == nil {
		return
	}
	since := time.Now().Add(-workitems.BranchFor)
	read := 0
	for offset := 0; read < backfillConversations && ctx.Err() == nil; offset += 100 {
		records, err := b.History.List(ctx, 100, offset)
		if err != nil {
			log.Printf("tracked items: backfill: %v", err)
			return
		}
		for _, r := range records {
			if r.CollectionID == "" || agent.IsAssistant(r.Purpose) || r.UpdatedAt.Before(since) || !b.workspaceLive(r.CollectionID) {
				continue
			}
			read++
			b.backfillConversation(ctx, r)
		}
		if len(records) < 100 {
			return
		}
	}
}

func (b *Bench) workspaceLive(id string) bool {
	if b.Collections == nil {
		return true
	}
	w, err := b.Collections.Get(id)
	return err == nil && w.State != "deleted"
}

// backfillConversation reads a conversation's attempts, newest first, until
// backfillEvents records were read.
func (b *Bench) backfillConversation(ctx context.Context, head sessionstore.Record) {
	conversation := head.ConversationID
	if conversation == "" {
		conversation = head.ID
	}
	budget := backfillEvents
	r := head
	for attempt := 0; attempt < backfillAttempts && budget > 0 && ctx.Err() == nil; attempt++ {
		if deleted, err := b.History.Deleted(ctx, r.ID); err == nil && len(deleted) == 0 {
			budget -= b.backfillAttempt(r.ID, r.CollectionID, conversation, budget)
		}
		if r.Previous == "" {
			return
		}
		previous, err := b.History.Get(ctx, r.Previous)
		if err != nil {
			return
		}
		r = previous
	}
}

// backfillAttempt reads the end of one attempt's journal, after what an
// earlier start read, and links what its PR-creating commands printed. It
// returns how many records it read.
func (b *Bench) backfillAttempt(id, workspace, conversation string, limit int) int {
	store := b.workItems.store
	after := store.Backfilled(id)
	records, err := journal.Tail(b.journalPath(id), limit, backfillBytes)
	if err != nil {
		if !errors.Is(err, os.ErrNotExist) {
			log.Printf("tracked items: backfill %s: %v", id, err)
		}
		return 0
	}
	creates := map[string]bool{}
	var last uint64
	read := 0
	for _, record := range records {
		last = max(last, record.Sequence)
		if record.Sequence <= after {
			continue
		}
		read++
		var ev agent.Event
		if json.Unmarshal(record.Value, &ev) != nil || ev.Type != "tool_activity" {
			continue
		}
		call, ok := toolCallOf(ev)
		if !ok || call.ID == "" {
			continue
		}
		key := ev.AgentID + "\x00" + call.ID
		if call.creates() {
			creates[key] = true
		}
		if len(call.Output) == 0 || !creates[key] {
			continue
		}
		delete(creates, key)
		for _, f := range workitems.Scan(call.output(), call.Status == "completed") {
			if f.Role != workitems.RoleDelivers {
				continue
			}
			if _, err := b.linkItem(workitems.Link{Ref: f.Ref, Workspace: workspace, Session: id, Conversation: conversation, Role: workitems.RoleDelivers, Source: workitems.SourceAgentOutput}); err != nil {
				log.Printf("tracked items: backfill %s: %v", id, err)
			}
		}
	}
	if last > after {
		_ = store.SetBackfilled(id, last)
	}
	return read
}
