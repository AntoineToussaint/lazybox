// Package engine owns generic agent-session supervision, operation ordering,
// queueing, receipts, pending requests, and the execution event journal.
// It depends on the agent contract, not provider-specific protocol details.
// Sessions and journals are local to this experimental Runner.
package engine
