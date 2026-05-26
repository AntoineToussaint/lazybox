//! Modal-submission handlers — the orchestrator's `handle_*`
//! contract surface that runs when a mounted modal completes:
//! textarea submit (reply), input submit (new workspace / project),
//! choice picks (reviewers / assignees / context menu / adopt
//! target), modal dismissal (Esc), and confirm prompts.
//!
//! Each handler reads the stashed state set by the matching
//! `mount_*` (in modals.rs) and returns the IPC commands the
//! orchestrator should ship to the daemon. Returning `Vec<IpcCommand>`
//! rather than calling `send_cmd` inline keeps these unit-testable
//! — see model/tests.rs for the effect-contract suite.
//!
//! The setup-wizard step machinery (`handle_runner_step`,
//! `mount_setup_modal`, `unmount_setup_modal`) co-locates here
//! since it's the same modal-state-mutation shape.

use super::{Id, Model, Msg};
use crate::realm::UserEvent;
use pilot_ipc::Command as IpcCommand;
use tuirealm::terminal::TerminalAdapter;

impl<T: TerminalAdapter> Model<T> {
    /// Reply textarea submit. Build a `PostReply` for the
    /// workspace that mounted the textarea. Empty bodies dismiss
    /// without posting; the footer "submitted — fetching" notice +
    /// an immediate `Refresh` keep the user from waiting on the
    /// 60s poll loop.
    ///
    /// **Effects**: returns IPC commands as a `Vec` (not sent
    /// inline) so unit tests can drive this handler with fixture
    /// state and assert on the returned commands without a real
    /// IPC client. Notice + modal-stack stay as direct mutations
    /// (tests inspect `Model` state after the call).
    pub fn handle_textarea_submitted(&mut self, body: String) -> Vec<IpcCommand> {
        self.pop_modal();
        let mut cmds = Vec::new();
        let target = self.pending_reply.take();
        if let Some(session_key) = target
            && !body.trim().is_empty()
        {
            cmds.push(IpcCommand::PostReply { session_key, body });
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.status.notice = Some(Notice::new(
                "Reply submitted — fetching…",
                NoticeSeverity::Info,
            ));
            cmds.push(IpcCommand::Refresh);
        }
        cmds
    }

    /// Input modal submit (single-line text). Dispatch by which
    /// Input modal is currently on top. Handles `NewWorkspace`
    /// (→ `CreateWorkspace`), `RequestReviewers`, `AddAssignees`.
    ///
    /// Reviewer / assignee inputs accept comma- or whitespace-
    /// separated logins. The `@` prefix is optional and stripped.
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    pub fn handle_input_submitted(&mut self, text: String) -> Vec<IpcCommand> {
        let top = self.modal_stack.last().cloned();
        self.pop_modal();
        let mut cmds = Vec::new();
        match top {
            Some(Id::NewWorkspace) => {
                let name = text.trim().to_string();
                let project_key = self.pending_new_workspace_project.take();
                match (name.is_empty(), project_key) {
                    (false, Some(project_key)) => {
                        tracing::info!(
                            workspace_name = %name,
                            project_key = %project_key,
                            "creating new pre-PR workspace under project",
                        );
                        cmds.push(IpcCommand::CreateWorkspace { name, project_key });
                    }
                    (false, None) => {
                        tracing::warn!(
                            workspace_name = %name,
                            "new-workspace submit without a stashed project_key — dropped",
                        );
                    }
                    _ => {}
                }
            }
            Some(Id::NewProject) => {
                let name = text.trim().to_string();
                if !name.is_empty() {
                    tracing::info!(project_name = %name, "creating new local project");
                    cmds.push(IpcCommand::CreateProject { name });
                }
            }
            // RequestReviewers / AddAssignees used to go through an
            // Input modal but were migrated to a `Choice::multi`
            // picker — see `mount_request_reviewers` /
            // `handle_choice_picked`. The corresponding Input arms
            // were removed; an Input modal under those Ids never
            // mounts anymore, so a stray submit would just fall
            // through to the default arm below.
            _ => {
                // Unknown input source — silently drop. The pop
                // above already cleared the modal.
            }
        }
        cmds
    }

    /// Route a Choice modal pick to the right handler. Five
    /// distinct flows share the same `Msg::ChoicePicked` envelope
    /// (Adopt target, Editor picker, Settings palette, runner-
    /// driven flows, plain pop-on-pick) — this fn fans out by
    /// inspecting the top modal id + setup state.
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    /// The Editor / Settings / runner branches may still emit
    /// commands internally via helper methods (`launch_editor`,
    /// `dispatch_settings_action`, `handle_runner_step`); only
    /// the directly-visible IPC commands land in the Vec.
    pub fn handle_choice_picked(&mut self, picks: Vec<usize>) -> Vec<IpcCommand> {
        let mut cmds = Vec::new();
        // Sidebar right-click context menu. Pick → dispatch the
        // same IpcCommand the matching keyboard shortcut would.
        // Empty pick (Esc) clears the stash silently.
        if matches!(self.modal_stack.last(), Some(Id::SidebarContext)) {
            use pilot_tui_core::action::Action;
            let stash = self.pending_sidebar_context.take();
            self.pop_modal();
            if let (Some((session_key, actions)), Some(&idx)) = (stash.as_ref(), picks.first())
                && let Some(action) = actions.get(idx)
            {
                let workspace_key = pilot_core::WorkspaceKey::new(session_key.as_str().to_string());
                match action {
                    Action::SpawnAgent(agent_id) => {
                        cmds.push(IpcCommand::Spawn {
                            session_key: session_key.clone(),
                            session_id: None,
                            kind: pilot_ipc::TerminalKind::Agent(agent_id.clone()),
                            cwd: None,
                            initial_prompt: None,
                        });
                    }
                    Action::SpawnShell => {
                        cmds.push(IpcCommand::Spawn {
                            session_key: session_key.clone(),
                            session_id: None,
                            kind: pilot_ipc::TerminalKind::Shell,
                            cwd: None,
                            initial_prompt: None,
                        });
                    }
                    Action::OpenEditor => {
                        // Same path as the `e` keyboard shortcut.
                        // Selection already moved on to this row
                        // via the right-click hit-test; `open_editor`
                        // operates on whatever's selected.
                        self.open_editor();
                    }
                    Action::MarkAllRead => {
                        cmds.push(IpcCommand::MarkRead {
                            session_key: session_key.clone(),
                        });
                    }
                    Action::MergePr => {
                        cmds.push(IpcCommand::MergePr { workspace_key });
                    }
                    Action::Archive => {
                        cmds.push(IpcCommand::Kill {
                            session_key: session_key.clone(),
                        });
                    }
                    // The menu only offers the six variants above
                    // (see `mount_sidebar_context_menu`'s candidate
                    // list). Anything else is a bug — fail loud so
                    // it surfaces in tests rather than silently
                    // doing nothing.
                    other => tracing::warn!("sidebar context menu: unhandled action {other:?}",),
                }
                self.redraw = true;
            }
            return cmds;
        }
        // Adopt picker (Id::AdoptTarget) — pick → send the
        // `Command::AdoptSessions` mapping source→target. Empty
        // pick (Esc → no Msg, but cover the defensive case) drops
        // the stash without firing.
        if matches!(self.modal_stack.last(), Some(Id::AdoptTarget)) {
            let target = picks
                .first()
                .and_then(|i| self.adopt_choices.get(*i).cloned());
            self.adopt_choices.clear();
            self.pop_modal();
            let source = self.pending_adopt_source.take();
            if let (Some(source_key), Some(target_key)) = (source, target) {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                cmds.push(IpcCommand::AdoptSessions {
                    source_workspace_key: source_key.clone(),
                    target_workspace_key: target_key.clone(),
                });
                self.status.notice = Some(Notice::new(
                    format!("adopted sessions: {source_key} → {target_key}"),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
            }
            return cmds;
        }
        // Reviewer picker (Id::RequestReviewers) — picks index
        // into `review_choices`. Empty pick drops the slot.
        if matches!(self.modal_stack.last(), Some(Id::RequestReviewers)) {
            let logins: Vec<String> = picks
                .iter()
                .filter_map(|i| self.review_choices.get(*i).cloned())
                .collect();
            self.review_choices.clear();
            self.pop_modal();
            let workspace_key = self.pending_review_request.take();
            if let (Some(workspace_key), false) = (workspace_key, logins.is_empty()) {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let count = logins.len();
                cmds.push(IpcCommand::RequestReviewers {
                    workspace_key,
                    logins,
                });
                self.status.notice = Some(Notice::new(
                    format!("requested {count} reviewer(s)"),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
            }
            return cmds;
        }
        // Assignees picker (Id::AddAssignees) — picker is pre-
        // checked with existing assignees, so submitting the current
        // selection is the *full desired set*. Fire SetAssignees;
        // the daemon diffs against the persisted task and runs
        // add + remove mutations as needed. Empty pick is meaningful
        // here ("clear all assignees") — don't drop it.
        if matches!(self.modal_stack.last(), Some(Id::AddAssignees)) {
            let logins: Vec<String> = picks
                .iter()
                .filter_map(|i| self.assignees_choices.get(*i).cloned())
                .collect();
            self.assignees_choices.clear();
            self.pop_modal();
            if let Some(workspace_key) = self.pending_assignees_request.take() {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let count = logins.len();
                let msg = if count == 0 {
                    "cleared assignees".to_string()
                } else {
                    format!("set assignees ({count})")
                };
                cmds.push(IpcCommand::SetAssignees {
                    workspace_key,
                    logins,
                });
                self.status.notice = Some(Notice::new(msg, NoticeSeverity::Info));
                self.redraw = true;
            }
            return cmds;
        }
        // Editor picker (Id::Editor) — pick → launch (or defer
        // behind a session-spawn when the workspace has no
        // worktree yet).
        if matches!(self.modal_stack.last(), Some(Id::Editor)) {
            let editor = picks
                .first()
                .and_then(|i| self.setup.editor_choices.get(*i).cloned());
            self.setup.editor_choices.clear();
            self.pop_modal();
            let Some(editor) = editor else { return cmds };
            if let Some(workspace_key) = self.setup.pending_editor_workspace.take() {
                self.setup.pending_editor_launch = Some((workspace_key.clone(), editor.clone()));
                cmds.push(IpcCommand::Spawn {
                    session_key: workspace_key.clone(),
                    session_id: None,
                    kind: pilot_ipc::TerminalKind::Shell,
                    cwd: None,
                    initial_prompt: None,
                });
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                self.status.notice = Some(Notice::new(
                    format!(
                        "Provisioning worktree for {workspace_key} — opening in {} when ready…",
                        editor.display
                    ),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
                return cmds;
            }
            // Worktree already on disk — launch directly.
            let worktree = self
                .sidebar
                .selected_workspace()
                .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()));
            if let Some(worktree) = worktree {
                self.launch_editor(&editor, &worktree);
            }
            return cmds;
        }
        // Settings palette is a non-runner Choice modal — if the
        // user just picked an action, route into a partial wizard
        // flow before falling through.
        if !self.setup.settings_actions.is_empty()
            && matches!(self.modal_stack.last(), Some(Id::Setup))
            && self.setup.runner.is_none()
        {
            let action = picks
                .first()
                .and_then(|i| self.setup.settings_actions.get(*i).cloned());
            self.setup.settings_actions.clear();
            self.pop_modal();
            if let Some(action) = action {
                self.dispatch_settings_action(action);
            }
            return cmds;
        }
        if let Some(mut runner) = self.setup.runner.take() {
            let step = runner.step_choice_picked(picks);
            self.handle_runner_step(runner, step);
        } else {
            self.pop_modal();
        }
        cmds
    }

    /// `Esc` / mount-stack pop. Setup wizard takes priority; the
    /// non-runner case routes by which prompt was on top so the
    /// daemon learns the "no" decision (merge stalls would otherwise
    /// re-prompt on the next poll).
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    /// Note: the setup-runner branch may still send commands
    /// internally via `handle_runner_step`; tests that drive the
    /// wizard path need to mock at a different layer.
    pub fn handle_modal_dismissed(&mut self) -> Vec<IpcCommand> {
        if let Some(mut runner) = self.setup.runner.take() {
            let step = runner.step_dismissed();
            self.handle_runner_step(runner, step);
            return Vec::new();
        }
        // Dispatch by which modal was on top BEFORE the pop so we
        // route the "no" decision correctly.
        let top = self.modal_stack.last().cloned();
        self.pop_modal();
        let mut cmds = Vec::new();
        match top {
            Some(Id::RemoveOutOfScope) => {
                self.active_removal_prompt = None;
            }
            Some(Id::MergeConfirm) => {
                // Esc on the merge modal = "no, keep them
                // separate." Tell the daemon to drop the stall so
                // future polls don't re-prompt.
                if let Some((issue_key, pr_key)) = self.active_merge_prompt.take() {
                    cmds.push(IpcCommand::ConfirmMerge {
                        issue_workspace_key: issue_key,
                        pr_workspace_key: pr_key,
                        accept: false,
                    });
                }
            }
            Some(Id::ActionConfirm) => {
                // Esc = cancel destructive action; drop the
                // queued Action without firing.
                self.pending_action_confirm = None;
            }
            Some(Id::RequestReviewers) => {
                // Esc cancels; drop the stashed workspace key +
                // candidate list so a later mount on a *different*
                // workspace doesn't pick up the wrong target.
                self.pending_review_request = None;
                self.review_choices.clear();
            }
            Some(Id::AddAssignees) => {
                self.pending_assignees_request = None;
                self.assignees_choices.clear();
            }
            _ => {}
        }
        // Always try to surface a queued prompt after a modal
        // dismisses — not just when the dismissed modal itself was
        // a prompt. Otherwise a user who has Help / Settings open
        // when the daemon emits a prompt would have it stuck in
        // the queue.
        self.maybe_mount_next_removal_prompt();
        self.maybe_mount_next_merge_prompt();
        cmds
    }

    /// `y` / `n` answer on a ConfirmModal. Routes by which modal
    /// id was on top; each branch maps `yes` to a side-effect
    /// (kill workspace, post merge-confirm to daemon, etc.).
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    pub fn handle_confirmed(&mut self, yes: bool) -> Vec<IpcCommand> {
        let top = self.modal_stack.last().cloned();
        self.pop_modal();
        let mut cmds = Vec::new();
        match top {
            Some(Id::RemoveOutOfScope) => {
                let target = self.active_removal_prompt.take();
                if yes && let Some(workspace_key) = target {
                    // Kill terminals + delete workspace.
                    let session_key: pilot_core::SessionKey = (&workspace_key).into();
                    cmds.push(IpcCommand::Kill { session_key });
                }
            }
            Some(Id::MergeConfirm) => {
                if let Some((issue_key, pr_key)) = self.active_merge_prompt.take() {
                    cmds.push(IpcCommand::ConfirmMerge {
                        issue_workspace_key: issue_key,
                        pr_workspace_key: pr_key,
                        accept: yes,
                    });
                }
            }
            Some(Id::ActionConfirm) => {
                // Unified destructive-action confirm. Yes →
                // dispatch the queued action via the unchecked
                // path (the gate already fired). No / Esc → drop
                // the stash silently.
                let pending = self.pending_action_confirm.take();
                if yes && let Some(action) = pending {
                    cmds.extend(self.dispatch_action_unchecked(&action));
                    self.redraw = true;
                }
            }
            Some(Id::CleanWorktreesConfirm) => {
                if yes {
                    use crate::realm::components::footer::{Notice, NoticeSeverity};
                    cmds.push(IpcCommand::CleanWorktrees);
                    // The work happens asynchronously on the daemon
                    // (filesystem walk + git worktree remove per
                    // session) — surface a placeholder notice so the
                    // user knows the click registered. The final
                    // count comes back via
                    // `Event::CleanWorktreesCompleted`.
                    self.status.notice =
                        Some(Notice::new("cleaning worktrees…", NoticeSeverity::Info));
                    self.redraw = true;
                }
            }
            _ => {}
        }
        self.maybe_mount_next_removal_prompt();
        self.maybe_mount_next_merge_prompt();
        cmds
    }

    /// Apply a [`crate::setup_flow::RunnerStep`] returned by the
    /// runner — mount the next modal, fire the on-complete hook, or
    /// drop the wizard. The `runner` argument lets us conditionally
    /// hold on to the runner across step transitions: `Next` puts it
    /// back; `Finish` / `Cancel` drop it.
    pub(super) fn handle_runner_step(
        &mut self,
        runner: crate::setup_flow::SetupRunner,
        step: crate::setup_flow::RunnerStep,
    ) {
        use crate::setup_flow::RunnerStep;
        match step {
            RunnerStep::Next(component) => {
                self.setup.runner = Some(runner);
                self.mount_setup_modal(component);
            }
            RunnerStep::Finish(outcome) => {
                let sources: Vec<String> = outcome.enabled_providers.iter().cloned().collect();
                // Cache the new persisted state so subsequent partial
                // flows (Settings → Add a repo) see the latest scopes.
                self.setup.persisted = Some(crate::setup_flow::outcome_to_persisted(&outcome));
                // Push the new repo subscriptions into the sidebar so
                // the user sees a header for the freshly-added repo
                // even before polling finds workspaces under it.
                self.refresh_subscribed_projects();
                if let Some(hook) = self.setup.on_complete.as_ref() {
                    hook(outcome);
                }
                self.unmount_setup_modal();
                self.send_cmd(IpcCommand::Subscribe);
                // Kick off an immediate poll so a freshly added repo
                // surfaces its open PRs/issues within seconds instead
                // of waiting for the long-lived 60s loop tick.
                self.send_cmd(IpcCommand::Refresh);
                self.set_focus_attr();
                if !sources.is_empty() {
                    self.show_polling(sources);
                }
            }
            RunnerStep::Cancel => {
                self.unmount_setup_modal();
                self.send_cmd(IpcCommand::Subscribe);
                self.set_focus_attr();
            }
            RunnerStep::Stay => {
                self.setup.runner = Some(runner);
            }
        }
    }

    /// Unmount whatever's at `Id::Setup` (or `Id::Splash` if the
    /// wizard hasn't moved off splash yet) and mount `component`
    /// there. The setup id is reused — only one wizard step is ever
    /// live at a time.
    pub(super) fn mount_setup_modal(
        &mut self,
        component: Box<dyn tuirealm::component::AppComponent<Msg, UserEvent>>,
    ) {
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        // Unmount whatever's on top.
        if let Some(top) = self.modal_stack.last().cloned() {
            let _ = self.app.umount(&top);
            self.modal_stack.pop();
        }
        let _ = self.app.mount(
            Id::Setup,
            component,
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Setup);
        let _ = self.app.active(&Id::Setup);
        self.redraw = true;
    }

    /// Drop whatever setup-related modal is on top of the stack.
    /// Called on Finish / Cancel.
    pub(super) fn unmount_setup_modal(&mut self) {
        if let Some(top) = self.modal_stack.last().cloned() {
            let _ = self.app.umount(&top);
            self.modal_stack.pop();
        }
        if let Some(top) = self.modal_stack.last() {
            let _ = self.app.active(top);
        }
        self.redraw = true;
    }
}
