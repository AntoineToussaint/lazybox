package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
)

const deletionGrace = 5 * time.Minute

type PendingDeletion struct {
	ID, WorkspaceID, SessionID, ConversationID, Title string
	Discard, Stop                                     bool
	Expires                                           time.Time
	Failure                                           string
}

type pendingDeletions struct {
	mu      sync.Mutex
	path    string
	entries map[string]PendingDeletion
}

func openPendingDeletions(path string) (*pendingDeletions, error) {
	store := &pendingDeletions{path: path, entries: map[string]PendingDeletion{}}
	raw, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return store, nil
	}
	if err != nil {
		return nil, err
	}
	if err := json.Unmarshal(raw, &store.entries); err != nil {
		return nil, err
	}
	if store.entries == nil {
		return nil, errors.New("invalid pending deletions")
	}
	return store, nil
}

func (s *pendingDeletions) save() error {
	raw, err := json.Marshal(s.entries)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(s.path), ".pending-deletions-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_, err = f.Write(raw)
	if err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err != nil || closeErr != nil {
		return errors.Join(err, closeErr)
	}
	return os.Rename(f.Name(), s.path)
}

func (b *Bench) PendingDeletions() []PendingDeletion {
	s := b.pendingDeletions
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]PendingDeletion, 0, len(s.entries))
	for _, entry := range s.entries {
		out = append(out, entry)
	}
	slices.SortFunc(out, func(a, b PendingDeletion) int {
		if order := a.Expires.Compare(b.Expires); order != 0 {
			return order
		}
		return strings.Compare(a.ID, b.ID)
	})
	return out
}

func (b *Bench) ScheduleDeletion(ctx context.Context, entry PendingDeletion) (PendingDeletion, error) {
	if entry.SessionID == "" {
		if b.Collections == nil {
			return entry, errors.New("workspaces unavailable")
		}
		workspace, err := b.Collections.Get(entry.WorkspaceID)
		if err != nil {
			return entry, err
		}
		if workspace.State == "deleted" || workspace.Name != entry.Title {
			return entry, errors.New("workspace name or state changed")
		}
		preview, err := b.Collections.PreviewDeletion(ctx, entry.WorkspaceID, b.Active)
		if err != nil {
			return entry, err
		}
		if preview.Blocked != "" {
			return entry, errors.New(preview.Blocked)
		}
		if len(preview.Active) != 0 && !entry.Stop {
			return entry, errors.New("confirm stopping active sessions")
		}
		if (len(preview.Dirty) != 0 || preview.FilesOutsideRepositories) && !entry.Discard {
			return entry, errors.New("confirm discarding workspace changes")
		}
		entry.ID = "workspace:" + entry.WorkspaceID
	} else {
		e, err := b.Get(entry.SessionID)
		if err != nil {
			return entry, err
		}
		if e.RecoveryRequired {
			return entry, errors.New("previous process ownership is unconfirmed")
		}
		if b.Active(e.ID) && !entry.Stop {
			return entry, errors.New("confirm stopping active sessions")
		}
		record, err := b.History.Get(ctx, e.ID)
		if err != nil {
			return entry, err
		}
		entry.ConversationID = record.ConversationID
		entry.WorkspaceID = record.CollectionID
		entry.Title = record.DisplayTitle
		if entry.Title == "" {
			entry.Title = record.Title
		}
		entry.ID = "conversation:" + record.ConversationID
		if record.ConversationID == "" {
			entry.ID = "conversation:" + e.ID
		}
	}
	s := b.pendingDeletions
	s.mu.Lock()
	defer s.mu.Unlock()
	old, exists := s.entries[entry.ID]
	if exists && old.Failure == "" {
		return old, nil
	}
	entry.Expires = time.Now().Add(deletionGrace)
	s.entries[entry.ID] = entry
	if err := s.save(); err != nil {
		if exists {
			s.entries[entry.ID] = old
		} else {
			delete(s.entries, entry.ID)
		}
		return entry, err
	}
	return entry, nil
}

func (b *Bench) UndoDeletion(id string) error {
	s := b.pendingDeletions
	s.mu.Lock()
	defer s.mu.Unlock()
	entry, ok := s.entries[id]
	// A failed cleanup has already stopped. Clearing its record cancels the
	// failed request without doing anything to the item or its files.
	if !ok || (entry.Failure == "" && !time.Now().Before(entry.Expires)) {
		return errors.New("the undo window has ended")
	}
	delete(s.entries, id)
	if err := s.save(); err != nil {
		s.entries[id] = entry
		return err
	}
	return nil
}

func (b *Bench) watchPendingDeletions() {
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		ticker := time.NewTicker(time.Second)
		defer ticker.Stop()
		for {
			b.finishPendingDeletions(b.ctx, time.Now())
			select {
			case <-b.ctx.Done():
				return
			case <-ticker.C:
			}
		}
	}()
}

func (b *Bench) finishPendingDeletions(ctx context.Context, now time.Time) {
	s := b.pendingDeletions
	for _, entry := range b.PendingDeletions() {
		if ctx.Err() != nil {
			return
		}
		if entry.Expires.After(now) || entry.Failure != "" {
			continue
		}
		// Once expired, Undo rejects the entry. Only this worker finalizes it.
		var err error
		if entry.SessionID != "" {
			reviewing := b.ReviewerRuns(entry.SessionID)
			err = b.DeleteConversation(ctx, entry.SessionID, entry.Stop)
			if err == nil {
				b.ReviewerEndedByHand(reviewing, reviews.EndedDeleted)
			}
		} else {
			_, err = b.DeleteWorkspace(ctx, entry.WorkspaceID, entry.Title, entry.Discard, entry.Stop)
			if err == nil && b.Reviews != nil {
				err = b.Reviews.Forget(entry.WorkspaceID)
			}
		}
		if ctx.Err() != nil {
			return
		}
		s.mu.Lock()
		if err != nil {
			entry.Failure = err.Error()
			s.entries[entry.ID] = entry
		} else {
			delete(s.entries, entry.ID)
		}
		if err := s.save(); err != nil {
			log.Print(fmt.Errorf("save pending deletion %s: %w", entry.ID, err))
		}
		s.mu.Unlock()
	}
}
