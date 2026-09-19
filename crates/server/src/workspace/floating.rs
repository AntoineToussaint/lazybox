//! Persistent repo-free workspaces. The record, not a key prefix, owns the folder.

use super::*;
use lazybox_core::{FloatingWorkspaceKind, SessionId, SessionKind, WorkspaceSession};
use std::path::PathBuf;

/// Create a fresh empty directory and its durable workspace record. Archived
/// directories are never reused: their contents still belong to the user.
pub fn create(
    config: &ServerConfig,
    name: &str,
    kind: FloatingWorkspaceKind,
) -> Result<WorkspaceKey, CreateWorkspaceError> {
    let project_key = super::create_local_project(config, "Floating");
    let _creation_guard = config.workspace_creations.lock();
    let slug = lazybox_core::slug::slugify(name);
    let base = if slug.is_empty() { "thinking" } else { &slug };
    std::fs::create_dir_all(lazybox_core::paths::sandboxes_root())?;
    for suffix in 1.. {
        let key = WorkspaceKey::new(format!("floating-{base}-{suffix}"));
        if config
            .store
            .get_workspace(&key)
            .map_err(CreateWorkspaceError::Allocate)?
            .is_some()
        {
            continue;
        }
        let path = lazybox_core::paths::sandbox_dir(key.as_str());
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
        let mut workspace = Workspace::empty(key.clone(), "", Utc::now());
        workspace.name = if name.trim().is_empty() {
            "Thinking".into()
        } else {
            name.trim().into()
        };
        workspace.project_key = Some(project_key);
        workspace.local = true;
        workspace.floating = Some(kind);
        if kind == FloatingWorkspaceKind::Coordination {
            workspace.role = Some(lazybox_core::Role::Coordinator);
        }
        if let Err(error) = commit_upsert(config, &key, workspace) {
            // Only remove our newly-created, still-empty directory. Never
            // recursively remove user data if something wrote into it.
            let _ = std::fs::remove_dir(&path);
            return Err(CreateWorkspaceError::Persist(error.to_string()));
        }
        return Ok(key);
    }
    unreachable!("unbounded suffix space")
}

/// Resolve a persisted floating workspace's session, rejecting stale session
/// ids before touching the filesystem. All its terminals share one folder.
pub async fn resolve_session(
    config: &ServerConfig,
    key: &WorkspaceKey,
    session_id: Option<SessionId>,
    kind: SessionKind,
) -> Result<(PathBuf, SessionId, bool), crate::ServerError> {
    let _guard = config.lock_workspace(key.as_str()).await;
    let mut workspace = crate::spawn_handler::load_workspace(config, key)?;
    if workspace.floating.is_none() || workspace.primary_task().is_some() {
        return Err(crate::ServerError::Workspace(
            "not a floating workspace".into(),
        ));
    }
    let session = match session_id {
        Some(id) => Some(workspace.find_session(id).ok_or_else(|| {
            crate::ServerError::Workspace(format!("session {id} not in workspace"))
        })?),
        None => workspace.default_session(),
    };
    let path = lazybox_core::paths::sandbox_dir(key.as_str());
    std::fs::create_dir_all(&path).map_err(|error| {
        crate::ServerError::Workspace(format!(
            "cannot open floating directory {}: {error}",
            path.display()
        ))
    })?;
    if let Some(session) = session {
        return Ok((path, session.id, false));
    }
    let session = WorkspaceSession::new(key.clone(), kind, path.clone(), Utc::now());
    let id = session.id;
    workspace.add_session(session.clone());
    commit_upsert(config, key, workspace).map_err(|error| {
        crate::ServerError::Workspace(format!("persist floating session: {error}"))
    })?;
    let _ = config.bus.send(Event::SessionCreated(Box::new(session)));
    Ok((path, id, false))
}

/// Read the overridable coordination brief for a workspace at launch time.
pub fn coordination_prompt(workspace: &Workspace, cfg: &lazybox_config::Config) -> Option<String> {
    (workspace.floating == Some(FloatingWorkspaceKind::Coordination))
        .then(|| {
            cfg.agent
                .coordination_prompt
                .clone()
                .unwrap_or_else(|| DEFAULT_COORDINATION_PROMPT.into())
        })
        .filter(|prompt| !prompt.trim().is_empty())
}

const DEFAULT_COORDINATION_PROMPT: &str = "This is a Lazybox coordination workspace: a repo-free place to plan and coordinate work across repositories. Wait for the user's objective before dispatching work.
Use an epic as the plan of record when an objective needs several independently owned changes. State the outcome, owner repositories, acceptance evidence, dependencies, blockers, and merge order there; keep it current as work lands.
Aim for the minimum necessary number of issues. Search existing records first, reuse them, and keep cohesive work in one issue. Split only for independent ownership, a real dependency, or a separately reviewable deliverable; do not create an issue for every step or file.
Make each change in the repository that owns the behavior. Before dependent work starts, agree on and record producer/consumer contracts and the owner of each interface. A blocker names the exact missing contract, decision, or change, who owns it, and what unblocks the dependent work.
The tracker record is the implementation workspace. Attach to its existing row, keep implementation in that repository, and use this folder for coordination notes. Cross-repo work belongs under one shared epic with explicit dependencies, not duplicated tracking issues.
Use the available Lazybox coordination tools (task_status, epic_status, list_sessions, read_session, shared notes, and notifications) to inspect live work before assigning it. Spawn workers only when authorized, on the owning tracker records, and respect working claims and opt-out labels. A quiet or finished agent turn is not proof that its task is complete.
Report concrete progress and evidence, remaining blockers with owners, and the next necessary action. Verify each deliverable and the integrated result before declaring the epic complete.";

/// Upgrade existing local sandbox records to explicit directory ownership.
/// The old key convention is consulted only during migration of persisted
/// records, never to authorize an incoming spawn for a nonexistent row.
pub(super) fn migrate_legacy(config: &ServerConfig) {
    let records = match config.store.list_workspaces() {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "cannot migrate legacy floating workspaces");
            return;
        }
    };
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(mut workspace) = Workspace::decode_persisted(&json) else {
            continue;
        };
        if workspace.floating.is_none()
            && workspace.local
            && workspace.linked_checkout.is_none()
            && workspace.primary_task().is_none()
            && workspace.key.as_str().starts_with("sandbox-")
            && lazybox_core::paths::sandbox_dir(workspace.key.as_str()).is_dir()
        {
            workspace.floating = Some(FloatingWorkspaceKind::Thinking);
            let key = workspace.key.clone();
            if let Err(error) = commit_upsert(config, &key, workspace) {
                report_commit_error(config, "migrate legacy floating workspace", &error);
            }
        }
    }
}
