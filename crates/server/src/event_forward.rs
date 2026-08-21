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
            // Something is queued behind a full channel. Race the next
            // raw event against the channel freeing a slot — biased
            // toward delivery so a sustained flood can't starve the
            // buffered lifecycle events / pending resync.
            tokio::select! {
                biased;
                _ = health.overloaded() => {
                    tracing::warn!(
                        capacity = lazybox_ipc::RAW_EVENT_CHANNEL_CAPACITY,
                        "event ingress overflowed — disconnecting slow client"
                    );
                    break;
                }
                permit = client_tx.reserve(), if state.has_buffered() => {
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

impl ForwardState {
    fn new(metrics: Arc<EventMetrics>) -> Self {
        Self {
            pending: VecDeque::new(),
            resync_queue: VecDeque::new(),
            resync_set: HashSet::new(),
            resync_debt: HashMap::new(),
            resync_unavailable_announced: HashSet::new(),
            covered_seq: HashMap::new(),
            metrics,
        }
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

    fn has_buffered(&self) -> bool {
        !self.pending.is_empty() || !self.resync_queue.is_empty()
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
                // Ordering rule: if anything is already buffered, or
                // this terminal is mid-resync, we cannot forward live
                // output without reordering it ahead of the queue — so
                // drop it and (re)schedule the resync, which carries
                // the up-to-date ring anyway.
                if self.has_buffered()
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
                // A terminal that exits no longer needs a resync.
                if let Event::TerminalExited { terminal_id, .. } = &other {
                    self.drop_resync(terminal_id);
                    self.covered_seq.remove(terminal_id);
                }
                // Lossless: send immediately when the channel has room
                // and nothing is queued ahead of it; otherwise enqueue
                // in order.
                if self.pending.is_empty() {
                    match client_tx.try_send(other) {
                        Ok(()) => ControlFlow::Continue(()),
                        Err(TrySendError::Full(evt)) => {
                            self.pending.push_back(evt);
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
                        self.pending.push_back(other);
                        ControlFlow::Continue(())
                    }
                }
            }
        }
    }

    /// Deliver one buffered item into a freed channel slot. Lossless
    /// events drain first (they're ordered ahead of resyncs); once
    /// they're gone, materialize one resync from the current ring.
    async fn deliver_one(
        &mut self,
        permit: tokio::sync::mpsc::Permit<'_, Event>,
        config: &ServerConfig,
    ) {
        if let Some(evt) = self.pending.pop_front() {
            permit.send(evt);
            return;
        }
        if let Some(terminal_id) = self.resync_queue.pop_front() {
            self.resync_set.remove(&terminal_id);
            let required_seq = self.resync_debt.get(&terminal_id).copied().unwrap_or(0);
            if let Some(snapshot) = resync_replay(config, terminal_id, required_seq).await {
                // The replay carries every chunk through `last_seq`; record
                // the floor so in-flight chunks already inside it are not
                // re-fed after the reset.
                self.mark_covered(terminal_id, snapshot.last_seq);
                self.resync_debt.remove(&terminal_id);
                self.resync_unavailable_announced.remove(&terminal_id);
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
                    bytes: Arc::<[u8]>::from(format!("chunk{seq}").into_bytes()),
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
                bytes: Arc::<[u8]>::from(vec![b'x']),
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
                bytes: Arc::<[u8]>::from(vec![b'y']),
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
                bytes: Arc::<[u8]>::from(b"B".to_vec()),
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
                    bytes: Arc::<[u8]>::from(vec![b'z']),
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
                    bytes: Arc::<[u8]>::from(vec![b'x']),
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
}
