package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"strings"
	"unicode"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

var ErrUnsupported = errors.New("operation unsupported by adapter")

type Capabilities struct {
	Images           bool   `json:"images"`
	Files            bool   `json:"files"`
	Questions        bool   `json:"questions"`
	Approvals        bool   `json:"approvals"`
	Steer            bool   `json:"steer"`
	Resume           bool   `json:"resume"`
	PendingResume    bool   `json:"pendingResume"`
	ProfileIsolation string `json:"profileIsolation"`
	Subagents        bool   `json:"subagents"`
	// LivePermission: the permission mode can change while the session runs.
	LivePermission bool `json:"livePermission,omitempty"`
	// StopAgent: a single sub-agent can be stopped while the main agent continues.
	StopAgent bool `json:"stopAgent,omitempty"`
	// LiveModel: the model can change while the session runs, for its
	// following turns.
	LiveModel bool `json:"liveModel,omitempty"`
}

// Open contains resolved execution configuration. Env is private child-process
// state: it must never be written to event journals or returned by transport.
type Open struct {
	ForkThread, ForkAfterTurn                             string
	ID, Cwd, Model, Resume, Profile, Permission, Workflow string
	Effort, Context                                       string
	Purpose                                               string
	Isolated                                              bool
	ProviderSettings                                      json.RawMessage
	Env                                                   []string
	Gateway                                               *Gateway
	Tools                                                 func(context.Context, string, json.RawMessage) (string, error)
	// Browser offers the agent browser's tools (BrowserTools) through Tools.
	Browser bool
	// ClaudeAIConnectors: a Claude session may load the person's claude.ai
	// connectors (cloud MCP servers). False turns them off for the session.
	ClaudeAIConnectors bool
	// Host asks a native adapter to run its process under a detached session
	// host so it survives the Runner. Nil keeps a Runner-owned local pipe.
	Host *Hosting
	// Grants are the conversation's "always allow" choices so far: a new
	// process of the provider starts holding them (Grant).
	Grants []Grant
	// ForkAfterMessage stands in for ForkAfterTurn when the turn was recorded
	// without a native id: the provider's id of the turn's last message, for
	// an adapter that can find where that turn ended in the native history.
	ForkAfterMessage string
}

// Hosting selects session hosts under Root. A non-nil Attach re-attaches to
// the native process an earlier Runner left running instead of starting one.
type Hosting struct {
	Root   string
	Attach *Attachment
	// Backend starts new hosts; nil selects the default session host. A host
	// is re-attached the same way whichever backend started it.
	Backend sessionhost.Backend
}

// HostOptions names a new agent host for the Runner's process inventory and
// its tmux window: the execution it belongs to, and provider · directory.
func (o Open) HostOptions(provider string) sessionhost.Options {
	label := provider
	if o.Cwd != "" {
		label += " · " + filepath.Base(o.Cwd)
	}
	return sessionhost.Options{Kind: "agent", Owner: o.ID, Label: label}
}

// Hosted is a session whose native process runs in a session host. Wrapping
// sessions forward it, so the Runner can say which host runs a conversation.
type Hosted interface{ HostID() string }

// Titler is a session that can name its conversation through its provider
// (the cheapest way the provider offers). description is the conversation's
// first message and a bounded excerpt of the reply (titles.Description); the
// answer is raw provider text, tidied by the caller. Wrapping sessions forward it.
type Titler interface {
	Title(ctx context.Context, description string) (string, error)
}

// TitleOf asks s to name its conversation; ErrUnsupported when it cannot.
func TitleOf(ctx context.Context, s Session, description string) (string, error) {
	if t, ok := s.(Titler); ok {
		return t.Title(ctx, description)
	}
	return "", ErrUnsupported
}

// HostOf returns the session host running s's native process, or "".
func HostOf(s Session) string {
	if h, ok := s.(Hosted); ok {
		return h.HostID()
	}
	return ""
}

// Attachment is everything a session layer needs to resume supervising a live
// native process. Each wrapping adapter unwraps its own State and passes the
// inner state down.
type Attachment struct {
	// State is this layer's DurableState as last recorded.
	State json.RawMessage
	// Reference is the last recorded native conversation reference.
	Reference string
	// Cursor is the highest transport cursor durably recorded in the journal.
	Cursor uint64
	// Responded holds the user's recorded response (Kind, Decision, Answers)
	// to each request answered before the restart. The host redelivers such a
	// request only if the reply never reached the agent, so an adapter can
	// answer it again, except where acting on it may already have had effects.
	// A response recorded before decisions were journaled has an empty Decision.
	Responded map[string]Operation
	// Transcript is the conversation as the journal recorded it, oldest first:
	// each user message the agent was given or may have been given (accepted
	// and no longer queued) as "\nUSER: <prompt>\n", followed by the root
	// agent's reply text. A message is placed where it was accepted, which can
	// precede the end of the reply it was queued behind. A layer that hands the
	// conversation to a new native session rebuilds its excerpt from it.
	Transcript string
	// Replayed counts, by ReplayKey, the events of a line the previous Runner
	// recorded only in part. They are in the journal and the Transcript
	// already; the host replays the whole line, and these must not count twice.
	Replayed map[string]int
}

// ReplayKey identifies an event derived from a native line by what the
// provider said, not by its position among the line's events, which can
// differ when the line is processed again by a fresh adapter.
func ReplayKey(ev Event) string {
	return fmt.Sprintf("%d\x00%s\x00%s\x00%s\x00%s\x00%s\x00%s\x00%s", ev.Cursor, ev.Type, ev.RequestID, ev.Status, ev.Name, ev.Text, ev.TurnID, ev.NativeID)
}

// Durable sessions can be re-attached by a later Runner. The state is opaque,
// private and small; it is recorded after every event.
type Durable interface{ DurableState() json.RawMessage }

// Detacher sessions can leave their native process running for a later Runner.
type Detacher interface{ Detach() error }

// Committer sessions release transport buffering once an event is durably
// recorded. Commit is cumulative: it covers every earlier event.
type Committer interface{ Commit(Event) }

type Gateway struct {
	ID       string `json:"id"`
	URL      string `json:"url"`
	TokenEnv string `json:"tokenEnv,omitempty"`
}

type Image struct {
	MediaType string `json:"mediaType"`
	Data      string `json:"data"`
}

type Kind string

const (
	Message   Kind = "message"
	Steer     Kind = "steer"
	Interrupt Kind = "interrupt"
	Answer    Kind = "answer"
	Approve   Kind = "approve"
	// Permission changes the running session's permission mode.
	Permission Kind = "permission"
	// StopAgent stops one running sub-agent; the main agent's turn continues.
	StopAgent Kind = "stop_agent"
	// Model switches the running session's model for its following turns.
	Model               Kind = "model"
	ListMcpServers      Kind = "list_mcp_servers"
	CompactContext      Kind = "compact_context"
	ListProviderCatalog Kind = "list_provider_catalog"
	// RevokeGrant takes back one "always allow" of the conversation (Grant).
	RevokeGrant Kind = "revoke_grant"
)

// PermissionModes are the modes a running session can switch to. Inherit
// (the provider's own settings) is only a starting choice.
var PermissionModes = map[string]bool{"ask": true, "read-only": true, "yolo": true}

// ModelChoice is one model a provider offers, as the model picker lists it:
// the id the provider takes (an alias or a full id), its name as people say
// it and a short description. The JSON is a command_catalog event's `models`.
type ModelChoice struct {
	Value        string   `json:"value"`
	DisplayName  string   `json:"displayName"`
	Description  string   `json:"description,omitempty"`
	EffortLevels []string `json:"effortLevels,omitempty"`
}

// ValidModel checks a model identifier a person chose for a running session:
// one token the provider resolves (an alias or a full id), never free text.
func ValidModel(model string) error {
	if model == "" {
		return fmt.Errorf("%w: model required", ErrInvalidModel)
	}
	if len(model) > 256 {
		return fmt.Errorf("%w: model name exceeds 256 bytes", ErrInvalidModel)
	}
	for _, r := range model {
		if unicode.IsSpace(r) || unicode.IsControl(r) {
			return fmt.Errorf("%w: model name must be one word, an alias or a model id", ErrInvalidModel)
		}
	}
	return nil
}

// ErrInvalidModel marks a model name ValidModel refuses.
var ErrInvalidModel = errors.New("invalid model")

// ValidEffort validates an optional override for providers with reasoning effort.
func ValidEffort(provider, effort string) error {
	if effort == "" {
		return nil
	}
	switch provider {
	case "claude":
		switch effort {
		case "low", "medium", "high", "xhigh", "max":
			return nil
		}
	case "codex":
		switch effort {
		case "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra":
			return nil
		}
	}
	return fmt.Errorf("%w: unsupported reasoning effort %q for %s", ErrInvalidModel, effort, provider)
}

type Operation struct {
	// QueueEditID holds a queued message until its editor saves or cancels.
	QueueEditID string  `json:"queueEditId,omitempty"`
	ID          string  `json:"id"`
	Kind        Kind    `json:"kind"`
	Text        string  `json:"text,omitempty"`
	Images      []Image `json:"images,omitempty"`
	Files       []File  `json:"files,omitempty"`
	RequestID   string  `json:"requestId,omitempty"`
	Decision    string  `json:"decision,omitempty"`
	// Always, with an accepting Decision, names one of the "always allow"
	// choices the request offered (its data's `always` list): the provider's
	// own way to stop asking within that scope. Empty allows once.
	Always string `json:"always,omitempty"`
	// Reason, with a declining Decision, is the user's why; the agent is told.
	Reason  string              `json:"reason,omitempty"`
	Answers map[string][]string `json:"answers,omitempty"`
	// Context is an update to the session's instructions, delivered with this
	// message but kept out of the user's text and transcript.
	Context string `json:"context,omitempty"`
	// Permission is the new mode of a Permission operation.
	Permission string `json:"permission,omitempty"`
	// AgentID is the Lazybox agent a StopAgent operation stops; NativeID is
	// the provider's id for it, resolved by the engine from its agent state.
	AgentID  string `json:"agentId,omitempty"`
	NativeID string `json:"nativeId,omitempty"`
	// Model is the new model of a Model operation.
	Model  string `json:"model,omitempty"`
	Effort string `json:"effort,omitempty"`
	// Section is the inventory requested by ListProviderCatalog.
	Section string `json:"section,omitempty"`
	// Origin, on a Message, marks one Lazybox wrote rather than the person.
	Origin *Origin `json:"origin,omitempty"`
	// GrantID is the grant a RevokeGrant operation takes back.
	GrantID string `json:"grantId,omitempty"`
}

// Origin marks a message Lazybox wrote, not the person: the answer to a
// repository request, a review's findings, a question between two agents, a
// mission's brief. The agent receives the message's text unchanged; clients
// show the message as an event of the conversation, in Summary's words, and
// never as the person's. Kind is one of the Origin* constants.
type Origin struct {
	Kind string `json:"kind"`
	// Summary is the event in one line: "Added authority to this workspace".
	Summary string `json:"summary"`
	// Detail is a short label beside it: a branch, "3 findings".
	Detail string `json:"detail,omitempty"`
	// Copy is what the event offers to copy, and shows on hover: a path.
	Copy string `json:"copy,omitempty"`
	// From is the agent the message speaks for ("Codex"); empty is Lazybox.
	From string `json:"from,omitempty"`
	// Conversation and Session name From's conversation when it is another
	// agent of this one's tree (an orchestrator and the agents it started),
	// so clients can open it.
	Conversation string `json:"conversation,omitempty"`
	Session      string `json:"session,omitempty"`
}

// Bounded is o as journaled: Summary and Detail on one line each, at most
// originLine runes, so a long answer quoted in a summary stays a line.
func (o *Origin) Bounded() *Origin {
	if o == nil {
		return nil
	}
	line := func(s string) string {
		s = strings.Join(strings.Fields(s), " ")
		if r := []rune(s); len(r) > originLine {
			s = string(r[:originLine-1]) + "…"
		}
		return s
	}
	out := *o
	out.Summary, out.Detail = line(o.Summary), line(o.Detail)
	return &out
}

const originLine = 280

// Kinds of Origin.
const (
	OriginRepositoryAdded    = "repository_added"
	OriginRepositoryDeclined = "repository_declined"
	OriginReviewBrief        = "review_brief"
	OriginReviewFindings     = "review_findings"
	OriginReviewRound        = "review_round"
	OriginAgentAsk           = "agent_ask"
	OriginAgentAnswer        = "agent_answer"
	OriginMissionBrief       = "mission_brief"
	// An orchestrator's brief to an agent it started, a message between an
	// orchestrator and its agent, and an agent's report to its orchestrator
	// (runner/orchestrator.go).
	OriginAgentBrief  = "agent_brief"
	OriginAgentNote   = "agent_note"
	OriginAgentReport = "agent_report"
	// Lazybox to an orchestrator: two of its agents changed the same files.
	OriginOverlap      = "overlap_detected"
	OriginContinuation = "continuation"
	// A conversation another one asked the person to start: its brief; and,
	// to the asking agent, the person's answer (runner/conversation_starts.go).
	OriginConversationBrief    = "conversation_brief"
	OriginConversationStarted  = "conversation_started"
	OriginConversationDeclined = "conversation_declined"
)

// Delivery is "acknowledged" only if the provider actually replied. A pipe
// write, an SDK iterator submission, and a completed turn are different facts.
type Receipt struct {
	ID         string `json:"id"`
	Delivery   string `json:"delivery"`
	ProviderID string `json:"providerId,omitempty"`
	Error      string `json:"error,omitempty"`
}

// File refers to bytes retained by the Runner, never a client-supplied path.
type File struct {
	Name      string `json:"name"`
	MediaType string `json:"mediaType"`
	Path      string `json:"path"`
	Size      int64  `json:"size"`
	// Delivery is how the file reached the agent (DeliverImage, DeliverText,
	// DeliverFile), recorded with the message so every client can say so.
	// Empty on a message an older Runner accepted.
	Delivery string `json:"delivery,omitempty"`
}

// Prompt is a message as text, without reading any file: the person's text,
// then a reference to each attached file. A replayed transcript (a portable
// checkpoint) shows what was sent this way.
func Prompt(op Operation) string {
	out := []string{}
	if op.Text != "" {
		out = append(out, op.Text)
	}
	for _, f := range op.Files {
		out = append(out, Reference(f))
	}
	return strings.Join(out, "\n\n")
}

type Event struct {
	TerminalAgentInput bool                `json:"terminalAgentInput,omitempty"`
	Type               string              `json:"type"`
	SessionID          string              `json:"sessionId,omitempty"`
	TurnID             string              `json:"turnId,omitempty"`
	RequestID          string              `json:"requestId,omitempty"`
	OperationID        string              `json:"operationId,omitempty"`
	Text               string              `json:"text,omitempty"`
	Status             string              `json:"status,omitempty"`
	Data               json.RawMessage     `json:"data,omitempty"`
	AgentID            string              `json:"agentId,omitempty"`
	ParentAgentID      string              `json:"parentAgentId,omitempty"`
	Name               string              `json:"name,omitempty"`
	AgentKind          string              `json:"agentKind,omitempty"`
	NativeID           string              `json:"nativeId,omitempty"`
	Account            *AccountObservation `json:"account,omitempty"`
	// Cursor is the session host transport position of the native line this
	// event came from. Recorded in the journal so a re-attaching Runner knows
	// exactly which lines it already holds.
	Cursor uint64 `json:"transportCursor,omitempty"`
	// Partial marks an event that more events from the same native line
	// follow. A line is acknowledged to its host only with its last event, so a
	// Runner that dies between them gets the whole line again.
	Partial bool `json:"transportPartial,omitempty"`
	// Attempt is the supervising layer's native attempt; never serialized.
	Attempt uint64 `json:"-"`
}

// UnreadableEvent records a provider line an adapter skipped because it could
// not read it. One odd line never ends supervision of a live agent: the
// adapter goes on, and this record (journal only, not shown in the
// conversation) says what was skipped.
const UnreadableEvent = "unreadable_event"

type Adapter interface {
	Open(context.Context, Open) (Session, error)
}
type Session interface {
	Reference() string
	Capabilities() Capabilities
	Apply(context.Context, Operation) (Receipt, error)
	Receive(context.Context) (Event, error)
	Close() error
}

// RuntimeSettings preserves provider-resolved policies across a same-provider handoff.
// The payload is private and opaque to the generic supervisor.
type RuntimeSettings interface{ SessionSettings() json.RawMessage }

// ProfileSwitcher sessions move to another account profile of the same
// provider when the person asks, keeping the execution (the subscription
// rotation layer implements it).
type ProfileSwitcher interface {
	// SwitchProfile continues on profile id. continueTurn says the latest
	// request is unfinished (it failed at a limit, or is running) and must be
	// continued now; otherwise the checkpoint travels with the next message.
	// The current profile retries in place instead.
	SwitchProfile(id string, continueTurn bool) error
	// SetRotation replaces the pool switched to automatically at a limit.
	SetRotation(ids []string) error
}

// ErrProcessExited marks a provider stream that ended because the native
// process exited (its exit was observed: a session host's exit report, or a
// local child reaped), as opposed to the Runner losing its connection to a
// process that may still run. Adapters wrap it; test with errors.Is.
var ErrProcessExited = errors.New("native process exited")

// ErrRejected marks a request the provider answered without acting on it: a
// JSON-RPC error reply, a refusal the adapter decided before writing anything,
// or an answer that names no turn it started. Its outcome is known, nothing
// is in flight, so a message it refused is a turn's failure
// (DeliveryRejectedFault) the next message continues, never a fault to
// reconcile. A delivery whose outcome is unknown (the write or the connection
// failed) is not this. Adapters wrap it (Rejected); test with errors.Is.
var ErrRejected = errors.New("the provider refused the request")

// Rejected marks err as the provider's definite refusal (ErrRejected).
func Rejected(err error) error { return rejected{err} }

type rejected struct{ error }

func (r rejected) Is(target error) bool { return target == ErrRejected }
func (r rejected) Unwrap() error        { return r.error }

// ErrTurnRunning marks a message the adapter did not send because the agent
// is still in a turn the engine did not know of (one a previous Runner
// started). Nothing was sent: the engine keeps the message first in line,
// counts the conversation busy, and delivers it when that turn ends.
var ErrTurnRunning = errors.New("the agent is still in a turn")

// DeliveryRejectedFault is the fault of a conversation whose message the
// provider refused (ErrRejected): a turn fault, so the next message
// continues the conversation.
const DeliveryRejectedFault = "delivery_rejected"

// ProcessExitFault is the fault of a conversation whose native process
// exited. It is a Runner fault, neither a turn fault (no process takes the
// next message) nor a TransportFault: re-attaching cannot bring the process
// back, so neither re-adoption nor a later event settles it. Resume continues
// the provider's conversation in a new process instead.
const ProcessExitFault = "provider_process_exit"

// TurnFault reports whether a fault came from the provider ending one turn with
// an error (an API error, an image it could not read, an overloaded service),
// as opposed to the Runner losing track of the provider. After a turn fault
// the conversation is intact: the person's next message continues it.
//
// A provider process that ended (ProcessExitFault) or a worker whose SDK
// stream failed (sdk_error, after which the worker exits) is not a turn
// fault: nothing is left to take the next message.
func TurnFault(fault string) bool {
	if fault == "" || LimitFault(fault) {
		return false
	}
	switch fault {
	case ProcessExitFault, "sdk_error":
		return false
	}
	for _, runner := range []string{"provider delivery failed", "event capture failed", "provider stream ended", "invalid or excessive pending provider requests", "native_exit_unconfirmed", "provider startup failed"} {
		if strings.HasPrefix(fault, runner) {
			return false
		}
	}
	return true
}

// LimitFault reports whether a session fault is an account limit a profile
// switch (or a reset) resolves, rather than a fault to reconcile.
func LimitFault(fault string) bool {
	return fault == "exhausted" || fault == "throttled" || fault == "auth_required" || fault == "billing_error"
}
