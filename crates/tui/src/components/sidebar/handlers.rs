//! Keyboard + daemon-event handlers for the sidebar.
//!
//! - `handle_key` is the big match arm covering every key the
//!   sidebar accepts: j/k navigation, mailbox cycling, snooze,
//!   archive, mark-read, spawn shortcuts, etc.
//! - `on_event` mirrors daemon events into the sidebar's local
//!   state (workspace map, terminal map, asking-states, viewer
//!   logins).
//!
//! Pulled out of `mod.rs` because each one is 100-250 lines and
//! their concerns are orthogonal to construction / accessors /
//! recompute logic that stays in the parent module.

use super::*;

impl Sidebar {
    pub fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        match (key.code, key.modifiers) {
            // ── Navigation ────────────────────────────────────────────
            // `FocusWorkspace` emission is centralized in
            // `Model::sync_panes` (called after every key dispatch);
            // local handlers only mutate the cursor.
            // `j`/`k` are the vim synonyms; both letters are free in
            // the catalog and the right pane binds them the same way.
            (KeyCode::Down, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.move_cursor_by(1);
                PaneOutcome::Consumed
            }
            (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.move_cursor_by(1);
                PaneOutcome::Consumed
            }
            (KeyCode::Up, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.move_cursor_by(-1);
                PaneOutcome::Consumed
            }
            (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.move_cursor_by(-1);
                PaneOutcome::Consumed
            }
            // ── Collapse / expand the cursor's repo group ─────────────
            // Space toggles the parent repo of whatever workspace /
            // session row the cursor is on. Mimics file-tree TUIs
            // (yazi, nnn, lf) where Space folds a directory.
            (KeyCode::Char(' '), KeyModifiers::NONE) => {
                self.toggle_repo_at_cursor();
                PaneOutcome::Consumed
            }

            // ── Spawn / open ──────────────────────────────────────────
            // The per-agent spawn keys (`c` claude, `x` codex, `u`
            // cursor) are catalog rows now (#102 P2) — generated per
            // enabled agent in `ActionDef::catalog`, dispatched through
            // `Model::dispatch_action(SpawnAgent(id))` before this
            // handler ever runs. The old `agent_shortcuts` side map is
            // gone; remap an agent's key via `ui.action_keys`
            // (`spawn_agent.<id>`).
            //
            // `w` for "work on this" — single polymorphic key. Spawns
            // the default agent with a context-aware prompt:
            //  - on an issue row → implement the issue
            //  - on a PR row with CI failing → fix the failing checks
            // Match guard hides the key in the hint bar when neither
            // case applies, so users see `w` only where it'll fire.
            // (We removed the old `f` mnemonic — `w` covers both
            // cases, plus the right-pane `w` for selected comments,
            // so the user has one work key everywhere.)
            (KeyCode::Char('w'), KeyModifiers::NONE)
                if matches!(
                    crate::intent::resolve_work(
                        self.selected_workspace(),
                        &[],
                        &self.default_agent,
                    ),
                    crate::intent::Intent::SpawnAgent { .. }
                ) =>
            {
                // Sidebar `w` never has selected-comments (the activity
                // pane owns that selection state), so pass an empty
                // slice — the resolver does the priority chain.
                let intent = crate::intent::resolve_work(
                    self.selected_workspace(),
                    &[],
                    &self.default_agent,
                );
                if let crate::intent::Intent::SpawnAgent {
                    workspace_key,
                    agent_id,
                    prompt,
                } = intent
                {
                    tracing::info!(%workspace_key, %agent_id, "sidebar: emitting Spawn(Agent) with work prompt");
                    cmds.push(Command::Spawn {
                        session_key: workspace_key,
                        session_id: self.selected_session_id(),
                        kind: TerminalKind::Agent(agent_id),
                        cwd: None,
                        initial_prompt: prompt,
                    });
                }
                PaneOutcome::Consumed
            }

            // `s` for shell — used to be `b` (for "bash") but the
            // hint bar reads better as "S shell / C claude / X codex /
            // U cursor" all-lowercase, and `s` is mnemonic.
            //
            // Decision lives in `intent::resolve_spawn_shell` — same
            // (workspace_state, key) → Intent shape as every other
            // spawn key.
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                match crate::intent::resolve_spawn_shell(self.selected_workspace()) {
                    crate::intent::Intent::SpawnShell { workspace_key } => {
                        tracing::info!(%workspace_key, "sidebar: emitting Spawn(Shell)");
                        cmds.push(Command::Spawn {
                            session_key: workspace_key,
                            session_id: self.selected_session_id(),
                            kind: TerminalKind::Shell,
                            cwd: None,
                            initial_prompt: None,
                        });
                    }
                    _ => {
                        tracing::warn!(
                            "sidebar: shell shortcut pressed but resolver returned NoOp \
                             (no workspace selected)"
                        );
                    }
                }
                PaneOutcome::Consumed
            }

            // ── Session actions ───────────────────────────────────────
            (KeyCode::Char('m'), KeyModifiers::NONE) => {
                if let crate::intent::Intent::MarkAllRead { session_key } =
                    crate::intent::resolve_mark_read(self.selected_workspace())
                {
                    cmds.push(Command::MarkRead { session_key });
                }
                PaneOutcome::Consumed
            }
            // `g` used to be sidebar-local refresh. Removed: it
            // duplicated `Shift+R` (global, discoverable from `?`
            // help) and collided with the vim `g`/`G` "go to
            // top/bottom" convention the right pane already uses.
            // One refresh binding, accessible from every pane.
            // `z` toggle-snooze and `Shift-Z` long-snooze are catalog
            // actions now (#102): `Model::dispatch_action(ToggleSnooze)`
            // and the `Confirm`-guarded `LongSnooze` row, which mounts
            // the unified Confirm modal instead of the old two-press
            // latch. That deleted the sidebar's `LatchSet`.

            // `f` role-filter, `o` sort, `/` search, and `Shift-S`
            // mailbox cycle moved into the action catalog
            // (Section::Sidebar, issue #98). They dispatch through
            // `Model::dispatch_action` like every other catalog key,
            // so they're now remappable via `ui.action_keys`, show in
            // the `?` help panel, and live in one collision-audited
            // place. The cursor-reset / recompute behaviour is
            // unchanged — it lives in the `Sidebar` methods these
            // actions call.

            // Shift+X Archive is now handled by the catalog
            // dispatch path in `Model::dispatch_action` — it calls
            // `Sidebar::arm_or_fire_archive` which drives the same
            // two-press latch this match arm used to. First press
            // arms (sidebar chrome shows "press again to confirm");
            // second within the latch window fires `Kill`.

            // MergePr (the `g m` leader) is now handled by the
            // catalog dispatch path in `Model::dispatch_action` — it does
            // the same `resolve_merge` precondition check, then
            // mounts the Confirm modal. The match arm here used to
            // queue a `pending_merge_requests` entry that the
            // orchestrator drained post-dispatch; that queue is
            // gone now (one fewer indirection).

            // Anything else: bubble up. Tab / Help / `?` / overlays /
            // quit are handled by parent components.
            _ => PaneOutcome::Pass,
        }
    }

    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::Snapshot {
                workspaces,
                terminals,
                ..
            } => {
                self.workspaces.clear();
                for w in workspaces {
                    let key: SessionKey = (&w.key).into();
                    self.workspaces.insert(key, w.clone());
                }
                self.running_terminals.clear();
                for t in terminals {
                    self.running_terminals
                        .insert(t.terminal_id, (t.session_key.clone(), t.kind.clone()));
                }
                self.reset_cursor_and_recompute();
            }
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind,
                ..
            } => {
                self.running_terminals
                    .insert(*terminal_id, (session_key.clone(), kind.clone()));
            }
            Event::TerminalExited { terminal_id, .. } => {
                self.running_terminals.remove(terminal_id);
            }
            Event::WorkspaceUpserted(workspace) => {
                let key: SessionKey = (&workspace.key).into();
                // Rising-edge desktop notifications. When a workspace
                // we already track gains an attention signal it
                // didn't have last poll — CI started failing, a
                // review got requested, a new comment landed — queue
                // a banner, gated per-signal by the same `attention`
                // config that drives the in-app badge plus the
                // `desktop_notify` master switch. First sight of a
                // workspace (not yet in the map) seeds the baseline
                // silently, so a fresh snapshot doesn't fire a burst
                // of banners on startup. Drained + fired by the
                // IO-aware wrapper, never by the inner sidebar (tests
                // must stay subprocess-free). `AgentAsking` is
                // excluded here — it's delivered via `Event::AgentState`.
                if self.attention.desktop_notify
                    && let Some(old) = self.workspaces.get(&key)
                {
                    let before = workspace_attention_signals(old, &self.agents_asking);
                    let after = workspace_attention_signals(workspace, &self.agents_asking);
                    for signal in after {
                        if !before.contains(&signal)
                            && attention_gate(signal, &self.attention)
                            && let Some(notif) = attention_notification(signal, workspace)
                        {
                            self.pending_notifications.push(notif);
                        }
                    }
                }
                self.workspaces.insert(key, (**workspace).clone());
                self.recompute_visible();
            }
            Event::WorkspaceRemoved(key) => {
                let session_key: SessionKey = key.into();
                self.workspaces.remove(&session_key);
                self.recompute_visible();
            }
            Event::SessionCreated(session) => {
                let key: SessionKey = (&session.workspace_key).into();
                if let Some(w) = self.workspaces.get_mut(&key) {
                    // Idempotent — the canonical add_session will
                    // refuse to duplicate if the daemon resends.
                    if w.find_session(session.id).is_none() {
                        w.sessions.push((**session).clone());
                    }
                    self.recompute_visible();
                }
            }
            Event::SessionEnded {
                workspace_key,
                session_id,
            } => {
                let key: SessionKey = workspace_key.into();
                if let Some(w) = self.workspaces.get_mut(&key) {
                    w.remove_session(*session_id);
                    self.recompute_visible();
                }
            }
            Event::AgentState {
                session_key, state, ..
            } => {
                tracing::info!(
                    %session_key,
                    state = ?state,
                    "sidebar: received Event::AgentState",
                );
                // The daemon-side detector flipped an agent into
                // `Asking` (yes/no prompt) or back to `Active`.
                // Update the sidebar-local `agents_asking` set —
                // the canonical store for this transient signal.
                //
                // Why a sidebar-local set instead of mutating
                // `workspace.sessions[i].state`: the next poll
                // cycle's `WorkspaceUpserted` rebuilds the workspace
                // from the persisted store, which doesn't (and
                // shouldn't) carry transient agent state. Mutating
                // it here would be silently undone within 60s. The
                // set survives poll broadcasts because nothing
                // touches it except `Event::AgentState`.
                //
                // On the Active → Asking edge, enqueue a desktop
                // notification (drained + fired by the outer
                // wrapper so library tests never trigger a real
                // `osascript` / `notify-send`).
                let transition = crate::agent_attention::apply_agent_state(
                    &mut self.agents_asking,
                    session_key,
                    *state,
                );
                // The "working" slot shares the same UI cell as the
                // `?` pill and is driven by the same event. Keep its
                // disjoint set in sync. No `recompute_visible()` is
                // needed for a working-only change: `compute_visible`
                // reads `agents_asking` but not `agents_working`, so
                // the row list is identical; the spinner glyph is read
                // fresh from `agents_working` at render time, and the
                // daemon-event path already forces a redraw.
                crate::agent_attention::apply_working_state(
                    &mut self.agents_working,
                    session_key,
                    *state,
                );
                // The `Done` slot shares the same UI cell. Keep its
                // disjoint set in sync; the rising edge (membership
                // changed *into* Done) alerts the user just like asking.
                let done_changed = crate::agent_attention::apply_done_state(
                    &mut self.agents_done,
                    session_key,
                    *state,
                );
                if matches!(
                    transition,
                    crate::agent_attention::AttentionTransition::NowAsking
                ) {
                    if let Some(workspace) = self.workspaces.get(session_key) {
                        // OS-level banner, gated by the config toggle.
                        // The footer notice below always fires — it's
                        // in-app and free of the banner's noise.
                        if self.attention.desktop_notify {
                            let title = format!("lazybox — {} needs input", workspace.name);
                            let body = workspace
                                .primary_task()
                                .map(|t| t.title.clone())
                                .unwrap_or_else(|| workspace.name.clone());
                            self.pending_notifications
                                .push(PendingNotification { title, body });
                        }
                        // Inline footer notice in addition to the OS
                        // popup — covers users with notifications muted
                        // (which is most of them while focused). Hint
                        // severity = 3s fade, dim color.
                        self.pending_asking_notices
                            .push(format!("{} needs input — press ! to jump", workspace.name));
                    }
                }
                // Rising edge into Done — the agent finished its turn
                // (#80). Alert with the same banner + footer-notice path
                // as asking, so a completed run is noticed even when
                // lazybox isn't the focused window.
                if done_changed
                    && matches!(*state, lazybox_ipc::AgentState::Done)
                    && let Some(workspace) = self.workspaces.get(session_key)
                {
                    if self.attention.desktop_notify {
                        let title = format!("lazybox — {} finished", workspace.name);
                        let body = workspace
                            .primary_task()
                            .map(|t| t.title.clone())
                            .unwrap_or_else(|| workspace.name.clone());
                        self.pending_notifications
                            .push(PendingNotification { title, body });
                    }
                    self.pending_asking_notices
                        .push(format!("{} finished", workspace.name));
                }
                // Only the asking transition can change the visible set
                // (it feeds the per-repo attention counter); a done- or
                // working-only change reads fresh at render time, and the
                // daemon-event path forces the redraw via
                // `displays_agent_state`.
                if !matches!(
                    transition,
                    crate::agent_attention::AttentionTransition::NoChange
                ) {
                    self.recompute_visible();
                }
            }
            Event::TerminalsRebadged { from, to } => {
                // The daemon moved every terminal owned by `from` onto
                // `to` (issue→PR collapse, manual adopt). The transient
                // attention sets are keyed by session, so migrate them
                // the same way `terminal_stack` re-points its slots.
                // Crucial for an agent parked on a prompt: the daemon
                // re-broadcasts `AgentState` only on the next output
                // chunk, which a stalled `InputNeeded` agent never
                // produces — so without this its `?` pill stays pinned
                // to the deleted issue key and the PR row shows no
                // badge, reading as a lost session (#205).
                // Re-point every live terminal owned by `from` onto `to`.
                // The runner badge (`N C`), the agent-reuse lookups, and
                // the `]]<digit>` jump all derive from `running_terminals`
                // keyed by session — without this the moved agent's badge
                // stays pinned to the deleted issue key and never appears
                // on the PR row until an unrelated event forces a rebuild
                // (#241).
                for (sk, _) in self.running_terminals.values_mut() {
                    if sk == from {
                        *sk = to.clone();
                    }
                }
                let asking =
                    crate::agent_attention::rebadge_attention(&mut self.agents_asking, from, to);
                crate::agent_attention::rebadge_attention(&mut self.agents_working, from, to);
                crate::agent_attention::rebadge_attention(&mut self.agents_done, from, to);
                if asking {
                    // Only the asking-set feeds the visible row list
                    // (per-repo attention counter); the others read
                    // fresh at render time.
                    self.recompute_visible();
                }
            }
            _ => {}
        }
    }
}

/// Build the desktop notification for a newly-risen attention signal,
/// or `None` for signals delivered through another path. The title
/// names the signal + workspace; the body is the underlying task's
/// title (falling back to the workspace name).
fn attention_notification(signal: AttentionSignal, w: &Workspace) -> Option<PendingNotification> {
    let title = match signal {
        AttentionSignal::CiFailing => format!("lazybox — CI failing on {}", w.name),
        AttentionSignal::ReviewPending => format!("lazybox — review requested on {}", w.name),
        AttentionSignal::Unread => format!("lazybox — new activity on {}", w.name),
        AttentionSignal::Mentioned => format!("lazybox — you were mentioned in {}", w.name),
        // Delivered via `Event::AgentState`, not workspace upserts.
        AttentionSignal::AgentAsking => return None,
    };
    let body = w
        .primary_task()
        .map(|t| t.title.clone())
        .unwrap_or_else(|| w.name.clone());
    Some(PendingNotification { title, body })
}
