//! Cumulative event-pipeline counters (issue #91).
//!
//! Event delivery has exactly two lossy points. Both are recoverable,
//! both already log a one-shot `warn!` — but a single line can't show a
//! *trend*: a slow leak of drops reads the same as a one-off burst, so a
//! regression in the coalescing/backpressure machinery (#79, #88) stays
//! invisible until a user notices a stall.
//!
//! - The per-connection forwarder ([`crate::event_forward`]) drops
//!   `TerminalOutput` when a client's bounded channel backs up under an
//!   output flood, then emits one resync from the replay ring.
//! - A connection that lags the broadcast bus past [`crate::BUS_CAPACITY`]
//!   loses events outright and recovers with a full snapshot.
//!
//! These process-wide atomics accumulate the totals so the loss shows up
//! as a climbing counter — in the existing warn lines (now stamped with
//! the running totals) and on the `/v1/metrics` JSON endpoint. Note the
//! command/keystroke path is deliberately absent: it rides an unbounded
//! channel and has no drop site, so "dropped input" is structurally 0.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide counters for the daemon's two lossy event paths. Cloned
/// (cheaply, via the surrounding `Arc`) into every `ServerConfig`.
#[derive(Debug, Default)]
pub struct EventMetrics {
    /// `TerminalOutput` chunks dropped by a connection forwarder because
    /// the client's bounded channel was full. Recovered via resync.
    terminal_output_dropped: AtomicU64,
    /// Resync episodes scheduled — one per terminal per congestion
    /// episode, regardless of how many chunks it dropped.
    terminal_resyncs: AtomicU64,
    /// Bus events a connection missed entirely by lagging the broadcast
    /// channel past capacity (summed across `Lagged(n)` reports).
    bus_lagged_events: AtomicU64,
    /// Recovery snapshots sent to re-sync a connection after a lag.
    bus_lag_recoveries: AtomicU64,
}

impl EventMetrics {
    /// Record one dropped output chunk. Returns the new running total so
    /// the caller can stamp it into the resync warn without a second load.
    pub fn record_output_dropped(&self) -> u64 {
        self.terminal_output_dropped.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record one scheduled resync episode. Returns the new running total.
    pub fn record_resync(&self) -> u64 {
        self.terminal_resyncs.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record a bus lag of `n` missed events.
    pub fn record_bus_lagged(&self, n: u64) {
        self.bus_lagged_events.fetch_add(n, Ordering::Relaxed);
    }

    /// Record one sent lag-recovery snapshot.
    pub fn record_bus_lag_recovery(&self) {
        self.bus_lag_recoveries.fetch_add(1, Ordering::Relaxed);
    }

    /// Point-in-time copy of every counter.
    pub fn snapshot(&self) -> EventMetricsSnapshot {
        EventMetricsSnapshot {
            terminal_output_dropped: self.terminal_output_dropped.load(Ordering::Relaxed),
            terminal_resyncs: self.terminal_resyncs.load(Ordering::Relaxed),
            bus_lagged_events: self.bus_lagged_events.load(Ordering::Relaxed),
            bus_lag_recoveries: self.bus_lag_recoveries.load(Ordering::Relaxed),
        }
    }
}

/// Serializable snapshot of [`EventMetrics`], served at `/v1/metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EventMetricsSnapshot {
    pub terminal_output_dropped: u64,
    pub terminal_resyncs: u64,
    pub bus_lagged_events: u64,
    pub bus_lag_recoveries: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_snapshot() {
        let m = EventMetrics::default();
        assert_eq!(m.record_output_dropped(), 1);
        assert_eq!(m.record_output_dropped(), 2);
        assert_eq!(m.record_resync(), 1);
        m.record_bus_lagged(5);
        m.record_bus_lagged(3);
        m.record_bus_lag_recovery();

        let snap = m.snapshot();
        assert_eq!(snap.terminal_output_dropped, 2);
        assert_eq!(snap.terminal_resyncs, 1);
        assert_eq!(snap.bus_lagged_events, 8);
        assert_eq!(snap.bus_lag_recoveries, 1);
    }
}
