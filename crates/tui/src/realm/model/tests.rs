// Tests may block (sleeps to cross latch windows, thread joins); the
// crate-wide blocking-call ban in clippy.toml targets the run loop.
#![allow(clippy::disallowed_methods)]

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
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::channel;
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
        use lazybox_ipc::Event as IpcEvent;

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

    /// Connecting to a daemon built from a different commit raises a
    /// sticky banner naming both builds — the stale-daemon skew the
    /// protocol handshake can't see. A matching build stays silent.
    #[test]
    fn daemon_build_mismatch_raises_sticky_banner() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();

        m.note_daemon_build(lazybox_ipc::BUILD_VERSION);
        assert!(
            m.status.notice.is_none(),
            "a matching daemon build must not raise a banner"
        );

        m.note_daemon_build("0.0.0+stale");
        let n = m.status.notice.as_ref().expect("mismatch banner set");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("build mismatch"));
        assert!(n.message.contains("0.0.0+stale"));
        assert!(n.message.contains(lazybox_ipc::BUILD_VERSION));
    }

    /// A build that trails `main` raises the persistent outdated-build
    /// warning: a sticky footer banner naming the fix *and* the sidebar
    /// flag the header repaints every frame. This is the uniformly-stale
    /// install the daemon/client mismatch check can't see (#234).
    #[test]
    fn outdated_build_raises_persistent_warning() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();

        m.note_outdated_build(89);

        let n = m.status.notice.as_ref().expect("outdated banner set");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("89"));
        assert!(n.message.contains("update & restart"));
        assert_eq!(m.sidebar.outdated_commits_behind(), Some(89));
    }

    /// A manual-refresh sync failure paints a sticky "✗ sync failed"
    /// banner; the next successful poll (auto-cycle) from the *same*
    /// provider must clear it so a recovered sync doesn't leave the
    /// red notice up forever.
    #[test]
    fn provider_error_banner_clears_on_next_successful_poll() {
        use lazybox_ipc::Event as IpcEvent;

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
    /// poll from a *different* provider (lazybox polls GitHub, Linear and
    /// Slack concurrently) must NOT erase a still-valid failure banner.
    #[test]
    fn provider_error_banner_survives_other_providers_poll() {
        use lazybox_ipc::Event as IpcEvent;

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
        use lazybox_ipc::Event as IpcEvent;

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
    /// project_key, spawn_agent }`. `spawn_agent` carries the
    /// configured default agent so creating a workspace lands the
    /// user straight in a live session. Without a stashed project_key
    /// the submit drops (see `mount_new_workspace_input` — the catalog
    /// `n` flow only mounts when a project is focused).
    #[test]
    fn input_submitted_for_new_workspace_returns_create_workspace() {
        let mut m = build_model();
        let pk = lazybox_core::ProjectKey::local("my-project");
        m.modal_stack.push(Id::NewWorkspace);
        m.pending_new_workspace_project = Some(pk.clone());
        let cmds = m.handle_input_submitted("  my-feature  ".into());
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::CreateWorkspace {
                name,
                project_key,
                spawn_agent,
            } => {
                assert_eq!(name, "my-feature");
                assert_eq!(project_key, &pk);
                // Default agent is "claude" unless YAML overrides it.
                assert_eq!(spawn_agent.as_deref(), Some("claude"));
            }
            other => panic!("expected CreateWorkspace, got {other:?}"),
        }
    }

    /// `Shift-W` with no projects yet can't resolve a container, so
    /// it surfaces a nudge instead of mounting a picker.
    #[test]
    fn start_agent_flow_without_projects_mounts_no_modal() {
        let mut m = build_model();
        m.start_agent_flow();
        assert!(
            m.modal_stack.is_empty(),
            "no project → footer nudge, no modal"
        );
    }

    /// Picking a project in the `Shift-W` start-agent picker funnels
    /// into the new-workspace name input (which then auto-spawns the
    /// default agent on submit). The pick itself sends no IPC and
    /// drains the stashed choices.
    #[test]
    fn start_agent_project_pick_funnels_into_new_workspace_input() {
        let mut m = build_model();
        let pk = lazybox_core::ProjectKey::local("proj");
        m.start_agent_project_choices = vec![pk.clone()];
        m.modal_stack.push(Id::StartAgentProject);
        let cmds = m.handle_choice_picked(vec![0]);
        assert!(cmds.is_empty(), "picking a project sends no IPC yet");
        assert_eq!(m.modal_stack.last(), Some(&Id::NewWorkspace));
        assert_eq!(m.pending_new_workspace_project.as_ref(), Some(&pk));
        assert!(
            m.start_agent_project_choices.is_empty(),
            "choices drained after pick"
        );
    }

    /// `Shift-N` with no tracked repos has nothing to pick, so it
    /// skips the picker and drops straight into the new-project input
    /// — the only way to bootstrap a brand-new, empty inbox.
    #[test]
    fn new_workspace_picker_without_projects_mounts_new_project_input() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        m.dispatch_action(&Action::NewProject);
        assert_eq!(m.modal_stack.last(), Some(&Id::NewProject));
    }

    /// `Shift-N` with tracked repos mounts the repo picker, listing
    /// each repo plus the trailing "create a new local project" row.
    #[test]
    fn new_workspace_picker_with_projects_mounts_repo_picker() {
        use lazybox_ipc::Event as IpcEvent;
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let pk = lazybox_core::ProjectKey::github("acme", "widget");
        m.handle_daemon_event(IpcEvent::ProjectUpserted(Box::new(
            lazybox_core::Project::new(pk.clone(), "acme/widget", chrono::Utc::now()),
        )));
        m.dispatch_action(&Action::NewProject);
        assert_eq!(m.modal_stack.last(), Some(&Id::NewWorkspaceRepo));
        assert_eq!(m.new_workspace_repo_choices, vec![pk]);
    }

    /// Picking a repo row funnels into the new-workspace name input
    /// under that repo (no project-creation step). The pick sends no
    /// IPC and drains the stashed choices.
    #[test]
    fn new_workspace_repo_pick_funnels_into_name_input() {
        let mut m = build_model();
        let pk = lazybox_core::ProjectKey::github("acme", "widget");
        m.new_workspace_repo_choices = vec![pk.clone()];
        m.modal_stack.push(Id::NewWorkspaceRepo);
        let cmds = m.handle_choice_picked(vec![0]);
        assert!(cmds.is_empty(), "picking a repo sends no IPC yet");
        assert_eq!(m.modal_stack.last(), Some(&Id::NewWorkspace));
        assert_eq!(m.pending_new_workspace_project.as_ref(), Some(&pk));
        assert!(
            m.new_workspace_repo_choices.is_empty(),
            "choices drained after pick"
        );
    }

    /// Picking the trailing escape-hatch row (index past the repo
    /// list) keeps the brand-new-project path available.
    #[test]
    fn new_workspace_repo_pick_escape_hatch_mounts_new_project() {
        let mut m = build_model();
        let pk = lazybox_core::ProjectKey::github("acme", "widget");
        m.new_workspace_repo_choices = vec![pk];
        m.modal_stack.push(Id::NewWorkspaceRepo);
        // Index 1 is the "create a new local project" row (the single
        // repo occupies index 0).
        let cmds = m.handle_choice_picked(vec![1]);
        assert!(cmds.is_empty());
        assert_eq!(m.modal_stack.last(), Some(&Id::NewProject));
        assert!(m.new_workspace_repo_choices.is_empty());
    }

    /// The "Configure LLM gateway" settings action routes straight to
    /// the single global URL input — no provider picker, no wizard
    /// runner. Freezes that routing (a regression that dropped the early
    /// return would fall through to the cached-inputs wizard path and
    /// warn instead of mounting). Disk-free: mounting only reads config
    /// for the pre-fill; nothing is saved.
    #[test]
    fn edit_llm_gateway_action_mounts_the_url_input() {
        use crate::realm::setup_ctx::SettingsAction;
        let mut m = build_model();
        m.dispatch_settings_action(SettingsAction::EditLlmGateway { set: false });
        assert_eq!(m.modal_stack.last(), Some(&Id::LlmGatewayUrl));
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
        m.active_removal_prompt = Some((ws_key.clone(), super::super::RemovalReason::OutOfScope));
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

    /// `y` on a merged-PR removal confirm produces
    /// `RemoveMergedWorkspace` (not `Kill`) — the merged path also
    /// deletes the worktree on the daemon side.
    #[test]
    fn confirmed_yes_on_merged_removal_returns_remove_merged_workspace() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#1");
        m.active_removal_prompt = Some((ws_key.clone(), super::super::RemovalReason::Merged));
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_confirmed(true);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::RemoveMergedWorkspace { session_key } => {
                assert_eq!(session_key, &SessionKey::from(&ws_key));
            }
            other => panic!("expected RemoveMergedWorkspace, got {other:?}"),
        }
    }

    /// A `MergedPrRemovable` event mounts the removal confirm (reason
    /// `Merged`), and a re-emit for the same workspace doesn't stack a
    /// second prompt — the daemon's one-shot transition plus this
    /// dedupe keep it to a single ask.
    #[test]
    fn merged_pr_removable_mounts_confirm_and_dedupes() {
        use lazybox_ipc::Event as IpcEvent;
        let mut m = build_model();
        let ev = || IpcEvent::MergedPrRemovable {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            label: "o/r#1".into(),
            active_terminal_count: 0,
            has_local_work: false,
        };
        m.handle_daemon_event(ev());
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
        assert!(matches!(
            m.active_removal_prompt,
            Some((_, super::super::RemovalReason::Merged))
        ));

        m.handle_daemon_event(ev());
        assert!(
            m.pending_removal_prompts.is_empty(),
            "re-emit must not stack a second prompt"
        );
    }

    /// `n` on RemoveOutOfScope clears the slot without producing
    /// a Kill — user said no, daemon doesn't need to hear about it.
    #[test]
    fn confirmed_no_on_remove_out_of_scope_returns_no_commands() {
        let mut m = build_model();
        m.active_removal_prompt = Some((
            WorkspaceKey::new("github:o/r#1"),
            super::super::RemovalReason::OutOfScope,
        ));
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
        m.active_removal_prompt = Some((
            WorkspaceKey::new("github:o/r#1"),
            super::super::RemovalReason::OutOfScope,
        ));
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

    /// Past `STOP_AT` (event 40) a momentum stream returns 0, killing
    /// the OS momentum tail so the view actually stops instead of
    /// trickling onward at STEP=1 for the full 1–2 s tail.
    #[test]
    fn dampen_scroll_step_momentum_tail_hard_stops_past_stop_at() {
        let mut m = build_model();
        let base = std::time::Instant::now();
        // Saturate the burst (39 events still admit at TAIL=1).
        for i in 0..39 {
            assert_ne!(
                m.dampen_scroll_step_at(false, base + MOMENTUM_GAP * i),
                0,
                "event {i} is inside the burst budget and must still admit",
            );
        }
        // Event 40 onwards: dropped.
        for i in 39..60 {
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
    fn snippets_from_yaml(label: &str, yaml: &str) -> lazybox_config::Snippets {
        let tmp_dir = std::env::temp_dir().join(format!(
            "lazybox-snippets-test-{}-{label}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tmp = tmp_dir.join("snippets.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        lazybox_config::Snippets::load_from(&tmp, lazybox_config::SnippetOrigin::Global).unwrap()
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

    /// Build a model with workspace `github:o/r#1` selected and a
    /// single live terminal of `kind` on screen, its snippet library
    /// loaded, and the picker primed to resolve row 0 → `snippet_key`.
    /// This is the exact pre-submit state BOTH snippet trigger paths
    /// (the `]]<key>` auto-submit and the picker's Enter) funnel into
    /// `handle_choice_picked`.
    fn model_with_active_terminal_and_snippet(
        label: &str,
        snippets_yaml: &str,
        snippet_key: &str,
        kind: lazybox_ipc::TerminalKind,
    ) -> Model<tuirealm::terminal::TestTerminalAdapter> {
        use lazybox_ipc::{Event as IpcEvent, TerminalId};
        let mut m = build_model();
        m.snippets = snippets_from_yaml(label, snippets_yaml);
        let ws_key = WorkspaceKey::new("github:o/r#1");
        let session_key: SessionKey = (&ws_key).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![lazybox_core::Workspace::empty(
                ws_key,
                "main",
                chrono::Utc::now(),
            )],
            terminals: vec![],
            projects: vec![],
        });
        assert!(
            m.sidebar.focus_workspace_key(&session_key),
            "seeded workspace should be selectable",
        );
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(1),
            session_key,
            kind,
            no_permission: false,
        });
        assert_eq!(
            m.terminals.active_terminal_id(),
            Some(TerminalId(1)),
            "the spawned terminal must be on screen",
        );
        m.snippet_choices = vec![snippet_key.to_string()];
        m.modal_stack.push(Id::SnippetPicker);
        m
    }

    /// Picking a snippet while an AGENT terminal is on screen routes
    /// through `InjectPrompt` — the daemon's settle-gated paste+submit
    /// path — carrying the body verbatim, NOT a raw `Write` with a
    /// crammed trailing `\r`. That split is what makes the submit land
    /// reliably across agents that debounce a pasted burst (#246).
    #[test]
    fn snippet_into_agent_terminal_routes_through_inject_prompt() {
        let mut m = model_with_active_terminal_and_snippet(
            "agent-single",
            r#"
snippets:
  rev:
    description: Review
    body: review the diff
"#,
            "rev",
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        );
        let cmds = m.handle_choice_picked(vec![0]);
        match cmds
            .iter()
            .find(|c| matches!(c, IpcCommand::InjectPrompt { .. }))
        {
            Some(IpcCommand::InjectPrompt {
                terminal_id,
                prompt,
                fallback_spawn,
            }) => {
                assert_eq!(prompt, "review the diff", "body injected verbatim");
                assert_eq!(*terminal_id, lazybox_ipc::TerminalId(1));
                assert!(
                    fallback_spawn.is_none(),
                    "the terminal is live — no spawn fallback needed",
                );
            }
            _ => panic!("agent snippet must inject, got {cmds:?}"),
        }
        assert!(
            !cmds.iter().any(|c| matches!(c, IpcCommand::Write { .. })),
            "the agent path must not ALSO raw-write the body",
        );
        // The recap tracker still pins the snippet as the latest
        // "you ▸ …" message even though the daemon does the PTY write.
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                IpcCommand::RecordUserMessage { message, .. } if message == "review the diff"
            )),
            "snippet must be recorded as the latest user message, got {cmds:?}",
        );
    }

    /// A multi-line body injects verbatim too — the embedded newlines
    /// reach the agent as-is, and the reliable submit is the daemon's
    /// separate Enter, so nothing in the TUI has to pre-rewrite the
    /// body into a bracketed paste (that's the shell-only encoding).
    #[test]
    fn snippet_into_agent_terminal_injects_multiline_body_verbatim() {
        let mut m = model_with_active_terminal_and_snippet(
            "agent-multi",
            "\nsnippets:\n  pr:\n    description: PR\n    body: |\n      first line\n      second line\n",
            "pr",
            lazybox_ipc::TerminalKind::Agent("codex".into()),
        );
        let cmds = m.handle_choice_picked(vec![0]);
        match cmds
            .iter()
            .find(|c| matches!(c, IpcCommand::InjectPrompt { .. }))
        {
            Some(IpcCommand::InjectPrompt { prompt, .. }) => {
                // The `|` block scalar keeps its trailing newline — the
                // body reaches the agent exactly as authored.
                assert_eq!(
                    prompt, "first line\nsecond line\n",
                    "multi-line body verbatim"
                );
            }
            _ => panic!("agent snippet must inject, got {cmds:?}"),
        }
    }

    /// Picking a snippet while a plain SHELL is on screen writes the
    /// `encode_snippet_for_pty` bytes directly — a shell has no paste
    /// debounce, so `body + \r` submits cleanly and the inject path
    /// (which no-ops on non-agent terminals) is skipped.
    #[test]
    fn snippet_into_shell_terminal_writes_encoded_bytes() {
        let mut m = model_with_active_terminal_and_snippet(
            "shell",
            r#"
snippets:
  ls:
    description: List
    body: ls -la
"#,
            "ls",
            lazybox_ipc::TerminalKind::Shell,
        );
        let cmds = m.handle_choice_picked(vec![0]);
        let bytes = cmds
            .iter()
            .find_map(|c| match c {
                IpcCommand::Write { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .expect("shell snippet must raw-write the encoded body");
        assert_eq!(
            bytes,
            super::super::inputs::encode_snippet_for_pty("ls -la")
        );
        assert!(bytes.ends_with(b"\r"), "shell write ends in a submit CR");
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, IpcCommand::InjectPrompt { .. })),
            "the shell path must not use the agent inject path",
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

    // ── `]]` leader chord (issue #205) ──────────────────────────────

    use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};

    /// Press the default escape char (`]`) into `handle_pane_key`.
    fn esc_key() -> RealmKey {
        RealmKey::new(Key::Char(']'), RealmMods::NONE)
    }

    /// A model focused on the terminal pane with a one-snippet library
    /// loaded — the precondition for arming the leader. `label` keys
    /// the fixture's tmp file so parallel tests don't share one.
    fn model_in_terminal_with_snippets(
        label: &str,
    ) -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let mut m = build_model();
        m.apply_snippets(snippets_from_yaml(
            label,
            r#"
snippets:
  rev:
    description: Review
    body: review body
"#,
        ));
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m
    }

    /// `]` then `]` with a snippet library present arms the leader and
    /// keeps focus on the terminal — it does NOT leave immediately.
    #[test]
    fn double_bracket_arms_leader_when_snippets_present() {
        let mut m = model_in_terminal_with_snippets("leader-arm");
        m.dispatch_key(esc_key());
        assert!(!m.terminal_leader_pending(), "one `]` only holds");
        m.dispatch_key(esc_key());
        assert!(m.terminal_leader_pending(), "`]]` arms the leader");
        assert_eq!(m.focus(), PaneFocus::Terminals, "leader doesn't leave yet");
    }

    /// Even with no snippets configured the leader still has bindings to
    /// offer — `]]f` focus toggle and `]]<digit>` agent jumps — so `]]`
    /// arms the leader and keeps focus on the terminal; the pane only
    /// leaves on the idle tick if no follow key arrives (#156 follow-up,
    /// which replaced the old leave-immediately path).
    #[test]
    fn double_bracket_arms_leader_even_without_snippets() {
        let mut m = build_model();
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        assert!(m.terminal_leader_pending(), "`]]` arms the leader");
        assert_eq!(m.focus(), PaneFocus::Terminals, "leader doesn't leave yet");
    }

    /// `]]<printable>` opens the snippet picker pre-filtered by the
    /// follow-up char, and disarms the leader.
    #[test]
    fn leader_then_char_opens_snippet_picker() {
        let mut m = model_in_terminal_with_snippets("leader-char");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        m.dispatch_key(RealmKey::new(Key::Char('r'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert!(matches!(m.top_modal(), Some(Id::SnippetPicker)));
    }

    /// `]]` then `Esc` cancels the leader back into the terminal —
    /// focus stays, no picker mounts.
    #[test]
    fn leader_then_esc_cancels_back_to_terminal() {
        let mut m = model_in_terminal_with_snippets("leader-esc");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        m.dispatch_key(RealmKey::new(Key::Esc, RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert_eq!(m.focus(), PaneFocus::Terminals, "Esc cancels, stays put");
        assert!(m.top_modal().is_none(), "no picker mounted");
    }

    /// A lone `]` followed by a non-`]` key is a literal `]` in the
    /// user's input: it must NOT arm the leader or open a picker, even
    /// with snippets configured (the bug this issue fixes).
    #[test]
    fn single_bracket_then_other_key_passes_through() {
        let mut m = model_in_terminal_with_snippets("leader-literal");
        m.dispatch_key(esc_key());
        m.dispatch_key(RealmKey::new(Key::Char('a'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending());
        assert!(m.top_modal().is_none(), "lone `]a` opens no picker");
        assert_eq!(m.focus(), PaneFocus::Terminals);
    }

    /// An armed leader with no follow-up key leaves the pane once the
    /// escape window elapses (the idle tick). Uses a 1ms window so the
    /// test doesn't sleep the default 600ms.
    #[test]
    fn idle_leader_leaves_on_tick_after_window() {
        let mut m = model_in_terminal_with_snippets("leader-idle");
        m.ui_defaults.escape_window = std::time::Duration::from_millis(1);
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        assert!(m.terminal_leader_pending());
        std::thread::sleep(std::time::Duration::from_millis(3));
        m.tick_terminal_leader();
        assert!(!m.terminal_leader_pending(), "window elapsed → disarmed");
        assert_eq!(m.focus(), PaneFocus::Sidebar, "idle leader leaves the pane");
    }

    /// A single-line snippet body is sent raw plus a trailing `\r`.
    /// No bracketed-paste wrapper — the agent submits it directly.
    #[test]
    fn encode_snippet_single_line_is_raw_plus_cr() {
        let bytes = super::super::inputs::encode_snippet_for_pty("review the diff");
        assert_eq!(bytes, b"review the diff\r");
    }

    /// A multi-line body is wrapped in a bracketed-paste pair with
    /// embedded newlines rewritten to `\r`, and the submit `\r`
    /// placed *outside* the closing `ESC[201~`. Without the wrapper
    /// the agent's paste auto-detection swallows the trailing `\r`
    /// and never submits (issue #204).
    #[test]
    fn encode_snippet_multi_line_is_bracketed_paste_with_trailing_cr() {
        let bytes = super::super::inputs::encode_snippet_for_pty("first line\nsecond line");
        assert_eq!(bytes, b"\x1b[200~first line\rsecond line\x1b[201~\r");
        assert!(
            bytes.ends_with(b"\x1b[201~\r"),
            "submit CR must land after the close marker, not inside the paste"
        );
    }

    /// The invariant #246 hardens for the shell encoding: WHATEVER the
    /// body, the encoded bytes end in a submit `\r`, and that `\r` sits
    /// OUTSIDE any bracketed-paste wrapper — never buffered inside the
    /// paste window as a literal newline. Covers single-line,
    /// multi-line, empty, and a body that already ends in a newline.
    #[test]
    fn encode_snippet_always_ends_in_submit_cr_outside_paste() {
        for body in ["one line", "first\nsecond", "a\nb\nc", "", "trailing\n"] {
            let bytes = super::super::inputs::encode_snippet_for_pty(body);
            assert_eq!(
                bytes.last(),
                Some(&b'\r'),
                "body {body:?} must end in a submit CR",
            );
            // If the body was bracketed, the close marker must come
            // before the final CR — i.e. the submit is outside the
            // paste. A body with no wrapper trivially satisfies this.
            if bytes.windows(6).any(|w| w == b"\x1b[200~") {
                let close = bytes
                    .windows(6)
                    .rposition(|w| w == b"\x1b[201~")
                    .expect("an opened paste must close");
                assert_eq!(
                    close + 6,
                    bytes.len() - 1,
                    "the submit CR is the only byte after ESC[201~ for body {body:?}",
                );
            }
        }
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
    use lazybox_ipc::{Client, EVENT_CHANNEL_CAPACITY, Event, TerminalId};
    use tokio::sync::mpsc;
    use tuirealm::ratatui::layout::Size;

    /// Build a `Model` wired to a bounded inbound event channel we can
    /// fill directly — the same bounded channel the real transport
    /// hands the TUI ([`lazybox_ipc::EVENT_CHANNEL_CAPACITY`]), minus the
    /// daemon-side forwarder. Returns the sender so the test floods it
    /// itself. The command channel's receiver is held alive so the
    /// model's `send` calls don't observe a closed channel.
    fn model_with_event_sender() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        mpsc::Sender<Event>,
        mpsc::UnboundedReceiver<lazybox_ipc::Command>,
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
            drain_daemon_events(&mut m, None),
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
            backlog = drain_daemon_events(&mut m, None);
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
        drain_daemon_events(&mut m, None);
        assert_eq!(m.event_backlog.resyncs(), 3);
    }

    /// A daemon event the idle wait woke on (`Wake::Daemon`) is handed
    /// to the next drain as `carried` — it must be processed even when
    /// the channel itself is empty, and it counts toward the batch.
    #[test]
    fn carried_event_is_processed_when_channel_is_empty() {
        let (mut m, _evt_tx, _cmd_rx) = model_with_event_sender();

        let carried = Event::TerminalResync {
            terminal_id: TerminalId(1),
            replay: b"hello".to_vec(),
            seq: 1,
        };
        let backlog = drain_daemon_events(&mut m, Some(carried));
        assert!(!backlog, "a single carried event is no backlog");
        // The resync was dispatched + observed — proof the carried
        // event didn't get dropped on the floor.
        assert_eq!(m.event_backlog.resyncs(), 1);
    }
}

#[cfg(test)]
mod wake_tests {
    //! The unified idle wait (`wait_for_wake`) is the latency fix for
    //! "daemon events sit in the channel until the 16ms input poll
    //! expires": both sources must interrupt the wait immediately,
    //! idle must still tick on schedule, and a closed source must
    //! degrade to the heartbeat instead of busy-spinning. These tests
    //! freeze that contract.
    use super::super::helpers::{LoopRuntime, TimedInput, Wake, wait_for_wake};
    use lazybox_ipc::{Event, TerminalId};
    use std::time::{Duration, Instant};

    fn rt() -> LoopRuntime {
        LoopRuntime::acquire().expect("loop runtime")
    }

    fn daemon_event(seq: u64) -> Event {
        Event::TerminalOutput {
            terminal_id: TerminalId(1),
            bytes: b"echo".to_vec(),
            seq,
        }
    }

    type InputChannel = (
        tokio::sync::mpsc::Sender<TimedInput>,
        tokio::sync::mpsc::Receiver<TimedInput>,
    );

    fn channels() -> (
        InputChannel,
        tokio::sync::mpsc::Sender<Event>,
        tokio::sync::mpsc::Receiver<Event>,
    ) {
        let (itx, irx) = tokio::sync::mpsc::channel(8);
        let (dtx, drx) = tokio::sync::mpsc::channel(8);
        ((itx, irx), dtx, drx)
    }

    /// A queued daemon event wakes the wait immediately — no input
    /// event required, and nowhere near the (deliberately huge)
    /// timeout. This is the regression test for the old behavior
    /// where daemon events waited out `crossterm::event::poll(16ms)`.
    #[test]
    fn daemon_event_wakes_idle_wait_without_input() {
        let rt = rt();
        let ((_itx, mut irx), dtx, mut drx) = channels();
        dtx.try_send(daemon_event(1)).expect("room");

        let (mut input_open, mut daemon_open) = (true, true);
        let start = Instant::now();
        let wake = wait_for_wake(
            &rt,
            &mut irx,
            &mut input_open,
            &mut drx,
            &mut daemon_open,
            Duration::from_secs(30),
        );
        assert!(matches!(wake, Wake::Daemon(_)));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "daemon event must interrupt the wait, not ride out the timeout"
        );
    }

    /// Same, but the event lands while the wait is already blocked —
    /// proves the wakeup path, not just the non-empty fast path.
    #[test]
    fn daemon_event_posted_mid_wait_interrupts_it() {
        let rt = rt();
        let ((_itx, mut irx), dtx, mut drx) = channels();
        let poster = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = dtx.try_send(daemon_event(1));
        });

        let (mut input_open, mut daemon_open) = (true, true);
        let start = Instant::now();
        let wake = wait_for_wake(
            &rt,
            &mut irx,
            &mut input_open,
            &mut drx,
            &mut daemon_open,
            Duration::from_secs(30),
        );
        poster.join().expect("poster thread");
        assert!(matches!(wake, Wake::Daemon(_)));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "wait should wake on the posted event, not the 30s timeout"
        );
    }

    /// With both sources ready, input wins (`biased` order) — a
    /// streaming burst must never delay a keystroke.
    #[test]
    fn input_beats_daemon_when_both_are_ready() {
        let rt = rt();
        let ((itx, mut irx), dtx, mut drx) = channels();
        dtx.try_send(daemon_event(1)).expect("room");
        itx.try_send(TimedInput {
            read_at: Instant::now(),
            event: crossterm::event::Event::FocusGained,
        })
        .expect("room");

        let (mut input_open, mut daemon_open) = (true, true);
        let wake = wait_for_wake(
            &rt,
            &mut irx,
            &mut input_open,
            &mut drx,
            &mut daemon_open,
            Duration::from_secs(30),
        );
        assert!(matches!(wake, Wake::Input(_)));
    }

    /// Nothing queued: the wait holds for the idle bound, then ticks.
    /// The heartbeat is what drives latch timeouts (`q q`, `]]`),
    /// spinner frames, and the modal-redraw window.
    #[test]
    fn idle_wait_times_out_to_tick() {
        let rt = rt();
        let ((_itx, mut irx), _dtx, mut drx) = channels();

        let (mut input_open, mut daemon_open) = (true, true);
        let idle = Duration::from_millis(20);
        let start = Instant::now();
        let wake = wait_for_wake(
            &rt,
            &mut irx,
            &mut input_open,
            &mut drx,
            &mut daemon_open,
            idle,
        );
        assert!(matches!(wake, Wake::Tick));
        assert!(
            start.elapsed() >= idle,
            "idle tick must wait out the full bound — no busy spin"
        );
    }

    /// A closed daemon channel flips its open flag and the NEXT wait
    /// degrades to the timed heartbeat — a hung-up daemon must not
    /// turn the loop into a busy spin.
    #[test]
    fn closed_daemon_channel_degrades_to_heartbeat() {
        let rt = rt();
        let ((_itx, mut irx), dtx, mut drx) = channels();
        drop(dtx);

        let (mut input_open, mut daemon_open) = (true, true);
        let wake = wait_for_wake(
            &rt,
            &mut irx,
            &mut input_open,
            &mut drx,
            &mut daemon_open,
            Duration::from_secs(30),
        );
        assert!(matches!(wake, Wake::Tick));
        assert!(!daemon_open, "closed source must disable its branch");

        // Branch disabled: the next wait runs out the idle bound
        // instead of returning instantly on the closed channel.
        let idle = Duration::from_millis(20);
        let start = Instant::now();
        let wake = wait_for_wake(
            &rt,
            &mut irx,
            &mut input_open,
            &mut drx,
            &mut daemon_open,
            idle,
        );
        assert!(matches!(wake, Wake::Tick));
        assert!(start.elapsed() >= idle);
    }
}

#[cfg(test)]
mod coalesce_tests {
    //! `coalesce_adjacent_output` collapses a streaming burst into one
    //! dispatch per terminal — this is what keeps memory bounded under
    //! a chatty agent. The merge must be byte-for-byte faithful and
    //! must NOT reorder across terminals or non-output events.
    use super::super::helpers::coalesce_adjacent_output;
    use lazybox_ipc::{Event, TerminalId};

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
mod stale_input_tests {
    //! The stale-input guard is what bounds input replay after a
    //! stall: input the run loop couldn't service while it was
    //! blocked must be dropped, not burst-replayed against UI state
    //! the user never saw (issue #49 — "it did all the clicking and
    //! quitting in succession").
    use super::super::helpers::{STALE_INPUT_MAX_AGE, StaleInputTally, should_drop_stale_input};
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use std::time::Duration;

    fn key_event() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
    }

    fn mouse_event() -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Fresh input always dispatches — the guard only engages when an
    /// event sat buffered past the staleness bound.
    #[test]
    fn fresh_input_is_never_dropped() {
        for ev in [key_event(), mouse_event(), Event::Paste("hi".into())] {
            assert!(!should_drop_stale_input(&ev, Duration::ZERO));
            assert!(!should_drop_stale_input(
                &ev,
                STALE_INPUT_MAX_AGE - Duration::from_millis(1)
            ));
        }
    }

    /// Keys and mouse events buffered past the bound are dropped —
    /// this is what keeps a buffered quit chord (or a backlog of
    /// clicks) from firing when a frozen loop recovers.
    #[test]
    fn stale_keys_and_mouse_are_dropped() {
        let age = STALE_INPUT_MAX_AGE + Duration::from_secs(2);
        assert!(should_drop_stale_input(&key_event(), age));
        assert!(should_drop_stale_input(&mouse_event(), age));
    }

    /// Paste is deliberate content (dropping it loses user data) and
    /// focus events describe current terminal state — both survive a
    /// stall regardless of age.
    #[test]
    fn stale_paste_and_focus_are_kept() {
        let age = STALE_INPUT_MAX_AGE + Duration::from_secs(2);
        assert!(!should_drop_stale_input(&Event::Paste("body".into()), age));
        assert!(!should_drop_stale_input(&Event::FocusGained, age));
        assert!(!should_drop_stale_input(&Event::FocusLost, age));
    }

    /// The tally batches a whole recovery burst into one report:
    /// count + oldest age out, then reset so the next episode starts
    /// clean.
    #[test]
    fn tally_accumulates_and_flushes_once() {
        let mut t = StaleInputTally::default();
        assert!(t.flush().is_none(), "empty tally has nothing to report");
        t.note(Duration::from_secs(3));
        t.note(Duration::from_secs(1));
        t.note(Duration::from_secs(2));
        let (dropped, oldest) = t.flush().expect("a report");
        assert_eq!(dropped, 3);
        assert_eq!(oldest, Duration::from_secs(3), "oldest age wins");
        assert!(t.flush().is_none(), "flush resets the episode");
    }
}

#[cfg(test)]
mod scroll_classification_tests {
    //! Mouse-wheel scroll is the one high-rate input: a flick fires
    //! faster than a full repaint, so its redraw is routed through the
    //! render throttle (coalesced to the display refresh) while discrete
    //! input keeps painting per event. The classifier is what splits the
    //! two — misclassifying a keystroke as scroll would make typing feel
    //! laggy; misclassifying scroll as discrete brings back the stall.
    use super::super::helpers::is_scroll_event;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn wheel_in_every_direction_is_scroll() {
        for kind in [
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert!(is_scroll_event(&mouse(kind)), "{kind:?} should be scroll");
        }
    }

    /// Clicks, drags, keys, and paste are discrete input — they must
    /// keep painting immediately, so the classifier must NOT fold them
    /// into the coalesced-scroll path.
    #[test]
    fn discrete_input_is_not_scroll() {
        assert!(!is_scroll_event(&mouse(MouseEventKind::Down(
            MouseButton::Left
        ))));
        assert!(!is_scroll_event(&mouse(MouseEventKind::Drag(
            MouseButton::Left
        ))));
        assert!(!is_scroll_event(&mouse(MouseEventKind::Moved)));
        assert!(!is_scroll_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::NONE
        ))));
        assert!(!is_scroll_event(&Event::Paste("text".into())));
    }
}

#[cfg(test)]
mod watchdog_tests {
    //! The loop watchdog turns "the UI felt frozen" into warn lines
    //! with durations in /tmp/lazybox.log. Iterations within the
    //! frame budget are silent; over-budget ones warn, rate-limited
    //! so a pathological loop doesn't flood the log at frame rate.
    use super::super::helpers::{FRAME_BUDGET, LoopWatchdog, PhaseTimings};
    use std::time::{Duration, Instant};

    #[test]
    fn within_budget_is_silent() {
        let mut w = LoopWatchdog::default();
        let now = Instant::now();
        assert!(!w.observe(Duration::ZERO, PhaseTimings::default(), now));
        assert!(!w.observe(FRAME_BUDGET, PhaseTimings::default(), now));
    }

    #[test]
    fn over_budget_warns() {
        let mut w = LoopWatchdog::default();
        assert!(w.observe(
            FRAME_BUDGET + Duration::from_millis(1),
            PhaseTimings::default(),
            Instant::now()
        ));
    }

    /// Back-to-back slow iterations inside the warn interval are
    /// suppressed; once the interval passes the next one warns again.
    #[test]
    fn warnings_are_rate_limited() {
        let mut w = LoopWatchdog::default();
        let t0 = Instant::now();
        let slow = FRAME_BUDGET + Duration::from_millis(100);
        let t = PhaseTimings::default();
        assert!(w.observe(slow, t, t0));
        assert!(!w.observe(slow, t, t0 + Duration::from_millis(200)));
        assert!(!w.observe(slow, t, t0 + Duration::from_millis(400)));
        assert!(w.observe(slow, t, t0 + Duration::from_secs(2)));
    }

    /// `worst` names the longest segment so the warn line points at the
    /// prime suspect — the whole reason the phase is broken down.
    #[test]
    fn worst_phase_picks_the_longest_segment() {
        let timings = PhaseTimings {
            dispatch: Duration::from_millis(1),
            drain: Duration::from_millis(80),
            ticks: Duration::from_millis(2),
            messages: Duration::from_millis(3),
            render: Duration::from_millis(40),
        };
        let (name, dur) = timings.worst();
        assert_eq!(name, "drain");
        assert_eq!(dur, Duration::from_millis(80));
    }

    /// An all-zero phase still resolves to a named segment, never a
    /// panic on the empty-iterator path.
    #[test]
    fn worst_phase_of_idle_iteration_is_defined() {
        let (name, dur) = PhaseTimings::default().worst();
        assert_eq!(dur, Duration::ZERO);
        assert!(!name.is_empty());
    }
}

#[cfg(test)]
mod perf_tests {
    //! The opt-in perf sampler (`LAZYBOX_PERF=1`) routes run-loop
    //! counters to a dedicated target. The sampling decision is a pure
    //! predicate so it's testable without the env var; the dropped-input
    //! tally is the headline "must stay 0" counter.
    use super::super::helpers::{PerfMonitor, sample_due};
    use std::time::Duration;

    /// Disabled is always a no-op, regardless of how slow the iteration
    /// was — no perf file, no overhead, when the flag is unset.
    #[test]
    fn disabled_never_samples() {
        assert!(!sample_due(false, Duration::from_secs(1), 4096, true));
    }

    /// Idle heartbeat iterations (under the floor, empty channel, within
    /// budget) are skipped so the perf log stays signal, not 60Hz noise.
    #[test]
    fn enabled_skips_idle_iterations() {
        assert!(!sample_due(true, Duration::from_micros(50), 0, false));
    }

    /// Real work clears the bar: an over-budget stall, a non-empty
    /// channel, or a work phase past the floor each earns a sample.
    #[test]
    fn enabled_samples_real_work() {
        assert!(sample_due(true, Duration::from_micros(50), 0, true)); // over budget
        assert!(sample_due(true, Duration::from_micros(50), 1, false)); // backlog
        assert!(sample_due(true, Duration::from_millis(2), 0, false)); // render-sized
    }

    /// Stale-input drops accumulate across episodes — the running total
    /// is the signal that the loop discarded keystrokes.
    #[test]
    fn dropped_input_accumulates() {
        let mut perf = PerfMonitor::new();
        assert_eq!(perf.dropped_input(), 0);
        perf.note_dropped_input(3, Duration::from_millis(600));
        perf.note_dropped_input(2, Duration::from_millis(700));
        assert_eq!(perf.dropped_input(), 5);
    }
}

#[cfg(test)]
mod render_throttle_tests {
    //! Background-driven frames (daemon output, spinner ticks) are
    //! coalesced to one display refresh so an output flood can't
    //! saturate the render path; input-driven frames bypass the cap so
    //! scrolling stays per-event progressive and keystrokes never wait
    //! behind redundant repaints.
    use super::super::helpers::{MIN_BACKGROUND_RENDER_INTERVAL, RenderThrottle};
    use std::time::{Duration, Instant};

    /// Input-driven redraws always paint, no matter how recently a
    /// frame rendered — that's what keeps a scroll gesture progressive.
    #[test]
    fn input_driven_always_renders() {
        let mut t = RenderThrottle::default();
        let now = Instant::now();
        t.record(now);
        // Zero elapsed since the last paint, but it's input → renders.
        assert!(t.should_render(now, true));
    }

    /// The first frame paints even with no prior render recorded, so
    /// startup isn't held back by the cap.
    #[test]
    fn first_background_frame_renders() {
        let t = RenderThrottle::default();
        assert!(t.should_render(Instant::now(), false));
    }

    /// Back-to-back background frames inside one refresh are coalesced;
    /// once a refresh has elapsed the next background frame paints.
    #[test]
    fn background_frames_coalesce_to_one_refresh() {
        let mut t = RenderThrottle::default();
        let t0 = Instant::now();
        t.record(t0);
        // Within the interval → deferred.
        assert!(!t.should_render(
            t0 + MIN_BACKGROUND_RENDER_INTERVAL - Duration::from_millis(1),
            false
        ));
        // At the interval → paints.
        assert!(t.should_render(t0 + MIN_BACKGROUND_RENDER_INTERVAL, false));
    }

    /// A background frame deferred during a burst still paints the
    /// moment input arrives — the deferred update is never stranded.
    #[test]
    fn deferred_background_frame_flushes_on_input() {
        let mut t = RenderThrottle::default();
        let t0 = Instant::now();
        t.record(t0);
        let mid = t0 + Duration::from_millis(1);
        assert!(!t.should_render(mid, false), "background frame waits");
        assert!(t.should_render(mid, true), "input flushes it immediately");
    }
}

#[cfg(test)]
mod subscribed_projects_tests {
    //! `refresh_subscribed_projects` add/remove contract — the
    //! placeholder headers lazybox synthesizes for narrowed repo
    //! subscriptions before the daemon surfaces a workspace.
    use super::super::*;
    use lazybox_core::{PersistedSetup, Project, ProjectKey};
    use lazybox_ipc::{Event as IpcEvent, channel};
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

    /// A reconnect `Snapshot` is authoritative for daemon projects: one
    /// deleted while the client was disconnected must be pruned, not
    /// linger as a ghost header.
    #[test]
    fn reconnect_snapshot_prunes_vanished_project() {
        let mut m = build_model();
        let pk = ProjectKey::github("acme", "widget");
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![],
            terminals: vec![],
            projects: vec![Project::new(pk.clone(), "acme/widget", chrono::Utc::now())],
        });
        assert!(m.projects.contains_key(&pk), "snapshot seeds the project");

        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![],
            terminals: vec![],
            projects: vec![],
        });
        assert!(
            !m.projects.contains_key(&pk),
            "project absent from the reconnect snapshot must be pruned"
        );
    }

    /// Locally-synthesized placeholders never appear in the snapshot, so
    /// pruning must spare them.
    #[test]
    fn reconnect_snapshot_keeps_synthesized_placeholder() {
        let mut m = build_model();
        let pk = ProjectKey::github("acme", "widget");
        m.setup.persisted = Some(persisted_with_scopes(&["github:acme/widget"]));
        m.refresh_subscribed_projects();
        assert!(m.projects.contains_key(&pk), "placeholder synthesized");

        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![],
            terminals: vec![],
            projects: vec![],
        });
        assert!(
            m.projects.contains_key(&pk),
            "synthesized placeholder must survive a reconnect snapshot"
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
    use lazybox_core::WorkspaceKey;
    use lazybox_ipc::{Event as IpcEvent, channel};
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

    /// `t` opens the theme picker (catalog → dispatch → mount): the
    /// modal mounts, every registered palette is offered, and the
    /// active theme is stashed so Esc can restore it. Esc then closes
    /// the picker and clears both stashes. The live-preview behavior
    /// (apply on highlight) is unit-tested on `Choice` itself; the
    /// persist-on-Enter path by the config round-trip test. This test
    /// avoids asserting on the process-global active theme, which other
    /// parallel tests legitimately mutate.
    #[test]
    fn theme_picker_opens_from_t_and_cancels_clean() {
        let mut m = build_model();

        assert!(m.top_modal().is_none(), "no modal before t");
        m.dispatch_key(KeyEvent::new(Key::Char('t'), KeyModifiers::NONE));
        assert_eq!(m.top_modal(), Some(&Id::ThemePicker));
        assert!(
            m.theme_picker_prev.is_some(),
            "the open theme is stashed for restore-on-cancel",
        );
        assert!(
            m.theme_choices.iter().any(|n| n == "Lazybox Light"),
            "the picker lists every registered theme",
        );

        m.dispatch_modal_key(key(Key::Esc));
        assert!(m.top_modal().is_none(), "Esc closes the picker");
        assert!(m.theme_picker_prev.is_none(), "restore stash is consumed");
        assert!(m.theme_choices.is_empty(), "choices are released");
    }

    /// `]` opens the read-only snippets browser from the sidebar
    /// (catalog → dispatch → mount), and Esc pops it. The browser is a
    /// global, so it fires with no workspace selected — the discovery
    /// entry point issue #237 asks for outside the `]]` terminal leader.
    #[test]
    fn bracket_opens_and_closes_snippet_browser() {
        let mut m = build_model();
        m.apply_snippets(lazybox_config::Snippets::builtin());

        assert!(m.top_modal().is_none(), "no modal before ]");
        m.dispatch_key(KeyEvent::new(Key::Char(']'), KeyModifiers::NONE));
        assert_eq!(m.top_modal(), Some(&Id::SnippetBrowser));

        m.dispatch_modal_key(key(Key::Esc));
        assert!(m.top_modal().is_none(), "Esc closes the snippets browser");
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

/// The `q q` quit chord (issue #100): the first `q` arms a hint
/// instead of quitting silently; a second `q` quits; `Esc` cancels.
mod quit_chord_tests {
    use super::super::Model;
    use lazybox_ipc::channel;
    use tuirealm::event::{Key, KeyEvent, KeyModifiers};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn q() -> KeyEvent {
        KeyEvent::new(Key::Char('q'), KeyModifiers::NONE)
    }

    #[test]
    fn first_q_arms_the_hint_without_quitting() {
        let mut m = build_model();
        m.dispatch_key(q());
        assert!(!m.quit, "a single q must not quit");
        assert!(
            m.q_arm_pending(),
            "the first q must arm the chord so the hint surfaces",
        );
    }

    #[test]
    fn second_q_quits() {
        let mut m = build_model();
        m.dispatch_key(q());
        m.dispatch_key(q());
        assert!(m.quit, "q q must quit");
    }

    #[test]
    fn esc_cancels_the_armed_chord() {
        let mut m = build_model();
        m.dispatch_key(q());
        assert!(m.q_arm_pending());
        m.dispatch_key(KeyEvent::new(Key::Esc, KeyModifiers::NONE));
        assert!(!m.quit, "Esc after the first q must not quit");
        assert!(!m.q_arm_pending(), "Esc must disarm the chord");
    }
}

#[cfg(test)]
mod merge_focus_follow_tests {
    //! Issue→PR collapse (#34): when the user is viewing the issue
    //! workspace as it gets absorbed, focus must follow the moved
    //! sessions onto the PR workspace — otherwise the cursor lands on an
    //! arbitrary row and the merged session looks lost.
    use super::super::*;
    use chrono::{Duration, Utc};
    use lazybox_core::{SessionKey, Task, TaskId, Workspace, WorkspaceKey};
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    /// `/pull/` in the URL routes the task into the PR slot; anything
    /// else lands as an issue (`pr == None`). `age` orders rows: the
    /// sidebar sorts updated_at desc, so a smaller age sits higher.
    fn task(key: &str, is_pr: bool, age: Duration) -> Task {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        let segment = if is_pr { "pull" } else { "issues" };
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("task: {key}"),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{path}/{segment}/{num}"),
            repo: Some("owner/repo".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now() - age,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        }
    }

    fn workspace(key: &str, is_pr: bool, age: Duration) -> Workspace {
        Workspace::from_task(task(key, is_pr, age), Utc::now())
    }

    fn agent_session(workspace_key: &WorkspaceKey) -> lazybox_core::WorkspaceSession {
        lazybox_core::WorkspaceSession::new(
            workspace_key.clone(),
            lazybox_core::SessionKind::Agent {
                agent_id: "claude".into(),
            },
            std::path::PathBuf::from("/tmp/wt"),
            Utc::now(),
        )
    }

    #[test]
    fn merge_while_viewing_issue_follows_focus_to_pr() {
        let mut m = build_model();

        // A decoy PR sits at the top of the list (newest). Without the
        // focus-follow it would win the "land on the first row" fallback
        // after the issue is removed — so this test only passes when
        // focus genuinely follows the merge to its target.
        let decoy = workspace("owner/repo#9", true, Duration::minutes(1));
        let issue = workspace("owner/repo#1", false, Duration::hours(1));
        let mut pr = workspace("owner/repo#2", true, Duration::hours(2));
        let issue_key = issue.key.clone();
        let pr_key = pr.key.clone();
        let decoy_key = decoy.key.clone();

        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(decoy)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(issue.clone())));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr.clone())));
        assert!(
            m.sidebar.focus_workspace_key(&SessionKey::from(&issue_key)),
            "issue workspace row should be focusable",
        );

        // Daemon-side merge event sequence: PR upsert (now holding the
        // moved session) → issue removal → merge notice.
        pr.add_session(agent_session(&pr_key));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(issue_key.clone()));
        m.handle_daemon_event(IpcEvent::WorkspaceMerged {
            issue_workspace_key: issue_key,
            pr_workspace_key: pr_key.clone(),
            issue_label: "owner/repo#1".into(),
            pr_label: "owner/repo#2".into(),
        });

        let selected = m.sidebar.selected_workspace().expect("a row is selected");
        assert_eq!(
            selected.key, pr_key,
            "focus followed the merge onto the PR workspace (not the decoy {decoy_key:?})",
        );
        assert!(
            !selected.sessions.is_empty(),
            "the merged session is visible under the PR workspace",
        );
    }

    #[test]
    fn merge_while_viewing_elsewhere_does_not_steal_focus() {
        let mut m = build_model();

        // Three rows; the user is parked on an unrelated PR, NOT on the
        // issue being merged.
        let issue = workspace("owner/repo#1", false, Duration::hours(1));
        let pr = workspace("owner/repo#2", true, Duration::hours(2));
        let other = workspace("owner/repo#3", true, Duration::minutes(1));
        let issue_key = issue.key.clone();
        let pr_key = pr.key.clone();
        let other_key = other.key.clone();

        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(issue.clone())));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr.clone())));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(other)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&other_key)));

        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(issue_key.clone()));
        m.handle_daemon_event(IpcEvent::WorkspaceMerged {
            issue_workspace_key: issue_key,
            pr_workspace_key: pr_key,
            issue_label: "owner/repo#1".into(),
            pr_label: "owner/repo#2".into(),
        });

        assert_eq!(
            m.sidebar.selected_workspace().map(|w| w.key.clone()),
            Some(other_key),
            "a merge the user wasn't watching must not yank their cursor",
        );
    }

    /// Regression for #160: the daemon's issue→PR merge burst
    /// (`TerminalsRebadged` → `WorkspaceRemoved` → `WorkspaceMerged`)
    /// arrives as one drain batch and must leave the loop responsive —
    /// projecting the panes ONCE for the batch, not once per event. A
    /// per-event `sync_panes` clones the selected `Workspace` and
    /// re-emits `FocusWorkspace` for every intermediate cursor position;
    /// under a real merge that compounded into the UI-thread stall the
    /// issue reported. We assert the whole burst drains without backlog,
    /// focus follows the merge onto the PR, and the daemon's focus hint
    /// was re-aimed at most once.
    #[test]
    fn merge_burst_coalesces_to_a_single_pane_sync() {
        use super::super::helpers::drain_daemon_events;
        use lazybox_ipc::{Client, Command, EVENT_CHANNEL_CAPACITY};
        use tokio::sync::mpsc;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = Client::from_channels(cmd_tx, evt_rx);
        let mut m = Model::<tuirealm::terminal::TestTerminalAdapter>::new_for_test(
            client,
            Size::new(120, 40),
        )
        .expect("model init");

        // A decoy PR sits newest, so the post-removal cursor fallback
        // would land THERE first: without coalescing the `WorkspaceRemoved`
        // sync emits `FocusWorkspace(decoy)` before `WorkspaceMerged`
        // re-aims at the real PR — two focus hints for one logical move.
        let decoy = workspace("owner/repo#9", true, Duration::minutes(1));
        let issue = workspace("owner/repo#1", false, Duration::hours(1));
        let mut pr = workspace("owner/repo#2", true, Duration::hours(2));
        let issue_key = issue.key.clone();
        let pr_key = pr.key.clone();
        pr.add_session(agent_session(&pr_key));

        // Seed the rows, park the cursor on the issue, and settle the
        // focus baseline so the burst is measured from "viewing the issue".
        for ws in [decoy, issue.clone(), pr.clone()] {
            m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        }
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&issue_key)));
        m.sync_panes();
        while cmd_rx.try_recv().is_ok() {} // drop setup focus/fetch traffic

        // The daemon's merge burst, delivered as ONE drain batch.
        let from: SessionKey = (&issue_key).into();
        let to: SessionKey = (&pr_key).into();
        for evt in [
            IpcEvent::TerminalsRebadged {
                from: from.clone(),
                to: to.clone(),
            },
            IpcEvent::WorkspaceRemoved(issue_key.clone()),
            IpcEvent::WorkspaceMerged {
                issue_workspace_key: issue_key.clone(),
                pr_workspace_key: pr_key.clone(),
                issue_label: "owner/repo#1".into(),
                pr_label: "owner/repo#2".into(),
            },
        ] {
            evt_tx.try_send(evt).expect("room in the bounded channel");
        }

        let backlog = drain_daemon_events(&mut m, None);
        assert!(!backlog, "a 3-event burst is well under the per-tick cap");

        assert_eq!(
            m.sidebar.selected_workspace().map(|w| w.key.clone()),
            Some(pr_key.clone()),
            "focus followed the merge onto the PR workspace",
        );

        let focus_hints = std::iter::from_fn(|| cmd_rx.try_recv().ok())
            .filter(|c| matches!(c, Command::FocusWorkspace { .. }))
            .count();
        assert_eq!(
            focus_hints, 1,
            "merge burst coalesced to a single FocusWorkspace hint \
             (per-event sync would re-aim it for the intermediate decoy too)",
        );
    }

    /// Regression for #177: `w` provisions a worktree first (seconds) and
    /// the `TerminalSpawned` lands much later. If the user navigated away
    /// in the meantime, focus must still snap back to the workspace `w`
    /// fired on — with the freshly-spawned agent as the active tab — not
    /// stay on wherever the cursor drifted.
    #[test]
    fn w_spawn_follows_to_target_after_navigating_away() {
        use lazybox_ipc::{TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();

        // A workable issue (slow first-time spawn) plus a decoy PR the
        // cursor can wander to while the worktree provisions.
        let issue = workspace("owner/repo#1", false, Duration::hours(1));
        let decoy = workspace("owner/repo#9", true, Duration::minutes(1));
        let issue_key = issue.key.clone();
        let decoy_key = decoy.key.clone();
        let issue_sk: SessionKey = (&issue_key).into();

        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(issue)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(decoy)));

        // Press `w` on the issue → arms the follow target + emits Spawn.
        assert!(m.sidebar.focus_workspace_key(&issue_sk));
        let cmds = m.dispatch_action(&Action::Work);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, lazybox_ipc::Command::Spawn { .. })),
            "`w` on a workable issue emits a Spawn",
        );

        // The worktree is still provisioning; the user wanders to the decoy.
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&decoy_key)));
        m.sync_panes();
        assert_eq!(
            m.sidebar.selected_workspace().map(|w| w.key.clone()),
            Some(decoy_key),
            "cursor parked on the decoy before the terminal lands",
        );

        // The agent terminal finally lands — much later, on the ISSUE.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: issue_sk,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
        });

        // Focus snapped back to the issue, new agent as the active tab.
        assert_eq!(
            m.sidebar.selected_workspace().map(|w| w.key.clone()),
            Some(issue_key),
            "focus follows the spawn back onto the workspace `w` fired on",
        );
        assert_eq!(
            m.focus,
            PaneFocus::Terminals,
            "focus lands on the terminal pane",
        );
        assert_eq!(
            m.terminals.active_terminal_id(),
            Some(TerminalId(7)),
            "the freshly-spawned agent is the active tab",
        );
    }

    /// Gap #3 of #177: `w` on a workspace with nothing to act on (no PR,
    /// issue, or selected comments) used to silently do nothing. It must
    /// now give explicit footer feedback and arm no follow target.
    #[test]
    fn w_on_unworkable_workspace_flashes_feedback() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();

        let bare = Workspace::empty(
            WorkspaceKey::new("github:owner/repo#sandbox"),
            "sandbox",
            Utc::now(),
        );
        let bare_sk: SessionKey = (&bare.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(bare)));
        assert!(m.sidebar.focus_workspace_key(&bare_sk));

        let cmds = m.dispatch_action(&Action::Work);
        assert!(cmds.is_empty(), "`w` with nothing to do emits no Spawn");
        assert!(
            m.spawn_follow_to.is_none(),
            "no follow target armed when nothing spawns",
        );
        let notice = m.status.notice.as_ref().expect("footer feedback shown");
        assert!(
            notice.message.contains("nothing to work on"),
            "explicit feedback instead of a silent no-op: {:?}",
            notice.message,
        );
    }

    /// Issue #224: bare `w` on a workspace whose only running agent is
    /// Codex must target Codex — not always spawn the default Claude.
    #[test]
    fn bare_w_targets_the_running_agent_over_default() {
        use lazybox_ipc::{Command, TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));

        // Only a Codex agent is running on this workspace.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(3),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
        });
        assert!(m.sidebar.focus_workspace_key(&sk));

        let cmds = m.dispatch_action(&Action::Work);
        let agent = cmds.iter().find_map(|c| match c {
            Command::Spawn {
                kind: TerminalKind::Agent(id),
                ..
            } => Some(id.clone()),
            _ => None,
        });
        assert_eq!(
            agent.as_deref(),
            Some("codex"),
            "bare `w` targets the running Codex, not the default Claude",
        );
    }

    /// Issue #224: the scoped `w x` chord forces Codex, injecting the
    /// contextual work prompt into the already-running Codex session
    /// (rather than spawning a fresh one).
    #[test]
    fn scoped_w_x_injects_into_running_codex() {
        use lazybox_ipc::{Client, Command, EVENT_CHANNEL_CAPACITY, TerminalId, TerminalKind};
        use tokio::sync::mpsc;
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (_evt_tx, evt_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = Client::from_channels(cmd_tx, evt_rx);
        let mut m = Model::<tuirealm::terminal::TestTerminalAdapter>::new_for_test(
            client,
            Size::new(120, 40),
        )
        .expect("model init");

        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(5),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
        });
        // `TerminalSpawned` auto-focuses the terminal pane; return to the
        // sidebar (cursor on the PR) so the catalog resolves `w`.
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));
        while cmd_rx.try_recv().is_ok() {} // drop setup traffic

        // `w` arms the timed leader; `x` completes `w x` → work in Codex.
        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert!(m.work_leader_pending(), "`w` arms the scoped-work leader");
        m.dispatch_key(KeyEvent::new(Key::Char('x'), KeyModifiers::NONE));
        assert!(!m.work_leader_pending(), "`x` resolves the leader");

        let inject = std::iter::from_fn(|| cmd_rx.try_recv().ok())
            .find(|c| matches!(c, Command::InjectPrompt { .. }));
        assert!(
            inject.is_some(),
            "`w x` injects the work prompt into the running Codex",
        );
    }

    /// Issue #224: with no follow-up key, the `w` leader times out on the
    /// idle tick and fires bare `Work` against the running-or-default
    /// agent — so bare `w` still works without pressing a scoped key.
    #[test]
    fn w_leader_times_out_to_bare_work() {
        use lazybox_ipc::{Client, Command, EVENT_CHANNEL_CAPACITY, TerminalId, TerminalKind};
        use tokio::sync::mpsc;
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (_evt_tx, evt_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = Client::from_channels(cmd_tx, evt_rx);
        let mut m = Model::<tuirealm::terminal::TestTerminalAdapter>::new_for_test(
            client,
            Size::new(120, 40),
        )
        .expect("model init");
        m.ui_defaults.escape_window = std::time::Duration::from_millis(1);

        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(8),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
        });
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));
        while cmd_rx.try_recv().is_ok() {} // drop setup traffic

        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert!(m.work_leader_pending(), "`w` arms the leader");

        // Window elapsed (1ms) → the idle tick fires bare Work, which
        // injects into the running Codex.
        std::thread::sleep(std::time::Duration::from_millis(5));
        m.tick_work_leader();
        assert!(!m.work_leader_pending(), "idle tick resolves the leader");

        let inject = std::iter::from_fn(|| cmd_rx.try_recv().ok())
            .find(|c| matches!(c, Command::InjectPrompt { .. }));
        assert!(
            inject.is_some(),
            "bare `w` (leader timeout) injects work into the running Codex",
        );
    }

    /// Issue #224 hardening: a mouse click cancels the armed `w` leader,
    /// so its idle-tick timeout can't fire a stray `Work` after the user
    /// clicked away.
    #[test]
    fn mouse_click_cancels_the_work_leader() {
        use crossterm::event::{KeyModifiers as CtMods, MouseButton, MouseEvent, MouseEventKind};
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};
        use tuirealm::ratatui::layout::Rect;

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert!(m.work_leader_pending(), "`w` arms the leader");

        m.dispatch_mouse_in(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: CtMods::NONE,
            },
            Rect::new(0, 0, 120, 40),
        );
        assert!(
            !m.work_leader_pending(),
            "a mouse click must cancel the armed work leader",
        );
    }

    /// Issue #224 hardening: if a modal mounts (via a daemon event)
    /// while the `w` leader is armed, the idle-tick timeout cancels the
    /// leader instead of firing `Work` behind the modal.
    #[test]
    fn work_leader_timeout_does_not_fire_behind_a_modal() {
        use lazybox_core::WorkspaceKey;
        use lazybox_ipc::{Client, Command, EVENT_CHANNEL_CAPACITY};
        use tokio::sync::mpsc;
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (_evt_tx, evt_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = Client::from_channels(cmd_tx, evt_rx);
        let mut m = Model::<tuirealm::terminal::TestTerminalAdapter>::new_for_test(
            client,
            Size::new(120, 40),
        )
        .expect("model init");
        m.ui_defaults.escape_window = std::time::Duration::from_millis(1);

        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));
        while cmd_rx.try_recv().is_ok() {}

        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert!(m.work_leader_pending(), "`w` arms the leader");

        // A modal mounts from a daemon event — no keystroke clears the
        // leader, so the idle tick must.
        m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
            workspace_key: WorkspaceKey::new("github:owner/repo#9"),
            label: "owner/repo#9".into(),
            title: None,
            active_terminal_count: 1,
        });
        assert!(!m.modal_stack.is_empty(), "a modal is up");

        std::thread::sleep(std::time::Duration::from_millis(5));
        m.tick_work_leader();

        assert!(!m.work_leader_pending(), "the timeout clears the leader");
        let spawned = std::iter::from_fn(|| cmd_rx.try_recv().ok())
            .any(|c| matches!(c, Command::Spawn { .. } | Command::InjectPrompt { .. }));
        assert!(!spawned, "bare `Work` must not fire behind a modal",);
    }
}

#[cfg(test)]
mod chord_resolution_tests {
    //! Catalog chord resolution must be focus-aware. Regression for
    //! the right-pane shadowing bugs: `G` / `z` / `m` resolved to the
    //! Workspace section's AddAssignees / ToggleSnooze / MarkAllRead
    //! before the activity pane's own bindings ever saw the key.
    use super::super::PaneFocus;
    use super::super::helpers::{find_action_for_stroke, section_rank};
    use lazybox_tui_core::action::{ActionDef, ActionKind, Chord, KeyStroke};
    use std::collections::BTreeMap;

    fn stroke(s: &str) -> KeyStroke {
        KeyStroke::parse(s).unwrap_or_else(|| panic!("{s:?} must parse"))
    }

    /// Runtime catalog with the built-in agents, no overrides — the
    /// resolution surface `find_action_for_stroke` consults.
    fn catalog() -> Vec<lazybox_tui_core::action::CatalogEntry> {
        let agents: Vec<String> = ["claude", "codex", "cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        ActionDef::catalog(&agents, &BTreeMap::new())
    }

    fn resolve(s: &str, focus: PaneFocus) -> Option<ActionKind> {
        find_action_for_stroke(&stroke(s), focus, &catalog()).map(|e| e.kind)
    }

    /// Chord collisions that exist on purpose: the same key binds an
    /// Activity action (wins under Right focus) and a Workspace
    /// action (wins under Sidebar focus, still reachable from Right
    /// when no Activity entry claims the chord). Anything NOT listed
    /// here that collides is a shipped ambiguity.
    fn known_aliases() -> Vec<(Chord, Vec<ActionKind>)> {
        vec![
            (
                Chord::Key(stroke("Enter")),
                vec![ActionKind::OpenWorkspace, ActionKind::ToggleActivity],
            ),
            (
                Chord::Key(stroke("z")),
                vec![ActionKind::ToggleSnooze, ActionKind::UndoMarkRead],
            ),
            (
                Chord::Key(stroke("Shift-G")),
                vec![ActionKind::AddAssignees, ActionKind::ActivityBottom],
            ),
        ]
    }

    #[test]
    fn right_focus_resolves_activity_bindings_over_workspace() {
        assert_eq!(
            resolve("z", PaneFocus::Right),
            Some(ActionKind::UndoMarkRead),
            "`z` on the activity pane is undo-mark-read, not snooze",
        );
        assert_eq!(
            resolve("Shift-G", PaneFocus::Right),
            Some(ActionKind::ActivityBottom),
            "`G` on the activity pane is jump-to-bottom, not assignees",
        );
        assert_eq!(
            resolve("g", PaneFocus::Right),
            Some(ActionKind::ActivityTop),
        );
        // `m` stays on the Workspace MarkAllRead entry — the dispatch
        // decides per-row vs workspace-wide based on focus + cursor.
        assert_eq!(
            resolve("m", PaneFocus::Right),
            Some(ActionKind::MarkAllRead),
        );
    }

    #[test]
    fn sidebar_focus_resolution_is_unchanged() {
        assert_eq!(
            resolve("z", PaneFocus::Sidebar),
            Some(ActionKind::ToggleSnooze),
        );
        assert_eq!(
            resolve("Shift-G", PaneFocus::Sidebar),
            Some(ActionKind::AddAssignees),
        );
        assert_eq!(
            resolve("m", PaneFocus::Sidebar),
            Some(ActionKind::MarkAllRead),
        );
        // Activity-only entries must not leak into sidebar dispatch
        // (`g` is the github group leader there).
        assert_eq!(resolve("g", PaneFocus::Sidebar), None);
    }

    #[test]
    fn navigation_synonyms_stay_clear_of_the_catalog() {
        // `j` / `k` are pane-handler bindings (cursor movement); the
        // catalog must never claim them or the panes go deaf.
        for focus in [PaneFocus::Sidebar, PaneFocus::Right] {
            assert_eq!(resolve("j", focus), None, "j must reach the pane");
            assert_eq!(resolve("k", focus), None, "k must reach the pane");
        }
    }

    /// No two bindings reachable from the same focus may share a
    /// chord, except the explicitly-known aliases above — and those
    /// must never collide *within* the same rank (a same-rank tie has
    /// no deterministic winner by design).
    #[test]
    fn no_ambiguous_chords_per_focus() {
        let overrides = BTreeMap::new();
        for focus in [PaneFocus::Sidebar, PaneFocus::Right] {
            let mut by_chord: std::collections::HashMap<Chord, Vec<(u8, ActionKind)>> =
                std::collections::HashMap::new();
            for def in ActionDef::all() {
                let Some(rank) = section_rank(def.section, focus) else {
                    continue;
                };
                // Every alternative (leader sequence AND legacy alias)
                // is a binding the matcher can resolve — check each.
                for chord in def.effective_chords(&overrides) {
                    by_chord.entry(chord).or_default().push((rank, def.kind));
                }
            }
            let aliases = known_aliases();
            for (chord, entries) in by_chord {
                if entries.len() < 2 {
                    continue;
                }
                // Same-rank ties are always a bug.
                for (i, (rank_a, kind_a)) in entries.iter().enumerate() {
                    for (rank_b, kind_b) in entries.iter().skip(i + 1) {
                        assert_ne!(
                            rank_a, rank_b,
                            "{focus:?}: {kind_a:?} and {kind_b:?} share chord {chord:?} \
                             at the same rank — no deterministic winner",
                        );
                    }
                }
                // Cross-rank shadowing must be a documented alias.
                let mut kinds: Vec<ActionKind> = entries.iter().map(|(_, k)| *k).collect();
                kinds.sort_by_key(|k| format!("{k:?}"));
                let known = aliases.iter().any(|(c, ks)| {
                    let mut ks = ks.clone();
                    ks.sort_by_key(|k| format!("{k:?}"));
                    *c == chord && ks == kinds
                });
                assert!(
                    known,
                    "{focus:?}: chord {chord:?} is bound by {kinds:?} but isn't a \
                     known intentional alias — add an explicit entry or rebind",
                );
            }
        }
    }
}

#[cfg(test)]
mod daemon_event_fastpath_tests {
    //! Perf contracts for the two highest-frequency daemon events:
    //! `TerminalOutput` and `AgentState`. Both used to run the full
    //! `handle_daemon_event` tail — a per-event Workspace clone in
    //! `sync_panes` plus an unconditional redraw — even when nothing
    //! on screen could have changed.
    use super::super::Model;
    use chrono::Utc;
    use lazybox_core::{Workspace, WorkspaceKey};
    use lazybox_ipc::{AgentState, Event as IpcEvent, TerminalId, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn seed_workspace(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) -> WorkspaceKey {
        let ws = Workspace::empty(WorkspaceKey::new("github:o/r#1"), "main", Utc::now());
        let key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![ws],
            terminals: vec![],
            projects: vec![],
        });
        key
    }

    #[test]
    fn output_for_an_invisible_terminal_does_not_redraw() {
        let mut m = build_model();
        seed_workspace(&mut m);
        m.redraw = false;
        m.handle_daemon_event(IpcEvent::TerminalOutput {
            terminal_id: TerminalId(99),
            bytes: b"background noise".to_vec(),
            seq: 1,
        });
        assert!(
            !m.redraw,
            "output addressed at a terminal that isn't on screen must not redraw",
        );
    }

    #[test]
    fn output_for_a_visible_terminal_still_redraws() {
        let mut m = build_model();
        let key = seed_workspace(&mut m);
        // Spawn a terminal on the selected workspace — the spawn
        // handler focuses the terminal pane and makes it visible.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: (&key).into(),
            kind: lazybox_ipc::TerminalKind::Shell,
            no_permission: false,
        });
        m.redraw = false;
        m.handle_daemon_event(IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes: b"$ ls\n".to_vec(),
            seq: 1,
        });
        assert!(m.redraw, "visible-terminal output must trigger a redraw");
    }

    #[test]
    fn repeated_agent_state_pings_do_not_redraw() {
        let mut m = build_model();
        let key = seed_workspace(&mut m);
        let session_key: lazybox_core::SessionKey = (&key).into();

        m.redraw = false;
        m.handle_daemon_event(IpcEvent::AgentState {
            terminal_id: TerminalId(7),
            session_key: session_key.clone(),
            state: AgentState::Working,
        });
        assert!(m.redraw, "the Idle→Working edge must redraw");

        m.redraw = false;
        m.handle_daemon_event(IpcEvent::AgentState {
            terminal_id: TerminalId(7),
            session_key: session_key.clone(),
            state: AgentState::Working,
        });
        assert!(
            !m.redraw,
            "a repeated Working ping changes nothing on screen — no redraw",
        );

        m.redraw = false;
        m.handle_daemon_event(IpcEvent::AgentState {
            terminal_id: TerminalId(7),
            session_key,
            state: AgentState::InputNeeded,
        });
        assert!(m.redraw, "the Working→InputNeeded edge must redraw");
    }

    /// Tab badges are per-terminal: a second agent in the same
    /// workspace can need a badge flip even when the sidebar's
    /// session-level state is already correct. The redraw skip must
    /// consult the terminal stack too.
    #[test]
    fn badge_flip_in_terminal_stack_forces_redraw() {
        let mut m = build_model();
        let key = seed_workspace(&mut m);
        let session_key: lazybox_core::SessionKey = (&key).into();

        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(1),
            session_key: session_key.clone(),
            kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
            no_permission: false,
        });
        m.handle_daemon_event(IpcEvent::AgentState {
            terminal_id: TerminalId(1),
            session_key: session_key.clone(),
            state: AgentState::Working,
        });

        // Second agent spawns Idle — sidebar already shows Working
        // for the session, but THIS tab's badge is stale.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(2),
            session_key: session_key.clone(),
            kind: lazybox_ipc::TerminalKind::Agent("codex".into()),
            no_permission: false,
        });

        m.redraw = false;
        m.handle_daemon_event(IpcEvent::AgentState {
            terminal_id: TerminalId(2),
            session_key,
            state: AgentState::Working,
        });
        assert!(
            m.redraw,
            "terminal 2's badge flips Idle→Working — must redraw even though \
             the sidebar's session-level state didn't change",
        );
    }
}

#[cfg(test)]
mod wheel_routing_tests {
    //! Wheel-event routing contract for the terminal pane
    //! (`Model::handle_mouse`):
    //!
    //! - inner program NOT mouse-tracking → the wheel scrolls the
    //!   LOCAL libghostty scrollback; nothing is written to the
    //!   daemon (no round trip, no damper);
    //! - inner program mouse-tracking (DECSET 1000/1002/1006) → the
    //!   wheel is SGR-encoded and shipped to the PTY as a single
    //!   damped `Write`.
    //!
    //! This is the client half of the native-scrollback feature: the
    //! tmux backend no longer sets `mouse on`, so `is_mouse_tracking`
    //! reflects the inner app instead of always reading true.
    use super::super::*;
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use tuirealm::ratatui::layout::{Rect, Size};

    fn build_model_with_terminal() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
        Rect,
    ) {
        let (client, server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.layout.last_area = Rect::new(0, 0, 120, 40);

        let key = lazybox_core::SessionKey::from("github:o/r#1");
        m.terminals.set_active_session(Some(key.clone()));
        m.terminals.on_daemon_event(&IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: key,
            kind: TerminalKind::Shell,
            no_permission: false,
        });
        m.focus = PaneFocus::Terminals;

        let (_, _, bottom) = crate::realm::layout::pane_areas(
            m.layout.last_area,
            m.layout.sidebar_pct,
            m.layout.right_top_pct,
            m.layout.sidebar_user_resized,
        );
        (m, server, bottom)
    }

    fn wheel_up_at(col: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        }
    }

    /// Wheel over the terminal pane while the inner program has NOT
    /// requested mouse tracking → pure local scroll, zero daemon
    /// traffic. This is the path that makes scrolling instant.
    #[test]
    fn wheel_scrolls_locally_when_inner_app_is_not_mouse_tracking() {
        let (mut m, mut server, bottom) = build_model_with_terminal();
        assert!(
            !m.terminals.focused_terminal_tracks_mouse(),
            "fresh shell terminal must not report mouse tracking"
        );

        // Drain startup traffic (Subscribe) so the assertion below
        // only sees what the wheel produced.
        while server.rx.try_recv().is_ok() {}

        m.redraw = false;
        m.handle_mouse(wheel_up_at(bottom.x + 2, bottom.y + 2));

        assert!(
            server.rx.try_recv().is_err(),
            "local scrollback path must not send any IPC command"
        );
        assert!(m.redraw, "local scroll repaints the viewport");
    }

    /// Wheel over the terminal pane while the inner program IS mouse
    /// tracking (vim/htop/claude with mouse on) → the event is
    /// SGR-encoded and written to the daemon for the inner app to
    /// handle.
    #[test]
    fn wheel_forwards_sgr_when_inner_app_tracks_mouse() {
        let (mut m, mut server, bottom) = build_model_with_terminal();

        // The inner program enables button-event tracking + SGR
        // encoding — exactly what vim / htop / claude emit.
        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes: b"\x1b[?1002h\x1b[?1006h".to_vec(),
            seq: 1,
        });
        assert!(
            m.terminals.focused_terminal_tracks_mouse(),
            "DECSET 1002 must flip the tracking flag"
        );

        while server.rx.try_recv().is_ok() {}

        // Fire INSIDE the grid: the body is inset by the left border
        // (+1 col) and three rows of top chrome (+3 row) — a shell has
        // no recap rows. A wheel 6 cols / 8 rows into the pane must
        // encode to grid cell (5, 5), i.e. 1-based wire coords (6, 6).
        m.handle_mouse(wheel_up_at(bottom.x + 6, bottom.y + 8));

        match server.rx.try_recv() {
            Ok(lazybox_ipc::Command::Write { terminal_id, bytes }) => {
                assert_eq!(terminal_id, TerminalId(7));
                // SGR wheel-up is button 64. The coordinates MUST undo
                // the render inset — the bug forwarded raw pane coords
                // (off by +1 col, +3 row). `64;6;6` proves the offset.
                assert!(
                    bytes.starts_with(b"\x1b[<64;6;6"),
                    "wheel must SGR-encode at grid cell (6,6), got {bytes:?}"
                );
            }
            other => panic!("expected a Write with SGR wheel bytes, got {other:?}"),
        }
    }

    /// A wheel (or click) over the pane's top chrome — the tab strip /
    /// divider / blank rows that sit ABOVE the grid — must never be
    /// forwarded to the inner program as if it were a grid cell.
    /// Regression for the off-by-3 forward bug: the old code subtracted
    /// only the pane origin, so a wheel at `bottom.y + 1` (chrome)
    /// encoded to a bogus near-origin cell instead of falling through
    /// to local scrolling.
    #[test]
    fn wheel_over_pane_chrome_does_not_forward_to_inner_app() {
        let (mut m, mut server, bottom) = build_model_with_terminal();
        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes: b"\x1b[?1002h\x1b[?1006h".to_vec(),
            seq: 1,
        });
        assert!(m.terminals.focused_terminal_tracks_mouse());
        while server.rx.try_recv().is_ok() {}

        // Row +1 is the tab strip — above the grid (which starts at +3).
        m.handle_mouse(wheel_up_at(bottom.x + 2, bottom.y + 1));

        assert!(
            server.rx.try_recv().is_err(),
            "a wheel over the tab strip must not Write a forwarded mouse event"
        );
    }

    // ── `]` flush + Ctrl-w literal: assert the BYTES reaching the PTY ──

    use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};

    /// Collect every byte written to the daemon since the last drain.
    fn drained_write_bytes(server: &mut lazybox_ipc::Connection) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(cmd) = server.rx.try_recv() {
            if let lazybox_ipc::Command::Write { bytes, .. } = cmd {
                out.extend_from_slice(&bytes);
            }
        }
        out
    }

    /// A lone `]` is HELD (not written) until the next key; a following
    /// non-`]` key flushes the literal `]` to the PTY, then itself. This
    /// is the headline behavior of the `]` fix — previously unverified at
    /// the byte level.
    #[test]
    fn held_bracket_flushes_to_pty_before_next_key() {
        let (mut m, mut server, _bottom) = build_model_with_terminal();
        while server.rx.try_recv().is_ok() {} // drain Subscribe

        m.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        assert!(
            drained_write_bytes(&mut server).is_empty(),
            "a lone `]` is held pending the chord, not written yet"
        );

        m.dispatch_key(RealmKey::new(Key::Char('a'), RealmMods::NONE));
        assert_eq!(
            drained_write_bytes(&mut server),
            b"]a",
            "the held `]` must reach the PTY ahead of the next key"
        );
    }

    /// `]]` completes the leader (here: arms it — even with no snippets
    /// the leader offers `]]f` / `]]<digit>`) and must NOT flush a
    /// literal `]` to the PTY.
    #[test]
    fn completed_leader_does_not_flush_a_bracket() {
        let (mut m, mut server, _bottom) = build_model_with_terminal();
        while server.rx.try_recv().is_ok() {}

        m.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        m.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        assert!(m.terminal_leader_pending(), "`]]` arms the leader");
        assert_eq!(m.focus(), PaneFocus::Terminals, "leader doesn't leave yet");
        assert!(
            !drained_write_bytes(&mut server).contains(&b']'),
            "`]]` is a chord, not two literal brackets"
        );
    }

    // (Ctrl-w escape-hatch byte behavior is unit-tested at the
    // TerminalStack level in `components::terminal_stack::ctrl_w_tests`,
    // which exercises `handle_key` directly without Model key routing.)
}

#[cfg(test)]
mod destructive_confirm_tests {
    //! Regression coverage for the destructive-action confirm path:
    //!
    //! 1. The right-click context menu must route MergePr / Archive
    //!    through the unified ActionConfirm modal — never hand-map
    //!    them straight to `MergePr` / `Kill` IPC commands.
    //! 2. The confirm must fire against the target resolved at MOUNT
    //!    time. Daemon events can move the sidebar cursor while the
    //!    modal is up; "Yes" must not act on whatever the cursor
    //!    drifted onto.
    use super::super::{ActionConfirmTarget, Id, Model};
    use chrono::Utc;
    use lazybox_core::{SessionKey, Task, TaskId, Workspace, WorkspaceKey};
    use lazybox_ipc::{Command as IpcCommand, Event as IpcEvent, channel};
    use lazybox_tui_core::action::Action;
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn seed(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>, key: &str) -> WorkspaceKey {
        let ws = Workspace::empty(WorkspaceKey::new(key), "main", Utc::now());
        let k = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        k
    }

    #[test]
    fn context_menu_archive_mounts_confirm_instead_of_killing() {
        let mut m = build_model();
        let wk = seed(&mut m, "github:o/r#1");
        let sk = SessionKey::from(&wk);
        m.pending_sidebar_context = Some((sk.clone(), vec![Action::MergePr, Action::Archive]));
        m.modal_stack.push(Id::SidebarContext);

        let cmds = m.handle_choice_picked(vec![1]);
        assert!(
            cmds.is_empty(),
            "Archive picked from the context menu must not emit Kill directly: {cmds:?}",
        );
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::ActionConfirm),
            "the unified confirm modal must mount",
        );

        // Confirming actually fires the kill, aimed at the menu's row.
        let cmds = m.handle_confirmed(true);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::Kill { session_key } => assert_eq!(session_key, &sk),
            other => panic!("expected Kill after Yes, got {other:?}"),
        }
    }

    #[test]
    fn context_menu_merge_pr_mounts_confirm_instead_of_merging() {
        let mut m = build_model();
        let wk = seed(&mut m, "github:o/r#1");
        let sk = SessionKey::from(&wk);
        m.pending_sidebar_context = Some((sk.clone(), vec![Action::MergePr, Action::Archive]));
        m.modal_stack.push(Id::SidebarContext);

        let cmds = m.handle_choice_picked(vec![0]);
        assert!(
            cmds.is_empty(),
            "MergePr picked from the context menu must not emit MergePr directly: {cmds:?}",
        );
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));
        match &m.pending_action_confirm {
            Some((Action::MergePr, ActionConfirmTarget::Workspace(k))) => assert_eq!(k, &sk),
            other => panic!("expected a stashed MergePr aimed at the menu's row, got {other:?}"),
        }
    }

    #[test]
    fn merge_confirm_fires_on_green_ci_without_approval() {
        // Regression for #144: a green-CI PR with no formal approval
        // (a personal repo / your own PR) is mergeable on GitHub, so
        // confirming `g m` must dispatch the merge — not flash
        // "no longer merge-ready" and do nothing.
        let mut m = build_model();
        let pr = merge_ready_pr_without_approval("github:owner/repo#1");
        let wk = pr.key.clone();
        let sk = SessionKey::from(&wk);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk), "PR row focusable");

        let cmds = m.dispatch_action(&Action::MergePr);
        assert!(
            cmds.is_empty(),
            "merge must gate on confirm first: {cmds:?}"
        );
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        let cmds = m.handle_confirmed(true);
        match cmds.as_slice() {
            [IpcCommand::MergePr { workspace_key }] => assert_eq!(workspace_key, &wk),
            other => panic!("expected a single MergePr command, got {other:?}"),
        }
    }

    /// A PR workspace GitHub would let you merge right now — CI green,
    /// no conflict — but with NO approving review, the case #144 was
    /// falsely blocking.
    fn merge_ready_pr_without_approval(key: &str) -> Workspace {
        let num = key.rsplit_once('#').map(|(_, n)| n).unwrap_or("1");
        let task = Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::Success,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/owner/repo/pull/{num}"),
            repo: Some("owner/repo".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        };
        Workspace::from_task(task, Utc::now())
    }

    #[test]
    fn confirm_fires_against_the_mount_time_target_not_the_live_cursor() {
        let mut m = build_model();
        let a = seed(&mut m, "github:o/r#1");
        let b = seed(&mut m, "github:o/r#2");
        let sa = SessionKey::from(&a);
        let sb = SessionKey::from(&b);

        assert!(m.sidebar.focus_workspace_key(&sa), "workspace A focusable");
        let cmds = m.dispatch_action(&Action::Archive);
        assert!(cmds.is_empty(), "destructive action must gate on confirm");
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        // A daemon event moves the cursor under the modal.
        assert!(m.sidebar.focus_workspace_key(&sb), "workspace B focusable");

        let cmds = m.handle_confirmed(true);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::Kill { session_key } => assert_eq!(
                session_key, &sa,
                "Yes must kill the workspace the prompt named, not the drifted selection",
            ),
            other => panic!("expected Kill, got {other:?}"),
        }
    }

    #[test]
    fn confirm_noops_with_notice_when_the_target_vanished() {
        let mut m = build_model();
        let a = seed(&mut m, "github:o/r#1");
        let sa = SessionKey::from(&a);

        assert!(m.sidebar.focus_workspace_key(&sa));
        let _ = m.dispatch_action(&Action::Archive);
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        // The workspace disappears while the modal is up.
        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(a));

        let cmds = m.handle_confirmed(true);
        assert!(
            cmds.is_empty(),
            "a vanished target must no-op, not fire at another row: {cmds:?}",
        );
        assert!(
            m.status.notice.is_some(),
            "the user should get a footer notice explaining the no-op",
        );
    }
}

#[cfg(test)]
mod queued_prompt_drain_tests {
    //! A daemon prompt (removal / merge) that arrives while another
    //! modal is up gets queued. Re-emits are deduped, so EVERY
    //! handler that pops the stack empty must drain the queue —
    //! including the picker handlers, not just dismiss/confirm.
    use super::super::{Id, Model};
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn queue_removal_prompt(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) {
        m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
            workspace_key: WorkspaceKey::new("github:o/r#9"),
            label: "o/r#9".into(),
            title: None,
            active_terminal_count: 1,
        });
    }

    #[test]
    fn removal_prompt_mounts_after_a_choice_picker_resolves() {
        let mut m = build_model();
        // A snooze picker is open when the daemon prompt arrives.
        m.pending_snooze_workspace = Some(SessionKey::from("github:o/r#1"));
        m.snooze_choices = vec![std::time::Duration::from_secs(3600)];
        m.modal_stack.push(Id::SnoozeDuration);
        queue_removal_prompt(&mut m);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SnoozeDuration),
            "the prompt must wait behind the open picker",
        );

        // Confirming the picker pops the stack — the queued prompt
        // must surface right then, not wait for a dismissal that
        // never comes.
        let _ = m.handle_choice_picked(vec![0]);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::RemoveOutOfScope),
            "queued removal prompt must mount once the picker resolves",
        );
        assert!(m.active_removal_prompt.is_some());
    }

    #[test]
    fn removal_prompt_mounts_after_an_input_submit() {
        let mut m = build_model();
        // The new-project input is open when the prompt arrives.
        m.modal_stack.push(Id::NewProject);
        queue_removal_prompt(&mut m);
        assert_eq!(m.modal_stack.last(), Some(&Id::NewProject));

        let _ = m.handle_input_submitted("scratch".into());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::RemoveOutOfScope),
            "queued removal prompt must mount once the input submits",
        );
    }

    #[test]
    fn removal_prompt_mounts_after_a_textarea_submit() {
        let mut m = build_model();
        m.pending_reply = Some(SessionKey::from("github:o/r#1"));
        m.modal_stack.push(Id::Reply);
        queue_removal_prompt(&mut m);
        assert_eq!(m.modal_stack.last(), Some(&Id::Reply));

        let _ = m.handle_textarea_submitted("looks good".into());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::RemoveOutOfScope),
            "queued removal prompt must mount once the reply submits",
        );
    }
}

#[cfg(test)]
mod setup_finish_tests {
    //! The wizard Finish handler must surface save failures (and not
    //! cache state that never hit disk), and must mention the .bak
    //! file when a malformed config was moved aside.
    use super::super::Model;
    use crate::setup::SetupReport;
    use crate::setup_flow::{RunnerStep, SetupOutcome, SetupRunner};
    use lazybox_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn report() -> SetupReport {
        SetupReport { tools: vec![] }
    }

    fn finish(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) {
        let runner = SetupRunner::new(report(), Default::default());
        let outcome = SetupOutcome::default_enabled(report());
        m.handle_runner_step(runner, RunnerStep::Finish(outcome));
    }

    #[test]
    fn failed_save_flashes_error_and_does_not_cache() {
        let mut m = build_model();
        m.setup.on_complete = Some(std::sync::Arc::new(|_| Err(anyhow::anyhow!("disk full"))));
        finish(&mut m);
        assert!(
            m.setup.persisted.is_none(),
            "a failed save must not cache the new persisted state",
        );
        let n = m.status.notice.as_ref().expect("an error notice is up");
        assert!(
            n.message.contains("NOT saved"),
            "the notice must say the save failed: {:?}",
            n.message,
        );
    }

    #[test]
    fn successful_save_caches_state_and_surfaces_the_backup() {
        let mut m = build_model();
        m.setup.on_complete = Some(std::sync::Arc::new(|_| {
            Ok(Some(std::path::PathBuf::from(
                "/tmp/config.yaml.bak-20260610",
            )))
        }));
        finish(&mut m);
        assert!(
            m.setup.persisted.is_some(),
            "a successful save caches the new persisted state",
        );
        let n = m.status.notice.as_ref().expect("a backup notice is up");
        assert!(
            n.message.contains("bak-20260610"),
            "the notice must point at the backup file: {:?}",
            n.message,
        );
    }
}

#[cfg(test)]
mod collapse_into_pr_tests {
    //! Issue #78: joining an Issue into a PR (`Shift-J`) must not drop
    //! the running Claude terminal. The daemon rebadges the live
    //! terminal onto the PR and emits, in order:
    //!   `TerminalsRebadged` → `WorkspaceUpserted(pr)` →
    //!   `WorkspaceRemoved(issue)` → `WorkspaceMerged`.
    //! This drives that exact sequence through the orchestrator and
    //! asserts the user ends up viewing the PR with the SAME terminal
    //! still on screen — not an empty pane where the session used to be.
    use super::super::*;
    use chrono::Utc;
    use lazybox_core::{SessionKind, Workspace, WorkspaceKey, WorkspaceSession};
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    #[test]
    fn shift_j_keeps_the_live_terminal_visible_on_the_pr() {
        let mut m = build_model();

        let issue_key = WorkspaceKey::new("github:o/r#50");
        let pr_key = WorkspaceKey::new("github:o/r#51");
        let issue_sk: lazybox_core::SessionKey = (&issue_key).into();
        let pr_sk: lazybox_core::SessionKey = (&pr_key).into();

        // Issue workspace carries a Claude session; PR is a separate row.
        let mut issue_ws = Workspace::empty(issue_key.clone(), "lazybox/issue-50", Utc::now());
        issue_ws.add_session(WorkspaceSession::new(
            issue_key.clone(),
            SessionKind::Agent {
                agent_id: "claude".into(),
            },
            std::path::PathBuf::from("/tmp/wt-50"),
            Utc::now(),
        ));
        let pr_ws = Workspace::empty(pr_key.clone(), "feature", Utc::now());
        // A third, unrelated row so the post-removal cursor has somewhere
        // to land that ISN'T the PR — this keeps the "view follows onto
        // the PR" assertion load-bearing rather than satisfied by the PR
        // being the only survivor.
        let other_ws = Workspace::empty(WorkspaceKey::new("github:o/r#9"), "other", Utc::now());
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![issue_ws.clone(), pr_ws.clone(), other_ws],
            terminals: vec![],
            projects: vec![],
        });

        // User is on the issue, with Claude running and on screen.
        assert!(m.sidebar.focus_workspace_key(&issue_sk), "focus issue row");
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: issue_sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
        });
        assert!(
            m.terminals.active_terminal_id() == Some(TerminalId(7)),
            "the Claude terminal is on screen before the join",
        );

        // The daemon's collapse broadcast, in wire order.
        m.handle_daemon_event(IpcEvent::TerminalsRebadged {
            from: issue_sk.clone(),
            to: pr_sk.clone(),
        });
        let mut pr_with_session = pr_ws.clone();
        let mut moved = issue_ws.sessions[0].clone();
        moved.workspace_key = pr_key.clone();
        pr_with_session.add_session(moved);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr_with_session)));
        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(issue_key.clone()));
        m.handle_daemon_event(IpcEvent::WorkspaceMerged {
            issue_workspace_key: issue_key.clone(),
            pr_workspace_key: pr_key.clone(),
            issue_label: "#50".into(),
            pr_label: "#51".into(),
        });

        // The view followed onto the PR, and the SAME terminal is still
        // on screen there — the session was carried over, not lost.
        assert_eq!(
            m.sidebar.selected_workspace_key().map(|k| k.as_str()),
            Some(pr_key.as_str()),
            "the view must follow the moved session onto the PR",
        );
        assert_eq!(
            m.terminals.active_session().map(|k| k.as_str()),
            Some(pr_sk.as_str()),
            "the terminal stack's active session must be the PR",
        );
        assert!(
            m.terminals.active_terminal_id() == Some(TerminalId(7)),
            "the live Claude terminal must remain visible on the PR",
        );
    }

    /// Issue #205 — the NOT-SHOWN dimension. A Claude parked on a prompt
    /// (`InputNeeded`) emits no further output, so the daemon never
    /// re-broadcasts its `AgentState` after the collapse. The badge must
    /// still follow onto the PR purely on the strength of
    /// `TerminalsRebadged` — otherwise the agent is alive but invisible,
    /// which is exactly how this bug keeps reading as "session lost".
    #[test]
    fn shift_j_keeps_the_input_needed_badge_on_the_pr() {
        use lazybox_ipc::AgentState;

        let mut m = build_model();

        let issue_key = WorkspaceKey::new("github:o/r#50");
        let pr_key = WorkspaceKey::new("github:o/r#51");
        let issue_sk: lazybox_core::SessionKey = (&issue_key).into();
        let pr_sk: lazybox_core::SessionKey = (&pr_key).into();

        let mut issue_ws = Workspace::empty(issue_key.clone(), "lazybox/issue-50", Utc::now());
        issue_ws.add_session(WorkspaceSession::new(
            issue_key.clone(),
            SessionKind::Agent {
                agent_id: "claude".into(),
            },
            std::path::PathBuf::from("/tmp/wt-50"),
            Utc::now(),
        ));
        let pr_ws = Workspace::empty(pr_key.clone(), "feature", Utc::now());
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![issue_ws.clone(), pr_ws.clone()],
            terminals: vec![],
            projects: vec![],
        });

        // Claude on the issue is blocked on a prompt.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: issue_sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
        });
        m.handle_daemon_event(IpcEvent::AgentState {
            session_key: issue_sk.clone(),
            terminal_id: TerminalId(7),
            state: AgentState::InputNeeded,
        });
        assert!(
            !m.sidebar
                .displays_agent_state(&pr_sk, AgentState::InputNeeded),
            "precondition: the PR is not yet asking",
        );

        // The collapse burst — note NO trailing AgentState under the PR
        // key, because the parked agent produced no new output.
        m.handle_daemon_event(IpcEvent::TerminalsRebadged {
            from: issue_sk.clone(),
            to: pr_sk.clone(),
        });
        let mut pr_with_session = pr_ws.clone();
        let mut moved = issue_ws.sessions[0].clone();
        moved.workspace_key = pr_key.clone();
        pr_with_session.add_session(moved);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr_with_session)));
        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(issue_key.clone()));
        m.handle_daemon_event(IpcEvent::WorkspaceMerged {
            issue_workspace_key: issue_key.clone(),
            pr_workspace_key: pr_key.clone(),
            issue_label: "#50".into(),
            pr_label: "#51".into(),
        });

        // NOT SHOWN guard: the InputNeeded badge rendered on the PR…
        assert!(
            m.sidebar
                .displays_agent_state(&pr_sk, AgentState::InputNeeded),
            "the agent's InputNeeded badge must follow onto the PR",
        );
        // …and stopped pointing at the now-deleted issue key.
        assert!(
            !m.sidebar
                .displays_agent_state(&issue_sk, AgentState::InputNeeded),
            "the badge must not linger on the deleted issue key",
        );
    }
}

#[cfg(test)]
mod tips_tests {
    //! Issue #115: the progressive feature-tip gating. `pick_tip` is
    //! the pure decision (no IO) behind `tick_tips`; these freeze the
    //! "stay quiet" rules — off when opted out, before the idle delay,
    //! while a modal / notice owns the footer — and the one positive
    //! path (idle + in-terminal → the leave-terminal tip).
    use super::super::*;
    use lazybox_ipc::channel;
    use std::time::{Duration, Instant};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    /// Enable tips and backdate the idle baseline so the delay gate is
    /// satisfied — the common setup for "a tip should now be eligible."
    fn armed_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let mut m = build_model();
        m.set_tips(true, Vec::new());
        m.tips_armed_at = Instant::now() - Duration::from_secs(60);
        m
    }

    #[test]
    fn no_tip_when_disabled() {
        let mut m = armed_model();
        m.set_tips(false, Vec::new());
        m.focus = PaneFocus::Terminals;
        assert!(m.pick_tip().is_none());
    }

    #[test]
    fn no_tip_before_idle_delay() {
        let mut m = armed_model();
        m.tips_armed_at = Instant::now();
        m.focus = PaneFocus::Terminals;
        assert!(m.pick_tip().is_none(), "a tip must wait out the idle delay",);
    }

    #[test]
    fn no_tip_while_a_notice_owns_the_footer() {
        let mut m = armed_model();
        m.focus = PaneFocus::Terminals;
        m.flash_info("something else");
        assert!(
            m.pick_tip().is_none(),
            "a tip must not clobber an existing notice",
        );
    }

    #[test]
    fn no_tip_while_a_modal_is_open() {
        let mut m = armed_model();
        m.focus = PaneFocus::Terminals;
        m.modal_stack.push(Id::Help);
        assert!(
            m.pick_tip().is_none(),
            "a tip must never compete with a modal",
        );
    }

    #[test]
    fn in_terminal_surfaces_the_leave_terminal_tip_once() {
        let mut m = armed_model();
        m.focus = PaneFocus::Terminals;
        let tip = m.pick_tip().expect("the in-terminal tip is eligible");
        assert_eq!(tip.id, "leave_terminal");
        // Once it has been marked shown this session, the cap kicks in.
        m.tip_shown_this_session = true;
        assert!(
            m.pick_tip().is_none(),
            "at most one tip surfaces per session",
        );
    }

    #[test]
    fn already_seen_tip_does_not_resurface() {
        let mut m = armed_model();
        m.set_tips(true, vec!["leave_terminal".to_string()]);
        m.tips_armed_at = Instant::now() - Duration::from_secs(60);
        m.focus = PaneFocus::Terminals;
        assert!(
            m.pick_tip().is_none(),
            "a tip already in tips_seen never repeats",
        );
    }
}

#[cfg(test)]
mod activity_pane_visibility_tests {
    //! Hide the Activity pane when a workspace has no activity worth
    //! showing (#162), with `Shift-P` to reveal / re-hide on demand.
    use super::super::{Model, PaneFocus};
    use chrono::Utc;
    use lazybox_core::{Workspace, WorkspaceKey};
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::event::{Key, KeyEvent, KeyModifiers};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn empty_ws(key: &str) -> Workspace {
        Workspace::empty(WorkspaceKey::new(key), "main", Utc::now())
    }

    fn ws_with_activity(key: &str) -> Workspace {
        let mut w = empty_ws(key);
        w.activity.push(lazybox_core::Activity {
            author: "alice".into(),
            body: "ping".into(),
            created_at: Utc::now(),
            kind: lazybox_core::ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        w
    }

    fn seed(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>, workspaces: Vec<Workspace>) {
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces,
            terminals: vec![],
            projects: vec![],
        });
    }

    fn shift_p() -> KeyEvent {
        KeyEvent::new(Key::Char('P'), KeyModifiers::SHIFT)
    }

    #[test]
    fn empty_workspace_hides_the_activity_pane() {
        let mut m = build_model();
        seed(&mut m, vec![empty_ws("github:o/r#1")]);
        assert!(
            !m.activity_pane_visible(),
            "a workspace with no activity / description hides the pane",
        );
    }

    #[test]
    fn workspace_with_activity_shows_the_pane() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        assert!(m.activity_pane_visible());
    }

    #[test]
    fn no_selection_keeps_the_pane_visible() {
        // The auto-hide rule is about a *selected* workspace with no
        // activity; an empty inbox keeps the pane's prior behavior.
        let m = build_model();
        assert!(m.activity_pane_visible());
    }

    #[test]
    fn shift_p_reveals_an_empty_pane_then_re_hides() {
        let mut m = build_model();
        seed(&mut m, vec![empty_ws("github:o/r#1")]);
        assert!(!m.activity_pane_visible(), "auto-hidden when empty");

        m.dispatch_key(shift_p());
        assert!(m.activity_pane_visible(), "Shift-P reveals it on demand");

        m.dispatch_key(shift_p());
        assert!(!m.activity_pane_visible(), "Shift-P again re-hides it");
    }

    #[test]
    fn shift_p_can_hide_a_non_empty_pane() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        assert!(m.activity_pane_visible());
        m.dispatch_key(shift_p());
        assert!(
            !m.activity_pane_visible(),
            "the override can hide a non-empty pane too"
        );
    }

    #[test]
    fn override_is_remembered_per_workspace_across_navigation() {
        let mut m = build_model();
        // Two empty rows; reveal the first, then move to the second.
        seed(
            &mut m,
            vec![empty_ws("github:o/r#1"), empty_ws("github:o/r#2")],
        );
        let first: lazybox_core::SessionKey = (&WorkspaceKey::new("github:o/r#1")).into();
        let second: lazybox_core::SessionKey = (&WorkspaceKey::new("github:o/r#2")).into();

        assert!(m.sidebar.focus_workspace_key(&first));
        m.sync_panes();
        m.dispatch_key(shift_p());
        assert!(m.activity_pane_visible(), "revealed on the first row");

        // Navigate to the second row — its own default (hidden) applies.
        assert!(m.sidebar.focus_workspace_key(&second));
        m.sync_panes();
        assert!(
            !m.activity_pane_visible(),
            "the manual reveal doesn't leak onto a different workspace",
        );

        // Back to the first — the reveal override is still in effect.
        assert!(m.sidebar.focus_workspace_key(&first));
        m.sync_panes();
        assert!(
            m.activity_pane_visible(),
            "the per-workspace override persists across navigation",
        );
    }

    #[test]
    fn tab_skips_the_hidden_activity_pane() {
        let mut m = build_model();
        seed(&mut m, vec![empty_ws("github:o/r#1")]);
        // Start on the sidebar; Tab should jump past the hidden Activity
        // pane straight to the terminal stack.
        assert_eq!(m.focus(), PaneFocus::Sidebar);
        m.dispatch_key(KeyEvent::new(Key::Tab, KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Terminals,
            "Tab skips the hidden Activity pane",
        );
    }

    #[test]
    fn enter_on_empty_workspace_goes_straight_to_terminal() {
        let mut m = build_model();
        seed(&mut m, vec![empty_ws("github:o/r#1")]);
        assert_eq!(m.focus(), PaneFocus::Sidebar);
        m.dispatch_key(KeyEvent::new(Key::Enter, KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Terminals,
            "opening an activity-less workspace lands on the terminal",
        );
    }

    #[test]
    fn enter_with_activity_focuses_the_activity_pane() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        assert_eq!(m.focus(), PaneFocus::Sidebar);
        m.dispatch_key(KeyEvent::new(Key::Enter, KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Right,
            "with activity present, Enter focuses the Activity pane to read it",
        );
    }
}

#[cfg(test)]
mod workspace_focus_memory_tests {
    //! Re-selecting a workspace restores the pane it was last focused in
    //! (#182): clicking away from an agent terminal and back must land
    //! focus on that terminal again, not strand it on the sidebar where
    //! keystrokes are silently lost.
    use super::super::{Model, PaneFocus};
    use chrono::Utc;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use lazybox_core::{SessionKey, Workspace, WorkspaceKey};
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use tuirealm::ratatui::layout::{Rect, Size};

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn empty_ws(key: &str) -> Workspace {
        Workspace::empty(WorkspaceKey::new(key), "main", Utc::now())
    }

    fn key_of(key: &str) -> SessionKey {
        (&WorkspaceKey::new(key)).into()
    }

    /// Register a live terminal slot for a workspace's session without
    /// disturbing the active selection — the terminal stack filters its
    /// visible set by the active session, so the slot only surfaces once
    /// that workspace is selected.
    fn spawn_terminal(
        m: &mut Model<tuirealm::terminal::TestTerminalAdapter>,
        key: &SessionKey,
        id: u64,
    ) {
        m.terminals.on_daemon_event(&IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(id),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
        });
    }

    fn left_down(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Screen row a click must land on to select `key`. The cursor index
    /// maps to a row below the sidebar's 5-line header (mirrors the
    /// `HEADER_HEIGHT` constant in `Sidebar::click_to_select`); scroll is
    /// zero for the handful of rows these tests seed.
    fn row_of(
        m: &mut Model<tuirealm::terminal::TestTerminalAdapter>,
        sidebar_rect: Rect,
        key: &SessionKey,
    ) -> u16 {
        assert!(
            m.__test_sidebar_mut().focus_workspace_key(key),
            "workspace {key:?} should be in the sidebar",
        );
        sidebar_rect.y + 5 + m.sidebar().cursor() as u16
    }

    #[test]
    fn re_selecting_a_workspace_restores_its_terminal_focus() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1"), empty_ws("github:o/r#2")],
            terminals: vec![],
            projects: vec![],
        });
        let a = key_of("github:o/r#1");
        let b = key_of("github:o/r#2");
        spawn_terminal(&mut m, &a, 1);
        spawn_terminal(&mut m, &b, 2);

        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _, right_bottom) = m.effective_pane_rects(area);
        let row_a = row_of(&mut m, sidebar_rect, &a);
        let row_b = row_of(&mut m, sidebar_rect, &b);

        // Select WS-A, then click into its agent terminal as if typing.
        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_a), area);
        assert_eq!(m.sidebar().selected_workspace_key(), Some(&a));
        m.dispatch_mouse_in(left_down(right_bottom.x + 2, right_bottom.y + 2), area);
        assert_eq!(m.focus(), PaneFocus::Terminals, "typing into WS-A's agent");
        assert_eq!(m.terminals.active_terminal_id(), Some(TerminalId(1)));

        // Click away to WS-B: first visit has no memory, so focus drops
        // to the sidebar (today's behavior for an unvisited workspace).
        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_b), area);
        assert_eq!(m.sidebar().selected_workspace_key(), Some(&b));
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "clicking a not-yet-driven workspace focuses the sidebar",
        );

        // Click back to WS-A: its remembered terminal focus is restored,
        // on WS-A's own active terminal.
        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_a), area);
        assert_eq!(m.sidebar().selected_workspace_key(), Some(&a));
        assert_eq!(
            m.focus(),
            PaneFocus::Terminals,
            "re-selecting WS-A restores focus to its agent terminal",
        );
        assert_eq!(
            m.terminals.active_terminal_id(),
            Some(TerminalId(1)),
            "restored focus lands on WS-A's active session, not WS-B's",
        );
    }

    #[test]
    fn clicking_the_already_selected_row_keeps_the_sidebar() {
        // The escape hatch: clicking the sidebar row of the workspace
        // whose terminal you're in drops to the sidebar instead of
        // bouncing focus back into the terminal.
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1")],
            terminals: vec![],
            projects: vec![],
        });
        let a = key_of("github:o/r#1");
        spawn_terminal(&mut m, &a, 1);

        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _, right_bottom) = m.effective_pane_rects(area);
        let row_a = row_of(&mut m, sidebar_rect, &a);

        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_a), area);
        m.dispatch_mouse_in(left_down(right_bottom.x + 2, right_bottom.y + 2), area);
        assert_eq!(m.focus(), PaneFocus::Terminals);

        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_a), area);
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "clicking the current workspace's row stays on the sidebar",
        );
    }
}

#[cfg(test)]
mod focus_mode_tests {
    use super::super::*;
    use chrono::Utc;
    use lazybox_core::{SessionKey, Task, Workspace};
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn workspace_with_agent(key: &str) -> Workspace {
        let task = Task {
            id: lazybox_core::TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("task: {key}"),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{}", key.replace('#', "/pull/")),
            repo: Some("owner/repo".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        };
        let mut ws = Workspace::from_task(task, Utc::now());
        let wk = ws.key.clone();
        ws.add_session(lazybox_core::WorkspaceSession::new(
            wk,
            lazybox_core::SessionKind::Agent {
                agent_id: "claude".into(),
            },
            std::path::PathBuf::from("/tmp/wt"),
            Utc::now(),
        ));
        ws
    }

    /// Mark the terminal stack non-empty by spawning a terminal for the
    /// active session — the precondition for entering focus mode.
    fn spawn_terminal(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>, key: &SessionKey) {
        m.terminals.set_active_session(Some(key.clone()));
        m.terminals.on_daemon_event(&IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(1),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
        });
    }

    fn char_key(c: char) -> RealmKey {
        RealmKey::new(Key::Char(c), RealmMods::NONE)
    }

    /// Arm the `]]` leader (two presses of the escape char) and then
    /// press `follow`, so `]]<follow>` resolves in one call. Focus must
    /// already be on the terminal.
    fn bracket_leader(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>, follow: char) {
        m.dispatch_key(char_key(']'));
        m.dispatch_key(char_key(']'));
        m.dispatch_key(char_key(follow));
    }

    /// `.` from the sidebar enters focus mode (with a live terminal) and
    /// pins focus to the terminal; `]]f` from inside the terminal exits,
    /// leaving focus on the terminal so the user keeps driving the same
    /// agent in the three-pane view.
    #[test]
    fn dot_and_bracket_f_toggle_focus_mode() {
        let mut m = build_model();
        let ws = workspace_with_agent("owner/repo#1");
        let key = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        spawn_terminal(&mut m, &key);
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();

        m.dispatch_key(char_key('.'));
        assert!(m.focus_mode, "`.` enters focus mode");
        assert_eq!(m.focus(), PaneFocus::Terminals, "focus pins to terminal");

        bracket_leader(&mut m, 'f');
        assert!(!m.focus_mode, "`]]f` exits focus mode");
        assert_eq!(m.focus(), PaneFocus::Terminals, "exit keeps the terminal");
    }

    /// With no live terminal there's nothing to maximize, so `.` is a
    /// no-op rather than dropping the user onto a blank screen.
    #[test]
    fn dot_without_terminal_is_a_noop() {
        let mut m = build_model();
        m.focus = PaneFocus::Sidebar;
        m.dispatch_key(char_key('.'));
        assert!(!m.focus_mode, "no terminal → no focus mode");
    }

    /// Bare `]]` arms the leader; with no follow key the pane leaves on
    /// the idle tick — and in focus mode that must also drop focus mode,
    /// since the sidebar it returns to is hidden while focus mode is on.
    #[test]
    fn bracket_idle_leave_exits_focus_mode() {
        let mut m = build_model();
        let ws = workspace_with_agent("owner/repo#1");
        let key = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        spawn_terminal(&mut m, &key);
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m.focus_mode = true;

        // `]]` arms the leader (no immediate leave now that the leader
        // always has bindings to offer); with no follow key the idle
        // tick leaves the pane once the escape window lapses.
        m.dispatch_key(char_key(']'));
        m.dispatch_key(char_key(']'));
        assert!(m.terminal_leader_at.is_some(), "`]]` arms the leader");
        // Force the idle window past, then tick — Instant can't be
        // fast-forwarded, so backdate the arm timestamp instead.
        m.terminal_leader_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
        m.tick_terminal_leader();
        assert!(!m.focus_mode, "idle `]]` exits focus mode");
        assert_eq!(m.focus(), PaneFocus::Sidebar);
    }

    /// `]]<digit>` moves the displayed terminal to the Nth agent
    /// workspace in sidebar order and keeps focus mode on, so the user
    /// hops to a specific agent heads-down.
    #[test]
    fn bracket_digit_jumps_to_agent_workspace_in_focus_mode() {
        let mut m = build_model();
        let ws1 = workspace_with_agent("owner/repo#1");
        let ws2 = workspace_with_agent("owner/repo#2");
        let key1 = SessionKey::from(&ws1.key);
        let key2 = SessionKey::from(&ws2.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws1)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws2)));

        // The jump number is the slot in this roster (sidebar order),
        // which the badge mirrors — read it rather than assume an order.
        let roster = m.sidebar.agent_workspace_keys();
        assert_eq!(roster.len(), 2, "both agents in the roster");
        assert!(roster.contains(&key1) && roster.contains(&key2));

        // Start parked on slot 2 so `]]1` is a real move to slot 1.
        let slot1 = roster[0].clone();
        let slot2 = roster[1].clone();
        assert!(m.sidebar.focus_workspace_key(&slot2));
        spawn_terminal(&mut m, &slot2);
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m.focus_mode = true;

        bracket_leader(&mut m, '1');
        assert!(m.focus_mode, "jump stays in focus mode");
        assert_eq!(
            m.sidebar.selected_workspace_key(),
            Some(&slot1),
            "`]]1` jumps to the first agent workspace in the roster",
        );
    }

    /// The attention summary the header reads counts unread / asking /
    /// CI / review across the visible mailbox.
    #[test]
    fn attention_summary_tracks_unread() {
        let mut m = build_model();
        let mut ws = workspace_with_agent("owner/repo#1");
        ws.activity.push(lazybox_core::Activity {
            author: "alice".into(),
            body: "ping".into(),
            created_at: Utc::now(),
            kind: lazybox_core::ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        ws.seen_count = 0;
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        let summary = m.sidebar.attention_summary();
        assert_eq!(summary.unread, 1, "the unseen comment counts as unread");
    }
}

mod jump_to_workspace_tests {
    use super::super::*;
    use chrono::{Duration, Utc};
    use lazybox_core::{SessionKey, Task, TaskId, Workspace};
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn task(key: &str, age: Duration) -> Task {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("task: {key}"),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{path}/pull/{num}"),
            repo: Some("owner/repo".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now() - age,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        }
    }

    fn seed_two(
        m: &mut Model<tuirealm::terminal::TestTerminalAdapter>,
    ) -> (SessionKey, SessionKey) {
        let a = Workspace::from_task(task("owner/repo#1", Duration::minutes(1)), Utc::now());
        let b = Workspace::from_task(task("owner/repo#2", Duration::hours(1)), Utc::now());
        let ak = SessionKey::from(&a.key);
        let bk = SessionKey::from(&b.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(a)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(b)));
        (ak, bk)
    }

    /// The backtick chord mounts the fuzzy switcher from the sidebar and
    /// stashes one row per tracked workspace.
    #[test]
    fn backtick_opens_jump_picker_from_sidebar() {
        let mut m = build_model();
        seed_two(&mut m);
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        m.dispatch_key(RealmKey::new(Key::Char('`'), RealmMods::NONE));
        assert!(matches!(m.top_modal(), Some(Id::JumpPicker)));
        assert_eq!(m.jump_choices.len(), 2);
    }

    /// The whole point of #171: the switcher is reachable from inside an
    /// agent terminal via the `]]` leader (`]]` then `` ` ``), without
    /// first leaving the terminal.
    #[test]
    fn terminal_leader_backtick_opens_jump_picker() {
        let mut m = build_model();
        seed_two(&mut m);
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        m.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        assert!(m.terminal_leader_pending(), "`]]` arms the leader");
        m.dispatch_key(RealmKey::new(Key::Char('`'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert!(matches!(m.top_modal(), Some(Id::JumpPicker)));
    }

    /// Picking a row lands the sidebar cursor on that workspace and pops
    /// the modal.
    #[test]
    fn picking_a_target_moves_the_cursor() {
        let mut m = build_model();
        let (_a, bk) = seed_two(&mut m);
        m.mount_jump_picker();
        let idx = m
            .jump_choices
            .iter()
            .position(|k| *k == bk)
            .expect("seeded workspace is a jump target");
        m.handle_choice_picked(vec![idx]);
        assert!(m.top_modal().is_none(), "modal popped after the pick");
        assert_eq!(m.sidebar.selected_workspace_key(), Some(&bk));
    }

    /// With nothing tracked the picker refuses to mount (a footer hint
    /// fires instead) — no empty modal.
    #[test]
    fn no_workspaces_does_not_mount() {
        let mut m = build_model();
        m.mount_jump_picker();
        assert!(m.top_modal().is_none());
        assert!(m.jump_choices.is_empty());
    }
}

#[cfg(test)]
mod terminal_section_dispatch_tests {
    //! #188: the terminal-pane actions must actually fire under terminal
    //! focus — `available_in_terminal()` is only a `section == Terminal`
    //! proxy, so these round-trip each `Section::Terminal` action through
    //! `handle_pane_key` under `PaneFocus::Terminals` to prove the
    //! proxy's premise. They also pin the central #188 finding: the leave
    //! chord is owned by `ui.terminal_escape_char`, NOT a remappable
    //! `leave_terminal` catalog chord the footer must never advertise.
    use super::super::*;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use lazybox_tui_core::action::{ActionDef, ActionKind, Section};
    use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};
    use tuirealm::ratatui::layout::Size;

    /// A model focused on a live (non-empty) terminal — so dispatch takes
    /// the real terminal-focus path (`resolve_focus` is `None`), not the
    /// empty-pane fallback that resolves keys as if the sidebar held
    /// focus.
    fn model_in_live_terminal() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let key = SessionKey::from("github:o/r#1");
        m.terminals.set_active_session(Some(key.clone()));
        m.terminals.on_daemon_event(&IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: key,
            kind: TerminalKind::Shell,
            no_permission: false,
        });
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m
    }

    fn esc_char(m: &Model<tuirealm::terminal::TestTerminalAdapter>) -> RealmKey {
        RealmKey::new(
            Key::Char(m.ui_defaults.terminal_escape_char),
            RealmMods::NONE,
        )
    }

    /// The leave chord is the escape char doubled — that's what the
    /// dispatcher matches. A baked-in `leave_terminal: Esc` override does
    /// NOT leave: the catalog chord is never consulted under terminal
    /// focus, so honoring it in the footer would be a lie.
    #[test]
    fn leave_terminal_override_does_not_leave_under_terminal_focus() {
        let mut m = model_in_live_terminal();
        let mut ov = std::collections::BTreeMap::new();
        ov.insert("leave_terminal".to_string(), "Esc".to_string());
        m.apply_action_key_overrides(ov);

        m.dispatch_key(RealmKey::new(Key::Esc, RealmMods::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Terminals,
            "a leave_terminal rebind must not leave; the escape char owns the chord",
        );
    }

    /// The escape char doubled leaves even with the `leave_terminal`
    /// override present — proving `ui.terminal_escape_char` is the chord
    /// owner. Uses a 1ms window so the idle tick fires without sleeping
    /// the default escape window.
    #[test]
    fn escape_char_doubled_leaves_regardless_of_override() {
        let mut m = model_in_live_terminal();
        m.ui_defaults.escape_window = std::time::Duration::from_millis(1);
        let mut ov = std::collections::BTreeMap::new();
        ov.insert("leave_terminal".to_string(), "Esc".to_string());
        m.apply_action_key_overrides(ov);

        m.dispatch_key(esc_char(&m));
        m.dispatch_key(esc_char(&m));
        assert!(
            m.terminal_leader_pending(),
            "the escape char doubled arms the leader"
        );
        std::thread::sleep(std::time::Duration::from_millis(3));
        m.tick_terminal_leader();
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "escape char doubled is the way out, override or not",
        );
    }

    /// The scroll chord (`Shift-PageUp`) is consumed by the terminal pane
    /// under focus rather than leaving or falling through to the catalog.
    #[test]
    fn terminal_scroll_chord_stays_in_the_pane() {
        let mut m = model_in_live_terminal();
        m.dispatch_key(RealmKey::new(Key::PageUp, RealmMods::SHIFT));
        assert_eq!(
            m.focus(),
            PaneFocus::Terminals,
            "scrolling scrollback must not leave the terminal",
        );
        assert!(m.top_modal().is_none(), "scroll opens no modal");
    }

    /// Every `Section::Terminal` action is accounted for by a dispatch
    /// round-trip above — so the `available_in_terminal()` proxy can't
    /// claim an action fires here without a test that actually exercises
    /// it. A new Terminal action forces a new arm (and its dispatch
    /// test).
    #[test]
    fn every_terminal_section_action_has_a_dispatch_path() {
        for def in ActionDef::all() {
            if def.section != Section::Terminal {
                continue;
            }
            match def.kind {
                // Exercised by the escape-char dispatch tests above.
                ActionKind::LeaveTerminal => {}
                // Exercised by `terminal_scroll_chord_stays_in_the_pane`.
                ActionKind::TerminalScroll => {}
                other => panic!(
                    "Section::Terminal action {other:?} has no dispatch round-trip test (#188)",
                ),
            }
        }
    }
}

#[cfg(test)]
mod spawn_spinner_projection_tests {
    //! #206: the footer spawn spinner is a projection of the live
    //! terminal set — it clears the instant a matching terminal exists,
    //! even when no `TerminalSpawned`/`TerminalFocusRequested` clear
    //! event reaches the model for that spawn.
    use super::super::*;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = lazybox_ipc::channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    #[test]
    fn projection_clears_spinner_without_a_spawn_event() {
        let mut m = build_model();
        let sk = SessionKey::new("github:o/r#1");
        // The agent terminal already exists (e.g. the spawn collapsed
        // onto an existing runner — the "terminal already existed" stuck
        // case the issue calls out).
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(3),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
        });
        // Light the spinner for that same target.
        m.status.note_spawning(
            "claude",
            sk.clone(),
            TerminalKind::Agent("claude".into()),
            1,
        );
        assert!(m.status.spawning.is_some());

        // A NON-spawn event drives `handle_daemon_event`; there is no
        // explicit clear path for it, yet the spinner clears because it
        // is recomputed from the live terminal set.
        m.handle_daemon_event(IpcEvent::PollCompleted {
            source: "github".into(),
            count: 0,
        });
        assert!(
            m.status.spawning.is_none(),
            "projection cleared the spinner without a matching spawn event",
        );
    }

    #[test]
    fn idle_tick_backstops_the_projection() {
        let mut m = build_model();
        let sk = SessionKey::new("github:o/r#1");
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(5),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
        });
        // Shell spawn whose baseline (0) is below the current count (1).
        m.status
            .note_spawning("shell", sk.clone(), TerminalKind::Shell, 0);
        assert!(m.status.spawning.is_some());

        // No further daemon events — the idle tick alone clears it.
        let _ = m.polling_tick();
        assert!(
            m.status.spawning.is_none(),
            "idle-tick backstop cleared the spinner",
        );
    }

    #[test]
    fn spinner_stays_lit_until_its_own_terminal_lands() {
        let mut m = build_model();
        let target = SessionKey::new("github:o/r#1");
        m.status.note_spawning(
            "claude",
            target.clone(),
            TerminalKind::Agent("claude".into()),
            0,
        );

        // A terminal for an UNRELATED workspace must not clear our
        // spinner (the old "any TerminalSpawned clears it" behavior).
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(8),
            session_key: SessionKey::new("github:o/r#2"),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
        });
        assert!(
            m.status.spawning.is_some(),
            "an unrelated spawn must not clear our spinner",
        );

        // Our target's terminal lands → cleared.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(9),
            session_key: target,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
        });
        assert!(
            m.status.spawning.is_none(),
            "spinner cleared by its own terminal"
        );
    }
}

#[cfg(test)]
mod worktree_progress_recovery_tests {
    //! Issue #219 — the "Setting up workspace" checklist hung forever on
    //! "Cloning repository". A broadcast-lag recovery `Snapshot` stands
    //! in for the events the client missed, which can include both the
    //! per-stage `WorktreeProgress` updates AND the one-shot
    //! `TerminalSpawned` that dismisses the modal. With all of those
    //! dropped, the checklist never advanced past its first step and
    //! never closed, even though the spawn had completed. The snapshot
    //! reconciliation tears the stuck modal down once it shows the
    //! session's terminal is live.
    use super::super::{Id, Model};
    use chrono::Utc;
    use lazybox_core::{Workspace, WorkspaceKey};
    use lazybox_ipc::{
        Event as IpcEvent, TerminalId, TerminalKind, TerminalSnapshot, WorktreeStep,
        WorktreeStepStatus, channel,
    };
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn terminal_snapshot(session_key: lazybox_core::SessionKey) -> TerminalSnapshot {
        TerminalSnapshot {
            terminal_id: TerminalId(7),
            session_key,
            kind: TerminalKind::Agent("claude".into()),
            replay: Vec::new(),
            last_seq: 0,
            no_permission: false,
            last_user_message: None,
        }
    }

    #[test]
    fn lag_recovery_snapshot_dismisses_stuck_checklist() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:mind-build/mind#1");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key.clone(), "main", Utc::now())],
            terminals: vec![],
            projects: vec![],
        });

        // Provisioning starts — the checklist mounts on "Cloning
        // repository". Then the client lags: the fetch/worktree-add/setup
        // updates and the `TerminalSpawned` are all dropped.
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::Clone,
            status: WorktreeStepStatus::Started,
        });
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "checklist must be up after the first progress event",
        );

        // The recovery snapshot the daemon sends in place of the missed
        // events shows the spawn finished: the session now has a live
        // terminal.
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key.clone(), "main", Utc::now())],
            terminals: vec![terminal_snapshot(session_key.clone())],
            projects: vec![],
        });
        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "a recovery snapshot showing the live terminal must dismiss the stuck checklist",
        );
        assert!(
            m.worktree_progress.is_none(),
            "checklist state must be cleared once dismissed",
        );
    }

    #[test]
    fn snapshot_without_the_session_terminal_leaves_checklist_up() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:mind-build/mind#1");
        let session_key: lazybox_core::SessionKey = (&key).into();
        let other: lazybox_core::SessionKey =
            (&WorkspaceKey::new("github:mind-build/mind#2")).into();

        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key,
            step: WorktreeStep::Clone,
            status: WorktreeStepStatus::Started,
        });

        // A snapshot whose live terminals are for OTHER sessions says
        // nothing about this spawn — the checklist must stay up.
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key, "main", Utc::now())],
            terminals: vec![terminal_snapshot(other)],
            projects: vec![],
        });
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "an unrelated snapshot must not tear down an in-flight checklist",
        );
    }

    #[test]
    fn snapshot_does_not_dismiss_a_failed_checklist() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:mind-build/mind#1");
        let session_key: lazybox_core::SessionKey = (&key).into();

        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::Clone,
            status: WorktreeStepStatus::Started,
        });
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::Clone,
            status: WorktreeStepStatus::Failed("fatal: could not read from remote".into()),
        });

        // Even with a live terminal in the snapshot, a checklist frozen on
        // an error stays up so the user can read it and press Esc.
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key, "main", Utc::now())],
            terminals: vec![terminal_snapshot(session_key)],
            projects: vec![],
        });
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "a failed checklist must survive a recovery snapshot",
        );
    }
}
