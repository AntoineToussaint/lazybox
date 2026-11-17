package main

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// The commands these tests run are this test binary: LAZYBOX_TEST_SERVE
// makes it a server (a TCP listener that says "served" per connection),
// LAZYBOX_TEST_QUIET a program that prints one line and never exits.
func init() {
	switch {
	case os.Getenv("LAZYBOX_TEST_SERVE") == "1":
		l, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			os.Exit(3)
		}
		fmt.Printf("\x1b[32mready\x1b[0m on port %d\n", l.Addr().(*net.TCPAddr).Port)
		for {
			conn, err := l.Accept()
			if err != nil {
				os.Exit(4)
			}
			fmt.Println("served")
			conn.Close()
		}
	case os.Getenv("LAZYBOX_TEST_QUIET") == "1":
		fmt.Println("watching for changes")
		time.Sleep(time.Hour)
		os.Exit(0)
	}
}

// fakeRunner is the Runner's pipe RPCs over real HTTP, so the follower (another
// process) reaches it like the CLI would.
type fakeRunner struct {
	genconnect.UnimplementedRunnerServiceHandler
	mu         sync.Mutex
	published  bytes.Buffer
	follows    []*pb.FollowPipeRequest
	finished   *pb.FinishPipeRequest
	stop       bool
	background bool
	// ended: the Runner ended the pipe (a deleted conversation, a closed Runner).
	ended bool
	url   string
}

func (r *fakeRunner) RegisterPipe(context.Context, *connect.Request[pb.RegisterPipeRequest]) (*connect.Response[pb.CommandStatus], error) {
	return connect.NewResponse(&pb.CommandStatus{Id: "cmd-1"}), nil
}
func (r *fakeRunner) Publish(_ context.Context, req *connect.Request[pb.PublishRequest]) (*connect.Response[pb.CommandStatus], error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if req.Msg.Offset != uint64(r.published.Len()) {
		return nil, connect.NewError(connect.CodeFailedPrecondition, fmt.Errorf("offset %d, holding %d", req.Msg.Offset, r.published.Len()))
	}
	r.published.Write(req.Msg.Data)
	return connect.NewResponse(&pb.CommandStatus{}), nil
}
func (r *fakeRunner) FinishPipe(_ context.Context, req *connect.Request[pb.FinishPipeRequest]) (*connect.Response[pb.CommandStatus], error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.finished = req.Msg
	return connect.NewResponse(&pb.CommandStatus{}), nil
}
func (r *fakeRunner) FollowPipe(ctx context.Context, req *connect.Request[pb.FollowPipeRequest]) (*connect.Response[pb.FollowPipeResponse], error) {
	r.mu.Lock()
	r.follows = append(r.follows, req.Msg)
	ended := r.ended
	r.mu.Unlock()
	if ended {
		return nil, connect.NewError(connect.CodeFailedPrecondition, errors.New("command capture is complete or failed"))
	}
	deadline := time.Now().Add(min(time.Duration(req.Msg.WaitMs)*time.Millisecond, 100*time.Millisecond))
	for {
		r.mu.Lock()
		reply := &pb.FollowPipeResponse{Stop: r.stop, Background: r.background && !req.Msg.Background, Offset: uint64(r.published.Len())}
		r.mu.Unlock()
		if reply.Stop || reply.Background || time.Now().After(deadline) {
			return connect.NewResponse(reply), nil
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func (r *fakeRunner) set(f func()) { r.mu.Lock(); f(); r.mu.Unlock() }
func (r *fakeRunner) output() string {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.published.String()
}
func (r *fakeRunner) lastFollow() *pb.FollowPipeRequest {
	r.mu.Lock()
	defer r.mu.Unlock()
	if len(r.follows) == 0 {
		return nil
	}
	return r.follows[len(r.follows)-1]
}
func (r *fakeRunner) finish() *pb.FinishPipeRequest {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.finished
}

func startFakeRunner(t *testing.T) (*fakeRunner, genconnect.RunnerServiceClient, endpoint) {
	t.Helper()
	r := &fakeRunner{}
	mux := http.NewServeMux()
	mux.Handle(genconnect.NewRunnerServiceHandler(r))
	server := httptest.NewServer(mux)
	t.Cleanup(server.Close)
	r.url = server.URL
	address := endpoint{URL: server.URL, Token: "fixture"}
	return r, genconnect.NewRunnerServiceClient(newHTTPClient(address.Token), address.URL), address
}

func eventually(t *testing.T, what string, ok func() bool) {
	t.Helper()
	for deadline := time.Now().Add(15 * time.Second); time.Now().Before(deadline); time.Sleep(20 * time.Millisecond) {
		if ok() {
			return
		}
	}
	t.Fatalf("timed out waiting: %s", what)
}

func self(t *testing.T) string {
	t.Helper()
	path, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	return path
}

// stopAndAwaitFinish asks the run to stop through the fake Runner, as
// `lazybox stop ID` or Stop in Lazybox would, and waits for its exit.
func stopAndAwaitFinish(t *testing.T, r *fakeRunner) *pb.FinishPipeRequest {
	t.Helper()
	r.set(func() { r.stop = true })
	eventually(t, "the stopped command's exit", func() bool { return r.finish() != nil })
	return r.finish()
}

// A server that is still running after the grace period and listens on a
// port goes on in the background: lazybox run returns at once with the id,
// the URL, its latest output (colour codes out) and the output file; the
// same process keeps serving and streaming to the console; the Runner learns
// its port; a stop reaches it and its exit is recorded.
func TestRunMovesAServerToTheBackgroundAndStopsItWhenAsked(t *testing.T) {
	r, client, address := startFakeRunner(t)
	dir := t.TempDir()
	var stdout, stderr bytes.Buffer
	started := time.Now()
	err := runCommand(context.Background(), client, nil, runOptions{session: "execution", outputDir: dir, grace: 500 * time.Millisecond, endpoint: address},
		[]string{"env", "LAZYBOX_TEST_SERVE=1", self(t)}, &stdout, &stderr)
	if err != nil {
		t.Fatalf("a server in the background is a success: %v (%s)", err, stderr.String())
	}
	if took := time.Since(started); took > 10*time.Second {
		t.Fatalf("lazybox run waited %s", took)
	}
	report := stdout.String()
	port := regexp.MustCompile(`ready on port (\d+)`).FindStringSubmatch(report)
	if port == nil {
		t.Fatalf("the report shows the latest output: %q", report)
	}
	for _, want := range []string{"→ running in the background as command cmd-1 after", "it listens on port " + port[1], "listening on http://localhost:" + port[1], "output so far (", dir, "lazybox stop cmd-1", "lazybox ls"} {
		if !strings.Contains(report, want) {
			t.Errorf("report misses %q:\n%s", want, report)
		}
	}
	if strings.Contains(report, "\x1b[") {
		t.Errorf("colour codes reached the agent: %q", report)
	}
	// The same process still serves, and its output still reaches the console.
	conn, err := net.Dial("tcp", "127.0.0.1:"+port[1])
	if err != nil {
		t.Fatal("the server did not survive lazybox run returning", err)
	}
	conn.Close()
	eventually(t, "output published after the move", func() bool { return strings.Contains(r.output(), "served") })
	eventually(t, "the Runner told the run is in the background with its port", func() bool {
		f := r.lastFollow()
		want, _ := strconv.Atoi(port[1])
		return f != nil && f.Background && slices.Contains(f.Ports, uint32(want)) && f.Pid > 0
	})
	if !strings.HasPrefix(r.output(), "\x1b[32mready") {
		t.Fatalf("the console keeps every byte from the start: %q", r.output())
	}
	finished := stopAndAwaitFinish(t, r)
	if finished.Signal == "" || finished.Report == nil || finished.Report.Filter != "background" || finished.Report.OutputPath == "" {
		t.Fatalf("the stop is recorded with its signal: %+v", finished)
	}
	saved, err := os.ReadFile(finished.Report.OutputPath)
	if err != nil || !strings.Contains(string(saved), " on port "+port[1]) || !strings.Contains(string(saved), "served") {
		t.Fatalf("the full-output file has the output from before and after the move: %v %q", err, saved)
	}
	if _, err = net.DialTimeout("tcp", "127.0.0.1:"+port[1], time.Second); err == nil {
		t.Fatal("the server still listens after the stop")
	}
}

// A watcher opens no port; its command line says what it is.
func TestRunMovesAWatcherToTheBackgroundByItsCommandLine(t *testing.T) {
	r, client, address := startFakeRunner(t)
	var stdout bytes.Buffer
	err := runCommand(context.Background(), client, nil, runOptions{session: "execution", outputDir: t.TempDir(), grace: 300 * time.Millisecond, endpoint: address},
		[]string{"env", "LAZYBOX_TEST_QUIET=1", self(t), "--watch"}, &stdout, &bytes.Buffer{})
	if err != nil {
		t.Fatal(err)
	}
	if out := stdout.String(); !strings.Contains(out, "it looks like a server or watcher") || !strings.Contains(out, "watching for changes") || strings.Contains(out, "listening on") {
		t.Fatalf("%q", out)
	}
	stopAndAwaitFinish(t, r)
}

// A command that ends within the grace period is an ordinary run, whatever
// it looks like; one that ends later without a port or a server's name is
// waited for, as before.
func TestRunWaitsForCommandsThatEnd(t *testing.T) {
	r, client, address := startFakeRunner(t)
	var stdout bytes.Buffer
	err := runCommand(context.Background(), client, nil, runOptions{session: "execution", outputDir: t.TempDir(), grace: 5 * time.Second, endpoint: address},
		[]string{"sh", "-c", "echo npm run dev; exit 3"}, &stdout, &bytes.Buffer{})
	var exit processExit
	if !errors.As(err, &exit) || exit != 3 || !strings.HasPrefix(stdout.String(), "[lazybox] sh -c echo npm run dev; exit 3 → exit 3") {
		t.Fatalf("%v %q", err, stdout.String())
	}
	stdout.Reset()
	err = runCommand(context.Background(), client, nil, runOptions{session: "execution", outputDir: t.TempDir(), grace: 200 * time.Millisecond, endpoint: address},
		[]string{"sh", "-c", "sleep 1.2; echo long test done"}, &stdout, &bytes.Buffer{})
	if err != nil || !strings.Contains(stdout.String(), "→ exit 0") || !strings.Contains(stdout.String(), "long test done") {
		t.Fatalf("a long command that is not a server is waited for: %v %q", err, stdout.String())
	}
	for _, f := range r.follows {
		if f.Background {
			t.Fatal("nothing went to the background")
		}
	}
}

// --background moves any command as soon as it has started; --wait keeps a
// server in the foreground.
func TestRunBackgroundAndWaitFlags(t *testing.T) {
	r, client, address := startFakeRunner(t)
	var stdout bytes.Buffer
	started := time.Now()
	err := runCommand(context.Background(), client, nil, runOptions{session: "execution", outputDir: t.TempDir(), background: true, endpoint: address},
		[]string{"env", "LAZYBOX_TEST_QUIET=1", self(t)}, &stdout, &bytes.Buffer{})
	if err != nil || !strings.Contains(stdout.String(), "you asked for it (--background)") || time.Since(started) > 8*time.Second {
		t.Fatalf("%v %q", err, stdout.String())
	}
	stopAndAwaitFinish(t, r)

	var stderr bytes.Buffer
	if err = runVerb(context.Background(), "", []string{"--wait", "5m", "--", "true"}, &stdout, &stderr); err == nil || !strings.Contains(err.Error(), "--wait=5m") {
		t.Fatalf("a bare --wait followed by a duration is a mistake to point out: %v", err)
	}
	if err = runVerb(context.Background(), "", []string{"--wait", "--background", "--", "true"}, &stdout, &stderr); err == nil {
		t.Fatal("--wait with --background")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Second)
	defer cancel()
	stdout.Reset()
	_, client, address = startFakeRunner(t)
	// --wait: still in the foreground long after the grace period, until interrupted.
	err = runCommand(ctx, client, nil, runOptions{session: "execution", outputDir: t.TempDir(), wait: true, grace: 200 * time.Millisecond, endpoint: address},
		[]string{"env", "LAZYBOX_TEST_SERVE=1", self(t)}, &stdout, &bytes.Buffer{})
	if err == nil || strings.Contains(stdout.String(), "background") || !strings.Contains(stdout.String(), " on port ") {
		t.Fatalf("--wait kept it until the interrupt: %v %q", err, stdout.String())
	}
}

// The person moves a foreground run to the background from Lazybox: the
// agent's call returns now, before any grace period.
func TestRunMovesToTheBackgroundWhenTheRunnerAsks(t *testing.T) {
	r, client, address := startFakeRunner(t)
	r.set(func() { r.background = true })
	var stdout bytes.Buffer
	err := runCommand(context.Background(), client, nil, runOptions{session: "execution", outputDir: t.TempDir(), grace: time.Hour, endpoint: address},
		[]string{"env", "LAZYBOX_TEST_QUIET=1", self(t)}, &stdout, &bytes.Buffer{})
	if err != nil || !strings.Contains(stdout.String(), "you moved it to the background from Lazybox") {
		t.Fatalf("%v %q", err, stdout.String())
	}
	stopAndAwaitFinish(t, r)
}

// Stop in Lazybox reaches a run still in the foreground too.
func TestRunStopsAForegroundCommandWhenTheRunnerAsks(t *testing.T) {
	r, client, address := startFakeRunner(t)
	go func() {
		// Once it runs, as a person would click Stop.
		for !strings.Contains(r.output(), "watching") {
			time.Sleep(10 * time.Millisecond)
		}
		r.set(func() { r.stop = true })
	}()
	var stdout bytes.Buffer
	started := time.Now()
	err := runCommand(context.Background(), client, nil, runOptions{session: "execution", outputDir: t.TempDir(), grace: time.Hour, endpoint: address},
		[]string{"env", "LAZYBOX_TEST_QUIET=1", self(t)}, &stdout, &bytes.Buffer{})
	var exit processExit
	if !errors.As(err, &exit) || !strings.Contains(stdout.String(), "watching for changes") {
		t.Fatalf("%v %q", err, stdout.String())
	}
	if took := time.Since(started); took > stopGrace {
		t.Fatalf("SIGTERM did not stop it; it took %s", took)
	}
}

// Without a Runner the command still moves (an agent's turn must not hang),
// and the report says how to stop it without one.
func TestRunMovesToTheBackgroundWithoutARunner(t *testing.T) {
	var stdout bytes.Buffer
	err := runCommand(context.Background(), nil, errors.New("offline"), runOptions{outputDir: t.TempDir(), background: true},
		[]string{"env", "LAZYBOX_TEST_QUIET=1", self(t)}, &stdout, &bytes.Buffer{})
	if err != nil {
		t.Fatal(err)
	}
	group := regexp.MustCompile(`kill -TERM -(\d+)`).FindStringSubmatch(stdout.String())
	if group == nil {
		t.Fatalf("%q", stdout.String())
	}
	pgid, _ := strconv.Atoi(group[1])
	if err = syscall.Kill(-pgid, 0); err != nil {
		t.Fatal("the command is not running", err)
	}
	_ = syscall.Kill(-pgid, syscall.SIGTERM)
	eventually(t, "the group to end", func() bool { return syscall.Kill(-pgid, 0) != nil })
}

// The follower resumes after a Runner that lost a reply, and says what it
// could not show while the Runner was away.
func TestPublisherResyncsAndNotesWhatItDropped(t *testing.T) {
	r, client, _ := startFakeRunner(t)
	p := &publisher{client: client, id: "cmd-1"}
	p.add([]byte("one\n"))
	if r.output() != "one\n" {
		t.Fatalf("%q", r.output())
	}
	// The Runner holds bytes this publisher never saw acknowledged.
	p.offset = 0
	p.pending = []byte("one\ntwo\n")
	p.resync(4)
	p.flush()
	if r.output() != "one\ntwo\n" {
		t.Fatalf("a lost reply was published twice: %q", r.output())
	}
	p.dropped = 1 << 20
	p.flush()
	if !strings.Contains(r.output(), "1.0 MiB of output not shown while the Runner was unreachable") {
		t.Fatalf("%q", r.output())
	}
	scanner := bufio.NewScanner(strings.NewReader(r.output()))
	lines := 0
	for scanner.Scan() {
		lines++
	}
	if lines != 4 {
		t.Fatalf("%q", r.output())
	}
}

// A run whose pipe Lazybox ended (its conversation deleted, the Runner
// closed) stops: nobody could see or stop it any more.
func TestABackgroundRunStopsWhenLazyboxEndsItsPipe(t *testing.T) {
	r, client, address := startFakeRunner(t)
	var stdout bytes.Buffer
	if err := runCommand(context.Background(), client, nil, runOptions{session: "execution", outputDir: t.TempDir(), background: true, endpoint: address},
		[]string{"env", "LAZYBOX_TEST_QUIET=1", self(t)}, &stdout, &bytes.Buffer{}); err != nil {
		t.Fatal(err)
	}
	var pid int32
	eventually(t, "the run in the background follows", func() bool {
		if f := r.lastFollow(); f != nil && f.Background {
			pid = f.Pid
			return true
		}
		return false
	})
	r.set(func() { r.ended = true })
	eventually(t, "the command's group to end", func() bool { return syscall.Kill(-int(pid), 0) != nil })
}
