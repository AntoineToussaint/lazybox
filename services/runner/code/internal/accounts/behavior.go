package accounts

// CheckInFirst starts a conversation with read-only permissions.
const CheckInFirst = "check-in-first-v1"

const sharedBehavior = `Read the available task and workspace context before deciding whether to ask. Respect the user's scope and the active permissions. Keep updates short and use them for decisions, findings, or blockers. Track every explicitly requested deliverable. A plan, a started command, or a pending verification step is progress. Continue through the authorized steps and verify the outcome before reporting completion. If you must stop, say exactly what remains and what decision, permission, or external change is needed.`

// BehaviorInstructions returns the pinned guidance for a selected profile.
// Permissions are resolved independently by the Runner.
func (o Options) BehaviorInstructions() string {
	switch o.BehaviorProfile {
	case "balanced-v1":
		return "## Behavior profile: Balanced\nInspect available context before asking. Start clearly requested reversible work. State assumptions that materially affect the result. Ask when an unresolved choice would change the outcome or require new authorization. Verify the result before reporting completion."
	case "hands-on-v1":
		return "## Behavior profile: Hands-on\nInspect the workspace and start authorized work promptly. Make reversible choices within scope and recover from routine failures. Ask about material ambiguities you cannot resolve from available evidence. Verify the result before reporting completion."
	case "balanced-v2":
		return "## Behavior profile: Balanced\n" + sharedBehavior + "\nStart clearly requested reversible work. State assumptions that materially affect the outcome. Ask when available evidence cannot resolve a choice that would materially change the result."
	case "hands-on-v2":
		return "## Behavior profile: Hands-on\n" + sharedBehavior + "\nStart authorized work promptly. Make reversible decisions within scope, recover from routine failures, and keep going through checks, handoffs, and other requested follow-through. Ask when a material decision or permission actually blocks progress."
	case CheckInFirst:
		return "## Behavior profile: Check in first\n" + sharedBehavior + "\nFor a request that changes files or external state, first inspect the relevant context, describe the intended change and verification, then wait for approval. After the user approves this plan and grants the needed permissions, carry it out without another check-in unless the scope materially changes."
	default:
		return ""
	}
}
