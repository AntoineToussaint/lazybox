//! # lazybox-events
//!
//! Generic event bus for lazybox. Providers produce `Event`s, the app consumes
//! them. The bus is a bounded broadcast channel — slow consumers drop old events.

mod bus;
mod types;

pub use bus::*;
pub use types::*;
