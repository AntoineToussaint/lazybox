//! Pure resolution for picker modal results.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use lazybox_core::{
    AutoFixKind, ProjectKey, SessionId, SessionKey, Workspace, WorkspaceKey,
    prompts::AgentHandoffRole,
};
use lazybox_ipc::{
    Command, DiscoveredCheckoutDto, TerminalId, TerminalKind, WorktreeInspectionDto,
};

use crate::{
    action::Action,
    editors::{EditorTemplate, OpenWithApp, OpenWithContext},
};

/// Payload access needed by [`resolve_pick`]. The renderer owns its picker
/// payload enum; this trait keeps the pure resolver independent of it.
pub trait PickPayload {
    type Filter: Clone;

    fn as_index(&self) -> Option<usize>;
    fn as_text(&self) -> Option<&str>;
    fn opt_text(&self) -> Option<Option<String>>;
    fn as_duration(&self) -> Option<Duration>;
    fn filter(&self) -> Option<Self::Filter>;
    fn policy(&self) -> Option<PolicyPick>;
    fn workspace(&self) -> Option<WorkspaceKey>;
    fn project(&self) -> Option<ProjectKey>;
    fn is_new_local_project(&self) -> bool;
    fn session(&self) -> Option<SessionKey>;
    fn handoff_role(&self) -> Option<AgentHandoffRole>;
}

/// Renderer-independent automation policy payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyPick {
    MergeOnGreen,
    AutoFix(AutoFixKind),
    Info(String),
}

/// Snippet data visible to a picker resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetPick {
    pub key: String,
    pub category: String,
    pub body: String,
}

/// One exact running conversation in the work-agent picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPickTarget {
    pub terminal_id: TerminalId,
    pub agent_id: String,
}

/// State carried by the work-agent picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPickerState {
    pub targets: Vec<WorkPickTarget>,
    pub session_id: Option<SessionId>,
    pub model_alias: Option<String>,
}

/// Pure state needed to resolve the picker currently on top.
#[derive(Debug, Clone)]
pub enum PickFlow {
    BroadcastSnippet {
        active: bool,
        snippets: Vec<SnippetPick>,
    },
    Snippet {
        terminal_id: Option<TerminalId>,
        snippets: Vec<SnippetPick>,
        /// `Enter` picks submit (`true`); `Shift-Enter` inserts the body
        /// without submitting (`false`), so the user can edit it before
        /// sending (issue #791).
        submit: bool,
    },
    Skill {
        terminal_id: Option<TerminalId>,
    },
    PromptHistory {
        terminal_id: Option<TerminalId>,
        terminal_is_agent: bool,
    },
    Jump,
    Url,
    Theme,
    DefaultAgent,
    DefaultModel {
        agent_id: Option<String>,
    },
    SidebarContext {
        session_key: Option<SessionKey>,
        actions: Vec<Action>,
    },
    Adopt {
        source: Option<WorkspaceKey>,
    },
    HandoffTarget {
        active: bool,
    },
    ConvertSession {
        active: bool,
    },
    StartAgentProject,
    NewWorkspaceRepo,
    /// Repo picker for an unmapped Linear team (#1041). Each row carries a
    /// [`PickPayload::as_text`] `owner/repo`; the pick persists
    /// `providers.linear.teams.<team>` and re-provisions.
    LinearTeamRepo {
        team: String,
    },
    Reviewers {
        workspace_key: Option<WorkspaceKey>,
    },
    Policy {
        workspace: Option<Box<Workspace>>,
    },
    WorkAgent {
        picker: Option<WorkPickerState>,
    },
    Snooze {
        session_key: Option<SessionKey>,
        now: DateTime<Utc>,
    },
    Labels {
        workspace_key: Option<WorkspaceKey>,
    },
    Assignees {
        workspace_key: Option<WorkspaceKey>,
    },
    Import {
        rows: Vec<DiscoveredCheckoutDto>,
    },
    Filters,
    Inspect {
        rows: Vec<WorktreeInspectionDto>,
    },
    Editor {
        choices: Vec<EditorTemplate>,
        pending_workspace: Option<SessionKey>,
        worktree: Option<PathBuf>,
    },
    /// Config-driven "Open with…" picker (issue #1100). Each row is an
    /// index into `apps`; the pick launches that app on the focused
    /// workspace with `ctx`'s tokens substituted.
    OpenWith {
        apps: Vec<OpenWithApp>,
        ctx: OpenWithContext,
    },
    Settings {
        action_count: usize,
    },
    /// Move-to-Space picker (`x m`, #1206). Each row is an index into
    /// `entries` so the assign / unassign / new-Space rows resolve
    /// without string sentinels.
    MoveToSpace {
        source: Option<String>,
        entries: Vec<SpacePickEntry>,
    },
    /// Right-click menu on a sidebar Space / repo header (#1211). Each
    /// row is an index into `actions`; the pick dispatches that action
    /// against the cursor (already parked on the clicked header).
    HeaderContext {
        actions: Vec<Action>,
    },
    Runner,
    Plain,
}

/// Which move-to-Space row starts highlighted (#1206): the last-used
/// Space when it still exists, else the source's current Space, else
/// the first row — so filing many repos into one Space is Enter each.
pub fn space_preselect_index(spaces: &[String], last: Option<&str>, current: &str) -> usize {
    last.and_then(|l| spaces.iter().position(|s| s == l))
        .or_else(|| spaces.iter().position(|s| s == current))
        .unwrap_or(0)
}

/// One row of the move-to-Space picker (#1206), in rendered order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpacePickEntry {
    /// A hand-created Space (a `ui.spaces` entry) to file the source
    /// into.
    Assign(String),
    /// Blank assignment — back to the owner auto-seed / `Ungrouped`.
    Unassign,
    /// Drop to the free-text input for a brand-new Space name.
    NewSpace,
}

/// Side-effect-free result of resolving one picker.
#[derive(Debug, Clone)]
pub enum PickOutcome<F> {
    NoOp,
    Pop,
    MountBroadcastComposer {
        snippet_key: Option<String>,
        body: Option<String>,
    },
    StaleSnippet(String),
    Commands {
        commands: Vec<Command>,
        notice: Option<String>,
    },
    /// A `Shift-Enter` snippet pick: deliver the body to `terminal_id`
    /// without submitting (compose only) AND mirror it into the client's
    /// composing buffer so the recap + persisted draft stay in step (#791).
    /// Split out from [`PickOutcome::Commands`] because the renderer must
    /// touch its own terminal state — the pure resolver can only name the
    /// intent.
    InsertSnippetDraft {
        terminal_id: TerminalId,
        snippet_key: String,
        category: String,
        body: String,
    },
    DeliverPrompt {
        terminal_id: TerminalId,
        text: String,
    },
    /// Trigger a skill explicitly: inject `text` (the "Use the `<name>`
    /// skill." instruction) through the settle-gated agent inject path and
    /// float `skill_name` to the front of the skills MRU (#797).
    TriggerSkill {
        terminal_id: TerminalId,
        skill_name: String,
        text: String,
    },
    Jump(SessionKey),
    OpenUrl(String),
    SaveTheme(String),
    SaveDefaultAgent(String),
    SaveDefaultModel {
        agent_id: String,
        alias: Option<String>,
    },
    DispatchAction {
        session_key: SessionKey,
        action: Action,
    },
    MountHandoffComposer {
        target: SessionKey,
    },
    StartSessionConversion {
        role: AgentHandoffRole,
    },
    MountNewWorkspace(ProjectKey),
    MountNewProject,
    /// Move-to-Space pick (#1206): assign `source` to the named Space,
    /// or unassign it (`None` → owner auto-seed). The renderer mutates
    /// its sidebar + persists `ui.spaces` / `ui.last_space`.
    AssignSpace {
        source: String,
        space: Option<String>,
    },
    /// "＋ New Space…" pick (#1206): drop to the free-text
    /// move-to-Space input for `source`.
    MountMoveToSpaceInput {
        source: String,
    },
    /// Header-menu pick (#1211): dispatch `action` against the sidebar
    /// cursor (no workspace key — the action reads the cursor row).
    DispatchCursorAction {
        action: Action,
    },
    /// Persist `providers.linear.teams.<team> = repo` and re-provision the
    /// stuck Linear spawn (#1041).
    MapLinearTeam {
        team: String,
        repo: String,
    },
    Reviewers {
        workspace_key: WorkspaceKey,
        logins: Vec<String>,
    },
    Work {
        target: WorkPickTarget,
        session_id: Option<SessionId>,
        model_alias: Option<String>,
    },
    Labels {
        workspace_key: WorkspaceKey,
        names: Vec<String>,
    },
    Assignees {
        workspace_key: WorkspaceKey,
        logins: Vec<String>,
    },
    MountImportConfirm(DiscoveredCheckoutDto),
    SetFilters(Vec<F>),
    MountInspectConfirm(WorktreeInspectionDto),
    ProvisionEditor {
        workspace_key: SessionKey,
        editor: EditorTemplate,
        command: Command,
        notice: String,
    },
    LaunchEditor {
        editor: EditorTemplate,
        worktree: PathBuf,
    },
    /// Launch a picked "Open with…" app on the focused workspace
    /// (issue #1100). The renderer resolves tokens + spawns.
    LaunchOpenWith {
        app: OpenWithApp,
        ctx: OpenWithContext,
    },
    DispatchSettings(usize),
    Runner(Vec<usize>),
}

/// The explicit-trigger instruction injected when a skill is picked from
/// the `]]k` picker (#797). A skill is normally model-selected; this makes
/// the agent invoke `name` deterministically, the way the user fires a
/// snippet.
pub fn skill_trigger_prompt(name: &str) -> String {
    format!("Use the `{name}` skill.")
}

/// Decode picker payloads against the flow state that produced them.
pub fn resolve_pick<P: PickPayload>(picks: &[P], flow: PickFlow) -> PickOutcome<P::Filter> {
    match flow {
        PickFlow::BroadcastSnippet { active, snippets } => {
            if !active {
                return PickOutcome::NoOp;
            }
            let picked_key = picks.first().and_then(P::as_text);
            let snippet =
                picked_key.and_then(|key| snippets.iter().find(|snippet| snippet.key == key));
            PickOutcome::MountBroadcastComposer {
                snippet_key: snippet.map(|snippet| snippet.key.clone()),
                body: snippet.map(|snippet| snippet.body.clone()),
            }
        }
        PickFlow::Snippet {
            terminal_id,
            snippets,
            submit,
        } => {
            let Some(key) = picks.first().and_then(P::as_text) else {
                return PickOutcome::NoOp;
            };
            let Some(snippet) = snippets.iter().find(|snippet| snippet.key == key) else {
                return PickOutcome::StaleSnippet(key.to_string());
            };
            let Some(terminal_id) = terminal_id else {
                return PickOutcome::Commands {
                    commands: Vec::new(),
                    notice: Some("no active terminal — open a session first".to_string()),
                };
            };
            if submit {
                PickOutcome::Commands {
                    commands: vec![Command::DeliverSnippet {
                        terminal_id,
                        snippet_key: snippet.key.clone(),
                        category: snippet.category.clone(),
                        body: snippet.body.clone(),
                        submit: true,
                    }],
                    notice: None,
                }
            } else {
                PickOutcome::InsertSnippetDraft {
                    terminal_id,
                    snippet_key: snippet.key.clone(),
                    category: snippet.category.clone(),
                    body: snippet.body.clone(),
                }
            }
        }
        PickFlow::Skill { terminal_id } => {
            let Some(name) = picks.first().and_then(P::as_text) else {
                return PickOutcome::NoOp;
            };
            let Some(terminal_id) = terminal_id else {
                return PickOutcome::Commands {
                    commands: Vec::new(),
                    notice: Some("no active terminal — open a session first".to_string()),
                };
            };
            PickOutcome::TriggerSkill {
                terminal_id,
                text: skill_trigger_prompt(name),
                skill_name: name.to_string(),
            }
        }
        PickFlow::PromptHistory {
            terminal_id,
            terminal_is_agent,
        } => {
            let text = picks.first().and_then(P::as_text);
            match (text, terminal_id, terminal_is_agent) {
                (Some(text), Some(terminal_id), true) => PickOutcome::DeliverPrompt {
                    terminal_id,
                    text: text.to_string(),
                },
                (Some(_), Some(_), false) => PickOutcome::Commands {
                    commands: Vec::new(),
                    notice: Some("session ended — nothing re-sent".to_string()),
                },
                _ => PickOutcome::NoOp,
            }
        }
        PickFlow::Jump => picks
            .first()
            .and_then(P::session)
            .map(PickOutcome::Jump)
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::Url => picks
            .first()
            .and_then(P::as_text)
            .map(|url| PickOutcome::OpenUrl(url.to_string()))
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::Theme => picks
            .first()
            .and_then(P::as_text)
            .map(|theme| PickOutcome::SaveTheme(theme.to_string()))
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::DefaultAgent => picks
            .first()
            .and_then(P::as_text)
            .map(|agent| PickOutcome::SaveDefaultAgent(agent.to_string()))
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::DefaultModel { agent_id } => {
            match (agent_id, picks.first().and_then(P::opt_text)) {
                (Some(agent_id), Some(alias)) => PickOutcome::SaveDefaultModel { agent_id, alias },
                _ => PickOutcome::NoOp,
            }
        }
        PickFlow::SidebarContext {
            session_key,
            actions,
        } => match (session_key, picks.first().and_then(P::as_index)) {
            (Some(session_key), Some(index)) => actions
                .get(index)
                .cloned()
                .map(|action| PickOutcome::DispatchAction {
                    session_key,
                    action,
                })
                .unwrap_or(PickOutcome::NoOp),
            _ => PickOutcome::NoOp,
        },
        PickFlow::Adopt { source } => match (source, picks.first().and_then(P::workspace)) {
            (Some(source_workspace_key), Some(target_workspace_key)) => PickOutcome::Commands {
                notice: Some(format!(
                    "adopted sessions: {source_workspace_key} → {target_workspace_key}"
                )),
                commands: vec![Command::AdoptSessions {
                    source_workspace_key,
                    target_workspace_key,
                }],
            },
            _ => PickOutcome::NoOp,
        },
        PickFlow::HandoffTarget { active } => {
            if !active {
                return PickOutcome::NoOp;
            }
            picks
                .first()
                .and_then(P::session)
                .map(|target| PickOutcome::MountHandoffComposer { target })
                .unwrap_or(PickOutcome::NoOp)
        }
        PickFlow::ConvertSession { active } => {
            if !active {
                return PickOutcome::NoOp;
            }
            picks
                .first()
                .and_then(P::handoff_role)
                .map(|role| PickOutcome::StartSessionConversion { role })
                .unwrap_or(PickOutcome::NoOp)
        }
        PickFlow::StartAgentProject => picks
            .first()
            .and_then(P::project)
            .map(PickOutcome::MountNewWorkspace)
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::NewWorkspaceRepo => match picks.first() {
            Some(payload) if payload.is_new_local_project() => PickOutcome::MountNewProject,
            Some(payload) => payload
                .project()
                .map(PickOutcome::MountNewWorkspace)
                .unwrap_or(PickOutcome::NoOp),
            None => PickOutcome::NoOp,
        },
        PickFlow::LinearTeamRepo { team } => picks
            .first()
            .and_then(P::as_text)
            .map(|repo| PickOutcome::MapLinearTeam {
                team,
                repo: repo.to_string(),
            })
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::MoveToSpace { source, entries } => {
            match (source, picks.first().and_then(P::as_index)) {
                (Some(source), Some(idx)) => match entries.get(idx) {
                    Some(SpacePickEntry::Assign(name)) => PickOutcome::AssignSpace {
                        source,
                        space: Some(name.clone()),
                    },
                    Some(SpacePickEntry::Unassign) => PickOutcome::AssignSpace {
                        source,
                        space: None,
                    },
                    Some(SpacePickEntry::NewSpace) => PickOutcome::MountMoveToSpaceInput { source },
                    None => PickOutcome::NoOp,
                },
                _ => PickOutcome::NoOp,
            }
        }
        PickFlow::HeaderContext { actions } => picks
            .first()
            .and_then(P::as_index)
            .and_then(|idx| actions.get(idx).cloned())
            .map(|action| PickOutcome::DispatchCursorAction { action })
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::Reviewers { workspace_key } => {
            let logins = text_values(picks);
            match (workspace_key, logins.is_empty()) {
                (Some(workspace_key), false) => PickOutcome::Reviewers {
                    workspace_key,
                    logins,
                },
                _ => PickOutcome::NoOp,
            }
        }
        PickFlow::Policy { workspace } => {
            let Some(policy) = picks.first().and_then(P::policy) else {
                return PickOutcome::NoOp;
            };
            let Some(workspace) = workspace else {
                return PickOutcome::NoOp;
            };
            let session_key = SessionKey::from(&workspace.key);
            match policy {
                PolicyPick::MergeOnGreen => {
                    // Config-agnostic: the daemon's `set_auto_merge_on_green`
                    // owns the author-gate refusal against its authoritative
                    // config, so this menu never pre-judges it (#845).
                    let enabled = !workspace.auto_merge_on_green;
                    PickOutcome::Commands {
                        commands: vec![Command::SetAutoMergeOnGreen {
                            session_key,
                            enabled,
                        }],
                        notice: Some(if enabled {
                            "merge on green: armed".to_string()
                        } else {
                            "merge on green: off".to_string()
                        }),
                    }
                }
                PolicyPick::AutoFix(kind) => {
                    let next = lazybox_core::toggled_arm(workspace.policies.arm(kind));
                    let name = match kind {
                        AutoFixKind::CiFailure => "auto-fix CI",
                        AutoFixKind::MergeConflict => "auto-fix conflict",
                    };
                    let state = match next {
                        lazybox_core::PolicyArm::Default => "follows config",
                        lazybox_core::PolicyArm::Arm => "armed",
                        lazybox_core::PolicyArm::Disarm => "disarmed",
                    };
                    PickOutcome::Commands {
                        commands: vec![Command::SetAutoFixPolicy {
                            session_key,
                            kind,
                            arm: next,
                        }],
                        notice: Some(format!("{name}: {state}")),
                    }
                }
                PolicyPick::Info(message) => PickOutcome::Commands {
                    commands: Vec::new(),
                    notice: Some(message),
                },
            }
        }
        PickFlow::WorkAgent { picker } => {
            let Some(picker) = picker else {
                return PickOutcome::NoOp;
            };
            let Some(index) = picks.first().and_then(P::as_index) else {
                return PickOutcome::NoOp;
            };
            picker
                .targets
                .get(index)
                .cloned()
                .map(|target| PickOutcome::Work {
                    target,
                    session_id: picker.session_id,
                    model_alias: picker.model_alias,
                })
                .unwrap_or(PickOutcome::NoOp)
        }
        PickFlow::Snooze { session_key, now } => {
            match (session_key, picks.first().and_then(P::as_duration)) {
                (Some(session_key), Some(duration)) => {
                    let until = now
                        + chrono::Duration::from_std(duration)
                            .unwrap_or(chrono::Duration::hours(4));
                    PickOutcome::Commands {
                        commands: vec![Command::Snooze { session_key, until }],
                        notice: Some(format!("snoozed for {}", duration_label(duration))),
                    }
                }
                _ => PickOutcome::NoOp,
            }
        }
        PickFlow::Labels { workspace_key } => match workspace_key {
            Some(workspace_key) => PickOutcome::Labels {
                workspace_key,
                names: text_values(picks),
            },
            None => PickOutcome::NoOp,
        },
        PickFlow::Assignees { workspace_key } => match workspace_key {
            Some(workspace_key) => PickOutcome::Assignees {
                workspace_key,
                logins: text_values(picks),
            },
            None => PickOutcome::NoOp,
        },
        PickFlow::Import { rows } => picks
            .first()
            .and_then(P::as_index)
            .and_then(|index| rows.get(index).cloned())
            .map(PickOutcome::MountImportConfirm)
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::Filters => PickOutcome::SetFilters(picks.iter().filter_map(P::filter).collect()),
        PickFlow::Inspect { rows } => resolve_inspect_pick(picks, rows),
        PickFlow::Editor {
            choices,
            pending_workspace,
            worktree,
        } => {
            let Some(editor) = picks
                .first()
                .and_then(P::as_index)
                .and_then(|index| choices.get(index).cloned())
            else {
                return PickOutcome::NoOp;
            };
            if let Some(workspace_key) = pending_workspace {
                let command = Command::Spawn {
                    model_alias: None,
                    session_key: workspace_key.clone(),
                    session_id: None,
                    client_request_id: None,
                    kind: TerminalKind::Shell,
                    cwd: None,
                    initial_prompt: None,
                    initial_snippet: None,
                    on_main: false,
                    access: lazybox_ipc::AgentRunAccess::Default,
                };
                let notice = format!(
                    "Provisioning worktree for {workspace_key} — opening in {} when ready…",
                    editor.display
                );
                PickOutcome::ProvisionEditor {
                    workspace_key,
                    editor,
                    command,
                    notice,
                }
            } else {
                match worktree {
                    Some(worktree) => PickOutcome::LaunchEditor { editor, worktree },
                    None => PickOutcome::NoOp,
                }
            }
        }
        PickFlow::OpenWith { apps, ctx } => picks
            .first()
            .and_then(P::as_index)
            .and_then(|index| apps.get(index).cloned())
            .map(|app| PickOutcome::LaunchOpenWith { app, ctx })
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::Settings { action_count } => picks
            .first()
            .and_then(P::as_index)
            .filter(|index| *index < action_count)
            .map(PickOutcome::DispatchSettings)
            .unwrap_or(PickOutcome::NoOp),
        PickFlow::Runner => PickOutcome::Runner(picks.iter().filter_map(P::as_index).collect()),
        PickFlow::Plain => PickOutcome::Pop,
    }
}

fn text_values<P: PickPayload>(picks: &[P]) -> Vec<String> {
    picks
        .iter()
        .filter_map(P::as_text)
        .map(str::to_string)
        .collect()
}

fn duration_label(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / 60 / 24)
    }
}

fn resolve_inspect_pick<P: PickPayload>(
    picks: &[P],
    rows: Vec<WorktreeInspectionDto>,
) -> PickOutcome<P::Filter> {
    let Some(index) = picks.first().and_then(P::as_index) else {
        return PickOutcome::NoOp;
    };
    let safe_first = rows
        .iter()
        .any(|row| !row.reasons.is_empty() && row.is_safe_to_delete);
    if safe_first && index == 0 {
        let commands = rows
            .iter()
            .filter(|row| !row.reasons.is_empty() && row.is_safe_to_delete)
            .map(|row| Command::DeleteOrphanedWorktree {
                path: row.path.clone(),
                force: false,
            })
            .collect::<Vec<_>>();
        return PickOutcome::Commands {
            notice: Some(format!(
                "deleting {} clearly-safe worktrees…",
                commands.len()
            )),
            commands,
        };
    }
    let row_index = if safe_first { index - 1 } else { index };
    rows.get(row_index)
        .cloned()
        .map(PickOutcome::MountInspectConfirm)
        .unwrap_or(PickOutcome::NoOp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editors::UserEditorEntry;
    use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};
    use std::path::PathBuf;

    #[derive(Clone)]
    enum Payload {
        Index(usize),
        Text(String),
        OptText(Option<String>),
        Duration(Duration),
        Filter(u8),
        Policy(PolicyPick),
        Workspace(WorkspaceKey),
        Project(ProjectKey),
        NewLocal,
        Session(SessionKey),
        HandoffRole(AgentHandoffRole),
    }

    impl PickPayload for Payload {
        type Filter = u8;

        fn as_index(&self) -> Option<usize> {
            match self {
                Self::Index(index) => Some(*index),
                _ => None,
            }
        }

        fn as_text(&self) -> Option<&str> {
            match self {
                Self::Text(text) => Some(text),
                _ => None,
            }
        }

        fn opt_text(&self) -> Option<Option<String>> {
            match self {
                Self::OptText(value) => Some(value.clone()),
                _ => None,
            }
        }

        fn as_duration(&self) -> Option<Duration> {
            match self {
                Self::Duration(duration) => Some(*duration),
                _ => None,
            }
        }

        fn filter(&self) -> Option<<Self as PickPayload>::Filter> {
            match self {
                Self::Filter(filter) => Some(*filter),
                _ => None,
            }
        }

        fn policy(&self) -> Option<PolicyPick> {
            match self {
                Self::Policy(policy) => Some(policy.clone()),
                _ => None,
            }
        }

        fn workspace(&self) -> Option<WorkspaceKey> {
            match self {
                Self::Workspace(key) => Some(key.clone()),
                _ => None,
            }
        }

        fn project(&self) -> Option<ProjectKey> {
            match self {
                Self::Project(key) => Some(key.clone()),
                _ => None,
            }
        }

        fn is_new_local_project(&self) -> bool {
            matches!(self, Self::NewLocal)
        }

        fn session(&self) -> Option<SessionKey> {
            match self {
                Self::Session(key) => Some(key.clone()),
                _ => None,
            }
        }

        fn handoff_role(&self) -> Option<AgentHandoffRole> {
            match self {
                Self::HandoffRole(role) => Some(*role),
                _ => None,
            }
        }
    }

    fn workspace() -> Workspace {
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: "owner/repo#1".into(),
            },
            title: "PR".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::Success,
            review: ReviewStatus::Approved,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/owner/repo/pull/1".into(),
            repo: Some("owner/repo".into()),
            branch: Some("feature".into()),
            base_branch: Some("main".into()),
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            assignees: vec![],
            reviewers: vec![],
            reviews: vec![],
            labels: vec![],
            mergeable: lazybox_core::Mergeable::Mergeable,
            auto_merge_enabled: false,
            is_in_merge_queue: false,
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
        Workspace::from_task(task, Utc::now())
    }

    fn inspection(path: &str, safe: bool) -> WorktreeInspectionDto {
        WorktreeInspectionDto {
            path: PathBuf::from(path),
            bare_path: None,
            branch: None,
            session_id: None,
            reasons: vec!["orphan".into()],
            size_bytes: 0,
            last_modified_unix: None,
            has_uncommitted_changes: false,
            has_unpushed_commits: false,
            is_safe_to_delete: safe,
        }
    }

    #[test]
    fn text_shaped_flows_decode_to_typed_outcomes() {
        let snippets = vec![SnippetPick {
            key: "review".into(),
            category: "git".into(),
            body: "Review this".into(),
        }];
        let picked = [Payload::Text("review".into())];

        assert!(matches!(
            resolve_pick(
                &picked,
                PickFlow::BroadcastSnippet {
                    active: true,
                    snippets: snippets.clone(),
                },
            ),
            PickOutcome::MountBroadcastComposer {
                snippet_key: Some(key),
                body: Some(body),
            } if key == "review" && body == "Review this"
        ));
        assert!(matches!(
            resolve_pick(
                &picked,
                PickFlow::Snippet {
                    terminal_id: Some(TerminalId(4)),
                    snippets: snippets.clone(),
                    submit: true,
                },
            ),
            PickOutcome::Commands { commands, .. }
                if matches!(commands.as_slice(), [Command::DeliverSnippet {
                    terminal_id: TerminalId(4),
                    submit: true,
                    ..
                }])
        ));
        // Shift-Enter resolves the same snippet to an insert-without-submit
        // outcome the renderer expands into a compose-only delivery plus a
        // composing-buffer update (issue #791).
        assert!(matches!(
            resolve_pick(
                &picked,
                PickFlow::Snippet {
                    terminal_id: Some(TerminalId(4)),
                    snippets: snippets.clone(),
                    submit: false,
                },
            ),
            PickOutcome::InsertSnippetDraft {
                terminal_id: TerminalId(4),
                body,
                ..
            } if body == "Review this"
        ));
        assert!(matches!(
            resolve_pick(&picked, PickFlow::Theme),
            PickOutcome::SaveTheme(theme) if theme == "review"
        ));
        assert!(matches!(
            resolve_pick(&picked, PickFlow::DefaultAgent),
            PickOutcome::SaveDefaultAgent(agent) if agent == "review"
        ));
        assert!(matches!(
            resolve_pick(&picked, PickFlow::Url),
            PickOutcome::OpenUrl(url) if url == "review"
        ));
    }

    #[test]
    fn skill_pick_triggers_explicit_invocation() {
        let picked = [Payload::Text("code-review".into())];
        match resolve_pick(
            &picked,
            PickFlow::Skill {
                terminal_id: Some(TerminalId(7)),
            },
        ) {
            PickOutcome::TriggerSkill {
                terminal_id,
                skill_name,
                text,
            } => {
                assert_eq!(terminal_id, TerminalId(7));
                assert_eq!(skill_name, "code-review");
                assert_eq!(text, "Use the `code-review` skill.");
            }
            other => panic!("skill pick must trigger, got {other:?}"),
        }
    }

    #[test]
    fn skill_pick_without_terminal_notices() {
        let picked = [Payload::Text("code-review".into())];
        assert!(matches!(
            resolve_pick(&picked, PickFlow::Skill { terminal_id: None }),
            PickOutcome::Commands { commands, notice }
                if commands.is_empty() && notice.is_some()
        ));
    }

    #[test]
    fn skill_pick_without_selection_is_noop() {
        assert!(matches!(
            resolve_pick(
                &[] as &[Payload],
                PickFlow::Skill {
                    terminal_id: Some(TerminalId(7)),
                },
            ),
            PickOutcome::NoOp
        ));
    }

    #[test]
    fn skill_trigger_prompt_names_the_skill() {
        assert_eq!(skill_trigger_prompt("deploy"), "Use the `deploy` skill.");
    }

    #[test]
    fn target_flows_preserve_typed_keys() {
        let workspace_key = WorkspaceKey::new("source");
        let target_key = WorkspaceKey::new("target");
        let project_key = ProjectKey::local("project");
        let session_key = SessionKey::new("session");

        assert!(matches!(
            resolve_pick(
                &[Payload::Workspace(target_key.clone())],
                PickFlow::Adopt {
                    source: Some(workspace_key.clone()),
                },
            ),
            PickOutcome::Commands { commands, .. }
                if matches!(commands.as_slice(), [Command::AdoptSessions {
                    source_workspace_key,
                    target_workspace_key,
                }] if source_workspace_key == &workspace_key
                    && target_workspace_key == &target_key)
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::Project(project_key.clone())],
                PickFlow::StartAgentProject,
            ),
            PickOutcome::MountNewWorkspace(key) if key == project_key
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::Session(session_key.clone())],
                PickFlow::HandoffTarget { active: true },
            ),
            PickOutcome::MountHandoffComposer { target } if target == session_key
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::HandoffRole(AgentHandoffRole::Critic)],
                PickFlow::ConvertSession { active: true },
            ),
            PickOutcome::StartSessionConversion {
                role: AgentHandoffRole::Critic,
            }
        ));
        assert!(matches!(
            resolve_pick(&[Payload::NewLocal], PickFlow::NewWorkspaceRepo),
            PickOutcome::MountNewProject
        ));
    }

    /// #1041: the unmapped-team repo picker resolves the picked `owner/repo`
    /// text into a `MapLinearTeam` outcome carrying the team it was opened
    /// for; an empty pick is a no-op rather than a partial mapping.
    #[test]
    fn linear_team_repo_pick_maps_the_team() {
        assert!(matches!(
            resolve_pick(
                &[Payload::Text("obin-ai/obin-platform".into())],
                PickFlow::LinearTeamRepo { team: "OBI".into() },
            ),
            PickOutcome::MapLinearTeam { team, repo }
                if team == "OBI" && repo == "obin-ai/obin-platform"
        ));
        assert!(matches!(
            resolve_pick::<Payload>(&[], PickFlow::LinearTeamRepo { team: "OBI".into() }),
            PickOutcome::NoOp
        ));
    }

    #[test]
    fn multi_select_and_duration_flows_build_commands() {
        let workspace_key = WorkspaceKey::new("workspace");
        let logins = [Payload::Text("alice".into()), Payload::Text("bob".into())];
        assert!(matches!(
            resolve_pick(
                &logins,
                PickFlow::Reviewers {
                    workspace_key: Some(workspace_key.clone()),
                },
            ),
            PickOutcome::Reviewers { logins, .. } if logins == ["alice", "bob"]
        ));
        assert!(matches!(
            resolve_pick(
                &[] as &[Payload],
                PickFlow::Labels {
                    workspace_key: Some(workspace_key.clone()),
                },
            ),
            PickOutcome::Labels { names, .. } if names.is_empty()
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::Duration(Duration::from_secs(3_600))],
                PickFlow::Snooze {
                    session_key: Some(SessionKey::from(&workspace_key)),
                    now: Utc::now(),
                },
            ),
            PickOutcome::Commands {
                commands,
                notice: Some(notice),
            } if matches!(commands.as_slice(), [Command::Snooze { .. }])
                && notice == "snoozed for 1h"
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::Filter(2), Payload::Filter(7)],
                PickFlow::Filters,
            ),
            PickOutcome::SetFilters(filters) if filters == [2, 7]
        ));
    }

    /// #1206: last-used wins, current Space is the fallback, and a
    /// vanished last-used degrades instead of misfiling.
    #[test]
    fn space_preselect_prefers_last_then_current_then_first() {
        let spaces = vec!["Work".to_string(), "Later".to_string()];
        assert_eq!(space_preselect_index(&spaces, Some("Later"), "Work"), 1);
        assert_eq!(space_preselect_index(&spaces, None, "Later"), 1);
        assert_eq!(space_preselect_index(&spaces, Some("Gone"), "Work"), 0);
        assert_eq!(space_preselect_index(&spaces, None, "obin-ai"), 0);
    }

    /// #1206: the move-to-Space picker resolves assign / unassign /
    /// new-Space by entries index, and degrades to `NoOp` (never a wrong
    /// assignment) on a stale index or a lost source stash.
    #[test]
    fn move_to_space_pick_resolves_assign_unassign_and_new() {
        let entries = vec![
            SpacePickEntry::Assign("Work".into()),
            SpacePickEntry::Assign("Later".into()),
            SpacePickEntry::Unassign,
            SpacePickEntry::NewSpace,
        ];
        let flow = |source: Option<&str>| PickFlow::MoveToSpace {
            source: source.map(str::to_string),
            entries: entries.clone(),
        };
        assert!(matches!(
            resolve_pick(&[Payload::Index(1)], flow(Some("obin-ai/lazybox"))),
            PickOutcome::AssignSpace { source, space: Some(space) }
                if source == "obin-ai/lazybox" && space == "Later"
        ));
        assert!(matches!(
            resolve_pick(&[Payload::Index(2)], flow(Some("obin-ai/lazybox"))),
            PickOutcome::AssignSpace { space: None, .. }
        ));
        assert!(matches!(
            resolve_pick(&[Payload::Index(3)], flow(Some("obin-ai/lazybox"))),
            PickOutcome::MountMoveToSpaceInput { source } if source == "obin-ai/lazybox"
        ));
        assert!(matches!(
            resolve_pick(&[Payload::Index(9)], flow(Some("obin-ai/lazybox"))),
            PickOutcome::NoOp
        ));
        assert!(matches!(
            resolve_pick(&[Payload::Index(0)], flow(None)),
            PickOutcome::NoOp
        ));
    }

    #[test]
    fn policy_and_index_flows_resolve_against_their_stash() {
        let ws = workspace();
        assert!(matches!(
            resolve_pick(
                &[Payload::Policy(PolicyPick::MergeOnGreen)],
                PickFlow::Policy {
                    workspace: Some(Box::new(ws)),
                },
            ),
            PickOutcome::Commands { commands, .. }
                if matches!(commands.as_slice(), [Command::SetAutoMergeOnGreen {
                    enabled: true,
                    ..
                }])
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(1)],
                PickFlow::SidebarContext {
                    session_key: Some(SessionKey::new("workspace")),
                    actions: vec![Action::Refresh, Action::OpenHelp],
                },
            ),
            PickOutcome::DispatchAction {
                action: Action::OpenHelp,
                ..
            }
        ));
        assert!(matches!(
            resolve_pick(&[Payload::Index(0)], PickFlow::Settings { action_count: 1 },),
            PickOutcome::DispatchSettings(0)
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(3), Payload::Text("ignored".into())],
                PickFlow::Runner,
            ),
            PickOutcome::Runner(indices) if indices == [3]
        ));
    }

    #[test]
    fn inspect_flow_separates_bulk_and_per_row_picks() {
        let rows = vec![inspection("/safe", true), inspection("/unsafe", false)];
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(0)],
                PickFlow::Inspect { rows: rows.clone() },
            ),
            PickOutcome::Commands { commands, .. } if commands.len() == 1
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(2)],
                PickFlow::Inspect { rows },
            ),
            PickOutcome::MountInspectConfirm(row)
                if row.path.as_path() == std::path::Path::new("/unsafe")
        ));
    }

    #[test]
    fn optional_model_and_plain_flows_are_explicit() {
        assert!(matches!(
            resolve_pick(
                &[Payload::OptText(None)],
                PickFlow::DefaultModel {
                    agent_id: Some("codex".into()),
                },
            ),
            PickOutcome::SaveDefaultModel {
                agent_id,
                alias: None,
            } if agent_id == "codex"
        ));
        assert!(matches!(
            resolve_pick(&[] as &[Payload], PickFlow::Plain),
            PickOutcome::Pop
        ));
    }

    #[test]
    fn remaining_stashed_flows_resolve_without_a_model() {
        let terminal_id = TerminalId(8);
        assert!(matches!(
            resolve_pick(
                &[Payload::Text("continue".into())],
                PickFlow::PromptHistory {
                    terminal_id: Some(terminal_id),
                    terminal_is_agent: true,
                },
            ),
            PickOutcome::DeliverPrompt {
                terminal_id: id,
                text,
            } if id == terminal_id && text == "continue"
        ));

        let session_key = SessionKey::new("workspace");
        assert!(matches!(
            resolve_pick(
                &[Payload::Session(session_key.clone())],
                PickFlow::Jump,
            ),
            PickOutcome::Jump(key) if key == session_key
        ));

        let session_id = SessionId::new();
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(0)],
                PickFlow::WorkAgent {
                    picker: Some(WorkPickerState {
                        targets: vec![WorkPickTarget {
                            terminal_id,
                            agent_id: "codex".into(),
                        }],
                        session_id: Some(session_id),
                        model_alias: Some("L".into()),
                    }),
                },
            ),
            PickOutcome::Work {
                target,
                session_id: Some(id),
                model_alias: Some(alias),
            } if target.terminal_id == terminal_id
                && target.agent_id == "codex"
                && id == session_id
                && alias == "L"
        ));

        let workspace_key = WorkspaceKey::new("workspace");
        assert!(matches!(
            resolve_pick(
                &[] as &[Payload],
                PickFlow::Assignees {
                    workspace_key: Some(workspace_key),
                },
            ),
            PickOutcome::Assignees { logins, .. } if logins.is_empty()
        ));

        let checkout = DiscoveredCheckoutDto {
            path: PathBuf::from("/checkout"),
            repo: Some("owner/repo".into()),
            branch: Some("main".into()),
            has_uncommitted_changes: false,
        };
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(0)],
                PickFlow::Import {
                    rows: vec![checkout],
                },
            ),
            PickOutcome::MountImportConfirm(target)
                if target.path.as_path() == std::path::Path::new("/checkout")
        ));

        let editor = EditorTemplate::from(UserEditorEntry {
            id: "code".into(),
            display: Some("VS Code".into()),
            command: "code".into(),
            args: Some(vec!["{path}".into()]),
        });
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(0)],
                PickFlow::Editor {
                    choices: vec![editor.clone()],
                    pending_workspace: Some(session_key),
                    worktree: None,
                },
            ),
            PickOutcome::ProvisionEditor {
                editor: picked,
                command: Command::Spawn {
                    kind: TerminalKind::Shell,
                    ..
                },
                ..
            } if picked.id == editor.id
        ));
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(0)],
                PickFlow::Editor {
                    choices: vec![editor],
                    pending_workspace: None,
                    worktree: Some(PathBuf::from("/worktree")),
                },
            ),
            PickOutcome::LaunchEditor { worktree, .. }
                if worktree.as_path() == std::path::Path::new("/worktree")
        ));
    }

    #[test]
    fn open_with_pick_resolves_the_indexed_app() {
        let apps = vec![
            OpenWithApp {
                name: "Obsidian".into(),
                command: "open".into(),
                args: Some(vec!["-a".into(), "Obsidian".into(), "{path}".into()]),
                key: None,
            },
            OpenWithApp {
                name: "Finder".into(),
                command: "open".into(),
                args: None,
                key: None,
            },
        ];
        let ctx = OpenWithContext {
            path: Some("/worktree".into()),
            ..OpenWithContext::default()
        };
        assert!(matches!(
            resolve_pick(
                &[Payload::Index(1)],
                PickFlow::OpenWith {
                    apps: apps.clone(),
                    ctx: ctx.clone(),
                },
            ),
            PickOutcome::LaunchOpenWith { app, .. } if app.name == "Finder"
        ));
        // An out-of-range index (stale modal) is a no-op, never a panic.
        assert!(matches!(
            resolve_pick(&[Payload::Index(9)], PickFlow::OpenWith { apps, ctx }),
            PickOutcome::NoOp
        ));
    }
}
