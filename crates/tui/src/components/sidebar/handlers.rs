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
            // Collapse / expand the cursor's repo group (`Space`, mimics
            // file-tree TUIs — yazi, nnn, lf — folding a directory) is a
            // catalog action now (`ActionKind::ToggleRepoGroup`, #338),
            // dispatched through `Model::dispatch_action` before this
            // handler runs — so it shows in `?` help, is remappable via
            // `ui.action_keys`, and surfaces in the footer hints.

            // Esc drops the broadcast multi-select set (the marks `v`
            // toggled). With no selection it bubbles up so Esc keeps
            // whatever meaning the outer layers give it.
            (KeyCode::Esc, KeyModifiers::NONE) => {
                if self.clear_broadcast_selection() {
                    PaneOutcome::Consumed
                } else {
                    PaneOutcome::Pass
                }
            }

            // ── Spawn / open ──────────────────────────────────────────
            // The per-agent spawn chords (`a c` claude, `a x` codex,
            // `a u` cursor) are catalog rows now (#102 P2, #304) —
            // generated per enabled agent in `ActionDef::catalog`,
            // dispatched through `Model::dispatch_action(SpawnAgent(id))`
            // before this handler ever runs. The old `agent_shortcuts`
            // side map is gone; remap an agent's chord via
            // `ui.action_keys` (`spawn_agent.<id>`).
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
                        model_alias: None,
                        session_key: workspace_key,
                        session_id: self.selected_session_id(),
                        kind: TerminalKind::Agent(agent_id),
                        cwd: None,
                        initial_prompt: prompt,
                        on_main: false,
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
                            model_alias: None,
                            session_key: workspace_key,
                            session_id: self.selected_session_id(),
                            kind: TerminalKind::Shell,
                            cwd: None,
                            initial_prompt: None,
                            on_main: false,
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
            // `z` toggle-snooze and `x z` long-snooze are catalog
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
                self.agents.clear();
                self.agent_terminal_states.clear();
                self.running_terminals.clear();
                for t in terminals {
                    self.running_terminals
                        .insert(t.terminal_id, (t.session_key.clone(), t.kind.clone()));
                    if let Some(state) = t.agent_state {
                        self.agent_terminal_states
                            .insert(t.terminal_id, (t.session_key.clone(), state));
                    }
                }
                self.rebuild_agent_aggregates();
                self.broadcast_selected
                    .retain(|k| self.workspaces.contains_key(k));
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
                let session_key = self
                    .running_terminals
                    .remove(terminal_id)
                    .map(|(session_key, _)| session_key)
                    .or_else(|| {
                        self.agent_terminal_states
                            .get(terminal_id)
                            .map(|(session_key, _)| session_key.clone())
                    });
                if !matches!(
                    self.agent_terminal_states.get(terminal_id),
                    Some((_, lazybox_ipc::AgentState::Exited { .. }))
                ) {
                    self.agent_terminal_states.remove(terminal_id);
                }
                if let Some(session_key) = session_key
                    && self.refresh_agent_aggregate(&session_key).asking_changed
                {
                    self.recompute_visible();
                }
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
                    let before = workspace_attention_signals(old, &self.agents);
                    let after = workspace_attention_signals(workspace, &self.agents);
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
                self.broadcast_selected.remove(&session_key);
                self.agents.remove(&session_key);
                self.agent_terminal_states
                    .retain(|_, (key, _)| key != &session_key);
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
                session_key,
                terminal_id,
                state,
            } => {
                tracing::info!(
                    %session_key,
                    state = ?state,
                    "sidebar: received Event::AgentState",
                );
                // The daemon-side detector flipped an agent into a new
                // `AgentState`. Fold it into the sidebar-local `agents`
                // map — the client projection for this terminal signal.
                //
                // Why a sidebar-local map instead of mutating
                // `workspace.sessions[i].state`: the next poll
                // cycle's `WorkspaceUpserted` rebuilds the workspace
                // from the persisted workspace record, which intentionally
                // does not carry per-terminal state. That state arrives in
                // terminal snapshots and live `Event::AgentState` updates.
                // Mutating the workspace here would be silently undone
                // within 60s.
                //
                // Recompute the workspace projection from every terminal,
                // then report aggregate rising edges into
                // `InputNeeded` / `Done` so the outer wrapper can enqueue
                // a desktop notification (drained + fired there so
                // library tests never trigger a real
                // `osascript` / `notify-send`).
                self.agent_terminal_states
                    .insert(*terminal_id, (session_key.clone(), *state));
                let change = self.refresh_agent_aggregate(session_key);
                if change.now_asking {
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
                        // severity = 3s fade, dim color. Slugged name:
                        // a raw issue title would displace the rest of
                        // the message (#291).
                        self.pending_asking_notices.push(format!(
                            "{} needs input — press ! to jump",
                            crate::util::notice_slug(&workspace.name)
                        ));
                    }
                }
                // Rising edge into Done — the agent finished its turn
                // (#80). Alert with the same banner + footer-notice path
                // as asking, so a completed run is noticed even when
                // lazybox isn't the focused window.
                if change.now_done
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
                    self.pending_asking_notices.push(format!(
                        "{} finished",
                        crate::util::notice_slug(&workspace.name)
                    ));
                }
                // Only a change in asking-ness can change the visible set
                // (it feeds the per-repo attention counter); a done- or
                // working-only change reads fresh at render time, and the
                // daemon-event path forces the redraw via
                // `displays_agent_state`.
                if change.asking_changed {
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
                for (session_key, _) in self.agent_terminal_states.values_mut() {
                    if session_key == from {
                        *session_key = to.clone();
                    }
                }
                self.rebuild_agent_aggregates();
                self.recompute_visible();
            }
            _ => {}
        }
    }

    fn refresh_agent_aggregate(
        &mut self,
        session_key: &SessionKey,
    ) -> crate::agent_attention::StateChange {
        let previous = self.agents.get(session_key).copied();
        let incoming = aggregate_agent_state(
            self.agent_terminal_states
                .values()
                .filter_map(|(key, state)| (key == session_key).then_some(*state)),
        );
        match incoming {
            Some(state) => {
                self.agents.insert(session_key.clone(), state);
            }
            None => {
                self.agents.remove(session_key);
            }
        }
        crate::agent_attention::state_change(previous, incoming)
    }

    fn rebuild_agent_aggregates(&mut self) {
        self.agents.clear();
        let session_keys: std::collections::HashSet<_> = self
            .agent_terminal_states
            .values()
            .map(|(session_key, _)| session_key.clone())
            .collect();
        for session_key in session_keys {
            let _ = self.refresh_agent_aggregate(&session_key);
        }
    }
}

fn aggregate_agent_state(
    states: impl Iterator<Item = lazybox_ipc::AgentState>,
) -> Option<lazybox_ipc::AgentState> {
    states.max_by_key(|state| match state {
        lazybox_ipc::AgentState::InputNeeded => 5,
        lazybox_ipc::AgentState::Working => 4,
        lazybox_ipc::AgentState::Done => 3,
        lazybox_ipc::AgentState::Exited { .. } => 2,
        lazybox_ipc::AgentState::Idle => 1,
    })
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
