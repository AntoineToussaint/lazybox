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
use tuirealm::terminal::TerminalAdapter;

impl<T: TerminalAdapter> Model<T> {
    pub(super) fn dispatch_diff_review(
        &mut self,
        workspace_key: lazybox_core::WorkspaceKey,
        comments: Vec<crate::realm::components::diff_review::DiffReviewComment>,
    ) -> Vec<IpcCommand> {
        let session_key: lazybox_core::SessionKey = workspace_key.as_str().into();
        let active = self
            .terminals
            .active_session()
            .filter(|active| active.as_str() == workspace_key.as_str())
            .and_then(|_| self.terminals.active_terminal_id())
            .filter(|terminal| self.terminals.terminal_is_agent(*terminal));
        let target = active.or_else(|| {
            let targets = self.sidebar.running_work_targets(&session_key);
            (targets.len() == 1).then(|| targets[0].terminal_id)
        });
        let Some(target) = target else {
            let count = self.sidebar.running_work_targets(&session_key).len();
            if count == 0 {
                self.flash_hint("review not sent — this workspace has no running agent");
            } else {
                self.flash_hint(
                    "review not sent — several agents are running; focus the target and retry",
                );
            }
            return Vec::new();
        };

        let prompt = format_diff_review_prompt(&comments);
        let mut commands = Vec::new();
        self.deliver_prompt(
            target,
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
            });
        }
    }

    /// Fan the composed broadcast body out to every stashed target. A
    /// snippet-seeded body uses `DeliverSnippet`, leaving terminal-kind
    /// handling and histories behind daemon confirmation; free text uses
    /// the existing agent/shell paths. A workspace with no running session
    /// is skipped and named in the summary notice.
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
        let snippet_key = draft.snippet_key.clone();
        let snippet_category = snippet_key
            .as_deref()
            .and_then(|key| self.snippets.get(key))
            .map(|snippet| snippet.category.clone())
            .unwrap_or_default();
        let mut cmds = Vec::new();
        let mut sent = 0usize;
        let mut skipped: Vec<String> = Vec::new();
        for key in &draft.targets {
            match self.sidebar.broadcast_terminal(key) {
                Some((terminal_id, is_agent)) => {
                    if let Some(snippet_key) = &snippet_key {
                        cmds.push(IpcCommand::DeliverSnippet {
                            terminal_id,
                            snippet_key: snippet_key.clone(),
                            category: snippet_category.clone(),
                            body: body.to_string(),
                        });
                    } else {
                        self.deliver_prompt(
                            terminal_id,
                            is_agent,
                            body,
                            lazybox_ipc::PromptSource::Typed,
                            &mut cmds,
                        );
                    }
                    sent += 1;
                }
                None => skipped.push(
                    self.sidebar
                        .workspace_by_key(key)
                        .map(|w| w.name.clone())
                        .unwrap_or_else(|| key.to_string()),
                ),
            }
        }
        if sent > 0 {
            self.sidebar.clear_broadcast_selection();
        }
        let summary = match (sent, skipped.len()) {
            (0, _) => "broadcast queued for nobody — no target has a running session".to_string(),
            (n, 0) => format!("queued for {n} workspace{}", if n == 1 { "" } else { "s" }),
            (n, _) => format!(
                "queued for {n} workspace{} ({} skipped: no session — {})",
                if n == 1 { "" } else { "s" },
                skipped.len(),
                skipped.join(", "),
            ),
        };
        self.flash_info(summary);
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
                        tracing::info!(
                            workspace_name = %name,
                            project_key = %project_key,
                            ?spawn_agent,
                            "creating new pre-PR workspace under project",
                        );
                        cmds.push(IpcCommand::CreateWorkspace {
                            name,
                            project_key,
                            spawn_agent,
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

    /// Route a Choice modal pick through the pure tui-core resolver,
    /// then apply its typed outcome.
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    /// Editor / Settings / runner outcomes may still emit commands
    /// internally through their effect helpers; directly-visible IPC
    /// commands land in the Vec.
    pub fn handle_choice_picked(&mut self, picks: Vec<ChoicePayload>) -> Vec<IpcCommand> {
        let cmds = self.choice_picked_inner(picks);
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
                if yes && let Some(ModalFlow::ActionConfirm { action, target }) = pending {
                    cmds.extend(self.dispatch_action_confirmed(&action, &target));
                    self.redraw = true;
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
            Some(Id::HelpActionConfirm) => {
                // Action proposed by the Ask Lazybox help agent (#353).
                // Yes → apply it; No / Esc → drop the stash, nothing
                // changes.
                let pending = self.modal_flow.take();
                if yes && let Some(ModalFlow::HelpAction { intent }) = pending {
                    self.apply_help_action(intent);
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
    fn apply_help_action(&mut self, intent: lazybox_tui_core::help::HelpActionIntent) {
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
        }
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
