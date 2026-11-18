package runner

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// SetPermission changes a running session's permission mode and remembers it,
// so a resume or a subscription rotation keeps the new mode.
func (b *Bench) SetPermission(ctx context.Context, e *Execution, operationID, mode string) (agent.Receipt, error) {
	if err := b.permissionAllowed(e, mode); err != nil {
		return agent.Receipt{}, err
	}
	r, err := e.Engine.Apply(ctx, agent.Operation{ID: operationID, Kind: agent.Permission, Permission: mode})
	if err != nil || r.Delivery == "failed" {
		return r, err
	}
	if err = b.remember(ctx, e, mode); err != nil {
		return r, err
	}
	// The conversation says what changed, where the person reads it.
	_ = b.Record(e, agent.Event{Type: "permission_changed", Text: mode, Status: "live"})
	return r, nil
}

func (b *Bench) permissionAllowed(e *Execution, mode string) error {
	if !agent.PermissionModes[mode] {
		return errors.New("permission must be ask, read-only or yolo")
	}
	if agent.IsAssistant(e.Purpose) {
		return errors.New("assistant conversations are read-only")
	}
	if mode != "read-only" && b.Reviews != nil {
		if match, ok := b.Reviews.ReviewerRun(e.ID, conversationOf(e)); ok && match.Run.TemplateID != "" {
			return errors.New("this review template requires read-only permissions")
		}
	}
	if mode == "yolo" && e.Options.Workflow == "plan" {
		return errors.New("YOLO is incompatible with plan mode")
	}
	return nil
}

func (b *Bench) remember(ctx context.Context, e *Execution, mode string) error {
	if err := b.History.Amend(ctx, e.ID, func(r *sessionstore.Record) { r.Options.Permission = mode }); err != nil {
		return err
	}
	e.activity.Lock()
	e.Options.Permission = mode
	e.Saved.Options.Permission = mode
	e.activity.Unlock()
	return nil
}

// RestartWithPermission changes mode for an agent whose running worker predates
// live permission changes: the agent is ended between turns and the same
// provider conversation resumes in a new agent started in that mode.
func (b *Bench) RestartWithPermission(ctx context.Context, e *Execution, mode string) (*Execution, error) {
	if err := b.permissionAllowed(e, mode); err != nil {
		return nil, err
	}
	if err := restartable(e); err != nil {
		return nil, err
	}
	state := e.Engine.Snapshot()
	if state.Reference == "" || !state.Capabilities.Resume {
		return nil, errors.New("this conversation cannot be resumed by its provider; start a new conversation with that permission")
	}
	if err := b.remember(ctx, e, mode); err != nil {
		return nil, err
	}
	next, err := b.restart(ctx, e)
	if err != nil {
		return nil, err
	}
	_ = b.Record(next, agent.Event{Type: "permission_changed", Text: mode, Status: "restart"})
	return next, nil
}

// restartable says why a conversation's agent cannot be restarted between
// turns to take a new setting, or nil.
func restartable(e *Execution) error {
	state := e.Engine.Snapshot()
	if state.Closed {
		return errors.New("the conversation has ended; resume it to continue")
	}
	if state.Busy || len(state.Pending) > 0 || state.Queued > 0 {
		return errors.New("wait for the agent to finish its turn, or interrupt it, before restarting it")
	}
	return nil
}

// restart ends e's agent and resumes the same provider conversation in a new
// one, started with the options now remembered.
func (b *Bench) restart(ctx context.Context, e *Execution) (*Execution, error) {
	if err := b.StopExecution(ctx, e.ID); err != nil {
		return nil, err
	}
	return b.Resume(ctx, e.ID, false)
}

// SetModel switches a running session's model for its following turns and
// remembers it, so a resume or a subscription rotation keeps the new model.
// Asked for during a turn, the change waits for that turn to end (the
// adapters apply it to the next one), and the conversation says so.
func (b *Bench) SetModel(ctx context.Context, e *Execution, operationID, model string) (agent.Receipt, error) {
	if err := agent.ValidModel(model); err != nil {
		return agent.Receipt{}, err
	}
	when := "live"
	if e.Engine.Snapshot().Busy {
		when = "next_turn"
	}
	r, err := e.Engine.Apply(ctx, agent.Operation{ID: operationID, Kind: agent.Model, Model: model})
	if err != nil || r.Delivery == "failed" {
		return r, err
	}
	if err = b.rememberModel(ctx, e, model); err != nil {
		return r, err
	}
	_ = b.Record(e, agent.Event{Type: "model_changed", Text: model, Status: when})
	return r, nil
}

// RestartWithModel switches the model of an agent whose running worker cannot
// switch live: the agent is ended between turns and the same provider
// conversation resumes in a new agent started with that model.
func (b *Bench) RestartWithModel(ctx context.Context, e *Execution, model string) (*Execution, error) {
	if err := agent.ValidModel(model); err != nil {
		return nil, err
	}
	if err := restartable(e); err != nil {
		return nil, err
	}
	state := e.Engine.Snapshot()
	if state.Reference == "" || !state.Capabilities.Resume {
		return nil, errors.New("this conversation cannot be resumed by its provider; start a new conversation with that model")
	}
	if err := b.rememberModel(ctx, e, model); err != nil {
		return nil, err
	}
	next, err := b.restart(ctx, e)
	if err != nil {
		return nil, err
	}
	_ = b.Record(next, agent.Event{Type: "model_changed", Text: model, Status: "restart"})
	return next, nil
}

func (b *Bench) rememberModel(ctx context.Context, e *Execution, model string) error {
	if err := b.History.Amend(ctx, e.ID, func(r *sessionstore.Record) { r.Options.Model = model }); err != nil {
		return err
	}
	e.activity.Lock()
	e.Options.Model = model
	e.Saved.Options.Model = model
	e.activity.Unlock()
	return nil
}
