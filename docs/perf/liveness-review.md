# Liveness review: the app must never become unresponsive

Issue: #1045. Related: #1030 (sync-driven freeze), #1031 (deep UI-stall
review — see [`ui-stall-review.md`](ui-stall-review.md)).

#1031 asked *why the UI stalls* and answered it as a performance problem
(render/drain cost). This review asks a stricter question: **can the app
ever get stuck — input ignored, a modal un-dismissable — with no way out?**
That is a *liveness* property, not a latency one. A slow frame recovers; a
deadlocked modal or a promise that never resolves does not. The wake-from-
sleep incident in #1045 was the second kind: a `Loading` modal awaiting a
value that never arrived, on a UI thread that was also behind, so it read as
a total freeze.

This doc states the invariant, audits every place it can be violated with
`file:line` evidence, and records which violations this PR closes versus
which remain design work (with the follow-up that owns each).

## The invariant

> **From any trigger — sleep/wake, sync burst, reconnect, a slow or lost
> daemon response — the app stays responsive to input and every modal stays
> dismissable. No UI-thread code path may wait forever, and none may block on
> a daemon/network response.**

Three sub-properties, each independently testable:

- **L1 — Dismissable.** Every modal responds to `Esc` from its own event
  handler, using only local state. Dismissal never depends on a daemon
  reply, a channel receive, or a background task.
- **L2 — No infinite wait.** Every modal that awaits a background result is
  bounded by a timeout. If the result never lands, the modal dismisses
  itself and the flow backs out — the same outcome as `Esc`.
- **L3 — No UI-thread block.** Nothing on the UI thread performs a blocking
  wait (`.await` on a live future, a blocking `recv`, a network call). All
  daemon/network IO is owned by the daemon or a dedicated thread; the UI
  thread only ever does non-blocking `try_recv`.

L1 keeps the *user's* escape hatch alive; L2 keeps the *app's* own progress
alive when the user isn't watching; L3 removes the only way the run loop
itself could hang.

## Audit

### L2 — modals awaiting a result

Every spinner/awaiting modal was checked for a timeout backstop.

| Modal | File | Awaits | Timeout before | Timeout now |
|-------|------|--------|----------------|-------------|
| `Loading` | `crates/tui/src/realm/components/loading.rs` | setup effect result via `sync_channel` | **none — could spin forever** | **60 s (this PR)** |
| `Polling` | `crates/tui/src/realm/components/polling.rs:29,165` | first `WorkspaceUpserted` | 15 s | 15 s |
| `WorktreeProgress` | `crates/tui/src/realm/components/worktree_progress.rs` | provision step events | terminal step → Esc-dismissable retry (`:163`) | unchanged |
| `PrChat` / `HelpAsk` | `crates/tui/src/realm/components/{pr_chat,help_ask}.rs` | streamed agent answer | `Esc`/`Ctrl-c` dismiss (`pr_chat.rs:74`) | unchanged |

**The gap was `Loading` alone.** It resolves only when the background task
calls `LoadingResult::send(value)` → `Msg::LoadingResolved`
(`loading.rs`). Two prior escape hatches existed —
`Esc`/`Ctrl-c` (L1, `loading.rs` `on`) and a `Disconnected` channel
detection for a producer that *drops* its sender (`TakeOutcome::Cancelled`).
Neither covers the #1045 incident: the producer task is still **alive**
(sender not dropped) but its value was dropped on the overflowed event
channel, or the task stalled on a daemon/network response. The channel stays
`Empty`, `Cancelled` never fires, and with the run loop behind, `Esc` isn't
serviced either → forever spinner.

**Fix (this PR) — two layers, by responsibility:**

- **Effect-level timeout (the graceful path).** The setup executor
  (`setup_screen.rs`, `run_effect`) now wraps each effect in a 30 s
  `EFFECT_TIMEOUT`. A scope listing that never responds resolves as a
  `Retryable` `ProviderError` — the variant the runner already turns into a
  dismissable error screen that **continues** the wizard
  (`setup_flow.rs:889-893`, `930-939`) — rather than hanging until the modal
  backstop cancels setup outright. `Detect` falls back to an empty report on
  expiry. This is where a timeout *belongs*: the layer that can express a
  retryable error the runner knows how to recover from. Tests:
  `setup_screen.rs` `bounded_scopes_times_out_into_a_retryable_error` /
  `…_passes_a_result_through_before_the_deadline`.
- **Modal-level backstop (the last resort).** `Loading` itself carries a
  `started_at: Instant` + a 60 s `TIMEOUT` and, on `Tick`, emits
  `Msg::LoadingTimedOut` once the deadline passes while still `Pending`. A
  delivered value always wins the race (the `Got` arm is checked first).
  Because the effect timeout (30 s) fires first for the current caller, this
  backstop only covers a result that was *produced but lost* — dropped on an
  overflowed event channel, the #1045 wake signature — or a future caller
  that forgets its own timeout. `Msg::LoadingTimedOut` flashes a Retryable
  notice (`model/mod.rs`) so the modal never vanishes unexplained, then
  dismisses. This mirrors the pattern `Polling` already uses
  (`polling.rs:165`) — `Loading` was simply missing it. Tests: `loading.rs`
  `times_out_when_result_never_arrives`,
  `delivered_result_wins_even_past_the_timeout`.

`Esc` remains available throughout both layers. Note the backstop is driven
by `Instant`, whose accounting of system-sleep time is platform-dependent;
either way the deadline is finite, so the modal is bounded on any platform —
it is not guaranteed to fire the *instant* the machine wakes.

`Polling`, `WorktreeProgress`, `PrChat`, and `HelpAsk` already satisfy L2
(timeout or user-dismissable terminal state) — verified, no change.

### L1 — dismissability

Every modal's `Esc` handler was checked to confirm it acts on **local
state** and cannot be gated on daemon/channel state:

- `Loading` (`loading.rs` `on`): `Esc`/`Ctrl-c` drop the receiver and
  emit `Msg::ModalDismissed` directly — no wait. ✅
- `Polling` (`polling.rs:287`): any key → `Msg::ModalDismissed`. ✅
- `PrChat`/`HelpAsk` (`pr_chat.rs:74`, `help_ask.rs:163`): `Esc`/`Ctrl-c`
  dismiss even mid-stream. ✅
- The modal stack itself (`crates/tui/src/realm/model/modals.rs`) dismisses
  by popping local state; no arm awaits a reply.

L1 holds today **provided the run loop services the keystroke** — which is
the L-input concern below, not a per-modal defect.

### L-input — input reaches the modal under a burst

`Esc` is only as live as the run loop that delivers it. The loop is strictly
single-threaded and services **one input event per iteration**, dispatched
after drain/ticks/messages/render (`helpers.rs:1391-1418`). A keystroke
arriving mid-phase waits out the **longest work phase** — telemetry p90
676 ms, storms chaining ~8 back-to-back (#1031 §1, §6). Worse, the stale-
input guard **discards** buffered key/mouse events older than 500 ms
(`STALE_INPUT_MAX_AGE`, `helpers.rs:734`) so a recovered UI doesn't burst-
fire a backlog — meaning a stall long enough to age a keystroke past 500 ms
drops it (28 real dropped-input episodes measured, #1031 §6).

This is the shared root with #1030/#1031 and is **partially addressed there
already**: the sidebar-recompute coalescing (#1030, merged c8eb2591) and the
two hottest render memoizations (#1031, merged) cut the phase durations that
starve input. The remaining structural cure — **servicing pending input
between sub-steps of an expensive drain/render** so a keystroke pre-empts a
burst's tail — is called out in #1031 §6 as the one true fix while VT parse
stays on-thread. It is a run-loop change of real risk and is **not** taken
here; it belongs with the #1031 backlog item, not bundled into this modal-
liveness PR. This PR's L2 timeout is the safety net for exactly the window
where L-input is degraded: even if `Esc` is dropped, the modal still
self-dismisses.

### L3 — no UI-thread block on daemon/network

Audited every daemon/network touch on the UI thread:

- The daemon owns all PTYs, polling, and network IO; the client is a thin
  renderer over IPC (see `CLAUDE.md` "Client / daemon split").
- The run loop reads the daemon channel with **non-blocking `try_recv`**
  only, bounded by a receive budget (`drain_daemon_events`,
  `helpers.rs:518-560`); it never `.await`s a daemon reply.
- A keystroke that triggers IO emits an `IpcCommand` and returns
  immediately; the reply arrives later as a `UserEvent::Daemon` event. No
  request/response is awaited inline.
- Setup effects (`ListScopes`/`Detect`) run on a **`tokio::spawn`**, off the
  UI thread, delivering back through the `Loading` channel
  (`setup_screen.rs:177`) — which is precisely why they need the L2 timeout.
- The blocking `crossterm::event::read` runs on a **dedicated reader thread**
  (`helpers.rs:1127-1157`), not the UI thread.

**No UI-thread blocking wait on a daemon/network response was found.** L3
holds today. The `!Send` VT parse/render is on-thread by construction
(#1031 §5) but is CPU work, not a blocking wait — it is an L-input latency
concern, already covered above.

### Sleep/wake reconciliation

The wake path was audited against the incident's three log signatures:

1. **Stale terminal IDs after wake** (`client-requested resync failed …
   not found`). The client requests a resync for a terminal the daemon no
   longer has; the daemon replies `TerminalResyncUnavailable`
   (`spawn_handler.rs:9194`, and the not-found / timed-out siblings at
   `:9170`, `:9201`) — it does **not** error-storm. The client marks the
   slot `desynced` and clears the pending flag
   (`terminal_stack.rs:3355-3359`) and flashes one **retryable** notice
   (`events.rs:1656-1660`), not a per-event error. Graceful — no change
   needed.
2. **Bounded-channel overflow → ring resync** (`helpers.rs:656`). The
   channel is bounded, so the wake catch-up burst can no longer grow the
   backlog without bound; overflow degrades to a grid resync from the ring
   (`BacklogMonitor::observe_resyncs`, `helpers.rs:648`), a symptom of the
   UI thread being behind. Draining that burst within budget is the
   #1030/#1031 drain-cost surface (D-0 "budget bounds receiving, not
   handling", #1031 §3) — tracked there, not re-fixed here.
3. **Orphaned modal on a pre-sleep request.** This is the one net-new
   liveness hole and is what this PR closes: a modal awaiting a result
   issued before the sleep, whose reply is lost across the wake. The
   effect-level timeout resolves the common "provider stalled" case into a
   retryable error that continues the wizard; the modal backstop covers the
   residual "result produced but dropped on the overflowed channel" case
   with a notice + dismiss. Either way it no longer orphans.

No modal is tied to a terminal resync, so signatures 1–2 cannot orphan a
modal directly; only the generic `Loading`-await case (signature 3) could,
and it is now bounded.

## Status

| Property | State | Owner |
|----------|-------|-------|
| L1 — every modal `Esc`-dismissable from local state | **holds** (audited) | — |
| L2 — every awaiting modal has a timeout | **closed** — setup effects get a retryable 30 s timeout that continues the wizard, plus a `Loading` modal backstop with a user notice (this PR); other modals already had one | this PR |
| L3 — no UI-thread block on daemon/network | **holds** (audited) | — |
| Sleep/wake — resync-not-found handled without an error storm | **holds** (audited) | — |
| Sleep/wake — orphaned pre-sleep modal | **closed** by L2 (this PR) | this PR |
| L-input — input serviced within frame budget under a burst | **partial** — phase costs cut by #1030/#1031; mid-phase input pre-emption remains | #1031 §6 backlog |
| Bounded drain — burst coalesced within budget | **partial** — coalescing landed (#1030); drain-handling budget (D-0) open | #1031 §3 backlog |

This PR closes the liveness holes that are cleanly, locally fixable and
testable (L2, and the orphaned-modal wake case it subsumes) and documents
L1/L3 as holding. The two `partial` rows are latency-under-load work that
shares #1030/#1031's run-loop surface; folding them in here would rebuild the
run loop's input scheduling in a modal-liveness PR. They stay with the
#1031 backlog that owns that surface, so each change lands where it can be
profiled and reviewed against the telemetry it targets.
