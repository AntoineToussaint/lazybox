package codex

import (
	"encoding/json"
	"strings"
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

func TestSpawnNamesTheChildAfterItsBriefAndLinksTheSpawningCall(t *testing.T) {
	long := strings.Repeat("x", 100)
	events := collaborationEvents(json.RawMessage(`{"item":{"id":"call-1","type":"collabAgentToolCall","tool":"spawnAgent","status":"completed","senderThreadId":"root-thread","prompt":"  Audit the reader `+long+`\nThen report.","agentsStates":{"b":{"status":"running"},"a":{"status":"completed","message":"All good"}}}}`), "root-thread")
	if len(events) != 2 || events[0].AgentID != "codex:a" || events[1].AgentID != "codex:b" {
		t.Fatalf("children not reported in a stable order: %+v", events)
	}
	if name := []rune(events[0].Name); len(name) != 80 || !strings.HasPrefix(events[0].Name, "Audit the reader") || !strings.HasSuffix(events[0].Name, "…") {
		t.Fatalf("name %q", events[0].Name)
	}
	var data map[string]string
	if err := json.Unmarshal(events[0].Data, &data); err != nil || data["toolUseId"] != "call-1" || !strings.Contains(data["prompt"], "Then report.") || data["message"] != "All good" {
		t.Fatalf("%s %v", events[0].Data, err)
	}
	if strings.Contains(string(events[1].Data), "message") {
		t.Fatalf("a child without a message got one: %s", events[1].Data)
	}
	// A follow-up message to a child is not its brief: it does not rename it.
	follow := collaborationEvents(json.RawMessage(`{"item":{"id":"call-2","type":"collabAgentToolCall","tool":"sendInput","senderThreadId":"root-thread","prompt":"Also check X","agentsStates":{"a":{"status":"running"}}}}`), "root-thread")
	if len(follow) != 1 || follow[0].Name != "" || strings.Contains(string(follow[0].Data), "Also check X") {
		t.Fatalf("%+v %s", follow, follow[0].Data)
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
