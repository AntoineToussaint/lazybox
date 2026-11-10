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
func TestLargeToolOutputIsBoundedBeforeJournaling(t *testing.T) {
	raw, _ := json.Marshal(map[string]any{"item": map[string]any{"id": "large", "type": "commandExecution", "aggregatedOutput": strings.Repeat("x", 1000000)}})
	output, err := normalizedToolCall(raw, true)
	if err != nil || len(output) > 34000 || !strings.Contains(string(output), "Tool preview truncated") {
		t.Fatal(len(output), err)
	}
}
