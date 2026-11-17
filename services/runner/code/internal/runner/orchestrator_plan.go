package runner

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Plans and budgets of an Orchestrator (work/plan.go; sota-agents.md §6.9).
// The plan is the orchestrator's proposal, the person approves it once with
// a cap; spawns reserve their estimate against the cap, and a spawn that
// would cross it is refused and the person is asked.

// ErrPlanRequired: the grant starts agents only from an approved plan.
var ErrPlanRequired = errors.New("the person asked to approve a plan before any agent starts: propose one with `lazybox mission plan` and wait for their approval, then start its tasks with `lazybox agents spawn --task ID`")

// planBriefing is the plan line of the orchestrator's briefing.
func planBriefing(required bool) string {
	how := "Propose your split as a plan: `lazybox mission plan -` reads YAML (summary; tasks: key, title, brief, role, permission, workspace same|new, paths, estimate_usd, after: [keys]). The person sees it on one sheet, with the paths two tasks share and the total, and approves it with a $ cap; then `lazybox agents ls` lists its tasks' ids and `lazybox agents spawn --task ID` starts each, in order."
	if required {
		return "The person asked to approve a plan before any agent starts. " + how
	}
	return how + " Agents you start without a plan carry no estimate."
}

// ProposePlan records the calling orchestrator's plan for the person.
func (b *Bench) ProposePlan(ctx context.Context, session, operation, summary string, tasks []work.PlanTask) (*work.State, *work.Plan, error) {
	c, err := b.orchestrationCaller(session)
	if err != nil {
		return nil, nil, err
	}
	if !c.holds {
		return nil, nil, ErrNoGrant
	}
	actor := work.Actor{Kind: work.ActorAgent, Role: "orchestrator", Conversation: c.conversation, Execution: c.e.ID}
	state, id, err := b.Work.ProposePlan(c.held.Mission.ID, actor, operation, work.PlanPayload{Grant: c.held.Grant.Grant, Summary: summary, Tasks: tasks})
	if err != nil {
		return nil, nil, err
	}
	p := state.PlanOf(id)
	b.recordLineageEvent(c.e, "plan_proposed", fmt.Sprintf("Proposed a plan: %s · est. %s · waiting for your approval", plural(len(p.Tasks), "agent", "agents"), dollars(p.TotalUSD())), map[string]any{"mission": state.ID, "plan": id})
	b.orch().broadcast()
	return state, p, nil
}

// DecidePlan is the person's decision on a plan; the orchestrator is told
// with its next turn.
func (b *Bench) DecidePlan(ctx context.Context, mission, plan, operation string, approve bool, capUSD float64, maxChildren int, note string) (*work.State, error) {
	state, err := b.Work.DecidePlan(mission, plan, work.Human, operation, approve, capUSD, maxChildren, note)
	if err != nil {
		return nil, err
	}
	p := state.PlanOf(plan)
	if g := state.Grant(p.Grant); g != nil {
		if e, err := b.Get(firstOf(g.Session, g.Holder)); err == nil {
			text := "You rejected the plan" + suffixed(note) + "."
			if approve {
				text = fmt.Sprintf("You approved the plan: %s, %s.", plural(len(p.Tasks), "task", "tasks"), capLabel(capUSD))
			}
			b.recordLineageEvent(e, "plan_decided", text, map[string]any{"mission": state.ID, "plan": plan, "approved": approve})
			message := "[Lazybox] The person rejected your plan" + suffixed(note) + ". Propose another with `lazybox mission plan`, or ask them what they want."
			if approve {
				message = fmt.Sprintf("[Lazybox] The person approved your plan (%s). Its tasks are on the mission board; start each with `lazybox agents spawn --task ID` (`lazybox agents ls` lists their ids), in order.", capLabel(capUSD)) + suffixed(note)
			}
			ctx, cancel := context.WithTimeout(b.ctx, 10*time.Second)
			defer cancel()
			// The orchestrator acts on the decision in its next turn.
			_, _ = b.deliverAgentMessage(ctx, e, "plan-"+plan+map[bool]string{true: "-approved", false: "-rejected"}[approve], message, lazyboxOrigin("plan_decided", map[bool]string{true: "Plan approved", false: "Plan rejected"}[approve], capLabel(capUSD)), work.PromptSource{Kind: work.SourceMission, Mission: state.ID})
		}
	}
	b.orch().broadcast()
	return state, nil
}

// RaiseBudget is the person's new cap after a budget stop.
func (b *Bench) RaiseBudget(ctx context.Context, mission, grant, operation string, capUSD float64) (*work.State, error) {
	state, err := b.Work.RaiseBudget(mission, grant, work.Human, operation, capUSD)
	if err != nil {
		return nil, err
	}
	if g := state.Grant(grant); g != nil {
		if e, err := b.Get(firstOf(g.Session, g.Holder)); err == nil {
			b.recordLineageEvent(e, "budget_raised", "You set the tree's budget to "+capLabel(capUSD)+"; new agents may start again.", map[string]any{"mission": state.ID, "grant": grant})
		}
	}
	b.orch().broadcast()
	return state, nil
}

// TreeBudget is what a grant's tree spent (the orchestrator and every agent
// it started) and what its live agents reserved: the part of each one's plan
// estimate it has not spent yet.
type TreeBudget struct {
	Spent, Reserved float64
	// Costs is each conversation's cost.
	Costs map[string]float64
}

// TreeBudgetOf is a grant's tree budget, for the board.
func (b *Bench) TreeBudgetOf(ctx context.Context, state *work.State, g *work.Grant) TreeBudget {
	return b.treeBudget(ctx, state, g)
}

func (b *Bench) treeBudget(ctx context.Context, state *work.State, g *work.Grant) TreeBudget {
	out := TreeBudget{Costs: map[string]float64{}}
	conversations := []string{g.Holder}
	for _, ch := range state.Children(g.Holder) {
		conversations = append(conversations, ch.Attempt.Conversation)
	}
	for _, c := range conversations {
		if _, seen := out.Costs[c]; seen {
			continue
		}
		cost := b.ConversationCost(ctx, c)
		out.Costs[c], out.Spent = cost, out.Spent+cost
	}
	for _, t := range state.Tasks {
		a := t.Current()
		if a == nil || a.Ended || a.Grant != g.Grant || state.Done(t) || t.Canceled {
			continue
		}
		if entry, ok := state.ApprovedTask(g.Grant, t.ID); ok {
			out.Reserved += max(0, entry.EstimateUSD-out.Costs[a.Conversation])
		}
	}
	return out
}

// costCache keeps conversation costs for a few seconds: boards poll often.
var costCache sync.Map

type cachedCost struct {
	at   time.Time
	usd  float64
	none bool
}

// ConversationCost is what a conversation's usage cost so far, in dollars
// (API-equivalent prices; unpriced usage counts as nothing).
func (b *Bench) ConversationCost(ctx context.Context, conversation string) float64 {
	if b.Telemetry == nil || conversation == "" {
		return 0
	}
	if v, ok := costCache.Load(conversation); ok && time.Since(v.(cachedCost).at) < 5*time.Second {
		return v.(cachedCost).usd
	}
	var prices telemetry.Pricer
	if b.Preferences != nil {
		prices = b.Preferences.Rates()
	}
	report, err := b.Telemetry.Report(ctx, telemetry.Query{Conversation: conversation, Prices: prices})
	if err != nil {
		return 0
	}
	var nanos int64
	for _, g := range report.Groups {
		if g.Nanos != nil {
			nanos += *g.Nanos
		}
	}
	usd := float64(nanos) / 1e9
	costCache.Store(conversation, cachedCost{at: time.Now(), usd: usd})
	return usd
}

// checkBudget refuses a spawn whose estimate would take the tree past its
// cap, and records the stop (once) so the person is asked; it records a
// warning at 80%.
func (b *Bench) checkBudget(ctx context.Context, state *work.State, g *work.Grant, estimate float64, orchestrator *Execution) error {
	limit := g.Ceiling.BudgetUSD
	if limit <= 0 {
		return nil
	}
	tree := b.treeBudget(ctx, state, g)
	committed := tree.Spent + tree.Reserved + estimate
	payload := work.BudgetPayload{CapUSD: limit, SpentUSD: round2(tree.Spent), ReservedUSD: round2(tree.Reserved)}
	if committed > limit {
		if _, wrote, _ := b.Work.RecordBudget(state.ID, g.Grant, work.BudgetExhausted, payload); wrote {
			b.recordLineageEvent(orchestrator, "budget_exhausted", fmt.Sprintf("New agents paused: %s spent and %s reserved of the %s budget. The person can raise it on the mission board.", dollars(tree.Spent), dollars(tree.Reserved), dollars(limit)), map[string]any{"mission": state.ID, "grant": g.Grant})
		}
		return fmt.Errorf("%w: the tree's budget is %s, and %s is spent, %s reserved by running agents, and this one is estimated at %s. New agents are paused until the person raises the budget; finish with the agents you have, or tell the person", work.ErrRefused, dollars(limit), dollars(tree.Spent), dollars(tree.Reserved), dollars(estimate))
	}
	if committed > 0.8*limit {
		_, _, _ = b.Work.RecordBudget(state.ID, g.Grant, work.BudgetWarning, payload)
	}
	return nil
}

// planned is what an approved plan says about a task the orchestrator starts.
func (b *Bench) planned(state *work.State, g *work.Grant, r *SpawnRequest) (float64, error) {
	if r.Task == "" {
		if g.Ceiling.PlanRequired {
			return 0, fmt.Errorf("%w: %v", work.ErrRefused, ErrPlanRequired)
		}
		return 0, nil
	}
	entry, ok := state.ApprovedTask(g.Grant, r.Task)
	if !ok {
		if g.Ceiling.PlanRequired {
			return 0, fmt.Errorf("%w: task %q is not in a plan the person approved; %v", work.ErrRefused, r.Task, ErrPlanRequired)
		}
		return 0, nil
	}
	if t := state.Task(r.Task); t != nil {
		for _, before := range t.BlockedBy {
			if pre := state.Task(before); pre != nil && !state.Done(pre) {
				return 0, fmt.Errorf("%w: “%s” starts after “%s”, which the person has not accepted yet", work.ErrRefused, t.Title, pre.Title)
			}
		}
	}
	// The plan's choices are the defaults; what is asked still has to fit the ceiling.
	r.Role, r.Permission, r.Profile, r.Workspace = firstOf(r.Role, entry.Role), firstOf(r.Permission, entry.Permission), firstOf(r.Profile, entry.Profile), firstOf(r.Workspace, entry.Workspace)
	return entry.EstimateUSD, nil
}

func dollars(v float64) string {
	if v >= 100 {
		return fmt.Sprintf("$%.0f", v)
	}
	return fmt.Sprintf("$%.2f", v)
}

func capLabel(v float64) string {
	if v <= 0 {
		return "no budget cap"
	}
	return dollars(v) + " cap"
}

func round2(v float64) float64 { return float64(int64(v*100+0.5)) / 100 }
