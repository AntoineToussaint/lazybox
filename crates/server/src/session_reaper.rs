//! Reap persistent sessions whose work is over (#1198).
//!
//! Sessions are persistent *intent* — "there should be a claude here" —
//! and the daemon deliberately restores them across restarts. But once a
//! workspace's PR merges or its issue closes, that intent has expired:
//! the agent sits idle at ~110 MB forever (tmux never reaps), and a week
//! of normal use was measured at tens of GB across dozens of stale
//! sessions. This module closes the loop:
//!
//! - A periodic sweep kills the live terminals of workspaces whose
//!   PR/issue has been closed/merged longer than
//!   `agent.reap_closed_after` (default 48h, `0s` disables).
//! - Startup restore consults the same predicate (`closed_beyond`) so
//!   a reaped session isn't resurrected at the next boot only to be
//!   reaped again.
//!
//! The workspace row, its sessions' metadata (worktree mapping, prompt
//! history) and the worktree itself are untouched — only the running
//! processes stop. `w w` starts a fresh agent in a keystroke. Errors are
//! non-fatal: a kill that fails is retried on the next sweep, never
//! escalated into workspace mutation.

use crate::ServerConfig;
use lazybox_ipc::Event;
use std::time::Duration;

/// Grace before the first sweep, so startup recovery/restore has fully
/// settled and a just-recovered fleet isn't raced mid-registration.
const FIRST_SWEEP_DELAY: Duration = Duration::from_secs(120);
/// Cadence between sweeps. Staleness is measured in days; hourly is
/// plenty and keeps the store scan negligible.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Whether `workspace`'s primary task has been merged/closed for longer
/// than `threshold`. Pure — shared by the periodic sweep and the
/// startup-restore gate so they can't disagree. A task without a
/// `closed_at` stamp falls back to `updated_at` (later than the close,
/// so strictly conservative).
pub(crate) fn closed_beyond(
    workspace: &lazybox_core::Workspace,
    threshold: Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let Some(task) = workspace.primary_task() else {
        return false;
    };
    if !matches!(
        task.state,
        lazybox_core::TaskState::Merged | lazybox_core::TaskState::Closed
    ) {
        return false;
    }
    let ended = task.closed_at.unwrap_or(task.updated_at);
    // An unrepresentable (astronomically large) threshold means "never".
    let Ok(threshold) = chrono::Duration::from_std(threshold) else {
        return false;
    };
    now.signed_duration_since(ended) >= threshold
}

/// One reap pass: kill the live terminals of every closed-beyond-grace
/// workspace. Returns how many terminals were reaped.
pub(crate) async fn sweep(config: &ServerConfig) -> usize {
    let Some(threshold) = lazybox_config::Config::load()
        .unwrap_or_default()
        .agent
        .reap_closed_after()
    else {
        return 0;
    };
    sweep_with(config, threshold, chrono::Utc::now()).await
}

/// [`sweep`] with the grace window and clock injected — the testable
/// core, independent of the user's on-disk config.
pub(crate) async fn sweep_with(
    config: &ServerConfig,
    threshold: Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> usize {
    let records = match crate::store_blocking(&config.store, |store| store.list_workspaces()).await
    {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "session reap: list_workspaces failed — skipping sweep");
            return 0;
        }
    };
    let mut reaped = 0usize;
    let mut workspaces_touched = 0usize;
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = serde_json::from_str::<lazybox_core::Workspace>(&json) else {
            continue;
        };
        if !closed_beyond(&workspace, threshold, now) {
            continue;
        }
        let killed = reap_workspace_terminals(config, workspace.key.as_str()).await;
        if killed > 0 {
            tracing::info!(
                workspace = %workspace.key,
                killed,
                "reaped idle session(s) — PR/issue closed past agent.reap_closed_after"
            );
            workspaces_touched += 1;
            reaped += killed;
        }
    }
    if reaped > 0 {
        let _ = config.bus.send(Event::Notification {
            title: "Idle sessions reaped".to_string(),
            body: format!(
                "stopped {reaped} agent/shell session(s) across {workspaces_touched} \
                 closed workspace(s) — w w respawns one anytime \
                 (agent.reap_closed_after tunes this)"
            ),
        });
    }
    reaped
}

/// Kill every live terminal mapped to `key_str`, via the same safe
/// sequence workspace teardown uses (acquire the interaction lock, kill
/// the backend session, hand teardown to the lifecycle owner). A failed
/// kill is skipped — the entry stays live and the next sweep retries.
async fn reap_workspace_terminals(config: &ServerConfig, key_str: &str) -> usize {
    let to_kill: Vec<(lazybox_ipc::TerminalId, String)> = config
        .terminal
        .entries
        .lock()
        .await
        .iter()
        .filter(|(_, entry)| {
            !entry.finishing
                && entry
                    .meta
                    .as_ref()
                    .is_some_and(|(sk, _)| sk.as_str() == key_str)
        })
        .filter_map(|(tid, entry)| entry.backend_key.clone().map(|key| (*tid, key)))
        .collect();
    let mut killed = 0usize;
    for (tid, backend_key) in to_kill {
        let Some(interaction) = crate::terminal_io::acquire_live(config, tid, &backend_key).await
        else {
            // The output pump won teardown after the snapshot — done.
            continue;
        };
        if let Err(error) = config.backend.kill(&backend_key).await {
            tracing::warn!(%backend_key, %error, "session reap: kill failed — will retry next sweep");
            continue;
        }
        drop(interaction);
        crate::spawn_handler::detach_killed_terminal(config, tid, &backend_key).await;
        killed += 1;
    }
    killed
}

/// Long-lived hourly reaper. First pass is delayed so startup
/// recovery/restore settles first.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    let config = config.clone();
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SWEEP_DELAY).await;
        loop {
            sweep(&config).await;
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SessionBackend;
    use lazybox_core::{SessionKey, Task, TaskId, TaskState, Workspace, WorkspaceKey};
    use lazybox_ipc::TerminalKind;

    fn task_in_state(state: TaskState, closed_days_ago: Option<i64>) -> Task {
        let now = chrono::Utc::now();
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            },
            title: "t".into(),
            body: None,
            state,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: now - chrono::Duration::days(closed_days_ago.unwrap_or(0)),
            created_at: None,
            closed_at: closed_days_ago.map(|d| now - chrono::Duration::days(d)),
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    fn workspace_with(key: &str, task: Option<Task>) -> Workspace {
        let mut workspace =
            Workspace::empty(WorkspaceKey::new(key), "reap-test", chrono::Utc::now());
        workspace.pr = task;
        workspace
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);

    /// The predicate only fires for merged/closed tasks past the grace
    /// window; open tasks, fresh closes, task-less workspaces, and a
    /// missing `closed_at` (falls back to `updated_at`) all hold.
    #[test]
    fn closed_beyond_gates_on_state_and_age() {
        let now = chrono::Utc::now();
        let threshold = 2 * 24 * 3600;
        let threshold = Duration::from_secs(threshold);

        let merged_old = workspace_with(
            "github:o/r#1",
            Some(task_in_state(TaskState::Merged, Some(3))),
        );
        assert!(closed_beyond(&merged_old, threshold, now));

        let closed_old = workspace_with(
            "github:o/r#1",
            Some(task_in_state(TaskState::Closed, Some(3))),
        );
        assert!(closed_beyond(&closed_old, threshold, now));

        let merged_fresh = workspace_with(
            "github:o/r#1",
            Some(task_in_state(TaskState::Merged, Some(1))),
        );
        assert!(!closed_beyond(&merged_fresh, threshold, now));

        let open_old = workspace_with(
            "github:o/r#1",
            Some(task_in_state(TaskState::Open, Some(30))),
        );
        assert!(!closed_beyond(&open_old, threshold, now));

        let no_task = workspace_with("github:o/r#1", None);
        assert!(!closed_beyond(&no_task, threshold, now));

        // No closed_at stamp: fall back to updated_at.
        let mut merged_no_stamp = task_in_state(TaskState::Merged, None);
        merged_no_stamp.closed_at = None;
        merged_no_stamp.updated_at = now - chrono::Duration::days(5);
        let workspace = workspace_with("github:o/r#1", Some(merged_no_stamp));
        assert!(closed_beyond(&workspace, threshold, now));
    }

    /// #1198 journey: an agent session on a workspace whose PR merged
    /// past the grace is reaped by the sweep — the backend session is
    /// killed, a second sweep finds nothing — while a live agent on an
    /// open workspace is untouched.
    #[tokio::test]
    async fn sweep_reaps_closed_workspaces_and_spares_open_ones() {
        let (config, backend) = crate::ServerConfig::in_memory_with_mock();

        // Two surviving backend sessions, adopted through the real
        // recovery path so terminal entries exist just like after a
        // daemon restart.
        let merged_key = SessionKey::new("github:o/r#1");
        let open_key = SessionKey::new("github:o/r#2");
        let merged_backend = backend
            .spawn(&[], None, &[], "reap-merged")
            .await
            .expect("spawn merged-workspace session");
        let open_backend = backend
            .spawn(&[], None, &[], "reap-open")
            .await
            .expect("spawn open-workspace session");
        crate::spawn_handler::persist_terminal_meta(
            &config,
            &merged_backend,
            &merged_key,
            &TerminalKind::Agent("claude".into()),
        )
        .await;
        crate::spawn_handler::persist_terminal_meta(
            &config,
            &open_backend,
            &open_key,
            &TerminalKind::Agent("claude".into()),
        )
        .await;
        crate::spawn_handler::recover_sessions(&config).await;

        for (key, days) in [(&merged_key, Some(3i64)), (&open_key, None)] {
            let state = if days.is_some() {
                TaskState::Merged
            } else {
                TaskState::Open
            };
            let workspace = workspace_with(key.as_str(), Some(task_in_state(state, days)));
            config
                .store
                .save_workspace(&lazybox_store::WorkspaceRecord {
                    key: key.as_str().to_string(),
                    created_at: workspace.created_at,
                    workspace_json: Some(
                        serde_json::to_string(&workspace).expect("workspace json"),
                    ),
                })
                .expect("seed workspace");
        }

        let reaped = sweep_with(&config, 2 * DAY, chrono::Utc::now()).await;
        assert_eq!(
            reaped, 1,
            "exactly the merged workspace's session is reaped"
        );
        let survivors = backend.list().await.expect("list backend sessions");
        assert_eq!(
            survivors,
            vec![open_backend.clone()],
            "open workspace's agent survives; merged one is gone",
        );

        // Idempotent: nothing left to reap.
        assert_eq!(sweep_with(&config, 2 * DAY, chrono::Utc::now()).await, 0);
    }
}
