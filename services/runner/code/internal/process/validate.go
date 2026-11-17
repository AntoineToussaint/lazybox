package process

import (
	"fmt"
	"io/fs"
	"path"
	"regexp"
	"slices"
	"strings"
	"time"
)

// permissionRank orders permissions for the ceiling.
var permissionRank = map[string]int{"read_only": 0, "ask": 1, "yolo": 2}

var identifier = regexp.MustCompile(`^[a-z][a-z0-9_-]{0,63}$`)

type validator struct {
	p       *Process
	ceiling Ceiling
	opts    Options
	issues  Issues
}

func (v *validator) errorf(path Path, format string, args ...any) {
	v.issues = append(v.issues, Issue{Severity: Error, Path: path.String(), path: path, Message: fmt.Sprintf(format, args...)})
}

func (v *validator) warnf(path Path, format string, args ...any) {
	v.issues = append(v.issues, Issue{Severity: Warning, Path: path.String(), path: path, Message: fmt.Sprintf(format, args...)})
}

// validate checks an effective process's meaning: what the schema cannot say.
func validate(p *Process, ceiling Ceiling, opts Options) Issues {
	v := &validator{p: p, ceiling: ceiling, opts: opts}
	v.autonomy()
	v.budgets()
	v.phases()
	v.gates()
	v.roles()
	v.holdout()
	for i, r := range p.Rules {
		at := Path{"rules"}.Index(i)
		events := v.events(at.Key("on"), r.On)
		for _, e := range events {
			if e == Timer || e == Stall {
				v.errorf(at.Key("on"), "%s has no period on a rule; wake a role with a trigger (on: %s with every: or after:)", e, e)
			}
		}
		v.condition(at.Key("if"), r.If, events)
		v.action(at.Key("do"), r.Do)
	}
	if p.Integration != nil && p.Integration.AfterEachMerge != nil {
		v.action(Path{"integration", "after_each_merge"}, *p.Integration.AfterEachMerge)
	}
	for i, n := range p.Notifications {
		v.events(Path{"notifications"}.Index(i).Key("on"), n.On)
	}
	if p.Snippets != nil {
		for _, phase := range sortedKeys(p.Snippets.Suggest) {
			if !slices.ContainsFunc(p.Phases, func(ph Phase) bool { return ph.ID == phase }) {
				v.errorf(Path{"snippets", "suggest", phase}, "no phase %q", phase)
			}
		}
	}
	for _, cycle := range p.graph().unboundedCycles() {
		at := pathOf(cycle[0].via)
		v.errorf(at, "trigger cycle with no limit: %s. Add limit: N to one of these rules, a budget to the agent role in it, or a human gate", describeCycle(cycle))
	}
	return v.issues
}

func (v *validator) autonomy() {
	p := v.p
	if p.Autonomy == 0 {
		v.errorf(Path{"autonomy"}, "autonomy is required (3, 4 or 5)")
	}
	if c := v.ceiling.Autonomy; c > 0 && p.Autonomy > c {
		v.errorf(Path{"autonomy"}, "autonomy %d is above your ceiling (%d); raise ceiling.autonomy in your user process file to allow it", p.Autonomy, c)
	}
	isolation := ""
	if p.Requires != nil {
		isolation = p.Requires.Isolation
	}
	if p.Autonomy >= 5 && isolation != "container" {
		v.errorf(Path{"autonomy"}, "autonomy 5 merges without a person, so builders must be isolated: set requires: {isolation: container}")
	}
	if isolation == "container" && v.opts.Isolation != "container" {
		v.errorf(Path{"requires", "isolation"}, "this Runner cannot isolate builders in containers yet; this process cannot run here")
	}
}

func (v *validator) budgets() {
	p := v.p
	mission, task := p.budgetOf("mission"), p.budgetOf("task")
	var ceilingMission, ceilingTask *Budget
	if v.ceiling.Budgets != nil {
		ceilingMission, ceilingTask = v.ceiling.Budgets.Mission, v.ceiling.Budgets.Task
	}
	v.budgetWithin(Path{"budgets", "mission"}, mission, ceilingMission, "your ceiling")
	v.budgetWithin(Path{"budgets", "task"}, task, ceilingTask, "your ceiling")
	if task != nil && mission != nil {
		v.budgetWithin(Path{"budgets", "task"}, &Budget{USD: task.USD, Wall: task.Wall}, mission, "the mission budget")
	}
	for _, name := range p.Roles.Keys {
		r := p.Roles.Values[name]
		if r == nil || r.Budget == nil {
			continue
		}
		at := Path{"roles", name, "budget"}
		if r.Budget.Retries > 0 {
			v.errorf(at.Key("retries"), "retries belong to tasks (budgets.task.retries), not to a role")
		}
		v.budgetWithin(at, &Budget{USD: r.Budget.USD, Wall: r.Budget.Wall}, mission, "the mission budget")
		v.budgetWithin(at, &Budget{Turns: r.Budget.Turns}, ceilingTask, "your ceiling")
	}
}

// budgetWithin reports each limit of b above the same limit of max.
func (v *validator) budgetWithin(at Path, b, max *Budget, what string) {
	if b == nil || max == nil {
		return
	}
	check := func(key string, value, limit float64, show func(float64) string) {
		if limit > 0 && value > limit {
			v.errorf(at.Key(key), "%s is above %s (%s)", show(value), what, show(limit))
		}
	}
	number := func(f float64) string { return fmt.Sprintf("%g", f) }
	duration := func(f float64) string { return Duration(time.Duration(f)).String() }
	check("usd", b.USD, max.USD, number)
	check("wall", float64(b.Wall), float64(max.Wall), duration)
	check("turns", float64(b.Turns), float64(max.Turns), number)
	check("retries", float64(b.Retries), float64(max.Retries), number)
}

func (v *validator) phases() {
	p := v.p
	if len(p.Phases) == 0 {
		v.errorf(Path{"phases"}, "a process needs at least one phase")
		return
	}
	next := false
	targets := map[string]bool{}
	for _, r := range p.Rules {
		next = next || r.Do.EnterNextPhase
		if r.Do.EnterPhase != "" {
			targets[r.Do.EnterPhase] = true
		}
	}
	seen := map[string]bool{}
	for i, ph := range p.Phases {
		at := Path{"phases"}.Index(i)
		if seen[ph.ID] {
			v.errorf(at.Key("id"), "phase %q is listed twice", ph.ID)
		}
		seen[ph.ID] = true
		if !identifier.MatchString(ph.ID) {
			v.errorf(at.Key("id"), "%q is not an id (lower case letters, digits, _ and -)", ph.ID)
		}
		if _, ok := p.Gates.Get(ph.Exit); !ok {
			v.errorf(at.Key("exit"), "no gate %q; every phase exits through a gate defined under gates", ph.Exit)
		}
		if i > 0 && !next && !targets[ph.ID] {
			v.errorf(at, "phase %q is unreachable: no rule enters it (enter_phase: %s) and none enters the next phase (enter_next_phase)", ph.ID, ph.ID)
		}
	}
}

func (v *validator) gates() {
	p := v.p
	used := map[string]bool{}
	for _, ph := range p.Phases {
		used[ph.Exit] = true
	}
	for _, r := range p.Rules {
		used[r.Do.RequestGate] = true
	}
	for _, name := range p.Gates.Keys {
		g := p.Gates.Values[name]
		at := Path{"gates", name}
		if g == nil {
			continue
		}
		if !identifier.MatchString(name) {
			v.errorf(at, "%q is not an id (lower case letters, digits, _ and -)", name)
		}
		switch g.Kind {
		case "":
			v.errorf(at, "kind is required (human, rule, check or agent)")
		case "human":
			if g.When != "" {
				v.errorf(at.Key("when"), "only the owner passes a human gate; a condition cannot")
			}
			if g.Role != "" {
				v.errorf(at.Key("role"), "only the owner passes a human gate; no role can be given it")
			}
		case "rule", "check":
			if g.When == "" {
				v.errorf(at, "a %s gate needs when: the condition that passes it", g.Kind)
			}
			if g.Role != "" {
				v.errorf(at.Key("role"), "only agent gates have a role")
			}
		case "agent":
			if g.Role == "" {
				v.errorf(at, "an agent gate needs role: the role that judges it")
			} else if _, ok := p.Roles.Get(g.Role); !ok {
				v.errorf(at.Key("role"), "no role %q", g.Role)
			}
		}
		if g.When != "" {
			if _, err := CompileGateCondition(g.When); err != nil {
				v.errorf(at.Key("when"), "%v", err)
			}
		}
		if !used[name] {
			v.warnf(at, "gate %q is never used: no phase exits through it and no rule requests it", name)
		}
	}
}

func (v *validator) roles() {
	p := v.p
	if p.Roles.Len() == 0 {
		v.errorf(Path{"roles"}, "a process needs at least one role")
	}
	ceilingPermission, limitPermission := permissionRank[v.ceiling.Permission], v.ceiling.Permission != ""
	for _, name := range p.Roles.Keys {
		r := p.Roles.Values[name]
		at := Path{"roles", name}
		if r == nil {
			continue
		}
		if !identifier.MatchString(name) {
			v.errorf(at, "%q is not a role name (lower case letters, digits, _ and -)", name)
		}
		if len(r.Provider) == 0 {
			v.errorf(at, "provider is required")
		}
		for i, provider := range r.Provider {
			pat := at.Key("provider")
			if len(r.Provider) > 1 {
				pat = pat.Index(i)
			}
			if other, ok := strings.CutPrefix(provider, "other_than_"); ok {
				if other == name {
					v.errorf(pat, "a role cannot be other than itself")
				} else if _, exists := p.Roles.Get(other); !exists {
					v.errorf(pat, "no role %q to be other than", other)
				}
				continue
			}
			if provider != "claude" && provider != "codex" {
				v.errorf(pat, "unknown provider %q; use claude, codex or other_than_<role>", provider)
				continue
			}
			if len(v.ceiling.Providers) > 0 && !slices.Contains(v.ceiling.Providers, provider) {
				v.errorf(pat, "provider %s is not allowed by your ceiling (%s)", provider, strings.Join(v.ceiling.Providers, ", "))
			}
		}
		if limitPermission && permissionRank[r.Permission] > ceilingPermission {
			v.errorf(at.Key("permission"), "%s is above your ceiling (%s)", r.Permission, v.ceiling.Permission)
		}
		for i, s := range r.Scopes {
			v.scope(at.Key("scopes").Index(i), s)
		}
		if slices.Contains(r.Context, "scenarios") && !r.HasScope("holdout.read") {
			v.errorf(at.Key("context"), "scenarios are holdouts: only a role with holdout.read may be handed them")
		}
		if r.Instructions != nil {
			v.instructions(at.Key("instructions"), *r.Instructions)
		}
		for j, t := range r.Triggers {
			tat := at.Key("triggers").Index(j)
			events := v.events(tat.Key("on"), t.On)
			v.condition(tat.Key("if"), t.If, events)
			timer, stall := slices.Contains(t.On, Timer), slices.Contains(t.On, Stall)
			if timer && t.Every == 0 {
				v.errorf(tat, "a timer trigger needs every: (such as 30m)")
			}
			if !timer && t.Every != 0 {
				v.errorf(tat.Key("every"), "every applies only to on: timer")
			}
			if stall && t.After == 0 {
				v.errorf(tat, "a stall trigger needs after: (such as 20m)")
			}
			if !stall && t.After != 0 {
				v.errorf(tat.Key("after"), "after applies only to on: stall")
			}
		}
	}
}

func (v *validator) scope(at Path, written string) {
	base, param, hasParam := strings.Cut(written, ":")
	if strings.HasPrefix(base, "gate.pass") {
		v.errorf(at, "no role may pass a gate: human gates are the owner's alone, and rule, check and agent gates pass on their own condition")
		return
	}
	expanded := expandScope(written)
	if len(expanded) == 0 {
		v.errorf(at, "unknown scope %q; the scopes are %s", written, strings.Join(Scopes(), ", "))
		return
	}
	if !hasParam {
		if base == "session.spawn" {
			v.errorf(at, "session.spawn names the role it may start: session.spawn:<role>")
		}
		return
	}
	if !parameterized[base] {
		v.errorf(at, "%s takes no parameter", base)
		return
	}
	switch base {
	case "session.spawn":
		if _, ok := v.p.Roles.Get(param); !ok {
			v.errorf(at, "no role %q to spawn", param)
		}
	case "task.create":
		if !identifier.MatchString(param) {
			v.errorf(at, "%q is not a task kind", param)
		}
	}
}

func (v *validator) holdout() {
	p := v.p
	var readers []string
	if p.Holdout != nil {
		readers = p.Holdout.Readers
		for i, r := range readers {
			if _, ok := p.Roles.Get(r); !ok {
				v.errorf(Path{"holdout", "readers"}.Index(i), "no role %q", r)
			}
		}
	}
	for _, name := range p.Roles.Keys {
		r := p.Roles.Values[name]
		if !r.HasScope("holdout.read") {
			continue
		}
		at := Path{"roles", name, "scopes"}
		switch {
		case p.Holdout == nil:
			v.errorf(at, "holdout.read without a holdout section")
		case !slices.Contains(readers, name):
			v.errorf(at, "%s holds holdout.read but is not in holdout.readers", name)
		}
		if r.HasScope("task.report") {
			v.errorf(at, "%s builds (task.report) and reads holdouts (holdout.read); a builder must never see the holdout", name)
		}
	}
}

// events checks event names and returns the known ones.
func (v *validator) events(at Path, names StringList) []string {
	var known []string
	for i, name := range names {
		pat := at
		if len(names) > 1 {
			pat = at.Index(i)
		}
		if _, ok := LookupEvent(name); !ok {
			v.errorf(pat, "unknown event type %q; see `lazybox process events`", name)
			continue
		}
		known = append(known, name)
	}
	return known
}

// condition compiles a condition once per event it may run on: each event
// has its own variables.
func (v *validator) condition(at Path, expr string, events []string) {
	if expr == "" {
		return
	}
	for _, e := range events {
		if _, err := CompileCondition(expr, e); err != nil {
			v.errorf(at, "on %s: %v", e, err)
			return
		}
	}
}

func (v *validator) role(at Path, name string) {
	if _, ok := v.p.Roles.Get(name); !ok {
		v.errorf(at, "no role %q", name)
	}
}

func (v *validator) action(at Path, a Action) {
	p := v.p
	verbs := 0
	for _, set := range []bool{a.Start != "", a.Wake != "", a.Send != "", a.RequestGate != "", a.EnterPhase != "", a.EnterNextPhase, a.CreateTask != "", len(a.Notify) > 0, a.Escalate != "", a.Stop != "", a.Exec != "", a.IntegrationQueue != ""} {
		if set {
			verbs++
		}
	}
	if verbs != 1 {
		v.errorf(at, "an action has exactly one verb (start, wake, send, request_gate, enter_phase, enter_next_phase, create_task, notify, escalate, stop, exec, integration.queue); found %d", verbs)
		return
	}
	if (a.Snippet != "" || a.Text != "") && a.Send == "" {
		v.errorf(at, "snippet and text go with send")
	}
	if a.Assign != "" && a.CreateTask == "" {
		v.errorf(at.Key("assign"), "assign goes with create_task")
	}
	switch {
	case a.Start != "":
		v.role(at.Key("start"), a.Start)
	case a.Wake != "":
		v.role(at.Key("wake"), a.Wake)
	case a.Stop != "":
		v.role(at.Key("stop"), a.Stop)
	case a.Send != "":
		v.role(at.Key("send"), a.Send)
		if (a.Snippet == "") == (a.Text == "") {
			v.errorf(at, "send needs exactly one of snippet or text")
		}
	case a.RequestGate != "":
		if _, ok := p.Gates.Get(a.RequestGate); !ok {
			v.errorf(at.Key("request_gate"), "no gate %q", a.RequestGate)
		}
	case a.EnterPhase != "":
		if !slices.ContainsFunc(p.Phases, func(ph Phase) bool { return ph.ID == a.EnterPhase }) {
			v.errorf(at.Key("enter_phase"), "no phase %q", a.EnterPhase)
		}
	case a.CreateTask != "":
		if !identifier.MatchString(a.CreateTask) {
			v.errorf(at.Key("create_task"), "%q is not a task kind", a.CreateTask)
		}
		if a.Assign != "" {
			v.role(at.Key("assign"), a.Assign)
		}
	case a.Exec != "":
		if !repoRelative(a.Exec) {
			v.errorf(at.Key("exec"), "%q must be a path inside the repository (relative, no ..)", a.Exec)
		}
	}
}

// repoRelative reports whether name stays inside the repository.
func repoRelative(name string) bool {
	return name != "" && !path.IsAbs(name) && !strings.Contains(name, `\`) && fs.ValidPath(path.Clean(name)) && path.Clean(name) != "."
}

// AgentFileCandidates are the files an instructions.agent name resolves to,
// in order: existing agent definitions are read as role instructions.
func AgentFileCandidates(name string) []string {
	return []string{
		".claude/agents/" + name + ".md",
		".github/agents/" + name + ".agent.md",
		".github/agents/" + name + ".md",
		".factory/droids/" + name + ".md",
	}
}

func (v *validator) instructions(at Path, in Instructions) {
	set := 0
	for _, s := range []string{in.File, in.Snippet, in.Text, in.Agent} {
		if s != "" {
			set++
		}
	}
	if set != 1 {
		v.errorf(at, "instructions name exactly one of file, snippet, text or agent; found %d", set)
		return
	}
	switch {
	case in.File != "":
		if !repoRelative(in.File) {
			v.errorf(at.Key("file"), "%q must be a path inside the repository (relative, no ..)", in.File)
			return
		}
		if v.opts.Files != nil {
			if _, err := fs.Stat(v.opts.Files, path.Clean(in.File)); err != nil {
				v.warnf(at.Key("file"), "%s does not exist in this repository; the role starts without these instructions", in.File)
			}
		}
	case in.Agent != "":
		if !identifier.MatchString(in.Agent) {
			v.errorf(at.Key("agent"), "%q is not an agent name", in.Agent)
			return
		}
		if v.opts.Files != nil && !slices.ContainsFunc(AgentFileCandidates(in.Agent), func(f string) bool {
			_, err := fs.Stat(v.opts.Files, f)
			return err == nil
		}) {
			v.warnf(at.Key("agent"), "no agent file for %q (looked for %s)", in.Agent, strings.Join(AgentFileCandidates(in.Agent), ", "))
		}
	}
}

func sortedKeys[T any](m map[string]T) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	slices.Sort(out)
	return out
}

// pathOf parses a Path's string form back (rules[2], roles.x.triggers[0]).
func pathOf(s string) Path {
	var out Path
	for _, part := range strings.Split(s, ".") {
		name, rest, _ := strings.Cut(part, "[")
		if name != "" {
			out = append(out, name)
		}
		for rest != "" {
			index, after, _ := strings.Cut(rest, "]")
			var n int
			fmt.Sscanf(index, "%d", &n)
			out = append(out, n)
			rest = strings.TrimPrefix(after, "[")
		}
	}
	return out
}
