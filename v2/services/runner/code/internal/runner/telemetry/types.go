// Package telemetry owns observations and accounting, independently of windows,
// provider processes and account credentials. Unknown is never represented by zero.
package telemetry

import (
	"context"
	"time"
)

type Scope struct {
	Workspace    string `json:"workspace"`
	Conversation string `json:"conversation"`
	Execution    string `json:"execution"`
	Profile      string `json:"profile"`
	Provider     string `json:"provider"`
	Attempt      string `json:"attempt"`
}
type Tokens struct {
	Input        *int64 `json:"input,omitempty"`  // Uncached input; disjoint from cache buckets.
	Output       *int64 `json:"output,omitempty"` // Includes reasoning, never added twice.
	CacheRead    *int64 `json:"cacheRead,omitempty"`
	CacheWrite5m *int64 `json:"cacheWrite5m,omitempty"`
	CacheWrite1h *int64 `json:"cacheWrite1h,omitempty"`
}
type Record struct {
	ID           string    `json:"id"`
	Scope        Scope     `json:"scope"`
	Agent        string    `json:"agent"` // Empty means child attribution unavailable.
	Parent       string    `json:"parent,omitempty"`
	Model        string    `json:"model"`
	Source       string    `json:"source"` // gateway or sdk
	ResponseID   string    `json:"responseId,omitempty"`
	Status       string    `json:"status"` // pending, complete, partial, failed, interrupted
	Tokens       Tokens    `json:"tokens"`
	Nanos        *int64    `json:"nanos,omitempty"` // API-equivalent token cost, never an invoice.
	PriceVersion string    `json:"priceVersion,omitempty"`
	Reason       string    `json:"reason,omitempty"`
	Tier         string    `json:"tier,omitempty"`
	ObservedAt   time.Time `json:"observedAt"`
}
type ContextDetail struct {
	Name   string `json:"name"`
	Tokens *int64 `json:"tokens,omitempty"`
}
type Category struct {
	Details  []ContextDetail `json:"details,omitempty"`
	Kind     string          `json:"kind"`
	Name     string          `json:"name"`
	Tokens   *int64          `json:"tokens,omitempty"`
	Included bool            `json:"included"`
}
type ContextSnapshot struct {
	Scope      Scope      `json:"scope"`
	Agent      string     `json:"agent"`
	Model      string     `json:"model"`
	Used       *int64     `json:"used,omitempty"`
	Capacity   *int64     `json:"capacity,omitempty"`
	Reserve    *int64     `json:"reserve,omitempty"`
	Source     string     `json:"source"`
	Categories []Category `json:"categories,omitempty"`
	ObservedAt time.Time  `json:"observedAt"`
}
type Query struct {
	Workspace, Execution, Conversation string
	Prices                             Pricer
}
type Group struct {
	Scope      Scope  `json:"scope"`
	Agent      string `json:"agent"`
	Model      string `json:"model"`
	Source     string `json:"source"`
	Tokens     Tokens `json:"tokens"`
	Nanos      *int64 `json:"nanos,omitempty"`
	Records    int64  `json:"records"`
	Unpriced   int64  `json:"unpriced"`
	Incomplete int64  `json:"incomplete"`
}
type Report struct {
	Groups   []Group           `json:"groups"`
	Contexts []ContextSnapshot `json:"contexts"`
	Recent   []Record          `json:"recent"`
}

// Store must acknowledge observations durably. Repeated IDs replace snapshots;
// they do not append cumulative counters as new billable requests.
type Store interface {
	Put(context.Context, Record) error
	PutCumulative(context.Context, Record, bool) error
	PutContext(context.Context, ContextSnapshot) error
	Report(context.Context, Query) (Report, error)
	LatestModel(context.Context, string) (string, error)
	Close() error
}
type Pricer interface{ Price(Record) Record }

func number(v int64) *int64 { return &v }
