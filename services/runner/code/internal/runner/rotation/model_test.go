package rotation

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// A model switched during the conversation is the one the next subscription
// starts with, including in the provider settings the handoff carries.
func TestRotationKeepsTheModelSwitchedTo(t *testing.T) {
	s, a, _ := fixture(t, []string{"a", "b"})
	first := <-a.opened
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "switch", Kind: agent.Model, Model: "switched-model", Effort: "high"}); err != nil {
		t.Fatal(err)
	}
	<-first.Applied
	first.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	receive(t, s, "profile_switching")
	second := <-a.opened
	if second.Open.Model != "switched-model" || second.Open.Effort != "high" {
		t.Fatalf("the next subscription starts on %q", second.Open.Model)
	}
	settings := string(second.Open.ProviderSettings)
	if !strings.Contains(settings, `"model":"switched-model"`) || !strings.Contains(settings, `"effort":"high"`) || !strings.Contains(settings, `"approvalPolicy":"never"`) {
		t.Fatalf("carried settings lost the switch or other settings: %s", settings)
	}
}

func TestWithModelLeavesSettingsWithoutAModelAlone(t *testing.T) {
	for _, settings := range []string{"", `{"approvalPolicy":"never"}`, `not json`} {
		if got := string(withModel([]byte(settings), "m", "")); got != settings {
			t.Fatalf("%q became %q", settings, got)
		}
	}
}
