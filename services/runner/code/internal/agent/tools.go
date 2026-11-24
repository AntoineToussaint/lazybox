package agent

import "encoding/json"

// CommandTool is the Runner tool through which agents start and inspect
// managed commands.
const CommandTool = "lazybox_command"

// ReviewFindingTool writes a proposed finding for the current reviewer only.
const ReviewFindingTool = "lazybox_review_finding"

const InteractiveCommandAction = "interactive"

// IsInteractiveCommand identifies the command action whose approval the Runner owns.
func IsInteractiveCommand(name string, args json.RawMessage) bool {
	if name != CommandTool {
		return false
	}
	var call struct {
		Action string `json:"action"`
	}
	return json.Unmarshal(args, &call) == nil && (call.Action == InteractiveCommandAction || call.Action == "interactive_read" || call.Action == "interactive_write")
}

// SettingsReadTool and SettingsUpdateTool are the Runner tools through which the
// help assistant reads and proposes changes to Lazybox's own settings. Only the
// help assistant has them; every update is put to the user before it runs.
const (
	SettingsReadTool   = "lazybox_get_settings"
	SettingsUpdateTool = "lazybox_update_settings"
)

// SettingsTool reports whether name is one of the help assistant's settings tools.
func SettingsTool(name string) bool {
	return name == SettingsReadTool || name == SettingsUpdateTool
}

// DocsSearchTool and DocsReadTool are the help assistant's read-only view of
// the Lazybox documentation (Help → Documentation). Neither asks the user.
const (
	DocsSearchTool = "lazybox_search_docs"
	DocsReadTool   = "lazybox_read_doc"
)

// DocsTool reports whether name is one of the help assistant's documentation tools.
func DocsTool(name string) bool {
	return name == DocsSearchTool || name == DocsReadTool
}

// ToolReplaySafe reports whether a tool call may simply run again when it is
// redelivered after a Runner restart. Managed commands outlive the Runner, so
// reading one or waiting for it has no effect a second run could repeat.
// Listing workspace actions has none either. Starting or cancelling a command
// or starting an action does: those get an explicit unknown outcome.
// Reading settings or the documentation has no effect; an approved settings update names the
// revision it was proposed against, so a second run applies nothing once the
// first did — it is refused as a conflict.
func ToolReplaySafe(name string, args json.RawMessage) bool {
	if SettingsTool(name) || DocsTool(name) {
		return true
	}
	if BrowserTool(name) {
		return browserReplaySafe(name)
	}
	if name != CommandTool {
		return false
	}
	var call struct {
		Action string `json:"action"`
	}
	if json.Unmarshal(args, &call) != nil {
		return false
	}
	return call.Action == "interactive_read" || call.Action == "read" || call.Action == "tail" || call.Action == "wait" || call.Action == "actions"
}
