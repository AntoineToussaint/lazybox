package runner

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"time"
)

// StopExecution closes the provider and cancels/waits for every Runner-owned
// command. Pipe producers are not owned processes and cannot be assumed stopped.
func (b *Bench) StopExecution(ctx context.Context, id string) error {
	e, err := b.Get(id)
	if err != nil {
		return err
	}
	if e.RecoveryRequired {
		return errors.New("previous process ownership is unconfirmed; reconcile before cleanup")
	}
	if err = e.Engine.Close(); err != nil {
		return err
	}
	e.activity.Lock()
	defer e.activity.Unlock()
	for _, status := range b.Commands.List(id) {
		if status.CleanupUnconfirmed() {
			return errors.New("command cleanup is unconfirmed; reconcile before releasing workspace resources")
		}
		if status.Done {
			continue
		}
		command, err := b.Commands.Get(status.ID)
		if err != nil {
			return err
		}
		if err = command.Cancel(); err != nil {
			return err
		}
		wait, cancel := context.WithTimeout(ctx, 10*time.Second)
		state, err := command.Wait(wait)
		cancel()
		if err != nil {
			return err
		}
		if state.CleanupUnconfirmed() {
			return errors.New("command cleanup is unconfirmed; reconcile before releasing workspace resources")
		}
	}
	return nil
}
func (b *Bench) DeleteWorkspace(ctx context.Context, id, name string, discard, stop bool) (collections.Collection, error) {
	if b.Collections == nil {
		return collections.Collection{}, errors.New("workspaces unavailable")
	}
	c, err := b.Collections.BeginDeletion(ctx, id, name, discard, stop, b.Active)
	if err != nil {
		return c, err
	}
	if c.State == "deleted" {
		// A retry after deletion finished: only what may have been left behind.
		if b.Artifacts != nil {
			err = b.Artifacts.DeleteWorkspace(id)
		}
		return c, err
	}
	// Actions stop and archive scripts run while the worktrees still exist.
	if err = b.workspaceDeleting(ctx, c); err != nil {
		return c, err
	}
	for _, id := range c.OwnerExecutionIDs {
		if b.Active(id) {
			if err = b.StopExecution(ctx, id); err != nil {
				return c, err
			}
		}
	}
	// A shell must not keep running in a directory that is about to go, and
	// its credential must not outlive the workspace.
	if b.Terminals != nil {
		for _, t := range b.Terminals.List(id) {
			if err = b.CloseTerminal(ctx, t.ID); err != nil {
				return c, err
			}
		}
	}
	if err = b.removeWorkspaceOutput(id); err != nil {
		return c, err
	}
	c, err = b.Collections.FinishDeletion(ctx, id, discard, b.Active)
	if err == nil && c.State == "deleted" && b.runtime != nil {
		// The port block is free for the next workspace.
		err = b.runtime.forget(id)
	}
	if err == nil && c.State == "deleted" {
		// Its delivered pull requests stay as history; its mentions go.
		b.itemsWorkspaceGone(id, c.Name)
	}
	if err == nil && c.State == "deleted" && b.Artifacts != nil {
		// A workspace's artifacts go with it, like its terminals and output.
		err = b.Artifacts.DeleteWorkspace(id)
	}
	return c, err
}
