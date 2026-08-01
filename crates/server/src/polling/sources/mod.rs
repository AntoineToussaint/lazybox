//! Provider adapters and source construction for polling.

use super::scheduler::{
    DEFAULT_ROUND_ROBIN_N, RoundRobinPick, plan_round_robin_tick_budgeted, will_run_global,
};
use super::upsert::load_workspace_offloaded;
use super::{
    EngagementSnapshot, FetchMode, GithubEngagementTarget, PolledScope, TaskSource, TickState,
    autofix, gh_polled_scope,
};
use crate::ServerConfig;
use chrono::Utc;
use lazybox_core::{AutoFixKind, FetchCoverage, ProviderConfig, Task, WorkspaceKey};
use lazybox_gh::GhClient;
use lazybox_ipc::{Event, ProviderErrorKind};
use lazybox_linear::LinearClient;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FullSweepCommit {
    pr_window: bool,
    sweep_timer: bool,
}

fn full_sweep_commit(
    global_pr_sweep: bool,
    pr_complete: bool,
    sweep_complete: bool,
) -> FullSweepCommit {
    FullSweepCommit {
        pr_window: global_pr_sweep && pr_complete,
        sweep_timer: sweep_complete,
    }
}

fn full_sweep_admitted(
    will_full_sweep: bool,
    manual_refresh: bool,
    governor_plan: &lazybox_gh::BackgroundPlan,
    required_points: u32,
) -> bool {
    will_full_sweep && governor_plan.admits_complete_graphql_unit(manual_refresh, required_points)
}

#[cfg(test)]
mod full_sweep_commit_tests {
    use super::*;

    #[test]
    fn degraded_pr_fetch_does_not_advance_pr_window_or_sweep_timer() {
        assert_eq!(
            full_sweep_commit(true, false, false),
            FullSweepCommit {
                pr_window: false,
                sweep_timer: false,
            }
        );
    }

    #[test]
    fn focused_work_precedes_the_cursor_committing_broad_sweep() {
        assert_eq!(
            full_sweep_stages(true),
            vec![FullSweepStage::Focused, FullSweepStage::Broad]
        );
        assert_eq!(full_sweep_stages(false), vec![FullSweepStage::Broad]);
    }

    #[test]
    fn complete_global_fetch_commits_both_progress_markers() {
        assert_eq!(
            full_sweep_commit(true, true, true),
            FullSweepCommit {
                pr_window: true,
                sweep_timer: true,
            }
        );
    }

    #[test]
    fn an_unknown_graphql_budget_only_admits_the_bootstrap_request() {
        let plan = lazybox_gh::BackgroundPlan {
            graphql_points: 4,
            rest_core_points: 1,
            graphql_budget_current: false,
            pressure: false,
            next_eligible_at: None,
            tick_interval: Duration::from_secs(60),
        };
        assert!(!full_sweep_admitted(true, false, &plan, 3));
        assert!(full_sweep_admitted(true, true, &plan, 3));
    }
}

/// `GhClient` adapter. The filter narrows the upstream result by
/// role and item type before they reach the daemon's upsert path —
/// disabled roles / types never become Workspaces. `scopes` further
/// narrows by repo / org: when non-empty, only tasks whose
/// `task.repo` matches a selected scope id pass through.
///
/// Fields are private + constructed through [`GhSource::new`]: the
/// `last_kind` cache has an invariant (initialized to `Full`) that a
/// struct literal could trivially break. Use `new`.
pub struct GhSource {
    client: GhClient,
    filter: ProviderConfig,
    scopes: std::collections::BTreeSet<String>,
    watch_repos: std::collections::BTreeSet<String>,
    detect_needs_reply: bool,
    /// Bus handle so the source can emit `PollProgress` events
    /// during its fetch. The polling layer doesn't pass `&ServerConfig`
    /// to `TaskSource::fetch` (would couple them), so each source
    /// keeps a clone of just the broadcast sender.
    bus: tokio::sync::broadcast::Sender<Event>,
    /// GitHub logins that may trigger auto-spawn via a `@lazybox`
    /// mention. Resolved by `sources_for` from
    /// `config.yaml::mention.allowed_logins`, with the authenticated
    /// viewer's login added as a default when the YAML list is
    /// empty. Empty here disables the feature entirely.
    mention_allowed_logins: std::collections::BTreeSet<String>,
    /// Auto-fix-on-failure settings, resolved by `sources_for` from
    /// `config.yaml::auto_fix`. When `enabled` is false (the default)
    /// the auto-fix scan is skipped entirely. See
    /// `lazybox_core::autofix`.
    auto_fix: lazybox_core::AutoFixSettings,
    /// Side channel for actions the source wants the polling tick to
    /// take after `fetch()` returns — today, auto-spawn requests
    /// triggered by `@lazybox` mentions. Populated inside `fetch` and
    /// drained by `tick_with_state` after the upsert pass so the
    /// freshly-created issue workspace exists before we spawn into it.
    pending_actions: std::sync::Arc<parking_lot::Mutex<Vec<ProviderAction>>>,
    /// Per-tick scheduling decision from `pick_repos_for_tick`.
    /// `sources_for` computes this against the cursor in
    /// `TickState::repo_sync_cursor` and writes it here so the
    /// `TaskSource::fetch` impl knows whether to fan out per-repo or
    /// to fire the global sweep. Held by value (not Arc) — each
    /// `sources_for` call produces a fresh source.
    scheduling: RoundRobinPick,
    governor_plan: lazybox_gh::BackgroundPlan,
    cursor_store: Option<std::sync::Arc<dyn lazybox_store::Store>>,
    /// Mode of the last successful fetch — read after `fetch` resolves
    /// by [`TaskSource::last_fetch_kind`]. `parking_lot::Mutex` is fine:
    /// trait methods take `&self` and the polling driver writes/reads
    /// strictly in sequence (fetch resolves, THEN last_fetch_kind), so
    /// there's no contention.
    last_kind: parking_lot::Mutex<FetchMode>,
    /// Retry delay reported by a failed side of the last partial
    /// successful fetch.
    retry_after_secs: parking_lot::Mutex<Option<u64>>,
    /// Coverage of the last full sweep. PARTIAL means one side
    /// (PRs or Issues) errored while the other returned results, so
    /// `fetch` returned `Ok` with only half the inbox to keep the
    /// rest alive. Read by [`TaskSource::polled_scope`] AFTER `fetch`
    /// resolves.
    ///
    /// Why this gates deletion: when the PR side fails, the client
    /// returns issues-only `Ok(..)` (see
    /// `GhClient::fetch_round_robin_with_status_and_mentions`). If
    /// `polled_scope` still claimed [`PolledScope::Exhaustive`],
    /// `rescope` would conclude every stored PR "fell out of scope"
    /// and DELETE it — a PR vanishing not because it closed, but
    /// because one poll hiccupped. A PR only legitimately leaves the
    /// inbox when it's merged/closed, falls out of the search
    /// (un-involved, un-requested), or the user re-scopes — never
    /// because a fetch failed. On a partial sweep we therefore report
    /// "no authoritative coverage" so rescope preserves everything
    /// this tick; the next clean sweep deletes anything genuinely
    /// gone. Initialized COMPLETE (a never-fetched source isn't
    /// partial).
    last_coverage: parking_lot::Mutex<FetchCoverage>,
    /// Whether the last global sweep narrowed the `involves:` search to
    /// `updated:>=` (issue #14). A windowed sweep only returned changed
    /// PRs, so — like `last_coverage` — it must NOT report
    /// `Exhaustive` coverage or rescope would delete every unchanged
    /// row. Read by [`TaskSource::polled_scope`] after `fetch` resolves.
    /// Initialized `false` (a never-fetched source isn't windowed).
    last_windowed: parking_lot::Mutex<bool>,
    /// Bounded focus/live/own-PR targets that are refreshed on every
    /// tick independently of notifications and search windows.
    hot_targets: Vec<GithubEngagementTarget>,
    /// Snoozed or terminal-state rows whose notifications wait for
    /// the slower discovery sweep.
    cold_targets: std::collections::BTreeSet<lazybox_gh::NotificationTarget>,
    /// Whether this tick also carries the base-cadence notifications
    /// heartbeat. False on the intervening hot-only ticks.
    poll_notifications: bool,
}

/// Out-of-band action a `TaskSource` may surface alongside the
/// `Vec<Task>` from `fetch()`. The polling tick drains these after
/// each fetch and dispatches them with full `&ServerConfig` access
/// (which the trait's `fetch` deliberately does not get).
#[derive(Debug, Clone)]
pub enum ProviderAction {
    /// Spawn `agent_id` in the workspace identified by `session_key`,
    /// optionally with `prompt` injected after the agent reaches its
    /// ready state. Today this fires when an allowed user has
    /// written `@lazybox` in an issue body or comment and lazybox
    /// already posted the 👀 reaction (the idempotency marker).
    AutoSpawnAgent {
        session_key: lazybox_core::SessionKey,
        agent_id: String,
        /// Model tier alias to spawn at (`@lazybox codex xhigh` /
        /// `lazybox:codex/xhigh`). `None` → the agent's default model.
        /// An unknown alias also resolves to the default downstream in
        /// `handle_spawn`.
        model_alias: Option<String>,
        prompt: Option<String>,
        /// Free-text reason for the trace log: "@lazybox mention by
        /// alice on owner/repo#42 body". Surfaces in /tmp/lazybox.log
        /// so a user wondering "why did lazybox start typing?" can
        /// trace it back to a specific comment.
        reason: String,
        /// Store-backed idempotency key for label-triggered spawns.
        /// GitHub labels persist across polls with no 👀-reaction
        /// equivalent, so `dispatch_action` records this key the first
        /// time it acts and skips on later polls — otherwise every 60s
        /// sweep would re-focus the live agent and re-inject the prompt.
        /// `None` for `@lazybox` mentions, which dedupe via the reaction.
        dedup_key: Option<String>,
    },
    /// Auto-fix a PR that's failing CI or conflicting with its base.
    /// Surfaced by the auto-fix scan (`evaluate_auto_fix`) during a
    /// fetch; the dispatcher applies the stateful cooldown /
    /// max-attempts guard, posts a brief PR comment, and spawns the
    /// agent with `prompt`. The pure eligibility guards already ran in
    /// the source; everything carried here is what the dispatcher
    /// needs without re-deriving it from a `Task`.
    AutoFixPr {
        session_key: lazybox_core::SessionKey,
        agent_id: String,
        prompt: String,
        /// `owner/name` — for the PR comment.
        repo: String,
        /// PR number — for the PR comment.
        pr_number: u64,
        /// CI failure vs merge conflict: picks the comment wording and
        /// namespaces the attempt counter.
        kind: AutoFixKind,
        /// Whether a global opt-out label is present on the PR. Computed
        /// in the source (it has the `Task`); the dispatcher combines it
        /// with the workspace's per-session [`lazybox_core::PolicyArm`]
        /// to decide whether to proceed (issue #363).
        opted_out: bool,
        /// Cooldown / max-attempts thresholds, carried from the
        /// source so the dispatcher (which only has `&ServerConfig`)
        /// doesn't have to reload config.
        settings: lazybox_core::AutoFixSettings,
        /// Free-text reason for the trace log.
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TargetedFlags {
    pub(super) hot: bool,
    pub(super) notification: bool,
}

#[derive(Debug, Clone)]
pub(super) struct TargetedRequest {
    pub(super) target: lazybox_gh::NotificationTarget,
    pub(super) flags: TargetedFlags,
}

struct TargetedOutcome {
    tasks: Vec<Task>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GhFetchPlan {
    Full,
    Warm,
    Hot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullSweepStage {
    Focused,
    Broad,
}

fn full_sweep_stages(has_focused_targets: bool) -> Vec<FullSweepStage> {
    if has_focused_targets {
        vec![FullSweepStage::Focused, FullSweepStage::Broad]
    } else {
        vec![FullSweepStage::Broad]
    }
}

pub(super) fn gh_fetch_plan(full_sweep_due: bool, poll_notifications: bool) -> GhFetchPlan {
    if full_sweep_due {
        GhFetchPlan::Full
    } else if poll_notifications {
        GhFetchPlan::Warm
    } else {
        GhFetchPlan::Hot
    }
}

pub(super) fn rank_targeted_requests(
    hot_targets: &[lazybox_gh::NotificationTarget],
    notification_entries: &[lazybox_gh::NotificationEntry],
    cold_targets: &std::collections::BTreeSet<lazybox_gh::NotificationTarget>,
) -> Vec<TargetedRequest> {
    let mut targets: std::collections::BTreeMap<lazybox_gh::NotificationTarget, TargetedFlags> =
        std::collections::BTreeMap::new();
    for target in hot_targets {
        targets.entry(target.clone()).or_default().hot = true;
    }
    for target in notification_entries
        .iter()
        .filter_map(lazybox_gh::NotificationEntry::target)
    {
        if cold_targets.contains(&target) && !targets.get(&target).is_some_and(|flags| flags.hot) {
            continue;
        }
        targets.entry(target).or_default().notification = true;
    }
    let mut ranked: Vec<TargetedRequest> = targets
        .into_iter()
        .map(|(target, flags)| TargetedRequest { target, flags })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .flags
            .hot
            .cmp(&left.flags.hot)
            .then_with(|| left.target.cmp(&right.target))
    });
    ranked
}

fn merge_targeted_tasks(base: &mut Vec<Task>, targeted: Vec<Task>) {
    let positions: std::collections::HashMap<lazybox_core::TaskId, usize> = base
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id.clone(), index))
        .collect();
    for task in targeted {
        if let Some(index) = positions.get(&task.id).copied() {
            base[index] = task;
        } else {
            base.push(task);
        }
    }
}

impl GhSource {
    fn hot_notification_targets(&self) -> Vec<lazybox_gh::NotificationTarget> {
        self.hot_targets
            .iter()
            .map(|hot| hot.target.clone())
            .collect()
    }

    pub fn new(
        client: GhClient,
        filter: ProviderConfig,
        scopes: std::collections::BTreeSet<String>,
        bus: tokio::sync::broadcast::Sender<Event>,
        mention_allowed_logins: std::collections::BTreeSet<String>,
        auto_fix: lazybox_core::AutoFixSettings,
        scheduling: RoundRobinPick,
    ) -> Self {
        let governor_plan = client.begin_background_tick(Duration::from_secs(60));
        Self {
            client,
            filter,
            scopes,
            watch_repos: std::collections::BTreeSet::new(),
            detect_needs_reply: true,
            bus,
            mention_allowed_logins,
            auto_fix,
            pending_actions: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            scheduling,
            governor_plan,
            cursor_store: None,
            // Default to Full so a never-fetched source doesn't
            // accidentally block rescope.
            last_kind: parking_lot::Mutex::new(FetchMode::Full),
            retry_after_secs: parking_lot::Mutex::new(None),
            last_coverage: parking_lot::Mutex::new(FetchCoverage::Complete),
            last_windowed: parking_lot::Mutex::new(false),
            hot_targets: Vec::new(),
            cold_targets: std::collections::BTreeSet::new(),
            poll_notifications: true,
        }
    }

    fn set_last_kind(&self, kind: FetchMode) {
        *self.last_kind.lock() = kind;
    }

    fn required_sweep_points(&self) -> u32 {
        let forecast = self.client.background_sweep_forecast(
            self.filter.pr_enabled(),
            self.filter.issue_enabled() || !self.mention_allowed_logins.is_empty(),
        );
        forecast.required_points(self.scheduling.run_global, self.filter.pr_enabled())
    }

    fn governor_status(&self) -> String {
        let next = if let Some(at) = self.governor_plan.next_eligible_at {
            at.to_rfc3339()
        } else if !self.client.should_full_sweep() {
            "notification heartbeat / hot targets".to_string()
        } else if self.scheduling.run_global {
            "global reconcile".to_string()
        } else if self.scheduling.repos.is_empty() {
            "notification heartbeat / hot targets".to_string()
        } else {
            format!("repos {}", self.scheduling.repos.join(","))
        };
        format!(
            "{} · cadence hot={}s sweep={}m cold≤{}m · next={next}",
            self.client.governor_summary(),
            self.governor_plan.tick_interval.as_secs(),
            GhClient::FULL_SWEEP_INTERVAL.as_secs() / 60,
            GhClient::FULL_RECONCILE_INTERVAL.as_secs() / 60,
        )
    }

    async fn persist_sync_cursors(&self) {
        let Some(store) = self.cursor_store.clone() else {
            return;
        };
        let key = format!("github:sync-cursors:v1:{}", self.client.username());
        let cursors = self.client.sync_cursors();
        let Ok(payload) = serde_json::to_string(&cursors) else {
            return;
        };
        match tokio::task::spawn_blocking(move || store.set_kv(&key, &payload)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("persist GitHub sync cursors failed: {error}"),
            Err(error) => tracing::warn!("persist GitHub sync cursors task failed: {error}"),
        }
    }

    async fn persist_rate_state(&self) {
        let Some(store) = self.cursor_store.clone() else {
            return;
        };
        // Skip the write when nothing persistable changed since the last
        // confirmed persist (idle notification-only ticks, warm-up).
        let Some(payload) = self.client.pending_rate_state_payload() else {
            return;
        };
        let key = format!("github:rate-state:v1:{}", self.client.username());
        let write = payload.clone();
        match tokio::task::spawn_blocking(move || store.set_kv(&key, &write)).await {
            Ok(Ok(())) => self.client.mark_rate_state_persisted(payload),
            Ok(Err(error)) => tracing::warn!("persist GitHub rate state failed: {error}"),
            Err(error) => tracing::warn!("persist GitHub rate state task failed: {error}"),
        }
    }

    fn set_retry_after_secs(&self, retry_after_secs: Option<u64>) {
        *self.retry_after_secs.lock() = retry_after_secs;
    }

    fn set_coverage(&self, coverage: FetchCoverage) {
        *self.last_coverage.lock() = coverage;
    }

    fn set_windowed(&self, windowed: bool) {
        *self.last_windowed.lock() = windowed;
    }

    fn emit_progress(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(source = "github", %message, "poll progress");
        let _ = self.bus.send(Event::PollProgress {
            source: "github".into(),
            message,
        });
    }

    /// Scan freshly-fetched tasks for auto-fix triggers and queue an
    /// [`ProviderAction::AutoFixPr`] for each eligible PR. Called from
    /// BOTH `fetch_full` and `fetch_incremental` (unlike `@lazybox`
    /// mention scanning, which needs the full GraphQL comment tree) —
    /// the CI / mergeable signals it reads live on every `Task`, so a
    /// notifications-driven incremental fetch fires auto-fix just as
    /// fast as a full sweep. The pure guards run here; the stateful
    /// cooldown / max-attempts guard runs in `dispatch_action` (it
    /// needs the store).
    ///
    /// Cheap no-op when the feature is disabled — the common case —
    /// so callers can invoke it unconditionally.
    fn queue_auto_fix_actions(&self, tasks: &[Task]) {
        if !self.auto_fix.enabled {
            return;
        }
        let mut queued = 0usize;
        let mut pending = self.pending_actions.lock();
        for task in tasks {
            // Task-shape eligibility only (global enable is gated above).
            // The label opt-out + per-session policy are resolved in the
            // dispatcher, which has the workspace store — so a
            // label-opted-out PR is still queued here and dropped there
            // unless the workspace explicitly armed it (issue #363).
            let Some(kind) = lazybox_core::auto_fix_candidate(task) else {
                continue;
            };
            let opted_out = lazybox_core::is_auto_fix_opted_out(task, &self.auto_fix);
            // Need a repo + numeric PR id to comment + key the counter.
            let Some(repo) = task.repo.clone() else {
                continue;
            };
            let Some(pr_number) = task_number_from_key(&task.id.key) else {
                continue;
            };
            let session_key = lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(task));
            let prompt = match kind {
                AutoFixKind::CiFailure => lazybox_core::prompts::build_fix_ci_prompt(task),
                AutoFixKind::MergeConflict => {
                    lazybox_core::prompts::build_fix_conflict_prompt(task)
                }
            };
            let reason = format!("auto-fix ({}) on {repo}#{pr_number}", kind.describe());
            // A label-opted-out PR is still queued so an explicit
            // per-session `Arm` can override the label in the dispatcher
            // (issue #363) — but by default it will be dropped there, so
            // it must NOT inflate the user-visible "Queued N" notice or
            // read as an actioned fix in the log. Only count / announce
            // the candidates that proceed without an explicit arm.
            tracing::info!(%reason, %session_key, opted_out, "queued auto-fix candidate");
            pending.push(ProviderAction::AutoFixPr {
                session_key,
                agent_id: DEFAULT_AGENT_ID.to_string(),
                prompt,
                repo,
                pr_number,
                kind,
                opted_out,
                settings: self.auto_fix.clone(),
                reason,
            });
            if !opted_out {
                queued += 1;
            }
        }
        if queued > 0 {
            self.emit_progress(format!("Queued {queued} auto-fix action(s)"));
        }
    }

    /// Heavy `involves:USER` GraphQL sweep — the historical fetch path,
    /// extracted from `TaskSource::fetch` so the new tick logic can
    /// fire it conditionally (every ~30 minutes, when notifications
    /// haven't given us a fast path, or as fallback on heartbeat
    /// failure).
    ///
    /// `@lazybox` mention scanning lives here (NOT in `fetch_incremental`)
    /// because the scan walks the full `involves:USER` response — the
    /// targeted single-PR/issue queries on the incremental path don't
    /// surface fresh issue bodies/comments anyway. A `@lazybox` mention
    /// will surface within the slow-sweep cadence (≤30 min default).
    async fn fetch_full(&self) -> Result<Vec<Task>, lazybox_core::ProviderError> {
        let want_prs = self.filter.pr_enabled();
        let want_issues = self.filter.issue_enabled();
        // The issue query also runs to scan for `@lazybox` mentions even
        // when issue *display* is off (the GitHub default is PR-only) —
        // see `GhClient::should_query_issues` (issue #50). Mirror that
        // here so the full sweep doesn't early-return before the scan.
        let scan_issues = want_issues || !self.mention_allowed_logins.is_empty();

        // Incremental window (issue #14). Only a GLOBAL PR sweep runs
        // the heavy `involves:` search, so only it carries a window;
        // the round-robin per-repo path is already scoped + cheap.
        // `since` is captured BEFORE the fetch (so a PR touched
        // mid-sweep is caught next time) and committed via
        // `record_pr_sweep_window` once the sweep succeeds.
        let global_pr_sweep = want_prs && self.scheduling.run_global;
        let sweep_started = chrono::Utc::now();
        let pr_since = if global_pr_sweep {
            self.client.next_pr_sweep_window()
        } else {
            None
        };

        let plan = match (want_prs, scan_issues) {
            (true, true) => "PRs + Issues",
            (true, false) => "PRs",
            (false, true) => "Issues",
            (false, false) => {
                self.emit_progress("nothing to fetch (no PR or Issue keys enabled)");
                return Ok(Vec::new());
            }
        };
        self.emit_progress(format!("Querying GitHub for {plan} (full sweep)…"));
        // Surface the rendered queries so a user debugging "filter
        // returned 0 results" can paste them into github.com/search.
        // Round-robin path adds the per-repo subset on top.
        if want_prs {
            if self.scheduling.run_global {
                self.emit_progress(format!(
                    "PR query (global{}): {}",
                    match pr_since {
                        Some(ts) => format!(", updated:>={}", ts.format("%Y-%m-%dT%H:%M:%SZ")),
                        None => String::new(),
                    },
                    self.client.pr_search_query()
                ));
            }
            if !self.scheduling.repos.is_empty() {
                self.emit_progress(format!(
                    "PR query (round-robin {} repo{}): {}",
                    self.scheduling.repos.len(),
                    if self.scheduling.repos.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    self.scheduling.repos.join(", "),
                ));
            }
        }
        if scan_issues {
            self.emit_progress(format!("Issue query: {}", self.client.issue_search_query()));
        }

        let outcome = self
            .client
            .fetch_round_robin_with_status_and_mentions(
                want_prs,
                &self.scheduling.repos,
                self.scheduling.run_global,
                want_issues,
                &self.mention_allowed_logins,
                pr_since,
            )
            .await
            .map_err(lazybox_core::ProviderError::from)?;
        let coverage = outcome.coverage;
        let pr_complete = outcome.pr_coverage == FetchCoverage::Complete;
        let sweep_complete = coverage == FetchCoverage::Complete;
        let raw = outcome.tasks;
        let partial_warning = outcome.partial_failure;
        self.set_retry_after_secs(outcome.retry_after_secs);
        let mentions = outcome.mentions;
        // Record this sweep's coverage so `polled_scope` downgrades
        // PARTIAL results from `Exhaustive` to "no authoritative
        // coverage" — otherwise rescope would delete the half of the
        // inbox the failed side couldn't return (e.g. every PR when
        // the PR query errored). See `last_coverage`.
        self.set_coverage(coverage);
        // A windowed sweep only returned changed PRs — it can't drive
        // deletion (issue #14). `pr_since.is_some()` implies this was a
        // global PR sweep, so this also clears the flag on reconcile +
        // round-robin ticks. See `last_windowed`.
        self.set_windowed(pr_since.is_some());
        // Surface partial sync failures to the user — one side
        // succeeded, the other errored, we kept the inbox alive but
        // the visible row set is incomplete. Without this notice the
        // user silently loses half their inbox until the next tick
        // maybe recovers.
        if let Some(msg) = partial_warning {
            // A partial sweep still returned OK (one side's rows), so this
            // is not a sync-stuck signal and stays a quiet `retryable`
            // status rather than escalating (#730).
            let _ = self.bus.send(Event::ProviderError {
                source: "github".into(),
                message: format!("partial sync — {msg}"),
                detail: "see /tmp/lazybox.log for the full error".into(),
                kind: ProviderErrorKind::Retryable.as_str().to_string(),
            });
        }

        // Process `@lazybox` mention triggers BEFORE returning the task
        // list. Two passes:
        //
        // 1. **Sync pass — queue spawns.** Walk every mention, look up
        //    its task in the freshly-polled set, and push the
        //    AutoSpawnAgent into `pending_actions`. No `.await`, no
        //    cancellation point. The polling tick will drain + dispatch
        //    after upsert (workspace must exist on disk before spawn).
        //
        // 2. **Async pass — react with 👀.** Fire `react_eyes`
        //    concurrently (bounded by 5 in-flight, matching the
        //    targeted-fetch fan-out). The reaction is the dedup marker
        //    for the next sweep's mention scan via `viewerHasReacted`.
        //
        // **Why queue first.** A cancel point between react_eyes
        // returning Ok and a subsequent push would strand the mention
        // with the emoji on the issue (committed remote state) and no
        // queued spawn (dropped local state). Next sweep would see
        // `viewerHasReacted=true` and skip — agent never starts.
        // Queueing first means the reverse failure mode: spawn fires
        // without an emoji, which the next sweep would re-trigger,
        // but `handle_spawn`'s singleton check (per `(session_key, kind)`)
        // collapses to a no-op. Idempotent.
        //
        // Reaction failures are LOGGED but do NOT block the spawn —
        // the queue is already populated. A failed react means we
        // re-spawn next tick; `handle_spawn`'s singleton makes that a
        // no-op too.
        if !mentions.is_empty() {
            self.emit_progress(format!(
                "Found {} @lazybox mention(s); queueing auto-spawn + reacting",
                mentions.len()
            ));
        }

        // Pass 1: build the spawn queue + pair off (mention, react-target)
        // for the parallel pass. We carry the `target_node_id` String
        // separately so the async pass owns it (the loop below moves
        // mentions into the queue).
        let mut react_targets: Vec<String> = Vec::with_capacity(mentions.len());
        // Issues an allowed user `@lazybox`-tagged this sweep. Re-admitted
        // into `kept` below so the auto-spawn always lands in a real
        // issue workspace/worktree, even when the display filter (role /
        // scope / issue-display-off) would drop the row — otherwise
        // `handle_spawn` finds no workspace and spawns the agent in
        // lazybox's own cwd with no branch (issue #50). See
        // `readmit_mentioned_tasks`.
        let mut mentioned_tasks: Vec<Task> = Vec::new();
        // Session keys a mention already queued this sweep, so the label
        // pass below doesn't ALSO queue a spawn for the same workspace —
        // otherwise a bare `@lazybox` (claude) plus a `lazybox:codex/…`
        // label would start two agents on one issue (different
        // `(session_key, kind)` singletons don't collapse).
        let mut mention_queued_keys: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        {
            let mut pending = self.pending_actions.lock();
            for mention in mentions {
                // Look up the matching task in the freshly-polled set so
                // we use the real title/body for the prompt + the
                // canonical workspace key derivation. If we can't find it
                // (shouldn't happen — the scan ran on the same response
                // that produced `raw`), skip rather than spawn against a
                // synthetic task.
                let Some(task) = raw.iter().find(|t| {
                    t.id.source == "github"
                        && t.repo.as_deref() == Some(mention.repo.as_str())
                        && task_number_from_key(&t.id.key) == Some(mention.issue_number)
                }) else {
                    tracing::warn!(
                        repo = %mention.repo,
                        issue = mention.issue_number,
                        "mention scan returned a target with no matching Task — skipping auto-spawn"
                    );
                    continue;
                };
                let session_key =
                    lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(task));
                let prompt = Some(lazybox_core::prompts::build_implement_issue_prompt(task));
                mentioned_tasks.push(task.clone());
                mention_queued_keys.insert(session_key.as_str().to_string());
                let reason = format!(
                    "@lazybox mention by {} on {}#{} ({})",
                    mention.triggered_by_login,
                    mention.repo,
                    mention.issue_number,
                    match &mention.source {
                        lazybox_gh::MentionSource::Body => "issue body",
                        lazybox_gh::MentionSource::Comment { .. } => "comment",
                    },
                );
                tracing::info!(
                    %reason,
                    target = %mention.target_node_id,
                    agent = mention.agent_id.as_deref().unwrap_or(DEFAULT_AGENT_ID),
                    model = mention.model_alias.as_deref().unwrap_or("<default>"),
                    "queued auto-spawn"
                );
                pending.push(ProviderAction::AutoSpawnAgent {
                    session_key,
                    agent_id: mention
                        .agent_id
                        .clone()
                        .unwrap_or_else(|| DEFAULT_AGENT_ID.to_string()),
                    model_alias: mention.model_alias.clone(),
                    prompt,
                    reason,
                    // Mentions dedupe via the 👀 reaction, not a store key.
                    dedup_key: None,
                });
                react_targets.push(mention.target_node_id);
            }
        }

        // `lazybox:<agent>/<model>` label triggers — the persistent,
        // more discoverable sibling of the `@lazybox` mention. Gated by
        // the same master switch (a non-empty allowlist enables the whole
        // GitHub auto-spawn feature). The eligibility rules live in the
        // pure `label_spawn_actions` (owner-only, PR-skip, mention-dedup);
        // here we just queue the results and re-admit their tasks so the
        // spawn lands in a real workspace (issue #50).
        if !self.mention_allowed_logins.is_empty() {
            let mut pending = self.pending_actions.lock();
            for (task, action) in label_spawn_actions(&raw, &mention_queued_keys) {
                if let ProviderAction::AutoSpawnAgent {
                    reason,
                    agent_id,
                    model_alias,
                    ..
                } = &action
                {
                    tracing::info!(
                        %reason,
                        %agent_id,
                        model = model_alias.as_deref().unwrap_or("<default>"),
                        "queued auto-spawn from label"
                    );
                }
                mentioned_tasks.push(task);
                pending.push(action);
            }
        }

        // Pass 2: fire reactions concurrently. 5 in flight matches
        // `fetch_incremental`'s targeted-fetch concurrency — same rate
        // budget shared, so this is the most parallelism we can give
        // without competing with ourselves. The collect() drives the
        // stream to completion; cancellation here only loses emoji
        // posts, the queued spawns survive.
        if !react_targets.is_empty() {
            use futures::stream::{self, StreamExt};
            const REACT_CONCURRENCY: usize = 5;
            stream::iter(react_targets)
                .for_each_concurrent(REACT_CONCURRENCY, |target_node_id| async move {
                    if let Err(e) = self.client.react_eyes(&target_node_id).await {
                        tracing::warn!(
                            target = %target_node_id,
                            "react_eyes failed (spawn still queued; next tick may re-fire — \
                             handle_spawn singleton makes that a no-op): {e}",
                        );
                    }
                })
                .await;
        }

        self.emit_progress(format!("Got {} raw items, applying filters…", raw.len()));
        let kept = apply_needs_reply_toggle(
            readmit_mentioned_tasks(
                filter_github_tasks_with_watches(
                    raw,
                    &self.filter,
                    &self.scopes,
                    &self.watch_repos,
                ),
                mentioned_tasks,
            ),
            self.detect_needs_reply,
        );
        self.emit_progress(format!("{} tasks kept after filter", kept.len()));

        // Advance the `updated:>=` floor (issue #14) only when this
        // tick completed the global `involves:` search. A failed PR side
        // may still return issue rows as a degraded success, but moving
        // the floor then would skip the PR interval it never fetched.
        // `pr_since.is_none()` means a reconcile sweep, which re-arms the
        // reconcile timer.
        let commit = full_sweep_commit(global_pr_sweep, pr_complete, sweep_complete);
        if commit.pr_window {
            self.client
                .record_pr_sweep_window(sweep_started, pr_since.is_none());
        }
        // A degraded result stays due so the next tick retries the
        // incomplete side instead of waiting for the normal cadence.
        if commit.sweep_timer {
            self.client.mark_full_sweep_done();
        }
        Ok(kept)
    }

    async fn fetch_targeted(
        &self,
        requests: Vec<TargetedRequest>,
    ) -> Result<TargetedOutcome, lazybox_core::ProviderError> {
        use futures::stream::{self, StreamExt};

        const TARGETED_FETCH_CONCURRENCY: usize = 5;
        let hot_node_ids: std::collections::BTreeMap<lazybox_gh::NotificationTarget, String> = self
            .hot_targets
            .iter()
            .filter_map(|hot| {
                hot.node_id
                    .as_ref()
                    .map(|node_id| (hot.target.clone(), node_id.clone()))
            })
            .collect();
        let mut batched = Vec::new();
        let mut individual = Vec::new();
        for request in requests {
            if request.flags.hot
                && let Some(node_id) = hot_node_ids.get(&request.target)
            {
                batched.push((request, node_id.clone()));
            } else {
                individual.push(request);
            }
        }

        let mut tasks = Vec::new();
        if !batched.is_empty() {
            let node_ids = batched
                .iter()
                .map(|(_, node_id)| node_id.clone())
                .collect::<Vec<_>>();
            let results = self
                .client
                .fetch_hot_tasks(&node_ids)
                .await
                .map_err(lazybox_core::ProviderError::from)?;
            for ((request, _), task) in batched.into_iter().zip(results) {
                if let Some(task) = task {
                    tasks.push(task);
                } else {
                    tracing::debug!(
                        hot = true,
                        "targeted: {}/{}#{} not visible — skipping",
                        request.target.owner,
                        request.target.repo,
                        request.target.number,
                    );
                }
            }
        }

        let results: Vec<_> = stream::iter(individual)
            .map(|request| async move {
                let result = match request.target.kind {
                    lazybox_gh::NotificationTargetKind::PullRequest => {
                        self.client
                            .fetch_single_pr(
                                &request.target.owner,
                                &request.target.repo,
                                request.target.number,
                            )
                            .await
                    }
                    lazybox_gh::NotificationTargetKind::Issue => {
                        self.client
                            .fetch_single_issue(
                                &request.target.owner,
                                &request.target.repo,
                                request.target.number,
                            )
                            .await
                    }
                };
                (request, result)
            })
            .buffer_unordered(TARGETED_FETCH_CONCURRENCY)
            .collect()
            .await;

        let mut first_error = None;
        for (request, result) in results {
            match result {
                Ok(Some(task)) => tasks.push(task),
                Ok(None) => {
                    tracing::debug!(
                        hot = request.flags.hot,
                        "targeted: {}/{}#{} not visible — skipping",
                        request.target.owner,
                        request.target.repo,
                        request.target.number,
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        hot = request.flags.hot,
                        notification = request.flags.notification,
                        "targeted: fetch failed for {}/{}#{}: {error}",
                        request.target.owner,
                        request.target.repo,
                        request.target.number,
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(error) = first_error {
            return Err(lazybox_core::ProviderError::from(error));
        }
        Ok(TargetedOutcome { tasks })
    }

    async fn fetch_hot_only(&self) -> Result<Vec<Task>, lazybox_core::ProviderError> {
        let hot_targets = self.hot_notification_targets();
        let requests = rank_targeted_requests(&hot_targets, &[], &self.cold_targets);
        let targeted = self.fetch_targeted(requests).await?;
        Ok(apply_needs_reply_toggle(
            filter_github_tasks_with_watches(
                targeted.tasks,
                &self.filter,
                &self.scopes,
                &self.watch_repos,
            ),
            self.detect_needs_reply,
        ))
    }

    /// Notifications-driven incremental fetch. Hot targets are added
    /// before notification targets, including when the REST heartbeat
    /// answers 304.
    async fn fetch_incremental(&self) -> Result<Option<Vec<Task>>, lazybox_core::ProviderError> {
        self.emit_progress("Checking GitHub notifications…");
        let poll = match self.client.fetch_notifications().await {
            Ok(p) => p,
            Err(e) => {
                // Heartbeat failure isn't fatal: signal "no
                // incremental data" so the outer `fetch` promotes to
                // a full sweep this tick. The full sweep also re-arms
                // the slow-sweep clock so a chronically-broken
                // heartbeat doesn't trap us in a loop.
                tracing::warn!("notifications heartbeat failed: {e} — promoting to full sweep");
                return Ok(None);
            }
        };
        let (entries, pending_cursor, commit_cursor) = match poll {
            lazybox_gh::NotificationsPoll::NotModified => {
                self.emit_progress("No new GitHub notifications (304)");
                (Vec::new(), None, false)
            }
            lazybox_gh::NotificationsPoll::Modified {
                entries,
                last_modified,
            } => (entries, last_modified, true),
        };
        self.emit_progress(format!(
            "{} hot target(s) + {} GitHub notification(s) — fetching changed PRs/issues",
            self.hot_targets.len(),
            entries.len(),
        ));

        let hot_targets = self.hot_notification_targets();
        let requests = rank_targeted_requests(&hot_targets, &entries, &self.cold_targets);
        let targeted = self.fetch_targeted(requests).await?;

        // At-most-once cursor advance coupled to work completion (#512).
        // Only commit the pending `Last-Modified` when every entry this
        // tick resolved (success or definitive not-visible). On any
        // transient failure we leave the old cursor in place so the next
        // heartbeat re-lists the un-fetched entry (GitHub returns the
        // full unread set on 200 — the successful entries re-fan-out too,
        // but the single-node deep-fetch is idempotent and deduped).
        if commit_cursor {
            self.client.commit_notifications_cursor(pending_cursor);
        }

        let kept = apply_needs_reply_toggle(
            filter_github_tasks_with_watches(
                targeted.tasks,
                &self.filter,
                &self.scopes,
                &self.watch_repos,
            ),
            self.detect_needs_reply,
        );
        self.emit_progress(format!(
            "{} task(s) refreshed via hot/notification targets",
            kept.len()
        ));
        Ok(Some(kept))
    }
}

fn log_rate_budget(summary: &str) {
    tracing::info!(
        target: "gh_governor",
        source = "github",
        snapshot = summary,
        "GitHub governor snapshot"
    );
}

/// Default agent id the auto-spawn flow uses when no override is
/// configured. Mirrors the historical `lazybox-tui` fallback so the
/// user gets the same agent whether they press `w w` or `@lazybox`-tag
/// the issue. Lives here (not behind a config lookup) because the
/// polling layer doesn't get a `&PersistedSetup` at fetch time —
/// the source is constructed once per tick and `fetch` is async.
const DEFAULT_AGENT_ID: &str = "claude";

/// Extract the trailing PR/issue number from a GitHub `TaskId::key`
/// (e.g. `"acme/widget#186" → 186`). Centralized so future callers
/// don't reinvent the rsplit-and-parse chain; today both the mention
/// loop and `TaskProvider::post_reply` (in `gh-provider`) need it.
fn task_number_from_key(key: &str) -> Option<u64> {
    key.rsplit_once('#').and_then(|(_, n)| n.parse().ok())
}

/// True when a label-triggered auto-spawn under `key` has already been
/// dispatched on a prior poll — the store-backed idempotency marker for
/// the `lazybox:<agent>/<model>` label path (labels have no 👀-reaction
/// dedup like mentions do).
async fn auto_spawn_already_triggered(config: &ServerConfig, key: &str) -> bool {
    let store = config.store.clone();
    let key = key.to_string();
    tokio::task::spawn_blocking(move || store.get_kv(&key))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .is_some()
}

/// True when at least one live agent terminal is bound to `session_key`.
/// Read right after `handle_spawn` returns to confirm the spawn actually
/// landed (or a session was already running) before persisting the label
/// idempotency marker — `terminal_meta` is populated synchronously on the
/// spawn / focus-existing paths but left untouched on a failed early-return.
async fn has_live_agent_session(
    config: &ServerConfig,
    session_key: &lazybox_core::SessionKey,
) -> bool {
    let terminal_meta = config.terminal.terminal_meta.lock().await;
    terminal_meta.values().any(|(sk, kind)| {
        sk.as_str() == session_key.as_str() && matches!(kind, lazybox_ipc::TerminalKind::Agent(_))
    })
}

/// Record that the label-triggered auto-spawn under `key` fired, so later
/// polls skip it. Best-effort: a failed write just means a harmless
/// re-dispatch next poll (the `handle_spawn` singleton collapses it).
async fn mark_auto_spawn_triggered(config: &ServerConfig, key: &str) {
    let store = config.store.clone();
    let key = key.to_string();
    match tokio::task::spawn_blocking(move || store.set_kv(&key, "1")).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "auto-spawn label marker write failed"),
        Err(error) => tracing::warn!(%error, "auto-spawn label marker task failed"),
    }
}

/// Dispatch one [`ProviderAction`] surfaced by a [`TaskSource`]
/// during the most recent fetch. Today this is just the
/// auto-spawn-on-mention path; future provider-driven actions plug
/// in by adding a variant + an arm.
///
/// Auto-spawn singleton enforcement happens inside `handle_spawn`
/// (it checks `find_existing_singleton` per `(session_key, kind)`),
/// so a `@lazybox` mention on an issue that already has a running
/// claude session focuses the existing terminal instead of starting
/// a second one. We rely on that rather than re-implementing the
/// check here, so the auto-spawn path and the user-pressed `w` path
/// have IDENTICAL semantics.
/// `gh` is the cached client for this tick (cloned from `TickState`),
/// used by the auto-fix arm to post the "lazybox is fixing…" PR comment.
/// `None` when no GitHub source ran this tick — the comment is then
/// skipped (best-effort), but the spawn still fires.
pub(super) async fn dispatch_action(
    config: &ServerConfig,
    source_name: &str,
    gh: Option<&GhClient>,
    action: ProviderAction,
) {
    match action {
        ProviderAction::AutoSpawnAgent {
            session_key,
            agent_id,
            model_alias,
            prompt,
            reason,
            dedup_key,
        } => {
            // Label-triggered spawns carry a store-backed marker (labels
            // persist with no reaction to skip on). If we've already
            // acted on this exact label, stop here — re-dispatching would
            // re-focus the live agent and re-inject the prompt every poll.
            if let Some(key) = dedup_key.as_deref()
                && auto_spawn_already_triggered(config, key).await
            {
                tracing::debug!(%session_key, key, "auto-spawn label already handled; skipping");
                return;
            }
            // An unknown agent id — a typo'd mention (`@lazybox codx`) or
            // a label naming an agent this build doesn't ship — falls back
            // to the default so the spawn still happens. `config.agents`
            // is the authoritative registry the spawn resolves argv from.
            let agent_id = if config.agents.get(&agent_id).is_some() {
                agent_id
            } else {
                tracing::warn!(
                    requested = %agent_id,
                    fallback = DEFAULT_AGENT_ID,
                    "auto-spawn requested an unknown agent; using the default"
                );
                DEFAULT_AGENT_ID.to_string()
            };
            tracing::info!(
                source = source_name,
                %session_key,
                %agent_id,
                model = model_alias.as_deref().unwrap_or("<default>"),
                %reason,
                has_prompt = prompt.is_some(),
                "auto-spawning agent on provider action"
            );
            // Label-triggered spawns carry a store-backed `dedup_key`;
            // `@lazybox` mentions dedupe via the 👀 reaction and carry
            // none. That tells the footer notice which tag to show (#645).
            let trigger = if dedup_key.is_some() {
                lazybox_ipc::AutonomousTrigger::Label
            } else {
                lazybox_ipc::AutonomousTrigger::Mention
            };
            crate::spawn_handler::handle_spawn(
                config,
                session_key.clone(),
                None,
                lazybox_ipc::TerminalKind::Agent(agent_id),
                crate::spawn_handler::SpawnOptions {
                    initial_prompt: prompt,
                    autonomous: true,
                    model_alias,
                    origin: lazybox_ipc::SpawnOrigin::Autonomous(trigger),
                    ..Default::default()
                },
            )
            .await;
            // Record the label marker only once a live agent session
            // actually exists for the workspace (`handle_spawn` inserts
            // into `terminal_meta` on success and on the focus-existing
            // path, but not on a failed early-return). Marking on failure
            // would strand the label — it never re-fires until relabelled.
            // Fire-and-forget after success is fine: the store marker plus
            // the `(session_key, kind)` singleton both make a re-dispatch
            // idempotent.
            if let Some(key) = dedup_key
                && has_live_agent_session(config, &session_key).await
            {
                mark_auto_spawn_triggered(config, &key).await;
            }
        }
        ProviderAction::AutoFixPr {
            session_key,
            agent_id,
            prompt,
            repo,
            pr_number,
            kind,
            opted_out,
            settings,
            reason,
        } => {
            // Per-session policy gate (issue #363). The source queued
            // this on task-shape eligibility alone; here — with the store
            // — resolve the workspace's arm against the label opt-out. A
            // `Disarm` (or a `Default` on a label-opted-out PR) drops the
            // fix before any comment, attempt, or spawn.
            //
            // Fail closed: `load_workspace_offloaded` returns `None` for a
            // genuinely-absent workspace AND for a transient store/deserialize
            // error, so we cannot tell them apart. The workspace was upserted
            // earlier in this same tick, so `None` here almost always means a
            // read error — and defaulting to `Default` would silently fire on
            // a workspace the user explicitly `Disarm`ed. Skipping instead is
            // safe: the CI-fail / conflict trigger persists, so the next sweep
            // retries once the workspace reads cleanly.
            let Some(workspace) =
                load_workspace_offloaded(config, &WorkspaceKey::new(session_key.as_str())).await
            else {
                tracing::warn!(
                    source = source_name,
                    %session_key,
                    ?kind,
                    "auto-fix: workspace policy unreadable — skipping this sweep (retries next tick)"
                );
                return;
            };
            let arm = workspace.policies.arm(kind);
            // The source already applied the task-shape gate
            // (`auto_fix_candidate`) and the global enable check before
            // queuing; here we compose the same enable + label + arm
            // policy layer the pure `resolve_auto_fix` path uses, so the
            // decision lives in one place (issue #363, tracker #512).
            if !lazybox_core::auto_fix_enabled_and_permitted(settings.enabled, opted_out, arm) {
                tracing::info!(
                    source = source_name,
                    %session_key,
                    ?kind,
                    arm = arm.as_str(),
                    opted_out,
                    "auto-fix not permitted for this workspace (per-session policy) — skipping"
                );
                return;
            }
            // Stateful guard: cooldown + max-attempts, persisted so it
            // survives restarts. Runs HERE (not in the source) because
            // it needs the store, which `TaskSource::fetch` doesn't get.
            let now = Utc::now();
            let decision = autofix::check(
                config.store.as_ref(),
                session_key.as_str(),
                kind,
                &settings,
                now,
            );
            match decision {
                autofix::AttemptDecision::Cooldown => {
                    tracing::info!(
                        source = source_name,
                        %session_key,
                        ?kind,
                        "auto-fix within cooldown — skipping this sweep"
                    );
                }
                autofix::AttemptDecision::Exhausted { notify } => {
                    tracing::warn!(
                        source = source_name,
                        %session_key,
                        ?kind,
                        max = settings.max_attempts,
                        "auto-fix budget exhausted — surfacing for manual attention"
                    );
                    if notify && let Some(gh) = gh {
                        let opt_out = settings
                            .opt_out_labels
                            .first()
                            .map(String::as_str)
                            .unwrap_or("no-auto-fix");
                        let hours = settings.window.as_secs() / 3600;
                        let body = format!(
                            "🛟 lazybox hit its auto-fix limit on this PR \
                             ({max} attempts at {what} in the last {hours}h) and is \
                             backing off — this one needs a human. Push a fix yourself, \
                             or add the `{opt_out}` label to silence auto-fix here.",
                            max = settings.max_attempts,
                            what = kind.describe(),
                        );
                        if let Err(e) = gh.post_issue_comment(&repo, pr_number, &body).await {
                            tracing::warn!(
                                "auto-fix: failed to post exhausted notice on {repo}#{pr_number}: {e}"
                            );
                        }
                    }
                }
                autofix::AttemptDecision::Proceed { attempt, max } => {
                    match crate::spawn_handler::deliver_auto_fix_prompt(
                        config,
                        session_key.clone(),
                        agent_id.clone(),
                        prompt,
                    )
                    .await
                    {
                        crate::spawn_handler::DoneGatedPromptOutcome::WaitingForDone => {
                            tracing::info!(
                                source = source_name,
                                %session_key,
                                ?kind,
                                "auto-fix: workspace agent has not settled to Done — skipping (no attempt burned)"
                            );
                            return;
                        }
                        crate::spawn_handler::DoneGatedPromptOutcome::DeliveryFailed => {
                            tracing::warn!(
                                source = source_name,
                                %session_key,
                                ?kind,
                                "auto-fix: prompt delivery failed — skipping (no attempt burned)"
                            );
                            return;
                        }
                        crate::spawn_handler::DoneGatedPromptOutcome::Delivered => {}
                    }
                    autofix::record_attempt(
                        config.store.as_ref(),
                        session_key.as_str(),
                        kind,
                        &settings,
                        now,
                    );
                    tracing::info!(
                        source = source_name,
                        %session_key,
                        %agent_id,
                        %reason,
                        attempt,
                        max,
                        "auto-fixing PR"
                    );
                    if let Some(gh) = gh {
                        let body = format!(
                            "🤖 lazybox is {what} on this PR (auto-fix attempt {attempt}/{max}). \
                             I'll push to this branch when it's sorted.",
                            what = kind.describe(),
                        );
                        if let Err(e) = gh.post_issue_comment(&repo, pr_number, &body).await {
                            tracing::warn!(
                                "auto-fix: failed to post kickoff comment on {repo}#{pr_number}: {e}"
                            );
                        }
                    }
                }
            }
        }
    }
}

impl TaskSource for GhSource {
    fn name(&self) -> &str {
        "github"
    }
    /// GH's per-tick coverage mirrors `RoundRobinPick`:
    /// `run_global=true` → exhaustive (the global involves:USER sweep
    /// hit every repo the user touches); otherwise → only the
    /// scheduler's `repos` slice was authoritatively queried. Issue
    /// #34: without this, `rescope` would treat unpolled repos as
    /// "fell out of scope" and delete their workspaces every warm tick.
    fn polled_scope(&self) -> PolledScope {
        let coverage = *self.last_coverage.lock();
        let windowed = *self.last_windowed.lock();
        gh_polled_scope(
            self.scheduling.run_global,
            &self.scheduling.repos,
            coverage,
            windowed,
        )
    }
    fn drain_actions(&self) -> Vec<ProviderAction> {
        let mut guard = self.pending_actions.lock();
        std::mem::take(&mut *guard)
    }
    fn record_items_changed(&self, count: usize) {
        self.client.note_items_changed(count);
        let governor = self.governor_status();
        log_rate_budget(&governor);
        self.emit_progress(format!("Governor: {governor}"));
    }
    /// Tiered fetch:
    ///
    /// 1. **Hot targets** — bounded focus/live/recent-own workspaces,
    ///    refreshed every tick and ranked ahead of notification rows.
    /// 2. **Slow full sweep** — heavy `involves:USER` GraphQL search,
    ///    fires every [`GhClient::FULL_SWEEP_INTERVAL`] (default 30 min)
    ///    and on the first tick after daemon start. Rescope runs.
    ///    `@lazybox` mention scanning ONLY happens on this path (the
    ///    full search response is what mention scanning walks).
    /// 3. **Fast notifications heartbeat** — `GET /notifications` with
    ///    `If-Modified-Since`; 304 → return empty `Vec`, no rescope.
    /// 4. **Targeted deep-fetch** — for each modified notification,
    ///    fetch only that one PR/issue via the single-node GraphQL
    ///    query (~85 cost units total, vs. 1000s for the full search).
    ///
    /// `last_fetch_kind` is updated each call so the tick driver can
    /// gate rescope on whether ALL sources reported `Full` this tick.
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        Box::pin(async move {
            let result = async {
                self.set_retry_after_secs(None);
                let manual_refresh = self.client.manual_refresh_pending();
                if self.client.should_full_sweep()
                    && !manual_refresh
                    && !self.governor_plan.graphql_budget_current
                {
                    self.emit_progress("Learning the current GitHub GraphQL budget…");
                    self.client
                        .bootstrap_graphql_budget()
                        .await
                        .map_err(lazybox_core::ProviderError::from)?;
                    self.set_last_kind(FetchMode::Incremental);
                    return Ok::<_, lazybox_core::ProviderError>(Vec::new());
                }
                let required_sweep_points = self.required_sweep_points();
                let full_sweep_admitted = full_sweep_admitted(
                    self.client.should_full_sweep(),
                    manual_refresh,
                    &self.governor_plan,
                    required_sweep_points,
                );
                let plan = gh_fetch_plan(full_sweep_admitted, self.poll_notifications);
                let (tasks, kind) = match plan {
                    GhFetchPlan::Full => {
                        let mut focused = Vec::new();
                        let mut broad = None;
                        for stage in full_sweep_stages(!self.hot_targets.is_empty()) {
                            match stage {
                                FullSweepStage::Focused => {
                                    let hot_targets = self.hot_notification_targets();
                                    let requests = rank_targeted_requests(
                                        &hot_targets,
                                        &[],
                                        &self.cold_targets,
                                    );
                                    let targeted = self.fetch_targeted(requests).await?;
                                    focused = apply_needs_reply_toggle(
                                        filter_github_tasks_with_watches(
                                            targeted.tasks,
                                            &self.filter,
                                            &self.scopes,
                                            &self.watch_repos,
                                        ),
                                        self.detect_needs_reply,
                                    );
                                }
                                FullSweepStage::Broad => {
                                    broad = Some(self.fetch_full().await?);
                                }
                            }
                        }
                        let mut tasks = broad.expect("full sweep stages always include broad");
                        merge_targeted_tasks(&mut tasks, focused);
                        (tasks, FetchMode::Full)
                    }
                    GhFetchPlan::Hot => (self.fetch_hot_only().await?, FetchMode::Hot),
                    GhFetchPlan::Warm => match self.fetch_incremental().await? {
                        Some(tasks) => (tasks, FetchMode::Incremental),
                        None => {
                            if self.governor_plan.graphql_points >= required_sweep_points {
                                tracing::info!(
                                    "incremental returned None; promoting to full sweep"
                                );
                                (self.fetch_full().await?, FetchMode::Full)
                            } else {
                                tracing::info!(
                                    allowance = self.governor_plan.graphql_points,
                                    "incremental failed; full sweep deferred by governor"
                                );
                                (Vec::new(), FetchMode::Incremental)
                            }
                        }
                    },
                };

                self.queue_auto_fix_actions(&tasks);
                self.set_last_kind(kind);
                Ok::<_, lazybox_core::ProviderError>(tasks)
            }
            .await;
            let governor = self.governor_status();
            log_rate_budget(&governor);
            self.emit_progress(format!("Governor: {governor}"));
            self.persist_sync_cursors().await;
            self.persist_rate_state().await;
            result
        })
    }
    fn last_fetch_kind(&self) -> FetchMode {
        *self.last_kind.lock()
    }
    fn retry_after_secs(&self) -> Option<u64> {
        *self.retry_after_secs.lock()
    }
}

/// `LinearClient` adapter.
pub struct LinearSource {
    pub client: LinearClient,
    pub filter: ProviderConfig,
    pub bus: tokio::sync::broadcast::Sender<Event>,
    /// Set by `fetch` when pagination stopped before consuming every
    /// page (a later page errored or the safety cap truncated the
    /// tail). A workspace absent from a partial result may simply live
    /// on a page we never got, so `polled_scope` downgrades to
    /// non-authoritative and rescope preserves the rest. Read by
    /// [`TaskSource::polled_scope`] AFTER `fetch` resolves (mirrors
    /// `GhSource::last_coverage`).
    last_coverage_partial: parking_lot::Mutex<bool>,
}

impl LinearSource {
    pub fn new(
        client: LinearClient,
        filter: ProviderConfig,
        bus: tokio::sync::broadcast::Sender<Event>,
    ) -> Self {
        Self {
            client,
            filter,
            bus,
            last_coverage_partial: parking_lot::Mutex::new(false),
        }
    }
    fn emit_progress(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(source = "linear", %message, "poll progress");
        let _ = self.bus.send(Event::PollProgress {
            source: "linear".into(),
            message,
        });
    }
}

impl TaskSource for LinearSource {
    fn name(&self) -> &str {
        "linear"
    }
    /// Linear's fetch paginates through every issue the user has
    /// access to with no per-team round-robin, so a COMPLETE fetch
    /// covers everything Linear owns this tick and a workspace not in
    /// `polled` genuinely fell out of upstream scope. A PARTIAL fetch
    /// (a page failed mid-pagination, or the safety cap truncated the
    /// tail) is non-authoritative: a missing workspace may just live
    /// on a page we never got, so we downgrade to `Repos(vec![])` —
    /// "covered no repos authoritatively" — and rescope preserves the
    /// stored Linear workspaces instead of deleting them.
    fn polled_scope(&self) -> PolledScope {
        let partial = *self.last_coverage_partial.lock();
        if partial {
            PolledScope::Repos(Vec::new())
        } else {
            PolledScope::Exhaustive
        }
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.emit_progress("Querying Linear for issues…");
            let outcome = self
                .client
                .fetch_all_with_coverage()
                .await
                .map_err(lazybox_core::ProviderError::from)?;
            *self.last_coverage_partial.lock() = outcome.is_partial();
            self.emit_progress(format!(
                "Got {} issues, applying filters…",
                outcome.items.len()
            ));
            let kept = filter_linear_tasks(outcome.items, &self.filter);
            self.emit_progress(format!("{} issues kept after filter", kept.len()));
            Ok(kept)
        })
    }
}

/// Build the GraphQL search qualifiers a `GhClient` should use,
/// derived from the user's persisted role + scope selection. The
/// result is appended to `default_search_qualifiers` (`is:open
/// is:pr archived:false`) before being sent to GitHub.
///
/// Mapping:
///
/// - **Role** — `involves:USER` when all four roles (or none) are
///   enabled. With a strict subset, emit explicit role qualifiers
///   (`author:USER`, `review-requested:USER`, `assignee:USER`,
///   `mentions:USER`) joined with `OR` inside parens.
/// - **Scope** — each `github:owner` becomes `org:owner`; each
///   `github:owner/repo` becomes `repo:owner/repo`. Multiple scope
///   qualifiers are OR'd inside parens so the user gets the union.
///
/// Empty role + empty scope → the same `involves:USER` baseline as
/// before, so legacy setups (no picker visited) keep working.
/// Qualifiers for the **PR** search. Reads `pr.*` keys.
pub fn build_pr_search_qualifiers(
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
    username: &str,
) -> Vec<String> {
    let mut quals = Vec::new();
    let pr_roles = [
        ("pr.author", "author"),
        ("pr.reviewer", "review-requested"),
        ("pr.assignee", "assignee"),
        ("pr.mentioned", "mentions"),
    ];
    quals.push(role_qualifier(filter, username, &pr_roles));
    if let Some(s) = scope_qualifier(scopes) {
        quals.push(s);
    }
    quals
}

/// Qualifiers for the **Issue** search. Reads `issue.*` keys (no
/// reviewer concept — issues don't have reviewers in GitHub).
pub fn build_issue_search_qualifiers(
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
    username: &str,
) -> Vec<String> {
    let mut quals = Vec::new();
    let issue_roles = [
        ("issue.author", "author"),
        ("issue.assignee", "assignee"),
        ("issue.mentioned", "mentions"),
    ];
    quals.push(role_qualifier(filter, username, &issue_roles));
    if let Some(s) = scope_qualifier(scopes) {
        quals.push(s);
    }
    quals
}

/// Build a single role qualifier for the GitHub search API.
///
/// Why not OR-with-parens — GitHub's qualifier-style search parser
/// silently mishandles parens-grouped ORs combined with other
/// qualifiers (`(author:X OR review-requested:X) repo:Y`): the API
/// returns 0 even when the unrouped equivalent
/// (`author:X repo:Y`) returns rows. Confirmed against `gh search
/// prs` 2026-05-01 — same token, same query, the paren-form returns
/// `[]` while the no-paren form returns the user's PRs.
///
/// So we use the search syntax that's known to work:
///
/// - **0 roles enabled** → `involves:USER`. The user will see no rows
///   because `filter_github_tasks` drops everything; we still want
///   the request to be valid.
/// - **1 role enabled** → emit that single qualifier directly
///   (`author:USER`). No OR, no parens, just works.
/// - **2+ roles enabled** → emit `involves:USER` (covers author,
///   reviewer, assignee, mentioned) and let `filter_github_tasks`
///   drop the disabled roles post-fetch. Slightly more bytes over
///   the wire, but reliable.
///
/// Net effect: the wire query never contains a parens group, so the
/// "0 results from a valid query" footgun is gone.
fn role_qualifier(filter: &ProviderConfig, username: &str, keys: &[(&str, &str)]) -> String {
    let enabled: Vec<&str> = keys
        .iter()
        .filter(|(k, _)| filter.has(k))
        .map(|(_, op)| *op)
        .collect();
    match enabled.len() {
        0 => format!("involves:{username}"),
        1 => format!("{}:{username}", enabled[0]),
        _ => format!("involves:{username}"),
    }
}

/// Scope qualifier for the search query.
///
/// Returns a single `repo:owner/name` / `org:foo` qualifier when
/// there's exactly one scope. With 2+ scopes we return `None` and
/// rely on `filter_github_tasks` to drop out-of-scope results
/// after the fetch.
///
/// **Why not OR-with-parens.** Same footgun documented on
/// `role_qualifier`: GitHub's search API silently returns 0 results
/// for `involves:USER (repo:A OR repo:B)`. The user saw their
/// entire inbox disappear (2026-05-27 incident) the moment they
/// added a second repo scope. The previous code emitted
/// `(repo:A OR repo:B)` — well-intentioned, broken in practice.
///
/// Cost: with 2+ scopes the search is now wider (`involves:USER`
/// alone) and we filter post-fetch. Acceptable — lazybox's typical
/// user is involved in <100 PRs total, well under the pagination
/// safety cap.
fn scope_qualifier(scopes: &std::collections::BTreeSet<String>) -> Option<String> {
    if scopes.is_empty() {
        return None;
    }
    let parts: Vec<String> = scopes
        .iter()
        .filter_map(|s| {
            let stripped = s.strip_prefix("github:")?;
            if stripped.contains('/') {
                Some(format!("repo:{stripped}"))
            } else {
                Some(format!("org:{stripped}"))
            }
        })
        .collect();
    match parts.len() {
        0 => None,
        1 => Some(parts.into_iter().next().unwrap()),
        // 2+: emit no scope qualifier on the wire. Post-fetch
        // filter handles the narrowing.
        _ => None,
    }
}

/// Drop GitHub tasks that don't match the user's enabled roles +
/// item types + scope selection. `scopes` is the
/// `selected_scopes["github"]` set (possibly empty); tasks pass the
/// scope gate when:
///
/// - `scopes` is empty (user didn't pick anything → see all), OR
/// - the task's repo matches a selected repo scope, OR
/// - the task's repo lives under a selected org scope (parent match).
///
/// Tasks without a usable `role` field default to passing the role
/// check (we trust the upstream classification).
pub fn filter_github_tasks(
    tasks: Vec<Task>,
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
) -> Vec<Task> {
    filter_github_tasks_with_watches(tasks, filter, scopes, &std::collections::BTreeSet::new())
}

pub fn filter_github_tasks_with_watches(
    tasks: Vec<Task>,
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
    watch_repos: &std::collections::BTreeSet<String>,
) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|t| {
            if t.url.contains("/pull/")
                && t.repo
                    .as_deref()
                    .is_some_and(|repo| watch_repos.contains(repo))
            {
                return true;
            }
            // Combined type+role gate. Issues use `issue.*` keys, PRs
            // use `pr.*` keys. Tasks of unknown type (discussions,
            // etc.) bypass the type/role filter — they don't have a
            // toggle.
            let type_role_ok = if t.url.contains("/pull/") {
                filter.pr_enabled() && filter.allows_pr_role(t.role)
            } else if t.url.contains("/issues/") {
                filter.issue_enabled() && filter.allows_issue_role(t.role)
            } else {
                true
            };
            if !type_role_ok {
                return false;
            }
            // Scope gate. Empty `scopes` = "all" (the no-picker
            // default). Otherwise repo:owner/name must match a
            // selected repo scope, OR its owner must match a
            // selected org scope.
            if scopes.is_empty() {
                return true;
            }
            let Some(repo) = t.repo.as_deref() else {
                return false;
            };
            let repo_scope = format!("github:{repo}");
            if scopes.contains(&repo_scope) {
                return true;
            }
            if let Some((owner, _)) = repo.split_once('/') {
                return scopes.contains(&format!("github:{owner}"));
            }
            false
        })
        .collect()
}

fn apply_needs_reply_toggle(mut tasks: Vec<Task>, detect_needs_reply: bool) -> Vec<Task> {
    if !detect_needs_reply {
        for task in &mut tasks {
            task.needs_reply = false;
        }
    }
    tasks
}

pub fn github_scopes_from_filters(
    filters: &[lazybox_config::Filter],
) -> std::collections::BTreeSet<String> {
    filters
        .iter()
        .filter_map(|filter| {
            filter
                .org
                .as_ref()
                .map(|org| format!("github:{org}"))
                .or_else(|| filter.repo.as_ref().map(|repo| format!("github:{repo}")))
        })
        .collect()
}

pub fn github_watch_repos_from_filters(
    filters: &[lazybox_config::Filter],
) -> std::collections::BTreeSet<String> {
    filters
        .iter()
        .filter_map(|filter| filter.watch.clone())
        .collect()
}

/// Re-admit `@lazybox`-mentioned issue tasks that `filter_github_tasks`
/// dropped, so an auto-spawn lands in a real workspace/worktree.
///
/// The `@lazybox` mention is an explicit, high-intent trigger: the user
/// asked lazybox to work on this exact issue. The passive display filter
/// (role / scope / issue-display-off) must not prevent the issue's
/// workspace from being created — otherwise `dispatch_action` →
/// `handle_spawn` finds no workspace and spawns the agent in lazybox's
/// own cwd with no branch (the issue #50 symptom). Tasks already in
/// `kept` are left as-is (dedup on `TaskId`); the rest are appended.
/// Build the auto-spawn actions for `lazybox:<agent>/<model>` labels on
/// the polled task set, paired with the task each targets (the caller
/// re-admits those tasks so the spawn lands in a real workspace).
///
/// Eligibility, in order:
/// - **PRs skipped** — the queued prompt is the implement-issue prompt.
/// - **Owner-only** — the label carries no author in the poll, so instead
///   of a per-actor allowlist we require the user to own the issue
///   (authored or assigned). A third party labelling an issue you're
///   merely `involves:`d in (e.g. mentioned) must not spend your tokens.
/// - **Mention-dedup** — a workspace a mention already queued this sweep
///   (`mention_queued_keys`) is skipped so a mention + a label don't
///   start two agents on one issue.
/// - **First matching label** wins; a bare `lazybox:codex` yields a
///   `None` model alias (agent default downstream).
///
/// The returned `dedup_key` is the store-backed idempotency marker
/// (labels re-appear every poll with no 👀-reaction to skip on).
pub fn label_spawn_actions(
    tasks: &[Task],
    mention_queued_keys: &std::collections::HashSet<String>,
) -> Vec<(Task, ProviderAction)> {
    let mut out = Vec::new();
    for task in tasks {
        if task.is_pr() {
            continue;
        }
        if !matches!(
            task.role,
            lazybox_core::TaskRole::Author | lazybox_core::TaskRole::Assignee
        ) {
            continue;
        }
        let session_key = lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(task));
        if mention_queued_keys.contains(session_key.as_str()) {
            continue;
        }
        let Some((label_name, agent_id, model_alias)) = task.labels.iter().find_map(|l| {
            lazybox_gh::parse_label_directive(&l.name)
                .map(|(agent, model)| (l.name.clone(), agent, model))
        }) else {
            continue;
        };
        let prompt = Some(lazybox_core::prompts::build_implement_issue_prompt(task));
        let reason = format!("{label_name} label on {}", task.id.key);
        let dedup_key = Some(format!("autospawn-label:{session_key}:{label_name}"));
        out.push((
            task.clone(),
            ProviderAction::AutoSpawnAgent {
                session_key,
                agent_id,
                model_alias,
                prompt,
                reason,
                dedup_key,
            },
        ));
    }
    out
}

pub fn readmit_mentioned_tasks(mut kept: Vec<Task>, mentioned: Vec<Task>) -> Vec<Task> {
    for task in mentioned {
        if !kept.iter().any(|k| k.id == task.id) {
            kept.push(task);
        }
    }
    kept
}

/// Drop Linear tasks whose role isn't enabled. Linear has no
/// PRs-vs-Issues distinction — flat `role.*` keys.
pub fn filter_linear_tasks(tasks: Vec<Task>, filter: &ProviderConfig) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|t| filter.allows_linear_role(t.role))
        .collect()
}

/// Pure reuse decision for the cached GitHub client: reuse only when
/// BOTH the credential source label and the token fingerprint match
/// the freshly-resolved credential.
///
/// The label alone is not enough — it names where the token came from
/// ("cmd:gh auth token", "GH_TOKEN env"), which is identical before
/// and after a rotation. Comparing the fingerprint (a short hash of
/// the token material, never the raw secret) is what makes a rotated
/// token rebuild the client instead of riding the startup token until
/// daemon restart.
pub fn gh_client_reusable(
    cached_source: &str,
    cached_fingerprint: &str,
    fresh_source: &str,
    fresh_fingerprint: &str,
) -> bool {
    cached_source == fresh_source && cached_fingerprint == fresh_fingerprint
}

/// Best-effort: build the source set from the user's persisted
/// setup. Each constructed source carries the per-provider filter
/// (role + item-type toggles) and applies it post-fetch. Providers
/// whose id isn't in `enabled_providers` are skipped entirely.
///
/// The `bus` is the daemon's broadcast sender; sources clone it so
/// they can emit `PollProgress` events during their fetch (drives
/// the polling-modal status line).
pub async fn sources_for(
    setup: &lazybox_core::PersistedSetup,
    bus: tokio::sync::broadcast::Sender<Event>,
    state: &mut TickState,
    viewer_identities: std::sync::Arc<parking_lot::Mutex<Vec<(String, String)>>>,
    gh_client_cache: crate::registries::GithubClientCache,
) -> Vec<Box<dyn TaskSource>> {
    sources_for_with_engagement(
        setup,
        bus,
        state,
        viewer_identities,
        gh_client_cache,
        &EngagementSnapshot::default(),
        true,
        None,
    )
    .await
}

#[cfg(test)]
mod auto_spawn_dedup_tests {
    use super::*;

    /// The label marker is only persisted once a live agent session
    /// exists — this predicate is what dispatch gates on, so a failed
    /// spawn (empty `terminal_meta`) leaves the label free to retry.
    #[tokio::test]
    async fn has_live_agent_session_tracks_terminal_meta() {
        let (config, _mock) = crate::ServerConfig::in_memory_with_mock();
        let sk = lazybox_core::SessionKey::new("github:o/r#9");
        assert!(
            !has_live_agent_session(&config, &sk).await,
            "no terminals → no live session (spawn failed) → label may retry"
        );

        config.terminal.terminal_meta.lock().await.insert(
            lazybox_ipc::TerminalId(1),
            (
                sk.clone(),
                lazybox_ipc::TerminalKind::Agent("claude".into()),
            ),
        );
        assert!(has_live_agent_session(&config, &sk).await);

        // A shell on a different workspace doesn't count as an agent.
        let sk2 = lazybox_core::SessionKey::new("github:o/r#10");
        config.terminal.terminal_meta.lock().await.insert(
            lazybox_ipc::TerminalId(2),
            (sk2.clone(), lazybox_ipc::TerminalKind::Shell),
        );
        assert!(!has_live_agent_session(&config, &sk2).await);
    }

    /// A `lazybox:<agent>/<model>` label re-appears on every poll (no
    /// 👀-reaction to skip on), so the store-backed marker must make a
    /// re-dispatch a full no-op — not even a focus/re-inject event.
    #[tokio::test]
    async fn label_dispatch_skips_when_marker_already_present() {
        let (config, _mock) = crate::ServerConfig::in_memory_with_mock();
        let session_key = lazybox_core::SessionKey::new("github:o/r#1");
        let key = format!("autospawn-label:{session_key}:lazybox:codex/xhigh");

        // First poll's effect: the marker is set once the spawn fired.
        assert!(!auto_spawn_already_triggered(&config, &key).await);
        mark_auto_spawn_triggered(&config, &key).await;
        assert!(auto_spawn_already_triggered(&config, &key).await);

        // Next poll re-queues the same label action. Dispatch must bail
        // before `handle_spawn` — no TerminalSpawned, no focus request.
        let mut rx = config.bus.subscribe();
        dispatch_action(
            &config,
            "github",
            None,
            ProviderAction::AutoSpawnAgent {
                session_key,
                agent_id: "claude".into(),
                model_alias: None,
                prompt: Some("Implement issue".into()),
                reason: "lazybox:codex/xhigh label on o/r#1".into(),
                dedup_key: Some(key),
            },
        )
        .await;
        assert!(
            rx.try_recv().is_err(),
            "an already-handled label must not emit any dispatch event"
        );
    }
}

#[cfg(test)]
mod auto_fix_dispatch_tests {
    use super::*;
    use lazybox_core::{AutoFixSettings, SessionKey, Workspace};
    use lazybox_ipc::{AgentState, TerminalId, TerminalKind};
    use lazybox_store::WorkspaceRecord;

    fn settings() -> AutoFixSettings {
        AutoFixSettings {
            enabled: true,
            opt_out_labels: Vec::new(),
            max_attempts: 3,
            cooldown: std::time::Duration::ZERO,
            window: std::time::Duration::from_secs(24 * 60 * 60),
        }
    }

    fn action(session_key: SessionKey) -> ProviderAction {
        ProviderAction::AutoFixPr {
            session_key,
            agent_id: "codex".into(),
            prompt: "Fix the failed CI checks.".into(),
            repo: "o/r".into(),
            pr_number: 7,
            kind: AutoFixKind::CiFailure,
            opted_out: false,
            settings: settings(),
            reason: "auto-fix (failed CI) on o/r#7".into(),
        }
    }

    fn seed_workspace(config: &ServerConfig, session_key: &SessionKey) {
        let workspace = Workspace::empty(
            WorkspaceKey::new(session_key.as_str()),
            "fix-ci",
            Utc::now(),
        );
        config
            .store
            .save_workspace(&WorkspaceRecord {
                key: workspace.key.as_str().into(),
                created_at: workspace.created_at,
                workspace_json: Some(
                    serde_json::to_string(&workspace).expect("serialize workspace"),
                ),
            })
            .expect("save workspace");
    }

    async fn seed_agent(
        config: &ServerConfig,
        session_key: &SessionKey,
        terminal_id: TerminalId,
        agent_id: &str,
        state: AgentState,
        on_main: bool,
    ) -> (TerminalId, String) {
        let backend_key = config
            .backend
            .spawn(&[agent_id.into()], None, &[], "auto-fix-test")
            .await
            .expect("spawn mock agent");
        config.terminal.terminal_meta.lock().await.insert(
            terminal_id,
            (
                session_key.clone(),
                TerminalKind::Agent(agent_id.to_string()),
            ),
        );
        config
            .terminal
            .terminals
            .lock()
            .await
            .insert(terminal_id, backend_key.clone());
        config
            .terminal
            .agent_states
            .lock()
            .await
            .insert(terminal_id, state);
        config
            .terminal
            .agent_state_generations
            .lock()
            .await
            .insert(terminal_id, 1);
        if on_main {
            config
                .terminal
                .on_main_terminals
                .lock()
                .await
                .insert(terminal_id);
        }
        (terminal_id, backend_key)
    }

    #[tokio::test]
    async fn working_agent_is_not_interrupted_or_charged_an_attempt() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let session_key = SessionKey::new("github:o/r#7");
        seed_workspace(&config, &session_key);
        let (_, backend_key) = seed_agent(
            &config,
            &session_key,
            TerminalId(705),
            "codex",
            AgentState::Working,
            false,
        )
        .await;

        dispatch_action(&config, "github", None, action(session_key.clone())).await;

        assert!(
            mock.writes_for(&backend_key).await.is_empty(),
            "auto-fix must not inject into a working agent"
        );
        assert_eq!(
            config
                .store
                .get_kv(&format!("autofix:{session_key}:ci"))
                .expect("read attempt record"),
            None,
            "waiting for Done must not consume an attempt"
        );
    }

    #[tokio::test]
    async fn done_agent_receives_the_red_pr_repair_prompt() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let session_key = SessionKey::new("github:o/r#7");
        seed_workspace(&config, &session_key);
        let (_, backend_key) = seed_agent(
            &config,
            &session_key,
            TerminalId(705),
            "codex",
            AgentState::Done,
            false,
        )
        .await;

        dispatch_action(&config, "github", None, action(session_key.clone())).await;

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while mock.writes_for(&backend_key).await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("settled agent should receive auto-fix promptly");
        let written = mock.writes_for(&backend_key).await.concat();
        assert!(
            String::from_utf8_lossy(&written).contains("Fix the failed CI checks."),
            "auto-fix prompt was not injected: {written:?}"
        );
        assert!(
            config
                .store
                .get_kv(&format!("autofix:{session_key}:ci"))
                .expect("read attempt record")
                .is_some(),
            "a delivered Done-gated repair consumes one attempt"
        );
    }

    #[tokio::test]
    async fn working_agent_of_another_kind_blocks_auto_fix() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let session_key = SessionKey::new("github:o/r#7");
        seed_workspace(&config, &session_key);
        let (_, backend_key) = seed_agent(
            &config,
            &session_key,
            TerminalId(706),
            "claude",
            AgentState::Working,
            false,
        )
        .await;

        dispatch_action(&config, "github", None, action(session_key.clone())).await;

        assert!(mock.writes_for(&backend_key).await.is_empty());
        assert_eq!(config.terminal.terminal_count().await, 1);
        assert_eq!(
            config
                .store
                .get_kv(&format!("autofix:{session_key}:ci"))
                .expect("read attempt record"),
            None
        );
    }

    #[tokio::test]
    async fn done_main_checkout_agent_is_the_exact_auto_fix_target() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let session_key = SessionKey::new("github:o/r#7");
        seed_workspace(&config, &session_key);
        let (_, backend_key) = seed_agent(
            &config,
            &session_key,
            TerminalId(707),
            "codex",
            AgentState::Done,
            true,
        )
        .await;

        dispatch_action(&config, "github", None, action(session_key)).await;

        assert!(
            String::from_utf8_lossy(&mock.writes_for(&backend_key).await.concat())
                .contains("Fix the failed CI checks.")
        );
        assert_eq!(
            config.terminal.terminal_count().await,
            1,
            "auto-fix must reuse the Done main-checkout agent"
        );
    }

    #[tokio::test]
    async fn agent_resuming_while_auto_fix_waits_for_io_is_not_interrupted() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let session_key = SessionKey::new("github:o/r#7");
        seed_workspace(&config, &session_key);
        let (terminal_id, backend_key) = seed_agent(
            &config,
            &session_key,
            TerminalId(708),
            "codex",
            AgentState::Done,
            false,
        )
        .await;
        let interaction = config.terminal.lock_terminal_io(&backend_key).await;
        let dispatch_config = config.clone();
        let dispatch_key = session_key.clone();
        let dispatch = tokio::spawn(async move {
            dispatch_action(&dispatch_config, "github", None, action(dispatch_key)).await;
        });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        config
            .terminal
            .record_agent_state(terminal_id, AgentState::Working)
            .await;
        drop(interaction);
        dispatch.await.expect("dispatch task");

        assert!(mock.writes_for(&backend_key).await.is_empty());
        assert_eq!(
            config
                .store
                .get_kv(&format!("autofix:{session_key}:ci"))
                .expect("read attempt record"),
            None
        );
    }
}

pub(super) async fn sources_for_with_engagement(
    setup: &lazybox_core::PersistedSetup,
    bus: tokio::sync::broadcast::Sender<Event>,
    state: &mut TickState,
    viewer_identities: std::sync::Arc<parking_lot::Mutex<Vec<(String, String)>>>,
    gh_client_cache: crate::registries::GithubClientCache,
    engagement: &EngagementSnapshot,
    poll_notifications: bool,
    cursor_store: Option<std::sync::Arc<dyn lazybox_store::Store>>,
) -> Vec<Box<dyn TaskSource>> {
    let mut sources: Vec<Box<dyn TaskSource>> = Vec::new();

    if setup.enabled_providers.contains(lazybox_gh::SOURCE) {
        match lazybox_gh::credential_chain()
            .resolve(lazybox_gh::SOURCE)
            .await
        {
            Ok(cred) => {
                let _initialization = gh_client_cache.lock_initialization().await;
                // Reuse the cached client when the credential source
                // AND the token material are unchanged. The source
                // label alone is rotation-stable ("cmd:gh auth token"
                // forever), so comparing only the label kept serving
                // the startup token after a rotation — every fresh
                // token the chain resolved was silently discarded and
                // the daemon stayed bricked on 401s until restart.
                // `with_filters` consumes Self and
                // returns a new client with refreshed qualifiers —
                // the underlying `Arc<Mutex<RateBudget>>` is cloned,
                // so observations made by previous ticks (or by the
                // GhSource we hand out below) remain visible to the
                // cached copy and vice versa.
                let cred_source = cred.source.clone();
                let cred_fingerprint = lazybox_gh::credential_fingerprint(cred.token());
                // Clone the cached client out under a brief std-lock and
                // release before any `.await` — the cache lock must never
                // span the `from_credential` network call (issue #92).
                let cached = gh_client_cache.cached().filter(|c| {
                    gh_client_reusable(
                        c.credential_source(),
                        c.credential_fingerprint(),
                        &cred_source,
                        &cred_fingerprint,
                    )
                });
                let restore_sync_cursors = cached.is_none();
                // Cap the cold-cache client build. `from_credential`
                // makes an untimed `/user` REST call, and this runs
                // OUTSIDE the 180s tick timeout — without a cap a
                // hung TCP connection here wedges the poll loop
                // forever (no tick ever starts, so no tick timeout
                // ever fires).
                const CLIENT_INIT_TIMEOUT: Duration = Duration::from_secs(15);
                let client_result: Result<GhClient, lazybox_core::ProviderError> = match cached {
                    Some(existing) => Ok(existing),
                    None => {
                        match tokio::time::timeout(
                            CLIENT_INIT_TIMEOUT,
                            GhClient::from_credential(cred),
                        )
                        .await
                        {
                            // Keep the typed classification (auth vs
                            // retryable vs permanent) — the failure
                            // broadcast below tells the TUI how loud
                            // to be about it.
                            Ok(result) => result.map_err(lazybox_core::ProviderError::from),
                            Err(_) => Err(lazybox_core::ProviderError::retryable(
                                lazybox_gh::SOURCE,
                                format!(
                                    "client init timed out after {}s",
                                    CLIENT_INIT_TIMEOUT.as_secs()
                                ),
                            )),
                        }
                    }
                };
                match client_result {
                    Ok(client) => {
                        let filter = setup.provider_config("github");
                        // Load YAML once for all runtime GitHub knobs
                        // that are intentionally outside the setup
                        // wizard: mention allowlist, auto-fix, and the
                        // documented `providers.github.*` section.
                        let cfg = lazybox_config::Config::load().ok();
                        let github_cfg = cfg.as_ref().map(|c| &c.providers.github);
                        let config_scopes = github_cfg
                            .map(|g| github_scopes_from_filters(&g.filters))
                            .unwrap_or_default();
                        let watch_repos = github_cfg
                            .map(|g| github_watch_repos_from_filters(&g.filters))
                            .unwrap_or_default();
                        let detect_needs_reply =
                            github_cfg.map(|g| g.detect_needs_reply).unwrap_or(true);
                        let poll_interval = github_cfg
                            .map(|g| g.poll_interval)
                            .unwrap_or(Duration::from_secs(60));
                        let background_budget_share = github_cfg
                            .map(|g| g.background_budget_share)
                            .unwrap_or(lazybox_gh::rate_budget::DEFAULT_BACKGROUND_SHARE);
                        let mut scopes = setup
                            .selected_scopes
                            .get("github")
                            .cloned()
                            .unwrap_or_default();
                        scopes.extend(config_scopes);
                        let pr_qualifiers =
                            build_pr_search_qualifiers(&filter, &scopes, client.username());
                        let issue_qualifiers =
                            build_issue_search_qualifiers(&filter, &scopes, client.username());
                        // `with_filters` returns a new owned client
                        // sharing the same budget Arc — `.clone()` on
                        // the result is cheap and keeps the cache in
                        // sync with what GhSource holds.
                        let client = client
                            .with_background_share(background_budget_share)
                            .with_filters(pr_qualifiers, issue_qualifiers)
                            .with_watch_repos(watch_repos.iter().cloned().collect())
                            .with_needs_reply(detect_needs_reply);
                        if restore_sync_cursors && let Some(store) = cursor_store.clone() {
                            let key = format!("github:sync-cursors:v1:{}", client.username());
                            match tokio::task::spawn_blocking(move || store.get_kv(&key)).await {
                                Ok(Ok(Some(payload))) => {
                                    match serde_json::from_str::<lazybox_gh::SyncCursors>(&payload)
                                    {
                                        Ok(cursors) => client.restore_sync_cursors(cursors),
                                        Err(error) => tracing::warn!(
                                            "parse persisted GitHub sync cursors failed: {error}"
                                        ),
                                    }
                                }
                                Ok(Ok(None)) => {}
                                Ok(Err(error)) => {
                                    tracing::warn!("load GitHub sync cursors failed: {error}")
                                }
                                Err(error) => {
                                    tracing::warn!("load GitHub sync cursors task failed: {error}")
                                }
                            }
                        }
                        // Reload the rate/throttle state BEFORE the
                        // governor's first `begin_background_tick` below so
                        // a restarted daemon resumes respecting the primary
                        // budget, secondary cooldown, and backoff level it
                        // learned last run instead of re-bursting into the
                        // same throttle.
                        if restore_sync_cursors && let Some(store) = cursor_store.clone() {
                            let key = format!("github:rate-state:v1:{}", client.username());
                            match tokio::task::spawn_blocking(move || store.get_kv(&key)).await {
                                Ok(Ok(Some(payload))) => {
                                    match serde_json::from_str::<lazybox_gh::PersistedRateState>(
                                        &payload,
                                    ) {
                                        Ok(state) => client.restore_rate_state(state),
                                        Err(error) => tracing::warn!(
                                            "parse persisted GitHub rate state failed: {error}"
                                        ),
                                    }
                                }
                                Ok(Ok(None)) => {}
                                Ok(Err(error)) => {
                                    tracing::warn!("load GitHub rate state failed: {error}")
                                }
                                Err(error) => {
                                    tracing::warn!("load GitHub rate state task failed: {error}")
                                }
                            }
                        }
                        // Cache + announce the authenticated viewer
                        // login so the TUI can render `@me` for the
                        // local user's bylines. Diffs the cache so we
                        // only broadcast when the value actually
                        // changes (token rotation, credential
                        // refresh, …) — quiet on the steady-state
                        // poll loop.
                        let viewer = client.username().to_string();
                        if !viewer.is_empty() {
                            let mut logins = viewer_identities.lock();
                            let entry = logins.iter_mut().find(|(src, _)| src == "github");
                            let changed = match entry {
                                Some((_, existing)) if *existing == viewer => false,
                                Some((_, existing)) => {
                                    *existing = viewer.clone();
                                    true
                                }
                                None => {
                                    logins.push(("github".into(), viewer.clone()));
                                    true
                                }
                            };
                            let snapshot = logins.clone();
                            drop(logins);
                            if changed {
                                let _ = bus.send(Event::ViewerIdentities { logins: snapshot });
                            }
                        }
                        gh_client_cache.store(client.clone());
                        // Resolve the `@lazybox` allowlist. Empty YAML
                        // list → fall back to "just the authenticated
                        // viewer", which mirrors the design doc's MVP
                        // scope (only the local lazybox user's own
                        // issues + comments count).
                        let mut mention_allowed: std::collections::BTreeSet<String> = cfg
                            .as_ref()
                            .map(|c| c.mention.allowed_logins.iter().cloned().collect())
                            .unwrap_or_default();
                        if mention_allowed.is_empty() && !viewer.is_empty() {
                            mention_allowed.insert(viewer.clone());
                        }
                        // Auto-fix settings (off unless the user opted
                        // in via `auto_fix.enabled: true`).
                        let auto_fix = cfg
                            .as_ref()
                            .map(|c| c.auto_fix.to_settings())
                            .unwrap_or_default();
                        // Round-robin scheduling. Pre-fetch we:
                        //   1. Ask the client whether this tick will
                        //      actually run a full sweep. Most ticks
                        //      take the notifications fast path and
                        //      never consult the round-robin pick —
                        //      advancing the cursor / tick counter on
                        //      those stamped repos "synced" without a
                        //      single query and evaluated the global-
                        //      sweep modulus against an inflated
                        //      counter.
                        //   2. On a full-sweep tick: prune stale
                        //      cursor entries, pick the per-tick
                        //      slice, bump the cursor for repos we're
                        //      about to query, and increment the tick
                        //      counter AFTER the pick so the K-th-tick
                        //      rule observes the value we passed in.
                        let will_full_sweep = client.should_full_sweep();
                        let now = std::time::Instant::now();
                        let sessioned_repos = engagement.sessioned_repos();
                        let governor_interval =
                            super::background_tick_interval(poll_interval, engagement.hot_count());
                        let governor_plan = client.begin_background_tick(governor_interval);
                        let want_prs = filter.pr_enabled();
                        let scan_issues = filter.issue_enabled() || !mention_allowed.is_empty();
                        let forecast = client.background_sweep_forecast(want_prs, scan_issues);
                        state.round_robin.prune(now);
                        let global_due = will_run_global(
                            state.round_robin.cursor.len(),
                            state.round_robin.tick,
                            DEFAULT_ROUND_ROBIN_N,
                        );
                        let required_sweep_points = forecast.required_points(global_due, want_prs);
                        let max_repos = if client.manual_refresh_pending() {
                            usize::MAX
                        } else {
                            forecast.repo_capacity(
                                governor_plan.graphql_points,
                                global_due,
                                DEFAULT_ROUND_ROBIN_N,
                            )
                        };
                        let full_sweep_admitted = full_sweep_admitted(
                            will_full_sweep,
                            client.manual_refresh_pending(),
                            &governor_plan,
                            required_sweep_points,
                        );
                        let scheduling = plan_round_robin_tick_budgeted(
                            &mut state.round_robin,
                            sessioned_repos,
                            full_sweep_admitted,
                            DEFAULT_ROUND_ROBIN_N,
                            max_repos,
                            now,
                        );
                        if will_full_sweep {
                            tracing::info!(
                                source = lazybox_gh::SOURCE,
                                tick = state.round_robin.tick,
                                run_global = scheduling.run_global,
                                round_robin = ?scheduling.repos,
                                sessioned = sessioned_repos.len(),
                                known_repos = state.round_robin.cursor.len(),
                                focused = state.round_robin.focused_repo.as_deref().unwrap_or(""),
                                allowance = governor_plan.graphql_points,
                                required = required_sweep_points,
                                reserve_share = background_budget_share,
                                pressure = governor_plan.pressure,
                                admitted = full_sweep_admitted,
                                "round-robin scheduling decision"
                            );
                        }
                        sources.push(Box::new(GhSource {
                            client,
                            filter,
                            scopes,
                            watch_repos,
                            detect_needs_reply,
                            bus: bus.clone(),
                            mention_allowed_logins: mention_allowed,
                            auto_fix,
                            pending_actions: std::sync::Arc::new(parking_lot::Mutex::new(
                                Vec::new(),
                            )),
                            scheduling,
                            governor_plan,
                            cursor_store: cursor_store.clone(),
                            // Default to Full so a never-fetched
                            // source doesn't accidentally block rescope.
                            last_kind: parking_lot::Mutex::new(FetchMode::Full),
                            retry_after_secs: parking_lot::Mutex::new(None),
                            last_coverage: parking_lot::Mutex::new(FetchCoverage::Complete),
                            last_windowed: parking_lot::Mutex::new(false),
                            hot_targets: engagement.hot_targets().to_vec(),
                            cold_targets: engagement.cold_targets().clone(),
                            poll_notifications,
                        }));
                    }
                    Err(e) => {
                        // A dead token at startup used to be log-only:
                        // the daemon looked idle in the UI while every
                        // tick silently failed to even build a client.
                        // Broadcast through the SAME debounced path fetch
                        // failures use, so a retryable init failure
                        // coalesces onto the shared sentinel instead of
                        // re-toasting when it alternates with a churning
                        // fetch retryable (#727).
                        tracing::warn!("github client init failed: {}", e.diagnostic());
                        state.broadcast_error_debounced(&bus, lazybox_gh::SOURCE, &e);
                    }
                }
            }
            Err(e) => tracing::info!("github credentials not available: {e}"),
        }
    }

    if setup.enabled_providers.contains("linear") {
        match LinearClient::from_env() {
            Ok(client) => sources.push(Box::new(LinearSource::new(
                client,
                setup.provider_config("linear"),
                bus.clone(),
            ))),
            Err(e) => tracing::info!("linear not configured: {e}"),
        }
    }

    sources
}

/// Convenience: build the default source set assuming both providers
/// are enabled with their default filters. Used by binaries that
/// bypass the setup screen (e.g. headless `lazybox server start` in
/// CI). When a saved `PersistedSetup` exists in the store, prefer
/// that instead.
pub async fn default_sources(
    bus: tokio::sync::broadcast::Sender<Event>,
) -> Vec<Box<dyn TaskSource>> {
    let setup = lazybox_core::PersistedSetup {
        enabled_providers: ["github".to_string(), "linear".to_string()]
            .into_iter()
            .collect(),
        enabled_agents: Default::default(),
        provider_filters: [
            ("github".into(), ProviderConfig::default_for("github")),
            ("linear".into(), ProviderConfig::default_for("linear")),
        ]
        .into_iter()
        .collect(),
        // Empty selected_scopes = "all scopes" (legacy behavior).
        selected_scopes: Default::default(),
    };
    // No persistent state — this helper is for ad-hoc / test paths
    // where a fresh client per call is the right behavior. Viewer
    // identities also get a throwaway slot: ad-hoc callers don't
    // need the cached value visible to other connections.
    let mut throwaway_state = TickState::default();
    let throwaway_viewers = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let throwaway_client_cache = crate::registries::GithubClientCache::default();
    sources_for(
        &setup,
        bus,
        &mut throwaway_state,
        throwaway_viewers,
        throwaway_client_cache,
    )
    .await
}
