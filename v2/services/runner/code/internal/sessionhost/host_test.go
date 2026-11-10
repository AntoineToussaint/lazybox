package sessionhost

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"
)

// The test binary doubles as a fake JSONL agent and as a short-lived "Runner"
// that launches a host and then dies, so lifecycle tests use real processes.
func TestMain(m *testing.M) {
	switch os.Getenv("SESSIONHOST_FIXTURE") {
	case "agent":
		fakeAgent()
		return
	case "runner":
		fakeRunner()
		return
	}
	os.Exit(m.Run())
}

func emit(w io.Writer, method string, params any) {
	data, _ := json.Marshal(map[string]any{"method": method, "params": params})
	fmt.Fprintf(w, "%s\n", data)
}

func fakeAgent() {
	out := bufio.NewWriter(os.Stdout)
	emit(out, "ready", os.Getpid())
	out.Flush()
	s := bufio.NewScanner(os.Stdin)
	s.Buffer(make([]byte, 4096), 1<<20)
	for s.Scan() {
		var m struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Params json.RawMessage `json:"params"`
		}
		_ = json.Unmarshal(s.Bytes(), &m)
		switch {
		case m.Method == "echo":
			emit(out, "echoed", m.Params)
		case m.Method == "ask":
			fmt.Fprintf(out, `{"id":%s,"method":"question"}`+"\n", m.Params)
		case m.Method == "flood":
			n, _ := strconv.Atoi(string(m.Params))
			for i := 1; i <= n; i++ {
				emit(out, "line", i)
			}
		case m.Method == "grandchild":
			c := exec.Command("/bin/sleep", "300")
			_ = c.Start()
			emit(out, "grandchild", c.Process.Pid)
		case m.Method == "exit":
			out.Flush()
			code, _ := strconv.Atoi(string(m.Params))
			os.Exit(code)
		case m.Method == "" && len(m.ID) > 0:
			emit(out, "answered", m.ID)
		}
		out.Flush()
	}
	out.Flush()
}

// fakeRunner launches a host with a spawned agent, reports the host ID and
// agent PID, then waits to be killed.
func fakeRunner() {
	ctx := context.Background()
	c, err := Launch(ctx, os.Getenv("SESSIONHOST_ROOT"), Options{})
	if err != nil {
		fmt.Println("error", err)
		os.Exit(2)
	}
	if err = c.Spawn(ctx, agentSpec()); err != nil {
		fmt.Println("error", err)
		os.Exit(2)
	}
	m, _, err := c.Next(ctx)
	if err != nil {
		os.Exit(2)
	}
	_ = c.Ack(m.Seq)
	fmt.Println(c.ID(), c.AgentPID())
	select {}
}

func agentSpec() Spec {
	self, _ := os.Executable()
	return Spec{Path: self, Env: append(os.Environ(), "SESSIONHOST_FIXTURE=agent", "FIXTURE_TOKEN=credential-value"), Cwd: os.TempDir()}
}

func root(t *testing.T) string {
	t.Helper()
	// Keep the root short and private; t.TempDir is removed after the test.
	dir, err := os.MkdirTemp("", "lbx-host-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		ids, _ := Hosts(dir)
		for _, id := range ids {
			_ = Stop(context.Background(), dir, id)
		}
		_ = os.RemoveAll(dir)
	})
	return dir
}

func deadline(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func started(t *testing.T, dir string) (*Client, int) {
	t.Helper()
	ctx := deadline(t)
	c, err := Launch(ctx, dir, Options{})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Spawn(ctx, agentSpec()); err != nil {
		t.Fatal(err)
	}
	m := next(t, c)
	var params struct {
		Method string
		Params int
	}
	_ = json.Unmarshal(m.Line, &params)
	if params.Method != "ready" || params.Params == 0 {
		t.Fatalf("agent did not report ready: %s", m.Line)
	}
	return c, params.Params
}

func next(t *testing.T, c *Client) *Message {
	t.Helper()
	m, exit, err := c.Next(deadline(t))
	if err != nil || exit != nil {
		t.Fatalf("expected a message, got exit=%+v err=%v", exit, err)
	}
	return m
}

func method(t *testing.T, m *Message) (string, json.RawMessage) {
	t.Helper()
	var v struct {
		Method string          `json:"method"`
		Params json.RawMessage `json:"params"`
	}
	if err := json.Unmarshal(m.Line, &v); err != nil {
		t.Fatal(err)
	}
	return v.Method, v.Params
}

func alive(pid int) bool { return syscall.Kill(pid, 0) == nil }

func eventually(t *testing.T, what string, check func() bool) {
	t.Helper()
	for end := time.Now().Add(8 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
		if check() {
			return
		}
	}
	t.Fatal(what)
}

func TestReattachReplaysOnlyUnacknowledgedLines(t *testing.T) {
	dir := root(t)
	c, pid := started(t, dir)
	_ = c.Ack(1)
	if err := c.Send(json.RawMessage(`{"method":"echo","params":"a"}`)); err != nil {
		t.Fatal(err)
	}
	m := next(t, c)
	if name, params := method(t, m); name != "echoed" || string(params) != `"a"` || m.Seq != 2 {
		t.Fatalf("unexpected %s", m.Line)
	}
	// Seq 2 was received but never acknowledged as recorded.
	_ = c.Detach()
	if !alive(pid) {
		t.Fatal("detaching stopped the agent")
	}
	again, err := Attach(deadline(t), dir, c.ID(), 1)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Detach()
	m = next(t, again)
	if name, _ := method(t, m); name != "echoed" || m.Seq != 2 || !m.Redelivered {
		t.Fatalf("unacknowledged line not replayed in place, marked as already sent: %+v %s", m, m.Line)
	}
	_ = again.Send(json.RawMessage(`{"method":"echo","params":"b"}`))
	if m = next(t, again); m.Seq != 3 {
		t.Fatalf("live output did not continue the sequence: %d", m.Seq)
	}
}

func TestRunnerKilledWithItsProcessGroupLeavesAgentRunning(t *testing.T) {
	dir := root(t)
	self, _ := os.Executable()
	runner := exec.Command(self)
	runner.Env = append(os.Environ(), "SESSIONHOST_FIXTURE=runner", "SESSIONHOST_ROOT="+dir)
	runner.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	out, err := runner.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err = runner.Start(); err != nil {
		t.Fatal(err)
	}
	var id string
	var pid int
	if _, err = fmt.Fscan(out, &id, &pid); err != nil {
		t.Fatal(err)
	}
	// What launchd, a terminal hangup or an OOM kill does to a Runner and its group.
	if err = syscall.Kill(-runner.Process.Pid, syscall.SIGKILL); err != nil {
		t.Fatal(err)
	}
	_ = runner.Wait()
	time.Sleep(100 * time.Millisecond)
	if !alive(pid) {
		t.Fatal("agent died with its Runner")
	}
	c, err := Attach(deadline(t), dir, id, 1)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Detach()
	_ = c.Send(json.RawMessage(`{"method":"echo","params":"after restart"}`))
	if name, params := method(t, next(t, c)); name != "echoed" || string(params) != `"after restart"` {
		t.Fatalf("agent not reachable after Runner death: %s %s", name, params)
	}
}

func TestUnansweredRequestIsRedeliveredUntilAnswered(t *testing.T) {
	dir := root(t)
	c, _ := started(t, dir)
	_ = c.Send(json.RawMessage(`{"method":"ask","params":7}`))
	m := next(t, c)
	if name, _ := method(t, m); name != "question" {
		t.Fatalf("expected a request: %s", m.Line)
	}
	// Recorded but not answered: the agent still waits on it.
	_ = c.Ack(m.Seq)
	_ = c.Detach()
	again, err := Attach(deadline(t), dir, c.ID(), m.Seq)
	if err != nil {
		t.Fatal(err)
	}
	r := next(t, again)
	if !r.Redelivered || r.Seq != m.Seq || string(r.Line) != string(m.Line) {
		t.Fatalf("pending request not redelivered: %+v %s", r, r.Line)
	}
	_ = again.Send(json.RawMessage(`{"id":7,"result":{"answer":"yes"}}`))
	if name, params := method(t, next(t, again)); name != "answered" || string(params) != "7" {
		t.Fatalf("answer did not reach the agent: %s %s", name, params)
	}
	_ = again.Detach()
	third, err := Attach(deadline(t), dir, c.ID(), m.Seq)
	if err != nil {
		t.Fatal(err)
	}
	defer third.Detach()
	if r = next(t, third); r.Seq == m.Seq {
		t.Fatal("answered request replayed")
	}
	if name, _ := method(t, r); name != "answered" {
		t.Fatalf("unexpected replay %s", r.Line)
	}
}

func TestAnsweredUnacknowledgedRequestIsNotReplayed(t *testing.T) {
	dir := root(t)
	c, _ := started(t, dir)
	_ = c.Send(json.RawMessage(`{"method":"ask","params":"q1"}`))
	m := next(t, c)
	_ = c.Send(json.RawMessage(`{"id":"q1","result":true}`))
	answered := next(t, c)
	_ = c.Detach()
	again, err := Attach(deadline(t), dir, c.ID(), 1)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Detach()
	if r := next(t, again); r.Seq != answered.Seq {
		t.Fatalf("answered request %d replayed as %d", m.Seq, r.Seq)
	}
}

func TestTerminateKillsTheAgentGroupAndHostExits(t *testing.T) {
	dir := root(t)
	c, pid := started(t, dir)
	_ = c.Send(json.RawMessage(`{"method":"grandchild"}`))
	_, params := method(t, next(t, c))
	grandchild, _ := strconv.Atoi(string(params))
	if !alive(grandchild) {
		t.Fatal("fixture grandchild not running")
	}
	_ = c.Terminate()
	for {
		m, exit, err := c.Next(deadline(t))
		if err != nil {
			t.Fatal(err)
		}
		if exit != nil {
			if !exit.Outcome.Started {
				t.Fatalf("exit lacks outcome: %+v", exit)
			}
			break
		}
		_ = m
	}
	_ = c.Release()
	eventually(t, "agent survived terminate", func() bool { return !alive(pid) })
	eventually(t, "background descendant survived terminate", func() bool { return !alive(grandchild) })
	eventually(t, "host did not exit after release", func() bool {
		ids, _ := Hosts(dir)
		return len(ids) == 0
	})
}

func TestAgentExitWhileDetachedKeepsOutputUntilCollected(t *testing.T) {
	dir := root(t)
	c, _ := started(t, dir)
	_ = c.Ack(1)
	_ = c.Detach()
	// Nobody is attached: the agent writes and exits on a later client's behalf.
	writer, err := Attach(deadline(t), dir, c.ID(), 1)
	if err != nil {
		t.Fatal(err)
	}
	_ = writer.Send(json.RawMessage(`{"method":"echo","params":"last words"}`))
	_ = writer.Send(json.RawMessage(`{"method":"exit","params":3}`))
	time.Sleep(200 * time.Millisecond)
	_ = writer.Detach()
	time.Sleep(300 * time.Millisecond)
	if ids, _ := Hosts(dir); len(ids) != 1 {
		t.Fatal("host left before its output was collected")
	}
	later, err := Attach(deadline(t), dir, c.ID(), 1)
	if err != nil {
		t.Fatal(err)
	}
	if name, params := method(t, next(t, later)); name != "echoed" || string(params) != `"last words"` {
		t.Fatalf("buffered output lost: %s", params)
	}
	_, exit, err := later.Next(deadline(t))
	if err != nil || exit == nil || exit.Outcome.ExitCode != 3 {
		t.Fatalf("exit not reported: %+v %v", exit, err)
	}
	_ = later.Release()
	eventually(t, "released host did not exit", func() bool {
		ids, _ := Hosts(dir)
		return len(ids) == 0
	})
}

func TestBackpressureBlocksTheAgentInsteadOfDroppingOutput(t *testing.T) {
	dir := root(t)
	ctx := deadline(t)
	c, err := Launch(ctx, dir, Options{BufferLimit: 4096})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Spawn(ctx, agentSpec()); err != nil {
		t.Fatal(err)
	}
	_ = c.Send(json.RawMessage(`{"method":"flood","params":2000}`))
	_ = c.Detach()
	time.Sleep(300 * time.Millisecond)
	again, err := Attach(deadline(t), dir, c.ID(), 0)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Detach()
	if name, _ := method(t, next(t, again)); name != "ready" {
		t.Fatal("first line lost")
	}
	for want := 1; want <= 2000; want++ {
		m := next(t, again)
		if _, params := method(t, m); string(params) != strconv.Itoa(want) {
			t.Fatalf("line %d lost or reordered: %s", want, params)
		}
		if want%100 == 0 {
			_ = again.Ack(m.Seq)
		}
	}
}

func TestControlConnectionRequiresToken(t *testing.T) {
	dir := root(t)
	c, _ := started(t, dir)
	defer c.Detach()
	record, err := ReadRecord(filepath.Join(dir, c.ID()))
	if err != nil {
		t.Fatal(err)
	}
	if info, _ := os.Stat(recordPath(filepath.Join(dir, c.ID()))); info.Mode().Perm() != 0600 {
		t.Fatalf("record permissions %v", info.Mode().Perm())
	}
	raw, err := net.Dial("tcp", "127.0.0.1:"+strconv.Itoa(record.Port))
	if err != nil {
		t.Fatal(err)
	}
	defer raw.Close()
	_ = json.NewEncoder(raw).Encode(frame{Op: "hello", Token: "wrong", Protocol: Protocol})
	_ = raw.SetReadDeadline(time.Now().Add(3 * time.Second))
	var f frame
	if err = json.NewDecoder(raw).Decode(&f); err == nil {
		t.Fatalf("unauthenticated client was answered: %+v", f)
	}
}

func TestIncompatibleClientIsRefusedButCanStopTheHost(t *testing.T) {
	dir := root(t)
	c, pid := started(t, dir)
	_ = c.Detach()
	record, _ := ReadRecord(filepath.Join(dir, c.ID()))
	raw, err := net.Dial("tcp", "127.0.0.1:"+strconv.Itoa(record.Port))
	if err != nil {
		t.Fatal(err)
	}
	enc, dec := json.NewEncoder(raw), json.NewDecoder(raw)
	_ = enc.Encode(frame{Op: "hello", Token: record.Token, Protocol: Protocol + 1})
	var f frame
	if err = dec.Decode(&f); err != nil || f.Op != "error" || f.Protocol != Protocol {
		t.Fatalf("future client not refused with the host version: %+v %v", f, err)
	}
	raw.Close()
	if err = Stop(deadline(t), dir, c.ID()); err != nil {
		t.Fatal(err)
	}
	eventually(t, "Stop left the agent running", func() bool { return !alive(pid) })
}

func TestProxyForwardsToTheCurrentRunnerGateway(t *testing.T) {
	seen := make(chan string, 4)
	gateway := func(name string) *httptest.Server {
		return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			seen <- name + " " + r.URL.Path
			_, _ = io.WriteString(w, name)
		}))
	}
	first, second := gateway("first"), gateway("second")
	defer first.Close()
	defer second.Close()
	dir := root(t)
	c, _ := started(t, dir)
	get := func(url string) (int, string) {
		resp, err := http.Get(url)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		body, _ := io.ReadAll(resp.Body)
		return resp.StatusCode, string(body)
	}
	proxy := c.ProxyURL()
	if code, _ := get(proxy + "/v1/messages"); code != http.StatusServiceUnavailable {
		t.Fatalf("no upstream should be retryable, got %d", code)
	}
	_ = c.SetUpstream(first.URL + "/route-1")
	time.Sleep(50 * time.Millisecond)
	if code, body := get(proxy + "/v1/messages"); code != 200 || body != "first" || <-seen != "first /route-1/v1/messages" {
		t.Fatalf("proxy did not reach the gateway route: %d %s", code, body)
	}
	base := proxy[:strings.LastIndex(proxy, "/")]
	if code, _ := get(base + "/guessed/v1/messages"); code != http.StatusNotFound {
		t.Fatal("proxy forwarded a request without its key")
	}
	_ = c.Detach()
	time.Sleep(50 * time.Millisecond)
	if code, _ := get(proxy + "/v1/messages"); code != http.StatusServiceUnavailable {
		t.Fatal("a departed Runner's gateway stayed routed")
	}
	again, err := Attach(deadline(t), dir, c.ID(), 1)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Detach()
	if again.ProxyURL() != proxy {
		t.Fatal("proxy URL changed across attachment")
	}
	_ = again.SetUpstream(second.URL + "/route-2")
	time.Sleep(50 * time.Millisecond)
	if code, body := get(proxy + "/v1/messages"); code != 200 || body != "second" || <-seen != "second /route-2/v1/messages" {
		t.Fatalf("proxy not re-pointed at the new Runner: %d %s", code, body)
	}
}

func TestExitDiagnosticsAreRedacted(t *testing.T) {
	dir := root(t)
	ctx := deadline(t)
	c, err := Launch(ctx, dir, Options{})
	if err != nil {
		t.Fatal(err)
	}
	err = c.Spawn(ctx, Spec{Path: "/bin/sh", Args: []string{"-c", `printf 'failed with %s\n' "$FIXTURE_TOKEN" >&2; exit 1`}, Env: []string{"FIXTURE_TOKEN=credential-value"}, Cwd: os.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	_, exit, err := c.Next(ctx)
	if err != nil || exit == nil || exit.Outcome.ExitCode != 1 || !strings.Contains(exit.Stderr, "failed with [redacted]") {
		t.Fatalf("diagnostics: %+v %v", exit, err)
	}
	_ = c.Release()
}

func TestProbeDoesNotSupersedeTheAttachedClient(t *testing.T) {
	dir := root(t)
	c, pid := started(t, dir)
	status, err := Probe(deadline(t), dir, c.ID())
	if err != nil || !status.Attached || !status.Spawned || status.Exited || status.AgentPID == 0 || status.Protocol != Protocol {
		t.Fatalf("%+v %v", status, err)
	}
	_ = c.Send(json.RawMessage(`{"method":"echo","params":"still mine"}`))
	if name, _ := method(t, next(t, c)); name != "echoed" {
		t.Fatal("probe took over the attached client")
	}
	_ = c.Detach()
	time.Sleep(50 * time.Millisecond)
	if status, err = Probe(deadline(t), dir, c.ID()); err != nil || status.Attached {
		t.Fatalf("detached host still reported attached: %+v %v", status, err)
	}
	if !alive(pid) {
		t.Fatal("agent stopped")
	}
}

func TestHostWhoseStateWasDeletedStopsItsAgent(t *testing.T) {
	dir := root(t)
	c, pid := started(t, dir)
	_ = c.Detach()
	if err := os.RemoveAll(filepath.Join(dir, c.ID())); err != nil {
		t.Fatal(err)
	}
	eventually(t, "an unreachable host kept its agent running", func() bool { return !alive(pid) })
}

func TestReplayMarksOnlyLinesAPreviousClientReceived(t *testing.T) {
	dir := root(t)
	c, _ := started(t, dir)
	_ = c.Send(json.RawMessage(`{"method":"ask","params":"seen"}`))
	seen := next(t, c) // Received, never acknowledged: maybe acted on.
	_ = c.Detach()
	writer, err := Attach(deadline(t), dir, c.ID(), 0)
	if err != nil {
		t.Fatal(err)
	}
	_ = writer.Detach() // Took over without reading anything new.
	again, err := Attach(deadline(t), dir, c.ID(), 0)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Detach()
	for {
		m := next(t, again)
		if m.Seq == seen.Seq {
			if !m.Redelivered {
				t.Fatal("a request a previous Runner received was replayed as new")
			}
			break
		}
	}
	_ = again.Send(json.RawMessage(`{"method":"echo","params":"fresh"}`))
	if fresh := next(t, again); fresh.Redelivered {
		t.Fatal("a line no client had seen was marked redelivered")
	}
}

// collect reads chunks until the program exits, by channel.
func collect(t *testing.T, c *Client, upTo int) (map[string]string, uint64, *Exit) {
	t.Helper()
	out := map[string]string{}
	var last uint64
	for n := 0; upTo == 0 || n < upTo; n++ {
		m, exit, err := c.Next(deadline(t))
		if err != nil {
			t.Fatal(err)
		}
		if exit != nil {
			return out, last, exit
		}
		var chunk Chunk
		if err = json.Unmarshal(m.Line, &chunk); err != nil {
			t.Fatalf("not a chunk: %s", m.Line)
		}
		out[chunk.Channel] += string(chunk.Data)
		last = m.Seq
	}
	return out, last, nil
}

func streamSpec(script string) Spec {
	return Spec{Path: "/bin/sh", Args: []string{"-c", script}, Env: os.Environ(), Cwd: os.TempDir(), Stream: true}
}

// A managed command's output is bytes on two streams. The host keeps it, in
// order, across a Runner going away, and replays only what was not acknowledged.
func TestStreamedCommandOutputSurvivesReattachment(t *testing.T) {
	dir := root(t)
	ctx := deadline(t)
	c, err := Launch(ctx, dir, Options{})
	if err != nil {
		t.Fatal(err)
	}
	gate := filepath.Join(t.TempDir(), "gate")
	if err = c.Spawn(ctx, streamSpec(`printf one; while [ ! -e "`+gate+`" ]; do sleep 0.02; done; printf two; printf oops >&2; exit 3`)); err != nil {
		t.Fatal(err)
	}
	first, seq, _ := collect(t, c, 1)
	if first["stdout"] != "one" {
		t.Fatalf("first chunk: %v", first)
	}
	_ = c.Ack(seq)
	time.Sleep(50 * time.Millisecond) // Let the ack land before going away.
	if err = c.Detach(); err != nil {
		t.Fatal(err)
	}
	// The command finishes while no Runner is attached.
	if err = os.WriteFile(gate, nil, 0600); err != nil {
		t.Fatal(err)
	}
	time.Sleep(200 * time.Millisecond)
	again, err := Attach(ctx, dir, c.ID(), seq)
	if err != nil {
		t.Fatal(err)
	}
	rest, _, exit := collect(t, again, 0)
	if rest["stdout"] != "two" || rest["stderr"] != "oops" {
		t.Fatalf("output after re-attachment: %v (already acknowledged output must not repeat)", rest)
	}
	if exit == nil || !exit.Outcome.Started || exit.Outcome.ExitCode != 3 {
		t.Fatalf("exit: %+v", exit)
	}
	_ = again.Release()
	eventually(t, "released host did not exit", func() bool {
		_, err := os.Stat(recordPath(filepath.Join(dir, c.ID())))
		return os.IsNotExist(err)
	})
}

// A command has no stdin to close politely; cancelling it kills its group now.
func TestTerminatingAStreamedCommandKillsItWithoutGrace(t *testing.T) {
	dir := root(t)
	ctx := deadline(t)
	c, err := Launch(ctx, dir, Options{})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Spawn(ctx, streamSpec(`/bin/sleep 300 & echo $!; wait`)); err != nil {
		t.Fatal(err)
	}
	out, _, _ := collect(t, c, 1)
	child, _ := strconv.Atoi(strings.TrimSpace(out["stdout"]))
	if child == 0 || !alive(child) {
		t.Fatalf("background child not reported: %q", out["stdout"])
	}
	began := time.Now()
	if err = c.Terminate(); err != nil {
		t.Fatal(err)
	}
	_, _, exit := collect(t, c, 0)
	if exit == nil || !exit.Outcome.Cancelled {
		t.Fatalf("exit: %+v", exit)
	}
	if took := time.Since(began); took > 1500*time.Millisecond {
		t.Fatalf("cancellation waited %v: a command has no graceful stop", took)
	}
	eventually(t, "background child survived cancellation", func() bool { return !alive(child) })
}

// A Runner adopts hosts launched by any version inside its window, speaking the
// host's version; outside the window it refuses but can still stop the host.
func TestClientSpeaksTheHostVersionInsideItsWindow(t *testing.T) {
	dir := root(t)
	c, pid := started(t, dir)
	_ = c.Detach()
	defer func(saved [2]int) { supported = saved }(supported)

	supported = [2]int{Protocol, Protocol + 1} // A newer Runner.
	again, err := Attach(deadline(t), dir, c.ID(), 0)
	if err != nil {
		t.Fatalf("a newer Runner refused a host inside its window: %v", err)
	}
	if again.Protocol() != Protocol {
		t.Fatalf("spoke %d to a version %d host", again.Protocol(), Protocol)
	}
	_ = again.Detach()

	supported = [2]int{Protocol + 1, Protocol + 1} // The window moved past this host.
	if _, err = Attach(deadline(t), dir, c.ID(), 0); err != ErrIncompatible {
		t.Fatalf("host outside the window: %v", err)
	}
	if err = Stop(deadline(t), dir, c.ID()); err != nil {
		t.Fatal(err)
	}
	eventually(t, "Stop left the agent running", func() bool { return !alive(pid) })
}

// A host nobody attaches to for longer than its orphan limit stops its program
// rather than running it for no one; an attached one is never stopped by it.
func TestOrphanedHostStopsItsProgram(t *testing.T) {
	dir := root(t)
	ctx := deadline(t)
	attached, err := Launch(ctx, dir, Options{OrphanAfter: 300 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	if err = attached.Spawn(ctx, streamSpec(`exec /bin/sleep 300`)); err != nil {
		t.Fatal(err)
	}
	orphan, err := Launch(ctx, dir, Options{OrphanAfter: 300 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	if err = orphan.Spawn(ctx, streamSpec(`exec /bin/sleep 300`)); err != nil {
		t.Fatal(err)
	}
	kept, dropped := attached.AgentPID(), orphan.AgentPID()
	_ = orphan.Detach()
	eventually(t, "orphaned program kept running", func() bool { return !alive(dropped) })
	eventually(t, "orphaned host kept its directory", func() bool {
		_, err := os.Stat(filepath.Join(dir, orphan.ID()))
		return os.IsNotExist(err)
	})
	if !alive(kept) {
		t.Fatal("the orphan limit stopped a program whose Runner is attached")
	}
}
