package accounts

import (
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
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
