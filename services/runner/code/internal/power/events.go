package power

import (
	"path"
	"slices"
	"strconv"
	"strings"
	"time"
)

// Events hooks may follow.
const (
	// EventNeedsInput: an agent asks for an approval or an answer.
	EventWorkspaceArchived    = "workspace.archived"
	EventConversationArchived = "conversation.archived"
	EventNeedsInput           = "agent.needs-input"
	// EventTurnDone: an agent's turn ended normally.
	EventTurnDone = "turn.done"
	// EventTurnFailed: a turn ended in a failure (a fault, a provider error).
	EventTurnFailed = "turn.failed"
	// EventChecksFailed: a pull request's checks failed on its latest head.
	EventChecksFailed = "pr.checks-failed"
	// EventMissionAccepted: a person accepted a mission task's work.
	EventMissionAccepted = "mission.accepted"
	// The other transitions of a pull request a workspace delivers
	// (runner/items): each emitted once, when it happens.
	EventPROpened         = "pr.opened"
	EventChecksPassed     = "pr.checks-passed"
	EventChangesRequested = "pr.changes-requested"
	EventPRApproved       = "pr.approved"
	EventPRMerged         = "pr.merged"
	EventPRClosed         = "pr.closed"
	EventPRConflicts      = "pr.conflicts"
)

// Events lists every event, in the order Settings shows them.
var Events = []string{EventWorkspaceArchived, EventConversationArchived, EventNeedsInput, EventTurnDone, EventTurnFailed, EventPROpened, EventChecksFailed, EventChecksPassed, EventChangesRequested, EventPRApproved, EventPRMerged, EventPRClosed, EventPRConflicts, EventMissionAccepted}

// Event is what happened, as a hook receives it: JSON on stdin and in the
// file $LAZYBOX_EVENT_FILE, the main fields also as LAZYBOX_EVENT_*
// variables, and the body of a webhook's POST.
type Event struct {
	Name string    `json:"event"`
	At   time.Time `json:"at"`
	// Test marks a run started from Settings' Test button.
	Test         bool          `json:"test,omitempty"`
	Workspace    *Ref          `json:"workspace,omitempty"`
	Conversation *Conversation `json:"conversation,omitempty"`
	// Reason is one short phrase ("Approval: npm test", "checks failed: lint").
	Reason string `json:"reason,omitempty"`
	// Line is the agent's last line, the pending question, or the failure.
	Line        string       `json:"line,omitempty"`
	Request     *Request     `json:"request,omitempty"`
	PullRequest *PullRequest `json:"pullRequest,omitempty"`
	Mission     *Mission     `json:"mission,omitempty"`
	// Key deduplicates: one event, one run per hook.
	Key string `json:"-"`
}

type Ref struct {
	ID   string `json:"id"`
	Name string `json:"name,omitempty"`
}

type Conversation struct {
	ID       string `json:"id"`
	Session  string `json:"session"`
	Title    string `json:"title,omitempty"`
	Provider string `json:"provider,omitempty"`
	Model    string `json:"model,omitempty"`
}

type Request struct {
	ID   string `json:"id"`
	Kind string `json:"kind"`
}

type PullRequest struct {
	Repository string   `json:"repository"`
	Number     int64    `json:"number"`
	URL        string   `json:"url,omitempty"`
	Title      string   `json:"title,omitempty"`
	Head       string   `json:"head"`
	Failed     []string `json:"failed,omitempty"`
}

type Mission struct {
	ID        string `json:"id"`
	Title     string `json:"title,omitempty"`
	Task      string `json:"task,omitempty"`
	TaskTitle string `json:"taskTitle,omitempty"`
	Note      string `json:"note,omitempty"`
}

// Summary is one line about the event, for Settings and the run's title.
func (e Event) Summary() string {
	var parts []string
	if e.Workspace != nil && e.Workspace.Name != "" {
		parts = append(parts, e.Workspace.Name)
	}
	switch {
	case e.Conversation != nil && e.Conversation.Title != "":
		parts = append(parts, e.Conversation.Title)
	case e.PullRequest != nil:
		parts = append(parts, e.PullRequest.Repository+"#"+itoa(e.PullRequest.Number))
	case e.Mission != nil && e.Mission.TaskTitle != "":
		parts = append(parts, e.Mission.TaskTitle)
	}
	line := strings.Join(parts, " · ")
	if e.Reason != "" {
		if line != "" {
			line += ": "
		}
		line += e.Reason
	}
	if line == "" {
		return e.Name
	}
	return line
}

func itoa(n int64) string { return strconv.FormatInt(n, 10) }

// Matches reports whether the hook follows this event and its filters admit it.
func (h Hook) Matches(e Event) bool {
	if !h.Enabled || !slices.Contains(h.On, e.Name) {
		return false
	}
	if len(h.Workspaces) > 0 && (e.Workspace == nil || !anyGlob(h.Workspaces, e.Workspace.Name, e.Workspace.ID)) {
		return false
	}
	if len(h.Conversations) > 0 && (e.Conversation == nil || !anyGlob(h.Conversations, e.Conversation.Title, e.Conversation.ID)) {
		return false
	}
	if len(h.Repositories) > 0 && (e.PullRequest == nil || !anyGlob(h.Repositories, e.PullRequest.Repository)) {
		return false
	}
	return true
}

// anyGlob matches values against patterns, case-insensitively, with * and ?.
func anyGlob(patterns []string, values ...string) bool {
	for _, p := range patterns {
		for _, v := range values {
			if v == "" {
				continue
			}
			if ok, err := path.Match(strings.ToLower(p), strings.ToLower(v)); err == nil && ok {
				return true
			}
		}
	}
	return false
}

// Environment is a hook program's environment: the person's own, without the
// Runner's configuration and service wiring (every LAZYBOX_V2_* and CODEFLY*
// variable, so no inherited credential reaches it), plus the event. Values
// that agents can influence (titles, last lines) arrive only as variables and
// JSON, never spliced into a command line, so a hook cannot be injected.
func Environment(inherited []string, e Event, eventFile string) []string {
	drop := map[string]bool{"PWD": true, "OLDPWD": true, "SHLVL": true, "CLAUDECODE": true}
	env := make([]string, 0, len(inherited)+12)
	for _, entry := range inherited {
		name, _, ok := strings.Cut(entry, "=")
		if !ok || drop[name] || strings.HasPrefix(name, "LAZYBOX_V2_") || strings.HasPrefix(name, "LAZYBOX_EVENT") || strings.HasPrefix(name, "CODEFLY") {
			continue
		}
		env = append(env, entry)
	}
	add := func(name, value string) {
		if value != "" {
			env = append(env, name+"="+strings.ReplaceAll(value, "\x00", ""))
		}
	}
	add("LAZYBOX_EVENT", e.Name)
	add("LAZYBOX_EVENT_FILE", eventFile)
	add("LAZYBOX_EVENT_SUMMARY", e.Summary())
	add("LAZYBOX_EVENT_REASON", e.Reason)
	if e.Test {
		add("LAZYBOX_EVENT_TEST", "1")
	}
	if e.Workspace != nil {
		add("LAZYBOX_EVENT_WORKSPACE", e.Workspace.ID)
		add("LAZYBOX_EVENT_WORKSPACE_NAME", e.Workspace.Name)
	}
	if e.Conversation != nil {
		add("LAZYBOX_EVENT_CONVERSATION", e.Conversation.ID)
		add("LAZYBOX_EVENT_SESSION", e.Conversation.Session)
		add("LAZYBOX_EVENT_TITLE", e.Conversation.Title)
	}
	if e.Request != nil {
		add("LAZYBOX_EVENT_REQUEST", e.Request.ID)
	}
	if e.PullRequest != nil {
		add("LAZYBOX_EVENT_PR_URL", e.PullRequest.URL)
		add("LAZYBOX_EVENT_PR_REPOSITORY", e.PullRequest.Repository)
	}
	if e.Mission != nil {
		add("LAZYBOX_EVENT_MISSION", e.Mission.ID)
	}
	return env
}
