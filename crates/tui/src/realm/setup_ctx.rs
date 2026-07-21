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
use lazybox_core::{PersistedSetup, ScopeSource, SessionKey};
use std::sync::Arc;

/// Cached setup detection results — the `SetupReport` + the source
/// list `SetupRunner::new` needs. Returned by `crate::setup::detect`
/// and stashed so the wizard can be re-opened from `,` without
/// re-running detection from scratch.
pub(crate) type SetupInputs = (setup::SetupReport, Arc<Vec<Box<dyn ScopeSource>>>);

/// Result of persisting a finished setup. `Ok(Some(path))` means a
/// malformed pre-existing config.yaml was moved aside to `path`
/// before the fresh write; `Err` means nothing was saved and the UI
/// must say so instead of showing success.
pub type SetupSaveResult = anyhow::Result<Option<std::path::PathBuf>>;

/// Hook fired on every wizard / partial-flow Finish. Returns the
/// save outcome so the Finish handler can surface failures and skip
/// caching state that never hit disk.
pub type SetupCompleteHook = Arc<dyn Fn(SetupOutcome) -> SetupSaveResult + Send + Sync>;

/// Tab a [`SettingsAction`] lives under in the Settings window (`,`).
/// The old palette was one flat `Choice` list — 11+ rows, growing two
/// per enabled provider, with "change theme" sitting next to "wipe
/// worktrees". Grouping by concern is what makes the window
/// scannable; the order of [`SettingsSection::ALL`] is the tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    /// Provider membership + per-provider scopes and filters.
    Providers,
    /// Agent roster, default agent, permission mode, LLM gateway.
    Agents,
    /// Theme + snippets — how lazybox looks and what it types for you.
    Appearance,
    /// Disk admin + the full-wizard escape hatch. Deliberately last:
    /// destructive-adjacent actions shouldn't share a screen with
    /// "change theme".
    Maintenance,
}

impl SettingsSection {
    /// Tab order in the Settings window.
    pub const ALL: [SettingsSection; 4] = [
        SettingsSection::Providers,
        SettingsSection::Agents,
        SettingsSection::Appearance,
        SettingsSection::Maintenance,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Agents => "Agents",
            Self::Appearance => "Appearance",
            Self::Maintenance => "Maintenance",
        }
    }
}

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
    /// Pick the default agent (`setup.default_agent`) — the one `w`
    /// "work on this" and new-workspace spawns use — then, when that
    /// agent declares model tiers, its default tier
    /// (`agents.<id>.models.default`). Carries the current default id
    /// (and its default-tier label, if one is set) for the label.
    EditDefaultAgent {
        current: String,
        tier: Option<String>,
    },
    /// Pick one agent's default model tier
    /// (`agents.<id>.models.default`) directly — without routing
    /// through the default-agent flow. One row per enabled agent that
    /// declares a tier menu. Carries the current default-tier label
    /// for the row badge.
    EditDefaultModel {
        agent_id: String,
        tier: Option<String>,
    },
    /// Toggle `agent.skip_permissions` — whether interactive Claude
    /// sessions launch with `--dangerously-skip-permissions`. Carries
    /// the current value so the dispatcher knows which way to flip.
    ToggleSkipPermissions { enabled: bool },
    /// Open the read-only snippets browser (same modal as the `]`
    /// shortcut). Lists the merged library so it's discoverable; `e`
    /// inside the browser opens `<lazybox_home>/snippets.yaml` in the
    /// editor, since snippets stay file-owned (no in-app editor by
    /// design).
    EditSnippets,
    /// Open the live-preview theme picker (same modal as the `t`
    /// shortcut). Persists the choice to `ui.theme`. Carries the
    /// active theme name so the row shows current state like its
    /// siblings.
    EditTheme { current: String },
    /// Configure the global LLM gateway base URL (`agent.llm_gateway_url`).
    /// Opens a single URL input; persists to `~/.lazybox/config.yaml`.
    /// `set` is whether a URL is currently configured, for the label.
    EditLlmGateway { set: bool },
    /// Bail out and run the full splash → providers → agents → … wizard.
    FullSetup,
    /// Admin: wipe every worktree whose session has no live
    /// terminal. Inbox rows stay. Dispatches `Command::CleanWorktrees`
    /// after a confirm prompt.
    CleanWorktrees,
    /// Admin: open the worktree inspector — lists every worktree
    /// lazybox finds on disk, flags orphans by reason, surfaces
    /// uncommitted / unpushed work, lets the user delete per-row or
    /// bulk-delete the clearly-safe set.
    InspectWorktrees,
    /// Probe each enabled agent CLI's installed vs latest version and
    /// report in the footer. Dispatches `Command::CheckAgentCliUpdates`.
    CheckAgentUpdates,
    /// Update every enabled agent CLI through its lazybox-managed
    /// channel (npm / brew / the CLI's own updater), out of band —
    /// never inside a live session. Dispatches
    /// `Command::UpdateAgentClis`; per-agent outcomes arrive as
    /// footer notices.
    UpdateAgentClis,
}

impl SettingsAction {
    pub fn label(&self) -> String {
        match self {
            Self::EditScopes { label, .. } => format!("Add / remove repos · {label}"),
            Self::EditFilters { label, .. } => format!("Edit roles + filters · {label}"),
            Self::EditProviders => "Edit providers (github / linear / …)".into(),
            Self::EditAgents => "Edit agents (claude / codex / cursor / …)".into(),
            Self::EditDefaultAgent { current, tier } => match tier {
                Some(tier) => format!("Change default agent · {current} · ◆ {tier}"),
                None => format!("Change default agent · {current}"),
            },
            Self::EditDefaultModel { agent_id, tier } => match tier {
                Some(tier) => format!("Default model · {agent_id} · ◆ {tier}"),
                None => format!("Default model · {agent_id}"),
            },
            Self::ToggleSkipPermissions { enabled } => format!(
                "Skip permission prompts for your sessions · {}",
                if *enabled { "on" } else { "off" }
            ),
            Self::EditSnippets => "Browse snippets (]]s<key> shortcuts)".into(),
            Self::EditTheme { current } => format!("Change theme (live preview) · {current}"),
            Self::EditLlmGateway { set } => format!(
                "Configure LLM gateway · {}",
                if *set { "on" } else { "off" }
            ),
            Self::FullSetup => "Run the full setup wizard".into(),
            Self::CleanWorktrees => "Clean worktrees (free disk, keep inbox)".into(),
            Self::InspectWorktrees => "Inspect worktrees…".into(),
            Self::CheckAgentUpdates => "Check agent CLI updates".into(),
            Self::UpdateAgentClis => "Update agent CLIs now".into(),
        }
    }

    /// Which Settings tab this action lives under. Every action maps
    /// to exactly one section — adding a `SettingsAction` variant is a
    /// compile error here until it's filed somewhere.
    pub fn section(&self) -> SettingsSection {
        match self {
            Self::EditScopes { .. } | Self::EditFilters { .. } | Self::EditProviders => {
                SettingsSection::Providers
            }
            Self::EditAgents
            | Self::EditDefaultAgent { .. }
            | Self::EditDefaultModel { .. }
            | Self::ToggleSkipPermissions { .. }
            | Self::EditLlmGateway { .. } => SettingsSection::Agents,
            Self::EditTheme { .. } | Self::EditSnippets => SettingsSection::Appearance,
            Self::FullSetup
            | Self::CleanWorktrees
            | Self::InspectWorktrees
            | Self::CheckAgentUpdates
            | Self::UpdateAgentClis => SettingsSection::Maintenance,
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
    /// `~/.lazybox/config.yaml`. Drives the `e` open-in-editor shortcut.
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
    /// dropped their accumulator. The returned [`SetupSaveResult`]
    /// tells the Finish handler whether the save actually landed.
    pub on_complete: Option<SetupCompleteHook>,
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
    fn llm_gateway_label_reflects_set_state() {
        assert!(
            SettingsAction::EditLlmGateway { set: false }
                .label()
                .ends_with("off")
        );
        assert!(
            SettingsAction::EditLlmGateway { set: true }
                .label()
                .ends_with("on")
        );
    }

    #[test]
    fn default_agent_label_names_the_current() {
        assert_eq!(
            SettingsAction::EditDefaultAgent {
                current: "codex".into(),
                tier: None,
            }
            .label(),
            "Change default agent · codex"
        );
    }

    #[test]
    fn default_agent_label_shows_the_default_tier_badge() {
        assert_eq!(
            SettingsAction::EditDefaultAgent {
                current: "claude".into(),
                tier: Some("Opus".into()),
            }
            .label(),
            "Change default agent · claude · ◆ Opus"
        );
    }

    #[test]
    fn default_model_label_names_agent_and_tier() {
        assert_eq!(
            SettingsAction::EditDefaultModel {
                agent_id: "claude".into(),
                tier: Some("Opus".into()),
            }
            .label(),
            "Default model · claude · ◆ Opus"
        );
        assert_eq!(
            SettingsAction::EditDefaultModel {
                agent_id: "codex".into(),
                tier: None,
            }
            .label(),
            "Default model · codex"
        );
    }

    #[test]
    fn theme_label_names_the_current_theme() {
        assert_eq!(
            SettingsAction::EditTheme {
                current: "Dark".into()
            }
            .label(),
            "Change theme (live preview) · Dark"
        );
    }

    #[test]
    fn sections_group_by_concern_with_maintenance_last() {
        use super::SettingsSection;
        assert_eq!(
            SettingsAction::EditProviders.section(),
            SettingsSection::Providers
        );
        assert_eq!(
            SettingsAction::EditScopes {
                provider_id: "github".into(),
                label: "GitHub".into()
            }
            .section(),
            SettingsSection::Providers
        );
        assert_eq!(
            SettingsAction::EditLlmGateway { set: false }.section(),
            SettingsSection::Agents
        );
        assert_eq!(
            SettingsAction::EditTheme {
                current: "Dark".into()
            }
            .section(),
            SettingsSection::Appearance
        );
        // Destructive-adjacent admin actions live on the LAST tab so
        // they never share a screen with appearance tweaks.
        assert_eq!(
            SettingsAction::CleanWorktrees.section(),
            SettingsSection::Maintenance
        );
        assert_eq!(
            SettingsSection::ALL.last().copied(),
            Some(SettingsSection::Maintenance)
        );
    }

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
