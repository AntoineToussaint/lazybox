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

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
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
			Params struct {
				Input []struct {
					Text string `json:"text"`
				} `json:"input"`
				Cursor         string          `json:"cursor"`
				ApprovalPolicy string          `json:"approvalPolicy"`
				SandboxPolicy  json.RawMessage `json:"sandboxPolicy"`
			} `json:"params"`
			Result struct {
				Decision     json.RawMessage `json:"decision"`
				Success      bool            `json:"success"`
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
			result := map[string]any{}
			if v := os.Getenv("FIXTURE_CODEX_VERSION"); v != "" {
				result["userAgent"] = "lazybox_v2_testbench/" + v + " (Mac OS 27.2.1; arm64) fixture (lazybox_v2_testbench; 0.1.0)"
			}
			send(map[string]any{"id": m.ID, "result": result})
		case "thread/start":
			send(map[string]any{"id": m.ID, "result": map[string]any{"model": "fixture-model", "thread": map[string]string{"id": "thread-1"}}})
		case "turn/start":
			if os.Getenv("FIXTURE_EXPECT_YOLO_TURN") != "" && (m.Params.ApprovalPolicy != "never" || string(m.Params.SandboxPolicy) != `{"type":"dangerFullAccess"}`) {
				send(map[string]any{"id": m.ID, "error": map[string]any{"code": -32602, "message": "turn did not receive YOLO policy"}})
				continue
			}
			send(map[string]any{"id": m.ID, "result": map[string]any{"turn": map[string]string{"id": "turn-1"}}})
			send(map[string]any{"method": "turn/started", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1"}}})
			if os.Getenv("FIXTURE_EXIT_IN_TURN") != "" {
				// The app-server dies mid-turn.
				os.Exit(3)
			}
			if os.Getenv("FIXTURE_ODD_NOTIFICATIONS") != "" {
				// Notifications the adapter does not use, whose error is a string.
				send(map[string]any{"method": "account/login/completed", "params": map[string]any{"loginId": nil, "success": false, "error": "login cancelled"}})
				send(map[string]any{"method": "mcpServer/startupStatus/updated", "params": map[string]any{"threadId": "thread-1", "name": "docs", "status": "failed", "error": "spawn failed"}})
				// One it uses, with a field of another type than it promises.
				send(map[string]any{"method": "item/agentMessage/delta", "params": map[string]any{"threadId": "thread-1", "delta": map[string]string{"text": "odd"}}})
				send(map[string]any{"method": "item/agentMessage/delta", "params": map[string]any{"threadId": "thread-1", "delta": "still here"}})
				send(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1", "status": "completed"}}})
				continue
			}
			if os.Getenv("FIXTURE_COMPACTION") != "" {
				// A compaction as 0.169 reports it: the item, then the deprecated notification.
				item := map[string]any{"type": "contextCompaction", "id": "compact-1"}
				send(map[string]any{"method": "thread/tokenUsage/updated", "params": map[string]any{"threadId": "thread-1", "turnId": "turn-1", "tokenUsage": map[string]any{"total": map[string]int{"totalTokens": 400000}, "last": map[string]int{"totalTokens": 156000}, "modelContextWindow": 200000}}})
				send(map[string]any{"method": "item/started", "params": map[string]any{"threadId": "thread-1", "turnId": "turn-1", "item": item}})
				send(map[string]any{"method": "item/completed", "params": map[string]any{"threadId": "thread-1", "turnId": "turn-1", "item": item}})
				send(map[string]any{"method": "thread/compacted", "params": map[string]any{"threadId": "thread-1", "turnId": "turn-1"}})
				send(map[string]any{"method": "item/agentMessage/delta", "params": map[string]any{"threadId": "thread-1", "delta": "after"}})
				send(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1", "status": "completed"}}})
				continue
			}
			if os.Getenv("FIXTURE_PERMISSIONS_APPROVAL") != "" {
				send(map[string]any{"id": 43, "method": "item/permissions/requestApproval", "params": map[string]any{"threadId": "thread-1", "turnId": "turn-1", "itemId": "item-1", "environmentId": nil, "startedAtMs": 1, "cwd": "/w", "reason": nil, "permissions": map[string]any{"network": map[string]any{"enabled": true}, "fileSystem": nil}}})
				continue
			}
			if os.Getenv("FIXTURE_NATIVE_APPROVAL") != "" {
				send(map[string]any{"id": 42, "method": "item/commandExecution/requestApproval", "params": map[string]any{"threadId": "thread-1", "turnId": "turn-1", "command": "rm -rf build", "proposedExecpolicyAmendment": []string{"rm", "-rf"}}})
				switch os.Getenv("FIXTURE_APPROVAL_ENDS") {
				case "resolved":
					// The app-server resolves the request itself, with no reply.
					send(map[string]any{"method": "serverRequest/resolved", "params": map[string]any{"threadId": "thread-1", "requestId": 42}})
					send(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1", "status": "interrupted"}}})
				case "turn":
					// The turn ends while the request is unanswered.
					send(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1", "status": "interrupted"}}})
				}
				continue
			}
			tool := "lazybox_command"
			if name := os.Getenv("FIXTURE_TOOL_NAME"); name != "" {
				tool = name
			}
			send(map[string]any{"id": 41, "method": "item/tool/call", "params": map[string]any{"threadId": "thread-1", "turnId": "turn-1", "tool": tool, "arguments": toolArguments()}})
		case "turn/steer":
			send(map[string]any{"id": m.ID, "result": map[string]any{"turnId": "turn-1"}})
			if len(m.Params.Input) > 0 {
				send(map[string]any{"method": "item/agentMessage/delta", "params": map[string]any{"threadId": "thread-1", "delta": "steer=" + m.Params.Input[0].Text}})
			}
		case "config/read":
			send(map[string]any{"id": m.ID, "result": map[string]any{"config": map[string]any{}}})
		case "model/list":
			// Two pages, a hidden preset and a repeat: the picker gets three models.
			if os.Getenv("FIXTURE_NO_MODEL_LIST") != "" {
				send(map[string]any{"id": m.ID, "error": map[string]any{"code": -32601, "message": "unknown method"}})
			} else if m.Params.Cursor == "" {
				send(map[string]any{"id": m.ID, "result": map[string]any{"data": []map[string]any{
					{"id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "GPT-6.1-Sol", "description": "Workhorse", "hidden": false, "isDefault": true},
					{"id": "internal", "model": "gpt-internal", "displayName": "Internal", "description": "", "hidden": true},
					{"id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna", "description": "Fast", "hidden": false},
				}, "nextCursor": "page-2"}})
			} else {
				send(map[string]any{"id": m.ID, "result": map[string]any{"data": []map[string]any{
					{"id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna", "hidden": false},
					{"id": "gpt-6-astra", "model": "gpt-6-astra", "displayName": "", "description": "Frontier", "hidden": false},
				}, "nextCursor": nil}})
			}
		case "":
			if string(m.ID) == "43" {
				var reply struct {
					Result json.RawMessage `json:"result"`
				}
				_ = json.Unmarshal(s.Bytes(), &reply)
				send(map[string]any{"method": "item/agentMessage/delta", "params": map[string]any{"threadId": "thread-1", "delta": "permissions=" + string(reply.Result)}})
				send(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1", "status": "completed"}}})
			}
			if string(m.ID) == "42" {
				decision := string(m.Result.Decision)
				_ = json.Unmarshal(m.Result.Decision, &decision)
				send(map[string]any{"method": "item/agentMessage/delta", "params": map[string]any{"threadId": "thread-1", "delta": "decision=" + decision}})
				if os.Getenv("FIXTURE_KEEP_TURN") == "" {
					send(map[string]any{"method": "turn/completed", "params": map[string]any{"threadId": "thread-1", "turn": map[string]string{"id": "turn-1", "status": "completed"}}})
				}
			}
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

	again, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "thread-1", Cursor: approval.Cursor, Responded: map[string]agent.Operation{"n:41": {Kind: agent.Approve, RequestID: "n:41", Decision: "accept"}}}}})
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

	again, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "thread-1", Cursor: approval.Cursor, Responded: map[string]agent.Operation{"n:41": {Kind: agent.Approve, RequestID: "n:41", Decision: "accept"}}}}})
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

// A decline whose reply never reached the app-server is repeated after the
// restart: nothing ran, so there is nothing unknown about it.
func TestRedeliveredDeclinedCallIsDeclinedAgain(t *testing.T) {
	root := fixtureRoot(t)
	self, _ := os.Executable()
	runs := make(chan string, 4)
	tools := func(ctx context.Context, name string, args json.RawMessage) (string, error) {
		runs <- string(args)
		return "ran", nil
	}
	first, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	_, _ = first.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "deploy"})
	approval := receive(t, first, "approval")
	first.(agent.Committer).Commit(approval)
	state := first.(agent.Durable).DurableState()
	// The Runner recorded the decline and died before its reply was written.
	_ = first.(agent.Detacher).Detach()

	declined := map[string]agent.Operation{"n:41": {Kind: agent.Approve, RequestID: "n:41", Decision: "decline"}}
	again, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "thread-1", Cursor: approval.Cursor, Responded: declined}}})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if reply := receive(t, again, "message_delta"); reply.Text != "success=false Command declined" {
		t.Fatalf("redelivered declined call: %q", reply.Text)
	}
	select {
	case args := <-runs:
		t.Fatalf("a declined command ran after the restart: %s", args)
	default:
	}
}

// A native approval the user answered, whose reply never reached the
// app-server, is answered again with the recorded decision, not asked again.
func TestRedeliveredAnsweredApprovalIsAnsweredAgain(t *testing.T) {
	t.Setenv("FIXTURE_NATIVE_APPROVAL", "1")
	root := fixtureRoot(t)
	self, _ := os.Executable()
	first, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	_, _ = first.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "clean"})
	approval := receive(t, first, "approval")
	if approval.RequestID != "n:42" {
		t.Fatalf("native approval: %+v", approval)
	}
	first.(agent.Committer).Commit(approval)
	state := first.(agent.Durable).DurableState()
	_ = first.(agent.Detacher).Detach()

	answered := map[string]agent.Operation{"n:42": {Kind: agent.Approve, RequestID: "n:42", Decision: "decline"}}
	again, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "thread-1", Cursor: approval.Cursor, Responded: answered}}})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	for {
		ev, err := again.Receive(ctx)
		if err != nil {
			t.Fatal(err)
		}
		switch ev.Type {
		case "approval":
			t.Fatal("an answered approval was asked again")
		case "message_delta":
			if ev.Text != "decision=decline" {
				t.Fatalf("app-server got %q", ev.Text)
			}
			return
		}
	}
}

// untilTurnEnds records every event as the engine does (committing each) and
// returns them, up to and including the turn's end.
func untilTurnEnds(t *testing.T, s agent.Session) []agent.Event {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	var events []agent.Event
	for {
		ev, err := s.Receive(ctx)
		if err != nil {
			t.Fatalf("before the turn ended (%d events): %v", len(events), err)
		}
		if c, ok := s.(agent.Committer); ok {
			c.Commit(ev)
		}
		events = append(events, ev)
		if ev.Type == "turn_ended" {
			return events
		}
	}
}

// When the app-server dies, Receive ends: the engine learns the provider is
// gone instead of waiting for events nothing will send.
func TestAppServerDeathEndsReceive(t *testing.T) {
	t.Setenv("FIXTURE_EXIT_IN_TURN", "1")
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "die"}); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	exited := false
	for {
		ev, err := s.Receive(ctx)
		if err != nil {
			if ctx.Err() != nil {
				t.Fatal("Receive never ended after the app-server died")
			}
			break
		}
		if ev.Type == "failure" && ev.Status == "provider_process_exit" {
			exited = true
		}
	}
	if !exited {
		t.Fatal("the app-server's exit was not reported before Receive ended")
	}
}

// One notification whose params are not what this adapter expects (a string
// error, a field of another type) is skipped; it never ends supervision, which
// would kill the app-server. The first unreadable one of a method it uses is
// recorded once; those it does not use are not even read.
func TestNotificationWithUnexpectedFieldsIsSkipped(t *testing.T) {
	t.Setenv("FIXTURE_ODD_NOTIFICATIONS", "1")
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "hello"}); err != nil {
		t.Fatal(err)
	}
	unreadable, text := 0, ""
	for _, ev := range untilTurnEnds(t, s) {
		switch ev.Type {
		case agent.UnreadableEvent:
			unreadable++
			if !strings.Contains(ev.Text, "item/agentMessage/delta") {
				t.Errorf("skipped line not named: %q", ev.Text)
			}
		case "message_delta":
			text += ev.Text
		case "failure":
			t.Errorf("an odd notification failed the turn: %+v", ev)
		}
	}
	if unreadable != 1 || text != "still here" {
		t.Fatalf("unreadable records %d, text %q", unreadable, text)
	}
}

// A request the app-server stopped waiting on without a reply (it resolved it
// itself, or the turn ended) is forgotten by the host: a re-attaching Runner
// is not shown a phantom approval.
func TestResolvedRequestIsNotReplayedAfterReattach(t *testing.T) {
	t.Setenv("FIXTURE_NATIVE_APPROVAL", "1")
	for _, ends := range []string{"resolved", "turn"} {
		t.Run(ends, func(t *testing.T) {
			t.Setenv("FIXTURE_APPROVAL_ENDS", ends)
			root := fixtureRoot(t)
			self, _ := os.Executable()
			first, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: root}})
			if err != nil {
				t.Fatal(err)
			}
			if _, err = first.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "clean"}); err != nil {
				t.Fatal(err)
			}
			events := untilTurnEnds(t, first)
			asked := false
			for _, ev := range events {
				asked = asked || (ev.Type == "approval" && ev.RequestID == "n:42")
			}
			if !asked {
				t.Fatal("the approval was never asked")
			}
			state := first.(agent.Durable).DurableState()
			if err = first.(agent.Detacher).Detach(); err != nil {
				t.Fatal(err)
			}

			again, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "thread-1", Cursor: events[len(events)-1].Cursor}}})
			if err != nil {
				t.Fatal(err)
			}
			defer again.Close()
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			defer cancel()
			for {
				ev, err := again.Receive(ctx)
				if err != nil {
					return // Nothing was redelivered.
				}
				if ev.Type == "approval" {
					t.Fatalf("a request nobody waits on was redelivered: %+v", ev)
				}
			}
		})
	}
}

func TestExplicitYOLOOverridesCarriedSettingsOnFirstTurn(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	binary, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	s, err := (Adapter{Binary: binary}).Open(ctx, agent.Open{
		Cwd: t.TempDir(), Permission: "yolo",
		Env:              append(os.Environ(), "FIXTURE_EXPECT_YOLO_TURN=1"),
		ProviderSettings: json.RawMessage(`{"approvalPolicy":"untrusted","sandboxPolicy":{"type":"workspaceWrite"}}`),
	})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if _, err := s.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "hello"}); err != nil {
		t.Fatal(err)
	}
}
