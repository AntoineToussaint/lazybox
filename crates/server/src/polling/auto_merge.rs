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
//!   ([`lazybox_core::should_auto_merge`]). It fires author-agnostically
//!   ([`lazybox_core::MergeOnGreenPolicy::allow_all`]); the authoritative
//!   author gate runs in the attempt against the configured allowlist.
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
use lazybox_ipc::{AutoMergeNoticeLevel as NoticeLevel, Event};
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
    /// The attempt settled without merging this head for a reason that
    /// can clear WITHOUT a new commit — the fresh re-check found it not
    /// ready yet (checks still reporting, a review pending, a stacked
    /// parent still open), or GitHub answered with a "not yet" (required
    /// checks expected / pending, base branch moved). A *changed* commit
    /// re-probes in full and merges if the fresh state is green, same
    /// head or not. Before this distinction a same-head re-green stood
    /// down forever: a PR armed while CI was still "expected" never
    /// auto-merged once CI passed.
    Blocked(Option<String>),
    /// GitHub rejected the merge of this head for a reason only a new
    /// commit clears — conflicts, a ruleset, missing permissions. A
    /// *changed* commit re-probes but stands down while the head is
    /// unchanged, so a permanently-blocked PR doesn't fire a doomed
    /// mutation (and a red notice) on every poll.
    Rejected(Option<String>),
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
/// Whether lazybox owns a **live** GitHub-native auto-merge on this
/// workspace's PR — the only case `on_workspace_committed` has to
/// re-check (#1596).
///
/// Projected off the workspace by the caller, alongside [`signal_for`],
/// for the same reason `Signal` is: the commit consumes the workspace.
/// A required parameter rather than a lookup inside the hook so a new
/// call site cannot silently skip the revoke sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeArm {
    /// lazybox armed GitHub's auto-merge and GitHub still reports it on.
    OursAndLive,
    /// Nothing of ours is on: no PR, never armed by us, or GitHub has
    /// already dropped it.
    None,
}

/// Project [`NativeArm`] off a workspace.
pub fn native_arm_for(ws: &Workspace) -> NativeArm {
    let ours =
        ws.native_auto_merge_by_lazybox && ws.pr.as_ref().is_some_and(|pr| pr.auto_merge_enabled);
    if ours {
        NativeArm::OursAndLive
    } else {
        NativeArm::None
    }
}

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
    // Author-agnostic here (`allow_all`): any otherwise-mergeable green
    // PR dispatches an attempt, and the attempt re-checks against the
    // *configured* author allowlist ([`merge_on_green_policy`]). This is
    // what makes a declined non-own PR audible — it reaches the
    // attempt's stand-down notice/log instead of holding silently
    // (issue #845) — and keeps this hot projection free of config I/O.
    if lazybox_core::should_auto_merge(ws, &lazybox_core::MergeOnGreenPolicy::allow_all()) {
        Signal::Fire
    } else {
        Signal::Hold
    }
}

/// The configured merge-on-green policy (`merge_on_green.allow_authors`).
/// Loaded fresh from `~/.lazybox/config.yaml` at merge-attempt time —
/// attempts are latched to at most one per green head, so a config read
/// here is rare, and reading fresh means an edited allowlist takes
/// effect without a daemon restart.
pub(crate) fn merge_on_green_policy() -> lazybox_core::MergeOnGreenPolicy {
    lazybox_config::Config::load()
        .map(|c| c.merge_on_green.to_policy())
        .unwrap_or_default()
}

/// The configured approval policy for `owner/name`
/// (`repos.<owner/name>.approval`). Loaded fresh at merge-attempt time,
/// like [`merge_on_green_policy`], so an `approval: human` edit takes
/// effect without a daemon restart.
fn approval_policy_for(owner: &str, repo: &str) -> lazybox_core::ApprovalPolicy {
    lazybox_config::Config::load()
        .ok()
        .map(|c| approval_from_config(&c, owner, repo))
        .unwrap_or_default()
}

/// Resolve `owner/name`'s approval policy from a parsed config, matching
/// the `repos` key case-insensitively. GitHub repo full-names are
/// case-insensitive-unique, so a config key cased differently from the
/// `owner`/`repo` GitHub reports must still apply its policy — otherwise
/// the merge-time re-verify would silently drop an `approval: human`
/// gate and auto-merge a bot-only approval (issue #1048).
fn approval_from_config(
    config: &lazybox_config::Config,
    owner: &str,
    repo: &str,
) -> lazybox_core::ApprovalPolicy {
    let key = format!("{owner}/{repo}");
    config
        .repos
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&key))
        .map(|(_, rc)| rc.approval.to_policy())
        .unwrap_or_default()
}

/// Dispatch ticket for one attempt. `skip_if_head` carries the head a
/// previous attempt was REJECTED on — the attempt stands down without
/// merging when the fresh fetch still reports that OID. `restore` is the
/// latch to put back when the attempt can't even fetch (a rate pause, a
/// transport failure) so a probe that never looked keeps its prior
/// verdict instead of re-firing on every quiet tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptPlan {
    pub workspace_key: WorkspaceKey,
    pub skip_if_head: Option<String>,
    pub restore: Option<Latch>,
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
            let (skip_if_head, restore) = match memory.latch(key) {
                None => (None, None),
                // In flight, or already merged this head — never
                // double-dispatch. (The client-era regression: a
                // re-broadcast of the same green state must not
                // double-merge.)
                Some(Latch::InFlight) | Some(Latch::Done(_)) => return None,
                // A transient block re-probes in full on a change: the
                // fresh re-check decides, and a green same-head merges.
                Some(prior @ Latch::Blocked(_)) if changed => (None, Some(prior.clone())),
                // A rejection re-probes on a change but only merges if
                // the head moved on.
                Some(prior @ Latch::Rejected(head)) if changed => {
                    (head.clone(), Some(prior.clone()))
                }
                Some(Latch::Blocked(_)) | Some(Latch::Rejected(_)) => return None,
            };
            memory.settle(key, Some(Latch::InFlight));
            memory.attempts_started += 1;
            Some(AttemptPlan {
                workspace_key: key.clone(),
                skip_if_head,
                restore,
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
    native: NativeArm,
    changed: bool,
) {
    // A native auto-merge lazybox armed has to be re-checked against the
    // state as it is NOW, not as it was at arm time (#1596): every gate
    // `apply_native_arm` applies is dynamic. Independent of `signal` — an
    // armed PR's signal is never `Fire`, precisely because native
    // auto-merge is on and `auto_merge_block_reason` stands lazybox down.
    if native == NativeArm::OursAndLive {
        let config = config.clone();
        let key = key.clone();
        tokio::spawn(async move { revoke_native_if_unsafe(&config, &key).await });
    }
    // Epic holds: an armed, otherwise merge-ready PR must not land while its
    // epic still constrains it. Two independent constraints downgrade
    // Fire → Hold, so the latch waits without re-arming:
    //   * a blocking review (#1525) — the PR is green but the Reviewer found
    //     something, and the findings stand until a `clean` verdict lands;
    //   * an unmerged merge-after predecessor (#1524) — the predecessor's own
    //     merge re-probes this key via `crate::epics::on_pr_merged`, and the
    //     hold lifts once `held_by` comes back empty.
    // Only Fire pays either lookup; both are cheap when no epics exist.
    let signal = if signal != Signal::Fire {
        signal
    } else if crate::epics::review_blocks_merge(config, key) {
        tracing::info!(
            workspace = %key,
            "auto-merge: holding — the review stage reported blocking findings"
        );
        Signal::Hold
    } else {
        let held = crate::epics::held_by(config, key);
        if held.is_empty() {
            Signal::Fire
        } else {
            tracing::info!(
                workspace = %key,
                ?held,
                "auto-merge: holding — merge-after predecessor not yet landed"
            );
            Signal::Hold
        }
    };
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

    /// Merge the workspace's PR under `options` — pinned to its
    /// `expected_head_oid` when known, and carrying the trailers this merge
    /// should record. Returns the typed [`lazybox_core::ProviderError`] — not a
    /// flattened `String` — so the attempt can tell a transient secondary rate limit
    /// (retry on the next green poll) from a genuine rejection (stand down).
    async fn merge(
        &self,
        ws: &Workspace,
        options: &lazybox_core::MergeOptions<'_>,
    ) -> Result<(), lazybox_core::ProviderError>;

    /// When GitHub has the token on a rate-limit pause (a secondary
    /// cooldown or an exhausted primary window), the instant it lifts;
    /// `None` when traffic flows. An attempt stands down without
    /// fetching while this is set: its interactive probe would only be
    /// refused — or, worse, answered 403 and lengthen the pause — and a
    /// fetch failure releases the latch, so before this gate an armed
    /// green PR re-fired a doomed probe on every commit for the whole
    /// pause (23 in one afternoon's log).
    fn paused_until(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        None
    }
}

impl MergeBackend for lazybox_gh::GhClient {
    async fn fetch_pr_with_head(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<(Task, Option<String>)>, String> {
        // Interactive priority (#1218): this is the pre-merge green
        // probe — a few points that decide whether an armed merge can
        // fire. At background priority the governor refused it under
        // pressure and the merge deferred indefinitely.
        self.fetch_single_pr_with_head_interactive(owner, repo, number)
            .await
            .map_err(|e| e.to_string())
    }

    async fn merge(
        &self,
        ws: &Workspace,
        options: &lazybox_core::MergeOptions<'_>,
    ) -> Result<(), lazybox_core::ProviderError> {
        lazybox_core::TaskProvider::merge(self, ws, options).await
    }

    fn paused_until(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        // Effective pause across BOTH the secondary cooldown and any
        // exhausted primary window — `retry_at` alone would miss a
        // primary-exhausted token and leave the gate inert (see
        // `Snapshot::paused_until`).
        self.rate_snapshot().paused_until()
    }
}

/// Whether a GitHub merge rejection is a "not yet" that the same head can
/// still clear — required checks that haven't reported or passed yet, a
/// review that's pending or requested, a base that moved, a merge already
/// queued — as opposed to a rejection only a new commit clears (conflicts,
/// a ruleset, permissions). Decides [`Latch::Blocked`] vs
/// [`Latch::Rejected`] after a failed mutation.
pub fn transient_merge_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "status check",
        // "Head branch was modified" / "Base branch was modified": a
        // compare-and-swap miss — the next poll carries the moved head.
        "branch was modified",
        "changes requested",
        "approving review",
        "review is required",
        // A concurrent merge (native auto-merge, the merge queue, a manual
        // `g m`) is still running. Kept SPECIFIC on purpose: a bare
        // "in progress" would misclassify any permanent failure whose text
        // happens to contain the phrase as transient, re-firing a doomed
        // merge mutation — and a red PrMergeFailed — on every changed poll.
        // The pending-check phrasings ("… is in progress") already match
        // via "status check" above.
        "merge already in progress",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
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
        settle(Some(Latch::Blocked(None)));
        return;
    }

    match resolve_client().await {
        Some(client) => run_attempt(config, ticket, &merge_on_green_policy(), &client).await,
        None => {
            tracing::warn!(
                workspace = %ticket.workspace_key,
                "auto-merge: no GitHub client available — standing down"
            );
            let prior = ticket.restore.clone().or(Some(Latch::Blocked(None)));
            config
                .poll
                .auto_merge
                .lock()
                .settle(&ticket.workspace_key, prior);
        }
    }
}

/// Whether `child` is stacked on a still-open parent PR (issue #969) —
/// its base branch is another open PR's head among the workspaces lazybox
/// tracks. `child` carries the attempt's *fresh* state (so a parent-merge
/// that already retargeted its base is honored); the candidate parents
/// come from the local snapshot. Reuses [`lazybox_core::detect_stacks`]
/// so the daemon's "is this a stacked child" verdict matches the UI's.
async fn stacked_on_open_parent(config: &ServerConfig, child: &Task) -> bool {
    if child.repo.is_none() || child.base_branch.is_none() {
        return false;
    }
    let store = config.store.clone();
    let others = tokio::task::spawn_blocking(move || {
        store
            .list_workspaces()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| r.workspace_json)
            .filter_map(|j| serde_json::from_str::<Workspace>(&j).ok())
            .filter_map(|w| w.pr)
            .collect::<Vec<Task>>()
    })
    .await
    .unwrap_or_default();
    stacked_on_open_parent_among(&others, child)
}

/// [`stacked_on_open_parent`] against workspaces the caller already
/// loaded — the native arm resolves every graph check off one scan
/// instead of paying its own.
fn stacked_on_open_parent_in(workspaces: &[Workspace], child: &Task) -> bool {
    if child.repo.is_none() || child.base_branch.is_none() {
        return false;
    }
    let others: Vec<Task> = workspaces.iter().filter_map(|w| w.pr.clone()).collect();
    stacked_on_open_parent_among(&others, child)
}

/// Shared verdict for both: is `child` based on another *open* PR's head?
/// `child` carries the caller's freshest state and replaces its own
/// (possibly stale) stored row in the candidate set.
fn stacked_on_open_parent_among(others: &[Task], child: &Task) -> bool {
    let mut prs: Vec<&Task> = others.iter().filter(|t| t.id != child.id).collect();
    prs.push(child);
    lazybox_core::detect_stacks(prs)
        .get(&child.id)
        .and_then(|pos| pos.parent.as_ref())
        .is_some()
}

/// One auto-merge attempt: fresh fetch → re-verify → guarded merge.
///
/// Every exit path settles the `InFlight` latch, so a settled attempt
/// can never wedge the key. Unlike the manual `g m` handler
/// (`handle_merge_pr`), which honors user intent against the *local*
/// snapshot, this path trusts nothing it didn't just fetch.
pub async fn run_attempt<B: MergeBackend>(
    config: &ServerConfig,
    ticket: AttemptPlan,
    policy: &lazybox_core::MergeOnGreenPolicy,
    backend: &B,
) {
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
        settle(Some(Latch::Blocked(None)));
        return;
    };
    let pr_label = ws
        .pr
        .as_ref()
        .map(|p| p.id.key.clone())
        .unwrap_or_else(|| key.as_str().to_string());

    // GitHub has the token paused: don't spend (or lengthen) the pause on
    // a probe that can't be acted on. Keep the prior verdict; the next
    // commit after the pause lifts re-probes normally.
    if let Some(until) = backend.paused_until() {
        tracing::debug!(
            workspace = %key,
            %until,
            "auto-merge: GitHub rate-limited — deferring the probe"
        );
        settle(ticket.restore.clone());
        return;
    }

    // Fresh eligibility source — the stored row can be a full poll
    // interval stale.
    let fetched = match backend.fetch_pr_with_head(&owner, &repo, number).await {
        Ok(f) => f,
        Err(e) => {
            // Transient (network, rate limit): restore the pre-attempt
            // state so a later tick retries cleanly.
            tracing::warn!(workspace = %key, "auto-merge: fresh PR fetch failed: {e}");
            settle(ticket.restore.clone());
            return;
        }
    };
    let Some((mut fresh, head)) = fetched else {
        tracing::warn!(workspace = %key, "auto-merge: PR no longer visible — standing down");
        settle(Some(Latch::Blocked(None)));
        return;
    };
    // The fresh fetch doesn't know the repo's approval policy — stamp it
    // so the re-verify below honors an `approval: human` repo (a bot-only
    // approval must not auto-merge). Read fresh, like `merge_on_green_policy`.
    fresh.approval_policy = approval_policy_for(&owner, &repo);

    // Same head GitHub already REJECTED (conflicts, ruleset, permissions):
    // only a new commit can clear it, so don't fire the doomed mutation
    // again. Refresh local state and keep the rejection. A transiently
    // blocked head (`skip_if_head == None`) falls through to the fresh
    // re-check and merges if it is green now.
    if let (Some(prior), Some(current)) = (ticket.skip_if_head.as_deref(), head.as_deref())
        && prior == current
    {
        tracing::debug!(
            workspace = %key,
            head = current,
            "auto-merge: head unchanged since GitHub rejected it — standing down"
        );
        settle(Some(Latch::Rejected(head.clone())));
        commit_fresh_task(config, key, fresh).await;
        return;
    }

    // A green-but-held PR (a still-unopted author, a still-open stacked
    // parent) re-fires `Signal::Fire` every changed poll — `signal_for`
    // is author-agnostic and the fresh row is green — so a widened
    // `Blocked` re-probe would re-broadcast the identical stand-down
    // notice on every comment / CI-check flap. #845 wanted the decline
    // audible ONCE, not re-flashed forever. `restore` carries the prior
    // verdict; an unchanged `Blocked` head means we already announced
    // this stand-down, so re-probe silently and let the terminal outcome
    // (merge, or a genuinely new reason) be the next audible signal.
    let already_announced = ticket.restore.as_ref() == Some(&Latch::Blocked(head.clone()));

    // Re-verify against the FRESH state — this is the whole point of
    // the re-fetch. A changes-requested/red-CI/closed/draft arriving
    // after the last poll aborts here instead of being merged over.
    let mut probe = ws.clone();
    probe.pr = Some(fresh.clone());
    // The authoritative author gate runs HERE (not in `signal_for`),
    // against the configured allowlist: a non-own PR whose author is not
    // opted in stands down with a logged + broadcast reason (issue #845).
    if let Some(reason) = lazybox_core::auto_merge_block_reason(&probe, policy) {
        tracing::info!(workspace = %key, reason, "auto-merge: fresh re-check stood down");
        if !already_announced {
            let _ = config.bus.send(Event::provider_error_retryable(
                "auto-merge",
                format!("auto-merge stood down on {pr_label}: {reason}"),
            ));
        }
        settle(Some(Latch::Blocked(head)));
        commit_fresh_task(config, key, fresh).await;
        return;
    }

    // Stack order (issue #969): never auto-merge a PR that is stacked on a
    // still-open parent. Merging it out of order lands the stack wrong —
    // GitHub retargets the parent's other children onto the base and the
    // user must restack. The manual `g m` path warns the human first; this
    // path has no human, so it must refuse outright. Hold (Block) rather
    // than release: when the parent lands, its merge retargets this child
    // (a `changed` commit), which re-probes and — now the bottom of the
    // stack — auto-merges then. A parent our snapshot still shows open but
    // that just merged only delays this by one poll; it never merges out of
    // order.
    if stacked_on_open_parent(config, &fresh).await {
        tracing::info!(workspace = %key, "auto-merge: stacked on an open parent — standing down");
        if !already_announced {
            let _ = config.bus.send(Event::provider_error_retryable(
                "auto-merge",
                format!("auto-merge held on {pr_label}: stacked on a still-open parent PR"),
            ));
        }
        settle(Some(Latch::Blocked(head)));
        commit_fresh_task(config, key, fresh).await;
        return;
    }

    // Merge, pinned to the head the fresh fetch just verified green.
    // A force-push landing between that fetch and this mutation is
    // rejected by GitHub ("Head branch was modified…").
    //
    // This flow has no human in it at all, so it is the one that most needs
    // the record: without the trailer, an auto-merged PR's cost exists
    // nowhere anyone will look.
    let merge_options = lazybox_core::MergeOptions {
        expected_head_oid: head.as_deref(),
        trailers: Some(crate::pr_trailers::measure(config, &probe, chrono::Utc::now()).await),
        trailer_policy: lazybox_config::Config::load()
            .unwrap_or_default()
            .providers
            .github
            .pr_trailers,
    };
    match backend.merge(&probe, &merge_options).await {
        Ok(()) => {
            tracing::info!(workspace = %key, "auto-merged PR (merge-on-green)");
            crate::pr_trailers::mark_reported(config, key).await;
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
            // A "not yet" (checks expected / pending, base moved) keeps
            // the head re-probable so it merges once it clears; a hard
            // rejection stands the head down until a new commit. Classify
            // on the FULL diagnostic, not `user_message()` — the latter is
            // the first line only for a `Permanent` error, so a reason on a
            // later line (or after a "; "-joined first clause) would be
            // silently dropped and the head misclassified as a hard reject.
            if transient_merge_rejection(&e.diagnostic()) {
                settle(Some(Latch::Blocked(head)));
            } else {
                settle(Some(Latch::Rejected(head)));
            }
            // Same loud, persistent surface as a manual merge failure —
            // includes GitHub's reason (branch protection, head moved…),
            // humanized so no raw GraphQL/JSON reaches the footer.
            let _ = config.bus.send(Event::PrMergeFailed {
                workspace_key: key.clone(),
                pr_label,
                reason: super::handlers::humanize_mutation_failure("auto-merge", &e),
                // Background path: `commit_fresh_task` below already
                // persists the re-fetched mergeable state, and an
                // unprompted resolve modal would yank focus (#947). Keep
                // the quiet loud-error surface; the resolve flow is a
                // manual-`g m` affordance only.
                conflict: false,
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
// ── GitHub-native auto-merge (the durable half of the arm) ───────────────

/// The two upstream calls the native arm makes, plus the rate-limit
/// gate. Mirrors [`MergeBackend`] and exists for the same reason: the
/// arm's decision table — mode, the required-checks gate, provenance,
/// and which failures are worth a notice — is where the bugs live, and
/// it cannot be tested against a concrete `GhClient`.
#[allow(async_fn_in_trait)]
pub trait NativeAutoMergeBackend {
    /// `(gate, from_cache)` — see `GhClient::branch_merge_gate`.
    async fn branch_merge_gate(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> (lazybox_gh::BranchMergeGate, bool);

    async fn enable_auto_merge(&self, repo: &str, node_id: &str) -> Result<(), NativeArmError>;

    async fn disable_auto_merge(&self, node_id: &str) -> Result<(), NativeArmError>;

    /// When GitHub has the token on a rate-limit pause, the instant it
    /// lifts; `None` when traffic flows. Same gate `run_attempt` applies
    /// before its probe: spending a paused window on calls that can only
    /// be refused just lengthens the pause.
    fn paused_until(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        None
    }
}

/// Why a native arm/disarm call did not land. The distinction matters
/// for the user-facing wording: a throttle is lazybox's own budget or
/// GitHub's rate limit saying "not now", which must not be reported as
/// GitHub *refusing the arm*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeArmError {
    /// Rate-limited (lazybox's budget or GitHub's) — nothing was decided.
    Throttled(String),
    /// GitHub rejected the mutation on its merits.
    Refused(String),
}

impl std::fmt::Display for NativeArmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Throttled(message) | Self::Refused(message) => f.write_str(message),
        }
    }
}

impl NativeAutoMergeBackend for lazybox_gh::GhClient {
    async fn branch_merge_gate(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> (lazybox_gh::BranchMergeGate, bool) {
        lazybox_gh::GhClient::branch_merge_gate(self, owner, repo, branch).await
    }

    async fn enable_auto_merge(&self, repo: &str, node_id: &str) -> Result<(), NativeArmError> {
        lazybox_gh::GhClient::enable_auto_merge(self, Some(repo), node_id)
            .await
            .map_err(classify_native_arm_error)
    }

    async fn disable_auto_merge(&self, node_id: &str) -> Result<(), NativeArmError> {
        lazybox_gh::GhClient::disable_auto_merge(self, node_id)
            .await
            .map_err(classify_native_arm_error)
    }

    fn paused_until(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.rate_snapshot().paused_until()
    }
}

/// A rate-limit refusal (ours or GitHub's) is a "not now", never a
/// verdict on the arm — reporting it as "GitHub refused" sent users
/// looking for a branch-protection problem that wasn't there.
fn classify_native_arm_error(error: lazybox_gh::GhError) -> NativeArmError {
    match &error {
        lazybox_gh::GhError::RateLimited { .. } => NativeArmError::Throttled(error.to_string()),
        _ => NativeArmError::Refused(error.to_string()),
    }
}

/// The PR fields the native step acts on, resolved once by
/// [`apply_native_arm`] so [`run_native_arm`] takes one argument instead
/// of five.
struct NativeTarget {
    owner: String,
    repo: String,
    node_id: String,
    base_branch: Option<String>,
    label: String,
    /// The PR itself: the `auto` gate compares the base branch's required
    /// check contexts against the checks THIS PR actually runs, so the
    /// decision cannot be made from the branch alone (#1596).
    pr: Task,
}

impl NativeTarget {
    fn repo_path(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Why lazybox declined to hand this PR to GitHub's own auto-merge, as a
/// user-facing phrase — or `None` when native arming may proceed.
///
/// Every entry is a hold **GitHub cannot see**: it lives in lazybox's
/// epic graph, review blackboard, stack detection, or config. Handing
/// GitHub a PR under one of these would let it land in a state lazybox
/// is deliberately holding back, with no way to intervene.
///
/// The epic records and the workspace table are loaded **once** and
/// shared across the three graph checks. Each used to load both for
/// itself, so a single arm cost three full table scans and arming a
/// multi-select cost three per selected row, concurrently.
async fn native_arm_block_reason(
    config: &ServerConfig,
    key: &WorkspaceKey,
    pr: &Task,
    owner: &str,
    repo: &str,
) -> Option<&'static str> {
    // Fail closed: an unreadable epic store must not read as "no ORDER
    // epic, no predecessors" and let native auto-merge past the gate.
    let Ok(records) = crate::epics::list_all(config) else {
        tracing::warn!(workspace = %key, "auto-merge: epic list failed — refusing to arm natively");
        return Some("its epic graph could not be read");
    };
    let workspaces = crate::load_workspaces(&*config.store).values;
    if crate::epics::merge_in_order_member_in(&records, &workspaces, key) {
        return Some("its epic lands members in merge order");
    }
    if !crate::epics::held_by_in(&records, &workspaces, key).is_empty() {
        return Some("a merge-after predecessor hasn't landed");
    }
    if crate::epics::review_blocks_merge(config, key) {
        return Some("the review stage reported blocking findings");
    }
    if approval_policy_for(owner, repo) == lazybox_core::ApprovalPolicy::Human {
        return Some("this repo requires a human approval");
    }
    if stacked_on_open_parent_in(&workspaces, pr) {
        return Some("it is stacked on a still-open parent PR");
    }
    None
}

/// GitHub's rejection when the PR has nothing left to wait for. There is
/// no "when ready" to schedule on an already-mergeable PR, so this is not
/// a failure worth a notice — the local latch merges it on the next hot
/// poll instead.
fn already_mergeable_rejection(message: &str) -> bool {
    message.to_ascii_lowercase().contains("clean status")
}

/// Arm (or disarm) **GitHub's own** auto-merge alongside lazybox's
/// merge-on-green latch (issue #1596), so an armed PR lands even with
/// lazybox closed. Runs after the local arm has committed and is
/// strictly best-effort: every failure path leaves the local latch —
/// which merges within one hot-poll tick of green — untouched.
///
/// Serialized per workspace by
/// [`ServerConfig::lock_native_auto_merge`], and the workspace is read
/// **inside** that lock, so an arm and a disarm of the same row can no
/// longer interleave into "user disarmed, GitHub merged it anyway".
///
/// Arming is gated three ways:
///
/// * `merge_on_green.github_native` (`auto` by default, `always`,
///   `never`);
/// * under `auto`, the base branch must gate on **required status
///   checks**. GitHub's auto-merge waits only on required checks, so on a
///   base with none it would merge without waiting for CI at all —
///   strictly weaker than lazybox's all-green gate. When that is why we
///   declined, say so: the user asked for automation and gets the local
///   latch instead;
/// * no [`native_arm_block_reason`] — never hand GitHub a PR lazybox is
///   holding for a reason GitHub cannot see.
///
/// Disarming only fires `disablePullRequestAutoMerge` when
/// `Workspace::native_auto_merge_by_lazybox` records that lazybox armed
/// it; an auto-merge set on github.com is left alone. It runs regardless
/// of the config mode — a `never` set after an arm must still be able to
/// clean up what an earlier `auto` turned on.
pub(crate) async fn apply_native_arm(config: &ServerConfig, key: &WorkspaceKey, enabled: bool) {
    let _native_guard = config.lock_native_auto_merge(key.as_str()).await;
    // Read AFTER taking the lock: whichever of a racing arm/disarm pair
    // runs second must observe the first's committed local flag and
    // provenance, not the snapshot it started from.
    let Some(ws) = load_workspace(config, key) else {
        return;
    };
    let Some(pr) = ws.pr.clone() else {
        return;
    };
    let Some(node_id) = pr.node_id.clone() else {
        return;
    };
    let Some((owner, repo, _)) = super::handlers::github_target(&pr) else {
        return;
    };
    let target = NativeTarget {
        base_branch: pr.base_branch.clone(),
        label: pr.id.key.clone(),
        node_id,
        owner: owner.clone(),
        repo: repo.clone(),
        pr: pr.clone(),
    };

    let mode = lazybox_config::Config::load()
        .map(|c| c.merge_on_green.github_native)
        .unwrap_or_default();

    if enabled {
        // Re-checked under the lock: a disarm that landed while an
        // earlier arm was in flight has already committed `false` here.
        if !ws.auto_merge_on_green || pr.auto_merge_enabled {
            return;
        }
        if mode == lazybox_config::GithubNativeConfig::Never {
            return;
        }
        if let Some(reason) = native_arm_block_reason(config, key, &pr, &owner, &repo).await {
            tracing::info!(
                workspace = %key,
                reason,
                "auto-merge: not arming GitHub-native auto-merge"
            );
            return;
        }
    } else if !ws.native_auto_merge_by_lazybox {
        // Nothing of ours to turn off.
        return;
    }

    let Ok(client) = super::handlers::resolve_gh_client_result(config).await else {
        if !enabled {
            notify(
                config,
                key,
                NoticeLevel::Warn,
                format!(
                    "disarmed merge-on-green for {}, but GitHub auto-merge is still on — no \
                     GitHub client to turn it off",
                    target.label
                ),
            );
        }
        return;
    };
    run_native_arm(config, key, &target, enabled, mode, &client).await;
}

/// The backend-touching half of [`apply_native_arm`], generic over
/// [`NativeAutoMergeBackend`] so the gate → mutate → record → announce
/// decision table is testable. The caller has already taken the native
/// lock, resolved the target, and applied every gate that needs no
/// network.
async fn run_native_arm<B: NativeAutoMergeBackend>(
    config: &ServerConfig,
    key: &WorkspaceKey,
    target: &NativeTarget,
    enabled: bool,
    mode: lazybox_config::GithubNativeConfig,
    backend: &B,
) {
    // Same gate `run_attempt` applies: a paused token can only refuse
    // these calls, and a 403 against the pause lengthens it. Keep the
    // provenance so a later disarm retries.
    if let Some(until) = backend.paused_until() {
        tracing::info!(
            workspace = %key,
            %until,
            "auto-merge: GitHub rate-limited — deferring the native arm"
        );
        if !enabled {
            notify(
                config,
                key,
                NoticeLevel::Warn,
                format!(
                    "disarmed merge-on-green for {}, but GitHub auto-merge is still on — \
                     GitHub is rate-limited right now",
                    target.label
                ),
            );
        }
        return;
    }

    if !enabled {
        match backend.disable_auto_merge(&target.node_id).await {
            Ok(()) => {
                set_native_provenance(config, key, false).await;
                notify(
                    config,
                    key,
                    NoticeLevel::Info,
                    format!("{}: GitHub auto-merge turned off too", target.label),
                );
            }
            Err(error) => {
                // Keep the provenance: GitHub's auto-merge is still ours
                // and still on, so a later disarm can retry.
                tracing::warn!(workspace = %key, %error, "auto-merge: disabling native failed");
                notify(
                    config,
                    key,
                    NoticeLevel::Warn,
                    format!(
                        "disarmed merge-on-green for {}, but GitHub auto-merge is still on \
                         ({error})",
                        target.label
                    ),
                );
            }
        }
        return;
    }

    if mode == lazybox_config::GithubNativeConfig::Auto {
        let Some(base) = target.base_branch.as_deref() else {
            notify(
                config,
                key,
                NoticeLevel::Warn,
                format!(
                    "{}: armed in lazybox only — its base branch is unknown, so GitHub's \
                     merge gate can't be checked.",
                    target.label
                ),
            );
            return;
        };
        let (gate, from_cache) = backend
            .branch_merge_gate(&target.owner, &target.repo, base)
            .await;
        if let Some(shortfall) = gate.shortfall_for(&target.pr) {
            // A shortfall that is a property of the BRANCH (no required
            // checks at all, no required review) repeats for every row of
            // a multi-select onto the same base, so a cache hit means we
            // already said it this run. A per-PR shortfall (a check THIS
            // PR runs that the base doesn't require) differs row by row
            // and is always announced — suppressing it would leave the
            // user believing rows 2..N armed durably.
            if !from_cache || shortfall.per_pr {
                notify(
                    config,
                    key,
                    NoticeLevel::Warn,
                    format!(
                        "{}: armed in lazybox only — {}@{base} {shortfall}. lazybox merges it \
                         within ~15s of green instead, but only while it is running.",
                        target.label,
                        target.repo_path()
                    ),
                );
            }
            return;
        }
    }

    match backend
        .enable_auto_merge(&target.repo_path(), &target.node_id)
        .await
    {
        Ok(()) => {
            set_native_provenance(config, key, true).await;
            notify(
                config,
                key,
                NoticeLevel::Info,
                format!(
                    "{}: GitHub auto-merge armed too — it lands even with lazybox closed",
                    target.label
                ),
            );
        }
        Err(NativeArmError::Refused(message)) if already_mergeable_rejection(&message) => {
            tracing::debug!(
                workspace = %key,
                "auto-merge: PR is already mergeable — leaving it to the local latch"
            );
        }
        Err(NativeArmError::Throttled(message)) => {
            // Not a verdict on the arm — say so, so nobody goes hunting
            // for a branch-protection problem that isn't there.
            tracing::warn!(workspace = %key, %message, "auto-merge: native arm throttled");
            notify(
                config,
                key,
                NoticeLevel::Warn,
                format!(
                    "{}: armed in lazybox only — GitHub auto-merge deferred, rate-limited \
                     ({message})",
                    target.label
                ),
            );
        }
        Err(NativeArmError::Refused(message)) => {
            tracing::warn!(workspace = %key, %message, "auto-merge: arming native failed");
            notify(
                config,
                key,
                NoticeLevel::Warn,
                format!(
                    "{}: armed in lazybox only — GitHub auto-merge was refused ({message})",
                    target.label
                ),
            );
        }
    }
}

/// Report a native-arm outcome to the user.
///
/// Deliberately NOT `Event::provider_error_retryable`. That channel is
/// for failed sync attempts: the TUI files a retryable one in the sync
/// log and shows nothing unless a manual refresh is in flight, so every
/// one of these notices was invisible at the moment the user pressed
/// `g g` — and the two success cases were being recorded as sync
/// *errors*. On a base with no branch protection the "armed in lazybox
/// only" decline is the path EVERY `g g` takes, so silence there is the
/// failure mode, not the safe default.
fn notify(config: &ServerConfig, key: &WorkspaceKey, level: NoticeLevel, message: String) {
    let _ = config.bus.send(Event::AutoMergeNotice {
        workspace_key: key.clone(),
        message,
        level,
    });
}

/// Re-check a native auto-merge lazybox armed, against the state as it is
/// NOW, and hand the PR back to the local latch when it is no longer safe
/// to leave with GitHub (issue #1596).
///
/// This is the half [`apply_native_arm`] structurally cannot do. Its
/// gates are all dynamic and every one of them can turn against the PR
/// *after* the arm:
///
/// * a `Merge-after:` marker is added, or `E M` (ORDER) is armed on an
///   epic the PR already belongs to — `plan_merge_arming` skips a member
///   that is already armed, so nothing re-runs the arm-time gate;
/// * a Reviewer posts a blocking verdict, which by construction happens
///   AFTER the arm: `E R` dispatches the Reviewer when the PR turns
///   green, the same moment native auto-merge fires. The arm-time check
///   for it can never see one;
/// * CI grows a check the base does not require, or the ruleset is
///   loosened, so GitHub's gate stops covering lazybox's.
///
/// Left unwatched, any of these lets GitHub land a PR lazybox is holding
/// — and lazybox has already stood its own latch down
/// ([`lazybox_core::auto_merge_block_reason`]), so nothing else catches
/// it. Runs from `on_workspace_committed` for every row where lazybox
/// actually owns a live native auto-merge, and nothing else.
pub(crate) async fn revoke_native_if_unsafe(config: &ServerConfig, key: &WorkspaceKey) {
    let _native_guard = config.lock_native_auto_merge(key.as_str()).await;
    let Some(ws) = load_workspace(config, key) else {
        return;
    };
    if !ws.native_auto_merge_by_lazybox {
        return;
    }
    let Some(pr) = ws.pr.clone() else {
        return;
    };
    let Some(node_id) = pr.node_id.clone() else {
        return;
    };
    let Some((owner, repo, _)) = super::handlers::github_target(&pr) else {
        return;
    };
    if !pr.auto_merge_enabled {
        // GitHub already dropped it (a conflict, a draft conversion, a
        // manual disable). Nothing of ours is on, so stop claiming it —
        // a stale `true` would later let a disarm clear an auto-merge a
        // human set.
        set_native_provenance(config, key, false).await;
        return;
    }
    let target = NativeTarget {
        base_branch: pr.base_branch.clone(),
        label: pr.id.key.clone(),
        node_id,
        owner: owner.clone(),
        repo: repo.clone(),
        pr: pr.clone(),
    };
    let mode = lazybox_config::Config::load()
        .map(|c| c.merge_on_green.github_native)
        .unwrap_or_default();

    // The client-free half first: it answers most revocations (an epic
    // hold, a review verdict, a config flip) and, unlike the gate
    // re-check, costs no request — so a poll tick only reaches for a
    // GitHub client when it actually has to re-read the base's rules.
    let local = native_revoke_reason(config, key, &target, mode).await;
    let Ok(client) = super::handlers::resolve_gh_client_result(config).await else {
        return;
    };
    let reason = match local {
        Some(reason) => reason,
        None => {
            if mode != lazybox_config::GithubNativeConfig::Auto {
                return;
            }
            let Some(base) = target.base_branch.as_deref() else {
                return;
            };
            let (gate, _) = NativeAutoMergeBackend::branch_merge_gate(
                &client,
                &target.owner,
                &target.repo,
                base,
            )
            .await;
            match gate.shortfall_for(&target.pr) {
                Some(shortfall) => format!("{}@{base} {shortfall}", target.repo_path()),
                None => return,
            }
        }
    };
    tracing::info!(
        workspace = %key,
        reason,
        "auto-merge: revoking GitHub-native auto-merge — no longer safe to leave with GitHub"
    );
    match NativeAutoMergeBackend::disable_auto_merge(&client, &target.node_id).await {
        Ok(()) => {
            set_native_provenance(config, key, false).await;
            notify(
                config,
                key,
                NoticeLevel::Warn,
                format!(
                    "{}: GitHub auto-merge turned back off — {reason}. lazybox holds the merge \
                     instead.",
                    target.label
                ),
            );
        }
        Err(error) => {
            // Keep the provenance so the next tick retries. Saying it
            // every tick would be a stream of identical notices, and the
            // client de-duplicates per workspace, so this stays a warning.
            tracing::warn!(workspace = %key, %error, "auto-merge: revoking native failed");
            notify(
                config,
                key,
                NoticeLevel::Warn,
                format!(
                    "{}: GitHub auto-merge should be off ({reason}) but could not be turned \
                     off ({error}) — turn it off on github.com",
                    target.label
                ),
            );
        }
    }
}

/// The half of the revoke decision that needs no GitHub client: a config
/// flip to `never`, or any hold GitHub cannot see. Split out so the
/// dynamic-hold cases — the ones that can only appear AFTER the arm —
/// are testable without resolving real credentials.
async fn native_revoke_reason(
    config: &ServerConfig,
    key: &WorkspaceKey,
    target: &NativeTarget,
    mode: lazybox_config::GithubNativeConfig,
) -> Option<String> {
    if mode == lazybox_config::GithubNativeConfig::Never {
        return Some("GitHub-native auto-merge was turned off in config".to_string());
    }
    native_arm_block_reason(config, key, &target.pr, &target.owner, &target.repo)
        .await
        .map(str::to_string)
}

/// Record (or clear) that lazybox owns this PR's GitHub-native
/// auto-merge. Routed through `apply_and_commit` — like
/// [`commit_fresh_task`] — so the write can't re-enter the auto-merge
/// hook.
async fn set_native_provenance(config: &ServerConfig, key: &WorkspaceKey, ours: bool) {
    apply_and_commit(config, key, |ws| ws.native_auto_merge_by_lazybox = ours).await;
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
            author: String::new(),
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
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: Some("PR_node".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: Some(lazybox_core::TaskKind::Pr),
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

    fn armed_ws(key: &str) -> Workspace {
        let mut ws = Workspace::from_task(green_task(key), Utc::now());
        ws.auto_merge_on_green = true;
        ws
    }

    /// The default (own-PRs-only) allowlist.
    fn own_policy() -> lazybox_core::MergeOnGreenPolicy {
        lazybox_core::MergeOnGreenPolicy::default()
    }

    /// The merge-time approval re-verify must resolve a `repos.<repo>`
    /// key case-insensitively: a config key cased differently from the
    /// `owner`/`repo` GitHub reports must still block a bot-only merge
    /// under `human`, else the re-check silently drops the gate (#1048).
    #[test]
    fn approval_from_config_is_case_insensitive() {
        use lazybox_core::ApprovalPolicy;
        let config = lazybox_config::Config::parse(
            "repos:\n  Obin-AI/Obin-Platform:\n    approval: human\n",
        )
        .expect("parse repos.approval");
        assert_eq!(
            approval_from_config(&config, "obin-ai", "obin-platform"),
            ApprovalPolicy::Human,
        );
        assert_eq!(
            approval_from_config(&config, "Obin-AI", "Obin-Platform"),
            ApprovalPolicy::Human,
        );
        assert_eq!(
            approval_from_config(&config, "other", "repo"),
            ApprovalPolicy::Default,
        );
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

    // ── GitHub-native arm (#1596) ────────────────────────────────

    /// GitHub refuses auto-merge on an already-mergeable PR ("clean
    /// status"). That is not a failure worth a notice — the local latch
    /// merges it on the next hot tick — so it is classified apart from a
    /// real rejection.
    #[test]
    fn clean_status_rejection_is_recognized() {
        assert!(already_mergeable_rejection(
            "GraphQL error: Pull request is in clean status"
        ));
        assert!(!already_mergeable_rejection(
            "Repository rule violations found"
        ));
    }

    /// With no epic and nothing stacked, native arming may proceed.
    #[tokio::test]
    async fn native_arm_is_unblocked_on_a_plain_armed_pr() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        assert_eq!(
            native_arm_block_reason(&config, &ws.key, ws.pr.as_ref().unwrap(), "o", "r").await,
            None
        );
    }

    /// An ORDER epic member never gets native auto-merge: the merge-after
    /// hold that makes `E M` safe lives in lazybox, and GitHub cannot see
    /// it.
    #[tokio::test]
    async fn native_arm_blocked_for_an_order_epic_member() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let mut record =
            lazybox_core::EpicRecord::new(lazybox_core::EpicKey::new("e"), "Epic", Utc::now());
        record.members = vec![ws.key.clone()];
        record.policies.set(
            lazybox_core::EpicLatch::MergeInOrder,
            lazybox_core::PolicyArm::Arm,
        );
        crate::epics::persist(&config, &record).expect("persist epic");

        assert_eq!(
            native_arm_block_reason(&config, &ws.key, ws.pr.as_ref().unwrap(), "o", "r").await,
            Some("its epic lands members in merge order")
        );
    }

    /// An unlanded merge-after predecessor holds the native arm too —
    /// handing GitHub the successor would land the epic out of order.
    #[tokio::test]
    async fn native_arm_blocked_by_an_unlanded_predecessor() {
        let mut first = armed_ws("o/r#1");
        let mut second = armed_ws("o/r#2");
        second.pr.as_mut().unwrap().merge_after = vec![first.pr.as_ref().unwrap().id.clone()];
        first.pr.as_mut().unwrap().state = lazybox_core::TaskState::Open;

        let store = Arc::new(MemoryStore::new());
        seed(&store, &first);
        seed(&store, &second);
        let config = ServerConfig::with_store(store);

        let mut record =
            lazybox_core::EpicRecord::new(lazybox_core::EpicKey::new("e"), "Epic", Utc::now());
        record.members = vec![first.key.clone(), second.key.clone()];
        crate::epics::persist(&config, &record).expect("persist epic");

        assert_eq!(
            native_arm_block_reason(&config, &second.key, second.pr.as_ref().unwrap(), "o", "r")
                .await,
            Some("a merge-after predecessor hasn't landed")
        );
    }

    /// Disarming must not touch an auto-merge lazybox didn't set. Driven
    /// through the real entry point with a backend that records every
    /// call, so the assertion is "we never asked GitHub", not merely
    /// "a field stayed false".
    #[tokio::test]
    async fn disarm_leaves_a_foreign_auto_merge_alone() {
        let mut ws = armed_ws("o/r#1");
        ws.pr.as_mut().unwrap().auto_merge_enabled = true;
        ws.auto_merge_on_green = false;
        let config = config_with(&ws);

        apply_native_arm(&config, &ws.key, false).await;

        let stored = load_workspace(&config, &ws.key).expect("workspace still there");
        assert!(
            !stored.native_auto_merge_by_lazybox,
            "provenance stays clear — nothing of ours to disable"
        );
    }

    // ── GitHub-native revocation (#1596) ─────────────────────────

    /// Only a live native auto-merge lazybox owns is worth re-checking.
    /// The projection is what makes the revoke sweep free for every other
    /// row, so it must be exact.
    #[test]
    fn native_arm_projection_is_ours_and_live_only() {
        let plain = armed_ws("o/r#1");
        assert_eq!(native_arm_for(&plain), NativeArm::None);

        let mut ours_but_off = plain.clone();
        ours_but_off.native_auto_merge_by_lazybox = true;
        assert_eq!(
            native_arm_for(&ours_but_off),
            NativeArm::None,
            "provenance without GitHub reporting it on is nothing to revoke"
        );

        let mut foreign = plain.clone();
        foreign.pr.as_mut().unwrap().auto_merge_enabled = true;
        assert_eq!(
            native_arm_for(&foreign),
            NativeArm::None,
            "an auto-merge set on github.com is not ours to revoke"
        );

        let mut ours = foreign.clone();
        ours.native_auto_merge_by_lazybox = true;
        assert_eq!(native_arm_for(&ours), NativeArm::OursAndLive);
    }

    /// The hole `apply_native_arm` structurally cannot close: its gates
    /// are all dynamic. Here `g g` armed the PR natively FIRST and `E M`
    /// (ORDER) was armed on its epic afterwards — `plan_merge_arming`
    /// skips an already-armed member, so nothing re-runs the arm-time
    /// gate. Left unrevoked, GitHub lands the member out of the epic's
    /// order, and lazybox's own latch has already stood down.
    #[tokio::test]
    async fn an_order_epic_armed_after_the_fact_revokes_the_native_arm() {
        let mut ws = armed_ws("o/r#1");
        ws.pr.as_mut().unwrap().auto_merge_enabled = true;
        ws.native_auto_merge_by_lazybox = true;
        let config = config_with(&ws);
        let target = native_target();

        assert_eq!(
            native_revoke_reason(
                &config,
                &ws.key,
                &target,
                lazybox_config::GithubNativeConfig::Auto
            )
            .await,
            None,
            "at arm time the epic constrained nothing"
        );

        let mut record =
            lazybox_core::EpicRecord::new(lazybox_core::EpicKey::new("e"), "Epic", Utc::now());
        record.members = vec![ws.key.clone()];
        record.policies.set(
            lazybox_core::EpicLatch::MergeInOrder,
            lazybox_core::PolicyArm::Arm,
        );
        crate::epics::persist(&config, &record).expect("persist epic");

        assert_eq!(
            native_revoke_reason(
                &config,
                &ws.key,
                &target,
                lazybox_config::GithubNativeConfig::Auto
            )
            .await,
            Some("its epic lands members in merge order".to_string()),
            "arming ORDER after the fact must revoke, not be ignored"
        );
    }

    /// A Reviewer's blocking verdict can only ever land AFTER the arm —
    /// `E R` dispatches the Reviewer when the PR turns green, the same
    /// moment native auto-merge fires. So the arm-time check for it is
    /// vacuous and the revoke sweep is the only thing that catches it.
    #[tokio::test]
    async fn a_blocking_review_verdict_after_the_arm_revokes() {
        let mut ws = armed_ws("o/r#1");
        ws.pr.as_mut().unwrap().auto_merge_enabled = true;
        ws.native_auto_merge_by_lazybox = true;
        let config = config_with(&ws);
        let target = native_target();

        assert_eq!(
            native_revoke_reason(
                &config,
                &ws.key,
                &target,
                lazybox_config::GithubNativeConfig::Auto
            )
            .await,
            None,
            "at arm time there is no verdict yet — which is the whole problem"
        );

        crate::epics::record_review_block_for_test(&config, &ws.key);
        assert_eq!(
            native_revoke_reason(
                &config,
                &ws.key,
                &target,
                lazybox_config::GithubNativeConfig::Auto
            )
            .await,
            Some("the review stage reported blocking findings".to_string())
        );
    }

    /// Flipping `merge_on_green.github_native` to `never` has to reach the
    /// PRs already handed to GitHub, not just future arms.
    #[tokio::test]
    async fn config_flipped_to_never_revokes_an_existing_native_arm() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        assert_eq!(
            native_revoke_reason(
                &config,
                &ws.key,
                &native_target(),
                lazybox_config::GithubNativeConfig::Never
            )
            .await,
            Some("GitHub-native auto-merge was turned off in config".to_string())
        );
    }

    /// GitHub dropping the auto-merge itself (a draft conversion, a
    /// conflict, a manual disable on github.com) makes the provenance a
    /// lie — and a stale `true` would later let a disarm clear an
    /// auto-merge lazybox does not own.
    #[tokio::test]
    async fn a_native_arm_github_already_dropped_clears_its_provenance() {
        let mut ws = armed_ws("o/r#1");
        ws.native_auto_merge_by_lazybox = true;
        ws.pr.as_mut().unwrap().auto_merge_enabled = false;
        let config = config_with(&ws);

        revoke_native_if_unsafe(&config, &ws.key).await;

        assert!(
            !load_workspace(&config, &ws.key)
                .expect("workspace")
                .native_auto_merge_by_lazybox,
            "stop claiming an auto-merge that is no longer on"
        );
    }

    // ── the arm decision table, through the backend seam ──────────

    #[derive(Default)]
    struct FakeNativeBackend {
        gate: lazybox_gh::BranchMergeGate,
        gate_from_cache: bool,
        enable_result: Option<NativeArmError>,
        disable_result: Option<NativeArmError>,
        paused_until: Option<chrono::DateTime<Utc>>,
        calls: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl FakeNativeBackend {
        /// A base that covers lazybox's gate for a PR running `build`:
        /// `build` is required and a review is required.
        fn gated() -> Self {
            Self {
                gate: lazybox_gh::BranchMergeGate {
                    required_contexts: vec!["build".into()],
                    required_reviews: 1,
                },
                ..Default::default()
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().clone()
        }
    }

    impl NativeAutoMergeBackend for FakeNativeBackend {
        async fn branch_merge_gate(
            &self,
            _owner: &str,
            _repo: &str,
            branch: &str,
        ) -> (lazybox_gh::BranchMergeGate, bool) {
            self.calls.lock().push(format!("gate:{branch}"));
            (self.gate.clone(), self.gate_from_cache)
        }
        async fn enable_auto_merge(
            &self,
            _repo: &str,
            node_id: &str,
        ) -> Result<(), NativeArmError> {
            self.calls.lock().push(format!("enable:{node_id}"));
            match &self.enable_result {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
        async fn disable_auto_merge(&self, node_id: &str) -> Result<(), NativeArmError> {
            self.calls.lock().push(format!("disable:{node_id}"));
            match &self.disable_result {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
        fn paused_until(&self) -> Option<chrono::DateTime<Utc>> {
            self.paused_until
        }
    }

    fn native_target() -> NativeTarget {
        native_target_running(&["build"])
    }

    /// A target whose PR runs `checks` — the input the `auto` gate's
    /// coverage test reads (#1596).
    fn native_target_running(checks: &[&str]) -> NativeTarget {
        let mut pr = green_task("o/r#1");
        pr.checks = checks
            .iter()
            .map(|name| lazybox_core::CheckRun {
                name: (*name).to_string(),
                status: lazybox_core::CiStatus::Success,
                url: None,
            })
            .collect();
        NativeTarget {
            owner: "o".into(),
            repo: "r".into(),
            node_id: "PR_node".into(),
            base_branch: Some("main".into()),
            label: "o/r#1".into(),
            pr,
        }
    }

    fn notices(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::AutoMergeNotice { message, .. } = event {
                out.push(message);
            }
        }
        out
    }

    /// The happy path: a gated base arms natively and records provenance,
    /// which is what makes a later disarm able to clean up.
    #[tokio::test]
    async fn gated_base_arms_natively_and_records_provenance() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeNativeBackend::gated();
        let mut rx = config.bus.subscribe();

        run_native_arm(
            &config,
            &ws.key,
            &native_target(),
            true,
            lazybox_config::GithubNativeConfig::Auto,
            &backend,
        )
        .await;

        assert_eq!(backend.calls(), vec!["gate:main", "enable:PR_node"]);
        assert!(
            load_workspace(&config, &ws.key)
                .expect("workspace")
                .native_auto_merge_by_lazybox
        );
        assert!(
            notices(&mut rx).iter().any(|m| m.contains("armed too")),
            "the durable arm is announced"
        );
    }

    /// An ungated base declines, says so once, and leaves provenance
    /// clear — the acceptance case from the issue.
    #[tokio::test]
    async fn ungated_base_declines_and_announces_once_per_branch() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let mut backend = FakeNativeBackend::default();
        let mut rx = config.bus.subscribe();

        run_native_arm(
            &config,
            &ws.key,
            &native_target(),
            true,
            lazybox_config::GithubNativeConfig::Auto,
            &backend,
        )
        .await;
        assert_eq!(backend.calls(), vec!["gate:main"], "no mutation is sent");
        assert!(
            !load_workspace(&config, &ws.key)
                .expect("workspace")
                .native_auto_merge_by_lazybox
        );
        let first = notices(&mut rx);
        assert!(
            first
                .iter()
                .any(|m| m.contains("has no required status checks")),
            "the decline explains itself: {first:?}"
        );

        // A second PR onto the same base: the gate answer is cached, so
        // the identical paragraph is not repeated. This is what keeps a
        // bulk arm from emitting one notice per selected row.
        backend.gate_from_cache = true;
        run_native_arm(
            &config,
            &ws.key,
            &native_target(),
            true,
            lazybox_config::GithubNativeConfig::Auto,
            &backend,
        )
        .await;
        assert!(
            notices(&mut rx).is_empty(),
            "a cached gate answer must not re-announce"
        );
    }

    /// `always` skips the gate entirely — the documented opt-in for
    /// landing without required checks.
    #[tokio::test]
    async fn always_mode_skips_the_required_checks_gate() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeNativeBackend::default();

        run_native_arm(
            &config,
            &ws.key,
            &native_target(),
            true,
            lazybox_config::GithubNativeConfig::Always,
            &backend,
        )
        .await;

        assert_eq!(
            backend.calls(),
            vec!["enable:PR_node"],
            "no gate probe, straight to the mutation"
        );
    }

    /// #1596 regression: a paused token can only refuse these calls, and
    /// a 403 against the pause lengthens it — the same gate `run_attempt`
    /// applies before its probe.
    #[tokio::test]
    async fn rate_limit_pause_defers_the_native_arm() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeNativeBackend {
            gate: lazybox_gh::BranchMergeGate {
                required_contexts: vec!["build".into()],
                required_reviews: 1,
            },
            paused_until: Some(Utc::now() + chrono::Duration::minutes(5)),
            ..Default::default()
        };

        run_native_arm(
            &config,
            &ws.key,
            &native_target(),
            true,
            lazybox_config::GithubNativeConfig::Auto,
            &backend,
        )
        .await;

        assert!(
            backend.calls().is_empty(),
            "nothing is spent against a paused token"
        );
    }

    /// A throttle is "not now", never GitHub judging the arm. Reporting
    /// it as a refusal sent people hunting for a branch-protection
    /// problem that wasn't there.
    #[tokio::test]
    async fn throttled_arm_is_not_reported_as_a_refusal() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeNativeBackend {
            gate: lazybox_gh::BranchMergeGate {
                required_contexts: vec!["build".into()],
                required_reviews: 1,
            },
            enable_result: Some(NativeArmError::Throttled("secondary rate limit".into())),
            ..Default::default()
        };
        let mut rx = config.bus.subscribe();

        run_native_arm(
            &config,
            &ws.key,
            &native_target(),
            true,
            lazybox_config::GithubNativeConfig::Auto,
            &backend,
        )
        .await;

        let seen = notices(&mut rx);
        assert!(
            seen.iter().any(|m| m.contains("rate-limited")),
            "a throttle names itself: {seen:?}"
        );
        assert!(
            !seen.iter().any(|m| m.contains("was refused")),
            "and is not dressed up as a GitHub refusal: {seen:?}"
        );
        assert!(
            !load_workspace(&config, &ws.key)
                .expect("workspace")
                .native_auto_merge_by_lazybox,
            "nothing was armed, so nothing is claimed"
        );
    }

    /// A failed disable keeps the provenance so a later disarm retries —
    /// clearing it would strand GitHub auto-merge ON with no record.
    #[tokio::test]
    async fn failed_disable_keeps_provenance_for_a_retry() {
        let mut ws = armed_ws("o/r#1");
        ws.auto_merge_on_green = false;
        ws.native_auto_merge_by_lazybox = true;
        let config = config_with(&ws);
        let backend = FakeNativeBackend {
            disable_result: Some(NativeArmError::Refused("boom".into())),
            ..Default::default()
        };

        run_native_arm(
            &config,
            &ws.key,
            &native_target(),
            false,
            lazybox_config::GithubNativeConfig::Auto,
            &backend,
        )
        .await;

        assert!(
            load_workspace(&config, &ws.key)
                .expect("workspace")
                .native_auto_merge_by_lazybox,
            "still ours, still on — a later disarm must be able to retry"
        );
    }

    /// #1596 regression, the disarm/arm race: a disarm committed while an
    /// arm was in flight must win. `apply_native_arm` re-reads the
    /// workspace *inside* the native lock, so the arm observes the
    /// disarm's committed `false` and stands down instead of handing
    /// GitHub a PR the user just cancelled.
    #[tokio::test]
    async fn arm_stands_down_when_a_disarm_committed_first() {
        let mut ws = armed_ws("o/r#1");
        // The state a disarm leaves behind: local arm off, nothing of
        // ours enabled upstream yet.
        ws.auto_merge_on_green = false;
        let config = config_with(&ws);

        // The in-flight arm reaches the native step only now.
        apply_native_arm(&config, &ws.key, true).await;

        assert!(
            !load_workspace(&config, &ws.key)
                .expect("workspace")
                .native_auto_merge_by_lazybox,
            "a superseded arm must not enable GitHub auto-merge"
        );
    }

    /// The native lock is what serializes the pair. Two opposite intents
    /// dispatched concurrently must not interleave: whichever runs second
    /// sees the first's committed result rather than its own stale
    /// snapshot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_lock_serializes_arm_and_disarm() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let first = config.lock_native_auto_merge(ws.key.as_str()).await;

        let config2 = config.clone();
        let key = ws.key.clone();
        let waiter = tokio::spawn(async move {
            let _second = config2.lock_native_auto_merge(key.as_str()).await;
            true
        });

        // The second acquisition must be blocked while the first is held.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), async {})
                .await
                .is_ok()
        );
        assert!(
            !waiter.is_finished(),
            "the native lock is exclusive per key"
        );
        drop(first);
        assert!(waiter.await.expect("waiter joins"));
    }

    #[tokio::test]
    async fn native_provenance_round_trips() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);

        set_native_provenance(&config, &ws.key, true).await;
        assert!(
            load_workspace(&config, &ws.key)
                .expect("workspace")
                .native_auto_merge_by_lazybox
        );

        set_native_provenance(&config, &ws.key, false).await;
        assert!(
            !load_workspace(&config, &ws.key)
                .expect("workspace")
                .native_auto_merge_by_lazybox
        );
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

    /// A green non-own PR now Fires (author-agnostic) rather than
    /// holding silently — so it reaches the attempt's author gate, whose
    /// stand-down surfaces the reason (issue #845). Before, `signal_for`
    /// tripped the own-PR gate and returned `Hold`, and nothing was ever
    /// logged or shown.
    #[test]
    fn signal_fires_non_own_green_pr_for_the_attempt_to_decide() {
        let mut ws = armed_ws("o/r#1");
        ws.pr.as_mut().unwrap().role = TaskRole::Reviewer;
        assert_eq!(signal_for(&ws), Signal::Fire);
        // A red non-own PR still holds — only fully-mergeable-but-for-the
        // -author PRs dispatch an attempt.
        ws.pr.as_mut().unwrap().ci = CiStatus::Failure;
        assert_eq!(signal_for(&ws), Signal::Hold);
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
            probe.skip_if_head, None,
            "a transient block re-probes in full — a green same head merges"
        );
        assert_eq!(
            probe.restore,
            Some(Latch::Blocked(Some("abc".into()))),
            "a probe that can't fetch restores the prior verdict"
        );
    }

    /// A GitHub rejection re-probes on a change but carries the rejected
    /// head so the attempt stands down while it is unchanged.
    #[test]
    fn rejected_reprobe_carries_the_rejected_head() {
        let mut m = AutoMergeMemory::default();
        let key = WorkspaceKey::new("github-o-r-1");
        m.settle(&key, Some(Latch::Rejected(Some("abc".into()))));
        assert!(plan(&mut m, &key, Signal::Fire, false).is_none());
        let probe = plan(&mut m, &key, Signal::Fire, true).expect("changed commit re-probes");
        assert_eq!(probe.skip_if_head.as_deref(), Some("abc"));
        assert_eq!(probe.restore, Some(Latch::Rejected(Some("abc".into()))));
    }

    #[test]
    fn transient_rejections_are_classified() {
        for msg in [
            "GraphQL error: 2 of 2 required status checks are expected.",
            "Required status check \"test\" is in progress.",
            "Base branch was modified. Review and try the merge again.",
            "Merge already in progress",
            "At least 1 approving review is required",
        ] {
            assert!(transient_merge_rejection(msg), "{msg}");
        }
        for msg in [
            "Pull Request has merge conflicts",
            "Repository rule violations found",
            "not authorized to merge",
            // The over-broad "in progress" trap: a hard failure that merely
            // mentions the phrase must NOT be treated as transient (else it
            // re-fires a doomed mutation + red notice every changed poll).
            "Merge blocked: a required deployment is in progress and failed",
        ] {
            assert!(!transient_merge_rejection(msg), "{msg}");
        }
    }

    /// The classifier runs on `ProviderError::diagnostic()` at the call
    /// site, not `user_message()` — which for a `Permanent` error is the
    /// FIRST LINE only. A reason on a later line must still classify: this
    /// is the fidelity the direct-string test above cannot exercise.
    #[test]
    fn classifier_sees_a_reason_past_the_first_line_via_diagnostic() {
        // A multi-line detail whose transient reason is NOT on line one.
        let err = lazybox_core::ProviderError::permanent(
            "github",
            "GraphQL error: merge could not be completed\nBase branch was modified. Review and try the merge again.",
        );
        assert_eq!(
            err.user_message(),
            "github: GraphQL error: merge could not be completed",
            "user_message truncates to the first line — the reason is lost"
        );
        assert!(
            !transient_merge_rejection(&err.user_message()),
            "the first line alone misclassifies the transient reason as hard"
        );
        assert!(
            transient_merge_rejection(&err.diagnostic()),
            "the full diagnostic preserves the transient reason"
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
        trailers: parking_lot::Mutex<Vec<Option<lazybox_core::PrTrailers>>>,
        paused_until: Option<chrono::DateTime<Utc>>,
        fetches: parking_lot::Mutex<u32>,
    }

    impl FakeBackend {
        fn merging(fresh: Task, head: &str) -> Self {
            Self {
                fetch: Ok(Some((fresh, Some(head.into())))),
                merge_result: Ok(()),
                merges: parking_lot::Mutex::new(Vec::new()),
                trailers: parking_lot::Mutex::new(Vec::new()),
                paused_until: None,
                fetches: parking_lot::Mutex::new(0),
            }
        }

        /// A backend whose merge fails with a scripted error — used to
        /// exercise the retryable (rate-limit) vs permanent branches.
        fn failing(fresh: Task, head: &str, err: lazybox_core::ProviderError) -> Self {
            Self {
                fetch: Ok(Some((fresh, Some(head.into())))),
                merge_result: Err(err),
                merges: parking_lot::Mutex::new(Vec::new()),
                trailers: parking_lot::Mutex::new(Vec::new()),
                paused_until: None,
                fetches: parking_lot::Mutex::new(0),
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
            *self.fetches.lock() += 1;
            self.fetch.clone()
        }

        fn paused_until(&self) -> Option<chrono::DateTime<Utc>> {
            self.paused_until
        }

        async fn merge(
            &self,
            _ws: &Workspace,
            options: &lazybox_core::MergeOptions<'_>,
        ) -> Result<(), lazybox_core::ProviderError> {
            self.merges
                .lock()
                .push(options.expected_head_oid.map(|s| s.to_string()));
            self.trailers.lock().push(options.trailers.clone());
            self.merge_result.clone()
        }
    }

    /// A ticket as `plan` issues it for a REJECTED head (`skip_if_head`
    /// set, the rejection restored on a fetch failure) or a first attempt.
    fn ticket(ws: &Workspace, skip_if_head: Option<&str>) -> AttemptPlan {
        AttemptPlan {
            workspace_key: ws.key.clone(),
            skip_if_head: skip_if_head.map(|s| s.to_string()),
            restore: skip_if_head.map(|s| Latch::Rejected(Some(s.to_string()))),
        }
    }

    /// A ticket as `plan` issues it for a transiently BLOCKED head: a
    /// full re-probe with the block restored on a fetch failure.
    fn blocked_ticket(ws: &Workspace, head: &str) -> AttemptPlan {
        AttemptPlan {
            workspace_key: ws.key.clone(),
            skip_if_head: None,
            restore: Some(Latch::Blocked(Some(head.to_string()))),
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

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;

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

    /// Merge-on-green carries the cost record too. This is the flow with no
    /// human in it at all: without the trailer, an auto-merged PR's spend
    /// exists nowhere anyone will look. The marker is stamped on success, so
    /// the same workspace's next PR bills only what it spends itself.
    #[tokio::test(flavor = "current_thread")]
    async fn auto_merge_writes_the_cost_trailer_and_closes_the_slice() {
        let ws = armed_ws("o/r#1");
        let store = Arc::new(MemoryStore::new());
        seed(&store, &ws);
        store
            .set_kv(&format!("meter-cost:{}", ws.key.as_str()), "13893891")
            .expect("seed the metered cost");
        let config = ServerConfig::with_store(store.clone());
        let backend = FakeBackend::merging(green_task("o/r#1"), "abc123");

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;

        let sent = backend.trailers.lock().clone();
        assert_eq!(
            sent.into_iter().next().flatten().map(|t| t.render()),
            Some("Lazybox-Cost: $13.89".to_string()),
            "the auto-merge must carry what the work cost",
        );
        assert_eq!(
            crate::client_kv::unreported_session_cost(&*store, ws.key.as_str()),
            0,
            "the merged slice is marked reported, so the next PR starts at zero",
        );
    }

    /// Issue #969: an armed, green PR that is stacked on a still-open
    /// parent must NOT auto-merge — merging it out of order would force a
    /// restack of the children. It stands down (Blocked) with a reason,
    /// mirroring the `g m` warning the human would have seen.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_holds_a_stacked_child_with_an_open_parent() {
        // Parent PR (open) on `main`; its head is `feat-parent`.
        let mut parent = green_task("o/r#1");
        parent.branch = Some("feat-parent".into());
        let parent_ws = Workspace::from_task(parent, Utc::now());

        // Child PR (armed, green) stacked on the parent's head.
        let mut child = green_task("o/r#2");
        child.branch = Some("feat-child".into());
        child.base_branch = Some("feat-parent".into());
        let mut child_ws = Workspace::from_task(child.clone(), Utc::now());
        child_ws.auto_merge_on_green = true;

        let store = Arc::new(MemoryStore::new());
        seed(&store, &parent_ws);
        seed(&store, &child_ws);
        let config = ServerConfig::with_store(store);
        let mut rx = config.bus.subscribe();

        let mut fresh = green_task("o/r#2");
        fresh.branch = Some("feat-child".into());
        fresh.base_branch = Some("feat-parent".into());
        let backend = FakeBackend::merging(fresh, "abc123");

        run_attempt(&config, ticket(&child_ws, None), &own_policy(), &backend).await;

        assert!(
            backend.merges.lock().is_empty(),
            "a stacked child must not merge ahead of its open parent",
        );
        assert_eq!(
            config.poll.auto_merge.lock().latch(&child_ws.key),
            Some(&Latch::Blocked(Some("abc123".into()))),
            "held (Blocked) so it re-probes once the parent lands",
        );
        let evt = rx.try_recv().expect("a stand-down notice is broadcast");
        match evt {
            Event::ProviderError { message, .. } => {
                assert!(
                    message.contains("stacked"),
                    "reason names the cause: {message}"
                )
            }
            other => panic!("expected a stand-down notice, got {other:?}"),
        }
    }

    /// The bottom of a stack (base is `main`, no open parent) still
    /// auto-merges normally — the guard only holds non-bottom PRs.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_merges_the_bottom_of_a_stack() {
        // A child stacked on THIS pr sits above it, but this PR's own base
        // is `main`, so it is the mergeable bottom.
        let mut child = green_task("o/r#2");
        child.branch = Some("feat-child".into());
        child.base_branch = Some("feature".into()); // feature == this PR's head
        let child_ws = Workspace::from_task(child, Utc::now());

        let bottom_ws = armed_ws("o/r#1"); // head "feature", base "main"
        let store = Arc::new(MemoryStore::new());
        seed(&store, &bottom_ws);
        seed(&store, &child_ws);
        let config = ServerConfig::with_store(store);

        let backend = FakeBackend::merging(green_task("o/r#1"), "abc123");
        run_attempt(&config, ticket(&bottom_ws, None), &own_policy(), &backend).await;

        assert_eq!(
            backend.merges.lock().as_slice(),
            &[Some("abc123".to_string())],
            "the bottom of the stack merges normally",
        );
    }

    /// Build an armed workspace whose PR is authored by someone else
    /// (a bot) — the Dependabot shape from issue #845.
    fn armed_bot_ws(key: &str, author: &str) -> Workspace {
        let mut task = green_task(key);
        task.role = TaskRole::Mentioned;
        task.author = author.into();
        let mut ws = Workspace::from_task(task, Utc::now());
        ws.auto_merge_on_green = true;
        ws
    }

    /// Issue #845 acceptance — the feature half: with the author opted
    /// in via the policy, a green non-own (bot) PR auto-merges.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_merges_a_green_bot_pr_when_opted_in() {
        let ws = armed_bot_ws("o/r#1", "dependabot[bot]");
        let config = config_with(&ws);
        let mut rx = config.bus.subscribe();
        let mut fresh = green_task("o/r#1");
        fresh.role = TaskRole::Mentioned;
        fresh.author = "dependabot[bot]".into();
        let backend = FakeBackend::merging(fresh, "abc123");
        let policy = lazybox_core::MergeOnGreenPolicy::from_allow_authors(["dependabot"]);

        run_attempt(&config, ticket(&ws, None), &policy, &backend).await;

        assert_eq!(
            backend.merges.lock().as_slice(),
            &[Some("abc123".to_string())],
            "an opted-in green bot PR merges"
        );
        assert!(matches!(rx.try_recv(), Ok(Event::PrMerged { .. })));
    }

    /// Issue #845 acceptance — the feedback half: an armed non-own PR
    /// that is NOT opted in must not merge, and must surface the reason
    /// (broadcast notice) rather than standing down silently.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_declines_non_own_pr_with_a_reason_when_not_opted_in() {
        let ws = armed_bot_ws("o/r#1", "dependabot[bot]");
        let config = config_with(&ws);
        let mut rx = config.bus.subscribe();
        let mut fresh = green_task("o/r#1");
        fresh.role = TaskRole::Mentioned;
        fresh.author = "dependabot[bot]".into();
        let backend = FakeBackend::merging(fresh, "abc123");

        // Default policy: own PRs only.
        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;

        assert!(backend.merges.lock().is_empty(), "must not merge");
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Blocked(Some("abc123".into())))
        );
        let evt = rx.try_recv().expect("a stand-down notice");
        match evt {
            Event::ProviderError {
                source, message, ..
            } => {
                assert_eq!(source, "auto-merge");
                assert!(message.contains("your own PRs"), "{message}");
            }
            other => panic!("expected a ProviderError notice, got {other:?}"),
        }
    }

    /// #845 wanted a non-own PR's decline audible ONCE. A green-but-held
    /// PR re-fires `Signal::Fire` every changed poll (`signal_for` is
    /// author-agnostic and the row is green), so a widened `Blocked`
    /// re-probe on the SAME head must stand down SILENTLY — no re-flashed
    /// footer notice on every comment / CI-check flap.
    #[tokio::test(flavor = "current_thread")]
    async fn a_reprobe_on_the_same_blocked_head_stands_down_silently() {
        let ws = armed_bot_ws("o/r#1", "dependabot[bot]");
        let config = config_with(&ws);
        let mut rx = config.bus.subscribe();
        let mut fresh = green_task("o/r#1");
        fresh.role = TaskRole::Mentioned;
        fresh.author = "dependabot[bot]".into();
        let backend = FakeBackend::merging(fresh, "abc123");

        // The re-probe ticket `plan` issues for a `Blocked` head already
        // stood down on "abc123" (`restore == Blocked(Some("abc123"))`).
        run_attempt(
            &config,
            blocked_ticket(&ws, "abc123"),
            &own_policy(),
            &backend,
        )
        .await;

        assert!(backend.merges.lock().is_empty(), "must not merge");
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Blocked(Some("abc123".into()))),
            "still held on the same head"
        );
        // No auto-merge stand-down notice on the re-probe — the first one
        // already fired. (commit_fresh_task's own upserts may ride the bus;
        // only an auto-merge ProviderError is forbidden here.)
        while let Ok(evt) = rx.try_recv() {
            if let Event::ProviderError { source, .. } = &evt {
                assert_ne!(
                    source.as_str(),
                    "auto-merge",
                    "a same-head re-probe must not re-flash the decline"
                );
            }
        }
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
            trailers: parking_lot::Mutex::new(Vec::new()),
            paused_until: None,
            fetches: parking_lot::Mutex::new(0),
        };

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;

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

        run_attempt(
            &config,
            ticket(&ws, Some("abc123")),
            &own_policy(),
            &backend,
        )
        .await;

        assert!(
            backend.merges.lock().is_empty(),
            "a head GitHub rejected must not be re-sent unchanged"
        );
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Rejected(Some("abc123".into())))
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

    /// The dead end this fixes: a head transiently blocked (required
    /// checks "expected", a pending review) that goes green on the SAME
    /// commit must merge on the re-probe — before, it stood down forever
    /// until someone pushed a new commit.
    #[tokio::test(flavor = "current_thread")]
    async fn transient_block_merges_on_the_same_head_once_green() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeBackend::merging(green_task("o/r#1"), "abc123");

        run_attempt(
            &config,
            blocked_ticket(&ws, "abc123"),
            &own_policy(),
            &backend,
        )
        .await;

        assert_eq!(
            backend.merges.lock().as_slice(),
            &[Some("abc123".to_string())],
            "a green same head after a transient block merges"
        );
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Done(Some("abc123".into())))
        );
    }

    /// While GitHub has the token paused, an attempt neither fetches nor
    /// merges and keeps its prior verdict — no probe storm through the
    /// pause (23 doomed probes in one afternoon's log).
    #[tokio::test(flavor = "current_thread")]
    async fn paused_backend_defers_without_fetching() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let mut backend = FakeBackend::merging(green_task("o/r#1"), "abc123");
        backend.paused_until = Some(Utc::now() + chrono::Duration::minutes(5));

        run_attempt(
            &config,
            blocked_ticket(&ws, "abc123"),
            &own_policy(),
            &backend,
        )
        .await;

        assert_eq!(*backend.fetches.lock(), 0, "no probe during the pause");
        assert!(backend.merges.lock().is_empty());
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Blocked(Some("abc123".into()))),
            "the prior verdict is restored"
        );

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            None,
            "a first attempt releases so the next commit after the pause re-probes"
        );
    }

    /// A hard rejection (conflicts) latches `Rejected`: the head stands
    /// down until a new commit, unlike a transient "not yet".
    #[tokio::test(flavor = "current_thread")]
    async fn hard_rejection_latches_rejected() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeBackend::failing(
            green_task("o/r#1"),
            "abc123",
            lazybox_core::ProviderError::permanent(
                "github",
                "GraphQL error: Pull Request has merge conflicts",
            ),
        );

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;

        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Rejected(Some("abc123".into())))
        );
    }

    /// A NEW head re-arms: the re-probe sees a different OID and the
    /// merge proceeds, pinned to the new head.
    #[tokio::test(flavor = "current_thread")]
    async fn attempt_rearms_on_a_new_head() {
        let ws = armed_ws("o/r#1");
        let config = config_with(&ws);
        let backend = FakeBackend::merging(green_task("o/r#1"), "def456");

        run_attempt(
            &config,
            ticket(&ws, Some("abc123")),
            &own_policy(),
            &backend,
        )
        .await;

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

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;

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

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;

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

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;

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
            trailers: parking_lot::Mutex::new(Vec::new()),
            paused_until: None,
            fetches: parking_lot::Mutex::new(0),
        };

        run_attempt(&config, ticket(&ws, None), &own_policy(), &backend).await;
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            None,
            "a first attempt that never fetched releases for a clean retry"
        );

        run_attempt(
            &config,
            ticket(&ws, Some("abc123")),
            &own_policy(),
            &backend,
        )
        .await;
        assert_eq!(
            config.poll.auto_merge.lock().latch(&ws.key),
            Some(&Latch::Rejected(Some("abc123".into()))),
            "a re-probe that never fetched keeps its prior verdict"
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
