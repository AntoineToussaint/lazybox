use crate::ServerConfig;
use lazybox_core::{SessionId, SessionKey};
use lazybox_ipc::{AgentAuthPhase, AgentRunAccess, Event, TerminalId, TerminalKind, UserPrompt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
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

#[derive(Debug, Clone)]
struct AuthFlow {
    agent_id: String,
    phase: AgentAuthPhase,
    auth_backend_key: Option<String>,
    cancelled: bool,
}

#[derive(Debug, Clone)]
struct FailedAuth {
    display_name: String,
    error: String,
    backend_key: Option<String>,
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

    async fn begin(&self, terminal_id: TerminalId, agent_id: &str) -> Result<(), TerminalId> {
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
                auth_backend_key: None,
                cancelled: false,
            },
        );
        Ok(())
    }

    async fn take_failed_backend(&self, terminal_id: TerminalId) -> Option<String> {
        self.failures
            .lock()
            .await
            .remove(&terminal_id)
            .and_then(|failure| failure.backend_key)
    }

    async fn record_failure(
        &self,
        terminal_id: TerminalId,
        display_name: String,
        error: String,
        backend_key: Option<String>,
    ) {
        if self.contexts.lock().await.contains_key(&terminal_id) {
            self.failures.lock().await.insert(
                terminal_id,
                FailedAuth {
                    display_name,
                    error,
                    backend_key,
                },
            );
        }
    }

    async fn set_phase(&self, terminal_id: TerminalId, phase: AgentAuthPhase) {
        if let Some(flow) = self.flows.lock().await.get_mut(&terminal_id) {
            flow.phase = phase;
        }
    }

    async fn set_auth_backend(&self, terminal_id: TerminalId, backend_key: Option<String>) {
        if let Some(flow) = self.flows.lock().await.get_mut(&terminal_id) {
            flow.auth_backend_key = backend_key;
        }
    }

    async fn cancel(&self, terminal_id: TerminalId) -> Option<String> {
        let mut flows = self.flows.lock().await;
        let flow = flows.get_mut(&terminal_id)?;
        flow.cancelled = true;
        flow.auth_backend_key.clone()
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
        self.flows.lock().await.contains_key(&terminal_id)
    }

    pub(crate) async fn recovery_terminal_ids(&self) -> std::collections::HashSet<TerminalId> {
        let mut ids: std::collections::HashSet<_> =
            self.flows.lock().await.keys().copied().collect();
        ids.extend(self.failures.lock().await.keys().copied());
        ids
    }

    pub(crate) async fn replay_events(&self) -> Vec<Event> {
        let context_ids: std::collections::HashSet<_> =
            self.contexts.lock().await.keys().copied().collect();
        let flows = self.flows.lock().await.clone();
        let failures = self.failures.lock().await.clone();
        let requirements = self.requirements.lock().await.clone();
        let mut events: Vec<_> = flows
            .iter()
            .filter_map(|(terminal_id, flow)| {
                context_ids
                    .contains(terminal_id)
                    .then_some(Event::AgentAuthProgress {
                        terminal_id: *terminal_id,
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
                    terminal_id: *terminal_id,
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
        events
    }

    async fn live_replacement(
        &self,
        old_terminal_id: TerminalId,
        session_key: &SessionKey,
        agent_id: &str,
    ) -> Option<AgentResumeContext> {
        self.contexts
            .lock()
            .await
            .values()
            .find(|context| {
                context.terminal_id != old_terminal_id
                    && context.backend_key.is_some()
                    && &context.session_key == session_key
                    && context.agent_id == agent_id
            })
            .cloned()
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
    let meta = config.terminal.terminal_meta.lock().await;
    let other_session_count = meta
        .iter()
        .filter(|(id, (_, kind))| {
            **id != terminal_id
                && matches!(kind, TerminalKind::Agent(agent_id) if agent_id == &context.agent_id)
        })
        .count();
    drop(meta);
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

pub(crate) async fn resume_agent(config: &ServerConfig, terminal_id: TerminalId) {
    let Some(context) = config.agent_recovery.context(terminal_id).await else {
        let _ = config.bus.send(Event::AgentAuthFinished {
            terminal_id,
            display_name: "Agent".into(),
            success: false,
            error: Some("this agent pane no longer has resumable launch metadata".into()),
        });
        return;
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
    crate::spawn_handler::handle_spawn(
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
            access: context.access,
            ..Default::default()
        },
    )
    .await;
    if let Some(replacement) = config
        .agent_recovery
        .live_replacement(terminal_id, &context.session_key, &context.agent_id)
        .await
    {
        crate::spawn_handler::restore_terminal_conversation_state(
            config,
            replacement.terminal_id,
            &context.prompt_history,
            context.composing_buffer.as_deref(),
        )
        .await;
        config.agent_recovery.forget(terminal_id).await;
    }
}

pub(crate) async fn start_reauthentication(
    config: &ServerConfig,
    terminal_id: TerminalId,
    switch_account: bool,
) {
    let Some(context) = config.agent_recovery.context(terminal_id).await else {
        let _ = config.bus.send(Event::AgentAuthFinished {
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
            terminal_id,
            display_name,
            success: false,
            error: Some("this agent does not support interactive authentication".into()),
        });
        return;
    };
    if let Err(owner) = config
        .agent_recovery
        .begin(terminal_id, &context.agent_id)
        .await
    {
        if owner == terminal_id {
            return;
        }
        let _ = config.bus.send(Event::AgentAuthFinished {
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

async fn run_reauthentication(
    config: ServerConfig,
    context: AgentResumeContext,
    commands: lazybox_agents::AgentAuthCommands,
    switch_account: bool,
) {
    let terminal_id = context.terminal_id;
    let display_name = agent_display_name(&config, &context.agent_id);
    if let Some(backend_key) = config.agent_recovery.take_failed_backend(terminal_id).await {
        let _ = config.backend.kill(&backend_key).await;
        crate::spawn_handler::detach_killed_terminal(&config, terminal_id, &backend_key).await;
        config.backend.release(&backend_key).await;
    }
    let _ = config.bus.send(Event::AgentAuthProgress {
        terminal_id,
        phase: AgentAuthPhase::LoggingOut,
    });
    if let Some(backend_key) = context.backend_key.as_deref() {
        if let Err(error) = config.backend.kill(backend_key).await {
            finish_failure(
                &config,
                terminal_id,
                &display_name,
                format!("could not stop the blocked agent: {error}"),
                None,
            )
            .await;
            return;
        }
        crate::spawn_handler::detach_killed_terminal(&config, terminal_id, backend_key).await;
    }
    if switch_account {
        let result = run_quiet_command(&config, terminal_id, &commands.logout, &context.cwd).await;
        if config.agent_recovery.is_cancelled(terminal_id).await {
            finish_failure(
                &config,
                terminal_id,
                &display_name,
                "authentication was cancelled".into(),
                None,
            )
            .await;
            return;
        }
        match result {
            Ok(Some(0)) => {}
            Ok(code) => {
                finish_failure(
                    &config,
                    terminal_id,
                    &display_name,
                    format!("provider logout exited with {}", exit_label(code)),
                    None,
                )
                .await;
                return;
            }
            Err(error) => {
                finish_failure(
                    &config,
                    terminal_id,
                    &display_name,
                    format!("provider logout could not start: {error}"),
                    None,
                )
                .await;
                return;
            }
        }
    }
    if config.agent_recovery.is_cancelled(terminal_id).await {
        finish_failure(
            &config,
            terminal_id,
            &display_name,
            "authentication was cancelled".into(),
            None,
        )
        .await;
        return;
    }
    config
        .agent_recovery
        .set_phase(terminal_id, AgentAuthPhase::LoginInteractive)
        .await;
    let _ = config.bus.send(Event::AgentAuthProgress {
        terminal_id,
        phase: AgentAuthPhase::LoginInteractive,
    });
    let login_key = match config
        .backend
        .spawn(&commands.login, Some(&context.cwd), &[], "agent-auth")
        .await
    {
        Ok(key) => key,
        Err(error) => {
            finish_failure(
                &config,
                terminal_id,
                &display_name,
                format!("provider login could not start: {error}"),
                None,
            )
            .await;
            return;
        }
    };
    config
        .agent_recovery
        .set_auth_backend(terminal_id, Some(login_key.clone()))
        .await;
    config
        .terminal
        .register_terminal(
            terminal_id,
            login_key.clone(),
            context.session_key.clone(),
            TerminalKind::Agent(context.agent_id.clone()),
        )
        .await;
    crate::spawn_handler::restore_terminal_conversation_state(
        &config,
        terminal_id,
        &context.prompt_history,
        context.composing_buffer.as_deref(),
    )
    .await;
    let login_code = pump_auth_terminal(&config, terminal_id, &login_key).await;
    config
        .agent_recovery
        .set_auth_backend(terminal_id, None)
        .await;
    if config.agent_recovery.is_cancelled(terminal_id).await {
        finish_failure(
            &config,
            terminal_id,
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
            terminal_id,
            &display_name,
            format!("provider login exited with {}", exit_label(login_code)),
            Some(login_key),
        )
        .await;
        return;
    }
    crate::spawn_handler::detach_killed_terminal(&config, terminal_id, &login_key).await;
    config
        .agent_recovery
        .set_phase(terminal_id, AgentAuthPhase::Resuming)
        .await;
    let _ = config.bus.send(Event::AgentAuthProgress {
        terminal_id,
        phase: AgentAuthPhase::Resuming,
    });
    resume_agent(&config, terminal_id).await;
    let resumed = config
        .agent_recovery
        .live_replacement(terminal_id, &context.session_key, &context.agent_id)
        .await
        .is_some();
    if resumed {
        config.backend.release(&login_key).await;
        let _ = config.bus.send(Event::AgentAuthFinished {
            terminal_id,
            display_name,
            success: true,
            error: None,
        });
    } else {
        config
            .terminal
            .register_terminal(
                terminal_id,
                login_key.clone(),
                context.session_key.clone(),
                TerminalKind::Agent(context.agent_id.clone()),
            )
            .await;
        crate::spawn_handler::restore_terminal_conversation_state(
            &config,
            terminal_id,
            &context.prompt_history,
            context.composing_buffer.as_deref(),
        )
        .await;
        finish_failure(
            &config,
            terminal_id,
            &display_name,
            "the agent could not be resumed".into(),
            Some(login_key),
        )
        .await;
        return;
    }
    config.agent_recovery.finish(terminal_id).await;
}

async fn run_quiet_command(
    config: &ServerConfig,
    terminal_id: TerminalId,
    argv: &[String],
    cwd: &std::path::Path,
) -> Result<Option<i32>, crate::backend::BackendError> {
    let key = config
        .backend
        .spawn(argv, Some(cwd), &[], "agent-auth")
        .await?;
    config
        .agent_recovery
        .set_auth_backend(terminal_id, Some(key.clone()))
        .await;
    let code = config.backend.wait_exit(&key).await;
    config
        .agent_recovery
        .set_auth_backend(terminal_id, None)
        .await;
    config.backend.release(&key).await;
    Ok(code)
}

async fn pump_auth_terminal(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
) -> Option<i32> {
    let Ok(mut subscription) = config.backend.subscribe(backend_key).await else {
        return config.backend.wait_exit(backend_key).await;
    };
    if !subscription.replay.is_empty() {
        let _ = config.bus.send(Event::TerminalOutput {
            terminal_id,
            bytes: subscription.replay,
            first_seq: 1,
            seq: subscription.last_seq,
        });
    }
    while let Some(chunk) = subscription.live.recv().await {
        let _ = config.bus.send(Event::TerminalOutput {
            terminal_id,
            bytes: chunk.bytes,
            first_seq: chunk.seq,
            seq: chunk.seq,
        });
    }
    config.backend.wait_exit(backend_key).await
}

async fn finish_failure(
    config: &ServerConfig,
    terminal_id: TerminalId,
    display_name: &str,
    error: String,
    backend_key: Option<String>,
) {
    config
        .agent_recovery
        .record_failure(
            terminal_id,
            display_name.to_string(),
            error.clone(),
            backend_key,
        )
        .await;
    config.agent_recovery.finish(terminal_id).await;
    let _ = config.bus.send(Event::AgentAuthFinished {
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

    async fn wait_for_argv(mock: &crate::backend::MockBackend, expected: &[&str]) -> Vec<String> {
        for _ in 0..10_000 {
            let all = mock.all_argv().await;
            if let Some(argv) = all.into_iter().find(|argv| {
                argv.iter()
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

    #[tokio::test]
    async fn reauthentication_runs_provider_commands_and_exact_resume() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, true).await;

        wait_for_argv(&mock, &["codex", "logout"]).await;
        mock.finish("mock-agent-auth-1", 0).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
        let login_key = config
            .terminal
            .backend_key_for(terminal_id)
            .await
            .expect("interactive login terminal");
        assert!(
            crate::spawn_handler::handle_write(
                &config,
                terminal_id,
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
        assert!(
            config
                .agent_recovery
                .live_replacement(
                    terminal_id,
                    &SessionKey::new("github:owner/repo#708"),
                    "codex",
                )
                .await
                .is_some()
        );
        let snapshot = crate::spawn_handler::snapshot_terminals(&config).await;
        let resumed = snapshot
            .iter()
            .find(|terminal| terminal.terminal_id != terminal_id)
            .expect("resumed terminal snapshot");
        assert_eq!(resumed.prompt_history[0].text, "keep this prompt");
        assert_eq!(resumed.composing_buffer.as_deref(), Some("keep this draft"));
    }

    #[tokio::test]
    async fn claude_reauthentication_uses_provider_commands_and_exact_resume() {
        let (config, mock, terminal_id) =
            recovery_fixture("claude", Some("claude-conversation-708")).await;
        start_reauthentication(&config, terminal_id, true).await;

        wait_for_argv(&mock, &["claude", "auth", "logout"]).await;
        mock.finish("mock-agent-auth-1", 0).await;
        wait_for_argv(&mock, &["claude", "auth", "login"]).await;
        let login_key = config
            .terminal
            .backend_key_for(terminal_id)
            .await
            .expect("interactive login terminal");
        mock.finish(&login_key, 0).await;
        wait_for_argv(&mock, &["claude", "--resume", "claude-conversation-708"]).await;
    }

    #[tokio::test]
    async fn failed_login_keeps_the_conversation_recoverable() {
        let (config, mock, terminal_id) = recovery_fixture("codex", Some("conversation-708")).await;
        start_reauthentication(&config, terminal_id, true).await;

        wait_for_argv(&mock, &["codex", "logout"]).await;
        mock.finish("mock-agent-auth-1", 0).await;
        wait_for_argv(&mock, &["codex", "login"]).await;
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
        assert_eq!(context.prompt_history[0].text, "keep this prompt");
        assert_eq!(context.composing_buffer.as_deref(), Some("keep this draft"));
        assert!(mock.all_argv().await.iter().all(|argv| !argv.starts_with(&[
            "codex".into(),
            "resume".into(),
            "conversation-708".into()
        ])));
        let snapshot = crate::spawn_handler::snapshot_terminals(&config).await;
        let failed = snapshot
            .iter()
            .find(|terminal| terminal.terminal_id == terminal_id)
            .expect("failed auth terminal remains reconnectable");
        assert!(failed.authenticating);
        assert_eq!(failed.prompt_history[0].text, "keep this prompt");
        assert_eq!(failed.composing_buffer.as_deref(), Some("keep this draft"));
        assert!(
            config
                .agent_recovery
                .replay_events()
                .await
                .iter()
                .any(|event| matches!(
                    event,
                    Event::AgentAuthFinished {
                        terminal_id: id,
                        success: false,
                        ..
                    } if *id == terminal_id
                ))
        );
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

        start_reauthentication(&config, terminal_id, true).await;
        wait_for_argv(&mock, &["codex", "logout"]).await;
        start_reauthentication(&config, other_terminal_id, true).await;
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
        assert!(
            config
                .agent_recovery
                .replay_events()
                .await
                .iter()
                .any(|event| matches!(
                    event,
                    Event::AgentAuthRequired {
                        terminal_id: id,
                        ..
                    } if *id == terminal_id
                ))
        );
        config
            .agent_recovery
            .requirements
            .lock()
            .await
            .remove(&terminal_id);

        start_reauthentication(&config, terminal_id, true).await;
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
