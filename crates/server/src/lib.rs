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
pub mod agent_stream;
pub mod agent_updates;
pub mod api_gateway;
pub mod auth;
pub mod backend;
pub mod chat;
pub mod client_kv;
pub mod event_forward;
pub mod keep_awake;
pub mod lifecycle;
pub mod metrics;
pub mod polling;
pub mod pty;
pub mod slack;
pub mod socket_service;
pub mod spawn_handler;
mod terminal_commands;
mod terminal_io;

use crate::backend::{RawPtyBackend, SessionBackend, TmuxBackend};
use lazybox_agents::Registry;
use lazybox_ipc::{AgentRunId, Connection, Event, TerminalId};
use lazybox_store::{MemoryStore, SqliteStore, Store};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{Mutex, broadcast};

/// Filesystem root used by daemon-owned git worktrees.
///
/// Production configs point at the persistent state root. Test configs get a
/// unique scratch root that is removed with the last config clone, keeping
/// tests away from the developer's real `~/.lazybox` clone cache.
#[derive(Debug)]
struct WorktreeRoot {
    path: PathBuf,
    cleanup_on_drop: bool,
}

impl WorktreeRoot {
    fn persistent(path: PathBuf) -> Self {
        Self {
            path,
            cleanup_on_drop: false,
        }
    }

    fn scratch() -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "lazybox-server-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            )),
            cleanup_on_drop: true,
        }
    }
}

impl Drop for WorktreeRoot {
    fn drop(&mut self) {
        if self.cleanup_on_drop
            && let Err(error) = std::fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                "failed to remove scratch worktree root: {error}"
            );
        }
    }
}

/// Where lazybox keeps its persistent state. Thin alias over
/// [`lazybox_core::paths::state_db`] so callers don't have to import
/// the paths module just for this one helper. Override via the
/// `LAZYBOX_HOME` env var (see `lazybox_core::paths` for details).
pub fn state_db_path() -> PathBuf {
    lazybox_core::paths::state_db()
}

/// Open the persistent store at the canonical path.
///
/// Failure is explicit: continuing with an empty in-memory database
/// would make existing workspaces appear deleted and allow new state to
/// vanish on exit.
pub fn open_store() -> Result<Arc<dyn Store>, ServerError> {
    open_store_at(&state_db_path())
}

fn open_store_at(path: &Path) -> Result<Arc<dyn Store>, ServerError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ServerError::Store(format!(
                "create state directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    SqliteStore::open(path)
        .map(|store| Arc::new(store) as Arc<dyn Store>)
        .map_err(|error| {
            ServerError::Store(format!("open persistent state {}: {error}", path.display()))
        })
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

/// Inline-lane budget. A command in the inline lane (see `command_lane`)
/// that holds the serve loop longer than this is an architecture
/// violation: while it runs, `tokio::select!` can service no other
/// command and forward no bus/poll event (#34, #206). The detached lane
/// carries everything that can touch the network, git, the filesystem,
/// or a contended lock, so a healthy inline handler stays in the low
/// single-digit milliseconds — the budget is generous enough never to
/// trip on scheduler jitter, yet far below the multi-second stalls a
/// real wedge produces. Crossing it bumps the
/// `inline_budget_violations` metric and logs the offending command.
pub const INLINE_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// Maximum concurrent slow command handlers owned by one client connection.
/// The two long-lived terminal routers are counted separately. When this cap
/// is reached, new detached mutations are rejected before spawning a task.
pub const MAX_CONNECTION_MUTATIONS: usize = 32;

/// How long a closing connection waits for its in-flight detached
/// mutation tasks (merge saves, worktree teardowns, spawns) before
/// abandoning them. Applied on EVERY serve-loop exit — client
/// disconnect, `Command::Shutdown`, and the graceful-stop signal the
/// socket service raises on SIGTERM — so a mutation that already
/// succeeded remotely isn't cancelled before its local save/broadcast.
/// Bounded: a wedged clone or git op must not hold shutdown hostage.
pub const MUTATION_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const CONNECTION_ROUTER_TASKS: usize = 2;

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
/// Any co-holding site (one that locks two of these maps at once)
/// must follow this order; the constant exists as a discoverable name
/// future callers can grep when they add one.
pub const TERMINAL_MAP_LOCK_ORDER: &str =
    "terminals → terminal_meta → terminal_sessions → agent_states";

/// `ServerConfig` is the per-process state shared across all client
/// connections — the persistent store, the broadcast bus the poller
/// pushes events into, and the agent registry the spawn handler reads.
/// Cheaply cloneable: `store` is `Arc`, `bus` is a tokio broadcast
/// `Sender` (clone is a refcount), `agents` is a small struct.
///
/// Per-process invariant: there is exactly **one** `ServerConfig` for
/// the whole process. Both `run_embedded` and `lazybox server start`
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
    /// Root for every daemon-owned bare clone and worktree. Private so
    /// production code cannot bypass [`ServerConfig::worktree_manager`].
    worktree_root: Arc<WorktreeRoot>,
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
    /// Terminals running on the repo's shared main checkout rather than
    /// an isolated worktree. Populated by `handle_spawn` for a
    /// main-checkout spawn; read by `snapshot_terminals` (badge replay
    /// on reconnect) and `find_existing_singleton` (so a main-checkout
    /// agent is a distinct singleton from the same agent on an isolated
    /// worktree in the same workspace). Cleaned on `TerminalExited`
    /// alongside the other per-terminal maps.
    pub on_main_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Model-tier display label a terminal was launched with (`"Opus"`),
    /// keyed by terminal id. Populated by `handle_spawn` when the user
    /// picked a tier; read by `snapshot_terminals` so a reconnecting
    /// client re-renders the tier badge. Cleaned on `TerminalExited`
    /// alongside the other per-terminal maps.
    pub terminal_models: Arc<Mutex<HashMap<TerminalId, String>>>,
    /// Recovered agent processes launched under an older PTY compatibility
    /// generation. Their inherited environment cannot be changed in place;
    /// Subscribe reports an actionable restart notice until they exit.
    pub outdated_agent_terminals: Arc<Mutex<HashSet<TerminalId>>>,
    /// Per-backend-key serialization between prompt-state persistence and
    /// terminal teardown. Draft/user-message writes run on a background lane;
    /// without this boundary a delayed write could finish after teardown's
    /// deletes and resurrect orphaned `terminal-draft:*` rows.
    terminal_persistence_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Global per-backend interaction lock shared by every connection and
    /// producer (keyboard input, prompt injection, chat, submit retry). The
    /// per-connection command lanes cannot by themselves prevent concurrent
    /// writes from different owners corrupting a PTY byte stream.
    terminal_io_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
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
    /// At most one readiness-gated prompt injection may wait per terminal.
    /// A synchronous mutex lets the spawned waiter's RAII guard release the
    /// reservation on every return/cancellation path.
    pub pending_prompt_injections: Arc<parking_lot::Mutex<std::collections::HashSet<TerminalId>>>,
    /// Factory for a structured agent run's underlying process I/O.
    /// Defaults to spawning a real subprocess; tests swap in an
    /// in-memory fake so they never launch an agent CLI or a shell.
    pub agent_stream_spawner: Arc<dyn agent_stream::AgentStreamSpawner>,
    /// Provider-neutral structured agent runs. Keyed by wire-side run id.
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
    /// Why its own `parking_lot::Mutex` rather than living in `poll_state`:
    /// `handle_fetch_pr_details` and the poll tick both need this client,
    /// and the cold-cache path builds it with a network `.await`. Holding
    /// `poll_state` across that build re-creates the exact serve-loop
    /// stall #91/#92 fight — and #133's `checkout_poll_state` made it
    /// worse by emptying `poll_state`'s copy for the whole tick, so every
    /// concurrent fetch hit the rebuild path. Keeping the client here
    /// means reaching it is a brief `parking_lot::Mutex` clone-out that
    /// never blocks on `poll_state` and never spans the `from_credential`
    /// await.
    pub gh_client_cache: Arc<parking_lot::Mutex<Option<lazybox_gh::GhClient>>>,
    /// Issue→PR merge-prompt dedupe memory. Deliberately separate from
    /// `poll_state`: the collapse path that touches it runs inside an
    /// `upsert`, and sharing `poll_state`'s non-reentrant
    /// `tokio::sync::Mutex` self-deadlocked when the tick held that
    /// guard across the upsert loop (#131/#132). A tick no longer holds
    /// `poll_state` across its body (#133), but `upsert` staying
    /// decoupled from it is worth keeping. See
    /// [`polling::MergePromptMemory`].
    pub merge_prompts: Arc<Mutex<polling::MergePromptMemory>>,
    /// Daemon-side "auto-merge on green" one-shot latch (the trigger
    /// moved out of the TUI client — see [`polling::auto_merge`]).
    /// Own `parking_lot::Mutex` for the same reason as `merge_prompts`:
    /// it's touched from inside the upsert/commit path and must stay
    /// decoupled from `poll_state`'s non-reentrant lock. In-memory on
    /// purpose — a restart re-evaluates and may re-attempt an
    /// idempotent merge, which GitHub answers "already merged".
    pub auto_merge: Arc<parking_lot::Mutex<polling::AutoMergeMemory>>,
    /// Workspace-removal prompt memory (level-triggered
    /// `MergedPrRemovable` re-emits + "keep" pins). Own lock for the
    /// same reason as `merge_prompts`: it's touched from inside the
    /// upsert path via `prompt_merged_pr_removal_with`. See
    /// [`polling::RemovalPromptMemory`].
    pub removal_prompts: Arc<Mutex<polling::RemovalPromptMemory>>,
    /// Authenticated user logins per provider source ("github" →
    /// "AntoineToussaint"). Populated by the polling layer when
    /// each provider client initializes; consumed by the Subscribe
    /// handler so reconnecting TUIs immediately know which authors
    /// in activity bylines are the local user (→ render as `@me`).
    /// `parking_lot::Mutex` (not tokio's) because the data is tiny
    /// and read/written from sync contexts only — no await needed.
    pub viewer_identities: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
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
    /// it. `parking_lot::Mutex` — the data is tiny and no await ever
    /// happens under the guard, which lets the drop guard release
    /// synchronously. Each claim carries a cancel `Notify`:
    /// `Command::CancelSpawn` pings it and the owning `handle_spawn`
    /// aborts its provisioning (dropping the in-flight `git` child) —
    /// how an Esc on the "Setting up workspace" checklist actually
    /// stops a wedged clone instead of letting it run on (issue #403).
    pub inflight_spawns:
        Arc<parking_lot::Mutex<HashMap<(String, String), Arc<tokio::sync::Notify>>>>,
    /// Pinged whenever an in-flight spawn claim is released, so
    /// waiters (duplicate spawns collapsing onto the winner, `Kill`
    /// waiting out a mid-flight provision) re-check promptly instead
    /// of busy-polling.
    pub inflight_spawn_changed: Arc<tokio::sync::Notify>,
    /// Workspace keys whose deletion began in this process (single delete,
    /// merged cleanup, or project cascade). Consulted both when a workspace
    /// row is missing and immediately after `backend.spawn`, so a provision
    /// that outlives the bounded delete wait is killed before registration.
    /// Failed deletes and successful unarchives remove their tombstone.
    pub deleted_workspaces: Arc<parking_lot::Mutex<HashSet<String>>>,
    /// Serializes the archive set's read-modify-write cycle across different
    /// workspace keys. The store exposes scalar KV operations, not an atomic
    /// set insert; without this lock two concurrent deletes can each load the
    /// old set and overwrite the other's tombstone.
    pub archive_updates: Arc<parking_lot::Mutex<()>>,
    /// Debounce memory for "stored workspace row is unreadable —
    /// preserved, not overwritten" reports, keyed by workspace key →
    /// last report time. The poll tick re-hits the same corrupt row
    /// every cycle; without this each ~60s tick would re-broadcast an
    /// identical storage error per corrupt row. Its own lock (not a
    /// `TickState` field) because `prepare_upsert` runs decoupled from
    /// `poll_state` — see the `MergePromptMemory` deadlock note in
    /// `polling`.
    pub undecodable_row_reports: Arc<parking_lot::Mutex<HashMap<String, std::time::Instant>>>,
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
    /// Per-workspace-key write serialization for the store's
    /// load-modify-save cycles (see [`ServerConfig::lock_workspace`]).
    /// Outer `parking_lot::Mutex` guards only the map lookup/insert and
    /// is never held across an await; the inner `tokio::sync::Mutex`
    /// is what mutation paths hold for the duration of a
    /// load→modify→commit. Entries are never removed — the map is
    /// bounded by the number of distinct workspace keys seen in this
    /// process (inbox-sized), and a `Mutex<()>` is a few dozen bytes.
    pub workspace_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl ServerConfig {
    /// Open the store at `~/.lazybox/v2/state.db`.
    ///
    /// Open failures (permissions, disk corruption) abort startup. A
    /// production process must never impersonate an empty installation
    /// when the user's persisted state is temporarily unavailable.
    pub fn from_user_config() -> Result<Self, ServerError> {
        let store = open_store()?;

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
        Ok(Self::with_store_backend_and_root(
            store,
            backend,
            WorktreeRoot::persistent(lazybox_core::paths::state_root()),
        ))
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
        Self::with_store_backend_and_root(store, backend, WorktreeRoot::scratch())
    }

    /// Like [`Self::with_store_and_backend`], but rooted at a
    /// caller-owned worktree directory instead of an unguessable
    /// scratch dir. The integration tier needs this to exercise REAL
    /// worktree provisioning: it pre-seeds `<root>/repos/<owner>/<repo>`
    /// with a bare clone of a local upstream so `provision_worktree`
    /// runs actual `git worktree add` machinery without the network.
    /// The caller owns the directory's lifetime.
    pub fn with_store_backend_and_worktree_root(
        store: Arc<dyn Store>,
        backend: Arc<dyn SessionBackend>,
        worktree_root: PathBuf,
    ) -> Self {
        Self::with_store_backend_and_root(store, backend, WorktreeRoot::persistent(worktree_root))
    }

    fn with_store_backend_and_root(
        store: Arc<dyn Store>,
        backend: Arc<dyn SessionBackend>,
        worktree_root: WorktreeRoot,
    ) -> Self {
        let (bus, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            agents: Registry::default_builtins(),
            store,
            bus,
            backend,
            worktree_root: Arc::new(worktree_root),
            terminals: Arc::new(Mutex::new(HashMap::new())),
            terminal_sessions: Arc::new(Mutex::new(HashMap::new())),
            agent_states: Arc::new(Mutex::new(HashMap::new())),
            terminal_meta: Arc::new(Mutex::new(HashMap::new())),
            no_permission_terminals: Arc::new(Mutex::new(HashSet::new())),
            on_main_terminals: Arc::new(Mutex::new(HashSet::new())),
            terminal_models: Arc::new(Mutex::new(HashMap::new())),
            outdated_agent_terminals: Arc::new(Mutex::new(HashSet::new())),
            terminal_persistence_locks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            terminal_io_locks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            agent_detect_resets: Arc::new(Mutex::new(HashSet::new())),
            hook_driven_terminals: Arc::new(Mutex::new(HashMap::new())),
            prompt_submit_signals: Arc::new(Mutex::new(HashMap::new())),
            pending_prompt_injections: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            agent_stream_spawner: Arc::new(agent_stream::ProcessAgentStreamSpawner),
            agent_runs: Arc::new(Mutex::new(HashMap::new())),
            next_agent_run_id: Arc::new(AtomicU64::new(1)),
            credential_store: Arc::new(auth::MemoryCredentialStore::new()),
            default_principal_id: lazybox_ipc::PrincipalId::local(),
            poll_state: Arc::new(Mutex::new(polling::TickState::default())),
            gh_client_cache: Arc::new(parking_lot::Mutex::new(None)),
            merge_prompts: Arc::new(Mutex::new(polling::MergePromptMemory::default())),
            auto_merge: Arc::new(parking_lot::Mutex::new(polling::AutoMergeMemory::default())),
            removal_prompts: Arc::new(Mutex::new(polling::RemovalPromptMemory::default())),
            viewer_identities: Arc::new(parking_lot::Mutex::new(Vec::new())),
            poll_wake: Arc::new(tokio::sync::Notify::new()),
            inflight_spawns: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            inflight_spawn_changed: Arc::new(tokio::sync::Notify::new()),
            deleted_workspaces: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            archive_updates: Arc::new(parking_lot::Mutex::new(())),
            undecodable_row_reports: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            input_needed_shapes: Arc::new(Mutex::new(HashMap::new())),
            event_metrics: Arc::new(metrics::EventMetrics::default()),
            workspace_locks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    /// Manager rooted in this daemon's configured filesystem namespace.
    /// All production worktree paths and all test seams must originate here.
    ///
    /// Network git operations authenticate with the same GitHub
    /// credential chain as the API pollers: the daemon usually has no
    /// usable SSH agent, so without this every base-ref refresh on an
    /// SSH-origin bare clone failed `Permission denied (publickey)`
    /// and new worktrees silently branched from a stale local main
    /// (issue #394). Resolved lazily per operation; when no token
    /// resolves, git's native (SSH) behavior is unchanged.
    /// The filesystem namespace this daemon provisions under —
    /// `repos/` (bare clones) and worktrees hang off it. Public so
    /// integration tests can pre-seed a local bare clone and drive the
    /// real provisioning path without touching the network.
    pub fn worktree_root_path(&self) -> &Path {
        &self.worktree_root.path
    }

    pub(crate) fn worktree_manager(&self) -> lazybox_git_ops::WorktreeManager {
        lazybox_git_ops::WorktreeManager::new(self.worktree_root.path.clone()).with_github_token(
            Arc::new(|| {
                Box::pin(async {
                    lazybox_gh::credential_chain()
                        .resolve(lazybox_gh::SOURCE)
                        .await
                        .ok()
                        .map(|c| c.into_token())
                })
            }),
        )
    }

    /// Serialize a workspace's load-modify-save cycle. Every mutation
    /// of a stored workspace row is an unserialized read-modify-write
    /// on the same kv record; without a lock, a poll tick's
    /// prepare→commit (which spans awaits) could interleave with a
    /// detached handler's mark-read on the serve loop's mutations
    /// JoinSet — the tick then saved its pre-mark copy and silently
    /// reverted the user's action. Hold the returned guard from
    /// BEFORE the workspace load until AFTER the commit.
    ///
    /// Per-key (not one global lock) because a tick's
    /// prepare→commit for one workspace can await the network of
    /// store scans; a global lock would queue the user's mark-read on
    /// an unrelated workspace behind it. Operations spanning multiple
    /// workspaces must use the internal `lock_workspaces` helper, which sorts
    /// and deduplicates keys before acquisition.
    pub async fn lock_workspace(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let entry = self.workspace_lock_entry(key);
        entry.lock_owned().await
    }

    fn workspace_lock_entry(&self, key: &str) -> Arc<Mutex<()>> {
        {
            let mut map = self.workspace_locks.lock();
            map.entry(key.to_string()).or_default().clone()
        }
    }

    /// Serialize a load→modify→commit that spans multiple workspace rows.
    /// Keys are sorted and deduplicated before locking, giving every caller
    /// one canonical acquisition order and preventing AB/BA deadlocks.
    /// Callers must not already hold an individual workspace guard.
    pub(crate) async fn lock_workspaces<I>(&self, keys: I) -> Vec<tokio::sync::OwnedMutexGuard<()>>
    where
        I: IntoIterator<Item = String>,
    {
        let mut keys: Vec<String> = keys.into_iter().collect();
        keys.sort_unstable();
        keys.dedup();
        let entries: Vec<_> = keys
            .iter()
            .map(|key| self.workspace_lock_entry(key))
            .collect();
        let mut guards = Vec::with_capacity(entries.len());
        for entry in entries {
            guards.push(entry.lock_owned().await);
        }
        guards
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

    /// Serialize durable prompt state with teardown for one backend session.
    /// Callers re-check terminal liveness after acquiring: teardown may have
    /// won the lock and removed the wire mapping while they waited.
    pub(crate) async fn lock_terminal_persistence(
        &self,
        backend_key: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let entry = {
            let mut locks = self.terminal_persistence_locks.lock();
            locks.entry(backend_key.to_string()).or_default().clone()
        };
        entry.lock_owned().await
    }

    /// Drop the registry entry after teardown. Existing guards/waiters keep
    /// their `Arc`; future stale writers create a fresh lock, re-check the now
    /// absent terminal mapping, and skip without resurrecting state.
    pub(crate) fn forget_terminal_persistence_lock(&self, backend_key: &str) {
        self.terminal_persistence_locks.lock().remove(backend_key);
    }

    /// Serialize every backend interaction for one terminal across all client
    /// connections and non-command producers.
    pub(crate) async fn lock_terminal_io(
        &self,
        backend_key: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let entry = {
            let mut locks = self.terminal_io_locks.lock();
            locks.entry(backend_key.to_string()).or_default().clone()
        };
        entry.lock_owned().await
    }

    /// Retire the registry entry after terminal teardown. Existing waiters
    /// keep the old `Arc`, re-check liveness, and reject stale work.
    pub(crate) fn forget_terminal_io_lock(&self, backend_key: &str) {
        self.terminal_io_locks.lock().remove(backend_key);
    }
}

pub struct Server {
    config: ServerConfig,
    /// Graceful-stop signal shared by every connection served through
    /// this instance. `None` (the default) means only client-side
    /// triggers (`Command::Shutdown`, disconnect) end the serve loop.
    /// The socket service raises it on SIGTERM so each connection runs
    /// its bounded mutation drain instead of being aborted mid-write.
    graceful_stop: Option<tokio::sync::watch::Receiver<bool>>,
    /// Bound on the exit-time drain of detached mutation tasks.
    /// [`MUTATION_DRAIN_TIMEOUT`] by default; overridable so tests can
    /// pin the abandon path without a five-second wall-clock wait.
    mutation_drain_timeout: std::time::Duration,
}

impl Server {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            graceful_stop: None,
            mutation_drain_timeout: MUTATION_DRAIN_TIMEOUT,
        }
    }

    /// Arm a graceful-stop signal: when `rx` observes `true` (or its
    /// sender drops), the serve loop stops as if the client had sent
    /// `Command::Shutdown` — including the bounded drain of in-flight
    /// detached mutations. Used by the socket service so SIGTERM
    /// doesn't abort a mutation between its remote side effect and the
    /// local save/broadcast.
    pub fn with_graceful_stop(mut self, rx: tokio::sync::watch::Receiver<bool>) -> Self {
        self.graceful_stop = Some(rx);
        self
    }

    /// Override the exit-time mutation drain bound (default
    /// [`MUTATION_DRAIN_TIMEOUT`]). Primarily for deterministic tests
    /// of the abandon path.
    pub fn with_mutation_drain_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.mutation_drain_timeout = timeout;
        self
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
        // wired one up (all production transports do; `None` is reserved for
        // explicitly finite internal harnesses).
        // The serve loop below keeps writing raw events to `conn.tx`
        // exactly as before — it never blocks on the client, so the
        // command path (keystroke `Write`s) stays responsive no matter
        // how far behind the consumer falls.
        let forward_task = conn.take_forward().map(|forward| {
            tokio::spawn(event_forward::forward_events(forward, self.config.clone()))
        });
        let mut bus_rx = self.config.bus.subscribe();
        // Detached mutation handlers (Spawn, Kill, InjectPrompt, the
        // GraphQL writers, worktree teardowns, …) land here instead of
        // a bare `tokio::spawn` so shutdown can DRAIN them: `Shutdown =>
        // break` used to abandon an in-flight Kill or Spawn mid-write.
        // Completed entries are reaped at the top of each loop turn so
        // the set doesn't accumulate results unboundedly.
        let mut mutations = tokio::task::JoinSet::new();
        // Order-sensitive terminal work runs off-loop, but never as one
        // detached task per command: dedicated routers preserve FIFO order,
        // isolate terminals, coalesce bursts, and keep SQLite drafts off the
        // PTY input lane.
        let (terminal_io_tx, terminal_io_rx) =
            tokio::sync::mpsc::channel(terminal_commands::ROUTER_CAPACITY);
        let (terminal_persist_tx, terminal_persist_rx) =
            tokio::sync::mpsc::channel(terminal_commands::ROUTER_CAPACITY);
        let io_config = self.config.clone();
        let io_event_tx = conn.tx.clone();
        mutations.spawn(async move {
            terminal_commands::run_io_router(io_config, io_event_tx, terminal_io_rx).await;
        });
        let persistence_config = self.config.clone();
        let persistence_event_tx = conn.tx.clone();
        mutations.spawn(async move {
            terminal_commands::run_persistence_router(
                persistence_config,
                persistence_event_tx,
                terminal_persist_rx,
            )
            .await;
        });
        // Subscribe is a connection-establishment command, not a general
        // snapshot endpoint. Re-running its full store/terminal snapshot can
        // manufacture an arbitrarily large structured-event backlog; live
        // recovery uses the bus-lag snapshot and RequestTerminalResync paths.
        let mut subscribed = false;
        // Cloned per connection: several serve loops can share one
        // service-owned watch channel.
        let mut graceful_stop = self.graceful_stop.clone();
        loop {
            while mutations.try_join_next().is_some() {}
            tokio::select! {
                _ = conn.tx.closed() => {
                    tracing::warn!("client event forwarder closed — ending connection");
                    break;
                }
                _ = graceful_stop_requested(&mut graceful_stop) => {
                    tracing::info!(
                        "graceful stop requested — ending connection and draining mutations"
                    );
                    break;
                }
                cmd = conn.rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    if matches!(&cmd, lazybox_ipc::Command::Subscribe) {
                        if subscribed {
                            let _ = conn.tx.send(Event::CommandRejected {
                                command: "Subscribe".into(),
                                message: "this connection is already subscribed".into(),
                            });
                            continue;
                        }
                        subscribed = true;
                    }
                    // Per-command name at INFO so a stalled IPC channel is
                    // visible at a glance — historically we'd see `daemon
                    // ← Subscribe` and then nothing, with no way to tell
                    // whether subsequent commands were arriving at all.
                    // `Write` floods on every keystroke; trim its payload
                    // but still emit one line per command so the cadence
                    // is observable.
                    let label = match &cmd {
                        lazybox_ipc::Command::Spawn { .. } => "Spawn",
                        lazybox_ipc::Command::CancelSpawn { .. } => "CancelSpawn",
                        lazybox_ipc::Command::Close { .. } => "Close",
                        lazybox_ipc::Command::IngestHook { .. } => "IngestHook",
                        lazybox_ipc::Command::CreateSession { .. } => "CreateSession",
                        lazybox_ipc::Command::Subscribe => "Subscribe",
                        lazybox_ipc::Command::Refresh => "Refresh",
                        lazybox_ipc::Command::Write { .. } => "Write",
                        lazybox_ipc::Command::RecordUserMessage { .. } => "RecordUserMessage",
                        lazybox_ipc::Command::RecordComposingBuffer { .. } => {
                            "RecordComposingBuffer"
                        }
                        lazybox_ipc::Command::Resize { .. } => "Resize",
                        lazybox_ipc::Command::RequestTerminalResync { .. } => {
                            "RequestTerminalResync"
                        }
                        lazybox_ipc::Command::InjectPrompt { .. } => "InjectPrompt",
                        lazybox_ipc::Command::MarkRead { .. } => "MarkRead",
                        lazybox_ipc::Command::FocusWorkspace { .. } => "FocusWorkspace",
                        lazybox_ipc::Command::MarkActivityRead { .. } => "MarkActivityRead",
                        lazybox_ipc::Command::UnmarkActivityRead { .. } => "UnmarkActivityRead",
                        lazybox_ipc::Command::FetchPrDetails { .. } => "FetchPrDetails",
                        lazybox_ipc::Command::SyncWorkspace { .. } => "SyncWorkspace",
                        lazybox_ipc::Command::PostReply { .. } => "PostReply",
                        lazybox_ipc::Command::MergePr { .. } => "MergePr",
                        lazybox_ipc::Command::UpdateBranch { .. } => "UpdateBranch",
                        lazybox_ipc::Command::CloseIssue { .. } => "CloseIssue",
                        lazybox_ipc::Command::DeleteOrClose { .. } => "DeleteOrClose",
                        lazybox_ipc::Command::ConfirmMerge { .. } => "ConfirmMerge",
                        lazybox_ipc::Command::Snooze { .. } => "Snooze",
                        lazybox_ipc::Command::Unsnooze { .. } => "Unsnooze",
                        lazybox_ipc::Command::SetAutoMergeOnGreen { .. } => "SetAutoMergeOnGreen",
                        lazybox_ipc::Command::SetTrackMain { .. } => "SetTrackMain",
                        lazybox_ipc::Command::SetAutoFixPolicy { .. } => "SetAutoFixPolicy",
                        lazybox_ipc::Command::Kill { .. } => "Kill",
                        lazybox_ipc::Command::RemoveMergedWorkspace { .. } => "RemoveMergedWorkspace",
                        lazybox_ipc::Command::KeepMergedWorkspace { .. } => "KeepMergedWorkspace",
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
                        lazybox_ipc::Command::ScanCheckouts { .. } => "ScanCheckouts",
                        lazybox_ipc::Command::ImportLocalCheckout { .. } => "ImportLocalCheckout",
                        lazybox_ipc::Command::DeleteOrphanedWorktree { .. } => "DeleteOrphanedWorktree",
                        lazybox_ipc::Command::FetchScrollback { .. } => "FetchScrollback",
                        lazybox_ipc::Command::CheckAgentCliUpdates => "CheckAgentCliUpdates",
                        lazybox_ipc::Command::UpdateAgentClis => "UpdateAgentClis",
                        lazybox_ipc::Command::SetNotes { .. } => "SetNotes",
                        lazybox_ipc::Command::RecordSentSnippet { .. } => "RecordSentSnippet",
                        lazybox_ipc::Command::RecordRecentSnippet { .. } => "RecordRecentSnippet",
                        lazybox_ipc::Command::SetUpdateDismissal { .. } => "SetUpdateDismissal",
                        lazybox_ipc::Command::Shutdown => "Shutdown",
                    };
                    // `Write` fires on every keystroke — at info it floods
                    // the log and makes real lifecycle lines unreadable.
                    if matches!(cmd, lazybox_ipc::Command::Write { .. }) {
                        tracing::debug!("daemon ← {label}");
                    } else {
                        tracing::info!("daemon ← {label}");
                    }
                    // No-blocking architecture (#206). Every command is
                    // dispatched through one lane decided structurally by
                    // `command_lane`, never by the arm body:
                    //   * `Shutdown` is loop control (handled below).
                    //   * the INLINE lane is a small allow-list of
                    //     provably-fast, order-sensitive handlers
                    //     (`Subscribe` only).
                    //   * terminal I/O/persistence use dedicated ordered
                    //     background lanes (never the serve-loop task).
                    //   * EVERYTHING else — and any command added later,
                    //     which falls through to `_ => Detached` — runs on
                    //     the `mutations` JoinSet, so it physically cannot
                    //     hold the `select!`. A detached handler that
                    //     stalls (network, git, a cold lock) cannot stall
                    //     keystrokes or bus/poll forwarding — exactly the
                    //     "frozen while syncing" bug class (#34, #206).
                    if matches!(cmd, lazybox_ipc::Command::Shutdown) {
                        break;
                    }
                    let cmd_started = std::time::Instant::now();
                    match command_lane(&cmd) {
                        CommandLane::Inline => {
                            dispatch_command(&self.config, &conn.tx, cmd).await;
                            // Watchdog: an inline handler must return to
                            // `select!` in microseconds. Past the budget it
                            // wedged the loop — record the violation
                            // (surfaced on `/v1/metrics`, asserted in the
                            // serve-loop tests) and name the offender.
                            let elapsed = cmd_started.elapsed();
                            if elapsed >= INLINE_BUDGET {
                                self.config.event_metrics.record_inline_budget_violation();
                                tracing::warn!(
                                    command = label,
                                    ms = elapsed.as_millis(),
                                    "INLINE command exceeded budget — serve loop BLOCKED; \
                                     keystroke Writes and bus/poll forwarding stalled this long"
                                );
                            } else {
                                tracing::debug!(
                                    command = label,
                                    ms = elapsed.as_millis(),
                                    "daemon → handled inline"
                                );
                            }
                        }
                        CommandLane::TerminalIo => {
                            if let Err(error) = terminal_io_tx.try_send(cmd) {
                                let command = error.into_inner();
                                tracing::warn!(?command, "terminal I/O router rejected command");
                                terminal_commands::reject_command(
                                    &conn.tx,
                                    &command,
                                    "terminal I/O router is full or unavailable",
                                );
                            }
                        }
                        CommandLane::TerminalPersistence => {
                            if let Err(error) = terminal_persist_tx.try_send(cmd) {
                                let command = error.into_inner();
                                tracing::warn!(?command, "terminal persistence router rejected command");
                                terminal_commands::reject_command(
                                    &conn.tx,
                                    &command,
                                    "terminal persistence router is full or unavailable",
                                );
                            }
                        }
                        CommandLane::Detached => {
                            if detached_mutation_capacity(&mutations) {
                                let cfg = self.config.clone();
                                let tx = conn.tx.clone();
                                mutations.spawn(async move {
                                    dispatch_command(&cfg, &tx, cmd).await;
                                });
                            } else {
                                tracing::warn!(
                                    max = MAX_CONNECTION_MUTATIONS,
                                    command = label,
                                    "connection mutation limit reached — rejecting command"
                                );
                                terminal_commands::reject_command(
                                    &conn.tx,
                                    &cmd,
                                    "too many commands are still in progress",
                                );
                            }
                        }
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
                                (
                                    load_workspaces(&*store),
                                    load_projects(&*store),
                                    client_kv::snapshot(&*store),
                                )
                            })
                            .await
                            {
                                Ok((workspaces, projects, client_kv)) => {
                                    let load_errors =
                                        workspaces.errors.len() + projects.errors.len();
                                    let mut terminals =
                                        spawn_handler::snapshot_terminals(&self.config).await;
                                    budget_snapshot_replay(&mut terminals);
                                    let _ = conn.tx.send(Event::Snapshot {
                                        workspaces: workspaces.values,
                                        terminals,
                                        projects: projects.values,
                                        recent_snippets: client_kv.recent_snippets,
                                        dismissed_updates: client_kv.dismissed_updates,
                                    });
                                    if load_errors > 0 {
                                        let _ = conn.tx.send(storage_recovery_event(load_errors));
                                    }
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
        drop(terminal_io_tx);
        drop(terminal_persist_tx);
        // Drain detached mutation tasks before returning — `Shutdown =>
        // break` used to abandon an in-flight Kill / Spawn / inject
        // mid-write. Bounded: a wedged clone or git op must not hold
        // shutdown hostage, so anything still running after
        // `mutation_drain_timeout` is abandoned (with a breadcrumb).
        if !mutations.is_empty() {
            let drain = async { while mutations.join_next().await.is_some() {} };
            if tokio::time::timeout(self.mutation_drain_timeout, drain)
                .await
                .is_err()
            {
                tracing::warn!(
                    drain_timeout = ?self.mutation_drain_timeout,
                    "shutdown: detached mutation task(s) still running past the drain bound — \
                     abandoning them"
                );
            }
        }
        if let Some(forward_task) = forward_task {
            // A healthy forwarder normally ends when its client receiver
            // closes. On explicit Shutdown that receiver may still be alive,
            // so abort and join the connection-owned task instead of leaving
            // it detached behind `serve`.
            forward_task.abort();
            let _ = forward_task.await;
        }
        Ok(())
    }
}

/// Resolve once a graceful stop has been requested: `true` observed on
/// the watch channel, or its sender (the socket service) gone. Pends
/// forever when the server has no graceful-stop channel armed. Watch
/// semantics are level-triggered, so recreating this future on every
/// serve-loop turn cannot lose the signal.
async fn graceful_stop_requested(rx: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    match rx {
        // Err means the sender dropped — the service side is going
        // away, which is a stop request too.
        Some(rx) => {
            let _ = rx.wait_for(|stop| *stop).await;
        }
        None => std::future::pending().await,
    }
}

/// Which lane a command runs in. See [`command_lane`].
enum CommandLane {
    /// Runs on the serve-loop task itself. Reserved for provably-fast,
    /// order-sensitive handlers; held to [`INLINE_BUDGET`] by a watchdog.
    Inline,
    /// Per-terminal FIFO input/resize/close/injection lane. Runs outside the
    /// serve loop.
    TerminalIo,
    /// Per-terminal FIFO durable prompt-state lane. Runs outside the serve
    /// loop and independently from PTY input.
    TerminalPersistence,
    /// Runs on the serve loop's `mutations` JoinSet so it can never hold
    /// the `select!`, no matter how slow the handler gets.
    Detached,
}

/// Decide a command's lane structurally — the single enforcement point
/// for the no-blocking guarantee (#206). The inline allow-list is tiny
/// and explicit; everything else, INCLUDING any command added later,
/// falls through to `Detached`, so the safe default is "cannot wedge the
/// loop." A handler earns the inline lane only if it is local, bounded,
/// and order-sensitive (keystroke ordering or snapshot sequencing) —
/// never if it can touch the network, git, the filesystem, or a
/// contended lock.
fn command_lane(cmd: &lazybox_ipc::Command) -> CommandLane {
    use lazybox_ipc::Command;
    match cmd {
        Command::Write { .. }
        | Command::Resize { .. }
        | Command::Close { .. }
        | Command::InjectPrompt { .. } => CommandLane::TerminalIo,
        Command::RecordUserMessage { .. } | Command::RecordComposingBuffer { .. } => {
            CommandLane::TerminalPersistence
        }
        Command::Subscribe => CommandLane::Inline,
        _ => CommandLane::Detached,
    }
}

fn detached_mutation_capacity(mutations: &tokio::task::JoinSet<()>) -> bool {
    mutations.len().saturating_sub(CONNECTION_ROUTER_TASKS) < MAX_CONNECTION_MUTATIONS
}

#[cfg(test)]
mod mutation_admission_tests {
    use super::*;

    #[tokio::test]
    async fn pending_connection_mutations_have_a_hard_task_cap() {
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..(CONNECTION_ROUTER_TASKS + MAX_CONNECTION_MUTATIONS - 1) {
            tasks.spawn(std::future::pending::<()>());
        }
        assert!(detached_mutation_capacity(&tasks));

        tasks.spawn(std::future::pending::<()>());
        assert!(!detached_mutation_capacity(&tasks));
        assert_eq!(
            tasks.len(),
            CONNECTION_ROUTER_TASKS + MAX_CONNECTION_MUTATIONS
        );

        tasks.shutdown().await;
    }
}

/// Run one command to completion. The serve loop calls this two ways
/// (see [`command_lane`]): INLINE for the fast, order-sensitive lane, or
/// on the `mutations` JoinSet for the detached lane. The body is
/// lane-agnostic — it just `.await`s the matching handler — so the
/// inline-vs-detached decision lives in exactly one place
/// (`command_lane`) instead of being re-litigated per arm. `Shutdown` is
/// loop control and never reaches here.
/// Execute one IPC command handler to completion.
///
/// Public for the JSON gateway's synchronous one-shot endpoint and its
/// integration harness. Normal clients should continue using an IPC transport.
#[doc(hidden)]
pub async fn dispatch_command(
    config: &ServerConfig,
    tx: &lazybox_ipc::EventSender,
    cmd: lazybox_ipc::Command,
) {
    match cmd {
        lazybox_ipc::Command::Subscribe => {
            // Offload the SQLite scans (issue #34) onto `spawn_blocking`
            // so the inline handler doesn't pin the runtime worker on the
            // parking_lot mutex + row iteration. Both scans share one
            // task to pay the spawn/handoff once. A panic inside (for example,
            // corrupt JSON) is logged loudly — silently sending an
            // empty Snapshot would render a blank sidebar with no
            // breadcrumb.
            let store = config.store.clone();
            let (workspaces, projects, client_kv) = match tokio::task::spawn_blocking(move || {
                (
                    load_workspaces(&*store),
                    load_projects(&*store),
                    client_kv::snapshot(&*store),
                )
            })
            .await
            {
                Ok(triple) => triple,
                Err(e) => {
                    tracing::error!(
                        "Subscribe snapshot load task failed: {e} — sending empty snapshot",
                    );
                    (
                        LoadOutcome::default(),
                        LoadOutcome::default(),
                        client_kv::ClientKvSnapshot::default(),
                    )
                }
            };
            let load_errors = workspaces.errors.len() + projects.errors.len();
            let mut terminals = spawn_handler::snapshot_terminals(config).await;
            budget_snapshot_replay(&mut terminals);
            // Derive compatibility warnings from the exact snapshot this
            // subscriber receives. Terminal teardown removes the primary
            // mapping before its auxiliary launch-generation marker; counting
            // the marker set independently could therefore warn about a
            // terminal absent from this snapshot.
            let restart_required = {
                let outdated = config.outdated_agent_terminals.lock().await;
                terminals
                    .iter()
                    .filter_map(|terminal| {
                        outdated
                            .contains(&terminal.terminal_id)
                            .then_some(terminal.terminal_id)
                    })
                    .collect::<Vec<_>>()
            };
            let _ = tx.send(Event::Snapshot {
                workspaces: workspaces.values,
                terminals,
                projects: projects.values,
                recent_snippets: client_kv.recent_snippets,
                dismissed_updates: client_kv.dismissed_updates,
            });
            if load_errors > 0 {
                let _ = tx.send(storage_recovery_event(load_errors));
            }
            if !restart_required.is_empty() {
                let _ = tx.send(Event::RecoveredTerminalsRequireRestart {
                    terminal_ids: restart_required,
                });
            }
            // A fresh subscriber may have missed removal prompts emitted
            // before it connected (broadcast is fire-and-forget) — reset
            // the reprompt throttle so the tick kicked below re-offers
            // any still-unresolved merged/closed workspace right away.
            polling::mark_removal_prompts_for_replay(config).await;
            // Kick a fresh poll so the freshly-opened TUI refreshes within
            // a few seconds instead of waiting out the current sleep.
            config.poll_wake.notify_one();
            // Replay cached viewer identities so a reconnecting TUI can
            // render `@me` without waiting for the next poll cycle.
            let logins = config.viewer_identities.lock().clone();
            if !logins.is_empty() {
                let _ = tx.send(Event::ViewerIdentities { logins });
            }
            // Last of the post-subscribe pushes: the daemon's authoritative
            // auto-fix policy config, so the policies menu (`g p`) reflects
            // what *this daemon* would do, not the client's own local config
            // — the two differ in `--connect` remote mode (tracker #512).
            // Loaded fresh from the daemon's config file (the poller reads
            // it the same way each tick); a missing/broken config reads as
            // the off-by-default settings. Emitted after the snapshot +
            // recovery/identity pushes so it never interposes in their
            // order.
            let auto_fix = lazybox_config::Config::load()
                .map(|c| c.auto_fix.to_settings())
                .unwrap_or_default();
            let _ = tx.send(Event::AutoFixPolicyConfig {
                enabled: auto_fix.enabled,
                opt_out_labels: auto_fix.opt_out_labels,
            });
        }
        lazybox_ipc::Command::Spawn {
            session_key,
            session_id,
            kind,
            cwd,
            initial_prompt,
            on_main,
            model_alias,
        } => {
            // A spawn carrying a pre-built work prompt is an autonomous
            // "work on this" launch — run it unattended (skip permissions,
            // subject to the `autonomous_skip_permissions` toggle) so it
            // doesn't stall on the folder-trust / tool-approval gate. Bare
            // interactive spawns carry no prompt and keep the
            // human-in-the-loop approval.
            let autonomous = spawn_handler::spawn_is_autonomous(&initial_prompt);
            spawn_handler::handle_spawn(
                config,
                session_key,
                session_id,
                kind,
                cwd,
                initial_prompt,
                autonomous,
                on_main,
                model_alias,
                false,
            )
            .await;
        }
        lazybox_ipc::Command::CancelSpawn { session_key } => {
            spawn_handler::handle_cancel_spawn(config, &session_key);
        }
        lazybox_ipc::Command::CreateSession {
            session_key,
            kind,
            label,
        } => {
            spawn_handler::handle_create_session(config, session_key, kind, label).await;
        }
        lazybox_ipc::Command::Write { terminal_id, bytes } => {
            spawn_handler::handle_write(config, terminal_id, &bytes).await;
        }
        lazybox_ipc::Command::RecordUserMessage {
            terminal_id,
            prompt,
        } => {
            spawn_handler::handle_record_user_message(config, terminal_id, &prompt).await;
        }
        lazybox_ipc::Command::InjectPrompt {
            terminal_id,
            prompt,
            fallback_spawn,
            submit,
        } => {
            spawn_handler::handle_inject_prompt(
                config,
                terminal_id,
                &prompt,
                fallback_spawn,
                submit,
            )
            .await;
        }
        lazybox_ipc::Command::RecordComposingBuffer {
            terminal_id,
            buffer,
        } => {
            spawn_handler::handle_record_composing_buffer(config, terminal_id, &buffer).await;
        }
        lazybox_ipc::Command::RequestTerminalResync {
            terminal_id,
            required_seq,
        } => {
            spawn_handler::handle_terminal_resync_request(config, tx, terminal_id, required_seq)
                .await;
        }
        lazybox_ipc::Command::Resize {
            terminal_id,
            cols,
            rows,
        } => {
            spawn_handler::handle_resize(config, terminal_id, cols, rows).await;
        }
        lazybox_ipc::Command::Close { terminal_id } => {
            spawn_handler::handle_close(config, terminal_id).await;
        }
        lazybox_ipc::Command::FetchScrollback { terminal_id } => {
            spawn_handler::handle_fetch_scrollback(config, tx, terminal_id).await;
        }
        lazybox_ipc::Command::IngestHook {
            terminal_id,
            hook,
            backend_key,
        } => {
            spawn_handler::handle_ingest_hook(config, terminal_id, backend_key, hook).await;
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
                config,
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
            agent_runs::handle_send_agent_input(config, run_id, message).await;
        }
        lazybox_ipc::Command::InterruptAgentRun { run_id } => {
            agent_runs::handle_interrupt_agent_run(config, run_id).await;
        }
        lazybox_ipc::Command::DecideAgentApproval {
            run_id,
            request_id,
            decision,
        } => {
            agent_runs::handle_decide_agent_approval(config, run_id, request_id, decision).await;
        }
        lazybox_ipc::Command::AnswerAgentQuestion {
            run_id,
            question_id,
            answer,
        } => {
            agent_runs::handle_answer_agent_question(config, run_id, question_id, answer).await;
        }
        lazybox_ipc::Command::UpsertProviderCredential {
            principal_id,
            credential,
        } => {
            auth::handle_upsert_provider_credential(config, tx, principal_id, credential).await;
        }
        lazybox_ipc::Command::RemoveProviderCredential {
            principal_id,
            provider_id,
        } => {
            auth::handle_remove_provider_credential(config, tx, principal_id, provider_id).await;
        }
        lazybox_ipc::Command::ListProviderCredentials { principal_id } => {
            auth::handle_list_provider_credentials(config, tx, principal_id).await;
        }
        lazybox_ipc::Command::MarkRead { session_key } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            // MarkRead is the user's "I just looked at this" signal —
            // treat it as a focus hint too so the round-robin sync cursor
            // bumps even when the TUI doesn't fire a separate
            // `FocusWorkspace`.
            polling::set_focused_workspace(config, &key).await;
            polling::mark_workspace_read(config, &key).await;
        }
        lazybox_ipc::Command::FocusWorkspace { session_key } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::set_focused_workspace(config, &key).await;
        }
        lazybox_ipc::Command::MarkActivityRead {
            session_key,
            index,
            fingerprint,
        } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::mark_activity_read(config, &key, index, fingerprint.as_ref()).await;
        }
        lazybox_ipc::Command::UnmarkActivityRead { session_key, index } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::unmark_activity_read(config, &key, index).await;
        }
        lazybox_ipc::Command::CreateWorkspace {
            name,
            project_key,
            spawn_agent,
        } => {
            // create_empty_workspace returns the final key — which may
            // carry a `-2` collision suffix the client can't predict — so
            // chain any requested spawn off it here rather than
            // round-tripping through the client. Bare interactive spawn
            // (no prompt) keeps the human-in-the-loop approval gate.
            let key = polling::create_empty_workspace(config, &name, project_key);
            if let Some(agent_id) = spawn_agent {
                let session_key: lazybox_core::SessionKey = (&key).into();
                spawn_handler::handle_spawn(
                    config,
                    session_key,
                    None,
                    lazybox_ipc::TerminalKind::Agent(agent_id),
                    None,
                    None,
                    false,
                    false,
                    None,
                    false,
                )
                .await;
            }
        }
        lazybox_ipc::Command::CreateProject { name } => {
            polling::create_local_project(config, &name);
        }
        lazybox_ipc::Command::Snooze { session_key, until } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::set_snooze(config, &key, Some(until)).await;
        }
        lazybox_ipc::Command::Unsnooze { session_key } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::set_snooze(config, &key, None).await;
        }
        lazybox_ipc::Command::SetNotes { session_key, notes } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::set_notes(config, &key, notes).await;
        }
        lazybox_ipc::Command::RecordSentSnippet {
            session_key,
            snippet_key,
        } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::record_sent_snippet(config, &key, snippet_key).await;
        }
        lazybox_ipc::Command::RecordRecentSnippet { key } => {
            client_kv::record_recent_snippet(config, key).await;
        }
        lazybox_ipc::Command::SetUpdateDismissal { target } => {
            client_kv::set_update_dismissal(config, target).await;
        }
        lazybox_ipc::Command::SetAutoMergeOnGreen {
            session_key,
            enabled,
        } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::set_auto_merge_on_green(config, &key, enabled).await;
        }
        lazybox_ipc::Command::SetTrackMain {
            session_key,
            enabled,
        } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::set_track_main(config, &key, enabled).await;
        }
        lazybox_ipc::Command::SetAutoFixPolicy {
            session_key,
            kind,
            arm,
        } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::set_auto_fix_policy(config, &key, kind, arm).await;
        }
        lazybox_ipc::Command::Kill { session_key } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            let _ = polling::delete_workspace(config, &key).await;
        }
        lazybox_ipc::Command::KeepMergedWorkspace { session_key } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::keep_merged_workspace(config, &key).await;
        }
        lazybox_ipc::Command::RemoveMergedWorkspace { session_key } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            polling::remove_merged_workspace(config, &key).await;
        }
        lazybox_ipc::Command::DeleteProject { project_key } => {
            polling::delete_project(config, &project_key).await;
        }
        lazybox_ipc::Command::CollapseIntoPr {
            issue_workspace_key,
        } => {
            let key = lazybox_core::WorkspaceKey::new(issue_workspace_key.as_str().to_string());
            polling::handle_collapse_into_pr(config, key).await;
        }
        lazybox_ipc::Command::Refresh => {
            // Manual poll trigger. Force a full sweep so a just-created
            // issue appears now instead of next scheduled sweep (issue
            // #180), then wake the long-lived poll loop — the single
            // source of truth for ticks.
            if let Some(client) = config.gh_client_cache.lock().as_ref() {
                client.force_full_sweep();
            }
            config.poll_wake.notify_one();
        }
        lazybox_ipc::Command::PostReply { session_key, body } => {
            polling::post_reply(config, session_key, body).await;
        }
        lazybox_ipc::Command::SetSessionLayout {
            session_key,
            session_id_raw,
            layout_json,
        } => {
            let key = lazybox_core::WorkspaceKey::new(session_key.as_str().to_string());
            let session_id = uuid::Uuid::parse_str(&session_id_raw)
                .ok()
                .map(lazybox_core::SessionId);
            let layout: Option<lazybox_core::SessionLayout> =
                serde_json::from_str(&layout_json).ok();
            if let (Some(sid), Some(lay)) = (session_id, layout) {
                polling::set_session_layout(config, &key, sid, lay).await;
            } else {
                tracing::warn!("SetSessionLayout: bad payload (id={:?})", session_id_raw);
            }
        }
        lazybox_ipc::Command::ConfirmMerge {
            issue_workspace_key,
            pr_workspace_key,
            accept,
        } => {
            polling::handle_confirm_merge(config, issue_workspace_key, pr_workspace_key, accept)
                .await;
        }
        lazybox_ipc::Command::AdoptSessions {
            source_workspace_key,
            target_workspace_key,
        } => {
            polling::handle_adopt_sessions(config, source_workspace_key, target_workspace_key)
                .await;
        }
        lazybox_ipc::Command::MergePr { workspace_key } => {
            polling::handle_merge_pr(config, workspace_key).await;
        }
        lazybox_ipc::Command::UpdateBranch { workspace_key } => {
            polling::handle_update_branch(config, workspace_key).await;
        }
        lazybox_ipc::Command::CloseIssue { workspace_key } => {
            polling::handle_close_issue(config, workspace_key).await;
        }
        lazybox_ipc::Command::DeleteOrClose { workspace_key } => {
            polling::handle_delete_or_close(config, workspace_key).await;
        }
        lazybox_ipc::Command::FetchPrDetails { workspace_key } => {
            polling::handle_fetch_pr_details(config, workspace_key).await;
        }
        lazybox_ipc::Command::SyncWorkspace { workspace_key } => {
            polling::handle_sync_workspace(config, workspace_key).await;
        }
        lazybox_ipc::Command::RequestReviewers {
            workspace_key,
            logins,
        } => {
            polling::handle_request_reviewers(config, workspace_key, logins).await;
        }
        lazybox_ipc::Command::AddAssignees {
            workspace_key,
            logins,
        } => {
            polling::handle_add_assignees(config, workspace_key, logins).await;
        }
        lazybox_ipc::Command::SetAssignees {
            workspace_key,
            logins,
        } => {
            polling::handle_set_assignees(config, workspace_key, logins).await;
        }
        lazybox_ipc::Command::SetLabels {
            workspace_key,
            names,
        } => {
            polling::handle_set_labels(config, workspace_key, names).await;
        }
        lazybox_ipc::Command::FetchRepoLabels { workspace_key } => {
            polling::handle_fetch_repo_labels(config, workspace_key).await;
        }
        lazybox_ipc::Command::CleanWorktrees => {
            polling::handle_clean_worktrees(config).await;
        }
        lazybox_ipc::Command::InspectWorktrees => {
            polling::handle_inspect_worktrees(config).await;
        }
        lazybox_ipc::Command::ScanCheckouts { roots } => {
            polling::handle_scan_checkouts(config, roots).await;
        }
        lazybox_ipc::Command::ImportLocalCheckout { path, spawn_agent } => {
            let key = polling::import_local_checkout(config, path).await;
            if let (Some(key), Some(agent_id)) = (key, spawn_agent) {
                let session_key: lazybox_core::SessionKey = (&key).into();
                spawn_handler::handle_spawn(
                    config,
                    session_key,
                    None,
                    lazybox_ipc::TerminalKind::Agent(agent_id),
                    None,
                    None,
                    false,
                    false,
                    None,
                    false,
                )
                .await;
            }
        }
        lazybox_ipc::Command::DeleteOrphanedWorktree { path, force } => {
            polling::handle_delete_orphaned_worktree(config, path, force).await;
        }
        lazybox_ipc::Command::CheckAgentCliUpdates => {
            agent_updates::handle_check(config, true).await;
        }
        lazybox_ipc::Command::UpdateAgentClis => {
            agent_updates::handle_update_all(config);
        }
        lazybox_ipc::Command::Shutdown => {
            unreachable!("Shutdown is loop control, intercepted by the serve loop")
        }
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

struct LoadOutcome<T> {
    values: Vec<T>,
    errors: Vec<String>,
}

impl<T> Default for LoadOutcome<T> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            errors: Vec::new(),
        }
    }
}

fn storage_recovery_event(skipped: usize) -> Event {
    Event::provider_error_permanent(
        "storage",
        format!(
            "{skipped} persisted record(s) could not be loaded. They were preserved, not deleted. \
             Back up the state directory and follow the recovery guide."
        ),
    )
}

/// Deserialize every persisted `Workspace`. Bad JSON is preserved in the
/// store and reported to the client instead of silently disappearing.
fn load_workspaces(store: &dyn Store) -> LoadOutcome<lazybox_core::Workspace> {
    let records = match store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("list_workspaces failed: {e}");
            return LoadOutcome {
                values: Vec::new(),
                errors: vec![format!("list workspaces: {e}")],
            };
        }
    };
    let mut outcome = LoadOutcome::default();
    for record in records {
        match record.workspace_json {
            // `decode_persisted`, not a lenient `from_str`: a row
            // stamped with a NEWER schema version (downgraded build)
            // must land in the preserve-and-report branch too, or a
            // later rewrite would silently drop the newer fields.
            Some(json) => match lazybox_core::Workspace::decode_persisted(&json) {
                Ok(workspace) => outcome.values.push(workspace),
                Err(error) => {
                    tracing::warn!("preserving unreadable workspace {}: {error}", record.key);
                    outcome
                        .errors
                        .push(format!("workspace {}: {error}", record.key));
                }
            },
            None => {
                tracing::warn!("preserving workspace {} with no JSON payload", record.key);
                outcome
                    .errors
                    .push(format!("workspace {}: missing JSON payload", record.key));
            }
        }
    }
    outcome
}

/// Cap on the TOTAL terminal-replay bytes embedded in one `Snapshot`.
///
/// A snapshot carries each terminal's full replay ring (up to
/// [`pty::REPLAY_RING_BYTES`] = 2 MiB each); with enough busy
/// terminals the serialized frame crossed the socket transport's
/// `MAX_FRAME_BYTES` (64 MiB), `write_frame` returned `TooLarge`, the
/// writer loop died, and the reconnecting `--connect` client looped
/// re-requesting the same unsendable snapshot forever. Half the frame
/// cap leaves ample headroom for the workspaces/projects JSON and
/// bincode overhead riding in the same frame.
const SNAPSHOT_REPLAY_BUDGET: usize = (lazybox_ipc::MAX_FRAME_BYTES as usize) / 2;

/// Keep whole authoritative replay buffers within
/// [`SNAPSHOT_REPLAY_BUDGET`]. A replay is atomic: slicing its prefix can
/// begin inside UTF-8/CSI state and cannot rebuild a fresh VT parser. We
/// therefore retain as many complete replays as fit (smallest first) and
/// mark the rest unavailable; a live client preserves its last coherent
/// grid instead of accepting a corrupt partial reset.
fn budget_snapshot_replay(terminals: &mut [lazybox_ipc::TerminalSnapshot]) {
    // Enforce the representation invariant even when the available
    // replays are under budget: unavailable must never carry a byte tail
    // a consumer could accidentally feed into a fresh parser.
    for terminal in terminals.iter_mut().filter(|t| !t.replay_available) {
        terminal.replay.clear();
    }
    let total: usize = terminals
        .iter()
        .filter(|t| t.replay_available)
        .map(|t| t.replay.len())
        .sum();
    if total <= SNAPSHOT_REPLAY_BUDGET {
        return;
    }
    tracing::warn!(
        terminals = terminals.len(),
        total_replay_bytes = total,
        budget = SNAPSHOT_REPLAY_BUDGET,
        "snapshot replay exceeds budget — omitting whole replays"
    );
    // Smallest-first maximizes the number of terminals that receive a
    // usable baseline. Ties retain stable terminal order.
    let mut order: Vec<usize> = (0..terminals.len()).collect();
    order.sort_by_key(|&i| terminals[i].replay.len());
    let mut used = 0usize;
    for idx in order {
        if !terminals[idx].replay_available {
            terminals[idx].replay.clear();
            continue;
        }
        let len = terminals[idx].replay.len();
        if used.saturating_add(len) <= SNAPSHOT_REPLAY_BUDGET {
            used += len;
        } else {
            terminals[idx].replay.clear();
            terminals[idx].replay_available = false;
        }
    }
}

/// Same shape as `load_workspaces` for the project table.
fn load_projects(store: &dyn Store) -> LoadOutcome<lazybox_core::Project> {
    let records = match store.list_projects() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("list_projects failed: {e}");
            return LoadOutcome {
                values: Vec::new(),
                errors: vec![format!("list projects: {e}")],
            };
        }
    };
    let mut outcome = LoadOutcome::default();
    for record in records {
        match record.project_json {
            Some(json) => match serde_json::from_str::<lazybox_core::Project>(&json) {
                Ok(project) => outcome.values.push(project),
                Err(error) => {
                    tracing::warn!("preserving unreadable project {}: {error}", record.key);
                    outcome
                        .errors
                        .push(format!("project {}: {error}", record.key));
                }
            },
            None => {
                tracing::warn!("preserving project {} with no JSON payload", record.key);
                outcome
                    .errors
                    .push(format!("project {}: missing JSON payload", record.key));
            }
        }
    }
    outcome
}

#[cfg(test)]
mod store_open_tests {
    use super::*;

    #[test]
    fn persistent_store_open_failure_is_returned() {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = temp.path().join("state.db");
        std::fs::create_dir(&database).expect("directory at database path");

        let error = match open_store_at(&database) {
            Ok(_) => panic!("a directory cannot be opened as a SQLite database"),
            Err(error) => error,
        };
        assert!(matches!(error, ServerError::Store(_)));
        assert!(error.to_string().contains("open persistent state"));
        assert!(error.to_string().contains(&database.display().to_string()));
    }

    #[test]
    fn persistent_store_parent_creation_failure_is_returned() {
        let temp = tempfile::tempdir().expect("tempdir");
        let blocked_parent = temp.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"file").expect("blocking file");
        let database = blocked_parent.join("state.db");

        let error = match open_store_at(&database) {
            Ok(_) => panic!("database parent cannot be a regular file"),
            Err(error) => error,
        };
        assert!(matches!(error, ServerError::Store(_)));
        assert!(error.to_string().contains("create state directory"));
        assert!(
            error
                .to_string()
                .contains(&blocked_parent.display().to_string())
        );
    }
}

#[cfg(test)]
mod snapshot_budget_tests {
    use super::*;

    fn full_ring_snapshot(i: u64) -> lazybox_ipc::TerminalSnapshot {
        lazybox_ipc::TerminalSnapshot {
            terminal_id: lazybox_ipc::TerminalId(i),
            session_key: lazybox_core::SessionKey::from("ws"),
            kind: lazybox_ipc::TerminalKind::Shell,
            // Full replay ring with a recognizable tail so tests can
            // verify trimming kept the NEWEST bytes.
            replay: (0..pty::REPLAY_RING_BYTES)
                .map(|b| (b % 251) as u8)
                .collect(),
            last_seq: 42,
            replay_available: true,
            no_permission: false,
            on_main: false,
            model_label: None,
            prompt_history: Vec::new(),
            composing_buffer: None,
        }
    }

    /// Regression: ~40 busy terminals × 2 MiB replay rings used to
    /// assemble an 80 MiB Snapshot — over the 64 MiB socket frame cap,
    /// which killed the writer loop and left a `--connect` client
    /// permanently re-requesting the same unsendable snapshot. The
    /// budgeted snapshot must bincode-encode under the cap.
    #[test]
    fn budgeted_snapshot_stays_under_the_frame_cap_with_many_full_rings() {
        let mut terminals: Vec<lazybox_ipc::TerminalSnapshot> =
            (0..40).map(full_ring_snapshot).collect();
        let raw_total: usize = terminals.iter().map(|t| t.replay.len()).sum();
        assert!(
            raw_total as u32 > lazybox_ipc::MAX_FRAME_BYTES,
            "fixture must reproduce the oversized-snapshot condition"
        );

        budget_snapshot_replay(&mut terminals);

        let total: usize = terminals.iter().map(|t| t.replay.len()).sum();
        assert!(
            total <= SNAPSHOT_REPLAY_BUDGET,
            "total replay {total} must fit the {SNAPSHOT_REPLAY_BUDGET} budget"
        );
        let encoded = bincode::serde::encode_to_vec(
            Event::Snapshot {
                workspaces: Vec::new(),
                terminals,
                projects: Vec::new(),
                recent_snippets: Vec::new(),
                dismissed_updates: Vec::new(),
            },
            bincode::config::legacy(),
        )
        .expect("snapshot serializes");
        assert!(
            (encoded.len() as u64) < u64::from(lazybox_ipc::MAX_FRAME_BYTES),
            "encoded snapshot ({} bytes) must stay under MAX_FRAME_BYTES",
            encoded.len()
        );
    }

    /// Budgeting must never turn a full replay into a byte tail and still
    /// call it authoritative. Each terminal is either whole or explicitly
    /// unavailable.
    #[test]
    fn budget_keeps_or_omits_each_replay_atomically() {
        let mut terminals: Vec<lazybox_ipc::TerminalSnapshot> =
            (0..40).map(full_ring_snapshot).collect();

        budget_snapshot_replay(&mut terminals);

        for t in &terminals {
            assert!(
                (t.replay_available && t.replay.len() == pty::REPLAY_RING_BYTES)
                    || (!t.replay_available && t.replay.is_empty()),
                "replay must remain whole or be explicitly unavailable"
            );
        }
    }

    /// Under-budget snapshots pass through untouched — the common
    /// case must not pay for the pathological one.
    #[test]
    fn budget_leaves_small_snapshots_alone() {
        let mut terminals = vec![full_ring_snapshot(1), full_ring_snapshot(2)];
        let before: Vec<usize> = terminals.iter().map(|t| t.replay.len()).collect();
        budget_snapshot_replay(&mut terminals);
        let after: Vec<usize> = terminals.iter().map(|t| t.replay.len()).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn budget_clears_unavailable_tail_even_when_under_budget() {
        let mut terminal = full_ring_snapshot(1);
        terminal.replay = b"arbitrary-tail".to_vec();
        terminal.replay_available = false;
        budget_snapshot_replay(std::slice::from_mut(&mut terminal));
        assert!(terminal.replay.is_empty());
        assert!(!terminal.replay_available);
    }
}
