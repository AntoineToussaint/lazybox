//! IPC mutation handlers — the daemon's "user pressed a key, mutate
//! upstream provider state" surface.
//!
//! These functions are the read-modify-write side of the polling
//! module: they look up the workspace, dispatch to the right provider
//! (via [`ProviderHandle`]), and emit events back to the bus. The
//! polling loop lives in the parent module ([`crate::polling`]), while
//! workspace-store machinery lives in `polling::upsert`.

use super::{
    EngagementSignals, EngagementTier, TickState, apply_and_commit, commit_upsert_reported,
    load_workspace,
};
use crate::ServerConfig;
use lazybox_core::{CiStatus, ReviewStatus, Task, Workspace, WorkspaceKey};
use lazybox_gh::GhClient;
use lazybox_ipc::Event;
use lazybox_linear::LinearClient;

/// Post a top-level reply to the workspace's primary task. Today this
/// targets only GitHub PRs/issues; Linear and other providers can grow
/// into the same shape. On success we don't update the local activity
/// feed inline — the next poll picks up the new comment, which keeps
/// the "what the upstream provider says" invariant intact.
pub async fn post_reply(
    config: &ServerConfig,
    session_key: lazybox_core::SessionKey,
    body: String,
) -> Result<(), String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err("reply body is empty".to_string());
    }
    let workspace_key = WorkspaceKey::new(session_key.as_str().to_string());
    let workspace = match config
        .store
        .get_workspace(&workspace_key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
    {
        Some(json) => match serde_json::from_str::<Workspace>(&json) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("post_reply: bad JSON for {workspace_key}: {e}");
                return Err(emit_reply_error(
                    config,
                    &format!("workspace decode failed: {e}"),
                ));
            }
        },
        None => {
            return Err(emit_reply_error(config, "workspace not found"));
        }
    };
    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            return Err(emit_reply_error(config, &e));
        }
    };
    if let Err(e) = run_mutation_with_retry(&config.bus, "reply", || {
        provider.post_reply(&workspace, trimmed)
    })
    .await
    {
        tracing::warn!("post_reply {workspace_key}: {e:?}");
        return Err(emit_reply_error(
            config,
            &format!("post failed: {}", humanize_mutation_failure("reply", &e)),
        ));
    }
    tracing::info!(
        "posted reply to {} ({} chars)",
        workspace_key,
        trimmed.len()
    );
    if let Some(client) = config.poll.cached_gh_client() {
        client.force_full_sweep();
    }
    config.poll.wake(true);
    Ok(())
}

fn emit_reply_error(config: &ServerConfig, msg: &str) -> String {
    let _ = config
        .bus
        .send(Event::provider_error_retryable("reply", msg));
    msg.to_string()
}

/// Runtime-polymorphic wrapper around the workspace's
/// `TaskProvider`. Using an enum (vs `Arc<dyn TaskProvider>`) is
/// deliberate: the trait uses `async fn` which isn't dyn-compatible
/// without the `async_trait` crate. The enum dispatches manually
/// in O(n_providers) — fine for the 2-3 providers lazybox will
/// ever have at once.
///
/// Adding a new provider: add a variant + the four `match` arms
/// below. Each arm delegates to the provider's `TaskProvider`
/// impl, so the GitHub/Linear/etc. backend logic stays where it
/// belongs.
pub enum ProviderHandle {
    Github(GhClient),
    Linear(LinearClient),
}

impl ProviderHandle {
    pub async fn merge(
        &self,
        ws: &lazybox_core::Workspace,
        expected_head_oid: Option<&str>,
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::merge(c, ws, expected_head_oid).await,
            Self::Linear(c) => lazybox_core::TaskProvider::merge(c, ws, expected_head_oid).await,
        }
    }
    pub async fn update_branch(
        &self,
        ws: &lazybox_core::Workspace,
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::update_branch(c, ws).await,
            Self::Linear(c) => lazybox_core::TaskProvider::update_branch(c, ws).await,
        }
    }
    pub async fn close_issue(
        &self,
        ws: &lazybox_core::Workspace,
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::close_issue(c, ws).await,
            Self::Linear(c) => lazybox_core::TaskProvider::close_issue(c, ws).await,
        }
    }
    pub async fn close_pr(
        &self,
        ws: &lazybox_core::Workspace,
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::close_pr(c, ws).await,
            Self::Linear(c) => lazybox_core::TaskProvider::close_pr(c, ws).await,
        }
    }
    pub async fn delete_issue(
        &self,
        ws: &lazybox_core::Workspace,
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::delete_issue(c, ws).await,
            Self::Linear(c) => lazybox_core::TaskProvider::delete_issue(c, ws).await,
        }
    }
    pub async fn request_reviewers(
        &self,
        ws: &lazybox_core::Workspace,
        logins: &[String],
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::request_reviewers(c, ws, logins).await,
            Self::Linear(c) => lazybox_core::TaskProvider::request_reviewers(c, ws, logins).await,
        }
    }
    pub async fn add_assignees(
        &self,
        ws: &lazybox_core::Workspace,
        logins: &[String],
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::add_assignees(c, ws, logins).await,
            Self::Linear(c) => lazybox_core::TaskProvider::add_assignees(c, ws, logins).await,
        }
    }
    pub async fn set_assignees(
        &self,
        ws: &lazybox_core::Workspace,
        logins: &[String],
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::set_assignees(c, ws, logins).await,
            Self::Linear(c) => lazybox_core::TaskProvider::set_assignees(c, ws, logins).await,
        }
    }
    pub async fn list_repo_labels(
        &self,
        ws: &lazybox_core::Workspace,
    ) -> Result<Vec<lazybox_core::Label>, lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::list_repo_labels(c, ws).await,
            Self::Linear(c) => lazybox_core::TaskProvider::list_repo_labels(c, ws).await,
        }
    }
    pub async fn list_requestable_reviewers(
        &self,
        ws: &lazybox_core::Workspace,
    ) -> Result<Vec<String>, lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::list_requestable_reviewers(c, ws).await,
            Self::Linear(c) => lazybox_core::TaskProvider::list_requestable_reviewers(c, ws).await,
        }
    }
    pub async fn set_labels(
        &self,
        ws: &lazybox_core::Workspace,
        names: &[String],
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::set_labels(c, ws, names).await,
            Self::Linear(c) => lazybox_core::TaskProvider::set_labels(c, ws, names).await,
        }
    }
    pub async fn post_reply(
        &self,
        ws: &lazybox_core::Workspace,
        body: &str,
    ) -> Result<(), lazybox_core::ProviderError> {
        match self {
            Self::Github(c) => lazybox_core::TaskProvider::post_reply(c, ws, body).await,
            Self::Linear(c) => lazybox_core::TaskProvider::post_reply(c, ws, body).await,
        }
    }
}

/// Build a provider handle for the workspace that owns this
/// mutation. Routes on the workspace key's `<source>-<rest>`
/// prefix — `"github-acme-widget-186"` → github,
/// `"linear-team-xyz"` → linear, `"sandbox"` → no upstream
/// (returns `Err`).
///
/// Each branch builds its own credential chain — github goes
/// through `gh auth token` + `GH_TOKEN` envs, linear goes through
/// `LINEAR_API_KEY`. Future providers add their own chain in a
/// new branch.
///
/// Errors come back as `String` ready for the handler's
/// `emit_err` callback.
async fn build_provider_for_workspace(
    config: &ServerConfig,
    workspace_key: &WorkspaceKey,
) -> Result<ProviderHandle, String> {
    let source = workspace_key
        .as_str()
        .split_once('-')
        .map(|(p, _)| p)
        .unwrap_or("");
    match source {
        s if s == lazybox_gh::SOURCE => resolve_gh_client_result(config)
            .await
            .map(ProviderHandle::Github),
        s if s == lazybox_linear::SOURCE => {
            let cred = lazybox_linear::credential_chain()
                .resolve(lazybox_linear::SOURCE)
                .await
                .map_err(|e| format!("linear credentials: {e}"))?;
            Ok(ProviderHandle::Linear(LinearClient::from_credential(cred)))
        }
        other => Err(format!(
            "no provider registered for workspace prefix `{other}`",
        )),
    }
}

#[cfg(test)]
mod provider_resolution_tests {
    use super::*;
    use lazybox_store::MemoryStore;
    use std::sync::Arc;

    #[tokio::test]
    async fn github_workspace_operations_reuse_the_process_governor() {
        let config = ServerConfig::with_store(Arc::new(MemoryStore::new()));
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(30);
        config.poll.cache_gh_client(
            GhClient::stub_with_rate_limit_for_tests(
                "test-source",
                "test-fingerprint",
                3210,
                5000,
                reset_at,
            )
            .expect("stub client"),
        );

        let provider =
            build_provider_for_workspace(&config, &WorkspaceKey::new("github-owner-repo-1"))
                .await
                .expect("cached GitHub provider");
        let ProviderHandle::Github(client) = provider else {
            panic!("github workspace must resolve the GitHub provider");
        };
        let remote = client
            .rate_snapshot()
            .remote
            .expect("cached governor observation");
        assert_eq!(remote.remaining, 3210);
        assert_eq!(remote.reset_at, reset_at);
    }
}

/// A user-initiated mutation that trips GitHub's secondary rate limit
/// should not hard-fail — GitHub resets the limit within a window and the
/// write can simply be retried. These bounds keep that retry queue honest:
/// it gives up after `MUTATION_MAX_ATTEMPTS` tries or `MUTATION_MAX_WAIT_SECS`
/// of total waiting, whichever comes first, and only then surfaces a real
/// (humanized) failure.
const MUTATION_MAX_ATTEMPTS: u32 = 5;
const MUTATION_MAX_WAIT_SECS: u64 = 600;
/// GraphQL mutation rate-limit errors carry no `resetAt` (that only rides
/// the header path), so fall back to a modest wait the loop then backs off.
const MUTATION_RETRY_FALLBACK_SECS: u64 = 60;

/// Bounded retry queue for a user-initiated GitHub mutation (merge,
/// update-branch, …) that hits GitHub's secondary rate limit.
///
/// A rate-limited write comes back as a `Retryable` [`lazybox_core::ProviderError`]
/// carrying the reset hint (`crate`-side, `mutation_provider_error` in the
/// GitHub client lifts a secondary-limit GraphQL error to that). Rather
/// than hard-fail with a raw red `×` the user must clear and re-press, we
/// surface a live, non-scary "queued — retrying in ~2m" notice, wait out
/// the window (honoring the hint, with per-attempt backoff + jitter so
/// lazybox doesn't re-collide with `gh`/agents on the shared token), and
/// retry. Only once the bounds above are exhausted does the final error
/// reach the caller.
///
/// Every non-retryable failure (a genuine GitHub rejection: conflict,
/// blocked checks, permission) returns immediately — those are not rate
/// limits and must surface at once.
pub(super) async fn run_mutation_with_retry<F, Fut>(
    bus: &tokio::sync::broadcast::Sender<Event>,
    op: &str,
    mut attempt: F,
) -> Result<(), lazybox_core::ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), lazybox_core::ProviderError>>,
{
    let mut attempt_num = 0u32;
    let mut waited = 0u64;
    loop {
        attempt_num += 1;
        let err = match attempt().await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let base = err
            .retry_after_secs()
            .unwrap_or(MUTATION_RETRY_FALLBACK_SECS);
        let wait = mutation_retry_wait(base, attempt_num);
        // Give up — surface the error — when: the failure isn't retryable
        // (a genuine rejection); we've spent our attempt budget; the whole
        // queue window is exhausted; or the reset window itself is longer
        // than we'll ever hold the queue. That last case is the honest exit
        // for a *primary* quota exhaustion (resets an hour out, not a
        // minute): queuing 10 min then failing is worse than telling the
        // user now to come back when it resets — so we don't pretend.
        if !err.is_retryable()
            || attempt_num >= MUTATION_MAX_ATTEMPTS
            || base > MUTATION_MAX_WAIT_SECS
            || waited.saturating_add(wait) > MUTATION_MAX_WAIT_SECS
        {
            return Err(err);
        }
        let _ = bus.send(Event::provider_error_retryable(
            op,
            format!(
                "{op} queued — GitHub rate-limited, retrying in {}",
                humanize_wait(wait)
            ),
        ));
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        waited = waited.saturating_add(wait);
    }
}

/// Wait before the next mutation attempt: honor the reset hint as the
/// floor, back off across attempts, and add ≤20% jitter so lazybox doesn't
/// re-collide with `gh`/agents on the shared token's secondary-limit
/// bucket. Capped at [`MUTATION_MAX_WAIT_SECS`].
fn mutation_retry_wait(base_secs: u64, attempt: u32) -> u64 {
    let base = base_secs.clamp(1, MUTATION_MAX_WAIT_SECS);
    let scaled = base
        .saturating_mul(u64::from(attempt))
        .min(MUTATION_MAX_WAIT_SECS);
    let span = (base / 5).max(1);
    let jitter = u64::from(chrono::Utc::now().timestamp_subsec_nanos()) % (span + 1);
    scaled.saturating_add(jitter).min(MUTATION_MAX_WAIT_SECS)
}

/// Render a wait as a short relative duration ("45s", "~2m"). Relative,
/// not an absolute clock time, so the notice reads the same regardless of
/// the daemon's timezone (it may be a headless / remote host whose local
/// time isn't the user's).
fn humanize_wait(secs: u64) -> String {
    if secs < 90 {
        format!("{secs}s")
    } else {
        format!("~{}m", secs.div_ceil(60))
    }
}

/// Humanize a mutation's final failure for the footer — one clean
/// sentence, never a raw GraphQL/JSON blob and never a false "retrying"
/// claim once the retry queue has stopped. A still-rate-limited error names
/// the window when it's known; every other failure is the provider's own
/// (already-humanized) user message.
pub(super) fn humanize_mutation_failure(op: &str, err: &lazybox_core::ProviderError) -> String {
    if err.is_retryable() {
        match err.retry_after_secs() {
            // A window measured in minutes (a primary quota reset) — name it
            // so "try again" has a concrete when.
            Some(secs) if secs >= 120 => {
                format!(
                    "GitHub is rate-limiting {op} — try again in about {}m",
                    secs.div_ceil(60)
                )
            }
            _ => format!("GitHub is rate-limiting {op} — try again shortly"),
        }
    } else {
        let raw = err.user_message();
        // Branch-protection rejections only reach a merge (`g m` / auto-merge /
        // bulk). Every message this humanizer emits is merge-phrased, and its
        // matches are generic enough ("not authorized", "write access to") to
        // fire on an unrelated failure — a reply or label mutation returning a
        // permission error must NOT be relabeled "Can't merge …". Gate on op.
        if op.contains("merge") {
            humanize_branch_protection(&raw).unwrap_or(raw)
        } else {
            raw
        }
    }
}

/// GitHub's branch-protection merge rejections arrive as raw GraphQL
/// strings — opaque, and worded as if the merge is broken when it's
/// usually a "not ready yet" state ("N of N required status checks are
/// expected"). The merge gate deliberately doesn't fetch protection
/// rules (`crates/core/src/policy.rs`), so translating GitHub's own
/// rejection is where the human guidance has to live. Map the common
/// ones to a plain-English notice that names the concrete next step
/// (wait for CI, `g s` sync, `g u` update branch, get an approval).
/// Returns `None` for anything unrecognized so the caller falls back to
/// the raw provider message rather than mistranslating it.
fn humanize_branch_protection(msg: &str) -> Option<String> {
    let lower = msg.to_ascii_lowercase();

    // GitHub's repository rulesets (issue #998) reject with a generic
    // "Repository rule violations found" that names no rule. The provider
    // enriches the message with the base branch's active rules (fetched
    // from the REST rules API) as an "active rules: …" tail when it can;
    // surface those names when present, else point at the usual suspects.
    // Checked first: an enriched name list can itself contain phrases the
    // branches below match on ("required status checks"), which would
    // otherwise mistranslate a ruleset block as a plain CI wait.
    if lower.contains("repository rule violation") {
        let rules = msg
            .split_once("active rules:")
            .map(|(_, tail)| tail.trim().trim_end_matches('.').trim())
            .filter(|t| !t.is_empty());
        return Some(match rules {
            Some(names) => format!(
                "Can't merge — a repository ruleset blocks this merge ({names}). \
                 Satisfy those requirements on GitHub (approvals, signed commits, \
                 an up-to-date branch), then merge — `g s` re-syncs this PR."
            ),
            None => "Can't merge — a repository ruleset blocks this merge. \
                 Check the PR's merge box on GitHub for the unmet requirement \
                 (required reviews, signed commits, linear history, or an \
                 up-to-date branch), satisfy it, then merge — `g s` re-syncs this PR."
                .to_string(),
        });
    }

    if lower.contains("required status check") {
        let required = required_check_count(&lower);
        if lower.contains("expected") {
            let checks = match required {
                Some(n) => format!("GitHub requires {n} status check{}", plural(n)),
                None => "GitHub's required status checks".to_string(),
            };
            return Some(format!(
                "Can't merge yet — {checks} but none have reported for this commit. \
                 Wait for CI to run (or re-trigger it), then merge once it's green — `g s` re-syncs this PR."
            ));
        }
        if lower.contains("pending") {
            return Some(
                "Can't merge yet — required status checks are still running. \
                 Wait for CI to finish and go green, then merge — `g s` re-syncs this PR."
                    .to_string(),
            );
        }
        // "have not succeeded" / "failing" — checks ran but didn't pass.
        return Some(
            "Can't merge — required status checks haven't passed. \
             Fix or re-run the failing checks, then merge — `g s` re-syncs this PR."
                .to_string(),
        );
    }

    if lower.contains("changes requested") {
        return Some(
            "Can't merge — a reviewer requested changes. \
             Address the feedback and get an approving review, then merge."
                .to_string(),
        );
    }

    if lower.contains("approving review") || lower.contains("review is required") {
        let reviews = required_check_count(&lower);
        let need = match reviews {
            Some(n) => format!("at least {n} approving review{}", plural(n)),
            None => "an approving review".to_string(),
        };
        return Some(format!(
            "Can't merge yet — branch protection needs {need}. Get the required approval, then merge."
        ));
    }

    if lower.contains("base branch was modified") {
        return Some(
            "Can't merge — the base branch moved since the merge started. \
             Re-sync (`g s`) and try the merge again."
                .to_string(),
        );
    }

    if lower.contains("out of date") || lower.contains("not up to date") {
        return Some(
            "Can't merge — the branch is out of date with its base. \
             Update it (`g u`), let the checks re-run, then merge."
                .to_string(),
        );
    }

    if lower.contains("not authorized") || lower.contains("write access to") {
        return Some(
            "Can't merge — you're not authorized to merge into this protected branch. \
             It may require admin rights or a different account."
                .to_string(),
        );
    }

    if is_merge_conflict(&lower) {
        return Some(
            "Can't merge — the branch has merge conflicts with its base. \
             Press w to have an agent bring it current and resolve them, then merge."
                .to_string(),
        );
    }

    None
}

/// True when a merge rejection is GitHub's "has merge conflicts" case —
/// the raw GraphQL string is `Pull Request has merge conflicts` (issue
/// #947). Takes an already-lowercased message. Used both to humanize the
/// footer copy and to flag `Event::PrMergeFailed` so the TUI can offer
/// the one-key resolve flow.
fn is_merge_conflict(lower: &str) -> bool {
    lower.contains("merge conflict") || lower.contains("has conflicts")
}

/// Pull the count out of a branch-protection message — the total in
/// "`<n> of <m> required status checks …`" (returns `m`), or the lone
/// number in "at least `N` approving review(s)". `None` when the message
/// carries no number (a bare "review is required").
///
/// Anchored to the "required status check" clause when present so a digit
/// GitHub might append later in the sentence (a check name, a trailing
/// "(run 42)") can't be mistaken for the total; the review path has no
/// such clause, so it scans the whole message.
fn required_check_count(lower: &str) -> Option<u32> {
    let scan = match lower.find("required status check") {
        Some(idx) => &lower[..idx],
        None => lower,
    };
    scan.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u32>().ok())
        .next_back()
        .filter(|n| *n > 0)
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Run a user-initiated mutation as a **daemon-owned** task, detached from
/// the per-connection command pool.
///
/// [`run_mutation_with_retry`] can hold its caller for minutes while it
/// waits out a rate-limit window. A mutation command runs on the serve
/// loop's per-connection `mutations` pool, bounded by `MAX_CONNECTION_MUTATIONS`;
/// left there, a bulk flow under a rate limit (e.g. `Shift-U` on many
/// behind PRs — exactly the burst that trips the secondary limit) would pin
/// slots for the whole window, starving the pool so later commands are
/// rejected, and a client disconnect would drop the still-queued write. As a
/// detached daemon task it does neither: the retry outlives the connection
/// and the outcome reaches every client over the event bus. Same dispatch
/// shape as [`super::auto_merge::on_workspace_committed`].
fn detach_mutation<Fut>(fut: Fut)
where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(fut);
}

/// After a `deleteIssue` fails, should the daemon fall back to a
/// NOT_PLANNED close? Only for a genuine "delete not permitted" — GitHub's
/// FORBIDDEN when the token lacks admin. A *retryable* rate-limit that only
/// exhausted its retry window is not a permission problem, so downgrading a
/// delete to a close on it would silently do the wrong thing; that surfaces
/// as a failure instead.
fn delete_should_fall_back_to_close(delete_err: &lazybox_core::ProviderError) -> bool {
    !delete_err.is_retryable()
}

/// Handle `Command::MergePr` — the MANUAL merge path (`g m`): load the
/// workspace, recover the PR's GraphQL node id from its primary task,
/// and ship a `mergePullRequest` mutation. On success the next poll
/// cycle picks up the new MERGED state and the workspace lands in the
/// Inactive mailbox (or folds into nothing if
/// `closingIssuesReferences` had set up a collapse).
///
/// Deliberately NO fresh eligibility re-check and NO `expectedHeadOid`
/// pin here: the user pressed the key against the state they're
/// looking at, and user intent wins — GitHub's own rejection is the
/// backstop, surfaced verbatim. (`Task` carries no head OID, so there
/// is no locally-known head to pin; the daemon-internal AUTO path —
/// `polling::auto_merge` — re-fetches, re-verifies, and pins the OID
/// instead. The `Command::MergePr` wire contract is unchanged.)
///
/// Errors surface as `Event::ProviderError` so the TUI can flash the
/// reason without us inventing a bespoke event variant.
pub async fn handle_merge_pr(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let config = config.clone();
    detach_mutation(async move { merge_pr_task(&config, workspace_key).await });
}

async fn merge_pr_task(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("merge", msg));
    };

    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!("merge: workspace {workspace_key} not found"));
        return;
    };
    let pr_label = ws.pr.as_ref().map(|p| p.id.key.clone());

    // Route through the workspace's matching provider. The handle
    // dispatches `merge` to github / linear / future-provider based
    // on the workspace key's prefix — the server stays provider-
    // agnostic.
    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) =
        run_mutation_with_retry(&config.bus, "merge", || provider.merge(&ws, None)).await
    {
        tracing::warn!("merge {workspace_key}: {e:?}");
        // A user-initiated merge that GitHub rejected is not a
        // transient blip — surface it as a distinct, persistent error
        // (with the reason) so the user can't mistake it for "the
        // keypress did nothing." The PR stays Open/actionable. A
        // secondary rate limit never lands here unless the retry queue
        // exhausted its window; then the reason is a humanized sentence,
        // not a raw GraphQL blob.
        let label = pr_label
            .clone()
            .unwrap_or_else(|| workspace_key.as_str().to_string());
        let conflict =
            !e.is_retryable() && is_merge_conflict(&e.user_message().to_ascii_lowercase());
        // GitHub just told us the branch conflicts, so our cached
        // `mergeable` was stale (a sibling of #144). Correct it to
        // `Conflicting` — and broadcast the corrected task BEFORE the
        // failure event — so the CONFLICT pill is accurate and the TUI's
        // resolve flow classifies the conflict prompt off fresh state.
        if conflict
            && let Some(mut pr) = ws.pr.clone()
            && !pr.mergeable.is_conflicting()
        {
            pr.mergeable = lazybox_core::Mergeable::Conflicting;
            super::upsert(config, pr).await;
        }
        let _ = config.bus.send(Event::PrMergeFailed {
            workspace_key: workspace_key.clone(),
            pr_label: label,
            reason: humanize_mutation_failure("merge", &e),
            conflict,
        });
        return;
    }
    tracing::info!("merged PR for workspace {workspace_key}");

    // Local Task still reads `Open` — the GitHub mutation succeeded
    // but our stored copy won't reflect MERGED until the next poll.
    // Broadcast `PrMerged` so the TUI can flash a footer notice and
    // the user doesn't think the keypress did nothing.
    if let Some(pr_label) = pr_label {
        let _ = config.bus.send(Event::PrMerged {
            workspace_key: workspace_key.clone(),
            pr_label,
        });
    }
    // Wake the poll loop so MERGED state lands in <5s instead of
    // waiting out the full interval.
    config.poll.wake(true);

    // Fire the workspace-cleanup prompt directly off this successful
    // merge (issue #573). A merged PR drops out of the incremental
    // poll's open-PR search, so the poll-path transition detection only
    // re-observes the open→merged flip via a periodic `is:merged` sweep
    // — minutes later, or within the user's attention window never. The
    // daemon knows the merge just succeeded, so route straight into the
    // same cleanup path the poll would have. The poll path stays as a
    // backstop; its re-emit is collapsed by the reprompt throttle + the
    // TUI's per-key dedupe, so this can't double-prompt.
    if let Some(cleanup) = merged_pr_cleanup(&ws) {
        on_terminal_transition(config, &workspace_key, cleanup).await;
    }
}

/// Handle `Command::UpdateBranch`: load the workspace, recover the PR's
/// GraphQL node id from its primary task, and ship an
/// `updatePullRequestBranch` mutation — the "Update branch" button on
/// github.com. On success the next poll cycle picks up the fresh
/// `mergeStateStatus` and the `BEHIND` tag clears.
///
/// Errors surface as a distinct, persistent `Event::BranchUpdateFailed`
/// (mirroring the merge path) so a rejected update can't be mistaken for
/// "the keypress did nothing." The PR stays actionable.
pub async fn handle_update_branch(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let config = config.clone();
    detach_mutation(async move { update_branch_task(&config, workspace_key).await });
}

async fn update_branch_task(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("update-branch", msg));
    };

    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!(
            "update-branch: workspace {workspace_key} not found"
        ));
        return;
    };
    let pr_label = ws
        .pr
        .as_ref()
        .map(|p| p.id.key.clone())
        .unwrap_or_else(|| workspace_key.as_str().to_string());

    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) =
        run_mutation_with_retry(&config.bus, "update-branch", || provider.update_branch(&ws)).await
    {
        tracing::warn!("update-branch {workspace_key}: {e:?}");
        let _ = config.bus.send(Event::BranchUpdateFailed {
            workspace_key: workspace_key.clone(),
            pr_label,
            reason: humanize_mutation_failure("update-branch", &e),
        });
        return;
    }
    tracing::info!("updated branch for workspace {workspace_key}");

    // The BEHIND tag won't clear until the next poll re-reads
    // `mergeStateStatus`. Broadcast `BranchUpdated` so the TUI flashes a
    // footer notice and the user doesn't think the keypress did nothing.
    let _ = config.bus.send(Event::BranchUpdated {
        workspace_key: workspace_key.clone(),
        pr_label,
    });
    // Wake the poll loop so the refreshed state lands in <5s instead of
    // waiting out the full interval.
    config.poll.wake(true);
}

/// Handle `Command::CloseIssue`: load the workspace, recover the
/// issue's GraphQL node id from its first github issue, and ship a
/// `closeIssue` mutation (state `NOT_PLANNED`). On success the next
/// poll cycle picks up the CLOSED state, the workspace lands in the
/// Inactive mailbox, and the daemon's open→closed detection offers
/// the usual removal prompt.
///
/// A user-initiated close GitHub rejected surfaces as a distinct,
/// persistent `Event::IssueCloseFailed` (mirroring the merge path) so
/// the user can't mistake it for "the keypress did nothing" — the
/// issue stays Open/actionable.
pub async fn handle_close_issue(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let config = config.clone();
    detach_mutation(async move { close_issue_task(&config, workspace_key).await });
}

async fn close_issue_task(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("close-issue", msg));
    };

    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!("close-issue: workspace {workspace_key} not found"));
        return;
    };
    let issue_label = ws
        .gh_issues
        .first()
        .map(|i| i.id.key.clone())
        .unwrap_or_else(|| workspace_key.as_str().to_string());

    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) =
        run_mutation_with_retry(&config.bus, "close-issue", || provider.close_issue(&ws)).await
    {
        tracing::warn!("close-issue {workspace_key}: {e:?}");
        let _ = config.bus.send(Event::IssueCloseFailed {
            workspace_key: workspace_key.clone(),
            issue_label,
            reason: humanize_mutation_failure("close-issue", &e),
        });
        return;
    }
    tracing::info!("closed issue for workspace {workspace_key}");

    // Local Task still reads `Open` until the next poll reconciles.
    // Broadcast `IssueClosed` so the TUI flashes a footer notice and
    // the user doesn't think the keypress did nothing.
    let _ = config.bus.send(Event::IssueClosed {
        workspace_key: workspace_key.clone(),
        issue_label,
    });
    // Wake the poll loop so CLOSED state (and the removal prompt) lands
    // in <5s instead of waiting out the full interval.
    config.poll.wake(true);

    // Route this successful close directly into the terminal-cleanup
    // path (issue #573). A self-initiated close won't necessarily
    // generate a viewer notification, and the poll only re-fetches a
    // closed issue via the notifications-driven single fetch — so the
    // open→closed flip is often never re-observed and cleanup never
    // runs. The daemon knows the close just succeeded, so don't wait for
    // a poll to rediscover it.
    //
    // For a closed issue this doesn't always PROMPT: the #552
    // safe-auto-remove inside `prompt_merged_pr_removal_with` reaps a
    // clean or session-less issue immediately (footer notice, no modal,
    // no archive so a reopen resurfaces it); only genuine local work or a
    // live terminal surfaces the keep/remove modal. The poll path stays
    // as a backstop and composes safely — a woken poll re-observing the
    // same close finds the row already gone (`closed_issue_transition`
    // needs a still-Open predecessor) or is collapsed by the reprompt
    // throttle + the TUI's per-key dedupe, so neither can double-fire.
    if let Some(cleanup) = closed_issue_cleanup(config, &ws) {
        on_terminal_transition(config, &workspace_key, cleanup).await;
    }
}

/// The [`super::TerminalCleanup`] to fire directly off a successful merge
/// of `ws`'s PR (issue #573), or `None` when the workspace carries no PR
/// whose id yields a number. Mirrors the poll path's
/// [`super::merged_transition_pr_number`], minus the predecessor-state
/// guard: the caller just merged, so the transition is known.
fn merged_pr_cleanup(ws: &Workspace) -> Option<super::TerminalCleanup> {
    let n = ws.pr.as_ref().and_then(super::task_number)?;
    Some(super::TerminalCleanup::MergedPr(n))
}

/// The [`super::TerminalCleanup`] to fire directly off a successful close
/// of `ws`'s primary issue (issue #573), or `None` when the close should
/// NOT fire a direct cleanup: the workspace has no github issue, its id
/// carries no number, or a PR claims it (`Closes #N`) and owns the
/// cleanup after the issue row collapses into it. That last case is the
/// same deferral the upsert path makes ([`super::pr_workspace_claiming_issue`])
/// — firing here too would surface a second, stale prompt for the
/// soon-to-be-absorbed issue row.
fn closed_issue_cleanup(config: &ServerConfig, ws: &Workspace) -> Option<super::TerminalCleanup> {
    let issue = ws.gh_issues.first()?;
    let n = super::task_number(issue)?;
    if super::pr_workspace_claiming_issue(config, &issue.id).is_some() {
        return None;
    }
    Some(super::TerminalCleanup::ClosedIssue(n))
}

/// Handle `Command::DeleteOrClose`: remove the workspace's primary
/// upstream item, resolved by kind.
///
/// - **PR** → close it without merging (`closePullRequest`).
/// - **Issue** → hard-delete it (`deleteIssue`). GitHub only permits
///   that for repo admins, so any delete failure degrades to the
///   NOT_PLANNED close the plain close-issue path uses —
///   `Event::IssueDeleted { fell_back_to_close: true }` tells the TUI
///   to say so instead of failing silently.
///
/// Success wakes the poll loop; the item no longer matches the open
/// searches, so the rescope sweep retires the workspace from the inbox
/// (the same level-triggered path an externally closed item takes).
/// Failure surfaces as a persistent `Event::DeleteOrCloseFailed`,
/// mirroring the merge/close paths.
pub async fn handle_delete_or_close(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let config = config.clone();
    detach_mutation(async move { delete_or_close_task(&config, workspace_key).await });
}

async fn delete_or_close_task(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("delete-or-close", msg));
    };

    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!(
            "delete-or-close: workspace {workspace_key} not found"
        ));
        return;
    };
    let label = ws
        .primary_task()
        .map(|t| t.id.key.clone())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| workspace_key.as_str().to_string());

    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };

    if ws.pr.is_some() {
        if let Err(e) =
            run_mutation_with_retry(&config.bus, "delete-or-close", || provider.close_pr(&ws)).await
        {
            tracing::warn!("close-pr {workspace_key}: {e:?}");
            let _ = config.bus.send(Event::DeleteOrCloseFailed {
                workspace_key: workspace_key.clone(),
                label,
                reason: humanize_mutation_failure("close-pr", &e),
            });
            return;
        }
        tracing::info!("closed PR for workspace {workspace_key}");
        let _ = config.bus.send(Event::PrClosed {
            workspace_key: workspace_key.clone(),
            pr_label: label,
        });
    } else {
        // Retry a rate-limited *delete* against the reset window rather than
        // letting it fall through to the close path — a transient limit must
        // not silently downgrade a delete into a NOT_PLANNED close. The
        // fallback is only for a genuine "delete not permitted" (a non-admin's
        // FORBIDDEN); a rate-limit that merely exhausted its window is not a
        // permission problem and surfaces as a failure instead.
        let fell_back_to_close =
            match run_mutation_with_retry(&config.bus, "delete-or-close", || {
                provider.delete_issue(&ws)
            })
            .await
            {
                Ok(()) => false,
                Err(delete_err) if !delete_should_fall_back_to_close(&delete_err) => {
                    tracing::warn!(
                        "delete-issue {workspace_key}: {delete_err:?} — rate-limited, not \
                         downgrading to close"
                    );
                    let _ = config.bus.send(Event::DeleteOrCloseFailed {
                        workspace_key: workspace_key.clone(),
                        label,
                        reason: humanize_mutation_failure("delete-issue", &delete_err),
                    });
                    return;
                }
                Err(delete_err) => {
                    tracing::warn!(
                        "delete-issue {workspace_key}: {delete_err:?} — falling back to close"
                    );
                    if let Err(close_err) =
                        run_mutation_with_retry(&config.bus, "delete-or-close", || {
                            provider.close_issue(&ws)
                        })
                        .await
                    {
                        tracing::warn!("close-issue fallback {workspace_key}: {close_err:?}");
                        let _ = config.bus.send(Event::DeleteOrCloseFailed {
                            workspace_key: workspace_key.clone(),
                            label,
                            reason: humanize_mutation_failure("close-issue", &close_err),
                        });
                        return;
                    }
                    true
                }
            };
        tracing::info!(
            "deleted issue for workspace {workspace_key} (fell_back_to_close: {fell_back_to_close})"
        );
        let _ = config.bus.send(Event::IssueDeleted {
            workspace_key: workspace_key.clone(),
            issue_label: label,
            fell_back_to_close,
        });
    }
    // Wake the poll loop so the vanished/closed state (and the rescope
    // removal) lands in <5s instead of waiting out the full interval.
    config.poll.wake(true);
}

/// Handle `Command::RequestReviewers`: add the given GitHub logins
/// as requested reviewers on the workspace's PR via GraphQL.
/// `union: true` on the mutation so existing reviewers aren't
/// dropped. Idempotent at GitHub's end — re-requesting an already
/// requested reviewer is a no-op.
///
/// On success, kicks a `Refresh` so the inbox reflects the new
/// reviewer set without waiting for the next 60s poll. Errors
/// surface as a `ProviderError` so the TUI footer flags it.
pub async fn handle_request_reviewers(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    logins: Vec<String>,
) {
    let config = config.clone();
    detach_mutation(async move { request_reviewers_task(&config, workspace_key, logins).await });
}

async fn request_reviewers_task(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    logins: Vec<String>,
) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("reviewers", msg));
    };
    if logins.is_empty() {
        return;
    }
    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!(
            "request_reviewers: workspace {workspace_key} not found"
        ));
        return;
    };
    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = run_mutation_with_retry(&config.bus, "reviewers", || {
        provider.request_reviewers(&ws, &logins)
    })
    .await
    {
        tracing::warn!("request_reviewers {workspace_key} {logins:?}: {e:?}");
        emit_err(&format!(
            "request reviewers failed: {}",
            humanize_mutation_failure("reviewers", &e)
        ));
    } else {
        tracing::info!("requested reviewers {logins:?} on workspace {workspace_key}");
        // Wake the poll loop so the reviewer chip on the row
        // updates immediately. Without this the sidebar lags 60s.
        config.poll.wake(true);
    }
}

/// Handle `Command::AddAssignees`: add the given logins as
/// assignees on the workspace's PR or issue (both implement
/// GraphQL's `Assignable` interface). Symmetric with
/// `handle_request_reviewers` — same credential chain, same
/// error-surface pattern.
pub async fn handle_add_assignees(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    logins: Vec<String>,
) {
    let config = config.clone();
    detach_mutation(async move { add_assignees_task(&config, workspace_key, logins).await });
}

async fn add_assignees_task(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    logins: Vec<String>,
) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("assignees", msg));
    };
    if logins.is_empty() {
        return;
    }
    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!(
            "add_assignees: workspace {workspace_key} not found"
        ));
        return;
    };
    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = run_mutation_with_retry(&config.bus, "assignees", || {
        provider.add_assignees(&ws, &logins)
    })
    .await
    {
        tracing::warn!("add_assignees {workspace_key} {logins:?}: {e:?}");
        emit_err(&format!(
            "add assignees failed: {}",
            humanize_mutation_failure("assignees", &e)
        ));
    } else {
        tracing::info!("added assignees {logins:?} on workspace {workspace_key}");
        config.poll.wake(true);
    }
}

/// Handle `Command::SetAssignees`: replace the workspace's assignee
/// set with the given logins (provider diffs against the current
/// task state and fires add + remove mutations as needed). Empty
/// `logins` clears every assignee. Triggers an immediate Refresh-
/// style poll afterwards so the sidebar / right pane reflect the
/// new set without waiting for the regular tick.
pub async fn handle_set_assignees(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    logins: Vec<String>,
) {
    let config = config.clone();
    detach_mutation(async move { set_assignees_task(&config, workspace_key, logins).await });
}

async fn set_assignees_task(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    logins: Vec<String>,
) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("assignees", msg));
    };
    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!(
            "set_assignees: workspace {workspace_key} not found"
        ));
        return;
    };
    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = run_mutation_with_retry(&config.bus, "assignees", || {
        provider.set_assignees(&ws, &logins)
    })
    .await
    {
        tracing::warn!("set_assignees {workspace_key} {logins:?}: {e:?}");
        emit_err(&format!(
            "update assignees failed: {}",
            humanize_mutation_failure("assignees", &e)
        ));
        return;
    }
    tracing::info!("set assignees to {logins:?} on workspace {workspace_key}");
    // Wake the poll loop so the task row picks up the new assignee
    // set immediately — without this the row stays stale for up to
    // a full interval (60s default).
    config.poll.wake(true);
}

/// Handle `Command::SetLabels`: replace the workspace's label set
/// with the given names. Provider diffs against the persisted set
/// and runs add/remove mutations against the GraphQL `Labelable`
/// interface (works for both PRs and issues). Empty `names` clears
/// every label. Kicks the poll loop so the row's chip column
/// updates immediately.
pub async fn handle_set_labels(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    names: Vec<String>,
) {
    let config = config.clone();
    detach_mutation(async move { set_labels_task(&config, workspace_key, names).await });
}

async fn set_labels_task(config: &ServerConfig, workspace_key: WorkspaceKey, names: Vec<String>) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("labels", msg));
    };
    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!("set_labels: workspace {workspace_key} not found"));
        return;
    };
    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) =
        run_mutation_with_retry(&config.bus, "labels", || provider.set_labels(&ws, &names)).await
    {
        tracing::warn!("set_labels {workspace_key} {names:?}: {e:?}");
        emit_err(&format!(
            "update labels failed: {}",
            humanize_mutation_failure("labels", &e)
        ));
        return;
    }
    tracing::info!("set labels to {names:?} on workspace {workspace_key}");
    config.poll.wake(true);
}

/// Handle `Command::FetchRepoLabels`: pull the workspace repo's full
/// label set and broadcast `Event::RepoLabels` so the TUI can
/// populate the picker. On failure, broadcast a retryable
/// `ProviderError` with source `"repo-labels"` — the client is
/// waiting on this reply to mount the picker, and staying silent left
/// its pending request armed forever with no picker and no error. On
/// that failure event the client falls back to a picker built from
/// the labels already on the task (or a clear footer error when the
/// task carries none).
pub async fn handle_fetch_repo_labels(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("repo-labels", msg));
    };
    let Some(ws) = load_workspace(config, &workspace_key) else {
        tracing::debug!("fetch_repo_labels: workspace {workspace_key} not found");
        emit_err(&format!(
            "fetch repo labels: workspace {workspace_key} not found"
        ));
        return;
    };
    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("fetch_repo_labels: {e}");
            emit_err(&e);
            return;
        }
    };
    match provider.list_repo_labels(&ws).await {
        Ok(labels) => {
            tracing::info!("fetch_repo_labels {workspace_key}: {} labels", labels.len());
            let _ = config.bus.send(Event::RepoLabels {
                workspace_key,
                labels,
            });
        }
        Err(e) => {
            tracing::warn!("fetch_repo_labels {workspace_key}: {e:?}");
            emit_err(&format!("label fetch failed: {e}"));
        }
    }
}

/// Handle `Command::FetchRequestableReviewers`: pull the accounts
/// requestable as reviewers on the workspace's PR and broadcast
/// `Event::RequestableReviewers` so the TUI can populate the reviewer
/// picker. On failure, broadcast a `ProviderError` with source
/// `"requestable-reviewers"` — the client is waiting on this reply to
/// mount the picker, and staying silent left its pending request armed
/// forever. On that failure event the client falls back to a picker
/// built from the PR's interaction-derived candidates.
pub async fn handle_fetch_requestable_reviewers(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
) {
    let emit_err = |msg: &str| {
        let _ = config.bus.send(Event::provider_error_retryable(
            "requestable-reviewers",
            msg,
        ));
    };
    let Some(ws) = load_workspace(config, &workspace_key) else {
        tracing::debug!("fetch_requestable_reviewers: workspace {workspace_key} not found");
        emit_err(&format!(
            "fetch requestable reviewers: workspace {workspace_key} not found"
        ));
        return;
    };
    let provider = match build_provider_for_workspace(config, &workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("fetch_requestable_reviewers: {e}");
            emit_err(&e);
            return;
        }
    };
    match provider.list_requestable_reviewers(&ws).await {
        Ok(logins) => {
            tracing::info!(
                "fetch_requestable_reviewers {workspace_key}: {} candidates",
                logins.len()
            );
            let _ = config.bus.send(Event::RequestableReviewers {
                workspace_key,
                logins,
            });
        }
        Err(e) => {
            tracing::warn!("fetch_requestable_reviewers {workspace_key}: {e:?}");
            emit_err(&format!("requestable reviewers fetch failed: {e}"));
        }
    }
}

pub(crate) async fn resolve_gh_client_result(config: &ServerConfig) -> Result<GhClient, String> {
    if let Some(client) = config.poll.cached_gh_client() {
        return Ok(client);
    }
    let _initialization = config.poll.gh_client_cache.lock_initialization().await;
    if let Some(client) = config.poll.cached_gh_client() {
        return Ok(client);
    }
    let cred = lazybox_gh::credential_chain()
        .resolve(lazybox_gh::SOURCE)
        .await
        .map_err(|error| format!("github credentials: {error}"))?;
    let client = GhClient::from_credential(cred)
        .await
        .map_err(|error| format!("github client init: {error}"))?;
    config.poll.cache_gh_client(client.clone());
    Ok(client)
}

/// Clone the process-wide `GhClient` out of the cache, building it once
/// on a cold cache. Polling, reads, and mutations all use this path so
/// they share one governor and one secondary-limit/concurrency domain.
pub(super) async fn resolve_gh_client(config: &ServerConfig) -> Option<GhClient> {
    match resolve_gh_client_result(config).await {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::warn!("{error}");
            None
        }
    }
}

/// Recover `(owner, repo, number)` for a GitHub task from its `repo`
/// (`"owner/repo"`) and the trailing `#N` of its `TaskId` key. `None`
/// for a task that isn't GitHub-shaped — no repo, or an unparseable
/// number.
pub(super) fn github_target(task: &lazybox_core::Task) -> Option<(String, String, u64)> {
    let (owner, name) = task.repo.as_deref()?.split_once('/')?;
    let number = task
        .id
        .key
        .rsplit_once('#')
        .and_then(|(_, n)| n.parse::<u64>().ok())?;
    Some((owner.to_string(), name.to_string(), number))
}

/// Handle `Command::SyncWorkspace`: a targeted re-poll of one
/// workspace's own GitHub entities — the "sync this" action. Instead
/// of the global `Refresh` sweep, deep-fetch the workspace's PR and
/// each linked GitHub issue by `(owner, repo, number)` and upsert the
/// fresh `Task`, so exactly that row's state and read markers refresh
/// at a fraction of a full sweep's cost.
///
/// Reuses the shared [`upsert`](super::upsert) ingestion path (merge +
/// persist + `WorkspaceUpserted` broadcast), so read state is
/// preserved just as it is on a normal poll. No-op for a workspace
/// with no GitHub PR/issue; per-entity fetch failures are logged and
/// skipped so one bad entity never poisons the rest.
pub async fn handle_sync_workspace(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let Some(client) = resolve_gh_client(config).await else {
        return;
    };
    let Some(workspace) = load_workspace(config, &workspace_key) else {
        return;
    };

    // A repo-scoped workspace with no PR/issue of its own (a scratch /
    // project workspace) has no entity to deep-fetch — but the user still
    // pressed `g s` to sync. Fall back to a forced re-poll of the repo so
    // freshly-opened PRs/issues surface now, instead of dead-ending on
    // "nothing to sync". Mirrors the manual `Command::Refresh` path.
    if workspace.pr.is_none() && workspace.gh_issues.is_empty() {
        if workspace.repo_slug().is_some() {
            if let Some(client) = config.poll.cached_gh_client() {
                client.force_full_sweep();
            }
            config.poll.request_force_refresh();
            config.poll.wake(true);
            tracing::info!(
                workspace = %workspace_key,
                "sync_workspace: no PR/issue on the workspace — forced a repo re-poll",
            );
        }
        return;
    }

    // Interactive priority (#1218): `g s` is user-initiated — it must
    // not queue behind the background budget, and a refusal must be
    // VISIBLE, not a swallowed log line: before this, a `g s` during a
    // backoff silently did nothing and the row read as "sync broken".
    let surface = |what: &str, owner: &str, repo: &str, number: u64, e: &dyn std::fmt::Display| {
        tracing::warn!("sync_workspace {owner}/{repo}#{number} ({what}): {e}");
        let _ = config.bus.send(Event::provider_error_retryable(
            "github",
            format!("sync {owner}/{repo}#{number}: {e}"),
        ));
    };
    if let Some(pr) = workspace.pr.as_ref()
        && let Some((owner, repo, number)) = github_target(pr)
    {
        match client
            .fetch_single_pr_interactive(&owner, &repo, number)
            .await
        {
            Ok(Some(task)) => super::upsert(config, task).await,
            Ok(None) => {}
            Err(e) => surface("pr", &owner, &repo, number, &e),
        }
    }

    for issue in &workspace.gh_issues {
        let Some((owner, repo, number)) = github_target(issue) else {
            continue;
        };
        match client
            .fetch_single_issue_interactive(&owner, &repo, number)
            .await
        {
            Ok(Some(task)) => super::upsert(config, task).await,
            Ok(None) => {}
            Err(e) => surface("issue", &owner, &repo, number, &e),
        }
    }

    tracing::info!(workspace = %workspace_key, "sync_workspace: targeted re-poll complete");
}

/// Handle `Command::FetchPrDetails`: pull the workspace's PR
/// review-thread activity from GitHub (the field the inbox-scan
/// query deliberately omits), merge it into the workspace's
/// activity list, and broadcast `WorkspaceUpserted`.
///
/// Idempotent: re-fetching produces the same activities. The merge
/// step dedupes by `node_id`, so calling this twice (e.g. user
/// re-opens the same PR) doesn't duplicate rows. No-op when the
/// workspace has no PR — issue-only workspaces don't have review
/// threads.
///
/// Errors are silent at the user-facing level (no error toast):
/// the inbox row already shows what we have; an upgrade-only
/// failure shouldn't pop a modal. The diagnostic still lands in
/// `/tmp/lazybox.log`.
pub async fn handle_fetch_pr_details(config: &ServerConfig, workspace_key: WorkspaceKey) {
    // Use the persistent client from TickState so the rate budget and
    // observations carry across calls — same logic as the long-lived
    // poll loop. Without this we'd build a fresh client for every
    // user-triggered fetch.
    let Some(client) = resolve_gh_client(config).await else {
        return;
    };

    // The node id comes from the workspace snapshot at call time; the
    // apply step re-loads right before the transform so the activity
    // merge applies to the freshest state — see `apply_pr_details`.
    let Some(initial) = load_workspace(config, &workspace_key) else {
        return;
    };
    let Some(node_id) = initial.pr.as_ref().and_then(|pr| pr.node_id.clone()) else {
        return;
    };
    let details = match client.fetch_pr_details(&node_id).await {
        Ok(Some(details)) => details,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("fetch_pr_details({node_id}): {e}");
            return;
        }
    };
    let merged_count = details.activities.len();
    apply_pr_details(config, &workspace_key, details).await;
    tracing::info!(
        workspace = %workspace_key,
        merged = merged_count,
        "fetch_pr_details: merged review-thread activities + PR fields"
    );
}

/// Splice freshly-fetched `PrDetails` into the stored workspace and
/// persist + broadcast the result. The transform runs against a
/// *re-loaded* copy (via `apply_and_commit`) so a concurrent poll
/// write between the GraphQL fetch and this apply isn't clobbered —
/// the race fix `handle_fetch_pr_details` discovered the hard way
/// (PR row stuck on "CI RUN" after GitHub flipped to SUCCESS).
///
/// `Workspace::merge_activity` dedups by (author, body, created_at)
/// AND remaps `read_indices` across the post-sort positions. A prior
/// implementation did a raw push + sort, which left `read_indices`
/// pointing at stale slots — every lazy-fetch silently scrambled the
/// user's read marks.
///
/// When the backfill populates a previously-empty `closes_issues`,
/// the issue→PR collapse re-runs afterwards. The inbox SEARCH_QUERY
/// now carries `closingIssuesReferences`, so for most PRs the poll
/// path learns the link first; but a details fetch can still be the
/// first to resolve a link the poll missed (an empty/late refs
/// response), and without the re-run the standalone issue workspace
/// would sit next to the PR until some future poll carried the refs.
pub async fn apply_pr_details(
    config: &ServerConfig,
    workspace_key: &WorkspaceKey,
    details: lazybox_gh::PrDetails,
) {
    let mut closes_backfilled = false;
    // The mutation primitive owns the workspace lock for its complete fresh
    // load→transform→commit sequence.
    let outcome = apply_and_commit(config, workspace_key, |ws| {
        let had_closes = ws
            .pr
            .as_ref()
            .is_some_and(|pr| !pr.closes_issues.is_empty());
        ws.merge_activity(&details.activities);
        merge_pr_details_into_workspace(ws, details);
        let has_closes = ws
            .pr
            .as_ref()
            .is_some_and(|pr| !pr.closes_issues.is_empty());
        closes_backfilled = !had_closes && has_closes;
    })
    .await;
    if outcome.is_applied() && closes_backfilled {
        // The primitive's single-key guard is dropped on return before the
        // collapse takes the PR plus every source issue lock.
        super::collapse_closing_issues_for(config, workspace_key).await;
    }
    if outcome.is_applied() {
        // The lazy detail fetch overwrites `pr.ci` / `pr.review`, so it
        // can be the first place lazybox observes an armed PR going
        // green — run the same auto-merge hook the poll commit path
        // runs, off the freshly-committed state.
        if let Some(ws) = super::load_workspace(config, workspace_key) {
            super::auto_merge::on_workspace_committed(
                config,
                workspace_key,
                super::auto_merge::signal_for(&ws),
                true,
            );
        }
    }
}

/// Splice a freshly-fetched `PrDetails` into a workspace's PR slot.
/// No-op when the workspace has no PR (the lazy-fetch path skips
/// issue-only workspaces upstream so this is mostly defensive).
///
/// Field rules:
/// - `ci`, `review`, `role`, `needs_reply`, `last_commenter` —
///   overwrite with the lazy result. The lazy query is authoritative;
///   it has the data the inbox-scan path could only approximate.
/// - `closes_issues`, `checks` — overwrite only when the lazy result
///   is non-empty. Both can be legitimately populated on the poll path
///   yet absent from a given lazy response (`closes_issues` lacks the
///   title fallback; `checks` come from a field the inbox scan drops),
///   so a lazy empty must not clobber the stored value. See the inline
///   note on `closes_issues` (#581).
/// - `unread_count` — recompute from the activity list since lazy
///   knows the full activity count. The workspace-level
///   `Workspace::unread_count()` still respects `read_indices`, so
///   user read state isn't disturbed.
fn merge_pr_details_into_workspace(ws: &mut Workspace, details: lazybox_gh::PrDetails) {
    let Some(pr) = ws.pr.as_mut() else {
        return;
    };
    if !details.closes_issues.is_empty() {
        // Replace verbatim — lazy wins WHEN it has data. But its list comes
        // from `closingIssuesReferences` ALONE (no title fallback), so a lazy
        // empty is not authoritative for our `closes_issues` (= refs ∪ title,
        // built on the poll path) and must not clobber a title-derived close.
        // Clearing a genuinely-removed reference (#581) is the poll path's
        // job — `preserve_lazy_pr_fields` no longer pins the stale value, so
        // every sweep re-derives the authoritative list.
        pr.closes_issues = details.closes_issues;
    }
    if !details.checks.is_empty() {
        pr.checks = details.checks;
    }
    pr.ci = details.ci;
    pr.review = details.review;
    // The lazy query always fetches the reviews connection, so it is
    // authoritative for who has reviewed — overwrite even when empty
    // (a PR with no reviews yet must clear a stale list).
    pr.reviews = details.reviews;
    pr.role = details.role;
    pr.needs_reply = details.needs_reply;
    pr.last_commenter = details.last_commenter;
    pr.unread_count = details.activities.len() as u32;
}

#[cfg(test)]
mod merge_pr_details_tests {
    use super::*;
    use lazybox_core::{Task, TaskId, TaskRole, TaskState, Workspace};

    fn pr_task(closes: Vec<TaskId>) -> Task {
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
            updated_at: chrono::Utc::now(),
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

    fn details(closes: Vec<TaskId>) -> lazybox_gh::PrDetails {
        lazybox_gh::PrDetails {
            activities: vec![],
            closes_issues: closes,
            checks: vec![],
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            reviews: vec![],
            role: TaskRole::Author,
            needs_reply: false,
            last_commenter: None,
        }
    }

    fn issue_42() -> TaskId {
        TaskId {
            source: "github".into(),
            key: "o/r#42".into(),
        }
    }

    /// The lazy details list is `closingIssuesReferences` ALONE (no title
    /// fallback), so an empty lazy result is not authoritative for our
    /// `closes_issues` (= refs ∪ title). It must NOT clobber a stored
    /// title-derived close — clearing genuinely-removed refs is the poll
    /// path's job (#581).
    #[test]
    fn empty_details_does_not_clobber_stored_closes() {
        let mut ws = Workspace::from_task(pr_task(vec![issue_42()]), chrono::Utc::now());
        merge_pr_details_into_workspace(&mut ws, details(vec![]));
        let closes = &ws.pr.as_ref().unwrap().closes_issues;
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0], issue_42());
    }

    /// When lazy DOES carry refs, it wins verbatim — the authoritative
    /// `closingIssuesReferences` replaces whatever was stored.
    #[test]
    fn non_empty_details_replaces_stored_closes() {
        let mut ws = Workspace::from_task(pr_task(vec![issue_42()]), chrono::Utc::now());
        let other = TaskId {
            source: "github".into(),
            key: "o/r#99".into(),
        };
        merge_pr_details_into_workspace(&mut ws, details(vec![other.clone()]));
        let closes = &ws.pr.as_ref().unwrap().closes_issues;
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0], other);
    }
}

/// Admin: walk every persisted workspace, drop sessions whose
/// terminals aren't currently live, and tear down the matching
/// worktree on disk. Inbox rows stay because we only touch
/// `workspace.sessions` (and the on-disk dir) — `workspace.pr` /
/// `gh_issues` aren't modified, so the sidebar keeps the row.
///
/// Live sessions (the user has an attached agent / shell) and worktrees
/// whose clean, pushed state cannot be freshly proven are skipped. Counts are
/// emitted on `Event::CleanWorktreesCompleted` so the TUI can surface a
/// visible summary instead of silently dropping a session on delete failure.
pub async fn handle_clean_worktrees(config: &ServerConfig) {
    clean_worktrees_with(config, &config.worktree_manager()).await;
}

/// Explicit-manager seam for [`handle_clean_worktrees`], keeping the complete
/// safety path hermetic in regression tests.
pub(crate) async fn clean_worktrees_with(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
) {
    // Full-table scan on `spawn_blocking` (issue #34's convention):
    // synchronous rusqlite under a contending process's busy_timeout
    // (5s) would pin a runtime worker.
    let store = config.store.clone();
    let records = match tokio::task::spawn_blocking(move || store.list_workspaces()).await {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => {
            tracing::warn!("clean_worktrees: list_workspaces failed: {e}");
            return;
        }
        Err(e) => {
            tracing::warn!("clean_worktrees: list_workspaces task failed: {e}");
            return;
        }
    };

    // Snapshot live session ids — anything in `terminal_sessions`
    // (the per-terminal owning-session map) is a session we must
    // not touch. Recovered (post-restart) terminals never land in
    // `terminal_sessions`, so also honor `terminal_meta`'s
    // workspace-key view — see `live_workspace_keys`.
    // Hold lock only for the snapshot. Release before expensive fs work.
    // Serialize against new provisioning/adoption: the per-repo lock inside
    // `delete_inspected` will re-run safety probes while it's held.
    let live_sessions: std::collections::HashSet<lazybox_core::SessionId>;
    let live_keys: std::collections::HashSet<String>;
    {
        let _ownership_guard = config.worktree_ownership_lock.lock().await;
        live_sessions = config
            .terminal
            .session_bindings()
            .await
            .into_values()
            .collect();
        live_keys = live_workspace_keys(config).await;
    }
    // Lock released here; expensive fs work follows.

    let tracked = match crate::workspace::collect_tracked_sessions(config).await {
        Ok(tracked) => tracked,
        Err(error) => {
            tracing::warn!(%error, "clean_worktrees: tracked-session scan failed");
            let skipped = records
                .iter()
                .filter_map(|record| record.workspace_json.as_deref())
                .filter_map(|json| serde_json::from_str::<lazybox_core::Workspace>(json).ok())
                .map(|workspace| workspace.sessions.len())
                .sum();
            let _ = config.bus.send(Event::CleanWorktreesCompleted {
                removed: 0,
                skipped,
            });
            return;
        }
    };
    let inspections = match mgr.inspect_worktrees(&tracked).await {
        Ok(inspections) => inspections,
        Err(error) => {
            tracing::warn!(%error, "clean_worktrees: safety inspection failed");
            let skipped = records
                .iter()
                .filter_map(|record| record.workspace_json.as_deref())
                .filter_map(|json| serde_json::from_str::<lazybox_core::Workspace>(json).ok())
                .map(|workspace| workspace.sessions.len())
                .sum();
            let _ = config.bus.send(Event::CleanWorktreesCompleted {
                removed: 0,
                skipped,
            });
            return;
        }
    };
    let by_path: std::collections::HashMap<_, _> = inspections
        .into_iter()
        .map(|row| (canon(&row.path), row))
        .collect();
    let mut removed: usize = 0;
    let mut skipped: usize = 0;

    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(mut workspace) = serde_json::from_str::<lazybox_core::Workspace>(&json) else {
            continue;
        };
        let workspace_key = workspace.key.clone();

        // Walk highest-index-first so removals don't shift indices
        // out from under the loop.
        let session_count = workspace.sessions.len();
        let mut wrote = false;
        for idx in (0..session_count).rev() {
            let session = workspace.sessions[idx].clone();
            if live_sessions.contains(&session.id)
                || live_keys.contains(session.workspace_key.as_str())
            {
                skipped += 1;
                continue;
            }
            if !session.worktree_path.exists() {
                workspace.sessions.remove(idx);
                removed += 1;
                wrote = true;
                continue;
            }
            let Some(row) = by_path.get(&canon(&session.worktree_path)) else {
                tracing::warn!(
                    workspace = %workspace_key,
                    session = %session.id,
                    worktree = %session.worktree_path.display(),
                    "clean_worktrees: checkout is not inspectable — preserving session",
                );
                skipped += 1;
                continue;
            };
            tracing::info!(
                workspace = %workspace_key,
                session = %session.id,
                worktree = %session.worktree_path.display(),
                "clean_worktrees: removing",
            );
            if let Err(error) = mgr.delete_inspected(row, /*force=*/ false).await {
                tracing::warn!(
                    workspace = %workspace_key,
                    session = %session.id,
                    worktree = %session.worktree_path.display(),
                    %error,
                    "clean_worktrees: fresh safety check refused removal — preserving session",
                );
                skipped += 1;
                continue;
            }
            workspace.sessions.remove(idx);
            removed += 1;
            wrote = true;
        }
        if wrote {
            commit_upsert_reported(config, &workspace_key, workspace, "clean stopped worktrees");
        }
    }

    tracing::info!(removed, skipped, "clean_worktrees: done",);
    let _ = config
        .bus
        .send(Event::CleanWorktreesCompleted { removed, skipped });
}

/// Snapshot every persisted session into the inspector's
/// `TrackedSession` shape. Walks `Store::list_workspaces` once, pulls
/// each session out, marks ones in `SessionRunState::Stopped` so the
/// inspector can surface the "session ended but worktree still here"
/// orphan category. Live (`Active`/`Idle`/`Asking`) sessions don't
/// move the orphan needle on their own.
/// Async since the sync-rusqlite offload: the scan + JSON decode run
/// on `spawn_blocking` (issue #34's convention) so a contending
/// process's 5s busy_timeout can't pin a runtime worker.
fn to_dto(row: lazybox_git_ops::WorktreeInspection) -> lazybox_ipc::WorktreeInspectionDto {
    lazybox_ipc::WorktreeInspectionDto {
        path: row.path,
        bare_path: row.bare_path,
        branch: row.branch,
        session_id: row.session_id,
        reasons: row.reasons.iter().map(|r| r.tag().to_string()).collect(),
        size_bytes: row.size_bytes,
        last_modified_unix: row
            .last_modified
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
        has_uncommitted_changes: row.has_uncommitted_changes,
        has_unpushed_commits: row.has_unpushed_commits,
        is_safe_to_delete: row.is_safe_to_delete,
    }
}

fn diff_to_dto(diff: lazybox_git_ops::WorktreeDiff) -> lazybox_ipc::WorkspaceDiffDto {
    lazybox_ipc::WorkspaceDiffDto {
        status: diff.status,
        stat: diff.stat,
        truncated: diff.truncated,
        files: diff
            .files
            .into_iter()
            .map(|file| lazybox_ipc::DiffFileDto {
                old_path: file.old_path,
                path: file.path,
                headers: file.headers,
                hunks: file
                    .hunks
                    .into_iter()
                    .map(|hunk| lazybox_ipc::DiffHunkDto {
                        header: hunk.header,
                        old_start: hunk.old_start,
                        new_start: hunk.new_start,
                        lines: hunk
                            .lines
                            .into_iter()
                            .map(|line| lazybox_ipc::DiffLineDto {
                                kind: match line.kind {
                                    lazybox_git_ops::DiffLineKind::Context => {
                                        lazybox_ipc::DiffLineKindDto::Context
                                    }
                                    lazybox_git_ops::DiffLineKind::Addition => {
                                        lazybox_ipc::DiffLineKindDto::Addition
                                    }
                                    lazybox_git_ops::DiffLineKind::Deletion => {
                                        lazybox_ipc::DiffLineKindDto::Deletion
                                    }
                                    lazybox_git_ops::DiffLineKind::Meta => {
                                        lazybox_ipc::DiffLineKindDto::Meta
                                    }
                                },
                                text: line.text,
                                old_line: line.old_line,
                                new_line: line.new_line,
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Read one workspace checkout's worktree diff and emit it to clients.
pub async fn handle_inspect_workspace_diff(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    target: lazybox_ipc::WorkspaceDiffTarget,
) {
    let result = load_workspace(config, &workspace_key)
        .ok_or_else(|| "workspace not found".to_string())
        .and_then(|workspace| match &target {
            lazybox_ipc::WorkspaceDiffTarget::Session(session_id) => workspace
                .sessions
                .iter()
                .find(|session| session.id == *session_id)
                .map(|session| session.worktree_path.clone())
                .ok_or_else(|| "session worktree not found".to_string()),
            lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout => workspace
                .linked_checkout
                .clone()
                .ok_or_else(|| "linked checkout not found".to_string()),
        });
    let session_key: lazybox_core::SessionKey = workspace_key.as_str().into();
    let session_id = match &target {
        lazybox_ipc::WorkspaceDiffTarget::Session(session_id) => Some(*session_id),
        lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout => None,
    };
    let agent_terminal_ids = config
        .terminal
        .agent_terminals_for_review(&session_key, session_id)
        .await;
    let (diff, error) = match result {
        Ok(path) => match lazybox_git_ops::inspect_worktree_diff(&path).await {
            Ok(diff) => (Some(diff_to_dto(diff)), None),
            Err(error) => {
                tracing::warn!(workspace = %workspace_key, "inspect workspace diff failed: {error}");
                (None, Some(error.to_string()))
            }
        },
        Err(error) => (None, Some(error)),
    };
    let _ = config.bus.send(Event::WorkspaceDiffInspected {
        workspace_key,
        target,
        agent_terminal_ids,
        diff,
        error,
    });
}

/// Run the worktree inspector and emit the result on the bus.
/// Read-only — pair with [`handle_delete_orphaned_worktree`] for
/// destructive follow-up.
pub async fn handle_inspect_worktrees(config: &ServerConfig) {
    inspect_worktrees_with(config, &config.worktree_manager()).await
}

/// Test seam for [`handle_inspect_worktrees`]. Production callers
/// use the default base; tests pass an explicit manager rooted at a
/// tempdir so they don't have to mutate `LAZYBOX_HOME`.
pub(crate) async fn inspect_worktrees_with(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
) {
    let inspections = match crate::workspace::collect_tracked_sessions(config).await {
        Ok(tracked) => mgr.inspect_worktrees(&tracked).await,
        Err(error) => {
            tracing::warn!(%error, "inspect_worktrees: tracked-session scan failed");
            let _ = config.bus.send(Event::WorktreesInspected {
                inspections: Vec::new(),
            });
            return;
        }
    };
    let inspections = match inspections {
        Ok(rows) => rows.into_iter().map(to_dto).collect::<Vec<_>>(),
        Err(e) => {
            tracing::warn!("inspect_worktrees failed: {e}");
            // Empty result is better than no reply — the modal would
            // hang forever waiting on `WorktreesInspected`. The
            // tracing line carries the diagnostic.
            Vec::new()
        }
    };
    let _ = config.bus.send(Event::WorktreesInspected { inspections });
}

/// Expand a leading `~/` against `$HOME`. Mirrors the daemon's mount /
/// scan path expansion so `scan.roots: [~/development]` resolves the
/// same way the CLI `lazybox scan` does.
fn expand_tilde(p: &std::path::Path) -> std::path::PathBuf {
    if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}

/// Scan the dev roots for on-disk git checkouts and reply with
/// `Event::CheckoutsDiscovered`. `roots` overrides `scan.roots` when
/// the user pointed the scan at an explicit folder; empty ⇒ config
/// roots. Read-only — importing is the separate `ImportLocalCheckout`
/// step. Checkouts already backing a linked workspace are dropped so a
/// re-scan doesn't re-offer them.
pub async fn handle_scan_checkouts(config: &ServerConfig, roots: Vec<std::path::PathBuf>) {
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    let roots: Vec<std::path::PathBuf> = if roots.is_empty() {
        cfg.scan.roots.clone()
    } else {
        roots
    }
    .iter()
    .map(|p| expand_tilde(p))
    .collect();

    // Skip anything under lazybox's own managed base — those are its
    // provisioned worktrees, not external dev-folder checkouts.
    let exclude = lazybox_core::paths::state_root();
    let found = if roots.is_empty() {
        Vec::new()
    } else {
        lazybox_git_ops::scan_external_checkouts(&roots, cfg.scan.max_depth, false, &exclude).await
    };

    let already_linked = linked_checkout_paths(config);
    let checkouts = found
        .into_iter()
        .filter(|c| !already_linked.contains(&canonicalize(&c.path)))
        .map(|c| lazybox_ipc::DiscoveredCheckoutDto {
            repo: c
                .remote_url
                .as_deref()
                .and_then(lazybox_core::github_owner_repo_from_url)
                .map(|(owner, repo)| format!("{owner}/{repo}")),
            path: c.path,
            branch: c.branch,
            has_uncommitted_changes: c.has_uncommitted_changes,
        })
        .collect::<Vec<_>>();

    let _ = config.bus.send(Event::CheckoutsDiscovered { checkouts });
}

/// Canonical `linked_checkout` paths of every linked workspace lazybox
/// already tracks, so the scan doesn't re-offer an imported checkout.
/// Best-effort — a store read failure yields an empty set, degrading to
/// "may re-offer" rather than failing the scan.
fn linked_checkout_paths(config: &ServerConfig) -> std::collections::HashSet<std::path::PathBuf> {
    let mut out = std::collections::HashSet::new();
    let Ok(records) = config.store.list_workspaces() else {
        return out;
    };
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(ws) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };
        if let Some(path) = ws.linked_checkout {
            out.insert(canonicalize(&path));
        }
    }
    out
}

fn canonicalize(p: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Delete a single worktree by path. Re-runs a fresh inspection of
/// that one row so the safety check uses live state (the inspector
/// result the TUI is acting on may be seconds old). `force = true`
/// bypasses uncommitted / unpushed / locked refusal.
pub async fn handle_delete_orphaned_worktree(
    config: &ServerConfig,
    path: std::path::PathBuf,
    force: bool,
) {
    delete_orphaned_worktree_with(config, &config.worktree_manager(), path, force).await
}

/// Test seam for [`handle_delete_orphaned_worktree`]. Same contract,
/// explicit manager.
pub(crate) async fn delete_orphaned_worktree_with(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    path: std::path::PathBuf,
    force: bool,
) {
    let tracked = match crate::workspace::collect_tracked_sessions(config).await {
        Ok(tracked) => tracked,
        Err(error) => {
            let _ = config.bus.send(Event::OrphanedWorktreeDeleted {
                path,
                ok: false,
                error: Some(format!("tracked-session scan failed: {error}")),
            });
            return;
        }
    };

    // Re-inspect, then look up this path in the report. Cheap: one
    // walk under `worktrees/` + a few git calls — far less work than
    // a full inspection-from-scratch on the TUI side.
    let inspections = match mgr.inspect_worktrees(&tracked).await {
        Ok(rows) => rows,
        Err(e) => {
            let _ = config.bus.send(Event::OrphanedWorktreeDeleted {
                path,
                ok: false,
                error: Some(format!("inspect failed: {e}")),
            });
            return;
        }
    };
    let target = inspections.iter().find(|row| row.path == path).cloned();
    let Some(target) = target else {
        let _ = config.bus.send(Event::OrphanedWorktreeDeleted {
            path,
            ok: false,
            error: Some("path is no longer under management".into()),
        });
        return;
    };

    // Hold lock only for the critical section: checking if the worktree
    // is orphaned and has a live owner. Release immediately before
    // the expensive deletion operation.
    {
        let _ownership_guard = config.worktree_ownership_lock.lock().await;
        if !target.is_orphaned()
            || crate::spawn_handler::managed_worktree_has_live_session_owner(config, &target.path)
        {
            let _ = config.bus.send(Event::OrphanedWorktreeDeleted {
                path,
                ok: false,
                error: Some("worktree is no longer orphaned".into()),
            });
            return;
        }
    }
    // Lock released here; expensive deletion operation follows.

    match mgr.delete_inspected(&target, force).await {
        Ok(()) => {
            let _ = config.bus.send(Event::OrphanedWorktreeDeleted {
                path: target.path,
                ok: true,
                error: None,
            });
        }
        Err(e) => {
            let _ = config.bus.send(Event::OrphanedWorktreeDeleted {
                path: target.path,
                ok: false,
                error: Some(e.to_string()),
            });
        }
    }
}

/// React to a workspace's primary task reaching a terminal state — a
/// PR merging or an issue closing. Called once per merge/close
/// transition from the upsert path — see
/// [`super::merged_transition_pr_number`] /
/// [`super::closed_issue_transition`].
///
/// Two paths, chosen by `worktree.auto_cleanup_merged` (loaded fresh
/// so the toggle takes effect without a restart):
/// - **on** — reap safe worktrees silently (the opt-in #74 behavior),
///   via [`cleanup_merged_worktrees_with`].
/// - **off** (default) — inspect the backing worktree(s) and emit
///   [`Event::MergedPrRemovable`] so the TUI prompts the user. Their
///   "yes" returns as `Command::RemoveMergedWorkspace`.
pub async fn on_terminal_transition(
    config: &ServerConfig,
    key: &WorkspaceKey,
    cleanup: super::TerminalCleanup,
) {
    let mgr = config.worktree_manager();
    let auto = lazybox_config::Config::load()
        .map(|c| c.worktree.auto_cleanup_merged)
        .unwrap_or(false);
    if auto {
        cleanup_merged_worktrees_with(config, &mgr, key, cleanup).await;
    } else {
        prompt_merged_pr_removal_with(config, &mgr, key, cleanup.removal_state()).await;
    }
}

/// Inspect a terminal-state workspace's backing worktrees and emit
/// [`Event::MergedPrRemovable`] so the TUI can prompt. Read-only — no
/// deletion happens until the user confirms (which comes back as
/// `Command::RemoveMergedWorkspace`). `has_local_work` is set when any
/// session worktree has uncommitted or unpushed work, so the modal can
/// warn before the force-delete. `terminal_state` (merged PR vs closed
/// issue) only steers the confirm-modal wording.
///
/// Every emit path (the open→terminal transition and the per-tick
/// reprompt sweep) funnels through here. A durable "keep" answer
/// ([`lazybox_core::CleanupPrompt::Declined`], issue #499) suppresses permanently;
/// re-emits are otherwise throttled to [`super::REMOVAL_REPROMPT_AFTER`]
/// via [`super::RemovalPromptMemory`] so a user staring at the modal
/// doesn't collect a fresh copy every tick.
///
/// A session-less **merged PR** still prompts (issue #499): removal just
/// drops the tracking row, but a merged PR shouldn't linger unprompted
/// just because it never had a worktree — `has_local_work` and
/// `active_terminal_count` come back `false`/`0`.
///
/// A **closed issue** is the end of that workspace's life, so a
/// *session-less* row (no worktree to reap, no terminal to kill) is
/// removed immediately via [`remove_merged_workspace_with`] (without
/// archiving, so reopening the issue on GitHub resurfaces it), with a
/// footer notice — there's nothing to lose and no reason to make the user
/// dismiss a bare row (issue #552). But a closed issue that still has a
/// backing session — even a clean, idle one — falls through to the
/// keep/remove prompt: reclaiming its worktree and killing its agent
/// without consent contradicts `auto_cleanup_merged: false` and was a
/// silent mass-reap release blocker (issue #1129). Mirrors
/// [`super::removal_candidate_state`].
pub(crate) async fn prompt_merged_pr_removal_with(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    key: &WorkspaceKey,
    terminal_state: lazybox_ipc::RemovableTerminalState,
) {
    let Some(workspace) = load_workspace(config, key) else {
        return;
    };
    // A "keep" answer is durable (issue #499) — check it before the
    // throttle so a declined workspace never re-prompts, even after a
    // restart clears the in-memory cadence memory.
    if workspace.cleanup_prompt == lazybox_core::CleanupPrompt::Declined {
        return;
    }

    let label = workspace
        .primary_task()
        .map(|t| t.id.key.clone())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| key.as_str().to_string());

    // A closed issue is the end of that workspace's life, but reclaiming
    // its backing worktree (often multi-GB) and killing its agent without
    // consent contradicts the user's `auto_cleanup_merged: false` setting
    // — a silent mass reap on a batch of closing issues is a release
    // blocker (issue #1129). So only a *session-less* closed issue
    // auto-removes on sight: that removal just drops the tracking row
    // (nothing on disk to reclaim, no terminal to kill), so there's
    // nothing to lose and no reason to make the user dismiss a bare row —
    // the genuinely-safe half of issue #552. A closed issue that still
    // has any backing session — even a clean, idle one — falls through to
    // the throttled keep/remove prompt below, where the user consents
    // before its worktree is destroyed. (When `auto_cleanup_merged` is
    // on we never reach here: `on_terminal_transition` routes to the
    // silent reaper instead.) Evaluated up front and NOT throttled so a
    // bare row is cleared promptly. The removal does NOT archive, so
    // reopening the issue on GitHub resurfaces it.
    if terminal_state == lazybox_ipc::RemovableTerminalState::Closed {
        let session_paths = workspace_worktree_paths(&workspace);
        let sessionless = session_paths.is_empty() && count_live_terminals(config, key).await == 0;
        if sessionless
            && let Some(reclaimed) = remove_merged_workspace_with(
                config,
                key,
                crate::workspace::WorkspaceRemovalReason::ClosedAuto,
            )
            .await
        {
            let mut body = format!("Removed workspace for closed {label}");
            if let Some(space) = crate::workspace::reclaimed_notice_body(reclaimed) {
                body.push_str(" · ");
                body.push_str(&space);
            }
            let _ = config.bus.send(Event::Notification {
                title: "lazybox".into(),
                body,
            });
            return;
        }
        // Not session-less, or the removal failed (row still present) —
        // fall through to the throttled prompt rather than retry every tick.
    }

    {
        let mut prompts = config.poll.removal_prompts.lock().await;
        let now = std::time::Instant::now();
        let stale = prompts
            .prompted
            .get(key.as_str())
            .map(|prev| now.duration_since(*prev) >= super::REMOVAL_REPROMPT_AFTER)
            .unwrap_or(true);
        if !stale {
            return;
        }
        prompts.prompted.insert(key.as_str().to_string(), now);
    }

    let session_paths = workspace_worktree_paths(&workspace);
    let active_terminal_count = count_live_terminals(config, key).await;
    let has_local_work = workspace_local_work(config, mgr, key, &session_paths)
        .await
        // A failed inspection is a warning, never a clean bill of health.
        // The destructive command performs the same fail-closed check again.
        .unwrap_or(true);

    tracing::info!(
        workspace = %key,
        active = active_terminal_count,
        has_local_work,
        ?terminal_state,
        "terminal state — prompting for workspace + worktree removal"
    );
    let _ = config.bus.send(Event::MergedPrRemovable {
        workspace_key: key.clone(),
        label,
        terminal_state,
        active_terminal_count,
        has_local_work,
    });
}

/// Handle `Command::RemoveMergedWorkspace`: the user confirmed the
/// merged-PR removal modal. Kills the sessions, drops the row, and
/// reclaims the now-idle worktree directories via the internal
/// `WorkspaceLifecycle` coordinator. A confirmed removal
/// archives, so the next poll doesn't resurrect the row. Returns the
/// reclaimed worktree space, or `None` if the removal was preserved.
pub async fn remove_merged_workspace(
    config: &ServerConfig,
    key: &WorkspaceKey,
) -> Option<crate::workspace::Reclaimed> {
    remove_merged_workspace_with(
        config,
        key,
        crate::workspace::WorkspaceRemovalReason::MergedConfirmed,
    )
    .await
}

/// Shared removal for the confirmed and closed-issue-auto paths.
/// `reason` controls whether the row is recorded in `KV_KEY_ARCHIVED`:
/// a user-confirmed removal archives (stay gone), the closed-issue
/// auto-remove (issue #552) does not (so a reopen resurfaces it). The
/// worktree directories are reclaimed inside `WorkspaceLifecycle`
/// (issue #575); this only adds the removal-prompt bookkeeping cleanup.
pub(crate) async fn remove_merged_workspace_with(
    config: &ServerConfig,
    key: &WorkspaceKey,
    reason: crate::workspace::WorkspaceRemovalReason,
) -> Option<crate::workspace::Reclaimed> {
    // Kills backing terminals, removes the row, and reclaims each
    // session's worktree dir; `reason` decides whether the next poll
    // may resurrect it. `None` means a prerequisite failed and the
    // lifecycle/store path already emitted a precise error — keep the
    // removal-prompt memory intact so the user can retry.
    let reclaimed = crate::workspace::WorkspaceLifecycle::new(config)
        .remove(key, reason)
        .await?;

    // The row is actually gone — now drop its reprompt bookkeeping. On a
    // failed prerequisite it must remain so the level-triggered prompt can
    // offer the destructive action again.
    config
        .poll
        .removal_prompts
        .lock()
        .await
        .prompted
        .remove(key.as_str());

    Some(reclaimed)
}

/// Canonicalize a path for cross-referencing inspector rows (reported
/// straight from `read_dir`) against a session's stored
/// `worktree_path` — the two can differ purely by symlink resolution
/// (macOS `/var` → `/private/var`, a symlinked `LAZYBOX_HOME`, …).
fn canon(p: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Canonicalized set of every session's worktree path in a workspace.
fn workspace_worktree_paths(
    workspace: &Workspace,
) -> std::collections::HashSet<std::path::PathBuf> {
    workspace
        .sessions
        .iter()
        .map(|s| canon(&s.worktree_path))
        .collect()
}

/// Whether any of a workspace's session worktrees hold uncommitted or
/// unpushed work. `Some(false)` — clean, and also the answer for a
/// session-less workspace (no worktree to inspect). `Some(true)` — at
/// least one worktree is dirty or ahead of its remote. `None` — the
/// inspect itself failed, so cleanliness is unknown: callers that would
/// destroy a worktree must treat `None` as unsafe, never as clean.
async fn workspace_local_work(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    key: &WorkspaceKey,
    session_paths: &std::collections::HashSet<std::path::PathBuf>,
) -> Option<bool> {
    if session_paths.is_empty() {
        return Some(false);
    }
    let tracked = match crate::workspace::collect_tracked_sessions(config).await {
        Ok(tracked) => tracked,
        Err(error) => {
            tracing::warn!(workspace = %key, %error, "prompt_merged_pr_removal: tracked-session scan failed");
            return None;
        }
    };
    match mgr.inspect_worktrees(&tracked).await {
        Ok(rows) => Some(session_paths.iter().any(|path| {
            rows.iter()
                .find(|row| canon(&row.path) == *path)
                .map(|row| {
                    row.has_uncommitted_changes
                        || row.has_unpushed_commits
                        || row.reasons.contains(&lazybox_git_ops::OrphanReason::Locked)
                })
                // The path exists in the workspace record but the inspector
                // cannot account for it. Warn and let the final server gate
                // refuse removal rather than presenting an unsafe "clean".
                .unwrap_or_else(|| path.exists())
        })),
        Err(e) => {
            tracing::warn!(workspace = %key, "prompt_merged_pr_removal: inspect failed: {e}");
            None
        }
    }
}

/// Cancel a pending workspace-removal prompt (issue #552): drop the
/// reprompt throttle memory so a re-close prompts cleanly, then
/// broadcast [`Event::RemovalCancelled`] so any TUI dismisses a still-
/// mounted "remove closed issue?" modal. Called when a closed issue
/// reopens before its removal was acted on.
pub(crate) async fn cancel_pending_removal(config: &ServerConfig, key: &WorkspaceKey) {
    config
        .poll
        .removal_prompts
        .lock()
        .await
        .prompted
        .remove(key.as_str());
    let _ = config.bus.send(Event::RemovalCancelled {
        workspace_key: key.clone(),
    });
}

/// Count live terminals (PTY/tmux sessions) bound to a workspace key,
/// via the authoritative `terminal_meta` map.
pub(super) async fn count_live_terminals(config: &ServerConfig, key: &WorkspaceKey) -> usize {
    config
        .terminal
        .metadata_map()
        .await
        .into_values()
        .filter(|(sk, _)| sk.as_str() == key.as_str())
        .count()
}

/// Workspace keys (as `SessionKey` strings) that currently have a
/// live terminal, per `terminal_meta`. Complements the
/// `terminal_sessions`-derived `SessionId` set in the cleanup paths:
/// terminals recovered after a daemon restart repopulate
/// `terminal_meta` but never `terminal_sessions` (the spawn-time
/// terminal → SessionId map), so a session-id-only liveness check
/// would happily reap a worktree under a live recovered agent. The
/// key-level set is coarser (it pins every session in the workspace)
/// but errs on the safe side.
async fn live_workspace_keys(config: &ServerConfig) -> std::collections::HashSet<String> {
    config
        .terminal
        .metadata_map()
        .await
        .into_values()
        .map(|(sk, _)| sk.as_str().to_string())
        .collect()
}

/// Silent worktree reaper for the opt-in `auto_cleanup_merged` path —
/// the cleanup half of [`on_terminal_transition`]. Explicit manager
/// (tempdir-rooted in tests) and no config gate so the caller decides
/// when cleanup runs.
///
/// Only removes worktrees the inspector flags `is_safe_to_delete`
/// (clean tree, pushed, unlocked — typically the merged branch was
/// auto-deleted upstream) AND whose session has no live terminal
/// attached. Each reaped session is dropped from the workspace and the
/// trimmed record re-committed; a final [`Event::Notification`] tells
/// the user what was cleaned.
pub(crate) async fn cleanup_merged_worktrees_with(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    key: &WorkspaceKey,
    cleanup: super::TerminalCleanup,
) {
    let Some(mut workspace) = load_workspace(config, key) else {
        return;
    };
    if workspace.sessions.is_empty() {
        return;
    }

    // Never yank a worktree the user is still attached to, even if its
    // tree is clean — that's the "don't pull a folder out from under
    // an active agent" guard the inspector's safety check can't make
    // on its own. Union with `terminal_meta`'s workspace-key view so
    // terminals recovered after a daemon restart (which never
    // repopulate `terminal_sessions`) still count as live.
    let live: std::collections::HashSet<lazybox_core::SessionId> = {
        config
            .terminal
            .session_bindings()
            .await
            .into_values()
            .collect()
    };
    let live_keys = live_workspace_keys(config).await;

    let tracked = match crate::workspace::collect_tracked_sessions(config).await {
        Ok(tracked) => tracked,
        Err(error) => {
            tracing::warn!(workspace = %key, %error, "cleanup_merged_worktrees: tracked-session scan failed");
            return;
        }
    };
    let inspections = match mgr.inspect_worktrees(&tracked).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(workspace = %key, "cleanup_merged_worktrees: inspect failed: {e}");
            return;
        }
    };

    // Index inspection rows by canonicalized path. The inspector
    // reports `path` straight from `read_dir`, while a session's
    // `worktree_path` is whatever was stored at checkout — the two can
    // differ purely by symlink resolution (e.g. macOS `/var` →
    // `/private/var`, or a symlinked `LAZYBOX_HOME`). Canonicalizing both
    // sides matches the inspector's own `canonical_or_self` keying so a
    // safe worktree is never silently skipped over a cosmetic path
    // difference.
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let by_path: std::collections::HashMap<
        std::path::PathBuf,
        &lazybox_git_ops::WorktreeInspection,
    > = inspections.iter().map(|r| (canon(&r.path), r)).collect();

    let mut removed: usize = 0;
    let mut wrote = false;
    // Walk highest-index-first so removals don't shift indices out
    // from under the loop.
    for idx in (0..workspace.sessions.len()).rev() {
        let session_id = workspace.sessions[idx].id;
        let worktree_path = workspace.sessions[idx].worktree_path.clone();
        if live.contains(&session_id)
            || live_keys.contains(workspace.sessions[idx].workspace_key.as_str())
        {
            continue;
        }
        let Some(&row) = by_path.get(&canon(&worktree_path)) else {
            continue;
        };
        if !row.is_safe_to_delete {
            continue;
        }
        match mgr.delete_inspected(row, /*force=*/ false).await {
            Ok(()) => {
                tracing::info!(
                    workspace = %key,
                    session = %session_id,
                    worktree = %worktree_path.display(),
                    "cleanup_merged_worktrees: removed",
                );
                workspace.sessions.remove(idx);
                removed += 1;
                wrote = true;
            }
            Err(e) => {
                tracing::warn!(
                    worktree = %worktree_path.display(),
                    "cleanup_merged_worktrees: delete refused: {e}",
                );
            }
        }
    }

    if wrote {
        commit_upsert_reported(config, key, workspace, "clean merged worktrees");
    }
    if removed > 0 {
        let noun = if removed == 1 {
            "worktree"
        } else {
            "worktrees"
        };
        let _ = config.bus.send(Event::Notification {
            title: "lazybox".into(),
            body: format!("Cleaned up {removed} {noun} for {}", cleanup.describe()),
        });
    }
}

/// Test seam for the legacy rescope reaper — explicit manager so its
/// fail-closed deletion contract remains covered without mutating
/// `LAZYBOX_HOME`. Production teardown is now owned by
/// `WorkspaceLifecycle`, which inspects and reclaims tracked worktrees after
/// taking the workspace lock.
#[cfg(test)]
pub(crate) async fn reap_safe_workspace_worktrees_with(
    config: &ServerConfig,
    mgr: &lazybox_git_ops::WorktreeManager,
    workspace: &lazybox_core::Workspace,
) {
    if workspace.sessions.is_empty() {
        return;
    }
    // Same liveness union as `cleanup_merged_worktrees_with`:
    // spawn-time session ids plus the recovered-terminal workspace
    // keys from `terminal_meta`.
    let live: std::collections::HashSet<lazybox_core::SessionId> = {
        config
            .terminal
            .session_bindings()
            .await
            .into_values()
            .collect()
    };
    let live_keys = live_workspace_keys(config).await;

    let tracked = match crate::workspace::collect_tracked_sessions(config).await {
        Ok(tracked) => tracked,
        Err(error) => {
            tracing::warn!(workspace = %workspace.key, %error, "reap_workspace_worktrees: tracked-session scan failed");
            return;
        }
    };
    let inspections = match mgr.inspect_worktrees(&tracked).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(workspace = %workspace.key, "reap_workspace_worktrees: inspect failed: {e}");
            return;
        }
    };
    let by_path: std::collections::HashMap<
        std::path::PathBuf,
        &lazybox_git_ops::WorktreeInspection,
    > = inspections.iter().map(|r| (canon(&r.path), r)).collect();

    for session in &workspace.sessions {
        if live.contains(&session.id) || live_keys.contains(session.workspace_key.as_str()) {
            continue;
        }
        let Some(&row) = by_path.get(&canon(&session.worktree_path)) else {
            continue;
        };
        if !row.is_safe_to_delete {
            tracing::info!(
                workspace = %workspace.key,
                worktree = %session.worktree_path.display(),
                "reap_workspace_worktrees: preserving (not safe to delete)"
            );
            continue;
        }
        match mgr.delete_inspected(row, /*force=*/ false).await {
            Ok(()) => {
                tracing::info!(
                    workspace = %workspace.key,
                    worktree = %session.worktree_path.display(),
                    "reap_workspace_worktrees: removed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    worktree = %session.worktree_path.display(),
                    "reap_workspace_worktrees: delete refused: {e}"
                );
            }
        }
    }
}

/// Attention score for prefetch selection (higher = warm sooner). The
/// base of 1 marks a PR as *fetchable*; anything above it clears the
/// `score > 1` threshold `prefetch_top_pr_details` applies and earns a
/// prefetch.
///
/// Engagement signals are deliberately stronger than inbox noise:
/// - live agent → +300
/// - focused workspace → +250
/// - own open PR → +25
///
/// Task signals:
/// - CI failing → +100 (highest-actionability — user wants to fix)
/// - Review pending / changes-requested → +50
/// - Unread activity → +10 per item (capped at +50)
/// - Base +1 (fetchable — the caller drops PRs without a `node_id`)
pub(crate) fn prefetch_score(pr: &Task, engagement: EngagementSignals) -> i32 {
    let mut score: i32 = 1;
    if engagement.live_agent {
        score += 300;
    }
    if engagement.focused {
        score += 250;
    }
    if engagement.own_open_pr {
        score += 25;
    }
    if matches!(pr.ci, CiStatus::Failure | CiStatus::Mixed) {
        score += 100;
    }
    if matches!(
        pr.review,
        ReviewStatus::ChangesRequested | ReviewStatus::Pending
    ) {
        score += 50;
    }
    score += (pr.unread_count.min(5) as i32) * 10;
    score
}

fn detail_prefetch_allowed(tier: EngagementTier, already_prefetched: bool) -> bool {
    tier == EngagementTier::Warm && !already_prefetched
}

fn prefetch_rank_score(pr: &Task, engagement: EngagementSignals, tier: EngagementTier) -> i32 {
    prefetch_score(pr, engagement) + i32::from(tier == EngagementTier::Hot) * 1_000
}

/// Post-tick prefetch: after a successful poll, pick the top-N PRs
/// most likely to be clicked next and concurrently fetch their
/// review-thread details so the right pane is hot when the user
/// gets there.
///
/// Why N=5 + concurrency=3: a measured prefetch batch of 5 PRs costs
/// ~1s wall-clock, ~59 KB, and 5 GraphQL units total — 1 unit per
/// `fetch_pr_details` call, not the ~550 originally guessed (see
/// `docs/sync-performance.md`). Both the GraphQL budget (5000/hr) and
/// the local rate bucket (capacity 30, refill 30/min) absorb that
/// trivially. Concurrency=3 keeps the local budget healthy alongside
/// the parallel main+merged+watched-repo branches of the same tick.
///
/// Warm rows dedup through `TickState::prefetched_pr_details`: once
/// their threads are pulled this daemon session they stay quiet. Hot
/// rows already carry the same full fields from the batched targeted
/// query, while cold rows never enter this deeper query.
///
/// Ranks candidates with `prefetch_score`; 0-above-base workspaces
/// are skipped — they don't need the prefetch.
pub async fn prefetch_top_pr_details(
    config: &ServerConfig,
    polled: &[WorkspaceKey],
    state: &mut TickState,
) {
    use futures::stream::{self, StreamExt};

    const PREFETCH_TOP_N: usize = 5;
    const PREFETCH_CONCURRENCY: usize = 3;

    // Reuse the persistent GhClient cache. If absent (linear-only
    // setup, or auth failed earlier), prefetch is a no-op.
    let Some(client) = config.poll.cached_gh_client() else {
        return;
    };

    // Score every polled workspace, keep the ones with a fetchable
    // PR. `polled` is the key list from the just-completed tick; load
    // each via the store path the rest of the handler module uses so
    // the scoring sees the post-upsert state.
    let engagement = config.poll.engagement_snapshot();
    let mut scored: Vec<(i32, String, WorkspaceKey)> = Vec::new();
    let mut seen_node_ids = std::collections::HashSet::new();
    for key in polled {
        let tier = engagement.tier_for(key);
        if tier != EngagementTier::Warm {
            continue;
        }
        let Some(ws) = load_workspace(config, key) else {
            continue;
        };
        let Some(pr) = ws.pr.as_ref() else {
            continue;
        };
        let Some(node_id) = pr.node_id.clone() else {
            continue;
        };
        if !seen_node_ids.insert(node_id.clone()) {
            continue;
        }
        if !detail_prefetch_allowed(tier, state.prefetched_pr_details.contains(&node_id)) {
            continue;
        }
        let score = prefetch_rank_score(pr, engagement.signals_for(key), tier);
        if score > 1 {
            scored.push((score, node_id, key.clone()));
        }
    }
    if scored.is_empty() {
        return;
    }
    // Highest scores first, take N.
    scored.sort_by_key(|s| std::cmp::Reverse(s.0));
    scored.truncate(PREFETCH_TOP_N);

    let total = scored.len();
    tracing::info!("prefetch_top_pr_details: prefetching {total} PRs concurrently");

    // Mark them all prefetched up-front so a slow result doesn't
    // re-trigger on the next tick.
    for (_, node_id, _) in &scored {
        state.prefetched_pr_details.insert(node_id.clone());
    }

    let started = std::time::Instant::now();
    let merged: usize = stream::iter(scored)
        .map(|(_score, node_id, key)| {
            let client = client.clone();
            async move {
                // Route through `apply_pr_details` so the race fix in
                // `handle_fetch_pr_details` applies here too (re-load
                // before the activity merge), along with the
                // closes-issues collapse re-run.
                let details = match client.prefetch_pr_details(&node_id).await {
                    Ok(Some(details)) => details,
                    Ok(None) => return 0usize,
                    Err(e) => {
                        tracing::debug!(
                            "prefetch_top_pr_details: fetch_pr_details({node_id}) failed: {e}",
                        );
                        return 0usize;
                    }
                };
                let merged_here = details.activities.len();
                apply_pr_details(config, &key, details).await;
                merged_here
            }
        })
        .buffer_unordered(PREFETCH_CONCURRENCY)
        .collect::<Vec<usize>>()
        .await
        .into_iter()
        .sum();
    tracing::info!(
        "prefetch_top_pr_details: {total} PRs prefetched in {}ms, {merged} activities merged",
        started.elapsed().as_millis()
    );
}

#[cfg(test)]
mod github_target_tests {
    use super::github_target;
    use lazybox_core::{CiStatus, Mergeable, ReviewStatus, Task, TaskId, TaskRole, TaskState};

    fn task(repo: Option<&str>, key: &str) -> Task {
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
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/42".into(),
            repo: repo.map(Into::into),
            branch: None,
            base_branch: None,
            updated_at: chrono::Utc::now(),
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

    #[test]
    fn recovers_owner_repo_number_from_a_github_task() {
        let t = task(Some("octo/widgets"), "octo/widgets#42");
        assert_eq!(
            github_target(&t),
            Some(("octo".into(), "widgets".into(), 42))
        );
    }

    #[test]
    fn none_without_a_repo() {
        let t = task(None, "octo/widgets#42");
        assert_eq!(github_target(&t), None);
    }

    #[test]
    fn none_when_the_key_has_no_parseable_number() {
        let t = task(Some("octo/widgets"), "octo/widgets");
        assert_eq!(github_target(&t), None);
    }
}

#[cfg(test)]
mod prefetch_score_tests {
    use super::{
        EngagementSignals, EngagementTier, detail_prefetch_allowed, prefetch_rank_score,
        prefetch_score,
    };
    use lazybox_core::{CiStatus, Mergeable, ReviewStatus, Task, TaskId, TaskRole, TaskState};

    fn pr() -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: "octo/widgets#42".into(),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/42".into(),
            repo: Some("octo/widgets".into()),
            branch: None,
            base_branch: None,
            updated_at: chrono::Utc::now(),
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
            node_id: Some("PR_node".into()),
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

    #[test]
    fn quiet_pr_stays_at_the_fetchable_base() {
        assert_eq!(prefetch_score(&pr(), EngagementSignals::default()), 1);
    }

    #[test]
    fn failing_ci_dominates() {
        let mut p = pr();
        p.ci = CiStatus::Failure;
        assert_eq!(prefetch_score(&p, EngagementSignals::default()), 101);
        p.ci = CiStatus::Mixed;
        assert_eq!(prefetch_score(&p, EngagementSignals::default()), 101);
    }

    #[test]
    fn pending_review_and_changes_requested_add_fifty() {
        let mut p = pr();
        p.review = ReviewStatus::Pending;
        assert_eq!(prefetch_score(&p, EngagementSignals::default()), 51);
        p.review = ReviewStatus::ChangesRequested;
        assert_eq!(prefetch_score(&p, EngagementSignals::default()), 51);
    }

    #[test]
    fn unread_activity_is_capped_at_five_items() {
        let mut p = pr();
        p.unread_count = 3;
        assert_eq!(prefetch_score(&p, EngagementSignals::default()), 1 + 30);
        p.unread_count = 100;
        assert_eq!(prefetch_score(&p, EngagementSignals::default()), 1 + 50);
    }

    #[test]
    fn weights_stack_across_signals() {
        let mut p = pr();
        p.ci = CiStatus::Failure;
        p.review = ReviewStatus::ChangesRequested;
        p.unread_count = 2;
        assert_eq!(
            prefetch_score(&p, EngagementSignals::default()),
            1 + 100 + 50 + 20
        );
    }

    #[test]
    fn engagement_outweighs_inbox_noise_and_stacks() {
        let signals = EngagementSignals {
            live_agent: true,
            focused: true,
            own_open_pr: true,
        };
        assert_eq!(prefetch_score(&pr(), signals), 1 + 300 + 250 + 25);
    }

    #[test]
    fn hot_and_cold_rows_skip_detail_prefetch_while_warm_rows_dedup() {
        assert!(!detail_prefetch_allowed(EngagementTier::Hot, false));
        assert!(!detail_prefetch_allowed(EngagementTier::Hot, true));
        assert!(!detail_prefetch_allowed(EngagementTier::Warm, true));
        assert!(detail_prefetch_allowed(EngagementTier::Warm, false));
        assert!(!detail_prefetch_allowed(EngagementTier::Cold, false));
        assert!(!detail_prefetch_allowed(EngagementTier::Cold, true));
    }

    #[test]
    fn hot_rank_beats_a_noisy_warm_pr() {
        let hot = prefetch_rank_score(
            &pr(),
            EngagementSignals {
                own_open_pr: true,
                ..EngagementSignals::default()
            },
            EngagementTier::Hot,
        );
        let mut noisy = pr();
        noisy.ci = CiStatus::Failure;
        noisy.review = ReviewStatus::ChangesRequested;
        noisy.unread_count = 5;
        let warm = prefetch_rank_score(&noisy, EngagementSignals::default(), EngagementTier::Warm);
        assert!(hot > warm);
    }
}

#[cfg(test)]
mod inspect_tests {
    //! Integration tests for `handle_inspect_worktrees` +
    //! `handle_delete_orphaned_worktree`. The handlers depend on a
    //! `WorktreeManager` plus the workspace store; both are wired up
    //! against a tempdir + an in-memory store here so the tests are
    //! hermetic (no `LAZYBOX_HOME` env mutation, no shared on-disk
    //! state with other tests).

    use super::*;
    use crate::ServerConfig;
    use lazybox_core::{SessionId, SessionKind, SessionRunState, WorkspaceSession};
    use lazybox_ipc::{Event, TerminalId, TerminalKind, WorkspaceDiffTarget};
    use lazybox_store::{MemoryStore, Store, WorkspaceRecord};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::TempDir;

    struct Fixture {
        base: TempDir,
        _upstream: TempDir,
        upstream_path: PathBuf,
        bare: PathBuf,
    }

    async fn run(cwd: &Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .current_dir(cwd)
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("tag.gpgsign=false")
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    async fn setup_fixture() -> Fixture {
        let upstream = TempDir::new().unwrap();
        run(upstream.path(), &["init", "-q", "-b", "main"]).await;
        run(upstream.path(), &["config", "user.email", "t@e.st"]).await;
        run(upstream.path(), &["config", "user.name", "tester"]).await;
        std::fs::write(upstream.path().join("README.md"), "hi\n").unwrap();
        run(upstream.path(), &["add", "."]).await;
        run(upstream.path(), &["commit", "-q", "-m", "init"]).await;

        let base = TempDir::new().unwrap();
        let bare = base.path().join("repos").join("o").join("r.git");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        run(
            base.path(),
            &[
                "clone",
                "--bare",
                "-q",
                &upstream.path().to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        )
        .await;
        // Deliberately NO `remote.origin.fetch` refspec: this is the
        // legacy bare-clone shape lazybox wrote before #1253 (and what
        // `git clone --bare` produces), still on disk for old installs
        // until `bare_repo_health`'s one-time repair runs. `@{u}` can
        // never resolve in it, so these tests prove the handlers work
        // off the branch's own `refs/remotes/origin/<branch>` ref
        // (maintained per-fetch by `add_wt` below, mirroring
        // `fetch_origin_ref`) rather than the config refspec.

        Fixture {
            base,
            upstream_path: upstream.path().to_path_buf(),
            _upstream: upstream,
            bare,
        }
    }

    /// Mirror of `WorktreeManager::checkout_at`: ensure a remote-
    /// tracking ref exists, then `worktree add -B` off it. The fixture
    /// produces a worktree shape identical to what the daemon creates
    /// at runtime.
    async fn add_wt(fx: &Fixture, name: &str, branch: &str) -> PathBuf {
        let has_branch = std::process::Command::new("git")
            .current_dir(&fx.upstream_path)
            .args(["rev-parse", "--verify", "--quiet", branch])
            .status()
            .unwrap()
            .success();
        if !has_branch {
            run(&fx.upstream_path, &["branch", branch]).await;
        }
        run(
            &fx.bare,
            &[
                "fetch",
                "-q",
                "origin",
                &format!("+{branch}:refs/remotes/origin/{branch}"),
            ],
        )
        .await;
        let wt = fx.base.path().join("worktrees").join(name);
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        run(
            &fx.bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                branch,
                &wt.to_string_lossy(),
                &format!("refs/remotes/origin/{branch}"),
            ],
        )
        .await;
        // Production `checkout_at` records tracking config when the
        // worktree branches off a remote-tracking ref. Mirror it: in
        // the legacy clone shape it can't make `@{u}` resolve, but it
        // is the evidence `branch_has_upstream_config` uses to classify
        // a vanished remote ref as BranchDeletedUpstream.
        run(
            &fx.bare,
            &["config", &format!("branch.{branch}.remote"), "origin"],
        )
        .await;
        run(
            &fx.bare,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        )
        .await;
        run(&wt, &["config", "user.email", "t@e.st"]).await;
        run(&wt, &["config", "user.name", "tester"]).await;
        wt
    }

    fn fresh_config(store: Arc<MemoryStore>) -> ServerConfig {
        ServerConfig::with_store(store)
    }

    /// Stash a workspace record so `collect_tracked_sessions` picks
    /// up its sessions when the inspector asks the store.
    fn seed_workspace(store: &MemoryStore, worktree_path: PathBuf, stopped: bool) -> SessionId {
        use lazybox_core::{
            SessionKey, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
        };
        let session_key: SessionKey = "github:o/r#1".into();
        let workspace_key: WorkspaceKey = WorkspaceKey::new(session_key.as_str().to_string());
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            },
            title: "test".into(),
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
            updated_at: chrono::Utc::now(),
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
        let mut workspace = Workspace::from_task(task, chrono::Utc::now());
        let mut session = WorkspaceSession::new(
            workspace_key.clone(),
            SessionKind::Shell,
            worktree_path,
            chrono::Utc::now(),
        );
        if stopped {
            session.state = SessionRunState::Stopped;
        }
        let session_id = session.id;
        workspace.sessions.push(session);
        let json = serde_json::to_string(&workspace).unwrap();
        store
            .save_workspace(&WorkspaceRecord {
                key: workspace_key.as_str().into(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(json),
            })
            .unwrap();
        session_id
    }

    async fn drain_until<F>(rx: &mut tokio::sync::broadcast::Receiver<Event>, pred: F) -> Event
    where
        F: Fn(&Event) -> bool,
    {
        loop {
            let evt = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("event timeout")
                .expect("event");
            if pred(&evt) {
                return evt;
            }
        }
    }

    /// Assert NO event matching `pred` is already queued or arrives
    /// within a short grace window. The emit paths under test are
    /// synchronous up to the `broadcast::send`, so anything they
    /// produced is in the channel before this runs.
    async fn assert_no_event<F>(rx: &mut tokio::sync::broadcast::Receiver<Event>, pred: F)
    where
        F: Fn(&Event) -> bool,
    {
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Err(_) => return,
                Ok(Ok(evt)) => assert!(!pred(&evt), "unexpected event: {evt:?}"),
                Ok(Err(_)) => return,
            }
        }
    }

    #[tokio::test]
    async fn workspace_diff_handler_reads_the_persisted_session_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "diff", "feat").await;
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();
        std::fs::write(wt.join("new.txt"), "new\n").unwrap();
        let store = Arc::new(MemoryStore::new());
        let session_id = seed_workspace(&store, wt, false);
        let config = fresh_config(store);
        let mut rx = config.bus.subscribe();
        let key = WorkspaceKey::new("github:o/r#1");
        let session_key: lazybox_core::SessionKey = key.as_str().into();
        config
            .terminal
            .register_terminal(
                TerminalId(7),
                "review-agent".into(),
                session_key.clone(),
                TerminalKind::Agent("codex".into()),
            )
            .await;
        config
            .terminal
            .associate_session(TerminalId(7), session_id)
            .await;
        let other_session = SessionId::new();
        config
            .terminal
            .register_terminal(
                TerminalId(8),
                "other-agent".into(),
                session_key.clone(),
                TerminalKind::Agent("codex".into()),
            )
            .await;
        config
            .terminal
            .associate_session(TerminalId(8), other_session)
            .await;
        config
            .terminal
            .register_terminal(
                TerminalId(9),
                "review-shell".into(),
                session_key,
                TerminalKind::Shell,
            )
            .await;

        handle_inspect_workspace_diff(
            &config,
            key.clone(),
            WorkspaceDiffTarget::Session(session_id),
        )
        .await;

        let event = drain_until(&mut rx, |event| {
            matches!(event, Event::WorkspaceDiffInspected { .. })
        })
        .await;
        let Event::WorkspaceDiffInspected {
            workspace_key,
            target,
            agent_terminal_ids,
            diff: Some(diff),
            error,
        } = event
        else {
            panic!("expected successful workspace diff event");
        };
        assert_eq!(workspace_key, key);
        assert_eq!(target, WorkspaceDiffTarget::Session(session_id));
        assert_eq!(agent_terminal_ids, vec![TerminalId(7)]);
        assert!(error.is_none());
        assert!(diff.files.iter().any(|file| file.path == "README.md"));
        assert!(diff.files.iter().any(|file| file.path == "new.txt"));
    }

    #[tokio::test]
    async fn workspace_diff_handler_reads_a_linked_checkout_without_a_session() {
        let fx = setup_fixture().await;
        let checkout = add_wt(&fx, "linked-diff", "feat").await;
        std::fs::write(checkout.join("linked.txt"), "linked\n").unwrap();
        let store = Arc::new(MemoryStore::new());
        seed_workspace(&store, checkout.clone(), false);
        let key = WorkspaceKey::new("github:o/r#1");
        let mut record = store
            .get_workspace(&key)
            .expect("read workspace")
            .expect("workspace");
        let mut workspace: lazybox_core::Workspace =
            serde_json::from_str(record.workspace_json.as_deref().expect("workspace json"))
                .expect("deserialize workspace");
        workspace.sessions.clear();
        workspace.linked_checkout = Some(checkout);
        record.workspace_json = Some(serde_json::to_string(&workspace).expect("serialize"));
        store
            .save_workspace(&record)
            .expect("save linked workspace");

        let config = fresh_config(store);
        let session_key: lazybox_core::SessionKey = key.as_str().into();
        config
            .terminal
            .register_terminal(
                TerminalId(11),
                "linked-agent".into(),
                session_key,
                TerminalKind::Agent("codex".into()),
            )
            .await;
        let mut rx = config.bus.subscribe();

        handle_inspect_workspace_diff(&config, key.clone(), WorkspaceDiffTarget::LinkedCheckout)
            .await;

        let event = drain_until(&mut rx, |event| {
            matches!(event, Event::WorkspaceDiffInspected { .. })
        })
        .await;
        let Event::WorkspaceDiffInspected {
            workspace_key,
            target,
            agent_terminal_ids,
            diff: Some(diff),
            error,
        } = event
        else {
            panic!("expected successful linked-checkout diff event");
        };
        assert_eq!(workspace_key, key);
        assert_eq!(target, WorkspaceDiffTarget::LinkedCheckout);
        assert_eq!(agent_terminal_ids, vec![TerminalId(11)]);
        assert!(error.is_none());
        assert!(diff.files.iter().any(|file| file.path == "linked.txt"));
    }

    /// Healthy inspector path: one bare clone + one tracked active
    /// session → exactly one inspection row, untagged, with the
    /// session id attached.
    #[tokio::test]
    async fn inspect_emits_tracked_active_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "ok", "feat").await;
        let store = Arc::new(MemoryStore::new());
        seed_workspace(&store, wt.clone(), /*stopped=*/ false);

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        inspect_worktrees_with(&config, &mgr).await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::WorktreesInspected { .. })).await;
        let Event::WorktreesInspected { inspections } = evt else {
            unreachable!("filtered above")
        };
        assert_eq!(inspections.len(), 1);
        assert_eq!(inspections[0].path, wt);
        assert!(
            inspections[0].reasons.is_empty(),
            "active tracked worktree should not be flagged: {:?}",
            inspections[0].reasons
        );
        assert!(inspections[0].session_id.is_some());
    }

    /// Stopped session shows up as `session-stopped` so the modal
    /// can offer to reap the worktree.
    #[tokio::test]
    async fn inspect_flags_stopped_session() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "stopped", "feat").await;
        let store = Arc::new(MemoryStore::new());
        seed_workspace(&store, wt.clone(), /*stopped=*/ true);

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        inspect_worktrees_with(&config, &mgr).await;
        let evt = drain_until(&mut rx, |e| matches!(e, Event::WorktreesInspected { .. })).await;
        let Event::WorktreesInspected { inspections } = evt else {
            unreachable!()
        };
        assert_eq!(inspections.len(), 1);
        assert!(
            inspections[0]
                .reasons
                .iter()
                .any(|r| r == "session-stopped"),
            "expected session-stopped tag, got {:?}",
            inspections[0].reasons,
        );
    }

    /// Untracked worktree (no matching session record) gets the
    /// `untracked` tag and is safe-to-delete.
    #[tokio::test]
    async fn inspect_flags_untracked_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "ghost", "feat").await;
        let store = Arc::new(MemoryStore::new());
        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        inspect_worktrees_with(&config, &mgr).await;
        let evt = drain_until(&mut rx, |e| matches!(e, Event::WorktreesInspected { .. })).await;
        let Event::WorktreesInspected { inspections } = evt else {
            unreachable!()
        };
        let row = inspections
            .iter()
            .find(|r| r.path == wt)
            .expect("ghost row");
        assert!(row.reasons.iter().any(|r| r == "untracked"));
        assert!(row.is_safe_to_delete);
    }

    /// Delete happy path: untracked worktree → safety gate passes →
    /// directory removed → ok=true event on the bus.
    #[tokio::test]
    async fn delete_removes_safe_worktree_and_emits_ok() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "safe", "feat").await;
        let store = Arc::new(MemoryStore::new());
        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        delete_orphaned_worktree_with(&config, &mgr, wt.clone(), /*force=*/ false).await;

        let evt = drain_until(&mut rx, |e| {
            matches!(e, Event::OrphanedWorktreeDeleted { .. })
        })
        .await;
        let Event::OrphanedWorktreeDeleted { path, ok, error } = evt else {
            unreachable!()
        };
        assert_eq!(path, wt);
        assert!(ok, "delete should succeed: {error:?}");
        assert!(!wt.exists(), "worktree dir should be gone");
    }

    /// Delete refusal: uncommitted changes block the non-force path.
    /// Daemon emits ok=false with the reason; the dir stays put so
    /// the user can recover.
    #[tokio::test]
    async fn delete_refuses_dirty_worktree_without_force() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "dirty", "feat").await;
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Arc::new(MemoryStore::new());
        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        delete_orphaned_worktree_with(&config, &mgr, wt.clone(), /*force=*/ false).await;
        let evt = drain_until(&mut rx, |e| {
            matches!(e, Event::OrphanedWorktreeDeleted { .. })
        })
        .await;
        let Event::OrphanedWorktreeDeleted { ok, error, .. } = evt else {
            unreachable!()
        };
        assert!(!ok);
        let msg = error.unwrap_or_default();
        assert!(msg.contains("uncommitted"), "got: {msg}");
        assert!(wt.exists(), "dirty worktree must be preserved");
    }

    #[tokio::test]
    async fn clean_worktrees_preserves_dirty_checkout_and_session_record() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "clean-command-dirty", "feat").await;
        std::fs::write(wt.join("release-wip.txt"), "must survive").unwrap();
        let store = Arc::new(MemoryStore::new());
        seed_workspace(&store, wt.clone(), /*stopped=*/ true);
        let config = fresh_config(store.clone());
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        clean_worktrees_with(&config, &mgr).await;

        let evt = drain_until(&mut rx, |event| {
            matches!(event, Event::CleanWorktreesCompleted { .. })
        })
        .await;
        assert!(matches!(
            evt,
            Event::CleanWorktreesCompleted {
                removed: 0,
                skipped: 1,
            }
        ));
        assert_eq!(
            std::fs::read_to_string(wt.join("release-wip.txt")).unwrap(),
            "must survive"
        );
        let record = store
            .get_workspace(&WorkspaceKey::new("github:o/r#1"))
            .unwrap()
            .expect("workspace row survives");
        let workspace: Workspace =
            serde_json::from_str(record.workspace_json.as_deref().unwrap()).unwrap();
        assert_eq!(workspace.sessions.len(), 1, "session stays recoverable");
    }

    /// Force=true overrides the safety gate even when the worktree
    /// has uncommitted changes. The directory is removed and the
    /// event reports ok=true.
    #[tokio::test]
    async fn delete_force_overrides_safety() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "dirty-force", "feat").await;
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Arc::new(MemoryStore::new());
        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        delete_orphaned_worktree_with(&config, &mgr, wt.clone(), /*force=*/ true).await;
        let evt = drain_until(&mut rx, |e| {
            matches!(e, Event::OrphanedWorktreeDeleted { .. })
        })
        .await;
        let Event::OrphanedWorktreeDeleted { ok, .. } = evt else {
            unreachable!()
        };
        assert!(ok);
        assert!(!wt.exists());
    }

    /// Path not under management → ok=false with a clear message.
    /// Prevents a stale TUI from accidentally removing something
    /// outside `<state_root>/worktrees/`.
    #[tokio::test]
    async fn delete_rejects_unknown_path() {
        let fx = setup_fixture().await;
        let store = Arc::new(MemoryStore::new());
        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        delete_orphaned_worktree_with(
            &config,
            &mgr,
            PathBuf::from("/tmp/this-was-never-a-worktree"),
            false,
        )
        .await;
        let evt = drain_until(&mut rx, |e| {
            matches!(e, Event::OrphanedWorktreeDeleted { .. })
        })
        .await;
        let Event::OrphanedWorktreeDeleted { ok, error, .. } = evt else {
            unreachable!()
        };
        assert!(!ok);
        assert!(
            error
                .unwrap_or_default()
                .contains("no longer under management")
        );
    }

    /// Seed a merged-PR workspace with one shell session rooted at
    /// `wt`, saved under its own `workspace.key` so `load_workspace`
    /// resolves it. Returns `(key, session_id)`.
    fn seed_merged_workspace(
        store: &MemoryStore,
        wt: PathBuf,
        branch: &str,
    ) -> (WorkspaceKey, SessionId) {
        seed_merged_workspace_numbered(store, wt, branch, 1)
    }

    /// Like [`seed_merged_workspace`] but with an explicit PR number,
    /// so a test can hold several merged workspaces at once.
    fn seed_merged_workspace_numbered(
        store: &MemoryStore,
        wt: PathBuf,
        branch: &str,
        number: u64,
    ) -> (WorkspaceKey, SessionId) {
        use lazybox_core::{Task, TaskId, TaskRole, TaskState, Workspace};
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("o/r#{number}"),
            },
            title: "merged pr".into(),
            body: None,
            state: TaskState::Merged,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/o/r/pull/{number}"),
            repo: Some("o/r".into()),
            branch: Some(branch.into()),
            base_branch: None,
            updated_at: chrono::Utc::now(),
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
        let mut workspace = Workspace::from_task(task, chrono::Utc::now());
        let key = workspace.key.clone();
        let session =
            WorkspaceSession::new(key.clone(), SessionKind::Shell, wt, chrono::Utc::now());
        let session_id = session.id;
        workspace.sessions.push(session);
        let json = serde_json::to_string(&workspace).unwrap();
        store
            .save_workspace(&WorkspaceRecord {
                key: key.as_str().into(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(json),
            })
            .unwrap();
        (key, session_id)
    }

    /// Seed a merged-PR workspace with **no sessions** (a tracking row
    /// the user watched but never opened a worktree for). Returns its
    /// key. Exercises the issue #499 session-less cleanup path.
    fn seed_merged_workspace_no_session(store: &MemoryStore, number: u64) -> WorkspaceKey {
        use lazybox_core::{Task, TaskId, TaskRole, TaskState, Workspace};
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("o/r#{number}"),
            },
            title: "merged pr".into(),
            body: None,
            state: TaskState::Merged,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/o/r/pull/{number}"),
            repo: Some("o/r".into()),
            branch: Some(format!("feat-{number}")),
            base_branch: None,
            updated_at: chrono::Utc::now(),
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
        let workspace = Workspace::from_task(task, chrono::Utc::now());
        let key = workspace.key.clone();
        store
            .save_workspace(&WorkspaceRecord {
                key: key.as_str().into(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
            })
            .unwrap();
        key
    }

    /// Build a **closed issue** task (`o/r#{number}`, `TaskKind::Issue`).
    /// Distinct from the merged-PR seeds so the #552 closed-issue paths
    /// are exercised with a genuine issue, not a PR mislabelled `Closed`.
    fn closed_issue_task(number: u64, branch: &str) -> lazybox_core::Task {
        use lazybox_core::{Task, TaskId, TaskKind, TaskRole, TaskState};
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("o/r#{number}"),
            },
            title: "closed issue".into(),
            body: None,
            state: TaskState::Closed,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/o/r/issues/{number}"),
            repo: Some("o/r".into()),
            branch: Some(branch.into()),
            base_branch: None,
            updated_at: chrono::Utc::now(),
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
            kind: Some(TaskKind::Issue),
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    /// Seed a closed-issue workspace with one session rooted at `wt`.
    fn seed_closed_issue_workspace(
        store: &MemoryStore,
        wt: PathBuf,
        branch: &str,
        number: u64,
    ) -> (WorkspaceKey, SessionId) {
        use lazybox_core::Workspace;
        let mut workspace =
            Workspace::from_task(closed_issue_task(number, branch), chrono::Utc::now());
        let key = workspace.key.clone();
        let session =
            WorkspaceSession::new(key.clone(), SessionKind::Shell, wt, chrono::Utc::now());
        let session_id = session.id;
        workspace.sessions.push(session);
        store
            .save_workspace(&WorkspaceRecord {
                key: key.as_str().into(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
            })
            .unwrap();
        (key, session_id)
    }

    /// Seed a closed-issue workspace with **no session** — the bare
    /// tracked row that used to linger as `x x` territory (#552).
    fn seed_closed_issue_no_session(store: &MemoryStore, number: u64) -> WorkspaceKey {
        use lazybox_core::Workspace;
        let workspace = Workspace::from_task(closed_issue_task(number, "feat"), chrono::Utc::now());
        let key = workspace.key.clone();
        store
            .save_workspace(&WorkspaceRecord {
                key: key.as_str().into(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
            })
            .unwrap();
        key
    }

    /// Drop the remote-tracking ref so the inspector sees the merged
    /// branch as auto-deleted upstream (the GitHub-on-merge default),
    /// which is what flips the worktree to `is_safe_to_delete`.
    async fn delete_remote_ref(fx: &Fixture, branch: &str) {
        run(
            &fx.bare,
            &["update-ref", "-d", &format!("refs/remotes/origin/{branch}")],
        )
        .await;
    }

    /// Happy path: merged PR whose branch was auto-deleted upstream →
    /// worktree reaped, session dropped from the stored workspace, and
    /// a `Notification` naming the PR lands on the bus.
    #[tokio::test]
    async fn cleanup_reaps_safe_merged_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "merged", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store.clone());
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        cleanup_merged_worktrees_with(
            &config,
            &mgr,
            &key,
            crate::polling::TerminalCleanup::MergedPr(1),
        )
        .await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::Notification { .. })).await;
        let Event::Notification { body, .. } = evt else {
            unreachable!()
        };
        assert!(body.contains("PR #1"), "got: {body}");
        assert!(!wt.exists(), "merged worktree should be gone");

        // Session record pruned so a restart doesn't resurrect a
        // pointer to a deleted directory.
        let reloaded = load_workspace(&config, &key).expect("workspace");
        assert!(reloaded.sessions.is_empty(), "session should be dropped");
    }

    /// A merged PR worktree with uncommitted work is NOT reaped — the
    /// inspector's safety gate (`is_safe_to_delete = false`) holds, no
    /// session is dropped, and no notification fires.
    #[tokio::test]
    async fn cleanup_preserves_dirty_merged_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "merged-dirty", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        cleanup_merged_worktrees_with(
            &config,
            &mgr,
            &key,
            crate::polling::TerminalCleanup::MergedPr(1),
        )
        .await;

        assert!(wt.exists(), "dirty worktree must be preserved");
        let reloaded = load_workspace(&config, &key).expect("workspace");
        assert_eq!(reloaded.sessions.len(), 1, "session must be retained");
    }

    /// Hardening regression: a session whose stored `worktree_path`
    /// reaches the worktree through a symlinked parent (the inspector
    /// reports the resolved real path) must still be matched and
    /// reaped. A naive `path == path` comparison would silently skip
    /// it; the canonicalized lookup catches it.
    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_matches_session_path_through_symlink() {
        let fx = setup_fixture().await;
        let real = add_wt(&fx, "canon", "feat").await;
        delete_remote_ref(&fx, "feat").await;

        // `<base>/worktrees-link` → `<base>/worktrees`, so the stored
        // path resolves to the same worktree by a different spelling.
        let link = fx.base.path().join("worktrees-link");
        std::os::unix::fs::symlink(fx.base.path().join("worktrees"), &link).unwrap();
        let symlinked = link.join("canon");

        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, symlinked, "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        cleanup_merged_worktrees_with(
            &config,
            &mgr,
            &key,
            crate::polling::TerminalCleanup::MergedPr(1),
        )
        .await;

        assert!(
            !real.exists(),
            "worktree reached via symlink should be reaped"
        );
        let reloaded = load_workspace(&config, &key).expect("workspace");
        assert!(reloaded.sessions.is_empty(), "session should be dropped");
    }

    /// Stopgap for recovered terminals (post-daemon-restart): they
    /// populate `terminal_meta` but never `terminal_sessions`, so the
    /// session-id liveness check alone would reap a worktree under a
    /// live recovered agent. The workspace-key union must keep it.
    #[tokio::test]
    async fn cleanup_skips_recovered_terminal_via_terminal_meta() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "merged-recovered", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        // Recovered terminal: terminal_meta only, no terminal_sessions.
        let session_key: lazybox_core::SessionKey = (&key).into();
        config
            .terminal
            .insert_meta(
                lazybox_ipc::TerminalId(1),
                session_key,
                lazybox_ipc::TerminalKind::Shell,
            )
            .await;
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        cleanup_merged_worktrees_with(
            &config,
            &mgr,
            &key,
            crate::polling::TerminalCleanup::MergedPr(1),
        )
        .await;

        assert!(
            wt.exists(),
            "recovered terminal's worktree must be preserved"
        );
        let reloaded = load_workspace(&config, &key).expect("workspace");
        assert_eq!(reloaded.sessions.len(), 1, "session must be retained");
    }

    /// Rescope-delete pre-pass: a safe (clean, remote branch gone)
    /// worktree with no live terminal is reaped so the dir doesn't
    /// leak once the workspace row disappears.
    #[tokio::test]
    async fn reap_removes_safe_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "reap-safe", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let workspace = load_workspace(&config, &key).expect("workspace");

        reap_safe_workspace_worktrees_with(&config, &mgr, &workspace).await;

        assert!(!wt.exists(), "safe worktree should be reaped");
    }

    /// The reap NEVER force-removes: a dirty worktree survives even
    /// though its workspace row is about to be rescope-deleted — the
    /// user's uncommitted work stays on disk for manual recovery.
    #[tokio::test]
    async fn reap_preserves_dirty_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "reap-dirty", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let workspace = load_workspace(&config, &key).expect("workspace");

        reap_safe_workspace_worktrees_with(&config, &mgr, &workspace).await;

        assert!(wt.exists(), "dirty worktree must never be force-removed");
    }

    /// A live terminal (recovered-terminal shape: terminal_meta only)
    /// pins the worktree even when the tree itself is reap-safe.
    #[tokio::test]
    async fn reap_skips_worktree_with_live_terminal() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "reap-live", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let session_key: lazybox_core::SessionKey = (&key).into();
        config
            .terminal
            .insert_meta(
                lazybox_ipc::TerminalId(2),
                session_key,
                lazybox_ipc::TerminalKind::Shell,
            )
            .await;
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let workspace = load_workspace(&config, &key).expect("workspace");

        reap_safe_workspace_worktrees_with(&config, &mgr, &workspace).await;

        assert!(wt.exists(), "live terminal's worktree must be preserved");
    }

    /// A session with a live terminal attached is skipped even when
    /// its tree is clean — we never pull a folder out from under an
    /// agent the user is actively using.
    #[tokio::test]
    async fn cleanup_skips_live_session() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "merged-live", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        // Pretend a terminal is attached to this session.
        config
            .terminal
            .associate_session(lazybox_ipc::TerminalId(1), sid)
            .await;
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        cleanup_merged_worktrees_with(
            &config,
            &mgr,
            &key,
            crate::polling::TerminalCleanup::MergedPr(1),
        )
        .await;

        assert!(wt.exists(), "live session's worktree must be preserved");
        let reloaded = load_workspace(&config, &key).expect("workspace");
        assert_eq!(reloaded.sessions.len(), 1, "live session must be retained");
    }

    /// Default (no auto-cleanup) path: a clean merged worktree emits
    /// `MergedPrRemovable` with `has_local_work = false` so the modal
    /// asks without a data-loss warning — and nothing is deleted yet.
    #[tokio::test]
    async fn prompt_emits_removable_for_clean_merged_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "clean", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Merged,
        )
        .await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
        let Event::MergedPrRemovable {
            label,
            has_local_work,
            active_terminal_count,
            ..
        } = evt
        else {
            unreachable!()
        };
        assert_eq!(label, "o/r#1");
        assert!(!has_local_work, "clean worktree must not warn");
        assert_eq!(active_terminal_count, 0);
        assert!(wt.exists(), "prompt must not delete anything");
    }

    /// A merged worktree with uncommitted work flags
    /// `has_local_work = true` so the confirm modal warns before the
    /// force-delete.
    #[tokio::test]
    async fn prompt_flags_local_work_for_dirty_merged_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "dirty", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Merged,
        )
        .await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
        let Event::MergedPrRemovable { has_local_work, .. } = evt else {
            unreachable!()
        };
        assert!(has_local_work, "dirty worktree must warn before delete");
    }

    /// A closed **issue** whose worktree has local work emits the same
    /// `MergedPrRemovable` prompt as a merged PR, but tags
    /// `terminal_state = Closed` so the modal copy reads "closed" (#250).
    /// The worktree is dirtied so the assertion is meaningful even under
    /// the pre-#1129 clean-auto-remove behavior.
    #[tokio::test]
    async fn prompt_emits_closed_terminal_state_for_issue() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "issue", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Closed,
        )
        .await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
        let Event::MergedPrRemovable { terminal_state, .. } = evt else {
            unreachable!()
        };
        assert_eq!(terminal_state, lazybox_ipc::RemovableTerminalState::Closed);
        assert!(wt.exists(), "prompt must not delete anything");
    }

    /// #552: a session-less closed issue auto-removes immediately (no
    /// worktree to reap, nothing to lose) instead of prompting or
    /// lingering as an `x x` row. The row is gone and no
    /// `MergedPrRemovable` is emitted.
    #[tokio::test]
    async fn closed_issue_without_session_auto_removes() {
        let store = Arc::new(MemoryStore::new());
        let key = seed_closed_issue_no_session(&store, 7);

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        );
        let mut rx = config.bus.subscribe();

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Closed,
        )
        .await;

        assert!(
            load_workspace(&config, &key).is_none(),
            "session-less closed issue must be removed immediately"
        );
        let removed = drain_until(&mut rx, |e| matches!(e, Event::WorkspaceRemoved(_)));
        let Event::WorkspaceRemoved(k) = removed.await else {
            unreachable!()
        };
        assert_eq!(k, key);
    }

    /// #1129: a closed issue whose worktree is clean and idle must NOT be
    /// silently reaped — reclaiming the worktree and killing its agent
    /// without consent contradicts `auto_cleanup_merged: false`. It
    /// prompts (`MergedPrRemovable`, `has_local_work: false`) and leaves
    /// the worktree + row intact until the user answers. (Only a
    /// session-less closed issue still auto-removes — see
    /// `closed_issue_without_session_auto_removes`.)
    #[tokio::test]
    async fn closed_issue_clean_session_prompts_not_reaped() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "issue-clean", "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_closed_issue_workspace(&store, wt.clone(), "feat", 8);

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Closed,
        )
        .await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
        let Event::MergedPrRemovable {
            has_local_work,
            terminal_state,
            ..
        } = evt
        else {
            unreachable!()
        };
        assert!(
            !has_local_work,
            "a clean worktree has no local work but must still prompt"
        );
        assert_eq!(terminal_state, lazybox_ipc::RemovableTerminalState::Closed);
        assert!(wt.exists(), "clean worktree must survive without consent");
        assert!(
            load_workspace(&config, &key).is_some(),
            "row must remain until the user answers the prompt"
        );
    }

    /// #552: the auto-remove must NOT archive the key — unlike a
    /// user-confirmed `x x`, an automatic close-cleanup should let a
    /// later reopen on GitHub re-create the workspace, so the row is
    /// dropped without being recorded in the archived set.
    #[tokio::test]
    async fn closed_issue_auto_remove_does_not_archive() {
        let store = Arc::new(MemoryStore::new());
        let key = seed_closed_issue_no_session(&store, 13);

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        );

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Closed,
        )
        .await;

        assert!(load_workspace(&config, &key).is_none(), "row must be gone");
        assert!(
            !crate::workspace::load_archived_set(&config).contains(key.as_str()),
            "auto-remove must not archive — a reopen should resurface it"
        );
    }

    /// #552: a closed issue that is clean but has a live terminal
    /// attached must NOT be yanked from under the user — it prompts
    /// (reporting the live terminal) instead of auto-removing.
    #[tokio::test]
    async fn closed_issue_clean_session_with_live_terminal_prompts() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "issue-live", "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_closed_issue_workspace(&store, wt.clone(), "feat", 12);

        let config = fresh_config(store);
        let session_key: lazybox_core::SessionKey = (&key).into();
        config
            .terminal
            .insert_meta(
                lazybox_ipc::TerminalId(1),
                session_key,
                lazybox_ipc::TerminalKind::Shell,
            )
            .await;
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Closed,
        )
        .await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
        let Event::MergedPrRemovable {
            active_terminal_count,
            ..
        } = evt
        else {
            unreachable!()
        };
        assert_eq!(
            active_terminal_count, 1,
            "the live terminal must be reported"
        );
        assert!(wt.exists(), "a live-terminal worktree must survive");
        assert!(load_workspace(&config, &key).is_some());
    }

    /// #552: a closed issue whose worktree has uncommitted work is NOT
    /// destroyed — it prompts (`MergedPrRemovable`, `has_local_work`)
    /// and leaves the worktree + row intact until the user answers.
    #[tokio::test]
    async fn closed_issue_dirty_session_prompts() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "issue-dirty", "feat").await;
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_closed_issue_workspace(&store, wt.clone(), "feat", 9);

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Closed,
        )
        .await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
        let Event::MergedPrRemovable {
            has_local_work,
            terminal_state,
            ..
        } = evt
        else {
            unreachable!()
        };
        assert!(has_local_work, "dirty worktree must warn, not auto-remove");
        assert_eq!(terminal_state, lazybox_ipc::RemovableTerminalState::Closed);
        assert!(wt.exists(), "dirty worktree must survive the prompt");
        assert!(
            load_workspace(&config, &key).is_some(),
            "row must remain until the user answers"
        );
    }

    /// #552 + #499: a durable "keep" answer pins even a session-less
    /// closed issue — the auto-remove is suppressed and the row stays.
    #[tokio::test]
    async fn closed_issue_declined_stays_declined() {
        let store = Arc::new(MemoryStore::new());
        let key = seed_closed_issue_no_session(&store, 10);
        {
            let mut ws = load_workspace(&fresh_config(store.clone()), &key).unwrap();
            ws.cleanup_prompt = lazybox_core::CleanupPrompt::Declined;
            store
                .save_workspace(&WorkspaceRecord {
                    key: key.as_str().into(),
                    created_at: chrono::Utc::now(),
                    workspace_json: Some(serde_json::to_string(&ws).unwrap()),
                })
                .unwrap();
        }

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(
            tempfile::tempdir().unwrap().path().to_path_buf(),
        );
        let mut rx = config.bus.subscribe();

        prompt_merged_pr_removal_with(
            &config,
            &mgr,
            &key,
            lazybox_ipc::RemovableTerminalState::Closed,
        )
        .await;

        assert!(
            load_workspace(&config, &key).is_some(),
            "a declined closed issue must not be auto-removed"
        );
        assert_no_event(&mut rx, |e| {
            matches!(
                e,
                Event::WorkspaceRemoved(_) | Event::MergedPrRemovable { .. }
            )
        })
        .await;
    }

    /// #552: cancelling a pending removal drops the reprompt throttle
    /// stamp (so a re-close prompts cleanly) and broadcasts
    /// `RemovalCancelled` so the TUI dismisses a mounted modal.
    #[tokio::test]
    async fn cancel_pending_removal_clears_memory_and_broadcasts() {
        let store = Arc::new(MemoryStore::new());
        let key = seed_closed_issue_no_session(&store, 11);
        let config = fresh_config(store);
        config
            .poll
            .removal_prompts
            .lock()
            .await
            .prompted
            .insert(key.as_str().to_string(), std::time::Instant::now());
        let mut rx = config.bus.subscribe();

        cancel_pending_removal(&config, &key).await;

        assert!(
            !config
                .poll
                .removal_prompts
                .lock()
                .await
                .prompted
                .contains_key(key.as_str()),
            "reprompt stamp must be dropped"
        );
        let evt = drain_until(&mut rx, |e| matches!(e, Event::RemovalCancelled { .. })).await;
        let Event::RemovalCancelled { workspace_key } = evt else {
            unreachable!()
        };
        assert_eq!(workspace_key, key);
    }

    /// Level-trigger sweep (#292): an unresolved merged workspace is
    /// re-prompted by `reprompt_unresolved_removals_with`, an immediate
    /// second sweep is throttled by the shared memory, and once the
    /// stamp ages past `REMOVAL_REPROMPT_AFTER` the sweep fires again.
    #[tokio::test]
    async fn reprompt_reemits_unresolved_removal_and_throttles() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "reprompt", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;
        let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
        let Event::MergedPrRemovable { label, .. } = evt else {
            unreachable!()
        };
        assert_eq!(label, "o/r#1");

        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;
        assert_no_event(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;

        {
            // Backdate the stamp past the interval. `checked_sub` can
            // return None on a host whose uptime is under the interval
            // (fresh CI VM) — dropping the stamp exercises the same
            // "no fresh emit on record" outcome.
            let mut prompts = config.poll.removal_prompts.lock().await;
            match std::time::Instant::now().checked_sub(crate::polling::REMOVAL_REPROMPT_AFTER) {
                Some(past) => {
                    prompts.prompted.insert(key.as_str().to_string(), past);
                }
                None => {
                    prompts.prompted.remove(key.as_str());
                }
            }
        }
        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;
        drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
    }

    /// An explicit "keep" answer (`Command::KeepMergedWorkspace`) pins
    /// the workspace: the sweep stays quiet even after the throttle
    /// window would have expired.
    #[tokio::test]
    async fn reprompt_skips_workspace_after_keep() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "kept", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        crate::polling::keep_merged_workspace(&config, &key).await;
        let mut rx = config.bus.subscribe();

        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;
        assert_no_event(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
    }

    /// Regression for #292: a client that subscribes AFTER the one
    /// transition broadcast (dropped on bus lag, or the client wasn't
    /// connected yet) still gets prompted — the Subscribe path resets
    /// the throttle via `mark_removal_prompts_for_replay` and the next
    /// sweep re-emits immediately.
    #[tokio::test]
    async fn reconnect_gets_reprompted_for_unresolved_removal() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "reconnect", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        seed_merged_workspace(&store, wt.clone(), "feat");

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        // First emit happens with nobody subscribed — lost for good.
        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;

        // A client connects: replay reset + the tick it wakes re-sweeps.
        let mut rx = config.bus.subscribe();
        crate::polling::mark_removal_prompts_for_replay(&config).await;
        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;
        drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
    }

    /// Regression for #292: the prompt memory is per-process, so a
    /// daemon restarted after persisting the merged state re-offers
    /// cleanup on its first sweep instead of staying silent forever.
    #[tokio::test]
    async fn fresh_daemon_reprompts_unresolved_removal() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "restart", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        seed_merged_workspace(&store, wt.clone(), "feat");
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        // First daemon session prompts, then "restarts" unanswered.
        let config = fresh_config(store.clone());
        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;

        let restarted = fresh_config(store);
        let mut rx = restarted.bus.subscribe();
        crate::polling::reprompt_unresolved_removals_with(&restarted, &mgr).await;
        drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
    }

    /// Regression for #292: two workspaces merged at once → the sweep
    /// prompts BOTH, one `MergedPrRemovable` per workspace.
    #[tokio::test]
    async fn sweep_prompts_each_merged_workspace() {
        let fx = setup_fixture().await;
        let wt1 = add_wt(&fx, "two-a", "feat-a").await;
        let wt2 = add_wt(&fx, "two-b", "feat-b").await;
        delete_remote_ref(&fx, "feat-a").await;
        delete_remote_ref(&fx, "feat-b").await;
        let store = Arc::new(MemoryStore::new());
        seed_merged_workspace_numbered(&store, wt1, "feat-a", 1);
        seed_merged_workspace_numbered(&store, wt2, "feat-b", 2);

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;

        let mut labels = std::collections::HashSet::new();
        for _ in 0..2 {
            let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
            let Event::MergedPrRemovable { label, .. } = evt else {
                unreachable!()
            };
            labels.insert(label);
        }
        assert_eq!(
            labels,
            ["o/r#1".to_string(), "o/r#2".to_string()].into(),
            "each merged workspace must get its own prompt"
        );
    }

    /// Issue #499: a merged PR the user tracked but never opened a
    /// worktree for (no sessions) is still offered for cleanup by the
    /// sweep — removal just drops the row, but a merged PR shouldn't
    /// linger unprompted. The prompt reports no worktree/terminal.
    #[tokio::test]
    async fn sweep_prompts_session_less_merged_pr() {
        let fx = setup_fixture().await;
        let store = Arc::new(MemoryStore::new());
        let key = seed_merged_workspace_no_session(&store, 7);

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;

        let evt = drain_until(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
        let Event::MergedPrRemovable {
            workspace_key,
            has_local_work,
            active_terminal_count,
            ..
        } = evt
        else {
            unreachable!()
        };
        assert_eq!(workspace_key, key);
        assert!(!has_local_work, "no worktree means no local work to warn");
        assert_eq!(active_terminal_count, 0, "no sessions means no terminals");
    }

    /// Issue #552: a session-less *closed issue* the transition missed
    /// is auto-removed by the level-trigger sweep — the row is dropped
    /// (nothing to reap) rather than prompted or left as `x x` territory.
    #[tokio::test]
    async fn sweep_auto_removes_session_less_closed_issue() {
        use lazybox_core::{Task, TaskId, TaskRole, TaskState, Workspace};
        let fx = setup_fixture().await;
        let store = Arc::new(MemoryStore::new());
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: "o/r#9".into(),
            },
            title: "closed issue".into(),
            body: None,
            state: TaskState::Closed,
            role: TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/issues/9".into(),
            repo: Some("o/r".into()),
            branch: None,
            base_branch: None,
            updated_at: chrono::Utc::now(),
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
        };
        let workspace = Workspace::from_task(task, chrono::Utc::now());
        let key = workspace.key.clone();
        store
            .save_workspace(&WorkspaceRecord {
                key: workspace.key.as_str().into(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
            })
            .unwrap();

        let config = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        crate::polling::reprompt_unresolved_removals_with(&config, &mgr).await;
        assert!(
            load_workspace(&config, &key).is_none(),
            "the bare closed-issue row must be auto-removed"
        );
        let removed = drain_until(&mut rx, |e| matches!(e, Event::WorkspaceRemoved(_))).await;
        assert!(matches!(removed, Event::WorkspaceRemoved(k) if k == key));
    }

    /// Issue #499: "keep" persists [`lazybox_core::CleanupPrompt::Declined`] on the
    /// stored row, so a *restarted* daemon (fresh in-memory prompt
    /// memory) never re-offers cleanup — unlike the old per-process pin
    /// that a restart cleared.
    #[tokio::test]
    async fn keep_persists_decline_across_restart() {
        let fx = setup_fixture().await;
        let store = Arc::new(MemoryStore::new());
        let key = seed_merged_workspace_no_session(&store, 3);

        let config = fresh_config(store.clone());
        crate::polling::keep_merged_workspace(&config, &key).await;

        // The decision is durable in the store, not just in memory.
        let persisted = load_workspace(&config, &key).expect("workspace");
        assert_eq!(
            persisted.cleanup_prompt,
            lazybox_core::CleanupPrompt::Declined,
            "keep must persist Declined on the workspace"
        );

        // A "restarted" daemon with empty prompt memory stays quiet.
        let restarted = fresh_config(store);
        let mgr = lazybox_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = restarted.bus.subscribe();
        crate::polling::reprompt_unresolved_removals_with(&restarted, &mgr).await;
        assert_no_event(&mut rx, |e| matches!(e, Event::MergedPrRemovable { .. })).await;
    }

    /// On confirm, `remove_merged_workspace_with` deletes the worktree
    /// AND drops the row (the worktree deletion `delete_workspace`
    /// alone skips), broadcasting `WorkspaceRemoved`.
    #[tokio::test]
    async fn remove_merged_deletes_worktree_and_row() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "remove", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = ServerConfig::with_store_backend_and_worktree_root(
            store,
            Arc::new(crate::backend::MockBackend::new()),
            fx.base.path().to_path_buf(),
        );
        let mut rx = config.bus.subscribe();

        remove_merged_workspace_with(
            &config,
            &key,
            crate::workspace::WorkspaceRemovalReason::MergedConfirmed,
        )
        .await;

        drain_until(&mut rx, |e| matches!(e, Event::WorkspaceRemoved(_))).await;
        assert!(!wt.exists(), "merged worktree should be deleted");
        assert!(
            load_workspace(&config, &key).is_none(),
            "row should be removed"
        );
    }

    /// #476: deleting a workspace with no backing terminals broadcasts
    /// `WorkspaceRemoved` and records the archive tombstone. The reorder
    /// (emit the echo before terminal teardown, so the row's
    /// disappearance isn't gated on killing terminals) must not regress
    /// the happy path.
    #[tokio::test]
    async fn delete_workspace_emits_removed_and_archives() {
        let store = Arc::new(MemoryStore::new());
        let wt = std::env::temp_dir().join(format!("lb-del-{}", std::process::id()));
        seed_workspace(&store, wt, /*stopped=*/ true);
        let config = fresh_config(store);
        let mut rx = config.bus.subscribe();
        let key = lazybox_core::WorkspaceKey::new("github:o/r#1".to_string());

        assert!(
            crate::workspace::delete_workspace(&config, &key)
                .await
                .is_some()
        );
        drain_until(&mut rx, |e| matches!(e, Event::WorkspaceRemoved(_))).await;
        assert!(load_workspace(&config, &key).is_none(), "store row removed");
        assert!(
            crate::workspace::load_archived_set(&config).contains(key.as_str()),
            "archive tombstone recorded so the next poll won't resurrect it",
        );
    }

    /// #575: deleting a workspace must reclaim its session worktree
    /// directories on disk, not just drop the store row — the leak that
    /// filled disks with multi-GB orphaned worktrees. The reclaimed total
    /// is returned so the caller can announce the freed space.
    #[tokio::test]
    async fn delete_workspace_reclaims_worktree_dir() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "delete-reclaim", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        let store = Arc::new(MemoryStore::new());
        seed_workspace(&store, wt.clone(), /*stopped=*/ true);
        let config = ServerConfig::with_store_backend_and_worktree_root(
            store,
            Arc::new(crate::backend::MockBackend::new()),
            fx.base.path().to_path_buf(),
        );
        let key = lazybox_core::WorkspaceKey::new("github:o/r#1".to_string());

        let reclaimed = crate::workspace::delete_workspace(&config, &key)
            .await
            .expect("delete should succeed");

        assert!(
            !wt.exists(),
            "worktree directory must be reclaimed on delete"
        );
        assert_eq!(reclaimed.worktrees, 1, "one worktree reclaimed");
        assert!(
            reclaimed.bytes > 0,
            "reclaimed byte total: {}",
            reclaimed.bytes
        );
        let body = crate::workspace::reclaimed_notice_body(reclaimed)
            .expect("non-empty reclaim produces a notice body");
        assert!(
            body.contains("reclaimed") && body.contains("worktree"),
            "notice should report reclaimed space: {body}"
        );
    }

    /// The reclaimed-space notice: nothing reclaimed → no notice; an
    /// empty worktree drops the "0 B" figure (#575 review); a non-empty
    /// reclaim reports the size; the count pluralizes.
    #[test]
    fn reclaimed_notice_body_formats() {
        use crate::workspace::{Reclaimed, reclaimed_notice_body};
        assert_eq!(reclaimed_notice_body(Reclaimed::default()), None);
        assert_eq!(
            reclaimed_notice_body(Reclaimed {
                worktrees: 1,
                bytes: 0
            }),
            Some("removed 1 worktree".to_string()),
        );
        assert_eq!(
            reclaimed_notice_body(Reclaimed {
                worktrees: 3,
                bytes: 2 * 1024 * 1024,
            }),
            Some("reclaimed 2.0 MB from 3 worktrees".to_string()),
        );
    }

    /// A confirmation is intent, not a stale cleanliness capability: work
    /// added after the modal opened must still survive the server-side gate.
    #[tokio::test]
    async fn remove_merged_reinspects_and_preserves_dirty_worktree() {
        let fx = setup_fixture().await;
        let wt = add_wt(&fx, "remove-dirty", "feat").await;
        delete_remote_ref(&fx, "feat").await;
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Arc::new(MemoryStore::new());
        let (key, _sid) = seed_merged_workspace(&store, wt.clone(), "feat");

        let config = ServerConfig::with_store_backend_and_worktree_root(
            store,
            Arc::new(crate::backend::MockBackend::new()),
            fx.base.path().to_path_buf(),
        );

        let removed = remove_merged_workspace_with(
            &config,
            &key,
            crate::workspace::WorkspaceRemovalReason::MergedConfirmed,
        )
        .await;

        assert!(removed.is_none(), "fresh dirty state must refuse removal");
        assert!(wt.exists(), "dirty worktree must be preserved");
        assert!(
            load_workspace(&config, &key).is_some(),
            "row must remain reachable for retry"
        );
    }

    /// #573: the merge handler routes a successful merge straight into
    /// the cleanup path, keyed on the PR's number, instead of waiting for
    /// a poll to rediscover the open→merged flip. The decision function
    /// yields `MergedPr(n)` for a PR workspace.
    #[test]
    fn merged_pr_cleanup_targets_the_pr_number() {
        let store = Arc::new(MemoryStore::new());
        let wt = std::env::temp_dir().join("lb-573-merge");
        let (key, _sid) = seed_merged_workspace_numbered(&store, wt, "feat", 42);
        let config = fresh_config(store);
        let ws = load_workspace(&config, &key).expect("workspace");

        match merged_pr_cleanup(&ws) {
            Some(crate::polling::TerminalCleanup::MergedPr(n)) => assert_eq!(n, 42),
            other => panic!("expected MergedPr(42), got {other:?}"),
        }
    }

    /// A workspace with no PR (a plain issue row) never fires the
    /// merged-PR cleanup — the merge handler only reaches this after a
    /// PR merge, but the guard keeps it total.
    #[test]
    fn merged_pr_cleanup_none_without_pr() {
        let store = Arc::new(MemoryStore::new());
        let wt = std::env::temp_dir().join("lb-573-noprmerge");
        let (key, _sid) = seed_closed_issue_workspace(&store, wt, "feat", 9);
        let config = fresh_config(store);
        let ws = load_workspace(&config, &key).expect("workspace");

        assert!(merged_pr_cleanup(&ws).is_none());
    }

    /// #573: the close handler routes a successful close straight into
    /// the cleanup path, keyed on the issue's number, when no PR claims
    /// the issue. The decision function yields `ClosedIssue(n)`.
    #[test]
    fn closed_issue_cleanup_targets_the_issue_number() {
        let store = Arc::new(MemoryStore::new());
        let wt = std::env::temp_dir().join("lb-573-close");
        let (key, _sid) = seed_closed_issue_workspace(&store, wt, "feat", 17);
        let config = fresh_config(store);
        let ws = load_workspace(&config, &key).expect("workspace");

        match closed_issue_cleanup(&config, &ws) {
            Some(crate::polling::TerminalCleanup::ClosedIssue(n)) => assert_eq!(n, 17),
            other => panic!("expected ClosedIssue(17), got {other:?}"),
        }
    }

    /// #573: when a PR claims the issue (`Closes #N`), the close handler
    /// defers — that PR's own merge prompt owns the cleanup after the
    /// issue row collapses into it, exactly as the upsert path defers.
    /// Firing here too would surface a second, stale prompt.
    #[test]
    fn closed_issue_cleanup_defers_when_pr_claims_it() {
        let store = Arc::new(MemoryStore::new());
        let wt = std::env::temp_dir().join("lb-573-claimed");
        let (issue_key, _sid) = seed_closed_issue_workspace(&store, wt, "feat", 21);

        // A separate PR workspace whose PR closes issue #21.
        let issue_id = lazybox_core::TaskId {
            source: "github".into(),
            key: "o/r#21".into(),
        };
        let pr_wt = std::env::temp_dir().join("lb-573-claiming-pr");
        let (pr_key, _pr_sid) = seed_merged_workspace_numbered(&store, pr_wt, "pr-feat", 22);
        let pr_record = store.get_workspace(&pr_key).unwrap().unwrap();
        let mut pr_ws: Workspace =
            serde_json::from_str(pr_record.workspace_json.as_ref().unwrap()).unwrap();
        pr_ws.pr.as_mut().unwrap().closes_issues.push(issue_id);
        store
            .save_workspace(&WorkspaceRecord {
                key: pr_key.as_str().into(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(serde_json::to_string(&pr_ws).unwrap()),
            })
            .unwrap();

        let config = fresh_config(store);
        let issue_ws = load_workspace(&config, &issue_key).expect("issue workspace");

        assert!(
            closed_issue_cleanup(&config, &issue_ws).is_none(),
            "a claimed issue must defer to the PR's cleanup prompt"
        );
    }
}

#[cfg(test)]
mod fetch_repo_labels_tests {
    //! `handle_fetch_repo_labels` must never fail silently: the client
    //! is waiting on a reply to mount its label picker, so every
    //! failure path broadcasts a `ProviderError` with the
    //! `"repo-labels"` source the client's fallback keys on.
    use super::*;
    use lazybox_ipc::Event;
    use lazybox_store::MemoryStore;
    use std::sync::Arc;

    #[tokio::test]
    async fn fetch_failure_broadcasts_repo_labels_provider_error() {
        let config = crate::ServerConfig::with_store(Arc::new(MemoryStore::new()));
        let mut rx = config.bus.subscribe();

        // Unknown workspace — the cheapest hermetic failure path
        // (returns before any provider/network is touched).
        handle_fetch_repo_labels(&config, WorkspaceKey::new("github-o-r-404")).await;

        let evt = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("event timeout")
            .expect("event");
        match evt {
            Event::ProviderError {
                source,
                message,
                kind,
                ..
            } => {
                assert_eq!(source, "repo-labels", "client fallback keys on this source");
                assert!(message.contains("not found"), "got {message:?}");
                assert_eq!(kind, "retryable");
            }
            other => panic!("expected a repo-labels ProviderError, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod mutation_retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The #804 core fix: a secondary-rate-limited mutation is queued and
    /// retried against the reset window, succeeding without a manual
    /// re-press — and a live, non-scary "queued — retrying in …" notice
    /// shows in place of a red `×`.
    #[tokio::test(start_paused = true)]
    async fn rate_limited_mutation_retries_then_succeeds() {
        let (bus, mut rx) = tokio::sync::broadcast::channel(16);
        let calls = AtomicU32::new(0);
        let result = run_mutation_with_retry(&bus, "merge", || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(lazybox_core::ProviderError::retryable_after(
                        "github",
                        "You have exceeded a secondary rate limit",
                        1,
                    ))
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert!(result.is_ok(), "queued retry must succeed: {result:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one failure, then one retry"
        );

        let ev = rx.try_recv().expect("a queued notice must be broadcast");
        match ev {
            Event::ProviderError {
                source,
                message,
                kind,
                ..
            } => {
                assert_eq!(source, "merge");
                assert!(
                    message.contains("queued"),
                    "live status names the queue: {message}"
                );
                // Relative duration, not an absolute clock time — so the
                // notice reads correctly regardless of the daemon's timezone.
                assert!(
                    message.contains("retrying in"),
                    "status shows a relative retry time: {message}"
                );
                assert!(
                    !message.contains(" at "),
                    "no absolute (timezone-dependent) clock time: {message}"
                );
                assert!(!message.contains('{'), "status is JSON-free: {message}");
                assert_eq!(
                    kind, "retryable",
                    "a queued notice must be non-scary / auto-fading"
                );
            }
            other => panic!("expected a queued ProviderError notice, got {other:?}"),
        }
    }

    /// A rate-limit whose reset window is longer than the whole queue budget
    /// (a *primary* quota exhaustion, resets ~an hour out) must surface at
    /// once — never spin ~10 min of doomed retries — and name the window so
    /// "try again" has a concrete when.
    #[tokio::test(start_paused = true)]
    async fn long_reset_window_surfaces_immediately_without_spinning() {
        let (bus, mut rx) = tokio::sync::broadcast::channel(16);
        let calls = AtomicU32::new(0);
        let result = run_mutation_with_retry(&bus, "merge", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(lazybox_core::ProviderError::retryable_after(
                    "github",
                    "API rate limit exceeded",
                    4_000, // > MUTATION_MAX_WAIT_SECS
                ))
            }
        })
        .await;

        let err = result.expect_err("a window we won't hold must surface");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no queuing/spinning on a window longer than the budget"
        );
        assert!(
            rx.try_recv().is_err(),
            "no 'queued, retrying' notice we can't honor"
        );
        let reason = humanize_mutation_failure("merge", &err);
        assert!(
            reason.contains("try again in about"),
            "names the reset window: {reason}"
        );
        assert!(!reason.contains('{'), "JSON-free: {reason}");
    }

    #[test]
    fn humanize_wait_is_relative_and_compact() {
        assert_eq!(humanize_wait(45), "45s");
        assert_eq!(humanize_wait(60), "60s");
        assert_eq!(humanize_wait(120), "~2m");
        assert_eq!(humanize_wait(121), "~3m");
    }

    /// A genuine rejection (conflict, blocked checks) is not a rate limit:
    /// it must surface at once, with no retry and no queued notice, and its
    /// footer reason stays a clean human sentence.
    #[tokio::test(start_paused = true)]
    async fn non_retryable_mutation_fails_immediately() {
        let (bus, mut rx) = tokio::sync::broadcast::channel(16);
        let calls = AtomicU32::new(0);
        let result = run_mutation_with_retry(&bus, "merge", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(lazybox_core::ProviderError::permanent(
                    "github",
                    "GraphQL error: Pull request is not mergeable",
                ))
            }
        })
        .await;

        let err = result.expect_err("a real rejection must fail");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no retry on a permanent error"
        );
        assert!(
            rx.try_recv().is_err(),
            "no queued notice for a permanent error"
        );

        let reason = humanize_mutation_failure("merge", &err);
        assert!(
            reason.contains("Pull request is not mergeable"),
            "got {reason}"
        );
        assert!(
            !reason.contains('{'),
            "footer reason stays JSON-free: {reason}"
        );
    }

    /// The headline case (#927): GitHub's "N of N required status checks
    /// are expected" branch-protection rejection must not leak raw — it's
    /// a "CI hasn't reported yet" state, so translate it into plain,
    /// actionable guidance instead of the GraphQL string.
    #[test]
    fn required_status_checks_expected_is_humanized() {
        let err = lazybox_core::ProviderError::permanent(
            "github",
            "GraphQL error: 10 of 10 required status checks are expected.",
        );
        let reason = humanize_mutation_failure("merge", &err);

        assert!(
            !reason.contains("status checks are expected"),
            "raw GraphQL string leaked: {reason}"
        );
        assert!(reason.contains("Can't merge yet"), "got {reason}");
        assert!(
            reason.contains("10 status checks"),
            "names the required count: {reason}"
        );
        assert!(
            reason.contains("Wait for CI"),
            "suggests the next step: {reason}"
        );
        assert!(!reason.contains('{'), "JSON-free: {reason}");
    }

    #[test]
    fn common_branch_protection_rejections_are_humanized() {
        let cases = [
            (
                "GraphQL error: 3 of 5 required status checks are expected.",
                "Wait for CI",
            ),
            (
                "GraphQL error: 2 of 5 required status checks are pending.",
                "still running",
            ),
            (
                "GraphQL error: 1 of 5 required status checks have not succeeded.",
                "haven't passed",
            ),
            (
                "GraphQL error: At least 1 approving review is required by reviewers with write access.",
                "approving review",
            ),
            (
                "GraphQL error: Changes requested by reviewers who have write access.",
                "requested changes",
            ),
            (
                "GraphQL error: Base branch was modified. Review and try the merge again.",
                "base branch moved",
            ),
            (
                "GraphQL error: Head branch is out of date. Review and try the merge again.",
                "out of date",
            ),
            (
                "GraphQL error: You're not authorized to push to this branch.",
                "not authorized",
            ),
            (
                "GraphQL error: Pull Request has merge conflicts",
                "merge conflicts",
            ),
        ];

        for (raw, needle) in cases {
            let err = lazybox_core::ProviderError::permanent("github", raw);
            let reason = humanize_mutation_failure("merge", &err);
            assert!(
                reason.contains(needle),
                "message `{raw}` should humanize to contain `{needle}`, got: {reason}"
            );
            assert!(
                !reason.contains("GraphQL"),
                "raw GraphQL string leaked for `{raw}`: {reason}"
            );
        }
    }

    /// An unrecognized rejection is not mistranslated — it falls through
    /// to the provider's own (already-terse) user message.
    #[test]
    fn unrecognized_rejection_falls_through_to_raw_message() {
        let err = lazybox_core::ProviderError::permanent(
            "github",
            "GraphQL error: Pull request is not mergeable",
        );
        let reason = humanize_mutation_failure("merge", &err);
        assert!(
            reason.contains("Pull request is not mergeable"),
            "got {reason}"
        );
    }

    /// Issue #947: GitHub's raw "Pull Request has merge conflicts" is
    /// detected as a conflict (so `Event::PrMergeFailed.conflict` flags
    /// the resolve flow) and humanized to a plain-English notice.
    #[test]
    fn merge_conflict_rejection_is_detected_and_humanized() {
        assert!(is_merge_conflict("pull request has merge conflicts"));
        assert!(is_merge_conflict("the branch has conflicts"));
        assert!(!is_merge_conflict("head branch is out of date"));

        let err = lazybox_core::ProviderError::permanent(
            "github",
            "GraphQL error: Pull Request has merge conflicts",
        );
        let reason = humanize_mutation_failure("merge", &err);
        assert!(reason.contains("merge conflicts"), "got {reason}");
        assert!(!reason.contains("GraphQL"), "raw string leaked: {reason}");
    }

    /// Issue #998: GitHub's repository-ruleset rejection ("Repository rule
    /// violations found") must not leak raw — it humanizes to a plain,
    /// actionable notice. When the provider couldn't name the rule, the
    /// notice still points at where to look and the next step.
    #[test]
    fn ruleset_violation_without_names_is_humanized() {
        let err = lazybox_core::ProviderError::permanent(
            "github",
            "GraphQL error: Repository rule violations found",
        );
        let reason = humanize_mutation_failure("merge", &err);
        assert!(
            !reason.contains("Repository rule violations found"),
            "raw GraphQL string leaked: {reason}"
        );
        assert!(reason.contains("Can't merge"), "got {reason}");
        assert!(reason.contains("ruleset"), "names the cause: {reason}");
        assert!(!reason.contains("GraphQL"), "raw string leaked: {reason}");
        assert!(!reason.contains('{'), "JSON-free: {reason}");
    }

    /// When the provider enriched the error with the base branch's active
    /// rules (the REST rules-API lookup), the notice names them — so the
    /// user sees *which* rule blocked the merge, not a generic wall.
    #[test]
    fn ruleset_violation_names_the_rules_when_present() {
        let err = lazybox_core::ProviderError::permanent(
            "github",
            "GraphQL error: Repository rule violations found — active rules: \
             2 approving reviews, signed commits, required status checks",
        );
        let reason = humanize_mutation_failure("merge", &err);
        assert!(reason.contains("Can't merge"), "got {reason}");
        assert!(
            reason.contains("2 approving reviews"),
            "names the review rule: {reason}"
        );
        assert!(
            reason.contains("signed commits"),
            "names the signature rule: {reason}"
        );
        // The enriched tail contains "required status checks" — the notice
        // must render it as a named ruleset cause, NOT be hijacked by the
        // CI-wait branch into "Wait for CI to run".
        assert!(
            !reason.contains("Wait for CI"),
            "must not be mistranslated as a plain CI wait: {reason}"
        );
        assert!(!reason.contains("GraphQL"), "raw string leaked: {reason}");
    }

    /// Branch-protection humanization is merge-only: the messages are all
    /// merge-phrased and the matches ("not authorized", "write access to")
    /// are generic enough to fire on an unrelated op. A non-merge mutation
    /// (reply, labels, reviewers) that fails with such a message must keep
    /// its raw provider text, not get relabeled "Can't merge …".
    #[test]
    fn non_merge_op_is_not_relabeled_as_a_merge_rejection() {
        let err = lazybox_core::ProviderError::permanent(
            "github",
            "GraphQL error: You're not authorized to push to this branch.",
        );
        for op in ["reply", "labels", "reviewers", "assignees", "close-pr"] {
            let reason = humanize_mutation_failure(op, &err);
            assert!(
                !reason.contains("Can't merge"),
                "op `{op}` must not be relabeled as a merge rejection: {reason}"
            );
            assert!(
                reason.contains("not authorized"),
                "op `{op}` keeps its raw provider text: {reason}"
            );
        }

        // The merge family still translates the same message.
        for op in ["merge", "auto-merge"] {
            let reason = humanize_mutation_failure(op, &err);
            assert!(
                reason.contains("Can't merge"),
                "op `{op}` should humanize: {reason}"
            );
        }
    }

    #[test]
    fn required_check_count_takes_the_total_required() {
        assert_eq!(
            required_check_count("10 of 10 required status checks"),
            Some(10)
        );
        assert_eq!(
            required_check_count("2 of 5 required status checks"),
            Some(5)
        );
        assert_eq!(required_check_count("at least 1 approving review"), Some(1));
        assert_eq!(required_check_count("review is required"), None);
        // A digit appended after the status-check clause must not be read as
        // the total — the count stays anchored to "<n> of <m> required …".
        assert_eq!(
            required_check_count("10 of 10 required status checks are expected (run 42)"),
            Some(10)
        );
    }

    #[test]
    fn retry_wait_honors_hint_floor_and_caps() {
        assert!(
            mutation_retry_wait(1, 1) >= 1,
            "never zero (would hot-loop)"
        );
        assert!(
            mutation_retry_wait(60, 3) >= 60,
            "the reset hint is the floor, never undercut"
        );
        assert!(
            mutation_retry_wait(MUTATION_MAX_WAIT_SECS + 1_000, 5) <= MUTATION_MAX_WAIT_SECS,
            "a huge hint is capped so the queue stays bounded"
        );
    }

    /// The retry queue must run detached from its caller: a mutation that
    /// sits in the queue for minutes cannot hold the caller (a per-connection
    /// command slot) for the whole window, and it must outlive the caller so
    /// a disconnect doesn't drop the queued write. `detach_mutation` returns
    /// before its (slow) work finishes, and that work completes on its own.
    #[tokio::test(start_paused = true)]
    async fn detached_mutation_runs_independently_of_its_caller() {
        let (bus, mut rx) = tokio::sync::broadcast::channel(4);
        let bus2 = bus.clone();
        detach_mutation(async move {
            // Stand in for a queued retry that waits out a rate-limit window.
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            let _ = bus2.send(Event::provider_error_retryable("merge", "done"));
        });

        // The caller did NOT await the work — nothing has been emitted yet.
        assert!(
            rx.try_recv().is_err(),
            "detach_mutation must return before the queued work finishes"
        );

        // The detached task completes on its own, after the caller moved on.
        tokio::time::sleep(std::time::Duration::from_secs(301)).await;
        assert!(
            matches!(rx.recv().await, Ok(Event::ProviderError { .. })),
            "the detached work still runs to completion / outlives the caller"
        );
    }

    /// A `deleteIssue` falls back to a NOT_PLANNED close only for a genuine
    /// permission failure — never for a rate-limit that merely exhausted its
    /// retry window (which would silently downgrade a delete into a close).
    #[test]
    fn delete_falls_back_to_close_only_on_a_genuine_permission_failure() {
        let forbidden = lazybox_core::ProviderError::permanent(
            "github",
            "Must have admin rights to Repository.",
        );
        assert!(
            delete_should_fall_back_to_close(&forbidden),
            "FORBIDDEN → fall back to close"
        );

        let rate_limited = lazybox_core::ProviderError::retryable_after(
            "github",
            "You have exceeded a secondary rate limit",
            60,
        );
        assert!(
            !delete_should_fall_back_to_close(&rate_limited),
            "an exhausted rate limit must not downgrade a delete into a close"
        );
    }
}
