package claude

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

// The worker asks the Runner for a managed-command action, then reports
// whether the call was answered or rejected.
func toolWorker(arguments string) string {
	return `read -r line
printf '%s\n' '{"id":"lazybox-1","result":{"reference":"fixture-ref"}}'
printf '%s\n' '{"id":"tool-1","method":"tool","params":{"name":"lazybox_command","arguments":` + arguments + `}}'
while IFS= read -r line; do
  case "$line" in
    *'"id":"tool-1"'*'"error"'*) printf '%s\n' '{"method":"event","params":{"type":"message_delta","text":"rejected"}}' ;;
    *'"id":"tool-1"'*) printf '%s\n' '{"method":"event","params":{"type":"message_delta","text":"ran"}}' ;;
  esac
done
`
}

func TestRedeliveredToolCallIsRejectedNotRerun(t *testing.T) {
	runs, again := redeliver(t, `{"action":"start","argv":["make","deploy"]}`, func(context.Context) (string, error) { return "", nil })
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	ev, err := again.Receive(ctx)
	if err != nil || ev.Type != "request_resolved" || ev.Status != "outcome_unknown_after_restart" || ev.Cursor == 0 {
		t.Fatalf("unknown outcome not recorded: %+v %v", ev, err)
	}
	ev, err = again.Receive(ctx)
	if err != nil || ev.Type != "message_delta" || ev.Text != "rejected" {
		t.Fatalf("redelivered tool call: %+v %v", ev, err)
	}
	if again.Reference() != "fixture-ref" {
		t.Fatal("reference lost")
	}
	select {
	case args := <-runs:
		t.Fatalf("managed command re-run after the restart: %s", args)
	default:
	}
}

// Waiting for a command has no effect, and the command outlived the Runner:
// the redelivered wait simply runs again and the agent gets its answer.
func TestRedeliveredWaitRunsAgain(t *testing.T) {
	runs, again := redeliver(t, `{"action":"wait","id":"c1"}`, func(context.Context) (string, error) { return `{"done":true}`, nil })
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	ev, err := again.Receive(ctx)
	if err != nil || ev.Type != "message_delta" || ev.Text != "ran" {
		t.Fatalf("redelivered wait: %+v %v", ev, err)
	}
	select {
	case <-runs:
	default:
		t.Fatal("the redelivered wait was not run again")
	}
}

// redeliver opens a hosted session whose worker asks for one tool call, leaves
// the first run in flight, detaches and re-attaches. Later runs use after.
func redeliver(t *testing.T, arguments string, after func(context.Context) (string, error)) (chan json.RawMessage, agent.Session) {
	t.Helper()
	root, err := os.MkdirTemp("", "lbx-claude-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		ids, _ := sessionhost.Hosts(root)
		for _, id := range ids {
			_ = sessionhost.Stop(context.Background(), root, id)
		}
		_ = os.RemoveAll(root)
	})
	worker := filepath.Join(t.TempDir(), "worker.sh")
	if err = os.WriteFile(worker, []byte(toolWorker(arguments)), 0700); err != nil {
		t.Fatal(err)
	}
	runs := make(chan json.RawMessage, 4)
	first := true
	tools := func(ctx context.Context, _ string, args json.RawMessage) (string, error) {
		if !first {
			runs <- args
			return after(ctx)
		}
		first = false
		runs <- args
		<-ctx.Done() // Still running when the Runner goes away.
		return "", ctx.Err()
	}
	a := Adapter{Node: "/bin/sh", Worker: worker}
	session, err := a.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root}})
	if err != nil {
		t.Fatal(err)
	}
	go func() { _, _ = session.Receive(context.Background()) }()
	select {
	case <-runs:
	case <-time.After(8 * time.Second):
		t.Fatal("tool call never reached the Runner")
	}
	state := session.(agent.Durable).DurableState()
	if err = session.(agent.Detacher).Detach(); err != nil {
		t.Fatal(err)
	}
	again, err := a.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Tools: tools, Host: &agent.Hosting{Root: root, Attach: &agent.Attachment{State: state, Reference: "fixture-ref"}}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = again.Close() })
	return runs, again
}
