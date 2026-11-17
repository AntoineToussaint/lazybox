package transport

import (
	"context"
	"errors"
	"slices"
	"sort"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/browser"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/protobuf/encoding/protojson"
)

// The agent browser (BROWSER.md). Every RPC here is the owner's (auth.go
// grants agents none): an agent drives only its own conversation's browser,
// through its execution's tool pipe, and the person watches and steers it here.

func wireBrowserAction(a browser.Action) *pb.BrowserAction {
	w := &pb.BrowserAction{Seq: a.Seq, Kind: a.Kind, Actor: a.Actor, Summary: a.Summary, Url: a.URL, Status: a.Status, Error: a.Error, AtUnixMillis: millis(a.At), DurationMs: uint32(a.Duration.Milliseconds())}
	if a.Box != nil {
		w.Box = &pb.BrowserBox{X: a.Box.X, Y: a.Box.Y, Width: a.Box.Width, Height: a.Box.Height}
	}
	return w
}

func wireBrowserState(s browser.State) *pb.BrowserState {
	w := &pb.BrowserState{ConversationId: s.Conversation, SessionId: s.Session, CollectionId: s.Workspace, Status: s.Status, Controller: s.Controller, Url: s.URL, Title: s.Title, Loading: s.Loading,
		CanGoBack: s.CanGoBack, CanGoForward: s.CanGoForward, BrowseFreely: s.BrowseFreely, Problem: s.Problem, ViewportWidth: uint32(s.ViewportWidth), ViewportHeight: uint32(s.ViewportHeight),
		UpdatedAtUnixMillis: millis(s.Updated), AgentName: s.Agent, Waiting: s.Waiting}
	if s.Current != nil {
		w.Current = wireBrowserAction(*s.Current)
	}
	return w
}

func wireBrowserRequest(r browser.Request) *pb.BrowserRequest {
	return &pb.BrowserRequest{Id: r.ID, SessionId: r.Session, ConversationId: r.Conversation, CollectionId: r.Workspace, Kind: r.Kind, Url: r.URL, Origin: r.Origin, Reason: r.Reason, FileName: r.FileName, CreatedAtUnixMillis: millis(r.Created), Permission: r.Permission, AgentName: r.Agent}
}

// browserRequestEvent is a waiting browser request as a session's pending
// entry, so every client that shows approvals shows it.
func browserRequestEvent(r browser.Request) *pb.AgentEvent {
	text := map[string]string{"site": "Open " + r.Origin, "download": "Download " + r.FileName, "sign_in": "Sign in on " + r.Origin}[r.Kind]
	data, _ := protojson.Marshal(wireBrowserRequest(r))
	return &pb.AgentEvent{Type: "browser_request", RequestId: r.ID, Text: text, ProviderDataJson: data}
}

// browserView adds a conversation's browser and its waiting requests to its session view.
func (s *Service) browserView(e *runner.Execution, view *pb.Session) {
	if s.Bench.Browser == nil {
		return
	}
	view.BrowserAccess = s.Bench.BrowserAccess(e)
	caller := s.Bench.BrowserCaller(e)
	if state, ok := s.Bench.Browser.State(caller.Conversation); ok {
		view.Browser = wireBrowserState(state)
	}
	if !view.Closed {
		for _, r := range s.Bench.Browser.Pending(caller.Conversation) {
			view.Pending = append(view.Pending, browserRequestEvent(r))
		}
	}
}

func browserError(err error) error {
	switch {
	case errors.Is(err, browser.ErrNotFound):
		return grpcError(codes.NotFound, err)
	case errors.Is(err, browser.ErrConflict):
		return grpcError(codes.Aborted, err)
	case errors.Is(err, browser.ErrUnavailable):
		return grpcError(codes.Unavailable, err)
	}
	return rpcError(err)
}

func (s *Service) browserCaller(session string) (browser.Caller, error) {
	if s.Bench.Browser == nil {
		return browser.Caller{}, grpcError(codes.Unavailable, browser.ErrUnavailable)
	}
	if session == "" {
		return browser.Caller{}, grpcError(codes.InvalidArgument, errors.New("session required"))
	}
	e, err := s.Bench.Get(session)
	if err != nil {
		return browser.Caller{}, rpcError(err)
	}
	return s.Bench.BrowserCaller(e), nil
}

func (s *Service) WatchBrowser(r *pb.WatchBrowserRequest, stream grpc.ServerStreamingServer[pb.BrowserEvent]) error {
	caller, err := s.browserCaller(r.SessionId)
	if err != nil {
		return err
	}
	w, first, stop := s.Bench.Browser.Watch(caller)
	defer stop()
	out := &pb.BrowserEvent{State: wireBrowserState(*first.State)}
	for _, a := range first.History {
		out.History = append(out.History, wireBrowserAction(a))
	}
	if first.Frame != nil {
		out.Frame = &pb.BrowserFrame{Jpeg: first.Frame.JPEG, Width: uint32(first.Frame.Width), Height: uint32(first.Frame.Height), Seq: first.Frame.Seq}
	}
	if err = stream.Send(out); err != nil {
		return err
	}
	ctx := stream.Context()
	for {
		var next *pb.BrowserEvent
		select {
		case <-ctx.Done():
			return nil
		case <-w.Done():
			// Too far behind (or the conversation was deleted): the client follows again.
			return nil
		case e := <-w.Events():
			next = &pb.BrowserEvent{}
			if e.State != nil {
				next.State = wireBrowserState(*e.State)
			}
			if e.Action != nil {
				next.Action = wireBrowserAction(*e.Action)
			}
		case f := <-w.Frames():
			next = &pb.BrowserEvent{Frame: &pb.BrowserFrame{Jpeg: f.JPEG, Width: uint32(f.Width), Height: uint32(f.Height), Seq: f.Seq}}
		}
		if err = stream.Send(next); err != nil {
			return err
		}
	}
}

func (s *Service) ControlBrowser(ctx context.Context, r *pb.ControlBrowserRequest) (*pb.BrowserState, error) {
	caller, err := s.browserCaller(r.SessionId)
	if err != nil {
		return nil, err
	}
	state, err := s.Bench.Browser.Control(ctx, caller, r.Action, r.Url)
	if err != nil {
		return nil, browserError(err)
	}
	return wireBrowserState(state), nil
}

func (s *Service) SendBrowserInput(ctx context.Context, r *pb.BrowserInputRequest) (*pb.BrowserState, error) {
	caller, err := s.browserCaller(r.SessionId)
	if err != nil {
		return nil, err
	}
	input := map[string]any{"type": r.Type, "x": r.X, "y": r.Y, "button": r.Button, "clickCount": r.ClickCount, "deltaX": r.DeltaX, "deltaY": r.DeltaY, "key": r.Key, "text": r.Text, "modifiers": r.Modifiers}
	state, err := s.Bench.Browser.Input(ctx, caller.Conversation, input)
	if err != nil {
		return nil, browserError(err)
	}
	return wireBrowserState(state), nil
}

func (s *Service) DecideBrowserRequest(_ context.Context, r *pb.DecideBrowserRequestRequest) (*pb.BrowserState, error) {
	if s.Bench.Browser == nil {
		return nil, grpcError(codes.Unavailable, browser.ErrUnavailable)
	}
	state, err := s.Bench.Browser.Decide(r.RequestId, r.Decision, r.Reason)
	if err != nil {
		return nil, browserError(err)
	}
	return wireBrowserState(state), nil
}

func (s *Service) GetBrowserSettings(context.Context, *pb.InfoRequest) (*pb.BrowserSettings, error) {
	if s.Bench.Browser == nil {
		return nil, grpcError(codes.Unavailable, browser.ErrUnavailable)
	}
	return s.browserSettings(s.Bench.Browser.Settings()), nil
}

// browserSettings is Settings → Browser: the rules, and per workspace its
// always-allowed sites and the size of its browser profile.
func (s *Service) browserSettings(v browser.Settings) *pb.BrowserSettings {
	w := &pb.BrowserSettings{Revision: v.Revision, DefaultPermission: v.DefaultPermission, AllowedSites: v.AllowedSites, Downloads: v.Downloads, ShowPanel: v.ShowPanel}
	// Why agents cannot browse, and the fix: install Chromium, turn it on.
	a := s.Bench.Browser.Availability()
	w.Availability, w.Problem, w.Installable = a.State, a.Problem, len(a.Install) > 0
	w.InstallCommandId = s.Bench.Browser.InstallID()
	names := map[string]string{}
	if s.Bench.Collections != nil {
		for _, c := range s.Bench.Collections.List() {
			if c.State != "deleted" && c.State != "deleting" {
				names[c.ID] = c.Name
			}
		}
	}
	data := s.Bench.Browser.ProfileData()
	seen := map[string]bool{}
	add := func(id string) {
		if seen[id] {
			return
		}
		seen[id] = true
		size, has := data[id]
		w.Workspaces = append(w.Workspaces, &pb.BrowserWorkspaceSites{CollectionId: id, Name: names[id], Sites: v.WorkspaceSites[id], HasData: has, DataBytes: size, Off: slices.Contains(v.OffWorkspaces, id)})
	}
	// Every workspace, so each can be turned off; then what is kept of others.
	for id := range names {
		add(id)
	}
	for id := range v.WorkspaceSites {
		add(id)
	}
	for id := range data {
		add(id)
	}
	for _, id := range v.OffWorkspaces {
		add(id)
	}
	sort.Slice(w.Workspaces, func(i, j int) bool {
		a, b := w.Workspaces[i], w.Workspaces[j]
		if a.Name != b.Name {
			return a.Name < b.Name
		}
		return a.CollectionId < b.CollectionId
	})
	return w
}

func (s *Service) SaveBrowserSettings(_ context.Context, r *pb.BrowserSettings) (*pb.BrowserSettings, error) {
	if s.Bench.Browser == nil {
		return nil, grpcError(codes.Unavailable, browser.ErrUnavailable)
	}
	next := browser.Settings{Revision: r.Revision, DefaultPermission: r.DefaultPermission, AllowedSites: r.AllowedSites, Downloads: r.Downloads, ShowPanel: r.ShowPanel, WorkspaceSites: map[string][]string{}}
	for _, ws := range r.Workspaces {
		next.WorkspaceSites[ws.CollectionId] = ws.Sites
		if ws.Off {
			next.OffWorkspaces = append(next.OffWorkspaces, ws.CollectionId)
		}
	}
	saved, err := s.Bench.Browser.SaveSettings(next)
	if err != nil {
		if errors.Is(err, browser.ErrConflict) {
			return nil, browserError(err)
		}
		return nil, grpcError(codes.InvalidArgument, err)
	}
	return s.browserSettings(saved), nil
}

func (s *Service) ClearBrowserData(ctx context.Context, r *pb.ClearBrowserDataRequest) (*pb.BrowserSettings, error) {
	if s.Bench.Browser == nil {
		return nil, grpcError(codes.Unavailable, browser.ErrUnavailable)
	}
	saved, err := s.Bench.Browser.ClearData(ctx, r.CollectionId)
	if err != nil {
		return nil, browserError(err)
	}
	return s.browserSettings(saved), nil
}

// InstallBrowser installs the agent browser's Chromium (Settings → Browser).
func (s *Service) InstallBrowser(context.Context, *pb.InfoRequest) (*pb.CommandStatus, error) {
	if s.Bench.Browser == nil {
		return nil, grpcError(codes.Unavailable, browser.ErrUnavailable)
	}
	status, err := s.Bench.InstallBrowser()
	if err != nil {
		if errors.Is(err, browser.ErrUnavailable) {
			return nil, browserError(err)
		}
		return nil, grpcError(codes.FailedPrecondition, err)
	}
	return wireStatus(status), nil
}

// GiveBrowser is "Give Claude the browser" in a conversation's menu.
func (s *Service) GiveBrowser(ctx context.Context, r *pb.GiveBrowserRequest) (*pb.SessionResponse, error) {
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	next, err := s.Bench.GiveBrowser(ctx, e)
	if err != nil {
		if errors.Is(err, browser.ErrUnavailable) {
			return nil, browserError(err)
		}
		return nil, grpcError(codes.FailedPrecondition, err)
	}
	return s.sessionResponse(ctx, next)
}
