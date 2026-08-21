//! Workspace and project lifecycle operations owned by the daemon.

use crate::ServerConfig;
use crate::polling::{
    CommitError, commit_upsert, commit_upsert_offloaded_reported, load_workspace,
    load_workspace_offloaded, report_commit_error,
};
use chrono::Utc;
use lazybox_core::{HopperMeta, Workspace, WorkspaceKey};
use lazybox_ipc::{Event, HopperEntryDraft};
use lazybox_store::{StoreError, StoreMutation, WorkspaceRecord};
use std::collections::{HashMap, HashSet};

/// Failure to create a durable workspace record. Creation is a user-issued
/// command, so callers must propagate this result instead of returning a key
/// for a row that may never have reached the store.
#[derive(Debug, thiserror::Error)]
pub enum CreateWorkspaceError {
    #[error("allocate a collision-free workspace key: {0}")]
    Allocate(#[source] StoreError),
    #[error("persist workspace: {0}")]
    Persist(String),
}

/// Failure to persist an ordered Hopper edit.
#[derive(Debug, thiserror::Error)]
pub enum SaveHopperError {
    #[error("load workspaces: {0}")]
    Load(#[source] StoreError),
    #[error("hopper entry names cannot be empty")]
    EmptyName,
    #[error("hopper workspace does not exist: {0}")]
    Missing(WorkspaceKey),
    #[error("workspace is not a hopper entry: {0}")]
    NotHopper(WorkspaceKey),
    #[error("hopper workspace appeared more than once: {0}")]
    Duplicate(WorkspaceKey),
    #[error("serialize hopper workspace {key}: {source}")]
    Serialize {
        key: WorkspaceKey,
        #[source]
        source: serde_json::Error,
    },
    #[error("persist hopper edit: {0}")]
    Persist(#[source] StoreError),
}

/// Atomically create, rename, and reorder the active personal Hopper.
///
/// Existing entries omitted by the client are retained after the submitted
/// rows. That is deliberate: saving text is never an implicit delete.
pub fn save_hopper(
    config: &ServerConfig,
    entries: Vec<HopperEntryDraft>,
) -> Result<Vec<WorkspaceKey>, SaveHopperError> {
    let _creation_guard = config.workspace_creations.lock();
    let records = config
        .store
        .list_workspaces()
        .map_err(SaveHopperError::Load)?;
    let mut workspaces = HashMap::<WorkspaceKey, Workspace>::new();
    let mut used = HashSet::<String>::new();
    for record in records {
        used.insert(record.key.clone());
        let Some(json) = record.workspace_json else {
            continue;
        };
        if let Ok(workspace) = Workspace::decode_persisted(&json) {
            workspaces.insert(workspace.key.clone(), workspace);
        }
    }

    let mut ordered = Vec::<Workspace>::new();
    let mut seen = HashSet::<WorkspaceKey>::new();
    for (position, draft) in entries.into_iter().enumerate() {
        let name = draft.name.trim();
        if name.is_empty() {
            return Err(SaveHopperError::EmptyName);
        }
        let mut workspace = if let Some(key) = draft.workspace_key {
            if !seen.insert(key.clone()) {
                return Err(SaveHopperError::Duplicate(key));
            }
            let Some(workspace) = workspaces.remove(&key) else {
                return Err(SaveHopperError::Missing(key));
            };
            if workspace.hopper.is_none() {
                return Err(SaveHopperError::NotHopper(workspace.key));
            }
            workspace
        } else {
            let base = {
                let slug = lazybox_core::slug::slugify(name);
                if slug.is_empty() {
                    "hopper-item".to_string()
                } else {
                    slug
                }
            };
            let mut suffix = 1u32;
            let key = loop {
                let candidate = if suffix == 1 {
                    base.clone()
                } else {
                    format!("{base}-{suffix}")
                };
                if used.insert(candidate.clone()) {
                    break WorkspaceKey::new(candidate);
                }
                suffix += 1;
            };
            seen.insert(key.clone());
            let mut workspace = Workspace::empty(key, "main", Utc::now());
            workspace.local = true;
            workspace.hopper = Some(HopperMeta {
                position: position as u32,
                completed_at: None,
            });
            workspace
        };
        workspace.name = name.to_string();
        let completed_at = workspace.hopper.and_then(|meta| meta.completed_at);
        workspace.hopper = Some(HopperMeta {
            position: position as u32,
            completed_at,
        });
        ordered.push(workspace);
    }

    let mut omitted: Vec<Workspace> = workspaces
        .into_values()
        .filter(|workspace| workspace.hopper.is_some())
        .collect();
    omitted.sort_by_key(|workspace| {
        (
            workspace
                .hopper
                .map(|meta| meta.position)
                .unwrap_or(u32::MAX),
            workspace.created_at,
            workspace.key.as_str().to_string(),
        )
    });
    for mut workspace in omitted {
        let completed_at = workspace.hopper.and_then(|meta| meta.completed_at);
        workspace.hopper = Some(HopperMeta {
            position: ordered.len() as u32,
            completed_at,
        });
        ordered.push(workspace);
    }

    let mutations = ordered
        .iter()
        .map(|workspace| {
            let json =
                serde_json::to_string(workspace).map_err(|source| SaveHopperError::Serialize {
                    key: workspace.key.clone(),
                    source,
                })?;
            Ok(StoreMutation::SaveWorkspace(WorkspaceRecord {
                key: workspace.key.as_str().to_string(),
                created_at: workspace.created_at,
                workspace_json: Some(json),
            }))
        })
        .collect::<Result<Vec<_>, SaveHopperError>>()?;
    config
        .store
        .apply_batch(&mutations)
        .map_err(SaveHopperError::Persist)?;

    let keys = ordered
        .iter()
        .map(|workspace| workspace.key.clone())
        .collect();
    for workspace in ordered {
        let _ = config
            .bus
            .send(Event::WorkspaceUpserted(std::sync::Arc::new(workspace)));
    }
    Ok(keys)
}

#[cfg(test)]
mod hopper_tests {
    use super::*;

    fn load(config: &ServerConfig, key: &WorkspaceKey) -> Workspace {
        let record = config
            .store
            .get_workspace(key)
            .expect("read hopper workspace")
            .expect("hopper workspace exists");
        Workspace::decode_persisted(
            record
                .workspace_json
                .as_deref()
                .expect("hopper workspace json"),
        )
        .expect("decode hopper workspace")
    }

    #[test]
    fn save_hopper_creates_then_renames_and_reorders_stable_workspaces() {
        let config = ServerConfig::in_memory();
        let created = save_hopper(
            &config,
            vec![
                HopperEntryDraft {
                    workspace_key: None,
                    name: "Write plan".into(),
                },
                HopperEntryDraft {
                    workspace_key: None,
                    name: "Fix tests".into(),
                },
            ],
        )
        .expect("create hopper");
        assert_eq!(created.len(), 2);
        assert!(load(&config, &created[0]).sessions.is_empty());
        assert!(load(&config, &created[0]).project_key.is_none());

        save_hopper(
            &config,
            vec![
                HopperEntryDraft {
                    workspace_key: Some(created[1].clone()),
                    name: "Fix all tests".into(),
                },
                HopperEntryDraft {
                    workspace_key: Some(created[0].clone()),
                    name: "Write plan".into(),
                },
            ],
        )
        .expect("edit hopper");

        let first = load(&config, &created[1]);
        let second = load(&config, &created[0]);
        assert_eq!(first.name, "Fix all tests");
        assert_eq!(first.hopper.expect("hopper metadata").position, 0);
        assert_eq!(second.hopper.expect("hopper metadata").position, 1);
    }

    #[test]
    fn omitted_rows_are_preserved_instead_of_deleted() {
        let config = ServerConfig::in_memory();
        let created = save_hopper(
            &config,
            vec![
                HopperEntryDraft {
                    workspace_key: None,
                    name: "One".into(),
                },
                HopperEntryDraft {
                    workspace_key: None,
                    name: "Two".into(),
                },
            ],
        )
        .expect("create hopper");
        save_hopper(
            &config,
            vec![HopperEntryDraft {
                workspace_key: Some(created[0].clone()),
                name: "One renamed".into(),
            }],
        )
        .expect("save partial edit");
        assert_eq!(load(&config, &created[1]).name, "Two");
        assert_eq!(
            load(&config, &created[1])
                .hopper
                .expect("hopper metadata")
                .position,
            1
        );
    }

    #[tokio::test]
    async fn project_assignment_is_one_time_and_hopper_only() {
        let config = ServerConfig::in_memory();
        let [key] = save_hopper(
            &config,
            vec![HopperEntryDraft {
                workspace_key: None,
                name: "Ship it".into(),
            }],
        )
        .expect("create hopper")
        .try_into()
        .expect("one hopper row");
        let first = lazybox_core::ProjectKey::local("first");
        let second = lazybox_core::ProjectKey::local("second");
        for project in [
            lazybox_core::Project::new(first.clone(), "first", Utc::now()),
            lazybox_core::Project::new(second.clone(), "second", Utc::now()),
        ] {
            config
                .store
                .save_project(&lazybox_store::ProjectRecord {
                    key: project.key.as_str().to_string(),
                    created_at: project.created_at,
                    project_json: Some(serde_json::to_string(&project).expect("serialize project")),
                })
                .expect("save project");
        }

        assign_hopper_project(&config, &key, first.clone()).await;
        assign_hopper_project(&config, &key, second).await;

        assert_eq!(load(&config, &key).project_key, Some(first));
    }

    #[tokio::test]
    async fn completion_is_reversible_and_preserves_workspace_identity() {
        let config = ServerConfig::in_memory();
        let [key] = save_hopper(
            &config,
            vec![HopperEntryDraft {
                workspace_key: None,
                name: "Write release notes".into(),
            }],
        )
        .expect("create hopper")
        .try_into()
        .expect("one hopper row");

        set_hopper_completed(&config, &key, true).await;
        let completed = load(&config, &key);
        assert!(
            completed
                .hopper
                .expect("hopper metadata")
                .completed_at
                .is_some()
        );

        set_hopper_completed(&config, &key, false).await;
        let reopened = load(&config, &key);
        assert_eq!(reopened.key, key);
        assert!(
            reopened
                .hopper
                .expect("hopper metadata")
                .completed_at
                .is_none()
        );
    }
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
) -> Result<WorkspaceKey, CreateWorkspaceError> {
    let _creation_guard = config.workspace_creations.lock();
    let key = allocate_workspace_key(config, name).map_err(|error| {
        report_workspace_create_error(config, "allocate workspace key", &error);
        CreateWorkspaceError::Allocate(error)
    })?;
    let mut workspace = Workspace::empty(key.clone(), "main", Utc::now());
    if !name.trim().is_empty() {
        workspace.name = name.trim().to_string();
    }
    workspace.project_key = Some(project_key);
    workspace.local = true;
    if let Err(error) = commit_upsert(config, &key, workspace) {
        report_commit_error(config, "create empty workspace", &error);
        return Err(CreateWorkspaceError::Persist(error.to_string()));
    }
    Ok(key)
}

/// Allocate a fresh, collision-free workspace key from a display name:
/// slugify, then try `<base>`, `<base>-2`, … until the store reports no
/// existing record. Falls back to `workspace` for an empty slug so the
/// key is always non-empty.
fn allocate_workspace_key(config: &ServerConfig, name: &str) -> Result<WorkspaceKey, StoreError> {
    let base = lazybox_core::slug::slugify(name);
    let base = if base.is_empty() {
        "workspace".to_string()
    } else {
        base
    };
    for i in 1.. {
        let key = if i == 1 {
            WorkspaceKey::new(base.clone())
        } else {
            WorkspaceKey::new(format!("{base}-{i}"))
        };
        if config
            .store
            .get_workspace(&key)?
            .and_then(|record| record.workspace_json)
            .is_none()
        {
            return Ok(key);
        }
    }
    unreachable!("an unbounded numeric suffix always yields a workspace key")
}

fn report_workspace_create_error(config: &ServerConfig, context: &'static str, error: &StoreError) {
    tracing::error!(context, error = %error, "workspace creation failed");
    let _ = config.bus.send(Event::provider_error_retryable(
        "store",
        format!("{context}: {error}"),
    ));
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
    create_linked_workspace(config, &name, project_key, path, &branch).ok()
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
) -> Result<WorkspaceKey, CreateWorkspaceError> {
    let _creation_guard = config.workspace_creations.lock();
    let key = allocate_workspace_key(config, name).map_err(|error| {
        report_workspace_create_error(config, "allocate linked workspace key", &error);
        CreateWorkspaceError::Allocate(error)
    })?;
    let mut workspace = Workspace::empty(key.clone(), branch, Utc::now());
    if !name.trim().is_empty() {
        workspace.name = name.trim().to_string();
    }
    workspace.project_key = Some(project_key);
    workspace.local = true;
    workspace.linked_checkout = Some(path);
    if let Err(error) = commit_upsert(config, &key, workspace) {
        report_commit_error(config, "import linked checkout", &error);
        return Err(CreateWorkspaceError::Persist(error.to_string()));
    }
    Ok(key)
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
    wake: Option<lazybox_core::SnoozeWake>,
) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace.snoozed_until = until;
    // The wake condition rides the snooze it belongs to: setting a new
    // snooze replaces it; un-snoozing (until = None) clears it. A
    // MANUAL un-snooze also clears any woke stamp — the user is
    // already looking at the row, so announcing the re-entry (#scale)
    // would be noise.
    workspace.snooze_wake = if until.is_some() { wake } else { None };
    if until.is_none() {
        workspace.woke_at = None;
    }
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

/// Attach a repo-less Hopper workspace to a tracked project. Provider-backed
/// workspaces cannot be retargeted through this local-only command, and an
/// existing assignment is preserved so two clients cannot silently move the
/// same workspace between repos. The normal upsert echo is the durability
/// acknowledgement used by the TUI to resume the pending spawn/editor action.
pub async fn assign_hopper_project(
    config: &ServerConfig,
    key: &WorkspaceKey,
    project_key: lazybox_core::ProjectKey,
) {
    if config
        .store
        .get_project(&project_key)
        .ok()
        .flatten()
        .is_none()
    {
        return;
    }
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    if workspace.hopper.is_none() || workspace.project_key.is_some() {
        return;
    }
    workspace.project_key = Some(project_key);
    commit_upsert_offloaded_reported(config, key, workspace, "assign hopper project").await;
}

/// Complete or reopen a Hopper item without touching sessions, terminal
/// history, or its checkout. Mailbox classification treats the timestamp as
/// an Inactive marker; clearing it restores the same workspace to Inbox.
pub async fn set_hopper_completed(config: &ServerConfig, key: &WorkspaceKey, completed: bool) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    let Some(mut hopper) = workspace.hopper else {
        return;
    };
    hopper.completed_at = completed.then(Utc::now);
    workspace.hopper = Some(hopper);
    commit_upsert_offloaded_reported(config, key, workspace, "set hopper completion").await;
}

/// Record a snippet delivery against a workspace (issue #463): the
/// authoritative, persisted half of the delivery transition. Mirrors
/// [`set_notes`] — load, apply the single [`Workspace::record_snippet_delivery`]
/// transition (which bumps the honest count and the MRU together), commit
/// (persist the JSON blob and broadcast `WorkspaceUpserted` so every TUI
/// sees the new count + indicator). Local-only — never synced.
///
/// Returns whether the workspace existed and the transition was applied.
/// A `false` means there was no workspace row to record into (e.g. a
/// session-less broadcast spawn); the caller decides what that implies
/// for the softer, non-authoritative projections.
pub async fn record_snippet_delivery(
    config: &ServerConfig,
    key: &WorkspaceKey,
    snippet_key: String,
) -> bool {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return false;
    };
    workspace.record_snippet_delivery(snippet_key);
    commit_upsert_offloaded_reported(config, key, workspace, "record snippet delivery").await;
    true
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

/// Persist the workspace's metering opt-in (the `$ meter` canary). Mirrors
/// [`set_track_main`]: load, set the flag, commit (persists + broadcasts
/// `WorkspaceUpserted` so the sidebar badge refreshes). The spawn path reads
/// it back to route the next spawn through the metering proxy.
pub async fn set_metered(config: &ServerConfig, key: &WorkspaceKey, enabled: bool) {
    let _ws_guard = config.lock_workspace(key.as_str()).await;
    let Some(mut workspace) = load_workspace_offloaded(config, key).await else {
        return;
    };
    workspace.metered = enabled;
    commit_upsert_offloaded_reported(config, key, workspace, "set metering preference").await;
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

/// One persisted worktree that prevents a workspace lifecycle removal.
/// Destructive callers share this single predicate: a checkout must be
/// freshly proven stopped, unlocked, clean, and fully pushed before any
/// row/archive mutation can make it unreachable from the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceRemovalRisk {
    pub path: std::path::PathBuf,
    pub reasons: Vec<String>,
}

impl WorkspaceRemovalRisk {
    fn describe(&self) -> String {
        format!("{} ({})", self.path.display(), self.reasons.join(", "))
    }
}

fn canonical_or_self(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `path` lives in the daemon-owned worktree namespace. Imported
/// checkouts and on-main sessions point at user-owned repositories and must
/// never enter the lifecycle reclaimer, even when an old record happens to
/// persist their path.
fn is_managed_worktree_path(config: &ServerConfig, path: &std::path::Path) -> bool {
    canonical_or_self(path).starts_with(canonical_or_self(
        &config.worktree_root_path().join("worktrees"),
    ))
}

/// Snapshot every persisted session into the git inspector's source-agnostic
/// shape. Workspace lifecycle owns this projection so cleanup, explicit
/// removal, rescope, and diagnostics cannot drift onto different notions of
/// which worktrees are still tracked.
pub(crate) async fn collect_tracked_sessions(
    config: &ServerConfig,
) -> Result<Vec<lazybox_git_ops::TrackedSession>, String> {
    let store = config.store.clone();
    let scan = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let records = store
            .list_workspaces()
            .map_err(|error| format!("list workspaces: {error}"))?;
        let mut tracked = Vec::with_capacity(records.len() * 2);
        for record in records {
            let json = record
                .workspace_json
                .ok_or_else(|| format!("workspace {} has no persisted payload", record.key))?;
            let workspace = serde_json::from_str::<Workspace>(&json)
                .map_err(|error| format!("decode workspace {}: {error}", record.key))?;
            for session in workspace.sessions {
                let raw = session.id.to_string();
                tracked.push(lazybox_git_ops::TrackedSession {
                    session_id: raw.get(..8).unwrap_or(&raw).to_string(),
                    worktree_path: session.worktree_path,
                    is_stopped: matches!(session.state, lazybox_core::SessionRunState::Stopped),
                });
            }
        }
        Ok(tracked)
    })
    .await;
    match scan {
        Ok(result) => result,
        Err(error) => Err(format!("tracked-session scan task failed: {error}")),
    }
}

/// Freshly inspect every on-disk worktree owned by `workspace` and return the
/// reasons removal must stop. An inspection failure is an error, never an
/// empty/safe answer. `is_safe_to_delete` deliberately supplies the final
/// authority: besides dirty/ahead/locked state it requires the persisted
/// session to be stopped, preventing a caller from deleting underneath a
/// terminal whose map changed while teardown was beginning.
pub(crate) async fn inspect_workspace_removal_risks(
    config: &ServerConfig,
    workspace: &Workspace,
) -> Result<Vec<WorkspaceRemovalRisk>, String> {
    inspect_workspace_risks(config, workspace, true).await
}

/// Preflight variant used before a project cascade stops any terminals. It
/// catches known local work across every child so an obviously-unsafe later
/// child cannot leave the project half deleted. The final per-workspace gate
/// still runs with `require_stopped=true` immediately before mutation.
pub(crate) async fn inspect_workspace_local_risks(
    config: &ServerConfig,
    workspace: &Workspace,
) -> Result<Vec<WorkspaceRemovalRisk>, String> {
    inspect_workspace_risks(config, workspace, false).await
}

async fn inspect_workspace_risks(
    config: &ServerConfig,
    workspace: &Workspace,
    require_stopped: bool,
) -> Result<Vec<WorkspaceRemovalRisk>, String> {
    let paths: Vec<std::path::PathBuf> = workspace
        .sessions
        .iter()
        .map(|session| session.worktree_path.clone())
        .filter(|path| path.exists() && is_managed_worktree_path(config, path))
        .collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let tracked = collect_tracked_sessions(config).await?;
    let inspections = config
        .worktree_manager()
        .inspect_worktrees(&tracked)
        .await
        .map_err(|error| format!("could not inspect worktrees safely: {error}"))?;
    let by_path: std::collections::HashMap<_, _> = inspections
        .iter()
        .map(|row| (canonical_or_self(&row.path), row))
        .collect();

    let mut risks = Vec::new();
    for path in paths {
        let Some(row) = by_path.get(&canonical_or_self(&path)) else {
            risks.push(WorkspaceRemovalRisk {
                path,
                reasons: vec!["checkout could not be verified by the worktree inspector".into()],
            });
            continue;
        };
        if row.is_safe_to_delete {
            continue;
        }
        let reasons = workspace_removal_reasons(row, require_stopped);
        if !reasons.is_empty() {
            risks.push(WorkspaceRemovalRisk { path, reasons });
        }
    }
    Ok(risks)
}

fn workspace_removal_reasons(
    row: &lazybox_git_ops::WorktreeInspection,
    require_stopped: bool,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if row.reasons.contains(&lazybox_git_ops::OrphanReason::Locked) {
        reasons.push("locked".into());
    }
    if !row.status_verified {
        reasons.push("cleanliness could not be proven".into());
    }
    if row.has_uncommitted_changes {
        reasons.push("uncommitted changes".into());
    }
    if row.has_unpushed_commits {
        reasons.push("unpushed commits".into());
    }
    if reasons.is_empty() && require_stopped && !row.is_safe_to_delete {
        reasons.push("checkout is still active".into());
    }
    reasons
}

#[cfg(test)]
mod removal_classification_tests {
    use super::*;
    use lazybox_store::{MemoryStore, Store, StoreError, StoreMutation, WorkspaceRecord};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingWorkspaceListStore;

    impl lazybox_store::Store for FailingWorkspaceListStore {
        fn list_workspaces(
            &self,
        ) -> Result<Vec<lazybox_store::WorkspaceRecord>, lazybox_store::StoreError> {
            Err(lazybox_store::StoreError::Backend(
                "database is locked".into(),
            ))
        }
    }

    #[tokio::test]
    async fn tracked_session_scan_failure_is_not_an_empty_safe_set() {
        let config = ServerConfig::with_store(std::sync::Arc::new(FailingWorkspaceListStore));
        let error = collect_tracked_sessions(&config)
            .await
            .expect_err("a failed store scan must fail closed");
        assert!(error.contains("database is locked"), "got: {error}");
    }

    struct SlowCreateStore {
        inner: MemoryStore,
        active_key_reads: AtomicUsize,
        max_key_reads: AtomicUsize,
    }

    impl Store for SlowCreateStore {
        fn apply_batch(&self, mutations: &[StoreMutation]) -> Result<(), StoreError> {
            self.inner.apply_batch(mutations)
        }

        fn get_workspace(&self, key: &WorkspaceKey) -> Result<Option<WorkspaceRecord>, StoreError> {
            let active = self.active_key_reads.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_key_reads.fetch_max(active, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            let result = self.inner.get_workspace(key);
            self.active_key_reads.fetch_sub(1, Ordering::SeqCst);
            result
        }

        fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
            self.inner.get_kv(key)
        }

        fn set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
            self.inner.set_kv(key, value)
        }

        fn delete_kv(&self, key: &str) -> Result<(), StoreError> {
            self.inner.delete_kv(key)
        }

        fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, StoreError> {
            self.inner.list_workspaces()
        }
    }

    #[test]
    fn concurrent_workspace_creates_allocate_distinct_durable_keys() {
        const CREATORS: usize = 8;
        let store = std::sync::Arc::new(SlowCreateStore {
            inner: MemoryStore::new(),
            active_key_reads: AtomicUsize::new(0),
            max_key_reads: AtomicUsize::new(0),
        });
        let config = ServerConfig::with_store(store.clone());
        let start = std::sync::Arc::new(std::sync::Barrier::new(CREATORS));
        let threads = (0..CREATORS)
            .map(|_| {
                let config = config.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    create_empty_workspace(
                        &config,
                        "Release",
                        lazybox_core::ProjectKey::local("project"),
                    )
                    .expect("workspace creation")
                })
            })
            .collect::<Vec<_>>();
        let keys = threads
            .into_iter()
            .map(|thread| thread.join().expect("creator thread"))
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(keys.len(), CREATORS, "every create owns a unique suffix");
        assert_eq!(
            store.max_key_reads.load(Ordering::SeqCst),
            1,
            "allocation and persistence share one creation boundary"
        );
        assert_eq!(store.list_workspaces().unwrap().len(), CREATORS);
    }

    #[test]
    fn active_preflight_distinguishes_verified_clean_from_unknown_status() {
        let mut row = lazybox_git_ops::WorktreeInspection {
            path: "/tmp/lazybox-release-guard".into(),
            bare_path: Some("/tmp/lazybox-release-guard.git".into()),
            branch: Some("release".into()),
            session_id: Some("session".into()),
            reasons: Vec::new(),
            size_bytes: 0,
            last_modified: None,
            has_uncommitted_changes: false,
            status_verified: true,
            has_unpushed_commits: false,
            // An active tracked session is not deletable yet, even when its
            // local-work probes succeeded.
            is_safe_to_delete: false,
        };
        assert!(workspace_removal_reasons(&row, false).is_empty());

        row.status_verified = false;
        assert_eq!(
            workspace_removal_reasons(&row, false),
            vec!["cleanliness could not be proven"]
        );
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

/// Reclaim every daemon-owned persisted session worktree for a workspace
/// being torn down. User-owned linked/on-main paths are outside the managed
/// namespace and are never candidates. The detached remover freshly
/// re-inspects cleanliness and ownership before doing anything destructive;
/// there is no force or blind `rm -rf` path.
///
/// The measured total is returned synchronously, but the removal itself
/// is spawned onto a detached task (see [`spawn_worktree_removal`]) so a
/// multi-GB `rm` can't stall the poll/reconcile cycle this teardown runs
/// inside (issue #1132). The second tuple element is that task's handle,
/// which the deletion path drops (fire-and-forget) and tests await.
async fn reclaim_workspace_worktrees(
    config: &ServerConfig,
    workspace: &Workspace,
) -> (Reclaimed, Option<tokio::task::JoinHandle<()>>) {
    // Measure the reclaim synchronously (a stat-only walk) so the
    // returned total — and the "reclaimed N GB" notice — is accurate
    // immediately, but hand the actual removal to a detached task: a
    // multi-GB `git worktree remove` takes 7–11s, and this
    // teardown runs inside the poll/reconcile cycle the user is
    // watching sync (issue #1132).
    let mut reclaimed = Reclaimed::default();
    let mut paths = Vec::new();
    for session in &workspace.sessions {
        let path = &session.worktree_path;
        if !path.exists() || !is_managed_worktree_path(config, path) {
            continue;
        }
        reclaimed.bytes += dir_size(path).await;
        reclaimed.worktrees += 1;
        paths.push(path.clone());
    }

    tombstone_legacy_remote_host(config, workspace);

    let cleanup =
        (!paths.is_empty()).then(|| spawn_worktree_removal(config, workspace.key.clone(), paths));
    (reclaimed, cleanup)
}

/// Safely remove reclaimed worktree directories on a detached task so a
/// slow `git worktree remove` (seconds on multi-GB checkouts)
/// can't stall the caller — the poll/reconcile cycle that triggered the
/// teardown (issue #1132). The caller has already killed every backing
/// terminal and dropped the session records.
///
/// Removal is deliberately conservative: worktree paths are deterministic
/// (`<root>/<scope>/<slug>`), so a workspace that goes out of scope and
/// comes back re-provisions the exact path a queued removal targets. The
/// removal therefore re-checks [`worktree_path_is_reclaimed`] under the
/// per-repo lock, immediately before deleting, and skips any path a fresh
/// provision or a re-created session now owns. It also re-runs the worktree
/// inspector and the delete boundary re-probes locked/dirty/unpushed state
/// under the repo lock. Missing or unverifiable inspection rows are preserved.
/// Returns the task handle for tests to await; the deletion path
/// fire-and-forgets it.
fn spawn_worktree_removal(
    config: &ServerConfig,
    key: WorkspaceKey,
    paths: Vec<std::path::PathBuf>,
) -> tokio::task::JoinHandle<()> {
    let mgr = config.worktree_manager();
    // Completion latch so the shutdown drain can wait for this task
    // (2026-08-19 audit, L6): a quit mid-`git worktree remove` used to
    // cancel it, leaving a half-deleted multi-GB directory with no
    // retry. Sent from a drop guard so a panic resolves the latch too.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    config.register_maintenance_latch(done_rx);
    let config = config.clone();
    tokio::spawn(async move {
        struct SignalOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for SignalOnDrop {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let _done = SignalOnDrop(Some(done_tx));
        // Hold lock only for the critical section: taking the fresh inspector
        // snapshot to serialize against provisioning/adoption. Release before
        // expensive deletion operations. The per-repo lock inside
        // `delete_inspected_if` re-runs safety probes while held, closing the
        // final status-to-remove window.
        let tracked;
        let by_path: std::collections::HashMap<_, _>;
        {
            let _ownership_guard = config.worktree_ownership_lock.lock().await;
            tracked = match collect_tracked_sessions(&config).await {
                Ok(tracked) => tracked,
                Err(error) => {
                    tracing::warn!(
                        workspace = %key,
                        %error,
                        "delete_workspace: could not classify tracked worktrees — preserving them",
                    );
                    return;
                }
            };
            let inspections = match mgr.inspect_worktrees(&tracked).await {
                Ok(inspections) => inspections,
                Err(error) => {
                    tracing::warn!(
                        workspace = %key,
                        %error,
                        "delete_workspace: deferred safety inspection failed — preserving worktrees",
                    );
                    return;
                }
            };
            by_path = inspections
                .into_iter()
                .map(|row| (canonical_or_self(&row.path), row))
                .collect();
        }
        // Lock released here; expensive deletion operations follow.

        for path in paths {
            let Some(row) = by_path.get(&canonical_or_self(&path)) else {
                tracing::warn!(
                    workspace = %key,
                    worktree = %path.display(),
                    "delete_workspace: worktree is not inspectable — preserving it",
                );
                continue;
            };
            let guard_config = config.clone();
            let guard_key = key.clone();
            let guard_path = path.clone();
            match mgr
                .delete_inspected_if(row, /*force=*/ false, move || {
                    !worktree_path_is_reclaimed(&guard_config, &guard_key, &guard_path)
                })
                .await
            {
                Ok(true) => {}
                Ok(false) => tracing::info!(
                    workspace = %key,
                    worktree = %path.display(),
                    "delete_workspace: worktree re-provisioned before removal — left in place",
                ),
                Err(error) => tracing::warn!(
                    workspace = %key,
                    worktree = %path.display(),
                    %error,
                    "delete_workspace: worktree no longer proved safe — preserving it",
                ),
            }
        }
    })
}

/// True when a torn-down workspace's worktree `path` has been re-claimed
/// by a live or in-flight session — either an in-flight provision holds a
/// claim on it, or the workspace came back into scope and a committed
/// session now points at it. The deferred worktree removal (#1132)
/// consults this **under the per-repo lock** so a slow `rm` can never
/// delete a freshly re-provisioned checkout at the same deterministic
/// slug path. The claim covers the window from provision start to the
/// session-row commit; the committed session covers it from the commit
/// onward — together gap-free, so the check is never fooled by a
/// provision in flight.
fn worktree_path_is_reclaimed(
    config: &ServerConfig,
    key: &WorkspaceKey,
    path: &std::path::Path,
) -> bool {
    if crate::spawn_handler::provisioning_worktree_is_claimed(config, path) {
        return true;
    }
    load_workspace(config, key).is_some_and(|workspace| {
        workspace
            .sessions
            .iter()
            .any(|session| crate::spawn_handler::session_paths_match(&session.worktree_path, path))
    })
}

#[cfg(test)]
mod reclaim_worktree_tests {
    use super::*;
    use lazybox_core::{SessionKind, SessionRunState, WorkspaceSession as Session};

    async fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let output = tokio::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
            .env("GIT_CONFIG_VALUE_0", "false")
            .env("GIT_CONFIG_KEY_1", "tag.gpgsign")
            .env("GIT_CONFIG_VALUE_1", "false")
            .output()
            .await
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    async fn managed_checkout_fixture(
        local_commit: bool,
    ) -> (
        tempfile::TempDir,
        ServerConfig,
        WorkspaceKey,
        std::path::PathBuf,
    ) {
        let root = tempfile::tempdir().expect("worktree root");
        let upstream = root.path().join("upstream");
        std::fs::create_dir_all(&upstream).expect("upstream dir");
        run_git(&upstream, &["init", "-q", "-b", "main"]).await;
        run_git(&upstream, &["config", "user.email", "test@example.com"]).await;
        run_git(&upstream, &["config", "user.name", "test"]).await;
        std::fs::write(upstream.join("README.md"), "base\n").expect("seed readme");
        run_git(&upstream, &["add", "."]).await;
        run_git(&upstream, &["commit", "-q", "-m", "base"]).await;

        let bare = root.path().join("repos/o/r.git");
        std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("bare parent dir");
        run_git(
            root.path(),
            &[
                "clone",
                "--bare",
                "-q",
                &upstream.to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        )
        .await;
        let worktree = root.path().join("worktrees/o-r-release-guard");
        std::fs::create_dir_all(worktree.parent().expect("worktree parent"))
            .expect("worktree parent dir");
        run_git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "release-guard",
                &worktree.to_string_lossy(),
                "main",
            ],
        )
        .await;
        run_git(&worktree, &["config", "user.email", "test@example.com"]).await;
        run_git(&worktree, &["config", "user.name", "test"]).await;
        if local_commit {
            std::fs::write(worktree.join("release-fix.txt"), "only local copy\n")
                .expect("local work");
            run_git(&worktree, &["add", "."]).await;
            run_git(&worktree, &["commit", "-q", "-m", "release fix"]).await;
        }

        let store = std::sync::Arc::new(lazybox_store::MemoryStore::new());
        let backend = std::sync::Arc::new(crate::backend::MockBackend::new());
        let config = ServerConfig::with_store_backend_and_worktree_root(
            store,
            backend,
            root.path().to_path_buf(),
        );
        let key = WorkspaceKey::new("github:o/r#1166");
        let mut workspace = Workspace::empty(key.clone(), "release-guard", Utc::now());
        let mut session = Session::new(
            key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            worktree.clone(),
            Utc::now(),
        );
        session.state = SessionRunState::Stopped;
        workspace.sessions.push(session);
        commit_upsert(&config, &key, workspace).expect("persist workspace fixture");
        (root, config, key, worktree)
    }

    fn seed_worktree(dir: &std::path::Path, bytes: usize) {
        std::fs::create_dir_all(dir).expect("create worktree dir");
        std::fs::write(dir.join("payload"), vec![0u8; bytes]).expect("write payload");
    }

    #[tokio::test]
    async fn spawn_worktree_removal_preserves_unverifiable_dirs() {
        let config = ServerConfig::in_memory();
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path().join("scratch-worktree");
        seed_worktree(&wt, 2048);

        let handle = spawn_worktree_removal(
            &config,
            WorkspaceKey::new("local:scratch"),
            vec![wt.clone()],
        );
        handle.await.expect("removal task");

        assert!(wt.exists(), "unmanaged content is never deleted blindly");
    }

    // Regression for the re-provision data-loss race (#1132 review): a
    // workspace goes out of scope and its worktree is queued for a slow
    // removal, but the same deterministic slug path is re-provisioned (a
    // committed session now points at it) before the `rm` runs. The
    // guarded removal must leave the live checkout — and its uncommitted
    // work — untouched.
    #[tokio::test]
    async fn deferred_removal_spares_a_reprovisioned_worktree() {
        let config = ServerConfig::in_memory();
        let wt = config
            .worktree_root_path()
            .join("worktrees")
            .join("scope-slug");
        seed_worktree(&wt, 4096);

        // A re-provision re-created the workspace with a session pointing
        // at the exact path the removal was queued to delete.
        let key = WorkspaceKey::new("local:reborn");
        let mut workspace = Workspace::empty(key.clone(), "reborn", Utc::now());
        workspace.sessions.push(Session::new(
            key.clone(),
            SessionKind::Shell,
            wt.clone(),
            Utc::now(),
        ));
        commit_upsert(&config, &key, workspace).expect("seed re-provisioned workspace");
        assert!(
            worktree_path_is_reclaimed(&config, &key, &wt),
            "a committed session at the path reads as reclaimed",
        );

        spawn_worktree_removal(&config, key, vec![wt.clone()])
            .await
            .expect("removal task");

        assert!(wt.exists(), "the re-provisioned checkout must survive");
        assert!(
            wt.join("payload").exists(),
            "the live checkout's contents are untouched",
        );
    }

    // The guard only spares *re-claimed* paths — a genuinely orphaned
    // worktree (no claim, no session in the store) is still reclaimed.
    #[tokio::test]
    async fn worktree_path_is_reclaimed_is_false_for_an_orphan() {
        let config = ServerConfig::in_memory();
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path().join("orphan");
        assert!(
            !worktree_path_is_reclaimed(&config, &WorkspaceKey::new("local:gone"), &wt),
            "no claim and no live session → safe to remove",
        );
    }

    #[tokio::test]
    async fn reclaim_measures_bytes_but_defers_the_removal() {
        let config = ServerConfig::in_memory();
        let wt = config
            .worktree_root_path()
            .join("worktrees")
            .join("scratch-worktree");
        seed_worktree(&wt, 4096);

        let key = WorkspaceKey::new("local:scratch");
        let mut workspace = Workspace::empty(key.clone(), "scratch", Utc::now());
        workspace.sessions.push(Session::new(
            key,
            SessionKind::Shell,
            wt.clone(),
            Utc::now(),
        ));

        let (reclaimed, cleanup) = reclaim_workspace_worktrees(&config, &workspace).await;

        // Byte accounting is synchronous, so the notice is accurate the
        // instant the poll path returns — before the slow `rm` runs.
        assert_eq!(reclaimed.worktrees, 1);
        assert!(
            reclaimed.bytes >= 4096,
            "measured the tree before removing it"
        );

        cleanup
            .expect("removal deferred to a task")
            .await
            .expect("removal task");
        assert!(
            wt.exists(),
            "an unmanaged directory is measured but preserved as unverifiable"
        );
    }

    #[tokio::test]
    async fn reclaim_skips_missing_worktrees_and_spawns_nothing() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("local:gone");
        let mut workspace = Workspace::empty(key.clone(), "gone", Utc::now());
        workspace.sessions.push(Session::new(
            key,
            SessionKind::Shell,
            std::path::PathBuf::from("/nonexistent/worktree"),
            Utc::now(),
        ));

        let (reclaimed, cleanup) = reclaim_workspace_worktrees(&config, &workspace).await;

        assert_eq!(reclaimed.worktrees, 0);
        assert_eq!(reclaimed.bytes, 0);
        assert!(cleanup.is_none(), "nothing on disk → no cleanup task");
    }

    #[tokio::test]
    async fn workspace_delete_preserves_committed_but_unpushed_work() {
        let (_root, config, key, worktree) = managed_checkout_fixture(true).await;
        let mut events = config.bus.subscribe();

        assert_eq!(
            delete_workspace(&config, &key).await.map(|_| ()),
            None,
            "x x must fail closed when the branch is ahead"
        );

        assert!(
            load_workspace(&config, &key).is_some(),
            "workspace row survives"
        );
        assert!(worktree.exists(), "worktree survives");
        assert_eq!(
            std::fs::read_to_string(worktree.join("release-fix.txt")).unwrap(),
            "only local copy\n"
        );
        let mut visible_error = false;
        while let Ok(event) = events.try_recv() {
            if matches!(
                event,
                Event::ProviderError { ref message, .. }
                    if message.contains("local work must be preserved")
                        && message.contains("unpushed commits")
            ) {
                visible_error = true;
            }
        }
        assert!(visible_error, "refusal is visible instead of silent");
    }

    #[tokio::test]
    async fn workspace_delete_reclaims_a_freshly_verified_clean_worktree() {
        let (_root, config, key, worktree) = managed_checkout_fixture(false).await;

        assert!(
            delete_workspace(&config, &key).await.is_some(),
            "clean stopped work remains deletable"
        );
        assert!(
            load_workspace(&config, &key).is_none(),
            "workspace row removed"
        );
        assert!(!worktree.exists(), "managed worktree reclaimed");
    }
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
    WorkspaceLifecycle::new(config)
        .remove(key, WorkspaceRemovalReason::UserArchive)
        .await
}

/// Why a workspace is leaving the store. Archive policy is derived here,
/// instead of being passed as an unlabelled boolean at each destructive call
/// site. Every trigger still receives the same fresh terminal/worktree safety
/// sequence from [`WorkspaceLifecycle`] (#1167).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceRemovalReason {
    UserArchive,
    MergedConfirmed,
    ClosedAuto,
    Rescope,
    ProjectCascade,
}

impl WorkspaceRemovalReason {
    fn archives(self) -> bool {
        matches!(
            self,
            Self::UserArchive | Self::MergedConfirmed | Self::ProjectCascade
        )
    }
}

/// Single owner of workspace teardown: freeze new spawns, wait for in-flight
/// provisioning, kill terminals, freshly inspect and preserve unsafe local
/// work, apply reason-driven archive policy, drop the row, then reclaim only
/// the verified managed checkout. No caller can select only part of that
/// sequence.
pub(crate) struct WorkspaceLifecycle<'a> {
    config: &'a ServerConfig,
}

impl<'a> WorkspaceLifecycle<'a> {
    pub(crate) fn new(config: &'a ServerConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub(crate) async fn remove(
        &self,
        key: &WorkspaceKey,
        reason: WorkspaceRemovalReason,
    ) -> Option<Reclaimed> {
        let config = self.config;
        // Own delete-vs-spawn serialization here so every destructive reason
        // gets it. Keeping this in one command arm let another caller race a
        // late spawn that recreated the worktree after deletion.
        config
            .deleted_workspaces
            .lock()
            .insert(key.as_str().to_string());
        crate::spawn_handler::await_inflight_spawns(&config.spawn, key.as_str()).await;
        let _workspace_guard = config.lock_workspace(key.as_str()).await;
        let reclaimed = self.remove_locked(key, reason).await;
        if reclaimed.is_some() {
            crate::spawn_handler::release_delete_tombstone(config, key.as_str());
        }
        reclaimed
    }

    async fn remove_locked(
        &self,
        key: &WorkspaceKey,
        reason: WorkspaceRemovalReason,
    ) -> Option<Reclaimed> {
        let config = self.config;
        let archive = reason.archives();
        let key_str = key.as_str();

        // Snapshot the sessions before the store row is dropped — deleting
        // the row also drops the session → worktree_path mapping we need to
        // reclaim the on-disk directories afterwards.
        let workspace_snapshot = load_workspace(config, key);

        // The poller reaches this path from a raw record scan, so `None` can
        // mean either "another owner already removed it" or "the row exists
        // but cannot be decoded." Only the former is safe to treat as gone.
        // Preserve unreadable/erroring records: their unknown fields may be
        // the only remaining pointers to sessions or local work.
        if reason == WorkspaceRemovalReason::Rescope && workspace_snapshot.is_none() {
            match config.store.get_workspace(key) {
                Ok(None) => {}
                Ok(Some(_)) => {
                    tracing::warn!(
                        workspace = %key,
                        "rescope: stored workspace is unreadable — preserving"
                    );
                    let _ = config.bus.send(Event::provider_error_permanent(
                        "store",
                        format!(
                            "workspace {key} was not removed from the inactive set because its stored record could not be decoded"
                        ),
                    ));
                    config.deleted_workspaces.lock().remove(key_str);
                    return None;
                }
                Err(error) => {
                    tracing::warn!(workspace = %key, %error, "rescope: workspace reload failed");
                    let _ = config.bus.send(Event::provider_error_retryable(
                        "store",
                        format!(
                            "workspace {key} was not removed from the inactive set because its stored record could not be reloaded: {error}"
                        ),
                    ));
                    config.deleted_workspaces.lock().remove(key_str);
                    return None;
                }
            }
        }

        // Rescope is an automatic housekeeping decision, so it may only
        // remove a genuinely empty row. Keep this final decision inside the
        // lifecycle's tombstone + workspace-lock boundary: doing the check in
        // the poller and then calling `remove` both left a race and, when the
        // poller retained the lock, deadlocked on this method's lock attempt.
        // Explicit removal reasons intentionally continue below and surface
        // the normal terminal/worktree confirmation safety gates.
        if reason == WorkspaceRemovalReason::Rescope
            && let Some(workspace) = workspace_snapshot.as_ref()
        {
            let has_live_terminal = config.terminal.entries.lock().await.values().any(|entry| {
                !entry.finishing
                    && entry
                        .meta
                        .as_ref()
                        .is_some_and(|(session_key, _)| session_key.as_str() == key_str)
            });
            if !workspace.sessions.is_empty() || workspace.has_notes() || has_live_terminal {
                tracing::info!(
                    workspace = %key,
                    "rescope: workspace gained a session, terminal, or notes during sweep — preserving"
                );
                config.deleted_workspaces.lock().remove(key_str);
                return None;
            }

            // A session-less row can still own an adoptable managed checkout
            // after partial recovery. Resolve it from the branch while the
            // workspace is frozen. Failure to inspect is not proof that the
            // checkout is absent: fail closed and tell the client instead of
            // silently dropping the only row that points at local work.
            if let Some((owner, repo)) = workspace
                .primary_task()
                .and_then(|task| task.repo.as_deref())
                .and_then(|repo| repo.split_once('/'))
            {
                match config
                    .worktree_manager()
                    .managed_worktrees_for_branch(owner, repo, &workspace.branch)
                    .await
                {
                    Ok(worktrees) if worktrees.is_empty() => {}
                    Ok(_) => {
                        tracing::info!(
                            workspace = %key,
                            "rescope: preserving out-of-scope workspace with a worktree still on disk"
                        );
                        config.deleted_workspaces.lock().remove(key_str);
                        return None;
                    }
                    Err(error) => {
                        tracing::warn!(
                            workspace = %key,
                            %error,
                            "rescope: could not verify whether the workspace has a managed worktree"
                        );
                        let _ = config.bus.send(Event::provider_error_retryable(
                            "git",
                            format!(
                                "workspace {key} was not removed from the inactive set because its local worktrees could not be verified: {error}"
                            ),
                        ));
                        config.deleted_workspaces.lock().remove(key_str);
                        return None;
                    }
                }
            }
        }

        // Find every terminal whose session_key matches via
        // terminal_meta — the authoritative wire-side mapping. Earlier
        // we parsed the backend_key prefix, but the backend's session
        // name format isn't part of any contract (tmux now uses
        // `lazybox-{repo}-{kind}-{pid}-{n}`); the meta map is. Locks are
        // taken + dropped before async backend.kill() calls.
        let to_kill: Vec<(lazybox_ipc::TerminalId, String)> = config
            .terminal
            .entries
            .lock()
            .await
            .iter()
            .filter(|(_, entry)| {
                !entry.finishing
                    && entry
                        .meta
                        .as_ref()
                        .is_some_and(|(sk, _)| sk.as_str() == key_str)
            })
            .filter_map(|(tid, entry)| entry.backend_key.clone().map(|key| (*tid, key)))
            .collect();

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

        // The terminal teardown above is intentionally followed by a fresh
        // server-side inspection. A client confirmation is not a cleanliness
        // capability: the checkout may have changed while the modal was open,
        // and a just-finished agent commonly leaves committed-but-unpushed work.
        // Every destructive entry point funnels through this exact gate before
        // archive/store mutation. There is currently no force override; unsafe
        // work stays tracked and visible until the user pushes/stashes it.
        if let Some(workspace) = workspace_snapshot.as_ref() {
            match inspect_workspace_removal_risks(config, workspace).await {
                Ok(risks) if risks.is_empty() => {}
                Ok(risks) => {
                    let detail = risks
                        .iter()
                        .map(WorkspaceRemovalRisk::describe)
                        .collect::<Vec<_>>()
                        .join("; ");
                    tracing::warn!(
                        workspace = %key,
                        risk_count = risks.len(),
                        %detail,
                        "workspace removal refused by fresh worktree safety gate",
                    );
                    let _ = config.bus.send(Event::provider_error_permanent(
                        "store",
                        format!(
                            "workspace {key} was not deleted because local work must be preserved: \
                         {detail}. Push, commit/stash, or clean the checkout, then retry"
                        ),
                    ));
                    config.deleted_workspaces.lock().remove(key_str);
                    return None;
                }
                Err(error) => {
                    tracing::warn!(workspace = %key, "workspace removal inspection failed: {error}");
                    let _ = config.bus.send(Event::provider_error_permanent(
                        "store",
                        format!(
                            "workspace {key} was not deleted because its worktrees could not be \
                         verified safely: {error}"
                        ),
                    ));
                    config.deleted_workspaces.lock().remove(key_str);
                    return None;
                }
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
        // The removal runs on a detached task so a multi-GB `rm` can't stall
        // this teardown — the poll/reconcile cycle the delete runs inside
        // (issue #1132). Production drops the handle: dropping a tokio
        // JoinHandle detaches the task rather than cancelling it, so the
        // reclaim finishes in the background while this path returns. Tests
        // join the real detached task instead, so assertions on the
        // reclaimed worktree directory are deterministic rather than racing
        // the background `rm` (the deferral itself is covered directly by
        // `reclaim_measures_bytes_but_defers_the_removal`).
        let (reclaimed, cleanup) = reclaim_workspace_worktrees(config, &workspace).await;
        #[cfg(test)]
        if let Some(handle) = cleanup {
            let _ = handle.await;
        }
        #[cfg(not(test))]
        drop(cleanup);
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

    let mut children: Vec<Workspace> = Vec::new();
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
            children.push(ws);
        }
    }

    // Preflight the entire cascade before deleting its first child. The
    // final removal path re-inspects after stopping each child's terminals,
    // but this first pass prevents a known-dirty later workspace from
    // turning one project action into a silent partial cascade.
    for workspace in &children {
        match inspect_workspace_local_risks(config, workspace).await {
            Ok(risks) if risks.is_empty() => {}
            Ok(risks) => {
                let detail = risks
                    .iter()
                    .map(WorkspaceRemovalRisk::describe)
                    .collect::<Vec<_>>()
                    .join("; ");
                let _ = config.bus.send(Event::provider_error_permanent(
                    "store",
                    format!(
                        "project {project_key} was not deleted because workspace {} has local \
                         work to preserve: {detail}",
                        workspace.key
                    ),
                ));
                return;
            }
            Err(error) => {
                let _ = config.bus.send(Event::provider_error_permanent(
                    "store",
                    format!(
                        "project {project_key} was not deleted because workspace {} could not be \
                         inspected safely: {error}",
                        workspace.key
                    ),
                ));
                return;
            }
        }
    }

    let child_keys: Vec<WorkspaceKey> = children
        .into_iter()
        .map(|workspace| workspace.key)
        .collect();

    tracing::info!(
        project_key = %project_key,
        workspace_count = child_keys.len(),
        "delete_project: cascading workspace deletes"
    );
    let mut reclaimed = Reclaimed::default();
    for key in &child_keys {
        let Some(child) = WorkspaceLifecycle::new(config)
            .remove(key, WorkspaceRemovalReason::ProjectCascade)
            .await
        else {
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
/// sending, so this path still travels as a raw index. When fingerprint
/// is provided, it prevents unmarking the wrong activity if indices
/// shifted between the mark and unmark operations.
pub async fn unmark_activity_read(
    config: &ServerConfig,
    key: &WorkspaceKey,
    index: usize,
    fingerprint: Option<&lazybox_core::ActivityFingerprint>,
) {
    apply_activity_mark(config, key, index, fingerprint, /*read=*/ false).await;
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
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: Some(TaskKind::Pr),
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
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
