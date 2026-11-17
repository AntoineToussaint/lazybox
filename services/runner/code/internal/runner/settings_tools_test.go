package runner

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/assistant"
)

// Only the help assistant can reach Lazybox's settings, and through nothing
// else: a coding agent keeps managed commands and never gets the settings
// tools, and the other assistants get no Runner tools at all.
func TestOnlyTheHelpAssistantGetsTheSettingsTools(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["claude"] = fake
	ctx := context.Background()

	if _, err = b.OpenAssistant("claude-default", "help", "help-context", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	help := fake.Session.Open
	if help.Tools == nil || help.Permission != "read-only" || !strings.Contains(help.Context, assistant.Settings) {
		t.Fatalf("help assistant: tools=%v permission=%s", help.Tools != nil, help.Permission)
	}
	read, err := help.Tools(ctx, agent.SettingsReadTool, json.RawMessage(`{}`))
	if err != nil {
		t.Fatal(err)
	}
	var current struct{ Revision uint64 }
	if err = json.Unmarshal([]byte(read), &current); err != nil || current.Revision == 0 {
		t.Fatalf("read: %s %v", read, err)
	}
	if _, err = help.Tools(ctx, agent.SettingsUpdateTool, json.RawMessage(`{"revision":`+jsonNumber(current.Revision)+`,"reason":"The user asked for approval prompts.","changes":{"permission":"ask"}}`)); err != nil {
		t.Fatal(err)
	}
	if b.Preferences.Snapshot().Permission != "ask" {
		t.Fatal("approved update not saved")
	}
	if _, err = help.Tools(ctx, agent.CommandTool, json.RawMessage(`{"action":"start","argv":["true"]}`)); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("help assistant reached managed commands: %v", err)
	}

	for _, purpose := range []string{"planning", "summary"} {
		if _, err = b.OpenAssistant("claude-default", purpose, "context", accounts.Options{}, "direct"); err != nil {
			t.Fatal(err)
		}
		if fake.Session.Open.Tools != nil || strings.Contains(fake.Session.Open.Context, assistant.Settings) {
			t.Fatalf("%s assistant got the settings tools", purpose)
		}
	}

	if _, err = b.OpenEmptyWorkspace("claude-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	coding := fake.Session.Open
	if coding.Tools == nil || strings.Contains(coding.Context, assistant.Settings) {
		t.Fatal("coding agent tools")
	}
	before := b.Preferences.Snapshot()
	for _, name := range []string{agent.SettingsReadTool, agent.SettingsUpdateTool} {
		if _, err = coding.Tools(ctx, name, json.RawMessage(`{"revision":`+jsonNumber(before.Revision)+`,"reason":"r","changes":{"permission":"yolo"}}`)); !errors.Is(err, agent.ErrUnsupported) {
			t.Fatalf("coding agent called %s: %v", name, err)
		}
	}
	if after := b.Preferences.Snapshot(); after.Revision != before.Revision || after.Permission != "ask" {
		t.Fatalf("coding agent changed settings: %+v", after)
	}
}

func jsonNumber(n uint64) string { data, _ := json.Marshal(n); return string(data) }
