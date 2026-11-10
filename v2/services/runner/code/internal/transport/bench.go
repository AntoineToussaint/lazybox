package transport

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"os"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/attachments"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/checkpoints"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/sessionstore"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type Service struct {
	Inspector gitinspect.Inspector
	pb.UnimplementedRunnerServiceServer
	Bench *runner.Bench
}

func rpcError(err error) error {
	if err == nil {
		return nil
	}
	code := codes.FailedPrecondition
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
	if errors.Is(err, journal.ErrFull) {
		code = codes.ResourceExhausted
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
	w := &pb.Session{Id: s.ID, ProfileId: s.Profile, ProviderReference: s.Reference, GatewayId: s.GatewayID, Busy: s.Busy, Closed: s.Closed, Fault: s.Fault, Queued: uint32(s.Queued), WorkId: e.WorkID, PreviousExecutionId: e.Previous, CollectionId: e.CollectionID, WorkingDirectory: e.Cwd, Purpose: e.Purpose, Status: string(s.Status), Account: wireAccount(s.Account), ActivityTruncated: s.ActivityTruncated, RotationProfileIds: e.Options.RotationProfiles, Capabilities: &pb.Capabilities{Images: c.Images, Files: c.Files, Questions: c.Questions, Approvals: c.Approvals, Steer: c.Steer, Resume: c.Resume, PendingResume: c.PendingResume, ProfileIsolation: c.ProfileIsolation, Subagents: c.Subagents}}
	w.ConversationId = e.Saved.ConversationID
	w.Title = s.Title
	if w.Title == "" {
		w.Title = e.Saved.Title
	}
	w.Historical = e.Archived
	w.RecoveryRequired = e.RecoveryRequired
	w.ResumeBlocked = e.Saved.ResumeBlocked
	w.CreatedAtUnixMillis = e.Saved.CreatedAt.UnixMilli()
	for _, a := range engine.SortedActivities(s.Agents) {
		w.Agents = append(w.Agents, &pb.AgentActivity{Id: a.ID, ParentId: a.ParentID, NativeId: a.NativeID, Name: a.Name, Kind: a.Kind, Status: string(a.Status), LastTurnStatus: a.LastTurnStatus, UpdatedAtUnixMillis: a.UpdatedAt.UnixMilli()})
	}
	for _, p := range s.Pending {
		w.Pending = append(w.Pending, wireEvent(p))
	}
	return w
}
func wireStatus(s commands.Status) *pb.CommandStatus {
	w := &pb.CommandStatus{Id: s.ID, SessionId: s.Spec.SessionID, Title: s.Spec.Title, Argv: s.Spec.Argv, Mode: s.Mode, Offset: s.Offset, Done: s.Done, Signal: s.Signal, CaptureError: s.Error}
	if s.ExitCode != nil {
		code := int32(*s.ExitCode)
		w.ExitCode = &code
	}
	return w
}

func (s *Service) Info(ctx context.Context, r *pb.InfoRequest) (*pb.InfoResponse, error) {
	if err := s.Bench.ReloadProfiles(); err != nil {
		return nil, rpcError(err)
	}
	w := &pb.InfoResponse{SharedSubscriptions: s.Bench.Config.SubscriptionState != s.Bench.Config.State, DefaultModel: s.Bench.Config.Defaults.Model, DefaultPermission: s.Bench.Config.Defaults.Permission, AttachmentLimits: &pb.AttachmentLimits{MaxFiles: attachments.MaxFiles, MaxFileBytes: attachments.MaxFileBytes, MaxMessageBytes: attachments.MaxMessageBytes}}
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
	for _, e := range s.Bench.List() {
		record, err := s.sessionView(ctx, e)
		if err != nil {
			return nil, err
		}
		w.Sessions = append(w.Sessions, record)
	}
	return w, nil
}
func (s *Service) OpenSession(ctx context.Context, r *pb.OpenSessionRequest) (*pb.SessionResponse, error) {
	p := r
	options := accounts.Options{}
	gateway := ""
	if p.Options != nil {
		options.Model = p.Options.Model
		options.Instructions = p.Options.Instructions
		options.Workflow = p.Options.Workflow
		options.Permission = p.Options.Permission
		options.RotationProfiles = p.Options.RotationProfileIds
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
		options.Model = p.Options.Model
		options.Instructions = p.Options.Instructions
		options.RotationProfiles = p.Options.RotationProfileIds
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
	return s.sessionResponse(ctx, e)
}

func (s *Service) Apply(ctx context.Context, r *pb.ApplyRequest) (*pb.Receipt, error) {
	p := r
	e, err := s.Bench.Get(p.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	op := agent.Operation{ID: p.OperationId}
	message := func(m *pb.Message) error {
		if m == nil {
			return errors.New("message required")
		}
		state := e.Engine.Snapshot()
		if state.Closed || state.Fault != "" {
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
		for i, file := range op.Files {
			media := http.DetectContentType(inputs[i].Data)
			if state.Capabilities.Images && len(op.Images) < 4 && file.Size <= 6<<20 && media == file.MediaType && (media == "image/png" || media == "image/jpeg" || media == "image/webp" || media == "image/gif") {
				op.Images = append(op.Images, agent.Image{MediaType: media, Data: base64.StdEncoding.EncodeToString(inputs[i].Data)})
			}
		}
		return nil
	}
	switch o := p.Operation.(type) {
	case *pb.ApplyRequest_Message:
		op.Kind = agent.Message
		if err := message(o.Message); err != nil {
			return nil, rpcError(err)
		}
	case *pb.ApplyRequest_Steer:
		op.Kind = agent.Steer
		if err := message(o.Steer); err != nil {
			return nil, rpcError(err)
		}
	case *pb.ApplyRequest_Interrupt:
		op.Kind = agent.Interrupt
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
	return &pb.Receipt{OperationId: receipt.ID, Delivery: receipt.Delivery, ProviderId: receipt.ProviderID, Error: receipt.Error}, nil
}

func (s *Service) WatchEvents(r *pb.WatchEventsRequest, stream grpc.ServerStreamingServer[pb.JournalEvent]) error {
	ctx := stream.Context()
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return rpcError(err)
	}
	cursor := r.After
	for {
		var record journal.Record
		if r.IncludePrevious {
			record, err = s.Bench.Timeline(ctx, e.ID, cursor)
		} else {
			record, err = e.Engine.Events(ctx, cursor)
		}
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return rpcError(err)
		}
		var ev agent.Event
		var envelope struct {
			Receipt *agent.Receipt `json:"receipt"`
		}
		if err = json.Unmarshal(record.Value, &ev); err != nil {
			return rpcError(err)
		}
		_ = json.Unmarshal(record.Value, &envelope)
		w := &pb.JournalEvent{Cursor: record.Sequence, CapturedAt: record.Time.Format(time.RFC3339Nano), Event: wireEvent(ev)}
		if envelope.Receipt != nil {
			v := envelope.Receipt
			w.Receipt = &pb.Receipt{OperationId: v.ID, Delivery: v.Delivery, ProviderId: v.ProviderID, Error: v.Error}
		}
		if ev.Type == "session_opened" || ev.Type == "execution_context" || ev.Type == "continuation_requested" {
			w.MetadataJson = record.Value
		}
		if err = stream.Send(w); err != nil {
			return err
		}
		cursor = record.Sequence
	}
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
		if _, err = next.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: prompt}); err != nil {
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
	c, err := s.Bench.Start(e, r.Title, r.Argv)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireStatus(c.Status()), nil
}
func (s *Service) RegisterPipe(ctx context.Context, r *pb.RegisterPipeRequest) (*pb.CommandStatus, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	c, err := s.Bench.RegisterPipe(e, r.Title)
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
	if err = c.FinishPipe(cause); err != nil {
		return nil, rpcError(err)
	}
	return wireStatus(c.Status()), nil
}
func (s *Service) ListCommands(ctx context.Context, r *pb.ListCommandsRequest) (*pb.ListCommandsResponse, error) {
	w := &pb.ListCommandsResponse{}
	for _, c := range s.Bench.Commands.List(r.SessionId) {
		w.Commands = append(w.Commands, wireStatus(c))
	}
	return w, nil
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

func (s *Service) Version(context.Context, *pb.VersionRequest) (*pb.VersionResponse, error) {
	return &pb.VersionResponse{Version: "0.1.0-experiment"}, nil
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
	for _, record := range records {
		e, err := s.Bench.Get(record.ID)
		if err != nil {
			return nil, rpcError(err)
		}
		view, err := s.sessionView(ctx, e)
		if err != nil {
			return nil, err
		}
		response.Sessions = append(response.Sessions, view)
	}
	return response, nil
}
func (s *Service) ResumeSession(ctx context.Context, r *pb.ResumeSessionRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Resume(ctx, r.SessionId, r.ProcessesStopped)
	if err != nil {
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
	// Creation order belongs to the conversation, not its latest continuation.
	if record.ConversationID != record.ID {
		root, err := s.Bench.History.Get(ctx, record.ConversationID)
		if err != nil {
			return nil, rpcError(err)
		}
		view.CreatedAtUnixMillis = root.CreatedAt.UnixMilli()
	}
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
