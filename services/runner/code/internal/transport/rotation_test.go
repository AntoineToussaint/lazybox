package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
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
	enableAccountPolicy(t, b)
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

// A new conversation rotates across the provider's other signed-in
// subscriptions unless someone opted it out, chose a pool, or excluded an
// account in Settings.
func TestDefaultRotationPoolCoversSignedInSubscriptionsWithOptOuts(t *testing.T) {
	service, _ := collectionService(t)
	b := service.Bench
	b.Adapters["codex"] = &agenttest.Adapter{}
	root := filepath.Join(b.Config.State, "accounts")
	for _, p := range []accounts.Profile{
		{ID: "work", Provider: "codex", Isolation: "environment", Home: filepath.Join(root, "work", "codex")},
		{ID: "personal", Provider: "codex", Isolation: "environment", Home: filepath.Join(root, "personal", "codex")},
		{ID: "spare", Provider: "codex", Isolation: "environment", Home: filepath.Join(root, "spare", "codex")},
		{ID: "claude-saved", Provider: "claude", Isolation: "environment", Home: filepath.Join(root, "claude-saved", "claude")},
	} {
		if err := accounts.StoreProfile(b.Config.State, p); err != nil {
			t.Fatal(err)
		}
	}
	if err := b.ReloadProfiles(); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	pool := func(options *pb.RunnerOptions, profile string) []string {
		t.Helper()
		r, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: profile, Cwd: t.TempDir(), Options: options})
		if err != nil {
			t.Fatal(err)
		}
		e, err := b.Get(r.Session.Id)
		if err != nil {
			t.Fatal(err)
		}
		return e.Options.RotationProfiles
	}
	// Without an account policy nothing switches by itself, whatever was asked.
	if got := pool(&pb.RunnerOptions{RotationProfileIds: []string{"spare"}}, "codex-default"); len(got) != 0 {
		t.Fatalf("a pool without an account policy: %v", got)
	}
	if got := pool(nil, "codex-default"); len(got) != 0 {
		t.Fatalf("a default pool without an account policy: %v", got)
	}
	enableAccountPolicy(t, b)
	if got := pool(nil, "codex-default"); strings.Join(got, ",") != "personal,spare,work" {
		t.Fatalf("default pool %v: every signed-in codex subscription, sorted, and no other provider", got)
	}
	if got := pool(nil, "work"); strings.Join(got, ",") != "personal,spare" {
		t.Fatalf("default pool %v must not contain the selected subscription", got)
	}
	settings, err := service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	settings.RotationExcludedProfileIds = []string{"spare"}
	if settings, err = service.SaveRunnerSettings(ctx, settings); err != nil || strings.Join(settings.RotationExcludedProfileIds, ",") != "spare" {
		t.Fatalf("exclusion not saved: %v %v", settings, err)
	}
	if got := pool(nil, "codex-default"); strings.Join(got, ",") != "personal,work" {
		t.Fatalf("excluded subscription still in the pool: %v", got)
	}
	if got := pool(&pb.RunnerOptions{NoRotation: true, RotationProfileIds: []string{"work"}}, "codex-default"); len(got) != 0 {
		t.Fatalf("opted-out conversation still rotates: %v", got)
	}
	if got := pool(&pb.RunnerOptions{RotationProfileIds: []string{"spare"}}, "codex-default"); strings.Join(got, ",") != "spare" {
		t.Fatalf("explicit pool lost: %v", got)
	}
	b.Config.Defaults.RotationProfiles = []string{}
	if got := pool(nil, "codex-default"); len(got) != 0 {
		t.Fatalf("a configured empty default must disable rotation: %v", got)
	}
}
