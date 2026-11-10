package agent

import (
	"encoding/json"
	"testing"
)

func TestOnlyInspectingACommandIsReplaySafe(t *testing.T) {
	for _, c := range []struct {
		name, args string
		safe       bool
	}{
		{CommandTool, `{"action":"read","id":"x"}`, true},
		{CommandTool, `{"action":"wait","id":"x"}`, true},
		{CommandTool, `{"action":"start","argv":["make"]}`, false},
		{CommandTool, `{"action":"cancel","id":"x"}`, false},
		{CommandTool, `not json`, false},
		{"other_tool", `{"action":"read"}`, false},
	} {
		if got := ToolReplaySafe(c.name, json.RawMessage(c.args)); got != c.safe {
			t.Errorf("%s %s: replay safe %v, want %v", c.name, c.args, got, c.safe)
		}
	}
}
