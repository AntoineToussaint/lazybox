package transport

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Missions (ORCHESTRATION.md slice 1). Every RPC here is the owner's: none is
// on the agent or terminal allowlist, so an agent cannot create, assign,
// report or accept work, and a human gate is passed only by the person.

func (s *Service) missions() (*work.Store, error) {
	if s.Bench.Work == nil {
		return nil, grpcError(codes.FailedPrecondition, errors.New("this Runner keeps no missions"))
	}
	return s.Bench.Work, nil
}

// workError says why a change was refused, without the internal prefix.
func workError(err error) error {
	switch {
	case err == nil:
		return nil
	case errors.Is(err, work.ErrRefused):
		return grpcError(codes.FailedPrecondition, errors.New(strings.TrimPrefix(err.Error(), work.ErrRefused.Error()+": ")))
	case errors.Is(err, os.ErrNotExist):
		return grpcError(codes.NotFound, err)
	}
	return rpcError(err)
}

func (s *Service) ListMissions(ctx context.Context, _ *pb.ListMissionsRequest) (*pb.ListMissionsResponse, error) {
	store, err := s.missions()
	if err != nil {
		return nil, err
	}
	out := &pb.ListMissionsResponse{}
	for _, m := range store.Missions() {
		out.Missions = append(out.Missions, s.wireMission(ctx, m.State, m.Err, false))
	}
	return out, nil
}

func (s *Service) GetMission(ctx context.Context, r *pb.GetMissionRequest) (*pb.Mission, error) {
	store, err := s.missions()
	if err != nil {
		return nil, err
	}
	state, err := store.Mission(r.MissionId)
	if errors.Is(err, os.ErrNotExist) {
		return nil, workError(err)
	}
	if r.RefreshEvidence && err == nil {
		s.refreshPullRequests(state)
	}
	return s.wireMission(ctx, state, err, true), nil
}

func (s *Service) CreateMission(ctx context.Context, r *pb.CreateMissionRequest) (*pb.Mission, error) {
	store, err := s.missions()
	if err != nil {
		return nil, err
	}
	state, err := store.Create(work.Human, r.RequestKey, r.Title, r.Brief)
	if err != nil {
		return nil, workError(err)
	}
	return s.wireMission(ctx, state, nil, true), nil
}

func (s *Service) UpdateMission(ctx context.Context, r *pb.UpdateMissionRequest) (*pb.Mission, error) {
	store, err := s.missions()
	if err != nil {
		return nil, err
	}
	if len(r.OperationId) > 128 {
		return nil, grpcError(codes.InvalidArgument, errors.New("operation id is longer than 128 bytes"))
	}
	id, key := r.MissionId, r.OperationId
	var state *work.State
	created, session := "", ""
	switch c := r.Change.(type) {
	case *pb.UpdateMissionRequest_PrepareCandidate:
		state, created, err = s.Bench.PrepareCandidate(ctx, id, key, c.PrepareCandidate.PlanId, c.PrepareCandidate.CheckArgv)
	case *pb.UpdateMissionRequest_PromoteCandidate:
		state, err = s.Bench.PromoteCandidate(ctx, id, c.PromoteCandidate.CandidateId)
	case *pb.UpdateMissionRequest_Edit:
		state, err = store.EditMission(id, work.Human, key, c.Edit.GetTitle(), c.Edit.GetBrief())
	case *pb.UpdateMissionRequest_Cancel:
		state, err = store.CancelMission(id, work.Human, key, c.Cancel.GetReason())
	case *pb.UpdateMissionRequest_AddGoal:
		state, created, err = store.AddGoal(id, work.Human, key, c.AddGoal.GetTitle())
	case *pb.UpdateMissionRequest_AddTask:
		t := c.AddTask
		state, created, err = store.AddTask(id, work.Human, key, t.GetGoalId(), t.GetTitle(), t.GetBrief(), t.GetBlockedBy())
	case *pb.UpdateMissionRequest_EditTask:
		state, err = store.EditTask(id, c.EditTask.GetTaskId(), work.Human, key, c.EditTask.GetTitle(), c.EditTask.GetBrief())
	case *pb.UpdateMissionRequest_CancelTask:
		state, err = store.CancelTask(id, c.CancelTask.GetTaskId(), work.Human, key, c.CancelTask.GetReason())
	case *pb.UpdateMissionRequest_Assign:
		state, created, session, err = s.assign(ctx, store, id, key, c.Assign)
	case *pb.UpdateMissionRequest_Report:
		state, err = store.Report(id, c.Report.GetTaskId(), work.Human, key, c.Report.GetSummary())
	case *pb.UpdateMissionRequest_Accept:
		state, err = store.Decide(id, c.Accept.GetTaskId(), work.Human, key, true, c.Accept.GetNote())
	case *pb.UpdateMissionRequest_Reject:
		state, err = store.Decide(id, c.Reject.GetTaskId(), work.Human, key, false, c.Reject.GetNote())
	case *pb.UpdateMissionRequest_Grant:
		state, created, err = s.grant(ctx, id, key, c.Grant)
	case *pb.UpdateMissionRequest_RevokeGrant:
		state, err = s.Bench.Revoke(ctx, id, c.RevokeGrant.GetGrantId(), key, c.RevokeGrant.GetReason())
	case *pb.UpdateMissionRequest_ApprovePlan:
		d := c.ApprovePlan
		state, err = s.Bench.DecidePlan(ctx, id, d.GetPlanId(), key, true, d.GetCapUsd(), int(d.GetMaxChildren()), d.GetNote())
	case *pb.UpdateMissionRequest_RejectPlan:
		state, err = s.Bench.DecidePlan(ctx, id, c.RejectPlan.GetPlanId(), key, false, 0, 0, c.RejectPlan.GetNote())
	case *pb.UpdateMissionRequest_RaiseBudget:
		state, err = s.Bench.RaiseBudget(ctx, id, c.RaiseBudget.GetGrantId(), key, c.RaiseBudget.GetCapUsd())
	default:
		return nil, grpcError(codes.InvalidArgument, errors.New("a typed change is required"))
	}
	if err != nil {
		return nil, workError(err)
	}
	out := s.wireMission(ctx, state, nil, true)
	out.CreatedId, out.SessionId = created, session
	return out, nil
}

// assign gives a task to an existing conversation, or opens one in the
// chosen workspace, and optionally sends it the task's brief as the
// conversation's next message (recorded as a mission prompt).
func (s *Service) assign(ctx context.Context, store *work.Store, mission, key string, a *pb.TaskAssign) (*work.State, string, string, error) {
	// OpenSession allocates a conversation before Store.Assign records its ID.
	// Serialize this task's requests so a concurrent retry sees the saved ID.
	scope := [2]string{mission, a.GetTaskId()}
	done := make(chan struct{})
	for {
		active, busy := s.assigning.LoadOrStore(scope, done)
		if !busy {
			break
		}
		select {
		case <-active.(chan struct{}):
		case <-ctx.Done():
			return nil, "", "", ctx.Err()
		}
	}
	defer func() { s.assigning.Delete(scope); close(done) }()
	current, err := store.Mission(mission)
	if err != nil {
		return nil, "", "", err
	}
	task := current.Task(a.GetTaskId())
	if task == nil {
		return nil, "", "", fmt.Errorf("%w: task %q does not exist", work.ErrRefused, a.GetTaskId())
	}
	if key != "" {
		// A retried assignment opens no second conversation.
		if e, ok := current.Event(key); ok {
			var p work.AttemptPayload
			if e.Type != work.TaskAssigned || json.Unmarshal(e.Payload, &p) != nil || p.Task != task.ID {
				return nil, "", "", fmt.Errorf("%w: operation already belongs to another change", work.ErrRefused)
			}
			err := s.sendAssignmentBrief(ctx, current, task, p)
			return current, p.Attempt, p.Execution, err
		}
	}
	if current.Canceled || task.Canceled || current.Done(task) {
		return nil, "", "", fmt.Errorf("%w: only an open task can be assigned", work.ErrRefused)
	}
	var e *runner.Execution
	switch {
	case a.Open != nil && a.SessionId != "":
		return nil, "", "", fmt.Errorf("%w: choose a conversation or open a new one, not both", work.ErrRefused)
	case a.Open != nil:
		if a.Open.CollectionId == "" {
			return nil, "", "", fmt.Errorf("%w: choose the workspace the new conversation works in", work.ErrRefused)
		}
		opened, err := s.OpenSession(ctx, a.Open)
		if err != nil {
			return nil, "", "", err
		}
		if e, err = s.Bench.Get(opened.GetSession().GetId()); err != nil {
			return nil, "", "", err
		}
	default:
		if e, err = s.Bench.Get(a.SessionId); err != nil {
			return nil, "", "", err
		}
	}
	if e.Purpose != "" && e.Purpose != "coding" {
		return nil, "", "", fmt.Errorf("%w: an Ask Lazybox conversation cannot work on a task", work.ErrRefused)
	}
	conversation := e.Saved.ConversationID
	if conversation == "" {
		conversation = e.ID
	}
	payload := work.AttemptPayload{Conversation: conversation, Workspace: e.CollectionID, Execution: e.ID, New: a.Open != nil}
	if a.SendBrief {
		operation := key
		if operation == "" {
			operation = commands.ID()
		}
		payload.BriefOperation = fmt.Sprintf("mission-brief-%x", sha256.Sum256([]byte(mission+"/"+operation)))
	}
	state, attempt, err := store.Assign(mission, task.ID, work.Human, key, payload)
	if err != nil {
		return nil, "", "", err
	}
	payload.Attempt = attempt
	err = s.sendAssignmentBrief(ctx, state, state.Task(task.ID), payload)
	return state, attempt, e.ID, err
}

// The operation is persisted with the assignment before delivery. Old assignments
// carry no pending operation; a retry returns their original result.
func (s *Service) sendAssignmentBrief(ctx context.Context, state *work.State, task *work.Task, p work.AttemptPayload) error {
	if p.BriefOperation == "" || (s.Bench.Prompts != nil && s.Bench.Prompts.Submitted(p.Conversation, p.BriefOperation)) {
		return nil
	}
	if at := task.Current(); at == nil || at.ID != p.Attempt || at.Ended || task.Canceled || state.Canceled || state.Done(task) {
		return fmt.Errorf("%w: assignment ended before its brief was delivered", work.ErrRefused)
	}
	origin := &agent.Origin{Kind: agent.OriginMissionBrief, Summary: "Task assigned · " + task.Title, Detail: state.Title}
	receipt, err := s.apply(ctx, &pb.ApplyRequest{SessionId: p.Execution, OperationId: p.BriefOperation, Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: briefMessage(state, task), Source: &pb.PromptSource{Kind: work.SourceMission, MissionId: state.ID, TaskId: task.ID}}}}, origin)
	if err != nil {
		return fmt.Errorf("assigned, but the brief was not delivered: %w", err)
	}
	if receipt.Error != "" {
		return fmt.Errorf("assigned, but the brief was not delivered: %s", receipt.Error)
	}
	return nil
}

// briefMessage is the message a conversation receives when a task is
// assigned to it: the task's brief, and how its work will be judged.
func briefMessage(m *work.State, t *work.Task) string {
	var b strings.Builder
	b.WriteString(t.Title)
	if t.Brief != "" {
		b.WriteString("\n\n" + t.Brief)
	}
	fmt.Fprintf(&b, "\n\n(Task of the Lazybox mission “%s”. Run tests and builds with `lazybox run -- …`: their exit status is recorded as this task's evidence. Say when you are done; the person reviews and accepts the result.)", m.Title)
	return b.String()
}

// Refresh only PRs explicitly delivered on the selected mission's live attempts.
func (s *Service) refreshPullRequests(state *work.State) {
	if !s.githubConnected() {
		return
	}
	for _, t := range state.Tasks {
		a := t.Current()
		if a == nil || a.Ended || state.Canceled || t.Canceled || state.Done(t) {
			continue
		}
		for _, entry := range t.Evidence {
			if entry.Attempt != a.ID || !entry.IsPullRequestLink() {
				continue
			}
			key := entry.Workspace + "/" + entry.Ref
			now := time.Now()
			last, loaded := s.prRefreshed.LoadOrStore(key, now)
			if loaded && (now.Sub(last.(time.Time)) < time.Minute || !s.prRefreshed.CompareAndSwap(key, last, now)) {
				continue
			}
			go func(link work.Evidence) {
				ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
				defer cancel()
				s.Bench.RefreshMissionPullRequest(ctx, link)
			}(entry.Evidence)
		}
	}
}

// observePullRequest records what the PR loop read about a member's branch:
// the workspace delivers that pull request (a tracked item whose transitions
// fire hooks). Checks reach only attempts explicitly linked to this PR.
func (s *Service) observePullRequest(collection, member string, out *pb.PullRequestStatus) {
	if out == nil || !out.Found {
		return
	}
	s.observeLoop(collection, member, out, statusOf(out), false, time.Time{})
}

// observeLoop records a PR loop read; readAt is when it began
// (Bench.ItemReadStart), so a read overtaken by a newer one is dropped.
func (s *Service) observeLoop(collection, member string, out *pb.PullRequestStatus, status github.PullRequestStatus, created bool, readAt time.Time) {
	if out == nil || !out.Found {
		return
	}
	s.Bench.ObservePullRequestLoop(collection, member, out.GithubRepository, status, created, readAt)
	pr := runner.PullRequestObservation{Workspace: collection, Member: member, Repository: out.GithubRepository, Title: out.Title, URL: out.Url, State: out.State, Head: out.HeadSha, Checks: out.ChecksState, Number: out.Number, Total: len(out.Checks)}
	for _, c := range out.Checks {
		if c.Status == "completed" && (c.Conclusion == "failure" || c.Conclusion == "timed_out" || c.Conclusion == "cancelled" || c.Conclusion == "action_required") {
			pr.Failed = append(pr.Failed, c.Name)
		}
	}
	s.Bench.RecordPullRequest(pr)
}

func (s *Service) ListPromptHistory(ctx context.Context, r *pb.ListPromptHistoryRequest) (*pb.PromptHistory, error) {
	if s.Bench.Prompts == nil {
		return &pb.PromptHistory{}, nil
	}
	conversation := r.ConversationId
	if conversation != "" {
		// Any session of the conversation names it.
		if e, err := s.Bench.Get(conversation); err == nil && e.Saved.ConversationID != "" {
			conversation = e.Saved.ConversationID
		}
	}
	out := &pb.PromptHistory{}
	for _, p := range s.Bench.Prompts.History(work.HistoryQuery{Conversation: conversation, Workspace: r.CollectionId, Search: r.Search, Limit: int(r.Limit)}) {
		out.Prompts = append(out.Prompts, &pb.PromptRecord{Text: p.Text, ConversationId: p.Conversation, CollectionId: p.Workspace, SessionId: p.Execution, AtUnixMillis: p.At.UnixMilli(), Source: wireSource(p.Source)})
	}
	memory := s.Bench.Prompts.Snippets(conversation, r.CollectionId)
	out.LastSnippet, out.NextSnippets, out.RecentSnippets = memory.Last, memory.Next, memory.Recent
	return out, nil
}

func wireSource(source work.PromptSource) *pb.PromptSource {
	return &pb.PromptSource{Kind: source.Kind, SnippetKey: source.Snippet, CommandId: source.Command, MissionId: source.Mission, TaskId: source.Task}
}

func promptSource(source *pb.PromptSource) work.PromptSource {
	if source == nil {
		return work.PromptSource{}
	}
	kind := source.Kind
	switch kind {
	case "", work.SourceTyped, work.SourceSnippet, work.SourceConsole, work.SourceInbox, work.SourceMission, work.SourceHandoff:
	default:
		kind = work.SourceTyped
	}
	return work.PromptSource{Kind: kind, Snippet: source.SnippetKey, Command: source.CommandId, Mission: source.MissionId, Task: source.TaskId}
}

func millis(t time.Time) int64 {
	if t.IsZero() {
		return 0
	}
	return t.UnixMilli()
}

// wireMission derives the board of a mission with the Runner's live
// observations. full adds the event log.
func (s *Service) wireMission(ctx context.Context, state *work.State, readErr error, full bool) *pb.Mission {
	out := &pb.Mission{Id: state.ID, Title: state.Title, Brief: state.Brief, Process: state.Process.Preset, ProcessDigest: state.Process.Digest, Autonomy: int32(state.Process.Autonomy), CreatedAtUnixMillis: millis(state.Created), UpdatedAtUnixMillis: millis(state.Updated), Canceled: state.Canceled, Seq: state.Seq, Ignored: uint32(state.Ignored)}
	out.AcceptedGeneration, out.AcceptedCandidateId = state.Generation, state.AcceptedCandidate
	for _, c := range state.Candidates {
		w := &pb.IntegrationCandidate{Id: c.ID, PlanId: c.Plan, BaseGeneration: c.Generation, State: c.State, CommandId: c.Command, Detail: c.Detail, CheckArgv: c.Check, TaskReviews: c.Reviews, RepairTaskId: c.RepairTask, Revisions: map[string]string{}}
		for _, m := range c.Members {
			w.Revisions[m.Name] = m.Commit
		}
		out.Candidates = append(out.Candidates, w)
	}

	if readErr != nil {
		out.Error = readErr.Error()
		return out
	}
	for _, g := range state.Goals {
		out.Goals = append(out.Goals, &pb.MissionGoal{Id: g.ID, Title: g.Title})
	}
	var conversations []string
	for _, t := range state.Tasks {
		for _, a := range t.Attempts {
			conversations = append(conversations, a.Conversation)
		}
	}
	for _, g := range state.Grants {
		conversations = append(conversations, g.Holder)
	}
	obs := s.Bench.Observe(ctx, conversations)
	for _, v := range work.Derive(state, obs) {
		task := &pb.MissionTask{Id: v.ID, GoalId: v.Goal, Title: v.Title, Brief: v.Brief, Status: v.Status, Trust: v.Trust, EvidenceTrust: v.EvidenceTrust, Reason: v.Reason, Flags: v.Flags, Gate: v.Gate, BlockedBy: v.BlockedBy, CreatedAtUnixMillis: millis(v.Created), UpdatedAtUnixMillis: millis(v.Task.Updated)}
		for _, a := range v.Attempts {
			wired := wireAttempt(a, obs.Conversations[a.Conversation])
			task.Attempts = append(task.Attempts, wired)
			if a == v.Attempt {
				task.Attempt = wired
			}
		}
		if report, ok := v.LastReport(); ok {
			task.Report, task.ReportedAtUnixMillis = report.Summary, millis(report.At)
		}
		// What it cost so far, against what its plan estimated.
		for _, a := range v.Attempts {
			task.CostUsd += s.Bench.AttemptCost(ctx, state, v.Task, a)
		}
		for _, p := range state.Plans {
			for key, created := range p.Created {
				if created != v.ID {
					continue
				}
				task.PlanId = p.Plan
				for _, entry := range p.Tasks {
					if entry.Key == key {
						task.EstimateUsd = entry.EstimateUSD
					}
				}
			}
		}
		for i := len(v.Gates) - 1; i >= 0; i-- {
			if g := v.Gates[i]; g.Type != work.GateRequested {
				task.DecisionNote = g.Note
				break
			}
		}
		for i := len(v.Evidence) - 1; i >= 0; i-- {
			task.Evidence = append(task.Evidence, wireEvidence(v.Evidence[i]))
		}
		if v.Attempt != nil && !v.Attempt.Ended && s.Bench.Prompts != nil {
			memory := s.Bench.Prompts.Snippets(v.Attempt.Conversation, "")
			task.LastSnippet, task.NextSnippets = memory.Last, memory.Next
		}
		out.Tasks = append(out.Tasks, task)
	}
	for _, g := range state.Grants {
		out.Grants = append(out.Grants, wireGrant(state, g, obs.Conversations[g.Holder].Session, s.Bench.TreeBudgetOf(ctx, state, g)))
	}
	for i := len(state.Plans) - 1; i >= 0; i-- {
		out.Plans = append(out.Plans, wirePlan(state.Plans[i]))
	}
	for _, l := range state.LiveLeases(time.Now()) {
		out.Leases = append(out.Leases, wireLease(state, l))
	}
	for i := len(state.Overlaps) - 1; i >= 0; i-- {
		o := state.Overlaps[i]
		out.Overlaps = append(out.Overlaps, &pb.MissionOverlap{TaskIds: o.Tasks, Paths: o.Paths, Kind: o.Kind, LeaseId: o.Lease})
	}
	out.AwaitingReview = uint32(state.AwaitingReview())
	if full {
		for i := len(state.Events) - 1; i >= 0 && len(out.Events) < 200; i-- {
			out.Events = append(out.Events, wireMissionEvent(state, state.Events[i]))
		}
		for i := len(state.Messages) - 1; i >= 0 && len(out.Messages) < 200; i-- {
			m := state.Messages[i]
			out.Messages = append(out.Messages, &pb.MissionMessage{Id: m.Message, Seq: m.Seq, AtUnixMillis: millis(m.At), FromConversationId: m.From, ToConversationId: m.To, FromLabel: m.FromLabel, ToLabel: m.ToLabel, Kind: m.Kind, Text: m.Text})
		}
	}
	return out
}

func wireAttempt(a *work.Attempt, live work.Liveness) *pb.MissionAttempt {
	out := &pb.MissionAttempt{Id: a.ID, ConversationId: a.Conversation, SessionId: a.Execution, CollectionId: a.Workspace, StartedAtUnixMillis: millis(a.Started), Outcome: a.Outcome, Live: live.State, LiveDetail: live.Detail, Title: live.Title,
		ParentConversationId: a.Parent, GrantId: a.Grant, Role: a.Role, Permission: a.Permission, ProfileId: a.Profile}
	if live.Session != "" {
		out.SessionId = live.Session
	}
	if a.Dispatch != nil && a.Started.IsZero() {
		out.Live = "reserved"
		out.LiveDetail = "waiting for the worker brief"
	}
	if a.Ended {
		out.EndedAtUnixMillis = millis(a.EndedAt)
		out.Live = work.LiveClosed
	}
	return out
}

func wireEvidence(r work.Recorded) *pb.MissionEvidence {
	out := &pb.MissionEvidence{ContractDigest: r.ContractDigest, Revisions: r.Revisions, Ref: r.Ref, Kind: r.Kind, Title: r.Title, Outcome: r.Outcome, Piped: r.Piped, Member: r.Member, Repository: r.Repository, Head: r.Head, Number: r.Number, Url: r.URL, State: r.State, CollectionId: r.Workspace, SessionId: r.Execution, CommandId: r.Command, OutputPath: r.OutputPath, Detail: r.Detail, AtUnixMillis: millis(r.At)}
	if r.ExitCode != nil {
		code := int32(*r.ExitCode)
		out.ExitCode = &code
	}
	return out
}

func wireMissionEvent(state *work.State, e work.Event) *pb.MissionEvent {
	out := &pb.MissionEvent{Seq: e.Seq, AtUnixMillis: millis(e.At), Type: e.Type, Version: uint32(e.V), Subject: e.Subject, ActorKind: e.Actor.Kind, ActorRole: e.Actor.Role, ConversationId: e.Actor.Conversation, Known: work.Known(e), Evidence: e.Evidence, Id: e.ID, CausedBy: e.CausedBy}
	out.Summary = eventSummary(state, e)
	if !out.Known {
		out.RawJson = string(e.Payload)
	}
	return out
}

// eventSummary is one line a person reads in the mission's history.
func eventSummary(state *work.State, e work.Event) string {
	var p struct {
		Title, Brief, Summary, Note, Reason, Gate, Outcome, Kind, Preset string
		Task, Conversation, Ref, Repository                              string
		ExitCode                                                         *int `json:"exit_code"`
		Number                                                           int64
		Head                                                             string
	}
	_ = json.Unmarshal(e.Payload, &p)
	task := ""
	if t := state.Task(p.Task); t != nil {
		task = "“" + t.Title + "”"
	}
	with := func(text string) string {
		if text = strings.TrimSpace(text); text != "" {
			return ": " + text
		}
		return ""
	}
	switch e.Type {
	case work.MissionCreated:
		return "Mission created" + with(p.Title)
	case work.MissionProcessPinned:
		return "Runs the " + p.Preset + " process"
	case work.MissionBriefChanged:
		return "Mission brief changed"
	case work.MissionCanceled:
		return "Mission canceled" + with(p.Reason)
	case work.GoalCreated:
		return "Goal added" + with(p.Title)
	case work.TaskCreated:
		return "Task added" + with(p.Title)
	case work.TaskBriefChanged:
		return "Brief of " + task + " changed"
	case work.TaskEdgeAdded:
		return "Dependency added to " + task
	case work.TaskCanceled:
		return task + " canceled" + with(p.Reason)
	case work.TaskAssigned:
		if e.Actor.Kind == work.ActorAgent {
			return task + " started by an orchestrator"
		}
		return task + " assigned to a conversation"
	case work.AttemptStarted:
		return "Attempt on " + task + " started"
	case work.AttemptEnded:
		return "Attempt on " + task + " ended (" + p.Outcome + ")"
	case work.TaskReported:
		return task + " reported done" + with(p.Summary)
	case work.EvidenceRecorded:
		switch p.Kind {
		case work.EvidenceRun:
			if p.ExitCode != nil {
				return fmt.Sprintf("Ran %s · exit %d", p.Title, *p.ExitCode)
			}
			return "Piped " + p.Title
		case work.EvidencePR:
			if p.Ref == work.PullRequestLinkRef(p.Repository, p.Number) {
				return fmt.Sprintf("Linked %s#%d to this task", p.Repository, p.Number)
			}
			return fmt.Sprintf("Pull request #%d · checks %s", p.Number, p.Outcome)
		case work.EvidenceCommit:
			return "Committed " + p.Title
		}
		return "Evidence recorded"
	case work.GateRequested:
		return "Waiting for you to accept " + task
	case work.GatePassed:
		return "You accepted " + task + with(p.Note)
	case work.GateRejected:
		return "You asked for changes on " + task + with(p.Note)
	case work.GrantIssued:
		var g work.GrantPayload
		_ = json.Unmarshal(e.Payload, &g)
		who := "“" + firstNonEmpty(g.Title, "a conversation") + "”"
		if g.Parent != "" {
			return "An orchestrator let " + who + " run agents (narrowed)"
		}
		return fmt.Sprintf("You let %s run agents: up to %d at once, %s", who, g.Ceiling.MaxChildren, g.Ceiling.Permission)
	case work.GrantRevoked:
		return "Orchestrator grant revoked" + with(p.Reason)
	case work.PlanProposed:
		var p work.PlanPayload
		_ = json.Unmarshal(e.Payload, &p)
		return fmt.Sprintf("An orchestrator proposed a plan of %d tasks", len(p.Tasks))
	case work.PlanApproved:
		return "You approved a plan" + capSuffix(e)
	case work.PlanRejected:
		return "You rejected a plan" + with(p.Note)
	case work.BudgetRaised:
		return "You set the tree's budget" + capSuffix(e)
	case work.BudgetWarning:
		return "The tree used 80% of its budget" + capSuffix(e)
	case work.BudgetExhausted:
		return "The tree reached its budget; new agents paused" + capSuffix(e)
	case work.LeaseClaimed:
		var l work.LeasePayload
		_ = json.Unmarshal(e.Payload, &l)
		return fmt.Sprintf("%s leased %s (%s)", task, l.Path, l.Mode)
	case work.LeaseReleased:
		return "A lease was released"
	case work.OverlapDetected:
		var o work.OverlapPayload
		_ = json.Unmarshal(e.Payload, &o)
		names := []string{}
		for _, id := range o.Tasks {
			if t := state.Task(id); t != nil {
				names = append(names, "“"+t.Title+"”")
			}
		}
		return "Overlap: " + strings.Join(names, " and ") + " both changed " + strings.Join(o.Paths, ", ")
	case work.WIPHeld:
		return "New agents held: work waits for your review"
	case work.MessageSent:
		var m work.MessagePayload
		_ = json.Unmarshal(e.Payload, &m)
		if m.Kind == "report" {
			return "“" + m.FromLabel + "” reported to “" + m.ToLabel + "”"
		}
		return "“" + m.FromLabel + "” → “" + m.ToLabel + "”: " + strings.Join(strings.Fields(m.Text), " ")
	}
	return e.Type + " (not understood by this Runner; kept as recorded)"
}
