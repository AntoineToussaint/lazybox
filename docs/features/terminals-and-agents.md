# Embedded terminals & agents

Each workspace can host one or more embedded terminals running a shell or a
coding agent (Claude Code, Codex, Cursor, or a generic CLI) inside its git
worktree. This is lazybox's highest-churn area — agent state detection and the
structured runtime are where focused testing pays off most.

See [`DESIGN.md` § Agent abstraction / LLM gateway](../../DESIGN.md),
[`ROADMAP.md` §1–3](../../ROADMAP.md), and
[Workspaces & worktrees](workspaces-and-worktrees.md) for the worktree a
session runs in.

---

## Embedded terminal

**Status:** stable
**Crate(s):** `tui-term`, `libghostty-vt` / `libghostty-vt-sys` (vendored), `agents` (tmux wrapper)
**Config / flags:** `terminal.escape_char` (default `]`)
**Key bindings:** see [Terminal interaction model](#terminal-interaction-model)

### What it does
Renders a real terminal inside the TUI — a PTY parsed by libghostty's VT engine
and drawn into ratatui. The daemon owns the PTY; the client replays bytes, so a
reconnecting client reconstructs the screen.

### How to use it
Spawn a shell or agent on a workspace (`s`, `a c`/`a x`/`a u`, or `w w`); the terminal
appears in the right-hand stack. Multiple terminals coexist per workspace.

### How it works (brief)
`TermSession` (`crates/tui-term/src/lib.rs`) wraps `portable-pty` + a
libghostty-vt parser behind a `Mutex`, read on a `std::thread`. The daemon keeps
a per-terminal **2 MiB ring buffer** (`REPLAY_RING_BYTES`,
`crates/server/src/pty.rs`); on `Subscribe` it replays the ring then
streams live `TerminalOutput` bytes. The default session backend is tmux
(`TmuxBackend`, `crates/server/src/backend/tmux.rs`), swappable via the
`SessionBackend` trait (`crates/server/src/backend/mod.rs`).

### Test checklist
- [ ] A spawned shell renders a working prompt and echoes input.
- [ ] Detaching/reattaching a client replays the screen from the ring buffer.
- [ ] A full-screen program (vim/less) renders correctly.
- [ ] Killing a session terminates its tmux session.
- [ ] Output beyond the 2 MiB ring scrolls without corrupting the live screen.

### Known sharp edges
- `make setup` fetches the pinned Ghostty source and Zig packages once into a
  shared host cache; subsequent `make release` builds run offline.
- tmux ≥ 3.3 is required for persistent sessions (`allow-passthrough`
  and friends in the transparent conf; older tmux turns an unknown conf
  option into a config-error view that blocks every headless attach).
  Older or missing tmux falls back to the ephemeral raw-PTY backend.

---

## Interactive & browser-based auth in terminals

**Status:** stable
**Crate(s):** `server` (`pty.rs`, `backend/tmux.rs`, `backend/raw_pty.rs`), `tui-core` (`editors.rs`)
**Config / flags:** —
**Key bindings:** `]]u` open a URL from the terminal on the client machine

### What it does
The embedded terminal is a **real PTY** — the child's stdin/stdout are an
`openpty` slave, so `isatty()` is true and the process' environment is inherited
wholesale (lazybox forces only `TERM`/`COLORTERM` and never clears the env).
Ordinary interactive commands therefore behave exactly as in any terminal:
`read -p`, a password prompt, or pasting a verification code all work.

The one flow that does **not** "just work" is a CLI that opens a browser and/or
binds a **localhost OAuth callback** — `gcloud auth application-default login`,
`gcloud auth login`, `gh auth login`'s web flow, `vercel login`, and the like.
lazybox has no browser-launch code in the agent PTY path, so the CLI relies on
its own inherited env (`BROWSER`, `DISPLAY`, a macOS GUI session) to spawn one.
That launch — and the `127.0.0.1` callback the CLI listens on — happens on the
machine running the **daemon**, which for a remote session (`lazybox --connect`,
see [Remote connect](daemon-and-deployment.md#remote-connect)) is the box, not
your laptop, and for a detached/headless daemon may have no browser at all.
This is inherent to browser + localhost-callback OAuth over a remote daemon, not
lazybox stripping TTY interactivity.

### How to use it
Run the **browserless** variant so the CLI prints a URL and reads the code back
over stdin, then open that URL on your own machine:

- `gcloud auth application-default login --no-launch-browser`
- `gcloud auth login --no-launch-browser`
- `gh auth login` → pick the "Paste an authentication token" path
- generally look for `--no-launch-browser`, `--no-browser`, or `--device-code`

Press `]]u` to open the printed URL — it scans the visible terminal and opens
the link on the **client** (your laptop), not the daemon host — complete the
flow there, and paste the verification code back at the prompt. Run these in a
shell (`s`) or via Claude's `!` bash line, **not** as an agent tool call: the
agent's non-interactive Bash tool can't paste the code back and will hang on the
prompt. When the daemon is local the default browser-launching flow already
works; the browserless path is what you reach for on a remote or headless box.

### How it works (brief)
`DaemonPty::spawn_inner` (`crates/server/src/pty.rs`, the raw-PTY backend's
spawn path) opens the PTY and inherits the daemon env, adding only
`TERM=xterm-256color` / `COLORTERM=truecolor`; the default tmux backend seeds
`COLORTERM` the same way (`-e`) and sets `TERM` from its `default-terminal`
conf. Browser launching lives only host-side
in the client (`browser_argv` / `open_url`, `crates/tui-core/src/editors.rs`),
which `]]u` drives (and `g o` for a workspace's PR/issue page) — never inside
the agent PTY.

### Test checklist
- [ ] An interactive prompt (`read -p "x: " v`, a password) works in an embedded terminal.
- [ ] `gcloud auth application-default login --no-launch-browser` prints a URL and accepts a pasted code.
- [ ] `]]u` opens the printed auth URL on the client machine, not the daemon host.
- [ ] With a remote daemon, the default (browser-launching) flow targets the box — the browserless variant is the working path.

### Known sharp edges
- The agent's own non-interactive Bash tool can't complete a code-paste flow — run auth in a shell (`s`) or via `!`.
- A detached/headless daemon may have no browser or GUI session; always prefer the browserless flag there.

---

## Spawn shell & agents

**Status:** stable
**Crate(s):** `agents` (`src/agent.rs`), `server` (`spawn_handler.rs`)
**Config / flags:** `setup.agents` (enabled), `setup.default_agent`, `ui.action_keys` `spawn_agent.<id>` entries (remap agent chords)
**Key bindings:** `s` shell; `a` agent leader — `a c` Claude, `a x` Codex, `a u` Cursor (agents without a built-in key convention are remappable via `spawn_agent.<id>`)

### What it does
Launches a shell or a coding agent in the focused workspace's worktree. Built-in
agents: Claude Code, Codex, Cursor Agent, and a YAML-configured GenericCli for
anything else.

### How to use it
On a workspace: `s` opens a shell; `a` opens the agent which-key group —
`a c` Claude, `a x` Codex, `a u` Cursor. The agent list and their chords come
from your enabled agents; `setup.default_agent` is what
[`w w`](#work-command) uses.

### How it works (brief)
The `Agent` trait (`crates/agents/src/agent.rs`) defines `spawn`/`resume` argv,
`detect_state`, `detect_ready_for_prompt`, and prompt injection. Built-ins:
Claude (`claude`, hooks-based state), Codex (`codex`), Cursor (`cursor-agent`),
GenericCli (user YAML). The server registers built-ins plus every
`agents.<id>` entry with a `command` at startup; `spawn_handler` builds the
worktree, env, and `SpawnCtx`, then launches through the tmux wrapper.

### Test checklist
- [ ] `s` opens a shell in the correct worktree dir.
- [ ] `a c` / `a x` / `a u` launch the respective agent if its binary is on PATH.
- [ ] A GenericCli agent defined in config launches with its configured argv.
- [ ] An agent missing from PATH fails gracefully (no crash).
- [ ] Resume argv (`claude --continue`) reattaches rather than starting fresh where supported.

### Known sharp edges
- Agent availability is PATH-detected; an agent installed mid-session isn't picked up without a re-detect.

---

## "Work" command

**Status:** stable
**Crate(s):** `tui-core` (`src/prompts.rs`, `src/intent.rs`), `core` (`src/prompts.rs`, `prompts/agent-work.md`)
**Config / flags:** uses `setup.default_agent`
**Key bindings:** `w` menu (`w w` default, `w c`/`w x`/`w u` explicit)

### What it does
Spawns the default agent with a **state-aware prompt** chosen from the
workspace's situation: fix a merge conflict, fix failing CI, address selected
review comments, or implement an issue.

### How to use it
Press `w w` on a workspace. With Activity rows multi-selected (`v`), `w w` targets
those comments ("address these comments"). Otherwise it picks the prompt by
priority: conflict → CI failure → issue-implementation / review.

### How it works (brief)
Priority chain in `crates/tui-core/src/prompts.rs`: `build_fix_conflict_prompt`
→ `build_fix_ci_prompt` → `build_implement_issue_prompt`
(`crates/core/src/prompts.rs`) / `build_address_comments_prompt`
(`crates/tui-core/src/intent.rs`). All include `AGENT_WORK_PREAMBLE` from
`crates/core/prompts/agent-work.md` (the no-destructive-shortcuts /
root-cause-over-masking principles).

### Test checklist
- [ ] On a PR with a conflict, `w w` produces the rebase/resolve/force-push prompt.
- [ ] On a PR with failing CI (no conflict), `w w` produces the fix-CI prompt.
- [ ] On an issue workspace with no PR, `w w` produces the implement-issue prompt.
- [ ] With comments selected (`v`), `w w` produces the address-comments prompt scoped to the selection.
- [ ] Every prompt carries the work preamble.

### Known sharp edges
- Priority order means a PR with *both* a conflict and CI failure gets the conflict prompt first.

---

## Autonomous sessions

**Status:** beta
**Crate(s):** `server` (`spawn_handler.rs`), `agents`, `config`
**Config / flags:** `agent.autonomous_skip_permissions` (default `true`), `agent.skip_permissions` (default `false`), `auto_fix.*`, `mention.*`
**Key bindings:** — (triggered by `@lazybox` mentions / auto-fix)

### What it does
When lazybox picks up `@lazybox`-triggered work, it spawns the agent **unattended**.
Since no human is there to approve tool-use prompts, those sessions launch Claude
with `--dangerously-skip-permissions` and the tab strip flags them with a
compact `⚠` glyph. Sessions you open yourself keep approval prompts on by
default. Focusing a flagged tab spells the meaning out in a one-shot footer
hint, so the terse glyph never leaves a user guessing.

### How to use it
Mention `@lazybox` in a GitHub issue or PR comment (gated by
`mention.allowed_logins`) to kick off autonomous work. The Slack mirror cannot
spawn agents — inbound Slack messages only forward text to an
already-running agent's terminal or answer status queries. To force prompts even on autonomous runs, or to opt your own
interactive sessions into bypass:

```yaml
agent:
  autonomous_skip_permissions: false  # prompt even autonomous runs
  skip_permissions: true              # bypass on sessions you open too
```

The "Skip permission prompts for your sessions" toggle in Settings flips
`skip_permissions`.

### How it works (brief)
`skip_permissions_for(autonomous, cfg)` (`crates/server/src/spawn_handler.rs`)
returns `autonomous_skip_permissions` for autonomous spawns, else
`skip_permissions`. The result rides in `SpawnCtx.skip_permissions`; Claude adds
`--dangerously-skip-permissions` when set. The `⚠` badge renders from
`TerminalMetadata::skip_permissions` (`crates/tui/.../terminal_stack.rs`). Blast
radius is bounded to the per-task worktree; the work preamble is the in-prompt
counterweight to relaxed approvals.

### Test checklist
- [ ] An autonomous spawn launches Claude with `--dangerously-skip-permissions` and shows the `⚠` badge.
- [ ] A session you open with `c` does *not* skip permissions by default (no badge).
- [ ] `autonomous_skip_permissions: false` makes autonomous runs prompt (no bypass).
- [ ] `skip_permissions: true` makes your own sessions bypass (badge appears).
- [ ] The Settings toggle flips `skip_permissions` and persists.

### Known sharp edges
- Claude Code refuses to start in bypass mode as root/sudo (worktree sessions don't hit this).
- Bypass mode removes the human-in-the-loop guard — only the worktree boundary and the work preamble remain.

---

## Agent state detection

**Status:** stable
**Crate(s):** `agents` (`src/detect.rs`, `src/state_machine.rs`, `tests/detect_fixtures.rs`), `ipc` (`AgentState`)
**Config / flags:** `agent.quiet_classify_secs` (quiet-timer → `Done` window, default 5), `agent.working_watchdog_secs` (stuck-`Working` fail-safe, default 15, `0` disables)
**Key bindings:** `!` jump to next waiting agent, `Shift-F` jump to next failing-CI PR

### What it does
Tracks whether an agent is **Working**, needs input (**InputNeeded**), or has
finished a turn (**Done**) so the sidebar can accurately badge sessions and `!`
can jump to the next one needing you. Structured lifecycle hooks are
authoritative where the agent provides them; tested terminal-state detection
and settle invariants cover the remaining paths.

### How to use it
Watch the sidebar agent-state chips. Press `!` to move the cursor to the next
workspace whose agent is waiting on input.

### How it works (brief)
Pure detectors in `crates/agents/src/detect.rs`. Claude's detector
(`claude_state`) strips ANSI, then looks for a **live chooser** (`❯` + numbered
options) or a **permission footer** (`esc to cancel` paired with choices),
guarded by *recency* against the idle composer footer so a parked prompt above a
prose list doesn't false-positive (#156/#163). "Working" is detected from the
live status line (`esc to interrupt`, or `·` + token counter). Codex/Cursor use
narrower yes/no pattern sets; GenericCli matches user-supplied
`asking_patterns`. Recent work (#153) added real ready/working-screen detection
so prompt injection no longer rides a fixed 10s deadline. Real-PTY transcripts
back the fixtures in `tests/detect_fixtures.rs`. Readings fold into an explicit
state machine (`src/state_machine.rs`) instead of overwriting the badge directly:
`Working` is a one-way door that only leaves for `Done`/`InputNeeded`/`Exited`.
`Done` is reachable for *every* agent (not just Claude's `Stop` hook) via a
generic quiet timer — after `agent.quiet_classify_secs` of no PTY output a
`Working` turn settles to `Done`, even when the resting screen matches no marker.
This settle is authoritative even on a **hook-driven** terminal whose last hook
is still fresh (#504): a busy agent repaints its status ticker within the quiet
window, so true byte-silence proves the turn ended regardless of whether a `Stop`
hook ever fired (a manual Ctrl-C/Esc, a lost hook). The whole hooks-vs-PTY gate —
which readings a fresh hook may override and which it may not — lives in
`AgentStateMachine::on_pty_reading` (`hooks_gate_allows`), not in the daemon's
output pump; the watchdog's *content-stability* force stays hook-subordinate,
since a ticking counter can mask a genuinely long silent tool call.

### Test checklist
- [ ] A Claude permission prompt flips the row to InputNeeded.
- [ ] A streaming/working Claude shows Working, not InputNeeded.
- [ ] A parked numbered list in prose does *not* false-trigger InputNeeded (recency gate).
- [ ] `!` jumps to the next workspace with a waiting agent.
- [ ] Codex/Cursor yes/no prompts are detected as InputNeeded.
- [ ] `Shift-F` jumps to the next PR with failing/mixed CI.

### Known sharp edges
- Generic CLIs rely on their configured `asking_patterns`; add a pattern when a
  custom agent uses a prompt lazybox cannot identify.
- Novel third-party agent UI changes can require a detector fixture update.

---

## Structured agent runtime

**Status:** experimental
**Crate(s):** `server` (`agent_runs.rs`, `agent_stream.rs`), `ipc`
**Config / flags:** — (driven over IPC / the JSON API)
**Key bindings:** — (not yet a TUI surface; for non-terminal clients)

### What it does
Runs supported agent CLIs behind provider-neutral structured commands/events
instead of a raw PTY, so non-terminal clients (Ask Lazybox, Tauri, iOS, the JSON
API) can render assistant text, tool calls, approvals, and questions as data.
Claude Code's persistent `stream-json` mode and Codex's turn-based `exec --json`
mode both map to the same lazybox IPC event vocabulary.

### How to use it
Not a TUI key — exercised via IPC / the [JSON API](daemon-and-deployment.md#json-http-api-gateway):
`StartAgentRun` (mode `StreamJson`), `SendAgentInput`, `InterruptAgentRun`,
`DecideAgentApproval`, `AnswerAgentQuestion`.

### How it works (brief)
`handle_start_agent_run` (`crates/server/src/agent_runs.rs`) launches
the structured adapter declared by the selected `Agent`: Claude stays in one
bidirectional process, while Codex exits after each turn and lazybox resumes its
thread for follow-ups while preserving one logical run id. Both emit
`AgentRunStarted`, `AgentAssistantTextDelta`, `AgentToolUse*`,
`AgentPermissionRequest`, `AgentTurnFinished`, `AgentRunFinished`, plus
`AgentRawJson` (always forwarded so clients can adopt new fields first). A
provider completion finishes a *turn*; only ending the logical adapter finishes
the *run*.

### Test checklist
- [ ] `StartAgentRun` with `StreamJson` starts a run and emits `AgentRunStarted`.
- [ ] Assistant text arrives as `AgentAssistantTextDelta`.
- [ ] Tool calls surface as `AgentToolUse*` events.
- [ ] `InterruptAgentRun` stops the run and cleans up the child.
- [ ] Raw JSON is forwarded for every event.
- [ ] A `result` line emits `AgentTurnFinished`, not `AgentRunFinished`.
- [ ] A Codex follow-up uses `exec resume` without changing the lazybox run id.

### Known sharp edges
- Foundation only (ROADMAP §1–3): structured runs aren't persisted yet, so a reconnecting client can't rediscover an active run.
- Token/cost accounting from the stream isn't fully wired here yet.
- Claude and Codex are currently the structured adapters; other enabled agents
  continue to work in PTYs and fail structured starts with a capability error.
- Only `StreamJson` mode is handled; `Terminal` mode goes through the [terminal path](#embedded-terminal).

---

## LLM gateway

**Status:** shipped
**Crate(s):** `server` (`gateway_env_for_agent`), `agents` (`LlmProvider`), `config` (`agent.llm_gateway_url`)
**Config / flags:** `agent.llm_gateway_url` (global), per-repo `env` overrides
**Key bindings:** —

### What it does
Points a spawned agent at an operator-provided LLM gateway by injecting a single
base-URL env var — `ANTHROPIC_BASE_URL` for Anthropic agents (Claude),
`OPENAI_BASE_URL` for OpenAI agents (Codex / Cursor). lazybox does **no
proxying itself**: there is no in-process HTTP server and no telemetry capture
from agent API traffic — the agent talks to the configured gateway directly.

> An earlier design ran an in-process 127.0.0.1 telemetry proxy (the `llm-proxy`
> crate); it was never wired up and has been removed. See the superseded section
> in `DESIGN.md` if structured telemetry is revived.

### How to use it
Set `agent.llm_gateway_url` in `~/.lazybox/config.yaml`. It's transparent to the
agent — the daemon adds the env var at spawn time. A per-repo `env` entry still
wins, so a repo can override or opt out.

### How it works (brief)
`gateway_env_for_agent` (`crates/server/src/spawn_handler.rs`) maps the agent's
`LlmProvider` to the right base-URL env var (`LlmProvider::base_url_env`) and
sets it to `agent.gateway_url()`. Returns nothing for non-agent spawns, agents
with no inferable provider (`GenericCli`), or when no gateway URL is set.

### Test checklist
- [x] A spawned Anthropic agent gets `ANTHROPIC_BASE_URL`; OpenAI agents get `OPENAI_BASE_URL`.
- [x] No env var is injected when `agent.llm_gateway_url` is unset.
- [x] A per-repo `env` override takes precedence over the global gateway.
- [x] `GenericCli` / non-agent spawns get nothing.

### Known sharp edges
- It only sets a base-URL env var; the gateway itself (routing, auth, telemetry) lives outside lazybox.
- An agent that ignores the base-URL env var, or whose provider can't be inferred (`GenericCli`), simply talks to its default upstream.

---

## Snippet workflows

**Status:** stable
**Crate(s):** `config` (`src/snippets.rs`), `tui` (picker)
**Config / flags:** `~/.lazybox/snippets.yaml` (global) + `<launch-dir>/.lazybox/snippets.yaml` (client-wide directory override)
**Key bindings:** `]]s`, then the snippet key (configurable escape char)

### What it does
Turns recurring agent instructions into reusable workflows that expand and
auto-submit to the focused terminal. The picker remembers recently used
workflows globally, while each workspace persists its 12 most recently
distinct snippet keys and renders that bounded count as a `]N` sidebar badge.

### How to use it
Open the categorized picker with `]]s`, inspect the live body + origin preview,
then select a row or type a uniquely matching key such as `rev` for the
`]]srev` fast path. Open `]]s` later and the last workflow is selected in
**Recent**, ready to repeat with `Enter`.

lazybox ships a built-in library. Add global or launch-directory workflows in
YAML:

```yaml
snippets:
  rev:
    description: "Ask for a review"
    body: "Please review the current diff and flag bugs."
```

In a terminal, press `]]s` then start typing the snippet key; the picker
filters by key, description, and category, and auto-submits when an exact key is
the unique key-prefix match. Ask Lazybox (`?`) can propose a global add/update,
confirm it with a body preview, and hot-reload it immediately. `Shift-B`
broadcasts a chosen workflow across selected workspaces. See
[`docs/snippets.md`](../snippets.md) and the
[multi-agent orchestration guide](https://lazybox.ai/docs/how-to/orchestrate-multiple-agents/).

### How it works (brief)
`Snippets::load_for_launch_dir` (`crates/config/src/snippets.rs`) merges
built-in, global, and launch-directory layers into one client-wide catalog.
The terminal's doubled
escape leader (`]]`) followed by `s` mounts the snippet picker
(`crates/tui/.../keys.rs`). The daemon persists the global Recent MRU in client
KV and per-workspace keys in the workspace record.

### Test checklist
- [ ] A global snippet expands and submits in a terminal via `]]s<key>`.
- [ ] A launch-directory snippet overrides a global one for the whole client.
- [ ] The picker filters as you type and auto-submits an exact, unique key.
- [ ] Recent survives a restart and selects the most recently sent workflow.
- [ ] Sending distinct keys updates the target workspace's `]N` badge up to its 12-key cap.
- [ ] Ask Lazybox confirms, writes, and hot-reloads a global snippet.
- [ ] A snippet-seeded broadcast records the key only on delivered targets.
- [ ] `Esc` dismisses the picker without sending.

### Known sharp edges
- The trigger reuses the terminal escape char; if you remap `terminal.escape_char`, the snippet trigger moves with it.
- The directory layer is chosen from the client's startup directory and does not change with sidebar workspace selection.

---

## Agent-to-agent handoff (`x s` send to session)

**Status:** stable
**Crate(s):** `tui-core` (`SendToSession` in `src/action.rs`), `tui`, `server`
**Config / flags:** —
**Key bindings:** `x s`

### What it does
Hands work from one running agent to another (#431): captures the focused
agent's on-screen output, lets you pick a target workspace, edit the brief,
and injects + submits it into that workspace's agent session.

### How to use it
With the source agent's terminal focused (or its workspace selected), press
`x s`. Pick the target workspace from the picker — the source workspace is
excluded, so a handoff can't loop back to itself — then edit the pre-filled
brief and submit. A visible `source → target` footer notice records the trail.

### How it works (brief)
`SendToSession` (`crates/tui-core/src/action.rs`) drives a
capture → pick → compose flow; delivery reuses the settle-gated prompt inject
so the brief lands when the target agent is ready for input.

### Test checklist
- [ ] `x s` captures the focused agent's output into the compose textarea.
- [ ] The target picker excludes the source workspace.
- [ ] Submit injects and sends the brief into the target's agent.
- [ ] A `source → target` notice appears after delivery.

---

## Terminal interaction model

**Status:** stable
**Crate(s):** `tui` (`realm/model/keys.rs`, `components/terminal_stack.rs`)
**Config / flags:** `terminal.escape_char` (default `]`), `ui.terminal_new_layout` (`split` default / `tabs`)
**Key bindings:** `]]` exit to sidebar, `Tab` focus (pre-input), `Ctrl-c` SIGINT, `]]…` tile management, mouse wheel scrollback, drag-select → OSC 52 copy

### What it does
Defines how keys and the mouse behave while a terminal is focused: nearly
everything forwards to the PTY, with a small set of lazybox-level escapes for
leaving, splitting, scrolling, and copying.

### How to use it
- `]]` (two presses of the escape char) returns to the sidebar.
- `Tab` cycles focus only before you've typed in the current visit; after the first keystroke it routes to the PTY (autocomplete).
- `Ctrl-c` is forwarded as SIGINT.
- `]]` then `|`/`\` (split vertical), `-` (split horizontal), arrows (move tile focus / cycle tabs), `x` (close the focused terminal). `Ctrl-w` is not a lazybox prefix — it reaches the inner program (readline word-erase).
- By default a second terminal in a workspace opens side-by-side (a split tile). Set `ui.terminal_new_layout: tabs` to have ordinary `s`/agent spawns stack behind the tab strip instead — the existing tile keeps its full size. Explicit `]]|` / `]]-` splits are unaffected.
- `]]t` toggles that default live (split ⇄ tabs), persisting it to `ui.terminal_new_layout` so it survives restart; the `]]` popup's `t` row shows the current setting. The change affects the *next* spawn, not terminals already open.
- Mouse wheel always scrolls lazybox's local history (3 rows/notch), including
  mouse-tracking and full-screen agents. In a split, the wheel scrolls the tile
  **under the cursor**, not the focused one (#362). Mouse tracking still applies
  to clicks, but never redirects a wheel event into the inner program.
- Left-click + drag does pane-scoped selection; release copies via OSC 52 (footer shows `copied N lines`). For host-native selection across the whole screen, press `F8` to flip mouse capture off, drag, then `F8` back.
- Open a link under the cursor by **right-click** or, more reliably, an **Alt / Ctrl + left-click** (#842). Right-click is the least consistently forwarded button — many emulators bind it to their own context menu — so the modifier-click is an emulator-independent path to the same open-URL behavior. Alt and Ctrl are the modifiers that ride an SGR mouse report; Cmd (macOS) is *not* encoded in mouse reports and most emulators intercept Cmd-click themselves, so don't rely on it. Both paths require mouse capture on (`F8`); the keyboard `]]u` picker works even with capture off.
- **Per-emulator right-click note:** whether a bare right-click reaches lazybox depends on the host terminal. Ghostty and Terminal.app generally forward it once mouse reporting is enabled; iTerm2 shows its own context menu on right-click by default (use the Alt-click path or `]]u`, or disable "Report mouse clicks" overrides). When right-click does nothing and the footer shows `mouse ?` (capture requested but unverified), the emulator is eating the event — reach for `]]u` or an Alt/Ctrl-click.
- `Shift-PageUp/PageDown` and `Shift-Home/End` move through scrollback (focused tile).

### How it works (brief)
Terminal key routing lives in `crates/tui/src/realm/model/keys.rs`; tile
management and scrollback in `components/terminal_stack.rs`. The escape char is
`terminal.escape_char`. OSC 52 emission is `emit_clipboard_copy`.

Scrolling has one encapsulated owner (`TerminalVt::scroll`, the sole caller of
libghostty's `scroll_viewport`); every wheel/keyboard surface routes through it
and gets back a typed `ScrollOutcome`, so a scroll can never silently no-op. The
full model and its regression harness (`crates/tui/tests/terminal_scroll.rs`)
are documented in [`docs/terminal-scrolling.md`](../terminal-scrolling.md).

### Test checklist
- [ ] `]]` returns to the sidebar from a terminal.
- [ ] `Tab` on a fresh visit cycles panes; after typing, `Tab` reaches the shell.
- [ ] `Ctrl-c` interrupts the running program.
- [ ] `]]-` / `]]|` split the terminal stack; `]]x` closes a tile.
- [ ] Old/recovered sessions with local history scroll in-process.
- [ ] A freshly spawned agent scrolls its scrollback in-process (wheel / `Shift-PageUp` / `Shift-Home`) from the first frame — no forward to the app.
- [ ] In a split, the wheel scrolls the tile under the cursor, not the focused one (#362).
- [ ] Full-screen and mouse-tracking programs scroll lazybox history without receiving wheel input.
- [ ] Drag-select copies via OSC 52 and the footer confirms the line count.
- [ ] `F8` toggles mouse capture for host-native selection.
- [ ] Right-click and Alt/Ctrl + left-click both open the URL under the cursor; a modifier-click that misses a link starts an ordinary selection instead.

### Known sharp edges
- `]]…` tile management and `Shift-Home/End` scrollback aren't in the README key tables — discover them via the `]]` popup or help (`?`).
- OSC 52 copy depends on the host terminal honoring OSC 52 (most do; some need it enabled).
