package transport

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// The Orchestrator verbs (`lazybox agents …`, runner/orchestrator.go). Each
// is on the agent allowlist for the caller's own execution only (auth.go);
// spawning also needs a grant in force, and every target must be in the
// caller's lineage. The Runner checks both again, and the mission log
// refuses what a grant does not allow.

// agentError says why an orchestration verb was refused, in words an agent acts on.
func agentError(err error) error {
	switch {
	case err == nil:
		return nil
	case errors.Is(err, runner.ErrNoGrant), errors.Is(err, runner.ErrNoChild):
		return grpcError(codes.PermissionDenied, err)
	}
	return workError(err)
}

func (s *Service) SpawnAgent(ctx context.Context, r *pb.SpawnAgentRequest) (*pb.AgentLineage, error) {
	view, err := s.Bench.Spawn(ctx, runner.SpawnRequest{Session: r.SessionId, Operation: r.OperationId, Title: r.Title, Brief: r.Brief, Task: r.TaskId, Profile: r.ProfileId, Model: r.Model, Permission: r.Permission, Role: r.Role, Workspace: r.Workspace})
	if err != nil {
		return nil, agentError(err)
	}
	return wireLineage(view), nil
}

func (s *Service) ListAgentLineage(ctx context.Context, r *pb.AgentLineageRequest) (*pb.AgentLineage, error) {
	view, err := s.Bench.Lineage(ctx, r.SessionId)
	if err != nil {
		return nil, agentError(err)
	}
	return wireLineage(view), nil
}

func (s *Service) SendAgentMessage(ctx context.Context, r *pb.SendAgentMessageRequest) (*pb.AgentMessageReceipt, error) {
	if len(r.OperationId) > 128 {
		return nil, grpcError(codes.InvalidArgument, errors.New("operation id is longer than 128 bytes"))
	}
	receipt, err := s.Bench.SendAgentMessage(ctx, r.SessionId, r.To, r.Text, r.OperationId)
	if err != nil {
		return nil, agentError(err)
	}
	return &pb.AgentMessageReceipt{Id: receipt.ID, ToConversationId: receipt.To, ToLabel: receipt.ToLabel, Delivery: receipt.Delivery}, nil
}

func (s *Service) WaitAgents(ctx context.Context, r *pb.WaitAgentsRequest) (*pb.AgentLineage, error) {
	view, err := s.Bench.WaitAgents(ctx, runner.WaitRequest{Session: r.SessionId, Targets: r.Targets, Until: r.Until, Any: r.Any, Wait: time.Duration(r.TimeoutSeconds) * time.Second, After: r.AfterSeq})
	if err != nil && !errors.Is(err, context.Canceled) {
		return nil, agentError(err)
	}
	return wireLineage(view), nil
}

func (s *Service) StopChildAgent(ctx context.Context, r *pb.StopChildAgentRequest) (*pb.AgentLineage, error) {
	view, err := s.Bench.StopChild(ctx, r.SessionId, r.Target, r.End, r.Reason)
	if err != nil {
		return nil, agentError(err)
	}
	return wireLineage(view), nil
}

func (s *Service) ProposePlan(ctx context.Context, r *pb.ProposePlanRequest) (*pb.MissionPlan, error) {
	if len(r.OperationId) > 128 {
		return nil, grpcError(codes.InvalidArgument, errors.New("operation id is longer than 128 bytes"))
	}
	tasks := make([]work.PlanTask, 0, len(r.Tasks))
	for _, t := range r.Tasks {
		tasks = append(tasks, work.PlanTask{Key: t.Key, Title: t.Title, Brief: t.Brief, Role: t.Role, Profile: t.ProfileId, Permission: t.Permission, Workspace: t.Workspace, Paths: t.Paths, EstimateUSD: t.EstimateUsd, After: t.After})
	}
	_, plan, err := s.Bench.ProposePlan(ctx, r.SessionId, r.OperationId, r.Summary, tasks)
	if err != nil {
		return nil, agentError(err)
	}
	return wirePlan(plan), nil
}

func (s *Service) ClaimLease(ctx context.Context, r *pb.ClaimLeaseRequest) (*pb.MissionLeases, error) {
	state, lease, err := s.Bench.ClaimLease(ctx, runner.LeaseRequest{Session: r.SessionId, Task: r.TaskId, Path: r.Path, Exclusive: r.Exclusive, TTL: time.Duration(r.TtlSeconds) * time.Second, Reason: r.Reason})
	if err != nil {
		return nil, agentError(err)
	}
	out := wireLeases(state, state.LiveLeases(time.Now()))
	if lease != nil {
		out.Claimed = wireLease(state, lease)
	}
	return out, nil
}

func (s *Service) ReleaseLease(ctx context.Context, r *pb.ReleaseLeaseRequest) (*pb.MissionLeases, error) {
	state, err := s.Bench.ReleaseLease(ctx, r.SessionId, r.Lease)
	if err != nil {
		return nil, agentError(err)
	}
	return wireLeases(state, state.LiveLeases(time.Now())), nil
}

func (s *Service) ListLeases(ctx context.Context, r *pb.LeasesRequest) (*pb.MissionLeases, error) {
	state, leases, err := s.Bench.Leases(ctx, r.SessionId)
	if err != nil {
		return nil, agentError(err)
	}
	return wireLeases(state, leases), nil
}

func wireLeases(state *work.State, leases []*work.Lease) *pb.MissionLeases {
	out := &pb.MissionLeases{MissionId: state.ID}
	for _, l := range leases {
		out.Leases = append(out.Leases, wireLease(state, l))
	}
	return out
}

func wireLease(state *work.State, l *work.Lease) *pb.MissionLease {
	out := &pb.MissionLease{Id: l.Lease, TaskId: l.Task, Path: l.Path, Mode: l.Mode, HolderConversationId: l.Holder.Conversation, ClaimedAtUnixMillis: millis(l.Claimed), ExpiresAtUnixMillis: millis(l.Expires()), Reason: l.Reason}
	if t := state.Task(l.Task); t != nil {
		out.TaskTitle = t.Title
	}
	return out
}

func wirePlan(p *work.Plan) *pb.MissionPlan {
	out := &pb.MissionPlan{Id: p.Plan, GrantId: p.Grant, Summary: p.Summary, TotalUsd: p.TotalUSD(), State: p.State, CapUsd: p.CapUSD, Note: p.Note, ProposedAtUnixMillis: millis(p.Proposed), DecidedAtUnixMillis: millis(p.Decided), ConversationId: p.ProposedBy.Conversation, ProposedBy: p.ProposedBy.Kind}
	for _, t := range p.Tasks {
		out.Tasks = append(out.Tasks, &pb.PlanTask{Key: t.Key, Title: t.Title, Brief: t.Brief, Role: t.Role, ProfileId: t.Profile, Permission: t.Permission, Workspace: t.Workspace, Paths: t.Paths, EstimateUsd: t.EstimateUSD, After: t.After, TaskId: p.Created[t.Key]})
	}
	for _, o := range p.Overlaps() {
		out.Overlaps = append(out.Overlaps, &pb.PlanOverlap{Path: o.Path, Keys: o.Keys})
	}
	return out
}

func (s *Service) ReportAgentTask(ctx context.Context, r *pb.ReportAgentTaskRequest) (*pb.AgentLineage, error) {
	view, err := s.Bench.ReportAgentTask(ctx, r.SessionId, r.Summary)
	if err != nil {
		return nil, agentError(err)
	}
	return wireLineage(view), nil
}

// grant and revoke are UpdateMission's grant changes: the person's alone.
func (s *Service) grant(ctx context.Context, mission, key string, g *pb.OrchestratorGrantChange) (*work.State, string, error) {
	if g == nil || g.SessionId == "" {
		return nil, "", errors.New("choose the conversation that orchestrates")
	}
	c := work.Ceiling{}
	if g.Ceiling != nil {
		c = work.Ceiling{Permission: g.Ceiling.Permission, Profiles: g.Ceiling.ProfileIds, MaxChildren: int(g.Ceiling.MaxChildren), Depth: int(g.Ceiling.Depth), BudgetUSD: g.Ceiling.BudgetUsd, PlanRequired: g.Ceiling.PlanRequired, ReviewLimit: int(g.Ceiling.ReviewLimit)}
	}
	return s.Bench.Grant(ctx, runner.GrantRequest{Mission: mission, Session: g.SessionId, Operation: key, Ceiling: c})
}

func wireCeiling(c work.Ceiling) *pb.OrchestratorCeiling {
	return &pb.OrchestratorCeiling{Permission: c.Permission, ProfileIds: c.Profiles, MaxChildren: uint32(c.MaxChildren), Depth: uint32(c.Depth), BudgetUsd: c.BudgetUSD, PlanRequired: c.PlanRequired, ReviewLimit: uint32(c.ReviewLimit)}
}

func wireGrant(state *work.State, g *work.Grant, session string, budget runner.TreeBudget) *pb.MissionGrant {
	if g == nil {
		return nil
	}
	children := 0
	for _, t := range state.Tasks {
		for _, a := range t.Attempts {
			if a.Grant == g.Grant {
				children++
			}
		}
	}
	return &pb.MissionGrant{Id: g.Grant, ConversationId: g.Holder, SessionId: firstNonEmpty(session, g.Session), Title: g.Title, ParentGrantId: g.Parent, Ceiling: wireCeiling(g.Ceiling), Scopes: g.Scopes,
		IssuedAtUnixMillis: millis(g.Issued), RevokedAtUnixMillis: millis(g.RevokedAt), RevokeReason: g.Reason, IssuedBy: g.IssuedBy.Kind, RevokedBy: g.RevokedBy.Kind,
		Live: state.Live(g), LiveChildren: uint32(state.LiveChildren(g.Grant)), Children: uint32(children),
		SpentUsd: budget.Spent, ReservedUsd: budget.Reserved, BudgetState: state.BudgetState(g.Grant)}
}

func wireChild(v runner.ChildView) *pb.AgentChild {
	return &pb.AgentChild{ConversationId: v.Conversation, SessionId: v.Session, Title: v.Title, TaskId: v.Task, TaskTitle: v.TaskTitle, TaskStatus: v.TaskStatus, CollectionId: v.Workspace, WorkspaceName: v.WorkspaceName,
		ProfileId: v.Profile, Model: v.Model, Permission: v.Permission, Role: v.Role, Live: v.Live, LiveDetail: v.LiveDetail, Reported: v.Reported, Report: v.Report, LastMessage: v.LastMessage,
		StartedAtUnixMillis: millis(v.Started), Ended: v.Ended, Outcome: v.Outcome, Relation: v.Relation, CostUsd: v.CostUSD, EstimateUsd: v.EstimateUSD}
}

func wireLineage(v runner.LineageView) *pb.AgentLineage {
	out := &pb.AgentLineage{Seq: v.Seq, Matched: v.Matched, Reason: v.Reason}
	if v.Mission != nil {
		out.MissionId, out.MissionTitle = v.Mission.ID, v.Mission.Title
		out.Grant = wireGrant(v.Mission, v.Grant, "", v.Budget)
	}
	if v.Parent != nil {
		out.Parent = wireChild(*v.Parent)
	}
	for _, c := range v.Children {
		out.Children = append(out.Children, wireChild(c))
	}
	for _, e := range v.Events {
		out.Events = append(out.Events, &pb.AgentLineageEvent{Seq: e.Seq, AtUnixMillis: millis(e.At), Type: e.Type, ConversationId: e.Conversation, Summary: e.Summary})
	}
	if v.Spawned != nil {
		out.Spawned = wireChild(*v.Spawned)
	}
	if v.Plan != nil {
		out.Plan = wirePlan(v.Plan)
	}
	out.SpentUsd, out.ReservedUsd = v.Budget.Spent, v.Budget.Reserved
	return out
}

// sessionLineage is where a conversation sits in an agent tree, for the app.
func (s *Service) sessionLineage(ctx context.Context, e *runner.Execution) *pb.SessionLineage {
	l, ok := s.Bench.LineageOf(ctx, e)
	if !ok {
		return nil
	}
	return &pb.SessionLineage{ParentConversationId: l.Parent, ParentTitle: l.ParentTitle, MissionId: l.Mission, MissionTitle: l.MissionTitle, TaskId: l.Task, Role: l.Role, GrantId: l.Grant, LiveChildren: uint32(l.LiveChildren), MaxChildren: uint32(l.MaxChildren)}
}

// capSuffix is " ($20.00 cap)" from a plan decision or a budget event.
func capSuffix(e work.Event) string {
	var p struct {
		CapUSD float64 `json:"cap_usd"`
	}
	_ = json.Unmarshal(e.Payload, &p)
	if p.CapUSD <= 0 {
		return ""
	}
	return fmt.Sprintf(" ($%.2f cap)", p.CapUSD)
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if strings.TrimSpace(v) != "" {
			return v
		}
	}
	return ""
}
