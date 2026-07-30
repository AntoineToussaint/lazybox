//! Workspace upsert preparation and atomic store persistence.

use super::sources::github_scopes_from_filters;
use super::{
    EngagementTier, PendingIssueMerge, auto_merge, commit_merge, handlers,
    lock_workspace_with_closing_issues, merge_closing_issue_workspaces,
    pr_workspace_claiming_issue,
};
use crate::ServerConfig;
use chrono::Utc;
use lazybox_core::{Task, Workspace, WorkspaceKey};
use lazybox_ipc::Event;
use lazybox_store::{StoreError, StoreMutation, WorkspaceRecord};

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
pub(super) struct UpsertContext {
    /// Workspace keys the user explicitly archived (`x x`).
    archived: std::collections::HashSet<String>,
    /// `closes_issues` TaskId → claiming PR workspace key. Mirrors
    /// what `pr_workspace_claiming_issue` derives per call. Updated
    /// inline as PR tasks flow through the batch so an issue polled
    /// AFTER its PR in the same tick still routes correctly.
    closes_index: std::collections::HashMap<lazybox_core::TaskId, WorkspaceKey>,
}

impl UpsertContext {
    pub(super) fn build(config: &ServerConfig) -> Self {
        let archived = crate::workspace::load_archived_set(config);
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

pub(super) async fn upsert_with_context(
    config: &ServerConfig,
    ctx: &mut UpsertContext,
    task: Task,
) {
    // Skip re-creating workspaces the user explicitly archived
    // (`x x`). Without this, every 60s tick re-creates the row
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
pub(super) async fn upsert_into_workspace_key(
    config: &ServerConfig,
    key: &WorkspaceKey,
    task: Task,
) {
    // LOST-UPDATE GUARD: this function is a load→modify→commit that
    // spans awaits (`prepare_upsert` → `commit_merge`). Detached
    // mutation handlers (mark-read, snooze, layout) run concurrently
    // on the serve loop's JoinSet and do their own load-modify-save
    // on the SAME kv row; without serialization, a tick that loaded a
    // pre-mark copy here would commit it after the user's mark landed
    // — silently reverting the mark. Held from before the first load
    // until after the commit; released before the terminal-transition
    // tail, which prompts/cleans through its own paths and must not
    // nest under this guard.
    let ws_guards = lock_workspace_with_closing_issues(config, key, Some(&task)).await;
    // 0. TERMINAL-STATE DETECTION: cheap pre-check (no IO) gates the
    //    store read — only a PR observed Merged or an issue observed
    //    Closed can trigger cleanup. We snapshot the previous state
    //    here, before `prepare_upsert` overwrites it, so we only act on
    //    the open→terminal *transition* and not on every subsequent tick
    //    of an already-merged PR / already-closed issue.
    let terminal_cleanup = if task.is_pr() && task.state == lazybox_core::TaskState::Merged {
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
        merged_transition_pr_number(prev.as_ref(), &task).map(TerminalCleanup::MergedPr)
    } else if !task.is_pr() && task.state == lazybox_core::TaskState::Closed {
        // An issue observed Closed. Unlike the merged-PR sweep, closed
        // issues only reach here via the notifications-driven single
        // fetch (`fetch_single_issue`), so a missing predecessor just
        // means "nothing to clean" — `closed_issue_transition` declines
        // and the row upserts normally without a cleanup prompt.
        //
        // But when a PR claims this issue ("Closes #N"), that PR's own
        // merge prompt owns the cleanup after the issue row collapses
        // into it. Firing here too would race the collapse and surface
        // a second, stale prompt for the soon-to-be-absorbed issue row
        // (the two prompts key on different workspaces, so the TUI's
        // per-key dedupe can't merge them). Defer to the PR path.
        if pr_workspace_claiming_issue(config, &task.id).is_some() {
            None
        } else {
            let prev = load_workspace(config, key);
            closed_issue_transition(prev.as_ref(), &task).map(TerminalCleanup::ClosedIssue)
        }
    } else {
        None
    };

    // REOPEN-CANCEL (issue #552): an issue flipping closed→open cancels
    // any outstanding removal prompt — the workspace is alive again, so a
    // stale "remove closed issue?" modal must go. Detected off the stored
    // (pre-`prepare_upsert`) state rather than the in-memory prompt
    // record so it still fires after a daemon restart, when a client may
    // hold a modal the restarted daemon has no cadence memory of. The
    // load is scoped to open issues, whose predecessor state we'd need to
    // read regardless.
    let reopened_issue = !task.is_pr()
        && task.state == lazybox_core::TaskState::Open
        && issue_reopened(load_workspace(config, key).as_ref(), &task);

    // 1. PREPARE: build the workspace's final in-memory state.
    //    Includes the optional issue-collapse merge — if a PR
    //    polls in with `closes_issues`, we fold standalone issue
    //    workspaces into it here. Async (touches the store +
    //    `terminal_meta`) but doesn't yet write the PR's own row.
    //    `None` means the stored row exists but is unreadable —
    //    committing would overwrite the preserved row with
    //    freshly-derived state, so this poll's update is dropped.
    let Some((workspace, pending_merges)) = prepare_upsert(config, key, task).await else {
        return;
    };

    // Auto-merge signal, captured off the final in-memory state before
    // the commit consumes it (commit only mutates sessions/paths, never
    // the PR fields the signal reads). The hook itself runs AFTER the
    // commit + broadcast below, so an attempt can never observe — or
    // race — a workspace state clients haven't seen.
    let auto_merge_signal = auto_merge::signal_for(&workspace);

    // 2. COMMIT: migrate worktree dirs to the (possibly new) PR slug,
    //    atomically persist the PR, terminal rebadges, and absorbed-issue
    //    deletes, then publish the complete I1–I6 event tail. The blocking
    //    owner retains every lock and projection even if the polling task is
    //    cancelled by its per-task timeout.
    let commit_outcome = commit_merge(config, workspace, pending_merges, ws_guards).await;

    // Daemon-side "auto-merge on green" (the trigger moved out of the
    // TUI): evaluate the freshly-committed state and dispatch a merge
    // attempt when it flipped merge-ready. One firing authority — a
    // headless daemon fires it, N attached clients can't double-fire.
    auto_merge::on_workspace_committed(
        config,
        key,
        auto_merge_signal,
        commit_outcome == CommitOutcome::Changed,
    );

    // 3. TERMINAL: the PR merged or the issue closed → either reap its
    //    safe-to-delete worktrees silently (when
    //    `worktree.auto_cleanup_merged` is on) or prompt the user to
    //    remove the workspace + worktree (the default). Runs after the
    //    commit so it re-reads the freshly persisted session set.
    if let Some(cleanup) = terminal_cleanup {
        handlers::on_terminal_transition(config, key, cleanup).await;
    } else if reopened_issue {
        handlers::cancel_pending_removal(config, key).await;
    }
}

/// A workspace's primary task just reached a terminal state that makes
/// its sessions + worktree cleanup candidates. Carries the task number
/// so the auto-cleanup notice can name the item.
#[derive(Debug, Clone, Copy)]
pub(super) enum TerminalCleanup {
    MergedPr(u64),
    ClosedIssue(u64),
}

impl TerminalCleanup {
    /// Which terminal state the modal copy should name.
    pub(super) fn removal_state(self) -> lazybox_ipc::RemovableTerminalState {
        match self {
            Self::MergedPr(_) => lazybox_ipc::RemovableTerminalState::Merged,
            Self::ClosedIssue(_) => lazybox_ipc::RemovableTerminalState::Closed,
        }
    }

    /// Human phrase for the silent auto-cleanup notification.
    pub(super) fn describe(self) -> String {
        match self {
            Self::MergedPr(n) => format!("merged PR #{n}"),
            Self::ClosedIssue(n) => format!("closed issue #{n}"),
        }
    }
}

/// Task number parsed off a task id (`owner/repo#123` → `123`), for a PR
/// or an issue alike. Mirrors the `#`-split [`Workspace::worktree_slug`]
/// uses. `None` for any id whose suffix isn't a number.
pub(super) fn task_number(task: &Task) -> Option<u64> {
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
pub(super) fn merged_transition_pr_number(prev: Option<&Workspace>, task: &Task) -> Option<u64> {
    if !task.is_pr() || task.state != lazybox_core::TaskState::Merged {
        return None;
    }
    let prev_state = prev.and_then(|w| w.task_by_id(&task.id))?.state;
    if prev_state == lazybox_core::TaskState::Merged {
        return None;
    }
    task_number(task)
}

/// Decide whether `task` represents an issue that *just* transitioned
/// into the closed state, returning its number when so.
///
/// Same one-shot contract as [`merged_transition_pr_number`]: it
/// requires a known predecessor that was **not** already closed (a
/// genuine open→closed flip), so once the closed state is persisted the
/// next observation sees `prev` already closed and skips — cleanup is
/// offered exactly once per close. A closed issue we have no prior
/// workspace for has no sessions to reap, so firing would prompt about
/// nothing.
pub(super) fn closed_issue_transition(prev: Option<&Workspace>, task: &Task) -> Option<u64> {
    if task.is_pr() || task.state != lazybox_core::TaskState::Closed {
        return None;
    }
    let prev_state = prev.and_then(|w| w.task_by_id(&task.id))?.state;
    if prev_state == lazybox_core::TaskState::Closed {
        return None;
    }
    task_number(task)
}

/// Whether `task` is an issue that *just* transitioned closed→open (a
/// reopen). The inverse of [`closed_issue_transition`]: it requires a
/// known predecessor that *was* closed, so a genuinely-open issue polled
/// fresh (no prior workspace, or a prior that was already open) is not a
/// reopen and can't cancel a removal that never existed.
pub(super) fn issue_reopened(prev: Option<&Workspace>, task: &Task) -> bool {
    if task.is_pr() || task.state != lazybox_core::TaskState::Open {
        return false;
    }
    prev.and_then(|w| w.task_by_id(&task.id)).map(|t| t.state)
        == Some(lazybox_core::TaskState::Closed)
}

/// What the store holds at a workspace key, with "present but
/// unreadable" kept distinct from "absent". The distinction is what
/// stops the upsert path from clobbering a row that startup's
/// `load_workspaces` honestly preserved ("preserved, not deleted"):
/// treating a decode failure as absent rebuilt the workspace fresh
/// from the task and overwrote the preserved row on the very next
/// poll — destroying its sessions, read-state, snooze, and policies.
enum StoredWorkspace {
    /// No row / no JSON payload — nothing to lose, safe to create.
    Absent,
    Present(Box<Workspace>),
    /// Row exists but cannot be read: store read error, corrupt JSON,
    /// or a schema stamped by a newer build (downgrade). Carries the
    /// reason for the report.
    Unreadable(String),
}

/// Strict load of the stored workspace row at `key`. Callers on a
/// read-modify-WRITE path must branch on [`StoredWorkspace::Unreadable`]
/// and skip the write; lenient read-only consumers can keep using
/// [`load_workspace`].
fn load_stored_workspace(config: &ServerConfig, key: &WorkspaceKey) -> StoredWorkspace {
    match config.store.get_workspace(key) {
        // A transient read error (e.g. SQLITE_BUSY) means we cannot
        // know whether a row exists — writing "fresh" state now could
        // overwrite one. Same preserve semantics as corruption.
        Err(e) => StoredWorkspace::Unreadable(format!("store read failed: {e}")),
        Ok(None) => StoredWorkspace::Absent,
        Ok(Some(record)) => match record.workspace_json {
            None => StoredWorkspace::Absent,
            Some(json) => match Workspace::decode_persisted(&json) {
                Ok(ws) => StoredWorkspace::Present(Box::new(ws)),
                Err(e) => StoredWorkspace::Unreadable(e.to_string()),
            },
        },
    }
}

/// Minimum interval between repeated "unreadable stored row" reports
/// for the same workspace key. The poll tick re-encounters the same
/// corrupt row every cycle; one storage event every ten minutes keeps
/// the condition visible without hint-bar spam every ~60s.
const UNDECODABLE_REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

/// Log + broadcast that a stored workspace row is unreadable and being
/// preserved, debounced per key via
/// [`ServerConfig::undecodable_row_reports`]. Mirrors startup's
/// `storage_recovery_event` wording so the user sees one consistent
/// story ("preserved, not deleted") from both paths.
fn report_unreadable_workspace_row(config: &ServerConfig, key: &str, reason: &str) {
    tracing::warn!(
        workspace_key = %key,
        %reason,
        "upsert: stored workspace row is unreadable — preserving it, skipping this poll's update"
    );
    let now = std::time::Instant::now();
    {
        let mut reports = config.undecodable_row_reports.lock();
        if let Some(last) = reports.get(key)
            && now.duration_since(*last) < UNDECODABLE_REPORT_EVERY
        {
            return;
        }
        reports.insert(key.to_string(), now);
    }
    let _ = config.bus.send(Event::provider_error_permanent(
        "storage",
        format!(
            "workspace {key} could not be loaded ({reason}). It was preserved, not \
             overwritten; provider updates for it are paused. Back up the state \
             directory and follow the recovery guide."
        ),
    ));
}

/// Pure-ish prepare step: load the existing workspace (if any),
/// attach the incoming task, and run the issue-collapse merge. No
/// store writes, no `WorkspaceUpserted` broadcast — the returned
/// `Workspace` is what we'll commit in step 3.
///
/// Returns `None` when the stored row exists but cannot be decoded
/// (corruption, store read error, or newer-schema row from a
/// downgrade): the caller must skip the upsert entirely so the
/// preserved row stays intact for startup/manual recovery.
///
/// Split out from `upsert_into_workspace_key` so a future test can
/// drive the prepare step against a mock store without committing
/// real state — the "did the merge attach the issue task?" question
/// is now answerable without the full IPC bus + store side effects.
async fn prepare_upsert(
    config: &ServerConfig,
    key: &WorkspaceKey,
    task: Task,
) -> Option<(Workspace, Vec<PendingIssueMerge>)> {
    let existing = match load_stored_workspace(config, key) {
        StoredWorkspace::Present(ws) => Some(*ws),
        StoredWorkspace::Absent => None,
        StoredWorkspace::Unreadable(reason) => {
            report_unreadable_workspace_row(config, key.as_str(), &reason);
            return None;
        }
    };

    let task_id = task.id.clone();
    let previous_task = existing
        .as_ref()
        .and_then(|workspace| workspace.task_by_id(&task_id))
        .cloned();

    let mut workspace = match existing {
        Some(mut w) => {
            w.attach_task(task);
            w
        }
        None => Workspace::from_task(task, Utc::now()),
    };

    let observation_window_ms = if task_id.source == lazybox_gh::SOURCE {
        config.event_metrics.observe_sync(key.as_str())
    } else {
        None
    };
    if let (Some(previous), Some(surfaced)) = (previous_task, workspace.task_by_id(&task_id))
        && sync_surface_changed(&previous, surfaced)
    {
        let age_ms = if surfaced.updated_at > previous.updated_at {
            Some((Utc::now() - surfaced.updated_at).num_milliseconds().max(0) as u64)
        } else {
            observation_window_ms
        };
        let tier = config.poll.engagement_tier_for(key);
        if let Some(age_ms) = age_ms {
            match tier {
                EngagementTier::Hot => config.event_metrics.record_hot_sync_latency(age_ms),
                EngagementTier::Cold => config.event_metrics.record_cold_sync_latency(age_ms),
                EngagementTier::Warm => config.event_metrics.record_warm_sync_latency(age_ms),
            }
        }
        tracing::info!(
            task = %task_id,
            workspace_key = %key.as_str(),
            update_age_ms = ?age_ms,
            engagement_tier = tier.label(),
            "sync: surfaced changed task state"
        );
    }

    // Issue-collapse pass — see `merge_closing_issue_workspaces`.
    // Happens here (in prepare) so the migration step sees the
    // final session set and renames worktrees in one pass.
    let pending_merges = merge_closing_issue_workspaces(config, &mut workspace).await;
    Some((workspace, pending_merges))
}

fn sync_surface_changed(previous: &Task, incoming: &Task) -> bool {
    previous.updated_at < incoming.updated_at
        || previous.state != incoming.state
        || previous.ci != incoming.ci
        || previous.review != incoming.review
        || previous.mergeable != incoming.mergeable
        || previous.is_behind_base != incoming.is_behind_base
        || previous.auto_merge_enabled != incoming.auto_merge_enabled
        || previous.is_in_merge_queue != incoming.is_in_merge_queue
        || previous.additions != incoming.additions
        || previous.deletions != incoming.deletions
        || check_runs_changed(&previous.checks, &incoming.checks)
}

fn check_runs_changed(
    previous: &[lazybox_core::CheckRun],
    incoming: &[lazybox_core::CheckRun],
) -> bool {
    previous.len() != incoming.len()
        || previous.iter().zip(incoming).any(|(left, right)| {
            left.name != right.name || left.status != right.status || left.url != right.url
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    Changed,
    Unchanged,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CommitError {
    #[error("serialize workspace {key}: {source}")]
    SerializeWorkspace {
        key: String,
        source: serde_json::Error,
    },
    #[error("serialize project {key}: {source}")]
    SerializeProject {
        key: String,
        source: serde_json::Error,
    },
    #[error("serialize terminal metadata {backend_key}: {source}")]
    SerializeTerminalMetadata {
        backend_key: String,
        source: serde_json::Error,
    },
    #[error("workspace key mismatch: commit key {expected}, payload key {actual}")]
    KeyMismatch { expected: String, actual: String },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("workspace commit task failed: {0}")]
    Task(String),
}

pub(crate) fn report_commit_error(
    config: &ServerConfig,
    context: &'static str,
    error: &CommitError,
) {
    tracing::error!(context, error = %error, "durable workspace commit failed");
    let _ = config.bus.send(Event::provider_error_retryable(
        "store",
        format!("{context}: {error}"),
    ));
}

struct CommittedWorkspaceBatch {
    outcome: CommitOutcome,
    project_events: Vec<lazybox_core::Project>,
    workspace_events: Vec<Workspace>,
}

/// Persist one or more workspace rows, optional deletions, and related KV
/// mutations as one transaction. Event publication is deliberately separate:
/// cross-workspace session moves must update their in-memory terminal routing
/// after durability succeeds but before clients see the workspace upsert.
fn persist_workspace_batch(
    config: &ServerConfig,
    upserts: Vec<(WorkspaceKey, Workspace)>,
    deletes: Vec<WorkspaceKey>,
    extra_mutations: Vec<StoreMutation>,
) -> Result<CommittedWorkspaceBatch, CommitError> {
    let mut mutations = extra_mutations;
    let mut workspace_events = Vec::new();
    let mut project_events = Vec::new();
    let mut planned_projects = std::collections::HashSet::new();

    for (key, workspace) in upserts {
        if workspace.key != key {
            return Err(CommitError::KeyMismatch {
                expected: key.as_str().to_string(),
                actual: workspace.key.as_str().to_string(),
            });
        }

        if let Some((project, record)) = project_record_for_workspace(config, &workspace)?
            && planned_projects.insert(record.key.clone())
        {
            mutations.push(StoreMutation::SaveProject(record));
            project_events.push(project);
        }

        let json = serde_json::to_string(&workspace).map_err(|source| {
            CommitError::SerializeWorkspace {
                key: key.as_str().to_string(),
                source,
            }
        })?;
        let unchanged = config
            .store
            .get_workspace(&key)?
            .and_then(|record| record.workspace_json)
            .is_some_and(|previous| previous == json);
        if unchanged {
            tracing::trace!(
                workspace_key = %key.as_str(),
                "commit_upsert: unchanged — skipping workspace write + broadcast"
            );
        } else {
            mutations.push(StoreMutation::SaveWorkspace(WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: workspace.created_at,
                workspace_json: Some(json),
            }));
            workspace_events.push(workspace);
        }
    }

    mutations.extend(deletes.into_iter().map(StoreMutation::DeleteWorkspace));
    if mutations.is_empty() {
        return Ok(CommittedWorkspaceBatch {
            outcome: CommitOutcome::Unchanged,
            project_events,
            workspace_events,
        });
    }

    config.store.apply_batch(&mutations)?;
    Ok(CommittedWorkspaceBatch {
        outcome: CommitOutcome::Changed,
        project_events,
        workspace_events,
    })
}

fn publish_workspace_batch(config: &ServerConfig, committed: CommittedWorkspaceBatch) {
    for project in committed.project_events {
        let _ = config.bus.send(Event::ProjectUpserted(Box::new(project)));
    }
    for workspace in committed.workspace_events {
        let _ = config
            .bus
            .send(Event::WorkspaceUpserted(Box::new(workspace)));
    }
}

/// Persist one or more workspace rows and optional deletions as one atomic
/// store transaction, then publish the corresponding upserts. The bus is a
/// projection of durable state: no event is emitted until `apply_batch`
/// succeeds, so a client can never observe a commit that will vanish on the
/// next restart.
fn commit_workspace_batch(
    config: &ServerConfig,
    upserts: Vec<(WorkspaceKey, Workspace)>,
    deletes: Vec<WorkspaceKey>,
) -> Result<CommitOutcome, CommitError> {
    let committed = persist_workspace_batch(config, upserts, deletes, Vec::new())?;
    let outcome = committed.outcome;
    publish_workspace_batch(config, committed);
    Ok(outcome)
}

pub(crate) fn commit_upsert(
    config: &ServerConfig,
    key: &WorkspaceKey,
    workspace: Workspace,
) -> Result<CommitOutcome, CommitError> {
    commit_workspace_batch(config, vec![(key.clone(), workspace)], Vec::new())
}

/// [`commit_upsert`] with the store round-trip moved onto
/// `spawn_blocking` (issue #34's convention): the no-change compare
/// re-reads the row and the save rewrites it — synchronous rusqlite
/// that can pin a runtime worker for up to the 5s busy_timeout when
/// another process contends on the DB. Async callers use this; the
/// sync `commit_upsert` remains for the synchronous mutation helpers
/// (`mutate.rs`, startup migrations) that cannot await.
pub(crate) async fn commit_upsert_offloaded(
    config: &ServerConfig,
    key: &WorkspaceKey,
    workspace: Workspace,
) -> Result<CommitOutcome, CommitError> {
    let config_owned = config.clone();
    let key_owned = key.clone();
    tokio::task::spawn_blocking(move || commit_upsert(&config_owned, &key_owned, workspace))
        .await
        .map_err(|error| CommitError::Task(error.to_string()))?
}

pub(crate) fn commit_upsert_reported(
    config: &ServerConfig,
    key: &WorkspaceKey,
    workspace: Workspace,
    context: &'static str,
) {
    if let Err(error) = commit_upsert(config, key, workspace) {
        report_commit_error(config, context, &error);
    }
}

pub(crate) async fn commit_upsert_offloaded_reported(
    config: &ServerConfig,
    key: &WorkspaceKey,
    workspace: Workspace,
    context: &'static str,
) {
    if let Err(error) = commit_upsert_offloaded(config, key, workspace).await {
        report_commit_error(config, context, &error);
    }
}

struct TerminalRebadgePlan {
    from: lazybox_core::SessionKey,
    to: lazybox_core::SessionKey,
    terminal_ids: Vec<lazybox_ipc::TerminalId>,
}

fn prepare_terminal_rebadges(
    terminals: &std::collections::HashMap<lazybox_ipc::TerminalId, String>,
    terminal_meta: &std::collections::HashMap<
        lazybox_ipc::TerminalId,
        (lazybox_core::SessionKey, lazybox_ipc::TerminalKind),
    >,
    moves: Vec<(lazybox_core::SessionKey, lazybox_core::SessionKey)>,
) -> Result<(Vec<StoreMutation>, Vec<TerminalRebadgePlan>), CommitError> {
    let mut mutations = Vec::new();
    let mut plans = Vec::new();
    for (from, to) in moves {
        let mut terminal_ids = Vec::new();
        for (terminal_id, (session_key, kind)) in terminal_meta {
            if *session_key != from {
                continue;
            }
            let Some(backend_key) = terminals.get(terminal_id) else {
                // Teardown claims `terminals` first. A metadata-only entry is
                // already exiting and must not be resurrected durably here.
                continue;
            };
            let (key, value) =
                crate::spawn_handler::encode_terminal_meta_record(backend_key, &to, kind).map_err(
                    |source| CommitError::SerializeTerminalMetadata {
                        backend_key: backend_key.clone(),
                        source,
                    },
                )?;
            mutations.push(StoreMutation::SetKv { key, value });
            terminal_ids.push(*terminal_id);
        }
        if !terminal_ids.is_empty() {
            plans.push(TerminalRebadgePlan {
                from,
                to,
                terminal_ids,
            });
        }
    }
    Ok((mutations, plans))
}

/// Commit workspace rows and terminal ownership as one durable operation.
/// The terminal maps are co-held in canonical order so spawn/teardown cannot
/// cross the transaction boundary with a half-registered terminal. In-memory
/// routing and bus events change only after the store batch succeeds.
///
/// The blocking owner performs the transaction, map update, and entire event
/// tail. Dropping the async caller detaches that owner instead of cancelling it
/// between a successful SQLite commit and its in-memory/client projections.
pub(super) async fn commit_workspace_move(
    config: &ServerConfig,
    upserts: Vec<(WorkspaceKey, Workspace)>,
    deletes: Vec<WorkspaceKey>,
    terminal_moves: Vec<(lazybox_core::SessionKey, lazybox_core::SessionKey)>,
    post_commit_events: Vec<Event>,
    workspace_guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
) -> Result<CommitOutcome, CommitError> {
    let terminal_guards = if terminal_moves.is_empty() {
        None
    } else {
        let terminals = config.terminal.terminals.clone().lock_owned().await;
        let terminal_meta = config.terminal.terminal_meta.clone().lock_owned().await;
        Some((terminals, terminal_meta))
    };
    let config_owned = config.clone();
    tokio::task::spawn_blocking(move || {
        let mut terminal_guards = terminal_guards;
        let (terminal_mutations, rebadge_plans) = match terminal_guards.as_ref() {
            Some((terminals, terminal_meta)) => {
                prepare_terminal_rebadges(terminals, terminal_meta, terminal_moves)?
            }
            None => (Vec::new(), Vec::new()),
        };
        let committed =
            persist_workspace_batch(&config_owned, upserts, deletes, terminal_mutations)?;
        let outcome = committed.outcome;

        if let Some((_, terminal_meta)) = terminal_guards.as_mut() {
            for plan in rebadge_plans {
                let mut changed = false;
                for terminal_id in plan.terminal_ids {
                    if let Some((session_key, _)) = terminal_meta.get_mut(&terminal_id)
                        && *session_key == plan.from
                    {
                        *session_key = plan.to.clone();
                        changed = true;
                    }
                }
                if changed {
                    let _ = config_owned.bus.send(Event::TerminalsRebadged {
                        from: plan.from,
                        to: plan.to,
                    });
                }
            }
        }
        drop(terminal_guards);
        publish_workspace_batch(&config_owned, committed);
        for event in post_commit_events {
            let _ = config_owned.bus.send(event);
        }
        drop(workspace_guards);
        Ok(outcome)
    })
    .await
    .map_err(|error| CommitError::Task(error.to_string()))?
}

/// [`load_workspace`] on `spawn_blocking` — same offload rationale as
/// [`commit_upsert_offloaded`]. A join failure reads as "not found";
/// callers already treat that as a benign no-op.
pub(crate) async fn load_workspace_offloaded(
    config: &ServerConfig,
    key: &WorkspaceKey,
) -> Option<Workspace> {
    let config_owned = config.clone();
    let key_owned = key.clone();
    tokio::task::spawn_blocking(move || load_workspace(&config_owned, &key_owned))
        .await
        .ok()
        .flatten()
}

/// The user's effective GitHub scope ids from config.yaml: the wizard
/// selection (`setup.scopes`) unioned with any `providers.github.filters`
/// org/repo entries — the same set the poller narrows on (see the merge
/// in `sources_for`). Keeping the two in sync means a repo scoped either
/// way resolves its `owner/repo` identically.
pub(crate) fn github_scopes_from_config(
    cfg: &lazybox_config::Config,
) -> std::collections::BTreeSet<String> {
    let mut scopes = cfg.setup.scopes.get("github").cloned().unwrap_or_default();
    scopes.extend(github_scopes_from_filters(&cfg.providers.github.filters));
    scopes
}

/// The exact `owner/repo` slug for a GitHub project key, recovered from
/// the user's configured scopes (`github:owner/repo`). Non-lossy where
/// the flat `github-{owner}-{repo}` key isn't. `None` for non-github
/// keys (checked before any config read), unreadable config, or an
/// org-level subscription with no per-repo scope.
fn github_slug_from_config_scopes(key: &lazybox_core::ProjectKey) -> Option<String> {
    if key.source_prefix() != "github" {
        return None;
    }
    let cfg = lazybox_config::Config::load().ok()?;
    let scopes = github_scopes_from_config(&cfg);
    key.github_slug_from_scopes(scopes.iter().map(String::as_str))
}

fn github_slug_for_workspace(
    project_key: &lazybox_core::ProjectKey,
    workspace: &Workspace,
) -> Option<String> {
    if project_key.source_prefix() != "github" {
        return None;
    }
    workspace
        .primary_task()
        .and_then(|task| task.repo.as_deref())
        .filter(|slug| project_key.matches_github_slug(slug))
        .map(str::to_string)
        .or_else(|| github_slug_from_config_scopes(project_key))
        .or_else(|| project_key.unambiguous_github_slug())
}

/// Prepare the missing parent Project, if any, so it can be committed in the
/// same atomic batch as the workspace that references it.
fn project_record_for_workspace(
    config: &ServerConfig,
    workspace: &Workspace,
) -> Result<Option<(lazybox_core::Project, lazybox_store::ProjectRecord)>, CommitError> {
    let Some(project_key) = workspace.project_key.clone() else {
        return Ok(None);
    };
    let github_slug = github_slug_for_workspace(&project_key, workspace);
    if let Some(record) = config.store.get_project(&project_key)? {
        let Some(json) = record.project_json else {
            return Ok(None);
        };
        let Ok(mut project) = serde_json::from_str::<lazybox_core::Project>(&json) else {
            return Ok(None);
        };
        if project.github_repo().is_some() {
            return Ok(None);
        }
        let Some(slug) = github_slug else {
            return Ok(None);
        };
        if !project.set_github_repo(&slug) {
            return Ok(None);
        }
        let json =
            serde_json::to_string(&project).map_err(|source| CommitError::SerializeProject {
                key: project_key.as_str().to_string(),
                source,
            })?;
        return Ok(Some((
            project,
            lazybox_store::ProjectRecord {
                project_json: Some(json),
                ..record
            },
        )));
    }

    let project = if let Some(slug) = github_slug {
        let (owner, repo) = slug
            .split_once('/')
            .expect("validated GitHub slug contains owner/repo");
        lazybox_core::Project::github(owner, repo, Utc::now())
    } else {
        let name = workspace
            .primary_task()
            .and_then(|task| task.repo.clone())
            .unwrap_or_else(|| match project_key.source_prefix() {
                "github" => project_key.as_str().to_string(),
                _ => project_key.display_name(),
            });
        lazybox_core::Project::new(project_key.clone(), name, Utc::now())
    };
    let json = serde_json::to_string(&project).map_err(|source| CommitError::SerializeProject {
        key: project_key.as_str().to_string(),
        source,
    })?;
    let record = lazybox_store::ProjectRecord {
        key: project_key.as_str().to_string(),
        created_at: project.created_at,
        project_json: Some(json),
    };
    Ok(Some((project, record)))
}

/// Heuristic for "is this Task the PR side of a PR/issue pair?".
/// Single source of truth: [`Task::is_pr`] — same method
/// `workspace::classify` consults — so adding a new provider
/// (GitLab `/merge_requests/`, Bitbucket `/pull-requests/`, …)
/// only requires extending that one method.
fn is_pr_task(task: &Task) -> bool {
    task.is_pr()
}

pub(crate) fn load_workspace(config: &ServerConfig, key: &WorkspaceKey) -> Option<Workspace> {
    let record = config.store.get_workspace(key).ok().flatten()?;
    let json = record.workspace_json?;
    // Strict decode: a corrupt row OR one stamped by a newer build
    // reads as `None` here. Every caller treats `None` as "nothing I
    // can act on" — mutation paths become no-ops (preserving the row)
    // and rescope's final delete gate preserves rather than reaps.
    match Workspace::decode_persisted(&json) {
        Ok(ws) => Some(ws),
        Err(e) => {
            tracing::warn!(
                workspace_key = %key.as_str(),
                error = %e,
                "load_workspace: stored row unreadable — treating as absent (row preserved)"
            );
            None
        }
    }
}
