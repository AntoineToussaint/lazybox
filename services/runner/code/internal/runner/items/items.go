// Package items keeps the work items a workspace delivers or mentions (pull
// requests first, PROJECTS_AND_INTEGRATIONS.md): a reference, a small snapshot
// of what Lazybox shows, and links to workspaces and conversations. Links and
// snapshots persist in the Runner's state, so a restart loses nothing and a
// deleted workspace's pull requests stay readable as history.
//
// A provider (GitHub, built in; extensions later) resolves URLs to items and
// keeps linked items fresh. The package owns the pure parts: the store, the
// transitions a fresh snapshot makes (the pr.* events, emitted once), when
// each item is due to be read again, and how agent output names pull
// requests. The Runner (runner/items.go) drives it.
package items

import (
	"strings"
	"time"
)

// Ref names an item at its provider.
type Ref struct {
	Provider string `json:"provider"`
	// Key is the provider's stable key: owner/name#number on GitHub.
	Key string `json:"key"`
}

// ID is the ref as one string, unique across providers.
func (r Ref) ID() string { return r.Provider + ":" + r.Key }

func (r Ref) Valid() bool { return r.Provider != "" && r.Key != "" }

// Kinds of item.
const (
	KindPullRequest = "pull_request"
	KindIssue       = "issue"
)

// State categories: the same words for every provider.
const (
	StateOpen    = "open"
	StateStarted = "started"
	StateReview  = "review"
	StateDone    = "done"
	StateClosed  = "closed"
	StateMerged  = "merged"
)

// Ended reports a category nothing more happens to, unless it reopens.
func Ended(category string) bool {
	return category == StateDone || category == StateClosed || category == StateMerged
}

// Roles of a link.
const (
	// RoleOrigin: the workspace was started from the item.
	RoleOrigin = "origin"
	// RoleDelivers: work the workspace produced (a pull request), tracked.
	RoleDelivers = "delivers"
	// RoleMentions: named in a conversation; listed, never tracked or notified.
	RoleMentions = "mentions"
)

// Sources of a link: how Lazybox learned of it.
const (
	SourceStartedFrom = "started-from"
	SourceBranch      = "branch"
	SourceAgentOutput = "agent-output"
	SourceCLI         = "cli"
	SourcePerson      = "person"
)

// Check is one CI signal on a pull request's head.
type Check struct {
	Name       string `json:"name"`
	Status     string `json:"status,omitempty"`
	Conclusion string `json:"conclusion,omitempty"`
	URL        string `json:"url,omitempty"`
	Summary    string `json:"summary,omitempty"`
	// Source is "run" (a check run) or "status" (a commit status): the
	// provider re-reads them separately.
	Source string `json:"source,omitempty"`
	// ID is a check run's id: a GitHub Actions job's, when Actions ran it.
	ID int64 `json:"id,omitempty"`
}

// Failing is a completed check whose result needs a code or CI change.
func (c Check) Failing() bool {
	switch c.Conclusion {
	case "failure", "timed_out", "action_required", "startup_failure":
		return true
	}
	return false
}

// PullRequest is what a code review item adds.
type PullRequest struct {
	Head    string `json:"head"`
	Base    string `json:"base,omitempty"`
	HeadSHA string `json:"headSha,omitempty"`
	Draft   bool   `json:"draft,omitempty"`
	// Checks is success, failure, pending or none.
	Checks string `json:"checks,omitempty"`
	// Review is approved, changes_requested, review_required or empty.
	Review         string  `json:"review,omitempty"`
	Mergeable      bool    `json:"mergeable,omitempty"`
	MergeableState string  `json:"mergeableState,omitempty"`
	CheckRuns      []Check `json:"checkRuns,omitempty"`
	MergeCommit    string  `json:"mergeCommit,omitempty"`
}

// Failing is the head's failing checks, in the provider's order.
func (p *PullRequest) Failing() []Check {
	if p == nil {
		return nil
	}
	var out []Check
	for _, c := range p.CheckRuns {
		if c.Failing() {
			out = append(out, c)
		}
	}
	return out
}

// FailingNames is the failing checks' names.
func (p *PullRequest) FailingNames() []string {
	var out []string
	for _, c := range p.Failing() {
		out = append(out, c.Name)
	}
	return out
}

// Item is the snapshot Lazybox keeps of an item.
type Item struct {
	Ref        Ref    `json:"ref"`
	Kind       string `json:"kind"`
	Title      string `json:"title,omitempty"`
	URL        string `json:"url,omitempty"`
	Repository string `json:"repository,omitempty"`
	Number     int64  `json:"number,omitempty"`
	Author     string `json:"author,omitempty"`
	// State is the category; Label the provider's own words.
	State string `json:"state"`
	Label string `json:"label,omitempty"`
	// RawState is the provider's state (open, closed, merged).
	RawState   string       `json:"rawState,omitempty"`
	UpdatedAt  time.Time    `json:"updatedAt,omitzero"`
	ClosedAt   time.Time    `json:"closedAt,omitzero"`
	ObservedAt time.Time    `json:"observedAt,omitzero"`
	PR         *PullRequest `json:"pullRequest,omitempty"`
}

// Link ties an item to a workspace.
type Link struct {
	Ref       Ref    `json:"ref"`
	Workspace string `json:"workspace"`
	// WorkspaceName is kept for history once the workspace is deleted.
	WorkspaceName string `json:"workspaceName,omitempty"`
	Deleted       bool   `json:"deleted,omitempty"`
	// Session is the execution that linked it; Conversation its conversation.
	Session      string `json:"session,omitempty"`
	Conversation string `json:"conversation,omitempty"`
	// Member is the workspace repository whose origin is the item's repository.
	Member string    `json:"member,omitempty"`
	Role   string    `json:"role"`
	Source string    `json:"source"`
	At     time.Time `json:"at"`
}

// Branch is a branch a workspace repository has been on.
type Branch struct {
	Workspace  string `json:"workspace"`
	Member     string `json:"member"`
	Repository string `json:"repository"`
	Name       string `json:"name"`
	// Pushed: the branch was known to be pushed when last seen (a
	// remote-tracking ref, or the upstream `git push -u` records). A pushed
	// branch is asked about at once and at the open interval. One not known
	// to be pushed is still asked about, less often: from a source whose
	// origin has no fetch refspec (repositories/fetchspec.go), an agent's
	// push without -u leaves nothing locally.
	Pushed    bool      `json:"pushed,omitempty"`
	FirstSeen time.Time `json:"firstSeen"`
	LastSeen  time.Time `json:"lastSeen"`
	// CheckedAt is when the provider was last asked for its pull requests;
	// ETag the validator of that answer.
	CheckedAt time.Time `json:"checkedAt,omitzero"`
	ETag      string    `json:"etag,omitempty"`
	// Failures counts lookups in a row that failed (the next backs off);
	// LastError says why the last one did, for ListLinks.
	Failures  int    `json:"failures,omitempty"`
	LastError string `json:"lastError,omitempty"`
}

func (b Branch) key() string {
	return b.Workspace + "\x00" + b.Member + "\x00" + b.Repository + "\x00" + b.Name
}

// Event is a transition of a tracked item, emitted once.
type Event struct {
	ID           string    `json:"id"`
	Name         string    `json:"name"`
	Ref          Ref       `json:"ref"`
	Workspace    string    `json:"workspace,omitempty"`
	Session      string    `json:"session,omitempty"`
	Conversation string    `json:"conversation,omitempty"`
	At           time.Time `json:"at"`
	Reason       string    `json:"reason,omitempty"`
	HeadSHA      string    `json:"headSha,omitempty"`
	Title        string    `json:"title,omitempty"`
	URL          string    `json:"url,omitempty"`
	Repository   string    `json:"repository,omitempty"`
	Number       int64     `json:"number,omitempty"`
	Failed       []string  `json:"failed,omitempty"`
}

// Key deduplicates an event: one transition, one hook run.
func (e Event) Key() string {
	return e.Name + "\x00" + e.Ref.ID() + "\x00" + e.HeadSHA + "\x00" + e.Reason
}

func trimTitle(title string) string {
	title = strings.Join(strings.Fields(title), " ")
	if len(title) > 300 {
		title = title[:299] + "…"
	}
	return title
}
