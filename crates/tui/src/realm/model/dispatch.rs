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
/// number. Feeds the pending merge / close footer notices. (The
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
        // `pending_action_confirm` and fires on `Msg::Confirmed(true)`.
        // This is the *only* path through `dispatch_action` for
        // destructive variants — there's no way to fire one
        // without the user confirming.
        if ActionDef::for_action(action).is_destructive() {
            // Resolve the concrete target NOW, while the selection
            // is the row the user acted on. The confirm fires
            // against this stash — see `pending_action_confirm`.
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
                    Action::Archive => vec![IpcCommand::Kill {
                        session_key: session_key.clone(),
                    }],
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
                        // Re-resolve against the stashed workspace so a
                        // state change while the modal was up can't
                        // snooze the wrong row.
                        if let crate::intent::Intent::Snooze {
                            session_key,
                            duration,
                        } = crate::intent::resolve_long_snooze(
                            workspace.as_ref(),
                            self.ui_defaults.long_snooze,
                        ) {
                            let until = chrono::Utc::now()
                                + chrono::Duration::from_std(duration)
                                    .unwrap_or_else(|_| chrono::Duration::days(365));
                            vec![IpcCommand::Snooze { session_key, until }]
                        } else {
                            Vec::new()
                        }
                    }
                    Action::MergePr => {
                        // Re-check merge preconditions against the
                        // STASHED workspace — state may have moved
                        // (new failing CI, fresh conflict) while the
                        // modal was up.
                        if let crate::intent::Intent::MergePr { workspace_key } =
                            crate::intent::resolve_merge(workspace.as_ref())
                        {
                            // Pending feedback: the merge result arrives
                            // seconds later via PrMerged / an error event,
                            // and a silent footer in between reads as a
                            // dropped confirm.
                            self.flash_info(format!(
                                "merging PR{}…",
                                task_number_suffix(
                                    workspace
                                        .as_ref()
                                        .and_then(|w| w.pr.as_ref())
                                        .map(|t| t.id.key.as_str())
                                        .unwrap_or("")
                                )
                            ));
                            vec![IpcCommand::MergePr { workspace_key }]
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
                        session_key: session_key.clone(),
                        session_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                        on_main: true,
                    }],
                    Action::SpawnShellOnMain => vec![IpcCommand::Spawn {
                        model_alias: None,
                        session_key: session_key.clone(),
                        session_id: None,
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
                    Action::Archive => vec![IpcCommand::DeleteProject {
                        project_key: project_key.clone(),
                    }],
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
                        session_key: sk,
                        session_id,
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
                        session_key: sk,
                        session_id,
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
                        session_key: sk,
                        session_id: None,
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
                        session_key: sk,
                        session_id: None,
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
                // The scoped `w c` / `w x` chords (Action::WorkWith) force
                // a specific agent.
                let target_agent = self.work_target_agent();
                self.push_work_spawn(&target_agent, session_id, None, &mut cmds);
            }
            Action::WorkWith(agent_id) => {
                // Scoped `w <agent>`: same contextual prompt as `w w`,
                // but forced to the chosen agent. `rewrite_spawn_to_inject`
                // injects when that agent is already running, else spawns.
                self.push_work_spawn(agent_id, session_id, None, &mut cmds);
            }
            Action::WorkTier(alias) => {
                // Flat `w S`: work on the same contextual target agent as
                // `w w`, but launch it at the picked model tier. The
                // alias is resolved against the target agent's menu daemon-
                // side, so it degrades to the default model for an agent
                // that doesn't define the tier.
                let target_agent = self.work_target_agent();
                self.push_work_spawn(&target_agent, session_id, Some(alias.clone()), &mut cmds);
            }
            Action::SpawnTier(alias) => {
                // `a S`: spawn the default agent at the picked tier.
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        session_key: sk,
                        session_id,
                        kind: lazybox_ipc::TerminalKind::Agent(
                            self.sidebar.default_agent().to_string(),
                        ),
                        cwd: None,
                        initial_prompt: None,
                        on_main: false,
                        model_alias: Some(alias.clone()),
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
                    | Intent::SetAutoMergeOnGreen { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. } => {}
                }
            }
            Action::NewProject => {
                self.mount_new_workspace_repo_picker();
            }
            Action::MarkAllRead => {
                // Context-sensitive: when the user has activities
                // multi-selected in the right pane, `m` marks only
                // THOSE rows. With no selection but the activity
                // cursor on a row (right pane focused), `m` marks
                // just that row — the explicit counterpart to the
                // auto-mark-on-hover timer. Otherwise it falls back
                // to the bulk "mark all of this workspace"
                // behaviour. Same key, smarter semantics based on
                // where the user is acting.
                let Some(sk) = session_key else {
                    return cmds;
                };
                let selected = self.right.selected_activity_indices();
                if selected.is_empty() {
                    if self.focus != PaneFocus::Right || !self.right.mark_cursor_row_read(&mut cmds)
                    {
                        cmds.push(IpcCommand::MarkRead { session_key: sk });
                    }
                } else {
                    let n = selected.len();
                    for index in selected {
                        cmds.push(IpcCommand::MarkActivityRead {
                            session_key: sk.clone(),
                            index,
                        });
                    }
                    self.flash_info(format!(
                        "marked {n} selected activit{} read",
                        if n == 1 { "y" } else { "ies" }
                    ));
                }
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
                    cmds.push(IpcCommand::Kill { session_key: sk });
                } else if let Some(project_key) = self.sidebar.focused_project_key() {
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
                    | Intent::SetAutoMergeOnGreen { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. } => {}
                }
            }
            Action::CollapseIntoPr => {
                // Catalog availability gates on "issue workspace exists"
                // — the cross-workspace lookup ("does any PR close
                // this?") happens here so the catalog stays
                // single-workspace. When no claiming PR is known
                // locally, surface a footer notice instead of firing
                // a no-op IPC the daemon would just log + drop.
                let Some(issue_ws) = self.sidebar.selected_workspace().cloned() else {
                    return cmds;
                };
                let Some(primary) = issue_ws.primary_task() else {
                    return cmds;
                };
                let claiming_pr = self
                    .sidebar
                    .workspaces_iter()
                    .find(|w| {
                        w.pr.as_ref()
                            .is_some_and(|pr| pr.closes_issues.contains(&primary.id))
                    })
                    .map(|w| lazybox_core::SessionKey::from(&w.key));
                match claiming_pr {
                    Some(_pr_key) => {
                        cmds.push(IpcCommand::CollapseIntoPr {
                            issue_workspace_key: lazybox_core::SessionKey::from(&issue_ws.key),
                        });
                        self.flash_info("joining into PR…");
                    }
                    None => {
                        self.flash_info("no PR closes this issue (or it isn't synced yet)");
                    }
                }
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
                if let crate::intent::Intent::MergePr { workspace_key } =
                    crate::intent::resolve_merge(workspace.as_ref())
                {
                    // Same pending feedback as the confirmed path — the
                    // result lands seconds later.
                    self.flash_info(format!(
                        "merging PR{}…",
                        task_number_suffix(
                            workspace
                                .as_ref()
                                .and_then(|w| w.pr.as_ref())
                                .map(|t| t.id.key.as_str())
                                .unwrap_or("")
                        )
                    ));
                    cmds.push(IpcCommand::MergePr { workspace_key });
                }
            }
            Action::ToggleAutoMerge => {
                let workspace = self.sidebar.selected_workspace().cloned();
                // Explicit variant list — a new Intent variant must be
                // triaged here at compile time, not silently dropped.
                use crate::intent::Intent;
                match crate::intent::resolve_toggle_auto_merge(workspace.as_ref()) {
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
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. } => {}
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
                    self.focus = PaneFocus::Sidebar;
                    self.set_focus_attr();
                    self.redraw = true;
                }
            }
            Action::JumpToFailingCi => {
                if self.sidebar.focus_next_failing_ci_workspace() {
                    self.focus = PaneFocus::Sidebar;
                    self.set_focus_attr();
                    self.redraw = true;
                } else {
                    self.flash_hint("no failing PRs");
                }
            }
            Action::ToggleActivityPane => {
                // Flip the Activity pane's visibility for the focused
                // workspace and remember the choice (so navigating away
                // and back keeps it). The recorded value is the desired
                // visibility — the negation of what's currently shown.
                if let Some(ws_key) = self.sidebar.selected_workspace().map(|w| w.key.clone()) {
                    let now_visible = self.activity_pane_visible();
                    self.activity_pane_overrides.insert(ws_key, !now_visible);
                    // Don't strand focus on a pane we just hid.
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
                    self.pending_labels_request = Some(ws_key.clone());
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
            Action::CyclePane => {
                // The keyboard path normally consumes the chord in
                // `handle_pane_key`'s guard arm (which owns the
                // live-terminal gating); this arm serves the other
                // surfaces (context menu, tests) with the same effect.
                self.cycle_pane_focus();
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
            Action::CycleRoleFilter => {
                self.sidebar.cycle_role_filter();
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
            Action::ToggleRepoGroup => {
                self.sidebar.toggle_repo_at_cursor();
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

    /// Resolve a "work on this" spawn for `agent_id` and queue it.
    /// Shared by `w w` ([`Action::Work`]) and the scoped `w c` / `w x`
    /// chords ([`Action::WorkWith`]): both build the same contextual
    /// prompt via [`crate::intent::resolve_work`] and differ only in how
    /// the target agent is chosen. The queued `Spawn` carries the prompt
    /// so `rewrite_spawn_to_inject` can fold it into an existing terminal.
    ///
    /// The activity selection lives in the right pane, but `w` must honor
    /// it from any focus — reading it here is sound because `set_workspace`
    /// clears the selection whenever the workspace key changes, so the
    /// right pane's indices always belong to the selected workspace.
    /// The agent `w w` (or a `w S` tier chord) targets on the
    /// selected workspace: whatever agent is already running there, else
    /// the configured default. Shared by `Work` and `WorkTier` so both
    /// pick the same agent before layering a tier on top.
    fn work_target_agent(&self) -> String {
        let default_agent = self.sidebar.default_agent().to_string();
        match self.sidebar.selected_workspace_key().cloned() {
            Some(sk) => self.sidebar.work_target_agent(&sk, &default_agent),
            None => default_agent,
        }
    }

    fn push_work_spawn(
        &mut self,
        agent_id: &str,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        let workspace = self.sidebar.selected_workspace();
        let selected = self.right.selected_activity_indices();
        let intent = crate::intent::resolve_work(workspace, &selected, agent_id);
        if let crate::intent::Intent::SpawnAgent {
            workspace_key,
            agent_id,
            prompt,
        } = intent
        {
            // Pin the follow target to the workspace `w` fired on so the
            // spawned agent's terminal is what focus lands on, even if a
            // slow first-time worktree provision lets the cursor wander
            // before the `TerminalSpawned` arrives (consumed in the
            // spawn-event handler).
            self.spawn_follow_to = Some((&workspace_key).into());
            cmds.push(IpcCommand::Spawn {
                session_key: (&workspace_key).into(),
                session_id,
                kind: lazybox_ipc::TerminalKind::Agent(agent_id),
                cwd: None,
                initial_prompt: prompt,
                on_main: false,
                model_alias,
            });
            self.right.clear_activity_selection();
        } else {
            // Nothing actionable to spawn (no PR, issue, or selected
            // comments). Surface that instead of silently swallowing
            // the keystroke.
            self.flash_hint("nothing to work on here");
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
        self.focus = PaneFocus::Terminals;
        self.terminal_user_typed_since_focus = false;
        self.set_focus_attr();
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
            self.focus = PaneFocus::Terminals;
            self.terminal_user_typed_since_focus = false;
            self.set_focus_attr();
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
        self.focus = PaneFocus::Sidebar;
        self.set_focus_attr();
    }
}
