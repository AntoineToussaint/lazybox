# Changelog

All notable user-facing changes to lazybox are documented here. The project
uses [Semantic Versioning](https://semver.org/) while pre-1.0 releases may still
contain explicitly documented compatibility changes.

## [Unreleased]

## [0.1.8] - 2026-07-26

### Added

- `lazybox worktree list` and `lazybox worktree gc` report managed worktrees
  with their disk totals and reclaim the orphaned ones left behind by
  interrupted or crashed provisioning (`--dry-run` / `--force`) (#574).
- A dismissable startup notice detects newer source commits or published
  releases, shows a checkout- and install-channel-specific manual update
  command, and stays quiet for a previously dismissed target.
- Agent-to-agent handoff (`x s`): capture a running agent's output, edit the
  brief, and inject it into another workspace's session (#431).
- Update branch: `g u` merges the base into a behind PR's head, and `Shift-U`
  fans it out over the sidebar multi-select (#484).
- A multi-select filter menu on `f` combining state, role, and kind predicates
  with match counts and removable header chips (#443).
- Per-workspace notes (`n`) (#458).
- A scrollable markdown reader modal for long or richly formatted PR/issue
  bodies, opened by a second `d` on the description preview (#448).
- In-place import of externally created checkouts (`x i`, backed by
  `ScanCheckouts`/`CheckoutsDiscovered`), completing the `lazybox scan`
  discovery flow (#452).
- A per-session prompt-history picker (`]]h`): every prompt sent to the agent,
  persisted across restarts, re-sendable with Enter (#523).
- A track-main arm keeping scratch workspaces tied to the repo's default
  branch (#535).
- Per-workspace sync (`g s`): a targeted re-poll of just the focused
  workspace's PR/issue instead of the global sweep (#456).
- Activity-pane cycling (`Shift-P`): full → one-line summary → hidden,
  remembered per workspace with a `ui.activity_pane_default` starting mode
  (#487).

### Fixed

- Deleting, merging, or closing a workspace now reclaims its worktree
  directory instead of leaking a multi-gigabyte checkout on disk (#573, #575).

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
- `lazybox scan` for read-only discovery of externally created checkouts
  (in-place import shipped later — see the Unreleased `x i` entry).
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

[Unreleased]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.8...HEAD
[0.1.8]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/AntoineToussaint/lazybox/compare/v0.1.6...v0.1.7
