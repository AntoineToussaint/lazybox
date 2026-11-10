package runner

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
)

type admissionFixture struct{ entered, release chan struct{} }

func (a *admissionFixture) Open(ctx context.Context, options agent.Open) (agent.Session, error) {
	close(a.entered)
	select {
	case <-a.release:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	return (&agenttest.Adapter{}).Open(ctx, options)
}

func TestAgentAdmissionReservesPendingStartsAndReleasesConfirmedExit(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), MaxLiveAgents: 1, MaxLiveCommands: 2})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	a := &admissionFixture{make(chan struct{}), make(chan struct{})}
	b.Adapters["codex"] = a
	started := make(chan *Execution, 1)
	failed := make(chan error, 1)
	go func() {
		e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
		started <- e
		failed <- err
	}()
	select {
	case <-a.entered:
	case <-time.After(5 * time.Second):
		t.Fatal("startup did not enter adapter")
	}
	_, second := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	close(a.release)
	if second == nil || !strings.Contains(second.Error(), "capacity") {
		t.Fatalf("pending start did not reserve capacity: %v", second)
	}
	e := <-started
	if err = <-failed; err != nil {
		t.Fatal(err)
	}
	if _, err = b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err == nil {
		t.Fatal("live execution exceeded configured limit")
	}
	if err = e.Engine.Close(); err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	if _, err = b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err != nil {
		t.Fatalf("confirmed exit did not release admission: %v", err)
	}
}

func TestAgentAdmissionFailureReleasesReservationAndZeroIsBounded(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), MaxLiveAgents: 1})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if _, err = b.OpenEmptyWorkspace("missing-profile", accounts.Options{}, "direct"); err == nil {
		t.Fatal("missing profile accepted")
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	if _, err = b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal("failed admission leaked reservation", err)
	}
	defaultBench, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer defaultBench.Close()
	if defaultBench.Config.MaxLiveAgents != 32 || defaultBench.Config.MaxLiveCommands != 128 {
		t.Fatal("zero became unlimited")
	}
	for _, cfg := range []Config{{State: t.TempDir(), MaxLiveAgents: -1}, {State: t.TempDir(), MaxLiveCommands: -1}} {
		if _, err = New(cfg); err == nil {
			t.Fatal("negative limit accepted")
		}
	}
}
