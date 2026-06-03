//! `UserEvent` — lazybox's daemon events lifted into tuirealm's user-event
//! channel.
//!
//! tuirealm's `Application` is generic over `UserEvent` so apps can
//! subscribe components to whatever stream they have. Lazybox's
//! daemon broadcasts `lazybox_ipc::Event`s via a tokio mpsc; we lift
//! each broadcast into a `UserEvent::Daemon(Box<Event>)` that
//! components subscribe to via `EventClause::User(...)`.
//!
//! ## Why `Box<Event>` and not the raw enum
//!
//! `lazybox_ipc::Event` is large (a few KiB once a Workspace is
//! attached). Boxing keeps the `UserEvent` size small (one pointer)
//! so passing it through tuirealm's internal queues is cheap.
//!
//! ## `PartialEq + Eq` requirement
//!
//! tuirealm requires `UserEvent: Eq + PartialEq + Clone + Send +
//! 'static`. `lazybox_ipc::Event` doesn't implement `PartialEq` (it
//! contains floats, `Box<Workspace>`s, etc.), so we use
//! reference-equality on the Arc — two `UserEvent::Daemon(arc1)`
//! are equal iff the arcs point at the same allocation. Components
//! that need to subscribe to specific event kinds use
//! `EventClause::Discriminant` instead of value-equality matching.

use lazybox_ipc::Event as IpcEvent;
use std::sync::Arc;

/// Lazybox's tuirealm UserEvent. Currently carries one variant; future
/// additions (timer ticks, child-process events, etc.) go here.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// One daemon-side event, lifted from the IPC stream. The Arc
    /// keeps the payload cheap to clone across components.
    Daemon(Arc<IpcEvent>),
}

impl PartialEq for UserEvent {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (UserEvent::Daemon(a), UserEvent::Daemon(b)) => Arc::ptr_eq(a, b),
        }
    }
}

impl Eq for UserEvent {}
