package runner

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
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
	if err != nil || c.State == "deleted" {
		return c, err
	}
	for _, id := range c.OwnerExecutionIDs {
		if b.Active(id) {
			if err = b.StopExecution(ctx, id); err != nil {
				return c, err
			}
		}
	}
	return b.Collections.FinishDeletion(ctx, id, discard, b.Active)
}
