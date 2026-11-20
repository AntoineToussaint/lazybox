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
	ID            string    `json:"id"`
	Scope         Scope     `json:"scope"`
	Agent         string    `json:"agent"` // Empty means child attribution unavailable.
	Parent        string    `json:"parent,omitempty"`
	Model         string    `json:"model"`
	Source        string    `json:"source"` // gateway or sdk
	ResponseID    string    `json:"responseId,omitempty"`
	Status        string    `json:"status"` // pending, complete, partial, failed, interrupted
	Tokens        Tokens    `json:"tokens"`
	Nanos         *int64    `json:"nanos,omitempty"` // API-equivalent token cost, never an invoice.
	Costs         *Costs    `json:"costs,omitempty"` // Nanos by part; absent when unpriced.
	PriceVersion  string    `json:"priceVersion,omitempty"`
	Reason        string    `json:"reason,omitempty"`
	Tier          string    `json:"tier,omitempty"`
	ObservedAt    time.Time `json:"observedAt"`
	latestRequest *Record   // Candidate request, verified against the cumulative delta.
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

// CommandRun is what one `lazybox run` gave its agent against what the command
// printed: the evidence for judging output compression (COMMAND_OUTPUT.md).
type CommandRun struct {
	ID          string    `json:"id"`
	Scope       Scope     `json:"scope"` // Execution is required; the rest when known.
	Filter      string    `json:"filter"`
	ExitCode    int       `json:"exitCode"`
	OutputBytes uint64    `json:"outputBytes"`
	AgentBytes  uint64    `json:"agentBytes"`
	ObservedAt  time.Time `json:"observedAt"`
}
type Query struct {
	Workspace, Execution, Conversation string
	Prices                             Pricer
	// Series also reports usage hour by hour (Report.Series).
	Series bool
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
	// Costs is Nanos by part, when every priced record says; absent otherwise.
	Costs *Costs `json:"costs,omitempty"`
	// UnpricedReasons says why the unpriced records have no price, each once.
	UnpricedReasons []string `json:"unpricedReasons,omitempty"`
	// UnpricedTokens are the tokens of the unpriced records: what Nanos leaves out.
	UnpricedTokens Tokens `json:"unpricedTokens"`
}

// Costs is what each part of a usage cost, in nanodollars, part for part
// as Tokens counts it.
type Costs Tokens
type Report struct {
	Groups   []Group           `json:"groups"`
	Contexts []ContextSnapshot `json:"contexts"`
	Recent   []Record          `json:"recent"`
	// Series is usage hour by hour, oldest first; only when Query.Series.
	Series []Bucket `json:"series,omitempty"`
}

// Bucket is usage in one hour (UTC) by conversation, provider and model,
// counted as Group counts it: the records it holds are the report's, once
// each, priced the same way.
type Bucket struct {
	HourUnixMillis int64  `json:"hourUnixMillis"`
	Conversation   string `json:"conversation"`
	Provider       string `json:"provider"`
	Model          string `json:"model"`
	Tokens         Tokens `json:"tokens"`
	Nanos          *int64 `json:"nanos,omitempty"`
	Records        int64  `json:"records"`
	Unpriced       int64  `json:"unpriced"`
}

// Store must acknowledge observations durably. Repeated IDs replace snapshots;
// they do not append cumulative counters as new billable requests.
type Store interface {
	Put(context.Context, Record) error
	PutCumulative(context.Context, Record, bool) error
	PutContext(context.Context, ContextSnapshot) error
	Report(context.Context, Query) (Report, error)
	LatestModel(context.Context, string) (string, error)
	PutCommand(context.Context, CommandRun) error
	Close() error
}
type Pricer interface{ Price(Record) Record }

func number(v int64) *int64 { return &v }
