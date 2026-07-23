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

use super::{Id, Model, Msg, dismissed_update_key};
use crate::realm::UserEvent;
use lazybox_ipc::Command as IpcCommand;
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
        // Note editor (issue #458): persist whatever's in the buffer,
        // including empty — an empty submission clears the note. Unlike
        // Reply, no non-empty guard, and no upstream side effects.
        if matches!(top, Some(Id::NoteEditor)) {
            let mut cmds = Vec::new();
            if let Some(session_key) = self.pending_note.take() {
                let note = body.trim_end().to_string();
                cmds.push(IpcCommand::SetWorkspaceNote { session_key, note });
                self.flash_info("Note saved.");
            }
            self.drain_queued_daemon_prompts();
            return cmds;
        }
        let mut cmds = Vec::new();
        let target = self.pending_reply.take();
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

    /// Fan the composed broadcast body out to every stashed target:
    /// a running agent terminal gets the settle-gated `InjectPrompt`
    /// (+ a `RecordUserMessage` so its recap line updates, #246-safe);
    /// a plain shell gets the encoded direct write; a workspace with
    /// no running session is skipped and named in the summary notice.
    /// The snippet MRU counts the bulk send once, and the sidebar
    /// selection clears only after something was actually delivered.
    fn dispatch_broadcast(&mut self, body: &str) -> Vec<IpcCommand> {
        let Some(draft) = self.pending_broadcast.take() else {
            return Vec::new();
        };
        // The compose step may leave the snippet's pre-fill padding (or
        // a stray trailing newline) behind; a trailing `\n` would reach
        // the agent as a soft line break, not content.
        let body = body.trim_end();
        if body.is_empty() {
            return Vec::new();
        }
        let mut cmds = Vec::new();
        let mut sent = 0usize;
        let mut skipped: Vec<String> = Vec::new();
        for key in &draft.targets {
            match self.sidebar.broadcast_terminal(key) {
                Some((terminal_id, true)) => {
                    // Same recap + inject pair as the single-target
                    // snippet path (see the SnippetPicker arm above).
                    let mut recap = body.as_bytes().to_vec();
                    recap.push(b'\r');
                    if let Some(message) = self.terminals.record_pty_write(terminal_id, &recap) {
                        cmds.push(IpcCommand::RecordUserMessage {
                            terminal_id,
                            message,
                        });
                    }
                    cmds.push(IpcCommand::InjectPrompt {
                        terminal_id,
                        prompt: body.to_string(),
                        fallback_spawn: None,
                        submit: true,
                    });
                    sent += 1;
                }
                Some((terminal_id, false)) => {
                    cmds.push(IpcCommand::Write {
                        terminal_id,
                        bytes: encode_snippet_for_pty(body),
                    });
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
            if let Some(snippet_key) = draft.snippet_key {
                self.record_recent_snippet(snippet_key);
            }
            self.sidebar.clear_broadcast_selection();
        }
        let summary = match (sent, skipped.len()) {
            (0, _) => "broadcast sent to nobody — no target has a running session".to_string(),
            (n, 0) => format!("sent to {n} workspace{}", if n == 1 { "" } else { "s" }),
            (n, _) => format!(
                "sent to {n} workspace{} ({} skipped: no session — {})",
                if n == 1 { "" } else { "s" },
                skipped.len(),
                skipped.join(", "),
            ),
        };
        self.flash_info(summary);
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
                let project_key = self.pending_new_workspace_project.take();
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
                    self.pending_focus_project_name = Some(name.clone());
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

    /// A question submitted from the `HelpAsk` modal (#302). Records
    /// the turn in the shared conversation and routes it to the help
    /// agent: the first question starts a headless structured run
    /// with the generated catalog + docs context as its opening
    /// message; follow-ups ride the same run so the context stays
    /// prompt-cached. Questions racing the run start are queued and
    /// flushed by the `AgentRunStarted` handler.
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    pub fn handle_help_asked(&mut self, question: String) -> Vec<IpcCommand> {
        use lazybox_ipc::{AgentInputMessage, AgentRuntimeMode};
        use lazybox_tui_core::help::{HELP_AGENT_PREFERENCE, HELP_SESSION_KEY, select_help_agent};

        let question = question.trim().to_string();
        if question.is_empty() {
            return Vec::new();
        }
        {
            let mut convo = self.help_convo_mut();
            convo.notice = None;
            convo
                .turns
                .push(crate::realm::components::help_ask::HelpTurn {
                    question: question.clone(),
                    ..Default::default()
                });
        }
        self.redraw = true;
        let Some(help_agent) = select_help_agent(&self.agents, Some(self.sidebar.default_agent()))
        else {
            let mut convo = self.help_convo_mut();
            convo.notice = Some(format!(
                "the help assistant needs a structured agent ({}) enabled — \
showing keybinding search only",
                HELP_AGENT_PREFERENCE.join(" or ")
            ));
            if let Some(turn) = convo.streaming_turn_mut() {
                turn.done = true;
            }
            return Vec::new();
        };
        if let Some(run_id) = self.help_run {
            return vec![IpcCommand::SendAgentInput {
                run_id,
                message: AgentInputMessage {
                    text: Some(question),
                    json: None,
                },
            }];
        }
        if self.help_run_starting {
            self.help_pending_questions.push(question);
            return Vec::new();
        }
        self.help_run_starting = true;
        let context = lazybox_tui_core::help::agent_context(
            &self.catalog,
            self.ui_defaults.terminal_escape_char,
        );
        vec![IpcCommand::StartAgentRun {
            session_key: lazybox_core::SessionKey::new(HELP_SESSION_KEY),
            session_id: None,
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
        }]
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
        let cmds = self.choice_picked_inner(picks);
        // The inner handler has many early-return arms; drain queued
        // daemon prompts HERE so every one of them gets the "modal
        // stack may have just emptied" treatment. (No-op while any
        // modal — including one the pick itself mounted — is up.)
        self.drain_queued_daemon_prompts();
        cmds
    }

    fn choice_picked_inner(&mut self, picks: Vec<usize>) -> Vec<IpcCommand> {
        let mut cmds = Vec::new();
        // Broadcast snippet step — the pick doesn't send anything yet:
        // it seeds the compose textarea with the snippet's body. An
        // empty pick is the picker's `Ctrl-F` "free text only" escape,
        // which skips straight to an empty compose buffer.
        if matches!(self.modal_stack.last(), Some(Id::BroadcastSnippet)) {
            let key = picks
                .first()
                .and_then(|i| self.snippet_choices.get(*i).cloned());
            self.snippet_choices.clear();
            self.pop_modal();
            if self.pending_broadcast.is_none() {
                return cmds;
            }
            let body = key
                .as_ref()
                .and_then(|k| self.snippets.get(k))
                .map(|s| s.body.clone());
            if let Some(draft) = self.pending_broadcast.as_mut() {
                // Only remember the key when it resolved to a body —
                // the MRU must not record a snippet that wasn't sent.
                draft.snippet_key = key.filter(|_| body.is_some());
            }
            self.mount_broadcast_textarea(body);
            return cmds;
        }
        // Snippet picker — pick → send the snippet body to the active
        // terminal AND submit it in one shot. The "expand AND submit"
        // combo is the whole point of the feature: the user gets the
        // prompt to the agent's input in a single keystroke chord, no
        // intermediate "review then send" step. How the submit lands
        // depends on the terminal — see the agent vs shell split below.
        if matches!(self.modal_stack.last(), Some(Id::SnippetPicker)) {
            let key = picks
                .first()
                .and_then(|i| self.snippet_choices.get(*i).cloned());
            self.snippet_choices.clear();
            self.pop_modal();
            let Some(key) = key else {
                return cmds;
            };
            let Some(snippet) = self.snippets.get(&key) else {
                // Picker resolved to a key the live snippet set
                // doesn't recognise — possible only if the
                // collection was swapped between mount and submit
                // (no in-process path does that today).
                tracing::warn!(
                    "snippet picker: picked key {key:?} but no entry in snippets — stale modal?",
                );
                return cmds;
            };
            let Some(terminal_id) = self.terminals.active_terminal_id() else {
                self.flash_info("no active terminal — open a session first");
                return cmds;
            };
            // An agent terminal gets the daemon's settle-gated inject
            // path — the SAME one `w w` uses: the body is pasted, then
            // Enter is sent as a separate keystroke once the paste's
            // repaint quiesces. A single write with a trailing `\r`
            // (`encode_snippet_for_pty`) is not enough: Claude batches
            // the burst as a paste and swallows the `\r` as a soft
            // newline, so the snippet expands but never submits (#246).
            // A plain shell has no paste debounce, so the direct
            // `body + \r` write submits cleanly there.
            if self.terminals.terminal_is_agent(terminal_id) {
                // Mirror the snippet into the recap tracker — the daemon
                // performs the actual PTY write, so without this the
                // pinned "you ▸ …" line would keep showing the previous
                // message. Feed the raw body + a submit `\r` (embedded
                // newlines stay `\n`, i.e. soft breaks) so the whole
                // body commits as one recap message.
                let mut recap = snippet.body.clone().into_bytes();
                recap.push(b'\r');
                if let Some(message) = self.terminals.record_pty_write(terminal_id, &recap) {
                    cmds.push(IpcCommand::RecordUserMessage {
                        terminal_id,
                        message,
                    });
                }
                cmds.push(IpcCommand::InjectPrompt {
                    terminal_id,
                    prompt: snippet.body.clone(),
                    fallback_spawn: None,
                    submit: true,
                });
            } else {
                let bytes = encode_snippet_for_pty(&snippet.body);
                cmds.push(IpcCommand::Write { terminal_id, bytes });
            }
            // Only reached once the snippet has actually been dispatched
            // (agent inject or shell write) — so the MRU tracks sent
            // snippets, not abandoned ones. Ends the `snippet` borrow of
            // `self.snippets` above (NLL) before this `&mut self` call.
            self.record_recent_snippet(key.clone());
            self.flash_info(format!("sent snippet ]{key}"));
            return cmds;
        }
        // Jump picker (Id::JumpPicker) — pick → land the cursor on the
        // chosen workspace (and follow it into its terminal when it has
        // one). Empty / Esc pick drops the stash without moving.
        if matches!(self.modal_stack.last(), Some(Id::JumpPicker)) {
            let target = picks
                .first()
                .and_then(|i| self.jump_choices.get(*i).cloned());
            self.jump_choices.clear();
            self.pop_modal();
            if let Some(key) = target {
                self.jump_to_workspace_key(&key);
            }
            return cmds;
        }
        // Theme picker — the highlighted palette is already live (the
        // on_highlight preview applied it as the cursor moved). Pick →
        // keep it and persist to `ui.theme`; the prev-theme stash is
        // dropped so a later Esc on another modal can't revert it.
        if matches!(self.modal_stack.last(), Some(Id::ThemePicker)) {
            let name = picks
                .first()
                .and_then(|i| self.theme_choices.get(*i).cloned());
            self.theme_choices.clear();
            self.theme_picker_prev = None;
            self.pop_modal();
            if let Some(name) = name {
                crate::theme::set_by_name(&name);
                match lazybox_config::Config::save_with(|c| c.ui.theme = Some(name.clone())) {
                    Ok(()) => self.flash_info(format!("theme: {name}")),
                    Err(e) => self.flash_info(format!("couldn't save theme: {e}")),
                }
                self.redraw = true;
            }
            return cmds;
        }
        // Default-agent picker — pick → persist `setup.default_agent`,
        // update both panes live (no restart), then chain into the
        // default-model picker when the agent declares tiers. Empty /
        // Esc drops the stash without changing anything.
        if matches!(self.modal_stack.last(), Some(Id::DefaultAgentPicker)) {
            let agent = picks
                .first()
                .and_then(|i| self.default_agent_choices.get(*i).cloned());
            self.default_agent_choices.clear();
            self.pop_modal();
            if let Some(agent) = agent {
                // Persist first; only apply live once the write lands so
                // a save failure never leaves the panes ahead of disk.
                match lazybox_config::Config::save_with(|c| {
                    c.setup.default_agent = Some(agent.clone());
                }) {
                    Ok(()) => {
                        self.set_default_agent(&agent);
                        self.flash_info(format!("default agent: {agent}"));
                        self.redraw = true;
                        self.mount_default_model_picker(&agent);
                    }
                    Err(e) => self.flash_info(format!("couldn't save config: {e}")),
                }
            }
            return cmds;
        }
        // Default-model picker (second step of the default-agent flow)
        // — pick → persist `agents.<id>.models.default` so bare spawns
        // use that tier; the `None` row unpins it (agent's own default
        // model). Empty / Esc keeps the current tier.
        if matches!(self.modal_stack.last(), Some(Id::DefaultModelPicker)) {
            let alias = picks
                .first()
                .and_then(|i| self.default_model_choices.get(*i).cloned());
            self.default_model_choices.clear();
            let agent = self.default_model_agent.take();
            self.pop_modal();
            if let (Some(agent), Some(alias)) = (agent, alias) {
                match lazybox_config::Config::save_with(|c| {
                    // Unpinning an agent with no YAML block is already
                    // a no-op — skip the insert so a dead `agents.<id>`
                    // stanza isn't serialized.
                    if alias.is_some() || c.agents.contains_key(&agent) {
                        c.agents.entry(agent.clone()).or_default().models.default = alias.clone();
                    }
                }) {
                    Ok(()) => {
                        // Mirror the write into the in-memory menu so
                        // the Settings row badge and the next picker
                        // open reflect it without a restart. Re-derive
                        // the merged menu from disk rather than storing
                        // the raw pick: unpinning falls back to the
                        // built-in default (Opus for Claude), and that
                        // fallback must apply live too — a bare spawn
                        // never runs without an explicit model in
                        // between.
                        let merged = lazybox_config::Config::load()
                            .unwrap_or_default()
                            .agent_models(&agent);
                        let label = merged
                            .default
                            .as_deref()
                            .and_then(|a| merged.tier(a))
                            .map(|t| t.label.clone());
                        self.agent_models.insert(agent.clone(), merged);
                        self.flash_info(match label {
                            Some(label) => format!("default model: {label}"),
                            None => "default model: agent default".to_string(),
                        });
                        self.redraw = true;
                    }
                    Err(e) => self.flash_info(format!("couldn't save config: {e}")),
                }
            }
            return cmds;
        }
        // Sidebar right-click context menu. Pick → route through
        // `dispatch_action`, the same single fan-out the keyboard
        // shortcut uses. That keeps the destructive gate intact:
        // MergePr / Archive mount the unified ActionConfirm modal
        // instead of firing straight at the daemon (a mis-click on
        // "merge" must not merge a PR with no confirmation). Empty
        // pick (Esc) clears the stash silently.
        if matches!(self.modal_stack.last(), Some(Id::SidebarContext)) {
            let stash = self.pending_sidebar_context.take();
            self.pop_modal();
            if let (Some((session_key, actions)), Some(&idx)) = (stash.as_ref(), picks.first())
                && let Some(action) = actions.get(idx).cloned()
            {
                // Re-aim the sidebar at the row the menu was raised
                // over. The right-click hit-test already moved the
                // selection there, but a daemon event (re-sort,
                // removal) could have shifted the cursor while the
                // menu was up — `dispatch_action` reads the live
                // selection, so pin it back to the menu's row first.
                if self.sidebar.focus_workspace_key(session_key) {
                    cmds.extend(self.dispatch_action(&action));
                } else {
                    self.flash_info("workspace is gone — action dropped");
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
                cmds.push(IpcCommand::AdoptSessions {
                    source_workspace_key: source_key.clone(),
                    target_workspace_key: target_key.clone(),
                });
                self.flash_info(format!("adopted sessions: {source_key} → {target_key}"));
            }
            return cmds;
        }
        // Start-agent project picker (Id::StartAgentProject) — pick a
        // project, then funnel into the new-workspace name input. That
        // input's submit auto-spawns the default agent, so this is the
        // first leg of "create workspace + start agent". Empty / Esc
        // pick drops the stash without advancing.
        if matches!(self.modal_stack.last(), Some(Id::StartAgentProject)) {
            let project = picks
                .first()
                .and_then(|i| self.start_agent_project_choices.get(*i).cloned());
            self.start_agent_project_choices.clear();
            self.pop_modal();
            if let Some(project_key) = project {
                self.mount_new_workspace_input(project_key);
            }
            return cmds;
        }
        // New-workspace repo picker (Id::NewWorkspaceRepo) — the
        // `x p` entry point. A pick that indexes into the repo
        // list funnels into the new-workspace name input under that
        // repo; the trailing escape-hatch row (index == list length)
        // falls back to creating a new local project. Empty / Esc
        // pick drops the stash without advancing.
        if matches!(self.modal_stack.last(), Some(Id::NewWorkspaceRepo)) {
            // `.get` is `None` exactly at the trailing escape-hatch row
            // (index == list length), so a picked repo → name input and
            // the escape hatch → new-project input. An empty pick (Esc)
            // just closes the picker.
            let picked = picks
                .first()
                .map(|i| self.new_workspace_repo_choices.get(*i).cloned());
            self.new_workspace_repo_choices.clear();
            self.pop_modal();
            match picked {
                Some(Some(project_key)) => self.mount_new_workspace_input(project_key),
                Some(None) => self.mount_new_project_input(),
                None => {}
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
                let count = logins.len();
                cmds.push(IpcCommand::RequestReviewers {
                    workspace_key,
                    logins,
                });
                self.flash_info(format!("requested {count} reviewer(s)"));
            }
            return cmds;
        }
        // Automation-policies menu (Id::PolicyPicker, issue #363) —
        // single-pick. Resolve the picked row's `PolicyToggle` against
        // the *live* workspace so the toggle acts on current state, then
        // dispatch the matching command and close. `Info` rows are
        // read-only — re-inform and close.
        if matches!(self.modal_stack.last(), Some(Id::PolicyPicker)) {
            use crate::realm::model::modals::PolicyToggle;
            let toggle = picks
                .first()
                .and_then(|i| self.policy_choices.get(*i).cloned());
            let workspace_key = self.pending_policy_workspace.take();
            self.policy_choices.clear();
            self.pop_modal();
            let (Some(toggle), Some(workspace_key)) = (toggle, workspace_key) else {
                return cmds;
            };
            let session_key = lazybox_core::SessionKey::from(&workspace_key);
            let ws = self
                .sidebar
                .workspace_iter()
                .find(|(k, _)| k.as_str() == workspace_key.as_str())
                .map(|(_, w)| w);
            let Some(ws) = ws else {
                return cmds;
            };
            match toggle {
                PolicyToggle::MergeOnGreen => {
                    let enabled = !ws.auto_merge_on_green;
                    cmds.push(IpcCommand::SetAutoMergeOnGreen {
                        session_key,
                        enabled,
                    });
                    self.flash_info(if enabled {
                        "merge on green: armed"
                    } else {
                        "merge on green: off"
                    });
                }
                PolicyToggle::AutoFix(kind) => {
                    // Label-agnostic cycle (Default → Disarm → Arm): the
                    // next state depends only on the current arm, so the
                    // daemon's authoritative opt-out set — which this
                    // client may not share in remote mode — never changes
                    // the outcome.
                    let next = lazybox_core::toggled_arm(ws.policies.arm(kind));
                    cmds.push(IpcCommand::SetAutoFixPolicy {
                        session_key,
                        kind,
                        arm: next,
                    });
                    let name = match kind {
                        lazybox_core::AutoFixKind::CiFailure => "auto-fix CI",
                        lazybox_core::AutoFixKind::MergeConflict => "auto-fix conflict",
                    };
                    let state = match next {
                        lazybox_core::PolicyArm::Default => "follows config",
                        lazybox_core::PolicyArm::Arm => "armed",
                        lazybox_core::PolicyArm::Disarm => "disarmed",
                    };
                    self.flash_info(format!("{name}: {state}"));
                }
                PolicyToggle::Info(msg) => {
                    self.flash_info(msg);
                }
            }
            return cmds;
        }
        // `w` multi-agent chooser (Id::WorkAgentPicker, #418) — pick →
        // replay the same work spawn `w` would have queued, targeted at
        // the chosen running agent. The Msg::ChoicePicked flush then
        // rewrites it to an inject into that agent's terminal. Empty /
        // Esc pick drops the stash without spawning anything.
        if matches!(self.modal_stack.last(), Some(Id::WorkAgentPicker)) {
            let stash = self.pending_work_picker.take();
            self.pop_modal();
            if let (Some(picker), Some(&idx)) = (stash, picks.first())
                && let Some(agent) = picker.agents.get(idx).cloned()
            {
                self.push_work_spawn(&agent, picker.session_id, picker.model_alias, &mut cmds);
            }
            return cmds;
        }
        // Snooze duration picker (Id::SnoozeDuration) — single-pick.
        // Translate the chosen index into a snooze deadline via the
        // stashed `snooze_choices`. Empty / Esc dismisses without
        // snoozing.
        if matches!(self.modal_stack.last(), Some(Id::SnoozeDuration)) {
            let duration = picks
                .first()
                .and_then(|i| self.snooze_choices.get(*i).copied());
            let workspace_key = self.pending_snooze_workspace.take();
            self.snooze_choices.clear();
            self.pop_modal();
            if let (Some(session_key), Some(duration)) = (workspace_key, duration) {
                let until = chrono::Utc::now()
                    + chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::hours(4));
                cmds.push(IpcCommand::Snooze { session_key, until });
                let mins = duration.as_secs() / 60;
                let label = if mins < 60 {
                    format!("{mins}m")
                } else if mins < 60 * 24 {
                    format!("{}h", mins / 60)
                } else {
                    format!("{}d", mins / 60 / 24)
                };
                self.flash_info(format!("snoozed for {label}"));
            }
            return cmds;
        }
        // Labels picker (Id::ManageLabels) — picker is pre-checked
        // with the currently-applied labels, so submitting the
        // current selection is the *full desired set*. Empty pick
        // is meaningful ("clear all labels") — don't drop it.
        if matches!(self.modal_stack.last(), Some(Id::ManageLabels)) {
            let names: Vec<String> = picks
                .iter()
                .filter_map(|i| self.labels_choices.get(*i).cloned())
                .collect();
            self.labels_choices.clear();
            self.pop_modal();
            if let Some(workspace_key) = self.pending_labels_request.take() {
                let count = names.len();
                let msg = if count == 0 {
                    "cleared labels".to_string()
                } else {
                    format!("set labels ({count})")
                };
                cmds.push(IpcCommand::SetLabels {
                    workspace_key,
                    names,
                });
                self.flash_info(msg);
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
                self.flash_info(msg);
            }
            return cmds;
        }
        // Filter menu (Id::FilterMenu) — picker is pre-checked with
        // the active filters, so the submitted selection IS the new
        // full set. An empty pick is meaningful ("clear all filters").
        if matches!(self.modal_stack.last(), Some(Id::FilterMenu)) {
            let filters: Vec<crate::components::sidebar::Filter> = picks
                .iter()
                .filter_map(|i| self.filter_choices.get(*i).copied())
                .collect();
            self.filter_choices.clear();
            self.pop_modal();
            let count = filters.len();
            self.sidebar.set_filters(filters);
            if count == 0 {
                self.flash_info("filters cleared");
            } else {
                self.flash_info(format!("{count} filter(s) active"));
            }
            return cmds;
        }
        // Worktree inspector (Id::InspectList) — pick a row, then
        // either fire the bulk shortcut or mount a per-row confirm.
        if matches!(self.modal_stack.last(), Some(Id::InspectList)) {
            // Drop the picker first so the confirm modal lands on
            // top of a clean stack.
            self.pop_modal();
            let Some(&idx) = picks.first() else {
                self.pending_inspect_rows.clear();
                return cmds;
            };
            let rows = std::mem::take(&mut self.pending_inspect_rows);
            // Rebuild the same logical index space the picker used:
            // sentinel at slot 0 when any safe rows exist, real
            // rows after that. Picker indices map 1:1.
            let safe_first = rows
                .iter()
                .filter(|r| !r.reasons.is_empty() && r.is_safe_to_delete)
                .count()
                > 0;
            if safe_first && idx == 0 {
                // Bulk shortcut — dispatch a delete per safe row,
                // skip the rest. Daemon re-checks safety per call;
                // a row whose state went stale (new uncommitted
                // change since inspection) gets a "no, refused"
                // event back and the modal will refresh.
                for row in &rows {
                    if !row.reasons.is_empty() && row.is_safe_to_delete {
                        cmds.push(IpcCommand::DeleteOrphanedWorktree {
                            path: row.path.clone(),
                            force: false,
                        });
                    }
                }
                let n = cmds.len();
                self.flash_info(format!("deleting {n} clearly-safe worktrees…"));
                return cmds;
            }
            let row_idx = if safe_first { idx - 1 } else { idx };
            if let Some(row) = rows.get(row_idx).cloned() {
                self.mount_inspect_confirm(row);
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
                    model_alias: None,
                    session_key: workspace_key.clone(),
                    session_id: None,
                    kind: lazybox_ipc::TerminalKind::Shell,
                    cwd: None,
                    initial_prompt: None,
                    on_main: false,
                });
                self.flash_info(format!(
                    "Provisioning worktree for {workspace_key} — opening in {} when ready…",
                    editor.display
                ));
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
        if self.modal_stack.last() == Some(&Id::Update) {
            self.pop_modal();
            if let Some(target) = self.pending_update_target.take() {
                if let Some(store) = self.update_store.take() {
                    let key = dismissed_update_key(&target);
                    let result_tx = self.update_dismissal_tx.clone();
                    match std::thread::Builder::new()
                        .name("update-dismissal".into())
                        .spawn(move || {
                            let result = store.set_kv(&key, "1").map_err(|error| error.to_string());
                            let _ = result_tx.send(result);
                        }) {
                        Ok(_worker) => self.update_dismissals_pending += 1,
                        Err(error) => {
                            self.flash_error(format!(
                                "could not remember update dismissal; it may reappear next launch: \
                                 {error}"
                            ));
                        }
                    }
                } else {
                    self.flash_error(
                        "could not remember update dismissal; local state is unavailable",
                    );
                }
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
        let mut cmds: Vec<IpcCommand> = Vec::new();
        match top {
            Some(Id::RemoveOutOfScope) => {
                self.active_removal_prompt = None;
            }
            Some(Id::MergeConfirm) => {
                // Esc on the merge modal = "dismiss for now, I'll
                // decide later." Pre-fix this sent
                // `ConfirmMerge { accept: false }`, which pinned the
                // issue in the daemon's `rejected_merge` for the
                // session and the user never saw the prompt again
                // until restart. Now: just close the modal. The
                // daemon's `prompted_merge` re-fires after
                // `MERGE_REPROMPT_AFTER` (5 min) so the prompt
                // self-heals; an explicit N (via `handle_confirmed`
                // below) is the only path that pins as rejected.
                self.active_merge_prompt = None;
            }
            Some(Id::ActionConfirm) => {
                // Esc = cancel destructive action; drop the
                // queued Action without firing.
                self.pending_action_confirm = None;
            }
            Some(Id::InspectList) => {
                // Picker closed without a pick — release the cached
                // rows so they don't bleed into a later inspector run
                // with stale paths.
                self.pending_inspect_rows.clear();
            }
            Some(Id::InspectConfirm) => {
                self.pending_inspect_target = None;
            }
            Some(Id::HelpActionConfirm) => {
                // Esc = decline the proposed action; drop the stash,
                // change nothing (#353).
                self.pending_help_action = None;
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
            Some(Id::ManageLabels) => {
                self.pending_labels_request = None;
                self.labels_choices.clear();
            }
            Some(Id::SnoozeDuration) => {
                self.pending_snooze_workspace = None;
                self.snooze_choices.clear();
            }
            Some(Id::WorkAgentPicker) => {
                self.pending_work_picker = None;
            }
            Some(Id::PolicyPicker) => {
                self.pending_policy_workspace = None;
                self.policy_choices.clear();
            }
            Some(Id::FilterMenu) => {
                // Esc = leave the active filters untouched.
                self.filter_choices.clear();
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
            Some(Id::JumpPicker) => {
                self.jump_choices.clear();
            }
            // Esc anywhere in the broadcast flow cancels the whole
            // thing — drop the stashed targets + picked snippet so a
            // later flow starts clean. The sidebar selection survives:
            // the user only backed out of composing, not of selecting.
            Some(Id::BroadcastSnippet) => {
                self.snippet_choices.clear();
                self.pending_broadcast = None;
            }
            Some(Id::BroadcastText) => {
                self.pending_broadcast = None;
            }
            Some(Id::ThemePicker) => {
                // Esc cancels the preview: restore the palette that was
                // active when the picker opened and drop the stashes.
                if let Some(prev) = self.theme_picker_prev.take() {
                    crate::theme::set_by_name(&prev);
                }
                self.theme_choices.clear();
                self.redraw = true;
            }
            Some(Id::DefaultAgentPicker) => {
                self.default_agent_choices.clear();
            }
            Some(Id::DefaultModelPicker) => {
                self.default_model_choices.clear();
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
                if let Some((workspace_key, reason)) = self.active_removal_prompt.take() {
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
                // dispatch the queued action against the target
                // stashed at mount time (the sidebar selection may
                // have drifted while the modal was up). No / Esc →
                // drop the stash silently.
                let pending = self.pending_action_confirm.take();
                if yes && let Some((action, target)) = pending {
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
                let target = self.pending_inspect_target.take();
                if yes && let Some(row) = target {
                    let force = row.has_uncommitted_changes || row.has_unpushed_commits;
                    cmds.push(IpcCommand::DeleteOrphanedWorktree {
                        path: row.path,
                        force,
                    });
                }
            }
            Some(Id::HelpActionConfirm) => {
                // Action proposed by the Ask Lazybox help agent (#353).
                // Yes → apply it; No / Esc → drop the stash, nothing
                // changes.
                let pending = self.pending_help_action.take();
                if yes && let Some(intent) = pending {
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
                        self.apply_snippets(lazybox_config::Snippets::load_merged(
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
    /// preset), and the returned [`ConfigEdit`] carries a `&'static`
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
                        crate::realm::setup_screen::run_effect(effect, sources.clone(), result);
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
/// Agent terminals (Claude / Codex / Cursor) do NOT use this — they go
/// through `Command::InjectPrompt`, where the daemon sends the paste
/// and the submit `\r` as separate writes gated on the paste's repaint
/// settling, the only way to make the submit reliable across agents
/// whose input areas debounce pasted bursts (#246).
pub(super) fn encode_snippet_for_pty(body: &str) -> Vec<u8> {
    let body = lazybox_agents::trim_leading_blank_lines(body);
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
