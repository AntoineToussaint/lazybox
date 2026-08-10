//! Workspace and project lifecycle operations owned by the daemon.

use crate::ServerConfig;
use crate::polling::{
    CommitError, commit_upsert, commit_upsert_offloaded_reported, commit_upsert_reported,
    load_workspace, load_workspace_offloaded, report_commit_error,
};
use chrono::Utc;
use lazybox_core::{Workspace, WorkspaceKey};
use lazybox_ipc::Event;
use lazybox_store::StoreMutation;

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
    let key = allocate_workspace_key(config, name);
    let mut workspace = Workspace::empty(key.clone(), "main", Utc::now());
    if !name.trim().is_empty() {
        workspace.name = name.trim().to_string();
    }
    workspace.project_key = Some(project_key);
    workspace.local = true;
    commit_upsert_reported(config, &key, workspace, "create empty workspace");
    key
}

/// Allocate a fresh, collision-free workspace key from a display name:
/// slugify, then try `<base>`, `<base>-2`, … until the store reports no
/// existing record. Falls back to `workspace` for an empty slug so the
/// key is always non-empty.
fn allocate_workspace_key(config: &ServerConfig, name: &str) -> WorkspaceKey {
    let base = lazybox_core::slug::slugify(name);
    let base = if base.is_empty() {
        "workspace".to_string()
    } else {
        base
    };
    (1..)
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
        .expect("infinite range yields a free key")
}

/// Import an on-disk checkout as a **linked (no-worktree) workspace**.
/// Re-describes `path` read-only to derive its `origin` repo and current
/// branch, then creates a workspace that points straight at `path` — no
/// worktree provisioned, no bare clone. A checkout whose `origin` maps to
/// a GitHub `owner/repo` lands under that repo's project so its
/// PR/issue/CI activity groups with it; one without a usable origin falls
/// back to a `local-<dir>` project. Returns the new key, or `None` when
/// `path` is no longer a git checkout (moved/deleted since the scan).
pub async fn import_local_checkout(
    config: &ServerConfig,
    path: std::path::PathBuf,
) -> Option<WorkspaceKey> {
    let Some(checkout) = lazybox_git_ops::describe_checkout_at(path.clone()).await else {
        let _ = config.bus.send(Event::provider_error_permanent(
            "import",
            format!("{} is no longer a git checkout", path.display()),
        ));
        return None;
    };

    let repo = checkout
        .remote_url
        .as_deref()
        .and_then(lazybox_core::github_owner_repo_from_url);
    let (project_key, name) = match repo {
        Some((owner, repo)) => (
            lazybox_core::ProjectKey::github(&owner, &repo),
            format!("{owner}/{repo}"),
        ),
        None => {
            let dir = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "checkout".to_string());
            (
                lazybox_core::ProjectKey::local(&lazybox_core::slug::slugify(&dir)),
                dir,
            )
        }
    };
    let branch = checkout.branch.unwrap_or_else(|| "main".to_string());
    Some(create_linked_workspace(
        config,
        &name,
        project_key,
        path,
        &branch,
    ))
}

/// Create a linked (no-worktree) workspace pointing at `path`. Sibling
/// of [`create_empty_workspace`]; the difference is `linked_checkout`
/// set (so the spawn path lands sessions in the existing checkout) and
/// the workspace's `branch` taken from the checkout's current branch
/// rather than a fixed `main`. `local = true` protects it from the
/// reconcile prune, like every hand-created workspace.
pub fn create_linked_workspace(
    config: &ServerConfig,
    name: &str,
    project_key: lazybox_core::ProjectKey,
    path: std::path::PathBuf,
    branch: &str,
) -> WorkspaceKey {
    let key = allocate_workspace_key(config, name);
    let mut workspace = Workspace::empty(key.clone(), branch, Utc::now());
    if !name.trim().is_empty() {
        workspace.name = name.trim().to_string();
    }
    workspace.project_key = Some(project_key);
    workspace.local = true;
    workspace.linked_checkout = Some(path);
    commit_upsert_reported(config, &key, workspace, "import linked checkout");
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
        Ok(None) => lazybox_core::Project::new(key.clone(), &display_name, Utc::now()),
        Err(error) => {
            let error = CommitError::Store(error);
            report_commit_error(config, "load local project", &error);
            return key;
        }
    };
    let json = match serde_json::to_string(&project) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                project_key = %key,
                "create_local_project: serde_json::to_string(project) failed: {e}",
            );
            return key;
        }
    };
    let record = lazybox_store::ProjectRecord {
        key: key.as_str().to_string(),
        created_at: project.created_at,
        project_json: Some(json),
    };
    if let Err(error) = config
        .store
        .apply_batch(&[StoreMutation::SaveProject(record)])
    {
        let error = CommitError::Store(error);
        report_commit_error(config, "create local project", &error);
        return key;
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
    if let Err(error) = commit_upsert(config, &ws_key, workspace) {
        report_commit_error(config, "migrate legacy sandbox", &error);
        return;
    }
    tracing::info!(
        "migrate_legacy_sandbox: moved `sandbox` workspace under `local-sandbox` project"
    );
}

/// Set or clear the workspace's `snoozed_until` timestamp. `None`
/// un-snoozes. Persists + broadcasts so the sidebar's mailbox-aware
/// rendering re-categorises the row.
pub async fn set_snooze(
    config: &ServerConfig,
    key: &WorkspaceKey,
    until: Option<chrono::DateTime<Utc>>,
) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace.snoozed_until = until;
    commit_upsert_offloaded_reported(config, key, workspace, "set workspace snooze").await;
}

/// Persist the workspace's free-form local note (issue #458). Mirrors
/// [`set_snooze`]: load, replace the field, commit (which persists the
/// JSON blob and broadcasts `WorkspaceUpserted` so every TUI sees the
/// new note). The note never leaves lazybox — no provider sync.
pub async fn set_notes(config: &ServerConfig, key: &WorkspaceKey, notes: String) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace.notes = notes;
    commit_upsert_offloaded_reported(config, key, workspace, "set workspace notes").await;
}

/// Rename a workspace's display name (issue #744). Mirrors
/// [`set_notes`]: load, replace the field, commit (persists the JSON blob
/// and broadcasts `WorkspaceUpserted` so every TUI updates the sidebar
/// label). Only the display `name` changes — the workspace key and any
/// session worktrees stay put, so nothing is orphaned. A blank name is
/// ignored so the row never renders empty; re-submitting the current
/// name emits nothing because `commit_upsert`'s no-change compare skips
/// the write + broadcast when the serialized row is identical.
pub async fn rename_workspace(config: &ServerConfig, key: &WorkspaceKey, name: String) {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return;
    }
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace.name = trimmed.to_string();
    commit_upsert_offloaded_reported(config, key, workspace, "rename workspace").await;
}

/// Record a snippet key as sent to a workspace's agent (issue #463).
/// Mirrors [`set_notes`]: load, push onto the MRU, commit (which
/// persists the JSON blob and broadcasts `WorkspaceUpserted` so every
/// TUI sees the updated per-session snippet history and its sidebar
/// indicator). Local-only — never synced to any provider.
pub async fn record_sent_snippet(config: &ServerConfig, key: &WorkspaceKey, snippet_key: String) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace.record_sent_snippet(snippet_key);
    commit_upsert_offloaded_reported(config, key, workspace, "record sent snippet").await;
}

/// Persist the workspace's "auto-merge on green" arm. Mirrors
/// [`set_snooze`]: load, flip the field, commit (which persists the
/// JSON blob and broadcasts `WorkspaceUpserted` so every TUI sees the
/// new arm state). The daemon owns the merge decision
/// ([`crate::polling::auto_merge`]).
///
/// Arming is **refused** when the merge-on-green author gate would
/// durably block the PR — a third party's PR whose author isn't opted
/// into `merge_on_green.allow_authors`. That decision is made HERE,
/// against the *daemon's* own config, so it's correct for every client
/// including a remote `--connect` session whose local config differs
/// (issue #845): the flag is left off and the reason is broadcast, so
/// the `ARM` pill never lights on a PR that could never merge. Only the
/// author gate refuses — transient CI / conflict / review states are
/// exactly what arming waits through, so those still arm.
pub async fn set_auto_merge_on_green(config: &ServerConfig, key: &WorkspaceKey, enabled: bool) {
    let policy = crate::polling::auto_merge::merge_on_green_policy();
    set_auto_merge_on_green_with_policy(config, key, enabled, &policy).await;
}

/// Policy-injecting core of [`set_auto_merge_on_green`], split out so a
/// test can pin the allowlist instead of reading the real config file.
async fn set_auto_merge_on_green_with_policy(
    config: &ServerConfig,
    key: &WorkspaceKey,
    enabled: bool,
    policy: &lazybox_core::MergeOnGreenPolicy,
) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    if enabled
        && !workspace.auto_merge_on_green
        && let Some(pr) = workspace.pr.as_ref()
        && lazybox_core::author_gate_blocks(pr, policy)
    {
        let label = pr.id.key.clone();
        let _ = config
            .bus
            .send(lazybox_ipc::Event::provider_error_retryable(
                "auto-merge",
                format!(
                    "won't arm merge-on-green for {label}: {} — add the author to \
                 merge_on_green.allow_authors to allow it",
                    lazybox_core::NON_AUTHOR_BLOCK
                ),
            ));
        return;
    }
    workspace.auto_merge_on_green = enabled;
    commit_upsert_offloaded_reported(config, key, workspace, "set auto-merge preference").await;
}

/// Persist the workspace's "track main" arm (issue #535). Mirrors
/// [`set_auto_merge_on_green`]: load, flip the field, commit (persists +
/// broadcasts `WorkspaceUpserted`). The actual fast-forwarding happens
/// in the background sweep ([`crate::polling::sync_tracked_workspaces`]); this only
/// records the intent. Disabling clears the stale "behind" badge so it
/// doesn't linger after the user stops tracking.
pub async fn set_track_main(config: &ServerConfig, key: &WorkspaceKey, enabled: bool) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace.track_main = enabled;
    if !enabled {
        workspace.track_main_behind = false;
    }
    commit_upsert_offloaded_reported(config, key, workspace, "set track-main preference").await;
}

/// Persist the workspace's per-session auto-fix arm for one
/// [`lazybox_core::AutoFixKind`] (issue #363). Mirrors
/// [`set_auto_merge_on_green`]: load, set the policy, commit (persists +
/// broadcasts `WorkspaceUpserted`). The auto-fix dispatcher reads the
/// stored arm back on the next fix candidate.
pub async fn set_auto_fix_policy(
    config: &ServerConfig,
    key: &WorkspaceKey,
    kind: lazybox_core::AutoFixKind,
    arm: lazybox_core::PolicyArm,
) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace.policies.set(kind, arm);
    commit_upsert_offloaded_reported(config, key, workspace, "set auto-fix policy").await;
}

/// Persist both per-session auto-fix arms in one workspace commit.
pub async fn set_auto_fix_policies(
    config: &ServerConfig,
    key: &WorkspaceKey,
    ci: lazybox_core::PolicyArm,
    conflict: lazybox_core::PolicyArm,
) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace
        .policies
        .set(lazybox_core::AutoFixKind::CiFailure, ci);
    workspace
        .policies
        .set(lazybox_core::AutoFixKind::MergeConflict, conflict);
    commit_upsert_offloaded_reported(config, key, workspace, "set auto-fix policies").await;
}

#[cfg(test)]
mod auto_fix_policy_tests {
    use super::*;

    #[tokio::test]
    async fn both_auto_fix_arms_commit_and_broadcast_together() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("github:o/r#705");
        let workspace = Workspace::empty(key.clone(), "auto-fix", Utc::now());
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: workspace.created_at,
                workspace_json: Some(serde_json::to_string(&workspace).expect("workspace json")),
            })
            .expect("seed workspace");
        let mut events = config.bus.subscribe();

        set_auto_fix_policies(
            &config,
            &key,
            lazybox_core::PolicyArm::Arm,
            lazybox_core::PolicyArm::Disarm,
        )
        .await;

        let Event::WorkspaceUpserted(workspace) = events.recv().await.expect("workspace update")
        else {
            panic!("expected WorkspaceUpserted");
        };
        assert_eq!(
            workspace.policies.arm(lazybox_core::AutoFixKind::CiFailure),
            lazybox_core::PolicyArm::Arm
        );
        assert_eq!(
            workspace
                .policies
                .arm(lazybox_core::AutoFixKind::MergeConflict),
            lazybox_core::PolicyArm::Disarm
        );
        assert!(
            events.try_recv().is_err(),
            "one atomic policy command emits one workspace update"
        );
        let stored = config
            .store
            .get_workspace(&key)
            .expect("read workspace")
            .expect("stored workspace");
        let stored: Workspace = serde_json::from_str(
            stored
                .workspace_json
                .as_deref()
                .expect("stored workspace json"),
        )
        .expect("decode workspace");
        assert_eq!(stored.policies, workspace.policies);
    }
}

#[cfg(test)]
mod rename_workspace_tests {
    use super::*;

    fn seed(config: &ServerConfig, key: &WorkspaceKey, name: &str) {
        let mut workspace = Workspace::empty(key.clone(), "scratch", Utc::now());
        workspace.name = name.to_string();
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: workspace.created_at,
                workspace_json: Some(serde_json::to_string(&workspace).expect("workspace json")),
            })
            .expect("seed workspace");
    }

    fn stored_name(config: &ServerConfig, key: &WorkspaceKey) -> String {
        let record = config
            .store
            .get_workspace(key)
            .expect("read workspace")
            .expect("stored workspace");
        let workspace: Workspace = serde_json::from_str(
            record
                .workspace_json
                .as_deref()
                .expect("stored workspace json"),
        )
        .expect("decode workspace");
        workspace.name
    }

    #[tokio::test]
    async fn rename_persists_and_broadcasts_the_new_name() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("local:scratch#1");
        seed(&config, &key, "Work");
        let mut events = config.bus.subscribe();

        rename_workspace(&config, &key, "  Rate limit spike  ".to_string()).await;

        let Event::WorkspaceUpserted(workspace) = events.recv().await.expect("workspace update")
        else {
            panic!("expected WorkspaceUpserted");
        };
        // Trimmed on the way in.
        assert_eq!(workspace.name, "Rate limit spike");
        // Key is left untouched so sessions/worktrees aren't orphaned.
        assert_eq!(workspace.key, key);
        assert_eq!(stored_name(&config, &key), "Rate limit spike");
    }

    #[tokio::test]
    async fn blank_rename_is_ignored() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("local:scratch#2");
        seed(&config, &key, "Work");
        let mut events = config.bus.subscribe();

        rename_workspace(&config, &key, "   ".to_string()).await;

        assert!(
            events.try_recv().is_err(),
            "a blank name must not commit or broadcast"
        );
        assert_eq!(stored_name(&config, &key), "Work");
    }

    #[tokio::test]
    async fn unchanged_rename_does_not_rebroadcast() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("local:scratch#3");
        seed(&config, &key, "Work");
        let mut events = config.bus.subscribe();

        // Submitting the current name (here with surrounding whitespace,
        // as an Enter-through on the prefilled input would) resolves to
        // the same stored name and must not force a write + broadcast.
        rename_workspace(&config, &key, "  Work  ".to_string()).await;

        assert!(
            events.try_recv().is_err(),
            "renaming to the current name must be a no-op"
        );
        assert_eq!(stored_name(&config, &key), "Work");
    }
}

/// Delete a workspace + all its sessions from the store. Broadcasts
/// `WorkspaceRemoved` so every connected TUI prunes its sidebar row.
/// Used by the sidebar's confirmed `x x` archive flow.
///
/// Reclaims the worktree directories on disk once the backing
/// terminals are dead and the store row is dropped (issue #575) — see
/// [`reclaim_workspace_worktrees`].
///
/// Also kills every backing terminal (PTY / tmux session) that
/// belonged to the workspace — without this the user's confirmed `x x`
/// hides the tabs in lazybox but leaves ghost tmux sessions visible
/// in `tmux ls`, which then re-surface on the next lazybox launch
/// via `recover_sessions`.
/// Strict read of the persisted archived set, distinguishing "no set
/// stored yet" (`Ok(empty)`) from "the set exists but could not be
/// read" (`Err`). The distinction matters for the read-modify-WRITE
/// callers ([`archive_workspace_key`] / [`unarchive_workspace_key`]):
/// treating one SQLITE_BUSY (or a corrupt payload) as an empty set and
/// then rewriting the row would replace the user's entire archive
/// history with a single element.
fn load_archived_set_strict(
    config: &ServerConfig,
) -> Result<std::collections::HashSet<String>, String> {
    let raw = config
        .store
        .get_kv(lazybox_core::KV_KEY_ARCHIVED)
        .map_err(|e| format!("read failed: {e}"))?;
    let Some(json) = raw else {
        return Ok(Default::default());
    };
    if json.trim().is_empty() {
        // Legacy empty payload — nothing stored, nothing to lose.
        return Ok(Default::default());
    }
    serde_json::from_str::<Vec<String>>(&json)
        .map(|v| v.into_iter().collect())
        .map_err(|e| format!("parse failed: {e}"))
}

/// Read the persisted set of archived workspace keys. Used by the
/// upsert path to skip re-creating a workspace the user explicitly
/// dismissed via `x x`. Returns an empty set when the kv entry
/// doesn't exist or fails to read — safe here because this consumer is
/// READ-ONLY and degrades gracefully (worst case the dismissed row
/// reappears one more time). Write paths must use
/// `load_archived_set_strict` instead.
pub fn load_archived_set(config: &ServerConfig) -> std::collections::HashSet<String> {
    load_archived_set_strict(config).unwrap_or_else(|e| {
        tracing::warn!("load_archived_set: {e} — treating as empty for read-only use");
        Default::default()
    })
}

/// Add `key` to the persisted archived set. Idempotent. Returns false when
/// persistence fails so a destructive caller can keep the workspace instead
/// of deleting it now and letting the next restart resurrect it.
///
/// A failed or unparseable READ of the existing set also returns false:
/// rewriting the row from a degraded read would wipe every previously
/// archived key. The caller's abort/rollback path already handles a
/// `false` archive.
#[must_use]
pub fn archive_workspace_key(config: &ServerConfig, key: &str) -> bool {
    let _update_guard = config.archive_updates.lock();
    let mut set = match load_archived_set_strict(config) {
        Ok(set) => set,
        Err(e) => {
            tracing::error!(
                "archive_workspace_key: existing archived set unreadable ({e}) — \
                 refusing to rewrite it; archive of {key} aborted"
            );
            return false;
        }
    };
    if !set.insert(key.to_string()) {
        return true;
    }
    let vec: Vec<&String> = set.iter().collect();
    let Ok(json) = serde_json::to_string(&vec) else {
        tracing::error!("archive_workspace_key: serialize failed");
        return false;
    };
    if let Err(e) = config.store.set_kv(lazybox_core::KV_KEY_ARCHIVED, &json) {
        tracing::warn!("archive_workspace_key: set_kv failed: {e}");
        return false;
    }
    true
}

/// Remove `key` from the persisted archived set so the next poll can
/// re-create the workspace. Clears the matching in-process spawn tombstone
/// only after persistence succeeds; otherwise an unarchived-but-still-deleted
/// workspace could race back into existence during this daemon run.
///
/// Same degraded-read contract as [`archive_workspace_key`]: an
/// unreadable existing set fails the operation instead of rewriting
/// (and thereby truncating) the stored history.
#[must_use]
pub fn unarchive_workspace_key(config: &ServerConfig, key: &str) -> bool {
    let _update_guard = config.archive_updates.lock();
    let mut set = match load_archived_set_strict(config) {
        Ok(set) => set,
        Err(e) => {
            tracing::error!(
                "unarchive_workspace_key: existing archived set unreadable ({e}) — \
                 refusing to rewrite it; unarchive of {key} aborted"
            );
            return false;
        }
    };
    if !set.remove(key) {
        config.deleted_workspaces.lock().remove(key);
        return true;
    }
    let vec: Vec<&String> = set.iter().collect();
    let Ok(json) = serde_json::to_string(&vec) else {
        tracing::error!("unarchive_workspace_key: serialize failed");
        return false;
    };
    if let Err(e) = config.store.set_kv(lazybox_core::KV_KEY_ARCHIVED, &json) {
        tracing::warn!("unarchive_workspace_key: set_kv failed: {e}");
        return false;
    }
    config.deleted_workspaces.lock().remove(key);
    true
}

/// Recursively sum the byte size of a directory tree. Best-effort: any
/// entry it can't stat is skipped, and symlinks are counted as their own
/// (near-zero) size rather than followed — so a symlinked directory can't
/// send the walk into a loop or double-count a target outside the tree.
/// The walk runs on `spawn_blocking` (sync `std::fs`) so a multi-GB
/// worktree can't pin an async runtime worker.
async fn dir_size(path: &std::path::Path) -> u64 {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        fn walk(dir: &std::path::Path) -> u64 {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return 0;
            };
            let mut total = 0u64;
            for entry in entries.flatten() {
                let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                if meta.is_dir() {
                    total += walk(&entry.path());
                } else {
                    total += meta.len();
                }
            }
            total
        }
        walk(&path)
    })
    .await
    .unwrap_or(0)
}

/// Human-readable byte size for the reclaimed-space notice.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// How much on-disk worktree space a teardown reclaimed. Threaded up to
/// the top-level user action so the "space came back" notice is emitted
/// once per action (aggregated across a project cascade, folded into the
/// closed-issue removal notice) rather than once per workspace.
#[derive(Debug, Default, Clone, Copy)]
pub struct Reclaimed {
    pub worktrees: usize,
    pub bytes: u64,
}

impl Reclaimed {
    fn add(&mut self, other: Reclaimed) {
        self.worktrees += other.worktrees;
        self.bytes += other.bytes;
    }
}

/// Human phrase for a reclaimed total, or `None` when nothing came back
/// (a session-less workspace). Omits the byte figure for empty worktrees
/// so the notice never reads "reclaimed 0 B".
pub(crate) fn reclaimed_notice_body(reclaimed: Reclaimed) -> Option<String> {
    if reclaimed.worktrees == 0 {
        return None;
    }
    let plural = if reclaimed.worktrees == 1 { "" } else { "s" };
    Some(if reclaimed.bytes == 0 {
        format!("removed {} worktree{plural}", reclaimed.worktrees)
    } else {
        format!(
            "reclaimed {} from {} worktree{plural}",
            format_bytes(reclaimed.bytes),
            reclaimed.worktrees,
        )
    })
}

/// Emit the reclaimed-space footer notice under `title`, if anything was
/// reclaimed. No-op otherwise.
pub(crate) fn notify_reclaimed(config: &ServerConfig, title: &str, reclaimed: Reclaimed) {
    if let Some(body) = reclaimed_notice_body(reclaimed) {
        let _ = config.bus.send(Event::Notification {
            title: title.to_string(),
            body,
        });
    }
}

/// Force-reclaim every persisted session's worktree directory for a
/// workspace being torn down, mirroring the per-session removal
/// [`handle_clean_worktrees`] performs: `remove_by_path` when the
/// upstream repo is known (so git's `worktrees/` index is pruned too),
/// falling back to `remove_dir_all` for repo-less scratch / pre-PR
/// checkouts. Unconditional — the delete path has already killed every
/// backing terminal, so there is no live checkout to protect, and
/// ephemeral on-main / linked checkouts are never persisted as sessions
/// (issue #452) so this never touches the user's real repo.
async fn reclaim_workspace_worktrees(config: &ServerConfig, workspace: &Workspace) -> Reclaimed {
    let mgr = config.worktree_manager();
    let bare_path = workspace
        .primary_task()
        .and_then(|t| t.repo.as_deref())
        .and_then(|repo| repo.split_once('/'))
        .map(|(owner, name)| mgr.bare_path(owner, name));

    let mut reclaimed = Reclaimed::default();
    for session in &workspace.sessions {
        let path = &session.worktree_path;
        if !path.exists() {
            continue;
        }
        let size = dir_size(path).await;
        if let Some(bare) = bare_path.as_ref() {
            let _ = mgr.remove_by_path(bare, path).await;
        } else {
            let _ = tokio::fs::remove_dir_all(path).await;
        }
        if path.exists() {
            tracing::warn!(
                worktree = %path.display(),
                "delete_workspace: worktree directory could not be reclaimed",
            );
            continue;
        }
        reclaimed.worktrees += 1;
        reclaimed.bytes += size;
    }

    tombstone_legacy_remote_host(config, workspace);

    reclaimed
}

/// Clear a legacy `remote-host:<workspace-key>` record left by the retired
/// #888 provision-on-open path. Nothing can stamp such a box anymore, but a
/// record written when that path was reachable can still point at a live GCE
/// instance — so drop the pointer and log the instance name for the operator
/// to reclaim by hand, rather than silently leaving both the record and a
/// possibly-running box behind.
fn tombstone_legacy_remote_host(config: &ServerConfig, workspace: &Workspace) {
    let kv_key = format!("remote-host:{}", workspace.key.as_str());
    let Ok(Some(record)) = config.store.get_kv(&kv_key) else {
        return;
    };
    tracing::warn!(
        workspace = workspace.key.as_str(),
        host = %describe_legacy_remote_host(&record),
        "clearing legacy remote-host record on delete; verify no GCE instance is left running",
    );
    let _ = config.store.delete_kv(&kv_key);
}

/// The GCE coordinates a legacy `remote-host` record names, formatted so the
/// delete log is actionable: reclaiming the box by hand is `gcloud compute
/// instances delete <instance> --zone <zone> --project <project>`, so all
/// three must be in the log, not just the instance name. Any field the
/// record lacks (or an unparseable record) reads as `<unknown>` rather than
/// dropping the warning entirely.
fn describe_legacy_remote_host(record: &str) -> String {
    let value =
        serde_json::from_str::<serde_json::Value>(record).unwrap_or(serde_json::Value::Null);
    let field = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unknown>")
            .to_string()
    };
    format!(
        "project={} zone={} instance={}",
        field("project"),
        field("zone"),
        field("instance"),
    )
}

#[cfg(test)]
mod tombstone_tests {
    use super::*;

    fn workspace(key: &WorkspaceKey) -> Workspace {
        Workspace::empty(key.clone(), "scratch", Utc::now())
    }

    #[test]
    fn clears_a_legacy_remote_host_record_and_leaves_none_behind() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("local:scratch#1");
        let kv_key = format!("remote-host:{}", key.as_str());
        config
            .store
            .set_kv(
                &kv_key,
                r#"{"project":"p","zone":"z","instance":"lazybox-old-box","id":"42"}"#,
            )
            .expect("seed legacy record");

        tombstone_legacy_remote_host(&config, &workspace(&key));

        assert_eq!(
            config.store.get_kv(&kv_key).expect("read kv"),
            None,
            "the legacy remote-host record must be cleared on delete"
        );
    }

    #[test]
    fn is_a_no_op_without_a_legacy_record() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("local:scratch#2");
        // No record seeded — must not panic or write anything.
        tombstone_legacy_remote_host(&config, &workspace(&key));
        assert_eq!(
            config
                .store
                .get_kv(&format!("remote-host:{}", key.as_str()))
                .expect("read kv"),
            None
        );
    }

    #[test]
    fn descriptor_names_project_zone_and_instance_for_reclamation() {
        // All three are needed to run `gcloud instances delete`; logging the
        // instance name alone leaves the operator unable to locate the box.
        let desc = describe_legacy_remote_host(
            r#"{"project":"internal-robin-dev","zone":"us-central1-a","instance":"lazybox-old-box","id":"42"}"#,
        );
        assert_eq!(
            desc,
            "project=internal-robin-dev zone=us-central1-a instance=lazybox-old-box"
        );
    }

    #[test]
    fn descriptor_degrades_to_unknown_fields_not_a_dropped_log() {
        // A malformed or partial record must still yield a warning-worthy
        // string rather than silently vanishing.
        assert_eq!(
            describe_legacy_remote_host("not json"),
            "project=<unknown> zone=<unknown> instance=<unknown>"
        );
        assert_eq!(
            describe_legacy_remote_host(r#"{"instance":"only-name"}"#),
            "project=<unknown> zone=<unknown> instance=only-name"
        );
    }
}

/// Delete a workspace, returning the worktree space reclaimed on success
/// or `None` when the row was preserved (a prerequisite failed). The
/// caller surfaces the reclaimed total via `notify_reclaimed`.
#[must_use]
pub async fn delete_workspace(config: &ServerConfig, key: &WorkspaceKey) -> Option<Reclaimed> {
    delete_workspace_with_archive(config, key, /*archive=*/ true).await
}

/// Like [`delete_workspace`] but with the archive decision explicit.
/// `archive=true` records the key in `KV_KEY_ARCHIVED` so the next poll
/// doesn't resurrect the row — the right choice for a user-intent
/// removal (`x x`, a confirmed merged/closed removal). `archive=false`
/// drops the row without archiving so a genuine upstream change can
/// re-create it: the closed-issue auto-remove (issue #552) uses this so
/// reopening the issue on GitHub brings its workspace back.
#[must_use]
pub async fn delete_workspace_with_archive(
    config: &ServerConfig,
    key: &WorkspaceKey,
    archive: bool,
) -> Option<Reclaimed> {
    // Own the delete-vs-spawn serialization here so every destructive caller
    // (single workspace, merged cleanup, project cascade) gets it. Keeping
    // this only in one command-dispatch arm let other callers race a late
    // spawn that recreated the terminal/worktree after deletion.
    config
        .deleted_workspaces
        .lock()
        .insert(key.as_str().to_string());
    crate::spawn_handler::await_inflight_spawns(&config.spawn, key.as_str()).await;
    let _workspace_guard = config.lock_workspace(key.as_str()).await;
    let reclaimed = delete_workspace_internal(config, key, archive).await;
    // The tombstone must not outlive the delete it guarded: a
    // recreated same-name workspace re-allocates the same key
    // (`allocate_workspace_key` only consults the store), and a stale
    // tombstone would silently kill every spawn on the new row. The
    // failure paths inside `delete_workspace_internal` already remove
    // it on rollback; the success path releases it here, once no
    // in-flight spawn that could still race the teardown remains (see
    // `release_delete_tombstone` for why that's the safe point).
    if reclaimed.is_some() {
        crate::spawn_handler::release_delete_tombstone(config, key.as_str());
    }
    reclaimed
}

/// Inner delete with the archive decision explicit. User-intent
/// deletes (`x x`, project cascade, merged-PR removal) archive so
/// the next poll doesn't resurrect the row. System-driven deletes
/// (rescope) must NOT archive: the workspace fell out of the polled
/// set for upstream/transient reasons (truncated query, scope edit, a
/// PR that closed and later reopens), and the archive guard in
/// `upsert` would permanently block it from ever being re-created.
pub(crate) async fn delete_workspace_internal(
    config: &ServerConfig,
    key: &WorkspaceKey,
    archive: bool,
) -> Option<Reclaimed> {
    let key_str = key.as_str();

    // Snapshot the sessions before the store row is dropped — deleting
    // the row also drops the session → worktree_path mapping we need to
    // reclaim the on-disk directories afterwards.
    let workspace_snapshot = load_workspace(config, key);

    // Find every terminal whose session_key matches via
    // terminal_meta — the authoritative wire-side mapping. Earlier
    // we parsed the backend_key prefix, but the backend's session
    // name format isn't part of any contract (tmux now uses
    // `lazybox-{repo}-{kind}-{pid}-{n}`); the meta map is. Locks are
    // taken + dropped before async backend.kill() calls.
    let to_kill_ids: Vec<lazybox_ipc::TerminalId> = {
        let meta = config.terminal.terminal_meta.lock().await;
        meta.iter()
            .filter(|(_, (sk, _))| sk.as_str() == key_str)
            .map(|(tid, _)| *tid)
            .collect()
    };
    let to_kill: Vec<(lazybox_ipc::TerminalId, String)> = {
        let terminals = config.terminal.terminals.lock().await;
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
            let Some(interaction) =
                crate::terminal_io::acquire_live(config, tid, &backend_key).await
            else {
                // The output pump won teardown after `to_kill` was
                // snapshotted. There is no live session left to signal.
                continue;
            };
            if let Err(e) = config.backend.kill(&backend_key).await {
                tracing::warn!("kill {backend_key}: {e}");
                let _ = config.bus.send(Event::provider_error_retryable(
                    "terminal",
                    format!(
                        "could not stop terminal {backend_key}; workspace {key} was not deleted: {e}"
                    ),
                ));
                // Preserve the workspace and every live mapping so the user
                // can retry. The backend contract deliberately keeps a slot
                // after a transport/timeout failure; deleting our metadata
                // here would orphan an agent we failed to stop. The client
                // rolls back its optimistic row removal off this "terminal"
                // error (#476).
                config.deleted_workspaces.lock().remove(key_str);
                return None;
            }
            drop(interaction);
            // One lifecycle owner handles every map, persisted terminal key,
            // AgentState::Exited, and TerminalExited. The output pump may
            // observe the child first or later; the owner's atomic claim
            // makes both orders idempotent and leaves backend release to the
            // pump that observed the real exit.
            crate::spawn_handler::detach_killed_terminal(config, tid, &backend_key).await;
        }
    }

    // Record the archive only after every requested terminal kill succeeded.
    // Otherwise a transient backend failure both keeps the workspace alive
    // and blocks the next poll from repairing/re-presenting it.
    if archive && !archive_workspace_key(config, key_str) {
        let _ = config.bus.send(Event::provider_error_retryable(
            "store",
            format!("could not archive workspace {key}; it was not deleted"),
        ));
        config.deleted_workspaces.lock().remove(key_str);
        return None;
    }

    if let Err(e) = config.store.delete_workspace(key) {
        tracing::warn!("delete_workspace failed: {e}");
        let rollback_ok = !archive || unarchive_workspace_key(config, key_str);
        if !rollback_ok {
            tracing::error!(
                workspace = %key,
                "delete_workspace rollback: could not remove archive tombstone",
            );
        }
        let _ = config.bus.send(Event::provider_error_retryable(
            "store",
            format!("could not delete workspace {key}: {e}"),
        ));
        if rollback_ok {
            config.deleted_workspaces.lock().remove(key_str);
        }
        return None;
    }
    let _ = config.bus.send(Event::WorkspaceRemoved(key.clone()));

    // The row (and its terminals) are gone — reclaim the worktree
    // directories on disk. Without this every teardown that routes
    // through delete_workspace (x x archive, project cascade,
    // rescope/retire) leaked multi-GB worktrees forever (issue #575).
    // The reclaimed total is returned, not announced here: the top-level
    // caller emits one notice per user action (see `notify_reclaimed`).
    let Some(workspace) = workspace_snapshot else {
        tracing::warn!(
            workspace = %key,
            "delete_workspace: no readable row to reclaim worktrees from",
        );
        return Some(Reclaimed::default());
    };
    let reclaimed = reclaim_workspace_worktrees(config, &workspace).await;
    if reclaimed.worktrees > 0 {
        tracing::info!(
            workspace = %key,
            worktrees = reclaimed.worktrees,
            bytes = reclaimed.bytes,
            "delete_workspace: reclaimed worktree directories",
        );
    }
    Some(reclaimed)
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
            tracing::error!(
                workspace = %record.key,
                project_key = %project_key,
                "delete_project: workspace record has no payload — refusing unsafe cascade",
            );
            let _ = config.bus.send(Event::provider_error_permanent(
                "store",
                format!(
                    "could not safely delete project {project_key}: workspace {} is unreadable",
                    record.key
                ),
            ));
            return;
        };
        let Ok(ws) = serde_json::from_str::<Workspace>(&json) else {
            tracing::error!(
                workspace = %record.key,
                project_key = %project_key,
                "delete_project: corrupt workspace payload — refusing unsafe cascade",
            );
            let _ = config.bus.send(Event::provider_error_permanent(
                "store",
                format!(
                    "could not safely delete project {project_key}: workspace {} is corrupt",
                    record.key
                ),
            ));
            return;
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
    let mut reclaimed = Reclaimed::default();
    for key in &child_keys {
        let Some(child) = delete_workspace(config, key).await else {
            tracing::warn!(
                project_key = %project_key,
                workspace = %key,
                "delete_project: child deletion failed — preserving project for retry",
            );
            return;
        };
        reclaimed.add(child);
    }

    if let Err(e) = config.store.delete_project(project_key) {
        tracing::warn!("delete_project store: {e}");
        let _ = config.bus.send(Event::provider_error_retryable(
            "store",
            format!("could not delete project {project_key}: {e}"),
        ));
        return;
    }
    let _ = config.bus.send(Event::ProjectRemoved(project_key.clone()));
    notify_reclaimed(config, "Project removed", reclaimed);
    tracing::info!(project_key = %project_key, "delete_project: done");
}

/// Persist a new `SessionLayout` for one session inside a workspace.
/// The user's tile arrangement (Tabs vs Splits with a tree) is local
/// to the workspace; this writes it through the store and broadcasts
/// `WorkspaceUpserted` so other clients see the new layout.
///
/// No-op when the workspace or session can't be found.
pub async fn set_session_layout(
    config: &ServerConfig,
    key: &WorkspaceKey,
    session_id: lazybox_core::SessionId,
    layout: lazybox_core::SessionLayout,
) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    let Some(session) = workspace.sessions.iter_mut().find(|s| s.id == session_id) else {
        tracing::debug!("set_session_layout: no session {session_id} in {key}");
        return;
    };
    session.layout = layout;
    commit_upsert_offloaded_reported(config, key, workspace, "set session layout").await;
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
///
/// `fingerprint` is the row's stable identity as the client saw it.
/// The daemon's list may have shifted since the client's snapshot (a
/// poll commits a new top-of-feed comment between the TUI resolving
/// the row and this command landing), so when a fingerprint is
/// present it is resolved against the CURRENT list — `index` only
/// serves as a position hint / same-content-twin disambiguator — and
/// a vanished row is a logged no-op rather than a mark on whatever
/// now occupies `index`.
pub async fn mark_activity_read(
    config: &ServerConfig,
    key: &WorkspaceKey,
    index: usize,
    fingerprint: Option<&lazybox_core::ActivityFingerprint>,
) {
    apply_activity_mark(config, key, index, fingerprint, /*read=*/ true).await;
}

/// Reverse of `mark_activity_read`. `z` undo binds here. The TUI
/// re-resolves the undo target against its latest snapshot before
/// sending, so this path still travels as a raw index.
pub async fn unmark_activity_read(config: &ServerConfig, key: &WorkspaceKey, index: usize) {
    apply_activity_mark(config, key, index, None, /*read=*/ false).await;
}

async fn apply_activity_mark(
    config: &ServerConfig,
    key: &WorkspaceKey,
    index: usize,
    fingerprint: Option<&lazybox_core::ActivityFingerprint>,
    read: bool,
) {
    // Lost-update guard: without it a poll tick's prepare→commit
    // window could overwrite this mark with the pre-mark copy it
    // loaded (see `upsert_into_workspace_key`).
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        tracing::debug!("apply_activity_mark: no record for {key}");
        return;
    };
    let index = match fingerprint {
        Some(fp) => match fp.resolve(&workspace.activity, index) {
            Some(resolved) => resolved,
            None => {
                tracing::debug!(
                    workspace_key = %key.as_str(),
                    index,
                    "apply_activity_mark: fingerprinted row not in current list — no-op"
                );
                return;
            }
        },
        None => index,
    };
    if read {
        workspace.mark_activity_read(index);
    } else {
        workspace.unmark_activity_read(index);
    }
    commit_upsert_offloaded_reported(config, key, workspace, "mark workspace activity").await;
}

/// Apply the user's "mark every activity item read" gesture to a
/// stored workspace and broadcast the change. Activity-seen state is
/// **independent** of the upstream provider state: providers only ever
/// rewrite the activity feed itself; `seen_count` + `read_indices`
/// belong to the local user. Preserving them across polls happens in
/// `upsert`; this function flips them all-read on demand.
///
/// No-op if the workspace isn't in the store.
pub async fn mark_workspace_read(config: &ServerConfig, key: &WorkspaceKey) {
    // Lost-update guard: serializes against the poll tick's
    // prepare→commit on the same row, which used to be able to save a
    // pre-mark copy over this write (regression test:
    // `workspace_lock_tests::tick_merge_cannot_revert_concurrent_mark_read`).
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        tracing::debug!("mark_workspace_read: no record for {key}");
        return;
    };
    workspace.mark_read_all();
    workspace.last_viewed_at = Some(Utc::now());
    commit_upsert_offloaded_reported(config, key, workspace, "mark workspace read").await;
}

#[cfg(test)]
mod archived_set_degraded_read_tests {
    //! Fix for the archived-set wipe: `archive_workspace_key` /
    //! `unarchive_workspace_key` are read-modify-writes of the whole
    //! `archived_workspaces_v1` row. One failing or unparseable READ
    //! used to be treated as "empty set", so the follow-up write
    //! replaced the user's entire archive history with a single
    //! element. Degraded reads must fail the operation instead.
    use super::*;
    use lazybox_store::{MemoryStore, Store, StoreError, StoreMutation};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FlakyArchiveStore {
        inner: MemoryStore,
        fail_archived_reads: AtomicBool,
    }

    impl Store for FlakyArchiveStore {
        fn apply_batch(&self, mutations: &[StoreMutation]) -> Result<(), StoreError> {
            self.inner.apply_batch(mutations)
        }
        fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
            if key == lazybox_core::KV_KEY_ARCHIVED
                && self.fail_archived_reads.load(Ordering::SeqCst)
            {
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

    fn seeded_config() -> (Arc<FlakyArchiveStore>, ServerConfig) {
        let store = Arc::new(FlakyArchiveStore {
            inner: MemoryStore::new(),
            fail_archived_reads: AtomicBool::new(false),
        });
        store
            .inner
            .set_kv(
                lazybox_core::KV_KEY_ARCHIVED,
                r#"["old-1","old-2","old-3"]"#,
            )
            .unwrap();
        let config = ServerConfig::with_store(store.clone());
        (store, config)
    }

    fn persisted_set(store: &FlakyArchiveStore) -> std::collections::HashSet<String> {
        serde_json::from_str::<Vec<String>>(
            &store
                .inner
                .get_kv(lazybox_core::KV_KEY_ARCHIVED)
                .unwrap()
                .unwrap(),
        )
        .unwrap()
        .into_iter()
        .collect()
    }

    #[test]
    fn failing_read_during_archive_must_not_shrink_the_persisted_set() {
        let (store, config) = seeded_config();
        store.fail_archived_reads.store(true, Ordering::SeqCst);

        assert!(
            !crate::workspace::archive_workspace_key(&config, "new-key"),
            "archive must fail loudly on a degraded read"
        );

        store.fail_archived_reads.store(false, Ordering::SeqCst);
        let set = persisted_set(&store);
        assert_eq!(
            set,
            ["old-1", "old-2", "old-3"]
                .into_iter()
                .map(String::from)
                .collect(),
            "the historical archived set must survive the failed attempt"
        );
    }

    #[test]
    fn corrupt_archived_payload_fails_archive_without_rewriting() {
        let (store, config) = seeded_config();
        store
            .inner
            .set_kv(lazybox_core::KV_KEY_ARCHIVED, "{definitely not json")
            .unwrap();

        assert!(!crate::workspace::archive_workspace_key(&config, "new-key"));
        assert_eq!(
            store
                .inner
                .get_kv(lazybox_core::KV_KEY_ARCHIVED)
                .unwrap()
                .as_deref(),
            Some("{definitely not json"),
            "an unparseable set is preserved for recovery, never replaced"
        );
    }

    #[test]
    fn failing_read_during_unarchive_must_not_wipe_the_set() {
        let (store, config) = seeded_config();
        store.fail_archived_reads.store(true, Ordering::SeqCst);
        assert!(!crate::workspace::unarchive_workspace_key(&config, "old-2"));
        store.fail_archived_reads.store(false, Ordering::SeqCst);
        assert_eq!(persisted_set(&store).len(), 3);
    }

    #[test]
    fn archive_still_works_on_a_healthy_store() {
        let (store, config) = seeded_config();
        assert!(crate::workspace::archive_workspace_key(&config, "new-key"));
        let set = persisted_set(&store);
        assert_eq!(set.len(), 4);
        assert!(set.contains("new-key"));
        assert!(crate::workspace::unarchive_workspace_key(
            &config, "new-key"
        ));
        assert_eq!(persisted_set(&store).len(), 3);
    }
}

/// The daemon owns the merge-on-green arm-time author gate (issue #845):
/// arming a third party's PR that isn't opted into the *daemon's*
/// `merge_on_green.allow_authors` is refused with a reason, so a remote
/// client whose local config differs can't wrongly pre-refuse or
/// pre-accept it. These pin the policy directly rather than reading the
/// real config file.
#[cfg(test)]
mod set_auto_merge_on_green_tests {
    use super::*;
    use lazybox_core::{
        CiStatus, MergeOnGreenPolicy, Mergeable, ReviewStatus, Task, TaskId, TaskKind, TaskRole,
        TaskState,
    };

    fn pr_task(key: &str, role: TaskRole, author: &str) -> Task {
        let repo = key.rsplit_once('#').map(|(p, _)| p).unwrap_or("o/r");
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: TaskState::Open,
            role,
            author: author.into(),
            ci: CiStatus::Success,
            review: ReviewStatus::Approved,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{repo}/pull/1"),
            repo: Some(repo.into()),
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
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: Some(TaskKind::Pr),
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    /// Seed a PR workspace and return the key it was stored under — the
    /// key [`Workspace::from_task`] derives, which is also the key the
    /// commit persists to (a hand-written key would miss the re-store).
    fn seed(config: &ServerConfig, task: Task, armed: bool) -> WorkspaceKey {
        let mut ws = Workspace::from_task(task, Utc::now());
        ws.auto_merge_on_green = armed;
        let key = ws.key.clone();
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).unwrap()),
            })
            .unwrap();
        key
    }

    fn stored_arm(config: &ServerConfig, key: &WorkspaceKey) -> bool {
        let record = config.store.get_workspace(key).unwrap().unwrap();
        serde_json::from_str::<Workspace>(record.workspace_json.as_deref().unwrap())
            .unwrap()
            .auto_merge_on_green
    }

    #[tokio::test]
    async fn refuses_arming_a_non_opted_in_third_party_pr() {
        let config = ServerConfig::in_memory();
        let key = seed(
            &config,
            pr_task("o/r#1", TaskRole::Reviewer, "dependabot[bot]"),
            false,
        );
        let mut rx = config.bus.subscribe();

        set_auto_merge_on_green_with_policy(&config, &key, true, &MergeOnGreenPolicy::default())
            .await;

        assert!(!stored_arm(&config, &key), "the arm flag must stay off");
        match rx.try_recv().expect("a refusal notice") {
            Event::ProviderError {
                source, message, ..
            } => {
                assert_eq!(source, "auto-merge");
                assert!(message.contains("your own PRs"), "{message}");
            }
            other => panic!("expected a ProviderError notice, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn arms_an_opted_in_third_party_pr() {
        let config = ServerConfig::in_memory();
        let key = seed(
            &config,
            pr_task("o/r#2", TaskRole::Reviewer, "dependabot[bot]"),
            false,
        );

        // `dependabot` opted in — the `[bot]` suffix is normalized away.
        let policy = MergeOnGreenPolicy::from_allow_authors(["dependabot"]);
        set_auto_merge_on_green_with_policy(&config, &key, true, &policy).await;

        assert!(stored_arm(&config, &key), "an opted-in author arms");
    }

    #[tokio::test]
    async fn arms_your_own_pr_under_the_default_policy() {
        let config = ServerConfig::in_memory();
        let key = seed(&config, pr_task("o/r#3", TaskRole::Author, "me"), false);

        set_auto_merge_on_green_with_policy(&config, &key, true, &MergeOnGreenPolicy::default())
            .await;

        assert!(stored_arm(&config, &key), "own PRs arm normally");
    }

    #[tokio::test]
    async fn disarming_a_non_own_pr_is_never_refused() {
        let config = ServerConfig::in_memory();
        let key = seed(
            &config,
            pr_task("o/r#4", TaskRole::Reviewer, "dependabot[bot]"),
            true,
        );

        set_auto_merge_on_green_with_policy(&config, &key, false, &MergeOnGreenPolicy::default())
            .await;

        assert!(
            !stored_arm(&config, &key),
            "the guard only gates enabling, never disarming"
        );
    }
}
