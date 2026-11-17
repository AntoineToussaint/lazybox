package integration

import (
	"context"
	"encoding/json"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
)

// A message steered into a running Claude turn reaches the model inside that
// turn: the real Claude Code CLI folds it in after the tool result the turn
// was waiting for, and the turn ends once, having read it. Real SDK and CLI,
// deterministic Messages endpoint (claudeMessagesFixture); no inference.
func TestClaudeSDKSteersARunningTurn(t *testing.T) {
	const command, steered = "fixture-steer-command", "STEERED: use port 4000"
	midTurn := make(chan bool, 4)
	worker, url := claudeMessagesFixture(t, func(seen fixtureTurn) fixtureReply {
		if !seen.said("BENCH_FIXTURE_STEER") {
			return fixtureReply{Text: "FIXTURE-HELLO"}
		}
		if !seen.Results[command] {
			return fixtureReply{ToolID: command, ToolName: "mcp__lazybox__command", ToolInput: map[string]any{"action": "start", "argv": []string{"/bin/sh", "-c", "printf 'started\n'"}}}
		}
		if seen.said(steered) {
			// Asked with the tool's result and the steered words together.
			midTurn <- true
			return fixtureReply{Text: "STEER-SEEN"}
		}
		return fixtureReply{Text: "STEER-MISSED"}
	})
	t.Setenv("BENCH_FIXTURE_KEY", "fixture-api-key")
	root := t.TempDir()
	b, err := runner.New(runner.Config{State: root, Worker: worker, Profiles: []accounts.Profile{{ID: "fixture", Provider: "claude", Home: filepath.Join(root, "profile"), Isolation: "environment", EnvRefs: map[string]string{"ANTHROPIC_API_KEY": "BENCH_FIXTURE_KEY"}}}, Gateways: []runner.Gateway{{Provider: "claude", Gateway: agent.Gateway{ID: "fixture-meter", URL: url + "/anthropic/execution"}}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	e, err := b.Open("fixture", t.TempDir(), "", accounts.Options{Model: "claude-fixture", Permission: "ask"}, "fixture-meter", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if !e.Engine.Snapshot().Capabilities.Steer {
		t.Fatal("the Claude worker does not offer steering")
	}
	live := e.Engine.(*engine.Engine)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if _, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: "BENCH_FIXTURE_STEER"}); err != nil {
		t.Fatal(err)
	}
	cursor, ended, reply := uint64(0), 0, ""
	for ended == 0 {
		r, err := e.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		cursor = r.Sequence
		var ev agent.Event
		_ = json.Unmarshal(r.Value, &ev)
		switch ev.Type {
		case "failure":
			t.Fatalf("real SDK fixture: %s %s", ev.Status, ev.Text)
		case "approval":
			// The turn waits on the person: a message sent now is queued,
			// then steered into this turn, before the command is allowed.
			id := commands.ID()
			if _, err = e.Engine.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: steered}); err != nil {
				t.Fatal(err)
			}
			if queued := e.Engine.Snapshot().QueuedMessages; len(queued) != 1 || queued[0].OperationID != id {
				t.Fatalf("not queued behind the running turn: %+v", queued)
			}
			if r, err := live.DeliverQueued(ctx, id, engine.DeliverSteer); err != nil || r.Delivery != "submitted" {
				t.Fatalf("steer: %+v %v", r, err)
			}
			if _, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Approve, RequestID: ev.RequestID, Decision: "accept"}); err != nil {
				t.Fatal(err)
			}
		case "message_delta":
			reply += ev.Text
		case "turn_ended":
			ended++
			if ev.Status != "completed" {
				t.Fatalf("turn ended %s", ev.Status)
			}
		}
	}
	select {
	case <-midTurn:
	default:
		t.Fatalf("the steered message never reached the model with the tool result; the turn replied %q", reply)
	}
	if reply != "STEER-SEEN" {
		t.Fatalf("the turn replied %q", reply)
	}
	// One turn: the steer started none of its own.
	time.Sleep(time.Second)
	if s := e.Engine.Snapshot(); s.Busy || len(s.QueuedMessages) != 0 {
		t.Fatalf("after the steered turn: busy %v, queued %+v", s.Busy, s.QueuedMessages)
	}
	t.Log("real SDK and CLI, deterministic gateway: a steered message is folded into the running turn after its tool result; one turn")
}
