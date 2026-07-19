//! # lazybox-store
//!
//! Persistent storage for lazybox. Abstracts behind a trait so the backend
//! can be swapped (SQLite, file-based, cloud, etc.).

pub mod mock;
mod sqlite;
mod traits;

pub use mock::MemoryStore;
pub use sqlite::SqliteStore;
pub use traits::{ProjectRecord, Store, StoreError, StoreMutation, WorkspaceRecord};
