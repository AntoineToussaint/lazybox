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

use super::{Model, Msg, PaneFocus};
use pilot_ipc::{Command as IpcCommand, Event as IpcEvent};
use tuirealm::terminal::TerminalAdapter;

impl<T: TerminalAdapter> Model<T> {
    /// Flip pilot's mouse capture on/off. Issues
    /// `EnableMouseCapture` / `DisableMouseCapture` to stdout so the
    /// host terminal switches between "send mouse to pilot" and
    /// "handle mouse natively (selection works)". Footer notice
    /// confirms which mode is now active.
    pub(super) fn toggle_mouse_capture(&mut self) {
        use crate::realm::components::footer::{Notice, NoticeSeverity};
        self.mouse_capture_on = !self.mouse_capture_on;
        let (msg, _) = if self.mouse_capture_on {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture,);
            ("mouse: pilot (clicks → splitter/focus, wheel → scroll)", ())
        } else {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture,);
            (
                "mouse: host (native selection ON — Ctrl-Shift-S to flip back)",
                (),
            )
        };
        self.status.notice = Some(Notice::new(msg, NoticeSeverity::Hint));
        self.redraw = true;
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
    }

    /// Forward an inbound daemon event into all three panes. Each
    /// pane decides whether the event is relevant. After the very
    /// first Snapshot, apply any pending CLI preselect. Also feeds
    /// the polling modal so it can detect "first task arrived".
    pub fn handle_daemon_event(&mut self, event: IpcEvent) {
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
        // Project lifecycle events. Mirror into `self.projects` so
        // the sidebar can render headers from it, then push the
        // updated map to the sidebar component.
        if let IpcEvent::ProjectUpserted(p) = &event {
            self.projects.insert(p.key.clone(), (**p).clone());
            self.sidebar.apply_projects(self.projects.clone());
            // Hand-off from Shift-N → CreateProject: the project
            // just landed in the sidebar, but its RepoHeader row
            // is unreachable via j/k (header rows are skipped). If
            // this upsert matches the name the user just typed,
            // focus the row + auto-mount the new-workspace input so
            // they can keep typing without re-aiming.
            if self.pending_focus_project_name.as_deref() == Some(p.name.as_str()) {
                self.pending_focus_project_name = None;
                let project_key = p.key.clone();
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
            self.redraw = true;
            return;
        }
        // Snapshot's project list seeds the same map on reconnect.
        // Push to the sidebar AFTER the snapshot's WorkspaceUpserted-
        // equivalent rows are processed below, so the first render
        // already has both layers.
        if let IpcEvent::Snapshot { projects, .. } = &event {
            for p in projects {
                self.projects.insert(p.key.clone(), p.clone());
            }
            self.sidebar.apply_projects(self.projects.clone());
        }

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
            let already_active = self
                .active_removal_prompt
                .as_ref()
                .map(|k| k == workspace_key)
                .unwrap_or(false);
            let already_queued = self
                .pending_removal_prompts
                .iter()
                .any(|(k, _, _, _)| k == workspace_key);
            if !already_active && !already_queued {
                self.pending_removal_prompts.push_back((
                    workspace_key.clone(),
                    label.clone(),
                    title.clone(),
                    *active_terminal_count,
                ));
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
            let already_active = self
                .active_merge_prompt
                .as_ref()
                .map(|(i, _)| i == issue_workspace_key)
                .unwrap_or(false);
            let already_queued = self
                .pending_merge_prompts
                .iter()
                .any(|(i, _, _, _, _)| i == issue_workspace_key);
            if !already_active && !already_queued {
                self.pending_merge_prompts.push_back((
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
            issue_label,
            pr_label,
            ..
        } = &event
        {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.status.notice = Some(Notice::new(
                format!("merged {issue_label} into {pr_label}"),
                NoticeSeverity::Info,
            ));
            self.redraw = true;
            return;
        }
        // Shift-M completed: GitHub accepted the merge. Optimistically
        // flip the local task state to Merged so the badge pill
        // changes IMMEDIATELY — without this the user has to wait up
        // to the next poll cycle (~30s) for the visual to catch up,
        // which felt broken. Refresh still goes out so the next
        // poll backfills everything else.
        if let IpcEvent::PrMerged {
            pr_label,
            workspace_key,
            ..
        } = &event
        {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.sidebar.mark_workspace_merged(workspace_key);
            self.status.notice = Some(Notice::new(
                format!("merged {pr_label}"),
                NoticeSeverity::Info,
            ));
            // Queue a "remove merged workspace?" prompt. Reuses the
            // existing RemoveOutOfScope confirm flow (Kill on Yes,
            // keep on No) — same UX, just triggered after a merge
            // instead of an out-of-scope detection. Active-terminal
            // count from sidebar lookup so the message reads truthfully.
            let already_active = self
                .active_removal_prompt
                .as_ref()
                .map(|k| k == workspace_key)
                .unwrap_or(false);
            let already_queued = self
                .pending_removal_prompts
                .iter()
                .any(|(k, _, _, _)| k == workspace_key);
            if !already_active && !already_queued {
                self.pending_removal_prompts.push_back((
                    workspace_key.clone(),
                    pr_label.clone(),
                    Some(format!("PR {pr_label} merged — remove workspace?")),
                    0,
                ));
                self.maybe_mount_next_removal_prompt();
            }
            self.send_cmd(IpcCommand::Refresh);
            self.redraw = true;
            return;
        }
        // Clear the lazy-fetch dedupe entry when a workspace is
        // removed, so a re-added workspace (e.g. user re-checks a
        // filter) gets a fresh details fetch on next focus.
        if let IpcEvent::WorkspaceRemoved(key) = &event {
            self.pr_details_fetched.remove(key);
        }
        self.sidebar.on_daemon_event(&event);
        // Surface Active→Asking transitions in the footer with a
        // brief Hint-severity notice. The sidebar already pushed an
        // OS notification + flipped its `?` glyph; this is the
        // in-pilot equivalent for users running with notifications
        // muted. Last one wins if multiple workspaces transition
        // in the same tick — they'll see them in sequence anyway as
        // the 3s Hint fade clears each.
        if let Some(msg) = self.sidebar.drain_pending_asking_notices().pop() {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.status.notice = Some(Notice::new(msg, NoticeSeverity::Hint));
        }
        self.right.on_daemon_event(&event);
        self.terminals.on_daemon_event(&event);
        if let Some(p) = self.status.polling.as_mut() {
            p.feed_daemon_event(&event);
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
                        use crate::realm::components::footer::{Notice, NoticeSeverity};
                        self.pending_refresh_ack = false;
                        self.status.notice = Some(Notice::new(
                            format!("✓ sync ok — {count} tasks from {source}"),
                            NoticeSeverity::Info,
                        ));
                    }
                    self.redraw = true;
                }
                IpcEvent::ProviderError {
                    source, message, ..
                } => {
                    // Manual refresh failed — convert the ack flag
                    // into a "sync failed" notice so the user
                    // doesn't have to guess whether their Shift-R
                    // worked.
                    if self.pending_refresh_ack {
                        use crate::realm::components::footer::{Notice, NoticeSeverity};
                        self.pending_refresh_ack = false;
                        self.status.notice = Some(Notice::new(
                            format!("✗ sync failed — {source}: {message}"),
                            NoticeSeverity::Permanent,
                        ));
                        self.redraw = true;
                    }
                }
                _ => {}
            }
        }
        // CleanWorktrees finished — replace the "cleaning…" notice
        // with the final count so the user sees how much was done.
        if let IpcEvent::CleanWorktreesCompleted { removed, skipped } = &event {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            let msg = if *skipped == 0 {
                format!("cleaned {removed} worktree(s)")
            } else {
                format!("cleaned {removed} worktree(s) · kept {skipped} (active)")
            };
            self.status.notice = Some(Notice::new(msg, NoticeSeverity::Hint));
            self.redraw = true;
        }
        if is_snapshot && self.preselect.is_some() {
            self.apply_preselect();
        }
        if is_spawn {
            // A terminal just appeared — auto-focus the Terminals
            // pane so the user can start typing immediately, and
            // clear any "Spawning…" footer notice that was set when
            // the matching Spawn command was sent.
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
            self.status.clear_spawning_notice();
            self.sync_panes();
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
        } else {
            self.sync_panes();
        }
        self.redraw = true;
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
        if let Some((session_key, index)) = self.right.tick() {
            tracing::info!(
                %session_key,
                index,
                "auto-mark-read fired → Command::MarkActivityRead",
            );
            self.send_cmd(IpcCommand::MarkActivityRead { session_key, index });
            self.redraw = true;
        }
    }

    /// Drive the polling spinner + termination check from the run
    /// loop. Cheap; called every iteration. Returns Some(msg) when
    /// the polling modal wants to be torn down.
    ///
    /// Flips `redraw` when:
    /// - the modal tick produced a termination message (caller will
    ///   apply it), OR
    /// - a background-poll spinner is active (the spinner glyph +
    ///   the `· Ns` elapsed counter both need a re-render every
    ///   tick or the user sees `4s` frozen forever even though the
    ///   poll is still in flight).
    pub fn polling_tick(&mut self) -> Option<Msg> {
        let msg = self.status.polling_tick();
        let needs_redraw = msg.is_some() || self.status.bg_poll.is_some();
        if needs_redraw {
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
    /// Called after every key dispatch and every daemon event.
    pub(super) fn sync_panes(&mut self) {
        let workspace = self.sidebar.selected_workspace().cloned();
        let session_key = self.sidebar.selected_workspace_key().cloned();
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
        self.right.set_workspace(workspace);
        self.terminals.set_active_session(session_key);
        self.terminals.set_layout(layout);
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
            let _ = self.sidebar.focus_session_id(pilot_core::SessionId(uuid));
            // Move focus to terminals so the user can type immediately.
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
        }
    }
}
