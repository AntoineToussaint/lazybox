//! Per-connection event forwarder: the single drop-and-resync point
//! between the daemon's raw event stream and a client's bounded inbound
//! channel.
//!
//! ## Why this exists
//!
//! The daemon emits one `Event::TerminalOutput` per PTY chunk. A chatty
//! agent (Claude streaming) can produce them faster than a client
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
//! The forwarder always drains its raw input promptly (output is
//! dropped in O(1), lossless events move to a small in-order queue), so
//! the unbounded raw channel never accumulates. The hard ceiling is the
//! bounded client channel; the only thing that can grow is the
//! low-volume lossless queue.

use crate::ServerConfig;
use lazybox_ipc::{Event, EventForward, TerminalId};
use std::collections::{HashSet, VecDeque};
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;

/// Cap on how long a single backend ring snapshot may take while
/// building a resync. A wedged PTY must not stall the forwarder; we'd
/// rather ship an empty replay (blank grid, self-heals on next output)
/// than hang. Matches the spawn handler's per-session snapshot budget.
const RESYNC_SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(500);

/// Drain the raw event stream into the bounded client channel, applying
/// drop-and-resync to `TerminalOutput`. Runs until either end closes.
pub async fn forward_events(forward: EventForward, config: ServerConfig) {
    let EventForward {
        mut raw_rx,
        client_tx,
    } = forward;
    let mut state = ForwardState::default();
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
            match raw_rx.recv().await {
                Some(evt) => {
                    if state.route(&client_tx, evt).is_break() {
                        break;
                    }
                }
                None => input_open = false,
            }
        } else {
            // Something is queued behind a full channel. Race the next
            // raw event against the channel freeing a slot — biased
            // toward delivery so a sustained flood can't starve the
            // buffered lifecycle events / pending resync.
            tokio::select! {
                biased;
                permit = client_tx.reserve(), if state.has_buffered() => {
                    match permit {
                        Ok(permit) => state.deliver_one(permit, &config).await,
                        Err(_) => break, // client gone
                    }
                }
                raw = raw_rx.recv(), if input_open => {
                    match raw {
                        Some(evt) => {
                            if state.route(&client_tx, evt).is_break() {
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
#[derive(Default)]
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
}

use std::ops::ControlFlow;

impl ForwardState {
    fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.resync_queue.is_empty()
    }

    fn has_buffered(&self) -> bool {
        !self.pending.is_empty() || !self.resync_queue.is_empty()
    }

    fn schedule_resync(&mut self, terminal_id: TerminalId) {
        if self.resync_set.insert(terminal_id) {
            self.resync_queue.push_back(terminal_id);
            tracing::warn!(
                ?terminal_id,
                "event channel full — dropping TerminalOutput, scheduled resync from ring"
            );
        }
    }

    fn drop_resync(&mut self, terminal_id: &TerminalId) {
        if self.resync_set.remove(terminal_id) {
            self.resync_queue.retain(|t| t != terminal_id);
        }
    }

    /// Route one raw event toward the client. Returns `Break` when the
    /// client channel has closed and the forwarder should stop.
    fn route(
        &mut self,
        client_tx: &tokio::sync::mpsc::Sender<Event>,
        evt: Event,
    ) -> ControlFlow<()> {
        match evt {
            Event::TerminalOutput { terminal_id, .. } => {
                // Ordering rule: if anything is already buffered, or
                // this terminal is mid-resync, we cannot forward live
                // output without reordering it ahead of the queue — so
                // drop it and (re)schedule the resync, which carries
                // the up-to-date ring anyway.
                if self.has_buffered() || self.resync_set.contains(&terminal_id) {
                    self.schedule_resync(terminal_id);
                    return ControlFlow::Continue(());
                }
                match client_tx.try_send(evt) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(TrySendError::Full(_)) => {
                        self.schedule_resync(terminal_id);
                        ControlFlow::Continue(())
                    }
                    Err(TrySendError::Closed(_)) => ControlFlow::Break(()),
                }
            }
            other => {
                // A terminal that exits no longer needs a resync.
                if let Event::TerminalExited { terminal_id, .. } = &other {
                    self.drop_resync(terminal_id);
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
                    self.pending.push_back(other);
                    ControlFlow::Continue(())
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
            let (replay, seq) = resync_replay(config, terminal_id).await;
            permit.send(Event::TerminalResync {
                terminal_id,
                replay,
                seq,
            });
        }
    }
}

/// Fetch the daemon-side replay ring + last seq for `terminal_id`.
/// Empty replay when the terminal is gone or the snapshot wedged — the
/// consumer just resets to a blank grid, which self-heals on the next
/// live chunk.
async fn resync_replay(config: &ServerConfig, terminal_id: TerminalId) -> (Vec<u8>, u64) {
    let key = config.terminals.lock().await.get(&terminal_id).cloned();
    let Some(key) = key else {
        return (Vec::new(), 0);
    };
    match tokio::time::timeout(RESYNC_SNAPSHOT_TIMEOUT, config.backend.snapshot(&key)).await {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(e)) => {
            tracing::warn!(?terminal_id, "resync snapshot failed: {e}");
            (Vec::new(), 0)
        }
        Err(_) => {
            tracing::warn!(?terminal_id, "resync snapshot timed out");
            (Vec::new(), 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerConfig;
    use lazybox_core::SessionKey;
    use lazybox_ipc::TerminalKind;
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
        // Seed the ring with the full screen the resync should replay.
        let ring = b"FULL-SCREEN-RING".to_vec();
        mock.emit(&key, &ring).await;
        let tid = TerminalId(1);
        config.terminals.lock().await.insert(tid, key.clone());

        // Tiny channel so a short flood forces overflow.
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (client_tx, client_rx) = mpsc::channel(2);
        let task = tokio::spawn(forward_events(
            EventForward { raw_rx, client_tx },
            config.clone(),
        ));

        // 20 chunks into a depth-2 channel → most get dropped.
        for seq in 1..=20 {
            raw_tx
                .send(Event::TerminalOutput {
                    terminal_id: tid,
                    bytes: format!("chunk{seq}").into_bytes(),
                    seq,
                })
                .unwrap();
        }
        // A lifecycle event mid-flood must survive the byte-stream drops.
        raw_tx
            .send(Event::TerminalExited {
                terminal_id: TerminalId(2),
                exit_code: Some(0),
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
        assert_eq!(resyncs[0].1, 1); // mock seq after one emit
        // We dropped output: fewer than 20 TerminalOutput got through.
        let outputs = got
            .iter()
            .filter(|e| matches!(e, Event::TerminalOutput { .. }))
            .count();
        assert!(outputs < 20, "nothing was dropped ({outputs} forwarded)");
    }

    /// With a roomy channel and a consumer that keeps up, nothing is
    /// dropped and no resync is emitted — the drop path is overflow-only.
    #[tokio::test]
    async fn no_overflow_forwards_everything_losslessly() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (client_tx, mut client_rx) = mpsc::channel(64);
        let task = tokio::spawn(forward_events(
            EventForward { raw_rx, client_tx },
            config.clone(),
        ));

        let sk = SessionKey::new("s");
        raw_tx
            .send(Event::TerminalSpawned {
                terminal_id: TerminalId(1),
                session_key: sk.clone(),
                kind: TerminalKind::Shell,
                no_permission: false,
            })
            .unwrap();
        for seq in 1..=10 {
            raw_tx
                .send(Event::TerminalOutput {
                    terminal_id: TerminalId(1),
                    bytes: vec![b'x'],
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
    }
}
