//! pilot-server — owns state and IO on behalf of TUI clients.
//!
//! Lives as a library so the in-process transport can call `Server::serve`
//! without a subprocess. When out-of-process (remote access, long-running
//! service), the `pilot` binary's `daemon` subcommand invokes the same
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
pub mod lifecycle;
pub mod polling;
pub mod pty;
pub mod slack;
pub mod socket_service;
pub mod spawn_handler;

use crate::backend::{RawPtyBackend, SessionBackend, TmuxBackend};
use pilot_agents::Registry;
use pilot_ipc::{AgentRunId, Connection, Event, TerminalId};
use pilot_store::{MemoryStore, SqliteStore, Store};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{Mutex, broadcast};

/// Where pilot keeps its persistent state. Thin alias over
/// [`pilot_core::paths::state_db`] so callers don't have to import
/// the paths module just for this one helper. Override via the
/// `PILOT_HOME` env var (see `pilot_core::paths` for details).
pub fn state_db_path() -> PathBuf {
    pilot_core::paths::state_db()
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

// REMOVED: wipe_legacy_worktrees. We never delete from `~/.pilot/`.
// pilot is constrained to `~/.pilot/v2/` for everything it writes
// — `state.db`, the bare-clone cache, every worktree. If a user
// has real work in `~/.pilot/worktrees/` from a prior tool, that's
// their data and pilot leaves it alone.

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
    /// in since `pilot_store` errors are `Box<dyn Error>`-shaped, but
    /// the human message survives.
    #[error("store: {0}")]
    Store(String),
    /// Workspace not found, malformed, or missing a required field
    /// (primary task, repo, branch). Surfaces from the spawn/upsert
    /// paths when the requested workspace can't be resolved into a
    /// concrete worktree target.
    #[error("workspace: {0}")]
    Workspace(String),
    /// Worktree / git operation failed. Wraps `pilot_git_ops` errors
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
/// [`spawn_handler::freeze_runners_in_session`]; that's why this
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
/// the whole process. Both `run_embedded` and `pilot daemon start`
/// build it once at startup so the polling loop's `SessionUpserted`
/// events reach every connected TUI.
#[derive(Clone)]
pub struct ServerConfig {
    pub agents: Registry,
    /// Persistent state at `~/.pilot/v2/state.db`.
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
    pub terminal_sessions: Arc<Mutex<HashMap<TerminalId, pilot_core::SessionId>>>,
    /// Cached `AgentState` per agent terminal. Populated by the
    /// output pump's state detector; transitions are broadcast as
    /// `Event::AgentState`. Caching avoids broadcasting on every
    /// PTY chunk when nothing changed.
    pub agent_states: Arc<Mutex<HashMap<TerminalId, pilot_ipc::AgentState>>>,
    /// Wire-side metadata per terminal: `(session_key, kind)`. The
    /// `terminals` map only carries the backend key; clients
    /// reconnecting via Subscribe need the full pairing so the
    /// initial Snapshot can route terminals into the right tab
    /// strip. Populated by `handle_spawn`, cleaned on
    /// `TerminalExited`.
    pub terminal_meta:
        Arc<Mutex<HashMap<TerminalId, (pilot_core::SessionKey, pilot_ipc::TerminalKind)>>>,
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
    pub default_principal_id: pilot_ipc::PrincipalId,
    /// Cross-tick polling state — provider-error debounce + the
    /// "already prompted" set for out-of-scope workspaces. Shared
    /// between the long-lived poll loop and `Command::Refresh`'s
    /// one-shot tick so dismissed prompts stay dismissed across
    /// both paths. Without this, Refresh would prompt, you'd
    /// dismiss, and 30s later the long-lived loop would re-prompt
    /// (each has its own `TickState`).
    pub poll_state: Arc<Mutex<polling::TickState>>,
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
}

impl ServerConfig {
    /// Open the store at `~/.pilot/v2/state.db`.
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
        // survive pilot-server restart and can be attached externally
        // via `tmux -L pilot attach -t <key>`; raw-pty is the
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
    /// don't want tmux side-effects (a real pilot tmux server, leftover
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
            agent_runs: Arc::new(Mutex::new(HashMap::new())),
            next_agent_run_id: Arc::new(AtomicU64::new(1)),
            credential_store: Arc::new(auth::MemoryCredentialStore::new()),
            default_principal_id: pilot_ipc::PrincipalId::local(),
            poll_state: Arc::new(Mutex::new(polling::TickState::default())),
            viewer_identities: Arc::new(std::sync::Mutex::new(Vec::new())),
            poll_wake: Arc::new(tokio::sync::Notify::new()),
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
    ) -> Option<(pilot_core::SessionKey, pilot_ipc::TerminalKind)> {
        self.terminal_meta.lock().await.get(&id).cloned()
    }

    /// Snapshot the cached `AgentState` for a terminal. `None` when
    /// the pump hasn't observed a state transition yet.
    pub async fn agent_state_for(&self, id: TerminalId) -> Option<pilot_ipc::AgentState> {
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
        let mut bus_rx = self.config.bus.subscribe();
        loop {
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
                        pilot_ipc::Command::Spawn { .. } => "Spawn",
                        pilot_ipc::Command::Close { .. } => "Close",
                        pilot_ipc::Command::CreateSession { .. } => "CreateSession",
                        pilot_ipc::Command::Subscribe => "Subscribe",
                        pilot_ipc::Command::Refresh => "Refresh",
                        pilot_ipc::Command::Write { .. } => "Write",
                        pilot_ipc::Command::Resize { .. } => "Resize",
                        pilot_ipc::Command::InjectPrompt { .. } => "InjectPrompt",
                        pilot_ipc::Command::MarkRead { .. } => "MarkRead",
                        pilot_ipc::Command::FocusWorkspace { .. } => "FocusWorkspace",
                        pilot_ipc::Command::MarkActivityRead { .. } => "MarkActivityRead",
                        pilot_ipc::Command::UnmarkActivityRead { .. } => "UnmarkActivityRead",
                        pilot_ipc::Command::FetchPrDetails { .. } => "FetchPrDetails",
                        pilot_ipc::Command::PostReply { .. } => "PostReply",
                        pilot_ipc::Command::MergePr { .. } => "MergePr",
                        pilot_ipc::Command::ConfirmMerge { .. } => "ConfirmMerge",
                        pilot_ipc::Command::Snooze { .. } => "Snooze",
                        pilot_ipc::Command::Unsnooze { .. } => "Unsnooze",
                        pilot_ipc::Command::Kill { .. } => "Kill",
                        pilot_ipc::Command::DeleteProject { .. } => "DeleteProject",
                        pilot_ipc::Command::CollapseIntoPr { .. } => "CollapseIntoPr",
                        pilot_ipc::Command::CreateWorkspace { .. } => "CreateWorkspace",
                        pilot_ipc::Command::CreateProject { .. } => "CreateProject",
                        pilot_ipc::Command::AdoptSessions { .. } => "AdoptSessions",
                        pilot_ipc::Command::RequestReviewers { .. } => "RequestReviewers",
                        pilot_ipc::Command::AddAssignees { .. } => "AddAssignees",
                        pilot_ipc::Command::SetAssignees { .. } => "SetAssignees",
                        pilot_ipc::Command::SetSessionLayout { .. } => "SetSessionLayout",
                        pilot_ipc::Command::StartAgentRun { .. } => "StartAgentRun",
                        pilot_ipc::Command::SendAgentInput { .. } => "SendAgentInput",
                        pilot_ipc::Command::InterruptAgentRun { .. } => "InterruptAgentRun",
                        pilot_ipc::Command::DecideAgentApproval { .. } => "DecideAgentApproval",
                        pilot_ipc::Command::AnswerAgentQuestion { .. } => "AnswerAgentQuestion",
                        pilot_ipc::Command::UpsertProviderCredential { .. } => "UpsertProviderCredential",
                        pilot_ipc::Command::RemoveProviderCredential { .. } => "RemoveProviderCredential",
                        pilot_ipc::Command::ListProviderCredentials { .. } => "ListProviderCredentials",
                        pilot_ipc::Command::CleanWorktrees => "CleanWorktrees",
                        pilot_ipc::Command::Shutdown => "Shutdown",
                    };
                    tracing::info!("daemon ← {label}");
                    match cmd {
                        pilot_ipc::Command::Subscribe => {
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
                            let store_for_ws = self.config.store.clone();
                            let store_for_proj = self.config.store.clone();
                            let workspaces = tokio::task::spawn_blocking(move || {
                                load_workspaces(&*store_for_ws)
                            })
                            .await
                            .unwrap_or_default();
                            let projects = tokio::task::spawn_blocking(move || {
                                load_projects(&*store_for_proj)
                            })
                            .await
                            .unwrap_or_default();
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
                        pilot_ipc::Command::Spawn {
                            session_key,
                            session_id,
                            kind,
                            cwd,
                            initial_prompt,
                        } => {
                            spawn_handler::handle_spawn(
                                &self.config,
                                session_key,
                                session_id,
                                kind,
                                cwd,
                                initial_prompt,
                            )
                            .await;
                        }
                        pilot_ipc::Command::CreateSession { session_key, kind, label } => {
                            spawn_handler::handle_create_session(
                                &self.config,
                                session_key,
                                kind,
                                label,
                            )
                            .await;
                        }
                        pilot_ipc::Command::Write { terminal_id, bytes } => {
                            spawn_handler::handle_write(&self.config, terminal_id, &bytes).await;
                        }
                        pilot_ipc::Command::InjectPrompt {
                            terminal_id,
                            prompt,
                            fallback_spawn,
                        } => {
                            spawn_handler::handle_inject_prompt(
                                &self.config,
                                terminal_id,
                                &prompt,
                                fallback_spawn,
                            )
                            .await;
                        }
                        pilot_ipc::Command::Resize { terminal_id, cols, rows } => {
                            spawn_handler::handle_resize(&self.config, terminal_id, cols, rows).await;
                        }
                        pilot_ipc::Command::Close { terminal_id } => {
                            spawn_handler::handle_close(&self.config, terminal_id).await;
                        }
                        pilot_ipc::Command::StartAgentRun {
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
                        pilot_ipc::Command::SendAgentInput { run_id, message } => {
                            agent_runs::handle_send_agent_input(&self.config, run_id, message)
                                .await;
                        }
                        pilot_ipc::Command::InterruptAgentRun { run_id } => {
                            agent_runs::handle_interrupt_agent_run(&self.config, run_id).await;
                        }
                        pilot_ipc::Command::DecideAgentApproval {
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
                        pilot_ipc::Command::AnswerAgentQuestion {
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
                        pilot_ipc::Command::UpsertProviderCredential {
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
                        pilot_ipc::Command::RemoveProviderCredential {
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
                        pilot_ipc::Command::ListProviderCredentials { principal_id } => {
                            auth::handle_list_provider_credentials(
                                &self.config,
                                &conn.tx,
                                principal_id,
                            )
                            .await;
                        }
                        pilot_ipc::Command::MarkRead { session_key } => {
                            let key = pilot_core::WorkspaceKey::new(
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
                        pilot_ipc::Command::FocusWorkspace { session_key } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::set_focused_workspace(&self.config, &key).await;
                        }
                        pilot_ipc::Command::MarkActivityRead { session_key, index } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::mark_activity_read(&self.config, &key, index);
                        }
                        pilot_ipc::Command::UnmarkActivityRead { session_key, index } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::unmark_activity_read(&self.config, &key, index);
                        }
                        pilot_ipc::Command::CreateWorkspace { name, project_key } => {
                            polling::create_empty_workspace(&self.config, &name, project_key);
                        }
                        pilot_ipc::Command::CreateProject { name } => {
                            polling::create_local_project(&self.config, &name);
                        }
                        pilot_ipc::Command::Snooze { session_key, until } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::set_snooze(&self.config, &key, Some(until));
                        }
                        pilot_ipc::Command::Unsnooze { session_key } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::set_snooze(&self.config, &key, None);
                        }
                        pilot_ipc::Command::Kill { session_key } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            polling::delete_workspace(&self.config, &key).await;
                        }
                        pilot_ipc::Command::DeleteProject { project_key } => {
                            polling::delete_project(&self.config, &project_key).await;
                        }
                        pilot_ipc::Command::CollapseIntoPr {
                            issue_workspace_key,
                        } => {
                            let key = pilot_core::WorkspaceKey::new(
                                issue_workspace_key.as_str().to_string(),
                            );
                            polling::handle_collapse_into_pr(&self.config, key).await;
                        }
                        pilot_ipc::Command::Refresh => {
                            // Manual poll trigger. Wakes the long-lived
                            // poll loop so it runs `run_one_tick`
                            // immediately on its own task — single
                            // source of truth for ticks, no parallel
                            // inline spawn that could race the loop's
                            // own next-due bookkeeping.
                            self.config.poll_wake.notify_one();
                        }
                        pilot_ipc::Command::PostReply { session_key, body } => {
                            // GraphQL post — detach to keep the serve
                            // loop responsive while the network call
                            // is in flight.
                            let cfg = self.config.clone();
                            tokio::spawn(async move {
                                polling::post_reply(&cfg, session_key, body).await;
                            });
                        }
                        pilot_ipc::Command::SetSessionLayout {
                            session_key,
                            session_id_raw,
                            layout_json,
                        } => {
                            let key = pilot_core::WorkspaceKey::new(
                                session_key.as_str().to_string(),
                            );
                            let session_id = uuid::Uuid::parse_str(&session_id_raw)
                                .ok()
                                .map(pilot_core::SessionId);
                            let layout: Option<pilot_core::SessionLayout> =
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
                        pilot_ipc::Command::Shutdown => break,
                        pilot_ipc::Command::ConfirmMerge {
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
                            tokio::spawn(async move {
                                polling::handle_confirm_merge(
                                    &cfg,
                                    issue_workspace_key,
                                    pr_workspace_key,
                                    accept,
                                )
                                .await;
                            });
                        }
                        pilot_ipc::Command::AdoptSessions {
                            source_workspace_key,
                            target_workspace_key,
                        } => {
                            polling::handle_adopt_sessions(
                                &self.config,
                                source_workspace_key,
                                target_workspace_key,
                            )
                            .await;
                        }
                        pilot_ipc::Command::MergePr { workspace_key } => {
                            // Detach for the same reason as
                            // `FetchPrDetails` — `gh pr merge` shells
                            // out and can stall for seconds.
                            let cfg = self.config.clone();
                            tokio::spawn(async move {
                                polling::handle_merge_pr(&cfg, workspace_key).await;
                            });
                        }
                        pilot_ipc::Command::FetchPrDetails { workspace_key } => {
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
                            tokio::spawn(async move {
                                polling::handle_fetch_pr_details(&cfg, workspace_key).await;
                            });
                        }
                        pilot_ipc::Command::RequestReviewers { workspace_key, logins } => {
                            let cfg = self.config.clone();
                            tokio::spawn(async move {
                                polling::handle_request_reviewers(&cfg, workspace_key, logins)
                                    .await;
                            });
                        }
                        pilot_ipc::Command::AddAssignees { workspace_key, logins } => {
                            let cfg = self.config.clone();
                            tokio::spawn(async move {
                                polling::handle_add_assignees(&cfg, workspace_key, logins).await;
                            });
                        }
                        pilot_ipc::Command::SetAssignees { workspace_key, logins } => {
                            let cfg = self.config.clone();
                            tokio::spawn(async move {
                                polling::handle_set_assignees(&cfg, workspace_key, logins).await;
                            });
                        }
                        pilot_ipc::Command::CleanWorktrees => {
                            // Detach — the walk does N filesystem
                            // ops (one `git worktree remove` per
                            // session) and the user shouldn't wait
                            // on the serve loop while it runs.
                            let cfg = self.config.clone();
                            tokio::spawn(async move {
                                polling::handle_clean_worktrees(&cfg).await;
                            });
                        }
                    }
                }
                bus = bus_rx.recv() => {
                    match bus {
                        Ok(evt) => {
                            let _ = conn.tx.send(evt);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Slow client missed `n` events. Tracing
                            // tells us when this is happening; the loop
                            // continues from the next still-buffered
                            // event so we don't block the bus.
                            tracing::warn!("client lagged behind bus by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        Ok(())
    }
}

/// Build a `PersistedSetup` from the YAML's `setup:` section so the
/// daemon can run a one-off poll (Command::Refresh) using the
/// latest user-edited subscriptions, without waiting for the long-
/// lived poll loop's next tick.
pub fn persisted_from_config(c: &pilot_config::Config) -> pilot_core::PersistedSetup {
    pilot_core::PersistedSetup {
        enabled_providers: c.setup.providers.clone(),
        enabled_agents: c.setup.agents.clone(),
        provider_filters: c
            .setup
            .filters
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    pilot_core::ProviderConfig {
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
fn load_workspaces(store: &dyn Store) -> Vec<pilot_core::Workspace> {
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
            match serde_json::from_str::<pilot_core::Workspace>(&json) {
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
fn load_projects(store: &dyn Store) -> Vec<pilot_core::Project> {
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
            match serde_json::from_str::<pilot_core::Project>(&json) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("skipping unreadable project {}: {e}", r.key);
                    None
                }
            }
        })
        .collect()
}
