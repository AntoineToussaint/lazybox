# The daemon

The server owns all state and IO — PTYs, provider polling, the store, agent
runs, the JSON API gateway. Clients are thin renderers over IPC. When you are
deciding where a behaviour belongs, it belongs here unless it is pure
presentation.

Read [`AGENTS.md`](../../AGENTS.md) first; this file only adds daemon depth.

## Client / daemon split

Same process by default — the transport is a tokio mpsc channel pair, no
serialization. Out-of-process mode uses a Unix socket (length-prefixed
bincode); SSH `-L` forwards it for remote use. The event bus is a
`tokio::sync::broadcast`: providers produce, subscribers (TUI clients, the API
gateway) consume.

The detached lane gives you **no command ordering**. Commands a client pushes
run as independent concurrent tasks, so `[A, B]` is not "A then B" — pass the
dependency in-band rather than sequencing two commands.

## The daemon is the terminal size authority

`pty.rs` and `terminal_io.rs` stamp every `TerminalOutput` chunk with the PTY
size it was read at, announce a resize in stream order as an empty chunk
carrying the new size, and attach `ReplaySizeSpan`s to ring replays. The
client's VT mirrors the PTY and is sized only from those stamps — never from
its pane, which drives `Command::Resize` alone. Bytes must parse at the size
they were laid out for; every client-side attempt to infer the size instead
produced duplicated lines in scrollback.

## Polling

`polling/scheduler.rs` runs tiers, not one interval: a hot set for live and
armed workspaces (15s), a per-repo rotation for the roster, and a periodic
unwindowed reconcile — the only pass allowed to retire rows. An armed
auto-merge rides the hot tier so green → merged is one tick, and leaves it
once GitHub-native auto-merge takes over.

Duplicate spawns are collapsed by the SpawnCoordinator rather than by the
callers, so a double-fire or an issue→PR rebadge never forks two backends.
Explicit spawn keys still force a new agent deliberately.

## Merge has two call sites

`polling/handlers.rs` (the interactive merge) and `polling/auto_merge.rs`
(merge-on-green) both wire merge and trailers. A change made in one of them
half-lands; put it in the provider, or change both.

Cost trailers (`pr_trailers.rs`, format contract in
`lazybox_core::pr_trailers`) are permanent and public, which is why
`providers.github.pr_trailers` gates them. Three invariants the code exists to
hold: `commitBody` *replaces* GitHub's default squash log, so the default is
read back and appended to — a body that cannot be resolved merges with **no**
trailer rather than a truncated log; an unmetered PR gets no `Lazybox-Cost`
line at all, never `$0.00`; and cost is billed per PR, not per workspace (the
`meter-cost-mark:` watermark is stamped at merge).

## GitHub-native auto-merge is gated on coverage

GitHub's auto-merge waits on *required* checks and reviews alone, so handing
it a PR whose other checks are failing would land it red. Native is armed only
where GitHub's gate provably covers lazybox's, and never where lazybox holds
the PR for a reason GitHub cannot see (epic merge order, an unlanded
predecessor, a blocking review verdict, an open stacked parent, a
human-approval repo). Every decline surfaces as a footer notice rather than a
swallowed provider error, and the check is re-run each poll tick. Arm and
disarm are serialized per workspace; disarming touches GitHub only when
lazybox armed it.

## The coordination MCP server

`mcp.rs` is the one MCP surface lazybox ships, and it is a coordination bus —
not a wrapper around repo actions. Spawned sessions get a per-session bearer
and a loopback `rmcp` endpoint (identity is the connection) exposing
`whoami` / `list_sessions` / `read_session`, the `post_note` / `read_notes`
blackboard, `notify_session`, `task_status`, the epic tools, and
`spawn_worker`. Agents drive `git` and `gh` directly; adding an approval layer
around those is a design change, not a fix. Design:
[`docs/mcp-coordination.md`](../../docs/mcp-coordination.md).

`task_status` (`task_status.rs`, #1785) answers "is anyone working on
`owner/repo#N`?". Resolve a record by scanning `Workspace::hierarchy_task_ids()`
— never `primary_task()`, and never by reverse-parsing a workspace key, which
`sanitize_key` makes lossy — so an issue still resolves after its PR takes over
the row. The report keeps tracker lifecycle, working-claim, session
(`SessionRunState`) and agent turn (`AgentState`) as separate facts because
none implies another: a finished turn is not a finished task, an unexpired
`lazybox:w:` claim is not a running process, and a live terminal that has not
reported a state is `unknown`, not idle. It is read-only — it must never reach
for `workspace::attach::attach_to_record`, which materializes a workspace from
the provider — and `Err` is reserved for status that could not be *established*,
so a failed lookup can never read as "no worker". The same derivation backs
`lazybox task status <ref>` over `Command::QueryTaskStatus`, which is the
documented fallback for a session that gets no MCP tools.

## The tracker-record cache handed to sessions

The daemon already pays GitHub for every record it shows, and the token's
budget is shared with every agent it spawns — agents outspent the poller 100:1
and left the inbox forty minutes stale (#1799). `task_cache.rs` serves that
cache back: `.lazybox/task.json` is written into the worktree at spawn, and
`task` / `get_issue` / `get_pr` / `list_issues` read it live. Nothing there
may fall back to a provider fetch — a miss is reported as a miss, or serving
an agent spends the budget the cache exists to protect. A store failure is an
error, never an empty result: empty is how these tools say "never polled",
which an agent acts on.

Three shapes there are load-bearing and easy to get wrong:

- **Comments come from `Workspace::activity`, never `Task::recent_activity`.**
  The inbox scan selects `comments(last: 1)` and `attach_task` replaces the
  task on every poll without preserving activity, so a polled task holds at
  most the newest comment while the thread accumulates in the workspace feed.
- **The record file is hidden with `info/exclude`, never a `.gitignore`.**
  `<repo>/.lazybox/` belongs to the repository — `snippets.yaml` lives there
  and is committed — so an ignore file in it breaks `git add` for the repo's
  own config, invisibly, and lands in the user's clone for a linked checkout.
- **`list_issues` returns summaries.** Full bodies at list scale are tens of
  thousands of tokens from the tool whose purpose is protecting context.

Cache age lives in `PollState`, not on the persisted `Workspace`: the commit
path skips a byte-identical row to avoid a write and a broadcast, so a stamp
inside the row would re-broadcast every row on every tick.

## The metering / context-hygiene proxy

`proxy/` sits between an agent and its provider. Two facts live at different
seams and must not be merged: `rewrote` is set at `rewrite()` (a wire fact),
while the saving is computed at `observe_usage()` (a billing fact). Collapsing
them disarms the cache kill switch. `measure` must run before `rewrite` in
`proxy/mod.rs`.

Metering composes by OR across the per-workspace flag, the Space and the
global mode — except that a configured `off` outranks both, being the kill
switch. The proxy reads this per request, so a flip lands on the next turn.

## Tests

`test_env.rs` carries the shared harness. Daemon tests bind real sockets and
spawn real processes: a closed tokio listener leaves its port unbindable for
about 85ms with no holder visible to `lsof`, and helper-spawn tests fail on
fixed timeouts when the box is loaded. Re-run a suspect failure in isolation
before treating it as your diff.
