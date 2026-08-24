//! Per-connection event forwarder: the single drop-and-resync point
//! between the daemon's raw event stream and a client's bounded inbound
//! channel.
//!
//! ## Why this exists
//!
//! The daemon emits one `Event::TerminalOutput` per PTY chunk. A chatty
//! agent can produce them faster than a client
//! consumes. The client-facing channel is bounded
//! ([`lazybox_ipc::EVENT_CHANNEL_CAPACITY`]) so inbound memory has a hard
//! ceiling — but a naive bounded send would either block the daemon
//! (starving every other client + the command path) or drop raw bytes
//! mid-stream (which corrupts the libghostty-vt parser on the consumer:
//! a half-eaten escape sequence garbles the screen).
//!
//! This forwarder resolves that:
//!
//! - **`TerminalOutput`** is `try_send`-ed. On a full channel it is
//!   *dropped* and the terminal is scheduled for resync. Output is the
//!   only droppable event — the bytes are recoverable from the daemon's
//!   replay ring.
//! - **A resync** (`Event::TerminalResync`) re-feeds the affected
//!   terminal's full ring once capacity returns, re-establishing a
//!   correct grid without the dropped bytes. Coalesced: one resync per
//!   terminal per congestion episode, no matter how many chunks were
//!   dropped.
//! - **Every other (lifecycle / structured) event is lossless.** It's
//!   buffered in order and delivered as capacity allows, never dropped.
//! - **Congestion is per terminal** (#1254): output is only quarantined
//!   behind this terminal's own debt or a queued event that mutates this
//!   terminal's stream (or a global `Snapshot`) — one overflowing
//!   terminal never converts the others' output into resync debt. Repeat
//!   replays for the same terminal are paced ([`RESYNC_DEBOUNCE`]) and
//!   capped at the snapshot replay budget, so recovery traffic cannot
//!   itself sustain the congestion it is recovering from.
//!
//! Both the raw ingress and lossless backlog are bounded. A client that stops
//! consuming long enough to exhaust either structured-event cap is
//! disconnected: continuing after dropping a lifecycle event would create a
//! silently corrupt client view, while buffering forever would make one stale
//! client an unbounded daemon-memory sink.

use crate::ServerConfig;
use crate::metrics::EventMetrics;
use lazybox_ipc::{Event, EventForward, TerminalId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;

/// Cap on how long a single backend ring snapshot may take while
/// building a resync. A wedged PTY must not stall the forwarder. Failure
/// leaves the terminal in resync debt; it never fabricates an empty
/// authoritative reset.
const RESYNC_SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(500);

/// Structured events are not reconstructible one by one. Retain a generous
/// bounded backlog, then fail the connection closed rather than drop them or
/// grow memory without limit.
const MAX_PENDING_STRUCTURED_EVENTS: usize = lazybox_ipc::RAW_EVENT_CHANNEL_CAPACITY;

/// Minimum spacing between two authoritative replays for the SAME
/// terminal (#1254). A replay is the fattest event the forwarder emits
/// (seed + ring, MiBs); re-materializing one per congestion episode with
/// no floor let the replies themselves refill the channel and
/// self-sustain the storm. Pace, never skip — the debt stays queued and
/// the replay is delivered when the window opens, exactly the
/// `LAG_RECOVERY_DEBOUNCE` posture the serve loop takes for bus-lag
/// snapshots. Other terminals are unaffected: pacing is per terminal and
/// `deliver_one` picks the first *ready* debtor.
const RESYNC_DEBOUNCE: Duration = Duration::from_secs(2);

/// Drain the raw event stream into the bounded client channel, applying
/// drop-and-resync to `TerminalOutput`. Runs until either end closes.
pub async fn forward_events(forward: EventForward, config: ServerConfig) {
    let EventForward {
        mut raw_rx,
        client_tx,
        health,
    } = forward;
    let mut state = ForwardState::new(config.event_metrics.clone());
    // The raw stream can close while lossless events / a pending resync
    // are still buffered behind a full channel. We must flush those
    // before exiting — otherwise the client loses a lifecycle event or
    // never recovers a desynced grid.
    let mut input_open = true;

    loop {
        if state.is_idle() {
            if !input_open {
                break;
            }
            // Nothing buffered and the channel can't be the bottleneck,
            // so just wait for the next raw event and route it.
            tokio::select! {
                biased;
                _ = health.overloaded() => {
                    tracing::warn!(
                        capacity = lazybox_ipc::RAW_EVENT_CHANNEL_CAPACITY,
                        "event ingress overflowed — disconnecting slow client"
                    );
                    break;
                }
                raw = raw_rx.recv() => {
                    match raw {
                        Some(evt) => {
                            if state.route(&client_tx, evt.into_event()).is_break() {
                                break;
                            }
                        }
                        None => input_open = false,
                    }
                }
            }
        } else {
            // Something is queued behind a full channel (or inside its
            // pacing window). Race the next raw event against the
            // channel freeing a slot — biased toward delivery so a
            // sustained flood can't starve the buffered lifecycle
            // events / pending resync. When everything queued is a
            // paced resync there is nothing to reserve a slot FOR yet;
            // arming the reserve anyway would busy-spin
            // (reserve → deliver nothing → reserve …), so delivery is
            // gated on readiness and the pace-deadline arm wakes the
            // loop when the earliest window opens — a quiescent stream
            // must not strand a paced debt forever.
            let now = tokio::time::Instant::now();
            let deliverable = state.has_deliverable(now);
            let pace_deadline = if deliverable {
                None
            } else {
                state.next_pace_deadline()
            };
            tokio::select! {
                biased;
                _ = health.overloaded() => {
                    tracing::warn!(
                        capacity = lazybox_ipc::RAW_EVENT_CHANNEL_CAPACITY,
                        "event ingress overflowed — disconnecting slow client"
                    );
                    break;
                }
                permit = client_tx.reserve(), if deliverable => {
                    match permit {
                        Ok(permit) => state.deliver_one(permit, &config).await,
                        Err(_) => break, // client gone
                    }
                }
                raw = raw_rx.recv(), if input_open => {
                    match raw {
                        Some(evt) => {
                            if state.route(&client_tx, evt.into_event()).is_break() {
                                break;
                            }
                        }
                        None => input_open = false,
                    }
                }
                _ = tokio::time::sleep_until(
                    pace_deadline.unwrap_or_else(tokio::time::Instant::now)
                ), if pace_deadline.is_some() => {
                    // Pacing elapsed — loop and re-evaluate readiness.
                }
            }
        }
    }
}

/// Bookkeeping for the in-order lossless queue and the set of terminals
/// owing a resync.
struct ForwardState {
    /// Lossless events that couldn't be sent immediately (channel was
    /// full), awaiting capacity. Low-volume by construction — only
    /// non-output events ever land here.
    pending: VecDeque<Event>,
    /// Terminals whose output was dropped and which therefore owe a
    /// resync, in first-dropped order. `resync_set` mirrors membership
    /// for O(1) dedupe.
    resync_queue: VecDeque<TerminalId>,
    resync_set: HashSet<TerminalId>,
    /// Highest dropped sequence each terminal's next authoritative replay
    /// must cover. Kept across failed snapshot attempts; the next output
    /// retries recovery instead of resuming a torn stream.
    resync_debt: HashMap<TerminalId, u64>,
    /// Terminals for which this recovery episode already surfaced an
    /// unavailable notice. Prevents a chatty terminal from spamming one
    /// lossless notice per retry.
    resync_unavailable_announced: HashSet<TerminalId>,
    /// Per-terminal count of queued lossless events that mutate that
    /// terminal's byte-stream state client-side (spawn/replace/exit,
    /// replays, deltas, scrollback). Live output for a terminal must
    /// never overtake one of THESE — but a queued event for terminal A
    /// says nothing about terminal B's stream, so B's output may still
    /// `try_send` (#1254: the process-wide `has_buffered()` gate turned
    /// one congested terminal into a dropped-output → resync episode for
    /// every terminal). Entries are removed at zero so membership is the
    /// gate.
    pending_stream_refs: HashMap<TerminalId, usize>,
    /// Count of queued events with process-wide stream impact — a
    /// `Snapshot` rebuilds every terminal's grid, so while one is queued
    /// all live output holds.
    pending_global_refs: usize,
    /// Per-terminal pacing floor for authoritative replays: no second
    /// `TerminalResync` for a terminal before this instant (see
    /// [`RESYNC_DEBOUNCE`]). Entries are dropped when the terminal
    /// exits.
    resync_paced_until: HashMap<TerminalId, tokio::time::Instant>,
    /// Highest per-terminal `seq` already delivered to the client inside
    /// a replay — either a `TerminalResync` (channel-overflow recovery)
    /// or a `Snapshot` (broadcast-lag recovery). The replay already
    /// contains every chunk through this seq, so any later
    /// `TerminalOutput` with `seq <= covered` is a duplicate. Re-feeding
    /// it into the consumer's parser double-draws the screen on top of
    /// the just-rebuilt grid — the "reload flicker / struck-through
    /// rows" of #103. Dropping it here is what makes the documented
    /// `TerminalResync` contract ("the resumed live stream — all seq
    /// strictly greater — applies exactly once") actually hold: the
    /// resync materializes from a ring that can be a few chunks ahead of
    /// what the forwarder has consumed from its raw input, so those
    /// in-flight chunks would otherwise arrive *after* the resync with a
    /// seq it already covered.
    covered_seq: HashMap<TerminalId, u64>,
    /// Process-wide drop/resync counters (issue #91).
    metrics: Arc<EventMetrics>,
}

use std::ops::ControlFlow;

/// Which terminals' byte-stream state a lossless event mutates on the
/// client — i.e. which terminals' live output must queue behind it to
/// preserve the ordering invariant `route` protects. Events that don't
/// touch a `TerminalStack` slot's stream reconstruction (workspace
/// upserts, agent-state badges, notifications, …) are `Free`: output
/// overtaking them cannot corrupt a grid, and treating them as global
/// was exactly the #1254 storm.
enum StreamScope {
    Free,
    Terminals([Option<TerminalId>; 2]),
    Global,
}

fn stream_scope(evt: &Event) -> StreamScope {
    match evt {
        // Rebuilds EVERY terminal's grid on the client.
        Event::Snapshot { .. } => StreamScope::Global,
        // Slot lifecycle + authoritative stream mutations for one
        // terminal. New stream-mutating event variants must be added
        // here, or their terminal's output may overtake them.
        Event::TerminalSpawned { terminal_id, .. }
        | Event::TerminalResync { terminal_id, .. }
        | Event::TerminalResyncUnavailable { terminal_id }
        | Event::TerminalDelta { terminal_id, .. }
        | Event::TerminalDeltaUnavailable { terminal_id }
        | Event::TerminalExited { terminal_id, .. }
        | Event::TerminalScrollback { terminal_id, .. }
        | Event::AgentAuthReplay { terminal_id, .. } => {
            StreamScope::Terminals([Some(*terminal_id), None])
        }
        Event::TerminalReplaced {
            old_terminal_id,
            terminal_id,
            ..
        } => StreamScope::Terminals([Some(*old_terminal_id), Some(*terminal_id)]),
        _ => StreamScope::Free,
    }
}

impl ForwardState {
    fn new(metrics: Arc<EventMetrics>) -> Self {
        Self {
            pending: VecDeque::new(),
            resync_queue: VecDeque::new(),
            resync_set: HashSet::new(),
            resync_debt: HashMap::new(),
            resync_unavailable_announced: HashSet::new(),
            pending_stream_refs: HashMap::new(),
            pending_global_refs: 0,
            resync_paced_until: HashMap::new(),
            covered_seq: HashMap::new(),
            metrics,
        }
    }

    /// Enqueue one lossless event, tracking which terminals' streams it
    /// gates. The ONLY writer of `pending` — bookkeeping and queue can't
    /// drift.
    fn push_pending(&mut self, evt: Event) {
        match stream_scope(&evt) {
            StreamScope::Free => {}
            StreamScope::Global => self.pending_global_refs += 1,
            StreamScope::Terminals(ids) => {
                for id in ids.into_iter().flatten() {
                    *self.pending_stream_refs.entry(id).or_insert(0) += 1;
                }
            }
        }
        self.pending.push_back(evt);
    }

    /// Dequeue the oldest lossless event, releasing its stream gates.
    /// The ONLY reader of `pending` — mirror of [`Self::push_pending`].
    fn pop_pending(&mut self) -> Option<Event> {
        let evt = self.pending.pop_front()?;
        match stream_scope(&evt) {
            StreamScope::Free => {}
            StreamScope::Global => {
                self.pending_global_refs = self.pending_global_refs.saturating_sub(1);
            }
            StreamScope::Terminals(ids) => {
                for id in ids.into_iter().flatten() {
                    if let Some(count) = self.pending_stream_refs.get_mut(&id) {
                        *count -= 1;
                        if *count == 0 {
                            self.pending_stream_refs.remove(&id);
                        }
                    }
                }
            }
        }
        Some(evt)
    }

    /// Does the lossless queue hold an event live output for
    /// `terminal_id` must not overtake?
    fn pending_blocks_output(&self, terminal_id: TerminalId) -> bool {
        self.pending_global_refs > 0 || self.pending_stream_refs.contains_key(&terminal_id)
    }

    /// Is this queued resync past its pacing floor?
    fn resync_ready(&self, terminal_id: TerminalId, now: tokio::time::Instant) -> bool {
        self.resync_paced_until
            .get(&terminal_id)
            .is_none_or(|&until| now >= until)
    }

    /// Anything a freed channel slot could carry right now — a lossless
    /// event, or a resync whose pacing window has opened.
    fn has_deliverable(&self, now: tokio::time::Instant) -> bool {
        !self.pending.is_empty() || self.resync_queue.iter().any(|t| self.resync_ready(*t, now))
    }

    /// Earliest pacing deadline among queued resyncs — the wake-up the
    /// forwarder arms when every queued debt is inside its window.
    fn next_pace_deadline(&self) -> Option<tokio::time::Instant> {
        self.resync_queue
            .iter()
            .filter_map(|t| self.resync_paced_until.get(t).copied())
            .min()
    }

    /// Record that `seq` (and everything before it) for `terminal_id`
    /// has been delivered inside a replay. Monotonic — a later, lower
    /// resync/snapshot seq never lowers the floor.
    fn mark_covered(&mut self, terminal_id: TerminalId, seq: u64) {
        let entry = self.covered_seq.entry(terminal_id).or_insert(0);
        *entry = (*entry).max(seq);
    }

    /// Has `seq` for `terminal_id` already been delivered via a replay?
    fn is_superseded(&self, terminal_id: TerminalId, seq: u64) -> bool {
        self.covered_seq
            .get(&terminal_id)
            .is_some_and(|&covered| seq <= covered)
    }

    fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.resync_queue.is_empty()
    }

    fn schedule_resync(&mut self, terminal_id: TerminalId, required_seq: u64) {
        // Every call here corresponds to one dropped output chunk; the
        // resync itself is coalesced (one per terminal per episode).
        let dropped_total = self.metrics.record_output_dropped();
        let debt = self.resync_debt.entry(terminal_id).or_insert(0);
        *debt = (*debt).max(required_seq);
        if self.resync_set.insert(terminal_id) {
            self.resync_queue.push_back(terminal_id);
            let resync_total = self.metrics.record_resync();
            tracing::warn!(
                ?terminal_id,
                output_dropped_total = dropped_total,
                resyncs_total = resync_total,
                "event channel full — dropping TerminalOutput, scheduled resync from ring"
            );
        }
    }

    fn drop_resync(&mut self, terminal_id: &TerminalId) {
        if self.resync_set.remove(terminal_id) {
            self.resync_queue.retain(|t| t != terminal_id);
        }
        self.resync_debt.remove(terminal_id);
        self.resync_unavailable_announced.remove(terminal_id);
    }

    /// Route one raw event toward the client. Returns `Break` when the
    /// client channel has closed and the forwarder should stop.
    fn route(
        &mut self,
        client_tx: &tokio::sync::mpsc::Sender<Event>,
        evt: Event,
    ) -> ControlFlow<()> {
        match evt {
            Event::TerminalOutput {
                terminal_id, seq, ..
            }
            | Event::AgentAuthOutput {
                terminal_id, seq, ..
            } if self.is_superseded(terminal_id, seq) => {
                // Already delivered inside a replay (resync/snapshot).
                // Forwarding it again would double-feed the consumer's
                // parser on top of the freshly rebuilt grid — #103's
                // reload flicker. The replay covers it, so drop silently
                // (not a lossy drop: no resync owed).
                ControlFlow::Continue(())
            }
            evt @ (Event::TerminalOutput {
                terminal_id, seq, ..
            }
            | Event::AgentAuthOutput {
                terminal_id, seq, ..
            }) => {
                // Ordering rule, PER TERMINAL (#1254): live output must
                // not overtake a queued structured event that mutates
                // THIS terminal's stream state (or a Snapshot, which
                // rebuilds every grid), and a terminal mid-resync stays
                // quarantined until its replay lands — forwarding would
                // reorder it ahead of the queue, so drop it and
                // (re)schedule the resync, which carries the up-to-date
                // ring anyway. Another terminal's backlog or debt never
                // gates this one: the old process-wide `has_buffered()`
                // gate converted one congested terminal into dropped
                // output — and therefore a fat resync — for every
                // terminal, and the replies refilled the channel until
                // the client was disconnected as slow.
                if self.pending_blocks_output(terminal_id)
                    || self.resync_set.contains(&terminal_id)
                    || self.resync_debt.contains_key(&terminal_id)
                {
                    self.schedule_resync(terminal_id, seq);
                    return ControlFlow::Continue(());
                }
                match client_tx.try_send(evt) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(TrySendError::Full(_)) => {
                        self.schedule_resync(terminal_id, seq);
                        ControlFlow::Continue(())
                    }
                    Err(TrySendError::Closed(_)) => ControlFlow::Break(()),
                }
            }
            other => {
                // A broadcast-lag recovery `Snapshot` re-feeds each
                // terminal's full ring, so it covers everything through
                // its `last_seq` exactly like a resync does — record the
                // floor so the lagged backlog that follows on the bus
                // (older chunks, `seq <= last_seq`) is dropped instead of
                // double-fed.
                if let Event::Snapshot { terminals, .. } = &other {
                    for t in terminals {
                        if t.replay_available {
                            self.mark_covered(t.terminal_id, t.last_seq);
                            self.drop_resync(&t.terminal_id);
                        }
                    }
                }
                // Pump-initiated and client-requested resyncs also carry
                // authoritative coverage. Record it here so any older raw
                // output already in this connection's queue is suppressed.
                if let Event::TerminalResync {
                    terminal_id, seq, ..
                }
                | Event::AgentAuthReplay {
                    terminal_id, seq, ..
                } = &other
                {
                    self.mark_covered(*terminal_id, *seq);
                    self.drop_resync(terminal_id);
                }
                // A terminal that exits no longer needs a resync — nor a
                // pacing floor.
                if let Event::TerminalExited { terminal_id, .. } = &other {
                    self.drop_resync(terminal_id);
                    self.covered_seq.remove(terminal_id);
                    self.resync_paced_until.remove(terminal_id);
                }
                // Lossless: send immediately when the channel has room
                // and nothing is queued ahead of it; otherwise enqueue
                // in order.
                if self.pending.is_empty() {
                    match client_tx.try_send(other) {
                        Ok(()) => ControlFlow::Continue(()),
                        Err(TrySendError::Full(evt)) => {
                            self.push_pending(evt);
                            ControlFlow::Continue(())
                        }
                        Err(TrySendError::Closed(_)) => ControlFlow::Break(()),
                    }
                } else {
                    if self.pending.len() >= MAX_PENDING_STRUCTURED_EVENTS {
                        tracing::warn!(
                            capacity = MAX_PENDING_STRUCTURED_EVENTS,
                            "structured event backlog exhausted — disconnecting slow client"
                        );
                        ControlFlow::Break(())
                    } else {
                        self.push_pending(other);
                        ControlFlow::Continue(())
                    }
                }
            }
        }
    }

    /// Deliver one buffered item into a freed channel slot. Lossless
    /// events drain first (they're ordered ahead of resyncs); once
    /// they're gone, materialize one resync from the current ring —
    /// picking the first debtor whose pacing window is open, so one
    /// chatty terminal inside its [`RESYNC_DEBOUNCE`] never stalls
    /// another terminal's recovery.
    async fn deliver_one(
        &mut self,
        permit: tokio::sync::mpsc::Permit<'_, Event>,
        config: &ServerConfig,
    ) {
        if let Some(evt) = self.pop_pending() {
            permit.send(evt);
            return;
        }
        let now = tokio::time::Instant::now();
        let ready = self
            .resync_queue
            .iter()
            .position(|t| self.resync_ready(*t, now));
        let Some(terminal_id) = ready.and_then(|pos| self.resync_queue.remove(pos)) else {
            // Every queued debt is inside its pacing window; the permit
            // is released and the forwarder's pace-deadline arm re-arms
            // delivery when the earliest window opens.
            return;
        };
        self.resync_set.remove(&terminal_id);
        let required_seq = self.resync_debt.get(&terminal_id).copied().unwrap_or(0);
        if let Some(snapshot) = resync_replay(config, terminal_id, required_seq).await {
            // Same whole-replay byte budget the Subscribe / bus-lag
            // snapshot enforces (`budget_snapshot_replay`): a replay
            // is atomic — slicing a prefix can begin inside
            // UTF-8/CSI state — so an over-budget one is omitted
            // whole and announced unavailable. The client preserves
            // its last coherent grid and retries with backoff.
            if snapshot.replay.len() > crate::SNAPSHOT_REPLAY_BUDGET {
                tracing::warn!(
                    ?terminal_id,
                    replay_bytes = snapshot.replay.len(),
                    budget = crate::SNAPSHOT_REPLAY_BUDGET,
                    "resync replay exceeds snapshot budget — reporting unavailable"
                );
                if self.resync_unavailable_announced.insert(terminal_id) {
                    permit.send(Event::TerminalResyncUnavailable { terminal_id });
                }
                return;
            }
            // The replay carries every chunk through `last_seq`; record
            // the floor so in-flight chunks already inside it are not
            // re-fed after the reset.
            self.mark_covered(terminal_id, snapshot.last_seq);
            self.resync_debt.remove(&terminal_id);
            self.resync_unavailable_announced.remove(&terminal_id);
            // Pace only materialized replays: they're the fat events
            // that refill the channel. An unavailable notice is tiny
            // and must not delay the retry that finally converges.
            self.resync_paced_until
                .insert(terminal_id, now + RESYNC_DEBOUNCE);
            permit.send(Event::TerminalResync {
                terminal_id,
                replay: snapshot.replay,
                seq: snapshot.last_seq,
            });
        } else if self.resync_unavailable_announced.insert(terminal_id) {
            permit.send(Event::TerminalResyncUnavailable { terminal_id });
        }
        // On repeated failure the permit is simply released.
        // `resync_debt` remains, and the next output retries.
    }
}

/// Fetch an authoritative daemon-side replay covering `required_seq`.
/// Returns `None` on absence, failure, timeout, or a stale snapshot.
/// Callers must preserve their last known screen and retry.
///
/// A `TerminalResync` REPLACES the client's grid with the replay, and the
/// backend's `snapshot` returns the ring's line-boundary-clean
/// `replay_snapshot` (`ReplayRing::replay_snapshot_into`) — VT-safe even after
/// the ring has wrapped, because it drops the partial leading line so the
/// replay starts on a clean boundary. So an *incomplete* (wrapped) ring is a
/// perfectly good resync source: the client adopts a correct, if
/// shorter-history, screen. This is the SAME seed every other consumer of a
/// wrapped ring now serves — fresh attach (`snapshot_terminals`), a
/// client-requested resync (`handle_terminal_resync_request`), and the pump's
/// gap recovery (`resync_replay_after_gap`) — so completeness never gates
/// reconstruction anywhere. Rejecting `!complete` here instead froze every
/// terminal that had ever produced more than the ring capacity — once
/// wrapped, `is_complete()` is false forever, so the first channel overflow
/// scheduled a resync that could never succeed and `route` then dropped all
/// further output for that terminal. Only genuine unavailability (backend
/// error/timeout) or a snapshot that doesn't even reach the gap
/// (`last_seq < required_seq`) is a real miss.
async fn resync_replay(
    config: &ServerConfig,
    terminal_id: TerminalId,
    required_seq: u64,
) -> Option<crate::backend::ReplaySnapshot> {
    let key = config.terminal.backend_key_for(terminal_id).await;
    let Some(key) = key else {
        return None;
    };
    match tokio::time::timeout(RESYNC_SNAPSHOT_TIMEOUT, config.backend.snapshot(&key)).await {
        Ok(Ok(snapshot)) if snapshot.last_seq < required_seq => {
            tracing::warn!(
                ?terminal_id,
                required_seq,
                snapshot_seq = snapshot.last_seq,
                "resync snapshot is stale; preserving client state"
            );
            None
        }
        Ok(Ok(snapshot)) => Some(snapshot),
        Ok(Err(e)) => {
            tracing::warn!(?terminal_id, "resync snapshot failed: {e}");
            None
        }
        Err(_) => {
            tracing::warn!(?terminal_id, "resync snapshot timed out");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerConfig;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{TerminalKind, TerminalSnapshot};
    use std::ops::ControlFlow;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// Drain `rx` until it closes (forwarder exits) or a generous
    /// timeout, returning everything delivered. A timeout fails the
    /// test loudly rather than hanging CI.
    async fn collect(mut rx: mpsc::Receiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(e)) => out.push(e),
                Ok(None) => break,
                Err(_) => panic!("forwarder did not converge — {} events so far", out.len()),
            }
        }
        out
    }

    #[test]
    fn structured_backlog_disconnects_at_its_declared_cap() {
        let metrics = ServerConfig::in_memory().event_metrics;
        let mut state = ForwardState::new(metrics);
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(Event::Notification {
            title: "occupy client slot".into(),
            body: String::new(),
        })
        .expect("client capacity");

        for index in 0..MAX_PENDING_STRUCTURED_EVENTS {
            assert!(matches!(
                state.route(
                    &tx,
                    Event::Notification {
                        title: "queued".into(),
                        body: index.to_string(),
                    },
                ),
                ControlFlow::Continue(())
            ));
        }
        assert_eq!(state.pending.len(), MAX_PENDING_STRUCTURED_EVENTS);
        assert!(matches!(
            state.route(
                &tx,
                Event::Notification {
                    title: "must disconnect".into(),
                    body: String::new(),
                },
            ),
            ControlFlow::Break(())
        ));
    }

    #[tokio::test]
    async fn raw_ingress_overload_terminates_the_forwarder() {
        let config = ServerConfig::in_memory();
        let (client_tx, mut client_rx) = mpsc::channel(1);
        let (raw_tx, forward) = lazybox_ipc::event_forward_channel(client_tx);
        for index in 0..lazybox_ipc::RAW_EVENT_CHANNEL_CAPACITY {
            raw_tx
                .send(Event::Notification {
                    title: "raw".into(),
                    body: index.to_string(),
                })
                .expect("raw capacity");
        }
        assert!(matches!(
            raw_tx.send(Event::Notification {
                title: "overflow".into(),
                body: String::new(),
            }),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));

        tokio::time::timeout(Duration::from_secs(1), forward_events(forward, config))
            .await
            .expect("overloaded forwarder exits");
        assert!(client_rx.recv().await.is_none());
    }

    /// A flood that overruns the bounded client channel drops
    /// `TerminalOutput` and recovers with exactly one `TerminalResync`
    /// carrying the daemon ring — and the interleaved lifecycle event
    /// is never lost.
    #[tokio::test]
    async fn overflow_drops_output_and_resyncs_from_ring() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        // Seed the backend ring with the same 20 sequenced chunks the
        // raw forwarder receives below. A valid resync must cover the
        // highest dropped sequence; stale snapshots are rejected.
        let mut ring = Vec::new();
        for seq in 1..=20 {
            let bytes = format!("chunk{seq}").into_bytes();
            ring.extend_from_slice(&bytes);
            mock.emit(&key, bytes).await;
        }
        let tid = TerminalId(1);
        config.terminal.bind_backend(tid, key.clone()).await;

        // Tiny channel so a short flood forces overflow.
        let (client_tx, client_rx) = mpsc::channel(2);
        let (raw_tx, forward) = lazybox_ipc::event_forward_channel(client_tx);
        let task = tokio::spawn(forward_events(forward, config.clone()));

        // 20 chunks into a depth-2 channel → most get dropped.
        for seq in 1..=20 {
            raw_tx
                .send(Event::TerminalOutput {
                    terminal_id: tid,
                    bytes: std::sync::Arc::<[u8]>::from(format!("chunk{seq}").into_bytes()),
                    first_seq: seq,
                    seq,
                })
                .unwrap();
        }
        // A lifecycle event mid-flood must survive the byte-stream drops.
        raw_tx
            .send(Event::TerminalExited {
                terminal_id: TerminalId(2),
                exit_code: Some(0),
                last_output: None,
            })
            .unwrap();
        drop(raw_tx); // let the forwarder finish and close the channel

        let got = collect(client_rx).await;
        task.await.unwrap();

        // Lifecycle event delivered losslessly.
        assert!(
            got.iter()
                .any(|e| matches!(e, Event::TerminalExited { terminal_id, .. } if *terminal_id == TerminalId(2))),
            "lifecycle event was dropped: {got:?}"
        );
        // Exactly one resync, carrying the ring + its last seq.
        let resyncs: Vec<_> = got
            .iter()
            .filter_map(|e| match e {
                Event::TerminalResync {
                    terminal_id,
                    replay,
                    seq,
                } if *terminal_id == tid => Some((replay.clone(), *seq)),
                _ => None,
            })
            .collect();
        assert_eq!(resyncs.len(), 1, "expected exactly one resync: {got:?}");
        assert_eq!(resyncs[0].0, ring);
        assert_eq!(resyncs[0].1, 20);
        // We dropped output: fewer than 20 TerminalOutput got through.
        let outputs = got
            .iter()
            .filter(|e| matches!(e, Event::TerminalOutput { .. }))
            .count();
        assert!(outputs < 20, "nothing was dropped ({outputs} forwarded)");
        // The drop and the resync episode are counted (issue #91).
        let snap = config.event_metrics.snapshot();
        assert_eq!(
            snap.terminal_output_dropped as usize,
            20 - outputs,
            "every dropped chunk is counted"
        );
        assert_eq!(snap.terminal_resyncs, 1, "one resync episode counted");
    }

    /// #103: after a resync re-feeds the ring up to `seq`, the resumed
    /// live stream can still carry chunks the resync already covered —
    /// the daemon ring runs a few chunks ahead of what the forwarder has
    /// consumed from its raw input, so those in-flight chunks reach
    /// `route` *after* the resync with a seq it already replayed.
    /// Re-forwarding them double-feeds the consumer's parser (the reload
    /// flicker). `deliver_one` records the resync floor; later duplicates
    /// must be dropped and only strictly-newer chunks forwarded.
    #[tokio::test]
    async fn resync_floor_drops_already_replayed_output() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"RING").await; // ring last_seq → 1
        let tid = TerminalId(1);
        config.terminal.bind_backend(tid, key.clone()).await;

        let mut state = ForwardState::new(config.event_metrics.clone());
        // A dropped chunk schedules the resync; materializing it records
        // the floor (the ring's last_seq = 1).
        state.schedule_resync(tid, 1);
        let (tx, mut rx) = mpsc::channel(8);
        let permit = tx.reserve().await.unwrap();
        state.deliver_one(permit, &config).await;
        assert!(
            matches!(rx.recv().await, Some(Event::TerminalResync { seq: 1, .. })),
            "resync carries the ring's last seq",
        );

        // Chunk seq=1 is already in the replay → dropped.
        let cf = state.route(
            &tx,
            Event::TerminalOutput {
                terminal_id: tid,
                bytes: std::sync::Arc::<[u8]>::from(vec![b'x']),
                first_seq: 1,
                seq: 1,
            },
        );
        assert!(matches!(cf, ControlFlow::Continue(())));
        // Chunk seq=2 is strictly newer → forwarded.
        let cf = state.route(
            &tx,
            Event::TerminalOutput {
                terminal_id: tid,
                bytes: std::sync::Arc::<[u8]>::from(vec![b'y']),
                first_seq: 2,
                seq: 2,
            },
        );
        assert!(matches!(cf, ControlFlow::Continue(())));

        drop(tx);
        let mut seqs = Vec::new();
        while let Some(e) = rx.recv().await {
            if let Event::TerminalOutput { seq, .. } = e {
                seqs.push(seq);
            }
        }
        assert_eq!(seqs, vec![2], "only the post-resync chunk survives");
    }

    /// A wrapped ring (`complete: false`) must still serve a resync. Once
    /// a terminal produces more than the ring capacity, `is_complete()` is
    /// false forever; rejecting that froze the terminal after its first
    /// channel overflow (the resync could never succeed, so `route` dropped
    /// all further output). The backend's snapshot is line-boundary-clean,
    /// so an incomplete ring is a valid — if shorter-history — reset.
    #[tokio::test]
    async fn incomplete_wrapped_ring_still_serves_a_resync() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"screen-state").await; // seq 1
        // The ring has wrapped past its capacity — snapshot reports
        // incomplete, exactly as a >2 MiB agent's ring does.
        mock.mark_snapshot_incomplete(&key).await;
        let tid = TerminalId(1);
        config.terminal.bind_backend(tid, key.clone()).await;

        let mut state = ForwardState::new(config.event_metrics.clone());
        let (tx, mut rx) = mpsc::channel(8);
        state.schedule_resync(tid, 1);
        let permit = tx.reserve().await.expect("permit");
        state.deliver_one(permit, &config).await;

        // The resync is served from the wrapped ring, not refused.
        assert!(
            matches!(
                rx.try_recv(),
                Ok(Event::TerminalResync { terminal_id, replay, seq })
                    if terminal_id == tid && replay == b"screen-state" && seq == 1
            ),
            "a wrapped-but-boundary-clean ring must serve the resync",
        );
        // Debt cleared → `route` resumes forwarding live output.
        assert!(!state.resync_debt.contains_key(&tid));
    }

    /// Snapshot failure must not become an empty authoritative reset or
    /// clear the resync debt. The next output is dropped, retries the
    /// snapshot, and only a complete replay covering that output is sent.
    #[tokio::test]
    async fn failed_resync_preserves_debt_and_retries_without_empty_reset() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"A").await;
        mock.fail_next_snapshots(&key, 1).await;
        let tid = TerminalId(1);
        config.terminal.bind_backend(tid, key.clone()).await;

        let mut state = ForwardState::new(config.event_metrics.clone());
        let (tx, mut rx) = mpsc::channel(8);
        state.schedule_resync(tid, 1);
        let permit = tx.reserve().await.expect("permit");
        state.deliver_one(permit, &config).await;
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::TerminalResyncUnavailable { terminal_id }) if terminal_id == tid
        ));
        assert_eq!(state.resync_debt.get(&tid), Some(&1));

        mock.emit(&key, b"B").await;
        let _ = state.route(
            &tx,
            Event::TerminalOutput {
                terminal_id: tid,
                bytes: std::sync::Arc::<[u8]>::from(b"B".to_vec()),
                first_seq: 2,
                seq: 2,
            },
        );
        assert!(rx.try_recv().is_err(), "torn output must stay suppressed");
        let permit = tx.reserve().await.expect("retry permit");
        state.deliver_one(permit, &config).await;
        assert!(matches!(
            rx.recv().await,
            Some(Event::TerminalResync { replay, seq: 2, .. }) if replay == b"AB"
        ));
        assert!(!state.resync_debt.contains_key(&tid));
    }

    /// A broadcast-lag recovery `Snapshot` covers each terminal through
    /// its `last_seq` just like a resync. The lagged backlog that follows
    /// on the bus (older chunks, `seq <= last_seq`) must be dropped, not
    /// double-fed on top of the snapshot's replay.
    #[tokio::test]
    async fn snapshot_floor_drops_lagged_backlog() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let mut state = ForwardState::new(config.event_metrics.clone());
        let tid = TerminalId(1);
        let (tx, mut rx) = mpsc::channel(16);

        // Recovery snapshot: terminal covered through seq 5.
        let snap = Event::Snapshot {
            workspaces: Vec::new(),
            terminals: vec![TerminalSnapshot {
                terminal_id: tid,
                session_key: SessionKey::new("s"),
                kind: TerminalKind::Shell,
                replay: b"REPLAY".to_vec(),
                last_seq: 5,
                replay_available: true,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: Vec::new(),
                composing_buffer: None,
                agent_state: None,
                authenticating: false,
            }],
            projects: Vec::new(),
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        };
        assert!(matches!(state.route(&tx, snap), ControlFlow::Continue(())));

        // Backlog chunks 3..=5 are already in the replay → dropped; 6..=7
        // are new → forwarded.
        for seq in 3..=7 {
            let _ = state.route(
                &tx,
                Event::TerminalOutput {
                    terminal_id: tid,
                    bytes: std::sync::Arc::<[u8]>::from(vec![b'z']),
                    first_seq: seq,
                    seq,
                },
            );
        }

        drop(tx);
        let mut seqs = Vec::new();
        while let Some(e) = rx.recv().await {
            if let Event::TerminalOutput { seq, .. } = e {
                seqs.push(seq);
            }
        }
        assert_eq!(
            seqs,
            vec![6, 7],
            "only chunks past the snapshot floor survive"
        );
    }

    /// #1254 finding 1: congestion is per terminal. One terminal in
    /// resync debt must not drop the OTHER terminals' output (the old
    /// process-wide `has_buffered()` gate converted one congested
    /// terminal into a resync for every terminal), and repeat replays
    /// for the congested terminal are paced — O(1) resync events per
    /// congestion episode, with the deferred debt still converging once
    /// the pacing window opens.
    #[tokio::test(start_paused = true)]
    async fn resync_storm_does_not_amplify_across_terminals() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"ring-1").await; // congested terminal's ring, seq 1
        let congested = TerminalId(1);
        config.terminal.bind_backend(congested, key.clone()).await;

        let mut state = ForwardState::new(config.event_metrics.clone());
        let (tx, mut rx) = mpsc::channel(64);

        // The congested terminal's output overflowed: it owes a resync.
        state.schedule_resync(congested, 1);

        // Healthy terminals keep streaming right through the episode.
        for t in 2..=5u64 {
            let cf = state.route(
                &tx,
                Event::TerminalOutput {
                    terminal_id: TerminalId(t),
                    bytes: vec![b'x'].into(),
                    first_seq: 1,
                    seq: 1,
                },
            );
            assert!(matches!(cf, ControlFlow::Continue(())));
        }
        let mut streamed = 0;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                Event::TerminalOutput { terminal_id, .. } => {
                    assert_ne!(terminal_id, congested);
                    streamed += 1;
                }
                other => panic!("unexpected event during the episode: {other:?}"),
            }
        }
        assert_eq!(
            streamed, 4,
            "one terminal's debt must not quarantine the others' output"
        );
        assert_eq!(
            state.resync_queue.len(),
            1,
            "the episode owes exactly one terminal's resync — no cross-terminal debt"
        );

        // The episode resolves with exactly one replay.
        let permit = tx.reserve().await.expect("permit");
        state.deliver_one(permit, &config).await;
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::TerminalResync { terminal_id, seq: 1, .. }) if terminal_id == congested
        ));

        // Renewed congestion inside the pacing window must not amplify
        // into another fat replay...
        mock.emit(&key, b"ring-2").await; // seq 2
        state.schedule_resync(congested, 2);
        let permit = tx.reserve().await.expect("permit");
        state.deliver_one(permit, &config).await;
        assert!(
            rx.try_recv().is_err(),
            "a repeat replay within the pacing window is deferred, not amplified"
        );
        assert!(
            state.has_deliverable(tokio::time::Instant::now() + RESYNC_DEBOUNCE),
            "the deferred debt stays queued for the next window"
        );

        // ...but the debt still converges once the window opens
        // (pace, never skip).
        tokio::time::advance(RESYNC_DEBOUNCE).await;
        let permit = tx.reserve().await.expect("permit");
        state.deliver_one(permit, &config).await;
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::TerminalResync { terminal_id, seq: 2, .. }) if terminal_id == congested
        ));
    }

    /// With a roomy channel and a consumer that keeps up, nothing is
    /// dropped and no resync is emitted — the drop path is overflow-only.
    #[tokio::test]
    async fn no_overflow_forwards_everything_losslessly() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let (client_tx, mut client_rx) = mpsc::channel(64);
        let (raw_tx, forward) = lazybox_ipc::event_forward_channel(client_tx);
        let task = tokio::spawn(forward_events(forward, config.clone()));

        let sk = SessionKey::new("s");
        raw_tx
            .send(Event::TerminalSpawned {
                terminal_id: TerminalId(1),
                session_key: sk.clone(),
                kind: TerminalKind::Shell,
                no_permission: false,
                on_main: false,
                model_label: None,
            })
            .unwrap();
        for seq in 1..=10 {
            raw_tx
                .send(Event::TerminalOutput {
                    terminal_id: TerminalId(1),
                    bytes: std::sync::Arc::<[u8]>::from(vec![b'x']),
                    first_seq: seq,
                    seq,
                })
                .unwrap();
        }
        drop(raw_tx);

        let mut outputs = 0;
        let mut resyncs = 0;
        while let Some(e) = client_rx.recv().await {
            match e {
                Event::TerminalOutput { .. } => outputs += 1,
                Event::TerminalResync { .. } => resyncs += 1,
                _ => {}
            }
        }
        task.await.unwrap();
        assert_eq!(outputs, 10, "all output should pass through");
        assert_eq!(resyncs, 0, "no resync without overflow");
        let snap = config.event_metrics.snapshot();
        assert_eq!(snap.terminal_output_dropped, 0, "nothing dropped");
        assert_eq!(snap.terminal_resyncs, 0, "no resync counted");
    }

    /// Two clients where one falls behind and detects a gap: only the
    /// affected client gets a resync, not both. This verifies that gap
    /// recovery is scoped per-connection, not broadcast to all subscribers.
    #[tokio::test]
    #[ignore = "consolidation: #1277 gap-resync test is WIP (expects fast=20 on a size-10 channel with no concurrent drain, and originally had a use-after-move); needs rework"]
    async fn gap_resync_scoped_to_affected_client_only() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        // Seed the backend ring with 20 chunks
        let mut ring = Vec::new();
        for seq in 1..=20 {
            let bytes = format!("chunk{seq}").into_bytes();
            ring.extend_from_slice(&bytes);
            mock.emit(&key, bytes).await;
        }
        let tid = TerminalId(1);
        config.terminal.bind_backend(tid, key.clone()).await;

        // Create two clients with different channel sizes:
        // - client_fast has a roomy channel (10 slots)
        // - client_slow has a tiny channel (2 slots)
        let (client_fast_tx, mut client_fast_rx) = mpsc::channel(10);
        let (client_slow_tx, mut client_slow_rx) = mpsc::channel(2);
        // Two independent forwarders reading from independent raw streams,
        // sending the same events to both clients.
        let (raw_tx_fast, forward_fast) = lazybox_ipc::event_forward_channel(client_fast_tx);
        let (raw_tx_slow, forward_slow) = lazybox_ipc::event_forward_channel(client_slow_tx);

        let task_fast = tokio::spawn(forward_events(forward_fast, config.clone()));
        let task_slow = tokio::spawn(forward_events(forward_slow, config.clone()));

        // Send events to fast client: spawned + 20 chunks
        raw_tx_fast
            .send(Event::TerminalSpawned {
                terminal_id: tid,
                session_key: SessionKey::new("s"),
                kind: TerminalKind::Shell,
                no_permission: false,
                on_main: false,
                model_label: None,
            })
            .unwrap();

        // Send events to slow client: spawned
        raw_tx_slow
            .send(Event::TerminalSpawned {
                terminal_id: tid,
                session_key: SessionKey::new("s"),
                kind: TerminalKind::Shell,
                no_permission: false,
                on_main: false,
                model_label: None,
            })
            .unwrap();

        // Send first 20 chunks to fast client (room for all)
        for seq in 1..=20 {
            raw_tx_fast
                .send(Event::TerminalOutput {
                    terminal_id: tid,
                    bytes: format!("chunk{seq}").into_bytes().into(),
                    first_seq: seq,
                    seq,
                })
                .unwrap();
        }

        // Send first 20 chunks to slow client, but it will overflow at seq 2
        for seq in 1..=20 {
            let _ = raw_tx_slow.send(Event::TerminalOutput {
                terminal_id: tid,
                bytes: format!("chunk{seq}").into_bytes().into(),
                first_seq: seq,
                seq,
            });
        }

        drop(raw_tx_fast);
        drop(raw_tx_slow);

        // Collect events from fast client
        let mut fast_outputs = 0;
        let mut fast_resyncs = 0;
        while let Ok(e) =
            tokio::time::timeout(Duration::from_millis(100), client_fast_rx.recv()).await
        {
            match e {
                Some(Event::TerminalOutput { .. }) => fast_outputs += 1,
                Some(Event::TerminalResync { .. }) => fast_resyncs += 1,
                None => break,
                _ => {}
            }
        }

        // Collect events from slow client
        let mut slow_outputs = 0;
        let mut slow_resyncs = 0;
        while let Ok(e) =
            tokio::time::timeout(Duration::from_millis(100), client_slow_rx.recv()).await
        {
            match e {
                Some(Event::TerminalOutput { .. }) => slow_outputs += 1,
                Some(Event::TerminalResync { .. }) => slow_resyncs += 1,
                None => break,
                _ => {}
            }
        }

        task_fast.await.ok();
        task_slow.await.ok();

        // Fast client should get all 20 chunks without resync (no overflow)
        assert_eq!(fast_outputs, 20, "fast client gets all output");
        assert_eq!(fast_resyncs, 0, "fast client needs no resync");

        // Slow client should get fewer chunks due to overflow, then a resync
        assert!(slow_outputs < 20, "slow client drops some output");
        assert_eq!(slow_resyncs, 1, "slow client gets exactly one resync");
    }
}
