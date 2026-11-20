package main

import (
	"fmt"
	"io"
	"os"
	"strings"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"gopkg.in/yaml.v3"
)

// `lazybox mission plan` is how an orchestrator proposes its split of a
// mission into agents (sota-agents.md §6.12); `approve`, `reject` and
// `budget` are the person's decisions, owner verbs like `missions accept`.

const missionUsage = `Usage:
  lazybox mission plan <file|->            (as an orchestrator: propose; YAML or JSON)
  lazybox mission approve <mission> [--plan ID] [--cap 20] [--wip 3] [--note …]
  lazybox mission reject <mission> [--plan ID] --note "why"
  lazybox mission budget <mission> --cap 40
  lazybox mission integrate <mission> --plan ID --key KEY --check "combined check"
  lazybox mission promote <mission> --candidate ID

A plan:
  summary: index first, then rank; docs alongside
  tasks:
    - key: index
      title: Index the corpus
      brief: "Objective: … Done when: … Boundaries: … Output: …"
      role: worker
      permission: ask            # read-only | ask | yolo, within your grant
      workspace: new             # same | new | pinned (proposal-time commits)
      paths: [search/]
      estimate_usd: 4
    - key: rank
      title: Rank results
      after: [index]
      estimate_usd: 3`

// planFile is a plan as an agent writes it.
type planFile struct {
	Contract *struct {
		Artifact string `yaml:"artifact" json:"artifact"`
		Version  uint32 `yaml:"version" json:"version"`
	} `yaml:"contract" json:"contract"`

	Summary string `yaml:"summary" json:"summary"`
	Tasks   []struct {
		Key         string   `yaml:"key" json:"key"`
		Title       string   `yaml:"title" json:"title"`
		Brief       string   `yaml:"brief" json:"brief"`
		Role        string   `yaml:"role" json:"role"`
		Profile     string   `yaml:"profile" json:"profile"`
		Permission  string   `yaml:"permission" json:"permission"`
		Workspace   string   `yaml:"workspace" json:"workspace"`
		Paths       []string `yaml:"paths" json:"paths"`
		EstimateUSD float64  `yaml:"estimate_usd" json:"estimate_usd"`
		After       []string `yaml:"after" json:"after"`
	} `yaml:"tasks" json:"tasks"`
}

func (c *cli) mission(args []string) error {
	if c.subcommandHelp(args, missionUsage) {
		return nil
	}
	switch args[0] {
	case "integrate", "promote":
		return c.missionIntegration(args[0], args[1:])
	case "plan":
		return c.missionPlan(args[1:])
	case "approve", "reject":
		return c.missionDecide(args[0] == "approve", args[1:])
	case "budget":
		return c.missionBudget(args[1:])
	}
	fmt.Fprintln(c.stderr, missionUsage)
	return fail(exitUsage, "unknown mission command %q", args[0])
}

func (c *cli) missionPlan(args []string) error {
	f := c.flags("mission")
	session := f.String("session", "", "the orchestrator's session (default: this agent's)")
	key := f.String("key", "", "idempotency key: a retried proposal with the same key is one plan")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		fmt.Fprintln(c.stderr, missionUsage)
		return fail(exitUsage, "give the plan's file, or - to read it from stdin")
	}
	var data []byte
	if positional[0] == "-" {
		data, err = io.ReadAll(io.LimitReader(c.stdin, 256<<10))
	} else {
		data, err = os.ReadFile(positional[0])
	}
	if err != nil {
		return err
	}
	var plan planFile
	// YAML is a superset of JSON: one decoder reads both.
	if err = yaml.Unmarshal(data, &plan); err != nil {
		return fail(exitUsage, "the plan is not YAML or JSON: %v", err)
	}
	if len(plan.Tasks) == 0 {
		return fail(exitUsage, "the plan has no tasks")
	}
	req := &pb.ProposePlanRequest{SessionId: me, OperationId: *key, Summary: plan.Summary}
	if plan.Contract != nil {
		if plan.Contract.Artifact == "" || plan.Contract.Version == 0 {
			return fail(exitUsage, "contract requires an artifact ID and positive version")
		}
		req.Contract = &pb.PlanContract{ArtifactId: plan.Contract.Artifact, Version: plan.Contract.Version}
	}
	for i, t := range plan.Tasks {
		if t.Key == "" {
			t.Key = fmt.Sprintf("t%d", i+1)
		}
		req.Tasks = append(req.Tasks, &pb.PlanTask{Key: t.Key, Title: t.Title, Brief: t.Brief, Role: t.Role, ProfileId: t.Profile, Permission: t.Permission, Workspace: t.Workspace, Paths: t.Paths, EstimateUsd: t.EstimateUSD, After: t.After})
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	out, err := got(cl.ProposePlan(c.ctx, connect.NewRequest(req)))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	c.out("Proposed plan %s: %d tasks, est. $%.2f. It waits for the person's approval; you are told when they decide.\n", out.Id, len(out.Tasks), out.TotalUsd)
	for _, o := range out.Overlaps {
		c.out("  %s is in %s: they would edit the same files; order them with after:, or expect the person to ask.\n", o.Path, strings.Join(o.Keys, " and "))
	}
	return nil
}

// missionByID finds a mission and its plan waiting for a decision (or the one named).
func (c *cli) missionPlanOf(cl *client, mission, plan string) (*pb.Mission, *pb.MissionPlan, error) {
	m, err := got(cl.GetMission(c.ctx, connect.NewRequest(&pb.GetMissionRequest{MissionId: mission})))
	if err != nil {
		return nil, nil, err
	}
	for _, p := range m.Plans {
		if (plan != "" && p.Id == plan) || (plan == "" && p.State == "proposed") {
			return m, p, nil
		}
	}
	if plan != "" {
		return m, nil, fail(exitNotFound, "mission %s has no plan %s", mission, plan)
	}
	return m, nil, fail(exitNotFound, "no plan waits for a decision on “%s”", m.Title)
}

func (c *cli) missionDecide(approve bool, args []string) error {
	f := c.flags("mission")
	planID := f.String("plan", "", "the plan (default: the one waiting)")
	capUSD := f.Float64("cap", 0, "approve: what the whole tree may spend, in dollars (0: no cap)")
	wip := f.Uint("wip", 0, "approve: agents at once (default: unchanged)")
	note := f.String("note", "", "a note recorded with the decision, and told to the orchestrator")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox mission %s <mission> [--plan ID]", map[bool]string{true: "approve", false: "reject"}[approve])
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	m, plan, err := c.missionPlanOf(cl, positional[0], *planID)
	if err != nil {
		return err
	}
	decision := &pb.PlanDecision{PlanId: plan.Id, CapUsd: *capUSD, MaxChildren: uint32(*wip), Note: *note}
	change := &pb.UpdateMissionRequest{MissionId: m.Id, OperationId: fmt.Sprintf("decide-%s-%d", plan.Id, c.now().UnixNano())}
	if approve {
		change.Change = &pb.UpdateMissionRequest_ApprovePlan{ApprovePlan: decision}
	} else {
		change.Change = &pb.UpdateMissionRequest_RejectPlan{RejectPlan: decision}
	}
	out, err := got(cl.UpdateMission(c.ctx, connect.NewRequest(change)))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	if approve {
		c.out("Approved: %d tasks on “%s”, %s.\n", len(plan.Tasks), m.Title, map[bool]string{true: fmt.Sprintf("$%.2f cap", *capUSD), false: "no cap"}[*capUSD > 0])
	} else {
		c.out("Rejected the plan for “%s”.\n", m.Title)
	}
	return nil
}

func (c *cli) missionBudget(args []string) error {
	f := c.flags("mission")
	capUSD := f.Float64("cap", -1, "what the whole tree may spend, in dollars (0: no cap)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 || *capUSD < 0 {
		return fail(exitUsage, "usage: lazybox mission budget <mission> --cap 40")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	m, err := got(cl.GetMission(c.ctx, connect.NewRequest(&pb.GetMissionRequest{MissionId: positional[0]})))
	if err != nil {
		return err
	}
	var grant *pb.MissionGrant
	for _, g := range m.Grants {
		if g.Live {
			grant = g
		}
	}
	if grant == nil {
		return fail(exitNotFound, "no conversation runs agents for “%s”", m.Title)
	}
	out, err := got(cl.UpdateMission(c.ctx, connect.NewRequest(&pb.UpdateMissionRequest{MissionId: m.Id, Change: &pb.UpdateMissionRequest_RaiseBudget{RaiseBudget: &pb.BudgetRaise{GrantId: grant.Id, CapUsd: *capUSD}}})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	c.out("The tree of “%s” may spend $%.2f; it spent $%.2f.\n", m.Title, *capUSD, grant.SpentUsd)
	return nil
}

func (c *cli) missionIntegration(verb string, args []string) error {
	f := c.flags("mission " + verb)
	plan := f.String("plan", "", "approved pinned plan")
	key := f.String("key", "", "unique candidate request key")
	check := f.String("check", "", "combined check from the candidate workspace root")
	candidate := f.String("candidate", "", "tested candidate to accept")
	pos, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(pos) != 1 {
		return fail(exitUsage, "give the mission ID")
	}
	req := &pb.UpdateMissionRequest{MissionId: pos[0], OperationId: *key}
	if verb == "integrate" {
		if *plan == "" || *key == "" || *check == "" || *candidate != "" {
			return fail(exitUsage, "integrate requires --plan, --key and --check")
		}
		req.Change = &pb.UpdateMissionRequest_PrepareCandidate{PrepareCandidate: &pb.IntegrationPrepare{PlanId: *plan, CheckArgv: []string{"sh", "-c", *check}}}
	} else {
		if *candidate == "" || *plan != "" || *check != "" || *key != "" {
			return fail(exitUsage, "promote requires --candidate")
		}
		req.Change = &pb.UpdateMissionRequest_PromoteCandidate{PromoteCandidate: &pb.IntegrationPromote{CandidateId: *candidate}}
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	out, err := got(cl.UpdateMission(c.ctx, connect.NewRequest(req)))
	if err != nil {
		return err
	}
	id := *candidate
	if verb == "integrate" {
		id = out.CreatedId
	}
	for _, result := range out.Candidates {
		if result.Id == id {
			if c.json {
				if err = c.printProtoJSON(result); err != nil {
					return err
				}
			} else {
				c.out("Candidate %s: %s (base generation %d, check %s) %s\n", id, result.State, result.BaseGeneration, result.CommandId, result.Detail)
			}
			if result.State == "failed" {
				return fmt.Errorf("integration candidate failed: %s", result.Detail)
			}
			return nil
		}
	}
	return fmt.Errorf("Runner returned no integration candidate %s", id)
}
