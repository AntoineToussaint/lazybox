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

## Nothing unattended destroys a tmux session

A tmux session is the user's work. Its scrollback is routinely the only
surviving record of what an agent did, and it is how they get back in. So
**only an explicit user action removes a backend** — `]]x`, `x k`, `x x`, an
explicit workspace delete. No exit code, no poll tick, no sweep, no
background timer.

`code 0` is not consent. An agent that exits one second after spawning
exited zero, and reaping its session destroyed the only evidence of why
(#1869: `claude` exited 0 on every respawn and a session with a 928-prompt
transcript was lost). The daemon therefore spawns panes under
`remain-on-exit on`: the program dies, the pane stays dead-but-intact, the
session survives, and the user reads it or closes it themselves.

That breaks the exit signal, so it is restored deliberately. Exit reaches the
daemon as the attach client's EOF; a session that outlives its program never
EOFs. `TmuxBackend` arms a **per-session** `pane-died` hook whose command is
`detach-client -s "<key>"` with the key written in literally — tmux expands no
`#{…}` in a hook argument, and a global bare `detach-client` detaches
whichever client the server saw last (measured on 3.7c: it detached a
different, still-running session). Detaching, not killing, turns pane death
into the ordinary EOF the whole lifecycle already speaks: `release` drops
lazybox's conduit and nothing else.

`is_alive` is the other half. Under `remain-on-exit` existence no longer
implies a running program, so it reports a dead pane as not-alive — and an
inconclusive probe as alive, never as gone. `recover_sessions` reads it and
**skips** a dead-paned survivor: it does not reattach (a dead pane never
EOFs, so the corpse would render as a live agent forever) and it does not
kill (nobody asked). It names them in one notice instead.

Retention is unbounded on purpose. If dead sessions accumulate, the answer is
a user-visible way to remove them — never a reaper. `agent.reap_closed_after`
is opt-in for the same reason: unset, lazybox reaps nothing.

## A deep-scrollback capture names a watermark it already covers

`SessionBackend::scrollback` returns `(history, seq)`, and `seq` promises the
client that everything at or below it is *in* the history — that is what lets
the client put back the live output the capture predates instead of erasing
it. tmux paints a chunk to its attach client only after the pane grid holds
that content, so the mark has to be read **before** `capture-pane`, never
after: a mark read afterwards covers bytes the capture predates, and both
sides then drop them (#1798). A chunk landing during the capture may be in it
already and is re-fed by the client — a repeated repaint, which is the visible
half of that trade.

## Polling

`polling/scheduler.rs` runs tiers, not one interval: a hot set for live and
armed workspaces (15s), a per-repo rotation for the roster, and a periodic
unwindowed reconcile — the only pass that may retire a row it no longer
sees. A windowed pass retires what it positively observes closed or
merged, so retirement does not stall when the reconcile is deferred. An armed
auto-merge rides the hot tier so green → merged is one tick, and leaves it
once GitHub-native auto-merge takes over.

Duplicate spawns are collapsed by the SpawnCoordinator rather than by the
callers, so a double-fire or an issue→PR rebadge never forks two backends.
Explicit spawn keys still force a new agent deliberately.

Repo-free workspaces persist `Workspace.floating` as their directory purpose.
`workspace/floating.rs` owns fresh-folder allocation and durable session
resolution through core paths; never authorize a spawn from a key prefix.
Archive preserves these folders and their user files. Coordination workspaces
start with the Coordinator role and load `agent.coordination_prompt` (or the
built-in brief) on every fresh start and resume, alongside the shared startup
contract. They organize work; implementation stays on the tracker record.

## Merge has two call sites

`polling/handlers.rs` (the interactive merge) and `polling/auto_merge.rs`
(merge-on-green) both wire merge and trailers. A change made in one of them
half-lands; put it in the provider, or change both.

**But lazybox usually does not perform the merge.** Since GitHub-native
auto-merge shipped (#1596 via #1607), GitHub writes the merge commit; `gh pr
merge` and the web UI never let lazybox write it. So the *common* path for
cost is the out-of-band recorder — `handlers::record_external_merge_trailers`,
gated in `polling/upsert.rs` on the `TerminalCleanup::MergedPr` transition,
with a durable `pending-merge-cost:` intent and a poll-tick sweep behind it.
A merged PR with no stored workspace returns before that gate, so there is
nothing to record against. Test the external path explicitly: covering only
lazybox's own merge covers the rare case, which is how #1917 shipped inert.

Cost trailers (`pr_trailers.rs`, format contract in
`lazybox_core::pr_trailers`) are permanent and public, which is why
`providers.github.pr_trailers` gates them.

**Measuring is not publishing, and public repos default to `off`.** A commit
trailer cannot be deleted without rewriting history, so per-PR spend on a repo
the world can clone is opt-in per repo (`repos: { owner/name: full }`);
`private` defaults to `full`. A workspace therefore measures a real cost and
correctly publishes nothing — which is a *decision*, and it must be said out
loud. #1917 was 25 merges of exactly this with no log, no event and no UI
anywhere, indistinguishable from a broken feature.

Four invariants the code exists to hold:

- `commitBody` *replaces* GitHub's default squash log, so the default is read
  back and appended to — a body that cannot be resolved merges with **no**
  trailer rather than a truncated log.
- An unmetered PR gets no `Lazybox-Cost` line at all, never `$0.00`:
  "free" and "not metered" must stay distinguishable.
- Cost is billed per PR, not per workspace — the `meter-cost-mark:` watermark
  is stamped at merge, and the issue→PR fold carries the issue-phase total
  onto the PR key (`client_kv::move_session_cost`, which *deletes* its source
  row, so a surviving source row proves the fold never ran).
- **Only a settled record may stamp the watermark.** The watermark claims
  "this figure has been dealt with", so `TrailerOutcome` decides, not the
  merge: `InCommit`/`InComment` landed and close the slice; `Nothing` is a
  deliberate policy withholding and closes it too (leaving it would roll this
  PR's spend onto the next one); `Dropped` was measured, permitted and
  **lost**, so it stays owed; and a reconciled merge wrote nothing lazybox
  passed. Both merge sites stamped before testing the outcome and silently
  retired real money — the guard lives in `pr_trailers::mark_merge_reported`
  so one place covers both.

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
blackboard, `notify_session`, `task_status`, the epic tools, the work-store
verbs (below) and `spawn_worker`. Agents drive `git` and `gh` directly; adding an approval layer
around those is a design change, not a fix. Design:
[`docs/mcp-coordination.md`](../../docs/mcp-coordination.md).

**The two spawn tools are the only agent-facing way to launch an agent**, so a
spawn knob missing from `SpawnWorkerArgs` / `StartWorkspaceArgs` is missing
from the fleet however complete the plumbing below it is — #1911 was exactly
that: every layer under the tool honoured a per-spawn model tier and neither
schema named it. Both now take `model`, resolved against the *target agent's*
own menu (`AgentModels::alias_for_requested_token`: a tier alias, label or
model id, else a `best`/`high`/`medium`/`low` capability word) before anything
is filed, attached or claimed, and refused with the menu listed when the agent
has no such tier. Refuse at this boundary, never by tightening
`spawn_plan::resolve_model_for_agent` — that fallback to the default tier is
load-bearing for the `w S` chord (one alias, heterogeneous target agents) and
for a restored session replaying a recorded alias whose tier config no longer
declares.

`task_status` (`task_status.rs`, #1785) answers "is anyone working on
`owner/repo#N`?". Resolve a record by scanning `Workspace::hierarchy_task_ids()`
— never `primary_task()`, and never by reverse-parsing a workspace key, which
`sanitize_key` makes lossy — so an issue still resolves after its PR takes over
the row. The report keeps tracker lifecycle, working-claim, session
(`SessionRunState`) and agent turn (`AgentState`) as separate facts because
none implies another: a finished turn is not a finished task, an unexpired
claim is not a running process, and a live terminal that has not
reported a state is `unknown`, not idle. It is read-only — it must never reach
for `workspace::attach::attach_to_record`, which materializes a workspace from
the provider — and `Err` is reserved for status that could not be *established*,
so a failed lookup can never read as "no worker". The same derivation backs
`lazybox task status <ref>` over `Command::QueryTaskStatus`, which is the
documented fallback for a session that gets no MCP tools.

## The work store: declared intent, not observed liveness

`work_store.rs` persists `lazybox_core::work` — the task/plan rows
`docs/agent-coordination-v2.md` names as the shared plan — under the `work:`
and `plan:` kv prefixes, and `create_work` / `my_work` / `update_work` /
`work_status` in `mcp.rs` are its agent-facing verbs. A handoff made through
them is a row with a requester, a lifecycle, a result and a provenance
history, which is what `notify_session` cannot be: that reports only that text
landed.

**`Lifecycle` is the third state enum here and must not be confused with the
other two.** `AgentState` is liveness *observed* from the PTY and the hooks;
`lazybox_core::TaskState` is the *tracker record's* state, owned by the
provider; `Lifecycle` is what whoever owns the work *declares*. They disagree
routinely and neither is wrong when they do — an agent at a permission prompt
is `InputNeeded` while its task is legitimately `Underway`. So a `Lifecycle`
never moves on an `AgentState` change alone.

**The exception is a sweep, not a hook, and that is the whole design.** Work
whose agent is gone has to fail, or it reads as in flight forever to whoever
is waiting. But `AgentState::Exited` is the wrong trigger: `Shift-K`,
auto-fix, `a c` and credit recovery all tear a terminal down and spawn a
replacement, so failing on teardown would fail the work of every agent lazybox
itself restarted. `sweep_stranded` instead fails `Underway` work whose owner
workspace has had no live agent for `STRANDED_GRACE`, once a minute. If you
are tempted to move this onto the exit path, that is the regression.

Two other things follow the store's rules rather than their own: multi-row
writes go through `Store::apply_batch`, because a half-applied roll-up makes a
progress bar disagree with the tasks it counts; and auto-check on merge rides
`workspace::check_todo_items_linked_to`, the one call site #1898 already had,
so the per-workspace checklist and the plan rows cannot disagree about
whether a record landed. The `todo_items` field is **not** yet folded into
these rows — the TUI's TODO list still reads it.

## Working claims: a label for presence, a comment for identity

`working_claims.rs` owns "an agent is working on this". It is two upstream
facts (#1922) and a durable local record that keeps them converged:

- the one stable `working` label — **presence**, read free from the poll
  payload by `Task::has_working_claim()`;
- one sticky comment per record — **identity**: holder, agent, model, started,
  last heartbeat, expiry, edited in place every 15 minutes.

The local `WorkingClaimRecord` (a `terminal-working-claim:<holder>` kv row) is
keyed by HOLDER, never by the upstream text, and remembers the comment id. That
id is what makes a steady-state heartbeat **one** request: the in-place comment
edit. Whether to spend a second on the label is answered by
`stable_label_attached`, which reads the persisted row — so a label that never
moved costs nothing, and one a human stripped is re-attached. The predecessor
minted a `lazybox:w:<device>:<session>:<expiry>` label per claim and spent two
requests renaming it to its new expiry every heartbeat.

Three consequences worth knowing before you touch this:

**Presence is now binary, so a lapsed claim is not free to spot.** The expiry
used to be readable from the label name on any tick. It now lives in the
comment, so `retire_lapsed_stable_claims` (on the 15-minute maintenance tick,
one `Cold` comment read per record whose label no local lease accounts for) is
what retires one. Until it runs, a lapsed claim still reads as claimed — the
conservative direction, which over-blocks a spawn rather than letting the fleet
double-spawn. A label with **no** comment of ours behind it is left strictly
alone: `working` is an ordinary word that a human or another tool may own, and
`task_status` reports it as unbacked rather than resolving it into a holder.

**The label is shared, so a release has to look before it detaches.** Two
boxes meant two labels before, so "release mine, leave the racing machine's
alone" was true by construction. `release_working_claim` now reads the standing
comment first and hands back `SupersededBy` when it names a different live
lease, touching nothing.

**The migration order is load-bearing.** A record carrying a pre-#1922
`legacy_label` (read through serde's old `label` field name) attaches the
stable label *first*, then retires the per-claim one. The other order leaves
the record momentarily unclaimed, which is exactly the double-spawn window the
claim exists to close.

`ClaimRelease::WorkspaceLockHeld` is unchanged and still required from workspace
removal — `project_synced_claim` takes the workspace's non-reentrant lock, and
the phantom-workspace hang after #1533/#1534 was a release parking on the lock
its own caller held.

## Two GitHub clients: who authors, who polls

Registering a GitHub App (`providers.github.app`) gives the status sweep an
*installation* credential with its own 5,000/hour, so agent sessions can no
longer starve the inbox (#1802). That splits one client into two, and the
split is easy to get wrong in a way nothing fails loudly about:

- `PollState::cached_gh_client()` is the **user** client. Mutations, reads and
  anything authored on the user's behalf go through it — a comment, merge or
  👀 reaction posted on the App client is attributed to the bot.
- `PollState::polling_gh_client()` is whatever the sweep is **actually
  running on**. Everything that asks about polling reads this: the rate-limit
  wait event, `Shift-R`'s force-full-sweep, the background PR-details
  prefetch. Reading the user client instead reports the wrong budget, or
  forces a sweep on a client that is not running one.

An installation token has no user, so three calls stay on the user client:
`GET /user` (the viewer login is carried in at construction instead),
GraphQL `viewer` (the budget bootstrap drops it), and `GET /notifications`
(the REST heartbeat — it is the user's own feed). The heartbeat and the sweep
share one cursor + sweep-clock state via `GhClient::sharing_sync_state_with`.

Coverage decides how much of the sweep the installation carries
(`poll_credential_plan`). Whenever discovery is **repo-first** — the user has
scoped or watched repos, so the sweep is a per-member fan-out — the roster is
partitioned: members the installation reaches run on its budget, the rest run
on the user token, in the same tick (#1807). Every read is routed the same
way (`GhSource::client_for`), because a credential that cannot see a repo does
not fail — a search returns fewer rows, a node read returns "not visible".
That routing, not a downstream guard, is what keeps a reconcile from retiring
rows it never really swept.

Two shapes cannot be partitioned and still fall back whole, with a notice
naming the reason: `include_accessible_repos` (an open-ended roster), and a
sweep with no roster at all — that one is a single global `involves:` search,
and there is nothing to split.

A split means the user client does real scheduled work, so it gets its own
`begin_background_tick`. A `RateBudget` that never begins a tick never clears
its per-tick scheduled accounting, and its spend accrues until every
scheduled request it makes is refused against an allowance nobody granted.

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

## A session's `gh` runs through the daemon

The record cache only helps an agent that chooses to read it. `gh_shim.rs`
covers the rest (#1801): `<home>/shims/gh` goes on every spawn's PATH ahead of
the real binary, and asks the daemon before it spends
(`Command::GhAdmit`) and reports after (`Command::GhCompleted`).

- **The read cache stores `gh`'s own bytes, keyed by repo scope + argv** — not
  a re-rendering of the cached `Task`. Agents parse that output; a lookalike
  would diverge from it silently. A key resolves to `None` (uncached) whenever
  the repo scope is ambiguous, so a key can never span two repositories.
- **A cache hit spends no quota token.** Charging for it would throttle the
  behaviour the cache exists to reward.
- **Reads yield at the governor's reserve; mutations do not.** The governor
  lets *interactive* work spend past the reserve on purpose. Agent `gh` is the
  background burn that emptied the budget, so its reads wait — but refusing
  `gh pr merge` strands the task rather than delaying it.
- **The change signal is the half that works at zero budget.** A mutation's
  outcome is already known locally, so `apply_known_record_state` writes it
  onto the row with no GitHub call. It must decide the one-shot terminal
  cleanup from the row *before* the flip persists: `closed_issue_transition`
  requires a non-terminal predecessor, so writing the state first silently
  costs the workspace its reap.
- **Everything degrades toward plain `gh`.** No daemon, a slow answer, an
  unrecognised subcommand, `LAZYBOX_GH_SHIM=0`, or `gh.real` — each runs
  exactly what the agent typed. The shim may pace `gh`; it may never break it.
- **Three independent guards against the shim resolving `gh` to itself**, because
  each alone has a hole. Path equality fails when `LAZYBOX_GH_SHIM_DIR` is
  stripped and `LAZYBOX_HOME` names another profile; the `SHIM_MARKER` content
  check covers that but not a hand-edited script; `SHIM_DEPTH_ENV` identifies
  nothing and so bounds recursion whatever else missed. A `gh` extension calls
  `gh` again, which is why the depth cap is 4 and not 1.
- **The shim is dispatched before `init_tracing()`** (`tui-boot/src/main.rs`).
  Tracing redirects OS stderr into the log file, and the shim runs `gh` with
  inherited stdio — on the wrong side of that call, `gh`'s own errors go to
  /tmp/lazybox.log and the agent gets a bare non-zero exit.
- **The read-cache key leads with host + credential fingerprint.** `owner/repo`
  is not unique across GitHub hosts, and `normalize_remote` discards the host,
  so repo+argv alone let an enterprise issue be answered with github.com's.
- **The cache is bounded three ways** — clamped TTL, entry count, total bytes.
  Age alone bounds nothing when the arrival rate scales with the fleet.

## Searching what an agent said

`agent_output_search.rs` answers the `/` search's `agent:` / `said:`
qualifiers over terminal OUTPUT (#1780). The prompt half (#1774) is
client-side; output exists only in the replay rings, so the client asks and
the daemon scans.

- **It returns deduplicated matching LINES, never a byte window.** An agent
  TUI repaints its whole box continuously, so a window is mostly duplicate
  frames — and those frames position each row with a CSI rather than a
  newline. Only a ROW-changing sequence ends a line: `C`/`D`/`G` move within
  one row (programs pad columns with `\x1b[<n>C` because it is shorter than
  spaces) and reconstruct as a blank, erases and SGR are invisible. Breaking
  on any of those split one rendered row in two, and every multi-word needle
  spanning the split missed.
- **Evidence is kept per needle, not per workspace.** The client ANDs the
  `agent:` terms, so a needle with no evidence *excludes* the workspace. One
  global line cap let a chatty needle spend the whole budget and silently
  drop a row that matched every term; buckets plus round-robin ordering give
  each term its own room, and a `const` assert pins `MATCH_CORPUS_BYTES` at
  one full line per needle so the byte trim cannot undo it. The same applies
  to `MAX_AGENT_OUTPUT_NEEDLES`: a needle past the cap is a false negative,
  not a widening, so raise it rather than trim it.
- **Cost is bounded before the query is issued**: the newest
  `SCAN_TAIL_BYTES` of each ring, `MATCH_CORPUS_BYTES` per workspace, and
  `SNAPSHOT_CONCURRENCY` snapshots in flight — sequential per-snapshot
  deadlines cost N × the deadline in series, which is what that bound exists
  for. The price is a function of terminal count, not of how chatty an agent
  has been.
- **An empty reply is load-bearing.** It is what clears the previous
  query's rows on the client, and a client that hears nothing cannot tell
  "no match" from "still scanning" — so the handler always sends one.
- **The reply is scoped to its request id, and the client latch times out.**
  The scan is asynchronous and unordered with respect to typing, so a reply
  that outlives its query would filter the sidebar by a needle the user typed
  past. The reply can also never arrive — a full event queue makes the
  forwarder close the connection — so the client releases the latch on a
  deadline and on the reconnect `Snapshot`, and re-asks on a slow cadence so
  a standing query stays as live as its prompt half already is.

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
