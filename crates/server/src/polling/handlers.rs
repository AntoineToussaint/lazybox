//! IPC mutation handlers — the daemon's "user pressed a key, mutate
//! upstream provider state" surface.
//!
//! These functions are the read-modify-write side of the polling
//! module: they look up the workspace, dispatch to the right provider
//! (via [`ProviderHandle`]), and emit events back to the bus. The
//! actual polling loop + workspace-store machinery lives in the
//! parent module ([`crate::polling`]); only the cross-cutting
//! helpers (`commit_upsert`, `load_workspace`) leak across via
//! `pub(super)`.

use super::{TickState, commit_upsert, fetch_and_apply, load_workspace};
use crate::ServerConfig;
use pilot_core::{CiStatus, ReviewStatus, Workspace, WorkspaceKey};
use pilot_gh::GhClient;
use pilot_ipc::Event;
use pilot_linear::LinearClient;

/// Post a top-level reply to the workspace's primary task. Today this
/// targets only GitHub PRs/issues; Linear and other providers can grow
/// into the same shape. On success we don't update the local activity
/// feed inline — the next poll picks up the new comment, which keeps
/// the "what the upstream provider says" invariant intact.
pub async fn post_reply(config: &ServerConfig, session_key: pilot_core::SessionKey, body: String) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return;
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
                emit_reply_error(config, &format!("workspace decode failed: {e}"));
                return;
            }
        },
        None => {
            emit_reply_error(config, "workspace not found");
            return;
        }
    };
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_reply_error(config, &e);
            return;
        }
    };
    if let Err(e) = provider.post_reply(&workspace, trimmed).await {
        tracing::warn!("post_reply {workspace_key}: {e:?}");
        emit_reply_error(config, &format!("post failed: {e}"));
        return;
    }
    tracing::info!(
        "posted reply to {} ({} chars)",
        workspace_key,
        trimmed.len()
    );
    // The poller picks up the comment on its next tick and broadcasts
    // a workspace upsert; nothing else to do here.
}

fn emit_reply_error(config: &ServerConfig, msg: &str) {
    let _ = config
        .bus
        .send(Event::provider_error_retryable("reply", msg));
}

/// Runtime-polymorphic wrapper around the workspace's
/// `TaskProvider`. Using an enum (vs `Arc<dyn TaskProvider>`) is
/// deliberate: the trait uses `async fn` which isn't dyn-compatible
/// without the `async_trait` crate. The enum dispatches manually
/// in O(n_providers) — fine for the 2-3 providers pilot will
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
    pub async fn merge(&self, ws: &pilot_core::Workspace) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::merge(c, ws).await,
            Self::Linear(c) => pilot_core::TaskProvider::merge(c, ws).await,
        }
    }
    pub async fn request_reviewers(
        &self,
        ws: &pilot_core::Workspace,
        logins: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::request_reviewers(c, ws, logins).await,
            Self::Linear(c) => pilot_core::TaskProvider::request_reviewers(c, ws, logins).await,
        }
    }
    pub async fn add_assignees(
        &self,
        ws: &pilot_core::Workspace,
        logins: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::add_assignees(c, ws, logins).await,
            Self::Linear(c) => pilot_core::TaskProvider::add_assignees(c, ws, logins).await,
        }
    }
    pub async fn set_assignees(
        &self,
        ws: &pilot_core::Workspace,
        logins: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::set_assignees(c, ws, logins).await,
            Self::Linear(c) => pilot_core::TaskProvider::set_assignees(c, ws, logins).await,
        }
    }
    pub async fn list_repo_labels(
        &self,
        ws: &pilot_core::Workspace,
    ) -> Result<Vec<pilot_core::Label>, pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::list_repo_labels(c, ws).await,
            Self::Linear(c) => pilot_core::TaskProvider::list_repo_labels(c, ws).await,
        }
    }
    pub async fn set_labels(
        &self,
        ws: &pilot_core::Workspace,
        names: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::set_labels(c, ws, names).await,
            Self::Linear(c) => pilot_core::TaskProvider::set_labels(c, ws, names).await,
        }
    }
    pub async fn post_reply(
        &self,
        ws: &pilot_core::Workspace,
        body: &str,
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::post_reply(c, ws, body).await,
            Self::Linear(c) => pilot_core::TaskProvider::post_reply(c, ws, body).await,
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
    workspace_key: &WorkspaceKey,
) -> Result<ProviderHandle, String> {
    let source = workspace_key
        .as_str()
        .split_once('-')
        .map(|(p, _)| p)
        .unwrap_or("");
    match source {
        s if s == pilot_gh::SOURCE => {
            let cred = pilot_gh::credential_chain()
                .resolve(pilot_gh::SOURCE)
                .await
                .map_err(|e| format!("github credentials: {e}"))?;
            let client = GhClient::from_credential(cred)
                .await
                .map_err(|e| format!("github client init: {e}"))?;
            Ok(ProviderHandle::Github(client))
        }
        s if s == pilot_linear::SOURCE => {
            let cred = pilot_linear::credential_chain()
                .resolve(pilot_linear::SOURCE)
                .await
                .map_err(|e| format!("linear credentials: {e}"))?;
            Ok(ProviderHandle::Linear(LinearClient::from_credential(cred)))
        }
        other => Err(format!(
            "no provider registered for workspace prefix `{other}`",
        )),
    }
}

/// Handle `Command::MergePr`: load the workspace, recover the PR's
/// GraphQL node id from its primary task, and ship a `mergePullRequest`
/// mutation. On success the next poll cycle picks up the new MERGED
/// state and the workspace lands in the Inactive mailbox (or folds
/// into nothing if `closingIssuesReferences` had set up a collapse).
///
/// Errors surface as `Event::ProviderError` so the TUI can flash the
/// reason without us inventing a bespoke event variant.
pub async fn handle_merge_pr(config: &ServerConfig, workspace_key: WorkspaceKey) {
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
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = provider.merge(&ws).await {
        tracing::warn!("merge {workspace_key}: {e:?}");
        emit_err(&format!("merge failed: {e}"));
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
    config.poll_wake.notify_one();
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
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = provider.request_reviewers(&ws, &logins).await {
        tracing::warn!("request_reviewers {workspace_key} {logins:?}: {e:?}");
        emit_err(&format!("request reviewers failed: {e}"));
    } else {
        tracing::info!("requested reviewers {logins:?} on workspace {workspace_key}");
        // Wake the poll loop so the reviewer chip on the row
        // updates immediately. Without this the sidebar lags 60s.
        config.poll_wake.notify_one();
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
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = provider.add_assignees(&ws, &logins).await {
        tracing::warn!("add_assignees {workspace_key} {logins:?}: {e:?}");
        emit_err(&format!("add assignees failed: {e}"));
    } else {
        tracing::info!("added assignees {logins:?} on workspace {workspace_key}");
        config.poll_wake.notify_one();
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
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = provider.set_assignees(&ws, &logins).await {
        tracing::warn!("set_assignees {workspace_key} {logins:?}: {e:?}");
        emit_err(&format!("update assignees failed: {e}"));
        return;
    }
    tracing::info!("set assignees to {logins:?} on workspace {workspace_key}");
    // Wake the poll loop so the task row picks up the new assignee
    // set immediately — without this the row stays stale for up to
    // a full interval (60s default).
    config.poll_wake.notify_one();
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
    let emit_err = |msg: &str| {
        let _ = config
            .bus
            .send(Event::provider_error_retryable("labels", msg));
    };
    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!("set_labels: workspace {workspace_key} not found"));
        return;
    };
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = provider.set_labels(&ws, &names).await {
        tracing::warn!("set_labels {workspace_key} {names:?}: {e:?}");
        emit_err(&format!("update labels failed: {e}"));
        return;
    }
    tracing::info!("set labels to {names:?} on workspace {workspace_key}");
    config.poll_wake.notify_one();
}

/// Handle `Command::FetchRepoLabels`: pull the workspace repo's full
/// label set and broadcast `Event::RepoLabels` so the TUI can
/// populate the picker. Silent on failure (we just don't broadcast),
/// the picker then falls back to whatever labels are already on the
/// task — same UX as a cold network.
pub async fn handle_fetch_repo_labels(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let Some(ws) = load_workspace(config, &workspace_key) else {
        tracing::debug!("fetch_repo_labels: workspace {workspace_key} not found");
        return;
    };
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("fetch_repo_labels: {e}");
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
        }
    }
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
/// `/tmp/pilot.log`.
pub async fn handle_fetch_pr_details(config: &ServerConfig, workspace_key: WorkspaceKey) {
    // Use the persistent client from TickState so the rate budget
    // and observations carry across calls — same logic as the
    // long-lived poll loop. Without this we'd build a fresh client
    // for every user-triggered fetch.
    // Clone the cached client out under a brief std-lock. The lock is
    // released before any `.await` — building a fresh client on a cold
    // cache must not hold a lock across the `from_credential` network
    // call (issue #92). The cache lives outside `poll_state` so this
    // never contends with a running poll tick.
    let cached = config
        .gh_client_cache
        .lock()
        .expect("gh_client_cache poisoned")
        .clone();
    let client = match cached {
        Some(c) => c,
        None => {
            let cred = match pilot_gh::credential_chain().resolve(pilot_gh::SOURCE).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("fetch_pr_details credentials: {e}");
                    return;
                }
            };
            match GhClient::from_credential(cred).await {
                Ok(c) => {
                    *config
                        .gh_client_cache
                        .lock()
                        .expect("gh_client_cache poisoned") = Some(c.clone());
                    c
                }
                Err(e) => {
                    tracing::warn!("fetch_pr_details client init: {e}");
                    return;
                }
            }
        }
    };

    // `fetch_and_apply` runs the network call against the initial
    // workspace snapshot, then re-loads right before the transform
    // so the activity merge applies to the freshest state — locks
    // in the race fix that the open-coded version had to discover
    // the hard way (PR row stuck on "CI RUN" after GitHub flipped
    // to SUCCESS because a 1-2s GraphQL write clobbered fresh poll
    // state with a stale snapshot).
    let result = fetch_and_apply(
        config,
        &workspace_key,
        |initial| {
            let client = client.clone();
            async move {
                let Some(pr) = initial.pr.as_ref() else {
                    return Ok::<_, ()>(None);
                };
                let Some(node_id) = pr.node_id.clone() else {
                    return Ok(None);
                };
                match client.fetch_pr_details(&node_id).await {
                    Ok(Some(details)) => Ok(Some(details)),
                    Ok(None) => Ok(None),
                    Err(e) => {
                        tracing::warn!("fetch_pr_details({node_id}): {e}");
                        Ok(None)
                    }
                }
            }
        },
        |ws, details_opt| {
            let Some(details) = details_opt else {
                return;
            };
            let merged_count = details.activities.len();
            // `Workspace::merge_activity` dedups by (author, body,
            // created_at) AND remaps `read_indices` across the
            // post-sort positions. A prior implementation here did a
            // raw push + sort, which left `read_indices` pointing at
            // stale slots — every lazy-fetch silently scrambled the
            // user's read marks.
            ws.merge_activity(&details.activities);
            merge_pr_details_into_workspace(ws, details);
            tracing::info!(
                workspace = %ws.key,
                merged = merged_count,
                "fetch_pr_details: merged review-thread activities + PR fields"
            );
        },
    )
    .await;
    // Result is Result<MutationOutcome, ()> — the fetcher swallows
    // its own provider errors; both Applied and Missing are
    // user-visible-silent successes.
    let _ = result;
}

/// Splice a freshly-fetched `PrDetails` into a workspace's PR slot.
/// No-op when the workspace has no PR (the lazy-fetch path skips
/// issue-only workspaces upstream so this is mostly defensive).
///
/// Field rules:
/// - `closes_issues`, `checks`, `ci`, `review`, `role`, `needs_reply`,
///   `last_commenter` — overwrite with the lazy result. The lazy
///   query is authoritative; it has the data the inbox-scan path
///   could only approximate.
/// - `unread_count` — recompute from the activity list since lazy
///   knows the full activity count. The workspace-level
///   `Workspace::unread_count()` still respects `read_indices`, so
///   user read state isn't disturbed.
fn merge_pr_details_into_workspace(ws: &mut Workspace, details: pilot_gh::PrDetails) {
    let Some(pr) = ws.pr.as_mut() else {
        return;
    };
    if !details.closes_issues.is_empty() {
        // Replace verbatim — lazy is authoritative.
        pr.closes_issues = details.closes_issues;
    }
    if !details.checks.is_empty() {
        pr.checks = details.checks;
    }
    pr.ci = details.ci;
    pr.review = details.review;
    pr.role = details.role;
    pr.needs_reply = details.needs_reply;
    pr.last_commenter = details.last_commenter;
    pr.unread_count = details.activities.len() as u32;
}

/// Admin: walk every persisted workspace, drop sessions whose
/// terminals aren't currently live, and tear down the matching
/// worktree on disk. Inbox rows stay because we only touch
/// `workspace.sessions` (and the on-disk dir) — `workspace.pr` /
/// `gh_issues` aren't modified, so the sidebar keeps the row.
///
/// Live sessions (the user has an attached claude / shell) are
/// silently skipped so a long-running agent isn't pulled out from
/// under itself. Counts are emitted on `Event::CleanWorktreesCompleted`
/// so the TUI can surface "cleaned N · kept M (active)".
pub async fn handle_clean_worktrees(config: &ServerConfig) {
    let records = match config.store.list_workspaces() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("clean_worktrees: list_workspaces failed: {e}");
            return;
        }
    };

    // Snapshot live session ids — anything in `terminal_sessions`
    // (the per-terminal owning-session map) is a session we must
    // not touch. Lock dropped before any async fs work.
    let live_sessions: std::collections::HashSet<pilot_core::SessionId> = {
        let map = config.terminal_sessions.lock().await;
        map.values().copied().collect()
    };

    let mgr = pilot_git_ops::WorktreeManager::default_base();
    let mut removed: usize = 0;
    let mut skipped: usize = 0;

    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(mut workspace) = serde_json::from_str::<pilot_core::Workspace>(&json) else {
            continue;
        };
        let workspace_key = workspace.key.clone();

        // Resolve the bare-repo path once per workspace (every
        // session shares the same upstream).
        let bare_path = workspace
            .primary_task()
            .and_then(|t| t.repo.as_deref())
            .and_then(|repo| repo.split_once('/'))
            .map(|(owner, name)| mgr.bare_path(owner, name));

        // Walk highest-index-first so removals don't shift indices
        // out from under the loop.
        let session_count = workspace.sessions.len();
        let mut wrote = false;
        for idx in (0..session_count).rev() {
            let session = workspace.sessions[idx].clone();
            if live_sessions.contains(&session.id) {
                skipped += 1;
                continue;
            }
            tracing::info!(
                workspace = %workspace_key,
                session = %session.id,
                worktree = %session.worktree_path.display(),
                "clean_worktrees: removing",
            );
            if let Some(bare) = bare_path.as_ref() {
                let _ = mgr.remove_by_path(bare, &session.worktree_path).await;
            } else {
                // No upstream repo metadata (rare — pre-PR / scratch
                // workspaces) → just `rm -rf` the dir, no git
                // bookkeeping to update.
                let _ = tokio::fs::remove_dir_all(&session.worktree_path).await;
            }
            workspace.sessions.remove(idx);
            removed += 1;
            wrote = true;
        }
        if wrote {
            commit_upsert(config, &workspace_key, workspace);
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
fn collect_tracked_sessions(config: &ServerConfig) -> Vec<pilot_git_ops::TrackedSession> {
    let records = match config.store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("inspect_worktrees: list_workspaces failed: {e}");
            return Vec::new();
        }
    };
    let mut out: Vec<pilot_git_ops::TrackedSession> = Vec::with_capacity(records.len() * 2);
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = serde_json::from_str::<pilot_core::Workspace>(&json) else {
            continue;
        };
        for session in workspace.sessions {
            let is_stopped = matches!(session.state, pilot_core::SessionRunState::Stopped);
            // First 8 chars of the UUID — enough to identify a row
            // in the modal without leaking the whole id into the UI.
            let raw = session.id.to_string();
            let session_id = raw.get(..8).unwrap_or(&raw).to_string();
            out.push(pilot_git_ops::TrackedSession {
                session_id,
                worktree_path: session.worktree_path,
                is_stopped,
            });
        }
    }
    out
}

fn to_dto(row: pilot_git_ops::WorktreeInspection) -> pilot_ipc::WorktreeInspectionDto {
    pilot_ipc::WorktreeInspectionDto {
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

/// Run the worktree inspector and emit the result on the bus.
/// Read-only — pair with [`handle_delete_orphaned_worktree`] for
/// destructive follow-up.
pub async fn handle_inspect_worktrees(config: &ServerConfig) {
    inspect_worktrees_with(config, &pilot_git_ops::WorktreeManager::default_base()).await
}

/// Test seam for [`handle_inspect_worktrees`]. Production callers
/// use the default base; tests pass an explicit manager rooted at a
/// tempdir so they don't have to mutate `PILOT_HOME`.
pub(crate) async fn inspect_worktrees_with(
    config: &ServerConfig,
    mgr: &pilot_git_ops::WorktreeManager,
) {
    let tracked = collect_tracked_sessions(config);
    let inspections = match mgr.inspect_worktrees(&tracked).await {
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

/// Delete a single worktree by path. Re-runs a fresh inspection of
/// that one row so the safety check uses live state (the inspector
/// result the TUI is acting on may be seconds old). `force = true`
/// bypasses uncommitted / unpushed / locked refusal.
pub async fn handle_delete_orphaned_worktree(
    config: &ServerConfig,
    path: std::path::PathBuf,
    force: bool,
) {
    delete_orphaned_worktree_with(
        config,
        &pilot_git_ops::WorktreeManager::default_base(),
        path,
        force,
    )
    .await
}

/// Test seam for [`handle_delete_orphaned_worktree`]. Same contract,
/// explicit manager.
pub(crate) async fn delete_orphaned_worktree_with(
    config: &ServerConfig,
    mgr: &pilot_git_ops::WorktreeManager,
    path: std::path::PathBuf,
    force: bool,
) {
    let tracked = collect_tracked_sessions(config);

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

/// Auto-reap the worktrees behind a freshly-merged PR's sessions.
///
/// Gated behind `worktree.auto_cleanup_merged` (off by default).
/// Called from the upsert path when a PR transitions to merged — see
/// [`super::merged_transition_pr_number`]. Loads the config fresh so a
/// user flipping the toggle takes effect without a restart, then
/// delegates to [`cleanup_merged_worktrees_with`] against the default
/// base.
pub async fn cleanup_merged_worktrees(config: &ServerConfig, key: &WorkspaceKey, pr_number: u64) {
    let enabled = pilot_config::Config::load()
        .map(|c| c.worktree.auto_cleanup_merged)
        .unwrap_or(false);
    if !enabled {
        return;
    }
    cleanup_merged_worktrees_with(
        config,
        &pilot_git_ops::WorktreeManager::default_base(),
        key,
        pr_number,
    )
    .await
}

/// Test seam for [`cleanup_merged_worktrees`] — same contract, an
/// explicit manager (tempdir-rooted in tests), and no config gate so
/// the caller decides when cleanup runs.
///
/// Only removes worktrees the inspector flags `is_safe_to_delete`
/// (clean tree, pushed, unlocked — typically the merged branch was
/// auto-deleted upstream) AND whose session has no live terminal
/// attached. Each reaped session is dropped from the workspace and the
/// trimmed record re-committed; a final [`Event::Notification`] tells
/// the user what was cleaned.
pub(crate) async fn cleanup_merged_worktrees_with(
    config: &ServerConfig,
    mgr: &pilot_git_ops::WorktreeManager,
    key: &WorkspaceKey,
    pr_number: u64,
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
    // on its own.
    let live: std::collections::HashSet<pilot_core::SessionId> = {
        let map = config.terminal_sessions.lock().await;
        map.values().copied().collect()
    };

    let tracked = collect_tracked_sessions(config);
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
    // `/private/var`, or a symlinked `PILOT_HOME`). Canonicalizing both
    // sides matches the inspector's own `canonical_or_self` keying so a
    // safe worktree is never silently skipped over a cosmetic path
    // difference.
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let by_path: std::collections::HashMap<std::path::PathBuf, &pilot_git_ops::WorktreeInspection> =
        inspections.iter().map(|r| (canon(&r.path), r)).collect();

    let mut removed: usize = 0;
    let mut wrote = false;
    // Walk highest-index-first so removals don't shift indices out
    // from under the loop.
    for idx in (0..workspace.sessions.len()).rev() {
        let session_id = workspace.sessions[idx].id;
        let worktree_path = workspace.sessions[idx].worktree_path.clone();
        if live.contains(&session_id) {
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
        commit_upsert(config, key, workspace);
    }
    if removed > 0 {
        let noun = if removed == 1 {
            "worktree"
        } else {
            "worktrees"
        };
        let _ = config.bus.send(Event::Notification {
            title: "pilot".into(),
            body: format!("Cleaned up {removed} {noun} for merged PR #{pr_number}"),
        });
    }
}

/// Post-tick prefetch: after a successful poll, pick the top-N PRs
/// most likely to be clicked next and concurrently fetch their
/// review-thread details so the right pane is hot when the user
/// gets there.
///
/// Why N=5 + concurrency=3: each `fetch_pr_details` call costs ~550
/// graphql units; with N=5 + 60s cadence that's ~27500/hr against
/// GitHub's 5000-cost-units-per-graphql-resource hourly budget. Fits
/// comfortably. Concurrency=3 keeps the local rate budget healthy
/// (capacity 30, refill 30/min) alongside the parallel main+merged+
/// watched-repo branches of the same tick.
///
/// Dedup via `TickState::prefetched_pr_details`: once we've pulled a
/// PR's threads this daemon session, the row's still subject to the
/// TUI's lazy-fetch on focus (so re-opens get fresh data), but we
/// don't re-pull every poll cycle. Cleared on daemon restart.
///
/// Scoring (descending):
/// - CI failing → +100 (highest-actionability — user wants to fix)
/// - Review pending / changes-requested → +50
/// - Unread activity → +10 per item (capped at +50)
/// - PR has `node_id` → +1 (otherwise we couldn't fetch anyway)
///
/// 0-score workspaces are skipped — they don't need the prefetch.
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
    let Some(client) = config
        .gh_client_cache
        .lock()
        .expect("gh_client_cache poisoned")
        .clone()
    else {
        return;
    };

    // Score every polled workspace, keep the ones with a fetchable
    // PR. `polled` is the key list from the just-completed tick; load
    // each via the store path the rest of the handler module uses so
    // the scoring sees the post-upsert state.
    let mut scored: Vec<(i32, String, WorkspaceKey)> = Vec::new();
    for key in polled {
        let Some(ws) = load_workspace(config, key) else {
            continue;
        };
        let Some(pr) = ws.pr.as_ref() else {
            continue;
        };
        let Some(node_id) = pr.node_id.clone() else {
            continue;
        };
        if state.prefetched_pr_details.contains(&node_id) {
            continue;
        }
        let mut score: i32 = 1;
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
                // Route through `fetch_and_apply` so the race fix in
                // `handle_fetch_pr_details` applies here too: re-load
                // before the activity merge so a concurrent poll
                // write isn't clobbered.
                let mut merged_here = 0usize;
                let _ = fetch_and_apply(
                    config,
                    &key,
                    |_initial| {
                        let client = client.clone();
                        let node_id = node_id.clone();
                        async move {
                            match client.fetch_pr_details(&node_id).await {
                                Ok(details) => Ok::<_, ()>(details),
                                Err(e) => {
                                    tracing::debug!(
                                        "prefetch_top_pr_details: fetch_pr_details({node_id}) failed: {e}",
                                    );
                                    Ok(None)
                                }
                            }
                        }
                    },
                    |ws, details_opt| {
                        let Some(details) = details_opt else {
                            return;
                        };
                        merged_here = details.activities.len();
                        ws.merge_activity(&details.activities);
                        merge_pr_details_into_workspace(ws, details);
                    },
                )
                .await;
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
mod inspect_tests {
    //! Integration tests for `handle_inspect_worktrees` +
    //! `handle_delete_orphaned_worktree`. The handlers depend on a
    //! `WorktreeManager` plus the workspace store; both are wired up
    //! against a tempdir + an in-memory store here so the tests are
    //! hermetic (no `PILOT_HOME` env mutation, no shared on-disk
    //! state with other tests).

    use super::*;
    use crate::ServerConfig;
    use pilot_core::{SessionId, SessionKind, SessionRunState, WorkspaceSession};
    use pilot_ipc::Event;
    use pilot_store::{MemoryStore, Store, WorkspaceRecord};
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
        // Pilot-style remote-tracking refspec so `@{u}` resolves
        // from worktrees (matches `WorktreeManager::checkout_at`).
        run(
            &bare,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        )
        .await;

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
        use pilot_core::{SessionKey, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey};
        let session_key: SessionKey = "github:o/r#1".into();
        let workspace_key: WorkspaceKey = WorkspaceKey::new(session_key.as_str().to_string());
        let task = Task {
            id: TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            },
            title: "test".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: pilot_core::CiStatus::None,
            review: pilot_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()),
            branch: Some("feat".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(),
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: pilot_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
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
        use pilot_core::{Task, TaskId, TaskRole, TaskState, Workspace};
        let task = Task {
            id: TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            },
            title: "merged pr".into(),
            body: None,
            state: TaskState::Merged,
            role: TaskRole::Author,
            ci: pilot_core::CiStatus::None,
            review: pilot_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()),
            branch: Some(branch.into()),
            base_branch: None,
            updated_at: chrono::Utc::now(),
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: pilot_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());
        let mut rx = config.bus.subscribe();

        cleanup_merged_worktrees_with(&config, &mgr, &key, 1).await;

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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        cleanup_merged_worktrees_with(&config, &mgr, &key, 1).await;

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
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        cleanup_merged_worktrees_with(&config, &mgr, &key, 1).await;

        assert!(
            !real.exists(),
            "worktree reached via symlink should be reaped"
        );
        let reloaded = load_workspace(&config, &key).expect("workspace");
        assert!(reloaded.sessions.is_empty(), "session should be dropped");
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
            .terminal_sessions
            .lock()
            .await
            .insert(pilot_ipc::TerminalId(1), sid);
        let mgr = pilot_git_ops::WorktreeManager::new(fx.base.path().to_path_buf());

        cleanup_merged_worktrees_with(&config, &mgr, &key, 1).await;

        assert!(wt.exists(), "live session's worktree must be preserved");
        let reloaded = load_workspace(&config, &key).expect("workspace");
        assert_eq!(reloaded.sessions.len(), 1, "live session must be retained");
    }
}
