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
)

// "Always allow" outlives the agent's process: a command the person allowed
// always is not asked for again after the agent ends and the conversation is
// resumed in a new worker; once taken back, it is asked for again. Real SDK
// and CLI, deterministic Messages endpoint (claudeMessagesFixture).
func TestClaudeAlwaysAllowOutlivesTheWorker(t *testing.T) {
	argv := []string{"/usr/bin/true", "fixture-grant"}
	worker, url := claudeMessagesFixture(t, func(seen fixtureTurn) fixtureReply {
		if seen.said("Write the title") {
			return fixtureReply{Text: "Fixture title"}
		}
		// Each RUN_n message asks for the same command once, under its own call id.
		for n := 9; n >= 1; n-- {
			id := "fixture-grant-" + string(rune('0'+n))
			if seen.said("RUN_" + string(rune('0'+n))) {
				if seen.Results[id] {
					return fixtureReply{Text: "RAN"}
				}
				return fixtureReply{ToolID: id, ToolName: "mcp__lazybox__command", ToolInput: map[string]any{"action": "start", "argv": argv}}
			}
		}
		return fixtureReply{Text: "FIXTURE-HELLO"}
	})
	t.Setenv("BENCH_FIXTURE_KEY", "fixture-api-key")
	root := t.TempDir()
	b, err := runner.New(runner.Config{State: root, Worker: worker, Profiles: []accounts.Profile{{ID: "fixture", Provider: "claude", Home: filepath.Join(root, "profile"), Isolation: "environment", EnvRefs: map[string]string{"ANTHROPIC_API_KEY": "BENCH_FIXTURE_KEY"}}}, Gateways: []runner.Gateway{{Provider: "claude", Gateway: agent.Gateway{ID: "fixture-meter", URL: url + "/anthropic/execution"}}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	// turn sends one message and answers each approval of its turn with
	// always ("" allows once); it returns how many approvals the turn asked.
	cursors := map[string]uint64{}
	turn := func(e *runner.Execution, text, always string) int {
		t.Helper()
		if _, err := e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: text}); err != nil {
			t.Fatal(err)
		}
		asked := 0
		for {
			r, err := e.Engine.Events(ctx, cursors[e.ID])
			if err != nil {
				t.Fatal(err)
			}
			cursors[e.ID] = r.Sequence
			var ev agent.Event
			_ = json.Unmarshal(r.Value, &ev)
			switch ev.Type {
			case "failure":
				t.Fatalf("real SDK fixture: %s %s", ev.Status, ev.Text)
			case "approval":
				asked++
				if _, err = e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Approve, RequestID: ev.RequestID, Decision: "accept", Always: always}); err != nil {
					t.Fatal(err)
				}
			case "turn_ended":
				return asked
			}
		}
	}
	e, err := b.Open("fixture", t.TempDir(), "", accounts.Options{Model: "claude-fixture", Permission: "ask"}, "fixture-meter", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if asked := turn(e, "RUN_1", "command"); asked != 1 {
		t.Fatalf("the first run asked %d times", asked)
	}
	grants := e.Engine.Snapshot().Grants
	if len(grants) != 1 || grants[0].Label != "`/usr/bin/true fixture-grant`" || grants[0].Tool != "mcp__lazybox__command" {
		t.Fatalf("the conversation keeps the grant: %+v", grants)
	}
	if asked := turn(e, "RUN_2", ""); asked != 0 {
		t.Fatalf("the same worker asked again: %d", asked)
	}
	// The agent's process ends; the conversation resumes in a new worker.
	waitCommands(t, ctx, b, e.ID)
	if err = e.Engine.Close(); err != nil {
		t.Fatal(err)
	}
	next, err := b.Resume(ctx, e.ID, false)
	if err != nil {
		t.Fatal(err)
	}
	if got := next.Engine.Snapshot().Grants; len(got) != 1 || got[0].ID != grants[0].ID {
		t.Fatalf("the resumed conversation lists %+v", got)
	}
	if asked := turn(next, "RUN_3", ""); asked != 0 {
		t.Fatalf("after a restart the agent asked again %d times for a command allowed always", asked)
	}
	// Taken back, it is asked for again.
	if _, err = next.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.RevokeGrant, GrantID: grants[0].ID}); err != nil {
		t.Fatal(err)
	}
	if got := next.Engine.Snapshot().Grants; len(got) != 0 {
		t.Fatalf("still listed after taking it back: %+v", got)
	}
	if asked := turn(next, "RUN_4", ""); asked != 1 {
		t.Fatalf("after taking it back the agent asked %d times", asked)
	}
	t.Log("real SDK and CLI, deterministic gateway: an always-allowed command is not asked for after the worker restarts; taken back, it is")
}

// waitCommands waits for the execution's managed commands to finish.
func waitCommands(t *testing.T, ctx context.Context, b *runner.Bench, id string) {
	t.Helper()
	for _, c := range b.Commands.List(id) {
		command, err := b.Commands.Get(c.ID)
		if err != nil {
			t.Fatal(err)
		}
		if _, err = command.Wait(ctx); err != nil {
			t.Fatal(err)
		}
	}
}
