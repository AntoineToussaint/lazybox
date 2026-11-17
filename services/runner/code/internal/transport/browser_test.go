package transport

import (
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/browser"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/encoding/protojson"
)

// withBrowser gives the fixture's Runner an agent browser whose service does
// not exist: everything before Chromium starts (asking, deciding, Stop) is
// real, and starting it fails with the service's absence.
func withBrowser(t *testing.T, f *authorityFixture) {
	t.Helper()
	m, err := browser.New(browser.Config{Script: filepath.Join(t.TempDir(), "missing-browser-service.js"), Root: filepath.Join(t.TempDir(), "browser"), AskTimeout: 5 * time.Second})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = m.Close() })
	f.bench.Browser = m
}

// browserRequest waits for a browser request in a session's pending entries.
func browserRequest(t *testing.T, ctx context.Context, f *authorityFixture, session string) *pb.BrowserRequest {
	t.Helper()
	for {
		got, err := f.client.GetSession(bearer(ctx, ownerToken), &pb.SessionRequest{SessionId: session})
		if err != nil {
			t.Fatal(err)
		}
		for _, p := range got.Session.Pending {
			if p.Type == "browser_request" {
				var r pb.BrowserRequest
				if err = protojson.Unmarshal(p.ProviderDataJson, &r); err != nil {
					t.Fatal(err)
				}
				if got.Session.Browser == nil || !got.Session.Browser.Waiting {
					t.Fatalf("the session's browser does not say it waits: %v", got.Session.Browser)
				}
				return &r
			}
		}
		select {
		case <-ctx.Done():
			t.Fatal("no browser request in the session")
		case <-time.After(20 * time.Millisecond):
		}
	}
}

func TestBrowserRPCsAreTheOwnersAndAnAgentDrivesOnlyItsOwnConversation(t *testing.T) {
	f := newAuthorityFixture(t)
	withBrowser(t, f)
	ctx := timeout(t)
	mine, fake, token := f.open(t)
	other, otherFake, _ := f.open(t)
	if !fake.Session.Open.Browser {
		t.Fatal("a coding agent was not offered the browser tools")
	}
	tool := func(open func(context.Context, string, json.RawMessage) (string, error), name, args string) chan error {
		done := make(chan error, 1)
		go func() { _, err := open(ctx, name, json.RawMessage(args)); done <- err }()
		return done
	}
	// The agent asks, through its own tool pipe; the request is on its own conversation.
	asked := tool(fake.Session.Open.Tools, "browser_navigate", `{"url":"https://docs.example/guide","reason":"Read the guide"}`)
	r := browserRequest(t, ctx, f, mine.ID)
	if r.Kind != "site" || r.Origin != "https://docs.example" || r.Reason != "Read the guide" || r.SessionId != mine.ID || r.AgentName != "Codex" {
		t.Fatalf("request %v", r)
	}
	if got, _ := f.client.GetSession(bearer(ctx, ownerToken), &pb.SessionRequest{SessionId: other.ID}); len(got.Session.Pending) != 0 || got.Session.Browser != nil {
		t.Fatal("another conversation shows this one's browser")
	}
	// Its credential can neither answer it nor steer, watch or configure any browser.
	agent := bearer(ctx, token)
	denied := func(what string, err error) {
		t.Helper()
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s with an agent credential: %v, want PermissionDenied", what, err)
		}
	}
	_, err := f.client.DecideBrowserRequest(agent, &pb.DecideBrowserRequestRequest{RequestId: r.Id, Decision: "workspace"})
	denied("DecideBrowserRequest", err)
	for _, action := range []string{"take_over", "give_back", "stop", "resume", "browse_freely"} {
		_, err = f.client.ControlBrowser(agent, &pb.ControlBrowserRequest{SessionId: mine.ID, Action: action})
		denied("ControlBrowser "+action, err)
	}
	_, err = f.client.SendBrowserInput(agent, &pb.BrowserInputRequest{SessionId: mine.ID, Type: "text", Text: "x"})
	denied("SendBrowserInput", err)
	_, err = f.client.GetBrowserSettings(agent, &pb.InfoRequest{})
	denied("GetBrowserSettings", err)
	_, err = f.client.SaveBrowserSettings(agent, &pb.BrowserSettings{DefaultPermission: "allow"})
	denied("SaveBrowserSettings", err)
	_, err = f.client.ClearBrowserData(agent, &pb.ClearBrowserDataRequest{CollectionId: mine.CollectionID})
	denied("ClearBrowserData", err)
	_, err = f.client.InstallBrowser(agent, &pb.InfoRequest{})
	denied("InstallBrowser", err)
	_, err = f.client.GiveBrowser(agent, &pb.GiveBrowserRequest{SessionId: mine.ID})
	denied("GiveBrowser", err)
	if watch, err := f.client.WatchBrowser(agent, &pb.WatchBrowserRequest{SessionId: mine.ID}); err == nil {
		_, err = watch.Recv()
		denied("WatchBrowser", err)
	}
	// The owner answers. The agent goes on, and here fails to start the missing service.
	if _, err = f.client.DecideBrowserRequest(bearer(ctx, ownerToken), &pb.DecideBrowserRequestRequest{RequestId: r.Id, Decision: "once"}); err != nil {
		t.Fatal(err)
	}
	if err = <-asked; err == nil || !strings.Contains(err.Error(), "browser service") {
		t.Fatalf("after the answer: %v", err)
	}
	if _, err = f.client.DecideBrowserRequest(bearer(ctx, ownerToken), &pb.DecideBrowserRequestRequest{RequestId: r.Id, Decision: "once"}); status.Code(err) != codes.NotFound {
		t.Fatalf("a request answered twice: %v", err)
	}
	// Stop is per conversation: the other agent still asks for itself.
	stopped, err := f.client.ControlBrowser(bearer(ctx, ownerToken), &pb.ControlBrowserRequest{SessionId: mine.ID, Action: "stop"})
	if err != nil || stopped.Status != "stopped" {
		t.Fatal(stopped, err)
	}
	if err = <-tool(fake.Session.Open.Tools, "browser_snapshot", `{}`); err == nil || !strings.Contains(err.Error(), "stopped your browser") {
		t.Fatalf("browsing after Stop: %v", err)
	}
	theirs := tool(otherFake.Session.Open.Tools, "browser_open", `{"url":"https://docs.example/"}`)
	if r = browserRequest(t, ctx, f, other.ID); r.SessionId != other.ID {
		t.Fatal(r)
	}
	if _, err = f.client.DecideBrowserRequest(bearer(ctx, ownerToken), &pb.DecideBrowserRequestRequest{RequestId: r.Id, Decision: "deny", Reason: "Not now"}); err != nil {
		t.Fatal(err)
	}
	if err = <-theirs; err == nil || !strings.Contains(err.Error(), "did not allow docs.example: Not now") {
		t.Fatal(err)
	}
}

func TestBrowserSettingsRoundTripAndValidate(t *testing.T) {
	f := newAuthorityFixture(t)
	withBrowser(t, f)
	ctx := bearer(timeout(t), ownerToken)
	s, err := f.client.GetBrowserSettings(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if s.DefaultPermission != "ask" || s.Downloads != "ask" || !s.ShowPanel || s.Problem != "" {
		t.Fatalf("defaults %v", s)
	}
	s.AllowedSites = []string{"https://Docs.Example/a", "*.trusted.example"}
	s.ShowPanel = false
	saved, err := f.client.SaveBrowserSettings(ctx, s)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(saved.AllowedSites, ",") != "*.trusted.example,docs.example" || saved.ShowPanel || saved.Revision != s.Revision+1 {
		t.Fatal(saved)
	}
	if _, err = f.client.SaveBrowserSettings(ctx, s); status.Code(err) != codes.Aborted {
		t.Fatalf("stale save: %v", err)
	}
	saved.DefaultPermission = "sometimes"
	if _, err = f.client.SaveBrowserSettings(ctx, saved); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("invalid save: %v", err)
	}
	if _, err = f.client.ClearBrowserData(ctx, &pb.ClearBrowserDataRequest{CollectionId: "../x"}); err == nil {
		t.Fatal("unsafe workspace accepted")
	}
}

func TestBrowserAvailabilityAndAccessReachTheApp(t *testing.T) {
	f := newAuthorityFixture(t)
	withBrowser(t, f)
	ctx := bearer(timeout(t), ownerToken)
	mine, _, _ := f.open(t)
	got, err := f.client.GetSession(ctx, &pb.SessionRequest{SessionId: mine.ID})
	if err != nil || got.Session.BrowserAccess != "has" {
		t.Fatalf("a conversation opened with the browser says so: %q %v", got.GetSession().GetBrowserAccess(), err)
	}
	if again, err := f.client.GiveBrowser(ctx, &pb.GiveBrowserRequest{SessionId: mine.ID}); err != nil || again.Session.Id != mine.ID {
		t.Fatalf("an agent that has the browser is not restarted: %v", err)
	}
	s, err := f.client.GetBrowserSettings(ctx, &pb.InfoRequest{})
	if err != nil || s.Availability != "ready" || s.Installable || s.Problem != "" {
		t.Fatalf("availability %v %v", s, err)
	}
	if _, err = f.client.InstallBrowser(ctx, &pb.InfoRequest{}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("nothing to install: %v", err)
	}
	// Off for one workspace, from Settings → Browser, kept and shown.
	s.Workspaces = append(s.Workspaces, &pb.BrowserWorkspaceSites{CollectionId: "ws-x", Off: true})
	saved, err := f.client.SaveBrowserSettings(ctx, s)
	if err != nil {
		t.Fatal(err)
	}
	off := false
	for _, w := range saved.Workspaces {
		off = off || (w.CollectionId == "ws-x" && w.Off)
	}
	if !off || !f.bench.Browser.WorkspaceOff("ws-x") {
		t.Fatalf("off for the workspace not kept: %v", saved.Workspaces)
	}
	// Off everywhere: the conversation says so, and Give the browser says where to turn it on.
	saved.DefaultPermission = "off"
	if _, err = f.client.SaveBrowserSettings(ctx, saved); err != nil {
		t.Fatal(err)
	}
	if got, _ = f.client.GetSession(ctx, &pb.SessionRequest{SessionId: mine.ID}); got.Session.BrowserAccess != "off" {
		t.Fatalf("off: %q", got.Session.BrowserAccess)
	}
	if _, err = f.client.GiveBrowser(ctx, &pb.GiveBrowserRequest{SessionId: mine.ID}); status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "Settings → Browser") {
		t.Fatalf("give while off: %v", err)
	}
	if s, err = f.client.GetBrowserSettings(ctx, &pb.InfoRequest{}); err != nil || s.Availability != "off" || s.Problem == "" {
		t.Fatalf("off availability %v %v", s, err)
	}
}
