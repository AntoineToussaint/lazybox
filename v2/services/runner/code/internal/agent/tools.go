package agent

import "encoding/json"

// CommandTool is the Runner tool through which agents start and inspect
// managed commands.
const CommandTool = "lazybox_command"

// ToolReplaySafe reports whether a tool call may simply run again when it is
// redelivered after a Runner restart. Managed commands outlive the Runner, so
// reading one or waiting for it has no effect a second run could repeat.
// Starting or cancelling one does: those get an explicit unknown outcome.
func ToolReplaySafe(name string, args json.RawMessage) bool {
	if name != CommandTool {
		return false
	}
	var call struct {
		Action string `json:"action"`
	}
	if json.Unmarshal(args, &call) != nil {
		return false
	}
	return call.Action == "read" || call.Action == "wait"
}
