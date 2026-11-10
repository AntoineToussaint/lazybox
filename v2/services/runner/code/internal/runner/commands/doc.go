// Package commands owns managed shell processes and pipe-output registration,
// spooling, lifecycle observations, and replay for agents and human consoles.
// Closing a viewer is independent of canceling execution.
// Managed processes and pipe capture share the same bounded output journal.
// With a host root (NewHosted), each managed command runs in its own session
// host and outlives the Runner: Detach leaves it running and the next manager
// re-attaches, recording the rest of its output exactly once.
package commands
