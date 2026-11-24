package runner

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
)

func applied(t *testing.T, s *agenttest.Session) agent.Operation {
	t.Helper()
	select {
	case op := <-s.Applied:
		return op
	case <-time.After(5 * time.Second):
		t.Fatal("operation never reached the provider")
	}
	return agent.Operation{}
}

func TestPermissionChangesWhileRunningAndIsRemembered(t *testing.T) {
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
	if _, err = b.SetPermission(context.Background(), e, "permission-1", "yolo", ""); err != nil {
		t.Fatal(err)
	}
	if got := applied(t, fake.Session); got.Kind != agent.Permission || got.Permission != "yolo" {
		t.Fatalf("provider not told: %+v", got)
	}
	saved, err := b.History.Get(context.Background(), e.ID)
	if err != nil || saved.Options.Permission != "yolo" || e.Options.Permission != "yolo" {
		t.Fatalf("new mode not remembered for resume: %v %q %q", err, saved.Options.Permission, e.Options.Permission)
	}
	if _, err = b.SetPermission(context.Background(), e, "permission-2", "inherit", ""); err == nil {
		t.Fatal("inherit is only a starting choice")
	}
}

func TestPermissionChangeRefusedWhereItCannotWork(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.SetPermission(context.Background(), e, "permission-1", "yolo", ""); err == nil {
		t.Fatal("a provider without live permission changes must refuse")
	}
	plan := &agenttest.Adapter{LivePermission: true}
	b.Adapters["codex"] = plan
	p, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{Workflow: "plan"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.SetPermission(context.Background(), p, "permission-2", "yolo", ""); err == nil || !strings.Contains(err.Error(), "plan") {
		t.Fatalf("YOLO must stay incompatible with plan mode: %v", err)
	}
}

func TestAgentWithoutLivePermissionRestartsInTheNewModeAndContinues(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{Permission: "ask"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.SetPermission(context.Background(), e, "live", "yolo", ""); err == nil {
		t.Fatal("a worker without live permission changes must refuse a live change")
	}
	fake.Session.Events <- agent.Event{Type: "turn_started", TurnID: "t"}
	waitHistory(t, func() bool { return e.Engine.Snapshot().Busy })
	if _, err = b.RestartWithPermission(context.Background(), e, "yolo"); err == nil || !strings.Contains(err.Error(), "finish its turn") {
		t.Fatalf("a busy agent must not be restarted: %v", err)
	}
	fake.Session.Events <- agent.Event{Type: "turn_ended", TurnID: "t", Status: "completed", SessionID: "native-reference"}
	waitHistory(t, func() bool { return !e.Engine.Snapshot().Busy })
	next, err := b.RestartWithPermission(context.Background(), e, "yolo")
	if err != nil {
		t.Fatal(err)
	}
	if next.ID == e.ID || next.Previous != e.ID {
		t.Fatalf("restart must continue the conversation as a successor: %q after %q", next.ID, e.ID)
	}
	if got := fake.Session.Open; got.Permission != "yolo" || got.Resume != "native-reference" {
		t.Fatalf("new agent must start in the new mode on the same provider conversation: %+v", got)
	}
	saved, err := b.History.Get(context.Background(), next.ID)
	if err != nil || saved.Options.Permission != "yolo" {
		t.Fatalf("mode not kept: %v %q", err, saved.Options.Permission)
	}
}

func TestFullAccessLeavesPlanForPendingClaudePlanExit(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{LivePermission: true}
	b.Adapters["claude"] = fake
	e, err := b.OpenEmptyWorkspace("claude-default", accounts.Options{Workflow: "plan"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	for _, request := range []agent.Event{
		{Type: "approval", RequestID: "edit", Text: "Edit"},
		{Type: "approval", RequestID: "exit", Text: "ExitPlanMode"},
	} {
		fake.Session.Events <- request
	}
	waitHistory(t, func() bool { return len(e.Engine.Snapshot().Pending) == 2 })
	ids := map[string]string{}
	for id, request := range e.Engine.Snapshot().Pending {
		ids[request.Text] = id
	}
	for _, id := range []string{"", "missing", ids["Edit"]} {
		if _, err := b.SetPermission(context.Background(), e, "refused", "yolo", id); err == nil {
			t.Fatalf("unexpected plan exit for %q", id)
		}
	}
	if _, err := b.SetPermission(context.Background(), e, "allow", "yolo", ids["ExitPlanMode"]); err != nil {
		t.Fatalf("%v: provider=%s workflow=%s pending=%+v", err, e.Profile.Provider, e.Options.Workflow, e.Engine.Snapshot().Pending)
	}
	saved, err := b.History.Get(context.Background(), e.ID)
	if err != nil {
		t.Fatal(err)
	}
	for _, options := range []accounts.Options{saved.Options, e.Options, e.Saved.Options} {
		if options.Workflow != "execute" || options.Permission != "yolo" {
			t.Fatalf("unexpected plan exit options: %+v", options)
		}
	}
}
