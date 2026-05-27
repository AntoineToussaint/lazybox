//! Catalog-action dispatch: `dispatch_action` (destructive-gate
//! wrapper) and `dispatch_action_unchecked` (the actual fan-out).
//! Every catalog `Action` whose effect fits a single IpcCommand or
//! modal mount lands here. The keyboard, right-click menu, palette,
//! and future remap UI all funnel through `dispatch_action` so
//! behavior stays consistent across surfaces.

use super::{Model, PaneFocus};
use pilot_ipc::Command as IpcCommand;
use tuirealm::terminal::TerminalAdapter;

impl<T: TerminalAdapter> Model<T> {
    /// Single fan-out from a catalog `Action` to its effect (IPC
    /// command, modal mount, focus shift, …). Surfaces (keyboard,
    /// right-click menu, future remap UI) all call this so behavior
    /// stays consistent across them.
    ///
    /// **Returns** the IPC commands the action produces, if any.
    /// UI-only effects (modal mounts, focus moves) happen via
    /// `&mut self` and aren't reflected in the return.
    pub fn dispatch_action(&mut self, action: &pilot_tui_core::action::Action) -> Vec<IpcCommand> {
        use pilot_tui_core::action::ActionDef;
        // Destructive gate, type-system enforced via the catalog.
        // Every destructive action is routed through the unified
        // Confirm modal first; the pending action lives in
        // `pending_action_confirm` and fires on `Msg::Confirmed(true)`.
        // This is the *only* path through `dispatch_action` for
        // destructive variants — there's no way to fire one
        // without the user confirming.
        if ActionDef::for_action(action).is_destructive() {
            // Project-header focused Archive deletes the whole
            // project (cascading to its workspaces) — the default
            // confirm prompt assumes a workspace target, which would
            // be a confusing lie. Compute a tailored prompt for that
            // case and let the rest fall through to the static
            // catalog prompt.
            let custom_prompt = self.action_confirm_override(action);
            self.mount_action_confirm(action.clone(), custom_prompt);
            return Vec::new();
        }
        self.dispatch_action_unchecked(action)
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
    fn action_confirm_override(
        &self,
        action: &pilot_tui_core::action::Action,
    ) -> Option<String> {
        use pilot_tui_core::action::Action;
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
        action: &pilot_tui_core::action::Action,
    ) -> Vec<IpcCommand> {
        use pilot_tui_core::action::Action;
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
        // handlers — without this, `c` / `s` on a focused session
        // would silently spawn into the wrong session.
        let session_id = self.sidebar.selected_session_id();
        match action {
            Action::SpawnShell => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        session_key: sk,
                        session_id,
                        kind: pilot_ipc::TerminalKind::Shell,
                        cwd: None,
                        initial_prompt: None,
                    });
                }
            }
            Action::SpawnAgent(agent_id) => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        session_key: sk,
                        session_id,
                        kind: pilot_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                    });
                }
            }
            Action::Work => {
                // Polymorphic spawn driven by `classify_work`:
                // PR-with-failing-CI gets "fix CI", issue gets
                // "implement issue", PR with open review threads
                // gets "address review", … Resolver returns
                // SpawnAgent with the right prompt, the dispatcher
                // just translates to IpcCommand.
                let default_agent = self.sidebar.default_agent().to_string();
                let workspace = self.sidebar.selected_workspace().cloned();
                let intent = crate::intent::resolve_work(workspace.as_ref(), &[], &default_agent);
                if let crate::intent::Intent::SpawnAgent {
                    workspace_key,
                    agent_id,
                    prompt,
                } = intent
                {
                    cmds.push(IpcCommand::Spawn {
                        session_key: (&workspace_key).into(),
                        session_id,
                        kind: pilot_ipc::TerminalKind::Agent(agent_id),
                        cwd: None,
                        initial_prompt: prompt,
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
                match crate::intent::resolve_new_workspace(focused) {
                    crate::intent::Intent::MountNewWorkspaceInput { project_key } => {
                        self.mount_new_workspace_input(project_key);
                    }
                    crate::intent::Intent::Notice(msg) => {
                        use crate::realm::components::footer::{Notice, NoticeSeverity};
                        self.status.notice = Some(Notice::new(msg, NoticeSeverity::Info));
                        self.redraw = true;
                    }
                    _ => {}
                }
            }
            Action::NewProject => {
                self.mount_new_project_input();
            }
            Action::MarkAllRead => {
                // Context-sensitive: when the user has activities
                // multi-selected in the right pane, `m` marks only
                // THOSE rows. When no selection, falls back to the
                // bulk "mark all of this workspace" behaviour. Same
                // key, smarter semantics based on what's selected.
                let Some(sk) = session_key else {
                    return cmds;
                };
                let selected = self.right.selected_activity_indices();
                if selected.is_empty() {
                    cmds.push(IpcCommand::MarkRead { session_key: sk });
                } else {
                    use crate::realm::components::footer::{Notice, NoticeSeverity};
                    let n = selected.len();
                    for index in selected {
                        cmds.push(IpcCommand::MarkActivityRead {
                            session_key: sk.clone(),
                            index,
                        });
                    }
                    self.status.notice = Some(Notice::new(
                        format!(
                            "marked {n} selected activit{} read",
                            if n == 1 { "y" } else { "ies" }
                        ),
                        NoticeSeverity::Info,
                    ));
                    self.redraw = true;
                }
            }
            Action::Archive => {
                // Destructive — only reachable from
                // `dispatch_action_unchecked` after the user said
                // Yes on the unified ActionConfirm modal.
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
                match crate::intent::resolve_adopt(workspace.as_ref()) {
                    crate::intent::Intent::MountAdoptPicker { source_key } => {
                        self.mount_adopt_picker(source_key);
                    }
                    crate::intent::Intent::Notice(msg) => {
                        use crate::realm::components::footer::{Notice, NoticeSeverity};
                        self.status.notice = Some(Notice::new(msg, NoticeSeverity::Info));
                        self.redraw = true;
                    }
                    _ => {}
                }
            }
            Action::CollapseIntoPr => {
                // Catalog availability gates on "issue workspace exists"
                // — the cross-workspace lookup ("does any PR close
                // this?") happens here so the catalog stays
                // single-workspace. When no claiming PR is known
                // locally, surface a footer notice instead of firing
                // a no-op IPC the daemon would just log + drop.
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let Some(issue_ws) = self.sidebar.selected_workspace().cloned() else {
                    return cmds;
                };
                let Some(primary) = issue_ws.primary_task() else {
                    return cmds;
                };
                let claiming_pr = self
                    .sidebar
                    .workspaces_iter()
                    .find(|w| w.pr.as_ref().is_some_and(|pr| pr.closes_issues.contains(&primary.id)))
                    .map(|w| pilot_core::SessionKey::from(&w.key));
                match claiming_pr {
                    Some(_pr_key) => {
                        cmds.push(IpcCommand::CollapseIntoPr {
                            issue_workspace_key: pilot_core::SessionKey::from(&issue_ws.key),
                        });
                        self.status.notice = Some(Notice::new(
                            "joining into PR…",
                            NoticeSeverity::Info,
                        ));
                        self.redraw = true;
                    }
                    None => {
                        self.status.notice = Some(Notice::new(
                            "no PR closes this issue (or it isn't synced yet)",
                            NoticeSeverity::Info,
                        ));
                        self.redraw = true;
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
                    let session_key = pilot_core::SessionKey::from(&workspace.key);
                    cmds.push(IpcCommand::Unsnooze { session_key });
                } else {
                    let session_key = pilot_core::SessionKey::from(&workspace.key);
                    self.mount_snooze_picker(session_key);
                }
            }
            Action::MergePr => {
                // Destructive — only reachable from
                // `dispatch_action_unchecked` after the user said
                // Yes on the unified ActionConfirm. Re-check the
                // merge preconditions defensively, then fire the
                // IPC. (Catalog availability gates the surface
                // from offering the action when CI / review /
                // conflict state isn't ready, so this re-check
                // mostly catches the rare race where state
                // changed while the modal was open.)
                let workspace = self.sidebar.selected_workspace().cloned();
                if let crate::intent::Intent::MergePr { workspace_key } =
                    crate::intent::resolve_merge(workspace.as_ref())
                {
                    cmds.push(IpcCommand::MergePr { workspace_key });
                }
            }
            Action::Refresh => {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                cmds.push(IpcCommand::Refresh);
                // Pre-arm the bg_poll indicator so the user gets
                // feedback on the keystroke — same as the
                // `Shift+R` handler did inline before.
                self.status
                    .note_poll_progress("github", "manual refresh requested");
                self.status.notice =
                    Some(Notice::new("refreshing…".to_string(), NoticeSeverity::Hint));
                // Arm a one-shot ack so the next PollCompleted /
                // ProviderError surfaces a clear "✓ sync ok" or
                // "✗ sync failed" footer notice — silent
                // spinner-clears were being read as "did anything
                // happen?"
                self.pending_refresh_ack = true;
                self.redraw = true;
            }
            Action::OpenHelp => {
                self.mount_help();
            }
            Action::OpenSettings => {
                self.open_settings();
            }
            Action::JumpToAsking => {
                if self.sidebar.focus_next_asking_workspace() {
                    self.focus = PaneFocus::Sidebar;
                    self.set_focus_attr();
                    self.redraw = true;
                }
            }
            Action::Reply => {
                // Reply targets the focused workspace. Resolver
                // returns `Intent::MountReply` when a workspace is
                // selected; we mount the textarea modal. Fires from
                // both Sidebar and Right (catalog Section::Workspace
                // covers both focuses).
                let intent = crate::intent::resolve_reply(self.sidebar.selected_workspace());
                if let crate::intent::Intent::MountReply { workspace_key } = intent {
                    let session_key: pilot_core::SessionKey = (&workspace_key).into();
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
            Action::OpenInBrowser => {
                // Read the primary task's URL and hand it to the
                // platform launcher. Surfaces a footer notice on
                // success / failure so the user knows whether the
                // browser actually came up — silent spawn failures
                // (no xdg-open on a headless box, etc.) would be
                // confusing otherwise.
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let Some(ws) = self.sidebar.selected_workspace() else {
                    return cmds;
                };
                let Some(url) = ws.primary_task().map(|t| t.url.clone()) else {
                    self.status.notice = Some(Notice::new(
                        "no task URL on this workspace",
                        NoticeSeverity::Info,
                    ));
                    self.redraw = true;
                    return cmds;
                };
                match pilot_tui_core::editors::open_url(&url) {
                    Ok(()) => {
                        tracing::info!(%url, "opened workspace URL in browser");
                        self.status.notice =
                            Some(Notice::new(format!("opened {url}"), NoticeSeverity::Info));
                    }
                    Err(e) => {
                        tracing::warn!(%url, "open_url failed: {e}");
                        self.status.notice = Some(Notice::new(
                            format!("open failed: {e}"),
                            NoticeSeverity::Retryable,
                        ));
                    }
                }
                self.redraw = true;
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
}
