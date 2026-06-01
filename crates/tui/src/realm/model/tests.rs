#[cfg(test)]
mod effects_tests {
    //! Handler effect-contract tests.
    //!
    //! These exercise the `handle_X(&mut self, ...) -> Vec<IpcCommand>`
    //! contract on the orchestrator's biggest message handlers
    //! (textarea submit, input submit, confirm y/n, modal dismiss,
    //! choice pick). Each test:
    //!
    //!   1. constructs a `Model` with `new_for_test`;
    //!   2. seeds the internal state the handler expects to read
    //!      (`pending_reply`, `active_merge_prompt`, modal stack, …);
    //!   3. calls `handle_X(...)`;
    //!   4. asserts on the returned `Vec<IpcCommand>` directly —
    //!      no need to drive a real IPC client.
    //!
    //! Inline `mod tests` (not `tests/`) so the test can poke
    //! private fields. Effect contracts that drift would be a
    //! silent regression otherwise — these tests freeze them.
    use super::super::*;
    use pilot_core::{SessionKey, WorkspaceKey};
    use pilot_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    /// Reply submission with a non-empty body + a pending reply
    /// target produces `PostReply` followed by `Refresh` (in that
    /// order — the Refresh kicks an immediate poll instead of
    /// waiting on the 60s loop).
    #[test]
    fn textarea_submitted_with_pending_reply_returns_postreply_then_refresh() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");
        m.pending_reply = Some(key.clone());
        let cmds = m.handle_textarea_submitted("hello".into());
        assert_eq!(cmds.len(), 2);
        match &cmds[0] {
            IpcCommand::PostReply { session_key, body } => {
                assert_eq!(session_key, &key);
                assert_eq!(body, "hello");
            }
            other => panic!("expected PostReply, got {other:?}"),
        }
        assert!(matches!(cmds[1], IpcCommand::Refresh));
    }

    /// Arm a sticky "✗ sync failed" banner for `source` the way a
    /// failed manual refresh (Shift-R) does, and assert it landed.
    /// Returns the model ready for the recovery half of each test.
    fn model_with_sync_error(source: &str) -> Model<tuirealm::terminal::TestTerminalAdapter> {
        use crate::realm::components::footer::NoticeSeverity;
        use pilot_ipc::Event as IpcEvent;

        let mut m = build_model();
        // PollCompleted/ProviderError are only processed when the
        // initial polling modal is gone.
        m.status.polling = None;

        m.pending_refresh_ack = true;
        m.handle_daemon_event(IpcEvent::ProviderError {
            source: source.into(),
            message: "boom".into(),
            detail: String::new(),
            kind: "retryable".into(),
        });

        assert_eq!(
            m.sync_error_source.as_deref(),
            Some(source),
            "sync error should be armed for {source}"
        );
        let n = m.status.notice.as_ref().expect("banner set");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("sync failed"));
        m
    }

    /// A manual-refresh sync failure paints a sticky "✗ sync failed"
    /// banner; the next successful poll (auto-cycle) from the *same*
    /// provider must clear it so a recovered sync doesn't leave the
    /// red notice up forever.
    #[test]
    fn provider_error_banner_clears_on_next_successful_poll() {
        use pilot_ipc::Event as IpcEvent;

        let mut m = model_with_sync_error("github");

        // Sync recovers on a later auto-cycle (no pending ack).
        m.handle_daemon_event(IpcEvent::PollCompleted {
            source: "github".into(),
            count: 3,
        });
        assert!(m.sync_error_source.is_none(), "flag cleared on recovery");
        assert!(
            m.status.notice.is_none(),
            "stale sync-failed banner should be cleared"
        );
    }

    /// The banner is owned by the provider that failed. A successful
    /// poll from a *different* provider (pilot polls GitHub, Linear and
    /// Slack concurrently) must NOT erase a still-valid failure banner.
    #[test]
    fn provider_error_banner_survives_other_providers_poll() {
        use pilot_ipc::Event as IpcEvent;

        let mut m = model_with_sync_error("github");

        // A different provider's auto-cycle succeeds while GitHub is
        // still down.
        m.handle_daemon_event(IpcEvent::PollCompleted {
            source: "linear".into(),
            count: 7,
        });
        assert_eq!(
            m.sync_error_source.as_deref(),
            Some("github"),
            "github banner must stay armed when linear recovers"
        );
        let n = m.status.notice.as_ref().expect("github banner still up");
        assert!(n.message.contains("sync failed"));

        // …and GitHub's own recovery still clears it.
        m.handle_daemon_event(IpcEvent::PollCompleted {
            source: "github".into(),
            count: 1,
        });
        assert!(m.sync_error_source.is_none());
        assert!(m.status.notice.is_none());
    }

    /// Any unrelated notice supersedes the sync-error banner and
    /// disarms the "clear on recovery" tag — otherwise a later poll
    /// would wrongly clear whatever notice is now on screen.
    #[test]
    fn unrelated_notice_disarms_sync_error_tag() {
        use pilot_ipc::Event as IpcEvent;

        let mut m = model_with_sync_error("github");

        m.flash_info("something else happened");
        assert!(
            m.sync_error_source.is_none(),
            "a fresh notice must disarm the sync-error tag"
        );

        // A subsequent GitHub poll must leave the new notice intact.
        m.handle_daemon_event(IpcEvent::PollCompleted {
            source: "github".into(),
            count: 2,
        });
        assert!(
            m.status.notice.is_some(),
            "the unrelated notice must not be cleared by recovery logic"
        );
    }

    /// Empty body short-circuits — no command produced, the
    /// modal is still popped (internal state), and the pending
    /// reply target is cleared. The whitespace case is handled
    /// the same way.
    #[test]
    fn textarea_submitted_with_empty_body_returns_no_commands() {
        let mut m = build_model();
        m.pending_reply = Some(SessionKey::from("github:o/r#1"));
        let cmds = m.handle_textarea_submitted("   ".into());
        assert!(cmds.is_empty());
        assert!(m.pending_reply.is_none());
    }

    /// No pending reply target → no command, even with a body.
    /// Defensive case (shouldn't reach this handler without a
    /// pending reply, but the contract handles it).
    #[test]
    fn textarea_submitted_with_no_target_returns_no_commands() {
        let mut m = build_model();
        let cmds = m.handle_textarea_submitted("hello".into());
        assert!(cmds.is_empty());
    }

    /// NewWorkspace input with a non-empty trimmed name AND a
    /// pre-stashed project_key produces `CreateWorkspace { name,
    /// project_key }`. Without a stashed project_key the submit
    /// drops (see `mount_new_workspace_input` — the catalog `n`
    /// flow only mounts when a project is focused).
    #[test]
    fn input_submitted_for_new_workspace_returns_create_workspace() {
        let mut m = build_model();
        let pk = pilot_core::ProjectKey::local("my-project");
        m.modal_stack.push(Id::NewWorkspace);
        m.pending_new_workspace_project = Some(pk.clone());
        let cmds = m.handle_input_submitted("  my-feature  ".into());
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::CreateWorkspace { name, project_key } => {
                assert_eq!(name, "my-feature");
                assert_eq!(project_key, &pk);
            }
            other => panic!("expected CreateWorkspace, got {other:?}"),
        }
    }

    /// Empty / whitespace-only input is dropped silently.
    #[test]
    fn input_submitted_with_empty_text_returns_no_commands() {
        let mut m = build_model();
        m.modal_stack.push(Id::NewWorkspace);
        let cmds = m.handle_input_submitted("   ".into());
        assert!(cmds.is_empty());
    }

    /// `y` on a RemoveOutOfScope confirm produces a `Kill` for
    /// the workspace + clears the prompt slot.
    #[test]
    fn confirmed_yes_on_remove_out_of_scope_returns_kill() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#1");
        m.active_removal_prompt = Some(ws_key.clone());
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_confirmed(true);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::Kill { session_key } => {
                assert_eq!(session_key, &SessionKey::from(&ws_key));
            }
            other => panic!("expected Kill, got {other:?}"),
        }
    }

    /// `n` on RemoveOutOfScope clears the slot without producing
    /// a Kill — user said no, daemon doesn't need to hear about it.
    #[test]
    fn confirmed_no_on_remove_out_of_scope_returns_no_commands() {
        let mut m = build_model();
        m.active_removal_prompt = Some(WorkspaceKey::new("github:o/r#1"));
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_confirmed(false);
        assert!(cmds.is_empty());
    }

    /// `y` on MergeConfirm → `ConfirmMerge { accept: true }`.
    /// `n` on the same → `ConfirmMerge { accept: false }`. Both
    /// produce a command (the daemon needs to know either way so
    /// it stops re-prompting).
    #[test]
    fn confirmed_routes_merge_confirm_yes_and_no_to_daemon() {
        for (input, expected_accept) in [(true, true), (false, false)] {
            let mut m = build_model();
            let issue = WorkspaceKey::new("github:o/r#1");
            let pr = WorkspaceKey::new("github:o/r#2");
            m.active_merge_prompt = Some((issue.clone(), pr.clone()));
            m.modal_stack.push(Id::MergeConfirm);
            let cmds = m.handle_confirmed(input);
            assert_eq!(cmds.len(), 1, "input={input}");
            match &cmds[0] {
                IpcCommand::ConfirmMerge {
                    issue_workspace_key,
                    pr_workspace_key,
                    accept,
                } => {
                    assert_eq!(issue_workspace_key, &issue);
                    assert_eq!(pr_workspace_key, &pr);
                    assert_eq!(*accept, expected_accept, "input={input}");
                }
                other => panic!("expected ConfirmMerge, got {other:?}"),
            }
        }
    }

    /// Esc on a MergeConfirm modal dismisses WITHOUT signalling the
    /// daemon. Pre-fix this sent `ConfirmMerge { accept: false }`,
    /// which pinned the issue in `rejected_merge` for the whole
    /// session — the user never saw the prompt again. Now: just
    /// close the modal; the daemon's `prompted_merge` re-fires
    /// after 5 minutes so the prompt self-heals.
    #[test]
    fn modal_dismissed_on_merge_confirm_is_silent_dismissal() {
        let mut m = build_model();
        m.active_merge_prompt = Some((
            WorkspaceKey::new("github:o/r#1"),
            WorkspaceKey::new("github:o/r#2"),
        ));
        m.modal_stack.push(Id::MergeConfirm);
        let cmds = m.handle_modal_dismissed();
        assert!(
            cmds.is_empty(),
            "Esc on merge modal must NOT signal accept:false, got: {cmds:?}",
        );
        assert!(
            m.active_merge_prompt.is_none(),
            "active_merge_prompt slot must clear so the queue can advance",
        );
    }

    /// Esc on a RemoveOutOfScope modal clears the slot but
    /// produces no command — there's nothing to tell the daemon;
    /// the workspace stays out of scope on its end too.
    #[test]
    fn modal_dismissed_on_remove_out_of_scope_clears_slot_silently() {
        let mut m = build_model();
        m.active_removal_prompt = Some(WorkspaceKey::new("github:o/r#1"));
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_modal_dismissed();
        assert!(cmds.is_empty());
        assert!(m.active_removal_prompt.is_none());
    }

    /// Inter-event cadence of the OS momentum stream (~16 ms frame
    /// rate). Gaps this tight accumulate the burst toward the hard
    /// stop.
    const MOMENTUM_GAP: std::time::Duration = std::time::Duration::from_millis(16);
    /// Inter-event cadence of deliberate hand-driven ticks, wider than
    /// the damper's 60 ms momentum threshold — each one restarts the
    /// burst.
    const USER_GAP: std::time::Duration = std::time::Duration::from_millis(120);

    /// Fresh gesture (no prior scroll) returns the full STEP. Exercises
    /// the public wrapper that reads the real clock.
    #[test]
    fn dampen_scroll_step_fresh_gesture_returns_initial_step() {
        let mut m = build_model();
        assert_eq!(m.dampen_scroll_step(false), 5);
    }

    /// A momentum stream (tight ~16 ms cadence) decays the step.
    /// Events 1-4 stay at full STEP (5), events 5-7 drop to MID (3),
    /// events 8-11 drop to TAIL (1).
    #[test]
    fn dampen_scroll_step_decays_within_momentum_stream() {
        let mut m = build_model();
        let base = std::time::Instant::now();
        let at = |n: u32| base + MOMENTUM_GAP * n;
        for i in 0..4 {
            assert_eq!(m.dampen_scroll_step_at(false, at(i)), 5);
        }
        for i in 4..7 {
            assert_eq!(m.dampen_scroll_step_at(false, at(i)), 3);
        }
        for i in 7..11 {
            assert_eq!(m.dampen_scroll_step_at(false, at(i)), 1);
        }
    }

    /// Past `STOP_AT` (event 12) a momentum stream returns 0, killing
    /// the OS momentum tail so the view actually stops within the
    /// issue's 100–200 ms acceptance window instead of trickling
    /// onward at STEP=1 for the full 1–2 s tail.
    #[test]
    fn dampen_scroll_step_momentum_tail_hard_stops_past_stop_at() {
        let mut m = build_model();
        let base = std::time::Instant::now();
        // Saturate the burst (11 events still admit at TAIL=1).
        for i in 0..11 {
            let _ = m.dampen_scroll_step_at(false, base + MOMENTUM_GAP * i);
        }
        // Event 12 onwards: dropped.
        for i in 11..41 {
            assert_eq!(m.dampen_scroll_step_at(false, base + MOMENTUM_GAP * i), 0);
        }
    }

    /// Regression for #86: deliberate ticks spaced wider than the
    /// momentum cadence must never decay or drop. Each tick restarts
    /// the burst, so even 40 sustained scrolls keep returning the full
    /// step — only the OS momentum tail is allowed to stop.
    #[test]
    fn dampen_scroll_step_sustained_user_ticks_never_drop() {
        let mut m = build_model();
        let base = std::time::Instant::now();
        for i in 0..40 {
            assert_eq!(
                m.dampen_scroll_step_at(false, base + USER_GAP * i),
                5,
                "user tick {i} must stay at full step, never decay or drop",
            );
        }
    }

    /// Direction reversal admits immediately at full STEP — real
    /// trackpad momentum never reverses, so a reverse-flick is
    /// unambiguous user intent. Swallowing the first reverse press
    /// would feel unresponsive when the user is course-correcting
    /// after an overshoot.
    #[test]
    fn dampen_scroll_step_direction_reversal_admits_immediately() {
        let mut m = build_model();
        let base = std::time::Instant::now();
        for i in 0..6 {
            let _ = m.dampen_scroll_step_at(false, base + MOMENTUM_GAP * i);
        }
        // Reverse: admit at full step (no dropped event).
        assert_eq!(m.dampen_scroll_step_at(true, base + MOMENTUM_GAP * 6), 5);
        // The reversal also restarts the burst, so the next
        // same-direction event stays at full step.
        assert_eq!(m.dampen_scroll_step_at(true, base + MOMENTUM_GAP * 7), 5);
    }

    /// Reverse-flick rescues a saturated burst — after the hard stop
    /// kicks in for the downward direction, a reverse-direction event
    /// must still get through. Otherwise a user correcting an
    /// overshoot would feel like the trackpad froze.
    #[test]
    fn dampen_scroll_step_reverse_admits_after_hard_stop() {
        let mut m = build_model();
        let base = std::time::Instant::now();
        for i in 0..20 {
            let _ = m.dampen_scroll_step_at(false, base + MOMENTUM_GAP * i);
        }
        assert_eq!(m.dampen_scroll_step_at(true, base + MOMENTUM_GAP * 20), 5);
    }

    /// A long idle resets a saturated burst: after a momentum stream
    /// has decayed to the hard stop, the next event past the pause is
    /// a fresh gesture at full step.
    #[test]
    fn dampen_scroll_step_resets_after_idle() {
        let mut m = build_model();
        let base = std::time::Instant::now();
        for i in 0..15 {
            let _ = m.dampen_scroll_step_at(false, base + MOMENTUM_GAP * i);
        }
        let after_idle = base + MOMENTUM_GAP * 14 + std::time::Duration::from_millis(300);
        assert_eq!(m.dampen_scroll_step_at(false, after_idle), 5);
    }

    /// Returning to the terminal pane with a single click restores the
    /// ability to interact in one click (#103). Before the fix, the
    /// first click after leaving the terminal only refocused it —
    /// `claim_for_selection` was gated on the OLD focus, so the click
    /// never registered inside the pane and a redundant second click
    /// was needed before typing/selection worked.
    #[test]
    fn single_click_back_into_terminal_claims_the_click() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use tuirealm::ratatui::layout::Rect;

        let mut m = build_model();
        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _right_top, right_bottom) = crate::realm::layout::pane_areas(
            area,
            m.layout.sidebar_pct,
            m.layout.right_top_pct,
            m.layout.sidebar_user_resized,
        );

        // Start as if the user had been typing in the terminal.
        m.focus = PaneFocus::Terminals;

        let down = |col, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // Click the sidebar — focus leaves the terminal.
        m.dispatch_mouse_in(down(sidebar_rect.x + 1, sidebar_rect.y + 1), area);
        assert_eq!(m.focus, PaneFocus::Sidebar);

        // A single click back into the terminal pane must BOTH refocus
        // it AND claim the click for the pane (selection start) so the
        // Up handler can deliver it to the inner program — no redundant
        // second click.
        m.terminal_selection = None;
        m.dispatch_mouse_in(down(right_bottom.x + 2, right_bottom.y + 2), area);
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert!(
            m.terminal_selection.is_some(),
            "first click back into the terminal must claim the click, not just refocus",
        );
    }

    /// Adopt picker: source + target workspace keys flow into an
    /// `AdoptSessions` command. The picks index resolves into the
    /// `adopt_choices` slot we set up.
    #[test]
    fn choice_picked_for_adopt_target_returns_adopt_sessions() {
        let mut m = build_model();
        let source = WorkspaceKey::new("github:o/r#1");
        let target = WorkspaceKey::new("github:o/r#2");
        m.pending_adopt_source = Some(source.clone());
        m.adopt_choices = vec![target.clone()];
        m.modal_stack.push(Id::AdoptTarget);
        let cmds = m.handle_choice_picked(vec![0]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::AdoptSessions {
                source_workspace_key,
                target_workspace_key,
            } => {
                assert_eq!(source_workspace_key, &source);
                assert_eq!(target_workspace_key, &target);
            }
            other => panic!("expected AdoptSessions, got {other:?}"),
        }
        // Side state: the adoption slot + choice list both clear.
        assert!(m.pending_adopt_source.is_none());
        assert!(m.adopt_choices.is_empty());
    }

    /// `Id::RequestReviewers` picker: selecting two indices into
    /// `review_choices` produces `Command::RequestReviewers` with
    /// those logins resolved + the workspace key from
    /// `pending_review_request`. (Migrated from the older Input
    /// modal — see `mount_request_reviewers`.)
    #[test]
    fn choice_picked_on_request_reviewers_modal_returns_request_reviewers_cmd() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#1");
        m.pending_review_request = Some(ws_key.clone());
        m.review_choices = vec!["alice".into(), "bob".into(), "carol".into()];
        m.modal_stack.push(Id::RequestReviewers);
        let cmds = m.handle_choice_picked(vec![0, 2]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::RequestReviewers {
                workspace_key,
                logins,
            } => {
                assert_eq!(workspace_key, &ws_key);
                assert_eq!(logins, &vec!["alice".to_string(), "carol".to_string()]);
            }
            other => panic!("expected RequestReviewers, got {other:?}"),
        }
        assert!(m.pending_review_request.is_none());
        assert!(m.review_choices.is_empty());
    }

    /// `Id::AddAssignees` picker now fires `SetAssignees` (not Add)
    /// so the daemon can diff against the current task and run both
    /// add + remove mutations as needed. The picked indices are the
    /// *full desired set*, not deltas.
    #[test]
    fn choice_picked_on_add_assignees_modal_returns_set_assignees_cmd() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#5");
        m.pending_assignees_request = Some(ws_key.clone());
        m.assignees_choices = vec!["alice".into(), "bob".into()];
        m.modal_stack.push(Id::AddAssignees);
        let cmds = m.handle_choice_picked(vec![1]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::SetAssignees {
                workspace_key,
                logins,
            } => {
                assert_eq!(workspace_key, &ws_key);
                assert_eq!(logins, &vec!["bob".to_string()]);
            }
            other => panic!("expected SetAssignees, got {other:?}"),
        }
    }

    /// Empty pick on the assignees picker is meaningful — it clears
    /// every assignee. Fire SetAssignees with an empty Vec so the
    /// daemon can remove them all. (Distinct from the reviewers
    /// picker, where empty pick is treated as cancel.)
    #[test]
    fn choice_picked_on_add_assignees_with_empty_picks_clears_assignees() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#7");
        m.pending_assignees_request = Some(ws_key.clone());
        m.assignees_choices = vec!["alice".into()];
        m.modal_stack.push(Id::AddAssignees);
        let cmds = m.handle_choice_picked(vec![]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::SetAssignees {
                workspace_key,
                logins,
            } => {
                assert_eq!(workspace_key, &ws_key);
                assert!(logins.is_empty(), "empty pick clears assignees");
            }
            other => panic!("expected SetAssignees, got {other:?}"),
        }
    }

    /// Empty pick (Esc — defensive) drops the slot without firing.
    #[test]
    fn choice_picked_on_request_reviewers_with_empty_picks_returns_no_commands() {
        let mut m = build_model();
        m.pending_review_request = Some(WorkspaceKey::new("github:o/r#1"));
        m.review_choices = vec!["alice".into()];
        m.modal_stack.push(Id::RequestReviewers);
        let cmds = m.handle_choice_picked(vec![]);
        assert!(cmds.is_empty());
    }

    /// Helper: load a snippets collection from an inline YAML
    /// string via the tmpfile path. Lets per-test fixtures stay
    /// self-contained without each one re-deriving a tmp path.
    fn snippets_from_yaml(label: &str, yaml: &str) -> pilot_config::Snippets {
        let tmp_dir = std::env::temp_dir().join(format!(
            "pilot-snippets-test-{}-{label}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tmp = tmp_dir.join("snippets.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        pilot_config::Snippets::load_from(&tmp, pilot_config::SnippetOrigin::Global).unwrap()
    }

    /// Snippet picker: picking a row with NO active terminal drops
    /// silently (the warning lands in the footer hint, not the
    /// command stream). The modal still pops + slot clears.
    #[test]
    fn choice_picked_on_snippet_picker_without_terminal_returns_no_commands() {
        let mut m = build_model();
        m.snippets = snippets_from_yaml(
            "no-terminal",
            r#"
snippets:
  rev:
    description: Review
    body: review body
"#,
        );
        // Stash the picker's view of "row 0 → key `rev`" directly.
        // The handler reads from `snippet_choices` to recover the
        // chosen key, then looks up the snippet via `self.snippets`.
        m.snippet_choices = vec!["rev".into()];
        m.modal_stack.push(Id::SnippetPicker);
        let cmds = m.handle_choice_picked(vec![0]);
        // No active terminal → no Write emitted. Snippet stash +
        // modal both clear regardless of dispatch outcome.
        assert!(cmds.is_empty(), "no command without an active terminal");
        assert!(m.snippet_choices.is_empty(), "snippet stash cleared");
        assert!(
            !matches!(m.modal_stack.last(), Some(Id::SnippetPicker)),
            "modal popped"
        );
    }

    /// apply_snippets seeds the model collection. Sanity check
    /// that the lookup path resolves.
    #[test]
    fn apply_snippets_makes_entries_visible_to_lookup() {
        let loaded = snippets_from_yaml(
            "apply",
            r#"
snippets:
  rev:
    description: Review the diff
    body: please review
"#,
        );
        let mut m = build_model();
        m.apply_snippets(loaded);
        assert!(!m.snippets.is_empty());
        assert_eq!(m.snippets.len(), 1);
        let rev = m.snippets.get("rev").expect("rev exists");
        assert_eq!(rev.description, "Review the diff");
        assert_eq!(rev.body, "please review");
    }

    /// mount_snippet_picker with an empty collection flashes a hint
    /// and refuses to mount — no Id::SnippetPicker on the stack.
    /// This is the "user typed `]<key>` but never configured any
    /// snippets" UX.
    #[test]
    fn mount_snippet_picker_with_empty_collection_skips_mount() {
        let mut m = build_model();
        m.mount_snippet_picker(String::new());
        assert!(
            !matches!(m.modal_stack.last(), Some(Id::SnippetPicker)),
            "empty snippet library shouldn't open a picker"
        );
        assert!(
            m.snippet_choices.is_empty(),
            "no snippets configured → no choice slot",
        );
    }

    /// mount_snippet_picker populates `snippet_choices` with the
    /// picker's row keys, in the same order the picker rendered
    /// them (alphabetical via the underlying BTreeMap). This is
    /// the contract `handle_choice_picked` relies on.
    #[test]
    fn mount_snippet_picker_stashes_keys_in_render_order() {
        let mut m = build_model();
        m.apply_snippets(snippets_from_yaml(
            "render-order",
            r#"
snippets:
  zeta:
    description: last
    body: z
  alpha:
    description: first
    body: a
"#,
        ));
        m.mount_snippet_picker(String::new());
        assert!(matches!(m.modal_stack.last(), Some(Id::SnippetPicker)));
        assert_eq!(m.snippet_choices, vec!["alpha".to_string(), "zeta".into()]);
    }
}

#[cfg(test)]
mod input_starvation_tests {
    //! Regression: a chatty agent must NEVER block the keyboard.
    //!
    //! The daemon emits one `TerminalOutput` per PTY chunk into an
    //! *unbounded* channel. The run loop used to drain it with an
    //! unbounded `while let Ok(..)`, so under sustained agent output
    //! `try_recv` never returned `Empty`, the loop never reached the
    //! keyboard read, and the user "couldn't type in the agent" until
    //! the burst ended. `drain_daemon_events` now caps the work per
    //! iteration so control ALWAYS returns to the input read — input
    //! starvation is impossible by construction. These tests freeze
    //! that bound.
    use super::super::Model;
    use super::super::helpers::{MAX_EVENTS_PER_TICK, drain_daemon_events};
    use pilot_ipc::{Client, EVENT_CHANNEL_CAPACITY, Event, TerminalId};
    use tokio::sync::mpsc;
    use tuirealm::ratatui::layout::Size;

    /// Build a `Model` wired to a bounded inbound event channel we can
    /// fill directly — the same bounded channel the real transport
    /// hands the TUI ([`pilot_ipc::EVENT_CHANNEL_CAPACITY`]), minus the
    /// daemon-side forwarder. Returns the sender so the test floods it
    /// itself. The command channel's receiver is held alive so the
    /// model's `send` calls don't observe a closed channel.
    fn model_with_event_sender() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        mpsc::Sender<Event>,
        mpsc::UnboundedReceiver<pilot_ipc::Command>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = Client::from_channels(cmd_tx, evt_rx);
        let model = Model::<tuirealm::terminal::TestTerminalAdapter>::new_for_test(
            client,
            Size::new(120, 40),
        )
        .expect("model init");
        (model, evt_tx, cmd_rx)
    }

    fn flood(tx: &mpsc::Sender<Event>, n: usize) {
        for seq in 0..n {
            tx.try_send(Event::TerminalOutput {
                terminal_id: TerminalId(1),
                bytes: b"streaming output chunk\n".to_vec(),
                seq: seq as u64,
            })
            .expect("bounded channel must have room for the flood");
        }
    }

    /// A single drain processes AT MOST one tick's worth of events and
    /// reports a backlog, leaving the rest queued — proof the loop
    /// falls through to the keyboard read instead of spinning on
    /// output forever.
    #[test]
    fn flood_does_not_drain_everything_in_one_tick() {
        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();

        // Keep the flood under the bounded channel's capacity so the
        // test exercises the per-tick *drain* cap, not the channel's
        // overflow path (that's covered by the forwarder's own tests).
        let flooded = MAX_EVENTS_PER_TICK * 4;
        assert!(flooded < EVENT_CHANNEL_CAPACITY);
        flood(&evt_tx, flooded);

        // One iteration's drain: must report a backlog (more queued)…
        assert!(
            drain_daemon_events(&mut m),
            "drain should signal a backlog when the channel is over the cap"
        );
        // …and must have left events behind (didn't drain everything).
        assert!(
            m.client.rx.try_recv().is_ok(),
            "events must remain queued after one bounded drain — \
             otherwise the keyboard read is starved"
        );
    }

    /// Repeated drains eventually empty the channel and report no
    /// backlog — the cap throttles per-tick, it doesn't drop events.
    #[test]
    fn repeated_drains_eventually_empty_the_channel() {
        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();

        let flooded = MAX_EVENTS_PER_TICK * 4;
        flood(&evt_tx, flooded);

        // Bound the loop generously above the minimum needed (4) so a
        // genuinely stuck drain trips the assert instead of hanging.
        let mut backlog = true;
        let mut iterations = 0;
        while backlog {
            backlog = drain_daemon_events(&mut m);
            iterations += 1;
            assert!(iterations <= 64, "drain never converged — possible spin");
        }
        // Channel fully consumed, no event left behind.
        assert!(m.client.rx.try_recv().is_err());
    }

    /// A `TerminalResync` — the daemon's signal that it dropped output
    /// on a full channel and rebuilt the grid from the ring — is
    /// counted by the BacklogMonitor so overflow episodes are
    /// observable in the log (acceptance criterion from #88).
    #[test]
    fn resync_events_are_recorded_by_backlog_monitor() {
        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();

        for _ in 0..3 {
            evt_tx
                .try_send(Event::TerminalResync {
                    terminal_id: TerminalId(1),
                    replay: b"hello".to_vec(),
                    seq: 7,
                })
                .expect("room for resync");
        }
        drain_daemon_events(&mut m);
        assert_eq!(m.event_backlog.resyncs(), 3);
    }
}

#[cfg(test)]
mod coalesce_tests {
    //! `coalesce_adjacent_output` collapses a streaming burst into one
    //! dispatch per terminal — this is what keeps memory bounded under
    //! a chatty agent. The merge must be byte-for-byte faithful and
    //! must NOT reorder across terminals or non-output events.
    use super::super::helpers::coalesce_adjacent_output;
    use pilot_ipc::{Event, TerminalId};

    fn out(id: u64, bytes: &[u8], seq: u64) -> Event {
        Event::TerminalOutput {
            terminal_id: TerminalId(id),
            bytes: bytes.to_vec(),
            seq,
        }
    }

    /// A run of same-terminal output merges into ONE event carrying
    /// the concatenated bytes and the LAST chunk's seq.
    #[test]
    fn adjacent_same_terminal_runs_merge_with_last_seq() {
        let input = vec![out(1, b"hel", 10), out(1, b"lo ", 11), out(1, b"world", 12)];
        let merged = coalesce_adjacent_output(input);
        assert_eq!(merged.len(), 1);
        match &merged[0] {
            Event::TerminalOutput {
                terminal_id,
                bytes,
                seq,
            } => {
                assert_eq!(*terminal_id, TerminalId(1));
                assert_eq!(bytes, b"hello world");
                assert_eq!(*seq, 12, "merged event carries the last chunk's seq");
            }
            other => panic!("expected one TerminalOutput, got {other:?}"),
        }
    }

    /// Output for a different terminal ends the run — no cross-terminal
    /// merging, and order is preserved.
    #[test]
    fn different_terminals_do_not_merge() {
        let input = vec![out(1, b"a", 1), out(2, b"b", 1), out(1, b"c", 2)];
        let merged = coalesce_adjacent_output(input);
        assert_eq!(merged.len(), 3, "no merge across terminals");
        // Order preserved: t1, t2, t1.
        let ids: Vec<u64> = merged
            .iter()
            .map(|e| match e {
                Event::TerminalOutput { terminal_id, .. } => terminal_id.0,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, vec![1, 2, 1]);
    }

    /// A non-output event between two same-terminal outputs breaks the
    /// run — ordering relative to other events is never disturbed.
    #[test]
    fn non_output_event_breaks_the_run() {
        let input = vec![
            out(1, b"before", 1),
            Event::TerminalExited {
                terminal_id: TerminalId(1),
                exit_code: Some(0),
            },
            out(1, b"after", 2),
        ];
        let merged = coalesce_adjacent_output(input);
        assert_eq!(merged.len(), 3, "the Exited event must not be absorbed");
        assert!(matches!(merged[1], Event::TerminalExited { .. }));
    }

    /// Empty input is a no-op.
    #[test]
    fn empty_input_yields_empty() {
        assert!(coalesce_adjacent_output(Vec::new()).is_empty());
    }
}

#[cfg(test)]
mod backlog_monitor_tests {
    //! The monitor is the leak detector: it watches the residual
    //! channel depth after each drain and only escalates when the
    //! backlog climbs to a new high — a steady stream of rising
    //! residuals is "the consumer is falling behind".
    use super::super::helpers::BacklogMonitor;

    /// A clear (residual 0) resets the consecutive-backlog streak.
    #[test]
    fn clearing_resets_the_streak() {
        let mut m = BacklogMonitor::default();
        m.observe(50);
        m.observe(80);
        assert_eq!(m.consecutive_backlog_ticks(), 2);
        m.observe(0);
        assert_eq!(m.consecutive_backlog_ticks(), 0, "streak resets on clear");
    }

    /// A backlog that climbs tick-over-tick raises the streak and the
    /// high-water mark — the signal a leak detector keys on.
    #[test]
    fn growing_backlog_tracks_streak_and_hwm() {
        let mut m = BacklogMonitor::default();
        for depth in [200usize, 700, 1500, 4000] {
            m.observe(depth);
        }
        assert_eq!(m.consecutive_backlog_ticks(), 4);
        assert_eq!(m.hwm(), 4000, "high-water mark tracks the worst depth");
    }

    /// The high-water mark never regresses when depth dips but stays
    /// non-zero — a transient dip isn't "recovered".
    #[test]
    fn hwm_is_monotonic_across_dips() {
        let mut m = BacklogMonitor::default();
        m.observe(3000);
        m.observe(100);
        assert_eq!(m.hwm(), 3000);
        assert_eq!(
            m.consecutive_backlog_ticks(),
            2,
            "still backlogged, no clear"
        );
    }
}

#[cfg(test)]
mod subscribed_projects_tests {
    //! `refresh_subscribed_projects` add/remove contract — the
    //! placeholder headers pilot synthesizes for narrowed repo
    //! subscriptions before the daemon surfaces a workspace.
    use super::super::*;
    use pilot_core::{PersistedSetup, Project, ProjectKey};
    use pilot_ipc::{Event as IpcEvent, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn persisted_with_scopes(scopes: &[&str]) -> PersistedSetup {
        let mut set = std::collections::BTreeSet::new();
        for s in scopes {
            set.insert((*s).to_string());
        }
        let mut selected_scopes = std::collections::BTreeMap::new();
        selected_scopes.insert("github".to_string(), set);
        PersistedSetup {
            selected_scopes,
            ..Default::default()
        }
    }

    /// Subscribing to a narrowed repo synthesizes a placeholder
    /// header; unsubscribing it removes the header again.
    #[test]
    fn unsubscribing_a_repo_drops_its_placeholder() {
        let mut m = build_model();
        let pk = ProjectKey::github("acme", "widget");

        m.setup.persisted = Some(persisted_with_scopes(&["github:acme/widget"]));
        m.refresh_subscribed_projects();
        assert!(m.projects.contains_key(&pk), "placeholder should appear");

        // User removes the repo scope.
        m.setup.persisted = Some(persisted_with_scopes(&[]));
        m.refresh_subscribed_projects();
        assert!(
            !m.projects.contains_key(&pk),
            "placeholder should be removed once unsubscribed"
        );
    }

    /// A daemon `ProjectUpserted` promotes the placeholder to an
    /// authoritative record; a subsequent scope removal must NOT yank
    /// it client-side — the daemon owns its lifecycle now.
    #[test]
    fn promoted_project_survives_scope_removal() {
        let mut m = build_model();
        let pk = ProjectKey::github("acme", "widget");

        m.setup.persisted = Some(persisted_with_scopes(&["github:acme/widget"]));
        m.refresh_subscribed_projects();

        // Daemon finds a workspace → authoritative upsert.
        m.handle_daemon_event(IpcEvent::ProjectUpserted(Box::new(Project::new(
            pk.clone(),
            "acme/widget",
            chrono::Utc::now(),
        ))));

        // Scope removed, but the daemon-owned project stays put until
        // the daemon broadcasts its own ProjectRemoved.
        m.setup.persisted = Some(persisted_with_scopes(&[]));
        m.refresh_subscribed_projects();
        assert!(
            m.projects.contains_key(&pk),
            "daemon-authoritative project must not be dropped by a scope edit"
        );
    }

    /// Whole-org subscriptions never synthesize a placeholder, so
    /// org-discovered projects are left untouched by a refresh.
    #[test]
    fn org_level_scope_leaves_discovered_projects_alone() {
        let mut m = build_model();
        let discovered = ProjectKey::github("acme", "found-by-polling");
        m.projects.insert(
            discovered.clone(),
            Project::new(
                discovered.clone(),
                "acme/found-by-polling",
                chrono::Utc::now(),
            ),
        );

        m.setup.persisted = Some(persisted_with_scopes(&["github:acme"]));
        m.refresh_subscribed_projects();
        assert!(
            m.projects.contains_key(&discovered),
            "whole-org discovered project must survive refresh"
        );
    }
}

#[cfg(test)]
mod base64_tests {
    use super::super::helpers::base64_encode;

    #[test]
    fn rfc4648_test_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}

#[cfg(test)]
mod modal_input_responsiveness_tests {
    //! Regression for #90: the out-of-scope Confirm modal froze the
    //! app during sync. The dispatcher used to forward each modal key
    //! to the listener channel and then busy-wait up to 150ms for the
    //! reply, blocking daemon-event draining and rendering on every
    //! keystroke. Forwarding must now return immediately and arm a
    //! redraw window so even no-`Msg` keys (Confirm arrows, Input
    //! typing) still repaint.
    use super::super::Id;
    use super::super::Model;
    use pilot_core::WorkspaceKey;
    use pilot_ipc::{Event as IpcEvent, channel};
    use tuirealm::event::{Event as RealmEvent, Key, KeyEvent, KeyModifiers};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn mount_out_of_scope_confirm(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) {
        m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            label: "o/r#1".into(),
            title: None,
            active_terminal_count: 1,
        });
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    }

    /// Forwarding a modal key returns immediately (no 150ms busy-wait)
    /// and arms the redraw window so the run loop repaints the modal
    /// even when the key produces no `Msg`.
    #[test]
    fn forwarding_a_modal_key_is_nonblocking_and_arms_redraw() {
        let mut m = build_model();
        mount_out_of_scope_confirm(&mut m);
        assert!(
            !m.modal_redraw_pending(),
            "no redraw window before any modal key is forwarded",
        );

        // Left arrow toggles the Confirm's highlight — a key that
        // mutates the modal WITHOUT emitting a Msg, the case the old
        // forced `redraw = true` covered.
        let t = std::time::Instant::now();
        m.forward_modal_event(RealmEvent::Keyboard(key(Key::Left)));
        assert!(
            t.elapsed() < std::time::Duration::from_millis(50),
            "forwarding must not block the dispatcher (old code waited 150ms/key)",
        );
        assert!(
            m.modal_redraw_pending(),
            "a redraw window must be armed so the no-Msg key still repaints",
        );
        // The toggle must not have dismissed the modal.
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    }

    /// Shift-D opens the read-only sync-status window, and a
    /// non-navigation key inside it pops it back off. Exercises the
    /// catalog → dispatch → mount wiring end to end.
    #[test]
    fn shift_d_opens_and_closes_sync_status_window() {
        let mut m = build_model();
        // Seed one success + one failure so the window has content.
        m.handle_daemon_event(IpcEvent::PollCompleted {
            source: "github".into(),
            count: 7,
        });
        m.handle_daemon_event(IpcEvent::ProviderError {
            source: "github".into(),
            message: "rate limit exceeded".into(),
            detail: "403 from api.github.com".into(),
            kind: "retryable".into(),
        });

        assert!(m.top_modal().is_none(), "no modal before Shift-D");
        m.dispatch_key(KeyEvent::new(Key::Char('D'), KeyModifiers::SHIFT));
        assert_eq!(m.top_modal(), Some(&Id::SyncStatus));

        // Esc (a non-navigation key) dismisses it via the modal pipeline.
        m.dispatch_modal_key(key(Key::Esc));
        assert!(m.top_modal().is_none(), "Esc closes the sync-status window",);
    }

    /// The redraw window is one-shot per keystroke window: once its
    /// deadline elapses, `modal_redraw_pending` reports false and clears
    /// itself so an idle modal stops re-rendering.
    #[test]
    fn redraw_window_clears_after_it_elapses() {
        let mut m = build_model();
        mount_out_of_scope_confirm(&mut m);
        m.forward_modal_event(RealmEvent::Keyboard(key(Key::Left)));
        assert!(m.modal_redraw_pending());
        // The window is well under a second; wait it out and confirm
        // the loop would stop forcing redraws.
        std::thread::sleep(std::time::Duration::from_millis(160));
        assert!(
            !m.modal_redraw_pending(),
            "an elapsed redraw window must clear so an idle modal isn't redrawn forever",
        );
    }
}
