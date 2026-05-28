//! Keyboard + mouse + paste handlers.
//!
//! The big `handle_pane_key` matcher (catalog-dispatch + per-key
//! arms + terminal-escape latch), the quit-chord resolver, the
//! detach lookup, and the mouse / paste handlers all live here.
//! Test-only `dispatch_*` entry points sit alongside so the
//! integration tests can drive the same code paths the run loop
//! does.
//!
//! Free helpers used only by this surface (`rect_contains`,
//! `key_event_to_chord`, `find_action_for_chord`,
//! `spawn_detached_pilot`, `emit_clipboard_copy`) live in `mod.rs`
//! today and are reachable from this submodule because child
//! modules can see their parent's private items.

use super::{
    Id, Model, PaneFocus, emit_clipboard_copy, find_action_for_chord, key_event_to_chord,
    rect_contains, spawn_detached_pilot,
};
use crate::realm::keymap::realm_key_to_crossterm;
use crate::realm::layout::pane_areas;
use pilot_ipc::Command as IpcCommand;
use std::time::Duration;
use tuirealm::application::PollStrategy;
use tuirealm::event::{Event as RealmEvent, Key, KeyEvent as RealmKey, KeyModifiers};
use tuirealm::ratatui::layout::Rect;
use tuirealm::terminal::TerminalAdapter;

impl<T: TerminalAdapter> Model<T> {
    /// Top-level key handler when no modal is active. Routes Tab,
    /// global escapes, and forwards everything else to the focused
    /// pane wrapper.
    pub(super) fn handle_pane_key(&mut self, key: RealmKey) {
        match key.code {
            // Tab cycles panes — but ONLY when the active pane has
            // no PTY swallowing keys. Inside a terminal with a live
            // PTY, Tab belongs to the shell / agent; the `]]`
            // escape sequence is the only way out (tmux-style
            // prefix model). With no terminals running, Tab cycles
            // normally — there's no inner program to forward it to.
            Key::Tab
                if !key.modifiers.contains(KeyModifiers::SHIFT)
                    && (self.focus != PaneFocus::Terminals
                        || self.terminals.is_empty()
                        || !self.terminal_user_typed_since_focus) =>
            {
                // Empty terminal pane OR fresh-entry-no-typing-yet →
                // cycle focus instead of forwarding Tab to the PTY.
                // After the user has typed even one character in this
                // focus session the flag flips and Tab goes to the
                // shell for autocomplete.
                self.q_latch.disarm();
                self.focus = self.focus.next();
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            _ if self.focus != PaneFocus::Terminals && self.matches_quit_chord(&key) => {
                // Quit chord (catalog `ActionKind::Quit`, default `q q`,
                // overridable via `ui.action_keys.quit`). `Double(inner)`
                // is the two-press latch; `Single` fires on first press.
                let chord = self.resolve_quit_chord();
                use pilot_tui_core::action::KeyChord;
                if matches!(chord, Some(KeyChord::Single { .. })) {
                    self.quit = true;
                    return;
                }
                if self.q_latch.tap(self.ui_defaults.quit_double_tap_window) {
                    self.quit = true;
                    return;
                }
                self.redraw = true;
                return;
            }
            // `?` Help, `!` JumpToAsking — both go through the
            // catalog dispatch below (Section::Global).
            // `Enter` on the sidebar = "open this row" → focus the
            // Activity pane so the user can read comments / reply.
            // Right pane keeps its own Enter meaning (toggle section);
            // terminals forward Enter as `\r` to the PTY.
            _ if self.focus == PaneFocus::Sidebar
                && key.code == Key::Enter
                && key.modifiers.is_empty() =>
            {
                self.q_latch.disarm();
                self.focus = PaneFocus::Right;
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            // Shift-arrows: resize splitters. Disabled inside a
            // terminal so the shell can still bind them.
            Key::Left | Key::Right | Key::Up | Key::Down
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    && self.focus != PaneFocus::Terminals =>
            {
                self.q_latch.disarm();
                let (dx, dy) = match key.code {
                    Key::Left => (-self.ui_defaults.split_step_percent, 0),
                    Key::Right => (self.ui_defaults.split_step_percent, 0),
                    Key::Up => (0, -self.ui_defaults.split_step_percent),
                    Key::Down => (0, self.ui_defaults.split_step_percent),
                    _ => (0, 0),
                };
                if self.layout.nudge_splits(dx, dy) {
                    self.redraw = true;
                }
                return;
            }
            // Ctrl-Shift-D: detach the focused pane into a new pilot
            // process. Many terminals report Ctrl-Shift-letter as the
            // capital letter with CONTROL set; some include SHIFT too.
            // Match either form.
            Key::Char('D')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.focus != PaneFocus::Terminals =>
            {
                self.q_latch.disarm();
                if let Some(spec) = self.focused_detach_spec() {
                    spawn_detached_pilot(&spec);
                }
                return;
            }
            // Toggle pilot's mouse capture so the host terminal
            // (Ghostty / iTerm2) regains native text selection. When
            // OFF the user can trackpad-select inside claude / shell
            // scrollback and Cmd-C normally; toggle back on for
            // splitter drag etc. Bound to multiple chords because
            // terminals report Ctrl-Shift-S inconsistently and
            // Ctrl-S itself is XOFF flow control:
            //   - F8         — function key, never conflicts with TTY
            //   - Alt-s      — Option-s on Mac (Alt-s elsewhere)
            //   - Ctrl-Alt-s — extra fallback for non-mac users
            // Available from any pane (including Terminals) so users
            // in claude can escape to a copy gesture without breaking
            // flow.
            Key::Function(8) => {
                self.q_latch.disarm();
                self.toggle_mouse_capture();
                return;
            }
            Key::Char('s')
                if key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.q_latch.disarm();
                self.toggle_mouse_capture();
                return;
            }
            Key::Char('s' | 'S')
                if key.modifiers.contains(KeyModifiers::ALT)
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.q_latch.disarm();
                self.toggle_mouse_capture();
                return;
            }
            // Reply, RequestReviewers, AddAssignees, OpenEditor,
            // NewWorkspace, NewProject, OpenHelp, OpenSettings,
            // Refresh, AdoptSessions all flow through the catalog
            // dispatch below — see the whitelist further down.
            _ => {
                // Any other key disarms.
                self.q_latch.disarm();
            }
        }

        // Terminal-pane escape sequence (`]]` by default). Two
        // consecutive presses of the escape char inside a terminal
        // return focus to the sidebar instead of forwarding to the
        // PTY. The first `]` is held back; if a non-`]` key arrives
        // before the second `]`, the held char is flushed to the PTY
        // first so the user's `]` isn't silently swallowed.
        if self.focus == PaneFocus::Terminals
            && key.modifiers.is_empty()
            && matches!(key.code, Key::Char(c) if c == self.ui_defaults.terminal_escape_char)
        {
            const ESCAPE_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);
            if self.escape_latch.tap(ESCAPE_WINDOW) {
                self.focus = PaneFocus::Sidebar;
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            return;
        }
        if self.focus == PaneFocus::Terminals && self.escape_latch.is_armed() {
            self.escape_latch.disarm();
            // Non-`]` key arrived after a held `]` — flush the held
            // char to the PTY before the new key, so typing patterns
            // like `]a` aren't lost.
            let mut held_cmds: Vec<IpcCommand> = Vec::new();
            let held = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(self.ui_defaults.terminal_escape_char),
                crossterm::event::KeyModifiers::NONE,
            );
            self.terminals.handle_key_direct(held, &mut held_cmds);
            for cmd in held_cmds {
                self.send_cmd(cmd);
            }
        }

        // We have a typed key already; skip the synthetic Event
        // round-trip and call the pane wrappers' direct entry points.
        let ct = realm_key_to_crossterm(&key);
        let mut cmds: Vec<IpcCommand> = Vec::new();

        // Catalog lookup first. If the keystroke matches a catalog
        // `Action` AND `dispatch_action` knows how to handle it
        // (returns a non-empty Vec or mutates state), the pane's
        // direct handler is skipped. Per-key match arms in the
        // panes still cover what `dispatch_action` doesn't yet —
        // see that function's coverage comment.
        if self.focus != PaneFocus::Terminals
            && let Some(chord) = key_event_to_chord(ct)
            && let Some(def) = find_action_for_chord(&chord, self.focus, &self.action_key_overrides)
        {
            use pilot_tui_core::action::Action;
            // Reconstruct a runtime Action from the static ActionDef.
            // `SpawnAgent` is the only variant with runtime data
            // (the agent id) — we don't yet have per-agent catalog
            // entries (`c` → claude, `x` → codex, …), so we let
            // those keys fall through to the pane handler. Once
            // the catalog grows per-agent entries (driven by the
            // user's enabled agents list), this map widens.
            let action: Option<Action> = match def.kind {
                pilot_tui_core::action::ActionKind::SpawnShell => Some(Action::SpawnShell),
                pilot_tui_core::action::ActionKind::MarkAllRead => Some(Action::MarkAllRead),
                pilot_tui_core::action::ActionKind::Work => Some(Action::Work),
                pilot_tui_core::action::ActionKind::OpenEditor => Some(Action::OpenEditor),
                pilot_tui_core::action::ActionKind::NewWorkspace => Some(Action::NewWorkspace),
                pilot_tui_core::action::ActionKind::NewProject => Some(Action::NewProject),
                pilot_tui_core::action::ActionKind::MergePr => Some(Action::MergePr),
                pilot_tui_core::action::ActionKind::Archive => Some(Action::Archive),
                pilot_tui_core::action::ActionKind::ToggleSnooze => Some(Action::ToggleSnooze),
                pilot_tui_core::action::ActionKind::Refresh => Some(Action::Refresh),
                pilot_tui_core::action::ActionKind::AdoptSessions => Some(Action::AdoptSessions),
                pilot_tui_core::action::ActionKind::CollapseIntoPr => Some(Action::CollapseIntoPr),
                pilot_tui_core::action::ActionKind::Reply => Some(Action::Reply),
                pilot_tui_core::action::ActionKind::RequestReviewers => {
                    Some(Action::RequestReviewers)
                }
                pilot_tui_core::action::ActionKind::AddAssignees => Some(Action::AddAssignees),
                pilot_tui_core::action::ActionKind::OpenInBrowser => Some(Action::OpenInBrowser),
                pilot_tui_core::action::ActionKind::OpenHelp => Some(Action::OpenHelp),
                pilot_tui_core::action::ActionKind::OpenSettings => Some(Action::OpenSettings),
                pilot_tui_core::action::ActionKind::JumpToAsking => Some(Action::JumpToAsking),
                _ => None,
            };
            if let Some(action) = action {
                // Any catalog dispatch counts as "non-quit key" so
                // the q q chord resets.
                self.q_latch.disarm();
                cmds.extend(self.dispatch_action(&action));
                // Drain queued cmds + early return — the catalog
                // handled the key, the pane shouldn't see it.
                self.sync_panes();
                for cmd in cmds {
                    let rewritten = self.rewrite_spawn_to_inject(cmd);
                    self.send_cmd(rewritten);
                }
                self.redraw = true;
                return;
            }
        }

        match self.focus {
            PaneFocus::Sidebar => self.sidebar.handle_key_direct(ct, &mut cmds),
            PaneFocus::Right => self.right.handle_key_direct(ct, &mut cmds),
            // Terminals pane with NO active terminal can't route to a
            // PTY. The empty-state hint says "press s for shell, c
            // for claude" — those bindings live on Sidebar, so we
            // forward there instead. PTY-routing resumes once the
            // first TerminalSpawned arrives.
            PaneFocus::Terminals if self.terminals.is_empty() => {
                self.sidebar.handle_key_direct(ct, &mut cmds);
            }
            PaneFocus::Terminals => {
                // Anything routed to the PTY counts as "user typed":
                // Tab gates above won't see this key as a cycle
                // trigger anymore.
                self.terminal_user_typed_since_focus = true;
                self.terminals.handle_key_direct(ct, &mut cmds);
            }
        }
        // Surface spawn intent in the footer so the user sees that
        // worktree creation / process startup is happening (can take
        // 1-3s on first session). The notice clears when the matching
        // `TerminalSpawned` arrives in `handle_daemon_event`.
        for cmd in &cmds {
            if let IpcCommand::Spawn { kind, .. } = cmd {
                let label = match kind {
                    pilot_ipc::TerminalKind::Shell => "shell".to_string(),
                    pilot_ipc::TerminalKind::Agent(a) => a.to_string(),
                    other => format!("{other:?}").to_lowercase(),
                };
                self.flash_info(format!("Spawning {label}…"));
            }
        }
        for cmd in cmds {
            let rewritten = self.rewrite_spawn_to_inject(cmd);
            self.send_cmd(rewritten);
        }
        // Sidebar j/k changes selection — propagate to right + terminals.
        self.sync_panes();
        self.redraw = true;
    }

    /// Returns true when the q-q latch is armed (used by the bottom
    /// hint bar to show "press q again" briefly).
    pub fn q_arm_pending(&self) -> bool {
        self.q_latch.is_armed()
    }

    /// Read-only accessor — which pane currently has focus. Used by
    /// tests + (in future) the bottom hint bar.
    pub fn focus(&self) -> PaneFocus {
        self.focus
    }

    /// Sidebar / right / activity split percentages — exposed so tests
    /// can verify Shift-arrow + drag updates apply correctly.
    pub fn split_pcts(&self) -> (u16, u16) {
        (self.layout.sidebar_pct, self.layout.right_top_pct)
    }

    /// Top of the modal stack (or None if no modal is mounted). Used
    /// by tests to verify that `?` mounts the help modal, etc.
    pub fn top_modal(&self) -> Option<&Id> {
        self.modal_stack.last()
    }

    /// Test entry point: drive a key through `handle_pane_key`. Lets
    /// integration tests bypass the run-loop's crossterm polling.
    pub fn dispatch_key(&mut self, key: RealmKey) {
        self.handle_pane_key(key);
    }

    /// Test entry point: drive a key through the *modal* pipeline —
    /// send into `modal_event_tx`, poll `app.tick` until the modal
    /// produces a Msg (or a short deadline elapses), then `update`
    /// each Msg. Exists because `dispatch_key` calls `handle_pane_key`,
    /// which is gated on an empty modal stack and therefore can't
    /// exercise key handling for a mounted Confirm, Input, etc.
    pub fn dispatch_modal_key(&mut self, key: RealmKey) {
        let _ = self.modal_event_tx.send(RealmEvent::Keyboard(key));
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            match self.app.tick(PollStrategy::Once(Duration::ZERO)) {
                Ok(messages) if !messages.is_empty() => {
                    for msg in messages {
                        self.update(msg);
                    }
                    return;
                }
                Ok(_) => {}
                Err(_) => return,
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Rewrite a `Spawn { kind: Agent(id), initial_prompt: Some(_) }`
    /// into `InjectPrompt { terminal_id, prompt }` when an agent
    /// of the same kind is already running on this workspace.
    /// Used by BOTH the pane-handler key path and the catalog
    /// dispatch path so `w` and per-pane shortcuts agree on
    /// "continue the existing conversation" semantics.
    fn rewrite_spawn_to_inject(&mut self, cmd: IpcCommand) -> IpcCommand {
        match cmd {
            IpcCommand::Spawn {
                session_key,
                session_id,
                kind: pilot_ipc::TerminalKind::Agent(agent_id),
                cwd,
                initial_prompt: Some(prompt),
            } => {
                if let Some(terminal_id) = self.sidebar.find_agent_terminal(&session_key, &agent_id)
                {
                    self.flash_hint(format!("→ injecting into existing {agent_id}"));
                    // Always carry the Spawn parameters so a stale
                    // terminal id (agent died between this lookup and
                    // the command arriving at the daemon) falls back
                    // to Spawn instead of silently dropping the
                    // user's prompt. The TUI's view of
                    // `running_terminals` is updated from a broadcast
                    // channel, so there's always a small window where
                    // `find_agent_terminal` returns a dead id.
                    let fallback_spawn = Some(pilot_ipc::SpawnFallback {
                        session_key: session_key.clone(),
                        session_id,
                        kind: pilot_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: cwd.clone(),
                    });
                    IpcCommand::InjectPrompt {
                        terminal_id,
                        prompt,
                        fallback_spawn,
                    }
                } else {
                    IpcCommand::Spawn {
                        session_key,
                        session_id,
                        kind: pilot_ipc::TerminalKind::Agent(agent_id),
                        cwd,
                        initial_prompt: Some(prompt),
                    }
                }
            }
            other => other,
        }
    }

    /// Test entry point: drive a mouse event through `handle_mouse`
    /// after manually setting `last_area` (since `view()` would
    /// otherwise be needed to populate it).
    pub fn dispatch_mouse_in(&mut self, m: crossterm::event::MouseEvent, area: Rect) {
        self.layout.last_area = area;
        self.handle_mouse(m);
    }

    /// Test accessor — read-only handle to the sidebar wrapper.
    pub fn sidebar(&self) -> &crate::realm::components::sidebar::Sidebar {
        &self.sidebar
    }

    /// Test accessor — mutable handle to the sidebar wrapper. Used
    /// by orchestrator tests (integration test crate) to position
    /// the cursor on a specific row before dispatching a key. Not
    /// `#[cfg(test)]` because integration tests live in a separate
    /// crate; doc-hidden + `__test_` prefix mark it as off-limits
    /// for production callers without forcing a test-config wall.
    #[doc(hidden)]
    pub fn __test_sidebar_mut(&mut self) -> &mut crate::realm::components::sidebar::Sidebar {
        &mut self.sidebar
    }

    /// Look up the Quit chord — catalog default OR
    /// `ui.action_keys.quit` override. Returns the parsed `KeyChord`
    /// (`Double` for `q q`, `Single` for a single-letter remap).
    fn resolve_quit_chord(&self) -> Option<pilot_tui_core::action::KeyChord> {
        use pilot_tui_core::action::{ActionDef, ActionKind, KeyChord};
        let def = ActionDef::for_kind(ActionKind::Quit);
        def.effective_chord(&self.action_key_overrides)
            .or_else(|| KeyChord::parse(def.default_keys))
    }

    /// Matches the FIRST key of the Quit chord (the entry-point for
    /// the latch). For `Double` chords this is the inner single
    /// chord's first press; for `Single` chords this is the chord
    /// itself.
    fn matches_quit_chord(&self, key: &RealmKey) -> bool {
        use pilot_tui_core::action::KeyChord;
        let Some(chord) = self.resolve_quit_chord() else {
            return false;
        };
        let first = match &chord {
            KeyChord::Single { .. } => chord,
            KeyChord::Double(inner) => (**inner).clone(),
        };
        let Some(input) = key_event_to_chord(realm_key_to_crossterm(key)) else {
            return false;
        };
        input == first
    }

    /// DetachSpec for the focused pane, or None if it can't detach
    /// (e.g. cursor on a repo header in the sidebar).
    fn focused_detach_spec(&self) -> Option<crate::pane::DetachSpec> {
        match self.focus {
            PaneFocus::Sidebar => self.sidebar.detachable(),
            PaneFocus::Right => self.right.detachable(),
            PaneFocus::Terminals => self.terminals.detachable(),
        }
    }

    /// Handle a bracketed-paste event from the host terminal. The
    /// host wraps the pasted text in `ESC[200~ … ESC[201~` and
    /// crossterm hands us the inner string. We forward the same
    /// wrapped sequence to the focused terminal's PTY so the
    /// inner program (Claude, shell, vim) sees a single paste
    /// instead of a stream of keystrokes.
    ///
    /// Only fires when the terminal pane is focused. Other panes
    /// don't have a useful paste-target today (reply textarea has
    /// its own keyboard path through tuirealm).
    pub fn handle_paste(&mut self, text: &str) {
        if self.focus != PaneFocus::Terminals {
            return;
        }
        let Some(terminal_id) = self.terminals.active_terminal_id() else {
            return;
        };
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        self.send_cmd(IpcCommand::Write { terminal_id, bytes });
        self.redraw = true;
    }

    /// Mouse routing:
    /// - Down on a splitter line → start drag (resize panes on
    ///   subsequent Drag events until Up).
    /// - Down anywhere else → focus the pane the click landed in.
    /// - Up → end the active drag.
    /// - ScrollUp/Down over the terminal pane → forward to the
    ///   terminal's scrollback (libghostty handles the actual move).
    pub fn handle_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;

        if self.layout.last_area.width == 0 || self.layout.last_area.height == 0 {
            return;
        }
        let (sidebar_rect, right_top_rect, right_bottom_rect) = pane_areas(
            self.layout.last_area,
            self.layout.sidebar_pct,
            self.layout.right_top_pct,
            self.layout.sidebar_user_resized,
        );

        match m.kind {
            MouseEventKind::Down(button) => {
                self.q_latch.disarm();
                // Tab-strip click on the terminal pane top row →
                // switch active tab. Checked BEFORE the
                // "forward to inner program" path because the tab
                // strip belongs to pilot, not to Claude/shell.
                if matches!(button, crossterm::event::MouseButton::Left)
                    && let Some(idx) = self.terminals.tab_at(m.column, m.row)
                {
                    self.terminals.set_active_tab(idx);
                    self.focus = PaneFocus::Terminals;
                    self.set_focus_attr();
                    self.redraw = true;
                    return;
                }
                // Right-click in the sidebar → open the workspace
                // context menu. Move the cursor to the clicked row
                // first (same as left-click) so the menu acts on
                // the visible selection.
                if matches!(button, crossterm::event::MouseButton::Right)
                    && rect_contains(sidebar_rect, m.column, m.row)
                {
                    self.focus = PaneFocus::Sidebar;
                    self.set_focus_attr();
                    if self.sidebar.click_to_select(sidebar_rect, m.row) {
                        self.sync_panes();
                    }
                    if let Some(ws) = self.sidebar.selected_workspace() {
                        let session_key: pilot_core::SessionKey = (&ws.key).into();
                        self.mount_sidebar_context_menu(session_key);
                    }
                    return;
                }
                // Right-click on a URL inside the terminal pane →
                // open the URL in the system browser. We intercept
                // BEFORE the PTY-forwarding path below so a
                // right-click on a link works the same regardless
                // of whether the inner program (claude, vim, …)
                // would otherwise grab the event. If no URL is
                // under the cursor we fall through to the normal
                // routing — the PTY still gets the right-click for
                // its own context menus.
                if matches!(button, crossterm::event::MouseButton::Right)
                    && rect_contains(right_bottom_rect, m.column, m.row)
                    && self
                        .layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                        .is_none()
                    && let Some(url) =
                        self.terminals.url_at(right_bottom_rect, m.column, m.row)
                {
                    match pilot_tui_core::editors::open_url(&url) {
                        Ok(()) => {
                            self.flash_hint(format!("opened {url}"));
                        }
                        Err(e) => {
                            tracing::warn!(%url, "open_url failed: {e}");
                            self.flash_hint(format!("open_url failed: {e}"));
                        }
                    }
                    return;
                }
                // A left-click in the terminal pane ALWAYS starts a
                // potential pilot selection — we commit to that
                // even when the inner program is mouse-tracking.
                let claim_for_selection = rect_contains(right_bottom_rect, m.column, m.row)
                    && self.focus == PaneFocus::Terminals
                    && matches!(button, crossterm::event::MouseButton::Left)
                    && self
                        .layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                        .is_none();

                // Forward CLICK-down to mouse-tracking inner programs
                // only when we're NOT claiming for selection — i.e.,
                // non-left buttons. Left clicks are deferred.
                if !claim_for_selection
                    && rect_contains(right_bottom_rect, m.column, m.row)
                    && self.focus == PaneFocus::Terminals
                    && self.terminals.focused_terminal_tracks_mouse()
                    && self
                        .layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                        .is_none()
                {
                    let cell_col = m.column.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = m.row.saturating_sub(right_bottom_rect.y) as u32;
                    let vt_button = match button {
                        crossterm::event::MouseButton::Left => libghostty_vt::mouse::Button::Left,
                        crossterm::event::MouseButton::Middle => {
                            libghostty_vt::mouse::Button::Middle
                        }
                        crossterm::event::MouseButton::Right => libghostty_vt::mouse::Button::Right,
                    };
                    if let Some((terminal_id, bytes)) = self.terminals.encode_mouse(
                        libghostty_vt::mouse::Action::Press,
                        Some(vt_button),
                        cell_col,
                        cell_row,
                    ) {
                        self.send_cmd(IpcCommand::Write { terminal_id, bytes });
                        self.redraw = true;
                        return;
                    }
                }
                if let Some(target) =
                    self.layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                {
                    self.layout.active_drag = Some(target);
                    return;
                }
                let target = if rect_contains(sidebar_rect, m.column, m.row) {
                    Some(PaneFocus::Sidebar)
                } else if rect_contains(right_top_rect, m.column, m.row) {
                    Some(PaneFocus::Right)
                } else if rect_contains(right_bottom_rect, m.column, m.row) {
                    Some(PaneFocus::Terminals)
                } else {
                    None
                };
                if let Some(focus) = target {
                    if self.focus != focus {
                        self.focus = focus;
                        self.set_focus_attr();
                        self.redraw = true;
                    }
                    if focus == PaneFocus::Sidebar {
                        // Try the header chips first (filter, then
                        // sort); if neither hit, fall through to row
                        // selection. All three outcomes update the
                        // same state, so one consolidated branch.
                        let handled = self.sidebar.click_to_cycle_filter(m.column, m.row)
                            || self.sidebar.click_to_cycle_sort(m.column, m.row)
                            || self.sidebar.click_to_select(sidebar_rect, m.row);
                        if handled {
                            // Double-click on a repo header → toggle
                            // its collapsed state (same effect as
                            // Space). Cursor already moved via
                            // click_to_select above so
                            // `toggle_repo_at_cursor` operates on
                            // the just-clicked header.
                            const DOUBLE_CLICK_WINDOW: std::time::Duration =
                                std::time::Duration::from_millis(400);
                            let is_double = matches!(button, crossterm::event::MouseButton::Left)
                                && self
                                    .last_click
                                    .map(|(c, r, t)| {
                                        c == m.column
                                            && r == m.row
                                            && t.elapsed() <= DOUBLE_CLICK_WINDOW
                                    })
                                    .unwrap_or(false);
                            if is_double && self.sidebar.cursor_on_repo_header() {
                                self.last_click = None;
                                self.sidebar.toggle_repo_at_cursor();
                            } else {
                                self.last_click =
                                    Some((m.column, m.row, std::time::Instant::now()));
                            }
                            self.sync_panes();
                            self.redraw = true;
                        }
                    }
                    // Pilot-side selection start: any left-click that
                    // landed in the terminal pane. Recording start ==
                    // end means a click-without-drag is treated as a
                    // click in the Up handler.
                    if focus == PaneFocus::Terminals
                        && matches!(button, crossterm::event::MouseButton::Left)
                        && claim_for_selection
                    {
                        self.terminal_selection = Some(((m.column, m.row), (m.column, m.row)));
                    } else {
                        let _ = button;
                    }
                    if focus == PaneFocus::Right {
                        const DOUBLE_CLICK_WINDOW: std::time::Duration =
                            std::time::Duration::from_millis(400);
                        let is_double = matches!(button, crossterm::event::MouseButton::Left)
                            && self
                                .last_click
                                .map(|(c, r, t)| {
                                    c == m.column
                                        && r == m.row
                                        && t.elapsed() <= DOUBLE_CLICK_WINDOW
                                })
                                .unwrap_or(false);
                        let handled = if is_double {
                            self.last_click = None; // consume the pair
                            self.right.handle_mouse_double_click(m.column, m.row)
                        } else {
                            self.last_click = Some((m.column, m.row, std::time::Instant::now()));
                            self.right.handle_mouse_click(m.column, m.row)
                        };
                        if handled {
                            self.redraw = true;
                        }
                        if let Some(msg) = self.right.drain_selection_notice() {
                            self.flash_hint(msg);
                        }
                    }
                }
            }
            MouseEventKind::Drag(_) => {
                if let Some(target) = self.layout.active_drag {
                    if self.layout.update_drag(target, m.column, m.row) {
                        self.redraw = true;
                    }
                    return;
                }
                if let Some((start, _)) = self.terminal_selection {
                    self.terminal_selection = Some((start, (m.column, m.row)));
                    self.redraw = true;
                }
            }
            MouseEventKind::Up(button) => {
                let was_drag = self.layout.active_drag.take().is_some();
                if was_drag {
                    self.layout.persist();
                }
                let mut click_no_drag_at: Option<(u16, u16)> = None;
                if let Some((start, end)) = self.terminal_selection.take() {
                    let was_drag = start != end;
                    if was_drag {
                        let text = self.terminals.extract_text(right_bottom_rect, start, end);
                        if !text.trim().is_empty() {
                            emit_clipboard_copy(&text);
                            let lines = text.lines().count();
                            self.flash_hint(format!(
                                "copied {} line{} to clipboard",
                                lines,
                                if lines == 1 { "" } else { "s" }
                            ));
                        }
                    } else {
                        click_no_drag_at = Some(start);
                    }
                    self.redraw = true;
                }
                if let Some((col, row)) = click_no_drag_at
                    && rect_contains(right_bottom_rect, col, row)
                    && self.focus == PaneFocus::Terminals
                    && self.terminals.focused_terminal_tracks_mouse()
                {
                    let cell_col = col.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = row.saturating_sub(right_bottom_rect.y) as u32;
                    let vt_button = match button {
                        crossterm::event::MouseButton::Left => libghostty_vt::mouse::Button::Left,
                        crossterm::event::MouseButton::Middle => {
                            libghostty_vt::mouse::Button::Middle
                        }
                        crossterm::event::MouseButton::Right => libghostty_vt::mouse::Button::Right,
                    };
                    for action in [
                        libghostty_vt::mouse::Action::Press,
                        libghostty_vt::mouse::Action::Release,
                    ] {
                        if let Some((terminal_id, bytes)) =
                            self.terminals
                                .encode_mouse(action, Some(vt_button), cell_col, cell_row)
                        {
                            self.send_cmd(IpcCommand::Write { terminal_id, bytes });
                        }
                    }
                    self.redraw = true;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // Apply the inertia damper BEFORE checking which pane
                // owns the event so the OS-driven "flick keeps
                // scrolling for 500ms after the gesture" phase decays
                // its STEP and a reverse-direction gesture cancels
                // the queued inertia. `dampen_scroll_step` returns 0
                // when the event should be dropped entirely (reverse
                // mid-burst).
                let raw_up = matches!(m.kind, MouseEventKind::ScrollUp);
                let scaled = self.dampen_scroll_step(raw_up, m);
                if scaled == 0 {
                    return;
                }
                if rect_contains(right_top_rect, m.column, m.row) {
                    let delta = if raw_up { -scaled } else { scaled };
                    if self.right.scroll_activity(delta) {
                        self.redraw = true;
                    }
                    return;
                }
                if !rect_contains(right_bottom_rect, m.column, m.row) {
                    return;
                }
                if self.terminals.focused_terminal_tracks_mouse() {
                    let cell_col = m.column.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = m.row.saturating_sub(right_bottom_rect.y) as u32;
                    let button = if raw_up {
                        libghostty_vt::mouse::Button::Four
                    } else {
                        libghostty_vt::mouse::Button::Five
                    };
                    if let Some((terminal_id, bytes)) = self.terminals.encode_mouse(
                        libghostty_vt::mouse::Action::Press,
                        Some(button),
                        cell_col,
                        cell_row,
                    ) {
                        self.send_cmd(IpcCommand::Write { terminal_id, bytes });
                        self.redraw = true;
                        return;
                    }
                }
                let delta = if raw_up { -scaled } else { scaled };
                let _ = self.terminals.scroll_active(delta);
                self.redraw = true;
            }
            _ => {}
        }
    }
}
