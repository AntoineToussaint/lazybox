package runner

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Delivery outcomes (audits/2026-11-22/delivery-outcomes-roadmap.md, milestone E).
//
// One projection, delivery.Project, serves every lookup: by conversation
// (`lazybox delivery status`, the conversation view) and by Mission task (the
// board, the acceptance inbox). A conversation working a Mission task and that
// task resolve to the same attempt, so both say the same thing, before and
// after a Runner restart: everything comes from the work log and the delivery
// journal. The workspace's current files are fingerprinted for the headline
// candidate on every lookup: a cached answer would call files current that a
// commit or an edit had already replaced, and say Delivered for them. Lookups
// that arrive while a fingerprint of the same workspace is under way (a board
// refreshing several tasks at once) share that one.

type fingerprintCache struct {
	mu       sync.Mutex
	inFlight map[string]*fingerprintCall
}

type fingerprintCall struct {
	done      chan struct{}
	candidate string
}

// currentCandidate is the workspace's candidate now, or "" when it is gone or
// cannot be read: an outcome is still served, only without a current one.
func (b *Bench) currentCandidate(workspace string) string {
	if workspace == "" {
		return ""
	}
	c := &b.fingerprints
	c.mu.Lock()
	if call, ok := c.inFlight[workspace]; ok {
		c.mu.Unlock()
		<-call.done
		return call.candidate
	}
	if c.inFlight == nil {
		c.inFlight = map[string]*fingerprintCall{}
	}
	call := &fingerprintCall{done: make(chan struct{})}
	c.inFlight[workspace] = call
	c.mu.Unlock()
	if candidate, err := b.candidate(workspace); err == nil {
		call.candidate = candidate
	}
	c.mu.Lock()
	delete(c.inFlight, workspace)
	c.mu.Unlock()
	close(call.done)
	return call.candidate
}

// ErrNoContract: nothing names what this conversation or task must deliver.
var ErrNoContract = errors.New("no delivery contract")

// DeliveryOutcome is the outcome of e's conversation: its live Mission
// attempt, else the spec it bound, else the last Mission attempt it worked.
func (b *Bench) DeliveryOutcome(e *Execution) (delivery.Outcome, error) {
	if b.Delivery == nil {
		return delivery.Outcome{}, errors.New("delivery evidence is unavailable on this Runner")
	}
	conversation := conversationOf(e)
	check, _, mission, err := b.missionContract(conversation)
	if err != nil {
		return delivery.Outcome{}, err
	}
	if mission {
		return b.TaskDeliveryOutcome(check.Subject.Mission, check.Subject.Task)
	}
	if binding, ok := b.Delivery.Binding(conversation); ok {
		o := delivery.Project(binding.Contract, binding.Attempt, delivery.Subject{Conversation: conversation}, b.currentCandidate(e.CollectionID), "", b.Delivery.Observations())
		b.markOutputs(&o)
		return o, nil
	}
	if missionID, taskID, ok := b.lastMissionTask(conversation); ok {
		return b.TaskDeliveryOutcome(missionID, taskID)
	}
	return delivery.Outcome{}, fmt.Errorf("%w: bind a spec with `lazybox delivery bind <spec-id>@<version>`, or work a Mission task whose plan pins one", ErrNoContract)
}

// lastMissionTask is the task of the latest Mission attempt conversation worked.
func (b *Bench) lastMissionTask(conversation string) (string, string, bool) {
	if b.Work == nil || conversation == "" {
		return "", "", false
	}
	var mission, task string
	var latest time.Time
	for _, m := range b.Work.Missions() {
		if m.State == nil {
			continue
		}
		for _, t := range m.State.Tasks {
			for _, at := range t.Attempts {
				if at.Conversation == conversation && !at.Assigned.Before(latest) {
					mission, task, latest = m.State.ID, t.ID, at.Assigned
				}
			}
		}
	}
	return mission, task, mission != ""
}

// TaskDeliveryOutcome is the outcome of a Mission task's latest attempt,
// against its approved plan's contract.
func (b *Bench) TaskDeliveryOutcome(missionID, taskID string) (delivery.Outcome, error) {
	if b.Delivery == nil || b.Work == nil {
		return delivery.Outcome{}, errors.New("delivery evidence is unavailable on this Runner")
	}
	state, err := b.Work.Mission(missionID)
	if err != nil {
		return delivery.Outcome{}, err
	}
	t := state.Task(taskID)
	if t == nil {
		return delivery.Outcome{}, os.ErrNotExist
	}
	at := t.Current()
	if at == nil {
		return delivery.Outcome{}, fmt.Errorf("%w: the task has no attempt yet", ErrNoContract)
	}
	_, plan := state.ApprovedTaskPlan(at.Grant, t.ID)
	if plan == nil || plan.Contract == nil || len(plan.Contract.Bindings) == 0 {
		return delivery.Outcome{}, fmt.Errorf("%w: the task's approved plan pins no spec.v2 contract", ErrNoContract)
	}
	o := delivery.Project(fromPlanContract(plan.Contract), at.ID, delivery.Subject{Conversation: at.Conversation, Mission: missionID, Task: taskID},
		b.currentCandidate(at.Workspace), taskDecision(state, t), b.Delivery.Observations())
	b.markOutputs(&o)
	return o, nil
}

// taskDecision is the person's latest decision at the task's gate.
func taskDecision(state *work.State, t *work.Task) string {
	if state.Done(t) {
		return delivery.Accepted
	}
	for i := len(t.Gates) - 1; i >= 0; i-- {
		switch t.Gates[i].Type {
		case work.GateRejected:
			return delivery.Rejected
		case work.GatePassed:
			return ""
		}
	}
	return ""
}

// markOutputs names each command run and says whether its full output is still kept.
func (b *Bench) markOutputs(o *delivery.Outcome) {
	for i := range o.Candidates {
		for j := range o.Candidates[i].Criteria {
			evidence := o.Candidates[i].Criteria[j].Evidence
			for k := range evidence {
				if evidence[k].Command != "" {
					evidence[k].Title, evidence[k].Output = b.runDetail(evidence[k].Command)
				}
			}
		}
	}
}

func (b *Bench) runDetail(command string) (title, output string) {
	if b.Commands == nil {
		return "", "unavailable"
	}
	c, err := b.Commands.Get(command)
	if err != nil {
		return "", "unavailable"
	}
	state := c.Status()
	if state.Report == nil || state.Report.OutputPath == "" {
		return state.Spec.Title, "unavailable"
	}
	if _, err := os.Stat(state.Report.OutputPath); err != nil {
		return state.Spec.Title, "unavailable"
	}
	return state.Spec.Title, "available"
}

// PublishDeliveryOutcome publishes o as an immutable verdict snapshot: a new
// artifact (kind report, schema delivery-outcome.v1) or the next version of
// artifactID. Earlier versions keep the verdict they were published with.
func (b *Bench) PublishDeliveryOutcome(o delivery.Outcome, workspace, artifactID string, author artifacts.Author, source artifacts.Source) (artifacts.Document, error) {
	if b.Artifacts == nil {
		return artifacts.Document{}, errors.New("artifacts are unavailable on this Runner")
	}
	content, err := json.MarshalIndent(o, "", "  ")
	if err != nil {
		return artifacts.Document{}, err
	}
	put := artifacts.Put{ID: artifactID, Content: string(content), Author: author, Source: source}
	if artifactID == "" {
		put.Title = "Delivery outcome · " + o.Contract.Name()
		put.Kind, put.Format = "report", "json"
	}
	return b.Artifacts.Put(workspace, put)
}

// DeliveryWorkspace is the workspace an outcome's work happens in: its
// conversation's, else the Mission attempt's.
func (b *Bench) DeliveryWorkspace(o delivery.Outcome) string {
	if b.Work != nil && o.Subject.Mission != "" {
		if state, err := b.Work.Mission(o.Subject.Mission); err == nil {
			if t := state.Task(o.Subject.Task); t != nil {
				for _, at := range t.Attempts {
					if at.ID == o.Attempt && at.Workspace != "" {
						return at.Workspace
					}
				}
			}
		}
	}
	for _, e := range b.List() {
		if conversationOf(e) == o.Subject.Conversation {
			return e.CollectionID
		}
	}
	return ""
}
