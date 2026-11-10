package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

type rotatingFixture struct{ opened chan *rotatingNative }
type rotatingNative struct {
	*agenttest.Session
	stopped atomic.Bool
}

func (a rotatingFixture) Open(_ context.Context, o agent.Open) (agent.Session, error) {
	s := &rotatingNative{Session: &agenttest.Session{Open: o, Ref: o.Profile + "-native", Events: make(chan agent.Event, 16), Applied: make(chan agent.Operation, 16)}}
	a.opened <- s
	return s, nil
}
func (s *rotatingNative) Close() error { s.stopped.Store(true); return nil }
func TestAutomaticRotationKeepsCollectionCommandsAndSiblingAccount(t *testing.T) {
	service, _ := collectionService(t)
	b := service.Bench
	t.Setenv("ROTATION_MARKER_A", "subscription-a")
	t.Setenv("ROTATION_MARKER_B", "subscription-b")
	root := filepath.Join(b.Config.State, "accounts")
	profiles := []accounts.Profile{{ID: "a", Provider: "codex", Isolation: "environment", Home: filepath.Join(root, "a", "codex"), EnvRefs: map[string]string{"PROFILE_MARKER": "ROTATION_MARKER_A"}}, {ID: "b", Provider: "codex", Isolation: "environment", Home: filepath.Join(root, "b", "codex"), EnvRefs: map[string]string{"PROFILE_MARKER": "ROTATION_MARKER_B"}}}
	for _, p := range profiles {
		if err := accounts.StoreProfile(b.Config.State, p); err != nil {
			t.Fatal(err)
		}
	}
	if err := b.ReloadProfiles(); err != nil {
		t.Fatal(err)
	}
	adapter := rotatingFixture{opened: make(chan *rotatingNative, 4)}
	b.Adapters["codex"] = adapter
	collection := createTestCollection(t, service)
	e, err := b.OpenCollection("a", collection.Id, "", accounts.Options{RotationProfiles: []string{"b"}}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	first := <-adapter.opened
	sibling, err := b.Open("a", t.TempDir(), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	siblingNative := <-adapter.opened
	command, err := b.Start(e, "retained build", []string{"/bin/sh", "-c", `printf '%s\n' "$PROFILE_MARKER"; sleep 1; printf 'build-finished\n'`})
	if err != nil {
		t.Fatal(err)
	}
	first.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	var next *rotatingNative
	select {
	case next = <-adapter.opened:
	case <-time.After(5 * time.Second):
		t.Fatal("no replacement")
	}
	select {
	case <-next.Applied:
	case <-time.After(5 * time.Second):
		t.Fatal("no handoff")
	}
	deadline := time.Now().Add(time.Second * 5)
	for e.Engine.Snapshot().Profile != "b" {
		if time.Now().After(deadline) {
			t.Fatal("profile not updated")
		}
		time.Sleep(time.Millisecond)
	}
	owner, err := b.Collections.Get(collection.Id)
	if err != nil {
		t.Fatal(err)
	}
	if owner.OwnerExecutionID != e.ID || !first.stopped.Load() || siblingNative.stopped.Load() || sibling.Engine.Snapshot().Profile != "a" {
		t.Fatal("collection/sibling ownership changed")
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Second*5)
	defer cancel()
	if _, err = command.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	output, _, err := command.Read(ctx, 0, 4096)
	if err != nil || !strings.Contains(string(output), "subscription-a") || !strings.Contains(string(output), "build-finished") {
		t.Fatalf("retained command changed account: %q %v", output, err)
	}
	newCommand, err := b.Start(e, "next command", []string{"/bin/sh", "-c", `printf '%s' "$PROFILE_MARKER"`})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = newCommand.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	output, _, err = newCommand.Read(ctx, 0, 4096)
	if err != nil || string(output) != "subscription-b" {
		t.Fatalf("new command has old credential context: %q %v", output, err)
	}
	if len(b.Commands.List(e.ID)) != 2 {
		t.Fatal("command replay or loss")
	}
}
