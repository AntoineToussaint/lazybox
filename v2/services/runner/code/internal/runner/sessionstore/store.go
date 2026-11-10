// Package sessionstore owns durable conversation identities and execution attempts.
// It does not launch providers, resolve credentials, or own terminal processes.
package sessionstore

import (
	"context"
	"encoding/json"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/engine"
)

// Record contains references to credentials, never resolved environment values.
// Previous links immutable attempts within one stable ConversationID.
type Record struct {
	DisplayTitle                                                     string // Resolved conversation title; projections derive this from the root attempt or explicit rename.
	ProfileBindings                                                  map[string]string
	ID, ConversationID, Previous, WorkID, CollectionID, Cwd, Purpose string
	ProfileID, Provider, ProfileBinding                              string
	Context                                                          string
	Options                                                          accounts.Options
	GatewayID                                                        string
	Title                                                            string
	UserTitle                                                        string
	CreatedAt, UpdatedAt                                             time.Time
	Snapshot                                                         engine.Snapshot
	ProviderSettings                                                 json.RawMessage
	ResumeBlocked                                                    string
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
	Create(context.Context, Record) error
	SaveState(context.Context, string, engine.Snapshot, json.RawMessage) error
	Get(context.Context, string) (Record, error)
	List(context.Context, int, int) ([]Record, error)
	Successor(context.Context, string) (Record, error)
	Delete(context.Context, string) ([]string, error)
	Deleted(context.Context, string) ([]string, error)
	Close() error
}
