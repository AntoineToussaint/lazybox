package process

import (
	"fmt"
	"slices"
	"strings"
)

// Reaction is one thing a process does when an event arrives: a rule, a
// role's trigger, or a notification.
type Reaction struct {
	// Kind is "rule", "trigger" or "notification".
	Kind string `json:"kind"`
	// Path addresses it in the process: rules[2], roles.coordinator.triggers[0],
	// notifications[1].
	Path string `json:"path"`
	// Condition is the CEL source; empty fires always.
	Condition string `json:"condition,omitempty"`
	// Action is a rule's action.
	Action *Action `json:"action,omitempty"`
	// Role is the role a trigger wakes, or the role a rule's action starts,
	// wakes, sends to, stops or assigns; Scopes and Permission are that role's.
	Role       string   `json:"role,omitempty"`
	Scopes     []string `json:"scopes,omitempty"`
	Permission string   `json:"permission,omitempty"`
	// Manual triggers wait for a person to start the role.
	Manual bool `json:"manual,omitempty"`
	// Limit is a rule's count limit per subject.
	Limit int `json:"limit,omitempty"`
	// To is where a notification goes.
	To []string `json:"to,omitempty"`
	// Emits are the event types this reaction can cause, directly or through
	// the role it starts.
	Emits []string `json:"emits,omitempty"`
	// Summary says it in one line.
	Summary string `json:"summary"`
}

// Reactions lists what the process does on an event, in firing order: rules
// in file order, then role triggers (roles in file order), then
// notifications. Conditions are not evaluated: this is everything that may
// fire.
func (p *Process) Reactions(event string) ([]Reaction, error) {
	if _, ok := LookupEvent(event); !ok {
		return nil, fmt.Errorf("unknown event type %q; see `lazybox process events`", event)
	}
	g := p.graph()
	var out []Reaction
	for i, r := range p.Rules {
		if !slices.Contains(r.On, event) {
			continue
		}
		action := r.Do
		x := Reaction{Kind: "rule", Path: Path{"rules"}.Index(i).String(), Condition: r.If, Action: &action, Limit: r.Limit, Role: action.role()}
		x.Emits = g.emitsOf(x.Path)
		x.Summary = describeAction(action)
		out = append(out, p.withRole(x))
	}
	for _, name := range p.Roles.Keys {
		role := p.Roles.Values[name]
		for j, t := range role.Triggers {
			if !slices.Contains(t.On, event) {
				continue
			}
			x := Reaction{Kind: "trigger", Path: Path{"roles", name, "triggers"}.Index(j).String(), Condition: t.If, Role: name, Manual: t.Mode == "manual"}
			x.Emits = g.emitsOf(x.Path)
			x.Summary = "wake " + name
			if x.Manual {
				x.Summary = "offer a person to start " + name
			}
			out = append(out, p.withRole(x))
		}
	}
	for i, n := range p.Notifications {
		if slices.Contains(n.On, event) {
			summary := "notify " + strings.Join(n.To, " and ")
			if n.Digest != "" {
				summary += " (" + n.Digest + " digest)"
			}
			out = append(out, Reaction{Kind: "notification", Path: Path{"notifications"}.Index(i).String(), To: n.To, Summary: summary})
		}
	}
	return out, nil
}

func (p *Process) withRole(x Reaction) Reaction {
	if r, ok := p.Roles.Get(x.Role); ok && r != nil {
		x.Scopes, x.Permission = r.Scopes, r.Permission
	}
	return x
}

// role is the role an action acts on.
func (a Action) role() string {
	for _, r := range []string{a.Start, a.Wake, a.Send, a.Stop, a.Assign} {
		if r != "" {
			return r
		}
	}
	return ""
}

func describeAction(a Action) string {
	switch {
	case a.Start != "":
		return "start " + a.Start
	case a.Wake != "":
		return "wake " + a.Wake
	case a.Send != "" && a.Snippet != "":
		return fmt.Sprintf("send snippet %s to %s", a.Snippet, a.Send)
	case a.Send != "":
		return fmt.Sprintf("send %q to %s", a.Text, a.Send)
	case a.RequestGate != "":
		return "request gate " + a.RequestGate
	case a.EnterPhase != "":
		return "enter phase " + a.EnterPhase
	case a.EnterNextPhase:
		return "enter the next phase"
	case a.CreateTask != "" && a.Assign != "":
		return fmt.Sprintf("create a %s task for %s", a.CreateTask, a.Assign)
	case a.CreateTask != "":
		return fmt.Sprintf("create a %s task", a.CreateTask)
	case len(a.Notify) > 0:
		return "notify " + strings.Join(a.Notify, " and ")
	case a.Escalate != "":
		return "escalate: " + a.Escalate
	case a.Stop != "":
		return "stop " + a.Stop
	case a.Exec != "":
		return "run " + a.Exec
	case a.IntegrationQueue != "":
		return "queue the task for integration"
	}
	return "nothing"
}

// FormatReactions writes Reactions for a person: one numbered line each,
// with its condition, the role's permission and scopes, and what it causes.
func FormatReactions(event string, reactions []Reaction) string {
	var b strings.Builder
	if len(reactions) == 0 {
		fmt.Fprintf(&b, "Nothing reacts to %s.\n", event)
		return b.String()
	}
	fmt.Fprintf(&b, "On %s, in order:\n", event)
	for i, x := range reactions {
		fmt.Fprintf(&b, "%2d. %s  [%s]\n", i+1, x.Summary, x.Path)
		if x.Condition != "" {
			fmt.Fprintf(&b, "    if   %s\n", x.Condition)
		}
		if x.Limit > 0 {
			fmt.Fprintf(&b, "    at most %d time(s) per subject\n", x.Limit)
		}
		if x.Role != "" && x.Kind != "notification" {
			scopes := "none"
			if len(x.Scopes) > 0 {
				scopes = strings.Join(x.Scopes, ", ")
			}
			fmt.Fprintf(&b, "    role %s (%s), scopes: %s\n", x.Role, x.Permission, scopes)
		}
		if len(x.Emits) > 0 {
			fmt.Fprintf(&b, "    may cause %s\n", strings.Join(x.Emits, ", "))
		}
	}
	return b.String()
}

// edge is one way an event can cause another.
type edge struct {
	from, to string
	via      string // the reaction's path
	what     string // how, for a person
	bound    string // what limits it; empty when nothing does
}

type reactionGraph struct{ edges []edge }

func (g *reactionGraph) emitsOf(path string) []string {
	var out []string
	for _, e := range g.edges {
		if e.via == path && !slices.Contains(out, e.to) {
			out = append(out, e.to)
		}
	}
	return out
}

// graph builds every way one event can cause another under this process.
func (p *Process) graph() *reactionGraph {
	g := &reactionGraph{}
	missionBound := ""
	if b := p.budgetOf("mission"); b != nil {
		missionBound = "budgets.mission"
	}
	agentBound := func(role string) string {
		if r, ok := p.Roles.Get(role); ok && r != nil && r.Budget != nil && *r.Budget != (Budget{}) {
			return "roles." + role + ".budget"
		}
		return missionBound
	}
	// agentEmits: what a role can cause once running, through its scopes and
	// the roles it may spawn, each with the bound of the agent that causes it.
	type caused struct{ event, role string }
	agentEmits := func(role string) []caused {
		var out []caused
		seen := map[string]bool{}
		var walk func(string)
		walk = func(name string) {
			if seen[name] {
				return
			}
			seen[name] = true
			out = append(out, caused{"attempt.ended", name})
			r, ok := p.Roles.Get(name)
			if !ok || r == nil {
				return
			}
			for _, written := range r.Scopes {
				for _, s := range expandScope(written) {
					for _, e := range scopes[s] {
						out = append(out, caused{e, name})
					}
				}
				if base, spawned, ok := strings.Cut(written, ":"); ok && base == "session.spawn" {
					walk(spawned)
				}
			}
		}
		walk(role)
		return out
	}
	add := func(from []string, via, what, bound string, to ...string) {
		for _, f := range from {
			for _, t := range to {
				g.edges = append(g.edges, edge{from: f, to: t, via: via, what: what, bound: bound})
			}
		}
	}
	addAgent := func(from []string, via, role, limit string) {
		for _, c := range agentEmits(role) {
			bound := limit
			if bound == "" {
				bound = agentBound(c.role)
			}
			add(from, via, c.role+" (agent)", bound, c.event)
		}
	}
	addAction := func(on []string, via string, a Action, limit int) {
		bound := ""
		if limit > 0 {
			bound = fmt.Sprintf("%s.limit", via)
		}
		what := describeAction(a)
		switch {
		case a.Start != "":
			add(on, via, what, bound, "attempt.started")
			addAgent(on, via, a.Start, bound)
		case a.Wake != "":
			addAgent(on, via, a.Wake, bound)
		case a.Send != "":
			add(on, via, what, bound, "prompt.submitted")
			if a.Snippet != "" {
				add(on, via, what, bound, "snippet.sent")
			}
			addAgent(on, via, a.Send, bound)
		case a.RequestGate != "":
			add(on, via, what, bound, "gate.requested")
			if gate, ok := p.Gates.Get(a.RequestGate); ok && gate != nil {
				switch gate.Kind {
				case "rule", "check":
					add(on, via, "gate "+a.RequestGate+" passes on its condition", bound, "gate.passed", "gate.rejected")
				case "agent":
					for _, c := range []string{"gate.passed", "gate.rejected"} {
						b := bound
						if b == "" {
							b = agentBound(gate.Role)
						}
						add(on, via, gate.Role+" judges gate "+a.RequestGate, b, c)
					}
				}
			}
		case a.EnterPhase != "", a.EnterNextPhase:
			add(on, via, what, bound, "phase.entered")
		case a.CreateTask != "":
			b := bound
			if b == "" && a.CreateTask == "fix" && p.budgetOf("task") != nil && p.budgetOf("task").Retries > 0 {
				b = "budgets.task.retries"
			}
			add(on, via, what, b, "task.created")
			if a.Assign != "" {
				add(on, via, what, b, "task.assigned", "attempt.started")
				for _, c := range agentEmits(a.Assign) {
					add(on, via, c.role+" (agent)", firstOf(b, agentBound(c.role)), c.event)
				}
			}
		case a.Escalate != "":
			add(on, via, what, bound, "escalation.raised")
		case a.Stop != "":
			add(on, via, what, bound, "attempt.ended")
		case a.Exec != "":
			add(on, via, what, bound, "evidence.recorded")
		case a.IntegrationQueue != "":
			add(on, via, what, bound, "integration.queued", "integration.merged", "integration.failed", "check.completed", "check.failed")
		}
	}
	for i, r := range p.Rules {
		addAction(r.On, Path{"rules"}.Index(i).String(), r.Do, r.Limit)
	}
	for _, name := range p.Roles.Keys {
		r := p.Roles.Values[name]
		if r == nil {
			continue
		}
		for j, t := range r.Triggers {
			if t.Mode == "manual" {
				continue // a person starts it: no automatic edge
			}
			addAgent(t.On, Path{"roles", name, "triggers"}.Index(j).String(), name, "")
		}
	}
	return g
}

func firstOf(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

func (p *Process) budgetOf(scope string) *Budget {
	if p.Budgets == nil {
		return nil
	}
	b := p.Budgets.Mission
	if scope == "task" {
		b = p.Budgets.Task
	}
	if b == nil || *b == (Budget{}) {
		return nil
	}
	return b
}

// unboundedCycles finds trigger cycles no limit or budget stops: one per
// strongly connected set of events, written as the chain of reactions.
func (g *reactionGraph) unboundedCycles() [][]edge {
	next := map[string][]edge{}
	var nodes []string
	for _, e := range g.edges {
		if e.bound != "" {
			continue
		}
		if _, ok := next[e.from]; !ok {
			nodes = append(nodes, e.from)
		}
		next[e.from] = append(next[e.from], e)
	}
	var cycles [][]edge
	reported := map[string]bool{}
	for _, start := range nodes {
		if reported[start] {
			continue
		}
		// Breadth-first search for the shortest way back to start.
		type step struct {
			at   string
			path []edge
		}
		queue := []step{{at: start}}
		visited := map[string]bool{start: true}
		for len(queue) > 0 {
			s := queue[0]
			queue = queue[1:]
			var found []edge
			for _, e := range next[s.at] {
				path := append(slices.Clone(s.path), e)
				if e.to == start {
					found = path
					break
				}
				if !visited[e.to] {
					visited[e.to] = true
					queue = append(queue, step{at: e.to, path: path})
				}
			}
			if found != nil {
				for _, e := range found {
					reported[e.from] = true
				}
				cycles = append(cycles, found)
				break
			}
		}
	}
	return cycles
}

func describeCycle(cycle []edge) string {
	var b strings.Builder
	for _, e := range cycle {
		fmt.Fprintf(&b, "%s → [%s: %s] → ", e.from, e.via, e.what)
	}
	b.WriteString(cycle[0].from)
	return b.String()
}
