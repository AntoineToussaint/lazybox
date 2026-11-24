package agent

// Type is one kind of agent. A conversation's purpose names its type; what a
// type may do, where it is shown and how it ends are decided here, once,
// rather than by comparing purposes where each decision is made.
type Type struct {
	// Name is the purpose it is defined for.
	Name  string
	Label string
	// Assistant: an Ask Lazybox side panel. It has no workspace or
	// repository lease and is read-only.
	Assistant bool
	// Workspace: it works in a workspace. It edits files, takes interactive
	// commands and workspace actions, and gets the person's behaviour profile.
	Workspace bool
	// Delegates: it may start other agents, conversations and missions.
	Delegates bool
	// Reviewable: its turns can be reviewed. A reviewer's own are not.
	Reviewable bool
	// Listed: it is a conversation of its own in the sidebar, tabs and lists.
	// An unlisted agent is reached from what it belongs to (an artifact,
	// a review).
	Listed bool
	// ReachesInbox: its questions, approvals and failures reach the inbox
	// and notifications. True for every agent that can act, listed or not.
	ReachesInbox bool
	// CloseWhenDelivered: its job ends once its work is handed on.
	CloseWhenDelivered bool
	// UI is how a client shows it: "chat", "reviewer", "companion" or
	// "assistant".
	UI string
}

// Purposes of the built-in types.
const (
	Coding   = "coding"
	Reviewer = "reviewer"
	Artifact = "artifact"
	Help     = "help"
	Planning = "planning"
	Summary  = "summary"
)

var types = map[string]Type{
	Coding: {Name: Coding, Label: "Coding agent", Workspace: true, Delegates: true, Reviewable: true, Listed: true, ReachesInbox: true, UI: "chat"},
	// A reviewer reads a conversation's work and files findings. It belongs
	// to its review: its card in the author's conversation and the review
	// tray open it.
	Reviewer: {Name: Reviewer, Label: "Reviewer", Workspace: true, ReachesInbox: true, CloseWhenDelivered: true, UI: "reviewer"},
	// An artifact companion works beside one artifact, opened from it.
	Artifact: {Name: Artifact, Label: "Artifact companion", Workspace: true, Reviewable: true, ReachesInbox: true, UI: "companion"},
	Help:     {Name: Help, Label: "Ask Lazybox", Assistant: true, UI: "assistant"},
	Planning: {Name: Planning, Label: "Planning assistant", Assistant: true, UI: "assistant"},
	Summary:  {Name: Summary, Label: "Summary assistant", Assistant: true, UI: "assistant"},
}

// TypeOf is the type a purpose names. An empty purpose is a coding agent (a
// conversation saved before purposes existed). An unknown one is treated as
// an assistant, the narrowest type, until something defines it.
func TypeOf(purpose string) Type {
	if purpose == "" {
		return types[Coding]
	}
	if t, ok := types[purpose]; ok {
		return t
	}
	return Type{Name: purpose, Label: purpose, Assistant: true, UI: "assistant"}
}

// IsAssistant reports whether purpose names an Ask Lazybox assistant.
func IsAssistant(purpose string) bool {
	_, known := types[purpose]
	return known && TypeOf(purpose).Assistant
}

// ValidPurpose reports whether a conversation may be opened with purpose.
func ValidPurpose(purpose string) bool {
	_, known := types[purpose]
	return purpose == "" || known
}
