package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"connectrpc.com/connect"
	"connectrpc.com/vanguard/vanguardgrpc"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/transport"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
)

const testOwnerToken = "owner-cli-fixture-token"

// fakeProvider opens a fresh fake native session per conversation.
type fakeProvider struct {
	mu       sync.Mutex
	sessions []*agenttest.Session
	opened   chan *agenttest.Session
}

func (p *fakeProvider) Open(_ context.Context, o agent.Open) (agent.Session, error) {
	s := &agenttest.Session{Open: o, Ref: "native-" + o.Profile, Events: make(chan agent.Event, 128), Applied: make(chan agent.Operation, 128), StopAgent: true}
	p.mu.Lock()
	p.sessions = append(p.sessions, s)
	p.mu.Unlock()
	p.opened <- s
	return s, nil
}

// testRunner is a real Runner behind the production interceptors, served
// over Connect on loopback, with an owner endpoint file and a config home.
type testRunner struct {
	t          *testing.T
	bench      *runner.Bench
	service    *transport.Service
	url        string
	endpoint   string
	home       string
	provider   *fakeProvider
	collection *pb.Collection
}

func newTestRunner(t *testing.T) *testRunner {
	t.Helper()
	t.Setenv("CLAUDE_CONFIG_DIR", t.TempDir())
	repo := t.TempDir()
	for _, args := range [][]string{{"init", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		if out, err := exec.Command("git", append([]string{"-C", repo}, args...)...).CombinedOutput(); err != nil {
			t.Fatalf("git: %v %s", err, out)
		}
	}
	var handler http.Handler = http.NotFoundHandler()
	var mu sync.RWMutex
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.RLock()
		h := handler
		mu.RUnlock()
		h.ServeHTTP(w, r)
	}))
	t.Cleanup(server.Close)
	state := t.TempDir()
	b, err := runner.New(runner.Config{State: state, OutputFallbackRoot: t.TempDir(), WorktreeRoot: t.TempDir(), Address: server.URL,
		Repositories: []collections.Repository{{ID: "api", Path: repo}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	provider := &fakeProvider{opened: make(chan *agenttest.Session, 16)}
	b.Adapters["claude"] = provider
	b.Adapters["codex"] = provider
	grpcServer := grpc.NewServer(transport.LocalAuthority(testOwnerToken, b)...)
	service := &transport.Service{Bench: b}
	pb.RegisterRunnerServiceServer(grpcServer, service)
	transcoder, err := vanguardgrpc.NewTranscoder(grpcServer)
	if err != nil {
		t.Fatal(err)
	}
	mu.Lock()
	handler = transcoder
	mu.Unlock()
	endpoint := filepath.Join(state, "endpoint.json")
	data, _ := json.Marshal(map[string]string{"url": server.URL, "token": testOwnerToken})
	if err = os.WriteFile(endpoint, data, 0o600); err != nil {
		t.Fatal(err)
	}
	r := &testRunner{t: t, bench: b, service: service, url: server.URL, endpoint: endpoint, home: t.TempDir(), provider: provider}
	created, err := service.CreateCollection(context.Background(), &pb.CreateCollectionRequest{RequestKey: "alpha", Spec: &pb.CollectionSpec{Name: "Alpha feature", Members: []*pb.WorktreeSpec{{Name: "api", RepositoryId: "api"}}}})
	if err != nil {
		t.Fatal(err)
	}
	r.collection = created.Collection
	return r
}

// open starts a conversation in the workspace and returns its fake native session.
func (r *testRunner) open(title string) (*pb.Session, *agenttest.Session) {
	r.t.Helper()
	opened, err := r.service.OpenSession(context.Background(), &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: r.collection.Id, Options: &pb.RunnerOptions{NoRotation: true}})
	if err != nil {
		r.t.Fatal(err)
	}
	native := <-r.provider.opened
	if title != "" {
		if _, err = r.service.RenameConversation(context.Background(), &pb.RenameConversationRequest{SessionId: opened.Session.Id, Title: title}); err != nil {
			r.t.Fatal(err)
		}
	}
	return opened.Session, native
}

// register writes a Runner locator, as the launchers do.
func (r *testRunner) register(name string) {
	r.t.Helper()
	dir := filepath.Join(r.home, ".config", "lazybox", "runners")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		r.t.Fatal(err)
	}
	data, _ := json.Marshal(locator{Name: name, Endpoint: r.endpoint})
	if err := os.WriteFile(filepath.Join(dir, name+".json"), data, 0o600); err != nil {
		r.t.Fatal(err)
	}
}

type result struct {
	stdout, stderr string
	code           int
	err            error
}

// cliRun runs a power verb as the binary would, with env as its whole environment.
func (r *testRunner) cli(env map[string]string, stdin string, args ...string) result {
	r.t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	return runPower(ctx, r.t, r.home, env, stdin, args...)
}

func runPower(ctx context.Context, t *testing.T, home string, env map[string]string, stdin string, args ...string) result {
	t.Helper()
	var stdout, stderr bytes.Buffer
	getenv := func(name string) string {
		if name == "HOME" {
			return home
		}
		return env[name]
	}
	c := &cli{ctx: ctx, stdin: strings.NewReader(stdin), stdout: &stdout, stderr: &stderr, getenv: getenv, now: time.Now, poll: 20 * time.Millisecond, stdinTTY: stdin == ""}
	handled, err := powerVerb(c, args)
	if !handled {
		t.Fatalf("%v is not a power verb", args)
	}
	code := 0
	if err != nil {
		code = exitCode(err)
		var coded exitError
		if errors.As(err, &coded) && coded.code == exitOK {
			code = 0
		}
	}
	return result{stdout.String(), stderr.String(), code, err}
}

// owner is the environment of a person's own terminal with --endpoint.
func (r *testRunner) owner() map[string]string {
	return map[string]string{"LAZYBOX_V2_ENDPOINT": r.endpoint}
}

func until(t *testing.T, what string, ok func() bool) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for !ok() {
		if time.Now().After(deadline) {
			t.Fatal(what)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func (r *testRunner) session(id string) *pb.Session {
	got, err := r.service.GetSession(context.Background(), &pb.SessionRequest{SessionId: id})
	if err != nil {
		r.t.Fatal(err)
	}
	return got.Session
}

func TestLsListsWorkspacesConversationsAndLiveState(t *testing.T) {
	r := newTestRunner(t)
	s, native := r.open("fix the tests")
	native.Events <- agent.Event{Type: "approval", RequestID: "req-1", Text: "npm test"}
	until(t, "approval never pending", func() bool { return len(r.session(s.Id).Pending) == 1 })
	out := r.cli(r.owner(), "", "ls")
	if out.code != 0 || !strings.Contains(out.stdout, "Alpha feature") || !strings.Contains(out.stdout, "fix the tests") || !strings.Contains(out.stdout, "needs input") || !strings.Contains(out.stdout, "Approval: npm test") {
		t.Fatalf("ls: %+v", out)
	}
	out = r.cli(r.owner(), "", "ls", "--json")
	var listing jsonListing
	if err := json.Unmarshal([]byte(out.stdout), &listing); err != nil {
		t.Fatal(err, out.stdout)
	}
	if len(listing.Workspaces) != 1 || len(listing.Workspaces[0].Conversations) != 1 {
		t.Fatalf("json: %+v", listing)
	}
	got := listing.Workspaces[0].Conversations[0]
	if got.State != stateNeedsInput || got.Session != s.Id || len(got.Pending) != 1 || !strings.HasSuffix(got.Pending[0].ID, "req-1") || got.Title != "fix the tests" {
		t.Fatalf("conversation: %+v", got)
	}
	// The inbox orders what needs the person first.
	out = r.cli(r.owner(), "", "inbox", "--json")
	if !strings.Contains(out.stdout, `"state": "needs-input"`) {
		t.Fatalf("inbox: %+v", out)
	}
}

func TestSendResolvesByTitleReadsStdinAndWaitFollowsTheTurn(t *testing.T) {
	r := newTestRunner(t)
	s, native := r.open("refactor parser")
	out := r.cli(r.owner(), "", "send", "refactor", "please", "continue")
	if out.code != 0 || !strings.Contains(out.stdout, "Sent to “refactor parser”") {
		t.Fatalf("send: %+v", out)
	}
	op := <-native.Applied
	if op.Kind != agent.Message || op.Text != "please continue" {
		t.Fatalf("delivered %+v", op)
	}
	// From stdin, with workspace/title.
	out = r.cli(r.owner(), "line one\nline two\n", "send", "Alpha feature/refactor parser", "-")
	if out.code != 0 {
		t.Fatalf("send stdin: %+v", out)
	}
	native.Events <- agent.Event{Type: "turn_started"}
	// wait --until idle blocks while the turn runs, and returns once it ends.
	done := make(chan result, 1)
	go func() { done <- r.cli(r.owner(), "", "wait", s.Id, "--until", "idle", "--timeout", "10s") }()
	select {
	case got := <-done:
		t.Fatalf("wait returned during the turn: %+v", got)
	case <-time.After(300 * time.Millisecond):
	}
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	op = <-native.Applied
	native.Events <- agent.Event{Type: "turn_started"}
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	got := <-done
	if got.code != 0 || !strings.Contains(got.stdout, "is idle") || op.Text != "line one\nline two\n" {
		t.Fatalf("wait: %+v (second message %q)", got, op.Text)
	}
	// A timeout is exit 5.
	native.Events <- agent.Event{Type: "turn_started"}
	until(t, "turn never started", func() bool { return r.session(s.Id).Busy })
	if got = r.cli(r.owner(), "", "wait", s.Id, "--until", "needs-input", "--timeout", "200ms"); got.code != exitTimeout {
		t.Fatalf("timeout: %+v", got)
	}
	// Unknown and ambiguous names are not found (3) and usage (2).
	if got = r.cli(r.owner(), "", "send", "nothing-like-this", "x"); got.code != exitNotFound {
		t.Fatalf("unknown: %+v", got)
	}
	if got = r.cli(r.owner(), "", "send", "refactor"); got.code != exitUsage {
		t.Fatalf("usage: %+v", got)
	}
}

func TestApproveDenyAndStop(t *testing.T) {
	r := newTestRunner(t)
	s, native := r.open("deploy")
	native.Events <- agent.Event{Type: "approval", RequestID: "req-ok", Text: "kubectl apply"}
	until(t, "pending", func() bool { return len(r.session(s.Id).Pending) == 1 })
	out := r.cli(r.owner(), "", "approve", "req-ok")
	if out.code != 0 || !strings.Contains(out.stdout, "Approved in “deploy”") {
		t.Fatalf("approve: %+v", out)
	}
	op := <-native.Applied
	if op.Kind != agent.Approve || op.RequestID != "req-ok" || op.Decision != "accept" {
		t.Fatalf("approval delivered as %+v", op)
	}
	native.Events <- agent.Event{Type: "request_resolved", RequestID: "req-ok"}
	native.Events <- agent.Event{Type: "approval", RequestID: "req-no", Text: "rm -rf build"}
	until(t, "second pending", func() bool {
		p := r.session(s.Id).Pending
		return len(p) == 1 && strings.HasSuffix(p[0].RequestId, "req-no")
	})
	// By conversation, when it has exactly one approval waiting.
	if out = r.cli(r.owner(), "", "deny", "deploy"); out.code != 0 {
		t.Fatalf("deny: %+v", out)
	}
	if op = <-native.Applied; op.Kind != agent.Approve || op.Decision != "decline" {
		t.Fatalf("denial delivered as %+v", op)
	}
	if out = r.cli(r.owner(), "", "approve", "no-such-request"); out.code != exitNotFound {
		t.Fatalf("missing request: %+v", out)
	}
	// stop interrupts a running turn.
	native.Events <- agent.Event{Type: "turn_started"}
	until(t, "busy", func() bool { return r.session(s.Id).Busy })
	if out = r.cli(r.owner(), "", "stop", "deploy"); out.code != 0 || !strings.Contains(out.stdout, "Interrupted") {
		t.Fatalf("stop: %+v", out)
	}
	if op = <-native.Applied; op.Kind != agent.Interrupt {
		t.Fatalf("stop delivered as %+v", op)
	}
	// An unknown name that is not a command either is not found.
	if out = r.cli(r.owner(), "", "stop", "zzzzzz-unknown"); out.code != exitNotFound {
		t.Fatalf("stop unknown: %+v", out)
	}
}

func TestOpenShotAndActionsRunGoThroughTheWindow(t *testing.T) {
	r := newTestRunner(t)
	s, _ := r.open("review")
	// No window open: exit 6 with the Runner's words.
	if out := r.cli(r.owner(), "", "open", "review"); out.code != exitUnavailable || !strings.Contains(out.err.Error(), "no Lazybox window") {
		t.Fatalf("no window: %+v", out)
	}
	requests, stop := r.bench.UI().Watch("desktop")
	defer stop()
	seen := make(chan string, 8)
	go func() {
		for req := range requests {
			seen <- req.Kind + " " + req.SessionID + " " + req.CollectionID + " " + req.File + " " + req.Panel + " " + req.ActionID
			png := []byte(nil)
			if req.Kind == "shot" {
				png = []byte("\x89PNG fixture")
			}
			_, _ = r.bench.UI().Complete(req.ID, "", "Showed it", png)
		}
	}()
	out := r.cli(r.owner(), "", "open", "review", "--panel", "changes")
	if out.code != 0 || !strings.Contains(out.stdout, "Showed it") {
		t.Fatalf("open: %+v", out)
	}
	if got := <-seen; got != "open "+s.Id+" "+r.collection.Id+"  changes " {
		t.Fatalf("window got %q", got)
	}
	if out = r.cli(r.owner(), "", "open", "Alpha feature"); out.code != 0 || <-seen != "open  "+r.collection.Id+"   " {
		t.Fatalf("open workspace: %+v", out)
	}
	if out = r.cli(r.owner(), "", "open", "api/README.md:12"); out.code != 0 || <-seen != "open  "+r.collection.Id+" api/README.md  " {
		t.Fatalf("open file: %+v", out)
	}
	file := filepath.Join(t.TempDir(), "shot.png")
	out = r.cli(r.owner(), "", "shot", "changes", "--out", file)
	if out.code != 0 || !strings.Contains(out.stdout, "shot.png") || <-seen != "shot    changes " {
		t.Fatalf("shot: %+v", out)
	}
	if data, _ := os.ReadFile(file); string(data) != "\x89PNG fixture" {
		t.Fatalf("png: %q", data)
	}
	if out = r.cli(r.owner(), "", "shot", "nonsense"); out.code != exitUsage {
		t.Fatalf("bad panel: %+v", out)
	}
	// actions run: the action must exist in the file.
	if err := os.WriteFile(r.bench.Actions().Path(), []byte("actions:\n  - id: go-on\n    steps:\n      - send: Continue.\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	r.bench.Actions().Reload() // the Runner rereads it every second
	out = r.cli(r.owner(), "", "actions", "run", "go-on", "--conversation", "review")
	if out.code != 0 || <-seen != "action "+s.Id+" "+r.collection.Id+"   go-on" {
		t.Fatalf("actions run: %+v", out)
	}
	if out = r.cli(r.owner(), "", "actions", "run", "missing"); out.code != exitNotFound {
		t.Fatalf("missing action: %+v", out)
	}
}

func TestActionsAndHooksVerbs(t *testing.T) {
	r := newTestRunner(t)
	path := r.bench.Actions().Path()
	// init creates the file and its schema; validate checks it locally.
	out := r.cli(r.owner(), "", "actions", "init")
	if out.code != 0 || strings.TrimSpace(out.stdout) != path {
		t.Fatalf("init: %+v", out)
	}
	if _, err := os.Stat(filepath.Join(filepath.Dir(path), "actions.schema.json")); err != nil {
		t.Fatal("no schema written", err)
	}
	if out = r.cli(r.owner(), "", "actions", "validate"); out.code != 0 || !strings.Contains(out.stdout, "✓") {
		t.Fatalf("validate starter: %+v", out)
	}
	marker := filepath.Join(t.TempDir(), "ran")
	file := "actions:\n  - id: go-on\n    key: g\n    steps:\n      - send: Continue.\nhooks:\n  - id: mark\n    on: turn.done\n    shell: echo \"$LAZYBOX_EVENT\" > " + marker + "; echo hook-output\n"
	if err := os.WriteFile(path, []byte(file), 0o600); err != nil {
		t.Fatal(err)
	}
	out = r.cli(r.owner(), "", "actions")
	if out.code != 0 || !strings.Contains(out.stdout, "go-on") || !strings.Contains(out.stdout, "]]g") || !strings.Contains(out.stdout, "mark") {
		t.Fatalf("actions ls: %+v", out)
	}
	bad := filepath.Join(t.TempDir(), "bad.yaml")
	_ = os.WriteFile(bad, []byte("actions:\n  - id: Bad Id\n    steps: []\n"), 0o600)
	out = r.cli(r.owner(), "", "actions", "validate", bad)
	if out.code != exitUsage || !strings.Contains(out.stdout, bad+":2:") {
		t.Fatalf("validate bad: %+v", out)
	}
	if out = r.cli(nil, "", "actions", "schema"); out.code != 0 || !strings.Contains(out.stdout, "json-schema.org") {
		t.Fatalf("schema: %+v", out)
	}
	// hooks: list, test (it runs, with its output), runs, log.
	r.open("hooked")
	if out = r.cli(r.owner(), "", "hooks"); out.code != 0 || !strings.Contains(out.stdout, "on turn.done") {
		t.Fatalf("hooks ls: %+v", out)
	}
	out = r.cli(r.owner(), "", "hooks", "test", "mark")
	if out.code != 0 || !strings.Contains(out.stdout, "succeeded") || !strings.Contains(out.stdout, "hook-output") {
		t.Fatalf("hooks test: %+v", out)
	}
	if data, _ := os.ReadFile(marker); strings.TrimSpace(string(data)) != "turn.done" {
		t.Fatalf("hook saw %q", data)
	}
	out = r.cli(r.owner(), "", "hooks", "runs", "--json")
	var runs struct {
		Runs []struct{ Id, State string } `json:"runs"`
	}
	if err := json.Unmarshal([]byte(out.stdout), &runs); err != nil || len(runs.Runs) != 1 || runs.Runs[0].State != "succeeded" {
		t.Fatalf("runs: %v %+v", err, out)
	}
	if out = r.cli(r.owner(), "", "hooks", "log", runs.Runs[0].Id); !strings.Contains(out.stdout, "hook-output") {
		t.Fatalf("log: %+v", out)
	}
	if out = r.cli(r.owner(), "", "hooks", "test", "nope"); out.code != exitNotFound {
		t.Fatalf("missing hook: %+v", out)
	}
}

func TestMissionsVerb(t *testing.T) {
	r := newTestRunner(t)
	out := r.cli(r.owner(), "", "missions", "new", "Ship", "the", "parser", "--brief", "Make it fast")
	if out.code != 0 {
		t.Fatalf("new: %+v", out)
	}
	id := strings.TrimSpace(out.stdout)
	if out = r.cli(r.owner(), "", "missions"); !strings.Contains(out.stdout, "Ship the parser") || !strings.Contains(out.stdout, id) {
		t.Fatalf("ls: %+v", out)
	}
	if out = r.cli(r.owner(), "", "missions", "show", id, "--json"); !strings.Contains(out.stdout, "Make it fast") {
		t.Fatalf("show: %+v", out)
	}
	if out = r.cli(r.owner(), "", "missions", "accept", id, "no-task"); out.code == 0 {
		t.Fatalf("accept of a missing task succeeded: %+v", out)
	}
	if out = r.cli(r.owner(), "", "missions", "show", "nope"); out.code != exitNotFound {
		t.Fatalf("show missing: %+v", out)
	}
}

func TestNewCreatesAWorkspaceFromTheCatalog(t *testing.T) {
	r := newTestRunner(t)
	out := r.cli(r.owner(), "", "new", "Beta", "--repo", "api", "--branch", "lazybox/beta")
	if out.code == exitNotFound {
		t.Skipf("the fixture catalog does not list the configured repository: %+v", out)
	}
	if out.code != 0 || strings.TrimSpace(out.stdout) == "" {
		t.Fatalf("new: %+v", out)
	}
	if ls := r.cli(r.owner(), "", "ls"); !strings.Contains(ls.stdout, "Beta") {
		t.Fatalf("ls after new: %+v", ls)
	}
	if out = r.cli(r.owner(), "", "new", "Gamma", "--repo", "not-a-repo"); out.code != exitNotFound {
		t.Fatalf("unknown repo: %+v", out)
	}
	if out = r.cli(r.owner(), "", "new", "Gamma"); out.code != exitUsage {
		t.Fatalf("no repo: %+v", out)
	}
}

func TestWatchStreamsATimelineAndJSON(t *testing.T) {
	r := newTestRunner(t)
	s, native := r.open("streamed")
	native.Events <- agent.Event{Type: "turn_started"}
	native.Events <- agent.Event{Type: "message_delta", Text: "Hello "}
	native.Events <- agent.Event{Type: "message_delta", Text: "world"}
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	until(t, "turn never ended", func() bool { return !r.session(s.Id).Busy })
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan result, 1)
	go func() { done <- runPower(ctx, t, r.home, r.owner(), "", "watch", "streamed") }()
	time.Sleep(500 * time.Millisecond)
	native.Events <- agent.Event{Type: "approval", RequestID: "req-live", Text: "go test ./..."}
	time.Sleep(500 * time.Millisecond)
	cancel()
	out := <-done
	if out.code != 0 {
		t.Fatalf("watch: %+v", out)
	}
	for _, want := range []string{"turn_started", "“Hello world”", "turn_ended", "completed", "approval", "go test ./...", "req-live"} {
		if !strings.Contains(out.stdout, want) {
			t.Fatalf("timeline lacks %q:\n%s", want, out.stdout)
		}
	}
	if strings.Contains(out.stdout, "message_delta") || strings.Contains(out.stdout, "\x1b[") {
		t.Fatalf("deltas not coalesced, or colour on a pipe:\n%s", out.stdout)
	}
	// NDJSON from a cursor, every record.
	ctx, cancel = context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	out = runPower(ctx, t, r.home, r.owner(), "", "watch", s.Id, "--json", "--since", "0")
	lines := strings.Split(strings.TrimSpace(out.stdout), "\n")
	var deltas, approvals int
	for _, line := range lines {
		var rec record
		if err := json.Unmarshal([]byte(line), &rec); err != nil {
			t.Fatalf("not NDJSON: %q", line)
		}
		if rec.Session != s.Id || rec.Cursor == 0 {
			t.Fatalf("record: %+v", rec)
		}
		switch rec.Type {
		case "message_delta":
			deltas++
		case "approval":
			approvals++
		}
	}
	if deltas != 2 || approvals != 1 {
		t.Fatalf("NDJSON: %d deltas, %d approvals\n%s", deltas, approvals, out.stdout)
	}
	// A workspace follows its conversations, labelled.
	ctx, cancel = context.WithTimeout(context.Background(), 800*time.Millisecond)
	defer cancel()
	out = runPower(ctx, t, r.home, r.owner(), "", "watch", "Alpha feature", "--since", "0")
	if !strings.Contains(out.stdout, "streamed") || !strings.Contains(out.stdout, "turn_ended") {
		t.Fatalf("workspace watch: %+v", out)
	}
	if out = r.cli(r.owner(), "", "watch", "x", "--since", "yesterday"); out.code != exitUsage {
		t.Fatalf("bad since: %+v", out)
	}
}

func TestDebugExplainsAConversation(t *testing.T) {
	r := newTestRunner(t)
	s, native := r.open("stuck one")
	native.Events <- agent.Event{Type: "turn_started"}
	until(t, "busy", func() bool { return r.session(s.Id).Busy })
	if out := r.cli(r.owner(), "", "stop", s.Id); out.code != 0 {
		t.Fatalf("stop: %+v", out)
	}
	<-native.Applied
	native.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
	native.Events <- agent.Event{Type: "approval", RequestID: "req-d", Text: "make release"}
	until(t, "pending", func() bool { return len(r.session(s.Id).Pending) == 1 })
	out := r.cli(r.owner(), "", "debug", "stuck one")
	if out.code != 0 {
		t.Fatalf("debug: %+v", out)
	}
	for _, want := range []string{"Recent lifecycle", "turn_started", "accepted control", "turn_ended", "Findings", "the last turn ended by an interrupt you sent at", "blocked on your approval: make release", "req-d (or deny)"} {
		if !strings.Contains(out.stdout, want) {
			t.Fatalf("debug lacks %q:\n%s", want, out.stdout)
		}
	}
	out = r.cli(r.owner(), "", "debug", s.Id, "--json")
	var d diagnosis
	if err := json.Unmarshal([]byte(out.stdout), &d); err != nil {
		t.Fatal(err, out.stdout)
	}
	if d.Session != s.Id || len(d.Pending) != 1 || len(d.Findings) < 2 || len(d.Events) == 0 {
		t.Fatalf("diagnosis: %+v", d)
	}
	// A quiet, healthy conversation says so.
	idle, _ := r.open("quiet")
	if out = r.cli(r.owner(), "", "debug", idle.Id); !strings.Contains(out.stdout, "nothing looks wrong") {
		t.Fatalf("idle debug: %+v", out)
	}
}

func TestEndpointResolution(t *testing.T) {
	r := newTestRunner(t)
	// No endpoint and nothing registered: exit 6.
	if out := r.cli(map[string]string{}, "", "ls"); out.code != exitUnavailable {
		t.Fatalf("nothing registered: %+v", out)
	}
	// One registered Runner is used from any terminal.
	r.register("dev")
	if out := r.cli(map[string]string{}, "", "ls"); out.code != 0 || !strings.Contains(out.stdout, "Alpha feature") {
		t.Fatalf("registered: %+v", out)
	}
	// Several: working is the default; another needs --runner.
	other := filepath.Join(t.TempDir(), "endpoint.json")
	_ = os.WriteFile(other, []byte(`{"url":"http://127.0.0.1:1","token":"x"}`), 0o600)
	dir := filepath.Join(r.home, ".config", "lazybox", "runners")
	data, _ := json.Marshal(locator{Name: "staging", Endpoint: other})
	_ = os.WriteFile(filepath.Join(dir, "staging.json"), data, 0o600)
	if out := r.cli(map[string]string{}, "", "ls"); out.code != exitUsage || !strings.Contains(out.err.Error(), "dev, staging") {
		t.Fatalf("ambiguous: %+v", out)
	}
	if out := r.cli(map[string]string{"LAZYBOX_RUNNER": "dev"}, "", "ls"); out.code != 0 {
		t.Fatalf("LAZYBOX_RUNNER: %+v", out)
	}
	r.register("working")
	if out := r.cli(map[string]string{}, "", "ls"); out.code != 0 {
		t.Fatalf("working default: %+v", out)
	}
	// An unreachable Runner is exit 6, and says where it looked.
	if out := r.cli(map[string]string{}, "", "ls", "--runner", "staging"); out.code != exitUnavailable || !strings.Contains(out.err.Error(), "127.0.0.1:1") {
		t.Fatalf("unreachable: %+v", out)
	}
	// runners lists them, live or not.
	out := r.cli(map[string]string{}, "", "runners", "--json")
	if out.code != 0 || !strings.Contains(out.stdout, `"name": "staging"`) || !strings.Contains(out.stdout, `"live": true`) {
		t.Fatalf("runners: %+v", out)
	}
	// --all groups every Runner and reports the dead one on stderr.
	out = r.cli(map[string]string{}, "", "ls", "--all")
	if out.code != 0 || strings.Count(out.stdout, "Alpha feature") != 2 || !strings.Contains(out.stderr, "staging") {
		t.Fatalf("ls --all: %+v", out)
	}
	// Inside an agent or a workspace terminal, the registry is never used implicitly.
	for _, scoped := range []string{"LAZYBOX_V2_SESSION_ID", "LAZYBOX_V2_COLLECTION_ID"} {
		if out = r.cli(map[string]string{scoped: "x"}, "", "ls"); out.code != exitUnavailable || !strings.Contains(out.err.Error(), "inside Lazybox") {
			t.Fatalf("%s fell back to the owner: %+v", scoped, out)
		}
	}
	// …but an explicit --runner is the person's choice.
	if out = r.cli(map[string]string{"LAZYBOX_V2_SESSION_ID": "x"}, "", "ls", "--runner", "dev"); out.code != 0 {
		t.Fatalf("explicit runner: %+v", out)
	}
}

func TestAgentCredentialIsDeniedOwnerVerbs(t *testing.T) {
	r := newTestRunner(t)
	_, native := r.open("agent")
	endpoint := ""
	for _, entry := range native.Open.Env {
		if v, ok := strings.CutPrefix(entry, "LAZYBOX_V2_ENDPOINT="); ok {
			endpoint = v
		}
	}
	if endpoint == "" {
		t.Fatal("agent has no endpoint")
	}
	env := map[string]string{"LAZYBOX_V2_ENDPOINT": endpoint, "LAZYBOX_V2_SESSION_ID": "x"}
	// `ls` lists the agent's own commands (agent_commands_test.go); here the session is not its own.
	for _, args := range [][]string{{"ls"}, {"send", "agent", "hi"}, {"inbox"}, {"hooks", "runs"}, {"actions"}} {
		out := r.cli(env, "", args...)
		// One line: what was refused and the next step, no paragraph of what agents may do.
		if out.code != exitDenied || !strings.HasPrefix(out.err.Error(), "refused: ") || strings.Contains(out.err.Error(), "\n") {
			t.Fatalf("%v with an agent credential: %+v", args, out)
		}
	}
}

func TestHostRefusalIsExplained(t *testing.T) {
	server := httptest.NewServer(http.NotFoundHandler())
	defer server.Close()
	endpoint := filepath.Join(t.TempDir(), "endpoint.json")
	_ = os.WriteFile(endpoint, []byte(`{"url":"`+server.URL+`","token":"t"}`), 0o600)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	out := runPower(ctx, t, t.TempDir(), map[string]string{"LAZYBOX_V2_ENDPOINT": endpoint}, "", "inbox")
	if out.code != exitUnavailable || !strings.Contains(out.err.Error(), "refused the request's Host") || !strings.Contains(out.err.Error(), "localhost") {
		t.Fatalf("404: %+v", out)
	}
}

func TestExitCodesFollowTheRunnersCodes(t *testing.T) {
	c := &client{target: target{URL: "http://localhost:1", EndpointPath: "/e.json"}}
	for _, tc := range []struct {
		err  error
		code int
	}{
		{context.DeadlineExceeded, exitTimeout},
		{errors.New("plain"), exitFailure},
		{fail(exitNotFound, "x"), exitNotFound},
	} {
		if got := exitCode(c.explain(tc.err)); got != tc.code {
			t.Errorf("%v: exit %d, want %d", tc.err, got, tc.code)
		}
	}
}

func TestLegacyFormsStayWithMain(t *testing.T) {
	for _, args := range [][]string{
		{"send", "--session", "s", "--text", "x"},
		{"open", "--cwd", "/tmp"},
		{"open"},
		{"approve", "--request", "r", "--decision", "accept"},
		{"wait", "--id", "c"},
		{"run", "--", "true"},
		{"events"},
	} {
		v := findVerb(args[0])
		if v != nil && (v.legacy == nil || !v.legacy(args[1:])) {
			t.Errorf("%v would be taken by the new form", args)
		}
	}
	for _, args := range [][]string{{"send", "conv", "hi"}, {"open", "alpha"}, {"approve", "req"}, {"wait", "conv", "--until", "idle"}} {
		if v := findVerb(args[0]); v.legacy != nil && v.legacy(args[1:]) {
			t.Errorf("%v would go to the legacy form", args)
		}
	}
}

func TestCompletionScriptsCoverEveryVerbAndParse(t *testing.T) {
	for _, shell := range []string{"zsh", "bash", "fish"} {
		script, err := completionScript(shell)
		if err != nil {
			t.Fatal(err)
		}
		for _, v := range completionVerbs() {
			if !strings.Contains(script, v[0]) {
				t.Errorf("%s completion lacks %s", shell, v[0])
			}
		}
		for _, v := range verbs {
			for _, f := range v.Flags {
				if !strings.Contains(script, strings.TrimPrefix(flagName(f), "--")) {
					t.Errorf("%s completion lacks %s %s", shell, v.Name, f)
				}
			}
		}
		binary, err := exec.LookPath(shell)
		if err != nil {
			t.Logf("%s not installed; syntax not checked", shell)
			continue
		}
		path := filepath.Join(t.TempDir(), "lazybox."+shell)
		if err = os.WriteFile(path, []byte(script), 0o600); err != nil {
			t.Fatal(err)
		}
		if out, err := exec.Command(binary, "-n", path).CombinedOutput(); err != nil {
			t.Errorf("%s -n: %v\n%s", shell, err, out)
		}
	}
	if _, err := completionScript("powershell"); exitCode(err) != exitUsage {
		t.Fatal("unknown shell accepted")
	}
}

func TestRunnerConnectionFailurePreservesCause(t *testing.T) {
	c := &client{target: target{URL: "http://localhost:1234", EndpointPath: "agent-endpoint.json"}}
	for _, cause := range []string{"dial tcp: operation not permitted", "dial tcp: connection refused"} {
		err := c.explain(connect.NewError(connect.CodeUnavailable, errors.New(cause)))
		var failure exitError
		if !errors.As(err, &failure) || failure.code != exitUnavailable || !strings.Contains(err.Error(), cause) {
			t.Fatalf("explain(%q) = %v", cause, err)
		}
	}
}
