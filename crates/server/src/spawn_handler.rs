//! Wires the IPC `Spawn`/`Write`/`Resize`/`Close` commands to the
//! [`SessionBackend`](crate::backend::SessionBackend) trait. The
//! server itself owns no PTY state — every backend-side operation
//! goes through `config.backend`.
//!
//! ## Per-process state on `ServerConfig`
//!
//! `ServerConfig::terminals` maps wire `TerminalId` → backend session
//! key. Multiple connections (in-process channel + a remote SSH
//! `pilot --connect`) share this map so they see the same set.
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
use pilot_agents::SpawnCtx;
use pilot_core::{
    SessionId, SessionKey, SessionKind, Task, Workspace, WorkspaceKey, WorkspaceSession as Session,
};
use pilot_ipc::{Event, TerminalId, TerminalKind, TerminalSnapshot};
use pilot_store::WorkspaceRecord;
use std::path::PathBuf;
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

fn alloc_terminal_id() -> TerminalId {
    TerminalId(NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed))
}

/// Build the argv for `kind`. None means we don't know how to spawn
/// it (unknown agent id, etc.) — handled by emitting a ProviderError.
fn argv_for(
    config: &ServerConfig,
    kind: &TerminalKind,
    cwd: &Option<PathBuf>,
    skip_permissions: bool,
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
    // Autonomous sessions (e.g. `@pilot`-triggered work) launch with
    // tool-use permission prompts disabled so the agent runs unattended
    // — there's no human nearby to approve. Gated by config so a
    // paranoid user can force prompts on every session. Interactive
    // spawns never bypass: the prompt IS the human-in-the-loop guard.
    // The flag works under both Claude subscription login and an API
    // key; the only bypass restriction is no-root/sudo, which the
    // worktree sessions satisfy.
    let cfg = pilot_config::Config::load().unwrap_or_default();
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
    // Singleton enforcement at the daemon (the source of truth for
    // who's running what). The TUI also intercepts duplicates
    // client-side for snappy focus-not-spawn behavior, but that
    // alone fails the moment a second client connects to the same
    // daemon. The guard here protects the invariant for everyone:
    // at most one Claude per session, one Codex per session, etc.
    if let Some(existing) = find_existing_singleton(config, &session_key, &kind).await {
        tracing::info!(
            terminal_id = ?existing,
            "handle_spawn: existing singleton found, sending TerminalFocusRequested"
        );
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
    let (cwd_path, owning_session): (Option<PathBuf>, Option<pilot_core::SessionId>) =
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
    let argv = match argv_for(config, &kind, &cwd_path, skip_permissions) {
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
    // `tmux ls` shows something like `pilot-github-acme-widget-126-claude-NNNN`
    // instead of `pilot-4`. Backends append their own uniqueness
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
    tracing::info!(%backend_key, "handle_spawn: backend.spawn ok");

    let terminal_id = alloc_terminal_id();
    // Insert the auxiliary maps BEFORE the primary `terminals` map.
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
    // next pilot start can reattach surviving tmux sessions to their
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
    let terminal_meta_map = config.terminal_meta.clone();
    let no_permission_map = config.no_permission_terminals.clone();
    let store_for_pump = config.store.clone();
    let id_for_pump = terminal_id;
    let key_for_pump = backend_key.clone();
    let agent_for_pump: Option<std::sync::Arc<dyn pilot_agents::Agent>> = match &kind {
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
        // Hysteresis: timestamp of the last Asking detection.
        // When detect_state returns Active and the previous state
        // was Asking, we ONLY transition to Active if it's been
        // long enough since we last saw Asking patterns — gives
        // the buffer time to capture genuine new output (user
        // typed a response, Claude is now streaming back), rather
        // than treating a ticker chunk that scrolled the prompt
        // out of buffer as "agent done."
        let mut last_asking_at: Option<std::time::Instant> = None;
        const ASKING_HYSTERESIS: std::time::Duration = std::time::Duration::from_secs(8);

        async fn maybe_emit_state_change(
            agent: Option<&std::sync::Arc<dyn pilot_agents::Agent>>,
            buf: &mut Vec<u8>,
            bytes: &[u8],
            states: &std::sync::Arc<
                tokio::sync::Mutex<std::collections::HashMap<TerminalId, pilot_ipc::AgentState>>,
            >,
            bus: &tokio::sync::broadcast::Sender<Event>,
            id: TerminalId,
            session_key: &SessionKey,
            last_asking_at: &mut Option<std::time::Instant>,
            hysteresis: std::time::Duration,
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
            let Some(new_state) = agent.detect_state(detect_window) else {
                return;
            };
            // Trace-level on steady-state runs (claude emits 100+
            // chunks/sec during streaming and we don't want to drown
            // the log). Only ELEVATE to debug-level on every Asking
            // detection so a missing `?` pill is easy to bisect from
            // the log without re-running with full trace verbosity.
            // Toggle full trace via `RUST_LOG=pilot_server=trace`.
            tracing::trace!(
                terminal_id = ?id,
                buf_len = buf.len(),
                detected = ?new_state,
                "detect_state ran",
            );
            if new_state == pilot_ipc::AgentState::Asking {
                tracing::debug!(
                    terminal_id = ?id,
                    buf_len = buf.len(),
                    tail_tip = %String::from_utf8_lossy(
                        &detect_window[detect_window.len().saturating_sub(120)..]
                    ),
                    "detect_state → Asking",
                );
            }
            if new_state == pilot_ipc::AgentState::Asking {
                *last_asking_at = Some(std::time::Instant::now());
            }
            let current = {
                let map = states.lock().await;
                map.get(&id).copied()
            };
            // Hysteresis. Claude's status-bar updates make the
            // detector miss the prompt for one chunk, then catch
            // it on the next. Without this guard the pill flickers
            // every few seconds while Claude is genuinely still
            // waiting.
            if current == Some(pilot_ipc::AgentState::Asking)
                && new_state == pilot_ipc::AgentState::Active
                && let Some(t) = last_asking_at
                && t.elapsed() < hysteresis
            {
                tracing::debug!(
                    terminal_id = ?id,
                    "state hysteresis: suppressing Asking → Active (only {:?} since last Asking)",
                    t.elapsed(),
                );
                return;
            }
            if current == Some(new_state) {
                return;
            }
            states.lock().await.insert(id, new_state);
            // Loud log so when the user reports "the pill didn't
            // show", we can confirm whether the daemon-side
            // detector actually fired vs. the event got lost
            // somewhere downstream. Keyed off TerminalId so
            // grep-ing the log file makes the path obvious.
            tracing::info!(
                terminal_id = ?id,
                %session_key,
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
                    signal.notify_waiters();
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
                &mut last_asking_at,
                ASKING_HYSTERESIS,
            )
            .await;
            let _ = bus.send(Event::TerminalOutput {
                terminal_id: id_for_pump,
                bytes: sub.replay.clone(),
                seq: sub.last_seq,
            });
            first_output_signal_for_pump.notify_waiters();
            signaled_first_output = true;
            check_ready(&state_buf, &mut signaled_ready, &ready_signal_for_pump);
        }
        while let Some(chunk) = sub.live.recv().await {
            maybe_emit_state_change(
                agent_for_pump.as_ref(),
                &mut state_buf,
                &chunk.bytes,
                &agent_states_map,
                &bus,
                id_for_pump,
                &session_key_for_pump,
                &mut last_asking_at,
                ASKING_HYSTERESIS,
            )
            .await;
            if !signaled_first_output {
                first_output_signal_for_pump.notify_one();
                signaled_first_output = true;
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
        terminal_meta_map.lock().await.remove(&id_for_pump);
        no_permission_map.lock().await.remove(&id_for_pump);
        let _ = store_for_pump.delete_kv(&format!("terminal:{key_for_pump}"));
        let _ = store_for_pump.delete_kv(&format!("terminal-noperm:{key_for_pump}"));
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
            //   2. first_output + SETTLE — if no agent override
            //      for detect_ready_for_prompt, or claude renders
            //      the input box without our detector matching,
            //      we still write after 600ms past first byte.
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

            // Race the tight ready_signal against the broad
            // first_output + settle fallback. Whichever fires
            // first wins; if neither, HARD_DEADLINE caps the wait.
            let ready_notify = ready_signal.notified();
            let first_output_notify = first_output.notified();
            tokio::select! {
                _ = tokio::time::timeout(HARD_DEADLINE, ready_notify) => {
                    tracing::info!(
                        terminal_id = ?id,
                        "initial_prompt: ready signal fired — writing immediately",
                    );
                }
                _ = async {
                    // Fallback: wait for first output, then SETTLE.
                    // This catches agents without a
                    // detect_ready_for_prompt override AND covers
                    // detector misses (e.g. Claude renders the
                    // input box in a way our pattern doesn't
                    // match yet).
                    let _ = tokio::time::timeout(HARD_DEADLINE, first_output_notify).await;
                    tokio::time::sleep(SETTLE).await;
                } => {
                    tracing::info!(
                        terminal_id = ?id,
                        "initial_prompt: first-output + settle path — writing now",
                    );
                }
            }
            tracing::info!(
                terminal_id = ?id,
                paste_len = paste.len(),
                "initial_prompt: writing paste to backend",
            );
            if let Err(e) = backend.write(&backend_key, &paste).await {
                tracing::warn!(
                    terminal_id = ?id,
                    "initial_prompt: backend.write(paste) failed: {e}"
                );
                return;
            }
            // Paste/submit split. Agents like Claude Code batch rapid
            // byte arrival as a paste; Enter inside that batch is a
            // soft line break, not a submit. Sending the submit
            // keystroke after a beat lets the paste settle so Enter
            // fires as its own keystroke. Agents that don't need a
            // separate submit (the default trait impl) return None
            // here and we skip the second write entirely.
            if let Some(submit_bytes) = submit {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if let Err(e) = backend.write(&backend_key, &submit_bytes).await {
                    tracing::warn!(
                        terminal_id = ?id,
                        "initial_prompt: backend.write(submit) failed: {e}"
                    );
                }
            }
        });
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
        let path = pilot_core::paths::sandbox_dir(workspace_key.as_str());
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
    let mut workspace = match load_workspace(config, &workspace_key) {
        Ok(w) => w,
        Err(_) => {
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

    let provisioned = provision_worktree(&workspace, &path).await;
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
/// - `github:owner/repo#42` → `pilot/issue-42`
/// - `linear:ENG-456`       → `pilot/linear-eng-456`
/// - anything else          → `pilot/<source>-<sanitized-key>`
fn derive_branch_for_branchless(task: &Task) -> String {
    let source = task.id.source.to_ascii_lowercase();
    let raw_key = &task.id.key;

    if source == "github" {
        if let Some(hash_idx) = raw_key.rfind('#') {
            let number = &raw_key[hash_idx + 1..];
            if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
                return format!("pilot/issue-{number}");
            }
        }
    }

    let sanitized: String = raw_key
        .chars()
        .map(|c| match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    format!("pilot/{source}-{trimmed}")
}

/// Try to set up a real git worktree at `target` for the workspace's
/// primary task. Returns Ok(()) when a checkout succeeded, Err when
/// we couldn't (caller falls back to a plain mkdir).
async fn provision_worktree(
    workspace: &Workspace,
    target: &std::path::Path,
) -> Result<(), crate::ServerError> {
    use crate::ServerError;
    let task = workspace
        .primary_task()
        .ok_or_else(|| ServerError::Workspace("workspace has no primary task".into()))?;
    let repo = task
        .repo
        .as_deref()
        .ok_or_else(|| ServerError::Workspace("task has no repo".into()))?;
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| ServerError::Workspace(format!("repo '{repo}' is not owner/name")))?;

    let mgr = pilot_git_ops::WorktreeManager::default_base();
    let worktree = match task.branch.as_deref() {
        Some(branch) => mgr
            .checkout_at(target, owner, name, branch)
            .await
            .map_err(|e| ServerError::Worktree(format!("checkout_at: {e}")))?,
        None => {
            // Issue (or other branchless task): cut a fresh branch
            // off the repo default. Branch name encodes the task key
            // so two spawns on the same issue land on the same branch
            // and subsequent presses are idempotent — without that,
            // pressing `c` twice on issue #42 would create
            // `pilot/issue-42-…` and `pilot/issue-42-…-2`, neither of
            // which corresponds to a PR the user can push.
            let new_branch = derive_branch_for_branchless(task);
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
    // error so a broken `~/.pilot/config.yaml` shows up loudly in
    // `/tmp/pilot.log` instead of users wondering why their mounts
    // stopped working after an edit.
    let cfg = match pilot_config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                repo = %format!("{owner}/{name}"),
                "Config::load failed (mounts will be skipped): {e}",
            );
            pilot_config::Config::default()
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
    // failed surfaces in /tmp/pilot.log.
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
fn config_mounts_to_git(specs: &[pilot_config::MountSpec]) -> Vec<pilot_git_ops::Mount> {
    specs
        .iter()
        .map(|m| pilot_git_ops::Mount {
            source: expand_tilde(&m.source),
            link_at: m.link_at.clone(),
            placement: match m.placement {
                pilot_config::PlacementSpec::Inside => pilot_git_ops::Placement::Inside,
                pilot_config::PlacementSpec::Above => pilot_git_ops::Placement::Above,
            },
        })
        .collect()
}

/// Convert per-config `ScriptSpec` → git-ops `Script`, expanding
/// `~/` in source paths. Specs with neither `content` nor `source`
/// set, or with both set, are skipped with a warning — we don't
/// want a bad entry in YAML to abort every script's install.
fn config_scripts_to_git(specs: &[pilot_config::ScriptSpec]) -> Vec<pilot_git_ops::Script> {
    specs
        .iter()
        .filter_map(|s| match (&s.content, &s.source) {
            (Some(body), None) => Some(pilot_git_ops::Script {
                name: s.name.clone(),
                body: pilot_git_ops::ScriptBody::Inline(body.clone()),
            }),
            (None, Some(path)) => Some(pilot_git_ops::Script {
                name: s.name.clone(),
                body: pilot_git_ops::ScriptBody::Linked(expand_tilde(path)),
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
    let cfg = match pilot_config::Config::load() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    env_for_repo(&cfg, &repo)
}

/// Whether a spawn should launch in no-permission / bypass mode.
/// Only autonomous (pilot-spawned) sessions are eligible, and only
/// when the `agent.autonomous_skip_permissions` toggle is on (default).
/// Pure so tests don't need a real YAML on disk.
pub(crate) fn skip_permissions_for(autonomous: bool, cfg: &pilot_config::Config) -> bool {
    autonomous && cfg.agent.autonomous_skip_permissions
}

/// Pure-data lookup so tests don't need a real YAML on disk.
pub(crate) fn env_for_repo(cfg: &pilot_config::Config, repo: &str) -> Vec<(String, String)> {
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
    session_id: pilot_core::SessionId,
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
        let mgr = pilot_git_ops::WorktreeManager::default_base();
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
                let _ = config.bus.send(pilot_ipc::Event::provider_error_retryable(
                    "worktree",
                    format!("PR-attach migration failed: {e}"),
                ));
            }
        }
    }

    moved_any
}

/// Root directory for every workspace's worktrees. Sits under the v2
/// state root next to `state.db` so a single `rm -rf ~/.pilot/v2/`
/// wipes everything pilot owns on disk. Override the parent via the
/// `PILOT_HOME` env var (see `pilot_core::paths`).
pub fn worktree_root() -> PathBuf {
    pilot_core::paths::worktrees_root()
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
    // If the user just sent Enter (`\r` or `\n`) to an agent
    // terminal that's currently in `Asking` state, optimistically
    // flip it back to `Active`. The detect_state loop will re-fire
    // `Asking` on the next output chunk if the agent's response
    // turned out to be another prompt; but for the common case
    // (user typed `y`/`yes`/`1`/<text> + Enter), the `?` pill
    // disappears immediately instead of lingering through the 8s
    // hysteresis window. Bracket-paste markers (`ESC[200~` / `ESC[201~`)
    // count too — those wrap claude's submit at the end.
    if !bytes.contains(&b'\r') && !bytes.contains(&b'\n') {
        return;
    }
    if config.agent_state_for(terminal_id).await != Some(pilot_ipc::AgentState::Asking) {
        return;
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
        .insert(terminal_id, pilot_ipc::AgentState::Active);
    tracing::debug!(
        ?terminal_id,
        "user pressed Enter; optimistically clearing Asking → Active"
    );
    let _ = config.bus.send(Event::AgentState {
        session_key,
        terminal_id,
        state: pilot_ipc::AgentState::Active,
    });
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
    fallback_spawn: Option<pilot_ipc::SpawnFallback>,
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
                handle_spawn(
                    config,
                    fb.session_key,
                    fb.session_id,
                    fb.kind,
                    fb.cwd,
                    Some(prompt.to_string()),
                    // `w`-driven inject is a user action — keep prompts on.
                    false,
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

    // NO Asking-wait in this path. Two earlier iterations of this
    // code waited for `agent_states[id] != Asking` before injecting,
    // motivated by the spawn-time race where Claude's "Trust this
    // folder? y/n" permission prompt would eat the first char of
    // the inject. That race is real but it only happens at SPAWN —
    // by the time the user is pressing `w` to inject into an
    // EXISTING claude session, the permission gate has been past
    // for a while.
    //
    // The Asking detector is intentionally permissive (matches
    // claude's idle main prompt via the "last line ends with `?`"
    // heuristic so the `?` sidebar pill surfaces "you're owed
    // input"). That same permissiveness makes it a bad gate for
    // inject: it stays Asking on the normal idle screen, so an
    // inject-wait waited the full 60s deadline EVERY time + then
    // injected after. From the user's perspective: pressing `w`
    // did nothing.
    //
    // For the spawn-time race we keep the wait in `handle_spawn`
    // where `Asking` near t=0 actually means a permission prompt.
    // For inject into a long-running claude, just write the paste.
    let backend = config.backend.clone();
    tokio::spawn(async move {
        if let Err(e) = backend.write(&backend_key, &paste).await {
            tracing::warn!("inject_prompt: backend.write(paste) failed: {e}");
            return;
        }
        // 200ms gap so the paste batch settles before Enter fires
        // (Claude treats rapid bytes as a paste — Enter inside the
        // paste is a soft line break).
        if let Some(submit_bytes) = submit {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let Err(e) = backend.write(&backend_key, &submit_bytes).await {
                tracing::warn!("inject_prompt: backend.write(submit) failed: {e}");
            }
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

/// Bind already-running backend sessions to fresh wire TerminalIds.
/// Called once at server startup so pilot restarts don't lose the
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
        let terminal_id = alloc_terminal_id();
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
/// after a pilot restart.
async fn persist_terminal_meta(
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
/// a pilot restart can re-render the indicator for surviving sessions
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
/// because there should be a claude running. Without this, a pilot
/// restart leaves a stale-looking sidebar with the terminal stack
/// reading "(no terminals)".
///
/// Per-session, per-pilot-lifetime: we only relaunch sessions that
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
                pilot_core::SessionKind::Agent { agent_id } => {
                    TerminalKind::Agent(agent_id.clone())
                }
                pilot_core::SessionKind::Shell => TerminalKind::Shell,
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
        let mut cfg = pilot_config::Config::default();
        let mut env = std::collections::BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "postgres://x".to_string());
        env.insert("OPENAI_API_KEY".to_string(), "sk-test".to_string());
        cfg.repos.insert(
            "acme/widget".into(),
            pilot_config::RepoConfig {
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
        let cfg = pilot_config::Config::default();
        assert!(env_for_repo(&cfg, "no/such-repo").is_empty());
    }

    #[test]
    fn skip_permissions_only_for_autonomous_when_enabled() {
        let mut cfg = pilot_config::Config::default();
        // Default config has the toggle on.
        assert!(cfg.agent.autonomous_skip_permissions);

        // Autonomous + toggle on → bypass.
        assert!(skip_permissions_for(true, &cfg));
        // Interactive never bypasses, even with the toggle on — the
        // prompt is the human-in-the-loop guard.
        assert!(!skip_permissions_for(false, &cfg));

        // Paranoid user flips the toggle off → no session bypasses.
        cfg.agent.autonomous_skip_permissions = false;
        assert!(!skip_permissions_for(true, &cfg));
        assert!(!skip_permissions_for(false, &cfg));
    }

    #[test]
    fn env_for_repo_case_sensitive() {
        let mut cfg = pilot_config::Config::default();
        let mut env = std::collections::BTreeMap::new();
        env.insert("X".into(), "1".into());
        cfg.repos.insert(
            "Owner/Repo".into(),
            pilot_config::RepoConfig {
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
            pilot_config::MountSpec {
                source: std::path::PathBuf::from("/a"),
                link_at: std::path::PathBuf::from("inside"),
                placement: pilot_config::PlacementSpec::Inside,
            },
            pilot_config::MountSpec {
                source: std::path::PathBuf::from("/b"),
                link_at: std::path::PathBuf::from("above"),
                placement: pilot_config::PlacementSpec::Above,
            },
        ];
        let mounts = config_mounts_to_git(&specs);
        assert_eq!(mounts.len(), 2);
        assert!(matches!(
            mounts[0].placement,
            pilot_git_ops::Placement::Inside
        ));
        assert!(matches!(
            mounts[1].placement,
            pilot_git_ops::Placement::Above
        ));
    }

    fn task_for(source: &str, key: &str) -> Task {
        Task {
            id: pilot_core::TaskId {
                source: source.into(),
                key: key.into(),
            },
            title: "t".into(),
            body: None,
            state: pilot_core::TaskState::Open,
            role: pilot_core::TaskRole::Author,
            ci: pilot_core::CiStatus::default(),
            review: pilot_core::ReviewStatus::default(),
            checks: vec![],
            unread_count: 0,
            url: String::new(),
            repo: Some("acme/widget".into()),
            branch: None,
            base_branch: None,
            updated_at: chrono::Utc::now(),
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: pilot_core::Mergeable::Unknown,
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

    /// Issue spawns get a deterministic `pilot/issue-<n>` branch so
    /// pressing the spawn key twice on the same issue lands on the
    /// same branch instead of accumulating orphans.
    #[test]
    fn derive_branch_for_branchless_github_issue() {
        let t = task_for("github", "acme/widget#42");
        assert_eq!(derive_branch_for_branchless(&t), "pilot/issue-42");
    }

    /// Linear / non-GitHub keys go through the sanitizer fallback so
    /// any odd characters become dashes and the source prefix keeps
    /// branches namespaced per-provider.
    #[test]
    fn derive_branch_for_branchless_linear() {
        let t = task_for("linear", "ENG-456");
        assert_eq!(derive_branch_for_branchless(&t), "pilot/linear-eng-456");
    }

    /// A non-numeric GitHub key (no `#`) falls through to the
    /// sanitizer instead of producing `pilot/issue-`.
    #[test]
    fn derive_branch_for_branchless_github_without_hash() {
        let t = task_for("github", "acme/widget");
        assert_eq!(derive_branch_for_branchless(&t), "pilot/github-acme-widget");
    }
}
