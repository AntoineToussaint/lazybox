use crate::ServerConfig;
use lazybox_core::{SessionId, SessionKey};
use lazybox_ipc::{AgentAuthPhase, AgentRunAccess, Event, TerminalId, TerminalKind, UserPrompt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

const AUTH_REPLAY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Env for the provider's own auth subprocesses (`login`, `login status`).
/// Empty on purpose: they must read and write the SAME credential home the
/// agent itself will use, and that is the daemon's inherited environment —
/// its `CODEX_HOME` when set, else the provider's own default. Adding a
/// credential-home override here would re-fork the login that #1656 merged
/// back together, so there is deliberately nothing to add.
const AUTH_ENV: &[(String, String)] = &[];

/// Ceiling on the provider's status probe (`codex login status`,
/// `claude auth status --json`). These read a local credential file and
/// return in milliseconds, so this is not a performance budget — it is a
/// liveness one. The probe is drained until its output channel CLOSES, which
/// only happens when the child exits, and `cancel_reauthentication` kills the
/// flow's registered process exactly once (when the user cancels) — so a
/// probe spawned after that point, or one that simply never exits (a `codex`
/// wrapper that waits on stdin), would otherwise hang the recovery flow for
/// good: the pane stays `authenticating` and a second Esc does nothing.
const AUTH_STATUS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Test-only shortening of [`AUTH_STATUS_PROBE_TIMEOUT`], in milliseconds; 0
/// means "use the real one". The wedge tests need the bound to actually
/// elapse, and driving that with `tokio::time::advance` under a paused clock
/// proved platform-dependent — it passed on macOS and failed on Linux CI,
/// because a `yield_now` wait loop keeps the runtime non-idle and the
/// interaction with the resume path's own timers differs. A genuinely short
/// real timeout is deterministic everywhere. Deliberately generous (see the
/// setter) so that if it ever leaked to a sibling test — `cargo test` shares
/// one process, unlike `cargo nextest` — that test's probe, which completes in
/// microseconds against the in-memory mock, still could not race it.
#[cfg(test)]
static AUTH_STATUS_PROBE_TIMEOUT_MS_OVERRIDE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn auth_status_probe_timeout() -> std::time::Duration {
    #[cfg(test)]
    {
        let ms = AUTH_STATUS_PROBE_TIMEOUT_MS_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            return std::time::Duration::from_millis(ms);
        }
    }
    AUTH_STATUS_PROBE_TIMEOUT
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct AgentResumeContext {
    pub terminal_id: TerminalId,
    pub session_key: SessionKey,
    pub session_id: Option<SessionId>,
    pub agent_id: String,
    pub cwd: PathBuf,
    pub backend_key: Option<String>,
    pub on_main: bool,
    pub model_alias: Option<String>,
    pub access: AgentRunAccess,
    pub no_permission: bool,
    pub provider_session_id: Option<String>,
    pub prompt_history: Vec<UserPrompt>,
    pub composing_buffer: Option<String>,
}

#[derive(Clone)]
struct AuthFlow {
    agent_id: String,
    phase: AgentAuthPhase,
    terminal_id: TerminalId,
    terminal_backend_key: Option<String>,
    auth_process_key: Option<String>,
    output: Option<lazybox_ipc::EventSender>,
    cancelled: bool,
}

#[derive(Clone)]
struct FailedAuth {
    terminal_id: TerminalId,
    display_name: String,
    error: String,
    backend_key: Option<String>,
    output: Option<lazybox_ipc::EventSender>,
}

#[derive(Debug, Clone)]
struct RequiredAuth {
    agent_id: String,
    display_name: String,
    reason: String,
    other_session_count: usize,
}

#[derive(Clone, Default)]
pub(crate) struct AgentRecoveryRegistry {
    contexts: Arc<Mutex<HashMap<TerminalId, AgentResumeContext>>>,
    flows: Arc<Mutex<HashMap<TerminalId, AuthFlow>>>,
    provider_flows: Arc<Mutex<HashMap<String, TerminalId>>>,
    failures: Arc<Mutex<HashMap<TerminalId, FailedAuth>>>,
    requirements: Arc<Mutex<HashMap<TerminalId, RequiredAuth>>>,
}

impl AgentRecoveryRegistry {
    pub(crate) async fn remember_spawn(&self, context: AgentResumeContext) {
        self.contexts
            .lock()
            .await
            .insert(context.terminal_id, context);
    }

    pub(crate) async fn context(&self, terminal_id: TerminalId) -> Option<AgentResumeContext> {
        self.contexts.lock().await.get(&terminal_id).cloned()
    }

    pub(crate) async fn mark_exited(
        &self,
        terminal_id: TerminalId,
        backend_key: &str,
        prompt_history: Vec<UserPrompt>,
        composing_buffer: Option<String>,
    ) {
        if let Some(context) = self.contexts.lock().await.get_mut(&terminal_id)
            && context.backend_key.as_deref() == Some(backend_key)
        {
            context.backend_key = None;
            context.prompt_history = prompt_history;
            context.composing_buffer = composing_buffer;
        }
    }

    pub(crate) async fn update_provider_session(
        &self,
        terminal_id: TerminalId,
        provider_session_id: String,
    ) {
        if let Some(context) = self.contexts.lock().await.get_mut(&terminal_id) {
            context.provider_session_id = Some(provider_session_id);
        }
    }

    async fn update_conversation(
        &self,
        terminal_id: TerminalId,
        prompt_history: Vec<UserPrompt>,
        composing_buffer: Option<String>,
    ) {
        if let Some(context) = self.contexts.lock().await.get_mut(&terminal_id) {
            context.prompt_history = prompt_history;
            context.composing_buffer = composing_buffer;
        }
    }

    pub(crate) fn rebadge_blocking(&self, terminal_ids: &[TerminalId], to: &SessionKey) {
        let mut contexts = self.contexts.blocking_lock();
        for terminal_id in terminal_ids {
            if let Some(context) = contexts.get_mut(terminal_id) {
                context.session_key = to.clone();
            }
        }
    }

    pub(crate) async fn forget(&self, terminal_id: TerminalId) {
        self.contexts.lock().await.remove(&terminal_id);
        self.failures.lock().await.remove(&terminal_id);
        self.requirements.lock().await.remove(&terminal_id);
    }

    async fn require(
        &self,
        terminal_id: TerminalId,
        agent_id: String,
        display_name: String,
        reason: String,
        other_session_count: usize,
    ) -> bool {
        let mut requirements = self.requirements.lock().await;
        if requirements.contains_key(&terminal_id) {
            return false;
        }
        requirements.insert(
            terminal_id,
            RequiredAuth {
                agent_id,
                display_name,
                reason,
                other_session_count,
            },
        );
        true
    }

    async fn is_required(&self, terminal_id: TerminalId) -> bool {
        self.requirements.lock().await.contains_key(&terminal_id)
    }

    async fn begin(
        &self,
        terminal_id: TerminalId,
        agent_id: &str,
        current_terminal_id: TerminalId,
        current_backend_key: Option<String>,
        output: Option<lazybox_ipc::EventSender>,
    ) -> Result<(), TerminalId> {
        let mut providers = self.provider_flows.lock().await;
        if let Some(owner) = providers.get(agent_id) {
            return Err(*owner);
        }
        providers.insert(agent_id.to_string(), terminal_id);
        drop(providers);
        self.flows.lock().await.insert(
            terminal_id,
            AuthFlow {
                agent_id: agent_id.to_string(),
                phase: AgentAuthPhase::LoginInteractive,
                terminal_id: current_terminal_id,
                terminal_backend_key: current_backend_key,
                auth_process_key: None,
                output,
                cancelled: false,
            },
        );
        Ok(())
    }

    async fn take_failure(&self, terminal_id: TerminalId) -> Option<FailedAuth> {
        self.failures.lock().await.remove(&terminal_id)
    }

    async fn failed_current(
        &self,
        terminal_id: TerminalId,
    ) -> Option<(TerminalId, Option<String>)> {
        self.failures
            .lock()
            .await
            .get(&terminal_id)
            .map(|failure| (failure.terminal_id, failure.backend_key.clone()))
    }

    async fn failure_for_terminal(
        &self,
        terminal_id: TerminalId,
    ) -> Option<(TerminalId, FailedAuth)> {
        self.failures
            .lock()
            .await
            .iter()
            .find_map(|(recovery_terminal_id, failure)| {
                (failure.terminal_id == terminal_id)
                    .then(|| (*recovery_terminal_id, failure.clone()))
            })
    }

    async fn record_failure(
        &self,
        terminal_id: TerminalId,
        current_terminal_id: TerminalId,
        display_name: String,
        error: String,
        backend_key: Option<String>,
        output: Option<lazybox_ipc::EventSender>,
    ) {
        if self.contexts.lock().await.contains_key(&terminal_id) {
            self.failures.lock().await.insert(
                terminal_id,
                FailedAuth {
                    terminal_id: current_terminal_id,
                    display_name,
                    error,
                    backend_key,
                    output,
                },
            );
        }
    }

    async fn set_phase(&self, terminal_id: TerminalId, phase: AgentAuthPhase) {
        if let Some(flow) = self.flows.lock().await.get_mut(&terminal_id) {
            flow.phase = phase;
        }
    }

    async fn set_auth_process(&self, terminal_id: TerminalId, backend_key: Option<String>) {
        if let Some(flow) = self.flows.lock().await.get_mut(&terminal_id) {
            flow.auth_process_key = backend_key;
        }
    }

    async fn set_current_terminal(
        &self,
        terminal_id: TerminalId,
        current_terminal_id: TerminalId,
        backend_key: Option<String>,
    ) {
        if let Some(flow) = self.flows.lock().await.get_mut(&terminal_id) {
            flow.terminal_id = current_terminal_id;
            flow.terminal_backend_key = backend_key;
        }
    }

    async fn current_terminal(&self, terminal_id: TerminalId) -> TerminalId {
        self.flows
            .lock()
            .await
            .get(&terminal_id)
            .map_or(terminal_id, |flow| flow.terminal_id)
    }

    async fn output(&self, terminal_id: TerminalId) -> Option<lazybox_ipc::EventSender> {
        self.flows
            .lock()
            .await
            .get(&terminal_id)
            .and_then(|flow| flow.output.clone())
    }

    async fn cancel(&self, terminal_id: TerminalId) -> Option<String> {
        let mut flows = self.flows.lock().await;
        let flow = flows.get_mut(&terminal_id)?;
        flow.cancelled = true;
        flow.auth_process_key.clone()
    }

    async fn is_cancelled(&self, terminal_id: TerminalId) -> bool {
        self.flows
            .lock()
            .await
            .get(&terminal_id)
            .is_some_and(|flow| flow.cancelled)
    }

    async fn finish(&self, terminal_id: TerminalId) {
        let flow = self.flows.lock().await.remove(&terminal_id);
        if let Some(flow) = flow {
            let mut providers = self.provider_flows.lock().await;
            if providers.get(&flow.agent_id) == Some(&terminal_id) {
                providers.remove(&flow.agent_id);
            }
        }
    }

    pub(crate) async fn active(&self, terminal_id: TerminalId) -> bool {
        self.flows
            .lock()
            .await
            .iter()
            .any(|(recovery_terminal_id, flow)| {
                *recovery_terminal_id == terminal_id || flow.terminal_id == terminal_id
            })
    }

    /// Whether this terminal is somewhere in the auth-required detour: a
    /// provider auth failure has been recorded (`require`) and not yet
    /// cleared, or a re-authentication flow is running for it. Auto-wait
    /// consults this so a usage-limit park that coincides with an auth expiry
    /// is held through the login/re-auth rather than having a continuation
    /// pasted into the logged-out screen.
    pub(crate) async fn auth_pending(&self, terminal_id: TerminalId) -> bool {
        self.is_required(terminal_id).await || self.active(terminal_id).await
    }

    pub(crate) async fn replay_events(
        &self,
        reconnect_output: Option<&lazybox_ipc::EventSender>,
    ) -> (Vec<Event>, Vec<(TerminalId, String)>) {
        let context_ids: std::collections::HashSet<_> =
            self.contexts.lock().await.keys().copied().collect();
        let mut replay_backends = Vec::new();
        let flows = {
            let mut flows = self.flows.lock().await;
            if let Some(output) = reconnect_output {
                for flow in flows.values_mut() {
                    if flow
                        .output
                        .as_ref()
                        .is_none_or(lazybox_ipc::EventSender::is_closed)
                    {
                        flow.output = Some(output.clone());
                        if let Some(backend_key) = &flow.terminal_backend_key {
                            replay_backends.push((flow.terminal_id, backend_key.clone()));
                        }
                    }
                }
            }
            flows.clone()
        };
        let failures = {
            let mut failures = self.failures.lock().await;
            if let Some(output) = reconnect_output {
                for failure in failures.values_mut() {
                    if failure
                        .output
                        .as_ref()
                        .is_none_or(lazybox_ipc::EventSender::is_closed)
                    {
                        failure.output = Some(output.clone());
                        if let Some(backend_key) = &failure.backend_key {
                            replay_backends.push((failure.terminal_id, backend_key.clone()));
                        }
                    }
                }
            }
            failures.clone()
        };
        let requirements = self.requirements.lock().await.clone();
        let mut events: Vec<_> = flows
            .iter()
            .filter_map(|(terminal_id, flow)| {
                context_ids
                    .contains(terminal_id)
                    .then_some(Event::AgentAuthProgress {
                        recovery_terminal_id: *terminal_id,
                        terminal_id: flow.terminal_id,
                        phase: flow.phase,
                    })
            })
            .collect();
        events.extend(
            failures
                .iter()
                .filter(|(terminal_id, _)| {
                    context_ids.contains(terminal_id) && !flows.contains_key(terminal_id)
                })
                .map(|(terminal_id, failure)| Event::AgentAuthFinished {
                    recovery_terminal_id: *terminal_id,
                    terminal_id: failure.terminal_id,
                    display_name: failure.display_name.clone(),
                    success: false,
                    error: Some(failure.error.clone()),
                }),
        );
        events.extend(
            requirements
                .iter()
                .filter(|(terminal_id, _)| {
                    context_ids.contains(terminal_id)
                        && !flows.contains_key(terminal_id)
                        && !failures.contains_key(terminal_id)
                })
                .map(|(terminal_id, required)| Event::AgentAuthRequired {
                    terminal_id: *terminal_id,
                    agent_id: required.agent_id.clone(),
                    display_name: required.display_name.clone(),
                    reason: required.reason.clone(),
                    other_session_count: required.other_session_count,
                }),
        );
        (events, replay_backends)
    }

    async fn shared_checkout_is_ambiguous(&self, context: &AgentResumeContext) -> bool {
        self.contexts.lock().await.values().any(|other| {
            other.terminal_id != context.terminal_id
                && other.backend_key.is_some()
                && other.agent_id == context.agent_id
                && other.cwd == context.cwd
        })
    }
}

pub(crate) async fn detect_required(
    config: &ServerConfig,
    terminal_id: TerminalId,
    reason: &'static str,
) {
    let Some(context) = config.agent_recovery.context(terminal_id).await else {
        return;
    };
    if config.agent_recovery.active(terminal_id).await {
        return;
    }
    // Every agent shares one machine-wide login, so a re-auth always touches
    // what the rest of the fleet reads. Count the other running sessions of
    // this agent so the prompt can name what is riding on it.
    let other_session_count = {
        let entries = config.terminal.entries.lock().await;
        entries
            .iter()
            .filter(|(id, entry)| {
                **id != terminal_id
                    && !entry.superseded
                    && !entry.authenticating
                    && entry.meta.as_ref().is_some_and(|(_, kind)| {
                        matches!(kind, TerminalKind::Agent(agent_id) if agent_id == &context.agent_id)
                    })
            })
            .count()
    };
    let display_name = config
        .agents
        .get(&context.agent_id)
        .map(|agent| agent.display_name().to_string())
        .unwrap_or_else(|| context.agent_id.clone());
    let reason = reason.to_string();
    if !config
        .agent_recovery
        .require(
            terminal_id,
            context.agent_id.clone(),
            display_name.clone(),
            reason.clone(),
            other_session_count,
        )
        .await
    {
        return;
    }
    let _ = config.bus.send(Event::AgentAuthRequired {
        terminal_id,
        agent_id: context.agent_id,
        display_name,
        reason,
        other_session_count,
    });
}

pub(crate) async fn resume_agent(
    config: &ServerConfig,
    terminal_id: TerminalId,
) -> Option<TerminalId> {
    resume_agent_with_prompt(config, terminal_id, None).await
}

/// Stop a usage-limit-blocked agent's process, respawn the same
/// conversation in its pane (`--resume`), and submit the continuation
/// prompt once the fresh composer is ready — the "restart with fresh
/// credentials" half of rate-limit recovery. A plain "continue" (`Shift-K`)
/// cannot make a running process re-read its credentials after the user
/// switched account / API key externally; only a respawn does. The kill →
/// detach → resume sequence is the one the re-auth flow uses for a blocked
/// pane, minus the interactive login step (the user has already re-authed
/// outside lazybox), and the nudge rides the spawn-time injector rather
/// than a blind keystroke so it waits for the booted composer. A pane
/// without launch metadata, or one mid re-authentication, is rejected
/// rather than half-restarted.
pub(crate) async fn restart_agent_and_continue(config: &ServerConfig, terminal_id: TerminalId) {
    let reject = |message: String| Event::CommandRejected {
        command: "RestartAgentAndContinue".into(),
        message,
    };
    let Some(context) = config.agent_recovery.context(terminal_id).await else {
        let _ = config.bus.send(reject(
            "this agent pane has no resumable launch metadata".into(),
        ));
        return;
    };
    // Bail if a re-auth flow is mid-flight for this terminal so a kill+respawn
    // can't stomp an in-progress interactive login. We read `active()` but
    // deliberately do NOT register ourselves as active. Two invariants make
    // that safe, and both are load-bearing:
    //   1. This runs fully inline on the per-terminal FIFO I/O lane
    //      (`run_io_lane`, keyed by `terminal_id`) — no `tokio::spawn`, no lane
    //      release — so a second command for THIS terminal (another restart, a
    //      re-auth) queues behind this call and cannot interleave. The guard
    //      only has to catch a re-auth *background task* started earlier, which
    //      registered itself via `begin()` and outlives the lane hop.
    //   2. We must not call `begin()` here: it claims a global lock keyed by
    //      `agent_id`, so a concurrent bulk `a R` of two panes running the same
    //      agent (each on its own terminal lane) would have all but the first
    //      rejected. Registering would break the headline bulk-restart path.
    // If either invariant changes (this path spawns, or the lane stops being
    // per-terminal), two restarts could double-kill/double-spawn — revisit then.
    if config.agent_recovery.active(terminal_id).await {
        let _ = config.bus.send(reject(
            "a re-authentication is already running for this agent".into(),
        ));
        return;
    }
    if let Some(backend_key) = context.backend_key.clone() {
        // Carry the conversation (history + any draft) across the swap
        // exactly as the re-auth flow does before it kills the pane.
        if let Some((prompt_history, composing_buffer)) =
            crate::spawn_handler::capture_terminal_conversation_state(config, terminal_id).await
        {
            config
                .agent_recovery
                .update_conversation(terminal_id, prompt_history, composing_buffer)
                .await;
        }
        let killed = {
            let _guard = config.terminal.lock_terminal_io(&backend_key).await;
            config.backend.kill(&backend_key).await
        };
        if let Err(error) = killed {
            let _ = config
                .bus
                .send(reject(format!("could not stop the agent: {error}")));
            return;
        }
        crate::spawn_handler::detach_killed_terminal(
            config,
            terminal_id,
            &backend_key,
            crate::working_claims::ClaimRelease::Project,
        )
        .await;
        config.backend.release(&backend_key).await;
    }
    tracing::info!(
        ?terminal_id,
        agent = %context.agent_id,
        "restart-rate-limited: respawning the agent to pick up fresh credentials"
    );
    resume_agent_with_prompt(config, terminal_id, Some(crate::auto_wait::resume_prompt())).await;
}

async fn resume_agent_with_prompt(
    config: &ServerConfig,
    terminal_id: TerminalId,
    initial_prompt: Option<String>,
) -> Option<TerminalId> {
    let Some(context) = config.agent_recovery.context(terminal_id).await else {
        let _ = config.bus.send(Event::AgentAuthFinished {
            recovery_terminal_id: terminal_id,
            terminal_id,
            display_name: "Agent".into(),
            success: false,
            error: Some("this agent pane no longer has resumable launch metadata".into()),
        });
        return None;
    };
    if context.provider_session_id.is_none()
        && (context.on_main
            || config
                .agent_recovery
                .shared_checkout_is_ambiguous(&context)
                .await)
    {
        let display_name = config
            .agents
            .get(&context.agent_id)
            .map(|agent| agent.display_name().to_string())
            .unwrap_or_else(|| context.agent_id.clone());
        let _ = config.bus.send(Event::AgentResumeFallback {
            terminal_id,
            display_name,
        });
    }
    let replaced_terminal_id = config.agent_recovery.current_terminal(terminal_id).await;
    let replacement = crate::spawn_handler::handle_spawn(
        config,
        context.session_key.clone(),
        context.session_id,
        TerminalKind::Agent(context.agent_id.clone()),
        crate::spawn_handler::SpawnOptions {
            cwd: Some(context.cwd.to_string_lossy().into_owned()),
            initial_prompt,
            on_main: context.on_main,
            model_alias: context.model_alias.clone(),
            resume: true,
            provider_session_id: context.provider_session_id.clone(),
            no_permission_override: Some(context.no_permission),
            replace_terminal_id: Some(replaced_terminal_id),
            prompt_history: context.prompt_history.clone(),
            composing_buffer: context.composing_buffer.clone(),
            access: context.access,
            ..Default::default()
        },
    )
    .await;
    if replacement.is_some() {
        config.agent_recovery.forget(terminal_id).await;
    }
    replacement
}

pub(crate) async fn start_reauthentication(
    config: &ServerConfig,
    terminal_id: TerminalId,
    output: Option<lazybox_ipc::EventSender>,
) {
    let Some(context) = config.agent_recovery.context(terminal_id).await else {
        let _ = config.bus.send(Event::AgentAuthFinished {
            recovery_terminal_id: terminal_id,
            terminal_id,
            display_name: "Agent".into(),
            success: false,
            error: Some("this agent pane is no longer recoverable".into()),
        });
        return;
    };
    if !config.agent_recovery.is_required(terminal_id).await {
        let _ = config.bus.send(Event::CommandRejected {
            command: "ReauthenticateAgent".into(),
            message: "the agent has not reported a provider authentication failure".into(),
        });
        return;
    }
    let Some(agent) = config.agents.get(&context.agent_id) else {
        return;
    };
    let display_name = agent_display_name(config, &context.agent_id);
    let Some(commands) = agent.auth_commands() else {
        let _ = config.bus.send(Event::AgentAuthFinished {
            recovery_terminal_id: terminal_id,
            terminal_id,
            display_name,
            success: false,
            error: Some("this agent does not support interactive authentication".into()),
        });
        return;
    };
    let (current_terminal_id, current_backend_key) = config
        .agent_recovery
        .failed_current(terminal_id)
        .await
        .unwrap_or((terminal_id, context.backend_key.clone()));
    if let Err(owner) = config
        .agent_recovery
        .begin(
            terminal_id,
            &context.agent_id,
            current_terminal_id,
            current_backend_key,
            output,
        )
        .await
    {
        if owner == terminal_id {
            return;
        }
        let _ = config.bus.send(Event::AgentAuthFinished {
            recovery_terminal_id: terminal_id,
            terminal_id,
            display_name,
            success: false,
            error: Some("another authentication flow is already running for this provider".into()),
        });
        return;
    }
    let config = config.clone();
    tokio::spawn(async move {
        run_reauthentication(config, context, commands).await;
    });
}

pub(crate) async fn cancel_reauthentication(config: &ServerConfig, terminal_id: TerminalId) {
    if let Some(backend_key) = config.agent_recovery.cancel(terminal_id).await {
        let _ = config.backend.kill(&backend_key).await;
    }
}

pub(crate) async fn close_failed_auth_terminal(
    config: &ServerConfig,
    terminal_id: TerminalId,
) -> Option<Result<(), String>> {
    let Some((recovery_terminal_id, failure)) = config
        .agent_recovery
        .failure_for_terminal(terminal_id)
        .await
    else {
        return None;
    };
    if let Some(backend_key) = failure.backend_key {
        if let Err(error) = config.backend.kill(&backend_key).await {
            return Some(Err(error.to_string()));
        }
        crate::spawn_handler::detach_killed_terminal(
            config,
            terminal_id,
            &backend_key,
            crate::working_claims::ClaimRelease::Project,
        )
        .await;
        config.backend.release(&backend_key).await;
    }
    config.agent_recovery.forget(recovery_terminal_id).await;
    Some(Ok(()))
}

pub(crate) async fn replay_auth_output(
    config: &ServerConfig,
    output: &lazybox_ipc::EventSender,
    backends: Vec<(TerminalId, String)>,
) {
    for (terminal_id, backend_key) in backends {
        if let Ok(Ok(snapshot)) =
            tokio::time::timeout(AUTH_REPLAY_TIMEOUT, config.backend.snapshot(&backend_key)).await
        {
            let _ = output.send(Event::AgentAuthReplay {
                terminal_id,
                replay: snapshot.replay,
                seq: snapshot.last_seq,
                sizes: snapshot.sizes,
            });
        }
    }
}

async fn run_reauthentication(
    config: ServerConfig,
    context: AgentResumeContext,
    commands: lazybox_agents::AgentAuthCommands,
) {
    let recovery_terminal_id = context.terminal_id;
    let display_name = agent_display_name(&config, &context.agent_id);
    // Every agent lazybox drives keeps ONE machine-wide login, shared by every
    // other running session of that agent AND the user's own interactive
    // pane. Running the provider `logout` there signs all of them out at once
    // — the acute bug (#1376). So lazybox never runs it: a pane's recovery is
    // a login-only refresh of the shared credential, and there is no code path
    // here that can invalidate it.
    //
    // That is not an absolute guarantee the user cannot end up logged out.
    // `login` is the provider's own subprocess: if the user cancels it
    // (`cancel_reauthentication` kills it) after it has cleared the credential
    // to begin a fresh sign-in, the shared login can be left empty. That
    // window is inherent to a shared login and cannot be closed from here, so
    // instead of leaving it silent, a cancel that lands on an empty shared
    // login is detected and named (see `cancelled_login_error`) — the user is
    // told the machine-wide login needs signing in again rather than
    // discovering it one failing pane at a time.
    //
    // The common case — an already-valid login the user re-triggered — is
    // protected because `login` exiting 0 is not trusted on its own: the
    // status gate below confirms the credential really is valid before the
    // conversation is resumed.
    let previous_failure = config
        .agent_recovery
        .take_failure(recovery_terminal_id)
        .await;
    let current_terminal_id = previous_failure
        .as_ref()
        .map_or(recovery_terminal_id, |failure| failure.terminal_id);
    let current_backend_key = previous_failure
        .as_ref()
        .and_then(|failure| failure.backend_key.clone())
        .or_else(|| context.backend_key.clone());
    config
        .agent_recovery
        .set_current_terminal(
            recovery_terminal_id,
            current_terminal_id,
            current_backend_key.clone(),
        )
        .await;
    if config
        .agent_recovery
        .is_cancelled(recovery_terminal_id)
        .await
    {
        finish_failure(
            &config,
            recovery_terminal_id,
            current_terminal_id,
            &display_name,
            "authentication was cancelled".into(),
            current_backend_key,
        )
        .await;
        return;
    }
    if config
        .agent_recovery
        .is_cancelled(recovery_terminal_id)
        .await
    {
        finish_failure(
            &config,
            recovery_terminal_id,
            current_terminal_id,
            &display_name,
            "authentication was cancelled".into(),
            current_backend_key,
        )
        .await;
        return;
    }
    config
        .agent_recovery
        .set_phase(recovery_terminal_id, AgentAuthPhase::LoginInteractive)
        .await;
    let login_key = match config
        .backend
        .spawn(&commands.login, Some(&context.cwd), AUTH_ENV, "agent-auth")
        .await
    {
        Ok(key) => key,
        Err(error) => {
            finish_failure(
                &config,
                recovery_terminal_id,
                current_terminal_id,
                &display_name,
                format!("provider login could not start: {error}"),
                current_backend_key,
            )
            .await;
            return;
        }
    };
    config
        .agent_recovery
        .set_auth_process(recovery_terminal_id, Some(login_key.clone()))
        .await;
    if config
        .agent_recovery
        .is_cancelled(recovery_terminal_id)
        .await
    {
        let _ = config.backend.kill(&login_key).await;
        config.backend.release(&login_key).await;
        finish_failure(
            &config,
            recovery_terminal_id,
            current_terminal_id,
            &display_name,
            "authentication was cancelled".into(),
            current_backend_key,
        )
        .await;
        return;
    }
    let old_terminal_guard = if let Some(backend_key) = current_backend_key.as_deref() {
        if let Some((prompt_history, composing_buffer)) =
            crate::spawn_handler::capture_terminal_conversation_state(&config, current_terminal_id)
                .await
        {
            config
                .agent_recovery
                .update_conversation(recovery_terminal_id, prompt_history, composing_buffer)
                .await;
        }
        let guard = config.terminal.lock_terminal_io(backend_key).await;
        if let Err(error) = config.backend.kill(backend_key).await {
            let _ = config.backend.kill(&login_key).await;
            config.backend.release(&login_key).await;
            finish_failure(
                &config,
                recovery_terminal_id,
                current_terminal_id,
                &display_name,
                format!("could not stop the blocked agent: {error}"),
                current_backend_key,
            )
            .await;
            return;
        }
        Some(guard)
    } else {
        None
    };
    let latest_context = config
        .agent_recovery
        .context(recovery_terminal_id)
        .await
        .unwrap_or(context);
    let auth_terminal_id = crate::spawn_handler::alloc_terminal_id(&*config.store);
    let model_label = model_label_for(&latest_context);
    // History/draft are workspace-scoped now (keyed by the stable session_key,
    // not the login terminal's fresh backend_key), so the re-auth login terminal
    // reads the same rows the blocked agent wrote — this keeps them populated.
    crate::spawn_handler::restore_workspace_conversation_state(
        &config,
        latest_context.session_key.as_str(),
        &latest_context.prompt_history,
        latest_context.composing_buffer.as_deref(),
    )
    .await;
    config
        .terminal
        .record_spawn_attributes(
            auth_terminal_id,
            latest_context.session_id,
            latest_context.access,
            latest_context.no_permission,
            latest_context.on_main,
            model_label.as_deref(),
        )
        .await;
    config
        .terminal
        .lock_registration()
        .await
        .register_replacement(
            current_terminal_id,
            auth_terminal_id,
            login_key.clone(),
            latest_context.session_key.clone(),
            TerminalKind::Agent(latest_context.agent_id.clone()),
            None,
            true,
        );
    config
        .agent_recovery
        .set_current_terminal(
            recovery_terminal_id,
            auth_terminal_id,
            Some(login_key.clone()),
        )
        .await;
    drop(old_terminal_guard);
    if let Some(backend_key) = current_backend_key.as_deref() {
        crate::spawn_handler::detach_killed_terminal(
            &config,
            current_terminal_id,
            backend_key,
            crate::working_claims::ClaimRelease::Project,
        )
        .await;
        if previous_failure.is_some() {
            config.backend.release(backend_key).await;
        }
    }
    let _ = config.bus.send(Event::TerminalReplaced {
        old_terminal_id: current_terminal_id,
        terminal_id: auth_terminal_id,
        session_key: latest_context.session_key.clone(),
        kind: TerminalKind::Agent(latest_context.agent_id.clone()),
        no_permission: latest_context.no_permission,
        on_main: latest_context.on_main,
        model_label,
        authenticating: true,
    });
    let _ = config.bus.send(Event::AgentAuthProgress {
        recovery_terminal_id,
        terminal_id: auth_terminal_id,
        phase: AgentAuthPhase::LoginInteractive,
    });
    let login_code =
        pump_auth_terminal(&config, recovery_terminal_id, auth_terminal_id, &login_key).await;
    config
        .agent_recovery
        .set_auth_process(recovery_terminal_id, None)
        .await;
    if config
        .agent_recovery
        .is_cancelled(recovery_terminal_id)
        .await
    {
        finish_failure(
            &config,
            recovery_terminal_id,
            auth_terminal_id,
            &display_name,
            cancelled_login_error(
                verify_authenticated(
                    &config,
                    recovery_terminal_id,
                    &commands.status,
                    commands.signed_out_marker,
                    &latest_context.cwd,
                    AUTH_ENV,
                )
                .await,
                &display_name,
            ),
            Some(login_key),
        )
        .await;
        return;
    }
    if login_code != Some(0) {
        finish_failure(
            &config,
            recovery_terminal_id,
            auth_terminal_id,
            &display_name,
            format!("provider login exited with {}", exit_label(login_code)),
            Some(login_key),
        )
        .await;
        return;
    }
    // lazybox never runs the provider `logout`, so `login` ran with the stale
    // credential still present and can exit 0 without actually
    // re-authenticating (e.g. reporting an already-present but expired
    // session). Trusting that exit code alone would resume straight back into
    // the same failed session and re-arm the auth loop, so confirm the
    // credential is genuinely valid with the provider's own status command
    // before resuming.
    let authenticated = verify_authenticated(
        &config,
        recovery_terminal_id,
        &commands.status,
        commands.signed_out_marker,
        &latest_context.cwd,
        AUTH_ENV,
    )
    .await;
    // Cancellation is checked on its own, BEFORE the gate's verdict is acted
    // on. The gate deliberately fails open for a probe that cannot answer,
    // and a cancel kills that probe mid-flight — so reading the verdict first
    // would let a re-auth the user explicitly cancelled go on to resume the
    // agent. A cancelled flow never resumes, whatever the probe managed to
    // say; the gate's answer only picks which cancellation message is true.
    if config
        .agent_recovery
        .is_cancelled(recovery_terminal_id)
        .await
    {
        finish_failure(
            &config,
            recovery_terminal_id,
            auth_terminal_id,
            &display_name,
            cancelled_login_error(authenticated, &display_name),
            Some(login_key),
        )
        .await;
        return;
    }
    if !authenticated {
        finish_failure(
            &config,
            recovery_terminal_id,
            auth_terminal_id,
            &display_name,
            "sign-in did not complete — the agent is still logged out. Please sign in again."
                .to_string(),
            Some(login_key),
        )
        .await;
        return;
    }
    config
        .agent_recovery
        .set_phase(recovery_terminal_id, AgentAuthPhase::Resuming)
        .await;
    let _ = config.bus.send(Event::AgentAuthProgress {
        recovery_terminal_id,
        terminal_id: auth_terminal_id,
        phase: AgentAuthPhase::Resuming,
    });
    if let Some(resumed_terminal_id) = resume_agent(&config, recovery_terminal_id).await {
        crate::spawn_handler::detach_killed_terminal(
            &config,
            auth_terminal_id,
            &login_key,
            crate::working_claims::ClaimRelease::Project,
        )
        .await;
        config.backend.release(&login_key).await;
        let _ = config.bus.send(Event::AgentAuthFinished {
            recovery_terminal_id,
            terminal_id: resumed_terminal_id,
            display_name,
            success: true,
            error: None,
        });
    } else {
        finish_failure(
            &config,
            recovery_terminal_id,
            auth_terminal_id,
            &display_name,
            "the agent could not be resumed".into(),
            Some(login_key),
        )
        .await;
        return;
    }
    config.agent_recovery.finish(recovery_terminal_id).await;
}

/// Message for a re-auth cancelled *after* the interactive login had started,
/// given whether the credential is `authenticated` now.
///
/// Killing the provider's `login` mid-flight is the one way lazybox's own
/// recovery can still leave the shared machine-wide credential empty: the
/// provider may have cleared it to begin a fresh sign-in, and the kill lands
/// before the replacement is written. Nothing here can prevent that (the
/// subprocess owns the credential), but it must not be SILENT — the user
/// would otherwise discover it one failing pane at a time, across every
/// session of that agent and their own interactive terminal.
///
/// So when the login is intact — the overwhelmingly common cancel, where the
/// user changed their mind before the provider touched anything — report the
/// plain cancellation and say nothing alarming. When it is not, name it.
fn cancelled_login_error(authenticated: bool, display_name: &str) -> String {
    if authenticated {
        return "authentication was cancelled".into();
    }
    format!(
        "authentication was cancelled while {display_name} was signing in, and the \
         shared machine-wide login is now empty — every {display_name} session on \
         this machine needs signing in again."
    )
}

/// Confirm the agent's login is actually valid before resuming, by running
/// the provider's own status command. Returns `true` (resume) unless the
/// probe reports, unambiguously, that the agent is signed out.
///
/// Deliberately fails OPEN, and the exit code alone is NOT an unambiguous
/// signal. A probe that cannot answer — an empty status command, a spawn
/// failure, a `codex` shim or an older build without a `login status`
/// subcommand (clap exits 2), an auth mode the probe does not recognize —
/// exits non-zero while saying nothing about the credential. Failing closed
/// there turns a sign-in the user just completed successfully into a
/// permanent "still logged out", with the conversation never resumed and
/// every retry looping; before Codex moved onto the shared login it skipped
/// this gate entirely, so that was a new way to strand a conversation.
///
/// So: when the agent declares a `signed_out_marker`, that token is the only
/// thing that stops a resume — a non-zero exit *without* it is treated as a
/// probe quirk and the resume proceeds. An agent that declares no marker has
/// nothing else to go on, so its exit code is all that is left and a non-zero
/// one does block. Every built-in that reaches this gate declares a marker.
///
/// Output is collected by draining the live subscription until it CLOSES, not
/// via a post-exit snapshot. The PTY reader thread and the child-reap exit
/// watcher are independent (see `pty.rs` reader EOF vs `raw_pty.rs`
/// `child.wait()`), so `wait_exit` can return while the final status bytes are
/// still buffered ahead of the ring — a snapshot then would miss the
/// signed-out marker and wrongly resume into a dead session. The live channel
/// closes only after the reader observes EOF, which is the one point every
/// output byte is guaranteed captured.
async fn verify_authenticated(
    config: &ServerConfig,
    terminal_id: TerminalId,
    status_argv: &[String],
    signed_out_marker: Option<&str>,
    cwd: &std::path::Path,
    env: &[(String, String)],
) -> bool {
    if status_argv.is_empty() {
        return true;
    }
    let Ok(key) = config
        .backend
        .spawn(status_argv, Some(cwd), env, "agent-auth")
        .await
    else {
        return true;
    };
    config
        .agent_recovery
        .set_auth_process(terminal_id, Some(key.clone()))
        .await;
    let mut output = Vec::new();
    let probe = async {
        if let Ok(mut subscription) = config.backend.subscribe(&key).await {
            output.extend_from_slice(&subscription.replay);
            while let Some(chunk) = subscription.live.recv().await {
                output.extend_from_slice(&chunk.bytes);
            }
        }
        config.backend.wait_exit(&key).await
    };
    let outcome = tokio::time::timeout(auth_status_probe_timeout(), probe).await;
    config
        .agent_recovery
        .set_auth_process(terminal_id, None)
        .await;
    let code = match outcome {
        Ok(code) => {
            config.backend.release(&key).await;
            code
        }
        Err(_) => {
            // A probe that never exits is the extreme case of one that cannot
            // answer: kill it, reap it, and resume rather than strand the
            // conversation on a wedged subprocess. But judge it on whatever it
            // DID print first — a probe that reported "not logged in" and then
            // wedged has already given the one unambiguous answer, and
            // resuming on it would land straight back in the dead session.
            let _ = config.backend.kill(&key).await;
            config.backend.release(&key).await;
            tracing::warn!(
                timeout = ?auth_status_probe_timeout(),
                "re-auth status probe did not exit — killed it; judging the \
                 partial output rather than leaving the pane stuck mid-recovery"
            );
            None
        }
    };
    let Some(marker) = signed_out_marker else {
        // No token to look for: the exit code is the only signal available,
        // and a probe that never exited has none.
        return code == Some(0);
    };
    // Whitespace-insensitive, case-folded scan on both sides so
    // pretty-printing or casing in the status output can't hide the
    // provider's signed-out token.
    let normalize = |s: &str| -> String {
        s.split_whitespace()
            .collect::<String>()
            .to_ascii_lowercase()
    };
    let signed_out = normalize(&String::from_utf8_lossy(&output)).contains(&normalize(marker));
    if signed_out {
        return false;
    }
    if code.is_some_and(|code| code != 0) {
        tracing::warn!(
            ?code,
            "re-auth status probe exited non-zero without its signed-out marker —              treating as an unusable probe and resuming rather than stranding the conversation"
        );
    }
    true
}

async fn pump_auth_terminal(
    config: &ServerConfig,
    recovery_terminal_id: TerminalId,
    terminal_id: TerminalId,
    backend_key: &str,
) -> Option<i32> {
    let Ok(mut subscription) = config.backend.subscribe(backend_key).await else {
        return config.backend.wait_exit(backend_key).await;
    };
    if !subscription.replay.is_empty()
        && let Some(output) = config.agent_recovery.output(recovery_terminal_id).await
    {
        // Same convention as the terminal pump's `replay_event`: a replay
        // produced at one size streams as stamped output, one that
        // straddles a resize needs the spans only the replay event carries.
        let event = match subscription.replay_sizes.as_slice() {
            [] | [_] => {
                let (cols, rows) = subscription
                    .replay_sizes
                    .first()
                    .map_or((0, 0), |span| (span.cols, span.rows));
                Event::AgentAuthOutput {
                    terminal_id,
                    bytes: subscription.replay,
                    first_seq: 1,
                    seq: subscription.last_seq,
                    cols,
                    rows,
                }
            }
            sizes => Event::AgentAuthReplay {
                terminal_id,
                replay: subscription.replay,
                seq: subscription.last_seq,
                sizes: sizes.to_vec(),
            },
        };
        let _ = output.send(event);
    }
    while let Some(chunk) = subscription.live.recv().await {
        if let Some(output) = config.agent_recovery.output(recovery_terminal_id).await {
            let _ = output.send(Event::AgentAuthOutput {
                terminal_id,
                bytes: chunk.bytes,
                first_seq: chunk.seq,
                seq: chunk.seq,
                cols: chunk.cols,
                rows: chunk.rows,
            });
        }
    }
    config.backend.wait_exit(backend_key).await
}

async fn finish_failure(
    config: &ServerConfig,
    recovery_terminal_id: TerminalId,
    terminal_id: TerminalId,
    display_name: &str,
    error: String,
    backend_key: Option<String>,
) {
    let output = config.agent_recovery.output(recovery_terminal_id).await;
    config
        .agent_recovery
        .record_failure(
            recovery_terminal_id,
            terminal_id,
            display_name.to_string(),
            error.clone(),
            backend_key,
            output,
        )
        .await;
    config.agent_recovery.finish(recovery_terminal_id).await;
    let _ = config.bus.send(Event::AgentAuthFinished {
        recovery_terminal_id,
        terminal_id,
        display_name: display_name.to_string(),
        success: false,
        error: Some(error),
    });
}

fn exit_label(code: Option<i32>) -> String {
    code.map_or_else(|| "no status".into(), |code| format!("status {code}"))
}

fn agent_display_name(config: &ServerConfig, agent_id: &str) -> String {
    config
        .agents
        .get(agent_id)
        .map(|agent| agent.display_name().to_string())
        .unwrap_or_else(|| agent_id.to_string())
}

fn model_label_for(context: &AgentResumeContext) -> Option<String> {
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    let models = cfg.agent_models(&context.agent_id);
    context
        .model_alias
        .as_deref()
        .and_then(|alias| models.tier(alias))
        .map(|tier| tier.label.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SessionBackend;

    async fn recovery_fixture(
        agent_id: &str,
        provider_session_id: Option<&str>,
    ) -> (ServerConfig, crate::backend::MockBackend, TerminalId) {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let terminal_id = TerminalId(708);
        let backend_key = mock
            .spawn(
                &[agent_id.into()],
                Some(std::path::Path::new("/tmp")),
                &[],
                "blocked",
            )
            .await
            .expect("spawn blocked agent");
        config
            .terminal
            .register_terminal(
                terminal_id,
                backend_key.clone(),
                SessionKey::new("github:owner/repo#708"),
                TerminalKind::Agent(agent_id.into()),
            )
            .await;
        config
            .agent_recovery
            .remember_spawn(AgentResumeContext {
                terminal_id,
                session_key: SessionKey::new("github:owner/repo#708"),
                session_id: None,
                agent_id: agent_id.into(),
                cwd: "/tmp".into(),
                backend_key: Some(backend_key),
                on_main: false,
                model_alias: None,
                access: AgentRunAccess::Default,
                no_permission: false,
                provider_session_id: provider_session_id.map(str::to_string),
                prompt_history: vec![UserPrompt {
                    text: "keep this prompt".into(),
                    timestamp_ms: 1,
                    source: lazybox_ipc::PromptSource::Typed,
                }],
                composing_buffer: Some("keep this draft".into()),
            })
            .await;
        assert!(
            config
                .agent_recovery
                .require(
                    terminal_id,
                    agent_id.into(),
                    agent_display_name(&config, agent_id),
                    format!(
                        "{} authentication is no longer valid.",
                        agent_display_name(&config, agent_id)
                    ),
                    0,
                )
                .await
        );
        crate::spawn_handler::restore_terminal_conversation_state(
            &config,
            terminal_id,
            &[UserPrompt {
                text: "keep this prompt".into(),
                timestamp_ms: 1,
                source: lazybox_ipc::PromptSource::Typed,
            }],
            Some("keep this draft"),
        )
        .await;
        (config, mock, terminal_id)
    }

    /// Shorten the status-probe bound for a test that needs it to elapse,
    /// restoring it on drop. 2s is far longer than any sibling test's probe
    /// (in-memory mock, microseconds) yet short enough to stay well inside
    /// nextest's 10s per-test deadline.
    struct ShortProbeTimeout;

    impl ShortProbeTimeout {
        fn arm() -> Self {
            super::AUTH_STATUS_PROBE_TIMEOUT_MS_OVERRIDE
                .store(2_000, std::sync::atomic::Ordering::Relaxed);
            Self
        }
    }

    impl Drop for ShortProbeTimeout {
        fn drop(&mut self) {
            super::AUTH_STATUS_PROBE_TIMEOUT_MS_OVERRIDE
                .store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    async fn wait_for_argv(mock: &crate::backend::MockBackend, expected: &[&str]) -> Vec<String> {
        // Agent spawns are wrapped in `nice -n <N>` (fleet-priority
        // shading); strip the wrapper so assertions compare the agent's
        // own argv.
        fn strip_nice(argv: &[String]) -> &[String] {
            match argv {
                [first, flag, _n, rest @ ..] if first == "nice" && flag == "-n" => rest,
                other => other,
            }
        }
        for _ in 0..10_000 {
            let all = mock.all_argv().await;
            if let Some(argv) = all.into_iter().find(|argv| {
                strip_nice(argv)
                    .iter()
                    .map(String::as_str)
                    .take(expected.len())
                    .eq(expected.iter().copied())
            }) {
                return argv;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "command was not spawned: {expected:?}; observed {:?}",
            mock.all_argv().await
        );
    }

    async fn wait_for_replacement(
        config: &ServerConfig,
        recovery_terminal_id: TerminalId,
    ) -> TerminalId {
        for _ in 0..10_000 {
            let current = config
                .agent_recovery
                .current_terminal(recovery_terminal_id)
                .await;
            if current != recovery_terminal_id {
                return current;
            }
            tokio::task::yield_now().await;
        }
        panic!("authentication terminal did not replace {recovery_terminal_id:?}");
    }

    #[tokio::test]
    async fn reauthentication_runs_provider_commands_and_exact_resume() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        let mut broadcast_events = config.bus.subscribe();
        let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel();
        start_reauthentication(
            &config,
            terminal_id,
            Some(lazybox_ipc::EventSender::from_unbounded(output_tx)),
        )
        .await;

        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.emit(&login_key, b"interactive provider output\r\n")
            .await;
        let output_event = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let event = output_rx.recv().await.expect("private output channel");
                if matches!(event, Event::AgentAuthOutput { .. }) {
                    return event;
                }
            }
        })
        .await
        .expect("private auth output deadline");
        assert!(matches!(
            output_event,
            Event::AgentAuthOutput {
                terminal_id: id,
                bytes,
                first_seq: 1,
                seq: 1,
                            ..
            } if id == auth_terminal_id && bytes == b"interactive provider output\r\n"
        ));
        while let Ok(event) = broadcast_events.try_recv() {
            assert!(
                !matches!(
                    event,
                    Event::AgentAuthOutput { .. } | Event::AgentAuthReplay { .. }
                ),
                "authentication output must never enter the process-wide event bus"
            );
        }
        assert!(
            crate::spawn_handler::handle_write(
                &config,
                auth_terminal_id,
                b"provider input\n",
                lazybox_ipc::TerminalInputIntent::Compose,
            )
            .await
        );
        assert_eq!(
            mock.writes_for(&login_key).await,
            vec![b"provider input\n".to_vec()]
        );
        mock.finish(&login_key, 0).await;
        wait_for_argv(&mock, &["codex", "login", "status"]).await;
        mock.finish("mock-agent-auth-2", 0).await;
        wait_for_argv(&mock, &["codex", "resume", "conversation-708"]).await;

        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!config.agent_recovery.active(terminal_id).await);
        let snapshot = crate::spawn_handler::snapshot_terminals(&config).await;
        assert!(
            snapshot
                .iter()
                .all(|terminal| terminal.terminal_id != terminal_id)
        );
        let resumed = snapshot
            .iter()
            .find(|terminal| matches!(terminal.kind, TerminalKind::Agent(_)))
            .expect("resumed terminal snapshot");
        assert_eq!(resumed.prompt_history[0].text, "keep this prompt");
        assert_eq!(resumed.composing_buffer.as_deref(), Some("keep this draft"));
    }

    #[tokio::test]
    async fn codex_reauthentication_shares_login_without_logging_other_workspaces_out() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-777")).await;
        start_reauthentication(&config, terminal_id, None).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        assert!(
            mock.env_for(&login_key)
                .await
                .expect("login command spawned")
                .iter()
                .all(|(k, _)| k != "CODEX_HOME"),
            "login must inherit the daemon's shared Codex home, not be pinned to a per-workspace one"
        );
        assert!(
            mock.all_argv()
                .await
                .iter()
                .all(|argv| argv.as_slice() != ["codex", "logout"]),
            "a shared machine-wide login must never be logged out by a single pane's re-auth"
        );
        mock.finish(&login_key, 0).await;
        wait_for_argv(&mock, &["codex", "login", "status"]).await;
        assert!(
            mock.env_for("mock-agent-auth-2")
                .await
                .expect("status probe spawned")
                .iter()
                .all(|(k, _)| k != "CODEX_HOME"),
            "the status probe must read the same shared home the login wrote"
        );
        mock.emit("mock-agent-auth-2", b"Not logged in").await;
        mock.finish("mock-agent-auth-2", 0).await;
        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!config.agent_recovery.active(terminal_id).await);
        assert!(
            mock.all_argv()
                .await
                .iter()
                .all(|argv| !argv.starts_with(&["codex".into(), "resume".into()]))
        );
        assert!(config.agent_recovery.context(terminal_id).await.is_some());
    }

    #[tokio::test]
    async fn reauthentication_never_signs_out_the_shared_login() {
        // Every agent keeps a machine-wide login, shared by every other
        // session of that agent AND the user's own interactive pane. A single
        // pane's re-auth must therefore NEVER run the provider `logout` —
        // that would sign all of them out at once (#1376). There is no longer
        // any request that can ask it to: the shared login is only ever
        // refreshed in place.
        let (config, mock, terminal_id) =
            recovery_fixture("claude", Some("claude-conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;

        // Login runs directly, with no preceding logout of the shared credential.
        wait_for_argv(&mock, &["claude", "auth", "login"]).await;
        assert!(
            mock.all_argv()
                .await
                .iter()
                .all(|argv| argv.as_slice() != ["claude", "auth", "logout"]),
            "a shared machine-wide login must never be logged out by a single pane's re-auth"
        );
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;
        // Because the logout was skipped, the resume is gated on a real status
        // check confirming the login actually took. It reports a live session,
        // so the exact-conversation resume proceeds.
        wait_for_argv(&mock, &["claude", "auth", "status"]).await;
        mock.emit("mock-agent-auth-2", br#"{"loggedIn": true}"#)
            .await;
        mock.finish("mock-agent-auth-2", 0).await;
        wait_for_argv(&mock, &["claude", "--resume", "claude-conversation-708"]).await;
    }

    /// A status probe that CANNOT answer must not strand the conversation.
    ///
    /// `codex login status` exits 1 when signed out, but a `codex` shim, a
    /// build predating the subcommand, or an auth mode the probe does not
    /// recognize also exits non-zero — while saying nothing at all about the
    /// credential. Failing closed there turned a sign-in the user had just
    /// completed into a permanent "still logged out" with the conversation
    /// never resumed and every retry looping. The signed-out MARKER is the
    /// only unambiguous signal, so a non-zero exit without it resumes.
    #[tokio::test]
    async fn a_status_probe_that_cannot_answer_still_resumes() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;

        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;
        // The probe fails the way a missing subcommand does: non-zero, and
        // nothing resembling the provider's signed-out token.
        wait_for_argv(&mock, &["codex", "login", "status"]).await;
        mock.emit(
            "mock-agent-auth-2",
            b"error: unrecognized subcommand 'status'",
        )
        .await;
        mock.finish("mock-agent-auth-2", 2).await;

        wait_for_argv(&mock, &["codex", "resume", "conversation-708"]).await;
    }

    /// The other half of the same gate: an unambiguous signed-out token still
    /// blocks the resume even though the exit code is now advisory.
    #[tokio::test]
    async fn a_signed_out_marker_blocks_the_resume_whatever_the_exit_code() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;

        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;
        wait_for_argv(&mock, &["codex", "login", "status"]).await;
        // Exit 0 — only the marker says otherwise, and it must win.
        mock.emit("mock-agent-auth-2", b"Not logged in").await;
        mock.finish("mock-agent-auth-2", 0).await;

        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!config.agent_recovery.active(terminal_id).await);
        assert!(
            mock.all_argv()
                .await
                .iter()
                .all(|argv| !argv.starts_with(&["codex".into(), "resume".into()])),
            "an explicitly signed-out status must not resume the agent"
        );
    }

    /// lazybox never logs out, so it must never SAY it is logging out. The
    /// re-auth flow used to open by broadcasting `LoggingOut`, which the TUI
    /// renders as "signing out of the provider…" — the exact opposite of the
    /// guarantee #1376 exists to give. The phase no longer exists at all, so
    /// the compiler enforces the message; what this pins is the phase a
    /// client reconnecting mid-flow actually reads.
    #[tokio::test]
    async fn a_client_reconnecting_mid_re_auth_reads_the_login_phase() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;

        let (replay, _) = config.agent_recovery.replay_events(None).await;
        assert!(
            replay.iter().any(|event| matches!(
                event,
                Event::AgentAuthProgress {
                    recovery_terminal_id,
                    terminal_id: id,
                    phase: AgentAuthPhase::LoginInteractive,
                } if *recovery_terminal_id == terminal_id && *id == auth_terminal_id
            )),
            "the only phase before the login completes is the interactive login: {replay:?}"
        );
        cancel_reauthentication(&config, terminal_id).await;
    }

    /// A status probe that never exits must not wedge the recovery.
    ///
    /// The probe is drained until its output channel closes, which only
    /// happens when the child exits, and `cancel_reauthentication` kills the
    /// flow's registered process exactly ONCE — so a probe that hangs (or one
    /// spawned on the cancel path, after that kill already fired) would leave
    /// the pane `authenticating` for good, with a second Esc doing nothing.
    #[tokio::test]
    async fn a_status_probe_that_never_exits_does_not_wedge_the_recovery() {
        let _short = ShortProbeTimeout::arm();
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;

        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;
        // The probe starts and is never finished — no exit, no channel close.
        wait_for_argv(&mock, &["codex", "login", "status"]).await;

        // The bound is real time now, so wait on the clock rather than a
        // yield-only spin (which would never let it elapse).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while config.agent_recovery.active(terminal_id).await
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            !config.agent_recovery.active(terminal_id).await,
            "the flow must give up on a wedged status probe, not hang forever"
        );
        // Giving up resumes rather than stranding the conversation, and the
        // wedged probe is killed rather than left running.
        wait_for_argv(&mock, &["codex", "resume", "conversation-708"]).await;
        assert!(
            mock.released_keys()
                .await
                .iter()
                .any(|key| key == "mock-agent-auth-2"),
            "the abandoned probe must be reaped, not leaked: {:?}",
            mock.released_keys().await
        );
    }

    /// A probe that printed its signed-out token and THEN wedged has already
    /// given the one unambiguous answer. Abandoning it must not discard that
    /// and resume — the resumed agent would land straight back in the dead
    /// session and re-arm the auth loop.
    #[tokio::test]
    async fn a_wedged_probe_that_already_reported_signed_out_still_blocks() {
        let _short = ShortProbeTimeout::arm();
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;

        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;
        wait_for_argv(&mock, &["codex", "login", "status"]).await;
        // Reports signed out, then never exits.
        mock.emit("mock-agent-auth-2", b"Not logged in").await;

        // The bound is real time now, so wait on the clock rather than a
        // yield-only spin (which would never let it elapse).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while config.agent_recovery.active(terminal_id).await
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(!config.agent_recovery.active(terminal_id).await);
        assert!(
            mock.all_argv()
                .await
                .iter()
                .all(|argv| !argv.starts_with(&["codex".into(), "resume".into()])),
            "the marker it managed to print must still block the resume"
        );
        assert!(
            config.agent_recovery.context(terminal_id).await.is_some(),
            "the conversation stays recoverable so the user can retry sign-in"
        );
    }

    /// Killing the provider's `login` mid-flight can leave the shared
    /// credential empty — it may have been cleared to begin a fresh sign-in.
    /// That cannot be prevented from here, but it must not be silent: the
    /// user would otherwise discover it one failing pane at a time, across
    /// every session of that agent and their own terminal.
    #[tokio::test]
    async fn a_cancel_that_empties_the_shared_login_says_so() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        wait_for_replacement(&config, terminal_id).await;

        cancel_reauthentication(&config, terminal_id).await;
        // The cancel probes the shared login, which the killed sign-in left empty.
        wait_for_argv(&mock, &["codex", "login", "status"]).await;
        mock.emit("mock-agent-auth-2", b"Not logged in").await;
        mock.finish("mock-agent-auth-2", 1).await;

        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        let (replay, _) = config.agent_recovery.replay_events(None).await;
        let error = replay
            .iter()
            .find_map(|event| match event {
                Event::AgentAuthFinished {
                    recovery_terminal_id,
                    error: Some(error),
                    ..
                } if *recovery_terminal_id == terminal_id => Some(error.clone()),
                _ => None,
            })
            .expect("the cancelled re-auth reports a failure");
        assert!(
            error.contains("shared machine-wide login is now empty"),
            "a cancel that emptied the shared login must name it: {error}"
        );
    }

    #[tokio::test]
    async fn login_reporting_signed_out_does_not_resume_into_a_dead_session() {
        // The shared-login path skips logout, so `claude auth login` can exit 0
        // while the session is still not authenticated (an expired credential
        // was already on disk). The status gate must catch that and refuse to
        // resume — otherwise the resumed agent immediately re-fails auth and
        // the loop returns. The conversation stays recoverable for a retry.
        let (config, mock, terminal_id) =
            recovery_fixture("claude", Some("claude-conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;

        wait_for_argv(&mock, &["claude", "auth", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;
        // Login "succeeds" (exit 0) but status reports no live session.
        wait_for_argv(&mock, &["claude", "auth", "status"]).await;
        mock.emit("mock-agent-auth-2", br#"{"loggedIn": false}"#)
            .await;
        mock.finish("mock-agent-auth-2", 0).await;

        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!config.agent_recovery.active(terminal_id).await);
        assert!(
            mock.all_argv()
                .await
                .iter()
                .all(|argv| argv.as_slice() != ["claude", "--resume", "claude-conversation-708"]),
            "a login that never established a session must not resume the agent"
        );
        assert!(
            config.agent_recovery.context(terminal_id).await.is_some(),
            "the conversation stays recoverable so the user can retry sign-in"
        );
        let (replay_events, _) = config.agent_recovery.replay_events(None).await;
        assert!(
            replay_events.iter().any(|event| matches!(
                event,
                Event::AgentAuthFinished {
                    recovery_terminal_id,
                    success: false,
                    ..
                } if *recovery_terminal_id == terminal_id
            )),
            "the failed re-auth is surfaced, not silently swallowed"
        );
    }

    #[tokio::test]
    async fn status_output_only_on_the_live_stream_still_blocks_resume() {
        // Regression guard for the read/exit race: on the real backend the
        // child-reap exit watcher and the PTY reader are independent, so the
        // status JSON can still be in flight on the live stream when
        // `wait_exit` returns — a post-exit ring snapshot would miss it and
        // wrongly resume. The gate must drain the live subscription, so a
        // `loggedIn: false` delivered ONLY on the live stream (never appended
        // to the replay ring) must still block the resume.
        let (config, mock, terminal_id) =
            recovery_fixture("claude", Some("claude-conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;

        wait_for_argv(&mock, &["claude", "auth", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;

        wait_for_argv(&mock, &["claude", "auth", "status"]).await;
        // Wait until the gate has actually subscribed, then deliver the
        // signed-out marker on the live stream ONLY (bypassing the ring a
        // snapshot would read) before closing the stream.
        for _ in 0..10_000 {
            if mock.subscriber_count("mock-agent-auth-2").await > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        mock.emit_live_only("mock-agent-auth-2", br#"{"loggedIn": false}"#)
            .await;
        mock.finish("mock-agent-auth-2", 0).await;

        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!config.agent_recovery.active(terminal_id).await);
        assert!(
            mock.all_argv()
                .await
                .iter()
                .all(|argv| argv.as_slice() != ["claude", "--resume", "claude-conversation-708"]),
            "a signed-out marker seen only on the live stream must still block the resume"
        );
        assert!(
            config.agent_recovery.context(terminal_id).await.is_some(),
            "the conversation stays recoverable so the user can retry sign-in"
        );
    }

    #[tokio::test]
    async fn cancel_during_the_status_probe_reports_cancelled_not_logged_out() {
        // The login may have succeeded; a cancel arriving during the status
        // probe must be reported as a cancellation, not misattributed as the
        // agent being "still logged out".
        let (config, mock, terminal_id) =
            recovery_fixture("claude", Some("claude-conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;

        wait_for_argv(&mock, &["claude", "auth", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;

        // Let the status probe start and subscribe, then cancel mid-probe.
        wait_for_argv(&mock, &["claude", "auth", "status"]).await;
        for _ in 0..10_000 {
            if mock.subscriber_count("mock-agent-auth-2").await > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        cancel_reauthentication(&config, terminal_id).await;

        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        let (replay_events, _) = config.agent_recovery.replay_events(None).await;
        assert!(
            replay_events.iter().any(|event| matches!(
                event,
                Event::AgentAuthFinished {
                    recovery_terminal_id,
                    success: false,
                    error: Some(error),
                    ..
                } if *recovery_terminal_id == terminal_id && error.contains("cancelled")
            )),
            "a cancel during the status probe is surfaced as cancelled, not 'still logged out'"
        );
    }

    #[tokio::test]
    async fn failed_login_keeps_the_conversation_recoverable() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        crate::spawn_handler::handle_record_user_message(
            &config,
            terminal_id,
            &UserPrompt {
                text: "new prompt before login starts".into(),
                timestamp_ms: 2,
                source: lazybox_ipc::PromptSource::Typed,
            },
        )
        .await;
        crate::spawn_handler::handle_record_composing_buffer(
            &config,
            terminal_id,
            "new draft before login starts",
        )
        .await;
        start_reauthentication(&config, terminal_id, None).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("login backend");
        mock.emit(&login_key, b"private device-code output\r\n")
            .await;
        mock.finish("mock-agent-auth-1", 1).await;

        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!config.agent_recovery.active(terminal_id).await);
        let context = config
            .agent_recovery
            .context(terminal_id)
            .await
            .expect("recoverable context survives");
        assert_eq!(
            context.provider_session_id.as_deref(),
            Some("conversation-708")
        );
        assert_eq!(
            context.prompt_history[1].text,
            "new prompt before login starts"
        );
        assert_eq!(
            context.composing_buffer.as_deref(),
            Some("new draft before login starts")
        );
        assert!(mock.all_argv().await.iter().all(|argv| !argv.starts_with(&[
            "codex".into(),
            "resume".into(),
            "conversation-708".into()
        ])));
        let snapshot = crate::spawn_handler::snapshot_terminals(&config).await;
        let failed = snapshot
            .iter()
            .find(|terminal| terminal.terminal_id == auth_terminal_id)
            .expect("failed auth terminal remains reconnectable");
        assert!(failed.authenticating);
        assert_eq!(
            failed.prompt_history[1].text,
            "new prompt before login starts"
        );
        assert_eq!(
            failed.composing_buffer.as_deref(),
            Some("new draft before login starts")
        );
        let (replay_events, _) = config.agent_recovery.replay_events(None).await;
        assert!(replay_events.iter().any(|event| matches!(
            event,
            Event::AgentAuthFinished {
                recovery_terminal_id,
                terminal_id: id,
                success: false,
                ..
            } if *recovery_terminal_id == terminal_id && *id == failed.terminal_id
        )));
    }

    #[tokio::test]
    async fn reconnect_during_codex_login_keeps_the_auth_terminal_addressable() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let snapshot = crate::spawn_handler::snapshot_terminals(&config).await;
        assert!(
            snapshot
                .iter()
                .any(|terminal| terminal.terminal_id == auth_terminal_id)
        );
        let (events, _) = config.agent_recovery.replay_events(None).await;
        assert!(events.iter().any(|event| matches!(event,
            Event::AgentAuthProgress { recovery_terminal_id, terminal_id: id,
                phase: AgentAuthPhase::LoginInteractive,
            } if *recovery_terminal_id == terminal_id && *id == auth_terminal_id
        )));
        cancel_reauthentication(&config, terminal_id).await;
    }

    #[tokio::test]
    async fn reconnect_during_login_receives_private_bounded_replay() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        let mut context = config
            .agent_recovery
            .context(terminal_id)
            .await
            .expect("resume context");
        context.on_main = true;
        context.no_permission = true;
        context.model_alias = Some("large".into());
        config.agent_recovery.remember_spawn(context).await;
        let (first_tx, first_rx) = tokio::sync::mpsc::unbounded_channel();
        start_reauthentication(
            &config,
            terminal_id,
            Some(lazybox_ipc::EventSender::from_unbounded(first_tx)),
        )
        .await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("login backend");
        mock.emit(&login_key, b"provider-owned login screen\r\n")
            .await;
        drop(first_rx);

        let (reconnect_tx, mut reconnect_rx) = tokio::sync::mpsc::unbounded_channel();
        let reconnect_tx = lazybox_ipc::EventSender::from_unbounded(reconnect_tx);
        let (events, replay_backends) = config
            .agent_recovery
            .replay_events(Some(&reconnect_tx))
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Event::AgentAuthProgress {
                recovery_terminal_id,
                terminal_id: current_terminal_id,
                phase: AgentAuthPhase::LoginInteractive,
            } if *recovery_terminal_id == terminal_id
                && *current_terminal_id == auth_terminal_id
        )));
        replay_auth_output(&config, &reconnect_tx, replay_backends).await;
        let replay = reconnect_rx.recv().await.expect("private replay");
        assert!(matches!(
            replay,
            Event::AgentAuthReplay {
                terminal_id: id,
                replay,
                ..
            } if id == auth_terminal_id
                && replay.ends_with(b"provider-owned login screen\r\n")
        ));
        let snapshot = crate::spawn_handler::snapshot_terminals(&config).await;
        let auth = snapshot
            .iter()
            .find(|terminal| terminal.terminal_id == auth_terminal_id)
            .expect("auth snapshot");
        assert!(
            auth.replay.is_empty(),
            "provider auth output must not leak through the shared workspace snapshot"
        );
        assert!(auth.on_main);
        assert!(auth.no_permission);

        cancel_reauthentication(&config, terminal_id).await;
    }

    #[tokio::test]
    async fn closing_a_failed_auth_pane_removes_its_server_side_recovery_state() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, None).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        mock.finish("mock-agent-auth-1", 1).await;
        for _ in 0..10_000 {
            if !config.agent_recovery.active(terminal_id).await {
                break;
            }
            tokio::task::yield_now().await;
        }

        let mut shared_events = config.bus.subscribe();
        assert!(
            crate::spawn_handler::handle_close(&config, auth_terminal_id, None).await,
            "failed authentication terminal is closeable"
        );
        assert!(
            config.agent_recovery.context(terminal_id).await.is_none(),
            "closing the recovery pane discards its saved daemon state"
        );
        assert!(
            crate::spawn_handler::snapshot_terminals(&config)
                .await
                .iter()
                .all(|terminal| terminal.terminal_id != auth_terminal_id)
        );
        assert!(
            config.agent_recovery.replay_events(None).await.0.is_empty(),
            "a later reconnect must not resurrect the closed pane"
        );
        while let Ok(event) = shared_events.try_recv() {
            assert!(
                !matches!(event, Event::TerminalExited { terminal_id: id, .. } if id == auth_terminal_id),
                "provider authentication output must not cross the shared terminal-exit channel"
            );
        }
    }

    #[tokio::test]
    async fn concurrent_provider_recovery_is_deduplicated_without_stopping_other_agent() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        let other_terminal_id = TerminalId(709);
        let other_backend_key = mock
            .spawn(
                &["codex".into()],
                Some(std::path::Path::new("/tmp/other")),
                &[],
                "working",
            )
            .await
            .expect("spawn other agent");
        config
            .terminal
            .register_terminal(
                other_terminal_id,
                other_backend_key.clone(),
                SessionKey::new("github:owner/repo#709"),
                TerminalKind::Agent("codex".into()),
            )
            .await;
        config
            .agent_recovery
            .remember_spawn(AgentResumeContext {
                terminal_id: other_terminal_id,
                session_key: SessionKey::new("github:owner/repo#709"),
                session_id: None,
                agent_id: "codex".into(),
                cwd: "/tmp/other".into(),
                backend_key: Some(other_backend_key.clone()),
                on_main: false,
                model_alias: None,
                access: AgentRunAccess::Default,
                no_permission: false,
                provider_session_id: Some("conversation-709".into()),
                prompt_history: Vec::new(),
                composing_buffer: None,
            })
            .await;
        assert!(
            config
                .agent_recovery
                .require(
                    other_terminal_id,
                    "codex".into(),
                    "Codex".into(),
                    "Codex authentication is no longer valid.".into(),
                    1,
                )
                .await
        );

        start_reauthentication(&config, terminal_id, None).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        start_reauthentication(&config, other_terminal_id, None).await;
        tokio::task::yield_now().await;

        assert_eq!(
            mock.all_argv()
                .await
                .iter()
                .filter(|argv| argv.as_slice() == ["codex", "login"])
                .count(),
            1
        );
        assert!(
            mock.list()
                .await
                .expect("list sessions")
                .contains(&other_backend_key),
            "the other provider session must remain running"
        );
    }

    #[tokio::test]
    async fn reauthentication_requires_adapter_detected_auth_failure() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        let (replay_events, _) = config.agent_recovery.replay_events(None).await;
        assert!(replay_events.iter().any(|event| matches!(
            event,
            Event::AgentAuthRequired {
                terminal_id: id,
                ..
            } if *id == terminal_id
        )));
        config
            .agent_recovery
            .requirements
            .lock()
            .await
            .remove(&terminal_id);

        start_reauthentication(&config, terminal_id, None).await;
        tokio::task::yield_now().await;

        assert!(
            mock.all_argv()
                .await
                .iter()
                .all(|argv| argv.as_slice() != ["codex", "logout"])
        );
        assert_eq!(mock.list().await.expect("list sessions").len(), 1);
    }

    #[tokio::test]
    async fn ambiguous_resume_without_provider_id_warns_before_cwd_fallback() {
        let (config, mock, terminal_id) = recovery_fixture("codex", None).await;
        let mut context = config
            .agent_recovery
            .context(terminal_id)
            .await
            .expect("recovery context");
        context.on_main = true;
        config.agent_recovery.remember_spawn(context.clone()).await;
        let backend_key = context.backend_key.expect("blocked backend");
        crate::spawn_handler::detach_killed_terminal(
            &config,
            terminal_id,
            &backend_key,
            crate::working_claims::ClaimRelease::Project,
        )
        .await;
        let mut events = config.bus.subscribe();

        resume_agent(&config, terminal_id).await;

        assert!(matches!(
            events.try_recv(),
            Ok(Event::AgentResumeFallback {
                terminal_id: id,
                ..
            }) if id == terminal_id
        ));
        wait_for_argv(&mock, &["codex", "resume", "--last"]).await;
    }

    /// `a R`: a limit-blocked agent whose process is still running is
    /// stopped, its backend released, and the exact conversation respawned
    /// in its pane through the provider's `--resume <session>` builder — the
    /// only way a fresh credential is picked up. The conversation carried
    /// across the swap is the one captured at kill time.
    #[tokio::test]
    async fn restart_rate_limited_kills_then_resumes_the_same_conversation() {
        let (config, mock, terminal_id) = recovery_fixture("claude", Some("sess-limited")).await;
        let old_backend = config
            .agent_recovery
            .context(terminal_id)
            .await
            .and_then(|c| c.backend_key)
            .expect("blocked backend");

        restart_agent_and_continue(&config, terminal_id).await;

        wait_for_argv(&mock, &["claude", "--resume", "sess-limited"]).await;
        assert!(
            mock.released_keys().await.contains(&old_backend),
            "the stopped agent's backend is released, not leaked"
        );
        assert_eq!(
            mock.list().await.expect("list sessions").len(),
            1,
            "exactly one live backend remains: the respawned agent"
        );
        // The recovery context was consumed by the successful resume.
        assert!(config.agent_recovery.context(terminal_id).await.is_none());
    }

    /// A pane with no launch metadata cannot be restarted; the daemon says
    /// so instead of half-killing something.
    #[tokio::test]
    async fn restart_rate_limited_rejects_a_pane_without_metadata() {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let mut events = config.bus.subscribe();

        restart_agent_and_continue(&config, TerminalId(4242)).await;

        assert!(matches!(
            events.try_recv(),
            Ok(Event::CommandRejected { command, .. }) if command == "RestartAgentAndContinue"
        ));
    }
}
