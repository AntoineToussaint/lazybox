# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is pilot?

A reactive PR inbox TUI. Instead of checking GitHub, events flow to you — new comments, CI failures, review requests surface automatically with read/unread tracking. Each task becomes a session with an embedded terminal for running Claude Code or a shell in a git worktree.

Source-agnostic: GitHub is one provider, but Linear/Jira/etc. plug in the same way.

## Build & Run

```bash
cargo build                    # build (first build compiles SQLite, takes ~30s)
cargo run -p pilot-tui      # run (uses `gh auth token` automatically)
cargo test --workspace         # tests
cargo clippy --workspace       # lint
make run                       # same as cargo run -p pilot-tui
```

Logs go to `/tmp/pilot.log`. State persisted in `~/.pilot/v2/state.db`.

## Architecture

14 crates organized as a client/daemon split with shared library crates. The
core library crates (core, auth, events, store) must NEVER depend on each
other.

```
crates/
  # ── shared libraries ────────────────────────────────────────────────
  core/            # Task, Session, Activity, SessionKey, time helpers. Source-agnostic.
  auth/            # CredentialProvider trait + chain. Env, Command, Static providers.
  events/          # In-process event bus (broadcast channel). EventKind enum.
  store/           # Store trait + SQLite backend. Sessions, read/unread, snooze.
  config/          # YAML loader for ~/.pilot/config.yaml.
  git-ops/         # Worktree manager (bare clones + per-task worktrees).
  tui-term/        # Embedded terminal: portable-pty + ghostty-vt + widget. Used
                   #   by both daemon (PTY ownership) and TUI (replay).

  # ── providers ───────────────────────────────────────────────────────
  gh-provider/     # GitHub PRs + Issues: octocrab polling → generic Events.
  linear-provider/ # Linear issues via GraphQL.

  # ── daemon-side ─────────────────────────────────────────────────────
  ipc/             # Wire types (Command/Event), framing, transport traits
                   #   (in-process channel and Unix-socket variants).
  agents/          # Agent trait + Claude/Codex/Cursor/GenericCli built-ins.
                   #   SessionWrapper trait (tmux is the default).
  llm-proxy/       # 127.0.0.1 HTTP pass-through that records structured
                   #   telemetry (tokens, tool calls, cost) from agent traffic.
  server/          # Server library: PTY lifecycle, ring buffers, provider
                   #   polling, agent runs, JSON API gateway.

  # ── client / binary ─────────────────────────────────────────────────
  tui/             # Component-tree TUI client. Hosts `pilot` binary with
                   #   subcommands: default (in-process daemon + TUI),
                   #   `daemon start/stop/status`, `server api`,
                   #   `--connect <socket>`.
```

### Key patterns

- **Client / daemon split**: Server owns state and IO (PTYs, polling, store);
  TUI is a thin renderer. Same process by default — transport is a tokio mpsc
  channel pair, no serialization. Out-of-process mode uses a Unix socket
  (length-prefixed bincode); SSH `-L` forwards it for remote use.
- **TUI tiers**: three traits, three jobs.
  - **`Pane`** (`crates/tui/src/pane.rs`) — focusable region. Owns keymap,
    border, handles keys, reacts to events. Three impls: `Sidebar`,
    `RightPane`, `TerminalStack`. `App` holds them as concrete fields, no
    `dyn Pane` indirection; focus is a single `PaneId`.
  - **`Modal`** (`crates/tui/src/modal.rs`) — full-screen overlay. Stack-
    based via `ModalStack`. Owns input while mounted; `tick()` polls async
    work (e.g. `LoadingModal` waits on a future).
  - **`Component`** (`crates/tui/src/component.rs`) — dumb sub-widget.
    Pure `render(area, frame)` + optional `on_event`. No focus, no keymap.
- **Configuration as `Flow`** (`crates/tui/src/flow.rs`): multi-step wizards
  (first-run setup, "Add Linear", "Edit scopes") are `Flow` impls that chain
  generic Modal primitives in `crates/tui/src/components/config/`
  (`ChoiceModal<T>`, `InputModal`, `ConfirmModal`, `LoadingModal<T>`). The
  App routes modal payloads back through `Flow::step()`; each flow accumulates
  its own state and emits a typed `Output`.
- **Event bus**: `tokio::sync::broadcast` inside the daemon. Providers
  produce; subscribers (TUI clients, JSON API gateway) consume.
- **Credential chain**: `EnvProvider("GH_TOKEN") → EnvProvider("GITHUB_TOKEN") → CommandProvider("gh auth token")`. Trait-based, extensible (Vault, Keychain, OAuth).
- **Store**: `Store` trait with `SqliteStore` backend at `~/.pilot/v2/state.db`.
  Read/unread, snooze, and session metadata persist across launches.
- **Terminal**: PTY reader on std::thread. ghostty-vt parser behind Mutex.
  Daemon keeps a per-terminal ring buffer (64 KB) for replay on reconnect.
- **Markdown**: PR descriptions rendered via `tui-markdown` (pulldown-cmark + syntect).
- **Structured agent runs**: Claude Code launched with `-p --input-format
  stream-json --output-format stream-json` for non-terminal clients (Tauri,
  iOS, JSON API). Raw JSON is preserved alongside normalized events.
- **Agent autonomy**: spawned Claude Code sessions drive the repo directly
  with `gh` and `git`. Pilot does not wrap these actions behind an
  MCP/tool-approval layer — the agent has the same tools it would in any
  other worktree.

### Adding a new provider

1. Create `crates/foo-provider/` depending on `pilot-core` + `pilot-events` + `pilot-auth`
2. Build a credential chain for auth
3. Implement client returning `Vec<Task>` + poller emitting `Event`s
4. Wire in `crates/server/` alongside the GitHub and Linear pollers

### Adding a new auth source

Implement `CredentialProvider` trait with `name()` and `async resolve(scope) → Credential`.
Add to the chain in `crates/server/`.

### Adding a new storage backend

Implement `Store` trait (get/save/mark_read/list/delete session records).
Swap in `crates/server/` instead of `SqliteStore`.

### Adding a new agent

Implement `Agent` in `crates/agents/` (id, spawn argv, resume argv, state
detection, optional hook config, prompt injection). Register in
`agents::registry()`. The `GenericCli` agent already handles arbitrary CLIs
via YAML config without recompilation.

## Keybindings

Each Pane (Sidebar / RightPane / TerminalStack) declares its own
keymap; the bottom hint bar reads from the focused Pane's
`Pane::keymap()`. Global keys live in `app::dispatch_key`.

**Global**: `Tab` cycle panes, `?` help, `q q` quit, `Ctrl-Shift-D`
detach focused pane to a new window, `Shift-arrows` resize splitters,
mouse-click any pane to focus it, mouse-drag splitters to resize.

**Sidebar**: `j/k` navigate, `Enter` open, `c` claude, `b` shell,
`x` codex, `u` cursor, `m` mark read, `/` search. `g` is a leader
key that opens the **github** group as a two-step chord (which-key
popup): `g m` merge, `g v` reviewers, `g a` assignees, `g l` labels,
`g o` open in browser. The legacy `Shift-{M,V,G,L,O}` keys remain as
direct aliases.

**RightPane (Activity)**: `j/k` scroll, `g/G` top/bottom.

**TerminalStack**: all keys forward to the PTY. `]]` (configurable
escape sequence) is a leader: bare `]]` returns to the sidebar, while
`]]<key>` opens the snippet picker (see
[`docs/snippets.md`](docs/snippets.md)) — fuzzy-filters by snippet
key, auto-submits when the filter uniquely matches. A lone `]`
followed by any non-`]` key is sent to the agent verbatim. `Ctrl-c`
is forwarded as an interrupt.

## Conventions

- `thiserror` for errors in library crates, `anyhow` in the binary (`tui`)
- No `unwrap()` in library crates
- Core 4 libraries (core, auth, events, store) must not depend on each other
- Provider crates depend on core + events + auth only
- Every public function has a test; every TUI component has a render snapshot
  (insta + ratatui `TestBackend`); every bug fix lands with a regression test
