package runner

import (
	"context"
	"errors"
	"os"
	"path/filepath"
)

// DeleteConversation stops owned execution before deleting Lazybox transcripts.
// Workspace files, provider-native history, credentials and usage records remain.
func (b *Bench) DeleteConversation(ctx context.Context, id string, stop bool) error {
	if ids, err := b.History.Deleted(ctx, id); err != nil {
		return err
	} else if len(ids) > 0 {
		return b.purgeConversation(ids)
	}
	// Lock the latest attempt against continuation, then check it is still latest.
	for {
		e, err := b.Get(id)
		if err != nil {
			return err
		}
		e.Continuation.Lock()
		next, err := b.History.Successor(ctx, id)
		if err == nil {
			e.Continuation.Unlock()
			id = next.ID
			continue
		}
		if !errors.Is(err, os.ErrNotExist) {
			e.Continuation.Unlock()
			return err
		}
		err = b.deleteLocked(ctx, e, stop)
		e.Continuation.Unlock()
		return err
	}
}
func (b *Bench) deleteLocked(ctx context.Context, head *Execution, stop bool) error {
	chain := []*Execution{}
	seen := map[string]bool{}
	for e := head; e != nil; {
		if seen[e.ID] {
			return errors.New("invalid execution lineage")
		}
		seen[e.ID] = true
		if e.RecoveryRequired {
			return errors.New("previous process ownership is unconfirmed; reconcile before deleting this conversation")
		}
		if b.Active(e.ID) && !stop {
			return errors.New("this conversation still has an agent or commands running; confirm Stop and delete")
		}
		chain = append(chain, e)
		if e.Previous == "" {
			break
		}
		var err error
		e, err = b.Get(e.Previous)
		if err != nil {
			return err
		}
	}
	for _, e := range chain {
		if err := b.StopExecution(ctx, e.ID); err != nil {
			return err
		}
	}
	for _, e := range chain {
		if b.Collections != nil && e.CollectionID != "" {
			if err := b.Collections.Release(e.CollectionID, e.ID); err != nil {
				return err
			}
		}
	}
	ids, err := b.History.Delete(ctx, head.ID)
	if err != nil {
		return err
	}
	b.mu.Lock()
	for _, id := range ids {
		delete(b.items, id)
		delete(b.archives, id)
	}
	b.mu.Unlock()
	// Readers have reached EOF after Close; dispose the journal descriptors too.
	for _, e := range chain {
		if err := e.spool.Dispose(); err != nil {
			return err
		}
	}
	return b.purgeConversation(ids)
}
func (b *Bench) purgeConversation(ids []string) error {
	for _, id := range ids {
		// IDs come exclusively from the durable store, but still constrain deletion
		// to one direct child of the Runner's session directory.
		if id == "" || filepath.Base(id) != id || id == "." || id == ".." {
			return errors.New("invalid stored execution identity")
		}
		if err := os.Remove(filepath.Join(b.Config.State, "sessions", id+".jsonl")); err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		// The re-attachment record can hold queued message text and image bytes.
		if err := os.Remove(b.attachmentPath(id)); err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		if err := b.Attachments.Delete(id); err != nil {
			return err
		}
	}
	return nil
}
