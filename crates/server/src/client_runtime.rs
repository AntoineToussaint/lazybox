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
        // Re-key legacy per-backend_key prompt-history / draft rows onto the
        // stable workspace scheme BEFORE recovery, so both the reattach
        // (`recover_sessions`) and the respawn (`restore_persisted_sessions`)
        // passes read the migrated rows. One-time, guarded by a KV flag (a
        // no-op single `get_kv` after the first run). Awaited inline — like the
        // sibling `migrate_legacy_sandbox` — rather than prepended to the
        // background recovery task, so it can't push `recover_sessions`'
        // `backend.list()` past a spawn racing in during startup (which would
        // reattach that fresh session as a duplicate terminal).
        crate::spawn_handler::migrate_prompt_history_to_workspace_keys(&config).await;

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
        tasks.push(crate::working_watchdog::spawn(&config));
        tasks.push(crate::error_inbox::spawn(&config));
        tasks.push(crate::stats_accumulator::spawn(&config));
        tasks.push(crate::session_cost::spawn(&config));
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

    /// Regression: startup recovery must never block the daemon launch, and
    /// must never be abandoned by a wall-clock deadline. It previously ran
    /// inline under a 5s timeout that, on a busy box, expired MID-LOOP and
    /// orphaned every still-live session (neither reattached nor re-spawned).
    ///
    /// Here a survivor is alive in the backend but `list()` is parked for 30s.
    /// `start()` must still return promptly (survivor not yet reattached), and
    /// once the inventory resolves the background recovery task must reattach
    /// the survivor with no deadline to drop it.
    #[tokio::test(start_paused = true)]
    async fn start_backgrounds_recovery_and_never_drops_survivors() {
        use crate::backend::SessionBackend;

        let (config, backend) = ServerConfig::in_memory_with_mock();
        // A session that survived the previous run: known to the backend,
        // absent from this fresh config.
        let survivor = backend
            .spawn(&["echo".into(), "hi".into()], None, &[], "survivor")
            .await
            .expect("spawn survivor");
        // Make the backend inventory slow so recovery parks at its very first
        // step — longer than the retired 5s recovery timeout.
        backend.set_list_delay(Duration::from_secs(30)).await;

        let runtime = ClientRuntime::start(
            config.clone(),
            ClientRuntimeOptions {
                poll_interval: Duration::from_secs(60),
                restore_persisted_sessions: false,
                slack: None,
            },
        )
        .await;

        // The discriminating assertion: the survivor is reattached even though
        // `list()` is parked far past the retired 5s recovery timeout. Under
        // the old inline-with-timeout path the paused clock would auto-advance
        // to 5s, fire the timeout, abandon recovery, and the survivor would
        // stay orphaned — this loop would then spin out and fail. Idling sleeps
        // let the paused clock auto-advance through the delayed inventory so the
        // background recovery task can run to completion.
        let mut reattached = false;
        for _ in 0..200 {
            if !config.terminal.is_empty().await {
                reattached = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert!(
            reattached,
            "survivor was never reattached — recovery was blocked or abandoned",
        );
        let ids = config.terminal.terminal_ids().await;
        assert_eq!(
            ids.len(),
            1,
            "expected one reattached survivor, got {ids:?}"
        );
        assert_eq!(
            config.terminal.backend_key_for(ids[0]).await.as_deref(),
            Some(survivor.as_str()),
        );

        runtime.shutdown().await;
    }
}
