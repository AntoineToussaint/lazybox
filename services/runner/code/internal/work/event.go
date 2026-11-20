package work

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"
)

// Event types. Each is versioned: Versions holds the version this Runner
// writes and understands. An event of an unknown type, or of a newer version,
// is kept in the log and shown raw; it never changes the derived board.
const (
	MissionCreated       = "mission.created"
	MissionProcessPinned = "mission.process_pinned"
	MissionBriefChanged  = "mission.brief_changed"
	MissionCanceled      = "mission.canceled"
	GoalCreated          = "goal.created"
	TaskCreated          = "task.created"
	TaskBriefChanged     = "task.brief_changed"
	TaskEdgeAdded        = "task.edge_added"
	TaskCanceled         = "task.canceled"
	TaskAssigned         = "task.assigned"
	AttemptStarted       = "attempt.started"
	AttemptEnded         = "attempt.ended"
	TaskReported         = "task.reported"
	EvidenceRecorded     = "evidence.recorded"
	GateRequested        = "gate.requested"
	GatePassed           = "gate.passed"
	GateRejected         = "gate.rejected"

	// The conversation family lives in the Runner-wide prompt log, not in a
	// mission's: a prompt belongs to a conversation, which may be no attempt.
	PromptSubmitted = "prompt.submitted"
	SnippetSent     = "snippet.sent"
)

// Versions is the version of each type this Runner writes and folds.
var Versions = map[string]int{
	CandidateReserved: 1, CandidateReady: 1, CandidateChecked: 1, CandidateAccepted: 1,
	MissionCreated: 1, MissionProcessPinned: 1, MissionBriefChanged: 1, MissionCanceled: 1,
	GoalCreated: 1, TaskCreated: 1, TaskBriefChanged: 1, TaskEdgeAdded: 1, TaskCanceled: 1,
	TaskAssigned: 1, AttemptStarted: 1, AttemptEnded: 1, TaskReported: 1, EvidenceRecorded: 1,
	GateRequested: 1, GatePassed: 1, GateRejected: 1,
	PromptSubmitted: 1, SnippetSent: 1,
	GrantIssued: 1, GrantRevoked: 1, MessageSent: 1,
	PlanProposed: 1, PlanApproved: 1, PlanRejected: 1, BudgetRaised: 1, BudgetWarning: 1, BudgetExhausted: 1,
	LeaseClaimed: 1, LeaseReleased: 1, OverlapDetected: 1, WIPHeld: 1,
}

// Known reports whether this Runner folds an event: a known type at a version
// it understands.
func Known(e Event) bool { v, ok := Versions[e.Type]; return ok && e.V >= 1 && e.V <= v }

// Actor kinds. Who did something is recorded, never inferred from text, and a
// human event is never attributed to an agent.
const (
	ActorHuman   = "human"
	ActorAgent   = "agent"
	ActorRule    = "rule"
	ActorCheck   = "check"
	ActorLazybox = "lazybox"
)

type Actor struct {
	Kind         string `json:"kind"`
	Role         string `json:"role,omitempty"`
	Conversation string `json:"conversation,omitempty"`
	Execution    string `json:"execution,omitempty"`
}

// Event is one entry of a mission's log, the only stored state of a mission.
// Seq is per mission and gap-free; ID is unique and sorts by time.
type Event struct {
	ID          string          `json:"id"`
	Seq         uint64          `json:"seq"`
	At          time.Time       `json:"at"`
	Mission     string          `json:"mission,omitempty"`
	Subject     string          `json:"subject"`
	Type        string          `json:"type"`
	V           int             `json:"v"`
	Actor       Actor           `json:"actor"`
	CausedBy    string          `json:"caused_by,omitempty"`
	Idempotency string          `json:"idempotency,omitempty"`
	Evidence    []string        `json:"evidence,omitempty"`
	Payload     json.RawMessage `json:"payload,omitempty"`
}

// Draft is an event before the log gives it an ID, a sequence and a time.
type Draft struct {
	Subject     string
	Type        string
	Actor       Actor
	CausedBy    string
	Idempotency string
	Evidence    []string
	Payload     any
}

// Payloads, one per type.
type (
	MissionPayload struct {
		Title string `json:"title,omitempty"`
		Brief string `json:"brief,omitempty"`
	}
	ProcessPayload struct {
		Preset   string `json:"preset"`
		Digest   string `json:"digest"`
		Autonomy int    `json:"autonomy"`
		// The gates the mission runs, pinned so a later release's preset
		// cannot change what an old mission's log means. Absent in logs
		// written before they were pinned: the preset is resolved then.
		ReportGate string `json:"report_gate,omitempty"`
		DoneGate   string `json:"done_gate,omitempty"`
	}
	CancelPayload struct {
		Task   string `json:"task,omitempty"`
		Reason string `json:"reason,omitempty"`
	}
	GoalPayload struct {
		Goal  string `json:"goal"`
		Title string `json:"title"`
	}
	TaskPayload struct {
		Task      string   `json:"task"`
		Goal      string   `json:"goal,omitempty"`
		Title     string   `json:"title,omitempty"`
		Brief     string   `json:"brief,omitempty"`
		BlockedBy []string `json:"blocked_by,omitempty"`
	}
	EdgePayload struct {
		Task string `json:"task"`
		// Kind is blocks (Task cannot start until After is done) or
		// merge_after (Task can build, lands after After).
		Kind  string `json:"kind"`
		After string `json:"after"`
	}
	Dispatch struct {
		ParentTitle   string `json:"parent_title"`
		ParentSession string `json:"parent_session"`
		Title         string `json:"title"`
		Brief         string `json:"brief"`
		Model         string `json:"model,omitempty"`
	}
	AttemptPayload struct {
		Dispatch     *Dispatch `json:"dispatch,omitempty"`
		Task         string    `json:"task"`
		Attempt      string    `json:"attempt"`
		Conversation string    `json:"conversation"`
		Workspace    string    `json:"workspace,omitempty"`
		Execution    string    `json:"execution,omitempty"`
		// New: the conversation was started for this task.
		New bool `json:"new,omitempty"`
		// Outcome, on attempt.ended: deleted, handoff or stopped.
		Outcome string `json:"outcome,omitempty"`
		// Lineage, on an assignment an orchestrator made (grant.go): the
		// conversation that started this one, under which grant, as what
		// role, at which permission and on which subscription.
		Parent     string `json:"parent,omitempty"`
		Grant      string `json:"grant,omitempty"`
		Role       string `json:"role,omitempty"`
		Permission string `json:"permission,omitempty"`
		Profile    string `json:"profile,omitempty"`
	}
	ReportPayload struct {
		Task    string `json:"task"`
		Summary string `json:"summary,omitempty"`
	}
	GatePayload struct {
		Gate string `json:"gate"`
		Task string `json:"task"`
		Note string `json:"note,omitempty"`
	}
)

// Evidence kinds and outcomes.
const (
	EvidenceRun    = "run"
	EvidenceCommit = "commit"
	EvidencePR     = "pr"
	// EvidenceReview is an agent review of the attempt's work
	// (review:<review>@<round>): its outcome fails while blockers are open.
	EvidenceReview = "review"

	OutcomePass    = "pass"
	OutcomeFail    = "fail"
	OutcomePending = "pending"
	OutcomeUnknown = "unknown"
)

// Evidence is a record Lazybox already keeps, referenced from a task and
// pinned to a revision so its freshness can be judged: a `lazybox run`
// (run:<execution>/<command>), a commit (commit:<member>@<sha>) or a pull
// request with its checks (pr:<owner/name>#<n>@<sha>).
type Evidence struct {
	// Corrects retracts a prior review pass without changing its ordering or attempt.
	Corrects       uint64 `json:"corrects,omitempty"`
	ContractDigest string `json:"contract_digest,omitempty"`
	Kind           string `json:"kind"`
	Ref            string `json:"ref"`
	Task           string `json:"task"`
	Attempt        string `json:"attempt,omitempty"`
	Title          string `json:"title,omitempty"`
	Outcome        string `json:"outcome"`
	// ExitCode is the command's own status; absent for a pipe (`lazybox log`).
	ExitCode *int `json:"exit_code,omitempty"`
	Piped    bool `json:"piped,omitempty"`
	// Revisions is each workspace repository's HEAD when a run finished.
	Revisions  map[string]string `json:"revisions,omitempty"`
	Workspace  string            `json:"workspace,omitempty"`
	Member     string            `json:"member,omitempty"`
	Repository string            `json:"repository,omitempty"`
	Execution  string            `json:"execution,omitempty"`
	Command    string            `json:"command,omitempty"`
	Number     int64             `json:"number,omitempty"`
	Head       string            `json:"head,omitempty"`
	State      string            `json:"state,omitempty"`
	URL        string            `json:"url,omitempty"`
	OutputPath string            `json:"output_path,omitempty"`
	Detail     string            `json:"detail,omitempty"`
	// Review: the open findings by severity when it was recorded.
	Blockers int `json:"blockers,omitempty"`
	Findings int `json:"findings,omitempty"`
}

// Subjects.
func TaskSubject(id string) string { return "task:" + id }
func GoalSubject(id string) string { return "goal:" + id }

const MissionSubject = "mission"

// Bounds on what a person or an agent can put in the log.
const (
	maxTitle    = 200
	maxBrief    = 4 << 10 // V1's four-line brief, at most 4 KB
	maxSummary  = 4 << 10
	maxNote     = 2 << 10
	maxIdentity = 128
)

// ErrRefused wraps every refusal: an event that does not fit the log is
// refused with the reason (like V1's TransitionRefused), never dropped.
var ErrRefused = errors.New("refused")

func refuse(format string, args ...any) error {
	return fmt.Errorf("%w: %s", ErrRefused, fmt.Sprintf(format, args...))
}

func cleanText(value string, limit int, what string) (string, error) {
	value = strings.TrimSpace(value)
	if len(value) > limit {
		return "", refuse("%s is longer than %d bytes", what, limit)
	}
	return value, nil
}

func decode[T any](e Event) (T, bool) {
	var v T
	if len(e.Payload) == 0 {
		return v, false
	}
	return v, json.Unmarshal(e.Payload, &v) == nil
}
