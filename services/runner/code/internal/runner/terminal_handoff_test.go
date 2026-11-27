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
		ID: "fixture-execution", Cwd: c.Root, CollectionID: c.ID, spool: spool,
		Options: accounts.Options{Permission: "yolo"},
		Env:     []string{"PATH=/usr/bin:/bin", "LAZYBOX_V2_ENDPOINT=private", "FIXTURE=present"},
	}
	type response struct {
		body string
		err  error
	}
	start := func(after uint64, agentInput bool) (agent.Event, <-chan response) {
		t.Helper()
		done := make(chan response, 1)
		args, err := json.Marshal(map[string]any{"action": "interactive", "agentInput": agentInput, "argv": []string{
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
	pending, rejected := start(0, false)
	if ids := b.PendingTerminalHandoffs(e); len(ids) != 1 || ids[0] != pending.RequestID {
		t.Fatalf("missing terminal approval in session attention: %v", ids)
	}
	if ids := b.PendingTerminalHandoffs(&Execution{ID: "another"}); len(ids) != 0 {
		t.Fatalf("approval in another conversation: %v", ids)
	}
	if len(b.Terminals.List(c.ID)) != 0 {
		t.Fatal("approval must precede launch, including in full access")
	}
	if _, err := b.DecideTerminalHandoff(pending.RequestID, false, false); err != nil {
		t.Fatal(err)
	}
	if result := <-rejected; result.err != nil || result.body != `{"status":"declined"}` {
		t.Fatalf("declined response: %+v", result)
	}
	if _, err := b.DecideTerminalHandoff(pending.RequestID, true, false); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("declined request could still launch: %v", err)
	}
	pending, accepted := start(2, false)
	terminal, err := b.DecideTerminalHandoff(pending.RequestID, true, false)
	if err != nil {
		t.Fatal(err)
	}
	if retried, err := b.DecideTerminalHandoff(pending.RequestID, true, false); err != nil || retried != terminal {
		t.Fatalf("retry did not return the same terminal: %v", err)
	}
	if ids := b.PendingTerminalHandoffs(e); len(ids) != 0 {
		t.Fatalf("accepted terminal still requests attention: %v", ids)
	}
	id := terminal.Status().ID
	for _, action := range []string{"interactive_read", "interactive_write"} {
		if _, err := b.interactiveTerminalIO(ctx, e, action, id, "unexpected", 0); err == nil {
			t.Fatalf("private terminal permitted %s", action)
		}
	}
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

	// Older clients see the original argv array and approve only a private terminal.
	pending, legacy := start(5, true)
	var argv []string
	if !pending.TerminalAgentInput || json.Unmarshal(pending.Data, &argv) != nil || len(argv) == 0 {
		t.Fatal("incompatible handoff proposal")
	}
	private, err := b.DecideTerminalHandoff(pending.RequestID, true, false)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := b.interactiveTerminalIO(ctx, e, "interactive_write", private.Status().ID, "unexpected", 0); err == nil {
		t.Fatal("old client granted agent input")
	}
	if err := private.SetPersonControls(false); err == nil {
		t.Fatal("private terminal permitted transfer")
	}
	if err := private.Write([]byte("fixture\n")); err != nil {
		t.Fatal(err)
	}
	if result := <-legacy; result.err != nil || result.body != `{"exitCode":23}` {
		t.Fatalf("private fallback: %+v", result)
	}
	pending, shared := start(8, true)
	terminal, err = b.DecideTerminalHandoff(pending.RequestID, true, true)
	if err != nil {
		t.Fatal(err)
	}
	result := <-shared
	var opened struct{ ID string }
	if result.err != nil || json.Unmarshal([]byte(result.body), &opened) != nil || opened.ID != terminal.Status().ID {
		t.Fatalf("shared terminal: %+v", result)
	}
	call := func(execution *Execution, action, input string, offset uint64) (string, error) {
		args, _ := json.Marshal(map[string]any{"action": action, "input": input, "offset": offset, "id": opened.ID})
		return b.toolWithEnv(ctx, execution, agent.CommandTool, args, nil)
	}
	if _, err := call(&Execution{ID: "other", CollectionID: c.ID}, "interactive_write", "wrong\n", 0); err == nil {
		t.Fatal("another execution could type")
	}
	output.Reset()
	offset = 0
	for !strings.Contains(output.String(), "enter code:") {
		result, err := call(e, "interactive_read", "", offset)
		if err != nil {
			t.Fatal(err)
		}
		var page struct {
			Output string
			Offset uint64
		}
		if err := json.Unmarshal([]byte(result), &page); err != nil {
			t.Fatal(err)
		}
		offset = page.Offset
		output.WriteString(page.Output)
	}
	if err := terminal.Write([]byte("wrong")); err == nil {
		t.Fatal("person typed before takeover")
	}
	if err := terminal.SetPersonControls(true); err != nil {
		t.Fatal(err)
	}
	if _, err := call(e, "interactive_write", "wrong", 0); err == nil {
		t.Fatal("agent typed after takeover")
	}
	if err := terminal.SetPersonControls(false); err != nil {
		t.Fatal(err)
	}
	continued := &Execution{ID: "resumed", CollectionID: c.ID}
	continued.Saved.ConversationID = e.ID
	if _, err := call(continued, "interactive_write", "fixture\n", 0); err != nil {
		t.Fatal(err)
	}
	status, err := terminal.Wait(ctx)
	if err != nil || status.ExitCode == nil || *status.ExitCode != 23 {
		t.Fatalf("agent input: %+v, %v", status, err)
	}
}
