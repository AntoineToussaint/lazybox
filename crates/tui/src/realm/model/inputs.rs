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

use super::{ChoicePayload, HelpQuestionKind, Id, ModalFlow, Model, Msg};
use crate::realm::UserEvent;
use lazybox_ipc::{Command as IpcCommand, TerminalId};
use std::path::{Path, PathBuf};
use tuirealm::terminal::TerminalAdapter;

/// The prompt the bulk "resume all rate-limited agents" action (`Shift-K`,
/// #847) injects into each limit-blocked agent to nudge it back to work
/// after the user has re-authed with another account.
const RESUME_PROMPT: &str = "continue";

/// The destination a `scaffold_skill` action resolves to, tagged with
/// how it was found so the confirm preview can say so plainly (#799):
/// writing to a fallback launch directory reads very differently from
/// writing into the focused workspace's repo.
pub(super) enum SkillScaffoldRoot {
    /// The focused workspace's live worktree.
    Worktree(PathBuf),
    /// Fallback: the directory lazybox was launched from, used when no
    /// focused workspace has an on-disk worktree.
    LaunchDir(PathBuf),
}

impl SkillScaffoldRoot {
    pub(super) fn path(&self) -> &Path {
        match self {
            Self::Worktree(p) | Self::LaunchDir(p) => p,
        }
    }

    /// One-line source phrase for the confirm preview, so a launch-dir
    /// fallback is never mistaken for the workspace's own repo.
    pub(super) fn describe(&self) -> &'static str {
        match self {
            Self::Worktree(_) => "the focused workspace's worktree",
            Self::LaunchDir(_) => "your launch directory (no workspace worktree is on disk)",
        }
    }
}

impl<T: TerminalAdapter> Model<T> {
    pub(super) fn dispatch_diff_review(
        &mut self,
        workspace_key: lazybox_core::WorkspaceKey,
        target: lazybox_ipc::WorkspaceDiffTarget,
        agent_terminal_ids: Vec<TerminalId>,
        comments: Vec<crate::realm::components::diff_review::DiffReviewComment>,
    ) -> Vec<IpcCommand> {
        let active = self
            .terminals
            .active_session()
            .filter(|active| active.as_str() == workspace_key.as_str())
            .and_then(|_| self.terminals.active_terminal_id())
            .filter(|terminal| agent_terminal_ids.contains(terminal));
        let terminal_id =
            active.or_else(|| (agent_terminal_ids.len() == 1).then(|| agent_terminal_ids[0]));
        let Some(terminal_id) = terminal_id else {
            let checkout = match target {
                lazybox_ipc::WorkspaceDiffTarget::Session(_) => "worktree",
                lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout => "linked checkout",
            };
            if agent_terminal_ids.is_empty() {
                self.flash_hint(format!(
                    "review not sent — this {checkout} has no running agent"
                ));
            } else {
                self.flash_hint(
                    "review not sent — several agents run in this checkout; focus the target and retry",
                );
            }
            return Vec::new();
        };

        let prompt = format_diff_review_prompt(&comments);
        let mut commands = Vec::new();
        self.deliver_prompt(
            terminal_id,
            true,
            &prompt,
            lazybox_ipc::PromptSource::Typed,
            &mut commands,
        );
        if self.modal_stack.last() == Some(&Id::DiffReview) {
            self.pop_modal();
        }
        self.flash_info(format!(
            "sent {} review comment{} to the agent",
            comments.len(),
            if comments.len() == 1 { "" } else { "s" }
        ));
        commands
    }

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
        let top = self.modal_stack.last().cloned();
        self.pop_modal();
        // The broadcast compose step shares the Textarea component with
        // Reply — route by the modal id that was on top, not by which
        // pending stash happens to be set.
        if matches!(top, Some(Id::BroadcastText)) {
            let cmds = self.dispatch_broadcast(&body);
            self.drain_queued_daemon_prompts();
            return cmds;
        }
        // The handoff compose step shares the Textarea component too —
        // route by top modal id, like broadcast (issue #431).
        if matches!(top, Some(Id::HandoffText)) {
            let cmds = self.dispatch_handoff(&body);
            self.drain_queued_daemon_prompts();
            return cmds;
        }
        // Notes also share the Textarea component. Unlike Reply, an
        // empty body is a valid submit — it clears the scratchpad — so
        // we persist whatever the user left rather than gating on
        // non-empty (issue #458).
        if matches!(top, Some(Id::Notes)) {
            let mut cmds = Vec::new();
            if let Some(ModalFlow::Notes {
                target: session_key,
            }) = self.modal_flow.take()
            {
                let cleared = body.trim().is_empty();
                cmds.push(IpcCommand::SetNotes {
                    session_key,
                    notes: body,
                });
                if cleared {
                    self.flash_info("Notes cleared");
                } else {
                    self.flash_info("Notes saved");
                }
            }
            self.drain_queued_daemon_prompts();
            return cmds;
        }
        let mut cmds = Vec::new();
        let target = match self.modal_flow.take() {
            Some(ModalFlow::Reply { target }) => Some(target),
            _ => None,
        };
        if let Some(session_key) = target
            && !body.trim().is_empty()
        {
            // Keep the composed text until the next reply: if the
            // daemon later reports the post failed (`ProviderError`
            // with source "reply"), the textarea is long gone — the
            // failure handler parks this in the messages log so the
            // user's words aren't lost.
            self.last_reply_body = Some(body.clone());
            cmds.push(IpcCommand::PostReply { session_key, body });
            self.flash_info("Reply submitted — fetching…");
            cmds.push(IpcCommand::Refresh);
        }
        self.drain_queued_daemon_prompts();
        cmds
    }

    /// Deliver a prompt `body` to one live terminal, appending the right
    /// IPC command(s) to `cmds`. An agent terminal gets the daemon's
    /// settle-gated `InjectPrompt` (+ a `RecordUserMessage` so its pinned
    /// "you ▸ …" recap updates): the body is pasted, then Enter is sent
    /// as a separate keystroke once the paste's repaint quiesces. A
    /// single `body + \r` write is NOT enough for an agent — Claude
    /// batches the burst as a paste and swallows the `\r` as a soft
    /// newline, so the prompt expands but never submits (#246). A plain
    /// shell has no paste debounce, so the encoded direct write submits
    /// cleanly. Shared by free-text broadcast and handoff paths so the
    /// #246 invariant lives in one place.
    pub(super) fn deliver_prompt(
        &mut self,
        terminal_id: TerminalId,
        is_agent: bool,
        body: &str,
        source: lazybox_ipc::PromptSource,
        cmds: &mut Vec<IpcCommand>,
    ) {
        if is_agent {
            // Feed the raw body + a submit `\r` (embedded newlines stay
            // `\n`, i.e. soft breaks) so the whole body commits as one
            // recap message.
            let mut recap = body.as_bytes().to_vec();
            recap.push(b'\r');
            if let Some(prompt) = self.terminals.record_pty_write(terminal_id, &recap, source) {
                cmds.push(IpcCommand::RecordUserMessage {
                    terminal_id,
                    prompt,
                });
            }
            cmds.push(IpcCommand::InjectPrompt {
                terminal_id,
                prompt: body.to_string(),
                fallback_spawn: None,
                submit: true,
            });
        } else {
            cmds.push(IpcCommand::Write {
                terminal_id,
                bytes: encode_snippet_for_pty(body),
                intent: lazybox_ipc::TerminalInputIntent::Submit,
            });
        }
    }

    /// Compose-submit for the broadcast flow. Delegates to the unified
    /// per-target pipeline ([`Self::dispatch_broadcast_op`], #1077): a
    /// broadcast is just a `Snippet` / `Prompt` op fanned over the targets
    /// stashed when the flow mounted. Session-less scoped workspaces get
    /// the default agent spun up seeded with the message (#836) behind a
    /// confirm gate; a selection that hits only live sessions runs through.
    fn dispatch_broadcast(&mut self, body: &str) -> Vec<IpcCommand> {
        let Some(ModalFlow::Broadcast { draft }) = self.modal_flow.take() else {
            return Vec::new();
        };
        // The compose step may leave the snippet's pre-fill padding (or
        // a stray trailing newline) behind; a trailing `\n` would reach
        // the agent as a soft line break, not content.
        let body = body.trim_end();
        if body.is_empty() {
            return Vec::new();
        }
        self.dispatch_broadcast_op(&draft.targets, draft.snippet_key.as_deref(), body)
    }

    /// Whether a session-less broadcast target can host an auto-started
    /// agent: it must resolve to a repo/project scope. A fully repo-less,
    /// project-less workspace (a Slack DM, a scratch row) has nothing to
    /// spawn into, so it stays skipped (#836).
    pub(super) fn broadcast_can_spawn(&self, key: &lazybox_core::SessionKey) -> bool {
        self.sidebar
            .workspace_by_key(key)
            .is_some_and(|w| w.worktree_scope().is_some())
    }

    /// Resume every workspace currently blocked on a provider usage /
    /// rate limit (`Shift-K`, #847). A one-shot settle-gated inject
    /// fan-out of a "continue" prompt across exactly the limit-blocked set
    /// — the bulk companion to re-authing with another account, so the
    /// user doesn't visit each terminal. Reuses the broadcast
    /// [`Self::deliver_prompt`] delivery, but sources its targets from the
    /// live agent-state map rather than the manual `v` selection (so it
    /// never touches that selection) and targets ONLY limit-blocked
    /// workspaces. Each target has a live agent (that's what `LimitReached`
    /// means), so none fall through to the spawn / skip cases.
    pub(super) fn resume_rate_limited_agents(&mut self) -> Vec<IpcCommand> {
        let terminals = self.sidebar.limit_reached_terminals();
        if terminals.is_empty() {
            self.flash_hint("no rate-limited agents to resume");
            return Vec::new();
        }
        let mut cmds = Vec::new();
        for terminal_id in &terminals {
            // Every `LimitReached` terminal is an agent (the state only
            // comes from agent detection), so `is_agent` is always true —
            // each gets the settle-gated `InjectPrompt`, never a shell
            // write.
            self.deliver_prompt(
                *terminal_id,
                true,
                RESUME_PROMPT,
                lazybox_ipc::PromptSource::Typed,
                &mut cmds,
            );
        }
        let resumed = terminals.len();
        let plural = if resumed == 1 { "" } else { "s" };
        self.flash_info(format!("resuming {resumed} rate-limited agent{plural}"));
        self.redraw = true;
        cmds
    }

    /// Deliver the composed handoff body into the target session
    /// (`x s`, issue #431): a running agent gets the same settle-gated
    /// `InjectPrompt` (+ `RecordUserMessage` recap) the broadcast path
    /// uses; a plain shell gets the encoded direct write. The visible
    /// "source → target" notice records the A→B trail. An empty body
    /// (the user cleared the seed) or a target that lost its session
    /// between pick and submit cancels with a notice, sending nothing.
    fn dispatch_handoff(&mut self, body: &str) -> Vec<IpcCommand> {
        let Some(ModalFlow::Handoff { draft }) = self.modal_flow.take() else {
            return Vec::new();
        };
        let Some(target) = draft.target else {
            return Vec::new();
        };
        let body = body.trim_end();
        if body.is_empty() {
            self.flash_info("handoff cancelled — nothing to send");
            return Vec::new();
        }
        let target_name = self
            .sidebar
            .workspace_by_key(&target)
            .map(|w| w.name.clone())
            .unwrap_or_else(|| target.to_string());
        let mut cmds = Vec::new();
        match self.sidebar.broadcast_terminal(&target) {
            Some((terminal_id, true)) => {
                self.deliver_prompt(
                    terminal_id,
                    true,
                    body,
                    lazybox_ipc::PromptSource::Typed,
                    &mut cmds,
                );
                self.flash_info(format!("handoff: {} → {target_name}", draft.source_name));
            }
            // The target's agent ended between picking it and submitting
            // (its session is gone, or only a shell remains — and a brief
            // is meant for an agent). Don't silently drop the composed
            // work: re-open the picker seeded with it so the user can
            // route it to another agent (or Esc out). `mount_handoff_picker`
            // nudges on its own if no other agent is running.
            _ => {
                self.flash_info(format!(
                    "{target_name}'s agent session ended — pick another target"
                ));
                self.mount_handoff_picker(&draft.source, draft.source_name, body.to_string());
            }
        }
        self.redraw = true;
        cmds
    }

    /// Surface the next queued daemon prompt (removal / merge) if the
    /// modal stack just emptied. Every handler that pops a modal must
    /// call this — the daemon dedupes its emits, so a prompt that
    /// arrives while another modal is up and never gets re-surfaced
    /// here is invisible forever.
    fn drain_queued_daemon_prompts(&mut self) {
        self.maybe_mount_next_auth_prompt();
        self.maybe_mount_next_removal_prompt();
        self.maybe_mount_next_merge_prompt();
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
                let project_key = match self.modal_flow.take() {
                    Some(ModalFlow::NewWorkspaceProject { project }) => Some(project),
                    _ => None,
                };
                match (name.is_empty(), project_key) {
                    (false, Some(project_key)) => {
                        // Land the user in a live session immediately:
                        // creating a workspace and then having to know to
                        // press `c` was the main first-run friction. The
                        // daemon spawns the configured default agent into
                        // the new workspace (see `CreateWorkspace`
                        // server handler). Same behavior for the global
                        // "start agent" shortcut, which funnels here.
                        let spawn_agent = Some(self.sidebar.default_agent().to_string());
                        let client_request_id = uuid::Uuid::new_v4().hyphenated().to_string();
                        self.pending_workspace_creates.insert(
                            client_request_id.clone(),
                            super::PendingWorkspaceCreate {
                                name: name.clone(),
                                spawn_agent: spawn_agent.is_some(),
                                workspace_key: None,
                            },
                        );
                        tracing::info!(
                            workspace_name = %name,
                            project_key = %project_key,
                            %client_request_id,
                            ?spawn_agent,
                            "creating new pre-PR workspace under project",
                        );
                        cmds.push(IpcCommand::CreateWorkspace {
                            name,
                            project_key,
                            spawn_agent,
                            client_request_id: Some(client_request_id),
                        });
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
            Some(Id::RenameSpace) => {
                let new = text.trim().to_string();
                let old = match self.modal_flow.take() {
                    Some(ModalFlow::RenameSpace { space }) => Some(space),
                    _ => None,
                };
                if let Some(old) = old {
                    match self.sidebar.rename_space(&old, &new) {
                        Some((old, new)) => {
                            self.flash_info(format!("Space {old} → {new}"));
                            self.redraw = true;
                        }
                        // Blank / unchanged: advise, never error.
                        None => self.flash_hint("Space name unchanged"),
                    }
                } else {
                    tracing::warn!("rename-space submit without a stashed name — dropped");
                }
            }
            Some(Id::RenameWorkspace) => {
                let name = text.trim().to_string();
                let target = match self.modal_flow.take() {
                    Some(ModalFlow::RenameWorkspace { target }) => Some(target),
                    _ => None,
                };
                match (name.is_empty(), target) {
                    (false, Some(session_key)) => {
                        tracing::info!(
                            workspace = %session_key,
                            new_name = %name,
                            "renaming workspace",
                        );
                        cmds.push(IpcCommand::RenameWorkspace { session_key, name });
                    }
                    (true, _) => {}
                    (false, None) => {
                        tracing::warn!(
                            new_name = %name,
                            "rename submit without a stashed target — dropped",
                        );
                    }
                }
            }
            Some(Id::MoveToSpace) => {
                let space = text.trim().to_string();
                let source = match self.modal_flow.take() {
                    Some(ModalFlow::MoveToSpace { source }) => Some(source),
                    _ => None,
                };
                if let Some(source) = source {
                    let resolved = self.sidebar.assign_source_to_space(&source, &space);
                    if !space.is_empty() {
                        // A typed name (usually a brand-new Space) becomes
                        // the picker's next preselection (#1206).
                        let last = space.clone();
                        lazybox_config::Config::save_with_async(move |c| {
                            c.ui.last_space = Some(last)
                        });
                    }
                    self.flash_info(format!("{source} → {resolved}"));
                    self.redraw = true;
                } else {
                    tracing::warn!("move-to-space submit without a stashed source — dropped");
                }
            }
            Some(Id::NewProject) => {
                let name = text.trim().to_string();
                if !name.is_empty() {
                    tracing::info!(project_name = %name, "creating new local project");
                    // Stash the name so the matching `ProjectUpserted`
                    // event can focus the new header + auto-mount the
                    // new-workspace input. Without this hand-off, the
                    // freshly-created project is unreachable via j/k
                    // (header rows are skipped by `move_cursor_by`).
                    self.deferred_focus_project = Some(name.clone());
                    cmds.push(IpcCommand::CreateProject { name });
                }
            }
            Some(Id::LlmGatewayUrl) => {
                let url = text.trim().to_string();
                // Empty input clears the gateway; `gateway_url` already
                // normalizes blank → unset, but store `None` so the YAML
                // key drops out entirely rather than persisting "".
                let value = (!url.is_empty()).then_some(url.clone());
                let saved = lazybox_config::Config::save_with(|c| {
                    c.agent.llm_gateway_url = value.clone();
                });
                match saved {
                    Ok(()) if url.is_empty() => {
                        self.flash_info("LLM gateway cleared — agents talk to the vendor directly")
                    }
                    Ok(()) => self.flash_info(format!("LLM gateway set to {url}")),
                    Err(e) => self.flash_info(format!("couldn't save config: {e}")),
                }
            }
            Some(Id::AddScanRoot) => {
                let typed = std::path::PathBuf::from(text.trim());
                let expanded = expand_scan_root(&typed);
                // Keep the readable `~/`/absolute form the user typed when
                // it already resolves absolutely; pin a relative path to
                // the client's CWD *now* so the daemon — which may run
                // with a different CWD in out-of-process mode — scans the
                // same directory instead of one relative to its own.
                let to_store = if expanded.is_absolute() {
                    typed
                } else {
                    std::path::absolute(&expanded).unwrap_or(expanded)
                };
                // Absolute, tilde-expanded key for existence + dedup, so
                // `~/code` and its absolute equivalent count as one root.
                let resolved = resolve_scan_root(&to_store);
                let exists = resolved.is_dir();

                // Dedup + append inside the single write, under the config
                // crate's save lock, so two quick adds can't both pass a
                // separate pre-check and duplicate the root.
                let push = to_store.clone();
                let mut added = false;
                let saved = lazybox_config::Config::save_with(|c| {
                    if c.scan
                        .roots
                        .iter()
                        .all(|r| resolve_scan_root(r) != resolved)
                    {
                        c.scan.roots.push(push);
                        added = true;
                    }
                });
                match saved {
                    Ok(()) if added => {
                        // Don't block a not-yet-present root (a network
                        // mount, a dir made later) — persist it, but flag
                        // the likely typo. YAML/CLI roots aren't validated
                        // either.
                        let note = if exists { "" } else { " (not present yet)" };
                        self.flash_info(format!(
                            "added scan root {}{note} — scanning…",
                            to_store.display()
                        ));
                        cmds.push(IpcCommand::ScanCheckouts {
                            roots: vec![to_store],
                        });
                    }
                    Ok(()) => {
                        self.flash_info(format!("{} is already a scan root", to_store.display()))
                    }
                    Err(e) => self.flash_info(format!("couldn't save config: {e}")),
                }
            }
            Some(Id::EditorForm) => {
                use crate::realm::components::input::Input;
                use crate::realm::model::EditorFormStage;
                let stage = match self.modal_flow.take() {
                    Some(ModalFlow::EditorForm { stage }) => stage,
                    _ => {
                        tracing::warn!("editor-form submit without a stashed stage — dropped");
                        return cmds;
                    }
                };
                let text = text.trim().to_string();
                match stage {
                    EditorFormStage::AwaitId => {
                        // A blank id would collide with itself and can't be
                        // launched — reject and drop back to the panel.
                        if text.is_empty() {
                            self.flash_error("editor id can't be empty");
                            self.mount_editors_panel();
                        } else {
                            // Advance to the launch-command prompt; display
                            // is left unset so the launch path titlecases
                            // the id (matching `From<UserEditorEntry>`).
                            self.set_modal_flow(ModalFlow::EditorForm {
                                stage: EditorFormStage::AwaitCommand {
                                    id: text,
                                    display: None,
                                },
                            });
                            let modal = Input::new("Launch command")
                                .title("Add editor")
                                .placeholder("e.g. code {path}");
                            self.mount_modal(Id::EditorForm, modal);
                        }
                    }
                    EditorFormStage::AwaitCommand { id, display } => {
                        self.save_editor_entry(&id, display, &text);
                    }
                }
            }
            Some(Id::SandboxInput) => {
                use crate::sandbox_flow::SandboxStage;
                let Some(ModalFlow::SandboxOnboarding { mut draft }) = self.modal_flow.take()
                else {
                    tracing::warn!("sandbox input submit without a stashed draft — dropped");
                    return cmds;
                };
                match draft.stage {
                    SandboxStage::GcpKey => {
                        // Non-interactive credential-state check (#1112): a
                        // blank path means ambient credentials; a given path
                        // must be a readable file, else re-ask with an
                        // actionable error. No CLI, no network — just an fs
                        // read, mirroring `GcpProvider::check_auth`'s offline
                        // key validation.
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            draft.set_key(None);
                        } else {
                            let typed = std::path::PathBuf::from(trimmed);
                            let expanded = expand_scan_root(&typed);
                            match std::fs::File::open(&expanded) {
                                // Store the `~/`-form the user typed so the
                                // YAML stays readable, like scan roots.
                                Ok(_) => draft.set_key(Some(typed)),
                                Err(e) => {
                                    self.flash_error(format!(
                                        "can't read service-account key {}: {e} — point it at a \
                                         readable JSON key, or leave blank for ambient credentials",
                                        expanded.display()
                                    ));
                                    draft.stage = SandboxStage::GcpKey;
                                }
                            }
                        }
                    }
                    SandboxStage::Project => {
                        draft.set_project(text);
                        if draft.needs_project() {
                            // A GCP box can't be provisioned without a
                            // project — re-ask rather than persist a config
                            // that would fail at connect time.
                            draft.stage = SandboxStage::Project;
                            self.flash_error("a GCP project id is required");
                        }
                    }
                    SandboxStage::Zone => draft.set_zone(text),
                    SandboxStage::User => draft.set_user(text),
                    SandboxStage::E2bTemplate => draft.set_template(text),
                    // No other stage mounts an Input modal.
                    other => {
                        tracing::warn!(
                            ?other,
                            "sandbox input submit at a non-input stage — dropped"
                        );
                        return cmds;
                    }
                }
                self.mount_sandbox_stage(draft);
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
        self.drain_queued_daemon_prompts();
        cmds
    }

    /// A question submitted from the `HelpAsk` modal. Follow-ups ride
    /// the current run; new questions interrupt it and start with fresh
    /// context.
    pub fn handle_help_question(
        &mut self,
        question: String,
        kind: HelpQuestionKind,
    ) -> Vec<IpcCommand> {
        use lazybox_ipc::AgentInputMessage;

        let question = question.trim().to_string();
        if question.is_empty() {
            return Vec::new();
        }
        let mut cmds = if kind == HelpQuestionKind::NewQuestion {
            self.reset_help_session()
        } else {
            Vec::new()
        };
        {
            let mut convo = self.help_convo_mut();
            convo.notice = None;
            convo
                .turns
                .push(crate::realm::components::help_ask::HelpTurn {
                    question: question.clone(),
                    ..Default::default()
                });
            convo.activate_thread();
        }
        self.redraw = true;
        if self.help_interrupt_on_start {
            if self.help_restart_question.is_none() {
                self.help_restart_question = Some(question);
            } else {
                self.help_pending_questions.push(question);
            }
            return cmds;
        }
        if let Some(run_id) = self.help_run {
            cmds.push(IpcCommand::SendAgentInput {
                run_id,
                message: AgentInputMessage {
                    text: Some(question),
                    json: None,
                },
            });
            return cmds;
        }
        if self.help_start_request.is_some() {
            self.help_pending_questions.push(question);
            return cmds;
        }
        if let Some(cmd) = self.start_help_run_command(&question) {
            cmds.push(cmd);
        }
        cmds
    }

    pub(super) fn start_help_run_command(&mut self, question: &str) -> Option<IpcCommand> {
        use lazybox_ipc::{AgentInputMessage, AgentRunAccess, AgentRuntimeMode};
        use lazybox_tui_core::help::{HELP_AGENT_PREFERENCE, HELP_SESSION_KEY, select_help_agent};

        let Some(help_agent) = select_help_agent(&self.agents, Some(self.sidebar.default_agent()))
        else {
            self.help_pending_questions.clear();
            let mut convo = self.help_convo_mut();
            convo.close_open_turns();
            convo.deactivate_thread();
            convo.notice = Some(format!(
                "the help assistant needs a structured agent ({}) enabled — \
showing keybinding search only",
                HELP_AGENT_PREFERENCE.join(" or ")
            ));
            return None;
        };
        let request_id =
            lazybox_ipc::AgentRunRequestId(uuid::Uuid::new_v4().hyphenated().to_string());
        self.help_start_request = Some(request_id.clone());
        let context = lazybox_tui_core::help::agent_context(
            &self.catalog,
            self.ui_defaults.terminal_escape_char,
        );
        Some(IpcCommand::StartAgentRun {
            request_id,
            session_key: lazybox_core::SessionKey::new(HELP_SESSION_KEY),
            session_id: None,
            source_terminal_id: None,
            agent: help_agent.to_string(),
            mode: AgentRuntimeMode::StreamJson,
            // Left to the daemon: the sentinel key resolves to no
            // workspace and `resolve_cwd` picks a neutral cwd on ITS
            // host — a client-side path wouldn't exist there in
            // out-of-process / remote mode.
            cwd: None,
            initial_input: Some(AgentInputMessage {
                text: Some(format!("{context}\n\n# Question\n\n{question}")),
                json: None,
            }),
            resume_latest: false,
            access: AgentRunAccess::ReadOnly,
        })
    }

    fn reset_help_session(&mut self) -> Vec<IpcCommand> {
        let interrupt = self
            .help_run
            .take()
            .map(|run_id| IpcCommand::InterruptAgentRun { run_id });
        self.help_pending_questions.clear();
        self.help_restart_question = None;
        if interrupt.is_some() {
            self.help_start_request = None;
            self.help_interrupt_on_start = false;
        } else {
            self.help_interrupt_on_start = self.help_start_request.is_some();
        }
        *self.help_convo_mut() = Default::default();
        interrupt.into_iter().collect()
    }

    /// Open the "Ask about this PR" chat (#945) for the focused
    /// workspace's PR (or, failing that, its first issue). Snapshots the
    /// task + activity, tears down any prior PR-chat thread, kicks off
    /// the worktree-diff read that grounds answers, and mounts the modal.
    pub(super) fn open_pr_chat(&mut self) {
        use super::PrChatSubject;
        use lazybox_ipc::WorkspaceDiffTarget;

        let Some(workspace) = self.sidebar.selected_workspace() else {
            self.flash_hint("no workspace focused to ask about");
            return;
        };
        // Target the same task the reader renders — `primary_task()`
        // (PR, else first issue) — so the chat is always about what the
        // user was reading, and the two can't drift apart.
        let Some(task) = workspace.primary_task() else {
            self.flash_hint("this workspace has no PR or issue to ask about");
            return;
        };
        let label = task.id.key.clone();
        let task = task.clone();
        let activity = workspace.activity.clone();
        let workspace_key = workspace.key.clone();
        // Diff-first grounding: prefer a session worktree, else a linked
        // checkout. Neither → no worktree, answered from metadata alone.
        let target = workspace
            .default_session()
            .map(|session| WorkspaceDiffTarget::Session(session.id))
            .or_else(|| {
                workspace
                    .linked_checkout
                    .as_ref()
                    .map(|_| WorkspaceDiffTarget::LinkedCheckout)
            });
        let has_worktree = target.is_some();

        // Tear down any prior PR-chat run and thread before rebinding.
        for cmd in self.reset_pr_chat_run() {
            self.send_cmd(cmd);
        }

        self.pr_chat_subject = Some(PrChatSubject {
            task,
            activity,
            has_worktree,
        });
        let grounding = if has_worktree {
            "Grounded in the PR's metadata, activity, and local worktree diff.".to_string()
        } else {
            "No worktree checked out — grounded in the PR's metadata and activity only.".to_string()
        };
        if let Some(target) = target {
            self.pr_chat_diff = None;
            self.pr_chat_diff_target = Some((workspace_key.clone(), target.clone()));
            self.send_cmd(IpcCommand::InspectWorkspaceDiff {
                workspace_key,
                target,
            });
        } else {
            // Nothing to inspect — resolve immediately so the first
            // question starts without waiting for a reply that never comes.
            self.pr_chat_diff = Some(None);
            self.pr_chat_diff_target = None;
        }

        self.mount_modal(
            Id::PrChat,
            crate::realm::components::pr_chat::PrChat::new(
                self.pr_chat_convo.clone(),
                label,
                grounding,
            ),
        );
        self.redraw = true;
    }

    /// A question submitted from the `PrChat` modal. Mirrors
    /// [`Self::handle_help_question`]: follow-ups ride the live run, new
    /// questions reset the thread. The opening question is held until the
    /// diff reply lands so it enters the run's context-bearing first turn.
    pub fn handle_pr_chat_question(
        &mut self,
        question: String,
        kind: HelpQuestionKind,
    ) -> Vec<IpcCommand> {
        use lazybox_ipc::AgentInputMessage;

        let question = question.trim().to_string();
        if question.is_empty() {
            return Vec::new();
        }
        let mut cmds = if kind == HelpQuestionKind::NewQuestion {
            self.reset_pr_chat_run()
        } else {
            Vec::new()
        };
        {
            let mut convo = self.pr_chat_convo_mut();
            convo.notice = None;
            convo
                .turns
                .push(crate::realm::components::help_ask::HelpTurn {
                    question: question.clone(),
                    ..Default::default()
                });
            convo.activate_thread();
        }
        self.redraw = true;
        if let Some(run_id) = self.pr_chat_run {
            cmds.push(IpcCommand::SendAgentInput {
                run_id,
                message: AgentInputMessage {
                    text: Some(question),
                    json: None,
                },
            });
            return cmds;
        }
        if self.pr_chat_request.is_some() {
            self.pr_chat_pending.push(question);
            return cmds;
        }
        // Opening question: hold it until the diff read resolves, so it
        // rides the run's first turn (the only one that carries context).
        // A second question asked during that window can't overwrite the
        // first — it queues as a follow-up the run flushes once started,
        // mirroring the `pr_chat_request` window above.
        if self.pr_chat_diff.is_none() {
            if self.pr_chat_held_question.is_none() {
                self.pr_chat_held_question = Some((question, kind));
            } else {
                self.pr_chat_pending.push(question);
            }
            return cmds;
        }
        if let Some(cmd) = self.start_pr_chat_run(&question) {
            cmds.push(cmd);
        }
        cmds
    }

    pub(super) fn start_pr_chat_run(&mut self, question: &str) -> Option<IpcCommand> {
        use lazybox_ipc::{AgentInputMessage, AgentRunAccess, AgentRuntimeMode};
        use lazybox_tui_core::help::{HELP_AGENT_PREFERENCE, select_help_agent};
        use lazybox_tui_core::pr_chat::{PR_CHAT_SESSION_KEY, PrDiff, pr_context};

        let subject = self.pr_chat_subject.as_ref()?;
        let Some(agent) = select_help_agent(&self.agents, Some(self.sidebar.default_agent()))
        else {
            self.pr_chat_pending.clear();
            let mut convo = self.pr_chat_convo_mut();
            convo.close_open_turns();
            convo.deactivate_thread();
            convo.notice = Some(format!(
                "Ask about this PR needs a structured agent ({}) enabled",
                HELP_AGENT_PREFERENCE.join(" or ")
            ));
            return None;
        };
        let diff = match &self.pr_chat_diff {
            Some(Some(dto)) => PrDiff::Available(dto),
            Some(None) if subject.has_worktree => PrDiff::Unreadable,
            _ => PrDiff::NoWorktree,
        };
        let context = pr_context(&subject.task, &subject.activity, diff);
        let request_id =
            lazybox_ipc::AgentRunRequestId(uuid::Uuid::new_v4().hyphenated().to_string());
        self.pr_chat_request = Some(request_id.clone());
        Some(IpcCommand::StartAgentRun {
            request_id,
            session_key: lazybox_core::SessionKey::new(PR_CHAT_SESSION_KEY),
            session_id: None,
            source_terminal_id: None,
            agent: agent.to_string(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: Some(AgentInputMessage {
                text: Some(format!("{context}\n\n# Question\n\n{question}")),
                json: None,
            }),
            resume_latest: false,
            access: AgentRunAccess::ReadOnly,
        })
    }

    /// Interrupt the live PR-chat run and clear the thread, keeping the
    /// subject + diff so the next question starts a fresh run with the
    /// same context.
    fn reset_pr_chat_run(&mut self) -> Vec<IpcCommand> {
        let interrupt = self
            .pr_chat_run
            .take()
            .map(|run_id| IpcCommand::InterruptAgentRun { run_id });
        self.pr_chat_request = None;
        self.pr_chat_pending.clear();
        self.pr_chat_held_question = None;
        *self.pr_chat_convo_mut() = Default::default();
        interrupt.into_iter().collect()
    }

    /// Route a Choice modal pick through the pure tui-core resolver,
    /// then apply its typed outcome.
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    /// Editor / Settings / runner outcomes may still emit commands
    /// internally through their effect helpers; directly-visible IPC
    /// commands land in the Vec.
    pub fn handle_choice_picked(&mut self, picks: Vec<ChoicePayload>) -> Vec<IpcCommand> {
        self.handle_choice_picked_with_submit(picks, true)
    }

    /// The `Shift-Enter` counterpart of [`Self::handle_choice_picked`]: the
    /// snippet picker inserts the body into the composer *without* the
    /// trailing submit CR, so the user can edit it before sending (issue
    /// #791). Every other picker ignores the distinction — its
    /// `pick_no_submit` falls back to `pick` — so this is a plain pick for
    /// them.
    pub fn handle_choice_picked_no_submit(&mut self, picks: Vec<ChoicePayload>) -> Vec<IpcCommand> {
        self.handle_choice_picked_with_submit(picks, false)
    }

    fn handle_choice_picked_with_submit(
        &mut self,
        picks: Vec<ChoicePayload>,
        submit: bool,
    ) -> Vec<IpcCommand> {
        let cmds = self.choice_picked_inner(picks, submit);
        // The inner handler has many early-return arms; drain queued
        // daemon prompts HERE so every one of them gets the "modal
        // stack may have just emptied" treatment. (No-op while any
        // modal — including one the pick itself mounted — is up.)
        self.drain_queued_daemon_prompts();
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
        if self.modal_stack.last() == Some(&Id::Update) {
            self.pop_modal();
            if let Some(ModalFlow::UpdateTarget { target }) = self.modal_flow.take() {
                // Let the daemon own the dismissal (#548) so it sticks
                // across clients and restarts. Record it locally too so a
                // re-derived same-target update this session stays hidden
                // without waiting on the next snapshot.
                if !self.dismissed_updates.iter().any(|t| t == &target) {
                    self.dismissed_updates.push(target.clone());
                }
                self.send_cmd(IpcCommand::SetUpdateDismissal { target });
            }
            self.drain_queued_daemon_prompts();
            return Vec::new();
        }
        if let Some(mut runner) = self.setup.runner.take() {
            let step = runner.step_dismissed();
            self.handle_runner_step(runner, step);
            return Vec::new();
        }
        // Dispatch by which modal was on top BEFORE the pop so we
        // route the "no" decision correctly.
        let top = self.modal_stack.last().cloned();
        self.pop_modal();
        if top == Some(Id::SnippetPicker)
            && let Some(ModalFlow::TourSnippet { return_step, .. }) = self.modal_flow.take()
        {
            self.mount_tour_at(return_step);
            return Vec::new();
        }
        // Cancelling any modal drops its [`ModalFlow`] continuation.
        // This one line replaces the ~two-dozen per-variant clears that
        // used to be here (each `pending_* = None`) — a missed one was
        // the leak the enum exists to prevent. Notable semantics that
        // still hold by dropping the flow:
        //   * RemoveOutOfScope Esc = defer; the daemon re-emits later.
        //   * MergeConfirm Esc = "decide later" — it does NOT send
        //     `ConfirmMerge { accept: false }` (that pins the issue as
        //     rejected until restart); the daemon's `prompted_merge`
        //     re-fires after `MERGE_REPROMPT_AFTER`. Only an explicit N
        //     (`handle_confirmed`) pins the rejection.
        //   * Broadcast / Handoff Esc cancels compose; the sidebar
        //     multi-select survives (only composing was abandoned).
        self.modal_flow = None;
        let mut cmds: Vec<IpcCommand> = Vec::new();
        // A few modals carry cancel state that is NOT part of the flow
        // enum — release it here.
        match top {
            Some(Id::Help) | Some(Id::HelpAsk) => {
                cmds.extend(self.reset_help_session());
            }
            Some(Id::ManageLabels) => {
                // The label picker's target lives in `awaiting_repo_labels`
                // (armed before the modal, coexists with others), not in
                // `modal_flow` — drop it so a later stray `RepoLabels`
                // can't re-mount on a stale target.
                self.awaiting_repo_labels = None;
            }
            Some(Id::WorktreeProgress) => {
                // Esc on the checklist — remember WHICH provisioning op
                // was dismissed so its later `WorktreeProgress` events
                // don't resurrect the modal on top of whatever the user
                // is typing (they update silently instead; see
                // `apply_worktree_progress`). Then drop the accumulated
                // state so a later spawn starts a fresh one.
                // While provisioning is still in flight, Esc is a real
                // cancel: tell the daemon to abort the provision (which
                // kills a wedged `git clone` and releases the in-flight
                // singleton claim so a retry starts fresh — issue #403)
                // rather than just closing the view over a hang. A
                // finished op (failed / degraded / session already
                // live) has nothing left to cancel.
                if let Some(state) = self.worktree_progress.as_ref()
                    && !state.failed()
                    && !state.warned()
                    && !state.dismiss_queued()
                {
                    cmds.push(IpcCommand::CancelSpawn {
                        session_key: state.session_key.clone(),
                    });
                }
                self.worktree_progress_dismissed = self
                    .worktree_progress
                    .as_ref()
                    .map(|s| s.session_key.clone());
                self.worktree_progress = None;
            }
            Some(Id::ThemePicker) => {
                // Esc cancels the preview: restore the palette that was
                // active when the picker opened.
                if let Some(prev) = self.theme_picker_prev.take() {
                    crate::theme::set_by_name(&prev);
                }
                self.redraw = true;
            }
            Some(Id::DefaultModelPicker) => {
                self.default_model_agent = None;
            }
            Some(Id::Setup) => {
                // Esc on the (non-runner) Settings window — drop the
                // stashed rows so a stale flat index can never be
                // resolved by a later Setup-id mount. The runner-owned
                // wizard path never reaches this match (it early-
                // returns through `step_dismissed` above).
                self.setup.settings_actions.clear();
            }
            _ => {}
        }
        // Always try to surface a queued prompt after a modal
        // dismisses — not just when the dismissed modal itself was
        // a prompt. Otherwise a user who has Help / Settings open
        // when the daemon emits a prompt would have it stuck in
        // the queue.
        self.drain_queued_daemon_prompts();
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
            Some(Id::AgentAuth) => {
                if let Some(ModalFlow::AgentAuth { terminal_id, retry }) = self.modal_flow.take() {
                    if yes {
                        cmds.push(IpcCommand::ReauthenticateAgent {
                            terminal_id,
                            switch_account: true,
                        });
                    } else if retry {
                        cmds.push(IpcCommand::CancelAgentReauthentication { terminal_id });
                    }
                }
            }
            Some(Id::RemoveOutOfScope) => {
                if let Some(ModalFlow::RemovalPrompt { workspace, reason }) = self.modal_flow.take()
                {
                    let workspace_key = workspace;
                    let session_key: lazybox_core::SessionKey = (&workspace_key).into();
                    match (yes, reason) {
                        // Out-of-scope: drop the row + kill terminals
                        // (worktree left on disk).
                        (true, super::RemovalReason::OutOfScope) => {
                            cmds.push(IpcCommand::Kill { session_key });
                        }
                        // Merged/Closed: also delete the worktree.
                        (true, super::RemovalReason::Merged | super::RemovalReason::Closed) => {
                            cmds.push(IpcCommand::RemoveMergedWorkspace { session_key });
                        }
                        // Explicit "no" on the merged/closed prompt is a
                        // decision the daemon must hear: it pins the
                        // workspace in `removal_prompts.kept` so the
                        // level-triggered re-emit stops asking. (Esc
                        // routes through `handle_modal_dismissed`
                        // instead and stays silent — the daemon
                        // re-prompts after its reprompt interval.)
                        (false, super::RemovalReason::Merged | super::RemovalReason::Closed) => {
                            cmds.push(IpcCommand::KeepMergedWorkspace { session_key });
                        }
                        // Out-of-scope "no" has nothing to tell the
                        // daemon — its own prompt memory dedupes.
                        (false, super::RemovalReason::OutOfScope) => {}
                    }
                }
            }
            Some(Id::MergeConfirm) => {
                if let Some(ModalFlow::MergePrompt { issue, pr }) = self.modal_flow.take() {
                    cmds.push(IpcCommand::ConfirmMerge {
                        issue_workspace_key: issue,
                        pr_workspace_key: pr,
                        accept: yes,
                    });
                }
            }
            Some(Id::ActionConfirm) => {
                // Unified destructive-action confirm. Yes →
                // dispatch the queued action against the target
                // stashed at mount time (the sidebar selection may
                // have drifted while the modal was up). No / Esc →
                // drop the stash silently.
                let pending = self.modal_flow.take();
                if yes && let Some(ModalFlow::ActionConfirm { action, targets }) = pending {
                    // A single target keeps the exact per-target path (and
                    // its focused-notice UX); a bulk set iterates the
                    // snapshot, one command per target, with an aggregate
                    // summary (#899).
                    if targets.len() > 1 {
                        cmds.extend(self.dispatch_action_confirmed_bulk(&action, &targets));
                    } else if let Some(target) = targets.first() {
                        cmds.extend(self.dispatch_action_confirmed(&action, target));
                    }
                    self.redraw = true;
                }
            }
            Some(Id::ConflictResolve) => {
                // `g m` resolve prompt (#947). Yes → spawn/attach the
                // agent with the conflict-resolution flow against the
                // stashed workspace. No / Esc → drop the stash silently.
                if let Some(ModalFlow::ConflictResolve { workspace }) = self.modal_flow.take()
                    && yes
                {
                    cmds.extend(self.dispatch_conflict_resolve(&workspace));
                    self.redraw = true;
                }
            }
            Some(Id::EditorRemoveConfirm) => {
                if let Some(ModalFlow::EditorRemoveConfirm { id }) = self.modal_flow.take()
                    && yes
                {
                    self.remove_editor(&id);
                }
            }
            Some(Id::SandboxConfirm) => {
                use crate::sandbox_flow::SandboxStage;
                if let Some(ModalFlow::SandboxOnboarding { mut draft }) = self.modal_flow.take() {
                    match draft.stage {
                        // E2B credential gate: Yes continues the walk; No
                        // abandons onboarding (the flow was already taken
                        // above). GCP has no confirm gate — its key step is an
                        // Input handled in `handle_input_submitted`.
                        SandboxStage::E2bSignIn if yes => {
                            draft.confirm_e2b_signin();
                            self.mount_sandbox_stage(draft);
                        }
                        SandboxStage::E2bSignIn => {}
                        // Final answer: record the toggle either way and
                        // persist the collected config.
                        SandboxStage::AutoConnect => {
                            draft.set_auto_connect(yes);
                            self.finish_sandbox_onboarding(&draft);
                        }
                        _ => {}
                    }
                }
            }
            Some(Id::CleanWorktreesConfirm) => {
                if yes {
                    cmds.push(IpcCommand::CleanWorktrees);
                    // The work happens asynchronously on the daemon
                    // (filesystem walk + git worktree remove per
                    // session) — surface a placeholder notice so the
                    // user knows the click registered. The final
                    // count comes back via
                    // `Event::CleanWorktreesCompleted`.
                    self.flash_info("cleaning worktrees…");
                }
            }
            Some(Id::InspectConfirm) => {
                let target = match self.modal_flow.take() {
                    Some(ModalFlow::InspectConfirm { target }) => Some(target),
                    _ => None,
                };
                if yes && let Some(row) = target {
                    let force = row.has_uncommitted_changes || row.has_unpushed_commits;
                    cmds.push(IpcCommand::DeleteOrphanedWorktree {
                        path: row.path,
                        force,
                    });
                }
            }
            Some(Id::ImportCheckoutConfirm) => {
                let target = match self.modal_flow.take() {
                    Some(ModalFlow::ImportConfirm { target }) => Some(target),
                    _ => None,
                };
                if yes && let Some(row) = target {
                    cmds.push(IpcCommand::ImportLocalCheckout {
                        path: row.path,
                        spawn_agent: None,
                    });
                    self.flash_info("importing checkout…");
                }
            }
            Some(Id::BroadcastConfirm) => {
                // The broadcast would start new agents (#836); yes re-resolves
                // the fixed target set against current session state and runs
                // the fan-out (#1077 — so a target whose agent died under the
                // modal recovers rather than delivering to a dead terminal),
                // no drops the stash and keeps the multi-select for a retry.
                if let Some(ModalFlow::BroadcastConfirm {
                    targets,
                    snippet_key,
                    body,
                }) = self.modal_flow.take()
                {
                    if yes {
                        cmds.extend(self.run_broadcast_confirmed(
                            &targets,
                            snippet_key.as_deref(),
                            &body,
                        ));
                    } else {
                        self.flash_info("broadcast cancelled");
                    }
                }
            }
            Some(Id::BulkSpawnConfirm) => {
                // Bulk `w w` / spawn / shell would start new agents (#899,
                // #836); yes runs the plan snapshotted at mount, no drops
                // it. The steps stay inert until run here, so a cancel
                // records nothing into any terminal's recap.
                if let Some(ModalFlow::BulkSpawnConfirm {
                    steps,
                    summary,
                    follow,
                }) = self.modal_flow.take()
                {
                    if yes {
                        self.sidebar.clear_broadcast_selection();
                        if let Some(target) = follow {
                            self.spawn_follow_to = Some(target);
                        }
                        self.flash_info(summary);
                        self.redraw = true;
                        cmds.extend(self.run_bulk_agent_steps(steps));
                    } else {
                        self.flash_info("cancelled");
                    }
                }
            }
            Some(Id::ClaimedSpawnConfirm) => {
                if let Some(ModalFlow::ClaimedSpawnConfirm { commands }) = self.modal_flow.take() {
                    if yes {
                        self.note_spawn_feedback(&commands);
                        cmds.extend(commands);
                    } else {
                        self.flash_info("agent start cancelled — existing ⚑ claim kept");
                    }
                }
            }
            Some(Id::HelpActionConfirm) => {
                // Action proposed by the Ask Lazybox help agent (#353).
                // Yes → apply it; No / Esc → drop the stash, nothing
                // changes.
                let pending = self.modal_flow.take();
                if yes && let Some(ModalFlow::HelpAction { intent, skill_root }) = pending {
                    self.apply_help_action(intent, skill_root);
                }
            }
            Some(Id::ErrorInboxClearConfirm) => {
                // Yes → wipe the durable Error Inbox; the daemon's empty
                // re-broadcast repaints the inbox still under this confirm.
                // No / Esc → nothing changes. No `ModalFlow` payload: the
                // clear has no target to stash.
                if yes {
                    cmds.push(IpcCommand::ClearErrors);
                }
            }
            _ => {}
        }
        self.drain_queued_daemon_prompts();
        cmds
    }

    /// Apply an allowlisted action the user confirmed (#353). Snippets
    /// are written + hot-reloaded (no restart); config edits are
    /// persisted through the same safe `save_with` path the settings UI
    /// uses, then live-applied where possible. Re-validates config edits
    /// so a value that went stale between propose and confirm can't slip
    /// through. All local — no IPC.
    fn apply_help_action(
        &mut self,
        intent: lazybox_tui_core::help::HelpActionIntent,
        skill_root: Option<std::path::PathBuf>,
    ) {
        use lazybox_tui_core::help::HelpActionIntent;
        match intent {
            HelpActionIntent::AddSnippet {
                key,
                category,
                description,
                body,
            } => {
                let key = key.trim().to_string();
                let snippet = lazybox_config::Snippet {
                    description: description.trim().to_string(),
                    category: category.trim().to_string(),
                    body,
                    skill: None,
                    provider: None,
                    origin: Default::default(),
                };
                match lazybox_config::Snippets::upsert_global_snippet(&key, &snippet) {
                    Ok(_) => {
                        self.apply_snippets(lazybox_config::Snippets::load_for_launch_dir(
                            std::env::current_dir().ok().as_deref(),
                        ));
                        self.flash_info(format!("snippet saved — send it with ]]s{key}"));
                    }
                    Err(e) => self.flash_error(format!("failed to save snippet: {e}")),
                }
            }
            HelpActionIntent::EditConfig { key, value } => {
                match self.validate_config_edit(&key, &value) {
                    Ok(edit) => self.apply_config_edit(edit),
                    Err(msg) => self.flash_error(msg),
                }
            }
            HelpActionIntent::ScaffoldSkill {
                name,
                description,
                body,
            } => {
                if let Err(msg) = self.validate_skill_scaffold(&name, &description, &body) {
                    self.flash_error(msg);
                    return;
                }
                // Write to the exact root resolved and previewed at
                // propose time, not a freshly-resolved one — the
                // sidebar selection may have moved under the confirm.
                let Some(root) = skill_root else {
                    self.flash_error(
                        "a skill scaffolds into a repo on your machine — unavailable for a remote daemon",
                    );
                    return;
                };
                match lazybox_config::scaffold_skill(&root, name.trim(), &description, &body) {
                    Ok(path) => self.flash_info(format!("skill scaffolded — {}", path.display())),
                    Err(e) => self.flash_error(format!("failed to scaffold skill: {e}")),
                }
            }
        }
    }

    /// The repo the `scaffold_skill` action writes into, carrying its
    /// provenance so the confirm preview can name it honestly. The
    /// focused workspace's live worktree is preferred; the directory
    /// lazybox was launched from is the fallback when no focused
    /// workspace has an on-disk worktree. `None` when attached to a
    /// remote daemon — the worktree path is server-side, so there's
    /// nothing local to scaffold into.
    pub(super) fn skill_scaffold_root(&self) -> Option<SkillScaffoldRoot> {
        if self.remote {
            return None;
        }
        if let Some(worktree) = self
            .sidebar
            .selected_workspace()
            .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()))
            .filter(|p| p.is_dir())
        {
            return Some(SkillScaffoldRoot::Worktree(worktree));
        }
        std::env::current_dir()
            .ok()
            .map(SkillScaffoldRoot::LaunchDir)
    }

    /// Boundary validation for a proposed `scaffold_skill` (#799): the
    /// name must be a clean folder id and both description and body must
    /// be present. Re-run at apply so a payload that went stale between
    /// propose and confirm can't slip a bad write through.
    pub(super) fn validate_skill_scaffold(
        &self,
        name: &str,
        description: &str,
        body: &str,
    ) -> Result<(), String> {
        lazybox_config::validate_skill_name(name.trim()).map_err(|e| e.to_string())?;
        if description.trim().is_empty() {
            return Err(
                "the assistant proposed a skill with no description — nothing was written".into(),
            );
        }
        if body.trim().is_empty() {
            return Err(
                "the assistant proposed a skill with an empty body — nothing was written".into(),
            );
        }
        Ok(())
    }

    /// Validate an `edit_config` intent against the allowlist and live
    /// state, canonicalizing the value. This is the security boundary:
    /// only these keys can be set, each value is checked against what's
    /// actually available (a registered theme, an enabled agent, a known
    /// preset), and the returned [`super::ConfigEdit`] carries a `&'static`
    /// key so the apply step can never be steered off the allowlist.
    pub(super) fn validate_config_edit(
        &self,
        key: &str,
        value: &str,
    ) -> Result<super::ConfigEdit, String> {
        let value = value.trim();
        if value.is_empty() {
            return Err(format!("no value given for `{key}`"));
        }
        match key {
            "ui.theme" => {
                // Match case-insensitively but store the theme's exact
                // registered spelling — `set_by_name` is an exact match.
                match crate::theme::list()
                    .iter()
                    .find(|t| t.name.eq_ignore_ascii_case(value))
                {
                    Some(t) => Ok(super::ConfigEdit {
                        key: "ui.theme",
                        value: t.name.to_string(),
                        summary: format!("theme → {}", t.name),
                        needs_restart: false,
                    }),
                    None => Err(format!(
                        "unknown theme \"{value}\" — pick one from the theme picker (t)"
                    )),
                }
            }
            "setup.default_agent" => {
                if self.agents.iter().any(|a| a == value) {
                    Ok(super::ConfigEdit {
                        key: "setup.default_agent",
                        value: value.to_string(),
                        summary: format!("default agent → {value}"),
                        needs_restart: false,
                    })
                } else {
                    Err(format!("\"{value}\" isn't one of your enabled agents"))
                }
            }
            "ui.keymap_preset" => {
                if lazybox_tui_core::action::keymap_preset(value).is_some() {
                    Ok(super::ConfigEdit {
                        key: "ui.keymap_preset",
                        value: value.to_string(),
                        summary: format!("keymap preset → {value}"),
                        needs_restart: true,
                    })
                } else {
                    Err(format!(
                        "unknown keymap preset \"{value}\" — use `default` or `vim`"
                    ))
                }
            }
            other => Err(format!("\"{other}\" isn't an editable config key")),
        }
    }

    /// Persist a validated config edit through `Config::save_with` (the
    /// atomic, 0600, mutex-serialized write the settings UI uses), then
    /// live-apply it where lazybox has a running-component hook. Keys
    /// with no live path (the keymap preset) land after a restart.
    fn apply_config_edit(&mut self, edit: super::ConfigEdit) {
        let value = edit.value.clone();
        let saved = match edit.key {
            "ui.theme" => lazybox_config::Config::save_with(move |c| c.ui.theme = Some(value)),
            "setup.default_agent" => {
                lazybox_config::Config::save_with(move |c| c.setup.default_agent = Some(value))
            }
            "ui.keymap_preset" => {
                lazybox_config::Config::save_with(move |c| c.ui.keymap_preset = Some(value))
            }
            // Unreachable: `edit.key` came from `validate_config_edit`,
            // which only ever emits the arms above.
            _ => return,
        };
        match saved {
            Ok(()) => {
                match edit.key {
                    "ui.theme" => {
                        crate::theme::set_by_name(&edit.value);
                    }
                    "setup.default_agent" => self.set_default_agent(&edit.value),
                    _ => {}
                }
                if edit.needs_restart {
                    self.flash_info(format!("{} — restart to apply", edit.summary));
                } else {
                    self.flash_info(edit.summary);
                }
                self.redraw = true;
            }
            Err(e) => self.flash_error(format!("couldn't save config: {e}")),
        }
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
            RunnerStep::Show { screen, effect } => {
                self.setup.runner = Some(runner);
                // Layer 2: turn the pure Screen into a widget. Loading
                // screens hand back a producer the executor delivers into.
                let (component, result) = crate::realm::setup_screen::render(screen);
                self.mount_setup_modal(component);
                // Layer 3: run the paired effect (if any) against the
                // registered scope sources. Result flows back as
                // `Msg::LoadingResolved` when the Loading modal ticks.
                if let (Some(effect), Some(result)) = (effect, result) {
                    if let Some((_, sources)) = self.setup.inputs.as_ref() {
                        crate::realm::setup_screen::run_effect(
                            effect,
                            sources.clone(),
                            self.setup.detector.clone(),
                            result,
                        );
                    } else {
                        tracing::warn!("handle_runner_step: effect requested but no scope sources");
                    }
                }
            }
            RunnerStep::Finish(outcome) => {
                let sources: Vec<String> = outcome.enabled_providers.iter().cloned().collect();
                let persisted = crate::setup_flow::outcome_to_persisted(&outcome);
                // Persist FIRST (via the installed hook) and only
                // cache the new state when the save actually landed —
                // otherwise the session would act on scopes that
                // evaporate on the next launch while the user saw a
                // success message.
                let save_result = match self.setup.on_complete.as_ref() {
                    Some(hook) => hook(outcome),
                    None => Ok(None),
                };
                self.unmount_setup_modal();
                self.send_cmd(IpcCommand::Subscribe);
                // Kick off an immediate poll so a freshly added repo
                // surfaces its open PRs/issues within seconds instead
                // of waiting for the long-lived 60s loop tick.
                self.send_cmd(IpcCommand::Refresh);
                self.set_focus_attr();
                match save_result {
                    Ok(backed_up) => {
                        // Cache the new persisted state so subsequent
                        // partial flows (Settings → Add a repo) see
                        // the latest scopes, and push the new repo
                        // subscriptions into the sidebar so a header
                        // shows even before polling finds workspaces.
                        self.setup.persisted = Some(persisted);
                        self.refresh_subscribed_projects();
                        if let Some(bak) = backed_up {
                            self.flash_info(format!(
                                "config.yaml was malformed — kept a backup at {}",
                                bak.display(),
                            ));
                        }
                        if !sources.is_empty() {
                            self.show_polling(sources);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("setup save failed: {e:#}");
                        self.flash(
                            format!("settings NOT saved: {e}"),
                            crate::realm::components::footer::NoticeSeverity::Permanent,
                        );
                    }
                }
                // First-run users land here after completing the
                // wizard — surface the feature tour now (no-op when
                // it's already been seen, or when this Finish came
                // from a partial Settings flow on a returning user).
                self.maybe_mount_tour();
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
        // Unmount whatever's on top — setup is a one-modal-at-a-time
        // flow; the same Id::Setup gets re-mounted for each wizard
        // step.
        if let Some(top) = self.modal_stack.last().cloned() {
            let _ = self.app.umount(&top);
            self.modal_stack.pop();
        }
        self.mount_modal_boxed(Id::Setup, component);
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

fn format_diff_review_prompt(
    comments: &[crate::realm::components::diff_review::DiffReviewComment],
) -> String {
    let mut prompt = String::from(
        "Local diff review\n\nPlease address each inline comment below in the current worktree. \
         Inspect the current diff before editing, preserve unrelated changes, and report how you \
         resolved each item.\n",
    );
    for (index, comment) in comments.iter().enumerate() {
        let location = match (comment.new_line, comment.old_line) {
            (Some(line), _) => format!("{}:{line}", comment.path),
            (None, Some(line)) => format!("{} (old line {line})", comment.path),
            (None, None) => comment.path.clone(),
        };
        prompt.push_str(&format!(
            "\n{}. {location}\n   Hunk: {}\n   Referenced: {}\n   Context:\n",
            index + 1,
            comment.hunk_header,
            comment.referenced_line
        ));
        for line in &comment.context {
            prompt.push_str("       ");
            prompt.push_str(line);
            prompt.push('\n');
        }
        prompt.push_str("   Review comment: ");
        prompt.push_str(&comment.body);
        prompt.push('\n');
    }
    prompt
}

/// Encode a snippet body for a **non-agent** terminal's PTY (a plain
/// shell). Single-line bodies are raw text plus a trailing `\r` (Enter
/// / submit). Multi-line bodies are wrapped in a bracketed-paste pair
/// (`ESC[200~ … ESC[201~`) with embedded newlines rewritten to `\r`,
/// and the submit `\r` placed *outside* the closing marker — so the
/// encoded bytes always end in a submit `\r` outside any paste wrapper.
///
/// Without the wrapper, a multi-line burst trips a shell's paste
/// auto-detection: the trailing `\r` lands inside the paste window and
/// is buffered as a literal newline rather than submitting. Bracketing
/// the body makes it one paste, so the `\r` after `ESC[201~` reads as a
/// clean submit.
///
/// Agent terminals (Claude / Codex / Cursor) do NOT use this. For
/// free-text delivery they use `Command::InjectPrompt`; snippets use
/// `Command::DeliverSnippet`, whose daemon-side agent branch applies the
/// same gated paste + submit protocol (#246).
pub(super) fn encode_snippet_for_pty(body: &str) -> Vec<u8> {
    let body = lazybox_tui_core::agents::trim_leading_blank_lines(body);
    if !body.contains('\n') {
        let mut bytes = Vec::with_capacity(body.len() + 1);
        bytes.extend_from_slice(body.as_bytes());
        bytes.push(b'\r');
        return bytes;
    }
    let mut out = Vec::with_capacity(body.len() + 16);
    out.extend_from_slice(b"\x1b[200~");
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push(b'\r');
        }
        out.extend_from_slice(line.as_bytes());
    }
    out.extend_from_slice(b"\x1b[201~");
    out.push(b'\r');
    out
}

/// Expand a leading `~/` in a scan-root path to the user's home
/// directory. The daemon expands the same way at scan time, so a stored
/// `~/` root stays readable in YAML. Non-`~/` paths pass through
/// unchanged (a relative path stays relative here).
fn expand_scan_root(p: &std::path::Path) -> std::path::PathBuf {
    if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix("~/"))
        && let Some(home) = super::home_dir()
    {
        return home.join(rest);
    }
    p.to_path_buf()
}

/// The absolute, tilde-expanded form of a scan root — the comparison key
/// for existence checks and dedup, so `~/code`, `/home/me/code`, and a
/// CWD-relative `code` collapse to one identity. Lexical only (no
/// filesystem access); a relative path is pinned to the current dir.
fn resolve_scan_root(p: &std::path::Path) -> std::path::PathBuf {
    let expanded = expand_scan_root(p);
    if expanded.is_absolute() {
        expanded
    } else {
        std::path::absolute(&expanded).unwrap_or(expanded)
    }
}
