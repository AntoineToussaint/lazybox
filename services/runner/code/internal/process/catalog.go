package process

import (
	"slices"
	"strings"
)

// EventType is one event type a process may react to, with the CEL variables
// a condition on it may use (besides event and mission, which every event
// has).
type EventType struct {
	Name   string   `json:"name"`
	Family string   `json:"family"`
	Vars   []string `json:"vars,omitempty"`
	// Synthetic events (timer, stall) are not stored; what they cause is.
	Synthetic bool `json:"synthetic,omitempty"`
}

// Synthetic trigger sources.
const (
	Timer = "timer"
	Stall = "stall"
)

var events = []EventType{
	{Name: "mission.created", Family: "mission"},
	{Name: "mission.brief_changed", Family: "mission"},
	{Name: "mission.process_pinned", Family: "mission"},
	{Name: "mission.canceled", Family: "mission"},
	{Name: "goal.created", Family: "plan", Vars: []string{"goal"}},
	{Name: "assertion.added", Family: "plan", Vars: []string{"goal", "assertion"}},
	{Name: "assertion.changed", Family: "plan", Vars: []string{"goal", "assertion"}},
	{Name: "task.created", Family: "plan", Vars: []string{"task"}},
	{Name: "task.edge_added", Family: "plan", Vars: []string{"task"}},
	{Name: "task.brief_changed", Family: "plan", Vars: []string{"task"}},
	{Name: "task.canceled", Family: "plan", Vars: []string{"task"}},
	{Name: "task.assigned", Family: "assignment", Vars: []string{"task"}},
	{Name: "attempt.started", Family: "assignment", Vars: []string{"task", "attempt"}},
	{Name: "attempt.ended", Family: "assignment", Vars: []string{"task", "attempt"}},
	{Name: "task.handoff", Family: "assignment", Vars: []string{"task", "attempt"}},
	{Name: "task.reported", Family: "progress", Vars: []string{"task"}},
	{Name: "task.question", Family: "progress", Vars: []string{"task", "question"}},
	{Name: "task.answered", Family: "progress", Vars: []string{"task", "question"}},
	{Name: "task.blocked", Family: "progress", Vars: []string{"task"}},
	{Name: "evidence.recorded", Family: "progress", Vars: []string{"task", "evidence"}},
	{Name: "verdict.recorded", Family: "verification", Vars: []string{"task", "verdict"}},
	{Name: "finding.filed", Family: "verification", Vars: []string{"task", "finding"}},
	{Name: "finding.disposed", Family: "verification", Vars: []string{"task", "finding"}},
	{Name: "decision.requested", Family: "decisions", Vars: []string{"decision"}},
	{Name: "decision.recorded", Family: "decisions", Vars: []string{"decision"}},
	{Name: "decision.superseded", Family: "decisions", Vars: []string{"decision"}},
	{Name: "phase.proposed", Family: "flow", Vars: []string{"phase"}},
	{Name: "phase.entered", Family: "flow", Vars: []string{"phase"}},
	{Name: "gate.requested", Family: "flow", Vars: []string{"gate", "task"}},
	{Name: "gate.passed", Family: "flow", Vars: []string{"gate", "task"}},
	{Name: "gate.rejected", Family: "flow", Vars: []string{"gate", "task"}},
	{Name: "escalation.raised", Family: "flow", Vars: []string{"escalation", "task"}},
	{Name: "integration.queued", Family: "integration", Vars: []string{"task", "integration"}},
	{Name: "integration.merged", Family: "integration", Vars: []string{"task", "integration", "pr"}},
	{Name: "integration.failed", Family: "integration", Vars: []string{"task", "integration", "failure"}},
	{Name: "integration.reverted", Family: "integration", Vars: []string{"task", "integration", "pr"}},
	{Name: "check.completed", Family: "integration", Vars: []string{"check", "pr"}},
	{Name: "check.failed", Family: "integration", Vars: []string{"check", "pr"}},
	{Name: "budget.warning", Family: "resources", Vars: []string{"budget"}},
	{Name: "budget.exhausted", Family: "resources", Vars: []string{"budget"}},
	{Name: "wip.held", Family: "resources", Vars: []string{"task"}},
	{Name: "status.reported", Family: "resources", Vars: []string{"status"}},
	{Name: "prompt.submitted", Family: "conversation", Vars: []string{"prompt"}},
	{Name: "snippet.sent", Family: "conversation", Vars: []string{"prompt", "snippet"}},
	{Name: Timer, Family: "synthetic", Synthetic: true},
	{Name: Stall, Family: "synthetic", Vars: []string{"task"}, Synthetic: true},
}

// gateVars are what a gate's when condition sees.
var gateVars = []string{"task", "pr"}

// Events lists every event type a process may name, in catalog order.
func Events() []EventType { return slices.Clone(events) }

// LookupEvent returns the catalog entry for name.
func LookupEvent(name string) (EventType, bool) {
	for _, e := range events {
		if e.Name == name {
			return e, true
		}
	}
	return EventType{}, false
}

// scopes maps each Lazybox tool scope to the events holding it lets a role
// cause. A scope with a parameter is written scope:param (task.create:fix,
// session.spawn:worker). There is deliberately no scope that passes a gate.
var scopes = map[string][]string{
	"mission.read":       nil,
	"evidence.read":      nil,
	"decision.read":      nil,
	"holdout.read":       nil,
	"goal.create":        {"goal.created"},
	"assertion.add":      {"assertion.added"},
	"assertion.change":   {"assertion.changed"},
	"task.create":        {"task.created"},
	"task.edge":          {"task.edge_added"},
	"task.assign":        {"task.assigned", "attempt.started"},
	"task.report":        {"task.reported"},
	"task.question":      {"task.question"},
	"decision.record":    {"decision.recorded"},
	"decision.request":   {"decision.requested"},
	"verdict.record":     {"verdict.recorded"},
	"finding.file":       {"finding.filed"},
	"finding.dispose":    {"finding.disposed"},
	"gate.request":       {"gate.requested"},
	"phase.propose":      {"phase.proposed"},
	"status.report":      {"status.reported"},
	"escalate":           {"escalation.raised"},
	"session.spawn":      {"attempt.started"},
	"session.send":       {"prompt.submitted"},
	"integration.queue":  {"integration.queued"},
	"integration.merge":  {"integration.merged", "integration.failed"},
	"integration.revert": {"integration.reverted"},
}

// parameterized scopes take :param; session.spawn requires one (a role).
var parameterized = map[string]bool{"task.create": true, "session.spawn": true}

// Scopes lists every scope a role may hold.
func Scopes() []string {
	out := make([]string, 0, len(scopes))
	for s := range scopes {
		out = append(out, s)
	}
	slices.Sort(out)
	return out
}

// expandScope resolves a scope as written (with a family.* wildcard or a
// :param) to the catalog scopes it grants.
func expandScope(written string) []string {
	base, _, _ := strings.Cut(written, ":")
	if family, ok := strings.CutSuffix(base, ".*"); ok {
		var out []string
		for _, s := range Scopes() {
			if strings.HasPrefix(s, family+".") {
				out = append(out, s)
			}
		}
		return out
	}
	if _, ok := scopes[base]; ok {
		return []string{base}
	}
	return nil
}

// HasScope reports whether a role holds scope (a catalog name, optionally
// with :param), honouring the wildcards and parameters it was written with:
// goal.* holds goal.create; task.create holds task.create:fix;
// task.create:fix does not hold task.create:feature.
func (r *Role) HasScope(scope string) bool {
	if r == nil {
		return false
	}
	wantBase, wantParam, _ := strings.Cut(scope, ":")
	for _, written := range r.Scopes {
		_, param, hasParam := strings.Cut(written, ":")
		if !slices.Contains(expandScope(written), wantBase) {
			continue
		}
		if !hasParam || param == wantParam {
			return true
		}
	}
	return false
}
