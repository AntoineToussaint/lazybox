# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is lazybox?

A reactive PR inbox TUI. Instead of checking GitHub, events flow to you — new comments, CI failures, review requests surface automatically with read/unread tracking. Each task becomes a session with an embedded terminal for running Claude Code or a shell in a git worktree.

Source-agnostic: GitHub is one provider, but Linear/Jira/etc. plug in the same way.

## Build & Run

```bash
cargo build                    # build (first build compiles SQLite, takes ~30s)
cargo run -p lazybox-tui      # run (uses `gh auth token` automatically)
cargo test --workspace         # tests
cargo clippy --workspace       # lint
make run                       # same as cargo run -p lazybox-tui
```

Logs go to `/tmp/lazybox.log`. State persisted in `~/.lazybox/v2/state.db`.

### Staying current (outdated-build guard)

A uniformly-stale build — daemon *and* client compiled from the same
old commit — passes every wire check silently and reproduces
already-fixed bugs. To catch it, the build bakes its commit
(`LAZYBOX_BUILD_GIT_SHA`) and source checkout
(`LAZYBOX_BUILD_SOURCE_DIR`) in `crates/ipc/build.rs`; at startup
`crates/tui/src/build_guard.rs` counts `<sha>..origin/main` and, when
non-zero, paints a persistent `⚠ N behind · update & restart` warning
in the sidebar header (plus a startup banner). The check reads the
local `origin/main` ref (no network), so refresh it first when in
doubt:

```bash
git fetch origin main      # update origin/main, then:
git pull --ff-only         # one-command update path
cargo run -p lazybox-tui  # rebuild + restart picks up the new build
```

The running build version is always visible in the sidebar header
(`lazybox --version` prints it too). Dev/source builds — anything not
compiled by cargo-dist's `--profile dist` (`LAZYBOX_RELEASE_BUILD`,
baked in `crates/ipc/build.rs`) — are tagged `vX.Y.Z (dev)` and never
raise the nudge: a source checkout is normally *ahead* of the latest
release and is updated with `git pull && cargo build`, not the
installer swap "update & restart" implies (issue #251). The nudge is
therefore gated on installer-managed release provenance; wiring the
release-tag comparison that would let a stale *release* binary count
how far it trails the channel is still future work, so in practice the
nudge is currently dormant.

## Architecture

16 crates organized as a client/daemon split with shared library crates. The
core library crates (core, auth, store) must NEVER depend on each
other.

```
crates/
  # ── shared libraries ────────────────────────────────────────────────
  core/            # Task, Session, Activity, SessionKey, time helpers. Source-agnostic.
  auth/            # CredentialProvider trait + chain. Env, Command, Static providers.
  store/           # Store trait + SQLite backend. Sessions, read/unread, snooze.
  config/          # YAML loader for ~/.lazybox/config.yaml.
  git-ops/         # Worktree manager (bare clones + per-task worktrees).
  tui-term/        # Embedded terminal: portable-pty + libghostty-vt + widget.
  libghostty-vt/   # Vendored safe Rust bindings for ghostty's VT parser.
  libghostty-vt-sys/ # Raw FFI layer under libghostty-vt (builds the C lib).

  # ── providers ───────────────────────────────────────────────────────
  gh-provider/     # GitHub PRs + Issues via octocrab polling.
  linear-provider/ # Linear issues via GraphQL.
  slack-provider/  # Slack DMs/channels via Web API + Socket Mode.

  # ── daemon-side ─────────────────────────────────────────────────────
  ipc/             # Wire types (Command/Event), framing, transport traits
                   #   (in-process channel and Unix-socket variants).
  agents/          # Agent trait + Claude/Codex/Cursor/GenericCli built-ins.
                   #   Also: per-provider LLM-gateway base-URL env injection
                   #   (ANTHROPIC_BASE_URL / OPENAI_BASE_URL ← agent.llm_gateway_url).
  server/          # Server library: PTY lifecycle, ring buffers, provider
                   #   polling, agent runs, JSON API gateway.

  # ── client / binary ─────────────────────────────────────────────────
  tui-core/        # Ratatui-free TUI logic: action catalog, intent
                   #   resolvers, latches, editors, platform shims.
  tui/             # tuirealm-based TUI client. Hosts `lazybox` binary with
                   #   subcommands: default (in-process daemon + TUI),
                   #   `server start/stop/status`, `server api`,
                   #   `--connect <socket>`.
```

### Key patterns

- **Client / daemon split**: Server owns state and IO (PTYs, polling, store);
  TUI is a thin renderer. Same process by default — transport is a tokio mpsc
  channel pair, no serialization. Out-of-process mode uses a Unix socket
  (length-prefixed bincode); SSH `-L` forwards it for remote use.
- **TUI structure** (tuirealm-based; see `crates/tui/src/realm/MIGRATION.md`):
  - **`Model`** (`crates/tui/src/realm/model/`) — the orchestrator. Holds the
    three panes as typed fields, owns focus (`PaneFocus`), keyboard/mouse
    dispatch, daemon-event fan-out, and the IPC client. Split into
    `keys.rs` / `events.rs` / `dispatch.rs` / `modals.rs` / `helpers.rs`.
  - **Panes** — the domain structs `Sidebar`, `RightPane`, `TerminalStack`
    live in `crates/tui/src/components/`; thin tuirealm wrappers in
    `crates/tui/src/realm/components/` delegate render + key dispatch to
    their inherent methods.
  - **Modals** (`crates/tui/src/realm/components/`) — tuirealm
    `AppComponent`s (`Confirm`, `Input`, `Choice`, `Textarea`, `Help`,
    `Loading`, …) mounted on a stack tracked by `Model::modal_stack`.
  - **Action catalog** (`crates/tui-core/src/action.rs`) — one `ActionDef`
    per user action (key chord, label, section, availability). Keyboard
    dispatch, footer hints, context menu, and the help panel all read it;
    user remaps come from `ui.action_keys` in YAML.
  - **Setup wizard** (`crates/tui/src/setup_flow.rs`) — realm-native
    `SetupRunner` state machine driving Choice/Loading/Error modals.
- **Event bus**: `tokio::sync::broadcast` inside the daemon. Providers
  produce; subscribers (TUI clients, JSON API gateway) consume.
- **Credential chain**: `EnvProvider("GH_TOKEN") → EnvProvider("GITHUB_TOKEN") → CommandProvider("gh auth token")`. Trait-based, extensible (Vault, Keychain, OAuth).
- **Store**: `Store` trait with `SqliteStore` backend at `~/.lazybox/v2/state.db`.
  Read/unread, snooze, and session metadata persist across launches.
- **Terminal**: daemon owns the PTYs (reader on std::thread, per-terminal
  ring buffer for replay on reconnect); the TUI parses the byte stream with
  the vendored `libghostty-vt` bindings (`!Send`, lives on the UI thread —
  one VT instance per terminal slot in `TerminalStack`).
- **Markdown**: comment/description rendering is hand-rolled
  (`components/comment_render.rs` + `right_pane/markdown.rs`) — inline-noise
  stripping and teaser extraction, no markdown crate.
- **Structured agent runs**: Claude Code launched with `-p --input-format
  stream-json --output-format stream-json` for non-terminal clients (Tauri,
  iOS, JSON API). Raw JSON is preserved alongside normalized events.
- **Agent autonomy**: spawned Claude Code sessions drive the repo directly
  with `gh` and `git`. Lazybox does not wrap these actions behind an
  MCP/tool-approval layer — the agent has the same tools it would in any
  other worktree.

### Adding a new provider

1. Create `crates/foo-provider/` depending on `lazybox-core` + `lazybox-auth`
2. Build a credential chain for auth
3. Implement client returning `Vec<Task>`
4. Wire in `crates/server/` alongside the GitHub / Linear / Slack pollers

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

Nearly every key lives in the action catalog
(`crates/tui-core/src/action.rs`, remappable via `ui.action_keys`) and
dispatches through `Model::handle_pane_key` → `dispatch_action`; only
per-pane cursor navigation (`j/k`, arrows) stays as pane match arms in
`components/{sidebar,right_pane,terminal_stack}`. A catalog row is one
enriched binding (#102): `chords: Vec<Chord>` where `Chord = Key |
Seq` — `Seq` is every leader/double-press (`g m`, `q q`, `] ]`), and
multiple chords are alternatives (`g v | Shift-V`); a `param`
(agent id) that generates one real `SpawnAgent` row per enabled agent
at startup; and a `guard` (`None | DoublePress | Confirm(prompt)`) that
carries the `q q` double-tap and the archive/merge/long-snooze confirm
modals. The Model holds a runtime catalog (static rows + generated
agent rows, overrides baked in); leader arming + the which-key popup
are pure functions of it (`seq_continuations`, `find_action_for_seq`).
Each row's `Section` (`Global` / `Workspace` / `Sidebar` / `Activity` /
`Terminal`) doubles as its resolution scope — `section_rank`
(`realm/model/helpers.rs`) maps `(Section, focus)` to a priority, and a
collision-detector test fails the build on two bindings colliding
within a section or at the same rank under a focus. The `?` help is the
generated Keys screen: every binding by scope with effective
(post-override) chords. `ui.keymap_preset` selects an in-tree starter
keymap (`default`, `vim`); explicit `ui.action_keys` layers on top.
The footer hint bar reads each pane's `contextual_bindings()`.

**Global**: `Tab` cycle panes, `?` help, `q q` quit, `,` settings,
`]` browse snippets (read-only catalog; `e` there opens the YAML),
`Shift-R` refresh, `Ctrl-L` force a full repaint (recovery for a
stale/garbled screen; resize and focus-regain also repaint
automatically), `Shift-T` tour, `Shift-D` sync status, `` ` ``
open the fuzzy jump-to-workspace picker (all repos; from inside an
agent use `]]` then `` ` ``), `!` jump to agent-asking workspace,
`Shift-F` jump to failing CI, `Shift-P` toggle
the activity pane (auto-hidden when the workspace has no activity), `.`
toggle focus mode (near-fullscreen agent terminal behind a slim event
header; from inside a terminal use `]]f`, and `]]q` exits),
`]]<digit>` jump the focused terminal straight to the Nth agent
workspace (sidebar order; the number rides a badge on each agent row
and the `]]` leader popup), `Shift-arrows` resize splitters, `F8` /
`Alt-s` / `Ctrl-Alt-s` toggle mouse capture (host-native text
selection), mouse-click any pane to focus it, mouse-drag splitters to
resize.

**Sidebar**: `j/k` or arrows navigate, `Enter` open (focus activity),
`w` work on this (contextual agent prompt), `c` claude, `x` codex,
`u` cursor, `s` shell, `e` editor, `m` mark read, `z` snooze,
`Shift-Z` long snooze, `f` cycle role filter, `o` cycle sort,
`Space` collapse/expand repo group, `Shift-S` cycle mailbox
(Inbox → Inactive → Snoozed), `/` search, `n` new workspace,
`Shift-N` new project, `Shift-A` adopt sessions, `Shift-J` join issue
into PR, `Shift-X` archive. `g` is a leader key that opens the
**github** group as a two-step chord (which-key popup): `g m` merge,
`g v` reviewers, `g a` assignees, `g l` labels, `g o` open in
browser. Merge has no direct-key alias — `g m` is its only default
chord. The legacy `Shift-{V,G,L,O}` keys remain as direct aliases
(Activity-section bindings shadow them while the right pane is
focused — e.g. `Shift-G` is jump-to-bottom there). `b` is a leader
for the **on-main** group (which-key popup): `b c` / `b x` / `b u`
start an agent, `b s` a shell, on the repo's shared **main checkout**
(default branch) instead of an isolated worktree — confirmed first
since edits land on the shared branch, and the resulting terminal
carries a `⎇ main` tab badge. Only surfaces on workspaces with a repo
scope.

**RightPane (Activity)**: `j/k` or arrows move the row cursor,
`g/G` top/bottom, `→/l` expand row, `←/h` collapse row, `Enter`
toggle the section, `Space`/`v` multi-select rows, `w` work on
selection, `d` toggle PR/issue description, `m` mark the focused row
read, `z` undo mark-read, `r` reply.

**TerminalStack**: all keys forward to the PTY. `]]` (configurable
escape sequence) is a *non-timed* leader (#252) that opens a small
command menu (which-key popup): `]]s` opens the snippet picker, `]]f`
toggles focus mode, `]]q` exits to the sidebar, `]]<digit>` jumps to
the Nth agent workspace, and `` ]]` `` opens the fuzzy workspace
switcher. The snippet picker (see
[`docs/snippets.md`](docs/snippets.md)) is a category-grouped list with
a live body-preview pane, filtering on key+description+category, that
auto-submits when the typed key uniquely matches (`]]srev`); snippets
sent this session float into a "Recent" group at the top (session MRU,
`recent_snippets`) so a repeat is `]]s`+Enter. The leader
is non-timed, so after `]]` it waits for the command key rather than
leaving on an idle timer — browsing snippets never races an exit; `Esc`
or any unbound key cancels back to the terminal. A lone `]` followed by
any non-`]` key is sent to the agent verbatim. `Ctrl-c`
is forwarded as an interrupt. Tile management rides the same leader
(#286): `]]|` / `]]-` split, `]]<arrow>` moves tile focus, `]]x`
closes the focused tile — `Ctrl-w` is no longer a lazybox prefix and
reaches the inner program (readline word-erase). `Shift-PgUp/PgDn`
scroll the scrollback, `Shift-Home/End` jump top/bottom (mouse wheel
works too).

## Conventions

- `thiserror` for errors in library crates, `anyhow` in the binary (`tui`)
- No `unwrap()` in library crates
- Core libraries (core, auth, store) must not depend on each other
- Provider crates depend on core + auth only
- Every public function has a test; every TUI component has a render snapshot
  (insta + ratatui `TestBackend`); every bug fix lands with a regression test
