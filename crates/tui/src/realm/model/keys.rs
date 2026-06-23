//! Keyboard + mouse + paste handlers.
//!
//! The big `handle_pane_key` matcher (catalog-dispatch + per-key
//! arms + terminal-escape latch), the quit-chord resolver, and the
//! mouse / paste handlers all live here.
//! Test-only `dispatch_*` entry points sit alongside so the
//! integration tests can drive the same code paths the run loop
//! does.
//!
//! Free helpers used only by this surface (`rect_contains`,
//! `key_event_to_chord`, `find_action_for_chord`,
//! `emit_clipboard_copy`) live in `mod.rs`
//! today and are reachable from this submodule because child
//! modules can see their parent's private items.

use super::{
    Id, Model, PaneFocus, emit_clipboard_copy, find_action_for_seq, find_action_for_stroke,
    key_event_to_stroke, rect_contains, seq_continuations,
};
use crate::realm::keymap::realm_key_to_crossterm;
use lazybox_ipc::Command as IpcCommand;
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
        // Snapshot the steady focus for the selected workspace before
        // this key mutates anything, so a later re-select can restore it
        // (#182).
        self.record_workspace_focus();
        // The sidebar's `/` search bar, while open, owns every
        // keystroke — route them straight in before pane / catalog /
        // global dispatch so typing a query (`f`, `s`, `Tab`, …) edits
        // text instead of firing a shortcut (including arming a leader
        // chord below). `Esc` / `Enter` close it from inside the same
        // handler.
        if self.focus == PaneFocus::Sidebar && self.sidebar.search_editing() {
            self.sidebar.handle_search_key(realm_key_to_crossterm(&key));
            // Filtering may have moved the selection; keep the right
            // pane / terminals in step.
            self.sync_panes();
            self.redraw = true;
            return;
        }
        // ── Leader-chord completion (issues #126, #102) ─────────────
        // When a leader prefix is armed, THIS key completes the
        // sequence — resolved before anything else so the second press
        // never leaks to a pane or PTY. The matching catalog entry
        // (`Chord::Seq([prefix, this])`) fires through the unified
        // `dispatch_action`; anything else (Esc, an unmapped key) just
        // cancels, which is the which-key convention. Which entry a
        // key completes is read straight from the catalog — no
        // hardcoded group table.
        if let Some(prefix) = self.leader.take() {
            self.redraw = true;
            let action = key_event_to_stroke(realm_key_to_crossterm(&key))
                .and_then(|stroke| find_action_for_seq(&prefix, &stroke, self.focus, &self.catalog))
                .and_then(action_from_entry);
            if let Some(action) = action {
                self.q_latch.disarm();
                let mut cmds = self.dispatch_action(&action);
                self.sync_panes();
                for cmd in cmds.drain(..) {
                    let rewritten = self.rewrite_spawn_to_inject(cmd);
                    self.send_cmd(rewritten);
                }
                return;
            }
            // The key matched no in-group action. `Esc` is the explicit
            // cancel and stops here; any other key cancels the chord but
            // falls through to normal dispatch so the keystroke isn't
            // silently swallowed (issue #165) — e.g. a mistyped `g x`
            // still runs `x`'s own action instead of vanishing.
            if key.code == Key::Esc {
                return;
            }
        }
        // ── Terminal `]]` leader (issue #205) ───────────────────────
        // Once `]]` arms the leader, THIS key selects a binding under
        // it and never reaches the PTY:
        //   - a printable char opens the snippet picker pre-filtered
        //     by that char (auto-submits on a unique match);
        //   - the escape char, Esc, or anything else cancels back into
        //     the terminal so the user can keep typing;
        //   - no key at all → the idle tick leaves the pane (see
        //     `tick_terminal_leader`).
        // The snippet library is the leader's binding set, mirroring
        // how the sidebar group-leader above resolves an `ActionGroup`.
        if self.terminal_leader_at.take().is_some() {
            self.redraw = true;
            if let Key::Char(c) = key.code
                && key.modifiers.is_empty()
                && !c.is_control()
                && c != self.ui_defaults.terminal_escape_char
            {
                // Reserved leader keys win over the snippet picker:
                //   - `]]<1..9>` jumps the view straight to the Nth
                //     agent workspace (sidebar order, top-down) — the
                //     numbered replacement for the old `F3` cycle;
                //   - `]]f` toggles focus mode without leaving the PTY,
                //     replacing the old `F2`.
                // Both shadow snippets bound to those keys (digits and
                // `f`), which is the documented trade-off for keeping
                // the jumps reachable heads-down. `` ` `` is reserved
                // the same way: it opens the fuzzy workspace switcher,
                // so "jump to any workspace" works without first
                // leaving the agent (issue #171).
                match c {
                    '1'..='9' => {
                        let n = c.to_digit(10).unwrap_or(0) as usize;
                        self.jump_to_agent_workspace(n);
                    }
                    'f' => self.toggle_focus_mode(),
                    '`' => self.mount_jump_picker(),
                    _ => self.mount_snippet_picker(c.to_string()),
                }
            }
            return;
        }
        match key.code {
            // Tab cycles panes — but ONLY when the active pane has
            // no PTY swallowing keys. Inside a terminal with a live
            // PTY, Tab belongs to the shell / agent; the `]]`
            // escape sequence is the only way out (tmux-style
            // prefix model). With no terminals running, Tab cycles
            // normally — there's no inner program to forward it to.
            Key::Tab
                if !key.modifiers.contains(KeyModifiers::SHIFT)
                    && !self.focus_mode
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
                // Skip the Activity pane when it's hidden — Tab should
                // never land focus on a pane the user can't see.
                if self.focus == PaneFocus::Right && !self.activity_pane_visible() {
                    self.focus = self.focus.next();
                }
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            _ if self.focus != PaneFocus::Terminals && self.matches_quit_chord(&key) => {
                // Quit chord (catalog `ActionKind::Quit`, default `q q`,
                // overridable via `ui.action_keys.quit`). A `Seq` is the
                // two-press latch; a single-keystroke `Key` remap fires
                // on first press.
                let chord = self.resolve_quit_chord();
                use lazybox_tui_core::action::Chord;
                if matches!(chord, Some(Chord::Key(_))) {
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
                // With no activity to show the pane is hidden, so jump
                // straight to the terminal — "open workspace → drive
                // the agent" in one keystroke. Otherwise focus the
                // Activity pane so the user can read / reply.
                self.focus = if self.activity_pane_visible() {
                    PaneFocus::Right
                } else {
                    PaneFocus::Terminals
                };
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
            // Toggle lazybox's mouse capture so the host terminal
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
            // Focus mode (#156). From the sidebar / activity panes it
            // rides the catalog dispatch below (the `.` binding). From
            // inside a terminal the PTY eats every key, so the entry
            // point there is the `]]` leader: `]]f` toggles focus mode
            // and `]]<digit>` jumps to a specific agent — both handled
            // in the leader block above. F-keys were dropped (#156
            // follow-up): macOS steals them for brightness / Mission
            // Control, so they never reliably reached lazybox.
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
                // Any other key disarms. If the quit hint was showing,
                // force a redraw so it clears even when the key itself
                // (e.g. Esc) doesn't otherwise repaint.
                if self.q_latch.is_armed() {
                    self.redraw = true;
                }
                self.q_latch.disarm();
            }
        }

        // Terminal-pane escape sequence + `]]` leader (issues #40,
        // #205). Two consecutive presses of the escape char inside a
        // terminal arm the leader; the first `]` is held back via
        // `escape_latch`.
        //   - `]]` then a binding key → snippet picker / shortcut.
        //   - `]]` then nothing → leave the pane on the idle tick.
        //   - a single `]` then a non-`]` key → flush a literal `]`
        //     to the PTY so `]` is typeable in code / arrays / md.
        // The escape-char branch is checked first so `]]` always wins
        // over the flush path.
        if self.focus == PaneFocus::Terminals
            && key.modifiers.is_empty()
            && matches!(key.code, Key::Char(c) if c == self.ui_defaults.terminal_escape_char)
        {
            let was_armed = self.escape_latch.is_armed();
            if self.escape_latch.tap(self.ui_defaults.escape_window) {
                // Second `]` within the window completes `]]`: arm the
                // leader so the next key can invoke a binding
                // (`]]<key>`). The leader always has something to offer
                // now — `]]f` toggles focus mode and `]]<digit>` jumps
                // to an agent even with no snippets configured — so it
                // arms unconditionally; the pane still leaves on the
                // idle tick if no key follows (see `tick_terminal_leader`).
                // (Previously a snippet-less `]]` left instantly.)
                self.terminal_leader_at = Some(std::time::Instant::now());
                self.redraw = true;
                return;
            }
            // `tap` returned false. If the latch was ALREADY armed, the
            // chord window had lapsed and `tap` re-armed for THIS press —
            // the previously-held `]` is stale and must be released as a
            // literal, else the first `]` of a slowly-typed `] … ]` pair
            // is silently dropped. (The new `]` is now the held one.)
            if was_armed {
                self.flush_held_escape_char();
            }
            return;
        }
        if self.focus == PaneFocus::Terminals && self.escape_latch.is_armed() {
            self.escape_latch.disarm();
            // A single `]` followed by a non-`]` key is a literal `]`
            // in the user's input, not a chord — flush the held `]`
            // to the PTY before the new key falls through to normal
            // dispatch below. Snippet invocation lives under the `]]`
            // leader now (issue #205), so a lone `]` is never
            // intercepted.
            self.flush_held_escape_char();
        }

        // We have a typed key already; skip the synthetic Event
        // round-trip and call the pane wrappers' direct entry points.
        let ct = realm_key_to_crossterm(&key);
        let mut cmds: Vec<IpcCommand> = Vec::new();

        // Catalog lookup first. A single keystroke that matches a
        // catalog entry (and that `dispatch_action` knows how to
        // handle) is fired here and the pane's direct handler is
        // skipped. If the keystroke instead *starts* a leader sequence
        // (`g` → the github actions), arm the leader so the next key
        // completes it. Per-key match arms in the panes still cover
        // what the catalog doesn't yet — see `dispatch_action`.
        //
        // An empty terminal pane has no PTY to feed, so its keys
        // resolve as if the sidebar were focused — that's what keeps
        // the empty-state hint's `c` / `s` / `x` shortcuts live now
        // that agent spawns are catalog rows rather than a sidebar arm.
        let resolve_focus = match self.focus {
            PaneFocus::Terminals if self.terminals.is_empty() => Some(PaneFocus::Sidebar),
            PaneFocus::Terminals => None,
            other => Some(other),
        };
        if let Some(rfocus) = resolve_focus
            && let Some(stroke) = key_event_to_stroke(ct)
        {
            let action =
                find_action_for_stroke(&stroke, rfocus, &self.catalog).and_then(action_from_entry);
            if let Some(action) = action {
                // Any catalog dispatch counts as "non-quit key" so
                // the q q chord resets.
                self.q_latch.disarm();
                let dispatched = self.dispatch_action(&action);
                // Drain queued cmds + early return — the catalog
                // handled the key, the pane shouldn't see it.
                self.flush_dispatched_cmds(dispatched);
                self.sync_panes();
                self.redraw = true;
                return;
            }
            // No single-key action — does this keystroke open a leader
            // group? If any catalog entry has a `Seq` starting with it
            // (reachable from this focus), arm the leader and show the
            // which-key popup. Purely catalog-driven: new leader groups
            // are data, not a `keys.rs` edit. Not armed from an empty
            // terminal pane (the leader completion keys off the real
            // focus, which is Terminals there).
            if self.focus != PaneFocus::Terminals
                && !seq_continuations(&stroke, rfocus, &self.catalog).is_empty()
            {
                self.q_latch.disarm();
                self.leader.arm(stroke);
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
        self.flush_dispatched_cmds(cmds);
        // Sidebar j/k changes selection — propagate to right + terminals.
        self.sync_panes();
        self.redraw = true;
    }

    /// Send the commands a key dispatch produced: arm the spawn
    /// spinner for any `Spawn` (worktree provisioning can take seconds
    /// on a cold clone, so the footer turns until `TerminalSpawned`),
    /// then rewrite spawn-into-running-agent to `InjectPrompt` and
    /// send. Shared by the catalog-dispatch path and the per-pane
    /// fallback so both surface the same feedback.
    fn flush_dispatched_cmds(&mut self, cmds: Vec<IpcCommand>) {
        for cmd in &cmds {
            if let IpcCommand::Spawn { kind, .. } = cmd {
                let label = match kind {
                    lazybox_ipc::TerminalKind::Shell => "shell".to_string(),
                    lazybox_ipc::TerminalKind::Agent(a) => a.to_string(),
                    other => format!("{other:?}").to_lowercase(),
                };
                self.status.note_spawning(label);
            }
        }
        for cmd in cmds {
            let rewritten = self.rewrite_spawn_to_inject(cmd);
            self.send_cmd(rewritten);
        }
    }

    /// Send a single literal escape char (`]`) to the focused terminal —
    /// the held first `]` that turned out NOT to start a `]]` chord.
    /// Shared by the three release paths: a non-`]` key followed, the
    /// chord window lapsed and re-armed on another `]`, or the idle tick
    /// timed the chord out ([`Self::tick_terminal_leader`]).
    pub(super) fn flush_held_escape_char(&mut self) {
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

    /// Returns true when the q-q latch is armed (used by the bottom
    /// hint bar to show "press q again" briefly).
    pub fn q_arm_pending(&self) -> bool {
        self.q_latch.is_armed()
    }

    /// The armed leader prefix keystroke, if any. Drives the which-key
    /// popup in `view`; also a test/inspection hook for the leader
    /// chords (#126, #102).
    pub fn leader_pending(&self) -> Option<lazybox_tui_core::action::KeyStroke> {
        self.leader.pending().copied()
    }

    /// Read-only accessor — which pane currently has focus. Used by
    /// tests + (in future) the bottom hint bar.
    pub fn focus(&self) -> PaneFocus {
        self.focus
    }

    /// Whether the terminal `]]` leader is currently armed. Drives the
    /// which-key popup in `view`; also a test/inspection hook for the
    /// #205 leader chord.
    pub fn terminal_leader_pending(&self) -> bool {
        self.terminal_leader_at.is_some()
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
    /// send into `modal_event_tx`, tick `app` until the modal
    /// produces a Msg (or a short deadline elapses), then `update`
    /// each Msg. Exists because `dispatch_key` calls `handle_pane_key`,
    /// which is gated on an empty modal stack and therefore can't
    /// exercise key handling for a mounted Confirm, Input, etc.
    ///
    /// NOT for the run loop: it blocks until the listener thread
    /// delivers the key (production forwards via
    /// `forward_modal_event` and picks the Msg up on a later
    /// iteration). The wait parks inside `tick`'s `recv_timeout` —
    /// no sleep/poll spin.
    pub fn dispatch_modal_key(&mut self, key: RealmKey) {
        let _ = self.modal_event_tx.send(RealmEvent::Keyboard(key));
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match self.app.tick(PollStrategy::Once(remaining)) {
                Ok(messages) if !messages.is_empty() => {
                    for msg in messages {
                        self.update(msg);
                    }
                    return;
                }
                // A no-Msg event (listener Tick, state-only key) —
                // keep waiting out the deadline for a real Msg.
                Ok(_) if !remaining.is_zero() => {}
                _ => return,
            }
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
                kind: lazybox_ipc::TerminalKind::Agent(agent_id),
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
                    let fallback_spawn = Some(lazybox_ipc::SpawnFallback {
                        session_key: session_key.clone(),
                        session_id,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
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
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id),
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

    /// Test accessor — read-only handle to the right-pane wrapper.
    /// Same contract as `__test_sidebar_mut`: integration tests only.
    #[doc(hidden)]
    pub fn __test_right(&self) -> &crate::realm::components::right::Right {
        &self.right
    }

    /// Look up the Quit chord — catalog default OR
    /// `ui.action_keys.quit` override. Returns the parsed `Chord`
    /// (`Seq` for `q q`, `Key` for a single-letter remap).
    fn resolve_quit_chord(&self) -> Option<lazybox_tui_core::action::Chord> {
        use lazybox_tui_core::action::{ActionDef, ActionKind};
        let def = ActionDef::for_kind(ActionKind::Quit);
        def.effective_chord(&self.action_key_overrides)
    }

    /// Matches the FIRST keystroke of the Quit chord (the entry-point
    /// for the latch). For a `Seq` (`q q`) this is its first stroke;
    /// for a `Key` it is the stroke itself.
    fn matches_quit_chord(&self, key: &RealmKey) -> bool {
        let Some(chord) = self.resolve_quit_chord() else {
            return false;
        };
        let Some(input) = key_event_to_stroke(realm_key_to_crossterm(key)) else {
            return false;
        };
        &input == chord.head()
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
        // Update the pinned-recap composing buffer for agent
        // terminals; without this, a pasted prompt would commit on
        // Enter as a blank recap (the keystroke tracker only sees the
        // CR, not the paste payload).
        self.terminals.record_paste(text);
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
        // Snapshot the steady focus for the selected workspace before
        // this click mutates anything, so a later re-select can restore
        // it (#182).
        self.record_workspace_focus();
        // In focus mode the sidebar + activity pane aren't drawn, so
        // their hit rects collapse to empty (no point can land in
        // them) and the terminal owns everything below the slim event
        // header — clicks and wheel events all route to the PTY. Keep
        // this split in lockstep with `view`'s focus-mode layout.
        // Outside focus mode, `effective_pane_rects` accounts for a
        // hidden Activity pane: when hidden, `right_top` is zero-height
        // so clicks there route to the terminal stack (and the
        // horizontal splitter disappears).
        let (sidebar_rect, right_top_rect, right_bottom_rect) = if self.focus_mode {
            let (pane_area, _) = super::split_for_footer(self.layout.last_area);
            let (_, body) = crate::realm::layout::focus_mode_areas(pane_area);
            (Rect::default(), Rect::default(), body)
        } else {
            self.effective_pane_rects(self.layout.last_area)
        };

        match m.kind {
            MouseEventKind::Down(button) => {
                self.q_latch.disarm();
                // Tab-strip click on the terminal pane top row →
                // switch active tab. Checked BEFORE the
                // "forward to inner program" path because the tab
                // strip belongs to lazybox, not to Claude/shell.
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
                        let session_key: lazybox_core::SessionKey = (&ws.key).into();
                        self.mount_sidebar_context_menu(session_key);
                    }
                    return;
                }
                // Right-click on a URL, file path, or issue reference
                // inside the terminal pane → open it (browser for URLs
                // and `#N` refs, the configured editor for paths). We
                // intercept BEFORE the PTY-forwarding path below so a
                // right-click on a link works the same regardless of
                // whether the inner program (claude, vim, …) would
                // otherwise grab the event. If nothing openable is
                // under the cursor we fall through to the normal
                // routing — the PTY still gets the right-click for
                // its own context menus.
                if matches!(button, crossterm::event::MouseButton::Right)
                    && rect_contains(right_bottom_rect, m.column, m.row)
                    && self
                        .layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                        .is_none()
                    && let Some(target) =
                        self.terminals.target_at(right_bottom_rect, m.column, m.row)
                {
                    self.open_click_target(target);
                    return;
                }
                // Splitter drag wins over both focus changes and
                // terminal interaction — clicking a splitter resizes,
                // it never refocuses or types into a pane.
                if let Some(target) =
                    self.layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                {
                    self.layout.active_drag = Some(target);
                    return;
                }
                // Move focus to the clicked pane BEFORE any
                // terminal-specific handling. The selection and
                // mouse-forwarding logic below keys off `self.focus`, so
                // focus has to be updated up front — otherwise the first
                // click after leaving the terminal only refocuses it and
                // the click never reaches the inner program, forcing a
                // redundant second click before typing works (#103).
                let target = if rect_contains(sidebar_rect, m.column, m.row) {
                    Some(PaneFocus::Sidebar)
                } else if rect_contains(right_top_rect, m.column, m.row) {
                    Some(PaneFocus::Right)
                } else if rect_contains(right_bottom_rect, m.column, m.row) {
                    Some(PaneFocus::Terminals)
                } else {
                    None
                };
                if let Some(focus) = target
                    && self.focus != focus
                {
                    self.focus = focus;
                    self.set_focus_attr();
                    self.redraw = true;
                }

                // A left-click in the terminal pane ALWAYS starts a
                // potential lazybox selection — we commit to that
                // even when the inner program is mouse-tracking.
                let claim_for_selection = rect_contains(right_bottom_rect, m.column, m.row)
                    && self.focus == PaneFocus::Terminals
                    && matches!(button, crossterm::event::MouseButton::Left);

                // Forward CLICK-down to mouse-tracking inner programs
                // only when we're NOT claiming for selection — i.e.,
                // non-left buttons. Left clicks are deferred.
                if !claim_for_selection
                    && rect_contains(right_bottom_rect, m.column, m.row)
                    && self.focus == PaneFocus::Terminals
                    && self.terminals.focused_terminal_tracks_mouse()
                    && let Some((cell_col, cell_row)) =
                        self.terminals
                            .screen_to_cell(right_bottom_rect, m.column, m.row)
                {
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
                if let Some(focus) = target {
                    if focus == PaneFocus::Sidebar {
                        // Try the header chips first (filter, then
                        // sort); if neither hit, fall through to row
                        // selection. All three outcomes update the
                        // same state, so one consolidated branch.
                        let prev_key = self.sidebar.selected_workspace_key().cloned();
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
                            let is_double = matches!(button, crossterm::event::MouseButton::Left)
                                && self
                                    .last_click
                                    .map(|(c, r, t)| {
                                        c == m.column
                                            && r == m.row
                                            && t.elapsed() <= crate::realm::DOUBLE_CLICK_WINDOW
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
                            // Clicking onto a *different* workspace restores
                            // the pane it was last focused in — so clicking
                            // back to a workspace whose agent you were typing
                            // into returns focus to that terminal instead of
                            // stranding it on the sidebar (#182). A click on
                            // the already-selected row keeps the sidebar
                            // focus the click just set, leaving an explicit
                            // way to step out of the terminal.
                            if self.sidebar.selected_workspace_key() != prev_key.as_ref() {
                                self.restore_workspace_focus();
                            }
                            self.redraw = true;
                        }
                    }
                    // Lazybox-side selection start: any left-click that
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
                        let is_double = matches!(button, crossterm::event::MouseButton::Left)
                            && self
                                .last_click
                                .map(|(c, r, t)| {
                                    c == m.column
                                        && r == m.row
                                        && t.elapsed() <= crate::realm::DOUBLE_CLICK_WINDOW
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
                    && let Some((cell_col, cell_row)) =
                        self.terminals.screen_to_cell(right_bottom_rect, col, row)
                {
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
                let raw_up = matches!(m.kind, MouseEventKind::ScrollUp);
                if rect_contains(right_top_rect, m.column, m.row) {
                    // Inertia damper: the OS-driven "flick keeps
                    // scrolling for 500ms after the gesture" phase
                    // decays its STEP and a reverse-direction gesture
                    // cancels the queued inertia. `dampen_scroll_step`
                    // returns 0 when the event should be dropped
                    // entirely (reverse mid-burst).
                    let scaled = self.dampen_scroll_step(raw_up);
                    if scaled == 0 {
                        return;
                    }
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
                    // Damped: every wheel event on this path is a
                    // daemon round trip + inner-program repaint, so
                    // the momentum tail must decay and hard-stop.
                    let scaled = self.dampen_scroll_step(raw_up);
                    if scaled == 0 {
                        return;
                    }
                    let cell = self
                        .terminals
                        .screen_to_cell(right_bottom_rect, m.column, m.row);
                    let encoded = cell.and_then(|(cell_col, cell_row)| {
                        let button = if raw_up {
                            libghostty_vt::mouse::Button::Four
                        } else {
                            libghostty_vt::mouse::Button::Five
                        };
                        self.terminals.encode_mouse(
                            libghostty_vt::mouse::Action::Press,
                            Some(button),
                            cell_col,
                            cell_row,
                        )
                    });
                    if let Some((terminal_id, bytes)) = encoded {
                        // One wheel notch encodes one line for the
                        // inner program, but the damper computed a
                        // multi-line step. Repeat the encoding
                        // `scaled` times in a SINGLE Write so the
                        // gesture moves the full step in one daemon
                        // round trip instead of N writes of one
                        // notch each.
                        let payload = bytes.repeat(scaled.max(1) as usize);
                        self.send_cmd(IpcCommand::Write {
                            terminal_id,
                            bytes: payload,
                        });
                        self.redraw = true;
                        return;
                    }
                    // Mouse-tracking flag was up but the event mapped to
                    // pane chrome or encoded to nothing — fall through to
                    // the local viewport with the damped step.
                    let delta = if raw_up { -scaled } else { scaled };
                    let _ = self.terminals.scroll_active(delta);
                    self.redraw = true;
                    return;
                }
                // Local scrollback (inner program is NOT tracking the
                // mouse): the viewport move is a pure in-process
                // libghostty call, no daemon round trip — so no
                // damper. Every OS wheel event moves a fixed small
                // step and the view stops exactly when the events
                // stop, like a native terminal.
                const LOCAL_WHEEL_STEP: isize = 3;
                let delta = if raw_up {
                    -LOCAL_WHEEL_STEP
                } else {
                    LOCAL_WHEEL_STEP
                };
                let _ = self.terminals.scroll_active(delta);
                self.redraw = true;
            }
            _ => {}
        }
    }
}

/// Reconstruct a runtime `Action` from a resolved catalog entry,
/// pulling the agent id out of a parameterized `SpawnAgent` row. This
/// is the bridge from the data-model catalog back to the dispatchable
/// `Action` enum — both single-key and leader-sequence resolution go
/// through it.
fn action_from_entry(
    entry: &lazybox_tui_core::action::CatalogEntry,
) -> Option<lazybox_tui_core::action::Action> {
    use lazybox_tui_core::action::{Action, ActionKind, Param};
    if entry.kind == ActionKind::SpawnAgent {
        return entry
            .param
            .as_ref()
            .map(|Param::Agent(id)| Action::SpawnAgent(id.clone()));
    }
    action_from_kind(entry.kind)
}

/// Reconstruct a runtime `Action` from a catalog `ActionKind`, for the
/// no-payload kinds the keyboard path dispatches (single keys AND
/// leader sequences resolve through here so both agree on semantics).
///
/// `SpawnAgent` carries the agent id as a `Param`, handled by
/// [`action_from_entry`]; reached here it has no payload, so `None`.
fn action_from_kind(
    kind: lazybox_tui_core::action::ActionKind,
) -> Option<lazybox_tui_core::action::Action> {
    use lazybox_tui_core::action::{Action, ActionKind};
    Some(match kind {
        ActionKind::SpawnShell => Action::SpawnShell,
        ActionKind::MarkAllRead => Action::MarkAllRead,
        ActionKind::Work => Action::Work,
        ActionKind::OpenEditor => Action::OpenEditor,
        ActionKind::NewWorkspace => Action::NewWorkspace,
        ActionKind::NewProject => Action::NewProject,
        ActionKind::MergePr => Action::MergePr,
        ActionKind::Archive => Action::Archive,
        ActionKind::ToggleSnooze => Action::ToggleSnooze,
        ActionKind::LongSnooze => Action::LongSnooze,
        ActionKind::Refresh => Action::Refresh,
        ActionKind::AdoptSessions => Action::AdoptSessions,
        ActionKind::CollapseIntoPr => Action::CollapseIntoPr,
        ActionKind::Reply => Action::Reply,
        ActionKind::RequestReviewers => Action::RequestReviewers,
        ActionKind::AddAssignees => Action::AddAssignees,
        ActionKind::ManageLabels => Action::ManageLabels,
        ActionKind::OpenInBrowser => Action::OpenInBrowser,
        ActionKind::CycleRoleFilter => Action::CycleRoleFilter,
        ActionKind::CycleSort => Action::CycleSort,
        ActionKind::CycleMailbox => Action::CycleMailbox,
        ActionKind::OpenSearch => Action::OpenSearch,
        ActionKind::OpenHelp => Action::OpenHelp,
        ActionKind::OpenTour => Action::OpenTour,
        ActionKind::OpenSyncStatus => Action::OpenSyncStatus,
        ActionKind::OpenSettings => Action::OpenSettings,
        ActionKind::JumpToWorkspace => Action::JumpToWorkspace,
        ActionKind::JumpToAsking => Action::JumpToAsking,
        ActionKind::JumpToFailingCi => Action::JumpToFailingCi,
        ActionKind::StartAgent => Action::StartAgent,
        ActionKind::ToggleActivityPane => Action::ToggleActivityPane,
        ActionKind::ToggleFocusMode => Action::ToggleFocusMode,
        _ => return None,
    })
}
