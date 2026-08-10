//! Wires the IPC `Spawn`/`Write`/`Resize`/`Close` commands to the
//! [`SessionBackend`](crate::backend::SessionBackend) trait. The
//! server itself owns no PTY state — every backend-side operation
//! goes through `config.backend`.
//!
//! ## Per-process state on `ServerConfig`
//!
//! `ServerConfig::terminal` maps wire `TerminalId` → backend session
//! key. Multiple connections (in-process channel + a remote SSH
//! `lazybox --connect`) share this map so they see the same set.
//!
//! ## Flow on Spawn
//!
//! 1. Resolve `kind` to argv:
//!    - `Agent(id)` → look up `Registry`, call `Agent::spawn(ctx)`.
//!    - `Shell` → the configured `shell.command`.
//!    - `LogTail` → `tail -F path`.
//! 2. `backend.spawn(argv, cwd, env)` returns a backend session key.
//! 3. Allocate a fresh `TerminalId`; store the pairing on
//!    `config.terminal.terminals`.
//! 4. `backend.subscribe(key)` → spawn a pump task that fans each
//!    output chunk to `config.bus` as `Event::TerminalOutput`. When
//!    the chunk stream ends, await `backend.wait_exit`, emit
//!    `Event::TerminalExited`, drop the map entry.
//! 5. Broadcast `Event::TerminalSpawned` to every subscriber.

mod spawn_executor;

pub use crate::spawn_plan::SpawnOptions;
use crate::spawn_plan::{SpawnPlanInput, build_spawn_plan};
use crate::{ServerConfig, SpawnCoordinator, TerminalRegistry, client_kv, terminal_io};
use chrono::Utc;
use futures::{StreamExt, stream};
use lazybox_core::{
    SessionId, SessionKey, SessionKind, Task, Workspace, WorkspaceKey, WorkspaceSession as Session,
};
use lazybox_ipc::{
    AgentRunAccess, Event, MAX_FRAME_BYTES, PromptSource, TerminalId, TerminalInputIntent,
    TerminalKind, TerminalSnapshot, UserPrompt, WorktreeStep, WorktreeStepStatus,
};
use lazybox_store::{StoreMutation, WorkspaceRecord};
use spawn_executor::{ExecutedSpawn, SpawnExecutionOutcome, execute_spawn_plan};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Hard ceiling on `backend.snapshot(key)` calls inside `snapshot_terminals`.
///
/// One wedged tmux session must not block the daemon's Subscribe handler.
/// Subscribe is the first thing every TUI sends after connecting, so if it
/// hangs the whole IPC channel stalls — no Spawn, no Write, nothing —
/// because subsequent commands queue behind the unfinished arm of
/// `tokio::select!`. 500ms is generous: a healthy mock/real backend
/// snapshots in microseconds; anything past that is a sign the per-PTY
/// ring mutex is being held by a hung pump and we'd rather degrade
/// (empty replay for that one terminal) than freeze the daemon.
const SNAPSHOT_PER_SESSION_TIMEOUT: Duration = Duration::from_millis(500);

/// Bound concurrent terminal snapshot assembly. Sequential 500ms deadlines
/// made N wedged sessions block Subscribe for N×500ms; unlimited fan-out
/// would instead stampede the blocking store pool on large installations.
const SNAPSHOT_CONCURRENCY: usize = 16;

/// Monotonic terminal-id allocator. Module-local so ids are unique
/// across the process even if the terminals map is wiped (tests, or
/// a future "kill all" command).
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

/// Store key for the highest terminal id ever allocated. Seeding the
/// allocator from it on every allocation makes ids unique across
/// daemon restarts, not just within one process — a terminal id is
/// referenced by artifacts that outlive the process (the per-terminal
/// hook settings file path), so a fresh daemon restarting at 1 would
/// silently reuse a surviving session's id.
const TERMINAL_ID_HIGH_WATER_KEY: &str = "terminal-id-high-water";
const SESSION_AGENT_ACCESS_PREFIX: &str = "session-agent-access:";

/// Every persisted value owned by one backend terminal. Keeping the key
/// namespace and cleanup inventory in one type prevents a restart feature
/// from adding a new `terminal-*` row that teardown never deletes (the draft
/// leak exposed by issue #373).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalPersistedField {
    Metadata,
    Access,
    NoPermission,
    /// Legacy single last-prompt row (`terminal-msg`). Superseded by
    /// `UserMessageHistory`; still read once for migration and swept at
    /// teardown so old rows don't leak (issue #523).
    UserMessage,
    /// Bounded per-session prompt history (`terminal-msgs`, JSON array of
    /// `UserPrompt`), issue #523.
    UserMessageHistory,
    Draft,
    PtyLaunchGeneration,
    AgentStateGeneration,
    AgentResume,
}

impl TerminalPersistedField {
    const ALL: [Self; 9] = [
        Self::Metadata,
        Self::Access,
        Self::NoPermission,
        Self::UserMessage,
        Self::UserMessageHistory,
        Self::Draft,
        Self::PtyLaunchGeneration,
        Self::AgentStateGeneration,
        Self::AgentResume,
    ];

    fn key(self, backend_key: &str) -> String {
        let prefix = match self {
            Self::Metadata => "terminal",
            Self::Access => "terminal-access",
            Self::NoPermission => "terminal-noperm",
            Self::UserMessage => "terminal-msg",
            Self::UserMessageHistory => "terminal-msgs",
            Self::Draft => "terminal-draft",
            Self::PtyLaunchGeneration => "terminal-pty-generation",
            Self::AgentStateGeneration => "terminal-agent-state-generation",
            Self::AgentResume => "terminal-agent-resume",
        };
        format!("{prefix}:{backend_key}")
    }
}

fn agent_state_key(backend_key: &str, generation: u64) -> String {
    format!("terminal-agent-state:{backend_key}:{generation}")
}

/// Serializes the seed → allocate → persist sequence of
/// [`alloc_terminal_id`]. Without it two concurrent spawns could
/// interleave as: A allocates 5, B allocates 6, B persists 6, A
/// persists 5 — regressing the stored high-water mark so a restarted
/// daemon re-issues 6 to a fresh terminal while a survivor's artifacts
/// still reference it.
static TERMINAL_ID_PERSIST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub(crate) fn alloc_terminal_id(store: &dyn lazybox_store::Store) -> TerminalId {
    // The guard makes the read-max-allocate-persist below atomic with
    // respect to every other in-process allocator, so the persisted
    // mark is always max(stored, allocated). Allocation is rare (one
    // per spawn) and the store calls are quick — a process-wide sync
    // mutex is fine. parking_lot locks are not poisoned by an unrelated
    // panic, so a later spawn can still advance the high-water mark.
    let _guard = TERMINAL_ID_PERSIST_LOCK.lock();
    // `fetch_max` (not a one-shot seed) so the allocator is correct
    // even when several stores are seen in one process (tests) — the
    // counter only ever moves forward.
    if let Ok(Some(raw)) = store.get_kv(TERMINAL_ID_HIGH_WATER_KEY)
        && let Ok(high_water) = raw.trim().parse::<u64>()
    {
        NEXT_TERMINAL_ID.fetch_max(high_water + 1, Ordering::Relaxed);
    }
    let id = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
    // Under the guard `id` is strictly greater than this store's
    // persisted mark (the seed above raised the counter past it), so
    // this write never moves the mark backwards.
    if let Err(e) = store.set_kv(TERMINAL_ID_HIGH_WATER_KEY, &id.to_string()) {
        tracing::warn!("terminal-id high-water mark: store write failed: {e}");
    }
    TerminalId(id)
}

/// How a workspace task's declared priority routes onto an agent's model
/// menu. Keeps "no priority declared" distinct from "declared but this
/// agent maps it to nothing": the latter silently falls back to the
/// default model, and looks from the outside exactly like the label was
/// ignored, so the spawn path logs it (issue #748).
#[derive(Debug, PartialEq, Eq)]
enum PriorityRoute {
    /// The task declared no priority — nothing to route.
    None,
    /// The declared priority maps to this tier alias.
    Mapped(String),
    /// The priority is declared, but this agent's menu maps it to no
    /// tier — the spawn keeps its default tier / model.
    Unmapped(lazybox_core::PriorityTier),
}

/// Pure decision behind [`priority_alias_for`]: route the (optional)
/// declared priority onto `models`, without touching the store, so the
/// three outcomes are individually testable.
fn route_declared_priority(
    tier: Option<lazybox_core::PriorityTier>,
    models: &lazybox_core::AgentModels,
) -> PriorityRoute {
    match tier {
        None => PriorityRoute::None,
        Some(tier) => match models.alias_for_priority(tier) {
            Some(alias) => PriorityRoute::Mapped(alias.to_string()),
            None => PriorityRoute::Unmapped(tier),
        },
    }
}

/// The tier alias the workspace task's declared priority
/// (`best`/`high`/`medium`/`low` label or `@best`/`@high`/`@medium`/`@low`
/// body marker) maps to for `models`. `None` when the task declares no
/// priority, the workspace/task can't be loaded, or this agent maps that
/// priority to nothing — the spawn then keeps its default tier / model.
/// The declared-but-unmapped case is logged so it doesn't look like the
/// priority was silently ignored. Used only as the fallback when no
/// explicit tier chord was passed.
fn priority_alias_for(
    config: &ServerConfig,
    session_key: &SessionKey,
    models: &lazybox_core::AgentModels,
) -> Option<String> {
    let tier = load_workspace(config, &WorkspaceKey::new(session_key.as_str()))
        .ok()
        .and_then(|w| {
            w.primary_task()
                .and_then(lazybox_core::resolve_priority_tier)
        });
    match route_declared_priority(tier, models) {
        PriorityRoute::None => None,
        PriorityRoute::Mapped(alias) => Some(alias),
        PriorityRoute::Unmapped(tier) => {
            tracing::info!(
                priority = tier.as_str(),
                "spawn: task declares `{}` priority but this agent maps it to no model tier — falling back to the default model",
                tier.as_str()
            );
            None
        }
    }
}

/// Absolute path to the per-terminal Claude settings file lazybox writes
/// at spawn. Deterministic in `terminal_id` so the pump can delete it on
/// exit without bookkeeping a map.
fn hook_settings_path(terminal_id: TerminalId) -> PathBuf {
    lazybox_core::paths::runtime_dir()
        .join("hooks")
        .join(format!("settings-{}.json", terminal_id.0))
}

/// Per-terminal file holding the backend session key for an agent whose
/// hook command is baked into the spawn argv (Codex). Argv can't be
/// rewritten once the process launches, and the backend key only exists
/// after `backend.spawn` returns — so the argv-baked command reads the
/// key from this file (written post-spawn) instead of embedding it, the
/// same late-binding trick Claude's settings-file rewrite performs.
/// Deterministic in `terminal_id`, so the pump deletes it on exit with
/// no bookkeeping, and it survives daemon restarts to keep a
/// tmux-surviving session's hooks correlating.
fn hook_backend_key_path(terminal_id: TerminalId) -> PathBuf {
    lazybox_core::paths::runtime_dir()
        .join("hooks")
        .join(format!("backend-key-{}", terminal_id.0))
}

/// The hook command an argv-hooked agent runs on each lifecycle event.
/// Reads the correlation key from [`hook_backend_key_path`] (see there
/// for why it's a file, not an inline value). Missing key file → a
/// flagless `hook-ingest` that drains stdin and exits 0, so a hook racing
/// the post-spawn key write is a harmless no-op, not a failure.
fn hook_command_keyfile(exe: &Path, key_path: &Path) -> String {
    guarded_hook_command(
        exe,
        &format!(" --backend-key-file \"{}\"", key_path.display()),
        &lazybox_core::paths::hook_log_path(),
    )
}

/// The shell command Claude runs on each lifecycle hook. Uses the
/// running lazybox binary's absolute path (so it works regardless of
/// `$PATH` inside the agent's environment) and bakes in the backend
/// session key (the tmux session name), so the daemon correlates the
/// hook back to this exact terminal with no guessing. The backend key
/// — not the wire `TerminalId` — because the agent process can outlive
/// the daemon (tmux): after a restart the surviving session's hooks
/// must still resolve, and the backend key is the identity that
/// survives while terminal ids are reallocated.
fn hook_command(exe: &Path, backend_key: &str) -> String {
    guarded_hook_command(
        exe,
        &format!(" --backend-key \"{backend_key}\""),
        &lazybox_core::paths::hook_log_path(),
    )
}

/// Hook command with no correlation flag — what the pre-spawn
/// placeholder settings file carries (see [`write_hook_settings`]'s
/// callers). `hook-ingest` without a correlation flag drains stdin and
/// exits 0, so if the agent ever races the post-spawn rewrite and
/// reads the placeholder, its hooks are harmless no-ops and the
/// session just keeps PTY detection.
fn hook_command_placeholder(exe: &Path) -> String {
    guarded_hook_command(exe, "", &lazybox_core::paths::hook_log_path())
}

/// A build-independent path to the lazybox binary to bake into an agent's
/// hook command. The daemon's own `current_exe()` for a dev build is
/// `<worktree>/target/debug/lazybox` — a path a `cargo clean`, a rebuild,
/// worktree removal, or session transfer (PR #717) invalidates, after
/// which every hook fires against a dead reference. So
/// [`ensure_stable_hook_exe`] copies that binary to the stable
/// `<home>/bin/lazybox` (see [`lazybox_core::paths::stable_exe_path`]) and
/// this bakes *that*: nothing under `<home>/bin` moves when a worktree does,
/// and the copy keeps working even after the original `target/debug`
/// artifact is cleaned.
///
/// This is a pure read — the copy lives in [`ensure_stable_hook_exe`], which
/// the daemon runs once at boot — so resolving a path here never writes to
/// `<home>/bin` (which would block the spawn on an ~80 MB copy and, in the
/// integration tests that don't isolate `LAZYBOX_HOME`, thrash the real
/// home). Returns the stable copy when present; otherwise the raw
/// `current_exe()` (still absolute, just less durable); or `None` when no
/// executable resolves at all, and the spawn falls back to PTY detection.
fn hook_exe() -> Option<PathBuf> {
    let stable = lazybox_core::paths::stable_exe_path();
    if stable.is_file() {
        return Some(stable);
    }
    // Fall back to the running binary when the stable copy isn't in place yet
    // (integration tests that drive `handle_spawn` directly, or a daemon whose
    // boot-time `ensure_stable_hook_exe` couldn't write `<home>/bin`). This is
    // safe against desktop defect #1 — a hook invoking a GUI binary as
    // `<exe> hook-ingest …` ingests and exits before any window opens, and
    // `ensure_stable_hook_exe` still refuses to *install* a non-hook-capable
    // executable as the durable helper.
    std::env::current_exe().ok().filter(|p| p.is_file())
}

pub const HOOK_HELPER_PROBE_ARG: &str = "--lazybox-hook-helper-probe";
pub const HOOK_HELPER_PROBE_RESPONSE: &str = "lazybox-hook-helper-v1";

pub fn hook_helper_probe_requested(args: &[String]) -> bool {
    args.len() == 1 && args[0] == HOOK_HELPER_PROBE_ARG
}

/// Copy the running binary to the stable `<home>/bin/lazybox` so agent
/// hooks can bake a path that survives a rebuild / `cargo clean` / worktree
/// removal. Best-effort and idempotent (a fresh copy is skipped by
/// `stabilize_exe`'s freshness check).
///
/// The daemon calls this **once at boot**, never per spawn: the copy is an
/// ~80 MB blocking `fs::copy` that would delay every `TerminalSpawned`, and
/// integration tests drive `handle_spawn` directly against the real
/// `LAZYBOX_HOME` — a per-spawn copy there raced dozens of test processes on
/// one `~/.lazybox/bin/lazybox`. Doing it at boot keeps the hot path a pure
/// `hook_exe` read and leaves tests (which never boot a daemon) untouched.
pub fn ensure_stable_hook_exe() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    ensure_stable_hook_exe_from(&current, &lazybox_core::paths::stable_exe_path())
}

fn ensure_stable_hook_exe_from(current: &Path, stable: &Path) -> Option<PathBuf> {
    if !is_hook_capable_exe(current) {
        tracing::error!(
            executable = %current.display(),
            "refusing to install a hook helper that cannot ingest lifecycle hooks"
        );
        return None;
    }
    let stabilized = stabilize_exe(current, stable);
    if stabilized.is_none() {
        // A copy/metadata failure (unwritable bin dir, full disk) otherwise
        // vanishes: `hook_exe` then finds no stable helper and — with the
        // current-exe fallback gone in release builds — silently disables
        // lifecycle hooks. Callers that only need best-effort hooks (tui-boot)
        // ignore the return, so this is the sole place the failure is recorded.
        tracing::error!(
            executable = %current.display(),
            stable = %stable.display(),
            "failed to install the stable hook helper; lifecycle hooks will be disabled"
        );
    }
    stabilized
}

fn is_hook_capable_exe(candidate: &Path) -> bool {
    let Ok(mut child) = std::process::Command::new(candidate)
        .arg(HOOK_HELPER_PROBE_ARG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let Ok(output) = child.wait_with_output() else {
                    return false;
                };
                return status.success()
                    && String::from_utf8_lossy(&output.stdout).trim()
                        == HOOK_HELPER_PROBE_RESPONSE;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Copy `current` to the stable `stable` path when the copy is missing or
/// stale, returning `stable`. Freshness is length + mtime: a re-copy only
/// happens after a rebuild (which bumps both), so a repeat call is a cheap
/// stat. `None` on any IO failure, leaving the caller to fall back to the
/// raw `current` path.
fn stabilize_exe(current: &Path, stable: &Path) -> Option<PathBuf> {
    if current == stable {
        return Some(stable.to_path_buf());
    }
    // Serialize concurrent copies. Spawns run on the Detached command lane
    // (up to `MAX_CONNECTION_MUTATIONS` at once — a `Shift-B` broadcast
    // fires many together) and every one calls this. Without the lock,
    // racing copies share the one fixed `lazybox.tmp`: a `rename` can
    // publish it as `stable` while another copy is still writing that
    // inode, briefly exposing a half-written binary — a hook exec in that
    // window fails with `ETXTBSY`, the exact red error the guard exists to
    // avoid. One daemon owns the profile (socket lock), so an in-process
    // lock suffices. Re-checking freshness *inside* the lock also collapses
    // the racing copies to one: the losers see the winner's copy and skip.
    static COPY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = COPY_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let src = std::fs::metadata(current).ok()?;
    let fresh =
        std::fs::metadata(stable)
            .ok()
            .is_some_and(|dst| match (dst.modified(), src.modified()) {
                // A newer-or-equal copy of the same size is up to date: our
                // own copy stamps a copy-time mtime ≥ the build's, and a
                // rebuild makes the source newer than any prior copy.
                (Ok(dst_m), Ok(src_m)) => dst.len() == src.len() && dst_m >= src_m,
                _ => false,
            });
    if fresh {
        return Some(stable.to_path_buf());
    }
    std::fs::create_dir_all(stable.parent()?).ok()?;
    // Copy to a temp sibling then rename: a hook mid-exec of the old copy
    // keeps its inode (rename only swaps the directory entry, so no
    // ETXTBSY and no torn read). `fs::copy` carries the exec bit over. The
    // fixed tmp name is safe only because `COPY_LOCK` serializes writers.
    let tmp = stable.with_extension("tmp");
    std::fs::copy(current, &tmp).ok()?;
    std::fs::rename(&tmp, stable).ok()?;
    Some(stable.to_path_buf())
}

/// The exec is guarded: the binary verified at spawn time can still be
/// deleted mid-session (`cargo clean` of the stable copy's source, or the
/// copy's own directory being removed), and without the guard every hook
/// fails with a raw `/bin/sh: <path>: No such file or directory`. The
/// guard degrades gracefully instead — it appends a note to
/// [`lazybox_core::paths::hook_log_path`] and exits 0, so a stale reference
/// costs a single missed state signal, never a red `PostToolUse:Bash hook
/// error` on every command the agent runs.
fn guarded_hook_command(exe: &Path, args: &str, log: &Path) -> String {
    let exe = exe.to_string_lossy();
    let log = log.display();
    format!(
        "[ -x \"{exe}\" ] || {{ echo \"lazybox hook: binary missing at {exe} (rebuild, cargo clean, or worktree removed) — state signal skipped\" >> \"{log}\" 2>/dev/null; exit 0; }}; \"{exe}\" hook-ingest{args}"
    )
}

/// Read and parse the user's `~/.claude/settings.json`, if present, so
/// the generated settings file can merge their existing hooks instead of
/// silently overriding them (Claude's `--settings` takes precedence over
/// user scope). Any error (missing file, bad JSON) → `None`, which the
/// generator treats as "no user hooks."
fn read_user_claude_settings() -> Option<serde_json::Value> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".claude").join("settings.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Generate and write the per-session hooks settings file for an agent
/// that supports structured lifecycle hooks. Returns the path to pass via
/// the agent's settings flag, or `None` when the agent has no hook
/// support or writing failed (in which case the spawn proceeds without
/// hooks and falls back to PTY detection).
///
/// Written in two phases by `handle_spawn`, because the hook command
/// carries the backend session key and that key only exists once
/// `backend.spawn` returns — while the settings *path* must already be
/// in the argv the backend launches:
///   1. pre-spawn, with [`hook_command_placeholder`] — so the file
///      exists (with the user's settings merged in) by the time the
///      agent boots, whatever the timing;
///   2. post-spawn, with [`hook_command`]`(backend_key)` — an atomic
///      rewrite (temp + rename) that lands within the same tick as
///      `backend.spawn` returning, long before the agent's runtime
///      gets around to reading its settings. If the agent somehow
///      reads the placeholder first, its hooks are no-ops and the
///      session falls back to PTY detection — degraded, never wrong.
fn write_hook_settings(
    config: &ServerConfig,
    kind: &TerminalKind,
    terminal_id: TerminalId,
    command: &str,
) -> Option<PathBuf> {
    let TerminalKind::Agent(agent_id) = kind else {
        return None;
    };
    let agent = config.agents.get(agent_id)?;
    let user = read_user_claude_settings();
    let settings = agent.build_hook_settings(command, user.as_ref())?;
    let path = hook_settings_path(terminal_id);
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("hook settings: create_dir_all {}: {e}", parent.display());
        return None;
    }
    let json = serde_json::to_string_pretty(&settings).ok()?;
    // Write-to-temp + rename so a concurrent reader (the just-launched
    // agent) can never observe a torn file on the phase-2 rewrite.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, json) {
        tracing::warn!("hook settings: write {}: {e}", tmp.display());
        return None;
    }
    match std::fs::rename(&tmp, &path) {
        Ok(()) => {
            tracing::info!(?terminal_id, path = %path.display(), "wrote hook settings file");
            Some(path)
        }
        Err(e) => {
            tracing::warn!("hook settings: rename into {}: {e}", path.display());
            None
        }
    }
}

/// Write the backend session key to the per-terminal file an argv-hooked
/// agent's baked hook command reads (see [`hook_backend_key_path`]).
/// Write-to-temp + rename so a hook that fires mid-write never reads a
/// torn key. Best-effort: a failure leaves the file absent, and a hook
/// that finds no key resolves to a harmless no-op.
fn write_hook_backend_key(terminal_id: TerminalId, backend_key: &str) {
    let path = hook_backend_key_path(terminal_id);
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("hook backend key: create_dir_all {}: {e}", parent.display());
        return;
    }
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, backend_key) {
        tracing::warn!("hook backend key: write {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!("hook backend key: rename into {}: {e}", path.display());
    }
}

/// Which emitter produced an `Event::AgentState`. Logged on every
/// broadcast so the PTY detector, optimistic flip, hook ingest, and exit
/// teardown interleave as one greppable stream on a single terminal — the
/// view the #167/#161 stale-key confusion needed but never had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateSource {
    /// The output pump's PTY screen-scrape detector.
    Pty,
    /// The optimistic `InputNeeded → Working` flip in `handle_write`
    /// when the user answers a prompt.
    Flip,
    /// A structured lifecycle hook ingested from the agent.
    Hook,
    /// The PTY-exit teardown moving a dead agent to `Exited` (#357).
    Exit,
}

/// Result of offering a direct (hook / input / exit) transition. The
/// candidate remains visible on a rejected or duplicate transition because
/// hook ingest still uses its prompt shape even when the state pill did not
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectStateTransition {
    previous: Option<lazybox_ipc::AgentState>,
    candidate: Option<lazybox_ipc::AgentState>,
    committed: bool,
}

#[derive(Clone)]
struct AgentStateDurability {
    store: std::sync::Arc<dyn lazybox_store::Store>,
    backend_key: String,
    generation: u64,
    poll: crate::PollState,
}

impl AgentStateDurability {
    async fn persist(&self, state: lazybox_ipc::AgentState) -> bool {
        let generation_key = TerminalPersistedField::AgentStateGeneration.key(&self.backend_key);
        let state_key = agent_state_key(&self.backend_key, self.generation);
        let generation = self.generation.to_string();
        let state = match serde_json::to_string(&state) {
            Ok(state) => state,
            Err(error) => {
                tracing::error!(
                    backend_key = %self.backend_key,
                    generation = self.generation,
                    %error,
                    "agent state persistence: encode failed"
                );
                return false;
            }
        };
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            store.apply_batch(&[
                StoreMutation::SetKv {
                    key: generation_key,
                    value: generation,
                },
                StoreMutation::SetKv {
                    key: state_key,
                    value: state,
                },
            ])
        })
        .await
        {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::error!(
                    backend_key = %self.backend_key,
                    generation = self.generation,
                    %error,
                    "agent state persistence: store write failed"
                );
                false
            }
            Err(error) => {
                tracing::error!(
                    backend_key = %self.backend_key,
                    generation = self.generation,
                    %error,
                    "agent state persistence: store task failed"
                );
                false
            }
        }
    }
}

async fn agent_state_durability(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
) -> Option<AgentStateDurability> {
    let generation = config
        .terminal
        .agent_state_generations
        .lock()
        .await
        .get(&terminal_id)
        .copied();
    let Some(generation) = generation else {
        tracing::error!(
            ?terminal_id,
            %backend_key,
            "agent state invariant: live terminal has no PTY generation"
        );
        return None;
    };
    Some(AgentStateDurability {
        store: config.store.clone(),
        backend_key: backend_key.to_string(),
        generation,
        poll: config.poll.clone(),
    })
}

struct StateFold<R> {
    result: R,
    committed: bool,
}

/// The single state-ownership boundary for an agent terminal.
///
/// It co-holds `terminal_meta → agent_states` in the documented canonical
/// order, folds one candidate under the state lock, persists the committed
/// state, updates the cache, and broadcasts before releasing either lock.
/// Durable order, cache order, and bus order are therefore identical: a
/// concurrent late hook can never be delivered after a committed `Exited`,
/// and an issue→PR rebadge cannot race between live-key resolution and the
/// event send (#161/#167/#357).
///
/// `fold` returns its caller-specific result and the state to commit. Both
/// the direct-transition wrapper and PTY-reading path route through here;
/// this is the only production `Event::AgentState` send site.
async fn fold_and_broadcast_agent_state<R>(
    terminals: &TerminalRegistry,
    bus: &tokio::sync::broadcast::Sender<Event>,
    durability: &AgentStateDurability,
    id: TerminalId,
    captured: &SessionKey,
    source: StateSource,
    // A short, greppable cause for this transition — the "why" the issue
    // #538 observability ask wants alongside `source`: which PTY liveness
    // tier settled it (`pty-quiet-settle`, `pty-watchdog-force`), or which
    // direct path drove it (`lifecycle-hook`, `user-answered-flip`,
    // `process-exit`). `source` says who; `reason` says why.
    reason: &'static str,
    fold: impl FnOnce(Option<lazybox_ipc::AgentState>, bool) -> (R, Option<lazybox_ipc::AgentState>),
) -> StateFold<R> {
    let meta = terminals.terminal_meta.lock().await;
    let live_session = meta.get(&id).map(|(sk, _)| sk.clone());
    let session_key = live_session.clone().unwrap_or_else(|| captured.clone());
    let terminal_live = live_session.is_some();
    let mut states = terminals.agent_states.lock().await;
    let previous = states.get(&id).copied();
    let (result, committed) = fold(previous, terminal_live);
    if let Some(state) = committed {
        if !durability.persist(state).await {
            return StateFold {
                result,
                committed: false,
            };
        }
        states.insert(id, state);
        tracing::info!(
            terminal_id = ?id,
            %session_key,
            ?source,
            reason,
            previous = ?previous,
            state = ?state,
            "agent state transition → cache + Event::AgentState",
        );
        let _ = bus.send(Event::AgentState {
            session_key,
            terminal_id: id,
            state,
        });
        if state == lazybox_ipc::AgentState::Done {
            durability.poll.wake(false);
        }
        return StateFold {
            result,
            committed: true,
        };
    }
    StateFold {
        result,
        committed: false,
    }
}

/// Offer a direct state candidate through the structural transition table,
/// then atomically cache and broadcast it through
/// [`fold_and_broadcast_agent_state`]. `candidate_for` runs under the state
/// lock so current-state-dependent hook mappings and optimistic flips are
/// compare-and-set operations rather than stale read/modify/write pairs.
async fn transition_and_broadcast_agent_state(
    terminals: &TerminalRegistry,
    bus: &tokio::sync::broadcast::Sender<Event>,
    durability: &AgentStateDurability,
    id: TerminalId,
    captured: &SessionKey,
    source: StateSource,
    candidate_for: impl FnOnce(Option<lazybox_ipc::AgentState>) -> Option<lazybox_ipc::AgentState>,
) -> DirectStateTransition {
    // The direct paths' "why" follows straight from which one drove the
    // move; the PTY path derives its own finer-grained reason in
    // `commit_pty_reading`.
    let reason = match source {
        StateSource::Flip => "user-answered-flip",
        StateSource::Hook => "lifecycle-hook",
        StateSource::Exit => "process-exit",
        StateSource::Pty => "pty",
    };
    let folded = fold_and_broadcast_agent_state(
        terminals,
        bus,
        durability,
        id,
        captured,
        source,
        reason,
        |previous, terminal_live| {
            let candidate = candidate_for(previous);
            // `Exit` is allowed to use the captured key during teardown;
            // every other source must still be attached to live metadata.
            // This is the ingress gate that prevents a delayed hook/PTY
            // reading from recreating state after terminal teardown.
            let committed = (terminal_live || source == StateSource::Exit)
                .then(|| {
                    candidate.and_then(|candidate| {
                        lazybox_agents::AgentStateMachine::transition(previous, candidate)
                    })
                })
                .flatten();
            (
                DirectStateTransition {
                    previous,
                    candidate,
                    committed: committed.is_some(),
                },
                committed,
            )
        },
    )
    .await;
    if folded.result.previous == Some(lazybox_ipc::AgentState::Working)
        && folded.result.candidate == Some(lazybox_ipc::AgentState::Idle)
        && !folded.committed
    {
        tracing::error!(
            terminal_id = ?id,
            ?source,
            "agent state invariant: refused Working → Idle transition"
        );
    }
    DirectStateTransition {
        committed: folded.committed,
        ..folded.result
    }
}

fn wake_poll_for_terminal_kind(config: &ServerConfig, kind: &TerminalKind) {
    if matches!(kind, TerminalKind::Agent(_)) {
        config.poll.wake(false);
    }
}

struct CorrelatedCommandOutcome {
    bus: tokio::sync::broadcast::Sender<Event>,
    client_request_id: Option<String>,
    failure_message: &'static str,
    finished: bool,
}

impl CorrelatedCommandOutcome {
    fn new(
        config: &ServerConfig,
        client_request_id: Option<String>,
        failure_message: &'static str,
    ) -> Self {
        Self {
            bus: config.bus.clone(),
            client_request_id,
            failure_message,
            finished: false,
        }
    }

    fn complete(&mut self) {
        self.finished = true;
        if let Some(client_request_id) = self.client_request_id.take() {
            let _ = self.bus.send(Event::CommandCompleted { client_request_id });
        }
    }
}

impl Drop for CorrelatedCommandOutcome {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(client_request_id) = self.client_request_id.take() {
            let _ = self.bus.send(Event::CommandFailed {
                client_request_id,
                message: self.failure_message.into(),
            });
        }
    }
}

/// Spawn a terminal inside a session and broadcast
/// `Event::TerminalSpawned`. Failures emit `Event::ProviderError` so
/// the user gets feedback in the TUI rather than a silent miss.
///
/// Resolution order for the cwd / target session:
///
/// 1. If `cwd` is `Some`, the caller wins — use it raw.
/// 2. Else load the workspace, find a session via `session_id` (or
///    fall back to its default) and use that session's
///    `worktree_path` as cwd.
/// 3. If the workspace has no sessions yet, auto-create one rooted
///    in `cwd_or_inherited` (current dir today) and persist the
///    workspace before spawning. The auto-creation emits
///    `Event::SessionCreated`.
///
/// This keeps the user-facing flow simple — pressing `s` on a fresh
/// workspace "just works" — while preserving the invariant that
/// every terminal lives inside a session, which lives inside a
/// folder worktree.
pub async fn handle_spawn(
    config: &ServerConfig,
    session_key: SessionKey,
    session_id: Option<SessionId>,
    kind: TerminalKind,
    options: SpawnOptions,
) -> Option<TerminalId> {
    let _workspace_agent = if matches!(kind, TerminalKind::Agent(_)) {
        Some(config.spawn.lock_workspace_agent(&session_key).await)
    } else {
        None
    };
    handle_spawn_inner(config, session_key, session_id, kind, options).await
}

/// The worktree path a spawn for this workspace resolves to, mirroring
/// the resolution order in [`resolve_or_create_session`] so the recreate
/// preserves the checkout that actually blocked *this* spawn rather than
/// always session index 0. `None` for a linked (no-worktree) workspace,
/// whose sessions run in the user's real checkout and must never be moved
/// aside, and for an `on_main` spawn on a repo-less workspace (no shared
/// checkout to resolve).
fn spawn_target_worktree_path(
    workspace: &Workspace,
    session_id: Option<SessionId>,
    on_main: bool,
) -> Option<PathBuf> {
    if workspace.is_linked() {
        return None;
    }
    if on_main {
        return main_worktree_path(workspace);
    }
    if let Some(id) = session_id {
        return workspace.find_session(id).map(|s| s.worktree_path.clone());
    }
    if let Some(session) = workspace.default_session() {
        return Some(session.worktree_path.clone());
    }
    Some(worktree_path_for_session(workspace, 0))
}

/// Move the checkout blocking a stuck spawn aside so its branch is freed
/// and a fresh worktree can be provisioned (issue #787's recreate). The
/// backup path (or `None` when nothing needed moving) is returned; the
/// branch-conflict classes only arise on repo-backed worktrees, so a
/// repo-less workspace is a no-op. `preserve_holder` is a
/// `BranchHeldManaged` holder at a different path; `None` moves the exact
/// worktree the recreate's spawn (`session_id` / `on_main`) will target.
async fn preserve_stuck_worktree(
    config: &ServerConfig,
    session_key: &SessionKey,
    session_id: Option<SessionId>,
    on_main: bool,
    preserve_holder: Option<String>,
) -> Result<Option<PathBuf>, lazybox_git_ops::GitError> {
    let workspace_key = WorkspaceKey::new(session_key.as_str());
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    let Ok(workspace) = load_workspace(config, &workspace_key) else {
        return Ok(None);
    };
    let Ok(Some(repo)) = repo_for_workspace_provision(config, &workspace, &cfg) else {
        return Ok(None);
    };
    let Some((owner, name)) = repo.split_once('/') else {
        return Ok(None);
    };
    let preserve_path = match preserve_holder {
        Some(holder) => PathBuf::from(holder),
        None => match spawn_target_worktree_path(&workspace, session_id, on_main) {
            Some(path) => path,
            // Linked / unresolvable target: never move the user's real
            // checkout — leave the spawn to surface its own error.
            None => return Ok(None),
        },
    };
    let mgr = config.worktree_manager();
    let bare_path = mgr.bare_path(owner, name);
    let backup = mgr
        .preserve_worktree_aside(&bare_path, &preserve_path)
        .await?;
    if let Some(backup) = &backup {
        tracing::info!(
            workspace = workspace_key.as_str(),
            preserved = %backup.display(),
            "preserved conflicting worktree aside before recreate",
        );
    }
    Ok(backup)
}

/// Recover a workspace wedged on a non-retryable worktree conflict
/// (issue #787): preserve the blocking checkout aside (`*.bak-<n>`),
/// freeing its branch, then re-run the original spawn so a fresh worktree
/// is provisioned. `preserve_holder` names the `BranchHeldManaged` holder
/// to move (a different-path non-live worktree); `None` moves the
/// workspace's own target worktree (the `BranchMismatch` / `DirtyLeftover`
/// case). The move + prune runs to completion before the spawn, so the
/// re-provision sees a clean slate — unlike two separate client commands,
/// which race on the detached mutation lanes.
pub async fn handle_recreate_worktree(
    config: &ServerConfig,
    spawn: lazybox_ipc::SpawnFallback,
    initial_prompt: Option<String>,
    on_main: bool,
    preserve_holder: Option<String>,
) {
    let session_key = spawn.session_key.clone();
    if let Err(error) = preserve_stuck_worktree(
        config,
        &session_key,
        spawn.session_id,
        on_main,
        preserve_holder,
    )
    .await
    {
        tracing::warn!(
            workspace = session_key.as_str(),
            "could not preserve worktree aside for recreate: {error}",
        );
        let _ = config.bus.send(Event::provider_error(
            "spawn:recreate",
            format!("could not free the stuck worktree: {error}"),
            lazybox_ipc::ProviderErrorKind::Permanent,
        ));
        return;
    }

    let autonomous = spawn_is_autonomous(&initial_prompt);
    handle_spawn(
        config,
        session_key,
        spawn.session_id,
        spawn.kind,
        SpawnOptions {
            cwd: spawn.cwd,
            initial_prompt,
            autonomous,
            on_main,
            model_alias: spawn.model_alias,
            access: spawn.access,
            client_request_id: spawn.client_request_id,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
            ..Default::default()
        },
    )
    .await;
}

async fn handle_spawn_inner(
    config: &ServerConfig,
    session_key: SessionKey,
    session_id: Option<SessionId>,
    kind: TerminalKind,
    options: SpawnOptions,
) -> Option<TerminalId> {
    let SpawnOptions {
        cwd,
        initial_prompt,
        autonomous,
        on_main,
        model_alias,
        resume,
        provider_session_id,
        no_permission_override,
        replace_terminal_id,
        prompt_history,
        composing_buffer,
        access,
        client_request_id,
        origin,
    } = options;
    let access = if matches!(&kind, TerminalKind::Agent(_)) {
        access
    } else {
        AgentRunAccess::Default
    };
    let mut outcome =
        CorrelatedCommandOutcome::new(config, client_request_id, "terminal was not spawned");
    // Autonomous sessions (e.g. `@lazybox`-triggered work) launch with
    // tool-use permission prompts disabled so the agent runs unattended
    // — there's no human nearby to approve. Gated by config so a
    // paranoid user can force prompts on every session. Interactive
    // spawns keep the prompt as the human-in-the-loop guard unless the
    // user opts in via `agent.skip_permissions` (Settings toggle).
    // The flag works under both Claude subscription login and an API
    // key; the only bypass restriction is no-root/sudo, which the
    // worktree sessions satisfy.
    // Wall-clock origin for the spawn → inject timing trace. Every
    // milestone below logs `elapsed_ms` against this so the "long delay
    // between `w` and the prompt being injected" is measurable from
    // `/tmp/lazybox.log` instead of guessed at (#142).
    let t0 = std::time::Instant::now();
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    let priority_model_alias = match &kind {
        TerminalKind::Agent(agent_id) if model_alias.is_none() => {
            priority_alias_for(config, &session_key, &cfg.agent_models(agent_id))
        }
        _ => None,
    };
    tracing::info!(
        %session_key,
        ?session_id,
        ?kind,
        cwd = ?cwd,
        has_initial_prompt = initial_prompt.is_some(),
        autonomous,
        "handle_spawn: entry"
    );
    // A linked (no-worktree) workspace runs every session in the user's
    // existing on-disk checkout — the same "shared checkout, not an
    // isolated worktree" shape as an on-main spawn. Treat it as on-main
    // from the very top so the inflight-singleton identity, the
    // duplicate-singleton check, and the resolver all agree it landed on
    // the shared checkout. Otherwise a normal `a c` (request
    // on_main=false) would land on the checkout yet claim the *non*-main
    // singleton, and a second press would spawn a DUPLICATE agent into
    // the real tree instead of reusing the first. The on-main path also
    // persists NO session, so no worktree-cleanup path can ever
    // `rm -rf` the user's real checkout. A `cwd` override is an ad-hoc
    // spawn with no workspace to inspect, so it's left untouched.
    let on_main = on_main || (cwd.is_none() && workspace_is_linked(config, &session_key));
    // In-flight guard — claim the singleton identity BEFORE the
    // duplicate check below. That check reads maps populated only after
    // worktree provisioning + `backend.spawn` (minutes on a cold
    // clone); two `w` presses in that window each passed it and
    // launched two skip-permissions agents into one worktree — the same
    // race hits autofix dispatch and startup restore. The loser
    // collapses onto the winner exactly like the existing-singleton
    // path: wait for the winner's terminal, deliver the prompt, focus.
    // Held in a drop guard so every exit path — including the failure
    // returns below — releases the claim.
    let inflight = match InflightSpawnGuard::try_claim(&config.spawn, &session_key, &kind, on_main)
    {
        Ok(guard) => guard,
        Err(()) => {
            if collapse_onto_inflight_spawn(
                config,
                &session_key,
                &kind,
                on_main,
                access,
                initial_prompt.as_deref(),
            )
            .await
            {
                outcome.complete();
            }
            return None;
        }
    };
    // Singleton enforcement at the daemon (the source of truth for
    // who's running what). The TUI also intercepts duplicates
    // client-side for snappy focus-not-spawn behavior, but that
    // alone fails the moment a second client connects to the same
    // daemon. The guard here protects the invariant for everyone:
    // at most one Claude per session, one Codex per session, etc.
    if let Some(existing) =
        find_existing_singleton(config, &session_key, &kind, Some(on_main)).await
    {
        if terminal_access_for(config, existing).await != access {
            let _ = config.bus.send(Event::provider_error_permanent(
                "spawn",
                "an agent with a different host-access policy is already running in this checkout",
            ));
            return None;
        }
        tracing::info!(
            terminal_id = ?existing,
            has_initial_prompt = initial_prompt.is_some(),
            "handle_spawn: existing singleton found, sending TerminalFocusRequested"
        );
        // A prompt-carrying Spawn collapsing onto a live singleton must
        // still deliver its work prompt — otherwise `w` on a PR whose
        // agent is already running silently drops the instruction and
        // the user just gets focused onto an idle terminal.
        if let Some(prompt) = initial_prompt.as_deref() {
            // Boxed: `handle_inject_prompt`'s fallback arm can call back
            // into `handle_spawn`, and a recursive async cycle needs one
            // pointer indirection to keep the futures finitely sized.
            // (This call passes no fallback, so it can't actually recurse.)
            Box::pin(handle_inject_prompt(config, existing, prompt, None, true)).await;
        }
        let _ = config.bus.send(Event::TerminalFocusRequested {
            terminal_id: existing,
        });
        outcome.complete();
        return Some(existing);
    }
    // Resolve target session + cwd. The cwd param wins over
    // workspace lookup so the existing `Spawn { cwd: Some(...) }`
    // callers (tests, in-process flows) keep working unchanged.
    // `owning_session` is the session id this spawn lives in — used
    // to populate `terminal_sessions` so the migration freeze can
    // scope correctly. None when cwd was overridden out-of-band.
    // `on_main` is the REQUEST; `landed_on_main` is whether the spawn
    // actually reached the shared main checkout. They diverge when the
    // request can't be honored — a `cwd` override, or a workspace with
    // no repo scope to give a main checkout — in which case the spawn
    // falls back to an isolated tree and must NOT wear the "main" badge.
    let (cwd_path, owning_session, landed_on_main): (
        PathBuf,
        Option<lazybox_core::SessionId>,
        bool,
    ) = if let Some(c) = cwd.as_deref() {
        (PathBuf::from(c), session_id, on_main)
    } else {
        // Race provisioning against a `CancelSpawn` on this claim: Esc
        // on the setup checklist must abort a wedged cold clone —
        // dropping the future kills the git child's whole process
        // group — and release the claim (guard drop at return) so a
        // retry starts fresh, instead of the clone running on and
        // every later spawn collapsing onto it (issue #403).
        let resolve =
            resolve_or_create_session(config, &session_key, session_id, &kind, on_main, origin);
        let cancel = inflight.cancel.clone();
        let resolved = tokio::select! {
            res = resolve => Some(res),
            _ = cancel.notified() => None,
        };
        let Some(resolved) = resolved else {
            tracing::info!(%session_key, "handle_spawn: cancelled while provisioning — aborting");
            // Unstick any client still showing this spawn's checklist
            // (the canceller's is already dismissed; another attached
            // client's would otherwise spin forever).
            emit_worktree_progress(
                config,
                &session_key,
                WorktreeStep::Clone,
                WorktreeStepStatus::Failed(lazybox_ipc::SPAWN_CANCELLED_NOTE.into()),
                origin,
            );
            return None;
        };
        match resolved {
            Ok((path, sid, landed)) => (path, Some(sid), landed),
            Err(e) => {
                // Label a worktree-provisioning failure distinctly so the
                // client routes it to the recovery modal without having to
                // re-classify the free-text message (#594) — every
                // provisioning failure is wrapped as `ServerError::Worktree`
                // above. A session/workspace race keeps `spawn:session`,
                // which the client footers as before.
                let source = match e {
                    crate::ServerError::Worktree(_) => "spawn:worktree",
                    _ => "spawn:session",
                };
                let _ = config
                    .bus
                    .send(Event::provider_error_permanent(source, e.to_string()));
                return None;
            }
        }
    };
    // Session resolution can atomically rebadge an already-running managed
    // branch owner onto this PR workspace. The first singleton check ran
    // before that transfer, when the terminal still belonged to the old
    // workspace key, so re-check now. Without this, `w` correctly preserved
    // the old agent but immediately launched a duplicate beside it.
    if let Some(existing) =
        find_existing_singleton(config, &session_key, &kind, Some(landed_on_main)).await
    {
        if terminal_access_for(config, existing).await != access {
            let _ = config.bus.send(Event::provider_error_permanent(
                "spawn",
                "an agent with a different host-access policy is already running in this checkout",
            ));
            return None;
        }
        if let Some(prompt) = initial_prompt.as_deref() {
            Box::pin(handle_inject_prompt(config, existing, prompt, None, true)).await;
        }
        let _ = config.bus.send(Event::TerminalFocusRequested {
            terminal_id: existing,
        });
        outcome.complete();
        return Some(existing);
    }
    // Session + worktree are resolved here — for a fresh issue this is
    // where a cold clone / `git fetch` / setup script gets paid
    // synchronously, so surfacing the elapsed time makes the otherwise-
    // silent worktree provisioning cost visible.
    tracing::info!(
        elapsed_ms = t0.elapsed().as_millis(),
        "handle_spawn: session/worktree resolved",
    );
    // From this final revalidation through terminal registration, serialize
    // with every workspace move/delete. Provisioning stays outside the lock,
    // but a merge that won during that slow phase makes this fresh load fail
    // (or no longer contain the selected session) instead of letting a stale
    // terminal register under a deleted source workspace.
    let workspace_registration_guard = if cwd.is_none() {
        let workspace_key = WorkspaceKey::new(session_key.as_str());
        let guard = config.lock_workspace(workspace_key.as_str()).await;
        let workspace = match load_workspace(config, &workspace_key) {
            Ok(workspace) => workspace,
            Err(error) => {
                let _ = config.bus.send(Event::provider_error_permanent(
                    "spawn:workspace",
                    format!("spawn target changed while provisioning: {error}"),
                ));
                return None;
            }
        };
        let requires_persisted_session = !landed_on_main
            && !workspace_key.as_str().starts_with("sandbox-")
            && owning_session.is_some();
        if requires_persisted_session
            && !owning_session.is_some_and(|id| workspace.find_session(id).is_some())
        {
            let _ = config.bus.send(Event::provider_error_permanent(
                "spawn:session",
                "spawn target session moved while provisioning; retry from its current workspace",
            ));
            return None;
        }
        Some(guard)
    } else {
        None
    };
    // Allocate the terminal id up front — the per-session hook settings
    // file path is derived from it, and that path must be in the argv
    // before the backend spawns. (The auxiliary/primary map inserts
    // still happen below, after the backend spawn.)
    let terminal_id = alloc_terminal_id(&*config.store);
    // For agents that report state through structured lifecycle hooks
    // (Claude), generate a per-session settings file wiring our hook
    // command into every tracked event, then launch with `--settings`.
    // Agents without hook support get `None` and keep PTY detection.
    // Phase 1 of 2: placeholder hook command — the real one needs the
    // backend key, which only exists after `backend.spawn` (see
    // `write_hook_settings`). Skipped entirely when the running binary
    // is no longer on disk (`cargo clean` under a `cargo run` daemon):
    // hooks would be guaranteed to fail, so the session keeps PTY
    // detection instead. The stable `<home>/bin/lazybox` copy that makes
    // this path survive a rebuild/clean/worktree-removal (#856) is
    // established once at daemon boot by `ensure_stable_hook_exe`, so this
    // hot path only *reads* it — a per-spawn copy would block every
    // `TerminalSpawned` on ~80 MB of IO.
    let exe = hook_exe();
    if exe.is_none() {
        tracing::warn!(
            ?terminal_id,
            "hooks: lazybox binary path is unresolvable or no longer on disk — \
             skipping hooks; agent state falls back to PTY detection"
        );
    }
    let hook_settings = exe.as_deref().and_then(|exe| {
        write_hook_settings(config, &kind, terminal_id, &hook_command_placeholder(exe))
    });
    // Correlated hook command for an argv-hooked agent (Codex). Reads its
    // backend key from a per-terminal file written post-spawn — argv is
    // fixed at launch, so the key can't be embedded inline like Claude's
    // rewritten settings file. Only agents that override
    // `hook_command_args` actually consume it; others ignore it.
    let argv_hook_command = exe
        .as_deref()
        .map(|exe| hook_command_keyfile(exe, &hook_backend_key_path(terminal_id)));
    let shell_command = if matches!(kind, TerminalKind::Shell) {
        cfg.shell.resolved_command()
    } else {
        String::new()
    };
    let agent_worktree = if matches!(kind, TerminalKind::Agent(_)) {
        std::fs::canonicalize(&cwd_path).unwrap_or_else(|_| cwd_path.clone())
    } else {
        cwd_path.clone()
    };
    let plan_error_source = format!("spawn:{kind:?}");
    let repo_env = collect_repo_env(config, &session_key);
    let plan = match build_spawn_plan(
        SpawnPlanInput {
            session_key,
            kind,
            cwd: cwd_path,
            agent_worktree,
            owning_session,
            initial_prompt,
            terminal_id,
            hook_settings,
            hook_command: argv_hook_command,
            repo_env,
            priority_model_alias,
            autonomous,
            landed_on_main,
            model_alias,
            resume,
            provider_session_id,
            no_permission_override,
            replace_terminal_id,
            prompt_history,
            composing_buffer,
            access,
            shell_command,
        },
        &cfg,
        &config.agents,
    ) {
        Ok(plan) => plan,
        Err(_) => {
            let _ = config.bus.send(Event::provider_error_permanent(
                &plan_error_source,
                "no agent registered for this id",
            ));
            return None;
        }
    };
    let executed = match execute_spawn_plan(config, plan, workspace_registration_guard, t0).await {
        Ok(SpawnExecutionOutcome::Spawned(executed)) => executed,
        Ok(SpawnExecutionOutcome::Cancelled) => return None,
        Err(error) => {
            tracing::error!("handle_spawn: spawn execution failed: {error}");
            let _ = config
                .bus
                .send(Event::provider_error_permanent("spawn", error.to_string()));
            return None;
        }
    };
    let ExecutedSpawn {
        backend_key,
        session_key,
        kind,
        initial_prompt,
        terminal_id,
        state_durability,
    } = executed;
    outcome.complete();

    // Pump backend output → bus. Also runs agent-state detection
    // on each chunk so the user sees a "needs input" badge when
    // Claude/Codex is waiting on an approval prompt. State is
    // cached per-terminal so we only broadcast on transitions.
    let bus = config.bus.clone();
    let backend = config.backend.clone();
    let terminal_registry = config.terminal.clone();
    let state_durability_for_pump = state_durability;
    // Whole-config clone for the shared exit teardown
    // (`teardown_exited_terminal`) — it sweeps every per-terminal map
    // and the persisted kv rows, so it takes the config rather than a
    // dozen individually cloned Arcs.
    let config_for_pump = config.clone();
    let id_for_pump = terminal_id;
    let key_for_pump = backend_key.clone();
    let agent_for_pump: Option<std::sync::Arc<dyn lazybox_agents::Agent>> = match &kind {
        TerminalKind::Agent(id) => config.agents.get(id),
        _ => None,
    };
    // Clone before the pump task takes ownership of `agent_for_pump`;
    // the post-spawn `initial_prompt` injector needs its own handle.
    let agent_for_inject = agent_for_pump.clone();
    // Signaled by the pump task on first detected output. The
    // initial-prompt injector waits on this with a timeout, replacing
    // the old 50ms-poll-on-shared-Mutex loop that competed with the
    // pump's `agent_states_map.lock()` write path.
    let first_output_signal = std::sync::Arc::new(tokio::sync::Notify::new());
    let first_output_signal_for_pump = first_output_signal.clone();
    let first_output_signal_for_inject = first_output_signal.clone();
    // Fired the first time the pump's detector sees the agent's
    // input box drawn AND no permission gate up — i.e. the agent
    // is *actually* ready to receive a pasted prompt. The inject
    // task waits on this so we no longer settle blindly past a
    // permission gate (the original "y eats my prompt" race) NOR
    // wait the full ASKING_DEADLINE on Claude's normal idle screen
    // (the "60s before inject" symptom from dogfood).
    let ready_signal = std::sync::Arc::new(tokio::sync::Notify::new());
    let ready_signal_for_pump = ready_signal.clone();
    let ready_signal_for_inject = ready_signal.clone();
    let session_key_for_pump = session_key.clone();
    // `Instant` is Copy, so both the pump and inject tasks get their own
    // copy of the spawn origin for the timing trace.
    let t0_for_pump = t0;
    let watchdog_after = working_watchdog_after(&cfg);
    let quiet_after = pty_quiet_classify_after(&cfg);
    tokio::spawn(async move {
        let sub = match backend.subscribe(&key_for_pump).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("backend subscribe {key_for_pump}: {e}");
                None
            }
        };
        // Run the pump only when subscribe succeeded; either way fall
        // through to the teardown below. A subscribe failure here lands
        // *after* TerminalSpawned was broadcast, so skipping teardown
        // would leave a phantom terminal entry that satisfies the
        // singleton guard forever and blocks respawn (`w`/`c`/`x`).
        let exit_code = async {
            let mut sub = sub?;
            // Per-terminal rolling buffer for state detection. Bumped
            // from 4 KiB to 32 KiB after a real bug: Claude's status-
            // bar updates (token counter / elapsed-time ticker) emit
            // tiny chunks that — with a small buffer — pushed the
            // "Esc to cancel · Tab to amend" footer out of scope.
            // detect_state then returned Active on the next chunk, the
            // pill flickered off, and the user couldn't tell Claude
            // still needed input. 32 KiB spans many minutes of status
            // ticks.
            const STATE_BUF_CAP: usize = 32 * 1024;
            let mut state_buf: Vec<u8> = Vec::with_capacity(STATE_BUF_CAP);
            // The terminal's lifecycle state machine. It owns the transition
            // table (the allowed edges, `Done` and `InputNeeded` stickiness):
            // an ambiguous byte-flow reading can never clear a finished `Done`
            // or a parked `?`, so a busy/waiting agent never flickers to Idle
            // when Claude's status line drops for a single chunk or a click
            // triggers a repaint. Every PTY reading commits through it; the
            // current state itself lives in the shared `agent_states` cache.
            let mut state_machine = lazybox_agents::AgentStateMachine::new();

            // Notify the initial-prompt injector exactly once when the
            // first byte of output arrives. `notify_one` STORES a permit
            // if no one is waiting yet, so we don't race the inject task's
            // `.notified()` registration — the permit is consumed when
            // the inject task starts waiting, even if pump runs first.
            let mut signaled_first_output = false;
            // `detect_ready_for_prompt` is the tight "agent's input box
            // is drawn AND no permission gate is up" signal. Fire it
            // ONCE — extra notifications are harmless but redundant
            // (`Notify` permits stack). The inject task only needs to
            // know "we reached ready at least once."
            let mut signaled_ready = false;
            let mut auth_required_emitted = false;
            let check_ready = |state_buf: &Vec<u8>,
                               last_chunk_len: usize,
                               signaled: &mut bool,
                               signal: &tokio::sync::Notify| {
                if *signaled {
                    return;
                }
                let Some(agent) = agent_for_pump.as_ref() else {
                    return;
                };
                // Same DETECT_WINDOW the pump's state detector uses —
                // covers the visible-screen tail without scanning
                // long-stale boot output.
                const DETECT_WINDOW: usize = 16 * 1024;
                let tail = &state_buf[state_buf.len().saturating_sub(DETECT_WINDOW)..];
                // Chunk-boundary hint within the tail slice: repaint-heavy
                // agents (Codex) judge readiness from the latest frame
                // rather than the append-only history (issue #425).
                let chunk_start = tail.len().saturating_sub(last_chunk_len);
                if agent.detect_ready_for_prompt_chunked(tail, chunk_start) {
                    // Time-to-ready is the first (and normally dominant)
                    // stage of the spawn→inject pipeline — log it so a slow
                    // inject can be attributed (issue #425).
                    tracing::info!(
                        terminal_id = ?id_for_pump,
                        elapsed_ms = t0_for_pump.elapsed().as_millis(),
                        "agent composer ready for prompt",
                    );
                    // `notify_one` STORES a permit when no waiter is
                    // registered yet; `notify_waiters` is edge-triggered
                    // and a ready signal fired before the inject task
                    // started waiting was lost forever, riding the
                    // inject path to its hard deadline.
                    signal.notify_one();
                    *signaled = true;
                }
            };
            // Quiet-classification timer (#289). Re-armed on every chunk;
            // when it fires — PTY_QUIET_CLASSIFY_AFTER with no output —
            // the resting screen is classified. While chunks flow, the
            // reading is `Working` unless an adapter recognizes distinctive
            // prompt chrome in the current chunk (see `note_pty_activity`).
            // Never armed for non-agent terminals (no detector to run).
            let mut quiet_deadline: Option<tokio::time::Instant> = None;
            // Working watchdog (#398): meaningful content changes move both
            // its stability origin and deadline. Firing moves only the next
            // check, so telemetry retains the true no-progress duration.
            let mut working_watchdog = WorkingWatchdog::new(
                agent_for_pump.as_ref().and(watchdog_after),
            );
            let mut watchdog_fp: Option<u64> = None;
            // The last time any byte arrived — moves with `quiet_deadline`,
            // not with the content fingerprint. Feeds the #538 status
            // telemetry: "time in Working after the last real output" is the
            // distribution the 5s quiet / 15s watchdog defaults should be
            // tuned against, and a `pty-watchdog-force` settle whose
            // `elapsed_since_output_ms` is tiny is the signature of a
            // keepalive-painting agent (gap 1).
            let mut last_output_at = tokio::time::Instant::now();
            // Length of the most recent chunk appended to `state_buf` —
            // the chunk-boundary hint the quiet classifier's same-chunk
            // rule needs.
            let mut last_chunk_len: usize = 0;
            if !sub.replay.is_empty() {
                let progress = agent_for_pump.is_some()
                    && watchdog_notes_progress(&mut watchdog_fp, &sub.replay);
                note_pty_activity(
                    agent_for_pump.as_ref(),
                    &mut state_buf,
                    &sub.replay,
                    sub.last_seq,
                    progress,
                    &terminal_registry,
                    &bus,
                    state_durability_for_pump.as_ref(),
                    id_for_pump,
                    &session_key_for_pump,
                    &mut state_machine,
                )
                .await;
                maybe_emit_auth_required(
                    &config_for_pump,
                    agent_for_pump.as_ref(),
                    &state_buf,
                    id_for_pump,
                    &mut auth_required_emitted,
                )
                .await;
                last_chunk_len = sub.replay.len();
                if agent_for_pump.is_some() {
                    last_output_at = tokio::time::Instant::now();
                    quiet_deadline = Some(last_output_at + quiet_after);
                    if progress {
                        working_watchdog.note_progress(last_output_at);
                    }
                }
                if sub.replay_complete {
                    let _ = bus.send(Event::TerminalOutput {
                        terminal_id: id_for_pump,
                        bytes: sub.replay.clone(),
                        first_seq: 1,
                        seq: sub.last_seq,
                    });
                } else {
                    tracing::warn!(
                        terminal_id = ?id_for_pump,
                        seq = sub.last_seq,
                        "initial replay prefix was truncated; not publishing a false baseline"
                    );
                }
                // Permit-storing, like the live first-output path below — a
                // replay that lands before the inject task registers its
                // waiter must not be lost.
                first_output_signal_for_pump.notify_one();
                signaled_first_output = true;
                check_ready(
                    &state_buf,
                    last_chunk_len,
                    &mut signaled_ready,
                    &ready_signal_for_pump,
                );
                // The composer being drawn is the "agent has booted" signal
                // the state machine needs to stop holding byte-flow `Working`
                // as boot chrome — crucial for the autonomous flow, whose
                // work prompt is injected during boot, before the first quiet
                // classification could latch it (#357).
                if signaled_ready {
                    state_machine.mark_booted();
                }
            }
            // High-water mark of everything delivered downstream so far
            // — the replay's `last_seq` at subscribe time, then advanced
            // per forwarded chunk (or to the ring's seq after a gap
            // resync).
            let mut last_seq = sub.last_seq;
            let mut resync_unavailable_announced = false;
            // On-demand reclassify poke (#869). A deferred prompt injection
            // asks the pump to re-read the live screen so a stale
            // `InputNeeded` — one whose gate already cleared without further
            // output to re-arm the quiet timer — releases without waiting for
            // a keystroke to drive the transition.
            let reclassify = terminal_registry.register_reclassify(id_for_pump).await;
            loop {
                let watchdog_due = working_watchdog
                    .prepare_select(tokio::time::Instant::now(), sub.live.len());
                tokio::select! {
                    // Pending output is drained before the quiet timer may
                    // classify, so a racing chunk cannot leave the classifier
                    // one frame behind. At the content-stability deadline the
                    // pump drains exactly the bounded batch that was already
                    // queued. New traffic cannot extend that batch, so repaint
                    // bytes cannot starve the watchdog; meaningful queued
                    // output can still move its anchor before classification.
                    biased;
                    chunk = sub.live.recv(), if working_watchdog.receiver_enabled(watchdog_due) => {
                let Some(chunk) = chunk else {
                    break;
                };
                working_watchdog.note_received(watchdog_due);
                // `subscribe` subscribes before snapshotting, so a live
                // chunk already covered by the replay (seq within the
                // snapshot's high-water mark) must be dropped to avoid
                // re-feeding the detector and re-emitting bytes.
                if chunk.seq <= last_seq {
                    continue;
                }
                if chunk.seq > last_seq.saturating_add(1) {
                    // A chunk was dropped between the backend's reader
                    // and this pump (bounded bridge overflow or broadcast
                    // lag). The byte stream now has a hole — forwarding
                    // the remainder would permanently desync every
                    // client's VT parser (a torn escape sequence garbles
                    // the grid). Resynchronize from the replay ring: the
                    // reader pushes to the ring BEFORE broadcasting, so a
                    // snapshot taken now covers both the dropped chunk
                    // and this one. Clients replace the terminal's state
                    // with the replay (`TerminalResync`), and the
                    // state-detection buffer is rebuilt from the same
                    // bytes so it can't scrape the torn stream either.
                    let Some(snapshot) =
                        resync_replay_after_gap(&*backend, &key_for_pump, chunk.seq, last_seq)
                            .await
                    else {
                        // Preserve the last coherent detector/client state.
                        // Do not advance `last_seq`; the next delivered
                        // chunk exposes the same debt and retries.
                        if !resync_unavailable_announced {
                            let _ = bus.send(Event::TerminalResyncUnavailable {
                                terminal_id: id_for_pump,
                            });
                            resync_unavailable_announced = true;
                        }
                        continue;
                    };
                    resync_unavailable_announced = false;
                    state_buf.clear();
                    let progress = agent_for_pump.is_some()
                        && watchdog_notes_progress(&mut watchdog_fp, &snapshot.replay);
                    note_pty_activity(
                        agent_for_pump.as_ref(),
                        &mut state_buf,
                        &snapshot.replay,
                        snapshot.last_seq,
                        progress,
                        &terminal_registry,
                        &bus,
                        state_durability_for_pump.as_ref(),
                        id_for_pump,
                        &session_key_for_pump,
                        &mut state_machine,
                    )
                    .await;
                    maybe_emit_auth_required(
                        &config_for_pump,
                        agent_for_pump.as_ref(),
                        &state_buf,
                        id_for_pump,
                        &mut auth_required_emitted,
                    )
                    .await;
                    last_chunk_len = snapshot.replay.len();
                    if agent_for_pump.is_some() {
                        last_output_at = tokio::time::Instant::now();
                        quiet_deadline = Some(last_output_at + quiet_after);
                        if progress {
                            working_watchdog.note_progress(last_output_at);
                        }
                    }
                    check_ready(
                        &state_buf,
                        last_chunk_len,
                        &mut signaled_ready,
                        &ready_signal_for_pump,
                    );
                    let _ = bus.send(Event::TerminalResync {
                        terminal_id: id_for_pump,
                        replay: snapshot.replay,
                        seq: snapshot.last_seq,
                    });
                    last_seq = snapshot.last_seq;
                    continue;
                }
                last_seq = chunk.seq;
                // The user just answered an InputNeeded prompt (Enter while
                // the `?` pill was up). Drop the accumulated detection
                // buffer so the just-answered prompt's markers can't
                // re-fire InputNeeded on this fresh chunk. See the
                // `agent_detect_resets` field doc for why this is safe — a
                // prompt that's genuinely still up gets re-rendered and
                // re-detected from the post-answer output.
                //
                // Only agent terminals are ever inserted into the set (the
                // optimistic flip in `handle_write` gates on InputNeeded,
                // which shells never reach), so skip the per-chunk lock
                // entirely for shells. Bind the `remove` result before the
                // `if` so the MutexGuard drops at the `;` rather than being
                // held across the body — the temporary-lifetime footgun the
                // `TERMINAL_MAP_LOCK_ORDER` note warns about (harmless today
                // with no await in the body, a latent deadlock the moment
                // one is added).
                if agent_for_pump.is_some() {
                    let answered = terminal_registry
                        .agent_detect_resets
                        .lock()
                        .await
                        .remove(&id_for_pump);
                    if answered {
                        state_buf.clear();
                        tracing::debug!(
                            terminal_id = ?id_for_pump,
                            "user answered prompt; clearing agent-state detection buffer",
                        );
                    }
                }
                let progress = agent_for_pump.is_some()
                    && watchdog_notes_progress(&mut watchdog_fp, &chunk.bytes);
                note_pty_activity(
                    agent_for_pump.as_ref(),
                    &mut state_buf,
                    &chunk.bytes,
                    chunk.seq,
                    progress,
                    &terminal_registry,
                    &bus,
                    state_durability_for_pump.as_ref(),
                    id_for_pump,
                    &session_key_for_pump,
                    &mut state_machine,
                )
                .await;
                maybe_emit_auth_required(
                    &config_for_pump,
                    agent_for_pump.as_ref(),
                    &state_buf,
                    id_for_pump,
                    &mut auth_required_emitted,
                )
                .await;
                last_chunk_len = chunk.bytes.len();
                if agent_for_pump.is_some() {
                    last_output_at = tokio::time::Instant::now();
                    quiet_deadline = Some(last_output_at + quiet_after);
                    if progress {
                        working_watchdog.note_progress(last_output_at);
                    }
                }
                if !signaled_first_output {
                    first_output_signal_for_pump.notify_one();
                    signaled_first_output = true;
                    tracing::info!(
                        terminal_id = ?id_for_pump,
                        elapsed_ms = t0_for_pump.elapsed().as_millis(),
                        "handle_spawn: first PTY output",
                    );
                }
                check_ready(
                    &state_buf,
                    last_chunk_len,
                    &mut signaled_ready,
                    &ready_signal_for_pump,
                );
                if signaled_ready {
                    state_machine.mark_booted();
                }
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id: id_for_pump,
                    bytes: chunk.bytes,
                    first_seq: chunk.seq,
                    seq: chunk.seq,
                });
                    }
                    // `unwrap_or_else(now)` only feeds the disabled arm —
                    // select! evaluates the expression even when the `if`
                    // precondition is false, it just never polls it.
                    _ = tokio::time::sleep_until(
                        quiet_deadline.unwrap_or_else(tokio::time::Instant::now)
                    ), if quiet_deadline.is_some() && !watchdog_due => {
                        quiet_deadline = None;
                        // #538 status telemetry: the stream went byte-silent.
                        // `elapsed_since_output_ms` is ~the quiet window by
                        // construction; `content_stable_ms` says how long the
                        // meaningful content had already been at rest when
                        // output stopped — the quiet-window tuning signal.
                        // `debug` (behind a dedicated target) so it stays out
                        // of the default log until an operator opts in to
                        // collect the distribution.
                        tracing::debug!(
                            target: "lazybox::agent_status_telemetry",
                            terminal_id = ?id_for_pump,
                            trigger = "quiet-timer",
                            elapsed_since_output_ms = last_output_at.elapsed().as_millis(),
                            content_stable_ms = working_watchdog
                                .content_stable_for(tokio::time::Instant::now())
                                .as_millis(),
                            "quiet classify firing",
                        );
                        classify_quiet_screen(
                            agent_for_pump.as_ref(),
                            &state_buf,
                            last_chunk_len,
                            lazybox_agents::Liveness::Silent,
                            &terminal_registry,
                            &bus,
                            state_durability_for_pump.as_ref(),
                            id_for_pump,
                            &session_key_for_pump,
                            &mut state_machine,
                        )
                        .await;
                    }
                    // Working watchdog (#398): unlike the quiet arm
                    // this one cannot be re-armed by byte flow alone —
                    // only a content-fingerprint change moves
                    // the stability origin — so a spinner-alive-but-idle
                    // agent still gets classified and forced out of
                    // `Working`. A no-op tick (terminal not Working,
                    // or a concurrent state change) just schedules the next
                    // check without erasing the content-stability age.
                    _ = tokio::time::sleep_until(
                        working_watchdog.deadline()
                            .unwrap_or_else(tokio::time::Instant::now)
                    ), if working_watchdog.deadline().is_some() => {
                        let fired_at = tokio::time::Instant::now();
                        let Some((watchdog_window, content_stable)) =
                            working_watchdog.fire(fired_at)
                        else {
                            continue;
                        };
                        let elapsed_since_output = last_output_at.elapsed();
                        // #538 status telemetry. Only meaningful while the
                        // turn is actually `Working`: the watchdog arm re-arms
                        // every window regardless of state, so an idle/done
                        // terminal would otherwise emit a line every 15s
                        // forever. Gate on the cached state (the same one the
                        // watchdog itself acts on) and keep it at `debug`
                        // behind the dedicated target. A small
                        // `elapsed_since_output_ms` here means bytes were still
                        // flowing (a spinner/keepalive/ticker) while the
                        // content stayed put — the gap-1 "keepalive pins
                        // Working" signature; a large one is a genuine silent
                        // stall the quiet timer would also have caught.
                        if terminal_registry
                            .agent_states
                            .lock()
                            .await
                            .get(&id_for_pump)
                            .copied()
                            == Some(lazybox_ipc::AgentState::Working)
                        {
                            if content_stable >= watchdog_window.saturating_mul(2) {
                                tracing::warn!(
                                    target: "lazybox::agent_status_telemetry",
                                    terminal_id = ?id_for_pump,
                                    reason = "pty-watchdog-force",
                                    elapsed_since_output_ms = elapsed_since_output.as_millis(),
                                    content_stable_ms = content_stable.as_millis(),
                                    watchdog_ms = watchdog_window.as_millis(),
                                    "working terminal exceeded twice the content-stability watchdog",
                                );
                            }
                            tracing::debug!(
                                target: "lazybox::agent_status_telemetry",
                                terminal_id = ?id_for_pump,
                                trigger = "working-watchdog",
                                reason = "pty-watchdog-force",
                                elapsed_since_output_ms = elapsed_since_output.as_millis(),
                                content_stable_ms = content_stable.as_millis(),
                                "working watchdog firing",
                            );
                        }
                        watchdog_reverify_parked_turn(
                            agent_for_pump.as_ref(),
                            &state_buf,
                            last_chunk_len,
                            &terminal_registry,
                            &bus,
                            state_durability_for_pump.as_ref(),
                            id_for_pump,
                            &session_key_for_pump,
                            &mut state_machine,
                        )
                        .await;
                    }
                    // On-demand reclassify (#869): a deferred inject asks the
                    // pump to re-read the resting screen NOW rather than wait
                    // up to the quiet window — or forever, once that timer has
                    // disarmed on a quiescent terminal. `force_reclassify_allowed`
                    // gates it on a settled stream and a non-reset terminal so
                    // the poke can't scrape a torn frame or preempt the
                    // reset-latched settle with a spurious Done.
                    _ = reclassify.notified() => {
                        if force_reclassify_allowed(
                            agent_for_pump.is_some(),
                            last_output_at,
                            &terminal_registry,
                            id_for_pump,
                        )
                        .await
                        {
                            classify_quiet_screen(
                                agent_for_pump.as_ref(),
                                &state_buf,
                                last_chunk_len,
                                lazybox_agents::Liveness::Silent,
                                &terminal_registry,
                                &bus,
                                state_durability_for_pump.as_ref(),
                                id_for_pump,
                                &session_key_for_pump,
                                &mut state_machine,
                            )
                            .await;
                        }
                    }
                }
            }
            backend.wait_exit(&key_for_pump).await
        }
        .await;
        teardown_exited_terminal(&config_for_pump, id_for_pump, &key_for_pump, exit_code).await;
    });

    // Schedule prompt injection. Drives the `f`-for-fix flow: the
    // sidebar / activity pane spawn an agent with a pre-built
    // instruction so the user doesn't have to retype it.
    //
    // Wait for the agent's composer before writing. Typing into a full-screen
    // agent during banner boot drops keystrokes onto the wrong UI surface and
    // the prompt ends up half-eaten. The PTY protocol declares whether
    // composer readiness is authoritative or a timed fallback is acceptable.
    if let (Some(prompt), Some(agent)) = (initial_prompt, &agent_for_inject) {
        let requires_ready = agent.pty_protocol().requires_ready();
        let encoded_prompt = agent.encode_prompt(&prompt, lazybox_agents::PromptIntent::Submit);
        let initial_write_len = encoded_prompt.initial_write_len();
        let backend_key = backend_key.clone();
        let id = terminal_id;
        let first_output = first_output_signal_for_inject;
        let ready_signal = ready_signal_for_inject;
        let t0_for_inject = t0;
        let config_for_inject = config.clone();
        tokio::spawn(async move {
            // Wait for the agent's input box to be drawn AND no
            // permission gate to be up — i.e. "the agent is genuinely
            // ready to receive a pasted prompt." The pump task fires
            // `ready_signal` exactly once when `Agent::
            // detect_ready_for_prompt` first returns true. This is
            // strictly tighter than the previous "wait for not-
            // Asking" approach: the loose Asking detector matched
            // a normal idle screen and made the wait spin
            // the full deadline before every inject.
            //
            // Fallback ladder (each step has its own deadline):
            //   1. ready_signal — preferred path, fires within
            //      seconds of the agent finishing its banner.
            //   2. first_output + SETTLE — for agents whose
            //      detector never reports ready (default impl),
            //      we still write 600ms past first byte. Agents
            //      with an authoritative readiness detector
            //      (`PtyProtocol::requires_ready`) SKIP this rung — a
            //      blind settle-write would land the paste in
            //      a folder-trust prompt if it's still up.
            //   3. HARD_DEADLINE — last resort, inject blindly so
            //      a cold-start hang doesn't silently lose the
            //      user's prompt.
            const HARD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
            const SETTLE: std::time::Duration = std::time::Duration::from_millis(600);
            tracing::info!(
                terminal_id = ?id,
                initial_write_len,
                "initial_prompt: waiting for agent ready signal",
            );

            let trigger = await_inject_window(
                requires_ready,
                &ready_signal,
                &first_output,
                HARD_DEADLINE,
                SETTLE,
            )
            .await;
            // The deadline rung means `ready` never fired within
            // HARD_DEADLINE. For an agent with an authoritative readiness
            // detector that does NOT mean "safe to paste": under
            // many concurrent spawns the pump lags behind the deadline, or
            // the agent is still parked on a boot-time gate (folder-trust /
            // login / bypass chooser). A blind paste here lands the
            // work-context prompt in a half-drawn screen or, with its
            // follow-up `\r`, ANSWERS the gate with it — the prompt is lost
            // and the user has no signal it happened. So instead of dropping
            // (the old `GATE_CAP` path) or pasting blindly, keep the prompt
            // pending and deliver it the moment the agent genuinely reaches
            // ready, bounded by terminal liveness AND `PENDING_READY_CAP` —
            // a flaky readiness detector must not silently turn the inject
            // into an unbounded wait (issue #425). The bare-deadline
            // blind paste is kept for detector-less agents (`requires_ready`
            // false), whose `ready` signal never fires — losing the prompt
            // to a cold-start hang is worse there than a best-effort paste.
            if trigger == InjectTrigger::Deadline && requires_ready {
                let pending_t0 = std::time::Instant::now();
                match await_pending_ready(
                    id,
                    &ready_signal,
                    &config_for_inject.terminal.terminals,
                    PENDING_READY_CAP,
                )
                .await
                {
                    PendingReady::Ready => {
                        tracing::info!(
                            terminal_id = ?id,
                            waited_ms = pending_t0.elapsed().as_millis(),
                            "initial_prompt: agent reached ready past the hard deadline",
                        );
                    }
                    PendingReady::TerminalGone => {
                        tracing::warn!(
                            terminal_id = ?id,
                            "initial_prompt: terminal exited before the agent became ready — work prompt not delivered"
                        );
                        let _ = config_for_inject.bus.send(Event::TerminalInputRejected {
                            terminal_id: id,
                            message: "agent never became ready — press w again to retry".into(),
                        });
                        return;
                    }
                    PendingReady::Capped => {
                        tracing::warn!(
                            terminal_id = ?id,
                            waited_ms = pending_t0.elapsed().as_millis(),
                            "initial_prompt: agent never reported ready within the bounded wait — work prompt not delivered"
                        );
                        let _ = config_for_inject.bus.send(Event::TerminalInputRejected {
                            terminal_id: id,
                            message: format!(
                                "agent did not become ready within {}s — press w again to retry",
                                (HARD_DEADLINE + PENDING_READY_CAP).as_secs()
                            ),
                        });
                        return;
                    }
                }
            }
            tracing::info!(
                terminal_id = ?id,
                initial_write_len,
                ?trigger,
                elapsed_ms = t0_for_inject.elapsed().as_millis(),
                "initial_prompt: inject window cleared — writing prompt sequence to backend",
            );
            let Some(interaction) =
                terminal_io::acquire_live(&config_for_inject, id, &backend_key).await
            else {
                tracing::warn!(
                    terminal_id = ?id,
                    "initial_prompt: terminal exited before the prompt interaction began"
                );
                let _ = config_for_inject.bus.send(Event::TerminalInputRejected {
                    terminal_id: id,
                    message: "agent terminal closed before the work prompt landed — press w again to retry"
                        .into(),
                });
                return;
            };
            match write_prompt_sequence(
                &config_for_inject,
                id,
                &backend_key,
                encoded_prompt,
                true,
                interaction,
            )
            .await
            {
                Ok(_) => {}
                Err(PromptWriteError::Initial(e)) => {
                    tracing::warn!(terminal_id = ?id, "initial_prompt: initial write failed: {e}");
                    let _ = config_for_inject.bus.send(Event::TerminalInputRejected {
                        terminal_id: id,
                        message: format!(
                            "work prompt was not delivered ({e}) — press w again to retry"
                        ),
                    });
                }
                Err(PromptWriteError::Submit(e)) => {
                    tracing::warn!(terminal_id = ?id, "initial_prompt: submit failed: {e}");
                    let _ = config_for_inject.bus.send(Event::TerminalInputRejected {
                        terminal_id: id,
                        message: format!(
                            "work prompt was pasted but could not be submitted ({e}) — open the terminal and press Enter"
                        ),
                    });
                }
            }
        });
    }
    Some(terminal_id)
}

/// Kill a backend process that finished spawning after its workspace entered
/// deletion. Returns true when the caller must abort terminal registration.
/// The pre-registration location is load-bearing: after this returns true no
/// terminal map, persistence row, output pump, or `TerminalSpawned` event has
/// been created, so teardown has nothing partial to reconcile.
async fn cancel_spawn_for_deleted_workspace(
    config: &ServerConfig,
    session_key: &SessionKey,
    backend_key: &str,
) -> bool {
    if !config
        .deleted_workspaces
        .lock()
        .contains(session_key.as_str())
    {
        return false;
    }

    tracing::warn!(
        workspace = session_key.as_str(),
        backend_key,
        "spawn completed after workspace deletion began — terminating before registration",
    );
    if let Err(error) = config.backend.kill(backend_key).await {
        tracing::error!(
            workspace = session_key.as_str(),
            backend_key,
            %error,
            "late spawn cancellation failed; backend may require manual cleanup",
        );
        let _ = config.bus.send(Event::provider_error_permanent(
            "spawn",
            format!(
                "workspace {} was deleted, but its late terminal {backend_key} could not be stopped: {error}",
                session_key.as_str()
            ),
        ));
    }
    true
}

/// Which rung of the inject ladder released the spawn-time paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectTrigger {
    /// The pump's `detect_ready_for_prompt` signal fired — input box
    /// drawn, no permission / trust gate up.
    Ready,
    /// First-output + settle fallback (agents without an authoritative
    /// readiness detector).
    Settle,
    /// `hard_deadline` elapsed; we paste blindly rather than lose the
    /// prompt to a cold-start hang.
    Deadline,
}

/// Block until it's safe to paste the spawn-time prompt.
///
/// Two regimes, chosen by `requires_ready`:
///
/// - `true` — the agent has an authoritative readiness detector
///   (Claude: input box drawn AND no folder-trust / permission gate).
///   Wait for the pump's `ready` signal and fall back to
///   `hard_deadline` only as a last resort. The time-based settle is
///   deliberately NOT honored here: if the folder-trust prompt is
///   still on screen when the settle timer expires, a blind paste
///   types the work-context prompt into the trust dialog instead of
///   the input box.
/// - `false` — detector-less agents whose `detect_ready_for_prompt`
///   never reports ready. Race `ready` against a first-output +
///   `settle` timer so the prompt still injects promptly.
async fn await_inject_window(
    requires_ready: bool,
    ready: &tokio::sync::Notify,
    first_output: &tokio::sync::Notify,
    hard_deadline: std::time::Duration,
    settle: std::time::Duration,
) -> InjectTrigger {
    let ready_notify = ready.notified();
    if requires_ready {
        return match tokio::time::timeout(hard_deadline, ready_notify).await {
            Ok(()) => InjectTrigger::Ready,
            Err(_) => InjectTrigger::Deadline,
        };
    }
    let first_output_notify = first_output.notified();
    tokio::select! {
        r = tokio::time::timeout(hard_deadline, ready_notify) => match r {
            Ok(()) => InjectTrigger::Ready,
            Err(_) => InjectTrigger::Deadline,
        },
        _ = async {
            let _ = tokio::time::timeout(hard_deadline, first_output_notify).await;
            tokio::time::sleep(settle).await;
        } => InjectTrigger::Settle,
    }
}

/// Upper bound on the post-deadline pending-ready park. Combined with the
/// inject `HARD_DEADLINE` this is the total worst-case wait before a
/// spawn-time prompt fails loudly instead of parking forever behind a
/// readiness detector that never fires against a repainting TUI
/// (issue #425).
const PENDING_READY_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// How a parked spawn-time prompt's pending-ready wait resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingReady {
    /// The pump's composer-ready signal fired — deliver the prompt now.
    Ready,
    /// The terminal exited (or never finished booting) — nothing left to
    /// deliver to; the caller surfaces the failure.
    TerminalGone,
    /// The bounded wait elapsed with no ready signal — fail loudly rather
    /// than park the prompt indefinitely.
    Capped,
}

/// Park a pending spawn-time prompt until the agent is genuinely ready to
/// receive it, instead of dropping it or pasting blindly past the inject
/// deadline.
///
/// `ready` is the pump's one-shot composer-drawn signal; it fires when the
/// agent leaves any boot-time gate (folder-trust / login / bypass chooser)
/// AND its input box is drawn, so waiting on it subsumes the old
/// gate-polling loop. The 1s poll re-checks terminal liveness so a terminal
/// that exits ends the wait rather than leaking the task — the pump removes
/// its `terminals` entry on exit. `cap` bounds the total wait: a readiness
/// detector that never fires must not silently become an unbounded park.
async fn await_pending_ready(
    id: TerminalId,
    ready: &tokio::sync::Notify,
    terminals: &tokio::sync::Mutex<std::collections::HashMap<TerminalId, String>>,
    cap: std::time::Duration,
) -> PendingReady {
    let cap_at = tokio::time::Instant::now() + cap;
    loop {
        if !terminals.lock().await.contains_key(&id) {
            return PendingReady::TerminalGone;
        }
        let now = tokio::time::Instant::now();
        if now >= cap_at {
            return PendingReady::Capped;
        }
        let poll = std::cmp::min(
            std::time::Duration::from_secs(1),
            cap_at.duration_since(now),
        );
        if tokio::time::timeout(poll, ready.notified()).await.is_ok() {
            return PendingReady::Ready;
        }
    }
}

/// Look up the session whose worktree this Spawn should land in.
///
/// - `Some(session_id)` → look it up in the workspace, error if it's
///   gone (rare race where the user removed the session between
///   selecting it and pressing the spawn key).
/// - `None` → use `Workspace::default_session`, or adopt/create one
///   when the workspace is empty. Adoption and creation emit
///   `Event::SessionCreated` so the sidebar's expansion-on-multi-
///   session UI reacts.
async fn resolve_or_create_session(
    config: &ServerConfig,
    session_key: &SessionKey,
    session_id: Option<SessionId>,
    kind: &TerminalKind,
    on_main: bool,
    origin: lazybox_ipc::SpawnOrigin,
) -> Result<(PathBuf, SessionId, bool), crate::ServerError> {
    let workspace_key = WorkspaceKey::new(session_key.as_str());

    // Sandbox workspaces (key prefix `sandbox-`) live in a
    // dedicated per-workspace directory at `paths::sandbox_dir(key)`.
    // No worktree provisioning — the dir is just a plain mkdir from
    // `create_sandbox_workspace`. Sessions all share that directory.
    if workspace_key.as_str().starts_with("sandbox-") {
        let path = lazybox_core::paths::sandbox_dir(workspace_key.as_str());
        // Best-effort mkdir in case the user removed the dir between
        // sandbox creation and spawn. Failure logs but doesn't abort.
        if let Err(e) = std::fs::create_dir_all(&path) {
            tracing::warn!(
                sandbox = %path.display(),
                "sandbox dir create_dir_all failed at spawn time: {e}",
            );
        }
        return Ok((path, session_id.unwrap_or_else(SessionId::new), false));
    }

    // Every non-sandbox spawn without an explicit cwd must resolve to
    // a persisted workspace. Falling back to the daemon's cwd is unsafe:
    // a stale key or store failure could otherwise launch an agent in
    // whichever repository happened to start the daemon. The tombstone
    // only improves the error for the delete-vs-spawn race.
    let workspace = match load_workspace(config, &workspace_key) {
        Ok(w) => w,
        Err(error) => {
            if config
                .deleted_workspaces
                .lock()
                .contains(workspace_key.as_str())
            {
                tracing::warn!(
                    workspace = workspace_key.as_str(),
                    "spawn: workspace was deleted while the spawn was in flight — aborting",
                );
                return Err(crate::ServerError::Workspace(format!(
                    "workspace {} was deleted — spawn aborted",
                    workspace_key.as_str()
                )));
            }
            return Err(error);
        }
    };

    // Linked (no-worktree) workspace: every session lands directly in
    // the user's existing checkout on whatever branch it already sits
    // on. No worktree is provisioned (the checkout already exists on
    // disk), no bare clone, and the branch is never switched. Reported
    // as "on main" so it reuses the shared-checkout machinery — one
    // agent singleton per checkout, shells share it, the auto-fix guard
    // tracks it — matching the "one checkout, multiple tasks share it"
    // contract. Takes precedence over the `on_main` request flag and an
    // explicit `session_id`, since a linked workspace has no isolated
    // per-session worktrees to target.
    if let Some(path) = workspace.linked_checkout.clone() {
        return Ok((path, SessionId::new(), true));
    }

    // Main-checkout spawn: skip the isolated per-session worktree and
    // land in the repo's shared checkout on its default branch. The
    // path is stable per repo (`<root>/<scope>/_main`) so every
    // main-checkout session across the repo's workspaces reuses one
    // worktree — that's the point (it IS the shared main). No `Session`
    // is persisted for it: the singleton guard handles agent reuse and
    // the deterministic path handles shells, so an ephemeral session id
    // is enough for `terminal_sessions`. Repo-less / standalone
    // workspaces have no scope and no meaningful "main", so they fall
    // through to normal isolated provisioning.
    if on_main && let Some(path) = main_worktree_path(&workspace) {
        // A failed main-checkout provision FAILS THE SPAWN. The old
        // fallback (`mkdir` an empty dir and carry on) fabricated a
        // directory that masqueraded as the shared main checkout —
        // the terminal opened in a non-git folder and the agent's
        // first `git` command was the only thing that noticed.
        if let Err(e) =
            provision_worktree(config, &workspace, &path, session_key, true, None, origin).await
        {
            tracing::warn!("main-checkout worktree provisioning failed: {e}");
            // Land the ✗ on the checklist row that actually aborted
            // (issue #557 B, acceptance #2) rather than always "Cloning".
            let message = e.to_string();
            emit_worktree_progress(
                config,
                session_key,
                lazybox_ipc::WorktreeRecovery::classify(&message).failed_step(),
                WorktreeStepStatus::Failed(message),
                origin,
            );
            return Err(crate::ServerError::Worktree(format!(
                "main checkout setup failed — spawn aborted, retry once the cause is fixed: {e}"
            )));
        }
        return Ok((path, SessionId::new(), true));
    }

    if let Some(id) = session_id {
        let session = workspace.find_session(id).ok_or_else(|| {
            crate::ServerError::Workspace(format!("session {id:?} not in workspace"))
        })?;
        ensure_worktree_present(
            config,
            &workspace,
            &session.worktree_path,
            session.worktree_branch.as_deref(),
            session_key,
            origin,
        )
        .await?;
        return Ok((session.worktree_path.clone(), session.id, false));
    }
    if let Some(session) = workspace.default_session() {
        ensure_worktree_present(
            config,
            &workspace,
            &session.worktree_path,
            session.worktree_branch.as_deref(),
            session_key,
            origin,
        )
        .await?;
        return Ok((session.worktree_path.clone(), session.id, false));
    }

    // Workspace exists but has no sessions yet — provision one.
    // Worktree path is human-readable: `<root>/<workspace_slug>` for
    // the first session, `<root>/<workspace_slug>-2` for the second,
    // etc. The slug is derived from the PR (PR-{n}-{title-slug}) or
    // from the user-supplied workspace name when the workspace is
    // pre-PR. `Session.id` stays a UUID for stable internal identity;
    // only the path is human-friendly.
    let kind_for_session = session_kind_from_terminal(kind);
    let path = worktree_path_for_session(&workspace, 0);
    let ownership_guard = config.worktree_ownership_lock.lock().await;
    if let Some((adopted_path, session_id)) = recover_untracked_pr_worktree_locked(
        config,
        &workspace_key,
        &kind_for_session,
        &path,
        session_key,
        origin,
    )
    .await?
    {
        return Ok((adopted_path, session_id, false));
    }
    let _provisioning_claim = ProvisioningWorktreeClaim::new(config, path.clone());
    drop(ownership_guard);

    let prov_start = std::time::Instant::now();
    let provisioned =
        provision_worktree(config, &workspace, &path, session_key, false, None, origin).await;
    tracing::info!(
        elapsed_ms = prov_start.elapsed().as_millis(),
        ok = provisioned.is_ok(),
        worktree = %path.display(),
        "provision_worktree complete",
    );
    let provisioned_branch = match provisioned {
        Ok(branch) => branch,
        Err(e) => {
            // Real-checkout failed (no GH access, branch missing, network
            // hiccup) — FAIL THE SPAWN. The old fallback persisted the
            // session anyway and `mkdir`'d an empty dir "so spawn works",
            // which pinned the session to a non-git directory forever:
            // every later spawn saw the path existed and never re-ran the
            // repair machinery. No session is persisted here, so the next
            // `w` press retries the full provision from scratch.
            tracing::warn!("worktree provisioning failed: {e}");
            // Surface the failure in the progress modal too, so a cold
            // clone that can't reach GitHub shows the error instead of the
            // checklist hanging on a forever spinner. The checkout sub-phases
            // (clone/fetch/worktree-add) are the only ones that abort
            // provisioning; mounts/scripts are best-effort. The modal freezes
            // on whichever step is on screen, so the exact variant here only
            // names where in the checklist the ✗ lands. The returned error
            // additionally lands as a `spawn:session` provider error via
            // `handle_spawn`'s resolve arm.
            // Classify the failure so the ✗ lands on the phase that actually
            // aborted (clone/fetch/worktree-add) instead of always "Cloning"
            // (issue #557 B, acceptance #2). The modal re-derives the same
            // class from this message to render its recovery affordance.
            let message = e.to_string();
            emit_worktree_progress(
                config,
                session_key,
                lazybox_ipc::WorktreeRecovery::classify(&message).failed_step(),
                WorktreeStepStatus::Failed(message),
                origin,
            );
            return Err(crate::ServerError::Worktree(format!(
                "git worktree setup failed — spawn aborted, retry once the cause is fixed: {e}"
            )));
        }
    };

    // Provisioning above intentionally runs without the workspace lock. Once
    // it finishes, serialize the fresh load→session insert→commit so a
    // concurrent issue→PR move cannot delete the source and then have this
    // stale spawn recreate it from its pre-provision snapshot.
    let _workspace_guard = config.lock_workspace(workspace_key.as_str()).await;
    let mut workspace = load_workspace(config, &workspace_key)?;
    if let Some(session) = workspace.default_session() {
        return Ok((session.worktree_path.clone(), session.id, false));
    }
    let mut session = Session::new(
        workspace_key.clone(),
        kind_for_session,
        path.clone(),
        Utc::now(),
    );
    session.worktree_branch = Some(provisioned_branch);
    let new_session_id = session.id;
    workspace.add_session(session.clone());
    persist_and_broadcast(config, &workspace).await?;
    let _ = config.bus.send(Event::SessionCreated(Box::new(session)));
    Ok((path, new_session_id, false))
}

async fn recover_untracked_pr_worktree_locked(
    config: &ServerConfig,
    workspace_key: &WorkspaceKey,
    kind: &SessionKind,
    intended_path: &Path,
    session_key: &SessionKey,
    origin: lazybox_ipc::SpawnOrigin,
) -> Result<Option<(PathBuf, SessionId)>, crate::ServerError> {
    let workspace_guard = config.lock_workspace(workspace_key.as_str()).await;
    let mut workspace = load_workspace(config, workspace_key)?;
    if let Some(session) = workspace.default_session() {
        return Ok(Some((session.worktree_path.clone(), session.id)));
    }

    let Some(task) = workspace.primary_task().filter(|task| task.is_pr()) else {
        return Ok(None);
    };
    let (Some(repo), Some(branch)) = (task.repo.clone(), task.branch.clone()) else {
        return Ok(None);
    };
    let Some((owner, name)) = repo.split_once('/') else {
        return Ok(None);
    };

    let candidates = match config
        .worktree_manager()
        .managed_worktrees_for_branch(owner, name, &branch)
        .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(
                workspace = workspace_key.as_str(),
                repo,
                branch,
                "could not inspect managed worktrees before provisioning: {error}"
            );
            return Ok(None);
        }
    };
    let [candidate] = candidates.as_slice() else {
        return Ok(None);
    };
    if provisioning_worktree_is_claimed(config, candidate)
        || managed_worktree_has_live_main_owner(config, candidate).await
    {
        return Ok(None);
    }

    // A uniquely-owned managed checkout is not an external git conflict.
    // It is the same durable Lazybox session wearing an obsolete workspace
    // badge (most commonly an issue badge after its PR appeared). Move that
    // session record and rebadge its live terminal atomically instead of
    // trying to create a second checkout for the already-held branch.
    //
    // The persisted `worktree_branch` is the proof that Lazybox provisioned
    // this exact branch. Legacy/foreign records without that proof retain the
    // explicit conflict flow rather than being adopted speculatively.
    if let Some(owner) =
        unique_transferable_managed_session_owner(config, candidate, &branch, workspace_key)
    {
        drop(workspace_guard);
        let transferred = crate::polling::transfer_owned_worktree_session(
            config,
            &owner.workspace_key,
            workspace_key,
            owner.session_id,
            candidate,
            &branch,
        )
        .await
        .map_err(|error| {
            crate::ServerError::Store(format!(
                "transfer managed session onto PR workspace: {error}"
            ))
        })?;
        if let Some(session) = transferred {
            tracing::info!(
                source_workspace = %owner.workspace_key,
                target_workspace = %workspace_key,
                session_id = %session.id,
                branch,
                worktree = %session.worktree_path.display(),
                "transferred managed branch owner onto PR workspace"
            );
            return Ok(Some((session.worktree_path, session.id)));
        }
        // Ownership changed while the two workspace locks were acquired.
        // Fall through to provisioning, which will either see the newly
        // established owner or surface the normal safe conflict.
        return Ok(None);
    }

    if managed_worktree_has_session_owner(config, candidate) {
        return Ok(None);
    }
    if paths_match(candidate, intended_path) {
        return Ok(None);
    }

    let worktree = lazybox_git_ops::Worktree {
        name: branch.clone(),
        path: candidate.clone(),
        branch: branch.clone(),
    };
    apply_worktree_setup(
        config,
        &config.worktree_manager(),
        &worktree,
        Some(&repo),
        session_key,
        origin,
    )
    .await;

    let mut session = Session::new(
        workspace_key.clone(),
        kind.clone(),
        candidate.clone(),
        Utc::now(),
    );
    session.worktree_branch = Some(branch.clone());
    let session_id = session.id;
    workspace.add_session(session.clone());
    persist_and_broadcast(config, &workspace).await?;
    let _ = config.bus.send(Event::SessionCreated(Box::new(session)));
    tracing::info!(
        workspace = workspace_key.as_str(),
        repo,
        branch,
        worktree = %candidate.display(),
        "adopted untracked managed worktree for PR"
    );
    Ok(Some((candidate.clone(), session_id)))
}

#[derive(Debug)]
struct TransferableManagedSessionOwner {
    workspace_key: WorkspaceKey,
    session_id: SessionId,
}

/// Find the sole daemon-provisioned session that owns `candidate`.
///
/// Automatic rebadging is deliberately narrower than generic path matching:
/// the source must contain exactly one session and that session must persist
/// the exact requested branch. `TerminalsRebadged` is workspace-scoped, so
/// moving a source with sibling sessions would incorrectly move their client
/// tabs too; those ambiguous cases keep the manual recovery flow.
fn unique_transferable_managed_session_owner(
    config: &ServerConfig,
    candidate: &Path,
    branch: &str,
    target_workspace_key: &WorkspaceKey,
) -> Option<TransferableManagedSessionOwner> {
    let records = config.store.list_workspaces().ok()?;
    let mut owner = None;
    for record in records {
        let json = record.workspace_json?;
        let workspace = Workspace::decode_persisted(&json).ok()?;
        let matching = workspace
            .sessions
            .iter()
            .filter(|session| paths_match(&session.worktree_path, candidate))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            continue;
        }
        if matching.len() != 1
            || workspace.sessions.len() != 1
            || workspace.key == *target_workspace_key
            || matching[0].worktree_branch.as_deref() != Some(branch)
            || owner.is_some()
        {
            return None;
        }
        owner = Some(TransferableManagedSessionOwner {
            workspace_key: workspace.key,
            session_id: matching[0].id,
        });
    }
    owner
}

pub(crate) fn managed_worktree_has_session_owner(config: &ServerConfig, candidate: &Path) -> bool {
    managed_worktree_has_matching_or_unknown_owner(config, candidate, |_| true)
}

pub(crate) fn managed_worktree_has_live_session_owner(
    config: &ServerConfig,
    candidate: &Path,
) -> bool {
    managed_worktree_has_matching_or_unknown_owner(config, candidate, |session| {
        !matches!(session.state, lazybox_core::SessionRunState::Stopped)
    })
}

fn managed_worktree_has_matching_or_unknown_owner(
    config: &ServerConfig,
    candidate: &Path,
    blocks: impl Fn(&Session) -> bool,
) -> bool {
    let records = match config.store.list_workspaces() {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(
                worktree = %candidate.display(),
                "could not verify worktree ownership before adoption: {error}"
            );
            return true;
        }
    };
    for record in records {
        let Some(json) = record.workspace_json else {
            return true;
        };
        let workspace = match Workspace::decode_persisted(&json) {
            Ok(workspace) => workspace,
            Err(error) => {
                tracing::warn!(
                    workspace = record.key,
                    "could not verify worktree ownership from stored workspace: {error}"
                );
                return true;
            }
        };
        if workspace
            .sessions
            .iter()
            .any(|session| paths_match(&session.worktree_path, candidate) && blocks(session))
        {
            return true;
        }
    }
    false
}

fn paths_match(left: &Path, right: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

pub(crate) fn session_paths_match(left: &Path, right: &Path) -> bool {
    paths_match(left, right)
}

struct ProvisioningWorktreeClaim {
    claims: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<PathBuf, usize>>>,
    path: PathBuf,
}

impl ProvisioningWorktreeClaim {
    fn new(config: &ServerConfig, path: PathBuf) -> Self {
        *config
            .provisioning_worktree_claims
            .lock()
            .entry(path.clone())
            .or_default() += 1;
        Self {
            claims: config.provisioning_worktree_claims.clone(),
            path,
        }
    }
}

impl Drop for ProvisioningWorktreeClaim {
    fn drop(&mut self) {
        let mut claims = self.claims.lock();
        let Some(count) = claims.get_mut(&self.path) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            claims.remove(&self.path);
        }
    }
}

fn provisioning_worktree_is_claimed(config: &ServerConfig, candidate: &Path) -> bool {
    config
        .provisioning_worktree_claims
        .lock()
        .keys()
        .any(|path| paths_match(path, candidate))
}

async fn reclaim_non_live_managed_holder(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    owner: &str,
    repo: &str,
    branch: &str,
    holder: &Path,
    intended_path: &Path,
) -> BranchHolderReclaim {
    let _ownership_guard = config.worktree_ownership_lock.lock().await;
    reclaim_non_live_managed_holder_locked(config, mgr, owner, repo, branch, holder, intended_path)
        .await
}

async fn reclaim_non_live_managed_holder_locked(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    owner: &str,
    repo: &str,
    branch: &str,
    holder: &Path,
    intended_path: &Path,
) -> BranchHolderReclaim {
    if managed_worktree_has_live_session_owner(config, holder)
        || provisioning_worktree_is_claimed(config, holder)
        || managed_worktree_has_live_main_owner(config, holder).await
    {
        return BranchHolderReclaim::Preserved;
    }

    match mgr
        .reclaim_managed_worktree_if_safe(owner, repo, branch, holder)
        .await
    {
        Ok(lazybox_git_ops::WorktreeReclaimOutcome::Reclaimed) => {
            tracing::info!(
                repo = %format!("{owner}/{repo}"),
                branch,
                holder = %holder.display(),
                intended = %intended_path.display(),
                "reclaimed non-live branch holder before provisioning"
            );
            BranchHolderReclaim::Reclaimed
        }
        Ok(lazybox_git_ops::WorktreeReclaimOutcome::Blocked(blocker)) => {
            BranchHolderReclaim::Blocked(blocker)
        }
        Ok(lazybox_git_ops::WorktreeReclaimOutcome::NotManaged) => BranchHolderReclaim::Preserved,
        Err(error) => {
            tracing::warn!(
                repo = %format!("{owner}/{repo}"),
                branch,
                holder = %holder.display(),
                "could not reclaim non-live branch holder: {error}"
            );
            BranchHolderReclaim::Blocked(lazybox_git_ops::WorktreeReclaimBlocker::SafetyCheckFailed)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchHolderReclaim {
    Reclaimed,
    Preserved,
    Blocked(lazybox_git_ops::WorktreeReclaimBlocker),
}

async fn managed_worktree_has_live_main_owner(config: &ServerConfig, candidate: &Path) -> bool {
    let inflight_main_workspaces: Vec<String> = config
        .spawn
        .inflight_spawns
        .lock()
        .iter()
        .filter(|(_, (_, on_main))| *on_main)
        .map(|((workspace, _), _)| workspace.clone())
        .collect();
    if inflight_main_workspaces.into_iter().any(|workspace_key| {
        load_workspace(config, &WorkspaceKey::new(workspace_key))
            .ok()
            .and_then(|workspace| main_worktree_path(&workspace))
            .is_some_and(|path| paths_match(&path, candidate))
    }) {
        return true;
    }

    let on_main = config.terminal.on_main_terminals.lock().await.clone();
    if on_main.is_empty() {
        return false;
    }
    let meta = config.terminal.terminal_meta.lock().await.clone();
    on_main.into_iter().any(|terminal_id| {
        meta.get(&terminal_id)
            .and_then(|(session_key, _)| {
                load_workspace(config, &WorkspaceKey::new(session_key.as_str())).ok()
            })
            .and_then(|workspace| main_worktree_path(&workspace))
            .is_some_and(|path| paths_match(&path, candidate))
    })
}

/// Build a deterministic branch name for a task that has no upstream
/// branch (issues, Linear tickets, future provider-specific items).
/// Deterministic on the task (id + title) so two spawns on the same
/// issue map to the same local branch — otherwise pressing the spawn
/// key twice would leave two orphan branches, neither push-ready.
///
/// The name reads naturally in the target repo: an issue number plus a
/// slug of its title, so a reviewer recognizes the branch at a glance.
/// `prefix` is the resolved `worktree.branch_prefix` (per-repo override
/// applied by the caller) — empty by default, so no tool branding
/// leaks in.
///
/// Examples (empty default prefix):
/// - `github:owner/repo#42` "Fix the thing" → `issue-42-fix-the-thing`
/// - `linear:ENG-456` "Ship it"             → `linear-eng-456-ship-it`
/// - title with no usable chars             → `issue-42`
fn derive_branch_for_branchless(prefix: &str, task: &Task) -> String {
    let source = task.id.source.to_ascii_lowercase();
    let raw_key = &task.id.key;

    let issue_number = (source == "github")
        .then(|| raw_key.rsplit_once('#').map(|(_, n)| n))
        .flatten()
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    let stem = match issue_number {
        Some(number) => format!("issue-{number}"),
        None => format!("{source}-{}", sanitize_branch_component(raw_key)),
    };

    let title_slug = lazybox_core::slug::slugify(&task.title);
    let stem = if title_slug.is_empty() {
        stem
    } else {
        format!("{stem}-{title_slug}")
    };

    join_branch_prefix(prefix, &stem)
}

/// Branch name for a blank workspace (no linked task at all).
/// Deterministic on the workspace key for the same reason
/// [`derive_branch_for_branchless`] is deterministic on the task id:
/// repeated spawns on the same workspace reuse one branch.
fn derive_branch_for_workspace(prefix: &str, workspace: &Workspace) -> String {
    join_branch_prefix(prefix, &sanitize_branch_component(workspace.key.as_str()))
}

/// Resolve the branch prefix for a worktree: a per-repo
/// `repos.<owner/name>.branch_prefix` override when set, otherwise the
/// global `worktree.branch_prefix`. `repo_key` is `None` for
/// standalone (repo-less) worktrees, which only see the global value.
fn resolve_branch_prefix<'a>(cfg: &'a lazybox_config::Config, repo_key: Option<&str>) -> &'a str {
    repo_key
        .and_then(|key| cfg.repos.get(key))
        .and_then(|repo| repo.branch_prefix.as_deref())
        .unwrap_or(&cfg.worktree.branch_prefix)
}

/// Join a (possibly empty) prefix to a derived branch component with a
/// `/` separator. The prefix is sanitized like any other component but
/// keeps `/` so multi-segment prefixes (`team/feature`) survive; an
/// empty prefix yields the bare component (`issue-42`).
fn join_branch_prefix(prefix: &str, rest: &str) -> String {
    let prefix: String = prefix
        .chars()
        .map(|c| match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '-' | '_' | '/' => c,
            _ => '-',
        })
        .collect();
    let prefix = prefix.trim_matches(|c| c == '-' || c == '/');
    if prefix.is_empty() {
        rest.to_string()
    } else {
        format!("{prefix}/{rest}")
    }
}

fn sanitize_branch_component(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .collect();
    sanitized.trim_matches('-').to_string()
}

fn isolated_branch_for_workspace(
    workspace: &Workspace,
    cfg: &lazybox_config::Config,
    repo_key: Option<&str>,
) -> String {
    if let Some(branch) = workspace
        .primary_task()
        .and_then(|task| task.branch.as_deref())
    {
        return branch.to_string();
    }
    let prefix = resolve_branch_prefix(cfg, repo_key);
    match workspace.primary_task() {
        // A Linear ticket honors the house branch template when one is
        // configured, falling back to the generic branchless naming.
        Some(task) if task.id.source == "linear" => derive_linear_branch(cfg, task)
            .unwrap_or_else(|| derive_branch_for_branchless(prefix, task)),
        Some(task) => derive_branch_for_branchless(prefix, task),
        None => derive_branch_for_workspace(prefix, workspace),
    }
}

fn repo_for_workspace_provision(
    config: &ServerConfig,
    workspace: &Workspace,
    cfg: &lazybox_config::Config,
) -> Result<Option<String>, crate::ServerError> {
    match workspace.primary_task() {
        // A Linear ticket's `repo` is the synthetic `linear/<team>`, never
        // a clonable GitHub repo — resolve the real one from the team map
        // (or fail loudly) instead of returning `task.repo` verbatim.
        Some(task) if task.id.source == "linear" => linear_repo_for_task(cfg, task).map(Some),
        Some(task) => Ok(task.repo.clone()),
        None if lazybox_core::workspace_project_key(workspace)
            .is_some_and(|key| key.source_prefix() == "github") =>
        {
            let github_scopes = crate::polling::github_scopes_from_config(cfg);
            Ok(Some(clonable_repo_from_project(
                config,
                workspace,
                Some(&github_scopes),
            )?))
        }
        None => Ok(None),
    }
}

/// Resolve the real GitHub `owner/repo` a Linear ticket should be worked
/// in. A Linear task's own `repo` is the synthetic `linear/<team>` (never
/// clonable), so it is routed as follows:
///
/// - An explicit `providers.linear.teams` mapping (team key → `owner/repo`)
///   is a deliberate, trusted signal and wins. A linked GitHub PR *refines*
///   it only when the PR lives under the same owner — the more precise
///   target for a team whose issues span several repos in one org. A
///   foreign-org linked PR never overrides the mapping: `linked_tasks` is
///   built from Linear attachments (`#922`), which are user/bot-controlled
///   data — any `github.com/<owner>/<repo>/pull/<n>` URL, in any org — and
///   must not redirect a configured clone target (issue #944 review).
/// - With no mapping (an unmapped or teamless ticket), a linked GitHub PR
///   is the authoritative repo — this is where linked-PR routing carries
///   its weight, landing the workspace in the repo the work already lives
///   in even for a team lazybox has never been told about.
///
/// A ticket that resolves through neither is a hard error: cloning
/// `linear/<team>` or silently `git init`-ing a standalone worktree would
/// both be wrong (issues #905, #944).
///
/// Note: routing here yields only the repo; the caller cuts a fresh branch
/// (a Linear task carries no `branch`). A linked PR's own head branch is
/// not checked out — that PR lives in a repo outside the user's GitHub
/// scope (else it would be attached and win `primary_task`), and its head
/// ref may be unfetchable.
fn linear_repo_for_task(
    cfg: &lazybox_config::Config,
    task: &Task,
) -> Result<String, crate::ServerError> {
    let linked = linked_github_repo(task);
    let team = task
        .repo
        .as_deref()
        .and_then(|r| r.strip_prefix("linear/"))
        .filter(|t| !t.is_empty());

    if let Some(team) = team
        && let Some(mapped) = cfg.providers.linear.teams.get(team).cloned()
    {
        return Ok(match &linked {
            Some(linked) if same_repo_owner(linked, &mapped) => linked.clone(),
            _ => mapped,
        });
    }

    if let Some(linked) = linked {
        return Ok(linked);
    }

    Err(match team {
        Some(team) => crate::ServerError::Workspace(format!(
            "Linear team `{team}` has no repo mapping and the ticket has \
             no linked GitHub PR — set providers.linear.teams.{team} in \
             ~/.lazybox/config.yaml"
        )),
        None => crate::ServerError::Workspace(
            "this Linear ticket has no team and no linked GitHub PR — \
             cannot resolve a repo to work in"
                .into(),
        ),
    })
}

/// The `owner/repo` of a Linear ticket's linked GitHub PR, if any. The
/// linked ids are GitHub PR `TaskId`s (`owner/repo#N`); when a ticket
/// links several, this is the first in `linked_tasks` order (Linear's
/// attachment order, which carries no guaranteed sort) — the caller
/// disambiguates against the team mapping.
fn linked_github_repo(task: &Task) -> Option<String> {
    task.linked_tasks
        .iter()
        .filter(|id| id.source == "github")
        .find_map(|id| id.key.split_once('#').map(|(repo, _)| repo.to_string()))
}

/// Whether two `owner/repo` strings share their owner segment. Owner
/// comparison is exact (case-sensitive): a case-only mismatch falls back
/// to the trusted config mapping rather than trusting the attachment.
fn same_repo_owner(a: &str, b: &str) -> bool {
    match (a.split_once('/'), b.split_once('/')) {
        (Some((oa, _)), Some((ob, _))) => oa == ob,
        _ => false,
    }
}

/// Branch name for a Linear ticket from the configured
/// `providers.linear.branch_template`. `None` when no template is set
/// (the caller falls back to the generic branchless naming) or the
/// rendered template collapses to nothing.
fn derive_linear_branch(cfg: &lazybox_config::Config, task: &Task) -> Option<String> {
    let linear = &cfg.providers.linear;
    let template = linear.branch_template.as_deref()?;
    let handle = linear
        .handle
        .as_deref()
        .map(lazybox_core::slug::slugify)
        .filter(|h| !h.is_empty())
        .or_else(git_user_handle)
        .unwrap_or_default();
    // Sanitize the configured type token the same way as every other
    // token — a `label_types` value with spaces/case (`"hot fix"`) would
    // otherwise inject an invalid git ref segment.
    let type_token = lazybox_core::branch_template::type_token_for_labels(
        task.labels.iter().map(|l| l.name.as_str()),
        &linear.label_types,
    )
    .map(lazybox_core::slug::slugify)
    .unwrap_or_default();
    let id = lazybox_core::slug::slugify(&task.id.key);
    let slug = lazybox_core::slug::slugify(&task.title);
    lazybox_core::branch_template::render_branch_template(
        template,
        &[
            ("handle", &handle),
            ("type", &type_token),
            ("id", &id),
            ("slug", &slug),
        ],
    )
}

/// The local git `user.name`, slugified, as the `{handle}` fallback when
/// `providers.linear.handle` is unset. `None` when git has no configured
/// name or the command can't run.
fn git_user_handle() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let handle = lazybox_core::slug::slugify(String::from_utf8_lossy(&out.stdout).trim());
    (!handle.is_empty()).then_some(handle)
}

/// `owner/repo` for a workspace with no linked task, recovered from its
/// project. Only `github-` keys carry a clonable repo — `local-`
/// projects legitimately have none and are routed to standalone
/// provisioning before this lookup.
///
/// The clone target must not be reconstructed by splitting the flat
/// `github-{owner}-{repo}` key on `-`: both fields can hold hyphens, so
/// a key like `github-codefly-dev-warden-platform` splits back to the
/// wrong `codefly/dev-warden-platform` and clones a repo that doesn't
/// exist. We recover the exact `owner/repo` from, in order: the user's
/// subscribed scope slug (`github:owner/repo`, unambiguous); the
/// canonical repo identity on the project record; and a key-only slug
/// when the flat key has exactly one possible boundary. `Project.name`
/// is presentation data and is never accepted as clone identity.
/// Ambiguous key-only projects fail instead of cloning a guessed repository.
pub(crate) fn clonable_repo_from_project(
    config: &ServerConfig,
    workspace: &Workspace,
    github_scopes: Option<&std::collections::BTreeSet<String>>,
) -> Result<String, crate::ServerError> {
    let key = lazybox_core::workspace_project_key(workspace).ok_or_else(|| {
        crate::ServerError::Workspace("workspace has no primary task or project".into())
    })?;
    if key.source_prefix() != "github" {
        return Err(crate::ServerError::Workspace(format!(
            "project '{key}' has no repo to clone"
        )));
    }
    if let Some(slug) =
        github_scopes.and_then(|s| key.github_slug_from_scopes(s.iter().map(String::as_str)))
    {
        return Ok(slug);
    }
    let record = config
        .store
        .get_project(&key)
        .map_err(|error| crate::ServerError::Store(format!("load project '{key}': {error}")))?;
    let canonical = record
        .and_then(|record| record.project_json)
        .map(|json| serde_json::from_str::<lazybox_core::Project>(&json))
        .transpose()?
        .and_then(|project| project.github_repo().map(str::to_string))
        .or_else(|| key.unambiguous_github_slug());
    canonical.ok_or_else(|| {
        crate::ServerError::Workspace(format!(
            "project '{key}' has no unambiguous GitHub repo slug"
        ))
    })
}

/// Broadcast a single worktree-provisioning progress transition.
/// Best-effort: a closed bus (no TUI attached) just drops it.
fn emit_worktree_progress(
    config: &ServerConfig,
    session_key: &SessionKey,
    step: WorktreeStep,
    status: WorktreeStepStatus,
    origin: lazybox_ipc::SpawnOrigin,
) {
    let _ = config.bus.send(Event::WorktreeProgress {
        session_key: session_key.clone(),
        step,
        status,
        origin,
    });
}

async fn provision_worktree(
    config: &ServerConfig,
    workspace: &Workspace,
    target: &std::path::Path,
    session_key: &SessionKey,
    on_main: bool,
    existing_branch: Option<&str>,
    origin: lazybox_ipc::SpawnOrigin,
) -> Result<String, crate::ServerError> {
    use crate::ServerError;
    use lazybox_git_ops::CheckoutPhase;

    // Mount the progress modal before the first (possibly slow) git
    // call so the user sees provisioning start immediately rather than
    // after key/repo resolution. `Fetch` is the always-present first
    // sub-phase of "preparing the worktree"; an actual cold clone (when
    // one is needed) arrives as a later `Clone` and upgrades the row's
    // label. Mounting on `Fetch` rather than `Clone` keeps the warm,
    // worktree-add-only path from ever implying a per-workspace clone.
    emit_worktree_progress(
        config,
        session_key,
        WorktreeStep::Fetch,
        WorktreeStepStatus::Started,
        origin,
    );

    // A blank workspace (created via `n` under a project, no issue/PR
    // linked) has no task to read a repo from. GitHub projects must
    // resolve an exact clone identity; local projects have no upstream.
    let task = workspace.primary_task();

    // Map git-ops' clone/fetch/worktree-add boundaries onto
    // `WorktreeProgress` so the long cold-clone phase shows advancing
    // sub-progress instead of one opaque spinner.
    let sink = {
        let bus = config.bus.clone();
        let session_key = session_key.clone();
        std::sync::Arc::new(move |phase: CheckoutPhase| {
            let (step, status) = match phase {
                CheckoutPhase::Cloning => (WorktreeStep::Clone, WorktreeStepStatus::Started),
                // A live transfer-progress line for the in-flight
                // clone: keeps the row's label but updates its detail.
                CheckoutPhase::CloneProgress(line) => {
                    (WorktreeStep::Clone, WorktreeStepStatus::Progress(line))
                }
                CheckoutPhase::Fetching => (WorktreeStep::Fetch, WorktreeStepStatus::Started),
                CheckoutPhase::AddingWorktree => {
                    (WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started)
                }
                // A degraded base-ref fetch: keep the "Preparing worktree"
                // (Fetch) row but flag it so the checklist shows the
                // stale-ref note instead of a silent success.
                CheckoutPhase::BaseRefStale(note) => {
                    (WorktreeStep::Fetch, WorktreeStepStatus::Warned(note))
                }
                // The refresh has been failing for over a day — every
                // worktree since then branched from an aging ref. A
                // checklist note alone proved too easy to dismiss for
                // days (issue #394), so also raise a sticky sync-error
                // banner that outlives the provisioning modal.
                CheckoutPhase::BaseRefStalePersistent(note) => {
                    let _ = bus.send(Event::provider_error(
                        "git:base-ref",
                        note.clone(),
                        lazybox_ipc::ProviderErrorKind::Permanent,
                    ));
                    (WorktreeStep::Fetch, WorktreeStepStatus::Warned(note))
                }
            };
            let _ = bus.send(Event::WorktreeProgress {
                session_key: session_key.clone(),
                step,
                status,
                origin,
            });
        })
    };
    let mgr = config.worktree_manager().with_progress(sink);
    let cfg = lazybox_config::Config::load().unwrap_or_default();

    // The upstream `owner/repo` to clone, when the workspace has one. A
    // task carries it directly; a blank workspace recovers it from a
    // GitHub project key. `None` covers the repo-less cases — a task
    // from a source with no repo (Slack, some Linear tickets), or a
    // blank workspace under a local project — which get a standalone
    // `git init` worktree below instead of an empty, non-git directory.
    let repo = repo_for_workspace_provision(config, workspace, &cfg)?;

    let (worktree, repo_key) = match repo {
        Some(repo) => {
            let (owner, name) = repo.split_once('/').ok_or_else(|| {
                ServerError::Workspace(format!("repo '{repo}' is not owner/name"))
            })?;
            // On-main: ignore the task branch and check out the repo's
            // default branch into the shared main worktree. `checkout_at`
            // is idempotent on the path, so repeated main-checkout spawns
            // reuse the one worktree rather than fighting over the branch.
            let on_main_branch = if on_main {
                Some(
                    mgr.default_branch(owner, name)
                        .await
                        .map_err(|e| ServerError::Worktree(format!("default_branch: {e}")))?,
                )
            } else {
                None
            };
            let worktree = match on_main_branch
                .as_deref()
                .or(existing_branch)
                .or(task.and_then(|t| t.branch.as_deref()))
            {
                Some(branch) => {
                    // Thread the PR number so `checkout_at` can fall back
                    // to `refs/pull/<N>/head` for a head branch that isn't
                    // on `origin` — a fork PR or a deleted head branch
                    // (issue #550). `None` on-main (that branch is always a
                    // plain origin branch) and for non-PR tasks.
                    let pr_number = (!on_main
                        && task.is_some_and(|task| task.branch.as_deref() == Some(branch)))
                    .then(|| task.and_then(Task::pr_number))
                    .flatten();
                    let mut checkout = mgr
                        .checkout_at(target, owner, name, branch, pr_number)
                        .await;
                    let reclaim = match &checkout {
                        Err(lazybox_git_ops::GitError::BranchHeldLive { holder, .. }) => Some((
                            holder.clone(),
                            reclaim_non_live_managed_holder(
                                config, &mgr, owner, name, branch, holder, target,
                            )
                            .await,
                        )),
                        _ => None,
                    };
                    match reclaim {
                        Some((_, BranchHolderReclaim::Reclaimed)) => {
                            checkout = mgr
                                .checkout_at(target, owner, name, branch, pr_number)
                                .await;
                        }
                        Some((holder, BranchHolderReclaim::Blocked(blocker))) => {
                            checkout = Err(lazybox_git_ops::GitError::BranchHeldManaged {
                                branch: branch.to_string(),
                                holder,
                                blocker,
                            });
                        }
                        Some((_, BranchHolderReclaim::Preserved)) => {
                            // The branch is held by a genuinely live checkout we
                            // must not co-opt — another workspace's session, or
                            // an agent that thrashed onto this branch inside its
                            // own worktree (#721). Rather than dead-end the PR's
                            // workspace in a permanent recovery modal, provision a
                            // detached checkout of the head: it lands on the PR's
                            // code without contending for the branch name, leaving
                            // the holder's branch and files untouched.
                            checkout = mgr
                                .checkout_pr_head_detached(target, owner, name, branch, pr_number)
                                .await;
                        }
                        None => {}
                    }
                    checkout.map_err(|e| ServerError::Worktree(format!("checkout_at: {e}")))?
                }
                None => {
                    // Issue (or other branchless task, or blank workspace):
                    // cut a fresh branch off the repo default. Branch name
                    // encodes the task (or the workspace key when there is
                    // no task) so two spawns on the same item land on the same
                    // branch and subsequent presses are idempotent — without
                    // that, pressing `c` twice on issue #42 would create
                    // `issue-42-…` and `issue-42-…-2`, neither of which
                    // corresponds to a PR the user can push.
                    let repo_key = format!("{owner}/{name}");
                    // Reuse this workspace's *own* prior worktree branch
                    // when one is already checked out at the target path,
                    // instead of re-deriving from the (mutable) issue title
                    // (issue #787). A retitled issue would otherwise derive
                    // a different `issue-N-<new-slug>` on the second attempt
                    // and hard-fail `BranchMismatch` against the branch the
                    // first attempt left on disk — thrashing on a workspace's
                    // own leftover. The derive is only for the very first
                    // provision, when nothing is on disk yet; a probe that
                    // errors is surfaced rather than papered over with a
                    // derive that would then mismatch the branch on disk.
                    let new_branch = match mgr.existing_worktree_branch(owner, name, target).await {
                        Ok(Some(branch)) => {
                            tracing::info!(
                                workspace = workspace.key.as_str(),
                                %branch,
                                "reusing the branch already checked out at the target worktree",
                            );
                            branch
                        }
                        Ok(None) => isolated_branch_for_workspace(workspace, &cfg, Some(&repo_key)),
                        Err(e) => {
                            return Err(ServerError::Worktree(format!(
                                "could not read the existing worktree branch at {}: {e}",
                                target.display()
                            )));
                        }
                    };
                    let base = mgr.default_branch(owner, name).await.map_err(|e| {
                        ServerError::Worktree(format!("default_branch lookup: {e}"))
                    })?;
                    let mut checkout = mgr
                        .checkout_new_branch_at(target, owner, name, &new_branch, &base)
                        .await;
                    let reclaim = match &checkout {
                        Err(lazybox_git_ops::GitError::BranchHeldLive { holder, .. }) => Some((
                            holder.clone(),
                            reclaim_non_live_managed_holder(
                                config,
                                &mgr,
                                owner,
                                name,
                                &new_branch,
                                holder,
                                target,
                            )
                            .await,
                        )),
                        _ => None,
                    };
                    match reclaim {
                        Some((_, BranchHolderReclaim::Reclaimed)) => {
                            checkout = mgr
                                .checkout_new_branch_at(target, owner, name, &new_branch, &base)
                                .await;
                        }
                        Some((holder, BranchHolderReclaim::Blocked(blocker))) => {
                            checkout = Err(lazybox_git_ops::GitError::BranchHeldManaged {
                                branch: new_branch.clone(),
                                holder,
                                blocker,
                            });
                        }
                        _ => {}
                    }
                    checkout.map_err(|e| {
                        ServerError::Worktree(format!("checkout_new_branch_at: {e}"))
                    })?
                }
            };
            (worktree, Some(format!("{owner}/{name}")))
        }
        None => {
            // No upstream repo to clone — initialize a standalone git
            // repo on lazybox's branch so the session still lands in a
            // real worktree rather than a bare directory. Branch name is
            // deterministic (same key → same branch) so repeated spawns
            // are idempotent.
            let branch = existing_branch
                .map(str::to_string)
                .unwrap_or_else(|| isolated_branch_for_workspace(workspace, &cfg, None));
            let worktree = mgr
                .init_standalone_at(target, &branch)
                .await
                .map_err(|e| ServerError::Worktree(format!("init_standalone_at: {e}")))?;
            (worktree, None)
        }
    };
    apply_worktree_setup(
        config,
        &mgr,
        &worktree,
        repo_key.as_deref(),
        session_key,
        origin,
    )
    .await;
    Ok(worktree.branch)
}

async fn apply_worktree_setup(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    worktree: &lazybox_git_ops::Worktree,
    repo_key: Option<&str>,
    session_key: &SessionKey,
    origin: lazybox_ipc::SpawnOrigin,
) {
    emit_worktree_progress(
        config,
        session_key,
        WorktreeStep::Setup,
        WorktreeStepStatus::Started,
        origin,
    );

    let cfg = match lazybox_config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                repo = ?repo_key,
                "Config::load failed (mounts will be skipped): {e}",
            );
            lazybox_config::Config::default()
        }
    };
    let mut mounts = config_mounts_to_git(&cfg.worktree.mounts);
    if let Some(repo_key) = repo_key
        && let Some(repo_cfg) = cfg.repos.get(repo_key)
    {
        mounts.extend(config_mounts_to_git(&repo_cfg.mounts));
    }
    let mount_label = repo_key.unwrap_or("standalone");
    if !mounts.is_empty()
        && let Err(e) = mgr.apply_mounts(worktree, &mounts).await
    {
        tracing::warn!("apply_mounts for {mount_label} failed: {e}");
    }

    // Scripts: same stacking as mounts (global + per-repo). Best-
    // effort — a single bad ScriptSpec (e.g. missing source, name
    // collision) logs a warning but doesn't fail the whole spawn.
    // The script that DID validate gets materialized; the one that
    // failed surfaces in /tmp/lazybox.log.
    let mut scripts = config_scripts_to_git(&cfg.worktree.scripts);
    if let Some(repo_key) = repo_key
        && let Some(repo_cfg) = cfg.repos.get(repo_key)
    {
        scripts.extend(config_scripts_to_git(&repo_cfg.scripts));
    }
    if !scripts.is_empty()
        && let Err(e) = mgr.apply_scripts(worktree, &scripts).await
    {
        tracing::warn!("apply_scripts for {mount_label} failed: {e}");
    }

    // Workload bring-up: the per-repo override replaces the global hook
    // wholesale (so the profile can switch per worktree), else the
    // global one runs. This gates the setup step — the agent/shell isn't
    // handed the worktree until `command` returns and, if configured, the
    // readiness probe passes (or the poll times out).
    let bringup = repo_key
        .and_then(|k| cfg.repos.get(k))
        .and_then(|r| r.bringup.clone())
        .or_else(|| cfg.worktree.bringup.clone());
    if let Some(bringup) = bringup {
        let repo_env = repo_key.map(|k| env_for_repo(&cfg, k)).unwrap_or_default();
        let outcome = execute_worktree_bringup(&worktree.path, &bringup, &repo_env, |line| {
            emit_worktree_progress(
                config,
                session_key,
                WorktreeStep::Setup,
                WorktreeStepStatus::Progress(line),
                origin,
            );
        })
        .await;
        if let Some(note) = outcome.warning() {
            tracing::warn!("worktree bring-up for {mount_label} degraded: {note}");
            emit_worktree_progress(
                config,
                session_key,
                WorktreeStep::Setup,
                WorktreeStepStatus::Warned(note),
                origin,
            );
        }
    }

    emit_worktree_progress(
        config,
        session_key,
        WorktreeStep::Setup,
        WorktreeStepStatus::Done,
        origin,
    );
}

/// Result of a workload bring-up run. A degraded outcome ([`Self::warning`]
/// non-`None`) never aborts the spawn — mirroring the best-effort
/// mounts/scripts phases — but it does surface a note the user must
/// acknowledge so a failed `dev up` / never-ready `dev doctor` isn't
/// invisible behind a handed-off session.
#[derive(Debug, PartialEq, Eq)]
enum BringupOutcome {
    /// `command` succeeded and (if configured) the readiness probe passed.
    Ready,
    /// `command` failed to start or exited non-zero.
    CommandFailed(String),
    /// `command` succeeded but the readiness probe never passed before
    /// the timeout.
    NotReady(String),
}

impl BringupOutcome {
    fn warning(&self) -> Option<String> {
        match self {
            Self::Ready => None,
            Self::CommandFailed(m) | Self::NotReady(m) => Some(m.clone()),
        }
    }
}

/// Substitute the chosen profile for a `{profile}` placeholder. The
/// profile is also exported as `LAZYBOX_PROFILE`, so a command can pick
/// whichever form reads better.
fn substitute_profile(command: &str, profile: &str) -> String {
    command.replace("{profile}", profile)
}

/// Build the `sh -c <script>` child every bring-up phase runs. Common to
/// the bring-up command and the readiness probe so both share the same
/// lifecycle guarantees:
///   * `kill_on_drop` — provisioning is raced against `CancelSpawn`
///     ([`handle_spawn`]'s `select!`); an Esc-cancel drops this future,
///     and without kill-on-drop the `dev up` would run on detached and
///     later spawns would collide with the half-built stack (issue #403).
///   * stdio null — the in-process daemon shares the TUI's tty; inherited
///     stdout/stderr would splatter over the alternate screen, and an
///     inherited stdin lets a prompting command block forever.
///   * the worktree's repo env — so bring-up sees the same
///     `repos.<owner/name>.env` the agent/shell that follows will.
fn bringup_command(
    worktree_path: &Path,
    script: &str,
    profile: &str,
    repo_env: &[(String, String)],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    for (k, v) in repo_env {
        cmd.env(k, v);
    }
    // Set last so a bring-up-owned profile wins over any repo-env collision.
    cmd.env("LAZYBOX_PROFILE", profile);
    cmd
}

/// Run a [`WorktreeBringup`] in `worktree_path`: the bring-up command
/// (bounded by `command_timeout_secs`), then (if set) poll the readiness
/// probe until it exits 0 or `readiness_timeout_secs` elapses. Both
/// phases inherit `repo_env` and run through [`bringup_command`], so a
/// dropped/cancelled spawn never leaves a child running. `on_progress`
/// feeds a line per phase into the worktree-progress event stream
/// (consumed by the JSON API gateway and the log; the TUI modal renders
/// the Setup row as a spinner).
async fn execute_worktree_bringup(
    worktree_path: &Path,
    bringup: &lazybox_config::WorktreeBringup,
    repo_env: &[(String, String)],
    mut on_progress: impl FnMut(String),
) -> BringupOutcome {
    let command = substitute_profile(&bringup.command, &bringup.profile);
    on_progress("running workload bring-up".to_string());
    let mut cmd = bringup_command(worktree_path, &command, &bringup.profile, repo_env);
    let status = match tokio::time::timeout(
        Duration::from_secs(bringup.command_timeout_secs),
        cmd.status(),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => {
            return BringupOutcome::CommandFailed(format!(
                "bring-up command timed out after {}s",
                bringup.command_timeout_secs
            ));
        }
    };
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            return BringupOutcome::CommandFailed(match s.code() {
                Some(code) => format!("bring-up command exited with status {code}"),
                None => "bring-up command terminated by signal".to_string(),
            });
        }
        Err(e) => {
            return BringupOutcome::CommandFailed(format!("bring-up command failed to start: {e}"));
        }
    }

    let Some(readiness) = bringup.readiness.as_deref() else {
        return BringupOutcome::Ready;
    };
    let readiness = substitute_profile(readiness, &bringup.profile);
    let interval = Duration::from_secs(bringup.readiness_interval_secs.max(1));
    let poll = async {
        loop {
            on_progress("waiting for workload readiness".to_string());
            let ready = bringup_command(worktree_path, &readiness, &bringup.profile, repo_env)
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);
            if ready {
                break;
            }
            tokio::time::sleep(interval).await;
        }
    };
    match tokio::time::timeout(Duration::from_secs(bringup.readiness_timeout_secs), poll).await {
        Ok(()) => BringupOutcome::Ready,
        Err(_) => BringupOutcome::NotReady(format!(
            "workload not ready after {}s — proceeding anyway",
            bringup.readiness_timeout_secs
        )),
    }
}

/// Convert per-config `MountSpec` → git-ops `Mount`, expanding a
/// leading `~/` in the source path. Kept here so the config crate
/// doesn't need to depend on `dirs` or git-ops.
fn config_mounts_to_git(specs: &[lazybox_config::MountSpec]) -> Vec<lazybox_git_ops::Mount> {
    specs
        .iter()
        .map(|m| lazybox_git_ops::Mount {
            source: expand_tilde(&m.source),
            link_at: m.link_at.clone(),
            placement: match m.placement {
                lazybox_config::PlacementSpec::Inside => lazybox_git_ops::Placement::Inside,
                lazybox_config::PlacementSpec::Above => lazybox_git_ops::Placement::Above,
            },
        })
        .collect()
}

/// Convert per-config `ScriptSpec` → git-ops `Script`, expanding
/// `~/` in source paths. Specs with neither `content` nor `source`
/// set, or with both set, are skipped with a warning — we don't
/// want a bad entry in YAML to abort every script's install.
fn config_scripts_to_git(specs: &[lazybox_config::ScriptSpec]) -> Vec<lazybox_git_ops::Script> {
    specs
        .iter()
        .filter_map(|s| match (&s.content, &s.source) {
            (Some(body), None) => Some(lazybox_git_ops::Script {
                name: s.name.clone(),
                body: lazybox_git_ops::ScriptBody::Inline(body.clone()),
            }),
            (None, Some(path)) => Some(lazybox_git_ops::Script {
                name: s.name.clone(),
                body: lazybox_git_ops::ScriptBody::Linked(expand_tilde(path)),
            }),
            (Some(_), Some(_)) => {
                tracing::warn!(
                    script = %s.name,
                    "script spec has both `content` and `source` — skipping (set exactly one)"
                );
                None
            }
            (None, None) => {
                tracing::warn!(
                    script = %s.name,
                    "script spec has neither `content` nor `source` — skipping"
                );
                None
            }
        })
        .collect()
}

/// Pull `repos.<owner/name>.env` out of YAML for the workspace
/// `session_key` lands in. Returns the (key, value) pairs the
/// backend should set in the spawned PTY. Empty when:
///   * config didn't load,
///   * workspace doesn't exist,
///   * workspace has no primary task / no repo,
///   * no `repos.<owner/name>` block, or
///   * block exists but `env` is empty.
fn collect_repo_env(config: &ServerConfig, session_key: &SessionKey) -> Vec<(String, String)> {
    let workspace_key = WorkspaceKey::new(session_key.as_str());
    let Ok(workspace) = load_workspace(config, &workspace_key) else {
        return Vec::new();
    };
    let Some(repo) = workspace.primary_task().and_then(|t| t.repo.clone()) else {
        return Vec::new();
    };
    let cfg = match lazybox_config::Config::load() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    env_for_repo(&cfg, &repo)
}

/// Whether a client `Spawn` should run as an autonomous, unattended
/// launch. A spawn that carries a pre-built initial prompt is a
/// lazybox-driven "work on this" (the `w` key / address-comments
/// flows) — the same end-state as an `@lazybox` mention — so it runs
/// unattended. Bare interactive spawns (`c` / `x` / `u` / `s`) carry
/// no prompt and stay human-in-the-loop.
pub(crate) fn spawn_is_autonomous(initial_prompt: &Option<String>) -> bool {
    initial_prompt.is_some()
}

/// How long the PTY must stay silent before the resting screen is
/// classified (`classify_quiet_screen`). While bytes are flowing the
/// agent is doing *something*, so the state reading is `Working`;
/// screen-scrape classification (`InputNeeded` / `Done`-adjacent /
/// `Idle`) runs only once the stream has been quiet this long (#289).
/// Claude repaints its status-line ticker about once a second while
/// busy, so a genuinely working agent never goes quiet this long — and
/// a blocking dialog freezes all output, so a parked prompt always
/// does. Default; override with `agent.quiet_classify_secs` (unset or
/// `0` → this default). Unlike the watchdog it can't be disabled — a
/// hookless agent has no other path to `Done`.
pub(crate) const PTY_QUIET_CLASSIFY_AFTER: Duration = Duration::from_secs(5);

/// Fail-safe watchdog for `Working` (#398). The quiet timer above
/// measures time since the last *byte*, so any low-rate repaint — a
/// spinner, a clock, a keepalive — re-arms it forever and
/// `classify_quiet_screen` never runs; with `Working` a one-way door
/// (#357) the terminal is then pinned. The watchdog instead measures
/// time since the last *meaningful* content change
/// ([`lazybox_agents::detect::content_fingerprint`] — repaint churn
/// doesn't reset it) and, once a `Working` terminal has shown none for
/// this long, classifies the screen regardless of byte flow and forces
/// the turn closed ([`watchdog_reverify_parked_turn`]). Default; override
/// with `agent.working_watchdog_secs` (0 disables).
pub(crate) const WORKING_WATCHDOG_AFTER: Duration = Duration::from_secs(15);

#[derive(Debug)]
struct WorkingWatchdog {
    window: Option<Duration>,
    content_stable_since: tokio::time::Instant,
    deadline: Option<tokio::time::Instant>,
    deadline_batch_remaining: Option<usize>,
}

impl WorkingWatchdog {
    fn new(window: Option<Duration>) -> Self {
        let now = tokio::time::Instant::now();
        Self {
            window,
            content_stable_since: now,
            deadline: window.map(|window| now + window),
            deadline_batch_remaining: None,
        }
    }

    fn note_progress(&mut self, at: tokio::time::Instant) {
        self.content_stable_since = at;
        self.deadline = self.window.map(|window| at + window);
        self.deadline_batch_remaining = None;
    }

    fn is_due(&self, now: tokio::time::Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    fn prepare_select(&mut self, now: tokio::time::Instant, queued_chunks: usize) -> bool {
        let due = self.is_due(now);
        if due {
            self.deadline_batch_remaining.get_or_insert(queued_chunks);
        } else {
            self.deadline_batch_remaining = None;
        }
        due
    }

    fn receiver_enabled(&self, due: bool) -> bool {
        !due || self
            .deadline_batch_remaining
            .is_some_and(|remaining| remaining > 0)
    }

    fn note_received(&mut self, due: bool) {
        if due && let Some(remaining) = &mut self.deadline_batch_remaining {
            *remaining = remaining.saturating_sub(1);
        }
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    fn content_stable_for(&self, now: tokio::time::Instant) -> Duration {
        now.saturating_duration_since(self.content_stable_since)
    }

    fn fire(&mut self, now: tokio::time::Instant) -> Option<(Duration, Duration)> {
        let window = self.window?;
        self.deadline = Some(now + window);
        self.deadline_batch_remaining = None;
        Some((window, self.content_stable_for(now)))
    }
}

/// The per-spawn watchdog window: the `agent.working_watchdog_secs`
/// override when set (`0` = disabled → `None`), else the default.
pub(crate) fn working_watchdog_after(cfg: &lazybox_config::Config) -> Option<Duration> {
    match cfg.agent.working_watchdog_secs {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(WORKING_WATCHDOG_AFTER),
    }
}

/// The per-spawn quiet-classify window: the `agent.quiet_classify_secs`
/// override when set to a positive value, else [`PTY_QUIET_CLASSIFY_AFTER`].
/// `0` (or unset) falls back to the default rather than disabling — a
/// zero window would busy-classify every idle loop, and the timer is a
/// hookless agent's only route to `Done`.
pub(crate) fn pty_quiet_classify_after(cfg: &lazybox_config::Config) -> Duration {
    match cfg.agent.quiet_classify_secs {
        Some(secs) if secs > 0 => Duration::from_secs(secs),
        _ => PTY_QUIET_CLASSIFY_AFTER,
    }
}

/// Fold one PTY chunk into the watchdog's meaningful-progress tracker:
/// returns whether `bytes` changed the content fingerprint — real
/// output — as opposed to repaint churn (a spinner frame, a counter
/// tick, cursor noise) that must NOT keep the watchdog at bay.
pub(crate) fn watchdog_notes_progress(last_fp: &mut Option<u64>, bytes: &[u8]) -> bool {
    match lazybox_agents::detect::content_fingerprint(bytes) {
        Some(fp) if *last_fp != Some(fp) => {
            *last_fp = Some(fp);
            true
        }
        _ => false,
    }
}

/// Pure-data lookup so tests don't need a real YAML on disk.
pub(crate) fn env_for_repo(cfg: &lazybox_config::Config, repo: &str) -> Vec<(String, String)> {
    cfg.repos
        .get(repo)
        .map(|rc| rc.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

fn expand_tilde(p: &std::path::Path) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}

/// If a stored Session points at a worktree path the user has since
/// removed (manual `rm -rf`, disk wipe) — or at a directory that is
/// NOT a completed checkout (the empty dir an old failed-provision
/// fallback fabricated, a `.git` dangling after the bare clone was
/// deleted) — restore it through the full provision path, whose
/// `checkout_at` validation owns the reclaim/repair decisions.
///
/// The old guard here was a bare `path.exists()`, which made the
/// git-ops repair machinery unreachable: once anything existed at the
/// path — including a plain empty folder — every later spawn
/// "succeeded" into it forever. The fast path now requires the dir to
/// actually look like a finished checkout.
///
/// Failure fails the spawn (no empty-dir fallback): the caller
/// surfaces it as a `spawn:session` provider error and the session
/// record stays pointed at the path, so the next spawn retries the
/// provision.
async fn ensure_worktree_present(
    config: &ServerConfig,
    workspace: &Workspace,
    path: &std::path::Path,
    expected_branch: Option<&str>,
    session_key: &SessionKey,
    origin: lazybox_ipc::SpawnOrigin,
) -> Result<(), crate::ServerError> {
    let ready = match expected_branch {
        Some(branch) => lazybox_git_ops::worktree_dir_ready_on_branch(path, branch).await,
        None => lazybox_git_ops::worktree_dir_ready(path).await,
    };
    if ready {
        return Ok(());
    }
    tracing::info!(
        worktree = %path.display(),
        ?expected_branch,
        "worktree missing, incomplete, or on the wrong branch — re-provisioning"
    );
    // Re-provisioning a persisted session's worktree — always an
    // isolated per-session tree (main-checkout terminals aren't
    // persisted as sessions).
    if let Err(e) = provision_worktree(
        config,
        workspace,
        path,
        session_key,
        false,
        expected_branch,
        origin,
    )
    .await
    {
        tracing::warn!("re-provision failed: {e}");
        emit_worktree_progress(
            config,
            session_key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Failed(e.to_string()),
            origin,
        );
        return Err(crate::ServerError::Worktree(format!(
            "re-checkout of {} failed — spawn aborted, retry once the cause is fixed: {e}",
            path.display()
        )));
    }
    Ok(())
}

/// Look for an existing terminal in `session_key`'s set whose
/// kind has the same singleton identity as `kind`. Returns the
/// wire-side `TerminalId` so the caller can broadcast a focus
/// request. None when nothing matches OR the kind isn't singleton.
///
/// `pub(crate)` so the polling layer's auto-fix dispatcher can ask
/// "is an agent already running on this PR?" and skip re-acting,
/// using the SAME definition of "already running" that `handle_spawn`
/// uses to collapse duplicate spawns.
pub(crate) async fn find_existing_singleton(
    config: &ServerConfig,
    session_key: &SessionKey,
    kind: &TerminalKind,
    on_main: Option<bool>,
) -> Option<TerminalId> {
    let target = kind.singleton_key()?;
    // A main-checkout agent and the same agent on an isolated worktree
    // are DISTINCT singletons within one workspace — otherwise a `b c`
    // (claude on main) would collapse onto an already-running isolated
    // claude and never reach the main checkout. `handle_spawn` passes
    // `Some(on_main)` to match its exact spawn; the auto-fix guard
    // passes `None` because it asks "is ANY agent already working this
    // PR?" and must skip regardless of which checkout it sits on.
    //
    // The checkout is read from the snapshot's own `on_main` field,
    // computed inside `snapshot_terminals` from the same maps in a
    // consistent order (`on_main_terminals` inserted before
    // `terminals`), so it never disagrees with the terminal it
    // describes. Reading a separate `on_main_terminals` clone here would
    // open a TOCTOU where a terminal that entered the snapshot after the
    // clone is misclassified as isolated.
    let snapshot = snapshot_terminals(config).await;
    snapshot
        .iter()
        .find(|t| {
            t.session_key == *session_key
                && t.kind.singleton_key().as_deref() == Some(&target)
                && !t.authenticating
                && on_main.is_none_or(|want| t.on_main == want)
        })
        .map(|t| t.terminal_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoneGatedPromptOutcome {
    Delivered,
    WaitingForDone,
    DeliveryFailed,
}

#[derive(Clone, PartialEq, Eq)]
struct WorkspaceAgent {
    terminal_id: TerminalId,
    backend_key: String,
    agent_id: String,
    state: Option<lazybox_ipc::AgentState>,
    on_main: bool,
}

async fn workspace_agents(config: &ServerConfig, session_key: &SessionKey) -> Vec<WorkspaceAgent> {
    let terminals = config.terminal.terminals.lock().await.clone();
    let metadata = config.terminal.terminal_meta.lock().await.clone();
    let states = config.terminal.agent_states.lock().await.clone();
    let on_main = config.terminal.on_main_terminals.lock().await.clone();
    let mut agents: Vec<_> = metadata
        .into_iter()
        .filter_map(|(terminal_id, (owner, kind))| {
            if owner != *session_key {
                return None;
            }
            let TerminalKind::Agent(agent_id) = kind else {
                return None;
            };
            Some(WorkspaceAgent {
                terminal_id,
                backend_key: terminals.get(&terminal_id)?.clone(),
                agent_id,
                state: states.get(&terminal_id).copied(),
                on_main: on_main.contains(&terminal_id),
            })
        })
        .collect();
    agents.sort_unstable_by(|left, right| {
        left.backend_key
            .cmp(&right.backend_key)
            .then_with(|| left.terminal_id.0.cmp(&right.terminal_id.0))
    });
    agents
}

fn auto_fix_target<'a>(
    agents: &'a [WorkspaceAgent],
    preferred_agent_id: &str,
) -> Option<&'a WorkspaceAgent> {
    agents.iter().min_by_key(|agent| {
        (
            agent.agent_id != preferred_agent_id,
            agent.on_main,
            agent.agent_id.as_str(),
            agent.terminal_id.0,
        )
    })
}

pub(crate) async fn deliver_auto_fix_prompt(
    config: &ServerConfig,
    session_key: SessionKey,
    preferred_agent_id: String,
    prompt: String,
) -> DoneGatedPromptOutcome {
    let _workspace_agent = config.spawn.lock_workspace_agent(&session_key).await;
    let agents = workspace_agents(config, &session_key).await;
    if agents.is_empty() {
        handle_spawn_inner(
            config,
            session_key.clone(),
            None,
            TerminalKind::Agent(preferred_agent_id.clone()),
            SpawnOptions {
                initial_prompt: Some(prompt),
                autonomous: true,
                origin: lazybox_ipc::SpawnOrigin::Autonomous(
                    lazybox_ipc::AutonomousTrigger::AutoFix,
                ),
                ..Default::default()
            },
        )
        .await;
        return if workspace_agents(config, &session_key)
            .await
            .iter()
            .any(|agent| agent.agent_id == preferred_agent_id)
        {
            DoneGatedPromptOutcome::Delivered
        } else {
            DoneGatedPromptOutcome::DeliveryFailed
        };
    }
    if agents
        .iter()
        .any(|agent| agent.state != Some(lazybox_ipc::AgentState::Done))
    {
        return DoneGatedPromptOutcome::WaitingForDone;
    }

    let mut interactions = Vec::with_capacity(agents.len());
    for agent in &agents {
        let Some(interaction) =
            terminal_io::acquire_live(config, agent.terminal_id, &agent.backend_key).await
        else {
            return DoneGatedPromptOutcome::DeliveryFailed;
        };
        interactions.push((agent.terminal_id, interaction));
    }

    let final_agents = workspace_agents(config, &session_key).await;
    if final_agents
        .iter()
        .any(|agent| agent.state != Some(lazybox_ipc::AgentState::Done))
    {
        return DoneGatedPromptOutcome::WaitingForDone;
    }
    if final_agents
        .iter()
        .map(|agent| {
            (
                agent.terminal_id,
                agent.backend_key.as_str(),
                agent.agent_id.as_str(),
                agent.on_main,
            )
        })
        .ne(agents.iter().map(|agent| {
            (
                agent.terminal_id,
                agent.backend_key.as_str(),
                agent.agent_id.as_str(),
                agent.on_main,
            )
        }))
    {
        return DoneGatedPromptOutcome::DeliveryFailed;
    }

    let Some(target) = auto_fix_target(&final_agents, &preferred_agent_id) else {
        return DoneGatedPromptOutcome::DeliveryFailed;
    };
    let Some(agent) = config.agents.get(&target.agent_id) else {
        tracing::warn!(
            agent_id = %target.agent_id,
            terminal_id = ?target.terminal_id,
            "auto-fix: target agent is not registered"
        );
        return DoneGatedPromptOutcome::DeliveryFailed;
    };
    let Some(position) = interactions
        .iter()
        .position(|(terminal_id, _)| *terminal_id == target.terminal_id)
    else {
        return DoneGatedPromptOutcome::DeliveryFailed;
    };
    let (_, interaction) = interactions.swap_remove(position);
    drop(interactions);
    let encoded = agent.encode_prompt(&prompt, lazybox_agents::PromptIntent::Submit);
    match write_prompt_sequence(
        config,
        target.terminal_id,
        &target.backend_key,
        encoded,
        true,
        interaction,
    )
    .await
    {
        Ok(_) => DoneGatedPromptOutcome::Delivered,
        Err(PromptWriteError::Submit(error)) => {
            tracing::warn!(
                terminal_id = ?target.terminal_id,
                %error,
                "auto-fix: prompt was pasted but submit failed"
            );
            DoneGatedPromptOutcome::Delivered
        }
        Err(PromptWriteError::Initial(error)) => {
            tracing::warn!(
                terminal_id = ?target.terminal_id,
                %error,
                "auto-fix: prompt was not delivered"
            );
            DoneGatedPromptOutcome::DeliveryFailed
        }
    }
}

fn singleton_claim_target(target: String, on_main: bool) -> String {
    if on_main {
        format!("{target}:main")
    } else {
        target
    }
}

async fn terminal_access_for(config: &ServerConfig, terminal_id: TerminalId) -> AgentRunAccess {
    config.terminal.access_for(terminal_id).await
}

/// Releases a claimed in-flight singleton identity when dropped — on
/// EVERY `handle_spawn` exit path (success, session-resolution failure,
/// backend failure, panic) — and pings waiters so collapsing duplicates
/// and `Kill` re-check promptly.
struct InflightSpawnGuard {
    set: std::sync::Arc<
        parking_lot::Mutex<
            std::collections::HashMap<
                (String, String),
                (std::sync::Arc<tokio::sync::Notify>, bool),
            >,
        >,
    >,
    changed: std::sync::Arc<tokio::sync::Notify>,
    key: (String, String),
    /// Pinged by `handle_cancel_spawn`; the owning `handle_spawn` races
    /// its provisioning against it and aborts when it fires.
    cancel: std::sync::Arc<tokio::sync::Notify>,
}

impl InflightSpawnGuard {
    /// Claim an in-flight identity. Singleton kinds (agents) claim
    /// `(workspace key, singleton kind key)`; `Err(())` when another
    /// spawn already holds it. Non-singleton kinds (shells, log tails)
    /// spawn freely — they claim a unique key that can never collide,
    /// so their provision is still cancellable (`CancelSpawn`) and
    /// `Kill`'s teardown still waits for it, without introducing any
    /// duplicate-collapse semantics.
    fn try_claim(
        coordinator: &SpawnCoordinator,
        session_key: &SessionKey,
        kind: &TerminalKind,
        on_main: bool,
    ) -> Result<Self, ()> {
        let (target, exclusive) = match kind.singleton_key() {
            // Fold the checkout into the identity so a main-checkout
            // spawn doesn't race-collapse onto an in-flight isolated
            // spawn of the same agent (mirrors
            // `find_existing_singleton`).
            Some(target) => (singleton_claim_target(target, on_main), true),
            None => {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (format!("nonsingleton:{n}"), false)
            }
        };
        let key = (session_key.as_str().to_string(), target);
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut set = coordinator.inflight_spawns.lock();
        if exclusive && set.contains_key(&key) {
            return Err(());
        }
        set.insert(key.clone(), (cancel.clone(), on_main));
        Ok(Self {
            set: coordinator.inflight_spawns.clone(),
            changed: coordinator.inflight_spawn_changed.clone(),
            key,
            cancel,
        })
    }
}

impl Drop for InflightSpawnGuard {
    fn drop(&mut self) {
        self.set.lock().remove(&self.key);
        self.changed.notify_waiters();
    }
}

/// `Command::CancelSpawn` — the user Esc'd the "Setting up workspace"
/// checklist. Ping every in-flight spawn claim on the workspace; each
/// owning `handle_spawn` aborts its provisioning, which drops (and so
/// kills, process group included) any in-flight `git clone`/`fetch`
/// child and releases the claim so a retry starts fresh. `notify_one`
/// stores a permit, so a cancel landing before the winner reaches its
/// select point is not lost. No-op when nothing is in flight.
pub(crate) fn handle_cancel_spawn(coordinator: &SpawnCoordinator, session_key: &SessionKey) {
    let cancels: Vec<_> = coordinator
        .inflight_spawns
        .lock()
        .iter()
        .filter(|((ws, _), _)| ws == session_key.as_str())
        .map(|(_, (cancel, _))| cancel.clone())
        .collect();
    tracing::info!(
        %session_key,
        claims = cancels.len(),
        "cancel_spawn: signalling in-flight provisions"
    );
    for cancel in cancels {
        cancel.notify_one();
    }
}

/// How long a duplicate spawn waits for the in-flight winner's terminal
/// before giving up. Worktree provisioning on a cold cache runs a full
/// `git clone --bare`, so this is generous; the wait also ends the
/// moment the winner releases its claim (success or failure).
const INFLIGHT_COLLAPSE_DEADLINE: Duration = Duration::from_secs(600);

/// A duplicate `handle_spawn` lost the in-flight claim. Wait for the
/// winner's terminal, then behave exactly like the existing-singleton
/// path: deliver the prompt (if any) into it and request focus. When
/// the winner fails (claim released, no terminal), drop the duplicate
/// with a retry notice — always, not only when a prompt was lost: an
/// Esc-cancel followed by an immediate re-press can land the retry
/// while the cancelled claim is still releasing, and a silently
/// swallowed key press reads as "lazybox ignored me".
async fn collapse_onto_inflight_spawn(
    config: &ServerConfig,
    session_key: &SessionKey,
    kind: &TerminalKind,
    on_main: bool,
    access: AgentRunAccess,
    prompt: Option<&str>,
) -> bool {
    tracing::info!(
        %session_key,
        ?kind,
        on_main,
        has_prompt = prompt.is_some(),
        "handle_spawn: a spawn for this singleton is already in flight — collapsing onto it",
    );
    let Some(existing) = await_inflight_singleton(config, session_key, kind, on_main).await else {
        tracing::warn!(
            %session_key,
            ?kind,
            "handle_spawn: in-flight spawn released without producing a terminal — dropping duplicate",
        );
        let _ = config.bus.send(Event::provider_error_retryable(
            "spawn",
            "an agent spawn was already in flight but failed — press the key again",
        ));
        return false;
    };
    if terminal_access_for(config, existing).await != access {
        let _ = config.bus.send(Event::provider_error_permanent(
            "spawn",
            "an in-flight agent completed with a different host-access policy",
        ));
        return false;
    }
    if let Some(prompt) = prompt {
        // Boxed for the same reason as the existing-singleton path in
        // `handle_spawn`: `handle_inject_prompt`'s fallback arm can
        // recurse into `handle_spawn`. (No fallback passed here, so it
        // can't actually recurse.)
        Box::pin(handle_inject_prompt(config, existing, prompt, None, true)).await;
    }
    let _ = config.bus.send(Event::TerminalFocusRequested {
        terminal_id: existing,
    });
    true
}

/// Wait for the in-flight winner's terminal to land in the live maps,
/// or for the winner to release its claim without one. Wakes on the
/// guard's `Notify` (with a periodic re-check so a missed ping can't
/// park the waiter), bounded by [`INFLIGHT_COLLAPSE_DEADLINE`].
async fn await_inflight_singleton(
    config: &ServerConfig,
    session_key: &SessionKey,
    kind: &TerminalKind,
    on_main: bool,
) -> Option<TerminalId> {
    // `target` matches the terminal_meta kind (un-folded); the inflight
    // CLAIM key folds `:main` exactly as `InflightSpawnGuard::try_claim`
    // does, so a main-checkout collapse waits on the right winner and
    // never mistakes an isolated agent's claim for it.
    let target = kind.singleton_key()?;
    let claim_target = singleton_claim_target(target.clone(), on_main);
    let claim = (session_key.as_str().to_string(), claim_target);
    let deadline = tokio::time::Instant::now() + INFLIGHT_COLLAPSE_DEADLINE;
    loop {
        if let Some(id) = live_singleton(config, session_key, &target, on_main).await {
            return Some(id);
        }
        let claimed = config.spawn.inflight_spawns.lock().contains_key(&claim);
        if !claimed || tokio::time::Instant::now() >= deadline {
            // Winner released (or we timed out). One final scan closes
            // the insert→release window — the maps are populated before
            // the winner's guard drops, so a miss here means the spawn
            // genuinely failed.
            return live_singleton(config, session_key, &target, on_main).await;
        }
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            config.spawn.inflight_spawn_changed.notified(),
        )
        .await;
    }
}

/// Lightweight live-singleton scan: the same identity match as
/// [`find_existing_singleton`], but reading the maps directly (no
/// backend ring snapshots — this is called from a wait loop) and
/// requiring the `terminals` entry too, so a returned id is immediately
/// injectable (`handle_inject_prompt` resolves the backend key through
/// `terminals`; `terminal_meta` is inserted first during spawn).
async fn live_singleton(
    config: &ServerConfig,
    session_key: &SessionKey,
    target: &str,
    on_main: bool,
) -> Option<TerminalId> {
    let candidates: Vec<TerminalId> = {
        let meta = config.terminal.terminal_meta.lock().await;
        meta.iter()
            .filter(|(_, (sk, k))| {
                sk == session_key && k.singleton_key().as_deref() == Some(target)
            })
            .map(|(id, _)| *id)
            .collect()
    };
    if candidates.is_empty() {
        return None;
    }
    let superseded = config.terminal.superseded_terminals.lock().await.clone();
    let authenticating = config
        .terminal
        .authenticating_terminals
        .lock()
        .await
        .clone();
    let present: Vec<TerminalId> = {
        let terminals = config.terminal.terminals.lock().await;
        candidates
            .into_iter()
            .filter(|id| {
                terminals.contains_key(id)
                    && !superseded.contains(id)
                    && !authenticating.contains(id)
            })
            .collect()
    };
    if present.is_empty() {
        return None;
    }
    // `terminal_meta` carries no checkout flag, so match the requested
    // one against `on_main_terminals`. Read LAST — after confirming the
    // candidate is in `terminals` — because `on_main_terminals` is
    // populated before `terminals` at spawn (and cleared after it at
    // teardown), so a terminal that's live in `terminals` always has its
    // checkout flag settled here. Reading the set first would open the
    // same TOCTOU `find_existing_singleton` avoids.
    let on_main_set = config.terminal.on_main_terminals.lock().await;
    present
        .into_iter()
        .find(|id| on_main_set.contains(id) == on_main)
}

/// How long `Kill` waits for an in-flight spawn on the same workspace
/// before tearing down anyway. Bounded so a wedged provision can't make
/// the user's explicit Kill hang forever; past the cap the teardown
/// proceeds and the tombstone in `deleted_workspaces` makes the late
/// spawn fail with a precise deletion error.
const KILL_INFLIGHT_WAIT: Duration = Duration::from_secs(30);

/// Serialize a workspace teardown against any spawn currently
/// provisioning into it. Without this, Kill could delete the worktree
/// mid-provision and the spawn would re-create it (plus a terminal)
/// right after teardown.
pub(crate) async fn await_inflight_spawns(coordinator: &SpawnCoordinator, workspace_key: &str) {
    let deadline = tokio::time::Instant::now() + KILL_INFLIGHT_WAIT;
    loop {
        let busy = coordinator
            .inflight_spawns
            .lock()
            .keys()
            .any(|(ws, _)| ws == workspace_key);
        if !busy {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                workspace = workspace_key,
                "kill: a spawn is still provisioning after {KILL_INFLIGHT_WAIT:?} — tearing down anyway",
            );
            return;
        }
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            coordinator.inflight_spawn_changed.notified(),
        )
        .await;
    }
}

/// Clear `workspace_key`'s delete tombstone in `deleted_workspaces`
/// once its race window is provably closed.
///
/// The tombstone exists to kill spawns that raced the delete: a spawn
/// that loaded the workspace before its store row vanished can still
/// be provisioning when the delete finishes (`await_inflight_spawns`
/// is bounded — a wedged provision outlives it), and the
/// pre-registration `cancel_spawn_for_deleted_workspace` check is what
/// stops it from registering a terminal for a dead workspace. Any
/// spawn STARTING after the delete fails on its own: the workspace
/// row is gone, so `resolve_or_create_session`'s load errors out.
///
/// So the tombstone is only needed while a pre-delete spawn claim is
/// still in flight. Common case: none is (the delete already drained
/// them) → clear synchronously. Otherwise clear in the background the
/// moment the last claim for the key drops. Without this the
/// tombstone lived forever: recreating a same-name workspace reuses
/// the same key, and every spawn on the new row was silently killed.
pub(crate) fn release_delete_tombstone(config: &ServerConfig, workspace_key: &str) {
    let still_busy = |cfg: &ServerConfig, key: &str| {
        cfg.spawn
            .inflight_spawns
            .lock()
            .keys()
            .any(|(ws, _)| ws == key)
    };
    if !still_busy(config, workspace_key) {
        config.deleted_workspaces.lock().remove(workspace_key);
        return;
    }
    tracing::info!(
        workspace = workspace_key,
        "delete finished with a spawn still in flight — deferring tombstone release until it drains",
    );
    let config = config.clone();
    let key = workspace_key.to_string();
    tokio::spawn(async move {
        loop {
            if !still_busy(&config, &key) {
                config.deleted_workspaces.lock().remove(key.as_str());
                return;
            }
            let _ = tokio::time::timeout(
                Duration::from_millis(200),
                config.spawn.inflight_spawn_changed.notified(),
            )
            .await;
        }
    });
}

/// PR-attach path reconciliation. Walks every session in `workspace`
/// and, for any whose persisted `worktree_path` no longer matches what
/// the current slug would generate, decides what to do — but it never
/// relocates a live worktree. Mutates `workspace` in place; the caller
/// owns persistence + broadcast.
///
/// Running synchronously inside `polling::upsert` (rather than
/// fire-and-forget) closes the race window where consumers could
/// briefly see a stale `worktree_path` between attach + reconciliation.
///
/// When a real worktree already exists on disk the session REUSES it in
/// place: we keep the existing `worktree_path` and do not `git worktree
/// move` it to chase the new slug. Renaming a live worktree yanks the
/// working directory out from under any agent/shell running inside it —
/// the long-standing "session lost on merge" bug (#78/#161/#167) that an
/// issue→PR absorb (the slug-changing case that reaches this branch)
/// triggered every time. Only sessions with no worktree yet (or a
/// non-worktree leftover) get their record rewritten to the slug path.
///
/// Returns whether any session record was actually rewritten. No-op
/// when every session already lives at the right place (most polls).
pub async fn migrate_session_paths_if_needed(workspace: &mut Workspace) -> bool {
    let root = worktree_root();
    migrate_session_paths_if_needed_under(workspace, &root).await
}

/// Explicit-root form of [`migrate_session_paths_if_needed`]. Keeping the
/// filesystem namespace as an argument makes migration tests hermetic and
/// ensures one root snapshot is used for the entire reconciliation pass even
/// if process configuration changes concurrently.
pub async fn migrate_session_paths_if_needed_under(workspace: &mut Workspace, root: &Path) -> bool {
    let mut moved_any = false;
    // Sort sessions by created_at so the index assignment matches
    // what `worktree_path_for_session` expects (first = no suffix,
    // second = -2, etc.).
    let mut order: Vec<usize> = (0..workspace.sessions.len()).collect();
    order.sort_by_key(|&i| workspace.sessions[i].created_at);

    for (slot, sess_idx) in order.into_iter().enumerate() {
        let expected = worktree_path_for_session_under(workspace, slot, root);
        let actual = workspace.sessions[sess_idx].worktree_path.clone();
        if actual == expected {
            continue;
        }
        let actual_exists = tokio::fs::metadata(&actual).await.is_ok();
        if !actual_exists {
            // Path moved by hand or never created. Just update the
            // record — no on-disk move needed.
            workspace.sessions[sess_idx].worktree_path = expected;
            moved_any = true;
            continue;
        }
        // Source dir exists but isn't actually a git worktree —
        // typically a leftover from V1's UUID-named worktree layout.
        // `git worktree move` would fail with "is not a working tree";
        // just update the record and let the next spawn re-provision.
        // We do NOT delete the orphan dir — the user might have
        // unrelated work in there, and earlier deletes have already
        // burned us once.
        let is_worktree = tokio::fs::metadata(actual.join(".git")).await.is_ok();
        if !is_worktree {
            tracing::info!(
                "session {} points at non-worktree {} — updating record only",
                workspace.sessions[sess_idx].id,
                actual.display()
            );
            workspace.sessions[sess_idx].worktree_path = expected;
            moved_any = true;
            continue;
        }
        // A real, live worktree exists at `actual`. REUSE IT IN PLACE —
        // keep the existing path and do NOT `git worktree move` it to
        // match the new slug. This branch only fires when the slug
        // changed under an existing worktree: an issue→PR absorb (the
        // session was just moved onto the PR workspace) or an upstream
        // PR-title edit. Renaming the directory there would pull the
        // working directory out from under any agent/shell running inside
        // it, destroying the very session a merge is meant to preserve —
        // the "session lost on merge" bug (#78/#161/#167). The persisted
        // `worktree_path` is authoritative for every spawn, so a folder
        // name that lags the current slug is purely cosmetic.
        tracing::debug!(
            "session {} reuses existing worktree {} in place (slug changed)",
            workspace.sessions[sess_idx].id,
            actual.display()
        );
    }

    moved_any
}

/// Root directory for every workspace's worktrees. Sits under the v2
/// state root next to `state.db` so a single `rm -rf ~/.lazybox/v2/`
/// wipes everything lazybox owns on disk. Override the parent via the
/// `LAZYBOX_HOME` env var (see `lazybox_core::paths`).
pub fn worktree_root() -> PathBuf {
    lazybox_core::paths::worktrees_root()
}

/// Compose the on-disk path for the Nth session of a workspace.
/// `index = 0` → `<root>/<scope>/<slug>` (no suffix, cleanest case).
/// `index = N` → `<root>/<scope>/<slug>-{N+1}` so the second session is
/// `slug-2`, third is `slug-3`, …  Matches the user mental model
/// where session-counter starts at "no number".
///
/// `<scope>` is the workspace's repo/project qualifier (#223): without
/// it, two workspaces that slug identically in different repos (e.g.
/// "Issues" in `ownerA/repoA` and `ownerB/repoB`) collide on the same
/// directory and cross-contaminate. A repo-less, project-less workspace
/// has no scope and keeps the flat `<root>/<slug>` path.
pub fn worktree_path_for_session(workspace: &Workspace, index: usize) -> PathBuf {
    worktree_path_for_session_under(workspace, index, &worktree_root())
}

/// Explicit-root form of [`worktree_path_for_session`]. `root` is the
/// directory that contains repo/project scopes, normally
/// [`worktree_root`].
pub fn worktree_path_for_session_under(
    workspace: &Workspace,
    index: usize,
    root: &Path,
) -> PathBuf {
    let mut name = workspace.worktree_slug();
    if index > 0 {
        name.push_str(&format!("-{}", index + 1));
    }
    match workspace.worktree_scope() {
        Some(scope) => root.join(scope).join(name),
        None => root.join(name),
    }
}

/// Shared main-checkout worktree path for a workspace's repo:
/// `<root>/<scope>/_main`. Keyed on the workspace's repo/project scope
/// (not its slug) so every workspace on the same repo resolves the same
/// path — the whole point of "the main checkout" is that it's shared.
/// `None` for a repo-less / project-less workspace, which has no scope
/// and no meaningful default branch to sit on.
///
/// The leading underscore matters: `worktree_path_for_session` names
/// isolated trees `<scope>/<slug>`, and `slug::slugify` only ever emits
/// `[a-z0-9-]`, so `_main` can never collide with a per-session slug —
/// including a workspace or project literally named "main". Without it a
/// "main"-slugged workspace's isolated tree and the shared checkout would
/// share one directory and `checkout_at`'s path-idempotency would
/// silently drop one onto the other's branch.
///
/// The `main` label is stable; the branch actually checked out is the
/// repo's resolved default (`main` or `master`), which the folder name
/// doesn't try to track.
pub fn main_worktree_path(workspace: &Workspace) -> Option<PathBuf> {
    workspace
        .worktree_scope()
        .map(|scope| worktree_root().join(scope).join("_main"))
}

/// Whether the workspace behind `session_key` is a linked (no-worktree)
/// checkout — its sessions run in the user's existing clone on disk.
/// Best-effort synchronous store read: a missing / unreadable record
/// reports `false`, so a spawn degrades to normal handling rather than
/// failing on a lookup error.
fn workspace_is_linked(config: &ServerConfig, session_key: &SessionKey) -> bool {
    let key = WorkspaceKey::new(session_key.as_str());
    load_workspace(config, &key)
        .map(|w| w.is_linked())
        .unwrap_or(false)
}

/// Explicit session creation. Always provisions a fresh worktree
/// folder, even if the workspace already has sessions — multi-session
/// workspaces are the whole point of this entry point.
pub async fn handle_create_session(
    config: &ServerConfig,
    session_key: SessionKey,
    kind: TerminalKind,
    label: Option<String>,
) {
    let workspace_key = WorkspaceKey::new(session_key.as_str());
    let _workspace_guard = config.lock_workspace(workspace_key.as_str()).await;
    let mut workspace = match load_workspace(config, &workspace_key) {
        Ok(w) => w,
        Err(e) => {
            let _ = config.bus.send(Event::provider_error_permanent(
                "create_session",
                e.to_string(),
            ));
            return;
        }
    };
    let path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut session = Session::new(
        workspace_key,
        session_kind_from_terminal(&kind),
        path,
        Utc::now(),
    );
    if let Some(label) = label {
        session.name = label;
    }
    workspace.add_session(session.clone());
    if let Err(e) = persist_and_broadcast(config, &workspace).await {
        let _ = config.bus.send(Event::provider_error_permanent(
            "create_session",
            e.to_string(),
        ));
        return;
    }
    let _ = config.bus.send(Event::SessionCreated(Box::new(session)));
}

/// Project a wire-side `TerminalKind` to a runtime `SessionKind`.
/// Today they're nearly isomorphic but they live at different layers
/// — `SessionKind` is what's persisted on the workspace, while
/// `TerminalKind` is the wire-format for spawn commands.
fn session_kind_from_terminal(kind: &TerminalKind) -> SessionKind {
    match kind {
        TerminalKind::Agent(agent_id) => SessionKind::Agent {
            agent_id: agent_id.clone(),
        },
        TerminalKind::Shell => SessionKind::Shell,
        TerminalKind::LogTail { path } => SessionKind::LogTail { path: path.clone() },
    }
}

fn load_workspace(
    config: &ServerConfig,
    key: &WorkspaceKey,
) -> Result<Workspace, crate::ServerError> {
    use crate::ServerError;
    let record = config
        .store
        .get_workspace(key)
        .map_err(|e| ServerError::Store(e.to_string()))?
        .ok_or_else(|| ServerError::Workspace(format!("unknown workspace {}", key.as_str())))?;
    let json = record
        .workspace_json
        .ok_or_else(|| ServerError::Workspace(format!("workspace {} has no json", key.as_str())))?;
    Ok(serde_json::from_str(&json)?)
}

async fn persist_and_broadcast(
    config: &ServerConfig,
    workspace: &Workspace,
) -> Result<(), crate::ServerError> {
    use crate::ServerError;
    let json = serde_json::to_string(workspace)?;
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace.key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(json),
        })
        .map_err(|e| ServerError::Store(e.to_string()))?;
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace.clone())));
    Ok(())
}

/// The recent tail of the rolling buffer the detector actually scans.
/// The 32 KiB outer buffer exists so spread-out tickers + small chunks
/// accumulate enough context, but once a prompt scrolls past this tail
/// it should STOP matching — otherwise the user's "I answered the
/// prompt and moved on" never reflects: the old `❯ 1.` text stays in
/// the buffer and the "needs input" label sticks. 16 KiB (was 8 KiB) —
/// Claude's bash-permission prompts can sit BELOW 8+ KiB of preview
/// output (long heredocs, `cat <<EOF | gh api ...` patches, multi-file
/// `cat` outputs, etc.); 16 KiB still evicts stale prompts within
/// ~half a screen of follow-up output, while comfortably covering
/// claude's largest tool-preview screens.
const DETECT_WINDOW: usize = 16 * 1024;

fn detect_window(buf: &[u8]) -> &[u8] {
    &buf[buf.len().saturating_sub(DETECT_WINDOW)..]
}

async fn maybe_emit_auth_required(
    config: &ServerConfig,
    agent: Option<&std::sync::Arc<dyn lazybox_agents::Agent>>,
    state_buf: &[u8],
    terminal_id: TerminalId,
    emitted: &mut bool,
) {
    if *emitted {
        return;
    }
    let Some(failure) = agent.and_then(|agent| agent.detect_auth_failure(detect_window(state_buf)))
    else {
        return;
    };
    *emitted = true;
    crate::agent_auth::detect_required(config, terminal_id, failure.reason).await;
}

/// Fetch the replay ring + covered seq for a pump that detected a seq
/// gap (a chunk dropped on the backend's bounded bridge or a lagged
/// broadcast). The reader thread pushes to the ring BEFORE
/// broadcasting, so a snapshot taken after observing `gap_chunk_seq`
/// covers every dropped chunk and the observed one. Failure, timeout, and
/// stale snapshots are explicit: the caller preserves its last coherent
/// state, drops the torn chunk, and retries on the next output instead of
/// fabricating an empty reset or coverage it never saw.
///
/// A wrapped ring (`complete: false`) is NOT a miss. Its `replay_snapshot`
/// is line-boundary-clean (`ReplayRing::replay_snapshot_into`), so the
/// `TerminalResync` still replaces the torn stream with a correct, if
/// shorter-history, screen — exactly as the forwarder's `resync_replay`
/// does. Rejecting `!complete` here froze the daemon pump for every client:
/// once the ring wrapped, `is_complete()` is false forever, so a single
/// upstream gap made every subsequent chunk re-enter this path and get
/// dropped (the callers never advance `last_seq` on `None`).
async fn resync_replay_after_gap(
    backend: &dyn crate::backend::SessionBackend,
    key: &str,
    gap_chunk_seq: u64,
    last_seq: u64,
) -> Option<crate::backend::ReplaySnapshot> {
    tracing::warn!(
        key,
        last_seq,
        chunk_seq = gap_chunk_seq,
        "output seq gap — chunk(s) dropped upstream; resyncing from replay ring"
    );
    match tokio::time::timeout(SNAPSHOT_PER_SESSION_TIMEOUT, backend.snapshot(key)).await {
        Ok(Ok(snapshot)) if snapshot.last_seq < gap_chunk_seq => {
            tracing::warn!(
                key,
                chunk_seq = gap_chunk_seq,
                snapshot_seq = snapshot.last_seq,
                "gap resync snapshot did not cover the observed chunk; retrying"
            );
            None
        }
        Ok(Ok(snapshot)) => Some(snapshot),
        Ok(Err(e)) => {
            tracing::warn!(key, "gap resync snapshot failed: {e}");
            None
        }
        Err(_) => {
            tracing::warn!(key, "gap resync snapshot timed out");
            None
        }
    }
}

/// Extract a readable tail from a terminal's raw replay ring for the
/// frozen exit pane (issue #368): strip ANSI/OSC escape sequences and
/// control bytes, collapse carriage-return overwrites, drop blank lines,
/// and keep the last few lines. `None` when nothing printable remains
/// (the "produced no output" case — the pane then just shows the failure
/// banner). Bounded so a full 64 KiB ring can't blow up the wire payload.
fn last_output_tail(bytes: &[u8]) -> Option<String> {
    agent_output_tail(bytes, 8)
}

/// Clean and line-limit raw terminal bytes into a legible text tail:
/// strip escape sequences, collapse in-place `\r` overwrites, drop
/// blank lines, and keep at most the final `max_lines`. Shared by the
/// dying-agent recap (`last_output_tail`) and the workspace-addressed
/// `get_agent_output` gateway read (issue #773).
pub(crate) fn agent_output_tail(bytes: &[u8], max_lines: usize) -> Option<String> {
    const MAX_LINE_CHARS: usize = 200;

    let text = String::from_utf8_lossy(bytes);
    // Normalize CRLF line breaks first so the trailing `\r` isn't mistaken
    // for an in-place overwrite below.
    let stripped = strip_ansi(&text).replace("\r\n", "\n");
    let mut lines: Vec<String> = stripped
        .split('\n')
        .map(|line| {
            // A bare `\r` (progress bars, spinners) overwrites the line
            // in place — keep only what a real terminal would show: the
            // content after the last carriage return.
            let visible = line.rsplit('\r').next().unwrap_or(line).trim_end();
            visible.chars().take(MAX_LINE_CHARS).collect::<String>()
        })
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let tail = lines.split_off(lines.len().saturating_sub(max_lines));
    Some(tail.join("\n"))
}

/// Read a running agent terminal's recent output as a cleaned,
/// line-limited text tail — the workspace-addressed read behind the
/// gateway's `get_agent_output` (issue #773). Prefers the backend's
/// deep scrollback (tmux `capture-pane`) and falls back to the live
/// ring snapshot for backends without a history source (raw PTY).
/// `None` when the terminal is unknown or produced no legible output.
pub async fn agent_output_snapshot(
    config: &ServerConfig,
    terminal_id: TerminalId,
    max_lines: usize,
) -> Option<String> {
    let key = config.terminal.backend_key_for(terminal_id).await?;
    let bytes = match config.backend.scrollback(&key).await {
        Ok(Some((replay, _seq))) => replay,
        _ => match config.backend.snapshot(&key).await {
            Ok(snapshot) => snapshot.replay,
            Err(_) => return None,
        },
    };
    // A tmux deep scrollback can be megabytes; cleaning all of it to keep
    // `max_lines` lines is wasteful on a frequently-polled read. Clean only
    // a trailing window generously sized to still contain `max_lines` lines
    // (well above the 200-char per-line cap the cleaner applies). Slicing
    // mid-sequence can corrupt at most the window's first line, which the
    // tail extractor drops anyway (see `strip_ansi`).
    let window = trailing_window(&bytes, max_lines.saturating_mul(TAIL_SCAN_BYTES_PER_LINE));
    agent_output_tail(window, max_lines)
}

/// Bytes to scan per requested output line before cleaning — comfortably
/// above the cleaner's 200-char line cap so the kept tail is never
/// truncated by the window boundary.
const TAIL_SCAN_BYTES_PER_LINE: usize = 1024;

/// The trailing `at_most` bytes of `bytes` (or all of them when shorter),
/// clamped to a sane floor/ceiling so a tiny `max_lines` still gets enough
/// context and a huge one can't scan an unbounded buffer.
fn trailing_window(bytes: &[u8], at_most: usize) -> &[u8] {
    let window = at_most.clamp(64 * 1024, 1024 * 1024);
    let start = bytes.len().saturating_sub(window);
    &bytes[start..]
}

/// Drop ANSI escape sequences and non-printing control bytes from
/// terminal output, leaving text, tabs, and newlines. Deliberately small
/// — enough to make a dying agent's final lines legible, not a full VT.
///
/// A caveat inherent to reading a fixed-capacity ring: `snapshot` can
/// begin mid-sequence (the oldest bytes were overwritten), so an orphan
/// escape *tail* — e.g. a leading `31m` with no `ESC [` — leaks as text.
/// It only ever affects the first line, which the tail extractor usually
/// drops, so it's left as-is rather than guessed at.
fn strip_ansi(input: &str) -> String {
    // Consume the body of a string sequence (DCS/OSC/APC/PM/SOS),
    // terminated by ST (ESC \) or, tolerantly, BEL.
    fn skip_string_sequence(chars: &mut std::iter::Peekable<std::str::Chars>) {
        while let Some(p) = chars.next() {
            if p == '\u{07}' {
                break;
            }
            if p == '\u{1b}' {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                break;
            }
        }
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: ESC [ … final byte in 0x40–0x7E.
                Some('[') => {
                    for p in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&p) {
                            break;
                        }
                    }
                }
                // String sequences terminated by ST/BEL: OSC (]), DCS (P),
                // SOS (X), APC (_), PM (^). Their payloads (window titles,
                // sixel/kitty-graphics data, query replies) are never text.
                Some(']' | 'P' | 'X' | '_' | '^') => skip_string_sequence(&mut chars),
                // Charset / other two-byte escapes: drop ESC + one byte.
                _ => {}
            },
            '\n' | '\t' | '\r' => out.push(c),
            // Control bytes and `from_utf8_lossy`'s replacement char (an
            // undecodable byte — a truncated multibyte at the ring
            // boundary, or an 8-bit C1 control the lossy decode mangled)
            // are never legible text.
            c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{fffd}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Exit teardown shared by `handle_spawn`'s output pump and
/// `recover_sessions`' recovery pump: broadcast `TerminalExited`, sweep
/// every per-terminal map, delete the persisted kv rows, release the
/// backend's session slot, and drop the generated hook settings file.
///
/// One function on purpose — the recovery pump used to hand-roll a
/// subset (terminals/terminal_meta/no_permission only), so entries
/// later inserted for a recovered terminal (agent_states,
/// hook_driven_terminals, input_needed_shapes, prompt_submit_signals)
/// outlived it, and its `terminal:*`/`terminal-noperm:*`/
/// `terminal-msg:*`/`terminal-msgs:*`/`terminal-draft:*`/
/// `terminal-pty-generation:*` kv rows accumulated in state.db forever.
pub(crate) async fn teardown_exited_terminal(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    exit_code: Option<i32>,
) {
    finish_terminal(config, terminal_id, backend_key, exit_code, true).await;
}

/// Complete the user-driven kill path through the same lifecycle owner as a
/// self-exit. The output pump still owns backend `release` once it observes
/// the actual child exit; if it wins the teardown race first, this call is an
/// idempotent no-op.
pub(crate) async fn detach_killed_terminal(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
) {
    finish_terminal(config, terminal_id, backend_key, None, false).await;
}

async fn finish_terminal(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    exit_code: Option<i32>,
    release_backend: bool,
) {
    // Join the same interaction boundary as writes, resizes, injections, and
    // kills before removing the live mapping. Otherwise an already-started
    // delayed write can complete after `TerminalExited` and mutate a backend
    // session the daemon has declared detached. Once the mapping is removed,
    // queued interaction waiters acquire this guard in turn, fail their
    // liveness re-check, and leave without touching the backend.
    let _io_guard = config.terminal.lock_terminal_io(backend_key).await;
    // Background draft/user-message persistence is independent from the PTY
    // input lane. Serialize its final write with the entire teardown claim +
    // kv sweep: persistence that won first completes before our deletes;
    // persistence that lost re-checks the removed terminal mapping and skips.
    // Either ordering ends with no orphan rows.
    let _persistence_guard = config.terminal.lock_terminal_persistence(backend_key).await;
    // Atomically claim the wire terminal. Forced workspace deletion and the
    // output pump can race to finish the same child; only the winner emits
    // lifecycle events and sweeps bookkeeping. The output-pump loser still
    // releases the backend slot after observing the real exit.
    let registered_key = config.terminal.terminals.lock().await.remove(&terminal_id);
    match registered_key.as_deref() {
        Some(key) if key == backend_key => {}
        Some(other) => {
            tracing::error!(
                ?terminal_id,
                expected = backend_key,
                registered = other,
                "terminal teardown key mismatch — refusing to sweep the wrong terminal",
            );
            config
                .terminal
                .terminals
                .lock()
                .await
                .insert(terminal_id, other.to_string());
            config
                .terminal
                .forget_terminal_persistence_lock(backend_key);
            config.terminal.forget_terminal_io_lock(backend_key);
            return;
        }
        None => {
            if release_backend {
                config.backend.release(backend_key).await;
            }
            config
                .terminal
                .forget_terminal_persistence_lock(backend_key);
            config.terminal.forget_terminal_io_lock(backend_key);
            return;
        }
    }

    // A spawned session going away used to be silent — a crashing agent
    // (e.g. its binary swapped out mid-run by a Homebrew self-upgrade,
    // issue #355) left no trace in the log, so #356 read as "the whole
    // workspace just vanished". Announce every exit with its status and
    // owning session/kind so the log makes an abnormal exit obvious.
    let meta = config
        .terminal
        .terminal_meta
        .lock()
        .await
        .get(&terminal_id)
        .cloned();
    let (session, kind) = match &meta {
        Some((session_key, kind)) => (Some(session_key.as_str()), Some(kind)),
        None => (None, None),
    };
    let provider_auth_terminal = config
        .terminal
        .authenticating_terminals
        .lock()
        .await
        .contains(&terminal_id);
    // Capture the cleaned tail of an exiting agent's output so a frozen
    // pane can show *why* it died instead of a blank screen (issue #368).
    // The client decides whether to surface it — a dead-on-arrival launch
    // paints it (#367), a clean exit auto-closes and ignores it — so the
    // capture is unconditional for agents but cheap (one bounded snapshot
    // per process exit, a rare event). Bounded by the same per-session
    // timeout every other reader uses so a wedged backend can't stall
    // teardown and leak the slot; read here, before `release` drops the
    // ring below.
    let last_output = if matches!(kind, Some(TerminalKind::Agent(_))) && !provider_auth_terminal {
        match tokio::time::timeout(
            SNAPSHOT_PER_SESSION_TIMEOUT,
            config.backend.snapshot(backend_key),
        )
        .await
        {
            Ok(Ok(snapshot)) => last_output_tail(&snapshot.replay),
            _ => None,
        }
    } else {
        None
    };
    if let Some(TerminalKind::Agent(agent_id)) = kind
        && let Some(output) = last_output.as_deref()
        && let Some(failure) = config
            .agents
            .get(agent_id)
            .and_then(|agent| agent.detect_auth_failure(output.as_bytes()))
    {
        crate::agent_auth::detect_required(config, terminal_id, failure.reason).await;
    }
    if matches!(kind, Some(TerminalKind::Agent(_))) {
        let (prompt_history, composing_buffer) = tokio::join!(
            load_prompt_history(config, backend_key),
            load_composing_buffer(config, backend_key),
        );
        config
            .agent_recovery
            .mark_exited(terminal_id, backend_key, prompt_history, composing_buffer)
            .await;
    }
    let authenticating = provider_auth_terminal || config.agent_recovery.active(terminal_id).await;
    match exit_code {
        Some(0) => tracing::info!(
            ?terminal_id,
            backend_key,
            session,
            ?kind,
            "teardown_exited_terminal: clean exit (code 0)"
        ),
        Some(code) => tracing::warn!(
            ?terminal_id,
            backend_key,
            session,
            ?kind,
            exit_code = code,
            "teardown_exited_terminal: terminal exited abnormally"
        ),
        None => tracing::warn!(
            ?terminal_id,
            backend_key,
            session,
            ?kind,
            "teardown_exited_terminal: terminal exited with no exit status (killed by signal?)"
        ),
    }
    // A dead agent must leave its last live pill: a crashed Codex that was
    // `Working` (or a `Done`/`InputNeeded` agent the user closed) has to move
    // to the terminal `Exited` state, not linger as a false "working" spinner
    // (#356/#357). Broadcast it before the maps are swept — `agent_states`
    // still holds the prior value and `terminal_meta` still resolves the live
    // session key. The atomic owner commits `Exited` into the cache before it
    // broadcasts, so a racing late hook/PTY reading sees the absorbing state
    // and cannot resurrect the process. Only agent terminals carry a state
    // pill; shells don't.
    if !authenticating
        && let Some((session_key, TerminalKind::Agent(_))) = meta
        && let Some(durability) = agent_state_durability(config, terminal_id, backend_key).await
    {
        transition_and_broadcast_agent_state(
            &config.terminal,
            &config.bus,
            &durability,
            terminal_id,
            &session_key,
            StateSource::Exit,
            |_| Some(lazybox_ipc::AgentState::Exited { code: exit_code }),
        )
        .await;
    }
    if !authenticating {
        let _ = config.bus.send(Event::TerminalExited {
            terminal_id,
            exit_code,
            last_output,
        });
    }
    // `terminals` was removed by the atomic claim above, so snapshots stop
    // seeing this id before any auxiliary map disappears. Keep terminal_meta
    // until the state event is sent, then close that ingress gate before
    // dropping the absorbing Exited tombstone. Safe because no two locks are
    // co-held here — each
    // `.lock().await.remove(...)` releases at end-of-statement.
    // `crate::TERMINAL_MAP_LOCK_ORDER` applies to co-holding sites only.
    terminal_io::clear_view_activity(config, terminal_id).await;
    config
        .terminal
        .terminal_sessions
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .agent_detect_resets
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .hook_driven_terminals
        .lock()
        .await
        .remove(&terminal_id);
    config
        .spawn
        .prompt_submit_signals
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .input_needed_shapes
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .reclassify_requests
        .lock()
        .await
        .remove(&terminal_id);
    // Close the state owner's live-terminal ingress gate before dropping the
    // absorbing Exited tombstone. Reversing these two removals creates a
    // window where a delayed hook sees `(meta: live, state: None)` and
    // resurrects the terminal from its first reading.
    config
        .terminal
        .terminal_meta
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .agent_states
        .lock()
        .await
        .remove(&terminal_id);
    let agent_state_generation = config
        .terminal
        .agent_state_generations
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .no_permission_terminals
        .lock()
        .await
        .remove(&terminal_id);
    config.terminal.forget_access(terminal_id).await;
    config
        .terminal
        .on_main_terminals
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .terminal_models
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .superseded_terminals
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .authenticating_terminals
        .lock()
        .await
        .remove(&terminal_id);
    config
        .terminal
        .outdated_agent_terminals
        .lock()
        .await
        .remove(&terminal_id);
    for field in TerminalPersistedField::ALL {
        let key = field.key(backend_key);
        if let Err(error) = config.store.delete_kv(&key) {
            tracing::warn!(?terminal_id, %key, %error, "terminal teardown: kv cleanup failed");
        }
    }
    if let Some(generation) = agent_state_generation {
        let key = agent_state_key(backend_key, generation);
        if let Err(error) = config.store.delete_kv(&key) {
            tracing::warn!(?terminal_id, %key, %error, "terminal teardown: agent state cleanup failed");
        }
    }
    // Release the backend's per-session slot (PTY fds, writer thread,
    // replay ring). The exit has been observed by the time we're here,
    // so this is a pure handle drop — for a self-exited session it's
    // the ONLY release path: `kill` never ran, and before this call
    // existed the slot lived in the backend map forever.
    if release_backend {
        config.backend.release(backend_key).await;
    }
    // Drop the per-session hook settings file we generated at spawn.
    // Best-effort — a leftover file is harmless (it's overwritten by
    // the next spawn that reuses the id, which can't happen anyway
    // since ids are monotonic) but cleaning up keeps the runtime dir
    // tidy. Reconstructed from the id, no bookkeeping needed.
    let _ = std::fs::remove_file(hook_settings_path(terminal_id));
    let _ = std::fs::remove_file(hook_backend_key_path(terminal_id));
    config
        .terminal
        .forget_terminal_persistence_lock(backend_key);
    config.terminal.forget_terminal_io_lock(backend_key);
}

/// Ingest one PTY output chunk for a terminal: append it to the rolling
/// detection buffer and offer the state machine a reading. Bytes flowing is
/// normally the working signal (issue #289). The only immediate exception is
/// an adapter's high-confidence current-chunk prompt detector: it may surface
/// unmistakable modal chrome without waiting for quiet. Inspecting only a
/// marker touched by the newest chunk preserves the stale-scrollback guard;
/// the full classifier still runs only in [`classify_quiet_screen`].
///
/// The ordinary byte-flow `Working` reading is ambiguous (`clear: false`),
/// so it can never clear a parked `?` or a finished `Done` (an incidental
/// repaint — a click, a focus, a pane resize — must not un-ask or un-finish,
/// #374). A positive current-chunk modal match is authoritative
/// (`InputNeeded`, `clear: true`). A genuinely resumed stream commits
/// `Working` off the next clear quiet-classification, once it comes to rest.
async fn note_pty_activity(
    agent: Option<&std::sync::Arc<dyn lazybox_agents::Agent>>,
    buf: &mut Vec<u8>,
    bytes: &[u8],
    output_seq: u64,
    // Whether this chunk moved the content fingerprint (the pump's
    // watchdog tracker) — real output rather than repaint churn. Rides
    // the byte-flow `Working` reading so a progress streak can re-open
    // `Working` from `Done` (#398).
    progress: bool,
    terminals: &TerminalRegistry,
    bus: &tokio::sync::broadcast::Sender<Event>,
    durability: Option<&AgentStateDurability>,
    id: TerminalId,
    session_key: &SessionKey,
    state_machine: &mut lazybox_agents::AgentStateMachine,
) {
    const STATE_BUF_CAP: usize = 32 * 1024;
    let Some(agent) = agent else {
        return;
    };
    let Some(durability) = durability else {
        tracing::error!(
            terminal_id = ?id,
            "agent state invariant: PTY activity has no durability context"
        );
        return;
    };
    buf.extend_from_slice(bytes);
    // Amortized trim: draining down to exactly STATE_BUF_CAP on every
    // chunk memmoves the whole 32 KiB tail per (often tiny) status-bar
    // tick. Let the buffer run to 2× cap and cut back to cap in one
    // drain — same detection semantics (the detector only ever reads
    // the DETECT_WINDOW tail, far inside the retained region) at O(1)
    // amortized cost per byte, for at most one extra 32 KiB of
    // transient memory per terminal.
    if buf.len() > STATE_BUF_CAP * 2 {
        let drop = buf.len() - STATE_BUF_CAP;
        buf.drain(..drop);
    }
    let detect_window = detect_window(buf);
    let last_chunk_start = detect_window.len().saturating_sub(bytes.len());
    let immediate_shape =
        agent.detect_input_needed_in_current_chunk(detect_window, last_chunk_start);
    let pty = if let Some(shape) = immediate_shape {
        terminals.input_needed_shapes.lock().await.insert(id, shape);
        lazybox_agents::PtyReading {
            state: lazybox_ipc::AgentState::InputNeeded,
            clear: true,
            progress: false,
            liveness: lazybox_agents::Liveness::Streaming,
            ready_for_prompt: false,
        }
    } else {
        lazybox_agents::PtyReading {
            state: lazybox_ipc::AgentState::Working,
            clear: false,
            progress,
            liveness: lazybox_agents::Liveness::Streaming,
            ready_for_prompt: false,
        }
    };
    commit_pty_reading(
        agent,
        detect_window,
        pty,
        terminals,
        bus,
        durability,
        id,
        session_key,
        state_machine,
        Some(output_seq),
    )
    .await;
}

/// Classify the resting screen once the PTY has been quiet for
/// [`PTY_QUIET_CLASSIFY_AFTER`] and fold the result into the state
/// machine. Only here does the screen-scrape detector run: with the
/// stream at rest the recency anchors are settled, so a structural
/// prompt on screen is a live gate (`InputNeeded`), a resting composer
/// is `Idle`, and a still-painted status line is a wedged `Working` —
/// none of which can flip a visibly-streaming session anymore.
///
/// `last_chunk_len` is the length of the most recent chunk appended to
/// `buf` — the chunk-boundary hint the detector's same-chunk rule needs
/// (a full-screen repaint delivers a live dialog and the bottom status
/// bar in ONE chunk, status bar last; position alone would read the
/// dialog as already answered).
async fn classify_quiet_screen(
    agent: Option<&std::sync::Arc<dyn lazybox_agents::Agent>>,
    buf: &[u8],
    last_chunk_len: usize,
    // How the pump reached this classification: [`Liveness::Silent`] from
    // the quiet timer (no BYTES for the quiet window) or
    // [`Liveness::Watchdog`] from the configured content-stability bound.
    // Both are authoritative inactivity evidence while `Working`.
    liveness: lazybox_agents::Liveness,
    terminals: &TerminalRegistry,
    bus: &tokio::sync::broadcast::Sender<Event>,
    durability: Option<&AgentStateDurability>,
    id: TerminalId,
    session_key: &SessionKey,
    state_machine: &mut lazybox_agents::AgentStateMachine,
) {
    let Some(agent) = agent else {
        return;
    };
    let Some(durability) = durability else {
        tracing::error!(
            terminal_id = ?id,
            "agent state invariant: PTY classification has no durability context"
        );
        return;
    };
    // A pending answer reset means the buffer's contents predate the
    // user's answer by decree: `handle_write` flipped the `?` to Working
    // and marked the buffer for clearing, but the clear only lands on the
    // NEXT chunk. Classifying the stale dialog now would re-raise the
    // just-answered `?` (and its notification) — so the classify below is
    // skipped either way. But this timer firing while the reset is STILL
    // latched is itself the signal that the answer produced zero output for
    // a whole quiet window: nothing arrived to clear the reset or settle the
    // turn. Settle it `Done` directly rather than peeking and returning — a
    // bare return leaves `Working` pinned, because the quiet timer disarms on
    // fire and only a chunk re-arms it, and zero output means no chunk is
    // coming (the watchdog was the sole escape, and none exists when
    // `working_watchdog_secs = 0`). Force `Stalled`, not this timer's
    // `Silent`, so the ambiguous zero-output `Done` yields to a fresh hook
    // through `commit_pty_reading`'s hooks-primary gate (`Silent` would
    // override it — see `hooks_gate_allows`). A genuinely live silent turn
    // keeps a fresh hook and stays `Working`; only a hook-stale or hookless
    // terminal settles here. Leave the reset latched: a late chunk still
    // clears the stale buffer via the pump's chunk arm, and by then the state
    // is `Done`.
    if terminals.agent_detect_resets.lock().await.contains(&id) {
        commit_pty_reading(
            agent,
            detect_window(buf),
            lazybox_agents::PtyReading {
                state: lazybox_ipc::AgentState::Done,
                clear: true,
                progress: false,
                liveness: lazybox_agents::Liveness::Stalled,
                ready_for_prompt: false,
            },
            terminals,
            bus,
            durability,
            id,
            session_key,
            state_machine,
            None,
        )
        .await;
        return;
    }
    let detect_window = detect_window(buf);
    if detect_window.is_empty() {
        return;
    }
    // The composer footer is stable while the screen rests, so scrape the
    // live model + effort here (once per settle) rather than on every
    // streaming chunk. A no-op for agents without a PTY model reading.
    detect_and_broadcast_model(agent, detect_window, terminals, bus, id, session_key).await;
    let last_chunk_start = detect_window.len().saturating_sub(last_chunk_len);
    let new_state = match agent.detect_observation_chunked(detect_window, last_chunk_start) {
        Some(observation) => {
            let new_state = observation.state();
            tracing::trace!(
                terminal_id = ?id,
                buf_len = buf.len(),
                detected = ?new_state,
                "classify_quiet_screen ran",
            );
            if let Some(shape) = observation.prompt_shape() {
                tracing::debug!(
                    terminal_id = ?id,
                    buf_len = buf.len(),
                    tail_tip = %String::from_utf8_lossy(
                        &detect_window[detect_window.len().saturating_sub(120)..]
                    ),
                    "classify_quiet_screen → InputNeeded",
                );
                // Shape comes from the adapter's semantic observation rather
                // than being guessed here. Recorded before state dedupe so a
                // re-rendered prompt can refresh its interaction contract.
                terminals.input_needed_shapes.lock().await.insert(id, shape);
            }
            new_state
        }
        // The resting screen classifies as nothing recognizable — common
        // for the weaker Codex/Cursor detectors (#225). Surface a bare
        // `Done`: the state machine settles a quiet `Working` turn to
        // `Done` with it and ignores it from every other state (a live `?`
        // stays asking, a never-worked `Idle` stays blank). Without this a
        // hookless agent whose composer doesn't match spins on `Working`
        // forever after finishing its turn.
        None => {
            tracing::trace!(
                terminal_id = ?id,
                buf_len = buf.len(),
                "classify_quiet_screen: unclassified resting screen → bare Done settle",
            );
            lazybox_ipc::AgentState::Done
        }
    };
    // `ready_for_prompt` is only probed for an Idle reading — the
    // hooks-primary gate uses it to decide whether a quiet Idle may
    // demote a hook-set `Working`.
    let ready_for_prompt =
        new_state == lazybox_ipc::AgentState::Idle && agent.detect_ready_for_prompt(detect_window);
    // The quiet window itself is the confidence: the screen has been at
    // rest for seconds, so the classification is authoritative (`clear`)
    // and no ambiguous-exit damping holds it.
    let pty = lazybox_agents::PtyReading {
        state: new_state,
        clear: true,
        progress: false,
        liveness,
        ready_for_prompt,
    };
    commit_pty_reading(
        agent,
        detect_window,
        pty,
        terminals,
        bus,
        durability,
        id,
        session_key,
        state_machine,
        None,
    )
    .await;
}

/// The content-stability watchdog fired (#398): [`WORKING_WATCHDOG_AFTER`]
/// with no meaningful content change. Re-classify the screen exactly as the
/// quiet path would — churn re-arms the byte-silence quiet timer so
/// `classify_quiet_screen` never runs on its own — for the two states a
/// content-stable screen can be wrong about:
///
///   - **`Working`** — a frozen status line that never settled. If it
///     *still* reads `Working` afterwards (hookless agents have no other
///     exit), force the turn closed with a clear `Done` reading.
///   - **`InputNeeded`** — a stale `?` whose turn has actually ended. A
///     finished agent that leaves a background shell running keeps the PTY
///     emitting bytes, so the stream never goes byte-silent and the quiet
///     timer can't reach it; only content-stability can notice the prompt
///     is gone (#872). The re-classification is a *clear* reading, so a
///     resting composer resolves the `?` while a genuinely live prompt
///     re-reads `InputNeeded` and stays. Subject to the same hooks-primary
///     gate as any PTY reading, so a fresh hook still owns the asking call
///     (#62) — only a stale-hook / hookless `?` clears here. `Working` is
///     the only state force-closed; a surviving `InputNeeded` is left as-is.
///
/// The force commits through [`commit_pty_reading`]. Its
/// [`Liveness::Watchdog`] evidence is authoritative while `Working`, so a
/// fresh lifecycle hook cannot extend the configured upper bound.
/// A pending answer reset no longer vetoes the whole tick: still latched
/// a full watchdog window after the answer, it means zero PTY output
/// followed, so the stale-buffer classify (which would re-raise the
/// just-answered `?`) is skipped and the turn is settled `Done` directly.
/// See the inline comment for why that can't pin `Working`.
async fn watchdog_reverify_parked_turn(
    agent: Option<&std::sync::Arc<dyn lazybox_agents::Agent>>,
    buf: &[u8],
    last_chunk_len: usize,
    terminals: &TerminalRegistry,
    bus: &tokio::sync::broadcast::Sender<Event>,
    durability: Option<&AgentStateDurability>,
    id: TerminalId,
    session_key: &SessionKey,
    state_machine: &mut lazybox_agents::AgentStateMachine,
) {
    let Some(agent) = agent else {
        return;
    };
    let Some(durability) = durability else {
        tracing::error!(
            terminal_id = ?id,
            "agent state invariant: PTY watchdog has no durability context"
        );
        return;
    };
    if !matches!(
        terminals.agent_states.lock().await.get(&id).copied(),
        Some(lazybox_ipc::AgentState::Working | lazybox_ipc::AgentState::InputNeeded)
    ) {
        return;
    }
    // A pending answer reset normally vetoes the whole tick: the buffer
    // predates the user's answer, so classifying it would re-raise the
    // just-answered `?`. But that reset is cleared only by the pump's NEXT
    // live chunk — so if it is STILL latched a full watchdog window after
    // the answer, the optimistic `Working` flip has seen zero PTY output:
    // the answer started no work, and nothing will arrive to clear the
    // reset or settle the turn. Skipping the tick then pins `Working`
    // forever (the quiet timer disarms itself and only a chunk re-arms it).
    // So skip only the stale-buffer classify (which would re-raise the
    // answered prompt) and commit the `Done` straight away. The configured
    // watchdog bound is authoritative even for a fresh-hook terminal. Leave
    // the reset latched — a late chunk clears the buffer via the chunk arm,
    // and by then the state is `Done`, so the watchdog no-ops.
    let answered = terminals.agent_detect_resets.lock().await.contains(&id);
    if !answered {
        classify_quiet_screen(
            Some(agent),
            buf,
            last_chunk_len,
            // The watchdog fires on content-stability, not byte-silence: a
            // ticking counter can keep the stream alive. This evidence is
            // the configured upper bound even when the last hook is fresh.
            lazybox_agents::Liveness::Watchdog,
            terminals,
            bus,
            Some(durability),
            id,
            session_key,
            state_machine,
        )
        .await;
    }
    // Force the turn closed only while the terminal STILL reads `Working` —
    // the stuck-status case the force exists for (a frozen status line, or a
    // zero-output answer the `?` flip pinned at `Working`). A re-classify
    // that already settled the turn, or an `InputNeeded` the answered-branch
    // skipped past (a hook/answer race), is resolved and must not be
    // force-closed: a `Done` from `InputNeeded` is rejected anyway, but this
    // also stops the spurious "forcing the turn closed" log for it.
    if terminals.agent_states.lock().await.get(&id).copied()
        != Some(lazybox_ipc::AgentState::Working)
    {
        return;
    }
    tracing::info!(
        terminal_id = ?id,
        answered,
        "working watchdog: forcing the turn closed",
    );
    commit_pty_reading(
        agent,
        detect_window(buf),
        lazybox_agents::PtyReading {
            state: lazybox_ipc::AgentState::Done,
            clear: true,
            progress: false,
            liveness: lazybox_agents::Liveness::Watchdog,
            ready_for_prompt: false,
        },
        terminals,
        bus,
        durability,
        id,
        session_key,
        state_machine,
        None,
    )
    .await;
}

/// Shared tail of both PTY state paths (`note_pty_activity`,
/// `classify_quiet_screen`): the hooks-primary gate, the state-machine
/// fold under the shared state-ownership boundary, and — on a real change —
/// the ordered cache update + emit. Lifted out of the output pump's spawn
/// closure so the emitted-on-change sequence is unit-testable (the
/// #167/#161 bugs were about the transition stream, not single-frame
/// classification).
/// Screen-scrape the agent's live model + reasoning effort from the detect
/// window and, when it differs from the terminal's cached label, update the
/// cache and broadcast [`Event::TerminalModelChanged`].
///
/// It writes the same `terminal_models` entry the spawn-time tier label uses,
/// so the live reading (Codex's `<model> <effort>` footer) supersedes the tier
/// for BOTH the sidebar model badge and the terminal tab badge, and flows into
/// the reconnect snapshot's `model_label` unchanged. Agents with no PTY model
/// reading (`detect_model_effort` → `None`, e.g. Claude — its pinned `--model`
/// tier label already names the model) are a cheap no-op. Broadcast only on a
/// change, so a resting Codex composer doesn't re-emit every settle.
async fn detect_and_broadcast_model(
    agent: &std::sync::Arc<dyn lazybox_agents::Agent>,
    detect_window: &[u8],
    terminals: &TerminalRegistry,
    bus: &tokio::sync::broadcast::Sender<Event>,
    id: TerminalId,
    // Fallback session key; the live key is re-resolved from `terminal_meta`
    // so a terminal rebadged onto a PR broadcasts under the live session.
    captured: &SessionKey,
) {
    let Some(model_label) = agent.detect_model_effort(detect_window) else {
        return;
    };
    let session_key = terminals
        .terminal_meta
        .lock()
        .await
        .get(&id)
        .map(|(sk, _)| sk.clone())
        .unwrap_or_else(|| captured.clone());
    let mut models = terminals.terminal_models.lock().await;
    if models
        .get(&id)
        .is_some_and(|current| *current == model_label)
    {
        return;
    }
    models.insert(id, model_label.clone());
    drop(models);
    let _ = bus.send(Event::TerminalModelChanged {
        session_key,
        terminal_id: id,
        model_label,
    });
}

async fn commit_pty_reading(
    agent: &std::sync::Arc<dyn lazybox_agents::Agent>,
    detect_window: &[u8],
    pty: lazybox_agents::PtyReading,
    terminals: &TerminalRegistry,
    bus: &tokio::sync::broadcast::Sender<Event>,
    durability: &AgentStateDurability,
    id: TerminalId,
    // Captured at spawn — used only as a fallback. The live key
    // is re-resolved from `terminal_meta` at emit time so a
    // terminal rebadged onto a PR (issue→PR collapse) broadcasts
    // its state under the PR session, not the deleted issue one.
    session_key: &SessionKey,
    state_machine: &mut lazybox_agents::AgentStateMachine,
    output_seq: Option<u64>,
) {
    if terminal_io::suppresses_agent_reading(terminals, id, output_seq).await
        && pty.state != lazybox_ipc::AgentState::InputNeeded
    {
        tracing::debug!(
            terminal_id = ?id,
            liveness = ?pty.liveness,
            state = ?pty.state,
            "client-provoked terminal redraw: suppressing lifecycle reading",
        );
        return;
    }
    // The pump gathers facts and defers every decision to the state
    // machine (`on_pty_reading`), which owns the whole hooks-primary gate
    // and hysteresis. The only fact the machine can't derive itself is how
    // long ago this terminal last spoke a lifecycle hook — a hook-driven
    // terminal is in the map, a pure screen-scraped one never is.
    //
    // Read outside the `states` lock the fold takes below, so a hook
    // ingesting in that window can leave this age one frame stale. Safe:
    // hook freshness is a soft signal (it only shifts a reading between
    // "gated" and "folded", never fabricates a state), the transition
    // table re-validates whatever folds, and the very next chunk re-reads
    // a fresh age — the same read-then-decide shape the pre-gate pump had.
    let since_last_hook = terminals
        .hook_driven_terminals
        .lock()
        .await
        .get(&id)
        .map(|at| at.elapsed());
    // The liveness tier that produced this reading is the PTY path's "why"
    // (#538): a byte-silent quiet-timer settle, a content-stable watchdog
    // force, or ordinary streaming (a live dialog surfacing, or a byte-flow
    // Working). The most common stuck-status question — "why did this
    // settle to Done / why is it still Working" — is answered by this tier.
    let reason = match (pty.liveness, pty.state) {
        (lazybox_agents::Liveness::Silent, _) => "pty-quiet-settle",
        (lazybox_agents::Liveness::Stalled | lazybox_agents::Liveness::Watchdog, _) => {
            "pty-watchdog-force"
        }
        (lazybox_agents::Liveness::Streaming, lazybox_ipc::AgentState::InputNeeded) => "pty-dialog",
        (lazybox_agents::Liveness::Streaming, _) => "pty-stream",
    };
    // Decide, insert, and broadcast under the same canonical lock boundary.
    // A separate cache insert followed by an unlocked broadcast allowed a
    // concurrent hook/exit to commit second but publish first, presenting the
    // client with a state older than the cache.
    let folded = fold_and_broadcast_agent_state(
        terminals,
        bus,
        durability,
        id,
        session_key,
        StateSource::Pty,
        reason,
        |current, terminal_live| {
            if !terminal_live {
                return ((lazybox_agents::Outcome::Rejected, current), None);
            }
            let outcome = state_machine.on_pty_reading(
                current,
                pty,
                since_last_hook,
                // Lazy: the dialog-supersession scan re-strips the window,
                // so only the one reading that needs it (stale hooks +
                // Working demoting a cached `?`) pays for it.
                || agent.working_reading_supersedes_dialog(detect_window),
            );
            let committed = match outcome {
                lazybox_agents::Outcome::Committed(state) => Some(state),
                _ => None,
            };
            ((outcome, current), committed)
        },
    )
    .await;
    let (result, previous) = folded.result;
    let outcome = if folded.committed {
        result
    } else if matches!(result, lazybox_agents::Outcome::Committed(_)) {
        lazybox_agents::Outcome::Rejected
    } else {
        result
    };
    if previous == Some(lazybox_ipc::AgentState::Working)
        && pty.state == lazybox_ipc::AgentState::Idle
        && outcome == lazybox_agents::Outcome::Rejected
    {
        tracing::error!(
            terminal_id = ?id,
            "agent state invariant: refused Working → Idle PTY classification"
        );
    }
    match outcome {
        // Keep flap-damping and hook-gating visible at debug — a stuck /
        // missing `?` pill is bisected from these lines. (Steady-state and
        // structural rejections stay silent to avoid flooding at 100+
        // chunks/sec.)
        lazybox_agents::Outcome::Damped => tracing::debug!(
            terminal_id = ?id,
            new_state = ?pty.state,
            "state hysteresis: damped ambiguous flap",
        ),
        lazybox_agents::Outcome::Gated => tracing::debug!(
            terminal_id = ?id,
            new_state = ?pty.state,
            ?since_last_hook,
            "hooks-primary gate: PTY reading suppressed by a fresh hook",
        ),
        lazybox_agents::Outcome::Committed(_)
        | lazybox_agents::Outcome::Unchanged
        | lazybox_agents::Outcome::Rejected => {}
    }
}

/// Byte ceiling on a single `TerminalScrollback` reply. The out-of-process
/// transport rejects any event frame over [`MAX_FRAME_BYTES`] *fatally* —
/// the sender tears the connection down rather than dropping one event — so
/// an unbounded deep-scrollback capture (which grows with
/// `terminal.scrollback_lines`) could disconnect a remote client instead of
/// merely missing the fetch. Held a margin below the frame limit to leave
/// room for the event's other fields and bincode framing.
const MAX_SCROLLBACK_REPLAY_BYTES: usize = MAX_FRAME_BYTES as usize - 1024 * 1024;

/// Bound a scrollback capture to what one event frame can carry, keeping
/// the most-recent bytes. The client anchors its viewport to the bottom and
/// its own VT can't hold unbounded history anyway, so dropping the *oldest*
/// lines beyond the cap loses nothing it could display — and it converts a
/// fatal oversized-frame disconnect into graceful truncation. The kept
/// prefix is advanced to the next line boundary so the reply never starts
/// mid-escape (at worst one already-truncated oldest line is dropped).
fn cap_scrollback_replay(replay: Vec<u8>, max_bytes: usize) -> Vec<u8> {
    if replay.len() <= max_bytes {
        return replay;
    }
    let tail_start = replay.len() - max_bytes;
    let aligned = replay[tail_start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(tail_start, |off| tail_start + off + 1);
    replay[aligned..].to_vec()
}

/// `Command::FetchScrollback` — hand the requesting client the
/// terminal's deep history from the backend's own retention (tmux
/// `capture-pane`), the same seed the restart/reattach path uses. The
/// reply goes to the requesting connection only: other clients keep
/// their local scrollback until they ask themselves, so a fetch never
/// resets a grid nobody asked to rebuild.
pub async fn handle_fetch_scrollback(
    config: &ServerConfig,
    tx: &lazybox_ipc::EventSender,
    terminal_id: TerminalId,
) {
    let Some(key) = config.terminal.backend_key_for(terminal_id).await else {
        tracing::trace!("scrollback fetch for unknown terminal {terminal_id:?}");
        return;
    };
    match config.backend.scrollback(&key).await {
        Ok(Some((replay, seq))) => {
            let replay = cap_scrollback_replay(replay, MAX_SCROLLBACK_REPLAY_BYTES);
            let _ = tx.send(Event::TerminalScrollback {
                terminal_id,
                replay,
                seq,
            });
        }
        // No history from the backend. Usually benign: a raw-PTY session's
        // client already holds every byte in its ring. But a tmux pane
        // stuck on the alternate screen — spawned on a stale/older-build
        // server that still allowed it (#919) — retains ZERO history and
        // can't be healed in place, so scroll-up would silently show
        // nothing. Tell the client to reopen the session instead, reusing
        // the older-build scrollback warning.
        Ok(None) => {
            if config.backend.history_disabled(&key).await {
                let _ = tx.send(Event::RecoveredTerminalsRequireRestart {
                    terminal_ids: vec![terminal_id],
                });
            }
        }
        Err(e) => {
            tracing::debug!(key = %key, "backend scrollback fetch failed: {e}");
        }
    }
}

pub async fn handle_write(
    config: &ServerConfig,
    terminal_id: TerminalId,
    bytes: &[u8],
    intent: TerminalInputIntent,
) -> bool {
    handle_write_batch(config, terminal_id, &[bytes.to_vec()], intent).await
}

/// Deliver adjacent client writes in one backend call while retaining their
/// logical boundaries for prompt-answer detection. Concatenating bytes and
/// then checking `len() == 1` would hide a queued bare chooser answer.
pub(crate) async fn handle_write_batch(
    config: &ServerConfig,
    terminal_id: TerminalId,
    writes: &[Vec<u8>],
    intent: TerminalInputIntent,
) -> bool {
    let total: usize = writes.iter().map(Vec::len).sum();
    let mut joined = Vec::with_capacity(total);
    for bytes in writes {
        joined.extend_from_slice(bytes);
    }
    let answered_chooser = writes
        .iter()
        .any(|bytes| bytes.len() == 1 && matches!(bytes[0], b'1'..=b'9' | b'y' | b'n' | 0x1b));
    let current_state = config.terminal.agent_state_for(terminal_id).await;
    let chooser_submission = intent == TerminalInputIntent::Compose
        && current_state == Some(lazybox_ipc::AgentState::InputNeeded)
        && answered_chooser
        && config
            .terminal
            .input_needed_shapes
            .lock()
            .await
            .get(&terminal_id)
            .copied()
            == Some(lazybox_agents::PromptShape::Chooser);
    let submitted = intent == TerminalInputIntent::Submit || chooser_submission;
    let effective_intent = if submitted {
        TerminalInputIntent::Submit
    } else {
        intent
    };
    let Some(key) = config.terminal.backend_key_for(terminal_id).await else {
        tracing::trace!("write to unknown terminal {terminal_id:?}");
        return false;
    };
    let Some(interaction) = terminal_io::acquire_live(config, terminal_id, &key).await else {
        return false;
    };
    if let Err(error) =
        terminal_io::write_locked(config, terminal_id, &key, &joined, effective_intent).await
    {
        tracing::warn!(?terminal_id, %key, %error, "terminal input was not delivered");
        let _ = config.bus.send(Event::TerminalInputRejected {
            terminal_id,
            message: format!("input was not delivered ({error}); retry after checking the session"),
        });
        return false;
    }
    // A submitted input is affirmative evidence that the agent may start a
    // turn. Commit Working optimistically for agent terminals so a hookless
    // session can leave Idle/Done and an answered prompt can clear its `?`.
    // Compose and View writes never enter this path.
    //
    // An answer is either Enter (`\r`/`\n` — `y`/`yes`/`1`/<text> +
    // Enter; bracket-paste markers wrapping claude's submit count too)
    // OR a bare chooser keystroke: Claude's choosers accept a single
    // digit, y/n, or Esc (dismiss) with no Enter at all. Without the
    // bare-key arm, answering a chooser with `1` left the stale
    // markers pinning `InputNeeded` until fresh output evicted them.
    if !submitted {
        return true;
    }
    // Explicit Submit may start the first turn, resume an idle composer, or
    // start a new turn from Done. A bare chooser answer is narrower: it only
    // submits while the same chooser is still live. Revalidate atomically at
    // the state transition because the terminal can resolve during the write.
    let flippable = |state: Option<lazybox_ipc::AgentState>| {
        if chooser_submission {
            state == Some(lazybox_ipc::AgentState::InputNeeded)
        } else {
            !matches!(
                state,
                Some(lazybox_ipc::AgentState::Working | lazybox_ipc::AgentState::Exited { .. })
            )
        }
    };
    if !flippable(config.terminal.agent_state_for(terminal_id).await) {
        return true;
    }
    // A bare chooser keystroke only ANSWERS chooser/permission-shaped
    // prompts. For a free-text elicitation, a lone digit / y / n is
    // just typing into the field — flipping the pill on it cleared a
    // real "agent is waiting on you". Enter is exempt: it submits the
    // elicitation answer, so the flip is correct. The shape is recorded
    // at detection time by the agent observation (including its current-
    // chunk fast path) and by `handle_ingest_hook` (permission → chooser,
    // elicit → free text);
    // with no recorded shape we conservatively don't flip on a bare key.
    if chooser_submission {
        let shape = config
            .terminal
            .input_needed_shapes
            .lock()
            .await
            .get(&terminal_id)
            .copied();
        if shape != Some(lazybox_agents::PromptShape::Chooser) {
            tracing::debug!(
                ?terminal_id,
                ?shape,
                "bare chooser keystroke on a non-chooser prompt — keeping InputNeeded",
            );
            return true;
        }
    }
    let session_key = config
        .terminal
        .terminal_meta
        .lock()
        .await
        .get(&terminal_id)
        .map(|(sk, _)| sk.clone());
    let Some(session_key) = session_key else {
        return true;
    };
    let Some(durability) = agent_state_durability(config, terminal_id, &key).await else {
        return true;
    };
    // Atomic compare-and-set under the state lock keeps the flip behind the
    // same transition choke point as PTY readings and hooks.
    let transition = transition_and_broadcast_agent_state(
        &config.terminal,
        &config.bus,
        &durability,
        terminal_id,
        &session_key,
        StateSource::Flip,
        |current| flippable(current).then_some(lazybox_ipc::AgentState::Working),
    )
    .await;
    if !transition.committed {
        return true;
    }
    // Tell the output pump to drop its detection buffer on the next
    // chunk. Without this the just-answered prompt's markers linger in
    // the rolling window and re-fire InputNeeded on the very next
    // chunk — reverting this optimistic flip and pinning the `?` pill
    // back on until ~16 KiB of fresh output finally evicts the stale
    // prompt. (The regression behind issue #101: "the ? won't go away
    // after I answer.")
    config
        .terminal
        .agent_detect_resets
        .lock()
        .await
        .insert(terminal_id);
    drop(interaction);
    true
}

/// How long the inject path waits for an active permission gate /
/// chooser to clear before giving up. These prompts are user-blocking,
/// so resolution is normally seconds; the bound only stops an abandoned
/// prompt from leaking the waiter task indefinitely.
const INJECT_INPUT_DEADLINE: Duration = Duration::from_secs(120);

/// How long a deferred inject waits between forcing a live re-read of the
/// agent's screen (issue #869). The loop is level-triggered: each tick pokes
/// the PTY pump to reclassify and then re-checks the *fresh* cached state, so
/// a quiescent-but-ready agent releases on its own rather than parking until a
/// keystroke drives a transition. Short enough that the release feels
/// immediate; long enough that a genuine gate isn't re-scraped in a busy loop.
const INJECT_RECLASSIFY_POLL: Duration = Duration::from_millis(250);

/// Minimum byte-quiet a forced reclassify requires before it scrapes the
/// screen. A reclassify poke that lands mid-paint would read a torn frame;
/// while bytes still flow the injection releases off the transitions that flow
/// already produces, so the poke can safely no-op until the stream settles.
const RECLASSIFY_MIN_QUIET: Duration = Duration::from_millis(150);

/// Whether a pump should honor an on-demand reclassify poke (#869) and scrape
/// the screen now. Shared by both PTY pumps' reclassify arms. Three conditions:
///
/// - only agent terminals have a detector to run;
/// - the stream must have been byte-quiet for [`RECLASSIFY_MIN_QUIET`], else a
///   mid-paint scrape reads a torn frame (the injection still releases off the
///   transitions that live output produces);
/// - the terminal must NOT be sitting on a just-answered prompt's reset. While
///   that reset is latched, [`classify_quiet_screen`]'s reset branch
///   force-settles `Done` on the stale pre-answer buffer — a settle the
///   deliberate quiet/watchdog timers own only after a FULL window of silence.
///   Firing it here (150ms after the optimistic `Working` flip, before the
///   answer's first output clears the reset) would preempt that with a spurious
///   `Done` that wakes polling and flickers the pill.
async fn force_reclassify_allowed(
    agent_present: bool,
    last_output_at: tokio::time::Instant,
    terminals: &TerminalRegistry,
    id: TerminalId,
) -> bool {
    agent_present
        && last_output_at.elapsed() >= RECLASSIFY_MIN_QUIET
        && !terminals.agent_detect_resets.lock().await.contains(&id)
}

/// Base wait for proof an injected prompt's submit actually
/// registered — a `UserPromptSubmit` hook or a `Working` transition.
/// Claude fires `UserPromptSubmit` synchronously with the submit, so
/// in the healthy case this resolves in well under a second. Each
/// resend attempt in [`confirm_prompt_submission`] waits one more
/// multiple of this, so a cold start where Claude is still painting
/// gets progressively more slack instead of a hail of Enters.
const SUBMIT_CONFIRM_DEADLINE: Duration = Duration::from_secs(3);

/// Enter resends after the initial submit keystroke before the
/// confirm loop gives up loudly.
const SUBMIT_RESEND_LIMIT: u32 = 3;

/// Quiet window the paste-settle gate requires between output chunks
/// before the submit keystroke is written. While the agent is still
/// repainting from the paste, an Enter can coalesce into the batch and
/// become a soft line break; once output has been quiet this long, the
/// paste has visibly settled and Enter lands as its own keystroke.
const PASTE_QUIET_WINDOW: Duration = Duration::from_millis(250);

/// Upper bound on the paste-settle wait. A busy agent (boot spinner,
/// streaming ticker) may never go fully quiet; past this cap the
/// submit is written anyway and the confirm loop's resends carry the
/// recovery instead.
const PASTE_SETTLE_CAP: Duration = Duration::from_secs(2);

/// Stage-specific failure from the shared PTY prompt writer. Callers keep
/// their context-specific user message (initial work vs live injection), while
/// framing, settle timing, submit ordering, and confirmation live in one path.
#[derive(Debug, thiserror::Error)]
enum PromptWriteError {
    #[error("initial prompt write failed: {0}")]
    Initial(#[source] terminal_io::TerminalIoFailure),
    #[error("prompt submit write failed: {0}")]
    Submit(#[source] terminal_io::TerminalIoFailure),
}

/// Execute one agent-declared prompt sequence while holding the terminal's
/// interaction lock. This is the single shell/PTY wrapper used by both
/// spawn-time work delivery and injection into an existing agent.
///
/// For guarded composers the first write is an explicit bracketed paste. The
/// second write is delayed until its repaint settles, then registered for
/// confirmation before Enter is sent. The lock is released before the
/// potentially long confirmation/retry loop; retries reacquire it through the
/// normal serialized terminal-I/O path.
async fn write_prompt_sequence(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    encoded: lazybox_agents::EncodedPrompt,
    submit: bool,
    interaction: tokio::sync::OwnedMutexGuard<()>,
) -> Result<bool, PromptWriteError> {
    let echo_probes = encoded.echo_probes().to_vec();
    let (initial_write, submit_write) = encoded.into_writes();
    // Subscribe BEFORE the first write so its repaint chunks cannot race the
    // settle gate. Line-oriented prompts submit inline and skip this receiver.
    let output_events = submit_write.is_some().then(|| config.bus.subscribe());
    let initial_intent = if submit && submit_write.is_none() {
        TerminalInputIntent::Submit
    } else {
        TerminalInputIntent::Compose
    };
    terminal_io::write_locked(
        config,
        terminal_id,
        backend_key,
        &initial_write,
        initial_intent,
    )
    .await
    .map_err(PromptWriteError::Initial)?;

    if let (Some(submit_bytes), Some(mut output_events)) = (submit_write, output_events) {
        let settle_t0 = std::time::Instant::now();
        let settle = await_paste_settled(
            &mut output_events,
            terminal_id,
            &echo_probes,
            PASTE_QUIET_WINDOW,
            PASTE_SETTLE_CAP,
        )
        .await;
        tracing::info!(
            terminal_id = ?terminal_id,
            trigger = ?settle,
            settle_ms = settle_t0.elapsed().as_millis(),
            "prompt paste settled — sending submit keystroke",
        );
        let confirm = prepare_submit_confirmation(config, terminal_id).await;
        terminal_io::write_locked(
            config,
            terminal_id,
            backend_key,
            &submit_bytes,
            TerminalInputIntent::Submit,
        )
        .await
        .map_err(PromptWriteError::Submit)?;
        if submit {
            mark_done_agent_working(config, terminal_id, backend_key).await;
        }
        drop(interaction);
        return Ok(confirm_prompt_submission(
            confirm,
            config,
            backend_key,
            &submit_bytes,
            SUBMIT_CONFIRM_DEADLINE,
        )
        .await);
    }
    if submit {
        mark_done_agent_working(config, terminal_id, backend_key).await;
    }
    Ok(true)
}

async fn mark_done_agent_working(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
) {
    let Some((session_key, TerminalKind::Agent(_))) =
        config.terminal.terminal_meta_for(terminal_id).await
    else {
        return;
    };
    let Some(durability) = agent_state_durability(config, terminal_id, backend_key).await else {
        return;
    };
    transition_and_broadcast_agent_state(
        &config.terminal,
        &config.bus,
        &durability,
        terminal_id,
        &session_key,
        StateSource::Flip,
        |current| {
            (current == Some(lazybox_ipc::AgentState::Done))
                .then_some(lazybox_ipc::AgentState::Working)
        },
    )
    .await;
}

/// Wait plumbing for [`confirm_prompt_submission`], registered BEFORE
/// the submit keystroke is written so a fast hook can't race the
/// waiter (`Notify::notify_one` stores a permit; the bus receiver is
/// subscribed up front for the same reason).
struct SubmitConfirmation {
    terminal_id: TerminalId,
    signal: std::sync::Arc<tokio::sync::Notify>,
    events: tokio::sync::broadcast::Receiver<Event>,
    coordinator: SpawnCoordinator,
    bus: tokio::sync::broadcast::Sender<Event>,
}

/// Register the proof-of-submission watchers for `terminal_id`. Every
/// agent terminal gets one: hook-driven terminals confirm via the
/// `UserPromptSubmit` signal, and terminals without hooks still emit
/// `Working` transitions on the bus from PTY screen-scraping
/// (`maybe_emit_state_change`), so there is always evidence to wait
/// for.
async fn prepare_submit_confirmation(
    config: &ServerConfig,
    terminal_id: TerminalId,
) -> SubmitConfirmation {
    let signal = config.spawn.register_prompt_confirmation(terminal_id).await;
    SubmitConfirmation {
        terminal_id,
        signal,
        events: config.bus.subscribe(),
        coordinator: config.spawn.clone(),
        bus: config.bus.clone(),
    }
}

/// After the paste + Enter, wait for evidence the prompt actually
/// entered the agent's turn: the `UserPromptSubmit` hook (forwarded by
/// `handle_ingest_hook` through the registered signal) or a `Working`
/// transition on the bus. If neither arrives, the prompt is almost
/// certainly parked in the composer — the paste landed but the Enter
/// was swallowed as a soft line break (issue #122) — so the submit
/// keystroke is resent, up to [`SUBMIT_RESEND_LIMIT`] times with a
/// linearly growing wait per attempt. Only the Enter, never the prompt
/// body: the body did land, and resending it would duplicate the
/// instruction. When every attempt goes unconfirmed the give-up is
/// loud — a user-visible error on the bus, not just a log line — so a
/// parked prompt can't silently strand the agent.
///
/// Before every Enter (resend or give-up) the authoritative
/// `agent_states` cache is re-checked: a permission chooser can appear
/// AFTER the inject path's one-time `InputNeeded` gate, exactly as the
/// paste lands. A bare Enter into that chooser selects its default
/// answer — silently auto-approving a tool the user never saw — which
/// is the same hazard the spawn path's readiness gate exists to avoid
/// (see `await_inject_window`). So a chooser observed here aborts the
/// resend loop and fails loudly instead of typing into the dialog.
async fn confirm_prompt_submission(
    mut confirm: SubmitConfirmation,
    config: &ServerConfig,
    backend_key: &str,
    submit_bytes: &[u8],
    deadline: Duration,
) -> bool {
    let mut resends = 0u32;
    let mut blocked_on_input = false;
    let confirmed = loop {
        let wait = deadline * (resends + 1);
        if await_submit_evidence(
            &confirm.signal,
            &mut confirm.events,
            confirm.terminal_id,
            &config.terminal.agent_states,
            wait,
        )
        .await
        {
            break true;
        }
        // No evidence — but before touching the keyboard again, consult
        // the authoritative state: if the agent is parked on a
        // permission gate / chooser, that dialog owns input and a bare
        // Enter would ANSWER it (typically "Yes"). Abort instead; the
        // loud failure below tells the user the prompt didn't start.
        if config.terminal.agent_state_for(confirm.terminal_id).await
            == Some(lazybox_ipc::AgentState::InputNeeded)
        {
            blocked_on_input = true;
            break false;
        }
        if resends >= SUBMIT_RESEND_LIMIT {
            break false;
        }
        resends += 1;
        tracing::info!(
            terminal_id = ?confirm.terminal_id,
            "no UserPromptSubmit / Working within {wait:?} of the submit — \
             prompt likely parked in the composer; resending Enter \
             ({resends}/{SUBMIT_RESEND_LIMIT})",
        );
        match terminal_io::write_live(
            config,
            confirm.terminal_id,
            backend_key,
            submit_bytes,
            TerminalInputIntent::Submit,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => break false,
            Err(e) => {
                // The live mapping survived but the retry itself was
                // rejected. Keep that visible even though this confirmation
                // loop can no longer make progress.
                tracing::warn!(
                    terminal_id = ?confirm.terminal_id,
                    "submit resend failed: {e}"
                );
                let _ = confirm.bus.send(Event::TerminalInputRejected {
                    terminal_id: confirm.terminal_id,
                    message: format!(
                        "the injected prompt's submit retry failed ({e}) — open the terminal and press Enter"
                    ),
                });
                break false;
            }
        }
    };
    // Remove the registration only if it's still OURS. A second
    // injection on the same terminal replaces the map entry with its
    // own `Notify`; removing blindly here would delete THAT signal,
    // orphan its waiter, and trigger a spurious Enter resend into the
    // agent.
    confirm
        .coordinator
        .remove_prompt_confirmation(confirm.terminal_id, &confirm.signal)
        .await;
    if confirmed {
        return true;
    }
    if blocked_on_input {
        tracing::warn!(
            terminal_id = ?confirm.terminal_id,
            resends,
            "prompt submit unconfirmed and the agent is now on a permission \
             prompt — suppressing Enter resends (Enter would answer the prompt)",
        );
        let _ = confirm.bus.send(Event::TerminalInputRejected {
            terminal_id: confirm.terminal_id,
            message: "a permission prompt appeared while the injected prompt was being \
                      submitted, so Enter was not resent (it would answer the prompt) — \
                      answer the agent's prompt, then re-send the work if it didn't start"
                .into(),
        });
        return false;
    }
    tracing::warn!(
        terminal_id = ?confirm.terminal_id,
        "prompt submit never confirmed after {SUBMIT_RESEND_LIMIT} Enter resends — \
         giving up; the prompt is likely parked in the composer",
    );
    let _ = confirm.bus.send(Event::TerminalInputRejected {
        terminal_id: confirm.terminal_id,
        message: "the injected prompt looks parked unsubmitted in the agent's composer — \
                  open the terminal and press Enter to start it"
            .into(),
    });
    false
}

/// Which evidence released the paste-settle gate — logged so a slow
/// paste→Enter hop can be attributed (issue #425).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteSettle {
    /// The composer echoed the pasted text (or its collapsed-paste
    /// placeholder) — the paste is processed; Enter is safe immediately.
    Echo,
    /// Output went quiet for the full quiet window.
    Quiet,
    /// The cap elapsed on a terminal that never went quiet and never
    /// showed the echo; the confirm loop's resends own recovery.
    Cap,
}

/// Bound on the raw post-paste bytes retained for echo matching. The echo
/// repaint lands within the first frames after the paste, so this only
/// guards against a pathological output flood during the settle window.
const PASTE_ECHO_SCAN_CAP: usize = 128 * 1024;

/// Block between the paste write and the submit keystroke until there is
/// evidence the paste batch settled, whichever arrives first:
///
/// - **echo** — the terminal's output since the paste contains one of
///   `echo_probes` (the composer re-rendered with the pasted text). This is
///   the primary signal for TUIs that repaint continuously (Codex) and
///   therefore never satisfy a global quiet window (issue #425): a
///   repainting status line must not delay the Enter.
/// - **quiet** — no output for `quiet` (the pre-#425 heuristic, still the
///   path for agents that go quiet after the paste and the fallback when
///   the echo is not recognized).
/// - **cap** — the bounded worst case.
///
/// `events` must be subscribed BEFORE the paste write so the chunks it
/// produces are observable here.
async fn await_paste_settled(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    terminal_id: TerminalId,
    echo_probes: &[String],
    quiet: Duration,
    cap: Duration,
) -> PasteSettle {
    let cap_at = tokio::time::Instant::now() + cap;
    let mut quiet_at = tokio::time::Instant::now() + quiet;
    // Raw bytes seen since the paste, accumulated so an echo split across
    // chunk boundaries (or interleaved with cursor-move escapes) still
    // matches after ANSI-stripping and compaction.
    let mut seen: Vec<u8> = Vec::new();
    loop {
        match tokio::time::timeout_at(quiet_at.min(cap_at), events.recv()).await {
            // Quiet window or the cap elapsed — settled either way.
            Err(_) => {
                return if tokio::time::Instant::now() >= cap_at {
                    PasteSettle::Cap
                } else {
                    PasteSettle::Quiet
                };
            }
            Ok(Ok(Event::TerminalOutput {
                terminal_id: tid,
                bytes,
                ..
            })) if tid == terminal_id => {
                if seen.len() < PASTE_ECHO_SCAN_CAP {
                    seen.extend_from_slice(&bytes);
                    if lazybox_agents::detect::paste_echo_observed(&seen, echo_probes) {
                        return PasteSettle::Echo;
                    }
                }
                quiet_at = tokio::time::Instant::now() + quiet;
            }
            Ok(Ok(_)) => {}
            // Chunks were dropped — can't tell when output stopped, so
            // conservatively restart the quiet window (the cap still
            // bounds the total wait).
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                quiet_at = tokio::time::Instant::now() + quiet;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return PasteSettle::Quiet,
        }
    }
}

/// True when submission evidence arrived before `deadline`: the
/// per-terminal `UserPromptSubmit` signal, or an `Event::AgentState`
/// flipping this terminal to `Working`.
///
/// On a `Lagged` receiver the very `Working` transition may have been
/// dropped, so the authoritative `states` map is consulted (mirroring
/// [`poll_input_resolution`]) rather than ignoring the gap — an
/// unobserved flip must not trigger spurious Enter resends into an
/// already-working agent. Only a cached `Working` counts as evidence:
/// any resting state (`Idle`/`Done`) may simply predate the submit.
async fn await_submit_evidence(
    signal: &tokio::sync::Notify,
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    terminal_id: TerminalId,
    states: &std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<TerminalId, lazybox_ipc::AgentState>>,
    >,
    deadline: Duration,
) -> bool {
    let wait = async {
        tokio::select! {
            _ = signal.notified() => true,
            confirmed = async {
                loop {
                    match events.recv().await {
                        Ok(Event::AgentState {
                            terminal_id: tid,
                            state: lazybox_ipc::AgentState::Working,
                            ..
                        }) if tid == terminal_id => break true,
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if states.lock().await.get(&terminal_id)
                                == Some(&lazybox_ipc::AgentState::Working)
                            {
                                break true;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break false,
                    }
                }
            } => confirmed,
        }
    };
    matches!(tokio::time::timeout(deadline, wait).await, Ok(true))
}

/// Outcome of one level-triggered poll step in the deferred-inject loop.
enum InputPoll {
    /// A non-`InputNeeded` transition arrived (the gate cleared).
    Resolved,
    /// The terminal exited while still blocked.
    Exited,
    /// The poll interval elapsed with no transition — re-read the fresh
    /// cached state, which the reclassify poke may have just refreshed.
    Tick,
}

/// One step of the level-triggered deferred-inject wait (issue #869): wait up
/// to `step` for a resolving state transition or a terminal exit, returning
/// [`InputPoll::Tick`] when neither arrives in time so the caller can re-read
/// the freshly-reclassified cached state rather than block on an event that a
/// quiescent agent never emits.
async fn poll_input_resolution(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    terminal_id: TerminalId,
    states: &std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<TerminalId, lazybox_ipc::AgentState>>,
    >,
    step: Duration,
) -> InputPoll {
    let wait = async {
        loop {
            match events.recv().await {
                Ok(Event::AgentState {
                    terminal_id: tid,
                    state,
                    ..
                }) if tid == terminal_id => {
                    if state != lazybox_ipc::AgentState::InputNeeded {
                        return InputPoll::Resolved;
                    }
                }
                Ok(Event::TerminalExited {
                    terminal_id: tid, ..
                }) if tid == terminal_id => return InputPoll::Exited,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if states.lock().await.get(&terminal_id)
                        != Some(&lazybox_ipc::AgentState::InputNeeded)
                    {
                        return InputPoll::Resolved;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return InputPoll::Exited,
            }
        }
    };
    (tokio::time::timeout(step, wait).await).unwrap_or(InputPoll::Tick)
}

/// Owns the single in-flight readiness-gated prompt injection for a terminal.
///
/// The guard is moved into the background task so every completion path —
/// success, rejection, terminal exit, timeout, or task cancellation — releases
/// the reservation synchronously in `Drop`.
struct PendingInjectionGuard {
    terminal_id: TerminalId,
    pending: std::sync::Arc<parking_lot::Mutex<std::collections::HashSet<TerminalId>>>,
}

impl PendingInjectionGuard {
    fn claim(coordinator: &SpawnCoordinator, terminal_id: TerminalId) -> Option<Self> {
        let pending = coordinator.pending_prompt_injections.clone();
        if !pending.lock().insert(terminal_id) {
            return None;
        }
        Some(Self {
            terminal_id,
            pending,
        })
    }
}

impl Drop for PendingInjectionGuard {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.terminal_id);
    }
}

/// Whether an inject must defer because the agent is parked on a prompt a
/// pasted answer would corrupt. Only a free-text `InputNeeded` prompt (the
/// agent asking an open question) takes a pasted snippet as its answer, so
/// that shape alone delivers immediately (issue #725). Every other
/// `InputNeeded` reading defers: a chooser / permission / Y-N dialog owns
/// input, and — matching [`lazybox_agents::AgentObservation::from_state`],
/// which treats a bare/legacy `InputNeeded` as a chooser — an `InputNeeded`
/// reading with no recorded shape is presumed chooser-like rather than
/// pasted into blind. The two maps are never co-held: the state lock is
/// released before the shape lock.
async fn inject_must_defer(
    states: &tokio::sync::Mutex<std::collections::HashMap<TerminalId, lazybox_ipc::AgentState>>,
    shapes: &tokio::sync::Mutex<std::collections::HashMap<TerminalId, lazybox_agents::PromptShape>>,
    id: TerminalId,
) -> bool {
    if states.lock().await.get(&id).copied() != Some(lazybox_ipc::AgentState::InputNeeded) {
        return false;
    }
    shapes.lock().await.get(&id).copied() != Some(lazybox_agents::PromptShape::FreeText)
}

/// Inject a prompt into an existing agent terminal. Same paste +
/// submit split as the spawn-time initial_prompt path, just
/// targeted at a live terminal instead of a fresh one. Quietly
/// no-ops if the terminal isn't an agent (shell terminals don't
/// have `inject_prompt`) or doesn't exist.
pub async fn handle_inject_prompt(
    config: &ServerConfig,
    terminal_id: TerminalId,
    prompt: &str,
    fallback_spawn: Option<lazybox_ipc::SpawnFallback>,
    submit: bool,
) {
    handle_inject_prompt_inner(config, terminal_id, prompt, fallback_spawn, submit, None).await;
}

#[derive(Clone)]
struct SnippetDelivery {
    session_key: SessionKey,
    snippet_key: String,
    category: String,
    body: String,
}

async fn handle_inject_prompt_inner(
    config: &ServerConfig,
    terminal_id: TerminalId,
    prompt: &str,
    fallback_spawn: Option<lazybox_ipc::SpawnFallback>,
    submit: bool,
    snippet: Option<SnippetDelivery>,
) {
    // Look up — and drop the guard — before any further await so
    // a nested handle_spawn (in the fallback path) can re-acquire
    // the same lock without deadlocking. Without the explicit
    // scope, the temporary `MutexGuard` from the match scrutinee
    // lives for the entire match arm. The helpers acquire-then-drop
    // the lock inside one method call so no guard can outlive the
    // scrutinee.
    let backend_key = match config.terminal.backend_key_for(terminal_id).await {
        Some(k) => k,
        None => {
            // The TUI's cached terminal id is stale — the agent died
            // between the user's `w` press and this command arriving.
            // If a fallback was provided, rewrite this into a Spawn
            // so the user's prompt isn't silently lost.
            if let Some(fb) = fallback_spawn {
                tracing::info!(
                    "inject_prompt: terminal {terminal_id:?} gone — falling back to Spawn"
                );
                // Same autonomy rule as a direct prompt-carrying Spawn
                // (`spawn_is_autonomous`): the rewritten Spawn hands the
                // agent pre-built work to run unattended, so it must not
                // park on permission prompts no human is watching.
                let prompt = Some(prompt.to_string());
                let autonomous = spawn_is_autonomous(&prompt);
                handle_spawn(
                    config,
                    fb.session_key,
                    fb.session_id,
                    fb.kind,
                    SpawnOptions {
                        cwd: fb.cwd,
                        initial_prompt: prompt,
                        autonomous,
                        model_alias: fb.model_alias,
                        access: fb.access,
                        client_request_id: fb.client_request_id,
                        ..Default::default()
                    },
                )
                .await;
                return;
            }
            tracing::debug!("inject_prompt to unknown terminal {terminal_id:?}");
            return;
        }
    };
    let kind = match config.terminal.terminal_meta_for(terminal_id).await {
        Some((_session_key, kind)) => kind,
        None => {
            tracing::debug!("inject_prompt: no terminal_meta for {terminal_id:?} — skipping");
            return;
        }
    };
    let agent = match &kind {
        TerminalKind::Agent(id) => match config.agents.get(id) {
            Some(a) => a,
            None => {
                tracing::warn!(
                    "inject_prompt: unknown agent id `{id}` for terminal {terminal_id:?}"
                );
                return;
            }
        },
        _ => {
            tracing::debug!("inject_prompt: terminal {terminal_id:?} is not an agent — skipping");
            return;
        }
    };
    // The PTY protocol owns BOTH framing and submission. In particular,
    // compose-only recall must omit the inline newline for line-oriented
    // agents as well as the separate CR used by guarded composers.
    let intent = if submit {
        lazybox_agents::PromptIntent::Submit
    } else {
        lazybox_agents::PromptIntent::Compose
    };
    let encoded_prompt = agent.encode_prompt(prompt, intent);

    // An InputNeeded gate may hold the waiter below for 30 seconds. Without a
    // per-terminal reservation every repeated `w` press spawned another
    // waiter, and all of them pasted once the gate cleared. Reject duplicates
    // explicitly instead of growing background work and duplicating input.
    let Some(pending_injection) = PendingInjectionGuard::claim(&config.spawn, terminal_id) else {
        let _ = config.bus.send(Event::TerminalInputRejected {
            terminal_id,
            message: "a prompt injection is already waiting for this agent — answer its prompt before retrying"
                .into(),
        });
        return;
    };

    // Readiness gate (issue #32, refined by #725). If the agent is parked
    // on a permission gate / chooser / Y-N prompt, that dialog owns input —
    // it expects `y`/`n`/`1`/`2`, not a pasted prompt. Writing the paste now
    // feeds it into the dialog, which rejects it, and the injection is
    // silently lost. Claude emits these prompts at ANY point in a session,
    // not just at spawn, so the inject path needs its own gate: wait for the
    // prompt to clear, then deliver the context.
    //
    // But a free-text `InputNeeded` prompt (the agent asking an open
    // question) is itself waiting for composed text — the pasted snippet IS
    // the answer, so deferring it deadlocks (the prompt never clears because
    // it's waiting for this very input). Gate on the prompt SHAPE, not on
    // `InputNeeded` alone: only a free-text prompt delivers now; every other
    // shape defers.
    //
    // Subscribe BEFORE reading the current state so a transition that
    // races between the read and the wait isn't missed.
    let events = config.bus.subscribe();
    let shapes = config.terminal.input_needed_shapes.clone();
    let blocked = inject_must_defer(&config.terminal.agent_states, &shapes, terminal_id).await;
    let states = config.terminal.agent_states.clone();
    let bus = config.bus.clone();
    let id = terminal_id;
    let config_for_confirm = config.clone();
    let snippet_for_confirm = snippet;
    // The per-terminal command lane may advance only after this task has
    // established its ordering position. An immediately-ready injection
    // acknowledges after taking the global interaction lock, so a following
    // Write cannot overtake it. A blocked injection acknowledges after its
    // readiness waiter is registered, deliberately leaving the lane free for
    // the Write that answers the permission/chooser gate.
    let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _pending_injection = pending_injection;
        let deadline = tokio::time::Instant::now() + INJECT_INPUT_DEADLINE;
        let mut events = events;
        let mut blocked = blocked;
        let mut registered_tx = Some(registered_tx);
        if blocked && let Some(tx) = registered_tx.take() {
            let _ = tx.send(());
        }
        while blocked {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!(
                    terminal_id = ?id,
                    "inject_prompt: agent still blocked on input after {INJECT_INPUT_DEADLINE:?}; dropping injection rather than feeding it into the prompt"
                );
                // The drop must be visible, not just a log line — the
                // user pressed `w` and their prompt evaporated.
                let _ = bus.send(Event::TerminalInputRejected {
                    terminal_id: id,
                    message: "the agent stayed on a permission prompt, so the injected work \
                              context was dropped — answer the prompt and press w again"
                        .into(),
                });
                return;
            }
            // Level-triggered readiness poll (#869). Ask the PTY pump to
            // re-read the live screen NOW, then wait a bounded step for the
            // resulting transition before re-checking the fresh cached state.
            // A quiescent agent whose gate already cleared releases here on
            // its own — no keystroke to drive the transition — while a genuine
            // gate re-scrapes as `InputNeeded` and stays parked. Without the
            // poke the wait would block on a bus transition a resting agent
            // never emits, so the injection sat until an inbound keystroke.
            config_for_confirm.terminal.request_reclassify(id).await;
            let step = INJECT_RECLASSIFY_POLL.min(remaining);
            match poll_input_resolution(&mut events, id, &states, step).await {
                // The terminal exited; fall through to `acquire_live`, which
                // recognizes the gone terminal and returns quietly.
                InputPoll::Exited => break,
                // A transition arrived — possibly the optimistic
                // InputNeeded → Working flip from a keystroke, or a just-
                // answered gate re-rendering another chooser. Let that output
                // settle before trusting the re-read.
                InputPoll::Resolved => tokio::time::sleep(INJECT_RECLASSIFY_POLL).await,
                // The step elapsed with no transition — the reclassify poke
                // above may have refreshed the cache; re-read it directly.
                InputPoll::Tick => {}
            }
            blocked = inject_must_defer(&states, &shapes, id).await;
        }
        let Some(interaction) =
            terminal_io::acquire_live(&config_for_confirm, id, &backend_key).await
        else {
            if let Some(tx) = registered_tx.take() {
                let _ = tx.send(());
            }
            tracing::debug!(
                ?id,
                "inject_prompt: terminal exited before interaction began"
            );
            return;
        };
        if let Some(tx) = registered_tx.take() {
            let _ = tx.send(());
        }
        match write_prompt_sequence(
            &config_for_confirm,
            id,
            &backend_key,
            encoded_prompt,
            submit,
            interaction,
        )
        .await
        {
            Ok(true) => {
                if let Some(snippet) = snippet_for_confirm {
                    let prompt = UserPrompt {
                        text: snippet.body.clone(),
                        timestamp_ms: Utc::now().timestamp_millis().max(0) as u64,
                        source: PromptSource::Snippet {
                            key: snippet.snippet_key.clone(),
                            category: snippet.category,
                        },
                    };
                    record_confirmed_snippet(
                        &config_for_confirm,
                        id,
                        snippet.session_key,
                        snippet.snippet_key,
                        Some(prompt),
                    )
                    .await;
                }
            }
            Ok(false) => {}
            Err(PromptWriteError::Initial(e)) => {
                tracing::warn!("inject_prompt: initial write failed: {e}");
                let _ = bus.send(Event::TerminalInputRejected {
                    terminal_id: id,
                    message: format!(
                        "injected prompt was not delivered ({e}) — press w again to retry"
                    ),
                });
            }
            Err(PromptWriteError::Submit(e)) => {
                tracing::warn!("inject_prompt: submit failed: {e}");
                let _ = bus.send(Event::TerminalInputRejected {
                    terminal_id: id,
                    message: format!(
                        "prompt was pasted but could not be submitted ({e}) — open the terminal and press Enter"
                    ),
                });
            }
        }
    });
    // A dropped sender means the task ended before it could establish either
    // position; there is no ordering work left for the lane to wait on.
    let _ = registered_rx.await;
}

/// Deliver a snippet through the terminal-kind-specific path and commit its
/// histories only after that path reports success. The workspace identity is
/// derived from daemon-owned terminal metadata rather than accepted from the
/// client.
pub async fn handle_deliver_snippet(
    config: &ServerConfig,
    terminal_id: TerminalId,
    snippet_key: String,
    category: String,
    body: String,
    submit: bool,
) {
    let Some((session_key, kind)) = config.terminal.terminal_meta_for(terminal_id).await else {
        let _ = config.bus.send(Event::TerminalInputRejected {
            terminal_id,
            message: "snippet was not delivered because the terminal is no longer running".into(),
        });
        return;
    };
    match kind {
        TerminalKind::Agent(_) => {
            // A no-submit insert (`Shift-Enter`) drops the snippet into the
            // composer for editing — it is not sent, so it records no
            // history/MRU (the `delivery` is passed only when submitting).
            let delivery = submit.then(|| SnippetDelivery {
                session_key,
                snippet_key,
                category,
                body: body.clone(),
            });
            handle_inject_prompt_inner(config, terminal_id, &body, None, submit, delivery).await;
        }
        TerminalKind::Shell => {
            let intent = if submit {
                TerminalInputIntent::Submit
            } else {
                TerminalInputIntent::Compose
            };
            if handle_write(
                config,
                terminal_id,
                &encode_shell_snippet(&body, submit),
                intent,
            )
            .await
                && submit
            {
                record_confirmed_snippet(config, terminal_id, session_key, snippet_key, None).await;
            }
        }
        TerminalKind::LogTail { .. } => {
            let _ = config.bus.send(Event::TerminalInputRejected {
                terminal_id,
                message: "snippets cannot be sent to a read-only log terminal".into(),
            });
        }
    }
}

async fn record_confirmed_snippet(
    config: &ServerConfig,
    terminal_id: TerminalId,
    session_key: SessionKey,
    snippet_key: String,
    prompt: Option<UserPrompt>,
) {
    if let Some(prompt) = &prompt {
        handle_record_user_message(config, terminal_id, prompt).await;
    }
    client_kv::record_recent_snippet(config, snippet_key.clone()).await;
    let workspace_key = WorkspaceKey::new(session_key.as_str().to_string());
    crate::workspace::record_sent_snippet(config, &workspace_key, snippet_key.clone()).await;
    let _ = config.bus.send(Event::SnippetDelivered {
        terminal_id,
        session_key,
        snippet_key,
        prompt,
    });
}

/// Encode a snippet body for a shell command line. `submit` appends the
/// trailing CR that runs the command (`Enter`); when `false` the body is
/// left on the command line unsubmitted so the user can edit it before
/// pressing Enter themselves (`Shift-Enter`, issue #791).
fn encode_shell_snippet(body: &str, submit: bool) -> Vec<u8> {
    let body = lazybox_agents::trim_leading_blank_lines(body);
    if !body.contains('\n') {
        let mut bytes = Vec::with_capacity(body.len() + 1);
        bytes.extend_from_slice(body.as_bytes());
        if submit {
            bytes.push(b'\r');
        }
        return bytes;
    }
    let mut bytes = Vec::with_capacity(body.len() + 16);
    bytes.extend_from_slice(b"\x1b[200~");
    for (index, line) in body.split('\n').enumerate() {
        if index > 0 {
            bytes.push(b'\r');
        }
        bytes.extend_from_slice(line.as_bytes());
    }
    bytes.extend_from_slice(b"\x1b[201~");
    if submit {
        bytes.push(b'\r');
    }
    bytes
}

pub async fn handle_resize(config: &ServerConfig, terminal_id: TerminalId, cols: u16, rows: u16) {
    let Some(key) = config.terminal.backend_key_for(terminal_id).await else {
        return;
    };
    if let Err(error) = terminal_io::resize_live(config, terminal_id, &key, cols, rows).await {
        tracing::warn!(?terminal_id, %key, cols, rows, %error, "backend resize failed");
    }
}

/// Request backend termination. Returns true once the backend accepted the
/// kill (including its idempotent already-gone case), false when the live
/// terminal should keep accepting input so the user can retry. The pump task
/// drains remaining output, observes the stream close, and owns the eventual
/// `Event::TerminalExited`.
pub async fn handle_close(
    config: &ServerConfig,
    terminal_id: TerminalId,
    client_request_id: Option<&str>,
) -> bool {
    if let Some(result) = crate::agent_auth::close_failed_auth_terminal(config, terminal_id).await {
        if let Err(error) = &result {
            tracing::warn!(?terminal_id, %error, "backend auth-terminal kill failed");
            if let Some(client_request_id) = client_request_id {
                let _ = config.bus.send(Event::CommandFailed {
                    client_request_id: client_request_id.into(),
                    message: format!("could not close authentication terminal: {error}"),
                });
            }
        }
        return result.is_ok();
    }
    let Some(key) = config.terminal.backend_key_for(terminal_id).await else {
        return true;
    };
    let Some(_guard) = terminal_io::acquire_live(config, terminal_id, &key).await else {
        return true;
    };
    if let Err(e) = config.backend.kill(&key).await {
        tracing::warn!("backend kill {key}: {e}");
        if let Some(client_request_id) = client_request_id {
            let _ = config.bus.send(Event::CommandFailed {
                client_request_id: client_request_id.into(),
                message: format!("could not close source terminal: {e}"),
            });
        }
        return false;
    }
    config.agent_recovery.forget(terminal_id).await;
    true
}

/// Handle a `Command::IngestHook`: a structured lifecycle hook fired by
/// an agent and forwarded by `lazybox hook-ingest`. Marks the terminal
/// hook-driven (so the PTY pump yields `Working`/`InputNeeded` to hooks)
/// and, when the hook implies a state, broadcasts the transition.
///
/// Correlation is by `backend_key` — the stable backend session key the
/// daemon baked into the hook command at spawn — reverse-resolved over
/// the live `terminals` map. The backend key survives daemon restarts,
/// so a tmux-surviving agent's hooks keep landing on the right terminal
/// after `recover_sessions` re-registers it; a wire `TerminalId` would
/// name a different terminal in the new process. Hooks that resolve to
/// nothing are dropped. Hooks carrying only the legacy `terminal_id`
/// (a settings file written before backend-key correlation) are
/// dropped too: every settings file this daemon writes carries the
/// backend key, so a legacy-only hook can only come from an older
/// process whose id space has no relation to ours — accepting it risks
/// cross-terminal state corruption, while dropping it just leaves that
/// session on PTY detection, same as before it had hooks at all.
///
/// Deduped against the cached `agent_states` so a stream of `PreToolUse`
/// hooks doesn't re-broadcast `Working` on every tool call — the
/// `AgentState` event only fires on an actual change, exactly like the
/// PTY path.
pub async fn handle_ingest_hook(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: Option<String>,
    hook: lazybox_ipc::HookEvent,
) {
    let resolved_backend_key = match backend_key.as_deref() {
        Some(key) => key.to_string(),
        None => {
            tracing::debug!(
                ?terminal_id,
                kind = ?hook.kind,
                "legacy terminal-id-only hook (pre-backend-key settings file), dropping"
            );
            return;
        }
    };
    let terminal_id = match backend_key.as_deref() {
        Some(key) => {
            let resolved = {
                let terminals = config.terminal.terminals.lock().await;
                terminals
                    .iter()
                    .find_map(|(id, k)| (k == key).then_some(*id))
            };
            match resolved {
                Some(id) => id,
                None => {
                    tracing::debug!(
                        backend_key = %key,
                        kind = ?hook.kind,
                        "hook for unknown backend key, dropping"
                    );
                    return;
                }
            }
        }
        None => unreachable!("backend key was checked above"),
    };
    // Resolve the workspace; a terminal mid-teardown (terminals entry
    // resolved but meta already swept) is dropped without marking
    // anything hook-driven.
    let (session_key, terminal_kind) = {
        let meta = config.terminal.terminal_meta.lock().await;
        match meta.get(&terminal_id) {
            Some((sk, kind)) => (sk.clone(), kind.clone()),
            None => {
                tracing::debug!(?terminal_id, kind = ?hook.kind, "hook for unknown terminal, dropping");
                return;
            }
        }
    };
    if let (TerminalKind::Agent(agent_id), Some(provider_session_id)) =
        (&terminal_kind, hook.session_id.as_deref())
    {
        config
            .agent_recovery
            .update_provider_session(terminal_id, provider_session_id.to_string())
            .await;
        if let Some(context) = config.agent_recovery.context(terminal_id).await {
            persist_agent_resume_context(config, &resolved_backend_key, &context).await;
        }
        if let Some(session_id) = config.terminal.terminal_session_for(terminal_id).await {
            persist_provider_session_id(
                config,
                &session_key,
                session_id,
                agent_id,
                provider_session_id,
            )
            .await;
        }
    }
    // From now on this terminal is hook-driven: the PTY detector defers
    // to hooks for Working/InputNeeded (until the timestamp recorded here
    // goes stale — see `lazybox_agents::HOOK_STALENESS`, consulted by the
    // state machine's gate). Done even for events that carry no state
    // change (e.g. SessionStart) — the signal is "this terminal speaks
    // hooks", not the specific transition.
    config
        .terminal
        .hook_driven_terminals
        .lock()
        .await
        .insert(terminal_id, std::time::Instant::now());
    terminal_io::clear_view_activity(config, terminal_id).await;
    // Proof-of-submission signal for the prompt-inject paths: a
    // `UserPromptSubmit` hook means the injected prompt actually
    // entered Claude's turn (issue #122's failure is the prompt parked
    // in the composer, which fires nothing).
    if hook.kind == lazybox_ipc::HookEventKind::UserPromptSubmit
        && let Some(signal) = config
            .spawn
            .prompt_submit_signals
            .lock()
            .await
            .get(&terminal_id)
    {
        signal.notify_one();
    }

    // Compare-and-set under one lock guard — a read under one
    // acquisition and an insert under another lets a concurrent PTY
    // pump transition slip between them and be clobbered. The hook →
    // state mapping consults the current state (an unrecognized
    // `Notification` is a no-change while `InputNeeded`), so it runs
    // under the same guard. `hook_to_state` yields the candidate; the
    // machine's transition table commits it (or rejects it — e.g. a
    // `SessionStart`/`SessionEnd` idle hook must not clear a `Done` the
    // preceding `Stop` just set, #80).
    let Some(durability) = agent_state_durability(config, terminal_id, &resolved_backend_key).await
    else {
        return;
    };
    // Record the prompt's shape — whether a bare chooser keystroke is a
    // complete answer — BEFORE the state is published, so a concurrent
    // inject (or `handle_write` optimistic flip) that observes the fresh
    // `InputNeeded` already sees the matching shape rather than a stale one
    // from the previous prompt (issue #725; the PTY paths order
    // shape-before-state the same way). Whether the hook yields
    // `InputNeeded` — the condition that gates recording — is a pure
    // function of the hook and the current state, so a plain cached read
    // suffices; the transition below still compare-and-sets the commit under
    // the state lock. Done even on a no-change re-assert (a chooser
    // following an elicitation, or vice versa, must update the gate), and
    // OUTSIDE the states guard so the two maps are never co-held.
    let cached_current = config.terminal.agent_state_for(terminal_id).await;
    if lazybox_agents::hook::hook_to_state(&hook, cached_current)
        == Some(lazybox_ipc::AgentState::InputNeeded)
    {
        config.terminal.input_needed_shapes.lock().await.insert(
            terminal_id,
            lazybox_agents::hook::notification_prompt_shape(hook.notification.as_deref()),
        );
    }
    let transition = transition_and_broadcast_agent_state(
        &config.terminal,
        &config.bus,
        &durability,
        terminal_id,
        &session_key,
        StateSource::Hook,
        |current| lazybox_agents::hook::hook_to_state(&hook, current),
    )
    .await;
    let Some(new_state) = transition.candidate else {
        return;
    };
    if !transition.committed {
        return;
    }
    // Hook-specific line (carries the originating `hook.kind`); the
    // source-tagged cache+broadcast line is emitted by the state owner.
    tracing::info!(
        ?terminal_id,
        %session_key,
        previous = ?transition.previous,
        state = ?new_state,
        hook = ?hook.kind,
        "hook → AgentState transition",
    );
}

async fn persist_provider_session_id(
    config: &ServerConfig,
    session_key: &SessionKey,
    session_id: SessionId,
    agent_id: &str,
    provider_session_id: &str,
) {
    let workspace_key = WorkspaceKey::new(session_key.as_str());
    let _guard = config.lock_workspace(workspace_key.as_str()).await;
    let mut workspace = match load_workspace(config, &workspace_key) {
        Ok(workspace) => workspace,
        Err(error) => {
            tracing::warn!(%workspace_key, %error, "could not persist provider session identity");
            return;
        }
    };
    let Some(session) = workspace
        .sessions
        .iter_mut()
        .find(|session| session.id == session_id)
    else {
        return;
    };
    if session
        .provider_session_ids
        .get(agent_id)
        .is_some_and(|stored| stored == provider_session_id)
    {
        return;
    }
    session
        .provider_session_ids
        .insert(agent_id.to_string(), provider_session_id.to_string());
    if let Err(error) = persist_and_broadcast(config, &workspace).await {
        tracing::warn!(%workspace_key, %error, "could not persist provider session identity");
    }
}

async fn pump_recovered_session(
    config: &ServerConfig,
    backend_key: &str,
    terminal_id: TerminalId,
    session_key: &SessionKey,
    agent: Option<std::sync::Arc<dyn lazybox_agents::Agent>>,
    restored_state: Option<lazybox_ipc::AgentState>,
    durability: Option<&AgentStateDurability>,
    mut sub: crate::backend::Subscription,
) {
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    let quiet_after = pty_quiet_classify_after(&cfg);
    let mut state_machine = restored_state.map_or_else(
        lazybox_agents::AgentStateMachine::new,
        lazybox_agents::AgentStateMachine::restored,
    );
    let mut state_buf = Vec::with_capacity(32 * 1024);
    let mut watchdog_fp = None;
    let mut working_watchdog =
        WorkingWatchdog::new(agent.as_ref().and(working_watchdog_after(&cfg)));
    let mut quiet_deadline = None;
    let mut last_chunk_len = 0;
    let mut last_output_at = tokio::time::Instant::now();
    let mut auth_required_emitted = false;

    if !sub.replay.is_empty() {
        replace_detection_history(&mut state_buf, &mut watchdog_fp, &sub.replay);
        note_pty_activity(
            agent.as_ref(),
            &mut state_buf,
            &[],
            sub.last_seq,
            false,
            &config.terminal,
            &config.bus,
            durability,
            terminal_id,
            session_key,
            &mut state_machine,
        )
        .await;
        maybe_emit_auth_required(
            config,
            agent.as_ref(),
            &state_buf,
            terminal_id,
            &mut auth_required_emitted,
        )
        .await;
        if agent.is_some() {
            last_output_at = tokio::time::Instant::now();
            quiet_deadline = Some(last_output_at + quiet_after);
        }
        let _ = config.bus.send(Event::TerminalOutput {
            terminal_id,
            bytes: sub.replay.clone(),
            first_seq: 1,
            seq: sub.last_seq,
        });
    }

    let mut last_seq = sub.last_seq;
    let mut resync_unavailable_announced = false;
    // On-demand reclassify poke (#869): mirror the primary pump so a deferred
    // inject into a recovered agent releases off a live re-read too.
    let reclassify = config.terminal.register_reclassify(terminal_id).await;
    loop {
        let watchdog_due =
            working_watchdog.prepare_select(tokio::time::Instant::now(), sub.live.len());
        tokio::select! {
            biased;
            chunk = sub.live.recv(), if working_watchdog.receiver_enabled(watchdog_due) => {
                let Some(chunk) = chunk else {
                    break;
                };
                working_watchdog.note_received(watchdog_due);
                if chunk.seq <= last_seq {
                    continue;
                }
                if chunk.seq > last_seq.saturating_add(1) {
                    let Some(snapshot) =
                        resync_replay_after_gap(&*config.backend, backend_key, chunk.seq, last_seq)
                            .await
                    else {
                        if !resync_unavailable_announced {
                            let _ = config.bus.send(Event::TerminalResyncUnavailable {
                                terminal_id,
                            });
                            resync_unavailable_announced = true;
                        }
                        continue;
                    };
                    resync_unavailable_announced = false;
                    let _ = terminal_io::suppresses_agent_reading(
                        &config.terminal,
                        terminal_id,
                        Some(snapshot.last_seq),
                    )
                    .await;
                    replace_detection_history(
                        &mut state_buf,
                        &mut watchdog_fp,
                        &snapshot.replay,
                    );
                    maybe_emit_auth_required(
                        config,
                        agent.as_ref(),
                        &state_buf,
                        terminal_id,
                        &mut auth_required_emitted,
                    )
                    .await;
                    last_chunk_len = 0;
                    if agent.is_some() {
                        last_output_at = tokio::time::Instant::now();
                        quiet_deadline = Some(last_output_at + quiet_after);
                    }
                    let _ = config.bus.send(Event::TerminalResync {
                        terminal_id,
                        replay: snapshot.replay,
                        seq: snapshot.last_seq,
                    });
                    last_seq = snapshot.last_seq;
                    continue;
                }
                last_seq = chunk.seq;
                if agent.is_some()
                    && config.terminal.agent_detect_resets.lock().await.remove(&terminal_id)
                {
                    state_buf.clear();
                }
                let progress =
                    agent.is_some() && watchdog_notes_progress(&mut watchdog_fp, &chunk.bytes);
                note_pty_activity(
                    agent.as_ref(),
                    &mut state_buf,
                    &chunk.bytes,
                    chunk.seq,
                    progress,
                    &config.terminal,
                    &config.bus,
                    durability,
                    terminal_id,
                    session_key,
                    &mut state_machine,
                )
                .await;
                maybe_emit_auth_required(
                    config,
                    agent.as_ref(),
                    &state_buf,
                    terminal_id,
                    &mut auth_required_emitted,
                )
                .await;
                last_chunk_len = chunk.bytes.len();
                if agent.is_some() {
                    let now = tokio::time::Instant::now();
                    last_output_at = now;
                    quiet_deadline = Some(now + quiet_after);
                    if progress {
                        working_watchdog.note_progress(now);
                    }
                }
                let _ = config.bus.send(Event::TerminalOutput {
                    terminal_id,
                    bytes: chunk.bytes,
                    first_seq: chunk.seq,
                    seq: chunk.seq,
                });
            }
            _ = tokio::time::sleep_until(
                quiet_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if quiet_deadline.is_some() && !watchdog_due => {
                quiet_deadline = None;
                classify_quiet_screen(
                    agent.as_ref(),
                    &state_buf,
                    last_chunk_len,
                    lazybox_agents::Liveness::Silent,
                    &config.terminal,
                    &config.bus,
                    durability,
                    terminal_id,
                    session_key,
                    &mut state_machine,
                )
                .await;
            }
            _ = tokio::time::sleep_until(
                working_watchdog.deadline().unwrap_or_else(tokio::time::Instant::now)
            ), if working_watchdog.deadline().is_some() => {
                if working_watchdog.fire(tokio::time::Instant::now()).is_none() {
                    continue;
                }
                watchdog_reverify_parked_turn(
                    agent.as_ref(),
                    &state_buf,
                    last_chunk_len,
                    &config.terminal,
                    &config.bus,
                    durability,
                    terminal_id,
                    session_key,
                    &mut state_machine,
                )
                .await;
            }
            _ = reclassify.notified() => {
                if force_reclassify_allowed(
                    agent.is_some(),
                    last_output_at,
                    &config.terminal,
                    terminal_id,
                )
                .await
                {
                    classify_quiet_screen(
                        agent.as_ref(),
                        &state_buf,
                        last_chunk_len,
                        lazybox_agents::Liveness::Silent,
                        &config.terminal,
                        &config.bus,
                        durability,
                        terminal_id,
                        session_key,
                        &mut state_machine,
                    )
                    .await;
                }
            }
        }
    }
}

fn replace_detection_history(
    state_buf: &mut Vec<u8>,
    watchdog_fp: &mut Option<u64>,
    replay: &[u8],
) {
    const STATE_BUF_CAP: usize = 32 * 1024;
    let start = replay.len().saturating_sub(STATE_BUF_CAP);
    state_buf.clear();
    state_buf.extend_from_slice(&replay[start..]);
    *watchdog_fp = None;
    let _ = watchdog_notes_progress(watchdog_fp, replay);
}

/// Recovery attaches allocate several descriptors apiece. Serializing them in
/// a small pool avoids a startup stampede when dozens of tmux sessions survive
/// a daemon restart, especially when the inherited descriptor limit is low.
const RECOVERY_ATTACH_CONCURRENCY: usize = 4;

/// A failed attach must not spin on the async runtime or flood the synchronous
/// log writer. The capped schedule remains self-healing while containing a
/// persistent backend/OS outage.
const RECOVERY_RETRY_MAX: Duration = Duration::from_secs(30);
const RECOVERY_HEALTHY_WINDOW: Duration = Duration::from_secs(30);

fn recovery_retry_delay(failures: u32, terminal_id: TerminalId) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    let base_secs = (1u64 << exponent).min(20);
    // Stable per-terminal spread prevents every failed survivor from waking
    // on the same boundary once the exponential component reaches its cap.
    let spread_cap_ms = if base_secs >= 20 { 10_000 } else { 500 };
    let spread_ms = terminal_id.0.wrapping_mul(6_364_136_223_846_793_005) % (spread_cap_ms + 1);
    Duration::from_secs(base_secs)
        .saturating_add(Duration::from_millis(spread_ms))
        .min(RECOVERY_RETRY_MAX)
}

fn should_warn_recovery_failure(failures: u32) -> bool {
    failures.is_power_of_two() || failures.is_multiple_of(10)
}

fn is_open_file_exhaustion(message: &str) -> bool {
    message.contains("Too many open files") || message.contains("os error 24")
}

fn reconstruct_legacy_agent_resume_context(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    session_key: &SessionKey,
    agent_id: &str,
    access: AgentRunAccess,
    no_permission: bool,
) -> Option<crate::agent_auth::AgentResumeContext> {
    let workspace_key = WorkspaceKey::new(session_key.as_str());
    let workspace = load_workspace(config, &workspace_key).ok()?;
    let session = workspace.sessions.iter().find(|session| {
        matches!(
            &session.kind,
            SessionKind::Agent {
                agent_id: session_agent_id
            } if session_agent_id == agent_id
        )
    })?;
    Some(crate::agent_auth::AgentResumeContext {
        terminal_id,
        session_key: session_key.clone(),
        session_id: Some(session.id),
        agent_id: agent_id.to_string(),
        cwd: session.worktree_path.clone(),
        backend_key: Some(backend_key.to_string()),
        on_main: false,
        model_alias: None,
        access,
        no_permission,
        provider_session_id: session.provider_session_ids.get(agent_id).cloned(),
        prompt_history: Vec::new(),
        composing_buffer: None,
    })
}

/// Bind already-running backend sessions to fresh wire TerminalIds.
/// Called once at server startup so lazybox restarts don't lose the
/// agents the user was running.
///
/// Per-survivor we look up the persisted `(session_key, kind)` pairing
/// (saved at spawn time, see `persist_terminal_meta`) so the sidebar
/// reattaches each PTY to its owning workspace. Survivors with no
/// persisted record fall back to a session_key=""/Shell placeholder —
/// rare in practice (only happens after a store wipe + dangling tmux),
/// and the user can clean those up via `x x`.
pub async fn recover_sessions(config: &ServerConfig) {
    let keys = match config.backend.list().await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("backend.list at startup: {e}");
            return;
        }
    };
    let live_keys: std::collections::HashSet<_> = keys.iter().cloned().collect();
    reconcile_missing_recovered_sessions(config, &live_keys).await;
    tracing::info!("recovering {} surviving session(s)", keys.len());
    let attach_permits =
        std::sync::Arc::new(tokio::sync::Semaphore::new(RECOVERY_ATTACH_CONCURRENCY));
    let resource_warning_emitted = std::sync::Arc::new(AtomicBool::new(false));
    for key in keys {
        let (session_key, kind) = load_terminal_meta(config, &key)
            .await
            .unwrap_or_else(|| (SessionKey::from(""), TerminalKind::Shell));
        let access = load_terminal_access(config, &key).await;
        let no_permission = load_no_permission(config, &key).await;
        let mut resume_context = match &kind {
            TerminalKind::Agent(agent_id) => load_agent_resume_context(config, &key)
                .await
                .filter(|context| context.agent_id == *agent_id),
            _ => None,
        };
        let required_generation = match &kind {
            TerminalKind::Agent(agent_id) => config
                .agents
                .get(agent_id)
                .map(|agent| agent.pty_launch_generation())
                .unwrap_or(0),
            _ => 0,
        };
        let persisted_generation = load_pty_launch_generation(config, &key).await.unwrap_or(0);
        let outdated_launch = required_generation > 0 && persisted_generation < required_generation;
        let terminal_id = alloc_terminal_id(&*config.store);
        if resume_context.is_none()
            && let TerminalKind::Agent(agent_id) = &kind
        {
            resume_context = reconstruct_legacy_agent_resume_context(
                config,
                terminal_id,
                &key,
                &session_key,
                agent_id,
                access,
                no_permission,
            );
        }
        if let Some(context) = &mut resume_context {
            let (prompt_history, composing_buffer) = tokio::join!(
                load_prompt_history(config, &key),
                load_composing_buffer(config, &key),
            );
            context.terminal_id = terminal_id;
            context.session_key = session_key.clone();
            context.backend_key = Some(key.clone());
            context.access = access;
            context.no_permission = no_permission;
            context.prompt_history = prompt_history;
            context.composing_buffer = composing_buffer;
        }
        let recovered_model_label = resume_context.as_ref().and_then(|context| {
            let cfg = lazybox_config::Config::load().unwrap_or_default();
            let models = cfg.agent_models(&context.agent_id);
            context
                .model_alias
                .as_deref()
                .and_then(|alias| models.tier(alias))
                .map(|tier| tier.label.clone())
        });
        let recovered_agent = if matches!(kind, TerminalKind::Agent(_)) {
            match load_recovered_agent_state(config, &key, terminal_id.0).await {
                Ok(restored) => Some(restored),
                Err(()) => continue,
            }
        } else {
            None
        };
        if access != AgentRunAccess::Default {
            config.terminal.record_access(terminal_id, access).await;
        }
        if let Some(context) = &resume_context {
            config
                .terminal
                .record_spawn_attributes(
                    terminal_id,
                    context.session_id,
                    context.access,
                    context.no_permission,
                    context.on_main,
                    recovered_model_label.as_deref(),
                )
                .await;
            config.agent_recovery.remember_spawn(context.clone()).await;
        }
        // Recover the primary maps as one visible registration, under the
        // same canonical lock pair as a fresh spawn. This prevents snapshot
        // or workspace-rebadge readers from observing a backend id without
        // its durable workspace owner (or vice versa).
        {
            let mut registration = config.terminal.lock_recovered_registration().await;
            let previous = registration.register(
                terminal_id,
                key.clone(),
                session_key.clone(),
                kind.clone(),
                recovered_agent
                    .as_ref()
                    .map(|(durability, state)| (durability.generation, *state)),
            );
            if previous.is_some() {
                tracing::error!(
                    ?terminal_id,
                    %key,
                    ?previous,
                    "agent state invariant: recovery replaced an existing hydrated state"
                );
            }
            if let Some((durability, state)) = &recovered_agent {
                tracing::info!(
                    ?terminal_id,
                    backend_key = %key,
                    generation = durability.generation,
                    state = ?state,
                    "agent state hydrated before terminal replay"
                );
            }
        }
        if no_permission {
            config
                .terminal
                .no_permission_terminals
                .lock()
                .await
                .insert(terminal_id);
        }
        if outdated_launch {
            config
                .terminal
                .outdated_agent_terminals
                .lock()
                .await
                .insert(terminal_id);
        }

        let config_for_pump = config.clone();
        let key_for_pump = key.clone();
        let session_key_for_pump = session_key.clone();
        let agent_for_pump = match &kind {
            TerminalKind::Agent(agent_id) => config.agents.get(agent_id),
            _ => None,
        };
        let restored_state = recovered_agent.as_ref().map(|(_, state)| *state);
        let state_durability = recovered_agent.map(|(durability, _)| durability);
        let attach_permits_for_pump = attach_permits.clone();
        let resource_warning_for_pump = resource_warning_emitted.clone();
        // Broadcast Spawned before spawning the pump — same race
        // guard as the main spawn path.
        let on_main = resume_context
            .as_ref()
            .is_some_and(|context| context.on_main);
        let _ = config.bus.send(Event::TerminalSpawned {
            terminal_id,
            session_key,
            kind,
            no_permission,
            on_main,
            model_label: recovered_model_label,
        });
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                if config_for_pump
                    .terminal
                    .backend_key_for(terminal_id)
                    .await
                    .as_deref()
                    != Some(key_for_pump.as_str())
                {
                    break;
                }

                let permit = match attach_permits_for_pump.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let attach_started = tokio::time::Instant::now();
                let subscribe_result = config_for_pump.backend.subscribe(&key_for_pump).await;
                drop(permit);
                let mut failure_reason = None;
                let attached = subscribe_result.is_ok();
                match subscribe_result {
                    Ok(sub) => {
                        let current_state = config_for_pump
                            .terminal
                            .agent_states
                            .lock()
                            .await
                            .get(&terminal_id)
                            .copied()
                            .or(restored_state);
                        pump_recovered_session(
                            &config_for_pump,
                            &key_for_pump,
                            terminal_id,
                            &session_key_for_pump,
                            agent_for_pump.clone(),
                            current_state,
                            state_durability.as_ref(),
                            sub,
                        )
                        .await;
                    }
                    Err(e) => {
                        failure_reason = Some(format!("subscribe failed: {e}"));
                    }
                }
                let attachment_lifetime = attach_started.elapsed();
                if attached && attachment_lifetime >= RECOVERY_HEALTHY_WINDOW {
                    failures = 0;
                }

                let permit = match attach_permits_for_pump.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let liveness = config_for_pump.backend.is_alive(&key_for_pump).await;
                drop(permit);
                match liveness {
                    Ok(false) => {
                        let exit_code = config_for_pump.backend.wait_exit(&key_for_pump).await;
                        teardown_exited_terminal(
                            &config_for_pump,
                            terminal_id,
                            &key_for_pump,
                            exit_code,
                        )
                        .await;
                        break;
                    }
                    Ok(true) => {
                        config_for_pump.backend.release(&key_for_pump).await;
                        failure_reason.get_or_insert_with(|| {
                            "output conduit ended while tmux session remained alive".into()
                        });
                    }
                    Err(error) => {
                        failure_reason.get_or_insert_with(|| {
                            format!("could not prove session liveness: {error}")
                        });
                    }
                }

                failures = failures.saturating_add(1);
                let retry_after = recovery_retry_delay(failures, terminal_id);
                let reason = failure_reason
                    .unwrap_or_else(|| "output conduit ended unexpectedly".to_string());
                if is_open_file_exhaustion(&reason)
                    && !resource_warning_for_pump.swap(true, Ordering::AcqRel)
                {
                    let _ = config_for_pump.bus.send(Event::provider_error_retryable(
                        "terminal",
                        "terminal recovery hit the process open-file limit; retries are throttled \
                         and existing sessions remain safe",
                    ));
                }
                if should_warn_recovery_failure(failures) {
                    tracing::warn!(
                        backend_key = %key_for_pump,
                        ?terminal_id,
                        failures,
                        retry_after_ms = retry_after.as_millis(),
                        %reason,
                        "recovered terminal attachment unavailable; retrying with backoff"
                    );
                } else {
                    tracing::debug!(
                        backend_key = %key_for_pump,
                        ?terminal_id,
                        failures,
                        retry_after_ms = retry_after.as_millis(),
                        %reason,
                        "recovered terminal attachment still unavailable"
                    );
                }
                tokio::time::sleep(retry_after).await;
            }
        });
    }
}

async fn reconcile_missing_recovered_sessions(
    config: &ServerConfig,
    live_keys: &std::collections::HashSet<String>,
) {
    let store = config.store.clone();
    let rows = match tokio::task::spawn_blocking(move || store.list_kv_prefix("terminal:")).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not enumerate persisted terminals for recovery");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "persisted terminal enumeration task failed");
            return;
        }
    };

    for (key, raw) in rows {
        let Some(backend_key) = key.strip_prefix("terminal:") else {
            continue;
        };
        if live_keys.contains(backend_key) {
            continue;
        }
        let parsed: (String, TerminalKind) = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::error!(
                    %backend_key,
                    %error,
                    "persisted terminal metadata is invalid; cannot reconcile missing session"
                );
                continue;
            }
        };
        let session_key = SessionKey::from(parsed.0.as_str());
        let kind = parsed.1;
        let terminal_id = alloc_terminal_id(&*config.store);
        let recovered_agent = if matches!(kind, TerminalKind::Agent(_)) {
            match load_recovered_agent_state(config, backend_key, terminal_id.0).await {
                Ok(restored) => Some(restored),
                Err(()) => continue,
            }
        } else {
            None
        };
        {
            config
                .terminal
                .lock_recovered_registration()
                .await
                .register(
                    terminal_id,
                    backend_key.to_string(),
                    session_key,
                    kind,
                    recovered_agent
                        .as_ref()
                        .map(|(durability, state)| (durability.generation, *state)),
                );
        }
        tracing::warn!(
            %backend_key,
            ?terminal_id,
            "persisted terminal is absent from backend inventory; committing Exited"
        );
        let exit_code = config.backend.wait_exit(backend_key).await;
        finish_terminal(config, terminal_id, backend_key, exit_code, false).await;
    }
}

/// Persist the `(session_key, kind)` pairing for `backend_key` to the
/// store under `terminal:{backend_key}`. Read back in `recover_sessions`
/// after a lazybox restart.
pub(crate) async fn persist_terminal_meta(
    config: &ServerConfig,
    backend_key: &str,
    session_key: &SessionKey,
    kind: &TerminalKind,
) {
    let (kv_key, payload) = match encode_terminal_meta_record(backend_key, session_key, kind) {
        Ok(record) => record,
        Err(e) => {
            tracing::warn!("persist terminal_meta: encode failed: {e}");
            return;
        }
    };
    // Store IO on `spawn_blocking` (issue #34's convention): sync
    // rusqlite under a contending process's 5s busy_timeout would
    // otherwise pin a runtime worker. Same for the sibling helpers
    // below.
    let store = config.store.clone();
    match tokio::task::spawn_blocking(move || store.set_kv(&kv_key, &payload)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("persist terminal_meta: store write failed: {e}"),
        Err(e) => tracing::warn!("persist terminal_meta: store task failed: {e}"),
    }
}

/// Encode the durable metadata row for one live terminal. Workspace moves use
/// this to place terminal rebadges in the same `Store::apply_batch`
/// transaction as their source/destination workspace updates.
pub(crate) fn encode_terminal_meta_record(
    backend_key: &str,
    session_key: &SessionKey,
    kind: &TerminalKind,
) -> Result<(String, String), serde_json::Error> {
    let payload = serde_json::to_string(&(session_key.as_str(), kind))?;
    Ok((TerminalPersistedField::Metadata.key(backend_key), payload))
}

/// Inverse of `persist_terminal_meta`. Returns None when nothing was
/// previously stored — caller falls back to a placeholder.
async fn load_terminal_meta(
    config: &ServerConfig,
    backend_key: &str,
) -> Option<(SessionKey, TerminalKind)> {
    let store = config.store.clone();
    let kv_key = TerminalPersistedField::Metadata.key(backend_key);
    let raw = tokio::task::spawn_blocking(move || store.get_kv(&kv_key))
        .await
        .ok()?
        .ok()
        .flatten()?;
    let parsed: (String, TerminalKind) = serde_json::from_str(&raw).ok()?;
    Some((SessionKey::from(parsed.0.as_str()), parsed.1))
}

pub(crate) async fn persist_agent_resume_context(
    config: &ServerConfig,
    backend_key: &str,
    context: &crate::agent_auth::AgentResumeContext,
) {
    let _guard = config.terminal.lock_terminal_persistence(backend_key).await;
    if config
        .terminal
        .backend_key_for(context.terminal_id)
        .await
        .as_deref()
        != Some(backend_key)
    {
        return;
    }
    let payload = match serde_json::to_string(context) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(%error, "persist agent resume context: encode failed");
            return;
        }
    };
    let store = config.store.clone();
    let key = TerminalPersistedField::AgentResume.key(backend_key);
    match tokio::task::spawn_blocking(move || store.set_kv(&key, &payload)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "persist agent resume context: store write failed")
        }
        Err(error) => tracing::warn!(%error, "persist agent resume context: store task failed"),
    }
}

async fn load_agent_resume_context(
    config: &ServerConfig,
    backend_key: &str,
) -> Option<crate::agent_auth::AgentResumeContext> {
    let store = config.store.clone();
    let key = TerminalPersistedField::AgentResume.key(backend_key);
    let raw = tokio::task::spawn_blocking(move || store.get_kv(&key))
        .await
        .ok()?
        .ok()
        .flatten()?;
    match serde_json::from_str(&raw) {
        Ok(context) => Some(context),
        Err(error) => {
            tracing::warn!(%error, "persisted agent resume context is invalid");
            None
        }
    }
}

fn session_agent_access_key(session_id: SessionId) -> String {
    format!("{SESSION_AGENT_ACCESS_PREFIX}{session_id}")
}

async fn persist_agent_access(
    config: &ServerConfig,
    backend_key: &str,
    session_id: Option<SessionId>,
    access: AgentRunAccess,
) -> Result<(), String> {
    let store = config.store.clone();
    let terminal_key = TerminalPersistedField::Access.key(backend_key);
    let session_key = session_id.map(session_agent_access_key);
    tokio::task::spawn_blocking(move || {
        match access {
            AgentRunAccess::Default => store.delete_kv(&terminal_key),
            AgentRunAccess::ReadOnly => store.set_kv(&terminal_key, "read-only"),
        }
        .map_err(|error| error.to_string())?;
        if let Some(session_key) = session_key {
            match access {
                AgentRunAccess::Default => store.delete_kv(&session_key),
                AgentRunAccess::ReadOnly => store.set_kv(&session_key, "read-only"),
            }
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn load_agent_access(config: &ServerConfig, key: String) -> AgentRunAccess {
    let store = config.store.clone();
    let value = tokio::task::spawn_blocking(move || store.get_kv(&key))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
    match value.as_deref() {
        Some("read-only") => AgentRunAccess::ReadOnly,
        _ => AgentRunAccess::Default,
    }
}

async fn load_terminal_access(config: &ServerConfig, backend_key: &str) -> AgentRunAccess {
    load_agent_access(config, TerminalPersistedField::Access.key(backend_key)).await
}

async fn load_session_access(config: &ServerConfig, session_id: SessionId) -> AgentRunAccess {
    load_agent_access(config, session_agent_access_key(session_id)).await
}

/// Persist whether `backend_key` was launched in no-permission mode so
/// a lazybox restart can re-render the indicator for surviving sessions
/// (`recover_sessions`). Stored under a key separate from
/// `terminal_meta` so the existing two-tuple payload format is left
/// untouched. Only written when `skip_permissions` — absence means
/// "prompts on", the common case.
async fn persist_no_permission(config: &ServerConfig, backend_key: &str, skip_permissions: bool) {
    if !skip_permissions {
        return;
    }
    let store = config.store.clone();
    let kv_key = TerminalPersistedField::NoPermission.key(backend_key);
    match tokio::task::spawn_blocking(move || store.set_kv(&kv_key, "1")).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!("persist terminal no-permission flag: store write failed: {e}")
        }
        Err(e) => tracing::warn!("persist terminal no-permission flag: store task failed: {e}"),
    }
}

/// Inverse of `persist_no_permission`. True when the surviving session
/// was launched in no-permission mode.
async fn load_no_permission(config: &ServerConfig, backend_key: &str) -> bool {
    let store = config.store.clone();
    let kv_key = TerminalPersistedField::NoPermission.key(backend_key);
    tokio::task::spawn_blocking(move || store.get_kv(&kv_key))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .is_some()
}

async fn persist_pty_launch_generation(config: &ServerConfig, backend_key: &str, generation: u32) {
    if generation == 0 {
        return;
    }
    let store = config.store.clone();
    let kv_key = TerminalPersistedField::PtyLaunchGeneration.key(backend_key);
    let value = generation.to_string();
    match tokio::task::spawn_blocking(move || store.set_kv(&kv_key, &value)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "persist terminal PTY launch generation failed")
        }
        Err(error) => tracing::warn!(%error, "persist terminal PTY generation task failed"),
    }
}

async fn load_pty_launch_generation(config: &ServerConfig, backend_key: &str) -> Option<u32> {
    let store = config.store.clone();
    let kv_key = TerminalPersistedField::PtyLaunchGeneration.key(backend_key);
    tokio::task::spawn_blocking(move || store.get_kv(&kv_key))
        .await
        .ok()?
        .ok()??
        .parse()
        .ok()
}

async fn initialize_agent_state_generation(
    config: &ServerConfig,
    backend_key: &str,
    generation: u64,
) -> Result<AgentStateDurability, String> {
    let store = config.store.clone();
    let generation_key = TerminalPersistedField::AgentStateGeneration.key(backend_key);
    let backend_key_owned = backend_key.to_string();
    let value = generation.to_string();
    tokio::task::spawn_blocking(move || {
        let previous = store
            .get_kv(&generation_key)?
            .and_then(|raw| raw.parse::<u64>().ok());
        let mut mutations = Vec::with_capacity(2);
        if let Some(previous) = previous
            && previous != generation
        {
            mutations.push(StoreMutation::DeleteKv {
                key: agent_state_key(&backend_key_owned, previous),
            });
        }
        mutations.push(StoreMutation::SetKv {
            key: generation_key,
            value,
        });
        store.apply_batch(&mutations)
    })
    .await
    .map_err(|error| format!("persistence task failed: {error}"))?
    .map_err(|error| error.to_string())?;
    Ok(AgentStateDurability {
        store: config.store.clone(),
        backend_key: backend_key.to_string(),
        generation,
        poll: config.poll.clone(),
    })
}

async fn load_recovered_agent_state(
    config: &ServerConfig,
    backend_key: &str,
    fallback_generation: u64,
) -> Result<(AgentStateDurability, lazybox_ipc::AgentState), ()> {
    let store = config.store.clone();
    let generation_key = TerminalPersistedField::AgentStateGeneration.key(backend_key);
    let backend_key_owned = backend_key.to_string();
    let loaded = match tokio::task::spawn_blocking(move || {
        let Some(raw_generation) = store.get_kv(&generation_key).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let generation = raw_generation
            .parse::<u64>()
            .map_err(|e| format!("invalid generation {raw_generation:?}: {e}"))?;
        let state_key = agent_state_key(&backend_key_owned, generation);
        let state = match store.get_kv(&state_key).map_err(|e| e.to_string())? {
            Some(raw_state) => Some(
                serde_json::from_str(&raw_state)
                    .map_err(|e| format!("invalid state {raw_state:?}: {e}"))?,
            ),
            None => None,
        };
        Ok::<_, String>(Some((generation, state)))
    })
    .await
    {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(error)) => {
            tracing::error!(
                %backend_key,
                %error,
                "agent state recovery failed; leaving terminal detached"
            );
            return Err(());
        }
        Err(error) => {
            tracing::error!(
                %backend_key,
                %error,
                "agent state recovery task failed; leaving terminal detached"
            );
            return Err(());
        }
    };

    let (generation, state) = match loaded {
        Some((generation, Some(state))) => (generation, state),
        Some((generation, None)) => {
            tracing::error!(
                %backend_key,
                "agent state invariant: recovered terminal had no committed lifecycle state; seeding conservative Working"
            );
            (generation, lazybox_ipc::AgentState::Working)
        }
        None => {
            tracing::error!(
                %backend_key,
                "agent state invariant: recovered terminal had no lifecycle generation; seeding conservative Working"
            );
            (fallback_generation, lazybox_ipc::AgentState::Working)
        }
    };
    let durability = AgentStateDurability {
        store: config.store.clone(),
        backend_key: backend_key.to_string(),
        generation,
        poll: config.poll.clone(),
    };
    if !matches!(loaded, Some((_, Some(_)))) && !durability.persist(state).await {
        return Err(());
    }
    Ok((durability, state))
}

/// Append one submitted prompt to an agent terminal's bounded per-session
/// history, keyed by backend session key so it survives a daemon restart
/// (which reassigns `TerminalId`s but keeps backend keys). Replayed to
/// clients in `snapshot_terminals` so the pinned "you ▸ …" recap (last
/// entry) and the `]]h` history are present immediately after reconnect —
/// the ring buffer only carries PTY output, never the input the recap is
/// built from (issue #523).
pub async fn handle_record_user_message(
    config: &ServerConfig,
    terminal_id: TerminalId,
    prompt: &UserPrompt,
) {
    let Some(backend_key) = config.terminal.backend_key_for(terminal_id).await else {
        tracing::trace!("record user message for unknown terminal {terminal_id:?}");
        return;
    };
    let _guard = config
        .terminal
        .lock_terminal_persistence(&backend_key)
        .await;
    if config
        .terminal
        .backend_key_for(terminal_id)
        .await
        .as_deref()
        != Some(backend_key.as_str())
    {
        tracing::debug!(
            ?terminal_id,
            %backend_key,
            "skip user-message persistence after terminal teardown"
        );
        return;
    }
    let store = config.store.clone();
    let history_key = TerminalPersistedField::UserMessageHistory.key(&backend_key);
    let legacy_key = TerminalPersistedField::UserMessage.key(&backend_key);
    let prompt = prompt.clone();
    let write = tokio::task::spawn_blocking(move || {
        let mut history = load_prompt_history_blocking(&*store, &history_key, &legacy_key);
        history.push(prompt);
        cap_prompt_history(&mut history);
        match serde_json::to_string(&history) {
            Ok(json) => store.set_kv(&history_key, &json),
            Err(e) => {
                tracing::warn!("persist terminal user message: serialize failed: {e}");
                Ok(())
            }
        }
    });
    match write.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("persist terminal user message: store write failed: {e}"),
        Err(e) => tracing::warn!("persist terminal user message: store task failed: {e}"),
    }
}

/// Cap on retained prompt-history entries per terminal. Old entries are
/// evicted oldest-first once the count or the total text budget (mirroring
/// the client's `COMPOSING_CAP` at 16× headroom for the running log) is
/// exceeded, so a long-lived session can't grow the row without bound.
const PROMPT_HISTORY_MAX_ENTRIES: usize = 200;
const PROMPT_HISTORY_MAX_BYTES: usize = 128 * 1024;

/// Evict oldest entries until the history fits both the entry-count and
/// total-byte budgets. Always keeps at least the newest entry so the recap
/// never blanks even for a single pathologically large prompt.
fn cap_prompt_history(history: &mut Vec<UserPrompt>) {
    while history.len() > PROMPT_HISTORY_MAX_ENTRIES {
        history.remove(0);
    }
    while history.len() > 1
        && history.iter().map(|p| p.text.len()).sum::<usize>() > PROMPT_HISTORY_MAX_BYTES
    {
        history.remove(0);
    }
}

/// Read the persisted history JSON, falling back to migrating the legacy
/// single-value `terminal-msg` row into a one-entry `Typed` history when
/// the new key doesn't exist yet (issue #523). Sync — runs inside the
/// caller's `spawn_blocking`.
fn load_prompt_history_blocking(
    store: &dyn lazybox_store::Store,
    history_key: &str,
    legacy_key: &str,
) -> Vec<UserPrompt> {
    if let Ok(Some(json)) = store.get_kv(history_key) {
        match serde_json::from_str::<Vec<UserPrompt>>(&json) {
            Ok(history) => return history,
            Err(e) => tracing::warn!("prompt history decode failed, resetting: {e}"),
        }
    }
    // Migrate the legacy last-prompt row as the first Typed entry. Its
    // original submit time is gone, so timestamp 0 marks it as "before
    // history tracking existed" rather than inventing a plausible one.
    match store.get_kv(legacy_key) {
        Ok(Some(text)) if !text.trim().is_empty() => vec![UserPrompt {
            text,
            timestamp_ms: 0,
            source: PromptSource::Typed,
        }],
        _ => Vec::new(),
    }
}

/// Read back the persisted prompt history (migrating the legacy row if
/// needed), oldest-first. Async since the sync-rusqlite offload (issue
/// #34's spawn_blocking convention).
async fn load_prompt_history(config: &ServerConfig, backend_key: &str) -> Vec<UserPrompt> {
    let store = config.store.clone();
    let history_key = TerminalPersistedField::UserMessageHistory.key(backend_key);
    let legacy_key = TerminalPersistedField::UserMessage.key(backend_key);
    tokio::task::spawn_blocking(move || {
        load_prompt_history_blocking(&*store, &history_key, &legacy_key)
    })
    .await
    .unwrap_or_default()
}

/// Persist the in-flight composer buffer (typed but not submitted) for
/// an agent terminal, keyed by backend session key so a half-typed
/// prompt survives a daemon restart (which reassigns `TerminalId`s but
/// keeps backend keys). An empty buffer clears the stored draft — the
/// composer emptied out, so there's nothing to recall. Replayed to
/// clients in `snapshot_terminals` as `composing_buffer` and recalled
/// into the composer with `]]r`.
pub async fn handle_record_composing_buffer(
    config: &ServerConfig,
    terminal_id: TerminalId,
    buffer: &str,
) {
    let Some(backend_key) = config.terminal.backend_key_for(terminal_id).await else {
        tracing::trace!("record composing buffer for unknown terminal {terminal_id:?}");
        return;
    };
    let _guard = config
        .terminal
        .lock_terminal_persistence(&backend_key)
        .await;
    if config
        .terminal
        .backend_key_for(terminal_id)
        .await
        .as_deref()
        != Some(backend_key.as_str())
    {
        tracing::debug!(
            ?terminal_id,
            %backend_key,
            "skip draft persistence after terminal teardown"
        );
        return;
    }
    let store = config.store.clone();
    let kv_key = TerminalPersistedField::Draft.key(&backend_key);
    let buffer = buffer.to_string();
    let write = tokio::task::spawn_blocking(move || {
        if buffer.is_empty() {
            store.delete_kv(&kv_key)
        } else {
            store.set_kv(&kv_key, &buffer)
        }
    })
    .await;
    match write {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("persist terminal composing buffer: store write failed: {e}"),
        Err(e) => tracing::warn!("persist terminal composing buffer: store task failed: {e}"),
    }
}

/// Read back the value `handle_record_composing_buffer` stored, or
/// `None` when the terminal has no pending draft.
async fn load_composing_buffer(config: &ServerConfig, backend_key: &str) -> Option<String> {
    let store = config.store.clone();
    let kv_key = TerminalPersistedField::Draft.key(backend_key);
    tokio::task::spawn_blocking(move || store.get_kv(&kv_key))
        .await
        .ok()?
        .ok()
        .flatten()
}

#[cfg(test)]
pub(crate) async fn restore_terminal_conversation_state(
    config: &ServerConfig,
    terminal_id: TerminalId,
    prompt_history: &[UserPrompt],
    composing_buffer: Option<&str>,
) {
    let Some(backend_key) = config.terminal.backend_key_for(terminal_id).await else {
        return;
    };
    restore_backend_conversation_state(config, &backend_key, prompt_history, composing_buffer)
        .await;
}

pub(crate) async fn capture_terminal_conversation_state(
    config: &ServerConfig,
    terminal_id: TerminalId,
) -> Option<(Vec<UserPrompt>, Option<String>)> {
    let Some(backend_key) = config.terminal.backend_key_for(terminal_id).await else {
        return None;
    };
    let _guard = config
        .terminal
        .lock_terminal_persistence(&backend_key)
        .await;
    if config
        .terminal
        .backend_key_for(terminal_id)
        .await
        .as_deref()
        != Some(backend_key.as_str())
    {
        return None;
    }
    Some(tokio::join!(
        load_prompt_history(config, &backend_key),
        load_composing_buffer(config, &backend_key),
    ))
}

pub(crate) async fn restore_backend_conversation_state(
    config: &ServerConfig,
    backend_key: &str,
    prompt_history: &[UserPrompt],
    composing_buffer: Option<&str>,
) {
    let store = config.store.clone();
    let history_key = TerminalPersistedField::UserMessageHistory.key(backend_key);
    let draft_key = TerminalPersistedField::Draft.key(backend_key);
    let prompt_history = prompt_history.to_vec();
    let composing_buffer = composing_buffer.map(str::to_string);
    let _ = tokio::task::spawn_blocking(move || {
        let mut mutations = Vec::with_capacity(2);
        if !prompt_history.is_empty()
            && let Ok(value) = serde_json::to_string(&prompt_history)
        {
            mutations.push(StoreMutation::SetKv {
                key: history_key,
                value,
            });
        }
        match composing_buffer {
            Some(value) if !value.is_empty() => mutations.push(StoreMutation::SetKv {
                key: draft_key,
                value,
            }),
            _ => mutations.push(StoreMutation::DeleteKv { key: draft_key }),
        }
        store.apply_batch(&mutations)
    })
    .await;
}

/// Used by `Subscribe` to seed a new client with what's already
/// running. Reads the parallel `terminal_meta` map populated by
/// `handle_spawn` so each snapshot carries the right session_key
/// and kind, not the empty-string placeholders an earlier version
/// returned.
pub async fn snapshot_terminals(config: &ServerConfig) -> Vec<TerminalSnapshot> {
    // Snapshot the two maps under their locks, then drop the locks
    // before any await on the backend — `backend.snapshot(key)` takes
    // its own lock on the backend's session map, and holding the
    // terminals/meta locks across that await would serialize every
    // backend op behind a Subscribe call.
    let entries: Vec<(
        TerminalId,
        String,
        SessionKey,
        TerminalKind,
        Option<lazybox_ipc::AgentState>,
        bool,
    )> = {
        let map = config.terminal.terminals.lock().await;
        let meta = config.terminal.terminal_meta.lock().await;
        let superseded = config.terminal.superseded_terminals.lock().await;
        let authenticating = config.terminal.authenticating_terminals.lock().await;
        let agent_states = config.terminal.agent_states.lock().await;
        map.iter()
            .filter_map(|(id, key)| {
                if superseded.contains(id) {
                    return None;
                }
                // Skip orphaned ids (terminals map says yes,
                // terminal_meta says no) — they should never exist in
                // steady state, only in a window during teardown.
                // Emitting a default-valued snapshot would feed the
                // TUI an empty-session-key workspace which the
                // sidebar would render as `(no repo)`.
                match meta.get(id).cloned() {
                    Some((sk, kind)) => Some((
                        *id,
                        key.clone(),
                        sk,
                        kind,
                        agent_states.get(id).copied(),
                        authenticating.contains(id),
                    )),
                    None => {
                        tracing::warn!(
                            terminal_id = ?id,
                            "snapshot_terminals: terminal_id has no terminal_meta entry — skipping"
                        );
                        None
                    }
                }
            })
            .collect()
    };
    let no_permission = config.terminal.no_permission_terminals.lock().await.clone();
    let on_main = config.terminal.on_main_terminals.lock().await.clone();
    let terminal_models = config.terminal.terminal_models.lock().await.clone();

    // Assemble independent terminals concurrently. `buffered` preserves the
    // stable map-entry order while capping fan-out; one wedged session now
    // consumes one slot for 500ms instead of serially multiplying the entire
    // Subscribe latency. The replay and its two persisted recap fields are
    // also independent and start together inside each slot.
    let no_permission = &no_permission;
    let on_main = &on_main;
    let terminal_models = &terminal_models;
    stream::iter(entries)
        .map(|(id, key, session_key, kind, agent_state, is_authenticating)| async move {
            let snapshot_fut = async {
                if is_authenticating {
                    None
                } else {
                    Some(
                        tokio::time::timeout(
                            SNAPSHOT_PER_SESSION_TIMEOUT,
                            config.backend.snapshot(&key),
                        )
                        .await,
                    )
                }
            };
            let history_fut = load_prompt_history(config, &key);
            let composing_fut = load_composing_buffer(config, &key);
            let (snapshot, prompt_history, composing_buffer) =
                tokio::join!(snapshot_fut, history_fut, composing_fut);
            // `replay_available` reflects whether the snapshot SUCCEEDED, not
            // whether the ring is still complete. A wrapped ring returns a
            // non-empty, line-boundary-clean `replay_snapshot`
            // (`ReplayRing::replay_snapshot_into`) — a correct if
            // shorter-history VT reset — so it is a perfectly good reattach
            // seed, the same one the resync paths serve. Only a genuine backend
            // failure/timeout leaves the client with no replay to adopt; that
            // path alone reports `replay_available: false` (and the client then
            // requests a resync via `handle_terminal_resync_request`).
            let (replay, last_seq, replay_available) = match snapshot {
                None => (Vec::new(), 0, true),
                Some(Ok(Ok(snap))) => (snap.replay, snap.last_seq, true),
                Some(Ok(Err(error))) => {
                    tracing::warn!(
                        terminal_id = ?id,
                        key = %key,
                        %error,
                        "snapshot_terminals: backend.snapshot failed — replay unavailable"
                    );
                    (Vec::new(), 0, false)
                }
                Some(Err(_)) => {
                    tracing::warn!(
                        terminal_id = ?id,
                        key = %key,
                        timeout_ms = SNAPSHOT_PER_SESSION_TIMEOUT.as_millis() as u64,
                        "snapshot_terminals: backend.snapshot timed out — replay unavailable"
                    );
                    (Vec::new(), 0, false)
                }
            };
            if let Some(replay_fingerprint) = crate::pty::debug_byte_fingerprint(&replay) {
                let composing_fingerprint = composing_buffer
                    .as_deref()
                    .map(str::as_bytes)
                    .map(crate::pty::byte_fingerprint);
                tracing::debug!(
                    terminal_id = ?id,
                    key = %key,
                    last_seq,
                    replay_available,
                    replay_len = replay_fingerprint.len,
                    replay_newlines = replay_fingerprint.newlines,
                    replay_hash = replay_fingerprint.hash,
                    draft_present = composing_fingerprint.is_some(),
                    draft_len = composing_fingerprint.map_or(0, |fingerprint| fingerprint.len),
                    draft_newlines = composing_fingerprint
                        .map_or(0, |fingerprint| fingerprint.newlines),
                    draft_hash = composing_fingerprint.map_or(0, |fingerprint| fingerprint.hash),
                    "terminal snapshot assembled at restore boundary"
                );
            }
            TerminalSnapshot {
                no_permission: no_permission.contains(&id),
                on_main: on_main.contains(&id),
                model_label: terminal_models.get(&id).cloned(),
                prompt_history,
                composing_buffer,
                terminal_id: id,
                session_key,
                kind,
                replay,
                last_seq,
                replay_available,
                agent_state,
                authenticating: is_authenticating,
            }
        })
        .buffered(SNAPSHOT_CONCURRENCY)
        .collect()
        .await
}

/// One live agent runtime, read straight from the terminal registries.
/// Unlike [`TerminalSnapshot`] this carries no PTY replay — it is the
/// lightweight projection `/v1/agents` needs to report what each agent
/// is doing, so a status poll never assembles (and discards) replay
/// rings.
#[derive(Debug, Clone)]
pub struct AgentTerminalRuntime {
    pub terminal_id: TerminalId,
    pub session_key: SessionKey,
    /// Agent id — `claude`, `codex`, `cursor`, ….
    pub agent_id: String,
    pub agent_state: Option<lazybox_ipc::AgentState>,
    /// The durable workspace session this terminal runs in, read in the
    /// same lock section as the rest of its metadata so it can't race a
    /// concurrent teardown.
    pub session_id: Option<SessionId>,
    pub on_main: bool,
    pub no_permission: bool,
    pub model_label: Option<String>,
    /// The most recent prompt submitted to this agent, if any.
    pub last_prompt: Option<UserPrompt>,
}

/// Point-in-time snapshot of every running agent, for the `/v1/agents`
/// read surface (issue #768). Reads only the in-memory terminal
/// registries — no `backend.snapshot`, so no replay is assembled — in a
/// single consistent lock section, then loads each agent's last prompt
/// from the store. Shells, log-tails, superseded terminals, and
/// still-authenticating login terminals are all excluded: only a live
/// agent doing (or ready to do) work is an agent to coordinate.
pub async fn agent_runtime_snapshot(config: &ServerConfig) -> Vec<AgentTerminalRuntime> {
    let entries: Vec<(
        TerminalId,
        String,
        SessionKey,
        String,
        Option<lazybox_ipc::AgentState>,
        Option<SessionId>,
        bool,
        bool,
        Option<String>,
    )> = {
        let map = config.terminal.terminals.lock().await;
        let meta = config.terminal.terminal_meta.lock().await;
        let superseded = config.terminal.superseded_terminals.lock().await;
        let authenticating = config.terminal.authenticating_terminals.lock().await;
        let agent_states = config.terminal.agent_states.lock().await;
        let sessions = config.terminal.terminal_sessions.lock().await;
        let no_permission = config.terminal.no_permission_terminals.lock().await;
        let on_main = config.terminal.on_main_terminals.lock().await;
        let terminal_models = config.terminal.terminal_models.lock().await;
        map.iter()
            .filter_map(|(id, backend_key)| {
                if superseded.contains(id) || authenticating.contains(id) {
                    return None;
                }
                let (session_key, kind) = meta.get(id).cloned()?;
                let TerminalKind::Agent(agent_id) = kind else {
                    return None;
                };
                Some((
                    *id,
                    backend_key.clone(),
                    session_key,
                    agent_id,
                    agent_states.get(id).copied(),
                    sessions.get(id).copied(),
                    no_permission.contains(id),
                    on_main.contains(id),
                    terminal_models.get(id).cloned(),
                ))
            })
            .collect()
    };

    let mut runtimes = Vec::with_capacity(entries.len());
    for (
        terminal_id,
        backend_key,
        session_key,
        agent_id,
        agent_state,
        session_id,
        no_permission,
        on_main,
        model_label,
    ) in entries
    {
        let last_prompt = load_prompt_history(config, &backend_key).await.pop();
        runtimes.push(AgentTerminalRuntime {
            terminal_id,
            session_key,
            agent_id,
            agent_state,
            session_id,
            on_main,
            no_permission,
            model_label,
            last_prompt,
        });
    }
    runtimes
}

/// Serve a client-observed sequence gap from the backend replay. This path
/// is defense in depth for drops below the daemon's normal pump/forwarder
/// recovery machinery.
///
/// A wrapped ring (`complete: false`) still serves the resync: its
/// `replay_snapshot` is line-boundary-clean and the `TerminalResync`
/// replaces the client grid, so the client adopts a correct, shorter-history
/// screen — the same seed `snapshot_terminals` and the forwarder's
/// `resync_replay` serve. Only a snapshot that doesn't even reach
/// `required_seq`, a backend error, or a timeout leaves the client desynced.
pub async fn handle_terminal_resync_request(
    config: &ServerConfig,
    tx: &lazybox_ipc::EventSender,
    terminal_id: TerminalId,
    required_seq: u64,
) {
    let key = config
        .terminal
        .terminals
        .lock()
        .await
        .get(&terminal_id)
        .cloned();
    let Some(key) = key else {
        let _ = tx.send(Event::TerminalResyncUnavailable { terminal_id });
        return;
    };
    let snapshot =
        tokio::time::timeout(SNAPSHOT_PER_SESSION_TIMEOUT, config.backend.snapshot(&key)).await;
    match snapshot {
        Ok(Ok(snapshot)) if snapshot.last_seq >= required_seq => {
            let _ = tx.send(Event::TerminalResync {
                terminal_id,
                replay: snapshot.replay,
                seq: snapshot.last_seq,
            });
        }
        Ok(Ok(snapshot)) => {
            tracing::warn!(
                ?terminal_id,
                required_seq,
                snapshot_seq = snapshot.last_seq,
                complete = snapshot.complete,
                "client-requested terminal resync unavailable"
            );
            let _ = tx.send(Event::TerminalResyncUnavailable { terminal_id });
        }
        Ok(Err(error)) => {
            tracing::warn!(?terminal_id, required_seq, %error, "client-requested resync failed");
            let _ = tx.send(Event::TerminalResyncUnavailable { terminal_id });
        }
        Err(_) => {
            tracing::warn!(
                ?terminal_id,
                required_seq,
                "client-requested resync timed out"
            );
            let _ = tx.send(Event::TerminalResyncUnavailable { terminal_id });
        }
    }
}

/// Walk every persisted workspace's `sessions` and spawn any whose
/// runner isn't already alive. Called once at startup after
/// `recover_sessions` (which reattaches surviving tmux sessions).
///
/// Sessions are persistent **intent**: a record means "the user
/// wants a claude here". Restoring at startup matches the user's
/// mental model — the sidebar shows `▸ claude` for a workspace
/// because there should be a claude running. Without this, a lazybox
/// restart leaves a stale-looking sidebar with the terminal stack
/// reading "(no terminals)".
///
/// Per-session, per-lazybox-lifetime: we only relaunch sessions that
/// don't currently have a live PTY. If the user explicitly killed
/// one earlier in this run, it stays dead until next restart.
pub async fn restore_persisted_sessions(config: &ServerConfig) {
    let workspaces = match config.store.list_workspaces() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("restore: list_workspaces failed: {e}");
            return;
        }
    };

    // Reclaim scrollback files for sessions that no longer exist (a
    // workspace archived via `x x`, a session removed) before restoring —
    // the durable history has no other GC hook (#468).
    gc_scrollback_files(&workspaces, &lazybox_core::paths::scrollback_dir());
    gc_session_access_policies(config, &workspaces);

    // Snapshot live (session_key, kind) pairs so we can dedupe.
    let mut live: std::collections::HashSet<(String, String)> = {
        let meta = config.terminal.terminal_meta.lock().await;
        meta.values()
            .map(|(sk, k)| (sk.as_str().to_string(), kind_id(k)))
            .collect()
    };
    // `terminal_meta` alone is not the whole truth: startup recovery is
    // run under a wall-clock bound (a wedged tmux must not freeze the
    // launch) and a timeout cancels `recover_sessions` MID-LOOP —
    // backend sessions it hadn't registered yet are alive but absent
    // from the maps. Spawning a "restore" for those would put a second
    // agent into the same worktree beside the surviving tmux session.
    // Invariant: a live backend session for key K must never coexist
    // with a fresh spawn for K — so fold the backend's own listing
    // (resolved through the same persisted meta recovery uses) into
    // the dedupe set. A listing failure means the backend is wedged
    // and live sessions are unknowable; skip the restore pass entirely
    // rather than risk double-spawning (a dead tmux server reports an
    // empty list as `Ok`, so cold-start restores still run).
    match config.backend.list().await {
        Ok(backend_keys) => {
            for backend_key in backend_keys {
                if let Some((sk, kind)) = load_terminal_meta(config, &backend_key).await {
                    live.insert((sk.as_str().to_string(), kind_id(&kind)));
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "restore: backend.list failed ({e}) — skipping session restore, \
                 cannot prove which persisted sessions are still live"
            );
            return;
        }
    }

    for record in workspaces {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };
        let session_key = SessionKey::from(workspace.key.as_str());
        for session in &workspace.sessions {
            let kind = match &session.kind {
                lazybox_core::SessionKind::Agent { agent_id } => {
                    TerminalKind::Agent(agent_id.clone())
                }
                lazybox_core::SessionKind::Shell => TerminalKind::Shell,
                // Compare / LogTail aren't auto-restored — those
                // are user-initiated transient runners.
                _ => continue,
            };
            let key_pair = (session_key.as_str().to_string(), kind_id(&kind));
            if live.contains(&key_pair) {
                continue;
            }
            let access = load_session_access(config, session.id).await;
            let provider_session_id = match &kind {
                TerminalKind::Agent(agent_id) => {
                    session.provider_session_ids.get(agent_id).cloned()
                }
                _ => None,
            };
            tracing::info!(
                "restoring session {:?} in workspace {}",
                kind,
                workspace.key
            );
            handle_spawn(
                config,
                session_key.clone(),
                Some(session.id),
                kind,
                SpawnOptions {
                    resume: true,
                    provider_session_id,
                    access,
                    origin: lazybox_ipc::SpawnOrigin::Autonomous(
                        lazybox_ipc::AutonomousTrigger::Restore,
                    ),
                    ..Default::default()
                },
            )
            .await;
        }
    }
}

fn gc_session_access_policies(
    config: &ServerConfig,
    workspaces: &[lazybox_store::WorkspaceRecord],
) {
    let mut keep = std::collections::HashSet::new();
    for record in workspaces {
        let Some(json) = &record.workspace_json else {
            return;
        };
        let Ok(workspace) = serde_json::from_str::<Workspace>(json) else {
            return;
        };
        keep.extend(
            workspace
                .sessions
                .iter()
                .map(|session| session.id.to_string()),
        );
    }
    let Ok(rows) = config.store.list_kv_prefix(SESSION_AGENT_ACCESS_PREFIX) else {
        return;
    };
    for (key, _) in rows {
        let Some(session_id) = key.strip_prefix(SESSION_AGENT_ACCESS_PREFIX) else {
            continue;
        };
        if !keep.contains(session_id)
            && let Err(error) = config.store.delete_kv(&key)
        {
            tracing::warn!(%key, %error, "session access-policy cleanup failed");
        }
    }
}

/// Delete durable scrollback files whose owning session is gone —
/// archived workspaces, removed sessions (#468). The persisted files are
/// keyed by session id (see `handle_spawn`), so the set of ids reachable
/// from every persisted workspace's `sessions` list is exactly what to
/// keep; any other `scrollback/*` file is an orphan.
///
/// Conservative: if any workspace record is unreadable we can't build a
/// complete keep-set, so we skip the sweep entirely rather than risk
/// deleting a live session's history. Best-effort IO throughout — a
/// failed unlink just leaves a bounded file to be retried next start.
fn gc_scrollback_files(workspaces: &[lazybox_store::WorkspaceRecord], dir: &std::path::Path) {
    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in workspaces {
        let Some(json) = &record.workspace_json else {
            return;
        };
        let Ok(workspace) = serde_json::from_str::<Workspace>(json) else {
            return;
        };
        for session in &workspace.sessions {
            keep.insert(session.id.to_string());
        }
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Missing dir = nothing persisted yet; anything else is logged
        // and skipped.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!("scrollback gc: read_dir {} failed: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // File names are the session id (plus a `.tmp` compaction
        // scratch file); keep only files for a live session.
        let is_orphan = match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) => !keep.contains(stem),
            None => true,
        };
        if is_orphan && let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("scrollback gc: remove {} failed: {e}", path.display());
        }
    }
}

/// Stable string identity for a `TerminalKind` — used as a hash
/// key in the live-session set during restoration. Mirrors the
/// `singleton_key()` shape but always returns Some.
fn kind_id(kind: &TerminalKind) -> String {
    match kind {
        TerminalKind::Agent(id) => format!("agent:{id}"),
        TerminalKind::Shell => "shell".into(),
        TerminalKind::LogTail { path } => format!("log:{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SessionBackend;
    use crate::spawn_plan::{
        argv_for as build_argv, gateway_env_for_agent, skip_permissions_for,
        with_agent_pty_spawn_env, with_agent_spawn_defaults, with_worktree_cargo_target,
    };

    /// Shell snippet encoding submits with a trailing CR on `Enter`, but a
    /// `Shift-Enter` insert leaves the command line unsubmitted — no CR —
    /// for both single- and multi-line bodies (issue #791).
    #[test]
    fn encode_shell_snippet_omits_the_trailing_cr_when_not_submitting() {
        // Single line.
        assert_eq!(encode_shell_snippet("ls -la", true), b"ls -la\r");
        assert_eq!(encode_shell_snippet("ls -la", false), b"ls -la");

        // Multi-line: bracketed paste either way; only the trailing CR
        // after the paste-end marker differs.
        let submitted = encode_shell_snippet("a\nb", true);
        assert!(
            submitted.ends_with(b"\x1b[201~\r"),
            "submit ends with paste-close + CR: {submitted:?}",
        );
        let inserted = encode_shell_snippet("a\nb", false);
        assert!(
            inserted.ends_with(b"\x1b[201~") && !inserted.ends_with(b"\r"),
            "no-submit ends with paste-close and no CR: {inserted:?}",
        );
    }

    #[test]
    fn route_declared_priority_distinguishes_unmapped_from_absent() {
        use lazybox_core::PriorityTier;
        let models = lazybox_core::AgentModels::builtin("claude").unwrap();
        // No priority declared → nothing to route.
        assert_eq!(route_declared_priority(None, &models), PriorityRoute::None);
        // A mapped priority yields its tier alias.
        assert_eq!(
            route_declared_priority(Some(PriorityTier::High), &models),
            PriorityRoute::Mapped("L".into())
        );
        // `best` is declared but the built-in menu maps it to nothing —
        // this must be distinct from `None` so the fallback is logged,
        // not silently indistinguishable from "no priority".
        assert_eq!(
            route_declared_priority(Some(PriorityTier::Best), &models),
            PriorityRoute::Unmapped(PriorityTier::Best)
        );
    }

    fn argv_for(
        config: &ServerConfig,
        kind: &TerminalKind,
        agent_worktree: &Path,
        resolve_shell: impl FnOnce() -> String,
        skip_permissions: bool,
        hook_settings_path: Option<PathBuf>,
        hook_command: Option<&str>,
        model_args: &[String],
        resume: bool,
    ) -> Option<Vec<String>> {
        build_argv(
            &config.agents,
            kind,
            agent_worktree,
            resolve_shell,
            skip_permissions,
            hook_settings_path,
            hook_command,
            model_args,
            resume,
            None,
            AgentRunAccess::Default,
        )
        .ok()
    }

    #[test]
    fn recovered_terminal_retry_is_exponential_spread_and_bounded() {
        let first = recovery_retry_delay(1, TerminalId(1));
        let second = recovery_retry_delay(2, TerminalId(1));
        let fourth = recovery_retry_delay(4, TerminalId(1));
        assert!(first >= Duration::from_secs(1));
        assert!(first <= Duration::from_millis(1_500));
        assert!(second > first);
        assert!(fourth >= Duration::from_secs(8));

        let capped_a = recovery_retry_delay(50, TerminalId(1));
        let capped_b = recovery_retry_delay(50, TerminalId(2));
        assert!(capped_a <= RECOVERY_RETRY_MAX);
        assert!(capped_b <= RECOVERY_RETRY_MAX);
        assert_ne!(
            capped_a, capped_b,
            "terminal-stable jitter must prevent a retry herd at the cap"
        );
    }

    #[test]
    fn recovered_terminal_retry_warnings_are_rate_limited() {
        let warned: Vec<u32> = (1..=30)
            .filter(|n| should_warn_recovery_failure(*n))
            .collect();
        assert_eq!(warned, [1, 2, 4, 8, 10, 16, 20, 30]);
        assert!(is_open_file_exhaustion(
            "PTY spawn: Too many open files (os error 24)"
        ));
        assert!(!is_open_file_exhaustion("tmux server unavailable"));
    }

    struct RejectingBatchStore {
        inner: lazybox_store::MemoryStore,
    }

    impl RejectingBatchStore {
        fn new() -> Self {
            Self {
                inner: lazybox_store::MemoryStore::new(),
            }
        }
    }

    impl lazybox_store::Store for RejectingBatchStore {
        fn apply_batch(
            &self,
            _mutations: &[lazybox_store::StoreMutation],
        ) -> Result<(), lazybox_store::StoreError> {
            Err(lazybox_store::StoreError::Backend(
                "injected batch failure".into(),
            ))
        }

        fn get_kv(&self, key: &str) -> Result<Option<String>, lazybox_store::StoreError> {
            self.inner.get_kv(key)
        }

        fn set_kv(&self, key: &str, value: &str) -> Result<(), lazybox_store::StoreError> {
            self.inner.set_kv(key, value)
        }

        fn delete_kv(&self, key: &str) -> Result<(), lazybox_store::StoreError> {
            self.inner.delete_kv(key)
        }
    }

    fn test_agent_state_durability(id: TerminalId) -> AgentStateDurability {
        AgentStateDurability {
            store: std::sync::Arc::new(lazybox_store::MemoryStore::new()),
            backend_key: format!("test-{}", id.0),
            generation: id.0,
            poll: crate::PollState::default(),
        }
    }

    #[tokio::test]
    async fn new_process_generation_discards_the_previous_agent_state() {
        let config = ServerConfig::in_memory();
        let backend_key = "reused-backend-key";
        let generation_key = TerminalPersistedField::AgentStateGeneration.key(backend_key);
        let old_state_key = agent_state_key(backend_key, 7);
        config
            .store
            .set_kv(&generation_key, "7")
            .expect("seed generation");
        config
            .store
            .set_kv(
                &old_state_key,
                &serde_json::to_string(&lazybox_ipc::AgentState::Working).expect("serialize state"),
            )
            .expect("seed state");

        initialize_agent_state_generation(&config, backend_key, 8)
            .await
            .expect("initialize next generation");

        assert_eq!(
            config
                .store
                .get_kv(&generation_key)
                .expect("load generation"),
            Some("8".into())
        );
        assert_eq!(
            config.store.get_kv(&old_state_key).expect("load old state"),
            None
        );
        assert_eq!(
            config
                .store
                .get_kv(&agent_state_key(backend_key, 8))
                .expect("load new state"),
            None
        );
    }

    #[tokio::test]
    async fn execute_spawn_plan_publishes_an_exact_durable_replacement() {
        let (config, backend) = ServerConfig::in_memory_with_mock();
        let mut events = config.bus.subscribe();
        let session_key = SessionKey::from("test:spawn-plan-execution");
        let terminal_id = TerminalId(4242);
        let cwd = std::env::current_dir().expect("current directory");
        let plan = build_spawn_plan(
            SpawnPlanInput {
                session_key: session_key.clone(),
                kind: TerminalKind::Agent("claude".into()),
                cwd: cwd.clone(),
                agent_worktree: cwd.clone(),
                owning_session: None,
                initial_prompt: None,
                terminal_id,
                hook_settings: None,
                hook_command: None,
                repo_env: vec![("PROJECT_ENV".into(), "test".into())],
                priority_model_alias: None,
                autonomous: false,
                landed_on_main: true,
                model_alias: None,
                resume: false,
                provider_session_id: None,
                no_permission_override: None,
                replace_terminal_id: Some(TerminalId(17)),
                prompt_history: Vec::new(),
                composing_buffer: None,
                access: AgentRunAccess::Default,
                shell_command: String::new(),
            },
            &lazybox_config::Config::default(),
            &config.agents,
        )
        .expect("valid plan");
        let expected_argv = plan.argv.clone();

        let executed = match execute_spawn_plan(&config, plan, None, std::time::Instant::now())
            .await
            .expect("execute plan")
        {
            SpawnExecutionOutcome::Spawned(executed) => executed,
            SpawnExecutionOutcome::Cancelled => panic!("spawn was cancelled"),
        };

        assert_eq!(backend.cwd_for(&executed.backend_key).await, Some(cwd));
        assert_eq!(
            backend.argv_for(&executed.backend_key).await,
            Some(expected_argv)
        );
        let snapshots = snapshot_terminals(&config).await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].terminal_id, terminal_id);
        assert_eq!(snapshots[0].session_key, session_key);
        assert!(
            config
                .store
                .get_kv(&TerminalPersistedField::Metadata.key(&executed.backend_key))
                .expect("load terminal metadata")
                .is_some()
        );
        assert_eq!(
            config
                .store
                .get_kv(&TerminalPersistedField::AgentStateGeneration.key(&executed.backend_key))
                .expect("load lifecycle generation"),
            Some(terminal_id.0.to_string())
        );
        assert!(
            config
                .store
                .get_kv(&TerminalPersistedField::AgentResume.key(&executed.backend_key))
                .expect("load resume metadata")
                .is_some()
        );
        assert!(matches!(
            events.recv().await.expect("spawn event"),
            Event::TerminalReplaced {
                old_terminal_id: TerminalId(17),
                terminal_id: event_terminal_id,
                session_key: event_session_key,
                on_main: true,
                authenticating: false,
                ..
            } if event_terminal_id == terminal_id && event_session_key == session_key
        ));
    }

    #[tokio::test]
    async fn recovered_agent_restores_resume_metadata_and_detects_auth_failure() {
        let backend = crate::backend::MockBackend::new();
        let backend_key = backend
            .spawn(
                &["claude".into()],
                Some(std::path::Path::new("/tmp")),
                &[],
                "surviving-agent",
            )
            .await
            .expect("spawn surviving backend");
        backend
            .emit(
                &backend_key,
                b"Not logged in. Run `claude auth login` to continue.\r\n",
            )
            .await;
        let store: std::sync::Arc<dyn lazybox_store::Store> =
            std::sync::Arc::new(lazybox_store::MemoryStore::new());
        let seed = ServerConfig::with_store_and_backend(store.clone(), backend.as_backend());
        let session_key = SessionKey::new("github:owner/repo#708");
        let kind = TerminalKind::Agent("claude".into());
        persist_terminal_meta(&seed, &backend_key, &session_key, &kind).await;
        persist_agent_access(&seed, &backend_key, None, AgentRunAccess::ReadOnly)
            .await
            .expect("persist access");
        let resume_context = crate::agent_auth::AgentResumeContext {
            terminal_id: TerminalId(1),
            session_key: session_key.clone(),
            session_id: None,
            agent_id: "claude".into(),
            cwd: "/tmp".into(),
            backend_key: Some(backend_key.clone()),
            on_main: true,
            model_alias: Some("L".into()),
            access: AgentRunAccess::ReadOnly,
            no_permission: false,
            provider_session_id: None,
            prompt_history: Vec::new(),
            composing_buffer: None,
        };
        seed.store
            .set_kv(
                &TerminalPersistedField::AgentResume.key(&backend_key),
                &serde_json::to_string(&resume_context).expect("serialize resume context"),
            )
            .expect("persist resume context");

        let restarted = ServerConfig::with_store_and_backend(store, backend.as_backend());
        let mut events = restarted.bus.subscribe();
        recover_sessions(&restarted).await;
        let terminal_id = restarted
            .terminal
            .terminal_ids()
            .await
            .into_iter()
            .next()
            .expect("recovered terminal id");
        let snapshot = snapshot_terminals(&restarted).await;
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].on_main);
        assert_eq!(snapshot[0].model_label.as_deref(), Some("Opus"));
        let context = restarted
            .agent_recovery
            .context(terminal_id)
            .await
            .expect("recovered resume context");
        assert!(context.provider_session_id.is_none());
        assert!(context.on_main);
        assert_eq!(context.model_alias.as_deref(), Some("L"));
        assert_eq!(context.access, AgentRunAccess::ReadOnly);
        handle_ingest_hook(
            &restarted,
            terminal_id,
            Some(backend_key.clone()),
            lazybox_ipc::HookEvent {
                kind: lazybox_ipc::HookEventKind::SessionStart,
                session_id: Some("claude-session-708".into()),
                cwd: None,
                tool_name: None,
                notification: None,
            },
        )
        .await;
        assert_eq!(
            load_agent_resume_context(&restarted, &backend_key)
                .await
                .and_then(|context| context.provider_session_id)
                .as_deref(),
            Some("claude-session-708"),
            "an on-main hook must durably refresh the exact resume id"
        );

        let auth_required = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let event = events.recv().await.expect("recovery event");
                if matches!(event, Event::AgentAuthRequired { .. }) {
                    return event;
                }
            }
        })
        .await
        .expect("recovered auth detection deadline");
        assert!(matches!(
            auth_required,
            Event::AgentAuthRequired {
                terminal_id: id,
                agent_id,
                ..
            } if id == terminal_id && agent_id == "claude"
        ));
    }

    #[tokio::test]
    async fn legacy_recovered_agent_reconstructs_resume_context_from_workspace_session() {
        let backend = crate::backend::MockBackend::new();
        let backend_key = backend
            .spawn(
                &["codex".into()],
                Some(std::path::Path::new("/tmp/legacy-agent")),
                &[],
                "legacy-agent",
            )
            .await
            .expect("spawn surviving backend");
        let store: std::sync::Arc<dyn lazybox_store::Store> =
            std::sync::Arc::new(lazybox_store::MemoryStore::new());
        let seed = ServerConfig::with_store_and_backend(store.clone(), backend.as_backend());
        let mut workspace =
            Workspace::from_task(task_for("github", "owner/repo#709"), chrono::Utc::now());
        let mut session = Session::new(
            workspace.key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            "/tmp/legacy-agent".into(),
            chrono::Utc::now(),
        );
        session
            .provider_session_ids
            .insert("codex".into(), "legacy-conversation-709".into());
        workspace.sessions.push(session.clone());
        seed.store
            .save_workspace(&WorkspaceRecord {
                key: workspace.key.as_str().into(),
                created_at: workspace.created_at,
                workspace_json: Some(
                    serde_json::to_string(&workspace).expect("serialize workspace"),
                ),
            })
            .expect("persist workspace");
        let session_key = SessionKey::new(workspace.key.as_str());
        persist_terminal_meta(
            &seed,
            &backend_key,
            &session_key,
            &TerminalKind::Agent("codex".into()),
        )
        .await;

        let restarted = ServerConfig::with_store_and_backend(store, backend.as_backend());
        recover_sessions(&restarted).await;
        let terminal_id = restarted.terminal.terminal_ids().await[0];
        let context = restarted
            .agent_recovery
            .context(terminal_id)
            .await
            .expect("legacy context reconstructed");
        assert_eq!(context.session_id, Some(session.id));
        assert_eq!(context.cwd, std::path::PathBuf::from("/tmp/legacy-agent"));
        assert_eq!(
            context.provider_session_id.as_deref(),
            Some("legacy-conversation-709")
        );
    }

    #[tokio::test]
    async fn agent_spawn_is_rolled_back_when_generation_is_not_durable() {
        let mock = crate::backend::MockBackend::new();
        let config = ServerConfig::with_store_and_backend(
            std::sync::Arc::new(RejectingBatchStore::new()),
            mock.as_backend(),
        );
        let mut events = config.bus.subscribe();

        handle_spawn(
            &config,
            SessionKey::from("test:failed-generation"),
            None,
            TerminalKind::Agent("codex".into()),
            SpawnOptions {
                cwd: Some(
                    std::env::current_dir()
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                ),
                ..Default::default()
            },
        )
        .await;

        assert!(
            snapshot_terminals(&config).await.is_empty(),
            "an agent without a durable lifecycle generation must never be published"
        );
        assert!(
            mock.list().await.expect("list mock sessions").is_empty(),
            "the provisioned backend process must be rolled back"
        );
        let provider_error = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Event::ProviderError { message, .. } =
                    events.recv().await.expect("spawn event")
                    && message.contains("lifecycle persistence")
                {
                    break message;
                }
            }
        })
        .await
        .expect("spawn failure event deadline");
        assert!(provider_error.contains("lifecycle persistence"));
    }

    #[tokio::test]
    async fn agent_registration_wakes_polling_but_shell_registration_does_not() {
        let config = ServerConfig::in_memory();
        wake_poll_for_terminal_kind(&config, &TerminalKind::Shell);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                config.poll.wait_for_wake(),
            )
            .await
            .is_err()
        );

        wake_poll_for_terminal_kind(&config, &TerminalKind::Agent("codex".into()));
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            config.poll.wait_for_wake(),
        )
        .await
        .expect("agent registration did not wake polling");
        assert!(
            !config.poll.take_warm_request(),
            "agent registration only needs the hot targeted path"
        );
    }

    #[tokio::test]
    async fn view_activity_is_released_only_by_submitted_input() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "resize-redraw")
            .await
            .expect("spawn");
        let terminal_id = TerminalId(699);
        config
            .terminal
            .register_terminal(
                terminal_id,
                backend_key,
                SessionKey::new("resize-redraw"),
                TerminalKind::Agent("codex".into()),
            )
            .await;

        handle_resize(&config, terminal_id, 100, 30).await;
        assert!(
            config
                .terminal
                .agent_terminal_activities
                .lock()
                .await
                .contains_key(&terminal_id)
        );

        assert!(handle_write(&config, terminal_id, b"x", TerminalInputIntent::Compose,).await);
        assert!(
            config
                .terminal
                .agent_terminal_activities
                .lock()
                .await
                .contains_key(&terminal_id)
        );

        assert!(handle_write(&config, terminal_id, b"\r", TerminalInputIntent::Submit,).await);
        assert!(
            !config
                .terminal
                .agent_terminal_activities
                .lock()
                .await
                .contains_key(&terminal_id)
        );

        config
            .terminal
            .record_agent_state(terminal_id, lazybox_ipc::AgentState::Working)
            .await;
        handle_resize(&config, terminal_id, 101, 31).await;
        assert!(
            config
                .terminal
                .agent_terminal_activities
                .lock()
                .await
                .contains_key(&terminal_id)
        );
    }

    #[tokio::test]
    async fn fast_output_during_submit_is_not_hidden_by_prior_view_activity() {
        use lazybox_ipc::AgentState::{Idle, Working};

        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "submit-redraw-race")
            .await
            .expect("spawn");
        let terminal_id = TerminalId(703);
        let session_key = SessionKey::new("submit-redraw-race");
        config
            .terminal
            .register_terminal(
                terminal_id,
                backend_key.clone(),
                session_key.clone(),
                TerminalKind::Agent("claude".into()),
            )
            .await;
        config.terminal.record_agent_state(terminal_id, Idle).await;
        config
            .terminal
            .record_agent_state_generation(terminal_id, terminal_id.0)
            .await;
        let durability = agent_state_durability(&config, terminal_id, &backend_key)
            .await
            .expect("state durability");

        handle_resize(&config, terminal_id, 100, 30).await;
        mock.set_write_delay(&backend_key, Duration::from_millis(200))
            .await;
        let write_config = config.clone();
        let write = tokio::spawn(async move {
            handle_write(
                &write_config,
                terminal_id,
                b"\r",
                TerminalInputIntent::Submit,
            )
            .await
        });
        while mock.write_attempts().await.is_empty() {
            tokio::task::yield_now().await;
        }

        let working_bytes = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");
        mock.emit(&backend_key, working_bytes).await;
        let agent = lazybox_agents::registry()
            .get("claude")
            .expect("claude agent");
        let mut buf = Vec::new();
        let mut machine = lazybox_agents::AgentStateMachine::new();
        machine.mark_booted();
        let mut events = config.bus.subscribe();
        note_pty_activity(
            Some(&agent),
            &mut buf,
            working_bytes,
            1,
            false,
            &config.terminal,
            &config.bus,
            Some(&durability),
            terminal_id,
            &session_key,
            &mut machine,
        )
        .await;

        assert_eq!(
            recv_state_for(&mut events, terminal_id),
            Some((session_key, Working)),
            "the response must become visible before the queued write reports completion",
        );
        assert!(!write.is_finished());
        assert!(write.await.expect("write task"));
    }

    #[tokio::test]
    async fn coalesced_writes_preserve_bare_chooser_answer_boundaries() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "chooser-batch")
            .await
            .expect("spawn");
        let terminal_id = TerminalId(700);
        let session_key = SessionKey::new("chooser");
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(terminal_id, backend_key.clone());
        config.terminal.terminal_meta.lock().await.insert(
            terminal_id,
            (session_key, TerminalKind::Agent("codex".into())),
        );
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(terminal_id, lazybox_ipc::AgentState::InputNeeded);
        config
            .terminal
            .agent_state_generations
            .lock()
            .await
            .insert(terminal_id, terminal_id.0);
        config
            .terminal
            .input_needed_shapes
            .lock()
            .await
            .insert(terminal_id, lazybox_agents::PromptShape::Chooser);

        handle_write_batch(
            &config,
            terminal_id,
            &[b"1".to_vec(), b"next".to_vec()],
            TerminalInputIntent::Compose,
        )
        .await;

        assert_eq!(mock.writes_for(&backend_key).await, vec![b"1next".to_vec()]);
        assert_eq!(
            config.terminal.agent_state_for(terminal_id).await,
            Some(lazybox_ipc::AgentState::Working),
            "the first logical write remains a one-key chooser answer"
        );
    }

    #[tokio::test]
    async fn failed_terminal_write_emits_terminal_error_not_provider_error() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let terminal_id = TerminalId(701);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(terminal_id, "missing-backend-session".into());
        let mut events = config.bus.subscribe();

        handle_write(
            &config,
            terminal_id,
            b"not delivered",
            TerminalInputIntent::Compose,
        )
        .await;

        let event = events.try_recv().expect("delivery failure event");
        assert!(matches!(
            event,
            Event::TerminalInputRejected {
                terminal_id: id,
                message,
            } if id == terminal_id && message.contains("not delivered")
        ));
    }

    #[tokio::test]
    async fn teardown_waits_for_an_inflight_terminal_write() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (config, mock) = ServerConfig::in_memory_with_mock();
            let terminal_id = TerminalId(702);
            let backend_key = mock
                .spawn(&[], None, &[], "teardown-write-race")
                .await
                .expect("spawn mock terminal");
            config
                .terminal
                .terminals
                .lock()
                .await
                .insert(terminal_id, backend_key.clone());
            mock.set_write_delay(&backend_key, Duration::from_millis(150))
                .await;

            let write_config = config.clone();
            let write = tokio::spawn(async move {
                handle_write(
                    &write_config,
                    terminal_id,
                    b"accepted-before-exit",
                    TerminalInputIntent::Compose,
                )
                .await;
            });
            loop {
                if !mock.write_attempts().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }

            let teardown_config = config.clone();
            let teardown_key = backend_key.clone();
            let teardown = tokio::spawn(async move {
                teardown_exited_terminal(&teardown_config, terminal_id, &teardown_key, Some(0))
                    .await;
            });
            tokio::time::sleep(Duration::from_millis(25)).await;

            assert!(
                !teardown.is_finished(),
                "teardown must join the interaction lock before detaching the terminal"
            );
            assert_eq!(
                config
                    .terminal
                    .backend_key_for(terminal_id)
                    .await
                    .as_deref(),
                Some(backend_key.as_str()),
                "the live mapping stays valid until the accepted write completes"
            );

            write.await.expect("write task");
            teardown.await.expect("teardown task");
            assert!(config.terminal.backend_key_for(terminal_id).await.is_none());
            assert_eq!(
                mock.writes_for(&backend_key).await,
                vec![b"accepted-before-exit".to_vec()]
            );
        })
        .await
        .expect("test deadline exceeded");
    }

    fn workspace_record_with_sessions(
        key: &str,
        ids: &[SessionId],
    ) -> lazybox_store::WorkspaceRecord {
        let mut ws = Workspace::empty(WorkspaceKey::new(key), "main", Utc::now());
        for id in ids {
            let mut session = Session::new(
                WorkspaceKey::new(key),
                SessionKind::Shell,
                std::path::PathBuf::from("/tmp/x"),
                Utc::now(),
            );
            session.id = *id;
            ws.add_session(session);
        }
        lazybox_store::WorkspaceRecord {
            key: key.to_string(),
            created_at: Utc::now(),
            workspace_json: Some(serde_json::to_string(&ws).unwrap()),
        }
    }

    /// GC keeps files whose session is still present in a workspace and
    /// deletes the orphans — including the `.tmp` compaction scratch file,
    /// which shares the session-id stem (#468).
    #[test]
    fn gc_scrollback_removes_only_orphans() {
        let dir = tempfile::TempDir::new().unwrap();
        let live = SessionId::new();
        let orphan = SessionId::new();
        let live_file = dir.path().join(live.to_string());
        let live_tmp = dir.path().join(format!("{live}.tmp"));
        let orphan_file = dir.path().join(orphan.to_string());
        for f in [&live_file, &live_tmp, &orphan_file] {
            std::fs::write(f, b"bytes").unwrap();
        }

        let record = workspace_record_with_sessions("ws-1", &[live]);
        gc_scrollback_files(&[record], dir.path());

        assert!(live_file.exists(), "a live session's history is kept");
        assert!(live_tmp.exists(), "its compaction scratch file is kept too");
        assert!(
            !orphan_file.exists(),
            "an archived session's file is reclaimed"
        );
    }

    /// A scrollback capture within the cap is sent verbatim; the production
    /// cap sits safely below the transport's fatal frame limit.
    #[test]
    fn small_scrollback_replay_is_unchanged() {
        let replay = b"line-1\r\nline-2\r\nline-3".to_vec();
        assert_eq!(
            cap_scrollback_replay(replay.clone(), MAX_SCROLLBACK_REPLAY_BYTES),
            replay
        );
        assert!(
            MAX_SCROLLBACK_REPLAY_BYTES < MAX_FRAME_BYTES as usize,
            "the reply cap must stay under the fatal frame limit"
        );
    }

    /// An oversized capture is truncated to at most the cap (so it can never
    /// blow the frame limit and drop the connection), keeps the MOST-RECENT
    /// bytes, and starts on a clean line boundary rather than mid-escape.
    #[test]
    fn oversized_scrollback_replay_keeps_the_recent_tail_at_a_line_boundary() {
        // 10 lines of 100 bytes each; cap at ~350 bytes keeps the last few.
        let mut replay = Vec::new();
        for i in 0..10u32 {
            replay.extend_from_slice(format!("line-{i:03}").as_bytes());
            replay.extend(std::iter::repeat_n(b'x', 100 - 8));
            replay.extend_from_slice(b"\r\n");
        }
        let capped = cap_scrollback_replay(replay.clone(), 350);

        assert!(capped.len() <= 350, "must fit the cap: {}", capped.len());
        assert!(
            capped.starts_with(b"line-"),
            "kept prefix must begin at a line boundary, not mid-line: {:?}",
            String::from_utf8_lossy(&capped[..capped.len().min(16)])
        );
        assert!(
            capped.ends_with(b"\r\n") && capped.windows(8).any(|w| w == b"line-009"),
            "the most-recent line must survive"
        );
        assert!(
            !capped.windows(8).any(|w| w == b"line-000"),
            "the oldest lines beyond the cap are dropped"
        );
    }

    /// Conservative: an unreadable workspace record means the keep-set is
    /// incomplete, so GC must not delete anything rather than risk wiping
    /// a live session's history.
    #[test]
    fn gc_scrollback_skips_when_a_record_is_unreadable() {
        let dir = tempfile::TempDir::new().unwrap();
        let orphan = SessionId::new();
        let orphan_file = dir.path().join(orphan.to_string());
        std::fs::write(&orphan_file, b"bytes").unwrap();

        let corrupt = lazybox_store::WorkspaceRecord {
            key: "ws-corrupt".to_string(),
            created_at: Utc::now(),
            workspace_json: Some("{not valid json".to_string()),
        };
        gc_scrollback_files(&[corrupt], dir.path());

        assert!(
            orphan_file.exists(),
            "no file is deleted when the keep-set can't be fully built"
        );
    }

    #[test]
    fn terminal_persistence_inventory_has_unique_cleanup_keys() {
        let keys: std::collections::HashSet<_> = TerminalPersistedField::ALL
            .into_iter()
            .map(|field| field.key("backend"))
            .collect();
        assert_eq!(keys.len(), TerminalPersistedField::ALL.len());
        assert_eq!(
            keys,
            [
                "terminal:backend".to_string(),
                "terminal-access:backend".to_string(),
                "terminal-noperm:backend".to_string(),
                "terminal-msg:backend".to_string(),
                "terminal-msgs:backend".to_string(),
                "terminal-draft:backend".to_string(),
                "terminal-pty-generation:backend".to_string(),
                "terminal-agent-state-generation:backend".to_string(),
                "terminal-agent-resume:backend".to_string(),
            ]
            .into(),
            "every persisted terminal field must live in the teardown inventory",
        );
    }

    #[test]
    fn encoded_terminal_metadata_matches_recovery_schema() {
        let session_key = SessionKey::from("github-acme-widget-42");
        let kind = TerminalKind::Agent("codex".into());
        let (key, payload) =
            encode_terminal_meta_record("backend-42", &session_key, &kind).unwrap();
        assert_eq!(key, "terminal:backend-42");
        let decoded: (String, TerminalKind) = serde_json::from_str(&payload).unwrap();
        assert_eq!(decoded.0, session_key.as_str());
        assert!(matches!(decoded.1, TerminalKind::Agent(id) if id == "codex"));
    }

    /// Per-repo env lookup returns the expected pairs and is
    /// case-sensitive on the repo key.
    #[test]
    fn env_for_repo_returns_repo_env() {
        let mut cfg = lazybox_config::Config::default();
        let mut env = std::collections::BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "postgres://x".to_string());
        env.insert("OPENAI_API_KEY".to_string(), "sk-test".to_string());
        cfg.repos.insert(
            "acme/widget".into(),
            lazybox_config::RepoConfig {
                env,
                mounts: vec![],
                scripts: vec![],
                branch_prefix: None,
                bringup: None,
            },
        );

        let out = env_for_repo(&cfg, "acme/widget");
        assert_eq!(out.len(), 2);
        let map: std::collections::BTreeMap<_, _> = out.into_iter().collect();
        assert_eq!(
            map.get("DATABASE_URL").map(String::as_str),
            Some("postgres://x")
        );
        assert_eq!(
            map.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-test")
        );
    }

    #[test]
    fn env_for_repo_returns_empty_when_repo_not_configured() {
        let cfg = lazybox_config::Config::default();
        assert!(env_for_repo(&cfg, "no/such-repo").is_empty());
    }

    #[test]
    fn gateway_env_routes_global_url_into_each_provider_var() {
        // One global gateway URL; the agent's provider only picks which
        // base-URL var carries it.
        let mut cfg = lazybox_config::Config::default();
        cfg.agent.llm_gateway_url = Some("http://gateway.internal".into());

        let claude = lazybox_agents::agent::builtins::Claude;
        assert_eq!(
            gateway_env_for_agent(&cfg, Some(&claude)),
            vec![(
                "ANTHROPIC_BASE_URL".to_string(),
                "http://gateway.internal".to_string()
            )]
        );
        // Codex and Cursor both speak OpenAI → same global URL, OpenAI var.
        for agent in [
            &lazybox_agents::agent::builtins::Codex as &dyn lazybox_agents::Agent,
            &lazybox_agents::agent::builtins::Cursor,
        ] {
            assert_eq!(
                gateway_env_for_agent(&cfg, Some(agent)),
                vec![(
                    "OPENAI_BASE_URL".to_string(),
                    "http://gateway.internal".to_string()
                )]
            );
        }
    }

    #[test]
    fn gateway_env_empty_when_unset_or_no_agent() {
        // No gateway configured → nothing for any agent.
        let bare = lazybox_config::Config::default();
        let claude = lazybox_agents::agent::builtins::Claude;
        assert!(gateway_env_for_agent(&bare, Some(&claude)).is_empty());

        let mut cfg = lazybox_config::Config::default();
        cfg.agent.llm_gateway_url = Some("http://gateway.internal".into());

        // Non-agent spawn (shell / log tail) passes `None`.
        assert!(gateway_env_for_agent(&cfg, None).is_empty());

        // A GenericCli agent has no inferable provider → no injection.
        let generic = lazybox_agents::agent::builtins::GenericCli {
            id: "custom".into(),
            display_name: "Custom".into(),
            spawn_cmd: vec!["custom".into()],
            resume_cmd: None,
            asking_patterns: vec![],
        };
        assert!(gateway_env_for_agent(&cfg, Some(&generic)).is_empty());

        // A whitespace-only URL is treated as unset.
        let mut blank = lazybox_config::Config::default();
        blank.agent.llm_gateway_url = Some("   ".into());
        assert!(gateway_env_for_agent(&blank, Some(&claude)).is_empty());
    }

    #[test]
    fn cargo_target_dir_is_pinned_under_the_worktree() {
        let cwd = PathBuf::from("/wt/acme-widget-feature");
        let out = with_worktree_cargo_target(Vec::new(), Some(&cwd));
        let map: std::collections::BTreeMap<_, _> = out.into_iter().collect();
        assert_eq!(
            map.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("/wt/acme-widget-feature/target")
        );
    }

    #[test]
    fn cargo_target_dir_respects_an_explicit_repo_setting() {
        let cwd = PathBuf::from("/wt/acme-widget-feature");
        let env = vec![("CARGO_TARGET_DIR".to_string(), "/shared/target".to_string())];
        let out = with_worktree_cargo_target(env, Some(&cwd));
        let map: std::collections::BTreeMap<_, _> = out.into_iter().collect();
        assert_eq!(
            map.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("/shared/target")
        );
    }

    #[test]
    fn cargo_target_dir_is_not_added_without_a_worktree() {
        let out = with_worktree_cargo_target(Vec::new(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn codex_spawn_suppresses_homebrew_auto_update() {
        let codex = lazybox_agents::agent::builtins::Codex;
        let out = with_agent_spawn_defaults(Vec::new(), Some(&codex));
        let map: std::collections::BTreeMap<_, _> = out.into_iter().collect();
        assert_eq!(
            map.get("HOMEBREW_NO_AUTO_UPDATE").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn claude_pty_spawn_requires_inline_renderer_without_homebrew_changes() {
        // Claude / Cursor don't self-update through `brew`, so suppressing
        // auto-update would only risk staling an unrelated `brew install`.
        let claude = lazybox_agents::agent::builtins::Claude;
        let defaults = with_agent_spawn_defaults(Vec::new(), Some(&claude));
        assert!(defaults.is_empty());
        let out = with_agent_pty_spawn_env(defaults, Some(&claude));
        let map: std::collections::BTreeMap<_, _> = out.into_iter().collect();
        assert!(!map.contains_key("HOMEBREW_NO_AUTO_UPDATE"));
        assert_eq!(
            map.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn claude_pty_renderer_overrides_a_colliding_repo_value() {
        let claude = lazybox_agents::agent::builtins::Claude;
        let env = vec![(
            "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".to_string(),
            "0".to_string(),
        )];
        let out = with_agent_pty_spawn_env(env, Some(&claude));
        assert_eq!(
            out,
            vec![(
                "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".to_string(),
                "1".to_string(),
            )]
        );
    }

    #[test]
    fn non_agent_spawn_leaves_homebrew_alone() {
        let out = with_agent_spawn_defaults(Vec::new(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn homebrew_suppression_respects_an_explicit_repo_setting() {
        let codex = lazybox_agents::agent::builtins::Codex;
        let env = vec![("HOMEBREW_NO_AUTO_UPDATE".to_string(), "0".to_string())];
        let out = with_agent_spawn_defaults(env, Some(&codex));
        let map: std::collections::BTreeMap<_, _> = out.into_iter().collect();
        assert_eq!(
            map.get("HOMEBREW_NO_AUTO_UPDATE").map(String::as_str),
            Some("0")
        );
    }

    /// Regression for #161: after an issue→PR collapse, the atomic move owner
    /// repoints the live terminal's `terminal_meta` entry onto the PR
    /// session. The output pump must broadcast its `AgentState` under the
    /// CURRENT (PR) key, not the issue key it captured at spawn — else a
    /// moved agent (e.g. one waiting on a prompt) emits state for the
    /// deleted issue workspace and looks lost. The state owner must prefer
    /// the map over the captured fallback.
    #[tokio::test]
    async fn state_owner_follows_a_rebadged_terminal_onto_the_pr() {
        let id = TerminalId(7);
        let issue_key: SessionKey = "github-o-r-161".into(); // captured at spawn
        let pr_key: SessionKey = "github-o-r-164".into(); // where rebadge moved it
        let terminals = TerminalRegistry::default();
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let durability = test_agent_state_durability(id);
        terminals.terminal_meta.lock().await.insert(
            id,
            (
                pr_key.clone(),
                lazybox_ipc::TerminalKind::Agent("claude".into()),
            ),
        );

        let transition = transition_and_broadcast_agent_state(
            &terminals,
            &bus,
            &durability,
            id,
            &issue_key,
            StateSource::Hook,
            |_| Some(lazybox_ipc::AgentState::Working),
        )
        .await;
        assert!(transition.committed);
        let Event::AgentState { session_key, .. } = rx.recv().await.expect("state event") else {
            panic!("expected AgentState")
        };
        assert_eq!(
            session_key, pr_key,
            "a rebadged terminal must broadcast state under the PR session, not the captured issue key",
        );
    }

    #[tokio::test]
    async fn done_transition_wakes_polling_for_red_pr_recheck() {
        let id = TerminalId(705);
        let key: SessionKey = "github-o-r-705".into();
        let terminals = TerminalRegistry::default();
        let (bus, _rx) = tokio::sync::broadcast::channel(4);
        let durability = test_agent_state_durability(id);
        let poll = durability.poll.clone();
        terminals.terminal_meta.lock().await.insert(
            id,
            (
                key.clone(),
                lazybox_ipc::TerminalKind::Agent("codex".into()),
            ),
        );
        terminals
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::Working);

        let transition = transition_and_broadcast_agent_state(
            &terminals,
            &bus,
            &durability,
            id,
            &key,
            StateSource::Hook,
            |_| Some(lazybox_ipc::AgentState::Done),
        )
        .await;

        assert!(transition.committed);
        tokio::time::timeout(std::time::Duration::from_millis(50), poll.wait_for_wake())
            .await
            .expect("Done should wake the poll loop immediately");
        assert!(
            !poll.take_warm_request(),
            "Done needs a targeted hot recheck, not a warm notification sweep"
        );
    }

    /// The captured key is the fallback only when the terminal is already
    /// gone from `terminal_meta` (mid-teardown) — a still-mapped terminal
    /// never falls back, so a stale capture can't leak through.
    #[tokio::test]
    async fn state_owner_falls_back_to_captured_when_terminal_swept() {
        let id = TerminalId(7);
        let captured: SessionKey = "github-o-r-161".into();
        let terminals = TerminalRegistry::default();
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let durability = test_agent_state_durability(id);

        transition_and_broadcast_agent_state(
            &terminals,
            &bus,
            &durability,
            id,
            &captured,
            StateSource::Exit,
            |_| Some(lazybox_ipc::AgentState::Exited { code: Some(0) }),
        )
        .await;
        let Event::AgentState { session_key, .. } = rx.recv().await.expect("state event") else {
            panic!("expected AgentState")
        };
        assert_eq!(
            session_key, captured,
            "missing meta entry falls back to the captured key"
        );
    }

    #[tokio::test]
    async fn state_owner_rejects_non_exit_signal_after_metadata_is_swept() {
        let id = TerminalId(7);
        let captured: SessionKey = "github-o-r-161".into();
        let terminals = TerminalRegistry::default();
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let durability = test_agent_state_durability(id);

        let late = transition_and_broadcast_agent_state(
            &terminals,
            &bus,
            &durability,
            id,
            &captured,
            StateSource::Hook,
            |_| Some(lazybox_ipc::AgentState::Working),
        )
        .await;

        assert!(!late.committed);
        assert!(terminals.agent_states.lock().await.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "a hook for a swept terminal must not recreate or broadcast state"
        );
    }

    #[tokio::test]
    async fn state_owner_commits_exit_and_rejects_late_resurrection() {
        use lazybox_ipc::AgentState;

        let id = TerminalId(8);
        let key: SessionKey = "github-o-r-357".into();
        let terminals = TerminalRegistry::default();
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let durability = test_agent_state_durability(id);
        terminals.terminal_meta.lock().await.insert(
            id,
            (
                key.clone(),
                lazybox_ipc::TerminalKind::Agent("codex".into()),
            ),
        );
        terminals
            .agent_states
            .lock()
            .await
            .insert(id, AgentState::Working);

        let exited = transition_and_broadcast_agent_state(
            &terminals,
            &bus,
            &durability,
            id,
            &key,
            StateSource::Exit,
            |_| Some(AgentState::Exited { code: Some(9) }),
        )
        .await;
        assert!(exited.committed);
        let late = transition_and_broadcast_agent_state(
            &terminals,
            &bus,
            &durability,
            id,
            &key,
            StateSource::Hook,
            |_| Some(AgentState::Working),
        )
        .await;
        assert!(!late.committed, "a late hook must not resurrect Exited");
        assert_eq!(
            terminals.agent_states.lock().await.get(&id),
            Some(&AgentState::Exited { code: Some(9) })
        );
        assert!(matches!(
            rx.recv().await,
            Ok(Event::AgentState {
                state: AgentState::Exited { code: Some(9) },
                ..
            })
        ));
        assert!(
            rx.try_recv().is_err(),
            "the rejected late Working state must not be broadcast"
        );
    }

    fn input_resolved_states() -> std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<TerminalId, lazybox_ipc::AgentState>>,
    > {
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
    }

    #[tokio::test]
    async fn poll_input_resolution_releases_on_non_input_needed_state() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let id = TerminalId(1);
        // A different terminal's transition is ignored; ours releases it.
        tx.send(Event::AgentState {
            session_key: "ws:other".into(),
            terminal_id: TerminalId(2),
            state: lazybox_ipc::AgentState::Idle,
        })
        .unwrap();
        tx.send(Event::AgentState {
            session_key: "ws:1".into(),
            terminal_id: id,
            state: lazybox_ipc::AgentState::Working,
        })
        .unwrap();
        assert!(matches!(
            poll_input_resolution(
                &mut rx,
                id,
                &input_resolved_states(),
                Duration::from_secs(1)
            )
            .await,
            InputPoll::Resolved
        ));
    }

    #[tokio::test]
    async fn poll_input_resolution_ticks_when_still_blocked() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let id = TerminalId(1);
        // Prompt stays up: only InputNeeded arrives, so the step must time out
        // into a Tick (re-read the freshly-reclassified cache) rather than
        // write into the live prompt.
        tx.send(Event::AgentState {
            session_key: "ws:1".into(),
            terminal_id: id,
            state: lazybox_ipc::AgentState::InputNeeded,
        })
        .unwrap();
        assert!(matches!(
            poll_input_resolution(
                &mut rx,
                id,
                &input_resolved_states(),
                Duration::from_millis(80)
            )
            .await,
            InputPoll::Tick
        ));
        drop(tx);
    }

    #[tokio::test]
    async fn poll_input_resolution_reports_terminal_exit() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let id = TerminalId(1);
        tx.send(Event::TerminalExited {
            terminal_id: id,
            exit_code: Some(0),
            last_output: None,
        })
        .unwrap();
        assert!(matches!(
            poll_input_resolution(
                &mut rx,
                id,
                &input_resolved_states(),
                Duration::from_secs(1)
            )
            .await,
            InputPoll::Exited
        ));
    }

    /// The reclassify poke's screen scrape (#869) must NOT fire while a
    /// just-answered prompt's reset is latched. `classify_quiet_screen`'s reset
    /// branch force-settles `Done` on the stale pre-answer buffer — a settle the
    /// quiet/watchdog timers own only after a full window — so letting the poke
    /// run there flickers a spurious `Done` right after the user answers a gate
    /// with an inject pending. Also pins the byte-quiet and agent-only guards.
    #[tokio::test]
    async fn force_reclassify_is_blocked_by_a_latched_answer_reset() {
        let config = ServerConfig::in_memory();
        let id = TerminalId(869);
        let quiet = tokio::time::Instant::now() - (RECLASSIFY_MIN_QUIET * 2);

        // Quiescent agent, no reset: the poke is honored.
        assert!(
            force_reclassify_allowed(true, quiet, &config.terminal, id).await,
            "a settled agent terminal with no pending answer must reclassify",
        );

        // A non-agent (shell) terminal has no detector to run.
        assert!(
            !force_reclassify_allowed(false, quiet, &config.terminal, id).await,
            "a non-agent terminal must never reclassify",
        );

        // Bytes still flowing (last output just now): scraping would read a
        // torn mid-paint frame, so the poke no-ops until the stream settles.
        assert!(
            !force_reclassify_allowed(true, tokio::time::Instant::now(), &config.terminal, id)
                .await,
            "a mid-paint terminal must not be scraped",
        );

        // The user just answered a gate: the reset is latched until the
        // answer's first output clears it. The poke must stand down.
        config.terminal.agent_detect_resets.lock().await.insert(id);
        assert!(
            !force_reclassify_allowed(true, quiet, &config.terminal, id).await,
            "a latched answer reset must suppress the forced reclassify so it \
             can't preempt the deliberate settle with a spurious Done",
        );

        // Once the reset clears (the answer's output arrived), the poke is
        // honored again.
        config.terminal.agent_detect_resets.lock().await.remove(&id);
        assert!(
            force_reclassify_allowed(true, quiet, &config.terminal, id).await,
            "clearing the reset must re-enable the forced reclassify",
        );
    }

    #[tokio::test]
    async fn duplicate_blocked_prompt_injection_is_rejected_and_never_queued() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "blocked-injection")
            .await
            .expect("spawn mock terminal");
        let id = TerminalId(711);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, backend_key.clone());
        config.terminal.terminal_meta.lock().await.insert(
            id,
            (
                SessionKey::new("blocked-injection"),
                TerminalKind::Agent("claude".into()),
            ),
        );
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::InputNeeded);
        // A chooser/permission gate: a pasted prompt would corrupt the
        // choice, so the inject defers (issue #725 only lifts the gate for
        // free-text prompts).
        config
            .terminal
            .input_needed_shapes
            .lock()
            .await
            .insert(id, lazybox_agents::PromptShape::Chooser);

        // The first call returns once its background waiter is registered;
        // it must not hold the per-terminal command lane for the keystroke
        // that answers this prompt.
        handle_inject_prompt(&config, id, "first", None, false).await;
        assert_eq!(config.spawn.pending_prompt_injections.lock().len(), 1);

        let mut events = config.bus.subscribe();
        handle_inject_prompt(&config, id, "duplicate", None, false).await;
        assert!(matches!(
            events.try_recv().expect("duplicate rejection"),
            Event::TerminalInputRejected {
                terminal_id,
                message,
            } if terminal_id == id && message.contains("already waiting")
        ));
        assert_eq!(config.spawn.pending_prompt_injections.lock().len(), 1);
        assert!(
            mock.writes_for(&backend_key).await.is_empty(),
            "neither prompt may be written into the live input gate"
        );

        // Terminal exit cancels the sole waiter and releases its reservation;
        // no 30-second deadline task is leaked after teardown.
        config
            .bus
            .send(Event::TerminalExited {
                terminal_id: id,
                exit_code: Some(0),
                last_output: None,
            })
            .expect("exit event");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !config.spawn.pending_prompt_injections.lock().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("injection reservation released after terminal exit");
    }

    /// A free-text `InputNeeded` prompt (the agent asking an open question)
    /// is itself waiting for composed text, so a pasted snippet IS the
    /// answer and must deliver immediately instead of deferring behind the
    /// readiness gate — deferring deadlocks because the prompt never clears
    /// (issue #725).
    #[tokio::test]
    async fn free_text_prompt_injection_delivers_immediately() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "free-text-injection")
            .await
            .expect("spawn mock terminal");
        let id = TerminalId(725);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, backend_key.clone());
        config.terminal.terminal_meta.lock().await.insert(
            id,
            (
                SessionKey::new("free-text-injection"),
                TerminalKind::Agent("claude".into()),
            ),
        );
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::InputNeeded);
        config
            .terminal
            .input_needed_shapes
            .lock()
            .await
            .insert(id, lazybox_agents::PromptShape::FreeText);

        handle_inject_prompt(&config, id, "the answer", None, false).await;

        // The prompt reaches the terminal instead of stalling behind the
        // gate — the opposite of the chooser case, where nothing is written.
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let joined: Vec<u8> = mock.writes_for(&backend_key).await.concat();
                if String::from_utf8_lossy(&joined).contains("the answer") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("free-text inject must deliver the prompt to the terminal");

        // The reservation is released after delivery, not held open by a
        // 30-second readiness waiter.
        tokio::time::timeout(Duration::from_secs(1), async {
            while !config.spawn.pending_prompt_injections.lock().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("injection reservation released after delivery");
    }

    /// A deferred inject must release the instant a live re-read shows the
    /// gate has cleared — driven by the loop's own reclassify poll, with no
    /// inbound transition event (issue #869). Before the fix the waiter blocked
    /// on a bus transition a quiescent agent never emits, so the snippet sat
    /// until the user typed and their keystroke drove the flip. Here NO bus
    /// event is ever sent: flipping only the cached state (exactly what the
    /// pump's forced reclassify would commit) must be enough to deliver, and a
    /// genuine gate that stays `InputNeeded` must keep deferring meanwhile.
    #[tokio::test]
    async fn deferred_injection_releases_via_live_state_poll_without_an_event() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "stale-input-needed")
            .await
            .expect("spawn mock terminal");
        let id = TerminalId(869);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, backend_key.clone());
        config.terminal.terminal_meta.lock().await.insert(
            id,
            (
                SessionKey::new("stale-input-needed"),
                TerminalKind::Agent("claude".into()),
            ),
        );
        // A chooser gate: a pasted prompt would corrupt the choice, so the
        // inject defers rather than delivering.
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::InputNeeded);
        config
            .terminal
            .input_needed_shapes
            .lock()
            .await
            .insert(id, lazybox_agents::PromptShape::Chooser);

        handle_inject_prompt(&config, id, "deferred work", None, false).await;

        // While the gate stands, the reservation is held and nothing is
        // written into the live dialog — even though several poll ticks run.
        assert_eq!(config.spawn.pending_prompt_injections.lock().len(), 1);
        tokio::time::sleep(INJECT_RECLASSIFY_POLL * 2).await;
        assert!(
            mock.writes_for(&backend_key).await.is_empty(),
            "a genuine gate must keep deferring — nothing pasted into the dialog",
        );

        // The gate clears to a resting composer. In production the pump's
        // forced reclassify refreshes this cache off a fresh screen read; here
        // we set it directly and send NO bus event, proving the release is
        // level-triggered off the poll rather than a keystroke transition.
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::Idle);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let joined: Vec<u8> = mock.writes_for(&backend_key).await.concat();
                if String::from_utf8_lossy(&joined).contains("deferred work") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(
            "a deferred inject must deliver once a live re-read clears the gate — no keystroke",
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while !config.spawn.pending_prompt_injections.lock().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("injection reservation released after delivery");
    }

    /// An `InputNeeded` reading with NO recorded prompt shape is presumed
    /// chooser-like — matching `AgentObservation::from_state`, which treats a
    /// bare/legacy `InputNeeded` as a chooser — so an inject DEFERS rather
    /// than pasting blind into a possible chooser. Only a positively
    /// free-text shape lifts the gate (issue #725). The pre-fix gate keyed on
    /// `== Chooser`, so an unknown shape wrongly delivered.
    #[tokio::test]
    async fn injection_defers_when_prompt_shape_is_unknown() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "unknown-shape-injection")
            .await
            .expect("spawn mock terminal");
        let id = TerminalId(7251);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, backend_key.clone());
        config.terminal.terminal_meta.lock().await.insert(
            id,
            (
                SessionKey::new("unknown-shape-injection"),
                TerminalKind::Agent("claude".into()),
            ),
        );
        // InputNeeded with NO `input_needed_shapes` entry.
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::InputNeeded);

        handle_inject_prompt(&config, id, "not the answer", None, false).await;

        // Deferred behind the readiness waiter: nothing is written into the
        // possible chooser, and the reservation stays held.
        assert_eq!(config.spawn.pending_prompt_injections.lock().len(), 1);
        assert!(
            mock.writes_for(&backend_key).await.is_empty(),
            "an unknown-shape prompt must not be pasted into blind",
        );

        // Terminal exit releases the waiter's reservation (no leaked task).
        config
            .bus
            .send(Event::TerminalExited {
                terminal_id: id,
                exit_code: Some(0),
                last_output: None,
            })
            .expect("exit event");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !config.spawn.pending_prompt_injections.lock().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("injection reservation released after terminal exit");
    }

    /// A hook-driven `InputNeeded` records the matching prompt shape before
    /// the state is published, the shape refreshes when the gate re-asserts
    /// with a different shape, and an unrecognized no-change notification
    /// leaves the live prompt's shape untouched. Pins the recording
    /// condition the #725 shape-before-publish reorder preserves.
    #[tokio::test]
    async fn hook_input_needed_records_and_refreshes_prompt_shape() {
        let id = TerminalId(7252);
        let key: SessionKey = "github-hook-shape".into();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "hook-shape")
            .await
            .expect("spawn backend");
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, backend_key.clone());
        config.terminal.terminal_meta.lock().await.insert(
            id,
            (
                key.clone(),
                lazybox_ipc::TerminalKind::Agent("claude".into()),
            ),
        );
        config
            .terminal
            .agent_state_generations
            .lock()
            .await
            .insert(id, id.0);
        // A turn has run, so the idle nudge is a genuine "blocked on me" gate
        // rather than premature startup chrome.
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::Done);

        let notify = |text: &str| lazybox_ipc::HookEvent {
            kind: lazybox_ipc::HookEventKind::Notification,
            session_id: None,
            cwd: None,
            tool_name: None,
            notification: Some(text.to_string()),
        };

        // Idle nudge → free-text elicitation.
        handle_ingest_hook(
            &config,
            id,
            Some(backend_key.clone()),
            notify("Claude is waiting for your input"),
        )
        .await;
        assert_eq!(
            config.terminal.agent_state_for(id).await,
            Some(lazybox_ipc::AgentState::InputNeeded),
        );
        assert_eq!(
            config
                .terminal
                .input_needed_shapes
                .lock()
                .await
                .get(&id)
                .copied(),
            Some(lazybox_agents::PromptShape::FreeText),
        );

        // A permission dialog re-asserts InputNeeded → shape refreshes to
        // chooser, so a later inject correctly defers instead of pasting.
        handle_ingest_hook(
            &config,
            id,
            Some(backend_key.clone()),
            notify("Claude needs your permission to run a tool"),
        )
        .await;
        assert_eq!(
            config
                .terminal
                .input_needed_shapes
                .lock()
                .await
                .get(&id)
                .copied(),
            Some(lazybox_agents::PromptShape::Chooser),
        );

        // An unrecognized notification while InputNeeded is a no-change: it
        // must NOT clobber the live prompt's recorded shape.
        handle_ingest_hook(
            &config,
            id,
            Some(backend_key.clone()),
            notify("some unrelated chatter"),
        )
        .await;
        assert_eq!(
            config.terminal.agent_state_for(id).await,
            Some(lazybox_ipc::AgentState::InputNeeded),
        );
        assert_eq!(
            config
                .terminal
                .input_needed_shapes
                .lock()
                .await
                .get(&id)
                .copied(),
            Some(lazybox_agents::PromptShape::Chooser),
            "an unrecognized no-change notification must not clobber the shape",
        );
    }

    /// The in-flight spawn guard claims a singleton identity exactly
    /// once, never blocks multi-instance kinds (shells), and releases
    /// on drop — including the early-return failure paths, which is the
    /// whole point of it being a drop guard.
    #[tokio::test]
    async fn inflight_guard_claims_once_and_releases_on_drop() {
        let config = ServerConfig::in_memory();
        let key: SessionKey = "test:ws-guard".into();
        let kind = TerminalKind::Agent("claude".into());

        let guard = InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, false)
            .expect("first claim wins");
        // Second claim on the same identity loses.
        assert!(InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, false).is_err());
        // A different kind on the same workspace is a separate identity.
        assert!(
            InflightSpawnGuard::try_claim(
                &config.spawn,
                &key,
                &TerminalKind::Agent("codex".into()),
                false
            )
            .is_ok()
        );
        // Shells are never singletons: every shell spawn claims its own
        // unique key (for cancellability + Kill serialization), so two
        // concurrent shell claims coexist and never collide.
        let _shell_a =
            InflightSpawnGuard::try_claim(&config.spawn, &key, &TerminalKind::Shell, false)
                .expect("shells never collide");
        let _shell_b =
            InflightSpawnGuard::try_claim(&config.spawn, &key, &TerminalKind::Shell, false)
                .expect("shells never collide");
        drop(guard);
        // Released → claimable again.
        assert!(InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, false).is_ok());
    }

    /// `CancelSpawn` pings the cancel channel of every in-flight claim
    /// on the target workspace — and only that workspace — so the
    /// owning `handle_spawn` aborts its provisioning. `notify_one`
    /// stores a permit, so a cancel that lands before the winner
    /// reaches its select point still takes effect (issue #403).
    #[tokio::test]
    async fn cancel_spawn_pings_only_the_workspaces_inflight_claims() {
        let config = ServerConfig::in_memory();
        let key: SessionKey = "test:ws-cancel".into();
        let other: SessionKey = "test:ws-untouched".into();
        let kind = TerminalKind::Agent("claude".into());

        let guard =
            InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, false).expect("claim wins");
        let shell = InflightSpawnGuard::try_claim(&config.spawn, &key, &TerminalKind::Shell, false)
            .expect("shells never collide");
        let bystander =
            InflightSpawnGuard::try_claim(&config.spawn, &other, &kind, false).expect("claim wins");

        // Cancel fired BEFORE anyone waits: the permit must persist.
        handle_cancel_spawn(&config.spawn, &key);
        tokio::time::timeout(Duration::from_secs(1), guard.cancel.notified())
            .await
            .expect("the claim's cancel channel fires");
        tokio::time::timeout(Duration::from_secs(1), shell.cancel.notified())
            .await
            .expect("a shell provision on the workspace is cancellable too");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), bystander.cancel.notified())
                .await
                .is_err(),
            "a cancel must not leak to another workspace's claim"
        );
    }

    /// A duplicate whose in-flight winner dies without producing a
    /// terminal must surface the retry notice even when it carried no
    /// prompt — an Esc-cancel followed by an immediate re-press lands
    /// exactly here, and a silently swallowed key press reads as
    /// "lazybox ignored me".
    #[tokio::test]
    async fn promptless_collapse_on_dead_winner_still_notifies() {
        let config = ServerConfig::in_memory();
        let key: SessionKey = "test:ws-collapse-notice".into();
        let kind = TerminalKind::Agent("claude".into());
        let mut rx = config.bus.subscribe();
        // No claim, no terminal: the winner is already gone.
        collapse_onto_inflight_spawn(&config, &key, &kind, false, AgentRunAccess::Default, None)
            .await;
        match rx.try_recv().expect("a retry notice is broadcast") {
            Event::ProviderError { source, kind, .. } => {
                assert_eq!(source, "spawn");
                assert_eq!(kind, "retryable");
            }
            other => panic!("expected ProviderError, got {other:?}"),
        }
    }

    /// #271: a main-checkout spawn of an agent is a DISTINCT singleton
    /// identity from the same agent on an isolated worktree in the same
    /// workspace — so `b c` doesn't race-collapse onto an in-flight
    /// isolated `c`.
    #[test]
    fn inflight_guard_separates_main_from_isolated() {
        let config = ServerConfig::in_memory();
        let key: SessionKey = "test:ws-main".into();
        let kind = TerminalKind::Agent("claude".into());

        let _isolated = InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, false)
            .expect("isolated claim wins");
        // The isolated identity is taken…
        assert!(InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, false).is_err());
        // …but the on-main identity is still free.
        let _main = InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, true)
            .expect("main claim wins");
        // And now the on-main identity is taken too.
        assert!(InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, true).is_err());
    }

    /// #271: `find_existing_singleton` matches by checkout. A live
    /// main-checkout agent is invisible to an isolated lookup and vice
    /// versa (so `b c` doesn't collapse onto isolated `c`), but the
    /// `None` "any checkout" lookup — what the auto-fix guard uses —
    /// finds it regardless, so a user's `b c` on a PR suppresses a
    /// duplicate auto-fix spawn.
    #[tokio::test]
    async fn find_existing_singleton_matches_by_checkout() {
        let config = ServerConfig::in_memory();
        let sk: SessionKey = "test:ws-fes".into();
        let kind = TerminalKind::Agent("claude".into());
        let tid = TerminalId(1);

        // Simulate a live main-checkout claude: meta + terminals entry
        // (so `snapshot_terminals` emits it) + the on_main marker.
        config
            .terminal
            .terminal_meta
            .lock()
            .await
            .insert(tid, (sk.clone(), kind.clone()));
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(tid, "backend-fes-1".to_string());
        config.terminal.on_main_terminals.lock().await.insert(tid);

        assert_eq!(
            find_existing_singleton(&config, &sk, &kind, Some(false)).await,
            None,
            "an isolated lookup must not see the main-checkout agent",
        );
        assert_eq!(
            find_existing_singleton(&config, &sk, &kind, Some(true)).await,
            Some(tid),
            "the main lookup finds it",
        );
        assert_eq!(
            find_existing_singleton(&config, &sk, &kind, None).await,
            Some(tid),
            "the auto-fix `None` lookup finds it on any checkout",
        );
    }

    /// Kill/Spawn serialization semantics: `await_inflight_spawns`
    /// WAITS while any spawn holds a claim on the workspace and returns
    /// as soon as the claim is released — so a teardown can't delete a
    /// worktree out from under a mid-flight provision (which would then
    /// re-create it). The wait is bounded (`KILL_INFLIGHT_WAIT`) so a
    /// wedged provision can't hold the user's explicit Kill hostage;
    /// past the cap the teardown proceeds and the `deleted_workspaces`
    /// tombstone aborts the late spawn instead.
    #[tokio::test]
    async fn kill_waits_for_inflight_spawn_to_release() {
        let config = ServerConfig::in_memory();
        let key: SessionKey = "test:ws-kill".into();
        let kind = TerminalKind::Agent("claude".into());
        let guard = InflightSpawnGuard::try_claim(&config.spawn, &key, &kind, false).unwrap();

        let cfg = config.clone();
        let waiter = tokio::spawn(async move {
            await_inflight_spawns(&cfg.spawn, "test:ws-kill").await;
        });
        // Still parked while the claim is held.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiter.is_finished(), "kill must wait out the spawn");

        drop(guard);
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("kill proceeds once the spawn releases")
            .unwrap();

        // No claim at all → returns immediately.
        tokio::time::timeout(
            Duration::from_millis(500),
            await_inflight_spawns(&config.spawn, "test:ws-other"),
        )
        .await
        .expect("no in-flight spawn → no wait");
    }

    #[tokio::test]
    async fn late_spawn_is_killed_before_registration_when_delete_timed_out() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let workspace: SessionKey = "test:late-spawn".into();
        let backend_key = config
            .backend
            .spawn(&["codex".into()], None, &[], "late-spawn")
            .await
            .unwrap();

        assert!(
            !cancel_spawn_for_deleted_workspace(&config, &workspace, &backend_key).await,
            "a live workspace must not cancel its spawn"
        );
        config
            .deleted_workspaces
            .lock()
            .insert(workspace.as_str().to_string());
        assert!(
            cancel_spawn_for_deleted_workspace(&config, &workspace, &backend_key).await,
            "the post-provision tombstone check must abort late registration"
        );
        assert_eq!(
            config.backend.wait_exit(&backend_key).await,
            Some(-1),
            "the unregistered backend process must be terminated, not orphaned"
        );
        assert!(config.terminal.terminals.lock().await.is_empty());
        assert!(config.terminal.terminal_meta.lock().await.is_empty());
    }

    /// The delete tombstone must not outlive the delete it guarded: a
    /// recreated same-name workspace re-allocates the same key, and a
    /// stale tombstone silently killed every spawn on the new row.
    /// With no spawn in flight the release is synchronous with the
    /// delete.
    #[tokio::test]
    async fn successful_delete_clears_the_tombstone() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let key = WorkspaceKey::new("test:del-clear");
        let ws = Workspace::empty(key.clone(), "main", Utc::now());
        config
            .store
            .save_workspace(&WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).expect("serialize")),
            })
            .expect("save workspace");

        assert!(
            crate::workspace::delete_workspace(&config, &key)
                .await
                .is_some()
        );
        assert!(
            !config.deleted_workspaces.lock().contains("test:del-clear"),
            "the tombstone is released once the delete has fully settled"
        );

        // The recreated same-key workspace's spawns are not killed by
        // a stale tombstone.
        let session_key: SessionKey = "test:del-clear".into();
        let backend_key = config
            .backend
            .spawn(&["claude".into()], None, &[], "recreated")
            .await
            .expect("spawn");
        assert!(
            !cancel_spawn_for_deleted_workspace(&config, &session_key, &backend_key).await,
            "a spawn on the recreated workspace must proceed"
        );
    }

    /// When a wedged provision outlived the delete's bounded
    /// `await_inflight_spawns` wait, the tombstone must stay up until
    /// that claim drains — it is the only thing stopping the late
    /// spawn from registering a terminal for the dead workspace — and
    /// be released right after.
    #[tokio::test(start_paused = true)]
    async fn delete_defers_tombstone_release_while_a_spawn_is_in_flight() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let key = WorkspaceKey::new("test:del-busy");
        let ws = Workspace::empty(key.clone(), "main", Utc::now());
        config
            .store
            .save_workspace(&WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).expect("serialize")),
            })
            .expect("save workspace");

        let session_key: SessionKey = "test:del-busy".into();
        let kind = TerminalKind::Agent("claude".into());
        let guard = InflightSpawnGuard::try_claim(&config.spawn, &session_key, &kind, false)
            .expect("claim the in-flight slot");

        // The delete waits out `KILL_INFLIGHT_WAIT` (auto-advanced
        // under paused time) and proceeds anyway — the wedged-spawn
        // shape.
        assert!(
            crate::workspace::delete_workspace(&config, &key)
                .await
                .is_some()
        );
        assert!(
            config.deleted_workspaces.lock().contains("test:del-busy"),
            "the tombstone must survive while the wedged spawn can still race registration"
        );

        drop(guard);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !config.deleted_workspaces.lock().contains("test:del-busy") {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the tombstone is released once the in-flight claim drains");
    }

    /// Concurrent injections must not clobber each other's submit
    /// signal: the first confirmation's cleanup may only remove the map
    /// entry if it is still ITS OWN `Notify` (`Arc::ptr_eq`). Pre-fix,
    /// it removed whatever was registered — deleting the second
    /// injection's signal, orphaning its waiter, and resending a
    /// spurious Enter into the agent.
    #[tokio::test]
    async fn overlapping_submit_confirmations_do_not_clobber_each_other() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(4245);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());

        let first = prepare_submit_confirmation(&config, id).await;
        // A second injection registers its own signal, replacing the
        // first's map entry.
        let second = prepare_submit_confirmation(&config, id).await;

        // The first exhausts its retries (no evidence) — but must NOT
        // remove the second's registration.
        confirm_prompt_submission(first, &config, &key, b"\r", Duration::from_millis(10)).await;
        assert!(
            config
                .spawn
                .prompt_submit_signals
                .lock()
                .await
                .contains_key(&id),
            "first confirmation must not remove the second's signal"
        );
        let resends = mock.writes_for(&key).await.len();
        assert_eq!(resends, SUBMIT_RESEND_LIMIT as usize);

        // The second's signal still works: a UserPromptSubmit-style
        // notify suppresses its resend, and its cleanup removes its own
        // registration.
        config
            .spawn
            .prompt_submit_signals
            .lock()
            .await
            .get(&id)
            .unwrap()
            .notify_one();
        confirm_prompt_submission(second, &config, &key, b"\r", Duration::from_millis(10)).await;
        assert_eq!(
            mock.writes_for(&key).await.len(),
            resends,
            "confirmed second submit must not resend"
        );
        assert!(
            config.spawn.prompt_submit_signals.lock().await.is_empty(),
            "second confirmation cleans up its own signal"
        );
    }

    /// Injection-safety regression: when the agent flips to
    /// `InputNeeded` after the paste (a permission chooser appeared and
    /// swallowed the submit), the confirm loop must NOT resend Enter —
    /// Enter into a chooser selects its default answer, silently
    /// auto-approving a tool the user never saw. It aborts and fails
    /// loudly instead.
    #[tokio::test]
    async fn chooser_mid_confirm_suppresses_enter_resends() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(4246);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::InputNeeded);
        let mut bus = config.bus.subscribe();

        let confirm = prepare_submit_confirmation(&config, id).await;
        confirm_prompt_submission(confirm, &config, &key, b"\r", Duration::from_millis(10)).await;

        assert!(
            mock.writes_for(&key).await.is_empty(),
            "no bare Enter may be written while a chooser owns input"
        );
        let mut rejected = false;
        while let Ok(ev) = bus.try_recv() {
            if matches!(
                ev,
                Event::TerminalInputRejected { terminal_id, .. } if terminal_id == id
            ) {
                rejected = true;
            }
        }
        assert!(
            rejected,
            "the suppressed submit must fail loudly, not evaporate"
        );
    }

    /// L5 regression: a bus receiver that lagged past the `Working`
    /// transition must fall back to the authoritative `agent_states`
    /// map instead of ignoring the gap — otherwise the confirm loop
    /// resends Enter into an already-working agent up to the limit and
    /// then posts a false "prompt parked" notice.
    #[tokio::test]
    async fn lagged_confirm_receiver_falls_back_to_state_map() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(4247);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        // The authoritative map saw the submit take: the agent is Working.
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, lazybox_ipc::AgentState::Working);

        let confirm = prepare_submit_confirmation(&config, id).await;
        // Overflow the confirmation's subscribed receiver so its next
        // recv reports `Lagged` — the regime in which the `Working`
        // transition itself was dropped from the bus.
        for _ in 0..crate::BUS_CAPACITY + 8 {
            let _ = config.bus.send(Event::TerminalOutput {
                terminal_id: TerminalId(9_999),
                bytes: Vec::new(),
                first_seq: 0,
                seq: 0,
            });
        }
        // Subscribed after the flood so this receiver never lags and a
        // false "prompt parked" notice can't hide behind its own gap.
        let mut bus = config.bus.subscribe();
        confirm_prompt_submission(confirm, &config, &key, b"\r", Duration::from_millis(10)).await;

        assert!(
            mock.writes_for(&key).await.is_empty(),
            "a lag-hidden Working transition must not trigger Enter resends"
        );
        let mut rejected = false;
        while let Ok(ev) = bus.try_recv() {
            if matches!(
                ev,
                Event::TerminalInputRejected { terminal_id, .. } if terminal_id == id
            ) {
                rejected = true;
            }
        }
        assert!(
            !rejected,
            "the lag fallback confirmed the submit; no false parked notice"
        );
    }

    /// Terminal ids seed from the persisted high-water mark so a
    /// restarted daemon can never reuse an id that a surviving
    /// session's artifacts (hook settings file path) still reference.
    #[test]
    fn terminal_ids_seed_from_persisted_high_water_mark() {
        use lazybox_store::Store;
        let store = lazybox_store::MemoryStore::new();
        store.set_kv(TERMINAL_ID_HIGH_WATER_KEY, "50000").unwrap();

        let id = alloc_terminal_id(&store);
        assert!(id.0 > 50_000, "id must start past the persisted mark");
        // Allocation bumps the persisted mark to the allocated id.
        let persisted: u64 = store
            .get_kv(TERMINAL_ID_HIGH_WATER_KEY)
            .unwrap()
            .expect("mark written on allocation")
            .parse()
            .unwrap();
        assert_eq!(persisted, id.0);

        // A store with no mark (fresh DB) can't move the allocator
        // backwards — ids stay strictly monotonic in-process.
        let fresh = lazybox_store::MemoryStore::new();
        let next = alloc_terminal_id(&fresh);
        assert!(next.0 > id.0);
    }

    /// Regression: concurrent allocations must never regress the
    /// persisted high-water mark. Before the persist lock, A could
    /// allocate 5, B allocate 6 and persist 6, then A persist 5 — a
    /// restart would re-issue 6 to a fresh terminal while a survivor's
    /// artifacts still referenced it. With the fix, the stored mark
    /// after any concurrent burst is exactly the max allocated id.
    #[test]
    fn concurrent_allocations_never_regress_persisted_high_water() {
        use lazybox_store::Store;
        let store = std::sync::Arc::new(lazybox_store::MemoryStore::new());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                (0..25)
                    .map(|_| alloc_terminal_id(&*store).0)
                    .collect::<Vec<u64>>()
            }));
        }
        let mut ids: Vec<u64> = Vec::new();
        for h in handles {
            ids.extend(h.join().expect("allocator thread panicked"));
        }

        // Uniqueness: 200 allocations, 200 distinct ids.
        let mut deduped = ids.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), ids.len(), "duplicate terminal id issued");

        let max_id = *ids.iter().max().expect("some ids allocated");
        let persisted: u64 = store
            .get_kv(TERMINAL_ID_HIGH_WATER_KEY)
            .unwrap()
            .expect("mark persisted")
            .parse()
            .unwrap();
        assert_eq!(
            persisted, max_id,
            "persisted high-water mark must equal the max allocated id, \
             not whichever allocator happened to write last"
        );
    }

    fn typed(text: &str) -> UserPrompt {
        UserPrompt {
            text: text.to_string(),
            timestamp_ms: 1,
            source: PromptSource::Typed,
        }
    }

    /// Issue #523: every submitted prompt recorded via
    /// `handle_record_user_message` is appended to a per-terminal history
    /// persisted against the backend key, and the whole list round-trips
    /// back through `snapshot_terminals` (oldest-first) so a reconnecting
    /// client can restore both the pinned recap and the `]]h` view.
    #[tokio::test]
    async fn recorded_user_messages_accumulate_and_round_trip() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(7);
        let session_key: SessionKey = "acme/widget#1".into();
        let kind = TerminalKind::Agent("claude".into());
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        config
            .terminal
            .terminal_meta
            .lock()
            .await
            .insert(id, (session_key.clone(), kind.clone()));

        // No prompt recorded yet → the snapshot carries an empty history.
        let before = snapshot_terminals(&config).await;
        assert!(
            before
                .iter()
                .find(|s| s.terminal_id == id)
                .unwrap()
                .prompt_history
                .is_empty()
        );

        handle_record_user_message(&config, id, &typed("rebase onto main")).await;
        handle_record_user_message(
            &config,
            id,
            &UserPrompt {
                text: "run the tests".into(),
                timestamp_ms: 2,
                source: PromptSource::Snippet {
                    key: "test".into(),
                    category: "CI".into(),
                },
            },
        )
        .await;

        let after = snapshot_terminals(&config).await;
        let history = &after
            .iter()
            .find(|s| s.terminal_id == id)
            .unwrap()
            .prompt_history;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text, "rebase onto main");
        assert_eq!(history[0].source, PromptSource::Typed);
        assert_eq!(history[1].text, "run the tests");
        assert_eq!(
            history[1].source,
            PromptSource::Snippet {
                key: "test".into(),
                category: "CI".into(),
            }
        );
    }

    /// The legacy single-value `terminal-msg` row migrates into the new
    /// history as one `Typed` entry the first time the history is read,
    /// so a prompt recorded before #523 isn't lost on upgrade.
    #[tokio::test]
    async fn legacy_last_message_migrates_into_history() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(9);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        config.terminal.terminal_meta.lock().await.insert(
            id,
            ("acme/widget#2".into(), TerminalKind::Agent("claude".into())),
        );
        config
            .store
            .set_kv(&TerminalPersistedField::UserMessage.key(&key), "old prompt")
            .unwrap();

        let migrated = load_prompt_history(&config, &key).await;
        assert_eq!(migrated.len(), 1);
        assert_eq!(migrated[0].text, "old prompt");
        assert_eq!(migrated[0].source, PromptSource::Typed);

        // A new submit appends after the migrated entry.
        handle_record_user_message(&config, id, &typed("new prompt")).await;
        let history = load_prompt_history(&config, &key).await;
        assert_eq!(
            history.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["old prompt", "new prompt"],
        );
    }

    /// Count-based eviction keeps the history bounded, dropping oldest.
    #[test]
    fn cap_prompt_history_evicts_oldest_over_count() {
        let mut history: Vec<UserPrompt> = (0..PROMPT_HISTORY_MAX_ENTRIES + 5)
            .map(|i| typed(&format!("p{i}")))
            .collect();
        cap_prompt_history(&mut history);
        assert_eq!(history.len(), PROMPT_HISTORY_MAX_ENTRIES);
        assert_eq!(history.first().unwrap().text, "p5");
        assert_eq!(
            history.last().unwrap().text,
            format!("p{}", PROMPT_HISTORY_MAX_ENTRIES + 4),
        );
    }

    /// Byte-based eviction keeps the newest entry even when it alone
    /// exceeds the budget, so the recap never blanks.
    #[test]
    fn cap_prompt_history_keeps_newest_over_byte_budget() {
        let big = "x".repeat(PROMPT_HISTORY_MAX_BYTES + 1);
        let mut history = vec![typed("small"), typed(&big)];
        cap_prompt_history(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, big);
    }

    /// Issue #373: the in-flight composer buffer persisted via
    /// `handle_record_composing_buffer` is keyed by backend session key
    /// and round-trips back through `snapshot_terminals`, so a client
    /// restarted onto a fresh daemon can recall a half-typed prompt.
    /// An empty buffer clears the stored draft.
    #[tokio::test]
    async fn recorded_composing_buffer_round_trips_and_clears() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(7);
        let session_key: SessionKey = "acme/widget#1".into();
        let kind = TerminalKind::Agent("claude".into());
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        config
            .terminal
            .terminal_meta
            .lock()
            .await
            .insert(id, (session_key.clone(), kind.clone()));

        // No draft yet → the snapshot carries None.
        let before = snapshot_terminals(&config).await;
        assert_eq!(
            before
                .iter()
                .find(|s| s.terminal_id == id)
                .unwrap()
                .composing_buffer,
            None,
        );

        handle_record_composing_buffer(&config, id, "fix the flaky ret").await;

        let after = snapshot_terminals(&config).await;
        assert_eq!(
            after
                .iter()
                .find(|s| s.terminal_id == id)
                .unwrap()
                .composing_buffer
                .as_deref(),
            Some("fix the flaky ret"),
        );

        // Emptying the buffer (the user submitted or cleared the line)
        // clears the stored draft rather than persisting "".
        handle_record_composing_buffer(&config, id, "").await;
        let cleared = snapshot_terminals(&config).await;
        assert_eq!(
            cleared
                .iter()
                .find(|s| s.terminal_id == id)
                .unwrap()
                .composing_buffer,
            None,
        );
    }

    /// A wrapped ring (`complete: false`) is a valid reattach seed: the
    /// snapshot carries its line-boundary-clean `replay_snapshot` and reports
    /// `replay_available: true`. Gating this on completeness blanked every
    /// terminal that had ever produced more than the ring capacity on
    /// reconnect / lag-recovery, then defeated the client's follow-up resync
    /// request (see `handle_terminal_resync_request`).
    #[tokio::test]
    async fn snapshot_terminals_serves_a_wrapped_ring() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"screen-state").await;
        mock.mark_snapshot_incomplete(&key).await;
        let id = TerminalId(1);
        let session_key: SessionKey = "acme/widget#1".into();
        let kind = TerminalKind::Agent("claude".into());
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        config
            .terminal
            .terminal_meta
            .lock()
            .await
            .insert(id, (session_key, kind));

        let snaps = snapshot_terminals(&config).await;
        let snap = snaps
            .iter()
            .find(|s| s.terminal_id == id)
            .expect("snapshot");
        assert!(
            snap.replay_available,
            "a wrapped-but-boundary-clean ring is a valid reattach seed"
        );
        assert_eq!(snap.replay, b"screen-state");
    }

    /// The counterpart to the wrapped-ring case: `replay_available` tracks
    /// snapshot SUCCESS, not ring completeness. A genuine backend failure has
    /// no authoritative replay, so it alone blanks the replay and flips
    /// `replay_available` to false — the signal that drives the client's
    /// recovery resync.
    #[tokio::test]
    async fn snapshot_terminals_flags_a_failed_snapshot_unavailable() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"screen-state").await;
        mock.fail_next_snapshots(&key, 1).await;
        let id = TerminalId(1);
        let session_key: SessionKey = "acme/widget#1".into();
        let kind = TerminalKind::Agent("claude".into());
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        config
            .terminal
            .terminal_meta
            .lock()
            .await
            .insert(id, (session_key, kind));

        let snaps = snapshot_terminals(&config).await;
        let snap = snaps
            .iter()
            .find(|s| s.terminal_id == id)
            .expect("snapshot");
        assert!(
            !snap.replay_available,
            "a failed snapshot carries no authoritative replay"
        );
        assert!(snap.replay.is_empty());
    }

    /// `agent_runtime_snapshot` is the `/v1/agents` read: only live
    /// agents surface — shells and still-authenticating login terminals
    /// are excluded — and each agent's durable session id is read in the
    /// same lock section it collects the rest of its metadata.
    #[tokio::test]
    async fn agent_runtime_snapshot_filters_to_live_agents() {
        let config = ServerConfig::in_memory();
        let workspace: SessionKey = "acme/widget#1".into();

        let agent = TerminalId(1);
        config
            .terminal
            .register_terminal(
                agent,
                "backend:agent".into(),
                workspace.clone(),
                TerminalKind::Agent("claude".into()),
            )
            .await;
        let session_id = SessionId::new();
        config.terminal.associate_session(agent, session_id).await;
        config
            .terminal
            .record_agent_state(agent, lazybox_ipc::AgentState::Working)
            .await;

        config
            .terminal
            .register_terminal(
                TerminalId(2),
                "backend:shell".into(),
                workspace.clone(),
                TerminalKind::Shell,
            )
            .await;

        let authenticating = TerminalId(3);
        config
            .terminal
            .register_terminal(
                authenticating,
                "backend:auth".into(),
                workspace.clone(),
                TerminalKind::Agent("codex".into()),
            )
            .await;
        config
            .terminal
            .authenticating_terminals
            .lock()
            .await
            .insert(authenticating);

        let runtimes = agent_runtime_snapshot(&config).await;
        assert_eq!(runtimes.len(), 1, "only the live, non-auth agent surfaces");
        let runtime = &runtimes[0];
        assert_eq!(runtime.terminal_id, agent);
        assert_eq!(runtime.agent_id, "claude");
        assert_eq!(runtime.agent_state, Some(lazybox_ipc::AgentState::Working));
        assert_eq!(runtime.session_id, Some(session_id));
        assert!(runtime.last_prompt.is_none());
    }

    /// A client-requested resync must be served from a wrapped ring. On
    /// reconnect a `>ring-capacity` terminal arrives `replay_available: false`
    /// and the client fires `RequestTerminalResync`; rejecting `!complete`
    /// here answered `TerminalResyncUnavailable`, so the pane stayed blank
    /// until new output. The ring's `replay_snapshot` is line-boundary-clean,
    /// a valid VT reset.
    #[tokio::test]
    async fn handle_terminal_resync_request_serves_a_wrapped_ring() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"screen-state").await; // seq 1
        mock.mark_snapshot_incomplete(&key).await;
        let id = TerminalId(1);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sender = lazybox_ipc::EventSender::from_unbounded(tx);
        handle_terminal_resync_request(&config, &sender, id, 1).await;

        match rx.try_recv() {
            Ok(Event::TerminalResync {
                terminal_id,
                replay,
                seq,
            }) => {
                assert_eq!(terminal_id, id);
                assert_eq!(replay, b"screen-state");
                assert_eq!(seq, 1);
            }
            other => panic!("wrapped ring must serve the resync, got {other:?}"),
        }
    }

    /// The genuine miss still stands: a snapshot whose `last_seq` doesn't even
    /// reach the client's `required_seq` is stale and must not be sent as an
    /// authoritative reset.
    #[tokio::test]
    async fn handle_terminal_resync_request_rejects_a_stale_snapshot() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"old").await; // seq 1
        let id = TerminalId(1);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sender = lazybox_ipc::EventSender::from_unbounded(tx);
        // The client is ahead of the snapshot (required 5 > last_seq 1).
        handle_terminal_resync_request(&config, &sender, id, 5).await;
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::TerminalResyncUnavailable { terminal_id }) if terminal_id == id
        ));
    }

    /// The pump's gap recovery must serve a wrapped ring, not drop the torn
    /// stream. Rejecting `!complete` froze the whole daemon pump for a
    /// `>ring-capacity` terminal after a single upstream gap: `is_complete()`
    /// is false forever once wrapped, and the callers never advance `last_seq`
    /// on `None`, so every subsequent chunk re-entered the gap branch and was
    /// dropped for all clients.
    #[tokio::test]
    async fn gap_resync_serves_a_wrapped_ring() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"screen-state").await; // seq 1
        mock.mark_snapshot_incomplete(&key).await;

        // Gap observed at seq 1; the wrapped ring covers it (last_seq >= gap).
        let snapshot = resync_replay_after_gap(&mock, &key, 1, 0)
            .await
            .expect("wrapped ring must serve the gap resync");
        assert_eq!(snapshot.replay, b"screen-state");
        assert_eq!(snapshot.last_seq, 1);
        assert!(!snapshot.complete, "the ring is genuinely wrapped");
    }

    /// The gap path's genuine miss is preserved: a snapshot that doesn't even
    /// reach the observed gap chunk can't cover it, so it must not be sent.
    #[tokio::test]
    async fn gap_resync_rejects_a_snapshot_that_misses_the_gap() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&[], None, &[], "t")
            .await
            .expect("spawn");
        mock.emit(&key, b"old").await; // seq 1
        // The observed gap chunk (seq 5) is newer than the snapshot's last_seq.
        assert!(resync_replay_after_gap(&mock, &key, 5, 1).await.is_none());
    }

    /// The #48 fix: a terminal that produces neither a
    /// `UserPromptSubmit` hook nor a `Working` transition after the
    /// submit gets a bounded run of Enter resends, then a user-visible
    /// give-up on the bus — never a silently parked prompt.
    #[tokio::test]
    async fn unconfirmed_submit_resends_enter_with_backoff_then_gives_up_loudly() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(4242);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());

        let confirm = prepare_submit_confirmation(&config, id).await;
        let mut bus_rx = config.bus.subscribe();
        let confirmed =
            confirm_prompt_submission(confirm, &config, &key, b"\r", Duration::from_millis(10))
                .await;

        assert!(
            !confirmed,
            "an exhausted retry loop is not a confirmed submit"
        );
        assert_eq!(
            mock.writes_for(&key).await,
            vec![b"\r".to_vec(); SUBMIT_RESEND_LIMIT as usize],
            "only Enter resends, never the prompt body, bounded by the limit"
        );
        assert!(
            config.spawn.prompt_submit_signals.lock().await.is_empty(),
            "signal registration cleaned up"
        );
        let mut gave_up_loudly = false;
        while let Ok(ev) = bus_rx.try_recv() {
            if matches!(ev, Event::TerminalInputRejected { .. }) {
                gave_up_loudly = true;
            }
        }
        assert!(
            gave_up_loudly,
            "exhausting the resends must surface a user-visible error"
        );
    }

    #[tokio::test]
    async fn rejected_submit_retry_emits_typed_terminal_failure() {
        let config = ServerConfig::in_memory_with_mock().0;
        let id = TerminalId(4248);
        let key = "missing-submit-backend".to_string();
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        let confirm = prepare_submit_confirmation(&config, id).await;
        let mut events = config.bus.subscribe();

        let confirmed =
            confirm_prompt_submission(confirm, &config, &key, b"\r", Duration::from_millis(1))
                .await;

        assert!(
            !confirmed,
            "a rejected retry must not report the prompt as delivered"
        );
        assert!(matches!(
            events.try_recv().expect("typed terminal failure"),
            Event::TerminalInputRejected {
                terminal_id,
                message,
            } if terminal_id == id && message.contains("submit retry failed")
        ));
        assert!(config.spawn.prompt_submit_signals.lock().await.is_empty());
    }

    /// A `UserPromptSubmit` hook (via the registered signal) is proof
    /// of submission — no resend.
    #[tokio::test]
    async fn user_prompt_submit_signal_suppresses_the_resend() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(4243);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());

        let confirm = prepare_submit_confirmation(&config, id).await;
        // What handle_ingest_hook does when UserPromptSubmit lands.
        // notify_one stores a permit, so firing before the wait is the
        // hard case this pins.
        config
            .spawn
            .prompt_submit_signals
            .lock()
            .await
            .get(&id)
            .unwrap()
            .notify_one();
        confirm_prompt_submission(confirm, &config, &key, b"\r", Duration::from_millis(100)).await;

        assert!(
            mock.writes_for(&key).await.is_empty(),
            "confirmed submit must not resend Enter"
        );
    }

    /// A `Working` transition on the bus is equally valid evidence —
    /// covers a hook path where `UserPromptSubmit` was missed but the
    /// turn clearly started, and is the ONLY evidence channel for
    /// terminals without hooks (PTY detection emits it too), which is
    /// why every terminal now gets a confirmation.
    #[tokio::test]
    async fn working_transition_suppresses_the_resend() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(4244);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());

        let confirm = prepare_submit_confirmation(&config, id).await;
        // The receiver was subscribed in prepare_submit_confirmation,
        // so this event is buffered for it.
        config
            .bus
            .send(Event::AgentState {
                session_key: "test:ws".into(),
                terminal_id: id,
                state: lazybox_ipc::AgentState::Working,
            })
            .unwrap();
        confirm_prompt_submission(confirm, &config, &key, b"\r", Duration::from_millis(100)).await;

        assert!(
            mock.writes_for(&key).await.is_empty(),
            "a Working transition counts as submission evidence"
        );
    }

    /// Evidence arriving mid-loop — after a resend already went out —
    /// stops the retries right there: no further Enters, no give-up
    /// error.
    #[tokio::test]
    async fn evidence_after_a_resend_stops_the_retry_loop() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(4246);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());

        let confirm = prepare_submit_confirmation(&config, id).await;
        let mut bus_rx = config.bus.subscribe();
        // Stand-in for Claude finally taking the resent Enter: once the
        // first resend hits the backend, fire UserPromptSubmit.
        let signals = config.spawn.prompt_submit_signals.clone();
        let mock_for_watch = mock.clone();
        let key_for_watch = key.clone();
        tokio::spawn(async move {
            while mock_for_watch.writes_for(&key_for_watch).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if let Some(signal) = signals.lock().await.get(&id) {
                signal.notify_one();
            }
        });
        confirm_prompt_submission(confirm, &config, &key, b"\r", Duration::from_millis(50)).await;

        assert_eq!(
            mock.writes_for(&key).await,
            vec![b"\r".to_vec()],
            "the loop stops at the resend that got confirmed"
        );
        while let Ok(ev) = bus_rx.try_recv() {
            assert!(
                !matches!(ev, Event::TerminalInputRejected { .. }),
                "a confirmed submit must not give up loudly"
            );
        }
    }

    /// #48 regression against captured real PTY bytes: the failure
    /// screen (prompt parked in the composer after the paste) must not
    /// read `Working` — otherwise it would fake submission evidence —
    /// so the confirm loop resends Enter; once the screen flips to the
    /// captured working status bar, the detector's `Working` broadcast
    /// confirms and the loop stops. The terminal is NOT hook-driven:
    /// pre-#48 these spawns had no confirmation at all.
    #[tokio::test]
    async fn captured_pty_bytes_drive_parked_prompt_recovery() {
        const PARKED: &[u8] =
            include_bytes!("../../agents/tests/fixtures/finished_with_parked_prompt.bin");
        const WORKING: &[u8] =
            include_bytes!("../../agents/tests/fixtures/claude_real_working_statusbar.bin");
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let claude = config.agents.get("claude").unwrap();
        assert_ne!(
            claude.detect_state(PARKED),
            Some(lazybox_ipc::AgentState::Working),
            "the parked-composer screen must not fake submission evidence"
        );
        let working_state = claude
            .detect_state(WORKING)
            .expect("the captured working screen must detect");
        assert_eq!(working_state, lazybox_ipc::AgentState::Working);

        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "t")
            .await
            .unwrap();
        let id = TerminalId(4247);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, key.clone());
        let confirm = prepare_submit_confirmation(&config, id).await;

        // Pump stand-in: the resent Enter takes, the screen flips from
        // the parked composer to the working status bar, and PTY
        // detection broadcasts the transition.
        let bus = config.bus.clone();
        let mock_for_watch = mock.clone();
        let key_for_watch = key.clone();
        tokio::spawn(async move {
            while mock_for_watch.writes_for(&key_for_watch).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            bus.send(Event::AgentState {
                session_key: "test:ws".into(),
                terminal_id: id,
                state: working_state,
            })
            .unwrap();
        });
        confirm_prompt_submission(confirm, &config, &key, b"\r", Duration::from_millis(50)).await;

        assert_eq!(
            mock.writes_for(&key).await,
            vec![b"\r".to_vec()],
            "one resend, then the Working transition confirms"
        );
    }

    /// The settle gate with no output at all: returns once the quiet
    /// window elapses, well before the cap.
    #[tokio::test]
    async fn paste_settle_returns_after_quiet_window() {
        let config = ServerConfig::in_memory();
        let mut events = config.bus.subscribe();
        let t0 = std::time::Instant::now();
        let settle = await_paste_settled(
            &mut events,
            TerminalId(1),
            &[],
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .await;
        let elapsed = t0.elapsed();
        assert_eq!(settle, PasteSettle::Quiet);
        assert!(elapsed >= Duration::from_millis(50));
        assert!(elapsed < Duration::from_secs(2), "must not ride the cap");
    }

    /// Output still flowing on the terminal extends the wait — the
    /// submit must not fire mid-repaint.
    #[tokio::test]
    async fn paste_settle_waits_out_streaming_output() {
        let config = ServerConfig::in_memory();
        let id = TerminalId(2);
        let mut events = config.bus.subscribe();
        let bus = config.bus.clone();
        tokio::spawn(async move {
            for seq in 0..6u64 {
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id: id,
                    bytes: b"paint".to_vec(),
                    first_seq: seq,
                    seq,
                });
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let t0 = std::time::Instant::now();
        await_paste_settled(
            &mut events,
            id,
            &[],
            Duration::from_millis(100),
            Duration::from_secs(5),
        )
        .await;
        assert!(
            t0.elapsed() >= Duration::from_millis(200),
            "settle must outlast the output burst, not return at the first quiet check"
        );
    }

    /// Other terminals' output is not evidence of an unsettled paste —
    /// it must not extend the wait.
    #[tokio::test]
    async fn paste_settle_ignores_other_terminals_output() {
        let config = ServerConfig::in_memory();
        let mut events = config.bus.subscribe();
        let bus = config.bus.clone();
        tokio::spawn(async move {
            for seq in 0..200u64 {
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id: TerminalId(99),
                    bytes: b"noise".to_vec(),
                    first_seq: seq,
                    seq,
                });
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let t0 = std::time::Instant::now();
        await_paste_settled(
            &mut events,
            TerminalId(3),
            &[],
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .await;
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "unrelated output must not hold the settle gate"
        );
    }

    /// A terminal that never goes quiet (boot spinner) hits the cap —
    /// the submit still fires; the confirm loop owns recovery from
    /// there.
    #[tokio::test]
    async fn paste_settle_is_bounded_by_the_cap() {
        let config = ServerConfig::in_memory();
        let id = TerminalId(4);
        let mut events = config.bus.subscribe();
        let bus = config.bus.clone();
        tokio::spawn(async move {
            for seq in 0..200u64 {
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id: id,
                    bytes: b"spin".to_vec(),
                    first_seq: seq,
                    seq,
                });
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let t0 = std::time::Instant::now();
        let settle = await_paste_settled(
            &mut events,
            id,
            &[],
            Duration::from_millis(100),
            Duration::from_millis(200),
        )
        .await;
        let elapsed = t0.elapsed();
        assert_eq!(settle, PasteSettle::Cap);
        assert!(elapsed >= Duration::from_millis(200));
        assert!(elapsed < Duration::from_secs(1), "the cap bounds the wait");
    }

    /// The #425 regression shape: a Codex-style TUI that repaints
    /// continuously (chunks every few ms) never satisfies the quiet
    /// window — but the frame that echoes the pasted prompt must release
    /// the gate immediately, well before the cap.
    #[tokio::test]
    async fn paste_settle_fires_on_echo_while_output_never_quiets() {
        let config = ServerConfig::in_memory();
        let id = TerminalId(5);
        let mut events = config.bus.subscribe();
        let bus = config.bus.clone();
        let encoded = lazybox_agents::PtyProtocol::GUARDED_COMPOSER.encode_prompt(
            "Address the review comments on PR #42 and push a fix",
            lazybox_agents::PromptIntent::Submit,
        );
        let probes = encoded.echo_probes().to_vec();
        tokio::spawn(async move {
            for seq in 0..400u64 {
                // Spinner churn on every frame; the composer echo repaint —
                // ANSI-wrapped, like a real frame — lands on frame 5.
                let bytes: Vec<u8> = if seq == 5 {
                    b"\x1b[2K\x1b[1;1H\x1b[7m\xe2\x80\xba\x1b[0m Address the review comments on PR #42 and push a fix".to_vec()
                } else {
                    b"\x1b[2K\xe2\x80\xa2 spin".to_vec()
                };
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id: id,
                    bytes,
                    first_seq: seq,
                    seq,
                });
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let t0 = std::time::Instant::now();
        let settle = await_paste_settled(
            &mut events,
            id,
            &probes,
            Duration::from_millis(500),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(settle, PasteSettle::Echo);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "the echo must release the gate before quiet window or cap: {:?}",
            t0.elapsed()
        );
    }

    /// A large paste an agent collapses into placeholder chrome instead of
    /// echoing verbatim (`[Pasted text #1 +12 lines]`) still counts as the
    /// paste echo.
    #[tokio::test]
    async fn paste_settle_accepts_collapsed_paste_placeholder_as_echo() {
        let config = ServerConfig::in_memory();
        let id = TerminalId(6);
        let mut events = config.bus.subscribe();
        let bus = config.bus.clone();
        let encoded = lazybox_agents::PtyProtocol::GUARDED_COMPOSER.encode_prompt(
            "line one of a long prompt\nline two\nline three",
            lazybox_agents::PromptIntent::Submit,
        );
        let probes = encoded.echo_probes().to_vec();
        tokio::spawn(async move {
            let _ = bus.send(Event::TerminalOutput {
                terminal_id: id,
                bytes: b"\x1b[2K> \x1b[2m[Pasted text #1 +2 lines]\x1b[0m".to_vec(),
                first_seq: 0,
                seq: 0,
            });
        });
        let settle = await_paste_settled(
            &mut events,
            id,
            &probes,
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(settle, PasteSettle::Echo);
    }

    /// Post-deadline pending-ready must be bounded: with the terminal alive
    /// but the ready signal never firing, the wait resolves `Capped` instead
    /// of parking the prompt forever (issue #425).
    #[tokio::test]
    async fn pending_ready_is_bounded_by_its_cap() {
        let config = ServerConfig::in_memory();
        let id = TerminalId(7);
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, "backend-key".to_string());
        let ready = tokio::sync::Notify::new();
        let t0 = std::time::Instant::now();
        let outcome = await_pending_ready(
            id,
            &ready,
            &config.terminal.terminals,
            Duration::from_millis(120),
        )
        .await;
        assert_eq!(outcome, PendingReady::Capped);
        assert!(t0.elapsed() >= Duration::from_millis(120));
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the cap must bound the pending-ready park"
        );
    }

    #[test]
    fn skip_permissions_follows_per_kind_toggles() {
        let mut cfg = lazybox_config::Config::default();
        // Defaults: autonomous on, interactive off.
        assert!(cfg.agent.autonomous_skip_permissions);
        assert!(!cfg.agent.skip_permissions);

        // Autonomous + autonomous-toggle on → bypass.
        assert!(skip_permissions_for(true, &cfg));
        // Interactive defaults off → keep the prompt.
        assert!(!skip_permissions_for(false, &cfg));

        // User opts interactive sessions into skip mode.
        cfg.agent.skip_permissions = true;
        assert!(skip_permissions_for(false, &cfg));
        // ...and that's independent of the autonomous toggle.
        assert!(skip_permissions_for(true, &cfg));

        // Paranoid user flips the autonomous toggle off; interactive
        // skip is unaffected by it.
        cfg.agent.autonomous_skip_permissions = false;
        assert!(!skip_permissions_for(true, &cfg));
        assert!(skip_permissions_for(false, &cfg));
    }

    #[test]
    fn prompt_carrying_spawn_is_autonomous() {
        // `w` / address-comments spawns carry a pre-built work prompt
        // → autonomous (unattended), so they skip the permission gate
        // that would otherwise hang the launch and eat the submit.
        assert!(spawn_is_autonomous(&Some("fix CI on PR #1".into())));
        // Bare `c` / `s` spawns carry no prompt → interactive.
        assert!(!spawn_is_autonomous(&None));
    }

    #[test]
    fn argv_for_claude_carries_skip_flag_per_toggle() {
        let config =
            ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let kind = TerminalKind::Agent("claude".into());
        let cwd = std::path::PathBuf::from("/tmp/wt");

        let with_skip = argv_for(
            &config,
            &kind,
            &cwd,
            || "bash".into(),
            true,
            None,
            None,
            &[],
            false,
        )
        .expect("claude registered");
        assert_eq!(
            with_skip,
            vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--strict-mcp-config".to_string(),
            ]
        );

        let without_skip = argv_for(
            &config,
            &kind,
            &cwd,
            || "bash".into(),
            false,
            None,
            None,
            &[],
            false,
        )
        .expect("claude registered");
        assert_eq!(without_skip, vec!["claude".to_string()]);

        // With a generated hook settings file, `--settings <path>` is
        // appended so Claude reports state through structured hooks.
        let with_hooks = argv_for(
            &config,
            &kind,
            &cwd,
            || "bash".into(),
            false,
            Some(std::path::PathBuf::from("/run/hooks/settings-1.json")),
            None,
            &[],
            false,
        )
        .expect("claude registered");
        assert_eq!(
            with_hooks,
            vec![
                "claude".to_string(),
                "--settings".to_string(),
                "/run/hooks/settings-1.json".to_string(),
            ]
        );
    }

    #[test]
    fn argv_for_codex_appends_hook_overrides_from_command() {
        let config =
            ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let kind = TerminalKind::Agent("codex".into());
        let cwd = std::path::PathBuf::from("/tmp/wt");

        // No hook command → PTY-only, argv untouched beyond the bare spawn.
        let bare = argv_for(
            &config,
            &kind,
            &cwd,
            || "bash".into(),
            false,
            None,
            None,
            &[],
            false,
        )
        .expect("codex registered");
        assert_eq!(bare, vec!["codex".to_string()]);

        // With a hook command, Codex's argv gains the trust-bypass flag and
        // one `-c hooks.<Event>=…` override per tracked lifecycle event, so
        // it reports state through the authoritative hook path.
        let cmd = "lazybox hook-ingest --backend-key-file \"/run/lzb/key-3\"";
        let argv = argv_for(
            &config,
            &kind,
            &cwd,
            || "bash".into(),
            false,
            None,
            Some(cmd),
            &[],
            false,
        )
        .expect("codex registered");
        assert_eq!(argv.first().map(String::as_str), Some("codex"));
        assert!(
            argv.contains(&"--dangerously-bypass-hook-trust".to_string()),
            "codex hook argv must bypass hook trust: {argv:?}",
        );
        let override_count = argv.iter().filter(|a| a.starts_with("hooks.")).count();
        assert_eq!(
            override_count,
            lazybox_agents::hook_settings::HOOKED_EVENTS.len(),
        );
    }

    #[test]
    fn argv_for_appends_model_tier_args() {
        let config =
            ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let kind = TerminalKind::Agent("claude".into());
        let cwd = std::path::PathBuf::from("/tmp/wt");
        // The tier's args are appended after the agent's own argv, so a
        // `--model` flag lands last and selects the picked model.
        let argv = argv_for(
            &config,
            &kind,
            &cwd,
            || "bash".into(),
            false,
            None,
            None,
            &["--model".to_string(), "claude-opus-4-8".to_string()],
            false,
        )
        .expect("claude registered");
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--model".to_string(),
                "claude-opus-4-8".to_string(),
            ]
        );
    }

    #[test]
    fn argv_for_resume_uses_agent_resume_incantation() {
        let config =
            ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let cwd = std::path::PathBuf::from("/tmp/wt");

        // Restore relaunches through the agent's declared resume path, so
        // the prior conversation reattaches instead of coming back blank.
        let claude = argv_for(
            &config,
            &TerminalKind::Agent("claude".into()),
            &cwd,
            || "bash".into(),
            false,
            None,
            None,
            &[],
            true,
        )
        .expect("claude registered");
        assert_eq!(claude, vec!["claude".to_string(), "--continue".to_string()]);

        let codex = argv_for(
            &config,
            &TerminalKind::Agent("codex".into()),
            &cwd,
            || "bash".into(),
            false,
            None,
            None,
            &[],
            true,
        )
        .expect("codex registered");
        assert_eq!(
            codex,
            vec![
                "codex".to_string(),
                "resume".to_string(),
                "--last".to_string(),
            ]
        );
    }

    #[test]
    fn argv_for_shell_uses_the_configured_command() {
        let config =
            ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));

        let argv = argv_for(
            &config,
            &TerminalKind::Shell,
            Path::new("/tmp"),
            || "fish".into(),
            false,
            None,
            None,
            &[],
            false,
        )
        .expect("shell argv");

        assert_eq!(argv, vec!["fish".to_string()]);
    }

    #[test]
    fn non_shell_argv_does_not_resolve_a_shell() {
        let config =
            ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));

        let agent = argv_for(
            &config,
            &TerminalKind::Agent("codex".into()),
            Path::new("/tmp"),
            || panic!("agent launch must not resolve a shell"),
            false,
            None,
            None,
            &[],
            false,
        )
        .expect("codex registered");
        assert_eq!(agent, vec!["codex".to_string()]);

        let log = argv_for(
            &config,
            &TerminalKind::LogTail {
                path: "/tmp/lazybox.log".into(),
            },
            Path::new("/tmp"),
            || panic!("log tail launch must not resolve a shell"),
            false,
            None,
            None,
            &[],
            false,
        )
        .expect("log tail argv");
        assert_eq!(
            log,
            vec![
                "tail".to_string(),
                "-F".to_string(),
                "/tmp/lazybox.log".to_string(),
            ]
        );
    }

    /// Persist a workspace built from `task` so `priority_alias_for`
    /// (which loads it by session key) can read its primary task.
    fn persist_task_workspace(config: &ServerConfig, task: Task) -> SessionKey {
        let ws = Workspace::from_task(task, Utc::now());
        let key = ws.key.clone();
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).expect("serialize ws")),
            })
            .expect("save workspace");
        SessionKey::new(key.as_str())
    }

    #[test]
    fn priority_alias_for_maps_label_to_builtin_tier_alias() {
        let config =
            ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let models = lazybox_core::AgentModels::builtin("claude").unwrap();

        let mut high = task_for("github", "acme/widget#7");
        high.labels = vec![lazybox_core::Label::new("high")];
        let key = persist_task_workspace(&config, high);
        // high → Claude's `L` (Opus) tier.
        assert_eq!(
            priority_alias_for(&config, &key, &models).as_deref(),
            Some("L")
        );

        // `@low` body marker → the `S` (Haiku) tier.
        let mut low = task_for("github", "acme/widget#8");
        low.body = Some("please handle this @low".into());
        let key = persist_task_workspace(&config, low);
        assert_eq!(
            priority_alias_for(&config, &key, &models).as_deref(),
            Some("S")
        );
    }

    #[test]
    fn priority_alias_for_none_without_priority_or_mapping() {
        let config =
            ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        // No priority declared → no alias, even for an agent with a map.
        let key = persist_task_workspace(&config, task_for("github", "acme/widget#7"));
        let claude = lazybox_core::AgentModels::builtin("claude").unwrap();
        assert_eq!(priority_alias_for(&config, &key, &claude), None);

        // A high-priority task, but an agent menu with no priority map →
        // no alias (agent keeps its default model).
        let mut high = task_for("github", "acme/widget#8");
        high.labels = vec![lazybox_core::Label::new("high")];
        let key = persist_task_workspace(&config, high);
        let no_map = lazybox_core::AgentModels {
            tiers: claude.tiers.clone(),
            ..Default::default()
        };
        assert_eq!(priority_alias_for(&config, &key, &no_map), None);
    }

    #[cfg(unix)]
    fn write_fake_exe(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write fake exe");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake exe");
    }

    /// The stable copy is a real, executable duplicate of the source that
    /// keeps working after the source (a per-worktree `target/debug`
    /// artifact) is removed — the whole point of not baking the source path.
    #[cfg(unix)]
    #[test]
    fn stabilize_exe_copies_and_survives_source_removal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lazybox-stabilize-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("target-debug-lazybox");
        let stable = dir.join("bin").join("lazybox");
        write_fake_exe(&src, "#!/bin/sh\necho hi\n");

        let got = stabilize_exe(&src, &stable).expect("copy succeeds");
        assert_eq!(got, stable);
        assert_eq!(
            std::fs::read_to_string(&stable).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        assert!(
            std::fs::metadata(&stable).unwrap().permissions().mode() & 0o111 != 0,
            "exec bit must survive the copy"
        );

        std::fs::remove_file(&src).unwrap();
        assert!(stable.is_file(), "stable copy outlives its source");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh copy is not re-written on every spawn: the second call is a
    /// no-op, so the steady-state cost is a stat, not a binary copy.
    #[cfg(unix)]
    #[test]
    fn stabilize_exe_skips_recopy_when_fresh() {
        let dir =
            std::env::temp_dir().join(format!("lazybox-stabilize-fresh-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src");
        let stable = dir.join("bin").join("lazybox");
        write_fake_exe(&src, "one");

        stabilize_exe(&src, &stable).expect("first copy");
        let first = std::fs::metadata(&stable).unwrap().modified().unwrap();
        stabilize_exe(&src, &stable).expect("second call");
        let second = std::fs::metadata(&stable).unwrap().modified().unwrap();
        assert_eq!(first, second, "a fresh copy must not be re-written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rebuild (source bytes change) re-copies so the stable path tracks
    /// the live binary rather than pinning the first build seen. A differing
    /// length trips the freshness check regardless of mtime granularity.
    #[cfg(unix)]
    #[test]
    fn stabilize_exe_recopies_when_source_changes() {
        let dir =
            std::env::temp_dir().join(format!("lazybox-stabilize-rebuild-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src");
        let stable = dir.join("bin").join("lazybox");
        write_fake_exe(&src, "old-build");
        stabilize_exe(&src, &stable).expect("first copy");

        write_fake_exe(&src, "a-longer-new-build-payload");
        stabilize_exe(&src, &stable).expect("re-copy");
        assert_eq!(
            std::fs::read_to_string(&stable).unwrap(),
            "a-longer-new-build-payload"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Concurrent spawns (the Detached command lane runs many at once) must
    /// never expose a half-written `stable`: before `COPY_LOCK`, racing
    /// copies shared the one fixed `lazybox.tmp` and a `rename` could
    /// publish an inode another copy was still writing. A reader thread
    /// asserts `stable` is only ever absent or the full length, and the
    /// final bytes match the source exactly.
    #[cfg(unix)]
    #[test]
    fn stabilize_exe_serializes_concurrent_copies_without_tearing() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir =
            std::env::temp_dir().join(format!("lazybox-stabilize-conc-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src");
        let stable = dir.join("bin").join("lazybox");
        // Large enough that a torn copy would be observable mid-write.
        let payload = vec![b'z'; 4 * 1024 * 1024];
        std::fs::write(&src, &payload).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        let full_len = payload.len() as u64;

        let src = Arc::new(src);
        let stable = Arc::new(stable);
        let stop = Arc::new(AtomicBool::new(false));

        let reader = {
            let stable = Arc::clone(&stable);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(meta) = std::fs::metadata(&*stable) {
                        let len = meta.len();
                        assert!(
                            len == 0 || len == full_len,
                            "observed a torn `stable` of {len} bytes"
                        );
                    }
                }
            })
        };

        let writers: Vec<_> = (0..8)
            .map(|_| {
                let src = Arc::clone(&src);
                let stable = Arc::clone(&stable);
                std::thread::spawn(move || stabilize_exe(&src, &stable).expect("stabilize"))
            })
            .collect();
        for w in writers {
            assert_eq!(w.join().unwrap(), *stable);
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();

        assert_eq!(
            std::fs::read(&*stable).unwrap(),
            payload,
            "final copy intact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Running already from the stable path is a no-op — never a self-copy
    /// (which would risk `ETXTBSY` writing over the live binary).
    #[cfg(unix)]
    #[test]
    fn stabilize_exe_noop_when_current_is_stable() {
        let dir =
            std::env::temp_dir().join(format!("lazybox-stabilize-self-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let stable = dir.join("lazybox");
        write_fake_exe(&stable, "self");
        let got = stabilize_exe(&stable, &stable).expect("no-op returns the path");
        assert_eq!(got, stable);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stable_hook_exe_rejects_a_gui_only_executable() {
        let directory = tempfile::tempdir().expect("tempdir");
        let gui = directory.path().join("lazybox-desktop");
        let stable = directory.path().join("bin/lazybox");
        write_fake_exe(&gui, "#!/bin/sh\nexit 0\n");

        assert_eq!(ensure_stable_hook_exe_from(&gui, &stable), None);
        assert!(!stable.exists());
    }

    #[cfg(unix)]
    #[test]
    fn stable_hook_exe_accepts_a_probed_cli_helper() {
        let directory = tempfile::tempdir().expect("tempdir");
        let cli = directory.path().join("lazybox");
        let stable = directory.path().join("bin/lazybox");
        write_fake_exe(
            &cli,
            &format!(
                "#!/bin/sh\n[ \"$1\" = \"{}\" ] && echo {}\n",
                HOOK_HELPER_PROBE_ARG, HOOK_HELPER_PROBE_RESPONSE
            ),
        );

        assert_eq!(
            ensure_stable_hook_exe_from(&cli, &stable),
            Some(stable.clone())
        );
        assert!(stable.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn stable_hook_exe_reports_a_copy_failure_as_none() {
        // A hook-capable source but an uninstallable target (its parent is a
        // regular file, so the bin dir can't be created) must surface as None,
        // not a stale success — otherwise `hook_exe` finds no helper and hooks
        // go dark. The accompanying error log is the only failure signal for
        // callers that ignore the return.
        let directory = tempfile::tempdir().expect("tempdir");
        let cli = directory.path().join("lazybox");
        write_fake_exe(
            &cli,
            &format!(
                "#!/bin/sh\n[ \"$1\" = \"{}\" ] && echo {}\n",
                HOOK_HELPER_PROBE_ARG, HOOK_HELPER_PROBE_RESPONSE
            ),
        );
        // `bin` is a file, so `bin/lazybox`'s parent can never be a directory.
        let blocker = directory.path().join("bin");
        std::fs::write(&blocker, "not a directory").expect("write blocker");
        let stable = blocker.join("lazybox");

        assert_eq!(ensure_stable_hook_exe_from(&cli, &stable), None);
    }

    /// The injected hook command pins the *running* binary by absolute
    /// path, never bare `lazybox` — so PATH skew can't select a stale build
    /// that rejects the flags we baked in (#848). Covers both the Claude
    /// (`--backend-key`) and Codex (`--backend-key-file`) forms.
    #[test]
    fn hook_commands_use_absolute_exe_path() {
        let exe = hook_exe().expect("running test binary must resolve");
        assert!(exe.is_absolute(), "current_exe must be absolute: {exe:?}");
        let quoted = format!("\"{}\"", exe.display());

        let claude = hook_command(&exe, "lzb-sess-7");
        assert!(claude.contains(&quoted), "bare/relative exe in: {claude}");

        let codex = hook_command_keyfile(&exe, Path::new("/run/lzb/backend-key-7"));
        assert!(codex.contains(&quoted), "bare/relative exe in: {codex}");
    }

    #[test]
    fn hook_command_quotes_exe_and_bakes_backend_key() {
        let cmd = hook_command(Path::new("/opt/lazy box/lazybox"), "lzb-sess-7");
        assert!(
            cmd.contains("\"/opt/lazy box/lazybox\" hook-ingest --backend-key \"lzb-sess-7\""),
            "exec missing or unquoted: {cmd}"
        );
        assert!(
            cmd.starts_with("[ -x \"/opt/lazy box/lazybox\" ]"),
            "missing existence guard: {cmd}"
        );
    }

    #[test]
    fn hook_command_placeholder_is_guarded_and_flagless() {
        let cmd = hook_command_placeholder(Path::new("/opt/lazybox"));
        assert!(cmd.starts_with("[ -x \"/opt/lazybox\" ]"), "{cmd}");
        assert!(cmd.ends_with("\"/opt/lazybox\" hook-ingest"), "{cmd}");
    }

    #[test]
    fn hook_command_keyfile_reads_key_from_path() {
        // Codex's argv-baked hook command can't embed the backend key
        // (unknown at launch), so it reads it from the per-terminal file.
        let cmd = hook_command_keyfile(
            Path::new("/opt/lazy box/lazybox"),
            Path::new("/run/lzb/backend-key-7"),
        );
        assert!(
            cmd.contains(
                "\"/opt/lazy box/lazybox\" hook-ingest --backend-key-file \"/run/lzb/backend-key-7\""
            ),
            "keyfile flag missing or unquoted: {cmd}"
        );
        assert!(cmd.starts_with("[ -x \"/opt/lazy box/lazybox\" ]"), "{cmd}");
    }

    #[test]
    fn backend_key_file_round_trips_written_key() {
        // The daemon writes the key post-spawn; the baked command reads it
        // back through `--backend-key-file`. The path is deterministic in
        // the terminal id, so no bookkeeping is needed to clean it up.
        let tid = TerminalId(918_273);
        write_hook_backend_key(tid, "lazybox-ws-codex-42-7");
        let path = hook_backend_key_path(tid);
        let read = std::fs::read_to_string(&path).expect("key file written");
        assert_eq!(read, "lazybox-ws-codex-42-7");
        let _ = std::fs::remove_file(&path);
    }

    /// Through a real `/bin/sh`: an existing executable passes the guard
    /// and receives the hook-ingest argv (quoting survives the shell).
    #[cfg(unix)]
    #[test]
    fn hook_command_execs_existing_binary_via_sh() {
        let cmd = guarded_hook_command(
            Path::new("/bin/echo"),
            " --backend-key lzb-sess-7",
            &std::env::temp_dir().join("lazybox-unused-hook.log"),
        );
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &cmd])
            .output()
            .expect("sh runs");
        assert!(out.status.success(), "guard blocked an existing binary");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "hook-ingest --backend-key lzb-sess-7"
        );
    }

    /// Through a real `/bin/sh`: a binary deleted after spawn must NOT hard
    /// error the lifecycle hook. It exits 0 (no `PostToolUse:Bash hook
    /// error`), writes nothing to the agent's stderr, and records the cause
    /// in the hook log instead — a missed state signal, not a failure.
    #[cfg(unix)]
    #[test]
    fn hook_command_missing_binary_degrades_silently() {
        let dir = std::env::temp_dir().join(format!("lazybox-hook-missing-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("hook.log");
        let gone = "/nonexistent/target/debug/lazybox";
        let cmd = guarded_hook_command(Path::new(gone), " --backend-key \"lzb-sess-9\"", &log);
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &cmd])
            .output()
            .expect("sh runs");
        assert_eq!(
            out.status.code(),
            Some(0),
            "a lifecycle hook must not hard-error"
        );
        assert!(
            out.stderr.is_empty(),
            "nothing on the agent-facing stderr: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let logged = std::fs::read_to_string(&log).expect("hook log written");
        assert!(
            logged.contains("lazybox hook: binary missing at /nonexistent/target/debug/lazybox"),
            "log should name the cause: {logged}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_for_repo_case_sensitive() {
        let mut cfg = lazybox_config::Config::default();
        let mut env = std::collections::BTreeMap::new();
        env.insert("X".into(), "1".into());
        cfg.repos.insert(
            "Owner/Repo".into(),
            lazybox_config::RepoConfig {
                env,
                mounts: vec![],
                scripts: vec![],
                branch_prefix: None,
                bringup: None,
            },
        );
        // Different case should miss.
        assert!(env_for_repo(&cfg, "owner/repo").is_empty());
        assert_eq!(env_for_repo(&cfg, "Owner/Repo").len(), 1);
    }

    #[test]
    fn expand_tilde_replaces_leading_tilde_with_home() {
        // SAFETY: tests in this crate run with --test-threads default.
        // We don't read HOME elsewhere in this test file, and we
        // restore it on exit.
        let prior = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/fake-home");
        }
        let out = expand_tilde(std::path::Path::new("~/data"));
        assert_eq!(out, std::path::PathBuf::from("/tmp/fake-home/data"));
        // Non-tilde paths pass through unchanged.
        assert_eq!(
            expand_tilde(std::path::Path::new("/abs/path")),
            std::path::PathBuf::from("/abs/path")
        );
        unsafe {
            if let Some(p) = prior {
                std::env::set_var("HOME", p);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    #[test]
    fn config_mounts_to_git_translates_placement() {
        let specs = vec![
            lazybox_config::MountSpec {
                source: std::path::PathBuf::from("/a"),
                link_at: std::path::PathBuf::from("inside"),
                placement: lazybox_config::PlacementSpec::Inside,
            },
            lazybox_config::MountSpec {
                source: std::path::PathBuf::from("/b"),
                link_at: std::path::PathBuf::from("above"),
                placement: lazybox_config::PlacementSpec::Above,
            },
        ];
        let mounts = config_mounts_to_git(&specs);
        assert_eq!(mounts.len(), 2);
        assert!(matches!(
            mounts[0].placement,
            lazybox_git_ops::Placement::Inside
        ));
        assert!(matches!(
            mounts[1].placement,
            lazybox_git_ops::Placement::Above
        ));
    }

    fn task_for(source: &str, key: &str) -> Task {
        Task {
            author: String::new(),
            id: lazybox_core::TaskId {
                source: source.into(),
                key: key.into(),
            },
            title: "t".into(),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::default(),
            review: lazybox_core::ReviewStatus::default(),
            checks: vec![],
            unread_count: 0,
            url: String::new(),
            repo: Some("acme/widget".into()),
            branch: None,
            base_branch: None,
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Unknown,
            is_behind_base: false,
            merge_blocked: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    fn titled_task(source: &str, key: &str, title: &str) -> Task {
        let mut t = task_for(source, key);
        t.title = title.into();
        t
    }

    /// By default (empty prefix) an issue spawn reads naturally in the
    /// target repo: the issue number plus a slug of its title, no tool
    /// branding. Deterministic on the task so two spawns on the same
    /// issue land on the same branch instead of accumulating orphans.
    #[test]
    fn derive_branch_for_branchless_github_issue() {
        let t = titled_task("github", "acme/widget#42", "Standardize log output");
        assert_eq!(
            derive_branch_for_branchless("", &t),
            "issue-42-standardize-log-output"
        );
    }

    /// A title with no usable characters (emoji-only) falls back to the
    /// bare `issue-<n>` stem rather than a dangling dash.
    #[test]
    fn derive_branch_for_branchless_github_issue_empty_title() {
        let t = titled_task("github", "acme/widget#42", "🚀");
        assert_eq!(derive_branch_for_branchless("", &t), "issue-42");
    }

    /// Linear / non-GitHub keys go through the sanitizer fallback so
    /// any odd characters become dashes and the source prefix keeps
    /// branches namespaced per-provider, then the title slug is
    /// appended.
    #[test]
    fn derive_branch_for_branchless_linear() {
        let t = titled_task("linear", "ENG-456", "Ship it");
        assert_eq!(
            derive_branch_for_branchless("", &t),
            "linear-eng-456-ship-it"
        );
    }

    /// A configured `providers.linear.branch_template` renders the house
    /// convention (`{handle}/{type}/{id}-{slug}`), resolving `{type}` from
    /// the ticket's labels and lowercasing the ticket id.
    #[test]
    fn derive_linear_branch_renders_house_convention() {
        let mut cfg = lazybox_config::Config::default();
        cfg.providers.linear.handle = Some("antoine".into());
        cfg.providers.linear.branch_template = Some("{handle}/{type}/{id}-{slug}".into());
        cfg.providers
            .linear
            .label_types
            .insert("Feature".into(), "feat".into());
        let mut t = titled_task("linear", "OBI-1749", "Template SA seam");
        t.labels = vec![lazybox_core::Label::new("Feature")];
        assert_eq!(
            derive_linear_branch(&cfg, &t).as_deref(),
            Some("antoine/feat/obi-1749-template-sa-seam"),
        );
    }

    /// No template configured → `None`, so the caller falls back to the
    /// generic branchless naming.
    #[test]
    fn derive_linear_branch_without_template_is_none() {
        let cfg = lazybox_config::Config::default();
        let t = titled_task("linear", "OBI-1749", "Ship it");
        assert_eq!(derive_linear_branch(&cfg, &t), None);
    }

    /// An unmapped `{type}` (no matching label) collapses out of the
    /// branch rather than leaving an orphaned `//` separator.
    #[test]
    fn derive_linear_branch_collapses_unmapped_type() {
        let mut cfg = lazybox_config::Config::default();
        cfg.providers.linear.handle = Some("antoine".into());
        cfg.providers.linear.branch_template = Some("{handle}/{type}/{id}-{slug}".into());
        let t = titled_task("linear", "OBI-1749", "Ship it");
        assert_eq!(
            derive_linear_branch(&cfg, &t).as_deref(),
            Some("antoine/obi-1749-ship-it"),
        );
    }

    /// A `label_types` value with stray whitespace/case is sanitized into
    /// a valid ref segment rather than injecting an invalid git branch.
    #[test]
    fn derive_linear_branch_sanitizes_type_token() {
        let mut cfg = lazybox_config::Config::default();
        cfg.providers.linear.handle = Some("antoine".into());
        cfg.providers.linear.branch_template = Some("{handle}/{type}/{id}-{slug}".into());
        cfg.providers
            .linear
            .label_types
            .insert("Bug".into(), "Hot Fix".into());
        let mut t = titled_task("linear", "OBI-1749", "Ship it");
        t.labels = vec![lazybox_core::Label::new("Bug")];
        assert_eq!(
            derive_linear_branch(&cfg, &t).as_deref(),
            Some("antoine/hot-fix/obi-1749-ship-it"),
        );
    }

    /// A mapped Linear team resolves to its real GitHub repo — never the
    /// synthetic `linear/<team>`.
    #[test]
    fn linear_repo_for_task_resolves_mapped_team() {
        let mut cfg = lazybox_config::Config::default();
        cfg.providers
            .linear
            .teams
            .insert("OBI".into(), "obin-ai/obin-platform".into());
        let mut t = task_for("linear", "OBI-1749");
        t.repo = Some("linear/OBI".into());
        assert_eq!(
            linear_repo_for_task(&cfg, &t).unwrap(),
            "obin-ai/obin-platform",
        );
    }

    /// An unmapped team is a hard error, not a clone of `linear/<team>`.
    #[test]
    fn linear_repo_for_task_unmapped_team_errors() {
        let cfg = lazybox_config::Config::default();
        let mut t = task_for("linear", "OBI-1749");
        t.repo = Some("linear/OBI".into());
        let err = linear_repo_for_task(&cfg, &t).unwrap_err();
        assert!(err.to_string().contains("OBI"), "{err}");
    }

    /// A Linear ticket with no team at all is likewise a hard error.
    #[test]
    fn linear_repo_for_task_teamless_errors() {
        let cfg = lazybox_config::Config::default();
        let mut t = task_for("linear", "OBI-1749");
        t.repo = None;
        assert!(linear_repo_for_task(&cfg, &t).is_err());
    }

    /// A linked GitHub PR is authoritative: the ticket routes to that PR's
    /// repo even when the team has no config mapping (#944).
    #[test]
    fn linear_repo_for_task_prefers_linked_pr() {
        let cfg = lazybox_config::Config::default();
        let mut t = task_for("linear", "OBI-1749");
        t.repo = Some("linear/OBI".into());
        t.linked_tasks = vec![lazybox_core::TaskId {
            source: "github".into(),
            key: "obin-ai/obin-platform#42".into(),
        }];
        assert_eq!(
            linear_repo_for_task(&cfg, &t).unwrap(),
            "obin-ai/obin-platform",
        );
    }

    /// A linked PR under the *same owner* as the mapping refines it — the
    /// more precise target for a team whose issues span several repos in
    /// one org.
    #[test]
    fn linear_repo_for_task_same_owner_linked_pr_refines_mapping() {
        let mut cfg = lazybox_config::Config::default();
        cfg.providers
            .linear
            .teams
            .insert("OBI".into(), "obin-ai/some-other-repo".into());
        let mut t = task_for("linear", "OBI-1749");
        t.repo = Some("linear/OBI".into());
        t.linked_tasks = vec![lazybox_core::TaskId {
            source: "github".into(),
            key: "obin-ai/obin-platform#42".into(),
        }];
        assert_eq!(
            linear_repo_for_task(&cfg, &t).unwrap(),
            "obin-ai/obin-platform",
        );
    }

    /// A foreign-org linked PR (an untrusted attachment) does NOT override
    /// an explicit team mapping — config wins (#944 review F1).
    #[test]
    fn linear_repo_for_task_foreign_linked_pr_does_not_override_mapping() {
        let mut cfg = lazybox_config::Config::default();
        cfg.providers
            .linear
            .teams
            .insert("OBI".into(), "obin-ai/obin-platform".into());
        let mut t = task_for("linear", "OBI-1749");
        t.repo = Some("linear/OBI".into());
        t.linked_tasks = vec![lazybox_core::TaskId {
            source: "github".into(),
            key: "randomorg/fork#3".into(),
        }];
        assert_eq!(
            linear_repo_for_task(&cfg, &t).unwrap(),
            "obin-ai/obin-platform",
        );
    }

    /// A linked PR resolves an unmapped, teamless ticket that would
    /// otherwise be a hard error.
    #[test]
    fn linear_repo_for_task_linked_pr_resolves_teamless() {
        let cfg = lazybox_config::Config::default();
        let mut t = task_for("linear", "OBI-1749");
        t.repo = None;
        t.linked_tasks = vec![lazybox_core::TaskId {
            source: "github".into(),
            key: "obin-ai/obin-platform#42".into(),
        }];
        assert_eq!(
            linear_repo_for_task(&cfg, &t).unwrap(),
            "obin-ai/obin-platform",
        );
    }

    /// A non-numeric GitHub key (no `#`) falls through to the
    /// sanitizer instead of producing `issue-`.
    #[test]
    fn derive_branch_for_branchless_github_without_hash() {
        let t = titled_task("github", "acme/widget", "Some work");
        assert_eq!(
            derive_branch_for_branchless("", &t),
            "github-acme-widget-some-work"
        );
    }

    /// Blank-workspace branches come from the workspace key, so two
    /// spawns on the same workspace reuse one branch.
    #[test]
    fn derive_branch_for_workspace_uses_workspace_key() {
        let ws = Workspace::empty(WorkspaceKey::new("my-experiment"), "main", Utc::now());
        assert_eq!(derive_branch_for_workspace("", &ws), "my-experiment");
    }

    /// A non-empty prefix namespaces the branch — `lazybox` restores
    /// the historical `lazybox/issue-<n>` layout, and multi-segment
    /// prefixes keep their `/` separators.
    #[test]
    fn derive_branch_for_branchless_custom_prefix() {
        let t = titled_task("github", "acme/widget#42", "Fix the thing");
        assert_eq!(
            derive_branch_for_branchless("lazybox", &t),
            "lazybox/issue-42-fix-the-thing"
        );
        assert_eq!(
            derive_branch_for_branchless("team/feature", &t),
            "team/feature/issue-42-fix-the-thing"
        );
        let ws = Workspace::empty(WorkspaceKey::new("my-experiment"), "main", Utc::now());
        assert_eq!(
            derive_branch_for_workspace("lazybox", &ws),
            "lazybox/my-experiment"
        );
    }

    /// A prefix with stray characters and surrounding separators is
    /// sanitized to a valid branch fragment: spaces and `#` become
    /// dashes, leading/trailing `/` and `-` are trimmed.
    #[test]
    fn derive_branch_for_branchless_sanitizes_prefix() {
        let t = titled_task("github", "acme/widget#42", "Fix the thing");
        assert_eq!(
            derive_branch_for_branchless("/My Team#/", &t),
            "my-team/issue-42-fix-the-thing"
        );
    }

    /// Per-repo `branch_prefix` overrides the global one; an unmatched
    /// repo (or `None` override) falls back to the global value, which
    /// defaults to empty.
    #[test]
    fn resolve_branch_prefix_per_repo_override() {
        let mut cfg = lazybox_config::Config::default();
        assert_eq!(resolve_branch_prefix(&cfg, Some("acme/widget")), "");

        cfg.worktree.branch_prefix = "lazybox".to_string();
        cfg.repos.insert(
            "acme/widget".to_string(),
            lazybox_config::RepoConfig {
                branch_prefix: Some("at".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(resolve_branch_prefix(&cfg, Some("acme/widget")), "at");
        // Other repos and the repo-less path see the global value.
        assert_eq!(resolve_branch_prefix(&cfg, Some("acme/other")), "lazybox");
        assert_eq!(resolve_branch_prefix(&cfg, None), "lazybox");
    }

    fn save_project(config: &ServerConfig, project: lazybox_core::Project) {
        let record = lazybox_store::ProjectRecord {
            key: project.key.as_str().to_string(),
            created_at: project.created_at,
            project_json: Some(serde_json::to_string(&project).unwrap()),
        };
        config.store.save_project(&record).unwrap();
    }

    /// A blank workspace under a GitHub project recovers `owner/repo`
    /// from the project record, so its Claude sessions get a real clone
    /// instead of an empty directory.
    #[test]
    fn clonable_repo_from_project_recovers_github_owner_repo() {
        let config = ServerConfig::in_memory();
        let key = lazybox_core::ProjectKey::github("AntoineToussaint", "lazybox");
        save_project(
            &config,
            lazybox_core::Project::github("AntoineToussaint", "lazybox", Utc::now()),
        );
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(key);
        assert_eq!(
            clonable_repo_from_project(&config, &ws, None).unwrap(),
            "AntoineToussaint/lazybox"
        );
    }

    /// Regression for #326: a hyphenated owner (`codefly-dev`) can't be
    /// recovered from the flat key OR from a project record that was
    /// itself seeded from the lossy key (the blank-workspace path). The
    /// subscribed scope slug (`github:codefly-dev/warden-platform`)
    /// carries the boundary, so the clone target resolves correctly even
    /// with a mangled record present.
    #[test]
    fn clonable_repo_from_project_recovers_hyphenated_owner_from_scopes() {
        let config = ServerConfig::in_memory();
        let key = lazybox_core::ProjectKey::github("codefly-dev", "warden-platform");
        // The record the buggy blank-workspace path would have written.
        save_project(
            &config,
            lazybox_core::Project::new(key.clone(), key.display_name(), Utc::now()),
        );
        assert_eq!(key.display_name(), "codefly/dev-warden-platform");
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(key);
        let scopes = ["github:codefly-dev/warden-platform".to_string()]
            .into_iter()
            .collect();
        assert_eq!(
            clonable_repo_from_project(&config, &ws, Some(&scopes)).unwrap(),
            "codefly-dev/warden-platform"
        );
    }

    /// An explicit project repo remains exact when the owner contains a
    /// hyphen, independently of the mutable display name.
    #[test]
    fn clonable_repo_from_project_handles_hyphenated_owner() {
        let config = ServerConfig::in_memory();
        let key = lazybox_core::ProjectKey::github("mind-build", "mind");
        // Sanity: the lossy key path would mangle this.
        assert_eq!(key.display_name(), "mind/build-mind");
        let mut project = lazybox_core::Project::github("mind-build", "mind", Utc::now());
        project.name = "presentation label".to_string();
        save_project(&config, project);
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(key);
        assert_eq!(
            clonable_repo_from_project(&config, &ws, None).unwrap(),
            "mind-build/mind"
        );
    }

    /// No project record (never registered) falls back to the
    /// key-derived name — correct for non-hyphenated repos, the best we
    /// can do without the record.
    #[test]
    fn clonable_repo_from_project_falls_back_to_key_without_record() {
        let config = ServerConfig::in_memory();
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github(
            "AntoineToussaint",
            "lazybox",
        ));
        assert_eq!(
            clonable_repo_from_project(&config, &ws, None).unwrap(),
            "AntoineToussaint/lazybox"
        );
    }

    #[test]
    fn clonable_repo_from_project_rejects_ambiguous_key_without_canonical_slug() {
        let config = ServerConfig::in_memory();
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github(
            "codefly-dev",
            "warden-platform",
        ));

        assert!(clonable_repo_from_project(&config, &ws, None).is_err());
    }

    #[test]
    fn clonable_repo_from_project_rejects_legacy_display_name_as_identity() {
        let config = ServerConfig::in_memory();
        let key = lazybox_core::ProjectKey::github("codefly-dev", "warden-platform");
        save_project(
            &config,
            lazybox_core::Project::new(key.clone(), "codefly/dev-warden-platform", Utc::now()),
        );
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(key);

        let error = clonable_repo_from_project(&config, &ws, None)
            .expect_err("a display label cannot establish clone identity");
        assert!(
            error
                .to_string()
                .contains("no unambiguous GitHub repo slug")
        );
    }

    /// `local-` projects have no upstream repo and are not clone targets.
    #[test]
    fn clonable_repo_from_project_rejects_local_project() {
        let config = ServerConfig::in_memory();
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::local("my-experiment"));
        assert!(clonable_repo_from_project(&config, &ws, None).is_err());
    }

    #[test]
    fn clonable_repo_from_project_errs_without_project_or_task() {
        let config = ServerConfig::in_memory();
        let ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        assert!(clonable_repo_from_project(&config, &ws, None).is_err());
    }

    #[test]
    fn last_output_tail_strips_ansi_and_keeps_final_lines() {
        // A dying codex prints a colored error to the PTY; the frozen
        // pane must show the plain error, not escape soup (#368).
        let raw =
            b"\x1b[?1049h\x1b[2J\x1b[H\x1b[31mError:\x1b[0m not logged in\r\nrun `codex login`\r\n";
        let tail = last_output_tail(raw).expect("printable output yields a tail");
        assert_eq!(tail, "Error: not logged in\nrun `codex login`");
    }

    #[test]
    fn last_output_tail_keeps_only_content_after_carriage_return() {
        // Progress bars overwrite in place with a bare `\r`; only the
        // last-written state should survive.
        let raw = b"\x1b[33mDownloading 10%\x1b[0m\r\x1b[32mDownloading 100%\x1b[0m\n";
        assert_eq!(last_output_tail(raw).as_deref(), Some("Downloading 100%"));
    }

    #[test]
    fn last_output_tail_caps_to_the_last_lines() {
        let raw: Vec<u8> = (0..20)
            .flat_map(|n| format!("line {n}\n").into_bytes())
            .collect();
        let tail = last_output_tail(&raw).expect("tail");
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 8);
        assert_eq!(lines.first(), Some(&"line 12"));
        assert_eq!(lines.last(), Some(&"line 19"));
    }

    #[test]
    fn trailing_window_clamps_between_a_floor_and_ceiling() {
        // Below the floor, the whole (short) buffer is returned.
        let small = vec![b'x'; 10];
        assert_eq!(trailing_window(&small, 1).len(), 10);
        // A big buffer with a modest request is clipped to the 64 KiB floor,
        // keeping the tail (the window is taken from the end).
        let big = vec![b'y'; 2 * 1024 * 1024];
        let floored = trailing_window(&big, 1);
        assert_eq!(floored.len(), 64 * 1024);
        assert_eq!(floored, &big[big.len() - 64 * 1024..]);
        // A huge request can never scan more than the 1 MiB ceiling.
        assert_eq!(trailing_window(&big, usize::MAX).len(), 1024 * 1024);
    }

    #[test]
    fn agent_output_tail_honors_a_custom_line_budget() {
        // The gateway's `get_agent_output` reads more than the 8-line
        // dying-agent recap; the shared cleaner respects the requested cap.
        let raw: Vec<u8> = (0..20)
            .flat_map(|n| format!("line {n}\n").into_bytes())
            .collect();
        let tail = agent_output_tail(&raw, 3).expect("tail");
        assert_eq!(tail, "line 17\nline 18\nline 19");
    }

    #[test]
    fn last_output_tail_drops_string_sequences_not_just_osc() {
        // OSC window title, then a DCS payload (terminated by ST), then
        // the real error — none of the sequence bodies may leak.
        let raw = b"\x1b]0;codex\x07\x1bPq#0;2;0;0;0\x1b\\Fatal: config missing\n";
        assert_eq!(
            last_output_tail(raw).as_deref(),
            Some("Fatal: config missing")
        );
    }

    #[test]
    fn last_output_tail_drops_undecodable_bytes() {
        // A raw byte the ring couldn't decode (a truncated multibyte at
        // the ring boundary, or an 8-bit C1 control) becomes
        // `from_utf8_lossy`'s replacement char — noise that must not
        // litter the readable tail.
        let raw = b"Fatal: bad config\xff (see log)\n";
        assert_eq!(
            last_output_tail(raw).as_deref(),
            Some("Fatal: bad config (see log)")
        );
    }

    #[test]
    fn last_output_tail_is_none_for_blank_output() {
        // An agent that exits before printing anything (or only emits
        // screen-control bytes) leaves no readable tail.
        assert_eq!(last_output_tail(b""), None);
        assert_eq!(last_output_tail(b"\x1b[2J\x1b[H\r\n\r\n"), None);
    }

    /// Regression for #223: two workspaces with the same name in
    /// different repos must resolve to different worktree directories.
    /// Before the fix, both produced `<root>/issues` and collided.
    #[test]
    fn worktree_path_is_scoped_by_repo() {
        let named = |name: &str, project: lazybox_core::ProjectKey| {
            let mut ws = Workspace::empty(
                WorkspaceKey::new(format!("local:{name}")),
                "main",
                Utc::now(),
            );
            ws.name = name.into();
            ws.project_key = Some(project);
            ws
        };
        let a = named(
            "Issues",
            lazybox_core::ProjectKey::github("ownerA", "repoA"),
        );
        let b = named(
            "Issues",
            lazybox_core::ProjectKey::github("ownerB", "repoB"),
        );

        let path_a = worktree_path_for_session(&a, 0);
        let path_b = worktree_path_for_session(&b, 0);
        assert_ne!(path_a, path_b);
        assert_eq!(
            path_a,
            worktree_root().join("github-ownera-repoa").join("issues")
        );
        assert_eq!(
            path_b,
            worktree_root().join("github-ownerb-repob").join("issues")
        );

        // Second session keeps the `-2` suffix under the same scope.
        assert_eq!(
            worktree_path_for_session(&a, 1),
            worktree_root().join("github-ownera-repoa").join("issues-2"),
        );
    }

    #[test]
    fn main_worktree_path_is_shared_per_repo() {
        // #271: the main checkout is one shared worktree per repo scope
        // — two DIFFERENT workspaces on the same repo resolve the SAME
        // `<root>/<scope>/main` path (unlike per-session worktrees),
        // which is what makes "the main checkout" a single shared tree.
        let named = |name: &str, project: lazybox_core::ProjectKey| {
            let mut ws = Workspace::empty(
                WorkspaceKey::new(format!("local:{name}")),
                "main",
                Utc::now(),
            );
            ws.name = name.into();
            ws.project_key = Some(project);
            ws
        };
        let pr = named("PR 12", lazybox_core::ProjectKey::github("acme", "widget"));
        let issue = named("Issues", lazybox_core::ProjectKey::github("acme", "widget"));
        let other = named("Issues", lazybox_core::ProjectKey::github("acme", "gadget"));

        let expected = worktree_root().join("github-acme-widget").join("_main");
        assert_eq!(main_worktree_path(&pr), Some(expected.clone()));
        assert_eq!(
            main_worktree_path(&issue),
            Some(expected),
            "two workspaces on the same repo share one main checkout",
        );
        assert_ne!(
            main_worktree_path(&pr),
            main_worktree_path(&other),
            "different repos get different main checkouts",
        );

        // A repo-less / project-less workspace has no shared main.
        let bare = Workspace::empty(WorkspaceKey::new("bare"), "main", Utc::now());
        assert_eq!(main_worktree_path(&bare), None);

        // The shared segment must never collide with an isolated
        // per-session tree — even for a workspace literally named "main"
        // (`slugify("main") == "main"`), whose isolated path is
        // `<scope>/main`. `_main` is unreachable by `slugify`
        // ([a-z0-9-] only), so the two never share a directory.
        let mut named_main = Workspace::empty(WorkspaceKey::new("local:main"), "main", Utc::now());
        named_main.name = "main".into();
        named_main.project_key = Some(lazybox_core::ProjectKey::github("acme", "widget"));
        assert_ne!(
            main_worktree_path(&named_main),
            Some(worktree_path_for_session(&named_main, 0)),
            "shared main checkout must not collide with a `main`-named workspace's tree",
        );
    }

    #[tokio::test]
    async fn active_provision_claim_prevents_sessionless_reclaim() {
        fn git(cwd: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = tempfile::tempdir().unwrap();
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(upstream.path().join("README.md"), "base\n").unwrap();
        git(upstream.path(), &["add", "."]);
        git(upstream.path(), &["commit", "-q", "-m", "base"]);

        let config = ServerConfig::with_store_backend_and_worktree_root(
            std::sync::Arc::new(lazybox_store::MemoryStore::new()),
            std::sync::Arc::new(crate::backend::MockBackend::new()),
            root.path().to_path_buf(),
        );
        let manager = config.worktree_manager();
        let bare = manager.bare_path("acme", "core");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--bare",
                "-q",
                &upstream.path().to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        );
        git(&bare, &["branch", "feature", "main"]);
        let holder = root.path().join("worktrees").join("in-flight");
        std::fs::create_dir_all(holder.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                "feature",
                &holder.to_string_lossy(),
                "refs/heads/feature",
            ],
        );

        let claim_a = ProvisioningWorktreeClaim::new(&config, holder.clone());
        let claim_b = ProvisioningWorktreeClaim::new(&config, holder.clone());
        drop(claim_a);
        assert_eq!(
            reclaim_non_live_managed_holder(
                &config,
                &manager,
                "acme",
                "core",
                "feature",
                &holder,
                &root.path().join("worktrees").join("other"),
            )
            .await,
            BranchHolderReclaim::Preserved,
            "one completed concurrent claim must not expose the other spawn's checkout"
        );
        assert!(holder.exists());

        drop(claim_b);
        assert_eq!(
            reclaim_non_live_managed_holder(
                &config,
                &manager,
                "acme",
                "core",
                "feature",
                &holder,
                &root.path().join("worktrees").join("other"),
            )
            .await,
            BranchHolderReclaim::Reclaimed
        );
        assert!(!holder.exists());
    }

    /// A linked (no-worktree) workspace resolves every spawn straight to
    /// its on-disk checkout: the returned cwd is the linked path, it's
    /// reported as landed-on-main (so it reuses the shared-checkout
    /// singleton + auto-fix machinery), and NO worktree is provisioned
    /// under the state root. The `on_main` request flag and an explicit
    /// `session_id` don't change the landing — a linked workspace has no
    /// isolated per-session trees.
    #[tokio::test]
    async fn linked_workspace_spawns_directly_in_the_checkout() {
        let config = ServerConfig::in_memory();
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("acme").join("widget");
        std::fs::create_dir_all(&checkout).unwrap();

        let mut ws = Workspace::empty(WorkspaceKey::new("acme-widget"), "feature-x", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github("acme", "widget"));
        ws.local = true;
        ws.linked_checkout = Some(checkout.clone());
        config
            .store
            .save_workspace(&WorkspaceRecord {
                key: ws.key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).unwrap()),
            })
            .unwrap();

        let session_key = SessionKey::new("acme-widget");
        let kind = TerminalKind::Agent("claude".into());
        // Even with on_main=false and a bogus session_id, the linked
        // branch wins.
        let (path, _id, landed_on_main) = resolve_or_create_session(
            &config,
            &session_key,
            Some(SessionId::new()),
            &kind,
            false,
            lazybox_ipc::SpawnOrigin::Interactive,
        )
        .await
        .expect("linked spawn resolves");

        assert_eq!(path, checkout, "sessions land in the real checkout");
        assert!(
            landed_on_main,
            "linked spawns reuse the shared-checkout path"
        );
        // No worktree provisioned anywhere under the managed root.
        assert!(
            !main_worktree_path(&ws).is_some_and(|p| p.exists()),
            "a linked workspace must not provision a `_main` worktree",
        );
    }

    /// End-to-end through `provision_worktree`: a blank workspace under
    /// a local project has no repo to clone, so it gets a standalone
    /// `git init` worktree on the workspace-key branch rather than a
    /// bare, non-git directory (#57).
    #[tokio::test]
    async fn provision_worktree_blank_local_workspace_inits_standalone() {
        let config = ServerConfig::in_memory();
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::local("notes"));
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("worktree");
        let session_key = SessionKey::new("scratch");
        let mut bus_rx = config.bus.subscribe();
        // An autonomous provision must stamp EVERY progress event with
        // its origin so the client can route the whole stream to a
        // footer notice rather than a modal (issue #645).
        provision_worktree(
            &config,
            &ws,
            &dir,
            &session_key,
            false,
            None,
            lazybox_ipc::SpawnOrigin::Autonomous(lazybox_ipc::AutonomousTrigger::Mention),
        )
        .await
        .unwrap();
        assert!(dir.join(".git").exists(), "a real git repo was created");

        // The checklist-driving progress events fire in order. A
        // standalone init has no clone or fetch to do, so the modal
        // mounts on the leading Fetch Started (never a Clone row, which
        // would imply a per-workspace clone), the worktree-add phase
        // animates, then Setup runs — all keyed to the spawn's session.
        let mut progress = Vec::new();
        while let Ok(ev) = bus_rx.try_recv() {
            if let Event::WorktreeProgress {
                session_key: sk,
                step,
                status,
                origin,
            } = ev
            {
                assert_eq!(sk, session_key);
                assert_eq!(
                    origin,
                    lazybox_ipc::SpawnOrigin::Autonomous(lazybox_ipc::AutonomousTrigger::Mention),
                    "the spawn origin must ride every progress step, not just the first",
                );
                progress.push((step, status));
            }
        }
        assert_eq!(
            progress,
            vec![
                (WorktreeStep::Fetch, WorktreeStepStatus::Started),
                (WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started),
                (WorktreeStep::Setup, WorktreeStepStatus::Started),
                (WorktreeStep::Setup, WorktreeStepStatus::Done),
            ],
        );
        let head = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "scratch",
            "standalone worktree is on the workspace branch",
        );
    }

    /// Issue #787: a second attempt on the same issue after its title
    /// drifted must reuse the branch already checked out at the target
    /// worktree, not derive a colliding `issue-N-<new-slug>` and hard-fail
    /// `BranchMismatch` against a workspace's own prior attempt.
    #[tokio::test]
    async fn reprovision_reuses_own_worktree_branch_across_title_drift() {
        fn git(cwd: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = tempfile::tempdir().unwrap();
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(upstream.path().join("README.md"), "base\n").unwrap();
        git(upstream.path(), &["add", "."]);
        git(upstream.path(), &["commit", "-q", "-m", "base"]);

        let config = ServerConfig::with_store_backend_and_worktree_root(
            std::sync::Arc::new(lazybox_store::MemoryStore::new()),
            std::sync::Arc::new(crate::backend::MockBackend::new()),
            root.path().to_path_buf(),
        );
        let bare = config.worktree_manager().bare_path("acme", "core");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--bare",
                "-q",
                &upstream.path().to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        );

        let session_key = SessionKey::new("github:acme/core#271");
        let target = root.path().join("wt");

        // First attempt: the issue is titled "large sample rung".
        let mut task = titled_task("github", "acme/core#271", "large sample rung");
        task.repo = Some("acme/core".into());
        let ws1 = Workspace::from_task(task, Utc::now());
        let branch1 = provision_worktree(
            &config,
            &ws1,
            &target,
            &session_key,
            false,
            None,
            lazybox_ipc::SpawnOrigin::Interactive,
        )
        .await
        .expect("first provision");
        assert_eq!(branch1, "issue-271-large-sample-rung");

        // The issue is retitled before a second attempt. Re-deriving from
        // the new title would collide with the branch already on disk.
        let mut task2 = titled_task(
            "github",
            "acme/core#271",
            "sample repos are not independent",
        );
        task2.repo = Some("acme/core".into());
        let ws2 = Workspace::from_task(task2, Utc::now());
        let branch2 = provision_worktree(
            &config,
            &ws2,
            &target,
            &session_key,
            false,
            None,
            lazybox_ipc::SpawnOrigin::Interactive,
        )
        .await
        .expect("second provision must not BranchMismatch against its own worktree");
        assert_eq!(
            branch2, "issue-271-large-sample-rung",
            "the workspace reuses its own on-disk branch instead of thrashing",
        );
    }

    /// Issue #787: recovering a `BranchHeldManaged` conflict preserves the
    /// named holder aside (keeping its files) and frees its branch, so a
    /// fresh provision on that branch then succeeds — the server half of
    /// the in-modal recreate.
    #[tokio::test]
    async fn preserve_stuck_worktree_moves_the_named_holder_and_frees_its_branch() {
        fn git(cwd: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = tempfile::tempdir().unwrap();
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(upstream.path().join("README.md"), "base\n").unwrap();
        git(upstream.path(), &["add", "."]);
        git(upstream.path(), &["commit", "-q", "-m", "base"]);

        let config = ServerConfig::with_store_backend_and_worktree_root(
            std::sync::Arc::new(lazybox_store::MemoryStore::new()),
            std::sync::Arc::new(crate::backend::MockBackend::new()),
            root.path().to_path_buf(),
        );
        let mgr = config.worktree_manager();
        let bare = mgr.bare_path("acme", "core");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--bare",
                "-q",
                &upstream.path().to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        );

        // A non-live managed worktree holds branch `feat`.
        let holder = root.path().join("holder");
        mgr.checkout_new_branch_at(&holder, "acme", "core", "feat", "main")
            .await
            .expect("provision holder");
        std::fs::write(holder.join("wip.txt"), "unsaved\n").unwrap();

        // Persist the stuck workspace so its repo resolves for the recovery.
        let mut task = titled_task("github", "acme/core#271", "the stuck issue");
        task.repo = Some("acme/core".into());
        let ws = Workspace::from_task(task, Utc::now());
        let session_key: SessionKey = SessionKey::new(ws.key.as_str());
        persist_and_broadcast(&config, &ws).await.unwrap();

        let backup = preserve_stuck_worktree(
            &config,
            &session_key,
            None,
            false,
            Some(holder.to_string_lossy().into_owned()),
        )
        .await
        .expect("preserve runs")
        .expect("the holder was preserved");
        assert!(!holder.exists(), "holder moved aside");
        assert_eq!(
            std::fs::read_to_string(backup.join("wip.txt")).unwrap(),
            "unsaved\n",
            "the holder's uncommitted work is preserved",
        );

        // The branch is now free: a fresh worktree add on `feat` succeeds.
        let fresh = root.path().join("fresh");
        mgr.checkout_new_branch_at(&fresh, "acme", "core", "feat", "main")
            .await
            .expect("branch freed for a fresh provision");
    }

    /// Issue #787 review #1: a `None` (own-worktree) recreate must preserve
    /// the checkout the spawn actually resolves to — the workspace's
    /// default (most-recent) session — not always session index 0. A
    /// multi-session workspace would otherwise have an unrelated (possibly
    /// live) worktree moved aside while the stuck one stayed put.
    #[tokio::test]
    async fn preserve_stuck_worktree_targets_the_default_session_not_index_zero() {
        fn git(cwd: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = tempfile::tempdir().unwrap();
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(upstream.path().join("README.md"), "base\n").unwrap();
        git(upstream.path(), &["add", "."]);
        git(upstream.path(), &["commit", "-q", "-m", "base"]);

        let config = ServerConfig::with_store_backend_and_worktree_root(
            std::sync::Arc::new(lazybox_store::MemoryStore::new()),
            std::sync::Arc::new(crate::backend::MockBackend::new()),
            root.path().to_path_buf(),
        );
        let mgr = config.worktree_manager();
        let bare = mgr.bare_path("acme", "core");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--bare",
                "-q",
                &upstream.path().to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        );

        // Two managed worktrees for one workspace: an earlier index-0
        // session and a later (default) session — the one a bare recreate
        // must preserve.
        let wt_first = root.path().join("first");
        let wt_default = root.path().join("default");
        mgr.checkout_new_branch_at(&wt_first, "acme", "core", "sess-first", "main")
            .await
            .expect("provision first");
        mgr.checkout_new_branch_at(&wt_default, "acme", "core", "sess-default", "main")
            .await
            .expect("provision default");
        std::fs::write(wt_default.join("wip.txt"), "unsaved\n").unwrap();

        let mut task = titled_task("github", "acme/core#271", "the stuck issue");
        task.repo = Some("acme/core".into());
        let mut ws = Workspace::from_task(task, Utc::now());
        let t0 = Utc::now();
        ws.add_session(lazybox_core::WorkspaceSession::new(
            ws.key.clone(),
            lazybox_core::SessionKind::Shell,
            wt_first.clone(),
            t0,
        ));
        ws.add_session(lazybox_core::WorkspaceSession::new(
            ws.key.clone(),
            lazybox_core::SessionKind::Shell,
            wt_default.clone(),
            t0 + chrono::Duration::seconds(60),
        ));
        let session_key = SessionKey::new(ws.key.as_str());
        persist_and_broadcast(&config, &ws).await.unwrap();

        let backup = preserve_stuck_worktree(&config, &session_key, None, false, None)
            .await
            .expect("preserve runs")
            .expect("the default session's worktree was preserved");
        assert!(
            !wt_default.exists(),
            "the spawn's target (default session) moved aside",
        );
        assert!(
            wt_first.exists(),
            "the unrelated index-0 session must be left untouched",
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("wip.txt")).unwrap(),
            "unsaved\n",
        );
    }

    #[tokio::test]
    async fn unresolved_github_project_does_not_initialize_a_standalone_repo() {
        let config = ServerConfig::in_memory();
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github(
            "unresolvable-owner-name",
            "unresolvable-repo-name",
        ));
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("worktree");

        let error = provision_worktree(
            &config,
            &ws,
            &dir,
            &SessionKey::new("scratch"),
            true,
            None,
            lazybox_ipc::SpawnOrigin::Interactive,
        )
        .await
        .expect_err("an unresolved GitHub project must abort provisioning");

        assert!(
            error
                .to_string()
                .contains("no unambiguous GitHub repo slug")
        );
        assert!(
            !dir.exists(),
            "resolution failure must not masquerade as a standalone checkout"
        );
    }

    /// A failed worktree provision must FAIL the spawn — not persist a
    /// session pinned to a fabricated empty dir. The old fallback
    /// `mkdir`'d the path, persisted the session anyway, and every
    /// later spawn short-circuited on `path.exists()` into a non-git
    /// folder with no route back to the repair machinery.
    #[tokio::test]
    async fn failed_provision_fails_spawn_without_persisting_a_session() {
        let config = ServerConfig::in_memory();
        let mut task = task_for("github", "acme/widget#94242");
        // A repo that isn't `owner/name` fails provisioning before any
        // git or network work — a deterministic offline failure.
        task.repo = Some("not-owner-name-format".into());
        let session_key = persist_task_workspace(&config, task);
        let kind = TerminalKind::Agent("claude".into());
        let mut bus_rx = config.bus.subscribe();

        let err = resolve_or_create_session(
            &config,
            &session_key,
            None,
            &kind,
            false,
            lazybox_ipc::SpawnOrigin::Interactive,
        )
        .await
        .expect_err("provision failure must fail the spawn loudly");
        assert!(
            err.to_string().contains("spawn aborted"),
            "the error names the abort: {err}"
        );

        // Issue #557 acceptance #2: the ✗ must land on the phase that
        // actually aborted, not always "Cloning". A malformed repo fails
        // pre-git, so the failure is reported on the Fetch (Preparing)
        // row and its message classifies to the BadRepo recovery class —
        // which the modal reads to render its affordance.
        let mut failed = None;
        while let Ok(ev) = bus_rx.try_recv() {
            if let Event::WorktreeProgress {
                step,
                status: WorktreeStepStatus::Failed(msg),
                ..
            } = ev
            {
                failed = Some((step, msg));
            }
        }
        let (step, msg) = failed.expect("a Failed progress event is emitted");
        assert_ne!(step, WorktreeStep::Clone, "no always-Clone mislabel: {msg}");
        assert_eq!(step, lazybox_ipc::WorktreeRecovery::BadRepo.failed_step());
        assert_eq!(
            lazybox_ipc::WorktreeRecovery::classify(&msg),
            lazybox_ipc::WorktreeRecovery::BadRepo,
            "message classifies to the BadRepo recovery class: {msg}"
        );

        let ws = load_workspace(&config, &WorkspaceKey::new(session_key.as_str()))
            .expect("workspace record survives");
        assert!(
            ws.sessions.is_empty(),
            "no session may be persisted for a worktree that was never provisioned"
        );
        let path = worktree_path_for_session(&ws, 0);
        assert!(
            !path.exists(),
            "no empty dir fabricated at {}",
            path.display()
        );
    }

    /// `ensure_worktree_present` used to hard-return on `path.exists()`,
    /// which pinned persisted sessions to whatever debris sat at the
    /// path (the empty dir an old failed provision fabricated). It now
    /// routes anything that isn't a completed checkout back through
    /// provisioning — here the standalone-init path, exercised offline.
    #[tokio::test]
    async fn ensure_worktree_present_reprovisions_a_non_checkout_dir() {
        let config = ServerConfig::in_memory();
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch-repair"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::local("notes"));
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("worktree");
        // The empty non-git dir the old fallback left behind.
        std::fs::create_dir_all(&dir).expect("mkdir");
        let session_key = SessionKey::new("scratch-repair");

        ensure_worktree_present(
            &config,
            &ws,
            &dir,
            None,
            &session_key,
            lazybox_ipc::SpawnOrigin::Interactive,
        )
        .await
        .expect("an empty leftover dir must be repaired, not trusted");
        assert!(
            dir.join(".git").exists(),
            "the empty dir became a real checkout"
        );

        // A healthy checkout short-circuits without touching anything.
        std::fs::write(dir.join("work.txt"), "user work").expect("write");
        ensure_worktree_present(
            &config,
            &ws,
            &dir,
            None,
            &session_key,
            lazybox_ipc::SpawnOrigin::Interactive,
        )
        .await
        .expect("healthy fast path");
        assert!(dir.join("work.txt").exists(), "fast path is a no-op");
    }

    const HARD: std::time::Duration = std::time::Duration::from_secs(10);
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(600);

    /// Gated agents (Claude) must NOT release on the settle timer:
    /// the folder-trust prompt may still be up. Even with first output
    /// already seen and well past the settle window, the inject window
    /// stays closed until the ready signal — proven here by the helper
    /// outlasting a timeout that's longer than SETTLE but shorter than
    /// HARD_DEADLINE.
    #[tokio::test(start_paused = true)]
    async fn gated_agent_ignores_settle_timer() {
        let ready = tokio::sync::Notify::new();
        let first_output = tokio::sync::Notify::new();
        first_output.notify_waiters();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            await_inject_window(true, &ready, &first_output, HARD, SETTLE),
        )
        .await;
        assert!(
            res.is_err(),
            "gated path must not write while no ready signal has fired",
        );
    }

    /// Gated agents release as soon as the ready signal fires, well
    /// before the hard deadline.
    #[tokio::test(start_paused = true)]
    async fn gated_agent_releases_on_ready() {
        let ready = tokio::sync::Notify::new();
        let first_output = tokio::sync::Notify::new();
        let trigger = tokio::select! {
            t = await_inject_window(true, &ready, &first_output, HARD, SETTLE) => t,
            _ = async {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                ready.notify_waiters();
                std::future::pending::<()>().await;
            } => unreachable!(),
        };
        assert_eq!(trigger, InjectTrigger::Ready);
    }

    /// A ready signal fired BEFORE the inject task registers its waiter
    /// must release the window immediately. `notify_one` stores a
    /// permit; the old `notify_waiters` was edge-triggered, so a fast
    /// pump that signalled ready first lost the wakeup forever and the
    /// inject rode the 10s hard deadline.
    #[tokio::test(start_paused = true)]
    async fn gated_agent_consumes_ready_signal_fired_before_wait() {
        let ready = tokio::sync::Notify::new();
        let first_output = tokio::sync::Notify::new();
        ready.notify_one();
        let start = tokio::time::Instant::now();
        let trigger = await_inject_window(true, &ready, &first_output, HARD, SETTLE).await;
        assert_eq!(trigger, InjectTrigger::Ready);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "pre-fired ready signal must release promptly, not ride a deadline",
        );
    }

    /// Same permit semantics for the detector-less first-output path —
    /// a replay that lands before the inject task waits must still
    /// release the settle rung.
    #[tokio::test(start_paused = true)]
    async fn detectorless_agent_consumes_first_output_fired_before_wait() {
        let ready = tokio::sync::Notify::new();
        let first_output = tokio::sync::Notify::new();
        first_output.notify_one();
        let start = tokio::time::Instant::now();
        let trigger = await_inject_window(false, &ready, &first_output, HARD, SETTLE).await;
        assert_eq!(trigger, InjectTrigger::Settle);
        assert!(
            start.elapsed() <= SETTLE + std::time::Duration::from_millis(50),
            "pre-fired first-output must release after one settle, not the deadline",
        );
    }

    /// Gated agents with a stuck readiness detector still inject at the
    /// hard deadline rather than silently dropping the prompt.
    #[tokio::test(start_paused = true)]
    async fn gated_agent_falls_back_to_deadline() {
        let ready = tokio::sync::Notify::new();
        let first_output = tokio::sync::Notify::new();
        let trigger = await_inject_window(true, &ready, &first_output, HARD, SETTLE).await;
        assert_eq!(trigger, InjectTrigger::Deadline);
    }

    /// Detector-less agents keep the first-output + settle path: with
    /// no ready signal ever, they still inject one settle past the
    /// first byte instead of waiting the full hard deadline.
    #[tokio::test(start_paused = true)]
    async fn detectorless_agent_writes_on_settle() {
        let ready = tokio::sync::Notify::new();
        let first_output = tokio::sync::Notify::new();
        let trigger = tokio::select! {
            t = await_inject_window(false, &ready, &first_output, HARD, SETTLE) => t,
            _ = async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                first_output.notify_waiters();
                std::future::pending::<()>().await;
            } => unreachable!(),
        };
        assert_eq!(trigger, InjectTrigger::Settle);
    }

    /// A prompt parked past the deadline must be released the instant the
    /// agent reaches ready — even though `ready` only fires once, and even
    /// when that firing lands before the waiter registers (`notify_one`
    /// stores a permit). This is the "pending prompt survives a gate and
    /// is delivered once ready" path.
    #[tokio::test(start_paused = true)]
    async fn pending_prompt_released_when_agent_becomes_ready() {
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let terminals =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                (TerminalId(7), "k".to_string()),
            ])));
        let ready_signal = ready.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            ready_signal.notify_one();
        });
        assert_eq!(
            await_pending_ready(TerminalId(7), &ready, &terminals, PENDING_READY_CAP).await,
            PendingReady::Ready,
            "ready firing must release the pending prompt for delivery",
        );
    }

    /// When the terminal exits before the agent ever reaches ready, the
    /// pending prompt can't be delivered — the helper reports failure so
    /// the caller surfaces it instead of leaking the task forever. The
    /// pump signals exit by removing the id from the `terminals` map.
    #[tokio::test(start_paused = true)]
    async fn pending_prompt_gives_up_when_terminal_exits() {
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let terminals =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                (TerminalId(7), "k".to_string()),
            ])));
        let terminals_for_exit = terminals.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            terminals_for_exit.lock().await.remove(&TerminalId(7));
        });
        assert_eq!(
            await_pending_ready(TerminalId(7), &ready, &terminals, PENDING_READY_CAP).await,
            PendingReady::TerminalGone,
            "a terminal that exits before ready must end the wait as a failure",
        );
    }

    /// Drives the pump's two state paths — [`note_pty_activity`] per PTY
    /// chunk and [`classify_quiet_screen`] for the post-quiet
    /// classification — the way the output pump does: one rolling buffer
    /// and the state machine persist across calls. Collects the
    /// `AgentState` the bus emits so a test can assert on the
    /// emitted-on-change *sequence*, which is what the #167/#161 bugs were
    /// about, rather than a single frame's classification.
    struct PumpDriver {
        agent: std::sync::Arc<dyn lazybox_agents::Agent>,
        buf: Vec<u8>,
        last_chunk_len: usize,
        terminals: TerminalRegistry,
        durability: AgentStateDurability,
        bus: tokio::sync::broadcast::Sender<Event>,
        rx: tokio::sync::broadcast::Receiver<Event>,
        id: TerminalId,
        session_key: SessionKey,
        state_machine: lazybox_agents::AgentStateMachine,
        next_output_seq: u64,
        /// The pump's rolling content fingerprint, mirrored so `feed`
        /// derives each chunk's progress bit the same way production does.
        watchdog_fp: Option<u64>,
    }

    impl PumpDriver {
        // Both hysteresis arguments are vestigial — the `Working → Idle`
        // edge is forbidden outright (no working-exit flap) and the
        // `InputNeeded` exit is now sticky against ambiguous readings with
        // no time bound (#374), so neither state carries a timing window
        // anymore — but kept in the signature so the many call sites read
        // uniformly. The driver starts booted: these tests exercise the
        // steady-state pump, not the boot gate (that lives in the
        // state-machine unit tests).
        fn new(input_hysteresis: Duration, working_hysteresis: Duration) -> Self {
            let agent = lazybox_agents::registry()
                .get("claude")
                .expect("claude agent is a built-in");
            Self::with_agent(agent, input_hysteresis, working_hysteresis)
        }

        /// A driver over an explicit agent — used to exercise a detector
        /// that returns `None` at rest (a pattern-less `GenericCli`), which
        /// the built-in Claude/Codex detectors never do.
        fn with_agent(
            agent: std::sync::Arc<dyn lazybox_agents::Agent>,
            _input_hysteresis: Duration,
            _working_hysteresis: Duration,
        ) -> Self {
            let id = TerminalId(7);
            let session_key: SessionKey = "github-o-r-1".into();
            let (bus, rx) = tokio::sync::broadcast::channel(256);
            let agent_id = agent.id().to_string();
            let terminals = TerminalRegistry::default();
            terminals
                .terminal_meta
                .try_lock()
                .expect("fresh terminal registry is unlocked")
                .insert(
                    id,
                    (
                        session_key.clone(),
                        lazybox_ipc::TerminalKind::Agent(agent_id),
                    ),
                );
            Self {
                agent,
                buf: Vec::new(),
                watchdog_fp: None,
                last_chunk_len: 0,
                next_output_seq: 1,
                terminals,
                durability: test_agent_state_durability(id),
                bus,
                rx,
                id,
                session_key,
                state_machine: {
                    let mut m = lazybox_agents::AgentStateMachine::new();
                    m.mark_booted();
                    m
                },
            }
        }

        /// Feed one PTY chunk; return the `AgentState`s broadcast for this
        /// terminal as a result (usually 0 or 1). Mirrors the pump's chunk
        /// arm: a pending answer reset drops the accumulated detection
        /// buffer before the chunk is ingested, and the progress bit is
        /// derived through the rolling content fingerprint — so churn vs
        /// content behaves as in production.
        async fn feed(&mut self, bytes: &[u8]) -> Vec<lazybox_ipc::AgentState> {
            let output_seq = self.next_output_seq;
            self.feed_at(output_seq, bytes).await
        }

        async fn feed_at(&mut self, output_seq: u64, bytes: &[u8]) -> Vec<lazybox_ipc::AgentState> {
            if self
                .terminals
                .agent_detect_resets
                .lock()
                .await
                .remove(&self.id)
            {
                self.buf.clear();
            }
            let progress = watchdog_notes_progress(&mut self.watchdog_fp, bytes);
            self.next_output_seq = self.next_output_seq.max(output_seq.saturating_add(1));
            note_pty_activity(
                Some(&self.agent),
                &mut self.buf,
                bytes,
                output_seq,
                progress,
                &self.terminals,
                &self.bus,
                Some(&self.durability),
                self.id,
                &self.session_key,
                &mut self.state_machine,
            )
            .await;
            self.last_chunk_len = bytes.len();
            self.drain()
        }

        /// The pump's quiet timer fired — PTY_QUIET_CLASSIFY_AFTER of
        /// silence — so classify the resting screen; return the
        /// `AgentState`s broadcast as a result.
        async fn quiet(&mut self) -> Vec<lazybox_ipc::AgentState> {
            classify_quiet_screen(
                Some(&self.agent),
                &self.buf,
                self.last_chunk_len,
                lazybox_agents::Liveness::Silent,
                &self.terminals,
                &self.bus,
                Some(&self.durability),
                self.id,
                &self.session_key,
                &mut self.state_machine,
            )
            .await;
            self.drain()
        }

        async fn view_action(&self) {
            self.view_action_after(self.next_output_seq.saturating_sub(1))
                .await;
        }

        async fn view_action_after(&self, after_seq: u64) {
            terminal_io::record_view_activity(&self.terminals, self.id, after_seq).await;
        }

        /// The user answers the parked prompt through lazybox:
        /// `handle_write`'s optimistic flip commits `Working` into the
        /// state cache and marks the detection buffer for reset (#101).
        /// The pump's own state machine is not consulted by the flip.
        async fn answer(&mut self) {
            self.terminals
                .agent_states
                .lock()
                .await
                .insert(self.id, lazybox_ipc::AgentState::Working);
            self.terminals
                .agent_detect_resets
                .lock()
                .await
                .insert(self.id);
        }

        /// A lifecycle hook just landed for this terminal — the pump
        /// records its arrival instant in `hook_driven`. A reading taken
        /// within [`lazybox_agents::HOOK_STALENESS`] of it is gated by the
        /// hooks-primary policy, so this is how a test asserts the PTY
        /// paths defer to a still-fresh hook.
        async fn hook_now(&mut self) {
            self.terminals
                .hook_driven_terminals
                .lock()
                .await
                .insert(self.id, std::time::Instant::now());
        }

        /// The pump's content-stability watchdog fired —
        /// [`WORKING_WATCHDOG_AFTER`] with no meaningful content change — so
        /// re-classify the screen, force a still-`Working` turn closed, and
        /// resolve a stale `InputNeeded`; return the `AgentState`s broadcast
        /// as a result.
        async fn watchdog(&mut self) -> Vec<lazybox_ipc::AgentState> {
            watchdog_reverify_parked_turn(
                Some(&self.agent),
                &self.buf,
                self.last_chunk_len,
                &self.terminals,
                &self.bus,
                Some(&self.durability),
                self.id,
                &self.session_key,
                &mut self.state_machine,
            )
            .await;
            self.drain()
        }

        fn drain(&mut self) -> Vec<lazybox_ipc::AgentState> {
            let mut out = Vec::new();
            while let Ok(ev) = self.rx.try_recv() {
                if let Event::AgentState {
                    terminal_id, state, ..
                } = ev
                    && terminal_id == self.id
                {
                    out.push(state);
                }
            }
            out
        }

        /// The terminal's current cached state (what the sidebar pill reads).
        async fn state(&self) -> Option<lazybox_ipc::AgentState> {
            self.terminals
                .agent_states
                .lock()
                .await
                .get(&self.id)
                .copied()
        }

        /// Reset the state machine to a freshly-spawned, **un-booted**
        /// terminal — the boot gate holds an ambiguous byte-flow `Working`
        /// until the composer is first classified. The default driver
        /// pre-boots for steady-state tests; a full-lifecycle timeline that
        /// starts at the boot→Idle edge needs the gate live.
        fn unbooted(mut self) -> Self {
            self.state_machine = lazybox_agents::AgentStateMachine::new();
            self
        }
    }

    /// The pump's two-path model (#289) under the one-way-door rule (#357)
    /// and the `InputNeeded`-stickiness rule (#374): chunks only ever read
    /// `Working` (bytes flowing = the agent is doing something); the
    /// classifier runs at the quiet boundary and decides the terminal state.
    /// A working agent that settles at a resting composer has **finished a
    /// turn** — the classifier promotes it to `Done`, never back to the
    /// never-worked `Idle`. `Done` then resists ambiguous byte flow, and so
    /// does a parked `InputNeeded`: an incidental repaint (a click, a focus,
    /// a redraw) can't clear either. Only a clear quiet-classification — the
    /// resumed stream painting a live status line — leaves them. ZERO
    /// hysteresis so no timing window is involved: the stickiness is
    /// structural, not a damped flap.
    #[tokio::test]
    async fn agent_state_transitions_emit_an_ordered_sequence() {
        use lazybox_ipc::AgentState::{Done, InputNeeded, Working};
        let idle = include_bytes!("../../agents/tests/fixtures/idle_composer.bin");
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");
        let input = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");

        let mut p = PumpDriver::new(Duration::ZERO, Duration::ZERO);
        let mut seq = Vec::new();
        seq.extend(p.feed(working).await); // bytes flowing → Working
        seq.extend(p.feed(idle).await); // still streaming → Working (deduped)
        seq.extend(p.quiet().await); // came to rest after working → Done
        seq.extend(p.feed(working).await); // stray byte flow can't un-finish Done
        seq.extend(p.feed(input).await); // dialog paints → still held against Done
        seq.extend(p.quiet().await); // dialog at rest → InputNeeded
        seq.extend(p.feed(working).await); // resumed byte flow is ambiguous — held off the `?` (#374)
        seq.extend(p.quiet().await); // resumed stream's live status line at rest → Working

        assert_eq!(
            seq,
            vec![Working, Done, InputNeeded, Working],
            "a settled worker is Done (never Idle); Done and a parked `?` both \
             resist byte flow and clear only on a live classification",
        );
    }

    /// One agent's full lifecycle expressed as its real captured PTY
    /// transcripts, one per phase.
    struct GoldenLifecycle {
        agent_id: &'static str,
        /// The resting composer at spawn, and again when a turn ends.
        idle: &'static [u8],
        /// A live status line mid-turn.
        working: &'static [u8],
        /// A structural approval/permission prompt.
        ask: &'static [u8],
    }

    /// **Per-agent golden lifecycle timeline (#538).** Drives the *real*
    /// pump — `note_pty_activity` per chunk, `classify_quiet_screen` at each
    /// quiet boundary — over each agent's *real* captured transcripts
    /// through a full session (boot→idle → work → ask → answer → done), and
    /// asserts the settled `AgentState` after every phase.
    ///
    /// The single-frame fixture suites (`detect_fixtures.rs`,
    /// `codex_fixtures.rs`) prove each capture classifies correctly in
    /// isolation; this proves they compose into the right *timeline* once
    /// folded through the state machine — the boot gate holding the opening
    /// Working, the settle promotion turning a hookless turn-end into `Done`
    /// (Codex screen-scrape tops out at `Idle`; only the settle reaches
    /// `Done`), and `InputNeeded` surviving the answer flip. A detector
    /// regression on any phase, or a fold regression on any edge, fails here
    /// for whichever agent it breaks.
    #[tokio::test]
    async fn per_agent_golden_lifecycle_walks_the_expected_timeline() {
        use lazybox_ipc::AgentState::{Done, InputNeeded, Working};

        // Deliberate cross-crate reach into the shared real-capture corpus in
        // `lazybox-agents` (the same one the single-frame detector suites
        // assert against, and the same reach the Claude-only sequence test
        // below already makes). The captures live with the detectors they
        // exercise; duplicating the binaries here would just risk the two
        // copies drifting. A rename over there breaks this test at compile
        // time, which is the intended tripwire.
        let lifecycles = [
            GoldenLifecycle {
                agent_id: "claude",
                idle: include_bytes!("../../agents/tests/fixtures/idle_composer.bin"),
                working: include_bytes!("../../agents/tests/fixtures/working_status_line.bin"),
                ask: include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin"),
            },
            GoldenLifecycle {
                agent_id: "codex",
                idle: include_bytes!("../../agents/tests/fixtures/codex_real_idle.bin"),
                working: include_bytes!("../../agents/tests/fixtures/codex_real_working.bin"),
                ask: include_bytes!("../../agents/tests/fixtures/codex_real_command_approval.bin"),
            },
        ];

        for lc in lifecycles {
            let agent = lazybox_agents::registry()
                .get(lc.agent_id)
                .expect("built-in agent");
            let mut p = PumpDriver::with_agent(agent, Duration::ZERO, Duration::ZERO).unbooted();

            // Boot: the composer draws. The ambiguous byte-flow Working is
            // held by the boot gate; the first quiet classification of the
            // resting composer settles the opening Idle.
            p.feed(lc.idle).await;
            p.quiet().await;
            assert_eq!(
                p.state().await,
                Some(lazybox_ipc::AgentState::Idle),
                "{}: a freshly-booted composer is Idle, never Done",
                lc.agent_id,
            );

            // A turn begins: bytes flow.
            p.feed(lc.working).await;
            assert_eq!(
                p.state().await,
                Some(Working),
                "{}: byte flow after boot is Working",
                lc.agent_id,
            );

            // A structural prompt paints and comes to rest → `?`.
            p.feed(lc.ask).await;
            p.quiet().await;
            assert_eq!(
                p.state().await,
                Some(InputNeeded),
                "{}: a settled approval prompt is InputNeeded",
                lc.agent_id,
            );

            // The user answers through lazybox: the optimistic flip commits
            // Working, and the resumed stream keeps it there.
            p.answer().await;
            p.feed(lc.working).await;
            assert_eq!(
                p.state().await,
                Some(Working),
                "{}: answering resumes Working",
                lc.agent_id,
            );

            // The turn ends at a resting composer. A working agent that
            // comes to rest has finished a turn → Done (never back to Idle),
            // even for Codex whose screen-scrape only ever reads Idle here.
            p.feed(lc.idle).await;
            p.quiet().await;
            assert_eq!(
                p.state().await,
                Some(Done),
                "{}: a settled turn-end is Done, not the never-worked Idle",
                lc.agent_id,
            );
        }
    }

    /// #374: clicking a parked `?` and then clicking away must not clear it.
    /// A parked prompt emits no output, so the only chunks that reach it are
    /// the incidental repaints a click / focus / redraw triggers — ambiguous
    /// byte-flow `Working` readings. None of them may clear the `?`, no matter
    /// how many arrive. Only a live classification (a resumed stream at rest)
    /// leaves it. ZERO hysteresis, so this is structural, not a timing window.
    #[tokio::test]
    async fn a_repaint_scrape_never_clears_a_parked_prompt() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");
        let input = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");

        let mut p = PumpDriver::new(Duration::ZERO, Duration::ZERO);
        assert_eq!(p.feed(input).await, vec![Working]); // dialog paints as byte flow → Working
        assert_eq!(p.quiet().await, vec![InputNeeded], "dialog at rest → ?");

        // The user clicks the asking session, then clicks away. Each
        // interaction re-renders the SAME dialog — an incidental byte-flow
        // repaint, no working status line. Every one is a no-op broadcast;
        // the `?` stands, however many arrive.
        for _ in 0..5 {
            assert!(
                p.feed(input).await.is_empty(),
                "a repaint scrape must not re-broadcast a state off a parked `?`",
            );
        }
        assert_eq!(
            p.state().await,
            Some(InputNeeded),
            "navigation never changes whether an agent is asking (#374)",
        );

        // Only a real resume clears it: the stream paints a live status line
        // and comes to rest, classifying (clear) as Working. The byte flow
        // itself is still ambiguous — held until the quiet boundary confirms.
        assert!(
            p.feed(working).await.is_empty(),
            "resumed byte flow is ambiguous — held"
        );
        assert_eq!(
            p.quiet().await,
            vec![Working],
            "a live classification resolves the `?`"
        );
    }

    /// The user's companion ask: a working agent that comes to rest keeps
    /// spinning no longer. A genuinely working agent repaints within the
    /// quiet window, so a quiet one has finished — the quiet classifier
    /// settles it to `Done` even when the last frame still paints a working
    /// status line (a wedged `Working` reading).
    #[tokio::test]
    async fn a_wedged_working_screen_settles_to_done() {
        use lazybox_ipc::AgentState::{Done, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");

        let mut p = PumpDriver::new(Duration::ZERO, Duration::ZERO);
        assert_eq!(p.feed(working).await, vec![Working]);
        // The stream goes quiet but the frame still shows the working status
        // line, so the classifier reads a clear `Working` — a wedged turn.
        assert_eq!(
            p.quiet().await,
            vec![Done],
            "a quiet working agent has finished — settle to Done, don't spin",
        );
    }

    /// #225: an agent whose resting screen classifies as nothing at all (a
    /// pattern-less `GenericCli`, whose detector returns `None`) must still
    /// settle a finished turn to `Done` instead of spinning forever. The
    /// quiet timer surfaces a bare `Done` for an unrecognized screen — but it
    /// only settles a `Working` turn, and never fabricates a finished turn
    /// from a parked `?`.
    #[tokio::test]
    async fn a_quiet_unclassifiable_screen_settles_working_to_done() {
        use lazybox_ipc::AgentState::{Done, InputNeeded, Working};
        // A detector-less agent: `detect_state` returns `None` for every
        // screen (no asking patterns, no status line).
        let agent: std::sync::Arc<dyn lazybox_agents::Agent> =
            std::sync::Arc::new(lazybox_agents::agent::builtins::GenericCli {
                id: "custom".into(),
                display_name: "Custom".into(),
                spawn_cmd: vec!["custom".into()],
                resume_cmd: None,
                asking_patterns: vec![],
            });
        let output = &b"building the widget...\n".repeat(16)[..];

        // A working agent that goes quiet on an unclassifiable screen → Done.
        let mut p = PumpDriver::with_agent(agent.clone(), Duration::ZERO, Duration::ZERO);
        assert_eq!(p.feed(output).await, vec![Working]);
        p.feed(output).await; // still streaming → Working (deduped)
        assert_eq!(
            p.quiet().await,
            vec![Done],
            "a finished turn settles to Done even when the screen doesn't classify",
        );

        // But the same unclassifiable quiet screen must NOT clear a parked
        // `?` — a bare Done carries no evidence the prompt was answered.
        let mut p = PumpDriver::with_agent(agent, Duration::ZERO, Duration::ZERO);
        p.terminals
            .agent_states
            .lock()
            .await
            .insert(p.id, InputNeeded);
        p.feed(output).await;
        assert!(
            p.quiet().await.is_empty(),
            "an unclassifiable quiet screen never clears a parked `?`",
        );
        assert_eq!(p.state().await, Some(InputNeeded));
    }

    /// The #289 headline regression: a session that is visibly streaming
    /// must render the spinner even when a stale prompt marker sits in the
    /// scrollback of the detect window. Pre-fix, the per-chunk classifier
    /// re-detected the marker on every chunk and pinned `?` on a working
    /// agent. The streaming path structurally never classifies — mid-stream
    /// a chunk only ever reads `Working`.
    #[tokio::test]
    async fn streaming_with_stale_prompt_marker_stays_working() {
        use lazybox_ipc::AgentState::Working;
        let input = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        // The prompt render arrives as a chunk: bytes flowing → spinner,
        // NOT `?` — mid-stream the marker can't be trusted as live.
        assert_eq!(p.feed(input).await, vec![Working]);
        // The agent keeps streaming prose with the marker still in the
        // buffer; no `?` may ever surface while output flows.
        for _ in 0..5 {
            assert_eq!(
                p.feed(b"tool output line\n").await,
                Vec::<lazybox_ipc::AgentState>::new(),
                "a streaming session must stay Working, stale marker or not",
            );
        }
    }

    /// Codex approval chrome is strong enough to bypass the quiet timer.
    /// This is the exact command-approval shape from the TUI: the sidebar
    /// must show `?` as soon as the modal paints, even if status repaints
    /// keep the PTY from ever remaining quiet for five seconds.
    #[tokio::test]
    async fn codex_approval_modal_surfaces_input_needed_immediately() {
        use lazybox_ipc::AgentState::InputNeeded;
        let agent = lazybox_agents::registry()
            .get("codex")
            .expect("codex agent is a built-in");
        let mut p = PumpDriver::with_agent(agent, Duration::ZERO, Duration::ZERO);
        let modal = "Would you like to run the following command?\n\
                     Environment: local\n\
                     › 1. Yes, proceed (y)\n\
                       2. Yes, and don't ask again\n\
                       3. No, and tell Codex what to do differently (esc)\n\
                     Press enter to confirm or esc to cancel";

        assert_eq!(p.feed(modal.as_bytes()).await, vec![InputNeeded]);
        assert_eq!(p.state().await, Some(InputNeeded));
        assert_eq!(
            p.terminals.input_needed_shapes.lock().await.get(&p.id),
            Some(&lazybox_agents::PromptShape::Chooser),
        );
    }

    /// The #399 acceptance flow, replayed over raw bytes captured from a
    /// live codex 0.144.6 approval round-trip: the sidebar `?` must surface
    /// on the modal's paint burst itself (the parked modal churns spinner
    /// repaints, so the quiet classifier never gets a turn), hold through
    /// that churn, and clear once the user answers through lazybox —
    /// never re-surfacing off the aftermath.
    #[tokio::test]
    async fn codex_real_capture_approval_flow_surfaces_and_clears() {
        use lazybox_ipc::AgentState::InputNeeded;
        let working = include_bytes!("../../agents/tests/fixtures/codex_real_working.bin");
        let paint =
            include_bytes!("../../agents/tests/fixtures/codex_real_approval_paint_burst.bin");
        let ticks =
            include_bytes!("../../agents/tests/fixtures/codex_real_approval_parked_ticks.bin");
        let answered =
            include_bytes!("../../agents/tests/fixtures/codex_real_approval_answered.bin");
        let settled =
            include_bytes!("../../agents/tests/fixtures/codex_real_approval_settled_idle.bin");
        let agent = lazybox_agents::registry()
            .get("codex")
            .expect("codex agent is a built-in");
        let mut p = PumpDriver::with_agent(agent, Duration::ZERO, Duration::ZERO);

        p.feed(working).await;
        // The paint burst flips the pill on the chunk itself — the "within
        // ~1s" acceptance is this line needing no quiet() first.
        assert_eq!(p.feed(paint).await, vec![InputNeeded]);
        assert_eq!(
            p.terminals.input_needed_shapes.lock().await.get(&p.id),
            Some(&lazybox_agents::PromptShape::Chooser),
        );
        // The parked modal keeps repainting its spinner ticks; none of that
        // churn may flap the `?`, and a quiet window that does sneak in
        // re-reads the same modal as a no-op.
        for chunk in ticks.chunks(1024) {
            assert!(p.feed(chunk).await.is_empty());
        }
        assert!(p.quiet().await.is_empty());
        assert_eq!(p.state().await, Some(InputNeeded));

        // The user answers through lazybox; from here `?` must never
        // return, even though the answered modal is still in the raw
        // stream's scrollback.
        p.answer().await;
        let mut after = Vec::new();
        for chunk in answered.chunks(2048) {
            after.extend(p.feed(chunk).await);
        }
        after.extend(p.quiet().await);
        after.extend(p.feed(settled).await);
        after.extend(p.quiet().await);
        assert!(
            !after.contains(&InputNeeded),
            "an answered approval must not re-surface `?`; got {after:?}",
        );
        assert_ne!(p.state().await, Some(InputNeeded));
    }

    /// The negative case from #399: the user answers the modal *inside the
    /// terminal* before the optimistic flip lands (no buffer reset), so the
    /// answered modal lingers verbatim in the detection window's scrollback
    /// while the aftermath streams over it. The stale-marker guard must keep
    /// the chunk path silent, and the quiet classification must read the
    /// post-answer screen, not the lingering modal.
    #[tokio::test]
    async fn answered_codex_modal_in_scrollback_never_resurfaces() {
        use lazybox_ipc::AgentState::InputNeeded;
        let working = include_bytes!("../../agents/tests/fixtures/codex_real_working.bin");
        let paint =
            include_bytes!("../../agents/tests/fixtures/codex_real_approval_paint_burst.bin");
        let answered =
            include_bytes!("../../agents/tests/fixtures/codex_real_approval_answered.bin");
        let settled =
            include_bytes!("../../agents/tests/fixtures/codex_real_approval_settled_idle.bin");
        let agent = lazybox_agents::registry()
            .get("codex")
            .expect("codex agent is a built-in");
        let mut p = PumpDriver::with_agent(agent, Duration::ZERO, Duration::ZERO);

        p.feed(working).await;
        assert_eq!(p.feed(paint).await, vec![InputNeeded]);

        // No answer() — the aftermath just streams in over the parked `?`.
        let mut after = Vec::new();
        for chunk in answered.chunks(2048) {
            after.extend(p.feed(chunk).await);
        }
        after.extend(p.quiet().await);
        after.extend(p.feed(settled).await);
        after.extend(p.quiet().await);
        assert!(
            !after.contains(&InputNeeded),
            "a lingering answered modal must not re-fire `?`; got {after:?}",
        );
        assert_ne!(
            p.state().await,
            Some(InputNeeded),
            "the settled post-answer screen must have cleared the `?`",
        );
    }

    struct FreeTextPromptAgent;

    impl lazybox_agents::Agent for FreeTextPromptAgent {
        fn id(&self) -> &'static str {
            "free-text-test"
        }

        fn display_name(&self) -> &'static str {
            "Free-text test agent"
        }

        fn spawn(&self, _ctx: &lazybox_agents::SpawnCtx) -> Vec<String> {
            vec!["free-text-test".into()]
        }

        fn detect_observation_chunked(
            &self,
            _recent_output: &[u8],
            _last_chunk_start: usize,
        ) -> Option<lazybox_agents::AgentObservation> {
            Some(lazybox_agents::AgentObservation::input_needed(
                lazybox_agents::PromptShape::FreeText,
            ))
        }
    }

    /// Prompt shape belongs to the adapter observation; the daemon must
    /// preserve it instead of collapsing every quiet prompt to a chooser.
    #[tokio::test]
    async fn quiet_classifier_preserves_agent_declared_prompt_shape() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};
        let agent: std::sync::Arc<dyn lazybox_agents::Agent> =
            std::sync::Arc::new(FreeTextPromptAgent);
        let mut p = PumpDriver::with_agent(agent, Duration::ZERO, Duration::ZERO);

        assert_eq!(p.feed(b"Please explain:").await, vec![Working]);
        assert_eq!(p.quiet().await, vec![InputNeeded]);
        assert_eq!(
            p.terminals.input_needed_shapes.lock().await.get(&p.id),
            Some(&lazybox_agents::PromptShape::FreeText),
        );
    }

    /// The counterpart: once the PTY has been quiet for the classify
    /// window, a permission prompt at rest MUST surface as `?`.
    #[tokio::test]
    async fn quiet_at_a_permission_prompt_classifies_input_needed() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};
        let input = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(input).await, vec![Working]);
        assert_eq!(
            p.quiet().await,
            vec![InputNeeded],
            "a dialog at rest past the quiet window must raise `?`",
        );
    }

    #[tokio::test]
    async fn resize_redraw_keeps_an_idle_agent_idle() {
        use lazybox_ipc::AgentState::Idle;
        let idle = include_bytes!("../../agents/tests/fixtures/idle_composer.bin");
        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        p.terminals.agent_states.lock().await.insert(p.id, Idle);
        p.view_action().await;

        assert_eq!(p.feed(idle).await, Vec::new());
        assert_eq!(p.quiet().await, Vec::new());
        assert_eq!(p.state().await, Some(Idle));
        assert!(
            p.terminals
                .agent_terminal_activities
                .lock()
                .await
                .get(&p.id)
                .is_none()
        );
    }

    #[tokio::test]
    async fn resize_redraw_still_surfaces_an_authoritative_prompt() {
        use lazybox_ipc::AgentState::{Idle, InputNeeded};
        let prompt = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");
        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        p.terminals.agent_states.lock().await.insert(p.id, Idle);
        p.view_action().await;

        assert_eq!(p.feed(prompt).await, Vec::new());
        assert_eq!(p.quiet().await, vec![InputNeeded]);
        assert_eq!(p.state().await, Some(InputNeeded));
    }

    #[tokio::test]
    async fn multi_chunk_resize_redraw_keeps_a_done_agent_done() {
        use lazybox_ipc::AgentState::Done;
        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        p.terminals.agent_states.lock().await.insert(p.id, Done);
        p.view_action().await;

        for chunk in [b"first frame".as_slice(), b"second frame", b"third frame"] {
            assert_eq!(p.feed(chunk).await, Vec::new());
        }
        assert_eq!(p.quiet().await, Vec::new());
        assert_eq!(p.state().await, Some(Done));
    }

    #[tokio::test]
    async fn output_queued_before_a_view_action_still_starts_working() {
        use lazybox_ipc::AgentState::{Idle, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");
        let mut p = PumpDriver::new(Duration::ZERO, Duration::ZERO);
        p.terminals.agent_states.lock().await.insert(p.id, Idle);

        p.view_action_after(1).await;

        assert_eq!(p.feed_at(1, working).await, vec![Working]);
        assert_eq!(p.state().await, Some(Working));
    }

    #[tokio::test]
    async fn older_quiet_timer_does_not_consume_a_new_view_epoch() {
        use lazybox_ipc::AgentState::Idle;
        let idle = include_bytes!("../../agents/tests/fixtures/idle_composer.bin");
        let mut p = PumpDriver::new(Duration::ZERO, Duration::ZERO);
        p.terminals.agent_states.lock().await.insert(p.id, Idle);
        p.buf.extend_from_slice(idle);
        p.last_chunk_len = idle.len();
        p.view_action_after(0).await;

        assert!(p.quiet().await.is_empty());
        assert!(p.feed_at(1, idle).await.is_empty());
        assert!(p.quiet().await.is_empty());
        assert_eq!(p.state().await, Some(Idle));
    }

    #[tokio::test(start_paused = true)]
    async fn view_epoch_without_output_does_not_hide_later_agent_work() {
        use lazybox_ipc::AgentState::{Idle, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");
        let mut p = PumpDriver::new(Duration::ZERO, Duration::ZERO);
        p.terminals.agent_states.lock().await.insert(p.id, Idle);
        p.view_action_after(0).await;
        tokio::time::advance(Duration::from_secs(3)).await;

        assert_eq!(p.feed_at(1, working).await, vec![Working]);
        assert_eq!(p.state().await, Some(Working));
    }

    #[tokio::test]
    async fn view_redraw_cannot_settle_a_working_agent_to_done() {
        use lazybox_ipc::AgentState::Working;
        let idle = include_bytes!("../../agents/tests/fixtures/idle_composer.bin");
        let mut p = PumpDriver::new(Duration::ZERO, Duration::ZERO);
        p.terminals.agent_states.lock().await.insert(p.id, Working);
        p.view_action().await;

        assert!(p.feed(idle).await.is_empty());
        assert!(p.quiet().await.is_empty());
        assert_eq!(p.state().await, Some(Working));
    }

    #[tokio::test]
    async fn view_redraw_cannot_clear_input_needed() {
        use lazybox_ipc::AgentState::InputNeeded;
        let idle = include_bytes!("../../agents/tests/fixtures/idle_composer.bin");
        let mut p = PumpDriver::new(Duration::ZERO, Duration::ZERO);
        p.terminals
            .agent_states
            .lock()
            .await
            .insert(p.id, InputNeeded);
        p.view_action().await;

        assert!(p.feed(idle).await.is_empty());
        assert!(p.quiet().await.is_empty());
        assert_eq!(p.state().await, Some(InputNeeded));
    }

    /// Quiet after a `Stop` hook: the resting composer classifies Idle,
    /// but `Done` is sticky against Idle — the "finished, take a look"
    /// alert must survive both the trailing paint chunks (hooks are fresh,
    /// so the byte-flow Working reading is gated) and the quiet
    /// classification.
    #[tokio::test]
    async fn quiet_after_stop_hook_keeps_done() {
        use lazybox_ipc::AgentState::Done;
        let idle = include_bytes!("../../agents/tests/fixtures/idle_composer.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        // A Stop hook just landed: state is Done, hooks are fresh.
        p.terminals.agent_states.lock().await.insert(p.id, Done);
        p.terminals
            .hook_driven_terminals
            .lock()
            .await
            .insert(p.id, std::time::Instant::now());
        // Claude paints its resting composer after the hook fired.
        assert_eq!(
            p.feed(idle).await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "trailing paint must not demote Done to Working",
        );
        assert_eq!(
            p.quiet().await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "the quiet Idle classification must not clear Done",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
    }

    /// The stale-hook variant of Done stickiness: 30+ seconds after the
    /// `Stop` hook, the hooks-primary gate no longer filters PTY readings
    /// — but a stray repaint (a pane resize, a reattach redraw) is still
    /// just a byte-flow `Working`, and an ambiguous Working may never
    /// clear `Done`. Pre-guard, the repaint committed Done → Working and
    /// the next quiet classification landed Idle: the "finished, take a
    /// look" marker silently wiped by resizing the window.
    #[tokio::test]
    async fn stray_repaint_after_stale_hooks_keeps_done() {
        use lazybox_ipc::AgentState::Done;
        let idle = include_bytes!("../../agents/tests/fixtures/idle_composer.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        // Done was set by a Stop hook long ago; the hook stream has been
        // silent since (a finished agent fires no more hooks).
        p.terminals.agent_states.lock().await.insert(p.id, Done);
        p.terminals
            .hook_driven_terminals
            .lock()
            .await
            .insert(p.id, std::time::Instant::now() - Duration::from_secs(31));
        // The resize repaints the resting composer as one chunk.
        assert_eq!(
            p.feed(idle).await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "a stray repaint must not demote Done to Working",
        );
        assert_eq!(
            p.quiet().await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "the follow-up quiet Idle must stay rejected by Done stickiness",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
    }

    /// The quiet timer racing the optimistic answer flip: `handle_write`
    /// flipped the `?` to Working and marked the detect buffer for reset,
    /// but the clear only lands on the next chunk. A quiet firing while the
    /// reset is still latched must NOT classify the stale dialog (that
    /// re-raised the just-answered `?`) — yet firing at all means the answer
    /// produced zero output for a full quiet window, so it settles the turn
    /// `Done` instead of peeking and returning. A bare return left `Working`
    /// pinned: the quiet timer disarms on fire and only a chunk re-arms it,
    /// and zero output means no chunk comes (the watchdog was the sole
    /// escape, and none exists when `working_watchdog_secs = 0`).
    #[tokio::test]
    async fn quiet_at_a_latched_answer_reset_settles_done() {
        use lazybox_ipc::AgentState::{Done, InputNeeded, Working};
        let input = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(input).await, vec![Working]);
        assert_eq!(p.quiet().await, vec![InputNeeded]);
        // The user answers: the flip commits Working and marks the reset.
        p.terminals.agent_states.lock().await.insert(p.id, Working);
        p.terminals.agent_detect_resets.lock().await.insert(p.id);
        assert_eq!(
            p.quiet().await,
            vec![Done],
            "a latched reset at the quiet timer settles Done, not the stale `?`",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
        assert!(
            p.terminals.agent_detect_resets.lock().await.contains(&p.id),
            "the reset stays latched — a late chunk still clears the buffer via the chunk arm",
        );
    }

    /// The quiet-path settle is the fail-safe for a *dead* zero-output
    /// answer, not a live one. An answered agent genuinely still at work
    /// (a silent tool call fires no bytes) keeps a *fresh* lifecycle hook,
    /// and the settle folds through the hooks-primary gate — so it is
    /// suppressed and the terminal stays `Working`, exactly as the watchdog
    /// path yields to a fresh hook.
    #[tokio::test]
    async fn quiet_zero_output_settle_still_yields_to_a_fresh_hook() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};
        let input = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(input).await, vec![Working]);
        assert_eq!(p.quiet().await, vec![InputNeeded]);
        // Answered, no chunk cleared the reset — but a hook landed just now,
        // so the agent is provably still at work.
        p.terminals.agent_states.lock().await.insert(p.id, Working);
        p.terminals.agent_detect_resets.lock().await.insert(p.id);
        p.hook_now().await;
        assert_eq!(
            p.quiet().await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "a fresh hook must gate the settle — the turn is still live",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Working)
        );
        assert!(
            p.terminals.agent_detect_resets.lock().await.contains(&p.id),
            "the reset stays latched: the next chunk, not this gated tick, clears it",
        );
    }

    /// A brief repaint burst (a pane resize) at a parked prompt must not
    /// flap the `?` off: the byte-flow Working reading is ambiguous, so it
    /// is held against `InputNeeded` unconditionally (#374), and the next
    /// quiet classification re-reads the same dialog.
    #[tokio::test]
    async fn repaint_burst_at_a_parked_prompt_is_damped() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};
        let input = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(input).await, vec![Working]);
        assert_eq!(p.quiet().await, vec![InputNeeded]);
        // The repaint re-delivers the same screen as one chunk.
        assert_eq!(
            p.feed(input).await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "a repaint must not clear the `?`",
        );
        assert_eq!(
            p.quiet().await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "re-classifying the same dialog is a no-op",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&InputNeeded)
        );
    }

    /// The #398 acceptance scenario: an agent that stops doing work but
    /// keeps animating a spinner never goes byte-quiet, so the quiet
    /// timer never fires — yet the content fingerprint stops changing,
    /// so the watchdog fires and forces the pinned `Working` closed.
    #[tokio::test]
    async fn spinner_pinned_working_is_forced_out_by_the_watchdog() {
        use lazybox_ipc::AgentState::{Done, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(working).await, vec![Working]);
        // The agent stalls but keeps repainting its spinner: glyph and
        // counters change, the letter stream doesn't. Byte flow re-arms
        // the quiet timer every frame; the watchdog anchor must not move.
        let mut fp = None;
        watchdog_notes_progress(&mut fp, working);
        for tick in 0..4u32 {
            let frame =
                format!("\r\x1b[2K✻ Gusting… ({tick}s · ↓ 7.{tick}k tokens · esc to interrupt)");
            assert_eq!(
                p.feed(frame.as_bytes()).await,
                Vec::<lazybox_ipc::AgentState>::new(),
                "spinner frames are plain byte flow — still Working",
            );
            if tick > 0 {
                assert!(
                    !watchdog_notes_progress(&mut fp, frame.as_bytes()),
                    "a spinner-only repaint must not reset the watchdog",
                );
            } else {
                watchdog_notes_progress(&mut fp, frame.as_bytes());
            }
        }
        // The watchdog window elapses with no meaningful change: the
        // screen still *reads* Working (live-looking status line), so
        // the classification can't help — the force closes the turn.
        assert_eq!(
            p.watchdog().await,
            vec![Done],
            "the watchdog must force a spinner-pinned Working out",
        );
    }

    /// The counterpart to the force: once `Done` (watchdog-forced or
    /// settled), resumed REAL output must re-open `Working` without
    /// waiting for a quiet pause — a heavy stream never offers one.
    /// Chunks that keep changing the content fingerprint are the resume
    /// event ([`lazybox_agents::state_machine`]'s progress streak);
    /// churn alone keeps `Done` pinned forever.
    #[tokio::test]
    async fn resumed_real_output_reopens_working_after_forced_done() {
        use lazybox_ipc::AgentState::{Done, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(working).await, vec![Working]);
        assert_eq!(p.watchdog().await, vec![Done]);
        // Spinner churn keeps arriving: identical letter stream, no
        // progress streak — Done must hold.
        for _ in 0..5 {
            assert_eq!(
                p.feed("\r\x1b[2K✻ Gusting… (2s · esc to interrupt)".as_bytes())
                    .await,
                Vec::<lazybox_ipc::AgentState>::new(),
                "churn alone must never clear Done",
            );
        }
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
        // The agent genuinely resumes: every chunk carries new content.
        let mut seq = Vec::new();
        seq.extend(p.feed(b"Reading crates/server/src/lib.rs\n").await);
        seq.extend(p.feed(b"Editing spawn_handler.rs\n").await);
        seq.extend(p.feed(b"Running cargo check\n").await);
        assert_eq!(
            seq,
            vec![Working],
            "sustained new content must re-open Working mid-stream",
        );
    }

    /// A parked prompt hidden behind spinner churn: the repaint keeps
    /// re-arming the quiet timer so `classify_quiet_screen` never runs
    /// on its own — the watchdog tick classifies regardless of byte
    /// flow and the dialog surfaces as `?` (no false `Done`).
    #[tokio::test]
    async fn parked_prompt_behind_spinner_churn_surfaces_via_watchdog() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};
        let input = include_bytes!("../../agents/tests/fixtures/permission_prompt_fragmented.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(input).await, vec![Working]);
        let mut fp = None;
        watchdog_notes_progress(&mut fp, input);
        for _ in 0..4 {
            // A one-cell spinner repaint: letterless, so it has no
            // fingerprint at all and can never count as progress.
            let frame = "\x1b[1;1H⠋".as_bytes();
            assert_eq!(p.feed(frame).await, Vec::<lazybox_ipc::AgentState>::new());
            assert!(!watchdog_notes_progress(&mut fp, frame));
        }
        assert_eq!(
            p.watchdog().await,
            vec![InputNeeded],
            "the watchdog tick must surface the parked dialog, not claim Done",
        );
    }

    /// The permission dialog and the resting composer that replaces it — the
    /// #872 shape: a real prompt parks the agent at `?`, the turn ends, and
    /// the composer is redrawn at rest with the prompt now scrollback.
    const PERMISSION_DIALOG: &str = concat!(
        "Do you want to proceed?\n",
        "❯ 1. Yes\n",
        "  2. No\n",
        "Esc to cancel · Tab to amend · ctrl+e to explain",
    );
    const SETTLED_COMPOSER: &str = concat!(
        "Final state: all checks pass — the PR is CLEAN.\n",
        "\n",
        "❯ \n",
        "? for shortcuts",
    );

    /// #872: a finished agent that leaves a background shell running keeps
    /// the PTY emitting bytes, so the byte-silence quiet timer never fires
    /// and a stale `?` (a prompt the turn has since moved past) would pin
    /// forever. The content-stability watchdog re-classifies the settled
    /// screen — the prompt is gone — and the `?` resolves instead of
    /// sticking.
    #[tokio::test]
    async fn watchdog_clears_a_stale_input_needed_when_the_turn_has_settled() {
        use lazybox_ipc::AgentState::{Idle, InputNeeded, Working};

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(PERMISSION_DIALOG.as_bytes()).await, vec![Working]);
        assert_eq!(p.quiet().await, vec![InputNeeded]);

        // The turn ends: the composer is redrawn at rest above the now-stale
        // prompt. As an ambiguous byte-flow reading this must NOT clear the
        // `?` on its own (#374) — a stray repaint reads exactly the same.
        assert_eq!(
            p.feed(SETTLED_COMPOSER.as_bytes()).await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "an ambiguous reading must not un-ask a parked prompt",
        );

        // The watchdog fires on content-stability (a background shell's
        // heartbeat keeps the stream alive, so the quiet timer can't) and
        // re-classifies: the prompt is scrollback now, so the `?` clears.
        assert_eq!(
            p.watchdog().await,
            vec![Idle],
            "a settled resting composer must resolve a stale InputNeeded",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Idle),
        );
    }

    /// The guard the #872 clear must keep: a prompt that is STILL on screen
    /// when the watchdog fires re-reads `InputNeeded`, so the `?` stays. The
    /// re-classification is affirmative evidence, not a blind timer, so it
    /// never un-asks a live prompt.
    #[tokio::test]
    async fn watchdog_leaves_a_live_parked_prompt_asking() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(PERMISSION_DIALOG.as_bytes()).await, vec![Working]);
        assert_eq!(p.quiet().await, vec![InputNeeded]);
        assert_eq!(
            p.watchdog().await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "a live prompt must survive the watchdog re-classification",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&InputNeeded),
        );
    }

    /// #62: while a lifecycle hook is fresh it owns the asking call, so the
    /// watchdog's re-classification of a resting-looking screen must NOT
    /// clear the `?` (the idle nudge raises it precisely at a ready
    /// composer). Only a stale-hook / hookless `?` clears here — the
    /// broken-hook case #872 is actually about.
    #[tokio::test]
    async fn watchdog_defers_to_a_fresh_hook_before_clearing_input_needed() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(PERMISSION_DIALOG.as_bytes()).await, vec![Working]);
        assert_eq!(p.quiet().await, vec![InputNeeded]);
        p.hook_now().await;
        p.feed(SETTLED_COMPOSER.as_bytes()).await;
        assert_eq!(
            p.watchdog().await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "a fresh hook keeps the `?` even when the screen looks resting",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&InputNeeded),
        );
    }

    /// The force-`Done` tail exists only for a stuck `Working` turn. A
    /// pending answer reset latched while the terminal reads `InputNeeded`
    /// (a hook/answer race) makes the watchdog skip the stale-buffer
    /// re-classify — so the tail must NOT then force the turn closed and
    /// must leave the `?` untouched (a `Done` from `InputNeeded` is rejected,
    /// so the guard is what stops the spurious "forcing the turn closed").
    #[tokio::test]
    async fn watchdog_does_not_force_close_an_input_needed_with_a_latched_reset() {
        use lazybox_ipc::AgentState::{InputNeeded, Working};

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(PERMISSION_DIALOG.as_bytes()).await, vec![Working]);
        assert_eq!(p.quiet().await, vec![InputNeeded]);
        p.terminals.agent_detect_resets.lock().await.insert(p.id);
        assert_eq!(
            p.watchdog().await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "a latched reset must not force an InputNeeded turn to Done",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&InputNeeded),
        );
    }

    /// The content-stability watchdog is the configured upper bound for a
    /// Working terminal even when its last lifecycle hook is still fresh.
    #[tokio::test]
    async fn watchdog_force_overrides_a_fresh_working_hook() {
        use lazybox_ipc::AgentState::{Done, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(working).await, vec![Working]);
        p.hook_now().await;
        assert_eq!(
            p.watchdog().await,
            vec![Done],
            "a fresh hook must not extend the configured watchdog bound",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
    }

    /// The fix (#504). The quiet timer measures true byte-silence, and a
    /// busy agent repaints its ticker within that window — so a byte-silent
    /// screen is authoritative that the turn ended. Like the watchdog's
    /// content-stability bound, the quiet classification settles a
    /// `Working` agent to `Done` even while a hook is still fresh. This is
    /// what stops a hook-driven agent whose `Stop` hook never fires (a
    /// manual interrupt, a lost hook) from pinning `Working`.
    #[tokio::test]
    async fn quiet_settles_working_to_done_despite_a_fresh_hook() {
        use lazybox_ipc::AgentState::{Done, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(working).await, vec![Working]);
        // A hook fired one instant ago.
        p.terminals
            .hook_driven_terminals
            .lock()
            .await
            .insert(p.id, std::time::Instant::now());
        // But the PTY has gone byte-silent: the quiet timer settles it.
        assert_eq!(
            p.quiet().await,
            vec![Done],
            "byte-silence must settle Working → Done without waiting for hook staleness",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
    }

    /// A pending answer reset still latched a full watchdog window after
    /// the answer means the optimistic `Working` flip saw zero PTY output —
    /// the answer started no work, and nothing will arrive to clear the
    /// reset or settle the turn. The watchdog must force it closed rather
    /// than pin `Working` forever, WITHOUT classifying the stale buffer
    /// (which would re-raise the just-answered `?`). Out of `Working` the
    /// tick stays a no-op.
    #[tokio::test]
    async fn watchdog_settles_a_zero_output_answer_instead_of_pinning_working() {
        use lazybox_ipc::AgentState::{Done, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(working).await, vec![Working]);
        // The user answered, but no chunk has arrived to clear the reset.
        p.terminals.agent_detect_resets.lock().await.insert(p.id);
        assert_eq!(
            p.watchdog().await,
            vec![Done],
            "a zero-output answer must settle to Done, not pin Working",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
        // The reset is left latched so a late chunk still clears the buffer
        // via the pump's chunk arm — but the terminal is `Done` now, so a
        // further watchdog tick is a plain no-op.
        assert!(p.terminals.agent_detect_resets.lock().await.contains(&p.id));
        assert_eq!(
            p.watchdog().await,
            Vec::<lazybox_ipc::AgentState>::new(),
            "out of Working the tick is a no-op",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
    }

    /// A fresh hook protects an ambiguous post-answer screen from the short
    /// quiet timer, but it cannot extend the configured watchdog upper bound.
    #[tokio::test]
    async fn watchdog_zero_output_settle_obeys_the_bound_despite_a_fresh_hook() {
        use lazybox_ipc::AgentState::{Done, Working};
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");

        let mut p = PumpDriver::new(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(p.feed(working).await, vec![Working]);
        // The user answered, no chunk cleared the reset, and the terminal
        // remained content-stable for the full watchdog window.
        p.terminals.agent_detect_resets.lock().await.insert(p.id);
        p.hook_now().await;
        assert_eq!(
            p.watchdog().await,
            vec![Done],
            "a fresh hook must not extend the configured watchdog bound",
        );
        assert_eq!(
            p.terminals.agent_states.lock().await.get(&p.id),
            Some(&Done)
        );
        // The reset stays latched: the buffer still predates the answer,
        // so the next chunk is still what clears it.
        assert!(p.terminals.agent_detect_resets.lock().await.contains(&p.id));
    }

    #[test]
    fn watchdog_progress_ignores_churn() {
        let mut fp = None;
        assert!(watchdog_notes_progress(&mut fp, b"Compiling lazybox v0.1"));
        // The same letters again — a repaint — are not progress.
        assert!(!watchdog_notes_progress(&mut fp, b"Compiling lazybox v0.1"));
        // Letterless churn never is, and must not clobber the anchor
        // fingerprint (the next identical repaint still dedupes).
        assert!(!watchdog_notes_progress(&mut fp, b"\x1b[2K"));
        assert!(!watchdog_notes_progress(&mut fp, b"12:03"));
        assert!(!watchdog_notes_progress(&mut fp, b"Compiling lazybox v0.1"));
        assert!(watchdog_notes_progress(&mut fp, b"Finished dev profile"));
    }

    #[test]
    fn watchdog_window_reads_config() {
        let mut cfg = lazybox_config::Config::default();
        assert_eq!(working_watchdog_after(&cfg), Some(WORKING_WATCHDOG_AFTER));
        cfg.agent.working_watchdog_secs = Some(30);
        assert_eq!(working_watchdog_after(&cfg), Some(Duration::from_secs(30)));
        cfg.agent.working_watchdog_secs = Some(0);
        assert_eq!(
            working_watchdog_after(&cfg),
            None,
            "0 disables the watchdog"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_rechecks_preserve_the_full_content_stability_age() {
        let mut watchdog = WorkingWatchdog::new(Some(Duration::from_secs(15)));

        tokio::time::advance(Duration::from_secs(15)).await;
        let first_at = tokio::time::Instant::now();
        assert!(watchdog.prepare_select(first_at, 0));
        let (window, first_stable) = watchdog.fire(first_at).unwrap();
        assert_eq!(first_stable, window);

        tokio::time::advance(Duration::from_secs(15)).await;
        let second_at = tokio::time::Instant::now();
        assert!(watchdog.prepare_select(second_at, 0));
        let (window, second_stable) = watchdog.fire(second_at).unwrap();
        assert_eq!(
            second_stable,
            window.saturating_mul(2),
            "a no-op watchdog check must not erase the no-progress invariant age",
        );
    }

    #[test]
    fn quiet_classify_window_reads_config() {
        let mut cfg = lazybox_config::Config::default();
        assert_eq!(pty_quiet_classify_after(&cfg), PTY_QUIET_CLASSIFY_AFTER);
        cfg.agent.quiet_classify_secs = Some(45);
        assert_eq!(
            pty_quiet_classify_after(&cfg),
            Duration::from_secs(45),
            "a positive override sets the quiet window",
        );
        cfg.agent.quiet_classify_secs = Some(0);
        assert_eq!(
            pty_quiet_classify_after(&cfg),
            PTY_QUIET_CLASSIFY_AFTER,
            "0 falls back to the default rather than disabling the timer",
        );
    }

    fn hook_event(kind: lazybox_ipc::HookEventKind) -> lazybox_ipc::HookEvent {
        lazybox_ipc::HookEvent {
            kind,
            session_id: None,
            cwd: None,
            tool_name: None,
            notification: None,
        }
    }

    fn recv_state_for(
        rx: &mut tokio::sync::broadcast::Receiver<Event>,
        id: TerminalId,
    ) -> Option<(SessionKey, lazybox_ipc::AgentState)> {
        while let Ok(ev) = rx.try_recv() {
            if let Event::AgentState {
                session_key,
                terminal_id,
                state,
            } = ev
                && terminal_id == id
            {
                return Some((session_key, state));
            }
        }
        None
    }

    /// The cross-emitter version of the #161 regression: after a terminal
    /// is rebadged onto a PR mid-flight, ALL THREE emitters — the PTY pump,
    /// the optimistic flip in `handle_write`, and hook ingest — must
    /// broadcast under the NEW (PR) key, not the issue key captured at
    /// spawn. The unified state owner routes all three through the same
    /// atomic cache+broadcast boundary, so this is the invariant to pin.
    #[tokio::test]
    async fn all_three_emitters_broadcast_under_the_rebadged_key() {
        use lazybox_ipc::AgentState;
        let id = TerminalId(7);
        let issue_key: SessionKey = "github-o-r-161".into(); // captured at spawn
        let pr_key: SessionKey = "github-o-r-164".into(); // rebadge target

        let (config, mock) = ServerConfig::in_memory_with_mock();
        let backend_key = mock
            .spawn(&[], None, &[], "rebadged")
            .await
            .expect("spawn live backend session");
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(id, backend_key.clone());
        // The workspace-move owner moved the live meta entry onto the PR.
        config.terminal.terminal_meta.lock().await.insert(
            id,
            (
                pr_key.clone(),
                lazybox_ipc::TerminalKind::Agent("claude".into()),
            ),
        );
        config
            .terminal
            .agent_state_generations
            .lock()
            .await
            .insert(id, id.0);
        let durability = agent_state_durability(&config, id, &backend_key)
            .await
            .expect("state durability");

        // (a) PTY transition: the pump captured the issue key at spawn, but
        // the live meta entry now points at the PR.
        let mut rx = config.bus.subscribe();
        let agent = lazybox_agents::registry().get("claude").unwrap();
        let working = include_bytes!("../../agents/tests/fixtures/working_status_line.bin");
        let mut buf = Vec::new();
        let mut state_machine = {
            let mut m = lazybox_agents::AgentStateMachine::new();
            m.mark_booted();
            m
        };
        note_pty_activity(
            Some(&agent),
            &mut buf,
            working,
            1,
            false,
            &config.terminal,
            &config.bus,
            Some(&durability),
            id,
            &issue_key,
            &mut state_machine,
        )
        .await;
        assert_eq!(
            recv_state_for(&mut rx, id),
            Some((pr_key.clone(), AgentState::Working)),
            "PTY emitter must broadcast under the rebadged PR key",
        );

        // (b) optimistic flip via handle_write — prereq: parked on a prompt.
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, AgentState::InputNeeded);
        let mut rx = config.bus.subscribe();
        handle_write(&config, id, b"\r", TerminalInputIntent::Submit).await;
        assert_eq!(
            recv_state_for(&mut rx, id),
            Some((pr_key.clone(), AgentState::Working)),
            "optimistic flip must broadcast under the rebadged PR key",
        );

        // (c) hook ingest via handle_ingest_hook — PreToolUse maps to Working.
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(id, AgentState::Idle);
        let mut rx = config.bus.subscribe();
        handle_ingest_hook(
            &config,
            id,
            Some(backend_key),
            hook_event(lazybox_ipc::HookEventKind::PreToolUse),
        )
        .await;
        assert_eq!(
            recv_state_for(&mut rx, id),
            Some((pr_key.clone(), AgentState::Working)),
            "hook emitter must broadcast under the rebadged PR key",
        );
    }

    /// #779: `detect_and_broadcast_model` scrapes the live model off the
    /// PTY window, caches it in `terminal_models`, and broadcasts under the
    /// terminal's *live* session (resolved from `terminal_meta`, not the
    /// captured spawn key) so a rebadged terminal reports on the PR.
    #[tokio::test]
    async fn detect_and_broadcast_model_emits_under_the_live_session() {
        let id = TerminalId(779);
        let captured: SessionKey = "github-o-r-778".into(); // captured at spawn
        let live: SessionKey = "github-o-r-779".into(); // where rebadge moved it
        let footer = include_bytes!("../../agents/tests/fixtures/codex_real_idle.bin");
        let terminals = TerminalRegistry::default();
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let agent = lazybox_agents::registry()
            .get("codex")
            .expect("codex agent");
        terminals.terminal_meta.lock().await.insert(
            id,
            (
                live.clone(),
                lazybox_ipc::TerminalKind::Agent("codex".into()),
            ),
        );

        detect_and_broadcast_model(&agent, footer, &terminals, &bus, id, &captured).await;

        let Event::TerminalModelChanged {
            session_key,
            terminal_id,
            model_label,
        } = rx.recv().await.expect("model event")
        else {
            panic!("expected TerminalModelChanged")
        };
        assert_eq!(terminal_id, id);
        assert_eq!(model_label, "gpt-5.5 · xhigh");
        assert_eq!(
            session_key, live,
            "the live model must broadcast under the rebadged session, not the spawn key",
        );
        assert_eq!(
            terminals.terminal_models.lock().await.get(&id),
            Some(&"gpt-5.5 · xhigh".to_string()),
            "the reading must be cached so it folds into the reconnect snapshot",
        );
    }

    /// #779: a resting composer re-reads the same footer every settle —
    /// re-broadcasting each time would spam the bus, so an unchanged model
    /// is a silent no-op.
    #[tokio::test]
    async fn detect_and_broadcast_model_is_a_no_op_when_unchanged() {
        let id = TerminalId(780);
        let key: SessionKey = "github-o-r-780".into();
        let footer = include_bytes!("../../agents/tests/fixtures/codex_real_idle.bin");
        let terminals = TerminalRegistry::default();
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let agent = lazybox_agents::registry()
            .get("codex")
            .expect("codex agent");
        terminals.terminal_meta.lock().await.insert(
            id,
            (
                key.clone(),
                lazybox_ipc::TerminalKind::Agent("codex".into()),
            ),
        );
        terminals
            .terminal_models
            .lock()
            .await
            .insert(id, "gpt-5.5 · xhigh".to_string());

        detect_and_broadcast_model(&agent, footer, &terminals, &bus, id, &key).await;

        assert!(
            rx.try_recv().is_err(),
            "an unchanged model must not re-broadcast",
        );
    }

    /// #779: agents with no PTY model reading (Claude names its model via
    /// the pinned `--model` tier, so `detect_model_effort` → `None`) are a
    /// cheap no-op — no cache write, no broadcast.
    #[tokio::test]
    async fn detect_and_broadcast_model_skips_agents_without_a_reading() {
        let id = TerminalId(781);
        let key: SessionKey = "github-o-r-781".into();
        let footer = include_bytes!("../../agents/tests/fixtures/codex_real_idle.bin");
        let terminals = TerminalRegistry::default();
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let agent = lazybox_agents::registry()
            .get("claude")
            .expect("claude agent");
        terminals.terminal_meta.lock().await.insert(
            id,
            (
                key.clone(),
                lazybox_ipc::TerminalKind::Agent("claude".into()),
            ),
        );

        detect_and_broadcast_model(&agent, footer, &terminals, &bus, id, &key).await;

        assert!(rx.try_recv().is_err(), "Claude never broadcasts a reading");
        assert!(
            terminals.terminal_models.lock().await.get(&id).is_none(),
            "no reading means no cache write",
        );
    }

    fn bringup(command: &str, readiness: Option<&str>) -> lazybox_config::WorktreeBringup {
        lazybox_config::WorktreeBringup {
            command: command.to_string(),
            profile: "robin".to_string(),
            command_timeout_secs: 30,
            readiness: readiness.map(str::to_string),
            readiness_timeout_secs: 2,
            readiness_interval_secs: 1,
        }
    }

    #[test]
    fn substitute_profile_replaces_the_placeholder() {
        assert_eq!(
            substitute_profile("dev up {profile}", "robin"),
            "dev up robin"
        );
        assert_eq!(substitute_profile("dev up", "robin"), "dev up");
    }

    #[tokio::test]
    async fn bringup_runs_command_in_the_worktree_with_the_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The command records both its cwd-relative write and the exported
        // profile, proving it ran inside the worktree with LAZYBOX_PROFILE set.
        let spec = bringup("printf '%s' \"$LAZYBOX_PROFILE\" > marker", None);
        let outcome = execute_worktree_bringup(dir.path(), &spec, &[], |_| {}).await;
        assert_eq!(outcome, BringupOutcome::Ready);
        let marker = std::fs::read_to_string(dir.path().join("marker")).expect("marker written");
        assert_eq!(marker, "robin");
    }

    #[tokio::test]
    async fn bringup_injects_the_repo_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The repo env must reach the bring-up command, same as it reaches
        // the agent/shell PTY that follows.
        let spec = bringup("printf '%s' \"$DATABASE_URL\" > marker", None);
        let env = vec![(
            "DATABASE_URL".to_string(),
            "postgres://localhost/dev".to_string(),
        )];
        let outcome = execute_worktree_bringup(dir.path(), &spec, &env, |_| {}).await;
        assert_eq!(outcome, BringupOutcome::Ready);
        let marker = std::fs::read_to_string(dir.path().join("marker")).expect("marker written");
        assert_eq!(marker, "postgres://localhost/dev");
    }

    #[tokio::test]
    async fn bringup_reports_a_nonzero_command_as_degraded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = bringup("exit 3", None);
        let outcome = execute_worktree_bringup(dir.path(), &spec, &[], |_| {}).await;
        assert!(matches!(outcome, BringupOutcome::CommandFailed(_)));
        assert!(outcome.warning().is_some());
    }

    #[tokio::test]
    async fn bringup_command_timeout_kills_the_child_and_degrades() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A command that would touch `marker` only after a long sleep. The
        // 1s command timeout must fire first and — via kill_on_drop —
        // actually kill the child, so `marker` is never written even after
        // we wait past the sleep. Without the timeout this wedges forever;
        // without kill_on_drop the orphaned `sh` still writes `marker`.
        let mut spec = bringup("sleep 30 && touch marker", None);
        spec.command_timeout_secs = 1;
        let outcome = execute_worktree_bringup(dir.path(), &spec, &[], |_| {}).await;
        assert!(matches!(outcome, BringupOutcome::CommandFailed(_)));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !dir.path().join("marker").exists(),
            "timed-out bring-up child must be killed, not left to finish"
        );
    }

    #[tokio::test]
    async fn bringup_gates_on_readiness_until_the_probe_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // command drops a `ready` file; readiness passes only once it exists,
        // so a passing probe short-circuits the poll immediately.
        let spec = bringup("touch ready", Some("test -f ready"));
        let outcome = execute_worktree_bringup(dir.path(), &spec, &[], |_| {}).await;
        assert_eq!(outcome, BringupOutcome::Ready);
    }

    #[tokio::test]
    async fn bringup_times_out_when_readiness_never_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = bringup("true", Some("false"));
        let outcome = execute_worktree_bringup(dir.path(), &spec, &[], |_| {}).await;
        assert!(matches!(outcome, BringupOutcome::NotReady(_)));
    }
}
