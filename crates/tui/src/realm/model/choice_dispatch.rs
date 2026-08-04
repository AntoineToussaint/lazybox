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

    fn pick_flow(&self, top: &Id, submit: bool) -> lazybox_tui_core::choice::PickFlow {
        use lazybox_tui_core::choice::{PickFlow, SnippetPick, WorkPickTarget, WorkPickerState};

        let snippets = || {
            self.snippets
                .all()
                .map(|(key, snippet)| SnippetPick {
                    key: key.to_string(),
                    category: snippet.category.clone(),
                    body: snippet.body.clone(),
                })
                .collect()
        };
        match top {
            Id::BroadcastSnippet => PickFlow::BroadcastSnippet {
                active: matches!(self.modal_flow, Some(ModalFlow::Broadcast { .. })),
                snippets: snippets(),
            },
            Id::SnippetPicker => PickFlow::Snippet {
                terminal_id: self.terminals.active_terminal_id(),
                snippets: snippets(),
                submit,
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
            Id::NewWorkspaceRepo => PickFlow::NewWorkspaceRepo,
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
        outcome: &lazybox_tui_core::choice::PickOutcome<crate::components::sidebar::Filter>,
    ) {
        use lazybox_tui_core::choice::PickOutcome;

        match top {
            Id::PromptHistoryPicker
            | Id::SidebarContext
            | Id::AdoptTarget
            | Id::RequestReviewers
            | Id::PolicyPicker
            | Id::WorkAgentPicker
            | Id::SnoozeDuration
            | Id::AddAssignees
            | Id::ImportCheckoutList
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
        outcome: lazybox_tui_core::choice::PickOutcome<crate::components::sidebar::Filter>,
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
                    cmds.extend(self.dispatch_action(&action));
                } else {
                    self.flash_info("workspace is gone — action dropped");
                }
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
            PickOutcome::SetFilters(filters) => {
                let count = filters.len();
                self.sidebar.set_filters(filters);
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
