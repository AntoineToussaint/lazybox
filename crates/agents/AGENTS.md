# Agents

The `Agent` trait plus the built-in Claude / Codex / Cursor / `GenericCli`
implementations, the state machine that reads what an agent is doing, and the
briefing every spawned session starts with.

Read [`AGENTS.md`](../../AGENTS.md) first; this file only adds agent depth.
Adding a new agent is a procedure — see the `add-an-agent` skill.

## The trait is the extension point

An `Agent` supplies its id, spawn and resume argv, state detection, optional
hook config, prompt injection, and `gateway_injection` (how a metering or
gateway base URL reaches it — an env var for Claude and Cursor, `-c` provider
flags for Codex). `registry()` returns the built-ins; `GenericCli` already
covers arbitrary CLIs from YAML without recompiling, so a new built-in needs a
reason `GenericCli` cannot serve.

## Agent state is a control signal, not a display hint

`detect.rs` decides whether an agent is Working, Idle, InputNeeded, blocked on
a usage limit, and so on. `InputNeeded` in particular is load-bearing: it
releases the inject gate and drives the `?` jump, so a false positive is
unclearable by the user and lets a settle-gated inject land mid-turn. Any new
detection rule must also be taught to `dialog_marker_pos` — a rule that lands
in only one of the two disagrees with itself.

Treat a detection change as behaviour, not heuristics tuning: it needs a
regression test over the real byte stream that fooled it.

A **sticky** state has a second half that is easy to miss: add it to
`is_blocked` in `state_machine.rs`, or the end-of-turn settle rule rewrites a
clear reading arriving from `Working` straight back to `Done`. That is what
made `Stalled` (#1782) necessary in the first place — `Done` meant both
"finished the task" and "gave up after a 502", so a turn that died on a
gateway failure rendered exactly like one that succeeded.

Anchor a new screen marker on a machine-rendered result cell — line-leading,
after the agent's own cell glyph (Claude `⏺`, Codex `■`), outside any markdown
fence. Agents here routinely print, diff and quote error strings, so a bare
substring table has them classifying each other as broken.

## Model tiers

Tiers are declared per agent under `agents.<id>.models` in YAML — an ordered
`alias → { label, args }` menu plus a `default` tier for bare spawns. Claude and
Codex ship built-in menus; other agents declare their own. The rules that are easy
to get wrong:

- A user `models:` block **overlays** the built-in menu — a declared alias
  replaces the same-alias tier in place, a new alias appends. `replace: true`
  takes the block as the whole menu, which is the only way to express a
  *restricted* set.
- `excluded_from_default` keeps a tier off every bare spawn; a user block
  never *inherits* a capability mapping onto such a tier it did not declare.
  Writing-class models must not be reachable by a coding task's label.
- Every built-in Claude and Codex spawn — PTY, resume, or structured/headless —
  passes an explicit model from lazybox's resolved default tier. A missing,
  dangling, or model-less tier refuses the launch instead of inheriting a
  provider CLI/account default. Claude config load warns when this pin
  disagrees with the user's ambient setting, because the override is otherwise
  invisible.
- A task's `model:<token>` label (or `@model:` body marker) resolves through
  alias, then label, then pinned id. The legacy `best`/`high`/`medium`/`low`
  spelling names urgency but selects a model; `model:` outranks it.
- Declarations resolve as **precedence ranks**, not a single winner: a token
  this agent has no tier for falls through to the next rank, and a rank whose
  members name different tiers selects nothing. GitHub does not promise label
  order, so never let it pick the model.
- An **untrusted** spawn — triggered by someone other than the viewer — reads
  labels only. A label is write-gated; an issue body is not.

Capability tiers are about model capability. Nothing ranks, queues or
schedules work by them; the genuinely-ranking `Priority` on `Task` is a
different type that merely shares the word.

## The session briefing

`session_context.rs` is what every spawned session reads before its first
prompt — in any repo, including ones with no agent-context file of their own.
It is user-visible text with tests over it (`crates/core/tests/`), so treat a
wording change as a behaviour change: it must not promise a workspace for a
filed issue, and it must keep naming the coordination tools, since a session
that does not know they exist will not look for them. The base half also has
to keep pointing at `.lazybox/task.json` and saying not to `gh issue view` the
record it already holds — the GitHub budget it protects is the daemon's own
(#1799). Global response/formatting rules live here too, once per session;
snippets carry only task-specific instructions and must not append a second
contract later in the turn.

PTY starts deliver the briefing through Claude's context hook, or native
startup arguments when there is no hook: Codex's `developer_instructions`
override and Claude's `--append-system-prompt`. This covers bare starts with
no task yet. Other adapters prefix their initial task; structured/headless
runs prefix the first input at the provider boundary. Codex's injected
`developer_instructions` is owned by Lazybox for these launches; repository
`AGENTS.md` guidance still loads normally.
