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

use super::{Id, Model, Msg, PaneFocus};
use lazybox_ipc::{Command as IpcCommand, Event as IpcEvent};
use tuirealm::terminal::TerminalAdapter;

impl<T: TerminalAdapter> Model<T> {
    /// Flip lazybox's mouse capture on/off. Issues
    /// `EnableMouseCapture` / `DisableMouseCapture` to stdout so the
    /// host terminal switches between "send mouse to lazybox" and
    /// "handle mouse natively (selection works)". Footer notice
    /// confirms which mode is now active.
    pub(super) fn toggle_mouse_capture(&mut self) {
        self.mouse_capture_on = !self.mouse_capture_on;
        let (msg, _) = if self.mouse_capture_on {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture,);
            (
                "mouse: lazybox (clicks → splitter/focus, wheel → scroll)",
                (),
            )
        } else {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture,);
            (
                "mouse: host (native selection ON — Ctrl-Shift-S to flip back)",
                (),
            )
        };
        self.flash_hint(msg);
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

    /// True when `workspace_key` is already the active removal prompt
    /// or sitting in the queue. The daemon dedupes per-process, but a
    /// re-emit (daemon restart, a `g m` `PrMerged` racing the poll's
    /// `MergedPrRemovable`) could otherwise stack duplicate prompts —
    /// belt and braces.
    fn removal_already_pending(&self, workspace_key: &lazybox_core::WorkspaceKey) -> bool {
        let active = self
            .active_removal_prompt
            .as_ref()
            .map(|(k, _)| k == workspace_key)
            .unwrap_or(false);
        let queued = self
            .pending_removal_prompts
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
            if visible {
                self.redraw = true;
            }
            return;
        }
        // Enforce the confirmed-merge latch centrally, before any pane or
        // the auto-merge check sees the workspace. Once GitHub accepted a
        // merge (`Event::PrMerged` latched the key), an incoming
        // poll/snapshot that still reports `Open` is stale — patch the
        // owned event to MERGED here so the sidebar row AND the right-pane
        // header agree, and `maybe_auto_merge` reads the merged state
        // (which `merge_block_reason` then blocks) instead of re-firing a
        // redundant merge. No-op unless a key is latched.
        if !self.merge_confirmed.is_empty() {
            match &mut event {
                IpcEvent::WorkspaceUpserted(ws) => self.apply_merge_latch(ws),
                IpcEvent::Snapshot { workspaces, .. } => {
                    for ws in workspaces.iter_mut() {
                        self.apply_merge_latch(ws);
                    }
                }
                _ => {}
            }
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
        // Project lifecycle events. Mirror into `self.projects` so
        // the sidebar can render headers from it, then push the
        // updated map to the sidebar component.
        if let IpcEvent::ProjectUpserted(p) = &event {
            self.projects.insert(p.key.clone(), (**p).clone());
            // Daemon owns this project now — stop treating it as a
            // client-side placeholder so a later scope edit won't yank
            // it out from under the daemon's `ProjectRemoved`.
            self.synthesized_projects.remove(&p.key);
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
                self.projects.insert(p.key.clone(), p.clone());
                self.synthesized_projects.remove(&p.key);
            }
            self.sidebar.apply_projects(self.projects.clone());
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
            if !self.removal_already_pending(workspace_key) {
                self.pending_removal_prompts
                    .push_back(super::RemovalPrompt {
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
            issue_workspace_key,
            pr_workspace_key,
            issue_label,
            pr_label,
        } = &event
        {
            self.flash_info(format!("merged {issue_label} into {pr_label}"));
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
            pr_label, reason, ..
        } = &event
        {
            self.flash_error(format!("✗ merge failed — {pr_label}: {reason}"));
            self.redraw = true;
            return;
        }
        // `Shift-C` reached GitHub and the issue was closed. The local
        // Task still reads `Open` until the next poll, so flash a notice
        // now; the daemon's open→closed detection (which the close
        // handler woke) follows with the workspace-removal prompt.
        if let IpcEvent::IssueClosed { issue_label, .. } = &event {
            self.flash_info(format!("closed {issue_label}"));
            self.redraw = true;
            return;
        }
        // `Shift-C` reached GitHub and was rejected — the close did NOT
        // happen. Surface a distinct, persistent error naming the reason
        // (mirrors `PrMergeFailed`). The issue stays Open/actionable.
        if let IpcEvent::IssueCloseFailed {
            issue_label,
            reason,
            ..
        } = &event
        {
            self.flash_error(format!("✗ close failed — {issue_label}: {reason}"));
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
            if !self.removal_already_pending(workspace_key) {
                let reason = match terminal_state {
                    lazybox_ipc::RemovableTerminalState::Merged => super::RemovalReason::Merged,
                    lazybox_ipc::RemovableTerminalState::Closed => super::RemovalReason::Closed,
                };
                self.pending_removal_prompts
                    .push_back(super::RemovalPrompt {
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
        // Clear the lazy-fetch dedupe entry when a workspace is
        // removed, so a re-added workspace (e.g. user re-checks a
        // filter) gets a fresh details fetch on next focus.
        if let IpcEvent::WorkspaceRemoved(key) = &event {
            self.pr_details_fetched.remove(key);
            // A merged/removed workspace can't re-fire; drop its arming
            // latch so the set doesn't leak keys across a session.
            self.auto_merge_fired.remove(key);
            // Same for the confirmed-merge latch — the row is gone, so a
            // stale entry could only leak or mis-patch a re-added key.
            self.merge_confirmed.remove(key);
            // Drop any Activity-pane visibility override so a re-added
            // workspace re-applies the empty-aware default instead of a
            // stale manual choice.
            self.activity_pane_overrides.remove(key);
            // Forget the remembered pane focus too, so a re-added
            // workspace doesn't inherit a stale restore target.
            let session_key: lazybox_core::SessionKey = key.into();
            self.workspace_focus.remove(&session_key);
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
            if self.pending_labels_request.as_ref() == Some(workspace_key) {
                self.mount_manage_labels(workspace_key.clone(), labels.clone());
                self.redraw = true;
            }
            return;
        }
        // First-time worktree provisioning progress. Drives the
        // spinner + step checklist modal. The panes ignore it, so
        // handle + return here rather than fan out. The matching
        // `TerminalSpawned` below dismisses the modal once ready.
        if let IpcEvent::WorktreeProgress {
            session_key,
            step,
            status,
        } = &event
        {
            self.apply_worktree_progress(session_key.clone(), *step, status.clone());
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
        // Client-side "auto-merge on green": a poll update to an armed
        // workspace may have just made its PR merge-ready. The event
        // carries the full workspace, so check it directly.
        if let IpcEvent::WorkspaceUpserted(ws) = &event {
            self.maybe_auto_merge(ws);
        }
        self.right.on_daemon_event(&event);
        self.terminals.on_daemon_event(&event);
        if let Some(p) = self.status.polling.as_mut() {
            p.feed_daemon_event(&event);
        }
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
            IpcEvent::ProviderError {
                source,
                message,
                detail,
                kind,
            } if !source.starts_with("spawn") => {
                self.status.sync.note_error(source, kind, message, detail);
            }
            _ => {}
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
                    source, message, ..
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
                        // No `TerminalSpawned` is coming, so a progress
                        // checklist that reached "Starting agent" would
                        // hang — surface the error in the footer and
                        // tear it down. `ProviderError` carries no
                        // session, so this clears whichever checklist
                        // is up (only ever one).
                        self.force_dismiss_worktree_progress();
                        self.flash_error(format!("✗ spawn failed — {message}"));
                    }
                    // Manual refresh failed — convert the ack flag
                    // into a "sync failed" notice so the user
                    // doesn't have to guess whether their Shift-R
                    // worked.
                    if self.pending_refresh_ack {
                        self.pending_refresh_ack = false;
                        self.flash_sync_error(
                            source,
                            format!("✗ sync failed — {source}: {message}"),
                        );
                    }
                }
                _ => {}
            }
        }
        // CleanWorktrees finished — replace the "cleaning…" notice
        // with the final count so the user sees how much was done.
        if let IpcEvent::CleanWorktreesCompleted { removed, skipped } = &event {
            let msg = if *skipped == 0 {
                format!("cleaned {removed} worktree(s)")
            } else {
                format!("cleaned {removed} worktree(s) · kept {skipped} (active)")
            };
            self.flash_hint(msg);
        }
        // Daemon-pushed user notice (e.g. auto-cleanup of a merged
        // PR's worktrees). Surface the body in the footer; the OS
        // banner path isn't wired for daemon-originated notices.
        if let IpcEvent::Notification { body, .. } = &event {
            self.flash_hint(body.clone());
        }
        // Worktree inspector replied. Swap the placeholder for the
        // real list. `mount_inspect_list` is idempotent — calling it
        // again after a delete re-renders the now-shorter list, so
        // the inspector stays open across edits.
        if let IpcEvent::WorktreesInspected { inspections } = &event {
            self.mount_inspect_list(inspections.clone());
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
            // A terminal just appeared — auto-focus the Terminals
            // pane so the user can start typing immediately, and
            // clear any legacy "Spawning…" footer notice that was set
            // when the matching Spawn command was sent. The animated
            // spawn *spinner* is NOT cleared here: it's a projection of
            // the live terminal set (recomputed at the tail of this
            // function via `recompute_spawn_spinner`), so a spawn event
            // for an unrelated workspace can't clear the wrong spinner
            // and a missing one can't strand it (#206).
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
            self.status.clear_spawning_notice();
            self.needs_pane_sync = true;
            // Pinned spawn-follow: a `w` press recorded the workspace it
            // targeted. When that workspace's terminal finally lands —
            // possibly seconds later after a cold worktree provision, and
            // possibly after the user navigated elsewhere — pull the
            // cursor back to it and mark the new terminal as the tab to
            // activate, so `w` reliably ends on the freshly-spawned agent
            // rather than wherever the cursor drifted. `pending_focus_terminal`
            // is applied by the upcoming `sync_panes`, after
            // `set_active_session` has rebuilt the followed workspace's
            // visible terminal set. `TerminalFocusRequested` (singleton
            // guard: the agent already existed) carries no session key, so
            // recover it from the terminal's own slot.
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
            if let Some((spawned_key, terminal_id)) = spawned
                && self.spawn_follow_to.as_ref() == Some(&spawned_key)
            {
                self.spawn_follow_to = None;
                self.sidebar.focus_workspace_key(&spawned_key);
                self.pending_focus_terminal = Some(terminal_id);
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
        } else {
            self.needs_pane_sync = true;
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

    /// Client-side "auto-merge on green" trigger. Called after every
    /// `WorkspaceUpserted` with the freshly-upserted workspace. When
    /// the workspace is armed and its own PR just satisfied every merge
    /// guard ([`lazybox_tui_core::intent::should_auto_merge`]), fire the
    /// same `MergePr` the manual `g m` uses — exactly once per arming.
    ///
    /// The persisted `auto_merge_on_green` flag is the arm; the
    /// transient `auto_merge_fired` set is the one-shot latch. Once
    /// dispatched we leave the key latched until the PR is no longer
    /// merge-ready (merged, or CI/state slipped) — at which point we
    /// clear it, so a genuine re-green after a failed race can re-fire,
    /// but a re-broadcast of the same green state can't double-merge.
    fn maybe_auto_merge(&mut self, ws: &lazybox_core::Workspace) {
        if crate::intent::should_auto_merge(ws) {
            if self.auto_merge_fired.insert(ws.key.clone()) {
                self.flash_info(format!("auto-merging {} — CI green", ws.name));
                self.send_cmd(IpcCommand::MergePr {
                    workspace_key: ws.key.clone(),
                });
            }
        } else {
            // No longer merge-ready — release the latch so a fresh green
            // (e.g. after a failing-check race made the first attempt a
            // no-op) re-arms the one-shot.
            self.auto_merge_fired.remove(&ws.key);
        }
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
    /// workspace, so both panes and `maybe_auto_merge` see one
    /// consistent state. No-op for un-latched keys.
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
            } else {
                self.escape_latch.disarm();
            }
        }
    }

    /// Fire bare `Work` once the `w` leader has sat idle for
    /// `ui_defaults.escape_window` with no follow-up key (issue #224).
    /// This is the bare-`w` path: the user pressed `w` and didn't pick a
    /// scoped agent (`w c` / `w x`), so we honor "work on this" against
    /// the running-or-default agent. A scoped chord, an Esc cancel, or
    /// any other key clears `work_leader_at` in the key handler before
    /// this fires. Called once per run-loop iteration (the loop ticks
    /// ~every 16ms even while idle), so it fires within a frame of the
    /// window elapsing.
    pub fn tick_work_leader(&mut self) {
        let Some(armed_at) = self.work_leader_at else {
            return;
        };
        if armed_at.elapsed() < self.ui_defaults.escape_window {
            return;
        }
        // The window elapsed — the leader resolves now, either way.
        self.work_leader_at = None;
        self.leader.disarm();
        self.redraw = true;
        // Don't fire bare `Work` if the user's context moved on after
        // pressing `w`: a modal opened, or focus left for the terminal
        // (e.g. a spawn landed). Mouse clicks cancel the leader at the
        // event itself (see `handle_mouse`). Firing here would spawn /
        // inject against whatever happens to be selected — a surprise.
        if !self.modal_stack.is_empty() || self.focus == PaneFocus::Terminals {
            return;
        }
        let cmds = self.dispatch_action(&lazybox_tui_core::action::Action::Work);
        self.flush_dispatched_cmds(cmds);
        self.sync_panes();
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
    /// - a background-poll spinner is active (the spinner glyph +
    ///   the `· Ns` elapsed counter both need a re-render every
    ///   tick or the user sees `4s` frozen forever even though the
    ///   poll is still in flight).
    pub fn polling_tick(&mut self) -> Option<Msg> {
        let msg = self.status.polling_tick();
        // Backstop the projection: if the spawn's terminal slipped in
        // without a daemon event reaching `handle_daemon_event` (e.g. it
        // already existed when the spawn was sent), the idle tick still
        // clears the spinner.
        let cleared_spawn = self.recompute_spawn_spinner();
        let needs_redraw = msg.is_some()
            || cleared_spawn
            || self.status.bg_poll.is_some()
            || self.status.spawning.is_some();
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
    /// Called after every key dispatch and once per daemon-event drain
    /// batch (via [`Self::flush_pane_sync`]).
    pub(super) fn sync_panes(&mut self) {
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
        self.right.set_workspace(workspace);
        self.terminals.set_active_session(session_key);
        self.terminals.set_layout(layout);
        // A pinned `w` spawn-follow asked for a specific terminal to be
        // the active tab. Apply it now that `set_active_session` has
        // (re)built the visible set for the followed workspace, so the
        // user lands on the fresh agent and not whatever tab the
        // workspace last had — a no-op if the terminal isn't in the
        // active session's visible set.
        if let Some(tid) = self.pending_focus_terminal.take() {
            self.terminals.focus_terminal(tid);
        }
        // If the selection landed on a workspace whose Activity pane is
        // hidden while that pane held focus, hand focus to the terminal
        // so keystrokes don't vanish into an unrendered pane.
        self.enforce_pane_focus();
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
            self.focus = remembered;
            self.set_focus_attr();
            self.redraw = true;
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
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
        }
    }
}
