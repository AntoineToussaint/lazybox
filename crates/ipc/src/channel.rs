//! In-process transport: direct tokio mpsc channels between TUI and
//! daemon. No serialization, no sockets. This is the default when both
//! halves live in the same process.

use crate::{Client, Connection, EVENT_CHANNEL_CAPACITY, EventForward};
use tokio::sync::mpsc;

/// Create a connected `Client` / `Connection` pair.
///
/// The daemon holds the `Connection`; the TUI holds the `Client`.
/// Dropping either end signals the other to shut down (channels close).
///
/// Commands (TUI → daemon) ride an unbounded channel — they're
/// low-volume and starving them would block keystrokes. Events
/// (daemon → TUI) flow raw into `Connection::tx` and are bridged to the
/// TUI's **bounded** `Client::rx` by the server-spawned forwarder
/// (carried in `EventForward`), which drops + re-syncs `TerminalOutput`
/// on overflow so inbound memory has a hard ceiling.
pub fn pair() -> (Client, Connection) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    // Raw, unbounded stream the serve loop writes to; the forwarder
    // drains it promptly so it never accumulates.
    let (raw_tx, raw_rx) = mpsc::unbounded_channel();
    // Bounded stream the TUI actually reads.
    let (client_tx, client_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    (
        Client::from_channels(cmd_tx, client_rx),
        Connection::with_forward(raw_tx, cmd_rx, EventForward { raw_rx, client_tx }),
    )
}
