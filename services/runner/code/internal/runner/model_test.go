package runner

import (
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
)

// journaled waits for a journal record of e containing every fragment.
func journaled(t *testing.T, e *Execution, fragments ...string) {
	t.Helper()
	waitHistory(t, func() bool {
		for cursor := uint64(0); cursor < e.Retained(); cursor++ {
			record, err := e.Engine.Events(context.Background(), cursor)
			if err != nil {
				continue
			}
			all := true
			for _, f := range fragments {
				all = all && strings.Contains(string(record.Value), f)
			}
			if all {
				return true
			}
		}
		return false
	})
}

func TestPermissionChangeIsSaidInTheConversation(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{LivePermission: true}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{Permission: "ask"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.SetPermission(context.Background(), e, "permission-1", "read-only"); err != nil {
		t.Fatal(err)
	}
	journaled(t, e, `"permission_changed"`, `"read-only"`, `"live"`)
}

func TestModelSwitchesWhileRunningAndIsRemembered(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{LiveModel: true}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{Model: "first-model"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.SetModelAndEffort(context.Background(), e, "model-1", "second-model", ""); err != nil {
		t.Fatal(err)
	}
	if got := applied(t, fake.Session); got.Kind != agent.Model || got.Model != "second-model" || got.Text != "" {
		t.Fatalf("provider not told, or told as a message: %+v", got)
	}
	saved, err := b.History.Get(context.Background(), e.ID)
	if err != nil || saved.Options.Model != "second-model" || e.Options.Model != "second-model" {
		t.Fatalf("new model not remembered for resume: %v %q %q", err, saved.Options.Model, e.Options.Model)
	}
	journaled(t, e, `"model_changed"`, `"second-model"`, `"live"`)
	// During a turn the change waits for it to end, and the conversation says so.
	fake.Session.Events <- agent.Event{Type: "turn_started", TurnID: "t"}
	waitHistory(t, func() bool { return e.Engine.Snapshot().Busy })
	if _, err = b.SetModelAndEffort(context.Background(), e, "model-2", "third-model", ""); err != nil {
		t.Fatalf("a change during a turn must be taken for the next one: %v", err)
	}
	if got := applied(t, fake.Session); got.Kind != agent.Model || got.Model != "third-model" {
		t.Fatalf("provider not told: %+v", got)
	}
	journaled(t, e, `"model_changed"`, `"third-model"`, `"next_turn"`)
	for _, bad := range []string{"", "two words", "tab\tmodel", strings.Repeat("m", 257)} {
		if _, err = b.SetModelAndEffort(context.Background(), e, "model-bad", bad, ""); !errors.Is(err, agent.ErrInvalidModel) {
			t.Fatalf("%q must be refused as an invalid model: %v", bad, err)
		}
	}
}

func TestAgentWithoutLiveModelRestartsWithTheNewModelAndContinues(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{Model: "first-model"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.SetModelAndEffort(context.Background(), e, "live", "second-model", ""); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("a worker without live model switches must refuse a live switch: %v", err)
	}
	fake.Session.Events <- agent.Event{Type: "turn_started", TurnID: "t"}
	waitHistory(t, func() bool { return e.Engine.Snapshot().Busy })
	if _, err = b.RestartWithModelAndEffort(context.Background(), e, "second-model", ""); err == nil || !strings.Contains(err.Error(), "finish its turn") {
		t.Fatalf("a busy agent must not be restarted: %v", err)
	}
	fake.Session.Events <- agent.Event{Type: "turn_ended", TurnID: "t", Status: "completed", SessionID: "native-reference"}
	waitHistory(t, func() bool { return !e.Engine.Snapshot().Busy })
	if _, err = b.RestartWithModelAndEffort(context.Background(), e, "two words", ""); !errors.Is(err, agent.ErrInvalidModel) {
		t.Fatalf("an invalid model must be refused before the agent stops: %v", err)
	}
	if e.Engine.Snapshot().Closed {
		t.Fatal("a refused restart stopped the agent")
	}
	next, err := b.RestartWithModelAndEffort(context.Background(), e, "second-model", "")
	if err != nil {
		t.Fatal(err)
	}
	if next.ID == e.ID || next.Previous != e.ID {
		t.Fatalf("restart must continue the conversation as a successor: %q after %q", next.ID, e.ID)
	}
	if got := fake.Session.Open; got.Model != "second-model" || got.Resume != "native-reference" {
		t.Fatalf("new agent must start with the new model on the same provider conversation: %+v", got)
	}
	saved, err := b.History.Get(context.Background(), next.ID)
	if err != nil || saved.Options.Model != "second-model" {
		t.Fatalf("model not kept: %v %q", err, saved.Options.Model)
	}
	journaled(t, next, `"model_changed"`, `"second-model"`, `"restart"`)
}
