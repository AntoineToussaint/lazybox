package codex

import (
	"encoding/json"
	"testing"
)

func TestNativeErrorDiscriminatorsDoNotRotateContextOrSessionBudget(t *testing.T) {
	for _, tc := range []struct{ raw, want string }{
		{`{"codexErrorInfo":"usageLimitExceeded"}`, "exhausted"},
		{`{"codexErrorInfo":"sessionBudgetExceeded"}`, "budget_exceeded"},
		{`{"codexErrorInfo":"contextWindowExceeded"}`, "context_full"},
		{`{"codexErrorInfo":{"unauthorized":{}}}`, "auth_required"},
		{`{"codexErrorInfo":"futureError"}`, "unknown"},
	} {
		if got := errorObservation(json.RawMessage(tc.raw)); got.State != tc.want {
			t.Fatalf("%s: %+v", tc.raw, got)
		}
	}
}
func TestCollabCallCompletedDoesNotMeanChildCompleted(t *testing.T) {
	events := collaborationEvents(json.RawMessage(`{"item":{"type":"collabAgentToolCall","status":"completed","senderThreadId":"root-thread","agentsStates":{"child":{"status":"running"}}}}`), "root-thread")
	if len(events) != 1 || events[0].Status != "running" || events[0].ParentAgentID != "root" || events[0].AgentID != "codex:child" {
		t.Fatalf("%+v", events)
	}
	if got := threadStatus(json.RawMessage(`{"status":{"type":"active","activeFlags":["waitingOnApproval"]}}`)); got != "waiting_approval" {
		t.Fatal(got)
	}
}

func TestQuotaSnapshotCarriesLatestRequiredReset(t *testing.T) {
	o := rateLimits(json.RawMessage(`{"rateLimits":{"primary":{"usedPercent":100,"resetsAt":100},"secondary":{"usedPercent":100,"resetsAt":200}}}`))
	if o.State != "exhausted" || o.ResetAt != 200 {
		t.Fatalf("%+v", o)
	}
	if o = rateLimits(json.RawMessage(`{"rateLimits":{"primary":{"usedPercent":50}}}`)); o.State != "available" {
		t.Fatalf("%+v", o)
	}
}

func TestUnknownQuotaSentinelDoesNotBecomeZeroUsage(t *testing.T) {
	observation := rateLimits(json.RawMessage(`{"rateLimits":{"primary":{"usedPercent":-1}}}`))
	if len(observation.Windows) != 0 || observation.State != "unknown" {
		t.Fatalf("invented quota: %+v", observation)
	}
}
