use crate::ServerConfig;
use lazybox_config::SlackConfig;
use std::time::Duration;
use tokio::task::JoinHandle;

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
        let mut tasks = Vec::new();
        // Session recovery runs OFF the launch critical path. It used to be
        // awaited under a 5s wall-clock, but that timeout cancelled the
        // reattach loop MID-ITERATION and silently abandoned every live tmux
        // session it hadn't reached yet — deterministically the
        // alphabetically-late workspaces, since `backend.list()` returns
        // sorted names. Those sessions stayed alive in tmux but unregistered,
        // and the restore pass then REFUSED to resurrect them (its
        // anti-double-spawn dedupe sees them alive in `backend.list()`),
        // stranding them in limbo — invisible in the UI though the agent kept
        // running. A slow or remote tmux server made the 5s budget hopeless for
        // the whole fleet, collapsing recovery entirely.
        //
        // Instead: recover to completion in the background, then chain the
        // restore pass after it (preserving the ordering the dedupe relies on
        // — restore must observe recovery's registrations). Launch stays
        // responsive because this is a detached task, not an awaited one, and a
        // wedged tmux cannot hang it: every tmux call carries its own per-op
        // timeout, so a stuck session errors and the loop moves on rather than
        // the whole pass being cancelled.
        let recovery_config = config.clone();
        let restore_persisted = options.restore_persisted_sessions;
        tasks.push(tokio::spawn(async move {
            crate::spawn_handler::recover_sessions(&recovery_config).await;
            if restore_persisted {
                crate::spawn_handler::restore_persisted_sessions(&recovery_config).await;
            }
        }));

        crate::workspace::migrate_legacy_sandbox(&config);
        tasks.push(crate::polling::spawn(config.clone(), options.poll_interval));
        tasks.push(crate::working_claims::spawn(config.clone()));
        tasks.push(crate::error_inbox::spawn(&config));
        tasks.push(crate::stats_accumulator::spawn(&config));
        tasks.push(crate::box_liveness::spawn(&config));
        if let Some(task) = crate::keep_awake::spawn(&config) {
            tasks.push(task);
        }
        tasks.push(crate::auto_wait::spawn(&config));
        // #1198: hourly reap of sessions whose PR/issue closed past the
        // grace window (and a startup-restore gate on the same predicate).
        tasks.push(crate::session_reaper::spawn(&config));
        tasks.push(crate::agent_updates::spawn_scheduled(config.clone()));
        if let Some(task) = crate::proxy::spawn(&config).await {
            tasks.push(task);
        }
        if let Some(task) = crate::codex_quota::spawn(&config) {
            tasks.push(task);
        }
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
