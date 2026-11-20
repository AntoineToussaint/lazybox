package acp

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// The fake ACP agent every conformance test drives through the real adapter:
// scripts/fixtures/acp-fake.mjs, the same one the ACP UI suite uses.
func fakeAgent(t *testing.T) Agent {
	t.Helper()
	node, err := exec.LookPath("node")
	if err != nil {
		t.Skip("node is not on the PATH; the fake ACP agent needs it")
	}
	script, err := filepath.Abs("../../../../../../../scripts/fixtures/acp-fake.mjs")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = os.Stat(script); err != nil {
		t.Fatalf("fake ACP agent missing: %v", err)
	}
	return Agent{ID: "fake", Title: "Fake agent", Command: node, Args: []string{script}}
}

// workspace is a conversation folder with a README, its symlinks resolved.
func workspace(t *testing.T) string {
	t.Helper()
	dir, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(dir, "README.md"), []byte("# Fixture\nhello readme\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

// commandTools stands in for the Runner's managed commands (lazybox_command).
type commandTools struct {
	mu    sync.Mutex
	calls []map[string]any
	// refuse makes a start fail the way a read-only conversation's does.
	refuse bool
}

func (c *commandTools) run(_ context.Context, name string, args json.RawMessage) (string, error) {
	if name != agent.CommandTool {
		return "", agent.ErrUnsupported
	}
	var call map[string]any
	_ = json.Unmarshal(args, &call)
	c.mu.Lock()
	c.calls = append(c.calls, call)
	c.mu.Unlock()
	status := `{"id":"cmd-1","done":true,"exitCode":0}`
	switch call["action"] {
	case "start":
		if c.refuse {
			return "", errors.New("managed command start is disabled for read-only executions")
		}
		return `{"id":"cmd-1","done":false}`, nil
	case "tail":
		return `{"output":"fake-tour-check\n","truncated":false,"status":` + status + `}`, nil
	case "wait", "cancel":
		return status, nil
	}
	return "", errors.New("unknown action")
}

func (c *commandTools) actions() []string {
	c.mu.Lock()
	defer c.mu.Unlock()
	out := []string{}
	for _, call := range c.calls {
		out = append(out, call["action"].(string))
	}
	return out
}

func open(t *testing.T, o agent.Open) agent.Session {
	t.Helper()
	if o.Cwd == "" {
		o.Cwd = workspace(t)
	}
	// The context is the agent's lifetime, as the engine's is.
	s, err := Adapter{Agent: fakeAgent(t)}.Open(context.Background(), o)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = s.Close() })
	return s
}

// until collects events until one matches, failing after a while.
func until(t *testing.T, s agent.Session, match func(agent.Event) bool) []agent.Event {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	var seen []agent.Event
	for {
		ev, err := s.Receive(ctx)
		if err != nil {
			t.Fatalf("waiting: %v; seen %s", err, summary(seen))
		}
		seen = append(seen, ev)
		if match(ev) {
			return seen
		}
	}
}

func typed(kind string) func(agent.Event) bool {
	return func(ev agent.Event) bool { return ev.Type == kind }
}

func summary(events []agent.Event) string {
	parts := []string{}
	for _, ev := range events {
		parts = append(parts, ev.Type+":"+ev.Status+":"+ev.Text)
	}
	return strings.Join(parts, " | ")
}

func text(events []agent.Event) string {
	var b strings.Builder
	for _, ev := range events {
		if ev.Type == "message_delta" {
			b.WriteString(ev.Text)
		}
	}
	return b.String()
}

func say(t *testing.T, s agent.Session, words string) []agent.Event {
	t.Helper()
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "op-" + words, Kind: agent.Message, Text: words}); err != nil {
		t.Fatal(err)
	}
	return until(t, s, typed("turn_ended"))
}

func toolCalls(events []agent.Event) map[string]map[string]any {
	out := map[string]map[string]any{}
	for _, ev := range events {
		if ev.Type != "tool_activity" {
			continue
		}
		var call map[string]any
		_ = json.Unmarshal(ev.Data, &call)
		out[call["id"].(string)] = call
	}
	return out
}

func TestTourMapsUpdatesApprovalsFilesAndTerminals(t *testing.T) {
	tools := &commandTools{}
	cwd := workspace(t)
	s := open(t, agent.Open{Cwd: cwd, Permission: "ask", Tools: tools.run, Context: "LAZYBOX-CONTEXT-MARKER brief"})
	opened := until(t, s, func(ev agent.Event) bool {
		return ev.Type == "command_catalog" && strings.Contains(string(ev.Data), "compress")
	})
	var catalog struct {
		Commands []struct{ Name, ArgumentHint string }
	}
	if json.Unmarshal(opened[len(opened)-1].Data, &catalog) != nil || len(catalog.Commands) != 2 || catalog.Commands[1].ArgumentHint != "add "+"<"+"text>" {
		t.Fatalf("slash commands: %s", opened[len(opened)-1].Data)
	}
	if _, err := agent.TitleOf(context.Background(), s, "tour"); err == nil {
		t.Fatal("a title before the agent named the conversation")
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "tour", Kind: agent.Message, Text: "tour"}); err != nil {
		t.Fatal(err)
	}
	before := until(t, s, typed("approval"))
	if title, err := agent.TitleOf(context.Background(), s, "tour"); err != nil || title != "Fake tour" {
		t.Fatalf("the agent's own title: %q %v", title, err)
	}
	for _, ev := range before {
		if ev.Type == "turn_started" {
			break
		}
		if ev.Type == "agent_permission" && ev.Text == "default" {
			continue
		}
		if ev.Type != "command_catalog" {
			t.Fatalf("the turn must start before anything of it: %s", summary(before))
		}
	}
	approval := before[len(before)-1]
	var card struct {
		Tool   string         `json:"tool"`
		Title  string         `json:"title"`
		Input  map[string]any `json:"input"`
		Always []AlwaysChoice `json:"always"`
	}
	if err := json.Unmarshal(approval.Data, &card); err != nil {
		t.Fatal(err)
	}
	if card.Tool != "Write" || card.Title != "Write notes.txt" || card.Input["file_path"] != filepath.Join(cwd, "notes.txt") || card.Input["content"] != "first line\nsecond line\n" {
		t.Fatalf("approval card: %s", approval.Data)
	}
	if len(card.Always) != 1 || card.Always[0].ID != "allow-always" || card.Always[0].Label != "edits like this" {
		t.Fatalf("always choices: %+v", card.Always)
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: approval.RequestID, Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	after := until(t, s, typed("turn_ended"))
	events := append(before, after...)
	if end := after[len(after)-1]; end.Status != "completed" {
		t.Fatalf("turn end: %+v", end)
	}
	if got := text(events); got != "Done: read the README, wrote notes.txt and ran the check." {
		t.Fatalf("message: %q", got)
	}
	written, err := os.ReadFile(filepath.Join(cwd, "notes.txt"))
	if err != nil || string(written) != "first line\nsecond line\n" {
		t.Fatalf("fs/write_text_file did not write the workspace file: %q %v", written, err)
	}
	calls := toolCalls(events)
	if read := calls["call-read"]; read["name"] != "Read" || read["status"] != "completed" || !strings.Contains(read["output"].(string), "hello readme") {
		t.Fatalf("read line: %v", read)
	}
	if edit := calls["call-edit"]; edit["name"] != "Write" || edit["status"] != "completed" {
		t.Fatalf("edit line: %v", edit)
	}
	run := calls["call-run"]
	if run["name"] != "Bash" || run["status"] != "completed" || run["commandId"] != "cmd-1" || run["exitCode"] != float64(0) || run["output"] != "fake-tour-check\n" {
		t.Fatalf("command line: %v", run)
	}
	if command := run["input"].(map[string]any)["command"]; command != "echo fake-tour-check" {
		t.Fatalf("command input: %v", command)
	}
	plan := calls["plan:turn-1"]
	entries := plan["input"].(map[string]any)["plan"].([]any)
	if plan["name"] != "update_plan" || len(entries) != 3 || entries[2].(map[string]any)["status"] != "completed" {
		t.Fatalf("plan: %v", plan)
	}
	if got := strings.Join(tools.actions(), ","); got != "start,wait,tail,tail,tail" {
		t.Fatalf("terminal calls: %s", got)
	}
	start := tools.calls[0]
	if argv, _ := json.Marshal(start["argv"]); string(argv) != `["echo","fake-tour-check"]` || start["cwd"] != cwd || start["title"] != "echo fake-tour-check" {
		t.Fatalf("terminal/create: %v", start)
	}
	var usage, thinking bool
	for _, ev := range events {
		if ev.Type == "usage" && strings.Contains(string(ev.Data), `"amount":0.0123`) {
			usage = true
		}
		if ev.Type == "activity" && ev.Status == "thinking" {
			thinking = true
			if ev.Text != "" || len(ev.Data) > 0 {
				t.Fatal("private reasoning journaled")
			}
		}
		if strings.Contains(ev.Text, "private reasoning") || strings.Contains(string(ev.Data), "private reasoning") {
			t.Fatal("private reasoning journaled")
		}
	}
	if !usage || !thinking {
		t.Fatalf("usage %v, thinking %v: %s", usage, thinking, summary(events))
	}
	if reply := text(say(t, s, "context?")); reply != "context=yes mode=default model=fake-pro" {
		t.Fatalf("context: %q", reply)
	}
}

func TestDecisionsMapOntoTheAgentsOptions(t *testing.T) {
	s := open(t, agent.Open{Permission: "ask"})
	ask := func(op agent.Operation) string {
		t.Helper()
		if _, err := s.Apply(context.Background(), agent.Operation{ID: "ask", Kind: agent.Message, Text: "ask"}); err != nil {
			t.Fatal(err)
		}
		approval := until(t, s, typed("approval"))
		op.RequestID = approval[len(approval)-1].RequestID
		op.Kind, op.ID = agent.Approve, "answer"
		if _, err := s.Apply(context.Background(), op); err != nil {
			t.Fatal(err)
		}
		return text(until(t, s, typed("turn_ended")))
	}
	if got := ask(agent.Operation{Decision: "accept"}); got != "decision=allow-once" {
		t.Fatalf("allow once: %q", got)
	}
	if got := ask(agent.Operation{Decision: "accept", Always: "allow-always"}); got != "decision=allow-always" {
		t.Fatalf("always: %q", got)
	}
	if got := ask(agent.Operation{Decision: "decline", Reason: "not on Friday"}); got != "decision=reject-once" {
		t.Fatalf("decline: %q", got)
	}
	// The reason reaches the agent with the next message.
	log := filepath.Join(t.TempDir(), "acp.log")
	t.Setenv("ACP_FAKE_LOG", log)
	s2 := open(t, agent.Open{Permission: "ask"})
	if _, err := s2.Apply(context.Background(), agent.Operation{ID: "ask", Kind: agent.Message, Text: "ask"}); err != nil {
		t.Fatal(err)
	}
	approval := until(t, s2, typed("approval"))
	if _, err := s2.Apply(context.Background(), agent.Operation{ID: "always-bogus", Kind: agent.Approve, RequestID: approval[len(approval)-1].RequestID, Decision: "accept", Always: "bogus"}); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("an always choice the request did not offer: %v", err)
	}
	if _, err := s2.Apply(context.Background(), agent.Operation{ID: "no", Kind: agent.Approve, RequestID: approval[len(approval)-1].RequestID, Decision: "decline", Reason: "not on Friday"}); err != nil {
		t.Fatal(err)
	}
	until(t, s2, typed("turn_ended"))
	say(t, s2, "hello")
	data, _ := os.ReadFile(log)
	if !strings.Contains(string(data), `I declined “Run make deploy”. not on Friday`) {
		t.Fatalf("decline reason not delivered with the next message:\n%s", data)
	}
}

func TestModesAnswerPermissionsWithoutAsking(t *testing.T) {
	yolo := open(t, agent.Open{Permission: "yolo"})
	events := say(t, yolo, "ask")
	for _, ev := range events {
		if ev.Type == "approval" {
			t.Fatal("YOLO asked the person")
		}
	}
	if got := text(events); got != "decision=allow-once" {
		t.Fatalf("yolo: %q", got)
	}
	if got := text(say(t, yolo, "context?")); !strings.Contains(got, "mode=yolo") {
		t.Fatalf("the agent's own yolo mode was not set: %q", got)
	}
	cwd := workspace(t)
	tools := &commandTools{refuse: true}
	readOnly := open(t, agent.Open{Cwd: cwd, Permission: "read-only", Tools: tools.run})
	if got := text(say(t, readOnly, "ask")); got != "decision=reject-once" {
		t.Fatalf("read-only allowed a command: %q", got)
	}
	if got := text(say(t, readOnly, "context?")); !strings.Contains(got, "mode=plan") {
		t.Fatalf("read-only did not set the plan mode: %q", got)
	}
	if got := text(say(t, readOnly, "write:"+filepath.Join(cwd, "x.txt"))); !strings.Contains(got, "read-only") {
		t.Fatalf("read-only wrote a file: %q", got)
	}
	if _, err := os.Stat(filepath.Join(cwd, "x.txt")); err == nil {
		t.Fatal("file written in a read-only conversation")
	}
	// The mode changes live, on the agent and in what Lazybox allows.
	if _, err := readOnly.Apply(context.Background(), agent.Operation{ID: "mode", Kind: agent.Permission, Permission: "ask"}); err != nil {
		t.Fatal(err)
	}
	if got := text(say(t, readOnly, "write:"+filepath.Join(cwd, "x.txt"))); got != "wrote" {
		t.Fatalf("write after leaving read-only: %q", got)
	}
	if got := text(say(t, readOnly, "context?")); !strings.Contains(got, "mode=default") {
		t.Fatalf("ask did not set the default mode: %q", got)
	}
}

func TestFilesStayInsideTheWorkspace(t *testing.T) {
	cwd := workspace(t)
	outside := t.TempDir()
	secret := filepath.Join(outside, "secret.txt")
	_ = os.WriteFile(secret, []byte("secret"), 0o600)
	if err := os.Symlink(outside, filepath.Join(cwd, "link")); err != nil {
		t.Fatal(err)
	}
	_ = os.MkdirAll(filepath.Join(cwd, ".git"), 0o755)
	s := open(t, agent.Open{Cwd: cwd, Permission: "ask"})
	for _, tc := range []struct{ words, want string }{
		{"read:" + filepath.Join(cwd, "README.md"), "content=# Fixture\nhello readme\n"},
		{"read:" + secret, "error -32602: outside this conversation's workspace"},
		{"read:" + filepath.Join(cwd, "link", "secret.txt"), "error -32602: outside this conversation's workspace"},
		{"read:" + filepath.Join(cwd, "missing.txt"), "error -32002: no such file"},
		{"read:README.md", "error -32602: paths must be absolute"},
		{"write:" + filepath.Join(cwd, "..", "escape.txt"), "error -32602: outside this conversation's workspace"},
		{"write:" + filepath.Join(cwd, "link", "planted.txt"), "error -32602: outside this conversation's workspace"},
		{"write:" + filepath.Join(cwd, ".git", "config"), "error -32602: Lazybox does not let agents write inside .git"},
		{"write:" + filepath.Join(cwd, "deep", "new", "file.txt"), "wrote"},
		{"run:ls", "error -32602: this conversation cannot run commands"},
	} {
		if got := text(say(t, s, tc.words)); !strings.HasPrefix(got, tc.want) {
			t.Errorf("%s: got %q, want %q…", tc.words, got, tc.want)
		}
	}
	if _, err := os.Stat(filepath.Join(outside, "planted.txt")); err == nil {
		t.Fatal("a write followed a symlink out of the workspace")
	}
	// A file the person attached may be read, though it is outside.
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "attach", Kind: agent.Message, Text: "read:" + secret, Files: []agent.File{{Name: "secret.txt", Path: secret}}}); err != nil {
		t.Fatal(err)
	}
	if got := text(until(t, s, typed("turn_ended"))); got != "content=secret" {
		t.Fatalf("attached file: %q", got)
	}
}

func TestTerminalCommandLineRunsThroughAShell(t *testing.T) {
	tools := &commandTools{}
	s := open(t, agent.Open{Permission: "ask", Tools: tools.run})
	if got := text(say(t, s, "run:npm test && echo ok")); got != "exit=0 output=fake-tour-check\n" {
		t.Fatalf("run: %q", got)
	}
	argv := []string{}
	for _, a := range tools.calls[0]["argv"].([]any) {
		argv = append(argv, a.(string))
	}
	if strings.Join(argv, "|") != "/bin/sh|-c|npm test && echo ok" {
		t.Fatalf("argv: %q", argv)
	}
}

func TestCancelAndFailuresEndTheTurn(t *testing.T) {
	s := open(t, agent.Open{Permission: "ask"})
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "wait", Kind: agent.Message, Text: "wait"}); err != nil {
		t.Fatal(err)
	}
	until(t, s, typed("message_delta"))
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "stop", Kind: agent.Interrupt}); err != nil {
		t.Fatal(err)
	}
	if end := until(t, s, typed("turn_ended")); end[len(end)-1].Status != "interrupted" {
		t.Fatalf("cancel: %s", summary(end))
	}
	for words, want := range map[string]string{"fail": "provider_error", "auth": "auth_required", "refuse": "refusal", "limit": "budget_exceeded"} {
		events := say(t, s, words)
		var failure *agent.Event
		for i := range events {
			if events[i].Type == "failure" {
				failure = &events[i]
			}
		}
		if failure == nil || failure.Status != want || failure.Text == "" || events[len(events)-1].Status != "failed" {
			t.Errorf("%s: %s", words, summary(events))
		}
		if !agent.TurnFault(failure.Status) && !agent.LimitFault(failure.Status) {
			t.Errorf("%s: the next message must be able to continue: %s", words, failure.Status)
		}
	}
	// The conversation goes on after a failed turn.
	if got := text(say(t, s, "hello")); got != "Hello from the fake ACP agent." {
		t.Fatalf("after failures: %q", got)
	}
}

// When the agent process dies, Receive ends after reporting it: the engine
// learns the provider is gone instead of waiting for events nothing will
// send. The exit is a Runner fault, not a turn fault a message continues.
func TestAgentDeathEndsReceive(t *testing.T) {
	s := open(t, agent.Open{})
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "die", Kind: agent.Message, Text: "die"}); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	var seen []agent.Event
	for {
		ev, err := s.Receive(ctx)
		if err != nil {
			if ctx.Err() != nil {
				t.Fatalf("Receive never ended after the agent died; seen %s", summary(seen))
			}
			break
		}
		seen = append(seen, ev)
	}
	last := seen[len(seen)-1]
	if last.Type != "failure" || last.Status != "provider_process_exit" || agent.TurnFault(last.Status) {
		t.Fatalf("the agent's exit was not reported as a Runner fault: %s", summary(seen))
	}
}

func TestStartupFailuresSayWhatToDo(t *testing.T) {
	a := Adapter{Agent: fakeAgent(t)}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	t.Setenv("ACP_FAKE_AUTH", "1")
	_, err := a.Open(ctx, agent.Open{Cwd: workspace(t)})
	if err == nil || !strings.Contains(err.Error(), "Fake agent needs you to sign in") || !strings.Contains(err.Error(), "Fake sign-in") {
		t.Fatalf("sign-in: %v", err)
	}
	t.Setenv("ACP_FAKE_AUTH", "")
	t.Setenv("ACP_FAKE_VERSION", "2")
	if _, err = a.Open(ctx, agent.Open{Cwd: workspace(t)}); err == nil || !strings.Contains(err.Error(), "ACP version 2") {
		t.Fatalf("version: %v", err)
	}
	t.Setenv("ACP_FAKE_VERSION", "")
	t.Setenv("ACP_FAKE_NO_RESUME", "1")
	if _, err = a.Open(ctx, agent.Open{Cwd: workspace(t), Resume: "fake-session-1"}); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("resume without support: %v", err)
	}
	if _, err = (Adapter{Agent: Agent{ID: "gone", Title: "Gone", Command: "/nonexistent/acp-agent"}}).Open(ctx, agent.Open{Cwd: workspace(t)}); err == nil {
		t.Fatal("a missing program started")
	}
}

func TestModelSwitchingAndCapabilities(t *testing.T) {
	s := open(t, agent.Open{Model: "fake-flash"})
	caps := s.Capabilities()
	if !caps.LiveModel || !caps.LivePermission || !caps.Approvals || !caps.Images || !caps.Resume || caps.Steer || caps.Questions {
		t.Fatalf("capabilities: %+v", caps)
	}
	opened := until(t, s, func(ev agent.Event) bool {
		return ev.Type == "command_catalog" && strings.Contains(string(ev.Data), "models")
	})
	if catalog := opened[len(opened)-1]; !strings.Contains(string(catalog.Data), `"displayName":"Fake Flash"`) {
		t.Fatalf("models: %s", catalog.Data)
	}
	if got := text(say(t, s, "context?")); !strings.Contains(got, "model=fake-flash") {
		t.Fatalf("model at start: %q", got)
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "m", Kind: agent.Model, Model: "fake-pro"}); err != nil {
		t.Fatal(err)
	}
	if got := text(say(t, s, "context?")); !strings.Contains(got, "model=fake-pro") {
		t.Fatalf("model switched live: %q", got)
	}
	if settings := string(s.(agent.RuntimeSettings).SessionSettings()); !strings.Contains(settings, `"model":"fake-pro"`) {
		t.Fatalf("a handoff keeps the model: %s", settings)
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "m2", Kind: agent.Model, Model: "gpt-9"}); !errors.Is(err, agent.ErrInvalidModel) || !strings.Contains(err.Error(), "fake-pro, fake-flash") {
		t.Fatalf("unknown model: %v", err)
	}
	if s.Reference() != "fake-session-1" {
		t.Fatalf("reference: %q", s.Reference())
	}
}

func TestResumeDoesNotRecordReplayedHistory(t *testing.T) {
	s := open(t, agent.Open{Resume: "fake-session-1", Context: "LAZYBOX-CONTEXT-MARKER"})
	events := say(t, s, "hello")
	if strings.Contains(summary(events), "old answer") || text(events) != "Hello from the fake ACP agent." {
		t.Fatalf("resume: %s", summary(events))
	}
	// An agent that can only load replays the history; the journal has it already.
	t.Setenv("ACP_FAKE_LOAD_ONLY", "1")
	loaded := open(t, agent.Open{Resume: "fake-session-1"})
	events = say(t, loaded, "hello")
	if strings.Contains(summary(events), "old answer") || text(events) != "Hello from the fake ACP agent." {
		t.Fatalf("load: %s", summary(events))
	}
}

// fixtureRoot is a private directory for session hosts, stopped after the test.
func fixtureRoot(t *testing.T) string {
	t.Helper()
	dir, err := os.MkdirTemp("", "lbx-acp-")
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

// commitAll acknowledges every event, as the engine does once it records them.
func commitAll(s agent.Session, events []agent.Event) {
	for _, ev := range events {
		s.(agent.Committer).Commit(ev)
	}
}

func TestTurnOutlivesARunnerRestart(t *testing.T) {
	t.Setenv("ACP_FAKE_SLOW_MS", "1500")
	root := fixtureRoot(t)
	cwd := workspace(t)
	a := Adapter{Agent: fakeAgent(t)}
	first, err := a.Open(context.Background(), agent.Open{Cwd: cwd, Permission: "ask", Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	if agent.HostOf(first) == "" {
		t.Fatal("not hosted")
	}
	if _, err = first.Apply(context.Background(), agent.Operation{ID: "slow", Kind: agent.Message, Text: "slow"}); err != nil {
		t.Fatal(err)
	}
	seen := until(t, first, typed("message_delta"))
	commitAll(first, seen)
	state := first.(agent.Durable).DurableState()
	cursor := seen[len(seen)-1].Cursor
	if err = first.(agent.Detacher).Detach(); err != nil {
		t.Fatal(err)
	}
	// The agent goes on without a Runner; a new one attaches mid-turn.
	time.Sleep(200 * time.Millisecond)
	again, err := a.Open(context.Background(), agent.Open{Cwd: cwd, Permission: "ask", Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "fake-session-1", Cursor: cursor}}})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	rest := until(t, again, typed("turn_ended"))
	if text(rest) != "slow part two." || rest[len(rest)-1].Status != "completed" || rest[len(rest)-1].TurnID != "turn-1" {
		t.Fatalf("after re-attach: %s", summary(rest))
	}
	commitAll(again, rest)
	// The conversation continues on the same agent process.
	if got := text(say(t, again, "hello")); got != "Hello from the fake ACP agent." {
		t.Fatalf("next turn: %q", got)
	}
	if again.Reference() != "fake-session-1" {
		t.Fatal("session reference lost")
	}
}

func TestPendingPermissionIsRedeliveredAfterARestart(t *testing.T) {
	root := fixtureRoot(t)
	cwd := workspace(t)
	a := Adapter{Agent: fakeAgent(t)}
	first, err := a.Open(context.Background(), agent.Open{Cwd: cwd, Permission: "ask", Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = first.Apply(context.Background(), agent.Operation{ID: "ask", Kind: agent.Message, Text: "ask"}); err != nil {
		t.Fatal(err)
	}
	seen := until(t, first, typed("approval"))
	commitAll(first, seen)
	approval := seen[len(seen)-1]
	state := first.(agent.Durable).DurableState()
	_ = first.(agent.Detacher).Detach()

	// Unanswered: asked again.
	again, err := a.Open(context.Background(), agent.Open{Cwd: cwd, Permission: "ask", Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "fake-session-1", Cursor: approval.Cursor}}})
	if err != nil {
		t.Fatal(err)
	}
	redelivered := until(t, again, typed("approval"))
	if r := redelivered[len(redelivered)-1]; r.RequestID != approval.RequestID {
		t.Fatalf("redelivered %q, want %q", r.RequestID, approval.RequestID)
	}
	commitAll(again, redelivered)
	state = again.(agent.Durable).DurableState()
	_ = again.(agent.Detacher).Detach()

	// Answered before the restart, never received: the answer is given again.
	third, err := a.Open(context.Background(), agent.Open{Cwd: cwd, Permission: "ask", Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "fake-session-1", Cursor: approval.Cursor, Responded: map[string]agent.Operation{approval.RequestID: {Kind: agent.Approve, RequestID: approval.RequestID, Decision: "accept", Always: "allow-always"}}}}})
	if err != nil {
		t.Fatal(err)
	}
	defer third.Close()
	end := until(t, third, typed("turn_ended"))
	for _, ev := range end {
		if ev.Type == "approval" {
			t.Fatal("an answered request was asked again")
		}
	}
	if got := text(end); got != "decision=allow-always" {
		t.Fatalf("answer after restart: %q", got)
	}
}
