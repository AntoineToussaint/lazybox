package runner

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/claude"
	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

// The test binary also plays a Claude SDK worker (same JSONL protocol as
// workers/claude-sdk) and a Runner process that the test kills.
func TestMain(m *testing.M) {
	if len(os.Args) > 1 && os.Args[1] == "fixture-claude-worker" {
		fixtureWorker()
		return
	}
	if os.Getenv("RUNNER_FIXTURE") == "runner" {
		fixtureRunner()
		return
	}
	os.Exit(m.Run())
}

// fixtureWorker raises one approval per turn and finishes the turn once it is
// answered, naming its own PID so a test can tell which process finished it.
func fixtureWorker() {
	out := bufio.NewWriter(os.Stdout)
	send := func(v any) { data, _ := json.Marshal(v); fmt.Fprintf(out, "%s\n", data); out.Flush() }
	event := func(v map[string]any) {
		v["sessionId"] = "fixture-ref"
		send(map[string]any{"method": "event", "params": v})
	}
	pid := strconv.Itoa(os.Getpid())
	s := bufio.NewScanner(os.Stdin)
	s.Buffer(make([]byte, 4096), 16<<20)
	for s.Scan() {
		var r struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Params struct {
				RequestID string `json:"requestId"`
				Decision  string `json:"decision"`
			} `json:"params"`
		}
		if json.Unmarshal(s.Bytes(), &r) != nil {
			os.Exit(1)
		}
		switch r.Method {
		case "open":
			send(map[string]any{"id": r.ID, "result": map[string]string{"reference": "fixture-ref"}})
			event(map[string]any{"type": "session_ready", "text": pid})
		case "message":
			send(map[string]any{"id": r.ID, "result": map[string]string{"delivery": "submitted"}})
			event(map[string]any{"type": "turn_started"})
			event(map[string]any{"type": "approval", "requestId": "req-1", "text": "Bash"})
		case "approve":
			if r.Params.RequestID != "req-1" {
				send(map[string]any{"id": r.ID, "error": map[string]string{"message": "unknown request"}})
				continue
			}
			send(map[string]any{"id": r.ID, "result": map[string]string{"delivery": "submitted"}})
			event(map[string]any{"type": "request_resolved", "requestId": "req-1"})
			event(map[string]any{"type": "message_delta", "text": r.Params.Decision + " by " + pid})
			event(map[string]any{"type": "turn_ended", "status": "completed"})
		}
	}
	if s.Err() != nil {
		os.Exit(1)
	}
}

func fixtureBench(t testing.TB, state string) *Bench {
	b, err := New(Config{State: state, Worker: "fixture-claude-worker"})
	if err != nil {
		if t == nil {
			fmt.Println("error", err)
			os.Exit(2)
		}
		t.Fatal(err)
	}
	self, _ := os.Executable()
	b.Adapters["claude"] = claude.Adapter{Node: self, Worker: "fixture-claude-worker"}
	return b
}

// openAwaitingApproval starts an execution and leaves its turn blocked on an approval.
func openAwaitingApproval(b *Bench) (*Execution, error) {
	e, err := b.OpenEmptyWorkspace("claude-default", accounts.Options{}, "direct")
	if err != nil {
		return nil, err
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "build", Kind: agent.Message, Text: "build it"}); err != nil {
		return nil, err
	}
	for end := time.Now().Add(10 * time.Second); time.Now().Before(end); time.Sleep(10 * time.Millisecond) {
		if _, ok := e.Engine.Snapshot().Pending["attempt-0:req-1"]; ok {
			return e, nil
		}
	}
	return nil, fmt.Errorf("approval never arrived: %+v", e.Engine.Snapshot())
}

// fixtureRunner opens an execution waiting on an approval, reports its ID, then
// waits to be killed.
func fixtureRunner() {
	b := fixtureBench(nil, os.Getenv("RUNNER_STATE"))
	e, err := openAwaitingApproval(b)
	if err != nil {
		fmt.Println("error", err)
		os.Exit(2)
	}
	fmt.Println(e.ID)
	select {}
}

func recorded(t *testing.T, b *Bench, id string) []agent.Event {
	t.Helper()
	var events []agent.Event
	// A live journal blocks at its end; everything recorded so far is already there.
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	for cursor := uint64(0); ; {
		r, err := b.Timeline(ctx, id, cursor)
		if err != nil {
			return events
		}
		var ev agent.Event
		_ = json.Unmarshal(r.Value, &ev)
		events = append(events, ev)
		cursor = r.Sequence
	}
}

func awaitEvent(t *testing.T, b *Bench, id, kind string) []agent.Event {
	t.Helper()
	for end := time.Now().Add(10 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
		events := recorded(t, b, id)
		for _, ev := range events {
			if ev.Type == kind {
				return events
			}
		}
	}
	t.Fatalf("%s never recorded: %+v", kind, recorded(t, b, id))
	return nil
}

func workerPID(t *testing.T, events []agent.Event) int {
	t.Helper()
	for _, ev := range events {
		if ev.Type == "session_ready" {
			pid, _ := strconv.Atoi(ev.Text)
			return pid
		}
	}
	t.Fatal("worker never reported ready")
	return 0
}

func count(events []agent.Event, kind string) int {
	n := 0
	for _, ev := range events {
		if ev.Type == kind {
			n++
		}
	}
	return n
}

// finishAfterAdoption answers the approval the previous Runner left pending and
// checks the same native process completes the turn.
func finishAfterAdoption(t *testing.T, b *Bench, id string, pid int) {
	t.Helper()
	e, err := b.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	if e.Archived || e.RecoveryRequired {
		t.Fatalf("live execution came back as history (archived=%v recovery=%v)", e.Archived, e.RecoveryRequired)
	}
	snapshot := e.Engine.Snapshot()
	if snapshot.Closed || !snapshot.Busy || snapshot.Fault != "" {
		t.Fatalf("live state lost: %+v", snapshot)
	}
	if _, ok := snapshot.Pending["attempt-0:req-1"]; !ok {
		t.Fatalf("pending approval lost: %+v", snapshot.Pending)
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "approve", Kind: agent.Approve, RequestID: "attempt-0:req-1", Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	events := awaitEvent(t, b, id, "turn_ended")
	finished := ""
	for _, ev := range events {
		if ev.Type == "message_delta" {
			finished = ev.Text
		}
	}
	if finished != "accept by "+strconv.Itoa(pid) {
		t.Fatalf("turn not finished by the original agent process %d: %q", pid, finished)
	}
	if count(events, "approval") != 1 || count(events, "runner_reattached") != 1 || count(events, "process_exit") != 0 {
		t.Fatalf("journal continuity broken: %+v", events)
	}
}

func stoppedEventually(t *testing.T, pid int) {
	t.Helper()
	for end := time.Now().Add(8 * time.Second); syscall.Kill(pid, 0) == nil; time.Sleep(20 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatalf("agent %d still running", pid)
		}
	}
}

func cleanupHosts(t *testing.T, state string) {
	t.Cleanup(func() {
		for _, root := range []string{filepath.Join(state, "hosts"), filepath.Join(state, "command-hosts")} {
			ids, _ := sessionhost.Hosts(root)
			for _, id := range ids {
				_ = sessionhost.Stop(context.Background(), root, id)
			}
		}
	})
}

// The requirement: a Runner killed outright (with its whole process group, as
// launchd or a crash would) is a no-op for the agent. The next Runner re-attaches
// and the same agent process finishes the same turn.
func TestRunnerKilledMidTurnIsANoOpForTheAgent(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	self, _ := os.Executable()
	runner := exec.Command(self)
	runner.Env = append(os.Environ(), "RUNNER_FIXTURE=runner", "RUNNER_STATE="+state)
	runner.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	out, err := runner.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err = runner.Start(); err != nil {
		t.Fatal(err)
	}
	line, err := bufio.NewReader(out).ReadString('\n')
	if err != nil || strings.HasPrefix(line, "error") {
		t.Fatalf("fixture Runner: %q %v", line, err)
	}
	id := strings.TrimSpace(line)
	if err = syscall.Kill(-runner.Process.Pid, syscall.SIGKILL); err != nil {
		t.Fatal(err)
	}
	_ = runner.Wait()

	b := fixtureBench(t, state)
	if err = b.Adopt(context.Background()); err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, b, id))
	if syscall.Kill(pid, 0) != nil {
		t.Fatal("agent died with the Runner")
	}
	finishAfterAdoption(t, b, id, pid)
	// Ending the session still stops the agent.
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	stoppedEventually(t, pid)
}

func TestGracefulDetachHandsTheLiveAgentToTheNextRunner(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	first := fixtureBench(t, state)
	e, err := openAwaitingApproval(first)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, first, e.ID))
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	if syscall.Kill(pid, 0) != nil {
		t.Fatal("Detach stopped the agent")
	}
	second := fixtureBench(t, state)
	if err = second.Adopt(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(second.List()) != 1 {
		t.Fatal("adopted execution not listed as live")
	}
	finishAfterAdoption(t, second, e.ID, pid)
	if err = second.Close(); err != nil {
		t.Fatal(err)
	}
	stoppedEventually(t, pid)
}

func TestCloseStillStopsAgentsWhenNotDetaching(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	b := fixtureBench(t, state)
	e, err := openAwaitingApproval(b)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, b, e.ID))
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	stoppedEventually(t, pid)
	if ids, _ := sessionhost.Hosts(filepath.Join(state, "hosts")); len(ids) != 0 {
		t.Fatalf("hosts left after Close: %v", ids)
	}
}

func TestAdoptStopsHostsNoExecutionClaims(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	var pids []int
	// An agent host and a command host nothing records.
	for _, root := range []string{filepath.Join(state, "hosts"), filepath.Join(state, "command-hosts")} {
		orphan, err := sessionhost.Launch(ctx, root, sessionhost.Options{})
		if err != nil {
			t.Fatal(err)
		}
		if err = orphan.Spawn(ctx, sessionhost.Spec{Path: "/bin/sleep", Args: []string{"300"}, Cwd: state}); err != nil {
			t.Fatal(err)
		}
		pids = append(pids, orphan.AgentPID())
		_ = orphan.Detach()
	}
	b := fixtureBench(t, state)
	defer b.Close()
	if err := b.Adopt(ctx); err != nil {
		t.Fatal(err)
	}
	for _, pid := range pids {
		stoppedEventually(t, pid)
	}
}

// A managed command an agent started keeps running across a graceful Runner
// restart, and the next Runner owns it: it can read, wait for and cancel it.
func TestGracefulDetachHandsRunningCommandsToTheNextRunner(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	first := fixtureBench(t, state)
	e, err := openAwaitingApproval(first)
	if err != nil {
		t.Fatal(err)
	}
	c, err := first.Start(e, "serve", []string{"/bin/sh", "-c", "echo $$; exec /bin/sleep 300"})
	if err != nil {
		t.Fatal(err)
	}
	var pid int
	for end := time.Now().Add(8 * time.Second); pid == 0 && time.Now().Before(end); time.Sleep(10 * time.Millisecond) {
		data, _, _ := c.Read(context.Background(), 0, 0)
		pid, _ = strconv.Atoi(strings.TrimSpace(string(data)))
	}
	if pid == 0 {
		t.Fatal("command never reported its PID")
	}
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	if syscall.Kill(pid, 0) != nil {
		t.Fatal("Detach stopped the managed command")
	}
	second := fixtureBench(t, state)
	if err = second.Adopt(context.Background()); err != nil {
		t.Fatal(err)
	}
	adopted, err := second.Commands.Get(c.Status().ID)
	if err != nil {
		t.Fatal(err)
	}
	if state := adopted.Status(); state.Done {
		t.Fatalf("adopted command reported finished: %+v", state)
	}
	if syscall.Kill(pid, 0) != nil {
		t.Fatal("adoption stopped the managed command")
	}
	if !second.Active(e.ID) {
		t.Fatal("a running adopted command does not hold its execution active")
	}
	if err = second.Close(); err != nil {
		t.Fatal(err)
	}
	stoppedEventually(t, pid)
}

func TestDeletingAConversationRemovesItsReattachmentRecord(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	b := fixtureBench(t, state)
	defer b.Close()
	e, err := openAwaitingApproval(b)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = os.Stat(b.attachmentPath(e.ID)); err != nil {
		t.Fatalf("hosted execution has no re-attachment record: %v", err)
	}
	if err = b.DeleteConversation(context.Background(), e.ID, true); err != nil {
		t.Fatal(err)
	}
	if _, err = os.Stat(b.attachmentPath(e.ID)); !os.IsNotExist(err) {
		t.Fatalf("re-attachment record retained after deletion: %v", err)
	}
}
