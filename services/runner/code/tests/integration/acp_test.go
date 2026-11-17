package integration

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/acp"
)

// TestRealACPAgent runs one turn with a real ACP agent installed on this
// computer (LAZYBOX_V2_REAL_ACP=gemini, goose, opencode…; the agent must be
// signed in): it reads a file through Lazybox and answers. Live and billable.
func TestRealACPAgent(t *testing.T) {
	id := os.Getenv("LAZYBOX_V2_REAL_ACP")
	if os.Getenv("LAZYBOX_V2_REAL") != "1" || id == "" {
		t.Skip("requires explicit live-provider opt-in and LAZYBOX_V2_REAL_ACP=<agent id>")
	}
	b, err := runner.New(runner.Config{State: t.TempDir(), ACPLookPath: exec.LookPath})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	a, ok := b.ACP.Get(id)
	if !ok {
		t.Fatalf("no ACP agent %q", id)
	}
	if a.Program(exec.LookPath) == "" {
		t.Skipf("%s is not installed", a.Title)
	}
	e, err := b.OpenEmptyWorkspace(acp.ProfileID(id), accounts.Options{Permission: "yolo"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	secret := "lazybox-acp-" + e.ID[:8]
	if err = os.WriteFile(filepath.Join(e.Cwd, "token.txt"), []byte(secret+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err = e.Engine.Apply(context.Background(), agent.Operation{ID: "real", Kind: agent.Message, Text: "Read the file token.txt in the current directory and reply with its exact contents and nothing else."}); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(4 * time.Minute)
	var reply strings.Builder
	var types []string
	for cursor := uint64(0); time.Now().Before(deadline); {
		ctx, cancel := context.WithTimeout(context.Background(), time.Until(deadline))
		r, err := b.Timeline(ctx, e.ID, cursor)
		cancel()
		if err != nil {
			t.Fatalf("journal: %v; seen %v", err, types)
		}
		cursor = r.Sequence
		var ev agent.Event
		_ = json.Unmarshal(r.Value, &ev)
		types = append(types, ev.Type+":"+ev.Status)
		if ev.Type == "message_delta" {
			reply.WriteString(ev.Text)
		}
		if ev.Type == "failure" {
			t.Logf("failure: %s %s", ev.Status, ev.Text)
		}
		if ev.Type == "turn_ended" {
			break
		}
	}
	t.Logf("%s events: %v", a.Title, types)
	if !strings.Contains(reply.String(), secret) {
		t.Fatalf("%s replied %q, want %q", a.Title, reply.String(), secret)
	}
}
