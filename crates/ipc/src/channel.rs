//! In-process transport: direct tokio mpsc channels between TUI and
//! daemon. No serialization, no sockets. This is the default when both
//! halves live in the same process.

use crate::{
    COMMAND_CHANNEL_CAPACITY, Client, Connection, EVENT_CHANNEL_CAPACITY, event_forward_channel,
};
use tokio::sync::mpsc;

/// Create a connected `Client` / `Connection` pair.
///
/// The daemon holds the `Connection`; the TUI holds the `Client`.
/// Dropping either end signals the other to shut down (channels close).
///
/// Commands (TUI → daemon) ride a bounded channel with loud synchronous
/// rejection on overload. Events
/// (daemon → TUI) flow raw into `Connection::tx` and are bridged to the
/// TUI's **bounded** `Client::rx` by the server-spawned forwarder
/// (carried in `EventForward`), which drops + re-syncs `TerminalOutput`
/// on overflow so inbound memory has a hard ceiling.
pub fn pair() -> (Client, Connection) {
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
    // Bounded stream the TUI actually reads.
    let (client_tx, client_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (raw_tx, forward) = event_forward_channel(client_tx);
    (
        Client::from_bounded_channels(cmd_tx, client_rx),
        Connection::with_forward(raw_tx, cmd_rx, forward),
    )
}
