package runner

import (
	"context"
	"errors"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
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
func (b *Bench) DeleteWorkspace(ctx context.Context, id, name string, discard, stop, keepArtifacts bool) (collections.Collection, error) {
	if b.Collections == nil {
		return collections.Collection{}, errors.New("workspaces unavailable")
	}
	c, err := b.Collections.BeginDeletion(ctx, id, name, discard, stop, keepArtifacts, b.Active)
	if err != nil {
		return c, err
	}
	if c.State == "deleted" {
		// A retry after deletion finished: only what may have been left behind.
		if b.Artifacts != nil {
			err = b.Artifacts.FinishWorkspaceDeletion(id, c.KeepArtifacts)
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
		err = b.Artifacts.FinishWorkspaceDeletion(id, c.KeepArtifacts)
	}
	return c, err
}

// RemoveWorkspaceRepository removes one clean worktree while preserving the workspace.
func (b *Bench) RemoveWorkspaceRepository(ctx context.Context, id, name string) (collections.Collection, error) {
	if b.Collections == nil {
		return collections.Collection{}, errors.New("workspaces unavailable")
	}
	if b.runtime != nil {
		b.runtime.control.Lock()
		defer b.runtime.control.Unlock()
	}
	b.terminalStart.Lock()
	defer b.terminalStart.Unlock()
	if b.Terminals != nil {
		for _, terminal := range b.Terminals.List(id) {
			if !terminal.Done {
				return collections.Collection{}, errors.New("close active workspace terminals before removing a repository")
			}
		}
	}
	for _, command := range b.Commands.ListWorkspace(id) {
		if !command.Done || command.CleanupUnconfirmed() {
			return collections.Collection{}, errors.New("stop active workspace actions and commands before removing a repository")
		}
	}
	forgetRuntime := func() error {
		if b.runtime == nil {
			return nil
		}
		return b.runtime.update(id, func(record *runtimeRecord) {
			delete(record.Setup, name)
			delete(record.Archive, name)
			delete(record.Files, name)
			for key := range record.Actions {
				if strings.HasPrefix(key, name+"/") {
					delete(record.Actions, key)
				}
			}
		})
	}
	return b.Collections.RemoveRepositoryMember(ctx, id, name, b.Active, forgetRuntime)
}
