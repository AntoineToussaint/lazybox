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
    Id, Model, PaneFocus, TerminalDrag, emit_clipboard_copy, find_action_for_seq,
    find_action_for_stroke, key_event_to_stroke, rect_contains, seq_continuations,
};
use crate::realm::keymap::realm_key_to_crossterm;
use lazybox_ipc::Command as IpcCommand;
use std::time::Duration;
use tuirealm::application::PollStrategy;
use tuirealm::event::{Event as RealmEvent, Key, KeyEvent as RealmKey, KeyModifiers};
use tuirealm::ratatui::layout::Rect;
use tuirealm::terminal::TerminalAdapter;

impl lazybox_tui_core::dispatch::AgentTerminalView for crate::realm::components::sidebar::Sidebar {
    fn find_agent_terminal(
        &self,
        session_key: &lazybox_core::SessionKey,
        agent_id: &str,
    ) -> Option<lazybox_ipc::TerminalId> {
        crate::realm::components::sidebar::Sidebar::find_agent_terminal(self, session_key, agent_id)
    }
}

impl<T: TerminalAdapter> Model<T> {
    /// Top-level key handler when no modal is active. Routes Tab,
    /// global escapes, and forwards everything else to the focused
    /// pane wrapper.
    pub(super) fn handle_pane_key(&mut self, key: RealmKey) {
        // Snapshot the steady focus for the selected workspace before
        // this key mutates anything, so a later re-select can restore it
        // (#182).
        self.record_workspace_focus();
        // Any key pressed while the sidebar is focused re-anchors a
        // wheel-detached viewport (#290): keys act on — or move — the
        // selection, so the selection must be on screen. Only the
        // wheel detaches; only mouse scrolling keeps it detached.
        if self.focus == PaneFocus::Sidebar && self.sidebar.reanchor_viewport() {
            self.redraw = true;
        }
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
        if let Some(prefix) = self.leader.pending().copied() {
            self.redraw = true;
            let rfocus = self.resolve_focus_for_keys();
            let stroke = key_event_to_stroke(realm_key_to_crossterm(&key));
            // Direct-letter fast path (#126, #102), unchanged and always
            // first: a second key that names an in-group continuation
            // fires it straight away, so a bound `j`/`k`/arrow never gets
            // hijacked by the navigation below.
            let direct = rfocus.zip(stroke).and_then(|(rf, s)| {
                find_action_for_seq(&prefix, &s, rf, &self.catalog).and_then(action_from_entry)
            });
            if direct.is_none() {
                // Not a direct hit — arrow / `j` / `k` move a highlight
                // through the popup and keep the leader armed; `Enter`
                // fires the highlighted continuation (#343). Arrows are
                // additive; `j`/`k` only reach here because the guard
                // above found no in-group binding for them.
                if let Some(delta) = popup_nav_delta(&key) {
                    if let Some(rf) = rfocus {
                        let len = seq_continuations(&prefix, rf, &self.catalog).len();
                        if len > 0 {
                            self.leader_highlight =
                                Some(advance_highlight(self.leader_highlight, delta, len));
                        }
                    }
                    return;
                }
                if key.code == Key::Enter
                    && let Some(idx) = self.leader_highlight
                    && let Some(rf) = rfocus
                    && let Some(action) = seq_continuations(&prefix, rf, &self.catalog)
                        .get(idx)
                        .and_then(|(_, entry)| action_from_entry(entry))
                {
                    self.leader.take();
                    self.leader_highlight = None;
                    self.q_latch.disarm();
                    let cmds = self.dispatch_action(&action);
                    self.flush_dispatched_cmds(cmds);
                    self.sync_panes();
                    return;
                }
            }
            // Resolve the chord: consume the leader and clear the
            // highlight. A direct in-group hit fires; `Esc` is the
            // explicit cancel; any other key cancels but falls through to
            // normal dispatch so the keystroke isn't silently swallowed
            // (#165) — a mistyped `g x` still runs `x`'s own action.
            self.leader.take();
            self.leader_highlight = None;
            if let Some(action) = direct {
                self.q_latch.disarm();
                let cmds = self.dispatch_action(&action);
                self.flush_dispatched_cmds(cmds);
                self.sync_panes();
                return;
            }
            if key.code == Key::Esc {
                return;
            }
        }
        // ── Terminal `]]` leader (issues #205, #252) ────────────────
        // Once `]]` arms the leader, THIS key picks a command from a
        // small, fixed menu — each a single mnemonic, tmux-prefix style —
        // and never reaches the PTY:
        //   - `]]s` opens the snippet picker (typing a full key there
        //     still auto-submits — the `]]srev` fast path);
        //   - `]]f` toggles focus mode without leaving the PTY;
        //   - `]]q` exits the terminal back to the sidebar;
        //   - `]]<1..9>` jumps the view straight to the Nth agent
        //     workspace (sidebar order, top-down);
        //   - `` ]]` `` opens the fuzzy workspace switcher (issue #171);
        //   - `]]|` / `]]-` split the focused tile, `]]<arrow>` moves
        //     tile focus (cycles tabs in Tabs mode), `]]x` closes the
        //     focused terminal — tile management rides the same leader
        //     as everything else so terminal mode has exactly one
        //     lazybox prefix (#286, ex-`Ctrl-w`);
        //   - Esc or any unbound key cancels back into the terminal so
        //     the user can keep typing.
        // The leader is NOT timed (#252): a timed idle-leave used to
        // race the follow-up key, silently dropping the user to the
        // sidebar mid-decision. It now waits for THIS key. A fixed
        // command menu (rather than binding every snippet key straight
        // to the leader) means nothing shadows a snippet and the exit is
        // a plain mnemonic, not a triple-tap.
        if self.terminal_leader_armed {
            self.redraw = true;
            use super::terminal_leader::LeaderCmd;
            // `j` / `k` move a highlight through the popup and keep the
            // leader armed; `Enter` fires the highlighted command (#343).
            // Arrows stay bound to tile / tab movement (#286), so only
            // the letter keys navigate here — neither names a leader
            // command, so this never shadows one.
            if let Some(delta) = popup_letter_nav_delta(&key) {
                self.move_terminal_leader_highlight(delta);
                return;
            }
            if key.code == Key::Enter {
                self.terminal_leader_armed = false;
                let cmd = self
                    .terminal_leader_highlight
                    .take()
                    .and_then(|idx| {
                        self.terminal_leader_menu_rows()
                            .get(idx)
                            .and_then(|(k, _)| single_menu_char(k))
                    })
                    .and_then(|c| LeaderCmd::from_key(Key::Char(c), KeyModifiers::empty()));
                if let Some(cmd) = cmd {
                    let mut cmds = Vec::new();
                    self.run_terminal_leader_cmd(cmd, &mut cmds);
                    self.flush_dispatched_cmds(cmds);
                }
                return;
            }
            // Direct-key path: which key means which command lives in ONE
            // place (`terminal_leader::LeaderCmd`), shared with the
            // which-key popup's row list so dispatch and display can't
            // drift. A `None` key is not a leader command — cancel back
            // to the terminal (the key is consumed, not forwarded,
            // matching the tmux-prefix "unbound key does nothing"
            // convention).
            self.terminal_leader_armed = false;
            self.terminal_leader_highlight = None;
            let mut cmds = Vec::new();
            if let Some(cmd) = LeaderCmd::from_key(key.code, key.modifiers) {
                self.run_terminal_leader_cmd(cmd, &mut cmds);
            }
            self.flush_dispatched_cmds(cmds);
            return;
        }
        // ── Dismiss the current footer notice (#309) ────────────────
        // One key clears whatever notice is up, regardless of severity —
        // the merge false-error (#305) that sat red with no way to swat
        // it is the motivating case. Severity still decides auto-fade
        // (Retryable/Info fade on their timers, Permanent/Auth stay);
        // this just lets the user clear any of them now.
        //
        // The key is the catalog's `DismissNotice` binding (default
        // Esc, remappable). Sequenced here — after the leader / terminal
        // `]]` cancels above — so it never pre-empts a leader disarm.
        // It deliberately yields Esc to two owners: a live terminal
        // (`resolve_focus_for_keys` is None there, so Esc reaches the
        // PTY) and a sidebar multi-select (Esc drops the `v` selection
        // first). Gated on a notice actually being up, so with a quiet
        // footer the key keeps its normal pane meaning.
        if self.status.notice.is_some()
            && self.resolve_focus_for_keys().is_some()
            && self.matches_dismiss_notice(&key)
            && !(self.focus == PaneFocus::Sidebar && self.sidebar.broadcast_selected_count() > 0)
        {
            // Esc acknowledges an action-error toast: record it so the
            // same failure re-firing on the next poll/attempt stays
            // dismissed until its reason changes or the condition clears
            // (#832). No-op for plain system banners.
            self.status.remember_dismissed_action_error();
            self.status.notice = None;
            self.sync_error_source = None;
            self.redraw = true;
            return;
        }
        // ── Inspect the current sticky error (#453) ─────────────────
        // The footer pill width-caps its message, so a merge rejection
        // or spawn failure renders truncated and unreadable ("… never
        // dismiss, offer no action, and truncate"). While a sticky
        // error is pinned, the `InspectNotice` binding (default Enter,
        // remappable) pops the full text in a wrapped detail modal.
        // Gated on a *sticky* notice, not any: the transient Info/Hint
        // notices auto-fade and are up too often for Enter to lose its
        // pane meaning. Sequenced after the dismiss branch — an Esc/Enter
        // remap collision resolves to dismiss first — and before the
        // per-pane Enter arms, which it deliberately shadows only while
        // an error is on screen.
        if self
            .status
            .notice
            .as_ref()
            .is_some_and(|n| n.severity.is_sticky())
            && self.resolve_focus_for_keys().is_some()
            && self.matches_inspect_notice(&key)
        {
            self.inspect_notice();
            return;
        }
        match key.code {
            // Cycle panes — the catalog's `CyclePane` chord (Tab by
            // default, `Ctrl-w` under the vim preset, remappable via
            // `ui.action_keys.cycle_pane`) — but ONLY when the active
            // pane has no PTY swallowing keys. Inside a terminal with a
            // live PTY the chord belongs to the shell / agent (Tab
            // autocomplete, readline `Ctrl-w`); the `]]` escape
            // sequence is the only way out (tmux-style prefix model).
            // With no terminals running (or fresh entry, no typing
            // yet) it cycles normally — there's no inner program to
            // forward it to. Kept as a guard arm rather than plain
            // catalog dispatch because the terminal gating below needs
            // to apply even where the catalog can't resolve (a live
            // but untyped terminal pane).
            _ if self.matches_cycle_pane(&key)
                && !self.focus_mode
                && (self.focus != PaneFocus::Terminals
                    || self.terminals.is_empty()
                    || !self.terminal_user_typed_since_focus) =>
            {
                self.q_latch.disarm();
                self.cycle_pane_focus();
                return;
            }
            // Completion of a two-DISTINCT-stroke quit override
            // (`quit: "q x"`): the head press armed the latch below;
            // this press is the declared second stroke. Checked before
            // the head arm so the second stroke can't be re-read as a
            // fresh head press, and before catalog / pane dispatch so
            // it never leaks as its own action.
            _ if self.focus != PaneFocus::Terminals && self.quit_seq_completes(&key) => {
                self.q_latch.disarm();
                self.quit = true;
                return;
            }
            _ if self.focus != PaneFocus::Terminals && self.matches_quit_chord(&key) => {
                // Quit chord (catalog `ActionKind::Quit`, default `q q`,
                // overridable via `ui.action_keys.quit`). A `Seq` of the
                // same stroke twice is the two-press latch; a
                // single-keystroke `Key` remap fires on first press; a
                // `Seq` of two distinct strokes arms here and completes
                // in the branch above (`quit: "q x"` must quit on
                // `q x`, not on `q q`).
                use lazybox_tui_core::action::Chord;
                match self.resolve_quit_chord() {
                    Some(Chord::Key(_)) => {
                        self.quit = true;
                        return;
                    }
                    Some(Chord::Seq(ref s)) if s.len() >= 2 && s[0] != s[1] => {
                        // Head of a distinct-stroke sequence: (re)arm
                        // only — a repeat of the head must never fire.
                        self.q_latch.disarm();
                        let _ = self.q_latch.tap(self.ui_defaults.quit_double_tap_window);
                        self.redraw = true;
                        return;
                    }
                    _ => {
                        if self.q_latch.tap(self.ui_defaults.quit_double_tap_window) {
                            self.quit = true;
                            return;
                        }
                        self.redraw = true;
                        return;
                    }
                }
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
                self.set_focus(if self.activity_pane_visible() {
                    PaneFocus::Right
                } else {
                    PaneFocus::Terminals
                });
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
            // splitter drag etc. The catalog row
            // (`ActionKind::ToggleMouseCapture`) ships three default
            // chords because terminals report Ctrl-Shift-S
            // inconsistently and Ctrl-S itself is XOFF flow control:
            //   - F8         — function key, never conflicts with TTY
            //   - Alt-s      — Option-s on Mac (Alt-s elsewhere)
            //   - Ctrl-Alt-s — extra fallback for non-mac users
            // Handled as a guard arm (not plain catalog dispatch) so it
            // stays reachable from a LIVE terminal — users in claude
            // escape to a copy gesture without `]]` first — while the
            // catalog row keeps it visible in `?` help and remappable
            // via `ui.action_keys.toggle_mouse_capture`.
            _ if self.matches_mouse_capture(&key) => {
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
                // leader so the next key picks a command (`]]s` snippets,
                // `]]f` focus, `]]q` exit, `]]<digit>` jump). It arms
                // unconditionally — the menu always has something to
                // offer. Non-timed (#252): it waits for the next key
                // rather than leaving on an idle tick, so browsing never
                // races the exit.
                self.terminal_leader_armed = true;
                self.terminal_leader_highlight = None;
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
        // the empty-state hint's `s` / `a c` shortcuts live now that
        // agent spawns are catalog rows rather than a sidebar arm.
        if let Some(rfocus) = self.resolve_focus_for_keys()
            && let Some(stroke) = key_event_to_stroke(ct)
        {
            let action =
                find_action_for_stroke(&stroke, rfocus, &self.catalog).and_then(action_from_entry);
            // Does this keystroke open a leader group? Any catalog entry
            // with a `Seq` starting with it (reachable from this focus)
            // makes it a leader. Most leaders (`g`, `a`) have no direct
            // action of their own. `w` is a regular deterministic leader:
            // `w w` chooses the default/running agent, while `w c` / `w x`
            // choose one explicitly. There is no timeout and therefore no
            // input delay or action that fires after the user's context has
            // moved. The which-key popup comes for free from `self.leader`.
            // Completion resolves
            // through `resolve_focus_for_keys` too, so a leader armed
            // from an empty terminal pane completes against the same
            // sidebar scope it armed under.
            // Only continuations the completion path can actually fire
            // (`action_from_entry`) make a stroke a leader. Quit's `q q`
            // is a `Seq` too, but it dispatches through the q-latch, not
            // the catalog — arming on it (reachable from an empty
            // terminal pane, where the quit branch is skipped) would
            // show a popup whose completion goes nowhere.
            let opens_leader = action.is_none()
                && seq_continuations(&stroke, rfocus, &self.catalog)
                    .iter()
                    .any(|(_, entry)| action_from_entry(entry).is_some());
            if opens_leader {
                self.q_latch.disarm();
                self.leader.arm(stroke);
                self.leader_highlight = None;
                self.redraw = true;
                return;
            }
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
        }

        match self.focus {
            PaneFocus::Sidebar => self.sidebar.handle_key_direct(ct, &mut cmds),
            PaneFocus::Right => self.right.handle_key_direct(ct, &mut cmds),
            // Terminals pane with NO active terminal can't route to a
            // PTY. The empty-state hint says "press s for shell, a c
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
        // A `d` on an overflowing description preview asks to read the
        // whole thing in the reader modal (#448) — the pane can't mount
        // it, so drain the request here.
        if self.right.take_open_description() {
            self.open_focused_description();
        }
        // Sidebar j/k changes selection — propagate to right + terminals.
        self.sync_panes();
        self.redraw = true;
    }

    /// Mount the full-description reader modal (#448) for whatever the
    /// activity pane is focused on. No-op when there's no body.
    pub(super) fn open_focused_description(&mut self) {
        if let (Some(title), Some(body)) = (self.right.task_body_title(), self.right.task_body()) {
            self.mount_description_modal(title, body);
        }
    }

    /// Send the commands a key dispatch produced: arm the spawn
    /// spinner for any `Spawn` (worktree provisioning can take seconds
    /// on a cold clone, so the footer turns until `TerminalSpawned`),
    /// then rewrite spawn-into-running-agent to `InjectPrompt` and
    /// send. Shared by the catalog-dispatch path and the per-pane
    /// fallback so both surface the same feedback.
    pub(super) fn flush_dispatched_cmds(&mut self, cmds: Vec<IpcCommand>) {
        for cmd in &cmds {
            if let IpcCommand::Spawn {
                kind, session_key, ..
            } = cmd
            {
                // Remember the spawn so a failed provision's `r` retry
                // can re-issue it verbatim (issue #557).
                self.last_spawn = Some(cmd.clone());
                let label = match kind {
                    lazybox_ipc::TerminalKind::Shell => "shell".to_string(),
                    lazybox_ipc::TerminalKind::Agent(a) => a.to_string(),
                    other => format!("{other:?}").to_lowercase(),
                };
                // Capture the session's current terminal count so the
                // spinner — a projection of live terminal state — knows a
                // fresh terminal has appeared (see
                // `recompute_spawn_spinner`).
                let baseline = self.terminals.terminal_count_for(session_key);
                self.status
                    .note_spawning(label, session_key.clone(), kind.clone(), baseline);
            }
            if let IpcCommand::InjectPrompt {
                prompt,
                fallback_spawn: Some(fallback),
                ..
            } = cmd
            {
                self.last_spawn = Some(IpcCommand::Spawn {
                    model_alias: fallback.model_alias.clone(),
                    access: fallback.access,
                    session_key: fallback.session_key.clone(),
                    session_id: fallback.session_id,
                    client_request_id: fallback.client_request_id.clone(),
                    kind: fallback.kind.clone(),
                    cwd: fallback.cwd.clone(),
                    initial_prompt: Some(prompt.clone()),
                    on_main: false,
                });
            }
        }
        let planned = lazybox_tui_core::dispatch::plan_dispatch(cmds.clone(), &self.sidebar);
        for (original, rewritten) in cmds.into_iter().zip(planned) {
            if let (
                IpcCommand::Spawn {
                    kind: lazybox_ipc::TerminalKind::Agent(agent_id),
                    initial_prompt: Some(_),
                    ..
                },
                IpcCommand::InjectPrompt { .. },
            ) = (&original, &rewritten)
            {
                self.flash_hint(format!("→ injecting into existing {agent_id}"));
            }
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

    /// The `]]` leader popup's rows in display order: the fixed command
    /// menu (tailored to the current tile / tab layout) followed by the
    /// agent-jump roster (`]]<digit>` → workspace name). The single
    /// source of truth for BOTH the popup renderer and highlight
    /// navigation, so the two can't drift (#343).
    pub(super) fn terminal_leader_menu_rows(&self) -> Vec<(String, String)> {
        let mut rows = super::terminal_leader::LeaderCmd::menu_rows(
            self.terminals.layout_is_splits(),
            self.terminals.visible_terminal_count(),
            self.terminals.terminal_new_layout(),
        );
        rows.extend(
            self.sidebar
                .agent_workspace_keys()
                .into_iter()
                .take(9)
                .enumerate()
                .filter_map(|(i, k)| {
                    self.sidebar
                        .workspace_by_key(&k)
                        .map(|w| ((i + 1).to_string(), w.name.clone()))
                }),
        );
        rows
    }

    /// Move the `]]` leader highlight by `delta`, wrapping. Only rows
    /// with a single-char key are landing spots — the `←↓↑→` tile-move
    /// aggregate has no single `Enter`-fireable key, so it's skipped —
    /// and navigation is bounded to the rows the popup actually shows
    /// (the tail collapses into "+N more"), so the highlight is always
    /// visible (#343).
    fn move_terminal_leader_highlight(&mut self, delta: i32) {
        let rows = self.terminal_leader_menu_rows();
        let visible = rows
            .len()
            .min(crate::realm::components::which_key::LEADER_MAX_ROWS);
        let selectable: Vec<usize> = (0..visible)
            .filter(|&i| single_menu_char(&rows[i].0).is_some())
            .collect();
        if selectable.is_empty() {
            return;
        }
        let pos = self
            .terminal_leader_highlight
            .and_then(|h| selectable.iter().position(|&i| i == h));
        let next = match pos {
            None if delta > 0 => 0,
            None => selectable.len() - 1,
            Some(p) => (p as i32 + delta).rem_euclid(selectable.len() as i32) as usize,
        };
        self.terminal_leader_highlight = Some(selectable[next]);
        self.redraw = true;
    }

    /// Run a resolved `]]` leader command. Shared by the direct-key
    /// path and the `Enter`-fires-the-highlight path (#343) so both
    /// dispatch identically.
    fn run_terminal_leader_cmd(
        &mut self,
        cmd: super::terminal_leader::LeaderCmd,
        cmds: &mut Vec<IpcCommand>,
    ) {
        use super::terminal_leader::LeaderCmd;
        use crate::components::terminal_stack::PendingSplit;
        match cmd {
            LeaderCmd::JumpAgent(n) => self.jump_to_agent_workspace(n),
            LeaderCmd::Snippets => self.mount_snippet_picker(String::new()),
            LeaderCmd::Skills => self.mount_skill_picker(String::new()),
            LeaderCmd::RecallPrompt => self.recall_prompt(cmds),
            LeaderCmd::PromptHistory => self.mount_prompt_history_picker(),
            LeaderCmd::OpenUrls => self.open_terminal_urls(),
            LeaderCmd::ToggleFocusMode => self.toggle_focus_mode(),
            LeaderCmd::ExitToSidebar => self.leave_terminal_to_sidebar(),
            LeaderCmd::JumpPicker => self.mount_jump_picker(),
            LeaderCmd::SplitVertical => self.terminals.split_tile(PendingSplit::Vertical, cmds),
            LeaderCmd::SplitHorizontal => self.terminals.split_tile(PendingSplit::Horizontal, cmds),
            LeaderCmd::MoveTile(dir) => self.terminals.move_tile_focus(dir, cmds),
            LeaderCmd::CloseTerminal => self.terminals.close_focused_tile(cmds),
            LeaderCmd::ToggleNewLayout => self.toggle_terminal_new_layout(),
        }
    }

    /// `]]t` — flip the new-terminal layout preference live and persist
    /// it to `ui.terminal_new_layout` so it survives restart. The
    /// runtime flip lands first (it can't fail); a write error only
    /// costs persistence, which we surface but don't roll back — the
    /// user's explicit toggle still holds for this session.
    fn toggle_terminal_new_layout(&mut self) {
        let now = self.terminals.toggle_terminal_new_layout();
        let word = match now {
            lazybox_config::NewTerminalLayout::Split => "split",
            lazybox_config::NewTerminalLayout::Tabs => "tabs",
        };
        match lazybox_config::Config::save_with(|c| c.ui.terminal_new_layout = now) {
            Ok(()) => self.flash_info(format!("new terminals open as {word}")),
            Err(e) => self.flash_info(format!("new terminals open as {word} (couldn't save: {e})")),
        }
        self.redraw = true;
    }

    /// `]]r` — drop the last prompt (in-flight draft, else last
    /// submitted message) back into the focused agent's composer without
    /// submitting it, so the user can edit and re-send. Both sources are
    /// restored from the daemon snapshot, so this recovers a prompt lost
    /// to a restart (issue #373). The daemon pastes without the settle-
    /// gated Enter (`submit: false`).
    fn recall_prompt(&mut self, cmds: &mut Vec<IpcCommand>) {
        match self.terminals.recall_prompt() {
            Some((terminal_id, prompt)) => {
                cmds.push(IpcCommand::InjectPrompt {
                    terminal_id,
                    prompt: prompt.clone(),
                    fallback_spawn: None,
                    submit: false,
                });
                cmds.push(IpcCommand::RecordComposingBuffer {
                    terminal_id,
                    buffer: prompt,
                });
            }
            None => self.flash_info("nothing to recall"),
        }
    }

    /// The focus catalog lookups resolve under, given the real pane
    /// focus. An empty terminal pane has no PTY to feed, so its keys
    /// resolve as if the sidebar were focused; a live terminal never
    /// reaches the catalog (`None`). Shared by single-stroke dispatch,
    /// leader arming + completion, and the which-key popup so all four
    /// agree on scope.
    pub(super) fn resolve_focus_for_keys(&self) -> Option<PaneFocus> {
        match self.focus {
            PaneFocus::Terminals if self.terminals.is_empty() => Some(PaneFocus::Sidebar),
            PaneFocus::Terminals => None,
            other => Some(other),
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

    /// The highlighted row in the armed catalog leader popup, or `None`
    /// for the direct-letter default. Test/inspection hook for the
    /// arrow / `j` / `k` navigation (#343).
    pub fn leader_highlight(&self) -> Option<usize> {
        self.leader_highlight
    }

    /// The highlighted row in the armed `]]` leader popup, or `None` for
    /// the direct-key default. Test/inspection hook (#343).
    pub fn terminal_leader_highlight(&self) -> Option<usize> {
        self.terminal_leader_highlight
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
        self.terminal_leader_armed
    }

    /// Cancel any armed leader chord — the catalog groups (`g`, `w`,
    /// `x`, …) or the non-timed terminal `]]` leader (#252).
    /// Called on mouse interaction: a click is an unambiguous "I'm doing
    /// something else" signal. Without it a non-timed leader would stay
    /// armed and swallow the next key as a menu choice. Keystrokes resolve
    /// each leader through its completion block instead, so they don't
    /// need this.
    pub(super) fn cancel_leader_chords(&mut self) {
        if self.leader.is_armed() || self.terminal_leader_armed {
            self.redraw = true;
        }
        self.leader.disarm();
        self.leader_highlight = None;
        self.terminal_leader_armed = false;
        self.terminal_leader_highlight = None;
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

    /// Rewrite a contextual work spawn to an exact running terminal.
    /// The caller has already resolved the terminal choice, so this
    /// method never performs another agent-id lookup.
    pub(super) fn rewrite_spawn_to_terminal(
        &mut self,
        cmd: IpcCommand,
        terminal_id: lazybox_ipc::TerminalId,
    ) -> IpcCommand {
        if let IpcCommand::Spawn {
            kind: lazybox_ipc::TerminalKind::Agent(agent_id),
            initial_prompt: Some(_),
            access: lazybox_ipc::AgentRunAccess::Default,
            ..
        } = &cmd
        {
            self.flash_hint(format!("→ injecting into existing {agent_id}"));
        }
        lazybox_tui_core::dispatch::plan_spawn_for_terminal(cmd, terminal_id)
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

    /// Whether `key` is the effective `DismissNotice` binding (default
    /// `Esc`, overridable via `ui.action_keys.dismiss_notice`). A
    /// single keystroke — no `Seq` — so we compare the chord head.
    fn matches_dismiss_notice(&self, key: &RealmKey) -> bool {
        use lazybox_tui_core::action::{ActionDef, ActionKind};
        let Some(chord) = ActionDef::for_kind(ActionKind::DismissNotice)
            .effective_chord(&self.action_key_overrides)
        else {
            return false;
        };
        let Some(input) = key_event_to_stroke(realm_key_to_crossterm(key)) else {
            return false;
        };
        &input == chord.head()
    }

    /// Whether `key` is the effective `InspectNotice` binding (default
    /// `Enter`, overridable via `ui.action_keys.inspect_notice`). A
    /// single keystroke — no `Seq` — so we compare the chord head.
    fn matches_inspect_notice(&self, key: &RealmKey) -> bool {
        use lazybox_tui_core::action::{ActionDef, ActionKind};
        let Some(chord) = ActionDef::for_kind(ActionKind::InspectNotice)
            .effective_chord(&self.action_key_overrides)
        else {
            return false;
        };
        let Some(input) = key_event_to_stroke(realm_key_to_crossterm(key)) else {
            return false;
        };
        &input == chord.head()
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

    /// Whether `key` completes an armed two-DISTINCT-stroke quit
    /// sequence (`quit: "q x"`): the latch was armed by the head
    /// within the double-tap window and this stroke equals the
    /// declared second stroke. Always false for the default `q q`
    /// (same-stroke sequences stay on the tap latch) and for a
    /// single-`Key` remap.
    fn quit_seq_completes(&self, key: &RealmKey) -> bool {
        use lazybox_tui_core::action::Chord;
        if !self.q_latch.is_armed()
            || self
                .q_latch
                .armed_past(self.ui_defaults.quit_double_tap_window)
        {
            return false;
        }
        let Some(Chord::Seq(strokes)) = self.resolve_quit_chord() else {
            return false;
        };
        if strokes.len() < 2 || strokes[0] == strokes[1] {
            return false;
        }
        let Some(input) = key_event_to_stroke(realm_key_to_crossterm(key)) else {
            return false;
        };
        input == strokes[1]
    }

    /// Whether `key` is the effective `CyclePane` binding (default
    /// `Tab`, `Ctrl-w` under the vim preset, remappable via
    /// `ui.action_keys.cycle_pane`). Every ` | ` alternative is
    /// honored; `Seq` alternatives are ignored (pane cycling is a
    /// single-stroke action).
    fn matches_cycle_pane(&self, key: &RealmKey) -> bool {
        self.matches_single_key_binding(lazybox_tui_core::action::ActionKind::CyclePane, key)
    }

    /// Whether `key` is one of the effective `ToggleMouseCapture`
    /// chords (defaults F8 / Alt-s / Ctrl-Alt-s, remappable via
    /// `ui.action_keys.toggle_mouse_capture`).
    fn matches_mouse_capture(&self, key: &RealmKey) -> bool {
        self.matches_single_key_binding(
            lazybox_tui_core::action::ActionKind::ToggleMouseCapture,
            key,
        )
    }

    /// Shared matcher for the explicit-branch actions that read their
    /// chord from the catalog: true when `key` equals any single-`Key`
    /// alternative of `kind`'s effective binding.
    fn matches_single_key_binding(
        &self,
        kind: lazybox_tui_core::action::ActionKind,
        key: &RealmKey,
    ) -> bool {
        use lazybox_tui_core::action::{ActionDef, Chord};
        let Some(input) = key_event_to_stroke(realm_key_to_crossterm(key)) else {
            return false;
        };
        ActionDef::for_kind(kind)
            .effective_chords(&self.action_key_overrides)
            .iter()
            .any(|c| matches!(c, Chord::Key(k) if k == &input))
    }

    /// Move focus to the next visible pane — the `CyclePane` effect,
    /// shared by the guard arm in `handle_pane_key` and the catalog
    /// dispatch (`dispatch_action`).
    pub(super) fn cycle_pane_focus(&mut self) {
        let mut next = self.focus.next();
        // Skip the Activity pane when it's hidden — cycling should
        // never land focus on a pane the user can't see.
        if next == PaneFocus::Right && !self.activity_pane_visible() {
            next = next.next();
        }
        self.set_focus(next);
        self.redraw = true;
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
        let draft = self.terminals.record_paste(text);
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        self.send_cmd(IpcCommand::Write {
            terminal_id,
            bytes,
            intent: lazybox_ipc::TerminalInputIntent::Compose,
        });
        // Persist the pasted-but-unsubmitted draft (issue #373) so a
        // restart can recall it — the same path key-by-key typing takes.
        if let Some((id, buffer)) = draft {
            self.send_cmd(IpcCommand::RecordComposingBuffer {
                terminal_id: id,
                buffer,
            });
        }
        self.redraw = true;
    }

    /// A mouse press while a *dismissable* modal (a read-only or
    /// progress overlay, never a destructive confirm — see
    /// `Id::dismissable_by_outside_click`) is up closes it, exactly
    /// like Esc, and then lets the click do its normal thing. So
    /// clicking a sidebar workspace while the worktree-provisioning
    /// checklist is up both dismisses the checklist and selects that
    /// workspace in one action, instead of the modal hijacking the
    /// click.
    ///
    /// Returns whether it handled the event. Only left/right/middle
    /// *presses* qualify — scroll and drag fall through to the normal
    /// forwarding path (where the wheel-gate in `dispatch_event` decides
    /// whether the top modal actually consumes the notch; today only the
    /// description reader does). A blocking modal, or no modal, returns
    /// `false` so the caller forwards to the modal / pane as before.
    pub fn dismiss_modal_on_outside_click(&mut self, m: crossterm::event::MouseEvent) -> bool {
        use crossterm::event::MouseEventKind;
        if !matches!(m.kind, MouseEventKind::Down(_)) {
            return false;
        }
        let dismissable = self
            .modal_stack
            .last()
            .is_some_and(Id::dismissable_by_outside_click);
        if !dismissable {
            return false;
        }
        // Close it the same way Esc does (per-modal cleanup + draining
        // any queued daemon prompt) — with one exception: an outside
        // click *backgrounds* an in-flight worktree provision, it does
        // not abort it. Esc on the checklist is a deliberate "cancel
        // this wedged clone" gesture (#403); clicking a sidebar row to
        // go do something else is not, and must never kill the spawn the
        // user just started. Drop the `CancelSpawn` the shared dismiss
        // path queues for that case — the provision keeps running and
        // its later progress events are absorbed silently (the checklist
        // state is already recorded as dismissed).
        let cmds = self
            .handle_modal_dismissed()
            .into_iter()
            .filter(|c| !matches!(c, IpcCommand::CancelSpawn { .. }))
            .collect();
        self.dispatch_cmds(cmds);
        // Fall through to normal pane hit-testing only when nothing is
        // left on the stack — a dismissable overlay stacked over
        // another modal must reveal that modal, not click into a pane
        // behind it.
        if self.modal_stack.is_empty() {
            self.handle_mouse(m);
        }
        true
    }

    /// Mouse routing:
    /// - Down on a splitter line → start drag (resize panes on
    ///   subsequent Drag events until Up).
    /// - Down anywhere else → focus the pane the click landed in.
    /// - Up → end the active drag.
    /// - ScrollUp/Down over the terminal pane → move the terminal's
    ///   lazybox scrollback (libghostty handles the actual move).
    pub fn handle_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;

        self.note_host_mouse_input();
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
        // header. Keep
        // this split in lockstep with `view`'s focus-mode layout.
        // Outside focus mode, `effective_pane_rects` accounts for a
        // hidden Activity pane: when hidden, `right_top` is zero-height
        // so mouse events there target the terminal stack (and the
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
                // A click landing anywhere off the `/` search input
                // dismisses the search instead of trapping keystrokes
                // in it — the mouse counterpart of `Esc` (#780). This
                // runs BEFORE any button/capture-specific early return
                // below so every down-click frees the user, not just a
                // left-click into a pane: a right-click, or any click
                // while host-native mouse mode short-circuits, still
                // exits find. The click then falls through to its
                // normal routing. Clicks on the input itself (bottom
                // bar or header search chip) are left alone so they
                // keep editing.
                if self.sidebar.search_editing()
                    && !self.sidebar.search_bar_hit(m.column, m.row)
                    && !self.sidebar.search_chip_hit(m.column, m.row)
                {
                    self.sidebar.dismiss_search();
                    self.sync_panes();
                    self.redraw = true;
                }
                // A left-click on the footer's `… +N ? all` overflow
                // cell opens `?` so the elided hints are reachable — the
                // count is no longer a dead end (#805). The footer sits
                // outside every pane rect, so this is the only handler
                // that claims the click; checked before pane routing.
                if matches!(button, crossterm::event::MouseButton::Left)
                    && self
                        .footer_overflow_rect
                        .is_some_and(|r| rect_contains(r, m.column, m.row))
                {
                    self.q_latch.disarm();
                    self.cancel_leader_chords();
                    self.mount_help_ask();
                    self.redraw = true;
                    return;
                }
                let right_click_span =
                    matches!(button, crossterm::event::MouseButton::Right).then(|| {
                        tracing::info_span!("terminal_right_click", column = m.column, row = m.row)
                    });
                let _right_click_guard = right_click_span.as_ref().map(|span| span.enter());
                if right_click_span.is_some() {
                    let pane = if rect_contains(sidebar_rect, m.column, m.row) {
                        "sidebar"
                    } else if rect_contains(right_top_rect, m.column, m.row) {
                        "activity"
                    } else if rect_contains(right_bottom_rect, m.column, m.row) {
                        "terminals"
                    } else {
                        "outside"
                    };
                    tracing::info!(
                        pane,
                        mouse_capture_on = self.mouse_capture_on,
                        "right-button event received"
                    );
                    if !self.mouse_capture_on {
                        tracing::info!(
                            reason = "host_native_mouse_mode",
                            "terminal right-click missed"
                        );
                        self.flash_info(
                            "mouse: host selection — right-click links are off; press F8 or Alt-s to enable",
                        );
                        return;
                    }
                }
                self.q_latch.disarm();
                self.cancel_leader_chords();
                // Tab-strip click on the terminal pane top row →
                // switch active tab. Checked BEFORE the
                // "forward to inner program" path because the tab
                // strip belongs to lazybox, not to Claude/shell.
                if matches!(button, crossterm::event::MouseButton::Left)
                    && let Some(idx) = self.terminals.tab_at(m.column, m.row)
                {
                    self.terminals.set_active_tab(idx);
                    self.set_focus(PaneFocus::Terminals);
                    self.redraw = true;
                    return;
                }
                // Left-click on the slim summary line (Summary mode) →
                // expand the pane back to the full feed. The summary is
                // a non-focusable header, so a plain focus change would
                // just bounce off `enforce_pane_focus`; instead treat
                // the click as "expand me" and record the Full override.
                if matches!(button, crossterm::event::MouseButton::Left)
                    && self.activity_pane_mode() == lazybox_config::ActivityPaneMode::Summary
                    && rect_contains(right_top_rect, m.column, m.row)
                    && let Some(ws_key) = self.sidebar.selected_workspace().map(|w| w.key.clone())
                {
                    self.activity_pane.expand(ws_key);
                    self.set_focus(PaneFocus::Right);
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
                    self.set_focus(PaneFocus::Sidebar);
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
                // The horizontal splitter is only a live resize handle
                // while the activity pane is full — in Summary / Hidden
                // its 1-row / 0-row seam must not grab drags.
                let horizontal_splitter = self.activity_pane_visible();
                if matches!(button, crossterm::event::MouseButton::Right)
                    && rect_contains(right_bottom_rect, m.column, m.row)
                {
                    if self
                        .layout
                        .hit_test_splitter(
                            m.column,
                            m.row,
                            sidebar_rect,
                            right_top_rect,
                            horizontal_splitter,
                        )
                        .is_some()
                    {
                        tracing::info!(reason = "pane_splitter", "terminal right-click missed");
                    } else {
                        match self.terminals.rendered_target_at(m.column, m.row) {
                            Some(resolved) => {
                                tracing::info!(
                                    terminal_id = ?resolved.terminal_id,
                                    tile = ?resolved.tile,
                                    body = ?resolved.body,
                                    "terminal tile resolved"
                                );
                                if let Some(target) = resolved.target {
                                    tracing::info!(
                                        terminal_id = ?resolved.terminal_id,
                                        target = ?target,
                                        "terminal click target resolved"
                                    );
                                    self.open_click_target(target);
                                    return;
                                }
                                let reason = if resolved.cell.is_some() {
                                    "no_openable_target"
                                } else {
                                    "tile_chrome"
                                };
                                tracing::info!(reason, "terminal right-click missed");
                                if self.focus != PaneFocus::Terminals {
                                    self.set_focus(PaneFocus::Terminals);
                                    self.redraw = true;
                                }
                                self.last_click = None;
                                if let Some((cell_col, cell_row)) = resolved.cell
                                    && self.terminals.terminal_tracks_mouse(resolved.terminal_id)
                                    && let Some((terminal_id, bytes)) =
                                        self.terminals.encode_mouse_for(
                                            resolved.terminal_id,
                                            libghostty_vt::mouse::Action::Press,
                                            Some(libghostty_vt::mouse::Button::Right),
                                            cell_col,
                                            cell_row,
                                        )
                                {
                                    self.send_cmd(IpcCommand::Write {
                                        terminal_id,
                                        bytes,
                                        intent: lazybox_ipc::TerminalInputIntent::View,
                                    });
                                    self.redraw = true;
                                }
                                return;
                            }
                            None => {
                                tracing::info!(
                                    reason = "no_rendered_tile_at_coordinates",
                                    "terminal right-click missed"
                                );
                                if self.focus != PaneFocus::Terminals {
                                    self.set_focus(PaneFocus::Terminals);
                                    self.redraw = true;
                                }
                                self.last_click = None;
                                return;
                            }
                        }
                    }
                }
                // Splitter drag wins over both focus changes and
                // terminal interaction — clicking a splitter resizes,
                // it never refocuses or types into a pane.
                if let Some(target) = self.layout.hit_test_splitter(
                    m.column,
                    m.row,
                    sidebar_rect,
                    right_top_rect,
                    horizontal_splitter,
                ) {
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
                    self.set_focus(focus);
                    self.redraw = true;
                }
                if matches!(button, crossterm::event::MouseButton::Left)
                    && target == Some(PaneFocus::Terminals)
                    && let Some(terminal_id) = self.terminals.tile_at(m.column, m.row)
                {
                    let mut cmds = Vec::new();
                    if self.terminals.focus_tile(terminal_id, &mut cmds) {
                        self.dispatch_cmds(cmds);
                        self.redraw = true;
                    }
                }

                // A click landing in the terminal pane (or nowhere)
                // breaks any pending sidebar / activity double-click
                // sequence, so the next click there starts fresh. This
                // keeps the #182 escape hatch — click into a terminal,
                // then click that workspace's row to step back out to
                // the sidebar — from being read as a sidebar
                // double-click-to-enter gesture (#441).
                if !matches!(target, Some(PaneFocus::Sidebar) | Some(PaneFocus::Right)) {
                    self.last_click = None;
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
                //
                // A click has no lazybox-side meaning once selection
                // declines it, so it belongs to any app that requested
                // mouse tracking. Wheels are different: they always
                // scroll lazybox's terminal history.
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
                        self.send_cmd(IpcCommand::Write {
                            terminal_id,
                            bytes,
                            intent: lazybox_ipc::TerminalInputIntent::View,
                        });
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
                        // The filter chip opens the menu (a modal the
                        // model owns) rather than mutating in place.
                        let filter_hit = self.sidebar.filter_chip_hit(m.column, m.row);
                        if filter_hit {
                            self.mount_filter_menu();
                        }
                        let search_hit = self.sidebar.search_chip_hit(m.column, m.row);
                        if search_hit {
                            self.sidebar.open_global_search();
                        }
                        let handled = filter_hit
                            || search_hit
                            || self.sidebar.click_to_cycle_sort(m.column, m.row)
                            || self.sidebar.click_to_select(sidebar_rect, m.row);
                        if handled {
                            // Double-click on a repo header → toggle
                            // its collapsed state (same effect as
                            // Space); on a workspace row → jump into
                            // its live agent terminal (#441). Cursor
                            // already moved via click_to_select above,
                            // so both operate on the just-clicked row.
                            let is_double = matches!(button, crossterm::event::MouseButton::Left)
                                && self
                                    .last_click
                                    .map(|(c, r, t)| {
                                        c == m.column
                                            && r == m.row
                                            && t.elapsed() <= crate::realm::DOUBLE_CLICK_WINDOW
                                    })
                                    .unwrap_or(false);
                            let double_on_workspace =
                                is_double && !self.sidebar.cursor_on_repo_header();
                            if is_double {
                                self.last_click = None; // consume the pair
                                if self.sidebar.cursor_on_repo_header() {
                                    self.sidebar.toggle_repo_at_cursor();
                                }
                            } else {
                                self.last_click =
                                    Some((m.column, m.row, std::time::Instant::now()));
                            }
                            self.sync_panes();
                            // A double-click drops focus straight into
                            // the workspace's live terminal — "select
                            // and enter the agent" in one gesture
                            // (#441). A single click instead restores
                            // the pane the workspace was last focused
                            // in whenever the selection changes — so
                            // clicking back to a workspace whose agent
                            // you were typing into returns focus to
                            // that terminal instead of stranding it on
                            // the sidebar (#182). A click on the
                            // already-selected row keeps the sidebar
                            // focus the click just set, leaving an
                            // explicit way to step out of the terminal.
                            if double_on_workspace {
                                self.enter_selected_workspace_terminal();
                            } else if self.sidebar.selected_workspace_key() != prev_key.as_ref() {
                                self.restore_workspace_focus();
                            }
                            self.redraw = true;
                        }
                    }
                    // Lazybox-side selection start: any left-click that
                    // landed in the terminal pane. The anchor is pinned
                    // in screen-absolute grid coords so it survives the
                    // viewport auto-scrolling under an edge drag; a
                    // press-release with no cell change (`dragged` never
                    // set) is treated as a plain click in the Up handler.
                    if focus == PaneFocus::Terminals
                        && matches!(button, crossterm::event::MouseButton::Left)
                        && claim_for_selection
                        && let Some(anchor) =
                            self.terminals
                                .selection_point(right_bottom_rect, m.column, m.row)
                    {
                        self.terminal_drag = Some(TerminalDrag {
                            down: (m.column, m.row),
                            anchor,
                            focus: anchor,
                            rect: right_bottom_rect,
                            pointer: (m.column, m.row),
                            dragged: false,
                        });
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
                        if self.right.take_open_description() {
                            self.open_focused_description();
                        }
                        if let Some(url) = self.right.take_open_url() {
                            self.open_external_url(&url);
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
                if self.terminal_drag.is_some() {
                    self.drive_terminal_drag(m.column, m.row);
                }
            }
            MouseEventKind::Up(button) => {
                let was_drag = self.layout.active_drag.take().is_some();
                if was_drag {
                    self.layout.persist();
                }
                let mut click_no_drag_at: Option<(u16, u16)> = None;
                if let Some(drag) = self.terminal_drag.take() {
                    if drag.dragged {
                        let text = self.terminals.extract_selection(drag.anchor, drag.focus);
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
                        click_no_drag_at = Some(drag.down);
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
                            self.send_cmd(IpcCommand::Write {
                                terminal_id,
                                bytes,
                                intent: lazybox_ipc::TerminalInputIntent::View,
                            });
                        }
                    }
                    self.redraw = true;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let raw_up = matches!(m.kind, MouseEventKind::ScrollUp);
                if rect_contains(sidebar_rect, m.column, m.row) {
                    // Viewport-only: the wheel never touches the
                    // selection (selection drives the right pane,
                    // terminal stack, and focus — a trackpad flick
                    // must not yank the user to another workspace,
                    // #290), so no `sync_panes` here. Moving the
                    // offset is a pure in-process mutation, so, like
                    // the terminal-scrollback path below, it moves a
                    // fixed small step per wheel event and stops when
                    // the events stop rather than riding the activity
                    // pane's inertia damper.
                    const SIDEBAR_WHEEL_STEP: isize = 3;
                    let delta = if raw_up {
                        -SIDEBAR_WHEEL_STEP
                    } else {
                        SIDEBAR_WHEEL_STEP
                    };
                    if self.sidebar.scroll_by_wheel(delta) {
                        self.redraw = true;
                    }
                    return;
                }
                // Only the full feed scrolls. The slim summary line
                // shares `right_top_rect`, but routing wheel events to
                // `scroll_activity` there would silently move the
                // not-rendered feed's offset.
                if self.activity_pane_visible() && rect_contains(right_top_rect, m.column, m.row) {
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
                // Scroll the tile UNDER THE CURSOR, not the focused one
                // (#362) — hover-to-scroll like every tiling terminal.
                // Hit-test the wheel coordinates against the tile rects
                // recorded during render; fall back to the focused
                // terminal when the pointer is over pane chrome (a
                // divider, the tab strip) so the wheel is never a no-op.
                let Some(target) = self
                    .terminals
                    .scroll_terminal_at(m.column, m.row)
                    .or_else(|| self.terminals.focused_terminal_id())
                else {
                    return;
                };
                // The wheel always belongs to lazybox's terminal
                // history. Agent screen modes and mouse tracking only
                // affect rendering and clicks; neither can redirect a
                // scroll into an app that may ignore it.
                const LOCAL_WHEEL_STEP: isize = 3;
                let delta = if raw_up {
                    -LOCAL_WHEEL_STEP
                } else {
                    LOCAL_WHEEL_STEP
                };
                let _outcome = self.terminals.scroll_terminal(target, delta);
                // First upward scroll of a visit arms a deep-scrollback
                // fetch — the daemon replies with the backend's retained
                // history (tmux capture-pane) so a live session scrolls
                // as deep as a restarted one (#393).
                if let Some(terminal_id) = self.terminals.take_scrollback_fetch() {
                    self.send_cmd(IpcCommand::FetchScrollback { terminal_id });
                }
                self.redraw = true;
            }
            _ => {}
        }
    }

    /// Advance the active terminal drag-selection to crossterm cell
    /// `(col, row)`: auto-scroll the viewport when the pointer is parked
    /// against the top/bottom edge, then re-derive the screen-absolute
    /// focus so the highlight tracks whatever content is now under the
    /// pointer. Shared by the `Drag` mouse handler and the idle-tick
    /// auto-scroll (`tick_terminal_drag`) so a pointer held still at the
    /// edge keeps scrolling (#432).
    pub(super) fn drive_terminal_drag(&mut self, col: u16, row: u16) {
        let (rect, down) = match &self.terminal_drag {
            Some(drag) => (drag.rect, drag.down),
            None => return,
        };
        let delta = edge_scroll_delta(rect, row);
        if delta != 0 {
            let _ = self.terminals.scroll_active(delta);
            // Reaching for scrollback above the local grid arms the same
            // deep-scrollback fetch the wheel/keyboard scroll paths do, so
            // an edge drag can select as deep as the daemon retained (#393).
            if let Some(terminal_id) = self.terminals.take_scrollback_fetch() {
                self.send_cmd(IpcCommand::FetchScrollback { terminal_id });
            }
        }
        let focus = self.terminals.selection_point(rect, col, row);
        if let Some(drag) = self.terminal_drag.as_mut() {
            drag.pointer = (col, row);
            if (col, row) != down {
                drag.dragged = true;
            }
            if let Some(focus) = focus {
                drag.focus = focus;
            }
        }
        self.redraw = true;
    }

    /// Idle-tick companion to [`Self::drive_terminal_drag`]: while a drag
    /// is held against an edge with no fresh mouse events, keep scrolling
    /// and extending the selection. A no-op when there is no drag or the
    /// pointer is away from an edge, so it costs nothing on a still drag.
    pub(super) fn tick_terminal_drag(&mut self) {
        let Some((rect, pointer)) = self
            .terminal_drag
            .as_ref()
            .map(|drag| (drag.rect, drag.pointer))
        else {
            return;
        };
        if edge_scroll_delta(rect, pointer.1) != 0 {
            self.drive_terminal_drag(pointer.0, pointer.1);
        }
    }
}

/// Per-step viewport scroll for an edge drag-selection, signed by
/// direction: negative (up into scrollback) when the pointer sits in the
/// top chrome or above the pane, positive (down toward live output) when
/// it reaches the bottom border or below, `0` inside the grid. The step
/// grows with how far past the edge the pointer strayed so a big
/// overshoot scrolls faster, capped so it never overshoots wildly.
fn edge_scroll_delta(rect: Rect, row: u16) -> isize {
    const MAX_STEP: u16 = 6;
    if rect.height == 0 {
        return 0;
    }
    // The grid's first visible row sits below the 3-row top chrome; treat
    // that band (and anything above the pane) as the scroll-up zone, and
    // the bottom border row (and below) as the scroll-down zone. Both
    // zones stay clear of real grid cells so an in-grid selection never
    // scrolls on its own.
    let top_zone = rect.y.saturating_add(2);
    let bottom_zone = rect.y.saturating_add(rect.height).saturating_sub(1);
    if row <= top_zone {
        -((top_zone - row).saturating_add(1).min(MAX_STEP) as isize)
    } else if row >= bottom_zone {
        (row - bottom_zone).saturating_add(1).min(MAX_STEP) as isize
    } else {
        0
    }
}

/// Highlight-navigation delta for a which-key popup: `Up`/`k` → −1,
/// `Down`/`j` → +1. Arrows plus the letters; see [`popup_letter_nav_delta`]
/// for the letters-only variant the `]]` leader uses (its arrows are
/// bound to tile movement). Callers only treat `j`/`k` as navigation
/// after ruling out an in-group binding for the same key (#343).
///
/// The arrows require an unmodified press, same as the letters, so a
/// `Shift-arrow` still reaches the splitter-resize arm rather than being
/// swallowed as popup navigation.
fn popup_nav_delta(key: &RealmKey) -> Option<i32> {
    match key.code {
        Key::Up if key.modifiers.is_empty() => Some(-1),
        Key::Down if key.modifiers.is_empty() => Some(1),
        _ => popup_letter_nav_delta(key),
    }
}

/// `j`/`k`-only highlight delta (`k` → −1, `j` → +1). Used where the
/// arrow keys already carry another meaning (the `]]` leader's tile /
/// tab movement, #286) so only the letters can navigate the popup.
fn popup_letter_nav_delta(key: &RealmKey) -> Option<i32> {
    match key.code {
        Key::Char('k') if key.modifiers.is_empty() => Some(-1),
        Key::Char('j') if key.modifiers.is_empty() => Some(1),
        _ => None,
    }
}

/// Move a popup highlight by `delta`, wrapping within `len` rows. From
/// no highlight a downward step lands on the first row and an upward
/// step on the last. `len` must be non-zero.
fn advance_highlight(current: Option<usize>, delta: i32, len: usize) -> usize {
    match current {
        None if delta > 0 => 0,
        None => len - 1,
        Some(i) => (i as i32 + delta).rem_euclid(len as i32) as usize,
    }
}

/// The sole character of a single-char menu key, or `None` for a
/// multi-char display aggregate (e.g. the `←↓↑→` tile-move row) that has
/// no single dispatching key and so can't be `Enter`-fired (#343).
///
/// INVARIANT: this is how `Enter` on a highlighted `]]` row re-derives
/// its command — it feeds the returned char straight back through
/// [`super::terminal_leader::LeaderCmd::from_key`]. That only resolves because
/// every `menu_rows` display key is *literally* its dispatch character
/// (`s`, `|`, `1`, …), never a prettified glyph. The lone exception is
/// the arrow aggregate, which is multi-char and so lands here as `None`.
/// If a future row shows a decorative key, give it a real single
/// dispatch char or it will silently no-op on `Enter`.
fn single_menu_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// Reconstruct a runtime `Action` from a resolved catalog entry,
/// pulling the agent id out of a parameterized `SpawnAgent` row. This
/// is the bridge from the data-model catalog back to the dispatchable
/// `Action` enum — both single-key and leader-sequence resolution go
/// through it.
pub(super) fn action_from_entry(
    entry: &lazybox_tui_core::action::CatalogEntry,
) -> Option<lazybox_tui_core::action::Action> {
    use lazybox_tui_core::action::{Action, ActionKind, Param};
    // A generated row's `Param` disambiguates the two shapes that share
    // an `ActionKind`: an agent-scoped row (`w c` → WorkWith) vs a
    // tier-scoped row (`w S` → WorkTier). SpawnAgent splits the same way.
    match (entry.kind, entry.param.as_ref()) {
        (ActionKind::SpawnAgent, Some(Param::Agent(id))) => {
            return Some(Action::SpawnAgent(id.clone()));
        }
        (ActionKind::SpawnAgent, Some(Param::Tier(alias))) => {
            return Some(Action::SpawnTier(alias.clone()));
        }
        (ActionKind::WorkWith, Some(Param::Agent(id))) => {
            return Some(Action::WorkWith(id.clone()));
        }
        (ActionKind::WorkWith, Some(Param::Tier(alias))) => {
            return Some(Action::WorkTier(alias.clone()));
        }
        (ActionKind::SpawnAgentOnMain, Some(Param::Agent(id))) => {
            return Some(Action::SpawnAgentOnMain(id.clone()));
        }
        _ => {}
    }
    action_from_kind(entry.kind)
}

/// Reconstruct a runtime `Action` from a catalog `ActionKind`, for the
/// no-payload kinds the keyboard path dispatches (single keys AND
/// leader sequences resolve through here so both agree on semantics).
///
/// `SpawnAgent` carries the agent id as a `Param`, handled by
/// [`action_from_entry`]; reached here it has no payload, so `None`.
pub(super) fn action_from_kind(
    kind: lazybox_tui_core::action::ActionKind,
) -> Option<lazybox_tui_core::action::Action> {
    use lazybox_tui_core::action::{Action, ActionKind};
    Some(match kind {
        ActionKind::SpawnShell => Action::SpawnShell,
        ActionKind::SpawnShellOnMain => Action::SpawnShellOnMain,
        ActionKind::MarkAllRead => Action::MarkAllRead,
        ActionKind::Work => Action::Work,
        ActionKind::OpenEditor => Action::OpenEditor,
        ActionKind::ViewDiff => Action::ViewDiff,
        ActionKind::NewWorkspace => Action::NewWorkspace,
        ActionKind::RenameWorkspace => Action::RenameWorkspace,
        ActionKind::NewProject => Action::NewProject,
        ActionKind::ImportCheckout => Action::ImportCheckout,
        ActionKind::AddScanRoot => Action::AddScanRoot,
        ActionKind::MergePr => Action::MergePr,
        ActionKind::UpdateBranch => Action::UpdateBranch,
        ActionKind::ToggleAutoMerge => Action::ToggleAutoMerge,
        ActionKind::ToggleAutoFix => Action::ToggleAutoFix,
        ActionKind::ToggleTrackMain => Action::ToggleTrackMain,
        ActionKind::ManagePolicies => Action::ManagePolicies,
        ActionKind::Archive => Action::Archive,
        ActionKind::CloseIssue => Action::CloseIssue,
        ActionKind::ToggleSnooze => Action::ToggleSnooze,
        ActionKind::LongSnooze => Action::LongSnooze,
        ActionKind::Refresh => Action::Refresh,
        ActionKind::ForceRedraw => Action::ForceRedraw,
        ActionKind::AdoptSessions => Action::AdoptSessions,
        ActionKind::SendToSession => Action::SendToSession,
        ActionKind::ConvertSession => Action::ConvertSession,
        ActionKind::CollapseIntoPr => Action::CollapseIntoPr,
        ActionKind::Reply => Action::Reply,
        ActionKind::EditNotes => Action::EditNotes,
        ActionKind::RequestReviewers => Action::RequestReviewers,
        ActionKind::AddAssignees => Action::AddAssignees,
        ActionKind::ManageLabels => Action::ManageLabels,
        ActionKind::SyncWorkspace => Action::SyncWorkspace,
        ActionKind::OpenInBrowser => Action::OpenInBrowser,
        ActionKind::DeleteOrClose => Action::DeleteOrClose,
        // Activity-pane cursor jumps (`g` / `Shift-G` under Right
        // focus). Dispatching these through the catalog is what keeps
        // a reflexive `g` in the activity pane from arming the
        // Workspace `g *` github leader (where `g g` toggled
        // auto-merge on the PR).
        ActionKind::ActivityTop => Action::ActivityTop,
        ActionKind::ActivityBottom => Action::ActivityBottom,
        // Pane cycling + mouse capture dispatch through their
        // chord-reading guard arms in `handle_pane_key` first; these
        // mappings serve the non-keyboard surfaces (context menu,
        // invariant tests) and keep the kinds out of the silent
        // fallback set.
        ActionKind::CyclePane => Action::CyclePane,
        // Directional pane focus (`Right`/`l` from the sidebar,
        // `Left`/`h` from the activity pane). Dispatched through the
        // catalog like every other focus move so they show in `?` help
        // and are remappable.
        ActionKind::FocusPaneRight => Action::FocusPaneRight,
        ActionKind::FocusPaneLeft => Action::FocusPaneLeft,
        ActionKind::ToggleMouseCapture => Action::ToggleMouseCapture,
        ActionKind::OpenFilterMenu => Action::OpenFilterMenu,
        ActionKind::CycleSort => Action::CycleSort,
        ActionKind::CycleMailbox => Action::CycleMailbox,
        ActionKind::OpenSearch => Action::OpenSearch,
        ActionKind::OpenGlobalSearch => Action::OpenGlobalSearch,
        ActionKind::ToggleRepoGroup => Action::ToggleRepoGroup,
        ActionKind::ToggleRepoPin => Action::ToggleRepoPin,
        ActionKind::SelectWorkspace => Action::SelectWorkspace,
        ActionKind::BroadcastToSelected => Action::BroadcastToSelected,
        ActionKind::UpdateBranchSelected => Action::UpdateBranchSelected,
        ActionKind::OpenHelp => Action::OpenHelp,
        ActionKind::OpenTour => Action::OpenTour,
        ActionKind::OpenSyncStatus => Action::OpenSyncStatus,
        ActionKind::OpenMessages => Action::OpenMessages,
        // DismissNotice is deliberately absent: it's routed through the
        // explicit Esc branch in `handle_pane_key` (which yields to a
        // sidebar multi-select and to a live terminal), not the generic
        // catalog dispatch, so it can never shadow a pane's own Esc.
        ActionKind::OpenSettings => Action::OpenSettings,
        ActionKind::OpenThemePicker => Action::OpenThemePicker,
        ActionKind::OpenSnippets => Action::OpenSnippets,
        ActionKind::JumpToWorkspace => Action::JumpToWorkspace,
        ActionKind::JumpToAsking => Action::JumpToAsking,
        ActionKind::JumpToFailingCi => Action::JumpToFailingCi,
        ActionKind::StartAgent => Action::StartAgent,
        ActionKind::ToggleActivityPane => Action::ToggleActivityPane,
        ActionKind::ToggleFocusMode => Action::ToggleFocusMode,
        _ => return None,
    })
}

/// The reviewed allowlist of catalog kinds that deliberately do NOT
/// dispatch through `dispatch_action` — each is consumed by a named
/// pane match arm or an explicit `handle_pane_key` branch instead.
/// Every other kind in the runtime catalog MUST resolve through
/// [`action_from_entry`]; the `dispatch_coverage` invariant test fails
/// the build when a new catalog row lands in neither camp (it would
/// render in `?` help and the footer while silently no-oping on the
/// keyboard).
///
/// Second tuple field: where the kind is actually handled. Third:
/// whether a `ui.action_keys` override on it takes effect — `false`
/// entries are hardcoded pane arms, and the startup keymap validation
/// warns when a user tries to remap one.
pub(super) const PANE_NATIVE_KINDS: &[(lazybox_tui_core::action::ActionKind, &str, bool)] = {
    use lazybox_tui_core::action::ActionKind as K;
    &[
        (
            K::OpenWorkspace,
            "handle_pane_key's sidebar Enter arm (keys.rs)",
            false,
        ),
        (
            K::ToggleActivity,
            "RightPane::handle_key's Enter arm (components/right_pane/mod.rs)",
            false,
        ),
        (
            K::ToggleRow,
            "RightPane::handle_key's Right/Left/l/h arms (components/right_pane/mod.rs)",
            false,
        ),
        (
            K::SelectRow,
            "RightPane::handle_key's Space/v arm (components/right_pane/mod.rs)",
            false,
        ),
        (
            K::ToggleDescription,
            "RightPane::handle_key's 'd' arm (components/right_pane/mod.rs)",
            false,
        ),
        (
            K::UndoMarkRead,
            "RightPane::handle_key's 'z' arm (components/right_pane/mod.rs)",
            false,
        ),
        (
            K::Quit,
            "handle_pane_key's quit-chord latch branches (keys.rs) — override honored",
            true,
        ),
        (
            K::DismissNotice,
            "handle_pane_key's dismiss-notice branch (keys.rs) — override honored",
            true,
        ),
        (
            K::InspectNotice,
            "handle_pane_key's inspect-notice branch (keys.rs) — override honored",
            true,
        ),
        (
            K::ResizeSplitter,
            "handle_pane_key's Shift-arrow arm (keys.rs)",
            false,
        ),
        (
            K::TerminalScroll,
            "TerminalStack::handle_key's Shift-PgUp/PgDn/Home/End arms (components/terminal_stack.rs)",
            false,
        ),
        (
            K::LeaveTerminal,
            "the terminal `]]` escape latch (keys.rs; chord comes from terminal.escape_char)",
            false,
        ),
    ]
};

/// Whether a `ui.action_keys` override targeting `config_key`'s
/// catalog row can actually change what fires. Catalog-dispatched rows
/// and the chord-reading explicit branches (quit, dismiss-notice,
/// cycle-pane, mouse-capture) honor overrides; the hardcoded
/// pane-native arms in [`PANE_NATIVE_KINDS`] don't.
pub(super) fn override_takes_effect(entry: &lazybox_tui_core::action::CatalogEntry) -> bool {
    if action_from_entry(entry).is_some() {
        return true;
    }
    PANE_NATIVE_KINDS
        .iter()
        .find(|(k, _, _)| *k == entry.kind)
        .map(|(_, _, effective)| *effective)
        .unwrap_or(false)
}

#[cfg(test)]
mod edge_scroll_tests {
    use super::edge_scroll_delta;
    use tuirealm::ratatui::layout::Rect;

    #[test]
    fn only_the_edges_scroll_and_direction_follows_the_edge() {
        // Pane rows 5..=24 (y=5, height=20).
        let rect = Rect::new(0, 5, 80, 20);
        // Inside the grid → no auto-scroll.
        assert_eq!(edge_scroll_delta(rect, 12), 0);
        // Top chrome band / above the pane → scroll up (negative).
        assert!(edge_scroll_delta(rect, 6) < 0, "top band scrolls up");
        assert!(edge_scroll_delta(rect, 0) < 0, "above the pane scrolls up");
        // Bottom border / below the pane → scroll down (positive).
        assert!(edge_scroll_delta(rect, 24) > 0, "bottom row scrolls down");
        assert!(
            edge_scroll_delta(rect, 60) > 0,
            "below the pane scrolls down"
        );
    }

    #[test]
    fn step_grows_with_overshoot_but_is_capped() {
        let rect = Rect::new(0, 5, 80, 20);
        // Just past the bottom edge is the smallest step; a large
        // overshoot accelerates but never exceeds the cap.
        let near = edge_scroll_delta(rect, 24);
        let far = edge_scroll_delta(rect, 60);
        assert!(far > near, "a bigger overshoot scrolls faster");
        assert!(far <= 6, "the step is capped");
    }
}

#[cfg(test)]
mod popup_nav_tests {
    use super::*;

    fn key(code: Key, mods: KeyModifiers) -> RealmKey {
        RealmKey::new(code, mods)
    }

    /// From no highlight, the first step lands on an edge; further steps
    /// wrap around within `len` in both directions (#343).
    #[test]
    fn advance_highlight_wraps_and_seeds_from_empty() {
        // Seed: down → first row, up → last row.
        assert_eq!(advance_highlight(None, 1, 4), 0);
        assert_eq!(advance_highlight(None, -1, 4), 3);
        // Wrap: past the end returns to the top, before the top to the end.
        assert_eq!(advance_highlight(Some(3), 1, 4), 0);
        assert_eq!(advance_highlight(Some(0), -1, 4), 3);
        // Interior steps are plain increments.
        assert_eq!(advance_highlight(Some(1), 1, 4), 2);
        assert_eq!(advance_highlight(Some(2), -1, 4), 1);
        // A stale index (list shrank under it) is normalised into range,
        // never panics.
        assert_eq!(advance_highlight(Some(9), 1, 4), 2);
    }

    /// The popup-nav letters (`j`/`k`) must never also name a `]]` leader
    /// command. The armed leader routes them to highlight navigation
    /// BEFORE command dispatch (`handle_pane_key`), so any command bound to
    /// `j`/`k` would be unreachable via its direct chord. Binding Skills to
    /// `k` (#797) tripped exactly this — guard the invariant the nav path
    /// relies on so a future letter command can't silently be shadowed.
    #[test]
    fn popup_nav_letters_never_name_a_leader_command() {
        use crate::realm::model::terminal_leader::LeaderCmd;
        for c in ['j', 'k'] {
            assert!(
                popup_letter_nav_delta(&key(Key::Char(c), KeyModifiers::NONE)).is_some(),
                "`{c}` is expected to navigate the popup",
            );
            assert!(
                LeaderCmd::from_key(Key::Char(c), KeyModifiers::empty()).is_none(),
                "`{c}` navigates the popup, so it must not bind a leader command \
                 (it would be shadowed and unreachable via its direct chord)",
            );
        }
    }

    /// Arrows and `j`/`k` navigate only unmodified; a `Shift-arrow` is
    /// left for the splitter-resize arm (#343).
    #[test]
    fn nav_deltas_require_an_unmodified_press() {
        assert_eq!(popup_nav_delta(&key(Key::Up, KeyModifiers::NONE)), Some(-1));
        assert_eq!(
            popup_nav_delta(&key(Key::Down, KeyModifiers::NONE)),
            Some(1)
        );
        assert_eq!(
            popup_nav_delta(&key(Key::Char('k'), KeyModifiers::NONE)),
            Some(-1)
        );
        assert_eq!(
            popup_nav_delta(&key(Key::Char('j'), KeyModifiers::NONE)),
            Some(1)
        );
        assert_eq!(popup_nav_delta(&key(Key::Down, KeyModifiers::SHIFT)), None);
        assert_eq!(
            popup_nav_delta(&key(Key::Char('j'), KeyModifiers::CONTROL)),
            None
        );
        // The `]]` variant never claims the arrows (they move tiles, #286).
        assert_eq!(
            popup_letter_nav_delta(&key(Key::Down, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            popup_letter_nav_delta(&key(Key::Char('j'), KeyModifiers::NONE)),
            Some(1)
        );
    }

    /// A single-char key yields its dispatch char; the multi-char arrow
    /// aggregate has none, so it's skipped for `Enter` (#343).
    #[test]
    fn single_menu_char_isolates_dispatchable_rows() {
        assert_eq!(single_menu_char("s"), Some('s'));
        assert_eq!(single_menu_char("|"), Some('|'));
        assert_eq!(single_menu_char("1"), Some('1'));
        assert_eq!(single_menu_char("←↓↑→"), None);
        assert_eq!(single_menu_char("←→"), None);
        assert_eq!(single_menu_char(""), None);
    }
}
