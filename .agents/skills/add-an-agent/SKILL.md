---
name: add-an-agent
description: Add or change an agent CLI backend in lazybox (Claude, Codex, Cursor, GenericCli) — the Agent trait implementation, registry entry, state detection, model tiers, and gateway injection. Use when supporting a new agent CLI, changing how an existing one is spawned or resumed, or fixing agent-state detection.
---

# Add an agent

## First: does it need to be a built-in?

`GenericCli` already runs arbitrary CLIs from YAML with no recompilation. A
new built-in earns its place only when the agent needs behaviour YAML cannot
express — structured stream protocols, auth-failure detection, credit
recovery, hook settings, per-provider gateway injection. If you are adding a
built-in to spawn a binary with different arguments, stop and use config.

## The trait

Implement `Agent` in `crates/agents/src/agent.rs` and register it in
`registry()` (`Registry::default_builtins`). Most of the trait has sensible
defaults; the ones you almost always supply are:

- `id`, `display_name`, `badge`
- `spawn` / `resume` / `resume_session` argv
- `detect_state` (and its chunked variants) — see below
- `encode_prompt`, so an injected prompt is delivered the way that CLI expects
- `gateway_injection`, if the agent can be metered: an env base URL for
  Claude and Cursor, `-c` provider flags for Codex
- `llm_provider` / `meterable`, which decide whether the proxy sees it at all

## State detection is the hard part

`detect.rs` turns raw PTY bytes into `Working`, `Idle`, `InputNeeded`,
usage-limit states and so on, and those states are **control signals**: an
`InputNeeded` releases the settle-gated inject and drives the `!` jump, so a
false positive is unclearable by the user and can land a prompt mid-turn.

Two rules:

1. A new rule must also be taught to `dialog_marker_pos` — a rule that lands
   in only one of the two disagrees with itself.
2. Land it with a regression test over the actual byte stream that fooled the
   detector, not a synthesized approximation of it.

Never widen a detector to make a symptom go away. Reproduce the stream, set
the rule back, and confirm the original bytes break it.

## Model tiers

Declare tiers under `agents.<id>.models` (an ordered `alias → { label, args }`
menu plus a `default` for bare spawns), or let the agent ship a built-in menu.
Re-read [`crates/agents/AGENTS.md`](../../../crates/agents/AGENTS.md) before
touching resolution — overlay-versus-replace, `excluded_from_default`, the
precedence-rank rules and the untrusted-spawn label-only rule are each load
bearing, and each has a test.

Pin model ids bare. A long-context suffix carries a premium that a bare spawn
must never opt into; a user who wants it declares a tier of their own.

## Check what else knows about agents

An agent id shows up beyond this crate: spawn chords generated from the
action catalog (`crates/tui-core/src/action.rs`), agent badges, setup
detection in `crates/tui-boot/`, and the session briefing in
`session_context.rs`. Adding a built-in without them leaves an agent that
exists but cannot be started.

## Verify

```bash
cargo test -p lazybox-agents
cargo test -p lazybox-tui-core   # catalog rows generated per enabled agent
```

Then the full gate — see the `run-local-checks` skill. Spawning a real CLI is
usually not exercisable in tests; say so in the PR body rather than implying
you ran it.
