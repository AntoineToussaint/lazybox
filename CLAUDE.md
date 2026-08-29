# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is lazybox?

A reactive PR inbox TUI. Instead of checking GitHub, events flow to you — new comments, CI failures, review requests surface automatically with read/unread tracking. Each task becomes a session with an embedded terminal for running Claude Code or a shell in a git worktree.

Source-agnostic: GitHub is one provider, but Linear/Jira/etc. plug in the same way.

## Build & Run

```bash
cargo build                    # build (first build compiles SQLite, takes ~30s)
cargo run -p lazybox-tui-boot # run (uses `gh auth token` automatically)
cargo test --workspace         # tests
cargo clippy --workspace       # lint
make run                       # same as cargo run -p lazybox-tui-boot
```

The `lazybox` binary lives in `lazybox-tui-boot`, not `lazybox-tui` (which is
now a library-only crate — see Architecture).

Logs go to `/tmp/lazybox.log`. State persisted in `~/.lazybox/v2/state.db`.

### Before you compile or test — check shared resources

Lazybox routinely runs many agents on one box. **Before** `cargo build`,
`cargo test`, or `cargo clippy`, check for available resources — sample the
system load (`uptime` vs. `nproc`) and back off, throttle the job count
(`CARGO_BUILD_JOBS`), or scope to the crate you touched (`cargo test -p
<crate>`) while iterating when the machine is already busy. Blindly grabbing
every core when fifteen other agents are doing the same is what drives the box
to 100% CPU. Throttling changes *how hard* you compile, never *whether* the
full gate suite runs before you push — scoped runs miss cross-crate gates.
Full guidance: [`docs/agent-resource-awareness.md`](docs/agent-resource-awareness.md).

### Staying current (outdated-build guard)

A uniformly-stale build — daemon *and* client compiled from the same
old commit — passes every wire check silently and reproduces
already-fixed bugs. At startup `crates/tui/src/build_guard.rs` checks
the channel appropriate to the running build and opens a dismissable
update modal when something newer exists:

- Dev/source builds compare the baked `LAZYBOX_BUILD_GIT_SHA` with the
  baked checkout's current `HEAD` and, when it can fast-forward safely,
  that branch's tracking upstream. This path performs no network request
  and still runs for dirty builds.
- Cargo-dist release builds compare `CARGO_PKG_VERSION` with GitHub's
  latest published release through a bounded, cached API request.

Dismissal is persisted for the available commit/release, so the same
target does not reappear on the next launch. A newer target is shown
again. Lazybox never updates itself; the modal only names the command:

Release installs get the command for their actual channel: Homebrew uses
`brew upgrade lazybox`, while cargo-dist's shell installer is re-run. Source
commands `cd` to the baked checkout before pulling or rebuilding, so they
never operate on the repository from which lazybox happened to be launched.

The running build version remains visible in the sidebar header and through
`lazybox --version`.

## Architecture

17 crates organized as a client/daemon split with shared library crates.
Core-library layering: `core` depends on no internal crate; `auth` depends on
no internal crate; `store` may depend on `core` only. The client/UI split is
policed too: `tui` (the UI library) may depend only on `{ipc, tui-core,
tui-term, config, core}` — a `use lazybox_store::…` there is a compile error;
`tui-boot` (the binary) carries the daemon/provider/store wiring. Enforced by
the workspace dep-rules test (`crates/core/tests/dep_rules.rs`).

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
                   #   resolvers, latches, editors, platform shims. Also the
                   #   UI library's gateway to agent metadata (badges).
  tui/             # tuirealm-based TUI *library*: realm model, panes,
                   #   modals — a thin renderer over IPC. No store/server/
                   #   provider deps (enforced by dep_rules).
  tui-boot/        # The binary crate. Hosts `lazybox` with subcommands
                   #   (default in-process daemon + TUI, `server
                   #   start/stop/status`, `server api`, `slack …`,
                   #   `--connect <socket>`), embedded-daemon boot, session
                   #   recovery, setup persistence, provider detection, and
                   #   the release build-guard (octocrab). Carries the wide
                   #   deps the UI library gave up.
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
dispatches through `Model::handle_pane_key` → `dispatch_action`; per-pane
cursor navigation (`j/k`, arrows) plus a small allowlisted set of
pane-native arms (`PANE_NATIVE_KINDS` in `realm/model/keys.rs`, ~a dozen
kinds — some of which don't honor remaps) stay as match arms in
`components/{sidebar,right_pane,terminal_stack}` and `keys.rs`. A catalog row is one
enriched binding (#102): `chords: Vec<Chord>` where `Chord = Key |
Seq` — `Seq` is every leader/double-press (`g m`, `q q`, `] ]`), and
multiple chords are alternatives (a user override like
`g v | Shift-V`); a `param`
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
The default keymap is leaders-only (#304): grouped actions ship a
single leader chord, no direct-key aliases — a concept with ≥2 sibling
actions gets a leader group (named in `leader_group_label`); only true
primary actions (`w`, `Enter`, navigation) earn top-level keys. The
footer hint bar reads each pane's `contextual_bindings()`, collapsing
a leader group into one cell (`g ▸ github`, `a ▸ agent`) — the
which-key popup and `?` help reuse the same group labels.

**Global**: `Tab` cycle panes, `?` help, `q q` quit, `,` settings,
`t` theme picker (live-preview palette list; the choice persists to
`ui.theme`), `Shift-W` start agent from anywhere (pick a project,
name a workspace, spawn the default agent — one flow, any pane),
`]` browse snippets (read-only catalog; `e` there opens the YAML),
`Shift-R` refresh, `Ctrl-L` force a full repaint (recovery for a
stale/garbled screen; resize and focus-regain also repaint
automatically), `Shift-T` tour, `Shift-D` sync status, `Shift-M`
messages log (a scrollable, `c`-clearable history of recent footer
notices; #309), `Esc` dismiss the current footer notice regardless of
severity — severity only drives auto-fade, never dismissability, and
`Esc` resolves through a fixed chain: it yields first to a live
terminal (the PTY owns it), then clears a live `v` multi-select (from
the sidebar or the activity pane alike), then dismisses the notice,
then falls to the pane (committed-search clear),
`` ` ``
open the fuzzy jump-to-workspace picker (all repos; from inside an
agent use `]]` then `` ` ``), `!` jump to agent-asking workspace,
`Shift-F` jump to failing CI, `Shift-P` cycle
the activity pane full → summary (a slim one-line count of new activity /
failing CI) → hidden → full, remembered per workspace with a
`ui.activity_pane_default` starting mode (auto-hidden when the workspace
has no activity), `.`
toggle focus mode (near-fullscreen agent terminal behind a slim event
header; from inside a terminal use `]]f`, and `]]q` exits). Focus mode
carries **multi-workspace layouts** (#1258): `]]v` cycles Single →
SplitV → SplitH → 2×2 Grid (persisted as `ui.focus_layout`, applied to
attach clients too); the panes fill from the starred roster — pane 1 =
the current workspace, then the next starred workspaces with a live
terminal in sidebar order, then most-recently-active agent workspaces,
then a dim star-nudge placeholder — each pane a bordered frame (accent
= focused) titled with the workspace name + agent-state badge + star
digit, showing that workspace's agent terminal (fallback: most recent),
its PTY resized to the pane. Input goes to the focused pane only;
`]]<arrow>` moves pane focus (panes-first — tile motion stays a Single
behavior), `]]<digit>` retargets the focused pane (swapping if the
target is already visible), `]]z` zooms the focused pane to Single and
back,
`]]<digit>` jump the focused terminal straight to the Nth **focused**
(starred) workspace (sidebar order; the number rides a badge on each
focused row and the `]]` leader popup — only focused workspaces are
numbered), `Shift-arrows` resize splitters
(`Shift-←/→` everywhere; `Shift-↑/↓` too, except in the sidebar where
they extend the multi-select instead — #932), `F8` /
`Alt-s` / `Ctrl-Alt-s` toggle mouse capture (host-native text
selection), mouse-click any pane to focus it, mouse-drag splitters to
resize.

**Sidebar**: `j/k` or arrows navigate, `Enter` open (focus activity),
`w w` work on this (contextual agent prompt), `s` shell, `e` editor,
`m` mark read, `z` snooze, `f` open the filter
menu (a multi-select over state / role / kind predicates — with-agent,
CI-failing, conflict, unread, asking, review-requested, auto-merge,
author/reviewer/assignee/mentioned, PR/issue — combined AND-across /
OR-within-axis, shown with match counts and as removable header chips),
`o` cycle sort, `Space` collapse/expand the repo group **only when the
cursor sits directly on its header row** — on a workspace row a bare
Space is inert so it can't fold the group you're navigating (#1099;
move to the header or click the ▾ triangle to collapse), `p`
pin/unpin the cursor's repo group to the top of the sidebar (pinned
repos render first in pin order, the rest keep the algorithmic order;
the pin set persists — #760), `Shift-S`
cycle mailbox (Inbox → Inactive → Snoozed), `/` search (composes with
the active filters; matches title, number, repo, labels, reviewers /
assignees). While editing, the bottom bar is a filled, accented field
with a `🔍 /<query>` prefix + block cursor so it's unmistakable you're
typing into search, matched substrings are underlined in the visible
rows, and a query that filters everything away shows an explicit "No
matches for … · Esc to clear" panel rather than a blank pane (#1099).
`x` is a
leader for the **workspace** group (which-key popup): `x n` new
workspace, `x R` rename (change the focused workspace's display name
in place — key/worktree stay put; #744), `x m` move the cursor's
source (repo / Linear team) to a **Space** — the higher-level grouping
tier above repo headers (#860): sources auto-seed into an owner-named
Space (`obin-ai/*` → `obin-ai`), a blank name unassigns back to that
default, and a named Space collects repos across owners; `Space` on a
Space header folds it, and both the assignment (`ui.spaces`) and
collapse (`ui.collapsed_spaces`) persist. `x p` new project, `x a`
adopt sessions, `x s` send to
session (agent-to-agent handoff, #431 — capture the focused agent's
on-screen output, pick a target workspace, edit the brief, and
inject + submit it into that session's agent; the source is excluded
so a handoff can't loop back to itself, and a visible `source →
target` notice records the trail), `x o` open with… (a config-driven
picker over `open_with:` apps — Obsidian / Finder / browser / … —
decoupled from the `e` code editor; `{path}`/`{url}`/`{branch}`/`{repo}`
tokens substituted at launch, each picker row showing its command;
apps whose tokens the workspace can't supply are hidden, a `{path}` app
on a worktreeless workspace provisions one first like `e`, and only
`{path}` apps are worktree-bound (decline on a remote workspace, `{url}`
apps open like `g o` per #742); a per-app `key:` binds a favorite to a
direct chord that skips the picker (`open_with_app.<name>`, remappable);
#1100), `x j` join
issue
into PR, `x z` long snooze, `x x` archive, `x c` close issue
(as not-planned, upstream; issue workspaces only, confirmed first),
`x k` close & kill — the combined `g d` + `x x`: delete/close the
issue or PR upstream AND archive the workspace (killing its sessions)
in one confirm, for ending a finished line of work (only when there's
an open issue/PR; confirmed first) —
the legacy `Shift-{N,A,J,X,C,Z}` direct aliases are gone (#304).
`r` reply (works from the sidebar as well as the activity pane —
it's a Workspace-section action). With a `sandbox:` box configured
(#965), `r` instead arms the **remote-spawn** leader — `r c` / `r x` /
`r u` spawn that agent on the box (the worker ensures/wakes/connects it
lazily; the row gets a latched `⇅ <box>` glyph, rolled back with an
error notice if the spawn drops), a multi-select fans the spawn out
behind the usual "start N agents?" confirm, and reply becomes the
same-key double-tap `r r` (the leader stashes the shadowed direct
action). `v` multi-selects the cursor
workspace; `Shift-↑`/`Shift-↓` extend the selection from the cursor
(spreadsheet-style contiguous sweep, #932) and `Shift-click` extends
it to the clicked row (marks survive j/k; `Esc` clears). A live
multi-select makes **every bulk-appropriate workspace action** target
the whole set instead of just the cursor row — selection is the
primary path, not a special broadcast mode (#932, mechanism from
#899): `w w` / `w S/M/L`, `w c`·`w x`,
`a c`·`a x`·`a S/M/L` and `s` start (or continue) the contextual
agent / shell on each selected workspace — heavy spawns gate behind
one "start N agents?" confirm and inject into any workspace already
running an agent; `g m` merge, `g u` update-branch, `z` snooze,
`x x` archive, `m` mark-read, `g s` sync, `g g` arm-auto-merge,
`g d` delete-or-close, and `x c` close-issue apply per target,
running the eligible ones and summarizing what was
skipped and why. Destructive bulk actions confirm with the count + an
affected list + the eligible/skipped split (e.g. "Merge 3 of 5
selected PRs?", "Close 3 PRs without merging and delete 1 issue?"),
snapshotting the selection at mount so a poll under
the modal can't redirect them. Inherently single-target actions stay
focused-only: open editor (`e`), rename (`x R`), view diff,
open-in-browser (`g o`),
reviewers/assignees/labels, the policies menu (`g p`), notes, pin
(`p`), move-to-Space (`x m`), and the on-main spawns (`b …`). The
shared `resolve_targets` helper (selection-or-focused) means a new
workspace action opts into bulk by reading it. `Shift-B` is the one
**broadcast-only** flow that survives selection-first (#932): free
text — optionally seeded from a snippet — sent to every selected
workspace, which no single-row action key expresses. A snippet picker
(`Ctrl-F` skips it for free text) feeds a compose textarea pre-filled
with the snippet body, and submit delivers per target — running agents
via the
settle-gated inject, plain shells via a direct write, and session-less
workspaces that still have a repo scope get the default agent spawned
with the message as its initial prompt (#836 — behind a "start N
agents?" confirm since spawning is heavy); only repo-less, project-less
workspaces (nothing to spawn into) are skipped and named in the summary
notice. `]]` is a **sidebar leader** too (#871 — the terminal escape
sequence made reachable where the cursor already is), addressing the
**cursor workspace's** agent so a snippet reaches one workspace without
entering its terminal or running the broadcast machinery: `]]s<key>`
sends a snippet (same fast-path + settle-gated `DeliverSnippet` inject as
inside the terminal — a session-less-but-spawnable workspace falls back
to the broadcast spawn per #836), `]]l` a skill (#797), `]]r` recall,
`]]h` history, `]]u` open-urls — the workspace-addressed subset only
(terminal-pane chords like `]]f`/`]]x` are inert here). Because `]]`
now shares the sidebar's `]` (browse), a lone `]` is held one
`escape_window` and resolves to the browser on the next key or the idle
tick, mirroring the terminal's held literal `]`. (The dedicated
`Shift-U` bulk-update-branch key is retired — `g u` under a selection
already updates every behind PR in the set, #484/#932.) `a` is a leader
for the **agent** group (which-key popup): `a c` claude, `a x` codex,
`a u` cursor — no top-level `c`/`x`/`u` aliases (re-add via
`ui.action_keys`, keyed `spawn_agent.<id>`). These explicit spawn keys
(and `r c`/`r x`/`r u`, `b c`/`b x`/`b u`, `a S`/`a M`/`a L`) always
start a NEW agent, even beside an idle one of the same kind (#1310, via
`Spawn { force_new: true }`) — lazybox no longer enforces one-agent-per-
workspace. The daemon still collapses genuinely-concurrent duplicate
spawns (the SpawnCoordinator) and adopts an issue→PR managed-branch-owner
transfer, so a double-fire or a rebadge never silently forks two backends.
The reuse-first keys stay reuse-first: bare `w`/`w w` inject into a live
conversation, and bulk / autonomous (`@lazybox` mentions, auto-fix) still
collapse onto a running agent. Both the `w` and `a`
leaders also carry **model-tier** chords (#308): `w S`/`w M`/`w L` work
on the contextual agent at the small / medium / large model, and
`a S`/`a M`/`a L` spawn the default agent at that tier. Tiers are
declared per agent under `agents.<id>.models` in YAML (an ordered
`alias → { label, args }` menu plus a `default` tier for bare spawns);
Claude ships a built-in Haiku/Sonnet/Opus menu, other agents define
their own. The alias is agent-agnostic at the chord — the daemon maps
it to whatever agent the spawn targets — and the picked tier's label
rides a `◆ Opus` tab badge. `g` is a leader that
opens the **github** group the same way: `g m` merge, `g u` update
branch (the "Update branch" button — merge base into head; only on a
PR behind its base, #484), `g g` toggle
auto-merge on green (lazybox merges automatically once CI passes —
own PR, no conflicts, no changes requested; only while lazybox runs),
`g p` policies (the unified automation-policies menu — one surface
listing merge-on-green, per-session auto-fix arm/disarm, and
GitHub-native auto-merge status for the focused PR/issue, each toggled
in place; #363), `g r` reviewers, `g a` assignees, `g l` labels,
`g s` sync (a targeted re-poll of just the focused workspace's own
PR/issue instead of the global `Shift-R` sweep — cheap when you're
waiting on one PR's CI; #456), `g o` open in browser, `g d` delete issue / close PR (confirmed
first, naming the target; an issue is hard-deleted when the token
has admin rights, else closed as not-planned with a notice; a PR is
closed without merging; #408) — leader chords only, the legacy
`Shift-{M,V,G,L,O}` direct aliases are gone (#304). Armed policies
surface as row pills (`ARM` merge-on-green, `FIX` auto-fix); the
per-session auto-fix arm/disarm overrides the global `no-auto-fix` /
`do-not-lazybox` label opt-out, which the menu still reflects. `b` is a leader
for the **on-main** group (which-key popup): `b c` / `b x` / `b u`
start an agent, `b s` a shell, on the repo's shared **main checkout**
(default branch) instead of an isolated worktree — confirmed first
since edits land on the shared branch, and the resulting terminal
carries a `⎇ main` tab badge. Only surfaces on workspaces with a repo
scope.

**RightPane (Activity)**: `j/k` or arrows move the row cursor,
`g/G` top/bottom, `→/l` expand row, `←/h` collapse row, `Enter`
toggle the section, `Space`/`v` multi-select rows, `w w` work on
selection, `d` toggle the PR/issue description teaser (Collapsed ⇄
Preview); a second `d` on a long — or richly-formatted (tables, fenced
code, images) — preview, or clicking `+N more lines`, opens the full
body in a scrollable markdown reader modal (#448: proper
headings/lists/code/links/tables, `j/k`·PgUp/PgDn·wheel to scroll,
click a link to open it, `Esc` to close; the header hint reads
`d · read full` when `d` will open it, `d · collapse` otherwise);
`a` in the reader opens **Ask about this PR** (#945) — a streamed
chat scoped to the focused PR/issue, reusing the Ask Lazybox
agent-piggyback with a PR-scoped context (metadata + activity + the
worktree diff when a checkout exists; degrades to metadata-only with
a note when none is), follow-ups keep context (`Tab` toggles
follow-up / new question), `Esc` returns to the reader.
`m` mark the focused row read, `z` undo mark-read, `r` reply.

**TerminalStack**: all keys forward to the PTY. `]]` (configurable
escape sequence) is a *non-timed* leader (#252) that opens a small
command menu (which-key popup): `]]s` opens the snippet picker, `]]l`
opens the skills picker (#797 — the focused agent's Claude Code skills
discovered from `.claude/skills/` + `~/.claude/skills/`, previewed by
their description and grouped by Repo/User scope; picking one injects an
explicit "Use the `<skill>` skill." instruction through the same
settle-gated path snippets use, so a model-selected capability gains a
deterministic trigger + Recent), `]]r`
recalls the last prompt (in-flight draft, else last submitted message)
back into the agent composer without submitting it — both survive a
restart (persisted per terminal in the store, #373), `]]h` opens the
per-session prompt-history picker (#523 — every prompt sent to this
agent, newest-first and timestamped, snippet-sourced entries tagged
with their key; Enter re-sends the picked prompt; the full history is
persisted per terminal as `terminal-msgs:*` and survives restart, and
the pinned `you ▸` recap is just its latest entry), `]]u` scans the
visible terminal for `http(s)://…` URLs and opens the picked one in
the browser (#596 — a single on-screen URL opens straight away, else a
picker lists them newest-first so `]]u`+Enter opens the last; an
emulator-independent path that sidesteps right-click / mouse-capture
quirks, and `target_at` now stitches soft-wrapped URLs so a right-click
on any row of a wrapped link resolves the whole URL). A link under the
cursor also opens on **right-click** or an **Alt/Ctrl + left-click**
(#842) — the modifier-click is forwarded far more consistently than a
bare right button (many emulators bind right-click to their own context
menu), so it's the reliable mouse path (Alt/Ctrl ride the SGR mouse
report; Cmd isn't encoded and most emulators eat it, so don't rely on
it); both need capture on, while `]]u` works even with capture off.
`]]f`
toggles focus mode, `]]v` cycles the focus-mode layout (Single →
SplitV → SplitH → Grid, #1258 — persists `ui.focus_layout`; only live
in focus mode, and in a multi-pane layout `]]<arrow>` / `]]<digit>` /
`]]z` become pane-focus / pane-retarget / pane-zoom, the popup rows
following suit), `]]q` exits to the sidebar, `]]<digit>` jumps to
the Nth focused (starred) workspace, and `` ]]` `` opens the fuzzy workspace
switcher. The snippet picker (see
[`docs/snippets.md`](docs/snippets.md)) is a category-grouped list with
a live body-preview pane, filtering on key+description+category, that
auto-submits when the typed key uniquely matches (`]]srev`); `Enter`
sends the highlighted snippet, `Shift-Enter` inserts its body into the
composer without submitting so you can edit first (#791); snippets
you've sent float into a "Recent" group at the top (MRU,
`recent_snippets`, persisted in the state DB across restarts) so a
repeat is `]]s`+Enter. The leader
is non-timed, so after `]]` it waits for the command key rather than
leaving on an idle timer — browsing snippets never races an exit; `Esc`
or any unbound key cancels back to the terminal. A lone `]` followed by
any non-`]` key is sent to the agent verbatim. `Ctrl-c`
is forwarded as an interrupt. Tile management rides the same leader
(#286): `]]|` / `]]-` split, `]]<arrow>` moves tile focus (cycles
tabs in Tabs mode), `]]x` closes the focused terminal (tile or active
tab) — `Ctrl-w` is no longer a lazybox prefix and reaches the inner
program (readline word-erase). `]]t` toggles whether a new shell/agent
opens as a split or a tab (#361), persisting `ui.terminal_new_layout`;
the `]]` popup's `t` row shows the current setting.
`Shift-PgUp/PgDn` scroll the
scrollback, `Shift-Home/End` jump top/bottom (mouse wheel works too).

## Conventions

- `thiserror` for errors in library crates, `anyhow` in the binary (`tui`)
- No `unwrap()` in library crates
- Core-library layering: core and auth depend on no internal crate; store may
  depend on core only (enforced by the workspace dep-rules test)
- Provider crates depend on core + auth only
- Every public function has a test; visually complex TUI components carry
  insta render snapshots (sidebar, themes) and the rest get ratatui
  `TestBackend` render tests; every bug fix lands with a regression test
