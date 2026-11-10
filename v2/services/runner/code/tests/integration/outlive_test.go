package integration

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

// TestRealAgentOutlivesRunnerRestart is the live check of the session-host
// requirement: a Runner restart is a no-op for a real agent. It detaches the
// Runner while a metered reply is streaming, starts a new Runner, and requires
// that the same native process finish that turn and keep its conversation in
// memory (no native resume), with provider traffic re-pointed at the new
// Runner's gateway. Live and billable: make test-real-outlive.
func TestRealAgentOutlivesRunnerRestart(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_REAL") != "1" {
		t.Skip("real harness runs require LAZYBOX_V2_REAL=1; see runtime evidence")
	}
	for _, provider := range []string{"codex", "claude"} {
		t.Run(provider, func(t *testing.T) {
			if selected := os.Getenv("LAZYBOX_V2_REAL_PROVIDER"); selected != "" && selected != provider {
				t.Skip("different provider selected")
			}
			version, err := exec.Command(provider, "--version").Output()
			if err != nil {
				t.Fatal(err)
			}
			t.Logf("harness version: %s", strings.TrimSpace(string(version)))
			root := t.TempDir()
			cfg := runner.Config{State: filepath.Join(root, "state"), Worker: os.Getenv("LAZYBOX_V2_WORKER")}
			hosts := filepath.Join(cfg.State, "hosts")
			t.Cleanup(func() {
				ids, _ := sessionhost.Hosts(hosts)
				for _, id := range ids {
					_ = sessionhost.Stop(context.Background(), hosts, id)
				}
			})
			ctx, cancel := context.WithTimeout(context.Background(), 6*time.Minute)
			defer cancel()

			first, err := runner.New(cfg)
			if err != nil {
				t.Fatal(err)
			}
			e, err := first.Open(provider+"-default", t.TempDir(), "", accounts.Options{}, "metered", "", "")
			if err != nil {
				_ = first.Close()
				t.Fatal(err)
			}
			id := e.ID
			reply := turn(ctx, t, e, "Remember the code word HELIOTROPE for later. Do not use tools or delegate. Reply only with OK.", 0)
			if !strings.Contains(strings.ToUpper(reply.text), "OK") {
				_ = first.Close()
				t.Fatalf("unexpected first reply: %q", reply.text)
			}
			pid := agentPID(ctx, t, hosts)

			// Start a long reply and leave while it streams.
			second := commands.ID()
			if _, err = e.Engine.Apply(ctx, agent.Operation{ID: second, Kind: agent.Message, Text: "Do not use tools or delegate. Write the numbers from 1 to 120 separated by single spaces, then a new line with only the code word I asked you to remember."}); err != nil {
				t.Fatal(err)
			}
			streaming := waitFor(ctx, t, e, reply.cursor, "message_delta")
			if err = first.Detach(); err != nil {
				t.Fatalf("detach: %v", err)
			}
			if syscall.Kill(pid, 0) != nil {
				t.Fatal("the agent died with its Runner")
			}
			t.Logf("Runner detached mid-reply (journal cursor %d); agent %d still running", streaming, pid)

			next, err := runner.New(cfg)
			if err != nil {
				t.Fatal(err)
			}
			defer next.Close()
			if err = next.Adopt(ctx); err != nil {
				t.Fatalf("adopt: %v", err)
			}
			adopted, err := next.Get(id)
			if err != nil {
				t.Fatalf("execution not adopted: %v", err)
			}
			if got := agentPID(ctx, t, hosts); got != pid {
				t.Fatalf("agent process changed across the restart: %d -> %d", pid, got)
			}
			finished := turn(ctx, t, adopted, "", reply.cursor)
			if !strings.Contains(finished.text, "120") || !strings.Contains(strings.ToUpper(finished.text), "HELIOTROPE") {
				t.Fatalf("the interrupted reply did not finish with the conversation intact: %q", finished.text)
			}
			if !finished.reattached {
				t.Fatal("no runner_reattached record in the journal")
			}
			memory := turn(ctx, t, adopted, "Without tools or delegation, reply only with the code word.", finished.cursor)
			if !strings.Contains(strings.ToUpper(memory.text), "HELIOTROPE") {
				t.Fatalf("native conversation memory lost after adoption: %q", memory.text)
			}
			if got := agentPID(ctx, t, hosts); got != pid {
				t.Fatalf("a later turn ran in another process: %d -> %d", pid, got)
			}
			t.Logf("PASS: %s reply interrupted by a Runner detach finished in the same process (%d) after adoption, and the next turn remembered the conversation", provider, pid)
		})
	}
}

type reply struct {
	text       string
	cursor     uint64
	reattached bool
}

// turn sends text (unless empty) and collects the root reply from cursor until
// the turn ends. Any failure, approval or process exit fails the test.
func turn(ctx context.Context, t *testing.T, e *runner.Execution, text string, cursor uint64) reply {
	t.Helper()
	if text != "" {
		if _, err := e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: text}); err != nil {
			t.Fatal(err)
		}
	}
	var out reply
	for {
		r, err := e.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		cursor = r.Sequence
		var ev agent.Event
		if err = json.Unmarshal(r.Value, &ev); err != nil {
			t.Fatal(err)
		}
		root := ev.AgentID == "" || ev.AgentID == "root"
		switch ev.Type {
		case "message_delta":
			if root {
				out.text += ev.Text
			}
		case "runner_reattached":
			out.reattached = true
		case "approval", "question":
			t.Fatalf("unexpected %s; no permission was granted", ev.Type)
		case "failure", "process_exit":
			t.Fatalf("%s: %s %s", ev.Type, ev.Status, ev.Text)
		case "turn_ended":
			if !root {
				continue
			}
			if ev.Status != "completed" && ev.Status != "" && ev.Status != "success" {
				t.Fatalf("turn ended %q", ev.Status)
			}
			out.cursor = cursor
			return out
		}
	}
}

func waitFor(ctx context.Context, t *testing.T, e *runner.Execution, cursor uint64, kind string) uint64 {
	t.Helper()
	for {
		r, err := e.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		cursor = r.Sequence
		var ev agent.Event
		_ = json.Unmarshal(r.Value, &ev)
		if ev.Type == kind {
			return cursor
		}
		if ev.Type == "turn_ended" {
			t.Fatalf("turn ended before any %s", kind)
		}
	}
}

// agentPID is the guarded agent process of the only live session host.
func agentPID(ctx context.Context, t *testing.T, hosts string) int {
	t.Helper()
	ids, err := sessionhost.Hosts(hosts)
	if err != nil || len(ids) != 1 {
		t.Fatalf("expected one session host, got %v %v", ids, err)
	}
	status, err := sessionhost.Probe(ctx, hosts, ids[0])
	if err != nil || status.AgentPID == 0 {
		t.Fatalf("probe: %+v %v", status, err)
	}
	return status.AgentPID
}
