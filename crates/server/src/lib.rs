//! lazybox-server — owns state and IO on behalf of TUI clients.
//!
//! Lives as a library so the in-process transport can call `Server::serve`
//! without a subprocess. When out-of-process (remote access, long-running
//! service), the `lazybox` binary's `daemon` subcommand invokes the same
//! `Server::serve` entrypoint over a Unix socket.
//!
//! Today the daemon exposes the PTY lifecycle (spawn/write/resize/close,
//! per-terminal ring buffer, reconnect replay) and the serve loop that
//! accepts `ipc::Command`s and emits `ipc::Event`s. Provider polling,
//! worktree management, agent hook plumbing, and LLM proxy integration
//! land on top of this core in the order described in `../DESIGN.md`.

// Cosmetic / pedantic lints that landed with clippy 1.95. The
// daemon is a high-traffic codebase mid-refactor; we don't want
// every CI bump to gate on style-only suggestions. The ones
// suppressed here either don't improve clarity (the `?` rewrite
// of `let-else`, blanket type-alias for one Tokio mpsc tuple) or
// would require an enum-variant rebox that touches every IPC
// handler. Re-enable individually as we touch the relevant code.
#![allow(
    clippy::large_enum_variant,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::question_mark,
    clippy::unwrap_or_default
)]

pub mod agent_runs;
pub mod agent_spawn;
pub mod agent_stream;
pub mod api_gateway;
pub mod auth;
pub mod backend;
pub mod chat;
pub mod event_forward;
pub mod lifecycle;
pub mod metrics;
pub mod polling;
pub mod pty;
pub mod slack;
pub mod socket_service;
pub mod spawn_handler;

use crate::backend::{RawPtyBackend, SessionBackend, TmuxBackend};
use lazybox_agents::Registry;
use lazybox_ipc::{AgentRunId, Connection, Event, TerminalId};
use lazybox_store::{MemoryStore, SqliteStore, Store};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{Mutex, broadcast};

/// Where lazybox keeps its persistent state. Thin alias over
/// [`lazybox_core::paths::state_db`] so callers don't have to import
/// the paths module just for this one helper. Override via the
/// `LAZYBOX_HOME` env var (see `lazybox_core::paths` for details).
pub fn state_db_path() -> PathBuf {
    lazybox_core::paths::state_db()
}

/// Open the persistent store at the canonical path. Returns `None` on
/// open failure (corrupt DB, permissions); callers fall back to skipping
/// persistence rather than aborting startup.
pub fn open_store() -> Option<Arc<dyn Store>> {
    let path = state_db_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match SqliteStore::open(&path) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::warn!("store open failed at {}: {e}", path.display());
            None
        }
    }
}

// REMOVED: wipe_legacy_worktrees. We never delete from `~/.lazybox/`.
// lazybox is constrained to `~/.lazybox/v2/` for everything it writes
// — `state.db`, the bare-clone cache, every worktree. If a user
// has real work in `~/.lazybox/worktrees/` from a prior tool, that's
// their data and lazybox leaves it alone.

/// Server-side error type. Used by `Server::serve` and the internal
/// helpers it composes. Public API exposes `Display` only — the
/// in-process TUI consumer just prints the message — but the typed
/// variants give us a `#[derive(Error)]` enum per CLAUDE.md's
/// library-crate convention (and let future consumers like the JSON
/// API gateway dispatch on kind).
///
/// Migrated from blanket `anyhow::Result` so the type signature of
/// every server helper describes its actual failure modes.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// IO failure — file descriptor, socket, subprocess pipe, etc.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encode/decode failure on a wire-bound payload.
    #[error("serde_json: {0}")]
    SerdeJson(#[from] serde_json::Error),
    /// Persistent-store read/write failure. The inner error is the
    /// store-specific message; we lose the typed variant on the way
    /// in since `lazybox_store` errors are `Box<dyn Error>`-shaped, but
    /// the human message survives.
    #[error("store: {0}")]
    Store(String),
    /// Workspace not found, malformed, or missing a required field
    /// (primary task, repo, branch). Surfaces from the spawn/upsert
    /// paths when the requested workspace can't be resolved into a
    /// concrete worktree target.
    #[error("workspace: {0}")]
    Workspace(String),
    /// Worktree / git operation failed. Wraps `lazybox_git_ops` errors
    /// + checkout / mount failures.
    #[error("worktree: {0}")]
    Worktree(String),
    /// Structured-agent stream subprocess failure (Claude stdin/stdout
    /// pipe, argv parse, spawn). Surfaces from `agent_stream` and
    /// `agent_runs`.
    #[error("agent: {0}")]
    Agent(String),
    /// Backend (PTY / tmux) failure — pass-through of whatever the
    /// `SessionBackend` returned, stringified.
    #[error("backend: {0}")]
    Backend(String),
    /// Catch-all for invariant violations and other internal
    /// inconsistencies. Use sparingly — a typed variant is almost
    /// always better than `Internal`.
    #[error("internal: {0}")]
    Internal(String),
}

/// Add a string context to a `Result<T, E: Display>`, similar to
/// `anyhow::Context::context` but producing a typed `ServerError`
/// variant. Picks the variant based on the source error kind via the
/// `Into<ServerError>` impls below, so callers don't have to spell
/// out the wrapping at every `?`.
pub trait ResultExt<T> {
    /// Wrap the error with a context prefix, producing
    /// `ServerError::<auto-selected>("ctx: <inner>")`.
    fn ctx(self, msg: &str) -> Result<T, ServerError>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for Result<T, E> {
    fn ctx(self, msg: &str) -> Result<T, ServerError> {
        self.map_err(|e| ServerError::Internal(format!("{msg}: {e}")))
    }
}

/// Same shape for `Option<T>`. `None.ctx("X")` → `ServerError::Internal("X")`.
impl<T> ResultExt<T> for Option<T> {
    fn ctx(self, msg: &str) -> Result<T, ServerError> {
        self.ok_or_else(|| ServerError::Internal(msg.to_string()))
    }
}

/// Capacity of the daemon's process-wide event broadcast bus. Events
/// produced by the poller and the PTY/proxy subsystems land here and
/// fan out to every connected client. If a slow client lags more than
/// `BUS_CAPACITY` events behind, it skips ahead — better than blocking
/// every other client on the slowest one.
pub const BUS_CAPACITY: usize = 1024;

/// Canonical lock-acquisition order for the per-terminal maps when
/// **co-holding** two or more. AB/BA deadlock requires two paths to
/// each hold one lock while waiting for the other; the order below
/// rules that out by convention.
///
/// **Order:** `terminals → terminal_meta → terminal_sessions → agent_states`.
///
/// This is a convention, not a runtime check — Tokio mutexes don't
/// enforce hierarchies. Sequential acquire-and-drop sites (each
/// `.lock().await.method(...)` releases at end-of-statement) DO NOT
/// have to follow this order: the per-statement guard never overlaps
/// the next statement's, so AB/BA can't form. Those sites are free
/// to use whatever order best suits their *reader*-consistency needs
/// (e.g. `handle_spawn` inserts meta before terminals so a snapshot
/// reader never sees a terminals entry without a matching meta).
///
/// The only co-holding site today is
/// `spawn_handler::freeze_runners_in_session`; that's why this
/// constant exists, as a discoverable name future callers can grep.
pub const TERMINAL_MAP_LOCK_ORDER: &str =
    "terminals → terminal_meta → terminal_sessions → agent_states";

/// `ServerConfig` is the per-process state shared across all client
/// connections — the persistent store, the broadcast bus the poller
/// pushes events into, and the agent registry the spawn handler reads.
/// Cheaply cloneable: `store` is `Arc`, `bus` is a tokio broadcast
/// `Sender` (clone is a refcount), `agents` is a small struct.
///
/// Per-process invariant: there is exactly **one** `ServerConfig` for
/// the whole process. Both `run_embedded` and `lazybox daemon start`
/// build it once at startup so the polling loop's `SessionUpserted`
/// events reach every connected TUI.
#[derive(Clone)]
pub struct ServerConfig {
    pub agents: Registry,
    /// Persistent state at `~/.lazybox/v2/state.db`.
    pub store: Arc<dyn Store>,
    /// Process-wide event bus. Producers (poller, PTY, proxy) call
    /// `bus.send(event)`; each `Server::serve` connection subscribes
    /// and forwards events into its own `Server.tx`.
    pub bus: broadcast::Sender<Event>,
    /// Pluggable session manager. Owns the actual agent processes —
    /// the server delegates spawn/write/resize/kill/subscribe.
    /// Default is `RawPtyBackend`; `TmuxBackend` adds persistence.
    pub backend: Arc<dyn SessionBackend>,
    /// Wire-side `TerminalId` ↔ backend session key. The server
    /// allocates numeric ids for the IPC stream; the backend uses its
    /// own stable string keys (e.g. tmux session names). This map
    /// translates between them. Every connection's serve loop reads
    /// + writes it.
    pub terminals: Arc<Mutex<HashMap<TerminalId, String>>>,
    /// Wire-side `TerminalId` → owning `SessionId`. Lets the
    /// migration code freeze just one session's runners during a
    /// `git worktree move`, instead of freezing every backend
    /// session in the process. Populated by `handle_spawn` when a
    /// terminal is created against a known session; entries are
    /// removed on `TerminalExited`.
    pub terminal_sessions: Arc<Mutex<HashMap<TerminalId, lazybox_core::SessionId>>>,
    /// Cached `AgentState` per agent terminal. Populated by the
    /// output pump's state detector; transitions are broadcast as
    /// `Event::AgentState`. Caching avoids broadcasting on every
    /// PTY chunk when nothing changed.
    pub agent_states: Arc<Mutex<HashMap<TerminalId, lazybox_ipc::AgentState>>>,
    /// Wire-side metadata per terminal: `(session_key, kind)`. The
    /// `terminals` map only carries the backend key; clients
    /// reconnecting via Subscribe need the full pairing so the
    /// initial Snapshot can route terminals into the right tab
    /// strip. Populated by `handle_spawn`, cleaned on
    /// `TerminalExited`.
    pub terminal_meta:
        Arc<Mutex<HashMap<TerminalId, (lazybox_core::SessionKey, lazybox_ipc::TerminalKind)>>>,
    /// Terminals launched in no-permission / bypass mode (autonomous
    /// sessions). Populated by `handle_spawn` when a spawn skips
    /// permission prompts; read by `snapshot_terminals` so a
    /// reconnecting client can re-render the indicator. Cleaned on
    /// `TerminalExited` alongside the other per-terminal maps.
    pub no_permission_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Terminals whose agent-state detection buffer should be dropped
    /// on the pump's next output chunk. Set by `handle_write` when the
    /// user submits an answer to an `InputNeeded` prompt (Enter while
    /// the `?` pill is up); consumed (and cleared) by the output pump.
    ///
    /// Without this, the just-answered prompt's markers (`❯`, the
    /// numbered options, `Esc to cancel`, `do you want to …`) linger in
    /// the rolling detection window and re-fire `InputNeeded` on the
    /// very next chunk — so the `?` pill reappears the instant after
    /// the user answers and never clears until ~16 KiB of fresh output
    /// evicts the stale prompt. Dropping the buffer here lets detection
    /// restart from post-answer output. Safe: if the prompt is
    /// genuinely still up, Claude re-renders it and the fresh chunk
    /// re-establishes `InputNeeded`.
    pub agent_detect_resets: Arc<Mutex<HashSet<TerminalId>>>,
    /// Agent terminals that have reported at least one structured
    /// lifecycle hook (`Command::IngestHook`), mapped to the arrival
    /// time of their most recent hook. For these, hooks are the
    /// authoritative source of `Working` / `InputNeeded`; the PTY
    /// detector only supplies the idle/interrupt fallback the `Stop`
    /// hook misses (Ctrl-C / Esc don't fire `Stop`) — unless the last
    /// hook is stale (see `spawn_handler::HOOK_STALENESS`), in which
    /// case the terminal degrades back to full PTY detection instead
    /// of freezing on the last hook state. A terminal that never
    /// reports a hook (old Claude version, hooks disabled, non-Claude
    /// agent) is absent here and keeps full PTY detection. Populated
    /// by `handle_ingest_hook`, cleaned on `TerminalExited`.
    pub hook_driven_terminals: Arc<Mutex<HashMap<TerminalId, std::time::Instant>>>,
    /// Per-terminal signal fired by `handle_ingest_hook` when a
    /// `UserPromptSubmit` hook lands. The prompt-inject paths register
    /// a `Notify` here before sending the submit keystroke so they can
    /// verify the prompt actually entered Claude's turn (and resend
    /// the Enter once if it didn't — issue #122). Entries are removed
    /// by the registering inject task; the pump also sweeps on
    /// `TerminalExited`.
    pub prompt_submit_signals: Arc<Mutex<HashMap<TerminalId, Arc<tokio::sync::Notify>>>>,
    /// Factory for a structured agent run's underlying process I/O.
    /// Defaults to spawning a real subprocess; tests swap in an
    /// in-memory fake so they never launch `claude` or a shell.
    pub agent_stream_spawner: Arc<dyn agent_stream::AgentStreamSpawner>,
    /// Structured stream-json agent runs. Keyed by wire-side run id.
    pub agent_runs: Arc<Mutex<HashMap<AgentRunId, agent_runs::AgentRunHandle>>>,
    /// Process-wide structured run id allocator.
    pub next_agent_run_id: Arc<AtomicU64>,
    /// Per-principal provider credential store. The default in-memory
    /// implementation is intentionally non-persistent until the
    /// encrypted production store is chosen.
    pub credential_store: Arc<dyn auth::CredentialStore>,
    /// Local/dev fallback principal. API auth can replace this with a
    /// per-connection principal later.
    pub default_principal_id: lazybox_ipc::PrincipalId,
    /// Cross-tick polling state — provider-error debounce + the
    /// "already prompted" set for out-of-scope workspaces. Shared
    /// between the long-lived poll loop and `Command::Refresh`'s
    /// one-shot tick so dismissed prompts stay dismissed across
    /// both paths. Without this, Refresh would prompt, you'd
    /// dismiss, and 30s later the long-lived loop would re-prompt
    /// (each has its own `TickState`).
    pub poll_state: Arc<Mutex<polling::TickState>>,
    /// Long-lived GitHub client, cached across ticks in its OWN lock —
    /// deliberately NOT a field of `poll_state` (issue #92). WITHOUT a
    /// persistent client, every tick (and every user-triggered
    /// `fetch_pr_details`) would rebuild it via
    /// `GhClient::from_credential`, resetting the inner `RateBudget` to
    /// its full-bucket / no-remote-observation default — the "GitHub said
    /// remaining=50, back off" knowledge from the last call is thrown
    /// away and the next request flies blind into a 429. We reuse the
    /// client (and its budget `Arc`) and only rebuild when the credential
    /// SOURCE changes.
    ///
    /// Why its own `std::sync::Mutex` rather than living in `poll_state`:
    /// `handle_fetch_pr_details` and the poll tick both need this client,
    /// and the cold-cache path builds it with a network `.await`. Holding
    /// `poll_state` across that build re-creates the exact serve-loop
    /// stall #91/#92 fight — and #133's `checkout_poll_state` made it
    /// worse by emptying `poll_state`'s copy for the whole tick, so every
    /// concurrent fetch hit the rebuild path. Keeping the client here
    /// means reaching it is a brief `std::sync::Mutex` clone-out that
    /// never blocks on `poll_state` and never spans the `from_credential`
    /// await.
    pub gh_client_cache: Arc<std::sync::Mutex<Option<lazybox_gh::GhClient>>>,
    /// Issue→PR merge-prompt dedupe memory. Deliberately separate from
    /// `poll_state`: the collapse path that touches it runs inside an
    /// `upsert`, and sharing `poll_state`'s non-reentrant
    /// `tokio::sync::Mutex` self-deadlocked when the tick held that
    /// guard across the upsert loop (#131/#132). A tick no longer holds
    /// `poll_state` across its body (#133), but `upsert` staying
    /// decoupled from it is worth keeping. See
    /// [`polling::MergePromptMemory`].
    pub merge_prompts: Arc<Mutex<polling::MergePromptMemory>>,
    /// Authenticated user logins per provider source ("github" →
    /// "AntoineToussaint"). Populated by the polling layer when
    /// each provider client initializes; consumed by the Subscribe
    /// handler so reconnecting TUIs immediately know which authors
    /// in activity bylines are the local user (→ render as `@me`).
    /// `std::sync::Mutex` (not tokio's) because the data is tiny
    /// and read/written from sync contexts only — no await needed.
    pub viewer_identities: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// Wake handle for the long-lived poll loop. Pinging this
    /// `Notify` makes the next poll tick fire immediately instead of
    /// waiting out the remainder of its current sleep. Used by:
    /// - `Command::Refresh` (manual `Shift-R`)
    /// - client connect (so a freshly-opened TUI sees current data
    ///   instead of "whatever the daemon polled last")
    /// - the lazy mergeable retry path (`Mergeable::Unknown` PRs
    ///   re-fired ~5s later so GitHub's lazy compute lands)
    pub poll_wake: Arc<tokio::sync::Notify>,
    /// In-flight singleton-spawn claims: `(workspace key, singleton
    /// kind key)` pairs a `handle_spawn` is currently provisioning.
    /// The duplicate-spawn check reads maps that are only populated
    /// AFTER worktree provisioning + `backend.spawn` — a minutes-long
    /// window on a cold clone — so two `w` presses (or autofix racing
    /// a user spawn, or startup restore racing both) each passed it
    /// and launched two skip-permissions agents into one worktree.
    /// Claimed synchronously at the top of `handle_spawn`, released by
    /// a drop guard on EVERY exit path; `Kill` also serializes against
    /// it. `std::sync::Mutex` — the data is tiny and no await ever
    /// happens under the guard, which lets the drop guard release
    /// synchronously.
    pub inflight_spawns: Arc<std::sync::Mutex<HashSet<(String, String)>>>,
    /// Pinged whenever an in-flight spawn claim is released, so
    /// waiters (duplicate spawns collapsing onto the winner, `Kill`
    /// waiting out a mid-flight provision) re-check promptly instead
    /// of busy-polling.
    pub inflight_spawn_changed: Arc<tokio::sync::Notify>,
    /// Workspace keys deleted in this process (`Kill` /
    /// `RemoveMergedWorkspace`). Consulted by the spawn path when a
    /// workspace row is missing: deleted-mid-spawn ABORTS the spawn,
    /// while a key that never existed keeps the test/--test-mode
    /// fallback of rooting the spawn in the daemon's cwd.
    pub deleted_workspaces: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Shape of the last `InputNeeded` decision per terminal — whether
    /// a bare chooser keystroke (`1`-`9`, y/n, Esc) is a complete
    /// answer. Written by the PTY detector (its structural triggers
    /// are all chooser-shaped) and by `handle_ingest_hook` (permission
    /// → chooser, elicitation → free text); read by `handle_write`'s
    /// optimistic InputNeeded→Working flip so a digit typed into a
    /// free-text elicitation can't clear a real `?`. Cleaned on
    /// `TerminalExited`.
    pub input_needed_shapes: Arc<Mutex<HashMap<TerminalId, lazybox_agents::PromptShape>>>,
    /// Cumulative counters for the event pipeline's two lossy paths
    /// (forwarder output drops + bus lag). Surfaced at `/v1/metrics` and
    /// stamped into the drop/lag warn lines (issue #91).
    pub event_metrics: Arc<metrics::EventMetrics>,
}

impl ServerConfig {
    /// Open the store at `~/.lazybox/v2/state.db`.
    ///
    /// Open failures (permissions, disk corruption) fall back to an
    /// in-memory store so the daemon still starts — better empty than
    /// dead.
    pub fn from_user_config() -> Self {
        let path = state_db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let store = match SqliteStore::open(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "falling back to in-memory store: couldn't open {}: {e}",
                    path.display()
                );
                return Self::with_store(Arc::new(MemoryStore::new()));
            }
        };

        // Pick the strongest available backend. tmux means sessions
        // survive lazybox-server restart and can be attached externally
        // via `tmux -L lazybox attach -t <key>`; raw-pty is the
        // ephemeral fallback when tmux isn't installed.
        let backend: Arc<dyn SessionBackend> = match TmuxBackend::detect() {
            Some(t) => {
                tracing::info!("session backend: tmux");
                Arc::new(t)
            }
            None => {
                tracing::info!("session backend: raw-pty (tmux unavailable)");
                Arc::new(RawPtyBackend::new())
            }
        };
        Self::with_store_and_backend(Arc::new(store), backend)
    }

    /// Build a config with an explicit store and the deterministic
    /// raw-pty backend. Used by tests and the `--test` mode that
    /// don't want tmux side-effects (a real lazybox tmux server, leftover
    /// sessions on disk). Production paths go through
    /// `from_user_config` which auto-detects tmux.
    pub fn with_store(store: Arc<dyn Store>) -> Self {
        Self::with_store_and_backend(store, Arc::new(RawPtyBackend::new()))
    }

    /// Build with explicit store + backend. Used by tests that want
    /// a stub backend, and by the binary wiring once backend
    /// detection (tmux vs raw-pty) lands.
    pub fn with_store_and_backend(store: Arc<dyn Store>, backend: Arc<dyn SessionBackend>) -> Self {
        let (bus, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            agents: Registry::default_builtins(),
            store,
            bus,
            backend,
            terminals: Arc::new(Mutex::new(HashMap::new())),
            terminal_sessions: Arc::new(Mutex::new(HashMap::new())),
            agent_states: Arc::new(Mutex::new(HashMap::new())),
            terminal_meta: Arc::new(Mutex::new(HashMap::new())),
            no_permission_terminals: Arc::new(Mutex::new(HashSet::new())),
            agent_detect_resets: Arc::new(Mutex::new(HashSet::new())),
            hook_driven_terminals: Arc::new(Mutex::new(HashMap::new())),
            prompt_submit_signals: Arc::new(Mutex::new(HashMap::new())),
            agent_stream_spawner: Arc::new(agent_stream::ProcessAgentStreamSpawner),
            agent_runs: Arc::new(Mutex::new(HashMap::new())),
            next_agent_run_id: Arc::new(AtomicU64::new(1)),
            credential_store: Arc::new(auth::MemoryCredentialStore::new()),
            default_principal_id: lazybox_ipc::PrincipalId::local(),
            poll_state: Arc::new(Mutex::new(polling::TickState::default())),
            gh_client_cache: Arc::new(std::sync::Mutex::new(None)),
            merge_prompts: Arc::new(Mutex::new(polling::MergePromptMemory::default())),
            viewer_identities: Arc::new(std::sync::Mutex::new(Vec::new())),
            poll_wake: Arc::new(tokio::sync::Notify::new()),
            inflight_spawns: Arc::new(std::sync::Mutex::new(HashSet::new())),
            inflight_spawn_changed: Arc::new(tokio::sync::Notify::new()),
            deleted_workspaces: Arc::new(std::sync::Mutex::new(HashSet::new())),
            input_needed_shapes: Arc::new(Mutex::new(HashMap::new())),
            event_metrics: Arc::new(metrics::EventMetrics::default()),
        }
    }

    /// Convenience: in-memory store + `MockBackend`. Never touches
    /// the filesystem, never spawns a real subprocess. The default
    /// for unit tests — see `in_memory_with_mock` when the test
    /// needs to drive the backend (inject output, finish a session).
    pub fn in_memory() -> Self {
        Self::with_store_and_backend(
            Arc::new(MemoryStore::new()),
            Arc::new(backend::MockBackend::new()),
        )
    }

    /// Like `in_memory`, but also returns the typed `MockBackend`
    /// handle so the test can call `emit`, `finish`, `writes_for`,
    /// etc. against the same backend the daemon is using.
    pub fn in_memory_with_mock() -> (Self, backend::MockBackend) {
        let mock = backend::MockBackend::new();
        let config =
            Self::with_store_and_backend(Arc::new(MemoryStore::new()), Arc::new(mock.clone()));
        (config, mock)
    }

    // ── Lock-then-clone helpers ──────────────────────────────────
    //
    // Pattern these helpers replace:
    //
    //   let key = match config.terminals.lock().await.get(&id).cloned() {
    //       Some(k) => k,
    //       None => return,
    //   };
    //
    // The MutexGuard is a temporary in the match scrutinee, so per
    // Rust temporary-scope rules it lives for the WHOLE match. Any
    // `.await` in a match arm holds the lock across the suspension
    // point — exactly the latent deadlock I caught in
    // `handle_inject_prompt` mid-push. Each helper here takes the
    // lock, clones the lookup result, and releases on return — no
    // way for a caller to accidentally hold a guard across an await.

    /// Snapshot the backend key for a wire-side terminal id.
    /// Returns `None` when the terminal isn't registered.
    pub async fn backend_key_for(&self, id: TerminalId) -> Option<String> {
        self.terminals.lock().await.get(&id).cloned()
    }

    /// Snapshot the `(session_key, kind)` metadata for a terminal.
    /// `None` when the terminal isn't registered (or the entry got
    /// cleaned by a concurrent `TerminalExited` between calls).
    pub async fn terminal_meta_for(
        &self,
        id: TerminalId,
    ) -> Option<(lazybox_core::SessionKey, lazybox_ipc::TerminalKind)> {
        self.terminal_meta.lock().await.get(&id).cloned()
    }

    /// Snapshot the cached `AgentState` for a terminal. `None` when
    /// the pump hasn't observed a state transition yet.
    pub async fn agent_state_for(&self, id: TerminalId) -> Option<lazybox_ipc::AgentState> {
        self.agent_states.lock().await.get(&id).copied()
    }
}

pub struct Server {
    config: ServerConfig,
}

impl Server {
    pub fn new(config: ServerConfig) -> Self {
        Self { config }
    }

    /// Accept a client connection (either an in-process `Server` from
    /// `ipc::channel::pair` or a remote `Server` from `ipc::socket::serve`).
    ///
    /// The loop selects on:
    /// - inbound commands from the client (Subscribe, Shutdown, …),
    /// - the process-wide broadcast bus (SessionUpserted, etc).
    ///
    /// Bus events are forwarded straight to the client. Commands are
    /// dispatched here; handlers that don't have a backing subsystem
    /// yet are trace-logged and dropped so adding a command at the IPC
    /// layer never breaks an existing client.
    pub async fn serve(&self, mut conn: Connection) -> Result<(), ServerError> {
        // Bridge the raw event stream to the client's bounded channel
        // through the drop-and-resync forwarder, when the transport
        // wired one up (in-process + socket clients do; the JSON API
        // gateway reads the raw stream directly and leaves this `None`).
        // The serve loop below keeps writing raw events to `conn.tx`
        // exactly as before — it never blocks on the client, so the
        // command path (keystroke `Write`s) stays responsive no matter
        // how far behind the consumer falls.
        if let Some(forward) = conn.take_forward() {
            tokio::spawn(event_forward::forward_events(forward, self.config.clone()));
        }
        let mut bus_rx = self.config.bus.subscribe();
        // Detached mutation handlers (Spawn, Kill, InjectPrompt, the
        // GraphQL writers, worktree teardowns, …) land here instead of
        // a bare `tokio::spawn` so shutdown can DRAIN them: `Shutdown =>
        // break` used to abandon an in-flight Kill or Spawn mid-write.
        // Completed entries are reaped at the top of each loop turn so
        // the set doesn't accumulate results unboundedly.
        let mut mutations = tokio::task::JoinSet::new();
        loop {
            while mutations.try_join_next().is_some() {}
            tokio::select! {
                cmd = conn.rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    // Per-command name at INFO so a stalled IPC channel is
                    // visible at a glance — historically we'd see `daemon
                    // ← Subscribe` and then nothing, with no way to tell
                    // whether subsequent commands were arriving at all.
                    // `Write` floods on every keystroke; trim its payload
                    // but still emit one line per command so the cadence
                    // is observable.
                    let label = match &cmd {
                        lazybox_ipc::Command::Spawn { .. } => "Spawn",
                        lazybox_ipc::Command::Close { .. } => "Close",
                        lazybox_ipc::Command::IngestHook { .. } => "IngestHook",
                        lazybox_ipc::Command::CreateSession { .. } => "CreateSession",
                        lazybox_ipc::Command::Subscribe => "Subscribe",
                        lazybox_ipc::Command::Refresh => "Refresh",
                        lazybox_ipc::Command::Write { .. } => "Write",
                        lazybox_ipc::Command::RecordUserMessage { .. } => "RecordUserMessage",
                        lazybox_ipc::Command::Resize { .. } => "Resize",
                        lazybox_ipc::Command::InjectPrompt { .. } => "InjectPrompt",
                        lazybox_ipc::Command::MarkRead { .. } => "MarkRead",
                        lazybox_ipc::Command::FocusWorkspace { .. } => "FocusWorkspace",
                        lazybox_ipc::Command::MarkActivityRead { .. } => "MarkActivityRead",
                        lazybox_ipc::Command::UnmarkActivityRead { .. } => "UnmarkActivityRead",
                        lazybox_ipc::Command::FetchPrDetails { .. } => "FetchPrDetails",
                        lazybox_ipc::Command::PostReply { .. } => "PostReply",
                        lazybox_ipc::Command::MergePr { .. } => "MergePr",
                        lazybox_ipc::Command::ConfirmMerge { .. } => "ConfirmMerge",
                        lazybox_ipc::Command::Snooze { .. } => "Snooze",
                        lazybox_ipc::Command::Unsnooze { .. } => "Unsnooze",
                        lazybox_ipc::Command::Kill { .. } => "Kill",
                        lazybox_ipc::Command::RemoveMergedWorkspace { .. } => "RemoveMergedWorkspace",
                        lazybox_ipc::Command::DeleteProject { .. } => "DeleteProject",
                        lazybox_ipc::Command::CollapseIntoPr { .. } => "CollapseIntoPr",
                        lazybox_ipc::Command::CreateWorkspace { .. } => "CreateWorkspace",
                        lazybox_ipc::Command::CreateProject { .. } => "CreateProject",
                        lazybox_ipc::Command::AdoptSessions { .. } => "AdoptSessions",
                        lazybox_ipc::Command::RequestReviewers { .. } => "RequestReviewers",
                        lazybox_ipc::Command::AddAssignees { .. } => "AddAssignees",
                        lazybox_ipc::Command::SetAssignees { .. } => "SetAssignees",
                        lazybox_ipc::Command::SetLabels { .. } => "SetLabels",
                        lazybox_ipc::Command::FetchRepoLabels { .. } => "FetchRepoLabels",
                        lazybox_ipc::Command::SetSessionLayout { .. } => "SetSessionLayout",
                        lazybox_ipc::Command::StartAgentRun { .. } => "StartAgentRun",
                        lazybox_ipc::Command::SendAgentInput { .. } => "SendAgentInput",
                        lazybox_ipc::Command::InterruptAgentRun { .. } => "InterruptAgentRun",
                        lazybox_ipc::Command::DecideAgentApproval { .. } => "DecideAgentApproval",
                        lazybox_ipc::Command::AnswerAgentQuestion { .. } => "AnswerAgentQuestion",
                        lazybox_ipc::Command::UpsertProviderCredential { .. } => "UpsertProviderCredential",
                        lazybox_ipc::Command::RemoveProviderCredential { .. } => "RemoveProviderCredential",
                        lazybox_ipc::Command::ListProviderCredentials { .. } => "ListProviderCredentials",
                        lazybox_ipc::Command::CleanWorktrees => "CleanWorktrees",
                        lazybox_ipc::Command::InspectWorktrees => "InspectWorktrees",
                        lazybox_ipc::Command::DeleteOrphanedWorktree { .. } => "DeleteOrphanedWorktree",
                        lazybox_ipc::Command::Shutdown => "Shutdown",
                    };
                    // `Write` fires on every keystroke — at info it floods
                    // the log and makes real lifecycle lines unreadable.
                    if matches!(cmd, lazybox_ipc::Command::Write { .. }) {
                        tracing::debug!("daemon ← {label}");
                    } else {
                        tracing::info!("daemon ← {label}");
                    }
                    // Time how long the serve loop spends INLINE on this
                    // command. `tokio::select!` is single-task: while a
                    // handler `.await`s (or worse, makes a synchronous
                    // blocking call like a parking_lot store-mutex
                    // acquire), NO other command — including the `Write`
                    // keystrokes that drive the terminal — can be
                    // serviced. The known-slow handlers detach via
                    // `tokio::spawn` and return here in microseconds; a
                    // handler that shows up SLOW below is one that's
                    // wedging the loop (the "can't type while GitHub
                    // syncs" class of bug — see issue #34). This is the
                    // breadcrumb that tells us WHICH command, not just
                    // "the app froze".
                    let cmd_started = std::time::Instant::now();
                    match cmd {
                        lazybox_ipc::Command::Subscribe => {
                            // Offload the SQLite scans (issue #34: pre-fix
                            // `list_workspaces` + `list_projects` ran on
                            // the daemon's IPC event-loop task, holding
                            // up bus-event forwarding for the duration —
                            // up to several hundred ms on a populated
                            // store, perceived in the UI as "frozen
                            // during sync"). `spawn_blocking` moves the
                            // blocking parking_lot mutex acquisition +
                            // row iteration off the runtime worker so
                            // `select!` stays responsive to bus events
                            // while the snapshot loads.
                            //
                            // Both scans share one task so the daemon
                            // only pays the spawn/handoff cost once and
                            // doesn't sequentially await two dispatches.
                            // A panic inside the task (poisoned mutex,
                            // corrupt JSON) is logged loudly — sending
                            // an empty Snapshot silently would render a
                            // blank sidebar with no breadcrumb in the
                            // log.
                            let store = self.config.store.clone();
                            let (workspaces, projects) = match tokio::task::spawn_blocking(
                                move || (load_workspaces(&*store), load_projects(&*store)),
                            )
                            .await
                            {
                                Ok(pair) => pair,
                                Err(e) => {
                                    tracing::error!(
                                        "Subscribe snapshot load task failed: {e} — sending empty snapshot",
                                    );
                                    (Vec::new(), Vec::new())
                                }
                            };
                            let terminals = spawn_handler::snapshot_terminals(&self.config).await;
                            let _ = conn.tx.send(Event::Snapshot {
                                workspaces,
                                terminals,
                                projects,
                            });
                            // Kick a fresh poll. The freshly-opened
                            // TUI sees the store-cached snapshot
                            // immediately above; the wake makes the
                            // poll loop refresh it within a few
                            // seconds instead of waiting out the
                            // remainder of its current sleep.
                            self.config.poll_wake.notify_one();
                            // Replay cached viewer identities so a
                            // reconnecting TUI can render `@me` for
                            // the local user's bylines without
                            // waiting for the next poll cycle.
                            let logins = self
                                .config
                                .viewer_identities
                                .lock()
                                .expect("viewer_identities poisoned")
                                .clone();
                            if !logins.is_empty() {
                                let _ =
                                    conn.tx.send(Event::ViewerIdentities { logins });
                            }
                        }
                        lazybox_ipc::Command::Spawn {
                            session_key,
                            session_id,
                            kind,
                            cwd,
                            initial_prompt,
                        } => {
                            // A spawn carrying a pre-built work prompt is
                            // an autonomous "work on this" launch (`w` /
                            // address-comments) — the same end-state as an
                            // `@lazybox` mention. Run it unattended (skip
                            // permissions, subject to the
                            // `autonomous_skip_permissions` toggle) so it
                            // doesn't stall on the folder-trust / tool-
                            // approval gate, which also blocks the post-
                            // spawn prompt inject and eats the submit
                            // keystroke. Bare interactive spawns (`c` / `x`
                            // / `u` / `s`) carry no prompt and keep the
                            // human-in-the-loop approval.
                            let autonomous = spawn_handler::spawn_is_autonomous(&initial_prompt);
                            // Detach — a spawn can provision a worktree,
                            // which on a cold cache runs `git clone --bare`
                            // (minutes on a big repo). Awaiting that inline
                            // freezes every keystroke `Write` behind it.
                            // The handler only touches Arc'd config state
                            // and broadcasts on the bus, so ordering with
                            // the serve loop doesn't matter.
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                spawn_handler::handle_spawn(
                                    &cfg,
                                    session_key,
                                    session_id,
                                    kind,
                                    cwd,
                                    initial_prompt,
                                    autonomous,
                                )
                                .await;
                            });
                        }
                        lazybox_ipc::Command::CreateSession { session_key, kind, label } => {
                            // Detach — provisions a fresh worktree folder
                            // (same clone exposure as Spawn).
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                spawn_handler::handle_create_session(
                                    &cfg,
                                    session_key,
                                    kind,
                                    label,
                                )
                                .await;
                            });
                        }
                        lazybox_ipc::Command::Write { terminal_id, bytes } => {
                            spawn_handler::handle_write(&self.config, terminal_id, &bytes).await;
                        }
                        lazybox_ipc::Command::RecordUserMessage { terminal_id, message } => {
                            spawn_handler::handle_record_user_message(
                                &self.config,
                                terminal_id,
                                &message,
                            )
                            .await;
                        }
                        lazybox_ipc::Command::InjectPrompt {
                            terminal_id,
                            prompt,
                            fallback_spawn,
                        } => {
                            // Detach — the stale-terminal fallback
                            // rewrites this into a full Spawn, which on
                            // a cold cache runs `git clone --bare`
                            // (minutes). Awaiting it inline froze every
                            // keystroke `Write` behind it — the exact
                            // class of stall the Spawn detach fixed.
                            // The handler only touches Arc'd config
                            // state, so loop ordering doesn't matter.
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                spawn_handler::handle_inject_prompt(
                                    &cfg,
                                    terminal_id,
                                    &prompt,
                                    fallback_spawn,
                                )
                                .await;
                            });
                        }
                        lazybox_ipc::Command::Resize { terminal_id, cols, rows } => {
                            spawn_handler::handle_resize(&self.config, terminal_id, cols, rows).await;
                        }
                        lazybox_ipc::Command::Close { terminal_id } => {
                            spawn_handler::handle_close(&self.config, terminal_id).await;
                        }
                        lazybox_ipc::Command::IngestHook { terminal_id, hook, backend_key } => {
                            spawn_handler::handle_ingest_hook(
                                &self.config,
                                terminal_id,
                                backend_key,
                                hook,
                            )
                            .await;
                        }
                        lazybox_ipc::Command::StartAgentRun {
                            session_key,
                            session_id,
                            agent,
                            mode,
                            cwd,
                            initial_input,
                        } => {
                            agent_runs::handle_start_agent_run(
                                &self.config,
                                session_key,
                                session_id,
                                agent,
                                mode,
                                cwd,
                                initial_input,
                            )
                            .await;
                        }
                        lazybox_ipc::Command::SendAgentInput { run_id, message } => {
                            agent_runs::handle_send_agent_input(&self.config, run_id, message)
                                .await;
                        }
                        lazybox_ipc::Command::InterruptAgentRun { run_id } => {
                            agent_runs::handle_interrupt_agent_run(&self.config, run_id).await;
                        }
                        lazybox_ipc::Command::DecideAgentApproval {
                            run_id,
                            request_id,
                            decision,
                        } => {
                            agent_runs::handle_decide_agent_approval(
                                &self.config,
                                run_id,
                                request_id,
                                decision,
                            )
                            .await;
                        }
                        lazybox_ipc::Command::AnswerAgentQuestion {
                            run_id,
                            question_id,
                            answer,
                        } => {
                            agent_runs::handle_answer_agent_question(
                                &self.config,
                                run_id,
                                question_id,
                                answer,
                            )
                            .await;
                        }
                        lazybox_ipc::Command::UpsertProviderCredential {
                            principal_id,
                            credential,
                        } => {
                            auth::handle_upsert_provider_credential(
                                &self.config,
                                &conn.tx,
                                principal_id,
                                credential,
                            )
                            .await;
                        }
                        lazybox_ipc::Command::RemoveProviderCredential {
                            principal_id,
                            provider_id,
                        } => {
                            auth::handle_remove_provider_credential(
                                &self.config,
                                &conn.tx,
                                principal_id,
                                provider_id,
                            )
                            .await;
                        }
                        lazybox_ipc::Command::ListProviderCredentials { principal_id } => {
                            auth::handle_list_provider_credentials(
                                &self.config,
                                &conn.tx,
                                principal_id,
                            )
                            .await;
                        }
                        lazybox_ipc::Command::MarkRead { session_key } => {
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            // MarkRead is the user's "I just looked
                            // at this workspace" signal — treat it
                            // as a focus hint too so the round-robin
                            // sync cursor bumps even when the TUI
                            // doesn't fire a separate
                            // `FocusWorkspace` (older clients, the
                            // auto-mark-on-hover path).
                            polling::set_focused_workspace(&self.config, &key).await;
                            polling::mark_workspace_read(&self.config, &key);
                        }
                        lazybox_ipc::Command::FocusWorkspace { session_key } => {
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::set_focused_workspace(&self.config, &key).await;
                        }
                        lazybox_ipc::Command::MarkActivityRead { session_key, index } => {
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::mark_activity_read(&self.config, &key, index);
                        }
                        lazybox_ipc::Command::UnmarkActivityRead { session_key, index } => {
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::unmark_activity_read(&self.config, &key, index);
                        }
                        lazybox_ipc::Command::CreateWorkspace {
                            name,
                            project_key,
                            spawn_agent,
                        } => {
                            // create_empty_workspace runs inline (cheap
                            // store write) and returns the final key —
                            // which may carry a `-2` collision suffix, so
                            // the client can't predict it. When the caller
                            // asked to land in a live session, chain the
                            // spawn here off that returned key rather than
                            // round-tripping through the client.
                            let key =
                                polling::create_empty_workspace(&self.config, &name, project_key);
                            if let Some(agent_id) = spawn_agent {
                                // Detach — same worktree-provision (cold
                                // `git clone --bare`) exposure as a bare
                                // Spawn; never block the serve loop on it.
                                // Bare interactive spawn (no prompt) keeps
                                // the human-in-the-loop approval gate.
                                let cfg = self.config.clone();
                                let session_key: lazybox_core::SessionKey = (&key).into();
                                mutations.spawn(async move {
                                    spawn_handler::handle_spawn(
                                        &cfg,
                                        session_key,
                                        None,
                                        lazybox_ipc::TerminalKind::Agent(agent_id),
                                        None,
                                        None,
                                        false,
                                    )
                                    .await;
                                });
                            }
                        }
                        lazybox_ipc::Command::CreateProject { name } => {
                            polling::create_local_project(&self.config, &name);
                        }
                        lazybox_ipc::Command::Snooze { session_key, until } => {
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::set_snooze(&self.config, &key, Some(until));
                        }
                        lazybox_ipc::Command::Unsnooze { session_key } => {
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::set_snooze(&self.config, &key, None);
                        }
                        lazybox_ipc::Command::Kill { session_key } => {
                            // Detach — tears down worktrees (git ops +
                            // filesystem removal) and backend sessions; a
                            // slow disk or wedged git must not freeze the
                            // serve loop.
                            //
                            // Serialized against any in-flight Spawn on
                            // the same workspace: tearing down while a
                            // spawn is mid-provision would let the spawn
                            // re-create the worktree + a terminal right
                            // after deletion. The tombstone makes a spawn
                            // that hasn't loaded its workspace row yet
                            // abort instead of falling back to spawning
                            // in the daemon's own cwd.
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                cfg.deleted_workspaces
                                    .lock()
                                    .expect("deleted_workspaces poisoned")
                                    .insert(key.as_str().to_string());
                                spawn_handler::await_inflight_spawns(&cfg, key.as_str()).await;
                                polling::delete_workspace(&cfg, &key).await;
                            });
                        }
                        lazybox_ipc::Command::RemoveMergedWorkspace { session_key } => {
                            // Detach — same worktree-teardown (and same
                            // spawn-race) exposure as Kill.
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                cfg.deleted_workspaces
                                    .lock()
                                    .expect("deleted_workspaces poisoned")
                                    .insert(key.as_str().to_string());
                                spawn_handler::await_inflight_spawns(&cfg, key.as_str()).await;
                                polling::remove_merged_workspace(&cfg, &key).await;
                            });
                        }
                        lazybox_ipc::Command::DeleteProject { project_key } => {
                            // Detach — deletes every workspace in the
                            // project (N worktree teardowns).
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::delete_project(&cfg, &project_key).await;
                            });
                        }
                        lazybox_ipc::Command::CollapseIntoPr {
                            issue_workspace_key,
                        } => {
                            // Detach — moves worktrees + freezes/kills
                            // backend sessions; can shell out to git.
                            let key = lazybox_core::WorkspaceKey::new(
                                issue_workspace_key.as_str().to_string(),
                            );
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_collapse_into_pr(&cfg, key).await;
                            });
                        }
                        lazybox_ipc::Command::Refresh => {
                            // Manual poll trigger. Wakes the long-lived
                            // poll loop so it runs `run_one_tick`
                            // immediately on its own task — single
                            // source of truth for ticks, no parallel
                            // inline spawn that could race the loop's
                            // own next-due bookkeeping.
                            //
                            // Force a full sweep on that tick: a manual
                            // refresh must feel authoritative. The
                            // incremental notifications path can't see
                            // an issue the user just created (GitHub
                            // sends no self-notification), so without
                            // this the new issue wouldn't appear until
                            // the next scheduled sweep, up to 10 min
                            // away (issue #180). The flag lives on the
                            // shared notifications state, so the source
                            // built from the cached client next tick
                            // observes it.
                            if let Some(client) = self
                                .config
                                .gh_client_cache
                                .lock()
                                .expect("gh_client_cache poisoned")
                                .as_ref()
                            {
                                client.force_full_sweep();
                            }
                            self.config.poll_wake.notify_one();
                        }
                        lazybox_ipc::Command::PostReply { session_key, body } => {
                            // GraphQL post — detach to keep the serve
                            // loop responsive while the network call
                            // is in flight.
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::post_reply(&cfg, session_key, body).await;
                            });
                        }
                        lazybox_ipc::Command::SetSessionLayout {
                            session_key,
                            session_id_raw,
                            layout_json,
                        } => {
                            let key = lazybox_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            let session_id = uuid::Uuid::parse_str(&session_id_raw)
                                .ok()
                                .map(lazybox_core::SessionId);
                            let layout: Option<lazybox_core::SessionLayout> =
                                serde_json::from_str(&layout_json).ok();
                            if let (Some(sid), Some(lay)) = (session_id, layout) {
                                polling::set_session_layout(&self.config, &key, sid, lay);
                            } else {
                                tracing::warn!(
                                    "SetSessionLayout: bad payload (id={:?})",
                                    session_id_raw
                                );
                            }
                        }
                        lazybox_ipc::Command::Shutdown => break,
                        lazybox_ipc::Command::ConfirmMerge {
                            issue_workspace_key,
                            pr_workspace_key,
                            accept,
                        } => {
                            // GraphQL merge → detach so a slow network
                            // can't freeze the serve loop. See the
                            // `FetchPrDetails` comment below for the
                            // full reasoning; the bug originally
                            // surfaced there but every GraphQL-touching
                            // handler has the same exposure.
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_confirm_merge(
                                    &cfg,
                                    issue_workspace_key,
                                    pr_workspace_key,
                                    accept,
                                )
                                .await;
                            });
                        }
                        lazybox_ipc::Command::AdoptSessions {
                            source_workspace_key,
                            target_workspace_key,
                        } => {
                            // Detach — session adoption migrates worktrees
                            // (git worktree move + freeze/resume), which
                            // can stall on a wedged git or tmux.
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_adopt_sessions(
                                    &cfg,
                                    source_workspace_key,
                                    target_workspace_key,
                                )
                                .await;
                            });
                        }
                        lazybox_ipc::Command::MergePr { workspace_key } => {
                            // Detach for the same reason as
                            // `FetchPrDetails` — `gh pr merge` shells
                            // out and can stall for seconds.
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_merge_pr(&cfg, workspace_key).await;
                            });
                        }
                        lazybox_ipc::Command::FetchPrDetails { workspace_key } => {
                            // **Bug fix**: `handle_fetch_pr_details`
                            // runs a GraphQL HTTP call. If the network
                            // stalls (octocrab, dropped connection,
                            // silent rate-limit), `.await`-ing it
                            // inline freezes the entire serve loop —
                            // `tokio::select!` cannot pick the next
                            // arm until the current one returns. The
                            // user perceives this as "Spawn key does
                            // nothing": Spawn/Write/MarkRead all
                            // queue behind the wedged fetch.
                            //
                            // Spawning detaches the handler so the
                            // serve loop is back in `select!` within
                            // microseconds. Order doesn't matter for
                            // this handler — it just merges activity
                            // and broadcasts via the bus.
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_fetch_pr_details(&cfg, workspace_key).await;
                            });
                        }
                        lazybox_ipc::Command::RequestReviewers { workspace_key, logins } => {
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_request_reviewers(&cfg, workspace_key, logins)
                                    .await;
                            });
                        }
                        lazybox_ipc::Command::AddAssignees { workspace_key, logins } => {
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_add_assignees(&cfg, workspace_key, logins).await;
                            });
                        }
                        lazybox_ipc::Command::SetAssignees { workspace_key, logins } => {
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_set_assignees(&cfg, workspace_key, logins).await;
                            });
                        }
                        lazybox_ipc::Command::SetLabels { workspace_key, names } => {
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_set_labels(&cfg, workspace_key, names).await;
                            });
                        }
                        lazybox_ipc::Command::FetchRepoLabels { workspace_key } => {
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_fetch_repo_labels(&cfg, workspace_key).await;
                            });
                        }
                        lazybox_ipc::Command::CleanWorktrees => {
                            // Detach — the walk does N filesystem
                            // ops (one `git worktree remove` per
                            // session) and the user shouldn't wait
                            // on the serve loop while it runs.
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_clean_worktrees(&cfg).await;
                            });
                        }
                        lazybox_ipc::Command::InspectWorktrees => {
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_inspect_worktrees(&cfg).await;
                            });
                        }
                        lazybox_ipc::Command::DeleteOrphanedWorktree { path, force } => {
                            let cfg = self.config.clone();
                            mutations.spawn(async move {
                                polling::handle_delete_orphaned_worktree(&cfg, path, force).await;
                            });
                        }
                    }
                    // Anything that held the single serve-loop task for
                    // more than a couple frames blocked every other
                    // command behind it. Warn loudly with the label so
                    // a "frozen during sync" report points straight at
                    // the offending handler instead of a guessing game.
                    let cmd_ms = cmd_started.elapsed().as_millis();
                    if cmd_ms >= 50 {
                        tracing::warn!(
                            command = label,
                            ms = cmd_ms,
                            "daemon serve loop BLOCKED on inline command handler — \
                             all other commands (incl. keystroke Writes) stalled this long"
                        );
                    } else {
                        tracing::debug!(command = label, ms = cmd_ms, "daemon → handled");
                    }
                }
                bus = bus_rx.recv() => {
                    match bus {
                        Ok(evt) => {
                            let _ = conn.tx.send(evt);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Slow client missed `n` events — possibly
                            // including one-shot lifecycle events
                            // (TerminalSpawned/Exited, SessionUpserted)
                            // that never repeat, leaving the client with
                            // zombie tabs / stuck spinners forever. Push
                            // a synthetic full Snapshot (same payload the
                            // Subscribe handler builds) so the client
                            // self-heals.
                            //
                            // Built INLINE (awaited here, store scans on
                            // `spawn_blocking`), not on a detached task:
                            // detaching let bus events forwarded between
                            // the snapshot LOAD and its SEND be
                            // overwritten by the older snapshot — zombie
                            // state came back the moment the resync
                            // landed. While this builds, no events are
                            // forwarded for this connection, which is
                            // exactly the sequencing the snapshot needs;
                            // lag recovery is rare enough that the brief
                            // serve-loop pause is acceptable.
                            self.config.event_metrics.record_bus_lagged(n);
                            tracing::warn!(
                                lagged = n,
                                bus_lagged_total =
                                    self.config.event_metrics.snapshot().bus_lagged_events,
                                "client lagged behind bus — sending recovery snapshot"
                            );
                            let store = self.config.store.clone();
                            match tokio::task::spawn_blocking(move || {
                                (load_workspaces(&*store), load_projects(&*store))
                            })
                            .await
                            {
                                Ok((workspaces, projects)) => {
                                    let terminals =
                                        spawn_handler::snapshot_terminals(&self.config).await;
                                    let _ = conn.tx.send(Event::Snapshot {
                                        workspaces,
                                        terminals,
                                        projects,
                                    });
                                    self.config.event_metrics.record_bus_lag_recovery();
                                }
                                Err(e) => {
                                    // Send NOTHING: an empty snapshot
                                    // would wipe the client's sidebar,
                                    // which is strictly worse than
                                    // staying lagged until the next
                                    // recovery opportunity.
                                    tracing::error!(
                                        "lag-recovery snapshot load failed: {e} — \
                                         continuing without a resync",
                                    );
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        // Drain detached mutation tasks before returning — `Shutdown =>
        // break` used to abandon an in-flight Kill / Spawn / inject
        // mid-write. Bounded: a wedged clone or git op must not hold
        // shutdown hostage, so anything still running after 5s is
        // abandoned (with a breadcrumb).
        if !mutations.is_empty() {
            let drain = async { while mutations.join_next().await.is_some() {} };
            if tokio::time::timeout(std::time::Duration::from_secs(5), drain)
                .await
                .is_err()
            {
                tracing::warn!(
                    "shutdown: detached mutation task(s) still running after 5s — abandoning them"
                );
            }
        }
        Ok(())
    }
}

/// Build a `PersistedSetup` from the YAML's `setup:` section so the
/// daemon can run a one-off poll (Command::Refresh) using the
/// latest user-edited subscriptions, without waiting for the long-
/// lived poll loop's next tick.
pub fn persisted_from_config(c: &lazybox_config::Config) -> lazybox_core::PersistedSetup {
    lazybox_core::PersistedSetup {
        enabled_providers: c.setup.providers.clone(),
        enabled_agents: c.setup.agents.clone(),
        provider_filters: c
            .setup
            .filters
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    lazybox_core::ProviderConfig {
                        enabled_keys: v.clone(),
                    },
                )
            })
            .collect(),
        selected_scopes: c.setup.scopes.clone(),
    }
}

/// Deserialize every persisted `Workspace`. Bad JSON is logged and
/// skipped so a single corrupted row doesn't break startup.
fn load_workspaces(store: &dyn Store) -> Vec<lazybox_core::Workspace> {
    let records = match store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("list_workspaces failed: {e}");
            return vec![];
        }
    };
    records
        .into_iter()
        .filter_map(|r| {
            let json = r.workspace_json?;
            match serde_json::from_str::<lazybox_core::Workspace>(&json) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!("skipping unreadable workspace {}: {e}", r.key);
                    None
                }
            }
        })
        .collect()
}

/// Same shape as `load_workspaces` for the project table. Used by
/// `Snapshot` to seed the sidebar's project headers on reconnect.
fn load_projects(store: &dyn Store) -> Vec<lazybox_core::Project> {
    let records = match store.list_projects() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("list_projects failed: {e}");
            return vec![];
        }
    };
    records
        .into_iter()
        .filter_map(|r| {
            let json = r.project_json?;
            match serde_json::from_str::<lazybox_core::Project>(&json) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("skipping unreadable project {}: {e}", r.key);
                    None
                }
            }
        })
        .collect()
}
