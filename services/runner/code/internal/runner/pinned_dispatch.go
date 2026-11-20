package runner

import (
	"context"
	"errors"
	"fmt"
	"os"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// The grant lock serializes recovery with a retried RPC. Assignment reserves
// capacity and both resource IDs before either resource exists. AttemptStarted
// acknowledges delivery; the engine deduplicates delivery by operation ID.
func (b *Bench) finishPinnedDispatch(ctx context.Context, parent *Execution, state *work.State, task *work.Task, at *work.Attempt) error {
	parent, err := b.latest(parent)
	if err != nil {
		return err
	}
	grant := state.Grant(at.Grant)
	if task.Canceled || state.Canceled || state.Done(task) || at.Ended || grant == nil || !state.Live(grant) {
		return fmt.Errorf("%w: assignment is no longer active", work.ErrRefused)
	}
	_, plan := state.ApprovedTaskPlan(at.Grant, task.ID)
	if plan == nil || len(plan.Baseline) == 0 || b.Collections == nil {
		return fmt.Errorf("%w: reserved workspace baseline unavailable", work.ErrRefused)
	}
	d := at.Dispatch
	spec := collections.Spec{Name: truncateRunes(d.Title, 120)}
	for _, m := range plan.Baseline {
		spec.Members = append(spec.Members, collections.MemberSpec{Name: m.Name, RepositoryID: m.Repository, BaseRef: m.Commit})
	}
	ws, err := b.Collections.CreateReserved(ctx, at.Workspace, spec)
	if err != nil {
		return fmt.Errorf("prepare reserved workspace: %w", err)
	}
	if ws.ID != at.Workspace {
		return errors.New("workspace reservation identity mismatch")
	}
	if err = b.WorkspaceCreated(ctx, ws.ID, nil, nil); err != nil {
		return err
	}
	child, err := b.Get(at.Execution)
	if errors.Is(err, os.ErrNotExist) {
		deleted, checkErr := b.History.Deleted(ctx, at.Execution)
		if checkErr != nil {
			return checkErr
		}
		if len(deleted) > 0 {
			return errors.New("reserved conversation was deleted")
		}
		opts := accounts.Options{Model: d.Model, Permission: work.NarrowerPermission(parent.Options.Permission, work.NarrowerPermission(grant.Ceiling.Permission, at.Permission)), NoRotation: true, Instructions: fmt.Sprintf(childBriefing, b.agentTitle(ctx, parent), state.Title, at.Role)}
		child, err = b.openWithID(at.Execution, at.Profile, "", "", opts, parent.Saved.GatewayID, "", "", ws.ID, "coding", "", false)
	}
	if err != nil {
		return fmt.Errorf("open reserved conversation: %w", err)
	}
	if child.CollectionID != at.Workspace {
		return errors.New("reserved conversation workspace mismatch")
	}
	_ = b.History.Rename(ctx, child.ID, d.Title)
	origin := agent.Origin{Kind: agent.OriginAgentBrief, Summary: "Task from “" + d.ParentTitle + "”: " + d.Title, Detail: at.Role, From: d.ParentTitle, Conversation: at.Parent, Session: d.ParentSession}
	operation := "spawn-" + task.ID + "-" + child.ID
	current, err := b.Work.Mission(state.ID)
	if err != nil {
		return err
	}
	target := current.Task(task.ID)
	if target == nil || target.Current() == nil || target.Current().ID != at.ID || target.Current().Ended || target.Canceled || current.Done(target) || !current.Live(current.Grant(at.Grant)) {
		return fmt.Errorf("%w: reservation is no longer active", work.ErrRefused)
	}
	if !b.Prompts.Submitted(at.Conversation, operation) {
		if _, err = b.deliverAgentMessage(ctx, child, operation, d.Brief, origin, work.PromptSource{Kind: work.SourceAgent, Mission: state.ID, Task: task.ID}); err != nil {
			return fmt.Errorf("deliver reserved task brief: %w", err)
		}
	}
	_, _, err = b.Work.Append(state.ID, work.Draft{Subject: work.TaskSubject(task.ID), Type: work.AttemptStarted, Actor: work.Actor{Kind: work.ActorLazybox, Conversation: at.Conversation, Execution: at.Execution}, Payload: work.AttemptPayload{Task: task.ID, Attempt: at.ID, Execution: at.Execution}})
	if err == nil {
		b.recordLineageEvent(parent, "agent_spawned", "Started “"+d.Title+"” in a pinned workspace", map[string]any{"task": task.ID, "mission": state.ID, "conversation": at.Conversation, "session": at.Execution})
		b.orch().broadcast()
	}
	return err
}

// Resume only incomplete reservations from the previous Runner instance.
// An explicit retry with the same key can recover a transient startup error.
func (b *Bench) resumePinnedDispatches() {
	for _, at := range b.Work.Attempts() {
		if at.Attempt.Dispatch == nil || !at.Attempt.Started.IsZero() {
			continue
		}
		b.watchers.Add(1)
		go func() {
			defer b.watchers.Done()
			unlock := b.orch().lock(at.Attempt.Grant)
			defer unlock()
			state, err := b.Work.Mission(at.Mission)
			if err != nil {
				return
			}
			task := state.Task(at.Task)
			if task == nil {
				return
			}
			a := task.Current()
			if a == nil || a.ID != at.Attempt.ID || a.Ended || !a.Started.IsZero() {
				return
			}
			parent, err := b.Get(a.Parent)
			if err != nil {
				return
			}
			if err = b.finishPinnedDispatch(b.ctx, parent, state, task, a); err != nil {
				b.recordLineageEvent(parent, "dispatch_pending", "Reserved task awaits retry: "+err.Error(), map[string]any{"task": task.ID, "mission": state.ID})
			}
		}()
	}
}
