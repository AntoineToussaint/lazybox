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

    /// Helper for the scroll damper tests: build a dummy mouse
    /// event. The damper only inspects `is_up` (passed separately)
    /// + the timestamps it reads via `Instant::now()`, so the
    /// per-event mouse data doesn't matter.
    fn dummy_wheel() -> crossterm::event::MouseEvent {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Fresh gesture (no prior scroll) returns the full STEP.
    #[test]
    fn dampen_scroll_step_fresh_gesture_returns_initial_step() {
        let mut m = build_model();
        assert_eq!(m.dampen_scroll_step(false, dummy_wheel()), 5);
    }

    /// Sustained same-direction burst decays the step. First 4
    /// events stay at 5, events 5-14 drop to 3, event 15+ → 1.
    #[test]
    fn dampen_scroll_step_decays_within_sustained_burst() {
        let mut m = build_model();
        for _ in 0..4 {
            assert_eq!(m.dampen_scroll_step(false, dummy_wheel()), 5);
        }
        // Event 5 onwards: mid step.
        assert_eq!(m.dampen_scroll_step(false, dummy_wheel()), 3);
        // Bump count up to 14 (still mid).
        for _ in 0..9 {
            assert_eq!(m.dampen_scroll_step(false, dummy_wheel()), 3);
        }
        // Event 15+: tail step.
        assert_eq!(m.dampen_scroll_step(false, dummy_wheel()), 1);
    }

    /// Direction reversal mid-burst returns 0 (drop the event) AND
    /// clears the burst, so the next event in the new direction
    /// starts fresh at full STEP. This is what cancels macOS's
    /// queued inertia from the prior gesture.
    #[test]
    fn dampen_scroll_step_direction_reversal_drops_event_and_resets_burst() {
        let mut m = build_model();
        // Build up a downward burst.
        for _ in 0..6 {
            let _ = m.dampen_scroll_step(false, dummy_wheel());
        }
        // Reverse: drop + reset.
        assert_eq!(m.dampen_scroll_step(true, dummy_wheel()), 0);
        // Next upward event: fresh burst → full step.
        assert_eq!(m.dampen_scroll_step(true, dummy_wheel()), 5);
    }

    /// After `BURST_IDLE` of inactivity, the next event is treated
    /// as a fresh gesture. We can't time-travel without injecting a
    /// clock, but we can prove the freshness path indirectly: a
    /// burst built up then explicitly cleared (None) returns to
    /// full step on the next event.
    #[test]
    fn dampen_scroll_step_after_explicit_clear_starts_fresh() {
        let mut m = build_model();
        for _ in 0..10 {
            let _ = m.dampen_scroll_step(false, dummy_wheel());
        }
        m.scroll_inertia = None;
        assert_eq!(m.dampen_scroll_step(false, dummy_wheel()), 5);
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
