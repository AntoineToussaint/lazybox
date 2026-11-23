package runner

import (
	"context"
	"errors"
	"fmt"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

func TestMissionBudgetUsesEachGrantAndAttemptWindow(t *testing.T) {
	usage, err := telemetry.Open(filepath.Join(t.TempDir(), "usage.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer usage.Close()
	b := Bench{Telemetry: usage}
	ctx := context.Background()
	at := func(n int64) time.Time { return time.Unix(n, 0).UTC() }
	cost := int64(1e9)
	for _, conversation := range []string{"holder", "child"} {
		for second := int64(1); second <= 10; second++ {
			r := telemetry.Record{ID: fmt.Sprintf("%s-%d", conversation, second), Scope: telemetry.Scope{Conversation: conversation, Execution: conversation, Profile: "codex-default", Provider: "codex"}, Status: "complete", Nanos: &cost, ObservedAt: at(second)}
			if err := usage.Put(ctx, r); err != nil {
				t.Fatal(err)
			}
		}
	}
	first := &work.Grant{GrantPayload: work.GrantPayload{Grant: "first", Holder: "holder"}, Issued: at(2), Revoked: true, RevokedAt: at(5)}
	second := &work.Grant{GrantPayload: work.GrantPayload{Grant: "second", Holder: "holder"}, Issued: at(7)}
	a := &work.Attempt{Conversation: "child", Grant: "first", Assigned: at(3), Ended: true, EndedAt: at(4)}
	task := &work.Task{Attempts: []*work.Attempt{a}}
	state := &work.State{Grants: []*work.Grant{first, second}, Tasks: []*work.Task{task}, Canceled: true, CanceledAt: at(9)}
	if got := b.TreeBudgetOf(ctx, state, first).Spent; got != 4 {
		t.Fatalf("revoked grant: got %v, want 3 from holder + 1 from child", got)
	}
	if got := b.TreeBudgetOf(ctx, state, second).Spent; got != 2 {
		t.Fatalf("second grant on same holder: got %v, want only holder usage before cancellation", got)
	}
	if got := b.AttemptCost(ctx, state, task, a); got != 1 {
		t.Fatalf("assigned task: got %v, want only usage during assignment", got)
	}
}

func TestSpawnRequiresReadableBudget(t *testing.T) {
	b := &Bench{}
	g := &work.Grant{GrantPayload: work.GrantPayload{Holder: "holder", Ceiling: work.Ceiling{BudgetUSD: 1}}}
	if err := b.checkBudget(context.Background(), &work.State{}, g, 0, nil); !errors.Is(err, work.ErrRefused) {
		t.Fatalf("spawn without usage under a budget returned %v", err)
	}
}
