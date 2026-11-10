package runner

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"strings"
	"testing"
)

func TestAgentPreferencesAreResolvedOnceAndResumeKeepsContextAndPermissions(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	d := b.Preferences.Snapshot()
	d.Context = "shared-context"
	d.CodexContext = "codex-context"
	d.ClaudeContext = "claude-context"
	d.Permission = "yolo"
	if err = b.Preferences.Save(d); err != nil {
		t.Fatal(err)
	}
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{Instructions: "session-context"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	saved := fake.Session.Open
	if saved.Permission != "yolo" {
		t.Fatal(saved)
	}
	for _, text := range []string{"shared-context", "codex-context", "session-context"} {
		if strings.Count(saved.Context, text) != 1 {
			t.Fatal(saved.Context)
		}
	}
	if strings.Contains(saved.Context, "claude-context") {
		t.Fatal("wrong provider context")
	}
	fake.Session.Events <- agent.Event{Type: "message_delta", Text: "answer", SessionID: "native-resume"}
	waitHistory(t, func() bool { return e.Engine.Snapshot().Reference == "native-resume" })
	if err = e.Engine.Close(); err != nil {
		t.Fatal(err)
	}
	d = b.Preferences.Snapshot()
	d.Context = "changed-context"
	d.Permission = "read-only"
	if err = b.Preferences.Save(d); err != nil {
		t.Fatal(err)
	}
	if _, err = b.Resume(context.Background(), e.ID, false); err != nil {
		t.Fatal(err)
	}
	resumed := fake.Session.Open
	if resumed.Context != saved.Context || resumed.Permission != "yolo" {
		t.Fatal("resume reapplied defaults", resumed)
	}
}
func TestExplicitNativeAndAssistantPermissionsOverrideRunnerYOLODefault(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), Defaults: accounts.Options{Workflow: "plan"}})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	d := b.Preferences.Snapshot()
	d.Permission = "yolo"
	if err = b.Preferences.Save(d); err != nil {
		t.Fatal(err)
	}
	fake := &agenttest.Adapter{}
	b.Adapters["claude"] = fake
	if _, err = b.OpenEmptyWorkspace("claude-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	if fake.Session.Open.Permission != "read-only" {
		t.Fatal("plan default became YOLO")
	}
	if _, err = b.OpenEmptyWorkspace("claude-default", accounts.Options{Permission: "inherit", Workflow: "execute"}, "direct"); err != nil {
		t.Fatal(err)
	}
	if fake.Session.Open.Permission != "inherit" {
		t.Fatal("explicit native settings ignored")
	}
	if _, err = b.OpenAssistant("claude-default", "help", "help-context", accounts.Options{Permission: "yolo"}, "direct"); err != nil {
		t.Fatal(err)
	}
	if fake.Session.Open.Permission != "read-only" {
		t.Fatal("helper permissions relaxed")
	}
}
