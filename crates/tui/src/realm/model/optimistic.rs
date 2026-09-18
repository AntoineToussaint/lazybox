//! Optimistic workspace removal + rollback (#476).
//!
//! Archiving or deleting a row should feel instant: take it out of the
//! sidebar on the keystroke, let the daemon round-trip run in the
//! background, and re-insert it if the daemon refuses.
//!
//! This is **local lifecycle only**. Provider field edits (reviewers,
//! assignees, labels, workflow state) used to live here too, correlated
//! by the daemon's `emit_err` source string. They no longer do: the
//! daemon owns in-flight provider intent in `lazybox_core::ProviderOps`
//! and broadcasts the row with the user's intent already laid over the
//! last observation (#1736). Two owners of the same fields is exactly
//! what that state machine exists to remove — a client rollback keyed on
//! a source string could not tell a failure belonging to the edit on
//! screen from one belonging to an edit the user had already replaced.
//!
//! Correlation rides existing events (no new IPC variant): a removal
//! matches a delete-failure `"store"` (archive / db) or `"terminal"` (a
//! backing agent that couldn't be stopped, so the daemon preserves the
//! workspace) error whose message names the removed key — the daemon
//! embeds `workspace {key}` / `project {key}` in each.

use super::Model;
use tuirealm::terminal::TerminalAdapter;

/// A locally-applied optimistic removal awaiting the daemon's echo.
pub(super) struct OptimisticMutation {
    /// The `emit_err` source that reverts this entry.
    source: &'static str,
    /// The workspace (or project) key. Reconciled against the success
    /// echo and matched against the failure message, which names the key.
    key: String,
    /// Prior rows to restore on rollback — the removed workspace, or a
    /// project's whole cascade.
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

    /// Drop any optimistic mutation the daemon has now reconciled — the
    /// success echo (`WorkspaceUpserted` / `WorkspaceRemoved` /
    /// `ProjectRemoved`) for `key` means the daemon's copy is
    /// authoritative, so the rollback stash is no longer needed.
    pub(super) fn reconcile_optimistic(&mut self, key: &str) {
        self.pending_mutations.retain(|m| m.key != key);
    }

    /// Roll back an optimistic removal whose delete failed. The daemon's
    /// delete-failure `ProviderError { source: "store" }` names the key
    /// in its message; re-insert the stashed rows when it matches a
    /// pending removal. Returns true when one was reverted.
    ///
    /// The cursor is put back too. Taking the row out moved it to a
    /// neighbour, and restoring the row without restoring the cursor
    /// rolls back only half the optimistic change: the row returns,
    /// selected is someone else, and every per-row action — plus any
    /// advice the failure notice gives about "this workspace" — lands
    /// on a workspace the user never touched (#1805). Only a
    /// single-row removal re-focuses; a project cascade restores a
    /// header and N children with no one row to return to.
    pub(super) fn rollback_optimistic_removal(&mut self, message: &str) -> bool {
        let Some(pos) = self
            .pending_mutations
            .iter()
            .position(|m| m.source == "store" && message.contains(&m.key))
        else {
            return false;
        };
        let mutation = self.pending_mutations.remove(pos);
        let restored_row = (mutation.project.is_none() && mutation.workspaces.len() == 1)
            .then(|| lazybox_core::SessionKey::from(&mutation.workspaces[0].key));
        self.apply_rollback(mutation);
        if let Some(key) = restored_row {
            self.sidebar.focus_workspace_key(&key);
        }
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
