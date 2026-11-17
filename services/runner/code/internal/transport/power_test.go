package transport

import (
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"
	"google.golang.org/protobuf/types/known/emptypb"
)

// powerFixture is a Runner with one workspace and one conversation in it,
// served behind the production interceptors.
type powerFixture struct {
	service   *Service
	fake      *agenttest.Adapter
	conn      *grpc.ClientConn
	client    pb.RunnerServiceClient
	workspace *pb.Collection
	session   string
}

func newPowerFixture(t *testing.T, actions string) *powerFixture {
	t.Helper()
	service, fake := collectionService(t)
	workspace := createTestCollection(t, service)
	if err := os.WriteFile(service.Bench.Actions().Path(), []byte(actions), 0o600); err != nil {
		t.Fatal(err)
	}
	if snap, _ := service.Bench.Actions().Reload(); snap.Err != "" || len(snap.Issues) != 0 {
		t.Fatalf("actions file: %+v", snap)
	}
	server := grpc.NewServer(LocalAuthority(ownerToken, service.Bench)...)
	pb.RegisterRunnerServiceServer(server, service)
	listener := bufconn.Listen(1 << 20)
	go func() { _ = server.Serve(listener) }()
	t.Cleanup(func() { server.Stop(); listener.Close() })
	conn, err := grpc.NewClient("passthrough:///power", grpc.WithTransportCredentials(insecure.NewCredentials()), grpc.WithContextDialer(func(ctx context.Context, _ string) (net.Conn, error) { return listener.DialContext(ctx) }))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	opened, err := service.OpenSession(context.Background(), &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: workspace.Id, Options: &pb.RunnerOptions{NoRotation: true}})
	if err != nil {
		t.Fatal(err)
	}
	return &powerFixture{service: service, fake: fake, conn: conn, client: pb.NewRunnerServiceClient(conn), workspace: workspace, session: opened.Session.Id}
}

func (f *powerFixture) owner(t *testing.T) context.Context { return bearer(timeout(t), ownerToken) }

// runs waits until the hook runs satisfy done.
func (f *powerFixture) runs(t *testing.T, done func([]*pb.HookRun) bool) []*pb.HookRun {
	t.Helper()
	var runs []*pb.HookRun
	until(t, "hook runs never settled", func() bool {
		got, err := f.client.ListHookRuns(f.owner(t), &pb.ListHookRunsRequest{})
		if err != nil {
			t.Fatal(err)
		}
		runs = got.Runs
		return done(runs)
	})
	return runs
}

func settled(n int) func([]*pb.HookRun) bool {
	return func(runs []*pb.HookRun) bool {
		if len(runs) != n {
			return false
		}
		for _, r := range runs {
			if r.State == power.RunRunning {
				return false
			}
		}
		return true
	}
}

// Hooks fire on what happens to a conversation, once each, as managed
// commands of the workspace that no conversation's console lists, with the
// event in their environment and no Runner credential.
func TestHooksRunOnConversationEventsAsManagedCommands(t *testing.T) {
	out := filepath.Join(t.TempDir(), "hook.log")
	t.Setenv("HOOK_OUT", out)
	f := newPowerFixture(t, `hooks:
  - id: record
    on: [agent.needs-input, turn.done, turn.failed]
    shell: 'printf "%s|%s|%s\n" "$LAZYBOX_EVENT" "$LAZYBOX_EVENT_REASON" "${LAZYBOX_V2_ENDPOINT:-no-credential}" >> "$HOOK_OUT"; cp "$LAZYBOX_EVENT_FILE" "$HOOK_OUT.$LAZYBOX_EVENT.json"'
  - id: elsewhere
    on: turn.done
    filter: { workspace: "not-this-one" }
    run: [/usr/bin/false]
`)
	b := f.service.Bench
	b.ObserveHookEvents() // the baseline: nothing that was already there fires
	native := f.fake.Session
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Text: "npm test"}
	until(t, "approval never pending", func() bool { got, _ := b.Get(f.session); return len(got.Engine.Snapshot().Pending) == 1 })
	b.ObserveHookEvents()
	b.ObserveHookEvents()
	runs := f.runs(t, settled(1))
	if r := runs[0]; r.State != power.RunSucceeded || r.HookId != "record" || r.Event != power.EventNeedsInput || r.CollectionId != f.workspace.Id || r.SessionId != f.session || r.CommandId == "" || *r.ExitCode != 0 {
		t.Fatalf("needs-input run: %+v", r)
	}
	native.Events <- agent.Event{Type: "request_resolved", RequestID: "r1"}
	native.Events <- agent.Event{Type: "turn_started"}
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	until(t, "turn never ended", func() bool {
		got, _ := b.Get(f.session)
		return !got.Engine.Snapshot().Agents["root"].TurnEndedAt.IsZero()
	})
	b.ObserveHookEvents()
	runs = f.runs(t, settled(2))
	if runs[0].Event != power.EventTurnDone || runs[0].State != power.RunSucceeded {
		t.Fatalf("turn run: %+v", runs[0])
	}
	log, err := os.ReadFile(out)
	if err != nil {
		t.Fatal(err)
	}
	if want := "agent.needs-input|Approval: npm test|no-credential\nturn.done|turn ended|no-credential\n"; string(log) != want {
		t.Fatalf("hook saw:\n%s\nwant:\n%s", log, want)
	}
	var event map[string]any
	data, _ := os.ReadFile(out + ".agent.needs-input.json")
	if err = json.Unmarshal(data, &event); err != nil || !strings.HasSuffix(event["request"].(map[string]any)["id"].(string), "r1") || event["workspace"].(map[string]any)["name"] != "Feature" {
		t.Fatalf("event file: %s %v", data, err)
	}
	// The run's output is its command's; no conversation console lists it.
	listed, err := f.client.ListCommands(f.owner(t), &pb.ListCommandsRequest{SessionId: f.session})
	if err != nil {
		t.Fatal(err)
	}
	for _, c := range listed.Commands {
		if c.Id == runs[0].CommandId || c.Origin == "hook" {
			t.Fatalf("a conversation console lists a hook run: %+v", c)
		}
	}
	read, err := f.client.ReadCommand(f.owner(t), &pb.ReadCommandRequest{CommandId: runs[0].CommandId, Limit: 1024})
	if err != nil || read.Status.Origin != "hook" || !read.Status.Done {
		t.Fatalf("hook command: %+v %v", read, err)
	}
	// The event file is removed with the run.
	if entries, _ := os.ReadDir(filepath.Join(b.Config.State, "hook-runs")); len(entries) != 0 {
		t.Fatalf("event files left: %v", entries)
	}
}

// A hook holds a credential only when granted, only while it runs, and only
// for exactly what a paired phone with the same grants may call (less Info).
func TestHookCredentialReachesOnlyItsGrantsWhileItRuns(t *testing.T) {
	out := filepath.Join(t.TempDir(), "endpoint.json")
	release := filepath.Join(t.TempDir(), "release")
	t.Setenv("HOOK_OUT", out)
	t.Setenv("HOOK_RELEASE", release)
	f := newPowerFixture(t, `hooks:
  - id: reader
    on: turn.done
    grants: [read]
    shell: 'cp "$LAZYBOX_V2_ENDPOINT" "$HOOK_OUT.tmp" && mv "$HOOK_OUT.tmp" "$HOOK_OUT"; while [ ! -e "$HOOK_RELEASE" ]; do sleep 0.05; done'
`)
	run, err := f.client.TestHook(f.owner(t), &pb.TestHookRequest{HookId: "reader"})
	if err != nil || !run.Test || run.State != power.RunRunning || strings.Join(run.Grants, ",") != "read" {
		t.Fatalf("test run: %+v %v", run, err)
	}
	var endpoint struct{ URL, Token string }
	until(t, "hook never received its endpoint", func() bool {
		data, err := os.ReadFile(out)
		return err == nil && json.Unmarshal(data, &endpoint) == nil
	})
	if !strings.HasPrefix(endpoint.Token, "lbx-hook.") || endpoint.Token == ownerToken {
		t.Fatalf("hook credential: %q", endpoint.Token)
	}
	ctx := bearer(timeout(t), endpoint.Token)
	if _, err = f.client.ListHistory(ctx, &pb.ListHistoryRequest{}); err != nil {
		t.Fatal("read grant cannot list conversations:", err)
	}
	if _, err = f.client.GetSession(ctx, &pb.SessionRequest{SessionId: f.session}); err != nil {
		t.Fatal("read grant cannot read a conversation:", err)
	}
	send := &pb.ApplyRequest{SessionId: f.session, OperationId: "hook-send", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "hi"}}}
	if _, err = f.client.Apply(ctx, send); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("read grant sent a message: %v", err)
	}
	// Every RPC off the read grant's list is denied, including every power
	// RPC and Info, and any RPC added later.
	allowed := map[string]bool{}
	for method, rule := range deviceRPCs {
		if rule.grant == "read" && !hookExcluded[method] {
			allowed[method] = true
		}
	}
	desc := pb.RunnerService_ServiceDesc
	checked := 0
	for _, m := range desc.Methods {
		method := "/" + desc.ServiceName + "/" + m.MethodName
		if allowed[method] {
			continue
		}
		if err := f.conn.Invoke(ctx, method, &emptypb.Empty{}, &emptypb.Empty{}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: hook credential got %v, want PermissionDenied", method, err)
		}
		checked++
	}
	for _, s := range desc.Streams {
		method := "/" + desc.ServiceName + "/" + s.StreamName
		if deviceStreams[method] == "read" {
			continue
		}
		stream, err := f.conn.NewStream(ctx, &grpc.StreamDesc{StreamName: s.StreamName, ServerStreams: s.ServerStreams}, method)
		if err == nil {
			if err = stream.SendMsg(&emptypb.Empty{}); err == nil {
				_ = stream.CloseSend()
				err = stream.RecvMsg(&emptypb.Empty{})
			}
		}
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: hook credential got %v, want PermissionDenied", method, err)
		}
	}
	if checked < 60 {
		t.Fatalf("checked only %d RPCs", checked)
	}
	// Forged: the grants are bound to the credential.
	forged := strings.Replace(endpoint.Token, "lbx-hook.", "lbx-hook.x", 1)
	if _, err = f.client.ListHistory(bearer(timeout(t), forged), &pb.ListHistoryRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("forged hook credential: %v", err)
	}
	// The credential ends with the run, and its endpoint file is removed.
	if err = os.WriteFile(release, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	f.runs(t, settled(1))
	if _, err = f.client.ListHistory(bearer(timeout(t), endpoint.Token), &pb.ListHistoryRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("a finished hook's credential still works: %v", err)
	}
	if entries, _ := os.ReadDir(filepath.Join(f.service.Bench.Config.State, "hook-endpoints")); len(entries) != 0 {
		t.Fatalf("hook endpoint files left: %v", entries)
	}
}

// A webhook posts the event and its log is the run's output.
func TestWebhookHookPostsTheEventAndLogsTheExchange(t *testing.T) {
	var mu sync.Mutex
	var bodies []string
	var headers []http.Header
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		data, _ := io.ReadAll(r.Body)
		mu.Lock()
		bodies, headers = append(bodies, string(data)), append(headers, r.Header.Clone())
		failing := len(bodies) > 1
		mu.Unlock()
		if failing {
			http.Error(w, "nope", http.StatusInternalServerError)
			return
		}
		_, _ = io.WriteString(w, "ok")
	}))
	t.Cleanup(server.Close)
	f := newPowerFixture(t, "hooks:\n  - id: post\n    on: turn.done\n    webhook:\n      url: "+server.URL+"\n      headers: { Authorization: Bearer s3cret }\n")
	actions, err := f.client.GetUserActions(f.owner(t), &pb.GetUserActionsRequest{})
	if err != nil || actions.Hooks[0].Headers["Authorization"] != "••••" {
		t.Fatalf("header values are shown: %+v %v", actions, err)
	}
	if _, err = f.client.TestHook(f.owner(t), &pb.TestHookRequest{HookId: "post"}); err != nil {
		t.Fatal(err)
	}
	runs := f.runs(t, settled(1))
	if runs[0].State != power.RunSucceeded || runs[0].HttpStatus != 200 {
		t.Fatalf("webhook run: %+v", runs[0])
	}
	mu.Lock()
	var event map[string]any
	if err = json.Unmarshal([]byte(bodies[0]), &event); err != nil || event["event"] != "turn.done" || event["test"] != true || headers[0].Get("Authorization") != "Bearer s3cret" || headers[0].Get("X-Lazybox-Event") != "turn.done" {
		t.Fatalf("posted %s %v %v", bodies[0], headers[0], err)
	}
	mu.Unlock()
	read, err := f.client.ReadCommand(f.owner(t), &pb.ReadCommandRequest{CommandId: runs[0].CommandId, Limit: 8192})
	if err != nil || !strings.Contains(string(read.Data), "POST "+server.URL) || !strings.Contains(string(read.Data), "200 OK") {
		t.Fatalf("webhook log: %q %v", read.GetData(), err)
	}
	if _, err = f.client.TestHook(f.owner(t), &pb.TestHookRequest{HookId: "post"}); err != nil {
		t.Fatal(err)
	}
	runs = f.runs(t, settled(2))
	if runs[0].State != power.RunFailed || runs[0].HttpStatus != 500 {
		t.Fatalf("failing webhook: %+v", runs[0])
	}
	if _, err = f.client.TestHook(f.owner(t), &pb.TestHookRequest{HookId: "missing"}); status.Code(err) != codes.NotFound {
		t.Fatalf("missing hook: %v", err)
	}
}

// The actions file round trip: issues by line, a broken save keeps the last
// good version, Init never replaces a file.
func TestUserActionsRPCs(t *testing.T) {
	f := newPowerFixture(t, "actions:\n  - id: go\n    key: g\n    steps:\n      - send: { snippet: review, to: new }\n      - when: failed\n        open: changes\n")
	got, err := f.client.GetUserActions(f.owner(t), &pb.GetUserActionsRequest{})
	if err != nil || len(got.Actions) != 1 || got.Actions[0].Steps[0].Snippet != "review" || got.Actions[0].Steps[1].Panel != "changes" || got.Schema == "" || len(got.Events) != len(power.Events) {
		t.Fatalf("actions: %+v %v", got, err)
	}
	path := f.service.Bench.Actions().Path()
	if err = os.WriteFile(path, []byte("actions:\n  - id: go\n    steps:\n      - send: hi\n  - id: bad\n    steps:\n      - fly: x\nhooks: [\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	broken, err := f.client.GetUserActions(f.owner(t), &pb.GetUserActionsRequest{})
	if err != nil || broken.Error == "" || !broken.Stale || len(broken.Actions) != 1 || broken.Actions[0].Key != "g" || broken.Revision <= got.Revision {
		t.Fatalf("broken file: %+v %v", broken, err)
	}
	if err = os.WriteFile(path, []byte("actions:\n  - id: go\n    steps:\n      - send: hi\n  - id: bad\n    steps:\n      - fly: x\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	fixed, _ := f.client.GetUserActions(f.owner(t), &pb.GetUserActionsRequest{})
	if fixed.Stale || len(fixed.Actions) != 1 || len(fixed.Issues) == 0 || fixed.Issues[0].Line != 7 || fixed.Issues[0].Severity != "error" {
		t.Fatalf("issues: %+v", fixed)
	}
	init, err := f.client.InitUserActions(f.owner(t), &pb.GetUserActionsRequest{})
	if err != nil || len(init.Actions) != 1 || init.Actions[0].Id != "go" {
		t.Fatalf("init replaced the file: %+v %v", init, err)
	}
	if _, err = os.Stat(init.SchemaPath); err != nil {
		t.Fatal("schema not written", err)
	}
}

// The CLI's requests reach the window that follows them, and its answer comes back.
func TestUiRequestsRoundTrip(t *testing.T) {
	f := newPowerFixture(t, "")
	ctx := f.owner(t)
	if _, err := f.client.PostUiRequest(ctx, &pb.UiRequest{Kind: "open", SessionId: f.session}); status.Code(err) != codes.Unavailable {
		t.Fatalf("no window: %v", err)
	}
	watch, err := f.client.WatchUiRequests(ctx, &pb.WatchUiRequestsRequest{Surface: "desktop"})
	if err != nil {
		t.Fatal(err)
	}
	if first, err := watch.Recv(); err != nil || first.Id != "" {
		t.Fatalf("registration: %+v %v", first, err)
	}
	if _, err = f.client.PostUiRequest(ctx, &pb.UiRequest{Kind: "action", ActionId: "nope"}); status.Code(err) != codes.NotFound {
		t.Fatalf("unknown action: %v", err)
	}
	posted, err := f.client.PostUiRequest(ctx, &pb.UiRequest{Kind: "shot", Panel: "changes"})
	if err != nil {
		t.Fatal(err)
	}
	got, err := watch.Recv()
	if err != nil || got.Id != posted.Id || got.Kind != "shot" || got.Panel != "changes" {
		t.Fatalf("delivered %+v %v", got, err)
	}
	if _, err = f.client.CompleteUiRequest(ctx, &pb.CompleteUiRequestRequest{Id: got.Id, Png: []byte("\x89PNG"), Detail: "Captured Changes"}); err != nil {
		t.Fatal(err)
	}
	done, err := f.client.WaitUiRequest(ctx, &pb.WaitUiRequestRequest{Id: posted.Id, TimeoutMs: 1000})
	if err != nil || done.State != "done" || string(done.Png) != "\x89PNG" || done.Detail != "Captured Changes" {
		t.Fatalf("outcome: %+v %v", done, err)
	}
}
