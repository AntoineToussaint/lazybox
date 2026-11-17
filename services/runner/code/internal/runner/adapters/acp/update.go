package acp

import (
	"encoding/json"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// update maps one session/update notification onto journal events.
func (s *session) update(raw json.RawMessage, emit func(agent.Event), activity func(status, name string), responding func()) {
	var n struct {
		Update json.RawMessage `json:"update"`
	}
	if json.Unmarshal(raw, &n) != nil {
		emit(agent.Event{Type: "failure", Status: "protocol_error", Text: "session/update without an update"})
		return
	}
	var head struct {
		Kind string `json:"sessionUpdate"`
	}
	_ = json.Unmarshal(n.Update, &head)
	s.mu.Lock()
	replaying, turn := s.replaying, s.state.Turn
	s.mu.Unlock()
	switch head.Kind {
	case "agent_message_chunk":
		if replaying {
			return
		}
		var c struct {
			Content json.RawMessage `json:"content"`
		}
		_ = json.Unmarshal(n.Update, &c)
		if text := blockText(c.Content); text != "" {
			responding()
			emit(agent.Event{Type: "message_delta", Text: text, TurnID: turn})
		}
	case "agent_thought_chunk":
		// Activity only; the agent's reasoning text is never journaled.
		if !replaying {
			activity("thinking", "")
		}
	case "tool_call", "tool_call_update":
		if replaying {
			return
		}
		call := s.merge(n.Update)
		if call == nil {
			return
		}
		data := s.normalized(call)
		status := call.lazyboxStatus()
		emit(agent.Event{Type: "tool_activity", Status: status, TurnID: turn, Data: data})
		if status == "running" {
			activity("tool", call.displayName())
		} else {
			activity("working", "")
		}
	case "plan":
		if replaying {
			return
		}
		if data := planCall(n.Update, turn); data != nil {
			emit(agent.Event{Type: "tool_activity", Status: "completed", TurnID: turn, Data: data})
		}
	case "available_commands_update":
		if data := commandCatalog(n.Update); data != nil {
			emit(agent.Event{Type: "command_catalog", Data: data})
		}
	case "current_mode_update":
		var u struct {
			Mode string `json:"currentModeId"`
		}
		_ = json.Unmarshal(n.Update, &u)
		s.mu.Lock()
		if s.state.Modes != nil && u.Mode != "" {
			s.state.Modes.Current = u.Mode
		}
		s.mu.Unlock()
	case "config_option_update":
		var u struct {
			Config json.RawMessage `json:"configOptions"`
		}
		_ = json.Unmarshal(n.Update, &u)
		if present(u.Config) {
			s.mu.Lock()
			s.state.Config = u.Config
			s.mu.Unlock()
			if ev, ok := s.catalogEvent(); ok {
				emit(ev)
			}
		}
	case "session_info_update":
		var u struct {
			Title *string `json:"title"`
		}
		if json.Unmarshal(n.Update, &u) == nil && u.Title != nil {
			s.mu.Lock()
			s.state.Title = strings.TrimSpace(*u.Title)
			s.mu.Unlock()
		}
	case "usage_update":
		// Context used and its size, and the session's cost when the agent
		// reports one; telemetry records both (telemetry.Adapter).
		emit(agent.Event{Type: "usage", Data: n.Update})
	}
}

// blockText is a content block as message text: text as it is, a link to a
// resource as a Markdown link. Images and audio have no text.
func blockText(raw json.RawMessage) string {
	var b struct {
		Type     string `json:"type"`
		Text     string `json:"text"`
		Name     string `json:"name"`
		Title    string `json:"title"`
		URI      string `json:"uri"`
		Resource struct {
			URI  string `json:"uri"`
			Text string `json:"text"`
		} `json:"resource"`
	}
	if json.Unmarshal(raw, &b) != nil {
		return ""
	}
	switch b.Type {
	case "text":
		return b.Text
	case "resource_link":
		name := b.Title
		if name == "" {
			name = b.Name
		}
		return "[" + name + "](" + b.URI + ")"
	case "resource":
		return b.Resource.Text
	}
	return ""
}

// toolCall is one ACP tool call as reported so far: updates replace the
// fields they carry.
type toolCall struct {
	ID        string            `json:"toolCallId"`
	Title     string            `json:"title,omitempty"`
	Name      string            `json:"name,omitempty"`
	Kind      string            `json:"kind,omitempty"`
	Status    string            `json:"status,omitempty"`
	Content   []json.RawMessage `json:"content,omitempty"`
	Locations []struct {
		Path string `json:"path"`
		Line *int   `json:"line,omitempty"`
	} `json:"locations,omitempty"`
	RawInput  json.RawMessage `json:"rawInput,omitempty"`
	RawOutput json.RawMessage `json:"rawOutput,omitempty"`
}

// merge folds a tool_call or tool_call_update into the call it is about.
// At most 512 calls are kept; the oldest finished one goes first.
func (s *session) merge(raw json.RawMessage) *toolCall {
	var u struct {
		ID        string          `json:"toolCallId"`
		Title     *string         `json:"title"`
		Name      *string         `json:"name"`
		Kind      *string         `json:"kind"`
		Status    *string         `json:"status"`
		Content   json.RawMessage `json:"content"`
		Locations json.RawMessage `json:"locations"`
		RawInput  json.RawMessage `json:"rawInput"`
		RawOutput json.RawMessage `json:"rawOutput"`
	}
	if json.Unmarshal(raw, &u) != nil || u.ID == "" {
		return nil
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	c := s.calls[u.ID]
	if c == nil {
		if len(s.calls) >= 512 {
			for id, old := range s.calls {
				if old.Status == "completed" || old.Status == "failed" {
					delete(s.calls, id)
					break
				}
			}
		}
		c = &toolCall{ID: u.ID}
		s.calls[u.ID] = c
	}
	set := func(dst *string, src *string) {
		if src != nil {
			*dst = *src
		}
	}
	set(&c.Title, u.Title)
	set(&c.Name, u.Name)
	set(&c.Kind, u.Kind)
	set(&c.Status, u.Status)
	if present(u.Content) {
		_ = json.Unmarshal(u.Content, &c.Content)
	}
	if present(u.Locations) {
		_ = json.Unmarshal(u.Locations, &c.Locations)
	}
	if present(u.RawInput) {
		c.RawInput = u.RawInput
	}
	if present(u.RawOutput) {
		c.RawOutput = u.RawOutput
	}
	copied := *c
	return &copied
}

func (c *toolCall) lazyboxStatus() string {
	switch c.Status {
	case "completed":
		return "completed"
	case "failed":
		return "failed"
	}
	return "running"
}

func (c *toolCall) displayName() string {
	if c.Name != "" {
		return c.Name
	}
	if c.Kind != "" {
		return c.Kind
	}
	return "tool"
}

func (c *toolCall) path() string {
	for _, l := range c.Locations {
		if l.Path != "" {
			return l.Path
		}
	}
	for _, key := range []string{"file_path", "path", "absolute_path", "filePath", "file"} {
		if v := c.input()[key]; v != nil {
			if text, ok := v.(string); ok && text != "" {
				return text
			}
		}
	}
	return ""
}

func (c *toolCall) input() map[string]any {
	var in map[string]any
	_ = json.Unmarshal(c.RawInput, &in)
	if in == nil {
		in = map[string]any{}
	}
	return in
}

func firstString(in map[string]any, keys ...string) string {
	for _, key := range keys {
		switch v := in[key].(type) {
		case string:
			if v != "" {
				return v
			}
		case []any:
			parts := []string{}
			for _, p := range v {
				if text, ok := p.(string); ok {
					parts = append(parts, text)
				}
			}
			if len(parts) > 0 {
				return strings.Join(parts, " ")
			}
		}
	}
	return ""
}

// diffs are the call's file changes, as the agent reported them.
type diff struct {
	Path    string  `json:"path"`
	OldText *string `json:"oldText"`
	NewText string  `json:"newText"`
}

func (c *toolCall) diffs() []diff {
	out := []diff{}
	for _, raw := range c.Content {
		var d struct {
			Type string `json:"type"`
			diff
		}
		if json.Unmarshal(raw, &d) == nil && d.Type == "diff" && d.Path != "" {
			out = append(out, d.diff)
		}
	}
	return out
}

// terminal is the managed command the call runs in, when it embeds one.
func (c *toolCall) terminal() string {
	for _, raw := range c.Content {
		var t struct {
			Type       string `json:"type"`
			TerminalID string `json:"terminalId"`
		}
		if json.Unmarshal(raw, &t) == nil && t.Type == "terminal" && t.TerminalID != "" {
			return t.TerminalID
		}
	}
	return ""
}

// outputText is the call's text content, else its raw output.
func (c *toolCall) outputText() string {
	parts := []string{}
	for _, raw := range c.Content {
		var b struct {
			Type    string          `json:"type"`
			Content json.RawMessage `json:"content"`
		}
		if json.Unmarshal(raw, &b) == nil && b.Type == "content" {
			if text := blockText(b.Content); text != "" {
				parts = append(parts, text)
			}
		}
	}
	if len(parts) > 0 {
		return strings.Join(parts, "\n")
	}
	if present(c.RawOutput) {
		var text string
		if json.Unmarshal(c.RawOutput, &text) == nil {
			return text
		}
		var o map[string]any
		if json.Unmarshal(c.RawOutput, &o) == nil {
			if text := firstString(o, "output", "stdout", "content", "result", "text"); text != "" {
				return text
			}
		}
		return string(c.RawOutput)
	}
	return ""
}

// toolShape is an ACP tool call in the vocabulary every Lazybox surface
// already reads (tool-lines.ts): Claude's tool names and inputs where the
// call's kind says what it is, so it gets the same line, diff and grouping.
func toolShape(c *toolCall) (string, map[string]any) {
	in := c.input()
	title := c.Title
	path := c.path()
	switch c.Kind {
	case "read":
		out := map[string]any{"title": title}
		if path != "" {
			out["file_path"] = path
		}
		return "Read", out
	case "edit":
		diffs := c.diffs()
		switch {
		case len(diffs) == 1 && (diffs[0].OldText == nil || *diffs[0].OldText == ""):
			return "Write", map[string]any{"file_path": diffs[0].Path, "content": bound(diffs[0].NewText), "title": title}
		case len(diffs) == 1:
			before, after := changedRegion(*diffs[0].OldText, diffs[0].NewText)
			return "Edit", map[string]any{"file_path": diffs[0].Path, "old_string": before, "new_string": after, "title": title}
		case len(diffs) > 1:
			changes := []any{}
			for _, d := range diffs {
				old := ""
				kind := "add"
				if d.OldText != nil {
					old, kind = *d.OldText, "update"
				}
				changes = append(changes, map[string]any{"path": d.Path, "kind": kind, "diff": unifiedHunk(old, d.NewText)})
			}
			return "File changes", map[string]any{"changes": changes, "title": title}
		}
		return "Edit", map[string]any{"file_path": path, "title": title}
	case "delete":
		return "Delete", map[string]any{"path": path, "title": title}
	case "move":
		return "Move", map[string]any{"path": path, "title": title}
	case "search":
		pattern := firstString(in, "pattern", "query", "regex", "search")
		if pattern == "" {
			pattern = title
		}
		out := map[string]any{"pattern": pattern}
		if where := firstString(in, "path", "dir_path", "directory"); where != "" {
			out["path"] = where
		}
		return "Grep", out
	case "execute":
		command := firstString(in, "command", "cmd", "argv")
		if command == "" {
			command = title
		}
		out := map[string]any{"command": command}
		if title != "" && title != command {
			out["description"] = title
		}
		if cwd := firstString(in, "cwd", "directory", "dir_path"); cwd != "" {
			out["cwd"] = cwd
		}
		return "Bash", out
	case "think":
		return "Thinking", map[string]any{}
	case "fetch":
		if url := firstString(in, "url", "uri"); url != "" {
			return "WebFetch", map[string]any{"url": url}
		}
		query := firstString(in, "query", "q", "prompt")
		if query == "" {
			query = title
		}
		return "WebSearch", map[string]any{"query": query}
	case "switch_mode":
		return "Switch mode", map[string]any{"title": title}
	}
	name := c.Name
	if name == "" {
		name = title
	}
	if name == "" {
		name = "Tool"
	}
	out := map[string]any{}
	for k, v := range in {
		out[k] = v
	}
	if title != "" && title != name {
		out["title"] = title
	}
	return name, out
}

// normalized is the call as a journal tool_call. A command it ran in a
// Lazybox terminal brings that command's id, its output and its exit code.
func (s *session) normalized(c *toolCall) json.RawMessage {
	name, input := toolShape(c)
	call := map[string]any{"type": "tool_call", "id": c.ID, "name": name, "status": c.lazyboxStatus(), "input": input}
	output := c.outputText()
	if id := c.terminal(); id != "" {
		call["commandId"] = id
		// While it runs, its output is in the console; once it ended, the
		// line says how, like any command's.
		if c.lazyboxStatus() != "running" {
			if out, code, ok := s.commandResult(id); ok {
				if output == "" {
					output = out
				}
				if code != nil {
					call["exitCode"] = *code
				}
			}
		}
	}
	if output != "" && c.lazyboxStatus() != "running" {
		call["output"] = bound(output)
	}
	data, _ := json.Marshal(call)
	return data
}

const textLimit = 32 << 10

// bound keeps a string within textLimit bytes, on a UTF-8 boundary.
func bound(text string) string {
	if len(text) <= textLimit {
		return text
	}
	cut := textLimit
	for cut > 0 && !utf8Start(text[cut]) {
		cut--
	}
	return text[:cut] + "\n[Tool preview truncated]"
}

func utf8Start(b byte) bool { return b&0xC0 != 0x80 }

// changedRegion is the part of an edit that changed, with two lines of
// context: a whole-file oldText/newText pair would fill the journal.
func changedRegion(before, after string) (string, string) {
	a, b := strings.Split(before, "\n"), strings.Split(after, "\n")
	start := 0
	for start < len(a) && start < len(b) && a[start] == b[start] {
		start++
	}
	end := 0
	for end < len(a)-start && end < len(b)-start && a[len(a)-1-end] == b[len(b)-1-end] {
		end++
	}
	from := max(0, start-2)
	tail := max(0, end-2)
	return bound(strings.Join(a[from:len(a)-tail], "\n")), bound(strings.Join(b[from:len(b)-tail], "\n"))
}

// unifiedHunk is one hunk from before to after, for a multi-file change.
func unifiedHunk(before, after string) string {
	old, next := changedRegion(before, after)
	lines := []string{"@@"}
	if old != "" || before != "" {
		for _, l := range strings.Split(old, "\n") {
			lines = append(lines, "-"+l)
		}
	}
	for _, l := range strings.Split(next, "\n") {
		lines = append(lines, "+"+l)
	}
	return bound(strings.Join(lines, "\n"))
}

// planCall is the agent's plan as the conversation's checklist: one
// update_plan call per turn, each update replacing the last.
func planCall(raw json.RawMessage, turn string) json.RawMessage {
	var p struct {
		Entries []struct {
			Content  string `json:"content"`
			Priority string `json:"priority,omitempty"`
			Status   string `json:"status"`
		} `json:"entries"`
	}
	if json.Unmarshal(raw, &p) != nil {
		return nil
	}
	if turn == "" {
		turn = "session"
	}
	data, _ := json.Marshal(map[string]any{"type": "tool_call", "id": "plan:" + turn, "name": "update_plan", "status": "completed", "input": map[string]any{"plan": p.Entries}})
	return data
}

// commandCatalog is the agent's slash commands as a command_catalog event.
func commandCatalog(raw json.RawMessage) json.RawMessage {
	var u struct {
		Commands []struct {
			Name        string `json:"name"`
			Description string `json:"description"`
			Input       *struct {
				Hint string `json:"hint"`
			} `json:"input"`
		} `json:"availableCommands"`
	}
	if json.Unmarshal(raw, &u) != nil {
		return nil
	}
	commands := []map[string]any{}
	for _, c := range u.Commands {
		name := strings.TrimPrefix(strings.TrimSpace(c.Name), "/")
		if name == "" {
			continue
		}
		command := map[string]any{"name": name, "description": c.Description}
		if c.Input != nil && c.Input.Hint != "" {
			command["argumentHint"] = c.Input.Hint
		}
		commands = append(commands, command)
	}
	data, _ := json.Marshal(map[string]any{"commands": commands})
	return data
}
