//! Wires the IPC `Spawn`/`Write`/`Resize`/`Close` commands to the
//! [`SessionBackend`](crate::backend::SessionBackend) trait. The
//! server itself owns no PTY state — every backend-side operation
//! goes through `config.backend`.
//!
//! ## Per-process state on `ServerConfig`
//!
//! `ServerConfig::terminals` maps wire `TerminalId` → backend session
//! key. Multiple connections (in-process channel + a remote SSH
//! `lazybox --connect`) share this map so they see the same set.
//!
//! ## Flow on Spawn
//!
//! 1. Resolve `kind` to argv:
//!    - `Agent(id)` → look up `Registry`, call `Agent::spawn(ctx)`.
//!    - `Shell` → user's `$SHELL` or fallback `/bin/sh`.
//!    - `LogTail` → `tail -F path`.
//! 2. `backend.spawn(argv, cwd, env)` returns a backend session key.
//! 3. Allocate a fresh `TerminalId`; store the pairing on
//!    `config.terminals`.
//! 4. `backend.subscribe(key)` → spawn a pump task that fans each
//!    output chunk to `config.bus` as `Event::TerminalOutput`. When
//!    the chunk stream ends, await `backend.wait_exit`, emit
//!    `Event::TerminalExited`, drop the map entry.
//! 5. Broadcast `Event::TerminalSpawned` to every subscriber.

use crate::ServerConfig;
use chrono::Utc;
use lazybox_agents::SpawnCtx;
use lazybox_core::{
    SessionId, SessionKey, SessionKind, Task, Workspace, WorkspaceKey, WorkspaceSession as Session,
};
use lazybox_ipc::{Event, TerminalId, TerminalKind, TerminalSnapshot};
use lazybox_store::WorkspaceRecord;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

fn alloc_terminal_id(store: &dyn lazybox_store::Store) -> TerminalId {
    // `fetch_max` (not a one-shot seed) so the allocator is correct
    // even when several stores are seen in one process (tests) — the
    // counter only ever moves forward.
    if let Ok(Some(raw)) = store.get_kv(TERMINAL_ID_HIGH_WATER_KEY)
        && let Ok(high_water) = raw.trim().parse::<u64>()
    {
        NEXT_TERMINAL_ID.fetch_max(high_water + 1, Ordering::Relaxed);
    }
    let id = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
    if let Err(e) = store.set_kv(TERMINAL_ID_HIGH_WATER_KEY, &id.to_string()) {
        tracing::warn!("terminal-id high-water mark: store write failed: {e}");
    }
    TerminalId(id)
}

/// Build the argv for `kind`. None means we don't know how to spawn
/// it (unknown agent id, etc.) — handled by emitting a ProviderError.
///
/// `hook_settings_path` is the per-session settings file the daemon
/// generated for an agent that reports state through structured hooks
/// (Claude). It's threaded into [`SpawnCtx`] so the agent's argv builder
/// can append its settings flag; `None` for agents without hook support.
fn argv_for(
    config: &ServerConfig,
    kind: &TerminalKind,
    cwd: &Option<PathBuf>,
    skip_permissions: bool,
    hook_settings_path: Option<PathBuf>,
) -> Option<Vec<String>> {
    match kind {
        TerminalKind::Agent(agent_id) => {
            let agent = config.agents.get(agent_id)?;
            let ctx = SpawnCtx {
                session_key: String::new(),
                worktree: cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
                repo: None,
                pr_number: None,
                env: Default::default(),
                skip_permissions,
                hook_settings_path,
            };
            Some(agent.spawn(&ctx))
        }
        TerminalKind::Shell => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            Some(vec![shell])
        }
        TerminalKind::LogTail { path } => Some(vec!["tail".into(), "-F".into(), path.clone()]),
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
    guarded_hook_command(exe, &format!(" --backend-key \"{backend_key}\""))
}

/// Hook command with no correlation flag — what the pre-spawn
/// placeholder settings file carries (see [`write_hook_settings`]'s
/// callers). `hook-ingest` without a correlation flag drains stdin and
/// exits 0, so if the agent ever races the post-spawn rewrite and
/// reads the placeholder, its hooks are harmless no-ops and the
/// session just keeps PTY detection.
fn hook_command_placeholder(exe: &Path) -> String {
    guarded_hook_command(exe, "")
}

/// Absolute path of the running lazybox binary, verified to still exist
/// on disk. The path can be dead even while the daemon is running: a
/// `cargo run` daemon execs from `target/debug/lazybox`, and a later
/// `cargo clean` unlinks that file while the process keeps running off
/// the deleted inode. `None` → the caller skips hook settings and the
/// spawn falls back to PTY state detection.
fn hook_exe() -> Option<PathBuf> {
    std::env::current_exe().ok().filter(|p| p.is_file())
}

/// The exec is guarded: the binary verified at spawn time can still be
/// deleted mid-session (`cargo clean`), and without the guard every hook
/// fails with a raw `/bin/sh: <path>: No such file or directory`. The
/// guard names the problem on stderr instead, so Claude's hook-failure
/// report points at the actual cause.
fn guarded_hook_command(exe: &Path, args: &str) -> String {
    let exe = exe.to_string_lossy();
    format!(
        "[ -x \"{exe}\" ] || {{ echo \"lazybox hook: binary missing at {exe} (removed by cargo clean or a rebuild?)\" >&2; exit 1; }}; \"{exe}\" hook-ingest{args}"
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
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("hook settings: create_dir_all {}: {e}", parent.display());
            return None;
        }
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
    cwd: Option<String>,
    initial_prompt: Option<String>,
    autonomous: bool,
) {
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
    let skip_permissions = skip_permissions_for(autonomous, &cfg);
    tracing::info!(
        %session_key,
        ?session_id,
        ?kind,
        cwd = ?cwd,
        has_initial_prompt = initial_prompt.is_some(),
        autonomous,
        skip_permissions,
        "handle_spawn: entry"
    );
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
    let _inflight = match InflightSpawnGuard::try_claim(config, &session_key, &kind) {
        Ok(guard) => guard,
        Err(()) => {
            collapse_onto_inflight_spawn(config, &session_key, &kind, initial_prompt.as_deref())
                .await;
            return;
        }
    };
    // Singleton enforcement at the daemon (the source of truth for
    // who's running what). The TUI also intercepts duplicates
    // client-side for snappy focus-not-spawn behavior, but that
    // alone fails the moment a second client connects to the same
    // daemon. The guard here protects the invariant for everyone:
    // at most one Claude per session, one Codex per session, etc.
    if let Some(existing) = find_existing_singleton(config, &session_key, &kind).await {
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
            Box::pin(handle_inject_prompt(config, existing, prompt, None)).await;
        }
        let _ = config.bus.send(Event::TerminalFocusRequested {
            terminal_id: existing,
        });
        return;
    }
    // Resolve target session + cwd. The cwd param wins over
    // workspace lookup so the existing `Spawn { cwd: Some(...) }`
    // callers (tests, in-process flows) keep working unchanged.
    // `owning_session` is the session id this spawn lives in — used
    // to populate `terminal_sessions` so the migration freeze can
    // scope correctly. None when cwd was overridden out-of-band.
    let (cwd_path, owning_session): (Option<PathBuf>, Option<lazybox_core::SessionId>) =
        if let Some(c) = cwd.as_deref() {
            (Some(PathBuf::from(c)), None)
        } else {
            match resolve_or_create_session(config, &session_key, session_id, &kind).await {
                Ok((path, sid)) => (Some(path), Some(sid)),
                Err(e) => {
                    let _ = config.bus.send(Event::provider_error_permanent(
                        "spawn:session",
                        e.to_string(),
                    ));
                    return;
                }
            }
        };
    // Session + worktree are resolved here — for a fresh issue this is
    // where a cold clone / `git fetch` / setup script gets paid
    // synchronously, so surfacing the elapsed time makes the otherwise-
    // silent worktree provisioning cost visible.
    tracing::info!(
        elapsed_ms = t0.elapsed().as_millis(),
        "handle_spawn: session/worktree resolved",
    );
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
    // detection instead.
    let hook_settings = match hook_exe() {
        Some(exe) => {
            write_hook_settings(config, &kind, terminal_id, &hook_command_placeholder(&exe))
        }
        None => {
            tracing::warn!(
                ?terminal_id,
                "hook settings: lazybox binary path is unresolvable or no longer on disk — \
                 skipping hooks; agent state falls back to PTY detection"
            );
            None
        }
    };
    let argv = match argv_for(
        config,
        &kind,
        &cwd_path,
        skip_permissions,
        hook_settings.clone(),
    ) {
        Some(a) => a,
        None => {
            let _ = config.bus.send(Event::provider_error_permanent(
                &format!("spawn:{kind:?}"),
                "no agent registered for this id",
            ));
            return;
        }
    };

    // Human-readable hint the backend bakes into its session name so
    // `tmux ls` shows something like `lazybox-github-acme-widget-126-claude-NNNN`
    // instead of `lazybox-4`. Backends append their own uniqueness
    // suffix (PID + counter) so the hint doesn't need to be unique.
    let kind_label = match &kind {
        TerminalKind::Agent(id) => id.clone(),
        TerminalKind::Shell => "shell".into(),
        TerminalKind::LogTail { path } => {
            let base = path.rsplit('/').next().unwrap_or(path);
            format!("log-{base}")
        }
    };
    let hint = format!("{}-{kind_label}", session_key.as_str());
    // Pre-trust the worktree for an unattended launch. Claude shows an
    // interactive workspace-trust dialog for any directory it hasn't
    // seen (skipped only in non-interactive `-p` mode, which we don't
    // use), so an autonomous spawn in a freshly provisioned worktree
    // would hang on it with no human to accept. Gated on
    // `skip_permissions`: interactive spawns keep the user-facing prompt.
    if skip_permissions
        && let TerminalKind::Agent(agent_id) = &kind
        && let Some(agent) = config.agents.get(agent_id)
        && let Some(worktree) = cwd_path.as_deref()
    {
        agent.prepare_unattended(worktree);
    }

    // Per-repo env injection: look up the workspace's primary task
    // repo, read `repos.<owner/name>.env` from YAML, fan it into
    // the spawn. Missing config or workspace = empty env, no error.
    let env = collect_repo_env(config, &session_key);
    tracing::info!(
        ?argv,
        cwd_path = ?cwd_path,
        %hint,
        env_count = env.len(),
        "handle_spawn: calling backend.spawn"
    );
    let backend_key = match config
        .backend
        .spawn(&argv, cwd_path.as_deref(), &env, &hint)
        .await
    {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("handle_spawn: backend.spawn failed: {e}");
            let _ = config
                .bus
                .send(Event::provider_error_permanent("spawn", e.to_string()));
            return;
        }
    };
    tracing::info!(
        %backend_key,
        elapsed_ms = t0.elapsed().as_millis(),
        "handle_spawn: backend.spawn ok",
    );
    // Phase 2 of 2: now the backend key exists, atomically rewrite the
    // settings file with the real correlated hook command.
    if hook_settings.is_some() {
        if let Some(exe) = hook_exe() {
            let _ = write_hook_settings(
                config,
                &kind,
                terminal_id,
                &hook_command(&exe, &backend_key),
            );
        }
    }

    // `terminal_id` was allocated above (before argv) so the hook
    // settings file could embed it. Insert the auxiliary maps BEFORE the
    // primary `terminals` map.
    // `snapshot_terminals` iterates `terminals` and looks up meta;
    // doing terminals-last means a snapshot during the gap sees no
    // entry for this id (consistent miss) instead of an entry with
    // a bogus default session_key (inconsistent hit). The
    // `TerminalSpawned` broadcast below tells clients about the
    // newly-complete terminal once both inserts have landed.
    // INTENTIONAL non-canonical order here: terminal_meta first,
    // terminal_sessions next, terminals LAST. This is safe (no two
    // locks co-held — each `.lock().await.insert(...)` releases at
    // end-of-statement) and the order is deliberate for a *reader*
    // race, not a writer-writer one: a snapshot that scans `terminals`
    // is guaranteed to find a matching `terminal_meta` entry, because
    // the meta lock is inserted into BEFORE the terminals lock. The
    // canonical order in `crate::TERMINAL_MAP_LOCK_ORDER` applies to
    // CO-HOLDING; sequential acquire-and-drop can use any order, and
    // here the snapshot invariant pins this one.
    config
        .terminal_meta
        .lock()
        .await
        .insert(terminal_id, (session_key.clone(), kind.clone()));
    if let Some(sid) = owning_session {
        config
            .terminal_sessions
            .lock()
            .await
            .insert(terminal_id, sid);
    }
    if skip_permissions {
        config
            .no_permission_terminals
            .lock()
            .await
            .insert(terminal_id);
    }
    config
        .terminals
        .lock()
        .await
        .insert(terminal_id, backend_key.clone());
    // Persist the (backend_key → session_key, kind) pairing so the
    // next lazybox start can reattach surviving tmux sessions to their
    // owning workspace. Without this, `recover_sessions` reattaches
    // raw PTYs but doesn't know which workspace they belong to —
    // sidebar badges go blank, even though the agent is still alive.
    persist_terminal_meta(config, &backend_key, &session_key, &kind).await;
    persist_no_permission(config, &backend_key, skip_permissions).await;

    // Pump backend output → bus. Also runs agent-state detection
    // on each chunk so the user sees a "needs input" badge when
    // Claude/Codex is waiting on an approval prompt. State is
    // cached per-terminal so we only broadcast on transitions.
    let bus = config.bus.clone();
    let backend = config.backend.clone();
    let terminals_map = config.terminals.clone();
    let term_sessions_map = config.terminal_sessions.clone();
    let agent_states_map = config.agent_states.clone();
    let agent_detect_resets_map = config.agent_detect_resets.clone();
    let hook_driven_map = config.hook_driven_terminals.clone();
    let prompt_submit_map = config.prompt_submit_signals.clone();
    let input_shapes_map = config.input_needed_shapes.clone();
    let terminal_meta_map = config.terminal_meta.clone();
    let no_permission_map = config.no_permission_terminals.clone();
    let store_for_pump = config.store.clone();
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
    // Broadcast BEFORE spawning the pump task. Otherwise a
    // fast-exiting terminal (e.g. a command that immediately
    // errors) can fire `TerminalExited` from the pump before this
    // `TerminalSpawned` even goes out — subscribers see "remove a
    // terminal you never told me about" and book-keeping diverges.
    let subscriber_count = config.bus.receiver_count();
    tracing::info!(
        ?terminal_id,
        %session_key,
        ?kind,
        subscriber_count,
        "handle_spawn: broadcasting TerminalSpawned"
    );
    let send_result = config.bus.send(Event::TerminalSpawned {
        terminal_id,
        session_key,
        kind,
        no_permission: skip_permissions,
    });
    if let Err(e) = send_result {
        tracing::error!("handle_spawn: bus.send(TerminalSpawned) failed: {e}");
    }
    tokio::spawn(async move {
        let mut sub = match backend.subscribe(&key_for_pump).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("backend subscribe {key_for_pump}: {e}");
                return;
            }
        };
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
        // Hysteresis: timestamp of the last InputNeeded detection.
        // When detect_state leaves InputNeeded (to Working or Idle)
        // and the previous state was InputNeeded, we ONLY honor the
        // transition if it's been long enough since we last saw the
        // prompt patterns — gives the buffer time to capture genuine
        // new output (user typed a response, Claude is now streaming
        // back), rather than treating a ticker chunk that scrolled the
        // prompt out of buffer as "agent done." The faster, expected
        // Working↔Idle flips are NOT damped — only leaving the sticky
        // "needs input" state is.
        let mut last_input_needed_at: Option<std::time::Instant> = None;
        const INPUT_NEEDED_HYSTERESIS: std::time::Duration = std::time::Duration::from_secs(8);

        async fn maybe_emit_state_change(
            agent: Option<&std::sync::Arc<dyn lazybox_agents::Agent>>,
            buf: &mut Vec<u8>,
            bytes: &[u8],
            states: &std::sync::Arc<
                tokio::sync::Mutex<std::collections::HashMap<TerminalId, lazybox_ipc::AgentState>>,
            >,
            bus: &tokio::sync::broadcast::Sender<Event>,
            id: TerminalId,
            session_key: &SessionKey,
            last_input_needed_at: &mut Option<std::time::Instant>,
            hysteresis: std::time::Duration,
            hook_driven: &std::sync::Arc<
                tokio::sync::Mutex<std::collections::HashMap<TerminalId, std::time::Instant>>,
            >,
            input_shapes: &std::sync::Arc<
                tokio::sync::Mutex<
                    std::collections::HashMap<TerminalId, lazybox_agents::PromptShape>,
                >,
            >,
        ) {
            const STATE_BUF_CAP: usize = 32 * 1024;
            let Some(agent) = agent else {
                return;
            };
            buf.extend_from_slice(bytes);
            if buf.len() > STATE_BUF_CAP {
                let drop = buf.len() - STATE_BUF_CAP;
                buf.drain(..drop);
            }
            // Search only the recent tail. The 32 KiB outer buffer
            // exists so spread-out tickers + small chunks accumulate
            // enough context, but once a prompt scrolls past this
            // tail it should STOP matching — otherwise the user's
            // "I answered the prompt and moved on" never reflects:
            // the old `❯ 1.` text stays in `buf`, every chunk
            // re-detects Asking, hysteresis refreshes forever, and
            // the "needs input" label sticks. 4 KiB is enough to
            // capture a single prompt's render + a screen of context,
            // small enough that next-screen content evicts the old.
            // 16 KiB (was 8 KiB) — Claude's bash-permission prompts
            // can sit BELOW 8+ KiB of preview output (long heredocs,
            // `cat <<EOF | gh api ...` patches, multi-file `cat`
            // outputs, etc.). The user reported a real
            // `Do you want to proceed?` prompt going undetected
            // because the prompt + arrow + choice markers were
            // pushed past the old 8 KiB tail. 16 KiB still evicts
            // stale prompts within ~half a screen of follow-up
            // output, while comfortably covering claude's largest
            // tool-preview screens.
            const DETECT_WINDOW: usize = 16 * 1024;
            let tail_start = buf.len().saturating_sub(DETECT_WINDOW);
            let detect_window = &buf[tail_start..];
            // Chunk-boundary hint: the chunk just appended occupies the
            // LAST `bytes.len()` bytes of the window. A full-screen
            // repaint delivers a live dialog and the bottom status bar
            // in one chunk (status bar last); the chunk-aware detector
            // keeps the dialog live instead of reading it as already
            // answered by the "more recent" work anchor.
            let last_chunk_start = detect_window.len().saturating_sub(bytes.len());
            let Some(new_state) = agent.detect_state_chunked(detect_window, last_chunk_start)
            else {
                return;
            };
            // Hooks-primary, PTY-fallback. Once a terminal has reported
            // any structured lifecycle hook, hooks own the Working↔Idle
            // distinction (deterministic, no screen-scraping flicker), so
            // a PTY `Working` reading is ignored for it. The PTY detector
            // still contributes two corrections hooks miss:
            //   - a confident idle (composer drawn, ready for a prompt) —
            //     Ctrl-C / Esc end Claude's turn without firing `Stop`, so
            //     a hook-driven terminal could otherwise stick at
            //     `Working` forever;
            //   - an on-screen permission dialog → `InputNeeded`. An
            //     inline mid-turn approval fires NO hook (`PreToolUse`
            //     lands only AFTER approval, `Notification` only after
            //     Claude goes idle), so the rendered `Esc to cancel`
            //     dialog is the sole source of truth — without honoring it
            //     the `?` never shows on a hook-driven terminal. The PTY
            //     detector's recency gating keeps this from leaking the
            //     stale-scrollback false positives it once produced.
            // When the last hook is older than `HOOK_STALENESS`, the
            // gate opens entirely: hooks that stopped flowing (socket
            // hiccup, helper failure) must degrade the terminal back to
            // scraping rather than freeze it on the last hook state.
            // A terminal that never reported a hook isn't in the map and
            // keeps full PTY detection unchanged.
            let last_hook_at = hook_driven.lock().await.get(&id).copied();
            if let Some(last_hook_at) = last_hook_at {
                let current = states.lock().await.get(&id).copied();
                if !pty_reading_allowed(
                    current,
                    new_state,
                    agent.detect_ready_for_prompt(detect_window),
                    // Lazy: the dialog-supersession scan re-strips the
                    // window, so only the one reading that needs it
                    // (stale hooks + Working demoting a cached `?`)
                    // pays for it.
                    || agent.working_reading_supersedes_dialog(detect_window),
                    last_hook_at.elapsed(),
                    HOOK_STALENESS,
                ) {
                    return;
                }
            }
            // Trace-level on steady-state runs (claude emits 100+
            // chunks/sec during streaming and we don't want to drown
            // the log). Only ELEVATE to debug-level on every Asking
            // detection so a missing `?` pill is easy to bisect from
            // the log without re-running with full trace verbosity.
            // Toggle full trace via `RUST_LOG=lazybox_server=trace`.
            tracing::trace!(
                terminal_id = ?id,
                buf_len = buf.len(),
                detected = ?new_state,
                "detect_state ran",
            );
            if new_state == lazybox_ipc::AgentState::InputNeeded {
                tracing::debug!(
                    terminal_id = ?id,
                    buf_len = buf.len(),
                    tail_tip = %String::from_utf8_lossy(
                        &detect_window[detect_window.len().saturating_sub(120)..]
                    ),
                    "detect_state → InputNeeded",
                );
                *last_input_needed_at = Some(std::time::Instant::now());
                // Every InputNeeded the PTY detector raises is
                // structurally a chooser / permission / consent dialog
                // (freeform asks are deliberately not flagged), so a
                // bare chooser keystroke is a complete answer. Hook-
                // raised elicitations overwrite this with `FreeText` in
                // `handle_ingest_hook`. Recorded before the dedupe
                // below so a re-rendered prompt refreshes the shape.
                input_shapes
                    .lock()
                    .await
                    .insert(id, lazybox_agents::PromptShape::Chooser);
            }
            // Hysteresis. Claude's status-bar updates make the
            // detector miss the prompt for one chunk, then catch
            // it on the next. Without this guard the pill flickers
            // every few seconds while Claude is genuinely still
            // waiting. Only damp the edge that LEAVES InputNeeded —
            // and only when the new reading is the ambiguous
            // fall-through. A clear signal that the prompt is gone — a
            // live Working status line, or an idle composer the
            // readiness probe affirmatively recognizes — is honored
            // immediately, so a wrong InputNeeded can't stick for the
            // full window once Claude is visibly streaming or idle.
            let clear_exit_signal = new_state == lazybox_ipc::AgentState::Working
                || (new_state == lazybox_ipc::AgentState::Idle
                    && agent.detect_ready_for_prompt(detect_window));
            // Read + decide + insert under ONE lock acquisition. A
            // separate read-then-insert let a concurrent writer (hook
            // ingest, the optimistic Enter flip) land between the two
            // and be silently clobbered by a stale decision.
            let current = {
                let mut map = states.lock().await;
                let current = map.get(&id).copied();
                if should_suppress_input_needed_exit(
                    current,
                    new_state,
                    clear_exit_signal,
                    last_input_needed_at.map(|t| t.elapsed()),
                    hysteresis,
                ) {
                    drop(map);
                    tracing::debug!(
                        terminal_id = ?id,
                        ?new_state,
                        "state hysteresis: suppressing InputNeeded → {:?}",
                        new_state,
                    );
                    return;
                }
                if current == Some(new_state) {
                    return;
                }
                map.insert(id, new_state);
                current
            };
            // Loud log so when the user reports "the pill didn't
            // show", we can confirm whether the daemon-side
            // detector actually fired vs. the event got lost
            // somewhere downstream. Keyed off TerminalId so
            // grep-ing the log file makes the path obvious.
            tracing::info!(
                terminal_id = ?id,
                %session_key,
                previous = ?current,
                state = ?new_state,
                "agent state transition → broadcasting Event::AgentState",
            );
            let _ = bus.send(Event::AgentState {
                session_key: session_key.clone(),
                terminal_id: id,
                state: new_state,
            });
        }

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
        let check_ready =
            |state_buf: &Vec<u8>, signaled: &mut bool, signal: &tokio::sync::Notify| {
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
                if agent.detect_ready_for_prompt(tail) {
                    // `notify_one` STORES a permit when no waiter is
                    // registered yet; `notify_waiters` is edge-triggered
                    // and a ready signal fired before the inject task
                    // started waiting was lost forever, riding the
                    // inject path to its hard deadline.
                    signal.notify_one();
                    *signaled = true;
                }
            };
        if !sub.replay.is_empty() {
            maybe_emit_state_change(
                agent_for_pump.as_ref(),
                &mut state_buf,
                &sub.replay,
                &agent_states_map,
                &bus,
                id_for_pump,
                &session_key_for_pump,
                &mut last_input_needed_at,
                INPUT_NEEDED_HYSTERESIS,
                &hook_driven_map,
                &input_shapes_map,
            )
            .await;
            let _ = bus.send(Event::TerminalOutput {
                terminal_id: id_for_pump,
                bytes: sub.replay.clone(),
                seq: sub.last_seq,
            });
            // Permit-storing, like the live first-output path below — a
            // replay that lands before the inject task registers its
            // waiter must not be lost.
            first_output_signal_for_pump.notify_one();
            signaled_first_output = true;
            check_ready(&state_buf, &mut signaled_ready, &ready_signal_for_pump);
        }
        while let Some(chunk) = sub.live.recv().await {
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
                let answered = agent_detect_resets_map.lock().await.remove(&id_for_pump);
                if answered {
                    state_buf.clear();
                    last_input_needed_at = None;
                    tracing::debug!(
                        terminal_id = ?id_for_pump,
                        "user answered prompt; clearing agent-state detection buffer",
                    );
                }
            }
            maybe_emit_state_change(
                agent_for_pump.as_ref(),
                &mut state_buf,
                &chunk.bytes,
                &agent_states_map,
                &bus,
                id_for_pump,
                &session_key_for_pump,
                &mut last_input_needed_at,
                INPUT_NEEDED_HYSTERESIS,
                &hook_driven_map,
                &input_shapes_map,
            )
            .await;
            if !signaled_first_output {
                first_output_signal_for_pump.notify_one();
                signaled_first_output = true;
                tracing::info!(
                    terminal_id = ?id_for_pump,
                    elapsed_ms = t0_for_pump.elapsed().as_millis(),
                    "handle_spawn: first PTY output",
                );
            }
            check_ready(&state_buf, &mut signaled_ready, &ready_signal_for_pump);
            let _ = bus.send(Event::TerminalOutput {
                terminal_id: id_for_pump,
                bytes: chunk.bytes,
                seq: chunk.seq,
            });
        }
        let exit_code = backend.wait_exit(&key_for_pump).await;
        let _ = bus.send(Event::TerminalExited {
            terminal_id: id_for_pump,
            exit_code,
        });
        // INTENTIONAL non-canonical sequence: terminals first (so
        // `snapshot_terminals` stops seeing this id immediately) and
        // terminal_meta LAST (so any snapshot that still saw it in
        // terminals can resolve the meta lookup). Safe because no two
        // locks are co-held — each `.lock().await.remove(...)` releases
        // at end-of-statement. `crate::TERMINAL_MAP_LOCK_ORDER` applies
        // to co-holding sites only.
        terminals_map.lock().await.remove(&id_for_pump);
        term_sessions_map.lock().await.remove(&id_for_pump);
        agent_states_map.lock().await.remove(&id_for_pump);
        agent_detect_resets_map.lock().await.remove(&id_for_pump);
        hook_driven_map.lock().await.remove(&id_for_pump);
        prompt_submit_map.lock().await.remove(&id_for_pump);
        input_shapes_map.lock().await.remove(&id_for_pump);
        terminal_meta_map.lock().await.remove(&id_for_pump);
        no_permission_map.lock().await.remove(&id_for_pump);
        let _ = store_for_pump.delete_kv(&format!("terminal:{key_for_pump}"));
        let _ = store_for_pump.delete_kv(&format!("terminal-noperm:{key_for_pump}"));
        // Drop the per-session hook settings file we generated at spawn.
        // Best-effort — a leftover file is harmless (it's overwritten by
        // the next spawn that reuses the id, which can't happen anyway
        // since ids are monotonic) but cleaning up keeps the runtime dir
        // tidy. Reconstructed from the id, no bookkeeping needed.
        let _ = std::fs::remove_file(hook_settings_path(id_for_pump));
    });

    // Schedule prompt injection. Drives the `f`-for-fix flow: the
    // sidebar / activity pane spawn an agent with a pre-built
    // instruction so the user doesn't have to retype it.
    //
    // Wait for the agent to start `Asking` (its first prompt screen)
    // before writing. Typing into Claude during its banner boot drops
    // keystrokes onto the wrong UI surface and the prompt ends up
    // half-eaten. Timeout after 10s and write anyway — better a
    // garbled prompt than a silently-lost one.
    if let (Some(prompt), Some(agent)) = (initial_prompt, &agent_for_inject) {
        let agent = agent.clone();
        let paste = agent.inject_prompt(&prompt);
        let submit = agent.inject_submit();
        let backend = config.backend.clone();
        let backend_key = backend_key.clone();
        let id = terminal_id;
        let first_output = first_output_signal_for_inject;
        let ready_signal = ready_signal_for_inject;
        let agent_states = config.agent_states.clone();
        let t0_for_inject = t0;
        let config_for_inject = config.clone();
        tokio::spawn(async move {
            // Wait for the agent's input box to be drawn AND no
            // permission gate to be up — i.e. "claude is genuinely
            // ready to receive a pasted prompt." The pump task fires
            // `ready_signal` exactly once when `Agent::
            // detect_ready_for_prompt` first returns true. This is
            // strictly tighter than the previous "wait for not-
            // Asking" approach: the loose Asking detector matched
            // claude's normal idle screen and made the wait spin
            // the full deadline before every inject.
            //
            // Fallback ladder (each step has its own deadline):
            //   1. ready_signal — preferred path, fires within
            //      seconds of claude finishing its banner.
            //   2. first_output + SETTLE — for agents whose
            //      detector never reports ready (default impl),
            //      we still write 600ms past first byte. Agents
            //      with an authoritative readiness detector
            //      (`inject_requires_ready`) SKIP this rung — a
            //      blind settle-write would land the paste in
            //      claude's folder-trust prompt if it's still up.
            //   3. HARD_DEADLINE — last resort, inject blindly so
            //      a cold-start hang doesn't silently lose the
            //      user's prompt.
            const HARD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
            const SETTLE: std::time::Duration = std::time::Duration::from_millis(600);
            tracing::info!(
                terminal_id = ?id,
                paste_len = paste.len(),
                "initial_prompt: waiting for agent ready signal",
            );

            let trigger = await_inject_window(
                agent.inject_requires_ready(),
                &ready_signal,
                &first_output,
                HARD_DEADLINE,
                SETTLE,
            )
            .await;
            // The deadline rung pastes blindly — but if the screen is a
            // boot-time gate (folder-trust / login / bypass chooser),
            // the paste plus its follow-up `\r` would blindly ANSWER
            // the chooser instead of landing in the input box. When the
            // cached state says the agent is parked on a gate, keep
            // waiting for the next ready / state change rather than
            // pasting, with an overall cap; past the cap, drop the
            // prompt loudly rather than feed it into the dialog.
            if trigger == InjectTrigger::Deadline {
                const GATE_CAP: std::time::Duration = std::time::Duration::from_secs(600);
                let gate_start = std::time::Instant::now();
                while agent_states.lock().await.get(&id).copied()
                    == Some(lazybox_ipc::AgentState::InputNeeded)
                {
                    if gate_start.elapsed() >= GATE_CAP {
                        tracing::warn!(
                            terminal_id = ?id,
                            "initial_prompt: agent still on an input gate after {GATE_CAP:?}; dropping the prompt rather than answering the gate with it"
                        );
                        return;
                    }
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        ready_signal.notified(),
                    )
                    .await;
                }
            }
            tracing::info!(
                terminal_id = ?id,
                paste_len = paste.len(),
                ?trigger,
                elapsed_ms = t0_for_inject.elapsed().as_millis(),
                "initial_prompt: inject window cleared — writing paste to backend",
            );
            // Subscribed before the paste write so the output chunks
            // the paste triggers are observable by the settle gate.
            let output_events = submit.is_some().then(|| config_for_inject.bus.subscribe());
            if let Err(e) = backend.write(&backend_key, &paste).await {
                tracing::warn!(
                    terminal_id = ?id,
                    "initial_prompt: backend.write(paste) failed: {e}"
                );
                return;
            }
            // Paste/submit split. Agents like Claude Code batch rapid
            // byte arrival as a paste; Enter inside that batch is a
            // soft line break, not a submit. Gate the submit keystroke
            // on the paste's repaint going quiet so Enter fires as its
            // own keystroke. Agents that don't need a separate submit
            // (the default trait impl) return None here and we skip
            // the second write entirely.
            if let (Some(submit_bytes), Some(mut output_events)) = (submit, output_events) {
                await_paste_settled(&mut output_events, id, PASTE_QUIET_WINDOW, PASTE_SETTLE_CAP)
                    .await;
                let confirm = prepare_submit_confirmation(&config_for_inject, id).await;
                if let Err(e) = backend.write(&backend_key, &submit_bytes).await {
                    tracing::warn!(
                        terminal_id = ?id,
                        "initial_prompt: backend.write(submit) failed: {e}"
                    );
                    return;
                }
                confirm_prompt_submission(
                    confirm,
                    &*backend,
                    &backend_key,
                    &submit_bytes,
                    SUBMIT_CONFIRM_DEADLINE,
                )
                .await;
            }
        });
    }
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

/// Look up the session whose worktree this Spawn should land in.
///
/// - `Some(session_id)` → look it up in the workspace, error if it's
///   gone (rare race where the user removed the session between
///   selecting it and pressing the spawn key).
/// - `None` → use `Workspace::default_session`, or auto-create one
///   when the workspace is empty. Auto-creation emits
///   `Event::SessionCreated` so the sidebar's expansion-on-multi-
///   session UI reacts.
async fn resolve_or_create_session(
    config: &ServerConfig,
    session_key: &SessionKey,
    session_id: Option<SessionId>,
    kind: &TerminalKind,
) -> Result<(PathBuf, SessionId), crate::ServerError> {
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
        return Ok((path, session_id.unwrap_or_else(SessionId::new)));
    }

    // Spawn against a workspace that isn't (yet) persisted — common
    // in tests and in --test mode, and fine in general: nothing
    // about the wire-side `session_key` requires the workspace to
    // exist on disk. Just root the spawn in the user's cwd. Use a
    // fresh ephemeral session id so terminal_sessions still gets a
    // mapping for the migration freeze.
    //
    // EXCEPT when the row is missing because the workspace was DELETED
    // while this spawn was in flight (Kill racing a slow provision):
    // that case must abort — the fallback would silently launch a
    // skip-permissions agent in the daemon's own cwd. The tombstone set
    // distinguishes "deleted in this process" from "never existed".
    let mut workspace = match load_workspace(config, &workspace_key) {
        Ok(w) => w,
        Err(_) => {
            if config
                .deleted_workspaces
                .lock()
                .expect("deleted_workspaces poisoned")
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
            return Ok((
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                SessionId::new(),
            ));
        }
    };

    if let Some(id) = session_id {
        let session = workspace.find_session(id).ok_or_else(|| {
            crate::ServerError::Workspace(format!("session {id:?} not in workspace"))
        })?;
        ensure_worktree_present(config, &workspace, &session.worktree_path).await;
        return Ok((session.worktree_path.clone(), session.id));
    }
    if let Some(session) = workspace.default_session() {
        ensure_worktree_present(config, &workspace, &session.worktree_path).await;
        return Ok((session.worktree_path.clone(), session.id));
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

    let prov_start = std::time::Instant::now();
    let provisioned = provision_worktree(&workspace, &path).await;
    tracing::info!(
        elapsed_ms = prov_start.elapsed().as_millis(),
        ok = provisioned.is_ok(),
        worktree = %path.display(),
        "provision_worktree complete",
    );
    if let Err(e) = &provisioned {
        // Real-checkout failed (no GH access, branch missing, network
        // hiccup) — fall back to an empty dir so spawn works. Surface
        // a non-fatal error so the user knows their `s` press landed
        // in a bare directory, not the PR's tree.
        tracing::warn!("worktree provisioning failed: {e}");
        let _ = config.bus.send(Event::provider_error_retryable(
            "worktree",
            format!("git worktree setup failed; using empty dir ({e})"),
        ));
        ensure_dir_exists(&path).await;
    }

    let session = Session::new(
        workspace_key.clone(),
        kind_for_session,
        path.clone(),
        Utc::now(),
    );
    let new_session_id = session.id;
    workspace.add_session(session.clone());
    persist_and_broadcast(config, &workspace).await?;
    let _ = config.bus.send(Event::SessionCreated(Box::new(session)));
    Ok((path, new_session_id))
}

/// Build a deterministic branch name for a task that has no upstream
/// branch (issues, Linear tickets, future provider-specific items).
/// Deterministic on `task.id` so two spawns on the same issue map to
/// the same local branch — otherwise pressing the spawn key twice
/// would leave two orphan branches, neither push-ready.
///
/// Examples:
/// - `github:owner/repo#42` → `lazybox/issue-42`
/// - `linear:ENG-456`       → `lazybox/linear-eng-456`
/// - anything else          → `lazybox/<source>-<sanitized-key>`
fn derive_branch_for_branchless(task: &Task) -> String {
    let source = task.id.source.to_ascii_lowercase();
    let raw_key = &task.id.key;

    if source == "github" {
        if let Some(hash_idx) = raw_key.rfind('#') {
            let number = &raw_key[hash_idx + 1..];
            if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
                return format!("lazybox/issue-{number}");
            }
        }
    }

    format!("lazybox/{source}-{}", sanitize_branch_component(raw_key))
}

/// Branch name for a blank workspace (no linked task at all).
/// Deterministic on the workspace key for the same reason
/// [`derive_branch_for_branchless`] is deterministic on the task id:
/// repeated spawns on the same workspace reuse one branch.
fn derive_branch_for_workspace(workspace: &Workspace) -> String {
    format!(
        "lazybox/{}",
        sanitize_branch_component(workspace.key.as_str())
    )
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

/// `owner/repo` for a workspace with no linked task, recovered from its
/// project key. Only `github-` keys carry a clonable repo — `local-`
/// projects legitimately have none, so they error and the caller's
/// empty-dir fallback stays the right outcome for them.
fn clonable_repo_from_project(workspace: &Workspace) -> Result<String, crate::ServerError> {
    let key = lazybox_core::workspace_project_key(workspace).ok_or_else(|| {
        crate::ServerError::Workspace("workspace has no primary task or project".into())
    })?;
    if key.source_prefix() != "github" {
        return Err(crate::ServerError::Workspace(format!(
            "project '{key}' has no repo to clone"
        )));
    }
    Ok(key.display_name())
}

/// Try to set up a real git worktree at `target` for the workspace's
/// primary task. Returns Ok(()) when a checkout succeeded, Err when
/// we couldn't (caller falls back to a plain mkdir).
async fn provision_worktree(
    workspace: &Workspace,
    target: &std::path::Path,
) -> Result<(), crate::ServerError> {
    use crate::ServerError;
    // A blank workspace (created via `n` under a project, no issue/PR
    // linked) has no task to read a repo from — but its project key
    // still encodes `owner/repo` for GitHub projects, so it gets a
    // real clone instead of the caller's empty-dir fallback.
    let task = workspace.primary_task();
    let repo = match task {
        Some(task) => task
            .repo
            .as_deref()
            .ok_or_else(|| ServerError::Workspace("task has no repo".into()))?
            .to_string(),
        None => clonable_repo_from_project(workspace)?,
    };
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| ServerError::Workspace(format!("repo '{repo}' is not owner/name")))?;

    let mgr = lazybox_git_ops::WorktreeManager::default_base();
    let worktree = match task.and_then(|t| t.branch.as_deref()) {
        Some(branch) => mgr
            .checkout_at(target, owner, name, branch)
            .await
            .map_err(|e| ServerError::Worktree(format!("checkout_at: {e}")))?,
        None => {
            // Issue (or other branchless task, or blank workspace):
            // cut a fresh branch off the repo default. Branch name
            // encodes the task key (or the workspace key when there is
            // no task) so two spawns on the same item land on the same
            // branch and subsequent presses are idempotent — without
            // that, pressing `c` twice on issue #42 would create
            // `lazybox/issue-42-…` and `lazybox/issue-42-…-2`, neither of
            // which corresponds to a PR the user can push.
            let new_branch = match task {
                Some(task) => derive_branch_for_branchless(task),
                None => derive_branch_for_workspace(workspace),
            };
            let base = mgr
                .default_branch(owner, name)
                .await
                .map_err(|e| ServerError::Worktree(format!("default_branch lookup: {e}")))?;
            mgr.checkout_new_branch_at(target, owner, name, &new_branch, &base)
                .await
                .map_err(|e| ServerError::Worktree(format!("checkout_new_branch_at: {e}")))?
        }
    };

    // Apply mounts: global `worktree.mounts` + per-repo
    // `repos.<owner/name>.mounts` from YAML. Best-effort — a mount
    // failure logs a warning but doesn't fail the spawn. Both are
    // idempotent so re-running this on an already-mounted worktree
    // is a no-op.
    // YAML load failures used to be `.unwrap_or_default()` — silently
    // disabling every configured mount on a typo. Surface the parse
    // error so a broken `~/.lazybox/config.yaml` shows up loudly in
    // `/tmp/lazybox.log` instead of users wondering why their mounts
    // stopped working after an edit.
    let cfg = match lazybox_config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                repo = %format!("{owner}/{name}"),
                "Config::load failed (mounts will be skipped): {e}",
            );
            lazybox_config::Config::default()
        }
    };
    let mut mounts = config_mounts_to_git(&cfg.worktree.mounts);
    let repo_key = format!("{owner}/{name}");
    if let Some(repo_cfg) = cfg.repos.get(&repo_key) {
        mounts.extend(config_mounts_to_git(&repo_cfg.mounts));
    }
    if !mounts.is_empty()
        && let Err(e) = mgr.apply_mounts(&worktree, &mounts).await
    {
        tracing::warn!("apply_mounts for {repo_key} failed: {e}");
    }

    // Scripts: same stacking as mounts (global + per-repo). Best-
    // effort — a single bad ScriptSpec (e.g. missing source, name
    // collision) logs a warning but doesn't fail the whole spawn.
    // The script that DID validate gets materialized; the one that
    // failed surfaces in /tmp/lazybox.log.
    let mut scripts = config_scripts_to_git(&cfg.worktree.scripts);
    if let Some(repo_cfg) = cfg.repos.get(&repo_key) {
        scripts.extend(config_scripts_to_git(&repo_cfg.scripts));
    }
    if !scripts.is_empty()
        && let Err(e) = mgr.apply_scripts(&worktree, &scripts).await
    {
        tracing::warn!("apply_scripts for {repo_key} failed: {e}");
    }
    let _ = worktree; // silence dead-binding warning from the
    // signature change; the worktree value is what
    // apply_mounts mutated and we're done with it.
    Ok(())
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

/// Whether a spawn should launch in no-permission / bypass mode.
/// Autonomous (lazybox-spawned) sessions follow `autonomous_skip_permissions`
/// (default on); interactive sessions the user opens follow the
/// separate `skip_permissions` toggle (default off). Pure so tests
/// don't need a real YAML on disk.
pub(crate) fn skip_permissions_for(autonomous: bool, cfg: &lazybox_config::Config) -> bool {
    if autonomous {
        cfg.agent.autonomous_skip_permissions
    } else {
        cfg.agent.skip_permissions
    }
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

/// How old a terminal's most recent hook may be before the PTY
/// detector regains full authority over it. Hooks normally arrive
/// every few seconds while Claude works (each tool call fires two);
/// half a minute of silence on a terminal whose PTY says `Working`
/// means the hook pipeline has stopped flowing (socket hiccup, helper
/// failure) and screen-scraping is the better signal again.
const HOOK_STALENESS: Duration = Duration::from_secs(30);

/// Whether a PTY-detector reading may be emitted for a hook-driven
/// terminal. Fresh hooks own Working↔Idle, so only two corrections
/// pass: an on-screen permission dialog (`InputNeeded`) and an
/// affirmatively-recognized idle composer. Once the last hook is older
/// than `staleness`, readings pass — the terminal degrades to plain PTY
/// detection instead of freezing on the last hook state — with ONE
/// exception: a `Working` reading demoting a hook-set `InputNeeded`. A
/// live dialog BLOCKS the hook stream (no tool calls fire while Claude
/// waits), so "stale hooks + cached `?`" is the normal shape of a real
/// unanswered dialog, not a broken pipeline; the demotion needs the
/// agent's affirmative evidence (`working_supersedes_dialog`: a tight
/// working anchor painted AFTER the dialog markers), or a full-repaint
/// status bar would clear a real `?`.
fn pty_reading_allowed(
    current: Option<lazybox_ipc::AgentState>,
    new_state: lazybox_ipc::AgentState,
    ready_for_prompt: bool,
    working_supersedes_dialog: impl FnOnce() -> bool,
    since_last_hook: Duration,
    staleness: Duration,
) -> bool {
    if since_last_hook >= staleness {
        let demotes_input_needed = current == Some(lazybox_ipc::AgentState::InputNeeded)
            && new_state == lazybox_ipc::AgentState::Working;
        return !demotes_input_needed || working_supersedes_dialog();
    }
    new_state == lazybox_ipc::AgentState::InputNeeded
        || (new_state == lazybox_ipc::AgentState::Idle && ready_for_prompt)
}

/// Hysteresis decision for the edge that LEAVES `InputNeeded`.
///
/// Claude's status-bar ticker can scroll the prompt out of the detect
/// window for a single chunk, flipping the reading to Idle even though
/// Claude is genuinely still waiting; without damping, the `?` pill
/// flickers off and back. But a (possibly wrong) `InputNeeded` must NOT
/// stick for the full hysteresis window once a CLEAR signal says
/// otherwise — a live `Working` status line or an affirmatively-drawn
/// idle composer. So the transient is damped ONLY when the new reading
/// is the ambiguous fall-through (`clear_exit_signal == false`); a
/// positive marker is honored immediately. Returns true to suppress.
fn should_suppress_input_needed_exit(
    current: Option<lazybox_ipc::AgentState>,
    new_state: lazybox_ipc::AgentState,
    clear_exit_signal: bool,
    since_last_input_needed: Option<std::time::Duration>,
    hysteresis: std::time::Duration,
) -> bool {
    current == Some(lazybox_ipc::AgentState::InputNeeded)
        && new_state != lazybox_ipc::AgentState::InputNeeded
        && !clear_exit_signal
        && since_last_input_needed.is_some_and(|e| e < hysteresis)
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

/// Idempotently create `path` (and parents). Used as the fallback when
/// git checkout can't run, and for re-validation when the persisted
/// session record points at a path that may have been removed by hand.
async fn ensure_dir_exists(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::create_dir_all(path).await;
}

/// If a stored Session points at a worktree path the user has since
/// removed (manual `rm -rf`, disk wipe, etc.), restore it. Tries a
/// real `git worktree add` first so the recovered tree carries the
/// PR's branch; falls back to a plain mkdir + ProviderError when git
/// can't help (no clone, branch missing, no network).
async fn ensure_worktree_present(
    config: &ServerConfig,
    workspace: &Workspace,
    path: &std::path::Path,
) {
    if path.exists() {
        return;
    }
    tracing::info!("worktree {} missing — re-provisioning", path.display());
    if let Err(e) = provision_worktree(workspace, path).await {
        tracing::warn!("re-provision failed: {e}");
        let _ = config.bus.send(Event::provider_error_retryable(
            "worktree",
            format!("re-checkout failed; using empty dir ({e})"),
        ));
        ensure_dir_exists(path).await;
    }
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
) -> Option<TerminalId> {
    let target = kind.singleton_key()?;
    let snapshot = snapshot_terminals(config).await;
    snapshot
        .iter()
        .find(|t| {
            t.session_key == *session_key && t.kind.singleton_key().as_deref() == Some(&target)
        })
        .map(|t| t.terminal_id)
}

/// Releases a claimed in-flight singleton identity when dropped — on
/// EVERY `handle_spawn` exit path (success, session-resolution failure,
/// backend failure, panic) — and pings waiters so collapsing duplicates
/// and `Kill` re-check promptly.
struct InflightSpawnGuard {
    set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>,
    changed: std::sync::Arc<tokio::sync::Notify>,
    key: (String, String),
}

impl InflightSpawnGuard {
    /// Claim `(workspace key, singleton kind key)` if free. `Ok(None)`
    /// for non-singleton kinds (shells spawn freely, no guard);
    /// `Err(())` when another spawn already holds the identity.
    fn try_claim(
        config: &ServerConfig,
        session_key: &SessionKey,
        kind: &TerminalKind,
    ) -> Result<Option<Self>, ()> {
        let Some(target) = kind.singleton_key() else {
            return Ok(None);
        };
        let key = (session_key.as_str().to_string(), target);
        let mut set = config
            .inflight_spawns
            .lock()
            .expect("inflight_spawns poisoned");
        if !set.insert(key.clone()) {
            return Err(());
        }
        Ok(Some(Self {
            set: config.inflight_spawns.clone(),
            changed: config.inflight_spawn_changed.clone(),
            key,
        }))
    }
}

impl Drop for InflightSpawnGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .expect("inflight_spawns poisoned")
            .remove(&self.key);
        self.changed.notify_waiters();
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
/// the winner fails (claim released, no terminal), drop the duplicate —
/// loudly when a prompt was lost with it.
async fn collapse_onto_inflight_spawn(
    config: &ServerConfig,
    session_key: &SessionKey,
    kind: &TerminalKind,
    prompt: Option<&str>,
) {
    tracing::info!(
        %session_key,
        ?kind,
        has_prompt = prompt.is_some(),
        "handle_spawn: a spawn for this singleton is already in flight — collapsing onto it",
    );
    let Some(existing) = await_inflight_singleton(config, session_key, kind).await else {
        tracing::warn!(
            %session_key,
            ?kind,
            "handle_spawn: in-flight spawn released without producing a terminal — dropping duplicate",
        );
        if prompt.is_some() {
            let _ = config.bus.send(Event::provider_error_retryable(
                "spawn",
                "an agent spawn was already in flight but failed — press the key again",
            ));
        }
        return;
    };
    if let Some(prompt) = prompt {
        // Boxed for the same reason as the existing-singleton path in
        // `handle_spawn`: `handle_inject_prompt`'s fallback arm can
        // recurse into `handle_spawn`. (No fallback passed here, so it
        // can't actually recurse.)
        Box::pin(handle_inject_prompt(config, existing, prompt, None)).await;
    }
    let _ = config.bus.send(Event::TerminalFocusRequested {
        terminal_id: existing,
    });
}

/// Wait for the in-flight winner's terminal to land in the live maps,
/// or for the winner to release its claim without one. Wakes on the
/// guard's `Notify` (with a periodic re-check so a missed ping can't
/// park the waiter), bounded by [`INFLIGHT_COLLAPSE_DEADLINE`].
async fn await_inflight_singleton(
    config: &ServerConfig,
    session_key: &SessionKey,
    kind: &TerminalKind,
) -> Option<TerminalId> {
    let target = kind.singleton_key()?;
    let claim = (session_key.as_str().to_string(), target.clone());
    let deadline = tokio::time::Instant::now() + INFLIGHT_COLLAPSE_DEADLINE;
    loop {
        if let Some(id) = live_singleton(config, session_key, &target).await {
            return Some(id);
        }
        let claimed = config
            .inflight_spawns
            .lock()
            .expect("inflight_spawns poisoned")
            .contains(&claim);
        if !claimed || tokio::time::Instant::now() >= deadline {
            // Winner released (or we timed out). One final scan closes
            // the insert→release window — the maps are populated before
            // the winner's guard drops, so a miss here means the spawn
            // genuinely failed.
            return live_singleton(config, session_key, &target).await;
        }
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            config.inflight_spawn_changed.notified(),
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
) -> Option<TerminalId> {
    let candidates: Vec<TerminalId> = {
        let meta = config.terminal_meta.lock().await;
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
    let terminals = config.terminals.lock().await;
    candidates.into_iter().find(|id| terminals.contains_key(id))
}

/// How long `Kill` waits for an in-flight spawn on the same workspace
/// before tearing down anyway. Bounded so a wedged provision can't make
/// the user's explicit Kill hang forever; past the cap the teardown
/// proceeds and the tombstone in `deleted_workspaces` stops the late
/// spawn from re-materializing in the daemon's cwd.
const KILL_INFLIGHT_WAIT: Duration = Duration::from_secs(30);

/// Serialize a workspace teardown against any spawn currently
/// provisioning into it. Without this, Kill could delete the worktree
/// mid-provision and the spawn would re-create it (plus a terminal)
/// right after teardown.
pub(crate) async fn await_inflight_spawns(config: &ServerConfig, workspace_key: &str) {
    let deadline = tokio::time::Instant::now() + KILL_INFLIGHT_WAIT;
    loop {
        let busy = config
            .inflight_spawns
            .lock()
            .expect("inflight_spawns poisoned")
            .iter()
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
            config.inflight_spawn_changed.notified(),
        )
        .await;
    }
}

/// Freeze every backend session belonging to `session_id`. Returns
/// the keys we froze so the caller can `resume` them after the
/// worktree move. With tmux the freeze detaches clients so the
/// inner shell can't read input mid-rename and print stale `pwd`;
/// other backends no-op cleanly.
///
/// Scoped to one session via the `terminal_sessions` map so an
/// unrelated workspace's runners don't pause for our migration.
async fn freeze_runners_in_session(
    config: &crate::ServerConfig,
    session_id: lazybox_core::SessionId,
) -> Vec<String> {
    // Lock-order: `terminals` before `terminal_sessions` per
    // `crate::TERMINAL_MAP_LOCK_ORDER`. This used to acquire them in
    // the opposite order, which inverted against every other call
    // site and created an AB/BA deadlock window if a pump-cleanup
    // path interleaved between the two acquires.
    let term_map = config.terminals.lock().await;
    let owners = config.terminal_sessions.lock().await;
    let keys: Vec<String> = owners
        .iter()
        .filter(|(_, sid)| **sid == session_id)
        .filter_map(|(tid, _)| term_map.get(tid).cloned())
        .collect();
    drop(owners);
    drop(term_map);
    for k in &keys {
        let _ = config.backend.freeze(k).await;
    }
    keys
}

/// PR-attach migration. Walks every session in `workspace`, checks
/// whether its persisted `worktree_path` matches what the current
/// slug would generate, and `git worktree move`s the mismatches.
/// Mutates `workspace` in place — the caller is responsible for
/// persistence + broadcast.
///
/// Running synchronously inside `polling::upsert` (rather than
/// fire-and-forget) closes the race window where consumers could
/// briefly see a stale `worktree_path` between attach + migration.
///
/// Live PTY processes inside the worktree keep their open dir handle
/// across the rename — POSIX `rename(2)` on a directory is atomic
/// and doesn't disturb existing inode references. Their `pwd` will
/// briefly print the old absolute path until they `cd .`. With the
/// tmux backend, `freeze_runners_in_session` detaches clients so
/// the inner shell can't even observe the rename mid-flight.
///
/// Returns whether any session was actually migrated. No-op when
/// every session already lives at the right place (most polls).
pub async fn migrate_session_paths_if_needed(
    config: &crate::ServerConfig,
    workspace: &mut Workspace,
) -> bool {
    let mut moved_any = false;
    // Sort sessions by created_at so the index assignment matches
    // what `worktree_path_for_session` expects (first = no suffix,
    // second = -2, etc.).
    let mut order: Vec<usize> = (0..workspace.sessions.len()).collect();
    order.sort_by_key(|&i| workspace.sessions[i].created_at);

    for (slot, sess_idx) in order.into_iter().enumerate() {
        let expected = worktree_path_for_session(workspace, slot);
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
        // Real move via git. Need owner + repo to find the bare clone.
        let Some(task) = workspace.primary_task() else {
            continue;
        };
        let Some(repo) = task.repo.as_deref() else {
            continue;
        };
        let Some((owner, name)) = repo.split_once('/') else {
            continue;
        };
        let mgr = lazybox_git_ops::WorktreeManager::default_base();
        let bare = mgr.bare_path(owner, name);
        if let Some(parent) = expected.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        // Freeze just this session's backend keys (not every backend
        // session in the process). The narrower scope means a busy
        // workspace's other sessions don't pause for an unrelated
        // migration.
        let session_id = workspace.sessions[sess_idx].id;
        let frozen_keys = freeze_runners_in_session(config, session_id).await;

        let result = mgr.move_worktree(&bare, &actual, &expected).await;

        for k in &frozen_keys {
            let _ = config.backend.resume(k).await;
        }

        match result {
            Ok(()) => {
                tracing::info!(
                    "migrated worktree {} → {}",
                    actual.display(),
                    expected.display()
                );
                workspace.sessions[sess_idx].worktree_path = expected;
                moved_any = true;
            }
            Err(e) => {
                tracing::warn!(
                    "git worktree move {} → {} failed: {e}",
                    actual.display(),
                    expected.display()
                );
                let _ = config
                    .bus
                    .send(lazybox_ipc::Event::provider_error_retryable(
                        "worktree",
                        format!("PR-attach migration failed: {e}"),
                    ));
            }
        }
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
/// `index = 0` → `<root>/<slug>` (no suffix, cleanest case).
/// `index = N` → `<root>/<slug>-{N+1}` so the second session is
/// `slug-2`, third is `slug-3`, …  Matches the user mental model
/// where session-counter starts at "no number".
fn worktree_path_for_session(workspace: &Workspace, index: usize) -> PathBuf {
    let mut name = workspace.worktree_slug();
    if index > 0 {
        name.push_str(&format!("-{}", index + 1));
    }
    worktree_root().join(name)
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

pub async fn handle_write(config: &ServerConfig, terminal_id: TerminalId, bytes: &[u8]) {
    let Some(key) = config.backend_key_for(terminal_id).await else {
        tracing::trace!("write to unknown terminal {terminal_id:?}");
        return;
    };
    if let Err(e) = config.backend.write(&key, bytes).await {
        tracing::warn!("backend write {key}: {e}");
    }
    // If the user just answered a prompt on an agent terminal that's
    // currently in `InputNeeded` state, optimistically flip it to
    // `Working` — the agent is about to act on the answer. The
    // detect_state loop will correct this on the next output chunk
    // (back to `InputNeeded` if the response turned out to be another
    // prompt, or to `Idle` once the agent goes quiet); but for the
    // common case the `?` pill clears immediately instead of lingering
    // through the 8s hysteresis window.
    //
    // An answer is either Enter (`\r`/`\n` — `y`/`yes`/`1`/<text> +
    // Enter; bracket-paste markers wrapping claude's submit count too)
    // OR a bare chooser keystroke: Claude's choosers accept a single
    // digit, y/n, or Esc (dismiss) with no Enter at all. Without the
    // bare-key arm, answering a chooser with `1` left the stale
    // markers pinning `InputNeeded` until fresh output evicted them.
    let pressed_enter = bytes.contains(&b'\r') || bytes.contains(&b'\n');
    let answered_chooser = bytes.len() == 1 && matches!(bytes[0], b'1'..=b'9' | b'y' | b'n' | 0x1b);
    if !pressed_enter && !answered_chooser {
        return;
    }
    if config.agent_state_for(terminal_id).await != Some(lazybox_ipc::AgentState::InputNeeded) {
        return;
    }
    // A bare chooser keystroke only ANSWERS chooser/permission-shaped
    // prompts. For a free-text elicitation, a lone digit / y / n is
    // just typing into the field — flipping the pill on it cleared a
    // real "agent is waiting on you". Enter is exempt: it submits the
    // elicitation answer, so the flip is correct. The shape is recorded
    // at detection time (PTY triggers are all chooser-shaped) and by
    // `handle_ingest_hook` (permission → chooser, elicit → free text);
    // with no recorded shape we conservatively don't flip on a bare key.
    if !pressed_enter {
        let shape = config
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
            return;
        }
    }
    let session_key = config
        .terminal_meta
        .lock()
        .await
        .get(&terminal_id)
        .map(|(sk, _)| sk.clone());
    let Some(session_key) = session_key else {
        return;
    };
    config
        .agent_states
        .lock()
        .await
        .insert(terminal_id, lazybox_ipc::AgentState::Working);
    // Tell the output pump to drop its detection buffer on the next
    // chunk. Without this the just-answered prompt's markers linger in
    // the rolling window and re-fire InputNeeded on the very next
    // chunk — reverting this optimistic flip and pinning the `?` pill
    // back on until ~16 KiB of fresh output finally evicts the stale
    // prompt. (The regression behind issue #101: "the ? won't go away
    // after I answer.")
    config.agent_detect_resets.lock().await.insert(terminal_id);
    tracing::debug!(
        ?terminal_id,
        pressed_enter,
        "user answered the prompt; optimistically flipping InputNeeded → Working"
    );
    let _ = config.bus.send(Event::AgentState {
        session_key,
        terminal_id,
        state: lazybox_ipc::AgentState::Working,
    });
}

/// How long the inject path waits for an active permission gate /
/// chooser to clear before giving up. These prompts are user-blocking,
/// so resolution is normally seconds; the bound only stops an abandoned
/// prompt from leaking the waiter task indefinitely.
const INJECT_INPUT_DEADLINE: Duration = Duration::from_secs(120);

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

/// Wait plumbing for [`confirm_prompt_submission`], registered BEFORE
/// the submit keystroke is written so a fast hook can't race the
/// waiter (`Notify::notify_one` stores a permit; the bus receiver is
/// subscribed up front for the same reason).
struct SubmitConfirmation {
    terminal_id: TerminalId,
    signal: std::sync::Arc<tokio::sync::Notify>,
    events: tokio::sync::broadcast::Receiver<Event>,
    signals_map: std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<TerminalId, std::sync::Arc<tokio::sync::Notify>>,
        >,
    >,
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
    let signal = std::sync::Arc::new(tokio::sync::Notify::new());
    config
        .prompt_submit_signals
        .lock()
        .await
        .insert(terminal_id, signal.clone());
    SubmitConfirmation {
        terminal_id,
        signal,
        events: config.bus.subscribe(),
        signals_map: config.prompt_submit_signals.clone(),
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
async fn confirm_prompt_submission(
    mut confirm: SubmitConfirmation,
    backend: &dyn crate::backend::SessionBackend,
    backend_key: &str,
    submit_bytes: &[u8],
    deadline: Duration,
) {
    let mut resends = 0u32;
    let confirmed = loop {
        let wait = deadline * (resends + 1);
        if await_submit_evidence(
            &confirm.signal,
            &mut confirm.events,
            confirm.terminal_id,
            wait,
        )
        .await
        {
            break true;
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
        if let Err(e) = backend.write(backend_key, submit_bytes).await {
            // The terminal is gone (or going) — nothing left to
            // confirm, and the loud give-up below would misreport a
            // parked prompt.
            tracing::warn!(
                terminal_id = ?confirm.terminal_id,
                "submit resend: backend.write failed: {e}"
            );
            break true;
        }
    };
    // Remove the registration only if it's still OURS. A second
    // injection on the same terminal replaces the map entry with its
    // own `Notify`; removing blindly here would delete THAT signal,
    // orphan its waiter, and trigger a spurious Enter resend into the
    // agent.
    {
        let mut signals = confirm.signals_map.lock().await;
        if signals
            .get(&confirm.terminal_id)
            .is_some_and(|s| std::sync::Arc::ptr_eq(s, &confirm.signal))
        {
            signals.remove(&confirm.terminal_id);
        }
    }
    if confirmed {
        return;
    }
    tracing::warn!(
        terminal_id = ?confirm.terminal_id,
        "prompt submit never confirmed after {SUBMIT_RESEND_LIMIT} Enter resends — \
         giving up; the prompt is likely parked in the composer",
    );
    let _ = confirm.bus.send(Event::provider_error_retryable(
        "inject_prompt",
        "the injected prompt looks parked unsubmitted in the agent's composer — \
         open the terminal and press Enter to start it",
    ));
}

/// Block until `terminal_id`'s output has been quiet for `quiet`, or
/// `cap` elapses — whichever comes first. Called between the paste
/// write and the submit keystroke so Enter is gated on evidence the
/// paste batch settled (the repaint it triggers has finished) instead
/// of a fixed sleep. `events` must be subscribed BEFORE the paste
/// write so the chunks it produces are observable here.
async fn await_paste_settled(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    terminal_id: TerminalId,
    quiet: Duration,
    cap: Duration,
) {
    let cap_at = tokio::time::Instant::now() + cap;
    let mut quiet_at = tokio::time::Instant::now() + quiet;
    loop {
        match tokio::time::timeout_at(quiet_at.min(cap_at), events.recv()).await {
            // Quiet window or the cap elapsed — settled either way.
            Err(_) => return,
            Ok(Ok(Event::TerminalOutput {
                terminal_id: tid, ..
            })) if tid == terminal_id => {
                quiet_at = tokio::time::Instant::now() + quiet;
            }
            Ok(Ok(_)) => {}
            // Chunks were dropped — can't tell when output stopped, so
            // conservatively restart the quiet window (the cap still
            // bounds the total wait).
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                quiet_at = tokio::time::Instant::now() + quiet;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return,
        }
    }
}

/// True when submission evidence arrived before `deadline`: the
/// per-terminal `UserPromptSubmit` signal, or an `Event::AgentState`
/// flipping this terminal to `Working`.
async fn await_submit_evidence(
    signal: &tokio::sync::Notify,
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    terminal_id: TerminalId,
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break false,
                    }
                }
            } => confirmed,
        }
    };
    matches!(tokio::time::timeout(deadline, wait).await, Ok(true))
}

/// Park until the agent on `terminal_id` leaves `InputNeeded` — i.e. the
/// permission gate / chooser / Y-N prompt it was blocked on has been
/// answered and it's safe to deliver an injected prompt. Returns `true`
/// once the agent reports any non-`InputNeeded` state, `false` if the
/// terminal exits first or `deadline` elapses while still blocked.
///
/// `events` must be subscribed BEFORE the caller reads the current state
/// so a transition that races the read isn't missed. On a `Lagged`
/// receiver (the very transition may have been dropped) the authoritative
/// `states` map is consulted rather than risk blocking to the deadline.
async fn wait_until_input_resolved(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    terminal_id: TerminalId,
    states: &std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<TerminalId, lazybox_ipc::AgentState>>,
    >,
    deadline: Duration,
) -> bool {
    let wait = async {
        loop {
            match events.recv().await {
                Ok(Event::AgentState {
                    terminal_id: tid,
                    state,
                    ..
                }) if tid == terminal_id => {
                    if state != lazybox_ipc::AgentState::InputNeeded {
                        return true;
                    }
                }
                Ok(Event::TerminalExited {
                    terminal_id: tid, ..
                }) if tid == terminal_id => return false,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if states.lock().await.get(&terminal_id)
                        != Some(&lazybox_ipc::AgentState::InputNeeded)
                    {
                        return true;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
            }
        }
    };
    matches!(tokio::time::timeout(deadline, wait).await, Ok(true))
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
) {
    // Look up — and drop the guard — before any further await so
    // a nested handle_spawn (in the fallback path) can re-acquire
    // the same lock without deadlocking. Without the explicit
    // scope, the temporary `MutexGuard` from the match scrutinee
    // lives for the entire match arm. The helpers acquire-then-drop
    // the lock inside one method call so no guard can outlive the
    // scrutinee.
    let backend_key = match config.backend_key_for(terminal_id).await {
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
                    fb.cwd,
                    prompt,
                    autonomous,
                )
                .await;
                return;
            }
            tracing::debug!("inject_prompt to unknown terminal {terminal_id:?}");
            return;
        }
    };
    let kind = match config.terminal_meta_for(terminal_id).await {
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
    let paste = agent.inject_prompt(prompt);
    let submit = agent.inject_submit();

    // Readiness gate (issue #32). If the agent is parked on a
    // permission gate / chooser / Y-N prompt, that dialog owns input —
    // it expects `y`/`n`/`1`/`2`, not a pasted prompt. Writing the
    // paste now feeds it into the dialog, which rejects it, and the
    // injection is silently lost. Claude emits these prompts at ANY
    // point in a session, not just at spawn, so the inject path needs
    // its own gate keyed on `InputNeeded`: wait for the prompt to
    // clear, then deliver the context.
    //
    // Subscribe BEFORE reading the current state so a transition that
    // races between the read and the wait isn't missed.
    let events = config.bus.subscribe();
    let blocked =
        config.agent_state_for(terminal_id).await == Some(lazybox_ipc::AgentState::InputNeeded);
    let backend = config.backend.clone();
    let states = config.agent_states.clone();
    let bus = config.bus.clone();
    let id = terminal_id;
    let config_for_confirm = config.clone();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + INJECT_INPUT_DEADLINE;
        let mut events = events;
        let mut blocked = blocked;
        while blocked {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero()
                || !wait_until_input_resolved(events, id, &states, remaining).await
            {
                tracing::warn!(
                    terminal_id = ?id,
                    "inject_prompt: agent still blocked on input after {INJECT_INPUT_DEADLINE:?}; dropping injection rather than feeding it into the prompt"
                );
                // The drop must be visible, not just a log line — the
                // user pressed `w` and their prompt evaporated.
                let _ = bus.send(Event::provider_error_retryable(
                    "inject_prompt",
                    "the agent stayed on a permission prompt, so the injected work \
                     context was dropped — answer the prompt and press w again",
                ));
                return;
            }
            // The release may be the optimistic InputNeeded → Working
            // flip from the user's keystroke, not a genuinely cleared
            // prompt (the answer could re-render another chooser).
            // Debounce, then re-check the cached state; if the prompt is
            // back, go around again. Re-subscribe BEFORE the re-read so
            // a transition racing the check isn't missed.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            events = bus.subscribe();
            blocked =
                states.lock().await.get(&id).copied() == Some(lazybox_ipc::AgentState::InputNeeded);
        }
        // Subscribed before the paste write so the output chunks the
        // paste triggers are observable by the settle gate.
        let output_events = submit.is_some().then(|| bus.subscribe());
        if let Err(e) = backend.write(&backend_key, &paste).await {
            tracing::warn!("inject_prompt: backend.write(paste) failed: {e}");
            return;
        }
        // Gate the submit keystroke on the paste's repaint going quiet
        // (Claude treats rapid bytes as a paste — Enter inside the
        // paste is a soft line break).
        if let (Some(submit_bytes), Some(mut output_events)) = (submit, output_events) {
            await_paste_settled(&mut output_events, id, PASTE_QUIET_WINDOW, PASTE_SETTLE_CAP).await;
            let confirm = prepare_submit_confirmation(&config_for_confirm, id).await;
            if let Err(e) = backend.write(&backend_key, &submit_bytes).await {
                tracing::warn!("inject_prompt: backend.write(submit) failed: {e}");
                return;
            }
            confirm_prompt_submission(
                confirm,
                &*backend,
                &backend_key,
                &submit_bytes,
                SUBMIT_CONFIRM_DEADLINE,
            )
            .await;
        }
    });
}

pub async fn handle_resize(config: &ServerConfig, terminal_id: TerminalId, cols: u16, rows: u16) {
    let Some(key) = config.backend_key_for(terminal_id).await else {
        return;
    };
    if let Err(e) = config.backend.resize(&key, cols, rows).await {
        tracing::warn!("backend resize {key}: {e}");
    }
}

/// Stop the session via the backend. The pump task drains the
/// remaining output chunks (if any), sees the stream close, and emits
/// `Event::TerminalExited` itself.
pub async fn handle_close(config: &ServerConfig, terminal_id: TerminalId) {
    let Some(key) = config.backend_key_for(terminal_id).await else {
        return;
    };
    if let Err(e) = config.backend.kill(&key).await {
        tracing::warn!("backend kill {key}: {e}");
    }
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
    let terminal_id = match backend_key.as_deref() {
        Some(key) => {
            let resolved = {
                let terminals = config.terminals.lock().await;
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
        None => {
            tracing::debug!(
                ?terminal_id,
                kind = ?hook.kind,
                "legacy terminal-id-only hook (pre-backend-key settings file), dropping"
            );
            return;
        }
    };
    // Resolve the workspace; a terminal mid-teardown (terminals entry
    // resolved but meta already swept) is dropped without marking
    // anything hook-driven.
    let session_key = {
        let meta = config.terminal_meta.lock().await;
        match meta.get(&terminal_id) {
            Some((sk, _)) => sk.clone(),
            None => {
                tracing::debug!(?terminal_id, kind = ?hook.kind, "hook for unknown terminal, dropping");
                return;
            }
        }
    };
    // From now on this terminal is hook-driven: the PTY detector defers
    // to hooks for Working/InputNeeded (until the timestamp recorded
    // here goes stale — see `HOOK_STALENESS`). Done even for events
    // that carry no state change (e.g. SessionStart) — the signal is
    // "this terminal speaks hooks", not the specific transition.
    config
        .hook_driven_terminals
        .lock()
        .await
        .insert(terminal_id, std::time::Instant::now());

    // Proof-of-submission signal for the prompt-inject paths: a
    // `UserPromptSubmit` hook means the injected prompt actually
    // entered Claude's turn (issue #122's failure is the prompt parked
    // in the composer, which fires nothing).
    if hook.kind == lazybox_ipc::HookEventKind::UserPromptSubmit
        && let Some(signal) = config.prompt_submit_signals.lock().await.get(&terminal_id)
    {
        signal.notify_one();
    }

    // Compare-and-set under one lock guard — a read under one
    // acquisition and an insert under another lets a concurrent PTY
    // pump transition slip between them and be clobbered. The hook →
    // state mapping consults the current state (an unrecognized
    // `Notification` is a no-change while `InputNeeded`), so it runs
    // under the same guard.
    let (prev, new_state, changed) = {
        let mut states = config.agent_states.lock().await;
        let prev = states.get(&terminal_id).copied();
        let Some(new_state) = lazybox_agents::hook::hook_to_state(&hook, prev) else {
            return;
        };
        let changed = prev != Some(new_state);
        if changed {
            states.insert(terminal_id, new_state);
        }
        (prev, new_state, changed)
    };
    // Record the prompt's shape — whether a bare chooser keystroke is a
    // complete answer — for `handle_write`'s optimistic flip. Done even
    // on a no-change re-assert (a chooser following an elicitation, or
    // vice versa, must update the gate), and OUTSIDE the states guard so
    // the two maps are never co-held.
    if new_state == lazybox_ipc::AgentState::InputNeeded {
        config.input_needed_shapes.lock().await.insert(
            terminal_id,
            lazybox_agents::hook::notification_prompt_shape(hook.notification.as_deref()),
        );
    }
    if !changed {
        return;
    }
    tracing::info!(
        ?terminal_id,
        %session_key,
        previous = ?prev,
        state = ?new_state,
        hook = ?hook.kind,
        "hook → broadcasting Event::AgentState",
    );
    let _ = config.bus.send(Event::AgentState {
        session_key,
        terminal_id,
        state: new_state,
    });
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
/// and the user can clean those up via Shift-X.
pub async fn recover_sessions(config: &ServerConfig) {
    let keys = match config.backend.list().await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("backend.list at startup: {e}");
            return;
        }
    };
    if keys.is_empty() {
        return;
    }
    tracing::info!("recovering {} surviving session(s)", keys.len());
    for key in keys {
        let (session_key, kind) = load_terminal_meta(config, &key)
            .await
            .unwrap_or_else(|| (SessionKey::from(""), TerminalKind::Shell));
        let no_permission = load_no_permission(config, &key).await;
        let terminal_id = alloc_terminal_id(&*config.store);
        config
            .terminals
            .lock()
            .await
            .insert(terminal_id, key.clone());
        // Populate terminal_meta so snapshot_terminals + the sidebar's
        // badge map see this PTY as belonging to its real workspace.
        // Without this the recovered terminal shows up as orphan and
        // nothing in the UI suggests it exists.
        config
            .terminal_meta
            .lock()
            .await
            .insert(terminal_id, (session_key.clone(), kind.clone()));
        if no_permission {
            config
                .no_permission_terminals
                .lock()
                .await
                .insert(terminal_id);
        }

        let bus = config.bus.clone();
        let backend = config.backend.clone();
        let terminals_map = config.terminals.clone();
        let terminal_meta_map = config.terminal_meta.clone();
        let no_permission_map = config.no_permission_terminals.clone();
        let key_for_pump = key.clone();
        // Broadcast Spawned before spawning the pump — same race
        // guard as the main spawn path.
        let _ = config.bus.send(Event::TerminalSpawned {
            terminal_id,
            session_key,
            kind,
            no_permission,
        });
        tokio::spawn(async move {
            let mut sub = match backend.subscribe(&key_for_pump).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("recover subscribe {key_for_pump}: {e}");
                    return;
                }
            };
            if !sub.replay.is_empty() {
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id,
                    bytes: sub.replay.clone(),
                    seq: sub.last_seq,
                });
            }
            while let Some(chunk) = sub.live.recv().await {
                let _ = bus.send(Event::TerminalOutput {
                    terminal_id,
                    bytes: chunk.bytes,
                    seq: chunk.seq,
                });
            }
            let exit_code = backend.wait_exit(&key_for_pump).await;
            let _ = bus.send(Event::TerminalExited {
                terminal_id,
                exit_code,
            });
            terminals_map.lock().await.remove(&terminal_id);
            terminal_meta_map.lock().await.remove(&terminal_id);
            no_permission_map.lock().await.remove(&terminal_id);
        });
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
    let payload = match serde_json::to_string(&(session_key.as_str(), kind)) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("persist terminal_meta: encode failed: {e}");
            return;
        }
    };
    if let Err(e) = config
        .store
        .set_kv(&format!("terminal:{backend_key}"), &payload)
    {
        tracing::warn!("persist terminal_meta: store write failed: {e}");
    }
}

/// Inverse of `persist_terminal_meta`. Returns None when nothing was
/// previously stored — caller falls back to a placeholder.
async fn load_terminal_meta(
    config: &ServerConfig,
    backend_key: &str,
) -> Option<(SessionKey, TerminalKind)> {
    let raw = config
        .store
        .get_kv(&format!("terminal:{backend_key}"))
        .ok()
        .flatten()?;
    let parsed: (String, TerminalKind) = serde_json::from_str(&raw).ok()?;
    Some((SessionKey::from(parsed.0.as_str()), parsed.1))
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
    if let Err(e) = config
        .store
        .set_kv(&format!("terminal-noperm:{backend_key}"), "1")
    {
        tracing::warn!("persist terminal no-permission flag: store write failed: {e}");
    }
}

/// Inverse of `persist_no_permission`. True when the surviving session
/// was launched in no-permission mode.
async fn load_no_permission(config: &ServerConfig, backend_key: &str) -> bool {
    config
        .store
        .get_kv(&format!("terminal-noperm:{backend_key}"))
        .ok()
        .flatten()
        .is_some()
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
    let entries: Vec<(TerminalId, String, SessionKey, TerminalKind)> = {
        let map = config.terminals.lock().await;
        let meta = config.terminal_meta.lock().await;
        map.iter()
            .filter_map(|(id, key)| {
                // Skip orphaned ids (terminals map says yes,
                // terminal_meta says no) — they should never exist in
                // steady state, only in a window during teardown.
                // Emitting a default-valued snapshot would feed the
                // TUI an empty-session-key workspace which the
                // sidebar would render as `(no repo)`.
                match meta.get(id).cloned() {
                    Some((sk, kind)) => Some((*id, key.clone(), sk, kind)),
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
    let no_permission = config.no_permission_terminals.lock().await.clone();

    let mut out = Vec::with_capacity(entries.len());
    for (id, key, session_key, kind) in entries {
        // Reconnecting `--connect` clients need the ring buffer so
        // their libghostty-vt can reconstruct the screen — without
        // it they see a blank terminal until the next chunk arrives.
        // Failure here is non-fatal: the snapshot is best-effort,
        // missing replay just degrades to the legacy behavior.
        //
        // The `timeout` is the load-bearing safety net: see
        // `SNAPSHOT_PER_SESSION_TIMEOUT`. Without it, one wedged tmux
        // pump can stall the daemon's Subscribe handler indefinitely,
        // and `tokio::select!` cannot poll the next command until this
        // arm returns. Stalling here = the entire IPC channel freezes.
        let (replay, last_seq) =
            match tokio::time::timeout(SNAPSHOT_PER_SESSION_TIMEOUT, config.backend.snapshot(&key))
                .await
            {
                Ok(Ok(snap)) => snap,
                Ok(Err(e)) => {
                    tracing::warn!(
                        terminal_id = ?id,
                        key = %key,
                        error = %e,
                        "snapshot_terminals: backend.snapshot failed — replay will be empty"
                    );
                    (Vec::new(), 0)
                }
                Err(_) => {
                    tracing::warn!(
                        terminal_id = ?id,
                        key = %key,
                        timeout_ms = SNAPSHOT_PER_SESSION_TIMEOUT.as_millis() as u64,
                        "snapshot_terminals: backend.snapshot timed out (wedged session?) \
                         — replay will be empty, daemon not blocked"
                    );
                    (Vec::new(), 0)
                }
            };
        out.push(TerminalSnapshot {
            no_permission: no_permission.contains(&id),
            terminal_id: id,
            session_key,
            kind,
            replay,
            last_seq,
        });
    }
    out
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

    // Snapshot live (session_key, kind) pairs so we can dedupe.
    let live: std::collections::HashSet<(String, String)> = {
        let meta = config.terminal_meta.lock().await;
        meta.values()
            .map(|(sk, k)| (sk.as_str().to_string(), kind_id(k)))
            .collect()
    };

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
                None,
                None,
                // Restored from a persisted session record, which
                // doesn't carry the autonomous flag — re-spawn with
                // prompts on rather than silently bypassing.
                false,
            )
            .await;
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

    fn input_resolved_states() -> std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<TerminalId, lazybox_ipc::AgentState>>,
    > {
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
    }

    #[tokio::test]
    async fn wait_until_input_resolved_releases_on_non_input_needed_state() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
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
        assert!(
            wait_until_input_resolved(rx, id, &input_resolved_states(), Duration::from_secs(1))
                .await
        );
    }

    #[tokio::test]
    async fn wait_until_input_resolved_gives_up_on_deadline_while_blocked() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let id = TerminalId(1);
        // Prompt stays up: only InputNeeded arrives, so the wait must
        // time out rather than write into the live prompt.
        tx.send(Event::AgentState {
            session_key: "ws:1".into(),
            terminal_id: id,
            state: lazybox_ipc::AgentState::InputNeeded,
        })
        .unwrap();
        assert!(
            !wait_until_input_resolved(rx, id, &input_resolved_states(), Duration::from_millis(80))
                .await
        );
        drop(tx);
    }

    #[tokio::test]
    async fn wait_until_input_resolved_returns_false_when_terminal_exits() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let id = TerminalId(1);
        tx.send(Event::TerminalExited {
            terminal_id: id,
            exit_code: Some(0),
        })
        .unwrap();
        assert!(
            !wait_until_input_resolved(rx, id, &input_resolved_states(), Duration::from_secs(1))
                .await
        );
    }

    #[test]
    fn hysteresis_damps_only_ambiguous_input_needed_exit() {
        use lazybox_ipc::AgentState::{Idle, InputNeeded, Working};
        let hyst = std::time::Duration::from_secs(8);
        let recent = Some(std::time::Duration::from_secs(1));
        let stale = Some(std::time::Duration::from_secs(9));

        // Ambiguous fall-through to Idle within the window → damp.
        assert!(should_suppress_input_needed_exit(
            Some(InputNeeded),
            Idle,
            false,
            recent,
            hyst,
        ));
        // A clear signal (live Working / affirmative idle composer) is
        // honored immediately, even within the window.
        assert!(!should_suppress_input_needed_exit(
            Some(InputNeeded),
            Working,
            true,
            recent,
            hyst,
        ));
        assert!(!should_suppress_input_needed_exit(
            Some(InputNeeded),
            Idle,
            true,
            recent,
            hyst,
        ));
        // Past the window → no damping regardless.
        assert!(!should_suppress_input_needed_exit(
            Some(InputNeeded),
            Idle,
            false,
            stale,
            hyst,
        ));
        // Never damp transitions that aren't leaving InputNeeded.
        assert!(!should_suppress_input_needed_exit(
            Some(Working),
            Idle,
            false,
            recent,
            hyst,
        ));
        // No prior InputNeeded timestamp → nothing to damp.
        assert!(!should_suppress_input_needed_exit(
            Some(InputNeeded),
            Idle,
            false,
            None,
            hyst,
        ));
    }

    /// PTY readings on a hook-driven terminal: fresh hooks own
    /// Working↔Idle, only the two corrections pass; stale hooks open
    /// the gate so the terminal degrades to scraping instead of
    /// freezing on the last hook state — except that demoting a
    /// hook-set `?` with a Working reading needs dialog-supersession
    /// evidence (a dialog blocks the hook stream, so stale + `?` is
    /// the normal shape of a REAL unanswered dialog).
    #[test]
    fn pty_reading_allowed_gates_on_hook_freshness() {
        use lazybox_ipc::AgentState::{Idle, InputNeeded, Working};
        let staleness = Duration::from_secs(30);
        let fresh = Duration::from_secs(1);
        let stale = Duration::from_secs(31);
        let supersedes = || true;
        let no_evidence = || false;

        // Fresh hooks: only the corrections pass, whatever the cache.
        assert!(pty_reading_allowed(
            None,
            InputNeeded,
            false,
            supersedes,
            fresh,
            staleness
        ));
        assert!(pty_reading_allowed(
            None, Idle, true, supersedes, fresh, staleness
        ));
        assert!(!pty_reading_allowed(
            None, Idle, false, supersedes, fresh, staleness
        ));
        assert!(!pty_reading_allowed(
            None, Working, false, supersedes, fresh, staleness
        ));
        assert!(!pty_reading_allowed(
            None, Working, true, supersedes, fresh, staleness
        ));

        // Stale hooks: full PTY fallback for everything…
        assert!(pty_reading_allowed(
            None,
            Working,
            false,
            no_evidence,
            stale,
            staleness
        ));
        assert!(pty_reading_allowed(
            Some(Working),
            Idle,
            false,
            no_evidence,
            stale,
            staleness
        ));
        assert!(pty_reading_allowed(
            None,
            InputNeeded,
            false,
            no_evidence,
            stale,
            staleness
        ));
        // …except Working demoting a hook-set `?`, which needs the
        // detector's affirmative "activity painted after the dialog
        // markers" evidence.
        assert!(!pty_reading_allowed(
            Some(InputNeeded),
            Working,
            false,
            no_evidence,
            stale,
            staleness
        ));
        assert!(pty_reading_allowed(
            Some(InputNeeded),
            Working,
            false,
            supersedes,
            stale,
            staleness
        ));
        // An InputNeeded re-assert and a ready-idle clear still pass.
        assert!(pty_reading_allowed(
            Some(InputNeeded),
            InputNeeded,
            false,
            no_evidence,
            stale,
            staleness
        ));
        assert!(pty_reading_allowed(
            Some(InputNeeded),
            Idle,
            true,
            no_evidence,
            stale,
            staleness
        ));
    }

    /// The in-flight spawn guard claims a singleton identity exactly
    /// once, never guards multi-instance kinds (shells), and releases
    /// on drop — including the early-return failure paths, which is the
    /// whole point of it being a drop guard.
    #[tokio::test]
    async fn inflight_guard_claims_once_and_releases_on_drop() {
        let config = ServerConfig::in_memory();
        let key: SessionKey = "test:ws-guard".into();
        let kind = TerminalKind::Agent("claude".into());

        let guard = InflightSpawnGuard::try_claim(&config, &key, &kind)
            .expect("first claim wins")
            .expect("agents are singletons");
        // Second claim on the same identity loses.
        assert!(InflightSpawnGuard::try_claim(&config, &key, &kind).is_err());
        // A different kind on the same workspace is a separate identity.
        assert!(
            InflightSpawnGuard::try_claim(&config, &key, &TerminalKind::Agent("codex".into()))
                .is_ok()
        );
        // Shells are never singletons — no guard, never blocked.
        assert!(matches!(
            InflightSpawnGuard::try_claim(&config, &key, &TerminalKind::Shell),
            Ok(None)
        ));
        drop(guard);
        // Released → claimable again.
        assert!(InflightSpawnGuard::try_claim(&config, &key, &kind).is_ok());
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
        let guard = InflightSpawnGuard::try_claim(&config, &key, &kind)
            .unwrap()
            .unwrap();

        let cfg = config.clone();
        let waiter = tokio::spawn(async move {
            await_inflight_spawns(&cfg, "test:ws-kill").await;
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
            await_inflight_spawns(&config, "test:ws-other"),
        )
        .await
        .expect("no in-flight spawn → no wait");
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

        let first = prepare_submit_confirmation(&config, id).await;
        // A second injection registers its own signal, replacing the
        // first's map entry.
        let second = prepare_submit_confirmation(&config, id).await;

        // The first exhausts its retries (no evidence) — but must NOT
        // remove the second's registration.
        confirm_prompt_submission(
            first,
            &*config.backend,
            &key,
            b"\r",
            Duration::from_millis(10),
        )
        .await;
        assert!(
            config.prompt_submit_signals.lock().await.contains_key(&id),
            "first confirmation must not remove the second's signal"
        );
        let resends = mock.writes_for(&key).await.len();
        assert_eq!(resends, SUBMIT_RESEND_LIMIT as usize);

        // The second's signal still works: a UserPromptSubmit-style
        // notify suppresses its resend, and its cleanup removes its own
        // registration.
        config
            .prompt_submit_signals
            .lock()
            .await
            .get(&id)
            .unwrap()
            .notify_one();
        confirm_prompt_submission(
            second,
            &*config.backend,
            &key,
            b"\r",
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(
            mock.writes_for(&key).await.len(),
            resends,
            "confirmed second submit must not resend"
        );
        assert!(
            config.prompt_submit_signals.lock().await.is_empty(),
            "second confirmation cleans up its own signal"
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

        let confirm = prepare_submit_confirmation(&config, id).await;
        let mut bus_rx = config.bus.subscribe();
        confirm_prompt_submission(
            confirm,
            &*config.backend,
            &key,
            b"\r",
            Duration::from_millis(10),
        )
        .await;

        assert_eq!(
            mock.writes_for(&key).await,
            vec![b"\r".to_vec(); SUBMIT_RESEND_LIMIT as usize],
            "only Enter resends, never the prompt body, bounded by the limit"
        );
        assert!(
            config.prompt_submit_signals.lock().await.is_empty(),
            "signal registration cleaned up"
        );
        let mut gave_up_loudly = false;
        while let Ok(ev) = bus_rx.try_recv() {
            if matches!(ev, Event::ProviderError { .. }) {
                gave_up_loudly = true;
            }
        }
        assert!(
            gave_up_loudly,
            "exhausting the resends must surface a user-visible error"
        );
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

        let confirm = prepare_submit_confirmation(&config, id).await;
        // What handle_ingest_hook does when UserPromptSubmit lands.
        // notify_one stores a permit, so firing before the wait is the
        // hard case this pins.
        config
            .prompt_submit_signals
            .lock()
            .await
            .get(&id)
            .unwrap()
            .notify_one();
        confirm_prompt_submission(
            confirm,
            &*config.backend,
            &key,
            b"\r",
            Duration::from_millis(100),
        )
        .await;

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
        confirm_prompt_submission(
            confirm,
            &*config.backend,
            &key,
            b"\r",
            Duration::from_millis(100),
        )
        .await;

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

        let confirm = prepare_submit_confirmation(&config, id).await;
        let mut bus_rx = config.bus.subscribe();
        // Stand-in for Claude finally taking the resent Enter: once the
        // first resend hits the backend, fire UserPromptSubmit.
        let signals = config.prompt_submit_signals.clone();
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
        confirm_prompt_submission(
            confirm,
            &*config.backend,
            &key,
            b"\r",
            Duration::from_millis(50),
        )
        .await;

        assert_eq!(
            mock.writes_for(&key).await,
            vec![b"\r".to_vec()],
            "the loop stops at the resend that got confirmed"
        );
        while let Ok(ev) = bus_rx.try_recv() {
            assert!(
                !matches!(ev, Event::ProviderError { .. }),
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
        confirm_prompt_submission(
            confirm,
            &*config.backend,
            &key,
            b"\r",
            Duration::from_millis(50),
        )
        .await;

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
        await_paste_settled(
            &mut events,
            TerminalId(1),
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .await;
        let elapsed = t0.elapsed();
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
                    seq,
                });
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let t0 = std::time::Instant::now();
        await_paste_settled(
            &mut events,
            id,
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
                    seq,
                });
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let t0 = std::time::Instant::now();
        await_paste_settled(
            &mut events,
            TerminalId(3),
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
                    seq,
                });
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let t0 = std::time::Instant::now();
        await_paste_settled(
            &mut events,
            id,
            Duration::from_millis(100),
            Duration::from_millis(200),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(elapsed >= Duration::from_millis(200));
        assert!(elapsed < Duration::from_secs(1), "the cap bounds the wait");
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
        let cwd = Some(std::path::PathBuf::from("/tmp/wt"));

        let with_skip = argv_for(&config, &kind, &cwd, true, None).expect("claude registered");
        assert_eq!(
            with_skip,
            vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string()
            ]
        );

        let without_skip = argv_for(&config, &kind, &cwd, false, None).expect("claude registered");
        assert_eq!(without_skip, vec!["claude".to_string()]);

        // With a generated hook settings file, `--settings <path>` is
        // appended so Claude reports state through structured hooks.
        let with_hooks = argv_for(
            &config,
            &kind,
            &cwd,
            false,
            Some(std::path::PathBuf::from("/run/hooks/settings-1.json")),
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

    /// The running test binary exists on disk, so resolution succeeds;
    /// the verified path is what gets baked into hook commands.
    #[test]
    fn hook_exe_resolves_to_existing_file() {
        let exe = hook_exe().expect("running test binary must resolve");
        assert!(exe.is_file());
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

    /// Through a real `/bin/sh`: an existing executable passes the guard
    /// and receives the hook-ingest argv (quoting survives the shell).
    #[cfg(unix)]
    #[test]
    fn hook_command_execs_existing_binary_via_sh() {
        let cmd = hook_command(Path::new("/bin/echo"), "lzb-sess-7");
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

    /// Through a real `/bin/sh`: a binary deleted after spawn produces a
    /// named lazybox error on stderr, not the shell's raw
    /// "No such file or directory".
    #[cfg(unix)]
    #[test]
    fn hook_command_missing_binary_reports_named_error() {
        let gone = "/nonexistent/target/debug/lazybox";
        let cmd = hook_command(Path::new(gone), "lzb-sess-9");
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &cmd])
            .output()
            .expect("sh runs");
        assert_eq!(out.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("lazybox hook: binary missing at /nonexistent/target/debug/lazybox"),
            "stderr should name the cause: {stderr}"
        );
        assert!(
            !stderr.contains("No such file or directory"),
            "raw shell error leaked: {stderr}"
        );
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
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Unknown,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        }
    }

    /// Issue spawns get a deterministic `lazybox/issue-<n>` branch so
    /// pressing the spawn key twice on the same issue lands on the
    /// same branch instead of accumulating orphans.
    #[test]
    fn derive_branch_for_branchless_github_issue() {
        let t = task_for("github", "acme/widget#42");
        assert_eq!(derive_branch_for_branchless(&t), "lazybox/issue-42");
    }

    /// Linear / non-GitHub keys go through the sanitizer fallback so
    /// any odd characters become dashes and the source prefix keeps
    /// branches namespaced per-provider.
    #[test]
    fn derive_branch_for_branchless_linear() {
        let t = task_for("linear", "ENG-456");
        assert_eq!(derive_branch_for_branchless(&t), "lazybox/linear-eng-456");
    }

    /// A non-numeric GitHub key (no `#`) falls through to the
    /// sanitizer instead of producing `lazybox/issue-`.
    #[test]
    fn derive_branch_for_branchless_github_without_hash() {
        let t = task_for("github", "acme/widget");
        assert_eq!(
            derive_branch_for_branchless(&t),
            "lazybox/github-acme-widget"
        );
    }

    /// Blank-workspace branches come from the workspace key, so two
    /// spawns on the same workspace reuse one branch.
    #[test]
    fn derive_branch_for_workspace_uses_workspace_key() {
        let ws = Workspace::empty(WorkspaceKey::new("my-experiment"), "main", Utc::now());
        assert_eq!(derive_branch_for_workspace(&ws), "lazybox/my-experiment");
    }

    /// A blank workspace under a GitHub project recovers `owner/repo`
    /// from the project key, so its Claude sessions get a real clone
    /// instead of an empty directory.
    #[test]
    fn clonable_repo_from_project_recovers_github_owner_repo() {
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github(
            "AntoineToussaint",
            "lazybox",
        ));
        assert_eq!(
            clonable_repo_from_project(&ws).unwrap(),
            "AntoineToussaint/lazybox"
        );
    }

    /// `local-` projects have no upstream repo — the lookup errors so
    /// the caller's empty-dir fallback stays their outcome.
    #[test]
    fn clonable_repo_from_project_rejects_local_project() {
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::local("my-experiment"));
        assert!(clonable_repo_from_project(&ws).is_err());
    }

    #[test]
    fn clonable_repo_from_project_errs_without_project_or_task() {
        let ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        assert!(clonable_repo_from_project(&ws).is_err());
    }

    /// End-to-end through `provision_worktree`: a blank workspace under
    /// a local project must fail fast (no git invocation possible) so
    /// `handle_spawn` falls back to a plain mkdir.
    #[tokio::test]
    async fn provision_worktree_blank_local_workspace_errors() {
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::local("notes"));
        let dir = std::env::temp_dir().join("lazybox-test-blank-local");
        assert!(provision_worktree(&ws, &dir).await.is_err());
        assert!(!dir.exists(), "failed provisioning must not create the dir");
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
}
