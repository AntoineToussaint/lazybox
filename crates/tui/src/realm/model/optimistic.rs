//! Optimistic mutations + rollback (#476).
//!
//! Every mutating action should feel instant: apply the change to local
//! state on the keystroke, let the daemon round-trip run in the
//! background, and reconcile — or roll back — when the result arrives.
//! This module is the shared machinery each action funnels through so
//! none of them hand-rolls apply → send → reconcile / rollback.
//!
//! An [`OptimisticMutation`] captures enough to do both halves:
//! - **reconcile** on the daemon's success echo (`WorkspaceUpserted` /
//!   `WorkspaceRemoved` / `ProjectRemoved` naming the key) — drop the
//!   entry, the daemon's copy is now authoritative;
//! - **roll back** on the matching failure `ProviderError` — restore the
//!   prior rows so a rejected mutation never leaves a lie on screen.
//!
//! Correlation rides existing events (no new IPC variant): chip edits
//! match the daemon's `emit_err` source (`"reviewers"` / `"assignees"` /
//! `"labels"`), and removals match `source == "store"` whose message
//! names the removed key (the daemon embeds `workspace {key}` /
//! `project {key}` in every delete-failure message).

use super::Model;
use tuirealm::terminal::TerminalAdapter;

/// A locally-applied optimistic mutation awaiting the daemon's echo.
pub(super) struct OptimisticMutation {
    /// The `emit_err` source that reverts this entry: `"reviewers"` /
    /// `"assignees"` / `"labels"` for chip edits, `"store"` for row /
    /// project removals.
    source: &'static str,
    /// The workspace (or project) key. Reconciled against the success
    /// echo and — for removals — matched against the failure message,
    /// which names the key.
    key: String,
    /// Prior rows to restore on rollback: the edited workspace (chip
    /// edits) or the removed workspace(s) (removals / project cascade).
    workspaces: Vec<lazybox_core::Workspace>,
    /// Prior project to restore on rollback — set only for a project
    /// removal, whose header vanished alongside its child workspaces.
    project: Option<lazybox_core::Project>,
}

impl<T: TerminalAdapter> Model<T> {
    /// Remove a workspace row now, before the `Kill` round-trip — the
    /// optimistic half of archive/delete. Stashes the row so a failed
    /// delete re-inserts it. Reconciled by `WorkspaceRemoved`.
    pub(super) fn optimistic_remove_workspace(&mut self, session_key: &lazybox_core::SessionKey) {
        if let Some(workspace) = self.sidebar.take_workspace(session_key) {
            self.pending_mutations.push(OptimisticMutation {
                source: "store",
                key: session_key.as_str().to_string(),
                workspaces: vec![workspace],
                project: None,
            });
            self.redraw = true;
        }
    }

    /// Remove a project header + its child workspaces now, before the
    /// `DeleteProject` round-trip. Reconciled by `ProjectRemoved`; a
    /// failed cascade re-inserts the project and every child.
    pub(super) fn optimistic_remove_project(&mut self, project_key: &lazybox_core::ProjectKey) {
        let Some(project) = self.projects.remove(project_key) else {
            return;
        };
        let child_keys: Vec<lazybox_core::SessionKey> = self
            .sidebar
            .workspace_iter()
            .filter(|(_, w)| w.project_key.as_ref() == Some(project_key))
            .map(|(k, _)| k.clone())
            .collect();
        let mut workspaces = Vec::new();
        for key in &child_keys {
            if let Some(ws) = self.sidebar.take_workspace(key) {
                workspaces.push(ws);
            }
        }
        self.sidebar.apply_projects(self.projects.clone());
        self.pending_mutations.push(OptimisticMutation {
            source: "store",
            key: project_key.as_str().to_string(),
            workspaces,
            project: Some(project),
        });
        self.redraw = true;
    }

    /// Apply an optimistic chip edit to a workspace's task and stash the
    /// prior workspace for rollback. `edit` mutates a clone of the live
    /// workspace (typically its PR's / issue's reviewer / assignee /
    /// label set); the prior copy reverts a rejected round-trip.
    /// Reconciled by the next `WorkspaceUpserted`.
    pub(super) fn optimistic_chip_edit(
        &mut self,
        workspace_key: &lazybox_core::WorkspaceKey,
        source: &'static str,
        edit: impl FnOnce(&mut lazybox_core::Workspace),
    ) {
        let session_key: lazybox_core::SessionKey = workspace_key.into();
        let Some(prior) = self.sidebar.workspace_by_key(&session_key).cloned() else {
            return;
        };
        let mut next = prior.clone();
        edit(&mut next);
        self.sidebar.restore_workspace(next);
        self.pending_mutations.push(OptimisticMutation {
            source,
            key: workspace_key.as_str().to_string(),
            workspaces: vec![prior],
            project: None,
        });
        self.redraw = true;
    }

    /// Drop any optimistic mutation the daemon has now reconciled — the
    /// success echo (`WorkspaceUpserted` / `WorkspaceRemoved` /
    /// `ProjectRemoved`) for `key` means the daemon's copy is
    /// authoritative, so the rollback stash is no longer needed.
    pub(super) fn reconcile_optimistic(&mut self, key: &str) {
        self.pending_mutations.retain(|m| m.key != key);
    }

    /// Roll back the oldest optimistic chip edit for `source` when its
    /// round-trip was rejected (`ProviderError` with that source).
    /// Returns true when one was reverted so the caller can flash.
    pub(super) fn rollback_optimistic_chip(&mut self, source: &str) -> bool {
        let Some(pos) = self
            .pending_mutations
            .iter()
            .position(|m| m.source == source)
        else {
            return false;
        };
        let mutation = self.pending_mutations.remove(pos);
        self.apply_rollback(mutation);
        true
    }

    /// Roll back an optimistic removal whose delete failed. The daemon's
    /// delete-failure `ProviderError { source: "store" }` names the key
    /// in its message; re-insert the stashed rows when it matches a
    /// pending removal. Returns true when one was reverted.
    pub(super) fn rollback_optimistic_removal(&mut self, message: &str) -> bool {
        let Some(pos) = self
            .pending_mutations
            .iter()
            .position(|m| m.source == "store" && message.contains(&m.key))
        else {
            return false;
        };
        let mutation = self.pending_mutations.remove(pos);
        self.apply_rollback(mutation);
        true
    }

    fn apply_rollback(&mut self, mutation: OptimisticMutation) {
        if let Some(project) = mutation.project {
            self.projects.insert(project.key.clone(), project);
            self.sidebar.apply_projects(self.projects.clone());
        }
        for workspace in mutation.workspaces {
            self.sidebar.restore_workspace(workspace);
        }
        self.redraw = true;
    }
}
