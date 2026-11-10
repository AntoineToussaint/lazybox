package jsonl

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

func TestMain(m *testing.M) {
	if os.Getenv("JSONL_FIXTURE") == "agent" {
		fixtureAgent()
		return
	}
	os.Exit(m.Run())
}

// fixtureAgent answers calls with their method, re-emits notifications and
// raises a server request on "ask".
func fixtureAgent() {
	out := bufio.NewWriter(os.Stdout)
	write := func(v any) { data, _ := json.Marshal(v); fmt.Fprintf(out, "%s\n", data); out.Flush() }
	write(map[string]any{"method": "ready", "params": os.Getpid()})
	s := bufio.NewScanner(os.Stdin)
	for s.Scan() {
		var m Message
		if json.Unmarshal(s.Bytes(), &m) != nil {
			continue
		}
		switch {
		case m.Method == "finish":
			write(map[string]any{"method": "last"})
			os.Exit(0)
		case m.Method == "ask":
			write(map[string]any{"id": "q-" + strings.Trim(string(m.Params), `"`), "method": "question"})
		case m.Method != "" && len(m.ID) > 0:
			write(map[string]any{"id": m.ID, "result": map[string]string{"method": m.Method, "id": string(m.ID)}})
		case m.Method != "":
			write(map[string]any{"method": "event", "params": m.Params})
		case len(m.ID) > 0:
			write(map[string]any{"method": "answered", "params": m.ID})
		}
	}
	if err := s.Err(); err != nil {
		os.Exit(1)
	}
}

func hostRoot(t *testing.T) string {
	t.Helper()
	dir, err := os.MkdirTemp("", "lbx-jsonl-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		ids, _ := sessionhost.Hosts(dir)
		for _, id := range ids {
			_ = sessionhost.Stop(context.Background(), dir, id)
		}
		_ = os.RemoveAll(dir)
	})
	return dir
}

func startHosted(t *testing.T, ctx context.Context, root string) (*Process, int) {
	t.Helper()
	h, err := Launch(ctx, root)
	if err != nil {
		t.Fatal(err)
	}
	self, _ := os.Executable()
	p, err := h.Start(ctx, self, nil, append(os.Environ(), "JSONL_FIXTURE=agent"), t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	m, err := p.Receive(ctx)
	if err != nil || m.Method != "ready" {
		t.Fatalf("%+v %v", m, err)
	}
	var pid int
	_ = json.Unmarshal(m.Params, &pid)
	return p, pid
}

func timeout(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func TestDetachedProcessSurvivesItsRunner(t *testing.T) {
	root := hostRoot(t)
	runner, stopRunner := context.WithCancel(context.Background())
	p, pid := startHosted(t, runner, root)
	result, err := p.Call(timeout(t), "start", map[string]string{"text": "hello"})
	if err != nil || !strings.Contains(string(result), `"start"`) {
		t.Fatalf("%s %v", result, err)
	}
	if err = p.Notify("note", "first"); err != nil {
		t.Fatal(err)
	}
	first, err := p.Receive(timeout(t))
	if err != nil || first.Seq == 0 {
		t.Fatalf("hosted message lacks its transport cursor: %+v %v", first, err)
	}
	p.Commit(first.Seq)
	id := p.HostID()
	if err = p.Detach(); err != nil {
		t.Fatal(err)
	}
	// The Runner's lifetime then ends: a detached agent is no longer its to stop.
	stopRunner()
	if _, err = p.Receive(timeout(t)); err == nil {
		t.Fatal("detached process kept delivering")
	}
	if err = p.Close(); err != nil {
		t.Fatalf("closing a detached process must not stop its agent: %v", err)
	}
	time.Sleep(100 * time.Millisecond)
	if syscall.Kill(pid, 0) != nil {
		t.Fatal("a detached agent was stopped")
	}
	next, err := Attach(timeout(t), root, id, first.Seq)
	if err != nil {
		t.Fatal(err)
	}
	result, err = next.Call(timeout(t), "again", nil)
	if err != nil || !strings.Contains(string(result), `"again"`) {
		t.Fatalf("re-attached call: %s %v", result, err)
	}
	if err = next.Close(); err != nil {
		t.Fatal(err)
	}
	awaitStopped(t, pid)
}

// Cancelling the Runner's context without detaching keeps the local-pipe
// contract: the agent stops. Only Detach, or the Runner dying, leaves it running.
func TestCancelledContextStillStopsAHostedAgent(t *testing.T) {
	root := hostRoot(t)
	runner, stopRunner := context.WithCancel(context.Background())
	_, pid := startHosted(t, runner, root)
	stopRunner()
	awaitStopped(t, pid)
}

func awaitStopped(t *testing.T, pid int) {
	t.Helper()
	for end := time.Now().Add(5 * time.Second); syscall.Kill(pid, 0) == nil; time.Sleep(20 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatal("hosted agent still running")
		}
	}
}

func TestHostedRequestRedeliveredAfterReattach(t *testing.T) {
	root := hostRoot(t)
	p, _ := startHosted(t, context.Background(), root)
	_ = p.Notify("ask", "approve")
	request, err := p.Receive(timeout(t))
	if err != nil || request.Method != "question" || request.Redelivered {
		t.Fatalf("%+v %v", request, err)
	}
	p.Commit(request.Seq)
	if err = p.Detach(); err != nil {
		t.Fatal(err)
	}
	next, err := Attach(timeout(t), root, p.HostID(), request.Seq)
	if err != nil {
		t.Fatal(err)
	}
	defer next.Close()
	again, err := next.Receive(timeout(t))
	if err != nil || !again.Redelivered || string(again.ID) != string(request.ID) {
		t.Fatalf("pending request not redelivered: %+v %v", again, err)
	}
	if err = next.Reply(again.ID, map[string]bool{"ok": true}); err != nil {
		t.Fatal(err)
	}
	answered, err := next.Receive(timeout(t))
	if err != nil || answered.Method != "answered" {
		t.Fatalf("%+v %v", answered, err)
	}
}

func TestLocalProcessCannotDetach(t *testing.T) {
	ctx := timeout(t)
	p, err := Start(ctx, "/bin/sh", []string{"-c", "cat >/dev/null"}, nil, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	if p.Detach() == nil || p.HostID() != "" {
		t.Fatal("a local pipe claimed it could outlive its Runner")
	}
}

// The host of an exited agent keeps its last output until the Runner has
// recorded it: a Runner that dies in between finds it again on re-attachment.
func TestExitedAgentHostKeepsOutputUntilRecorded(t *testing.T) {
	root := hostRoot(t)
	p, _ := startHosted(t, context.Background(), root)
	id := p.HostID()
	if err := p.Notify("finish", nil); err != nil {
		t.Fatal(err)
	}
	last, err := p.Receive(timeout(t))
	if err != nil || last.Method != "last" {
		t.Fatalf("%+v %v", last, err)
	}
	if _, err = p.Receive(timeout(t)); err == nil {
		t.Fatal("no exit after the last line")
	}
	// Not yet recorded: the host must still hold the line.
	time.Sleep(300 * time.Millisecond)
	if ids, _ := sessionhost.Hosts(root); len(ids) != 1 {
		t.Fatalf("host released before its last line was recorded: %v", ids)
	}
	// This Runner dies before recording; the next one gets the line and the exit.
	_ = p.host.Detach()
	next, err := Attach(timeout(t), root, id, 0)
	if err != nil {
		t.Fatal(err)
	}
	var again Message
	for again.Method != "last" {
		if again, err = next.Receive(timeout(t)); err != nil {
			t.Fatalf("the unrecorded last line was lost: %v", err)
		}
	}
	if _, err = next.Receive(timeout(t)); err == nil {
		t.Fatal("no exit after the redelivered last line")
	}
	next.Commit(again.Seq)
	deadline := time.Now().Add(3 * time.Second)
	for {
		ids, _ := sessionhost.Hosts(root)
		if len(ids) == 0 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("host not released once its output was recorded: %v", ids)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

// A re-attached Runner never reuses its predecessor's request IDs: a reply to
// one of the old calls must not answer a new one.
func TestReattachedRunnerUsesFreshRequestIDs(t *testing.T) {
	root := hostRoot(t)
	p, _ := startHosted(t, context.Background(), root)
	callID := func(p *Process) string {
		t.Helper()
		result, err := p.Call(timeout(t), "probe", nil)
		if err != nil {
			t.Fatal(err)
		}
		var r struct{ ID string }
		if err = json.Unmarshal(result, &r); err != nil || r.ID == "" {
			t.Fatalf("%s %v", result, err)
		}
		return r.ID
	}
	first := callID(p)
	id := p.HostID()
	if err := p.Detach(); err != nil {
		t.Fatal(err)
	}
	next, err := Attach(timeout(t), root, id, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer next.Close()
	if again := callID(next); again == first {
		t.Fatalf("the re-attached Runner reused request ID %s", first)
	}
}
