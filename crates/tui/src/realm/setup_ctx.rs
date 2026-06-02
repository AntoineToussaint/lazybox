//! `SetupCtx` — grouped state for the setup wizard, settings palette,
//! and editor-open shortcut. These eight fields used to sprawl across
//! `Model`; bundling them here lets the reader see at a glance that
//! they all belong to the same flow.
//!
//! Logic stays on `Model` for now (every method also touches `app`,
//! `modal_stack`, `client`, etc.). That carve-out is a follow-up. The
//! win from this struct alone is a tidier `Model` definition and a
//! single import line for any future code touching setup state.

use crate::editors::EditorTemplate;
use crate::setup;
use crate::setup_flow::{SetupOutcome, SetupRunner};
use pilot_core::{PersistedSetup, ScopeSource, SessionKey};
use std::sync::Arc;

/// Cached setup detection results — the `SetupReport` + the source
/// list `SetupRunner::new` needs. Returned by `crate::setup::detect`
/// and stashed so the wizard can be re-opened from `,` without
/// re-running detection from scratch.
pub(crate) type SetupInputs = (setup::SetupReport, Arc<Vec<Box<dyn ScopeSource>>>);

/// One row in the Settings palette (`,` opens this).
#[derive(Debug, Clone)]
pub enum SettingsAction {
    /// Add / remove orgs + repos for a provider.
    EditScopes { provider_id: String, label: String },
    /// Edit role / item-type filters for a provider.
    EditFilters { provider_id: String, label: String },
    /// Re-run the providers picker (enable/disable github / linear / …).
    EditProviders,
    /// Re-run the agents picker.
    EditAgents,
    /// Toggle `agent.skip_permissions` — whether interactive Claude
    /// sessions launch with `--dangerously-skip-permissions`. Carries
    /// the current value so the dispatcher knows which way to flip.
    ToggleSkipPermissions { enabled: bool },
    /// Open `<pilot_home>/snippets.yaml` in the configured editor,
    /// seeding a commented template first when the file is absent.
    /// Snippets are file-owned (no in-app editor by design), so this
    /// just gets the user to the file.
    EditSnippets,
    /// Bail out and run the full splash → providers → agents → … wizard.
    FullSetup,
    /// Admin: wipe every worktree whose session has no live
    /// terminal. Inbox rows stay. Dispatches `Command::CleanWorktrees`
    /// after a confirm prompt.
    CleanWorktrees,
    /// Admin: open the worktree inspector — lists every worktree
    /// pilot finds on disk, flags orphans by reason, surfaces
    /// uncommitted / unpushed work, lets the user delete per-row or
    /// bulk-delete the clearly-safe set.
    InspectWorktrees,
}

impl SettingsAction {
    pub fn label(&self) -> String {
        match self {
            Self::EditScopes { label, .. } => format!("Add / remove repos · {label}"),
            Self::EditFilters { label, .. } => format!("Edit roles + filters · {label}"),
            Self::EditProviders => "Edit providers (github / linear / …)".into(),
            Self::EditAgents => "Edit agents (claude / codex / cursor / …)".into(),
            Self::ToggleSkipPermissions { enabled } => format!(
                "Skip permission prompts for your sessions · {}",
                if *enabled { "on" } else { "off" }
            ),
            Self::EditSnippets => "Edit snippets (]]<key> shortcuts)".into(),
            Self::FullSetup => "Run the full setup wizard".into(),
            Self::CleanWorktrees => "Clean worktrees (free disk, keep inbox)".into(),
            Self::InspectWorktrees => "Inspect worktrees…".into(),
        }
    }
}

pub(crate) struct SetupCtx {
    /// In-flight setup wizard. When `Some`, splash/choice/loading
    /// messages route through the runner's state machine instead of
    /// the generic post-splash path. `None` once setup completes (or
    /// the user cancels).
    pub runner: Option<SetupRunner>,
    /// Cached setup inputs — populated on first launch by
    /// `main::run_embedded_realm` so the wizard can be re-opened
    /// mid-session (key `,`) for adding repos / agents without
    /// re-detecting from scratch. Refresh inside the wizard via `r`.
    pub inputs: Option<SetupInputs>,
    /// Last-known persisted setup. Cached at startup + after every
    /// successful wizard run. Used by partial flows from the Settings
    /// palette to pre-seed the SetupRunner with existing state so
    /// "Edit filters for github" doesn't lose the user's linear config.
    pub persisted: Option<PersistedSetup>,
    /// Items behind the active SettingsMenu picker. Choice gives us
    /// indices; we map them back to actions here.
    pub settings_actions: Vec<SettingsAction>,
    /// Editors detected on PATH at startup + any custom entries from
    /// `~/.pilot/config.yaml`. Drives the `e` open-in-editor shortcut.
    /// Empty when no editor is installed.
    pub editors: Vec<EditorTemplate>,
    /// Items behind the active editor picker (when `e` finds 2+
    /// editors). Same shape as `settings_actions`.
    pub editor_choices: Vec<EditorTemplate>,
    /// Editor launch deferred behind a session-spawn. Populated when
    /// the user pressed `e` on a workspace with no worktree yet: the
    /// orchestrator emits `Command::Spawn(Shell)` to provision one,
    /// and `handle_daemon_event` fires the launch when the matching
    /// `TerminalSpawned` arrives.
    pub pending_editor_launch: Option<(SessionKey, EditorTemplate)>,
    /// Workspace waiting for an editor pick — set when `e` was pressed
    /// on a worktreeless workspace AND multiple editors were detected.
    /// The Choice picker resolves to a template, at which point we
    /// move it to `pending_editor_launch`.
    pub pending_editor_workspace: Option<SessionKey>,
    /// Hook invoked every time setup finishes successfully (first-run
    /// wizard AND partial flows from the Settings palette — "Add a
    /// repo", "Edit filters", etc.). `main.rs::run_embedded_realm`
    /// installs this so each Finish persists the YAML and (re)spawns
    /// the polling loop. Held as `Arc<dyn Fn>` so it can fire many
    /// times — earlier we used `FnOnce` and partial flows silently
    /// dropped their accumulator.
    pub on_complete: Option<Arc<dyn Fn(SetupOutcome) + Send + Sync>>,
}

impl SetupCtx {
    pub fn new() -> Self {
        Self {
            runner: None,
            inputs: None,
            persisted: None,
            settings_actions: Vec::new(),
            editors: Vec::new(),
            editor_choices: Vec::new(),
            pending_editor_launch: None,
            pending_editor_workspace: None,
            on_complete: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SettingsAction;

    #[test]
    fn skip_permissions_label_reflects_state() {
        assert!(
            SettingsAction::ToggleSkipPermissions { enabled: false }
                .label()
                .ends_with("off")
        );
        assert!(
            SettingsAction::ToggleSkipPermissions { enabled: true }
                .label()
                .ends_with("on")
        );
    }
}
