package codex

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

func TestMain(m *testing.M) {
	if len(os.Args) > 1 && os.Args[1] == "app-server" {
		fixtureAppServer()
		return
	}
	os.Exit(m.Run())
}

// fixtureAppServer speaks the slice of the app-server protocol this adapter
// uses: one thread, one turn, one lazybox_command dynamic tool call per turn.
func fixtureAppServer() {
	out := bufio.NewWriter(os.Stdout)
	send := func(v any) { data, _ := json.Marshal(v); fmt.Fprintf(out, "%s\n", data); out.Flush() }
	s := bufio.NewScanner(os.Stdin)
	for s.Scan() {
		var m struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Result struct {
				Success      bool `json:"success"`
				ContentItems []struct {
					Text string `json:"text"`
				} `json:"contentItems"`
			} `json:"result"`
		}
		if json.Unmarshal(s.Bytes(), &m) != nil {
			os.Exit(1)
		}
		switch m.Method {
		case "initialize":
			send(map[string]any{"id": m.ID, "result": map[string]any{}})
		case "thread/start":
			send(map[string]any{"id": m.ID, "result": map[string]any{"model": "fixture-model", "thread": map[string]string{"id": "thread-1"}}})
		case "turn/start":
			send(map[string]any{"id": m.ID, "result": map[string]any{"turn": map[string]string{"id": "turn-1"}}})
			send(map[string]any{"method": "turn/started", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1"}}})
			send(map[string]any{"id": 41, "method": "item/tool/call", "params": map[string]any{"threadId": "thread-1", "turnId": "turn-1", "tool": "lazybox_command", "arguments": toolArguments()}})
		case "":
			if string(m.ID) == "41" {
				text := ""
				if len(m.Result.ContentItems) > 0 {
					text = m.Result.ContentItems[0].Text
				}
				send(map[string]any{"method": "item/agentMessage/delta", "params": map[string]any{"threadId": "thread-1", "delta": fmt.Sprintf("success=%v %s", m.Result.Success, text)}})
				if os.Getenv("FIXTURE_TURN_FAILS") != "" {
					// One native line the adapter turns into two events.
					send(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-1", "turn": map[string]any{"id": "turn-1", "status": "failed", "error": map[string]any{"message": "usage limit", "codexErrorInfo": "usageLimitExceeded"}}}})
				} else {
					send(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1", "status": "completed"}}})
				}
			}
		}
	}
}

// toolArguments is the fixture's one tool call: a start unless the test asks
// for another action through the environment the host passes on.
func toolArguments() map[string]any {
	if action := os.Getenv("FIXTURE_TOOL_ACTION"); action != "" {
		return map[string]any{"action": action, "id": "c1"}
	}
	return map[string]any{"action": "start", "argv": []string{"make", "deploy"}}
}

func fixtureRoot(t *testing.T) string {
	t.Helper()
	dir, err := os.MkdirTemp("", "lbx-codex-")
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

func receive(t *testing.T, s agent.Session, kind string) agent.Event {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	for {
		ev, err := s.Receive(ctx)
		if err != nil {
			t.Fatalf("waiting for %s: %v", kind, err)
		}
		if ev.Type == kind {
			return ev
		}
	}
}

// An approved command still running when the Runner went away may or may not
// have run. After re-attachment the adapter answers the redelivered call with an
// explicit unknown outcome and never runs the command a second time.
func TestRedeliveredApprovedCommandIsNeverRunTwice(t *testing.T) {
	root := fixtureRoot(t)
	self, _ := os.Executable()
	runs := make(chan string, 4)
	tools := func(ctx context.Context, name string, args json.RawMessage) (string, error) {
		runs <- string(args)
		<-ctx.Done() // A long command, interrupted by the Runner going away.
		return "", ctx.Err()
	}
	first, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = first.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "deploy"}); err != nil {
		t.Fatal(err)
	}
	approval := receive(t, first, "approval")
	if approval.RequestID != "n:41" || approval.Cursor == 0 {
		t.Fatalf("tool approval: %+v", approval)
	}
	if _, err = first.Apply(context.Background(), agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: "n:41", Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	if args := <-runs; !strings.Contains(args, "deploy") {
		t.Fatalf("command not started: %s", args)
	}
	state := first.(agent.Durable).DurableState()
	var recorded durable
	if json.Unmarshal(state, &recorded) != nil || recorded.Turn != "turn-1" || recorded.Host == "" {
		t.Fatalf("durable state lacks the live turn: %s", state)
	}
	if err = first.(agent.Detacher).Detach(); err != nil {
		t.Fatal(err)
	}

	again, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "thread-1", Cursor: approval.Cursor, Responded: map[string]bool{"n:41": true}}}})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if resolved := receive(t, again, "request_resolved"); resolved.Status != "outcome_unknown_after_restart" {
		t.Fatalf("redelivered approved call: %+v", resolved)
	}
	if reply := receive(t, again, "message_delta"); !strings.HasPrefix(reply.Text, "success=false") || !strings.Contains(reply.Text, "outcome is unknown") {
		t.Fatalf("app-server not told the outcome is unknown: %q", reply.Text)
	}
	receive(t, again, "turn_ended")
	select {
	case args := <-runs:
		t.Fatalf("command ran a second time after the restart: %s", args)
	default:
	}
	if again.Reference() != "thread-1" {
		t.Fatal("native thread reference lost")
	}
}

func TestRedeliveredUnansweredApprovalStaysPending(t *testing.T) {
	root := fixtureRoot(t)
	self, _ := os.Executable()
	tools := func(context.Context, string, json.RawMessage) (string, error) { return "deployed", nil }
	first, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	_, _ = first.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "deploy"})
	approval := receive(t, first, "approval")
	first.(agent.Committer).Commit(approval)
	state := first.(agent.Durable).DurableState()
	_ = first.(agent.Detacher).Detach()

	again, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "thread-1", Cursor: approval.Cursor}}})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if redelivered := receive(t, again, "approval"); redelivered.RequestID != "n:41" {
		t.Fatalf("pending approval not redelivered: %+v", redelivered)
	}
	if _, err = again.Apply(context.Background(), agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: "n:41", Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	if reply := receive(t, again, "message_delta"); reply.Text != "success=true deployed" {
		t.Fatalf("approval after re-attachment did not run the command once: %q", reply.Text)
	}
}

// Waiting for a command has no effect and the command outlived the Runner, so
// an approved wait in flight at the restart just runs again.
func TestRedeliveredApprovedWaitRunsAgain(t *testing.T) {
	t.Setenv("FIXTURE_TOOL_ACTION", "wait")
	root := fixtureRoot(t)
	self, _ := os.Executable()
	runs := make(chan string, 4)
	interrupted := make(chan struct{})
	tools := func(ctx context.Context, name string, args json.RawMessage) (string, error) {
		runs <- string(args)
		select {
		case <-interrupted:
			return `{"done":true,"exitCode":0}`, nil
		default:
		}
		close(interrupted)
		<-ctx.Done() // The first wait is cut short by the Runner going away.
		return "", ctx.Err()
	}
	first, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	_, _ = first.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "wait"})
	approval := receive(t, first, "approval")
	if _, err = first.Apply(context.Background(), agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: "n:41", Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	<-runs
	state := first.(agent.Durable).DurableState()
	_ = first.(agent.Detacher).Detach()

	again, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "thread-1", Cursor: approval.Cursor, Responded: map[string]bool{"n:41": true}}}})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if reply := receive(t, again, "message_delta"); reply.Text != `success=true {"done":true,"exitCode":0}` {
		t.Fatalf("redelivered wait: %q", reply.Text)
	}
	if args := <-runs; !strings.Contains(args, "wait") {
		t.Fatalf("second run: %s", args)
	}
}

// A native line that yields several events is acknowledged only with its last
// one, so a Runner dying between them gets the whole line again.
func TestEventsFromOneLineAreMarkedPartialUntilTheLast(t *testing.T) {
	t.Setenv("FIXTURE_TURN_FAILS", "1")
	root := fixtureRoot(t)
	self, _ := os.Executable()
	tools := func(context.Context, string, json.RawMessage) (string, error) { return "ok", nil }
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	_, _ = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "deploy"})
	receive(t, s, "approval")
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "yes", Kind: agent.Approve, RequestID: "n:41", Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	failure := receive(t, s, "failure")
	ended := receive(t, s, "turn_ended")
	if failure.Cursor == 0 || failure.Cursor != ended.Cursor {
		t.Fatalf("events of one line carry cursors %d and %d", failure.Cursor, ended.Cursor)
	}
	if !failure.Partial || ended.Partial {
		t.Fatalf("partial marks: failure %v, turn_ended %v", failure.Partial, ended.Partial)
	}
}
