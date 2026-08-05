//! Catalog-action dispatch: `dispatch_action` (destructive-gate
//! wrapper) and `dispatch_action_unchecked` (the actual fan-out).
//! Every catalog `Action` whose effect fits a single IpcCommand or
//! modal mount lands here. The keyboard, right-click menu, palette,
//! and future remap UI all funnel through `dispatch_action` so
//! behavior stays consistent across surfaces.

use super::{ActionConfirmTarget, Model, PaneFocus};
use lazybox_ipc::Command as IpcCommand;
use tuirealm::terminal::TerminalAdapter;

/// The ` #N` suffix parsed from a task id key
/// (`"github:o/r#42"` → `" #42"`), or empty when the key carries no
/// number. Feeds the pending close footer notices. (The
/// *workspace* key won't do — `sanitize_key` strips the `#`.)
fn task_number_suffix(task_key: &str) -> String {
    task_key
        .rsplit_once('#')
        .map(|(_, n)| format!(" #{n}"))
        .unwrap_or_default()
}

/// Cap a task title for confirm-modal copy (same 80-char convention
/// as the removal prompts) so a long title can't blow up the modal.
fn truncate_title(title: &str) -> String {
    if title.chars().count() > 80 {
        let truncated: String = title.chars().take(79).collect();
        format!("{truncated}…")
    } else {
        title.to_string()
    }
}

impl<T: TerminalAdapter> Model<T> {
    fn execute_dispatch_intent(
        &mut self,
        intent: crate::intent::Intent,
        workspace: Option<&lazybox_core::Workspace>,
    ) -> Vec<IpcCommand> {
        use crate::intent::Intent;

        if let Some(notice) = crate::intent::pending_notice(&intent, workspace) {
            self.flash_info(notice);
        }
        match intent {
            Intent::MergePr { workspace_key } => {
                vec![IpcCommand::MergePr { workspace_key }]
            }
            Intent::UpdateBranch { workspace_key } => {
                vec![IpcCommand::UpdateBranch { workspace_key }]
            }
            Intent::Snooze {
                session_key,
                duration,
            } => {
                let until = chrono::Utc::now()
                    + chrono::Duration::from_std(duration)
                        .unwrap_or_else(|_| chrono::Duration::days(365));
                vec![IpcCommand::Snooze { session_key, until }]
            }
            Intent::MarkAllRead { session_key } => {
                vec![IpcCommand::MarkRead { session_key }]
            }
            Intent::MarkActivitiesRead {
                session_key,
                targets,
                optimistic,
                notice,
            } => {
                if let Some(notice) = notice {
                    self.flash_info(notice);
                }
                if optimistic {
                    for target in &targets {
                        self.right.mark_activity_read_locally(target.index);
                    }
                }
                targets
                    .into_iter()
                    .map(|target| IpcCommand::MarkActivityRead {
                        session_key: session_key.clone(),
                        index: target.index,
                        fingerprint: target.fingerprint,
                    })
                    .collect()
            }
            Intent::UpdateSelectedBranches {
                workspace_keys,
                selected_count: _,
            } => {
                if !workspace_keys.is_empty() {
                    self.sidebar.clear_broadcast_selection();
                    self.redraw = true;
                }
                workspace_keys
                    .into_iter()
                    .map(|workspace_key| IpcCommand::UpdateBranch { workspace_key })
                    .collect()
            }
            Intent::CollapseIntoPr {
                issue_workspace_key,
            } => vec![IpcCommand::CollapseIntoPr {
                issue_workspace_key,
            }],
            Intent::MountHandoffPicker {
                source_key,
                source_name,
                seed,
                notice,
            } => {
                if let Some(notice) = notice {
                    self.flash_info(notice);
                }
                self.mount_handoff_picker(&source_key, source_name, seed);
                Vec::new()
            }
            Intent::Notice(notice) => {
                self.flash_info(notice);
                Vec::new()
            }
            Intent::NoOp
            | Intent::SpawnAgent { .. }
            | Intent::SpawnShell { .. }
            | Intent::MountReply { .. }
            | Intent::MountNewWorkspaceInput { .. }
            | Intent::MountAdoptPicker { .. }
            | Intent::OpenEditor
            | Intent::SetAutoMergeOnGreen { .. }
            | Intent::SetTrackMain { .. }
            | Intent::KillWorkspace { .. }
            | Intent::Unsnooze { .. } => Vec::new(),
        }
    }

    /// Single fan-out from a catalog `Action` to its effect (IPC
    /// command, modal mount, focus shift, …). Surfaces (keyboard,
    /// right-click menu, future remap UI) all call this so behavior
    /// stays consistent across them.
    ///
    /// **Returns** the IPC commands the action produces, if any.
    /// UI-only effects (modal mounts, focus moves) happen via
    /// `&mut self` and aren't reflected in the return.
    pub fn dispatch_action(
        &mut self,
        action: &lazybox_tui_core::action::Action,
    ) -> Vec<IpcCommand> {
        use lazybox_tui_core::action::ActionDef;
        // Destructive gate, type-system enforced via the catalog.
        // Every destructive action is routed through the unified
        // Confirm modal first; the pending action lives in
        // `ModalFlow::ActionConfirm` and fires on `Msg::Confirmed(true)`.
        // This is the *only* path through `dispatch_action` for
        // destructive variants — there's no way to fire one
        // without the user confirming.
        if ActionDef::for_action(action).is_destructive() {
            // Resolve the concrete target NOW, while the selection
            // is the row the user acted on. The confirm fires
            // against this stash — see `ModalFlow::ActionConfirm`.
            let Some(target) = self.resolve_action_confirm_target() else {
                // Nothing focused to act on. The catalog's
                // availability gate keeps surfaces from offering the
                // action here; drop silently like the unchecked path
                // would have.
                return Vec::new();
            };
            // Archive is the only destructive action that applies to a
            // project header (it deletes the project + cascades). Any
            // other — e.g. `x z` long-snooze pressed while the
            // cursor sits on a project header with no workspace
            // selected — has nothing to act on, so drop it silently
            // rather than mount a confirm that would no-op on Yes.
            if matches!(target, ActionConfirmTarget::Project(_))
                && !matches!(action, lazybox_tui_core::action::Action::Archive)
            {
                return Vec::new();
            }
            // Project-header focused Archive deletes the whole
            // project (cascading to its workspaces) — the default
            // confirm prompt assumes a workspace target, which would
            // be a confusing lie. Compute a tailored prompt for that
            // case and let the rest fall through to the static
            // catalog prompt.
            let custom_prompt = self.action_confirm_override(action);
            self.mount_action_confirm(action.clone(), target, custom_prompt);
            return Vec::new();
        }
        self.dispatch_action_unchecked(action)
    }

    /// Resolve what a destructive action mounted right now would act
    /// on: the selected workspace row, or (for project-header focus)
    /// the focused project. None when neither is focused.
    fn resolve_action_confirm_target(&self) -> Option<ActionConfirmTarget> {
        if let Some(sk) = self.sidebar.selected_workspace_key() {
            return Some(ActionConfirmTarget::Workspace(sk.clone()));
        }
        self.sidebar
            .focused_project_key()
            .map(ActionConfirmTarget::Project)
    }

    /// Carry out a destructive action the user just confirmed,
    /// against the target stashed at mount time. The stash — not the
    /// live sidebar selection — names the row the prompt described;
    /// if it no longer exists (removed by a daemon event while the
    /// modal was up) this no-ops with a footer notice instead of
    /// firing at whatever the cursor drifted onto.
    pub(crate) fn dispatch_action_confirmed(
        &mut self,
        action: &lazybox_tui_core::action::Action,
        target: &ActionConfirmTarget,
    ) -> Vec<IpcCommand> {
        use lazybox_tui_core::action::Action;
        match target {
            ActionConfirmTarget::Workspace(session_key) => {
                let workspace = self.sidebar.workspace_by_key(session_key).cloned();
                if workspace.is_none() {
                    self.flash_info("workspace is gone — nothing to do");
                    return Vec::new();
                }
                match action {
                    Action::Archive => {
                        // Optimistic: drop the row now so archive feels
                        // instant instead of waiting for the daemon's
                        // `WorkspaceRemoved` echo. A failed delete
                        // re-inserts it (#476).
                        self.optimistic_remove_workspace(session_key);
                        vec![IpcCommand::Kill {
                            session_key: session_key.clone(),
                        }]
                    }
                    Action::CloseIssue => match workspace.as_ref() {
                        // Re-check against the STASHED workspace — a poll
                        // could have closed the issue or attached a PR
                        // while the modal was up. Same predicate the
                        // catalog gates the keypress on, so the two never
                        // disagree.
                        Some(ws)
                            if lazybox_tui_core::action::availability(
                                lazybox_tui_core::action::ActionKind::CloseIssue,
                                Some(ws),
                            ) =>
                        {
                            // Pending feedback at command send — the
                            // provider round trip takes seconds, and a
                            // silent gap after "Yes" reads as a dropped
                            // keypress (same convention as CollapseIntoPr's
                            // "joining into PR…").
                            self.flash_info(format!(
                                "closing issue{}…",
                                task_number_suffix(
                                    ws.gh_issues
                                        .first()
                                        .map(|i| i.id.key.as_str())
                                        .unwrap_or("")
                                )
                            ));
                            vec![IpcCommand::CloseIssue {
                                workspace_key: ws.key.clone(),
                            }]
                        }
                        _ => {
                            self.flash_info("issue is no longer open — nothing to close");
                            Vec::new()
                        }
                    },
                    Action::DeleteOrClose => match workspace.as_ref() {
                        // Re-check against the STASHED workspace — the
                        // item may have merged/closed or the workspace
                        // changed shape while the modal was up. Same
                        // predicate the catalog gates the keypress on.
                        Some(ws)
                            if lazybox_tui_core::action::availability(
                                lazybox_tui_core::action::ActionKind::DeleteOrClose,
                                Some(ws),
                            ) =>
                        {
                            // Pending feedback at command send — the
                            // provider round trip takes seconds (same
                            // convention as merge/close).
                            if let Some(pr) = ws.pr.as_ref() {
                                self.flash_info(format!(
                                    "closing PR{}…",
                                    task_number_suffix(&pr.id.key)
                                ));
                            } else {
                                self.flash_info(format!(
                                    "deleting issue{}…",
                                    task_number_suffix(
                                        ws.gh_issues
                                            .first()
                                            .map(|i| i.id.key.as_str())
                                            .unwrap_or("")
                                    )
                                ));
                            }
                            vec![IpcCommand::DeleteOrClose {
                                workspace_key: ws.key.clone(),
                            }]
                        }
                        _ => {
                            self.flash_info(
                                "the issue / PR is no longer open — nothing to delete or close",
                            );
                            Vec::new()
                        }
                    },
                    Action::LongSnooze => {
                        let intent = crate::intent::resolve_long_snooze(
                            workspace.as_ref(),
                            self.ui_defaults.long_snooze,
                        );
                        self.execute_dispatch_intent(intent, workspace.as_ref())
                    }
                    Action::MergePr => {
                        // Re-check merge preconditions against the
                        // STASHED workspace — state may have moved
                        // (new failing CI, fresh conflict) while the
                        // modal was up.
                        let intent = crate::intent::resolve_merge(workspace.as_ref());
                        if matches!(intent, crate::intent::Intent::MergePr { .. }) {
                            self.execute_dispatch_intent(intent, workspace.as_ref())
                        } else {
                            let reason = workspace
                                .as_ref()
                                .and_then(|w| w.pr.as_ref())
                                .and_then(crate::intent::merge_block_reason)
                                .unwrap_or("the PR is no longer merge-ready");
                            self.flash_info(format!("can't merge: {reason}"));
                            Vec::new()
                        }
                    }
                    // On-main spawns fire against the STASHED workspace,
                    // not the live selection — a daemon event could have
                    // drifted the sidebar cursor while the confirm was up,
                    // and `dispatch_action_unchecked` would otherwise
                    // launch a shared-branch session on whatever the
                    // cursor landed on. `on_main` always targets the
                    // shared checkout, so `session_id` is None.
                    Action::SpawnAgentOnMain(agent_id) => vec![IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: session_key.clone(),
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                        on_main: true,
                    }],
                    Action::SpawnShellOnMain => vec![IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: session_key.clone(),
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Shell,
                        cwd: None,
                        initial_prompt: None,
                        on_main: true,
                    }],
                    // A future destructive action that hasn't grown a
                    // targeted arm yet falls back to the legacy
                    // selection-based dispatch.
                    other => self.dispatch_action_unchecked(other),
                }
            }
            ActionConfirmTarget::Project(project_key) => {
                if !self.projects.contains_key(project_key) {
                    self.flash_info("project is gone — nothing to do");
                    return Vec::new();
                }
                match action {
                    Action::Archive => {
                        // Optimistic: drop the project header + its child
                        // rows now; a failed cascade re-inserts them all
                        // (#476).
                        self.optimistic_remove_project(project_key);
                        vec![IpcCommand::DeleteProject {
                            project_key: project_key.clone(),
                        }]
                    }
                    other => self.dispatch_action_unchecked(other),
                }
            }
        }
    }

    /// Internal: actually carry out an action without checking the
    /// destructive flag. Public `dispatch_action` gates on
    /// `is_destructive` and routes through the Confirm modal for
    /// the destructive ones — this method is what the modal's
    /// `Msg::Confirmed(true)` handler calls AFTER the user
    /// approved.
    ///
    /// Callers OTHER than `dispatch_action` and the
    /// `ActionConfirm` Yes-handler must not exist. Keeping it
    /// `pub(crate)` so the type system makes that hard to break.
    /// Pick a tailored confirm-modal prompt for the destructive
    /// `action` at its current focus context. Returns `None` to fall
    /// back to the static `ActionDef::confirm_prompt`.
    ///
    /// Today the only override is `Archive` against a project
    /// header — the workspace-focused phrasing ("Archive the focused
    /// workspace?") is wrong for "delete this project and N
    /// workspaces under it." Adding more overrides here is the right
    /// growth path — keeps catalog defaults declarative and the
    /// context-sensitive copy out of the dispatch.
    fn action_confirm_override(&self, action: &lazybox_tui_core::action::Action) -> Option<String> {
        use lazybox_tui_core::action::Action;
        // Delete/close names its exact target — the number + title of
        // the issue/PR the confirmed keypress destroys — so the modal
        // never asks about an ambiguous "this".
        if matches!(action, Action::DeleteOrClose) {
            let sk = self.sidebar.selected_workspace_key()?;
            let ws = self.sidebar.workspace_by_key(sk)?;
            return Some(match ws.pr.as_ref() {
                Some(pr) => format!(
                    "Close PR {} — \"{}\" — without merging? Reopen on GitHub to undo.",
                    pr.id.key,
                    truncate_title(&pr.title),
                ),
                None => {
                    let issue = ws.gh_issues.first()?;
                    format!(
                        "Delete issue {} — \"{}\"? Deletion is permanent; without admin \
                         rights it is closed as not-planned instead.",
                        issue.id.key,
                        truncate_title(&issue.title),
                    )
                }
            });
        }
        if !matches!(action, Action::Archive) {
            return None;
        }
        // Workspace focus → use the default prompt.
        if self.sidebar.selected_workspace_key().is_some() {
            return None;
        }
        // Project header focus → custom phrasing.
        let project_key = self.sidebar.focused_project_key()?;
        let project_label = self
            .sidebar
            .project_label_for(&project_key)
            .unwrap_or_else(|| project_key.as_str().to_string());
        let child_count = self.sidebar.workspaces_in_project(&project_key);
        Some(match child_count {
            0 => format!("Delete project `{project_label}`?"),
            1 => format!(
                "Delete project `{project_label}`? Its 1 workspace + any running sessions will be killed."
            ),
            n => format!(
                "Delete project `{project_label}`? Its {n} workspaces + any running sessions will be killed."
            ),
        })
    }

    pub(crate) fn dispatch_action_unchecked(
        &mut self,
        action: &lazybox_tui_core::action::Action,
    ) -> Vec<IpcCommand> {
        use lazybox_tui_core::action::Action;
        let mut cmds = Vec::new();
        // Workspace-scoped actions need a target — grab the
        // sidebar's selection. Mismatch (no selection) silently
        // drops the action; the catalog's `availability` gates the
        // surface from offering it in that state.
        let session_key = self.sidebar.selected_workspace_key().cloned();
        // `session_id` is non-None when the cursor sits on a
        // session sub-row of a workspace; passing it makes the
        // daemon target that specific session instead of picking
        // / creating one. Matches the sidebar's existing spawn
        // handlers — without this, `a c` / `s` on a focused session
        // would silently spawn into the wrong session.
        let session_id = self.sidebar.selected_session_id();
        match action {
            Action::SpawnShell => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk,
                        session_id,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Shell,
                        cwd: None,
                        initial_prompt: None,
                        on_main: false,
                    });
                }
            }
            Action::SpawnAgent(agent_id) => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk,
                        session_id,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                        on_main: false,
                    });
                }
            }
            // Main-checkout variants (`b c` / `b s`, confirm-guarded):
            // same spawn, but `on_main` tells the daemon to land in the
            // repo's shared main checkout instead of an isolated
            // worktree. `session_id` is deliberately dropped — "on main"
            // always targets the shared checkout, not the selected
            // session sub-row.
            Action::SpawnAgentOnMain(agent_id) => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk,
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                        on_main: true,
                    });
                }
            }
            Action::SpawnShellOnMain => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk,
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Shell,
                        cwd: None,
                        initial_prompt: None,
                        on_main: true,
                    });
                }
            }
            Action::Work => {
                // Default work (`w w`) targets whatever agent is already running on
                // this workspace (so it injects into an existing Codex /
                // Cursor session instead of always spawning the default),
                // falling back to the default agent when none is running.
                // Several running conversations → ask which one (#418).
                // The scoped `w c` / `w x` chords (Action::WorkWith) force
                // a specific agent.
                self.dispatch_work(session_id, None, &mut cmds);
            }
            Action::WorkWith(agent_id) => {
                self.dispatch_work_with(agent_id, session_id, None, &mut cmds);
            }
            Action::WorkTier(alias) => {
                // Flat `w S`: work on the same contextual target agent as
                // `w w`, but launch it at the picked model tier. The
                // alias is resolved against the target agent's menu daemon-
                // side, so it degrades to the default model for an agent
                // that doesn't define the tier.
                self.dispatch_work(session_id, Some(alias.clone()), &mut cmds);
            }
            Action::SpawnTier(alias) => {
                // `a S`: spawn the default agent at the picked tier.
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        session_key: sk,
                        session_id,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(
                            self.sidebar.default_agent().to_string(),
                        ),
                        cwd: None,
                        initial_prompt: None,
                        on_main: false,
                        model_alias: Some(alias.clone()),
                        access: lazybox_ipc::AgentRunAccess::Default,
                    });
                }
            }
            Action::OpenEditor => {
                // `open_editor` is the orchestrator's existing
                // helper — it picks the right template (single
                // editor → launch directly; multiple → mount
                // picker; none → footer notice).
                self.open_editor();
            }
            Action::ViewDiff => {
                if let Some((workspace_key, target)) =
                    self.sidebar.selected_workspace().and_then(|workspace| {
                        let target = session_id
                            .or_else(|| workspace.default_session().map(|session| session.id))
                            .map(lazybox_ipc::WorkspaceDiffTarget::Session)
                            .or_else(|| {
                                workspace
                                    .linked_checkout
                                    .as_ref()
                                    .map(|_| lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout)
                            })?;
                        Some((workspace.key.clone(), target))
                    })
                {
                    self.pending_diff_session = Some((workspace_key.clone(), target.clone()));
                    self.flash_hint("reading worktree diff…");
                    cmds.push(IpcCommand::InspectWorkspaceDiff {
                        workspace_key,
                        target,
                    });
                } else {
                    self.flash_hint("this workspace has no worktree to review");
                }
            }
            Action::NewWorkspace => {
                let focused = self.sidebar.focused_project_key();
                // Explicit variant list (no `_` catch-all) so a new
                // Intent variant is a compile error here — this
                // consumer must decide what it means instead of
                // silently swallowing it.
                use crate::intent::Intent;
                match crate::intent::resolve_new_workspace(focused) {
                    Intent::MountNewWorkspaceInput { project_key } => {
                        self.mount_new_workspace_input(project_key);
                    }
                    Intent::Notice(msg) => {
                        self.flash_info(msg);
                    }
                    // The resolver only produces the two arms above;
                    // the rest are unreachable from this call.
                    Intent::NoOp
                    | Intent::SpawnAgent { .. }
                    | Intent::SpawnShell { .. }
                    | Intent::MountReply { .. }
                    | Intent::MountAdoptPicker { .. }
                    | Intent::OpenEditor
                    | Intent::MergePr { .. }
                    | Intent::UpdateBranch { .. }
                    | Intent::SetAutoMergeOnGreen { .. }
                    | Intent::SetTrackMain { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. }
                    | Intent::MarkActivitiesRead { .. }
                    | Intent::UpdateSelectedBranches { .. }
                    | Intent::CollapseIntoPr { .. }
                    | Intent::MountHandoffPicker { .. } => {}
                }
            }
            Action::RenameWorkspace => {
                // Rename targets the focused workspace's display label
                // (any workspace, even session-less). Section::Workspace,
                // so this fires from both Sidebar and Right focus.
                if let Some(ws) = self.sidebar.selected_workspace() {
                    let session_key: lazybox_core::SessionKey = (&ws.key).into();
                    self.mount_rename_workspace_input(session_key);
                }
            }
            Action::MoveToSpace => {
                // Assign the repo group at/above the cursor to a Space.
                // The group resolves from a header row too, so this is
                // reachable whether the cursor is on the header or a
                // workspace under it.
                if let Some(source) = self.sidebar.cursor_repo() {
                    self.mount_move_to_space_input(source);
                } else {
                    self.flash_info("select a repo group first".to_string());
                }
            }
            Action::NewProject => {
                self.mount_new_workspace_repo_picker();
            }
            Action::ImportCheckout => {
                self.start_scan_checkouts();
            }
            Action::AddScanRoot => {
                self.mount_add_scan_root_input();
            }
            Action::MarkAllRead => {
                let workspace = self.sidebar.selected_workspace().cloned();
                let selected = self.right.selected_activity_indices();
                let cursor = (self.focus == PaneFocus::Right)
                    .then(|| self.right.activity_cursor_target())
                    .flatten();
                let intent =
                    crate::intent::resolve_mark_read_targets(workspace.as_ref(), &selected, cursor);
                cmds.extend(self.execute_dispatch_intent(intent, workspace.as_ref()));
            }
            Action::Archive => {
                // Destructive — normally routed through
                // `dispatch_action_confirmed` with a mount-time
                // target; this selection-based arm is the fallback.
                //
                // Polymorphic by focused row: cursor on a workspace /
                // session row deletes that workspace; cursor on a
                // project header (RepoHeader) deletes the whole
                // project and cascades to its workspaces. The
                // availability gate (`availability` in the catalog)
                // already ensures one of the two has a target.
                if let Some(sk) = session_key {
                    self.optimistic_remove_workspace(&sk);
                    cmds.push(IpcCommand::Kill { session_key: sk });
                } else if let Some(project_key) = self.sidebar.focused_project_key() {
                    self.optimistic_remove_project(&project_key);
                    cmds.push(IpcCommand::DeleteProject { project_key });
                }
            }
            Action::AdoptSessions => {
                // Resolver decides "has sessions to adopt?": yes
                // → mount the target picker; no → footer notice.
                // Same shape as the inline handler had.
                let workspace = self.sidebar.selected_workspace().cloned();
                // Explicit variant list — a new Intent variant must be
                // triaged here at compile time, not silently dropped.
                use crate::intent::Intent;
                match crate::intent::resolve_adopt(workspace.as_ref()) {
                    Intent::MountAdoptPicker { source_key } => {
                        self.mount_adopt_picker(source_key);
                    }
                    Intent::Notice(msg) => {
                        self.flash_info(msg);
                    }
                    Intent::NoOp
                    | Intent::SpawnAgent { .. }
                    | Intent::SpawnShell { .. }
                    | Intent::MountReply { .. }
                    | Intent::MountNewWorkspaceInput { .. }
                    | Intent::OpenEditor
                    | Intent::MergePr { .. }
                    | Intent::UpdateBranch { .. }
                    | Intent::SetAutoMergeOnGreen { .. }
                    | Intent::SetTrackMain { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. }
                    | Intent::MarkActivitiesRead { .. }
                    | Intent::UpdateSelectedBranches { .. }
                    | Intent::CollapseIntoPr { .. }
                    | Intent::MountHandoffPicker { .. } => {}
                }
            }
            Action::CollapseIntoPr => {
                let issue_workspace = self.sidebar.selected_workspace().cloned();
                let workspaces = self.sidebar.workspaces_iter().collect::<Vec<_>>();
                let intent =
                    crate::intent::resolve_collapse_into_pr(issue_workspace.as_ref(), &workspaces);
                cmds.extend(self.execute_dispatch_intent(intent, issue_workspace.as_ref()));
            }
            Action::ToggleSnooze => {
                // When the workspace is already snoozed, `z` toggles
                // it off (no picker — that'd be friction). When NOT
                // snoozed, mount the duration picker so the user can
                // pick something meaningful instead of paying the
                // YAML default every time.
                let now = chrono::Utc::now();
                let Some(workspace) = self.sidebar.selected_workspace().cloned() else {
                    return cmds;
                };
                if workspace.is_snoozed(now) {
                    let session_key = lazybox_core::SessionKey::from(&workspace.key);
                    cmds.push(IpcCommand::Unsnooze { session_key });
                } else {
                    let session_key = lazybox_core::SessionKey::from(&workspace.key);
                    self.mount_snooze_picker(session_key);
                }
            }
            Action::MergePr => {
                // Destructive — normally routed through
                // `dispatch_action_confirmed` with a mount-time
                // target; this selection-based arm is the fallback.
                // Re-check the merge preconditions defensively, then
                // fire the IPC. (Catalog availability gates the surface
                // from offering the action when CI / review /
                // conflict state isn't ready, so this re-check
                // mostly catches the rare race where state
                // changed while the modal was open.)
                let workspace = self.sidebar.selected_workspace().cloned();
                let intent = crate::intent::resolve_merge(workspace.as_ref());
                cmds.extend(self.execute_dispatch_intent(intent, workspace.as_ref()));
            }
            Action::UpdateBranch => {
                // Non-destructive (Guard::None), so it fires straight
                // through here. Re-resolve against the live selection —
                // the catalog availability gate already keeps the action
                // off non-behind PRs, so this mostly names the target.
                let workspace = self.sidebar.selected_workspace().cloned();
                let intent = crate::intent::resolve_update_branch(workspace.as_ref());
                cmds.extend(self.execute_dispatch_intent(intent, workspace.as_ref()));
            }
            Action::UpdateBranchSelected => {
                let keys = self.sidebar.selected_broadcast_keys();
                let selected = keys
                    .iter()
                    .filter_map(|key| self.sidebar.workspace_by_key(key))
                    .collect::<Vec<_>>();
                let intent = crate::intent::resolve_update_branch_selected(keys.len(), &selected);
                cmds.extend(self.execute_dispatch_intent(intent, None));
            }
            Action::ToggleAutoMerge => {
                let workspace = self.sidebar.selected_workspace().cloned();
                // Resolve the configured allowlist so arming a
                // non-opted-in third-party PR is refused with a reason
                // rather than lighting a pill that never fires (#845).
                let policy = lazybox_config::Config::load()
                    .map(|c| c.merge_on_green.to_policy())
                    .unwrap_or_default();
                // Explicit variant list — a new Intent variant must be
                // triaged here at compile time, not silently dropped.
                use crate::intent::Intent;
                match crate::intent::resolve_toggle_auto_merge(workspace.as_ref(), &policy) {
                    Intent::SetAutoMergeOnGreen {
                        workspace_key,
                        enabled,
                    } => {
                        let name = workspace
                            .as_ref()
                            .map(|w| crate::util::notice_slug(&w.name).into_owned())
                            .unwrap_or_default();
                        if enabled {
                            self.flash_info(format!("auto-merge on green: armed for {name}"));
                        } else {
                            self.flash_info(format!("auto-merge on green: off for {name}"));
                        }
                        cmds.push(IpcCommand::SetAutoMergeOnGreen {
                            session_key: lazybox_core::SessionKey::from(&workspace_key),
                            enabled,
                        });
                    }
                    Intent::Notice(msg) => self.flash_info(msg),
                    Intent::NoOp
                    | Intent::SpawnAgent { .. }
                    | Intent::SpawnShell { .. }
                    | Intent::MountReply { .. }
                    | Intent::MountNewWorkspaceInput { .. }
                    | Intent::MountAdoptPicker { .. }
                    | Intent::OpenEditor
                    | Intent::MergePr { .. }
                    | Intent::UpdateBranch { .. }
                    | Intent::SetTrackMain { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. }
                    | Intent::MarkActivitiesRead { .. }
                    | Intent::UpdateSelectedBranches { .. }
                    | Intent::CollapseIntoPr { .. }
                    | Intent::MountHandoffPicker { .. } => {}
                }
            }
            Action::ToggleAutoFix => {
                let Some(workspace) = self.sidebar.selected_workspace().cloned() else {
                    return cmds;
                };
                if workspace.pr.is_none() {
                    return cmds;
                }
                let arm = if workspace.policies.any_auto_fix_armed() {
                    lazybox_core::PolicyArm::Disarm
                } else {
                    lazybox_core::PolicyArm::Arm
                };
                let name = crate::util::notice_slug(&workspace.name);
                match arm {
                    lazybox_core::PolicyArm::Arm if self.auto_fix_enabled => self.flash_info(
                        format!("auto-fix: armed for {name} (CI failures + conflicts)"),
                    ),
                    lazybox_core::PolicyArm::Arm => self
                        .flash_info(format!("auto-fix: armed for {name}, but disabled globally")),
                    lazybox_core::PolicyArm::Disarm => {
                        self.flash_info(format!("auto-fix: off for {name}"))
                    }
                    lazybox_core::PolicyArm::Default => unreachable!(),
                }
                let session_key = lazybox_core::SessionKey::from(&workspace.key);
                cmds.push(IpcCommand::SetAutoFixPolicies {
                    session_key,
                    ci: arm,
                    conflict: arm,
                });
            }
            Action::ToggleTrackMain => {
                let workspace = self.sidebar.selected_workspace().cloned();
                // Explicit variant list (no `_`): a new Intent variant is
                // triaged here at compile time, not silently dropped.
                use crate::intent::Intent;
                match crate::intent::resolve_toggle_track_main(workspace.as_ref()) {
                    Intent::SetTrackMain {
                        workspace_key,
                        enabled,
                    } => {
                        let name = workspace
                            .as_ref()
                            .map(|w| crate::util::notice_slug(&w.name).into_owned())
                            .unwrap_or_default();
                        if enabled {
                            self.flash_info(format!("track main: on for {name}"));
                        } else {
                            self.flash_info(format!("track main: off for {name}"));
                        }
                        cmds.push(IpcCommand::SetTrackMain {
                            session_key: lazybox_core::SessionKey::from(&workspace_key),
                            enabled,
                        });
                    }
                    Intent::Notice(msg) => self.flash_info(msg),
                    Intent::NoOp
                    | Intent::SpawnAgent { .. }
                    | Intent::SpawnShell { .. }
                    | Intent::MountReply { .. }
                    | Intent::MountNewWorkspaceInput { .. }
                    | Intent::MountAdoptPicker { .. }
                    | Intent::OpenEditor
                    | Intent::MergePr { .. }
                    | Intent::UpdateBranch { .. }
                    | Intent::SetAutoMergeOnGreen { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. }
                    | Intent::MarkActivitiesRead { .. }
                    | Intent::UpdateSelectedBranches { .. }
                    | Intent::CollapseIntoPr { .. }
                    | Intent::MountHandoffPicker { .. } => {}
                }
            }
            Action::ManagePolicies => {
                // Unified automation-policies menu (issue #363). Surfaces
                // for any workspace carrying a PR or a GitHub issue; the
                // menu itself marks which policies apply to PRs vs issues.
                if let Some(ws) = self.sidebar.selected_workspace()
                    && (ws.pr.is_some() || !ws.gh_issues.is_empty())
                {
                    let ws_key = ws.key.clone();
                    self.mount_policy_picker(ws_key);
                }
            }
            Action::Refresh => {
                cmds.push(IpcCommand::Refresh);
                // Pre-arm the bg_poll indicator so the user gets
                // feedback on the keystroke — same as the
                // `Shift+R` handler did inline before.
                self.status
                    .note_poll_progress("github", "manual refresh requested");
                self.flash_hint("refreshing…");
                // Arm a one-shot ack so the next PollCompleted /
                // ProviderError surfaces a clear "✓ sync ok" or
                // "✗ sync failed" footer notice — silent
                // spinner-clears were being read as "did anything
                // happen?"
                self.pending_refresh_ack = true;
            }
            Action::ForceRedraw => {
                self.force_full_redraw();
            }
            Action::OpenHelp => {
                self.mount_help_ask();
            }
            Action::OpenTour => {
                self.mount_tour();
            }
            Action::OpenSyncStatus => {
                self.mount_sync_status();
            }
            Action::OpenMessages => {
                self.mount_messages();
            }
            Action::OpenErrorInbox => {
                self.mount_error_inbox();
            }
            Action::OpenSettings => {
                self.open_settings();
            }
            Action::OpenThemePicker => {
                self.mount_theme_picker();
            }
            Action::OpenSnippets => {
                self.mount_snippet_browser();
            }
            Action::JumpToWorkspace => {
                self.mount_jump_picker();
            }
            Action::JumpToAsking => {
                if self.sidebar.focus_next_asking_workspace() {
                    self.set_focus(PaneFocus::Sidebar);
                    self.redraw = true;
                }
            }
            Action::JumpToFailingCi => {
                if self.sidebar.focus_next_failing_ci_workspace() {
                    self.set_focus(PaneFocus::Sidebar);
                    self.redraw = true;
                } else {
                    self.flash_hint("no failing PRs");
                }
            }
            Action::JumpToLimited => {
                if self.sidebar.focus_next_limit_reached_workspace() {
                    self.set_focus(PaneFocus::Sidebar);
                    self.redraw = true;
                } else {
                    self.flash_hint("no rate-limited agents");
                }
            }
            Action::ResumeRateLimited => {
                cmds.extend(self.resume_rate_limited_agents());
            }
            Action::ToggleActivityPane => {
                if let Some(ws_key) = self.sidebar.selected_workspace().map(|w| w.key.clone()) {
                    self.activity_pane.cycle(
                        ws_key,
                        self.right.has_visible_content(),
                        self.ui_defaults.activity_pane_default,
                    );
                    // Don't strand focus on a pane we just collapsed.
                    self.enforce_pane_focus();
                    self.redraw = true;
                }
            }
            Action::ToggleFocusMode => {
                self.toggle_focus_mode();
            }
            Action::StartAgent => {
                // Global "just start working" entry point. Unlike `n`
                // (which needs the sidebar cursor on a project), this
                // works from any pane: mount a project picker, then
                // the name input, then create+spawn. With a single
                // project the picker is skipped and we jump straight
                // to naming.
                self.start_agent_flow();
            }
            Action::Reply => {
                // Reply targets the focused workspace. Resolver
                // returns `Intent::MountReply` when a workspace is
                // selected; we mount the textarea modal. Fires from
                // both Sidebar and Right (catalog Section::Workspace
                // covers both focuses).
                let intent = crate::intent::resolve_reply(self.sidebar.selected_workspace());
                if let crate::intent::Intent::MountReply { workspace_key } = intent {
                    let session_key: lazybox_core::SessionKey = (&workspace_key).into();
                    self.mount_reply(session_key);
                }
            }
            Action::EditNotes => {
                // Notes attach to the focused workspace (any workspace,
                // even a session-less one). Section::Workspace, so this
                // fires from both Sidebar and Right focus.
                if let Some(ws) = self.sidebar.selected_workspace() {
                    let session_key: lazybox_core::SessionKey = (&ws.key).into();
                    self.mount_notes(session_key);
                }
            }
            Action::RequestReviewers => {
                if let Some(ws) = self.sidebar.selected_workspace()
                    && ws.pr.is_some()
                {
                    let ws_key = ws.key.clone();
                    self.mount_request_reviewers(ws_key);
                }
            }
            Action::AddAssignees => {
                if let Some(ws) = self.sidebar.selected_workspace() {
                    // Assignment requires a GraphQL Assignable id —
                    // PR or gh issue with a node_id. Empty pre-PR
                    // workspaces don't qualify.
                    let has_target = ws.pr.as_ref().map(|p| p.node_id.is_some()).unwrap_or(false)
                        || ws
                            .gh_issues
                            .first()
                            .map(|i| i.node_id.is_some())
                            .unwrap_or(false);
                    if has_target {
                        let ws_key = ws.key.clone();
                        self.mount_add_assignees(ws_key);
                    }
                }
            }
            Action::ManageLabels => {
                // Labels require a `Labelable` node id — same as
                // assignees. Pre-PR scratch workspaces don't qualify.
                if let Some(ws) = self.sidebar.selected_workspace() {
                    let has_target = ws.pr.as_ref().map(|p| p.node_id.is_some()).unwrap_or(false)
                        || ws
                            .gh_issues
                            .first()
                            .map(|i| i.node_id.is_some())
                            .unwrap_or(false);
                    if !has_target {
                        self.flash_info("no PR / issue to label");
                        return cmds;
                    }
                    let ws_key = ws.key.clone();
                    // Two-step: ask the daemon for the repo's label
                    // set, then mount the picker when
                    // `IpcEvent::RepoLabels` arrives. Stash the
                    // workspace key so the event handler knows
                    // whether the response is still relevant.
                    self.awaiting_repo_labels = Some(ws_key.clone());
                    cmds.push(IpcCommand::FetchRepoLabels {
                        workspace_key: ws_key,
                    });
                    self.flash_hint("loading repo labels…");
                }
            }
            Action::OpenInBrowser => {
                // Read the primary task's URL and hand it to the
                // platform launcher. Surfaces a footer notice on
                // success / failure so the user knows whether the
                // browser actually came up — silent spawn failures
                // (no xdg-open on a headless box, etc.) would be
                // confusing otherwise.

                let Some(ws) = self.sidebar.selected_workspace() else {
                    return cmds;
                };
                let Some(url) = ws.primary_task().map(|t| t.url.clone()) else {
                    self.flash_info("no task URL on this workspace");
                    return cmds;
                };
                let browser = self.ui_defaults.browser.clone();
                match lazybox_tui_core::editors::open_url(&url, browser.as_deref()) {
                    Ok(()) => {
                        // open_url is fire-and-forget (the launcher is
                        // spawned, not waited on) — phrase as in-progress.
                        tracing::info!(%url, "opening workspace URL in browser");
                        self.flash_info(format!("opening {url}…"));
                    }
                    Err(e) => {
                        tracing::warn!(%url, "open_url failed: {e}");
                        self.flash(
                            format!("open failed: {e}"),
                            crate::realm::components::footer::NoticeSeverity::Retryable,
                        );
                    }
                }
            }
            Action::SyncWorkspace => {
                // Targeted re-poll of just this workspace's PR / issue —
                // cheaper than the global refresh when you're waiting on
                // one PR's CI. The daemon deep-fetches the entity and
                // upserts it, so the row's state + read markers update
                // without a full sweep.
                let Some(ws) = self.sidebar.selected_workspace() else {
                    return cmds;
                };
                if ws.pr.is_none() && ws.gh_issues.is_empty() {
                    self.flash_info("nothing to sync on this workspace");
                    return cmds;
                }
                let workspace_key = ws.key.clone();
                cmds.push(IpcCommand::SyncWorkspace { workspace_key });
                self.flash_hint("syncing…");
            }
            Action::CyclePane => {
                // The keyboard path normally consumes the chord in
                // `handle_pane_key`'s guard arm (which owns the
                // live-terminal gating); this arm serves the other
                // surfaces (context menu, tests) with the same effect.
                self.cycle_pane_focus();
            }
            Action::FocusPaneRight => {
                // Sidebar `Right`/`l`: step focus into the pane on the
                // right. Skip the activity pane when it's hidden and go
                // straight to the terminal, mirroring the Tab-cycle skip
                // in `cycle_pane_focus`.
                self.set_focus(if self.activity_pane_visible() {
                    PaneFocus::Right
                } else {
                    PaneFocus::Terminals
                });
                self.redraw = true;
            }
            Action::FocusPaneLeft => {
                // Activity-pane `Left`/`h`: collapse an expanded row
                // first — that's the pane's own meaning — and only when
                // there's nothing left to collapse step focus back to
                // the sidebar, so arrow navigation is reversible without
                // clobbering the collapse gesture.
                if !self.right.collapse_focused_row() {
                    self.set_focus(PaneFocus::Sidebar);
                }
                self.redraw = true;
            }
            Action::ToggleMouseCapture => {
                self.toggle_mouse_capture();
            }
            Action::ActivityTop => {
                // `g` under Right focus: jump the activity cursor to
                // the first row. Catalog-dispatched so the vim
                // go-to-top reflex can never arm the Workspace `g *`
                // github leader from the activity pane (where a
                // reflexive `g g` used to silently toggle auto-merge).
                self.right.activity_cursor_top();
                self.redraw = true;
            }
            Action::ActivityBottom => {
                self.right.activity_cursor_bottom();
                self.redraw = true;
            }
            Action::OpenFilterMenu => {
                self.mount_filter_menu();
            }
            Action::CycleSort => {
                self.sidebar.cycle_sort();
            }
            Action::CycleMailbox => {
                self.sidebar.cycle_mailbox();
            }
            Action::OpenSearch => {
                self.sidebar.open_search();
            }
            Action::OpenGlobalSearch => {
                self.sidebar.open_global_search();
            }
            Action::ToggleRepoGroup => {
                // `Space` folds whichever tier the cursor rests on: a
                // Space header collapses the whole Space (#860), any
                // other row collapses its repo group.
                if self.sidebar.cursor_on_space_header() {
                    self.sidebar.toggle_space_at_cursor();
                } else {
                    self.sidebar.toggle_repo_at_cursor();
                }
            }
            Action::ToggleRepoPin => {
                if let Some((repo, pinned)) = self.sidebar.toggle_pin_at_cursor() {
                    let verb = if pinned { "pinned" } else { "unpinned" };
                    self.flash_info(format!("{verb} {repo}"));
                    self.redraw = true;
                }
            }
            Action::ToggleFocusWorkspace => {
                if let Some((label, focused)) = self.sidebar.toggle_focus_at_cursor() {
                    let verb = if focused { "focused" } else { "unfocused" };
                    self.flash_info(format!("{verb} {label}"));
                    self.redraw = true;
                }
            }
            Action::SelectWorkspace => {
                // Toggle the cursor row's broadcast mark. The notice
                // names the running count + the broadcast key so the
                // marks visibly lead somewhere.
                if let Some(now_selected) = self.sidebar.toggle_broadcast_select() {
                    let n = self.sidebar.broadcast_selected_count();
                    let keys = lazybox_tui_core::action::ActionDef::for_kind(
                        lazybox_tui_core::action::ActionKind::BroadcastToSelected,
                    )
                    .effective_keys_display(&self.action_key_overrides);
                    let verb = if now_selected {
                        "selected"
                    } else {
                        "deselected"
                    };
                    if n == 0 {
                        self.flash_info(format!("{verb} — selection empty"));
                    } else {
                        self.flash_info(format!("{verb} — {n} marked · {keys} to broadcast"));
                    }
                    self.redraw = true;
                }
            }
            Action::BroadcastToSelected => {
                self.mount_broadcast_picker();
            }
            Action::SendToSession => {
                let workspace = self.sidebar.selected_workspace().cloned();
                let captured = workspace.as_ref().and_then(|workspace| {
                    let source_key = lazybox_core::SessionKey::from(&workspace.key);
                    self.terminals
                        .agent_terminal_for(&source_key)
                        .map(|terminal_id| {
                            self.terminals.visible_text(terminal_id).unwrap_or_default()
                        })
                });
                let intent = crate::intent::resolve_send_to_session(workspace.as_ref(), captured);
                cmds.extend(self.execute_dispatch_intent(intent, workspace.as_ref()));
            }
            Action::ConvertSession => {
                if self.conversion.is_some() {
                    self.flash_info("a session conversion is already in progress");
                    return cmds;
                }
                let Some(source_key) = self.sidebar.selected_workspace_key().cloned() else {
                    return cmds;
                };
                let focused_source = self.terminals.focused_terminal_id().filter(|terminal_id| {
                    self.terminals.terminal_is_agent(*terminal_id)
                        && self.terminals.session_key_for(*terminal_id) == Some(&source_key)
                });
                let Some(source_terminal) =
                    focused_source.or_else(|| self.terminals.agent_terminal_for(&source_key))
                else {
                    self.flash_info("no agent session here to convert");
                    return cmds;
                };
                let Some(agent) = self
                    .terminals
                    .terminal_agent_id(source_terminal)
                    .map(str::to_string)
                else {
                    self.flash_info("no agent session here to convert");
                    return cmds;
                };
                if !matches!(agent.as_str(), "claude" | "codex") {
                    self.flash_info(format!(
                        "{agent} does not support structured session handoffs"
                    ));
                    return cmds;
                }
                if self.terminals.terminal_is_on_main(source_terminal) {
                    self.flash_info("session conversion currently requires an isolated worktree");
                    return cmds;
                }
                let source_name = self
                    .sidebar
                    .workspace_by_key(&source_key)
                    .map(|workspace| workspace.name.clone())
                    .unwrap_or_else(|| source_key.to_string());
                self.mount_conversion_role_picker(super::ConversionDraft {
                    source_terminal,
                    source: source_key,
                    source_name,
                    agent,
                });
            }
            // Actions not yet handled here stay in the existing
            // handlers. As we migrate, the per-key match arms in
            // `handle_pane_key` and the pane wrappers get deleted
            // and the case lands here.
            other => {
                tracing::debug!(
                    "dispatch_action: {other:?} not yet migrated; falling back to legacy handler",
                );
            }
        }
        cmds
    }

    /// Resolve and fire `w w` (or a `w S` tier chord) on the selected
    /// workspace (issue #418). Shared by `Work` and `WorkTier` so both
    /// pick the same agent before layering a tier on top:
    ///
    /// - one live conversation → work targets its exact terminal;
    /// - several live conversations → mount the chooser instead of
    ///   silently guessing, including when several use the same agent;
    /// - none → fresh spawn of the configured default.
    fn dispatch_work(
        &mut self,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        use crate::components::sidebar::WorkTarget;
        let default_agent = self.sidebar.default_agent().to_string();
        let target = match self.sidebar.selected_workspace_key().cloned() {
            Some(sk) => self.sidebar.work_target(&sk, &default_agent),
            None => WorkTarget::Spawn(default_agent),
        };
        self.dispatch_work_target(target, session_id, model_alias, cmds);
    }

    /// Resolve a scoped `w <agent>` without letting another running
    /// agent kind participate in the choice.
    fn dispatch_work_with(
        &mut self,
        agent_id: &str,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        use crate::components::sidebar::WorkTarget;
        let target = match self.sidebar.selected_workspace_key().cloned() {
            Some(sk) => self.sidebar.work_target_for_agent(&sk, agent_id),
            None => WorkTarget::Spawn(agent_id.to_string()),
        };
        self.dispatch_work_target(target, session_id, model_alias, cmds);
    }

    fn dispatch_work_target(
        &mut self,
        target: crate::components::sidebar::WorkTarget,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        use crate::components::sidebar::WorkTarget;
        match target {
            WorkTarget::Spawn(agent_id) => {
                self.push_work_command(&agent_id, None, session_id, model_alias, cmds);
            }
            WorkTarget::Running(target) => {
                self.push_work_command(
                    &target.agent_id,
                    Some(target.terminal_id),
                    session_id,
                    model_alias,
                    cmds,
                );
            }
            WorkTarget::Choose(targets) => {
                self.mount_work_agent_picker(targets, session_id, model_alias);
            }
        }
    }

    /// Resolve a "work on this" command for `agent_id` and queue it.
    /// Shared by `w w` ([`lazybox_tui_core::action::Action::Work`]), the scoped `w c` / `w x`
    /// chords ([`lazybox_tui_core::action::Action::WorkWith`]), and the which-agent picker's pick
    /// (`choice_picked_inner`, issue #418): all build the same
    /// contextual prompt via [`crate::intent::resolve_work`] and differ
    /// only in how the target conversation is chosen. An exact running
    /// terminal receives an inject; otherwise the command remains a
    /// spawn.
    ///
    /// The activity selection lives in the right pane, but `w` must honor
    /// it from any focus — reading it here is sound because `set_workspace`
    /// clears the selection whenever the workspace key changes, so the
    /// right pane's indices always belong to the selected workspace.
    pub(super) fn push_work_command(
        &mut self,
        agent_id: &str,
        terminal_id: Option<lazybox_ipc::TerminalId>,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        let workspace = self.sidebar.selected_workspace();
        let selected = self.right.selected_activity_indices();
        let intent =
            crate::intent::resolve_work(workspace, &selected, agent_id, self.sidebar.conventions());
        match intent {
            crate::intent::Intent::SpawnAgent {
                workspace_key,
                agent_id,
                prompt,
            } => {
                // Pin the follow target to the workspace `w` fired on so the
                // spawned agent's terminal is what focus lands on, even if a
                // slow first-time worktree provision lets the cursor wander
                // before the `TerminalSpawned` arrives (consumed in the
                // spawn-event handler).
                self.spawn_follow_to = Some((&workspace_key).into());
                let spawn = IpcCommand::Spawn {
                    session_key: (&workspace_key).into(),
                    session_id,
                    client_request_id: None,
                    kind: lazybox_ipc::TerminalKind::Agent(agent_id),
                    cwd: None,
                    initial_prompt: prompt,
                    on_main: false,
                    model_alias,
                    access: lazybox_ipc::AgentRunAccess::Default,
                };
                let command = match terminal_id {
                    Some(terminal_id) => self.rewrite_spawn_to_terminal(spawn, terminal_id),
                    None => spawn,
                };
                cmds.push(command);
                self.right.clear_activity_selection();
            }
            // A merged/closed workspace steers to cleanup: surface the
            // "press x x to archive" notice rather than provisioning a
            // worktree for finished work (issue #557).
            crate::intent::Intent::Notice(msg) => self.flash_hint(msg),
            _ => {
                // Nothing actionable (no workspace selected). Surface that
                // instead of silently swallowing the keystroke.
                self.flash_hint("nothing to work on here");
            }
        }
    }

    /// Enter or leave focus mode (issue #156). Entering requires a
    /// live terminal to maximize — focus mode over an empty stack
    /// would show a blank screen — so we flash a hint and stay put
    /// when there's nothing to focus. Entering pins focus to the
    /// terminal; leaving keeps it there so the user lands back in the
    /// three-pane view still driving the same agent.
    pub(super) fn toggle_focus_mode(&mut self) {
        if self.focus_mode {
            self.focus_mode = false;
            self.redraw = true;
            return;
        }
        if self.terminals.is_empty() {
            self.flash_hint("no agent terminal to focus");
            return;
        }
        self.focus_mode = true;
        self.set_focus(PaneFocus::Terminals);
        self.redraw = true;
    }

    /// Switch the displayed terminal to the Nth agent workspace,
    /// counting in sidebar (top-down) order — the deterministic
    /// `]]<digit>` jump that replaced the old `F3` cycle. `n` is
    /// 1-based, matching the number badge on the sidebar row and the
    /// roster in the `]]` leader popup. Keeps focus on the terminal so
    /// it works seamlessly inside focus mode; flashes when there's no
    /// agent at that slot. Mirrors the `!` / `Shift-F` jumps but lands
    /// the cursor on a specific agent rather than asking / failing-CI.
    pub(super) fn jump_to_agent_workspace(&mut self, n: usize) {
        if self.sidebar.focus_nth_agent_workspace(n) {
            self.set_focus(PaneFocus::Terminals);
            self.sync_panes();
            self.redraw = true;
        } else {
            self.flash_hint(format!("no agent #{n}"));
        }
    }

    /// Leave the terminal pane back to the sidebar. Exits focus mode
    /// if it was on — the sidebar is hidden there, so returning to it
    /// must restore the normal three-pane layout.
    pub(super) fn leave_terminal_to_sidebar(&mut self) {
        self.focus_mode = false;
        self.set_focus(PaneFocus::Sidebar);
    }
}
