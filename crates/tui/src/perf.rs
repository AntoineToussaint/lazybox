//! Opt-in performance observability, gated behind `LAZYBOX_PERF=1`.
//!
//! When the env var is set, the run loop's watchdog + per-phase
//! counters are routed to a dedicated tracing target ([`TARGET`]) that
//! `init_tracing` pipes into its own sibling `*-perf.log` file, and
//! over-budget iterations raise an on-screen footer indicator so a
//! stall is obvious live without tailing a log. Off by default: the
//! target has no subscriber, the perf file is never created, and the
//! sampler short-circuits before formatting a single field.

use std::sync::OnceLock;

/// Tracing target for perf events. Deliberately *not* under the
/// `lazybox` prefix so the main `lazybox=info` env filter never
/// captures it — perf output lives only in the dedicated perf log,
/// greppable on its own instead of buried in polling noise.
pub const TARGET: &str = "perf";

/// Whether `LAZYBOX_PERF=1` is set. Read once and cached: the flag is
/// fixed for the process lifetime, and the sampler consults it every
/// run-loop iteration.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("LAZYBOX_PERF").as_deref() == Ok("1"))
}
