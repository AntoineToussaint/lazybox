//! Daemon-side "auto-merge on green" (issue #363's arm, daemon-fired).
//!
//! The trigger used to live in the TUI client: every `WorkspaceUpserted`
//! re-ran the eligibility predicate and the client shipped a
//! `Command::MergePr`, latched by a per-client in-memory set. Three
//! structural bugs followed: a headless daemon (`lazybox server start`)
//! never auto-merged, two attached clients double-fired the same merge,
//! and non-TUI clients (JSON API, Tauri) silently lacked the feature.
//! The trigger now runs HERE, in the polling commit path — one firing
//! authority regardless of how many clients (including zero) are
//! attached.
//!
//! ## Shape
//!
//! - [`signal_for`] projects a freshly-committed workspace onto a
//!   [`Signal`] using the shared core predicate
//!   ([`lazybox_core::should_auto_merge`] — the same one the TUI renders
//!   its pill from).
//! - [`plan`] runs the one-shot latch ([`AutoMergeMemory`]) and decides
//!   whether to dispatch an attempt.
//! - [`run_attempt`] is the attempt itself: **re-fetch the PR fresh**
//!   (the stored row can be minutes stale — a changes-requested arriving
//!   after the last poll must not get merged over), re-verify
//!   eligibility on the fresh state, and only then merge — pinning
//!   GitHub's `expectedHeadOid` to the head the fresh fetch verified
//!   green, so a force-push racing the merge is rejected upstream.
//!
//! ## Latch semantics (per workspace key)
//!
//! - fires **once per green head**: after an attempt settles, its head
//!   OID is remembered ([`Latch::Done`] / [`Latch::Blocked`]);
//! - red CI then re-green on the SAME head does not re-merge — a
//!   `Blocked` re-probe sees the unchanged head and stands down, a
//!   `Done` entry skips outright;
//! - a NEW head (force-push / new commit) re-arms once it's green
//!   again: the changed workspace re-probes, the fresh fetch reports a
//!   different OID, and the attempt proceeds;
//! - disarming, losing the PR, or reaching a terminal state releases
//!   the latch entirely.
//!
//! The memory is deliberately **in-memory only**: a daemon restart
//! re-evaluates armed workspaces and may re-attempt a merge that
//! already landed — harmless, because `mergePullRequest` on an
//! already-merged PR is classified as success by the client's
//! idempotence guard (`ALREADY_MERGED_MARKERS`).

use super::{apply_and_commit, load_workspace};
use crate::ServerConfig;
use lazybox_core::{Task, Workspace, WorkspaceKey};
use lazybox_ipc::Event;
use std::collections::HashMap;

/// One-shot latch state for a workspace whose merge-on-green arm has
/// produced (or is producing) an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Latch {
    /// An attempt is in flight — don't dispatch another.
    InFlight,
    /// A merge mutation was dispatched successfully for this head.
    /// Nothing more to do until the poll confirms the terminal state
    /// (which releases the latch).
    Done(Option<String>),
    /// The attempt settled without merging this head — the fresh
    /// re-check failed, the merge was rejected, or infrastructure was
    /// missing. A *changed* commit re-probes; the probe only merges if
    /// the head moved on.
    Blocked(Option<String>),
}

/// Daemon-wide auto-merge memory. Own `parking_lot::Mutex` on
/// [`ServerConfig`] — NOT a `TickState` field — for the same reason as
/// `MergePromptMemory` (#131/#132): it's touched from inside the
/// `upsert` path, which must stay decoupled from `poll_state`'s
/// non-reentrant lock.
#[derive(Default)]
pub struct AutoMergeMemory {
    latches: HashMap<String, Latch>,
    /// Attempts dispatched since startup — observability for tests and
    /// the sync-status log.
    pub(crate) attempts_started: u64,
}

impl AutoMergeMemory {
    /// Current latch for a workspace key (test/diagnostic view).
    pub(crate) fn latch(&self, key: &WorkspaceKey) -> Option<&Latch> {
        self.latches.get(key.as_str())
    }

    fn settle(&mut self, key: &WorkspaceKey, latch: Option<Latch>) {
        match latch {
            Some(l) => {
                self.latches.insert(key.as_str().to_string(), l);
            }
            None => {
                self.latches.remove(key.as_str());
            }
        }
    }
}

/// What a freshly-committed workspace means for the auto-merge latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Armed and merge-ready right now — a candidate to fire.
    Fire,
    /// Armed but not merge-ready (red/pending CI, draft, changes
    /// requested, native auto-merge on, …). Keep any latch as-is: this
    /// is exactly the red-phase a same-head re-green must ride through
    /// without re-arming.
    Hold,
    /// The arm no longer applies — disarmed, no PR, or the PR reached a
    /// terminal state. Release the latch so the key doesn't leak.
    Release,
}

/// Project a workspace onto a [`Signal`] via the shared core predicate.
pub fn signal_for(ws: &Workspace) -> Signal {
    let terminal = ws.pr.as_ref().is_some_and(|pr| {
        matches!(
            pr.state,
            lazybox_core::TaskState::Merged | lazybox_core::TaskState::Closed
        )
    });
    if !ws.auto_merge_on_green || ws.pr.is_none() || terminal {
        return Signal::Release;
    }
    if lazybox_core::should_auto_merge(ws) {
        Signal::Fire
    } else {
        Signal::Hold
    }
}

/// Dispatch ticket for one attempt. `skip_if_head` carries the head a
/// previous attempt already settled on — the attempt stands down
/// without merging when the fresh fetch still reports that OID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptPlan {
    pub workspace_key: WorkspaceKey,
    pub skip_if_head: Option<String>,
}

/// Run the latch state machine for one committed workspace. Returns the
/// attempt to dispatch, if any, and leaves the key marked `InFlight` so
/// concurrent commits can't double-dispatch. `changed` is the commit's
/// outcome (a byte-identical re-poll is `false`): a `Blocked` key only
/// re-probes on a change, so a permanently-blocked-but-quiet PR costs
/// nothing per tick.
pub fn plan(
    memory: &mut AutoMergeMemory,
    key: &WorkspaceKey,
    signal: Signal,
    changed: bool,
) -> Option<AttemptPlan> {
    match signal {
        Signal::Release => {
            memory.settle(key, None);
            None
        }
        Signal::Hold => None,
        Signal::Fire => {
            let skip_if_head = match memory.latch(key) {
                None => None,
                // In flight, or already merged this head — never
                // double-dispatch. (The client-era regression: a
                // re-broadcast of the same green state must not
                // double-merge.)
                Some(Latch::InFlight) | Some(Latch::Done(_)) => return None,
                Some(Latch::Blocked(head)) if changed => head.clone(),
                Some(Latch::Blocked(_)) => return None,
            };
            memory.settle(key, Some(Latch::InFlight));
            memory.attempts_started += 1;
            Some(AttemptPlan {
                workspace_key: key.clone(),
                skip_if_head,
            })
        }
    }
}

/// Post-commit hook: called by the polling commit paths with the
/// committed workspace's signal. Dispatches the attempt on a detached
/// task so a poll tick's per-task timeout can't cancel a merge
/// mid-mutation.
pub(crate) fn on_workspace_committed(
    config: &ServerConfig,
    key: &WorkspaceKey,
    signal: Signal,
    changed: bool,
) {
    let ticket = {
        let mut memory = config.poll.auto_merge.lock();
        plan(&mut memory, key, signal, changed)
    };
    let Some(ticket) = ticket else {
        return;
    };
    tracing::info!(
        workspace = %ticket.workspace_key,
        reprobe = ticket.skip_if_head.is_some(),
        "auto-merge: dispatching attempt"
    );
    let config = config.clone();
    tokio::spawn(async move { run_real_attempt(&config, ticket).await });
}

/// Provider seam for [`run_attempt`] — the two upstream calls an
/// attempt makes, mockable in tests. Implemented by `GhClient`; the
/// enum-dispatch dance `ProviderHandle` does isn't needed here because
/// merge-on-green is a PR concept and only GitHub has PRs today.
#[allow(async_fn_in_trait)]
pub trait MergeBackend {
    /// Fresh single-PR fetch returning the task plus its head OID.
    /// `Ok(None)` = PR not visible (deleted / transferred / scope
    /// revoked).
    async fn fetch_pr_with_head(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<(Task, Option<String>)>, String>;

    /// Merge the workspace's PR, pinned to `expected_head_oid` when
    /// known. Returns the typed [`ProviderError`] — not a flattened
    /// `String` — so the attempt can tell a transient secondary rate limit
    /// (retry on the next green poll) from a genuine rejection (stand down).
    async fn merge(
        &self,
        ws: &Workspace,
        expected_head_oid: Option<&str>,
    ) -> Result<(), lazybox_core::ProviderError>;
}

impl MergeBackend for lazybox_gh::GhClient {
    async fn fetch_pr_with_head(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<(Task, Option<String>)>, String> {
        self.fetch_single_pr_with_head(owner, repo, number)
            .await
            .map_err(|e| e.to_string())
    }

    async fn merge(
        &self,
        ws: &Workspace,
        expected_head_oid: Option<&str>,
    ) -> Result<(), lazybox_core::ProviderError> {
        lazybox_core::TaskProvider::merge(self, ws, expected_head_oid).await
    }
}

/// Resolve the real GitHub client and run the attempt. A missing
/// credential leaves the key `Blocked` (not released) so a daemon
/// without GitHub auth doesn't spin a doomed attempt on every tick.
async fn run_real_attempt(config: &ServerConfig, ticket: AttemptPlan) {
    run_real_attempt_with_resolver(config, ticket, || {
        super::handlers::resolve_gh_client(config)
    })
    .await;
}

async fn run_real_attempt_with_resolver<F, Fut>(
    config: &ServerConfig,
    ticket: AttemptPlan,
    resolve_client: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<lazybox_gh::GhClient>>,
{
    let key = &ticket.workspace_key;
    let settle = |latch: Option<Latch>| {
        config.poll.auto_merge.lock().settle(key, latch);
    };
    let Some(ws) = load_workspace(config, key) else {
        settle(None);
        return;
    };
    if !ws.auto_merge_on_green {
        settle(None);
        return;
    }
    if ws
        .pr
        .as_ref()
        .and_then(super::handlers::github_target)
        .is_none()
    {
        settle(Some(Latch::Blocked(ticket.skip_if_head.clone())));
        return;
    }

    match resolve_client().await {
        Some(client) => run_attempt(config, ticket, &client).await,
        None => {
            tracing::warn!(
                workspace = %ticket.workspace_key,
                "auto-merge: no GitHub client available — standing down"
            );
            let prior = ticket.skip_if_head.clone();
            config
                .poll
                .auto_merge
                .lock()
                .settle(&ticket.workspace_key, Some(Latch::Blocked(prior)));
        }
    }
}

/// One auto-merge attempt: fresh fetch → re-verify → guarded merge.
///
/// Every exit path settles the `InFlight` latch, so a settled attempt
/// can never wedge the key. Unlike the manual `g m` handler
/// (`handle_merge_pr`), which honors user intent against the *local*
/// snapshot, this path trusts nothing it didn't just fetch.
pub async fn run_attempt<B: MergeBackend>(config: &ServerConfig, ticket: AttemptPlan, backend: &B) {
    let key = &ticket.workspace_key;
    let settle = |latch: Option<Latch>| {
        config.poll.auto_merge.lock().settle(key, latch);
    };

    let Some(ws) = load_workspace(config, key) else {
        settle(None);
        return;
    };
    // Re-check the arm at attempt time: the user may have disarmed
    // between dispatch and now.
    if !ws.auto_merge_on_green {
        settle(None);
        return;
    }
    let Some((owner, repo, number)) = ws.pr.as_ref().and_then(super::handlers::github_target)
    else {
        // Not GitHub-shaped (no repo / unparseable number) — nothing to
        // merge against. Block rather than release so we don't re-plan
        // an unfixable target every tick.
        tracing::debug!(workspace = %key, "auto-merge: no GitHub target — standing down");
        settle(Some(Latch::Blocked(ticket.skip_if_head.clone())));
        return;
    };
    let pr_label = ws
        .pr
        .as_ref()
        .map(|p| p.id.key.clone())
        .unwrap_or_else(|| key.as_str().to_string());

    // Fresh eligibility source — the stored row can be a full poll
    // interval stale.
    let fetched = match backend.fetch_pr_with_head(&owner, &repo, number).await {
        Ok(f) => f,
        Err(e) => {
            // Transient (network, rate limit): restore the pre-attempt
            // state so a later tick retries cleanly.
            tracing::warn!(workspace = %key, "auto-merge: fresh PR fetch failed: {e}");
            settle(ticket.skip_if_head.clone().map(|h| Latch::Blocked(Some(h))));
            return;
        }
    };
    let Some((fresh, head)) = fetched else {
        tracing::warn!(workspace = %key, "auto-merge: PR no longer visible — standing down");
        settle(Some(Latch::Blocked(ticket.skip_if_head.clone())));
        return;
    };

    // Same head a previous attempt already settled on: a red→re-green
    // of an unchanged head must not merge twice. Refresh local state
    // and keep the block.
    if let (Some(prior), Some(current)) = (ticket.skip_if_head.as_deref(), head.as_deref())
        && prior == current
    {
        tracing::debug!(
            workspace = %key,
            head = current,
            "auto-merge: head unchanged since last attempt — standing down"
        );
        settle(Some(Latch::Blocked(head.clone())));
        commit_fresh_task(config, key, fresh).await;
        return;
    }

    // Re-verify against the FRESH state — this is the whole point of
    // the re-fetch. A changes-requested/red-CI/closed/draft arriving
    // after the last poll aborts here instead of being merged over.
    let mut probe = ws.clone();
    probe.pr = Some(fresh.clone());
    if let Some(reason) = lazybox_core::auto_merge_block_reason(&probe) {
        tracing::info!(workspace = %key, reason, "auto-merge: fresh re-check stood down");
        let _ = config.bus.send(Event::provider_error_retryable(
            "auto-merge",
            format!("auto-merge stood down on {pr_label}: {reason}"),
        ));
        settle(Some(Latch::Blocked(head)));
        commit_fresh_task(config, key, fresh).await;
        return;
    }

    // Merge, pinned to the head the fresh fetch just verified green.
    // A force-push landing between that fetch and this mutation is
    // rejected by GitHub ("Head branch was modified…").
    match backend.merge(&probe, head.as_deref()).await {
        Ok(()) => {
            tracing::info!(workspace = %key, "auto-merged PR (merge-on-green)");
            settle(Some(Latch::Done(head)));
            // Mirror `handle_merge_pr`: the local Task still reads
            // `Open` — broadcast `PrMerged` so clients flash the notice
            // and hold MERGED, then wake the poll to reconcile.
            let _ = config.bus.send(Event::PrMerged {
                workspace_key: key.clone(),
                pr_label,
            });
            config.poll.wake(true);
        }
        Err(e) if e.is_retryable() => {
            // A secondary rate limit is transient: GitHub resets it within a
            // window and the PR is still green. Don't `Blocked`-latch (that
            // stands the head down until a new commit and would strand a
            // rate-limited auto-merge forever) — release the latch so the
            // next green poll re-attempts.
            //
            // Deliberately no bus notice: this is a *background* retry that
            // re-fires every poll until the window clears, so a per-poll
            // "rate-limited — will retry" would just repeat. The user-facing
            // signal is the terminal outcome — `PrMerged` once it lands, or a
            // real `PrMergeFailed` if it turns out non-transient — not the
            // daemon's internal waiting. Manual merges (`handle_merge_pr`) do
            // surface a live queued status; auto-merge stays quiet.
            tracing::warn!(
                workspace = %key,
                "auto-merge rate-limited; will re-attempt next poll: {}",
                e.diagnostic()
            );
            settle(None);
            commit_fresh_task(config, key, fresh).await;
        }
        Err(e) => {
            tracing::warn!(workspace = %key, "auto-merge failed: {}", e.diagnostic());
            settle(Some(Latch::Blocked(head)));
            // Same loud, persistent surface as a manual merge failure —
            // includes GitHub's reason (branch protection, head moved…),
            // humanized so no raw GraphQL/JSON reaches the footer.
            let _ = config.bus.send(Event::PrMergeFailed {
                workspace_key: key.clone(),
                pr_label,
                reason: super::handlers::humanize_mutation_failure("auto-merge", &e),
            });
            commit_fresh_task(config, key, fresh).await;
        }
    }
}

/// Persist the freshly-fetched task so the sidebar shows why the
/// attempt stood down (e.g. the changes-requested review) without
/// waiting out the next poll. Routed through `apply_and_commit` —
/// deliberately NOT `upsert` — so the refresh can't recursively
/// re-enter the auto-merge hook.
async fn commit_fresh_task(config: &ServerConfig, key: &WorkspaceKey, fresh: Task) {
    apply_and_commit(config, key, |ws| ws.attach_task(fresh)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lazybox_core::{CiStatus, ReviewStatus, TaskId, TaskRole, TaskState};
    use lazybox_store::{MemoryStore, Store, WorkspaceRecord};
    use std::sync::Arc;

    fn green_task(key: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::Success,
            review: ReviewStatus::Approved,
            checks: vec![],
            unread_count: 0,
            url: {
                let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
                format!("https://github.com/{path}/pull/{num}")
            },
            repo: key.rsplit_once('#').map(|(path, _)| path.to_string()),
            branch: Some("feature".into()),
            base_branch: Some("main".into()),
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: Some("PR_node".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: Some(lazybox_core::TaskKind::Pr),
            closes_issues: vec![],
            priority: None,
            state_label: None,
        }
    }

    fn armed_ws(key: &str) -> Workspace {
        let mut ws = Workspace::from_task(green_task(key), Utc::now());
        ws.auto_merge_on_green = true;
        ws
    }

    fn seed(store: &MemoryStore, ws: &Workspace) {
        store
            .save_workspace(&WorkspaceRecord {
                key: ws.key.as_str().into(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(ws).unwrap()),
            })
            .unwrap();
    }

    fn config_with(ws: &Workspace) -> ServerConfig {
        let store = Arc::new(MemoryStore::new());
        seed(&store, ws);
        ServerConfig::with_store(store)
    }

    // ── signal_for ───────────────────────────────────────────────

    #[test]
    fn signal_fire_only_when_armed_and_green() {
        let mut ws = armed_ws("o/r#1");
        assert_eq!(signal_for(&ws), Signal::Fire);
        ws.pr.as_mut().unwrap().ci = CiStatus::Pending;
        assert_eq!(signal_for(&ws), Signal::Hold, "pending CI holds");
        ws.pr.as_mut().unwrap().ci = CiStatus::Success;
        ws.auto_merge_on_green = false;
        assert_eq!(signal_for(&ws), Signal::Release, "disarm releases");
    }

    #[test]
    fn signal_release_on_terminal_state_or_missing_pr() {
        let mut ws = armed_ws("o/r#1");
        ws.pr.as_mut().unwrap().state = TaskState::Merged;
        assert_eq!(signal_for(&ws), Signal::Release);
        let mut gone = armed_ws("o/r#1");
        gone.pr = None;
        assert_eq!(signal_for(&gone), Signal::Release);
    }

    // ── plan / latch semantics (daemon port of the client latch
    //    tests that lived at `realm/model/tests.rs`) ───────────────

    /// Client-era `armed_green_pr_fires_merge_exactly_once`: the first
    /// Fire dispatches; the same green state re-broadcast next poll is
    /// swallowed by the latch.
    #[test]
    fn armed_green_fires_exactly_once() {
        let mut m = AutoMergeMemory::default();
        let key = WorkspaceKey::new("github-o-r-1");
        let first = plan(&mut m, &key, Signal::Fire, true);
        assert!(first.is_some(), "armed + green dispatches");
        assert_eq!(m.latch(&key), Some(&Latch::InFlight));
        assert!(
            plan(&mut m, &key, Signal::Fire, false).is_none(),
            "re-broadcast of the same green state must not double-dispatch"
        );
        assert_eq!(m.attempts_started, 1);
    }

    /// Client-era `unarmed_or_ungreen_never_fires`.
    #[test]
    fn hold_and_release_never_dispatch() {
        let mut m = AutoMergeMemory::default();
        let key = WorkspaceKey::new("github-o-r-1");
        assert!(plan(&mut m, &key, Signal::Hold, true).is_none());
        assert!(plan(&mut m, &key, Signal::Release, true).is_none());
        assert_eq!(m.attempts_started, 0);
    }

    /// Client-era `re_green_after_failed_race_re_fires`, upgraded with
    /// head semantics: a Blocked key re-probes on a changed commit
    /// (the probe itself decides whether the head moved), but an
    /// unchanged commit never re-probes.
    #[test]
    fn blocked_reprobes_only_on_a_changed_commit() {
        let mut m = AutoMergeMemory::default();
        let key = WorkspaceKey::new("github-o-r-1");
        m.settle(&key, Some(Latch::Blocked(Some("abc".into()))));
        assert!(
            plan(&mut m, &key, Signal::Fire, false).is_none(),
            "an unchanged re-poll of a blocked key must not re-probe"
        );
        let probe = plan(&mut m, &key, Signal::Fire, true).expect("changed commit re-probes");
        assert_eq!(
            probe.skip_if_head.as_deref(),
            Some("abc"),
            "the probe carries the already-settled head for comparison"
        );
    }

    /// Client-era `confirmed_merge_suppresses_a_redundant_auto_merge`:
    /// once a merge was dispatched successfully (`Done`), interim polls
    /// still reporting the armed PR as green + Open must not re-fire.
    #[test]
    fn done_suppresses_interim_green_polls() {
        let mut m = AutoMergeMemory::default();
        let key = WorkspaceKey::new("github-o-r-1");
        m.settle(&key, Some(Latch::Done(Some("abc".into()))));
        assert!(plan(&mut m, &key, Signal::Fire, true).is_none());
        assert!(plan(&mut m, &key, Signal::Fire, false).is_none());
    }

    /// Release clears the latch so a later re-arm starts fresh (and the
    /// map doesn't leak keys for merged/removed workspaces).
    #[test]
    fn release_clears_the_latch() {
        let mut m = AutoMergeMemory::default();
        let key = WorkspaceKey::new("github-o-r-1");
        m.settle(&key, Some(Latch::Done(None)));
        assert!(plan(&mut m, &key, Signal::Release, true).is_none());
        assert_eq!(m.latch(&key), None);
        assert!(
            plan(&mut m, &key, Signal::Fire, false).is_some(),
            "a fresh arming after release dispatches again"
        );
    }

    // ── run_attempt (fake backend) ───────────────────────────────

    /// Recording fake: scripted fetch result + merge result.
    struct FakeBackend {
        fetch: Result<Option<(Task, Option<String>)>, String>,
        merge_result: Result<(), lazybox_core::ProviderError>,
        merges: parking_lot::Mutex<Vec<Option<String>>>,
    }

    impl FakeBackend {
        fn merging(fresh: Task, head: &str) -> Self {
            Self {
                fetch: Ok(Some((fresh, Some(head.into())))),
                merge_result: Ok(()),
                merges: parking_lot::Mutex::new(Vec::new()),
            }
        }

        /// A backend whose merge fails with a scripted error — used to
        /// exercise the retryable (rate-limit) vs permanent branches.
        fn failing(fresh: Task, head: &str, err: lazybox_core::ProviderError) -> Self {
            Self {
                fetch: Ok(Some((fresh, Some(head.into())))),
                merge_result: Err(err),
                merges: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    impl MergeBackend for FakeBackend {
        async fn fetch_pr_with_head(
            &self,
            _owner: &str,
            _repo: &str,
            _number: u64,
        ) -> Result<Option<(Task, Option<String>)>, String> {
            self.fetch.clone()
        }

        async fn merge(
            &self,
            _ws: &Workspace,
            expected_head_oid: Option<&str>,
        ) -> Result<(), lazybox_core::ProviderError> {
            self.merges
                .lock()
                .push(expected_head_oid.map(|s| s.to_string()));
            self.merge_result.clone()
        }
    }

    fn ticket(ws: &Workspace, skip_if_head: Option<&str>) -> AttemptPlan {
        AttemptPlan {
            workspace_key: ws.key.clone(),
            skip_if_head: skip_if_head.map(|s| s.to_string()),
        }
    }

    /// The happy path merges exactly once, pinned to the freshly
    /// verified head OID, and broadcasts `PrMerged`.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_merges_with_the_fresh_head_oid() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let mut rx = config.bus.subscribe();
        let backend = FakeBackend::merging(green_task("o/r#1"), "abc123");

        run_attempt(&config, ticket(&ws, None), &backend).await;

        assert_eq!(
            backend.merges.lock().as_slice(),
            &[Some("abc123".to_string())],
            "the merge must be pinned to the head the fresh fetch verified"
        );
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Done(Some("abc123".into())))
        );
        let evt = rx.try_recv().expect("PrMerged must be broadcast");
        assert!(matches!(evt, Event::PrMerged { .. }), "got {evt:?}");
    }

    /// The stale-row hole this feature closes: the stored workspace is
    /// green, but the FRESH fetch reports changes-requested. The
    /// attempt must not merge, must surface a stand-down notice, and
    /// must persist the fresh state.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_stands_down_when_fresh_state_is_ineligible() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let mut rx = config.bus.subscribe();
        let mut fresh = green_task("o/r#1");
        fresh.review = ReviewStatus::ChangesRequested;
        let backend = FakeBackend {
            fetch: Ok(Some((fresh, Some("abc123".into())))),
            merge_result: Ok(()),
            merges: parking_lot::Mutex::new(Vec::new()),
        };

        run_attempt(&config, ticket(&ws, None), &backend).await;

        assert!(backend.merges.lock().is_empty(), "must not merge");
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Blocked(Some("abc123".into())))
        );
        let evt = rx.try_recv().expect("stand-down notice");
        match evt {
            Event::ProviderError {
                source, message, ..
            } => {
                assert_eq!(source, "auto-merge");
                assert!(message.contains("changes were requested"), "{message}");
            }
            other => panic!("expected ProviderError notice, got {other:?}"),
        }
        // The fresh (ineligible) state was committed so the UI shows
        // why — and the re-loaded workspace no longer plans an attempt.
        let stored = load_workspace(&config, &ws.key).expect("workspace persisted");
        assert_eq!(
            stored.pr.as_ref().unwrap().review,
            ReviewStatus::ChangesRequested
        );
        assert_eq!(signal_for(&stored), Signal::Hold);
    }

    /// Red→re-green on the SAME head: the re-probe compares the fresh
    /// head with the one the previous attempt settled on and stands
    /// down silently — no second merge, no notice spam.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_suppresses_an_unchanged_head_on_reprobe() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let mut rx = config.bus.subscribe();
        let backend = FakeBackend::merging(green_task("o/r#1"), "abc123");

        run_attempt(&config, ticket(&ws, Some("abc123")), &backend).await;

        assert!(
            backend.merges.lock().is_empty(),
            "an unchanged head must not merge twice"
        );
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Blocked(Some("abc123".into())))
        );
        // Silent apart from the fresh-state commit's own broadcasts
        // (workspace/project upserts) — no merge outcome and no
        // stand-down notice.
        while let Ok(evt) = rx.try_recv() {
            assert!(
                !matches!(
                    evt,
                    Event::PrMerged { .. }
                        | Event::PrMergeFailed { .. }
                        | Event::ProviderError { .. }
                ),
                "suppression must not emit merge/notice events, got {evt:?}"
            );
        }
    }

    /// A NEW head re-arms: the re-probe sees a different OID and the
    /// merge proceeds, pinned to the new head.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_rearms_on_a_new_head() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeBackend::merging(green_task("o/r#1"), "def456");

        run_attempt(&config, ticket(&ws, Some("abc123")), &backend).await;

        assert_eq!(
            backend.merges.lock().as_slice(),
            &[Some("def456".to_string())]
        );
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Done(Some("def456".into())))
        );
    }

    /// A rejected merge (branch protection, head moved between fetch
    /// and mutation) surfaces the same loud `PrMergeFailed` a manual
    /// merge does, and blocks the head so quiet ticks don't hammer.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_merge_failure_emits_pr_merge_failed_and_blocks() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let mut rx = config.bus.subscribe();
        let backend = FakeBackend::failing(
            green_task("o/r#1"),
            "abc123",
            lazybox_core::ProviderError::permanent(
                "github",
                "Head branch was modified. Review and try the merge again.",
            ),
        );

        run_attempt(&config, ticket(&ws, None), &backend).await;

        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Blocked(Some("abc123".into())))
        );
        let evt = rx.try_recv().expect("PrMergeFailed");
        match evt {
            Event::PrMergeFailed { reason, .. } => {
                assert!(reason.contains("Head branch was modified"), "{reason}");
                assert!(!reason.contains('{'), "no raw JSON in the reason: {reason}");
            }
            other => panic!("expected PrMergeFailed, got {other:?}"),
        }
    }

    /// A secondary rate limit during an auto-merge is transient: the attempt
    /// must NOT `Blocked`-latch the head (which would strand it until a new
    /// commit) — it releases the latch so the next green poll re-attempts.
    /// And because that re-attempt fires every poll until the window clears,
    /// the background path stays quiet: no red `PrMergeFailed`, and no
    /// per-poll "will retry" notice that would just repeat.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_rate_limited_releases_latch_and_stays_quiet() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let mut rx = config.bus.subscribe();
        let backend = FakeBackend::failing(
            green_task("o/r#1"),
            "abc123",
            lazybox_core::ProviderError::retryable_after(
                "github",
                "You have exceeded a secondary rate limit",
                60,
            ),
        );

        run_attempt(&config, ticket(&ws, None), &backend).await;

        // Latch released (not Blocked) so a still-green re-poll re-attempts.
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            None,
            "a transient rate limit must not permanently block the head"
        );
        // No PrMergeFailed and no per-poll retry notice — the background
        // retry is silent until it resolves to a terminal outcome.
        loop {
            match rx.try_recv() {
                Ok(Event::PrMergeFailed { .. }) => {
                    panic!("a rate limit must not surface as a red PrMergeFailed")
                }
                Ok(Event::ProviderError { source, .. }) if source == "auto-merge" => {
                    panic!("a background auto-merge retry must not emit a per-poll notice")
                }
                // Unrelated commit/broadcast events (e.g. the fresh-task
                // commit) are fine; keep draining until the bus is empty.
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }

    /// Disarming between dispatch and attempt aborts and releases the
    /// latch — the user's explicit "off" always wins.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_aborts_when_disarmed_mid_flight() {
        let mut ws = armed_ws("o/r#1");
        ws.auto_merge_on_green = false;
        let config = config_with(&ws);
        let backend = FakeBackend::merging(green_task("o/r#1"), "abc123");

        run_attempt(&config, ticket(&ws, None), &backend).await;

        assert!(backend.merges.lock().is_empty());
        assert_eq!(config.poll.auto_merge.lock().latch(&ws.key), None);
    }

    /// A transient fetch failure restores the pre-attempt latch so a
    /// later tick retries cleanly.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_fetch_failure_restores_prior_state() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeBackend {
            fetch: Err("rate limited".into()),
            merge_result: Ok(()),
            merges: parking_lot::Mutex::new(Vec::new()),
        };

        run_attempt(&config, ticket(&ws, None), &backend).await;
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            None,
            "a first attempt that never fetched releases for a clean retry"
        );

        run_attempt(&config, ticket(&ws, Some("abc123")), &backend).await;
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Blocked(Some("abc123".into()))),
            "a re-probe that never fetched keeps its blocked head"
        );
    }

    // ── wiring: the polling commit path drives the hook ──────────

    /// End-to-end through `polling::upsert`: committing an armed +
    /// green workspace dispatches an attempt. The attempt behavior and
    /// latch transitions are pinned independently below and by the
    /// fake-backend tests above.
    #[tokio::test(flavor = "current_thread")]
    async fn upsert_commit_path_dispatches_the_attempt() {
        let mut task = green_task("o/r#1");
        task.repo = None;
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store);

        // Discover the workspace, then arm it (the arm handler doesn't
        // route through `upsert`, so this can't dispatch yet).
        super::super::upsert(&config, task.clone()).await;
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        crate::workspace::set_auto_merge_on_green(&config, &key, true).await;
        assert_eq!(config.poll.auto_merge.lock().attempts_started, 0);

        // A re-poll of the armed green workspace dispatches once …
        super::super::upsert(&config, task.clone()).await;
        assert_eq!(config.poll.auto_merge.lock().attempts_started, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn real_attempt_without_a_target_settles_before_client_resolution() {
        let mut ws = armed_ws("o/r#1");
        ws.pr.as_mut().unwrap().repo = None;
        let config = config_with(&ws);
        let ticket = plan(
            &mut config.poll.auto_merge.lock(),
            &ws.key,
            Signal::Fire,
            true,
        )
        .expect("armed green workspace dispatches");
        let resolver_called = std::cell::Cell::new(false);

        run_real_attempt_with_resolver(&config, ticket, || {
            resolver_called.set(true);
            async { None }
        })
        .await;

        assert!(!resolver_called.get(), "credential lookup must not run");
        assert!(matches!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(Latch::Blocked(_))
        ));
    }
}
