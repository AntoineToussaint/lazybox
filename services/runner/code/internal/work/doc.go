// Package work is the mission model of ORCHESTRATION.md: Mission → (optional)
// Goal → Task → Attempt, kept as an append-only, typed and versioned event log
// per mission. Nothing else about a mission is stored: its board (each task's
// status and the trust behind it) is derived from the log and from live
// observations by pure functions (Fold, Derive), so a restarted Runner, a
// replayed golden log and a running Runner agree on every row.
//
// The package is independent of transport, UI and provider adapters. The
// Runner (internal/runner/work.go) feeds it evidence it already records —
// `lazybox run` reports, commits, pull requests and their checks — and the
// liveness of the conversations that are attempts.
//
// The process a mission runs is behind Policy: PolicyFor resolves a built-in
// preset through internal/process, and a mission pins what it uses.
package work
