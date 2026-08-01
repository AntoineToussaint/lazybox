use super::{
    AgentStateDurability, cancel_spawn_for_deleted_workspace, hook_backend_key_path, hook_command,
    hook_exe, initialize_agent_state_generation, persist_agent_access,
    persist_agent_resume_context, persist_no_permission, persist_pty_launch_generation,
    persist_terminal_meta, restore_backend_conversation_state, wake_poll_for_terminal_kind,
    write_hook_backend_key, write_hook_settings,
};
use crate::{ServerConfig, spawn_plan::SpawnPlan};
use lazybox_core::SessionKey;
use lazybox_ipc::{Event, TerminalId, TerminalKind};

pub(super) struct ExecutedSpawn {
    pub(super) backend_key: String,
    pub(super) session_key: SessionKey,
    pub(super) kind: TerminalKind,
    pub(super) initial_prompt: Option<String>,
    pub(super) terminal_id: TerminalId,
    pub(super) state_durability: Option<AgentStateDurability>,
}

pub(super) enum SpawnExecutionOutcome {
    Spawned(ExecutedSpawn),
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SpawnExecutionError {
    #[error("{0}")]
    Backend(#[from] crate::backend::BackendError),
    #[error("agent lifecycle persistence failed: {0}")]
    LifecyclePersistence(String),
    #[error("agent access policy persistence failed: {0}")]
    AccessPersistence(String),
}

pub(super) async fn execute_spawn_plan(
    config: &ServerConfig,
    plan: SpawnPlan,
    workspace_registration_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    started_at: std::time::Instant,
) -> Result<SpawnExecutionOutcome, SpawnExecutionError> {
    if plan.flags.no_permission
        && let TerminalKind::Agent(agent_id) = &plan.kind
        && let Some(agent) = config.agents.get(agent_id)
    {
        agent.prepare_unattended(&plan.cwd);
    }
    let agent = match &plan.kind {
        TerminalKind::Agent(id) => config.agents.get(id),
        _ => None,
    };
    // Seed this session's isolated credential home from the machine-wide
    // login before launch, so an isolating agent (Codex) starts
    // authenticated in its own `CODEX_HOME` — the env var itself is already
    // baked into `plan.env` by the spawn plan. No-op for agents that keep
    // the machine-wide login.
    if let Some(agent) = &agent {
        crate::spawn_plan::seed_credential_home(agent.as_ref(), &plan.session_key);
    }
    tracing::info!(
        program = plan.argv.first().map(String::as_str).unwrap_or("<empty>"),
        arg_count = plan.argv.len().saturating_sub(1),
        cwd_path = ?plan.cwd,
        hint = %plan.hint,
        env_count = plan.env.len(),
        "execute_spawn_plan: calling backend.spawn"
    );
    // Model identity is meaningful only for agent spawns; shells and log
    // tails carry none. Surface the resolved tier so which model actually
    // ran is visible even when no `◆` badge shows (issue #748).
    if let TerminalKind::Agent(agent_id) = &plan.kind {
        tracing::info!(
            agent = %agent_id,
            model_alias = plan.model_alias.as_deref().unwrap_or("<none>"),
            model = plan.model_label.as_deref().unwrap_or("<agent built-in default>"),
            "execute_spawn_plan: resolved agent model tier"
        );
    }
    let backend_key = match config
        .backend
        .spawn_persistent(
            &plan.argv,
            Some(&plan.cwd),
            &plan.env,
            &plan.hint,
            plan.persist_key.as_deref(),
        )
        .await
    {
        Ok(key) => key,
        Err(error) => {
            if let Some(path) = &plan.hook_settings {
                let _ = std::fs::remove_file(path);
            }
            return Err(error.into());
        }
    };
    let SpawnPlan {
        session_key,
        kind,
        argv: _,
        cwd: plan_cwd,
        env: _,
        hint: _,
        persist_key: _,
        owning_session,
        initial_prompt,
        terminal_id,
        hook_settings,
        model_label,
        model_alias,
        provider_session_id,
        replace_terminal_id,
        prompt_history,
        composing_buffer,
        access,
        flags,
    } = plan;
    let skip_permissions = flags.no_permission;
    let landed_on_main = flags.on_main;
    tracing::info!(
        %backend_key,
        elapsed_ms = started_at.elapsed().as_millis(),
        "execute_spawn_plan: backend.spawn ok",
    );
    if cancel_spawn_for_deleted_workspace(config, &session_key, &backend_key).await {
        if let Some(path) = hook_settings {
            let _ = std::fs::remove_file(path);
        }
        return Ok(SpawnExecutionOutcome::Cancelled);
    }
    let agent_session_id = matches!(&kind, TerminalKind::Agent(_))
        .then_some(owning_session)
        .flatten();
    if let Err(error) = persist_agent_access(config, &backend_key, agent_session_id, access).await {
        tracing::error!(
            %backend_key,
            ?terminal_id,
            %error,
            "agent spawn rolled back because its host-access policy was not durable"
        );
        if let Err(kill_error) = config.backend.kill(&backend_key).await {
            tracing::error!(
                %backend_key,
                %kill_error,
                "failed to roll back agent backend after access-policy persistence failure"
            );
        }
        if let Some(path) = hook_settings {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(hook_backend_key_path(terminal_id));
        return Err(SpawnExecutionError::AccessPersistence(error));
    }
    if hook_settings.is_some()
        && let Some(exe) = hook_exe()
    {
        let _ = write_hook_settings(
            config,
            &kind,
            terminal_id,
            &hook_command(&exe, &backend_key),
        );
    }
    if flags.uses_argv_hooks {
        write_hook_backend_key(terminal_id, &backend_key);
    }
    let state_durability = if agent.is_some() {
        match initialize_agent_state_generation(config, &backend_key, terminal_id.0).await {
            Ok(durability) => Some(durability),
            Err(error) => {
                tracing::error!(
                    %backend_key,
                    ?terminal_id,
                    %error,
                    "agent spawn rolled back because lifecycle generation was not durable"
                );
                if let Err(kill_error) = config.backend.kill(&backend_key).await {
                    tracing::error!(
                        %backend_key,
                        %kill_error,
                        "failed to roll back agent backend after lifecycle persistence failure"
                    );
                }
                if let Some(path) = hook_settings {
                    let _ = std::fs::remove_file(path);
                }
                let _ = std::fs::remove_file(hook_backend_key_path(terminal_id));
                return Err(SpawnExecutionError::LifecyclePersistence(error));
            }
        }
    } else {
        None
    };
    let agent_state_generation = state_durability
        .as_ref()
        .map(|durability| durability.generation);

    if !prompt_history.is_empty() || composing_buffer.is_some() {
        restore_backend_conversation_state(
            config,
            &backend_key,
            &prompt_history,
            composing_buffer.as_deref(),
        )
        .await;
    }
    config
        .terminal
        .record_spawn_attributes(
            terminal_id,
            owning_session,
            access,
            skip_permissions,
            landed_on_main,
            model_label.as_deref(),
        )
        .await;
    let resume_context = if let TerminalKind::Agent(agent_id) = &kind {
        let context = crate::agent_auth::AgentResumeContext {
            terminal_id,
            session_key: session_key.clone(),
            session_id: owning_session,
            agent_id: agent_id.clone(),
            cwd: plan_cwd,
            backend_key: Some(backend_key.clone()),
            on_main: landed_on_main,
            model_alias,
            access,
            no_permission: skip_permissions,
            provider_session_id,
            prompt_history,
            composing_buffer,
        };
        config.agent_recovery.remember_spawn(context.clone()).await;
        Some(context)
    } else {
        None
    };
    {
        let mut registration = config.terminal.lock_registration().await;
        if let Some(old_terminal_id) = replace_terminal_id {
            registration.register_replacement(
                old_terminal_id,
                terminal_id,
                backend_key.clone(),
                session_key.clone(),
                kind.clone(),
                agent_state_generation,
                false,
            );
        } else {
            registration.register(
                terminal_id,
                backend_key.clone(),
                session_key.clone(),
                kind.clone(),
                agent_state_generation,
            );
        }
        persist_terminal_meta(config, &backend_key, &session_key, &kind).await;
        if let Some(agent) = agent.as_deref() {
            persist_pty_launch_generation(config, &backend_key, agent.pty_launch_generation())
                .await;
        }
    }
    if let Some(context) = &resume_context {
        persist_agent_resume_context(config, &backend_key, context).await;
    }
    wake_poll_for_terminal_kind(config, &kind);
    drop(workspace_registration_guard);
    persist_no_permission(config, &backend_key, skip_permissions).await;

    let subscriber_count = config.bus.receiver_count();
    tracing::info!(
        ?terminal_id,
        %session_key,
        ?kind,
        subscriber_count,
        "execute_spawn_plan: broadcasting TerminalSpawned"
    );
    let event = if let Some(old_terminal_id) = replace_terminal_id {
        Event::TerminalReplaced {
            old_terminal_id,
            terminal_id,
            session_key: session_key.clone(),
            kind: kind.clone(),
            no_permission: skip_permissions,
            on_main: landed_on_main,
            model_label,
            authenticating: false,
        }
    } else {
        Event::TerminalSpawned {
            terminal_id,
            session_key: session_key.clone(),
            kind: kind.clone(),
            no_permission: skip_permissions,
            on_main: landed_on_main,
            model_label,
        }
    };
    if let Err(error) = config.bus.send(event) {
        tracing::error!("execute_spawn_plan: bus.send(spawn lifecycle) failed: {error}");
    }

    Ok(SpawnExecutionOutcome::Spawned(ExecutedSpawn {
        backend_key,
        session_key,
        kind,
        initial_prompt,
        terminal_id,
        state_durability,
    }))
}
