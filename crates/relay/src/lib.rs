//! Rendezvous relay for lazybox's BYOR remote access.
//!
//! A box dials **out** to a codefly-hosted relay and holds the
//! connection open — behind NAT, with no inbound ports, DNS, or certs.
//! The relay brokers a client to that box by box-id and forwards
//! **ciphertext only**; it executes nothing and, once the E2E channel
//! (#891) wraps the stream, never sees plaintext.
//!
//! - [`server::Relay`] — the codefly-hosted broker (the `lazybox-relay`
//!   binary).
//! - [`client::serve_box`] — the box side (`lazybox serve` dials out with
//!   this).
//! - [`client::connect_through_relay`] — the client side.

pub mod client;
pub mod protocol;
pub mod server;

pub use client::{OnClient, connect_through_relay, serve_box};
pub use protocol::{Ack, Hello, ToBox};
pub use server::Relay;
