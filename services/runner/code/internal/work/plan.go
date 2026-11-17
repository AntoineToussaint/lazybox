package work

import (
	"fmt"
	"slices"
	"strings"
	"time"
)

// Plan approval and budgets (audits/2026-11-16/sota-agents.md §6.9, §6.11):
// an orchestrator proposes how it would split a mission into agents, with an
// estimate; the person approves it once, with a $ cap, before any of them
// starts (Warp's pattern). The cap is the grant's budget: spawns reserve
// their estimate against it, and when what the tree spent plus what its live
// agents reserved would cross it, new spawns stop and the person is asked.

const (
	PlanProposed    = "plan.proposed"
	PlanApproved    = "plan.approved"
	PlanRejected    = "plan.rejected"
	BudgetRaised    = "budget.raised"
	BudgetWarning   = "budget.warning"
	BudgetExhausted = "budget.exhausted"
)

// Plan states, derived from the log.
const (
	PlanStateProposed   = "proposed"
	PlanStateApproved   = "approved"
	PlanStateRejected   = "rejected"
	PlanStateSuperseded = "superseded"
)

// Bounds of a plan.
const (
	MaxPlanTasks = 16
	maxPaths     = 32
	maxPath      = 256
)

type (
	// PlanTask is one agent a plan would start.
	PlanTask struct {
		// Key names the task inside the plan (after: refers to keys).
		Key         string   `json:"key"`
		Title       string   `json:"title"`
		Brief       string   `json:"brief,omitempty"`
		Role        string   `json:"role,omitempty"`
		Profile     string   `json:"profile,omitempty"`
		Permission  string   `json:"permission,omitempty"`
		Workspace   string   `json:"workspace,omitempty"`
		Paths       []string `json:"paths,omitempty"`
		EstimateUSD float64  `json:"estimate_usd,omitempty"`
		After       []string `json:"after,omitempty"`
	}
	// PlanPayload is plan.proposed.
	PlanPayload struct {
		Plan    string     `json:"plan"`
		Grant   string     `json:"grant"`
		Summary string     `json:"summary,omitempty"`
		Tasks   []PlanTask `json:"tasks"`
	}
	// PlanDecisionPayload is plan.approved or plan.rejected. On approval,
	// Tasks maps each plan key to the mission task it became, and the cap
	// and fan-out become the grant's.
	PlanDecisionPayload struct {
		Plan        string            `json:"plan"`
		CapUSD      float64           `json:"cap_usd,omitempty"`
		MaxChildren int               `json:"max_children,omitempty"`
		Note        string            `json:"note,omitempty"`
		Tasks       map[string]string `json:"tasks,omitempty"`
	}
	// BudgetPayload is budget.raised (a new cap), budget.warning or
	// budget.exhausted (what was spent and reserved against the cap).
	BudgetPayload struct {
		Grant       string  `json:"grant"`
		CapUSD      float64 `json:"cap_usd"`
		SpentUSD    float64 `json:"spent_usd,omitempty"`
		ReservedUSD float64 `json:"reserved_usd,omitempty"`
		Note        string  `json:"note,omitempty"`
	}
)

// Plan is a plan as the log says it.
type Plan struct {
	PlanPayload
	State       string
	ProposedBy  Actor
	Proposed    time.Time
	ProposedSeq uint64
	Decided     time.Time
	CapUSD      float64
	Note        string
	// Created maps plan keys to the tasks the approval created.
	Created map[string]string
}

// TotalUSD is the plan's estimate.
func (p *Plan) TotalUSD() float64 {
	total := 0.0
	for _, t := range p.Tasks {
		total += t.EstimateUSD
	}
	return total
}

// Overlap is a path two of a plan's tasks both name.
type Overlap struct {
	Path string
	Keys []string
}

// Overlaps are the paths more than one task of the plan would touch: where
// parallel writers collide. A path is a glob prefix; "api/" and "api/x.go"
// overlap.
func (p *Plan) Overlaps() []Overlap {
	var out []Overlap
	for i, a := range p.Tasks {
		for _, b := range p.Tasks[i+1:] {
			for _, pa := range a.Paths {
				for _, pb := range b.Paths {
					if path, ok := pathsMeet(pa, pb); ok {
						out = appendOverlap(out, path, a.Key, b.Key)
					}
				}
			}
		}
	}
	return out
}

func appendOverlap(list []Overlap, path, a, b string) []Overlap {
	for i := range list {
		if list[i].Path == path {
			for _, k := range []string{a, b} {
				if !slices.Contains(list[i].Keys, k) {
					list[i].Keys = append(list[i].Keys, k)
				}
			}
			return list
		}
	}
	return append(list, Overlap{Path: path, Keys: []string{a, b}})
}

// pathsMeet reports whether two path globs can name the same file, and the
// narrower of them. Only a trailing * or / is understood; that is enough to
// tell "api/" from "web/".
func pathsMeet(a, b string) (string, bool) {
	ta, tb := strings.TrimSuffix(strings.TrimSuffix(a, "*"), "**"), strings.TrimSuffix(strings.TrimSuffix(b, "*"), "**")
	switch {
	case ta == tb:
		return a, true
	case strings.HasSuffix(ta, "/") && strings.HasPrefix(tb, ta):
		return b, true
	case strings.HasSuffix(tb, "/") && strings.HasPrefix(ta, tb):
		return a, true
	}
	return "", false
}

// PlanOf returns a plan of the mission, or nil.
func (s *State) PlanOf(id string) *Plan {
	for _, p := range s.Plans {
		if p.Plan == id {
			return p
		}
	}
	return nil
}

// PendingPlan is the plan of a grant waiting for the person, or nil.
func (s *State) PendingPlan(grant string) *Plan {
	for i := len(s.Plans) - 1; i >= 0; i-- {
		if p := s.Plans[i]; p.Grant == grant && p.State == PlanStateProposed {
			return p
		}
	}
	return nil
}

// ApprovedTask reports whether a task came from an approved plan of a grant,
// and that plan's entry for it.
func (s *State) ApprovedTask(grant, task string) (PlanTask, bool) {
	for _, p := range s.Plans {
		if p.Grant != grant || p.State != PlanStateApproved {
			continue
		}
		for key, id := range p.Created {
			if id != task {
				continue
			}
			for _, t := range p.Tasks {
				if t.Key == key {
					return t, true
				}
			}
		}
	}
	return PlanTask{}, false
}

// BudgetState is the latest budget event of a grant: "" (nothing to say),
// warning or exhausted; a raise clears it.
func (s *State) BudgetState(grant string) string {
	state := ""
	for _, e := range s.Events {
		if e.Type == PlanApproved {
			if d, ok := decode[PlanDecisionPayload](e); ok {
				if p := s.PlanOf(d.Plan); p != nil && p.Grant == grant {
					state = ""
				}
			}
			continue
		}
		if e.Subject != GrantSubject(grant) {
			continue
		}
		switch e.Type {
		case BudgetWarning:
			state = "warning"
		case BudgetExhausted:
			state = "exhausted"
		case BudgetRaised:
			state = ""
		}
	}
	return state
}

func (t PlanTask) validate(g *Grant) error {
	if t.Key == "" || len(t.Key) > 64 || strings.ContainsAny(t.Key, " \t\n") {
		return refuse("each planned task needs a short key without spaces")
	}
	if strings.TrimSpace(t.Title) == "" || len(t.Title) > maxTitle {
		return refuse("planned task %q needs a title of at most %d bytes", t.Key, maxTitle)
	}
	if len(t.Brief) > maxBrief {
		return refuse("the brief of planned task %q is longer than %d bytes", t.Key, maxBrief)
	}
	if len(t.Role) > 32 {
		return refuse("the role of planned task %q is a short label", t.Key)
	}
	if t.Permission != "" {
		rank, ok := permissionRank[t.Permission]
		if !ok || rank > permissionRank[g.Ceiling.Permission] {
			return refuse("planned task %q asks for %s; the grant allows %s at most", t.Key, t.Permission, g.Ceiling.Permission)
		}
	}
	if t.Profile != "" && !slices.Contains(g.Ceiling.Profiles, t.Profile) {
		return refuse("planned task %q names subscription %q, which the grant does not allow", t.Key, t.Profile)
	}
	if t.Workspace != "" && t.Workspace != "same" && t.Workspace != "new" {
		return refuse("planned task %q: workspace is same or new", t.Key)
	}
	if t.EstimateUSD < 0 || t.EstimateUSD > 10000 {
		return refuse("planned task %q: an estimate between $0 and $10,000", t.Key)
	}
	if len(t.Paths) > maxPaths {
		return refuse("planned task %q names more than %d paths", t.Key, maxPaths)
	}
	for _, p := range t.Paths {
		if p == "" || len(p) > maxPath || strings.HasPrefix(p, "/") || strings.Contains(p, "..") {
			return refuse("planned task %q: paths are relative to the workspace, like api/handler.go or web/", t.Key)
		}
	}
	return nil
}

// foldPlan folds the plan and budget families.
func (s *State) foldPlan(e Event) error {
	switch e.Type {
	case PlanProposed:
		p, ok := decode[PlanPayload](e)
		if !ok || p.Plan == "" || e.Subject != "plan:"+p.Plan {
			return refuse("a plan needs an id")
		}
		if s.PlanOf(p.Plan) != nil {
			return refuse("plan %q exists", p.Plan)
		}
		g := s.Grant(p.Grant)
		if g == nil || !s.Live(g) || e.Actor.Kind != ActorAgent || e.Actor.Conversation != g.Holder {
			return refuse("only an orchestrator with a grant in force proposes a plan")
		}
		if len(p.Tasks) == 0 || len(p.Tasks) > MaxPlanTasks {
			return refuse("a plan has 1 to %d tasks", MaxPlanTasks)
		}
		if len(p.Summary) > maxSummary {
			return refuse("a plan's summary is at most %d bytes", maxSummary)
		}
		keys := map[string]bool{}
		for _, t := range p.Tasks {
			if err := t.validate(g); err != nil {
				return err
			}
			if keys[t.Key] {
				return refuse("two planned tasks are both %q", t.Key)
			}
			keys[t.Key] = true
		}
		for _, t := range p.Tasks {
			for _, a := range t.After {
				if !keys[a] || a == t.Key {
					return refuse("planned task %q starts after %q, which is not another task of the plan", t.Key, a)
				}
			}
		}
		if planCycle(p.Tasks) {
			return refuse("the plan's order has a cycle")
		}
		// A new proposal replaces one still waiting.
		if old := s.PendingPlan(p.Grant); old != nil {
			old.State = PlanStateSuperseded
		}
		s.Plans = append(s.Plans, &Plan{PlanPayload: p, State: PlanStateProposed, ProposedBy: e.Actor, Proposed: e.At, ProposedSeq: e.Seq})
	case PlanApproved, PlanRejected:
		if e.Actor.Kind != ActorHuman {
			return refuse("only a person approves or rejects a plan")
		}
		d, _ := decode[PlanDecisionPayload](e)
		p := s.PlanOf(d.Plan)
		if p == nil || p.State != PlanStateProposed {
			return refuse("plan %q is not waiting for a decision", d.Plan)
		}
		if len(d.Note) > maxNote {
			return refuse("note is longer than %d bytes", maxNote)
		}
		p.Decided, p.Note = e.At, d.Note
		if e.Type == PlanRejected {
			p.State = PlanStateRejected
			return nil
		}
		g := s.Grant(p.Grant)
		if g == nil || !s.Live(g) {
			return refuse("the plan's grant is no longer in force")
		}
		if d.CapUSD < 0 || d.CapUSD > 10000 {
			return refuse("a cap between $0 (none) and $10,000")
		}
		if d.MaxChildren < 0 || d.MaxChildren > MaxChildrenLimit {
			return refuse("between 1 and %d agents at once", MaxChildrenLimit)
		}
		for _, t := range p.Tasks {
			if d.Tasks[t.Key] == "" {
				return refuse("the approval names no task for %q", t.Key)
			}
		}
		p.State, p.CapUSD, p.Created = PlanStateApproved, d.CapUSD, d.Tasks
		// The person's cap and fan-out are the grant's from now on.
		g.Ceiling.BudgetUSD = d.CapUSD
		if d.MaxChildren > 0 {
			g.Ceiling.MaxChildren = d.MaxChildren
		}
	case BudgetRaised:
		if e.Actor.Kind != ActorHuman {
			return refuse("only a person changes a budget")
		}
		b, _ := decode[BudgetPayload](e)
		g := s.Grant(b.Grant)
		if g == nil || !s.Live(g) || e.Subject != GrantSubject(b.Grant) {
			return refuse("the grant is not in force")
		}
		if b.CapUSD < 0 || b.CapUSD > 10000 {
			return refuse("a cap between $0 (none) and $10,000")
		}
		g.Ceiling.BudgetUSD = b.CapUSD
	case BudgetWarning, BudgetExhausted:
		if e.Actor.Kind != ActorLazybox {
			return refuse("only Lazybox records a budget it measured")
		}
		b, _ := decode[BudgetPayload](e)
		if s.Grant(b.Grant) == nil || e.Subject != GrantSubject(b.Grant) {
			return refuse("grant %q does not exist", b.Grant)
		}
	}
	return nil
}

func planCycle(tasks []PlanTask) bool {
	after := map[string][]string{}
	for _, t := range tasks {
		after[t.Key] = t.After
	}
	state := map[string]int{}
	var visit func(string) bool
	visit = func(k string) bool {
		switch state[k] {
		case 1:
			return true
		case 2:
			return false
		}
		state[k] = 1
		for _, n := range after[k] {
			if visit(n) {
				return true
			}
		}
		state[k] = 2
		return false
	}
	for _, t := range tasks {
		if visit(t.Key) {
			return true
		}
	}
	return false
}

// ---- Operations ---------------------------------------------------------------

// ProposePlan records an orchestrator's plan for the person to approve.
func (s *Store) ProposePlan(mission string, actor Actor, key string, p PlanPayload) (*State, string, error) {
	p.Plan = shortID("pl")
	p.Summary = strings.TrimSpace(p.Summary)
	for i := range p.Tasks {
		p.Tasks[i].Title = strings.TrimSpace(p.Tasks[i].Title)
		p.Tasks[i].Brief = strings.TrimSpace(p.Tasks[i].Brief)
	}
	state, events, err := s.Append(mission, Draft{Subject: "plan:" + p.Plan, Type: PlanProposed, Actor: actor, Idempotency: key, Payload: p})
	if err == nil && len(events) == 0 {
		p.Plan = existingSubject(state, key, "plan:")
	}
	return state, p.Plan, err
}

// DecidePlan approves (creating its tasks, in order) or rejects a plan. Only
// a person decides.
func (s *Store) DecidePlan(mission, plan string, actor Actor, key string, approve bool, capUSD float64, maxChildren int, note string) (*State, error) {
	note, err := cleanText(note, maxNote, "note")
	if err != nil {
		return nil, err
	}
	current, err := s.Mission(mission)
	if err != nil {
		return nil, err
	}
	if key != "" {
		if _, ok := current.Event(key); ok {
			return current, nil
		}
	}
	p := current.PlanOf(plan)
	if p == nil {
		return nil, refuse("plan %q does not exist", plan)
	}
	if !approve {
		state, _, err := s.Append(mission, Draft{Subject: "plan:" + plan, Type: PlanRejected, Actor: actor, Idempotency: key, Payload: PlanDecisionPayload{Plan: plan, Note: note}})
		return state, err
	}
	ids := map[string]string{}
	var drafts []Draft
	for _, t := range p.Tasks {
		ids[t.Key] = shortID("t")
	}
	drafts = append(drafts, Draft{Subject: "plan:" + plan, Type: PlanApproved, Actor: actor, Idempotency: key, Payload: PlanDecisionPayload{Plan: plan, CapUSD: capUSD, MaxChildren: maxChildren, Note: note, Tasks: ids}})
	// Tasks in an order where each comes after what it waits for.
	done := map[string]bool{}
	for len(done) < len(p.Tasks) {
		progressed := false
		for _, t := range p.Tasks {
			if done[t.Key] {
				continue
			}
			ready := true
			for _, a := range t.After {
				ready = ready && done[a]
			}
			if !ready {
				continue
			}
			var blocked []string
			for _, a := range t.After {
				blocked = append(blocked, ids[a])
			}
			drafts = append(drafts, Draft{Subject: TaskSubject(ids[t.Key]), Type: TaskCreated, Actor: Actor{Kind: ActorRule, Role: "plan"}, Payload: TaskPayload{Task: ids[t.Key], Title: t.Title, Brief: t.Brief, BlockedBy: blocked}})
			done[t.Key], progressed = true, true
		}
		if !progressed {
			return nil, fmt.Errorf("%w: the plan's order has a cycle", ErrRefused)
		}
	}
	state, _, err := s.Append(mission, drafts...)
	return state, err
}

// RaiseBudget sets a grant's cap (the person, after a budget stop).
func (s *Store) RaiseBudget(mission, grant string, actor Actor, key string, capUSD float64) (*State, error) {
	state, _, err := s.Append(mission, Draft{Subject: GrantSubject(grant), Type: BudgetRaised, Actor: actor, Idempotency: key, Payload: BudgetPayload{Grant: grant, CapUSD: capUSD}})
	return state, err
}

// RecordBudget records that a grant's tree reached its warning or its cap,
// once per crossing.
func (s *Store) RecordBudget(mission, grant, kind string, b BudgetPayload) (*State, bool, error) {
	current, err := s.Mission(mission)
	if err != nil {
		return nil, false, err
	}
	want := map[string]string{BudgetWarning: "warning", BudgetExhausted: "exhausted"}[kind]
	if want == "" {
		return nil, false, refuse("budget.warning or budget.exhausted")
	}
	if current.BudgetState(grant) == want {
		return current, false, nil
	}
	b.Grant = grant
	state, _, err := s.Append(mission, Draft{Subject: GrantSubject(grant), Type: kind, Actor: Actor{Kind: ActorLazybox}, Payload: b})
	return state, err == nil, err
}
