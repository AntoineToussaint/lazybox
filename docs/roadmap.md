# Roadmap

What lazybox is working on, in order. The coordination design this builds on
is [`agent-coordination-v2.md`](agent-coordination-v2.md); this file is the
sequence. Update it in the PR that finishes, reorders or adds an item — an
agent reading a stale roadmap starts the wrong work.

Status: `now` · `next` · `later` · `done`.

## 1. Ship what is built — `done` (0.1.18)

PR #1890 is a month of fixes that no one is running: the delivery owner and
real turn results, clickable header/footer/overview, the `in-flight` filter,
pricing (Opus 5.5, 1-hour cache writes, fast mode, OpenAI cached input),
keep-awake linger and restart handoff, `start_workspace`, `answer_session`,
and the lost-paste recovery.

- Mark #1890 ready, merge it, cut **0.1.18** (`make cut-release`; the manual
  checks are the user's to attest).
- Done when: 0.1.18 is released and installed.

## 2. TODO, phase 1 — `now`

The Hopper becomes **TODO** — the user's own cross-project juggler and the
plan agents read (design §"Plan = the TODO tree").

- Rename Hopper → TODO everywhere (UI, keys, docs, config — keep the old
  config key readable).
- Sub-TODOs: plain checklist items under a TODO, cheap (no workspace each).
  Any item can **link** to a workspace, issue or PR; `Enter` jumps there.
- Progress like GitHub task lists: `▰▰▱ 2/3` per line, a total at the top.
- Auto-check, on by default: an item linked to a PR checks when it merges, to
  an issue when it closes.
- Task store with immutable ids — the base every later phase builds on.
- Done when: a TODO with nested items, links and live progress round-trips
  through a daemon restart, and a merged PR checks its item off.

## 3. Coordination, rest of phase 2 — `later`

- `complete_task(id, summary, artifacts)`; `reply_request` becomes a case of
  it.
- Move the remaining delivery paths onto the delivery owner (auto-fix,
  resume, credit recovery, broadcast) so every one returns a receipt, and
  show delivered / queued / refused in the TUI.
- Fix: requests and notes lost on the issue→PR fold; a role set after spawn
  never reaching the agent; `list_sessions` hiding exited agents.

## 4. Lean agent context, phase 3 — `later`

- `lazybox_guide(topic)` on demand; the always-on briefing cut to ~1.5 KB
  with its budget test.
- ~30 MCP tools folded to about eight (`start_workspace` → `delegate`,
  `answer_session` into `ask`/`deliver`).
- The same verbs as `lazybox task …` CLI for agents without MCP.

## 5. Shared state, phase 4 — `later`

Subscriptions with a snapshot first; "PR #N just merged, touching these
paths" change notices; the notes board rebuilt as typed entries (fresh, old
board read-only); file claims that warn on overlap.

## 6. Across machines, phase 5 — `later`

Tasks and subscriptions over the relay; the task record as the remote-handoff
unit.

## Bugs and debts, worked alongside

- **Keys not reaching a stuck agent** (2026-09-28): on #1855 no keystroke
  reached the daemon after a TUI reconnect; suspected a pane wrongly marked
  exited. Needs a reproduction.
- **Test runs rotate the live `/tmp/lazybox.log`**, hiding the running
  daemon's log.
- **Working claims as comments, not labels**; an **Interrupted** agent state
  (today an interrupt reads as Done or as a question).
- **Disk**: reap `target/` of merged and closed workspaces.
- Glyphs with several meanings (`◆` five, `⚠` `◔` `✓` two each) — a decision
  for the user.
