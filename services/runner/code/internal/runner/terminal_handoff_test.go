package runner

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

func TestInteractiveCommandHandsPTYToOwner(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), WorktreeRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	c, err := b.Collections.Create(ctx, "interactive", collections.Spec{Name: "Interactive"})
	if err != nil {
		t.Fatal(err)
	}
	spool, err := journal.Open(filepath.Join(t.TempDir(), "events.jsonl"), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	defer spool.Close()
	e := &Execution{
		Cwd: c.Root, CollectionID: c.ID, spool: spool,
		Options: accounts.Options{Permission: "yolo"},
		Env:     []string{"PATH=/usr/bin:/bin", "LAZYBOX_V2_ENDPOINT=private", "FIXTURE=present"},
	}
	type response struct {
		body string
		err  error
	}
	start := func(after uint64) (agent.Event, <-chan response) {
		t.Helper()
		done := make(chan response, 1)
		args, err := json.Marshal(map[string]any{"action": "interactive", "argv": []string{
			"/bin/sh", "-c", `test -t 0 && test -t 1 && test -z "${LAZYBOX_V2_ENDPOINT+x}" && test "$FIXTURE" = present || exit 1
printf 'enter code: '
IFS= read -r line
test "$line" = fixture && test "$1" = 'literal $(false)' || exit 2
exit 23`, "fixture", "literal $(false)",
		}})
		if err != nil {
			t.Fatal(err)
		}
		go func() {
			body, err := b.toolWithEnv(ctx, e, agent.CommandTool, args, nil)
			done <- response{body, err}
		}()
		record, err := spool.Next(ctx, after)
		if err != nil {
			t.Fatal(err)
		}
		var ev agent.Event
		if err := json.Unmarshal(record.Value, &ev); err != nil || ev.Type != "terminal_handoff" || ev.Status != "pending" {
			t.Fatalf("expected handoff request, got %s (%v)", record.Value, err)
		}
		return ev, done
	}
	pending, rejected := start(0)
	if len(b.Terminals.List(c.ID)) != 0 {
		t.Fatal("approval must precede launch, including in full access")
	}
	if _, err := b.DecideTerminalHandoff(pending.RequestID, false); err != nil {
		t.Fatal(err)
	}
	if result := <-rejected; result.err != nil || result.body != `{"status":"declined"}` {
		t.Fatalf("declined response: %+v", result)
	}
	if _, err := b.DecideTerminalHandoff(pending.RequestID, true); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("declined request could still launch: %v", err)
	}
	pending, accepted := start(2)
	terminal, err := b.DecideTerminalHandoff(pending.RequestID, true)
	if err != nil {
		t.Fatal(err)
	}
	if retried, err := b.DecideTerminalHandoff(pending.RequestID, true); err != nil || retried != terminal {
		t.Fatalf("retry did not return the same terminal: %v", err)
	}
	id := terminal.Status().ID
	if _, err := os.Stat(b.TerminalEndpoint(id)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("handoff got an endpoint: %v", err)
	}
	if _, _, err := b.TerminalScope(b.terminalToken(id)); err == nil {
		t.Fatal("handoff credential was accepted")
	}
	select {
	case result := <-accepted:
		t.Fatalf("tool returned before the user entered input: %+v", result)
	default:
	}
	var output strings.Builder
	var offset uint64
	for !strings.Contains(output.String(), "enter code:") {
		data, from, _, err := terminal.Next(ctx, offset)
		if err != nil {
			t.Fatalf("expected interactive prompt, got %q (%v)", output.String(), err)
		}
		output.Write(data)
		offset = from + uint64(len(data))
	}
	if err := terminal.Write([]byte("fixture\n")); err != nil {
		t.Fatal(err)
	}
	if result := <-accepted; result.err != nil || result.body != `{"exitCode":23}` {
		t.Fatalf("expected only the exit status, got %+v", result)
	}
}
