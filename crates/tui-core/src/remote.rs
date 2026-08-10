//! Remote-box worker → UI notices.
//!
//! The `r`-spawn worker (tui-boot's `remote_box`) owns the box lifecycle
//! off the UI thread; the realm `Model` deliberately never drains a remote
//! client's *daemon* events (a second daemon's `Snapshot` would clobber the
//! local inbox). This narrow channel is the worker's only voice back to the
//! UI: bring-up progress and dropped commands surface as footer notices
//! instead of vanishing into the log file.

use lazybox_core::SessionKey;

/// One worker→UI notice. Sent over a plain `tokio::sync::mpsc` channel the
/// `Model` drains with `try_recv` once per run-loop iteration — never a
/// wire event, so it can't collide with daemon state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteBoxNotice {
    /// Bring-up progress ("provisioning…", "connected") — an info flash.
    Info(String),
    /// One or more commands to the box were dropped (a bring-up exhausted
    /// its retries — taking every command queued behind it — or the live
    /// link refused a send). `session_keys` names every workspace whose
    /// optimistic `⇅` tag must be rolled back: the sessions those spawns
    /// advertised will never exist. Aggregated so a bulk fan-out that dies
    /// with the box produces one notice, not one per queued spawn.
    Dropped {
        session_keys: Vec<SessionKey>,
        error: String,
    },
}
