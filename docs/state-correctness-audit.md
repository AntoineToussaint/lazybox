# State-correctness audit: workspace / session / terminal lifecycle (#1374)

Why the same class of bug — resurrection, stuck "spawning", ghost rows,
stale targets, lost state — keeps recurring: **multiple, unreconciled
sources of truth** for "what workspaces / sessions / terminals exist,"
which disagree after restart / archive / merge / kill. This document
enumerates every source and every mutation site, maps each divergence to
`file:line` with the exact race, defines the single authoritative
reconciliation those sources should conform to, and ranks the concrete
defects that remain.

Companion issues: resurrection #1365 → #1368 (fixed), history/draft
re-keying #1362 (fixed), notification flood #1370 (fixed), stuck-spawning
invariant #1372 (open); newly surfaced by this audit: #1377, #1378. Line
numbers are as of the commit that adds this document; treat them as
anchors, not addresses.

## Cross-cutting findings, up front

1. **Teardown is already consolidated; recovery and the "existence"
   record are not.** Every destructive workspace path funnels through one
   owner (`WorkspaceLifecycle::remove`, `crates/server/src/workspace/mod.rs:2146`).
   But the durable answer to "does this workspace still exist?" is split
   across the store's workspace rows *and* a separate `archived_workspaces_v1`
   set, and the two are consulted by different passes with different rules.

2. **The archived set conflates two independent concerns.** It is used
   both to (a) kill an orphan tmux session on restart so a deleted
   workspace can't reattach, and to (b) suppress re-polling a row back
   into the inbox. Removal reasons that want (a) but *not* (b) —
   `ClosedAuto` (issue→PR collapse, must resurface on reopen) and
   `Rescope` — deliberately skip the set (`WorkspaceRemovalReason::archives`,
   `workspace/mod.rs:2122`) and therefore lose the resurrection backstop
   too. See Defect D2.

3. **Startup runs two reconcile passes with divergent rules.**
   `recover_sessions` (`crates/server/src/spawn_handler.rs:8847`) consults
   the archived set strictly before reattaching a live tmux survivor;
   `reconcile_missing_recovered_sessions` (`spawn_handler.rs:9211`), which
   handles persisted `terminal:` rows whose tmux session is gone, does
   **not**. See Defect D3.

4. **The client "spawning" arc is the one lifecycle state with no
   bounded floor.** The daemon's in-flight collapse is deadline-bounded
   (`INFLIGHT_COLLAPSE_DEADLINE = 600s`, `spawn_handler.rs:4834`); the
   client `spawning` set clears only on six specific events and has no
   age cap and no `Esc` escape, so any spawn-launch failure that emits
   none of those events strands the arc until the next full `Snapshot`.
   This is the substance of open issue #1372. See Defect D1.

---

## 1. The sources of truth

| # | Source | Where | Holds | Authority |
|---|--------|-------|-------|-----------|
| S1 | **Store — workspace rows** | `crates/store/src/sqlite.rs:81` (single `kv` table); `workspace:{key}` → JSON | The set of workspaces that exist + their sessions→worktree mapping | **Intent.** Highest authority for "should this exist." |
| S2 | **Store — archived set** | one kv row `KV_KEY_ARCHIVED = "archived_workspaces_v1"` (`crates/core/src/config.rs:290`); writers `archive_workspace_key`/`unarchive_workspace_key` (`workspace/mod.rs:1067`/`1103`) | Keys the user (or a confirmed merge) got rid of | Durable "the user removed this" record; the resurrection backstop. |
| S3 | **Store — per-terminal meta** | `terminal:{backend_key}` → `(session_key, kind)` JSON; also `terminal-msgs:{session_key}` (history), `terminal-draft:{session_key}` (draft) — key builders `spawn_handler.rs:163`/`167` | The durable pointer from a backend session to its workspace + kind | Rebuilds the registry on restart (`recover_sessions`). |
| S4 | **tmux backend** | `TmuxBackend::list` (`crates/server/src/backend/tmux.rs:927`) runs `tmux list-sessions` | The **actual live** detached sessions | **Resource.** The lifecycle source of truth for "is a process alive" (the in-memory conduit map is deliberately not consulted). |
| S5 | **Daemon in-memory registry** | `TerminalRegistry` (`crates/server/src/registries.rs:311`); `entries: Mutex<HashMap<TerminalId, TerminalEntry>>` + lock-free mirror `live_backend_keys` | Live terminals this daemon owns right now | **Derived cache** of S1–S4, rebuilt at startup. |
| S6 | **Client projections** | inner `Sidebar` (`crates/tui/src/components/sidebar/mod.rs`): `running_terminals:290`, `agent_terminal_states:357`, derived `agents:369`, `spawning:380` | What the UI paints (badges, spinner, asking pill) | **Derived cache** of daemon events + `Snapshot`. Must never be authoritative. |

There is deliberately **no** terminal/session table on the `Store` trait
(`crates/store/src/traits.rs:193`) — terminal state is entirely kv-backed
and reconstructed, which is why S3 + S4 must be reconciled rather than
read as a table.

## 2. Mutation sites (existence & lifecycle)

| Action | Client (`dispatch.rs`) | IPC command (`lib.rs`) | Server owner | Notes |
|--------|------------------------|------------------------|--------------|-------|
| **Spawn agent/shell** (`w`, `a *`, `s`, `b *`) | builds `Spawn { …, force_new }` | `Command::Spawn` @ `1668` | `handle_spawn` → `handle_spawn_inner` (`spawn_handler.rs:1082`/`1238`); dedup via `SpawnCoordinator` (`registries.rs:1393`), claim @ `1352`; terminal + `TerminalSpawned` in `spawn_executor.rs:286` | Duplicate spawns collapse onto the winner (`await_inflight_singleton:4895`); cancel via `Command::CancelSpawn`. |
| **Archive** (`x x`) | `optimistic_remove_workspace` + `Kill` (`dispatch.rs:789`) | `Command::Kill` @ `2086` | `delete_workspace` → `WorkspaceLifecycle::remove(UserArchive)` (`workspace/mod.rs:2103`/`2146`) | Archives (S2). |
| **Close & kill** (`x k`) | optimistic remove + `DeleteOrClose` **and** `Kill` (`dispatch.rs:906`) | `DeleteOrClose` + `Kill` | upstream close (best-effort) + `delete_workspace` | The `Kill` half ends the local workspace. |
| **Delete/close upstream** (`g d`) | `DeleteOrClose` only (`dispatch.rs:836`) | `Command::DeleteOrClose` @ `2175` | `polling::handle_delete_or_close` | **Upstream only — does not remove the local workspace or its terminals.** |
| **Merge** (`g m`) | `MergePr` (`dispatch.rs:609`) | `Command::MergePr` @ `2166` | `handle_merge_pr` (`polling/handlers.rs:655`) | No local teardown; emits `PrMerged`, poll picks up MERGED. Removal is a **separate prompted step** (`MergedPrRemovable` → `RemoveMergedWorkspace` → `remove_locked(MergedConfirmed)`, archives). |
| **Kill one terminal** | Close | `Command::Close { terminal_id }` @ `1800` | `handle_close` (`spawn_handler.rs:8226`) → `backend.kill` + `agent_recovery.forget` | Single terminal, not the workspace. |
| **Project cascade** | DeleteProject | `Command::DeleteProject` @ `2102` | `delete_project` (`workspace/mod.rs:2545`) loops `remove(ProjectCascade)` | Preflight-inspects every child; archives. |
| **Reap stale session** | — (daemon-internal) | — | `session_reaper` (`crates/server/src/session_reaper.rs`), predicate `closed_beyond` | Kills live terminals of long-merged/closed workspaces; leaves rows/worktrees. Startup restore consults the same predicate. |
| **Recover on restart** | — | — | `recover_sessions` (`spawn_handler.rs:8847`) + `restore_persisted_sessions` (`10535`) | Reattach survivors, then respawn intent. |
| **Reconcile dead rows** | — | — | `reconcile_missing_recovered_sessions` (`spawn_handler.rs:9211`) | Commit `Exited` for persisted terminals with no live session. |

### The teardown sequence (`remove_locked`, `workspace/mod.rs:2168`)

The single destructive ordering every reason inherits:

1. Snapshot the workspace (`2180`) — needed for worktree reclaim after the row is gone.
2. **Kill registry terminals** (`2298`–`2357`): `to_kill` filtered by `entry.meta.session_key == key` (the authoritative wire mapping); `backend.kill` then `detach_killed_terminal`. **A kill failure aborts the whole delete** and rolls the client back via a `"terminal"`-sourced `ProviderError` (`2334`).
3. Fresh safety inspection `inspect_workspace_removal_risks` (`2367`) — refuses on unpushed/dirty local work.
4. **`archive_workspace_key`** (`2409`) — only when `reason.archives()`.
5. `store.delete_workspace` (`2418`), archive rollback on failure (`2420`).
6. `Event::WorkspaceRemoved` (`2436`).
7. **Orphan tmux sweep** (`2448`–`2491`): `backend.list()`, skip `registry_backend_keys`, and for any session whose `terminal:` meta names this workspace, `backend.kill` + `sweep_terminal_persisted_fields`. Best-effort: a `backend.list()` failure does **not** undo the committed delete.
8. Reclaim worktree dirs (`2515`).

## 3. Reconciliation today (who wins, and where they disagree)

**Startup order** (`crates/server/src/client_runtime.rs`): migrate history keys → `recover_sessions` (reattach live survivors) → `restore_persisted_sessions` (respawn intent) → later, `session_reaper` first sweep (`FIRST_SWEEP_DELAY = 120s`).

**`recover_sessions` (the good pattern, post-#1368):**
- Lists live tmux sessions (S4), reads the archived set (S2) **once, strictly** — an unreadable set fails *open* (reattach all) rather than nuke live work (`8867`–`8876`).
- Per survivor: kill + sweep as an orphan **only when both** its `session_key` is in the archived set **and** `workspace_row_is_absent` (`8899`–`8916`, `9534`). The two-condition test exists because archived keys are permanent and workspace keys are reused, so an archived-then-recreated workspace lingers in the set while being live.
- Otherwise reattach: alloc `TerminalId`, register (`lock_recovered_registration`), broadcast `TerminalSpawned`, spawn a retrying attach pump.

**`reconcile_missing_recovered_sessions` (the divergent pass):** enumerates
`terminal:` rows absent from `live_keys` and commits `Exited` +
`sweep_terminal_persisted_fields` for each (`9211`–`9285`). It does **not**
read the archived set (Defect D3).

**Client:** rebuilds all six projections from `Snapshot`
(`handlers.rs:215`–`250`, hard reset at `231`) and mutates them per event
thereafter; optimistic mutations reconcile/roll back against the daemon
echo (`crates/tui/src/realm/model/optimistic.rs`).

**Authority in practice:** `Store (S1+S2) > tmux (S4) > registry (S5) >
client (S6)`, with S3 as the durable bridge S4 reattaches through. The
principle is right; the gaps are where a pass substitutes a *proxy* for
the authority (archived-set membership instead of "row absent," or a
per-event clear instead of a bounded floor).

## 4. Divergence map

**Closed (verified fixed) — kept for the ledger:**

- **Resurrection of archived/deleted workspaces on restart** (#1365 → #1368).
  Root: `recover_sessions` reattached every tmux survivor without consulting
  S2, and archive's registry sweep missed orphan tmux sessions with no live
  entry. Fixed by the strict archived-set guard (`8899`) + the delete-time
  orphan sweep (`workspace/mod.rs:2448`). Guarded by
  `recovery_kills_orphan_session_for_archived_workspace_but_reattaches_the_rest`
  and `recovery_reattaches_recreated_workspace_whose_key_lingers_in_archived_set`.
- **History/draft lost across restart** (#1362). Root: keyed by the
  ephemeral `backend_key`. Fixed by re-keying to `terminal-msgs:{session_key}` /
  `terminal-draft:{session_key}` (`spawn_handler.rs:163`/`167`) + a startup
  migration.
- **Notification flood on focus loss** (#1370). Root: every rising edge
  suppressed while focused re-fired the instant focus was lost. Fixed by
  `NotificationCoalescer` (`crates/tui/src/notify_coalesce.rs`, 500ms tumbling
  window + per-workspace dedupe).

**Open divergences** — see the ranked defects below (D1–D3).

## 5. The ONE authoritative reconciliation

**Principle.** The store is *intent*, tmux is *resource*, the registry
and client are *derived caches*. Every restart/reconnect must run **one**
pass that reads the authorities and makes the resources and caches
conform — never an ad-hoc guard bolted onto each mutation path.

**Split the overloaded archived set into two records.** The single
`archived_workspaces_v1` set today answers two questions it should not
conflate:

- a **session tombstone** — "a backend session for this key must be
  killed, never reattached" (needed by *every* removal reason, including
  `ClosedAuto`/`Rescope`);
- an **inbox-suppression / archived** flag — "do not re-poll this row
  into the inbox" (wanted only by user-facing archive; `ClosedAuto` must
  *not* set it, so a reopened issue resurfaces).

**The workspace row stays the authority; the tombstone only narrows the
kill.** Rows are durable — `restore_persisted_sessions` reads them from
the store via `list_workspaces()` (`crates/server/src/spawn_handler.rs:10536`),
so at startup, before any poll, every workspace that existed at shutdown
still has its row and every removed one does not. Row-presence is
therefore the authority for "should this exist," exactly as
`recover_sessions` already treats it (`workspace_row_is_absent`,
`spawn_handler.rs:8899`–`8916`). The tombstone must **not** replace that
gate: it is permanent and workspace keys are reused
(`allocate_workspace_key`), so an archived-then-recreated workspace is
`row_present` yet still tombstoned — killing on the tombstone alone
destroys that live workspace on every restart, which is the precise
`#1368` regression. The rule keeps row-absence as the kill trigger and
adds the tombstone only as a *disambiguator for the row-absent case*:

```
for each live tmux session S with meta (session_key, kind):
    row_present = store.get_workspace(session_key).is_some()
    if row_present:  reattach(S)                    # authority says it exists → survivor
    else:            kill(S); sweep(session_key)     # row gone → removed or crashed mid-remove; orphan
```

A row-present survivor is always reattached — recreation after archive is
covered for free, with no `recover_sessions`-style two-condition
special-case, because the row (not the permanent tombstone) decides. The
tombstone earns its keep in the *silent-sweep* refinement below, not in
the reattach-vs-kill choice; the `unreadable → fail open (reattach)` rule
stays.

**One pass, not two — and this is where the tombstone earns its keep.**
`reconcile_missing_recovered_sessions` handles persisted `terminal:` rows
whose tmux session is *gone*. Row-absence alone can't tell "the user
removed this" from "the agent crashed": both leave a dead row. The
tombstone disambiguates — a dead row whose session is tombstoned is swept
**silently** (no `Exited` broadcast for a workspace the user already
removed); a dead row that is *not* tombstoned commits `Exited` as today
(a real crash the UI should reflect). This is the tombstone's only
load-bearing use; the reattach-vs-kill choice above never consults it.

**Rebadge caveat.** Issue→PR collapse re-points terminals from the issue
key onto the PR key (`TerminalsRebadged`), so the surviving session's
meta already names the PR — tombstoning the *issue* key is correctly
inert for it (the PR row is present, the terminal reattaches under the PR
key). The tombstone therefore covers only sessions that were *not*
rebadged: an issue-keyed tmux session whose agent had already exited
(lingering, no registry entry, never moved). That narrow set is exactly
what the delete-time `backend.list()` sweep can miss on failure, which is
why the tombstone is the durable backstop for it (see D2).

**Caches rebuild, never patch.** The registry (S5) is populated purely
by the reconcile result; the client (S6) rebuilds purely from the
post-reconcile `Snapshot`. This is already true — the invariant to keep
is that no recovery path emits per-terminal lifecycle events for a
tombstoned workspace, so the client never has a ghost to reconcile away.

Adopting this is a **migration**, not a rewrite: the session-tombstone
set is written on every `remove_locked` (all reasons), the archived flag
keeps its current writers, and `recover_sessions` +
`reconcile_missing_recovered_sessions` are unified behind the rule above.
Each step below is independently shippable with a regression test.

## 6. Ranked defects → child issues

Ranked by severity × confidence. Each is a child of #1374 with its
regression test named: D1 = #1372 (open), D2 = #1377, D3 = #1378.

### D1 — Client "spawning" arc can spin forever; no age cap, no `Esc` (open: #1372)

**Severity: high · Confidence: high.** The `spawning` set
(`sidebar/mod.rs:380`) is *set* in exactly one place
(`WorktreeProgress::Started/Progress`, `handlers.rs:528`) and *cleared*
in six (`Snapshot:231`, `TerminalSpawned:266`, `AgentState:401`,
`WorktreeProgress::Failed:535`, `WorkspaceRemoved:336`, and
`ProviderError` **only if `source.starts_with("spawn")`**, `545`–`558`).
Any spawn-launch failure that emits none of these — a `Cancelled`
outcome returned with no event (`spawn_handler.rs:1673`), or a failure
surfaced with a non-`spawn` source (`"git"`/`"store"`/`"terminal"`)
after `WorktreeProgress::Started` — strands the arc until the next full
`Snapshot`, with no manual escape.
**Failure scenario:** provision starts (`spawning` set), the agent
launch fails in a way that reports a `"git"`-sourced error → arc spins
forever; `Esc` does nothing.
**Fix:** give `spawning` a bounded floor (age cap → auto-clear with a
loud notice) and an `Esc` escape, and guarantee every spawn terminal
path emits a workspace-targeted terminal event. **Test:**
`spawning_arc_clears_on_non_spawn_error_and_on_age_cap` (client) +
`every_spawn_failure_emits_a_workspace_targeted_signal` (daemon).

### D2 — Non-archiving removals have no resurrection backstop (#1377)

**Severity: medium. Confidence: high that the orphan survives; medium on
the resurrection mechanism (see below).** `ClosedAuto` (issue→PR collapse)
and `Rescope` delete the row but skip the archived set
(`WorkspaceRemovalReason::archives`, `workspace/mod.rs:2122`). Their only
orphan protection is the best-effort delete-time `backend.list()` sweep
(`2448`), which does not undo the delete on failure.

The orphan surface is narrower than "any collapsed issue": collapse
rebadges the issue's terminals onto the PR key (`TerminalsRebadged`), so
a live agent moves rather than orphans. What remains is the issue-keyed
tmux session whose agent had **already exited** — lingering, no registry
entry, never rebadged — which the registry sweep can't see and the
`backend.list()` sweep is the sole killer of.

**Failure scenario:** an issue with such a lingering exited-agent session
collapses into its PR while the delete-time `backend.list()` transiently
fails. The orphan tmux session survives. On restart `recover_sessions`
lists it and — because the workspace was never archived — reattaches it
instead of killing it. Note the resurrected artifact is a *terminal*, not
a store row: `restore_persisted_sessions` is row-driven (`list_workspaces()`)
and never recreates a deleted row, so the reattached orphan surfaces as a
**ghost workspace synthesized client-side from a terminal with no backing
row** (the same client synthesis behind the empty-ghost-repo-group class),
not as a resurrected `workspace:{key}` row. Same user-visible symptom as
`#1365`; different mechanism from a poll re-adding the row.
**Fix:** the session-tombstone split in §5 — tombstone *all* removal
reasons for kill-on-recovery while leaving `ClosedAuto` out of the
inbox-suppression flag so reopen still resurfaces. **Test:**
`closed_auto_orphan_session_is_not_resurrected_when_delete_time_sweep_failed`.

### D3 — `reconcile_missing_recovered_sessions` ignores the archived set (#1378)

**Severity: low · Confidence: high.** Unlike `recover_sessions`, the
dead-row pass (`spawn_handler.rs:9211`) registers a persisted `terminal:`
row and commits `Exited` without checking the archived set. Because
`finish_terminal` sweeps the row (`5883`) it is one-shot, not a
resurrection — but it briefly registers and broadcasts a lifecycle event
for a workspace the user already archived, a needless ghost the client
must absorb.
**Failure scenario:** an archived workspace's agent had already exited
(tmux gone, `terminal:` row never swept because it hit neither the
registry nor the delete-time `backend.list()`); on restart the reconcile
pass fires `Exited` for it.
**Fix:** apply the same archived/tombstone guard here — sweep silently
instead of registering + broadcasting. **Test:**
`reconcile_missing_sessions_sweeps_archived_rows_without_broadcasting_exited`.

---

*This audit is documentation of intent for a multi-step migration; the
fixes land as the child issues above, each with its own regression test,
under the umbrella #1374.*
