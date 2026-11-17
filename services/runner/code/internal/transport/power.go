package transport

import (
	"context"
	"encoding/json"
	"errors"
	"slices"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
)

// Power-user extensibility (POWER.md). Every RPC here is the owner's: none is
// on the agent, terminal or device allowlists, and a hook reaches only
// PostUiRequest for `do` of a safe action and WaitUiRequest for its own
// requests (hook_auth.go).

func (s *Service) GetUserActions(context.Context, *pb.GetUserActionsRequest) (*pb.UserActions, error) {
	snap, _ := s.Bench.Actions().Reload()
	return wireUserActions(s.Bench.Actions(), snap), nil
}

func (s *Service) InitUserActions(context.Context, *pb.GetUserActionsRequest) (*pb.UserActions, error) {
	snap, err := s.Bench.Actions().Init()
	if err != nil {
		return nil, rpcError(err)
	}
	return wireUserActions(s.Bench.Actions(), snap), nil
}

func wireUserActions(m *power.Manager, snap power.Snapshot) *pb.UserActions {
	out := &pb.UserActions{Path: snap.Path, Exists: snap.Exists, Error: snap.Err, Stale: snap.Stale, Revision: snap.Revision, LoadedAtUnixMillis: snap.LoadedAt.UnixMilli(), Schema: power.Schema, SchemaPath: m.SchemaPath(), Events: power.Events}
	for _, i := range snap.Issues {
		out.Issues = append(out.Issues, &pb.UserActionIssue{Path: i.Path, Line: uint32(i.Line), Message: i.Message, Severity: i.Severity})
	}
	for _, a := range snap.File.Actions {
		w := &pb.UserAction{Id: a.ID, Title: a.Title, Description: a.Description, Icon: a.Icon, Key: a.Key, Where: a.Where, Confirm: a.Confirm}
		for _, st := range a.Steps {
			w.Steps = append(w.Steps, &pb.UserActionStep{Kind: st.Kind, Name: st.Name, When: st.When, Text: st.Text, Snippet: st.Snippet, To: st.To, Wait: st.Wait,
				Panel: st.Panel, Url: st.URL, File: st.File, Line: uint32(st.FileLine), Action: st.Action, Command: st.Command, Permission: st.Permission,
				Until: st.Until, TimeoutSeconds: uint32(st.Timeout / time.Second), DoAction: st.Do, DoArgument: st.DoArgument})
		}
		out.Actions = append(out.Actions, w)
	}
	for _, h := range snap.File.Hooks {
		out.Hooks = append(out.Hooks, &pb.UserHook{Id: h.ID, Description: h.Description, On: h.On, Workspaces: h.Workspaces, Conversations: h.Conversations, Repositories: h.Repositories,
			Run: h.Run, Shell: h.Shell, Webhook: h.Webhook, Headers: redactedHeaders(h.Headers), Grants: h.Grants, TimeoutSeconds: uint32(h.Timeout / time.Second), Enabled: h.Enabled})
	}
	return out
}

// redactedHeaders keeps header names and hides values: they are usually
// secrets, and a screen or a screenshot should not show them.
func redactedHeaders(in map[string]string) map[string]string {
	if len(in) == 0 {
		return nil
	}
	out := make(map[string]string, len(in))
	for k := range in {
		out[k] = "••••"
	}
	return out
}

func (s *Service) ListHookRuns(_ context.Context, r *pb.ListHookRunsRequest) (*pb.HookRuns, error) {
	limit := int(r.Limit)
	if limit <= 0 || limit > 100 {
		limit = 50
	}
	out := &pb.HookRuns{}
	for _, run := range s.Bench.Hooks().Runs(limit) {
		out.Runs = append(out.Runs, wireHookRun(run))
	}
	return out, nil
}

func (s *Service) TestHook(_ context.Context, r *pb.TestHookRequest) (*pb.HookRun, error) {
	run, err := s.Bench.Hooks().Test(r.HookId, s.Bench.SampleHookEvent(r.HookId))
	if err != nil {
		return nil, grpcError(codes.NotFound, err)
	}
	return wireHookRun(run), nil
}

func wireHookRun(r power.HookRun) *pb.HookRun {
	w := &pb.HookRun{Id: r.ID, HookId: r.HookID, Event: r.Event, State: r.State, CommandId: r.CommandID, Error: r.Error, Summary: r.Summary,
		CollectionId: r.CollectionID, SessionId: r.SessionID, StartedAtUnixMillis: r.Started.UnixMilli(), HttpStatus: uint32(r.HTTPStatus), Grants: r.Grants, Test: r.Test}
	if !r.Ended.IsZero() {
		w.EndedAtUnixMillis = r.Ended.UnixMilli()
	}
	if r.ExitCode != nil {
		code := int32(*r.ExitCode)
		w.ExitCode = &code
	}
	return w
}

func (s *Service) PostUiRequest(ctx context.Context, r *pb.UiRequest) (*pb.UiRequest, error) {
	if (r.Kind == power.UIAction || r.Kind == power.UIDo) && r.ActionId == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("name the action to run"))
	}
	who, _ := ctx.Value(principalKey{}).(principal)
	if r.Kind == power.UIDo {
		a, ok := power.Registry().Action(r.ActionId)
		if !ok {
			hint := ""
			if near := power.Suggest(r.ActionId, power.Registry().ActionIDs()); near != "" {
				hint = " (did you mean " + near + "?)"
			}
			return nil, grpcError(codes.NotFound, errors.New("the app has no action "+r.ActionId+hint+"; lazybox do --list lists them"))
		}
		if r.Argument != "" && a.Argument == "" {
			return nil, grpcError(codes.InvalidArgument, errors.New(a.ID+" takes no argument"))
		}
		// A hook may only show, focus or arrange (hook_auth.go lets it this far).
		if who.hook != "" && !a.Safe {
			return nil, grpcError(codes.PermissionDenied, errors.New(a.ID+" is not one a hook may run: hooks run only actions that show, focus or arrange (lazybox do --list marks them safe)"))
		}
		// So may an extension, and only the actions approved by name (extension_auth.go).
		if who.extension != "" && !a.Safe {
			return nil, grpcError(codes.PermissionDenied, errors.New(a.ID+" is not one an extension may run: only actions that show, focus or arrange"))
		}
	}
	// Reread first: an action saved a moment ago is there.
	snap, _ := s.Bench.Actions().Reload()
	if r.Kind == power.UIAction && !slices.ContainsFunc(snap.File.Actions, func(a power.Action) bool { return a.ID == r.ActionId }) {
		return nil, grpcError(codes.NotFound, errors.New("no action "+r.ActionId+" in the actions file"))
	}
	if r.Kind == power.UIOpen && r.CollectionId == "" && r.SessionId == "" && r.Panel == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("open a workspace, a conversation, a file or a panel"))
	}
	posted, err := s.Bench.UI().Post(power.UIRequest{Kind: r.Kind, CollectionID: r.CollectionId, SessionID: r.SessionId, File: r.File, Line: int(r.Line), Panel: r.Panel, ActionID: r.ActionId, Argument: r.Argument, Poster: who.poster()})
	if errors.Is(err, power.ErrNoWindow) {
		return nil, grpcError(codes.Unavailable, err)
	}
	if err != nil {
		return nil, grpcError(codes.FailedPrecondition, err)
	}
	return wireUIRequest(posted), nil
}

func (s *Service) WaitUiRequest(ctx context.Context, r *pb.WaitUiRequestRequest) (*pb.UiRequest, error) {
	timeout := time.Duration(r.TimeoutMs) * time.Millisecond
	if timeout <= 0 {
		timeout = 30 * time.Second
	}
	timeout = min(timeout, 2*time.Minute)
	who, _ := ctx.Value(principalKey{}).(principal)
	got, err := s.Bench.UI().Wait(ctx, r.Id, who.poster(), timeout)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireUIRequest(got), nil
}

func (s *Service) WatchUiRequests(r *pb.WatchUiRequestsRequest, stream grpc.ServerStreamingServer[pb.UiRequest]) error {
	surface := r.Surface
	if surface != "desktop" {
		surface = "web"
	}
	requests, stop := s.Bench.UI().Watch(surface)
	defer stop()
	// A first, empty message tells the window it is registered.
	if err := stream.Send(&pb.UiRequest{}); err != nil {
		return err
	}
	for {
		select {
		case <-stream.Context().Done():
			return nil
		case req := <-requests:
			if err := stream.Send(wireUIRequest(req)); err != nil {
				return err
			}
		}
	}
}

func (s *Service) CompleteUiRequest(_ context.Context, r *pb.CompleteUiRequestRequest) (*pb.UiRequest, error) {
	got, err := s.Bench.UI().Complete(r.Id, r.Error, r.Detail, r.Png)
	if err != nil {
		return nil, grpcError(codes.FailedPrecondition, err)
	}
	return wireUIRequest(got), nil
}

func wireUIRequest(r power.UIRequest) *pb.UiRequest {
	return &pb.UiRequest{Id: r.ID, Kind: r.Kind, CollectionId: r.CollectionID, SessionId: r.SessionID, File: r.File, Line: uint32(r.Line), Panel: r.Panel, ActionId: r.ActionID,
		State: r.State, Error: r.Error, Png: r.PNG, Detail: r.Detail, CreatedAtUnixMillis: r.Created.UnixMilli(), Argument: r.Argument}
}

// GetConfigFiles is the person's config files as last loaded (POWER.md,
// "Config files"); a new revision means one of them changed.
func (s *Service) GetConfigFiles(context.Context, *pb.GetConfigFilesRequest) (*pb.ConfigFiles, error) {
	snap, _ := s.Bench.ConfigFiles().Reload()
	return wireConfigFiles(snap)
}

// UpdateConfigFile changes one config file; a change that would not load is
// refused with each problem, and nothing is written.
func (s *Service) UpdateConfigFile(_ context.Context, r *pb.UpdateConfigFileRequest) (*pb.ConfigFiles, error) {
	var edits []power.Edit
	for _, e := range r.Edits {
		edits = append(edits, power.Edit{Path: e.Path, JSON: e.Json})
	}
	var content []byte
	if r.Content != "" {
		content = []byte(r.Content)
	}
	if len(edits) == 0 && content == nil && !r.Remove {
		return nil, grpcError(codes.InvalidArgument, errors.New("nothing to change: give edits, content or remove"))
	}
	snap, err := s.Bench.ConfigFiles().Update(r.Name, edits, content, r.Remove)
	var refused *power.RefusedError
	if errors.As(err, &refused) {
		return nil, grpcError(codes.FailedPrecondition, err)
	}
	if err != nil {
		return nil, grpcError(codes.InvalidArgument, err)
	}
	return wireConfigFiles(snap)
}

func wireConfigFiles(snap power.ConfigSnapshot) (*pb.ConfigFiles, error) {
	out := &pb.ConfigFiles{Directory: snap.Dir, Revision: snap.Revision}
	for _, d := range snap.Docs {
		w := &pb.ConfigFile{Name: d.Name, Kind: d.Kind, Path: d.Path, Exists: d.Exists, Error: d.Err, Stale: d.Stale, Revision: d.Revision, LoadedAtUnixMillis: d.LoadedAt.UnixMilli(), SchemaPath: d.SchemaPath}
		for _, i := range d.Issues {
			w.Issues = append(w.Issues, &pb.UserActionIssue{Path: i.Path, Line: uint32(i.Line), Message: i.Message, Severity: i.Severity})
		}
		if d.Value != nil {
			data, err := json.Marshal(d.Value)
			if err != nil {
				return nil, rpcError(err)
			}
			w.Json = string(data)
		}
		out.Files = append(out.Files, w)
	}
	return out, nil
}
