package transport

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agentshell"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/attachments"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/checkpoints"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/version"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type Service struct {
	Inspector gitinspect.Inspector
	pb.UnimplementedRunnerServiceServer
	Bench       *runner.Bench
	assigning   sync.Map // mission/task -> channel closed when the assignment finishes
	prRefreshed sync.Map // workspace/delivered PR -> last mission refresh time
	// Restart is the launcher that starts this Runner again after RestartRunner;
	// nil when nothing would (a plain `codefly run`, a test, a Codefly service).
	Restart Restarter
	// Devices are the phones paired with this Runner, and Remote how they
	// reach it (REMOTE.md); nil where the Runner pairs none.
	Devices *devices.Manager
	Remote  RemoteAccess
	// Push sends paired phones their notifications (push.go); nil where the
	// Runner sends none.
	Push PushNotifier
}

// Restarter records a restart request and begins the Runner's ordinary
// shutdown (restart.Supervisor).
type Restarter interface{ Request() error }

// instance names this Runner process. A client that asked for a restart knows
// the new process serves once Info reports another one.
var instance = commands.ID()

// RestartRunner is the owner's alone (it is not on the agent allowlist). The
// Runner stops exactly as on any restart; its launcher starts it again.
func (s *Service) RestartRunner(context.Context, *pb.RestartRunnerRequest) (*pb.RestartRunnerResponse, error) {
	if s.Restart == nil {
		return nil, status.Error(codes.FailedPrecondition, "this Runner was not started by a launcher that can start it again; restart it where it runs")
	}
	if err := s.Restart.Request(); err != nil {
		return nil, status.Error(codes.Internal, err.Error())
	}
	return &pb.RestartRunnerResponse{InstanceId: instance}, nil
}

func rpcError(err error) error {
	if err == nil {
		return nil
	}
	code := codes.FailedPrecondition
	if errors.Is(err, collections.ErrBranchExists) {
		code = codes.AlreadyExists
	}
	if errors.Is(err, os.ErrNotExist) {
		code = codes.NotFound
	}
	if errors.Is(err, journal.ErrCursor) {
		code = codes.OutOfRange
	}
	if errors.Is(err, context.Canceled) {
		code = codes.Canceled
	}
	if errors.Is(err, context.DeadlineExceeded) {
		code = codes.DeadlineExceeded
	}
	if errors.Is(err, journal.ErrFull) || errors.Is(err, runner.ErrRepositoryRequestLimit) || errors.Is(err, runner.ErrConversationStartLimit) {
		code = codes.ResourceExhausted
	}
	if errors.Is(err, runner.ErrInvalidRepositoryRequest) || errors.Is(err, runner.ErrInvalidConversationStart) || errors.Is(err, agent.ErrInvalidModel) {
		code = codes.InvalidArgument
	}
	return grpcError(code, err)
}

func wireAccount(value *agent.AccountObservation) *pb.AccountObservation {
	if value == nil {
		return nil
	}
	result := &pb.AccountObservation{State: value.State, Reason: value.Reason, ResetAt: value.ResetAt, RetryAfterSeconds: value.RetryAfterSeconds, Plan: value.Plan, ObservedAtUnixMillis: value.ObservedAt, Email: value.Email}
	for _, window := range value.Windows {
		result.Windows = append(result.Windows, &pb.UsageWindow{Name: window.Name, UsedPercent: window.UsedPercent, ResetAt: window.ResetAt, DurationMinutes: window.DurationMinutes})
	}
	return result
}
func wireEvent(e agent.Event) *pb.AgentEvent {
	return &pb.AgentEvent{Type: e.Type, SessionReference: e.SessionID, TurnId: e.TurnID, RequestId: e.RequestID, OperationId: e.OperationID, Text: e.Text, Status: e.Status, ProviderDataJson: e.Data, AgentId: e.AgentID, ParentAgentId: e.ParentAgentID, Name: e.Name, AgentKind: e.AgentKind, NativeId: e.NativeID, Account: wireAccount(e.Account)}
}
func wireSession(e *runner.Execution) *pb.Session {
	s := e.Engine.Snapshot()
	c := s.Capabilities
	w := &pb.Session{Id: s.ID, ProfileId: s.Profile, ProviderReference: s.Reference, GatewayId: s.GatewayID, Busy: s.Busy, Closed: s.Closed, Fault: s.Fault, Queued: uint32(s.Queued), WorkId: e.WorkID, PreviousExecutionId: e.Previous, CollectionId: e.CollectionID, WorkingDirectory: e.Cwd, Purpose: e.Purpose, Status: string(s.Status), Account: wireAccount(s.Account), ActivityTruncated: s.ActivityTruncated, RotationProfileIds: e.Options.RotationProfiles, Capabilities: &pb.Capabilities{Images: c.Images, Files: c.Files, Questions: c.Questions, Approvals: c.Approvals, Steer: c.Steer, Resume: c.Resume, PendingResume: c.PendingResume, ProfileIsolation: c.ProfileIsolation, Subagents: c.Subagents, LivePermission: c.LivePermission, StopAgent: c.StopAgent, LiveModel: c.LiveModel}}
	w.ConversationId = e.Saved.ConversationID
	w.ForkedFromSessionId, w.ForkedFromCursor = e.Saved.ForkedFrom, e.Saved.ForkCursor
	w.Permission, w.Workflow = e.Options.Permission, e.Options.Workflow
	w.BehaviorProfile = e.Options.BehaviorProfile
	w.Effort = e.Options.Effort
	w.Title = s.Title
	if w.Title == "" {
		w.Title = e.Saved.Title
	}
	w.Historical = e.Archived
	// A conversation whose agent's exit could not be confirmed resumes once
	// the person confirms it is stopped, as one that needs recovery does.
	w.RecoveryRequired = e.RecoveryRequired || (s.Closed && s.Fault == "native_exit_unconfirmed")
	w.ResumeBlocked = e.Saved.ResumeBlocked
	w.CreatedAtUnixMillis = e.Saved.CreatedAt.UnixMilli()
	for _, a := range engine.SortedActivities(s.Agents) {
		w.Agents = append(w.Agents, wireActivity(a))
	}
	w.LastMessage = s.LastMessage
	for _, q := range s.QueuedMessages {
		m := &pb.QueuedMessage{OperationId: q.OperationID, Text: q.Text}
		if o := q.Origin; o != nil {
			m.Origin = &pb.MessageOrigin{Kind: o.Kind, Summary: o.Summary, Detail: o.Detail, Copy: o.Copy, From: o.From}
		}
		if !q.QueuedAt.IsZero() {
			m.QueuedAtUnixMillis = q.QueuedAt.UnixMilli()
		}
		w.QueuedMessages = append(w.QueuedMessages, m)
	}
	for _, p := range s.Pending {
		w.Pending = append(w.Pending, wireEvent(p))
	}
	for _, g := range s.Grants {
		grant := &pb.PermissionGrant{Id: g.ID, Label: g.Label, Tool: g.Tool}
		if !g.GrantedAt.IsZero() {
			grant.GrantedAtUnixMillis = g.GrantedAt.UnixMilli()
		}
		w.PermissionGrants = append(w.PermissionGrants, grant)
	}
	return w
}
func wireActivity(a agent.Activity) *pb.AgentActivity {
	w := &pb.AgentActivity{Id: a.ID, ParentId: a.ParentID, NativeId: a.NativeID, Name: a.Name, Kind: a.Kind, Status: string(a.Status), LastTurnStatus: a.LastTurnStatus, UpdatedAtUnixMillis: a.UpdatedAt.UnixMilli()}
	if !a.TurnEndedAt.IsZero() {
		w.TurnEndedAtUnixMillis = a.TurnEndedAt.UnixMilli()
	}
	if !a.TurnStartedAt.IsZero() {
		w.TurnStartedAtUnixMillis = a.TurnStartedAt.UnixMilli()
	}
	w.Phase, w.Tool = a.Phase, a.Tool
	if !a.PhaseSince.IsZero() && a.Phase != "" {
		w.PhaseSinceUnixMillis = a.PhaseSince.UnixMilli()
	}
	return w
}
func wireStatus(s commands.Status) *pb.CommandStatus {
	w := &pb.CommandStatus{Id: s.ID, SessionId: s.Spec.SessionID, CollectionId: s.Spec.CollectionID, ShareWith: s.Spec.ShareWith, Title: s.Spec.Title, Argv: s.Spec.Argv, Cwd: s.Spec.Cwd, Mode: s.Mode, Offset: s.Offset, Done: s.Done, Signal: s.Signal, CaptureError: s.Error,
		Background: s.Background && !s.Done, CanBackground: s.CanBackground(), StartedAtUnixMillis: s.StartedAt, Pid: int32(s.PID)}
	if !s.Done {
		w.Ports = s.Ports
	}
	if s.Spec.TerminalID != "" {
		w.Origin = "terminal"
	}
	if s.Spec.Kind != "" {
		// A workspace action, setup or archive (WORKSPACE_RUNTIME.md).
		w.Origin = s.Spec.Kind
	}
	if s.ExitCode != nil {
		code := int32(*s.ExitCode)
		w.ExitCode = &code
	}
	if r := s.Report; r != nil {
		w.Report = &pb.CommandReport{ExitCode: int32(r.ExitCode), OutputBytes: r.OutputBytes, AgentBytes: r.AgentBytes, Filter: r.Filter, OutputPath: r.OutputPath, AgentView: r.AgentView, Piped: r.Piped}
	}
	return w
}

// listedStatus is a command in a listing, which the console polls: the
// compact view is left out (a console reads it from the run's own status).
func listedStatus(s commands.Status) *pb.CommandStatus {
	w := wireStatus(s)
	if w.Report != nil {
		w.Report.AgentView = ""
	}
	return w
}

func (s *Service) Info(ctx context.Context, r *pb.InfoRequest) (*pb.InfoResponse, error) {
	if err := s.Bench.ReloadProfiles(); err != nil {
		return nil, rpcError(err)
	}
	w := &pb.InfoResponse{InstanceId: instance, RestartAvailable: s.Restart != nil, SharedSubscriptions: s.Bench.Config.SubscriptionState != s.Bench.Config.State, DefaultModel: s.Bench.Config.Defaults.Model, DefaultPermission: s.Bench.Config.Defaults.Permission, AttachmentLimits: &pb.AttachmentLimits{MaxFiles: attachments.MaxFiles, MaxFileBytes: attachments.MaxFileBytes, MaxMessageBytes: attachments.MaxMessageBytes}}
	if s.Bench.History != nil {
		id, err := s.Bench.History.Identity(ctx)
		if err != nil {
			return nil, rpcError(err)
		}
		w.RunnerId = id
	}
	for _, p := range s.Bench.Profiles() {
		w.Profiles = append(w.Profiles, s.wireProfile(p))
	}
	for _, flow := range s.Bench.PendingEnrollments() {
		w.PendingEnrollments = append(w.PendingEnrollments, wireEnrollment(flow))
	}
	for _, g := range s.Bench.Config.Gateways {
		w.Gateways = append(w.Gateways, &pb.Gateway{Id: g.ID, Provider: g.Provider})
	}
	for _, provider := range []string{"claude", "codex"} {
		w.Gateways = append(w.Gateways, &pb.Gateway{Id: "metered", Provider: provider})
	}
	hidden := s.hiddenConversations()
	for _, e := range s.Bench.List() {
		if hidden[e.ID] {
			continue
		}
		record, err := s.sessionView(ctx, e)
		if err != nil {
			return nil, err
		}
		if hidden[record.ConversationId] {
			continue
		}
		w.Sessions = append(w.Sessions, record)
	}
	if callerDevice(ctx) != nil {
		return deviceInfo(w), nil
	}
	return w, nil
}
func (s *Service) OpenSession(ctx context.Context, r *pb.OpenSessionRequest) (*pb.SessionResponse, error) {
	p := r
	if fork := p.Fork; fork != nil {
		if p.ProfileId != "" || p.Cwd != "" || p.ResumeReference != "" || p.CollectionId != "" || p.Options != nil || p.EmptyWorkspace {
			return nil, grpcError(codes.InvalidArgument, errors.New("fork settings are resolved from the source conversation"))
		}
		e, err := s.Bench.ForkConversation(ctx, fork.SourceSessionId, fork.GetAfterTurnId(), fork.GetBeforePromptCursor(), fork.CheckpointFiles, fork.RequestKey)
		if err != nil {
			return nil, rpcError(err)
		}
		return s.sessionResponse(ctx, e)
	}
	options := accounts.Options{}
	gateway := ""
	if p.Options != nil {
		options.Model, options.Effort = p.Options.Model, p.Options.Effort
		options.Instructions = p.Options.Instructions
		options.BehaviorProfile = p.Options.BehaviorProfile
		options.Workflow = p.Options.Workflow
		options.Permission = p.Options.Permission
		options.RotationProfiles = p.Options.RotationProfileIds
		options.NoRotation = p.Options.NoRotation
		gateway = p.Options.GatewayId
	}
	if p.EmptyWorkspace && (p.CollectionId != "" || p.Cwd != "" || p.ResumeReference != "") {
		return nil, grpcError(codes.InvalidArgument, errors.New("empty workspace cannot specify cwd, collection or resume"))
	}
	if p.CollectionId != "" && p.Cwd != "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("select collection_id or cwd, not both"))
	}
	var e *runner.Execution
	var err error
	if p.EmptyWorkspace {
		e, err = s.Bench.OpenEmptyWorkspace(p.ProfileId, options, gateway)
	} else if p.CollectionId != "" {
		e, err = s.Bench.OpenCollection(p.ProfileId, p.CollectionId, p.ResumeReference, options, gateway, "")
	} else {
		e, err = s.Bench.Open(p.ProfileId, p.Cwd, p.ResumeReference, options, gateway, "", "")
	}
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}
func (s *Service) OpenAssistant(ctx context.Context, p *pb.OpenAssistantRequest) (*pb.SessionResponse, error) {
	options := accounts.Options{}
	gateway := ""
	if p.Options != nil {
		options.Model, options.Effort = p.Options.Model, p.Options.Effort
		options.Instructions = p.Options.Instructions
		options.RotationProfiles = p.Options.RotationProfileIds
		options.NoRotation = p.Options.NoRotation
		gateway = p.Options.GatewayId
	}
	e, err := s.Bench.OpenAssistant(p.ProfileId, p.Purpose, p.Context, options, gateway, p.CollectionId)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}
func (s *Service) GetSession(ctx context.Context, r *pb.SessionRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}
func (s *Service) CloseSession(ctx context.Context, r *pb.SessionRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = e.Engine.Close(); err != nil {
		return nil, rpcError(err)
	}
	s.Bench.ReviewerEndedByHand(s.Bench.ReviewerRuns(r.SessionId), reviews.EndedSession)
	return s.sessionResponse(ctx, e)
}

func (s *Service) Apply(ctx context.Context, r *pb.ApplyRequest) (*pb.Receipt, error) {
	return s.apply(ctx, r, nil)
}

// apply is Apply, for the Runner as well: origin marks a message Lazybox
// wrote (a mission's brief), which no client can claim.
func (s *Service) apply(ctx context.Context, r *pb.ApplyRequest, origin *agent.Origin) (*pb.Receipt, error) {
	p := r
	e, err := s.Bench.Get(p.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	op := agent.Operation{ID: p.OperationId}
	if _, isMessage := p.Operation.(*pb.ApplyRequest_Message); isMessage {
		op.Origin = origin
	}
	message := func(m *pb.Message) error {
		if m == nil {
			return errors.New("message required")
		}
		state := e.Engine.Snapshot()
		// At a subscription limit a message waits in the queue, and after a
		// provider error ended a turn the next message continues the
		// conversation (see engine.Apply, which makes the same decision).
		if state.Closed || (state.Fault != "" && !agent.LimitFault(state.Fault) && !agent.TurnFault(state.Fault)) {
			return errors.New("session is closed or faulted")
		}
		if len(m.Attachments) > 0 && !state.Capabilities.Files {
			return agent.ErrUnsupported
		}
		if op.ID == "" || len(op.ID) > 128 {
			return errors.New("operation id required (max 128 bytes)")
		}
		op.Text = m.Text
		payloadBytes := len(m.Text)
		for _, img := range m.Images {
			payloadBytes += len(img.Base64Data)
			op.Images = append(op.Images, agent.Image{MediaType: img.MediaType, Data: img.Base64Data})
		}
		inputs := make([]attachments.Input, 0, len(m.Attachments))
		for _, file := range m.Attachments {
			payloadBytes += base64.StdEncoding.EncodedLen(len(file.Data))
			inputs = append(inputs, attachments.Input{Name: file.Name, MediaType: file.MediaType, Data: file.Data})
		}
		if payloadBytes > 34<<20 {
			return errors.New("message exceeds the combined attachment/image limit")
		}
		var err error
		op.Files, err = s.Bench.Attachments.Put(e.ID, inputs)
		if err != nil {
			return err
		}
		// How each file reaches the agent, the same rule for every provider
		// (agent.Delivery): an image it can see as an image, text as its text,
		// anything else by reference to the retained file.
		for i, file := range op.Files {
			media := http.DetectContentType(inputs[i].Data)
			image := state.Capabilities.Images && len(op.Images) < 4 && file.Size <= 6<<20 && media == file.MediaType && attachments.Previewable(media)
			if image {
				op.Images = append(op.Images, agent.Image{MediaType: media, Data: base64.StdEncoding.EncodeToString(inputs[i].Data)})
			}
			op.Files[i].Delivery = agent.Delivery(inputs[i].Data, image)
		}
		return nil
	}
	var briefing runner.BriefingUpdate
	reverted, orchestration := "", ""
	// A message or steer the agent accepts goes in the prompt log.
	var prompt *pb.Message
	switch o := p.Operation.(type) {
	case *pb.ApplyRequest_Message:
		op.Kind = agent.Message
		if err := message(o.Message); err != nil {
			return nil, rpcError(err)
		}
		prompt = o.Message
		// What changed in the agent's briefing travels with the next message
		// that starts a turn, and so does a revert of the workspace's files
		// since the last one.
		briefing = s.Bench.BriefingForMessage(e)
		reverted = s.Bench.CodeRevertNote(e)
		orchestration = s.Bench.OrchestrationNote(e)
		op.Context = strings.TrimSpace(briefing.Text + "\n\n" + reverted + "\n\n" + orchestration)
	case *pb.ApplyRequest_Permission:
		receipt, err := s.Bench.SetPermission(ctx, e, p.OperationId, o.Permission.GetMode())
		if err != nil {
			return nil, rpcError(err)
		}
		return &pb.Receipt{OperationId: receipt.ID, Delivery: receipt.Delivery, ProviderId: receipt.ProviderID, Error: receipt.Error}, nil
	case *pb.ApplyRequest_Model:
		receipt, err := s.Bench.SetModelAndEffort(ctx, e, p.OperationId, o.Model.GetModel(), o.Model.GetEffort())
		if err != nil {
			return nil, rpcError(err)
		}
		return &pb.Receipt{OperationId: receipt.ID, Delivery: receipt.Delivery, ProviderId: receipt.ProviderID, Error: receipt.Error}, nil
	case *pb.ApplyRequest_Steer:
		op.Kind = agent.Steer
		if err := message(o.Steer); err != nil {
			return nil, rpcError(err)
		}
		prompt = o.Steer
	case *pb.ApplyRequest_Interrupt:
		op.Kind = agent.Interrupt
		// Stop on a reviewer mid-review stops its review, recorded before the
		// turn ends so the turn it cuts short is not taken for a round's end.
		if snap := e.Engine.Snapshot(); snap.Busy && !snap.Closed {
			s.Bench.ReviewerEndedByHand(s.Bench.ReviewerRuns(e.ID), reviews.EndedInterrupted)
		}
	case *pb.ApplyRequest_DeliverQueued:
		// A message already accepted (and in the prompt log): only its turn changes.
		queue, ok := e.Engine.(queueRuntime)
		if !ok {
			return nil, rpcError(agent.ErrUnsupported)
		}
		var foreground []*commands.Command
		if o.DeliverQueued.GetHow() == engine.DeliverNow && e.Engine.Snapshot().Busy {
			for _, candidate := range s.Bench.Commands.List(e.ID) {
				if candidate.CanBackground() {
					if command, err := s.Bench.Commands.Get(candidate.ID); err == nil {
						foreground = append(foreground, command)
					}
				}
			}
		}
		receipt, err := queue.DeliverQueued(ctx, o.DeliverQueued.GetOperationId(), o.DeliverQueued.GetHow())
		if errors.Is(err, engine.ErrNotQueued) {
			return nil, grpcError(codes.FailedPrecondition, err)
		}
		if err != nil {
			return nil, rpcError(err)
		}
		// A followed run is owned by its pipe broker and can outlive the
		// provider's turn interrupt. Ask that broker to stop it as well.
		for _, command := range foreground {
			command.CancelForegroundPipe()
		}
		return &pb.Receipt{OperationId: receipt.ID, Delivery: receipt.Delivery, ProviderId: receipt.ProviderID, Error: receipt.Error}, nil
	case *pb.ApplyRequest_ListProviderCatalog:
		op.Kind, op.Section = agent.ListProviderCatalog, o.ListProviderCatalog.GetSection()
	case *pb.ApplyRequest_CompactContext:
		op.Kind = agent.CompactContext
	case *pb.ApplyRequest_ListMcpServers:
		op.Kind = agent.ListMcpServers
	case *pb.ApplyRequest_StopAgent:
		op.Kind = agent.StopAgent
		op.AgentID = o.StopAgent.GetAgentId()
	case *pb.ApplyRequest_RevokeGrant:
		op.Kind = agent.RevokeGrant
		op.GrantID = o.RevokeGrant.GetGrantId()
	case *pb.ApplyRequest_Answer:
		op.Kind = agent.Answer
		op.RequestID = o.Answer.RequestId
		op.Answers = map[string][]string{}
		for k, v := range o.Answer.Answers {
			op.Answers[k] = v.Values
		}
	case *pb.ApplyRequest_Approval:
		op.Kind = agent.Approve
		op.RequestID = o.Approval.RequestId
		op.Always, op.Reason = o.Approval.Always, o.Approval.Reason
		switch o.Approval.Decision {
		case pb.Decision_DECISION_ACCEPT:
			op.Decision = "accept"
		case pb.Decision_DECISION_DECLINE:
			op.Decision = "decline"
		case pb.Decision_DECISION_CANCEL:
			op.Decision = "cancel"
		}
	default:
		return nil, grpcError(codes.InvalidArgument, errors.New("typed operation required"))
	}
	receipt, err := e.Engine.Apply(ctx, op)
	if err != nil {
		return nil, rpcError(err)
	}
	if prompt != nil && receipt.Error == "" {
		s.Bench.RecordPrompt(e, p.OperationId, prompt.Text, promptSource(prompt.Source))
	}
	if briefing.Text != "" && receipt.Error == "" {
		// The message, and the update with it, is accepted. If recording that
		// fails, the update is sent again with the next message: harmless.
		_ = s.Bench.BriefingDelivered(ctx, e, briefing, p.OperationId)
	}
	if reverted != "" {
		_ = s.Bench.CodeRevertNoteDelivered(e, reverted)
	}
	if orchestration != "" {
		s.Bench.OrchestrationNoteDelivered(e, orchestration)
	}
	return &pb.Receipt{OperationId: receipt.ID, Delivery: receipt.Delivery, ProviderId: receipt.ProviderID, Error: receipt.Error}, nil
}

// queueRuntime is a live session whose queued messages can be delivered early.
type queueRuntime interface {
	DeliverQueued(ctx context.Context, operationID, how string) (agent.Receipt, error)
}

func (s *Service) WatchEvents(r *pb.WatchEventsRequest, stream grpc.ServerStreamingServer[pb.JournalEvent]) error {
	ctx := stream.Context()
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return rpcError(err)
	}
	cursor := r.After
	// The lineage is resolved once for the stream: a replay of thousands of
	// records does not look the conversation up again for each.
	var timeline *runner.Timeline
	if r.IncludePrevious {
		if timeline, err = s.Bench.OpenTimeline(e.ID); err != nil {
			return rpcError(err)
		}
	}
	// Each record says how far the history went when it was read, so a reader
	// can tell replay from live records: it renders a long conversation once,
	// when it has caught up, instead of every step of the replay.
	var retained uint64
	head := func() uint64 {
		if timeline != nil {
			return timeline.Retained()
		}
		return e.Retained()
	}
	// A reader that asked for heartbeats hears one as soon as it has every
	// record there was, then one per quiet interval: an idle conversation's
	// stream says nothing else, and its reader could not tell it from one
	// that never connected.
	caughtUp := !r.Heartbeats
	beat := func() error {
		return stream.Send(&pb.JournalEvent{Cursor: cursor, Retained: max(retained, cursor), Heartbeat: true})
	}
	if !caughtUp && cursor >= head() {
		if err = beat(); err != nil {
			return err
		}
		caughtUp = true
	}
	for {
		var record journal.Record
		read, cancel := ctx, context.CancelFunc(func() {})
		if r.Heartbeats {
			read, cancel = context.WithTimeout(ctx, watchHeartbeat)
		}
		if timeline != nil {
			record, err = timeline.Next(read, cursor)
		} else {
			record, err = e.Engine.Events(read, cursor)
		}
		cancel()
		if r.Heartbeats && errors.Is(err, context.DeadlineExceeded) && ctx.Err() == nil {
			if err = beat(); err != nil {
				return err
			}
			caughtUp = true
			continue
		}
		if err == nil && record.Sequence >= retained {
			if timeline != nil {
				retained = timeline.Retained()
			} else {
				retained = e.Retained()
			}
		}
		// A Runner going away closes every journal under its readers. Its live
		// conversations go on in the next Runner, so that end (or a read the
		// shutdown broke) says "reconnect", never "the conversation is over":
		// a reader told the history ended stops following it for good.
		if err != nil && ctx.Err() == nil && s.Bench.Stopping() {
			return grpcError(codes.Unavailable, fmt.Errorf("the Runner is restarting; reconnect after cursor %d", cursor))
		}
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return rpcError(err)
		}
		w, err := journalEvent(record, retained)
		if err != nil {
			return rpcError(err)
		}
		if err = stream.Send(w); err != nil {
			return err
		}
		cursor = record.Sequence
		if !caughtUp && cursor >= retained {
			if err = beat(); err != nil {
				return err
			}
			caughtUp = true
		}
	}
}

// watchHeartbeat is how long a WatchEvents stream that asked for heartbeats
// stays quiet before it sends one.
var watchHeartbeat = 15 * time.Second

// journalEvent is a journal record as clients read it.
func journalEvent(record journal.Record, retained uint64) (*pb.JournalEvent, error) {
	var ev agent.Event
	var envelope struct {
		Receipt *agent.Receipt `json:"receipt"`
	}
	if err := json.Unmarshal(record.Value, &ev); err != nil {
		return nil, err
	}
	_ = json.Unmarshal(record.Value, &envelope)
	w := &pb.JournalEvent{Cursor: record.Sequence, CapturedAt: record.Time.Format(time.RFC3339Nano), Event: wireEvent(ev), Retained: max(retained, record.Sequence)}
	if envelope.Receipt != nil {
		v := envelope.Receipt
		w.Receipt = &pb.Receipt{OperationId: v.ID, Delivery: v.Delivery, ProviderId: v.ProviderID, Error: v.Error}
	}
	if ev.Type == "session_opened" || ev.Type == "execution_context" || ev.Type == "continuation_requested" {
		w.MetadataJson = record.Value
	}
	return w, nil
}

// Turn-aligned pages are bounded to about a mebibyte of journal unless the
// reader asks for less; a reader asking for more gets at most 8 MiB.
const tailBytes, maxTailBytes = 1 << 20, 8 << 20

func (s *Service) ReadSessionTail(ctx context.Context, r *pb.ReadSessionTailRequest) (*pb.ReadSessionTailResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	timeline, err := s.Bench.OpenTimeline(e.ID)
	if err != nil {
		return nil, rpcError(err)
	}
	budget := int64(r.MaxBytes)
	if budget == 0 {
		budget = tailBytes
	}
	records, start, retained, err := timeline.Page(r.Before, int(r.Turns), min(budget, maxTailBytes))
	if err != nil {
		return nil, rpcError(err)
	}
	page := &pb.ReadSessionTailResponse{Start: start, Retained: retained, Events: make([]*pb.JournalEvent, 0, len(records))}
	for _, record := range records {
		w, err := journalEvent(record, retained)
		if err != nil {
			return nil, rpcError(err)
		}
		page.Events = append(page.Events, w)
	}
	return page, nil
}

func (s *Service) ContinueSession(ctx context.Context, r *pb.ContinueSessionRequest) (*pb.SessionResponse, error) {
	p := r
	e, err := s.Bench.Get(p.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	if agent.IsAssistant(e.Purpose) {
		return nil, rpcError(errors.New("assistant conversations use follow-up messages; open a new assistant for a separate conversation"))
	}
	e.Continuation.Lock()
	defer e.Continuation.Unlock()
	state := e.Engine.Snapshot()
	if state.Closed || state.Busy || len(state.Pending) > 0 {
		return nil, rpcError(errors.New("quiesce the current turn and reconcile all pending requests before continuation"))
	}
	for _, command := range s.Bench.Commands.List(e.ID) {
		if !command.Done {
			return nil, rpcError(errors.New("finish or cancel active commands before continuation"))
		}
	}
	if p.Reason == "" {
		return nil, rpcError(errors.New("explicit continuation reason required"))
	}
	profile := p.ProfileId
	if profile == "" {
		profile = state.Profile
	}
	resume := ""
	prompt := ""
	switch p.Mode {
	case "native":
		if profile != state.Profile || !state.Capabilities.Resume || state.Reference == "" {
			return nil, rpcError(errors.New("native resume is supported only within the same profile; cross-account recovery is unverified"))
		}
		resume = state.Reference
	case "checkpoint":
		if p.Checkpoint == nil {
			return nil, rpcError(errors.New("explicit checkpoint required"))
		}
		if profile != state.Profile {
			if _, err = accounts.Select(s.Bench.Profiles(), []string{profile}, nil, e.Profile.Provider, state.Profile); err != nil {
				return nil, rpcError(err)
			}
		}
		c := p.Checkpoint
		prompt, err = (checkpoints.Checkpoint{Objective: c.Objective, Criteria: c.Criteria, Decisions: c.Decisions, Outstanding: c.Outstanding, Results: c.Results, SourceRevisions: c.SourceRevisions, WorkingTreeState: c.WorkingTreeState, CommandIDs: c.CommandIds, Services: c.ServiceReferences, OmittedContext: c.OmittedContext}).Prompt()
		if err != nil {
			return nil, rpcError(err)
		}
	default:
		return nil, rpcError(errors.New("continuation mode must be native or checkpoint"))
	}
	if err = s.Bench.Record(e, map[string]any{"type": "continuation_requested", "mode": p.Mode, "reason": p.Reason, "profile": profile, "checkpoint": p.Checkpoint}); err != nil {
		return nil, rpcError(err)
	}
	if err = e.Engine.Close(); err != nil {
		return nil, rpcError(err)
	}
	options := e.Options
	var next *runner.Execution
	if e.CollectionID != "" {
		next, err = s.Bench.OpenCollection(profile, e.CollectionID, resume, options, s.Bench.GatewayID(e), e.ID)
	} else {
		next, err = s.Bench.Open(profile, e.Cwd, resume, options, s.Bench.GatewayID(e), e.WorkID, e.ID)
	}
	if err != nil {
		return nil, rpcError(errors.New("continuation failed after closing old adapter; old writer remains closed: " + err.Error()))
	}
	if prompt != "" {
		origin := &agent.Origin{Kind: agent.OriginContinuation, Summary: "Continued from a checkpoint", Detail: p.Reason}
		if _, err = next.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: prompt, Origin: origin}); err != nil {
			return nil, rpcError(err)
		}
	}
	return s.sessionResponse(ctx, next)
}

func (s *Service) StartCommand(ctx context.Context, r *pb.StartCommandRequest) (*pb.CommandStatus, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	c, err := s.Bench.StartForPerson(e, r.Title, r.Argv, r.Cwd)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireStatus(c.Status()), nil
}
func (s *Service) RegisterPipe(ctx context.Context, r *pb.RegisterPipeRequest) (*pb.CommandStatus, error) {
	if r.Check != "" && r.Needs != "" {
		return nil, rpcError(errors.New("a run is either a --check or a --needs of one criterion"))
	}
	if r.CollectionId != "" || r.ShareWith != "" {
		if r.Check != "" || r.Needs != "" {
			return nil, rpcError(errors.New("--check and --needs record a conversation's delivery evidence: run them from its agent"))
		}
		if r.SessionId != "" || r.CollectionId == "" {
			return nil, rpcError(errors.New("a pipe belongs to a session or to a workspace, not both; only a workspace run is shared"))
		}
		c, err := s.Bench.RegisterWorkspacePipe(ctx, r.CollectionId, callerTerminal(ctx), r.Title, r.ShareWith)
		if err != nil {
			return nil, rpcError(err)
		}
		return wireStatus(c.Status()), nil
	}
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	var check *runner.PipeCheck
	if r.Check != "" {
		check = &runner.PipeCheck{Criterion: r.Check}
	} else if r.Needs != "" {
		check = &runner.PipeCheck{Criterion: r.Needs, Prerequisite: true}
	}
	c, err := s.Bench.RegisterPipe(e, r.Title, r.CreatesPullRequest, check)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireStatus(c.Status()), nil
}
func (s *Service) Publish(ctx context.Context, r *pb.PublishRequest) (*pb.CommandStatus, error) {
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = c.Publish(r.Offset, r.Data); err != nil {
		return nil, rpcError(err)
	}
	return wireStatus(c.Status()), nil
}
func (s *Service) FinishPipe(ctx context.Context, r *pb.FinishPipeRequest) (*pb.CommandStatus, error) {
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	var cause error
	if r.CaptureError != "" {
		cause = errors.New(r.CaptureError)
	}
	var report *commands.Report
	if r := r.Report; r != nil {
		report = &commands.Report{ExitCode: int(r.ExitCode), OutputBytes: r.OutputBytes, AgentBytes: r.AgentBytes, Filter: r.Filter, OutputPath: r.OutputPath, AgentView: r.AgentView, Piped: r.Piped}
	}
	if err = s.Bench.FinishPipe(c, cause, r.Signal, report); err != nil {
		return nil, rpcError(err)
	}
	state := c.Status()
	w := wireStatus(state)
	w.Delivery = s.Bench.DeliveryNote(state)
	return w, nil
}

// BindDelivery pins a published spec.v2 revision to the caller's conversation.
func (s *Service) BindDelivery(ctx context.Context, r *pb.BindDeliveryRequest) (*pb.DeliveryBinding, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	b, err := s.Bench.BindDelivery(e, r.ArtifactId, r.Version)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.DeliveryBinding{Conversation: b.Conversation, Attempt: b.Attempt, ArtifactId: b.Contract.Artifact, Version: b.Contract.Version, Digest: b.Contract.Digest, Criteria: b.Contract.Lines()}, nil
}
func (s *Service) ListCommands(ctx context.Context, r *pb.ListCommandsRequest) (*pb.ListCommandsResponse, error) {
	w := &pb.ListCommandsResponse{}
	for _, c := range s.Bench.Commands.List(r.SessionId) {
		w.Commands = append(w.Commands, s.listedCommand(ctx, c))
	}
	// An agent lists only its own commands (`lazybox ls`): the workspace's
	// runs come from the person's terminals.
	if who, _ := ctx.Value(principalKey{}).(principal); who.session != "" && !who.owner {
		return w, nil
	}
	// A conversation's console also shows its workspace's runs.
	if r.SessionId != "" {
		if e, err := s.Bench.Get(r.SessionId); err == nil && e.CollectionID != "" {
			for _, c := range s.Bench.Commands.ListWorkspace(e.CollectionID) {
				w.Commands = append(w.Commands, s.listedCommand(ctx, c))
			}
		}
	}
	return w, nil
}

// listedCommand is a listed command with the ports it listens on now.
func (s *Service) listedCommand(ctx context.Context, c commands.Status) *pb.CommandStatus {
	w := listedStatus(c)
	if !c.Done {
		w.Ports = s.Bench.CommandPorts(ctx, c)
		if c.Mode == "managed" && w.Pid == 0 {
			if command, err := s.Bench.Commands.Get(c.ID); err == nil {
				w.Pid = int32(command.PID())
			}
		}
	}
	return w
}

// FollowPipe is a `lazybox run` following its pipe: it reports its state and
// waits for the Runner's request (stop, move to the background).
func (s *Service) FollowPipe(ctx context.Context, r *pb.FollowPipeRequest) (*pb.FollowPipeResponse, error) {
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	ports := r.Ports
	if len(ports) > 64 {
		ports = ports[:64]
	}
	reply, err := c.Follow(ctx, commands.FollowUpdate{Background: r.Background, PID: int(r.Pid), Ports: ports}, time.Duration(r.WaitMs)*time.Millisecond)
	if errors.Is(err, commands.ErrRestarting) {
		return nil, grpcError(codes.Unavailable, err)
	}
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.FollowPipeResponse{Stop: reply.Stop, Background: reply.Background, Offset: reply.Offset}, nil
}

// BackgroundCommand moves a foreground `lazybox run` to the background.
func (s *Service) BackgroundCommand(ctx context.Context, r *pb.CommandRequest) (*pb.CommandStatus, error) {
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = c.RequestBackground(); err != nil {
		return nil, grpcError(codes.FailedPrecondition, err)
	}
	return wireStatus(c.Status()), nil
}

// ClaimCommandShare hands a shared run to the first message box that asks.
func (s *Service) ClaimCommandShare(ctx context.Context, r *pb.CommandRequest) (*pb.ClaimCommandShareResponse, error) {
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	claimed, err := s.Bench.ClaimCommandShare(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.ClaimCommandShareResponse{Claimed: claimed, Status: wireStatus(c.Status())}, nil
}
func (s *Service) ReadCommand(ctx context.Context, r *pb.ReadCommandRequest) (*pb.ReadCommandResponse, error) {
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	data, status, err := c.Read(ctx, r.Offset, int(r.Limit))
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.ReadCommandResponse{Data: data, Status: wireStatus(status), NextOffset: r.Offset + uint64(len(data))}, nil
}
func (s *Service) WaitCommand(ctx context.Context, r *pb.CommandRequest) (*pb.CommandStatus, error) {
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	status, err := c.Wait(ctx)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireStatus(status), nil
}
func (s *Service) CancelCommand(ctx context.Context, r *pb.CommandRequest) (*pb.CommandStatus, error) {
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = c.Cancel(); err != nil {
		return nil, rpcError(err)
	}
	return wireStatus(c.Status()), nil
}
func (s *Service) WatchOutput(r *pb.WatchOutputRequest, stream grpc.ServerStreamingServer[pb.OutputEvent]) error {
	ctx := stream.Context()
	c, err := s.Bench.Commands.Get(r.CommandId)
	if err != nil {
		return rpcError(err)
	}
	cursor := r.After
	for {
		record, err := c.Next(ctx, cursor)
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return rpcError(err)
		}
		var out commands.Output
		if err = json.Unmarshal(record.Value, &out); err != nil {
			return rpcError(err)
		}
		w := &pb.OutputEvent{Cursor: record.Sequence, CapturedAt: record.Time.Format(time.RFC3339Nano), Type: out.Type, Channel: out.Channel, Offset: out.Offset, Data: out.Data}
		if out.Status != nil {
			w.Status = wireStatus(*out.Status)
		}
		if err = stream.Send(w); err != nil {
			return err
		}
		cursor = record.Sequence
	}
}

func grpcError(code codes.Code, err error) error { return status.Error(code, err.Error()) }

func (s *Service) Version(ctx context.Context, r *pb.VersionRequest) (*pb.VersionResponse, error) {
	check, done := s.Bench.AgentCLI()
	if r.WaitAgentCli {
		wait, cancel := context.WithTimeout(ctx, 30*time.Second)
		select {
		case <-done:
			check, _ = s.Bench.AgentCLI()
		case <-wait.Done():
		}
		cancel()
	}
	return &pb.VersionResponse{Version: version.String(), AgentCli: wireAgentCLI(check)}, nil
}

// wireAgentCLI is which lazybox agents' shells reach, or nil before a probe.
func wireAgentCLI(c *agentshell.Check) *pb.AgentCliCheck {
	if c == nil {
		return nil
	}
	return &pb.AgentCliCheck{Cli: c.CLI, Shell: c.Shell, Shadow: c.Shadow, ShadowIn: c.ShadowIn, Unfixed: c.Unfixed, Fixed: c.Fixed(), Notice: c.Notice(), Problem: c.Problem, CheckedAtUnixMillis: c.CheckedAt.UnixMilli()}
}

func (s *Service) ListHistory(ctx context.Context, r *pb.ListHistoryRequest) (*pb.ListHistoryResponse, error) {
	limit := int(r.Limit)
	if limit == 0 {
		limit = 50
	}
	if limit > 100 {
		return nil, rpcError(errors.New("history page limit is 100"))
	}
	records, err := s.Bench.History.Query(ctx, sessionstore.Query{Limit: limit, Offset: int(r.Offset), Workspace: r.CollectionId, Search: r.Search})
	if err != nil {
		return nil, rpcError(err)
	}
	response := &pb.ListHistoryResponse{HasMore: len(records) == limit}
	hidden := s.hiddenConversations()
	for _, record := range records {
		if hidden[record.ID] || hidden[record.ConversationID] {
			continue
		}
		e, err := s.Bench.Get(record.ID)
		var view *pb.Session
		if err == nil {
			view, err = s.sessionView(ctx, e)
		}
		if ctx.Err() != nil {
			return nil, rpcError(ctx.Err())
		}
		if errors.Is(err, os.ErrNotExist) {
			continue // deleted since the page was read
		}
		if err != nil {
			// One conversation that cannot be read does not hide the rest of
			// the page: it is listed with what the store holds and why.
			log.Printf("history: conversation %s unreadable: %v", record.ID, err)
			view = unreadableSession(record, err)
		}
		response.Sessions = append(response.Sessions, view)
	}
	return response, nil
}

// unreadableSession lists a conversation the Runner could not open from its
// stored record alone, with the reason as its fault.
func unreadableSession(r sessionstore.Record, err error) *pb.Session {
	title := r.DisplayTitle
	if title == "" {
		title = r.Title
	}
	return &pb.Session{Id: r.ID, ConversationId: r.ConversationID, ProfileId: r.ProfileID, Provider: r.Provider, PreviousExecutionId: r.Previous, WorkId: r.WorkID, CollectionId: r.CollectionID, WorkingDirectory: r.Cwd, Purpose: r.Purpose, Title: title, Historical: true, Closed: true, Status: string(agent.Unknown), Fault: "conversation unreadable: " + err.Error(), CreatedAtUnixMillis: r.CreatedAt.UnixMilli(), PinnedAtUnixMillis: r.PinnedAt, ArchivedAtUnixMillis: r.ArchivedAt}
}
func (s *Service) RestartWithPermission(ctx context.Context, r *pb.RestartWithPermissionRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	next, err := s.Bench.RestartWithPermission(ctx, e, r.Mode)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, next)
}
func (s *Service) RestartWithModel(ctx context.Context, r *pb.RestartWithModelRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	next, err := s.Bench.RestartWithModelAndEffort(ctx, e, r.Model, r.Effort)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, next)
}
func (s *Service) ResumeSession(ctx context.Context, r *pb.ResumeSessionRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Resume(ctx, r.SessionId, r.ProcessesStopped)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}

// SwitchSubscription continues a running conversation on another subscription
// of its provider (or retries on its own after a reset), in the same execution.
func (s *Service) SwitchSubscription(ctx context.Context, r *pb.SwitchSubscriptionRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = s.Bench.SwitchSubscription(ctx, e, r.ProfileId, r.EnableRotation); err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}

// SetSessionRotation turns automatic switching at a limit on or off for a
// running conversation, or for every running one of its provider.
func (s *Service) SetSessionRotation(ctx context.Context, r *pb.SetSessionRotationRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = s.Bench.SetSessionRotation(ctx, e, r.Enabled, r.AllSessions); err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}

func (s *Service) sessionView(ctx context.Context, e *runner.Execution) (*pb.Session, error) {
	record, err := s.Bench.History.Get(ctx, e.ID)
	if err != nil {
		return nil, rpcError(err)
	}
	view := wireSession(e)
	view.InstructionsPending = !view.Closed && s.Bench.BriefingUpdate(e).Text != ""
	view.Provider = e.Profile.Provider
	view.Model = e.Options.Model
	if s.Bench.Telemetry != nil {
		model, err := s.Bench.Telemetry.LatestModel(ctx, e.ID)
		if err != nil {
			return nil, rpcError(err)
		}
		if model != "" {
			view.Model = model
			view.ModelReported = true
		}
	}
	if next, err := s.Bench.History.Successor(ctx, e.ID); err == nil {
		view.SuccessorExecutionId = next.ID
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, rpcError(err)
	}
	if e.CollectionID != "" && s.Bench.Collections != nil {
		if workspace, err := s.Bench.Collections.Get(e.CollectionID); err == nil && (workspace.State == "deleted" || workspace.State == "deleting") {
			view.ResumeBlocked = "This workspace was deleted or is being cleaned up. Its conversation remains available as history."
		}
	}
	if record.DisplayTitle != "" {
		view.Title = record.DisplayTitle
	}
	view.PinnedAtUnixMillis = record.PinnedAt
	view.ArchivedAtUnixMillis = record.ArchivedAt
	// Creation order belongs to the conversation, not its latest continuation.
	if record.ConversationID != record.ID {
		root, err := s.Bench.History.Get(ctx, record.ConversationID)
		if err != nil {
			return nil, rpcError(err)
		}
		view.CreatedAtUnixMillis = root.CreatedAt.UnixMilli()
	}
	// An agent's request for a repository waits for the person like an
	// approval does: in the conversation and in the inbox.
	if !view.Closed {
		for _, r := range s.Bench.PendingRepositoryRequests(e) {
			view.Pending = append(view.Pending, repositoryRequestEvent(r))
		}
		// So does its request to start another conversation.
		for _, r := range s.Bench.PendingConversationStarts(e) {
			view.Pending = append(view.Pending, conversationStartEvent(r))
		}
	}
	// So do the sites, downloads and sign-ins its agent browser asks about.
	s.browserView(e, view)
	view.Lineage = s.sessionLineage(ctx, e)
	return view, nil
}
func (s *Service) sessionResponse(ctx context.Context, e *runner.Execution) (*pb.SessionResponse, error) {
	view, err := s.sessionView(ctx, e)
	if err != nil {
		return nil, err
	}
	return &pb.SessionResponse{Session: view}, nil
}
func (s *Service) RenameConversation(ctx context.Context, r *pb.RenameConversationRequest) (*pb.SessionResponse, error) {
	if err := s.Bench.History.Rename(ctx, r.SessionId, r.Title); err != nil {
		return nil, rpcError(err)
	}
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}

// PinConversation keeps a conversation on top of the person's lists, in every
// window and on a paired phone.
func (s *Service) PinConversation(ctx context.Context, r *pb.PinConversationRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = s.Bench.History.Pin(ctx, r.SessionId, r.Pinned); err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}

// ArchiveConversation takes a conversation out of the working lists, or brings
// it back. It stops nothing and deletes nothing.
func (s *Service) ArchiveConversation(ctx context.Context, r *pb.ArchiveConversationRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = s.Bench.History.Archive(ctx, r.SessionId, r.Archived); err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}
