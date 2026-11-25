package accounts

import (
	"errors"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"path/filepath"
	"testing"
	"time"
)

func TestAvailabilityPersistsAndOnlyReprobesAfterReset(t *testing.T) {
	path := filepath.Join(t.TempDir(), "accounts", "availability.json")
	state, err := OpenState(path)
	if err != nil {
		t.Fatal(err)
	}
	reset := time.Now().Add(time.Hour).Unix()
	for id, observation := range map[string]agent.AccountObservation{"a": {State: "exhausted", ResetAt: reset}, "b": {State: "auth_required"}, "c": {State: "context_full"}, "d": {State: "throttled"}} {
		if err = state.Observe(id, observation); err != nil {
			t.Fatal(err)
		}
	}
	state, err = OpenState(path)
	if err != nil {
		t.Fatal(err)
	}
	if state.Eligible("a", time.Now()) || state.Eligible("b", time.Now()) || state.Eligible("d", time.Now()) || !state.Eligible("a", time.Unix(reset, 0)) || !state.Eligible("c", time.Now()) {
		t.Fatal("wrong eligibility")
	}
	if state.Observation("b").State != "auth_required" || state.Observation("missing").State != "unknown" {
		t.Fatal("observation not retained")
	}
}

func TestUsageWindowsMergeWithoutInventingMissingDataAndPersistIndependently(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	state, err := OpenState(path)
	if err != nil {
		t.Fatal(err)
	}
	if err = state.Observe("a", agent.AccountObservation{State: "available", Plan: "plus", Email: "saved@example.invalid", Windows: []agent.UsageWindow{{Name: "primary", UsedPercent: 45}, {Name: "secondary", UsedPercent: 60}}}); err != nil {
		t.Fatal(err)
	}
	if err = state.Observe("a", agent.AccountObservation{State: "available", Windows: []agent.UsageWindow{{Name: "primary", UsedPercent: 85}}}); err != nil {
		t.Fatal(err)
	}
	state, err = OpenState(path)
	if err != nil {
		t.Fatal(err)
	}
	observation := state.Observation("a")
	if len(observation.Windows) != 2 || observation.Windows[0].UsedPercent != 85 || observation.Windows[1].UsedPercent != 60 || observation.Plan != "plus" || observation.Email != "saved@example.invalid" || observation.ObservedAt == 0 {
		t.Fatalf("%+v", observation)
	}
	observation.Windows[0].UsedPercent = 0
	if state.Observation("a").Windows[0].UsedPercent != 85 {
		t.Fatal("mutable quota snapshot")
	}
	if len(state.Observation("b").Windows) != 0 || state.Observation("b").State != "unknown" || state.Observation("b").Email != "" {
		t.Fatal("missing account invented quota")
	}
}

func TestRotationCapacityChecksAllWindows(t *testing.T) {
	state, err := OpenState(filepath.Join(t.TempDir(), "availability.json"))
	if err != nil {
		t.Fatal(err)
	}
	now := time.Now()
	if err = state.Observe("a", agent.AccountObservation{State: "available", Windows: []agent.UsageWindow{
		{Name: "session", UsedPercent: 100, ResetAt: now.Add(-time.Minute).Unix()},
		{Name: "weekly", UsedPercent: 100, ResetAt: now.Add(time.Hour).Unix()},
	}}); err != nil {
		t.Fatal(err)
	}
	if state.Eligible("a", now) {
		t.Fatal("the weekly limit still blocks rotation")
	}
	if got := state.RotationCapacity("a", now.Add(time.Hour)); got != ProbeAllowed {
		t.Fatalf("after both windows reset, capacity = %v, want probe allowed", got)
	}
	if err = state.Observe("a", agent.AccountObservation{State: "available", Windows: []agent.UsageWindow{
		{Name: "weekly", UsedPercent: 100},
	}}); err != nil {
		t.Fatal(err)
	}
	if state.Eligible("a", now.Add(time.Hour)) {
		t.Fatal("a full window without a reset still blocks rotation")
	}
}

func TestClearForgetsALimitButNotSignInOrBilling(t *testing.T) {
	path := filepath.Join(t.TempDir(), "availability.json")
	state, err := OpenState(path)
	if err != nil {
		t.Fatal(err)
	}
	reset := time.Now().Add(time.Hour).Unix()
	for id, observation := range map[string]agent.AccountObservation{
		"limited": {State: "exhausted", ResetAt: reset, Plan: "max", Email: "kept@example.invalid", Windows: []agent.UsageWindow{{Name: "five_hour", UsedPercent: 100, ResetAt: reset}, {Name: "weekly", UsedPercent: 40}}},
		"full":    {State: "available", Windows: []agent.UsageWindow{{Name: "weekly", UsedPercent: 100}}},
		"signin":  {State: "auth_required"},
		"fine":    {State: "available"},
	} {
		if err = state.Observe(id, observation); err != nil {
			t.Fatal(err)
		}
	}
	for _, id := range []string{"limited", "full"} {
		if err = state.Clear(id); err != nil {
			t.Fatalf("%s: %v", id, err)
		}
	}
	for _, id := range []string{"signin", "fine", "missing"} {
		if err = state.Clear(id); !errors.Is(err, ErrNotLimited) {
			t.Fatalf("%s: got %v, want ErrNotLimited", id, err)
		}
	}
	// Durable: a Runner restart keeps the cleared limit cleared.
	state, err = OpenState(path)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Now()
	cleared := state.Observation("limited")
	if cleared.State != "available" || cleared.ResetAt != 0 || cleared.Plan != "max" || cleared.Email != "kept@example.invalid" || len(cleared.Windows) != 1 || cleared.Windows[0].Name != "weekly" || cleared.ObservedAt == 0 {
		t.Fatalf("%+v", cleared)
	}
	if state.RotationCapacity("limited", now) != CapacityReported || state.RotationCapacity("full", now) != CapacityReported {
		t.Fatal("a cleared limit still blocks rotation")
	}
	if state.Observation("signin").State != "auth_required" {
		t.Fatal("a sign-in problem was cleared")
	}
}
