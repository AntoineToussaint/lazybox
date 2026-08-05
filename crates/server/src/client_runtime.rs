use crate::ServerConfig;
use lazybox_config::SlackConfig;
use std::time::Duration;
use tokio::task::JoinHandle;

const RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ClientRuntimeOptions {
    pub poll_interval: Duration,
    pub restore_persisted_sessions: bool,
    pub slack: Option<SlackConfig>,
}

pub struct ClientRuntime {
    tasks: Vec<JoinHandle<()>>,
}

impl ClientRuntime {
    pub async fn start(config: ServerConfig, options: ClientRuntimeOptions) -> Self {
        if tokio::time::timeout(
            RECOVERY_TIMEOUT,
            crate::spawn_handler::recover_sessions(&config),
        )
        .await
        .is_err()
        {
            tracing::warn!("recover_sessions timed out after 5s — continuing without recovery");
        }

        let mut tasks = Vec::new();
        if options.restore_persisted_sessions {
            let restore_config = config.clone();
            tasks.push(tokio::spawn(async move {
                crate::spawn_handler::restore_persisted_sessions(&restore_config).await;
            }));
        }

        crate::workspace::migrate_legacy_sandbox(&config);
        tasks.push(crate::polling::spawn(config.clone(), options.poll_interval));
        tasks.push(crate::error_inbox::spawn(&config));
        if let Some(task) = crate::keep_awake::spawn(&config) {
            tasks.push(task);
        }
        tasks.push(crate::auto_wait::spawn(&config));
        tasks.push(crate::agent_updates::spawn_scheduled(config.clone()));
        if let Some(slack) = options.slack
            && let Some(task) = crate::slack::spawn(config, slack)
        {
            tasks.push(task);
        }

        Self { tasks }
    }

    pub async fn shutdown(mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for ClientRuntime {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_finishes_every_owned_service() {
        let runtime = ClientRuntime::start(
            ServerConfig::in_memory(),
            ClientRuntimeOptions {
                poll_interval: Duration::from_secs(60),
                restore_persisted_sessions: false,
                slack: None,
            },
        )
        .await;
        let tasks = runtime
            .tasks
            .iter()
            .map(JoinHandle::abort_handle)
            .collect::<Vec<_>>();

        runtime.shutdown().await;

        assert!(tasks.iter().all(tokio::task::AbortHandle::is_finished));
    }
}
