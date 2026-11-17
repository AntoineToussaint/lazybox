package runner

import (
	"context"
	"errors"
	"fmt"
	"log"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/codecheckpoints"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
)

// checkpointTimeout bounds one turn's checkpoint; the turn waits for it.
const checkpointTimeout = 2 * time.Minute

// conversationOf is the conversation an execution belongs to: checkpoints
// span every attempt of it, so a resumed conversation keeps its turns.
func conversationOf(e *Execution) string {
	if e.Saved.ConversationID != "" {
		return e.Saved.ConversationID
	}
	return e.ID
}

// checkpointRepositories is what a turn of e can change: every ready member
// of its workspace, or the repository its directory is in.
func (b *Bench) checkpointRepositories(ctx context.Context, e *Execution) []codecheckpoints.Repository {
	if e.CollectionID != "" {
		if b.Collections == nil {
			return nil
		}
		c, err := b.Collections.Get(e.CollectionID)
		if err != nil {
			return nil
		}
		repos := []codecheckpoints.Repository{}
		for _, m := range c.Members {
			if m.Ready {
				repos = append(repos, codecheckpoints.Repository{Member: m.Name, Path: m.Path})
			}
		}
		return repos
	}
	if e.Cwd == "" {
		return nil
	}
	out, err := exec.CommandContext(ctx, "git", "-C", e.Cwd, "rev-parse", "--show-toplevel").Output()
	top := strings.TrimSpace(string(out))
	if err != nil || top == "" {
		return nil // an empty workspace or a plain directory: nothing to checkpoint
	}
	return []codecheckpoints.Repository{{Member: filepath.Base(top), Path: top}}
}

// watchTurns follows e's turns: it names the conversation after a turn
// (titles.go), checkpoints its workspace before each turn and after it ends,
// then lets reviews follow the turn (review_runs.go). Assistant
// conversations never change code: only naming follows them.
func (b *Bench) watchTurns(e *Execution) {
	live, ok := e.Engine.(*engine.Engine)
	if !ok {
		return
	}
	code := !agent.IsAssistant(e.Purpose) && (b.Checkpoints != nil || b.Reviews != nil)
	conversation := conversationOf(e)
	hooks := engine.TurnHooks{
		After: func(operation string, ended time.Time) {
			// Beside the checkpoint, never ahead of it: naming waits on the provider.
			go b.nameConversation(e)
			if !code {
				return
			}
			if b.Checkpoints != nil {
				b.checkpoint(func(ctx context.Context) error {
					repos := b.checkpointRepositories(ctx, e)
					if len(repos) == 0 {
						return nil
					}
					return b.Checkpoints.EndTurn(ctx, conversation, repos, operation, ended)
				})
			}
			// After the checkpoint: a review asks what the turn changed, and
			// a mission's attempts are checked for overlap (orchestrator_lease.go).
			b.reviewTurnEnded(e, operation)
			b.overlapTurnEnded(e)
		},
	}
	if !agent.IsAssistant(e.Purpose) {
		// What its tools print can say it opened a pull request (items.go).
		hooks.Tool = func(ev agent.Event) { b.itemToolEvent(e, ev) }
	}
	if code && b.Checkpoints != nil {
		hooks.Before = func(op agent.Operation) {
			b.checkpoint(func(ctx context.Context) error {
				repos := b.checkpointRepositories(ctx, e)
				if len(repos) == 0 {
					return nil
				}
				_, err := b.Checkpoints.BeginTurn(ctx, conversation, repos, op.ID, op.Text)
				return err
			})
		}
	}
	live.SetTurnHooks(hooks)
}

// checkpoint runs one bounded checkpoint unless the Runner is shutting down,
// which waits for it. A failed checkpoint never stops the turn.
func (b *Bench) checkpoint(take func(context.Context) error) {
	b.mu.Lock()
	if b.closed {
		b.mu.Unlock()
		return
	}
	b.checkpointing.Add(1)
	b.mu.Unlock()
	defer b.checkpointing.Done()
	ctx, cancel := context.WithTimeout(b.ctx, checkpointTimeout)
	defer cancel()
	if err := take(ctx); err != nil {
		log.Printf("code checkpoint: %v", err)
	}
}

// ErrAgentWorking refuses a revert while an agent could be writing files.
var ErrAgentWorking = errors.New("an agent is working in this workspace; wait for its turn to end, or interrupt it, before reverting files")

// workspaceIdle refuses while any agent sharing e's workspace is in a turn
// or has a message queued: a revert under it would be overwritten, or
// would overwrite what it is writing.
func (b *Bench) workspaceIdle(e *Execution) func() error {
	return func() error {
		for _, other := range b.List() {
			same := other.ID == e.ID || (e.CollectionID != "" && other.CollectionID == e.CollectionID) || (e.CollectionID == "" && other.CollectionID == "" && other.Cwd == e.Cwd)
			if !same {
				continue
			}
			if s := other.Engine.Snapshot(); !s.Closed && (s.Busy || s.Queued > 0) {
				title := s.Title
				if title == "" {
					title = other.ID
				}
				return fmt.Errorf("%w (busy: “%s”)", ErrAgentWorking, title)
			}
		}
		return nil
	}
}

// CodeCheckpoints is the checkpoint record of id's conversation.
func (b *Bench) CodeCheckpoints(id string) (codecheckpoints.Record, error) {
	e, err := b.checkpointed(id)
	if err != nil {
		return codecheckpoints.Record{}, err
	}
	return b.Checkpoints.List(conversationOf(e))
}

func (b *Bench) checkpointed(id string) (*Execution, error) {
	if b.Checkpoints == nil {
		return nil, errors.New("code checkpoints are not enabled on this Runner")
	}
	e, err := b.Get(id)
	if err != nil {
		return nil, err
	}
	if agent.IsAssistant(e.Purpose) {
		return nil, errors.New("assistant conversations have no code checkpoints")
	}
	return e, nil
}

// TurnChanges is what turn seq of id's conversation changed.
func (b *Bench) TurnChanges(ctx context.Context, id string, seq int) ([]codecheckpoints.RepoChanges, bool, error) {
	e, err := b.checkpointed(id)
	if err != nil {
		return nil, false, err
	}
	return b.Checkpoints.Changes(ctx, conversationOf(e), seq)
}

// TurnDiff is one file's change in turn seq.
func (b *Bench) TurnDiff(ctx context.Context, id string, seq int, member, path, original string) (codecheckpoints.Diff, error) {
	e, err := b.checkpointed(id)
	if err != nil {
		return codecheckpoints.Diff{}, err
	}
	return b.Checkpoints.Diff(ctx, conversationOf(e), seq, member, path, original)
}

// RevertPreview says what reverting to before turn seq would change, and
// why it would be refused now ("" when it would not).
func (b *Bench) RevertPreview(ctx context.Context, id string, seq int) ([]codecheckpoints.RepoPreview, string, error) {
	e, err := b.checkpointed(id)
	if err != nil {
		return nil, "", err
	}
	repos, err := b.Checkpoints.Preview(ctx, conversationOf(e), seq, b.checkpointRepositories(ctx, e))
	if err != nil {
		return nil, "", err
	}
	blocked := ""
	if err = b.workspaceIdle(e)(); err != nil {
		blocked = err.Error()
	}
	return repos, blocked, nil
}

// RevertCode restores the files of id's workspace to before turn seq.
func (b *Bench) RevertCode(ctx context.Context, id string, seq int) (codecheckpoints.Revert, error) {
	e, err := b.checkpointed(id)
	if err != nil {
		return codecheckpoints.Revert{}, err
	}
	r, err := b.Checkpoints.Revert(ctx, conversationOf(e), seq, b.workspaceIdle(e))
	if err == nil {
		b.noteRevert(e, r)
	}
	return r, err
}

// UndoRevertCode restores the files the latest revert replaced.
func (b *Bench) UndoRevertCode(ctx context.Context, id, revert string) (codecheckpoints.Revert, error) {
	e, err := b.checkpointed(id)
	if err != nil {
		return codecheckpoints.Revert{}, err
	}
	r, err := b.Checkpoints.Undo(ctx, conversationOf(e), revert, b.workspaceIdle(e))
	if err == nil {
		b.noteRevert(e, r)
	}
	return r, err
}

// noteRevert puts the revert in the conversation, where the person reads
// what happened to the files. The agent is told with the next message.
func (b *Bench) noteRevert(e *Execution, r codecheckpoints.Revert) {
	text := fmt.Sprintf("Files reverted to before turn %d.", r.Turn)
	if r.Kind == "undo" {
		text = fmt.Sprintf("Revert to before turn %d undone; files are back as they were.", r.Turn)
	}
	head, err := b.latest(e)
	if err != nil || head.Archived {
		return
	}
	if s := head.Engine.Snapshot(); s.Closed {
		return
	}
	_ = b.Record(head, agent.Event{Type: "code_reverted", Text: text, Status: r.Kind, RequestID: r.ID})
}

// latest is the newest attempt of e's conversation.
func (b *Bench) latest(e *Execution) (*Execution, error) {
	for i := 0; i < 10000; i++ {
		next, err := b.History.Successor(b.ctx, e.ID)
		if err != nil {
			return e, nil
		}
		if e, err = b.Get(next.ID); err != nil {
			return nil, err
		}
	}
	return nil, errors.New("invalid execution lineage")
}

// CodeRevertNote is what e's agent must be told with its next message after
// a revert, or "".
func (b *Bench) CodeRevertNote(e *Execution) string {
	if b.Checkpoints == nil || agent.IsAssistant(e.Purpose) {
		return ""
	}
	return b.Checkpoints.Note(conversationOf(e))
}

// CodeRevertNoteDelivered records that the agent was sent note.
func (b *Bench) CodeRevertNoteDelivered(e *Execution, note string) error {
	if b.Checkpoints == nil || note == "" {
		return nil
	}
	return b.Checkpoints.NoteDelivered(conversationOf(e), note)
}
