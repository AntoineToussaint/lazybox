package runner

import (
	"context"
	"errors"
	"os"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
)

// RewindConversation replaces a conversation from one of the person's
// messages on, in place: the agent of attempt id is stopped, and its
// successor continues the same conversation from a fork of the native
// session that ends just before that message. The conversation's timeline
// then stops at the message (Timeline caps); what was replaced stays in the
// earlier attempts' journals and in the provider's own session. The caller
// sends the revised message to the attempt returned. Workspace files are
// left as they are. If the provider cannot start the fork, the successor
// drops its cut and stands as an ordinary failed resume of the conversation
// as it was (openWithID): nothing is lost, and it can be resumed.
func (b *Bench) RewindConversation(ctx context.Context, id string, before uint64) (*Execution, error) {
	if id == "" || before == 0 {
		return nil, errors.New("select the message to replace")
	}
	b.forkMu.Lock()
	defer b.forkMu.Unlock()
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	// A request repeated after a lost response finds the attempt it started.
	if successor, err := b.History.Successor(ctx, id); err == nil {
		if successor.Cut != before {
			return nil, errConversationMoved
		}
		return b.Get(successor.ID)
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	e, err := b.Get(id)
	if err != nil {
		return nil, err
	}
	if (e.Saved.Provider != "codex" && e.Saved.Provider != "claude") || agent.IsAssistant(e.Purpose) {
		return nil, agent.ErrUnsupported
	}
	timeline, err := b.OpenTimeline(id)
	if err != nil {
		return nil, err
	}
	state := e.Engine.Snapshot()
	// The same boundary a restart in a conversation of its own is cut at.
	fork, err := b.forkBoundary(ctx, e, timeline, "", before, nil)
	if err != nil {
		return nil, err
	}
	fork.id, fork.cut, fork.history = commands.ID(), before, nil
	// Everything that can refuse the successor without starting it is checked
	// before the agent is stopped.
	if e.RecoveryRequired {
		return nil, errors.New("previous process ownership is unconfirmed; stop/reconcile its agent and commands before editing")
	}
	if _, _, err := b.continuable(ctx, e); err != nil {
		return nil, err
	}
	// Whatever the agent and its commands are doing came after the message
	// being replaced.
	if !e.Archived && !state.Closed {
		if err := b.StopExecution(ctx, id); err != nil {
			return nil, err
		}
	}
	return b.succeed(ctx, id, false, &fork)
}
