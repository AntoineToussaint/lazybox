//! Daemon-event handling, tick callbacks, focus/mouse-mode toggles,
//! sidebar↔right↔terminals state synchronization, and the
//! post-create preselect helper.
//!
//! The big `handle_daemon_event` matcher lives here — it routes
//! `IpcEvent`s (workspace upserts, terminal lifecycle, viewer
//! identities, project mirroring, etc.) into the right pane and
//! into the various pending-modal queues. The tick handlers
//! (`tick_notice`, `tick_right`, `polling_tick`) plus the
//! mouse-capture toggle and pane-focus sync helpers round out
//! the "things the run loop calls between keystrokes" surface.

use super::{Id, ModalFlow, Model, Msg, PaneFocus, ShellCommandConfig};
use lazybox_ipc::{Command as IpcCommand, Event as IpcEvent};
use std::time::Duration;
use tuirealm::terminal::TerminalAdapter;

/// Left in a help turn when the agent proposed an action (#353) but the
/// user had already closed Ask, so it never surfaced a confirm. Marks
/// the answer so a reopened transcript reads as an unapplied proposal.
const ACTION_DROPPED_NOTE: &str =
    "\n\n_(Proposed an action, but Ask was closed before you could confirm — ask again to apply.)_";

const MOUSE_CAPTURE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

impl<T: TerminalAdapter> Model<T> {
    /// Flush all client-side terminal sequence debts through bounded batch
    /// commands. Snapshot recovery can mark hundreds of terminals at once;
    /// chunking by the protocol bound keeps each frame finite without
    /// consuming one command-channel slot per terminal (#1171).
    pub(super) fn flush_pending_terminal_resyncs(&mut self) {
        let requests = self.terminals.drain_pending_resync_requests();
        if requests.is_empty() {
            return;
        }

        let mut sent = 0;
        for chunk in requests.chunks(lazybox_ipc::MAX_RESYNC_REQUESTS_PER_BATCH) {
            if !self.try_send_cmd(IpcCommand::RequestTerminalResync {
                requests: chunk.to_vec(),
            }) {
                self.terminals
                    .requeue_resync_requests(requests[sent..].to_vec());
                return;
            }
            sent += chunk.len();
        }
    }
}

/// Compose a readable action-failure banner (merge/close/update/delete
/// rejected). Leads with the reason — the part that matters — and trims
/// the `owner/repo#NNN` label to just `#NNN`, so the footer's
/// middle-ellipsis keeps the reason instead of spending its budget on
/// the repo-owner prefix and dropping it in the elided middle (#588).
fn action_failure_notice(verb: &str, label: &str, reason: &str) -> String {
    let reason = strip_graphql_path(reason);
    let short = label
        .rsplit_once('#')
        .map(|(_, n)| format!("#{n}"))
        .unwrap_or_else(|| label.to_string());
    format!("✗ {verb} failed: {reason} ({short})")
}

/// Strip the ` [at graphqlPath]` diagnostic suffix GitHub's GraphQL
/// error text carries (`GqlError::full`). It's debugging noise that,
/// mid-truncation, survives as a meaningless `…[at mergePullRequest]`
/// tail while the human reason is elided (#588). Leaves the human
/// message verbatim.
fn strip_graphql_path(reason: &str) -> String {
    let mut s = reason.to_string();
    while let Some(start) = s.find(" [at ") {
        match s[start..].find(']') {
            Some(rel) => s.replace_range(start..start + rel + 1, ""),
            None => break,
        }
    }
    s.trim().to_string()
}

impl<T: TerminalAdapter> Model<T> {
    fn request_host_mouse_capture(&mut self, enabled: bool) -> std::io::Result<()> {
        let result = (self.mouse_capture_requester)(enabled);
        self.mouse_capture_requested_at = std::time::Instant::now();
        result
    }

    pub(super) fn mouse_input_verified(&self) -> bool {
        self.host_mouse_verified
    }

    /// Flip lazybox's mouse capture on/off. Issues
    /// `EnableMouseCapture` / `DisableMouseCapture` to stdout so the
    /// host terminal switches between "send mouse to lazybox" and
    /// "handle mouse natively (selection works)". Footer notice
    /// confirms which mode is now active.
    pub(super) fn toggle_mouse_capture(&mut self) {
        self.mouse_capture_on = !self.mouse_capture_on;
        self.host_mouse_verified = false;
        self.mouse_unverified_logged = false;
        let msg = if self.mouse_capture_on {
            "mouse: lazybox capture requested — move the pointer to verify; F8 or Alt-s for host selection"
        } else {
            "mouse: host selection — right-click links off; use ]]u to open a link, or F8 / Alt-s to enable"
        };
        match self.request_host_mouse_capture(self.mouse_capture_on) {
            Ok(()) => {
                tracing::info!(
                    mouse_capture_on = self.mouse_capture_on,
                    "mouse capture toggled"
                );
                self.flash_info(msg);
            }
            Err(e) => {
                tracing::warn!(
                    mouse_capture_on = self.mouse_capture_on,
                    "mouse capture request failed: {e}"
                );
                self.flash_error(format!("mouse mode failed: {e}"));
            }
        }
    }

    pub(super) fn note_host_mouse_input(&mut self) {
        if !self.mouse_capture_on || self.host_mouse_verified {
            return;
        }
        self.host_mouse_verified = true;
        self.mouse_unverified_logged = false;
        tracing::info!("host mouse reporting verified");
        if self.focus == PaneFocus::Terminals {
            self.redraw = true;
        }
    }

    pub(super) fn host_focus_gained(&mut self) {
        if !self.mouse_capture_on {
            return;
        }
        // A focus change can mean the host terminal was re-initialized —
        // display sleep/wake, a window restore, a tmux/screen re-attach —
        // any of which can silently stop the emulator forwarding mouse
        // reports. Re-arm verification so a genuinely broken emulator
        // re-surfaces the `]]u` hint instead of lazybox claiming mouse
        // works forever (idle, by contrast, is NOT re-armed — that
        // heuristic false-flagged a still pointer, which is the #949 bug).
        // Re-arm SILENTLY: the next mouse event re-verifies within
        // milliseconds on a working emulator, and click-to-open is never
        // gated on the flag, so no first action is blocked. The old code
        // additionally flashed a confusing "waiting for host reporting"
        // notice here — that flash was the visible symptom of #949.
        self.host_mouse_verified = false;
        self.mouse_unverified_logged = false;
        if let Err(e) = self.request_host_mouse_capture(true) {
            tracing::warn!("mouse capture reassert failed on focus regain: {e}");
            self.flash_error(format!("mouse mode failed: {e}"));
        } else {
            tracing::info!("mouse capture reasserted on focus regain");
        }
    }

    pub(super) fn tick_mouse_capture(&mut self) {
        if !self.mouse_capture_on {
            return;
        }
        if self.mouse_capture_requested_at.elapsed() < MOUSE_CAPTURE_REFRESH_INTERVAL {
            return;
        }
        match self.request_host_mouse_capture(true) {
            Ok(()) => {
                if !self.mouse_input_verified() && !self.mouse_unverified_logged {
                    tracing::info!(
                        reason = "no_mouse_event_since_capture_request",
                        "host mouse reporting remains unverified"
                    );
                    self.mouse_unverified_logged = true;
                }
                tracing::debug!(
                    verified = self.mouse_input_verified(),
                    "mouse capture refreshed"
                );
            }
            Err(e) => {
                tracing::warn!("mouse capture refresh failed: {e}");
            }
        }
    }

    /// The single owned mutator for `focus`. Assigns the focused pane AND
    /// fans the change out to the panes' `focused` flags (via
    /// `set_focus_attr`), so the derived per-pane flag — and the
    /// typed-since-focus reset — can never drift from `self.focus`. Every
    /// focus change routes through here; the old "assigned `self.focus` but
    /// forgot `set_focus_attr()`" footgun (a pane highlighting focus while
    /// keys route elsewhere) is now unrepresentable. `set_focus_attr` stays
    /// callable on its own for the rare re-fan with no focus change (initial
    /// mount, a pane-visibility toggle).
    pub(super) fn set_focus(&mut self, focus: PaneFocus) {
        // A held single `]` (the escape latch) belongs to the pane it was
        // pressed in. Both the terminal and the sidebar now arm it (#871),
        // so a focus change must drop it — otherwise the held `]` resolves
        // in the pane you moved to: a stray literal `]` into an agent, or a
        // spurious snippet browser. Mirrors the original "focus left the
        // terminal → drop the held `]`" rule, applied symmetrically.
        if self.focus != focus {
            self.escape_latch.disarm();
            // A deliberate focus change ends any in-flight sidebar
            // typing run (#1110), so bouncing to a terminal and back
            // doesn't leave a stale burst that suppresses the next
            // genuine shortcut.
            self.sidebar_burst.reset();
        }
        self.focus = focus;
        self.set_focus_attr();
    }

    pub(super) fn set_focus_attr(&mut self) {
        self.sidebar.set_focused(self.focus == PaneFocus::Sidebar);
        self.right.set_focused(self.focus == PaneFocus::Right);
        self.terminals
            .set_focused(self.focus == PaneFocus::Terminals);
        // Reset the typed-since-focus flag every time focus changes.
        // A fresh visit to the terminal pane starts with `false` so
        // a single Tab cycles back out (no input → no autocomplete
        // target). After the first non-Tab key the flag flips and
        // Tab routes to the PTY normally.
        self.terminal_user_typed_since_focus = false;
        // Entering the terminal pane on an outdated old-build terminal
        // explains its broken scrollback in context (#544).
        self.hint_outdated_scroll_focus();
        // Same for a bypass-mode terminal's compact `⚠` glyph (#989).
        self.hint_no_permission_focus();
    }

    /// Record the recovered old-build terminals the daemon flagged and,
    /// only when the tracked set actually grows, surface a single
    /// auto-fading notice (#544). The daemon re-emits
    /// `RecoveredTerminalsRequireRestart` on every reconnect snapshot;
    /// re-flashing an already-known set is exactly the permanent global
    /// nag this replaces — clearing that flagged terminal requires
    /// killing a live agent, which no one does to dismiss a banner. So
    /// the notice is `Retryable` (auto-fades, `Esc`-dismissable) rather
    /// than `Permanent`, and fires at most once per newly-flagged
    /// terminal.
    fn note_outdated_scroll_terminals(&mut self, terminal_ids: &[lazybox_ipc::TerminalId]) {
        let mut grew = false;
        for id in terminal_ids {
            if self.outdated_scroll_terminals.insert(*id) {
                grew = true;
            }
        }
        if !grew {
            return;
        }
        let count = self.outdated_scroll_terminals.len();
        let noun = if count == 1 { "session" } else { "sessions" };
        self.flash(
            format!(
                "⚠ scrollback limited in {count} recovered agent {noun} started by an \
                 older lazybox build — reopen a session to enable it"
            ),
            crate::realm::components::footer::NoticeSeverity::Retryable,
        );
    }

    /// Drop a terminal from the outdated-scroll set once it exits (#544):
    /// reopening the session is what heals it, so an exit means the
    /// warning no longer applies and must stop nagging.
    fn forget_outdated_scroll_terminal(&mut self, terminal_id: lazybox_ipc::TerminalId) {
        self.outdated_scroll_terminals.remove(&terminal_id);
        if self.outdated_scroll_hinted == Some(terminal_id) {
            self.outdated_scroll_hinted = None;
        }
    }

    /// When focus rests on a recovered old-build terminal whose
    /// scrollback is broken, explain it in context (#544) — once per
    /// terminal, so re-syncing or bouncing pane focus on the same
    /// terminal never re-nags. Resetting the throttle when the active
    /// terminal changes lets re-entering an affected terminal re-explain.
    fn hint_outdated_scroll_focus(&mut self) {
        let active = self.terminals.active_terminal_id();
        if self.outdated_scroll_hinted != active {
            self.outdated_scroll_hinted = None;
        }
        if self.focus == PaneFocus::Terminals
            && let Some(id) = active
            && self.outdated_scroll_terminals.contains(&id)
            && self.outdated_scroll_hinted != Some(id)
        {
            self.outdated_scroll_hinted = Some(id);
            self.flash_hint(
                "scrollback unavailable here — reopen this session (older lazybox build) \
                 to enable it",
            );
        }
    }

    /// The `⚠` tab glyph is compact by design (#989), so landing on a
    /// bypass-mode terminal spells out what it means in the footer —
    /// once per terminal, so re-syncing or bouncing pane focus on the
    /// same terminal never re-nags. Resetting the throttle when the
    /// active terminal changes lets re-entering a bypass terminal
    /// re-explain. Called from every path that can change which terminal
    /// is active: pane-focus changes, workspace selection, and the
    /// `]]`-leader tab/tile switches that never touch pane focus.
    ///
    /// Yields to the #544 outdated-scroll hint: a terminal that is both
    /// recovered-old-build and bypass shows the functional
    /// scrollback-limited warning rather than this informational one, so
    /// the two focus hints (run back-to-back in `set_focus_attr`) can't
    /// clobber each other.
    pub(super) fn hint_no_permission_focus(&mut self) {
        let active = self.terminals.active_terminal_id();
        if self.no_permission_hinted != active {
            self.no_permission_hinted = None;
        }
        if self.focus == PaneFocus::Terminals
            && let Some(id) = active
            && self.terminals.terminal_no_permission(id)
            && !self.outdated_scroll_terminals.contains(&id)
            && self.no_permission_hinted != Some(id)
        {
            self.no_permission_hinted = Some(id);
            self.flash_hint(
                "⚠ no-permission mode — this agent runs unattended, auto-accepting \
                 tool-use prompts",
            );
        }
    }

    /// True when `workspace_key` is already the active removal prompt
    /// or sitting in the queue. The daemon dedupes per-process, but a
    /// re-emit (daemon restart, a `g m` `PrMerged` racing the poll's
    /// `MergedPrRemovable`) could otherwise stack duplicate prompts —
    /// belt and braces.
    fn removal_already_pending(&self, workspace_key: &lazybox_core::WorkspaceKey) -> bool {
        let active = matches!(
            &self.modal_flow,
            Some(ModalFlow::RemovalPrompt { workspace, .. }) if workspace == workspace_key
        );
        let queued = self
            .removal_prompt_queue
            .iter()
            .any(|p| &p.workspace_key == workspace_key);
        active || queued
    }

    /// Forward an inbound daemon event into all three panes, then
    /// project the (possibly moved) sidebar selection onto the right
    /// pane + terminal stack. The projection is deferred to a single
    /// `flush_pane_sync` so processing one event in isolation
    /// (the per-event test entry point) still ends fully synced, while
    /// the run loop's drain can coalesce a whole batch into one
    /// projection — see `dispatch_daemon_event`.
    pub fn handle_daemon_event(&mut self, event: IpcEvent) {
        self.dispatch_daemon_event(event);
        self.flush_pane_sync();
    }

    fn start_conversion_target(&mut self) {
        let Some(conversion) = self.conversion.as_mut() else {
            return;
        };
        let Some(prompt) = conversion.target_prompt.clone() else {
            return;
        };
        let Some(session_id) = conversion.source_session_id else {
            self.conversion = None;
            self.flash_info("session conversion stopped — source session ownership was lost");
            return;
        };
        conversion.phase = super::ConversionPhase::Spawning;
        let command = IpcCommand::Spawn {
            session_key: conversion.draft.source.clone(),
            session_id: Some(session_id),
            client_request_id: Some(conversion.request_id.0.clone()),
            kind: lazybox_ipc::TerminalKind::Agent(conversion.draft.agent.clone()),
            cwd: None,
            initial_prompt: Some(prompt),
            initial_snippet: None,
            on_main: false,
            model_alias: None,
            access: match conversion.role {
                lazybox_core::prompts::AgentHandoffRole::Continue => {
                    lazybox_ipc::AgentRunAccess::Default
                }
                lazybox_core::prompts::AgentHandoffRole::Critic => {
                    lazybox_ipc::AgentRunAccess::ReadOnly
                }
            },
        };
        self.spawn_follow_to = Some(conversion.draft.source.clone());
        self.last_spawn = Some(command.clone());
        let message = format!(
            "handoff ready: {} → fresh {} ({})…",
            conversion.draft.source_name,
            conversion.draft.agent,
            conversion.role.label().to_ascii_lowercase(),
        );
        self.send_cmd(command);
        self.flash_info(message);
    }

    /// Feed the sidebar's per-provider usage tracker from the structured
    /// agent-run stream (#1059): bind each run to its agent, keep its
    /// turn's high-water mark, commit that once per turn, and drop the
    /// binding when the run finishes. A single turn reports usage more
    /// than once (streaming `message_delta` + final `result`), so usage is
    /// committed on `AgentTurnFinished`, not summed per event. A pure join
    /// over events every other handler leaves untouched.
    fn record_agent_usage(&mut self, event: &IpcEvent) {
        match event {
            IpcEvent::AgentRunStarted { run_id, agent, .. } => {
                self.sidebar.note_agent_run(*run_id, agent);
            }
            IpcEvent::AgentUsage { run_id, usage } => {
                self.sidebar.add_agent_usage(*run_id, usage);
            }
            IpcEvent::AgentSessionUsage { agent_id, usage } => {
                self.sidebar.add_agent_session_usage(agent_id, usage);
            }
            IpcEvent::AgentProviderQuota { agent_id, quota } => {
                self.sidebar.note_provider_quota(agent_id, *quota);
            }
            IpcEvent::AgentTurnFinished { run_id, .. } => {
                self.sidebar.commit_agent_turn(*run_id);
            }
            IpcEvent::AgentRunFinished { run_id, .. } => {
                self.sidebar.finish_agent_run(*run_id);
            }
            _ => {}
        }
    }

    fn handle_conversion_agent_event(&mut self, event: &IpcEvent) -> bool {
        match event {
            IpcEvent::AgentRunStarted {
                request_id,
                run_id,
                session_key,
                session_id,
                ..
            } if self.conversion.as_ref().is_some_and(|conversion| {
                conversion.phase == super::ConversionPhase::Starting
                    && conversion.request_id == *request_id
            }) =>
            {
                if let Some(conversion) = self.conversion.as_mut() {
                    let Some(session_id) = session_id else {
                        self.send_cmd(IpcCommand::InterruptAgentRun { run_id: *run_id });
                        self.conversion = None;
                        self.flash_info(
                            "session conversion stopped — source terminal has no owning session",
                        );
                        return true;
                    };
                    conversion.run_id = Some(*run_id);
                    conversion.draft.source = session_key.clone();
                    conversion.source_session_id = Some(*session_id);
                    conversion.phase = super::ConversionPhase::Capturing;
                }
                true
            }
            IpcEvent::AgentAssistantTextDelta { run_id, delta }
                if self
                    .conversion
                    .as_ref()
                    .is_some_and(|conversion| conversion.run_id == Some(*run_id)) =>
            {
                const MAX_HANDOFF_BYTES: usize = 128 * 1024;
                let too_large = self.conversion.as_ref().is_some_and(|conversion| {
                    conversion.response.len().saturating_add(delta.len()) > MAX_HANDOFF_BYTES
                });
                if too_large {
                    self.conversion = None;
                    self.send_cmd(IpcCommand::InterruptAgentRun { run_id: *run_id });
                    self.flash_info("session conversion stopped — agent handoff exceeded 128 KiB");
                } else if let Some(conversion) = self.conversion.as_mut() {
                    conversion.response.push_str(delta);
                }
                true
            }
            IpcEvent::AgentTurnFinished {
                run_id,
                result,
                error,
                ..
            } if self.conversion.as_ref().is_some_and(|conversion| {
                conversion.phase == super::ConversionPhase::Capturing
                    && conversion.run_id == Some(*run_id)
            }) =>
            {
                let Some(mut conversion) = self.conversion.take() else {
                    return true;
                };
                self.send_cmd(IpcCommand::InterruptAgentRun { run_id: *run_id });
                if let Some(error) = error {
                    self.flash_info(format!(
                        "session conversion stopped — handoff failed: {error}"
                    ));
                    return true;
                }
                let handoff = result
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or(&conversion.response)
                    .trim()
                    .to_string();
                if handoff.is_empty() {
                    self.flash_info(
                        "session conversion stopped — the agent returned an empty handoff",
                    );
                    return true;
                }
                if handoff.len() > 128 * 1024 {
                    self.flash_info("session conversion stopped — agent handoff exceeded 128 KiB");
                    return true;
                }
                conversion.target_prompt = Some(lazybox_core::prompts::build_handoff_role_prompt(
                    conversion.role,
                    &handoff,
                ));
                let mut commands = Vec::new();
                let awaiting_exit = self.terminals.prepare_agent_replacement(
                    conversion.draft.source_terminal,
                    &conversion.request_id.0,
                    &mut commands,
                );
                conversion.phase = if awaiting_exit {
                    super::ConversionPhase::AwaitingSourceExit
                } else {
                    super::ConversionPhase::Spawning
                };
                self.conversion = Some(conversion);
                for command in commands {
                    self.send_cmd(command);
                }
                if !awaiting_exit {
                    self.start_conversion_target();
                }
                true
            }
            IpcEvent::AgentRunFinished { run_id, error, .. }
                if self.conversion.as_ref().is_some_and(|conversion| {
                    conversion.phase == super::ConversionPhase::Capturing
                        && conversion.run_id == Some(*run_id)
                }) =>
            {
                self.conversion = None;
                self.flash_info(format!(
                    "session conversion stopped — handoff agent exited{}",
                    error
                        .as_deref()
                        .map(|message| format!(": {message}"))
                        .unwrap_or_default(),
                ));
                true
            }
            IpcEvent::AgentRunFinished { run_id, .. }
                if self
                    .conversion
                    .as_ref()
                    .is_some_and(|conversion| conversion.run_id == Some(*run_id)) =>
            {
                true
            }
            IpcEvent::AgentRunStartFailed {
                request_id,
                message,
            } if self.conversion.as_ref().is_some_and(|conversion| {
                conversion.phase == super::ConversionPhase::Starting
                    && conversion.request_id == *request_id
            }) =>
            {
                self.conversion = None;
                self.flash_info(format!(
                    "session conversion stopped — handoff unavailable: {message}"
                ));
                true
            }
            IpcEvent::TerminalExited { terminal_id, .. }
                if self.conversion.as_ref().is_some_and(|conversion| {
                    conversion.phase == super::ConversionPhase::AwaitingSourceExit
                        && conversion.draft.source_terminal == *terminal_id
                }) =>
            {
                self.start_conversion_target();
                false
            }
            IpcEvent::CommandCompleted { client_request_id }
                if self.conversion.as_ref().is_some_and(|conversion| {
                    conversion.phase == super::ConversionPhase::Spawning
                        && conversion.request_id.0.as_str() == client_request_id
                }) =>
            {
                if let Some(conversion) = self.conversion.take() {
                    self.flash_info(format!(
                        "converted: {} → {} {}",
                        conversion.draft.source_name,
                        conversion.role.label().to_ascii_lowercase(),
                        conversion.draft.agent,
                    ));
                }
                true
            }
            IpcEvent::CommandFailed {
                client_request_id,
                message,
            } if self.conversion.as_ref().is_some_and(|conversion| {
                conversion.request_id.0.as_str() == client_request_id
                    && matches!(
                        conversion.phase,
                        super::ConversionPhase::AwaitingSourceExit
                            | super::ConversionPhase::Spawning
                    )
            }) =>
            {
                if let Some(conversion) = self.conversion.take() {
                    self.terminals
                        .cancel_agent_replacement(conversion.draft.source_terminal);
                }
                self.flash_info(format!("session conversion stopped — {message}"));
                true
            }
            IpcEvent::TerminalsRebadged { from, to }
                if self
                    .conversion
                    .as_ref()
                    .is_some_and(|conversion| conversion.draft.source == *from) =>
            {
                if let Some(conversion) = self.conversion.as_mut() {
                    conversion.draft.source = to.clone();
                }
                false
            }
            _ => false,
        }
    }

    /// Consume one event belonging to the help-assistant run (#302),
    /// feeding the shared `help_convo` the `HelpAsk` modal renders.
    /// Returns `true` when the event was help-run traffic so
    /// `dispatch_daemon_event` skips the pane fan-out for it. The run
    /// is recognized by the request id generated for `StartAgentRun`;
    /// everything after correlates on the captured run id.
    fn handle_help_agent_event(&mut self, event: &IpcEvent) -> bool {
        match event {
            IpcEvent::AgentRunStarted {
                request_id, run_id, ..
            } if self.help_start_request.as_ref() == Some(request_id) => {
                self.help_start_request = None;
                if self.help_interrupt_on_start {
                    self.help_interrupt_on_start = false;
                    self.send_cmd(lazybox_ipc::Command::InterruptAgentRun { run_id: *run_id });
                    if let Some(question) = self.help_restart_question.take() {
                        if let Some(cmd) = self.start_help_run_command(&question) {
                            self.send_cmd(cmd);
                        }
                    } else {
                        self.help_pending_questions.clear();
                    }
                    self.redraw = true;
                    return true;
                }
                self.help_run = Some(*run_id);
                // Questions that raced the run start ride in now, in
                // submission order.
                for question in std::mem::take(&mut self.help_pending_questions) {
                    self.send_cmd(lazybox_ipc::Command::SendAgentInput {
                        run_id: *run_id,
                        message: lazybox_ipc::AgentInputMessage {
                            text: Some(question),
                            json: None,
                        },
                    });
                }
                self.redraw = true;
                true
            }
            IpcEvent::AgentAssistantTextDelta { run_id, delta }
                if Some(*run_id) == self.help_run =>
            {
                if let Some(turn) = self.help_convo_mut().streaming_turn_mut() {
                    turn.answer.push_str(delta);
                }
                self.redraw = true;
                true
            }
            IpcEvent::AgentTurnFinished {
                run_id,
                result,
                error,
                ..
            } if Some(*run_id) == self.help_run => {
                let mut intent = None;
                {
                    let mut convo = self.help_convo_mut();
                    if let Some(turn) = convo.streaming_turn_mut() {
                        // The result carries the authoritative final text;
                        // prefer it over the accumulated deltas so a
                        // dropped delta can't leave a truncated answer.
                        if let Some(result) = result.as_deref().filter(|r| !r.trim().is_empty()) {
                            turn.answer = result.to_string();
                        }
                        turn.done = true;
                        // A finished answer may carry a proposed action
                        // (#353). Always strip the raw block so no intent
                        // JSON leaks into the transcript, then decide what
                        // to do with it. The confirm mounts below, off the
                        // lock — but only while the help modal is still
                        // open (the same gate `propose_help_action` uses).
                        // If the user closed Ask before the answer arrived
                        // the action is dropped; leave a short note in its
                        // place so a reopened transcript doesn't read as if
                        // it ran.
                        if let Some(parsed) =
                            lazybox_tui_core::help::parse_action_intent(&turn.answer)
                        {
                            turn.answer = lazybox_tui_core::help::strip_action_block(&turn.answer);
                            if self.modal_stack.last() == Some(&Id::HelpAsk) {
                                intent = Some(parsed);
                            } else {
                                turn.answer.push_str(ACTION_DROPPED_NOTE);
                            }
                        }
                    }
                    if let Some(error) = error {
                        convo.notice = Some(error.clone());
                    }
                }
                self.redraw = true;
                if let Some(intent) = intent {
                    self.propose_help_action(intent);
                }
                true
            }
            IpcEvent::AgentRunFinished { run_id, error, .. } if Some(*run_id) == self.help_run => {
                // The process exited — the next question starts a
                // fresh run (with fresh context). Every open turn is
                // dead with it, including follow-ups queued behind the
                // one that was streaming.
                self.help_run = None;
                self.help_start_request = None;
                let mut convo = self.help_convo_mut();
                let unanswered = convo.close_open_turns();
                convo.deactivate_thread();
                if let Some(error) = error {
                    convo.notice = Some(format!("help assistant exited: {error}"));
                } else if unanswered {
                    convo.notice = Some(
                        "help assistant exited before answering — ask again to restart it".into(),
                    );
                }
                drop(convo);
                self.redraw = true;
                true
            }
            IpcEvent::AgentRunStartFailed {
                request_id,
                message,
            } if self.help_start_request.as_ref() == Some(request_id) => {
                self.help_start_request = None;
                if self.help_interrupt_on_start {
                    self.help_interrupt_on_start = false;
                    if let Some(question) = self.help_restart_question.take() {
                        if let Some(cmd) = self.start_help_run_command(&question) {
                            self.send_cmd(cmd);
                        }
                    } else {
                        self.help_pending_questions.clear();
                    }
                    self.redraw = true;
                    return true;
                }
                self.help_pending_questions.clear();
                let mut convo = self.help_convo_mut();
                convo.close_open_turns();
                convo.deactivate_thread();
                convo.notice = Some(format!("help assistant unavailable — {message}"));
                drop(convo);
                self.redraw = true;
                true
            }
            _ => false,
        }
    }

    /// Consume one event belonging to the "Ask about this PR" run
    /// (#945), feeding the shared `pr_chat_convo` the `PrChat` modal
    /// renders. Correlates like [`Self::handle_help_agent_event`] but has
    /// no proposed-action layer — this chat only reads and explains.
    fn handle_pr_chat_agent_event(&mut self, event: &IpcEvent) -> bool {
        match event {
            IpcEvent::AgentRunStarted {
                request_id, run_id, ..
            } if self.pr_chat_request.as_ref() == Some(request_id) => {
                self.pr_chat_request = None;
                self.pr_chat_run = Some(*run_id);
                for question in std::mem::take(&mut self.pr_chat_pending) {
                    self.send_cmd(lazybox_ipc::Command::SendAgentInput {
                        run_id: *run_id,
                        message: lazybox_ipc::AgentInputMessage {
                            text: Some(question),
                            json: None,
                        },
                    });
                }
                self.redraw = true;
                true
            }
            IpcEvent::AgentAssistantTextDelta { run_id, delta }
                if Some(*run_id) == self.pr_chat_run =>
            {
                if let Some(turn) = self.pr_chat_convo_mut().streaming_turn_mut() {
                    turn.answer.push_str(delta);
                }
                self.redraw = true;
                true
            }
            IpcEvent::AgentTurnFinished {
                run_id,
                result,
                error,
                ..
            } if Some(*run_id) == self.pr_chat_run => {
                let mut convo = self.pr_chat_convo_mut();
                if let Some(turn) = convo.streaming_turn_mut() {
                    if let Some(result) = result.as_deref().filter(|r| !r.trim().is_empty()) {
                        turn.answer = result.to_string();
                    }
                    turn.done = true;
                }
                if let Some(error) = error {
                    convo.notice = Some(error.clone());
                }
                drop(convo);
                self.redraw = true;
                true
            }
            IpcEvent::AgentRunFinished { run_id, error, .. }
                if Some(*run_id) == self.pr_chat_run =>
            {
                self.pr_chat_run = None;
                self.pr_chat_request = None;
                let mut convo = self.pr_chat_convo_mut();
                let unanswered = convo.close_open_turns();
                convo.deactivate_thread();
                if let Some(error) = error {
                    convo.notice = Some(format!("assistant exited: {error}"));
                } else if unanswered {
                    convo.notice =
                        Some("assistant exited before answering — ask again to restart it".into());
                }
                drop(convo);
                self.redraw = true;
                true
            }
            IpcEvent::AgentRunStartFailed {
                request_id,
                message,
            } if self.pr_chat_request.as_ref() == Some(request_id) => {
                self.pr_chat_request = None;
                self.pr_chat_pending.clear();
                let mut convo = self.pr_chat_convo_mut();
                convo.close_open_turns();
                convo.deactivate_thread();
                convo.notice = Some(format!("assistant unavailable — {message}"));
                drop(convo);
                self.redraw = true;
                true
            }
            _ => false,
        }
    }

    /// Run the deferred `sync_panes` if any event in this drain batch
    /// asked for one. A no-op when nothing pane-affecting was seen, so
    /// the run loop can call it unconditionally after every drain.
    pub(super) fn flush_pane_sync(&mut self) {
        if self.needs_pane_sync {
            self.needs_pane_sync = false;
            self.sync_panes();
        }
    }

    /// Route one daemon event into the panes and the pending-modal
    /// queues. Handlers that move the sidebar selection set
    /// `needs_pane_sync` rather than calling `sync_panes` inline, so a
    /// merge burst (`TerminalsRebadged` → `WorkspaceRemoved` →
    /// `WorkspaceMerged`) drained together re-projects the panes once
    /// instead of once per event — each `sync_panes` clones the
    /// selected `Workspace` and re-emits `FocusWorkspace`, work that
    /// only the batch's final selection makes worthwhile.
    ///
    /// After the very first Snapshot, apply any pending CLI preselect.
    /// Also feeds the polling modal so it can detect "first task
    /// arrived".
    pub(super) fn dispatch_daemon_event(&mut self, mut event: IpcEvent) {
        // Hot path: PTY output. Bytes only mutate one terminal's grid
        // — no workspace / sidebar / layout state changes — so skip
        // the full fan-out (and the Workspace clone inside
        // `sync_panes`) entirely. A chatty agent emits hundreds of
        // these per second; cloning the selected workspace for each
        // one was measurable. Redraw only when the target terminal is
        // actually on screen — output for a background workspace's
        // terminal changes no pixels.
        if let IpcEvent::TerminalOutput { terminal_id, .. } = &event {
            let visible = self.terminals.is_terminal_visible(*terminal_id);
            self.terminals.on_daemon_event(&event);
            // Resync requests queued by this event flush once per drain
            // batch (#1237, with the batch's `flush_pane_sync`), not per
            // event — during a desync storm the per-event flush emitted
            // one command per event into the 32-slot channel keystrokes
            // share, so the batching cap never engaged.
            if visible {
                self.redraw = true;
            }
            return;
        }
        // Deep-scrollback reply (#393): like raw output, it only
        // mutates one terminal's grid — no workspace / layout state —
        // so it takes the same short-circuit instead of the full
        // fan-out.
        if let IpcEvent::TerminalScrollback { terminal_id, .. } = &event {
            let visible = self.terminals.is_terminal_visible(*terminal_id);
            self.terminals.on_daemon_event(&event);
            if visible {
                self.redraw = true;
            }
            return;
        }
        // Fold structured-run token usage into the per-provider running
        // total the header widget shows (#1059). Side-effect only — it
        // never consumes the event, so it must run before the
        // conversion/help/pr-chat handlers below (which `return` early for
        // their own runs, and whose runs draw down the same plan window we
        // are accounting).
        self.record_agent_usage(&event);
        // Help-assistant run traffic (#302): structured agent JSONL
        // events no pane consumes. Route them into the shared help
        // conversation and stop — this must run before the general
        // fan-out so an `agent_run` provider error lands in the help
        // modal instead of the footer/sync-log as a bogus sync failure.
        if self.handle_conversion_agent_event(&event) {
            return;
        }
        if self.handle_help_agent_event(&event) {
            return;
        }
        if self.handle_pr_chat_agent_event(&event) {
            return;
        }
        match &event {
            IpcEvent::AgentCreditExhausted { hint, .. } => {
                let key = lazybox_tui_core::action::ActionDef::for_kind(
                    lazybox_tui_core::action::ActionKind::RecoverAgentCredit,
                )
                .effective_keys_display(&self.action_key_overrides);
                self.flash_error(format!(
                    "agent is out of credit — {hint}; press {key} to recover"
                ));
            }
            IpcEvent::AgentCreditRecovery {
                client_request_id,
                stage,
                ..
            } if self
                .pending_credit_recoveries
                .contains_key(client_request_id) =>
            {
                let message = match stage {
                    lazybox_ipc::AgentCreditRecoveryStage::SelectingWait => {
                        "selecting Wait for credit…"
                    }
                    lazybox_ipc::AgentCreditRecoveryStage::WaitingForComposer => {
                        "waiting for credit and a ready composer…"
                    }
                    lazybox_ipc::AgentCreditRecoveryStage::InjectingContinuation => {
                        "credit available — submitting continuation…"
                    }
                };
                self.flash_info(message);
            }
            IpcEvent::CommandCompleted { client_request_id }
                if self
                    .pending_credit_recoveries
                    .remove(client_request_id)
                    .is_some() =>
            {
                self.flash_info("credit recovered — continuation submitted");
            }
            IpcEvent::CommandFailed {
                client_request_id,
                message,
            } if self
                .pending_credit_recoveries
                .remove(client_request_id)
                .is_some() =>
            {
                self.flash_error(format!("credit recovery failed — {message}"));
            }
            _ => {}
        }
        // New-workspace creation is request-correlated because the daemon,
        // not the client, allocates the final collision-suffixed key. Reveal
        // that exact row as soon as durability is acknowledged, then keep the
        // request pending until the optional agent spawn explicitly completes
        // or fails. This closes the old silent-success hole where the row and
        // terminal existed but focus stayed on an unrelated repository.
        match &event {
            IpcEvent::WorkspaceCreated {
                client_request_id,
                workspace_key,
            } => {
                if let Some(pending) = self.pending_workspace_creates.get_mut(client_request_id) {
                    let session_key: lazybox_core::SessionKey = workspace_key.into();
                    pending.workspace_key = Some(session_key.clone());
                    let spawn_agent = pending.spawn_agent;
                    let name = pending.name.clone();
                    self.sidebar.focus_workspace_key(&session_key);
                    if spawn_agent {
                        self.spawn_follow_to = Some(session_key);
                        self.flash_info(format!("created {name} — starting agent…"));
                    } else {
                        self.flash_info(format!("created {name}"));
                    }
                    self.needs_pane_sync = true;
                    self.redraw = true;
                }
            }
            IpcEvent::CommandCompleted { client_request_id } => {
                if let Some(pending) = self.pending_workspace_creates.remove(client_request_id) {
                    let message = if pending.spawn_agent {
                        format!("workspace {} ready", pending.name)
                    } else {
                        format!("workspace {} created", pending.name)
                    };
                    self.flash_info(message);
                }
            }
            IpcEvent::CommandFailed {
                client_request_id,
                message,
            } => {
                if let Some(pending) = self.pending_workspace_creates.remove(client_request_id) {
                    if pending.workspace_key.as_ref() == self.spawn_follow_to.as_ref() {
                        self.spawn_follow_to = None;
                    }
                    let failure = if pending.workspace_key.is_some() {
                        format!(
                            "✗ workspace {} was created, but its agent failed to start — {message}",
                            pending.name
                        )
                    } else {
                        format!("✗ workspace {} was not created — {message}", pending.name)
                    };
                    self.flash_error(failure);
                }
            }
            _ => {}
        }
        if let IpcEvent::WorkspaceFocusRequested { session_key } = &event {
            self.jump_to_workspace_key(session_key);
            return;
        }
        // Enforce the confirmed-merge latch centrally, before any pane
        // sees the workspace. Once GitHub accepted a merge
        // (`Event::PrMerged` latched the key), an incoming
        // poll/snapshot that still reports `Open` is stale — patch the
        // owned event to MERGED here so the sidebar row AND the right-pane
        // header agree. No-op unless a key is latched.
        if !self.merge_confirmed.is_empty() || !self.remote_marks.is_empty() {
            match &mut event {
                IpcEvent::WorkspaceUpserted(ws) => {
                    // The payload rides an `Arc` on the bus (M6):
                    // copy-on-write to patch it. When this client is
                    // the last holder (the common in-process case by
                    // dispatch time) this is a plain in-place borrow,
                    // not a clone.
                    let ws = std::sync::Arc::make_mut(ws);
                    self.apply_merge_latch(ws);
                    self.apply_remote_latch(ws);
                }
                IpcEvent::Snapshot { workspaces, .. } => {
                    for ws in workspaces.iter_mut() {
                        self.apply_merge_latch(ws);
                        self.apply_remote_latch(ws);
                    }
                }
                // Deliberately ignored: the latch only patches
                // workspace payloads, and no other variant carries
                // one. Exhaustive on purpose — a new Event variant
                // must be classified here (does it carry workspaces
                // needing the merge latch?) before this compiles.
                IpcEvent::ViewerIdentities { .. }
                | IpcEvent::AutoFixPolicyConfig { .. }
                | IpcEvent::ShellCommandConfig { .. }
                | IpcEvent::AgentAvailabilityConfig { .. }
                | IpcEvent::WorkspaceRemoved(_)
                | IpcEvent::ProjectUpserted(_)
                | IpcEvent::ProjectRemoved(_)
                | IpcEvent::WorkspaceOutOfScope { .. }
                | IpcEvent::WorkspaceMergePending { .. }
                | IpcEvent::WorkspaceMerged { .. }
                | IpcEvent::PrMerged { .. }
                | IpcEvent::PrMergeFailed { .. }
                | IpcEvent::BranchUpdated { .. }
                | IpcEvent::BranchUpdateFailed { .. }
                | IpcEvent::IssueClosed { .. }
                | IpcEvent::IssueCloseFailed { .. }
                | IpcEvent::PrClosed { .. }
                | IpcEvent::IssueDeleted { .. }
                | IpcEvent::DeleteOrCloseFailed { .. }
                | IpcEvent::MergedPrRemovable { .. }
                | IpcEvent::RemovalCancelled { .. }
                | IpcEvent::RepoLabels { .. }
                | IpcEvent::RequestableReviewers { .. }
                | IpcEvent::SessionCreated(_)
                | IpcEvent::WorktreeProgress { .. }
                | IpcEvent::SessionEnded { .. }
                | IpcEvent::TerminalSpawned { .. }
                | IpcEvent::TerminalReplaced { .. }
                | IpcEvent::TerminalOutput { .. }
                | IpcEvent::AgentAuthOutput { .. }
                | IpcEvent::AgentAuthReplay { .. }
                | IpcEvent::TerminalResync { .. }
                | IpcEvent::TerminalResyncUnavailable { .. }
                | IpcEvent::TerminalDelta { .. }
                | IpcEvent::TerminalDeltaUnavailable { .. }
                | IpcEvent::TerminalScrollback { .. }
                | IpcEvent::TerminalExited { .. }
                | IpcEvent::TerminalFocusRequested { .. }
                | IpcEvent::WorkspaceFocusRequested { .. }
                | IpcEvent::TerminalsRebadged { .. }
                | IpcEvent::AgentState { .. }
                | IpcEvent::ProviderError { .. }
                | IpcEvent::GithubRateLimitWait { .. }
                | IpcEvent::PollCompleted { .. }
                | IpcEvent::PollProgress { .. }
                | IpcEvent::Notification { .. }
                | IpcEvent::CleanWorktreesCompleted { .. }
                | IpcEvent::WorktreesInspected { .. }
                | IpcEvent::WorkspaceDiffInspected { .. }
                | IpcEvent::CheckoutsDiscovered { .. }
                | IpcEvent::OrphanedWorktreeDeleted { .. }
                | IpcEvent::AgentRunStarted { .. }
                | IpcEvent::AgentRunStartFailed { .. }
                | IpcEvent::AgentRawJson { .. }
                | IpcEvent::AgentDebug { .. }
                | IpcEvent::AgentAssistantTextDelta { .. }
                | IpcEvent::AgentToolCallStarted { .. }
                | IpcEvent::AgentToolCallDelta { .. }
                | IpcEvent::AgentToolCallFinished { .. }
                | IpcEvent::AgentPermissionRequest { .. }
                | IpcEvent::AgentUserQuestion { .. }
                | IpcEvent::AgentUsage { .. }
                | IpcEvent::AgentSessionUsage { .. }
                | IpcEvent::AgentProviderQuota { .. }
                | IpcEvent::AgentTurnFinished { .. }
                | IpcEvent::AgentRunFinished { .. }
                | IpcEvent::ProviderCredentialUpdated { .. }
                | IpcEvent::ProviderCredentialRemoved { .. }
                | IpcEvent::ProviderCredentialsListed { .. }
                | IpcEvent::TerminalInputRejected { .. }
                | IpcEvent::CommandRejected { .. }
                | IpcEvent::CommandCompleted { .. }
                | IpcEvent::CommandFailed { .. }
                | IpcEvent::AgentCliUpdatesChecked { .. }
                | IpcEvent::AgentCliUpdateFinished { .. }
                | IpcEvent::SnippetDelivered { .. }
                | IpcEvent::AgentAuthRequired { .. }
                | IpcEvent::AgentAuthProgress { .. }
                | IpcEvent::AgentAuthFinished { .. }
                | IpcEvent::AgentResumeFallback { .. }
                | IpcEvent::TerminalModelChanged { .. }
                | IpcEvent::RecoveredTerminalsRequireRestart { .. }
                | IpcEvent::AgentUsageLimit { .. }
                | IpcEvent::AgentCreditRecovery { .. }
                | IpcEvent::AgentCreditExhausted { .. }
                | IpcEvent::WorkspaceCreated { .. }
                | IpcEvent::ErrorInbox { .. }
                | IpcEvent::ResourcePosture(..) => {}
            }
        }
        // The reset countdown parsed from a usage-limit banner (#1012):
        // stash it and refresh the escalating alert so the sticky banner
        // gains its `· resets <hint>` fragment. The `LimitReached` state
        // itself rode an `AgentState` event (handled below); this only
        // enriches the countdown text, so it's a leaf — no pane fan-out.
        if let IpcEvent::AgentUsageLimit {
            terminal_id,
            reset_hint,
            ..
        } = &event
        {
            self.usage_limit_reset = Some(reset_hint.clone());
            // Also attribute the countdown to the limited terminal's
            // agent, so the always-visible usage summary can show ` ·
            // resets 3pm` for that provider (#1059).
            self.sidebar
                .note_usage_limit_reset(*terminal_id, reset_hint.clone());
            self.refresh_usage_limit_alert();
            return;
        }
        // Agent-state pings repeat at the detector's cadence while an
        // agent streams. Forward them (the asking/working sets and the
        // terminal tab badges live downstream) but skip `sync_panes` —
        // the event can't change the selection or workspace data — and
        // only redraw when the displayed state actually flips.
        if let IpcEvent::AgentState {
            session_key, state, ..
        } = &event
        {
            // Two displays can go stale independently: the sidebar's
            // session-level asking/working pill AND the terminal
            // stack's per-terminal tab badges (a workspace can run
            // two agents whose badges flip without the session-level
            // state moving). Redraw when EITHER would change.
            let changed = !self.sidebar.displays_agent_state(session_key, *state)
                || !self.terminals.displays_agent_state(session_key, *state);
            self.sidebar.on_daemon_event(&event);
            if let Some(msg) = self.sidebar.drain_pending_asking_notices().pop() {
                self.flash_hint(msg);
            }
            // The sidebar just folded this state in, so the rate-limited
            // count is current: reconcile the escalating usage-limit alert
            // (#1012) — raises/retracts its banner on the count's edges.
            self.refresh_usage_limit_alert();
            self.right.on_daemon_event(&event);
            self.terminals.on_daemon_event(&event);
            if changed {
                self.redraw = true;
            }
            return;
        }
        // Viewer identities — fold into the local map and forward
        // to RightPane so activity bylines can render `@me`. This
        // arrives once per daemon connection (just after Snapshot)
        // and re-emits whenever the gh client's authenticated user
        // changes (token rotation).
        if let IpcEvent::ViewerIdentities { logins } = &event {
            for (source, login) in logins {
                self.viewer_logins.insert(source.clone(), login.clone());
            }
            self.right.set_viewer_logins(self.viewer_logins.clone());
            self.redraw = true;
            return;
        }
        // The daemon's authoritative auto-fix policy config (enable switch
        // + opt-out labels), arriving among the post-subscribe pushes.
        // Overrides the client-local config applied at startup so the
        // policies menu (`g p`) reflects what the daemon would actually do
        // — the two configs differ under `--connect` (tracker #512).
        if let IpcEvent::AutoFixPolicyConfig {
            enabled,
            opt_out_labels,
        } = &event
        {
            self.apply_auto_fix_config(*enabled, opt_out_labels.clone());
            self.redraw = true;
            return;
        }
        if let IpcEvent::ShellCommandConfig {
            command,
            configured,
        } = &event
        {
            self.shell_command_config = Some(ShellCommandConfig {
                command: command.clone(),
                configured: *configured,
            });
            self.redraw = true;
            return;
        }
        // The daemon's spawnable-agent set and its default work agent,
        // arriving among the post-subscribe pushes. Only a remote
        // (`--connect`) client adopts them: it otherwise falls back to
        // the hardcoded trio + `claude` because its own local config
        // never applies over the socket (its PATH is the wrong machine's).
        // The embedded client already applied its authoritative local
        // config at startup — the same file the daemon reads — so
        // re-applying here would be a no-op that needlessly rebuilds the
        // catalog over the startup keymap/model wiring. Set the box's
        // default first; `set_agents` then reconciles a default the set
        // doesn't include. See #742.
        if let IpcEvent::AgentAvailabilityConfig {
            agents,
            default_agent,
        } = &event
        {
            if self.remote {
                if let Some(default) = default_agent {
                    self.set_default_agent(default);
                }
                self.set_agents(agents.clone());
                self.redraw = true;
            }
            return;
        }
        // Project lifecycle events. Mirror into `self.projects` so
        // the sidebar can render headers from it, then push the
        // updated map to the sidebar component.
        if let IpcEvent::ProjectUpserted(p) = &event {
            let project = (**p).clone();
            self.projects.insert(project.key.clone(), project.clone());
            // Daemon owns this project now — stop treating it as a
            // client-side placeholder so a later scope edit won't yank
            // it out from under the daemon's `ProjectRemoved`.
            self.synthesized_projects.remove(&project.key);
            self.sidebar.apply_projects(self.projects.clone());
            // Hand-off from x p → CreateProject: the project
            // just landed in the sidebar, but its RepoHeader row
            // is unreachable via j/k (header rows are skipped). If
            // this upsert matches the name the user just typed,
            // focus the row + auto-mount the new-workspace input so
            // they can keep typing without re-aiming.
            if self.deferred_focus_project.as_deref() == Some(project.name.as_str()) {
                self.deferred_focus_project = None;
                let project_key = project.key;
                if self.sidebar.focus_project_header(&project_key) {
                    self.mount_new_workspace_input(project_key);
                }
            }
            self.redraw = true;
            return;
        }
        if let IpcEvent::ProjectRemoved(key) = &event {
            self.projects.remove(key);
            self.sidebar.apply_projects(self.projects.clone());
            // The cascade the daemon confirmed reconciles any optimistic
            // project removal (#476).
            self.reconcile_optimistic(key.as_str());
            self.redraw = true;
            return;
        }
        // Snapshot's project list seeds the same map on reconnect.
        // Push to the sidebar AFTER the snapshot's WorkspaceUpserted-
        // equivalent rows are processed below, so the first render
        // already has both layers.
        if let IpcEvent::Snapshot { projects, .. } = &event {
            // The snapshot is authoritative for daemon-known projects, so
            // drop any that vanished while the client was disconnected
            // (out-of-process / SSH). Keep locally-synthesized projects —
            // they never appear in the snapshot and are reconciled by the
            // workspace sync.
            let snapshot_keys: std::collections::HashSet<_> =
                projects.iter().map(|p| p.key.clone()).collect();
            let synthesized = &self.synthesized_projects;
            self.projects
                .retain(|k, _| snapshot_keys.contains(k) || synthesized.contains(k));
            for p in projects {
                let project = p.clone();
                self.projects.insert(project.key.clone(), project.clone());
                self.synthesized_projects.remove(&project.key);
            }
            self.sidebar.apply_projects(self.projects.clone());
        }

        // The daemon owns the snippet MRU and the dismissed-update set
        // (#548). Seed both from every snapshot so in-process and
        // `--connect` clients share one persisted view, then re-evaluate a
        // stashed update against the now-known dismissal set.
        if let IpcEvent::Snapshot {
            recent_snippets,
            dismissed_updates,
            ..
        } = &event
        {
            self.dismissed_updates = dismissed_updates.clone();
            self.seed_recent_snippets_from_snapshot(recent_snippets.clone());
            self.snapshot_seen = true;
            self.maybe_show_pending_update();
        }

        // A broadcast-lag recovery `Snapshot` stands in for the events
        // the client missed — which can include the one-shot
        // `TerminalSpawned` that dismisses an in-flight
        // worktree-provisioning checklist, AND the per-stage
        // `WorktreeProgress` updates that would have advanced it. Left
        // unhandled the checklist hangs forever on whatever step it last
        // saw. Rather than special-case it here with an abrupt teardown,
        // let the snapshot's terminals register below and reconcile it at
        // the tail of this function via
        // `reconcile_worktree_progress_with_terminals`: that *queues* a
        // graceful dismiss, so the checklist still walks its remaining
        // stages for their minimum dwell before closing instead of
        // flashing a single half-step. A failed checklist is left up
        // (reconcile skips it) so the user can still read its error.

        let is_snapshot = matches!(&event, IpcEvent::Snapshot { .. });
        let is_spawn = matches!(
            &event,
            IpcEvent::TerminalSpawned { .. } | IpcEvent::TerminalFocusRequested { .. }
        );

        // Out-of-scope workspaces with running terminals — queue a
        // Confirm prompt before killing anything. Don't forward the
        // event to panes; they'd just ignore it anyway and a queued
        // prompt is the only reasonable response.
        if let IpcEvent::WorkspaceOutOfScope {
            workspace_key,
            label,
            title,
            active_terminal_count,
        } = &event
        {
            // Dedupe: ignore re-emits for the workspace currently
            // being prompted about OR already queued. The daemon
            // dedupes per-process, but a daemon restart would reset
            // its state and could spam the same prompt. Belt and
            // braces.
            if !self.removal_already_pending(workspace_key)
                && !self.dismissed_removal_prompts.contains(workspace_key)
            {
                self.removal_prompt_queue.push_back(super::RemovalPrompt {
                    workspace_key: workspace_key.clone(),
                    label: label.clone(),
                    title: title.clone(),
                    terminal_count: *active_terminal_count,
                    reason: super::RemovalReason::OutOfScope,
                    has_local_work: false,
                });
                self.maybe_mount_next_removal_prompt();
                self.redraw = true;
            }
            return;
        }
        // Same pattern for issue→PR merge prompts: queue + surface
        // one at a time so the modal stack doesn't pile up.
        if let IpcEvent::WorkspaceMergePending {
            issue_workspace_key,
            pr_workspace_key,
            issue_label,
            pr_label,
            active_terminal_count,
        } = &event
        {
            let already_active = matches!(
                &self.modal_flow,
                Some(ModalFlow::MergePrompt { issue, .. }) if issue == issue_workspace_key
            );
            let already_queued = self
                .merge_prompt_queue
                .iter()
                .any(|(i, _, _, _, _)| i == issue_workspace_key);
            if !already_active && !already_queued {
                self.merge_prompt_queue.push_back((
                    issue_workspace_key.clone(),
                    pr_workspace_key.clone(),
                    issue_label.clone(),
                    pr_label.clone(),
                    *active_terminal_count,
                ));
                self.maybe_mount_next_merge_prompt();
                self.redraw = true;
            }
            return;
        }
        // Silent-merge notice: the daemon collapsed an issue row into
        // its PR without prompting (no live sessions to worry about).
        // Flash a footer line so the row disappearance has context.
        if let IpcEvent::WorkspaceMerged {
            issue_workspace_key,
            pr_workspace_key,
            issue_label,
            pr_label,
        } = &event
        {
            self.flash_info(format!("joined {issue_label} into {pr_label}"));
            // If the user was viewing the issue workspace that just got
            // absorbed, follow the moved sessions onto the PR workspace
            // so they don't land on an arbitrary row with the session
            // seemingly gone. `WorkspaceRemoved` (handled above, earlier
            // in the event stream) recorded the viewed key.
            if self.merge_follow_from.take().as_ref() == Some(issue_workspace_key) {
                let pr_key: lazybox_core::SessionKey = pr_workspace_key.into();
                if self.sidebar.focus_workspace_key(&pr_key) {
                    self.needs_pane_sync = true;
                }
            }
            self.redraw = true;
            return;
        }
        // `g m` completed: GitHub accepted the merge. Latch the key so
        // the MERGED state is authoritative from here on (any interim
        // poll still reporting `Open` is patched back to MERGED at ingest
        // — see `apply_merge_latch`), then flip both panes' stored copies
        // IMMEDIATELY so the badge pill and the right-pane header change
        // now instead of waiting up to a poll cycle (~30s) for the visual
        // to catch up. Refresh still goes out so the next poll backfills
        // everything else and eventually confirms + releases the latch.
        if let IpcEvent::PrMerged {
            pr_label,
            workspace_key,
            ..
        } = &event
        {
            self.merge_confirmed.insert(workspace_key.clone());
            self.sidebar.mark_workspace_merged(workspace_key);
            self.right.mark_workspace_merged(workspace_key);
            // A prior "merge failed" for this workspace no longer
            // describes reality — clear it so the success can show (#588).
            self.clear_action_error(workspace_key);
            self.flash_info(format!("merged {pr_label}"));
            // The removal prompt is NOT queued here. Both user-initiated
            // (`g m`) and externally-merged PRs are surfaced by the
            // daemon's open→merged transition, which emits a single
            // `MergedPrRemovable` (with worktree-safety info this event
            // lacks). The `Refresh` below wakes that poll so the prompt
            // follows within a few seconds.
            self.send_cmd(IpcCommand::Refresh);
            self.redraw = true;
            return;
        }
        // `g m` reached GitHub and was rejected — the merge did NOT
        // happen. Surface a distinct, persistent error (Permanent
        // severity → no auto-fade) naming the reason so it reads as
        // "your merge failed," not a transient sync blip. The PR stays
        // Open/actionable; no optimistic MERGED flip here.
        if let IpcEvent::PrMergeFailed {
            workspace_key,
            pr_label,
            reason,
            conflict,
        } = &event
        {
            if *conflict {
                // Conflict is a branch in the road, not a wall (#947):
                // offer the one-key resolve flow instead of a dead-end
                // red error. The daemon corrected the cached mergeable
                // state before this event, so the CONFLICT pill is
                // already accurate.
                self.mount_conflict_resolve(workspace_key, pr_label);
            } else {
                self.flash_action_error(
                    workspace_key,
                    action_failure_notice("merge", pr_label, reason),
                );
            }
            self.redraw = true;
            return;
        }
        // `g u` / `Shift-U` reached GitHub and the branch was updated.
        // The BEHIND tag clears on the next poll (which the handler
        // woke), so flash a notice now so the keypress reads as done.
        if let IpcEvent::BranchUpdated {
            workspace_key,
            pr_label,
            ..
        } = &event
        {
            self.clear_action_error(workspace_key);
            self.flash_info(format!("updated branch {pr_label}"));
            self.redraw = true;
            return;
        }
        // `g u` reached GitHub and was rejected (conflict, permissions)
        // — the update did NOT happen. Persistent error naming the
        // reason, mirroring `PrMergeFailed`. The PR stays actionable.
        if let IpcEvent::BranchUpdateFailed {
            workspace_key,
            pr_label,
            reason,
        } = &event
        {
            self.flash_action_error(
                workspace_key,
                action_failure_notice("update branch", pr_label, reason),
            );
            self.redraw = true;
            return;
        }
        // `x c` reached GitHub and the issue was closed. The local
        // Task still reads `Open` until the next poll, so flash a notice
        // now; the daemon's open→closed detection (which the close
        // handler woke) follows with the workspace-removal prompt.
        if let IpcEvent::IssueClosed {
            workspace_key,
            issue_label,
        } = &event
        {
            self.clear_action_error(workspace_key);
            self.flash_info(format!("closed {issue_label}"));
            self.redraw = true;
            return;
        }
        // `x c` reached GitHub and was rejected — the close did NOT
        // happen. Surface a distinct, persistent error naming the reason
        // (mirrors `PrMergeFailed`). The issue stays Open/actionable.
        if let IpcEvent::IssueCloseFailed {
            workspace_key,
            issue_label,
            reason,
        } = &event
        {
            self.flash_action_error(
                workspace_key,
                action_failure_notice("close", issue_label, reason),
            );
            self.redraw = true;
            return;
        }
        // `g d` reached GitHub and the PR was closed without merging.
        // Same "flash now, poll reconciles later" contract as the
        // merge/close notices; the rescope sweep retires the row.
        if let IpcEvent::PrClosed {
            workspace_key,
            pr_label,
        } = &event
        {
            self.clear_action_error(workspace_key);
            self.flash_info(format!("closed {pr_label}"));
            self.redraw = true;
            return;
        }
        // `g d` reached GitHub and the issue is gone — hard-deleted, or
        // (when the token lacked the admin rights a delete needs) closed
        // as not-planned. Name the degradation so "delete" never
        // silently means "closed, still exists."
        if let IpcEvent::IssueDeleted {
            workspace_key,
            issue_label,
            fell_back_to_close,
        } = &event
        {
            self.clear_action_error(workspace_key);
            if *fell_back_to_close {
                self.flash_error(format!(
                    "delete not permitted — closed {issue_label} as not-planned instead"
                ));
            } else {
                self.flash_info(format!("deleted {issue_label}"));
            }
            self.redraw = true;
            return;
        }
        // `g d` reached GitHub and was rejected — nothing was deleted
        // or closed. Persistent error naming the reason, mirroring
        // `PrMergeFailed` / `IssueCloseFailed`.
        if let IpcEvent::DeleteOrCloseFailed {
            workspace_key,
            label,
            reason,
        } = &event
        {
            self.flash_action_error(
                workspace_key,
                action_failure_notice("delete/close", label, reason),
            );
            self.redraw = true;
            return;
        }
        // The daemon detected a PR merge or an issue close and wants the
        // user to decide whether to remove the workspace + delete its
        // worktree. Queue it onto the shared removal-prompt machinery
        // (reason `Merged` / `Closed`, which differ only in copy),
        // deduped against any already-active/queued prompt.
        if let IpcEvent::MergedPrRemovable {
            workspace_key,
            label,
            terminal_state,
            active_terminal_count,
            has_local_work,
        } = &event
        {
            if !self.removal_already_pending(workspace_key)
                && !self.dismissed_removal_prompts.contains(workspace_key)
            {
                let reason = match terminal_state {
                    lazybox_ipc::RemovableTerminalState::Merged => super::RemovalReason::Merged,
                    lazybox_ipc::RemovableTerminalState::Closed => super::RemovalReason::Closed,
                };
                self.removal_prompt_queue.push_back(super::RemovalPrompt {
                    workspace_key: workspace_key.clone(),
                    label: label.clone(),
                    title: None,
                    terminal_count: *active_terminal_count,
                    reason,
                    has_local_work: *has_local_work,
                });
                self.maybe_mount_next_removal_prompt();
                self.redraw = true;
            }
            return;
        }
        // A closed issue reopened before its removal was acted on
        // (#552): drop any queued or mounted "remove closed issue?"
        // prompt for it — the workspace is alive again.
        if let IpcEvent::RemovalCancelled { workspace_key } = &event {
            self.dismissed_removal_prompts.remove(workspace_key);
            self.cancel_removal_prompt(workspace_key);
            self.redraw = true;
            return;
        }
        // Clear the lazy-fetch dedupe entry when a workspace is
        // removed, so a re-added workspace (e.g. user re-checks a
        // filter) gets a fresh details fetch on next focus.
        if let IpcEvent::WorkspaceRemoved(key) = &event {
            // The daemon confirmed the removal — reconcile any optimistic
            // archive/delete of this row (#476).
            self.reconcile_optimistic(key.as_str());
            // A removal prompt may only exist for a LIVE workspace. One
            // can sit queued behind another modal while the user archives
            // or deletes the row; without this, dismissing that modal
            // later mounts "<PR> was merged — remove workspace?" for a
            // workspace that no longer exists (#NNN). Deletion retracts
            // the prompt — the same cancel the reopen path (#552) uses.
            self.cancel_removal_prompt(key);
            // Beyond the removal *prompt*, any other modal opened FOR this
            // workspace (reply, notes, reviewers, assignees, snooze,
            // policies, conflict-resolve, sidebar context, adopt, the setup
            // checklist) would otherwise linger as an orphan pointing at a
            // PR/issue that no longer exists — dismiss it too.
            self.dismiss_modals_for_removed_workspace(key);
            self.pr_details_fetched.remove(key);
            // Drop the confirmed-merge latch — the row is gone, so a
            // stale entry could only leak or mis-patch a re-added key.
            self.merge_confirmed.remove(key);
            // Drop any Activity-pane visibility override so a re-added
            // workspace re-applies the empty-aware default instead of a
            // stale manual choice.
            self.activity_pane.forget(key);
            // Forget the remembered pane focus too, so a re-added
            // workspace doesn't inherit a stale restore target.
            let session_key: lazybox_core::SessionKey = key.into();
            self.workspace_focus.remove(&session_key);
            // Drop the remote-box latch — same leak/mis-patch argument as
            // `merge_confirmed` above: a re-added key must not inherit a
            // stale `⇅` tag.
            self.remote_marks.remove(&session_key);
            // Drop the autonomous-spawn notice marker so a re-added
            // workspace's next auto-spawn announces again, and a spawn
            // that never reached a live terminal can't leak the entry
            // (issue #645).
            self.autonomous_spawn_notified.remove(&session_key);
            // Capture this BEFORE the sidebar (below) moves the cursor
            // off the now-gone row: if the user was viewing the
            // workspace being removed, a trailing `WorkspaceMerged`
            // should follow the moved sessions onto the PR workspace.
            if self.sidebar.selected_workspace_key().map(|k| k.as_str()) == Some(key.as_str()) {
                self.merge_follow_from = Some(key.clone());
            }
        }
        // Response to a `FetchRepoLabels` command — mount the picker
        // once the daemon has the repo's label set. We tolerate
        // out-of-band events (e.g. a stale fetch firing after the
        // user dismissed the picker) by only mounting when the
        // workspace key still matches the pending request.
        if let IpcEvent::RepoLabels {
            workspace_key,
            labels,
        } = &event
        {
            if self.awaiting_repo_labels.as_ref() == Some(workspace_key) {
                self.mount_manage_labels(workspace_key.clone(), labels.clone());
                self.redraw = true;
            }
            return;
        }
        // Response to a `FetchRequestableReviewers` command — mount the
        // reviewer picker once the daemon has the repo's requestable
        // set. Same out-of-band tolerance as `RepoLabels`: only mount
        // when the workspace key still matches the pending request.
        if let IpcEvent::RequestableReviewers {
            workspace_key,
            logins,
        } = &event
        {
            if self.awaiting_requestable_reviewers.as_ref() == Some(workspace_key) {
                self.mount_request_reviewers(workspace_key.clone(), logins.clone());
                self.redraw = true;
            }
            return;
        }
        // Resource-posture reply (2026-08-19 audit) — repaint the open
        // Shift-D window. Dropped by `update_sync_status_posture` when
        // the window is already closed.
        if let IpcEvent::ResourcePosture(posture) = &event {
            self.update_sync_status_posture(posture.clone());
            self.redraw = true;
            return;
        }
        // Durable Error Inbox snapshot (#831) — repaint the open inbox.
        // A snapshot that lands while the inbox is closed is dropped by
        // `update_error_inbox`.
        if let IpcEvent::ErrorInbox { errors } = &event {
            self.update_error_inbox(errors.clone());
            self.redraw = true;
            return;
        }
        // First-time worktree provisioning progress. A user-initiated
        // spawn drives the spinner + step checklist modal; an autonomous
        // (GitHub label / `@lazybox` mention) spawn is background work
        // the user didn't ask for, so it reports a footer notice instead
        // of stealing focus with a modal (issue #645). Either way the
        // panes ignore it, so handle + return here rather than fan out.
        // The matching `TerminalSpawned` below dismisses the modal once
        // ready.
        if let IpcEvent::WorktreeProgress {
            session_key,
            step,
            status,
            origin,
        } = &event
        {
            self.route_worktree_progress(session_key.clone(), *step, status.clone(), *origin);
            // The sidebar folds the same progress into its per-row
            // "spawning" arc (#1069) — the modal/footer isn't the only
            // surface anymore, so the row shows the spawn is coming.
            self.sidebar.on_daemon_event(&event);
            self.redraw = true;
            return;
        }
        self.sidebar.on_daemon_event(&event);
        // Surface Active→Asking transitions in the footer with a
        // brief Hint-severity notice. The sidebar already pushed an
        // OS notification + flipped its `?` glyph; this is the
        // in-lazybox equivalent for users running with notifications
        // muted. Last one wins if multiple workspaces transition
        // in the same tick — they'll see them in sequence anyway as
        // the 3s Hint fade clears each.
        if let Some(msg) = self.sidebar.drain_pending_asking_notices().pop() {
            self.flash_hint(msg);
        }
        // NOTE: "auto-merge on green" is fired by the DAEMON's polling
        // commit path (`polling::auto_merge`) — the client no longer
        // triggers merges, so a headless daemon fires it and N attached
        // clients can't double-fire it.
        if let IpcEvent::WorkspaceUpserted(ws) = &event {
            // The daemon's fresh copy is authoritative — reconcile any
            // optimistic chip edit (reviewers/assignees/labels) on this
            // workspace (#476).
            self.reconcile_optimistic(ws.key.as_str());
            let resume = self
                .pending_hopper_action
                .as_ref()
                .is_some_and(|(key, _)| key == &ws.key && ws.project_key.is_some());
            if resume && let Some((_, action)) = self.pending_hopper_action.take() {
                let commands = self.dispatch_action(&action);
                self.dispatch_cmds(commands);
                self.flash_info("repo assigned — starting workspace");
            }
        }
        self.right.on_daemon_event(&event);
        self.terminals.on_daemon_event(&event);
        match &event {
            IpcEvent::AgentAuthRequired {
                terminal_id,
                display_name,
                other_session_count,
                credentials_isolated,
                ..
            } => {
                self.queue_agent_auth_prompt(super::AgentAuthPrompt {
                    terminal_id: *terminal_id,
                    display_name: display_name.clone(),
                    other_session_count: *other_session_count,
                    credentials_isolated: *credentials_isolated,
                    retry: false,
                    error: None,
                });
            }
            IpcEvent::AgentAuthProgress { phase, .. } => {
                let message = match phase {
                    lazybox_ipc::AgentAuthPhase::LoggingOut => "signing out of the provider…",
                    lazybox_ipc::AgentAuthPhase::LoginInteractive => {
                        "complete sign-in in the terminal…"
                    }
                    lazybox_ipc::AgentAuthPhase::Resuming => {
                        "sign-in complete — resuming conversation…"
                    }
                };
                self.flash_info(message);
            }
            IpcEvent::AgentAuthFinished {
                recovery_terminal_id,
                terminal_id: _,
                display_name,
                success,
                error,
            } => {
                if *success {
                    self.flash_info(format!("{display_name} conversation resumed"));
                    self.set_focus(PaneFocus::Terminals);
                } else {
                    self.queue_agent_auth_prompt(super::AgentAuthPrompt {
                        terminal_id: *recovery_terminal_id,
                        display_name: display_name.clone(),
                        other_session_count: 0,
                        credentials_isolated: false,
                        retry: true,
                        error: error.clone(),
                    });
                }
            }
            IpcEvent::AgentResumeFallback { display_name, .. } => {
                self.flash_info(format!(
                    "no exact {display_name} session id is available — resuming the latest conversation in this checkout"
                ));
            }
            _ => {}
        }
        self.flush_pending_terminal_resyncs();
        if matches!(&event, IpcEvent::TerminalResyncUnavailable { .. }) {
            self.flash(
                "terminal output paused — authoritative replay unavailable; retrying",
                crate::realm::components::footer::NoticeSeverity::Retryable,
            );
        }
        if let Some(p) = self.status.polling.as_mut() {
            p.feed_daemon_event(&event);
        }
        let poll_failed = if let IpcEvent::GithubRateLimitWait {
            remaining,
            limit,
            reset_at,
        } = &event
        {
            self.status
                .note_github_rate_limit_wait(*remaining, *limit, *reset_at);
            self.pending_refresh_ack = false;
            self.redraw = true;
            false
        } else if let IpcEvent::ProviderError { source, .. } = &event {
            self.status.note_poll_failed(source)
        } else if matches!(
            &event,
            IpcEvent::PollProgress { source, .. } | IpcEvent::PollCompleted { source, .. }
                if source == "github"
        ) {
            self.status.github_rate_limit_wait = None;
            false
        } else {
            false
        };
        // Durable sync-attempt log feeding the sync-status window.
        // Recorded for every cycle regardless of whether the polling
        // modal / footer spinner is up — those are transient, this is
        // the session-scoped history. Spawn failures also arrive as
        // `ProviderError` (with a `spawn*` source) but aren't sync
        // attempts, so they're filtered out.
        match &event {
            IpcEvent::PollCompleted { source, count } => {
                self.status.sync.note_completed(source, *count);
            }
            IpcEvent::GithubRateLimitWait {
                remaining,
                limit,
                reset_at,
            } => {
                self.status
                    .sync
                    .note_rate_limited(*remaining, *limit, *reset_at);
            }
            IpcEvent::ProviderError {
                source,
                message,
                detail,
                kind,
            } if !source.starts_with("spawn") => {
                self.status.sync.note_error(source, kind, message, detail);
            }
            // Spawn-sourced errors fall through the guard above —
            // they're spawn failures, not sync attempts.
            IpcEvent::ProviderError { .. } => {}
            // Deliberately ignored: not sync-attempt outcomes.
            // Exhaustive on purpose — a new Event variant must be
            // classified here before this compiles.
            IpcEvent::Snapshot { .. }
            | IpcEvent::ViewerIdentities { .. }
            | IpcEvent::AutoFixPolicyConfig { .. }
            | IpcEvent::ShellCommandConfig { .. }
            | IpcEvent::AgentAvailabilityConfig { .. }
            | IpcEvent::WorkspaceUpserted(_)
            | IpcEvent::WorkspaceRemoved(_)
            | IpcEvent::ProjectUpserted(_)
            | IpcEvent::ProjectRemoved(_)
            | IpcEvent::WorkspaceOutOfScope { .. }
            | IpcEvent::WorkspaceMergePending { .. }
            | IpcEvent::WorkspaceMerged { .. }
            | IpcEvent::PrMerged { .. }
            | IpcEvent::PrMergeFailed { .. }
            | IpcEvent::BranchUpdated { .. }
            | IpcEvent::BranchUpdateFailed { .. }
            | IpcEvent::IssueClosed { .. }
            | IpcEvent::IssueCloseFailed { .. }
            | IpcEvent::PrClosed { .. }
            | IpcEvent::IssueDeleted { .. }
            | IpcEvent::DeleteOrCloseFailed { .. }
            | IpcEvent::MergedPrRemovable { .. }
            | IpcEvent::RemovalCancelled { .. }
            | IpcEvent::RepoLabels { .. }
            | IpcEvent::RequestableReviewers { .. }
            | IpcEvent::SessionCreated(_)
            | IpcEvent::WorktreeProgress { .. }
            | IpcEvent::SessionEnded { .. }
            | IpcEvent::TerminalSpawned { .. }
            | IpcEvent::TerminalReplaced { .. }
            | IpcEvent::TerminalOutput { .. }
            | IpcEvent::AgentAuthOutput { .. }
            | IpcEvent::AgentAuthReplay { .. }
            | IpcEvent::TerminalResync { .. }
            | IpcEvent::TerminalResyncUnavailable { .. }
            | IpcEvent::TerminalDelta { .. }
            | IpcEvent::TerminalDeltaUnavailable { .. }
            | IpcEvent::TerminalScrollback { .. }
            | IpcEvent::TerminalExited { .. }
            | IpcEvent::TerminalFocusRequested { .. }
            | IpcEvent::WorkspaceFocusRequested { .. }
            | IpcEvent::TerminalsRebadged { .. }
            | IpcEvent::AgentState { .. }
            | IpcEvent::PollProgress { .. }
            | IpcEvent::Notification { .. }
            | IpcEvent::CleanWorktreesCompleted { .. }
            | IpcEvent::WorktreesInspected { .. }
            | IpcEvent::WorkspaceDiffInspected { .. }
            | IpcEvent::CheckoutsDiscovered { .. }
            | IpcEvent::OrphanedWorktreeDeleted { .. }
            | IpcEvent::AgentRunStarted { .. }
            | IpcEvent::AgentRunStartFailed { .. }
            | IpcEvent::AgentRawJson { .. }
            | IpcEvent::AgentDebug { .. }
            | IpcEvent::AgentAssistantTextDelta { .. }
            | IpcEvent::AgentToolCallStarted { .. }
            | IpcEvent::AgentToolCallDelta { .. }
            | IpcEvent::AgentToolCallFinished { .. }
            | IpcEvent::AgentPermissionRequest { .. }
            | IpcEvent::AgentUserQuestion { .. }
            | IpcEvent::AgentUsage { .. }
            | IpcEvent::AgentSessionUsage { .. }
            | IpcEvent::AgentProviderQuota { .. }
            | IpcEvent::AgentTurnFinished { .. }
            | IpcEvent::AgentRunFinished { .. }
            | IpcEvent::ProviderCredentialUpdated { .. }
            | IpcEvent::ProviderCredentialRemoved { .. }
            | IpcEvent::ProviderCredentialsListed { .. }
            | IpcEvent::TerminalInputRejected { .. }
            | IpcEvent::CommandRejected { .. }
            | IpcEvent::CommandCompleted { .. }
            | IpcEvent::CommandFailed { .. }
            | IpcEvent::AgentCliUpdatesChecked { .. }
            | IpcEvent::AgentCliUpdateFinished { .. }
            | IpcEvent::SnippetDelivered { .. }
            | IpcEvent::AgentAuthRequired { .. }
            | IpcEvent::AgentAuthProgress { .. }
            | IpcEvent::AgentAuthFinished { .. }
            | IpcEvent::AgentResumeFallback { .. }
            | IpcEvent::TerminalModelChanged { .. }
            | IpcEvent::RecoveredTerminalsRequireRestart { .. }
            | IpcEvent::AgentUsageLimit { .. }
            | IpcEvent::AgentCreditRecovery { .. }
            | IpcEvent::AgentCreditExhausted { .. }
            | IpcEvent::WorkspaceCreated { .. }
            | IpcEvent::ErrorInbox { .. }
            | IpcEvent::ResourcePosture(..) => {}
        }
        // Background-poll indicator. Lights up whenever the daemon
        // emits PollProgress (any cycle, initial or not); clears on
        // PollCompleted. Visible only after the initial Polling modal
        // is gone — the modal already shows its own (richer) spinner
        // and we don't want two indicators flashing at once.
        if self.status.polling.is_none() {
            match &event {
                IpcEvent::PollProgress { source, message } => {
                    self.status.note_poll_progress(source, message);
                    self.redraw = true;
                }
                IpcEvent::PollCompleted { source, count } => {
                    self.status.note_poll_completed(source);
                    // Consume the manual-refresh ack with a clear
                    // success footer notice. Auto-cycles (the user
                    // didn't ask for them) stay silent.
                    if self.pending_refresh_ack {
                        self.pending_refresh_ack = false;
                        // Release the recovered provider's sticky
                        // "✗ sync failed" banner FIRST — the
                        // severity-aware `flash` refuses to let an
                        // Info displace a live Permanent, so without
                        // this the "✓ sync ok" would be routed to the
                        // log and the stale red banner would stay up.
                        self.clear_sync_error_if_recovered(source);
                        self.flash_info(format!("✓ sync ok — {count} tasks from {source}"));
                    } else {
                        // Sync recovered on an auto-cycle: if a sticky
                        // "✗ sync failed" banner for *this* provider is
                        // up, clear it now that its poll succeeded —
                        // leaving it would falsely imply sync is still
                        // broken. A banner for a different provider is
                        // left intact.
                        self.clear_sync_error_if_recovered(source);
                    }
                    self.redraw = true;
                }
                IpcEvent::ProviderError {
                    source,
                    message,
                    kind,
                    ..
                } => {
                    // A spawn failed (worktree provisioning, unknown
                    // agent id, …) — `handle_spawn` reports these with
                    // a `spawn*` source. No `TerminalSpawned` will ever
                    // arrive, so clear the spinner now instead of
                    // leaving it to time out on the guard, and surface
                    // why so the user isn't left guessing.
                    if source.starts_with("spawn") {
                        if self.status.clear_spawning() {
                            self.redraw = true;
                        }
                        // Drop any pending spawn-follow pin — leaving it
                        // armed would let a later unrelated spawn inherit
                        // this `w`'s follow.
                        self.spawn_follow_to = None;
                        // A worktree-provisioning failure already mounted
                        // the actionable checklist modal (classified
                        // error + hint + `r` retry, issue #557). That is
                        // the recovery surface — do NOT tear it down into
                        // a single truncated footer line (the exact
                        // regression this issue reported). The redundant
                        // provider-error footer is suppressed; the modal
                        // owns the failure.
                        let modal_owns_failure =
                            self.worktree_progress.as_ref().is_some_and(|s| s.failed());
                        if !modal_owns_failure {
                            // No live checklist caught the `Failed` step
                            // (retry, fast spawn, or a dismissed
                            // checklist), yet a worktree-provisioning
                            // failure — which the daemon labels
                            // `spawn:worktree` — must still reach the
                            // recovery modal rather than leak to a
                            // middle-truncated footer line that elides the
                            // recovery text (#594). Other spawn errors
                            // (unknown agent, backend spawn, target-moved
                            // race) stay on the footer as before — a retry
                            // there wouldn't help, so the daemon marks them
                            // with a different source.
                            let routed = source == "spawn:worktree"
                                && self.route_spawn_failure_to_recovery(message);
                            if !routed {
                                // Tear down only a stale checklist for
                                // *this* failing spawn — never a concurrent
                                // spawn's live checklist (finding 3).
                                if !self.worktree_checklist_is_foreign_and_live() {
                                    self.worktree_progress_dismissed = None;
                                    self.force_dismiss_worktree_progress();
                                }
                                // A spawn failure is an action result, not
                                // persistent system state. Keep the complete
                                // text in Shift-M's messages log but let the
                                // footer toast fade; otherwise one failed
                                // attempt pins the footer forever even after
                                // the user fixes the checkout or successfully
                                // starts from another workspace.
                                self.flash(
                                    format!("✗ spawn failed — {message}"),
                                    crate::realm::components::footer::NoticeSeverity::Retryable,
                                );
                            }
                        }
                    } else if source == "repo-labels" {
                        self.handle_repo_labels_failed(message);
                    } else if source == "requestable-reviewers" {
                        self.handle_requestable_reviewers_failed(message);
                    } else if let Some(action) = mutation_failure_label(source) {
                        // A user-initiated GitHub mutation was rejected
                        // (or never reached the provider). Pre-fix the
                        // client had already flashed optimistic success
                        // ("requested N reviewer(s)", "set labels…") at
                        // command-send time and the rejection went to
                        // the Shift-D sync log only — the user believed
                        // the mutation landed. Surface it like
                        // `PrMergeFailed`: a Permanent, named error.
                        if source == "reply"
                            && let Some(body) = self.last_reply_body.take()
                        {
                            // The compose textarea was consumed on
                            // submit — park the lost text in the
                            // durable messages log so it's recoverable.
                            self.status.messages.record(
                                &format!("unsent reply text: {body}"),
                                crate::realm::components::footer::NoticeSeverity::Info,
                            );
                            self.flash_error(format!(
                                "✗ reply failed — {message} · your text is in the \
                                 messages log (Shift-M)"
                            ));
                        } else {
                            self.flash_error(format!("✗ {action} failed — {message}"));
                        }
                        // Revert the optimistic chip edit (#476). No-op
                        // for sources that don't carry one (reply / merge
                        // / close-issue), so the flash above still stands.
                        self.rollback_optimistic_chip(source);
                    } else if matches!(source.as_str(), "store" | "terminal")
                        && self.rollback_optimistic_removal(message)
                    {
                        // An optimistic archive/delete the daemon
                        // rejected: the row (and, for a project, its
                        // children) was removed locally, so re-insert it
                        // and surface why (#476). Delete failures arrive
                        // as `store` (archive/db) or `terminal` (a backing
                        // agent that couldn't be stopped) errors naming the
                        // key; one naming no pending removal keeps its
                        // quiet sync-log-only handling.
                        self.flash_error(format!("✗ delete failed — {message}"));
                    } else if self.pending_refresh_ack || poll_failed {
                        // A genuine sync-poll failure — reached only when
                        // the branches above did NOT already own this error
                        // (spawn / mutation / repo-labels / optimistic
                        // rollback), so an actionable mutation rejection can
                        // never be silently swallowed by the transient path
                        // below. A failed in-flight poll owns an explicit
                        // footer state until that provider recovers; manual
                        // refreshes use the same state even if their
                        // progress event was missed.
                        //
                        // A `retryable` transient is self-healing — the
                        // daemon is still auto-retrying — so it must NOT
                        // raise the red "✗ sync failed" banner that buries
                        // the errors the user must actually act on (#730).
                        // Only an actionable failure (retries `exhausted`,
                        // auth, permanent) escalates to the banner; a live
                        // transient gives calm, auto-fading feedback on an
                        // explicit refresh and stays silent on background
                        // cycles (the attempt is still in the Shift-D sync
                        // log either way).
                        let was_manual = std::mem::take(&mut self.pending_refresh_ack);
                        if lazybox_ipc::ProviderErrorKind::from_wire(kind).is_actionable() {
                            self.flash_sync_error(
                                source,
                                format!("✗ sync failed — {source}: {message}"),
                            );
                        } else if was_manual {
                            self.flash(
                                format!("⟳ {message}"),
                                crate::realm::components::footer::NoticeSeverity::Retryable,
                            );
                        }
                    }
                }
                // Deliberately ignored: no poll-indicator / mutation-
                // failure semantics. Exhaustive on purpose — a new
                // Event variant must be classified here before this
                // compiles.
                IpcEvent::Snapshot { .. }
                | IpcEvent::ViewerIdentities { .. }
                | IpcEvent::AutoFixPolicyConfig { .. }
                | IpcEvent::ShellCommandConfig { .. }
                | IpcEvent::AgentAvailabilityConfig { .. }
                | IpcEvent::WorkspaceUpserted(_)
                | IpcEvent::WorkspaceRemoved(_)
                | IpcEvent::ProjectUpserted(_)
                | IpcEvent::ProjectRemoved(_)
                | IpcEvent::WorkspaceOutOfScope { .. }
                | IpcEvent::WorkspaceMergePending { .. }
                | IpcEvent::WorkspaceMerged { .. }
                | IpcEvent::PrMerged { .. }
                | IpcEvent::PrMergeFailed { .. }
                | IpcEvent::BranchUpdated { .. }
                | IpcEvent::BranchUpdateFailed { .. }
                | IpcEvent::IssueClosed { .. }
                | IpcEvent::IssueCloseFailed { .. }
                | IpcEvent::PrClosed { .. }
                | IpcEvent::IssueDeleted { .. }
                | IpcEvent::DeleteOrCloseFailed { .. }
                | IpcEvent::MergedPrRemovable { .. }
                | IpcEvent::RemovalCancelled { .. }
                | IpcEvent::RepoLabels { .. }
                | IpcEvent::RequestableReviewers { .. }
                | IpcEvent::SessionCreated(_)
                | IpcEvent::WorktreeProgress { .. }
                | IpcEvent::SessionEnded { .. }
                | IpcEvent::TerminalSpawned { .. }
                | IpcEvent::TerminalReplaced { .. }
                | IpcEvent::TerminalOutput { .. }
                | IpcEvent::AgentAuthOutput { .. }
                | IpcEvent::AgentAuthReplay { .. }
                | IpcEvent::TerminalResync { .. }
                | IpcEvent::TerminalResyncUnavailable { .. }
                | IpcEvent::TerminalDelta { .. }
                | IpcEvent::TerminalDeltaUnavailable { .. }
                | IpcEvent::TerminalScrollback { .. }
                | IpcEvent::TerminalExited { .. }
                | IpcEvent::TerminalFocusRequested { .. }
                | IpcEvent::WorkspaceFocusRequested { .. }
                | IpcEvent::TerminalsRebadged { .. }
                | IpcEvent::AgentState { .. }
                | IpcEvent::GithubRateLimitWait { .. }
                | IpcEvent::Notification { .. }
                | IpcEvent::CleanWorktreesCompleted { .. }
                | IpcEvent::WorktreesInspected { .. }
                | IpcEvent::WorkspaceDiffInspected { .. }
                | IpcEvent::CheckoutsDiscovered { .. }
                | IpcEvent::OrphanedWorktreeDeleted { .. }
                | IpcEvent::AgentRunStarted { .. }
                | IpcEvent::AgentRunStartFailed { .. }
                | IpcEvent::AgentRawJson { .. }
                | IpcEvent::AgentDebug { .. }
                | IpcEvent::AgentAssistantTextDelta { .. }
                | IpcEvent::AgentToolCallStarted { .. }
                | IpcEvent::AgentToolCallDelta { .. }
                | IpcEvent::AgentToolCallFinished { .. }
                | IpcEvent::AgentPermissionRequest { .. }
                | IpcEvent::AgentUserQuestion { .. }
                | IpcEvent::AgentUsage { .. }
                | IpcEvent::AgentSessionUsage { .. }
                | IpcEvent::AgentProviderQuota { .. }
                | IpcEvent::AgentTurnFinished { .. }
                | IpcEvent::AgentRunFinished { .. }
                | IpcEvent::ProviderCredentialUpdated { .. }
                | IpcEvent::ProviderCredentialRemoved { .. }
                | IpcEvent::ProviderCredentialsListed { .. }
                | IpcEvent::TerminalInputRejected { .. }
                | IpcEvent::CommandRejected { .. }
                | IpcEvent::CommandCompleted { .. }
                | IpcEvent::CommandFailed { .. }
                | IpcEvent::AgentCliUpdatesChecked { .. }
                | IpcEvent::AgentCliUpdateFinished { .. }
                | IpcEvent::SnippetDelivered { .. }
                | IpcEvent::AgentAuthRequired { .. }
                | IpcEvent::AgentAuthProgress { .. }
                | IpcEvent::AgentAuthFinished { .. }
                | IpcEvent::AgentResumeFallback { .. }
                | IpcEvent::TerminalModelChanged { .. }
                | IpcEvent::RecoveredTerminalsRequireRestart { .. }
                | IpcEvent::AgentUsageLimit { .. }
                | IpcEvent::AgentCreditRecovery { .. }
                | IpcEvent::AgentCreditExhausted { .. }
                | IpcEvent::WorkspaceCreated { .. }
                | IpcEvent::ErrorInbox { .. }
                | IpcEvent::ResourcePosture(..) => {}
            }
        }
        // CleanWorktrees finished — replace the "cleaning…" notice
        // with the final count so the user sees how much was done.
        if let IpcEvent::CleanWorktreesCompleted { removed, skipped } = &event {
            let msg = if *skipped == 0 {
                format!("cleaned {removed} worktree(s)")
            } else {
                format!("cleaned {removed} worktree(s) · kept {skipped} (active or unsafe)")
            };
            self.flash_hint(msg);
        }
        // Daemon-pushed user notice (e.g. auto-cleanup of a merged
        // PR's worktrees). Surface the body in the footer; the OS
        // banner path isn't wired for daemon-originated notices.
        if let IpcEvent::Notification { body, .. } = &event {
            self.flash_hint(body.clone());
        }
        if let IpcEvent::SnippetDelivered {
            terminal_id,
            snippet_key,
            prompt,
            ..
        } = &event
        {
            self.apply_recent_snippet(snippet_key.clone());
            if let Some(prompt) = prompt {
                self.terminals
                    .apply_delivered_prompt(*terminal_id, prompt.clone());
            }
            let tour_step = match &self.modal_flow {
                Some(ModalFlow::TourSnippet {
                    terminal,
                    success_step,
                    ..
                }) if terminal == terminal_id => Some(*success_step),
                _ => None,
            };
            if let Some(step) = tour_step {
                self.modal_flow = None;
                self.mount_tour_at(step);
            }
            self.flash_info(format!("sent snippet ]{snippet_key}"));
            self.redraw = true;
        }
        // Terminal delivery failures are retryable user-input errors, not
        // provider polling failures. Keep them out of the sync log/modal and
        // surface the exact recovery action in the footer/messages history.
        if let IpcEvent::TerminalInputRejected {
            terminal_id,
            message,
        } = &event
        {
            let tour_step = match &self.modal_flow {
                Some(ModalFlow::TourSnippet {
                    terminal,
                    return_step,
                    ..
                }) if terminal == terminal_id => Some(*return_step),
                _ => None,
            };
            if let Some(step) = tour_step {
                self.modal_flow = None;
                self.mount_tour_at(step);
            }
            self.flash(
                format!("⚠ terminal input not delivered — {message}"),
                crate::realm::components::footer::NoticeSeverity::Retryable,
            );
        }
        if let IpcEvent::CommandRejected { command, message } = &event {
            self.flash(
                format!("⚠ {command} was not accepted — {message}"),
                crate::realm::components::footer::NoticeSeverity::Retryable,
            );
        }
        // A recovered process cannot inherit a newer PTY launch environment.
        // This is terminal lifecycle state, not a provider failure: keep it
        // out of first-poll termination, sync history, and manual-refresh
        // acknowledgement handling.
        if let IpcEvent::RecoveredTerminalsRequireRestart { terminal_ids } = &event
            && !terminal_ids.is_empty()
        {
            self.note_outdated_scroll_terminals(terminal_ids);
        }
        // Out-of-band agent-CLI version check. A scheduled sweep stays
        // quiet unless something is actionable; a manual check always
        // answers, even when everything is current.
        if let IpcEvent::AgentCliUpdatesChecked { statuses, manual } = &event {
            self.note_agent_cli_updates(statuses, *manual);
        }
        // One agent's managed update finished — success and failure
        // both name the agent and the outcome, replacing the CLIs' own
        // in-session banners.
        if let IpcEvent::AgentCliUpdateFinished {
            display_name,
            ok,
            message,
            ..
        } = &event
        {
            if *ok {
                self.flash_info(format!("✓ {display_name}: {message}"));
            } else {
                self.flash_error(format!("✗ {display_name} update failed — {message}"));
            }
        }
        // Worktree inspector replied. Swap the placeholder for the
        // real list. `mount_inspect_list` is idempotent — calling it
        // again after a delete re-renders the now-shorter list, so
        // the inspector stays open across edits.
        if let IpcEvent::WorktreesInspected { inspections } = &event {
            self.mount_inspect_list(inspections.clone());
        }
        // PR-chat's diff read (#945) rides the same event but a separate
        // correlation field, so it coexists with the diff-review consumer
        // below. Once it lands, release the opening question it was holding.
        if let IpcEvent::WorkspaceDiffInspected {
            workspace_key,
            target,
            diff,
            ..
        } = &event
            && self.pr_chat_diff_target.as_ref() == Some(&(workspace_key.clone(), target.clone()))
        {
            self.pr_chat_diff_target = None;
            self.pr_chat_diff = Some(diff.clone());
            if let Some((question, _)) = self.pr_chat_held_question.take()
                && let Some(cmd) = self.start_pr_chat_run(&question)
            {
                self.send_cmd(cmd);
            }
            self.redraw = true;
        }
        if let IpcEvent::WorkspaceDiffInspected {
            workspace_key,
            target,
            agent_terminal_ids,
            diff,
            error,
        } = &event
            && self.pending_diff_session.as_ref() == Some(&(workspace_key.clone(), target.clone()))
        {
            self.pending_diff_session = None;
            match (diff, error) {
                (Some(diff), _) if self.modal_stack.is_empty() => {
                    self.mount_modal(
                        Id::DiffReview,
                        crate::realm::components::diff_review::DiffReview::new(
                            workspace_key.clone(),
                            target.clone(),
                            agent_terminal_ids.clone(),
                            diff.clone(),
                        ),
                    );
                }
                (Some(_), _) => {
                    self.flash_hint("diff is ready — close the current modal and reopen review");
                }
                (None, Some(error)) => self.flash_error(format!("couldn't read diff: {error}")),
                (None, None) => self.flash_error("couldn't read diff"),
            }
        }
        // Dev-folder scan replied. Swap the loading placeholder for the
        // import picker listing every discovered checkout.
        if let IpcEvent::CheckoutsDiscovered { checkouts } = &event {
            self.mount_import_checkout_picker(checkouts.clone());
        }
        // One row removed (or refused). Surface the outcome in the
        // footer and re-inspect so the modal's list drops the row
        // (on success) or shows fresh safety state (on refusal).
        if let IpcEvent::OrphanedWorktreeDeleted { path, ok, error } = &event {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            if *ok {
                self.flash_hint(format!("removed {name}"));
            } else {
                let why = error.as_deref().unwrap_or("refused");
                self.flash_error(format!("✗ {name}: {why}"));
            }
            // Only re-inspect when the inspector modal is open —
            // outside that flow the user doesn't expect a list to
            // appear unprompted.
            if self.modal_stack.contains(&Id::InspectList)
                || self.modal_stack.contains(&Id::InspectConfirm)
            {
                self.send_cmd(lazybox_ipc::Command::InspectWorktrees);
            }
        }
        if is_snapshot && self.preselect.is_some() {
            self.apply_preselect();
        }
        if is_spawn {
            // The session is ready — queue the provisioning checklist
            // for dismissal (unless a step failed, in which case it
            // stays up for the user to read). The modal isn't torn down
            // on the spot: it holds until every step has been shown for
            // its minimum dwell, so a fast provision walks the full
            // checklist instead of flashing only the first step. Keyed
            // by session_key so a concurrent unrelated spawn can't
            // dismiss the wrong modal.
            if let IpcEvent::TerminalSpawned { session_key, .. } = &event {
                self.queue_worktree_progress_dismiss(session_key);
            }
            // Recover the spawned terminal's (session, id) pair.
            // `TerminalFocusRequested` (singleton guard: the agent
            // already existed) carries no session key, so recover it
            // from the terminal's own slot.
            let spawned = match &event {
                IpcEvent::TerminalSpawned {
                    session_key,
                    terminal_id,
                    ..
                } => Some((session_key.clone(), *terminal_id)),
                IpcEvent::TerminalFocusRequested { terminal_id } => self
                    .terminals
                    .session_key_for(*terminal_id)
                    .map(|sk| (sk.clone(), *terminal_id)),
                _ => None,
            };
            // A terminal appeared — auto-focus the Terminals pane so
            // the user can start typing immediately, but ONLY when
            // this client actually asked for the spawn (correlated via
            // the spawn spinner / follow pin / deferred-editor stash;
            // in multi-client mode every client receives every
            // `TerminalSpawned`, and pre-fix another client's spawn
            // yanked focus out from under whatever this one was
            // doing). Never steal focus while an interactive modal is
            // mounted or the sidebar `/` search is being typed — a
            // keystroke mid-word must not land in a fresh shell. The
            // provisioning checklist is exempt from the modal guard:
            // it's a progress overlay for this very spawn, and the
            // whole point of `w` is landing in the agent behind it.
            let requested_here = spawned.as_ref().is_some_and(|(sk, _)| {
                self.status
                    .spawning
                    .as_ref()
                    .is_some_and(|sp| &sp.session_key == sk)
                    || self.spawn_follow_to.as_ref() == Some(sk)
                    || self
                        .setup
                        .pending_editor_launch
                        .as_ref()
                        .is_some_and(|(k, _)| k == sk)
                    || self
                        .setup
                        .pending_open_with_launch
                        .as_ref()
                        .is_some_and(|(k, _)| k == sk)
            });
            let interactive_modal_up = self
                .modal_stack
                .iter()
                .any(|id| *id != Id::WorktreeProgress);
            if requested_here && !interactive_modal_up && !self.sidebar.search_editing() {
                self.set_focus(PaneFocus::Terminals);
            }
            // Clear any legacy "Spawning…" footer notice that was set
            // when the matching Spawn command was sent. The animated
            // spawn *spinner* is NOT cleared here: it's a projection of
            // the live terminal set (recomputed at the tail of this
            // function via `recompute_spawn_spinner`), so a spawn event
            // for an unrelated workspace can't clear the wrong spinner
            // and a missing one can't strand it (#206).
            self.status.clear_spawning_notice();
            self.needs_pane_sync = true;
            // Pinned spawn-follow: a `w` press recorded the workspace it
            // targeted. When that workspace's terminal finally lands —
            // possibly seconds later after a cold worktree provision, and
            // possibly after the user navigated elsewhere — pull the
            // cursor back to it and mark the new terminal as the tab to
            // activate, so `w` reliably ends on the freshly-spawned agent
            // rather than wherever the cursor drifted. `deferred_focus_terminal`
            // is applied by the upcoming `sync_panes`, after
            // `set_active_session` has rebuilt the followed workspace's
            // visible terminal set.
            if let Some((spawned_key, terminal_id)) = spawned
                && self.spawn_follow_to.as_ref() == Some(&spawned_key)
            {
                self.spawn_follow_to = None;
                self.sidebar.focus_workspace_key(&spawned_key);
                self.deferred_focus_terminal = Some(terminal_id);
            }
            // Editor-deferred-by-spawn: the user pressed `e` on a
            // workspace with no worktree; we asked the daemon to
            // spawn a shell so a worktree got provisioned. Look
            // up the queued target's worktree from the sidebar's
            // workspace map (NOT `selected_workspace()`) so the
            // launch fires even if the user has since navigated
            // to a different workspace.
            if let Some((target_key, editor)) = self.setup.pending_editor_launch.clone()
                && let Some(worktree) = self
                    .sidebar
                    .workspace_by_key(&target_key)
                    .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()))
            {
                self.setup.pending_editor_launch = None;
                self.launch_editor(&editor, &worktree);
            }
            // Open-with-deferred-by-spawn (#1100): the user picked a
            // `{path}` app on a worktreeless workspace, so we spawned a
            // shell to provision one. Resolve the target by key (not the
            // cursor) and fire once its worktree exists, mirroring the
            // editor path above.
            if let Some((target_key, app)) = self.setup.pending_open_with_launch.clone() {
                let ctx = self
                    .sidebar
                    .workspace_by_key(&target_key)
                    .map(Self::open_with_context_for)
                    .filter(|ctx| ctx.path.is_some());
                if let Some(ctx) = ctx {
                    self.setup.pending_open_with_launch = None;
                    self.launch_open_with(&app, &ctx);
                }
            }
        } else {
            self.needs_pane_sync = true;
        }
        // A flagged old-build terminal that exits no longer needs its
        // "reopen to enable scrolling" warning (#544) — reopening it is
        // exactly what clears the flag, so drop it from the tracked set
        // and let the notice/focus hint fall silent on its own.
        if let IpcEvent::TerminalExited { terminal_id, .. } = &event {
            self.forget_outdated_scroll_terminal(*terminal_id);
        }
        // Focus mode needs a live terminal to fill the screen. If the
        // focused workspace's last terminal just exited, drop back to
        // the three-pane view instead of stranding the user on a
        // near-fullscreen empty pane with no hint (mirrors the
        // no-terminal fallback in `jump_to_workspace_key`).
        if self.focus_mode
            && matches!(&event, IpcEvent::TerminalExited { .. })
            && self.terminals.active_terminal_id().is_none()
        {
            self.focus_mode = false;
            if self.focus == PaneFocus::Terminals {
                self.set_focus(PaneFocus::Sidebar);
            }
            self.flash_hint("terminal exited — left focus mode");
        }
        // Same projection as the spawn spinner: if the terminal stack
        // already proves the worktree-backed session exists, queue the
        // checklist dismissal even when the specific TerminalSpawned
        // event was missed or replaced by a reconnect Snapshot (#219).
        self.reconcile_worktree_progress_with_terminals();
        // This event may have added the terminal an in-flight spawn was
        // waiting for. Recompute the spinner from the now-current
        // terminal set rather than trusting a single clear event (#206).
        self.recompute_spawn_spinner();
        self.redraw = true;
    }

    /// The daemon couldn't fetch the repo's label set for a pending
    /// `g l` request (`ProviderError { source: "repo-labels" }`).
    /// Pre-fix the server broadcast nothing on this failure and the
    /// stash stayed armed forever — the picker just never appeared.
    /// Now: consume the stash and fall back to the documented degraded
    /// picker built from the labels already on the task, so the user
    /// can still toggle/remove those; when the task carries no labels
    /// at all there's nothing to pick from, so surface a clear error
    /// instead.
    fn handle_repo_labels_failed(&mut self, message: &str) {
        let Some(workspace_key) = self.awaiting_repo_labels.take() else {
            // Not our request (another client's `g l`, or the user
            // already dismissed) — nothing to do.
            return;
        };
        // Union of the PR's + first issue's labels, deduped by name —
        // the same sources `mount_manage_labels` pre-checks.
        let mut labels: Vec<lazybox_core::Label> = Vec::new();
        if let Some(ws) = self
            .sidebar
            .workspace_iter()
            .find(|(k, _)| k.as_str() == workspace_key.as_str())
            .map(|(_, w)| w)
        {
            let mut push = |l: &lazybox_core::Label| {
                if !labels.iter().any(|e| e.name == l.name) {
                    labels.push(l.clone());
                }
            };
            if let Some(pr) = &ws.pr {
                for l in &pr.labels {
                    push(l);
                }
            }
            if let Some(issue) = ws.gh_issues.first() {
                for l in &issue.labels {
                    push(l);
                }
            }
        }
        if labels.is_empty() {
            self.flash_error(format!("✗ couldn't load repo labels — {message}"));
        } else {
            self.mount_manage_labels(workspace_key, labels);
            // The mount can refuse (another modal owns the stack —
            // see the don't-preempt guard); only advertise the
            // degraded picker when it's actually up.
            if matches!(self.modal_stack.last(), Some(Id::ManageLabels)) {
                self.flash_hint("repo labels unavailable — showing this task's labels only");
            }
        }
        self.redraw = true;
    }

    /// The daemon couldn't fetch the requestable-reviewer set for a
    /// pending `g r` request (`ProviderError { source:
    /// "requestable-reviewers" }`). Consume the stash and fall back to
    /// the interaction-derived picker (people already on the PR) —
    /// exactly today's pre-fix behavior — so the action never dead-ends
    /// on a fetch error. `mount_request_reviewers` with an empty
    /// `fetched` degrades to that candidate list on its own.
    fn handle_requestable_reviewers_failed(&mut self, message: &str) {
        let Some(workspace_key) = self.awaiting_requestable_reviewers.take() else {
            return;
        };
        // Whether the interaction-derived fallback has anyone to offer —
        // computed before the mount so the flash matches what the picker
        // actually shows. `mount_request_reviewers` clears the stash and
        // reads `modal_flow`, not the stash, so there's nothing to
        // re-arm before calling it.
        let has_participants = !self
            .gather_candidate_logins(&workspace_key, true)
            .is_empty();
        self.mount_request_reviewers(workspace_key, Vec::new());
        let mounted = matches!(self.modal_stack.last(), Some(Id::RequestReviewers));
        if mounted && has_participants {
            self.flash_hint("requestable reviewers unavailable — showing PR participants only");
        } else {
            // Either the mount was deferred (another modal owns the
            // stack) or the fallback picker is empty — there are no
            // participants to fall back to, so surface the error rather
            // than claim we're "showing participants".
            self.flash_error(format!("✗ couldn't load requestable reviewers — {message}"));
        }
        self.redraw = true;
    }

    /// Patch a workspace about to be stored/fanned-out so a confirmed
    /// merge stays MERGED. Once `Event::PrMerged` latched a key, GitHub
    /// already accepted the merge, so:
    /// - an incoming poll still showing `Open` is stale → force `Merged`
    ///   (stamp `closed_at` so the sidebar's grace window keys off it);
    /// - an incoming poll showing the terminal state (`Merged`/`Closed`)
    ///   has caught up → accept it and release the latch;
    /// - a workspace that lost its PR entirely → release the latch.
    ///
    /// Applied at ingest to every `WorkspaceUpserted` / `Snapshot`
    /// workspace, so both panes see one consistent state. No-op for
    /// un-latched keys.
    pub(super) fn apply_merge_latch(&mut self, ws: &mut lazybox_core::Workspace) {
        if !self.merge_confirmed.contains(&ws.key) {
            return;
        }
        match ws.pr.as_mut() {
            Some(pr)
                if matches!(
                    pr.state,
                    lazybox_core::TaskState::Merged | lazybox_core::TaskState::Closed
                ) =>
            {
                self.merge_confirmed.remove(&ws.key);
            }
            Some(pr) => {
                pr.state = lazybox_core::TaskState::Merged;
                if pr.closed_at.is_none() {
                    pr.closed_at = Some(chrono::Utc::now());
                }
            }
            None => {
                self.merge_confirmed.remove(&ws.key);
            }
        }
    }

    /// Re-apply the optimistic remote-box tag at ingest. The local daemon
    /// doesn't know about the box, so every `Snapshot`/`WorkspaceUpserted`
    /// it sends arrives with `remote: None` — without this latch the `⇅`
    /// glyph an `r`-spawn painted is wiped by the next poll. Mirrors
    /// [`Self::apply_merge_latch`]; the latch is released only by
    /// [`Model::unmark_remote`] (worker reported the spawn dropped).
    pub(super) fn apply_remote_latch(&mut self, ws: &mut lazybox_core::Workspace) {
        if ws.remote.is_some() {
            // A payload that already carries a remote tag is authoritative
            // (a future remote-aware daemon) — don't fight it.
            return;
        }
        let sk: lazybox_core::SessionKey = (&ws.key).into();
        if let Some(remote) = self.remote_marks.get(&sk) {
            ws.remote = Some(remote.clone());
        }
    }

    /// Auto-fade transient notices. Called once per iteration in
    /// the run loop. Severity decides the timeout:
    /// - Retryable: 5s. Hiccups self-heal, no need to linger.
    /// - Info: 15s. Spawn-progress and similar — long enough that a
    ///   slow worktree creation doesn't fade mid-flight; short
    ///   enough that a stuck notice (e.g. spawn never landed)
    ///   doesn't follow the user around forever.
    /// - Permanent / Auth: stay until dismissed (`e`).
    pub fn tick_notice(&mut self) {
        if self.status.tick_notice() {
            self.redraw = true;
        }
    }

    /// Drive the right-pane auto-mark-read timer. Called once per
    /// iteration. When the timer fires on an unread row under the
    /// cursor, the inner pane mutates its workspace state AND we
    /// ship `Command::MarkActivityRead` so the daemon persists.
    /// Without this hook the auto-mark never fires — the timer
    /// counted forever and unread badges never dropped.
    pub fn tick_right(&mut self) {
        if let Some((session_key, index, fingerprint)) = self.right.tick() {
            tracing::info!(
                %session_key,
                index,
                "auto-mark-read fired → Command::MarkActivityRead",
            );
            self.send_cmd(IpcCommand::MarkActivityRead {
                session_key,
                index,
                fingerprint: Some(fingerprint),
            });
            self.redraw = true;
        }
    }

    /// Resolve a lone held `]` once its chord window lapses. The `]]`
    /// leader itself is non-timed now (#252) — it waits for the next
    /// key rather than leaving on an idle tick, so browsing snippets
    /// never races an "exit to sidebar" — so this only handles the
    /// *first* `]` of a would-be chord that never got a second press.
    /// Called once per run-loop iteration (the loop ticks ~every 16ms
    /// even while idle).
    pub fn tick_terminal_leader(&mut self) {
        // A lone `]` that armed the chord but never saw a second press
        // (or any following key) would otherwise sit held indefinitely.
        // Once the chord window lapses, resolve it:
        //   - focus still on the terminal → release it as a literal `]`
        //     so a trailing `]` in a prompt reaches the agent;
        //   - focus has since left the terminal (the user Tabbed/clicked
        //     away while the `]` was held) → DROP it, never inject it
        //     into whichever terminal is focused next. Without this drop
        //     the held `]` would surface as a stray keystroke when focus
        //     later returns.
        if self.escape_latch.armed_past(self.ui_defaults.escape_window) {
            if self.focus == PaneFocus::Terminals {
                self.escape_latch.disarm();
                self.flush_held_escape_char();
                self.redraw = true;
            } else if self.focus == PaneFocus::Sidebar {
                // A lone `]` in the sidebar that never saw a second press
                // resolves to the snippet browser once the chord window
                // lapses (#871) — the sidebar mirror of the literal-`]`
                // flush above.
                self.escape_latch.disarm();
                self.resolve_held_sidebar_escape();
                self.redraw = true;
            } else {
                self.escape_latch.disarm();
            }
        }
    }

    /// Advance the sidebar's "working" spinner. Called once per run-
    /// loop iteration; the sidebar itself rate-limits the frame
    /// advance (so this is a cheap no-op most ticks) and only reports
    /// `true` when the glyph actually changed — which is the only
    /// time the animation needs a fresh frame. No working agent → no
    /// redraws at all.
    pub fn tick_working(&mut self) {
        if self.sidebar.tick_working() {
            self.redraw = true;
        }
    }

    /// Clear the spawn spinner once a terminal for its target exists in
    /// the live terminal set. The spinner is a *projection* of that set,
    /// not a latch waiting on one `TerminalSpawned`/`TerminalFocusRequested`
    /// to arrive: a dropped, raced, or mismatched event (rebadge after a
    /// merge, focus redirected, terminal already existed) can no longer
    /// strand it (#206). Called after every daemon event — any of which
    /// can produce the awaited terminal — and on the idle tick as a
    /// backstop. Returns true if it cleared (caller redraws).
    pub fn recompute_spawn_spinner(&mut self) -> bool {
        let satisfied = self.status.spawning.as_ref().is_some_and(|sp| {
            self.terminals
                .spawn_satisfied(&sp.session_key, &sp.kind, sp.baseline_count)
        });
        if satisfied {
            self.status.clear_spawning()
        } else {
            false
        }
    }

    /// Drive the polling spinner + termination check from the run
    /// loop. Cheap; called every iteration. Returns Some(msg) when
    /// the polling modal wants to be torn down.
    ///
    /// Flips `redraw` when:
    /// - the modal tick produced a termination message (caller will
    ///   apply it), OR
    /// - an indicator's glyph / elapsed label actually advanced this
    ///   tick (the `StatusCtx` heartbeat runs at an 80ms cadence). The
    ///   old gate was "an indicator exists", which forced a full
    ///   re-render on every ~16ms run-loop heartbeat for the entire
    ///   duration of a poll or provision — 4 of every 5 of those
    ///   frames repainted an unchanged screen.
    pub fn polling_tick(&mut self) -> Option<Msg> {
        let (msg, spinner_advanced) = self.status.polling_tick();
        // Backstop the projection: if the spawn's terminal slipped in
        // without a daemon event reaching `handle_daemon_event` (e.g. it
        // already existed when the spawn was sent), the idle tick still
        // clears the spinner.
        let cleared_spawn = self.recompute_spawn_spinner();
        if msg.is_some() || cleared_spawn || spinner_advanced {
            self.redraw = true;
        }
        msg
    }

    /// Tear down the polling modal. Called when its tick / feed
    /// returns Some(msg) (saw workspace, timed out, etc.).
    pub(super) fn dismiss_polling(&mut self) {
        if self.status.dismiss_polling() {
            self.redraw = true;
        }
    }

    /// Project sidebar selection onto the right pane + terminal stack.
    /// Cheap to call; the inner setters bail when nothing changed.
    /// Called after every key dispatch and once per daemon-event drain
    /// batch (via [`Self::flush_pane_sync`]).
    pub(super) fn sync_panes(&mut self) {
        // Identity gate (#1237): this runs after EVERY key dispatch, and
        // used to deep-clone the selected Workspace (sessions + tasks +
        // the full activity Vec) plus re-project all three panes per
        // character typed. When the selection AND the sidebar's pane
        // revision are unchanged since the last sync, nothing the
        // projection reads can have changed — skip it all. Daemon events
        // bump the revision (per event), so any real change re-syncs on
        // the next call.
        let identity = (
            self.sidebar.selected_workspace_key().cloned(),
            self.sidebar.pane_state_rev(),
        );
        if self.last_pane_sync_identity.as_ref() == Some(&identity) {
            // Deferred one-shots still honored on the fast path: a
            // spawn-follow parked here must not wait for the next
            // daemon event.
            if let Some(tid) = self.deferred_focus_terminal.take() {
                self.terminals.focus_terminal(tid);
            }
            return;
        }
        self.last_pane_sync_identity = Some(identity);
        let workspace = self.sidebar.selected_workspace().cloned();
        let session_key = self.sidebar.selected_workspace_key().cloned();
        // Daemon round-robin hint: every cursor mutation that
        // changes the selected workspace emits one
        // `FocusWorkspace`. Centralized here (not in each key/mouse
        // handler) so j/k, click-to-select, programmatic preselect,
        // and event-driven recompute all feed the same scheduler
        // signal without duplicating the dedup. The daemon ignores
        // hints for workspaces with no upstream GitHub repo, so
        // pre-PR sandboxes don't deform the rotation.
        if session_key != self.last_focused_session_key {
            self.last_focused_session_key = session_key.clone();
            if let Some(key) = session_key.clone() {
                self.send_cmd(IpcCommand::FocusWorkspace { session_key: key });
            }
        }
        // Lazy-fetch trigger: when the focused workspace has a PR
        // and we haven't pulled its review-thread activity this
        // session, kick off the back-fill. The dedupe set prevents
        // re-firing on every key press / poll event for the same
        // workspace; `WorkspaceRemoved` clears the entry so a
        // re-added workspace gets a fresh fetch.
        if let Some(w) = workspace.as_ref()
            && w.pr.is_some()
            && !self.pr_details_fetched.contains(&w.key)
        {
            self.pr_details_fetched.insert(w.key.clone());
            tracing::info!(
                workspace_key = %w.key.as_str(),
                "lazy-fetch: requesting PR details",
            );
            self.send_cmd(IpcCommand::FetchPrDetails {
                workspace_key: w.key.clone(),
            });
        }
        // Also forward the workspace's persisted SessionLayout to
        // the terminal stack so the user's tile arrangement
        // follows them across workspace switches. Each workspace's
        // default session carries its own Tabs/Splits state; the
        // stack used to keep whatever layout the LAST workspace
        // had, so jumping from a split workspace to a tabs one
        // would render the new one with the old split's tree.
        let layout = workspace
            .as_ref()
            .and_then(|w| w.default_session())
            .map(|s| s.layout.clone())
            .unwrap_or_default();
        let stack = session_key
            .as_ref()
            .and_then(|k| self.sidebar.stack_info(k))
            .cloned();
        self.right.set_stack(stack);
        self.right.set_workspace(workspace);
        self.terminals.set_active_session(session_key);
        self.terminals.set_layout(layout);
        // A pinned `w` spawn-follow asked for a specific terminal to be
        // the active tab. Apply it now that `set_active_session` has
        // (re)built the visible set for the followed workspace, so the
        // user lands on the fresh agent and not whatever tab the
        // workspace last had — a no-op if the terminal isn't in the
        // active session's visible set.
        if let Some(tid) = self.deferred_focus_terminal.take() {
            self.terminals.focus_terminal(tid);
        }
        // If the selection landed on a workspace whose Activity pane is
        // hidden while that pane held focus, hand focus to the terminal
        // so keystrokes don't vanish into an unrendered pane.
        self.enforce_pane_focus();
        // Navigating onto an outdated old-build terminal explains its
        // broken scrollback in context (#544).
        self.hint_outdated_scroll_focus();
        // Navigating onto a bypass-mode terminal explains its `⚠` (#989).
        self.hint_no_permission_focus();
    }

    /// Snapshot the pane the user currently rests in for the selected
    /// workspace, so re-selecting that workspace later can restore it
    /// (#182). Called at input-event entry — i.e. the steady state
    /// *before* the event mutates focus/selection — so a sidebar click
    /// that moves focus to the sidebar never overwrites the terminal
    /// focus of the workspace being left.
    pub(super) fn record_workspace_focus(&mut self) {
        if let Some(key) = self.sidebar.selected_workspace_key() {
            self.workspace_focus.insert(key.clone(), self.focus);
        }
    }

    /// Restore the pane focus remembered for the currently-selected
    /// workspace, falling back to the existing focus when there's no
    /// memory or the remembered pane isn't currently available (its
    /// terminal exited, or the Activity pane is hidden). Call after the
    /// sidebar cursor + panes have synced so the availability checks see
    /// the target workspace's terminals.
    pub(super) fn restore_workspace_focus(&mut self) {
        let Some(remembered) = self
            .sidebar
            .selected_workspace_key()
            .and_then(|key| self.workspace_focus.get(key).copied())
        else {
            return;
        };
        let available = match remembered {
            PaneFocus::Terminals => self.terminals.active_terminal_id().is_some(),
            PaneFocus::Right => self.activity_pane_visible(),
            PaneFocus::Sidebar => true,
        };
        if available && self.focus != remembered {
            self.set_focus(remembered);
            self.redraw = true;
        }
    }

    /// Double-click "enter": drop focus into the selected workspace's
    /// live terminal (#441). With no live session it degrades to the
    /// default open — the activity pane when there's activity to show,
    /// otherwise the sidebar selection the click already made.
    pub(super) fn enter_selected_workspace_terminal(&mut self) {
        let target = if self.terminals.active_terminal_id().is_some() {
            PaneFocus::Terminals
        } else if self.activity_pane_visible() {
            PaneFocus::Right
        } else {
            PaneFocus::Sidebar
        };
        if self.focus != target {
            self.set_focus(target);
            self.redraw = true;
        }
    }

    /// Turn an agent-CLI update-check reading into footer notices.
    /// Availability is always announced — but an agent the daemon is
    /// about to auto-update (scheduled sweep + `auto_update: true`) is
    /// announced as exactly that, not as an instruction to go update
    /// it manually. A manual check (`,` → maintenance) always answers,
    /// including probe errors; a scheduled sweep with nothing
    /// actionable stays silent and leaves its probe errors in the
    /// daemon log.
    pub(super) fn note_agent_cli_updates(
        &mut self,
        statuses: &[lazybox_ipc::AgentCliUpdateStatus],
        manual: bool,
    ) {
        let label = |s: &lazybox_ipc::AgentCliUpdateStatus| match (&s.installed, &s.latest) {
            (Some(i), Some(l)) => format!("{} {i} → {l}", s.display_name),
            _ => s.display_name.clone(),
        };
        // On a scheduled sweep the daemon applies auto_update agents'
        // updates itself right after this event; only on a manual
        // check is every available update the user's to trigger.
        let (auto, needs_action): (Vec<_>, Vec<_>) = statuses
            .iter()
            .filter(|s| s.update_available)
            .partition(|s| s.auto_update && !manual);
        let needs_action: Vec<String> = needs_action.into_iter().map(label).collect();
        let auto: Vec<String> = auto.into_iter().map(label).collect();
        if !needs_action.is_empty() {
            self.flash_info(format!(
                "⬆ agent update available — {} · update via , ▸ maintenance",
                needs_action.join(", ")
            ));
        } else if !auto.is_empty() {
            self.flash_info(format!("⬆ auto-updating agent CLIs — {}", auto.join(", ")));
        }
        if !manual {
            return;
        }
        let errors: Vec<String> = statuses
            .iter()
            .filter_map(|s| s.error.as_ref().map(|e| format!("{}: {e}", s.display_name)))
            .collect();
        if !errors.is_empty() {
            // May displace the availability notice above — both land
            // in the Shift-M log, and the sticky error is the one the
            // user must not miss.
            self.flash_error(format!("✗ agent update check — {}", errors.join(" · ")));
        } else if statuses.is_empty() {
            self.flash_hint("no enabled agent has a managed update channel");
        } else if needs_action.is_empty() {
            let versions: Vec<String> = statuses
                .iter()
                .map(|s| match &s.installed {
                    Some(v) => format!("{} {v}", s.display_name),
                    None => s.display_name.clone(),
                })
                .collect();
            self.flash_info(format!("✓ agent CLIs up to date — {}", versions.join(", ")));
        }
    }

    /// Apply the pending `--workspace [--session]` selection. One-shot
    /// — clears `self.preselect` so subsequent snapshots don't
    /// override the user's manual cursor moves.
    pub(super) fn apply_preselect(&mut self) {
        let Some(p) = self.preselect.take() else {
            return;
        };
        let landed = self.sidebar.focus_workspace_key(&p.workspace_key);
        if !landed {
            tracing::info!(
                "preselect: workspace key {:?} not found in first snapshot",
                p.workspace_key
            );
            return;
        }
        if let Some(raw) = p.session_id_raw
            && let Ok(uuid) = uuid::Uuid::parse_str(&raw)
        {
            let _ = self.sidebar.focus_session_id(lazybox_core::SessionId(uuid));
            // Move focus to terminals so the user can type immediately.
            self.set_focus(PaneFocus::Terminals);
        }
    }
}

/// Map a `ProviderError` source string to the user-facing verb of the
/// GitHub mutation that failed, for the `✗ <action> failed — <reason>`
/// Permanent notice. `None` for sources that are NOT user-initiated
/// mutations (poll cycles, spawn paths, worktree provisioning, agent
/// runs …) — those keep their existing handling. The strings mirror
/// the daemon's `emit_err` sources in
/// `crates/server/src/polling/handlers.rs`.
fn mutation_failure_label(source: &str) -> Option<&'static str> {
    match source {
        "reviewers" => Some("request reviewers"),
        "assignees" => Some("update assignees"),
        "labels" => Some("update labels"),
        "reply" => Some("reply"),
        // Pre-flight merge / close failures (workspace lookup, provider
        // build). The GitHub-rejected cases arrive as the dedicated
        // `PrMergeFailed` / `IssueCloseFailed` events instead.
        "merge" => Some("merge"),
        "close-issue" => Some("close issue"),
        _ => None,
    }
}

#[cfg(test)]
mod failure_notice_tests {
    use super::{action_failure_notice, strip_graphql_path};

    #[test]
    fn reason_leads_and_label_is_trimmed_to_number() {
        let n = action_failure_notice("merge", "AntoineToussaint/lazybox#588", "not mergeable");
        assert_eq!(n, "✗ merge failed: not mergeable (#588)");
        // The reason precedes the label so middle-truncation keeps it.
        assert!(n.find("not mergeable") < n.find("#588"));
        // The owner/repo prefix that used to eat the width budget is gone.
        assert!(!n.contains("AntoineToussaint"));
    }

    #[test]
    fn strips_every_graphql_path_segment() {
        // Joined multi-error reasons carry one suffix per error.
        let cleaned = strip_graphql_path(
            "A merge is already in progress [at mergePullRequest]; base modified [at repository]",
        );
        assert_eq!(cleaned, "A merge is already in progress; base modified",);
    }

    #[test]
    fn label_without_hash_is_kept_whole() {
        // Fallback labels (workspace keys) may lack a `#NNN`.
        let n = action_failure_notice("close", "github:local-ws", "permission denied");
        assert!(n.contains("(github:local-ws)"), "{n}");
    }
}
