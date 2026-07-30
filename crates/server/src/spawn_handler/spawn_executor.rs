use super::{
    AgentStateDurability, cancel_spawn_for_deleted_workspace, hook_backend_key_path, hook_command,
    hook_exe, initialize_agent_state_generation, persist_no_permission,
    persist_pty_launch_generation, persist_terminal_meta, wake_poll_for_terminal_kind,
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
    tracing::info!(
        program = plan.argv.first().map(String::as_str).unwrap_or("<empty>"),
        arg_count = plan.argv.len().saturating_sub(1),
        cwd_path = ?plan.cwd,
        hint = %plan.hint,
        env_count = plan.env.len(),
        "execute_spawn_plan: calling backend.spawn"
    );
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
        cwd: _,
        env: _,
        hint: _,
        persist_key: _,
        owning_session,
        initial_prompt,
        terminal_id,
        hook_settings,
        model_label,
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

    config
        .terminal
        .record_spawn_attributes(
            terminal_id,
            owning_session,
            skip_permissions,
            landed_on_main,
            model_label.as_deref(),
        )
        .await;
    {
        let mut registration = config.terminal.lock_registration().await;
        registration.register(
            terminal_id,
            backend_key.clone(),
            session_key.clone(),
            kind.clone(),
            agent_state_generation,
        );
        persist_terminal_meta(config, &backend_key, &session_key, &kind).await;
        if let Some(agent) = agent.as_deref() {
            persist_pty_launch_generation(config, &backend_key, agent.pty_launch_generation())
                .await;
        }
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
    if let Err(error) = config.bus.send(Event::TerminalSpawned {
        terminal_id,
        session_key: session_key.clone(),
        kind: kind.clone(),
        no_permission: skip_permissions,
        on_main: landed_on_main,
        model_label,
    }) {
        tracing::error!("execute_spawn_plan: bus.send(TerminalSpawned) failed: {error}");
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
