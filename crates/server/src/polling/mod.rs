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
//! `last_viewed_at` all survive the poll. We do it inline here since
//! there's only one place sessions enter the system.

mod autofix;
mod handlers;
mod mutate;
mod scheduler;

pub use scheduler::{
    CURSOR_TTL, DEFAULT_ROUND_ROBIN_N, RoundRobinPick, RoundRobinState, pick_repos_for_tick,
    plan_round_robin_tick,
};

pub use handlers::{
    ProviderHandle, apply_pr_details, handle_add_assignees, handle_clean_worktrees,
    handle_delete_orphaned_worktree, handle_fetch_pr_details, handle_fetch_repo_labels,
    handle_inspect_worktrees, handle_merge_pr, handle_request_reviewers, handle_set_assignees,
    handle_set_labels, post_reply, prefetch_top_pr_details, remove_merged_workspace,
};
pub use mutate::{MutationOutcome, apply_and_commit, fetch_and_apply};

use crate::ServerConfig;
use chrono::Utc;
use futures::FutureExt;
use lazybox_core::{AutoFixKind, ProviderConfig, Task, Workspace, WorkspaceKey};
use lazybox_gh::GhClient;
use lazybox_ipc::Event;
use lazybox_linear::LinearClient;
use lazybox_store::WorkspaceRecord;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

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
    partial: bool,
    windowed: bool,
) -> PolledScope {
    if partial || windowed {
        return PolledScope::Repos(Vec::new());
    }
    if run_global {
        PolledScope::Exhaustive
    } else {
        PolledScope::Repos(repos.to_vec())
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
    pending_actions: std::sync::Arc<std::sync::Mutex<Vec<ProviderAction>>>,
    /// Per-tick scheduling decision from `pick_repos_for_tick`.
    /// `sources_for` computes this against the cursor in
    /// `TickState::repo_sync_cursor` and writes it here so the
    /// `TaskSource::fetch` impl knows whether to fan out per-repo or
    /// to fire the global sweep. Held by value (not Arc) — each
    /// `sources_for` call produces a fresh source.
    scheduling: RoundRobinPick,
    /// Mode of the last successful fetch — read after `fetch` resolves
    /// by [`TaskSource::last_fetch_kind`]. `std::sync::Mutex` is fine:
    /// trait methods take `&self` and the polling driver writes/reads
    /// strictly in sequence (fetch resolves, THEN last_fetch_kind), so
    /// there's no contention.
    last_kind: std::sync::Mutex<FetchMode>,
    /// Whether the last full sweep was a PARTIAL success — one side
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
    /// gone. Initialized `false` (a never-fetched source isn't
    /// partial).
    last_coverage_partial: std::sync::Mutex<bool>,
    /// Whether the last global sweep narrowed the `involves:` search to
    /// `updated:>=` (issue #14). A windowed sweep only returned changed
    /// PRs, so — like `last_coverage_partial` — it must NOT report
    /// `Exhaustive` coverage or rescope would delete every unchanged
    /// row. Read by [`TaskSource::polled_scope`] after `fetch` resolves.
    /// Initialized `false` (a never-fetched source isn't windowed).
    last_windowed: std::sync::Mutex<bool>,
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
        prompt: Option<String>,
        /// Free-text reason for the trace log: "@lazybox mention by
        /// alice on owner/repo#42 body". Surfaces in /tmp/lazybox.log
        /// so a user wondering "why did lazybox start typing?" can
        /// trace it back to a specific comment.
        reason: String,
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
        prompt: Option<String>,
        /// `owner/name` — for the PR comment.
        repo: String,
        /// PR number — for the PR comment.
        pr_number: u64,
        /// CI failure vs merge conflict: picks the comment wording and
        /// namespaces the attempt counter.
        kind: AutoFixKind,
        /// Cooldown / max-attempts thresholds, carried from the
        /// source so the dispatcher (which only has `&ServerConfig`)
        /// doesn't have to reload config.
        settings: lazybox_core::AutoFixSettings,
        /// Free-text reason for the trace log.
        reason: String,
    },
}

impl GhSource {
    pub fn new(
        client: GhClient,
        filter: ProviderConfig,
        scopes: std::collections::BTreeSet<String>,
        bus: tokio::sync::broadcast::Sender<Event>,
        mention_allowed_logins: std::collections::BTreeSet<String>,
        auto_fix: lazybox_core::AutoFixSettings,
        scheduling: RoundRobinPick,
    ) -> Self {
        Self {
            client,
            filter,
            scopes,
            bus,
            mention_allowed_logins,
            auto_fix,
            pending_actions: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            scheduling,
            // Default to Full so a never-fetched source doesn't
            // accidentally block rescope.
            last_kind: std::sync::Mutex::new(FetchMode::Full),
            last_coverage_partial: std::sync::Mutex::new(false),
            last_windowed: std::sync::Mutex::new(false),
        }
    }

    fn set_last_kind(&self, kind: FetchMode) {
        *self
            .last_kind
            .lock()
            .expect("GhSource.last_kind mutex poisoned") = kind;
    }

    fn set_coverage_partial(&self, partial: bool) {
        *self
            .last_coverage_partial
            .lock()
            .expect("GhSource.last_coverage_partial mutex poisoned") = partial;
    }

    fn set_windowed(&self, windowed: bool) {
        *self
            .last_windowed
            .lock()
            .expect("GhSource.last_windowed mutex poisoned") = windowed;
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
        let mut pending = self
            .pending_actions
            .lock()
            .expect("GhSource.pending_actions poisoned");
        for task in tasks {
            let Some(kind) = lazybox_core::evaluate_auto_fix(task, &self.auto_fix) else {
                continue;
            };
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
            tracing::info!(%reason, %session_key, "queued auto-fix");
            pending.push(ProviderAction::AutoFixPr {
                session_key,
                agent_id: DEFAULT_AGENT_ID.to_string(),
                prompt: Some(prompt),
                repo,
                pr_number,
                kind,
                settings: self.auto_fix.clone(),
                reason,
            });
            queued += 1;
        }
        if queued > 0 {
            self.emit_progress(format!("Queued {queued} auto-fix action(s)"));
        }
    }

    /// Heavy `involves:USER` GraphQL sweep — the historical fetch path,
    /// extracted from `TaskSource::fetch` so the new tick logic can
    /// fire it conditionally (every ~10 minutes, when notifications
    /// haven't given us a fast path, or as fallback on heartbeat
    /// failure).
    ///
    /// `@lazybox` mention scanning lives here (NOT in `fetch_incremental`)
    /// because the scan walks the full `involves:USER` response — the
    /// targeted single-PR/issue queries on the incremental path don't
    /// surface fresh issue bodies/comments anyway. A `@lazybox` mention
    /// will surface within the slow-sweep cadence (≤10 min default).
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

        let (raw, partial_warning, mentions) = self
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
        // Record whether this sweep was partial so `polled_scope`
        // downgrades from `Exhaustive` to "no authoritative coverage"
        // — otherwise rescope would delete the half of the inbox the
        // failed side couldn't return (e.g. every PR when the PR
        // query errored). See `last_coverage_partial`.
        self.set_coverage_partial(partial_warning.is_some());
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
            let _ = self.bus.send(Event::ProviderError {
                source: "github".into(),
                message: format!("partial sync — {msg}"),
                detail: "see /tmp/lazybox.log for the full error".into(),
                kind: "retryable".into(),
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
        {
            let mut pending = self
                .pending_actions
                .lock()
                .expect("GhSource.pending_actions poisoned");
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
                tracing::info!(%reason, target = %mention.target_node_id, "queued auto-spawn");
                pending.push(ProviderAction::AutoSpawnAgent {
                    session_key,
                    agent_id: DEFAULT_AGENT_ID.to_string(),
                    prompt,
                    reason,
                });
                react_targets.push(mention.target_node_id);
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
        let kept = readmit_mentioned_tasks(
            filter_github_tasks(raw, &self.filter, &self.scopes),
            mentioned_tasks,
        );
        self.emit_progress(format!("{} tasks kept after filter", kept.len()));

        // Auto-fix scan: queue fix-CI / resolve-conflict spawns for
        // eligible PRs. Drained + dispatched alongside mention spawns.
        self.queue_auto_fix_actions(&kept);

        // Advance the `updated:>=` floor (issue #14) only when this
        // tick actually ran the global `involves:` search — a
        // round-robin per-repo sweep didn't look at the whole involved
        // set, so moving the floor past PRs it never fetched would drop
        // them from the next window. `pr_since.is_none()` here means a
        // reconcile sweep, which re-arms the reconcile timer.
        if global_pr_sweep {
            self.client
                .record_pr_sweep_window(sweep_started, pr_since.is_none());
        }
        // Mark sweep complete BEFORE returning so the next tick's
        // `should_full_sweep` check sees fresh data.
        self.client.mark_full_sweep_done();
        log_rate_budget(&self.client);
        Ok(kept)
    }

    /// Notifications-driven incremental fetch. Returns `Ok(None)` when
    /// no targeted fetch should follow this tick (304 from GitHub or
    /// heartbeat failure that we want to swallow) — caller treats that
    /// as a no-op tick. Returns `Ok(Some(tasks))` with the targeted
    /// deep-fetched PRs/issues otherwise.
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
        let entries = match poll {
            lazybox_gh::NotificationsPoll::NotModified => {
                self.emit_progress("No new GitHub notifications (304)");
                return Ok(Some(Vec::new()));
            }
            lazybox_gh::NotificationsPoll::Modified { entries } => entries,
        };
        self.emit_progress(format!(
            "{} GitHub notification(s) — fetching changed PRs/issues",
            entries.len()
        ));

        // Dedup at the source: GitHub fires several notifications per
        // PR within a window (one per comment + one per CI status flip),
        // and we want exactly one targeted fetch per distinct PR/issue.
        // `BTreeSet<NotificationTarget>` collapses duplicates and gives
        // deterministic iteration order — useful for stable logs.
        let targets: std::collections::BTreeSet<lazybox_gh::NotificationTarget> = entries
            .iter()
            .filter_map(lazybox_gh::NotificationEntry::target)
            .collect();

        // Bounded-concurrent fan-out, mirroring the watched-repo
        // pattern in `GhClient::fetch_all_prs`. 5 in flight is the
        // same compromise: large enough to compress the latency of 10+
        // targets into two batches, small enough that the local rate
        // budget (capacity 30) doesn't get fully drained by a single
        // tick. Failures are logged per-target — one bad fetch never
        // poisons the rest of the batch.
        use futures::stream::{self, StreamExt};
        const TARGETED_FETCH_CONCURRENCY: usize = 5;
        let tasks: Vec<Task> = stream::iter(targets)
            .map(|target| async move {
                let result = match target.kind {
                    lazybox_gh::NotificationTargetKind::PullRequest => {
                        self.client
                            .fetch_single_pr(&target.owner, &target.repo, target.number)
                            .await
                    }
                    lazybox_gh::NotificationTargetKind::Issue => {
                        self.client
                            .fetch_single_issue(&target.owner, &target.repo, target.number)
                            .await
                    }
                };
                (target, result)
            })
            .buffer_unordered(TARGETED_FETCH_CONCURRENCY)
            .filter_map(|(target, result)| async move {
                match result {
                    Ok(Some(t)) => Some(t),
                    Ok(None) => {
                        tracing::debug!(
                            "incremental: {}/{}#{} not visible — skipping",
                            target.owner,
                            target.repo,
                            target.number,
                        );
                        None
                    }
                    Err(e) => {
                        // Per-target failure is non-fatal: log and move on.
                        // The next tick's heartbeat will re-deliver the
                        // notification if it's still relevant; the full
                        // sweep timer eventually catches anything stuck.
                        tracing::warn!(
                            "incremental: fetch failed for {}/{}#{}: {e}",
                            target.owner,
                            target.repo,
                            target.number,
                        );
                        None
                    }
                }
            })
            .collect()
            .await;

        let kept = filter_github_tasks(tasks, &self.filter, &self.scopes);
        self.emit_progress(format!(
            "{} task(s) refreshed via notifications",
            kept.len()
        ));
        // Auto-fix fires on the fast path too: the CI / mergeable
        // signals it reads are on every Task, so a notification-driven
        // CI-failure flip kicks off a fix without waiting for the next
        // full sweep.
        self.queue_auto_fix_actions(&kept);
        log_rate_budget(&self.client);
        Ok(Some(kept))
    }
}

fn log_rate_budget(client: &GhClient) {
    let snap = client.rate_snapshot();
    if let Some(remote) = snap.remote {
        tracing::info!(
            source = "github",
            remote_remaining = remote.remaining,
            remote_limit = remote.limit,
            local_available = snap.local_available,
            local_capacity = snap.local_capacity,
            "rate budget snapshot"
        );
    }
}

/// Default agent id the auto-spawn flow uses when no override is
/// configured. Mirrors the historical `lazybox-tui` fallback so the
/// user gets the same agent whether they press `w` or `@lazybox`-tag
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
async fn dispatch_action(
    config: &ServerConfig,
    source_name: &str,
    gh: Option<&GhClient>,
    action: ProviderAction,
) {
    match action {
        ProviderAction::AutoSpawnAgent {
            session_key,
            agent_id,
            prompt,
            reason,
        } => {
            tracing::info!(
                source = source_name,
                %session_key,
                %agent_id,
                %reason,
                has_prompt = prompt.is_some(),
                "auto-spawning agent on provider action"
            );
            crate::spawn_handler::handle_spawn(
                config,
                session_key,
                None,
                lazybox_ipc::TerminalKind::Agent(agent_id),
                None,
                prompt,
                // Autonomous `@lazybox` spawn — launch unattended with
                // permission prompts disabled (subject to the
                // `agent.autonomous_skip_permissions` toggle).
                true,
            )
            .await;
        }
        ProviderAction::AutoFixPr {
            session_key,
            agent_id,
            prompt,
            repo,
            pr_number,
            kind,
            settings,
            reason,
        } => {
            let term_kind = lazybox_ipc::TerminalKind::Agent(agent_id.clone());
            // If a fix agent is ALREADY running on this PR, let it
            // finish — don't burn an attempt, post a duplicate "I'm
            // fixing this" comment, or no-op-spawn on top of it. The
            // trigger (red CI / conflict) persists across polls and a
            // fix can take longer than the cooldown, so without this
            // check a slow agent would silently exhaust the budget +
            // spam the PR while it's actually still working. Uses the
            // SAME singleton definition `handle_spawn` uses to collapse
            // duplicate spawns, so the two stay consistent.
            if let Some(existing) =
                crate::spawn_handler::find_existing_singleton(config, &session_key, &term_kind)
                    .await
            {
                tracing::info!(
                    source = source_name,
                    %session_key,
                    ?kind,
                    ?existing,
                    "auto-fix: agent already running on this PR — skipping (no attempt burned)"
                );
                return;
            }
            // Stateful guard: cooldown + max-attempts, persisted so it
            // survives restarts. Runs HERE (not in the source) because
            // it needs the store, which `TaskSource::fetch` doesn't get.
            let decision = autofix::check_and_record(
                config.store.as_ref(),
                session_key.as_str(),
                kind,
                &settings,
                Utc::now(),
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
                    tracing::info!(
                        source = source_name,
                        %session_key,
                        %agent_id,
                        %reason,
                        attempt,
                        max,
                        "auto-fixing PR (spawning agent)"
                    );
                    // Post the "why work just started" comment BEFORE
                    // spawning so the user sees lazybox's note even if the
                    // agent races to push first. Best-effort: a failed
                    // comment never blocks the fix.
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
                    crate::spawn_handler::handle_spawn(
                        config,
                        session_key,
                        None,
                        term_kind,
                        None,
                        prompt,
                        // Unattended auto-fix spawn — the agent has to
                        // clear the first-run workspace-trust dialog on
                        // the fresh worktree (else the injected fix
                        // prompt lands in the trust chooser) and push a
                        // fix without a human to approve its edits.
                        true,
                    )
                    .await;
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
        let partial = *self
            .last_coverage_partial
            .lock()
            .expect("GhSource.last_coverage_partial mutex poisoned");
        let windowed = *self
            .last_windowed
            .lock()
            .expect("GhSource.last_windowed mutex poisoned");
        gh_polled_scope(
            self.scheduling.run_global,
            &self.scheduling.repos,
            partial,
            windowed,
        )
    }
    fn drain_actions(&self) -> Vec<ProviderAction> {
        let mut guard = self
            .pending_actions
            .lock()
            .expect("GhSource.pending_actions poisoned");
        std::mem::take(&mut *guard)
    }
    /// Tiered fetch (issue #19):
    ///
    /// 1. **Slow full sweep** — heavy `involves:USER` GraphQL search,
    ///    fires every [`GhClient::FULL_SWEEP_INTERVAL`] (default 10 min)
    ///    and on the first tick after daemon start. Rescope runs.
    ///    `@lazybox` mention scanning ONLY happens on this path (the
    ///    full search response is what mention scanning walks).
    /// 2. **Fast notifications heartbeat** — `GET /notifications` with
    ///    `If-Modified-Since`; 304 → return empty `Vec`, no rescope.
    /// 3. **Targeted deep-fetch** — for each modified notification,
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
            // `last_kind` is only consulted by the tick driver in the
            // `Ok` arm (errored sources never reach the all_full
            // check), so we set it inside `Ok` branches only — the
            // value held during an Err is unobservable.
            if self.client.should_full_sweep() {
                let tasks = self.fetch_full().await?;
                self.set_last_kind(FetchMode::Full);
                return Ok(tasks);
            }
            match self.fetch_incremental().await? {
                Some(tasks) => {
                    self.set_last_kind(FetchMode::Incremental);
                    Ok(tasks)
                }
                None => {
                    // Heartbeat failed quietly — fall back to full sweep
                    // rather than silently freezing the inbox. The full
                    // sweep also re-arms the slow-sweep clock so we
                    // don't loop on the same broken heartbeat.
                    tracing::info!("incremental returned None; promoting to full sweep");
                    let tasks = self.fetch_full().await?;
                    self.set_last_kind(FetchMode::Full);
                    Ok(tasks)
                }
            }
        })
    }
    fn last_fetch_kind(&self) -> FetchMode {
        *self
            .last_kind
            .lock()
            .expect("GhSource.last_kind mutex poisoned")
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
    /// `GhSource::last_coverage_partial`).
    last_coverage_partial: std::sync::Mutex<bool>,
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
            last_coverage_partial: std::sync::Mutex::new(false),
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
        let partial = *self
            .last_coverage_partial
            .lock()
            .expect("LinearSource.last_coverage_partial mutex poisoned");
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
            *self
                .last_coverage_partial
                .lock()
                .expect("LinearSource.last_coverage_partial mutex poisoned") = outcome.is_partial();
            self.emit_progress(format!(
                "Got {} issues, applying filters…",
                outcome.tasks.len()
            ));
            let kept = filter_linear_tasks(outcome.tasks, &self.filter);
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
    tasks
        .into_iter()
        .filter(|t| {
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
    viewer_identities: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    gh_client_cache: std::sync::Arc<std::sync::Mutex<Option<GhClient>>>,
) -> Vec<Box<dyn TaskSource>> {
    let mut sources: Vec<Box<dyn TaskSource>> = Vec::new();

    if setup.enabled_providers.contains(lazybox_gh::SOURCE) {
        match lazybox_gh::credential_chain()
            .resolve(lazybox_gh::SOURCE)
            .await
        {
            Ok(cred) => {
                // Reuse the cached client when the credential source
                // is unchanged. `with_filters` consumes Self and
                // returns a new client with refreshed qualifiers —
                // the underlying `Arc<Mutex<RateBudget>>` is cloned,
                // so observations made by previous ticks (or by the
                // GhSource we hand out below) remain visible to the
                // cached copy and vice versa.
                let cred_source = cred.source.clone();
                // Clone the cached client out under a brief std-lock and
                // release before any `.await` — the cache lock must never
                // span the `from_credential` network call (issue #92).
                let cached = gh_client_cache
                    .lock()
                    .expect("gh_client_cache poisoned")
                    .clone()
                    .filter(|c| c.credential_source() == cred_source.as_str());
                // Cap the cold-cache client build. `from_credential`
                // makes an untimed `/user` REST call, and this runs
                // OUTSIDE the 180s tick timeout — without a cap a
                // hung TCP connection here wedges the poll loop
                // forever (no tick ever starts, so no tick timeout
                // ever fires).
                const CLIENT_INIT_TIMEOUT: Duration = Duration::from_secs(15);
                let client_result: Result<GhClient, String> = match cached {
                    Some(existing) => Ok(existing),
                    None => {
                        match tokio::time::timeout(
                            CLIENT_INIT_TIMEOUT,
                            GhClient::from_credential(cred),
                        )
                        .await
                        {
                            Ok(result) => result.map_err(|e| e.to_string()),
                            Err(_) => Err(format!(
                                "client init timed out after {}s",
                                CLIENT_INIT_TIMEOUT.as_secs()
                            )),
                        }
                    }
                };
                match client_result {
                    Ok(client) => {
                        let filter = setup.provider_config("github");
                        let scopes = setup
                            .selected_scopes
                            .get("github")
                            .cloned()
                            .unwrap_or_default();
                        let pr_qualifiers =
                            build_pr_search_qualifiers(&filter, &scopes, client.username());
                        let issue_qualifiers =
                            build_issue_search_qualifiers(&filter, &scopes, client.username());
                        // `with_filters` returns a new owned client
                        // sharing the same budget Arc — `.clone()` on
                        // the result is cheap and keeps the cache in
                        // sync with what GhSource holds.
                        let client = client.with_filters(pr_qualifiers, issue_qualifiers);
                        // Cache + announce the authenticated viewer
                        // login so the TUI can render `@me` for the
                        // local user's bylines. Diffs the cache so we
                        // only broadcast when the value actually
                        // changes (token rotation, credential
                        // refresh, …) — quiet on the steady-state
                        // poll loop.
                        let viewer = client.username().to_string();
                        if !viewer.is_empty() {
                            let mut logins = viewer_identities
                                .lock()
                                .expect("viewer_identities poisoned");
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
                        *gh_client_cache.lock().expect("gh_client_cache poisoned") =
                            Some(client.clone());
                        // Resolve the `@lazybox` allowlist. Empty YAML
                        // list → fall back to "just the authenticated
                        // viewer", which mirrors the design doc's MVP
                        // scope (only the local lazybox user's own
                        // issues + comments count).
                        // Load config ONCE for both the mention
                        // allowlist and the auto-fix settings so we
                        // don't read the YAML twice per tick.
                        let cfg = lazybox_config::Config::load().ok();
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
                        let scheduling = plan_round_robin_tick(
                            &mut state.round_robin,
                            will_full_sweep,
                            DEFAULT_ROUND_ROBIN_N,
                            now,
                        );
                        if will_full_sweep {
                            tracing::info!(
                                source = lazybox_gh::SOURCE,
                                tick = state.round_robin.tick,
                                run_global = scheduling.run_global,
                                round_robin = ?scheduling.repos,
                                known_repos = state.round_robin.cursor.len(),
                                focused = state.round_robin.focused_repo.as_deref().unwrap_or(""),
                                "round-robin scheduling decision"
                            );
                        }
                        sources.push(Box::new(GhSource {
                            client,
                            filter,
                            scopes,
                            bus: bus.clone(),
                            mention_allowed_logins: mention_allowed,
                            auto_fix,
                            pending_actions: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                            scheduling,
                            // Default to Full so a never-fetched
                            // source doesn't accidentally block rescope.
                            last_kind: std::sync::Mutex::new(FetchMode::Full),
                            last_coverage_partial: std::sync::Mutex::new(false),
                            last_windowed: std::sync::Mutex::new(false),
                        }));
                    }
                    Err(e) => tracing::warn!("github client init failed: {e}"),
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
    let throwaway_viewers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let throwaway_client_cache = std::sync::Arc::new(std::sync::Mutex::new(None));
    sources_for(
        &setup,
        bus,
        &mut throwaway_state,
        throwaway_viewers,
        throwaway_client_cache,
    )
    .await
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
}

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
    /// always merge by hand via the future adopt-sessions flow.
    pub(crate) rejected: std::collections::HashSet<String>,
}

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
                if mode == FetchMode::Incremental {
                    all_full = false;
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
                    let mut seen_repos: std::collections::HashSet<&str> =
                        std::collections::HashSet::new();
                    for task in &tasks {
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
                for (i, task) in tasks.into_iter().enumerate() {
                    if task.mergeable == lazybox_core::Mergeable::Unknown {
                        saw_unknown_mergeable = true;
                    }
                    let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
                    let task_id = task.id.to_string();
                    polled.push(key);
                    let one_started = std::time::Instant::now();
                    match tokio::time::timeout(
                        UPSERT_TIMEOUT_PER_TASK,
                        upsert_with_context(config, &mut upsert_ctx, task),
                    )
                    .await
                    {
                        Ok(()) => {
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
                            let _ = config.bus.send(Event::ProviderError {
                                source: source.name().to_string(),
                                message: format!(
                                    "upsert timed out on {task_id} — task skipped this tick"
                                ),
                                detail: "see /tmp/lazybox.log for the slow step".into(),
                                kind: "retryable".into(),
                            });
                        }
                    }
                }
                tracing::info!(
                    "tick: upserted {total} tasks in {}ms",
                    upsert_started.elapsed().as_millis()
                );
                // Clear the debounce slot — the next failure should
                // broadcast even if it carries the same message as a
                // previous run.
                state.last_error.remove(source.name());
                // Always emit `PollCompleted`, even on 0 tasks, so
                // the TUI can distinguish "polling hasn't run yet"
                // from "polling found nothing matching your filter".
                let _ = config.bus.send(Event::PollCompleted {
                    source: source.name().to_string(),
                    count,
                });
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
                let gh = config
                    .gh_client_cache
                    .lock()
                    .expect("gh_client_cache poisoned")
                    .clone();
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
                let kind = if e.is_retryable() {
                    "retryable"
                } else if e.is_auth() {
                    "auth"
                } else {
                    "permanent"
                };
                // Capture the longest retry-after hint across all
                // failing sources this tick. Provider gave us a
                // precise number (GitHub's rateLimit.resetAt) —
                // honor it.
                if let Some(secs) = e.retry_after_secs() {
                    max_retry_after_secs =
                        Some(max_retry_after_secs.map_or(secs, |existing| existing.max(secs)));
                }
                // Debounce: only emit a ProviderError if the message
                // changed since the last failure for this source.
                // Same rate-limit error every minute → one event,
                // not 60/hour.
                let msg = e.user_message();
                let prev = state.last_error.get(source.name());
                if prev.map(String::as_str) != Some(msg.as_str()) {
                    state
                        .last_error
                        .insert(source.name().to_string(), msg.clone());
                    let _ = config.bus.send(Event::ProviderError {
                        source: e.source().to_string(),
                        message: msg,
                        detail: e.diagnostic(),
                        kind: kind.to_string(),
                    });
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
    // an empty result." The user can press Shift-X / Settings →
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

    let records = match config.store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("rescope: list_workspaces failed: {e}");
            return;
        }
    };

    // Per session_key → count of live terminals. Lets us both
    // detect "has active session" and report the count to the user
    // when prompting.
    let terminal_meta = config.terminal_meta.lock().await;
    let mut active_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (sk, _) in terminal_meta.values() {
        *active_counts.entry(sk.as_str().to_string()).or_default() += 1;
    }
    drop(terminal_meta);

    let now = chrono::Utc::now();
    for r in records {
        if polled_set.contains(r.key.as_str()) {
            continue;
        }
        let key = WorkspaceKey::new(r.key.clone());
        // Decode the stored workspace once — used by both the
        // snoozed-skip guard AND the locally-created (no upstream
        // task) guard below.
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
                // Safe to remove silently: nothing's running.
                tracing::info!(
                    workspace_key = %r.key,
                    "rescope: removing out-of-scope workspace"
                );
                // Reap the workspace's worktrees BEFORE the row goes
                // away — once it's deleted, `collect_tracked_sessions`
                // can never find the paths again and the dirs leak
                // forever. Only worktrees the inspector deems safe
                // (clean tree, pushed, no live terminal) are removed;
                // dirty ones are left on disk for manual recovery.
                if let Some(ws) = stored_ws.as_ref() {
                    handlers::reap_safe_workspace_worktrees(config, ws).await;
                }
                // `archive: false` — rescope is a system decision, not
                // user intent. Archiving here would permanently block
                // the workspace from re-creation when the upstream
                // item comes back into scope (truncated query, scope
                // re-add, reopened PR).
                delete_workspace_internal(config, &key, /*archive=*/ false).await;
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
    //    `config.poll_wake.notified()` is selected against the
    //    chunked sleep — pinging the Notify forces an immediate
    //    re-check of the wall-clock condition.
    tokio::spawn(async move {
        use std::time::Instant;
        const CHUNK: Duration = Duration::from_secs(5);
        const UNKNOWN_RETRY: Duration = Duration::from_secs(5);

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
                    _ = config.poll_wake.notified() => {
                        tracing::info!("polling: woken (Refresh / Subscribe / UNKNOWN retry)");
                        break;
                    }
                }
            }

            tick_n += 1;
            tracing::info!("polling: tick #{tick_n} starting");

            // Tolerate panics inside `run_one_tick`. tokio swallows
            // panics from spawned tasks by default; without this
            // wrapper a single buggy poll cycle would silently kill
            // the entire long-lived loop, leaving CI/mergeable badges
            // frozen until daemon restart. Caught panics get logged
            // at error level + the loop continues with a normal
            // interval — degraded behaviour is far better than
            // silent death.
            let summary = match std::panic::AssertUnwindSafe(run_one_tick(&config))
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
                    let _ = config.bus.send(Event::ProviderError {
                        source: "github".into(),
                        message: format!("poll cycle crashed: {msg}"),
                        detail: msg,
                        kind: "retryable".into(),
                    });
                    TickSummary::default()
                }
            };
            tracing::info!(
                "polling: tick #{tick_n} done (path={}, retry_after={:?}, unknown_mergeable={})",
                if summary.all_full {
                    "full-sweep"
                } else {
                    "incremental"
                },
                summary.retry_after_secs,
                summary.saw_unknown_mergeable,
            );

            // Base cadence is `interval`. If a provider reported a
            // rate-limit reset window, use whichever is longer.
            let mut next_in = match summary.retry_after_secs {
                Some(secs) => interval.max(Duration::from_secs(secs)),
                None => interval,
            };
            if summary.retry_after_secs.is_some() {
                tracing::warn!(
                    "polling: backing off {}s before next tick (rate-limit hint)",
                    next_in.as_secs(),
                );
            }
            // GitHub returns `mergeable: UNKNOWN` while it computes
            // mergeability in the background. The second query
            // (issued ~5s later) almost always returns the real
            // value, so override the cadence in that case.
            if summary.saw_unknown_mergeable && UNKNOWN_RETRY < next_in {
                tracing::info!(
                    "polling: re-firing in {}s to chase UNKNOWN mergeable",
                    UNKNOWN_RETRY.as_secs(),
                );
                next_in = UNKNOWN_RETRY;
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
                    _ = config.poll_wake.notified() => { break; }
                }
            }
            let outcome = tick_with_state(&config, &sources, &mut state).await;
            let retry_after = outcome.retry_after_secs;
            let saw_unknown = outcome.saw_unknown_mergeable;
            rescope_with_state(&config, &outcome, &mut state).await;
            let mut next_in = match retry_after {
                Some(secs) => interval.max(Duration::from_secs(secs)),
                None => interval,
            };
            if saw_unknown && UNKNOWN_RETRY < next_in {
                next_in = UNKNOWN_RETRY;
            }
            next_due = Instant::now() + next_in;
        }
    })
}

/// Single iteration of the poll loop. Loads the latest persisted
/// setup, builds sources, ticks, rescopes. Shared between the
/// long-lived spawn and the `Command::Refresh` immediate-tick path.
/// Uses `config.poll_state` so prompt-dismissal memory crosses both
/// paths.
/// Summary of one tick — what the driver loop needs to schedule
/// the next one. `retry_after_secs` extends the next sleep when a
/// provider reported a rate-limit reset window; `saw_unknown_mergeable`
/// triggers a quick re-poll so GitHub's lazy mergeability landing
/// doesn't have to wait out the full interval.
#[derive(Debug, Default, Clone, Copy)]
pub struct TickSummary {
    pub retry_after_secs: Option<u64>,
    pub saw_unknown_mergeable: bool,
    /// True when every successful source ran a full sweep (no source
    /// took the incremental notifications path). Surfaced in the
    /// driver's per-tick log so the delivery path of a slow update is
    /// visible without cross-referencing per-source lines.
    pub all_full: bool,
}

/// Check the cross-tick [`TickState`] OUT of `config.poll_state`,
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
pub async fn checkout_poll_state(config: &ServerConfig) -> TickState {
    std::mem::take(&mut *config.poll_state.lock().await)
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
pub async fn restore_poll_state(config: &ServerConfig, mut state: TickState) {
    let mut guard = config.poll_state.lock().await;
    if guard.round_robin.focused_repo.is_some() {
        state.round_robin.focused_repo = guard.round_robin.focused_repo.take();
    }
    *guard = state;
}

pub async fn run_one_tick(config: &ServerConfig) -> TickSummary {
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
    let mut state = checkout_poll_state(config).await;
    let summary = run_tick_inner(config, &setup, &mut state).await;
    restore_poll_state(config, state).await;
    summary
}

/// The body of one poll tick, operating on a `state` checked out of
/// `poll_state`. Builds the source list, ticks, rescopes. Split out of
/// `run_one_tick` so the checkout/restore of `poll_state` brackets it
/// cleanly (see `checkout_poll_state`).
async fn run_tick_inner(
    config: &ServerConfig,
    setup: &lazybox_core::PersistedSetup,
    state: &mut TickState,
) -> TickSummary {
    let sources = sources_for(
        setup,
        config.bus.clone(),
        state,
        config.viewer_identities.clone(),
        config.gh_client_cache.clone(),
    )
    .await;
    if sources.is_empty() {
        // User disabled every provider (or credentials all
        // failed to resolve). Treat as "deliberately empty
        // result" — rescope so existing workspaces actually
        // disappear from the sidebar. Without this, unchecking
        // every provider leaves the inbox frozen with stale
        // rows that no current poll source could produce.
        // No-sources path counts as a complete view of "what should be
        // here" — leave `all_full = true` so rescope removes orphaned
        // workspaces from the disabled providers.
        let outcome = TickOutcome {
            polled: vec![],
            any_source_succeeded: true,
            retry_after_secs: None,
            saw_unknown_mergeable: false,
            source_scopes: std::collections::HashMap::new(),
            all_full: true,
        };
        rescope_with_state(config, &outcome, state).await;
        return TickSummary::default();
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
            let _ = config.bus.send(Event::ProviderError {
                source: "github".into(),
                message: format!(
                    "sync exceeded {}s — see /tmp/lazybox.log for the slow step",
                    TICK_OVERALL_TIMEOUT.as_secs()
                ),
                detail: "the per-upsert / per-graphql / per-git timeouts should catch this; \
                         hitting the outer cap means something escaped them"
                    .into(),
                kind: "retryable".into(),
            });
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
    };
    rescope_with_state(config, &outcome, state).await;
    // (Prefetch of top-N PR details — implemented but disabled until
    // the underlying sync stability work lands. The spawn re-locked
    // `poll_state`; with the tick no longer holding it across the
    // upsert loop (#133) that is no longer a deadlock risk, but
    // layering "background fetch fan-out" on top of "single sync is
    // fragile" was the wrong order. Re-enable once the next-tick
    // cadence is visibly healthy in `/tmp/lazybox.log`.)
    summary
}

/// Merge `task` into the existing workspace for its workspace key
/// (PR + matching issues collapse to one row), persist it, and
/// broadcast `WorkspaceUpserted`. The store backs this with
/// `workspace:<key>` rows in the kv table — no schema migration
/// needed.
///
/// Read state (`seen_count`, `read_indices`, `snoozed_until`,
/// `last_viewed_at`) is preserved across updates — providers only
/// own upstream-derived fields.
///
/// ## Issue → PR collapsing
///
/// GitHub PRs can link to issues via `closingIssuesReferences` (the
/// canonical "Closes #N" / "Fixes #N" mapping). When we observe a PR
/// whose closes_issues lists an issue that already lives in its own
/// standalone workspace, we **merge** the issue workspace into the
/// PR workspace — moving its sessions over (terminals keep running)
/// and dropping the standalone row. Conversely, when an issue is
/// polled and a PR already claims it, we route the issue update into
/// the PR workspace instead of building a duplicate.
pub async fn upsert(config: &ServerConfig, task: Task) {
    let mut ctx = UpsertContext::build(config);
    upsert_with_context(config, &mut ctx, task).await;
}

/// Per-tick context shared across a batch of `upsert` calls. Both
/// fields used to be re-derived from the store on EVERY task — a
/// KV read + JSON parse for the archive set, and a full workspace-
/// list deserialize for the closes-issue lookup — which made the
/// upsert loop's cost quadratic in inbox size. The tick builds this
/// once and threads it through; the one-off public `upsert` builds a
/// fresh context per call (identical behavior, just not batched).
struct UpsertContext {
    /// Workspace keys the user explicitly archived (`Shift-X`).
    archived: std::collections::HashSet<String>,
    /// `closes_issues` TaskId → claiming PR workspace key. Mirrors
    /// what `pr_workspace_claiming_issue` derives per call. Updated
    /// inline as PR tasks flow through the batch so an issue polled
    /// AFTER its PR in the same tick still routes correctly.
    closes_index: std::collections::HashMap<lazybox_core::TaskId, WorkspaceKey>,
}

impl UpsertContext {
    fn build(config: &ServerConfig) -> Self {
        let archived = load_archived_set(config);
        let mut closes_index = std::collections::HashMap::new();
        if let Ok(records) = config.store.list_workspaces() {
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
                for id in &pr.closes_issues {
                    closes_index
                        .entry(id.clone())
                        .or_insert_with(|| ws.key.clone());
                }
            }
        }
        Self {
            archived,
            closes_index,
        }
    }
}

async fn upsert_with_context(config: &ServerConfig, ctx: &mut UpsertContext, task: Task) {
    // Skip re-creating workspaces the user explicitly archived
    // (`Shift-X`). Without this, every 60s tick re-creates the row
    // from the upstream task and the dismiss feels broken. Cached
    // archive set lives in the store under KV_KEY_ARCHIVED.
    let candidate_key = lazybox_core::workspace_key_for(&task);
    if ctx.archived.contains(&candidate_key) {
        tracing::debug!(
            workspace_key = %candidate_key,
            "upsert: skipping archived workspace"
        );
        return;
    }

    if is_pr_task(&task) {
        // Keep the per-tick index current: a PR carrying closing refs
        // claims those issues for the rest of this batch, so an issue
        // task later in the same tick routes into this PR workspace
        // instead of building a standalone row.
        for id in &task.closes_issues {
            ctx.closes_index
                .entry(id.clone())
                .or_insert_with(|| WorkspaceKey::new(candidate_key.clone()));
        }
    } else {
        // For issues: if a PR somewhere already claims this issue as
        // closed-by, route the upsert into that PR workspace. This is
        // the "issue polled AFTER its PR" path. We only kick in when
        // the issue has no standalone workspace yet — once one exists,
        // either the PR poll will collapse them or the issue's own row
        // remains until the PR shows up.
        let issue_key = WorkspaceKey::new(candidate_key.clone());
        let already_standalone = config
            .store
            .get_workspace(&issue_key)
            .ok()
            .flatten()
            .is_some();
        if !already_standalone && let Some(pr_key) = ctx.closes_index.get(&task.id).cloned() {
            tracing::info!(
                issue = %task.id,
                pr_workspace = %pr_key,
                "routing issue upsert into PR workspace (closingIssuesReferences)"
            );
            upsert_into_workspace_key(config, &pr_key, task).await;
            return;
        }
    }

    let key = WorkspaceKey::new(candidate_key);
    upsert_into_workspace_key(config, &key, task).await;
}

/// Inner upsert: load workspace at `key`, attach the task, migrate
/// linked-issue workspaces if the task is a PR with closing refs,
/// then persist + broadcast. Split out from `upsert` so the
/// "route to PR workspace" path can reuse the same write/broadcast
/// behaviour without duplicating it.
async fn upsert_into_workspace_key(config: &ServerConfig, key: &WorkspaceKey, task: Task) {
    // 0. MERGE DETECTION: cheap pre-check (no IO) gates the store
    //    read — only a PR observed as Merged can trigger cleanup. We
    //    snapshot the previous state here, before `prepare_upsert`
    //    overwrites it, so we only act on the open→merged *transition*
    //    and not on every subsequent tick of an already-merged PR.
    let merged_pr_to_clean = if task.is_pr() && task.state == lazybox_core::TaskState::Merged {
        let prev = load_workspace(config, key);
        // A merged PR we have no workspace for is a recently-merged
        // sweep result (`is:merged` last 7d) for a PR the user never
        // tracked or already dismissed. The sweep exists only to
        // back-fill the final MERGED state onto workspaces we already
        // have — creating a fresh row here re-surfaces stale merged PRs
        // into the inbox on every full sweep, and the issue-collapse
        // pass in `prepare_upsert` would then fold the user's active
        // issue workspaces (`Closes #N`) into that brand-new row,
        // wiping in-progress work from the sidebar on a manual Shift-R
        // sync (#64). First-discovery of an already-merged PR has no
        // sessions to reap either, which is why
        // `merged_transition_pr_number` already declines to act on it.
        if prev.is_none() {
            tracing::debug!(
                workspace_key = %key.as_str(),
                "upsert: skipping merged PR with no existing workspace (recently-merged sweep back-fill only)"
            );
            return;
        }
        merged_transition_pr_number(prev.as_ref(), &task)
    } else {
        None
    };

    // 1. PREPARE: build the workspace's final in-memory state.
    //    Includes the optional issue-collapse merge — if a PR
    //    polls in with `closes_issues`, we fold standalone issue
    //    workspaces into it here. Async (touches the store +
    //    `terminal_meta`) but doesn't yet write the PR's own row.
    let (mut workspace, pending_merges) = prepare_upsert(config, key, task).await;

    // 2. MIGRATE: rename worktree dirs to match the (possibly
    //    new) PR slug. Async git operation. If it fails, log
    //    loudly but continue to commit the metadata — the next
    //    spawn re-provisions paths and a partial mismatch is
    //    survivable; a missing broadcast is not.
    crate::spawn_handler::migrate_session_paths_if_needed(config, &mut workspace).await;

    // 3. COMMIT: persist the final state + broadcast it. Failures
    //    here log at `error` so an operator can spot a workspace
    //    that won't survive restart. Issue rows absorbed by the
    //    merge are deleted only AFTER this commit lands — see
    //    `finalize_issue_merges` for why the order matters.
    commit_upsert(config, key, workspace);
    finalize_issue_merges(config, key, pending_merges);

    // 4. MERGED: the PR just merged → either reap its safe-to-delete
    //    worktrees silently (when `worktree.auto_cleanup_merged` is
    //    on) or prompt the user to remove the workspace + worktree
    //    (the default). Runs after the commit so it re-reads the
    //    freshly persisted session set.
    if let Some(pr_number) = merged_pr_to_clean {
        handlers::on_merged_pr(config, key, pr_number).await;
    }
}

/// PR number parsed off a task id (`owner/repo#123` → `123`). Mirrors
/// the `#`-split [`Workspace::worktree_slug`] uses. `None` for issues
/// or any id whose suffix isn't a number.
fn pr_number_from_task(task: &Task) -> Option<u64> {
    task.id
        .key
        .rsplit_once('#')
        .and_then(|(_, n)| n.parse().ok())
}

/// Decide whether `task` represents a PR that *just* transitioned into
/// the merged state, returning its number when so.
///
/// Requires a known predecessor that was **not** already merged — a
/// genuine open→merged flip. Two reasons this is stricter than "is the
/// incoming task merged?":
/// - It's a one-shot guard: once the merged state is persisted, the
///   next poll sees `prev` already merged and skips, so cleanup runs
///   exactly once per merge.
/// - A PR first discovered already-merged has no prior workspace and
///   thus no sessions to reap, so firing would only burn an inspect
///   sweep for nothing.
fn merged_transition_pr_number(prev: Option<&Workspace>, task: &Task) -> Option<u64> {
    if !task.is_pr() || task.state != lazybox_core::TaskState::Merged {
        return None;
    }
    let prev_state = prev.and_then(|w| w.task_by_id(&task.id))?.state;
    if prev_state == lazybox_core::TaskState::Merged {
        return None;
    }
    pr_number_from_task(task)
}

/// Pure-ish prepare step: load the existing workspace (if any),
/// attach the incoming task, and run the issue-collapse merge. No
/// store writes, no `WorkspaceUpserted` broadcast — the returned
/// `Workspace` is what we'll commit in step 3.
///
/// Split out from `upsert_into_workspace_key` so a future test can
/// drive the prepare step against a mock store without committing
/// real state — the "did the merge attach the issue task?" question
/// is now answerable without the full IPC bus + store side effects.
async fn prepare_upsert(
    config: &ServerConfig,
    key: &WorkspaceKey,
    task: Task,
) -> (Workspace, Vec<PendingIssueMerge>) {
    let existing = config
        .store
        .get_workspace(key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
        .and_then(|j| serde_json::from_str::<Workspace>(&j).ok());

    // Sync-latency probe: when this task already lives in the workspace
    // and the incoming copy is genuinely fresher, log how stale it was
    // by the time we processed it (`now - task.updated_at`). This is the
    // end-to-end "an update took a long time to appear" signal — large
    // values point at a slow delivery path (full-sweep cadence) rather
    // than a slow upsert. First-discovery (no existing task) is skipped:
    // its age reflects the PR's history, not delivery latency.
    if let Some(prev) = existing.as_ref().and_then(|w| w.task_by_id(&task.id))
        && task.updated_at > prev.updated_at
    {
        let age_ms = (Utc::now() - task.updated_at).num_milliseconds().max(0);
        tracing::info!(
            task = %task.id,
            workspace_key = %key.as_str(),
            update_age_ms = age_ms,
            "sync: delivered fresher task"
        );
    }

    let mut workspace = match existing {
        Some(mut w) => {
            w.attach_task(task);
            w
        }
        None => Workspace::from_task(task, Utc::now()),
    };

    // Issue-collapse pass — see `merge_closing_issue_workspaces`.
    // Happens here (in prepare) so the migration step sees the
    // final session set and renames worktrees in one pass.
    let pending_merges = merge_closing_issue_workspaces(config, &mut workspace).await;
    (workspace, pending_merges)
}

/// Side-effect-only commit: serialize + persist + broadcast.
/// Pulled out so the failure modes are isolated — a store-write
/// error doesn't suppress the bus broadcast, and a bus-send error
/// doesn't take down the daemon.
///
/// Also ensures the Project this workspace belongs to is registered.
/// `Workspace::from_task` populates `project_key` from the task's
/// repo string; we use that here to upsert a Project record so the
/// sidebar can render a header for it even before the user explicitly
/// creates the project. Idempotent — re-broadcasting an existing
/// record on every workspace upsert costs a bus send but keeps the
/// data model consistent with no extra bookkeeping.
pub(super) fn commit_upsert(config: &ServerConfig, key: &WorkspaceKey, workspace: Workspace) {
    ensure_project_for_workspace(config, &workspace);
    // Serialization failure here means the workspace exists in memory
    // but won't survive a restart — and the silent `.ok()` previously
    // stored `None`, so the next process would read back an empty
    // record without any indication something went wrong. Log loudly
    // so a broken Serialize impl shows up in /tmp/lazybox.log instead
    // of mysterious post-restart data loss.
    let json = match serde_json::to_string(&workspace) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                workspace_key = %key.as_str(),
                "commit_upsert: serde_json::to_string(workspace) failed: {e} \
                 — record will persist with NULL json (will read back empty)",
            );
            None
        }
    };
    // No-change short-circuit: the steady-state poll re-commits every
    // workspace every tick even when the upstream task is byte-for-byte
    // identical. Comparing against the stored serialization skips both
    // the SQLite write and the `WorkspaceUpserted` broadcast (every
    // connected client re-renders on that event) when nothing changed.
    // Byte equality only — any real change (task fields, read state,
    // sessions) produces different JSON and flows through normally.
    if let Some(new_json) = json.as_deref() {
        let unchanged = config
            .store
            .get_workspace(key)
            .ok()
            .flatten()
            .and_then(|r| r.workspace_json)
            .is_some_and(|prev| prev == new_json);
        if unchanged {
            tracing::trace!(
                workspace_key = %key.as_str(),
                "commit_upsert: unchanged — skipping write + broadcast"
            );
            return;
        }
    }
    let record = WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: json,
    };
    if let Err(e) = config.store.save_workspace(&record) {
        // Bumped to error: a store write failure means the
        // workspace we just broadcast won't survive a restart.
        // Caller side can't currently see this, but at least the
        // log is loud.
        tracing::error!(
            workspace_key = %record.key,
            "save_workspace failed: {e}"
        );
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
}

/// Ensure a Project record exists for the workspace's parent project,
/// upserting + broadcasting on first sight. Driven from `commit_upsert`,
/// so every workspace that flows through polling auto-registers its
/// containing project. Idempotent — calling repeatedly with the same
/// workspace re-saves and re-broadcasts the same Project record (cheap).
///
/// `Workspace::project_key` is populated by `Workspace::from_task` via
/// `lazybox_core::project_key_for_task`. When it's `None` (back-compat
/// reads of pre-Project records, or a workspace with no upstream task),
/// we skip — Stage 1 doesn't try to back-fill projects for orphan
/// workspaces.
fn ensure_project_for_workspace(config: &ServerConfig, workspace: &Workspace) {
    let Some(project_key) = workspace.project_key.clone() else {
        return;
    };
    // Skip the write + broadcast if we've already registered this
    // project. Keeps bus traffic to one event per project per process
    // — without this, every workspace upsert would re-fire the project
    // event and consumers that drain "one event per upsert" would
    // desync (mark_workspace_read in particular).
    if matches!(config.store.get_project(&project_key), Ok(Some(_))) {
        return;
    }
    // Display name for the project. Prefer the workspace's
    // `primary_task().repo` (the "owner/repo" string) when present —
    // that's what the sidebar header has always shown. Fall back to
    // the key-derived name (`github-owner-repo` → `owner/repo`) rather
    // than the raw key string.
    let name = workspace
        .primary_task()
        .and_then(|t| t.repo.clone())
        .unwrap_or_else(|| project_key.display_name());
    let project = lazybox_core::Project::new(project_key.clone(), name, Utc::now());
    let json = match serde_json::to_string(&project) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                project_key = %project_key,
                "ensure_project: serde_json::to_string(project) failed: {e}",
            );
            None
        }
    };
    let record = lazybox_store::ProjectRecord {
        key: project_key.as_str().to_string(),
        created_at: project.created_at,
        project_json: json,
    };
    if let Err(e) = config.store.save_project(&record) {
        tracing::error!(
            project_key = %record.key,
            "save_project failed: {e}",
        );
    }
    let _ = config.bus.send(Event::ProjectUpserted(Box::new(project)));
}

/// Heuristic for "is this Task the PR side of a PR/issue pair?".
/// Single source of truth: [`Task::is_pr`] — same method
/// `workspace::classify` consults — so adding a new provider
/// (GitLab `/merge_requests/`, Bitbucket `/pull-requests/`, …)
/// only requires extending that one method.
fn is_pr_task(task: &Task) -> bool {
    task.is_pr()
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

/// An issue workspace absorbed into a PR during
/// [`merge_closing_issue_workspaces`], whose store row must still be
/// deleted. Deletion is deferred to [`finalize_issue_merges`] — AFTER
/// the PR's own commit — so a crash or timeout between the absorb and
/// the commit can't leave the moved sessions in neither stored
/// workspace.
struct PendingIssueMerge {
    issue_key: WorkspaceKey,
    issue_label: String,
    pr_label: String,
}

/// Issue id a lazybox-named branch implies. Issue spawns check out
/// `lazybox/issue-<n>` (see `spawn_handler::derive_branch_for_branchless`),
/// so a PR opened from that worktree closes issue `<repo>#<n>` even
/// when neither GitHub's `closingIssuesReferences` nor the body text
/// says so yet (the agent forgot the "Closes #N" line, or the lazy
/// details fetch hasn't run). Used as an extra collapse candidate —
/// the "target must be an ISSUE workspace" filter downstream keeps a
/// false positive harmless.
fn issue_id_from_branch(pr: &Task) -> Option<lazybox_core::TaskId> {
    let number = pr
        .branch
        .as_deref()?
        .strip_prefix("lazybox/issue-")
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))?;
    let repo = pr.repo.as_deref().filter(|r| !r.is_empty())?;
    Some(lazybox_core::TaskId {
        source: pr.id.source.clone(),
        key: format!("{repo}#{number}"),
    })
}

async fn merge_closing_issue_workspaces(
    config: &ServerConfig,
    workspace: &mut Workspace,
) -> Vec<PendingIssueMerge> {
    let mut pending: Vec<PendingIssueMerge> = Vec::new();
    let Some(pr) = workspace.pr.as_ref() else {
        return pending;
    };
    let mut closed_ids: Vec<lazybox_core::TaskId> = pr.closes_issues.clone();
    // Branch-name fallback: a `lazybox/issue-N` head branch claims
    // issue #N even before the closing refs are known.
    if let Some(id) = issue_id_from_branch(pr)
        && !closed_ids.contains(&id)
    {
        closed_ids.push(id);
    }
    if closed_ids.is_empty() {
        tracing::trace!(
            workspace = %workspace.key,
            "merge: PR has no closes_issues — nothing to fold"
        );
        return pending;
    }
    tracing::debug!(
        workspace = %workspace.key,
        candidates = ?closed_ids.iter().map(|t| &t.key).collect::<Vec<_>>(),
        "merge: scanning closes_issues for collapse candidates"
    );

    closed_ids.dedup();

    for issue_id in closed_ids {
        let issue_key = issue_id_to_workspace_key(&issue_id);
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

        // Live-terminal safety net: stall and prompt rather than
        // silently absorbing the user's running work. The gate is
        // LIVE terminals (`terminal_meta`), not session records: a
        // session whose PTY died long ago is just metadata, and
        // prompting on it would park the auto-transfer behind a modal
        // forever (re-fired every 5 min, never completing unattended).
        // Dead session records move over silently with the merge.
        // `merge_prompts` dedupes so a user staring at the modal
        // doesn't see fresh copies every 60s; its `rejected` set is
        // the "no, leave them separate" pin until lazybox restarts.
        let live_terminals = handlers::count_live_terminals(config, &issue_key).await;
        if live_terminals > 0 {
            let issue_key_str = issue_key.as_str().to_string();
            let should_prompt = {
                let mut prompts = config.merge_prompts.lock().await;
                if prompts.rejected.contains(&issue_key_str) {
                    false
                } else {
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
                }
            };
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
        // the issue's store row is deleted by `finalize_issue_merges`
        // only after the PR commit lands, so a cancellation between
        // the two never strands the moved sessions row-less.
        let issue_label = workspace_label_for(&issue_ws, &issue_key);
        let pr_label = workspace_label_for(workspace, &workspace.key);
        absorb_issue_workspace(config, workspace, issue_ws).await;
        pending.push(PendingIssueMerge {
            issue_key: issue_key.clone(),
            issue_label,
            pr_label,
        });

        tracing::info!(
            issue_workspace = %issue_key,
            pr_workspace = %workspace.key,
            "merged issue workspace into PR (closingIssuesReferences)"
        );
    }
    pending
}

/// Second half of the silent issue→PR merge: drop the absorbed issue
/// rows + broadcast the removal/merge notices. MUST run after
/// `commit_upsert` persisted the PR (now carrying the moved sessions)
/// — the auto path used to delete first, and a cancellation (e.g. the
/// 15s per-task upsert timeout) between the delete and the commit
/// left the sessions in neither stored workspace. Commit-then-delete
/// keeps them recoverable under the PR key throughout the window,
/// matching what `handle_confirm_merge` always did.
fn finalize_issue_merges(
    config: &ServerConfig,
    pr_key: &WorkspaceKey,
    pending: Vec<PendingIssueMerge>,
) {
    for merge in pending {
        if let Err(e) = config.store.delete_workspace(&merge.issue_key) {
            tracing::warn!(
                issue_workspace = %merge.issue_key,
                "delete_workspace during PR merge failed: {e}"
            );
        }
        let _ = config
            .bus
            .send(Event::WorkspaceRemoved(merge.issue_key.clone()));
        let _ = config.bus.send(Event::WorkspaceMerged {
            issue_workspace_key: merge.issue_key,
            pr_workspace_key: pr_key.clone(),
            issue_label: merge.issue_label,
            pr_label: merge.pr_label,
        });
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
    let Some(mut workspace) = load_workspace(config, key) else {
        return;
    };
    let pending = merge_closing_issue_workspaces(config, &mut workspace).await;
    if pending.is_empty() {
        // Nothing absorbed (no candidates, or a live-terminal prompt
        // fired instead) — skip the redundant commit + broadcast.
        return;
    }
    crate::spawn_handler::migrate_session_paths_if_needed(config, &mut workspace).await;
    commit_upsert(config, key, workspace);
    finalize_issue_merges(config, key, pending);
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
        let mut prompts = config.merge_prompts.lock().await;
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

    absorb_issue_workspace(config, &mut pr_ws, issue_ws).await;
    crate::spawn_handler::migrate_session_paths_if_needed(config, &mut pr_ws).await;

    // Persist the PR — now carrying the moved sessions — BEFORE dropping
    // the issue record. If we deleted first and crashed before the
    // commit, the session would exist in neither stored workspace and be
    // lost on the next start. Commit-then-delete leaves the session
    // recoverable under the PR key throughout the window.
    let pr_key = pr_ws.key.clone();
    commit_upsert(config, &pr_key, pr_ws);

    if let Err(e) = config.store.delete_workspace(&issue_workspace_key) {
        tracing::warn!(
            issue_workspace = %issue_workspace_key,
            "delete_workspace during ConfirmMerge failed: {e}"
        );
    }
    let _ = config
        .bus
        .send(Event::WorkspaceRemoved(issue_workspace_key.clone()));
    let _ = config.bus.send(Event::WorkspaceMerged {
        issue_workspace_key,
        pr_workspace_key: pr_key.clone(),
        issue_label,
        pr_label,
    });
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
        let mut prompts = config.merge_prompts.lock().await;
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
/// wire-side traffic follows them durably (see `rebadge_terminals`).
/// Unlike the issue→PR merge, we do NOT delete the source workspace
/// — the user may still want it as a tracking row (or remove it
/// explicitly via `Shift-X`).
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

    let source_session_key: lazybox_core::SessionKey = (&source_key).into();
    let target_session_key: lazybox_core::SessionKey = (&target_key).into();
    let moved = source_ws.sessions.len();
    for mut session in source_ws.sessions.drain(..) {
        session.workspace_key = target_key.clone();
        target_ws.add_session(session);
    }
    rebadge_terminals(config, &source_session_key, &target_session_key).await;

    crate::spawn_handler::migrate_session_paths_if_needed(config, &mut target_ws).await;

    tracing::info!(
        source_workspace = %source_key,
        target_workspace = %target_key,
        moved,
        "adopted sessions across workspaces"
    );

    let source_key_owned = source_ws.key.clone();
    let target_key_owned = target_ws.key.clone();
    commit_upsert(config, &source_key_owned, source_ws);
    commit_upsert(config, &target_key_owned, target_ws);
}

/// Move `issue_ws`'s sessions, gh/linear-issue tasks, and any
/// terminal_meta entries onto `pr_workspace`. Caller is responsible
/// for deleting the issue workspace from the store and broadcasting
/// the `WorkspaceRemoved` / `WorkspaceUpserted` / `WorkspaceMerged`
/// events around the call.
async fn absorb_issue_workspace(
    config: &ServerConfig,
    pr_workspace: &mut Workspace,
    issue_ws: Workspace,
) {
    let issue_session_key: lazybox_core::SessionKey = (&issue_ws.key).into();
    let pr_session_key: lazybox_core::SessionKey = (&pr_workspace.key).into();

    for mut session in issue_ws.sessions {
        session.workspace_key = pr_workspace.key.clone();
        pr_workspace.add_session(session);
    }
    for issue_task in &issue_ws.gh_issues {
        pr_workspace.attach_task(issue_task.clone());
    }
    for issue_task in &issue_ws.linear_issues {
        pr_workspace.attach_task(issue_task.clone());
    }

    rebadge_terminals(config, &issue_session_key, &pr_session_key).await;
}

/// Repoint every terminal currently keyed to `from` so it belongs to
/// `to` — both the in-memory `terminal_meta` map AND the persisted
/// `terminal:{backend_key}` record `recover_sessions` reads at startup.
///
/// The persisted record is the easy one to forget: rebadging only the
/// in-memory map leaves a live PTY attached to its new workspace until
/// the next daemon restart, at which point `recover_sessions` reattaches
/// it under the OLD session_key. After an issue→PR collapse that old
/// workspace is gone, so the session is orphaned — the "session lost on
/// merge" bug. Updating both keeps the handoff durable across restarts.
async fn rebadge_terminals(
    config: &ServerConfig,
    from: &lazybox_core::SessionKey,
    to: &lazybox_core::SessionKey,
) {
    let rebadged: Vec<(lazybox_ipc::TerminalId, lazybox_ipc::TerminalKind)> = {
        let mut meta = config.terminal_meta.lock().await;
        meta.iter_mut()
            .filter(|(_, entry)| entry.0 == *from)
            .map(|(tid, entry)| {
                entry.0 = to.clone();
                (*tid, entry.1.clone())
            })
            .collect()
    };
    if rebadged.is_empty() {
        return;
    }
    let backend_keys: Vec<(String, lazybox_ipc::TerminalKind)> = {
        let terminals = config.terminals.lock().await;
        rebadged
            .into_iter()
            .filter_map(|(tid, kind)| terminals.get(&tid).map(|bk| (bk.clone(), kind)))
            .collect()
    };
    for (backend_key, kind) in &backend_keys {
        crate::spawn_handler::persist_terminal_meta(config, backend_key, to, kind).await;
    }
    // Tell live TUI clients to re-point their terminal slots. This must
    // reach them before the `WorkspaceRemoved` that follows an issue→PR
    // collapse — otherwise the terminal stack drops the moved slots
    // (they still carry `from`'s key) and the session disappears.
    let _ = config.bus.send(Event::TerminalsRebadged {
        from: from.clone(),
        to: to.clone(),
    });
}

/// Synthesize the workspace key an issue TaskId would have produced
/// when first upserted as a standalone workspace.
fn issue_id_to_workspace_key(issue_id: &lazybox_core::TaskId) -> WorkspaceKey {
    let stub = Task {
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
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Mergeable,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
    };
    WorkspaceKey::new(lazybox_core::workspace_key_for(&stub))
}

pub(super) fn load_workspace(config: &ServerConfig, key: &WorkspaceKey) -> Option<Workspace> {
    let record = config.store.get_workspace(key).ok().flatten()?;
    let json = record.workspace_json?;
    serde_json::from_str::<Workspace>(&json).ok()
}

/// Record the user's "I'm looking at this workspace" hint on the
/// round-robin scheduler. The next `pick_repos_for_tick` call reads
/// it and bumps the repo to the front of the rotation so a comment
/// landing on the visible PR shows up next cycle instead of waiting
/// its turn.
///
/// No-op when:
/// - the workspace is missing from the store (race with a delete);
/// - the workspace has no primary task (locally-created pre-PR
///   sandbox);
/// - the primary task isn't a GitHub item (Linear doesn't share the
///   per-repo fan-out model);
/// - the primary task has no usable repo string.
///
/// The hint is *replaced*, not accumulated — only the most-recent
/// focus matters; older selections age out via stalest-first ordering.
pub async fn set_focused_workspace(config: &ServerConfig, key: &WorkspaceKey) {
    let Some(workspace) = load_workspace(config, key) else {
        return;
    };
    let Some(task) = workspace.primary_task() else {
        return;
    };
    if task.id.source != lazybox_gh::SOURCE {
        return;
    }
    let Some(repo) = task
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
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
    match config.poll_state.try_lock() {
        Ok(mut state) => {
            let prev = state.round_robin.focused_repo.replace(repo.to_string());
            if prev.as_deref() != Some(repo) {
                tracing::debug!(
                    workspace_key = %key.as_str(),
                    repo,
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
}

/// `owner/repo#N` for PR / issue rows; falls back to the workspace
/// key string otherwise. Used in the confirm modal + footer notice.
fn workspace_label_for(workspace: &Workspace, key: &WorkspaceKey) -> String {
    workspace
        .primary_task()
        .map(|t| t.id.key.clone())
        .unwrap_or_else(|| key.as_str().to_string())
}

/// Create an empty workspace (no PR, no issues) named by the user.
/// Generates a `WorkspaceKey` from the name's slug, disambiguating
/// with a numeric suffix if a workspace with that key already
/// exists. Persists + broadcasts `WorkspaceUpserted`.
///
/// Returns the new key so the caller (sidebar, tests) can land the
/// cursor on the freshly-created row.
pub fn create_empty_workspace(
    config: &ServerConfig,
    name: &str,
    project_key: lazybox_core::ProjectKey,
) -> WorkspaceKey {
    let base = lazybox_core::slug::slugify(name);
    let base = if base.is_empty() {
        "workspace".to_string()
    } else {
        base
    };
    // Collision: try `<base>`, `<base>-2`, `<base>-3`, ... until the
    // store reports no existing record.
    let key = (1..)
        .map(|i| {
            if i == 1 {
                WorkspaceKey::new(base.clone())
            } else {
                WorkspaceKey::new(format!("{base}-{i}"))
            }
        })
        .find(|k| {
            config
                .store
                .get_workspace(k)
                .ok()
                .flatten()
                .and_then(|r| r.workspace_json)
                .is_none()
        })
        .expect("infinite range yields a free key");

    let mut workspace = Workspace::empty(key.clone(), "main", Utc::now());
    if !name.trim().is_empty() {
        workspace.name = name.trim().to_string();
    }
    workspace.project_key = Some(project_key);
    workspace.local = true;
    commit_upsert(config, &key, workspace);
    key
}

/// Create (or re-open) a local Project by name. Slugifies the name,
/// builds a `local-<slug>` ProjectKey, persists a Project record,
/// and broadcasts `ProjectUpserted` so the sidebar can render the
/// new header immediately. Idempotent: calling with the same name
/// twice opens the existing project — projects are named
/// containers, like directories, so this matches user expectation.
///
/// Returns the project key so the caller (TUI) can land focus on
/// the new header.
pub fn create_local_project(config: &ServerConfig, name: &str) -> lazybox_core::ProjectKey {
    let base = lazybox_core::slug::slugify(name);
    let slug = if base.is_empty() {
        "project".to_string()
    } else {
        base
    };
    let key = lazybox_core::ProjectKey::local(&slug);
    // Idempotent: re-broadcast the existing record on collision.
    let display_name = if name.trim().is_empty() {
        slug.clone()
    } else {
        name.trim().to_string()
    };
    let project = match config.store.get_project(&key) {
        Ok(Some(record)) => record
            .project_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<lazybox_core::Project>(j).ok())
            .unwrap_or_else(|| lazybox_core::Project::new(key.clone(), &display_name, Utc::now())),
        _ => lazybox_core::Project::new(key.clone(), &display_name, Utc::now()),
    };
    let json = match serde_json::to_string(&project) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                project_key = %key,
                "create_local_project: serde_json::to_string(project) failed: {e}",
            );
            None
        }
    };
    let record = lazybox_store::ProjectRecord {
        key: key.as_str().to_string(),
        created_at: project.created_at,
        project_json: json,
    };
    if let Err(e) = config.store.save_project(&record) {
        tracing::error!(
            project_key = %record.key,
            "save_project failed: {e}",
        );
    }
    let _ = config.bus.send(Event::ProjectUpserted(Box::new(project)));
    key
}

/// One-shot post-Stage-4 migration: if a pre-refactor `sandbox`
/// workspace exists in the store with no `project_key`, create a
/// "Sandbox" local Project and stamp the workspace with it so the
/// row reappears under a real Project header instead of landing in
/// `(no repo)`. Idempotent — already-migrated workspaces (project_key
/// set) are left alone; a missing sandbox workspace is a no-op.
///
/// Called once at daemon startup from both `run_embedded_realm` and
/// `server_start` so each lazybox launch self-heals legacy state.
pub fn migrate_legacy_sandbox(config: &ServerConfig) {
    let key = WorkspaceKey::new("sandbox".to_string());
    let Some(record) = config.store.get_workspace(&key).ok().flatten() else {
        return;
    };
    let Some(json) = record.workspace_json else {
        return;
    };
    let mut workspace: Workspace = match serde_json::from_str(&json) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("migrate_legacy_sandbox: failed to parse stored workspace: {e}");
            return;
        }
    };
    if workspace.project_key.is_some() {
        // Already migrated — skip.
        return;
    }
    let project_key = create_local_project(config, "Sandbox");
    workspace.project_key = Some(project_key);
    let ws_key = workspace.key.clone();
    commit_upsert(config, &ws_key, workspace);
    tracing::info!(
        "migrate_legacy_sandbox: moved `sandbox` workspace under `local-sandbox` project"
    );
}

/// Set or clear the workspace's `snoozed_until` timestamp. `None`
/// un-snoozes. Persists + broadcasts so the sidebar's mailbox-aware
/// rendering re-categorises the row.
pub fn set_snooze(config: &ServerConfig, key: &WorkspaceKey, until: Option<chrono::DateTime<Utc>>) {
    let Some(mut workspace) = load_workspace(config, key) else {
        return;
    };
    workspace.snoozed_until = until;
    commit_upsert(config, key, workspace);
}

/// Delete a workspace + all its sessions from the store. Broadcasts
/// `WorkspaceRemoved` so every connected TUI prunes its sidebar row.
/// Used by the sidebar's `Shift-X` two-press kill flow.
///
/// Does NOT delete the worktree directories on disk — that's a
/// future enhancement (needs to also kill any live PTY runners
/// rooted in those paths). For now we just drop the metadata; the
/// worktree dirs survive as ordinary git checkouts the user can
/// reuse or remove manually.
///
/// Also kills every backing terminal (PTY / tmux session) that
/// belonged to the workspace — without this the user's `Shift-X X`
/// hides the tabs in lazybox but leaves ghost tmux sessions visible
/// in `tmux ls`, which then re-surface on the next lazybox launch
/// via `recover_sessions`.
/// Read the persisted set of archived workspace keys. Used by the
/// upsert path to skip re-creating a workspace the user explicitly
/// dismissed via `Shift-X`. Returns an empty set when the kv entry
/// doesn't exist or fails to parse — degrades gracefully (worst
/// case the dismissed row reappears one more time).
pub fn load_archived_set(config: &ServerConfig) -> std::collections::HashSet<String> {
    config
        .store
        .get_kv(lazybox_core::KV_KEY_ARCHIVED)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

/// Add `key` to the persisted archived set. Idempotent.
pub fn archive_workspace_key(config: &ServerConfig, key: &str) {
    let mut set = load_archived_set(config);
    if !set.insert(key.to_string()) {
        return;
    }
    let vec: Vec<&String> = set.iter().collect();
    if let Ok(json) = serde_json::to_string(&vec)
        && let Err(e) = config.store.set_kv(lazybox_core::KV_KEY_ARCHIVED, &json)
    {
        tracing::warn!("archive_workspace_key: set_kv failed: {e}");
    }
}

/// Remove `key` from the persisted archived set so the next poll
/// can re-create the workspace. Today there's no UI for this; kept
/// public for a future "Settings → Restore Archive" flow.
pub fn unarchive_workspace_key(config: &ServerConfig, key: &str) {
    let mut set = load_archived_set(config);
    if !set.remove(key) {
        return;
    }
    let vec: Vec<&String> = set.iter().collect();
    if let Ok(json) = serde_json::to_string(&vec)
        && let Err(e) = config.store.set_kv(lazybox_core::KV_KEY_ARCHIVED, &json)
    {
        tracing::warn!("unarchive_workspace_key: set_kv failed: {e}");
    }
}

pub async fn delete_workspace(config: &ServerConfig, key: &WorkspaceKey) {
    delete_workspace_internal(config, key, /*archive=*/ true).await
}

/// Inner delete with the archive decision explicit. User-intent
/// deletes (`Shift-X`, project cascade, merged-PR removal) archive so
/// the next poll doesn't resurrect the row. System-driven deletes
/// (rescope) must NOT archive: the workspace fell out of the polled
/// set for upstream/transient reasons (truncated query, scope edit, a
/// PR that closed and later reopens), and the archive guard in
/// `upsert` would permanently block it from ever being re-created.
async fn delete_workspace_internal(config: &ServerConfig, key: &WorkspaceKey, archive: bool) {
    let key_str = key.as_str();
    // Record the archive so the next poll's upsert skips re-creating
    // this row. Without this, the user pressed `Shift-X`, the row
    // disappeared briefly, then the next 60s tick re-added it from
    // the upstream task — extremely confusing.
    if archive {
        archive_workspace_key(config, key_str);
    }

    // Find every terminal whose session_key matches via
    // terminal_meta — the authoritative wire-side mapping. Earlier
    // we parsed the backend_key prefix, but the backend's session
    // name format isn't part of any contract (tmux now uses
    // `lazybox-{repo}-{kind}-{pid}-{n}`); the meta map is. Locks are
    // taken + dropped before async backend.kill() calls.
    let to_kill_ids: Vec<lazybox_ipc::TerminalId> = {
        let meta = config.terminal_meta.lock().await;
        meta.iter()
            .filter(|(_, (sk, _))| sk.as_str() == key_str)
            .map(|(tid, _)| *tid)
            .collect()
    };
    let to_kill: Vec<(lazybox_ipc::TerminalId, String)> = {
        let terminals = config.terminals.lock().await;
        to_kill_ids
            .into_iter()
            .filter_map(|tid| terminals.get(&tid).map(|k| (tid, k.clone())))
            .collect()
    };

    if !to_kill.is_empty() {
        tracing::info!(
            "delete_workspace {key}: killing {} backing terminal(s)",
            to_kill.len()
        );
        for (tid, backend_key) in to_kill {
            if let Err(e) = config.backend.kill(&backend_key).await {
                tracing::warn!("kill {backend_key}: {e}");
            }
            // Clean every auxiliary map too. The pump task will
            // ALSO clean these when wait_exit returns, but that
            // happens on a tokio task with no upper bound on
            // latency. Doing it here closes the window where
            // rescope (or another subsystem) would see an entry
            // for a workspace we just deleted.
            config.terminals.lock().await.remove(&tid);
            config.terminal_meta.lock().await.remove(&tid);
            config.terminal_sessions.lock().await.remove(&tid);
            config.agent_states.lock().await.remove(&tid);
            // Mirror the daemon-pump's exit broadcast so any
            // still-connected clients see the tab disappear.
            let _ = config.bus.send(Event::TerminalExited {
                terminal_id: tid,
                exit_code: None,
            });
        }
    }

    if let Err(e) = config.store.delete_workspace(key) {
        tracing::warn!("delete_workspace failed: {e}");
    }
    let _ = config.bus.send(Event::WorkspaceRemoved(key.clone()));
}

/// Delete a Project: cascade through every workspace whose
/// `project_key` matches, then drop the Project record itself.
/// Broadcasts `WorkspaceRemoved` for each workspace and
/// `ProjectRemoved` for the project so the TUI can drop the rows
/// in one batch.
///
/// Workspace deletion routes through `delete_workspace` so each
/// workspace's backing terminals are killed and the archive set is
/// updated — without that step, the next poll would re-create the
/// workspaces from upstream tasks and the project would never
/// stay gone.
pub async fn delete_project(config: &ServerConfig, project_key: &lazybox_core::ProjectKey) {
    tracing::info!(project_key = %project_key, "delete_project: starting cascade");

    // Snapshot the workspace list before mutation — `delete_workspace`
    // removes rows from the store, so iterating a live cursor would
    // miss entries.
    let records = match config.store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("delete_project: list_workspaces failed: {e}");
            return;
        }
    };

    let mut child_keys: Vec<WorkspaceKey> = Vec::new();
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(ws) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };
        if ws.project_key.as_ref() == Some(project_key) {
            child_keys.push(ws.key);
        }
    }

    tracing::info!(
        project_key = %project_key,
        workspace_count = child_keys.len(),
        "delete_project: cascading workspace deletes"
    );
    for key in &child_keys {
        delete_workspace(config, key).await;
    }

    if let Err(e) = config.store.delete_project(project_key) {
        tracing::warn!("delete_project store: {e}");
    }
    let _ = config.bus.send(Event::ProjectRemoved(project_key.clone()));
    tracing::info!(project_key = %project_key, "delete_project: done");
}

/// Persist a new `SessionLayout` for one session inside a workspace.
/// The user's tile arrangement (Tabs vs Splits with a tree) is local
/// to the workspace; this writes it through the store and broadcasts
/// `WorkspaceUpserted` so other clients see the new layout.
///
/// No-op when the workspace or session can't be found.
pub fn set_session_layout(
    config: &ServerConfig,
    key: &WorkspaceKey,
    session_id: lazybox_core::SessionId,
    layout: lazybox_core::SessionLayout,
) {
    let Some(mut workspace) = load_workspace(config, key) else {
        return;
    };
    let Some(session) = workspace.sessions.iter_mut().find(|s| s.id == session_id) else {
        tracing::debug!("set_session_layout: no session {session_id} in {key}");
        return;
    };
    session.layout = layout;
    commit_upsert(config, key, workspace);
}

/// Apply a partial-mark to one activity row. Used by the TUI's
/// auto-mark-on-hover feature so the user can scroll past comments
/// and have them flip read individually, instead of `MarkRead`'s
/// "flip the whole workspace" behavior. Persists + broadcasts.
///
/// No-op when the workspace isn't in the store or `index` is out of
/// range — both are user-driven inputs and we don't want a TUI race
/// (poll deletes a workspace while the user hovers) to crash the
/// daemon.
pub fn mark_activity_read(config: &ServerConfig, key: &WorkspaceKey, index: usize) {
    apply_activity_mark(config, key, index, /*read=*/ true);
}

/// Reverse of `mark_activity_read`. `z` undo binds here.
pub fn unmark_activity_read(config: &ServerConfig, key: &WorkspaceKey, index: usize) {
    apply_activity_mark(config, key, index, /*read=*/ false);
}

fn apply_activity_mark(config: &ServerConfig, key: &WorkspaceKey, index: usize, read: bool) {
    let Some(mut workspace) = load_workspace(config, key) else {
        tracing::debug!("apply_activity_mark: no record for {key}");
        return;
    };
    if read {
        workspace.mark_activity_read(index);
    } else {
        workspace.unmark_activity_read(index);
    }
    commit_upsert(config, key, workspace);
}

/// Apply the user's "mark every activity item read" gesture to a
/// stored workspace and broadcast the change. Activity-seen state is
/// **independent** of the upstream provider state: providers only ever
/// rewrite the activity feed itself; `seen_count` + `read_indices`
/// belong to the local user. Preserving them across polls happens in
/// `upsert`; this function flips them all-read on demand.
///
/// No-op if the workspace isn't in the store.
pub fn mark_workspace_read(config: &ServerConfig, key: &WorkspaceKey) {
    let Some(mut workspace) = load_workspace(config, key) else {
        tracing::debug!("mark_workspace_read: no record for {key}");
        return;
    };
    workspace.mark_read_all();
    workspace.last_viewed_at = Some(Utc::now());
    commit_upsert(config, key, workspace);
}

#[cfg(test)]
mod merge_detection_tests {
    use super::*;
    use lazybox_core::{TaskId, TaskRole, TaskState};

    fn task(source: &str, key: &str, url: &str, state: TaskState) -> Task {
        Task {
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
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        }
    }

    fn pr(key: &str, state: TaskState) -> Task {
        task("github", key, "https://github.com/o/r/pull/7", state)
    }

    #[test]
    fn pr_number_parses_trailing_id_segment() {
        assert_eq!(
            pr_number_from_task(&pr("o/r#7", TaskState::Merged)),
            Some(7)
        );
    }

    #[test]
    fn pr_number_none_when_no_hash_number() {
        let mut t = pr("o/r#7", TaskState::Merged);
        t.id.key = "ENG-42".into();
        assert_eq!(pr_number_from_task(&t), None);
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
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: closes,
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
        config.terminal_meta.lock().await.insert(
            lazybox_ipc::TerminalId(1),
            (session_key, lazybox_ipc::TerminalKind::Shell),
        );

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
    use std::sync::Arc;
    use std::sync::Mutex;

    /// A `TaskSource` that returns whatever tasks the test stashed in
    /// it. Defaults — `polled_scope = Repos([])`, `last_fetch_kind =
    /// Full` — keep `tick` on the non-destructive path so the test
    /// observes only the upsert broadcasts.
    struct FixtureSource {
        tasks: Mutex<Vec<Task>>,
    }

    impl FixtureSource {
        fn new(tasks: Vec<Task>) -> Self {
            Self {
                tasks: Mutex::new(tasks),
            }
        }
        fn set(&self, tasks: Vec<Task>) {
            *self.tasks.lock().unwrap() = tasks;
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
            let tasks = self.tasks.lock().unwrap().clone();
            Box::pin(async move { Ok(tasks) })
        }
    }

    fn issue(key: &str, title: &str) -> Task {
        Task {
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
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Unknown,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
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
        let sources: Vec<Box<dyn TaskSource>> =
            vec![Box::new(FixtureSource::new(vec![issue("o/r#1", "first")]))];

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
    }
}
