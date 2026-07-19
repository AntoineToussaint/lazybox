# Changelog

All notable user-facing changes to lazybox are documented here. The project
uses [Semantic Versioning](https://semver.org/) while pre-1.0 releases may still
contain explicitly documented compatibility changes.

## [Unreleased]

## [0.1.7] - 2026-07-19

### Added

- A persistent, pinned Ghostty/Zig source cache and a strictly offline
  `make release` path after one online `make setup`.
- Release-artifact and TUI startup smoke tests, Lighthouse/size budgets,
  Criterion regression tracking, security reporting guidance, state-recovery
  documentation, and automated dependency updates.
- A grouped `x` workspace menu for creating workspaces/projects, adopting or
  joining sessions, long snooze, archive, and issue close actions.
- Exact Rust 1.88 MSRV verification in CI and a pinned stable contributor
  toolchain.
- An accessible custom 404 page and resilient no-JavaScript homepage content.
- Ask Lazybox as the primary `?` surface, with live effective-keymap search,
  conversational workflow help, and searchable terminal-leader commands.
- `lazybox scan` for discovering and adopting externally created checkouts.
- A unified policies menu for per-PR and per-issue automation controls, plus a
  configurable tabs-first layout for newly opened terminals.
- Explicit terminal `Exited` state so clean exits, crashes, and agents that
  fail during startup remain distinguishable.

### Changed

- The HTTP command endpoint now waits for command handling to finish, enforces
  body/time/connection limits, and requires an explicit opt-in for plaintext
  non-loopback binding.
- The `lb` command is a small exec alias instead of a second full TUI binary.
- Default contextual work is now the deterministic `w w` chord; the previous
  timed single-`w` fallback and its 600 ms delay were removed.
- The workspace uses Rust edition 2024 and the newest dependency releases that
  preserve the Rust 1.88 compiler floor.
- IPC serialization now uses bincode 2 with explicit frame limits and
  trailing-byte rejection. Older daemon/client pairs must be upgraded
  together.
- Workspace-management operations are grouped under `x`; consult the generated
  keybinding reference or Ask Lazybox for the complete mapping.
- The shortcut system now follows five named menus (`w` work, `a` agent, `b`
  main branch, `g` GitHub, `x` workspace), with a compact secondary index and
  `g r` as the mnemonic reviewer chord (replacing `g v`).
- Terminal scrolling, replay recovery, and input now pass through one owner
  with sequenced resynchronization and isolated ordered command lanes.
- IPC protocol version 11 adds bounded admission and explicit lifecycle
  contracts; daemon and client must be upgraded together.

### Fixed

- Unreadable persisted records are preserved and surfaced through the TUI
  event stream and JSON API instead of silently disappearing.
- Production shared-state locks no longer poison after a thread panic, and
  process-spawn logs no longer print raw argument values.
- Unknown or stale workspace spawn requests can no longer launch an agent in
  the daemon's current directory.
- Failure to open persistent state is now fatal and visible instead of silently
  falling back to an empty in-memory database.
- Slack setup reads bot and app tokens with terminal echo disabled.
- `terminal.escape_char` is honored and legacy configuration remains readable.
- Terminal help now documents the real `]]q` exit and `]]s<key>` snippet
  chords, including when the terminal escape character is remapped.
- Homepage contrast, accessible navigation names, reduced-motion behavior,
  video fallback loading, and no-JavaScript rendering.
- Mouse-wheel input now scrolls the terminal tile under the pointer, including
  local scrollback on the primary screen, without fighting the focused tile.
- Terminal output survives detach/reconnect gaps without stale cursors,
  duplicate bytes, lost tails, or unbounded replay loops.
- Agent crashes and immediate startup failures remain visible, while cleanly
  exited agent terminals close automatically without deleting the workspace.
- The last agent prompt survives restart, `InputNeeded` remains sticky across
  ambiguous scrapes, long Ask Lazybox input wraps, and which-key menus support
  arrow and `j`/`k` navigation.
- Workspace persistence is atomic and truthful, workspace/terminal moves are
  serialized, and failed commits can no longer leave memory and SQLite out of
  sync.
- IPC connections, background tasks, terminal writes, and command queues now
  have explicit capacity and shutdown bounds instead of growing or hanging
  indefinitely under load.

[Unreleased]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.7...HEAD
[0.1.7]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.6...v0.1.7
