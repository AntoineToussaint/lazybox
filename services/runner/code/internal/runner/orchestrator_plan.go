package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
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
	how := "Propose your split as a plan: `lazybox mission plan -` reads YAML (summary; tasks: key, title, brief, role, permission, workspace same|new|pinned, paths, estimate_usd, after: [keys]). The person sees it on one sheet, with the paths two tasks share and the total, and approves it with an optional budget cap; then `lazybox agents ls` lists its tasks' ids and `lazybox agents spawn --task ID` starts each, in order."
	if required {
		return "The person asked to approve a plan before any agent starts. " + how
	}
	return how + " Agents you start without a plan carry no estimate."
}

// ProposePlan records the calling orchestrator's plan for the person.
func (b *Bench) ProposePlan(ctx context.Context, session, operation, summary string, tasks []work.PlanTask, artifactID string, version uint32) (*work.State, *work.Plan, error) {
	c, err := b.orchestrationCaller(session)
	if err != nil {
		return nil, nil, err
	}
	if !c.holds {
		return nil, nil, ErrNoGrant
	}
	actor := work.Actor{Kind: work.ActorAgent, Role: "orchestrator", Conversation: c.conversation, Execution: c.e.ID}
	payload := work.PlanPayload{Grant: c.held.Grant.Grant, Summary: summary, Tasks: tasks}
	if event, ok := c.held.Mission.Event(operation); operation != "" && ok && event.Type == work.PlanProposed {
		if plan := c.held.Mission.PlanOf(strings.TrimPrefix(event.Subject, "plan:")); plan != nil && plan.Grant == payload.Grant {
			return c.held.Mission, plan, nil
		}
	}
	if artifactID != "" || version != 0 {
		if artifactID == "" || version == 0 || b.Artifacts == nil {
			return nil, nil, fmt.Errorf("%w: give a spec artifact ID and a positive version", work.ErrRefused)
		}
		doc, err := b.Artifacts.Get(c.e.CollectionID, artifactID, version)
		if err != nil {
			return nil, nil, err
		}
		var header struct{ Schema string }
		if doc.Kind != "spec" || doc.Format != "json" || json.Unmarshal([]byte(doc.Content), &header) != nil {
			return nil, nil, fmt.Errorf("%w: the contract needs a structured spec", work.ErrRefused)
		}
		contract := &work.PlanContract{Artifact: artifactID, Version: version, Workspace: c.e.CollectionID, Digest: artifacts.Digest(doc.Content)}
		switch header.Schema {
		case "spec.v2":
			contract.Bindings, err = delivery.ParseSpecV2(doc.Content)
			if err != nil {
				return nil, nil, fmt.Errorf("%w: %v", work.ErrRefused, err)
			}
			for _, criterion := range contract.Bindings {
				level := "optional"
				if criterion.Required {
					level = "required"
				}
				contract.Criteria = append(contract.Criteria, fmt.Sprintf("[%s, %s] %s", criterion.ID, level, criterion.Expected))
			}
		case "spec.v1":
			var legacy struct{ AcceptanceCriteria []string }
			if json.Unmarshal([]byte(doc.Content), &legacy) != nil || len(legacy.AcceptanceCriteria) == 0 {
				return nil, nil, fmt.Errorf("%w: missing acceptance criteria", work.ErrRefused)
			}
			contract.Criteria = legacy.AcceptanceCriteria
		default:
			return nil, nil, fmt.Errorf("%w: unsupported spec schema", work.ErrRefused)
		}
		if len(strings.Join(contract.Criteria, "\n")) > 4096 {
			return nil, nil, fmt.Errorf("%w: mission acceptance criteria exceed 4 KiB", work.ErrRefused)
		}
		payload.Contract = contract
	}
	for _, t := range tasks {
		if t.Workspace == "pinned" {
			payload.Baseline, err = b.planBaseline(ctx, c.e.CollectionID)
			if err != nil {
				return nil, nil, err
			}
			break
		}
	}
	state, id, err := b.Work.ProposePlan(c.held.Mission.ID, actor, operation, payload)
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
	out, _ := b.treeBudget(ctx, state, g, false)
	return out
}

func (b *Bench) treeBudget(ctx context.Context, state *work.State, g *work.Grant, fresh bool) (TreeBudget, error) {
	out := TreeBudget{Costs: map[string]float64{}}
	end := state.CanceledAt
	for ancestor := g; ancestor != nil; ancestor = state.Grant(ancestor.Parent) {
		end = earliestBound(end, ancestor.RevokedAt)
	}
	var err error
	out.Spent, err = b.conversationCost(ctx, g.Holder, g.Issued, end, fresh)
	if err != nil {
		return out, err
	}
	out.Costs[g.Holder] = out.Spent
	for _, t := range state.Tasks {
		for _, a := range t.Attempts {
			if a.Grant != g.Grant {
				continue
			}
			cost, err := b.conversationCost(ctx, a.Conversation, a.Assigned, earliestBound(end, t.ClosedAt, a.EndedAt), fresh)
			if err != nil {
				return out, err
			}
			out.Costs[a.Conversation] += cost
			out.Spent += cost
			if a == t.Current() && !a.Ended && t.ClosedAt.IsZero() && end.IsZero() {
				if entry, ok := state.ApprovedTask(g.Grant, t.ID); ok {
					out.Reserved += max(0, entry.EstimateUSD-cost)
				}
			}
		}
	}
	return out, nil
}

// An open end has no timestamp. Stop attributing spend when the first owner closes.
func earliestBound(bounds ...time.Time) time.Time {
	var end time.Time
	for _, bound := range bounds {
		if !bound.IsZero() && (end.IsZero() || bound.Before(end)) {
			end = bound
		}
	}
	return end
}

// AttemptCost counts usage observed while the conversation was assigned to this task.
func (b *Bench) AttemptCost(ctx context.Context, state *work.State, task *work.Task, a *work.Attempt) float64 {
	cost, _ := b.conversationCost(ctx, a.Conversation, a.Assigned, earliestBound(a.EndedAt, task.ClosedAt, state.CanceledAt), false)
	return cost
}

type costWindow struct {
	conversation string
	since, until time.Time
}

type cachedCost struct {
	at  time.Time
	usd float64
}

func (b *Bench) conversationCost(ctx context.Context, conversation string, since, until time.Time, fresh bool) (float64, error) {
	if b.Telemetry == nil || conversation == "" {
		return 0, errors.New("usage is unavailable")
	}
	key := costWindow{conversation, since, until}
	if v, ok := b.conversationCosts.Load(key); !fresh && ok && time.Since(v.(cachedCost).at) < 5*time.Second {
		return v.(cachedCost).usd, nil
	}
	var prices telemetry.Pricer
	if b.Preferences != nil {
		prices = b.Preferences.Rates()
	}
	report, err := b.Telemetry.Report(ctx, telemetry.Query{Conversation: conversation, Prices: prices, Since: since, Until: until})
	if err != nil {
		return 0, err
	}
	var nanos int64
	for _, g := range report.Groups {
		if g.Nanos != nil {
			nanos += *g.Nanos
		}
	}
	usd := float64(nanos) / 1e9
	b.conversationCosts.Store(key, cachedCost{at: time.Now(), usd: usd})
	return usd, nil
}

// checkBudget refuses a spawn whose estimate would take the tree past its
// cap, and records the stop (once) so the person is asked; it records a
// warning at 80%.
func (b *Bench) checkBudget(ctx context.Context, state *work.State, g *work.Grant, estimate float64, orchestrator *Execution) error {
	limit := g.Ceiling.BudgetUSD
	if limit <= 0 {
		return nil
	}
	// A paid spawn must use current, readable telemetry even if the board just polled.
	tree, err := b.treeBudget(ctx, state, g, true)
	if err != nil {
		return fmt.Errorf("%w: cannot check the mission budget: %v", work.ErrRefused, err)
	}
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
	entry, plan := state.ApprovedTaskPlan(g.Grant, r.Task)
	if plan == nil {
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
	r.contract = plan.Contract
	if entry.Workspace == "pinned" {
		if r.StartFrom != "" || (r.Workspace != "" && r.Workspace != "pinned") {
			return 0, fmt.Errorf("%w: the approved task requires its pinned baseline", work.ErrRefused)
		}
		r.baseline = plan.Baseline
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

func (b *Bench) planBaseline(ctx context.Context, workspace string) ([]work.BaselineMember, error) {
	if workspace == "" || b.Collections == nil {
		return nil, fmt.Errorf("%w: a pinned plan requires a workspace", work.ErrRefused)
	}
	c, err := b.Collections.Get(workspace)
	if err != nil {
		return nil, err
	}
	evidence, err := b.Collections.Evidence(ctx, workspace)
	if err != nil {
		return nil, err
	}
	if len(c.Members) == 0 || len(evidence) != len(c.Members) {
		return nil, fmt.Errorf("%w: workspace membership is empty or changed while capturing the baseline", work.ErrRefused)
	}
	var baseline []work.BaselineMember
	for i, m := range c.Members {
		e := evidence[i]
		if !m.Ready || e.Path != m.Path || e.Name != m.Name || e.Head == "" || e.Error != "" || e.Status != "" {
			return nil, fmt.Errorf("%w: member %q must be ready, clean and readable before proposing a pinned plan", work.ErrRefused, m.Name)
		}
		baseline = append(baseline, work.BaselineMember{Name: m.Name, Repository: m.RepositoryID, Commit: e.Head})
	}
	return baseline, nil
}
