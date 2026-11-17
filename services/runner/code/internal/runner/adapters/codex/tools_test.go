package codex

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestToolItemsNormalizeAtProviderBoundary(t *testing.T) {
	for _, tc := range []struct {
		raw  string
		done bool
		want string
	}{
		{`{"item":{"id":"cmd","type":"commandExecution","command":"echo fixture"}}`, false, `"status":"running"`},
		{`{"item":{"id":"cmd","type":"commandExecution","aggregatedOutput":"fixture","exitCode":0}}`, true, `"status":"completed"`},
		{`{"item":{"id":"cmd","type":"commandExecution","exitCode":1}}`, true, `"status":"failed"`},
		{`{"item":{"id":"mcp","type":"mcpToolCall","tool":"read","error":{"message":"failed"}}}`, true, `"status":"failed"`},
		// A command line says how it ended, how long it took, where, and what it read.
		{`{"item":{"id":"cmd","type":"commandExecution","command":"cat a.go","cwd":"/w","exitCode":0,"durationMs":4200,"commandActions":[{"type":"read","command":"cat a.go","name":"a.go","path":"/w/a.go"}]}}`, true, `"actions":[{"type":"read","command":"cat a.go","name":"a.go","path":"/w/a.go"}],"cwd":"/w","durationMs":4200,"exitCode":0`},
	} {
		got, err := normalizedToolCall(json.RawMessage(tc.raw), tc.done)
		if err != nil || !strings.Contains(string(got), tc.want) || !strings.Contains(string(got), `"type":"tool_call"`) {
			t.Fatal(string(got), err)
		}
	}
	for _, kind := range []string{"reasoning", "agentMessage", "future"} {
		got, err := normalizedToolCall(json.RawMessage(`{"item":{"id":"secret","type":"`+kind+`","text":"PRIVATE"}}`), true)
		if err != nil || len(got) != 0 {
			t.Fatal(string(got), err)
		}
	}
}
func TestSubAgentCallCarriesItsBriefAndReceivers(t *testing.T) {
	got, err := normalizedToolCall(json.RawMessage(`{"item":{"id":"call","type":"collabAgentToolCall","tool":"spawnAgent","status":"inProgress","prompt":"Audit the reader","receiverThreadIds":["child"]}}`), false)
	if err != nil {
		t.Fatal(err)
	}
	var call struct {
		Name  string
		Input map[string]any
	}
	if err = json.Unmarshal(got, &call); err != nil || call.Name != "spawnAgent" || call.Input["prompt"] != "Audit the reader" || call.Input["tool"] != "spawnAgent" {
		t.Fatalf("%s %v", got, err)
	}
	if receivers, _ := call.Input["receiverThreadIds"].([]any); len(receivers) != 1 || receivers[0] != "child" {
		t.Fatalf("%s", got)
	}
}
func TestLargeToolOutputIsBoundedBeforeJournaling(t *testing.T) {
	raw, _ := json.Marshal(map[string]any{"item": map[string]any{"id": "large", "type": "commandExecution", "aggregatedOutput": strings.Repeat("x", 1000000)}})
	output, err := normalizedToolCall(raw, true)
	if err != nil || len(output) > 34000 || !strings.Contains(string(output), "Tool preview truncated") {
		t.Fatal(len(output), err)
	}
}

func TestPlanUpdatesAreOneChecklistPerTurn(t *testing.T) {
	got := string(planToolCall(json.RawMessage(`{"threadId":"t","turnId":"turn-1","explanation":"Two steps","plan":[{"step":"Read","status":"completed"},{"step":"Fix","status":"inProgress"}]}`)))
	want := `{"id":"plan:turn-1","input":{"explanation":"Two steps","plan":[{"step":"Read","status":"completed"},{"step":"Fix","status":"inProgress"}]},"name":"update_plan","status":"completed","type":"tool_call"}`
	if got != want {
		t.Fatal(got)
	}
	if planToolCall(json.RawMessage(`{"turnId":"turn-1","plan":null}`)) != nil {
		t.Fatal("an empty plan is not a call")
	}
}
