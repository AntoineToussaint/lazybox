package accounts

// BehaviorInstructions returns the pinned guidance for a selected profile.
// Permissions are resolved independently by the Runner.
func (o Options) BehaviorInstructions() string {
	switch o.BehaviorProfile {
	case "balanced-v1":
		return "## Behavior profile: Balanced\nInspect available context before asking. Start clearly requested reversible work. State assumptions that materially affect the result. Ask when an unresolved choice would change the outcome or require new authorization. Verify the result before reporting completion."
	case "hands-on-v1":
		return "## Behavior profile: Hands-on\nInspect the workspace and start authorized work promptly. Make reversible choices within scope and recover from routine failures. Ask about material ambiguities you cannot resolve from available evidence. Verify the result before reporting completion."
	default:
		return ""
	}
}
