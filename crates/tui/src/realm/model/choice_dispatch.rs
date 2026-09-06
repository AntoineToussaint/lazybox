//! Choice-modal adapter: collect renderer state, resolve in tui-core, and
//! apply the resulting effects.

use super::{ChoicePayload, Id, ModalFlow, Model};
use lazybox_ipc::Command as IpcCommand;
use tuirealm::terminal::TerminalAdapter;

impl<T: TerminalAdapter> Model<T> {
    pub(super) fn choice_picked_inner(
        &mut self,
        picks: Vec<ChoicePayload>,
        submit: bool,
    ) -> Vec<IpcCommand> {
        use lazybox_tui_core::choice::{PickOutcome, resolve_pick};

        let Some(top) = self.modal_stack.last().cloned() else {
            return Vec::new();
        };
        // Sandbox onboarding drives its own draft state machine rather than
        // the tui-core PickFlow catalog (#1112).
        if top == Id::SandboxProviderPick {
            self.sandbox_provider_picked(&picks);
            return Vec::new();
        }
        let outcome = resolve_pick(&picks, self.pick_flow(&top, submit));
        if let PickOutcome::Runner(indices) = outcome {
            if let Some(mut runner) = self.setup.runner.take() {
                let step = runner.step_choice_picked(indices);
                self.handle_runner_step(runner, step);
            }
            return Vec::new();
        }

        self.consume_pick_state(&top, &outcome);
        self.pop_modal();
        self.apply_pick_outcome(outcome)
    }

    /// Provider row picked in sandbox onboarding: record it on the draft
    /// and mount the provider's first question. A missing draft or empty
    /// pick (Esc) just cancels — `mount_sandbox_stage` re-stashes the flow
    /// only when it actually advances.
    fn sandbox_provider_picked(&mut self, picks: &[ChoicePayload]) {
        let Some(ModalFlow::SandboxOnboarding { mut draft }) = self.modal_flow.take() else {
            self.pop_modal();
            return;
        };
        self.pop_modal();
        let Some(ChoicePayload::Text(provider)) = picks.first() else {
            return;
        };
        draft.set_provider(provider.clone());
        self.mount_sandbox_stage(draft);
    }

    fn pick_flow(&self, top: &Id, submit: bool) -> lazybox_tui_core::choice::PickFlow {
        use lazybox_tui_core::choice::{PickFlow, SnippetPick, WorkPickTarget, WorkPickerState};

        let snippets = || {
            self.snippets
                .all()
                .map(|(key, snippet)| SnippetPick {
                    key: key.to_string(),
                    category: snippet.category.clone(),
                    body: snippet.dispatch_body(),
                })
                .collect()
        };
        match top {
            Id::BroadcastSnippet => PickFlow::BroadcastSnippet {
                active: matches!(self.modal_flow, Some(ModalFlow::Broadcast { .. })),
                snippets: snippets(),
            },
            Id::SnippetPicker => PickFlow::Snippet {
                terminal_id: self.picker_target_terminal(),
                snippets: snippets(),
                submit,
            },
            Id::SkillPicker => PickFlow::Skill {
                terminal_id: self.picker_target_terminal(),
            },
            Id::PromptHistoryPicker => {
                let terminal_id = match &self.modal_flow {
                    Some(ModalFlow::PromptHistory { terminal }) => Some(*terminal),
                    _ => None,
                };
                PickFlow::PromptHistory {
                    terminal_id,
                    terminal_is_agent: terminal_id
                        .is_some_and(|terminal| self.terminals.terminal_is_agent(terminal)),
                }
            }
            Id::JumpPicker => PickFlow::Jump,
            Id::UrlPicker => PickFlow::Url,
            Id::ThemePicker => PickFlow::Theme,
            Id::DefaultAgentPicker => PickFlow::DefaultAgent,
            Id::DefaultModelPicker => PickFlow::DefaultModel {
                agent_id: self.default_model_agent.clone(),
            },
            Id::SidebarContext => {
                let (session_key, actions) = match &self.modal_flow {
                    Some(ModalFlow::SidebarContext {
                        session_key,
                        actions,
                    }) => (Some(session_key.clone()), actions.clone()),
                    _ => (None, Vec::new()),
                };
                PickFlow::SidebarContext {
                    session_key,
                    actions,
                }
            }
            Id::AdoptTarget => PickFlow::Adopt {
                source: match &self.modal_flow {
                    Some(ModalFlow::AdoptSource { source }) => Some(source.clone()),
                    _ => None,
                },
            },
            Id::HandoffTarget => PickFlow::HandoffTarget {
                active: matches!(self.modal_flow, Some(ModalFlow::Handoff { .. })),
            },
            Id::ConvertSessionRole => PickFlow::ConvertSession {
                active: matches!(self.modal_flow, Some(ModalFlow::ConvertSession { .. })),
            },
            Id::StartAgentProject => PickFlow::StartAgentProject,
            Id::StartSheet => PickFlow::StartSheet,
            Id::NewWorkspaceRepo => PickFlow::NewWorkspaceRepo,
            Id::HopperProject => match &self.modal_flow {
                Some(ModalFlow::HopperProject { workspace, action }) => PickFlow::HopperProject {
                    workspace: workspace.clone(),
                    action: action.clone(),
                },
                _ => PickFlow::Plain,
            },
            Id::LinearTeamRepo => match &self.modal_flow {
                Some(ModalFlow::LinearTeamRepo { team }) => {
                    PickFlow::LinearTeamRepo { team: team.clone() }
                }
                _ => PickFlow::Plain,
            },
            Id::JiraProjectRepo => match &self.modal_flow {
                Some(ModalFlow::JiraProjectRepo { project }) => PickFlow::JiraProjectRepo {
                    project: project.clone(),
                },
                _ => PickFlow::Plain,
            },
            Id::MoveToSpacePicker => match &self.modal_flow {
                Some(ModalFlow::MoveToSpacePick { source, entries }) => PickFlow::MoveToSpace {
                    source: Some(source.clone()),
                    entries: entries.clone(),
                },
                _ => PickFlow::Plain,
            },
            Id::HeaderContext => match &self.modal_flow {
                Some(ModalFlow::HeaderContext { actions }) => PickFlow::HeaderContext {
                    actions: actions.clone(),
                },
                _ => PickFlow::Plain,
            },
            Id::RequestReviewers => PickFlow::Reviewers {
                workspace_key: match &self.modal_flow {
                    Some(ModalFlow::ReviewRequest { workspace }) => Some(workspace.clone()),
                    _ => None,
                },
            },
            Id::PolicyPicker => {
                let workspace = match &self.modal_flow {
                    Some(ModalFlow::PolicyWorkspace { workspace }) => self
                        .sidebar
                        .workspace_iter()
                        .find(|(key, _)| key.as_str() == workspace.as_str())
                        .map(|(_, workspace)| Box::new(workspace.clone())),
                    _ => None,
                };
                PickFlow::Policy { workspace }
            }
            Id::WorkAgentPicker => {
                let picker = match &self.modal_flow {
                    Some(ModalFlow::WorkPicker { picker }) => Some(WorkPickerState {
                        targets: picker
                            .targets
                            .iter()
                            .map(|target| WorkPickTarget {
                                terminal_id: target.terminal_id,
                                agent_id: target.agent_id.clone(),
                            })
                            .collect(),
                        session_id: picker.session_id,
                        model_alias: picker.model_alias.clone(),
                    }),
                    _ => None,
                };
                PickFlow::WorkAgent { picker }
            }
            Id::SnoozeDuration => PickFlow::Snooze {
                session_key: match &self.modal_flow {
                    Some(ModalFlow::Snooze { workspace }) => Some(workspace.clone()),
                    _ => None,
                },
                now: chrono::Utc::now(),
            },
            Id::SourceSnooze => match &self.modal_flow {
                Some(ModalFlow::SourceSnooze { key, level }) => PickFlow::SourceSnooze {
                    key: key.clone(),
                    level: *level,
                    now: chrono::Utc::now(),
                },
                _ => PickFlow::Plain,
            },
            Id::SourceLevel => match &self.modal_flow {
                Some(ModalFlow::SourceLevel { key }) => PickFlow::SourceLevel { key: key.clone() },
                _ => PickFlow::Plain,
            },
            Id::ViewPicker => match &self.modal_flow {
                Some(ModalFlow::ViewPick { views }) => PickFlow::View {
                    views: views.clone(),
                },
                _ => PickFlow::Plain,
            },
            Id::ManageLabels => PickFlow::Labels {
                workspace_key: self.awaiting_repo_labels.clone(),
            },
            Id::AddAssignees => PickFlow::Assignees {
                workspace_key: match &self.modal_flow {
                    Some(ModalFlow::AssigneesRequest { workspace }) => Some(workspace.clone()),
                    _ => None,
                },
            },
            Id::ImportCheckoutList => PickFlow::Import {
                rows: match &self.modal_flow {
                    Some(ModalFlow::ImportList { rows }) => rows.clone(),
                    _ => Vec::new(),
                },
            },
            Id::FilterMenu => PickFlow::Filters,
            Id::InspectList => PickFlow::Inspect {
                rows: match &self.modal_flow {
                    Some(ModalFlow::InspectList { rows }) => rows.clone(),
                    _ => Vec::new(),
                },
            },
            Id::Editor => PickFlow::Editor {
                choices: self.setup.editor_choices.clone(),
                pending_workspace: self.setup.pending_editor_workspace.clone(),
                worktree: self.sidebar.selected_workspace().and_then(|workspace| {
                    workspace
                        .sessions
                        .first()
                        .map(|session| session.worktree_path.clone())
                }),
            },
            // Reconstruct the SAME filtered list the picker was mounted
            // with (#1100) so the picked index maps to the right app.
            Id::OpenWith => match self.actionable_open_with() {
                Some((apps, ctx)) => PickFlow::OpenWith { apps, ctx },
                None => PickFlow::Plain,
            },
            Id::Setup if self.setup.runner.is_some() => PickFlow::Runner,
            Id::Setup if !self.setup.settings_actions.is_empty() => PickFlow::Settings {
                action_count: self.setup.settings_actions.len(),
            },
            _ => PickFlow::Plain,
        }
    }

    fn consume_pick_state(
        &mut self,
        top: &Id,
        outcome: &lazybox_tui_core::choice::PickOutcome<crate::components::sidebar::FilterEntry>,
    ) {
        use lazybox_tui_core::choice::PickOutcome;

        match top {
            Id::PromptHistoryPicker
            | Id::SidebarContext
            | Id::HeaderContext
            | Id::AdoptTarget
            | Id::RequestReviewers
            | Id::PolicyPicker
            | Id::WorkAgentPicker
            | Id::SnoozeDuration
            | Id::SourceSnooze
            | Id::SourceLevel
            | Id::ViewPicker
            | Id::AddAssignees
            | Id::ImportCheckoutList
            | Id::LinearTeamRepo
            | Id::JiraProjectRepo
            | Id::HopperProject
            | Id::MoveToSpacePicker
            | Id::InspectList => {
                self.modal_flow = None;
            }
            Id::HandoffTarget if !matches!(outcome, PickOutcome::MountHandoffComposer { .. }) => {
                self.modal_flow = None;
            }
            Id::ConvertSessionRole
                if !matches!(outcome, PickOutcome::StartSessionConversion { .. }) =>
            {
                self.modal_flow = None;
            }
            Id::ManageLabels => {
                self.awaiting_repo_labels = None;
            }
            Id::DefaultModelPicker => {
                self.default_model_agent = None;
            }
            Id::ThemePicker => {
                self.theme_picker_prev = None;
            }
            Id::Editor => {
                self.setup.editor_choices.clear();
                if matches!(outcome, PickOutcome::ProvisionEditor { .. }) {
                    self.setup.pending_editor_workspace = None;
                }
            }
            Id::Setup if !matches!(outcome, PickOutcome::DispatchSettings(_)) => {
                self.setup.settings_actions.clear();
            }
            _ => {}
        }
    }

    fn apply_pick_outcome(
        &mut self,
        outcome: lazybox_tui_core::choice::PickOutcome<crate::components::sidebar::FilterEntry>,
    ) -> Vec<IpcCommand> {
        use lazybox_tui_core::choice::PickOutcome;

        let mut cmds = Vec::new();
        match outcome {
            PickOutcome::NoOp | PickOutcome::Pop => {}
            PickOutcome::MountBroadcastComposer { snippet_key, body } => {
                if let Some(ModalFlow::Broadcast { draft }) = self.modal_flow.as_mut() {
                    draft.snippet_key = snippet_key;
                    self.mount_broadcast_textarea(body);
                }
            }
            PickOutcome::StaleSnippet(key) => {
                tracing::warn!(
                    "snippet picker: picked key {key:?} but no entry in snippets — stale modal?",
                );
            }
            PickOutcome::Commands { commands, notice } => {
                cmds.extend(commands);
                if let Some(notice) = notice {
                    self.flash_info(notice);
                }
            }
            // Source-attention ladder (#scale): a client-side effect —
            // the sidebar applies + persists it; the daemon observes
            // the config change on its next tick.
            PickOutcome::SourceAttention { key, entry, notice } => {
                self.sidebar.set_source_attention(&key, entry);
                self.flash_info(notice);
            }
            // Saved view recall (#scale): apply + persist the frozen
            // lens.
            PickOutcome::ApplyView { name, lens } => {
                self.sidebar.apply_lens(&lens);
                self.flash_info(format!("view: {name}"));
            }
            PickOutcome::InsertSnippetDraft {
                terminal_id,
                snippet_key,
                category,
                body,
            } => {
                // Deliver the body to the composer without submitting …
                cmds.push(IpcCommand::DeliverSnippet {
                    terminal_id,
                    snippet_key,
                    category,
                    body: body.clone(),
                    submit: false,
                });
                // … and mirror it into the client's composing buffer so the
                // recap reflects it on a later manual submit and the
                // persisted draft (below) isn't clobbered by the next
                // body-less keystroke (#791). No-op for shells, which have no
                // composer recap.
                if let Some((id, draft)) = self.terminals.record_compose_insert(terminal_id, &body)
                {
                    cmds.push(IpcCommand::RecordComposingBuffer {
                        terminal_id: id,
                        buffer: draft,
                    });
                }
            }
            PickOutcome::DeliverPrompt { terminal_id, text } => {
                self.deliver_prompt(
                    terminal_id,
                    true,
                    &text,
                    lazybox_ipc::PromptSource::Typed,
                    &mut cmds,
                );
                self.flash_info("re-sent prompt");
            }
            PickOutcome::TriggerSkill {
                terminal_id,
                skill_name,
                text,
            } => {
                self.deliver_prompt(
                    terminal_id,
                    true,
                    &text,
                    lazybox_ipc::PromptSource::Typed,
                    &mut cmds,
                );
                self.apply_recent_skill(skill_name.clone());
                self.flash_info(format!("triggered skill: {skill_name}"));
            }
            PickOutcome::Jump(key) => self.jump_to_workspace_key(&key),
            PickOutcome::OpenUrl(url) => self.open_external_url(&url),
            PickOutcome::SaveTheme(name) => {
                crate::theme::set_by_name(&name);
                match lazybox_config::Config::save_with(|config| {
                    config.ui.theme = Some(name.clone());
                }) {
                    Ok(()) => self.flash_info(format!("theme: {name}")),
                    Err(error) => self.flash_info(format!("couldn't save theme: {error}")),
                }
                self.redraw = true;
            }
            PickOutcome::SaveDefaultAgent(agent) => {
                match lazybox_config::Config::save_with(|config| {
                    config.setup.default_agent = Some(agent.clone());
                }) {
                    Ok(()) => {
                        self.set_default_agent(&agent);
                        self.flash_info(format!("default agent: {agent}"));
                        self.redraw = true;
                        self.mount_default_model_picker(&agent);
                    }
                    Err(error) => self.flash_info(format!("couldn't save config: {error}")),
                }
            }
            PickOutcome::SaveDefaultModel { agent_id, alias } => {
                match lazybox_config::Config::save_with(|config| {
                    if alias.is_some() || config.agents.contains_key(&agent_id) {
                        config
                            .agents
                            .entry(agent_id.clone())
                            .or_default()
                            .models
                            .default = alias.clone();
                    }
                }) {
                    Ok(()) => {
                        let merged = lazybox_config::Config::load()
                            .unwrap_or_default()
                            .agent_models(&agent_id);
                        let label = merged
                            .default
                            .as_deref()
                            .and_then(|value| merged.tier(value))
                            .map(|tier| tier.label.clone());
                        self.agent_models.insert(agent_id, merged);
                        self.flash_info(match label {
                            Some(label) => format!("default model: {label}"),
                            None => "default model: agent default".to_string(),
                        });
                        self.redraw = true;
                    }
                    Err(error) => self.flash_info(format!("couldn't save config: {error}")),
                }
            }
            PickOutcome::DispatchAction {
                session_key,
                action,
            } => {
                if self.sidebar.focus_workspace_key(&session_key) {
                    // A right-click action names one explicit row. Drop any
                    // ambient `v` multi-select first so a bulk-destructive
                    // action (merge / archive / long-snooze) can't fan out
                    // over the whole selection instead of the clicked row
                    // (#899's `resolve_confirm_targets` reads that selection).
                    if super::dispatch::is_bulk_destructive(&action) {
                        self.sidebar.clear_broadcast_selection();
                    }
                    cmds.extend(self.dispatch_action(&action));
                } else {
                    self.flash_info("workspace is gone — action dropped");
                }
                self.redraw = true;
            }
            PickOutcome::AssignHopperProject {
                workspace,
                project,
                action,
            } => {
                self.pending_hopper_action = Some((workspace.clone(), action));
                cmds.push(IpcCommand::AssignHopperProject {
                    workspace_key: workspace,
                    project_key: project,
                });
                self.flash_hint("assigning repo…");
                self.redraw = true;
            }
            PickOutcome::DispatchCursorAction { action } => {
                // The right-click already parked the cursor on the
                // header; the action reads the cursor row directly.
                cmds.extend(self.dispatch_action(&action));
                self.redraw = true;
            }
            PickOutcome::MountHandoffComposer { target } => {
                if let Some(ModalFlow::Handoff { draft }) = self.modal_flow.as_mut() {
                    draft.target = Some(target);
                    self.mount_handoff_textarea();
                }
            }
            PickOutcome::StartSessionConversion { role } => {
                if let Some(ModalFlow::ConvertSession { draft }) = self.modal_flow.take() {
                    let request_id =
                        lazybox_ipc::AgentRunRequestId(uuid::Uuid::new_v4().to_string());
                    let prompt = lazybox_core::prompts::build_agent_handoff_request_prompt(role);
                    cmds.push(IpcCommand::StartAgentRun {
                        request_id: request_id.clone(),
                        session_key: draft.source.clone(),
                        session_id: None,
                        source_terminal_id: Some(draft.source_terminal),
                        agent: draft.agent.clone(),
                        mode: lazybox_ipc::AgentRuntimeMode::StreamJson,
                        cwd: None,
                        initial_input: Some(lazybox_ipc::AgentInputMessage {
                            text: Some(prompt),
                            json: None,
                        }),
                        resume_latest: true,
                        access: lazybox_ipc::AgentRunAccess::ReadOnly,
                        // Step 1: the handoff-generation run uses the agent's
                        // default model; escalating the Critic to a stronger
                        // tier is the next step (#1312 follow-up).
                        model_alias: None,
                    });
                    self.flash_info(format!(
                        "asking {} to author a {} handoff…",
                        draft.agent,
                        role.label().to_ascii_lowercase(),
                    ));
                    self.conversion = Some(super::PendingConversion {
                        draft,
                        role,
                        request_id,
                        run_id: None,
                        source_session_id: None,
                        response: String::new(),
                        target_prompt: None,
                        phase: super::ConversionPhase::Starting,
                    });
                }
            }
            PickOutcome::MountNewWorkspace(project_key) => {
                self.mount_new_workspace_input(project_key);
            }
            PickOutcome::MountNewProject => self.mount_new_project_input(),
            PickOutcome::StartChat => cmds.extend(self.start_chat_cmds()),
            PickOutcome::MountNewWorkspaceRepoPicker => self.mount_new_workspace_repo_picker(),
            PickOutcome::MountStartAgentPicker => {
                let projects = self.sidebar.projects_for_picker();
                self.mount_start_agent_picker(projects);
            }
            PickOutcome::AssignSpace { source, space } => {
                let resolved = self
                    .sidebar
                    .assign_source_to_space(&source, space.as_deref().unwrap_or(""));
                if let Some(name) = space {
                    // Remembered as the picker's preselection so filing
                    // the next repo into the same Space is one confirm
                    // (#1206). Unassign deliberately leaves it alone.
                    lazybox_config::Config::save_with_async(move |c| c.ui.last_space = Some(name));
                }
                self.flash_info(format!("{source} → {resolved}"));
                self.redraw = true;
            }
            PickOutcome::MountMoveToSpaceInput { source } => {
                self.mount_move_to_space_input(source);
            }
            PickOutcome::MapLinearTeam { team, repo } => {
                let (team_key, repo_slug) = (team.clone(), repo.clone());
                match lazybox_config::Config::save_with(move |config| {
                    config.providers.linear.teams.insert(team_key, repo_slug);
                }) {
                    Ok(()) => {
                        self.flash_info(format!("mapped Linear team {team} → {repo}"));
                        // The daemon reloads config on the next provision, so
                        // re-issuing the spawn now resolves through the freshly-
                        // persisted mapping (#1041) — no manual retry. Reached
                        // whether the picker opened directly (the primary path,
                        // no failure modal) or as the last-resort recovery, so
                        // it re-sends unconditionally rather than gating on a
                        // failed checklist.
                        self.reprovision_after_linear_map();
                    }
                    Err(error) => self.flash_error(format!("couldn't save mapping: {error}")),
                }
            }
            PickOutcome::MapJiraProject { project, repo } => {
                let (project_key, repo_slug) = (project.clone(), repo.clone());
                match lazybox_config::Config::save_with(move |config| {
                    config
                        .providers
                        .jira
                        .projects
                        .insert(project_key, repo_slug);
                }) {
                    Ok(()) => {
                        self.flash_info(format!("mapped Jira project {project} → {repo}"));
                        self.reprovision_after_linear_map();
                    }
                    Err(error) => self.flash_error(format!("couldn't save mapping: {error}")),
                }
            }
            PickOutcome::Reviewers {
                workspace_key,
                logins,
            } => {
                let count = logins.len();
                self.optimistic_chip_edit(&workspace_key, "reviewers", |workspace| {
                    if let Some(pr) = workspace.pr.as_mut() {
                        for login in &logins {
                            if !pr.reviewers.contains(login) {
                                pr.reviewers.push(login.clone());
                            }
                        }
                    }
                });
                cmds.push(IpcCommand::RequestReviewers {
                    workspace_key,
                    logins,
                });
                self.flash_info(format!("requested {count} reviewer(s)"));
            }
            PickOutcome::Work {
                target,
                session_id,
                model_alias,
            } => {
                self.push_work_command(
                    &target.agent_id,
                    Some(target.terminal_id),
                    session_id,
                    model_alias,
                    &mut cmds,
                );
            }
            PickOutcome::Labels {
                workspace_key,
                names,
            } => {
                let count = names.len();
                self.optimistic_chip_edit(&workspace_key, "labels", |workspace| {
                    let known: std::collections::HashMap<String, String> = workspace
                        .pr
                        .iter()
                        .flat_map(|pr| pr.labels.iter())
                        .chain(
                            workspace
                                .gh_issues
                                .first()
                                .into_iter()
                                .flat_map(|issue| issue.labels.iter()),
                        )
                        .map(|label| (label.name.clone(), label.color.clone()))
                        .collect();
                    let next = names
                        .iter()
                        .map(|name| lazybox_core::Label {
                            name: name.clone(),
                            color: known.get(name).cloned().unwrap_or_default(),
                        })
                        .collect();
                    if let Some(pr) = workspace.pr.as_mut() {
                        pr.labels = next;
                    } else if let Some(issue) = workspace.gh_issues.first_mut() {
                        issue.labels = next;
                    }
                });
                cmds.push(IpcCommand::SetLabels {
                    workspace_key,
                    names,
                });
                self.flash_info(if count == 0 {
                    "cleared labels".to_string()
                } else {
                    format!("set labels ({count})")
                });
            }
            PickOutcome::Assignees {
                workspace_key,
                logins,
            } => {
                let count = logins.len();
                self.optimistic_chip_edit(&workspace_key, "assignees", |workspace| {
                    if let Some(pr) = workspace.pr.as_mut() {
                        pr.assignees = logins.clone();
                    } else if let Some(issue) = workspace.gh_issues.first_mut() {
                        issue.assignees = logins.clone();
                    } else if let Some(issue) = workspace.linear_issues.first_mut() {
                        // Linear issues hold a single assignee; the
                        // provider keeps the LAST login (see
                        // LinearClient::set_assignees). Mirror that so the
                        // optimistic chips don't briefly show an
                        // impossible two-assignee state before the poll
                        // reconciles.
                        issue.assignees = logins.last().cloned().into_iter().collect();
                    }
                });
                cmds.push(IpcCommand::SetAssignees {
                    workspace_key,
                    logins,
                });
                self.flash_info(if count == 0 {
                    "cleared assignees".to_string()
                } else {
                    format!("set assignees ({count})")
                });
            }
            PickOutcome::MountImportConfirm(target) => {
                self.mount_import_checkout_confirm(target);
            }
            PickOutcome::SetFilters(entries) => {
                let count = entries.len();
                self.sidebar.set_filter_entries(entries);
                if count == 0 {
                    self.flash_info("filters cleared");
                } else {
                    self.flash_info(format!("{count} filter(s) active"));
                }
            }
            PickOutcome::MountInspectConfirm(target) => self.mount_inspect_confirm(target),
            PickOutcome::ProvisionEditor {
                workspace_key,
                editor,
                command,
                notice,
            } => {
                self.setup.pending_editor_launch = Some((workspace_key, editor));
                cmds.push(command);
                self.flash_info(notice);
            }
            PickOutcome::LaunchEditor { editor, worktree } => {
                self.launch_editor(&editor, &worktree);
            }
            PickOutcome::LaunchOpenWith { app, ctx } => {
                self.launch_open_with(&app, &ctx);
            }
            PickOutcome::DispatchSettings(index) => {
                let action = self.setup.settings_actions.get(index).cloned();
                self.setup.settings_actions.clear();
                if let Some(action) = action {
                    self.dispatch_settings_action(action);
                }
            }
            PickOutcome::Runner(_) => unreachable!("runner outcomes return before modal pop"),
        }
        cmds
    }
}

#[cfg(test)]
mod optimistic_assignee_tests {
    use crate::realm::Model;
    use chrono::Utc;
    use lazybox_core::{
        CiStatus, Mergeable, ReviewStatus, SessionKey, Task, TaskId, TaskKind, TaskRole, TaskState,
        Workspace, WorkspaceKey,
    };
    use lazybox_ipc::{Event as IpcEvent, channel};
    use lazybox_tui_core::choice::PickOutcome;
    use tuirealm::ratatui::layout::Size;

    fn issue_task(source: &str, url: &str, assignees: Vec<String>) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: source.into(),
                key: "ENG-1".into(),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Assignee,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: url.into(),
            repo: Some("o/r".into()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees,
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Unknown,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: Some("node".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: Some(TaskKind::Issue),
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    fn model_with(task: Task) -> (Model<tuirealm::terminal::TestTerminalAdapter>, WorkspaceKey) {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
        let ws = Workspace::from_task(task, Utc::now());
        let key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![ws],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        (m, key)
    }

    /// Linear issues hold a single assignee. When the picker submits the
    /// existing assignee (listed first) plus a newly-ticked one, the
    /// optimistic chips must show the last-wins single assignee — not an
    /// impossible two-assignee state that only self-corrects on the next
    /// poll.
    #[test]
    fn linear_assignee_optimistic_edit_reflects_single_last_login() {
        let (mut m, key) = model_with(issue_task(
            "linear",
            "https://linear.app/acme/issue/ENG-1",
            vec!["Alice".into()],
        ));

        let _ = m.apply_pick_outcome(PickOutcome::Assignees {
            workspace_key: key.clone(),
            logins: vec!["Alice".into(), "Bob".into()],
        });

        let session_key: SessionKey = (&key).into();
        let ws = m.sidebar.workspace_by_key(&session_key).expect("workspace");
        assert_eq!(
            ws.linear_issues[0].assignees,
            vec!["Bob".to_string()],
            "single-assignee Linear issue reflects last-wins, not a 2-assignee state",
        );
    }

    /// GitHub issues are multi-assignee; splitting the Linear branch out
    /// must not regress that — the full selected set is reflected.
    #[test]
    fn github_issue_assignee_optimistic_edit_keeps_all_logins() {
        let (mut m, key) = model_with(issue_task(
            "github",
            "https://github.com/o/r/issues/1",
            vec![],
        ));

        let _ = m.apply_pick_outcome(PickOutcome::Assignees {
            workspace_key: key.clone(),
            logins: vec!["Alice".into(), "Bob".into()],
        });

        let session_key: SessionKey = (&key).into();
        let ws = m.sidebar.workspace_by_key(&session_key).expect("workspace");
        assert_eq!(
            ws.gh_issues[0].assignees,
            vec!["Alice".to_string(), "Bob".to_string()],
            "GitHub issue keeps the full multi-assignee set",
        );
    }
}
