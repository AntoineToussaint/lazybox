use crate::ServerConfig;
use lazybox_core::{SessionId, SessionKey};
use lazybox_ipc::{AgentAuthPhase, AgentRunAccess, Event, TerminalId, TerminalKind, UserPrompt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

const AUTH_REPLAY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

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
    credentials_isolated: bool,
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
        credentials_isolated: bool,
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
                credentials_isolated,
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
                phase: AgentAuthPhase::LoggingOut,
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
                    credentials_isolated: required.credentials_isolated,
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
    // When this agent isolates its login per session (Codex → a private
    // `CODEX_HOME`), a re-auth only rewrites this session's own credential;
    // the rest of the fleet is untouched, so there is no cascade to warn
    // about and the "other running sessions" count is moot.
    let credentials_isolated = config
        .agents
        .get(&context.agent_id)
        .and_then(|agent| agent.credential_isolation())
        .is_some();
    let other_session_count = if credentials_isolated {
        0
    } else {
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
            credentials_isolated,
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
        credentials_isolated,
    });
}

pub(crate) async fn resume_agent(
    config: &ServerConfig,
    terminal_id: TerminalId,
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
    switch_account: bool,
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
        run_reauthentication(config, context, commands, switch_account).await;
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
        crate::spawn_handler::detach_killed_terminal(config, terminal_id, &backend_key).await;
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
            });
        }
    }
}

async fn run_reauthentication(
    config: ServerConfig,
    context: AgentResumeContext,
    commands: lazybox_agents::AgentAuthCommands,
    switch_account: bool,
) {
    let recovery_terminal_id = context.terminal_id;
    let display_name = agent_display_name(&config, &context.agent_id);
    // Scope the logout/login/resume to this session's own credential home
    // (Codex → an isolated `CODEX_HOME`) so the provider commands rewrite
    // only this session's login, never the machine-wide one the rest of the
    // fleet shares. Empty for an agent that keeps the machine-wide login;
    // the resume itself re-derives the same env through the spawn plan.
    let auth_env = auth_credential_env(&config, &context);
    // A shared, machine-wide login (Claude keeps no per-session credential
    // home) is used by every other running session of this agent AND the
    // user's own interactive pane. Running the provider `logout` there signs
    // ALL of them out at once — the acute bug (#1376): a single pane's
    // "switch account" was logging the whole fleet, and the user, out.
    //
    // The clean long-term fix is per-session credential isolation (Codex →
    // its own `CODEX_HOME`), so a `logout`/`login` rewrites only this
    // session's copy. That path is real but not universally available: an
    // agent that stores its login outside a relocatable home (Claude on
    // macOS keeps it in the process-global Keychain, unscoped by
    // `CLAUDE_CONFIG_DIR`) cannot be isolated by seeding a directory, so
    // `credential_isolation()` returns None for it. Giving Claude a genuine
    // per-session login is tracked separately (see the credential-isolation
    // notes on #1376) and is out of reach here.
    //
    // Until then, the deliberate mitigation for a shared login is to NOT run
    // the destructive `logout`: we downgrade "switch account" to a login-only
    // refresh. lazybox's own code no longer invalidates the shared credential
    // — but note this is not an absolute guarantee that the user cannot end up
    // logged out: `login` is the provider's own subprocess, and if the user
    // cancels it (`cancel_reauthentication` kills it) after it has cleared the
    // credential to begin a fresh sign-in, the shared login can still be left
    // empty. That residual window is inherent to a shared login and only fully
    // closes with isolation above; the common case (an already-valid login the
    // user re-triggered) is protected because a login that leaves the session
    // valid is confirmed by the status gate below before we resume.
    let credentials_isolated = config
        .agents
        .get(&context.agent_id)
        .and_then(|agent| agent.credential_isolation())
        .is_some();
    let switch_account = if switch_account && !credentials_isolated {
        tracing::info!(
            agent_id = %context.agent_id,
            "re-auth: refreshing shared machine-wide login in place; skipping logout so other sessions stay signed in"
        );
        false
    } else {
        switch_account
    };
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
    let _ = config.bus.send(Event::AgentAuthProgress {
        recovery_terminal_id,
        terminal_id: current_terminal_id,
        phase: AgentAuthPhase::LoggingOut,
    });
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
    if switch_account {
        let result = run_quiet_command(
            &config,
            recovery_terminal_id,
            &commands.logout,
            &context.cwd,
            &auth_env,
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
        match result {
            Ok(Some(0)) => {}
            Ok(code) => {
                finish_failure(
                    &config,
                    recovery_terminal_id,
                    current_terminal_id,
                    &display_name,
                    format!("provider logout exited with {}", exit_label(code)),
                    current_backend_key,
                )
                .await;
                return;
            }
            Err(error) => {
                finish_failure(
                    &config,
                    recovery_terminal_id,
                    current_terminal_id,
                    &display_name,
                    format!("provider logout could not start: {error}"),
                    current_backend_key,
                )
                .await;
                return;
            }
        }
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
        .spawn(&commands.login, Some(&context.cwd), &auth_env, "agent-auth")
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
        crate::spawn_handler::detach_killed_terminal(&config, current_terminal_id, backend_key)
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
            "authentication was cancelled".into(),
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
    // For a shared login we skipped the logout above, so `login` ran with the
    // stale credential still present and can exit 0 without actually
    // re-authenticating (e.g. reporting an already-present but expired
    // session). Trusting that exit code alone would resume straight back into
    // the same failed session and re-arm the auth loop. Confirm the credential
    // is genuinely valid with the provider's own status command before
    // resuming; an isolated login already did a clean logout+login, so it needs
    // no re-check.
    if !credentials_isolated
        && !verify_authenticated(
            &config,
            recovery_terminal_id,
            &commands.status,
            commands.signed_out_marker,
            &latest_context.cwd,
            &auth_env,
        )
        .await
    {
        // The status gate can fail for two reasons: the user cancelled the
        // re-auth mid-probe (the login itself may have succeeded), or the
        // login genuinely didn't take. Report the cancel as such — mirroring
        // the other cancel points — rather than telling a user who just
        // completed sign-in that they're "still logged out".
        let error = if config
            .agent_recovery
            .is_cancelled(recovery_terminal_id)
            .await
        {
            "authentication was cancelled".to_string()
        } else {
            "sign-in did not complete — the agent is still logged out. Please sign in again."
                .to_string()
        };
        finish_failure(
            &config,
            recovery_terminal_id,
            auth_terminal_id,
            &display_name,
            error,
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
        crate::spawn_handler::detach_killed_terminal(&config, auth_terminal_id, &login_key).await;
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

/// Isolated credential-home env for a re-auth flow. Seeds the per-session
/// home so the provider `login` writes into this session's own
/// `CODEX_HOME` rather than the machine-wide login every other session
/// shares. Empty for an agent that keeps the machine-wide login.
fn auth_credential_env(
    config: &ServerConfig,
    context: &AgentResumeContext,
) -> Vec<(String, String)> {
    let Some(agent) = config.agents.get(&context.agent_id) else {
        return Vec::new();
    };
    crate::spawn_plan::seed_credential_home(agent.as_ref(), &context.session_key);
    crate::spawn_plan::credential_home_env(Some(agent.as_ref()), &context.session_key)
}

/// Confirm the agent's login is actually valid before resuming, by running
/// the provider's own status command. Returns `true` (resume) unless the
/// status command exits non-zero OR its output contains `signed_out_marker`
/// (the provider's explicit "not logged in" token).
///
/// Deliberately fails OPEN: an empty status command, a spawn error, no marker
/// configured, or an output that lacks the marker all return `true` so a
/// status-probe quirk can never block an otherwise-successful re-auth. The
/// only signals that stop a resume are the two unambiguous "not logged in"
/// ones. This gate matters only on the shared-login path (isolated logins
/// already did a clean logout+login), so it is called only there.
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
    let output = match config.backend.subscribe(&key).await {
        Ok(mut subscription) => {
            let mut bytes = subscription.replay;
            while let Some(chunk) = subscription.live.recv().await {
                bytes.extend_from_slice(&chunk.bytes);
            }
            bytes
        }
        Err(_) => Vec::new(),
    };
    let code = config.backend.wait_exit(&key).await;
    config
        .agent_recovery
        .set_auth_process(terminal_id, None)
        .await;
    config.backend.release(&key).await;
    let signed_out = signed_out_marker.is_some_and(|marker| {
        // Whitespace-insensitive, case-folded scan on both sides so
        // pretty-printing or casing in the status output can't hide the
        // provider's signed-out token.
        let normalize = |s: &str| -> String {
            s.split_whitespace()
                .collect::<String>()
                .to_ascii_lowercase()
        };
        normalize(&String::from_utf8_lossy(&output)).contains(&normalize(marker))
    });
    code == Some(0) && !signed_out
}

async fn run_quiet_command(
    config: &ServerConfig,
    terminal_id: TerminalId,
    argv: &[String],
    cwd: &std::path::Path,
    env: &[(String, String)],
) -> Result<Option<i32>, crate::backend::BackendError> {
    let key = config
        .backend
        .spawn(argv, Some(cwd), env, "agent-auth")
        .await?;
    config
        .agent_recovery
        .set_auth_process(terminal_id, Some(key.clone()))
        .await;
    if config.agent_recovery.is_cancelled(terminal_id).await {
        let _ = config.backend.kill(&key).await;
    }
    let code = config.backend.wait_exit(&key).await;
    config
        .agent_recovery
        .set_auth_process(terminal_id, None)
        .await;
    config.backend.release(&key).await;
    Ok(code)
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
        let _ = output.send(Event::AgentAuthOutput {
            terminal_id,
            bytes: subscription.replay,
            first_seq: 1,
            seq: subscription.last_seq,
        });
    }
    while let Some(chunk) = subscription.live.recv().await {
        if let Some(output) = config.agent_recovery.output(recovery_terminal_id).await {
            let _ = output.send(Event::AgentAuthOutput {
                terminal_id,
                bytes: chunk.bytes,
                first_seq: chunk.seq,
                seq: chunk.seq,
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
                    false,
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
            true,
            Some(lazybox_ipc::EventSender::from_unbounded(output_tx)),
        )
        .await;

        wait_for_argv(&mock, &["codex", "logout"]).await;
        mock.finish("mock-agent-auth-1", 0).await;
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
    async fn codex_reauthentication_scopes_provider_commands_to_the_isolated_home() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-777")).await;
        start_reauthentication(&config, terminal_id, true, None).await;

        // Logout runs in this session's own CODEX_HOME, so it rewrites only
        // this session's credential — never the machine-wide `~/.codex` the
        // rest of the fleet shares.
        wait_for_argv(&mock, &["codex", "logout"]).await;
        let logout_env = mock
            .env_for("mock-agent-auth-1")
            .await
            .expect("logout command spawned");
        let codex_home = logout_env
            .iter()
            .find(|(k, _)| k == "CODEX_HOME")
            .map(|(_, v)| v.clone())
            .expect("logout is scoped to an isolated CODEX_HOME");
        assert!(
            codex_home.contains("agent-homes/codex/"),
            "unexpected CODEX_HOME: {codex_home}"
        );

        // Login reuses the very same isolated home so the refreshed token
        // lands where this session — and only this session — reads it.
        mock.finish("mock-agent-auth-1", 0).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("interactive login terminal");
        let login_env = mock
            .env_for(&login_key)
            .await
            .expect("login command spawned");
        assert!(
            login_env
                .iter()
                .any(|(k, v)| k == "CODEX_HOME" && v == &codex_home),
            "login must reuse the same isolated CODEX_HOME: {login_env:?}"
        );
    }

    #[tokio::test]
    async fn claude_switch_account_never_signs_out_the_shared_login() {
        // Claude keeps a machine-wide login (no per-session credential home),
        // shared by every other Claude session AND the user's own interactive
        // pane. A single pane's re-auth must therefore NEVER run the provider
        // `logout` — that would sign all of them out at once. Even when the
        // client asks to "switch account" (`switch_account: true`), the shared
        // login is only refreshed in place (login), never invalidated, so a
        // cancelled login can't leave the user logged out.
        let (config, mock, terminal_id) =
            recovery_fixture("claude", Some("claude-conversation-708")).await;
        start_reauthentication(&config, terminal_id, true, None).await;

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

    #[tokio::test]
    async fn login_reporting_signed_out_does_not_resume_into_a_dead_session() {
        // The shared-login path skips logout, so `claude auth login` can exit 0
        // while the session is still not authenticated (an expired credential
        // was already on disk). The status gate must catch that and refuse to
        // resume — otherwise the resumed agent immediately re-fails auth and
        // the loop returns. The conversation stays recoverable for a retry.
        let (config, mock, terminal_id) =
            recovery_fixture("claude", Some("claude-conversation-708")).await;
        start_reauthentication(&config, terminal_id, true, None).await;

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
        start_reauthentication(&config, terminal_id, true, None).await;

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
        start_reauthentication(&config, terminal_id, true, None).await;

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
        start_reauthentication(&config, terminal_id, true, None).await;

        wait_for_argv(&mock, &["codex", "logout"]).await;
        crate::spawn_handler::handle_record_user_message(
            &config,
            terminal_id,
            &UserPrompt {
                text: "new prompt while logout starts".into(),
                timestamp_ms: 2,
                source: lazybox_ipc::PromptSource::Typed,
            },
        )
        .await;
        crate::spawn_handler::handle_record_composing_buffer(
            &config,
            terminal_id,
            "new draft while logout starts",
        )
        .await;
        mock.finish("mock-agent-auth-1", 0).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        let login_key = config
            .terminal
            .backend_key_for(auth_terminal_id)
            .await
            .expect("login backend");
        mock.emit(&login_key, b"private device-code output\r\n")
            .await;
        mock.finish("mock-agent-auth-2", 1).await;

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
            "new prompt while logout starts"
        );
        assert_eq!(
            context.composing_buffer.as_deref(),
            Some("new draft while logout starts")
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
            "new prompt while logout starts"
        );
        assert_eq!(
            failed.composing_buffer.as_deref(),
            Some("new draft while logout starts")
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
    async fn reconnect_during_logout_keeps_the_original_terminal_addressable() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, true, None).await;
        wait_for_argv(&mock, &["codex", "logout"]).await;

        let snapshot = crate::spawn_handler::snapshot_terminals(&config).await;
        assert!(
            snapshot
                .iter()
                .any(|terminal| terminal.terminal_id == terminal_id),
            "logout must not create a gap where reconnect sees no pane"
        );
        let (events, _) = config.agent_recovery.replay_events(None).await;
        assert!(events.iter().any(|event| matches!(
            event,
            Event::AgentAuthProgress {
                recovery_terminal_id,
                terminal_id: current_terminal_id,
                phase: AgentAuthPhase::LoggingOut,
            } if *recovery_terminal_id == terminal_id && *current_terminal_id == terminal_id
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
            true,
            Some(lazybox_ipc::EventSender::from_unbounded(first_tx)),
        )
        .await;
        wait_for_argv(&mock, &["codex", "logout"]).await;
        mock.finish("mock-agent-auth-1", 0).await;
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
        start_reauthentication(&config, terminal_id, true, None).await;
        wait_for_argv(&mock, &["codex", "logout"]).await;
        mock.finish("mock-agent-auth-1", 0).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let auth_terminal_id = wait_for_replacement(&config, terminal_id).await;
        mock.finish("mock-agent-auth-2", 1).await;
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
                    false,
                )
                .await
        );

        start_reauthentication(&config, terminal_id, true, None).await;
        wait_for_argv(&mock, &["codex", "logout"]).await;
        start_reauthentication(&config, other_terminal_id, true, None).await;
        tokio::task::yield_now().await;

        assert_eq!(
            mock.all_argv()
                .await
                .iter()
                .filter(|argv| argv.as_slice() == ["codex", "logout"])
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

        start_reauthentication(&config, terminal_id, true, None).await;
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
        crate::spawn_handler::detach_killed_terminal(&config, terminal_id, &backend_key).await;
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
}
