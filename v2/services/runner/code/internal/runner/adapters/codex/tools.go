package codex

import "encoding/json"

// normalizedToolCall is the provider boundary. Consumers need no Codex item
// semantics, and reasoning/message items never become tool history.
func normalizedToolCall(raw json.RawMessage, completed bool) (json.RawMessage, error) {
	var p struct {
		Item struct {
			ID, Type, Tool, Status                                              string
			Arguments, Command, Query, Changes, Result, AggregatedOutput, Error json.RawMessage
			ExitCode                                                            *int
		}
	}
	if err := json.Unmarshal(raw, &p); err != nil {
		return nil, err
	}
	i := p.Item
	names := map[string]string{"commandExecution": "Shell command", "fileChange": "File changes", "mcpToolCall": "MCP tool", "dynamicToolCall": "Tool", "webSearch": "Web search", "imageView": "View image", "collabAgentToolCall": "Sub-agent"}
	name, ok := names[i.Type]
	if !ok || i.ID == "" {
		return nil, nil
	}
	if i.Tool != "" {
		name = i.Tool
	}
	status := "running"
	if completed {
		status = "completed"
	}
	if i.Status == "failed" || i.Status == "declined" || i.Status == "error" || (len(i.Error) > 0 && string(i.Error) != "null") || (i.ExitCode != nil && *i.ExitCode != 0) {
		status = "failed"
	}
	first := func(values ...json.RawMessage) json.RawMessage {
		for _, v := range values {
			if len(v) > 0 && string(v) != "null" {
				if len(v) > 32768 {
					var text string
					if json.Unmarshal(v, &text) != nil {
						text = string(v)
					}
					if len(text) > 32768 {
						text = text[:32768]
					}
					bounded, _ := json.Marshal(text + "\n[Tool preview truncated]")
					return bounded
				}
				return v
			}
		}
		return nil
	}
	out := struct {
		Type, ID, Name, Status string
		Input, Output          json.RawMessage
	}{Type: "tool_call", ID: i.ID, Name: name, Status: status, Input: first(i.Arguments, i.Command, i.Query, i.Changes), Output: first(i.Result, i.AggregatedOutput, i.Error)}
	// Keep the JSON vocabulary identical across adapters.
	return json.Marshal(map[string]any{"type": out.Type, "id": out.ID, "name": out.Name, "status": out.Status, "input": out.Input, "output": out.Output})
}
