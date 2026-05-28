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
        // Each two-press latch disarms when its trigger key isn't
        // the next press. Single source of truth for the "first
        // press arms, second press fires, anything else disarms"
        // contract is owned by `LatchSet`. One call disarms every
        // registered latch whose trigger doesn't match this key —
        // no per-action `if !is_shift_X { latch.disarm() }` line.
        self.latches.disarm_others(key.code, key.modifiers);

        match (key.code, key.modifiers) {
            // ── Navigation ────────────────────────────────────────────
            // `FocusWorkspace` emission is centralized in
            // `Model::sync_panes` (called after every key dispatch);
            // local handlers only mutate the cursor.
            (KeyCode::Down, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.move_cursor_by(1);
                PaneOutcome::Consumed
            }
            (KeyCode::Up, m) if !m.contains(KeyModifiers::SHIFT) => {
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
            // Any Char key listed in `agent_shortcuts` spawns that
            // agent for the selected session. Defaults: `c` → Claude,
            // `x` → Codex, `u` → Cursor. AppRoot can remap at startup
            // via `with_agent_shortcuts`. Keys NOT in the map bubble
            // up, so overlays / other components get a fair shot.
            //
            // The (workspace_state, agent_id) → Intent decision lives
            // in `intent::resolve_spawn_agent`; this handler is the
            // execute side. Returning `Intent::NoOp` when nothing is
            // selected is now testable in isolation instead of being
            // a silent inline branch.
            (KeyCode::Char(c), m)
                if self.agent_shortcuts.contains_key(&c)
                    && !m.contains(KeyModifiers::CONTROL)
                    && !m.contains(KeyModifiers::ALT) =>
            {
                let agent_id = self.agent_shortcuts.get(&c).cloned().unwrap_or_default();
                match crate::intent::resolve_spawn_agent(self.selected_workspace(), &agent_id) {
                    crate::intent::Intent::SpawnAgent {
                        workspace_key,
                        agent_id,
                        prompt,
                    } => {
                        tracing::info!(
                            key = %c, %workspace_key, agent_id = %agent_id,
                            "sidebar: emitting Spawn(Agent)"
                        );
                        cmds.push(Command::Spawn {
                            session_key: workspace_key,
                            // The selected session sub-row, if any,
                            // scopes the spawn into a specific
                            // worktree. None → daemon picks the
                            // workspace's default session.
                            session_id: self.selected_session_id(),
                            kind: TerminalKind::Agent(agent_id),
                            cwd: None,
                            initial_prompt: prompt,
                        });
                    }
                    _ => {
                        tracing::warn!(
                            key = %c,
                            "sidebar: agent shortcut pressed but resolver returned NoOp \
                             (no workspace selected or empty agent id)"
                        );
                    }
                }
                PaneOutcome::Consumed
            }
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
            // `z` toggle-snooze is now handled by the catalog
            // dispatch in `Model::dispatch_action(ToggleSnooze)` —
            // same resolver, same effect, reads
            // `ui_defaults.short_snooze` instead of the sidebar's
            // local copy (one fewer place to keep in sync).
            (KeyCode::Char('Z'), m) if m.contains(KeyModifiers::SHIFT) => {
                // Two-press confirm — 1-year snooze is effectively
                // "hide forever" with no obvious undo. The
                // `ConfirmLatch::arm_or_fire` returns true on the
                // SECOND consecutive press; otherwise it arms +
                // returns false. The actual snooze duration lives
                // in the Intent the resolver returns.
                let Some(session_key) = self.selected_session_key().cloned() else {
                    return PaneOutcome::Consumed;
                };
                if !self
                    .latches
                    .arm_or_fire(TRIGGER_LONG_SNOOZE, session_key.clone())
                {
                    return PaneOutcome::Consumed;
                }
                let workspace = self.selected_workspace();
                let intent = crate::intent::resolve_long_snooze(workspace, self.long_snooze);
                if let crate::intent::Intent::Snooze {
                    session_key,
                    duration,
                } = intent
                {
                    let until = chrono::Utc::now()
                        + chrono::Duration::from_std(duration)
                            .unwrap_or(chrono::Duration::days(365));
                    cmds.push(Command::Snooze { session_key, until });
                }
                PaneOutcome::Consumed
            }

            // ── Role filter cycle ─────────────────────────────────────
            // `f` cycles the role filter (All → Author → Reviewer →
            // Assignee → Mentioned → All). Renders as a chip on row 1
            // of the sidebar header. Cursor resets — the row the user
            // was on may have been filtered out, and landing at the
            // new top is less surprising than vanishing off-screen.
            (KeyCode::Char('f'), KeyModifiers::NONE) => {
                self.cycle_role_filter();
                PaneOutcome::Consumed
            }

            // `o` cycles sort order (Default → ByRole → ByRoleSplit →
            // Default). Default is recency; ByRole groups Author /
            // Reviewer / Assignee / Mentioned within each repo;
            // ByRoleSplit adds role section headers between groups.
            (KeyCode::Char('o'), KeyModifiers::NONE) => {
                self.cycle_sort_mode();
                PaneOutcome::Consumed
            }

            // ── Mailbox cycle (Inbox → Inactive → Snoozed → Inbox)
            (KeyCode::Char('S'), m) if m.contains(KeyModifiers::SHIFT) => {
                self.mailbox = match self.mailbox {
                    Mailbox::Inbox => Mailbox::Inactive,
                    Mailbox::Inactive => Mailbox::Snoozed,
                    Mailbox::Snoozed => Mailbox::Inbox,
                };
                // New mailbox → reset cursor to top; old cursor key is
                // almost certainly not visible in the other mailbox.
                self.reset_cursor_and_recompute();
                PaneOutcome::Consumed
            }

            // Shift+X Archive is now handled by the catalog
            // dispatch path in `Model::dispatch_action` — it calls
            // `Sidebar::arm_or_fire_archive` which drives the same
            // two-press latch this match arm used to. First press
            // arms (sidebar chrome shows "press again to confirm");
            // second within the latch window fires `Kill`.

            // Shift+M MergePr is now handled by the catalog
            // dispatch path in `Model::dispatch_action` — it does
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
            } => {
                self.running_terminals
                    .insert(*terminal_id, (session_key.clone(), kind.clone()));
            }
            Event::TerminalExited { terminal_id, .. } => {
                self.running_terminals.remove(terminal_id);
            }
            Event::WorkspaceUpserted(workspace) => {
                let key: SessionKey = (&workspace.key).into();
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
                if matches!(
                    transition,
                    crate::agent_attention::AttentionTransition::NowAsking
                ) {
                    if let Some(workspace) = self.workspaces.get(session_key) {
                        let title = format!("pilot — {} needs input", workspace.name);
                        let body = workspace
                            .primary_task()
                            .map(|t| t.title.clone())
                            .unwrap_or_else(|| workspace.name.clone());
                        self.pending_notifications.push(PendingNotification {
                            title: title.clone(),
                            body,
                        });
                        // Inline footer notice in addition to the OS
                        // popup — covers users with notifications muted
                        // (which is most of them while focused). Hint
                        // severity = 3s fade, dim color.
                        self.pending_asking_notices
                            .push(format!("{} needs input — press ! to jump", workspace.name));
                    }
                }
                if !matches!(
                    transition,
                    crate::agent_attention::AttentionTransition::NoChange
                ) {
                    self.recompute_visible();
                }
            }
            _ => {}
        }
    }
}
