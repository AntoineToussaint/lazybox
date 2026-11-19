// Package sessionstore owns durable conversation identities and execution attempts.
// It does not launch providers, resolve credentials, or own terminal processes.
package sessionstore

import (
	"context"
	"encoding/json"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
)

// Record contains references to credentials, never resolved environment values.
// Previous links immutable attempts within one stable ConversationID.
type Record struct {
	ForkedFrom                                                       string `json:",omitempty"`
	ForkCursor                                                       uint64 `json:",omitempty"`
	DisplayTitle                                                     string // Resolved conversation title; projections derive this from the root attempt or explicit rename.
	ProfileBindings                                                  map[string]string
	ID, ConversationID, Previous, WorkID, CollectionID, Cwd, Purpose string
	ProfileID, Provider, ProfileBinding                              string
	Context                                                          string
	Options                                                          accounts.Options
	GatewayID                                                        string
	Title                                                            string
	UserTitle                                                        string
	// GeneratedTitle is the title generated after the conversation's first
	// turn (titles, agent.Titler); UserTitle wins over it. Resolved on read.
	GeneratedTitle string `json:",omitempty"`
	// Opening is the conversation's first message (bounded), resolved on read
	// from its first attempt.
	Opening              string `json:",omitempty"`
	CreatedAt, UpdatedAt time.Time
	Snapshot             engine.Snapshot
	ProviderSettings     json.RawMessage
	ResumeBlocked        string
	// Instructions is the last whole shared-instructions update delivered to
	// this conversation by a Runner from before Briefing; Context holds what
	// it started with. Both are read only to place such a conversation.
	Instructions string `json:",omitempty"`
	// Briefing is the version of each briefing section this conversation was
	// given, by section ID (runner/briefing.go): at its start, then with each
	// update. Nil in a record from before Lazybox kept it.
	Briefing map[string]string `json:",omitempty"`
	// BrowserTools: the agent was started with the agent browser's tools.
	// Nil in a record from before Lazybox kept it.
	BrowserTools *bool `json:",omitempty"`
	// PinnedAt and ArchivedAt are the conversation's marks in Unix
	// milliseconds, 0 when unset. Like DisplayTitle they are projected from
	// the conversation on read, never stored in an attempt.
	PinnedAt   int64 `json:",omitempty"`
	ArchivedAt int64 `json:",omitempty"`
}

// Store commits the identity before provider startup and the observed state
// before acknowledging capture. Implementations enforce one successor per attempt.
type Query struct {
	Limit, Offset int
	Workspace     *string
	Search        string
}

type Store interface {
	Identity(context.Context) (string, error)
	Query(context.Context, Query) ([]Record, error)
	Rename(context.Context, string, string) error
	// SetGeneratedTitle records a generated title; a rename still wins.
	SetGeneratedTitle(context.Context, string, string) error
	// Pin and Archive set or clear a conversation's mark, by any attempt's ID.
	Pin(context.Context, string, bool) error
	Archive(context.Context, string, bool) error
	Create(context.Context, Record) error
	SaveState(context.Context, string, engine.Snapshot, json.RawMessage) error
	// Amend changes one attempt's record atomically.
	Amend(context.Context, string, func(*Record)) error
	Get(context.Context, string) (Record, error)
	List(context.Context, int, int) ([]Record, error)
	Successor(context.Context, string) (Record, error)
	Delete(context.Context, string) ([]string, error)
	Deleted(context.Context, string) ([]string, error)
	Close() error
}
