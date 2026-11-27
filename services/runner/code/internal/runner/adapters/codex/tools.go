package codex

import (
	"encoding/json"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

const commandSchema = `{"type":"object","properties":{"action":{"type":"string","enum":["start","read","wait","cancel","actions","start_action","interactive","interactive_read","interactive_write","verify","test"]},"input":{"type":"string"},"agentInput":{"type":"boolean"},"member":{"type":"string"},"expected":{"type":"string"},"regression":{"type":"string"},"argv":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string","description":"interactive: optional absolute directory inside the conversation workspace."},"id":{"type":"string"},"offset":{"type":"integer","minimum":0},"name":{"type":"string","description":"start_action: the workspace action to start, as actions lists it; omit for the default action."},"restart":{"type":"boolean","description":"start_action: restart it if it is running."}},"required":["action"],"additionalProperties":false}`

const reviewFindingSchema = `{"type":"object","properties":{"file":{"type":"string","description":"Workspace-relative repository/path."},"startLine":{"type":"integer","minimum":1},"endLine":{"type":"integer","minimum":1},"severity":{"type":"string","enum":["blocker","major","minor","nit"]},"title":{"type":"string"},"body":{"type":"string"},"key":{"type":"string","description":"Optional stable key for retrying the same finding."}},"required":["file","startLine","severity","title","body"],"additionalProperties":false}`

const settingsChangeSchema = `{"type":"object","properties":{"revision":{"type":"integer","minimum":1,"description":"The revision lazybox_get_settings returned. A stale revision is refused."},"reason":{"type":"string","description":"One or two plain sentences for the user: what changes and why."},"changes":{"type":"object","description":"Only the settings that change; each value replaces the current one whole.","properties":{"naming":{"type":"string","enum":["repositories","generated"]},"permission":{"type":"string","enum":["","ask","read-only","yolo"]},"context":{"type":"string","maxLength":32768},"claudeContext":{"type":"string","maxLength":16384},"codexContext":{"type":"string","maxLength":16384}},"additionalProperties":false}},"required":["revision","reason","changes"],"additionalProperties":false}`

// dynamicTools are the Runner tools an execution may call, by its purpose: the
// help assistant gets the documentation and settings tools, a coding agent the managed-command
// tool (and the browser tools when the Runner offers its agent browser), and
// other assistants (which have no Runner tools) none.
func dynamicTools(purpose string, browser bool) []map[string]any {
	switch {
	case purpose == "help":
		return []map[string]any{
			{"type": "function", "name": agent.DocsSearchTool, "description": "Search the Lazybox documentation (the pages Help → Documentation shows) by a few words. Returns the best pages with their path, title and matching lines.", "inputSchema": json.RawMessage(`{"type":"object","properties":{"query":{"type":"string","description":"A few words about the topic, such as \"switch subscription limit\"."}},"required":["query"],"additionalProperties":false}`)},
			{"type": "function", "name": agent.DocsReadTool, "description": "Read one Lazybox documentation page by the path search or the page list gives, such as guides/chat.md.", "inputSchema": json.RawMessage(`{"type":"object","properties":{"path":{"type":"string","description":"The page's path inside the documentation, such as guides/subscriptions.md."}},"required":["path"],"additionalProperties":false}`)},
			{"type": "function", "name": agent.SettingsReadTool, "description": "Read Lazybox's agent settings (Settings → Agents & workspaces) and their revision. Never returns credentials.", "inputSchema": json.RawMessage(`{"type":"object","properties":{},"additionalProperties":false}`)},
			{"type": "function", "name": agent.SettingsUpdateTool, "description": "Propose a change to Lazybox's agent settings. The user sees the reason and the proposed values and must allow it; nothing changes otherwise. Changeable: naming, permission, context, claudeContext, codexContext.", "inputSchema": json.RawMessage(settingsChangeSchema)},
		}
	case agent.IsAssistant(purpose):
		return nil
	}
	tools := []map[string]any{{"type": "function", "name": agent.CommandTool, "description": "Start, read, wait for or cancel a runner-owned command. Output remains available to independent human consoles. Start requires human approval. Never repeat a start after receiving its command id. interactive proposes argv for the person to review and run in a real terminal for prompts or sign-in. Set agentInput to true for ordinary prompts. After approval use interactive_read with the returned id and offset to see prompts and exit status, and interactive_write with id and input to type exact text (include a newline to submit). For credentials omit agentInput so the person types privately and the tool waits for exit. actions lists the workspace's actions (dev servers and the like, with their state and URL); start_action starts one by name (restart: true restarts it); read its output with its commandId.", "inputSchema": json.RawMessage(commandSchema)}}
	tools = append(tools, map[string]any{"type": "function", "name": agent.ReviewFindingTool, "description": "File one proposed finding in the current agent review. Only the assigned reviewer can call this; returns the saved finding ID.", "inputSchema": json.RawMessage(reviewFindingSchema)})
	if browser {
		for _, t := range agent.BrowserTools {
			tools = append(tools, map[string]any{"type": "function", "name": t.Name, "description": t.Description, "inputSchema": t.Schema})
		}
	}
	return tools
}

// toolOffered reports whether name is one of the dynamic tools purpose offers.
// A coding thread resumed from an earlier start may hold the browser tools
// whatever this Runner offers now; the Runner refuses them when browsing is off.
func toolOffered(purpose, name string) bool {
	for _, t := range dynamicTools(purpose, !agent.IsAssistant(purpose)) {
		if t["name"] == name {
			return true
		}
	}
	return false
}

// toolApproval is the approval card title of a dynamic tool call; empty when
// the call runs without asking, which reading settings, documentation and managed command state does.
func toolApproval(name string, args json.RawMessage) string {
	if name == agent.CommandTool && agent.ToolReplaySafe(name, args) {
		return ""
	}
	switch name {
	case agent.SettingsReadTool, agent.DocsSearchTool, agent.DocsReadTool, agent.ReviewFindingTool:
		return ""
	case agent.SettingsUpdateTool:
		return "Change Lazybox settings"
	}
	if agent.BrowserTool(name) {
		// The browser asks the person itself, per site, download and sign-in
		// (BROWSER.md); a second approval per click would teach them to click through.
		return ""
	}
	return "Run managed command tool"
}

// toolDeclined answers the agent when the user declines a dynamic tool call.
func toolDeclined(name string) string {
	if name == agent.SettingsUpdateTool {
		return "The user declined this settings change; nothing was saved."
	}
	return "Command declined"
}

// normalizedToolCall is the provider boundary. Consumers need no Codex item
// semantics, and reasoning/message items never become tool history.
func normalizedToolCall(raw json.RawMessage, completed bool) (json.RawMessage, error) {
	var p struct {
		Item struct {
			ID, Type, Tool, Status, Cwd                                         string
			Arguments, Command, Query, Changes, Result, AggregatedOutput, Error json.RawMessage
			CommandActions                                                      json.RawMessage
			ExitCode                                                            *int
			DurationMs                                                          *int64
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
	input := first(i.Arguments, i.Command, i.Query, i.Changes)
	if i.Type == "collabAgentToolCall" {
		// What the parent asked of which sub-agents; the input a reader needs.
		var collab struct {
			Item struct {
				Prompt            *string  `json:"prompt"`
				ReceiverThreadIDs []string `json:"receiverThreadIds"`
			} `json:"item"`
		}
		_ = json.Unmarshal(raw, &collab)
		spawn := map[string]any{}
		if collab.Item.Prompt != nil && *collab.Item.Prompt != "" {
			spawn["prompt"] = boundedRunes(*collab.Item.Prompt, briefLimit)
		}
		if len(collab.Item.ReceiverThreadIDs) > 0 {
			spawn["receiverThreadIds"] = collab.Item.ReceiverThreadIDs
		}
		if i.Tool != "" {
			spawn["tool"] = i.Tool
		}
		if len(spawn) > 0 {
			input, _ = json.Marshal(spawn)
		}
	}
	out := struct {
		Type, ID, Name, Status string
		Input, Output          json.RawMessage
	}{Type: "tool_call", ID: i.ID, Name: name, Status: status, Input: input, Output: first(i.Result, i.AggregatedOutput, i.Error)}
	// Keep the JSON vocabulary identical across adapters.
	call := map[string]any{"type": out.Type, "id": out.ID, "name": out.Name, "status": out.Status, "input": out.Input, "output": out.Output}
	// What a transcript line says of a command: how it ended, how long it
	// took, where it ran, and what Codex parsed it as (a read, a search).
	if i.ExitCode != nil {
		call["exitCode"] = *i.ExitCode
	}
	if i.DurationMs != nil {
		call["durationMs"] = *i.DurationMs
	}
	if i.Cwd != "" {
		call["cwd"] = i.Cwd
	}
	if len(i.CommandActions) > 0 && string(i.CommandActions) != "null" && string(i.CommandActions) != "[]" && len(i.CommandActions) <= 4096 {
		call["actions"] = i.CommandActions
	}
	return json.Marshal(call)
}

// planToolCall is Codex's plan update in the normalized tool-call shape,
// named like its update_plan tool, one call per turn.
func planToolCall(raw json.RawMessage) json.RawMessage {
	var p struct {
		TurnID      string          `json:"turnId"`
		Explanation *string         `json:"explanation"`
		Plan        json.RawMessage `json:"plan"`
	}
	if json.Unmarshal(raw, &p) != nil || p.TurnID == "" || len(p.Plan) == 0 || string(p.Plan) == "null" {
		return nil
	}
	input := map[string]any{"plan": p.Plan}
	if p.Explanation != nil && *p.Explanation != "" {
		input["explanation"] = *p.Explanation
	}
	out, _ := json.Marshal(map[string]any{"type": "tool_call", "id": "plan:" + p.TurnID, "name": "update_plan", "status": "completed", "input": input})
	return out
}

// browserContent is a browser tool's result as Codex content items: its text,
// and a screenshot as an image.
func browserContent(text string) []any {
	r := agent.ParseBrowserResult(text)
	items := []any{map[string]string{"type": "inputText", "text": r.Text}}
	if r.Image != nil {
		items = append(items, map[string]string{"type": "inputImage", "imageUrl": "data:" + r.Image.MediaType + ";base64," + r.Image.Data})
	}
	return items
}
