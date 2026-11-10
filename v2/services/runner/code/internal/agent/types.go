package agent

import (
	"context"
	"encoding/json"
	"errors"
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
}

// Open contains resolved execution configuration. Env is private child-process
// state: it must never be written to event journals or returned by transport.
type Open struct {
	ID, Cwd, Model, Resume, Profile, Permission, Workflow string
	Context                                               string
	Purpose                                               string
	Isolated                                              bool
	ProviderSettings                                      json.RawMessage
	Env                                                   []string
	Gateway                                               *Gateway
	Tools                                                 func(context.Context, string, json.RawMessage) (string, error)
	// Host asks a native adapter to run its process under a detached session
	// host so it survives the Runner. Nil keeps a Runner-owned local pipe.
	Host *Hosting
}

// Hosting selects session hosts under Root. A non-nil Attach re-attaches to
// the native process an earlier Runner left running instead of starting one.
type Hosting struct {
	Root   string
	Attach *Attachment
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
	// Responded holds request IDs the user already answered. A redelivered
	// request in this set may have had effects whose outcome is now unknown.
	Responded map[string]bool
	// Transcript is the conversation as the journal recorded it, oldest first:
	// each user message the agent was given or may have been given (accepted
	// and no longer queued) as "\nUSER: <prompt>\n", followed by the root
	// agent's reply text. A message is placed where it was accepted, which can
	// precede the end of the reply it was queued behind. A layer that hands the
	// conversation to a new native session rebuilds its excerpt from it.
	Transcript string
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
)

type Operation struct {
	ID        string              `json:"id"`
	Kind      Kind                `json:"kind"`
	Text      string              `json:"text,omitempty"`
	Images    []Image             `json:"images,omitempty"`
	Files     []File              `json:"files,omitempty"`
	RequestID string              `json:"requestId,omitempty"`
	Decision  string              `json:"decision,omitempty"`
	Answers   map[string][]string `json:"answers,omitempty"`
}

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
}

// Prompt gives CLI agents provider-neutral references to generic files.
// Images remain separate native image inputs for vision.
func Prompt(op Operation) string {
	if len(op.Files) == 0 {
		return op.Text
	}
	data, _ := json.Marshal(op.Files)
	return op.Text + "\n\nAttached files available on this Runner (read as needed):\n" + string(data)
}

type Event struct {
	Type          string              `json:"type"`
	SessionID     string              `json:"sessionId,omitempty"`
	TurnID        string              `json:"turnId,omitempty"`
	RequestID     string              `json:"requestId,omitempty"`
	OperationID   string              `json:"operationId,omitempty"`
	Text          string              `json:"text,omitempty"`
	Status        string              `json:"status,omitempty"`
	Data          json.RawMessage     `json:"data,omitempty"`
	AgentID       string              `json:"agentId,omitempty"`
	ParentAgentID string              `json:"parentAgentId,omitempty"`
	Name          string              `json:"name,omitempty"`
	AgentKind     string              `json:"agentKind,omitempty"`
	NativeID      string              `json:"nativeId,omitempty"`
	Account       *AccountObservation `json:"account,omitempty"`
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
