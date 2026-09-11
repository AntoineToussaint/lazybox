//! The producer behind [`lazybox_core::PrTrailers`] (#1592).
//!
//! lazybox performs the merge itself, so it owns the moment the commit body
//! is written — and it is the only participant that knows what the PR took.
//! This module reads that back off the durable state the daemon already
//! keeps and hands it to the merge path.
//!
//! Only measured fields are filled. Everything else stays `None` and renders
//! nothing: a `Lazybox-Cost: $0.00` would read as "this was free" rather than
//! "this wasn't metered", which is the failure the whole trailer format is
//! shaped to avoid.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use lazybox_core::{
    AgentCount, CostTrailer, EffortTrailer, PrTrailers, SessionKind, TimeTrailer, Workspace,
    WorkspaceKey,
};

use crate::{ServerConfig, client_kv, polling::autofix};

/// What `workspace` took, as of `now` — the merge instant.
pub async fn measure(
    config: &ServerConfig,
    workspace: &Workspace,
    now: DateTime<Utc>,
) -> PrTrailers {
    let store = config.store.clone();
    let cost_lock = config.session_cost_lock.clone();
    let key = workspace.key.as_str().to_string();
    let measured = tokio::task::spawn_blocking(move || {
        let _guard = cost_lock.lock();
        (
            client_kv::unreported_session_cost(&*store, &key),
            autofix::ci_attempts_so_far(&*store, &key),
        )
    })
    .await;
    // A metered PR that reports nothing looks identical to an unmetered one,
    // so say why rather than letting the figures silently read as zero.
    let (cost_micros, ci_repairs) = match measured {
        Ok(measured) => measured,
        Err(e) => {
            tracing::warn!(
                workspace = %workspace.key,
                "measuring the PR's cost failed ({e}) — merging with no trailer",
            );
            (0, 0)
        }
    };

    PrTrailers {
        cost: (cost_micros > 0).then_some(CostTrailer {
            micros: Some(cost_micros),
            tokens: None,
        }),
        agents: agent_counts(workspace),
        effort: EffortTrailer {
            turns: None,
            human_handoffs: None,
            ci_repairs: (ci_repairs > 0).then_some(ci_repairs),
        },
        time: TimeTrailer {
            issue_to_merge_secs: earliest_issue_open(workspace).and_then(|at| elapsed(at, now)),
            work_to_merge_secs: first_agent_spawn(workspace).and_then(|at| elapsed(at, now)),
        },
    }
}

/// A reconciled terminal state confirms the merge, not trailer delivery.
/// Preserve the pending cost watermark when the write was never confirmed.
pub(crate) async fn mark_merge_reported(
    config: &ServerConfig,
    key: &WorkspaceKey,
    trailers: &PrTrailers,
    progress: &lazybox_core::MergeProgress,
) {
    if !progress.was_reconciled() {
        mark_reported(config, key, trailers).await;
    }
}

/// Close this workspace's cost slice by exactly the figure `trailers`
/// carried, so a later PR on the same workspace bills only what it spends
/// itself.
///
/// Takes the reported figure rather than re-reading the total: an agent goes
/// on spending while the merge mutation is in flight, and stamping the
/// then-current total would bury that spend in the watermark — absent from
/// this PR's trailer and subtracted from the next one's.
pub async fn mark_reported(config: &ServerConfig, key: &WorkspaceKey, trailers: &PrTrailers) {
    let Some(reported) = trailers.cost.as_ref().and_then(|c| c.micros) else {
        return;
    };
    let store = config.store.clone();
    let cost_lock = config.session_cost_lock.clone();
    let key = key.as_str().to_string();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = cost_lock.lock();
        client_kv::mark_session_cost_reported(&*store, &key, reported);
    })
    .await;
    if let Err(e) = result {
        tracing::warn!("mark reported cost task failed: {e}");
    }
}

/// Agent sessions grouped by agent id, busiest first so the primary agent
/// leads the line. Ties break on the id to keep the rendering stable.
///
/// This counts *sessions*, not model tiers: the usage event carries only the
/// agent id, so a run reads `claude ×3` rather than `claude-opus-5 ×3`.
fn agent_counts(workspace: &Workspace) -> Vec<AgentCount> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for session in &workspace.sessions {
        if let SessionKind::Agent { agent_id } = &session.kind {
            *counts.entry(agent_id.as_str()).or_default() += 1;
        }
    }
    let mut out: Vec<AgentCount> = counts
        .into_iter()
        .map(|(label, count)| AgentCount {
            label: label.to_string(),
            count,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
    out
}

/// When the earliest linked issue was opened — the start of lead time.
/// Strictly `created_at`: `Task::opened_at` falls back to `updated_at`,
/// which on a busy issue is minutes ago and would report a lead time of
/// nearly zero.
fn earliest_issue_open(workspace: &Workspace) -> Option<DateTime<Utc>> {
    workspace
        .gh_issues
        .iter()
        .chain(&workspace.linear_issues)
        .filter_map(|task| task.created_at)
        .min()
}

/// When an agent first started on this workspace — the start of cycle time,
/// and the number nothing in GitHub records.
fn first_agent_spawn(workspace: &Workspace) -> Option<DateTime<Utc>> {
    workspace
        .sessions
        .iter()
        .filter(|s| matches!(s.kind, SessionKind::Agent { .. }))
        .map(|s| s.created_at)
        .min()
}

/// Whole seconds from `start` to `now`, or `None` when `start` is in the
/// future — a clock skew that would otherwise render as a nonsense span.
fn elapsed(start: DateTime<Utc>, now: DateTime<Utc>) -> Option<u64> {
    now.signed_duration_since(start)
        .num_seconds()
        .try_into()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::{
        CiStatus, ReviewStatus, Task, TaskId, TaskKind, TaskRole, TaskState, WorkspaceSession,
    };
    use lazybox_store::{MemoryStore, Store};
    use std::sync::Arc;

    const KEY: &str = "github:o/r#42";

    fn workspace() -> Workspace {
        Workspace::empty(WorkspaceKey::new(KEY), "feature", at(0))
    }

    fn issue(number: u64, created_at: DateTime<Utc>) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("o/r#{number}"),
            },
            title: format!("issue {number}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/o/r/issues/{number}"),
            repo: Some("o/r".into()),
            branch: None,
            base_branch: None,
            updated_at: created_at,
            created_at: Some(created_at),
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Default::default(),
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
            kind: Some(TaskKind::Issue),
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
            blocked_by: vec![],
            merge_after: vec![],
            contracts: vec![],
            blocked_on: None,
        }
    }

    fn agent_session(agent_id: &str, created_at: DateTime<Utc>) -> WorkspaceSession {
        WorkspaceSession::new(
            WorkspaceKey::new(KEY),
            SessionKind::Agent {
                agent_id: agent_id.to_string(),
            },
            "/tmp/wt".into(),
            created_at,
        )
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).expect("valid timestamp")
    }

    fn config_with(store: Arc<dyn Store>) -> ServerConfig {
        ServerConfig::with_store(store)
    }

    /// The whole producer over a real store: a metered workspace with an
    /// issue, two agent sessions and a repaired CI run renders every field
    /// it can measure — and the ones it cannot stay absent.
    #[tokio::test]
    async fn measures_every_field_that_has_data() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        store.set_kv("meter-cost:github:o/r#42", "1500000").unwrap();
        store
            .set_kv(
                "autofix:github:o/r#42:ci",
                r#"{"attempts":2,"window_start":null,"last_attempt":null}"#,
            )
            .unwrap();
        let config = config_with(store);

        let mut ws = workspace();
        ws.gh_issues = vec![issue(7, at(0))];
        ws.sessions = vec![
            agent_session("claude", at(3_600)),
            agent_session("claude", at(4_000)),
            agent_session("codex", at(5_000)),
        ];

        let trailers = measure(&config, &ws, at(90_000)).await;
        assert_eq!(
            trailers.render(),
            "Lazybox-Cost: $1.50\n\
             Lazybox-Agents: claude ×2, codex ×1\n\
             Lazybox-Effort: 2 CI repairs\n\
             Lazybox-Time: issue→merge 1d1h · work→merge 1d",
        );
    }

    /// The trailer renders this figure as "CI repairs", so a merge-conflict
    /// rebase must not be counted into it — publishing a conflict fix as a CI
    /// repair misstates what happened, permanently.
    #[tokio::test]
    async fn conflict_repairs_are_not_counted_as_ci_repairs() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        store
            .set_kv(
                "autofix:github:o/r#42:conflict",
                r#"{"attempts":3,"window_start":null,"last_attempt":null}"#,
            )
            .unwrap();
        let config = config_with(store);

        let trailers = measure(&config, &workspace(), at(600)).await;
        assert_eq!(
            trailers.effort.ci_repairs, None,
            "three conflict rebases are not three CI repairs",
        );
    }

    /// An unmetered PR must produce NO cost line — never `$0.00`, which
    /// would read as "this was free" instead of "this wasn't metered".
    #[tokio::test]
    async fn an_unmetered_workspace_has_no_cost_line() {
        let config = config_with(Arc::new(MemoryStore::new()));
        let mut ws = workspace();
        ws.sessions = vec![agent_session("claude", at(0))];

        let trailers = measure(&config, &ws, at(600)).await;
        assert!(trailers.cost.is_none());
        assert!(!trailers.render().contains("Lazybox-Cost"));
    }

    /// A workspace with nothing measurable renders an empty block, so the
    /// merge path writes no trailer paragraph at all.
    #[tokio::test]
    async fn a_bare_workspace_measures_nothing() {
        let config = config_with(Arc::new(MemoryStore::new()));
        let trailers = measure(&config, &workspace(), at(600)).await;
        assert!(trailers.is_empty(), "{trailers:?}");
    }

    /// The since-marker: the first PR bills the whole accrued total (the
    /// issue-phase spend folded in by `move_session_cost` is part of its
    /// work), and only spend AFTER that merge reaches the next one.
    #[tokio::test]
    async fn the_marker_bills_each_pr_only_for_its_own_slice() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        // The issue's spend, folded into the PR's row before the first merge.
        client_kv::move_session_cost(&*store, "github:o/r#7", "github:o/r#42");
        store.set_kv("meter-cost:github:o/r#7", "400000").unwrap();
        client_kv::move_session_cost(&*store, "github:o/r#7", "github:o/r#42");
        store.set_kv("meter-cost:github:o/r#42", "1000000").unwrap();
        let config = config_with(store.clone());
        let ws = workspace();

        let first = measure(&config, &ws, at(600)).await;
        assert_eq!(
            first.cost.as_ref().and_then(|c| c.micros),
            Some(1_000_000),
            "the first PR bills the whole total, issue phase included",
        );

        mark_reported(&config, &ws.key, &first).await;
        // The workspace keeps working and spends another $0.25.
        store.set_kv("meter-cost:github:o/r#42", "1250000").unwrap();

        let second = measure(&config, &ws, at(1_200)).await;
        assert_eq!(
            second.cost.and_then(|c| c.micros),
            Some(250_000),
            "the reused workspace bills only what it spent since the merge",
        );
    }

    #[tokio::test]
    async fn a_reconciled_merge_keeps_unwritten_costs_pending() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        store.set_kv("meter-cost:github:o/r#42", "900000").unwrap();
        let config = config_with(store.clone());
        let ws = workspace();
        let trailers = measure(&config, &ws, at(600)).await;
        let progress = lazybox_core::MergeProgress::default();
        progress.mark_reconciled();
        mark_merge_reported(&config, &ws.key, &trailers, &progress).await;
        assert_eq!(
            client_kv::unreported_session_cost(&*store, ws.key.as_str()),
            900_000,
            "observing MERGED must not claim the missing trailer was written"
        );

        // A normal confirmed merge still closes exactly its measured slice.
        mark_merge_reported(&config, &ws.key, &trailers, &Default::default()).await;
        assert_eq!(
            client_kv::unreported_session_cost(&*store, ws.key.as_str()),
            0
        );
    }

    /// The agent does not stop while the merge mutation is in flight. Spend
    /// priced in that window was never in any trailer, so the watermark must
    /// not absorb it — stamping the then-current total instead of the
    /// reported figure hid it from this PR AND the next one.
    #[tokio::test]
    async fn spend_arriving_during_the_merge_is_billed_to_the_next_pr() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        store.set_kv("meter-cost:github:o/r#42", "1000000").unwrap();
        let config = config_with(store.clone());
        let ws = workspace();

        let reported = measure(&config, &ws, at(600)).await;
        assert_eq!(
            reported.cost.as_ref().and_then(|c| c.micros),
            Some(1_000_000)
        );

        // The agent's last turn prices while the mutation is in flight.
        store.set_kv("meter-cost:github:o/r#42", "1400000").unwrap();
        mark_reported(&config, &ws.key, &reported).await;

        let next = measure(&config, &ws, at(1_200)).await;
        assert_eq!(
            next.cost.and_then(|c| c.micros),
            Some(400_000),
            "the $0.40 spent during the merge must survive to the next PR, \
             not vanish into the watermark",
        );
    }

    /// A merge that publishes no cost figure must not move the watermark:
    /// doing so would silently consume a slice nothing recorded.
    #[tokio::test]
    async fn an_unpublished_cost_leaves_the_watermark_alone() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        store.set_kv("meter-cost:github:o/r#42", "900000").unwrap();
        let config = config_with(store.clone());
        let ws = workspace();

        mark_reported(&config, &ws.key, &PrTrailers::default()).await;

        assert_eq!(
            client_kv::unreported_session_cost(&*store, ws.key.as_str()),
            900_000,
            "nothing was reported, so nothing is marked reported",
        );
    }

    /// Clock skew (a session stamped in the future) omits the span rather
    /// than rendering a wrapped-around one.
    #[tokio::test]
    async fn a_future_start_omits_its_span() {
        let config = config_with(Arc::new(MemoryStore::new()));
        let mut ws = workspace();
        ws.sessions = vec![agent_session("claude", at(9_000))];

        let trailers = measure(&config, &ws, at(600)).await;
        assert_eq!(trailers.time.work_to_merge_secs, None);
    }
}
