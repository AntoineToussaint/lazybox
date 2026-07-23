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
a per-terminal **64 KB ring buffer**; on `Subscribe` it replays the ring then
streams live `TerminalOutput` bytes. The default session backend is tmux
(`TmuxBackend`, `crates/server/src/backend/tmux.rs`), swappable via the
`SessionBackend` trait (`crates/server/src/backend/mod.rs`).

### Test checklist
- [ ] A spawned shell renders a working prompt and echoes input.
- [ ] Detaching/reattaching a client replays the screen from the ring buffer.
- [ ] A full-screen program (vim/less) renders correctly.
- [ ] Killing a session terminates its tmux session.
- [ ] Output beyond 64 KB scrolls without corrupting the live screen.

### Known sharp edges
- `make setup` fetches the pinned Ghostty source and Zig packages once into a
  shared host cache; subsequent `make release` builds run offline.
- tmux ≥ 3.3 is required for persistent sessions (`allow-passthrough`
  and friends in the transparent conf; older tmux turns an unknown conf
  option into a config-error view that blocks every headless attach).
  Older or missing tmux falls back to the ephemeral raw-PTY backend.

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
GenericCli (user YAML). `registry()` (`crates/agents/src/lib.rs`) registers the
builtins; the server's `spawn_handler` builds the worktree, env, and
`SpawnCtx`, then launches through the tmux wrapper.

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
`⚠ no-perms` badge. Sessions you open yourself keep approval prompts on by
default.

### How to use it
Mention `@lazybox` (e.g. via the Slack mirror or a configured trigger) to kick off
autonomous work. To force prompts even on autonomous runs, or to opt your own
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
`--dangerously-skip-permissions` when set. The `⚠ no-perms` badge renders from
`TerminalMetadata::skip_permissions` (`crates/tui/.../terminal_stack.rs`). Blast
radius is bounded to the per-task worktree; the work preamble is the in-prompt
counterweight to relaxed approvals.

### Test checklist
- [ ] An autonomous spawn launches Claude with `--dangerously-skip-permissions` and shows the `⚠ no-perms` badge.
- [ ] A session you open with `c` does *not* skip permissions by default (no badge).
- [ ] `autonomous_skip_permissions: false` makes autonomous runs prompt (no bypass).
- [ ] `skip_permissions: true` makes your own sessions bypass (badge appears).
- [ ] The Settings toggle flips `skip_permissions` and persists.

### Known sharp edges
- Claude Code refuses to start in bypass mode as root/sudo (worktree sessions don't hit this).
- Bypass mode removes the human-in-the-loop guard — only the worktree boundary and the work preamble remain.

---

## Agent state detection

**Status:** beta
**Crate(s):** `agents` (`src/detect.rs`, `src/state_machine.rs`, `tests/detect_fixtures.rs`), `ipc` (`AgentState`)
**Config / flags:** `agent.quiet_classify_secs` (quiet-timer → `Done` window, default 5), `agent.working_watchdog_secs` (stuck-`Working` fail-safe, default 15, `0` disables)
**Key bindings:** `!` jump to next waiting agent, `Shift-F` jump to next failing-CI PR

### What it does
Infers whether an agent is **Working**, needs input (**InputNeeded**), or is
**Idle** from its terminal output, so the sidebar can badge waiting sessions and
`!` can jump to the next one needing you.

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

### Test checklist
- [ ] A Claude permission prompt flips the row to InputNeeded.
- [ ] A streaming/working Claude shows Working, not InputNeeded.
- [ ] A parked numbered list in prose does *not* false-trigger InputNeeded (recency gate).
- [ ] `!` jumps to the next workspace with a waiting agent.
- [ ] Codex/Cursor yes/no prompts are detected as InputNeeded.
- [ ] `Shift-F` jumps to the next PR with failing/mixed CI.

### Known sharp edges
- Detection is heuristic over rendered terminal bytes; novel agent UIs or themes can fool it. The structured runtime is the longer-term replacement.
- Codex/Cursor pattern sets are narrower than Claude's and miss custom prompt phrasings.

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

## Snippets

**Status:** stable
**Crate(s):** `config` (`src/snippets.rs`), `tui` (picker)
**Config / flags:** `~/.lazybox/snippets.yaml` (global) + `<repo>/.lazybox/snippets.yaml` (repo-local)
**Key bindings:** `]]s`, then the snippet key (configurable escape char)

### What it does
Configurable text shortcuts that expand and auto-send to the focused terminal /
agent — e.g. a canned "review this diff" prompt bound to a key.

### How to use it
Define snippets in YAML:

```yaml
snippets:
  rev:
    description: "Ask for a review"
    body: "Please review the current diff and flag bugs."
```

In a terminal, press `]]s` then start typing the snippet key; the picker
fuzzy-filters and auto-submits the body (with a trailing carriage return) when
the filter uniquely matches. See [`docs/snippets.md`](../snippets.md).

### How it works (brief)
`Snippets::load_merged` (`crates/config/src/snippets.rs`) merges global +
repo-local files (repo wins on key collision). The terminal's doubled escape
leader (`]]`) followed by `s` mounts the snippet picker
(`crates/tui/.../keys.rs`).

### Test checklist
- [ ] A global snippet expands and submits in a terminal via `]]s<key>`.
- [ ] A repo-local snippet overrides a global one with the same key.
- [ ] The picker fuzzy-filters as you type and auto-submits on a unique match.
- [ ] `Esc` dismisses the picker without sending.

### Known sharp edges
- The trigger reuses the terminal escape char; if you remap `terminal.escape_char`, the snippet trigger moves with it.

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

### Known sharp edges
- `]]…` tile management and `Shift-Home/End` scrollback aren't in the README key tables — discover them via the `]]` popup or help (`?`).
- OSC 52 copy depends on the host terminal honoring OSC 52 (most do; some need it enabled).
