package runner

import (
	"context"
	"errors"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// sharedInstructions are the Runner-wide sections every new coding session
// starts with: the editable Settings → Agents text, its provider-specific
// part, and the built-in command briefing.
type sharedInstructions struct {
	settings, provider, briefing string
	// browser is the conversation's own browser section (browser_briefing.go).
	browser string
}

func (b *Bench) sharedInstructions(provider, purpose string) sharedInstructions {
	prefs := b.Preferences.Snapshot()
	s := sharedInstructions{settings: prefs.Context, provider: prefs.ClaudeContext}
	if provider == "codex" {
		s.provider = prefs.CodexContext
	}
	if accounts.ACPProvider(provider) {
		s.provider = ""
	}
	if b.Config.CLIDir != "" && !agent.IsAssistant(purpose) {
		s.briefing = b.briefing()
	}
	return s
}

func (s sharedInstructions) text() string {
	parts := []string{}
	for _, part := range []string{s.settings, s.provider, s.briefing, s.browser} {
		if part = strings.TrimSpace(part); part != "" {
			parts = append(parts, part)
		}
	}
	return strings.Join(parts, "\n\n")
}

// A running Claude session keeps the system prompt it started with, so new
// instructions must arrive in the conversation. The wording says they replace
// the old ones, the way Codex announces its own instruction changes.
const instructionsPreamble = "[Lazybox] The shared instructions for this conversation were updated. They replace the Lazybox instructions you were given when it started:\n\n"

// InstructionsUpdate returns what a running coding session should receive with
// its next message, or "" when it already has the current shared instructions.
// A session is current when every current section appears in what it started
// with, or when it already received exactly this update. This also covers
// sessions started before updates were tracked.
func (b *Bench) InstructionsUpdate(e *Execution) string {
	if agent.IsAssistant(e.Purpose) || e.Saved.ID == "" {
		return ""
	}
	shared := b.sharedInstructions(e.Profile.Provider, e.Purpose)
	shared.browser = b.browserBriefing(e.Profile.Provider, e.Purpose, e.CollectionID, e.browserTools)
	update := instructionsPreamble + shared.text()
	if shared.text() == "" || e.Saved.Instructions == update {
		return ""
	}
	for _, part := range []string{shared.settings, shared.provider, shared.briefing, shared.browser} {
		if part = strings.TrimSpace(part); part != "" && !strings.Contains(e.Saved.Context, part) {
			return update
		}
	}
	return ""
}

// InstructionsDelivered records that a session received update, so it is
// neither sent again nor reported as pending.
func (b *Bench) InstructionsDelivered(ctx context.Context, e *Execution, update string) error {
	if err := b.History.Amend(ctx, e.ID, func(r *sessionstore.Record) { r.Instructions = update }); err != nil {
		return err
	}
	e.activity.Lock()
	e.Saved.Instructions = update
	e.activity.Unlock()
	return nil
}

// SetPermission changes a running session's permission mode and remembers it,
// so a resume or a subscription rotation keeps the new mode.
func (b *Bench) SetPermission(ctx context.Context, e *Execution, operationID, mode string) (agent.Receipt, error) {
	if err := permissionAllowed(e, mode); err != nil {
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

func permissionAllowed(e *Execution, mode string) error {
	if !agent.PermissionModes[mode] {
		return errors.New("permission must be ask, read-only or yolo")
	}
	if agent.IsAssistant(e.Purpose) {
		return errors.New("assistant conversations are read-only")
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
	if err := permissionAllowed(e, mode); err != nil {
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
