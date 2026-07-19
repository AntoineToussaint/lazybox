//! Pluggable session backend.
//!
//! The lazybox SERVER is a stateless dispatcher — it owns workspace
//! metadata in SQLite, runs polling, and routes command/event
//! traffic to and from connected clients. It does **not** own the
//! agent processes themselves. That job belongs to a session
//! backend implementing [`SessionBackend`].
//!
//! ## Backends
//!
//! - [`RawPtyBackend`] — opens a PTY directly. Sessions die when the
//!   server quits. Default in `--test` and as the universal
//!   fallback when nothing fancier is available.
//! - `TmuxBackend` (next commit) — `tmux -L lazybox` server runs the
//!   agent. Survives lazybox-server restarts; user can attach from
//!   any other terminal via `tmux attach -L lazybox -t <key>`.
//!
//! ## Backend session keys
//!
//! Each backend assigns a stable string key per session (e.g. tmux
//! uses the session name `lazybox-claude-…`). The dispatcher allocates
//! a numeric `TerminalId` for the wire, and keeps a
//! `(TerminalId ↔ backend_key)` map in `ServerConfig.terminals`. On
//! restart the server reads `backend.list()` to re-bind to existing
//! sessions and re-allocates fresh `TerminalId`s for them.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use tokio::sync::mpsc;

pub mod mock;
pub mod raw_pty;
pub mod tmux;
pub use mock::MockBackend;
pub use raw_pty::RawPtyBackend;
pub use tmux::TmuxBackend;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("spawn: {0}")]
    Spawn(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend: {0}")]
    Other(String),
}

/// One chunk of output flowing from a session.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    /// Monotonic per-session sequence number. Lets clients detect
    /// gaps when re-attaching after a lag.
    pub seq: u64,
    pub bytes: Vec<u8>,
}

/// A point-in-time replay of a session's output stream.
///
/// `complete` is the load-bearing part of the contract: only a replay
/// containing every byte from a clean terminal baseline may be used to
/// reset a client's VT parser. A bounded raw-byte ring becomes incomplete
/// once it overwrites its prefix; treating that tail as authoritative can
/// begin inside UTF-8 / CSI state and renders a corrupt grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaySnapshot {
    pub replay: Vec<u8>,
    pub last_seq: u64,
    pub complete: bool,
}

/// Capacity of a `Subscription`'s live channel. Bounded so a stalled
/// consumer puts a hard ceiling on daemon-side buffering: producers
/// `try_send` and drop chunks on overflow (the `seq` gap + resync
/// machinery recovers the consumer from the replay ring) instead of
/// accumulating an unbounded backlog.
pub const SUBSCRIPTION_CHANNEL_CAPACITY: usize = 256;

/// A live subscription to a session's output. Receives chunks until
/// the channel closes (session exited).
pub struct Subscription {
    /// Replay of recent output captured from the backend's scrollback,
    /// so a fresh subscriber can reconstruct the screen without
    /// waiting for new output. Empty if the backend doesn't support
    /// replay (e.g. simple tmux without `capture-pane`).
    pub replay: Vec<u8>,
    /// Whether `replay` starts at a clean terminal baseline and can
    /// therefore rebuild a parser from scratch.
    pub replay_complete: bool,
    /// `seq` value at the moment of subscription; chunks arriving on
    /// `live` start at `seq > last_seq`.
    pub last_seq: u64,
    /// Live chunks. Closes when the session exits or the backend
    /// stops streaming. Bounded (`SUBSCRIPTION_CHANNEL_CAPACITY`);
    /// chunks may be dropped under sustained consumer stall — detect
    /// via `seq` gaps and recover from the replay ring.
    pub live: mpsc::Receiver<OutputChunk>,
}

/// Stateless future-returning trait. We use `Pin<Box<dyn Future>>`
/// over `async-trait` to match the polling::TaskSource style already
/// in the crate and to keep this trait `dyn`-compatible without
/// extra crates.
pub trait SessionBackend: Send + Sync + 'static {
    /// Short stable id for telemetry and config. "raw-pty", "tmux".
    fn id(&self) -> &'static str;

    /// Spawn a fresh session. Returns the backend's stable key for
    /// this session — opaque to callers. The server pairs it with a
    /// `TerminalId` for wire use.
    ///
    /// `hint` is a short human-readable seed the backend should bake
    /// into the visible key (`tmux ls` shows this). Typically
    /// `{workspace_slug}-{kind_label}`. The backend is allowed to
    /// suffix with a counter / PID to guarantee uniqueness, but the
    /// hint should remain readable so users grepping their tmux
    /// session list can see what each one belongs to.
    fn spawn<'a>(
        &'a self,
        argv: &'a [String],
        cwd: Option<&'a Path>,
        env: &'a [(String, String)],
        hint: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, BackendError>> + Send + 'a>>;

    /// Write bytes to the session's stdin.
    fn write<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>>;

    /// Resize the session's terminal grid.
    fn resize<'a>(
        &'a self,
        key: &'a str,
        cols: u16,
        rows: u16,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>>;

    /// Terminate the session. Returns Ok even if the session was
    /// already gone — kill is idempotent.
    fn kill<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>>;

    /// Drop the backend's in-process handle (slot) for a session whose
    /// exit has already been observed, WITHOUT terminating anything.
    ///
    /// The daemon's output-pump teardown calls this once the session's
    /// exit is seen, so backends that keep per-session state (PTY fds,
    /// writer threads, replay rings) release it even when the child
    /// exited on its own — `kill` only runs for user-driven closes, so
    /// without this hook a self-exiting agent leaked its slot forever.
    ///
    /// Deliberately distinct from `kill`: a tmux attach-client EOF can
    /// outlive the tmux session it was attached to, and releasing the
    /// dead conduit must not `kill-session` a survivor that restart
    /// recovery should rediscover. Default: no-op, for backends without
    /// per-session slots (the mock records the call for tests).
    fn release<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let _ = key;
        Box::pin(async {})
    }

    /// Sessions the backend currently knows about. Used at server
    /// startup to rediscover surviving sessions (tmux scenario);
    /// returns empty for ephemeral backends like raw-pty.
    fn list<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, BackendError>> + Send + 'a>>;

    /// Open an output stream + replay for `key`.
    fn subscribe<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Subscription, BackendError>> + Send + 'a>>;

    /// Cheap replay snapshot for `key`. Used by `Subscribe`
    /// to seed reconnecting `--connect` clients without spawning a
    /// pump task per snapshot call (`subscribe` allocates a broadcast
    /// receiver + pump). Default impl errors so backends opt in
    /// explicitly; both bundled backends implement it.
    fn snapshot<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ReplaySnapshot, BackendError>> + Send + 'a>> {
        let key = key.to_string();
        Box::pin(async move {
            Err(BackendError::NotFound(format!(
                "snapshot unimplemented for {key}"
            )))
        })
    }

    /// Wait for the session to exit. Returns the exit code if known.
    /// Implementations should be safe to call repeatedly; subsequent
    /// calls return the cached code.
    fn wait_exit<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<i32>> + Send + 'a>>;
}
