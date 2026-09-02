//! Provider polling. The daemon owns ONE polling task per process; it
//! drives the configured `TaskSource`s on an interval, upserts each
//! returned task into the store, and broadcasts `SessionUpserted`
//! events through the `ServerConfig::bus` so every connected client
//! sees the change.
//!
//! ## Why a `TaskSource` trait, not direct GhClient/LinearClient calls
//!
//! The polling loop's logic — interval, upsert, broadcast, error
//! reporting — is identical for every provider. Hard-coding GitHub and
//! Linear into the loop would make it hard to test (real HTTP calls
//! against real APIs) and hard to extend (adding Jira would touch the
//! loop). With `TaskSource`, the loop is provider-agnostic and tests
//! drop in a fixture source that returns whatever vector of tasks the
//! test wants.
//!
//! ## Read-state preservation on update
//!
//! When a task we've seen before comes back from a provider, we merge
//! its fresh fields onto the existing `Session` rather than replacing
//! it — so `seen_count`, `read_indices`, `snoozed_until`, and
//! `last_viewed_at` all survive the poll. That write path lives in
//! `polling::upsert`.

pub mod auto_merge;
mod autofix;
mod handlers;
mod mutate;
mod scheduler;
mod sources;
mod upsert;

pub use auto_merge::AutoMergeMemory;
pub use scheduler::{
    CURSOR_TTL, DEFAULT_ROUND_ROBIN_N, RoundRobinPick, RoundRobinState, pick_repos_for_tick,
    pick_repos_for_tick_budgeted, plan_round_robin_tick, plan_round_robin_tick_budgeted,
    will_run_global,
};

pub(crate) use handlers::resolve_gh_client_result;
pub use handlers::{
    ProviderHandle, apply_pr_details, handle_add_assignees, handle_clean_worktrees,
    handle_close_issue, handle_close_pr, handle_convert_to_draft, handle_delete_or_close,
    handle_delete_orphaned_worktree, handle_fetch_pr_details, handle_fetch_repo_labels,
    handle_fetch_requestable_reviewers, handle_inspect_workspace_diff, handle_inspect_worktrees,
    handle_mark_ready, handle_merge_pr, handle_request_reviewers, handle_scan_checkouts,
    handle_set_assignees, handle_set_labels, handle_sync_workspace, handle_update_branch,
    post_reply, prefetch_top_pr_details, remove_merged_workspace,
};
pub use mutate::{MutationOutcome, apply_and_commit, fetch_and_apply};
#[cfg(test)]
use sources::{GhFetchPlan, gh_fetch_plan, partition_targeted_requests, rank_targeted_requests};
pub use sources::{
    GhSource, LinearSource, ProviderAction, build_issue_search_qualifiers,
    build_pr_search_qualifiers, default_sources, filter_github_tasks,
    filter_github_tasks_with_watches, filter_linear_tasks, gh_client_reusable,
    github_scopes_from_filters, github_watch_repos_from_filters, label_spawn_actions,
    readmit_mentioned_tasks, sources_for,
};
use sources::{dispatch_action, sources_for_with_engagement};
pub use upsert::upsert;
pub(crate) use upsert::{
    CommitError, CommitOutcome, commit_upsert, commit_upsert_offloaded_reported,
    commit_upsert_reported, github_scopes_from_config, load_workspace, load_workspace_offloaded,
    report_commit_error,
};
use upsert::{
    TerminalCleanup, UpsertContext, commit_workspace_move, task_number, upsert_with_context,
};
#[cfg(test)]
use upsert::{
    closed_issue_transition, issue_reopened, merged_transition_pr_number, upsert_into_workspace_key,
};

use crate::ServerConfig;
use chrono::Utc;
use futures::FutureExt;
use lazybox_core::{Task, Workspace, WorkspaceKey};
use lazybox_ipc::{Event, ProviderErrorKind};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub const HOT_SET_MAX: usize = 3;
pub const HOT_POLL_INTERVAL: Duration = Duration::from_secs(15);
const OWN_PR_HOT_WINDOW: chrono::Duration = chrono::Duration::hours(24);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EngagementTier {
    Hot,
    #[default]
    Warm,
    Cold,
}

impl EngagementTier {
    fn label(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngagementSignals {
    pub live_agent: bool,
    pub focused: bool,
    pub own_open_pr: bool,
}

#[derive(Debug, Clone)]
pub struct GithubEngagementTarget {
    pub workspace_key: WorkspaceKey,
    pub target: lazybox_gh::NotificationTarget,
    pub node_id: Option<String>,
}

#[derive(Debug, Clone)]
struct EngagementEntry {
    tier: EngagementTier,
    signals: EngagementSignals,
}

#[derive(Debug, Clone, Default)]
pub struct EngagementSnapshot {
    entries: std::collections::HashMap<String, EngagementEntry>,
    hot_targets: Vec<GithubEngagementTarget>,
    cold_targets: std::collections::BTreeSet<lazybox_gh::NotificationTarget>,
    cold_only_repos: std::collections::HashSet<String>,
    active_repos: std::collections::HashSet<String>,
    sessioned_repos: std::collections::HashSet<String>,
}

impl EngagementSnapshot {
    pub fn tier_for(&self, key: &WorkspaceKey) -> EngagementTier {
        self.entries
            .get(key.as_str())
            .map(|entry| entry.tier)
            .unwrap_or_default()
    }

    pub fn signals_for(&self, key: &WorkspaceKey) -> EngagementSignals {
        self.entries
            .get(key.as_str())
            .map(|entry| entry.signals)
            .unwrap_or_default()
    }

    pub fn hot_targets(&self) -> &[GithubEngagementTarget] {
        &self.hot_targets
    }

    pub fn cold_targets(&self) -> &std::collections::BTreeSet<lazybox_gh::NotificationTarget> {
        &self.cold_targets
    }

    pub fn cold_only_repos(&self) -> &std::collections::HashSet<String> {
        &self.cold_only_repos
    }

    pub fn active_repos(&self) -> &std::collections::HashSet<String> {
        &self.active_repos
    }

    /// Repos backing a workspace with a persisted session (any kind,
    /// live or idle). These are Tier 0 — force-included in every
    /// `repo:`-scoped fetch, uncapped, so a repo you're actively
    /// working in never goes stale.
    pub fn sessioned_repos(&self) -> &std::collections::HashSet<String> {
        &self.sessioned_repos
    }

    pub fn hot_count(&self) -> usize {
        self.hot_targets.len()
    }
}

#[derive(Debug, Default)]
pub struct PollEngagement {
    focused_workspace: Option<String>,
    snapshot: EngagementSnapshot,
}

impl PollEngagement {
    pub fn focused_workspace(&self) -> Option<&str> {
        self.focused_workspace.as_deref()
    }

    pub fn snapshot(&self) -> EngagementSnapshot {
        self.snapshot.clone()
    }

    pub fn tier_for(&self, key: &WorkspaceKey) -> EngagementTier {
        self.snapshot.tier_for(key)
    }

    fn replace_snapshot(&mut self, snapshot: EngagementSnapshot) {
        self.snapshot = snapshot;
    }

    fn set_focused_workspace(&mut self, workspace_key: Option<String>) -> bool {
        if self.focused_workspace == workspace_key {
            return false;
        }
        self.focused_workspace = workspace_key;
        true
    }
}

#[derive(Debug, Clone)]
struct EngagementCandidate {
    workspace_key: WorkspaceKey,
    target: lazybox_gh::NotificationTarget,
    node_id: Option<String>,
    repo: String,
    updated_at: chrono::DateTime<Utc>,
    cold: bool,
    live_agent: bool,
    own_open_pr: bool,
    /// Workspace has a persisted session (any [`lazybox_core::SessionKind`],
    /// live process or not). This is the Tier 0 "I'm actively working
    /// in this repo" signal — stronger than `live_agent`, which only
    /// counts a live Agent PTY and misses shells and post-restart idle
    /// sessions.
    sessioned: bool,
}

fn select_engagement_snapshot(
    candidates: Vec<EngagementCandidate>,
    focused_workspace: Option<&str>,
    now: chrono::DateTime<Utc>,
) -> EngagementSnapshot {
    let mut eligible: Vec<&EngagementCandidate> = candidates
        .iter()
        .filter(|candidate| {
            candidate.sessioned
                || candidate.live_agent
                || focused_workspace == Some(candidate.workspace_key.as_str())
                || (!candidate.cold
                    && candidate.own_open_pr
                    && now.signed_duration_since(candidate.updated_at) <= OWN_PR_HOT_WINDOW)
        })
        .collect();
    eligible.sort_by(|left, right| {
        let left_focused = focused_workspace == Some(left.workspace_key.as_str());
        let right_focused = focused_workspace == Some(right.workspace_key.as_str());
        right_focused
            .cmp(&left_focused)
            .then_with(|| right.sessioned.cmp(&left.sessioned))
            .then_with(|| right.live_agent.cmp(&left.live_agent))
            .then_with(|| right.own_open_pr.cmp(&left.own_open_pr))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| {
                left.workspace_key
                    .as_str()
                    .cmp(right.workspace_key.as_str())
            })
    });

    // Sessioned workspaces (Tier 0) are ALWAYS hot — uncapped, bounded
    // only by how many worktrees the user has open. `HOT_SET_MAX` caps
    // only the remaining engagement signals (focus / live agent / recent
    // own PR) so those can't drown out a repo the user is working in.
    let mut hot_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut capped_used = 0usize;
    for candidate in eligible {
        if candidate.sessioned {
            hot_keys.insert(candidate.workspace_key.as_str().to_string());
        } else if capped_used < HOT_SET_MAX {
            hot_keys.insert(candidate.workspace_key.as_str().to_string());
            capped_used += 1;
        }
    }

    let mut entries = std::collections::HashMap::new();
    let mut hot_targets = Vec::new();
    let mut cold_targets = std::collections::BTreeSet::new();
    let mut repos = std::collections::HashSet::new();
    let mut non_cold_repos = std::collections::HashSet::new();
    let mut sessioned_repos = std::collections::HashSet::new();
    for candidate in candidates {
        if candidate.sessioned {
            sessioned_repos.insert(candidate.repo.clone());
        }
        let focused = focused_workspace == Some(candidate.workspace_key.as_str());
        let tier = if hot_keys.contains(candidate.workspace_key.as_str()) {
            EngagementTier::Hot
        } else if candidate.cold {
            EngagementTier::Cold
        } else {
            EngagementTier::Warm
        };
        repos.insert(candidate.repo.clone());
        if tier != EngagementTier::Cold {
            non_cold_repos.insert(candidate.repo.clone());
        }
        if tier == EngagementTier::Hot {
            hot_targets.push(GithubEngagementTarget {
                workspace_key: candidate.workspace_key.clone(),
                target: candidate.target.clone(),
                node_id: candidate.node_id.clone(),
            });
        } else if tier == EngagementTier::Cold {
            cold_targets.insert(candidate.target.clone());
        }
        entries.insert(
            candidate.workspace_key.as_str().to_string(),
            EngagementEntry {
                tier,
                signals: EngagementSignals {
                    live_agent: candidate.live_agent,
                    focused,
                    own_open_pr: candidate.own_open_pr,
                },
            },
        );
    }
    hot_targets.sort_by(|left, right| {
        let left_entry = entries
            .get(left.workspace_key.as_str())
            .map(|entry| entry.signals)
            .unwrap_or_default();
        let right_entry = entries
            .get(right.workspace_key.as_str())
            .map(|entry| entry.signals)
            .unwrap_or_default();
        right_entry
            .focused
            .cmp(&left_entry.focused)
            .then_with(|| right_entry.live_agent.cmp(&left_entry.live_agent))
            .then_with(|| {
                left.workspace_key
                    .as_str()
                    .cmp(right.workspace_key.as_str())
            })
    });
    let cold_only_repos = repos
        .difference(&non_cold_repos)
        .cloned()
        .collect::<std::collections::HashSet<_>>();

    EngagementSnapshot {
        entries,
        hot_targets,
        cold_targets,
        cold_only_repos,
        active_repos: non_cold_repos,
        sessioned_repos,
    }
}

pub async fn refresh_github_engagement(config: &ServerConfig) -> EngagementSnapshot {
    let live_agent_workspaces: std::collections::HashSet<String> = {
        let entries = config.terminal.entries.lock().await;
        entries
            .values()
            .filter(|entry| !entry.finishing)
            .filter_map(|entry| entry.meta.as_ref())
            .filter(|(_, kind)| matches!(kind, lazybox_ipc::TerminalKind::Agent(_)))
            .map(|(session_key, _)| session_key.as_str().to_string())
            .collect()
    };

    let store = config.store.clone();
    let records = match tokio::task::spawn_blocking(move || store.list_workspaces()).await {
        Ok(Ok(records)) => records,
        Ok(Err(error)) => {
            tracing::warn!("engagement: list_workspaces failed: {error}");
            return config.poll.engagement_snapshot();
        }
        Err(error) => {
            tracing::warn!("engagement: workspace scan task failed: {error}");
            return config.poll.engagement_snapshot();
        }
    };

    let now = Utc::now();
    // Source-attention ladder (#scale): the ONE place user curation
    // reaches the scheduler. The client persists `ui.source_attention`;
    // `Config::load()` is mtime-cached, so this observes the change on
    // the tick after the write. A Muted (or source-snoozed) repo's
    // workspaces classify Cold — clearing every hot/forced signal —
    // which parks the repo via `suspended_cold` and drops its
    // notification targets; a Digest repo merely stops forcing itself
    // into every sweep and never promotes to the hot cadence.
    let user_cfg = lazybox_config::Config::load().unwrap_or_default();
    let source_level = |label: &str| {
        lazybox_config::effective_source_attention(
            &user_cfg.ui.source_attention,
            label,
            Some(&lazybox_config::space_of(label, &user_cfg.ui.spaces)),
        )
        .effective_level(now)
    };
    let mut candidates = Vec::new();
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = Workspace::decode_persisted(&json) else {
            continue;
        };
        let Some(task) = workspace.primary_task() else {
            continue;
        };
        if task.id.source != lazybox_gh::SOURCE {
            continue;
        }
        let Some((owner, repo_name, number)) = handlers::github_target(task) else {
            continue;
        };
        let repo = format!("{owner}/{repo_name}");
        let level = source_level(&repo);
        let muted = level == lazybox_config::SourceAttentionLevel::Muted;
        let digest = level == lazybox_config::SourceAttentionLevel::Digest;
        let active = !matches!(
            task.state,
            lazybox_core::TaskState::Closed | lazybox_core::TaskState::Merged
        );
        let own_open_pr =
            task.is_pr() && active && task.role == lazybox_core::TaskRole::Author && !muted;
        let cold = workspace.is_snoozed(now) || !active || muted;
        candidates.push(EngagementCandidate {
            workspace_key: workspace.key.clone(),
            target: lazybox_gh::NotificationTarget {
                owner,
                repo: repo_name,
                number,
                kind: if task.is_pr() {
                    lazybox_gh::NotificationTargetKind::PullRequest
                } else {
                    lazybox_gh::NotificationTargetKind::Issue
                },
            },
            node_id: task.node_id.clone(),
            repo,
            updated_at: task.updated_at,
            cold,
            live_agent: !muted && live_agent_workspaces.contains(workspace.key.as_str()),
            own_open_pr,
            sessioned: !(muted || digest) && !workspace.sessions.is_empty(),
        });
    }

    let mut engagement = config.poll.engagement.write();
    let snapshot =
        select_engagement_snapshot(candidates, engagement.focused_workspace.as_deref(), now);
    tracing::info!(
        hot = snapshot.hot_count(),
        cold_repos = snapshot.cold_only_repos.len(),
        focused = engagement.focused_workspace.as_deref().unwrap_or(""),
        "engagement tiers refreshed"
    );
    engagement.replace_snapshot(snapshot.clone());
    snapshot
}

#[cfg(test)]
mod engagement_tier_tests {
    use super::*;
    use lazybox_core::{
        CheckRun, CiStatus, Mergeable, ReviewStatus, TaskId, TaskKind, TaskRole, TaskState,
    };

    fn target(number: u64) -> lazybox_gh::NotificationTarget {
        lazybox_gh::NotificationTarget {
            owner: "o".into(),
            repo: "r".into(),
            number,
            kind: lazybox_gh::NotificationTargetKind::PullRequest,
        }
    }

    fn candidate(number: u64) -> EngagementCandidate {
        EngagementCandidate {
            workspace_key: WorkspaceKey::new(format!("github:o/r#{number}")),
            target: target(number),
            node_id: Some(format!("PR_{number}")),
            repo: "o/r".into(),
            updated_at: Utc::now(),
            cold: false,
            live_agent: false,
            own_open_pr: true,
            sessioned: false,
        }
    }

    fn task(number: u64, role: TaskRole) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("o/r#{number}"),
            },
            title: format!("PR {number}"),
            body: None,
            state: TaskState::Open,
            role,
            ci: CiStatus::Success,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/o/r/pull/{number}"),
            repo: Some("o/r".into()),
            branch: Some("feature".into()),
            base_branch: Some("main".into()),
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: Some(format!("PR_{number}")),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            kind: Some(TaskKind::Pr),
            priority: None,
            state_label: None,
        }
    }

    #[test]
    fn hot_selection_is_bounded_and_keeps_focus_first() {
        let candidates: Vec<_> = (1..=8).map(candidate).collect();
        let focused = WorkspaceKey::new("github:o/r#8");
        let snapshot = select_engagement_snapshot(candidates, Some(focused.as_str()), Utc::now());
        assert_eq!(snapshot.hot_count(), HOT_SET_MAX);
        assert_eq!(snapshot.tier_for(&focused), EngagementTier::Hot);
        assert!(
            snapshot
                .hot_targets()
                .first()
                .is_some_and(|target| target.workspace_key == focused)
        );
    }

    #[test]
    fn cold_only_repos_leave_the_round_robin_tier() {
        let mut cold = candidate(1);
        cold.cold = true;
        cold.own_open_pr = false;
        cold.repo = "o/cold".into();
        cold.target.repo = "cold".into();
        let mut warm = candidate(2);
        warm.own_open_pr = false;
        warm.repo = "o/warm".into();
        warm.target.repo = "warm".into();

        let snapshot = select_engagement_snapshot(vec![cold, warm], None, Utc::now());
        assert_eq!(
            snapshot.tier_for(&WorkspaceKey::new("github:o/r#1")),
            EngagementTier::Cold
        );
        assert!(snapshot.cold_only_repos().contains("o/cold"));
        assert!(!snapshot.cold_only_repos().contains("o/warm"));
    }

    /// Every sessioned workspace stays hot even when there are more of
    /// them than `HOT_SET_MAX` — the Tier 0 cap-bypass. Pre-fix the
    /// `.take(HOT_SET_MAX)` dropped session-bearing repos past the top 3.
    #[test]
    fn all_sessioned_repos_stay_hot_beyond_the_cap() {
        let candidates: Vec<_> = (1..=6)
            .map(|n| {
                let mut c = candidate(n);
                c.own_open_pr = false;
                c.sessioned = true;
                c.repo = format!("o/r{n}");
                c
            })
            .collect();
        let keys: Vec<_> = candidates.iter().map(|c| c.workspace_key.clone()).collect();

        let snapshot = select_engagement_snapshot(candidates, None, Utc::now());
        assert!(
            snapshot.hot_count() >= 6,
            "all 6 sessioned repos must be hot"
        );
        for key in &keys {
            assert_eq!(snapshot.tier_for(key), EngagementTier::Hot);
        }
        assert_eq!(snapshot.sessioned_repos().len(), 6);
        assert!(snapshot.cold_only_repos().is_empty());
    }

    /// A persisted-but-idle session (no live agent PTY, no own PR) is
    /// engaged, not cold — the signal is `workspace.sessions`, not a
    /// live Agent terminal. A shell session behaves identically.
    #[test]
    fn persisted_session_without_live_agent_is_engaged() {
        let mut idle = candidate(1);
        idle.cold = true;
        idle.own_open_pr = false;
        idle.live_agent = false;
        idle.sessioned = true;
        let key = idle.workspace_key.clone();

        let snapshot = select_engagement_snapshot(vec![idle], None, Utc::now());
        assert_eq!(snapshot.tier_for(&key), EngagementTier::Hot);
        assert!(snapshot.sessioned_repos().contains("o/r"));
        assert!(snapshot.cold_only_repos().is_empty());
    }

    /// Non-sessioned engagement signals (focus / live agent / recent own
    /// PR) still share the `HOT_SET_MAX` cap so they can't drown out the
    /// round-robin, while sessioned repos ride above it.
    #[test]
    fn non_sessioned_hot_stays_capped_alongside_uncapped_sessioned() {
        let mut sessioned: Vec<_> = (1..=4)
            .map(|n| {
                let mut c = candidate(n);
                c.own_open_pr = false;
                c.sessioned = true;
                c.repo = format!("o/s{n}");
                c
            })
            .collect();
        // Five recent own-PR (non-sessioned) candidates competing for the
        // 3 capped slots.
        let recent: Vec<_> = (10..=14)
            .map(|n| {
                let mut c = candidate(n);
                c.repo = format!("o/w{n}");
                c
            })
            .collect();
        sessioned.extend(recent);

        let snapshot = select_engagement_snapshot(sessioned, None, Utc::now());
        // 4 sessioned (uncapped) + 3 capped own-PR = 7 hot.
        assert_eq!(snapshot.hot_count(), 7);
    }

    #[test]
    fn live_agent_overrides_inactive_classification() {
        let mut live = candidate(1);
        live.cold = true;
        live.own_open_pr = false;
        live.live_agent = true;
        let key = live.workspace_key.clone();

        let snapshot = select_engagement_snapshot(vec![live], None, Utc::now());
        assert_eq!(snapshot.tier_for(&key), EngagementTier::Hot);
        assert!(snapshot.signals_for(&key).live_agent);
        assert!(snapshot.cold_only_repos().is_empty());
    }

    #[test]
    fn old_own_pr_stays_warm() {
        let mut own = candidate(1);
        own.updated_at = Utc::now() - OWN_PR_HOT_WINDOW - chrono::Duration::seconds(1);
        let key = own.workspace_key.clone();
        let snapshot = select_engagement_snapshot(vec![own], None, Utc::now());
        assert_eq!(snapshot.tier_for(&key), EngagementTier::Warm);
    }

    #[test]
    fn snoozed_own_pr_stays_cold_without_focus_or_live_agent() {
        let mut own = candidate(1);
        own.cold = true;
        let key = own.workspace_key.clone();
        let snapshot = select_engagement_snapshot(vec![own], None, Utc::now());
        assert_eq!(snapshot.tier_for(&key), EngagementTier::Cold);
    }

    #[test]
    fn targeted_requests_dedup_and_rank_hot_first() {
        let hot = target(2);
        let notification: lazybox_gh::NotificationEntry =
            serde_json::from_value(serde_json::json!({
                "reason": "review_requested",
                "subject": {
                    "title": "PR",
                    "url": "https://api.github.com/repos/o/r/pulls/1",
                    "type": "PullRequest"
                },
                "repository": {"full_name": "o/r"}
            }))
            .expect("notification fixture");
        let hot_notification: lazybox_gh::NotificationEntry =
            serde_json::from_value(serde_json::json!({
                "subject": {
                    "url": "https://api.github.com/repos/o/r/pulls/2",
                    "type": "PullRequest"
                }
            }))
            .expect("hot notification fixture");
        let ranked = rank_targeted_requests(
            &[hot],
            &[notification.clone(), hot_notification.clone()],
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].target.number, 2);
        assert!(ranked[0].flags.hot);
        assert!(ranked[0].flags.notification);
        assert_eq!(ranked[1].target.number, 1);
        assert!(!ranked[1].flags.hot);

        let cold_targets = [target(1)].into_iter().collect();
        let ranked = rank_targeted_requests(
            &[target(2)],
            &[notification, hot_notification],
            &cold_targets,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].target.number, 2);
        assert!(ranked[0].flags.hot);
        assert!(ranked[0].flags.notification);
    }

    #[test]
    fn hot_targets_skip_the_batch_once_the_server_rejects_it() {
        let hot_node_ids: std::collections::BTreeMap<_, _> =
            [(target(2), "PR_two".to_string())].into_iter().collect();
        let requests = vec![
            sources::TargetedRequest {
                target: target(2),
                flags: sources::TargetedFlags {
                    hot: true,
                    notification: false,
                },
            },
            sources::TargetedRequest {
                target: target(1),
                flags: sources::TargetedFlags {
                    hot: false,
                    notification: true,
                },
            },
        ];

        // Healthy server: the hot target with a cached node id rides the
        // batched `nodes(ids:)` query, the notification target goes alone.
        let (batched, individual) =
            partition_targeted_requests(requests.clone(), &hot_node_ids, true);
        assert_eq!(batched.len(), 1);
        assert_eq!(batched[0].0.target.number, 2);
        assert_eq!(batched[0].1, "PR_two");
        assert_eq!(individual.len(), 1);
        assert_eq!(individual[0].target.number, 1);

        // A server that rejects the batch (GHES 3.18): nothing is batched,
        // hot targets are fetched one at a time and keep their rank.
        let (batched, individual) = partition_targeted_requests(requests, &hot_node_ids, false);
        assert!(batched.is_empty());
        assert_eq!(
            individual
                .iter()
                .map(|request| request.target.number)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
    }

    #[test]
    fn hot_only_ticks_skip_the_notifications_heartbeat() {
        assert_eq!(gh_fetch_plan(false, false), GhFetchPlan::Hot);
        assert_eq!(gh_fetch_plan(false, true), GhFetchPlan::Warm);
        assert_eq!(gh_fetch_plan(true, false), GhFetchPlan::Full);
    }

    #[tokio::test]
    async fn live_agent_membership_flows_into_the_engagement_snapshot() {
        let config = ServerConfig::in_memory();
        let task = task(42, TaskRole::Reviewer);
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        upsert(&config, task).await;
        config
            .terminal
            .insert_meta(
                lazybox_ipc::TerminalId(1),
                lazybox_core::SessionKey::from(key.as_str()),
                lazybox_ipc::TerminalKind::Agent("claude".into()),
            )
            .await;

        let snapshot = refresh_github_engagement(&config).await;
        assert_eq!(snapshot.tier_for(&key), EngagementTier::Hot);
        assert!(snapshot.signals_for(&key).live_agent);
    }

    /// End-to-end: a workspace with a persisted (non-live) session — no
    /// Agent terminal in `terminal_meta` — is classified engaged and its
    /// repo lands in `sessioned_repos`, even when the underlying task is
    /// a reviewer PR with no live agent. Pre-fix this workspace was cold
    /// unless it happened to win one of the three hot slots.
    #[tokio::test]
    async fn persisted_session_repo_flows_into_sessioned_repos() {
        use lazybox_core::{SessionKind, WorkspaceSession};

        let config = ServerConfig::in_memory();
        let task = task(51, TaskRole::Reviewer);
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        let mut workspace = Workspace::from_task(task, Utc::now());
        workspace.add_session(WorkspaceSession::new(
            workspace.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        ));
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: workspace.key.as_str().to_string(),
                created_at: workspace.created_at,
                workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
            })
            .unwrap();

        // No terminal_meta entry — this is NOT a live agent.
        assert!(config.terminal.metadata_map().await.is_empty());

        let snapshot = refresh_github_engagement(&config).await;
        assert_eq!(snapshot.tier_for(&key), EngagementTier::Hot);
        assert!(!snapshot.signals_for(&key).live_agent);
        assert!(snapshot.sessioned_repos().contains("o/r"));
    }

    #[tokio::test]
    async fn fresher_upserts_feed_hot_and_cold_latency_histograms() {
        let config = ServerConfig::in_memory();
        let mut hot = task(1, TaskRole::Reviewer);
        hot.updated_at = Utc::now() - chrono::Duration::seconds(10);
        let hot_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&hot));
        upsert(&config, hot.clone()).await;
        set_focused_workspace(&config, &hot_key).await;
        refresh_github_engagement(&config).await;
        hot.updated_at = Utc::now() - chrono::Duration::seconds(2);
        upsert(&config, hot.clone()).await;
        hot.ci = CiStatus::Failure;
        hot.checks = vec![CheckRun {
            name: "build".into(),
            status: CiStatus::Failure,
            url: None,
        }];
        upsert(&config, hot.clone()).await;
        let mut lean_hot = hot;
        lean_hot.checks.clear();
        upsert(&config, lean_hot).await;

        let mut cold = task(2, TaskRole::Reviewer);
        cold.updated_at = Utc::now() - chrono::Duration::minutes(10);
        let cold_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&cold));
        upsert(&config, cold.clone()).await;
        crate::workspace::set_snooze(
            &config,
            &cold_key,
            Some(Utc::now() + chrono::Duration::hours(1)),
            None,
        )
        .await;
        set_focused_workspace(&config, &WorkspaceKey::new("local:other")).await;
        refresh_github_engagement(&config).await;
        cold.updated_at = Utc::now() - chrono::Duration::minutes(3);
        upsert(&config, cold).await;

        let metrics = config.event_metrics.snapshot();
        assert_eq!(metrics.hot_sync_samples, 2);
        assert_eq!(metrics.cold_sync_samples, 1);
        assert!(metrics.hot_sync_p95_ms.is_some_and(|age| age < 10_000));
        assert!(metrics.cold_sync_p95_ms.is_some_and(|age| age >= 170_000));
    }
}

/// Did the last `fetch` return ALL in-scope tasks, or a subset?
///
/// `Full` is the historical behavior — the source ran an exhaustive
/// `involves:USER` query and the tick can trust "anything not in this
/// list is out of scope, drop it" semantics (rescope). `Incremental`
/// is the new mode introduced for the notifications-driven fast path:
/// the source fetched only the tasks GitHub flagged as recently
/// changed, and the tick MUST NOT rescope against this list — doing so
/// would drop every workspace not touched in the last 30 seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    Full,
    Incremental,
    Hot,
}

impl FetchMode {
    /// Greppable label for sync-latency tracing — which delivery path
    /// produced this tick's tasks. `Full` is the exhaustive
    /// `involves:USER` sweep; `Incremental` is the notifications-driven
    /// fast path (the closest thing lazybox has to an event push). When an
    /// update "took a long time to appear," this is the first thing to
    /// check in `/tmp/lazybox.log`: did it arrive via a slow full sweep or
    /// a fast notifications poll?
    pub fn label(self) -> &'static str {
        match self {
            FetchMode::Full => "full-sweep",
            FetchMode::Incremental => "notifications",
            FetchMode::Hot => "hot-targets",
        }
    }
}

/// Anything that can produce a flat list of `Task`s. Implementations
/// should be cheap to construct and cheap to call repeatedly: they're
/// invoked on every poll tick.
///
/// Errors are typed (`lazybox_core::ProviderError`) so polling can
/// distinguish retryable hiccups from auth failures from permanent
/// bugs and react accordingly. See `lazybox_core::provider`.
pub trait TaskSource: Send + Sync + 'static {
    /// Short stable name for telemetry / `Event::ProviderError`
    /// (e.g. "github", "linear").
    fn name(&self) -> &str;

    /// Fetch the current set of tasks. Returns a classified error so
    /// the polling loop can pick the right log level + decide whether
    /// to retry.
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>;

    /// What this source authoritatively covered in the most recent
    /// `fetch`. Drives the `rescope` deletion guard: only workspaces
    /// owned by a source whose scope this tick is authoritative for
    /// are candidates for removal.
    ///
    /// Default is [`PolledScope::Repos`]`(Vec::new())` — "I covered no
    /// repos authoritatively this tick", so `rescope` preserves every
    /// stored workspace owned by this source. The destructive choice
    /// ([`PolledScope::Exhaustive`]) requires an explicit override so
    /// a forgetful source author can't silently re-introduce the
    /// issue #34 bug: a new partial-coverage source (Jira polling one
    /// project at a time, GitHub round-robining repos, …) that omits
    /// the override gets the safe answer.
    ///
    /// `GhSource` and `LinearSource` both override below.
    fn polled_scope(&self) -> PolledScope {
        PolledScope::Repos(Vec::new())
    }

    /// Drain side-effect actions accumulated during the most recent
    /// `fetch`. The polling tick calls this after each fetch and
    /// dispatches the resulting [`ProviderAction`]s with full
    /// `&ServerConfig` access — used today by `GhSource` to surface
    /// auto-spawn requests triggered by `@lazybox` mentions. Default
    /// impl returns nothing so sources without side effects don't
    /// have to think about it.
    fn drain_actions(&self) -> Vec<ProviderAction> {
        Vec::new()
    }

    /// Mode of the most-recent successful `fetch`. Sources that ALWAYS
    /// return the complete in-scope set (Linear, every test fixture)
    /// can leave this as the default `Full`. Sources that may return a
    /// subset (notifications-driven `GhSource`) override to record the
    /// mode of the call they just made.
    ///
    /// Read AFTER `fetch` resolves. The tick driver consults it to
    /// decide whether rescope can run for this tick — see
    /// `TickOutcome::all_full`.
    fn last_fetch_kind(&self) -> FetchMode {
        FetchMode::Full
    }

    /// Retry delay attached to a partial successful fetch. Read after
    /// `fetch` resolves so the scheduler can retain successful tasks
    /// without immediately retrying a rate-limited side.
    fn retry_after_secs(&self) -> Option<u64> {
        None
    }

    fn record_items_changed(&self, _count: usize) {}
}

/// What a [`TaskSource`] authoritatively covered in its most recent
/// `fetch`. The polling tick captures this per-source into
/// [`TickOutcome::source_scopes`] so `rescope` can tell the
/// difference between "this workspace fell out of upstream scope"
/// (delete) and "we just didn't query its repo this tick" (preserve).
///
/// Issue #34: pre-fix `GhSource`'s round-robin scheduler would poll
/// 3 of N repos per tick, but `rescope` treated every stored
/// workspace not in `polled` as out-of-scope. Result: every minute,
/// PRs from the other (N - 3) repos disappeared from the sidebar
/// until the next global sweep re-discovered them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolledScope {
    /// This source covered everything it owns this tick. Any stored
    /// workspace tagged with this source but not in `polled` is a
    /// genuine out-of-scope candidate.
    Exhaustive,
    /// This source only queried these specific repos. Workspaces
    /// tagged with this source whose `task.repo` is NOT in this list
    /// must be preserved — we have no information about them this
    /// tick.
    Repos(Vec<String>),
}

/// Decide what coverage the GitHub source reports to `rescope` for a
/// tick, given the round-robin scheduling decision and whether the
/// sweep was a PARTIAL success (one of PRs/Issues errored).
///
/// `partial` is the override that closes the data-loss hole: when the
/// PR side fails, the client returns issues-only `Ok(..)` to keep the
/// inbox alive. If we still reported `Exhaustive`, `rescope` would
/// read "PRs not in the polled set" as "PRs fell out of scope" and
/// DELETE every PR — a PR vanishing because one poll hiccupped, not
/// because it merged/closed. On a partial sweep we report empty
/// coverage (`Repos([])`, matching no stored workspace) so the whole
/// github inbox is preserved this tick; the next clean sweep
/// reconciles legitimately-gone rows.
///
/// `windowed` is the same kind of override for the incremental
/// `updated:>=` sweep (issue #14): a windowed global sweep only
/// returned PRs that *changed*, so the unchanged majority is absent
/// from the polled set — exactly the shape `rescope` would misread as
/// "fell out of scope." Only the periodic unwindowed reconcile sweep
/// reports `Exhaustive` and is allowed to drive deletion.
pub fn gh_polled_scope(
    run_global: bool,
    repos: &[String],
    coverage: lazybox_core::FetchCoverage,
    windowed: bool,
) -> PolledScope {
    if coverage.is_partial() || windowed {
        return PolledScope::Repos(Vec::new());
    }
    if run_global {
        PolledScope::Exhaustive
    } else {
        PolledScope::Repos(repos.to_vec())
    }
}

/// Run one poll tick: every source is called once and its tasks are
/// upserted. Errors from one source don't stop the others.
pub async fn tick(config: &ServerConfig, sources: &[Box<dyn TaskSource>]) -> TickOutcome {
    let mut state = TickState::default();
    tick_with_state(config, sources, &mut state).await
}

/// Per-loop state that the long-lived `spawn` task threads through
/// `tick` so we can debounce: re-broadcasting the same provider
/// error every 60s spams the TUI with identical hint-bar churn. We
/// only re-broadcast when the error message (or success/failure
/// classification) actually changes for a given source.
#[derive(Default)]
pub struct TickState {
    last_error: std::collections::HashMap<String, String>,
    /// Per-source backoff deadlines (#1218). A retry-after hint parks
    /// ONLY the source that produced it — `sources_for_with_engagement`
    /// skips building that source until the deadline — while every
    /// other source (Linear on a GitHub hint, the REST notifications
    /// heartbeat on a GraphQL hint) keeps its normal cadence. The
    /// driver-level sleep is clamped to a small multiple of the tick
    /// interval; this map carries the full deadline.
    pub(crate) source_backoff_until: std::collections::HashMap<String, std::time::Instant>,
    /// Workspace keys we've already broadcast `WorkspaceOutOfScope`
    /// for. Without this, every 60s tick would re-prompt the user
    /// about the same workspace (they said "no" once, that's
    /// final). Re-entered into the polled set on the next successful
    /// poll that surfaces the workspace again — i.e. once the user
    /// re-adds the filter / scope, we forget the dismissal and the
    /// workspace can produce a fresh prompt later if it falls out
    /// of scope again.
    prompted_out_of_scope: std::collections::HashSet<String>,
    /// PR node-ids whose review-thread details we've already prefetched
    /// this daemon session. Prevents the post-tick prefetch path from
    /// re-firing `fetch_pr_details` on every poll cycle for the same
    /// PR — once is enough; the TUI's lazy-fetch refreshes on focus
    /// for users who want explicit re-pull.
    pub(crate) prefetched_pr_details: std::collections::HashSet<String>,
    /// Per-repo round-robin bookkeeping: cursor, focused repo, and
    /// monotonic tick counter. Co-located in `RoundRobinState` so
    /// the scheduling-related logic stays inside `polling::scheduler`
    /// — adding TTL pruning or a dynamic-N knob doesn't touch this
    /// struct.
    pub round_robin: RoundRobinState,
    /// Consecutive polls in which each task (keyed by task id) has
    /// reported `Mergeable::Unknown`. Drives the fast-repoll cap:
    /// only the first [`UNKNOWN_MERGEABLE_MAX_FAST_PROBES`] Unknown
    /// sightings arm the 5s re-poll; after that the task waits out
    /// the normal cadence. Entries clear the moment the task reports
    /// a definitive value (or stops being polled Unknown).
    unknown_mergeable_probes: std::collections::HashMap<String, u32>,
    /// Consecutive retryable-failure count per source. A self-healing
    /// transient stays a quiet status until this reaches
    /// [`RETRYABLE_EXHAUSTION_ATTEMPTS`]; only then is a hintless
    /// transient (network, 5xx) genuinely stuck and escalated to an
    /// actionable error (#730). A throttle carries a backoff hint and
    /// never escalates however high the streak climbs (#772). Reset by
    /// [`Self::clear_error`] on any successful fetch or rate-limit wait.
    retryable_streak: std::collections::HashMap<String, u32>,
    /// Repos the authenticated user can access (owned / org-member /
    /// direct-collaborator), fetched via `GhClient::accessible_scopes`
    /// and unioned into the poll scope allowlist (only when
    /// `providers.github.include_accessible_repos` is set) so involved
    /// PRs/issues in any of them surface without a manual setup tick.
    /// Memoized rather than refetched each tick — it only changes with
    /// the user's memberships. Populated solely from a *complete* fetch
    /// (a failed fetch leaves it `None` to retry next tick, never caching
    /// a truncated allowlist), and reset when the GitHub client is
    /// rebuilt so a token rotation to another account refetches.
    pub(crate) implicit_gh_scopes: Option<Vec<String>>,
    /// Linear's own poll cadence, decoupled from the shared tick loop.
    /// Persists the next-due clock + idle streak across ticks so
    /// `sources_for_with_engagement` only builds a `LinearSource` when the
    /// Linear cadence is actually due — Linear tickets change far less
    /// often than GitHub CI/PR state (#1032).
    pub(crate) linear_schedule: sources::LinearSchedule,
    /// Consecutive ticks on which a GitHub full sweep was DUE but the rate
    /// governor refused to admit it (needs more GraphQL points than the
    /// per-tick allowance covers). Past ~25 watched repos this can hold
    /// forever, silently stalling new-issue/PR reconcile discovery (#1391).
    /// Once the streak crosses [`sources::DISCOVERY_BEHIND_TICKS`] the
    /// daemon raises one user-visible "discovery behind" advisory; a manual
    /// refresh or a tick that finally admits the sweep resets it.
    pub(crate) full_sweep_deferral_streak: u32,
    /// Whether the "discovery behind" advisory has already been broadcast
    /// for the current deferral episode, so the notice fires once when the
    /// stall sets in rather than every tick. Cleared when a sweep is
    /// admitted (or isn't due), re-arming the notice for a later re-stall.
    pub(crate) discovery_behind_notified: bool,
}

impl TickState {
    /// Broadcast a `ProviderError` for `source_key` unless it merely
    /// repeats the failure already surfaced for that source this session.
    ///
    /// A retryable transient the daemon is auto-retrying is self-healing,
    /// so it must NOT shout as a red error — noise that buries the
    /// failures the user actually has to act on (#730). While retries are
    /// in flight it broadcasts as a quiet `retryable` status, coalesced
    /// onto one stable [`RETRYABLE_DEDUPE_KEY`] sentinel so a churning
    /// message (a ticking "retrying in Ns", a throttle alternating with a
    /// 502) fires once, not once per cycle (#727). Only once a *hintless*
    /// transient (network, 5xx — not a throttle) persists for
    /// [`RETRYABLE_EXHAUSTION_ATTEMPTS`] consecutive cycles — retries
    /// genuinely exhausted, sync actually stuck — does it escalate to an
    /// `exhausted` error the user can act on, carrying the classified
    /// cause (#772). A throttle never escalates: it is the daemon backing
    /// off a rate limit, self-heals on reset, and blaming the token or the
    /// connection for it is a dead-end the user can't act on. Auth/permanent errors
    /// are actionable immediately and debounce on their real message so a
    /// changed error still surfaces. A genuine recovery calls
    /// [`Self::clear_error`], re-arming the next broadcast.
    ///
    /// Every path that surfaces a poll failure (fetch error, client-init
    /// failure) MUST go through here so the two never drift onto different
    /// key schemes for the shared `last_error` slot — and so a transient
    /// alternating across those call sites counts toward one exhaustion
    /// streak.
    fn broadcast_error_debounced(
        &mut self,
        bus: &tokio::sync::broadcast::Sender<Event>,
        source_key: &str,
        error: &lazybox_core::ProviderError,
    ) {
        let (kind, message): (ProviderErrorKind, String) = if error.is_retryable() {
            let streak = self
                .retryable_streak
                .entry(source_key.to_string())
                .or_insert(0);
            *streak += 1;
            // A throttle (a rate limit carrying a backoff window) is the
            // daemon deliberately waiting out the provider's reset — the
            // token and the connection are both fine and there is nothing
            // the user can do but wait. It must never escalate to a red
            // actionable error, however long the backoff runs (#772/#782:
            // the exemplar throttle mis-surfaced as "check your connection
            // or token", forever; a governor self-throttle under
            // shared-token contention is the same story). A self-throttle
            // always carries a retry hint, so this gate already exempts it
            // — no separate `is_self_throttle` branch is needed; its honest
            // wording lives in `ProviderError::user_message`. Only a
            // hintless transient (network, 5xx) that keeps failing is
            // genuinely stuck, and its escalation carries the real cause
            // rather than a generic token blame.
            if error.retry_after_secs().is_none() && *streak >= RETRYABLE_EXHAUSTION_ATTEMPTS {
                (ProviderErrorKind::Exhausted, error.exhausted_message())
            } else {
                (ProviderErrorKind::Retryable, error.user_message())
            }
        } else {
            // A definitive (auth/permanent) failure breaks the retryable
            // streak — a later transient starts counting fresh.
            self.retryable_streak.remove(source_key);
            let kind = if error.is_auth() {
                ProviderErrorKind::Auth
            } else {
                ProviderErrorKind::Permanent
            };
            (kind, error.user_message())
        };
        // Retryable/exhausted collapse onto stable sentinels so a churning
        // message fires once; auth/permanent key on their real message so
        // a genuine change of condition still re-surfaces.
        let dedupe_key = match kind {
            ProviderErrorKind::Retryable => RETRYABLE_DEDUPE_KEY,
            ProviderErrorKind::Exhausted => EXHAUSTED_DEDUPE_KEY,
            ProviderErrorKind::Auth | ProviderErrorKind::Permanent => message.as_str(),
        };
        // A quiet retryable must not overwrite a standing exhausted error.
        // Once sync is genuinely stuck, a throttle or hiccup arriving
        // mid-streak would otherwise flicker the surface red→quiet→red
        // every cycle (#727). The exhausted error stays put until a real
        // recovery calls [`Self::clear_error`]; an auth/permanent change
        // of condition still surfaces (only Retryable is suppressed here).
        if kind == ProviderErrorKind::Retryable
            && self.last_error.get(source_key).map(String::as_str) == Some(EXHAUSTED_DEDUPE_KEY)
        {
            return;
        }
        if self.last_error.get(source_key).map(String::as_str) == Some(dedupe_key) {
            return;
        }
        self.last_error
            .insert(source_key.to_string(), dedupe_key.to_string());
        let _ = bus.send(Event::ProviderError {
            source: error.source().to_string(),
            message,
            detail: error.diagnostic(),
            kind: kind.as_str().to_string(),
        });
    }

    /// Forget the last surfaced failure for `source_key` so its next
    /// failure broadcasts even if it repeats an earlier message, and
    /// reset its retryable-exhaustion streak. Called when the source
    /// recovers — a successful fetch or a rate-limit wait replacing the
    /// error state.
    /// Park `source` until `secs` from now (#1218). Saturating max —
    /// a longer existing deadline is never shortened.
    pub(crate) fn park_source(&mut self, source: &str, secs: u64) {
        let until =
            std::time::Instant::now() + std::time::Duration::from_secs(secs.min(60 * 60 * 24));
        let entry = self
            .source_backoff_until
            .entry(source.to_string())
            .or_insert(until);
        if until > *entry {
            *entry = until;
        }
    }

    /// Whether `source` is parked on a retry-after deadline right now.
    /// Expired entries are dropped on read.
    pub(crate) fn source_parked(&mut self, source: &str) -> Option<std::time::Duration> {
        let now = std::time::Instant::now();
        match self.source_backoff_until.get(source) {
            Some(until) if *until > now => Some(*until - now),
            Some(_) => {
                self.source_backoff_until.remove(source);
                None
            }
            None => None,
        }
    }

    fn clear_error(&mut self, source_key: &str) {
        self.last_error.remove(source_key);
        self.retryable_streak.remove(source_key);
    }
}

/// How many consecutive `Mergeable::Unknown` sightings of one task may
/// arm the 5s fast re-poll before that task falls back to the normal
/// poll cadence. GitHub computes mergeability within a few seconds in
/// the happy case; a PR still Unknown after three fast probes isn't
/// going to resolve on the next 5s probe either, and the fast loop
/// burns rate budget.
const UNKNOWN_MERGEABLE_MAX_FAST_PROBES: u32 = 3;

/// Issue→PR merge-prompt dedupe memory, kept in its OWN lock —
/// deliberately NOT a field of [`TickState`].
///
/// The collapse path that reads/writes it (`merge_closing_issue_workspaces`)
/// runs deep inside `upsert`. When that memory lived in `TickState` and
/// the tick held `poll_state` across the whole upsert loop, the collapse
/// reaching back for the same non-reentrant `tokio::sync::Mutex`
/// self-deadlocked until the per-task upsert timeout fired — PRs closing
/// a live-session issue stalling ~15s every tick (issue #131). Two
/// independent guards now prevent that: this memory has its own lock
/// (#132), and `run_one_tick` no longer holds `poll_state` across the
/// tick at all (#133, see `checkout_poll_state`). Keep both — `upsert`
/// staying decoupled from `poll_state` is a useful invariant on its own.
/// Do not fold these fields back into `TickState`.
#[derive(Default)]
pub struct MergePromptMemory {
    /// Issue workspace keys we've already broadcast
    /// `WorkspaceMergePending` for, with the timestamp of the last
    /// emission. A `HashSet` would stay pinned until the matching
    /// `Command::ConfirmMerge` arrived — a user who Esc-dismissed the
    /// modal then never saw it again until daemon restart. Re-prompt
    /// after `MERGE_REPROMPT_AFTER` so dismissals self-heal. Entries
    /// are removed on explicit confirm/reject so accepted/rejected
    /// pairs don't re-fire.
    pub(crate) prompted: std::collections::HashMap<String, std::time::Instant>,
    /// Issue workspace keys for which the user replied "no" to the
    /// merge prompt. We don't re-prompt this session — the user can
    /// still merge explicitly with `Command::CollapseIntoPr`.
    pub(crate) rejected: std::collections::HashSet<String>,
}

/// Workspace-removal prompt memory — the level-trigger state behind
/// [`Event::MergedPrRemovable`]. The prompt used to be fire-once (one
/// broadcast on the open→terminal flip); a TUI that was lagged,
/// disconnected, or not yet started lost it forever and the merged
/// workspace sat unprompted (issue #292). Same self-heal contract and
/// same own-lock reasoning as [`MergePromptMemory`].
///
/// This is now purely the emit-cadence throttle. The user's "keep"
/// answer lives on the workspace itself as [`lazybox_core::CleanupPrompt::Declined`]
/// (issue #499), so it survives a restart instead of being a
/// per-process pin.
#[derive(Default)]
pub struct RemovalPromptMemory {
    /// Workspace keys we've broadcast `MergedPrRemovable` for, with
    /// the last emission time. The per-tick reprompt scan re-emits
    /// after [`REMOVAL_REPROMPT_AFTER`] while the workspace stays a
    /// removal candidate, so an Esc dismissal or a dropped broadcast
    /// heals on its own. Cleared wholesale on client (re)connect so a
    /// fresh subscriber is prompted on the next tick, not in 5 min.
    pub(crate) prompted: std::collections::HashMap<String, std::time::Instant>,
}

#[derive(Debug, Clone, Copy)]
struct GithubRateLimitWait {
    remaining: u32,
    limit: u32,
    reset_at: chrono::DateTime<Utc>,
}

impl GithubRateLimitWait {
    fn retry_after_secs(self, now: chrono::DateTime<Utc>) -> u64 {
        let duration = self
            .reset_at
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default();
        duration
            .as_secs()
            .saturating_add(u64::from(duration.subsec_nanos() > 0))
            .max(1)
    }

    fn event(self) -> Event {
        Event::GithubRateLimitWait {
            remaining: self.remaining,
            limit: self.limit,
            reset_at: self.reset_at,
        }
    }
}

fn github_rate_limit_wait(
    snapshot: &lazybox_gh::RateSnapshot,
    now: chrono::DateTime<Utc>,
) -> Option<GithubRateLimitWait> {
    if let Some(retry_at) = snapshot.retry_at
        && retry_at > now
    {
        let remote = snapshot.remote.as_ref();
        return Some(GithubRateLimitWait {
            remaining: remote.map_or(0, |limit| limit.remaining),
            limit: remote.map_or(0, |limit| limit.limit),
            reset_at: retry_at,
        });
    }
    let remote = snapshot.remote.as_ref()?;
    (remote.remaining <= lazybox_gh::rate_budget::LOW_THRESHOLD && remote.reset_at > now).then_some(
        GithubRateLimitWait {
            remaining: remote.remaining,
            limit: remote.limit,
            reset_at: remote.reset_at,
        },
    )
}

/// A governor self-throttle is lazybox *deliberately* pacing itself against
/// the GraphQL budget (local reserve protection or a spent background
/// allowance) — not a stuck or slow sync. It carries a retry hint but,
/// unlike a remote-budget exhaustion, the observed budget is still well
/// above [`LOW_THRESHOLD`](lazybox_gh::rate_budget::LOW_THRESHOLD), so
/// [`github_rate_limit_wait`] returns `None` and the failure would otherwise
/// fall through to a debounced retryable `ProviderError`.
///
/// That path leaves the footer lying: the per-tick `Governor:` `PollProgress`
/// lights the "syncing github… still working" spinner every wake, while the
/// `ProviderError` that would clear it is coalesced onto one sentinel and
/// suppressed after the first cycle (see
/// [`TickState::broadcast_error_debounced`]). The spinner then spins forever
/// with an ever-climbing elapsed time — the sync reads as hung when it is
/// simply waiting out the budget window.
///
/// Surfacing the self-throttle as an explicit [`Event::GithubRateLimitWait`]
/// — the same honest "GitHub rate-limited · ~Nm" countdown a remote limit
/// gets — clears the phantom spinner on the client and states plainly that
/// lazybox is waiting, not working. `remote` is the GraphQL primary-budget
/// view, so its `remaining`/`limit` give the real budget in the label; the
/// reset prefers the observed GraphQL window, falling back to the error's own
/// retry hint when the budget hasn't been learned yet.
fn github_self_throttle_wait(
    error: &lazybox_core::ProviderError,
    snapshot: &lazybox_gh::RateSnapshot,
    now: chrono::DateTime<Utc>,
) -> Option<GithubRateLimitWait> {
    if !error.is_self_throttle() {
        return None;
    }
    let remote = snapshot.remote.as_ref();
    // A self-throttle is lazybox's OWN pacing decision, and its honest
    // wait is the error's own retry hint — the tick interval, after
    // which the governor re-credits and re-plans. This used to prefer
    // the GraphQL WINDOW RESET (up to ~1h out): a 60s "come back next
    // tick" was promoted into the logged "backing off 3295s" that
    // blacked out all polling for ~55 minutes and let PR state go
    // stale enough to break `g m` (#1203/#1218). The window reset is
    // only the right wait when the REMOTE budget is genuinely at the
    // floor — a GitHub-imposed condition, not a self-imposed one.
    let remote_exhausted =
        remote.is_some_and(|limit| limit.remaining <= lazybox_gh::rate_budget::LOW_THRESHOLD);
    let hint = error
        .retry_after_secs()
        .map(|secs| now + chrono::Duration::seconds(secs.min(i64::MAX as u64) as i64))
        .filter(|reset| *reset > now);
    let window = remote
        .map(|limit| limit.reset_at)
        .filter(|reset| *reset > now);
    let reset_at = if remote_exhausted {
        window.or(hint)
    } else {
        hint.or(window)
    }?;
    Some(GithubRateLimitWait {
        remaining: remote.map_or(0, |limit| limit.remaining),
        limit: remote.map_or(0, |limit| limit.limit),
        reset_at,
    })
}

#[cfg(test)]
mod rate_limit_wait_tests {
    use super::*;

    fn snapshot(
        remaining: u32,
        limit: u32,
        reset_at: chrono::DateTime<Utc>,
    ) -> lazybox_gh::RateSnapshot {
        lazybox_gh::RateSnapshot {
            local_available: 30.0,
            local_capacity: 30,
            remote: Some(lazybox_gh::RemoteRateLimit {
                remaining,
                limit,
                reset_at,
                observed_at: std::time::Instant::now(),
            }),
            background_share: lazybox_gh::rate_budget::DEFAULT_BACKGROUND_SHARE,
            resources: Vec::new(),
            tick: lazybox_gh::rate_budget::AccountingSnapshot::default(),
            total: lazybox_gh::rate_budget::AccountingSnapshot::default(),
            request_p50_ms: None,
            request_p95_ms: None,
            request_p99_ms: None,
            circuit_reason: None,
            retry_at: None,
            operations: Vec::new(),
        }
    }

    #[test]
    fn remote_low_budget_becomes_an_explicit_wait_event() {
        let now = Utc::now();
        let wait =
            github_rate_limit_wait(&snapshot(98, 5000, now + chrono::Duration::minutes(7)), now);

        assert!(matches!(
            wait.map(GithubRateLimitWait::event),
            Some(Event::GithubRateLimitWait {
                remaining: 98,
                limit: 5000,
                ..
            })
        ));
    }

    #[test]
    fn healthy_or_reset_budget_is_not_a_wait() {
        let now = Utc::now();
        assert!(
            github_rate_limit_wait(
                &snapshot(101, 5000, now + chrono::Duration::minutes(7)),
                now,
            )
            .is_none()
        );
        assert!(
            github_rate_limit_wait(&snapshot(98, 5000, now - chrono::Duration::seconds(1)), now,)
                .is_none()
        );
    }

    #[test]
    fn wait_deadline_supplies_the_scheduler_backoff_without_an_error_hint() {
        let now = Utc::now();
        let wait = github_rate_limit_wait(
            &snapshot(98, 5000, now + chrono::Duration::seconds(414)),
            now,
        )
        .expect("low remote budget");

        assert_eq!(wait.retry_after_secs(now), 414);
    }

    /// A governor self-throttle carries a healthy remote budget (well above
    /// `LOW_THRESHOLD`), so `github_rate_limit_wait` alone stays quiet — yet
    /// it must still become an explicit wait so the client clears its
    /// "syncing… still working" spinner instead of spinning forever. The
    /// label carries the real GraphQL budget and the countdown prefers the
    /// observed reset window.
    #[test]
    fn self_throttle_becomes_a_wait_even_above_the_low_threshold() {
        let now = Utc::now();
        let snap = snapshot(2201, 5000, now + chrono::Duration::minutes(15));
        assert!(
            github_rate_limit_wait(&snap, now).is_none(),
            "a healthy remote budget is not a remote-limit wait on its own"
        );

        let err = lazybox_core::ProviderError::self_throttle(
            lazybox_gh::SOURCE,
            "GitHub graphql reserve protected (2201 remaining, 2250 reserved)",
            900,
        );
        let wait = github_self_throttle_wait(&err, &snap, now).expect("self-throttle → wait");
        assert!(matches!(
            wait.event(),
            Event::GithubRateLimitWait {
                remaining: 2201,
                limit: 5000,
                ..
            }
        ));
        assert_eq!(
            wait.reset_at,
            now + chrono::Duration::minutes(15),
            "hint (900s) and window (15m) agree here — see the next test \
             for the case where they diverge"
        );
    }

    /// #1218 regression: a self-throttle with a healthy remote budget
    /// must wait its OWN retry hint (the tick interval), never the
    /// GraphQL window reset. The old preference promoted a 60s "come
    /// back next tick" into the logged "backing off 3295s" — a
    /// ~55-minute self-imposed blackout of all polling.
    #[test]
    fn self_throttle_with_healthy_budget_waits_its_own_hint_not_the_window() {
        let now = Utc::now();
        // Window resets 55 minutes out; the governor's hint is 60s.
        let snap = snapshot(2201, 5000, now + chrono::Duration::seconds(3295));
        let err = lazybox_core::ProviderError::self_throttle(
            lazybox_gh::SOURCE,
            "GitHub graphql tick allowance exhausted",
            60,
        );
        let wait = github_self_throttle_wait(&err, &snap, now).expect("self-throttle → wait");
        assert_eq!(
            wait.retry_after_secs(now),
            60,
            "self-imposed pacing waits the tick interval, not the window reset"
        );
    }

    /// Only a genuinely exhausted REMOTE budget (at/below LOW_THRESHOLD)
    /// justifies sleeping to the window reset — that's GitHub-imposed.
    #[test]
    fn self_throttle_with_exhausted_budget_waits_for_the_window() {
        let now = Utc::now();
        let snap = snapshot(42, 5000, now + chrono::Duration::seconds(3295));
        let err = lazybox_core::ProviderError::self_throttle(
            lazybox_gh::SOURCE,
            "GitHub graphql reserve protected",
            60,
        );
        let wait = github_self_throttle_wait(&err, &snap, now).expect("self-throttle → wait");
        assert_eq!(
            wait.retry_after_secs(now),
            3295,
            "an exhausted remote budget is GitHub-imposed — the window is honest"
        );
    }

    /// #1218: a retry hint parks only its own source, expires on
    /// schedule, and a longer deadline is never shortened.
    #[test]
    fn park_source_isolates_and_expires() {
        let mut state = TickState::default();
        assert!(state.source_parked(lazybox_gh::SOURCE).is_none());
        state.park_source(lazybox_gh::SOURCE, 60);
        assert!(state.source_parked(lazybox_gh::SOURCE).is_some());
        assert!(
            state.source_parked("linear").is_none(),
            "a GitHub hint must never park Linear"
        );
        // A shorter follow-up hint must not shorten the deadline.
        state.park_source(lazybox_gh::SOURCE, 1);
        assert!(
            state
                .source_parked(lazybox_gh::SOURCE)
                .is_some_and(|d| d.as_secs() > 30),
            "a longer existing deadline survives a shorter hint"
        );
        // A zero-length park is immediately expired and dropped.
        state.park_source("linear", 0);
        assert!(state.source_parked("linear").is_none());
    }

    /// #1218: the driver-level sleep is clamped — one source's hint
    /// slows the loop briefly but can never stop the world for a full
    /// rate window.
    #[test]
    fn driver_backoff_is_clamped() {
        let d = next_tick_delay_with_hot(
            std::time::Duration::from_secs(60),
            Some(3295),
            false,
            std::time::Duration::from_secs(5),
            0,
        );
        assert_eq!(
            d,
            std::time::Duration::from_secs(120),
            "the loop sleeps at most the cap; the parked source carries the full deadline"
        );
    }

    /// A plain retryable transient (5xx, network) is NOT a self-throttle —
    /// it must stay on the debounced-error path, not masquerade as a
    /// deliberate rate-limit wait.
    #[test]
    fn a_plain_transient_is_not_a_self_throttle_wait() {
        let now = Utc::now();
        let snap = snapshot(2201, 5000, now + chrono::Duration::minutes(15));
        let err =
            lazybox_core::ProviderError::retryable_after(lazybox_gh::SOURCE, "502 bad gateway", 30);
        assert!(github_self_throttle_wait(&err, &snap, now).is_none());
    }

    /// Before the GraphQL budget has been learned (`remote: None`), a
    /// self-throttle still surfaces an honest countdown from the error's
    /// own retry hint rather than falling back to the phantom spinner.
    #[test]
    fn self_throttle_falls_back_to_the_retry_hint_before_budget_is_learned() {
        let now = Utc::now();
        let mut snap = snapshot(0, 0, now);
        snap.remote = None;
        let err = lazybox_core::ProviderError::self_throttle(
            lazybox_gh::SOURCE,
            "GitHub graphql background allowance spent (0/3)",
            15,
        );
        let wait = github_self_throttle_wait(&err, &snap, now).expect("retry-hint fallback wait");
        assert_eq!(wait.remaining, 0);
        assert_eq!(wait.limit, 0);
        assert_eq!(wait.retry_after_secs(now), 15);
    }
}

/// Debounce sentinel that collapses an entire retryable-transient
/// streak — a throttle, an HTTP 502, a "retrying in Ns" hiccup the
/// daemon is already auto-retrying — onto a single `ProviderError`
/// broadcast. The NUL prefix guarantees it can never equal a real
/// `ProviderError::user_message`. See
/// [`TickState::broadcast_error_debounced`] (#727).
const RETRYABLE_DEDUPE_KEY: &str = "\u{0}retryable";

/// Debounce sentinel for the escalated `exhausted` error, distinct from
/// [`RETRYABLE_DEDUPE_KEY`] so the quiet→actionable transition
/// re-broadcasts once while the exhausted state itself stays coalesced.
/// See [`TickState::broadcast_error_debounced`] (#730).
const EXHAUSTED_DEDUPE_KEY: &str = "\u{0}exhausted";

/// Consecutive retryable-failure cycles a source may burn before its
/// transient escalates from a quiet status to an actionable `exhausted`
/// error. Below this the daemon is still auto-retrying and the failure
/// is self-healing noise; at it, retries are exhausted and sync is
/// genuinely stuck (#730).
const RETRYABLE_EXHAUSTION_ATTEMPTS: u32 = 3;

pub async fn tick_with_state(
    config: &ServerConfig,
    sources: &[Box<dyn TaskSource>],
    state: &mut TickState,
) -> TickOutcome {
    // Track every workspace key we upserted this tick. Callers use
    // it for "in scope" rescoping after the tick — anything in the
    // store NOT in this set is a candidate for removal.
    let mut polled: Vec<WorkspaceKey> = Vec::new();
    // Per-source success tracking. Rescoping needs "did anyone
    // actually report?" — a genuinely empty result set (filter
    // matches nothing) is data; "all sources errored" is not.
    let mut any_source_succeeded = false;
    // Longest "retry after N seconds" hint surfaced by any source
    // this tick. Plumbed back into the driver loop so we sleep at
    // least that long before the next attempt — without it we'd
    // keep firing the same rate-limited query at the normal cadence
    // and watch the budget stay pegged.
    let mut max_retry_after_secs: Option<u64> = None;
    let mut saw_unknown_mergeable = false;
    // Per-source authoritative coverage for this tick. Only successful
    // sources land here — a failed source has no authority to delete
    // stored workspaces this cycle, so omitting it lets `rescope`
    // preserve them. (Issue #34: a round-robin GH tick only polls 3
    // of N repos; without per-source scope, `rescope` would delete
    // workspaces from the unpolled (N - 3) repos.)
    let mut source_scopes: std::collections::HashMap<String, PolledScope> =
        std::collections::HashMap::new();
    // Default to true: empty `sources` (no providers configured) is a
    // legitimate "no work, but rescope cleanly" path. The loop below
    // flips to false the moment any successful source returns an
    // incremental fetch.
    let mut all_full = true;
    for source in sources {
        let fetch_started = std::time::Instant::now();
        match source.fetch().await {
            Ok(tasks) => {
                let fetch_ms = fetch_started.elapsed().as_millis();
                any_source_succeeded = true;
                source_scopes.insert(source.name().to_string(), source.polled_scope());
                let mode = source.last_fetch_kind();
                if mode != FetchMode::Full {
                    all_full = false;
                }
                if let Some(secs) = source.retry_after_secs() {
                    max_retry_after_secs =
                        Some(max_retry_after_secs.map_or(secs, |existing| existing.max(secs)));
                    state.park_source(source.name(), secs);
                }
                let count = tasks.len();
                tracing::info!(
                    source = source.name(),
                    path = mode.label(),
                    count,
                    fetch_ms,
                    "sync: fetch complete"
                );
                // A 0-result poll is a SUCCESSFUL query that happens to
                // match nothing — not a failure. It's usually benign
                // (filter/scope matches no open work right now) and
                // sometimes a transient GitHub hiccup that returns 200
                // with an empty `search.nodes`. Either way the query
                // didn't error, so we must NOT raise a `ProviderError`:
                // doing so painted a sticky red "✗ sync failed" banner
                // (Permanent severity, never auto-fades) on a healthy
                // sync — especially jarring right after a manual
                // Shift-R. We still log loudly for diagnostics, and the
                // `PollCompleted { count: 0 }` below lets the TUI show a
                // calm "✓ sync ok — 0 tasks" notice instead.
                if count == 0 {
                    tracing::warn!(
                        source = source.name(),
                        "poll returned 0 tasks — if unexpected, check `,` Settings: filter roles \
                         + selected scopes both have to match SOMETHING in the user's repos. \
                         /tmp/lazybox.log has the exact GraphQL query string above."
                    );
                }
                // Per-task wall-clock cap. The git-op `run_git_in`
                // timeout (30s) already guards against hung subprocs,
                // but defense-in-depth: anything in the upsert path
                // (sqlite, bus broadcast, prepare_upsert's
                // closing-issues scan, an unexpected `.await` on
                // something we don't own) gets 15s total. If a single
                // task exceeds that, we log loudly + skip it; the
                // next tick re-attempts. Without this guard, the
                // poll loop's critical path was uncapped — one bad
                // task could paralyze every subsequent tick.
                const UPSERT_TIMEOUT_PER_TASK: std::time::Duration =
                    std::time::Duration::from_secs(15);
                let upsert_started = std::time::Instant::now();
                let total = tasks.len();
                // Seed the round-robin cursor with repos we
                // observed in the result set. Borrowed `&str` dedup
                // keeps allocation cost proportional to unique repos
                // (typically <10), not total task count (often >30).
                // Done BEFORE `tasks.into_iter()` so the dedup set's
                // borrow lifetime ends cleanly; the cursor write
                // itself owns the small handful of unique strings.
                // Combined with the pre-fetch bump in `sources_for`,
                // this captures both "we queried this repo" and "the
                // global sweep turned up a new repo we now want to
                // round-robin through" — the rotation expands
                // automatically as the user's involvement set grows.
                let now = std::time::Instant::now();
                if source.name() == lazybox_gh::SOURCE {
                    let engagement = config.poll.engagement_snapshot();
                    let mut seen_repos: std::collections::HashSet<&str> =
                        std::collections::HashSet::new();
                    for task in &tasks {
                        let workspace_key =
                            WorkspaceKey::new(lazybox_core::workspace_key_for(task));
                        if engagement.tier_for(&workspace_key) == EngagementTier::Cold {
                            continue;
                        }
                        if let Some(repo) = task.repo.as_deref()
                            && !repo.is_empty()
                        {
                            seen_repos.insert(repo);
                        }
                    }
                    for repo in &seen_repos {
                        state.round_robin.record_sync(repo, now);
                    }
                }
                // One store scan for the whole batch instead of a
                // KV read + full workspace-list deserialize per task
                // — see `UpsertContext`.
                let mut upsert_ctx = UpsertContext::build(config);
                let mut changed_items = 0usize;
                for (i, task) in tasks.into_iter().enumerate() {
                    let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
                    let task_id = task.id.to_string();
                    // Fast-repoll trigger for GitHub's lazily-computed
                    // mergeability — with two guards that used to be
                    // missing:
                    //   1. Only NON-terminal tasks count. Merged/closed
                    //      PRs never get a computed value upstream, so
                    //      counting them kept the 5s fast-loop armed on
                    //      literally every merged-sweep tick (the
                    //      provider now maps them to a definitive value,
                    //      but the scheduler must not depend on every
                    //      provider getting that right).
                    //   2. Per-task probe cap. GitHub usually computes
                    //      mergeability within a few seconds; if N fast
                    //      probes in a row still say Unknown, fall back
                    //      to the normal cadence for that task instead
                    //      of 5s-polling indefinitely. The counter
                    //      resets the moment a definitive value lands.
                    if task.mergeable == lazybox_core::Mergeable::Unknown
                        && !matches!(
                            task.state,
                            lazybox_core::TaskState::Merged | lazybox_core::TaskState::Closed
                        )
                    {
                        let probes = state
                            .unknown_mergeable_probes
                            .entry(task_id.clone())
                            .or_insert(0);
                        *probes = probes.saturating_add(1);
                        if *probes <= UNKNOWN_MERGEABLE_MAX_FAST_PROBES {
                            saw_unknown_mergeable = true;
                        } else if *probes == UNKNOWN_MERGEABLE_MAX_FAST_PROBES + 1 {
                            tracing::info!(
                                task = %task_id,
                                "mergeable still UNKNOWN after {UNKNOWN_MERGEABLE_MAX_FAST_PROBES} \
                                 fast probes — falling back to the normal poll cadence"
                            );
                        }
                    } else {
                        state.unknown_mergeable_probes.remove(&task_id);
                    }
                    polled.push(key);
                    let one_started = std::time::Instant::now();
                    match tokio::time::timeout(
                        UPSERT_TIMEOUT_PER_TASK,
                        upsert_with_context(config, &mut upsert_ctx, task),
                    )
                    .await
                    {
                        Ok(outcome) => {
                            changed_items += usize::from(outcome == CommitOutcome::Changed);
                            let one_ms = one_started.elapsed().as_millis();
                            if one_ms > 500 {
                                tracing::warn!(
                                    "upsert {}/{total} ({task_id}) took {one_ms}ms — slow",
                                    i + 1
                                );
                            }
                        }
                        Err(_elapsed) => {
                            tracing::error!(
                                "upsert {}/{total} ({task_id}) TIMED OUT after {}s — \
                                 skipping; next tick will re-attempt",
                                i + 1,
                                UPSERT_TIMEOUT_PER_TASK.as_secs(),
                            );
                            // A single task's upsert timing out is a
                            // per-task hiccup, NOT a sync-stuck signal: the
                            // fetch itself succeeded, so this deliberately
                            // does not feed the exhaustion streak (and could
                            // not — the successful fetch calls `clear_error`
                            // below, resetting it). Emitted as a plain quiet
                            // `retryable` status; the TUI keeps it off the
                            // error banner (#730).
                            let _ = config.bus.send(Event::ProviderError {
                                source: source.name().to_string(),
                                message: format!(
                                    "upsert timed out on {task_id} — task skipped this tick"
                                ),
                                detail: "see /tmp/lazybox.log for the slow step".into(),
                                kind: ProviderErrorKind::Retryable.as_str().to_string(),
                            });
                        }
                    }
                }
                source.record_items_changed(changed_items);
                tracing::info!(
                    "tick: upserted {total} tasks in {}ms",
                    upsert_started.elapsed().as_millis()
                );
                // Clear the debounce slot — the next failure should
                // broadcast even if it carries the same message as a
                // previous run.
                state.clear_error(source.name());
                // Always emit `PollCompleted`, even on 0 tasks, so
                // the TUI can distinguish "polling hasn't run yet"
                // from "polling found nothing matching your filter".
                let _ = config.bus.send(Event::PollCompleted {
                    source: source.name().to_string(),
                    count,
                });
                if source.name() == lazybox_gh::SOURCE {
                    let now = Utc::now();
                    let rate_limit_wait = config
                        .poll
                        .cached_gh_client()
                        .and_then(|client| github_rate_limit_wait(&client.rate_snapshot(), now));
                    if let Some(wait) = rate_limit_wait {
                        let secs = wait.retry_after_secs(now);
                        max_retry_after_secs =
                            Some(max_retry_after_secs.map_or(secs, |existing| existing.max(secs)));
                        let _ = config.bus.send(wait.event());
                    }
                }
                // Drain + dispatch any side-effect actions the source
                // queued during `fetch` (today: auto-spawn requests
                // from `@lazybox` mentions).
                //
                // ORDERING INVARIANT: this MUST run after the upsert
                // loop AND before rescope. `dispatch_action` calls
                // `handle_spawn`, which expects the freshly-upserted
                // workspace to exist on disk; rescope may delete
                // out-of-scope workspaces. Moving this below rescope
                // would silently break auto-spawn — the workspace
                // gets deleted before the spawn dispatches and the
                // agent starts in a sandbox.
                let actions = source.drain_actions();
                if !actions.is_empty() {
                    tracing::info!(
                        source = source.name(),
                        count = actions.len(),
                        "dispatching provider actions"
                    );
                }
                // Clone the cached GitHub client (Arc-backed, cheap) so
                // the auto-fix arm can post its PR comment. Read from the
                // dedicated cache lock, not `poll_state` (issue #92).
                let gh = config.poll.cached_gh_client();
                for action in actions {
                    dispatch_action(config, source.name(), gh.as_ref(), action).await;
                }
            }
            Err(e) => {
                if e.is_retryable() {
                    tracing::warn!(diagnostic = %e.diagnostic(), "poll failed (retryable)");
                } else if e.is_auth() {
                    tracing::error!(diagnostic = %e.diagnostic(), "poll failed (auth)");
                } else {
                    tracing::error!(diagnostic = %e.diagnostic(), "poll failed (permanent)");
                }
                // An auth-classified failure means the token the cached
                // client holds is dead (expired / revoked / rotated).
                // Drop the cache so the NEXT tick re-resolves the
                // credential chain and rebuilds from scratch — without
                // this, the cache-reuse filter could keep handing back
                // the bricked client until daemon restart.
                if e.is_auth() && source.name() == lazybox_gh::SOURCE {
                    config.poll.clear_cached_gh_client();
                    tracing::info!(
                        "cleared cached GitHub client after auth failure — \
                         next tick rebuilds from the credential chain"
                    );
                }
                // Capture the longest retry-after hint across all
                // failing sources this tick. Provider gave us a
                // precise number (GitHub's rateLimit.resetAt) —
                // honor it.
                if let Some(secs) = e.retry_after_secs() {
                    max_retry_after_secs =
                        Some(max_retry_after_secs.map_or(secs, |existing| existing.max(secs)));
                    state.park_source(source.name(), secs);
                }
                let now = Utc::now();
                let rate_limit_wait = if source.name() == lazybox_gh::SOURCE {
                    config.poll.cached_gh_client().and_then(|client| {
                        let snapshot = client.rate_snapshot();
                        // A remote-budget exhaustion / API 403 is an honest
                        // wait; a governor self-throttle (still well above the
                        // low threshold) is the same "waiting, not working"
                        // condition and must surface as a wait too, or its
                        // debounced retryable error leaves the "syncing…"
                        // spinner spinning forever.
                        github_rate_limit_wait(&snapshot, now)
                            .or_else(|| github_self_throttle_wait(&e, &snapshot, now))
                    })
                } else {
                    None
                };
                if let Some(wait) = rate_limit_wait {
                    let secs = wait.retry_after_secs(now);
                    max_retry_after_secs =
                        Some(max_retry_after_secs.map_or(secs, |existing| existing.max(secs)));
                    state.park_source(source.name(), secs);
                    state.clear_error(source.name());
                    let _ = config.bus.send(wait.event());
                } else {
                    state.broadcast_error_debounced(&config.bus, source.name(), &e);
                }
            }
        }
    }
    TickOutcome {
        polled,
        any_source_succeeded,
        retry_after_secs: max_retry_after_secs,
        saw_unknown_mergeable,
        source_scopes,
        all_full,
    }
}

/// What `tick` / `tick_with_state` returns. The list of workspace
/// keys polled into the store, plus a "did anyone actually report?"
/// flag so callers (rescope) can distinguish "filter genuinely
/// matches nothing today" from "every source failed".
#[derive(Debug)]
pub struct TickOutcome {
    pub polled: Vec<WorkspaceKey>,
    pub any_source_succeeded: bool,
    /// Longest "wait at least N seconds before retrying" hint
    /// surfaced by any source this tick — populated when a provider
    /// reports a precise reset window (GitHub's `rateLimit.resetAt`,
    /// HTTP `Retry-After`, …). The polling loop's outer driver uses
    /// this to extend the sleep before the next tick, instead of
    /// blindly tick-tick-ticking at the configured cadence and
    /// burning the same rate-limit error each time.
    pub retry_after_secs: Option<u64>,
    /// True when at least one polled task carried
    /// `Mergeable::Unknown` — GitHub returns that while it lazily
    /// computes mergeability after a new commit. The loop schedules
    /// one extra wake at +5s so the next poll picks up the real
    /// value instead of waiting out the full interval.
    pub saw_unknown_mergeable: bool,
    /// Per-source authoritative coverage for this tick. Populated for
    /// every source whose `fetch` returned `Ok` — failed sources are
    /// omitted so `rescope` preserves their workspaces (we don't know
    /// which ones are still in upstream scope after a transient
    /// error). Empty when no source succeeded OR when callers
    /// (legacy tests) construct an outcome manually; in those cases
    /// `rescope` falls back to its pre-#34 behavior of treating every
    /// stored workspace as exhaustively covered.
    ///
    /// A `HashMap` (not a `Vec`) because each source appears at most
    /// once per tick — duplicates would be a bug, and the rescope
    /// lookup is point-query by source name.
    pub source_scopes: std::collections::HashMap<String, PolledScope>,
    /// True when EVERY successful source reported `FetchMode::Full`.
    /// Rescope is conditional on this — an incremental
    /// (notifications-driven) source only returns the tasks GitHub
    /// flagged as recently changed, so trusting "anything not polled
    /// is out of scope" would delete every workspace the user didn't
    /// touch in the last 30 seconds. See `rescope_with_state`.
    pub all_full: bool,
}

// INVARIANT: `all_full` defaults to `true`, NOT `bool::default()`.
// The "no sources configured" rescope path constructs a default
// outcome and then runs rescope — leaving `all_full: false` would
// silently disable rescope cleanup for that path. Hand-rolled
// `Default` impl rather than derive so the override is impossible
// to miss in code review.
impl Default for TickOutcome {
    fn default() -> Self {
        Self {
            polled: Vec::new(),
            any_source_succeeded: false,
            retry_after_secs: None,
            saw_unknown_mergeable: false,
            source_scopes: std::collections::HashMap::new(),
            all_full: true,
        }
    }
}

/// Compare `polled` against the persisted workspace set; remove
/// workspaces no longer in scope (filter / scope change). Active
/// sessions are preserved — those workspaces stay until the user
/// kills them explicitly (or, in a future phase, confirms removal
/// via a prompt).
///
/// Empty `polled` is treated as "no data this cycle" and skipped —
/// otherwise a single network blip would wipe the whole sidebar.
/// Callers that genuinely want a fresh slate should delete
/// workspaces directly.
pub async fn rescope(config: &ServerConfig, outcome: &TickOutcome) {
    let mut state = TickState::default();
    rescope_with_state(config, outcome, &mut state).await;
}

pub async fn rescope_with_state(
    config: &ServerConfig,
    outcome: &TickOutcome,
    state: &mut TickState,
) {
    // No source produced a successful response — every provider
    // errored out (rate limit, network, auth). Treat as a transient
    // hiccup and skip the rescope; otherwise a single bad minute
    // would wipe the whole sidebar.
    if !outcome.any_source_succeeded {
        return;
    }
    // Incremental ticks (notifications-driven fast path, issue #19)
    // only return the tasks GitHub flagged as recently changed. The
    // workspaces NOT in `polled` are the ones nobody mentioned this
    // window — almost always "still in scope, just quiet." Trusting
    // rescope here would delete every untouched workspace; wait for
    // the next FULL sweep (≤ 10 min by default) which gives us the
    // complete in-scope picture.
    if !outcome.all_full {
        tracing::debug!(
            polled = outcome.polled.len(),
            "rescope: skipping (incremental tick — waiting for next full sweep)"
        );
        return;
    }
    // CRITICAL data-loss guard: a 0-result poll wipes the entire
    // inbox if we let rescope run. Plausible causes for "0 tasks
    // returned" include:
    //   - GitHub API hiccup mid-search (returns 200 OK with empty
    //     `search.nodes`)
    //   - the user just edited their `~/.lazybox/config.yaml` scopes
    //     and removed every repo (deliberate)
    //   - a transient auth issue that returns no results without
    //     erroring
    // Only the second case is a real intent-to-rescope. Without a
    // way to distinguish, the safest default is "never rescope on
    // an empty result." The user can press `x x` / Settings →
    // Clean to explicitly remove rows.
    //
    // Symptom this fixes: user pressed Shift-R, got
    // "github returned 0 tasks", and ALL workspaces vanished.
    if outcome.polled.is_empty() {
        tracing::warn!(
            "rescope: skipping (0 polled tasks — refusing to delete every workspace; \
             the next non-empty poll will rescope normally)"
        );
        return;
    }
    let polled_set: std::collections::HashSet<&str> =
        outcome.polled.iter().map(|k| k.as_str()).collect();
    // Anything we polled is back in scope — drop any "already
    // prompted" memory for it so a future fall-out triggers a fresh
    // prompt.
    state
        .prompted_out_of_scope
        .retain(|k| !polled_set.contains(k.as_str()));

    // Per-source coverage map. A workspace is only a deletion
    // candidate when its source ran successfully this tick AND
    // (the source ran exhaustively OR the workspace's repo is in
    // the source's polled-repos slice). Issue #34: pre-fix, a
    // round-robin GH tick polled 3 of N repos, but every stored
    // workspace not in `polled` got deleted — so PRs from the
    // unpolled (N - 3) repos disappeared every warm tick.
    //
    // An empty map (legacy callers, test fixtures, the no-sources
    // path) signals "no per-source info this tick" — we fall back
    // to the pre-#34 behavior of treating every unpolled workspace
    // as a deletion candidate. The production poll loop always
    // populates this.
    let scope_by_source = &outcome.source_scopes;

    // Full-table scan on `spawn_blocking` (issue #34's convention):
    // synchronous rusqlite under a contending process (busy_timeout =
    // 5s) would otherwise pin this runtime worker for seconds.
    let store = config.store.clone();
    let records = match tokio::task::spawn_blocking(move || store.list_workspaces()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!("rescope: list_workspaces failed: {e}");
            return;
        }
        Err(e) => {
            tracing::warn!("rescope: list_workspaces task failed: {e}");
            return;
        }
    };

    // Per session_key → count of live terminals. Lets us both
    // detect "has active session" and report the count to the user
    // when prompting.
    let entries = config.terminal.entries.lock().await;
    let mut active_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for entry in entries.values().filter(|entry| !entry.finishing) {
        if let Some((sk, _)) = entry.meta.as_ref() {
            *active_counts.entry(sk.as_str().to_string()).or_default() += 1;
        }
    }
    drop(entries);

    let now = chrono::Utc::now();
    for r in records {
        if polled_set.contains(r.key.as_str()) {
            continue;
        }
        let key = WorkspaceKey::new(r.key.clone());
        // Decode the stored workspace once — used by both the
        // snoozed-skip guard AND the locally-created (no upstream
        // task) guard below. Lenient decode is deliberate: this copy
        // only feeds preserve-guards, never a write. A row that fails
        // to decode here can still never be reaped — the silent-delete
        // branch below re-loads through the STRICT `load_workspace`
        // and preserves anything it cannot read.
        let stored_ws = r
            .workspace_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<Workspace>(j).ok());
        // Preserve snoozed workspaces. The user said "hide until
        // <date>"; deleting on a poll that doesn't list it would
        // defeat that intent.
        if stored_ws.as_ref().is_some_and(|w| w.is_snoozed(now)) {
            continue;
        }
        // Preserve locally-authored workspaces. The user created
        // these by hand (the `n` flow), not from a provider task, so
        // they never appear in the polled set — pruning them on a
        // poll that doesn't list them destroys work the user
        // explicitly created (issue #87). The `local` flag is
        // authoritative; the task-shape fallback covers records
        // written before the flag existed (pre-PR sandbox workspaces
        // carry a `project_key` but no upstream task).
        if stored_ws.as_ref().is_some_and(|w| {
            w.local
                || (w.pr.is_none()
                    && w.gh_issues.is_empty()
                    && w.linear_issues.is_empty()
                    && w.project_key.is_some())
        }) {
            continue;
        }
        // Per-source scope guard (issue #34). Only authoritative
        // sources can authorize a delete. A workspace whose source
        // didn't run this tick (transient error, source not enabled)
        // or whose repo wasn't in the polled slice (round-robin)
        // must be preserved — we have no fresh information about it.
        //
        // Empty `scope_by_source` keeps legacy behavior (every
        // unpolled workspace is a candidate); orphans with no
        // primary task fall through to the existing locally-created
        // guard above.
        if !scope_by_source.is_empty()
            && let Some(task) = stored_ws.as_ref().and_then(|w| w.primary_task())
        {
            let source = task.id.source.as_str();
            let in_authoritative_scope = match scope_by_source.get(source) {
                None => false,
                Some(PolledScope::Exhaustive) => true,
                Some(PolledScope::Repos(repos)) => task
                    .repo
                    .as_deref()
                    .is_some_and(|r| repos.iter().any(|x| x == r)),
            };
            if !in_authoritative_scope {
                tracing::debug!(
                    workspace_key = %r.key,
                    source,
                    repo = task.repo.as_deref().unwrap_or(""),
                    "rescope: preserving (out of this tick's authoritative scope)"
                );
                continue;
            }
        }
        // An out-of-scope ISSUE workspace that still carries sessions
        // is almost always a just-auto-closed issue whose PR merged
        // ("Closes #N"): GitHub drops the closed issue from the
        // open-scope poll, so it lands here before
        // `merge_closing_issue_workspaces` gets another chance to fold
        // it in. The silent-delete branch below keys "is anything
        // running?" off live `terminal_meta` entries, but a session
        // whose PTY has exited is still a recoverable record in
        // `sessions` — deleting the workspace would take it (and any
        // live terminal) with it (issue #202). Collapse it into the
        // claiming PR instead, the same absorb the confirm-merge path
        // runs.
        let collapse_target = stored_ws
            .as_ref()
            .filter(|w| w.pr.is_none() && !w.sessions.is_empty())
            .and_then(|w| w.primary_task())
            .and_then(|t| pr_workspace_claiming_issue(config, &t.id));
        if let Some(pr_key) = collapse_target {
            tracing::info!(
                issue_workspace = %r.key,
                pr_workspace = %pr_key,
                "rescope: collapsing out-of-scope issue with sessions into its PR instead of deleting"
            );
            handle_confirm_merge(config, key.clone(), pr_key, true).await;
            state.prompted_out_of_scope.remove(r.key.as_str());
            continue;
        }
        match active_counts.get(r.key.as_str()).copied() {
            None | Some(0) => {
                // A live terminal isn't the only thing worth preserving:
                // a session whose PTY has exited (the agent finished and
                // closed Claude, the common case right after it opens a
                // PR) keeps a recoverable worktree + session record, but
                // leaves no `terminal_meta` entry. Reaping here on a full
                // sweep that happened to drop the workspace from scope is
                // the "session lost on merge" bug (#136): a merged PR is
                // surfaced for removal once, at the merge transition, via
                // `MergedPrRemovable` — rescope must not race ahead of
                // that prompt and silently destroy the work. Gate the
                // sweep on session records, not just live terminals, so
                // the only rows it reaps are genuinely session-less.
                if stored_ws.as_ref().is_some_and(|w| !w.sessions.is_empty()) {
                    tracing::info!(
                        workspace_key = %r.key,
                        "rescope: preserving out-of-scope workspace with recoverable sessions"
                    );
                    continue;
                }
                // User notes are user work exactly like sessions are:
                // a session-less row carrying a non-empty note must
                // survive the sweep (it just goes Inactive) rather
                // than take the note with it.
                if stored_ws.as_ref().is_some_and(|w| w.has_notes()) {
                    tracing::info!(
                        workspace_key = %r.key,
                        "rescope: preserving out-of-scope workspace with user notes"
                    );
                    continue;
                }
                // Safe to remove silently: nothing's running.
                tracing::info!(
                    workspace_key = %r.key,
                    "rescope: removing out-of-scope workspace"
                );
                // The lifecycle owns the tombstone, workspace lock, and final
                // fresh empty-row/worktree check. Rescope does not archive, so
                // an upstream item can reappear on a later poll.
                let _ = crate::workspace::WorkspaceLifecycle::new(config)
                    .remove(&key, crate::workspace::WorkspaceRemovalReason::Rescope)
                    .await;
                state.prompted_out_of_scope.remove(r.key.as_str());
            }
            Some(count) => {
                // Has active sessions — ask the user, once. Without
                // the dedupe, every 60s tick would re-fire the same
                // prompt for a workspace the user already dismissed.
                if state.prompted_out_of_scope.contains(r.key.as_str()) {
                    continue;
                }
                state.prompted_out_of_scope.insert(r.key.clone());
                // Build a short label + title from the stored workspace
                // JSON if available; fall back to the raw key.
                //
                // `task.id.key` is already `owner/repo#N` (e.g.
                // `acme/widget#7307`) — concatenating `repo`
                // in front of it previously produced
                // `acme/widget#acme/widget#7307`.
                // Trust `id.key` and only fall back to `repo` when the
                // key is missing.
                let task_ref = r
                    .workspace_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<lazybox_core::Workspace>(json).ok())
                    .and_then(|w| w.primary_task().cloned());
                let (label, title) = match task_ref {
                    Some(t) => {
                        let label = if !t.id.key.is_empty() {
                            t.id.key.clone()
                        } else if let Some(repo) = &t.repo {
                            repo.clone()
                        } else {
                            r.key.clone()
                        };
                        let title = if !t.title.is_empty() {
                            Some(t.title.clone())
                        } else {
                            None
                        };
                        (label, title)
                    }
                    None => (r.key.clone(), None),
                };
                tracing::info!(
                    workspace_key = %r.key,
                    active = count,
                    "rescope: out of scope with active sessions — prompting"
                );
                let _ = config.bus.send(Event::WorkspaceOutOfScope {
                    workspace_key: key,
                    label,
                    title,
                    active_terminal_count: count,
                });
            }
        }
    }
}

/// Spawn the long-lived polling loop. Returns the join handle so the
/// caller can `abort()` on shutdown if it wants — `lazybox server stop`
/// drops the whole process so we don't bother in main.
///
/// Each tick reads `~/.lazybox/config.yaml` fresh and rebuilds the
/// source list. This means a filter / scope change made via the
/// Settings palette takes effect on the NEXT tick at the latest —
/// no separate "respawn polling" plumbing needed, and the previous
/// per-Finish-respawn pattern (which leaked one tokio task per
/// edit) is gone.
pub fn spawn(config: ServerConfig, interval: Duration) -> tokio::task::JoinHandle<()> {
    // Two reasons this loop is more complicated than the old
    // `tick → sleep(interval)` shape:
    //
    // 1. **Laptop sleep** — `tokio::time::sleep` uses the OS
    //    monotonic clock, which on macOS pauses while the lid is
    //    closed. A 60s sleep started before sleep finishes 60s
    //    *after* wake, so polling effectively dies for whatever
    //    wall-clock interval the laptop was asleep. We now sleep in
    //    short chunks (≤5s) and compare wall-clock `Instant` since
    //    last tick — the moment the wake delivers a chunk that took
    //    longer than the interval, we tick.
    //
    // 2. **Active TUI waiting on fresh data** — `Command::Refresh`,
    //    a new client connecting, or "GitHub returned `mergeable:
    //    UNKNOWN`, please re-query in a few seconds" all want a
    //    fast wake instead of waiting out the rest of a 60s sleep.
    //    `config.poll.wait_for_wake()` is selected against the
    //    chunked sleep — pinging the Notify forces an immediate
    //    re-check of the wall-clock condition.
    tokio::spawn(async move {
        use std::time::Instant;
        const CHUNK: Duration = Duration::from_secs(5);
        const UNKNOWN_RETRY: Duration = Duration::from_secs(5);
        /// Hard floor between scheduled tick starts. A tick can cost a
        /// full credential resolve + GraphQL sweep; nothing legitimate
        /// needs it re-run with zero gap.
        const MIN_TICK_GAP: Duration = Duration::from_secs(5);

        tracing::info!(
            "polling: loop started (interval={}s, every tick logs `polling: tick #N starting`)",
            interval.as_secs()
        );
        // One-shot config snapshot at boot — single greppable line
        // for "why no PRs?" debugging. Lists every enabled provider,
        // its filter-role keys, and its scope set. Without this, a
        // collaborator's "doesn't sync for me" report required us to
        // ask 4 questions over chat; now: `grep "polling: config" /tmp/lazybox.log`.
        if let Ok(cfg) = lazybox_config::Config::load() {
            let providers: Vec<&String> = cfg.setup.providers.iter().collect();
            let filters: std::collections::BTreeMap<&String, Vec<&String>> = cfg
                .setup
                .filters
                .iter()
                .map(|(p, keys)| (p, keys.iter().collect()))
                .collect();
            let scopes: std::collections::BTreeMap<&String, Vec<&String>> = cfg
                .setup
                .scopes
                .iter()
                .map(|(p, keys)| (p, keys.iter().collect()))
                .collect();
            tracing::info!(
                "polling: config — providers={:?} filters={:?} scopes={:?}",
                providers,
                filters,
                scopes,
            );
        } else {
            tracing::warn!(
                "polling: config — could not load ~/.lazybox/config.yaml; falling back to defaults"
            );
        }

        // `next_due` starts in the past so the first iteration ticks
        // immediately (matches the previous loop's "first run is
        // eager" behaviour).
        let mut next_due: Instant = Instant::now();
        let mut next_warm_due: Instant = Instant::now();
        let mut tick_n: u64 = 0;
        loop {
            // Wait until `next_due`, with the chunked-sleep + wake
            // path baked in. If we're already past `next_due` (e.g.
            // first iteration, or laptop just woke), skip straight
            // through.
            loop {
                let now = Instant::now();
                if now >= next_due {
                    break;
                }
                let remaining = next_due - now;
                let chunk = remaining.min(CHUNK);
                tokio::select! {
                    _ = tokio::time::sleep(chunk) => {}
                    _ = config.poll.wait_for_wake() => {
                        tracing::info!("polling: woken (Refresh / Subscribe / UNKNOWN retry)");
                        break;
                    }
                }
            }

            tick_n += 1;
            tracing::info!("polling: tick #{tick_n} starting");
            let warm_requested = config.poll.take_warm_request();
            let poll_warm = Instant::now() >= next_warm_due || warm_requested;
            // Force Linear past its cadence gate ONLY on an explicit user
            // refresh — NOT on `warm_requested`, which the many
            // post-mutation / subscribe `wake(true)` calls also set and
            // would otherwise re-poll Linear on unrelated GitHub activity
            // (#1032).
            let force_linear = config.poll.take_force_refresh();

            // Tolerate panics inside `run_one_tick`. tokio swallows
            // panics from spawned tasks by default; without this
            // wrapper a single buggy poll cycle would silently kill
            // the entire long-lived loop, leaving CI/mergeable badges
            // frozen until daemon restart. Caught panics get logged
            // at error level + the loop continues with a normal
            // interval — degraded behaviour is far better than
            // silent death.
            let summary = match std::panic::AssertUnwindSafe(run_one_tick_with_notifications(
                &config,
                poll_warm,
                force_linear,
            ))
            .catch_unwind()
            .await
            {
                Ok(s) => s,
                Err(payload) => {
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "<non-string panic payload>".to_string()
                    };
                    tracing::error!(
                        "polling: tick #{tick_n} PANICKED: {msg} — loop will continue at next interval"
                    );
                    // A caught poll-cycle panic is a code bug, not something
                    // the user can remediate — escalating it to the
                    // exhaustion path would mislabel it "check your
                    // connection or token", and the `TickState` lives inside
                    // the panicked future (checked out of `poll_state`) and
                    // is not safely reachable here anyway. It stays a quiet
                    // `retryable` status for parity with other self-healing
                    // hiccups; the error-level log above is the developer
                    // signal (#730).
                    let _ = config.bus.send(Event::ProviderError {
                        source: "github".into(),
                        message: format!("poll cycle crashed: {msg}"),
                        detail: msg,
                        kind: ProviderErrorKind::Retryable.as_str().to_string(),
                    });
                    TickSummary::default()
                }
            };
            tracing::info!(
                "polling: tick #{tick_n} done (path={}, retry_after={:?}, unknown_mergeable={}, hot={})",
                if summary.all_full {
                    "full-sweep"
                } else {
                    "incremental"
                },
                summary.retry_after_secs,
                summary.saw_unknown_mergeable,
                summary.hot_count,
            );

            if poll_warm {
                next_warm_due = Instant::now() + interval;
            }
            let warm_in = next_warm_due.saturating_duration_since(Instant::now());
            let next_in = next_tick_delay_with_hot(
                warm_in,
                summary.retry_after_secs,
                summary.saw_unknown_mergeable,
                UNKNOWN_RETRY,
                summary.hot_count,
            );
            // Wakes raised WHILE the tick body ran (Done agent states,
            // focus changes, subscribes) left a stored `Notify` permit
            // that resolved the next wait instantly — and a wake-driven
            // tick that outlived the warm window zeroed `warm_in`. Both
            // produced the logged "next tick in 0s" storms: back-to-back
            // ticks, each re-running credential resolution and the
            // GraphQL sweep. Drain the permit here and honor it as one
            // coalesced follow-up after MIN_TICK_GAP; genuinely new
            // wakes (an explicit Refresh) still interrupt the sleep
            // immediately, so user latency is unchanged.
            let woke_during_tick = config.poll.drain_pending_wake();
            let next_in = if woke_during_tick {
                tracing::debug!(
                    "polling: wake during tick #{tick_n} — coalescing into one follow-up tick"
                );
                MIN_TICK_GAP
            } else {
                next_in.max(MIN_TICK_GAP)
            };
            if summary.retry_after_secs.is_some() {
                tracing::warn!(
                    "polling: backing off {}s before next tick (rate-limit hint)",
                    next_in.as_secs(),
                );
            } else if summary.saw_unknown_mergeable && next_in < interval {
                tracing::info!(
                    "polling: re-firing in {}s to chase UNKNOWN mergeable",
                    next_in.as_secs(),
                );
            }
            next_due = Instant::now() + next_in;
            // Expose the effective cadence every tick so "why is sync
            // slow?" is answerable from the log alone: the base interval,
            // the actual wait (longer when backing off), and whether a
            // rate-limit hint forced the gap open.
            tracing::info!(
                "polling: tick #{tick_n} next tick in {}s (base interval {}s{})",
                next_in.as_secs(),
                interval.as_secs(),
                if next_in > interval {
                    " — backing off"
                } else {
                    ""
                },
            );
        }
    })
}

/// Test-only entry point: spawn a polling loop with an explicit
/// source list (skips the YAML reload). Production code should use
/// `spawn`; this exists so tests can inject mock `TaskSource`s
/// without writing a config file.
#[doc(hidden)]
pub fn spawn_with_sources(
    config: ServerConfig,
    sources: Vec<Box<dyn TaskSource>>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    // Mirrors `spawn`'s chunked-sleep + Notify-wake loop so the
    // test entry covers the same control flow (laptop-sleep
    // resilience + Refresh / Subscribe waking the next tick).
    tokio::spawn(async move {
        if sources.is_empty() {
            tracing::warn!("no provider sources configured — polling task is idle");
            return;
        }
        use std::time::Instant;
        let chunk = interval.min(Duration::from_secs(5));
        const UNKNOWN_RETRY: Duration = Duration::from_secs(5);
        let mut state = TickState::default();
        let mut next_due: Instant = Instant::now();
        loop {
            loop {
                let now = Instant::now();
                if now >= next_due {
                    break;
                }
                let remaining = next_due - now;
                let nap = remaining.min(chunk);
                tokio::select! {
                    _ = tokio::time::sleep(nap) => {}
                    _ = config.poll.wait_for_wake() => { break; }
                }
            }
            let outcome = tick_with_state(&config, &sources, &mut state).await;
            let retry_after = outcome.retry_after_secs;
            let saw_unknown = outcome.saw_unknown_mergeable;
            rescope_with_state(&config, &outcome, &mut state).await;
            let next_in = next_tick_delay(interval, retry_after, saw_unknown, UNKNOWN_RETRY);
            next_due = Instant::now() + next_in;
        }
    })
}

/// Single iteration of the poll loop. Loads the latest persisted
/// setup, builds sources, ticks, rescopes. Shared between the
/// long-lived spawn and the `Command::Refresh` immediate-tick path.
/// Uses `config.poll.tick_state` so prompt-dismissal memory crosses both
/// paths.
/// Summary of one tick — what the driver loop needs to schedule
/// the next one. `retry_after_secs` extends the next sleep when a
/// provider reported a rate-limit reset window; `saw_unknown_mergeable`
/// triggers a quick re-poll so GitHub's lazy mergeability landing
/// doesn't have to wait out the full interval.
/// How long to sleep before the next poll tick. Pure so the ordering
/// contract is testable:
///
/// 1. Start from the configured base cadence.
/// 2. The unknown-mergeable fast probe may only SHORTEN it (chase
///    GitHub's lazy mergeability computation ~5s later).
/// 3. A provider-reported `retry_after` may only LENGTHEN it — and it
///    is applied LAST, so a rate-limit reset window always beats the
///    fast probe. (The old code applied the 5s override after the
///    retry-after clamp, which stomped a provider-mandated multi-
///    minute backoff down to 5s and kept hammering a limited token.)
pub fn next_tick_delay(
    interval: Duration,
    retry_after_secs: Option<u64>,
    saw_unknown_mergeable: bool,
    unknown_retry: Duration,
) -> Duration {
    next_tick_delay_with_hot(
        interval,
        retry_after_secs,
        saw_unknown_mergeable,
        unknown_retry,
        0,
    )
}

pub fn next_tick_delay_with_hot(
    interval: Duration,
    retry_after_secs: Option<u64>,
    saw_unknown_mergeable: bool,
    unknown_retry: Duration,
    hot_count: usize,
) -> Duration {
    let engagement_interval = background_tick_interval(interval, hot_count);
    let base = if saw_unknown_mergeable {
        engagement_interval.min(unknown_retry)
    } else {
        engagement_interval
    };
    // The DRIVER-level sleep is clamped (#1218): a retry hint parks
    // only its own source (`TickState::source_backoff_until`), so the
    // loop itself never has to sleep out a full GraphQL window — that
    // global sleep is what froze Linear, the REST notifications
    // heartbeat, and the hot path for 55 minutes on one GitHub hint.
    // The clamp still slows empty ticks (the parked source builds
    // nothing), it just never stops the world.
    //
    // NOTE: Tunable via `server.polling_backoff_cap_secs` in config.
    // This constant is the default; the daemon should read the config
    // and use that value instead (tracked in #1254).
    const DRIVER_BACKOFF_CAP: Duration = Duration::from_secs(120);
    match retry_after_secs {
        Some(secs) => base.max(Duration::from_secs(secs).min(DRIVER_BACKOFF_CAP)),
        None => base,
    }
}

pub fn background_tick_interval(interval: Duration, hot_count: usize) -> Duration {
    if hot_count > 0 {
        interval.min(HOT_POLL_INTERVAL)
    } else {
        interval
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TickSummary {
    pub retry_after_secs: Option<u64>,
    pub saw_unknown_mergeable: bool,
    /// True when every successful source ran a full sweep (no source
    /// took the incremental notifications path). Surfaced in the
    /// driver's per-tick log so the delivery path of a slow update is
    /// visible without cross-referencing per-source lines.
    pub all_full: bool,
    /// Number of bounded GitHub targets that keep the loop on its
    /// tighter cadence.
    pub hot_count: usize,
}

/// Check the cross-tick [`TickState`] OUT of `config.poll.tick_state`,
/// returning an owned copy and leaving a `default()` in its place.
/// Paired with [`restore_poll_state`].
///
/// This is the structural fix for the issue #133 footgun. `run_one_tick`
/// used to hold the `poll_state` guard across the ENTIRE tick — every
/// network fetch, every per-task `upsert`, rescope. Two problems
/// followed:
///
/// 1. **Re-entrant deadlock.** `poll_state` is a non-reentrant
///    `tokio::sync::Mutex`. Anything reachable from the deep `upsert`
///    call chain that reached back for `poll_state.lock().await`
///    self-deadlocked until a downstream timeout fired — the
///    merge-collapse path did exactly that (issue #131). Holding the
///    guard for the whole tick made re-introducing that trivial.
/// 2. **Serve-loop starvation.** The serve loop's own `poll_state`
///    users — the detached `fetch_pr_details` client cache, the
///    round-robin focus hint — blocked behind a ~17s GitHub fetch, so
///    typing in an agent / opening a session stalled until the sync
///    finished.
///
/// Checking the state out instead means the guard is FREE for the
/// fetch + upsert duration. The driver loop runs ticks serially
/// (`run_one_tick` is awaited to completion before the next), so no
/// other tick contends for the checked-out state; the only concurrent
/// writer is the focus hint, which [`restore_poll_state`] folds back in.
pub async fn checkout_poll_state(poll: &crate::PollState) -> TickState {
    std::mem::take(&mut *poll.tick_state.lock().await)
}

/// Restore a [`TickState`] checked out by [`checkout_poll_state`].
///
/// While the state was checked out, the serve loop's round-robin focus
/// hint (`set_focused_workspace`) may have recorded a fresh
/// `focused_repo` into the now-default state behind `poll_state`. That
/// is the user's latest sidebar navigation and must steer the NEXT
/// tick's round-robin, so we prefer it over the value the tick carried
/// out. Every other `TickState` field is owned exclusively by the tick,
/// so the checked-out copy is authoritative for them.
pub async fn restore_poll_state(poll: &crate::PollState, mut state: TickState) {
    let mut guard = poll.tick_state.lock().await;
    if guard.round_robin.focused_repo.is_some() {
        state.round_robin.focused_repo = guard.round_robin.focused_repo.take();
    }
    *guard = state;
}

pub async fn run_one_tick(config: &ServerConfig) -> TickSummary {
    run_one_tick_with_notifications(config, true, true).await
}

async fn run_one_tick_with_notifications(
    config: &ServerConfig,
    poll_notifications: bool,
    force_linear: bool,
) -> TickSummary {
    let setup = match lazybox_config::Config::load() {
        Ok(c) => crate::persisted_from_config(&c),
        Err(e) => {
            tracing::warn!("polling: config.yaml load failed: {e}");
            return TickSummary::default();
        }
    };
    // Check the cross-tick `TickState` OUT of `poll_state` for the
    // duration of this tick instead of holding the guard across it.
    // INVARIANT (issue #133): the `poll_state` guard is FREE for the
    // entire fetch + upsert call chain, so nothing reachable from
    // `upsert` can block on a guard we're holding (we hold none), and
    // the serve loop's own `poll_state` users stay responsive while a
    // slow sync runs. See `checkout_poll_state`.
    let mut state = checkout_poll_state(&config.poll).await;
    let summary =
        run_tick_inner(config, &setup, &mut state, poll_notifications, force_linear).await;
    restore_poll_state(&config.poll, state).await;
    // Level-triggered removal prompts (issue #292): after every tick,
    // re-offer cleanup for any workspace still merged/closed with
    // sessions and no answer. Outside the tick body — it only needs
    // the store, not poll state, and must run even when providers
    // errored (the merged state is already persisted locally).
    reprompt_unresolved_removals(config).await;
    // Keep every "track main" workspace (issue #535) fast-forwarded to
    // its base branch. Its own pass over the store, gated on the
    // per-workspace arm — decoupled from the provider round-robin so a
    // tracked workspace is synced every tick regardless of which repos
    // this cycle happened to poll.
    sync_tracked_workspaces(config).await;
    summary
}

/// What an EMPTY source list authorizes. `Some(outcome)` — every
/// provider is deliberately disabled, so rescope with full authority
/// and let stale rows disappear. `None` — at least one provider is
/// enabled but produced no source (credentials timed out, cadence
/// gate, client init failure): a FAILED view of the world, never an
/// authoritative empty one, so no rescope may run (2026-08-19 audit,
/// D1 — the fabricated `any_source_succeeded: true` here was one guard
/// away from deleting every workspace on a `gh auth token` timeout).
fn empty_sources_rescope_outcome(any_provider_enabled: bool) -> Option<TickOutcome> {
    if any_provider_enabled {
        return None;
    }
    Some(TickOutcome {
        polled: vec![],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::new(),
        all_full: true,
    })
}

/// The body of one poll tick, operating on a `state` checked out of
/// `poll_state`. Builds the source list, ticks, rescopes. Split out of
/// `run_one_tick` so the checkout/restore of `poll_state` brackets it
/// cleanly (see `checkout_poll_state`).
async fn run_tick_inner(
    config: &ServerConfig,
    setup: &lazybox_core::PersistedSetup,
    state: &mut TickState,
    poll_notifications: bool,
    force_linear: bool,
) -> TickSummary {
    let engagement = refresh_github_engagement(config).await;
    let hot_count = if setup.enabled_providers.contains(lazybox_gh::SOURCE) {
        engagement.hot_count()
    } else {
        0
    };
    state.round_robin.update_engagement_repos(
        engagement.cold_only_repos(),
        engagement.active_repos(),
        std::time::Instant::now(),
    );
    let sources = sources_for_with_engagement(
        setup,
        config.bus.clone(),
        state,
        config.poll.viewer_identities.clone(),
        config.poll.gh_client_cache.clone(),
        &engagement,
        poll_notifications,
        force_linear,
        Some(config.store.clone()),
    )
    .await;
    if sources.is_empty() {
        // Two very different worlds produce an empty source list, and
        // conflating them nearly deleted every workspace (2026-08-19
        // audit, D1):
        //
        // 1. The user disabled every provider. A deliberate empty
        //    result — rescope with full authority so stale rows from
        //    the disabled providers actually disappear.
        // 2. Providers are ENABLED but none produced a source this
        //    tick — credentials timed out, Linear's cadence gate
        //    skipped it, a client init failed. That is a FAILED view
        //    of the world, not an empty one: asserting success +
        //    `all_full` here sailed a 5s `gh auth token` timeout past
        //    every rescope authority guard, leaving only the final
        //    "refusing to delete every workspace" backstop between a
        //    subprocess timeout and total data loss.
        match empty_sources_rescope_outcome(!setup.enabled_providers.is_empty()) {
            Some(outcome) => rescope_with_state(config, &outcome, state).await,
            None => tracing::warn!(
                "no poll sources despite enabled providers — treating the tick as failed, \
                 preserving all workspaces (no rescope)"
            ),
        }
        return TickSummary {
            hot_count,
            ..TickSummary::default()
        };
    }
    // Overall tick cap — defense in depth. Each sub-step already has
    // its own timeout (25s per graphql call × 3 retries, 30s per git
    // subprocess, 15s per upsert), but the OUTER tick has no cap.
    // A pathological combination — slow network + busy fs + 35
    // tasks each timing out — could still consume minutes. 180s is
    // generous (a real worst-case tick on a slow network with watched
    // repos can hit ~60s) but well under the long-poll spinner's
    // 90s footer guard, so a stuck tick surfaces visibly.
    const TICK_OVERALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
    let outcome = match tokio::time::timeout(
        TICK_OVERALL_TIMEOUT,
        tick_with_state(config, &sources, state),
    )
    .await
    {
        Ok(o) => o,
        Err(_) => {
            tracing::error!(
                "tick_with_state TIMED OUT after {}s — abandoning this tick, next interval will retry",
                TICK_OVERALL_TIMEOUT.as_secs()
            );
            // A whole tick overrunning the outer cap IS a sync failure —
            // route it through the same debounced/escalating path as a
            // fetch error (not a raw per-tick broadcast) so it coalesces
            // to one quiet status and, if it keeps happening, escalates to
            // an actionable `exhausted` error once retries run out (#730).
            // The timed-out future has been dropped by `.await`, so its
            // `&mut state` borrow is released and `state` is usable here.
            state.broadcast_error_debounced(
                &config.bus,
                lazybox_gh::SOURCE,
                &lazybox_core::ProviderError::retryable(
                    lazybox_gh::SOURCE,
                    format!(
                        "sync exceeded {}s — the per-upsert / per-graphql / per-git \
                         timeouts should catch this; hitting the outer cap means \
                         something escaped them",
                        TICK_OVERALL_TIMEOUT.as_secs()
                    ),
                ),
            );
            // Hard-timeout path: NOT a clean view of scope, so leave
            // `all_full = false` to keep rescope conservative.
            TickOutcome {
                polled: vec![],
                any_source_succeeded: false,
                retry_after_secs: Some(30),
                saw_unknown_mergeable: false,
                source_scopes: std::collections::HashMap::new(),
                all_full: false,
            }
        }
    };
    let summary = TickSummary {
        retry_after_secs: outcome.retry_after_secs,
        saw_unknown_mergeable: outcome.saw_unknown_mergeable,
        all_full: outcome.all_full,
        hot_count,
    };
    rescope_with_state(config, &outcome, state).await;
    // Warm the right pane before the user gets there (issue #530):
    // after a successful poll, prefetch review-thread details for the
    // top-N highest-attention PRs so opening a row doesn't pay a cold
    // `fetch_pr_details` round-trip. Runs inline on the tick's tail —
    // the tick no longer holds `poll_state` across its body (#133), so
    // the old detached spawn that re-locked it is unnecessary. The
    // batch is ~1s / ~5 GraphQL units (see `prefetch_top_pr_details`),
    // trivial against a multi-second sweep and the 5000/hr budget. Skip
    // it on a failed tick — `polled` is empty and the client may be
    // rate-limited.
    //
    // Bounded by its own cap: this runs OUTSIDE the tick's overall
    // timeout, and a pathological hang (per-detail 25s × 3 retries,
    // several in flight) could otherwise stall the next tick. On elapse
    // the future is dropped — the up-front dedup marks already landed,
    // and any un-warmed row still lazy-fetches on focus.
    if outcome.any_source_succeeded {
        const PREFETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        if tokio::time::timeout(
            PREFETCH_TIMEOUT,
            prefetch_top_pr_details(config, &outcome.polled, state),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                "prefetch_top_pr_details exceeded {}s — abandoning; rows lazy-fetch on focus",
                PREFETCH_TIMEOUT.as_secs()
            );
        }
    }
    summary
}

/// Scan stored workspaces for one whose PR claims `issue_id` via
/// `closes_issues`. Returns the PR's workspace key when a match is
/// found. Linear in the workspace count — fine in practice (10s to
/// low 100s of workspaces).
fn pr_workspace_claiming_issue(
    config: &ServerConfig,
    issue_id: &lazybox_core::TaskId,
) -> Option<WorkspaceKey> {
    let records = config.store.list_workspaces().ok()?;
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(ws) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };
        let Some(pr) = &ws.pr else {
            continue;
        };
        if pr.closes_issues.iter().any(|id| id == issue_id) {
            return Some(ws.key);
        }
    }
    None
}

/// The terminal state that makes `workspace` a removal candidate:
/// a merged PR, or a closed issue no PR workspace claims (the PR's
/// own prompt owns that cleanup — same deferral as the upsert path).
/// `None` for open work, task-less rows, and workspaces the user
/// already answered "keep" for ([`lazybox_core::CleanupPrompt::Declined`]).
///
/// A **merged PR** qualifies even with no sessions (issue #499): its
/// tracking row should be offered for cleanup even when it never had a
/// worktree — removal just drops the row, but the user shouldn't have to
/// discover `x x` to be rid of it. A **closed issue** likewise qualifies
/// with or without sessions (issue #552): `prompt_merged_pr_removal_with`
/// auto-removes a session-less row and prompts when there's a backing
/// worktree (issue #1129), so the level-trigger sweep must
/// reach even a bare closed-issue row that the open→closed transition
/// missed (a daemon restart, a lagged broadcast).
fn removal_candidate_state(
    config: &ServerConfig,
    workspace: &Workspace,
) -> Option<lazybox_ipc::RemovableTerminalState> {
    // A removal already in flight owns this workspace: don't nominate it as
    // a fresh cleanup candidate while `remove()` is tearing it down (the
    // tombstone is released only if that removal fails or the row is gone).
    if config
        .deleted_workspaces
        .lock()
        .contains(workspace.key.as_str())
    {
        return None;
    }
    if workspace.cleanup_prompt == lazybox_core::CleanupPrompt::Declined {
        return None;
    }
    let task = workspace.primary_task()?;
    if task.is_pr() {
        return (task.state == lazybox_core::TaskState::Merged)
            .then_some(lazybox_ipc::RemovableTerminalState::Merged);
    }
    if task.state != lazybox_core::TaskState::Closed
        || pr_workspace_claiming_issue(config, &task.id).is_some()
    {
        return None;
    }
    Some(lazybox_ipc::RemovableTerminalState::Closed)
}

/// Level-trigger sweep for workspace-removal prompts. The open→terminal
/// transition emits `MergedPrRemovable` exactly once; if that one
/// broadcast is missed (TUI lagged on the bus, client disconnected,
/// daemon restarted after persisting the merged state) the workspace
/// would sit unprompted forever (issue #292). Runs once per poll tick:
/// any stored workspace still in terminal state with sessions — and
/// not answered "keep" — is re-prompted through the same
/// `handlers::prompt_merged_pr_removal_with` path, whose
/// [`RemovalPromptMemory`] gate keeps the cadence to
/// `REMOVAL_REPROMPT_AFTER`.
///
/// No-op when `worktree.auto_cleanup_merged` is on — that opt-in path
/// reaps silently on the transition instead of prompting.
pub async fn reprompt_unresolved_removals(config: &ServerConfig) {
    let auto = lazybox_config::Config::load()
        .map(|c| c.worktree.auto_cleanup_merged)
        .unwrap_or(false);
    if auto {
        return;
    }
    reprompt_unresolved_removals_with(config, &config.worktree_manager()).await;
}

/// Test seam for [`reprompt_unresolved_removals`] — explicit manager
/// so tests can root it at a tempdir without mutating `LAZYBOX_HOME`.
pub(crate) async fn reprompt_unresolved_removals_with(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
) {
    let records = match crate::store_blocking(&config.store, |store| store.list_workspaces()).await
    {
        Ok(records) => records,
        Err(e) => {
            tracing::warn!("reprompt_unresolved_removals: list_workspaces failed: {e}");
            return;
        }
    };
    for record in records {
        let Some(ws) = record
            .workspace_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<Workspace>(j).ok())
        else {
            continue;
        };
        let Some(state) = removal_candidate_state(config, &ws) else {
            continue;
        };
        handlers::prompt_merged_pr_removal_with(config, mgr, &ws.key, state).await;
    }
}

/// Background "track main" sweep (issue #535). For every workspace the
/// user armed with track-main, keep its worktree fast-forwarded onto
/// `origin/<default>` while clean, and record whether it's behind so the
/// sidebar badge reflects reality. Runs every tick, independent of the
/// provider round-robin — a track-main workspace is swept even when its
/// repo wasn't polled this cycle and even when providers are offline
/// (this is local git work). Cheap when nothing is armed: one
/// `list_workspaces` read and an early return.
pub async fn sync_tracked_workspaces(config: &ServerConfig) {
    sync_tracked_workspaces_with(config, &config.worktree_manager()).await;
}

/// Test seam for [`sync_tracked_workspaces`] — explicit manager so tests
/// can root it at a tempdir without mutating `LAZYBOX_HOME`.
pub(crate) async fn sync_tracked_workspaces_with(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
) {
    let records = match crate::store_blocking(&config.store, |store| store.list_workspaces()).await
    {
        Ok(records) => records,
        Err(e) => {
            tracing::warn!("sync_tracked_workspaces: list_workspaces failed: {e}");
            return;
        }
    };
    // Pre-filter to the armed keys so we don't lock + reload every row.
    let keys: Vec<WorkspaceKey> = records
        .into_iter()
        .filter_map(|r| r.workspace_json)
        .filter_map(|j| serde_json::from_str::<Workspace>(&j).ok())
        .filter(|ws| ws.track_main)
        .map(|ws| ws.key)
        .collect();
    if keys.is_empty() {
        return;
    }

    let cfg = lazybox_config::Config::load().unwrap_or_default();
    let github_scopes = github_scopes_from_config(&cfg);
    for key in keys {
        sync_one_tracked_workspace(config, mgr, &github_scopes, &key).await;
    }
}

/// Fast-forward one track-main workspace's worktrees and persist its
/// resolved base branch + "behind" verdict. Isolated per workspace so a
/// single repo's fetch failure never aborts the rest of the sweep.
///
/// The workspace lock is held only for the brief snapshot and the final
/// commit — never across the network git (fetch + merge). A background
/// sweep holding the per-workspace lock through a stalled fetch would
/// block whatever command the user issues on that workspace next
/// (reply, inject, snooze, spawn — all take the same lock), so the git
/// work runs lock-free between two short critical sections.
async fn sync_one_tracked_workspace(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    github_scopes: &std::collections::BTreeSet<String>,
    key: &WorkspaceKey,
) {
    // ── Snapshot under the lock, then release before any network git. ──
    let (repo, base_branch, worktrees) = {
        let _ws_guard = config.lock_workspace(key.as_str()).await;
        let Some(workspace) = load_workspace_offloaded(config, key).await else {
            return;
        };
        // Re-check under the lock: the user may have disarmed since the
        // scan, or the row may be a linked / repo-less / PR workspace
        // tracking can't act on.
        if !workspace.track_main || !workspace.supports_track_main() {
            return;
        }
        let Ok(repo) = crate::spawn_handler::clonable_repo_from_project(
            config,
            &workspace,
            Some(github_scopes),
        ) else {
            return;
        };
        let worktrees: Vec<std::path::PathBuf> = workspace
            .sessions
            .iter()
            .map(|s| s.worktree_path.clone())
            .filter(|p| p.exists())
            .collect();
        (repo, workspace.base_branch.clone(), worktrees)
    };

    // Nothing on disk to sync → don't resolve a base branch. Resolving it
    // runs `ensure_bare_clone`, which would clone the whole repo for a
    // workspace that has no worktree to fast-forward.
    if worktrees.is_empty() {
        return;
    }
    let Some((owner, name)) = repo.split_once('/') else {
        return;
    };

    // ── Network git, lock-free. ──────────────────────────────────────
    // Resolve the base branch once (handles main vs master); remember
    // whether we resolved it fresh so the commit phase persists it.
    let (base, base_resolved) = match base_branch {
        Some(b) => (b, false),
        None => match mgr
            .default_branch(owner, name, lazybox_git_ops::LockPriority::Background)
            .await
        {
            Ok(b) => (b, true),
            Err(e) => {
                tracing::debug!(workspace = %key, error = %e, "track-main: default branch unresolved");
                return;
            }
        },
    };

    // Fast-forward every on-disk worktree. A workspace can hold several
    // (review + experiment); "behind" if ANY is behind-and-blocked.
    let mut behind = false;
    for wt in &worktrees {
        match mgr.fast_forward_to_base(wt, owner, name, &base).await {
            Ok(outcome) => {
                if outcome.is_behind() {
                    behind = true;
                }
                tracing::debug!(workspace = %key, ?outcome, "track-main sync");
            }
            Err(e) => {
                // A failing FF means "not verifiably synced" — exactly
                // what the behind flag exists to say ("could not be
                // brought up to date automatically"). The old arm left
                // `behind` untouched, so a persistently erroring sync
                // (base ref gone, merge blocked on an untracked-file
                // collision) rendered as ✓ synced forever.
                behind = true;
                tracing::warn!(workspace = %key, worktree = %wt.display(), error = %e, "track-main sync failed");
            }
        }
    }

    // ── Persist the sweep-owned verdict under the lock. ──────────────
    // Re-load so a concurrent user edit (reply, notes, disarm) between
    // the snapshot and now isn't clobbered by a blind overwrite.
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    if !workspace.track_main {
        return;
    }
    let mut dirty = false;
    if base_resolved && workspace.base_branch.is_none() {
        workspace.base_branch = Some(base);
        dirty = true;
    }
    if workspace.track_main_behind != behind {
        workspace.track_main_behind = behind;
        dirty = true;
    }
    if dirty {
        commit_upsert_offloaded_reported(config, key, workspace, "track-main sync").await;
    }
}

/// Handle `Command::KeepMergedWorkspace`: the user answered "no" on
/// the removal modal. Persist [`lazybox_core::CleanupPrompt::Declined`] on the row so
/// the reprompt sweep stops asking — across restarts, not just this
/// session (issue #499). The row stays until removed explicitly.
pub async fn keep_merged_workspace(config: &ServerConfig, key: &WorkspaceKey) {
    config
        .poll
        .removal_prompts
        .lock()
        .await
        .prompted
        .remove(key.as_str());
    let outcome = apply_and_commit(config, key, |ws| {
        ws.cleanup_prompt = lazybox_core::CleanupPrompt::Declined;
    })
    .await;
    tracing::info!(
        workspace = %key,
        ?outcome,
        "user kept terminal-state workspace; cleanup prompt declined (persisted)"
    );
}

/// A client just (re)connected: forget the per-workspace emit
/// timestamps (NOT the "keep" pins) so the next reprompt sweep
/// re-fires immediately for anything still unresolved, instead of
/// waiting out `REMOVAL_REPROMPT_AFTER`. A prompt the reconnecting
/// client never saw shouldn't be throttled as if it had been.
pub async fn mark_removal_prompts_for_replay(config: &ServerConfig) {
    config.poll.removal_prompts.lock().await.prompted.clear();
}

/// If `workspace`'s PR closes issues that lazybox tracks as their own
/// workspaces, fold each issue's workspace into `workspace` and
/// remove the standalone row. Sessions move over (terminals keep
/// running); `terminal_meta` is rewritten so wire-side events for
/// the old session_key flow to the new one.
///
/// Safety net: when the issue workspace has live sessions, we DON'T
/// merge silently — auto-absorbing a user's running Claude/codex
/// session into a different workspace key is too easy to miss. We
/// emit `WorkspaceMergePending` instead and stash the candidate;
/// the TUI prompts and replies via `Command::ConfirmMerge`. Empty
/// issue workspaces still merge silently and emit a
/// `WorkspaceMerged` notice so the user sees the row disappear
/// with context.
///
/// No-op when there's no PR, no `closes_issues`, or no matching
/// issue workspace exists yet.
/// Re-prompt the merge modal after this long when the previous
/// prompt was dismissed without an explicit Y/N. 5 minutes is short
/// enough that a user who Esc'd "I'll deal with this later" sees
/// the prompt come back the same session, long enough that the
/// dismissal isn't immediately undone (the user wants a beat to
/// finish what they were doing).
const MERGE_REPROMPT_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// Re-emit an unanswered workspace-removal prompt after this long,
/// for the same reason as [`MERGE_REPROMPT_AFTER`]: a dismissal (or a
/// lost broadcast) shouldn't mean never being asked again.
pub(crate) const REMOVAL_REPROMPT_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// An issue workspace absorbed into a PR during
/// [`merge_closing_issue_workspaces`], whose store row must be deleted in
/// the same transaction that saves the absorbed PR. The matching removal
/// events are queued into the same commit owner after that transaction
/// succeeds, so clients never project a partially committed collapse.
struct PendingIssueMerge {
    issue_key: WorkspaceKey,
    issue_label: String,
    pr_label: String,
    /// Ids of the sessions the absorb carried over from this issue —
    /// the checkouts the collapse must never lose. `commit_merge` uses
    /// them to tell carried sessions apart from ones the PR minted for
    /// itself beforehand (stub-retirement candidates).
    moved_session_ids: Vec<lazybox_core::SessionId>,
}

/// Issue id a lazybox-named branch implies. Issue spawns check out
/// `<prefix>/issue-<n>-<title-slug>` (see
/// `spawn_handler::derive_branch_for_branchless`), so a PR opened from that
/// worktree closes issue `<repo>#<n>` even when neither GitHub's
/// `closingIssuesReferences` nor the body text says so yet (the agent forgot
/// the "Closes #N" line, or the lazy details fetch hasn't run). Used as an
/// extra collapse candidate — the "target must be an ISSUE workspace" filter
/// downstream keeps a false positive harmless.
///
/// The id lives in the `issue-<n>` stem of the branch's last path segment,
/// not in a fixed `lazybox/issue-<n>` string: the branch prefix is empty by
/// default (#108) and a title slug is appended after the number (#109), so
/// the match reads the leading numeric component of the stem and ignores
/// both the prefix and the slug.
///
/// GitHub-only: the `issue-<n>` stem and the `<repo>#<n>` key it rebuilds are
/// GitHub conventions (`derive_branch_for_branchless` only emits that stem for
/// GitHub tasks). Gating on the source both keeps the heuristic off other
/// providers — whose keys aren't `<repo>#<n>` — and narrows the branch shapes
/// a stray match could fire on.
fn issue_id_from_branch(pr: &Task) -> Option<lazybox_core::TaskId> {
    if !pr.id.source.eq_ignore_ascii_case("github") {
        return None;
    }
    let branch = pr.branch.as_deref()?;
    let stem = branch.rsplit('/').next().unwrap_or(branch);
    let number = stem
        .strip_prefix("issue-")?
        .split('-')
        .next()
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))?;
    let repo = pr.repo.as_deref().filter(|r| !r.is_empty())?;
    Some(lazybox_core::TaskId {
        source: pr.id.source.clone(),
        key: format!("{repo}#{number}"),
    })
}

/// True when the PR body references `issue` with a NON-closing keyword
/// (`part of` / `refs` / `ref` / `related to`) immediately before `#<n>`.
/// GitHub turns only *closing* keywords into `closingIssuesReferences`, so
/// a body that says "Part of #<n>" is explicit intent that the PR does NOT
/// close the issue — the weak branch-name heuristic must defer to it (#581).
/// A body with `Closes #<n>` not yet resolved matches no non-closing keyword
/// here, so the branch fallback still fires during that timing gap.
fn body_references_issue_non_closing(pr: &Task, issue: &lazybox_core::TaskId) -> bool {
    let Some(number) = issue.key.rsplit('#').next().filter(|n| !n.is_empty()) else {
        return false;
    };
    let Some(body) = pr.body.as_deref() else {
        return false;
    };
    let lower = body.to_ascii_lowercase();
    const NON_CLOSING_KEYWORDS: &[&str] = &["part of", "refs", "ref", "related to"];
    for keyword in NON_CLOSING_KEYWORDS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(keyword) {
            let after = from + rel + keyword.len();
            if reference_number_follows(&lower[after..], number) {
                return true;
            }
            from = after;
        }
    }
    false
}

/// After a keyword, does `#<number>` follow (allowing `:`/whitespace in
/// between)? The trailing boundary check keeps `#57` from matching a `#579`
/// reference.
fn reference_number_follows(after_keyword: &str, number: &str) -> bool {
    let rest = after_keyword.trim_start_matches([' ', '\t', '\r', '\n', ':']);
    let Some(rest) = rest.strip_prefix('#') else {
        return false;
    };
    match rest.strip_prefix(number) {
        Some(tail) => !tail.starts_with(|c: char| c.is_ascii_digit()),
        None => false,
    }
}

/// Workspace rows a PR may absorb, including the lazybox branch-name
/// fallback. Kept in one helper so lock planning and the merge pass cannot
/// drift into recognizing different source rows.
///
/// The branch-name link is a WEAK signal: an agent that named its branch
/// `issue-<n>-…` usually does close that issue, but a PR that deliberately
/// does NOT ("Part of #<n>", tracking/checklist issues) must win. So the
/// branch fallback fires only in the timing gap where GitHub has resolved
/// no closing reference yet (`closes_issues` empty) AND the body carries no
/// explicit non-closing reference to the branch-derived issue (#581). Once
/// `closingIssuesReferences` is populated it is authoritative and the branch
/// stem is ignored entirely.
fn closing_issue_workspace_keys(pr: &Task) -> Vec<WorkspaceKey> {
    let mut ids = pr.closes_issues.clone();
    if pr.closes_issues.is_empty()
        && let Some(id) = issue_id_from_branch(pr)
        && !body_references_issue_non_closing(pr, &id)
    {
        ids.push(id);
    }
    // Cross-provider links (#922): Linear ticket identifiers parsed from
    // the PR's branch / title / body by the GitHub provider. Always
    // considered — the `TEAM-<n>` shape is specific, and the downstream
    // "target must be a ticket/issue workspace" filter keeps a stray
    // match harmless.
    ids.extend(pr.linked_tasks.iter().cloned());
    let mut keys: Vec<_> = ids
        .into_iter()
        .map(|id| issue_id_to_workspace_key(&id))
        .collect();
    keys.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    keys.dedup();
    keys
}

/// Full collapse-candidate set for a PR, unioning the pure candidates
/// ([`closing_issue_workspace_keys`]) with the authoritative Linear
/// attachment signal (#922), which lives on the *ticket* and so needs a
/// store scan the pure list can't do. Used by both lock planning and the
/// merge pass so the two can't recognize different source rows.
fn collapse_candidate_keys(config: &ServerConfig, pr: &Task) -> Vec<WorkspaceKey> {
    let mut keys = closing_issue_workspace_keys(pr);
    for key in linked_ticket_workspace_keys(config, pr) {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    keys.dedup();
    keys
}

/// Linear ticket workspaces that link back to `pr` through the ticket's
/// own GitHub attachment (#922) — Linear's integration records the PR
/// URL, which the Linear provider parsed into the ticket's
/// `linked_tasks`. Authoritative but ticket-side, so it can't be derived
/// from the PR alone. Only PR-less (ticket) workspaces are considered, so
/// this never folds one PR into another.
fn linked_ticket_workspace_keys(config: &ServerConfig, pr: &Task) -> Vec<WorkspaceKey> {
    let Ok(records) = config.store.list_workspaces() else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(ws) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };
        if ws.pr.is_some() {
            continue;
        }
        let links_pr = ws
            .linear_issues
            .iter()
            .any(|ticket| ticket.linked_tasks.contains(&pr.id));
        if links_pr {
            keys.push(ws.key);
        }
    }
    keys
}

/// Acquire the destination PR and every source issue lock in canonical
/// order. The destination is re-read after acquisition; if a concurrent
/// details write added a closing ref between planning and locking, drop the
/// guards and retry with the expanded set before touching any source row.
async fn lock_workspace_with_closing_issues(
    config: &ServerConfig,
    workspace_key: &WorkspaceKey,
    incoming_pr: Option<&Task>,
) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
    let mut keys = std::collections::BTreeSet::from([workspace_key.as_str().to_string()]);
    if let Some(pr) = incoming_pr.filter(|task| task.is_pr()) {
        keys.extend(
            collapse_candidate_keys(config, pr)
                .into_iter()
                .map(|key| key.as_str().to_string()),
        );
    }

    loop {
        let guards = config.lock_workspaces(keys.iter().cloned()).await;
        let mut required = keys.clone();
        if let Some(workspace) = load_workspace(config, workspace_key)
            && let Some(pr) = workspace.pr.as_ref()
        {
            required.extend(
                collapse_candidate_keys(config, pr)
                    .into_iter()
                    .map(|key| key.as_str().to_string()),
            );
        }
        if required == keys {
            return guards;
        }
        drop(guards);
        keys = required;
    }
}

async fn merge_closing_issue_workspaces(
    config: &ServerConfig,
    workspace: &mut Workspace,
) -> Vec<PendingIssueMerge> {
    let mut pending: Vec<PendingIssueMerge> = Vec::new();
    let Some(pr) = workspace.pr.as_ref() else {
        return pending;
    };
    let issue_keys = collapse_candidate_keys(config, pr);
    // Snapshot which candidates came from GitHub's `closingIssuesReferences`
    // (vs the weak branch-name fallback) so the merge log names the real
    // source (#581) — the immutable `pr` borrow ends before the loop mutates
    // `workspace`.
    let closes_keys: std::collections::HashSet<String> = pr
        .closes_issues
        .iter()
        .map(|id| issue_id_to_workspace_key(id).as_str().to_string())
        .collect();
    if issue_keys.is_empty() {
        tracing::trace!(
            workspace = %workspace.key,
            "merge: PR has no closes_issues — nothing to fold"
        );
        return pending;
    }
    tracing::debug!(
        workspace = %workspace.key,
        candidates = ?issue_keys.iter().map(WorkspaceKey::as_str).collect::<Vec<_>>(),
        "merge: scanning closes_issues for collapse candidates"
    );

    for issue_key in issue_keys {
        if issue_key == workspace.key {
            // Self-link — nothing to merge.
            continue;
        }
        let Some(issue_ws) = load_workspace(config, &issue_key) else {
            continue;
        };

        // Critical safety check: only merge ACTUAL issue workspaces.
        // GitHub's `#N` syntax is the same for issues and PRs, and
        // our body-text fallback parser can't tell them apart from
        // the body alone — a PR body that says "Closes #141" where
        // #141 is itself a PR would otherwise have us absorb that
        // PR's workspace into this one. Symptom: PRs vanish from
        // the inbox shortly after each poll. (`closingIssuesReferences`
        // from GraphQL is safe — GitHub only returns issues there —
        // but we union both sources and have to filter here.)
        if issue_ws.pr.is_some() {
            tracing::debug!(
                target_workspace = %issue_key,
                source_pr = ?workspace.pr.as_ref().map(|p| &p.id),
                "skip merge: target is itself a PR workspace, not an issue"
            );
            continue;
        }

        // An explicit "no" pins the rows apart until restart, even if
        // the protected terminal exits later. Without this gate ahead
        // of the silent path, the next poll absorbed a rejected issue
        // as soon as its live-terminal count reached zero.
        let live_terminals = handlers::count_live_terminals(config, &issue_key).await;
        let issue_key_str = issue_key.as_str().to_string();
        let should_prompt = {
            let mut prompts = config.poll.merge_prompts.lock().await;
            if prompts.rejected.contains(&issue_key_str) {
                None
            } else if live_terminals == 0 {
                Some(false)
            } else {
                // Live-terminal safety net: stall and prompt rather
                // than silently absorbing the user's running work.
                // Dead session records move over silently.
                Some({
                    let now = std::time::Instant::now();
                    let stale = prompts
                        .prompted
                        .get(&issue_key_str)
                        .map(|prev| now.duration_since(*prev) >= MERGE_REPROMPT_AFTER)
                        .unwrap_or(true);
                    if stale {
                        prompts.prompted.insert(issue_key_str, now);
                        true
                    } else {
                        false
                    }
                })
            }
        };
        let Some(should_prompt) = should_prompt else {
            continue;
        };
        if live_terminals > 0 {
            if should_prompt {
                let _ = config.bus.send(Event::WorkspaceMergePending {
                    issue_workspace_key: issue_key.clone(),
                    pr_workspace_key: workspace.key.clone(),
                    issue_label: workspace_label_for(&issue_ws, &issue_key),
                    pr_label: workspace_label_for(workspace, &workspace.key),
                    active_terminal_count: live_terminals,
                });
            }
            continue;
        }

        // No live terminal — safe to merge silently. Sessions (and
        // their dead-but-recoverable records) move onto the PR here;
        // `commit_merge` saves the PR and deletes the issue row in one
        // transaction, then its commit owner publishes removal.
        // Capture the link source before `issue_ws` is consumed below: a
        // Linear ticket workspace is a cross-provider (#922) match, which
        // is neither GitHub's `closingIssuesReferences` nor the branch-name
        // fallback.
        let is_cross_provider = issue_ws
            .primary_task()
            .is_some_and(|task| task.id.source == "linear");
        let issue_label = workspace_label_for(&issue_ws, &issue_key);
        let pr_label = workspace_label_for(workspace, &workspace.key);
        let moved_session_ids = absorb_issue_workspace(workspace, issue_ws);
        pending.push(PendingIssueMerge {
            issue_key: issue_key.clone(),
            issue_label,
            pr_label,
            moved_session_ids,
        });

        let link_source = if is_cross_provider {
            "cross-provider link"
        } else if closes_keys.contains(issue_key.as_str()) {
            "closingIssuesReferences"
        } else {
            "branch-name inference"
        };
        tracing::info!(
            issue_workspace = %issue_key,
            pr_workspace = %workspace.key,
            link_source,
            "merged issue workspace into PR"
        );
    }
    pending
}

/// Build the issue-removal event tail for the atomic move owner. Keeping these
/// events inside that owner means caller cancellation cannot strand connected
/// clients between the durable delete and its removal/merge projections.
fn issue_merge_events(pr_key: &WorkspaceKey, pending: Vec<PendingIssueMerge>) -> Vec<Event> {
    let mut events = Vec::with_capacity(pending.len() * 2);
    for merge in pending {
        events.push(Event::WorkspaceRemoved(merge.issue_key.clone()));
        events.push(Event::WorkspaceMerged {
            issue_workspace_key: merge.issue_key,
            pr_workspace_key: pr_key.clone(),
            issue_label: merge.issue_label,
            pr_label: merge.pr_label,
        });
    }
    events
}

/// Single owner of the issue→PR collapse tail — the one place the
/// I1–I6 event ordering is sequenced, so it's a property of one
/// function instead of a convention replicated across call sites.
///
/// Callers absorb their issue workspace(s) into `pr_ws` in memory first, then
/// hand the absorbed PR plus the list of issues to retire here. This function
/// owns every durable and observable side effect:
///   1. migrate worktree paths to the (possibly new) PR slug,
///   2. atomically rebadge terminal metadata, save the PR, and delete every
///      absorbed issue,
///   3. update live terminal routing and broadcast `TerminalsRebadged` (I1),
///      then `WorkspaceUpserted{pr}` carrying the moved sessions (I2),
///   4. broadcast `WorkspaceRemoved` followed by `WorkspaceMerged` per issue
///      (I3/I6).
///
/// The PR save and issue deletes are one store transaction. There is no crash
/// window in which the moved sessions exist in neither row, and no removal
/// event is emitted if the transaction fails.
///
/// `pending` may be empty: the normal (non-merge) upsert path routes
/// every commit through here too, in which case this is just the
/// migrate + commit path with no terminal or removal event tail.
/// Returns the commit outcome (`Unchanged` also covers a failed
/// commit — nothing new became durable/visible) so the caller's
/// auto-merge hook can tell a byte-identical re-poll from real news.
async fn commit_merge(
    config: &ServerConfig,
    mut pr_ws: Workspace,
    pending: Vec<PendingIssueMerge>,
    workspace_guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
) -> CommitOutcome {
    let moved: std::collections::HashSet<lazybox_core::SessionId> = pending
        .iter()
        .flat_map(|merge| merge.moved_session_ids.iter().copied())
        .collect();
    retire_pr_stub_sessions(config, &mut pr_ws, &moved).await;
    crate::spawn_handler::migrate_session_paths_if_needed(&mut pr_ws).await;
    let pr_key = pr_ws.key.clone();
    let deletes = pending
        .iter()
        .map(|merge| merge.issue_key.clone())
        .collect::<Vec<_>>();
    let pr_session_key: lazybox_core::SessionKey = (&pr_key).into();
    let terminal_moves = pending
        .iter()
        .map(|merge| {
            let issue_session_key: lazybox_core::SessionKey = (&merge.issue_key).into();
            (issue_session_key, pr_session_key.clone())
        })
        .collect();
    let post_commit_events = issue_merge_events(&pr_key, pending);
    match commit_workspace_move(
        config,
        vec![(pr_key.clone(), pr_ws)],
        deletes,
        terminal_moves,
        post_commit_events,
        workspace_guards,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            report_commit_error(config, "merge issue workspace into PR", &error);
            CommitOutcome::Unchanged
        }
    }
}

/// The worktree half of the issue→PR collapse (#446). A PR workspace
/// can mint a session for itself before the collapse runs — its
/// `closes_issues` backfills lazily, so the row is spawnable while the
/// issue's checkout (often carrying uncommitted WIP) still lives under
/// the issue slug. Once the absorb carries that real checkout across,
/// such a pre-existing PR session is usually a pristine
/// just-provisioned stub; left alone it wins `default_session` (newest
/// `created_at`) and every later spawn lands in the stub while the
/// carried WIP sits stranded in a sibling directory.
///
/// Retire those stubs: drop the session record and remove its
/// worktree. Anything that might hold work is kept — a live terminal,
/// uncommitted or unpushed state, or a non-worktree directory with
/// contents. No-op unless the absorb carried at least one session
/// whose worktree exists on disk (retiring a healthy checkout in favor
/// of nothing would only force a re-provision).
async fn retire_pr_stub_sessions(
    config: &ServerConfig,
    pr_ws: &mut Workspace,
    moved: &std::collections::HashSet<lazybox_core::SessionId>,
) {
    if moved.is_empty() {
        return;
    }
    let mut carried_real_checkout = false;
    for session in &pr_ws.sessions {
        if moved.contains(&session.id) && tokio::fs::metadata(&session.worktree_path).await.is_ok()
        {
            carried_real_checkout = true;
            break;
        }
    }
    if !carried_real_checkout {
        return;
    }

    let live: std::collections::HashSet<lazybox_core::SessionId> = config
        .terminal
        .session_bindings()
        .await
        .into_values()
        .collect();
    let mgr = config.worktree_manager();
    let repo = pr_ws
        .primary_task()
        .and_then(|task| task.repo.as_deref())
        .and_then(|repo| repo.split_once('/'))
        .map(|(owner, name)| (owner.to_string(), name.to_string()));
    let bare = repo
        .as_ref()
        .map(|(owner, name)| mgr.bare_path(owner, name));

    let mut idx = 0;
    while idx < pr_ws.sessions.len() {
        let session = &pr_ws.sessions[idx];
        if moved.contains(&session.id) || live.contains(&session.id) {
            idx += 1;
            continue;
        }
        let path = session.worktree_path.clone();
        let branch = session.worktree_branch.clone();
        let session_id = session.id;
        let on_disk = tokio::fs::metadata(&path).await.is_ok();
        let retire = if !on_disk {
            true
        } else if tokio::fs::metadata(path.join(".git")).await.is_ok() {
            lazybox_git_ops::worktree_is_pristine(&path, bare.as_deref(), branch.as_deref()).await
        } else {
            // A provisioning fallback leaves a plain empty dir; one
            // with contents could be anything the user put there.
            dir_is_empty(&path).await
        };
        if !retire {
            idx += 1;
            continue;
        }
        tracing::info!(
            workspace = %pr_ws.key,
            session = %session_id.0,
            worktree = %path.display(),
            "collapse: retiring pristine PR stub session; the absorbed checkout takes over",
        );
        if on_disk {
            let removed = match repo.as_ref() {
                Some((owner, name)) => matches!(
                    mgr.reclaim_managed_worktree_if_safe(
                        owner,
                        name,
                        &pr_ws.branch,
                        &path,
                        lazybox_git_ops::LockPriority::Background
                    )
                    .await,
                    Ok(lazybox_git_ops::WorktreeReclaimOutcome::Reclaimed)
                ),
                // A provisioning fallback can only be retired by an
                // empty-directory removal. Unlike `remove_dir_all`, this
                // fails harmlessly if a file appears after `dir_is_empty`.
                None => tokio::fs::remove_dir(&path).await.is_ok(),
            };
            if !removed {
                tracing::warn!(
                    workspace = %pr_ws.key,
                    session = %session_id.0,
                    worktree = %path.display(),
                    "collapse: fresh safety check refused PR stub removal — preserving session",
                );
                idx += 1;
                continue;
            }
        }
        pr_ws.sessions.remove(idx);
    }
}

async fn dir_is_empty(path: &std::path::Path) -> bool {
    match tokio::fs::read_dir(path).await {
        Ok(mut entries) => matches!(entries.next_entry().await, Ok(None)),
        Err(_) => false,
    }
}

/// Re-run the issue-collapse pass for a stored PR workspace and
/// persist the result. Used when something OTHER than a provider poll
/// populates `closes_issues` — today the lazy details backfill
/// (`apply_pr_details`): the inbox SEARCH_QUERY omits
/// `closingIssuesReferences`, so the collapse inside `prepare_upsert`
/// never sees the refs until the details fetch writes them, and that
/// commit path didn't re-run the merge — the issue workspace stalled
/// standalone until the next full PR poll.
pub(super) async fn collapse_closing_issues_for(config: &ServerConfig, key: &WorkspaceKey) {
    // Lock the PR and every issue it may absorb. The planner revalidates the
    // PR after acquisition, so a racing details write cannot introduce an
    // unlocked source row.
    let workspace_guards = lock_workspace_with_closing_issues(config, key, None).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    let pending = merge_closing_issue_workspaces(config, &mut workspace).await;
    if pending.is_empty() {
        // Nothing absorbed (no candidates, or a live-terminal prompt
        // fired instead) — skip the redundant commit + broadcast.
        return;
    }
    commit_merge(config, workspace, pending, workspace_guards).await;
}

/// The TUI replied to a `WorkspaceMergePending` prompt. Accept → run
/// the merge, persist + broadcast the absorbed PR workspace, drop the
/// stash. Reject → drop the stash + pin the issue key into
/// `rejected_merge` so we don't re-prompt this session.
pub async fn handle_confirm_merge(
    config: &ServerConfig,
    issue_workspace_key: WorkspaceKey,
    pr_workspace_key: WorkspaceKey,
    accept: bool,
) {
    {
        let mut prompts = config.poll.merge_prompts.lock().await;
        prompts.prompted.remove(issue_workspace_key.as_str());
        if !accept {
            prompts
                .rejected
                .insert(issue_workspace_key.as_str().to_string());
        }
    }
    if !accept {
        tracing::info!(
            issue_workspace = %issue_workspace_key,
            "user rejected workspace merge; pinned for this session"
        );
        return;
    }

    // The move is one two-row load→modify→commit operation. Sorting inside
    // `lock_workspaces` makes opposing concurrent moves deadlock-safe.
    let workspace_guards = config
        .lock_workspaces([
            issue_workspace_key.as_str().to_string(),
            pr_workspace_key.as_str().to_string(),
        ])
        .await;

    let Some(mut pr_ws) = load_workspace(config, &pr_workspace_key) else {
        tracing::warn!(
            pr_workspace = %pr_workspace_key,
            "ConfirmMerge: PR workspace missing — aborting"
        );
        return;
    };
    let Some(issue_ws) = load_workspace(config, &issue_workspace_key) else {
        tracing::warn!(
            issue_workspace = %issue_workspace_key,
            "ConfirmMerge: issue workspace missing — aborting"
        );
        return;
    };
    // Defensive: refuse to absorb a PR workspace. The merge code
    // path is meant for ISSUE → PR collapse; if `issue_workspace_key`
    // somehow points at a PR (loose body-text parser, stale modal,
    // race), bail rather than destroy the PR row.
    if issue_ws.pr.is_some() {
        tracing::warn!(
            target_workspace = %issue_workspace_key,
            "ConfirmMerge: refusing to absorb a PR workspace into another PR"
        );
        return;
    }
    let issue_label = workspace_label_for(&issue_ws, &issue_workspace_key);
    let pr_label = workspace_label_for(&pr_ws, &pr_workspace_key);

    let moved_session_ids = absorb_issue_workspace(&mut pr_ws, issue_ws);

    // Hand off to the single merge owner: it migrates paths, commits
    // the PR (now carrying the moved sessions) BEFORE deleting the issue
    // row, and broadcasts WorkspaceRemoved → WorkspaceMerged in the
    // order the TUI's `merge_follow_from` relies on.
    commit_merge(
        config,
        pr_ws,
        vec![PendingIssueMerge {
            issue_key: issue_workspace_key,
            issue_label,
            pr_label,
            moved_session_ids,
        }],
        workspace_guards,
    )
    .await;
}

/// Manual issue→PR collapse triggered by the user. Resolves the
/// target PR locally (scanning for a workspace whose
/// `closes_issues` includes this issue's task id), then runs the
/// same absorb path the auto-prompt's "Yes" reaches.
///
/// Bypasses both `rejected_merge` (so a previously-dismissed prompt
/// becomes actionable again) and the live-session safety gate (the
/// user explicitly asked for this — the safety gate exists to
/// protect against silent absorption, not to block explicit intent).
///
/// No-op when the issue has no claiming PR in local state — there's
/// nothing to collapse into yet. The TUI's availability gate
/// (`Action::CollapseIntoPr` resolver) should keep this no-op rare.
pub async fn handle_collapse_into_pr(config: &ServerConfig, issue_workspace_key: WorkspaceKey) {
    let Some(issue_ws) = load_workspace(config, &issue_workspace_key) else {
        tracing::warn!(
            issue_workspace = %issue_workspace_key,
            "collapse_into_pr: issue workspace missing — aborting"
        );
        return;
    };
    if issue_ws.pr.is_some() {
        // Defensive: only ISSUE workspaces can be folded; refuse to
        // absorb a PR row.
        tracing::warn!(
            target_workspace = %issue_workspace_key,
            "collapse_into_pr: refusing — target is itself a PR workspace"
        );
        return;
    }
    // Find the PR workspace that closes this issue. The issue
    // workspace has at most one primary task; route through it.
    let Some(primary) = issue_ws.primary_task() else {
        return;
    };
    let Some(pr_workspace_key) = pr_workspace_claiming_issue(config, &primary.id) else {
        tracing::info!(
            issue_workspace = %issue_workspace_key,
            "collapse_into_pr: no PR workspace claims this issue — nothing to collapse"
        );
        return;
    };

    // Clear any dedupe state so the modal pipeline doesn't re-fire
    // a duplicate prompt right after this completes.
    {
        let mut prompts = config.poll.merge_prompts.lock().await;
        prompts.prompted.remove(issue_workspace_key.as_str());
        prompts.rejected.remove(issue_workspace_key.as_str());
    }

    // Reuse the confirm-accept path — same end-state, one
    // implementation. `handle_confirm_merge` re-loads workspaces
    // before mutating so the explicit-bypass path stays race-safe.
    handle_confirm_merge(config, issue_workspace_key, pr_workspace_key, true).await;
}

/// Manual "adopt": move every session out of `source_key`'s
/// workspace and into `target_key`'s, rebadging terminals so
/// wire-side traffic follows them durably (see `commit_workspace_move`).
/// Unlike the issue→PR merge, we do NOT delete the source workspace
/// — the user may still want it as a tracking row (or remove it
/// explicitly via `x x`).
///
/// No-op when either workspace is missing or `source == target`.
pub async fn handle_adopt_sessions(
    config: &ServerConfig,
    source_key: WorkspaceKey,
    target_key: WorkspaceKey,
) {
    if source_key == target_key {
        return;
    }
    let workspace_guards = config
        .lock_workspaces([
            source_key.as_str().to_string(),
            target_key.as_str().to_string(),
        ])
        .await;
    let Some(mut source_ws) = load_workspace(config, &source_key) else {
        tracing::warn!(
            source_workspace = %source_key,
            "AdoptSessions: source workspace missing — aborting"
        );
        return;
    };
    let Some(mut target_ws) = load_workspace(config, &target_key) else {
        tracing::warn!(
            target_workspace = %target_key,
            "AdoptSessions: target workspace missing — aborting"
        );
        return;
    };
    if source_ws.sessions.is_empty() {
        tracing::info!(
            source_workspace = %source_key,
            "AdoptSessions: source has no sessions — nothing to move"
        );
        return;
    }

    // Adopt used to drain only sessions, dropping the source's activity
    // and every user-owned field (issue #554). Route through the same two
    // carriers the issue→PR merge uses so the flows can't diverge. The
    // source row survives here (it may stay as a tracking row), so this is
    // a non-destructive read — both sides keep their notes/snippets.
    target_ws.absorb_activity_from(&source_ws);
    target_ws.absorb_user_state_from(&source_ws);

    let source_session_key: lazybox_core::SessionKey = (&source_key).into();
    let target_session_key: lazybox_core::SessionKey = (&target_key).into();
    let moved = source_ws.sessions.len();
    for mut session in source_ws.sessions.drain(..) {
        session.workspace_key = target_key.clone();
        target_ws.add_session(session);
    }
    crate::spawn_handler::migrate_session_paths_if_needed(&mut target_ws).await;

    tracing::info!(
        source_workspace = %source_key,
        target_workspace = %target_key,
        moved,
        "adopted sessions across workspaces"
    );

    let source_key_owned = source_ws.key.clone();
    let target_key_owned = target_ws.key.clone();
    if let Err(error) = commit_workspace_move(
        config,
        vec![(source_key_owned, source_ws), (target_key_owned, target_ws)],
        Vec::new(),
        vec![(source_session_key, target_session_key)],
        Vec::new(),
        workspace_guards,
    )
    .await
    {
        report_commit_error(config, "adopt sessions across workspaces", &error);
    }
}

/// Move the one daemon-provisioned session that owns an exact PR head
/// worktree onto the PR workspace.
///
/// This is the automatic counterpart of `Shift-A`, used only after spawn
/// resolution finds the PR's exact branch already checked out by Lazybox.
/// Both workspaces and live terminal metadata commit as one transaction, so a
/// reload can never observe the session missing from both rows or still
/// routed under the obsolete workspace key.
pub(crate) async fn transfer_owned_worktree_session(
    config: &ServerConfig,
    source_key: &WorkspaceKey,
    target_key: &WorkspaceKey,
    session_id: lazybox_core::SessionId,
    expected_path: &std::path::Path,
    expected_branch: &str,
) -> Result<Option<lazybox_core::WorkspaceSession>, CommitError> {
    if source_key == target_key {
        return Ok(None);
    }
    let workspace_guards = config
        .lock_workspaces([
            source_key.as_str().to_string(),
            target_key.as_str().to_string(),
        ])
        .await;
    let (Some(mut source_ws), Some(mut target_ws)) = (
        load_workspace(config, source_key),
        load_workspace(config, target_key),
    ) else {
        return Ok(None);
    };
    if !target_ws.sessions.is_empty() || source_ws.sessions.len() != 1 {
        return Ok(None);
    }
    let Some(source_session) = source_ws.sessions.first() else {
        return Ok(None);
    };
    if source_session.id != session_id
        || source_session.worktree_branch.as_deref() != Some(expected_branch)
        || !crate::spawn_handler::session_paths_match(&source_session.worktree_path, expected_path)
    {
        return Ok(None);
    }
    // `TerminalsRebadged` is intentionally workspace-scoped. Prove that
    // every terminal currently wearing the source badge belongs to this
    // one persisted session; a shared-main terminal or an unknown/sibling
    // owner would otherwise be dragged onto the PR as collateral.
    let source_session_key: lazybox_core::SessionKey = source_key.into();
    let entries = config.terminal.entries.lock().await;
    if entries
        .iter()
        .filter(|(_, entry)| {
            !entry.finishing
                && entry
                    .meta
                    .as_ref()
                    .is_some_and(|(owner, _)| owner == &source_session_key)
        })
        .any(|(_, entry)| entry.on_main || entry.session_id != Some(session_id))
    {
        return Ok(None);
    }
    drop(entries);

    target_ws.absorb_activity_from(&source_ws);
    target_ws.absorb_user_state_from(&source_ws);
    let mut session = source_ws.sessions.remove(0);
    session.workspace_key = target_key.clone();
    target_ws.add_session(session.clone());

    let target_session_key: lazybox_core::SessionKey = target_key.into();
    commit_workspace_move(
        config,
        vec![
            (source_ws.key.clone(), source_ws),
            (target_ws.key.clone(), target_ws),
        ],
        Vec::new(),
        vec![(source_session_key, target_session_key)],
        Vec::new(),
        workspace_guards,
    )
    .await?;
    Ok(Some(session))
}

/// Move `issue_ws`'s sessions and linked tasks onto `pr_workspace`,
/// returning the moved session ids.
/// Terminal metadata is planned and committed later by `commit_merge`, in
/// the same transaction as these workspace rows. Caller is responsible
/// for deleting the issue workspace from the store and broadcasting
/// the `WorkspaceRemoved` / `WorkspaceUpserted` / `WorkspaceMerged`
/// events around the call.
fn absorb_issue_workspace(
    pr_workspace: &mut Workspace,
    issue_ws: Workspace,
) -> Vec<lazybox_core::SessionId> {
    // Carry the issue's comment history AND its read/seen state onto
    // the PR before its row is deleted — without this the collapse
    // silently dropped both (docs/resiliency-review.md). Runs before
    // `attach_task` below so the read marks are established first;
    // the tasks' `recent_activity` re-merge is then a no-op
    // content-wise and `merge_activity` preserves the marks.
    pr_workspace.absorb_activity_from(&issue_ws);
    // Carry every user-owned field (snippets, notes, snooze, arms,
    // policies, cleanup answer, track-main, last-viewed) onto the PR
    // before the issue row is deleted (issue #554). One merge routine,
    // shared with adopt, so the two flows can't diverge.
    pr_workspace.absorb_user_state_from(&issue_ws);
    let mut moved = Vec::with_capacity(issue_ws.sessions.len());
    for mut session in issue_ws.sessions {
        session.workspace_key = pr_workspace.key.clone();
        moved.push(session.id);
        pr_workspace.add_session(session);
    }
    for issue_task in &issue_ws.gh_issues {
        pr_workspace.attach_task(issue_task.clone());
    }
    for issue_task in &issue_ws.linear_issues {
        pr_workspace.attach_task(issue_task.clone());
    }
    moved
}

/// Synthesize the workspace key an issue TaskId would have produced
/// when first upserted as a standalone workspace.
fn issue_id_to_workspace_key(issue_id: &lazybox_core::TaskId) -> WorkspaceKey {
    let stub = Task {
        author: String::new(),
        id: issue_id.clone(),
        title: String::new(),
        body: None,
        state: lazybox_core::TaskState::Open,
        role: lazybox_core::TaskRole::Author,
        ci: lazybox_core::CiStatus::None,
        review: lazybox_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: String::new(),
        repo: None,
        branch: None,
        base_branch: None,
        updated_at: Utc::now(),
        created_at: None,
        closed_at: None,
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
    };
    WorkspaceKey::new(lazybox_core::workspace_key_for(&stub))
}

/// Replace the current GitHub focus hint and wake the poll loop when
/// the focused workspace changes.
pub async fn set_focused_workspace(config: &ServerConfig, key: &WorkspaceKey) {
    // Off the serve loop: this runs for EVERY FocusWorkspace — once per
    // sidebar keystroke — and `load_workspace` is a synchronous sqlite
    // read against a store with a 5s busy timeout. Inline, a
    // checkpointing store wedged the single-task serve loop (and with
    // it every keystroke Write and Spawn) behind disk IO.
    let repo = {
        let config_owned = config.clone();
        let key_owned = key.clone();
        tokio::task::spawn_blocking(move || load_workspace(&config_owned, &key_owned))
            .await
            .ok()
            .flatten()
    }
    .and_then(|workspace| {
        workspace
            .primary_task()
            .filter(|task| task.id.source == lazybox_gh::SOURCE)
            .and_then(|task| task.repo.clone())
    })
    .map(|repo| repo.trim().to_string())
    .filter(|repo| !repo.is_empty());
    let focused_workspace = repo.as_ref().map(|_| key.as_str().to_string());
    let changed = config
        .poll
        .engagement
        .write()
        .set_focused_workspace(focused_workspace);

    // Best-effort round-robin focus hint — NEVER block here.
    //
    // This runs INLINE on the daemon serve loop for every
    // `FocusWorkspace` / `MarkRead`, i.e. on every sidebar navigation.
    // A poll tick now checks `poll_state` OUT for its duration rather
    // than holding the guard across the whole cycle (#133): the tick
    // only holds it for the sub-millisecond `mem::take` / restore in
    // `checkout_poll_state` / `restore_poll_state`, running the fetch +
    // upsert on an owned copy. So this `try_lock` usually succeeds even
    // mid-sync. We still use `try_lock` rather than `.lock().await`,
    // though: `handle_fetch_pr_details` also acquires `poll_state` (and
    // may build a client under it), and a blocking acquire here would
    // wedge the single-task serve loop behind ANY holder, queueing every
    // keystroke `Write` and `Spawn` — the "can't type in the agent while
    // GitHub syncs" regression. The hint only steers WHICH repo the NEXT
    // poll prioritizes, so skipping it under contention costs nothing.
    match config.poll.tick_state.try_lock() {
        Ok(mut state) => {
            let prev = std::mem::replace(&mut state.round_robin.focused_repo, repo.clone());
            if prev != repo {
                tracing::debug!(
                    workspace_key = %key.as_str(),
                    repo = repo.as_deref().unwrap_or(""),
                    "round-robin focus updated"
                );
            }
        }
        Err(_) => {
            tracing::debug!(
                workspace_key = %key.as_str(),
                repo,
                "poll tick holds poll_state — skipping round-robin focus hint to keep \
                 the serve loop responsive (keystrokes/spawns must not wait on the sync)"
            );
        }
    }
    if changed {
        tracing::info!(
            workspace_key = %key.as_str(),
            github = repo.is_some(),
            "polling focus changed — scheduling debounced targeted refresh"
        );
        config.poll.wake_for_focus_debounced();
    }
}

/// `owner/repo#N` for PR / issue rows; falls back to the workspace
/// key string otherwise. Used in the confirm modal + footer notice.
fn workspace_label_for(workspace: &Workspace, key: &WorkspaceKey) -> String {
    workspace
        .primary_task()
        .map(|t| t.id.key.clone())
        .unwrap_or_else(|| key.as_str().to_string())
}

#[cfg(test)]
mod empty_sources_tests {
    use super::*;

    /// D1 regression (2026-08-19 audit): providers ENABLED but no
    /// source built (credential timeout, cadence skip, init failure)
    /// must never rescope — the old path fabricated a "successful,
    /// exhaustive, empty" outcome that reached the final delete gate.
    #[test]
    fn enabled_providers_with_no_sources_never_authorize_a_rescope() {
        assert!(empty_sources_rescope_outcome(true).is_none());
    }

    /// All providers deliberately disabled: the legacy full-authority
    /// empty rescope stays, so stale rows actually disappear.
    #[test]
    fn all_providers_disabled_keeps_the_deliberate_empty_rescope() {
        let outcome = empty_sources_rescope_outcome(false).expect("deliberate empty view");
        assert!(outcome.any_source_succeeded);
        assert!(outcome.all_full);
        assert!(outcome.polled.is_empty());
    }
}

#[cfg(test)]
mod workspace_lock_tests {
    use super::*;
    use lazybox_core::{TaskId, TaskRole, TaskState};
    use lazybox_store::{MemoryStore, Store, StoreError, StoreMutation};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Store wrapper that parks the FIRST `workspace:*` read until the
    /// test releases it — a deterministic way to hold a poll tick
    /// inside its load→modify→commit window while a user mutation
    /// races it.
    struct GateStore {
        inner: MemoryStore,
        armed: AtomicBool,
        entered_tx: parking_lot::Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release_rx: parking_lot::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl Store for GateStore {
        fn apply_batch(&self, mutations: &[StoreMutation]) -> Result<(), StoreError> {
            self.inner.apply_batch(mutations)
        }

        fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
            // Read FIRST, then park: the racing tick must walk away
            // holding the PRE-mark copy (the load already happened)
            // while the concurrent mark lands — that's the lost-update
            // window. Parking before the read would hand the tick the
            // post-mark value and mask the bug.
            let result = self.inner.get_kv(key);
            if key.starts_with("workspace:") && self.armed.swap(false, Ordering::SeqCst) {
                if let Some(tx) = self.entered_tx.lock().take() {
                    let _ = tx.send(());
                }
                if let Some(rx) = self.release_rx.lock().take() {
                    // Blocking a worker thread is fine: the test runs
                    // on a multi-thread runtime with spare workers.
                    let _ = rx.recv_timeout(std::time::Duration::from_secs(10));
                }
            }
            result
        }

        fn set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
            self.inner.set_kv(key, value)
        }

        fn delete_kv(&self, key: &str) -> Result<(), StoreError> {
            self.inner.delete_kv(key)
        }

        fn list_workspaces(&self) -> Result<Vec<lazybox_store::WorkspaceRecord>, StoreError> {
            self.inner.list_workspaces()
        }

        fn list_projects(&self) -> Result<Vec<lazybox_store::ProjectRecord>, StoreError> {
            self.inner.list_projects()
        }
    }

    fn open_pr_task() -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()),
            branch: Some("feat".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
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

    /// Regression for the lost-update race: a poll tick's
    /// load→modify→commit (`upsert_into_workspace_key`) interleaving
    /// with a user's `mark_workspace_read` used to save the tick's
    /// pre-mark copy AFTER the mark landed, silently reverting it.
    ///
    /// The gate parks the tick inside `prepare_upsert`'s workspace
    /// load; the mark fires while the tick is parked. With per-key
    /// serialization the mark queues behind the tick's guard and
    /// applies AFTER its commit, so the stored row keeps
    /// `last_viewed_at`. Without the lock, the mark runs inside the
    /// window and the tick's commit erases it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tick_merge_cannot_revert_concurrent_mark_read() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let store = Arc::new(GateStore {
            inner: MemoryStore::new(),
            armed: AtomicBool::new(false),
            entered_tx: parking_lot::Mutex::new(Some(entered_tx)),
            release_rx: parking_lot::Mutex::new(Some(release_rx)),
        });
        let config = ServerConfig::with_store(store.clone());

        // Seed the workspace (gate not armed yet).
        let task = open_pr_task();
        let ws = Workspace::from_task(task.clone(), Utc::now());
        let key = ws.key.clone();
        store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).expect("serialize")),
            })
            .expect("seed");
        assert!(
            load_workspace(&config, &key)
                .expect("seeded")
                .last_viewed_at
                .is_none(),
            "fixture starts unviewed"
        );

        // Arm the gate, then start the tick: it parks inside its
        // workspace load, mid load→modify→commit.
        store.armed.store(true, Ordering::SeqCst);
        let tick_config = config.clone();
        let tick_key = key.clone();
        let tick = tokio::spawn(async move {
            upsert_into_workspace_key(&tick_config, &tick_key, task).await;
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("tick must reach its workspace load");

        // User marks the workspace read while the tick is parked.
        let mark_config = config.clone();
        let mark_key = key.clone();
        let mark = tokio::spawn(async move {
            crate::workspace::mark_workspace_read(&mark_config, &mark_key).await;
        });
        // Give the mark task time to reach the workspace lock before
        // releasing the tick — the exact interleaving the bug needs.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        release_tx.send(()).expect("release the parked tick");

        tick.await.expect("tick task");
        mark.await.expect("mark task");

        let stored = load_workspace(&config, &key).expect("workspace persisted");
        assert!(
            stored.last_viewed_at.is_some(),
            "the tick's commit must not revert a concurrent mark-read"
        );
    }

    /// Lazy PR details used to bypass the workspace lock. Parking its fresh
    /// load while a mark-read committed reproduced a last-writer-wins loss:
    /// the details commit saved its pre-mark copy over the user's action.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pr_details_cannot_revert_concurrent_mark_read() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let store = Arc::new(GateStore {
            inner: MemoryStore::new(),
            armed: AtomicBool::new(false),
            entered_tx: parking_lot::Mutex::new(Some(entered_tx)),
            release_rx: parking_lot::Mutex::new(Some(release_rx)),
        });
        let config = ServerConfig::with_store(store.clone());
        let workspace = Workspace::from_task(open_pr_task(), Utc::now());
        let key = workspace.key.clone();
        store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: workspace.created_at,
                workspace_json: Some(serde_json::to_string(&workspace).expect("serialize")),
            })
            .expect("seed");

        let details = lazybox_gh::PrDetails {
            activities: Vec::new(),
            closes_issues: Vec::new(),
            checks: Vec::new(),
            ci: lazybox_core::CiStatus::Success,
            review: lazybox_core::ReviewStatus::Approved,
            reviews: Vec::new(),
            role: TaskRole::Author,
            needs_reply: false,
            last_commenter: None,
        };
        store.armed.store(true, Ordering::SeqCst);
        let details_config = config.clone();
        let details_key = key.clone();
        let details_task = tokio::spawn(async move {
            handlers::apply_pr_details(&details_config, &details_key, details).await;
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("details apply must reach its workspace load");

        let mark_config = config.clone();
        let mark_key = key.clone();
        let mark_task = tokio::spawn(async move {
            crate::workspace::mark_workspace_read(&mark_config, &mark_key).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        release_tx.send(()).expect("release details apply");
        details_task.await.expect("details task");
        mark_task.await.expect("mark task");

        let stored = load_workspace(&config, &key).expect("workspace persisted");
        assert!(
            stored.last_viewed_at.is_some(),
            "details commit must preserve the concurrent mark-read"
        );
        assert_eq!(
            stored.pr.as_ref().expect("PR").ci,
            lazybox_core::CiStatus::Success,
            "the details update itself must also persist"
        );
    }
}

#[cfg(test)]
mod github_scope_config_tests {
    use super::*;

    /// The scope set the clone-target recovery matches against is the
    /// union of the wizard selection and the `providers.github.filters`
    /// block, so a repo scoped either way resolves — and a hyphenated
    /// owner recovers losslessly off that merged set. Regression for #326.
    #[test]
    fn github_scopes_from_config_unions_setup_and_filters() {
        let mut cfg = lazybox_config::Config::default();
        cfg.setup.scopes.insert(
            "github".into(),
            ["github:codefly-dev/warden-platform".to_string()]
                .into_iter()
                .collect(),
        );
        cfg.providers.github.filters = vec![lazybox_config::Filter {
            org: None,
            repo: Some("acme/widget".into()),
            watch: None,
        }];

        let scopes = github_scopes_from_config(&cfg);
        assert!(scopes.contains("github:codefly-dev/warden-platform"));
        assert!(scopes.contains("github:acme/widget"));

        let key = lazybox_core::ProjectKey::github("codefly-dev", "warden-platform");
        assert_eq!(
            key.github_slug_from_scopes(scopes.iter().map(String::as_str)),
            Some("codefly-dev/warden-platform".to_string()),
        );
    }
}

#[cfg(test)]
mod merge_detection_tests {
    use super::*;
    use lazybox_core::{TaskId, TaskRole, TaskState};

    fn task(source: &str, key: &str, url: &str, state: TaskState) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: source.into(),
                key: key.into(),
            },
            title: "t".into(),
            body: None,
            state,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: url.into(),
            repo: Some("o/r".into()),
            branch: Some("feat".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
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

    fn pr(key: &str, state: TaskState) -> Task {
        task("github", key, "https://github.com/o/r/pull/7", state)
    }

    #[test]
    fn pr_number_parses_trailing_id_segment() {
        assert_eq!(task_number(&pr("o/r#7", TaskState::Merged)), Some(7));
    }

    #[test]
    fn pr_number_none_when_no_hash_number() {
        let mut t = pr("o/r#7", TaskState::Merged);
        t.id.key = "ENG-42".into();
        assert_eq!(task_number(&t), None);
    }

    #[test]
    fn fresh_merge_with_open_predecessor_fires() {
        let prev = Workspace::from_task(pr("o/r#7", TaskState::Open), Utc::now());
        let incoming = pr("o/r#7", TaskState::Merged);
        assert_eq!(merged_transition_pr_number(Some(&prev), &incoming), Some(7));
    }

    #[test]
    fn merge_without_predecessor_does_not_fire() {
        // First time we ever see the PR and it's already merged — no
        // prior workspace, so no sessions to reap. Skip rather than
        // burn an inspect sweep on nothing.
        let incoming = pr("o/r#7", TaskState::Merged);
        assert_eq!(merged_transition_pr_number(None, &incoming), None);
    }

    #[test]
    fn merge_with_predecessor_lacking_the_task_does_not_fire() {
        // Predecessor workspace exists but never had this PR attached
        // (e.g. an issue-only row): no prior state to flip from.
        let prev = Workspace::empty(WorkspaceKey::new("o-r-7"), "feat", Utc::now());
        let incoming = pr("o/r#7", TaskState::Merged);
        assert_eq!(merged_transition_pr_number(Some(&prev), &incoming), None);
    }

    #[test]
    fn already_merged_predecessor_does_not_refire() {
        let prev = Workspace::from_task(pr("o/r#7", TaskState::Merged), Utc::now());
        let incoming = pr("o/r#7", TaskState::Merged);
        assert_eq!(merged_transition_pr_number(Some(&prev), &incoming), None);
    }

    #[test]
    fn non_merged_state_never_fires() {
        let incoming = pr("o/r#7", TaskState::Open);
        assert_eq!(merged_transition_pr_number(None, &incoming), None);
    }

    #[test]
    fn issue_task_never_fires() {
        // An issue (not a PR) flipping to a closed/merged-like state
        // must not trip PR cleanup, even though issues carry numbers.
        let issue = task(
            "github",
            "o/r#7",
            "https://github.com/o/r/issues/7",
            TaskState::Merged,
        );
        assert_eq!(merged_transition_pr_number(None, &issue), None);
    }

    fn issue(key: &str, state: TaskState) -> Task {
        task("github", key, "https://github.com/o/r/issues/7", state)
    }

    #[test]
    fn fresh_issue_close_with_open_predecessor_fires() {
        let prev = Workspace::from_task(issue("o/r#7", TaskState::Open), Utc::now());
        let incoming = issue("o/r#7", TaskState::Closed);
        assert_eq!(closed_issue_transition(Some(&prev), &incoming), Some(7));
    }

    #[test]
    fn issue_close_without_predecessor_does_not_fire() {
        // Never tracked → no session/worktree to clean; prompting would
        // ask about nothing.
        let incoming = issue("o/r#7", TaskState::Closed);
        assert_eq!(closed_issue_transition(None, &incoming), None);
    }

    #[test]
    fn already_closed_issue_predecessor_does_not_refire() {
        let prev = Workspace::from_task(issue("o/r#7", TaskState::Closed), Utc::now());
        let incoming = issue("o/r#7", TaskState::Closed);
        assert_eq!(closed_issue_transition(Some(&prev), &incoming), None);
    }

    #[test]
    fn open_issue_never_fires_close_cleanup() {
        let prev = Workspace::from_task(issue("o/r#7", TaskState::Open), Utc::now());
        let incoming = issue("o/r#7", TaskState::Open);
        assert_eq!(closed_issue_transition(Some(&prev), &incoming), None);
    }

    #[test]
    fn closed_pr_does_not_trip_issue_cleanup() {
        // A PR (not an issue) reaching Closed must go through the PR
        // path, never `closed_issue_transition`.
        let prev = Workspace::from_task(pr("o/r#7", TaskState::Open), Utc::now());
        let incoming = pr("o/r#7", TaskState::Closed);
        assert_eq!(closed_issue_transition(Some(&prev), &incoming), None);
    }

    #[test]
    fn issue_reopen_from_closed_predecessor_fires() {
        let prev = Workspace::from_task(issue("o/r#7", TaskState::Closed), Utc::now());
        let incoming = issue("o/r#7", TaskState::Open);
        assert!(issue_reopened(Some(&prev), &incoming));
    }

    #[test]
    fn issue_open_without_closed_predecessor_is_not_a_reopen() {
        // Freshly-discovered open issue, or one that was already open —
        // no removal was ever pending, so nothing to cancel.
        let incoming = issue("o/r#7", TaskState::Open);
        assert!(!issue_reopened(None, &incoming));
        let prev = Workspace::from_task(issue("o/r#7", TaskState::Open), Utc::now());
        assert!(!issue_reopened(Some(&prev), &incoming));
    }

    #[test]
    fn reopened_pr_does_not_trip_issue_reopen() {
        let prev = Workspace::from_task(pr("o/r#7", TaskState::Closed), Utc::now());
        let incoming = pr("o/r#7", TaskState::Open);
        assert!(!issue_reopened(Some(&prev), &incoming));
    }

    fn pr_on_branch(branch: &str) -> Task {
        let mut t = pr("o/r#99", TaskState::Open);
        t.branch = Some(branch.into());
        t
    }

    #[test]
    fn issue_id_from_branch_reads_slug_suffixed_stem() {
        // The default (empty) branch prefix plus the #109 title slug: the
        // fallback must still recover the issue number from the stem.
        let id = issue_id_from_branch(&pr_on_branch("issue-42-fix-the-thing"))
            .expect("issue number in slug-suffixed branch");
        assert_eq!(id.source, "github");
        assert_eq!(id.key, "o/r#42");
    }

    #[test]
    fn issue_id_from_branch_reads_prefixed_stem() {
        // A non-empty prefix (`worktree.branch_prefix`, possibly
        // multi-segment) sits ahead of the stem and must be ignored.
        let id = issue_id_from_branch(&pr_on_branch("team/feat/issue-7-do-it"))
            .expect("issue number behind a multi-segment prefix");
        assert_eq!(id.key, "o/r#7");
    }

    #[test]
    fn issue_id_from_branch_reads_bare_number_stem() {
        // Empty title slug → the stem is just `issue-<n>`.
        let id = issue_id_from_branch(&pr_on_branch("issue-5")).expect("bare issue stem");
        assert_eq!(id.key, "o/r#5");
    }

    #[test]
    fn issue_id_from_branch_ignores_non_issue_branches() {
        assert!(issue_id_from_branch(&pr_on_branch("linear-eng-456-ship")).is_none());
        assert!(issue_id_from_branch(&pr_on_branch("issue-fix-thing")).is_none());
        assert!(issue_id_from_branch(&pr_on_branch("feat")).is_none());
    }

    #[test]
    fn issue_id_from_branch_ignores_non_github_sources() {
        // The `issue-<n>` stem is a GitHub spawn convention; a non-GitHub
        // PR on such a branch must not be rebuilt into a `<repo>#<n>` key.
        let mut t = pr_on_branch("issue-5");
        t.id.source = "linear".into();
        assert!(issue_id_from_branch(&t).is_none());
    }

    #[test]
    fn closing_issue_workspace_keys_includes_branch_fallback() {
        // With no `closes_issues`, the branch-derived candidate is the
        // only thing linking the PR to its issue workspace.
        let pr = pr_on_branch("issue-42-fix-the-thing");
        let keys = closing_issue_workspace_keys(&pr);
        let expected = issue_id_to_workspace_key(&TaskId {
            source: "github".into(),
            key: "o/r#42".into(),
        });
        assert!(
            keys.contains(&expected),
            "branch fallback must surface the issue workspace key, got {keys:?}"
        );
    }

    fn branch_issue_key() -> WorkspaceKey {
        issue_id_to_workspace_key(&TaskId {
            source: "github".into(),
            key: "o/r#42".into(),
        })
    }

    #[test]
    fn closing_issue_workspace_keys_branch_fallback_survives_body_closes() {
        // Timing gap: body says `Closes #42` but GitHub hasn't resolved it
        // into `closingIssuesReferences` yet — the branch fallback still
        // fires because a closing keyword is not a non-closing one.
        let mut pr = pr_on_branch("issue-42-fix-the-thing");
        pr.body = Some("Closes #42.\n\nSome work.".into());
        assert!(closing_issue_workspace_keys(&pr).contains(&branch_issue_key()));
    }

    #[test]
    fn closing_issue_workspace_keys_branch_fallback_suppressed_by_part_of() {
        // #581: an explicit non-closing reference to the branch issue must
        // win over the weak branch-name heuristic.
        let mut pr = pr_on_branch("issue-42-fix-the-thing");
        pr.body = Some("Part of #42 — deliberately not Closes.".into());
        assert!(
            !closing_issue_workspace_keys(&pr).contains(&branch_issue_key()),
            "Part of #42 must suppress the branch-name collapse"
        );
    }

    #[test]
    fn closing_issue_workspace_keys_branch_fallback_suppressed_when_closes_populated() {
        // Once GitHub resolves any closing reference, `closes_issues` is
        // authoritative and the branch stem is ignored.
        let mut pr = pr_on_branch("issue-42-fix-the-thing");
        pr.closes_issues = vec![TaskId {
            source: "github".into(),
            key: "o/r#7".into(),
        }];
        let keys = closing_issue_workspace_keys(&pr);
        assert!(!keys.contains(&branch_issue_key()));
        assert!(keys.contains(&issue_id_to_workspace_key(&TaskId {
            source: "github".into(),
            key: "o/r#7".into(),
        })));
    }

    #[test]
    fn body_references_issue_non_closing_respects_number_boundary() {
        let issue_42 = TaskId {
            source: "github".into(),
            key: "o/r#42".into(),
        };
        let mut pr = pr_on_branch("issue-42-fix-the-thing");
        // A `Refs #421` mention must NOT count as a non-closing ref to #42.
        pr.body = Some("Refs #421 for context.".into());
        assert!(!body_references_issue_non_closing(&pr, &issue_42));
        // Recognizes the keyword variants and `:`/whitespace separators.
        pr.body = Some("related to: #42".into());
        assert!(body_references_issue_non_closing(&pr, &issue_42));
    }
}

#[cfg(test)]
mod rescope_collapse_tests {
    use super::*;
    use lazybox_core::{
        SessionKind, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey, WorkspaceSession,
    };
    use lazybox_store::Store;
    use std::sync::Arc;

    fn gh_task(key: &str, url: &str, state: TaskState, closes: Vec<TaskId>) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: "t".into(),
            body: None,
            state,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: url.into(),
            repo: Some("o/r".into()),
            branch: Some("feat".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
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
            closes_issues: closes,
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    fn seed(store: &lazybox_store::MemoryStore, ws: &Workspace) {
        store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: ws.key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(ws).unwrap()),
            })
            .unwrap();
    }

    fn exhaustive_github_tick(polled: Vec<WorkspaceKey>) -> TickOutcome {
        let mut source_scopes = std::collections::HashMap::new();
        source_scopes.insert("github".to_string(), PolledScope::Exhaustive);
        TickOutcome {
            polled,
            any_source_succeeded: true,
            retry_after_secs: None,
            saw_unknown_mergeable: false,
            source_scopes,
            all_full: true,
        }
    }

    /// Regression for #924: switching GitHub identity re-scopes the
    /// poll to the new account, so the previous account's PRs vanish
    /// from the fetch. A **provider-derived** (`local = false`) workspace
    /// that still holds a session must survive that poll miss — the
    /// reconcile sweep keeps it out-of-scope/inactive rather than
    /// pruning it and orphaning the live tmux/agent session.
    #[tokio::test]
    async fn identity_switch_preserves_other_accounts_session_bearing_pr() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        // Account A's PR workspace with a live session (provider-derived).
        let a_pr = gh_task(
            "o/r#100",
            "https://github.com/o/r/pull/100",
            TaskState::Open,
            vec![],
        );
        let mut a_ws = Workspace::from_task(a_pr, Utc::now());
        assert!(!a_ws.local, "a provider PR workspace is local=false");
        let session = WorkspaceSession::new(
            a_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        );
        let sid = session.id;
        a_ws.add_session(session);
        let a_key = a_ws.key.clone();
        seed(&store, &a_ws);

        // Switch to account B: the poll now returns only B's PR in the
        // same repo. A's PR is absent from the (exhaustive) fetch.
        let b_pr = gh_task(
            "o/r#200",
            "https://github.com/o/r/pull/200",
            TaskState::Open,
            vec![],
        );
        let b_ws = Workspace::from_task(b_pr, Utc::now());
        let b_key = b_ws.key.clone();
        seed(&store, &b_ws);

        let outcome = exhaustive_github_tick(vec![b_key.clone()]);
        let mut state = TickState::default();
        rescope_with_state(&config, &outcome, &mut state).await;

        let after = load_workspace(&config, &a_key)
            .expect("account A's session-bearing PR workspace must survive the identity switch");
        assert!(
            after.sessions.iter().any(|s| s.id == sid),
            "the orphaned session must be preserved, not pruned"
        );
    }

    /// Regression for #924: even when the workspace record has lost its
    /// session (state desync / partial recovery), a provisioned worktree
    /// still on disk is user work — the sweep must not prune the row and
    /// orphan the directory. Session worktrees are UUID-named, so the
    /// guard finds them by branch via `git worktree list`; this test
    /// provisions a real bare clone + worktree checked out on the
    /// workspace's branch, exactly the runtime shape.
    #[tokio::test]
    async fn out_of_scope_workspace_with_worktree_on_disk_survives() {
        fn git(dir: &std::path::Path, args: &[&str]) {
            let ok = std::process::Command::new("git")
                .current_dir(dir)
                .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
                .args(args)
                .status()
                .expect("run git")
                .success();
            assert!(ok, "git {args:?} failed in {}", dir.display());
        }

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();

        // Upstream repo carrying a `feat` branch.
        let upstream = root.join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q"]);
        git(&upstream, &["config", "user.email", "t@e.st"]);
        git(&upstream, &["config", "user.name", "t"]);
        git(&upstream, &["commit", "--allow-empty", "-q", "-m", "init"]);
        git(&upstream, &["branch", "feat"]);

        // Bare clone at the manager's canonical repo path, plus a
        // UUID-named worktree checked out on `feat` under the managed
        // worktrees root — the shape the daemon provisions at runtime.
        let bare = root.join("repos").join("o").join("r.git");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(
            root,
            &[
                "clone",
                "--bare",
                "-q",
                &upstream.to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        );
        // No `remote.origin.fetch` refspec: the legacy pre-#1253 bare-
        // clone shape — the branch's own remote-tracking ref below is
        // what the probes must work off (see git-ops `unpushed`).
        git(
            &bare,
            &["fetch", "-q", "origin", "+feat:refs/remotes/origin/feat"],
        );
        let wt = root.join("worktrees").join("session-uuid");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                "feat",
                &wt.to_string_lossy(),
                "refs/remotes/origin/feat",
            ],
        );

        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store_backend_and_worktree_root(
            store.clone(),
            Arc::new(crate::backend::MockBackend::new()),
            root.to_path_buf(),
        );

        // Provider PR workspace on branch `feat`, NO session record.
        let pr = gh_task(
            "o/r#100",
            "https://github.com/o/r/pull/100",
            TaskState::Open,
            vec![],
        );
        let ws = Workspace::from_task(pr, Utc::now());
        assert!(ws.sessions.is_empty(), "seeded without a session record");
        assert_eq!(ws.branch, "feat", "workspace tracks the worktree's branch");
        let ws_key = ws.key.clone();
        seed(&store, &ws);

        // Another PR is the only thing in scope this tick.
        let other = gh_task(
            "o/r#200",
            "https://github.com/o/r/pull/200",
            TaskState::Open,
            vec![],
        );
        let other_ws = Workspace::from_task(other, Utc::now());
        let other_key = other_ws.key.clone();
        seed(&store, &other_ws);

        let outcome = exhaustive_github_tick(vec![other_key.clone()]);
        let mut state = TickState::default();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rescope_with_state(&config, &outcome, &mut state),
        )
        .await
        .expect("rescope must not deadlock while handing removal to the lifecycle owner");

        assert!(
            load_workspace(&config, &ws_key).is_some(),
            "a workspace with a worktree still on disk (found by branch) must not be pruned"
        );
    }

    /// Counterpart to the worktree-on-disk guard: a genuinely empty
    /// out-of-scope workspace (no session, no notes, no worktree on
    /// disk) is still reaped, so the guard doesn't wedge stale rows.
    #[tokio::test]
    async fn out_of_scope_workspace_without_worktree_is_reaped() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let pr = gh_task(
            "o/r#100",
            "https://github.com/o/r/pull/100",
            TaskState::Open,
            vec![],
        );
        let ws = Workspace::from_task(pr, Utc::now());
        let ws_key = ws.key.clone();
        seed(&store, &ws);

        let other = gh_task(
            "o/r#200",
            "https://github.com/o/r/pull/200",
            TaskState::Open,
            vec![],
        );
        let other_ws = Workspace::from_task(other, Utc::now());
        let other_key = other_ws.key.clone();
        seed(&store, &other_ws);

        let outcome = exhaustive_github_tick(vec![other_key.clone()]);
        let mut state = TickState::default();
        rescope_with_state(&config, &outcome, &mut state).await;

        assert!(
            load_workspace(&config, &ws_key).is_none(),
            "a session-less, worktree-less out-of-scope workspace is still reaped"
        );
    }

    /// Regression for #202: a PR merges and GitHub auto-closes its
    /// `Closes #N` issue. The closed issue drops out of the open-scope
    /// poll, so the rescope reaper sees it as out-of-scope. Because the
    /// issue's session has no live `terminal_meta` entry (the PTY
    /// exited, but the session record survives), the old reaper deleted
    /// it silently — losing the session. The reaper must instead
    /// collapse the issue into its claiming PR so the session moves
    /// across.
    #[tokio::test]
    async fn out_of_scope_issue_with_sessions_collapses_into_claiming_pr() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let issue_task = gh_task(
            "o/r#50",
            "https://github.com/o/r/issues/50",
            TaskState::Open,
            vec![],
        );
        let mut issue_ws = Workspace::from_task(issue_task.clone(), Utc::now());
        let session = WorkspaceSession::new(
            issue_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        );
        let session_id = session.id;
        issue_ws.add_session(session);
        let issue_key = issue_ws.key.clone();
        seed(&store, &issue_ws);

        let pr_task = gh_task(
            "o/r#51",
            "https://github.com/o/r/pull/51",
            TaskState::Merged,
            vec![issue_task.id.clone()],
        );
        let pr_ws = Workspace::from_task(pr_task, Utc::now());
        let pr_key = pr_ws.key.clone();
        seed(&store, &pr_ws);

        // Only the PR is in scope this tick — the auto-closed issue
        // fell out of the open-item poll.
        let outcome = exhaustive_github_tick(vec![pr_key.clone()]);
        let mut state = TickState::default();
        rescope_with_state(&config, &outcome, &mut state).await;

        // Issue row is gone (collapsed, not lingering)...
        assert!(
            load_workspace(&config, &issue_key).is_none(),
            "issue workspace should be collapsed away"
        );
        // ...and its session now lives on the PR workspace.
        let pr_after = load_workspace(&config, &pr_key).expect("PR workspace survives");
        assert!(
            pr_after.sessions.iter().any(|s| s.id == session_id),
            "session must move onto the PR workspace, not be deleted"
        );
        let moved = pr_after
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .unwrap();
        assert_eq!(
            moved.workspace_key, pr_key,
            "moved session must be re-keyed to the PR workspace"
        );
    }

    /// Counterpart: an out-of-scope workspace with sessions but NO
    /// claiming PR has nowhere to collapse into, so the reaper leaves it
    /// alone — the session's PTY has exited (absent from `terminal_meta`)
    /// but its worktree + record are still recoverable, and rescope must
    /// never silently destroy that (#136). This pins both that the
    /// collapse path is gated on a real claiming PR (the row is NOT moved
    /// onto some unrelated PR) and that the fallback is preserve, not
    /// delete.
    #[tokio::test]
    async fn out_of_scope_issue_without_claiming_pr_is_preserved() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let issue_task = gh_task(
            "o/r#50",
            "https://github.com/o/r/issues/50",
            TaskState::Open,
            vec![],
        );
        let mut issue_ws = Workspace::from_task(issue_task, Utc::now());
        let session = WorkspaceSession::new(
            issue_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        );
        issue_ws.add_session(session);
        let issue_key = issue_ws.key.clone();
        seed(&store, &issue_ws);

        // A different, unrelated PR is in scope — it does not claim the
        // issue, so there is no collapse target.
        let other_pr = gh_task(
            "o/r#99",
            "https://github.com/o/r/pull/99",
            TaskState::Open,
            vec![],
        );
        let other_ws = Workspace::from_task(other_pr, Utc::now());
        let other_key = other_ws.key.clone();
        seed(&store, &other_ws);

        let outcome = exhaustive_github_tick(vec![other_key.clone()]);
        let mut state = TickState::default();
        rescope_with_state(&config, &outcome, &mut state).await;

        let preserved =
            load_workspace(&config, &issue_key).expect("session-bearing row must be preserved");
        assert_eq!(
            preserved.sessions.len(),
            1,
            "without a claiming PR the session stays put rather than being reaped"
        );
        let other_after = load_workspace(&config, &other_key).expect("unrelated PR survives");
        assert!(
            other_after.sessions.is_empty(),
            "the session must not be collapsed onto an unrelated PR"
        );
    }

    /// Regression for #250: when an issue a PR already claims
    /// ("Closes #N") is observed Closed, the daemon must NOT emit its
    /// own cleanup prompt. The PR's merge prompt owns cleanup after the
    /// collapse; firing here too would surface a second, stale prompt
    /// for the soon-to-be-absorbed issue row — and the two prompts key
    /// on different workspaces, so the TUI's per-key dedupe can't merge
    /// them.
    #[tokio::test]
    async fn closed_issue_claimed_by_pr_does_not_emit_removal_prompt() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        // Standalone issue workspace with a session; Open so the
        // incoming Closed is a genuine transition.
        let issue_open = gh_task(
            "o/r#50",
            "https://github.com/o/r/issues/50",
            TaskState::Open,
            vec![],
        );
        let mut issue_ws = Workspace::from_task(issue_open.clone(), Utc::now());
        let session = WorkspaceSession::new(
            issue_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        );
        issue_ws.add_session(session);
        let issue_key = issue_ws.key.clone();
        seed(&store, &issue_ws);

        // A PR claims the issue via `closes_issues`.
        let pr_task = gh_task(
            "o/r#51",
            "https://github.com/o/r/pull/51",
            TaskState::Open,
            vec![issue_open.id.clone()],
        );
        seed(&store, &Workspace::from_task(pr_task, Utc::now()));

        let mut rx = config.bus.subscribe();

        // The issue is now observed Closed on its own workspace key.
        let issue_closed = gh_task(
            "o/r#50",
            "https://github.com/o/r/issues/50",
            TaskState::Closed,
            vec![],
        );
        upsert_into_workspace_key(&config, &issue_key, issue_closed).await;

        let mut saw_removable = false;
        while let Ok(evt) = rx.try_recv() {
            if matches!(evt, Event::MergedPrRemovable { .. }) {
                saw_removable = true;
            }
        }
        assert!(
            !saw_removable,
            "a PR-claimed closed issue must defer cleanup to the PR's merge prompt"
        );
    }

    /// The reprompt sweep's candidate filter: merged PR workspaces
    /// qualify with OR without sessions (issue #499); open work doesn't.
    #[tokio::test]
    async fn removal_candidate_state_matches_merged_pr_with_sessions() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let merged = gh_task(
            "o/r#7",
            "https://github.com/o/r/pull/7",
            TaskState::Merged,
            vec![],
        );
        let mut with_sessions = Workspace::from_task(merged.clone(), Utc::now());
        with_sessions.add_session(WorkspaceSession::new(
            with_sessions.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        ));
        assert_eq!(
            removal_candidate_state(&config, &with_sessions),
            Some(lazybox_ipc::RemovableTerminalState::Merged)
        );

        // Issue #499: a session-less merged PR is still a candidate — its
        // tracking row should be offered for cleanup regardless.
        let session_less = Workspace::from_task(merged.clone(), Utc::now());
        assert_eq!(
            removal_candidate_state(&config, &session_less),
            Some(lazybox_ipc::RemovableTerminalState::Merged)
        );

        // ...unless the user already answered "keep" (durable decline).
        let mut declined = Workspace::from_task(merged.clone(), Utc::now());
        declined.cleanup_prompt = lazybox_core::CleanupPrompt::Declined;
        assert_eq!(removal_candidate_state(&config, &declined), None);

        // ...and NOT while a removal is already in flight: `remove()` marks
        // `deleted_workspaces` before its slow reclaim, so the level-trigger
        // sweep must not re-nominate a workspace the single removal owner is
        // already tearing down (orphan-modal race).
        let in_flight = Workspace::from_task(merged, Utc::now());
        assert_eq!(
            removal_candidate_state(&config, &in_flight),
            Some(lazybox_ipc::RemovableTerminalState::Merged),
            "a merged PR is a candidate before any removal starts",
        );
        config
            .deleted_workspaces
            .lock()
            .insert(in_flight.key.as_str().to_string());
        assert_eq!(
            removal_candidate_state(&config, &in_flight),
            None,
            "a workspace whose removal is in flight is no longer a cleanup candidate",
        );
        // Removal failed / released the tombstone → the row survives and the
        // level trigger may offer cleanup again.
        config
            .deleted_workspaces
            .lock()
            .remove(in_flight.key.as_str());
        assert_eq!(
            removal_candidate_state(&config, &in_flight),
            Some(lazybox_ipc::RemovableTerminalState::Merged),
            "releasing the tombstone (removal failed) re-allows the prompt",
        );

        let open = gh_task(
            "o/r#8",
            "https://github.com/o/r/pull/8",
            TaskState::Open,
            vec![],
        );
        let mut open_ws = Workspace::from_task(open, Utc::now());
        open_ws.add_session(WorkspaceSession::new(
            open_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        ));
        assert_eq!(removal_candidate_state(&config, &open_ws), None);
    }

    /// Same deferral as the transition path (#250): a closed issue a
    /// PR claims via `closes_issues` is NOT a sweep candidate — the
    /// PR's own prompt owns that cleanup. Unclaimed closed issues are.
    #[tokio::test]
    async fn removal_candidate_state_defers_pr_claimed_closed_issue() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let closed_issue = gh_task(
            "o/r#50",
            "https://github.com/o/r/issues/50",
            TaskState::Closed,
            vec![],
        );
        let mut issue_ws = Workspace::from_task(closed_issue.clone(), Utc::now());
        issue_ws.add_session(WorkspaceSession::new(
            issue_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        ));
        assert_eq!(
            removal_candidate_state(&config, &issue_ws),
            Some(lazybox_ipc::RemovableTerminalState::Closed),
            "an unclaimed closed issue is a candidate"
        );

        let pr_task = gh_task(
            "o/r#51",
            "https://github.com/o/r/pull/51",
            TaskState::Open,
            vec![closed_issue.id.clone()],
        );
        seed(&store, &Workspace::from_task(pr_task, Utc::now()));
        assert_eq!(
            removal_candidate_state(&config, &issue_ws),
            None,
            "a PR-claimed closed issue defers to the PR's prompt"
        );
    }

    /// #552: a session-less closed issue is now a sweep candidate too
    /// (`prompt_merged_pr_removal_with` auto-removes it), so the
    /// level-trigger can catch a transition the daemon missed.
    #[tokio::test]
    async fn removal_candidate_state_matches_session_less_closed_issue() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let closed_issue = gh_task(
            "o/r#70",
            "https://github.com/o/r/issues/70",
            TaskState::Closed,
            vec![],
        );
        let issue_ws = Workspace::from_task(closed_issue, Utc::now());
        assert_eq!(
            removal_candidate_state(&config, &issue_ws),
            Some(lazybox_ipc::RemovableTerminalState::Closed),
        );
    }

    /// #552: a closed issue reopening while a removal prompt is
    /// outstanding cancels it — the upsert broadcasts `RemovalCancelled`
    /// and drops the reprompt throttle stamp.
    #[tokio::test]
    async fn reopened_issue_cancels_pending_removal() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let closed_issue = gh_task(
            "o/r#71",
            "https://github.com/o/r/issues/71",
            TaskState::Closed,
            vec![],
        );
        let ws = Workspace::from_task(closed_issue, Utc::now());
        let key = ws.key.clone();
        seed(&store, &ws);
        config
            .poll
            .removal_prompts
            .lock()
            .await
            .prompted
            .insert(key.as_str().to_string(), std::time::Instant::now());

        let mut rx = config.bus.subscribe();
        let reopened = gh_task(
            "o/r#71",
            "https://github.com/o/r/issues/71",
            TaskState::Open,
            vec![],
        );
        upsert_into_workspace_key(&config, &key, reopened).await;

        let mut saw_cancel = false;
        while let Ok(evt) = rx.try_recv() {
            if matches!(evt, Event::RemovalCancelled { .. }) {
                saw_cancel = true;
            }
        }
        assert!(saw_cancel, "reopen must broadcast RemovalCancelled");
        assert!(
            !config
                .poll
                .removal_prompts
                .lock()
                .await
                .prompted
                .contains_key(key.as_str()),
            "reopen must drop the reprompt throttle stamp"
        );
    }

    /// #552: reopen-cancel fires off the stored predecessor state, not
    /// the in-memory prompt stamp — so a daemon restarted with empty
    /// cadence memory (holding no stamp) still tells clients to dismiss a
    /// stale removal modal when the issue reopens.
    #[tokio::test]
    async fn reopened_issue_cancels_without_prior_stamp() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let closed_issue = gh_task(
            "o/r#72",
            "https://github.com/o/r/issues/72",
            TaskState::Closed,
            vec![],
        );
        let ws = Workspace::from_task(closed_issue, Utc::now());
        let key = ws.key.clone();
        seed(&store, &ws);
        // No removal_prompts stamp — models a fresh daemon process.

        let mut rx = config.bus.subscribe();
        let reopened = gh_task(
            "o/r#72",
            "https://github.com/o/r/issues/72",
            TaskState::Open,
            vec![],
        );
        upsert_into_workspace_key(&config, &key, reopened).await;

        let mut saw_cancel = false;
        while let Ok(evt) = rx.try_recv() {
            if matches!(evt, Event::RemovalCancelled { .. }) {
                saw_cancel = true;
            }
        }
        assert!(
            saw_cancel,
            "reopen must broadcast RemovalCancelled even with no in-memory stamp"
        );
    }

    /// Regression + stress for #136: merging a PR must preserve its
    /// session reliably, not "sometimes." A merged PR whose agent
    /// terminal has exited still owns a recoverable worktree + session
    /// record but leaves no `terminal_meta` entry. When a later full
    /// sweep drops the PR from scope (the recently-merged `is:merged`
    /// sub-query transiently failed, or round-robin didn't cover its
    /// repo this tick), the rescope reaper used to silently delete it
    /// whenever no live terminal was attached — losing the session.
    /// Repeated rescope passes must always preserve it.
    #[tokio::test]
    async fn merged_pr_session_survives_repeated_out_of_scope_rescope() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let pr_task = gh_task(
            "o/r#77",
            "https://github.com/o/r/pull/77",
            TaskState::Merged,
            vec![],
        );
        let mut pr_ws = Workspace::from_task(pr_task, Utc::now());
        let session = WorkspaceSession::new(
            pr_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        );
        let session_id = session.id;
        pr_ws.add_session(session);
        let pr_key = pr_ws.key.clone();
        seed(&store, &pr_ws);

        // Another PR is the only thing in scope this tick — the merged
        // PR fell out of the recently-merged sweep. No `terminal_meta`
        // entry: the agent finished and its PTY exited.
        let other = gh_task(
            "o/r#88",
            "https://github.com/o/r/pull/88",
            TaskState::Open,
            vec![],
        );
        let other_ws = Workspace::from_task(other, Utc::now());
        let other_key = other_ws.key.clone();
        seed(&store, &other_ws);

        let outcome = exhaustive_github_tick(vec![other_key.clone()]);
        let mut state = TickState::default();
        for pass in 0..25 {
            rescope_with_state(&config, &outcome, &mut state).await;
            let after = load_workspace(&config, &pr_key)
                .unwrap_or_else(|| panic!("merged PR session reaped on rescope pass {pass}"));
            assert!(
                after.sessions.iter().any(|s| s.id == session_id),
                "the merged PR's session must never be silently reaped (pass {pass})"
            );
        }
    }

    /// The other half of "preserve a live session across merge": a
    /// merged PR with a LIVE terminal attached takes the prompt branch,
    /// not the silent-delete branch, so it likewise survives rescope.
    #[tokio::test]
    async fn merged_pr_session_with_live_terminal_survives_rescope() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let pr_task = gh_task(
            "o/r#77",
            "https://github.com/o/r/pull/77",
            TaskState::Merged,
            vec![],
        );
        let mut pr_ws = Workspace::from_task(pr_task, Utc::now());
        let session = WorkspaceSession::new(
            pr_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        );
        let session_id = session.id;
        pr_ws.add_session(session);
        let pr_key = pr_ws.key.clone();
        seed(&store, &pr_ws);

        // Live agent: a terminal is attached to the PR's session.
        let session_key: lazybox_core::SessionKey = (&pr_key).into();
        config
            .terminal
            .insert_meta(
                lazybox_ipc::TerminalId(1),
                session_key,
                lazybox_ipc::TerminalKind::Shell,
            )
            .await;

        let other = gh_task(
            "o/r#88",
            "https://github.com/o/r/pull/88",
            TaskState::Open,
            vec![],
        );
        let other_ws = Workspace::from_task(other, Utc::now());
        let other_key = other_ws.key.clone();
        seed(&store, &other_ws);

        let outcome = exhaustive_github_tick(vec![other_key.clone()]);
        let mut state = TickState::default();
        rescope_with_state(&config, &outcome, &mut state).await;

        let after = load_workspace(&config, &pr_key)
            .expect("merged PR with a live terminal must survive rescope");
        assert!(
            after.sessions.iter().any(|s| s.id == session_id),
            "the live session must be preserved across merge"
        );
    }

    /// Regression for #64: the recently-merged sweep (`is:merged` last
    /// 7d) returns PRs the user may never have tracked. Upserting such a
    /// merged PR must NOT create a fresh workspace — doing so re-surfaces
    /// already-merged PRs into the inbox on every manual Shift-R sync.
    #[tokio::test]
    async fn merged_pr_sweep_does_not_create_new_workspace() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let pr_task = gh_task(
            "o/r#51",
            "https://github.com/o/r/pull/51",
            TaskState::Merged,
            vec![],
        );
        let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr_task));

        upsert(&config, pr_task).await;

        assert!(
            load_workspace(&config, &pr_key).is_none(),
            "a merged PR with no existing workspace must not be re-surfaced into the inbox"
        );
    }

    /// Regression for #64: when the merged sweep returns a PR that
    /// `Closes #N`, the issue-collapse pass must not fire if the PR has
    /// no existing workspace. Otherwise it folds the user's active issue
    /// workspace (sessions and all) into a brand-new merged-PR row —
    /// wiping in-progress work from the sidebar on a manual sync.
    #[tokio::test]
    async fn merged_pr_sweep_does_not_collapse_active_issue() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let issue_task = gh_task(
            "o/r#50",
            "https://github.com/o/r/issues/50",
            TaskState::Open,
            vec![],
        );
        let mut issue_ws = Workspace::from_task(issue_task.clone(), Utc::now());
        let session = WorkspaceSession::new(
            issue_ws.key.clone(),
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        );
        let session_id = session.id;
        issue_ws.add_session(session);
        let issue_key = issue_ws.key.clone();
        seed(&store, &issue_ws);

        // Merged sweep returns the closing PR, which the user never
        // tracked as its own workspace.
        let pr_task = gh_task(
            "o/r#51",
            "https://github.com/o/r/pull/51",
            TaskState::Merged,
            vec![issue_task.id.clone()],
        );
        let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr_task));

        upsert(&config, pr_task).await;

        let issue_after =
            load_workspace(&config, &issue_key).expect("active issue workspace must survive");
        assert!(
            issue_after.sessions.iter().any(|s| s.id == session_id),
            "the issue's session must not be folded into a non-existent merged PR"
        );
        assert!(
            load_workspace(&config, &pr_key).is_none(),
            "the merged PR must not be re-surfaced"
        );
    }

    /// Counterpart: a merged PR the user IS tracking (its workspace
    /// already exists) still back-fills MERGED state and collapses its
    /// closing issue — the normal open→merged flow must keep working.
    #[tokio::test]
    async fn merged_pr_with_existing_workspace_still_collapses_issue() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let issue_task = gh_task(
            "o/r#50",
            "https://github.com/o/r/issues/50",
            TaskState::Open,
            vec![],
        );
        let issue_ws = Workspace::from_task(issue_task.clone(), Utc::now());
        let issue_key = issue_ws.key.clone();
        seed(&store, &issue_ws);

        // The PR was tracked while open, so its workspace already exists.
        let open_pr = gh_task(
            "o/r#51",
            "https://github.com/o/r/pull/51",
            TaskState::Open,
            vec![issue_task.id.clone()],
        );
        let pr_ws = Workspace::from_task(open_pr, Utc::now());
        let pr_key = pr_ws.key.clone();
        seed(&store, &pr_ws);

        // Now it merges — back-fill the MERGED state onto the existing
        // workspace and fold the closing issue in.
        let merged_pr = gh_task(
            "o/r#51",
            "https://github.com/o/r/pull/51",
            TaskState::Merged,
            vec![issue_task.id.clone()],
        );
        upsert(&config, merged_pr).await;

        let pr_after = load_workspace(&config, &pr_key).expect("tracked PR workspace survives");
        assert_eq!(
            pr_after.pr.map(|p| p.state),
            Some(TaskState::Merged),
            "the existing PR workspace must back-fill the final MERGED state"
        );
        assert!(
            load_workspace(&config, &issue_key).is_none(),
            "the closing issue must collapse into the tracked PR workspace"
        );
    }

    /// `set_notes` persists the free-form local note into the workspace
    /// blob and it reloads verbatim (issue #458).
    #[tokio::test]
    async fn set_notes_persists_and_reloads() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let task = gh_task(
            "o/r#7",
            "https://github.com/o/r/pull/7",
            TaskState::Open,
            vec![],
        );
        let ws = Workspace::from_task(task, Utc::now());
        let key = ws.key.clone();
        seed(&store, &ws);

        crate::workspace::set_notes(&config, &key, "check the flaky retry".into()).await;

        let reloaded = load_workspace(&config, &key).expect("workspace survives");
        assert_eq!(reloaded.notes, "check the flaky retry");
        assert!(reloaded.has_notes());

        // Clearing to empty removes the indicator but leaves the row.
        crate::workspace::set_notes(&config, &key, String::new()).await;
        let cleared = load_workspace(&config, &key).expect("workspace survives");
        assert!(cleared.notes.is_empty());
        assert!(!cleared.has_notes());
    }

    /// `record_snippet_delivery` prepends onto the workspace's MRU,
    /// bumps the honest count, and reloads verbatim; a re-send moves the
    /// key to the front (no duplicate) yet still counts (issue #463).
    #[tokio::test]
    async fn record_snippet_delivery_persists_count_and_mru() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let task = gh_task(
            "o/r#9",
            "https://github.com/o/r/pull/9",
            TaskState::Open,
            vec![],
        );
        let ws = Workspace::from_task(task, Utc::now());
        let key = ws.key.clone();
        seed(&store, &ws);

        crate::workspace::record_snippet_delivery(&config, &key, "rev".into()).await;
        crate::workspace::record_snippet_delivery(&config, &key, "plan".into()).await;
        let reloaded = load_workspace(&config, &key).expect("workspace survives");
        assert_eq!(
            reloaded.sent_snippets.recent().to_vec(),
            vec!["plan", "rev"],
            "newest-first",
        );
        assert_eq!(reloaded.sent_snippets.total(), 2, "count persists");

        crate::workspace::record_snippet_delivery(&config, &key, "rev".into()).await;
        let reloaded = load_workspace(&config, &key).expect("workspace survives");
        assert_eq!(
            reloaded.sent_snippets.recent().to_vec(),
            vec!["rev", "plan"],
            "a re-send moves the key to the front without duplicating",
        );
        assert_eq!(
            reloaded.sent_snippets.total(),
            3,
            "but the re-send still counts",
        );
    }

    /// A poll upsert overwrites upstream-derived fields but must leave
    /// the local note intact — it's user-owned, like snooze (#458).
    #[tokio::test]
    async fn notes_survive_upsert() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let mut task = gh_task(
            "o/r#8",
            "https://github.com/o/r/pull/8",
            TaskState::Open,
            vec![],
        );
        let mut ws = Workspace::from_task(task.clone(), Utc::now());
        ws.notes = "keep me across polls".into();
        let key = ws.key.clone();
        seed(&store, &ws);

        // A later poll delivers a fresher copy of the same task.
        task.title = "renamed upstream".into();
        task.updated_at = Utc::now();
        upsert(&config, task).await;

        let after = load_workspace(&config, &key).expect("workspace survives");
        assert_eq!(after.notes, "keep me across polls");
    }
}

#[cfg(test)]
mod unreadable_row_preservation_tests {
    //! Fix for "unparseable workspace rows are silently clobbered by
    //! the next poll": startup preserves an unreadable row with a
    //! warning, but `prepare_upsert` used to lenient-parse with `.ok()`
    //! and rebuild `Workspace::from_task` fresh on failure — the next
    //! poll of the same PR then overwrote the preserved row, destroying
    //! its sessions / read-state / snooze / policies. These tests pin
    //! the new contract: present-but-unreadable rows (corrupt JSON,
    //! newer-schema stamp, failing store reads) are never overwritten,
    //! and the condition is reported on the bus (debounced).
    use super::*;
    use lazybox_core::{TaskId, TaskRole, TaskState};
    use lazybox_store::{MemoryStore, Store, StoreError, StoreMutation};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn gh_pr_task(key: &str) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{}", key.replace('#', "/pull/")),
            repo: Some("o/r".into()),
            branch: Some("feat".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
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

    /// Drain the bus and count `ProviderError { source: "storage" }`
    /// events.
    fn storage_error_count(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> usize {
        let mut n = 0;
        while let Ok(ev) = rx.try_recv() {
            if let Event::ProviderError { source, .. } = ev
                && source == "storage"
            {
                n += 1;
            }
        }
        n
    }

    /// Valid JSON, wrong shape (a "future enum variant" a lenient read
    /// chokes on), plus live-session markers the old behavior would
    /// have destroyed.
    const WRONG_SHAPE_ROW: &str = r#"{"key":"github-o-r-1","name":"n","branch":"feat",
        "pr":{"totally":"different-shape"},"gh_issues":[],"linear_issues":[],
        "activity":[],"seen_count":0,"sessions":[{"future_variant":true}],
        "created_at":"2026-01-01T00:00:00Z","last_viewed_at":null}"#;

    #[tokio::test]
    async fn upsert_never_overwrites_a_corrupt_stored_row() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());
        let task = gh_pr_task("o/r#1");
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        let kv_key = format!("workspace:{}", key.as_str());
        store.set_kv(&kv_key, WRONG_SHAPE_ROW).unwrap();

        let mut rx = config.bus.subscribe();
        upsert_into_workspace_key(&config, &key, task.clone()).await;

        assert_eq!(
            store.get_kv(&kv_key).unwrap().as_deref(),
            Some(WRONG_SHAPE_ROW),
            "the preserved-but-unreadable row must be left byte-identical"
        );
        assert_eq!(
            storage_error_count(&mut rx),
            1,
            "the skip must be reported on the bus"
        );

        // Second poll of the same key within the debounce window:
        // still no overwrite, and no report spam.
        upsert_into_workspace_key(&config, &key, task).await;
        assert_eq!(
            store.get_kv(&kv_key).unwrap().as_deref(),
            Some(WRONG_SHAPE_ROW)
        );
        assert_eq!(
            storage_error_count(&mut rx),
            0,
            "repeat reports for the same key are debounced"
        );
    }

    /// A row stamped with a NEWER schema parses fine under lenient
    /// serde but must be preserved too: rewriting it from an older
    /// build would silently drop the newer build's fields.
    #[tokio::test]
    async fn upsert_never_overwrites_a_newer_schema_row() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());
        let task = gh_pr_task("o/r#2");
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));

        let mut ws = Workspace::from_task(task.clone(), Utc::now());
        ws.notes = "state a newer build wrote".into();
        let mut row: serde_json::Value = serde_json::to_value(&ws).unwrap();
        row["schema"] = serde_json::json!(u32::MAX);
        row["field_from_the_future"] = serde_json::json!({"important": true});
        let row = serde_json::to_string(&row).unwrap();
        let kv_key = format!("workspace:{}", key.as_str());
        store.set_kv(&kv_key, &row).unwrap();

        let mut rx = config.bus.subscribe();
        upsert_into_workspace_key(&config, &key, task).await;

        assert_eq!(
            store.get_kv(&kv_key).unwrap().as_deref(),
            Some(row.as_str()),
            "a downgraded build must not rewrite (and truncate) a newer-schema row"
        );
        assert_eq!(storage_error_count(&mut rx), 1);
    }

    /// Store wrapper whose `workspace:*` reads fail while armed —
    /// SQLITE_BUSY shaped. A failing READ during the upsert must skip
    /// the write, not masquerade as "row absent" and create fresh.
    struct FailingReadStore {
        inner: MemoryStore,
        fail_reads: AtomicBool,
    }

    impl Store for FailingReadStore {
        fn apply_batch(&self, mutations: &[StoreMutation]) -> Result<(), StoreError> {
            self.inner.apply_batch(mutations)
        }
        fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
            if key.starts_with("workspace:") && self.fail_reads.load(Ordering::SeqCst) {
                return Err(StoreError::Backend("database is locked".into()));
            }
            self.inner.get_kv(key)
        }
        fn set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
            self.inner.set_kv(key, value)
        }
        fn delete_kv(&self, key: &str) -> Result<(), StoreError> {
            self.inner.delete_kv(key)
        }
        fn list_workspaces(&self) -> Result<Vec<lazybox_store::WorkspaceRecord>, StoreError> {
            self.inner.list_workspaces()
        }
        fn list_projects(&self) -> Result<Vec<lazybox_store::ProjectRecord>, StoreError> {
            self.inner.list_projects()
        }
    }

    #[tokio::test]
    async fn upsert_skips_when_the_stored_row_cannot_be_read() {
        let store = Arc::new(FailingReadStore {
            inner: MemoryStore::new(),
            fail_reads: AtomicBool::new(false),
        });
        let config = ServerConfig::with_store(store.clone());
        let task = gh_pr_task("o/r#3");
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        let ws = Workspace::from_task(task.clone(), Utc::now());
        let row = serde_json::to_string(&ws).unwrap();
        let kv_key = format!("workspace:{}", key.as_str());
        store.inner.set_kv(&kv_key, &row).unwrap();

        store.fail_reads.store(true, Ordering::SeqCst);
        upsert_into_workspace_key(&config, &key, task).await;
        store.fail_reads.store(false, Ordering::SeqCst);

        assert_eq!(
            store.inner.get_kv(&kv_key).unwrap().as_deref(),
            Some(row.as_str()),
            "a transient read failure must not let the upsert rebuild the row fresh"
        );
    }

    /// Rescope's silent-delete branch must also preserve an unreadable
    /// row: the corrupt row decodes to `None` for every guard, but the
    /// final fresh-load gate refuses to reap what it cannot read.
    #[tokio::test]
    async fn rescope_preserves_a_corrupt_out_of_scope_row() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());
        let kv_key = "workspace:github-o-r-1";
        store.set_kv(kv_key, WRONG_SHAPE_ROW).unwrap();

        // Non-empty polled set (a different key) with legacy scope
        // info: every unpolled workspace is a deletion candidate.
        let outcome = TickOutcome {
            polled: vec![WorkspaceKey::new("github-o-r-999")],
            any_source_succeeded: true,
            retry_after_secs: None,
            saw_unknown_mergeable: false,
            source_scopes: std::collections::HashMap::new(),
            all_full: true,
        };
        let mut state = TickState::default();
        rescope_with_state(&config, &outcome, &mut state).await;

        assert_eq!(
            store.get_kv(kv_key).unwrap().as_deref(),
            Some(WRONG_SHAPE_ROW),
            "rescope must preserve (not reap) a row it cannot decode"
        );
    }

    /// TOCTOU regression (issue #1385): an `x x` archive that lands AFTER
    /// the pre-lock archived-set gate but while an upsert is still in flight
    /// must not resurrect the deleted row. Modeled by seeding the archived
    /// set (as the under-lock archive would) and driving
    /// `upsert_into_workspace_key` with no stored row: the re-check under the
    /// workspace lock must skip rather than rebuild `Workspace::from_task`.
    #[tokio::test]
    async fn upsert_skips_a_workspace_archived_after_the_pre_lock_check() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());
        let task = gh_pr_task("o/r#7");
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        let kv_key = format!("workspace:{}", key.as_str());

        assert!(
            crate::workspace::archive_workspace_key(&config, key.as_str()),
            "seed the archived set"
        );

        upsert_into_workspace_key(&config, &key, task).await;

        assert_eq!(
            store.get_kv(&kv_key).unwrap(),
            None,
            "an upsert must not resurrect a row archived under the lock"
        );
    }

    /// Companion for a non-archiving delete: the `deleted_workspaces`
    /// tombstone (held across the delete's race window) must also block an
    /// in-flight upsert, since a plain delete never enters the archived set.
    #[tokio::test]
    async fn upsert_skips_a_workspace_with_a_live_delete_tombstone() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());
        let task = gh_pr_task("o/r#8");
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        let kv_key = format!("workspace:{}", key.as_str());

        config
            .deleted_workspaces
            .lock()
            .insert(key.as_str().to_string());

        upsert_into_workspace_key(&config, &key, task).await;

        assert_eq!(
            store.get_kv(&kv_key).unwrap(),
            None,
            "an upsert must not resurrect a row with a live delete tombstone"
        );
    }
}

#[cfg(test)]
mod tick_noop_skip_tests {
    //! The steady-state poll re-fetches every task each tick; when the
    //! upstream task is byte-identical to the stored workspace,
    //! `commit_upsert` must skip both the store write AND the
    //! `WorkspaceUpserted` broadcast. Untested before, this guards the
    //! short-circuit against a future volatile field silently
    //! resurrecting the every-tick write+broadcast storm.
    use super::*;
    use lazybox_core::{TaskId, TaskRole, TaskState};
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// A `TaskSource` that returns whatever tasks the test stashed in
    /// it. Defaults — `polled_scope = Repos([])`, `last_fetch_kind =
    /// Full` — keep `tick` on the non-destructive path so the test
    /// observes only the upsert broadcasts.
    struct FixtureSource {
        tasks: Mutex<Vec<Task>>,
        changed_counts: Mutex<Vec<usize>>,
    }

    struct RateLimitFixtureSource {
        succeeds: bool,
        retry_after_secs: Option<u64>,
    }

    impl FixtureSource {
        fn new(tasks: Vec<Task>) -> Self {
            Self {
                tasks: Mutex::new(tasks),
                changed_counts: Mutex::new(Vec::new()),
            }
        }
        fn set(&self, tasks: Vec<Task>) {
            *self.tasks.lock() = tasks;
        }
    }

    impl TaskSource for FixtureSource {
        fn name(&self) -> &str {
            lazybox_gh::SOURCE
        }
        fn fetch<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            let tasks = self.tasks.lock().clone();
            Box::pin(async move { Ok(tasks) })
        }

        fn record_items_changed(&self, count: usize) {
            self.changed_counts.lock().push(count);
        }
    }

    impl TaskSource for RateLimitFixtureSource {
        fn name(&self) -> &str {
            lazybox_gh::SOURCE
        }

        fn fetch<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                if self.succeeds {
                    Ok(Vec::new())
                } else {
                    Err(lazybox_core::ProviderError::retryable(
                        lazybox_gh::SOURCE,
                        "aggregate query failed without a retry hint",
                    ))
                }
            })
        }

        fn retry_after_secs(&self) -> Option<u64> {
            self.retry_after_secs
        }
    }

    fn issue(key: &str, title: &str) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: title.into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{key}").replace('#', "/issues/"),
            repo: Some("o/r".into()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Unknown,
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

    /// Drain everything currently queued on the receiver without
    /// blocking. The tick is fully awaited before we call this, so every
    /// synchronous `bus.send` it made has already landed.
    fn drain(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            out.push(evt);
        }
        out
    }

    fn upserted_keys(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::WorkspaceUpserted(ws) => Some(ws.key.as_str().to_string()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn unchanged_task_skips_write_and_broadcast() {
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());
        let source = Arc::new(FixtureSource::new(vec![issue("o/r#1", "first")]));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(FixtureSourceRef(source.clone()))];

        let key = lazybox_core::workspace_key_for(&issue("o/r#1", "first"));
        let mut rx = config.bus.subscribe();
        tick(&config, &sources).await;
        let first = drain(&mut rx);
        assert_eq!(
            upserted_keys(&first),
            vec![key],
            "first sight of a task must upsert + broadcast it"
        );

        // Re-poll the identical task. The short-circuit must skip it.
        let mut rx = config.bus.subscribe();
        tick(&config, &sources).await;
        let second = drain(&mut rx);
        assert!(
            upserted_keys(&second).is_empty(),
            "a byte-identical re-poll must not re-broadcast WorkspaceUpserted, got {:?}",
            upserted_keys(&second),
        );
        assert_eq!(
            *source.changed_counts.lock(),
            vec![1, 0],
            "the accounting hook must report durable changes, not fetched rows"
        );
    }

    #[tokio::test]
    async fn changed_task_still_broadcasts() {
        // Guard against the skip test passing for the wrong reason (e.g.
        // broadcasts silently disabled): a real field change must flow
        // through normally.
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());
        let source = Arc::new(FixtureSource::new(vec![issue("o/r#1", "first")]));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(FixtureSourceRef(source.clone()))];

        let key = lazybox_core::workspace_key_for(&issue("o/r#1", "first"));
        let mut rx = config.bus.subscribe();
        tick(&config, &sources).await;
        assert_eq!(upserted_keys(&drain(&mut rx)), vec![key.clone()]);

        // Mutate a stored field; the next poll's JSON differs.
        source.set(vec![issue("o/r#1", "retitled")]);
        let mut rx = config.bus.subscribe();
        tick(&config, &sources).await;
        assert_eq!(
            upserted_keys(&drain(&mut rx)),
            vec![key],
            "a changed task must re-broadcast"
        );
    }

    fn config_with_low_github_budget(
        reset_at: chrono::DateTime<Utc>,
    ) -> (ServerConfig, tokio::sync::broadcast::Receiver<Event>) {
        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        config.poll.cache_gh_client(
            lazybox_gh::GhClient::stub_with_rate_limit_for_tests(
                "test",
                "fingerprint",
                98,
                5000,
                reset_at,
            )
            .expect("stub GitHub client"),
        );
        let rx = config.bus.subscribe();
        (config, rx)
    }

    #[tokio::test]
    async fn cached_rate_limit_controls_an_error_without_a_retry_hint() {
        let reset_at = Utc::now() + chrono::Duration::minutes(10);
        let (config, mut rx) = config_with_low_github_budget(reset_at);
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(RateLimitFixtureSource {
            succeeds: false,
            retry_after_secs: None,
        })];

        let outcome = tick(&config, &sources).await;
        let events = drain(&mut rx);

        assert!(
            outcome.retry_after_secs.is_some_and(|secs| secs >= 598),
            "scheduler must wait for the cached reset, got {:?}",
            outcome.retry_after_secs
        );
        assert!(events.iter().any(|event| matches!(
            event,
            Event::GithubRateLimitWait {
                remaining: 98,
                limit: 5000,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::ProviderError { .. })),
            "a known rate-limit wait must not be reported as a generic failure"
        );
    }

    /// #727 + #730: a self-healing retryable transient whose specific
    /// message churns every retry cycle (a throttle with a ticking
    /// "retrying in Ns", then a 502) must collapse to a SINGLE quiet
    /// `retryable` broadcast while the daemon is still auto-retrying — not
    /// re-fire on every attempt (pre-#727 the debounce keyed on the exact
    /// message, so each varying message looked new and flickered a banner).
    /// Once retries are exhausted it escalates to exactly one `exhausted`
    /// error, still coalesced, not one per subsequent cycle.
    #[tokio::test]
    async fn churning_retryable_streak_broadcasts_one_quiet_then_one_exhausted() {
        struct ChurningRetryable {
            attempt: std::sync::atomic::AtomicU64,
        }
        impl TaskSource for ChurningRetryable {
            fn name(&self) -> &str {
                lazybox_gh::SOURCE
            }
            fn fetch<'a>(
                &'a self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                // Alternate a ticking throttle with a 502 so the
                // user-facing message is genuinely different every cycle
                // — exactly the churn the exact-message debounce missed.
                let n = self
                    .attempt
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    Err(if n.is_multiple_of(2) {
                        lazybox_core::ProviderError::retryable_after(
                            lazybox_gh::SOURCE,
                            "secondary rate limit",
                            15 + n,
                        )
                    } else {
                        lazybox_core::ProviderError::retryable(
                            lazybox_gh::SOURCE,
                            "HTTP 502 (Bad Gateway)",
                        )
                    })
                })
            }
        }

        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(ChurningRetryable {
            attempt: std::sync::atomic::AtomicU64::new(0),
        })];
        let mut state = TickState::default();
        let mut rx = config.bus.subscribe();

        // Run past the exhaustion threshold so both regimes are exercised.
        for _ in 0..(RETRYABLE_EXHAUSTION_ATTEMPTS + 2) {
            tick_with_state(&config, &sources, &mut state).await;
        }

        let events = drain(&mut rx);
        let count = |want: &str| {
            events
                .iter()
                .filter(|event| matches!(event, Event::ProviderError { kind, .. } if kind == want))
                .count()
        };
        assert_eq!(
            count("retryable"),
            1,
            "the still-retrying streak must broadcast one quiet status, not one per retry"
        );
        assert_eq!(
            count("exhausted"),
            1,
            "exhausted retries escalate to exactly one actionable error, not one per cycle"
        );
    }

    /// #730: a retryable transient must stay a quiet `retryable` status
    /// while the daemon is still auto-retrying and only escalate to an
    /// actionable `exhausted` error once its retries are exhausted. A
    /// recovery before the threshold resets the streak, so an intermittent
    /// hiccup that heals every couple of cycles never escalates.
    #[tokio::test]
    async fn retryable_escalates_to_exhausted_only_after_retries_run_out() {
        #[derive(Clone, Copy, PartialEq)]
        enum Mode {
            Retryable,
            Ok,
        }
        struct ModedSource {
            mode: Arc<Mutex<Mode>>,
        }
        impl TaskSource for ModedSource {
            fn name(&self) -> &str {
                lazybox_gh::SOURCE
            }
            fn fetch<'a>(
                &'a self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                let mode = *self.mode.lock();
                Box::pin(async move {
                    match mode {
                        Mode::Retryable => Err(lazybox_core::ProviderError::retryable(
                            lazybox_gh::SOURCE,
                            "transient hiccup",
                        )),
                        Mode::Ok => Ok(Vec::new()),
                    }
                })
            }
        }

        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        let mode = Arc::new(Mutex::new(Mode::Retryable));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(ModedSource { mode: mode.clone() })];
        let mut state = TickState::default();
        let mut rx = config.bus.subscribe();
        let count = |events: Vec<Event>, want: &str| {
            events
                .into_iter()
                .filter(|event| matches!(event, Event::ProviderError { kind, .. } if kind == want))
                .count()
        };

        // A hiccup that heals just short of exhaustion never escalates:
        // the recovery resets the streak.
        for _ in 0..(RETRYABLE_EXHAUSTION_ATTEMPTS - 1) {
            tick_with_state(&config, &sources, &mut state).await;
        }
        *mode.lock() = Mode::Ok;
        tick_with_state(&config, &sources, &mut state).await;
        let healed = drain(&mut rx);
        assert_eq!(
            count(healed.clone(), "retryable"),
            1,
            "the pre-recovery streak surfaces one quiet status"
        );
        assert_eq!(
            count(healed, "exhausted"),
            0,
            "a transient that heals before exhaustion must never escalate"
        );

        // Now a streak that actually persists to exhaustion escalates.
        *mode.lock() = Mode::Retryable;
        for _ in 0..RETRYABLE_EXHAUSTION_ATTEMPTS {
            tick_with_state(&config, &sources, &mut state).await;
        }
        assert_eq!(
            count(drain(&mut rx), "exhausted"),
            1,
            "a persisting transient escalates to one actionable error"
        );
    }

    /// #772: a pure throttle (a rate limit carrying a backoff window) must
    /// NEVER escalate to an actionable `exhausted` error, however long the
    /// throttle persists. The daemon is deliberately backing off a working
    /// token and connection; "check your connection or token" is a
    /// dead-end the user can't act on. It stays one quiet `retryable`
    /// status the whole time.
    #[tokio::test]
    async fn pure_throttle_never_escalates_to_exhausted() {
        struct AlwaysThrottled;
        impl TaskSource for AlwaysThrottled {
            fn name(&self) -> &str {
                lazybox_gh::SOURCE
            }
            fn fetch<'a>(
                &'a self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    Err(lazybox_core::ProviderError::retryable_after(
                        lazybox_gh::SOURCE,
                        "secondary rate limit",
                        30,
                    ))
                })
            }
        }

        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(AlwaysThrottled)];
        let mut state = TickState::default();
        let mut rx = config.bus.subscribe();

        // Run well past the exhaustion threshold — a throttle that would
        // otherwise have escalated several times over.
        for _ in 0..(RETRYABLE_EXHAUSTION_ATTEMPTS + 3) {
            tick_with_state(&config, &sources, &mut state).await;
        }

        let events = drain(&mut rx);
        let count = |want: &str| {
            events
                .iter()
                .filter(|event| matches!(event, Event::ProviderError { kind, .. } if kind == want))
                .count()
        };
        assert_eq!(
            count("exhausted"),
            0,
            "a throttle must never escalate to an actionable error"
        );
        assert_eq!(
            count("retryable"),
            1,
            "the throttle surfaces as one quiet, self-healing status"
        );
    }

    /// #772: an exhausted escalation carries a terse, one-row message even
    /// when the underlying error's `detail` is a long diagnostic sentence
    /// (as the tick-timeout path produces). The raw diagnostic must not
    /// balloon the `✗ sync failed` banner.
    #[tokio::test]
    async fn exhausted_escalation_message_stays_terse() {
        struct VerboseRetryable;
        impl TaskSource for VerboseRetryable {
            fn name(&self) -> &str {
                lazybox_gh::SOURCE
            }
            fn fetch<'a>(
                &'a self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    Err(lazybox_core::ProviderError::retryable(
                        lazybox_gh::SOURCE,
                        "sync exceeded 180s — the per-upsert / per-graphql / per-git \
                         timeouts should catch this; hitting the outer cap means \
                         something escaped them and the whole tick was abandoned",
                    ))
                })
            }
        }

        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(VerboseRetryable)];
        let mut state = TickState::default();
        let mut rx = config.bus.subscribe();

        for _ in 0..RETRYABLE_EXHAUSTION_ATTEMPTS {
            tick_with_state(&config, &sources, &mut state).await;
        }

        let exhausted = drain(&mut rx)
            .into_iter()
            .find_map(|event| match event {
                Event::ProviderError { kind, message, .. } if kind == "exhausted" => Some(message),
                _ => None,
            })
            .expect("a persisting hintless transient escalates to exhausted");
        assert!(
            exhausted.len() < 80,
            "the exhausted banner stays one row, got {} chars: {exhausted}",
            exhausted.len()
        );
        assert!(
            !exhausted.contains("per-graphql"),
            "the raw diagnostic must not leak into the banner: {exhausted}"
        );
    }

    /// #782: a governor self-throttle — lazybox deliberately pacing its
    /// own sync under shared-token contention — must never escalate to the
    /// actionable "check your token" error, no matter how long it lasts.
    /// The token, connection, and GitHub budget are all fine; it's an
    /// honest, self-clearing backoff, so it stays one quiet `retryable`
    /// status with a message that blames neither the token nor the
    /// connection.
    #[tokio::test]
    async fn governor_self_throttle_never_escalates_and_stays_honest() {
        struct SelfThrottled;
        impl TaskSource for SelfThrottled {
            fn name(&self) -> &str {
                lazybox_gh::SOURCE
            }
            fn fetch<'a>(
                &'a self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move {
                    Err(lazybox_core::ProviderError::self_throttle(
                        lazybox_gh::SOURCE,
                        "GitHub graphql background allowance spent (0/3, retry in 15s)",
                        15,
                    ))
                })
            }
        }

        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(SelfThrottled)];
        let mut state = TickState::default();
        let mut rx = config.bus.subscribe();

        // Well past the exhaustion threshold — a genuine transient would
        // have escalated by now.
        for _ in 0..(RETRYABLE_EXHAUSTION_ATTEMPTS + 3) {
            tick_with_state(&config, &sources, &mut state).await;
        }

        let events = drain(&mut rx);
        let exhausted = events
            .iter()
            .filter(
                |event| matches!(event, Event::ProviderError { kind, .. } if kind == "exhausted"),
            )
            .count();
        assert_eq!(
            exhausted, 0,
            "a governor self-throttle must never escalate to an actionable error"
        );
        let messages: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                Event::ProviderError { kind, message, .. } if kind == "retryable" => {
                    Some(message.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            messages.len(),
            1,
            "the self-throttle surfaces exactly one quiet status"
        );
        let message = messages[0];
        // The message may explain the token is *in use* (it is), but must
        // never tell the user something is wrong with it or the connection —
        // that's the lie #782 is about.
        assert!(
            !message.contains("check your"),
            "the message must not tell the user to check the token/connection: {message}"
        );
        assert!(
            message.contains("pacing"),
            "the message names the deliberate self-pacing backoff: {message}"
        );
    }

    /// Finding #1: the fetch-failure path and the client-init-failure
    /// path both surface through [`TickState::broadcast_error_debounced`],
    /// so they can never drift onto different key schemes for the shared
    /// `last_error` slot. Two differently-worded retryables — as when an
    /// init failure alternates with a churning fetch throttle — coalesce
    /// to one toast; an escalation to auth still surfaces; and a cleared
    /// slot re-arms the next retryable. Pre-unification the init path kept
    /// its own exact-message debounce and could re-toast on that
    /// alternation.
    #[tokio::test]
    async fn debounced_error_coalesces_retryables_across_call_sites() {
        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        let mut state = TickState::default();
        let mut rx = config.bus.subscribe();
        let kinds = |events: Vec<Event>, want: &str| {
            events
                .into_iter()
                .filter(|event| matches!(event, Event::ProviderError { kind, .. } if kind == want))
                .count()
        };

        // A fetch throttle then an init-style hiccup: different messages,
        // one broadcast.
        state.broadcast_error_debounced(
            &config.bus,
            lazybox_gh::SOURCE,
            &lazybox_core::ProviderError::retryable_after(lazybox_gh::SOURCE, "throttled", 15),
        );
        state.broadcast_error_debounced(
            &config.bus,
            lazybox_gh::SOURCE,
            &lazybox_core::ProviderError::retryable(lazybox_gh::SOURCE, "client init timed out"),
        );
        assert_eq!(
            kinds(drain(&mut rx), "retryable"),
            1,
            "differently-worded retryables (fetch vs init) coalesce to one toast"
        );

        // Escalation to auth is a genuine change of condition — surfaces.
        state.broadcast_error_debounced(
            &config.bus,
            lazybox_gh::SOURCE,
            &lazybox_core::ProviderError::auth(lazybox_gh::SOURCE, "token revoked"),
        );
        assert_eq!(kinds(drain(&mut rx), "auth"), 1);

        // Recovery clears the slot, re-arming the next retryable.
        state.clear_error(lazybox_gh::SOURCE);
        state.broadcast_error_debounced(
            &config.bus,
            lazybox_gh::SOURCE,
            &lazybox_core::ProviderError::retryable(lazybox_gh::SOURCE, "another hiccup"),
        );
        assert_eq!(kinds(drain(&mut rx), "retryable"), 1);
    }

    /// The dedupe sentinel must not swallow a genuine change of
    /// condition: a retryable streak that escalates to an auth failure
    /// still surfaces the auth error, and a recovery between failures
    /// re-arms the next retryable broadcast.
    #[tokio::test]
    async fn retryable_dedupe_still_surfaces_escalation_and_recovery() {
        #[derive(Clone, Copy, PartialEq)]
        enum Mode {
            Retryable,
            Ok,
            Auth,
        }
        struct ModedSource {
            mode: Arc<Mutex<Mode>>,
        }
        impl TaskSource for ModedSource {
            fn name(&self) -> &str {
                lazybox_gh::SOURCE
            }
            fn fetch<'a>(
                &'a self,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                        + Send
                        + 'a,
                >,
            > {
                let mode = *self.mode.lock();
                Box::pin(async move {
                    match mode {
                        Mode::Retryable => Err(lazybox_core::ProviderError::retryable(
                            lazybox_gh::SOURCE,
                            "transient hiccup",
                        )),
                        Mode::Ok => Ok(Vec::new()),
                        Mode::Auth => Err(lazybox_core::ProviderError::auth(
                            lazybox_gh::SOURCE,
                            "token revoked",
                        )),
                    }
                })
            }
        }

        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        let mode = Arc::new(Mutex::new(Mode::Retryable));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(ModedSource { mode: mode.clone() })];
        let mut state = TickState::default();
        let mut rx = config.bus.subscribe();

        let count = |events: Vec<Event>, want_kind: &str| {
            events
                .into_iter()
                .filter(
                    |event| matches!(event, Event::ProviderError { kind, .. } if kind == want_kind),
                )
                .count()
        };

        // Two retryable ticks collapse to one broadcast.
        tick_with_state(&config, &sources, &mut state).await;
        tick_with_state(&config, &sources, &mut state).await;
        assert_eq!(count(drain(&mut rx), "retryable"), 1);

        // Escalation to auth is a real change of condition — it surfaces.
        *mode.lock() = Mode::Auth;
        tick_with_state(&config, &sources, &mut state).await;
        assert_eq!(count(drain(&mut rx), "auth"), 1);

        // A genuine recovery clears the debounce slot…
        *mode.lock() = Mode::Ok;
        tick_with_state(&config, &sources, &mut state).await;
        // …so the next retryable after recovery broadcasts again.
        *mode.lock() = Mode::Retryable;
        tick_with_state(&config, &sources, &mut state).await;
        assert_eq!(count(drain(&mut rx), "retryable"), 1);
    }

    #[tokio::test]
    async fn cached_rate_limit_controls_a_successful_partial_result() {
        let reset_at = Utc::now() + chrono::Duration::minutes(10);
        let (config, mut rx) = config_with_low_github_budget(reset_at);
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(RateLimitFixtureSource {
            succeeds: true,
            retry_after_secs: None,
        })];

        let outcome = tick(&config, &sources).await;
        let events = drain(&mut rx);
        let completed = events
            .iter()
            .position(|event| matches!(event, Event::PollCompleted { source, .. } if source == lazybox_gh::SOURCE))
            .expect("successful result completes the poll");
        let waiting = events
            .iter()
            .position(|event| matches!(event, Event::GithubRateLimitWait { .. }))
            .expect("low cached budget starts a wait");

        assert!(
            outcome.retry_after_secs.is_some_and(|secs| secs >= 598),
            "scheduler must wait for the cached reset, got {:?}",
            outcome.retry_after_secs
        );
        assert!(
            completed < waiting,
            "the wait must replace the completed state after a partial result"
        );
    }

    #[tokio::test]
    async fn successful_partial_result_preserves_its_scheduler_retry_hint() {
        let config = ServerConfig::with_store(Arc::new(lazybox_store::MemoryStore::new()));
        let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(RateLimitFixtureSource {
            succeeds: true,
            retry_after_secs: Some(414),
        })];

        let outcome = tick(&config, &sources).await;

        assert_eq!(outcome.retry_after_secs, Some(414));
        assert!(outcome.any_source_succeeded);
    }

    /// Thin newtype so a single `FixtureSource` can be shared with the
    /// test (to mutate its tasks) while still moving a `Box<dyn
    /// TaskSource>` into `tick`.
    struct FixtureSourceRef(Arc<FixtureSource>);

    impl TaskSource for FixtureSourceRef {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn fetch<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            self.0.fetch()
        }

        fn record_items_changed(&self, count: usize) {
            self.0.record_items_changed(count);
        }
    }
}

#[cfg(test)]
mod track_main_sweep_tests {
    use super::*;
    use lazybox_core::{SessionKind, WorkspaceKey, WorkspaceSession as Session};
    use lazybox_git_ops::WorktreeManager;
    use std::path::Path;
    use std::sync::Arc;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com"])
            .args(["-c", "user.name=test"])
            .args(["-c", "commit.gpgsign=false"])
            .args(["-c", "init.defaultBranch=main"])
            .current_dir(cwd)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A persisted track-main workspace whose one session sits in a
    /// scratch worktree cut off `main`, plus the manager rooted at the
    /// same base. Returns (tmp, config, manager, src path, worktree path).
    async fn seeded_tracked_workspace() -> (
        tempfile::TempDir,
        ServerConfig,
        WorktreeManager,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir src");
        git(&src, &["init", "-q"]);
        git(&src, &["branch", "-M", "main"]);
        std::fs::write(src.join("f.txt"), "c1\n").expect("write");
        git(&src, &["add", "f.txt"]);
        git(&src, &["commit", "-q", "-m", "c1"]);

        let mgr = WorktreeManager::new(tmp.path().join("base"));
        // The manager's canonical bare-clone path (`<base>/repos/<owner>/
        // <repo>.git`); pre-seeded from the local `src` so the offline
        // `checkout_new_branch_at` finds a healthy clone instead of trying
        // to fetch from github.
        let bare = tmp
            .path()
            .join("base")
            .join("repos")
            .join("acme")
            .join("widgets.git");
        std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("mkdir repos");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().expect("utf8"),
                bare.to_str().expect("utf8"),
            ],
        );
        let wt = tmp.path().join("wt");
        mgr.checkout_new_branch_at(&wt, "acme", "widgets", "scratch", "main")
            .await
            .expect("provision scratch worktree");

        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store);
        let key = WorkspaceKey::new("scratch");
        let mut ws = Workspace::empty(key.clone(), "scratch", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github("acme", "widgets"));
        ws.local = true;
        ws.track_main = true;
        ws.add_session(Session::new(
            key.clone(),
            SessionKind::Shell,
            wt.clone(),
            Utc::now(),
        ));
        commit_upsert_reported(&config, &key, ws, "seed track-main workspace");

        (tmp, config, mgr, src, wt)
    }

    /// End-to-end: the sweep resolves + persists the base branch and
    /// fast-forwards a clean, behind worktree onto `origin/main`.
    #[tokio::test]
    async fn sweep_fast_forwards_clean_workspace_and_persists_base() {
        let (_tmp, config, mgr, src, wt) = seeded_tracked_workspace().await;
        // Upstream advances after the worktree was cut.
        std::fs::write(src.join("f.txt"), "c2\n").expect("write");
        git(&src, &["add", "f.txt"]);
        git(&src, &["commit", "-q", "-m", "c2"]);

        sync_tracked_workspaces_with(&config, &mgr).await;

        let ws =
            load_workspace(&config, &WorkspaceKey::new("scratch")).expect("workspace persists");
        assert_eq!(
            ws.base_branch.as_deref(),
            Some("main"),
            "resolved base branch is persisted"
        );
        assert!(
            !ws.track_main_behind,
            "a clean worktree is fast-forwarded, not left behind"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("f.txt")).expect("read"),
            "c2\n",
            "the worktree tree advanced to main"
        );
    }

    /// A dirty worktree behind main is left untouched and flagged behind
    /// so the sidebar badge can surface it.
    #[tokio::test]
    async fn sweep_flags_dirty_workspace_behind_without_touching_it() {
        let (_tmp, config, mgr, src, wt) = seeded_tracked_workspace().await;
        std::fs::write(src.join("f.txt"), "c2\n").expect("write");
        git(&src, &["add", "f.txt"]);
        git(&src, &["commit", "-q", "-m", "c2"]);
        // Uncommitted local work.
        std::fs::write(wt.join("f.txt"), "wip\n").expect("write");

        sync_tracked_workspaces_with(&config, &mgr).await;

        let ws =
            load_workspace(&config, &WorkspaceKey::new("scratch")).expect("workspace persists");
        assert!(
            ws.track_main_behind,
            "a dirty behind worktree is flagged behind"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("f.txt")).expect("read"),
            "wip\n",
            "the uncommitted work is never touched"
        );
    }

    /// A fast-forward that ERRORS (here: git's own refusal to overwrite
    /// an untracked file the incoming tree needs) must flag the
    /// workspace behind — "could not be brought up to date" — not
    /// render it as ✓ synced. The old `Err` arm left `behind = false`,
    /// so a persistently failing sync looked permanently up to date.
    #[tokio::test]
    async fn sweep_flags_failing_fast_forward_as_behind_not_synced() {
        let (_tmp, config, mgr, src, wt) = seeded_tracked_workspace().await;
        // Upstream adds a NEW tracked file...
        std::fs::write(src.join("new.txt"), "upstream\n").expect("write");
        git(&src, &["add", "new.txt"]);
        git(&src, &["commit", "-q", "-m", "add new.txt"]);
        // ...which collides with an untracked local file of the same
        // name: the tree is status-clean (untracked-files=no), behind,
        // not diverged — and `merge --ff-only` refuses to clobber.
        std::fs::write(wt.join("new.txt"), "local scratch\n").expect("write");

        sync_tracked_workspaces_with(&config, &mgr).await;

        let ws =
            load_workspace(&config, &WorkspaceKey::new("scratch")).expect("workspace persists");
        assert!(
            ws.track_main_behind,
            "a failing fast-forward must surface as behind, never as synced"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("new.txt")).expect("read"),
            "local scratch\n",
            "the local untracked file is never clobbered"
        );
    }

    /// A worktree parked on a detached HEAD (bisect step, `git checkout
    /// <sha>` inspection) is refused and reported behind — and, above
    /// all, never advanced under the user.
    #[tokio::test]
    async fn sweep_never_advances_a_detached_head_worktree() {
        let (_tmp, config, mgr, src, wt) = seeded_tracked_workspace().await;
        std::fs::write(src.join("f.txt"), "c2\n").expect("write");
        git(&src, &["add", "f.txt"]);
        git(&src, &["commit", "-q", "-m", "c2"]);
        let before = git(&wt, &["rev-parse", "HEAD"]);
        git(&wt, &["checkout", "-q", "--detach"]);

        sync_tracked_workspaces_with(&config, &mgr).await;

        assert_eq!(
            git(&wt, &["rev-parse", "HEAD"]),
            before,
            "the detached HEAD is left exactly where the user parked it"
        );
        let ws =
            load_workspace(&config, &WorkspaceKey::new("scratch")).expect("workspace persists");
        assert!(
            ws.track_main_behind,
            "a refused sync reports behind, not silently synced"
        );
    }

    /// A tracked workspace with no on-disk worktree is left alone — no
    /// base branch resolved (which would clone the whole repo for nothing
    /// to sync).
    #[tokio::test]
    async fn sweep_skips_workspace_without_a_worktree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = WorktreeManager::new(tmp.path().join("base"));
        let store = Arc::new(lazybox_store::MemoryStore::new());
        let config = ServerConfig::with_store(store);
        let key = WorkspaceKey::new("scratch");
        let mut ws = Workspace::empty(key.clone(), "scratch", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github("acme", "widgets"));
        ws.local = true;
        ws.track_main = true;
        // No sessions → no worktree on disk.
        commit_upsert_reported(&config, &key, ws, "seed session-less workspace");

        sync_tracked_workspaces_with(&config, &mgr).await;

        let ws = load_workspace(&config, &key).expect("workspace persists");
        assert_eq!(
            ws.base_branch, None,
            "no base branch resolved when there's no worktree to sync"
        );
        assert!(!ws.track_main_behind);
        // No bare clone was provisioned for a workspace with nothing to sync.
        assert!(
            !tmp.path().join("base").join("repos").exists(),
            "no repo should be cloned for a session-less tracked workspace"
        );
    }
}
