// Tests may block (sleeps to cross latch windows, thread joins); the
// crate-wide blocking-call ban in clippy.toml targets the run loop.
#![allow(clippy::disallowed_methods)]

/// Serializes tests that mutate the process-global `LAZYBOX_HOME` so a
/// parallel test can't observe another's temp home (or the real one).
/// Shared across every test module in this binary — a per-module lock
/// would let two modules' mutators race. Held for the whole body of
/// each such test.
#[cfg(test)]
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod prompt_history_format_tests {
    //! Formatting helpers for the `]]h` prompt-history rows (#523).
    use super::super::{relative_age, summarize_prompt};

    #[test]
    fn relative_age_buckets_by_magnitude() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_age(now, now), "just now");
        assert_eq!(relative_age(now - 30_000, now), "just now");
        assert_eq!(relative_age(now - 120_000, now), "2m ago");
        assert_eq!(relative_age(now - 3 * 3_600_000, now), "3h ago");
        assert_eq!(relative_age(now - 5 * 86_400_000, now), "5d ago");
        // Clock skew (future timestamp) collapses to "just now".
        assert_eq!(relative_age(now + 5_000, now), "just now");
    }

    #[test]
    fn relative_age_zero_is_the_migrated_marker() {
        assert_eq!(relative_age(0, 1_000_000_000_000), "earlier");
    }

    #[test]
    fn summarize_prompt_collapses_whitespace_and_newlines() {
        assert_eq!(
            summarize_prompt("fix bug in foo.rs\nand   retry"),
            "fix bug in foo.rs and retry",
        );
    }
}

#[cfg(test)]
mod agent_auth_recovery_tests {
    use super::super::*;
    use lazybox_ipc::{Command, Event, TerminalId, channel};
    use tuirealm::ratatui::layout::{Rect, Size};
    use tuirealm::ratatui::{Terminal, backend::TestBackend};

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(100, 30)).expect("model init")
    }

    fn rendered_auth_modal(model: &mut Model<tuirealm::terminal::TestTerminalAdapter>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                model
                    .app
                    .view(&Id::AgentAuth, frame, Rect::new(0, 0, 100, 20))
            })
            .expect("render auth modal");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|col| buffer[(col, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn auth_required_warns_about_other_provider_sessions_and_confirms() {
        let mut model = build_model();
        // A non-isolated provider still warns that re-auth is machine-wide.
        model.handle_daemon_event(Event::AgentAuthRequired {
            terminal_id: TerminalId(7),
            agent_id: "claude".into(),
            display_name: "Claude Code".into(),
            reason: "provider sign-in expired".into(),
            other_session_count: 2,
            credentials_isolated: false,
        });

        assert_eq!(model.top_modal(), Some(&Id::AgentAuth));
        let screen = rendered_auth_modal(&mut model)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(screen.contains("Claude Code authentication is no longer valid"));
        // Wrapping can insert the modal border between words, so assert on
        // fragments that each stay on one line.
        assert!(
            screen.contains("machine-wide Claude Code login")
                && screen.contains("may affect 2 other running")
                && screen.contains("sessions."),
            "{screen}"
        );
        assert!(screen.contains("Sign in and continue"));
        assert!(matches!(
            model.handle_confirmed(true).as_slice(),
            [Command::ReauthenticateAgent {
                terminal_id: TerminalId(7),
                switch_account: true,
            }]
        ));
    }

    #[test]
    fn isolated_auth_required_drops_the_machine_wide_cascade_warning() {
        let mut model = build_model();
        // An isolated provider (Codex → per-session `CODEX_HOME`) never
        // cascades, so the modal reassures instead of warning.
        model.handle_daemon_event(Event::AgentAuthRequired {
            terminal_id: TerminalId(7),
            agent_id: "codex".into(),
            display_name: "Codex".into(),
            reason: "provider sign-in expired".into(),
            other_session_count: 0,
            credentials_isolated: true,
        });

        let screen = rendered_auth_modal(&mut model)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(screen.contains("Only this agent is affected"), "{screen}");
        assert!(!screen.contains("machine-wide"), "{screen}");
        assert!(!screen.contains("other running"), "{screen}");
    }

    #[test]
    fn auth_prompt_escape_leaves_the_pane_untouched() {
        let mut model = build_model();
        model.handle_daemon_event(Event::AgentAuthRequired {
            terminal_id: TerminalId(8),
            agent_id: "claude".into(),
            display_name: "Claude Code".into(),
            reason: "provider sign-in expired".into(),
            other_session_count: 0,
            credentials_isolated: false,
        });

        assert!(model.handle_modal_dismissed().is_empty());
        assert!(model.top_modal().is_none());
    }

    #[test]
    fn failed_login_offers_retry_without_losing_recovery_identity() {
        let mut model = build_model();
        model.handle_daemon_event(Event::AgentAuthFinished {
            recovery_terminal_id: TerminalId(9),
            terminal_id: TerminalId(9),
            display_name: "Claude Code".into(),
            success: false,
            error: Some("provider login exited with status 1".into()),
        });

        let screen = rendered_auth_modal(&mut model)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(screen.contains("Claude Code sign-in did not complete"));
        assert!(screen.contains("conversation is still saved"));
        assert!(screen.contains("Retry"));
        assert!(matches!(
            model.handle_confirmed(true).as_slice(),
            [Command::ReauthenticateAgent {
                terminal_id: TerminalId(9),
                switch_account: true,
            }]
        ));
    }
}

#[cfg(test)]
mod editor_notice_tests {
    use super::super::opened_file_notice;
    use crate::editors::OpenFileOutcome;
    use std::path::Path;

    #[test]
    fn file_open_notice_reports_an_applied_location() {
        assert_eq!(
            opened_file_notice(
                Path::new("/repo/src/main.rs"),
                "PyCharm",
                OpenFileOutcome::OpenedAt {
                    line: 12,
                    column: Some(3),
                },
            ),
            "opened /repo/src/main.rs:12:3 in PyCharm"
        );
    }

    #[test]
    fn file_open_notice_discloses_an_unsupported_app_location() {
        assert_eq!(
            opened_file_notice(
                Path::new("/repo/src/main.rs"),
                "PyCharm",
                OpenFileOutcome::OpenedWithoutLocation {
                    line: 12,
                    column: Some(3),
                },
            ),
            "opened /repo/src/main.rs in PyCharm (line 12:3 unavailable via macOS app launch)"
        );
    }
}

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

    /// `c` in the Error Inbox wipes a *durable* store, so it must not fire
    /// on the keypress — it mounts a confirm, and only an explicit Yes
    /// emits `ClearErrors`. Without the gate a stray `c` erased all triage
    /// history irreversibly.
    #[test]
    fn error_inbox_clear_is_gated_by_a_confirm() {
        use lazybox_ipc::Command;
        let mut m = build_model();
        m.mount_error_inbox();
        assert_eq!(m.top_modal(), Some(&Id::ErrorInbox));

        // Pressing `c` mounts the confirm gate; nothing is sent yet.
        m.update(Msg::ErrorInboxClearRequested);
        assert_eq!(
            m.top_modal(),
            Some(&Id::ErrorInboxClearConfirm),
            "clear must be confirmed, not immediate",
        );
        // Declining sends no command and returns to the inbox.
        assert!(m.handle_confirmed(false).is_empty());
        assert_eq!(m.top_modal(), Some(&Id::ErrorInbox));

        // Confirming is the only path that wipes the store.
        m.update(Msg::ErrorInboxClearRequested);
        assert!(matches!(
            m.handle_confirmed(true).as_slice(),
            [Command::ClearErrors]
        ));
    }

    #[test]
    fn diff_review_uses_the_settle_gated_agent_injection_path() {
        use crate::realm::components::diff_review::DiffReviewComment;
        use lazybox_core::Workspace;
        use lazybox_ipc::{TerminalKind, UserPrompt};

        let mut model = build_model();
        let workspace_key = WorkspaceKey::new("github:o/r#1");
        let session_key: SessionKey = (&workspace_key).into();
        model.handle_daemon_event(lazybox_ipc::Event::WorkspaceUpserted(Box::new(
            Workspace::empty(workspace_key.clone(), "review", chrono::Utc::now()),
        )));
        model.handle_daemon_event(lazybox_ipc::Event::TerminalSpawned {
            terminal_id: lazybox_ipc::TerminalId(7),
            session_key,
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });

        let commands = model.dispatch_diff_review(
            workspace_key,
            lazybox_ipc::WorkspaceDiffTarget::Session(lazybox_core::SessionId::new()),
            vec![lazybox_ipc::TerminalId(7)],
            vec![DiffReviewComment {
                path: "src/lib.rs".into(),
                old_line: None,
                new_line: Some(11),
                hunk_header: "@@ -10 +10,2 @@ fn run()".into(),
                referenced_line: "+fix();".into(),
                context: vec![" keep();".into(), "+fix();".into()],
                body: "rename this helper".into(),
                anchor_row: 3,
            }],
        );

        assert!(
            matches!(
                commands.as_slice(),
                [
                    IpcCommand::RecordUserMessage {
                        terminal_id: lazybox_ipc::TerminalId(7),
                        prompt: UserPrompt { text, .. },
                    },
                    IpcCommand::InjectPrompt {
                        terminal_id: lazybox_ipc::TerminalId(7),
                        prompt,
                        submit: true,
                        fallback_spawn: None,
                    }
                ] if text.contains("src/lib.rs:11")
                    && text.contains("rename this helper")
                    && prompt.contains("src/lib.rs:11")
                    && prompt.contains("rename this helper")
            ),
            "unexpected review commands: {commands:#?}"
        );
    }

    #[test]
    fn diff_review_targets_the_agent_resolved_for_the_inspected_session() {
        use crate::realm::components::diff_review::DiffReviewComment;
        use lazybox_core::Workspace;
        use lazybox_ipc::{TerminalKind, WorkspaceDiffTarget};

        let mut model = build_model();
        let workspace_key = WorkspaceKey::new("github:o/r#1");
        let session_key: SessionKey = (&workspace_key).into();
        model.handle_daemon_event(lazybox_ipc::Event::WorkspaceUpserted(Box::new(
            Workspace::empty(workspace_key.clone(), "review", chrono::Utc::now()),
        )));
        for terminal_id in [8, 7] {
            model.handle_daemon_event(lazybox_ipc::Event::TerminalSpawned {
                terminal_id: lazybox_ipc::TerminalId(terminal_id),
                session_key: session_key.clone(),
                kind: TerminalKind::Agent("codex".into()),
                no_permission: false,
                on_main: false,
                model_label: None,
            });
        }

        let commands = model.dispatch_diff_review(
            workspace_key,
            WorkspaceDiffTarget::Session(lazybox_core::SessionId::new()),
            vec![lazybox_ipc::TerminalId(8)],
            vec![DiffReviewComment {
                path: "src/lib.rs".into(),
                old_line: None,
                new_line: Some(11),
                hunk_header: "@@ -10 +10,2 @@ fn run()".into(),
                referenced_line: "+fix();".into(),
                context: vec!["+fix();".into()],
                body: "fix this".into(),
                anchor_row: 3,
            }],
        );

        assert!(commands.iter().all(|command| match command {
            IpcCommand::RecordUserMessage { terminal_id, .. }
            | IpcCommand::InjectPrompt { terminal_id, .. } =>
                *terminal_id == lazybox_ipc::TerminalId(8),
            _ => true,
        }));
        assert_eq!(commands.len(), 2);
    }

    #[test]
    fn view_diff_requests_the_selected_worktree_and_mounts_its_response() {
        use lazybox_core::{SessionKind, Workspace, WorkspaceSession};
        use lazybox_ipc::WorkspaceDiffDto;
        use lazybox_tui_core::action::Action;

        let mut model = build_model();
        let workspace_key = WorkspaceKey::new("github:o/r#1");
        let mut workspace = Workspace::empty(workspace_key.clone(), "review", chrono::Utc::now());
        workspace.sessions.push(WorkspaceSession::new(
            workspace_key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            "/tmp/review-a".into(),
            chrono::Utc::now(),
        ));
        let session = WorkspaceSession::new(
            workspace_key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            "/tmp/review-b".into(),
            chrono::Utc::now(),
        );
        let session_id = session.id;
        workspace.sessions.push(session);
        model.handle_daemon_event(lazybox_ipc::Event::WorkspaceUpserted(Box::new(workspace)));
        assert!(model.sidebar.focus_session_id(session_id));

        let commands = model.dispatch_action(&Action::ViewDiff);
        assert!(matches!(
            commands.as_slice(),
            [IpcCommand::InspectWorkspaceDiff {
                workspace_key: target,
                target: lazybox_ipc::WorkspaceDiffTarget::Session(target_session),
            }] if target == &workspace_key && target_session == &session_id
        ));
        assert_eq!(
            model.pending_diff_session.as_ref(),
            Some(&(
                workspace_key.clone(),
                lazybox_ipc::WorkspaceDiffTarget::Session(session_id)
            ))
        );

        model.handle_daemon_event(lazybox_ipc::Event::WorkspaceDiffInspected {
            workspace_key,
            target: lazybox_ipc::WorkspaceDiffTarget::Session(session_id),
            agent_terminal_ids: vec![lazybox_ipc::TerminalId(7)],
            diff: Some(WorkspaceDiffDto {
                status: Vec::new(),
                stat: Vec::new(),
                files: Vec::new(),
                truncated: false,
            }),
            error: None,
        });
        assert_eq!(model.modal_stack.last(), Some(&Id::DiffReview));
        assert!(model.pending_diff_session.is_none());
    }

    #[test]
    fn view_diff_uses_the_workspace_default_session_when_no_session_row_is_selected() {
        use lazybox_core::{SessionKind, Workspace, WorkspaceSession};
        use lazybox_tui_core::action::Action;

        let mut model = build_model();
        let workspace_key = WorkspaceKey::new("github:o/r#1");
        let now = chrono::Utc::now();
        let mut workspace = Workspace::empty(workspace_key.clone(), "review", now);
        let older = WorkspaceSession::new(
            workspace_key.clone(),
            SessionKind::Shell,
            "/tmp/review-older".into(),
            now - chrono::Duration::minutes(1),
        );
        let newer = WorkspaceSession::new(
            workspace_key.clone(),
            SessionKind::Shell,
            "/tmp/review-newer".into(),
            now,
        );
        let newer_id = newer.id;
        workspace.sessions.extend([older, newer]);
        model.handle_daemon_event(lazybox_ipc::Event::WorkspaceUpserted(Box::new(workspace)));
        let session_key: SessionKey = (&workspace_key).into();
        assert!(model.sidebar.focus_workspace_key(&session_key));

        let commands = model.dispatch_action(&Action::ViewDiff);

        assert!(matches!(
            commands.as_slice(),
            [IpcCommand::InspectWorkspaceDiff {
                target: lazybox_ipc::WorkspaceDiffTarget::Session(session_id),
                ..
            }] if *session_id == newer_id
        ));
    }

    #[test]
    fn view_diff_targets_a_linked_checkout_without_sessions() {
        use lazybox_core::Workspace;
        use lazybox_tui_core::action::Action;

        let mut model = build_model();
        let workspace_key = WorkspaceKey::new("github:o/r#1");
        let mut workspace =
            Workspace::empty(workspace_key.clone(), "linked review", chrono::Utc::now());
        workspace.linked_checkout = Some("/tmp/linked-review".into());
        model.handle_daemon_event(lazybox_ipc::Event::WorkspaceUpserted(Box::new(workspace)));
        let session_key: SessionKey = (&workspace_key).into();
        assert!(model.sidebar.focus_workspace_key(&session_key));

        let commands = model.dispatch_action(&Action::ViewDiff);

        assert!(matches!(
            commands.as_slice(),
            [IpcCommand::InspectWorkspaceDiff {
                workspace_key: target,
                target: lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout,
            }] if target == &workspace_key
        ));
    }

    /// Reply submission with a non-empty body + a pending reply
    /// target produces `PostReply` followed by `Refresh` (in that
    /// order — the Refresh kicks an immediate poll instead of
    /// waiting on the 60s loop).
    #[test]
    fn textarea_submitted_with_pending_reply_returns_postreply_then_refresh() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");
        m.modal_flow = Some(super::super::ModalFlow::Reply {
            target: key.clone(),
        });
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

    /// Notes share the Textarea component with Reply/Broadcast, so the
    /// submit handler routes on the modal id that was on top. A
    /// non-empty note persists via `SetNotes` and clears the pending
    /// target (issue #458).
    #[test]
    fn textarea_submitted_notes_persists_setnotes() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");
        m.modal_flow = Some(super::super::ModalFlow::Notes {
            target: key.clone(),
        });
        m.modal_stack.push(Id::Notes);
        let cmds = m.handle_textarea_submitted("check the flaky retry".into());
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::SetNotes { session_key, notes } => {
                assert_eq!(session_key, &key);
                assert_eq!(notes, "check the flaky retry");
            }
            other => panic!("expected SetNotes, got {other:?}"),
        }
        assert!(m.modal_flow.is_none());
    }

    /// An empty/whitespace note is a valid submit — it clears the
    /// scratchpad — so it still emits `SetNotes` rather than being
    /// dropped the way an empty reply is.
    #[test]
    fn textarea_submitted_empty_notes_clears_scratchpad() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");
        m.modal_flow = Some(super::super::ModalFlow::Notes {
            target: key.clone(),
        });
        m.modal_stack.push(Id::Notes);
        let cmds = m.handle_textarea_submitted("   ".into());
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::SetNotes { session_key, notes } => {
                assert_eq!(session_key, &key);
                assert!(notes.trim().is_empty());
            }
            other => panic!("expected SetNotes, got {other:?}"),
        }
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
        // `exhausted` (retries run out) is the actionable failure that
        // arms the sticky banner; a live `retryable` transient stays quiet
        // (#730).
        m.handle_daemon_event(IpcEvent::ProviderError {
            source: source.into(),
            message: "boom".into(),
            detail: String::new(),
            kind: "exhausted".into(),
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

    /// An empty daemon snapshot with no dismissed targets.
    fn empty_snapshot() -> lazybox_ipc::Event {
        lazybox_ipc::Event::Snapshot {
            workspaces: Vec::new(),
            terminals: Vec::new(),
            projects: Vec::new(),
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        }
    }

    fn release_update(available: &str) -> crate::build_guard::AvailableUpdate {
        crate::build_guard::AvailableUpdate::Release {
            current: "v0.1.7".into(),
            available: available.into(),
            install: crate::build_guard::ReleaseInstall::Homebrew,
        }
    }

    // The update modal waits for the first snapshot so its dismissal check
    // runs against the daemon's authoritative set, not an empty one (#548).
    #[test]
    fn update_modal_defers_until_the_first_snapshot() {
        let (client, _server) = lazybox_ipc::channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model");

        m.show_update_if_new(release_update("v0.2.0"));
        assert!(
            m.top_modal().is_none(),
            "no modal before the dismissal set is known"
        );

        m.handle_daemon_event(empty_snapshot());
        assert_eq!(
            m.top_modal(),
            Some(&Id::Update),
            "the snapshot releases the stashed update"
        );
    }

    // Dismissing routes through the daemon (`SetUpdateDismissal`) instead of
    // a client-local store write, so it sticks across clients/restarts (#548).
    #[test]
    fn update_dismissal_routes_through_the_daemon() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let (client, mut server) = lazybox_ipc::channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model");
        m.handle_daemon_event(empty_snapshot());
        while server.rx.try_recv().is_ok() {} // drain Subscribe + snapshot side effects

        m.show_update_if_new(release_update("v0.2.0"));
        assert_eq!(m.top_modal(), Some(&Id::Update));
        m.dispatch_modal_key(KeyEvent::new(Key::Enter, KeyModifiers::NONE));
        assert!(m.top_modal().is_none(), "Enter dismisses the modal");

        let mut dismissal = None;
        while let Ok(cmd) = server.rx.try_recv() {
            if let IpcCommand::SetUpdateDismissal { target } = cmd {
                dismissal = Some(target);
            }
        }
        assert_eq!(
            dismissal.as_deref(),
            Some("release:v0.2.0"),
            "dismissal is reported to the daemon"
        );

        // The local echo keeps the same target quiet this session without
        // waiting on the next snapshot.
        m.show_update_if_new(release_update("v0.2.0"));
        assert!(m.top_modal().is_none(), "the dismissed target stays quiet");

        // A newer target is not covered by the older dismissal.
        m.show_update_if_new(release_update("v0.3.0"));
        assert_eq!(m.top_modal(), Some(&Id::Update));
    }

    // A target the daemon already reports as dismissed never re-mounts.
    #[test]
    fn dismissed_target_from_the_snapshot_stays_quiet() {
        let (client, _server) = lazybox_ipc::channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model");
        m.handle_daemon_event(lazybox_ipc::Event::Snapshot {
            workspaces: Vec::new(),
            terminals: Vec::new(),
            projects: Vec::new(),
            recent_snippets: Vec::new(),
            dismissed_updates: vec!["release:v0.2.0".into()],
        });

        m.show_update_if_new(release_update("v0.2.0"));
        assert!(m.top_modal().is_none(), "dismissed target stays quiet");

        m.show_update_if_new(release_update("v0.3.0"));
        assert_eq!(
            m.top_modal(),
            Some(&Id::Update),
            "a fresh target still shows"
        );
    }

    /// Issue #265: a `g m` merge GitHub rejected must surface as a
    /// distinct, persistent error (Permanent severity → never
    /// auto-fades) naming the reason, not a self-fading retryable
    /// flash. The PR stays actionable, so no optimistic MERGED flip.
    #[test]
    fn pr_merge_failed_raises_a_persistent_error_naming_the_reason() {
        use crate::realm::components::footer::NoticeSeverity;
        use lazybox_ipc::Event as IpcEvent;

        let mut m = build_model();
        m.status.polling = None;

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
            reason: "Required status check \"ci\" is expected".into(),
            conflict: false,
        });

        let n = m.status.notice.as_ref().expect("merge-failed banner set");
        assert_eq!(
            n.severity,
            NoticeSeverity::Permanent,
            "a failed merge is a prominent, not-transient error",
        );
        assert!(n.message.contains("merge failed"), "message: {}", n.message);
        // The label is trimmed to just `#1` (the reason, not the
        // owner/repo prefix, is what the footer's truncation must keep).
        assert!(n.message.contains("#1"), "message: {}", n.message);
        // The reason leads, ahead of the label (#588).
        assert!(
            n.message.contains("Required status check"),
            "the GitHub reason must be quoted: {}",
            n.message,
        );
        assert!(
            n.message.find("Required status check") < n.message.find("#1"),
            "reason must lead, label trails: {}",
            n.message,
        );
    }

    /// The ` [at mergePullRequest]` GraphQL-path suffix GitHub's error
    /// text carries is noise that, mid-truncation, survived while the
    /// real reason was elided (#588). It must be stripped, leaving the
    /// human message verbatim.
    #[test]
    fn merge_failed_strips_graphql_path_suffix() {
        use lazybox_ipc::Event as IpcEvent;

        let mut m = build_model();
        m.status.polling = None;

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
            reason: "A merge is already in progress [at mergePullRequest]".into(),
            conflict: false,
        });

        let n = m.status.notice.as_ref().expect("merge-failed banner");
        assert!(
            n.message.contains("A merge is already in progress"),
            "human reason must survive: {}",
            n.message,
        );
        assert!(
            !n.message.contains("[at "),
            "GraphQL-path noise must be stripped: {}",
            n.message,
        );
    }

    /// A "merge failed" banner is tagged with its workspace, so a later
    /// `PrMerged` for that same workspace self-clears the stale error
    /// and the success notice shows in its place (#588).
    #[test]
    fn pr_merged_clears_a_stale_merge_failed_banner() {
        use lazybox_ipc::Event as IpcEvent;

        let mut m = build_model();
        m.status.polling = None;
        let ws = lazybox_core::WorkspaceKey::new("github:o/r#1");

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: ws.clone(),
            pr_label: "o/r#1".into(),
            reason: "base branch was modified".into(),
            conflict: false,
        });
        assert!(m.status.notice.is_some(), "error banner is up");

        // The same PR later merges (retry, auto-merge, or a poll).
        m.handle_daemon_event(IpcEvent::PrMerged {
            workspace_key: ws,
            pr_label: "o/r#1".into(),
        });
        let n = m
            .status
            .notice
            .as_ref()
            .expect("success notice replaces it");
        assert!(
            n.message.contains("merged"),
            "the merge success must show, not the stale error: {}",
            n.message,
        );
    }

    /// Self-clearing is workspace-scoped: a success for a *different*
    /// workspace must not wipe another workspace's failure banner.
    #[test]
    fn pr_merged_leaves_another_workspaces_error_alone() {
        use lazybox_ipc::Event as IpcEvent;

        let mut m = build_model();
        m.status.polling = None;

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
            reason: "not mergeable".into(),
            conflict: false,
        });

        // A different PR merges.
        m.handle_daemon_event(IpcEvent::PrMerged {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            pr_label: "o/r#2".into(),
        });
        let n = m.status.notice.as_ref().expect("banner survives");
        assert!(
            n.message.contains("merge failed"),
            "another workspace's success must not clear this error: {}",
            n.message,
        );
    }

    /// A retried merge that fails the same way must not stack duplicate
    /// rows in the messages log — the footer just refreshes (#588).
    #[test]
    fn repeated_identical_merge_failure_does_not_stack() {
        use lazybox_ipc::Event as IpcEvent;

        let mut m = build_model();
        m.status.polling = None;
        let fail = || IpcEvent::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
            reason: "not mergeable".into(),
            conflict: false,
        };

        m.handle_daemon_event(fail());
        let after_first = m.status.messages.recent().count();
        m.handle_daemon_event(fail());
        m.handle_daemon_event(fail());
        assert_eq!(
            m.status.messages.recent().count(),
            after_first,
            "identical retries must not append duplicate messages-log rows",
        );
    }

    /// Issue #832: dismissing (Esc) a merge-failure toast means "I've
    /// seen this." An identical `PrMergeFailed` re-emitted on the next
    /// poll / auto-merge attempt must stay dismissed — not resurrect the
    /// same red banner every cycle (the never-disappearing footer bug).
    #[test]
    fn dismissed_merge_failure_stays_dismissed_on_identical_refire() {
        use lazybox_ipc::Event as IpcEvent;
        use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};

        let mut m = build_model();
        m.status.polling = None;
        let fail = || IpcEvent::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
            reason: "Required status check \"ci\" is expected".into(),
            conflict: false,
        };

        m.handle_daemon_event(fail());
        assert!(m.status.notice.is_some(), "the first failure surfaces");

        // Esc — the user acknowledges it.
        m.dispatch_key(RealmKey::new(Key::Esc, RealmMods::NONE));
        assert!(m.status.notice.is_none(), "Esc clears the banner");

        // The auto-merge retries and fails identically next poll.
        m.handle_daemon_event(fail());
        assert!(
            m.status.notice.is_none(),
            "an identical dismissed failure must not re-surface (#832)",
        );
    }

    /// A *changed* reason for the same workspace is a new condition, so
    /// it re-surfaces even after the prior reason was dismissed (#832) —
    /// suppression keys on the exact message, never blanket-muting a
    /// workspace.
    #[test]
    fn dismissed_merge_failure_resurfaces_on_changed_reason() {
        use lazybox_ipc::Event as IpcEvent;
        use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};

        let mut m = build_model();
        m.status.polling = None;
        let ws = lazybox_core::WorkspaceKey::new("github:o/r#1");

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: ws.clone(),
            pr_label: "o/r#1".into(),
            reason: "Required status check \"ci\" is expected".into(),
            conflict: false,
        });
        m.dispatch_key(RealmKey::new(Key::Esc, RealmMods::NONE));
        assert!(m.status.notice.is_none(), "first reason dismissed");

        // A different rejection reason arrives.
        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: ws,
            pr_label: "o/r#1".into(),
            reason: "base branch was modified".into(),
            conflict: false,
        });
        let n = m
            .status
            .notice
            .as_ref()
            .expect("a changed reason must re-surface");
        assert!(
            n.message.contains("base branch was modified"),
            "the new reason shows: {}",
            n.message,
        );
    }

    /// A superseding success re-arms the surface: after dismissing a
    /// failure and then the PR merging, a *later* failure with the same
    /// message must show again — the condition genuinely recurred (#832).
    #[test]
    fn success_re_arms_a_dismissed_merge_failure() {
        use lazybox_ipc::Event as IpcEvent;
        use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};

        let mut m = build_model();
        m.status.polling = None;
        let ws = lazybox_core::WorkspaceKey::new("github:o/r#1");
        let fail = || IpcEvent::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
            reason: "not mergeable".into(),
            conflict: false,
        };

        m.handle_daemon_event(fail());
        m.dispatch_key(RealmKey::new(Key::Esc, RealmMods::NONE));

        // The PR later merges — the condition cleared.
        m.handle_daemon_event(IpcEvent::PrMerged {
            workspace_key: ws,
            pr_label: "o/r#1".into(),
        });

        // A brand-new failure with the same wording must not be
        // swallowed by the stale dismissal.
        m.handle_daemon_event(fail());
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("not mergeable")),
            "a recurrence after a success must surface again (#832)",
        );
    }

    /// Suppression is Esc-scoped: without a dismiss, an identical
    /// re-fire still refreshes the live banner (it must not vanish just
    /// because it repeated). Guards against over-suppressing the
    /// still-visible case (#832 / #588).
    #[test]
    fn undismissed_identical_merge_failure_keeps_the_banner() {
        use lazybox_ipc::Event as IpcEvent;

        let mut m = build_model();
        m.status.polling = None;
        let fail = || IpcEvent::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
            reason: "not mergeable".into(),
            conflict: false,
        };

        m.handle_daemon_event(fail());
        m.handle_daemon_event(fail());
        assert!(
            m.status.notice.is_some(),
            "an undismissed repeat keeps the banner up",
        );
    }

    /// Letting an action-error toast auto-fade (its 45s elapses, no Esc)
    /// is the same acknowledgment as dismissing it: an identical re-fire
    /// afterwards must stay quiet, not resurrect the banner on the next
    /// genuine attempt (#832). Before the fix only the Esc path
    /// suppressed, so a faded failure re-shouted — the never-disappearing
    /// footer for anyone who didn't press Esc.
    #[test]
    fn faded_merge_failure_stays_dismissed_on_identical_refire() {
        use lazybox_ipc::Event as IpcEvent;
        use std::time::{Duration, Instant};

        let mut m = build_model();
        m.status.polling = None;
        let fail = || IpcEvent::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
            reason: "not mergeable".into(),
            conflict: false,
        };

        m.handle_daemon_event(fail());
        assert!(m.status.notice.is_some(), "the first failure surfaces");

        // Age the toast past its fade window, then run the fade tick.
        m.status.notice.as_mut().expect("toast up").set_at =
            Instant::now() - Duration::from_secs(120);
        assert!(m.status.tick_notice(), "the toast auto-fades");
        assert!(m.status.notice.is_none(), "faded away");

        // The next identical failure must not re-surface — the fade
        // acknowledged it, exactly as an Esc would have.
        m.handle_daemon_event(fail());
        assert!(
            m.status.notice.is_none(),
            "a faded failure stays dismissed on an identical re-fire (#832)",
        );
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

    /// The sync-error banner is sticky (Permanent), so a routine
    /// lower-severity flash no longer displaces it — the banner (and
    /// its "clear on recovery" tag) stays armed, and the routine
    /// notice lands in the messages log instead. Only an unrelated
    /// notice that actually REPLACES the banner (another sticky)
    /// disarms the tag — otherwise a later poll would wrongly clear
    /// whatever notice is now on screen.
    #[test]
    fn unrelated_notice_disarms_sync_error_tag() {
        use lazybox_ipc::Event as IpcEvent;

        let mut m = model_with_sync_error("github");

        // A routine Info flash is suppressed by the sticky banner:
        // the tag stays armed and recovery still clears the banner.
        m.flash_info("something else happened");
        assert_eq!(
            m.sync_error_source.as_deref(),
            Some("github"),
            "a suppressed flash must leave the banner attribution intact"
        );

        // An unrelated STICKY notice replaces the banner → tag disarms.
        m.flash_error("something sticky happened");
        assert!(
            m.sync_error_source.is_none(),
            "a notice that actually replaces the banner must disarm the tag"
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
        m.modal_flow = Some(super::super::ModalFlow::Reply {
            target: SessionKey::from("github:o/r#1"),
        });
        let cmds = m.handle_textarea_submitted("   ".into());
        assert!(cmds.is_empty());
        assert!(m.modal_flow.is_none());
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
        m.modal_flow = Some(super::super::ModalFlow::NewWorkspaceProject {
            project: pk.clone(),
        });
        let cmds = m.handle_input_submitted("  my-feature  ".into());
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::CreateWorkspace {
                name,
                project_key,
                spawn_agent,
                client_request_id,
            } => {
                assert_eq!(name, "my-feature");
                assert_eq!(project_key, &pk);
                // Default agent is "claude" unless YAML overrides it.
                assert_eq!(spawn_agent.as_deref(), Some("claude"));
                assert!(client_request_id.is_some());
            }
            other => panic!("expected CreateWorkspace, got {other:?}"),
        }
    }

    /// Regression for the silent new-workspace failure: the display name is
    /// not the identity (`Work` may allocate `work-8`). The correlated daemon
    /// acknowledgement must reveal the exact allocated row and arm the
    /// terminal follow before the slow spawn lands.
    #[test]
    fn workspace_created_ack_focuses_allocated_collision_key() {
        let mut m = build_model();
        let project = lazybox_core::ProjectKey::github("AntoineToussaint", "lazybox");
        let decoy_key = lazybox_core::WorkspaceKey::new("decoy");
        let mut decoy =
            lazybox_core::Workspace::empty(decoy_key.clone(), "main", chrono::Utc::now());
        decoy.project_key = Some(project.clone());
        m.handle_daemon_event(lazybox_ipc::Event::WorkspaceUpserted(Box::new(decoy)));
        assert!(
            m.sidebar
                .focus_workspace_key(&lazybox_core::SessionKey::from(&decoy_key))
        );

        m.modal_stack.push(Id::NewWorkspace);
        m.modal_flow = Some(super::super::ModalFlow::NewWorkspaceProject {
            project: project.clone(),
        });
        let commands = m.handle_input_submitted("Work".into());
        let request_id = match commands.as_slice() {
            [
                IpcCommand::CreateWorkspace {
                    client_request_id: Some(request_id),
                    ..
                },
            ] => request_id.clone(),
            other => panic!("expected one correlated CreateWorkspace, got {other:?}"),
        };

        let allocated_key = lazybox_core::WorkspaceKey::new("work-8");
        let mut created =
            lazybox_core::Workspace::empty(allocated_key.clone(), "main", chrono::Utc::now());
        created.name = "Work".into();
        created.project_key = Some(project);
        created.local = true;
        m.handle_daemon_event(lazybox_ipc::Event::WorkspaceUpserted(Box::new(created)));
        assert_eq!(
            m.sidebar
                .selected_workspace()
                .map(|workspace| &workspace.key),
            Some(&decoy_key),
            "a generic upsert must not guess that another client's row is ours",
        );

        m.handle_daemon_event(lazybox_ipc::Event::WorkspaceCreated {
            client_request_id: request_id.clone(),
            workspace_key: allocated_key.clone(),
        });
        let allocated_session = lazybox_core::SessionKey::from(&allocated_key);
        assert_eq!(
            m.sidebar.selected_workspace_key(),
            Some(&allocated_session),
            "the acknowledgement reveals the daemon-allocated row",
        );
        assert_eq!(
            m.spawn_follow_to.as_ref(),
            Some(&allocated_session),
            "the optional agent spawn follows the newly allocated workspace",
        );
        assert!(m.pending_workspace_creates.contains_key(&request_id));

        m.handle_daemon_event(lazybox_ipc::Event::CommandCompleted {
            client_request_id: request_id.clone(),
        });
        assert!(!m.pending_workspace_creates.contains_key(&request_id));
    }

    /// A store/spawn failure carrying our create request id is a permanent,
    /// named UI error and releases the pending request. It cannot disappear
    /// into `/tmp/lazybox.log` only.
    #[test]
    fn workspace_create_failure_is_visible_and_clears_pending_request() {
        let mut m = build_model();
        m.modal_stack.push(Id::NewWorkspace);
        m.modal_flow = Some(super::super::ModalFlow::NewWorkspaceProject {
            project: lazybox_core::ProjectKey::local("project"),
        });
        let commands = m.handle_input_submitted("Broken".into());
        let request_id = match commands.as_slice() {
            [
                IpcCommand::CreateWorkspace {
                    client_request_id: Some(request_id),
                    ..
                },
            ] => request_id.clone(),
            other => panic!("expected one correlated CreateWorkspace, got {other:?}"),
        };

        m.handle_daemon_event(lazybox_ipc::Event::CommandFailed {
            client_request_id: request_id.clone(),
            message: "database is locked".into(),
        });

        assert!(!m.pending_workspace_creates.contains_key(&request_id));
        let notice = m.status.notice.as_ref().expect("failure is surfaced");
        assert!(notice.message.contains("Broken"));
        assert!(notice.message.contains("not created"));
        assert!(notice.message.contains("database is locked"));
    }

    #[test]
    fn workspace_create_send_failure_is_visible_and_clears_pending_request() {
        let (client, server) = channel::pair();
        drop(server);
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let request_id = "create-disconnected".to_string();
        m.pending_workspace_creates.insert(
            request_id.clone(),
            super::super::PendingWorkspaceCreate {
                name: "Disconnected".into(),
                spawn_agent: true,
                workspace_key: None,
            },
        );

        m.dispatch_cmds(vec![IpcCommand::CreateWorkspace {
            name: "Disconnected".into(),
            project_key: lazybox_core::ProjectKey::local("project"),
            spawn_agent: Some("claude".into()),
            client_request_id: Some(request_id.clone()),
        }]);

        assert!(!m.pending_workspace_creates.contains_key(&request_id));
        let notice = m.status.notice.as_ref().expect("failure is surfaced");
        assert!(notice.message.contains("Disconnected"));
        assert!(notice.message.contains("not created"));
        assert!(notice.message.contains("unavailable"));
    }

    /// RenameWorkspace input with a non-empty trimmed name AND a
    /// stashed target produces `RenameWorkspace { session_key, name }`.
    #[test]
    fn input_submitted_for_rename_returns_rename_workspace() {
        let mut m = build_model();
        let target: lazybox_core::SessionKey = "github:o/r#1".into();
        m.modal_stack.push(Id::RenameWorkspace);
        m.modal_flow = Some(super::super::ModalFlow::RenameWorkspace {
            target: target.clone(),
        });
        let cmds = m.handle_input_submitted("  Rate limit spike  ".into());
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::RenameWorkspace { session_key, name } => {
                assert_eq!(session_key, &target);
                assert_eq!(name, "Rate limit spike");
            }
            other => panic!("expected RenameWorkspace, got {other:?}"),
        }
    }

    /// A blank rename submit commits nothing — the row keeps its name.
    #[test]
    fn input_submitted_for_blank_rename_drops() {
        let mut m = build_model();
        m.modal_stack.push(Id::RenameWorkspace);
        m.modal_flow = Some(super::super::ModalFlow::RenameWorkspace {
            target: "github:o/r#1".into(),
        });
        let cmds = m.handle_input_submitted("   ".into());
        assert!(cmds.is_empty(), "blank rename must not emit a command");
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
        m.modal_stack.push(Id::StartAgentProject);
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Project(pk.clone())]);
        assert!(cmds.is_empty(), "picking a project sends no IPC yet");
        assert_eq!(m.modal_stack.last(), Some(&Id::NewWorkspace));
        assert!(matches!(
            &m.modal_flow,
            Some(super::super::ModalFlow::NewWorkspaceProject { project }) if *project == pk
        ));
    }

    /// The async `x p → CreateProject → ProjectUpserted` hand-off
    /// auto-mounts the new-workspace name input, but it must NOT do so
    /// over a modal the user opened during the daemon round-trip:
    /// arming a second `modal_flow` on top of that modal's live
    /// continuation would clobber it (a reply that never posts) or trip
    /// `set_modal_flow`'s double-arm assert. With any modal up,
    /// `mount_new_workspace_input` is a no-op — the project header is
    /// still focused, so the user can press `x n`.
    #[test]
    fn new_workspace_input_does_not_preempt_an_open_modal() {
        let mut m = build_model();
        // A live flow modal — e.g. a reply the user opened while the
        // freshly-created project's ProjectUpserted was in flight.
        let reply_key = lazybox_core::SessionKey::from("github:o/r#1");
        m.modal_flow = Some(super::super::ModalFlow::Reply {
            target: reply_key.clone(),
        });
        m.modal_stack.push(Id::Reply);

        m.mount_new_workspace_input(lazybox_core::ProjectKey::local("proj"));

        // The reply flow and its modal survive untouched; no NewWorkspace
        // input was stacked on top.
        assert_eq!(m.modal_stack.last(), Some(&Id::Reply));
        assert!(matches!(
            &m.modal_flow,
            Some(super::super::ModalFlow::Reply { target }) if *target == reply_key
        ));
    }

    /// `f` mounts the composable filter menu with a row per filter,
    /// and picking rows replaces the sidebar's active set (no IPC —
    /// filtering is client-local).
    #[test]
    fn filter_menu_pick_sets_the_active_filter_set() {
        use crate::components::sidebar::Filter;
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        m.dispatch_action(&Action::OpenFilterMenu);
        assert_eq!(m.modal_stack.last(), Some(&Id::FilterMenu));

        use crate::components::sidebar::FilterEntry;
        let cmds = m.handle_choice_picked(vec![
            ChoicePayload::Filter(FilterEntry::Predicate(Filter::Author)),
            ChoicePayload::Filter(FilterEntry::Predicate(Filter::Pr)),
        ]);
        assert!(cmds.is_empty(), "filtering sends no IPC");
        assert!(m.modal_stack.is_empty(), "menu closes on pick");
        let active: Vec<Filter> = m.sidebar.filters().iter().collect();
        assert_eq!(active, vec![Filter::Author, Filter::Pr]);
    }

    /// An empty pick clears every active filter.
    #[test]
    fn filter_menu_empty_pick_clears_filters() {
        use crate::components::sidebar::Filter;
        let mut m = build_model();
        m.sidebar.set_filters([Filter::Unread]);
        m.mount_filter_menu();
        let cmds = m.handle_choice_picked(vec![]);
        assert!(cmds.is_empty());
        assert!(m.sidebar.filters().is_empty(), "empty pick clears filters");
    }

    /// Reorder-safety — the whole point of the typed-`ChoicePayload`
    /// refactor (#512). A pick resolves by its typed payload, never by a
    /// position into a Model-side shadow list, so a picker rendered in a
    /// different order than its items were built can no longer resolve the
    /// wrong target. Picking the LAST filter in `Filter::ALL` order applies
    /// exactly that filter; the old positional pick of index 0 would have
    /// applied the FIRST filter instead.
    #[test]
    fn typed_pick_resolves_by_payload_not_position() {
        use crate::components::sidebar::Filter;
        let mut m = build_model();
        m.mount_filter_menu();
        use crate::components::sidebar::FilterEntry;
        let last = *Filter::ALL.last().expect("at least one filter");
        assert_ne!(last, Filter::ALL[0], "test needs a non-first filter");
        let cmds =
            m.handle_choice_picked(vec![ChoicePayload::Filter(FilterEntry::Predicate(last))]);
        assert!(cmds.is_empty());
        let active: Vec<Filter> = m.sidebar.filters().iter().collect();
        assert_eq!(
            active,
            vec![last],
            "the payload's filter is applied, not the row at position 0",
        );
    }

    /// `x p` with no tracked repos has nothing to pick, so it
    /// skips the picker and drops straight into the new-project input
    /// — the only way to bootstrap a brand-new, empty inbox.
    #[test]
    fn new_workspace_picker_without_projects_mounts_new_project_input() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        m.dispatch_action(&Action::NewProject);
        assert_eq!(m.modal_stack.last(), Some(&Id::NewProject));
    }

    /// `x p` with tracked repos mounts the repo picker, listing
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
    }

    /// Picking a repo row funnels into the new-workspace name input
    /// under that repo (no project-creation step). The pick sends no
    /// IPC and drains the stashed choices.
    #[test]
    fn new_workspace_repo_pick_funnels_into_name_input() {
        let mut m = build_model();
        let pk = lazybox_core::ProjectKey::github("acme", "widget");
        m.modal_stack.push(Id::NewWorkspaceRepo);
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Project(pk.clone())]);
        assert!(cmds.is_empty(), "picking a repo sends no IPC yet");
        assert_eq!(m.modal_stack.last(), Some(&Id::NewWorkspace));
        assert!(matches!(
            &m.modal_flow,
            Some(super::super::ModalFlow::NewWorkspaceProject { project }) if *project == pk
        ));
    }

    /// Picking the trailing escape-hatch row (index past the repo
    /// list) keeps the brand-new-project path available.
    #[test]
    fn new_workspace_repo_pick_escape_hatch_mounts_new_project() {
        let mut m = build_model();
        m.modal_stack.push(Id::NewWorkspaceRepo);
        // The "create a new local project" escape-hatch row.
        let cmds = m.handle_choice_picked(vec![ChoicePayload::NewLocalProject]);
        assert!(cmds.is_empty());
        assert_eq!(m.modal_stack.last(), Some(&Id::NewProject));
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
        m.modal_flow = Some(super::super::ModalFlow::RemovalPrompt {
            workspace: ws_key.clone(),
            reason: super::super::RemovalReason::OutOfScope,
        });
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
        m.modal_flow = Some(super::super::ModalFlow::RemovalPrompt {
            workspace: ws_key.clone(),
            reason: super::super::RemovalReason::Merged,
        });
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
    /// second prompt — the daemon's level-triggered re-emits (#292)
    /// rely on this dedupe to keep an unanswered prompt to a single
    /// visible ask.
    #[test]
    fn merged_pr_removable_mounts_confirm_and_dedupes() {
        use lazybox_ipc::Event as IpcEvent;
        let mut m = build_model();
        let ev = || IpcEvent::MergedPrRemovable {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            label: "o/r#1".into(),
            terminal_state: lazybox_ipc::RemovableTerminalState::Merged,
            active_terminal_count: 0,
            has_local_work: false,
        };
        m.handle_daemon_event(ev());
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
        assert!(matches!(
            m.modal_flow,
            Some(super::super::ModalFlow::RemovalPrompt {
                reason: super::super::RemovalReason::Merged,
                ..
            })
        ));

        m.handle_daemon_event(ev());
        assert!(
            m.removal_prompt_queue.is_empty(),
            "re-emit must not stack a second prompt"
        );
    }

    /// #552: a `RemovalCancelled` for the workspace whose removal confirm
    /// is mounted dismisses that modal — a reopened issue must not leave
    /// a stale "remove closed issue?" prompt up.
    #[test]
    fn removal_cancelled_dismisses_mounted_prompt() {
        use lazybox_ipc::Event as IpcEvent;
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::MergedPrRemovable {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            label: "o/r#1".into(),
            terminal_state: lazybox_ipc::RemovableTerminalState::Closed,
            active_terminal_count: 0,
            has_local_work: true,
        });
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));

        m.handle_daemon_event(IpcEvent::RemovalCancelled {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
        });
        assert_eq!(m.top_modal(), None, "reopen must dismiss the modal");
        assert!(m.modal_flow.is_none());
    }

    /// #552: even if the removal confirm has been buried under another
    /// modal, a `RemovalCancelled` clears its binding so a later confirm
    /// can't destroy the reopened workspace — the buried modal is left in
    /// place (it's not on top) but is now a no-op.
    #[test]
    fn removal_cancelled_neutralizes_buried_prompt() {
        use lazybox_ipc::Event as IpcEvent;
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::MergedPrRemovable {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            label: "o/r#1".into(),
            terminal_state: lazybox_ipc::RemovableTerminalState::Closed,
            active_terminal_count: 0,
            has_local_work: true,
        });
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
        // Bury the removal confirm under another modal.
        m.modal_stack.push(Id::Help);

        m.handle_daemon_event(IpcEvent::RemovalCancelled {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
        });
        assert!(
            m.modal_flow.is_none(),
            "flow must be cleared so a confirm can't remove the reopened row"
        );
        assert_eq!(
            m.top_modal(),
            Some(&Id::Help),
            "a buried removal confirm is not popped (it's not on top)"
        );
    }

    /// A `RemovalCancelled` for a *different* workspace leaves the
    /// mounted prompt untouched.
    #[test]
    fn removal_cancelled_ignores_other_workspace() {
        use lazybox_ipc::Event as IpcEvent;
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::MergedPrRemovable {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            label: "o/r#1".into(),
            terminal_state: lazybox_ipc::RemovableTerminalState::Closed,
            active_terminal_count: 0,
            has_local_work: true,
        });
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));

        m.handle_daemon_event(IpcEvent::RemovalCancelled {
            workspace_key: WorkspaceKey::new("github:o/r#2"),
        });
        assert_eq!(
            m.top_modal(),
            Some(&Id::RemoveOutOfScope),
            "an unrelated cancel must not dismiss this prompt"
        );
    }

    /// Regression for #292: two PRs merging in the same poll produce
    /// two `MergedPrRemovable` events → two modals, one after the
    /// other. The second queues behind the first and mounts as soon as
    /// the first is answered.
    #[test]
    fn two_merged_pr_removable_events_serialize_into_two_modals() {
        use lazybox_ipc::Event as IpcEvent;
        let mut m = build_model();
        let ev = |n: u64| IpcEvent::MergedPrRemovable {
            workspace_key: WorkspaceKey::new(format!("github:o/r#{n}")),
            label: format!("o/r#{n}"),
            terminal_state: lazybox_ipc::RemovableTerminalState::Merged,
            active_terminal_count: 0,
            has_local_work: false,
        };
        m.handle_daemon_event(ev(1));
        m.handle_daemon_event(ev(2));
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
        assert_eq!(
            m.removal_prompt_queue.len(),
            1,
            "second prompt must queue behind the active one"
        );

        let cmds = m.handle_confirmed(true);
        match &cmds[..] {
            [IpcCommand::RemoveMergedWorkspace { session_key }] => {
                assert_eq!(session_key.as_str(), "github:o/r#1");
            }
            other => panic!("expected RemoveMergedWorkspace for #1, got {other:?}"),
        }
        assert_eq!(
            m.top_modal(),
            Some(&Id::RemoveOutOfScope),
            "answering the first modal must mount the second"
        );
        match &m.modal_flow {
            Some(super::super::ModalFlow::RemovalPrompt {
                workspace,
                reason: super::super::RemovalReason::Merged,
            }) => {
                assert_eq!(workspace.as_str(), "github:o/r#2");
            }
            other => panic!("expected active prompt for #2, got {other:?}"),
        }
    }

    /// `n` on a merged/closed removal confirm tells the daemon to stop
    /// re-prompting (`KeepMergedWorkspace`) — unlike Esc, which stays
    /// silent so the daemon's level-triggered re-emit self-heals.
    #[test]
    fn confirmed_no_on_merged_removal_sends_keep() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#1");
        m.modal_flow = Some(super::super::ModalFlow::RemovalPrompt {
            workspace: ws_key.clone(),
            reason: super::super::RemovalReason::Merged,
        });
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_confirmed(false);
        match &cmds[..] {
            [IpcCommand::KeepMergedWorkspace { session_key }] => {
                assert_eq!(session_key, &SessionKey::from(&ws_key));
            }
            other => panic!("expected KeepMergedWorkspace, got {other:?}"),
        }
        assert!(m.modal_flow.is_none());
    }

    /// Esc on a merged removal confirm is a deferral, not an answer:
    /// no command goes out (the daemon re-prompts after its interval)
    /// and the slot clears so a re-emit can queue again.
    #[test]
    fn modal_dismissed_on_merged_removal_is_silent_deferral() {
        let mut m = build_model();
        m.modal_flow = Some(super::super::ModalFlow::RemovalPrompt {
            workspace: WorkspaceKey::new("github:o/r#1"),
            reason: super::super::RemovalReason::Merged,
        });
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_modal_dismissed();
        assert!(
            cmds.is_empty(),
            "Esc must NOT send KeepMergedWorkspace, got: {cmds:?}"
        );
        assert!(m.modal_flow.is_none());
    }

    /// `n` on RemoveOutOfScope clears the slot without producing
    /// a Kill — user said no, daemon doesn't need to hear about it.
    #[test]
    fn confirmed_no_on_remove_out_of_scope_returns_no_commands() {
        let mut m = build_model();
        m.modal_flow = Some(super::super::ModalFlow::RemovalPrompt {
            workspace: WorkspaceKey::new("github:o/r#1"),
            reason: super::super::RemovalReason::OutOfScope,
        });
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
            m.modal_flow = Some(super::super::ModalFlow::MergePrompt {
                issue: issue.clone(),
                pr: pr.clone(),
            });
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
        m.modal_flow = Some(super::super::ModalFlow::MergePrompt {
            issue: WorkspaceKey::new("github:o/r#1"),
            pr: WorkspaceKey::new("github:o/r#2"),
        });
        m.modal_stack.push(Id::MergeConfirm);
        let cmds = m.handle_modal_dismissed();
        assert!(
            cmds.is_empty(),
            "Esc on merge modal must NOT signal accept:false, got: {cmds:?}",
        );
        assert!(
            m.modal_flow.is_none(),
            "merge-prompt flow must clear so the queue can advance",
        );
    }

    /// Read the initial Enter default of the Confirm modal mounted
    /// under `id`. `Confirm::state()` exposes the highlighted button as
    /// a bool so the mount site's default is assertable end-to-end.
    fn mounted_confirm_default_yes(
        m: &Model<tuirealm::terminal::TestTerminalAdapter>,
        id: Id,
    ) -> bool {
        use tuirealm::state::{State, StateValue};
        match m.app.state(&id).expect("confirm modal is mounted") {
            State::Single(StateValue::Bool(b)) => b,
            other => panic!("expected a Bool state, got {other:?}"),
        }
    }

    /// Issue #312: the issue→PR session-merge prompt now defaults Enter
    /// to Yes — accepting is the expected, non-destructive path (the
    /// prompt only appears because a closing PR was detected).
    #[test]
    fn merge_prompt_defaults_to_yes() {
        let mut m = build_model();
        m.merge_prompt_queue.push_back((
            WorkspaceKey::new("github:o/r#1"),
            WorkspaceKey::new("github:o/r#2"),
            "o/r#1".into(),
            "o/r#2".into(),
            1,
        ));
        m.maybe_mount_next_merge_prompt();
        assert_eq!(m.top_modal(), Some(&Id::MergeConfirm));
        assert!(
            mounted_confirm_default_yes(&m, Id::MergeConfirm),
            "issue→PR merge prompt should default to Yes",
        );
    }

    /// Issue #525: the workspace-removal prompt is the *event* path — it
    /// pops unsolicited (a merged/closed task), so its default comes from
    /// `ui.confirm_default.event` (default No); a stray Enter must not
    /// force-delete a worktree.
    #[test]
    fn removal_prompt_defaults_to_no_from_event_source() {
        let mut m = build_model();
        m.removal_prompt_queue
            .push_back(super::super::RemovalPrompt {
                workspace_key: WorkspaceKey::new("github:o/r#1"),
                label: "o/r#1".into(),
                title: None,
                terminal_count: 0,
                reason: super::super::RemovalReason::Merged,
                has_local_work: false,
            });
        m.maybe_mount_next_removal_prompt();
        assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
        assert!(
            !mounted_confirm_default_yes(&m, Id::RemoveOutOfScope),
            "event-driven removal prompt should default to No",
        );
    }

    /// Issue #525: a user who sets `event: yes` opts the unsolicited
    /// removal prompt into a Yes default.
    #[test]
    fn removal_prompt_respects_yes_event_override() {
        use lazybox_config::ConfirmDefault;

        let mut m = build_model();
        m.ui_defaults.confirm_default.event = ConfirmDefault::Yes;
        m.removal_prompt_queue
            .push_back(super::super::RemovalPrompt {
                workspace_key: WorkspaceKey::new("github:o/r#1"),
                label: "o/r#1".into(),
                title: None,
                terminal_count: 0,
                reason: super::super::RemovalReason::Merged,
                has_local_work: false,
            });
        m.maybe_mount_next_removal_prompt();
        assert!(
            mounted_confirm_default_yes(&m, Id::RemoveOutOfScope),
            "event: yes flips the removal prompt to Yes",
        );
    }

    /// Issue #312: the clean-worktrees bulk-wipe confirm defaults No.
    #[test]
    fn clean_worktrees_prompt_defaults_to_no() {
        let mut m = build_model();
        m.mount_clean_worktrees_confirm();
        assert!(
            !mounted_confirm_default_yes(&m, Id::CleanWorktreesConfirm),
            "clean-worktrees prompt should default to No",
        );
    }

    /// Issue #312: the inspector's delete-worktree confirm defaults No.
    #[test]
    fn inspect_delete_prompt_defaults_to_no() {
        let mut m = build_model();
        m.mount_inspect_confirm(lazybox_ipc::WorktreeInspectionDto {
            path: std::path::PathBuf::from("/tmp/worktrees/o-r-feat"),
            bare_path: None,
            branch: Some("feat".into()),
            session_id: None,
            reasons: vec!["untracked".into()],
            size_bytes: 0,
            last_modified_unix: Some(0),
            has_uncommitted_changes: false,
            has_unpushed_commits: false,
            is_safe_to_delete: false,
        });
        assert!(
            !mounted_confirm_default_yes(&m, Id::InspectConfirm),
            "inspector delete prompt should default to No",
        );
    }

    /// Issue #525: `mount_action_confirm` is the *shortcut* path — the
    /// user pressed a destructive chord, so the default comes from
    /// `ui.confirm_default.destructive_shortcut` (default Yes), not from
    /// the prompt. `x x` archive + Enter completes the archive.
    #[test]
    fn action_confirm_defaults_yes_from_shortcut_source() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        m.mount_action_confirm(
            Action::Archive,
            vec![super::super::ActionConfirmTarget::Workspace(
                SessionKey::from("github:o/r#1"),
            )],
            None,
        );
        assert!(
            mounted_confirm_default_yes(&m, Id::ActionConfirm),
            "shortcut-initiated Archive confirm should default to Yes",
        );
    }

    /// Issue #525: a cautious user forcing `destructive_shortcut: no`
    /// flips even a chord-initiated confirm back to No.
    #[test]
    fn action_confirm_respects_no_shortcut_override() {
        use lazybox_config::ConfirmDefault;
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        m.ui_defaults.confirm_default.destructive_shortcut = ConfirmDefault::No;
        m.mount_action_confirm(
            Action::Archive,
            vec![super::super::ActionConfirmTarget::Workspace(
                SessionKey::from("github:o/r#1"),
            )],
            None,
        );
        assert!(
            !mounted_confirm_default_yes(&m, Id::ActionConfirm),
            "destructive_shortcut: no forces the confirm back to No",
        );
    }

    /// Issue #525: the on-main spawn confirm is a benign awareness gate,
    /// not a destructive action — it always defaults Yes, even when a
    /// cautious user has forced `destructive_shortcut: no` for the
    /// genuinely destructive prompts.
    #[test]
    fn on_main_spawn_confirm_stays_yes_despite_no_shortcut_override() {
        use lazybox_config::ConfirmDefault;
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        m.ui_defaults.confirm_default.destructive_shortcut = ConfirmDefault::No;
        m.mount_action_confirm(
            Action::SpawnAgentOnMain("claude".into()),
            vec![super::super::ActionConfirmTarget::Workspace(
                SessionKey::from("github:o/r#1"),
            )],
            None,
        );
        assert!(
            mounted_confirm_default_yes(&m, Id::ActionConfirm),
            "benign on-main gate affirms regardless of the destructive knob",
        );
    }

    /// Esc on a RemoveOutOfScope modal clears the slot but
    /// produces no command — there's nothing to tell the daemon;
    /// the workspace stays out of scope on its end too.
    #[test]
    fn modal_dismissed_on_remove_out_of_scope_clears_slot_silently() {
        let mut m = build_model();
        m.modal_flow = Some(super::super::ModalFlow::RemovalPrompt {
            workspace: WorkspaceKey::new("github:o/r#1"),
            reason: super::super::RemovalReason::OutOfScope,
        });
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_modal_dismissed();
        assert!(cmds.is_empty());
        assert!(m.modal_flow.is_none());
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

    /// Build a model with workspace `github:o/r#1` selected and a single
    /// live shell terminal on screen — the minimal state for a terminal
    /// mouse gesture (selection / click forwarding) to have a target.
    fn model_with_terminal() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind};
        let mut m = build_model();
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(m.sidebar.focus_workspace_key(&session_key));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key,
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        assert_eq!(m.terminals.active_terminal_id(), Some(TerminalId(1)));
        m
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

        let mut m = model_with_terminal();
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
        m.terminal_drag = None;
        m.dispatch_mouse_in(down(right_bottom.x + 2, right_bottom.y + 2), area);
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert!(
            m.terminal_drag.is_some(),
            "first click back into the terminal must claim the click, not just refocus",
        );
    }

    /// A press then drag to a different cell marks the gesture as a real
    /// selection and moves the focus endpoint off the anchor; releasing
    /// clears the drag. A press-release on the same cell stays a click
    /// (`dragged` never set). Guards the mouse-down/drag/up wiring for
    /// the scrollback-aware selection (#432).
    #[test]
    fn terminal_drag_marks_selection_then_clears_on_release() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use tuirealm::ratatui::layout::Rect;

        let mut m = model_with_terminal();
        m.focus = PaneFocus::Terminals;
        let area = Rect::new(0, 0, 120, 40);
        let (_sidebar, _right_top, right_bottom) = crate::realm::layout::pane_areas(
            area,
            m.layout.sidebar_pct,
            m.layout.right_top_pct,
            m.layout.sidebar_user_resized,
        );
        let ev = |kind, col, row| MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // Press inside the grid, then drag two rows down.
        let (c0, r0) = (right_bottom.x + 3, right_bottom.y + 4);
        m.dispatch_mouse_in(ev(MouseEventKind::Down(MouseButton::Left), c0, r0), area);
        let anchor = m.terminal_drag.expect("press claims a drag").anchor;
        m.dispatch_mouse_in(
            ev(MouseEventKind::Drag(MouseButton::Left), c0, r0 + 2),
            area,
        );
        let drag = m.terminal_drag.expect("drag still active");
        assert!(drag.dragged, "moving off the anchor cell is a real drag");
        assert_eq!(drag.anchor, anchor, "anchor stays pinned");
        assert_ne!(drag.focus, anchor, "focus tracked the pointer");

        m.dispatch_mouse_in(ev(MouseEventKind::Up(MouseButton::Left), c0, r0 + 2), area);
        assert!(m.terminal_drag.is_none(), "release ends the drag");
    }

    /// Adopt picker: source + target workspace keys flow into an
    /// `AdoptSessions` command. The pick carries the target as a
    /// `ChoicePayload::Workspace`, which the handler resolves directly.
    #[test]
    fn choice_picked_for_adopt_target_returns_adopt_sessions() {
        let mut m = build_model();
        let source = WorkspaceKey::new("github:o/r#1");
        let target = WorkspaceKey::new("github:o/r#2");
        m.modal_flow = Some(super::super::ModalFlow::AdoptSource {
            source: source.clone(),
        });
        m.modal_stack.push(Id::AdoptTarget);
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Workspace(target.clone())]);
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
        // Side state: the adoption slot clears.
        assert!(m.modal_flow.is_none());
    }

    /// `Id::RequestReviewers` picker: selecting two rows (each carrying
    /// its login as a `ChoicePayload::Text`) produces
    /// `Command::RequestReviewers` with those logins + the workspace key from
    /// `pending_review_request`. (Migrated from the older Input
    /// modal — see `mount_request_reviewers`.)
    #[test]
    fn choice_picked_on_request_reviewers_modal_returns_request_reviewers_cmd() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#1");
        m.modal_flow = Some(super::super::ModalFlow::ReviewRequest {
            workspace: ws_key.clone(),
        });
        m.modal_stack.push(Id::RequestReviewers);
        let cmds = m.handle_choice_picked(vec![
            ChoicePayload::Text("alice".into()),
            ChoicePayload::Text("carol".into()),
        ]);
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
        assert!(m.modal_flow.is_none());
    }

    /// `Id::AddAssignees` picker now fires `SetAssignees` (not Add)
    /// so the daemon can diff against the current task and run both
    /// add + remove mutations as needed. The picked indices are the
    /// *full desired set*, not deltas.
    #[test]
    fn choice_picked_on_add_assignees_modal_returns_set_assignees_cmd() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#5");
        m.modal_flow = Some(super::super::ModalFlow::AssigneesRequest {
            workspace: ws_key.clone(),
        });
        m.modal_stack.push(Id::AddAssignees);
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("bob".into())]);
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
        m.modal_flow = Some(super::super::ModalFlow::AssigneesRequest {
            workspace: ws_key.clone(),
        });
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
        m.modal_flow = Some(super::super::ModalFlow::ReviewRequest {
            workspace: WorkspaceKey::new("github:o/r#1"),
        });
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
        // The handler resolves the picked key from the payload, then
        // looks up the snippet via `self.snippets`.
        m.modal_stack.push(Id::SnippetPicker);
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("rev".into())]);
        // No active terminal → no Write emitted. The modal pops
        // regardless of dispatch outcome.
        assert!(cmds.is_empty(), "no command without an active terminal");
        assert!(
            !matches!(m.modal_stack.last(), Some(Id::SnippetPicker)),
            "modal popped"
        );
    }

    /// Build a model with workspace `github:o/r#1` selected and a
    /// single live terminal of `kind` on screen, its snippet library
    /// loaded, and the picker primed to resolve row 0 → `snippet_key`.
    /// This is the exact pre-submit state BOTH snippet trigger paths
    /// (the `]]s<key>` auto-submit and the picker's Enter) funnel into
    /// `handle_choice_picked`. The caller passes the picked key as a
    /// `ChoicePayload::Text` to `handle_choice_picked`.
    fn model_with_active_terminal_and_snippet(
        label: &str,
        snippets_yaml: &str,
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(
            m.sidebar.focus_workspace_key(&session_key),
            "seeded workspace should be selectable",
        );
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key,
            kind,
            no_permission: false,
            on_main: false,
        });
        assert_eq!(
            m.terminals.active_terminal_id(),
            Some(TerminalId(1)),
            "the spawned terminal must be on screen",
        );
        m.modal_stack.push(Id::SnippetPicker);
        m
    }

    /// Picking a snippet routes through one semantic delivery command. The
    /// daemon selects the agent's settle-gated paste+submit path and owns
    /// history updates after confirmation.
    #[test]
    fn snippet_into_agent_terminal_routes_through_confirmed_delivery() {
        let mut m = model_with_active_terminal_and_snippet(
            "agent-single",
            r#"
snippets:
  rev:
    description: Review
    body: review the diff
"#,
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        );
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("rev".into())]);
        match cmds.as_slice() {
            [
                IpcCommand::DeliverSnippet {
                    terminal_id,
                    snippet_key,
                    category,
                    body,
                    submit,
                },
            ] => {
                assert_eq!(body, "review the diff", "body delivered verbatim");
                assert_eq!(*terminal_id, lazybox_ipc::TerminalId(1));
                assert_eq!(snippet_key, "rev");
                assert!(category.is_empty());
                assert!(*submit, "Enter picks submit");
            }
            _ => panic!("agent snippet must use DeliverSnippet, got {cmds:?}"),
        }
        assert!(
            m.recent_snippets.is_empty(),
            "the client must wait for confirmed delivery",
        );
    }

    /// Shift-Enter (`handle_choice_picked_no_submit`) delivers the snippet
    /// with `submit: false` AND mirrors the body into the client's composing
    /// buffer, persisting the merged draft — so the recap reflects it on a
    /// later manual submit and the draft survives a restart (issue #791).
    #[test]
    fn snippet_shift_enter_inserts_without_submit_and_tracks_the_draft() {
        let mut m = model_with_active_terminal_and_snippet(
            "agent-nosubmit",
            r#"
snippets:
  rev:
    description: Review
    body: review the diff
"#,
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        );
        let cmds = m.handle_choice_picked_no_submit(vec![ChoicePayload::Text("rev".into())]);
        match cmds.as_slice() {
            [
                IpcCommand::DeliverSnippet { body, submit, .. },
                IpcCommand::RecordComposingBuffer { buffer, .. },
            ] => {
                assert_eq!(body, "review the diff", "body delivered verbatim");
                assert!(!*submit, "Shift-Enter inserts without submitting");
                assert_eq!(
                    buffer, "review the diff",
                    "the persisted draft carries the inserted body",
                );
            }
            _ => panic!(
                "Shift-Enter must deliver without submit and persist the draft, got {cmds:?}"
            ),
        }
    }

    /// A multi-line body preserves its content and trailing newline;
    /// the reliable submit is the daemon's separate Enter, so nothing
    /// in the TUI has to pre-rewrite the body into a bracketed paste
    /// (that's the shell-only encoding).
    #[test]
    fn snippet_into_agent_terminal_injects_multiline_body_verbatim() {
        let mut m = model_with_active_terminal_and_snippet(
            "agent-multi",
            "\nsnippets:\n  pr:\n    description: PR\n    body: |\n      first line\n      second line\n",
            lazybox_ipc::TerminalKind::Agent("codex".into()),
        );
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("pr".into())]);
        match cmds
            .iter()
            .find(|c| matches!(c, IpcCommand::DeliverSnippet { .. }))
        {
            Some(IpcCommand::DeliverSnippet { body, .. }) => {
                // The `|` block scalar keeps its trailing newline — the
                // body reaches the agent exactly as authored.
                assert_eq!(
                    body, "first line\nsecond line\n",
                    "multi-line body verbatim"
                );
            }
            _ => panic!("agent snippet must inject, got {cmds:?}"),
        }
    }

    /// A skill-dispatching snippet (#798) resolves to an explicit skill
    /// invocation on the way to the agent, through the same `DeliverSnippet`
    /// path a text snippet uses — so Recent / `]N` / broadcast are unchanged.
    #[test]
    fn skill_snippet_delivers_resolved_invocation() {
        let mut m = model_with_active_terminal_and_snippet(
            "agent-skill",
            "\nsnippets:\n  review:\n    description: Review\n    skill: code-review\n    body: review the current diff\n",
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        );
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("review".into())]);
        match cmds
            .iter()
            .find(|c| matches!(c, IpcCommand::DeliverSnippet { .. }))
        {
            Some(IpcCommand::DeliverSnippet {
                snippet_key, body, ..
            }) => {
                assert_eq!(snippet_key, "review");
                assert_eq!(
                    body,
                    "Use the `code-review` skill to complete this task:\n\nreview the current diff"
                );
            }
            _ => panic!("skill snippet must inject a resolved invocation, got {cmds:?}"),
        }
    }

    /// The client sends the same delivery command for every terminal kind;
    /// the daemon owns kind-specific encoding and confirmation.
    #[test]
    fn snippet_into_shell_terminal_uses_confirmed_delivery_command() {
        let mut m = model_with_active_terminal_and_snippet(
            "shell",
            r#"
snippets:
  ls:
    description: List
    body: ls -la
"#,
            lazybox_ipc::TerminalKind::Shell,
        );
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("ls".into())]);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::DeliverSnippet {
                snippet_key,
                body,
                ..
            }] if snippet_key == "ls" && body == "ls -la"
        ));
    }

    /// Sending a snippet records it in the session MRU so the picker's
    /// "Recent" group can float it to the top next time (#252). A shell
    /// terminal keeps the setup simple.
    #[test]
    fn sending_a_snippet_waits_for_delivery_before_recording_recent() {
        let mut m = model_with_active_terminal_and_snippet(
            "recent",
            "\nsnippets:\n  ls:\n    description: List\n    body: ls -la\n",
            lazybox_ipc::TerminalKind::Shell,
        );
        assert!(m.recent_snippets.is_empty(), "nothing sent yet");
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("ls".into())]);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::DeliverSnippet { .. }]
        ));
        assert!(m.recent_snippets.is_empty(), "queued is not delivered");
        m.handle_daemon_event(lazybox_ipc::Event::SnippetDelivered {
            terminal_id: lazybox_ipc::TerminalId(1),
            session_key: "github:o/r#1".into(),
            snippet_key: "ls".into(),
            prompt: None,
        });
        assert_eq!(m.recent_snippets, vec!["ls"]);
    }

    #[test]
    fn tour_sends_and_repeats_through_the_real_picker() {
        use crate::realm::components::tour::SEND_SNIPPET_STEP;

        let mut m = model_with_active_terminal_and_snippet(
            "tour",
            "\nsnippets:\n  rev:\n    description: Review\n    body: review the diff\n",
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        );
        m.modal_stack.clear();
        m.mount_tour_at(SEND_SNIPPET_STEP);
        m.update(Msg::TourTrySnippet);
        assert_eq!(m.top_modal(), Some(&Id::SnippetPicker));

        let first = m.handle_choice_picked(vec![ChoicePayload::Text("rev".into())]);
        assert!(matches!(
            first.as_slice(),
            [IpcCommand::DeliverSnippet { .. }]
        ));
        assert!(m.recent_snippets.is_empty());
        m.handle_daemon_event(lazybox_ipc::Event::SnippetDelivered {
            terminal_id: lazybox_ipc::TerminalId(1),
            session_key: "github:o/r#1".into(),
            snippet_key: "rev".into(),
            prompt: Some(lazybox_ipc::UserPrompt {
                text: "review the diff".into(),
                timestamp_ms: 611,
                source: lazybox_ipc::PromptSource::Snippet {
                    key: "rev".into(),
                    category: String::new(),
                },
            }),
        });
        assert_eq!(m.top_modal(), Some(&Id::Tour));
        assert_eq!(m.recent_snippets, vec!["rev"]);

        m.update(Msg::TourRepeatSnippet);
        assert_eq!(m.top_modal(), Some(&Id::SnippetPicker));
        let repeat = m.handle_choice_picked(vec![ChoicePayload::Text("rev".into())]);
        assert!(matches!(
            repeat.as_slice(),
            [IpcCommand::DeliverSnippet {
                snippet_key,
                ..
            }] if snippet_key == "rev"
        ));
        m.handle_daemon_event(lazybox_ipc::Event::SnippetDelivered {
            terminal_id: lazybox_ipc::TerminalId(1),
            session_key: "github:o/r#1".into(),
            snippet_key: "rev".into(),
            prompt: None,
        });
        assert_eq!(m.top_modal(), Some(&Id::Tour));
        assert!(m.modal_flow.is_none());
    }

    /// Workspace attribution is not part of the client command: the daemon
    /// derives it from the live terminal, so a client cannot credit a
    /// different workspace before delivery succeeds.
    #[test]
    fn sending_a_snippet_does_not_claim_a_workspace() {
        let mut m = model_with_active_terminal_and_snippet(
            "sent-history",
            "\nsnippets:\n  ls:\n    description: List\n    body: ls -la\n",
            lazybox_ipc::TerminalKind::Shell,
        );
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("ls".into())]);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::DeliverSnippet {
                snippet_key,
                ..
            }] if snippet_key == "ls"
        ));
    }

    /// The session MRU is most-recent-first, de-duplicated, and capped.
    #[test]
    fn recent_snippets_mru_dedups_and_caps() {
        let mut m = build_model();
        for k in ["a", "b", "c"] {
            m.apply_recent_snippet(k.to_string());
        }
        assert_eq!(m.recent_snippets, vec!["c", "b", "a"], "most-recent first");
        // Re-using an entry moves it to the front without duplicating.
        m.apply_recent_snippet("a".to_string());
        assert_eq!(m.recent_snippets, vec!["a", "c", "b"]);
        // The list is capped — oldest entries fall off the end.
        for k in ["d", "e", "f", "g"] {
            m.apply_recent_snippet(k.to_string());
        }
        assert_eq!(m.recent_snippets.len(), 5, "capped at RECENT_SNIPPETS_MAX");
        assert_eq!(m.recent_snippets[0], "g", "newest at the front");
        assert!(
            !m.recent_snippets.contains(&"b".to_string()),
            "oldest dropped"
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

    /// Build a model with N seeded workspaces (`github:o/r#1..N`) and
    /// one terminal per `Some(kind)` entry (terminal ids 1..N, index-
    /// aligned with the workspaces). Returns the workspace session
    /// keys in seed order — the target list a broadcast would use.
    fn model_with_broadcast_targets(
        kinds: &[Option<lazybox_ipc::TerminalKind>],
    ) -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        Vec<SessionKey>,
    ) {
        model_with_broadcast_targets_and_snippets(kinds, lazybox_config::Snippets::default())
    }

    fn model_with_broadcast_targets_and_snippets(
        kinds: &[Option<lazybox_ipc::TerminalKind>],
        snippets: lazybox_config::Snippets,
    ) -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        Vec<SessionKey>,
    ) {
        use lazybox_ipc::{Event as IpcEvent, TerminalId};
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test_with_snippets(client, Size::new(120, 40), snippets)
            .expect("model init");
        let keys: Vec<SessionKey> = (1..=kinds.len())
            .map(|i| SessionKey::from(format!("github:o/r#{i}").as_str()))
            .collect();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: keys
                .iter()
                .map(|k| {
                    lazybox_core::Workspace::empty(
                        WorkspaceKey::new(k.as_str()),
                        "main",
                        chrono::Utc::now(),
                    )
                })
                .collect(),
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        for (i, kind) in kinds.iter().enumerate() {
            if let Some(kind) = kind {
                m.handle_daemon_event(IpcEvent::TerminalSpawned {
                    model_label: None,
                    terminal_id: TerminalId(i as u64 + 1),
                    session_key: keys[i].clone(),
                    kind: kind.clone(),
                    no_permission: false,
                    on_main: false,
                });
            }
        }
        (m, keys)
    }

    /// `Space` collapses a repo group ONLY when the cursor sits on its
    /// header row — on a workspace row it is inert, so a reflexive Space
    /// mid-navigation can't fold the group you're inside (#1099). Drives
    /// the real key routing so the dispatch guard, not just the catalog
    /// resolution, is under test.
    #[test]
    fn space_collapse_is_gated_to_the_header_row() {
        use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};
        let space = || RealmKey::new(Key::Char(' '), RealmMods::NONE);
        let up = || RealmKey::new(Key::Char('k'), RealmMods::NONE);

        let (mut m, _keys) = model_with_broadcast_targets(&[None, None]);
        assert_eq!(m.sidebar.visible_workspace_count(), 2, "both rows visible");
        // Cursor starts on a workspace row: Space must leave the group open
        // (a collapse would drop the workspace rows from the visible list).
        assert!(!m.sidebar.cursor_on_repo_header());
        m.dispatch_key(space());
        assert_eq!(
            m.sidebar.visible_workspace_count(),
            2,
            "a bare Space on a workspace row must not collapse the group",
        );

        // Walk up onto the repo header (j/k stop on headers), where Space
        // is the intended collapse toggle.
        for _ in 0..6 {
            if m.sidebar.cursor_on_repo_header() {
                break;
            }
            m.dispatch_key(up());
        }
        assert!(
            m.sidebar.cursor_on_repo_header(),
            "k navigation reaches the repo header row",
        );
        m.dispatch_key(space());
        assert_eq!(
            m.sidebar.visible_workspace_count(),
            0,
            "Space on the header row collapses the group",
        );
    }

    /// Like [`model_with_broadcast_targets`], but every seeded workspace
    /// carries a GitHub project scope, so a session-less target is
    /// spawnable (`worktree_scope().is_some()`) rather than skipped —
    /// the case #836's auto-start covers.
    fn model_with_scoped_broadcast_targets(
        kinds: &[Option<lazybox_ipc::TerminalKind>],
    ) -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        Vec<SessionKey>,
    ) {
        use lazybox_ipc::{Event as IpcEvent, TerminalId};
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let keys: Vec<SessionKey> = (1..=kinds.len())
            .map(|i| SessionKey::from(format!("github:o/r#{i}").as_str()))
            .collect();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: keys
                .iter()
                .map(|k| {
                    let mut w = lazybox_core::Workspace::empty(
                        WorkspaceKey::new(k.as_str()),
                        "main",
                        chrono::Utc::now(),
                    );
                    w.project_key = Some(lazybox_core::ProjectKey::github("o", "r"));
                    w
                })
                .collect(),
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        for (i, kind) in kinds.iter().enumerate() {
            if let Some(kind) = kind {
                m.handle_daemon_event(IpcEvent::TerminalSpawned {
                    model_label: None,
                    terminal_id: TerminalId(i as u64 + 1),
                    session_key: keys[i].clone(),
                    kind: kind.clone(),
                    no_permission: false,
                    on_main: false,
                });
            }
        }
        (m, keys)
    }

    #[test]
    fn configured_model_broadcast_can_pick_the_builtin_audit_snippet() {
        let (mut m, keys) = model_with_broadcast_targets_and_snippets(
            &[Some(lazybox_ipc::TerminalKind::Agent("claude".into()))],
            lazybox_config::Snippets::builtin(),
        );
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        assert_eq!(m.sidebar.toggle_broadcast_select(), Some(true));

        m.mount_broadcast_picker();
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::BroadcastSnippet),
            "a configured model must offer the snippet picker"
        );

        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("audit".into())]);
        assert!(cmds.is_empty(), "picking only advances the flow");
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::BroadcastText),
            "the built-in audit snippet advances to editable composition"
        );
        let Some(super::super::ModalFlow::Broadcast { draft }) = &m.modal_flow else {
            panic!("broadcast draft survives into composition");
        };
        assert_eq!(draft.snippet_key.as_deref(), Some("audit"));
    }

    /// Broadcast compose submit with two agent targets and one
    /// session-less target: one `InjectPrompt` + `RecordUserMessage`
    /// pair PER agent terminal (the #246-safe settle-gated path), no
    /// raw `Write`, and a summary notice naming the skip.
    #[test]
    fn broadcast_fans_out_one_inject_per_agent_and_reports_skips() {
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Agent("codex".into())),
            None,
        ]);
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys,
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let cmds = m.handle_textarea_submitted("merge when the PR is green".into());

        let inject_tids: Vec<u64> = cmds
            .iter()
            .filter_map(|c| match c {
                IpcCommand::InjectPrompt {
                    terminal_id,
                    prompt,
                    fallback_spawn,
                    submit: _,
                } => {
                    assert_eq!(prompt, "merge when the PR is green");
                    assert!(fallback_spawn.is_none());
                    Some(terminal_id.0)
                }
                _ => None,
            })
            .collect();
        assert_eq!(inject_tids, vec![1, 2], "one inject per agent target");
        let recap_count = cmds
            .iter()
            .filter(|c| matches!(
                c,
                IpcCommand::RecordUserMessage { prompt, .. } if prompt.text == "merge when the PR is green"
            ))
            .count();
        assert_eq!(recap_count, 2, "each agent gets its recap line");
        assert!(
            !cmds.iter().any(|c| matches!(c, IpcCommand::Write { .. })),
            "agents must not ALSO get a raw write",
        );
        assert!(m.modal_flow.is_none(), "draft consumed");
        let notice = m.status.notice.as_ref().expect("summary notice");
        assert!(
            notice.message.contains("queued for 2 workspaces"),
            "summary counts queued deliveries: {}",
            notice.message,
        );
        assert!(
            notice.message.contains("1 skipped"),
            "summary names the session-less target: {}",
            notice.message,
        );
    }

    /// A snippet broadcast queues one daemon-owned confirmed delivery per
    /// live target. No workspace history claim is emitted for the skipped
    /// target.
    #[test]
    fn broadcast_with_a_snippet_queues_confirmed_delivery_per_live_target() {
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Shell),
            None,
        ]);
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys.clone(),
                snippet_key: Some("rev".into()),
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let cmds = m.handle_textarea_submitted("review the diff".into());

        let delivered: Vec<u64> = cmds
            .iter()
            .filter_map(|c| match c {
                IpcCommand::DeliverSnippet {
                    terminal_id,
                    snippet_key,
                    body,
                    ..
                } => {
                    assert_eq!(snippet_key, "rev");
                    assert_eq!(body, "review the diff");
                    Some(terminal_id.0)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            delivered,
            vec![1, 2],
            "the two live targets are queued, the skipped one is not",
        );
    }

    /// A target whose only session is a plain shell gets the encoded
    /// direct write (shells have no paste debounce), not the agent
    /// inject path.
    #[test]
    fn broadcast_to_shell_target_writes_encoded_bytes() {
        let (mut m, keys) = model_with_broadcast_targets(&[Some(lazybox_ipc::TerminalKind::Shell)]);
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys,
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let cmds = m.handle_textarea_submitted("ls -la".into());
        match cmds.as_slice() {
            [
                IpcCommand::Write {
                    terminal_id, bytes, ..
                },
            ] => {
                assert_eq!(terminal_id.0, 1);
                assert_eq!(
                    bytes,
                    &super::super::inputs::encode_snippet_for_pty("ls -la")
                );
            }
            other => panic!("shell target must get exactly one Write, got {other:?}"),
        }
    }

    /// A workspace running an agent AND a shell delivers to the agent —
    /// the instruction is meant for the conversation, not the prompt.
    #[test]
    fn broadcast_prefers_agent_terminal_over_shell() {
        use lazybox_ipc::{Event as IpcEvent, TerminalId};
        let (mut m, keys) = model_with_broadcast_targets(&[Some(lazybox_ipc::TerminalKind::Shell)]);
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(9),
            session_key: keys[0].clone(),
            kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys,
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let cmds = m.handle_textarea_submitted("go".into());
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                IpcCommand::InjectPrompt { terminal_id, .. } if terminal_id.0 == 9
            )),
            "agent terminal wins: {cmds:?}",
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, IpcCommand::Write { .. })),
            "the shell must not receive a duplicate copy",
        );
    }

    /// #836: a session-less target that still resolves to a repo scope
    /// no longer gets silently skipped — because delivering means
    /// spawning a fresh agent (heavy), the compose submit raises a
    /// confirm gate first and dispatches nothing until it's answered.
    #[test]
    fn broadcast_confirms_before_starting_agents_for_scoped_session_less_targets() {
        let (mut m, keys) = model_with_scoped_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            None,
        ]);
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys,
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let cmds = m.handle_textarea_submitted("ship it".into());
        assert!(
            cmds.is_empty(),
            "nothing dispatched until confirmed: {cmds:?}"
        );
        assert_eq!(m.top_modal(), Some(&Id::BroadcastConfirm));
        assert!(
            matches!(
                m.modal_flow,
                Some(super::super::ModalFlow::BroadcastConfirm { .. })
            ),
            "the composed body + targets are stashed for the confirm",
        );
    }

    /// #836: confirming the gate delivers to the live target AND spawns
    /// the default agent into the session-less scoped one, seeded with
    /// the broadcast as its initial prompt (the daemon injects it once
    /// the agent settles).
    #[test]
    fn broadcast_confirm_yes_starts_default_agent_seeded_with_the_message() {
        let (mut m, keys) = model_with_scoped_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            None,
        ]);
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys.clone(),
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let _ = m.handle_textarea_submitted("ship it".into());
        assert_eq!(m.top_modal(), Some(&Id::BroadcastConfirm));

        let cmds = m.handle_confirmed(true);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                IpcCommand::InjectPrompt { terminal_id, prompt, .. }
                    if terminal_id.0 == 1 && prompt == "ship it"
            )),
            "the live agent gets the settle-gated inject: {cmds:?}",
        );
        let spawn = cmds
            .iter()
            .find_map(|c| match c {
                IpcCommand::Spawn {
                    session_key,
                    kind,
                    initial_prompt,
                    on_main,
                    ..
                } => Some((
                    session_key.clone(),
                    kind.clone(),
                    initial_prompt.clone(),
                    *on_main,
                )),
                _ => None,
            })
            .expect("the session-less scoped target spawns the default agent");
        assert_eq!(spawn.0, keys[1]);
        assert!(
            matches!(&spawn.1, lazybox_ipc::TerminalKind::Agent(id) if id == "claude"),
            "the default agent (claude) is spawned: {:?}",
            spawn.1,
        );
        assert_eq!(
            spawn.2.as_deref(),
            Some("ship it"),
            "the broadcast seeds the new agent's initial prompt",
        );
        assert!(!spawn.3, "broadcast spawns into a worktree, not main");
        let notice = m.status.notice.as_ref().expect("summary notice");
        assert!(
            notice.message.contains("started 1 agent"),
            "summary reports the auto-start: {}",
            notice.message,
        );
    }

    /// #1077 review regression: if a live-agent target's terminal dies
    /// while the "start N agents?" confirm is up, "yes" must re-resolve it
    /// — spawning a fresh seeded agent — not fire a delivery at the dead
    /// terminal (which the daemon silently drops while the summary lies
    /// "queued"). Snapshotting the resolved steps at compose time would
    /// have replayed an `InjectPrompt` at the gone terminal; re-resolving
    /// at confirm-yes recovers.
    #[test]
    fn broadcast_confirm_re_resolves_a_target_whose_agent_died_under_the_modal() {
        // A: live agent (terminal 1); B: session-less spawnable → forces
        // the confirm gate. Both carry a repo scope, so a session-less A
        // is spawnable too.
        let (mut m, keys) = model_with_scoped_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            None,
        ]);
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys.clone(),
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let _ = m.handle_textarea_submitted("ship it".into());
        assert_eq!(m.top_modal(), Some(&Id::BroadcastConfirm));

        // A's agent exits while the confirm is up.
        m.handle_daemon_event(lazybox_ipc::Event::TerminalExited {
            terminal_id: lazybox_ipc::TerminalId(1),
            exit_code: None,
            last_output: None,
        });

        let cmds = m.handle_confirmed(true);
        assert!(
            !cmds.iter().any(|c| matches!(
                c,
                IpcCommand::InjectPrompt { terminal_id, .. } if terminal_id.0 == 1
            )),
            "no delivery fires at the terminal that died under the modal: {cmds:?}",
        );
        let seeded_spawns = cmds
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    IpcCommand::Spawn { initial_prompt: Some(p), .. } if p == "ship it"
                )
            })
            .count();
        assert_eq!(
            seeded_spawns, 2,
            "the dead-agent target re-resolves to a fresh seeded spawn alongside the session-less one: {cmds:?}",
        );
    }

    /// #836: declining the gate spawns nothing and keeps the sidebar
    /// multi-select intact so the user can retry or change the pick.
    #[test]
    fn broadcast_confirm_no_cancels_without_spawning() {
        let (mut m, keys) = model_with_scoped_broadcast_targets(&[None]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        assert_eq!(m.sidebar.toggle_broadcast_select(), Some(true));
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys,
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let _ = m.handle_textarea_submitted("ship it".into());
        assert_eq!(m.top_modal(), Some(&Id::BroadcastConfirm));

        let cmds = m.handle_confirmed(false);
        assert!(cmds.is_empty(), "cancel dispatches nothing: {cmds:?}");
        assert!(m.modal_flow.is_none(), "the stash is dropped");
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            1,
            "the multi-select survives a cancel",
        );
    }

    /// #847: the bulk "resume all rate-limited agents" action injects a
    /// settle-gated "continue" into every limit-blocked agent — and ONLY
    /// those, never a merely-working or asking one.
    #[test]
    fn resume_rate_limited_targets_only_limit_blocked_agents() {
        use lazybox_ipc::{AgentState, Event as IpcEvent, TerminalId};
        use lazybox_tui_core::action::Action;
        let agent = || Some(lazybox_ipc::TerminalKind::Agent("claude".into()));
        let (mut m, keys) = model_with_broadcast_targets(&[agent(), agent(), agent()]);
        // ws1 + ws3 rate-limited, ws2 merely working.
        for (i, state) in [
            AgentState::LimitReached,
            AgentState::Working,
            AgentState::LimitReached,
        ]
        .into_iter()
        .enumerate()
        {
            m.handle_daemon_event(IpcEvent::AgentState {
                session_key: keys[i].clone(),
                terminal_id: TerminalId(i as u64 + 1),
                state,
            });
        }

        let cmds = m.dispatch_action(&Action::ResumeRateLimited);
        let mut injected: Vec<(u64, &str)> = cmds
            .iter()
            .filter_map(|c| match c {
                IpcCommand::InjectPrompt {
                    terminal_id,
                    prompt,
                    submit: true,
                    ..
                } => Some((terminal_id.0, prompt.as_str())),
                _ => None,
            })
            .collect();
        injected.sort();
        assert_eq!(
            injected,
            vec![(1, "continue"), (3, "continue")],
            "only the two limit-blocked agents get the settle-gated resume, \
             never the working one: {cmds:?}",
        );
    }

    /// #847: with nothing rate-limited, the bulk resume dispatches no
    /// inject and flashes a hint rather than silently doing nothing.
    #[test]
    fn resume_rate_limited_with_no_targets_is_a_no_op_hint() {
        use lazybox_tui_core::action::Action;
        let agent = || Some(lazybox_ipc::TerminalKind::Agent("claude".into()));
        let (mut m, _keys) = model_with_broadcast_targets(&[agent()]);
        let cmds = m.dispatch_action(&Action::ResumeRateLimited);
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, IpcCommand::InjectPrompt { .. })),
            "no agent is rate-limited, so nothing is injected: {cmds:?}",
        );
    }

    /// #847 (review finding): a workspace with two agents where only the
    /// HIGHER-id one is rate-limited must resume THAT terminal, not the
    /// lower-id working sibling. Targeting the workspace and routing
    /// through its lowest-id agent (as broadcast does) would inject
    /// "continue" into the working agent and skip the blocked one.
    #[test]
    fn resume_rate_limited_targets_the_blocked_terminal_not_a_working_sibling() {
        use lazybox_ipc::{AgentState, Event as IpcEvent, TerminalId};
        use lazybox_tui_core::action::Action;
        let agent = || Some(lazybox_ipc::TerminalKind::Agent("claude".into()));
        // One workspace with agent terminal 1; add a second agent (id 2)
        // into the SAME workspace.
        let (mut m, keys) = model_with_broadcast_targets(&[agent()]);
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: keys[0].clone(),
            kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        // Lower-id agent is working; only the higher-id one is blocked.
        m.handle_daemon_event(IpcEvent::AgentState {
            session_key: keys[0].clone(),
            terminal_id: TerminalId(1),
            state: AgentState::Working,
        });
        m.handle_daemon_event(IpcEvent::AgentState {
            session_key: keys[0].clone(),
            terminal_id: TerminalId(2),
            state: AgentState::LimitReached,
        });

        let cmds = m.dispatch_action(&Action::ResumeRateLimited);
        let injected: Vec<u64> = cmds
            .iter()
            .filter_map(|c| match c {
                IpcCommand::InjectPrompt { terminal_id, .. } => Some(terminal_id.0),
                _ => None,
            })
            .collect();
        assert_eq!(
            injected,
            vec![2],
            "only the blocked terminal (2) resumes, never the working sibling (1): {cmds:?}",
        );
    }

    /// #1012: the escalating usage-limit alert rides the count of
    /// rate-limited workspaces — a sticky footer banner (naming the
    /// resume action, plus the parsed reset time) raised as agents hit
    /// the wall, updated as the count grows, retracted once they all
    /// recover. The passive header count tracks the same set.
    #[test]
    fn usage_limit_alert_escalates_and_retracts_with_the_blocked_count() {
        use crate::realm::components::footer::NoticeSeverity;
        use lazybox_ipc::{AgentState, Event as IpcEvent, TerminalId};
        let agent = || Some(lazybox_ipc::TerminalKind::Agent("claude".into()));
        let (mut m, keys) = model_with_broadcast_targets(&[agent(), agent()]);
        assert_eq!(m.sidebar.limit_reached_workspace_count(), 0);
        assert!(m.status.notice.is_none(), "nothing blocked yet");

        // First agent hits its usage limit → sticky banner + header count.
        m.handle_daemon_event(IpcEvent::AgentState {
            session_key: keys[0].clone(),
            terminal_id: TerminalId(1),
            state: AgentState::LimitReached,
        });
        assert_eq!(m.sidebar.limit_reached_workspace_count(), 1);
        let n = m.status.notice.as_ref().expect("banner raised");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("1 agent rate-limited"), "{}", n.message);

        // The reset countdown folds into the same banner.
        m.handle_daemon_event(IpcEvent::AgentUsageLimit {
            session_key: keys[0].clone(),
            terminal_id: TerminalId(1),
            reset_hint: "3pm".into(),
        });
        assert!(
            m.status
                .notice
                .as_ref()
                .unwrap()
                .message
                .contains("resets 3pm"),
            "{}",
            m.status.notice.as_ref().unwrap().message,
        );

        // A second agent blocks → count and banner escalate together.
        m.handle_daemon_event(IpcEvent::AgentState {
            session_key: keys[1].clone(),
            terminal_id: TerminalId(2),
            state: AgentState::LimitReached,
        });
        assert_eq!(m.sidebar.limit_reached_workspace_count(), 2);
        assert!(
            m.status
                .notice
                .as_ref()
                .unwrap()
                .message
                .contains("2 agents rate-limited"),
            "{}",
            m.status.notice.as_ref().unwrap().message,
        );

        // Both recover → banner retracts, count clears.
        for (k, t) in [
            (keys[0].clone(), TerminalId(1)),
            (keys[1].clone(), TerminalId(2)),
        ] {
            m.handle_daemon_event(IpcEvent::AgentState {
                session_key: k,
                terminal_id: t,
                state: AgentState::Working,
            });
        }
        assert_eq!(m.sidebar.limit_reached_workspace_count(), 0);
        assert!(
            m.status.notice.is_none(),
            "banner retracted once every agent recovered",
        );
    }

    /// #1012: `ui.usage_limit_alerts = false` suppresses the escalating
    /// sticky banner, but the passive `⏳ N limited` header count still
    /// tracks the blocked set. The opt-out only silences the escalation —
    /// the transient #847 rising-edge hint ("hit its usage limit —
    /// Shift-L/Shift-K") still fires, so the assertion targets the sticky
    /// (`Permanent`) banner specifically, not any footer notice.
    #[test]
    fn usage_limit_alert_disabled_keeps_header_count_but_raises_no_banner() {
        use crate::realm::components::footer::NoticeSeverity;
        use lazybox_ipc::{AgentState, Event as IpcEvent, TerminalId};
        let agent = || Some(lazybox_ipc::TerminalKind::Agent("claude".into()));
        let (mut m, keys) = model_with_broadcast_targets(&[agent()]);
        m.ui_defaults.usage_limit_alerts = false;
        m.handle_daemon_event(IpcEvent::AgentState {
            session_key: keys[0].clone(),
            terminal_id: TerminalId(1),
            state: AgentState::LimitReached,
        });
        assert_eq!(m.sidebar.limit_reached_workspace_count(), 1);
        assert!(
            !m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.severity == NoticeSeverity::Permanent),
            "escalation opted out — no sticky banner despite the block",
        );
    }

    /// #836: a session-less workspace with NO repo scope (nothing to
    /// spawn into) stays skipped — no confirm gate, named in the notice.
    #[test]
    fn broadcast_skips_session_less_workspace_with_no_repo_scope() {
        let (mut m, keys) = model_with_broadcast_targets(&[None]);
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys,
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let cmds = m.handle_textarea_submitted("ship it".into());
        assert!(
            cmds.is_empty(),
            "nothing spawned for an unspawnable row: {cmds:?}"
        );
        assert_ne!(
            m.top_modal(),
            Some(&Id::BroadcastConfirm),
            "no confirm gate when there's nothing to spawn",
        );
        let notice = m.status.notice.as_ref().expect("summary notice");
        assert!(
            notice.message.contains("skipped (no repo)"),
            "the unspawnable target is named as skipped: {}",
            notice.message,
        );
    }

    /// Step one of the broadcast flow: picking a snippet doesn't send
    /// anything — it stashes the key on the draft and funnels into the
    /// compose textarea (pre-filled with the body upstream). The
    /// composed buffer (snippet body + appended custom text) is then
    /// what every target receives, and the MRU counts the bulk send
    /// once, not once per target.
    #[test]
    fn broadcast_snippet_pick_records_mru_only_after_confirmed_deliveries() {
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
        ]);
        m.snippets = snippets_from_yaml(
            "broadcast-compose",
            "\nsnippets:\n  rev:\n    description: Review\n    body: review the diff\n",
        );
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys.clone(),
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastSnippet);

        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("rev".into())]);
        assert!(cmds.is_empty(), "the pick itself sends nothing");
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::BroadcastText),
            "pick funnels into the compose step",
        );
        assert_eq!(
            match &m.modal_flow {
                Some(super::super::ModalFlow::Broadcast { draft }) => {
                    draft.snippet_key.as_deref()
                }
                _ => None,
            },
            Some("rev"),
        );

        // The compose buffer arrives pre-filled with the snippet body;
        // the user appended a line. Everything lands as ONE message.
        let cmds =
            m.handle_textarea_submitted("review the diff\n\nfocus on the auth changes\n".into());
        let prompts: Vec<&str> = cmds
            .iter()
            .filter_map(|c| match c {
                IpcCommand::DeliverSnippet { body, .. } => Some(body.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            prompts,
            vec![
                "review the diff\n\nfocus on the auth changes",
                "review the diff\n\nfocus on the auth changes",
            ],
            "snippet + custom text compose into one body per target",
        );
        assert!(m.recent_snippets.is_empty(), "queued is not delivered");
        for (index, session_key) in keys.into_iter().enumerate() {
            m.handle_daemon_event(lazybox_ipc::Event::SnippetDelivered {
                terminal_id: lazybox_ipc::TerminalId(index as u64 + 1),
                session_key,
                snippet_key: "rev".into(),
                prompt: None,
            });
        }
        assert_eq!(m.recent_snippets, vec!["rev"], "bulk send de-duplicates");
    }

    /// `Ctrl-F` in the broadcast picker arrives as an empty pick: no
    /// snippet key is stashed and the flow continues to the compose
    /// step for a free-text-only send.
    #[test]
    fn broadcast_free_text_pick_skips_snippet() {
        let (mut m, keys) = model_with_broadcast_targets(&[Some(
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        )]);
        m.snippets = snippets_from_yaml(
            "broadcast-freetext",
            "\nsnippets:\n  rev:\n    description: Review\n    body: review the diff\n",
        );
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys,
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastSnippet);
        let cmds = m.handle_choice_picked(Vec::new());
        assert!(cmds.is_empty());
        assert_eq!(m.modal_stack.last(), Some(&Id::BroadcastText));
        let Some(super::super::ModalFlow::Broadcast { draft }) = &m.modal_flow else {
            panic!("draft survives");
        };
        assert_eq!(draft.snippet_key, None, "free text only — no snippet");
        let cmds = m.handle_textarea_submitted("ad-hoc prompt".into());
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                IpcCommand::InjectPrompt { prompt, .. } if prompt == "ad-hoc prompt"
            )),
            "free text still delivers: {cmds:?}",
        );
        assert!(m.recent_snippets.is_empty(), "no snippet, no MRU entry");
    }

    /// `Shift-B` resolves its targets from the sidebar multi-select at
    /// mount time, and a delivered broadcast clears the selection —
    /// the same contract as the activity pane's `w`. A broadcast that
    /// reached nobody keeps the marks so the user can retry after
    /// spawning an agent.
    #[test]
    fn broadcast_clears_selection_after_delivery_but_not_after_all_skipped() {
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            None,
        ]);
        for key in &keys {
            assert!(m.sidebar.focus_workspace_key(key));
            assert_eq!(m.sidebar.toggle_broadcast_select(), Some(true));
        }
        assert_eq!(m.sidebar.broadcast_selected_count(), 2);

        // All-skipped broadcast (only the session-less target): the
        // selection survives for a retry.
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: vec![keys[1].clone()],
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let cmds = m.handle_textarea_submitted("hello".into());
        assert!(cmds.is_empty(), "nothing deliverable: {cmds:?}");
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            2,
            "all-skipped send must not clear the marks",
        );

        // Delivered broadcast: selection clears.
        let expected_targets = m.sidebar.selected_broadcast_keys();
        m.mount_broadcast_picker();
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::BroadcastText),
            "empty snippet library skips straight to compose",
        );
        assert_eq!(
            match &m.modal_flow {
                Some(super::super::ModalFlow::Broadcast { draft }) => Some(draft.targets.clone()),
                _ => None,
            },
            Some(expected_targets),
            "targets resolved from the multi-select in sidebar order",
        );
        let cmds = m.handle_textarea_submitted("hello".into());
        assert!(
            cmds.iter()
                .any(|c| matches!(c, IpcCommand::InjectPrompt { .. })),
            "agent target delivered: {cmds:?}",
        );
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            0,
            "successful send consumes the selection",
        );
    }

    /// Esc on the compose step cancels the flow (draft dropped) but
    /// keeps the sidebar selection — the user backed out of composing,
    /// not of selecting.
    #[test]
    fn broadcast_dismiss_drops_draft_but_keeps_selection() {
        let (mut m, keys) = model_with_broadcast_targets(&[Some(
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        )]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        assert_eq!(m.sidebar.toggle_broadcast_select(), Some(true));
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys,
                snippet_key: None,
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let cmds = m.handle_modal_dismissed();
        assert!(cmds.is_empty());
        assert!(m.modal_flow.is_none(), "Esc cancels the flow");
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            1,
            "the marks survive a cancelled compose",
        );
    }

    /// `Shift-B` with nothing selected refuses to mount and nudges
    /// toward `v` instead.
    #[test]
    fn broadcast_with_empty_selection_flashes_a_nudge() {
        let (mut m, _keys) = model_with_broadcast_targets(&[None]);
        m.mount_broadcast_picker();
        assert!(m.modal_stack.is_empty(), "no selection, no modal");
        assert!(m.modal_flow.is_none());
        let notice = m.status.notice.as_ref().expect("nudge notice");
        assert!(notice.message.contains("v"), "nudge names the select key");
    }

    /// Agent-to-agent handoff (`x s`, issue #431), full flow: dispatch
    /// on the focused agent workspace opens the target picker, picking a
    /// target funnels into the compose step, and submit injects the
    /// edited brief into the target session (settle-gated inject +
    /// recap) with a "source → target" notice. The captured seed is
    /// empty in tests (no rendered grid), so the user-composed body is
    /// what gets delivered.
    #[test]
    fn send_to_session_full_flow_injects_into_picked_target() {
        use lazybox_tui_core::action::Action;
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Agent("codex".into())),
        ]);
        // Source = the first agent workspace.
        assert!(m.sidebar.focus_workspace_key(&keys[0]));

        m.dispatch_action(&Action::SendToSession);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::HandoffTarget),
            "dispatch opens the target picker",
        );

        // The source is excluded — a handoff can't loop back to itself;
        // the remaining candidate is keys[1].
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Session(keys[1].clone())]);
        assert!(cmds.is_empty(), "picking the target sends nothing yet");
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::HandoffText),
            "the pick funnels into the compose step",
        );
        assert_eq!(
            match &m.modal_flow {
                Some(super::super::ModalFlow::Handoff { draft }) => draft.target.clone(),
                _ => None,
            },
            Some(keys[1].clone()),
        );

        let cmds = m.handle_textarea_submitted("build the parser".into());
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                IpcCommand::InjectPrompt { terminal_id, prompt, submit: true, .. }
                    if terminal_id.0 == 2 && prompt == "build the parser"
            )),
            "the brief is injected + submitted into the target agent: {cmds:?}",
        );
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                IpcCommand::RecordUserMessage { terminal_id, prompt }
                    if terminal_id.0 == 2 && prompt.text == "build the parser"
            )),
            "the target's recap line updates: {cmds:?}",
        );
        assert!(m.modal_flow.is_none(), "draft consumed");
        let notice = m.status.notice.as_ref().expect("handoff notice");
        assert!(
            notice.message.contains("handoff:") && notice.message.contains('→'),
            "notice records the A→B trail: {}",
            notice.message,
        );
    }

    /// The handoff target picker only offers OTHER workspaces that have
    /// a running session: the source and any session-less workspace are
    /// filtered out.
    #[test]
    fn send_to_session_excludes_source_and_sessionless_targets() {
        use lazybox_tui_core::action::Action;
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Agent("codex".into())),
            None,
        ]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        m.dispatch_action(&Action::SendToSession);
        // Source (keys[0]) and the session-less workspace (keys[2]) are
        // both excluded, leaving only keys[1] — enough to open the
        // picker rather than nudge. Picking it resolves that target.
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::HandoffTarget),
            "one eligible candidate opens the picker",
        );
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Session(keys[1].clone())]);
        assert!(cmds.is_empty(), "picking the target sends nothing yet");
        assert_eq!(
            match &m.modal_flow {
                Some(super::super::ModalFlow::Handoff { draft }) => draft.target.clone(),
                _ => None,
            },
            Some(keys[1].clone()),
            "the only candidate is the other running session",
        );
    }

    /// A handoff whose source is the only running session has nobody to
    /// hand off to: nudge and stash nothing.
    #[test]
    fn send_to_session_with_no_other_session_nudges() {
        use lazybox_tui_core::action::Action;
        let (mut m, keys) = model_with_broadcast_targets(&[Some(
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        )]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        m.dispatch_action(&Action::SendToSession);
        assert!(m.modal_stack.is_empty(), "no target, no picker");
        assert!(m.modal_flow.is_none());
        let notice = m.status.notice.as_ref().expect("nudge notice");
        assert!(
            notice.message.contains("no other running agent"),
            "nudge explains why: {}",
            notice.message,
        );
    }

    /// `x s` on a workspace whose only session is a plain shell has no
    /// agent output to hand off — nudge instead of opening the picker.
    #[test]
    fn send_to_session_from_shell_only_workspace_nudges() {
        use lazybox_tui_core::action::Action;
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Shell),
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
        ]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        m.dispatch_action(&Action::SendToSession);
        assert!(m.modal_stack.is_empty(), "no agent source, no picker");
        assert!(m.modal_flow.is_none());
        let notice = m.status.notice.as_ref().expect("nudge notice");
        assert!(
            notice.message.contains("no agent session"),
            "nudge explains why: {}",
            notice.message,
        );
    }

    /// A shell-only workspace is NOT offered as a handoff target — the
    /// brief is meant for another agent, not a shell prompt. With a
    /// shell as the only other session, the picker refuses to mount.
    #[test]
    fn send_to_session_excludes_shell_targets() {
        use lazybox_tui_core::action::Action;
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Shell),
        ]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        m.dispatch_action(&Action::SendToSession);
        assert!(m.modal_stack.is_empty(), "a shell isn't a handoff target");
        assert!(m.modal_flow.is_none());
        let notice = m.status.notice.as_ref().expect("nudge notice");
        assert!(
            notice.message.contains("no other running agent"),
            "nudge names the reason: {}",
            notice.message,
        );
    }

    /// When the source scrape comes back empty (here: a freshly-spawned
    /// agent with no rendered output yet), the flow flags it rather than
    /// opening a silent empty composer — but still proceeds to the picker
    /// so the user can compose the brief by hand.
    #[test]
    fn send_to_session_flags_an_empty_capture_but_still_opens_the_picker() {
        use lazybox_tui_core::action::Action;
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Agent("codex".into())),
        ]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        m.dispatch_action(&Action::SendToSession);
        let notice = m.status.notice.as_ref().expect("notice");
        assert!(
            notice.message.contains("couldn't capture"),
            "empty scrape is surfaced: {}",
            notice.message,
        );
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::HandoffTarget),
            "the flow still proceeds to the picker",
        );
    }

    /// Clearing the seed and submitting an empty body cancels the
    /// handoff without sending anything.
    #[test]
    fn send_to_session_empty_body_cancels() {
        use lazybox_tui_core::action::Action;
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Agent("codex".into())),
        ]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        m.dispatch_action(&Action::SendToSession);
        m.handle_choice_picked(vec![ChoicePayload::Session(keys[1].clone())]);
        let cmds = m.handle_textarea_submitted("   \n".into());
        assert!(cmds.is_empty(), "empty body sends nothing: {cmds:?}");
        assert!(m.modal_flow.is_none(), "draft consumed even on cancel");
        let notice = m.status.notice.as_ref().expect("cancel notice");
        assert!(notice.message.contains("cancelled"), "{}", notice.message);
    }

    /// If the picked target's session ends between pick and submit, the
    /// composed brief is NOT silently dropped: the picker re-opens seeded
    /// with the edited body so it can be routed to a session that's still
    /// live. Modeled by a draft whose target no longer has a session.
    #[test]
    fn send_to_session_dead_target_reopens_picker_preserving_brief() {
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            None,
            Some(lazybox_ipc::TerminalKind::Agent("codex".into())),
        ]);
        // The user picked keys[1], but its session is gone by submit time.
        m.modal_flow = Some(super::super::ModalFlow::Handoff {
            draft: HandoffDraft {
                source: keys[0].clone(),
                source_name: "planner".into(),
                seed: "original brief".into(),
                target: Some(keys[1].clone()),
            },
        });
        m.modal_stack.push(Id::HandoffText);

        let cmds = m.handle_textarea_submitted("refined brief".into());
        assert!(
            cmds.is_empty(),
            "nothing delivered to a dead target: {cmds:?}"
        );
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::HandoffTarget),
            "the picker re-opens instead of dropping the work",
        );
        assert_eq!(
            match &m.modal_flow {
                Some(super::super::ModalFlow::Handoff { draft }) => Some(draft.seed.as_str()),
                _ => None,
            },
            Some("refined brief"),
            "the edited brief becomes the new seed — not lost",
        );
        // The target slot cleared so the re-opened picker starts unbound;
        // the still-live other session (keys[2]) is the sole candidate.
        assert!(
            match &m.modal_flow {
                Some(super::super::ModalFlow::Handoff { draft }) => draft.target.clone(),
                _ => None,
            }
            .is_none(),
            "the dead target is cleared before the picker re-opens",
        );
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Session(keys[2].clone())]);
        assert!(
            cmds.is_empty(),
            "re-picking the live target sends nothing yet"
        );
        assert_eq!(
            match &m.modal_flow {
                Some(super::super::ModalFlow::Handoff { draft }) => draft.target.clone(),
                _ => None,
            },
            Some(keys[2].clone()),
            "only the still-live other session can be re-picked",
        );
    }

    /// Esc anywhere in the handoff flow drops the stash so a later
    /// handoff starts clean.
    #[test]
    fn send_to_session_dismiss_drops_the_stash() {
        use lazybox_tui_core::action::Action;
        let (mut m, keys) = model_with_broadcast_targets(&[
            Some(lazybox_ipc::TerminalKind::Agent("claude".into())),
            Some(lazybox_ipc::TerminalKind::Agent("codex".into())),
        ]);
        assert!(m.sidebar.focus_workspace_key(&keys[0]));
        m.dispatch_action(&Action::SendToSession);
        m.handle_modal_dismissed();
        assert!(m.modal_flow.is_none(), "Esc on the picker cancels");
    }

    /// A `--connect` client must not launch a local editor against a
    /// server-side worktree path. `e` on a remote client declines with
    /// a notice and issues no command; the same setup on a local client
    /// provisions a worktree (a `Spawn`) so the editor can open. See
    /// #742.
    #[test]
    fn remote_client_declines_local_editor_launch() {
        use lazybox_tui_core::action::Action;

        fn editor_dispatch(remote: bool) -> Vec<IpcCommand> {
            let (client, mut server) = channel::pair();
            let mut model = {
                let m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
                if remote { m.with_remote() } else { m }
            };
            model.cache_editors(vec![crate::editors::EditorTemplate::from(
                crate::editors::UserEditorEntry {
                    id: "test".into(),
                    display: Some("Test".into()),
                    command: "true".into(),
                    args: Some(vec![]),
                },
            )]);
            let key = SessionKey::new("github:o/r#742");
            model.handle_daemon_event(lazybox_ipc::Event::WorkspaceUpserted(Box::new(
                lazybox_core::Workspace::empty(
                    WorkspaceKey::new(key.as_str()),
                    "main",
                    chrono::Utc::now(),
                ),
            )));
            assert!(model.sidebar.focus_workspace_key(&key));
            while server.rx.try_recv().is_ok() {}
            model.dispatch_action(&Action::OpenEditor);
            assert!(
                model.modal_stack.is_empty(),
                "no editor picker mounts for a single detected editor",
            );
            std::iter::from_fn(|| server.rx.try_recv().ok()).collect()
        }

        assert!(
            editor_dispatch(false)
                .iter()
                .any(|c| matches!(c, IpcCommand::Spawn { .. })),
            "local client provisions a worktree so the editor can open",
        );
        assert!(
            !editor_dispatch(true)
                .iter()
                .any(|c| matches!(c, IpcCommand::Spawn { .. })),
            "remote client must not act on a server-side worktree path",
        );
    }

    fn model_with_conversion_source(
        agent: &str,
        on_main: bool,
    ) -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
        SessionKey,
    ) {
        use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind};
        let (client, server) = channel::pair();
        let mut model = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let key = SessionKey::new("github:o/r#649");
        model.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(
            lazybox_core::Workspace::empty(
                WorkspaceKey::new(key.as_str()),
                "main",
                chrono::Utc::now(),
            ),
        )));
        model.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: key.clone(),
            kind: TerminalKind::Agent(agent.into()),
            no_permission: false,
            on_main,
            model_label: None,
        });
        assert!(model.sidebar.focus_workspace_key(&key));
        (model, server, key)
    }

    #[test]
    fn convert_session_uses_authored_handoff_then_replaces_the_source() {
        use lazybox_core::prompts::AgentHandoffRole;
        use lazybox_ipc::{
            AgentRunAccess, AgentRunId, AgentRuntimeMode, Event as IpcEvent, TerminalId,
            TerminalKind,
        };
        use lazybox_tui_core::action::Action;

        let (mut model, mut server, key) = model_with_conversion_source("codex", false);
        while server.rx.try_recv().is_ok() {}

        model.dispatch_action(&Action::ConvertSession);
        assert_eq!(model.modal_stack.last(), Some(&Id::ConvertSessionRole));
        let commands = model
            .handle_choice_picked(vec![ChoicePayload::HandoffRole(AgentHandoffRole::Continue)]);
        let request_id = match commands.as_slice() {
            [
                IpcCommand::StartAgentRun {
                    request_id,
                    session_key,
                    session_id,
                    source_terminal_id,
                    agent,
                    mode,
                    initial_input: Some(input),
                    resume_latest,
                    access,
                    ..
                },
            ] => {
                assert_eq!(session_key, &key);
                assert_eq!(*session_id, None);
                assert_eq!(*source_terminal_id, Some(TerminalId(7)));
                assert_eq!(agent, "codex");
                assert_eq!(*mode, AgentRuntimeMode::StreamJson);
                assert!(*resume_latest);
                assert_eq!(*access, AgentRunAccess::ReadOnly);
                let prompt = input.text.as_deref().expect("handoff request");
                assert!(prompt.contains("## Repository state"));
                assert!(prompt.contains("Return only"));
                request_id.clone()
            }
            other => panic!("expected one structured handoff run, got {other:?}"),
        };

        let source_session_id = lazybox_core::SessionId::new();
        model.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id: request_id.clone(),
            run_id: AgentRunId(11),
            session_key: key.clone(),
            session_id: Some(source_session_id),
            agent: "codex".into(),
            mode: AgentRuntimeMode::StreamJson,
        });
        model.handle_daemon_event(IpcEvent::AgentAssistantTextDelta {
            run_id: AgentRunId(11),
            delta: "## Goal\nFinish the parser\n\n## Repository state\nsrc/parser.rs modified"
                .into(),
        });
        model.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(11),
            result: None,
            session_id: Some("thread-649".into()),
            error: None,
        });

        let after_handoff: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
        assert!(after_handoff.iter().any(|command| matches!(
            command,
            IpcCommand::InterruptAgentRun {
                run_id: AgentRunId(11)
            }
        )));
        assert!(after_handoff.iter().any(|command| matches!(
            command,
            IpcCommand::Close {
                terminal_id: TerminalId(7),
                ..
            }
        )));
        assert!(
            !after_handoff
                .iter()
                .any(|command| matches!(command, IpcCommand::Spawn { .. })),
            "the replacement waits until the singleton source has exited",
        );

        model.handle_daemon_event(IpcEvent::TerminalExited {
            terminal_id: TerminalId(7),
            exit_code: None,
            last_output: None,
        });
        let spawn = std::iter::from_fn(|| server.rx.try_recv().ok())
            .find(|command| matches!(command, IpcCommand::Spawn { .. }))
            .expect("fresh role agent spawned after source exit");
        match spawn {
            IpcCommand::Spawn {
                session_key,
                session_id,
                client_request_id,
                kind: TerminalKind::Agent(agent),
                initial_prompt: Some(prompt),
                access,
                ..
            } => {
                assert_eq!(session_key, key);
                assert_eq!(session_id, Some(source_session_id));
                assert_eq!(client_request_id.as_deref(), Some(request_id.0.as_str()));
                assert_eq!(agent, "codex");
                assert_eq!(access, AgentRunAccess::Default);
                assert!(prompt.contains("continuing in-progress work"));
                assert!(prompt.contains("Finish the parser"));
                assert!(
                    prompt
                        .contains("<untrusted-content source=\"agent-authored session handoff\">")
                );
            }
            other => panic!("expected seeded agent spawn, got {other:?}"),
        }

        model.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(8),
            session_key: key,
            kind: TerminalKind::Agent("codex".into()),
            no_permission: true,
            on_main: false,
            model_label: None,
        });
        assert!(
            model.conversion.is_some(),
            "an unrelated terminal lifecycle event cannot complete the request"
        );
        model.handle_daemon_event(IpcEvent::CommandCompleted {
            client_request_id: request_id.0,
        });
        assert!(model.conversion.is_none());
        let notice = model.status.notice.as_ref().expect("conversion trail");
        assert!(notice.message.contains("→ continue codex"));
    }

    #[test]
    fn failed_authored_handoff_leaves_the_source_agent_running() {
        use lazybox_core::prompts::AgentHandoffRole;
        use lazybox_ipc::{AgentRunId, AgentRuntimeMode, Event as IpcEvent, TerminalId};
        use lazybox_tui_core::action::Action;

        let (mut model, mut server, key) = model_with_conversion_source("claude", false);
        while server.rx.try_recv().is_ok() {}
        model.dispatch_action(&Action::ConvertSession);
        let commands =
            model.handle_choice_picked(vec![ChoicePayload::HandoffRole(AgentHandoffRole::Critic)]);
        let request_id = match &commands[0] {
            IpcCommand::StartAgentRun { request_id, .. } => request_id.clone(),
            other => panic!("expected StartAgentRun, got {other:?}"),
        };
        model.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id,
            run_id: AgentRunId(12),
            session_key: key,
            session_id: Some(lazybox_core::SessionId::new()),
            agent: "claude".into(),
            mode: AgentRuntimeMode::StreamJson,
        });
        model.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(12),
            result: None,
            session_id: None,
            error: Some("resume failed".into()),
        });

        let commands: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
        assert!(commands.iter().any(|command| matches!(
            command,
            IpcCommand::InterruptAgentRun {
                run_id: AgentRunId(12)
            }
        )));
        assert!(
            !commands.iter().any(|command| matches!(
                command,
                IpcCommand::Close { .. } | IpcCommand::Spawn { .. }
            ))
        );
        assert!(model.terminals.terminal_is_agent(TerminalId(7)));
        assert!(model.conversion.is_none());
        assert!(
            model
                .status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("handoff failed"))
        );
    }

    #[test]
    fn failed_conversion_target_spawn_releases_the_conversion_latch() {
        use lazybox_core::prompts::AgentHandoffRole;
        use lazybox_ipc::{AgentRunId, AgentRuntimeMode, Event as IpcEvent};
        use lazybox_tui_core::action::Action;

        let (mut model, mut server, key) = model_with_conversion_source("codex", false);
        model.dispatch_action(&Action::ConvertSession);
        let commands =
            model.handle_choice_picked(vec![ChoicePayload::HandoffRole(AgentHandoffRole::Critic)]);
        let request_id = match &commands[0] {
            IpcCommand::StartAgentRun { request_id, .. } => request_id.clone(),
            other => panic!("expected StartAgentRun, got {other:?}"),
        };
        model.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id: request_id.clone(),
            run_id: AgentRunId(13),
            session_key: key.clone(),
            session_id: Some(lazybox_core::SessionId::new()),
            agent: "codex".into(),
            mode: AgentRuntimeMode::StreamJson,
        });
        model.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(13),
            result: Some("## Goal\nReview the parser".into()),
            session_id: None,
            error: None,
        });
        model.handle_daemon_event(IpcEvent::TerminalExited {
            terminal_id: lazybox_ipc::TerminalId(7),
            exit_code: None,
            last_output: None,
        });
        assert!(model.conversion.is_some());

        let spawn = std::iter::from_fn(|| server.rx.try_recv().ok())
            .find(|command| matches!(command, IpcCommand::Spawn { .. }))
            .expect("critic target spawn");
        assert!(matches!(
            spawn,
            IpcCommand::Spawn {
                access: lazybox_ipc::AgentRunAccess::ReadOnly,
                client_request_id: Some(ref id),
                ..
            } if id == &request_id.0
        ));

        model.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: lazybox_ipc::TerminalId(99),
            session_key: key,
            kind: lazybox_ipc::TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
        model.handle_daemon_event(IpcEvent::CommandFailed {
            client_request_id: "another-spawn".into(),
            message: "unrelated".into(),
        });
        assert!(model.conversion.is_some());

        model.handle_daemon_event(IpcEvent::CommandFailed {
            client_request_id: request_id.0,
            message: "backend unavailable".into(),
        });

        assert!(model.conversion.is_none());
        assert!(
            model
                .status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("backend unavailable"))
        );
    }

    #[test]
    fn conversion_follows_a_source_workspace_rebadge() {
        use lazybox_core::prompts::AgentHandoffRole;
        use lazybox_ipc::{AgentRunId, AgentRuntimeMode, Event as IpcEvent};
        use lazybox_tui_core::action::Action;

        let (mut model, mut server, source) = model_with_conversion_source("codex", false);
        while server.rx.try_recv().is_ok() {}
        model.dispatch_action(&Action::ConvertSession);
        let commands = model
            .handle_choice_picked(vec![ChoicePayload::HandoffRole(AgentHandoffRole::Continue)]);
        let request_id = match &commands[0] {
            IpcCommand::StartAgentRun { request_id, .. } => request_id.clone(),
            other => panic!("expected StartAgentRun, got {other:?}"),
        };
        let source_session_id = lazybox_core::SessionId::new();
        model.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id,
            run_id: AgentRunId(14),
            session_key: source.clone(),
            session_id: Some(source_session_id),
            agent: "codex".into(),
            mode: AgentRuntimeMode::StreamJson,
        });
        let target = SessionKey::new("github:o/r#650");
        model.handle_daemon_event(IpcEvent::TerminalsRebadged {
            from: source,
            to: target.clone(),
        });
        model.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(14),
            result: Some("## Goal\nContinue the parser".into()),
            session_id: Some("thread-649".into()),
            error: None,
        });
        model.handle_daemon_event(IpcEvent::TerminalExited {
            terminal_id: lazybox_ipc::TerminalId(7),
            exit_code: None,
            last_output: None,
        });

        let spawn = std::iter::from_fn(|| server.rx.try_recv().ok())
            .find(|command| matches!(command, IpcCommand::Spawn { .. }))
            .expect("replacement spawn");
        assert!(matches!(
            spawn,
            IpcCommand::Spawn {
                session_key,
                session_id: Some(id),
                ..
            } if session_key == target && id == source_session_id
        ));
    }

    #[test]
    fn conversion_close_failure_keeps_the_source_terminal_usable() {
        use lazybox_core::prompts::AgentHandoffRole;
        use lazybox_ipc::{AgentRunId, AgentRuntimeMode, Event as IpcEvent, TerminalId};
        use lazybox_tui_core::action::Action;

        let (mut model, _server, key) = model_with_conversion_source("claude", false);
        model.dispatch_action(&Action::ConvertSession);
        let commands =
            model.handle_choice_picked(vec![ChoicePayload::HandoffRole(AgentHandoffRole::Critic)]);
        let request_id = match &commands[0] {
            IpcCommand::StartAgentRun { request_id, .. } => request_id.clone(),
            other => panic!("expected StartAgentRun, got {other:?}"),
        };
        model.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id: request_id.clone(),
            run_id: AgentRunId(15),
            session_key: key,
            session_id: Some(lazybox_core::SessionId::new()),
            agent: "claude".into(),
            mode: AgentRuntimeMode::StreamJson,
        });
        model.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(15),
            result: Some("## Goal\nReview the parser".into()),
            session_id: None,
            error: None,
        });
        model.handle_daemon_event(IpcEvent::CommandFailed {
            client_request_id: request_id.0,
            message: "could not close source terminal".into(),
        });

        assert!(model.conversion.is_none());
        model.handle_daemon_event(IpcEvent::TerminalExited {
            terminal_id: TerminalId(7),
            exit_code: Some(1),
            last_output: Some("agent stopped".into()),
        });
        assert!(
            model.terminals.terminal_is_agent(TerminalId(7)),
            "a failed replacement close must not classify a later natural exit as a user close"
        );
    }

    #[test]
    fn oversized_authored_handoff_is_interrupted_before_source_close() {
        use lazybox_core::prompts::AgentHandoffRole;
        use lazybox_ipc::{AgentRunId, AgentRuntimeMode, Event as IpcEvent};
        use lazybox_tui_core::action::Action;

        let (mut model, mut server, key) = model_with_conversion_source("codex", false);
        while server.rx.try_recv().is_ok() {}
        model.dispatch_action(&Action::ConvertSession);
        let commands = model
            .handle_choice_picked(vec![ChoicePayload::HandoffRole(AgentHandoffRole::Continue)]);
        let request_id = match &commands[0] {
            IpcCommand::StartAgentRun { request_id, .. } => request_id.clone(),
            other => panic!("expected StartAgentRun, got {other:?}"),
        };
        model.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id,
            run_id: AgentRunId(16),
            session_key: key,
            session_id: Some(lazybox_core::SessionId::new()),
            agent: "codex".into(),
            mode: AgentRuntimeMode::StreamJson,
        });
        model.handle_daemon_event(IpcEvent::AgentAssistantTextDelta {
            run_id: AgentRunId(16),
            delta: "x".repeat(128 * 1024 + 1),
        });

        assert!(model.conversion.is_none());
        let commands: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
        assert!(commands.iter().any(|command| matches!(
            command,
            IpcCommand::InterruptAgentRun {
                run_id: AgentRunId(16)
            }
        )));
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, IpcCommand::Close { .. }))
        );
    }

    #[test]
    fn convert_session_rejects_agents_without_structured_resume() {
        use lazybox_tui_core::action::Action;
        let (mut model, _server, _key) = model_with_conversion_source("cursor", false);

        model.dispatch_action(&Action::ConvertSession);

        assert!(model.modal_stack.is_empty());
        assert!(
            model
                .status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("does not support"))
        );
    }

    /// mount_snippet_picker with an empty collection flashes a hint
    /// and refuses to mount — no Id::SnippetPicker on the stack.
    /// This is the "user typed `]]s<key>` but never configured any
    /// snippets" UX.
    #[test]
    fn mount_snippet_picker_with_empty_collection_skips_mount() {
        let mut m = build_model();
        m.mount_snippet_picker(String::new());
        assert!(
            !matches!(m.modal_stack.last(), Some(Id::SnippetPicker)),
            "empty snippet library shouldn't open a picker"
        );
    }

    /// mount_snippet_picker mounts the picker when snippets exist. Each
    /// row now carries its own `ChoicePayload::Text(key)` (#512), so
    /// there is no parallel render-order stash for the pick to index
    /// into — resolution is by payload, exercised end-to-end in the
    /// snippet-dispatch tests above.
    #[test]
    fn mount_snippet_picker_with_snippets_mounts() {
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
    }

    // ── Skills picker (issue #797) ──────────────────────────────────

    /// `mount_skill_picker` with no active terminal flashes a hint and
    /// refuses to mount — skills only make sense inside an agent session.
    #[test]
    fn mount_skill_picker_without_terminal_skips_mount() {
        let mut m = build_model();
        m.mount_skill_picker(String::new());
        assert!(
            !matches!(m.modal_stack.last(), Some(Id::SkillPicker)),
            "no active terminal shouldn't open a skills picker",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("no active terminal")),
        );
    }

    /// `mount_skill_picker` on a non-agent (shell) terminal nudges the
    /// user rather than opening — a shell can't invoke a skill.
    #[test]
    fn mount_skill_picker_on_shell_terminal_skips_mount() {
        let mut m = model_with_active_terminal_and_snippet(
            "skill-shell",
            "snippets:\n  x:\n    description: d\n    body: b\n",
            lazybox_ipc::TerminalKind::Shell,
        );
        m.modal_stack.clear();
        m.mount_skill_picker(String::new());
        assert!(
            !matches!(m.modal_stack.last(), Some(Id::SkillPicker)),
            "a shell terminal shouldn't open a skills picker",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("agent sessions")),
        );
    }

    /// Picking a skill injects the explicit "Use the `<skill>` skill."
    /// instruction through the settle-gated agent path and floats the
    /// skill into the session-local Recent MRU.
    #[test]
    fn skill_pick_injects_explicit_trigger_and_records_recent() {
        let mut m = model_with_active_terminal_and_snippet(
            "skill-pick",
            "snippets:\n  x:\n    description: d\n    body: b\n",
            lazybox_ipc::TerminalKind::Agent("claude".into()),
        );
        // Swap the picker on top for the skills picker so the choice
        // dispatch resolves against `PickFlow::Skill`.
        m.modal_stack.pop();
        m.modal_stack.push(Id::SkillPicker);
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("code-review".into())]);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                IpcCommand::InjectPrompt { terminal_id, prompt, .. }
                    if *terminal_id == lazybox_ipc::TerminalId(1)
                        && prompt == "Use the `code-review` skill."
            )),
            "skill trigger must inject the explicit instruction, got {cmds:?}",
        );
        assert_eq!(m.recent_skills, vec!["code-review".to_string()]);
    }

    /// A temp worktree carrying one repo skill under `.claude/skills/`,
    /// plus a model whose focused agent terminal is rooted there — the
    /// fixture for the end-to-end `]]l` chord tests.
    fn tmp_worktree_with_skill(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lazybox-skilltest-{}-{}", std::process::id(), tag));
        let skill_dir = dir.join(".claude").join("skills").join("code-review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: code-review\ndescription: Review a diff.\n---\nbody\n",
        )
        .unwrap();
        dir
    }

    fn model_with_agent_at_worktree(
        worktree: std::path::PathBuf,
    ) -> Model<tuirealm::terminal::TestTerminalAdapter> {
        use lazybox_ipc::{Event as IpcEvent, TerminalId};
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#1");
        let session_key: SessionKey = (&ws_key).into();
        let mut ws = lazybox_core::Workspace::empty(ws_key.clone(), "main", chrono::Utc::now());
        ws.add_session(lazybox_core::WorkspaceSession::new(
            ws_key,
            lazybox_core::SessionKind::Agent {
                agent_id: "claude".into(),
            },
            worktree,
            chrono::Utc::now(),
        ));
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![ws],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(m.sidebar.focus_workspace_key(&session_key));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key,
            kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m
    }

    /// End-to-end: arming the leader and pressing `l` opens the skills
    /// picker. Exercises the real key routing (`handle_pane_key`), which is
    /// where the original #797 defect lived — `k` was shadowed by the
    /// popup-navigation letters, so the direct chord silently did nothing.
    #[test]
    fn leader_l_opens_skill_picker_end_to_end() {
        let worktree = tmp_worktree_with_skill("e2e");
        let mut m = model_with_agent_at_worktree(worktree);
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        assert!(m.terminal_leader_pending(), "`]]` arms the leader");
        m.dispatch_key(RealmKey::new(Key::Char('l'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "`]]l` consumed the leader");
        assert!(
            matches!(m.top_modal(), Some(Id::SkillPicker)),
            "`]]l` opens the skills picker",
        );
    }

    /// `k` stays popup-highlight navigation inside the armed leader — it
    /// must NOT resolve to the skills command (the shadowing that made the
    /// original `]]k` binding dead). The leader stays armed and no picker
    /// opens.
    #[test]
    fn leader_k_navigates_and_never_opens_skill_picker() {
        let worktree = tmp_worktree_with_skill("k-nav");
        let mut m = model_with_agent_at_worktree(worktree);
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        m.dispatch_key(RealmKey::new(Key::Char('k'), RealmMods::NONE));
        assert!(
            m.terminal_leader_pending(),
            "`k` navigates the popup and keeps the leader armed",
        );
        assert!(
            m.top_modal().is_none(),
            "`]]k` must not open the skills picker",
        );
    }

    /// Over `--connect` the agent's worktree and `~/.claude/skills` live on
    /// the daemon host, so a local scan would surface the wrong machine's
    /// skills. The picker refuses to mount and names the reason, even
    /// though a scannable skill exists locally.
    #[test]
    fn skill_picker_is_unavailable_over_remote() {
        let worktree = tmp_worktree_with_skill("remote");
        let mut m = model_with_agent_at_worktree(worktree).with_remote();
        m.mount_skill_picker(String::new());
        assert!(
            !matches!(m.modal_stack.last(), Some(Id::SkillPicker)),
            "a remote client shouldn't scan the local filesystem for skills",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("--connect")),
        );
    }

    /// #1100: the remote gate is per-app, not per-feature. A `{url}` app
    /// (open the PR in a browser) has no worktree dependency, so it must
    /// NOT be refused on a remote daemon the way a `{path}` app is — the
    /// same reasoning that lets `g o` open-in-browser work over `--connect`.
    /// The favorite-key / direct path launches without the picker's
    /// availability filter, so on this PR-less workspace the `{url}` app
    /// falls through to the launcher and fails on the missing token — NOT
    /// on a remote block, proving it was never remote-gated.
    #[test]
    fn open_with_url_app_is_not_remote_blocked() {
        let worktree = tmp_worktree_with_skill("ow-url-remote");
        let mut m = model_with_agent_at_worktree(worktree).with_remote();
        m.cache_open_with(vec![crate::editors::OpenWithApp {
            name: "PR in browser".into(),
            command: "open".into(),
            args: Some(vec!["{url}".into()]),
            key: None,
        }]);
        m.dispatch_action(&lazybox_tui_core::action::Action::OpenWithApp(
            "PR in browser".into(),
        ));
        let notice = m
            .status
            .notice
            .as_ref()
            .map(|notice| notice.message.clone())
            .unwrap_or_default();
        assert!(
            !notice.contains("remote daemon"),
            "a {{url}} app must not be refused on a remote daemon: {notice:?}",
        );
        assert!(
            notice.contains("unavailable"),
            "it should fall through to the missing-{{url}} token error: {notice:?}",
        );
    }

    /// #1100: a `{path}` app IS worktree-bound — the worktree is a
    /// server-side path over `--connect`, so it declines on a remote
    /// daemon and points at the server shell.
    #[test]
    fn open_with_path_app_declines_on_remote() {
        let worktree = tmp_worktree_with_skill("ow-path-remote");
        let mut m = model_with_agent_at_worktree(worktree).with_remote();
        m.cache_open_with(vec![crate::editors::OpenWithApp {
            name: "Obsidian".into(),
            command: "open".into(),
            args: Some(vec!["-a".into(), "Obsidian".into(), "{path}".into()]),
            key: None,
        }]);
        m.open_with_picker();
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("remote daemon")),
            "a {{path}} app must decline on a remote daemon",
        );
    }

    /// #1100: a `{path}` app on a workspace with no session yet has no
    /// worktree on disk to open. Rather than error, it provisions one
    /// (spawns a shell) and defers the launch — exactly what `e` does.
    #[test]
    fn open_with_path_app_without_a_worktree_provisions_one() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#7");
        let session_key: SessionKey = (&ws_key).into();
        // No `add_session` — the workspace has no worktree on disk.
        let ws = lazybox_core::Workspace::empty(ws_key, "main", chrono::Utc::now());
        m.handle_daemon_event(lazybox_ipc::Event::Snapshot {
            workspaces: vec![ws],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(m.sidebar.focus_workspace_key(&session_key));
        m.cache_open_with(vec![crate::editors::OpenWithApp {
            name: "Obsidian".into(),
            command: "open".into(),
            args: None,
            key: None,
        }]);
        m.open_with_picker();
        assert_eq!(
            m.setup
                .pending_open_with_launch
                .as_ref()
                .map(|(key, _)| key.clone()),
            Some(session_key),
            "the launch is queued behind a worktree provision",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("Provisioning")),
        );
    }

    /// #1100: the queued `{path}` launch fires once the provisioned
    /// worktree lands (a `TerminalSpawned` for the target), resolving its
    /// worktree by key — the deferred-launch counterpart of `e`.
    #[test]
    fn open_with_deferred_launch_fires_when_the_worktree_lands() {
        let worktree = tmp_worktree_with_skill("ow-deferred");
        let mut m = model_with_agent_at_worktree(worktree);
        let session_key: SessionKey = (&WorkspaceKey::new("github:o/r#1")).into();
        // A prior provision left this launch waiting; the command can't
        // spawn, so nothing real is launched — we only assert it fired.
        m.setup.pending_open_with_launch = Some((
            session_key.clone(),
            crate::editors::OpenWithApp {
                name: "Obsidian".into(),
                command: "/nonexistent-open-with-launcher".into(),
                args: Some(vec!["{path}".into()]),
                key: None,
            },
        ));
        m.handle_daemon_event(lazybox_ipc::Event::TerminalSpawned {
            model_label: None,
            terminal_id: lazybox_ipc::TerminalId(2),
            session_key,
            kind: lazybox_ipc::TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        assert!(
            m.setup.pending_open_with_launch.is_none(),
            "the deferred launch fired and cleared once the worktree existed",
        );
    }

    /// #1100 picker polish: apps whose tokens the workspace can't supply
    /// are filtered out, so the picker never offers a choice that would
    /// just fail. Two `{url}` apps on a PR-less workspace leave nothing to
    /// run — no picker mounts, and the notice explains why.
    #[test]
    fn open_with_picker_filters_out_unavailable_apps() {
        let worktree = tmp_worktree_with_skill("ow-filter");
        let mut m = model_with_agent_at_worktree(worktree);
        let url_app = |name: &str| crate::editors::OpenWithApp {
            name: name.into(),
            command: "open".into(),
            args: Some(vec!["{url}".into()]),
            key: None,
        };
        m.cache_open_with(vec![url_app("PR"), url_app("PR2")]);
        m.open_with_picker();
        assert!(
            m.top_modal().is_none(),
            "no picker mounts when every app's token is unavailable",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("can run")),
        );
    }

    /// #1100 per-app key: dispatching `OpenWithApp("<name>")` (what a
    /// favorite `key:` binds) launches that specific app directly. Here
    /// the workspace has no PR, so the `{url}` app surfaces the named
    /// token error — proving the key path reached the launcher.
    #[test]
    fn open_with_favorite_key_launches_the_named_app() {
        let worktree = tmp_worktree_with_skill("ow-fav-key");
        let mut m = model_with_agent_at_worktree(worktree);
        m.cache_open_with(vec![crate::editors::OpenWithApp {
            name: "PR".into(),
            command: "open".into(),
            args: Some(vec!["{url}".into()]),
            key: Some("O".into()),
        }]);
        m.dispatch_action(&lazybox_tui_core::action::Action::OpenWithApp("PR".into()));
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|notice| notice.message.contains("unavailable")),
            "the favorite key reached launch_open_with for the named app",
        );
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

    /// `]]s` opens the snippet picker and disarms the leader (#252).
    #[test]
    fn leader_s_opens_snippet_picker() {
        let mut m = model_in_terminal_with_snippets("leader-char");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        m.dispatch_key(RealmKey::new(Key::Char('s'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert!(matches!(m.top_modal(), Some(Id::SnippetPicker)));
    }

    // ── Sidebar `]]` leader parity (issue #871) ─────────────────────

    /// Two agent workspaces (`github:o/r#1` → terminal 1, `github:o/r#2`
    /// → terminal 2), a one-snippet library, sidebar focused with the
    /// cursor on workspace #1 — the fixture that proves a sidebar `]]s`
    /// addresses the cursor workspace's agent, not the other workspace's.
    fn model_two_agent_workspaces_sidebar(
        label: &str,
    ) -> Model<tuirealm::terminal::TestTerminalAdapter> {
        use lazybox_ipc::{Event as IpcEvent, TerminalId};
        let mut m = build_model();
        m.apply_snippets(snippets_from_yaml(
            label,
            "snippets:\n  rev:\n    description: Review\n    body: review the diff\n",
        ));
        let a = WorkspaceKey::new("github:o/r#1");
        let b = WorkspaceKey::new("github:o/r#2");
        let a_key: SessionKey = (&a).into();
        let b_key: SessionKey = (&b).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![
                lazybox_core::Workspace::empty(a, "main", chrono::Utc::now()),
                lazybox_core::Workspace::empty(b, "main", chrono::Utc::now()),
            ],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        for (tid, key) in [(1, &a_key), (2, &b_key)] {
            m.handle_daemon_event(IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(tid),
                session_key: key.clone(),
                kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
                no_permission: false,
                on_main: false,
            });
        }
        m.set_focus(PaneFocus::Sidebar);
        assert!(m.sidebar.focus_workspace_key(&a_key));
        m
    }

    /// `]]` arms in the sidebar too, and its menu offers only the
    /// workspace-addressed subset (snippets/skills/recall/history/urls) —
    /// no terminal-only tile/focus rows.
    #[test]
    fn sidebar_double_bracket_arms_leader_with_workspace_subset() {
        let mut m = model_two_agent_workspaces_sidebar("sidebar-arm");
        m.dispatch_key(esc_key());
        assert!(!m.terminal_leader_pending(), "one `]` only holds");
        m.dispatch_key(esc_key());
        assert!(m.terminal_leader_pending(), "`]]` arms the sidebar leader");
        assert_eq!(m.focus(), PaneFocus::Sidebar, "stays in the sidebar");
        let rows = m.terminal_leader_menu_rows();
        let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["s", "l", "r", "h", "u"],
            "sidebar menu is the workspace-addressed subset only",
        );
    }

    /// The headline acceptance: `]]s` from the sidebar delivers the picked
    /// snippet to the *cursor* workspace's agent (terminal 1) — NOT the
    /// active/focused terminal (terminal 2) — via `DeliverSnippet`.
    #[test]
    fn sidebar_snippet_send_delivers_to_cursor_workspace_only() {
        use lazybox_ipc::TerminalId;
        let mut m = model_two_agent_workspaces_sidebar("sidebar-send");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        m.dispatch_key(RealmKey::new(Key::Char('s'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert!(matches!(m.top_modal(), Some(Id::SnippetPicker)));
        assert_eq!(
            m.leader_target,
            Some(TerminalId(1)),
            "retarget resolves the cursor workspace, not the active terminal",
        );
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Text("rev".into())]);
        match cmds.as_slice() {
            [
                IpcCommand::DeliverSnippet {
                    terminal_id,
                    body,
                    submit,
                    ..
                },
            ] => {
                assert_eq!(
                    *terminal_id,
                    TerminalId(1),
                    "delivered to the cursor workspace"
                );
                assert_eq!(body, "review the diff");
                assert!(*submit, "a picked snippet submits");
            }
            other => panic!("expected one DeliverSnippet to terminal 1, got {other:?}"),
        }
        // The retarget is scoped to that one pick — the next picker falls
        // back to the focused terminal.
        assert_eq!(m.leader_target, None, "leader_target cleared on modal pop");
    }

    /// #1077 regression: with a `v` multi-select active, `]]s` from the
    /// sidebar fans the snippet out over the WHOLE selection (via the
    /// broadcast picker) instead of the single-cursor retarget — which
    /// hit one row and left the rest to fall into `w w` / a spawn (the
    /// reported "snippet-on-one, w-w-on-others" bug).
    #[test]
    fn sidebar_snippet_send_under_multiselect_fans_out_via_broadcast() {
        let mut m = model_two_agent_workspaces_sidebar("sidebar-multi");
        let a_key: SessionKey = SessionKey::from("github:o/r#1");
        let b_key: SessionKey = SessionKey::from("github:o/r#2");
        for key in [&a_key, &b_key] {
            assert!(m.sidebar.focus_workspace_key(key));
            m.sidebar.toggle_broadcast_select();
        }
        assert_eq!(m.sidebar.broadcast_selected_count(), 2);

        let mut cmds = Vec::new();
        m.run_sidebar_leader_cmd(
            crate::realm::model::terminal_leader::LeaderCmd::Snippets,
            &mut cmds,
        );
        assert!(cmds.is_empty(), "picking hasn't happened yet");
        assert_eq!(
            m.top_modal(),
            Some(&Id::BroadcastSnippet),
            "a live multi-select fans out through the broadcast picker",
        );
        assert_eq!(
            m.leader_target, None,
            "no single-cursor retarget when the selection is what we act on",
        );
    }

    /// A terminal-only chord (`]]f` focus mode) is inert from the sidebar:
    /// it isn't offered and resolves to nothing rather than toggling.
    #[test]
    fn sidebar_leader_ignores_terminal_only_commands() {
        let mut m = model_two_agent_workspaces_sidebar("sidebar-inert");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        m.dispatch_key(RealmKey::new(Key::Char('f'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert!(!m.focus_mode, "`]]f` does nothing from the sidebar");
        assert_eq!(m.focus(), PaneFocus::Sidebar);
    }

    /// A lone `]` in the sidebar keeps its browse meaning: once resolved
    /// (here by the next non-`]` key) it opens the read-only snippet
    /// browser rather than arming the leader.
    #[test]
    fn sidebar_lone_bracket_opens_snippet_browser() {
        let mut m = model_two_agent_workspaces_sidebar("sidebar-browse");
        m.dispatch_key(esc_key());
        assert!(!m.terminal_leader_pending());
        m.dispatch_key(RealmKey::new(Key::Char('j'), RealmMods::NONE));
        assert!(
            matches!(m.top_modal(), Some(Id::SnippetBrowser)),
            "a lone `]` browses the snippet library",
        );
    }

    /// Session-less but spawnable cursor workspace: `]]s` reuses the
    /// broadcast flow (#836 spawn-if-none) rather than dropping the send.
    #[test]
    fn sidebar_snippet_send_session_less_falls_back_to_broadcast() {
        use lazybox_ipc::Event as IpcEvent;
        let mut m = build_model();
        m.apply_snippets(snippets_from_yaml(
            "sidebar-sessionless",
            "snippets:\n  rev:\n    description: Review\n    body: review the diff\n",
        ));
        let a = WorkspaceKey::new("github:o/r#1");
        let a_key: SessionKey = (&a).into();
        // A repo/project scope makes the session-less workspace spawnable
        // (`worktree_scope().is_some()`), the #836 case broadcast covers.
        let mut ws = lazybox_core::Workspace::empty(a, "main", chrono::Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github("o", "r"));
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![ws],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        m.set_focus(PaneFocus::Sidebar);
        assert!(m.sidebar.focus_workspace_key(&a_key));
        let mut cmds = Vec::new();
        m.run_sidebar_leader_cmd(
            crate::realm::model::terminal_leader::LeaderCmd::Snippets,
            &mut cmds,
        );
        assert!(
            matches!(m.top_modal(), Some(Id::BroadcastSnippet)),
            "a session-less workspace routes through the broadcast flow",
        );
    }

    /// A lone `]` held in the sidebar is dropped the moment focus leaves —
    /// it must not resolve in the pane you moved to (a stray literal `]`
    /// flushed into an agent, or a spurious browser). Both panes arm the
    /// shared escape latch now (#871), so this cross-pane leak is guarded
    /// by disarming on any focus change.
    #[test]
    fn sidebar_held_bracket_is_dropped_on_focus_change() {
        let mut m = model_two_agent_workspaces_sidebar("sidebar-focuschg");
        m.dispatch_key(esc_key());
        assert!(m.escape_latch.is_armed(), "the lone `]` is held");
        assert!(
            !m.terminal_leader_pending(),
            "one `]` doesn't arm the leader"
        );
        m.set_focus(PaneFocus::Terminals);
        assert!(
            !m.escape_latch.is_armed(),
            "a focus change drops the held `]` so it can't leak into the terminal",
        );
    }

    /// A sidebar `]]l` on a workspace whose only terminal is a shell is
    /// refused (skills need an agent). The refused mount must leave no
    /// `leader_target` behind, or the next picker would misdirect.
    #[test]
    fn sidebar_skill_on_shell_leaves_no_stale_target() {
        let mut m = model_with_active_terminal_and_snippet(
            "sidebar-skill-shell",
            "snippets:\n  rev:\n    description: d\n    body: b\n",
            lazybox_ipc::TerminalKind::Shell,
        );
        m.modal_stack.clear();
        let mut cmds = Vec::new();
        m.run_sidebar_leader_cmd(
            crate::realm::model::terminal_leader::LeaderCmd::Skills,
            &mut cmds,
        );
        assert!(
            !matches!(m.top_modal(), Some(Id::SkillPicker)),
            "a shell workspace refuses the skill picker",
        );
        assert_eq!(
            m.leader_target, None,
            "a refused mount leaves no stale retarget",
        );
    }

    /// `]]<unbound>` — a key that isn't a leader command — cancels back
    /// to the terminal without opening the picker or leaving (#252). Only
    /// `s`/`f`/`q`/`x`/digit/`` ` ``/the split-and-arrow tile chords are
    /// commands now.
    #[test]
    fn leader_then_unbound_key_cancels_back_to_terminal() {
        let mut m = model_in_terminal_with_snippets("leader-unbound");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        m.dispatch_key(RealmKey::new(Key::Char('r'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert!(m.top_modal().is_none(), "unbound `]]r` opens no picker");
        assert_eq!(m.focus(), PaneFocus::Terminals, "stays in the terminal");
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

    /// The `]]` leader is non-timed (#252): an armed leader with no
    /// follow-up key stays armed across idle ticks and never leaves on
    /// its own, so a user reading the popup and deciding which command
    /// to press is never yanked to the sidebar mid-decision. `]]q` is
    /// the explicit exit (see `leader_q_leaves_to_sidebar`).
    #[test]
    fn idle_leader_stays_armed_and_never_leaves_on_tick() {
        let mut m = model_in_terminal_with_snippets("leader-idle");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        assert!(m.terminal_leader_pending());
        // Ticking — however long the pane sits idle — must not leave.
        std::thread::sleep(std::time::Duration::from_millis(3));
        m.tick_terminal_leader();
        assert!(m.terminal_leader_pending(), "non-timed leader stays armed");
        assert_eq!(m.focus(), PaneFocus::Terminals, "idle leader never leaves");
    }

    /// `]]q` is the explicit exit to the sidebar, replacing the old
    /// idle-tick leave (#252).
    #[test]
    fn leader_q_leaves_to_sidebar() {
        let mut m = model_in_terminal_with_snippets("leader-quit");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        assert!(m.terminal_leader_pending(), "`]]` arms the leader");
        m.dispatch_key(RealmKey::new(Key::Char('q'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "`]]q` consumes the leader");
        assert_eq!(m.focus(), PaneFocus::Sidebar, "`]]q` leaves to the sidebar");
        assert!(m.top_modal().is_none(), "no picker mounted on exit");
    }

    /// `]]s` still opens the picker even when a long time passes between
    /// the leader and the `s` — the race that made the picker "flash and
    /// vanish" is gone because the leader no longer times out (#252).
    #[test]
    fn leader_then_delayed_s_still_opens_picker() {
        let mut m = model_in_terminal_with_snippets("leader-delay");
        m.dispatch_key(esc_key());
        m.dispatch_key(esc_key());
        // Simulate an idle gap (PTY output flooding, a thoughtful user):
        // ticks fire but must not disarm the leader.
        for _ in 0..5 {
            m.tick_terminal_leader();
        }
        assert!(m.terminal_leader_pending(), "leader survived the idle gap");
        m.dispatch_key(RealmKey::new(Key::Char('s'), RealmMods::NONE));
        assert!(matches!(m.top_modal(), Some(Id::SnippetPicker)));
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

    #[test]
    fn encode_snippet_trims_blank_prefix_but_preserves_first_line_indentation() {
        assert_eq!(
            super::super::inputs::encode_snippet_for_pty("\n \tcommand"),
            b"command\r",
        );
        assert_eq!(
            super::super::inputs::encode_snippet_for_pty("    indented command"),
            b"    indented command\r",
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
    //! The daemon emits one `TerminalOutput` per PTY chunk and can keep
    //! the bounded inbound channel continuously non-empty. The run loop used
    //! to drain it with an unbounded `while let Ok(..)`, so under sustained agent output
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
                first_seq: seq as u64,
                seq: seq as u64,
            })
            .expect("bounded channel must have room for the flood");
        }
    }

    fn flood_size() -> usize {
        let flooded = (MAX_EVENTS_PER_TICK * 4).min(EVENT_CHANNEL_CAPACITY - 1);
        assert!(
            flooded > MAX_EVENTS_PER_TICK,
            "inbound capacity must retain more than one drain tick"
        );
        flooded
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
        let flooded = flood_size();
        flood(&evt_tx, flooded);

        // One iteration's drain: must report a backlog (more queued)…
        assert!(
            drain_daemon_events(&mut m, &mut Vec::new(), || false),
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

        let flooded = flood_size();
        flood(&evt_tx, flooded);

        // Bound the loop generously above the minimum needed (4) so a
        // genuinely stuck drain trips the assert instead of hanging.
        let mut backlog = true;
        let mut iterations = 0;
        while backlog {
            backlog = drain_daemon_events(&mut m, &mut Vec::new(), || false);
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
        drain_daemon_events(&mut m, &mut Vec::new(), || false);
        assert_eq!(m.event_backlog.resyncs(), 3);
    }

    /// A GitHub sync burst upserts many workspaces in one drain batch.
    /// The sidebar's O(N log N) visible-list rebuild must run ONCE for the
    /// whole batch, not once per upsert — otherwise a full sweep is
    /// O(N²) on the single UI thread and the "drain" phase blows the
    /// frame budget, freezing keyboard input during sync (#1030).
    #[test]
    fn sync_burst_rebuilds_visible_list_once() {
        use chrono::Utc;
        use lazybox_core::{Workspace, WorkspaceKey};

        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();
        let before = m.sidebar.recompute_count();

        // Well under the per-tick drain cap so the whole burst drains in
        // one iteration (exercising batch coalescing, not the cap).
        let n = 20;
        for i in 0..n {
            let ws = Workspace::empty(
                WorkspaceKey::new(format!("github:o/r#{i}")),
                "main",
                Utc::now(),
            );
            evt_tx
                .try_send(Event::WorkspaceUpserted(Box::new(ws)))
                .expect("bounded channel must have room for the burst");
        }

        assert!(
            !drain_daemon_events(&mut m, &mut Vec::new(), || false),
            "a sub-cap burst must drain in a single tick"
        );
        assert!(
            m.client.rx.try_recv().is_err(),
            "the whole burst should have been consumed"
        );
        // Every upsert landed — coalescing must not drop workspaces.
        assert_eq!(
            m.sidebar.visible_workspace_count(),
            n,
            "all upserted workspaces must be in the visible list"
        );
        // …and the whole batch rebuilt the visible list exactly once,
        // instead of once per upsert.
        assert_eq!(
            m.sidebar.recompute_count() - before,
            1,
            "a sync burst must coalesce to one visible-list rebuild"
        );
    }

    /// A `WorkspaceFocusRequested` that lands in the SAME drain batch as
    /// the upsert of its target must still focus that row. The upsert's
    /// visible-list rebuild is coalesced (deferred to the batch flush), so
    /// the focus request's by-key scan has to self-heal that pending
    /// rebuild — otherwise it reads a stale list without the row, the jump
    /// silently fails, and the cursor is left elsewhere (#1030).
    #[test]
    fn focus_request_lands_on_a_row_upserted_in_the_same_batch() {
        use chrono::Utc;
        use lazybox_core::{SessionKey, Workspace, WorkspaceKey};

        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();
        let key = WorkspaceKey::new("github:o/r#7");
        let sk: SessionKey = (&key).into();

        evt_tx
            .try_send(Event::WorkspaceUpserted(Box::new(Workspace::empty(
                key,
                "main",
                Utc::now(),
            ))))
            .expect("room for the upsert");
        evt_tx
            .try_send(Event::WorkspaceFocusRequested {
                session_key: sk.clone(),
            })
            .expect("room for the focus request");

        drain_daemon_events(&mut m, &mut Vec::new(), || false);

        assert_eq!(
            m.sidebar.selected_workspace_key(),
            Some(&sk),
            "focus must land on a row upserted earlier in the same drain batch"
        );
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
        let mut carried_in = vec![carried];
        let backlog = drain_daemon_events(&mut m, &mut carried_in, || false);
        assert!(!backlog, "a single carried event is no backlog");
        assert!(carried_in.is_empty(), "the carried event was consumed");
        // The resync was dispatched + observed — proof the carried
        // event didn't get dropped on the floor.
        assert_eq!(m.event_backlog.resyncs(), 1);
    }

    /// Seed the channel with `n` distinct workspace upserts (never
    /// coalesced, unlike same-terminal output), so the dispatch loop has
    /// `n` real iterations a keystroke can interrupt between.
    fn flood_upserts(tx: &mpsc::Sender<Event>, n: usize) {
        use chrono::Utc;
        use lazybox_core::{Workspace, WorkspaceKey};
        for i in 0..n {
            let ws = Workspace::empty(
                WorkspaceKey::new(format!("github:o/r#{i}")),
                "main",
                Utc::now(),
            );
            tx.try_send(Event::WorkspaceUpserted(Box::new(ws)))
                .expect("room for the upsert");
        }
    }

    /// #1031 §6 / #1055: the collection cap bounds how much we pull off
    /// the channel, but *handling* a batch is unbounded — so a keystroke
    /// arriving mid-batch must pre-empt the dispatch loop, not wait out the
    /// whole burst. With input already pending, the drain dispatches one
    /// event (progress is guaranteed) and carries the rest over untouched.
    #[test]
    fn pending_input_preempts_the_dispatch_loop_and_carries_the_rest() {
        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();
        let n = 5;
        flood_upserts(&evt_tx, n);

        let mut carried = Vec::new();
        // Input is waiting the whole time → yield after the first event.
        let backlog = drain_daemon_events(&mut m, &mut carried, || true);

        assert!(backlog, "an un-dispatched tail is a backlog");
        assert_eq!(
            m.sidebar.visible_workspace_count(),
            1,
            "exactly one event dispatched before yielding to the waiting keystroke",
        );
        assert_eq!(
            carried.len(),
            n - 1,
            "the rest of the batch is carried, not dropped",
        );
    }

    /// The pre-emption must still converge: even with input pending on
    /// EVERY check, each drain dispatches at least one event, so repeated
    /// drains eventually process the whole batch with nothing lost — the
    /// daemon stream can't be starved by a busy keyboard either.
    #[test]
    fn constant_input_still_drains_every_event_one_at_a_time() {
        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();
        let n = 5;
        flood_upserts(&evt_tx, n);

        let mut carried = Vec::new();
        let mut iterations = 0;
        loop {
            let backlog = drain_daemon_events(&mut m, &mut carried, || true);
            iterations += 1;
            assert!(
                iterations <= 32,
                "constant pre-emption never converged — no progress guarantee"
            );
            if !backlog {
                break;
            }
        }
        assert!(carried.is_empty(), "no event left carried");
        assert!(m.client.rx.try_recv().is_err(), "channel fully consumed");
        assert_eq!(
            m.sidebar.visible_workspace_count(),
            n,
            "every upsert eventually landed despite pre-emption on every event",
        );
        assert_eq!(iterations, n, "one event per drain under constant input");
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
            first_seq: seq,
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
mod wake_burst_liveness_tests {
    //! Issue #1045: the whole-app freeze after sleep/wake was a wake
    //! catch-up burst flooding the single UI thread WHILE a `Loading`
    //! modal awaited a result that never landed. Neither half alone
    //! reproduces it — the freeze was the two together — so this drives
    //! the REAL model through both at once on a single instance:
    //!
    //! - the burst can't monopolise the thread — with host input
    //!   pending, one drain dispatches a single event and carries the
    //!   rest (`if input_pending() { break; }`, #1055), so `Esc` and the
    //!   modal's own tick still get a turn instead of waiting out the
    //!   whole burst; and
    //! - the mounted modal can't wait forever — its timeout resolves to
    //!   `Msg::LoadingTimedOut` (the component side of that is proven in
    //!   `loading.rs`), and here the MODEL must actually pop the modal on
    //!   that message — the wiring the component test can't see — even
    //!   while the burst tail is still queued.
    //!
    //! The modal is held silent with its producer ALIVE (the #1045
    //! "produced-but-lost / task stalled" shape, distinct from a dropped
    //! sender, which `TakeOutcome::Cancelled` already covers).
    use super::super::helpers::{MAX_EVENTS_PER_TICK, drain_daemon_events};
    use super::super::{Id, Model};
    use crate::realm::Msg;
    use crate::realm::components::loading::Loading;
    use lazybox_core::{Workspace, WorkspaceKey};
    use lazybox_ipc::{Client, EVENT_CHANNEL_CAPACITY, Event, TerminalId};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tuirealm::application::PollStrategy;
    use tuirealm::ratatui::layout::Size;

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

    /// A wake catch-up burst of DISTINCT workspace upserts. Distinct
    /// matters: same-terminal `TerminalOutput` coalesces into one event
    /// (`coalesce_adjacent_output`), so it can't exercise per-event
    /// pre-emption — the drain would dispatch the single merged event and
    /// never re-check input. Upserts never coalesce, so the dispatch loop
    /// has `n` real iterations a pending keystroke can break out of.
    fn wake_burst_of_upserts(tx: &mpsc::Sender<Event>, n: usize) {
        use chrono::Utc;
        for i in 0..n {
            let ws = Workspace::empty(
                WorkspaceKey::new(format!("github:o/r#{i}")),
                "main",
                Utc::now(),
            );
            tx.try_send(Event::WorkspaceUpserted(Box::new(ws)))
                .expect("bounded channel must have room for the burst");
        }
    }

    #[test]
    fn wake_burst_preempts_input_while_a_mounted_loading_modal_self_dismisses() {
        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();

        // A `Loading` modal is up on the model, mounted under its real
        // `Id::Setup` (the id the setup flow mounts it under), awaiting a
        // value that will never land — producer held alive but silent.
        let (loading, _never_sends) = Loading::pending("waking…");
        m.mount_modal(Id::Setup, loading);
        assert_eq!(
            m.top_modal(),
            Some(&Id::Setup),
            "the loading modal is mounted before the burst",
        );

        // The wake burst floods the UI thread's inbound channel.
        let n = 128;
        wake_burst_of_upserts(&evt_tx, n);

        // Responsiveness: with a keystroke pending the whole time, ONE
        // drain dispatches exactly one event and carries the rest — the
        // burst cannot hold the thread (and thus `Esc` / the modal's
        // tick) hostage for its full length.
        let mut carried = Vec::new();
        let backlog = drain_daemon_events(&mut m, &mut carried, || true);
        assert!(backlog, "an un-dispatched tail is a backlog");
        assert_eq!(
            carried.len(),
            n - 1,
            "input pre-empts after one event: the rest is carried, not drained in one shot",
        );
        assert_eq!(
            m.sidebar.visible_workspace_count(),
            1,
            "exactly one burst event applied before yielding to the waiting keystroke",
        );

        // Liveness: `Msg::LoadingTimedOut` must dismiss the mounted modal
        // through the model — not merely flash a notice and leave it up —
        // even with the burst tail still queued.
        m.update(Msg::LoadingTimedOut);
        assert!(
            m.top_modal().is_none(),
            "a timed-out loading modal must be dismissed by the model, not left orphaned",
        );
        assert!(
            !carried.is_empty(),
            "the modal cleared while the burst tail was still pending — mid-burst, not after it drained",
        );
    }

    /// A wake catch-up burst of the exact signal the #1131 warning names:
    /// the daemon dropping `TerminalOutput` on a full channel and emitting
    /// one `TerminalResync` per busy terminal per congestion episode as the
    /// client falls behind. Cycled over a handful of terminals to mirror
    /// several busy terminals resyncing at once; `TerminalResync` is never
    /// coalesced (only adjacent `TerminalOutput` is), so every event is
    /// counted by the `BacklogMonitor` regardless of the terminal it names.
    fn overflow_burst(tx: &mpsc::Sender<Event>, n: usize) {
        for i in 0..n {
            tx.try_send(Event::TerminalResync {
                terminal_id: TerminalId((i % 4) as u64 + 1),
                replay: b"recovered grid".to_vec(),
                seq: i as u64 + 1,
            })
            .expect("bounded channel must have room for the burst");
        }
    }

    /// Fill the inbound channel to one below capacity — a burst larger
    /// than any single bounded drain can consume, so a tail is always left
    /// behind (the proof the drain stayed bounded, #1113 D-0). The
    /// precondition asserted here is the real architectural invariant the
    /// test rests on: the channel holds more than one drain's worth, so an
    /// overflow episode outlives a single drain. It stays true across any
    /// value of `MAX_EVENTS_PER_TICK` short of the channel capacity itself,
    /// rather than coincidentally at today's 256-vs-512.
    fn burst_over_one_tick() -> usize {
        const {
            assert!(
                EVENT_CHANNEL_CAPACITY - 1 > MAX_EVENTS_PER_TICK,
                "channel capacity must exceed one drain's cap for a burst to outlast a single drain",
            )
        };
        EVENT_CHANNEL_CAPACITY - 1
    }

    /// Drive the bounded drain the way the run loop does — once per
    /// iteration — until the overflow episode registers. A single drain
    /// normally dispatches the whole first batch and registers it, but a
    /// scheduler stall in the two statements before the drain takes its
    /// first event off the channel could collect zero events, and thus
    /// zero resyncs, on that call. Looping until one lands makes the
    /// "overflow registered" assertion independent of that timing. Every
    /// drain must still report a backlog: the burst is sized to outlast one
    /// drain, and a drain that registers no resync also took nothing, so
    /// the next drain still sees the whole burst queued.
    fn drain_until_overflow_registers(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) {
        let mut carried = Vec::new();
        for _ in 0..4 {
            assert!(
                drain_daemon_events(m, &mut carried, || false),
                "a bounded drain must leave the overflow burst's tail queued",
            );
            if m.event_backlog.resyncs() > 0 {
                return;
            }
        }
        panic!("overflow episode never registered across repeated bounded drains");
    }

    /// #1131: an overflow burst on focus-regain must not strand a `Loading`
    /// modal. Its liveness backstop — driven by the modal's OWN tick, which
    /// keeps firing every loop iteration since #1120 moved the render flush
    /// off the UI thread — must still time out and be popped by the model
    /// while the burst tail is still queued and unread.
    #[test]
    fn overflow_burst_still_lets_a_loading_modal_time_out() {
        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();

        // Modal awaits a value that never lands — producer held alive but
        // silent (the #1045 "produced-but-lost / stalled" shape). A tiny
        // timeout crosses on the first real listener tick instead of
        // waiting out the production 60s.
        let (loading, _never_sends) = Loading::pending("waking…");
        let loading = loading.timeout(Duration::from_millis(1));
        m.mount_modal(Id::Setup, loading);

        overflow_burst(&evt_tx, burst_over_one_tick());

        // Drain stays bounded every call (a tail remains) and registers
        // the overflow episode — so this is genuinely the #1131 signal,
        // not an ordinary sub-cap batch.
        drain_until_overflow_registers(&mut m);
        assert!(
            m.top_modal().is_some(),
            "the modal is still up right after the burst drain",
        );

        // Drive the modal's own tick (as the run loop does every
        // iteration). The timeout backstop must resolve to
        // `Msg::LoadingTimedOut` and the model must pop the modal — without
        // the queued burst tail being drained first.
        let queued_before = m.client.rx.len();
        assert!(queued_before > 0, "the burst tail is still queued");
        let mut saw_timeout = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while m.top_modal().is_some() && std::time::Instant::now() < deadline {
            if let Ok(messages) = m.app.tick(PollStrategy::Once(Duration::from_millis(20))) {
                for msg in messages {
                    if matches!(msg, Msg::LoadingTimedOut) {
                        saw_timeout = true;
                    }
                    m.update(msg);
                }
            }
        }
        assert!(
            saw_timeout,
            "the modal's own tick must fire its timeout backstop",
        );
        assert!(
            m.top_modal().is_none(),
            "a timed-out loading modal must be popped by the model, not left orphaned",
        );
        assert_eq!(
            m.client.rx.len(),
            queued_before,
            "the modal cleared with the burst tail still queued — its liveness is independent of the burst draining",
        );
    }

    /// #1131: the other liveness escape hatch — `Esc` must still dismiss a
    /// `Loading` modal mid-overflow-burst. Uses the production 60s timeout
    /// so the ONLY thing that can pop the modal within the test is the
    /// user's keypress reaching it through the real listener pipeline while
    /// the burst tail is still queued.
    #[test]
    fn overflow_burst_leaves_a_loading_modal_esc_dismissable() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();

        let (loading, _never_sends) = Loading::pending("waking…");
        m.mount_modal(Id::Setup, loading);

        overflow_burst(&evt_tx, burst_over_one_tick());
        drain_until_overflow_registers(&mut m);
        assert!(m.top_modal().is_some(), "the modal is up before Esc");

        // Esc through the real modal pipeline (listener → app.tick →
        // Msg::ModalDismissed → model). The burst tail is still queued and
        // undrained, yet Esc must reach the modal and pop it.
        let queued_before = m.client.rx.len();
        assert!(queued_before > 0, "the burst tail is still queued");
        m.dispatch_modal_key(KeyEvent::new(Key::Esc, KeyModifiers::NONE));
        assert!(
            m.top_modal().is_none(),
            "Esc must dismiss the loading modal mid-overflow-burst",
        );
        assert_eq!(
            m.client.rx.len(),
            queued_before,
            "Esc cleared the modal with the burst tail still queued — not by draining it first",
        );
    }

    /// #1146: exercise the production work phase, not a hand-written subset.
    /// A wedged render writer must defer a frame while `app.tick` remains live
    /// enough to expire and pop a loading modal during an overflow episode.
    #[test]
    fn run_loop_step_skips_backpressured_paint_while_modal_times_out() {
        use super::super::helpers::{
            PerfMonitor, PhaseTimings, RENDER_BACKPRESSURE_CAP, RenderThrottle, StaleInputTally,
            run_loop_step,
        };
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let (mut m, evt_tx, _cmd_rx) = model_with_event_sender();
        let (loading, _never_sends) = Loading::pending("waking…");
        m.mount_modal(Id::Setup, loading.timeout(Duration::from_millis(1)));
        overflow_burst(&evt_tx, burst_over_one_tick());
        m.render_pending = Some(Arc::new(AtomicUsize::new(RENDER_BACKPRESSURE_CAP + 1)));

        let (_input_tx, mut input_rx) = tokio::sync::mpsc::channel(1);
        let mut carried = Vec::new();
        let mut stale_tally = StaleInputTally::default();
        let mut perf = PerfMonitor::new();
        let mut throttle = RenderThrottle::default();
        let mut redraw_is_input = false;
        let mut timings = PhaseTimings::default();
        let mut saw_skip = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);

        while m.top_modal().is_some() && std::time::Instant::now() < deadline {
            let outcome = run_loop_step(
                &mut m,
                &mut carried,
                &mut input_rx,
                &mut stale_tally,
                &mut perf,
                &mut throttle,
                &mut redraw_is_input,
                &mut timings,
            );
            if outcome.skipped_for_backpressure {
                saw_skip = true;
                assert!(m.redraw, "a skipped paint must remain pending");
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        assert!(saw_skip, "writer backpressure engaged the real skip path");
        assert!(
            m.redraw,
            "the wedged writer never consumed the pending frame"
        );
        assert!(
            m.top_modal().is_none(),
            "the modal timed out while painting remained backpressured"
        );
    }
}

#[cfg(test)]
mod coalesce_tests {
    //! `coalesce_adjacent_output` collapses a streaming burst into one
    //! dispatch per terminal — this is what keeps memory bounded under
    //! a chatty agent. The merge must be byte-for-byte faithful and
    //! must NOT reorder across terminals or non-output events.
    use super::super::helpers::coalesce_adjacent_output;
    use super::super::*;
    use lazybox_ipc::{Event, TerminalId};

    fn out(id: u64, bytes: &[u8], seq: u64) -> Event {
        Event::TerminalOutput {
            terminal_id: TerminalId(id),
            bytes: bytes.to_vec(),
            first_seq: seq,
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
                first_seq,
                seq,
            } => {
                assert_eq!(*terminal_id, TerminalId(1));
                assert_eq!(bytes, b"hello world");
                assert_eq!(*first_seq, 10, "merged event keeps its first seq");
                assert_eq!(*seq, 12, "merged event carries the last chunk's seq");
            }
            other => panic!("expected one TerminalOutput, got {other:?}"),
        }
    }

    /// Coalescing must not erase the only evidence of a missing chunk.
    /// Keeping non-contiguous ranges separate lets TerminalStack reject
    /// the second range and wait for an authoritative resync.
    #[test]
    fn sequence_gap_ends_a_same_terminal_run() {
        let merged = coalesce_adjacent_output(vec![out(1, b"one", 1), out(1, b"three", 3)]);
        assert_eq!(merged.len(), 2);
        assert!(matches!(
            &merged[0],
            Event::TerminalOutput {
                first_seq: 1,
                seq: 1,
                ..
            }
        ));
        assert!(matches!(
            &merged[1],
            Event::TerminalOutput {
                first_seq: 3,
                seq: 3,
                ..
            }
        ));
    }

    #[test]
    fn client_gap_dispatches_one_daemon_resync_request() {
        let (client, mut server) = lazybox_ipc::channel::pair();
        let mut model = Model::new_for_test(client, tuirealm::ratatui::layout::Size::new(120, 40))
            .expect("model");
        while server.rx.try_recv().is_ok() {} // initial Subscribe
        let session_key = lazybox_core::SessionKey::new("s");
        model
            .terminals
            .set_active_session(Some(session_key.clone()));
        model.terminals.on_daemon_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(1),
            session_key,
            kind: lazybox_ipc::TerminalKind::Shell,
            no_permission: false,
            on_main: false,
            model_label: None,
        });

        model.handle_daemon_event(out(1, b"one", 1));
        model.handle_daemon_event(out(1, b"three", 3));
        assert!(matches!(
            server.rx.try_recv(),
            Ok(IpcCommand::RequestTerminalResync {
                requests,
            }) if requests == vec![lazybox_ipc::TerminalResyncRequest {
                terminal_id: TerminalId(1),
                required_seq: 3,
            }]
        ));
        assert!(
            server.rx.try_recv().is_err(),
            "one request per recovery episode"
        );
    }

    #[test]
    fn unavailable_snapshot_immediately_requests_terminal_repair() {
        let (client, mut server) = lazybox_ipc::channel::pair();
        let mut model = Model::new_for_test(client, tuirealm::ratatui::layout::Size::new(120, 40))
            .expect("model");
        while server.rx.try_recv().is_ok() {} // initial Subscribe

        model.handle_daemon_event(Event::Snapshot {
            workspaces: Vec::new(),
            projects: Vec::new(),
            terminals: vec![lazybox_ipc::TerminalSnapshot {
                terminal_id: TerminalId(9),
                session_key: lazybox_core::SessionKey::new("quiet"),
                kind: lazybox_ipc::TerminalKind::Shell,
                replay: Vec::new(),
                last_seq: 17,
                replay_available: false,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: Vec::new(),
                composing_buffer: None,
                agent_state: None,
                authenticating: false,
            }],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        assert!(matches!(
            server.rx.try_recv(),
            Ok(IpcCommand::RequestTerminalResync {
                requests,
            }) if requests == vec![lazybox_ipc::TerminalResyncRequest {
                terminal_id: TerminalId(9),
                required_seq: 17,
            }]
        ));
        assert!(server.rx.try_recv().is_err(), "exactly one repair request");
    }

    /// #1171: a replay-budgeted reconnect snapshot can omit dozens of quiet
    /// terminals at once. Recovery must consume one command slot for the
    /// whole set, not overflow the 32-deep lane with one request per terminal.
    #[test]
    fn unavailable_snapshot_batches_a_resync_storm_into_one_command() {
        let (client, mut server) = lazybox_ipc::channel::pair();
        let mut model = Model::new_for_test(client, tuirealm::ratatui::layout::Size::new(120, 40))
            .expect("model");
        while server.rx.try_recv().is_ok() {} // initial Subscribe

        let count = lazybox_ipc::COMMAND_CHANNEL_CAPACITY + 17;
        let terminals = (1..=count)
            .map(|number| lazybox_ipc::TerminalSnapshot {
                terminal_id: TerminalId(number as u64),
                session_key: lazybox_core::SessionKey::new(format!("quiet-{number}")),
                kind: lazybox_ipc::TerminalKind::Shell,
                replay: Vec::new(),
                last_seq: number as u64,
                replay_available: false,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: Vec::new(),
                composing_buffer: None,
                agent_state: None,
                authenticating: false,
            })
            .collect();

        model.handle_daemon_event(Event::Snapshot {
            workspaces: Vec::new(),
            projects: Vec::new(),
            terminals,
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        let command = server.rx.try_recv().expect("one batched recovery command");
        let IpcCommand::RequestTerminalResync { requests } = command else {
            panic!("expected resync batch, got {command:?}");
        };
        assert_eq!(requests.len(), count);
        assert!(server.rx.try_recv().is_err(), "the storm used one slot");
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
                last_output: None,
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
            assert!(!should_drop_stale_input(&ev, Duration::ZERO, false));
            assert!(!should_drop_stale_input(
                &ev,
                STALE_INPUT_MAX_AGE - Duration::from_millis(1),
                false,
            ));
        }
    }

    /// Keys and mouse events buffered past the bound are dropped —
    /// this is what keeps a buffered quit chord (or a backlog of
    /// clicks) from firing when a frozen loop recovers.
    #[test]
    fn stale_keys_and_mouse_are_dropped() {
        let age = STALE_INPUT_MAX_AGE + Duration::from_secs(2);
        assert!(should_drop_stale_input(&key_event(), age, false));
        assert!(should_drop_stale_input(&mouse_event(), age, false));
    }

    /// Paste is deliberate content (dropping it loses user data) and
    /// focus events describe current terminal state — both survive a
    /// stall regardless of age.
    #[test]
    fn stale_paste_and_focus_are_kept() {
        let age = STALE_INPUT_MAX_AGE + Duration::from_secs(2);
        assert!(!should_drop_stale_input(
            &Event::Paste("body".into()),
            age,
            false
        ));
        assert!(!should_drop_stale_input(&Event::FocusGained, age, false));
        assert!(!should_drop_stale_input(&Event::FocusLost, age, false));
    }

    /// A stale Esc is ALWAYS delivered — on a confirm modal
    /// (`modal_retains_keys == false`) and in a pane alike. Esc only
    /// cancels / dismisses / backs out, so it carries none of the
    /// destructive-replay risk that makes a stale `Enter`/`Y` unsafe. This
    /// guarantees the user can always escape a modal that felt frozen under
    /// a stall instead of having to kill the terminal. A stale destructive
    /// key (`Enter`) on the same confirm still drops.
    #[test]
    fn stale_esc_always_survives_so_a_modal_can_always_be_dismissed() {
        let age = STALE_INPUT_MAX_AGE + Duration::from_secs(2);
        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            !should_drop_stale_input(&esc, age, false),
            "a stale Esc must survive on a confirm modal so the user can bail out",
        );
        assert!(
            !should_drop_stale_input(&esc, age, true),
            "a stale Esc survives on a retaining modal too",
        );
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            should_drop_stale_input(&enter, age, false),
            "a stale Enter must never replay a merge/archive confirm",
        );
    }

    /// #1055: while a filter-picker owns input, a buffered keystroke is
    /// kept, not dropped — its content is stable so the key still lands
    /// where the user aimed it. A stale *mouse* click stays dropped even
    /// then (its target coordinates can be wrong), and the picker
    /// exemption never rescues pane input.
    #[test]
    fn stale_key_survives_for_a_retaining_modal() {
        let age = STALE_INPUT_MAX_AGE + Duration::from_secs(2);
        assert!(
            !should_drop_stale_input(&key_event(), age, true),
            "a picker's buffered key must reach it",
        );
        assert!(
            should_drop_stale_input(&mouse_event(), age, true),
            "a stale click's coordinates can still be wrong",
        );
        assert!(
            should_drop_stale_input(&key_event(), age, false),
            "pane input is unaffected by the picker exemption",
        );
    }

    /// Exhaustive classification of every modal `Id` for the stale-key
    /// exemption (#1055). The `match` has no wildcard on purpose: a
    /// newly-added `Id` fails to compile here until it is deliberately
    /// classified, so the retain allowlist can never silently drift from
    /// its criterion (a new picker keeping the latency bug, or — worse — a
    /// new confirm quietly opting itself in). `true` iff a late buffered
    /// `Enter` can only advance a local, single-step, reversible selection;
    /// every confirm, destructive-action menu, and outward-effect input is
    /// `false`.
    #[test]
    fn stale_key_retention_is_classified_for_every_modal() {
        use crate::realm::model::Id;

        let expected = |id: &Id| -> bool {
            match id {
                // Retain: local, single-step, reversible selections whose
                // whole interaction is the keystroke.
                Id::SnippetPicker
                | Id::SkillPicker
                | Id::SnippetBrowser
                | Id::JumpPicker
                | Id::PromptHistoryPicker
                | Id::UrlPicker
                | Id::ThemePicker
                | Id::FilterMenu
                | Id::SnoozeDuration
                | Id::DefaultAgentPicker
                | Id::DefaultModelPicker
                | Id::WorkAgentPicker => true,
                // Drop — confirms (a stale Enter must not confirm).
                Id::AgentAuth
                | Id::RemoveOutOfScope
                | Id::MergeConfirm
                | Id::CleanWorktreesConfirm
                | Id::InspectConfirm
                | Id::ImportCheckoutConfirm
                | Id::ActionConfirm
                | Id::ConflictResolve
                | Id::ErrorInboxClearConfirm
                | Id::BroadcastConfirm
                | Id::BulkSpawnConfirm
                | Id::ClaimedSpawnConfirm
                | Id::EditorRemoveConfirm
                | Id::HelpActionConfirm => false,
                // Drop — destructive-action menus / delete-routing lists.
                Id::SidebarContext | Id::InspectList | Id::ImportCheckoutList => false,
                // Drop — outward-effect inputs (post/label/deliver).
                Id::Reply
                | Id::Notes
                | Id::RequestReviewers
                | Id::AddAssignees
                | Id::ManageLabels
                | Id::PolicyPicker
                | Id::BroadcastText
                | Id::HandoffText => false,
                // Drop — text/config inputs and multi-step flow steps.
                Id::NewWorkspace
                | Id::RenameWorkspace
                | Id::MoveToSpace
                | Id::NewProject
                | Id::NewWorkspaceRepo
                | Id::LinearTeamRepo
                | Id::Editor
                // Open-with launches an external app — an outward
                // effect a stale Enter must not trigger.
                | Id::OpenWith
                | Id::EditorsPanel
                | Id::EditorForm
                | Id::Setup
                | Id::AdoptTarget
                | Id::StartAgentProject
                | Id::LlmGatewayUrl
                | Id::AddScanRoot
                | Id::BroadcastSnippet
                | Id::HandoffTarget
                | Id::ConvertSessionRole
                | Id::SandboxProviderPick
                | Id::SandboxInput
                | Id::SandboxConfirm => false,
                // Drop — read-only / progress / streamed surfaces.
                Id::Splash
                | Id::Help
                | Id::HelpAsk
                | Id::Error
                | Id::Update
                | Id::Polling
                | Id::Tour
                | Id::SyncStatus
                | Id::Messages
                | Id::ErrorInbox
                | Id::InspectLoading
                | Id::WorktreeProgress
                | Id::DescriptionModal
                | Id::DiffReview
                | Id::PrChat => false,
            }
        };

        for id in [
            Id::Splash,
            Id::Help,
            Id::HelpAsk,
            Id::Error,
            Id::AgentAuth,
            Id::Update,
            Id::Polling,
            Id::Reply,
            Id::Notes,
            Id::NewWorkspace,
            Id::RenameWorkspace,
            Id::MoveToSpace,
            Id::NewProject,
            Id::NewWorkspaceRepo,
            Id::LinearTeamRepo,
            Id::Editor,
            Id::OpenWith,
            Id::EditorsPanel,
            Id::EditorForm,
            Id::EditorRemoveConfirm,
            Id::Setup,
            Id::RemoveOutOfScope,
            Id::MergeConfirm,
            Id::AdoptTarget,
            Id::StartAgentProject,
            Id::RequestReviewers,
            Id::AddAssignees,
            Id::ManageLabels,
            Id::FilterMenu,
            Id::PolicyPicker,
            Id::SnoozeDuration,
            Id::LlmGatewayUrl,
            Id::AddScanRoot,
            Id::SidebarContext,
            Id::CleanWorktreesConfirm,
            Id::InspectLoading,
            Id::InspectList,
            Id::InspectConfirm,
            Id::ImportCheckoutList,
            Id::ImportCheckoutConfirm,
            Id::ActionConfirm,
            Id::ConflictResolve,
            Id::SnippetPicker,
            Id::SkillPicker,
            Id::Tour,
            Id::SyncStatus,
            Id::Messages,
            Id::ErrorInbox,
            Id::ErrorInboxClearConfirm,
            Id::WorktreeProgress,
            Id::JumpPicker,
            Id::PromptHistoryPicker,
            Id::UrlPicker,
            Id::ThemePicker,
            Id::SnippetBrowser,
            Id::BroadcastSnippet,
            Id::BroadcastText,
            Id::BroadcastConfirm,
            Id::BulkSpawnConfirm,
            Id::ClaimedSpawnConfirm,
            Id::HandoffTarget,
            Id::HandoffText,
            Id::ConvertSessionRole,
            Id::DefaultAgentPicker,
            Id::DefaultModelPicker,
            Id::HelpActionConfirm,
            Id::WorkAgentPicker,
            Id::DescriptionModal,
            Id::DiffReview,
            Id::PrChat,
        ] {
            assert_eq!(
                id.retains_stale_keys(),
                expected(&id),
                "{id:?} classified inconsistently — update retains_stale_keys and this match together",
            );
        }
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
            ..Default::default()
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

    #[test]
    fn authoritative_github_repo_drives_hyphenated_project_header() {
        let mut m = build_model();
        let pk = ProjectKey::github("codefly-dev", "warden-platform");
        let mut project = Project::github("codefly-dev", "warden-platform", chrono::Utc::now());
        project.name = "codefly/dev-warden-platform".to_string();

        m.handle_daemon_event(IpcEvent::ProjectUpserted(Box::new(project)));

        assert_eq!(
            m.projects.get(&pk).and_then(Project::github_repo),
            Some("codefly-dev/warden-platform")
        );
        assert_eq!(
            m.sidebar.project_label_for(&pk).as_deref(),
            Some("codefly-dev/warden-platform")
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(m.projects.contains_key(&pk), "snapshot seeds the project");

        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
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
    use super::super::ChoicePayload;
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

    #[test]
    fn github_rate_limit_event_replaces_spinner_without_sync_error() {
        let mut m = build_model();
        m.status.polling = None;
        m.pending_refresh_ack = true;
        m.handle_daemon_event(IpcEvent::PollProgress {
            source: "github".into(),
            message: "Fetching issues".into(),
        });
        assert!(m.status.bg_poll.is_some());

        m.handle_daemon_event(IpcEvent::GithubRateLimitWait {
            remaining: 98,
            limit: 5000,
            reset_at: chrono::Utc::now() + chrono::Duration::minutes(7),
        });

        assert!(m.status.bg_poll.is_none());
        assert!(m.status.github_rate_limit_wait.is_some());
        assert!(!m.pending_refresh_ack);
        assert!(
            m.status.notice.is_none(),
            "an intentional wait must not raise a sync-failed notice"
        );
        assert!(matches!(
            m.status
                .sync
                .latest_per_source()
                .first()
                .map(|entry| &entry.outcome),
            Some(crate::realm::status_ctx::SyncOutcome::RateLimited {
                remaining: 98,
                limit: 5000,
                ..
            })
        ));

        m.handle_daemon_event(IpcEvent::PollProgress {
            source: "github".into(),
            message: "Fetching issues".into(),
        });
        assert!(m.status.github_rate_limit_wait.is_none());
        assert!(m.status.bg_poll.is_some());
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

        m.dispatch_modal_key(key(Key::Esc));
        assert!(m.top_modal().is_none(), "Esc closes the picker");
        assert!(m.theme_picker_prev.is_none(), "restore stash is consumed");
    }

    /// The "Change default agent" settings action routes straight to
    /// the single-pick agent picker, pre-positioned on the current
    /// default and with the enabled agent ids stashed for the pick.
    /// Disk-free: mounting only reads the sidebar's current default.
    #[test]
    fn edit_default_agent_action_mounts_the_picker() {
        use crate::realm::setup_ctx::SettingsAction;
        let mut m = build_model();
        m.dispatch_settings_action(SettingsAction::EditDefaultAgent {
            current: "claude".into(),
            tier: None,
        });
        assert_eq!(m.modal_stack.last(), Some(&Id::DefaultAgentPicker));
        m.dispatch_modal_key(key(Key::Esc));
        assert!(m.top_modal().is_none(), "Esc closes the picker");
    }

    /// The default-model picker (second step of the default-agent
    /// flow) offers an "agent default" row plus every declared tier,
    /// opens pre-positioned on the current default tier, and stashes
    /// the aliases + target agent for the pick. Esc releases both
    /// without changing anything. Disk-free: mounting only reads the
    /// in-memory tier menus.
    #[test]
    fn default_model_picker_offers_tiers_and_cancels_clean() {
        let mut m = build_model();
        let mut models = lazybox_core::AgentModels::builtin("claude").unwrap();
        models.default = Some("L".into());
        m.set_agent_models([("claude".to_string(), models)].into());

        m.mount_default_model_picker("claude");
        assert_eq!(m.modal_stack.last(), Some(&Id::DefaultModelPicker));
        assert_eq!(m.default_model_agent.as_deref(), Some("claude"));

        m.dispatch_modal_key(key(Key::Esc));
        assert!(m.top_modal().is_none(), "Esc closes the picker");
        assert!(m.default_model_agent.is_none(), "agent stash is released");
    }

    /// An agent with no declared tier menu has nothing to pick — the
    /// default-model step is skipped entirely (no modal mounts).
    #[test]
    fn default_model_picker_skips_agents_without_tiers() {
        let mut m = build_model();
        m.mount_default_model_picker("codex");
        assert!(m.top_modal().is_none(), "no tier menu → no second step");
    }

    /// End-to-end settings flow on a temp `LAZYBOX_HOME`: picking the
    /// default agent persists `setup.default_agent` and chains into the
    /// tier picker; picking a tier persists
    /// `agents.<id>.models.default` and mirrors it into the in-memory
    /// menu so the Settings badge and the next picker open reflect it
    /// without a restart.
    #[test]
    fn default_agent_pick_chains_into_tier_pick_and_persists_both() {
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("lazybox-default-tier-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator in
        // this binary, so this single-writer mutation can't race.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let mut m = build_model();
        m.set_agent_models(
            [(
                "claude".to_string(),
                lazybox_core::AgentModels::builtin("claude").unwrap(),
            )]
            .into(),
        );
        m.mount_default_agent_picker();
        let _ = m.handle_choice_picked(vec![ChoicePayload::Text("claude".into())]);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::DefaultModelPicker),
            "agent pick chains into the tier picker",
        );

        let _ = m.handle_choice_picked(vec![ChoicePayload::OptText(Some("L".into()))]);
        assert!(m.top_modal().is_none(), "tier pick ends the flow");

        let cfg = lazybox_config::Config::load_from(&home.join("config.yaml")).expect("config");
        assert_eq!(cfg.setup.default_agent.as_deref(), Some("claude"));
        assert_eq!(cfg.agent_models("claude").default.as_deref(), Some("L"));
        assert_eq!(
            m.agent_models["claude"].default.as_deref(),
            Some("L"),
            "mirrored into the in-memory menu",
        );

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Unpinning ("Agent default") for an agent with no YAML block is
    /// already a no-op — no dead `agents.<id>` stanza is serialized.
    #[test]
    fn unpinning_an_unconfigured_agent_writes_no_agents_stanza() {
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("lazybox-unpin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator in
        // this binary, so this single-writer mutation can't race.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let mut m = build_model();
        m.set_agent_models(
            [(
                "claude".to_string(),
                lazybox_core::AgentModels::builtin("claude").unwrap(),
            )]
            .into(),
        );
        m.mount_default_model_picker("claude");
        // Row 0 unpins → OptText(None), the "agent default" payload.
        let _ = m.handle_choice_picked(vec![ChoicePayload::OptText(None)]);

        let cfg = lazybox_config::Config::load_from(&home.join("config.yaml")).expect("config");
        assert!(
            !cfg.agents.contains_key("claude"),
            "no dead stanza for an unpin that changed nothing",
        );
        assert_eq!(
            m.agent_models["claude"].default.as_deref(),
            Some("L"),
            "unpinning lands on the built-in default tier, never the ambient model",
        );

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A tier pinning a Fable-class model is never offered as a
    /// default — the picker filters rows on `excluded_from_default()`,
    /// so the Fable tier drops while the standard tiers stay (the tier
    /// itself remains spawnable through an explicit chord). The picker
    /// still mounts; the filtered set is verified via the same
    /// predicate the mount loop applies.
    #[test]
    fn default_model_picker_excludes_fable_tiers() {
        let mut m = build_model();
        let mut models = lazybox_core::AgentModels::builtin("claude").unwrap();
        let fable = lazybox_core::ModelTier {
            alias: "F".into(),
            label: "Fable".into(),
            short: None,
            args: vec!["--model".into(), "claude-fable-5".into()],
        };
        assert!(
            fable.excluded_from_default(),
            "a Fable-class tier is not default-eligible",
        );
        for tier in &models.tiers {
            assert!(
                !tier.excluded_from_default(),
                "the standard {} tier stays default-eligible",
                tier.alias,
            );
        }
        models.tiers.push(fable);
        m.set_agent_models([("claude".to_string(), models)].into());

        m.mount_default_model_picker("claude");
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::DefaultModelPicker),
            "the picker still mounts with the Fable tier filtered out",
        );
        m.dispatch_modal_key(key(Key::Esc));
    }

    /// The per-agent "Default model" settings row opens the tier
    /// picker for that agent directly — no default-agent step first.
    #[test]
    fn edit_default_model_action_mounts_the_picker_for_that_agent() {
        use crate::realm::setup_ctx::SettingsAction;
        let mut m = build_model();
        m.set_agent_models(
            [(
                "claude".to_string(),
                lazybox_core::AgentModels::builtin("claude").unwrap(),
            )]
            .into(),
        );
        m.dispatch_settings_action(SettingsAction::EditDefaultModel {
            agent_id: "claude".into(),
            tier: None,
        });
        assert_eq!(m.modal_stack.last(), Some(&Id::DefaultModelPicker));
        assert_eq!(m.default_model_agent.as_deref(), Some("claude"));
        m.dispatch_modal_key(key(Key::Esc));
    }

    /// The Settings palette lists one "Default model" row per enabled
    /// agent with a tier menu, badged with the current default tier —
    /// Opus out of the box for Claude.
    #[test]
    fn settings_lists_a_default_model_row_per_tiered_agent() {
        use crate::realm::setup_ctx::SettingsAction;
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("lazybox-model-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator in
        // this binary, so this single-writer mutation can't race.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let mut m = build_model();
        let mut persisted = lazybox_core::PersistedSetup::default();
        persisted.enabled_providers.insert("github".into());
        m.cache_persisted_setup(persisted);
        m.open_settings();
        let row = m
            .setup
            .settings_actions
            .iter()
            .find_map(|a| match a {
                SettingsAction::EditDefaultModel { agent_id, tier } if agent_id == "claude" => {
                    Some(tier.clone())
                }
                _ => None,
            })
            .expect("claude gets a direct default-model row");
        assert_eq!(
            row.as_deref(),
            Some("Opus"),
            "the badge names the pinned built-in default",
        );

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Switching the default agent re-keys the `w S` / `a S` tier
    /// chords to the new agent's menu live — the catalog must not keep
    /// serving the previous agent's tier labels until a restart.
    #[test]
    fn switching_default_agent_rekeys_tier_chords_live() {
        use lazybox_tui_core::action::Param;
        let mut m = build_model();
        m.set_agent_models(
            [
                (
                    "claude".to_string(),
                    lazybox_core::AgentModels::builtin("claude").unwrap(),
                ),
                (
                    "codex".to_string(),
                    lazybox_core::AgentModels {
                        tiers: vec![lazybox_core::ModelTier {
                            alias: "M".into(),
                            label: "GPT-5".into(),
                            short: None,
                            args: vec![],
                        }],
                        ..Default::default()
                    },
                ),
            ]
            .into(),
        );
        let tier_labels = |m: &Model<tuirealm::terminal::TestTerminalAdapter>| -> Vec<String> {
            m.catalog()
                .iter()
                .filter(|e| matches!(&e.param, Some(Param::Tier(_))))
                .map(|e| e.label.to_string())
                .collect()
        };
        assert!(
            tier_labels(&m).contains(&"Opus".to_string()),
            "claude (the startup default) drives the chords",
        );

        m.set_default_agent("codex");
        let labels = tier_labels(&m);
        assert!(
            labels.contains(&"GPT-5".to_string()),
            "codex's menu drives the chords after the switch",
        );
        assert!(
            !labels.contains(&"Opus".to_string()),
            "claude's rows are gone",
        );
    }

    /// `set_default_agent` updates the agent both panes resolve `w`
    /// against, live — the persist half is covered by the config
    /// round-trip test. Disk-free.
    #[test]
    fn set_default_agent_updates_sidebar_live() {
        let mut m = build_model();
        assert_eq!(m.sidebar.default_agent(), "claude");
        m.set_default_agent("codex");
        assert_eq!(m.sidebar.default_agent(), "codex");
    }

    /// `]` opens the read-only snippets browser from the sidebar, and Esc
    /// pops it. Since #871 made `]]` a sidebar leader too, the first `]` is
    /// held pending a possible second — a lone press resolves to the
    /// browser once a non-`]` key follows (mirroring the terminal's held
    /// literal `]`). The browser is a global, so it fires with no
    /// workspace selected — the discovery entry point issue #237 asks for.
    #[test]
    fn bracket_opens_and_closes_snippet_browser() {
        let mut m = build_model();
        m.apply_snippets(lazybox_config::Snippets::builtin());

        assert!(m.top_modal().is_none(), "no modal before ]");
        m.dispatch_key(KeyEvent::new(Key::Char(']'), KeyModifiers::NONE));
        assert!(m.top_modal().is_none(), "a lone `]` is held pending `]]`");
        m.dispatch_key(KeyEvent::new(Key::Char('j'), KeyModifiers::NONE));
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
    use super::super::modals::PolicyToggle;
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
            author: String::new(),
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
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
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
    fn claimed_workspace_requires_confirmation_before_a_new_agent_spawn() {
        let (client, mut server) = channel::pair();
        let mut model = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let mut claimed = workspace("owner/repo#1164", false, Duration::hours(1));
        claimed
            .gh_issues
            .first_mut()
            .unwrap()
            .labels
            .push(lazybox_core::Label::new("Working"));
        let session_key = SessionKey::from(&claimed.key);
        model.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(claimed)));
        while server.rx.try_recv().is_ok() {}

        model.flush_dispatched_cmds(vec![lazybox_ipc::Command::Spawn {
            session_key: session_key.clone(),
            session_id: None,
            client_request_id: Some("claim-test".into()),
            kind: lazybox_ipc::TerminalKind::Agent("codex".into()),
            cwd: None,
            initial_prompt: Some("fix it".into()),
            on_main: false,
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
        }]);

        assert_eq!(model.top_modal(), Some(&Id::ClaimedSpawnConfirm));
        assert!(
            server.rx.try_recv().is_err(),
            "the daemon must not receive the spawn before explicit confirmation"
        );
        assert!(model.handle_confirmed(false).is_empty());

        // Re-open and accept: the exact snapshotted command is released.
        model.flush_dispatched_cmds(vec![lazybox_ipc::Command::Spawn {
            session_key,
            session_id: None,
            client_request_id: Some("claim-test-2".into()),
            kind: lazybox_ipc::TerminalKind::Agent("codex".into()),
            cwd: None,
            initial_prompt: Some("fix it".into()),
            on_main: false,
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
        }]);
        assert!(matches!(
            model.handle_confirmed(true).as_slice(),
            [lazybox_ipc::Command::Spawn {
                client_request_id: Some(request_id),
                ..
            }] if request_id == "claim-test-2"
        ));
    }

    #[test]
    fn claimed_workspace_allows_read_only_agent_without_confirmation() {
        let (client, mut server) = channel::pair();
        let mut model = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let mut claimed = workspace("owner/repo#1164", false, Duration::hours(1));
        claimed
            .gh_issues
            .first_mut()
            .unwrap()
            .labels
            .push(lazybox_core::Label::new("working"));
        let session_key = SessionKey::from(&claimed.key);
        model.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(claimed)));
        while server.rx.try_recv().is_ok() {}

        model.flush_dispatched_cmds(vec![lazybox_ipc::Command::Spawn {
            session_key,
            session_id: None,
            client_request_id: Some("read-only-claim-test".into()),
            kind: lazybox_ipc::TerminalKind::Agent("codex".into()),
            cwd: None,
            initial_prompt: None,
            on_main: false,
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::ReadOnly,
        }]);

        assert_ne!(model.top_modal(), Some(&Id::ClaimedSpawnConfirm));
        assert!(matches!(
            server.rx.try_recv(),
            Ok(lazybox_ipc::Command::Spawn {
                access: lazybox_ipc::AgentRunAccess::ReadOnly,
                client_request_id: Some(request_id),
                ..
            }) if request_id == "read-only-claim-test"
        ));
    }

    /// `g u` on a multi-select fans out one `UpdateBranch` per selected
    /// PR that's actually behind its base; up-to-date PRs are skipped,
    /// and the selection clears afterward. (The retired `Shift-U` key's
    /// bulk behavior now lives entirely on `g u` — #932.)
    #[test]
    fn bulk_update_branch_fans_out_over_behind_prs_only() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();

        let mut behind_a = workspace("owner/repo#1", true, Duration::hours(1));
        behind_a.pr.as_mut().unwrap().is_behind_base = true;
        let up_to_date = workspace("owner/repo#2", true, Duration::hours(2));
        let mut behind_b = workspace("owner/repo#3", true, Duration::hours(3));
        behind_b.pr.as_mut().unwrap().is_behind_base = true;

        let key_a = behind_a.key.clone();
        let key_b = behind_b.key.clone();

        for ws in [behind_a, up_to_date.clone(), behind_b] {
            m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        }

        // Mark all three rows.
        for key in [&key_a, &up_to_date.key, &key_b] {
            assert!(m.sidebar.focus_workspace_key(&SessionKey::from(key)));
            m.sidebar.toggle_broadcast_select();
        }
        assert_eq!(m.sidebar.broadcast_selected_count(), 3);

        let cmds = m.dispatch_action(&Action::UpdateBranch);

        let targets: Vec<lazybox_core::WorkspaceKey> = cmds
            .into_iter()
            .map(|c| match c {
                IpcCommand::UpdateBranch { workspace_key } => workspace_key,
                other => panic!("expected UpdateBranch, got {other:?}"),
            })
            .collect();
        assert_eq!(targets.len(), 2, "only the two behind PRs fan out");
        assert!(targets.contains(&key_a));
        assert!(targets.contains(&key_b));
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            0,
            "selection clears after the bulk fire",
        );
    }

    /// `g u` with no behind-base PR selected fires nothing and
    /// leaves the selection intact for another action.
    #[test]
    fn bulk_update_branch_with_no_behind_pr_is_noop() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let up_to_date = workspace("owner/repo#2", true, Duration::hours(2));
        let key = up_to_date.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(up_to_date)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&key)));
        m.sidebar.toggle_broadcast_select();

        let cmds = m.dispatch_action(&Action::UpdateBranch);
        assert!(cmds.is_empty(), "no behind PR → no command");
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            1,
            "selection survives a no-op bulk update",
        );
    }

    /// End-to-end selection-first (#932): Shift-↑/↓ in the sidebar sweep
    /// a contiguous multi-select through the real key handler, and a
    /// normal action then fans out one op per selected row.
    #[test]
    fn shift_arrow_sweep_selects_range_and_action_fans_out() {
        use lazybox_tui_core::action::Action;
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let mut m = build_model();
        let mut a = workspace("owner/repo#1", true, Duration::hours(1));
        a.pr.as_mut().unwrap().is_behind_base = true;
        let mut b = workspace("owner/repo#2", true, Duration::hours(2));
        b.pr.as_mut().unwrap().is_behind_base = true;
        let mut c = workspace("owner/repo#3", true, Duration::hours(3));
        c.pr.as_mut().unwrap().is_behind_base = true;
        for ws in [a, b, c] {
            m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        }

        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();

        // Sweep up to the top, then back down: the additive extend marks
        // every workspace row the cursor passes, regardless of start row
        // or the repo/kind headers between rows.
        for _ in 0..8 {
            m.dispatch_key(KeyEvent::new(Key::Up, KeyModifiers::SHIFT));
        }
        for _ in 0..8 {
            m.dispatch_key(KeyEvent::new(Key::Down, KeyModifiers::SHIFT));
        }

        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            3,
            "the Shift-arrow sweep grabs all three contiguous rows",
        );

        // A normal, un-prefixed action now fans out across the whole
        // selection — no Shift-U detour (#932).
        let cmds = m.dispatch_action(&Action::UpdateBranch);
        assert_eq!(cmds.len(), 3, "one UpdateBranch per selected PR");
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

    /// Policies menu (`g p`, issue #363): opening it on a PR workspace
    /// with merge-on-green already armed, then picking the merge-on-green
    /// row, emits `SetAutoMergeOnGreen { enabled: false }` — a toggle
    /// read from the live workspace state. Side state clears on pick.
    #[test]
    fn policy_picker_toggles_merge_on_green_off() {
        let mut m = build_model();
        let mut ws = workspace("owner/repo#1", true, Duration::hours(1));
        ws.auto_merge_on_green = true;
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));

        m.mount_policy_picker(ws_key.clone());
        assert_eq!(m.modal_stack.last(), Some(&Id::PolicyPicker));
        // Row 0 is merge-on-green (see `build_policy_rows`).
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Policy(PolicyToggle::MergeOnGreen)]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::SetAutoMergeOnGreen {
                session_key,
                enabled,
            } => {
                assert_eq!(session_key.as_str(), ws_key.as_str());
                assert!(!enabled, "armed → toggling off");
            }
            other => panic!("expected SetAutoMergeOnGreen, got {other:?}"),
        }
        assert!(m.modal_flow.is_none());
    }

    /// Pressing `g g` (`Action::ToggleAutoMerge`) arms merge-on-green
    /// *optimistically*: the local workspace flag flips on the keypress so the
    /// `⚡` row glyph shows immediately, instead of only after the daemon
    /// persists the flag and rebroadcasts the workspace — a round-trip that's
    /// invisible under output-heavy load (#1090). The daemon command still
    /// goes out; its echo confirms (or, if the author gate declines, clears it).
    #[test]
    fn toggle_auto_merge_arms_optimistically() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let ws = workspace("owner/repo#1", true, Duration::hours(1));
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));
        assert!(
            !m.sidebar
                .selected_workspace()
                .expect("selected")
                .auto_merge_on_green,
            "precondition: not armed"
        );

        let cmds = m.dispatch_action(&Action::ToggleAutoMerge);

        assert!(
            cmds.iter()
                .any(|c| matches!(c, IpcCommand::SetAutoMergeOnGreen { enabled: true, .. })),
            "must still tell the daemon to arm"
        );
        assert!(
            m.sidebar
                .selected_workspace()
                .expect("selected")
                .auto_merge_on_green,
            "g g must arm optimistically so the ⚡ glyph shows on the keypress",
        );
    }

    /// Picking an auto-fix row toggles the per-session arm. On a default
    /// (unlabeled) PR that means Default → Disarm →
    /// `SetAutoFixPolicy { kind: CiFailure, arm: Disarm }`.
    #[test]
    fn policy_picker_toggles_auto_fix_ci() {
        let mut m = build_model();
        let ws = workspace("owner/repo#7", true, Duration::hours(1));
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));

        m.mount_policy_picker(ws_key.clone());
        // Rows: 0 merge-on-green, 1 native (info), 2 auto-fix CI.
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Policy(PolicyToggle::AutoFix(
            lazybox_core::AutoFixKind::CiFailure,
        ))]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::SetAutoFixPolicy {
                session_key,
                kind,
                arm,
            } => {
                assert_eq!(session_key.as_str(), ws_key.as_str());
                assert_eq!(*kind, lazybox_core::AutoFixKind::CiFailure);
                assert_eq!(*arm, lazybox_core::PolicyArm::Disarm);
            }
            other => panic!("expected SetAutoFixPolicy, got {other:?}"),
        }
    }

    #[test]
    fn direct_auto_fix_toggle_arms_both_failure_kinds() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        m.auto_fix_enabled = true;
        let ws = workspace("owner/repo#7", true, Duration::hours(1));
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));

        let cmds = m.dispatch_action(&Action::ToggleAutoFix);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::SetAutoFixPolicies {
                session_key,
                ci: lazybox_core::PolicyArm::Arm,
                conflict: lazybox_core::PolicyArm::Arm,
            }] if session_key.as_str() == ws_key.as_str()
        ));
        let notice = m
            .status
            .notice
            .as_ref()
            .map(|notice| notice.message.as_str())
            .unwrap_or_default();
        assert!(
            notice.contains("CI failures + conflicts"),
            "toggle notice must explain both repair signals: {notice:?}"
        );
    }

    #[test]
    fn direct_auto_fix_toggle_disarms_both_when_either_is_armed() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let mut ws = workspace("owner/repo#8", true, Duration::hours(1));
        ws.policies.set(
            lazybox_core::AutoFixKind::CiFailure,
            lazybox_core::PolicyArm::Arm,
        );
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));

        let cmds = m.dispatch_action(&Action::ToggleAutoFix);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::SetAutoFixPolicies {
                ci: lazybox_core::PolicyArm::Disarm,
                conflict: lazybox_core::PolicyArm::Disarm,
                ..
            }]
        ));
    }

    #[test]
    fn shift_a_dispatches_the_direct_auto_fix_toggle() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let mut m = build_model();
        m.auto_fix_enabled = true;
        let ws = workspace("owner/repo#9", true, Duration::hours(1));
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));

        m.dispatch_key(KeyEvent::new(Key::Char('A'), KeyModifiers::SHIFT));
        let notice = m
            .status
            .notice
            .as_ref()
            .map(|notice| notice.message.as_str())
            .unwrap_or_default();
        assert!(
            notice.starts_with("auto-fix: armed"),
            "Shift-A must fire the direct catalog action: {notice:?}"
        );
    }

    /// The native-auto-merge row is read-only: picking it emits no
    /// command (it's a status row, not a toggle).
    #[test]
    fn policy_picker_native_auto_merge_row_is_read_only() {
        let mut m = build_model();
        let ws = workspace("owner/repo#8", true, Duration::hours(1));
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));

        m.mount_policy_picker(ws_key);
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Policy(PolicyToggle::Info(
            "GitHub auto-merge".into(),
        ))]);
        assert!(
            cmds.is_empty(),
            "native auto-merge status row emits no command"
        );
    }

    /// The daemon's `AutoFixPolicyConfig` event overrides the
    /// client-local auto-fix config so the policies menu reflects what
    /// the *daemon* would do — the two configs diverge under `--connect`
    /// (tracker #512). The glyph itself is proven off-when-disabled in
    /// `modals::tests`; here we prove the event feeds those menu inputs.
    #[test]
    fn auto_fix_policy_config_event_updates_menu_inputs() {
        let mut m = build_model();
        // Client starts at the off-by-default (opt-in) settings.
        assert!(!m.auto_fix_enabled);

        m.handle_daemon_event(IpcEvent::AutoFixPolicyConfig {
            enabled: true,
            opt_out_labels: vec!["skip-fix".into()],
        });
        assert!(m.auto_fix_enabled, "daemon's enable flag is applied");
        assert_eq!(
            m.auto_fix_opt_out_labels,
            vec!["skip-fix".to_string()],
            "daemon's opt-out label set replaces the client-local one",
        );
    }

    /// The description-reader modal (#448): opening it for the focused
    /// workspace's body mounts `Id::DescriptionModal`, and dismissing it
    /// pops cleanly (it carries no pending model state).
    #[test]
    fn open_focused_description_mounts_and_dismisses() {
        let mut m = build_model();
        let mut t = task("owner/repo#1", false, Duration::hours(1));
        t.body = Some(format!("# Heading\n\n{}", "word ".repeat(400)));
        let ws = Workspace::from_task(t, Utc::now());
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));
        m.sync_panes();

        m.open_focused_description();
        assert_eq!(m.modal_stack.last(), Some(&Id::DescriptionModal));

        m.update(Msg::ModalDismissed);
        assert!(
            m.modal_stack.is_empty(),
            "the reader modal pops without leaving pending state",
        );
    }

    /// A workspace with no body has nothing to read — opening is a no-op.
    #[test]
    fn open_focused_description_noop_without_body() {
        let mut m = build_model();
        let ws = workspace("owner/repo#2", false, Duration::hours(1));
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));
        m.sync_panes();

        m.open_focused_description();
        assert!(m.modal_stack.is_empty(), "no body → no modal");
    }

    /// The dispatch → mount seam: pressing `g p` (`Action::ManagePolicies`)
    /// on a focused PR workspace actually mounts the picker with rows
    /// stashed — the wiring the direct `handle_choice_picked` tests skip.
    #[test]
    fn manage_policies_action_mounts_picker() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let mut m = build_model();
        let ws = workspace("owner/repo#3", true, Duration::hours(1));
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));

        m.dispatch_key(KeyEvent::new(Key::Char('g'), KeyModifiers::NONE));
        m.dispatch_key(KeyEvent::new(Key::Char('p'), KeyModifiers::NONE));
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::PolicyPicker),
            "g p mounts the policies menu",
        );
        assert!(matches!(
            &m.modal_flow,
            Some(super::super::ModalFlow::PolicyWorkspace { workspace }) if *workspace == ws_key
        ));
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

        let backlog = drain_daemon_events(&mut m, &mut Vec::new(), || false);
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

    /// #271: the on-main session actions emit a `Spawn { on_main: true }`
    /// targeting the shared main checkout (no session sub-row), and they
    /// are confirm-guarded so `dispatch_action` mounts a confirm rather
    /// than firing directly.
    #[test]
    fn on_main_actions_spawn_with_on_main_flag() {
        use lazybox_ipc::{Command, TerminalKind};
        use lazybox_tui_core::action::{Action, ActionDef};

        let mut m = build_model();
        let ws = workspace("owner/repo#1", true, Duration::minutes(1));
        let sk: SessionKey = (&ws.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        // The unchecked dispatch (what the confirm's Yes runs) builds the
        // real command with `on_main: true`.
        let cmds = m.dispatch_action_unchecked(&Action::SpawnAgentOnMain("claude".into()));
        assert!(
            matches!(
                cmds.as_slice(),
                [Command::Spawn { kind: TerminalKind::Agent(id), on_main: true, session_id: None, .. }]
                    if id == "claude"
            ),
            "agent-on-main emits Spawn(Agent, on_main) with no session sub-row: {cmds:?}",
        );

        let cmds = m.dispatch_action_unchecked(&Action::SpawnShellOnMain);
        assert!(
            matches!(
                cmds.as_slice(),
                [Command::Spawn {
                    kind: TerminalKind::Shell,
                    on_main: true,
                    ..
                }]
            ),
            "shell-on-main emits Spawn(Shell, on_main): {cmds:?}",
        );

        // Both are confirm-guarded, so the guarded entry point mounts a
        // confirm and emits nothing directly.
        assert!(ActionDef::for_action(&Action::SpawnShellOnMain).is_destructive());
        let guarded = m.dispatch_action(&Action::SpawnShellOnMain);
        assert!(
            guarded.is_empty(),
            "the guarded path defers to the confirm modal: {guarded:?}",
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

        // Dispatch Work (`w w`) on the issue → arms the follow target + emits Spawn.
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
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: issue_sk,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
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

    /// Issue #308: the flat tier chords carry the picked model alias to
    /// the daemon. `w M` works on the contextual agent at tier M; `a S`
    /// spawns the default agent at tier S.
    #[test]
    fn tier_chords_thread_model_alias_into_spawn() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let issue = workspace("owner/repo#1", false, Duration::hours(1));
        let issue_sk: SessionKey = (&issue.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(issue)));
        assert!(m.sidebar.focus_workspace_key(&issue_sk));

        let work_alias = m
            .dispatch_action(&Action::WorkTier("M".into()))
            .into_iter()
            .find_map(|c| match c {
                lazybox_ipc::Command::Spawn { model_alias, .. } => Some(model_alias),
                _ => None,
            });
        assert_eq!(
            work_alias,
            Some(Some("M".to_string())),
            "`w M` spawns with tier alias M",
        );

        let spawn_alias = m
            .dispatch_action(&Action::SpawnTier("S".into()))
            .into_iter()
            .find_map(|c| match c {
                lazybox_ipc::Command::Spawn { model_alias, .. } => Some(model_alias),
                _ => None,
            });
        assert_eq!(
            spawn_alias,
            Some(Some("S".to_string())),
            "`a S` spawns with tier alias S",
        );
    }

    #[test]
    fn contextual_work_tier_reaches_the_daemon_inject_fallback() {
        use lazybox_ipc::{Client, Command, EVENT_CHANNEL_CAPACITY, TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;
        use tokio::sync::mpsc;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (_evt_tx, evt_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let client = Client::from_channels(cmd_tx, evt_rx);
        let mut m = Model::<tuirealm::terminal::TestTerminalAdapter>::new_for_test(
            client,
            Size::new(120, 40),
        )
        .expect("model init");

        let issue = workspace("owner/repo#1", false, Duration::hours(1));
        let issue_sk: SessionKey = (&issue.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(issue)));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(3),
            session_key: issue_sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        assert!(m.sidebar.focus_workspace_key(&issue_sk));
        while cmd_rx.try_recv().is_ok() {}

        let cmds = m.dispatch_action(&Action::WorkTier("M".into()));
        m.flush_dispatched_cmds(cmds);

        let alias =
            std::iter::from_fn(|| cmd_rx.try_recv().ok()).find_map(|command| match command {
                Command::InjectPrompt {
                    fallback_spawn: Some(fallback),
                    ..
                } => Some(fallback.model_alias),
                _ => None,
            });
        assert_eq!(
            alias,
            Some(Some("M".to_string())),
            "the daemon-visible fallback spawn keeps the explicit tier",
        );
    }

    #[test]
    fn read_only_spawn_is_not_rewritten_to_a_writable_terminal() {
        use lazybox_ipc::{AgentRunAccess, Command, TerminalId, TerminalKind};

        let mut model = build_model();
        let command = Command::Spawn {
            session_key: "test:critic".into(),
            session_id: None,
            client_request_id: Some("critic-1".into()),
            kind: TerminalKind::Agent("codex".into()),
            cwd: Some("/tmp/critic".into()),
            initial_prompt: Some("Review without editing".into()),
            on_main: false,
            model_alias: None,
            access: AgentRunAccess::ReadOnly,
        };

        assert!(matches!(
            model.rewrite_spawn_to_terminal(command, TerminalId(7)),
            Command::Spawn {
                access: AgentRunAccess::ReadOnly,
                client_request_id: Some(request_id),
                ..
            } if request_id == "critic-1"
        ));
    }

    /// Issue #557: `w` on an empty/scratch workspace (no PR, issue, or
    /// selected comments) used to be a silent no-op. It now spawns the
    /// default agent (bare) and arms the follow target, so a blank
    /// workspace is a usable starting point rather than a dead key.
    #[test]
    fn w_on_scratch_workspace_spawns_a_bare_agent() {
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
        let spawn = cmds.iter().find_map(|c| match c {
            lazybox_ipc::Command::Spawn { initial_prompt, .. } => Some(initial_prompt.clone()),
            _ => None,
        });
        assert_eq!(
            spawn,
            Some(None),
            "`w` on a scratch workspace spawns a bare agent (no fabricated prompt)",
        );
        assert!(
            m.spawn_follow_to.is_some(),
            "the spawn arms a follow target so focus lands on the new terminal",
        );
    }

    /// Issue #224: default work (`w w`) whose only running agent is
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
            model_label: None,
            terminal_id: TerminalId(3),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
        });
        assert!(m.sidebar.focus_workspace_key(&sk));

        let cmds = m.dispatch_action(&Action::Work);
        let terminal_id = cmds.iter().find_map(|command| match command {
            Command::InjectPrompt { terminal_id, .. } => Some(*terminal_id),
            _ => None,
        });
        assert_eq!(
            terminal_id,
            Some(TerminalId(3)),
            "`w w` targets the running Codex, not the default Claude",
        );
    }

    /// Issue #418: with SEVERAL distinct agents running, `w w` must ask
    /// which one to inject into (a chooser modal) instead of silently
    /// picking the default; the pick replays the work spawn against the
    /// chosen agent.
    #[test]
    fn bare_w_with_several_agents_mounts_chooser_and_pick_targets_it() {
        use lazybox_ipc::{Command, TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        for (tid, agent) in [(3, "codex"), (4, "claude")] {
            m.handle_daemon_event(IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(tid),
                session_key: sk.clone(),
                kind: TerminalKind::Agent(agent.into()),
                no_permission: false,
                on_main: false,
            });
        }
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));

        let cmds = m.dispatch_action(&Action::Work);
        assert!(
            cmds.is_empty(),
            "several running agents must not silently spawn/inject: {cmds:?}",
        );
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::WorkAgentPicker),
            "the multi-agent chooser is up",
        );
        let agents: Vec<String> = match &m.modal_flow {
            Some(super::super::ModalFlow::WorkPicker { picker }) => picker
                .targets
                .iter()
                .map(|target| target.agent_id.clone())
                .collect(),
            _ => panic!("picker stash armed"),
        };
        assert_eq!(
            agents,
            vec!["claude".to_string(), "codex".to_string()],
            "rows list every running agent, sorted",
        );

        // Pick Codex (row 1) → target that exact terminal.
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Index(1)]);
        match cmds.as_slice() {
            [Command::InjectPrompt { terminal_id, .. }] => {
                assert_eq!(*terminal_id, TerminalId(3))
            }
            other => panic!("expected one InjectPrompt for Codex, got {other:?}"),
        }
        assert!(m.modal_flow.is_none(), "stash consumed");
    }

    /// Issue #418: the chooser's pick rides the same spawn→inject
    /// rewrite as the keyboard path, so picking a running Codex injects
    /// into its terminal instead of spawning a second Codex.
    #[test]
    fn work_chooser_pick_injects_into_the_chosen_agent() {
        use lazybox_ipc::{Client, Command, EVENT_CHANNEL_CAPACITY, TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;
        use tokio::sync::mpsc;

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
        for (tid, agent) in [(3, "codex"), (4, "claude")] {
            m.handle_daemon_event(IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(tid),
                session_key: sk.clone(),
                kind: TerminalKind::Agent(agent.into()),
                no_permission: false,
                on_main: false,
            });
        }
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));
        assert!(m.dispatch_action(&Action::Work).is_empty());
        while cmd_rx.try_recv().is_ok() {} // drop setup traffic

        // Pick Codex (row 1), flushed the way Msg::ChoicePicked flushes.
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Index(1)]);
        m.flush_dispatched_cmds(cmds);
        let inject = std::iter::from_fn(|| cmd_rx.try_recv().ok()).find_map(|c| match c {
            Command::InjectPrompt { terminal_id, .. } => Some(terminal_id),
            _ => None,
        });
        assert_eq!(
            inject,
            Some(TerminalId(3)),
            "the pick injects into the running Codex terminal",
        );
    }

    #[test]
    fn work_chooser_disambiguates_two_sessions_of_the_same_agent() {
        use lazybox_ipc::{Client, Command, EVENT_CHANNEL_CAPACITY, TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;
        use tokio::sync::mpsc;

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
        for terminal_id in [TerminalId(3), TerminalId(4)] {
            m.handle_daemon_event(IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id,
                session_key: sk.clone(),
                kind: TerminalKind::Agent("codex".into()),
                no_permission: false,
                on_main: false,
            });
        }
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));

        assert!(m.dispatch_action(&Action::Work).is_empty());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::WorkAgentPicker),
            "two live conversations must be disambiguated before injection",
        );
        while cmd_rx.try_recv().is_ok() {}

        let cmds = m.handle_choice_picked(vec![ChoicePayload::Index(1)]);
        m.flush_dispatched_cmds(cmds);
        let target =
            std::iter::from_fn(|| cmd_rx.try_recv().ok()).find_map(|command| match command {
                Command::InjectPrompt { terminal_id, .. } => Some(terminal_id),
                _ => None,
            });
        assert_eq!(
            target,
            Some(TerminalId(4)),
            "the selected conversation receives the contextual prompt",
        );
    }

    /// Issue #418: Esc on the multi-agent chooser cancels cleanly —
    /// stash dropped, nothing spawned.
    #[test]
    fn work_chooser_dismiss_drops_the_stash() {
        use lazybox_ipc::{TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        for (tid, agent) in [(3, "codex"), (4, "cursor")] {
            m.handle_daemon_event(IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(tid),
                session_key: sk.clone(),
                kind: TerminalKind::Agent(agent.into()),
                no_permission: false,
                on_main: false,
            });
        }
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));
        assert!(m.dispatch_action(&Action::Work).is_empty());
        assert!(matches!(
            m.modal_flow,
            Some(super::super::ModalFlow::WorkPicker { .. })
        ));

        let cmds = m.handle_modal_dismissed();
        assert!(cmds.is_empty(), "Esc fires nothing: {cmds:?}");
        assert!(m.modal_flow.is_none(), "stash dropped on Esc");
    }

    /// Issue #418: a `w S` tier chord that lands on several running
    /// agents routes through the same chooser, and a stale-terminal
    /// fallback still carries the tier alias.
    #[test]
    fn work_tier_chooser_carries_model_alias_through_the_pick() {
        use lazybox_ipc::{Command, TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        for (tid, agent) in [(3, "codex"), (4, "claude")] {
            m.handle_daemon_event(IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(tid),
                session_key: sk.clone(),
                kind: TerminalKind::Agent(agent.into()),
                no_permission: false,
                on_main: false,
            });
        }
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));

        assert!(m.dispatch_action(&Action::WorkTier("M".into())).is_empty());
        assert_eq!(m.modal_stack.last(), Some(&Id::WorkAgentPicker));

        let cmds = m.handle_choice_picked(vec![ChoicePayload::Index(1)]);
        match cmds.as_slice() {
            [
                Command::InjectPrompt {
                    terminal_id,
                    fallback_spawn: Some(fallback),
                    ..
                },
            ] => {
                assert_eq!(*terminal_id, TerminalId(3));
                assert!(matches!(
                    &fallback.kind,
                    TerminalKind::Agent(id) if id == "codex"
                ));
                assert_eq!(
                    fallback.model_alias.as_deref(),
                    Some("M"),
                    "tier alias survives the pick"
                );
            }
            other => panic!("expected one tiered InjectPrompt, got {other:?}"),
        }
    }

    /// Issue #418: the default agent is the target only when NOTHING is
    /// running on the workspace.
    #[test]
    fn bare_w_with_no_running_agent_spawns_the_default() {
        use lazybox_ipc::{Command, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
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
            Some(m.sidebar.default_agent()),
            "no running agent → the configured default spawns",
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
            model_label: None,
            terminal_id: TerminalId(5),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
        });
        // `TerminalSpawned` auto-focuses the terminal pane; return to the
        // sidebar (cursor on the PR) so the catalog resolves `w`.
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));
        while cmd_rx.try_recv().is_ok() {} // drop setup traffic

        // `w` opens the deterministic work menu; `x` completes `w x`.
        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert_eq!(
            m.leader_pending(),
            lazybox_tui_core::action::KeyStroke::parse("w"),
            "`w` opens the work menu",
        );
        m.dispatch_key(KeyEvent::new(Key::Char('x'), KeyModifiers::NONE));
        assert!(m.leader_pending().is_none(), "`x` resolves the leader");

        let inject = std::iter::from_fn(|| cmd_rx.try_recv().ok())
            .find(|c| matches!(c, Command::InjectPrompt { .. }));
        assert!(
            inject.is_some(),
            "`w x` injects the work prompt into the running Codex",
        );
    }

    /// `w w` deterministically fires Work against the running-or-default
    /// agent. There is no idle timeout, so the command lands on the
    /// second keystroke without 600ms of artificial latency.
    #[test]
    fn w_w_fires_default_work_immediately() {
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
            model_label: None,
            terminal_id: TerminalId(8),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
        });
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        assert!(m.sidebar.focus_workspace_key(&sk));
        while cmd_rx.try_recv().is_ok() {} // drop setup traffic

        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert_eq!(
            m.leader_pending(),
            lazybox_tui_core::action::KeyStroke::parse("w"),
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "the menu key alone does not act"
        );
        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert!(m.leader_pending().is_none(), "second `w` resolves the menu");

        let inject = std::iter::from_fn(|| cmd_rx.try_recv().ok())
            .find(|c| matches!(c, Command::InjectPrompt { .. }));
        assert!(
            inject.is_some(),
            "`w w` injects work into the running Codex",
        );
    }

    /// An *empty* terminal pane resolves keys with sidebar scope, so the
    /// complete `w w` menu chord must work there too. A live terminal
    /// still owns its keys and uses the `]]` command menu.
    #[test]
    fn w_w_from_empty_terminal_pane_fires_work() {
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
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));
        // Terminal pane focused with NO terminals — the empty-state
        // hint's scope.
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        while cmd_rx.try_recv().is_ok() {} // drop setup traffic

        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert!(m.leader_pending().is_some(), "`w` opens the work menu");
        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::NONE));
        assert!(m.leader_pending().is_none(), "second `w` resolves it");

        let spawned = std::iter::from_fn(|| cmd_rx.try_recv().ok())
            .any(|c| matches!(c, Command::Spawn { .. } | Command::InjectPrompt { .. }));
        assert!(
            spawned,
            "`w w` from an empty terminal pane must still fire Work",
        );
    }

    /// Arrow / `j` / `k` move a highlight through the which-key leader
    /// popup and keep the leader armed; no action fires until `Enter`,
    /// and `Esc` clears both the leader and the highlight (#343).
    #[test]
    fn leader_popup_arrows_move_highlight_without_firing() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();

        // `g` arms the github group (6 continuations); nothing highlighted.
        m.dispatch_key(KeyEvent::new(Key::Char('g'), KeyModifiers::NONE));
        assert!(m.leader_pending().is_some(), "`g` arms the github leader");
        assert_eq!(m.leader_highlight(), None, "no highlight until navigation");

        // Down/Up move the highlight; the leader stays armed and nothing
        // fires.
        m.dispatch_key(KeyEvent::new(Key::Down, KeyModifiers::NONE));
        assert_eq!(m.leader_highlight(), Some(0));
        m.dispatch_key(KeyEvent::new(Key::Down, KeyModifiers::NONE));
        assert_eq!(m.leader_highlight(), Some(1));
        m.dispatch_key(KeyEvent::new(Key::Up, KeyModifiers::NONE));
        assert_eq!(m.leader_highlight(), Some(0));
        assert!(
            m.leader_pending().is_some(),
            "navigation keeps the leader armed"
        );
        assert!(m.top_modal().is_none(), "navigation fires no action");

        // `j`/`k` navigate too — the github group binds neither.
        m.dispatch_key(KeyEvent::new(Key::Char('j'), KeyModifiers::NONE));
        assert_eq!(m.leader_highlight(), Some(1));
        m.dispatch_key(KeyEvent::new(Key::Char('k'), KeyModifiers::NONE));
        assert_eq!(m.leader_highlight(), Some(0));

        // Esc cancels: leader disarmed, highlight cleared.
        m.dispatch_key(KeyEvent::new(Key::Esc, KeyModifiers::NONE));
        assert!(m.leader_pending().is_none(), "Esc cancels the leader");
        assert_eq!(m.leader_highlight(), None);
    }

    /// `Enter` fires the highlighted continuation, then disarms the
    /// leader and clears the highlight (#343).
    #[test]
    fn leader_popup_enter_fires_highlighted_entry() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();
        while server.rx.try_recv().is_ok() {}

        m.dispatch_key(KeyEvent::new(Key::Char('g'), KeyModifiers::NONE));
        m.dispatch_key(KeyEvent::new(Key::Down, KeyModifiers::NONE));
        assert_eq!(m.leader_highlight(), Some(0));

        m.dispatch_key(KeyEvent::new(Key::Enter, KeyModifiers::NONE));
        assert!(m.leader_pending().is_none(), "Enter resolves the leader");
        assert_eq!(m.leader_highlight(), None);
        // Every github continuation either mounts a modal (merge confirm,
        // label editor) or emits a command — so *some* effect proves the
        // highlighted action dispatched rather than silently cancelling.
        let fired = m.top_modal().is_some() || server.rx.try_recv().is_ok();
        assert!(fired, "Enter dispatches the highlighted github action");
    }

    /// An active highlight never shadows the direct-letter path: typing a
    /// continuation's own key still fires it (arrows are additive, #343).
    #[test]
    fn leader_popup_direct_letter_still_fires_with_highlight_active() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();

        m.dispatch_key(KeyEvent::new(Key::Char('g'), KeyModifiers::NONE));
        m.dispatch_key(KeyEvent::new(Key::Down, KeyModifiers::NONE));
        assert_eq!(m.leader_highlight(), Some(0), "a row is highlighted");

        // `g m` = merge, a destructive action → confirm modal, regardless
        // of which row the highlight sat on.
        m.dispatch_key(KeyEvent::new(Key::Char('m'), KeyModifiers::NONE));
        assert!(
            m.leader_pending().is_none(),
            "the direct letter resolves the leader"
        );
        assert_eq!(m.leader_highlight(), None);
        assert!(
            m.top_modal().is_some(),
            "`g m` fires merge despite the active highlight"
        );
    }

    /// Issue #304: `q` in an empty terminal pane must not arm a leader.
    /// Quit's `q q` is a catalog `Seq`, but it dispatches through the
    /// q-latch, not `dispatch_action` — arming on it would pop a
    /// which-key menu whose completion goes nowhere.
    #[test]
    fn q_in_empty_terminal_pane_does_not_arm_a_dead_quit_leader() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers};

        let mut m = build_model();
        let pr = workspace("owner/repo#1", true, Duration::hours(1));
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        assert!(m.terminals.is_empty(), "fixture: no terminals spawned");

        m.dispatch_key(KeyEvent::new(Key::Char('q'), KeyModifiers::NONE));
        assert!(
            m.leader_pending().is_none(),
            "`q` must not arm a leader — its completion can't dispatch Quit",
        );
        assert!(!m.quit, "a lone `q` never quits");
    }

    /// A mouse click cancels an armed catalog leader so the next key is
    /// interpreted in the newly-clicked context, not as an old menu choice.
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
        assert!(m.leader_pending().is_some(), "`w` arms the leader");

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
            m.leader_pending().is_none(),
            "a mouse click must cancel the armed work leader",
        );
    }

    // ── #899: multi-select honored across workspace actions ─────────

    /// Seed `keys` as workspaces and mark every one in the `v`
    /// multi-select set, leaving the cursor on the last.
    fn seed_and_select(
        m: &mut Model<tuirealm::terminal::TestTerminalAdapter>,
        rows: Vec<Workspace>,
    ) -> Vec<SessionKey> {
        let keys: Vec<SessionKey> = rows.iter().map(|w| SessionKey::from(&w.key)).collect();
        for ws in rows {
            m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        }
        for key in &keys {
            assert!(m.sidebar.focus_workspace_key(key));
            m.sidebar.toggle_broadcast_select();
        }
        keys
    }

    /// A non-empty selection resolves to every marked row; an empty
    /// selection falls back to the focused row (acceptance #1).
    #[test]
    fn resolve_targets_is_selection_or_focused() {
        let mut m = build_model();
        let a = workspace("owner/repo#1", true, Duration::hours(1));
        let b = workspace("owner/repo#2", true, Duration::hours(2));
        let key_a = SessionKey::from(&a.key);
        let key_b = SessionKey::from(&b.key);

        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(a)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(b)));

        // Empty selection, cursor on b → just b.
        assert!(m.sidebar.focus_workspace_key(&key_b));
        assert_eq!(m.resolve_targets(), vec![key_b.clone()]);

        // Mark both → the whole set regardless of cursor.
        for key in [&key_a, &key_b] {
            assert!(m.sidebar.focus_workspace_key(key));
            m.sidebar.toggle_broadcast_select();
        }
        let targets = m.resolve_targets();
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&key_a) && targets.contains(&key_b));
    }

    /// `g s` sync fans out one `SyncWorkspace` per selected row and
    /// clears the selection (acceptance #6).
    #[test]
    fn bulk_sync_fans_out_over_selection() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let keys = seed_and_select(
            &mut m,
            vec![
                workspace("owner/repo#1", true, Duration::hours(1)),
                workspace("owner/repo#2", true, Duration::hours(2)),
            ],
        );

        let cmds = m.dispatch_action(&Action::SyncWorkspace);
        assert_eq!(cmds.len(), 2, "one SyncWorkspace per selected PR");
        assert!(
            cmds.iter()
                .all(|c| matches!(c, IpcCommand::SyncWorkspace { .. }))
        );
        assert_eq!(m.sidebar.broadcast_selected_count(), 0, "selection clears");
        let _ = keys;
    }

    /// `m` mark-read fans out one `MarkRead` per selected workspace.
    #[test]
    fn bulk_mark_read_fans_out() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        seed_and_select(
            &mut m,
            vec![
                workspace("owner/repo#1", true, Duration::hours(1)),
                workspace("owner/repo#2", true, Duration::hours(2)),
            ],
        );
        let cmds = m.dispatch_action(&Action::MarkAllRead);
        assert_eq!(cmds.len(), 2);
        assert!(
            cmds.iter()
                .all(|c| matches!(c, IpcCommand::MarkRead { .. }))
        );
    }

    /// `z` snooze toggles each selected row against its own state. From
    /// the Inbox the selectable rows are all awake, so every one snoozes;
    /// the toggle still keys off each row's own state (a snoozed row,
    /// reachable from the Snoozed mailbox, would wake instead).
    #[test]
    fn bulk_snooze_toggles_each_against_its_state() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        seed_and_select(
            &mut m,
            vec![
                workspace("owner/repo#1", true, Duration::hours(1)),
                workspace("owner/repo#2", true, Duration::hours(2)),
            ],
        );

        let cmds = m.dispatch_action(&Action::ToggleSnooze);
        assert_eq!(cmds.len(), 2);
        assert!(
            cmds.iter().all(|c| matches!(c, IpcCommand::Snooze { .. })),
            "awake rows snooze",
        );
    }

    /// `g g` arms auto-merge on every selected PR; a non-PR row (issue)
    /// is skipped and counted (acceptance #6).
    #[test]
    fn bulk_auto_merge_arms_prs_and_skips_issues() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        seed_and_select(
            &mut m,
            vec![
                workspace("owner/repo#1", true, Duration::hours(1)),
                workspace("owner/repo#2", false, Duration::hours(2)),
                workspace("owner/repo#3", true, Duration::hours(3)),
            ],
        );
        let cmds = m.dispatch_action(&Action::ToggleAutoMerge);
        assert_eq!(cmds.len(), 2, "only the two PRs arm");
        assert!(
            cmds.iter()
                .all(|c| matches!(c, IpcCommand::SetAutoMergeOnGreen { enabled: true, .. }))
        );
    }

    /// `g u` update-branch over a selection fans out only the
    /// behind-base PRs — the sole bulk path now that `Shift-U` is
    /// retired (#932).
    #[test]
    fn bulk_update_branch_via_g_u_fans_out_behind_prs() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let mut behind = workspace("owner/repo#1", true, Duration::hours(1));
        behind.pr.as_mut().unwrap().is_behind_base = true;
        let up_to_date = workspace("owner/repo#2", true, Duration::hours(2));
        seed_and_select(&mut m, vec![behind, up_to_date]);

        let cmds = m.dispatch_action(&Action::UpdateBranch);
        assert_eq!(cmds.len(), 1, "only the behind PR updates");
        assert!(matches!(cmds[0], IpcCommand::UpdateBranch { .. }));
    }

    /// An empty selection leaves single-row behavior untouched: `g s`
    /// syncs just the focused workspace (acceptance: empty unchanged).
    #[test]
    fn empty_selection_syncs_only_focused_row() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let a = workspace("owner/repo#1", true, Duration::hours(1));
        let b = workspace("owner/repo#2", true, Duration::hours(2));
        let key_b = SessionKey::from(&b.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(a)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(b)));
        assert!(m.sidebar.focus_workspace_key(&key_b));

        let cmds = m.dispatch_action(&Action::SyncWorkspace);
        assert_eq!(cmds.len(), 1, "no selection → focused row only");
    }

    /// `g m` merge over a selection stashes the whole set in the confirm
    /// flow; iterating it fires one `MergePr` per merge-ready PR and
    /// skips the ineligible (acceptance #2, #5, #6).
    #[test]
    fn bulk_merge_confirms_the_set_and_skips_ineligible() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let ready = workspace("owner/repo#1", true, Duration::hours(1));
        let mut blocked = workspace("owner/repo#2", true, Duration::hours(2));
        blocked.pr.as_mut().unwrap().review = lazybox_core::ReviewStatus::ChangesRequested;
        let ready_key = ready.key.clone();
        seed_and_select(&mut m, vec![ready, blocked]);

        let pending = m.dispatch_action(&Action::MergePr);
        assert!(pending.is_empty(), "merge gates on confirm first");
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));
        match &m.modal_flow {
            Some(ModalFlow::ActionConfirm {
                action: Action::MergePr,
                targets,
            }) => assert_eq!(targets.len(), 2, "the whole selection is stashed"),
            other => panic!("expected a bulk merge confirm, got {other:?}"),
        }

        let cmds = m.handle_confirmed(true);
        assert_eq!(cmds.len(), 1, "only the merge-ready PR fires");
        match &cmds[0] {
            IpcCommand::MergePr { workspace_key } => assert_eq!(workspace_key, &ready_key),
            other => panic!("expected MergePr for the ready PR, got {other:?}"),
        }
        assert_eq!(m.sidebar.broadcast_selected_count(), 0);
    }

    /// `x x` archive over a selection confirms with the count, then kills
    /// each snapshot target — even one removed under the open modal is
    /// simply skipped, never redirecting onto a drifted cursor
    /// (acceptance #2, #5).
    #[test]
    fn bulk_archive_iterates_snapshot() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let first = workspace("owner/repo#1", true, Duration::hours(1));
        let gone = first.key.clone();
        seed_and_select(
            &mut m,
            vec![first, workspace("owner/repo#2", true, Duration::hours(2))],
        );

        assert!(m.dispatch_action(&Action::Archive).is_empty());
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        // A daemon event removes one target while the confirm is up.
        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(gone));

        let cmds = m.handle_confirmed(true);
        assert_eq!(
            cmds.len(),
            1,
            "the surviving target is killed; the gone one skipped"
        );
        assert!(matches!(cmds[0], IpcCommand::Kill { .. }));
        assert_eq!(m.sidebar.broadcast_selected_count(), 0);
    }

    /// A right-click "Merge" acts on the clicked row even with a `v`
    /// multi-select active: the ambient selection is cleared so a
    /// bulk-destructive context action can't fan out over the whole set
    /// instead of the explicit row (review follow-up).
    #[test]
    fn context_menu_bulk_action_targets_clicked_row_not_selection() {
        use super::super::ChoicePayload;
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let a = workspace("owner/repo#1", true, Duration::hours(1));
        let b = workspace("owner/repo#2", true, Duration::hours(2));
        let wk_a = a.key.clone();
        let key_a = SessionKey::from(&a.key);
        seed_and_select(&mut m, vec![a, b]);
        assert_eq!(m.sidebar.broadcast_selected_count(), 2);

        // Right-click "Merge" on row A only.
        m.modal_flow = Some(ModalFlow::SidebarContext {
            session_key: key_a.clone(),
            actions: vec![Action::MergePr],
        });
        m.modal_stack.push(Id::SidebarContext);
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Index(0)]);
        assert!(cmds.is_empty(), "merge gates on confirm");

        // Single-target confirm for the clicked row — not a bulk set.
        match &m.modal_flow {
            Some(ModalFlow::ActionConfirm {
                action: Action::MergePr,
                targets,
            }) => {
                assert_eq!(targets.len(), 1, "only the clicked row is targeted");
                match &targets[0] {
                    ActionConfirmTarget::Workspace(k) => assert_eq!(k, &key_a),
                    other => panic!("expected the clicked workspace, got {other:?}"),
                }
            }
            other => panic!("expected a single-row merge confirm, got {other:?}"),
        }
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            0,
            "the explicit right-click cleared the ambient multi-select",
        );

        let cmds = m.handle_confirmed(true);
        assert!(
            matches!(cmds.as_slice(), [IpcCommand::MergePr { workspace_key }] if workspace_key == &wk_a),
            "only the clicked row merges: {cmds:?}",
        );
    }

    /// When every stashed target regresses before Yes, nothing acts and
    /// the marks survive so the user can retry — the no-op-survives rule
    /// `bulk_dispatch` already follows, now honored on the confirmed
    /// destructive path too (review follow-up).
    #[test]
    fn bulk_merge_keeps_selection_when_all_targets_regress() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let a = workspace("owner/repo#1", true, Duration::hours(1));
        let b = workspace("owner/repo#2", true, Duration::hours(2));
        let mut a_red = a.clone();
        a_red.pr.as_mut().unwrap().ci = lazybox_core::CiStatus::Failure;
        let mut b_red = b.clone();
        b_red.pr.as_mut().unwrap().ci = lazybox_core::CiStatus::Failure;
        seed_and_select(&mut m, vec![a, b]);

        assert!(m.dispatch_action(&Action::MergePr).is_empty());

        // Both stashed PRs regress under the modal.
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(a_red)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(b_red)));

        let cmds = m.handle_confirmed(true);
        assert!(
            cmds.is_empty(),
            "nothing merges when every stashed PR regressed"
        );
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            2,
            "the selection survives a no-op bulk merge",
        );
    }

    /// A bulk `a c` spawn gates behind a "start N agents?" confirm (#836);
    /// confirming emits the snapshotted spawns (acceptance #4).
    #[test]
    fn bulk_spawn_agent_gates_behind_confirm() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        seed_and_select(
            &mut m,
            vec![
                workspace("owner/repo#1", true, Duration::hours(1)),
                workspace("owner/repo#2", true, Duration::hours(2)),
            ],
        );

        let pending = m.dispatch_action(&Action::SpawnAgent("claude".into()));
        assert!(pending.is_empty(), "spawning many agents gates on confirm");
        assert_eq!(m.modal_stack.last(), Some(&Id::BulkSpawnConfirm));

        let cmds = m.handle_confirmed(true);
        assert_eq!(cmds.len(), 2, "one spawn per selected workspace");
        assert!(cmds.iter().all(|c| matches!(
            c,
            IpcCommand::Spawn {
                kind: lazybox_ipc::TerminalKind::Agent(_),
                ..
            }
        )));
        assert_eq!(m.sidebar.broadcast_selected_count(), 0);
    }

    /// A bulk `w w` with no live agents plans a contextual spawn per row
    /// and gates behind the same confirm (acceptance #4).
    #[test]
    fn bulk_work_plans_contextual_spawns() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        seed_and_select(
            &mut m,
            vec![
                workspace("owner/repo#1", true, Duration::hours(1)),
                workspace("owner/repo#2", true, Duration::hours(2)),
            ],
        );

        assert!(m.dispatch_action(&Action::Work).is_empty());
        assert_eq!(m.modal_stack.last(), Some(&Id::BulkSpawnConfirm));

        let cmds = m.handle_confirmed(true);
        assert_eq!(cmds.len(), 2);
        assert!(cmds.iter().all(|c| matches!(
            c,
            IpcCommand::Spawn {
                kind: lazybox_ipc::TerminalKind::Agent(_),
                ..
            }
        )));
    }

    /// #928 acceptance #3: the model-tier chords (`w S/M/L`) fan out over
    /// a multi-select the same way bare `w w` does. Every *fresh* target
    /// spawns at the picked tier; a target already running an agent is
    /// injected into tier-less — a live session can't be retiered, so the
    /// bulk inject drops the alias (unlike the single-target path, which
    /// carries it in the fallback spawn). Both halves are asserted so the
    /// asymmetry is a documented contract, not an accident.
    #[test]
    fn bulk_work_tier_fans_out_with_alias() {
        use lazybox_ipc::{TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        // A row already running an agent → inject target.
        let live = workspace("owner/repo#1", true, Duration::hours(1));
        let live_sk: SessionKey = (&live.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(live)));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: live_sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        // Two rows with no agent → spawn targets (and `spawned > 0` gates
        // the fan-out behind the confirm).
        let fresh_a = workspace("owner/repo#2", true, Duration::hours(2));
        let fresh_b = workspace("owner/repo#3", true, Duration::hours(3));
        let fresh_a_sk: SessionKey = (&fresh_a.key).into();
        let fresh_b_sk: SessionKey = (&fresh_b.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(fresh_a)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(fresh_b)));

        for key in [&live_sk, &fresh_a_sk, &fresh_b_sk] {
            assert!(m.sidebar.focus_workspace_key(key));
            m.sidebar.toggle_broadcast_select();
        }

        assert!(m.dispatch_action(&Action::WorkTier("M".into())).is_empty());
        assert_eq!(m.modal_stack.last(), Some(&Id::BulkSpawnConfirm));

        let cmds = m.handle_confirmed(true);

        let spawns: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, IpcCommand::Spawn { .. }))
            .collect();
        assert_eq!(spawns.len(), 2, "one spawn per agent-less selected row");
        assert!(
            spawns.iter().all(|c| matches!(
                c,
                IpcCommand::Spawn {
                    kind: TerminalKind::Agent(_),
                    model_alias: Some(alias),
                    ..
                } if alias == "M"
            )),
            "every fresh-row spawn carries the picked tier: {spawns:?}",
        );

        let injects: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                IpcCommand::InjectPrompt {
                    terminal_id,
                    fallback_spawn,
                    ..
                } => Some((terminal_id, fallback_spawn)),
                _ => None,
            })
            .collect();
        assert_eq!(injects.len(), 1, "the one live agent is injected into");
        assert_eq!(*injects[0].0, TerminalId(7));
        assert!(
            injects[0].1.is_none(),
            "a live agent is injected tier-less — its model can't be changed: {cmds:?}",
        );

        assert_eq!(m.sidebar.broadcast_selected_count(), 0);
    }

    /// #1077 headline acceptance: for N selected workspaces, a snippet
    /// broadcast fires N snippet deliveries — one per row, never a mix —
    /// and `w w` fires N injects over the *same* set. Both flow through
    /// the one `apply_one` pipeline, so snippet and work fan out
    /// identically: no row falls into a different action.
    #[test]
    fn snippet_and_work_fan_out_identically_over_a_selection() {
        use lazybox_ipc::{TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let tmp_dir = std::env::temp_dir().join(format!("lazybox-fanout-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tmp = tmp_dir.join("snippets.yaml");
        std::fs::write(
            &tmp,
            "snippets:\n  rev:\n    description: Review\n    body: review the diff\n",
        )
        .unwrap();
        m.apply_snippets(
            lazybox_config::Snippets::load_from(&tmp, lazybox_config::SnippetOrigin::Global)
                .unwrap(),
        );
        let mut keys = Vec::new();
        for (i, tid) in [(1u64, 11u64), (2, 12), (3, 13)] {
            let ws = workspace(&format!("owner/repo#{i}"), true, Duration::hours(i as i64));
            let key: SessionKey = (&ws.key).into();
            m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
            m.handle_daemon_event(IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(tid),
                session_key: key.clone(),
                kind: TerminalKind::Agent("claude".into()),
                no_permission: false,
                on_main: false,
            });
            keys.push(key);
        }
        let select_all = |m: &mut Model<tuirealm::terminal::TestTerminalAdapter>| {
            for key in &keys {
                assert!(m.sidebar.focus_workspace_key(key));
                m.sidebar.toggle_broadcast_select();
            }
        };

        // --- snippet fan-out (the reported-bug path) ---
        select_all(&mut m);
        assert_eq!(m.sidebar.broadcast_selected_count(), 3);
        m.modal_flow = Some(super::super::ModalFlow::Broadcast {
            draft: BroadcastDraft {
                targets: keys.clone(),
                snippet_key: Some("rev".into()),
            },
        });
        m.modal_stack.push(Id::BroadcastText);
        let snippet_cmds = m.handle_textarea_submitted("review the diff".into());
        let mut snippet_targets: Vec<u64> = snippet_cmds
            .iter()
            .filter_map(|c| match c {
                IpcCommand::DeliverSnippet { terminal_id, .. } => Some(terminal_id.0),
                _ => None,
            })
            .collect();
        snippet_targets.sort_unstable();
        assert_eq!(
            snippet_targets,
            vec![11, 12, 13],
            "the snippet reaches every selected agent — no row falls into w-w or a spawn: {snippet_cmds:?}",
        );
        assert!(
            !snippet_cmds.iter().any(|c| matches!(
                c,
                IpcCommand::Spawn { .. } | IpcCommand::InjectPrompt { .. }
            )),
            "snippet delivery never spawns or injects a work prompt: {snippet_cmds:?}",
        );
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            0,
            "selection clears after delivery"
        );

        // --- `w w` fan-out over the same selection ---
        select_all(&mut m);
        let work_cmds = m.dispatch_action(&Action::Work);
        let mut work_targets: Vec<u64> = work_cmds
            .iter()
            .filter_map(|c| match c {
                IpcCommand::InjectPrompt { terminal_id, .. } => Some(terminal_id.0),
                _ => None,
            })
            .collect();
        work_targets.sort_unstable();
        assert_eq!(
            work_targets,
            vec![11, 12, 13],
            "w w injects into every selected agent — identical fan-out to the snippet: {work_cmds:?}",
        );
    }

    /// #1077 guard: `apply_one` — the single per-target executor every
    /// fan-out shares — must resolve its target from its `key` argument
    /// alone, never from the sidebar cursor / selection. A dispatchable
    /// path that reached for `selected_workspace()` here would reintroduce
    /// the divergence (one row acted on, the rest something else) the
    /// unified pipeline exists to prevent.
    #[test]
    fn apply_one_resolves_targets_only_from_its_argument() {
        let src = include_str!("dispatch.rs");
        let start = src
            .find("fn apply_one(")
            .expect("apply_one is the single per-target executor");
        // Bound the body by brace-matching from its opening `{`, so the
        // guard survives method reordering / new helpers (apply_one has no
        // brace-bearing string or char literals, so a raw scan is exact).
        let open = start + src[start..].find('{').expect("apply_one has a body");
        let mut depth = 0usize;
        let mut end = open;
        for (i, ch) in src[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &src[open..=end];
        assert!(
            depth == 0 && end > open,
            "apply_one body braces did not balance — guard bound is wrong",
        );
        assert!(
            !body.contains("selected_workspace"),
            "apply_one must resolve its target from `key`, not selected_workspace()",
        );
    }

    /// #899 regression: an inherently single-target destructive action
    /// (`x c` close-issue) must stay focused-only even under a `v`
    /// selection — before the fix it swept the whole set behind the
    /// generic "Archive N workspaces?" prompt and closed every marked
    /// issue.
    #[test]
    fn close_issue_under_selection_stays_focused_only() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let a = workspace("owner/repo#1", false, Duration::hours(1));
        let focus_key = SessionKey::from(&a.key);
        seed_and_select(
            &mut m,
            vec![a, workspace("owner/repo#2", false, Duration::hours(2))],
        );
        assert_eq!(m.sidebar.broadcast_selected_count(), 2);
        assert!(m.sidebar.focus_workspace_key(&focus_key));

        assert!(m.dispatch_action(&Action::CloseIssue).is_empty());
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));
        match &m.modal_flow {
            Some(ModalFlow::ActionConfirm {
                action: Action::CloseIssue,
                targets,
            }) => assert_eq!(
                targets.as_slice(),
                [ActionConfirmTarget::Workspace(focus_key.clone())],
                "close-issue targets the focused row only, never the selection",
            ),
            other => panic!("expected a single-target close-issue confirm, got {other:?}"),
        }
    }

    /// #899 regression: on-main spawn (`b c`) is inherently single-target
    /// too — a `v` selection must not fan it out or mislabel it as an
    /// archive.
    #[test]
    fn on_main_spawn_under_selection_stays_focused_only() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        let a = workspace("owner/repo#1", true, Duration::hours(1));
        let focus_key = SessionKey::from(&a.key);
        seed_and_select(
            &mut m,
            vec![a, workspace("owner/repo#2", true, Duration::hours(2))],
        );
        assert!(m.sidebar.focus_workspace_key(&focus_key));

        assert!(
            m.dispatch_action(&Action::SpawnAgentOnMain("claude".into()))
                .is_empty()
        );
        match &m.modal_flow {
            Some(ModalFlow::ActionConfirm {
                action: Action::SpawnAgentOnMain(_),
                targets,
            }) => assert_eq!(
                targets.as_slice(),
                [ActionConfirmTarget::Workspace(focus_key.clone())],
                "on-main spawn stays focused-only under a selection",
            ),
            other => panic!("expected a single-target on-main confirm, got {other:?}"),
        }
    }

    /// #899 regression: a bulk `w w` over a *mixed* selection (one live
    /// agent → inject, one no-agent → spawn) gates behind the spawn
    /// confirm. Cancelling must record NOTHING into the live agent's
    /// recap — before the fix the inject's `record_pty_write` ran at
    /// plan-build time, leaving a phantom prompt the user never sent.
    #[test]
    fn cancelled_bulk_work_records_no_phantom_prompt() {
        use lazybox_ipc::{TerminalId, TerminalKind};
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        // A row with a live agent (will be an inject target) …
        let live = workspace("owner/repo#1", true, Duration::hours(1));
        let live_sk: SessionKey = (&live.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(live)));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: live_sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        // … and a row with no agent (a spawn target), so `spawned > 0`
        // forces the confirm gate.
        let fresh = workspace("owner/repo#2", true, Duration::hours(2));
        let fresh_sk: SessionKey = (&fresh.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(fresh)));

        for key in [&live_sk, &fresh_sk] {
            assert!(m.sidebar.focus_workspace_key(key));
            m.sidebar.toggle_broadcast_select();
        }

        assert!(m.dispatch_action(&Action::Work).is_empty());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::BulkSpawnConfirm),
            "a spawn in the mix gates the fan-out",
        );

        // Cancel: nothing runs, so nothing is recorded.
        let cmds = m.handle_confirmed(false);
        assert!(cmds.is_empty(), "cancel emits no commands");
        assert!(
            m.terminals.prompt_history_for(TerminalId(7)).is_none(),
            "cancelling must not leave a phantom prompt in the live agent's recap",
        );

        // Confirming instead does record + inject.
        assert!(m.dispatch_action(&Action::Work).is_empty());
        let cmds = m.handle_confirmed(true);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, IpcCommand::InjectPrompt { .. })),
            "confirm injects into the live agent",
        );
        assert!(
            m.terminals.prompt_history_for(TerminalId(7)).is_some(),
            "confirm records the prompt it actually delivered",
        );
    }

    fn conflicting_pr(key: &str) -> Workspace {
        let mut ws = workspace(key, true, Duration::hours(1));
        ws.pr.as_mut().unwrap().mergeable = lazybox_core::Mergeable::Conflicting;
        ws
    }

    fn stacked_pr(key: &str, head: &str, base: &str) -> Workspace {
        let mut ws = workspace(key, true, Duration::hours(1));
        let pr = ws.pr.as_mut().unwrap();
        pr.branch = Some(head.into());
        pr.base_branch = Some(base.into());
        ws
    }

    /// Issue #969: `g m` on a PR stacked on a still-open parent warns
    /// that merging lands the stack out of order, naming the parent — so
    /// the user restacks the children instead of discovering the retarget
    /// after the fact. The bottom of the stack keeps the default prompt.
    #[test]
    fn g_m_on_a_stacked_child_warns_before_merging_out_of_order() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let parent = stacked_pr("owner/repo#1", "feat-a", "main");
        let child = stacked_pr("owner/repo#2", "feat-b", "feat-a");
        let parent_key = SessionKey::from(&parent.key);
        let child_key = SessionKey::from(&child.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(parent)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(child)));

        assert!(m.sidebar.focus_workspace_key(&child_key));
        let prompt = m
            .action_confirm_override(&Action::MergePr)
            .expect("a stacked child gets a stack-aware confirm prompt");
        assert!(
            prompt.contains("stacked on #1") && prompt.contains("out of order"),
            "unexpected stacked-merge prompt: {prompt}",
        );

        // The bottom PR (based on main) merges with the default prompt.
        assert!(m.sidebar.focus_workspace_key(&parent_key));
        assert!(
            m.action_confirm_override(&Action::MergePr).is_none(),
            "the bottom of the stack keeps the default merge confirm",
        );
    }

    /// Issue #947: `g m` on a PR lazybox already knows is conflicting is
    /// a doomed dispatch. Instead of a merge confirm that can only fail,
    /// route straight to the one-key resolve prompt — no `MergePr`
    /// command leaves.
    #[test]
    fn g_m_on_a_conflicting_pr_offers_the_resolve_prompt() {
        use lazybox_tui_core::action::Action;

        let mut m = build_model();
        let ws = conflicting_pr("owner/repo#1");
        let sk: SessionKey = (&ws.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        let cmds = m.dispatch_action(&Action::MergePr);
        assert!(
            cmds.is_empty(),
            "a doomed merge must not dispatch: {cmds:?}",
        );
        assert_eq!(
            m.top_modal(),
            Some(&Id::ConflictResolve),
            "the resolve prompt is offered instead of a merge confirm",
        );
        assert!(matches!(
            m.modal_flow,
            Some(ModalFlow::ConflictResolve { ref workspace }) if *workspace == sk,
        ));
    }

    /// Issue #947: a merge that GitHub rejected for conflicts surfaces as
    /// the actionable resolve prompt, not a dead-end red error.
    #[test]
    fn pr_merge_failed_with_conflict_offers_resolve() {
        let mut m = build_model();
        let ws = conflicting_pr("owner/repo#1");
        let key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: key,
            pr_label: "owner/repo#1".into(),
            reason: "Can't merge — the branch has merge conflicts with its base.".into(),
            conflict: true,
        });

        assert_eq!(
            m.top_modal(),
            Some(&Id::ConflictResolve),
            "a conflict failure offers the resolve prompt",
        );
        assert!(
            m.status.notice.is_none(),
            "no dead-end error banner when we can offer a resolve",
        );
    }

    /// #1055: the conflict-resolve prompt is an async `PrMergeFailed`
    /// reply, so — like every other async daemon mount — it must not
    /// preempt a modal the user already has open. A `default_yes()` confirm
    /// popping onto the stack under a stall would let a buffered `Enter`
    /// (aimed at the open picker) spawn the resolution agent unprompted.
    /// The offer is dropped with a `g m` hint; the CONFLICT pill re-arms it.
    #[test]
    fn pr_merge_failed_with_conflict_does_not_preempt_an_open_modal() {
        let mut m = build_model();
        let ws = conflicting_pr("owner/repo#1");
        let key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));

        // The user has the snippet picker open (a retaining modal).
        m.modal_stack.push(Id::SnippetPicker);

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: key,
            pr_label: "owner/repo#1".into(),
            reason: "Can't merge — the branch has merge conflicts with its base.".into(),
            conflict: true,
        });

        assert_eq!(
            m.top_modal(),
            Some(&Id::SnippetPicker),
            "the open modal wins — the resolve confirm must not stack over it",
        );
        assert!(
            !m.modal_stack.contains(&Id::ConflictResolve),
            "no conflict-resolve confirm was mounted under the picker either",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("g m")),
            "a hint points at re-triggering the resolve",
        );
    }

    /// Issue #947: a non-conflict merge failure keeps the existing
    /// persistent-error surface — the resolve prompt is conflict-only.
    #[test]
    fn pr_merge_failed_without_conflict_still_errors() {
        let mut m = build_model();
        let ws = workspace("owner/repo#1", true, Duration::hours(1));
        let key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: key,
            pr_label: "owner/repo#1".into(),
            reason: "changes were requested".into(),
            conflict: false,
        });

        assert!(
            m.top_modal().is_none(),
            "a non-conflict failure never opens the resolve prompt",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("merge failed")),
            "the persistent error still surfaces",
        );
    }

    /// Issue #947: accepting the resolve prompt spawns/attaches the
    /// agent with the conflict-resolution prompt and re-syncs the PR so
    /// a stale CONFLICT can't strand the user (ties #144).
    #[test]
    fn confirming_resolve_spawns_conflict_agent_and_syncs() {
        let mut m = build_model();
        let ws = conflicting_pr("owner/repo#1");
        let key = ws.key.clone();
        let sk: SessionKey = (&key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));

        m.handle_daemon_event(IpcEvent::PrMergeFailed {
            workspace_key: key.clone(),
            pr_label: "owner/repo#1".into(),
            reason: "merge conflicts".into(),
            conflict: true,
        });
        assert_eq!(m.top_modal(), Some(&Id::ConflictResolve));

        let cmds = m.handle_confirmed(true);

        let spawned = cmds.iter().find_map(|c| match c {
            IpcCommand::Spawn {
                session_key,
                initial_prompt: Some(prompt),
                ..
            } => Some((session_key.clone(), prompt.clone())),
            _ => None,
        });
        let (spawn_key, prompt) = spawned.expect("resolve spawns an agent with a prompt");
        assert_eq!(spawn_key, sk, "the spawn targets the conflicting workspace");
        assert!(
            prompt.contains("conflict"),
            "the injected prompt is the conflict-resolution flow: {prompt}",
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, IpcCommand::SyncWorkspace { workspace_key } if *workspace_key == key)),
            "the resolve path re-syncs the PR's mergeable state (#144): {cmds:?}",
        );
    }

    /// Issues #947 + #899: a bulk `g m` over a `v`-selection must NOT be
    /// hijacked into the single-workspace resolve prompt just because the
    /// cursor row is conflicting — it belongs to the bulk fan-out, which
    /// reports conflicting PRs as skipped. The resolve prompt is a
    /// single-target affordance only.
    #[test]
    fn bulk_g_m_with_conflicting_cursor_row_does_not_hijack_into_resolve() {
        use lazybox_tui_core::action::Action;
        let mut m = build_model();
        // Cursor lands on the conflicting row (seeded last).
        seed_and_select(
            &mut m,
            vec![
                workspace("owner/repo#1", true, Duration::hours(1)),
                conflicting_pr("owner/repo#2"),
            ],
        );

        assert!(m.dispatch_action(&Action::MergePr).is_empty());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::ActionConfirm),
            "a bulk merge routes to the bulk confirm, not the resolve prompt",
        );
        assert!(
            !matches!(m.modal_flow, Some(ModalFlow::ConflictResolve { .. })),
            "the single-target resolve prompt must not preempt a bulk merge",
        );
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
            // `Enter` also drives the Global `InspectNotice` (rank 0),
            // but only ever fires from the explicit sticky-error branch
            // in `handle_pane_key`; it is pane-native (not catalog-
            // dispatched), so it falls through to the pane's own Enter
            // when no error is up — exactly like Esc/DismissNotice.
            // Sidebar sees InspectNotice + OpenWorkspace; the activity
            // pane adds its own ToggleActivity.
            (
                Chord::Key(stroke("Enter")),
                vec![ActionKind::InspectNotice, ActionKind::OpenWorkspace],
            ),
            (
                Chord::Key(stroke("Enter")),
                vec![
                    ActionKind::InspectNotice,
                    ActionKind::OpenWorkspace,
                    ActionKind::ToggleActivity,
                ],
            ),
            (
                Chord::Key(stroke("z")),
                vec![ActionKind::ToggleSnooze, ActionKind::UndoMarkRead],
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
            "`G` on the activity pane is jump-to-bottom",
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

    /// The broadcast pair resolves only under sidebar focus: `v`
    /// toggles the multi-select and `Shift-B` opens the broadcast —
    /// while the activity pane keeps its own pane-local `v` (row
    /// multi-select), which must not be shadowed by a catalog entry.
    #[test]
    fn broadcast_keys_resolve_only_under_sidebar_focus() {
        assert_eq!(
            resolve("v", PaneFocus::Sidebar),
            Some(ActionKind::SelectWorkspace),
        );
        assert_eq!(
            resolve("Shift-B", PaneFocus::Sidebar),
            Some(ActionKind::BroadcastToSelected),
        );
        assert_eq!(
            resolve("v", PaneFocus::Right),
            None,
            "activity-pane `v` stays pane-local",
        );
        assert_eq!(resolve("Shift-B", PaneFocus::Right), None);
    }

    #[test]
    fn sidebar_focus_resolution_is_unchanged() {
        assert_eq!(
            resolve("z", PaneFocus::Sidebar),
            Some(ActionKind::ToggleSnooze),
        );
        // Assignees dropped its `Shift-G` alias (#304) — the `g a`
        // leader chord is its only binding, so the bare stroke resolves
        // to nothing from the sidebar.
        assert_eq!(resolve("Shift-G", PaneFocus::Sidebar), None);
        assert_eq!(
            resolve("m", PaneFocus::Sidebar),
            Some(ActionKind::MarkAllRead),
        );
        // Activity-only entries must not leak into sidebar dispatch
        // (`g` is the github group leader there).
        assert_eq!(resolve("g", PaneFocus::Sidebar), None);
    }

    /// Repo-group collapse migrated from an out-of-catalog sidebar
    /// pane arm into the catalog (#338): `Space` now resolves to
    /// `ToggleRepoGroup` under sidebar focus, so it shows in help and
    /// is remappable. Under activity focus `Space` keeps its own
    /// pane-scoped meaning (`SelectRow`) — the two never collide
    /// because each resolves under a different focus.
    #[test]
    fn space_collapses_repo_group_via_catalog() {
        assert_eq!(
            resolve("Space", PaneFocus::Sidebar),
            Some(ActionKind::ToggleRepoGroup),
            "`Space` on the sidebar collapses the repo group",
        );
        assert_eq!(
            resolve("Space", PaneFocus::Right),
            Some(ActionKind::SelectRow),
            "`Space` on the activity pane selects the row",
        );
    }

    /// `p` under sidebar focus pins/unpins the cursor's repo group
    /// (#760). Guards the key-routing contract: nothing pane-native
    /// swallows `p` before catalog dispatch, and it doesn't leak into
    /// the activity pane (which has no pin action).
    #[test]
    fn p_pins_repo_group_via_catalog() {
        assert_eq!(
            resolve("p", PaneFocus::Sidebar),
            Some(ActionKind::ToggleRepoPin),
            "`p` on the sidebar pins the repo group",
        );
        assert_eq!(
            resolve("p", PaneFocus::Right),
            None,
            "`p` has no meaning in the activity pane",
        );
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
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
            first_seq: 1,
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
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: (&key).into(),
            kind: lazybox_ipc::TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        m.redraw = false;
        m.handle_daemon_event(IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes: b"$ ls\n".to_vec(),
            first_seq: 1,
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
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: session_key.clone(),
            kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        m.handle_daemon_event(IpcEvent::AgentState {
            terminal_id: TerminalId(1),
            session_key: session_key.clone(),
            state: AgentState::Working,
        });

        // Second agent spawns Idle — sidebar already shows Working
        // for the session, but THIS tab's badge is stale.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: session_key.clone(),
            kind: lazybox_ipc::TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
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
    //! every wheel scrolls lazybox's local history, independent of the
    //! terminal's screen and mouse-tracking modes. No wheel bytes are
    //! ever written into the inner program.
    use super::super::*;
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use tuirealm::ratatui::layout::{Rect, Size};

    fn build_model_with_kind(
        kind: TerminalKind,
    ) -> (
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
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: key,
            kind,
            no_permission: false,
            on_main: false,
        });
        m.focus = PaneFocus::Terminals;

        let (_, _, bottom) = crate::realm::layout::pane_areas(
            m.layout.last_area,
            m.layout.sidebar_pct,
            m.layout.right_top_pct,
            m.layout.sidebar_user_resized,
        );
        // Render once so the terminal pane records its tile rect. The
        // wheel handler hit-tests wheel coordinates against it (#362),
        // and in the real app a render always precedes a mouse event.
        {
            use tuirealm::ratatui::{Terminal, backend::TestBackend};
            let mut term = Terminal::new(TestBackend::new(
                m.layout.last_area.width,
                m.layout.last_area.height,
            ))
            .unwrap();
            term.draw(|f| m.terminals.view_in(bottom, f)).unwrap();
        }
        (m, server, bottom)
    }

    fn build_model_with_terminal() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
        Rect,
    ) {
        build_model_with_kind(TerminalKind::Shell)
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

        // The viewport move is in-process; the only allowed daemon
        // traffic is the first-scroll deep-scrollback fetch (#393) —
        // never a Write, which would leak the wheel into the inner
        // program.
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                matches!(cmd, lazybox_ipc::Command::FetchScrollback { .. }),
                "local scrollback path must not send PTY traffic: {cmd:?}"
            );
        }
        assert!(m.redraw, "local scroll repaints the viewport");
    }

    /// Mouse tracking and the alternate-screen bit must never divert a
    /// wheel into the agent. Claude ignores forwarded SGR wheel reports,
    /// so forwarding here made the gesture a silent no-op.
    #[test]
    fn wheel_never_forwards_to_alt_screen_mouse_tracking_agent() {
        let (mut m, mut server, bottom) =
            build_model_with_kind(TerminalKind::Agent("claude".into()));

        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes: b"\x1b[?1049h\x1b[?1002h\x1b[?1006h".to_vec(),
            first_seq: 1,
            seq: 1,
        });

        while server.rx.try_recv().is_ok() {}
        m.handle_mouse(wheel_up_at(bottom.x + 6, bottom.y + 8));

        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                !matches!(cmd, lazybox_ipc::Command::Write { .. }),
                "an agent wheel must never be forwarded to the PTY: {cmd:?}",
            );
        }
        assert!(m.redraw, "the local wheel path requests a repaint");
    }

    /// Pane chrome falls back to the focused tile. It must keep the same
    /// local behavior as a wheel directly over the terminal grid.
    #[test]
    fn wheel_over_pane_chrome_scrolls_focused_history() {
        let (mut m, mut server, bottom) = build_model_with_terminal();
        let mut bytes = Vec::new();
        for i in 0..200 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes,
            first_seq: 1,
            seq: 1,
        });
        let before = scroll_offset(&m);
        while server.rx.try_recv().is_ok() {}

        m.handle_mouse(wheel_up_at(bottom.x + 2, bottom.y + 1));
        assert_eq!(
            scroll_offset(&m),
            before - 3,
            "chrome fallback scrolls the focused terminal"
        );
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(!matches!(cmd, lazybox_ipc::Command::Write { .. }));
        }
    }

    /// Pull the `offset=` field out of `scrollbar_summary`.
    fn scroll_offset(m: &Model<tuirealm::terminal::TestTerminalAdapter>) -> u64 {
        let summary = m.terminals.scrollbar_summary().expect("summary");
        summary
            .split_whitespace()
            .find_map(|kv| kv.strip_prefix("offset="))
            .expect("offset field")
            .parse()
            .expect("numeric offset")
    }

    /// Regression for #306: each wheel notch over the terminal body must
    /// actually move the scrollback viewport. The full route — mouse
    /// dispatch → `scroll_active` → `scroll_viewport(Delta)` →
    /// `scrollbar().offset` — used to be blamed on a libghostty Delta
    /// no-op; this pins the whole chain end to end so a regression in
    /// any hop (routing, damping, FFI) fails loudly.
    #[test]
    fn wheel_moves_the_scrollback_offset() {
        let (mut m, _server, bottom) = build_model_with_terminal();
        let mut bytes = Vec::new();
        for i in 0..200 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes,
            first_seq: 1,
            seq: 1,
        });

        let bottom_offset = scroll_offset(&m);
        assert!(bottom_offset > 0, "200 lines must produce scrollback");

        // One notch = LOCAL_WHEEL_STEP (3) rows up.
        m.handle_mouse(wheel_up_at(bottom.x + 2, bottom.y + 4));
        assert_eq!(scroll_offset(&m), bottom_offset - 3);

        // Sustained scrolling keeps walking toward the top.
        for _ in 0..5 {
            m.handle_mouse(wheel_up_at(bottom.x + 2, bottom.y + 4));
        }
        assert_eq!(scroll_offset(&m), bottom_offset - 18);
    }

    /// Agent identity is not part of wheel routing. Once the backend has
    /// exposed retained history, both Codex and Claude scroll it through
    /// the same local viewport path even when they request mouse clicks.
    #[test]
    fn codex_and_claude_scroll_retained_history_locally() {
        for agent_id in ["codex", "claude"] {
            let (mut m, mut server, bottom) =
                build_model_with_kind(TerminalKind::Agent(agent_id.into()));
            let mut history = Vec::new();
            for i in 0..200 {
                history.extend_from_slice(format!("{agent_id} line {i}\r\n").as_bytes());
            }
            if agent_id == "claude" {
                m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
                    terminal_id: TerminalId(7),
                    bytes: b"\x1b[?1049h\x1b[?1002h\x1b[?1006h".to_vec(),
                    first_seq: 1,
                    seq: 1,
                });
                m.terminals.on_daemon_event(&IpcEvent::TerminalScrollback {
                    terminal_id: TerminalId(7),
                    replay: history,
                    seq: 2,
                });
            } else {
                let mut bytes = b"\x1b[?1002h\x1b[?1006h".to_vec();
                bytes.extend_from_slice(&history);
                m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
                    terminal_id: TerminalId(7),
                    bytes,
                    first_seq: 1,
                    seq: 1,
                });
            }
            assert!(m.terminals.focused_terminal_tracks_mouse());
            let bottom_offset = scroll_offset(&m);
            assert!(bottom_offset > 0, "{agent_id} must have scrollback");

            while server.rx.try_recv().is_ok() {}
            m.handle_mouse(wheel_up_at(bottom.x + 6, bottom.y + 8));
            assert_eq!(
                scroll_offset(&m),
                bottom_offset - 3,
                "{agent_id} must scroll through lazybox history",
            );
            while let Ok(cmd) = server.rx.try_recv() {
                assert!(
                    matches!(cmd, lazybox_ipc::Command::FetchScrollback { .. }),
                    "{agent_id} wheel must not reach the PTY: {cmd:?}",
                );
            }
        }
    }

    /// A brand-new mouse-tracking agent still owns no wheel events.
    /// Its first upward gesture may fetch retained history, and later
    /// gestures move that history without changing input routing.
    #[test]
    fn fresh_primary_screen_agent_scrolls_local_scrollback() {
        let (mut m, mut server, bottom) =
            build_model_with_kind(TerminalKind::Agent("codex".into()));

        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes: b"\x1b[?1002h\x1b[?1006h".to_vec(),
            first_seq: 1,
            seq: 1,
        });
        assert!(m.terminals.focused_terminal_tracks_mouse());
        while server.rx.try_recv().is_ok() {}
        m.handle_mouse(wheel_up_at(bottom.x + 6, bottom.y + 8));
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                matches!(cmd, lazybox_ipc::Command::FetchScrollback { .. }),
                "a fresh wheel must not reach the PTY: {cmd:?}",
            );
        }

        let mut bytes = Vec::new();
        for i in 0..200 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes,
            first_seq: 2,
            seq: 2,
        });
        let bottom_offset = scroll_offset(&m);
        assert!(bottom_offset > 0, "200 lines must produce scrollback");
        while server.rx.try_recv().is_ok() {}
        m.handle_mouse(wheel_up_at(bottom.x + 6, bottom.y + 8));
        assert_eq!(
            scroll_offset(&m),
            bottom_offset - 3,
            "the wheel must move the fresh agent's viewport into scrollback",
        );
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                matches!(cmd, lazybox_ipc::Command::FetchScrollback { .. }),
                "the local scroll must not forward an SGR report: {cmd:?}",
            );
        }
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

    // (Ctrl-w now forwards straight to the PTY — no lazybox prefix,
    // #286 — unit-tested at the TerminalStack level in
    // `components::terminal_stack::ctrl_w_tests`.)
}

#[cfg(test)]
mod input_priority_tests {
    //! Input-priority pre-dispatch (#1134): while a focused agent
    //! terminal is repainting full-screen, the `!Send` VT build blocks
    //! the loop for tens of ms. `drain_priority_input` forwards buffered
    //! keystrokes to the PTY *before* that build runs, so a key reaches
    //! the process a frame sooner. These freeze the reorder's contract:
    //! it fires only for a focused terminal with a pending build, and it
    //! leaves the buffer alone in every other pane/state.
    use super::super::helpers::{
        PerfMonitor, PhaseTimings, StaleInputTally, TimedInput, drain_priority_input,
    };
    use super::super::{Model, PaneFocus};
    use crossterm::event::{
        Event as CtEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
    };
    use lazybox_core::{SessionKey, Workspace, WorkspaceKey};
    use lazybox_ipc::{Command, Event as IpcEvent, TerminalId, TerminalKind, channel};
    use std::time::Instant;
    use tuirealm::ratatui::layout::{Rect, Size};

    const TID: TerminalId = TerminalId(7);

    /// A focused agent terminal backed by a real sidebar workspace. The
    /// workspace matters: `handle_pane_key` re-runs `sync_panes` after
    /// every dispatch, which re-binds the terminals to the sidebar's
    /// selected workspace — without a matching row it would clear the
    /// terminal out from under the second buffered key.
    fn model_with_focused_agent() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
    ) {
        let (client, server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.layout.last_area = Rect::new(0, 0, 120, 40);
        let ws_key = WorkspaceKey::new("github:o/r#1");
        let session_key: SessionKey = (&ws_key).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(ws_key, "main", chrono::Utc::now())],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(m.sidebar.focus_workspace_key(&session_key));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TID,
            session_key,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        assert_eq!(m.terminals.active_terminal_id(), Some(TID));
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        (m, server)
    }

    fn key_input(c: char) -> TimedInput {
        TimedInput {
            read_at: Instant::now(),
            event: CtEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
        }
    }

    fn wheel_input() -> TimedInput {
        TimedInput {
            read_at: Instant::now(),
            event: CtEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 4,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }),
        }
    }

    /// Drain the startup handshake (Subscribe etc.) so a later assertion
    /// only sees what the pre-dispatch produced.
    fn drain_startup(server: &mut lazybox_ipc::Connection) {
        while server.rx.try_recv().is_ok() {}
    }

    fn written_bytes(server: &mut lazybox_ipc::Connection) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(cmd) = server.rx.try_recv() {
            if let Command::Write {
                terminal_id, bytes, ..
            } = cmd
            {
                assert_eq!(terminal_id, TID, "write must target the focused terminal");
                out.extend(bytes);
            }
        }
        out
    }

    /// The headline fix: with a build pending on a focused terminal, a
    /// buffered keystroke is forwarded to the PTY by the pre-dispatch —
    /// ahead of the render — and the buffer is drained.
    #[test]
    fn pending_keystroke_reaches_pty_before_the_build() {
        let (mut m, mut server) = model_with_focused_agent();
        drain_startup(&mut server);
        m.redraw = true;

        let (itx, mut irx) = tokio::sync::mpsc::channel(8);
        itx.try_send(key_input('a')).expect("room");
        itx.try_send(key_input('b')).expect("room");

        let mut redraw_is_input = false;
        drain_priority_input(
            &mut m,
            &mut irx,
            &mut StaleInputTally::default(),
            &mut PerfMonitor::new(),
            &mut PhaseTimings::default(),
            &mut redraw_is_input,
        );

        assert_eq!(
            written_bytes(&mut server),
            b"ab",
            "both keys hit the PTY early"
        );
        assert!(irx.try_recv().is_err(), "the input buffer is drained");
        assert!(redraw_is_input, "a discrete key arms the immediate paint");
    }

    /// A no-op unless a build is pending — with nothing to render there is
    /// no frame to jump ahead of, so the key rides the normal post-wait
    /// path and the buffer is left untouched.
    #[test]
    fn no_pending_render_leaves_the_buffer_alone() {
        let (mut m, mut server) = model_with_focused_agent();
        drain_startup(&mut server);
        m.redraw = false;

        let (itx, mut irx) = tokio::sync::mpsc::channel(8);
        itx.try_send(key_input('a')).expect("room");

        let mut redraw_is_input = false;
        drain_priority_input(
            &mut m,
            &mut irx,
            &mut StaleInputTally::default(),
            &mut PerfMonitor::new(),
            &mut PhaseTimings::default(),
            &mut redraw_is_input,
        );

        assert!(written_bytes(&mut server).is_empty(), "no early write");
        assert!(
            irx.try_recv().is_ok(),
            "the key stays buffered for the post-wait path"
        );
    }

    /// Scoped to the terminal: with focus on another pane the pre-dispatch
    /// is inert, so a sidebar key can't be diverted into the PTY.
    #[test]
    fn other_pane_focus_is_inert() {
        let (mut m, mut server) = model_with_focused_agent();
        m.focus = PaneFocus::Sidebar;
        drain_startup(&mut server);
        m.redraw = true;

        let (itx, mut irx) = tokio::sync::mpsc::channel(8);
        itx.try_send(key_input('a')).expect("room");

        let mut redraw_is_input = false;
        drain_priority_input(
            &mut m,
            &mut irx,
            &mut StaleInputTally::default(),
            &mut PerfMonitor::new(),
            &mut PhaseTimings::default(),
            &mut redraw_is_input,
        );

        assert!(
            written_bytes(&mut server).is_empty(),
            "no write off the terminal pane"
        );
        assert!(irx.try_recv().is_ok(), "the key stays buffered");
    }

    /// A scroll notch breaks the drain so scrollback keeps its
    /// one-step-per-frame progression: the wheel event is serviced, but the
    /// keystroke queued behind it is left for the next iteration.
    #[test]
    fn scroll_notch_breaks_the_drain() {
        let (mut m, mut server) = model_with_focused_agent();
        drain_startup(&mut server);
        m.redraw = true;

        let (itx, mut irx) = tokio::sync::mpsc::channel(8);
        itx.try_send(wheel_input()).expect("room");
        itx.try_send(key_input('a')).expect("room");

        let mut redraw_is_input = false;
        drain_priority_input(
            &mut m,
            &mut irx,
            &mut StaleInputTally::default(),
            &mut PerfMonitor::new(),
            &mut PhaseTimings::default(),
            &mut redraw_is_input,
        );

        // The wheel is a local scroll (no PTY bytes) and the drain stops
        // there, so the buffered 'a' survives for the post-wait path.
        assert!(
            written_bytes(&mut server).is_empty(),
            "wheel scrolls locally, no PTY write"
        );
        assert!(
            irx.try_recv().is_ok(),
            "the key behind the scroll stays buffered"
        );
    }
}

#[cfg(test)]
mod leader_tile_tests {
    //! `]]` leader tile commands (#286): tile/split management rides
    //! the same leader as every other terminal-mode chord, replacing
    //! the retired `Ctrl-w` prefix.
    use super::super::*;
    use lazybox_ipc::{
        Command as IpcCommand, Event as IpcEvent, TerminalId, TerminalKind, channel,
    };
    use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};
    use tuirealm::ratatui::layout::Size;

    fn build_model_with_terminals(
        n: u64,
    ) -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
    ) {
        let (client, server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let key = lazybox_core::SessionKey::from("github:o/r#1");
        m.terminals.set_active_session(Some(key.clone()));
        for id in 1..=n {
            m.terminals.on_daemon_event(&IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(id),
                session_key: key.clone(),
                kind: TerminalKind::Shell,
                no_permission: false,
                on_main: false,
            });
        }
        m.focus = PaneFocus::Terminals;
        (m, server)
    }

    fn two_leaf_split() -> lazybox_core::SessionLayout {
        lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![0],
        }
    }

    fn arm_leader(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) {
        m.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        m.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        assert!(m.terminal_leader_pending(), "`]]` arms the leader");
    }

    /// `]]|` splits the focused tile: a Shell spawn goes to the daemon
    /// and the leader is consumed. `|` arrives shifted on most hosts,
    /// so the chord must accept the SHIFT modifier.
    #[test]
    fn leader_pipe_splits_the_focused_tile() {
        let (mut m, mut server) = build_model_with_terminals(1);
        while server.rx.try_recv().is_ok() {}
        arm_leader(&mut m);
        m.dispatch_key(RealmKey::new(Key::Char('|'), RealmMods::SHIFT));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert_eq!(m.focus(), PaneFocus::Terminals, "split stays in the pane");
        let mut saw_spawn = false;
        while let Ok(cmd) = server.rx.try_recv() {
            if matches!(
                cmd,
                IpcCommand::Spawn {
                    kind: TerminalKind::Shell,
                    ..
                }
            ) {
                saw_spawn = true;
            }
        }
        assert!(saw_spawn, "`]]|` emits a Shell spawn for the new tile");
    }

    /// `]]→` moves tile focus across the split and persists the layout.
    #[test]
    fn leader_arrow_moves_tile_focus() {
        let (mut m, mut server) = build_model_with_terminals(2);
        m.terminals.set_layout(two_leaf_split());
        while server.rx.try_recv().is_ok() {}
        arm_leader(&mut m);
        m.dispatch_key(RealmKey::new(Key::Right, RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert_eq!(
            m.terminals.focused_terminal_id(),
            Some(TerminalId(2)),
            "`]]→` moves focus to the right tile"
        );
        let mut saw_persist = false;
        while let Ok(cmd) = server.rx.try_recv() {
            if matches!(cmd, IpcCommand::SetSessionLayout { .. }) {
                saw_persist = true;
            }
        }
        assert!(saw_persist, "tile-focus moves persist the layout");
    }

    #[test]
    fn clicking_a_terminal_tile_focuses_it_and_routes_input_to_it() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use tuirealm::ratatui::{Terminal, backend::TestBackend};

        let (mut m, mut server) = build_model_with_terminals(2);
        m.layout.last_area = Rect::new(0, 0, 120, 40);
        m.terminals.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![1],
        });
        m.terminals.set_active_tab(1);

        let area = m.layout.last_area;
        let (_, _, bottom) = m.effective_pane_rects(area);
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| m.terminals.view_in(bottom, f)).unwrap();
        let col = bottom.x + 4;
        let row = bottom.y + 6;
        assert_eq!(
            m.terminals.scroll_terminal_at(col, row),
            Some(TerminalId(1))
        );
        while server.rx.try_recv().is_ok() {}

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            m.handle_mouse(MouseEvent {
                kind,
                column: col,
                row,
                modifiers: KeyModifiers::empty(),
            });
        }

        assert_eq!(m.focus(), PaneFocus::Terminals);
        assert_eq!(
            m.terminals.focused_terminal_id(),
            Some(TerminalId(1)),
            "the clicked left tile becomes visibly focused",
        );

        m.handle_paste("paste");
        m.dispatch_key(RealmKey::new(Key::Char('a'), RealmMods::NONE));

        let mut persisted_focus = None;
        let mut writes = Vec::new();
        while let Ok(cmd) = server.rx.try_recv() {
            match cmd {
                IpcCommand::SetSessionLayout { layout_json, .. } => {
                    let layout: lazybox_core::SessionLayout =
                        serde_json::from_str(&layout_json).expect("valid persisted layout");
                    if let lazybox_core::SessionLayout::Splits { focused, .. } = layout {
                        persisted_focus = Some(focused);
                    }
                }
                IpcCommand::Write {
                    terminal_id, bytes, ..
                } => {
                    writes.push((terminal_id, bytes));
                }
                _ => {}
            }
        }
        assert_eq!(persisted_focus, Some(vec![0]));
        assert_eq!(
            writes,
            vec![
                (TerminalId(1), b"\x1b[200~paste\x1b[201~".to_vec()),
                (TerminalId(1), b"a".to_vec()),
            ]
        );
    }

    #[test]
    fn clicking_a_terminal_focus_bar_routes_input_to_that_tile() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use tuirealm::ratatui::{Terminal, backend::TestBackend};

        let (mut m, mut server) = build_model_with_terminals(2);
        m.layout.last_area = Rect::new(0, 0, 120, 40);
        m.terminals.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![1],
        });
        m.terminals.set_active_tab(1);

        let area = m.layout.last_area;
        let (_, _, bottom) = m.effective_pane_rects(area);
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| m.terminals.view_in(bottom, f)).unwrap();
        let col = bottom.x + 4;
        let row = bottom.y + 3;
        assert_eq!(m.terminals.scroll_terminal_at(col, row), None);
        assert_eq!(m.terminals.tile_at(col, row), Some(TerminalId(1)));
        while server.rx.try_recv().is_ok() {}

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            m.handle_mouse(MouseEvent {
                kind,
                column: col,
                row,
                modifiers: KeyModifiers::empty(),
            });
        }

        m.dispatch_key(RealmKey::new(Key::Char('b'), RealmMods::NONE));

        let mut persisted_focus = None;
        let mut write = None;
        while let Ok(cmd) = server.rx.try_recv() {
            match cmd {
                IpcCommand::SetSessionLayout { layout_json, .. } => {
                    let layout: lazybox_core::SessionLayout =
                        serde_json::from_str(&layout_json).expect("valid persisted layout");
                    if let lazybox_core::SessionLayout::Splits { focused, .. } = layout {
                        persisted_focus = Some(focused);
                    }
                }
                IpcCommand::Write {
                    terminal_id, bytes, ..
                } => {
                    write = Some((terminal_id, bytes));
                }
                _ => {}
            }
        }
        assert_eq!(persisted_focus, Some(vec![0]));
        assert_eq!(write, Some((TerminalId(1), b"b".to_vec())));
    }

    /// #362: a wheel event over the LEFT tile scrolls the left
    /// terminal's scrollback, not the focused RIGHT one. Before the fix
    /// the handler always scrolled the focused terminal, so hovering the
    /// left tile moved the right shell. Drives the real `handle_mouse`
    /// route end to end.
    #[test]
    fn wheel_over_unfocused_tile_leaves_the_focused_tile_unscrolled() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        use tuirealm::ratatui::{Terminal, backend::TestBackend};

        let (mut m, mut server) = build_model_with_terminals(2);
        m.layout.last_area = Rect::new(0, 0, 120, 40);
        // Focus the RIGHT tile (terminal 2); hover will land on the left.
        m.terminals.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![1],
        });
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(2)));

        // Fill both shells with scrollback so there's somewhere to move.
        for id in [1u64, 2] {
            let mut bytes = Vec::new();
            for i in 0..200 {
                bytes.extend_from_slice(format!("t{id} line {i}\r\n").as_bytes());
            }
            m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
                terminal_id: TerminalId(id),
                bytes,
                first_seq: 1,
                seq: 1,
            });
        }

        // Render the terminal pane so each tile's rect is recorded for
        // the wheel hit-test.
        let area = m.layout.last_area;
        let (_, _, bottom) = m.effective_pane_rects(area);
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| m.terminals.view_in(bottom, f)).unwrap();

        let focused_offset = |m: &Model<tuirealm::terminal::TestTerminalAdapter>| -> u64 {
            m.terminals
                .scrollbar_summary()
                .expect("summary")
                .split_whitespace()
                .find_map(|kv| kv.strip_prefix("offset="))
                .expect("offset field")
                .parse()
                .expect("numeric offset")
        };
        let before = focused_offset(&m);
        while server.rx.try_recv().is_ok() {}

        // Wheel a few cells into the LEFT half of the body.
        m.redraw = false;
        m.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: bottom.x + 4,
            row: bottom.y + 6,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });

        assert_eq!(
            focused_offset(&m),
            before,
            "wheeling over the unfocused left tile must not scroll the focused right tile",
        );
        assert!(m.redraw, "scrolling the hovered tile repaints");
        // The only allowed IPC is the hovered tile's first-scroll
        // deep-scrollback fetch (#393) — no Write may leak, and the
        // fetch must target the HOVERED terminal, not the focused one.
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                matches!(
                    cmd,
                    lazybox_ipc::Command::FetchScrollback {
                        terminal_id: TerminalId(1)
                    }
                ),
                "a local scroll of the hovered tile sends no PTY traffic: {cmd:?}",
            );
        }
    }

    /// Screen and mouse modes on a hovered tile never turn its wheel
    /// into terminal input, even when another tile holds focus.
    #[test]
    fn wheel_over_unfocused_alt_screen_tile_never_writes_to_pty() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        use tuirealm::ratatui::{Terminal, backend::TestBackend};

        let (mut m, mut server) = build_model_with_terminals(2);
        m.layout.last_area = Rect::new(0, 0, 120, 40);
        // Focus the LEFT tile; the RIGHT tile is the mouse-tracking app
        // the wheel hovers.
        m.terminals.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![0],
        });
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(1)));

        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(2),
            bytes: b"\x1b[?1049h\x1b[?1002h\x1b[?1006h".to_vec(),
            first_seq: 1,
            seq: 1,
        });

        let area = m.layout.last_area;
        let (_, _, bottom) = m.effective_pane_rects(area);
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| m.terminals.view_in(bottom, f)).unwrap();

        // A point in the right half of the body lands in the right tile.
        let col = bottom.x + bottom.width * 3 / 4;
        let row = bottom.y + 6;
        assert_eq!(
            m.terminals.scroll_terminal_at(col, row),
            Some(TerminalId(2)),
            "the point is over the right tile",
        );
        while server.rx.try_recv().is_ok() {}

        m.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });

        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                !matches!(cmd, lazybox_ipc::Command::Write { .. }),
                "hovered tile wheel must never become PTY input: {cmd:?}",
            );
        }
    }

    /// `]]` + `j`/`k` move a highlight through the command popup and keep
    /// the leader armed; `Enter` fires the highlighted command (#343).
    /// Row 0 is `s` (snippets); `f` (focus mode) is row 5.
    #[test]
    fn terminal_leader_jk_highlight_and_enter_fire() {
        let (mut m, mut server) = build_model_with_terminals(1);
        while server.rx.try_recv().is_ok() {}
        arm_leader(&mut m);
        assert_eq!(
            m.terminal_leader_highlight(),
            None,
            "no highlight until navigation"
        );

        m.dispatch_key(RealmKey::new(Key::Char('j'), RealmMods::NONE));
        assert_eq!(m.terminal_leader_highlight(), Some(0));
        assert!(
            m.terminal_leader_pending(),
            "`j` navigates, the leader stays armed"
        );
        m.dispatch_key(RealmKey::new(Key::Char('j'), RealmMods::NONE));
        assert_eq!(m.terminal_leader_highlight(), Some(1));
        m.dispatch_key(RealmKey::new(Key::Char('k'), RealmMods::NONE));
        assert_eq!(m.terminal_leader_highlight(), Some(0));
        // Menu order: s,l,r,h,u,f,… — step to `focus mode` at index 5.
        for _ in 0..5 {
            m.dispatch_key(RealmKey::new(Key::Char('j'), RealmMods::NONE));
        }
        assert_eq!(m.terminal_leader_highlight(), Some(5));

        assert!(!m.focus_mode, "focus mode starts off");
        m.dispatch_key(RealmKey::new(Key::Enter, RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "Enter resolves the leader");
        assert_eq!(m.terminal_leader_highlight(), None);
        assert!(
            m.focus_mode,
            "Enter fires the highlighted `focus mode` command"
        );
    }

    /// `]]` + an arrow key still moves tile focus (#286): arrows keep
    /// their tile / tab meaning and are never hijacked for popup
    /// navigation (#343).
    #[test]
    fn terminal_leader_arrows_still_move_tiles_not_the_highlight() {
        let (mut m, mut server) = build_model_with_terminals(2);
        m.terminals.set_layout(two_leaf_split());
        while server.rx.try_recv().is_ok() {}
        arm_leader(&mut m);

        m.dispatch_key(RealmKey::new(Key::Right, RealmMods::NONE));
        assert_eq!(
            m.terminal_leader_highlight(),
            None,
            "arrows don't navigate the popup"
        );
        assert!(
            !m.terminal_leader_pending(),
            "the arrow fired MoveTile and consumed the leader"
        );
        assert_eq!(
            m.terminals.focused_terminal_id(),
            Some(TerminalId(2)),
            "`]]→` moved tile focus, not a highlight"
        );
    }

    /// In a split layout the popup carries a `←↓↑→ move tile` aggregate
    /// row that has no single `Enter`-fireable key; `j`/`k` step past it
    /// so the highlight only ever lands on a dispatchable row (#343).
    #[test]
    fn terminal_leader_jk_skips_the_non_dispatchable_aggregate_row() {
        let (mut m, mut server) = build_model_with_terminals(2);
        m.terminals.set_layout(two_leaf_split());
        while server.rx.try_recv().is_ok() {}
        arm_leader(&mut m);

        // Splits menu order: s,l,r,h,u,f,q,`,|,- then the `move tile`
        // aggregate at index 10, then `x` at index 11. Ten `j` presses
        // reach index 9.
        for _ in 0..10 {
            m.dispatch_key(RealmKey::new(Key::Char('j'), RealmMods::NONE));
        }
        assert_eq!(
            m.terminal_leader_highlight(),
            Some(9),
            "reached the last row before the aggregate"
        );
        m.dispatch_key(RealmKey::new(Key::Char('j'), RealmMods::NONE));
        assert_eq!(
            m.terminal_leader_highlight(),
            Some(11),
            "`j` jumps over the aggregate at index 10"
        );
        m.dispatch_key(RealmKey::new(Key::Char('k'), RealmMods::NONE));
        assert_eq!(
            m.terminal_leader_highlight(),
            Some(9),
            "`k` skips it going back too"
        );
    }

    /// `]]x` closes the focused tile: its PTY is killed daemon-side and
    /// the two-leaf split collapses back to Tabs.
    #[test]
    fn leader_x_closes_focused_tile() {
        let (mut m, mut server) = build_model_with_terminals(2);
        m.terminals.set_layout(two_leaf_split());
        while server.rx.try_recv().is_ok() {}
        arm_leader(&mut m);
        m.dispatch_key(RealmKey::new(Key::Char('x'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        let mut saw_close = false;
        while let Ok(cmd) = server.rx.try_recv() {
            if matches!(cmd, IpcCommand::Close { .. }) {
                saw_close = true;
            }
        }
        assert!(saw_close, "`]]x` kills the focused tile's PTY");
    }

    /// Issue #596: `]]u` scans the focused terminal for URLs. Several
    /// on-screen URLs open the keyboard picker rather than opening blind.
    #[test]
    fn leader_u_opens_the_url_picker_when_several_urls_are_visible() {
        let (mut m, mut server) = build_model_with_terminals(1);
        while server.rx.try_recv().is_ok() {}
        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(1),
            bytes: b"see https://a.example.com and https://b.example.com\r\n".to_vec(),
            first_seq: 1,
            seq: 1,
        });
        arm_leader(&mut m);
        m.dispatch_key(RealmKey::new(Key::Char('u'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert_eq!(
            m.top_modal(),
            Some(&Id::UrlPicker),
            "`]]u` mounts the URL picker for multiple URLs",
        );
    }

    /// With nothing openable on screen, `]]u` opens no picker — just a
    /// footer hint (which mounts no modal).
    #[test]
    fn leader_u_with_no_urls_opens_no_picker() {
        let (mut m, mut server) = build_model_with_terminals(1);
        while server.rx.try_recv().is_ok() {}
        m.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(1),
            bytes: b"no links here\r\n".to_vec(),
            first_seq: 1,
            seq: 1,
        });
        arm_leader(&mut m);
        m.dispatch_key(RealmKey::new(Key::Char('u'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");
        assert!(m.top_modal().is_none(), "no URLs → no picker");
        // A focused-but-URL-less terminal reports "no URLs", never the
        // "no terminal focused" message reserved for the absent-terminal
        // case (#596 review, finding #1).
        assert_eq!(
            m.status.messages.recent().next().map(|e| e.message.clone()),
            Some("no URLs on screen".to_string()),
        );
    }

    /// Issue #373: after a restart the daemon snapshot restores the
    /// in-flight draft; `]]r` recalls it back into the agent composer as
    /// an `InjectPrompt` with `submit: false`, so the recovered text
    /// lands editable rather than being auto-sent.
    #[test]
    fn leader_r_recalls_the_restored_draft_without_submitting() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let key = lazybox_core::SessionKey::from("github:o/r#1");
        m.terminals.set_active_session(Some(key.clone()));
        // A reconnect / fresh-daemon snapshot carrying the persisted
        // draft — the state a restart lands in.
        m.terminals.on_daemon_event(&IpcEvent::Snapshot {
            workspaces: vec![],
            projects: vec![],
            terminals: vec![lazybox_ipc::TerminalSnapshot {
                terminal_id: TerminalId(1),
                session_key: key.clone(),
                kind: TerminalKind::Agent("claude".into()),
                replay: Vec::new(),
                last_seq: 0,
                replay_available: true,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: Vec::new(),
                composing_buffer: Some("\n  recover me".into()),
                agent_state: None,
                authenticating: false,
            }],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        m.focus = PaneFocus::Terminals;
        while server.rx.try_recv().is_ok() {}

        arm_leader(&mut m);
        m.dispatch_key(RealmKey::new(Key::Char('r'), RealmMods::NONE));
        assert!(!m.terminal_leader_pending(), "leader consumed");

        let inject = std::iter::from_fn(|| server.rx.try_recv().ok()).find_map(|c| match c {
            IpcCommand::InjectPrompt {
                terminal_id,
                prompt,
                submit,
                ..
            } => Some((terminal_id, prompt, submit)),
            _ => None,
        });
        match inject {
            Some((terminal_id, prompt, submit)) => {
                assert_eq!(terminal_id, TerminalId(1));
                assert_eq!(prompt, "\n  recover me");
                assert!(!submit, "recall drops text in the composer, unsubmitted");
            }
            None => panic!("`]]r` must emit an InjectPrompt"),
        }
        assert!(
            std::iter::from_fn(|| server.rx.try_recv().ok()).any(|command| matches!(
                command,
                IpcCommand::RecordComposingBuffer { terminal_id, buffer }
                    if terminal_id == TerminalId(1) && buffer == "\n  recover me"
            )),
            "recall must persist the exact draft mirrored by the client",
        );
    }

    /// Issue #523: `]]h` opens the per-session prompt-history picker over
    /// the prompts sent to the focused agent, and picking one re-sends it
    /// into the session (a fresh `Typed` submit via `InjectPrompt`).
    #[test]
    fn leader_h_opens_prompt_history_and_resends_a_pick() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let key = lazybox_core::SessionKey::from("github:o/r#1");
        m.terminals.set_active_session(Some(key.clone()));
        // Seed a two-prompt history via the daemon snapshot (typing-into-
        // history is covered by the terminal_stack integration tests; here
        // the harness doesn't keep the tab focus that live typing needs).
        m.terminals.on_daemon_event(&IpcEvent::Snapshot {
            workspaces: vec![],
            projects: vec![],
            terminals: vec![lazybox_ipc::TerminalSnapshot {
                terminal_id: TerminalId(1),
                session_key: key.clone(),
                kind: TerminalKind::Agent("claude".into()),
                replay: Vec::new(),
                last_seq: 0,
                replay_available: true,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: vec![
                    lazybox_ipc::UserPrompt {
                        text: "rebase onto main".into(),
                        timestamp_ms: 1,
                        source: lazybox_ipc::PromptSource::Typed,
                    },
                    lazybox_ipc::UserPrompt {
                        text: "run the tests".into(),
                        timestamp_ms: 2,
                        source: lazybox_ipc::PromptSource::Snippet {
                            key: "test".into(),
                            category: "CI".into(),
                        },
                    },
                ],
                composing_buffer: None,
                agent_state: None,
                authenticating: false,
            }],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        // A single-leaf split so `focused_terminal_id` resolves via the
        // tile tree (the model harness doesn't drive the tab/active-session
        // focus the real app maintains).
        m.terminals.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::Leaf { terminal_id: 1 },
            focused: vec![],
        });
        m.focus = PaneFocus::Terminals;
        while server.rx.try_recv().is_ok() {}

        // `]]h` opens the history picker.
        arm_leader(&mut m);
        m.dispatch_key(RealmKey::new(Key::Char('h'), RealmMods::NONE));
        assert!(matches!(m.top_modal(), Some(Id::PromptHistoryPicker)));

        // Pick the older prompt → re-sent into the session. The picked
        // row now carries its full prompt text as the payload (no shadow
        // Vec), so the handler re-injects exactly that text.
        let cmds =
            m.handle_choice_picked(vec![ChoicePayload::Text("rebase onto main".to_string())]);
        assert!(m.top_modal().is_none(), "picker closes on pick");
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                IpcCommand::InjectPrompt { terminal_id, prompt, .. }
                    if *terminal_id == TerminalId(1) && prompt == "rebase onto main"
            )),
            "the picked prompt is re-injected: {cmds:?}",
        );

        // If the target agent exited while the picker was open, picking
        // sends nothing and doesn't falsely claim a resend.
        m.modal_flow = Some(super::super::ModalFlow::PromptHistory {
            terminal: TerminalId(404),
        });
        m.push_modal(Id::PromptHistoryPicker);
        let cmds =
            m.handle_choice_picked(vec![ChoicePayload::Text("rebase onto main".to_string())]);
        assert!(
            !cmds.iter().any(|c| matches!(
                c,
                IpcCommand::InjectPrompt { .. } | IpcCommand::Write { .. }
            )),
            "no send to a vanished terminal: {cmds:?}",
        );
    }

    /// `]]h` with no prompts sent yet is a no-op with a hint, not a
    /// silently-empty picker.
    #[test]
    fn leader_h_without_history_flashes_and_does_not_mount() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let key = lazybox_core::SessionKey::from("github:o/r#1");
        m.terminals.set_active_session(Some(key.clone()));
        m.terminals.on_daemon_event(&IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        // A single-leaf split so `focused_terminal_id` resolves via the
        // tile tree (the model harness doesn't drive the tab/active-session
        // focus the real app maintains).
        m.terminals.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::Leaf { terminal_id: 1 },
            focused: vec![],
        });
        m.focus = PaneFocus::Terminals;
        while server.rx.try_recv().is_ok() {}

        arm_leader(&mut m);
        m.dispatch_key(RealmKey::new(Key::Char('h'), RealmMods::NONE));
        assert!(m.top_modal().is_none(), "no picker without history");
    }
}

#[cfg(test)]
mod terminal_url_mouse_tests {
    use super::super::*;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use std::sync::{Arc, Mutex};
    use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};
    use tuirealm::ratatui::layout::Size;
    use tuirealm::ratatui::{Terminal, backend::TestBackend};
    use tuirealm::terminal::TerminalAdapter;

    type TestModel = Model<tuirealm::terminal::TestTerminalAdapter>;

    fn build_model(count: u64) -> (TestModel, lazybox_ipc::Connection, Arc<Mutex<Vec<String>>>) {
        let (client, server) = channel::pair();
        let mut model = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        model.layout.last_area = Rect::new(0, 0, 120, 40);
        let session = lazybox_core::SessionKey::from("github:o/r#1");
        model.terminals.set_active_session(Some(session.clone()));
        for id in 1..=count {
            model.terminals.on_daemon_event(&IpcEvent::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(id),
                session_key: session.clone(),
                kind: TerminalKind::Shell,
                no_permission: false,
                on_main: false,
            });
        }
        model.set_focus(PaneFocus::Terminals);
        let opened = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&opened);
        model.url_opener = Box::new(move |url, _browser| {
            captured
                .lock()
                .expect("opened URL mutex")
                .push(url.to_string());
            Ok(())
        });
        (model, server, opened)
    }

    fn render(model: &mut TestModel) -> Rect {
        let area = model.layout.last_area;
        let (_, _, bottom) = model.effective_pane_rects(area);
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
        terminal
            .draw(|frame| model.terminals.view_in(bottom, frame))
            .expect("terminal render");
        bottom
    }

    fn feed(model: &mut TestModel, terminal_id: u64, bytes: Vec<u8>) {
        model.terminals.on_daemon_event(&IpcEvent::TerminalOutput {
            terminal_id: TerminalId(terminal_id),
            bytes,
            first_seq: 1,
            seq: 1,
        });
    }

    fn body_origin(model: &TestModel, pane: Rect, terminal_id: u64) -> (u16, u16) {
        for row in pane.y..pane.y.saturating_add(pane.height) {
            for col in pane.x..pane.x.saturating_add(pane.width) {
                if model.terminals.scroll_terminal_at(col, row) == Some(TerminalId(terminal_id)) {
                    return (col, row);
                }
            }
        }
        panic!("terminal {terminal_id} has no rendered body");
    }

    fn right_click(model: &mut TestModel, col: u16, row: u16) {
        model.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        });
    }

    fn modifier_left_click(model: &mut TestModel, col: u16, row: u16, modifiers: KeyModifiers) {
        model.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers,
        });
    }

    fn opened_urls(opened: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        opened.lock().expect("opened URL mutex").clone()
    }

    fn rendered_model(model: &mut TestModel) -> String {
        model.view();
        let buffer = model.terminal.raw().backend().buffer();
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|col| buffer[(col, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A drag selection across two side-by-side split tiles stays scoped
    /// to the tile the drag STARTED in: the copy holds only that terminal's
    /// rows, never the neighbour's, and the on-screen highlight never
    /// crosses the tile boundary (#1101). The drag begins in the
    /// *non-focused* left tile, so this pins the mouse-down's
    /// focus-the-clicked-tile step — drop it and the anchor would land in
    /// the previously-focused right tile and copy its text instead. The
    /// highlight-span check covers the reverse-video overlay path
    /// (`selection_screen_span`) that the copy never exercises, and pins the
    /// column clamp directly: selection maps screen coords through the start
    /// tile's recorded grid (#1021), so a column past the tile boundary
    /// clamps to the tile's edge instead of projecting into the adjacent
    /// grid's cells.
    #[test]
    fn drag_selection_scoped_to_the_start_tile() {
        let (mut model, _server, _opened) = build_model(2);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Splits {
                tree: lazybox_core::TileTree::HSplit {
                    left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                    right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                    ratio: 50,
                },
                // Focus the RIGHT tile; the drag below starts in the left one.
                focused: vec![1],
            });
        feed(&mut model, 1, b"AAA0\r\nAAA1\r\nAAA2\r\n".to_vec());
        feed(&mut model, 2, b"BBB0\r\nBBB1\r\nBBB2\r\n".to_vec());
        let pane = render(&mut model);
        let (lx, ly) = body_origin(&model, pane, 1);
        let (rx, _) = body_origin(&model, pane, 2);
        // Press in the interior of the left tile — its horizontal midpoint,
        // maximally clear of the sidebar/right splitter seam that hugs the
        // tile's left edge and would otherwise steal the click as a resize.
        let press_col = (lx + rx) / 2;

        // Press in the left tile's first row, then drag across the tile
        // boundary into the right tile two rows lower.
        model.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: press_col,
            row: ly,
            modifiers: KeyModifiers::empty(),
        });
        model.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: rx + 3,
            row: ly + 2,
            modifiers: KeyModifiers::empty(),
        });

        let drag = model.terminal_drag.expect("left-press claims a drag");
        // The drag binds to the tile it STARTED in (left / terminal 1), not
        // the tile that was focused when it began (right / terminal 2).
        assert_eq!(
            drag.terminal,
            TerminalId(1),
            "the drag anchors to the start tile, not the focused one",
        );

        // The copy holds only the start tile's text. The middle row is
        // fully spanned regardless of the anchor/focus columns, so it must
        // be the start tile's row — and only that.
        let copied = model
            .terminals
            .extract_selection(drag.terminal, drag.anchor, drag.focus);
        assert!(
            copied.contains("AAA1"),
            "the start tile's rows are copied: {copied:?}",
        );
        assert!(
            !copied.contains("BBB"),
            "the adjacent tile never bleeds into the copy: {copied:?}",
        );

        // The highlight span is clamped to the start tile: both projected
        // endpoints stay left of the right tile's first column. A regression
        // that mapped the drag through the whole pane instead of the tile's
        // grid would project the focus into the right tile (>= rx).
        let (hstart, hend) = model
            .terminals
            .selection_screen_span(drag.terminal, pane, drag.anchor, drag.focus)
            .expect("a visible selection span");
        assert!(
            hstart.0 < rx && hend.0 < rx,
            "the highlight never crosses into the right tile (rx={rx}): {hstart:?} {hend:?}",
        );

        // Focus divergence mid-gesture must not redirect the copy: if an
        // event refocuses the other tile before release, extraction still
        // reads the tile the drag started in (`drag.terminal`), not live
        // focus — the guarantee that makes scoping correct-by-construction
        // rather than a side effect of focus following the click.
        let mut cmds = Vec::new();
        model.terminals.focus_tile(TerminalId(2), &mut cmds);
        let after_refocus =
            model
                .terminals
                .extract_selection(drag.terminal, drag.anchor, drag.focus);
        assert!(
            after_refocus.contains("AAA1") && !after_refocus.contains("BBB"),
            "copy stays pinned to the start tile after focus moves: {after_refocus:?}",
        );
    }

    #[test]
    fn right_click_opens_plain_url_through_model_launcher() {
        let (mut model, _server, opened) = build_model(1);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        render(&mut model);
        let url = "https://plain.example.com/path";
        feed(&mut model, 1, format!("see {url}\r\n").into_bytes());
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);

        right_click(&mut model, x + 4, y);

        assert_eq!(opened_urls(&opened), vec![url]);
    }

    #[test]
    fn right_click_opens_soft_wrapped_continuation_through_model_launcher() {
        let (mut model, _server, opened) = build_model(1);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        render(&mut model);
        let url = format!("https://wrapped.example.com/{}", "a".repeat(160));
        feed(&mut model, 1, format!("{url}\r\n").into_bytes());
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);

        right_click(&mut model, x + 2, y + 1);

        assert_eq!(opened_urls(&opened), vec![url]);
    }

    #[test]
    fn right_click_opens_osc8_visible_label_through_model_launcher() {
        let (mut model, _server, opened) = build_model(1);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        render(&mut model);
        let url = "https://osc8.example.com/docs";
        feed(
            &mut model,
            1,
            format!("\x1b]8;;{url}\x1b\\documentation\x1b]8;;\x1b\\\r\n").into_bytes(),
        );
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);

        right_click(&mut model, x + 2, y);

        assert_eq!(opened_urls(&opened), vec![url]);
    }

    #[test]
    fn alt_left_click_opens_url_like_right_click() {
        let (mut model, _server, opened) = build_model(1);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        render(&mut model);
        let url = "https://plain.example.com/path";
        feed(&mut model, 1, format!("see {url}\r\n").into_bytes());
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);

        modifier_left_click(&mut model, x + 4, y, KeyModifiers::ALT);

        assert_eq!(opened_urls(&opened), vec![url]);
    }

    #[test]
    fn ctrl_and_super_left_click_also_open_urls() {
        for modifier in [KeyModifiers::CONTROL, KeyModifiers::SUPER] {
            let (mut model, _server, opened) = build_model(1);
            model
                .terminals
                .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
            render(&mut model);
            let url = "https://plain.example.com/path";
            feed(&mut model, 1, format!("see {url}\r\n").into_bytes());
            let pane = render(&mut model);
            let (x, y) = body_origin(&model, pane, 1);

            modifier_left_click(&mut model, x + 4, y, modifier);

            assert_eq!(opened_urls(&opened), vec![url], "modifier {modifier:?}");
        }
    }

    #[test]
    fn plain_left_click_does_not_open_url() {
        let (mut model, _server, opened) = build_model(1);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        render(&mut model);
        let url = "https://plain.example.com/path";
        feed(&mut model, 1, format!("see {url}\r\n").into_bytes());
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);

        modifier_left_click(&mut model, x + 4, y, KeyModifiers::empty());

        assert!(
            opened_urls(&opened).is_empty(),
            "a plain left-click starts selection, it must not open the link"
        );
    }

    #[test]
    fn modifier_left_click_miss_does_not_open() {
        let (mut model, _server, opened) = build_model(1);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        render(&mut model);
        feed(&mut model, 1, b"no link here\r\n".to_vec());
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);

        modifier_left_click(&mut model, x + 4, y, KeyModifiers::ALT);

        assert!(
            opened_urls(&opened).is_empty(),
            "a modifier-click that misses a link opens nothing"
        );
    }

    fn assert_split_opens_each_unfocused_tile(vertical: bool) {
        let (mut model, _server, opened) = build_model(2);
        let tree = if vertical {
            lazybox_core::TileTree::VSplit {
                top: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                bottom: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            }
        } else {
            lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            }
        };
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Splits {
                tree: tree.clone(),
                focused: vec![1],
            });
        render(&mut model);
        let urls = [
            "https://first-tile.example.com",
            "https://second-tile.example.com",
        ];
        feed(&mut model, 1, format!("{}\r\n", urls[0]).into_bytes());
        feed(&mut model, 2, format!("{}\r\n", urls[1]).into_bytes());
        let pane = render(&mut model);
        let (first_x, first_y) = body_origin(&model, pane, 1);

        right_click(&mut model, first_x + 8, first_y);

        assert_eq!(opened_urls(&opened), vec![urls[0]]);
        assert_eq!(
            model.terminals.focused_terminal_id(),
            Some(TerminalId(2)),
            "URL inspection must not move keyboard focus"
        );

        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Splits {
                tree,
                focused: vec![0],
            });
        let pane = render(&mut model);
        let (second_x, second_y) = body_origin(&model, pane, 2);

        right_click(&mut model, second_x + 8, second_y);

        assert_eq!(opened_urls(&opened), urls);
        assert_eq!(
            model.terminals.focused_terminal_id(),
            Some(TerminalId(1)),
            "URL inspection must not move keyboard focus"
        );
    }

    #[test]
    fn right_click_opens_each_unfocused_horizontal_split_tile() {
        assert_split_opens_each_unfocused_tile(false);
    }

    #[test]
    fn right_click_opens_each_unfocused_vertical_split_tile() {
        assert_split_opens_each_unfocused_tile(true);
    }

    #[test]
    fn right_click_miss_is_forwarded_to_the_clicked_unfocused_split_tile() {
        let (mut model, mut server, opened) = build_model(2);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Splits {
                tree: lazybox_core::TileTree::HSplit {
                    left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                    right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                    ratio: 50,
                },
                focused: vec![1],
            });
        for terminal_id in 1..=2 {
            feed(&mut model, terminal_id, b"\x1b[?1002h\x1b[?1006h".to_vec());
        }
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);
        while server.rx.try_recv().is_ok() {}

        right_click(&mut model, x + 2, y);

        assert!(opened_urls(&opened).is_empty());
        let writes: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok())
            .filter_map(|command| match command {
                IpcCommand::Write {
                    terminal_id,
                    intent,
                    ..
                } => Some((terminal_id, intent)),
                _ => None,
            })
            .collect();
        assert_eq!(
            writes,
            vec![(TerminalId(1), lazybox_ipc::TerminalInputIntent::View)]
        );
        assert_eq!(
            model.terminals.focused_terminal_id(),
            Some(TerminalId(2)),
            "right-click forwarding must not move keyboard focus",
        );
    }

    #[test]
    fn right_click_only_hits_the_visible_tab() {
        let (mut model, _server, opened) = build_model(2);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        render(&mut model);
        let urls = [
            "https://visible-first.example.com",
            "https://hidden-second.example.com",
        ];
        feed(&mut model, 1, format!("{}\r\n", urls[0]).into_bytes());
        feed(&mut model, 2, format!("{}\r\n", urls[1]).into_bytes());
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);
        assert_eq!(
            model.terminals.scroll_terminal_at(x, y),
            Some(TerminalId(1))
        );

        right_click(&mut model, x + 8, y);

        assert_eq!(opened_urls(&opened), vec![urls[0]]);

        model.terminals.set_active_tab(1);
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 2);
        assert_eq!(
            model.terminals.scroll_terminal_at(x, y),
            Some(TerminalId(2))
        );

        right_click(&mut model, x + 8, y);

        assert_eq!(opened_urls(&opened), urls);
    }

    #[test]
    fn host_native_mouse_mode_reports_how_to_enable_url_clicks() {
        let (mut model, _server, opened) = build_model(1);
        model.dispatch_key(RealmKey::new(Key::Function(8), RealmMods::NONE));
        assert!(!model.mouse_capture_on);
        render(&mut model);
        let url = "https://host-mode.example.com";
        feed(&mut model, 1, format!("{url}\r\n").into_bytes());
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);

        right_click(&mut model, x, y);

        assert!(opened_urls(&opened).is_empty());
        let notice = model.status.notice.as_ref().expect("mouse-mode notice");
        assert!(notice.message.contains("right-click links off"));
        assert!(notice.message.contains("]]u"));
        assert!(notice.message.contains("F8"));
    }

    #[test]
    fn mouse_reporting_guidance_tracks_disabled_unverified_and_verified() {
        let (mut model, _server, _opened) = build_model(1);
        model.mouse_capture_on = false;
        model.status.notice = None;

        let host_mode = rendered_model(&mut model);
        assert!(host_mode.contains("F8"));
        assert!(host_mode.contains("links off"));
        assert!(host_mode.contains("]] menu"));

        model.mouse_capture_on = true;
        model.host_mouse_verified = false;
        let unverified = rendered_model(&mut model);
        assert!(unverified.contains("mouse ?"));
        assert!(unverified.contains("host reporting"));

        model.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 80,
            row: 20,
            modifiers: KeyModifiers::empty(),
        });
        let verified = rendered_model(&mut model);
        assert!(verified.contains("]] menu"));
        assert!(!verified.contains("mouse ?"));
        assert!(!verified.contains("host reporting"));
        assert!(!verified.contains("Ctrl-c"));
    }

    #[test]
    fn mouse_capture_refresh_reasserts_through_the_host_boundary() {
        let (mut model, _server, _opened) = build_model(1);
        let requested = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requested);
        model.mouse_capture_requester = Box::new(move |enabled| {
            captured.lock().expect("mouse request mutex").push(enabled);
            Ok(())
        });
        model.mouse_capture_requested_at =
            std::time::Instant::now() - std::time::Duration::from_secs(3);
        let stale_request = model.mouse_capture_requested_at;

        model.tick_mouse_capture();

        assert!(model.mouse_capture_requested_at > stale_request);
        assert_eq!(*requested.lock().expect("mouse request mutex"), vec![true]);
    }

    #[test]
    fn verified_mouse_reporting_does_not_decay_on_idle() {
        let (mut model, _server, _opened) = build_model(1);
        model.host_mouse_verified = true;
        model.mouse_capture_requested_at =
            std::time::Instant::now() - std::time::Duration::from_secs(30);

        // A long idle stretch (no fresh mouse events) must not expire
        // verification — a working emulator doesn't stop reporting (#949).
        model.tick_mouse_capture();

        assert!(model.mouse_input_verified());
        let rendered = rendered_model(&mut model);
        assert!(!rendered.contains("mouse ?"));
    }

    #[test]
    fn focus_regain_does_not_flash_waiting_notice() {
        // The precise #949 repro: capture on, terminal focused, mouse not
        // yet re-verified — the exact state the old `host_focus_gained`
        // flashed "mouse: waiting for host reporting" on. Refocus must be
        // silent.
        let (mut model, _server, _opened) = build_model(1);
        model.host_mouse_verified = false;
        model.status.notice = None;

        model.host_focus_gained();

        assert!(
            model.status.notice.is_none(),
            "refocus must not flash a 'waiting for host reporting' notice (#949)"
        );
    }

    #[test]
    fn focus_regain_re_arms_verification_then_re_verifies_silently() {
        // Finding #1: verification must NOT be sticky-forever. A focus
        // regain (display sleep/wake, tmux re-attach) re-arms it so a
        // genuinely broken emulator re-surfaces the hint — but silently,
        // and the next mouse event re-verifies on a working emulator.
        let (mut model, _server, _opened) = build_model(1);
        model.host_mouse_verified = true;
        model.status.notice = None;

        model.host_focus_gained();
        assert!(
            !model.mouse_input_verified(),
            "focus regain must re-arm verification (not stay sticky-true)"
        );
        assert!(model.status.notice.is_none(), "re-arm must be silent");

        model.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 80,
            row: 20,
            modifiers: KeyModifiers::empty(),
        });
        assert!(
            model.mouse_input_verified(),
            "the next mouse event must re-verify on a working emulator"
        );
        assert!(
            model.status.notice.is_none(),
            "silent re-verification must not flash a notice"
        );
    }

    #[test]
    fn terminal_click_opens_url_after_focus_regain_reset() {
        // Acceptance #5: a click on a URL cell opens even when the
        // verification flag was reset — here by a real focus regain, the
        // path that clears it. The click is ungated, so it opens on the
        // first try and self-verifies.
        let (mut model, _server, opened) = build_model(1);
        model
            .terminals
            .set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        render(&mut model);
        let url = "https://reset-flag.example.com/path";
        feed(&mut model, 1, format!("see {url}\r\n").into_bytes());
        let pane = render(&mut model);
        let (x, y) = body_origin(&model, pane, 1);

        model.host_mouse_verified = true;
        model.host_focus_gained();
        assert!(
            !model.mouse_input_verified(),
            "sanity: focus regain reset the verification flag"
        );

        modifier_left_click(&mut model, x + 4, y, KeyModifiers::ALT);

        assert_eq!(opened_urls(&opened), vec![url]);
        assert!(
            model.mouse_input_verified(),
            "the arriving click must itself verify host mouse reporting"
        );
    }

    #[test]
    fn keyboard_url_picker_still_opens_when_mouse_capture_is_off() {
        let (mut model, _server, opened) = build_model(1);
        model.mouse_capture_on = false;
        let url = "https://keyboard-fallback.example.com";
        feed(&mut model, 1, format!("{url}\r\n").into_bytes());

        model.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        model.dispatch_key(RealmKey::new(Key::Char(']'), RealmMods::NONE));
        model.dispatch_key(RealmKey::new(Key::Char('u'), RealmMods::NONE));

        assert_eq!(opened_urls(&opened), vec![url]);
    }
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
    use super::super::{ActionConfirmTarget, ChoicePayload, Id, ModalFlow, Model};
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
        m.modal_flow = Some(ModalFlow::SidebarContext {
            session_key: sk.clone(),
            actions: vec![Action::MergePr, Action::Archive],
        });
        m.modal_stack.push(Id::SidebarContext);

        // Row 1 is Archive in the stashed action list.
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Index(1)]);
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
        m.modal_flow = Some(ModalFlow::SidebarContext {
            session_key: sk.clone(),
            actions: vec![Action::MergePr, Action::Archive],
        });
        m.modal_stack.push(Id::SidebarContext);

        // Row 0 is MergePr in the stashed action list.
        let cmds = m.handle_choice_picked(vec![ChoicePayload::Index(0)]);
        assert!(
            cmds.is_empty(),
            "MergePr picked from the context menu must not emit MergePr directly: {cmds:?}",
        );
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));
        match &m.modal_flow {
            Some(ModalFlow::ActionConfirm {
                action: Action::MergePr,
                targets,
            }) => assert_eq!(
                targets.as_slice(),
                [ActionConfirmTarget::Workspace(sk.clone())]
            ),
            other => panic!("expected a stashed MergePr aimed at the menu's row, got {other:?}"),
        }
    }

    #[test]
    fn close_issue_gates_on_confirm_then_fires_close_command() {
        // Issue #270: `x c` must route through the confirm modal
        // (nothing closed without a yes), and Yes emits a single
        // `CloseIssue` aimed at the focused workspace.
        let mut m = build_model();
        let ws = open_issue_workspace("github:o/r#7");
        let wk = ws.key.clone();
        let sk = SessionKey::from(&wk);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk), "issue row focusable");

        let cmds = m.dispatch_action(&Action::CloseIssue);
        assert!(
            cmds.is_empty(),
            "close must gate on confirm first: {cmds:?}"
        );
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        let cmds = m.handle_confirmed(true);
        match cmds.as_slice() {
            [IpcCommand::CloseIssue { workspace_key }] => assert_eq!(workspace_key, &wk),
            other => panic!("expected a single CloseIssue command, got {other:?}"),
        }
    }

    #[test]
    fn close_issue_confirm_noops_when_issue_closed_under_the_modal() {
        // The confirmed dispatch re-checks the stashed workspace: if a
        // poll closed the issue while the modal was up, Yes must NOT
        // fire a redundant close — it flashes and emits nothing.
        let mut m = build_model();
        let ws = open_issue_workspace("github:o/r#7");
        let wk = ws.key.clone();
        let sk = SessionKey::from(&wk);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        let _ = m.dispatch_action(&Action::CloseIssue);
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        // The issue closes upstream while the modal is up.
        let mut closed = open_issue_workspace("github:o/r#7");
        if let Some(issue) = closed.gh_issues.first_mut() {
            issue.state = lazybox_core::TaskState::Closed;
        }
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(closed)));

        let cmds = m.handle_confirmed(true);
        assert!(
            cmds.is_empty(),
            "a closed-under-modal issue must not re-fire a close: {cmds:?}",
        );
    }

    #[test]
    fn delete_or_close_on_issue_gates_on_confirm_then_fires_command() {
        // Issue #408: `g d` on an issue workspace routes through the
        // confirm modal (nothing deleted without a yes); Yes emits a
        // single `DeleteOrClose` aimed at the focused workspace.
        let mut m = build_model();
        let ws = open_issue_workspace("github:o/r#7");
        let wk = ws.key.clone();
        let sk = SessionKey::from(&wk);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk), "issue row focusable");

        let cmds = m.dispatch_action(&Action::DeleteOrClose);
        assert!(
            cmds.is_empty(),
            "delete must gate on confirm first: {cmds:?}"
        );
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        let cmds = m.handle_confirmed(true);
        match cmds.as_slice() {
            [IpcCommand::DeleteOrClose { workspace_key }] => assert_eq!(workspace_key, &wk),
            other => panic!("expected a single DeleteOrClose command, got {other:?}"),
        }
    }

    #[test]
    fn delete_or_close_on_pr_gates_on_confirm_then_fires_command() {
        // Issue #408: the same `g d` on a PR workspace resolves to a
        // PR close — still confirm-gated, same command.
        let mut m = build_model();
        let pr = merge_ready_pr_without_approval("github:owner/repo#1");
        let wk = pr.key.clone();
        let sk = SessionKey::from(&wk);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk), "PR row focusable");

        let cmds = m.dispatch_action(&Action::DeleteOrClose);
        assert!(cmds.is_empty(), "close must gate on confirm: {cmds:?}");
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        let cmds = m.handle_confirmed(true);
        match cmds.as_slice() {
            [IpcCommand::DeleteOrClose { workspace_key }] => assert_eq!(workspace_key, &wk),
            other => panic!("expected a single DeleteOrClose command, got {other:?}"),
        }
    }

    #[test]
    fn delete_or_close_confirm_noops_when_pr_merged_under_the_modal() {
        // The confirmed dispatch re-checks the stashed workspace: if a
        // poll merged the PR while the modal was up, Yes must NOT fire
        // a redundant close — it flashes and emits nothing.
        let mut m = build_model();
        let pr = merge_ready_pr_without_approval("github:owner/repo#1");
        let sk = SessionKey::from(&pr.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        let _ = m.dispatch_action(&Action::DeleteOrClose);
        assert_eq!(m.modal_stack.last(), Some(&Id::ActionConfirm));

        // The PR merges upstream while the modal is up.
        let mut merged = merge_ready_pr_without_approval("github:owner/repo#1");
        if let Some(pr) = merged.pr.as_mut() {
            pr.state = lazybox_core::TaskState::Merged;
        }
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(merged)));

        let cmds = m.handle_confirmed(true);
        assert!(
            cmds.is_empty(),
            "a merged-under-modal PR must not fire a close: {cmds:?}",
        );
    }

    /// An open GitHub issue workspace (no PR) — the only shape the
    /// close action is offered on. Built by reshaping a PR fixture into
    /// an issue (matching `intent.rs`'s test helper).
    fn open_issue_workspace(key: &str) -> Workspace {
        let mut ws = merge_ready_pr_without_approval(key);
        let mut issue = ws.pr.take().expect("fixture has a PR to reshape");
        let num = key.rsplit_once('#').map(|(_, n)| n).unwrap_or("1");
        issue.url = format!("https://github.com/o/r/issues/{num}");
        ws.attach_task(issue);
        ws
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

    #[test]
    fn sync_workspace_targets_the_focused_workspace() {
        // `g s` on a focused PR/issue re-polls just that entity — the
        // command carries the focused workspace's own key, no confirm
        // gate.
        let mut m = build_model();
        let pr = merge_ready_pr_without_approval("github:owner/repo#1");
        let wk = pr.key.clone();
        let sk = SessionKey::from(&wk);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk), "PR row focusable");

        let cmds = m.dispatch_action(&Action::SyncWorkspace);
        match cmds.as_slice() {
            [IpcCommand::SyncWorkspace { workspace_key }] => assert_eq!(workspace_key, &wk),
            other => panic!("expected a single SyncWorkspace command, got {other:?}"),
        }
    }

    /// A PR workspace GitHub would let you merge right now — CI green,
    /// no conflict — but with NO approving review, the case #144 was
    /// falsely blocking.
    fn merge_ready_pr_without_approval(key: &str) -> Workspace {
        let num = key.rsplit_once('#').map(|(_, n)| n).unwrap_or("1");
        let task = Task {
            author: String::new(),
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
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
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
    use super::super::{Id, ModalFlow, Model};
    use crate::realm::ChoicePayload;
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
        m.modal_flow = Some(ModalFlow::Snooze {
            workspace: SessionKey::from("github:o/r#1"),
        });
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
        let _ = m.handle_choice_picked(vec![ChoicePayload::Duration(
            std::time::Duration::from_secs(3600),
        )]);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::RemoveOutOfScope),
            "queued removal prompt must mount once the picker resolves",
        );
        assert!(matches!(
            m.modal_flow,
            Some(ModalFlow::RemovalPrompt { .. })
        ));
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
        m.modal_flow = Some(ModalFlow::Reply {
            target: SessionKey::from("github:o/r#1"),
        });
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
    //! Issue #78: joining an Issue into a PR (`x j`) must not drop
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        // User is on the issue, with Claude running and on screen.
        assert!(m.sidebar.focus_workspace_key(&issue_sk), "focus issue row");
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: issue_sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        // Claude on the issue is blocked on a prompt.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: issue_sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
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
    fn leave_terminal_tip_uses_the_configured_escape_char() {
        let mut m = armed_model();
        m.ui_defaults.terminal_escape_char = '}';
        m.focus = PaneFocus::Terminals;

        let tip = m.pick_tip().expect("the in-terminal tip is eligible");

        assert!(tip.message.contains("}}q"), "{}", tip.message);
        assert!(!tip.message.contains("]]q"), "{}", tip.message);
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
    //! showing (#162), with `Shift-P` cycling full → summary → hidden
    //! → full on demand (#487).
    use super::super::{Model, PaneFocus};
    use chrono::Utc;
    use lazybox_config::ActivityPaneMode;
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
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
    fn shift_p_reveals_an_empty_pane_then_cycles() {
        let mut m = build_model();
        seed(&mut m, vec![empty_ws("github:o/r#1")]);
        assert_eq!(
            m.activity_pane_mode(),
            ActivityPaneMode::Hidden,
            "auto-hidden when empty"
        );

        // From Hidden the cycle wraps to Full, then Summary, then back.
        m.dispatch_key(shift_p());
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Full);
        m.dispatch_key(shift_p());
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Summary);
        m.dispatch_key(shift_p());
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Hidden);
    }

    #[test]
    fn shift_p_cycles_a_non_empty_pane_full_summary_hidden() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        assert_eq!(
            m.activity_pane_mode(),
            ActivityPaneMode::Full,
            "content present → starts full"
        );
        assert!(m.activity_pane_visible());

        m.dispatch_key(shift_p());
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Summary);
        assert!(
            !m.activity_pane_visible(),
            "the slim summary line is not the focusable full pane"
        );

        m.dispatch_key(shift_p());
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Hidden);

        m.dispatch_key(shift_p());
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Full, "wraps back");
    }

    fn ws_with_n_activities(key: &str, n: usize) -> Workspace {
        let mut w = empty_ws(key);
        for i in 0..n {
            w.activity.push(lazybox_core::Activity {
                author: format!("u{i}"),
                body: format!("comment {i}"),
                created_at: Utc::now(),
                kind: lazybox_core::ActivityKind::Comment,
                node_id: Some(format!("n-{i}")),
                path: None,
                line: None,
                diff_hunk: None,
                thread_id: None,
            });
        }
        w
    }

    #[test]
    fn clicking_the_summary_line_expands_to_full() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use tuirealm::ratatui::layout::Rect;

        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        // Full → Summary.
        m.dispatch_key(shift_p());
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Summary);

        // The slim summary line sits at the top of the right column.
        let area = Rect::new(0, 0, 120, 40);
        let (_, right_top, _) = m.effective_pane_rects(area);
        assert_eq!(right_top.height, 1, "summary keeps a single row");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: right_top.x + 2,
            row: right_top.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        m.dispatch_mouse_in(click, area);
        assert_eq!(
            m.activity_pane_mode(),
            ActivityPaneMode::Full,
            "clicking the summary line restores the full feed",
        );
        assert_eq!(m.focus(), PaneFocus::Right);
    }

    #[test]
    fn summary_seam_does_not_start_a_splitter_drag() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use tuirealm::ratatui::layout::Rect;

        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        m.dispatch_key(shift_p()); // Full → Summary
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Summary);

        let area = Rect::new(0, 0, 120, 40);
        let (_, right_top, _) = m.effective_pane_rects(area);
        // The seam sits at the terminal's first row (just below the
        // 1-row summary). In Full mode this is a draggable splitter;
        // in Summary it must not be.
        let seam = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: right_top.x + 5,
            row: right_top.y + 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        m.dispatch_mouse_in(seam, area);
        assert!(
            m.layout.active_drag.is_none(),
            "the summary / terminal seam must not arm a horizontal splitter drag",
        );
    }

    #[test]
    fn scrolling_the_summary_line_does_not_move_the_hidden_feed() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        use tuirealm::ratatui::layout::Rect;

        let mut m = build_model();
        // A long feed so a downward wheel WOULD scroll if it were routed
        // to the activity pane.
        seed(&mut m, vec![ws_with_n_activities("github:o/r#1", 60)]);
        m.dispatch_key(shift_p()); // Full → Summary
        assert_eq!(m.activity_pane_mode(), ActivityPaneMode::Summary);

        let area = Rect::new(0, 0, 120, 40);
        let (_, right_top, _) = m.effective_pane_rects(area);
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: right_top.x + 2,
            row: right_top.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        m.redraw = false;
        m.dispatch_mouse_in(wheel, area);
        assert!(
            !m.redraw,
            "a wheel over the slim summary line must be a no-op, not scroll the hidden feed",
        );
    }

    #[test]
    fn activity_pane_default_sets_the_initial_mode() {
        let mut m = build_model();
        m.ui_defaults.activity_pane_default = ActivityPaneMode::Summary;
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        assert_eq!(
            m.activity_pane_mode(),
            ActivityPaneMode::Summary,
            "un-toggled workspace opens in the configured default",
        );

        // The empty-workspace auto-hide still wins over the default.
        seed(&mut m, vec![empty_ws("github:o/r#2")]);
        let second: lazybox_core::SessionKey = (&WorkspaceKey::new("github:o/r#2")).into();
        assert!(m.sidebar.focus_workspace_key(&second));
        m.sync_panes();
        assert_eq!(
            m.activity_pane_mode(),
            ActivityPaneMode::Hidden,
            "nothing to summarize → auto-hidden regardless of default",
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

    // ── Directional pane focus (#492) ──────────────────────────────

    #[test]
    fn sidebar_right_arrow_focuses_the_activity_pane() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        m.sync_panes();
        assert_eq!(m.focus(), PaneFocus::Sidebar);
        m.dispatch_key(KeyEvent::new(Key::Right, KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Right,
            "Right from the sidebar steps focus into the activity pane",
        );
    }

    #[test]
    fn sidebar_vim_l_focuses_the_activity_pane() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        m.sync_panes();
        m.dispatch_key(KeyEvent::new(Key::Char('l'), KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Right,
            "vim `l` mirrors the Right arrow from the sidebar",
        );
    }

    #[test]
    fn sidebar_right_skips_a_hidden_activity_pane() {
        let mut m = build_model();
        seed(&mut m, vec![empty_ws("github:o/r#1")]);
        m.sync_panes();
        assert!(!m.activity_pane_visible());
        m.dispatch_key(KeyEvent::new(Key::Right, KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Terminals,
            "Right skips straight to the terminal when the activity pane is hidden",
        );
    }

    #[test]
    fn activity_left_with_nothing_expanded_returns_to_the_sidebar() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        m.sync_panes();
        m.focus = PaneFocus::Right;
        m.set_focus_attr();
        m.dispatch_key(KeyEvent::new(Key::Left, KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "Left with no expanded row steps focus back to the sidebar",
        );
    }

    #[test]
    fn activity_left_collapses_an_expanded_row_before_leaving() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        m.sync_panes();
        m.focus = PaneFocus::Right;
        m.set_focus_attr();
        // Right expands the focused row (pane-local meaning); the first
        // Left then collapses it and stays put, only the second Left
        // leaves — so collapse is never clobbered by focus movement.
        m.dispatch_key(KeyEvent::new(Key::Right, KeyModifiers::NONE));
        m.dispatch_key(KeyEvent::new(Key::Left, KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Right,
            "the first Left collapses the expanded row and keeps focus",
        );
        m.dispatch_key(KeyEvent::new(Key::Left, KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "a second Left, with nothing left to collapse, returns to the sidebar",
        );
    }

    #[test]
    fn activity_vim_h_returns_to_the_sidebar() {
        let mut m = build_model();
        seed(&mut m, vec![ws_with_activity("github:o/r#1")]);
        m.sync_panes();
        m.focus = PaneFocus::Right;
        m.set_focus_attr();
        m.dispatch_key(KeyEvent::new(Key::Char('h'), KeyModifiers::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "vim `h` mirrors the Left arrow from the activity pane",
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
            model_label: None,
            terminal_id: TerminalId(id),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
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

    /// Regression for #441: a single click on a sidebar workspace only
    /// selects it, while a double-click drops focus straight into its
    /// live agent terminal — no extra keystrokes to reach the running
    /// session. A workspace with no live terminal degrades gracefully
    /// to the plain selection.
    #[test]
    fn double_click_enters_the_workspace_agent_terminal() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1"), empty_ws("github:o/r#2")],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let a = key_of("github:o/r#1");
        let b = key_of("github:o/r#2");
        // WS-A has a live agent terminal; WS-B has none.
        spawn_terminal(&mut m, &a, 1);

        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _, _) = m.effective_pane_rects(area);
        let row_a = row_of(&mut m, sidebar_rect, &a);
        let row_b = row_of(&mut m, sidebar_rect, &b);

        // Single click on WS-A: selects the row, stays on the sidebar.
        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_a), area);
        assert_eq!(m.sidebar().selected_workspace_key(), Some(&a));
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "a single click only selects the workspace",
        );

        // Second click at the same spot (within the double-click
        // window) enters WS-A's live agent terminal.
        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_a), area);
        assert_eq!(m.sidebar().selected_workspace_key(), Some(&a));
        assert_eq!(
            m.focus(),
            PaneFocus::Terminals,
            "a double click jumps into the agent terminal",
        );
        assert_eq!(m.terminals.active_terminal_id(), Some(TerminalId(1)));

        // Double-click WS-B, which has no live session: it degrades to
        // a plain selection rather than stranding focus in an empty
        // terminal pane.
        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_b), area);
        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_b), area);
        assert_eq!(m.sidebar().selected_workspace_key(), Some(&b));
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "double-clicking a session-less workspace just selects it",
        );
    }

    /// Companion to #441: double-clicking a workspace with no live
    /// terminal but a visible activity pane falls back to opening that
    /// pane rather than stranding focus in an empty terminal slot.
    #[test]
    fn double_click_without_a_terminal_opens_the_activity_pane() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers as TuiMods};

        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1")],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let a = key_of("github:o/r#1");
        // No terminal spawned; force the (otherwise auto-hidden)
        // activity pane visible so the fallback has somewhere to land.
        m.dispatch_key(KeyEvent::new(Key::Char('P'), TuiMods::SHIFT));
        assert!(m.activity_pane_visible());

        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _, _) = m.effective_pane_rects(area);
        let row_a = row_of(&mut m, sidebar_rect, &a);

        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_a), area);
        m.dispatch_mouse_in(left_down(sidebar_rect.x + 1, row_a), area);
        assert_eq!(
            m.focus(),
            PaneFocus::Right,
            "with no terminal the double-click opens the activity pane",
        );
    }

    /// Regression for #268: a wheel event over the sidebar scrolls the
    /// list instead of being swallowed. Before the fix the
    /// `ScrollUp/ScrollDown` router only handled the activity pane and
    /// the terminal, so the wheel did nothing over the sidebar even
    /// though the scrollbar showed the list overflowing.
    ///
    /// The wheel moves the viewport offset only (#290): the render's
    /// keep-cursor-visible clamp is skipped while wheel-detached, so we
    /// assert on the settled scroll offset after a render — proving the
    /// visible list actually moved — and that the cursor/selection
    /// stayed put.
    #[test]
    fn wheel_over_the_sidebar_scrolls_the_list() {
        use tuirealm::ratatui::{Terminal, backend::TestBackend};

        let mut m = build_model();
        // Seed far more workspaces than the viewport can show so the
        // list overflows and the viewport has room to travel.
        let workspaces: Vec<Workspace> = (1..=60)
            .map(|n| empty_ws(&format!("github:o/r#{n}")))
            .collect();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces,
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _, _) = m.effective_pane_rects(area);
        let wheel = |kind| MouseEvent {
            kind,
            column: sidebar_rect.x + sidebar_rect.width / 2,
            row: sidebar_rect.y + 10,
            modifiers: KeyModifiers::NONE,
        };
        // Render the sidebar into `sidebar_rect`; `render` recomputes the
        // scroll offset to keep the cursor on-screen, which is the value
        // the scrollbar thumb tracks.
        let render = |m: &mut Model<tuirealm::terminal::TestTerminalAdapter>| {
            let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            term.draw(|f| m.__test_sidebar_mut().view_in(sidebar_rect, f))
                .unwrap();
        };

        render(&mut m);
        assert_eq!(m.sidebar().__test_scroll(), 0, "list starts at the top");

        // Sustained wheel-down: the scroll offset (and the scrollbar
        // thumb) moves down; the cursor and selection stay exactly
        // where they were.
        let before_cursor = m.sidebar().cursor();
        let before_selected = m.sidebar().selected_workspace_key().cloned();
        m.redraw = false;
        for _ in 0..20 {
            m.dispatch_mouse_in(wheel(MouseEventKind::ScrollDown), area);
        }
        render(&mut m);
        assert_eq!(
            m.sidebar().cursor(),
            before_cursor,
            "wheel-down over the sidebar must not move the cursor",
        );
        assert_eq!(
            m.sidebar().selected_workspace_key().cloned(),
            before_selected,
            "wheel-down over the sidebar must not change the selection",
        );
        assert!(
            m.sidebar().__test_scroll() > 0,
            "sustained wheel-down must scroll the list off the top: scroll={}",
            m.sidebar().__test_scroll(),
        );
        assert!(m.redraw, "scrolling the sidebar repaints");

        // Wheel back up past the top: the offset clamps to zero (it
        // never scrolls above the first row).
        for _ in 0..40 {
            m.dispatch_mouse_in(wheel(MouseEventKind::ScrollUp), area);
        }
        render(&mut m);
        assert_eq!(
            m.sidebar().__test_scroll(),
            0,
            "wheel-up must return the list to the top",
        );
    }

    /// Any key pressed while the sidebar is focused re-anchors a
    /// wheel-detached viewport (#290). The wheel may leave the cursor
    /// off-screen, but keys act on the selection — `m` marks IT read,
    /// `z` snoozes IT — so the frame after a keypress must show it.
    #[test]
    fn sidebar_key_reanchors_a_wheel_detached_viewport() {
        use tuirealm::event::{Key, KeyEvent, KeyModifiers as RealmMods};
        use tuirealm::ratatui::{Terminal, backend::TestBackend};

        let mut m = build_model();
        let workspaces: Vec<Workspace> = (1..=60)
            .map(|n| empty_ws(&format!("github:o/r#{n}")))
            .collect();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces,
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _, _) = m.effective_pane_rects(area);
        let render = |m: &mut Model<tuirealm::terminal::TestTerminalAdapter>| {
            let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            term.draw(|f| m.__test_sidebar_mut().view_in(sidebar_rect, f))
                .unwrap();
        };

        render(&mut m);
        for _ in 0..20 {
            m.dispatch_mouse_in(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: sidebar_rect.x + sidebar_rect.width / 2,
                    row: sidebar_rect.y + 10,
                    modifiers: KeyModifiers::NONE,
                },
                area,
            );
        }
        render(&mut m);
        assert!(
            m.sidebar().__test_scroll() > m.sidebar().cursor(),
            "wheel-down leaves the cursor off-screen above the viewport",
        );

        // `m` targets the selection without moving the cursor — the
        // re-anchor must come from the keypress itself, not from a
        // cursor move.
        m.dispatch_key(KeyEvent::new(Key::Char('m'), RealmMods::NONE));
        render(&mut m);
        assert!(
            m.sidebar().__test_scroll() <= m.sidebar().cursor(),
            "a sidebar keypress snaps the viewport back onto the cursor",
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
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
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
            author: String::new(),
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
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
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
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
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

    /// The snippet picker is provider-scoped by the focused workspace's
    /// task sources (#868). `scoped_picker_rows` is exactly what
    /// `mount_snippet_picker` feeds the picker, so this exercises the real
    /// filter: with no focused terminal every snippet shows; on a GitHub
    /// workspace the Linear snippets drop out; and on a workspace that
    /// spans both providers (a Linear issue with a GitHub PR) both sets
    /// surface — the regression finding #1 guards against.
    #[test]
    fn snippet_picker_is_provider_scoped_by_workspace_sources() {
        let keys = |m: &Model<tuirealm::terminal::TestTerminalAdapter>| -> Vec<String> {
            m.scoped_picker_rows().into_iter().map(|r| r.key).collect()
        };

        let mut m = build_model();
        m.apply_snippets(lazybox_config::Snippets::builtin());

        // No focused terminal → unknown sources → every snippet shows.
        assert!(m.active_workspace_sources().is_empty());
        let unfocused = keys(&m);
        assert!(unfocused.iter().any(|k| k == "triage"), "github shows");
        assert!(unfocused.iter().any(|k| k == "wip"), "linear shows");
        assert!(unfocused.iter().any(|k| k == "rev"), "generic shows");

        // Focus a GitHub workspace → github + generic show, linear drops.
        let ws = workspace_with_agent("owner/repo#1");
        let key = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        spawn_terminal(&mut m, &key);
        assert_eq!(m.active_workspace_sources(), vec!["github".to_string()]);
        let scoped = keys(&m);
        assert!(scoped.iter().any(|k| k == "triage"), "github stays");
        assert!(scoped.iter().any(|k| k == "rev"), "generic stays");
        assert!(
            !scoped.iter().any(|k| k == "wip"),
            "linear snippet is scoped out on a github workspace",
        );

        // Focus a cross-provider workspace (Linear issue + GitHub PR) →
        // both providers' snippets surface (finding #1).
        let mut cross = workspace_with_agent("owner/repo#2");
        let mut linear = cross.primary_task().expect("has a task").clone();
        linear.id.source = "linear".into();
        cross.linear_issues.push(linear);
        let cross_key = SessionKey::from(&cross.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(cross)));
        spawn_terminal(&mut m, &cross_key);
        assert_eq!(
            m.active_workspace_sources(),
            vec!["github".to_string(), "linear".to_string()],
        );
        let both = keys(&m);
        assert!(
            both.iter().any(|k| k == "triage"),
            "github on cross-provider"
        );
        assert!(both.iter().any(|k| k == "wip"), "linear on cross-provider");
    }

    /// `]]q` exits the terminal to the sidebar — and in focus mode that
    /// must also drop focus mode, since the sidebar it returns to is
    /// hidden while focus mode is on (#252, replacing the old idle-tick
    /// leave).
    #[test]
    fn leader_q_exits_focus_mode() {
        let mut m = build_model();
        let ws = workspace_with_agent("owner/repo#1");
        let key = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        spawn_terminal(&mut m, &key);
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m.focus_mode = true;

        // `]]` arms the non-timed leader; `q` is the exit command.
        m.dispatch_key(char_key(']'));
        m.dispatch_key(char_key(']'));
        assert!(m.terminal_leader_pending(), "`]]` arms the leader");
        m.dispatch_key(char_key('q'));
        assert!(!m.focus_mode, "`]]q` exits focus mode");
        assert_eq!(m.focus(), PaneFocus::Sidebar);
    }

    /// Once the snippet picker is mounted, nothing in the daemon-event
    /// stream may auto-close it (#252): a flood of PTY output, an agent
    /// state change, a spawn that steals pane focus, and the idle tick
    /// all fire, and the picker stays on top the whole time. This is the
    /// "flash for ~0.1s and vanish" the issue is about — proven immune.
    #[test]
    fn snippet_picker_survives_daemon_output_and_focus_steal() {
        let mut m = build_model();
        let ws = workspace_with_agent("owner/repo#1");
        let key = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        spawn_terminal(&mut m, &key);
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        m.apply_snippets(lazybox_config::Snippets::builtin());

        m.mount_snippet_picker("r".to_string());
        assert!(
            matches!(m.top_modal(), Some(Id::SnippetPicker)),
            "picker up"
        );

        // Agent spews output, changes state, a spawn lands (which steals
        // focus to the terminal), and the run loop keeps ticking.
        for seq in 0..20 {
            m.handle_daemon_event(IpcEvent::TerminalOutput {
                terminal_id: TerminalId(1),
                bytes: b"codex spinner churn...\r\n".to_vec(),
                first_seq: seq,
                seq,
            });
            m.tick_terminal_leader();
        }
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        m.tick_terminal_leader();

        assert!(
            matches!(m.top_modal(), Some(Id::SnippetPicker)),
            "no daemon event or tick may close the picker",
        );
    }

    /// `]]<digit>` moves the displayed terminal to the Nth **focused**
    /// (starred) workspace in sidebar order and keeps focus mode on, so
    /// the user hops to a curated workspace heads-down.
    #[test]
    fn bracket_digit_jumps_to_focused_workspace_in_focus_mode() {
        let mut m = build_model();
        let ws1 = workspace_with_agent("owner/repo#1");
        let ws2 = workspace_with_agent("owner/repo#2");
        let key1 = SessionKey::from(&ws1.key);
        let key2 = SessionKey::from(&ws2.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws1)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws2)));

        // Only focused workspaces are numbered now — star both so they
        // enter the roster.
        assert!(m.sidebar.focus_workspace_key(&key1));
        m.sidebar.toggle_focus_at_cursor();
        assert!(m.sidebar.focus_workspace_key(&key2));
        m.sidebar.toggle_focus_at_cursor();

        // The jump number is the slot in this roster (sidebar order),
        // which the badge mirrors — read it rather than assume an order.
        let roster = m.sidebar.numbered_workspace_keys();
        assert_eq!(roster.len(), 2, "both focused workspaces in the roster");
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
            "`]]1` jumps to the first focused workspace in the roster",
        );
    }

    /// An unfocused workspace gets no jump number, and `]]<digit>`
    /// past the focused count flashes instead of moving.
    #[test]
    fn unfocused_workspaces_are_not_numbered() {
        let mut m = build_model();
        let ws1 = workspace_with_agent("owner/repo#1");
        let ws2 = workspace_with_agent("owner/repo#2");
        let key1 = SessionKey::from(&ws1.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws1)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws2)));

        // Nothing starred → nothing numbered.
        assert!(
            m.sidebar.numbered_workspace_keys().is_empty(),
            "no focused workspace, so no jump numbers"
        );

        // Star just one → exactly one slot, in first position.
        assert!(m.sidebar.focus_workspace_key(&key1));
        m.sidebar.toggle_focus_at_cursor();
        assert_eq!(m.sidebar.numbered_workspace_keys(), vec![key1.clone()]);
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
            author: String::new(),
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
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
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
    }

    /// The whole point of #171: the switcher is reachable from inside an
    /// `set_focus` is the single owned focus mutator: it assigns `focus`
    /// AND fans the change out (via `set_focus_attr`, which also resets the
    /// typed-since-focus flag). Routing every focus change through it makes
    /// the "assigned focus but forgot to fan out" desync unrepresentable —
    /// here the flag reset proves the fan-out ran as part of the assignment.
    #[test]
    fn set_focus_assigns_and_fans_out_in_one_step() {
        let mut m = build_model();
        m.terminal_user_typed_since_focus = true;
        m.set_focus(PaneFocus::Terminals);
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert!(
            !m.terminal_user_typed_since_focus,
            "set_focus must fan out (the flag reset lives in set_focus_attr)",
        );
    }

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
        m.handle_choice_picked(vec![ChoicePayload::Session(bk.clone())]);
        assert!(m.top_modal().is_none(), "modal popped after the pick");
        assert_eq!(m.sidebar.selected_workspace_key(), Some(&bk));
    }

    #[test]
    fn notification_focus_request_jumps_to_its_workspace() {
        let mut m = build_model();
        let (ak, bk) = seed_two(&mut m);
        let mut hidden = Workspace::from_task(task("owner/repo#2", Duration::hours(1)), Utc::now());
        hidden.snoozed_until = Some(Utc::now() + Duration::hours(1));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(hidden)));
        assert!(
            !m.sidebar.focus_workspace_key(&bk),
            "target starts outside the Inbox"
        );
        assert!(m.sidebar.focus_workspace_key(&ak));
        m.sync_panes();

        m.handle_daemon_event(IpcEvent::WorkspaceFocusRequested {
            session_key: bk.clone(),
        });

        assert_eq!(m.sidebar.selected_workspace_key(), Some(&bk));
        assert_eq!(m.focus(), PaneFocus::Sidebar);
    }

    /// With nothing tracked the picker refuses to mount (a footer hint
    /// fires instead) — no empty modal.
    #[test]
    fn no_workspaces_does_not_mount() {
        let mut m = build_model();
        m.mount_jump_picker();
        assert!(m.top_modal().is_none());
    }
}

#[cfg(test)]
mod terminal_section_dispatch_tests {
    //! #188: the terminal-pane actions must actually fire under terminal
    //! focus — `available_in_terminal()` is only a `section == Terminal`
    //! proxy, so these round-trip each `Section::Terminal` action through
    //! `handle_pane_key` under `PaneFocus::Terminals` to prove the
    //! proxy's premise. They also pin the central #188 finding: the leave
    //! chord is owned by `terminal.escape_char`, NOT a remappable
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
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: key,
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
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

    /// `]]q` leaves even with the `leave_terminal` override present —
    /// proving `terminal.escape_char` (not the action_keys slot) owns
    /// the chord. `]]` arms the non-timed leader and `q` is its exit
    /// command (#252).
    #[test]
    fn leader_q_leaves_regardless_of_override() {
        let mut m = model_in_live_terminal();
        let mut ov = std::collections::BTreeMap::new();
        ov.insert("leave_terminal".to_string(), "Esc".to_string());
        m.apply_action_key_overrides(ov);

        m.dispatch_key(esc_char(&m));
        m.dispatch_key(esc_char(&m));
        assert!(
            m.terminal_leader_pending(),
            "the escape char doubled arms the leader"
        );
        m.dispatch_key(RealmKey::new(Key::Char('q'), RealmMods::NONE));
        assert_eq!(
            m.focus(),
            PaneFocus::Sidebar,
            "`]]q` is the way out, override or not",
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
            model_label: None,
            terminal_id: TerminalId(3),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
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
            model_label: None,
            terminal_id: TerminalId(5),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
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
            model_label: None,
            terminal_id: TerminalId(8),
            session_key: SessionKey::new("github:o/r#2"),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        assert!(
            m.status.spawning.is_some(),
            "an unrelated spawn must not clear our spinner",
        );

        // Our target's terminal lands → cleared.
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(9),
            session_key: target,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        assert!(
            m.status.spawning.is_none(),
            "spinner cleared by its own terminal"
        );
    }

    #[test]
    fn recovered_agent_restart_warning_isolated_from_polling_refresh_and_spawns() {
        let mut m = build_model();
        let target = SessionKey::new("github:o/r#1");
        m.status
            .note_spawning("codex", target, TerminalKind::Agent("codex".into()), 0);
        m.show_polling(vec!["github".into()]);

        m.handle_daemon_event(IpcEvent::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(7)],
        });

        assert!(
            m.status.spawning.is_some(),
            "a recovery warning must not cancel an unrelated active spawn"
        );
        assert!(
            m.status.polling.is_some(),
            "terminal compatibility must not terminate first-poll feedback"
        );
        m.status.polling_last_tick =
            std::time::Instant::now() - std::time::Duration::from_millis(100);
        assert!(
            m.polling_tick().is_none(),
            "terminal compatibility must not queue a delayed polling failure"
        );
        assert!(m.status.dismiss_polling());

        m.pending_refresh_ack = true;
        m.handle_daemon_event(IpcEvent::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(7)],
        });
        assert!(
            m.pending_refresh_ack,
            "terminal compatibility must not consume a manual refresh acknowledgement"
        );
        let notice = m.status.notice.as_ref().expect("scrollback notice");
        assert!(notice.message.contains("scrollback limited"));
        assert!(!notice.message.contains("spawn failed"));
        assert!(!notice.message.contains("sync failed"));
        // The old permanent global nag is gone (#544): the notice
        // auto-fades and yields to `Esc` like any transient hiccup.
        assert_eq!(
            notice.severity,
            crate::realm::components::footer::NoticeSeverity::Retryable
        );
        assert!(!notice.severity.is_sticky());
    }

    /// #544: the daemon re-emits `RecoveredTerminalsRequireRestart` on
    /// every reconnect snapshot. Re-flashing the same flagged set was the
    /// permanent-nag behavior; a re-fire for an already-known set must
    /// stay silent so a dismissed notice stays dismissed.
    #[test]
    fn recovered_scroll_warning_does_not_renag_on_reconnect() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(7), TerminalId(8)],
        });
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("scrollback limited")),
            "first flag surfaces a notice"
        );
        // User dismisses it (Esc), then a reconnect re-emits the same set.
        m.status.notice = None;
        m.handle_daemon_event(IpcEvent::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(7), TerminalId(8)],
        });
        assert!(
            m.status.notice.is_none(),
            "a known set must not re-nag after dismissal"
        );
        // A genuinely NEW flagged terminal still earns one fresh notice.
        m.handle_daemon_event(IpcEvent::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(7), TerminalId(8), TerminalId(9)],
        });
        let notice = m
            .status
            .notice
            .as_ref()
            .expect("new flag surfaces a notice");
        assert!(notice.message.contains("scrollback limited"));
        assert_eq!(notice.severity, NoticeSeverity::Retryable);
    }

    /// #544: reopening a flagged session is what heals its scrollback, so
    /// when one exits it must drop out of the tracked set — no manual
    /// dismissal, and no lingering per-terminal hint.
    #[test]
    fn recovered_scroll_warning_auto_clears_when_terminal_exits() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(7)],
        });
        assert!(m.outdated_scroll_terminals.contains(&TerminalId(7)));
        m.handle_daemon_event(IpcEvent::TerminalExited {
            terminal_id: TerminalId(7),
            exit_code: Some(0),
            last_output: None,
        });
        assert!(
            m.outdated_scroll_terminals.is_empty(),
            "an exited terminal clears the flag without a manual dismiss"
        );
    }

    /// #544: focusing a flagged old-build terminal explains the broken
    /// scrollback in context — as a one-shot `Hint`, and only while that
    /// terminal is the focused one.
    #[test]
    fn focusing_outdated_terminal_hints_in_context() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();
        let ws_key = lazybox_core::WorkspaceKey::new("github:o/r#1");
        let session_key: SessionKey = (&ws_key).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![lazybox_core::Workspace::empty(
                ws_key,
                "main",
                chrono::Utc::now(),
            )],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(m.sidebar.focus_workspace_key(&session_key));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        assert_eq!(m.terminals.active_terminal_id(), Some(TerminalId(1)));
        // The single live terminal (id 1) is the recovered old-build one.
        m.handle_daemon_event(IpcEvent::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(1)],
        });
        m.status.notice = None;
        m.set_focus(PaneFocus::Terminals);
        let notice = m.status.notice.as_ref().expect("focus hint");
        assert!(notice.message.contains("scrollback unavailable"));
        assert_eq!(notice.severity, NoticeSeverity::Hint);
        // Re-focusing the same terminal must not re-nag.
        m.status.notice = None;
        m.set_focus(PaneFocus::Sidebar);
        m.set_focus(PaneFocus::Terminals);
        assert!(
            m.status.notice.is_none(),
            "staying on the same flagged terminal must not re-flash the hint"
        );
    }

    /// #989: the `⚠` no-permission tab glyph is compact, so focusing a
    /// bypass-mode terminal spells out its meaning in the footer — a
    /// one-shot `Hint`, once per terminal.
    #[test]
    fn focusing_no_permission_terminal_hints_in_context() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();
        let ws_key = lazybox_core::WorkspaceKey::new("github:o/r#1");
        let session_key: SessionKey = (&ws_key).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![lazybox_core::Workspace::empty(
                ws_key,
                "main",
                chrono::Utc::now(),
            )],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(m.sidebar.focus_workspace_key(&session_key));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: true,
            on_main: false,
        });
        assert_eq!(m.terminals.active_terminal_id(), Some(TerminalId(1)));
        m.status.notice = None;
        m.set_focus(PaneFocus::Terminals);
        let notice = m.status.notice.as_ref().expect("focus hint");
        assert!(notice.message.contains("no-permission mode"));
        assert_eq!(notice.severity, NoticeSeverity::Hint);
        // Re-focusing the same terminal must not re-nag.
        m.status.notice = None;
        m.set_focus(PaneFocus::Sidebar);
        m.set_focus(PaneFocus::Terminals);
        assert!(
            m.status.notice.is_none(),
            "staying on the same bypass terminal must not re-flash the hint"
        );
    }

    /// #989: a `]]`-leader tab switch changes the active terminal without
    /// touching pane focus or the selected workspace, so the hint must
    /// re-fire from the leader-dispatch path — otherwise cycling onto a
    /// bypass tab leaves its compact `⚠` unexplained.
    #[test]
    fn leader_tab_cycle_to_bypass_terminal_hints() {
        use lazybox_core::SessionLayout;
        use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};
        let mut m = build_model();
        let sk = SessionKey::new("github:o/r#1");
        // Two agent tabs in one session: interactive (tab 0), bypass (tab 1).
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: true,
            on_main: false,
        });
        m.terminals.set_active_session(Some(sk));
        m.terminals.set_layout(SessionLayout::Tabs { active: 0 });
        m.terminals.set_active_tab(0);
        m.focus = PaneFocus::Terminals;
        m.set_focus_attr();
        assert_eq!(m.terminals.active_terminal_id(), Some(TerminalId(1)));
        m.status.notice = None;

        // `]]→` cycles to the bypass tab. This routes through the terminal
        // leader, which returns before `sync_panes`/`set_focus` — the paths
        // the other two hint hooks live on.
        m.terminal_leader_armed = true;
        m.dispatch_key(RealmKey::new(Key::Right, RealmMods::NONE));
        assert_eq!(
            m.terminals.active_terminal_id(),
            Some(TerminalId(2)),
            "the leader arrow must cycle to the bypass tab"
        );
        let notice = m.status.notice.as_ref().expect("cycle hint");
        assert!(
            notice.message.contains("no-permission mode"),
            "cycling onto a bypass tab must explain its ⚠; got: {}",
            notice.message
        );
    }

    /// #989: a terminal that is both a recovered old-build tab (#544) and a
    /// bypass spawn must show the functional scrollback warning, not the
    /// informational bypass hint — the two focus hints run back-to-back and
    /// must not clobber each other.
    #[test]
    fn outdated_scroll_hint_takes_precedence_over_no_permission() {
        let mut m = build_model();
        let ws_key = lazybox_core::WorkspaceKey::new("github:o/r#1");
        let session_key: SessionKey = (&ws_key).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![lazybox_core::Workspace::empty(
                ws_key,
                "main",
                chrono::Utc::now(),
            )],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(m.sidebar.focus_workspace_key(&session_key));
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: true,
            on_main: false,
        });
        m.handle_daemon_event(IpcEvent::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(1)],
        });
        m.status.notice = None;
        m.set_focus(PaneFocus::Terminals);
        let notice = m.status.notice.as_ref().expect("focus hint");
        assert!(
            notice.message.contains("scrollback unavailable"),
            "the functional scrollback warning must win over the bypass hint; got: {}",
            notice.message
        );
        assert!(
            !notice.message.contains("no-permission mode"),
            "the bypass hint must not clobber the scrollback warning"
        );
    }
}

#[cfg(test)]
mod worktree_progress_recovery_tests {
    //! Issue #219 / #253 — the "Setting up workspace" checklist hung
    //! forever on "Cloning repository". A broadcast-lag recovery
    //! `Snapshot` stands in for the events the client missed, which can
    //! include both the per-stage `WorktreeProgress` updates AND the
    //! one-shot `TerminalSpawned` that dismisses the modal. With all of
    //! those dropped, the checklist never advanced past its first step
    //! and never closed, even though the spawn had completed.
    //!
    //! The snapshot reconciliation proves the spawn finished (the
    //! session's terminal is live) and *queues* a graceful dismiss —
    //! per #253 it no longer tears the modal down on the spot, so the
    //! checklist still walks its remaining stages for their minimum
    //! dwell instead of flashing a single half-step before vanishing.
    use super::super::{Id, Model};
    use chrono::Utc;
    use lazybox_core::{Workspace, WorkspaceKey};
    use lazybox_ipc::{
        Event as IpcEvent, TerminalId, TerminalKind, TerminalSnapshot, WorktreeStep,
        WorktreeStepStatus, channel,
    };
    use std::time::{Duration, Instant};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn terminal_snapshot(session_key: lazybox_core::SessionKey) -> TerminalSnapshot {
        TerminalSnapshot {
            model_label: None,
            terminal_id: TerminalId(7),
            session_key,
            kind: TerminalKind::Agent("claude".into()),
            replay: Vec::new(),
            last_seq: 0,
            replay_available: true,
            no_permission: false,
            on_main: false,
            prompt_history: Vec::new(),
            composing_buffer: None,
            agent_state: None,
            authenticating: false,
        }
    }

    #[test]
    fn lag_recovery_snapshot_queues_graceful_dismiss_not_an_abrupt_teardown() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:mind-build/mind#1");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key.clone(), "main", Utc::now())],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        // Provisioning starts — the checklist mounts on "Cloning
        // repository". Then the client lags: the fetch/worktree-add/setup
        // updates and the `TerminalSpawned` are all dropped.
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::Clone,
            status: WorktreeStepStatus::Started,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "checklist must be up after the first progress event",
        );

        // The recovery snapshot the daemon sends in place of the missed
        // events shows the spawn finished: the session now has a live
        // terminal. Per #253 this must NOT tear the modal down on the
        // spot — it queues a graceful dismiss so the stages still walk.
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key.clone(), "main", Utc::now())],
            terminals: vec![terminal_snapshot(session_key.clone())],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "checklist must stay up to walk its stages, not vanish mid-flight",
        );
        let state = m
            .worktree_progress
            .as_ref()
            .expect("checklist state retained until the walk finishes");
        assert!(
            state.dismiss_queued(),
            "the live-terminal snapshot must queue a graceful dismiss",
        );
    }

    /// After the recovery snapshot queues the dismiss, the display walks
    /// every remaining stage (one per min-dwell) and only then tears the
    /// modal down — the #253 fix for "only ever shows one step".
    #[test]
    fn lag_recovery_snapshot_walks_stages_then_dismisses() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:mind-build/mind#1");
        let session_key: lazybox_core::SessionKey = (&key).into();

        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::Clone,
            status: WorktreeStepStatus::Started,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key, "main", Utc::now())],
            terminals: vec![terminal_snapshot(session_key)],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "checklist still up immediately after the snapshot",
        );

        // Drive the min-dwell walk with a monotonically advancing clock
        // (each tick well past the 500ms dwell) instead of sleeping. The
        // modal survives the first few steps and closes only once the
        // whole checklist has been shown.
        let base = Instant::now();
        let mut dismissed_at = None;
        for step in 1..=8u32 {
            m.advance_worktree_progress_at(base + Duration::from_secs(u64::from(step)));
            if !m.modal_stack.contains(&Id::WorktreeProgress) {
                dismissed_at = Some(step);
                break;
            }
        }
        let dismissed_at = dismissed_at.expect("checklist must eventually dismiss after the walk");
        assert!(
            dismissed_at >= 4,
            "must walk all four stages before dismissing, closed after only {dismissed_at}",
        );
        assert!(
            m.worktree_progress.is_none(),
            "checklist state cleared once the walk completes",
        );
    }

    /// Issue #267 — the checklist's *state* walked correctly (the tests
    /// above pass), but the mounted modal was re-added with `app.mount`,
    /// which errors on an already-live id; the error was swallowed, so
    /// every step past the first was silently dropped and the user only
    /// ever saw step 1 before it vanished. This renders the *real*
    /// mounted component and proves an advanced step actually repaints.
    #[test]
    fn advancing_the_checklist_repaints_the_mounted_modal() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        use tuirealm::ratatui::layout::Rect;

        // Render the *mounted* `WorktreeProgress` component (whatever the
        // app currently holds under that id) to a fresh backend, so we
        // observe the actual on-screen component — not the Model's
        // separate `WorktreeProgressState`, which advanced correctly even
        // while the stale component stayed mounted (issue #267).
        fn rendered(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) -> String {
            let mut term = Terminal::new(TestBackend::new(70, 20)).expect("test terminal");
            term.draw(|f| {
                m.app
                    .view(&Id::WorktreeProgress, f, Rect::new(0, 0, 70, 20))
            })
            .expect("draw mounted modal");
            let buf = term.backend().buffer();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        let mut m = build_model();
        let key = WorkspaceKey::new("github:mind-build/mind#1");
        let session_key: lazybox_core::SessionKey = (&key).into();

        // Mount on the first step, then drive the daemon truth forward so
        // the display has somewhere to walk to.
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::WorktreeAdd,
            status: WorktreeStepStatus::Started,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key,
            step: WorktreeStep::Setup,
            status: WorktreeStepStatus::Started,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        // The freshly-mounted modal shows step 0 as the in-flight spinner
        // — nothing is checked off yet.
        assert!(
            !rendered(&mut m).contains('✓'),
            "no step should be done before the walk starts",
        );

        // Walk the display one step past the min-dwell. `shown` advances
        // and step 0 checks off — which only paints if the re-mount
        // replaced the stale component.
        m.advance_worktree_progress_at(Instant::now() + Duration::from_secs(1));
        assert!(
            rendered(&mut m).contains('✓'),
            "an advanced step must repaint the mounted modal, not freeze on step 1:\n{}",
            rendered(&mut m),
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
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });

        // A snapshot whose live terminals are for OTHER sessions says
        // nothing about this spawn — the checklist must stay up.
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key, "main", Utc::now())],
            terminals: vec![terminal_snapshot(other)],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
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
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::Clone,
            status: WorktreeStepStatus::Failed("fatal: could not read from remote".into()),
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });

        // Even with a live terminal in the snapshot, a checklist frozen on
        // an error stays up so the user can read it and press Esc.
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key, "main", Utc::now())],
            terminals: vec![terminal_snapshot(session_key)],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "a failed checklist must survive a recovery snapshot",
        );
    }

    /// Issue #557's headline regression: a provisioning failure mounts the
    /// actionable checklist modal (classified error + hint + `r` retry),
    /// but the daemon *also* emits a redundant `spawn:session`
    /// provider-error. That footer used to tear the modal down and replace
    /// it with a single truncated one-liner (`✗ spawn failed — git w… and
    /// retry`). The modal now owns the failure; the footer is suppressed.
    #[test]
    fn provider_error_does_not_clobber_the_actionable_failure_modal() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:acme/widget#42");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![Workspace::empty(key, "main", Utc::now())],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Failed(
                "worktree: checkout_at: branch 'feat' not found locally or on origin".into(),
            ),
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));

        m.handle_daemon_event(IpcEvent::provider_error_permanent(
            "spawn:session",
            "worktree: git worktree setup failed — spawn aborted, retry once the cause is \
             fixed: worktree: checkout_at: branch 'feat' not found locally or on origin",
        ));
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "the actionable modal must survive the redundant provider-error footer",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_none_or(|n| !n.message.contains("spawn failed")),
            "no truncated spawn-failed footer when the modal owns the failure",
        );
    }

    fn remembered_spawn(session_key: lazybox_core::SessionKey) -> lazybox_ipc::Command {
        lazybox_ipc::Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key,
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Agent("claude".into()),
            cwd: None,
            initial_prompt: None,
            on_main: false,
        }
    }

    /// Issue #1041 (reopened): an unmapped Linear team must never dead-end
    /// `w w` with a "× spawn aborted" failure modal. The daemon's "has no
    /// repo mapping" failure now opens the repo picker **directly** — no
    /// failed checklist, no manual `r` — stashing the team so the pick can
    /// persist `providers.linear.teams.OBI` against the tracked GitHub repos.
    #[test]
    fn unmapped_linear_team_opens_a_repo_picker_stashing_the_team() {
        let mut m = build_model();
        // A tracked GitHub repo the picker can offer.
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![],
            terminals: vec![],
            projects: vec![lazybox_core::Project::github(
                "obin-ai",
                "obin-platform",
                Utc::now(),
            )],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        let key = WorkspaceKey::new("linear:OBI-1749");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.last_spawn = Some(remembered_spawn(session_key));

        m.handle_daemon_event(IpcEvent::provider_error_permanent(
            "spawn:worktree",
            "workspace: Linear team `OBI` has no repo mapping and the ticket has no \
             linked GitHub PR — set providers.linear.teams.OBI in ~/.lazybox/config.yaml",
        ));
        assert!(
            m.modal_stack.contains(&Id::LinearTeamRepo),
            "the picker opens directly as the primary path",
        );
        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress) && m.worktree_progress.is_none(),
            "no '× spawn aborted' failure modal is shown before the picker",
        );
        assert!(
            matches!(
                &m.modal_flow,
                Some(super::super::ModalFlow::LinearTeamRepo { team }) if team == "OBI",
            ),
            "the picker stashes the team the pick will be persisted under",
        );

        // The pick is remembered as `providers.linear.teams.OBI`, and undoing
        // a mis-pick means hand-editing config — so the picker itself must
        // say the choice persists, not leave the user to infer it.
        let rendered = {
            use tuirealm::ratatui::Terminal;
            use tuirealm::ratatui::backend::TestBackend;
            use tuirealm::ratatui::layout::Rect;
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("test terminal");
            terminal
                .draw(|frame| {
                    m.app
                        .view(&Id::LinearTeamRepo, frame, Rect::new(0, 0, 100, 20))
                })
                .expect("render picker");
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|row| {
                    (0..buffer.area.width)
                        .map(|col| buffer[(col, row)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            rendered.contains("saved"),
            "the picker must signal the choice is remembered: {rendered}",
        );
    }

    /// The real primary path (#1041 reopened): the daemon surfaces the
    /// unmapped-team failure as a `WorktreeProgress::Failed` step. That step
    /// must open the picker directly, tearing down the in-flight spinner —
    /// never freezing on a "× spawn aborted" checklist the user has to `r`
    /// past.
    #[test]
    fn unmapped_linear_team_failed_step_opens_the_picker_over_the_spinner() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![],
            terminals: vec![],
            projects: vec![lazybox_core::Project::github(
                "obin-ai",
                "obin-platform",
                Utc::now(),
            )],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let key = WorkspaceKey::new("linear:OBI-1749");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.last_spawn = Some(remembered_spawn(session_key.clone()));

        // Provisioning starts — the spinner mounts.
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: session_key.clone(),
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Started,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));

        // …then the repo resolution fails: the picker replaces the spinner.
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key,
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Failed(
                "workspace: Linear team `OBI` has no repo mapping and the ticket has no \
                 linked GitHub PR — set providers.linear.teams.OBI in ~/.lazybox/config.yaml"
                    .into(),
            ),
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(
            m.modal_stack.contains(&Id::LinearTeamRepo),
            "the failed step opens the picker directly",
        );
        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress) && m.worktree_progress.is_none(),
            "the in-flight spinner is torn down, not left frozen on a failure",
        );
    }

    /// The unmapped-team wire error the daemon emits for team `OBI`.
    const OBI_UNMAPPED: &str = "workspace: Linear team `OBI` has no repo mapping and \
         the ticket has no linked GitHub PR — set providers.linear.teams.OBI in \
         ~/.lazybox/config.yaml";

    fn snapshot_with_obin_platform() -> IpcEvent {
        IpcEvent::Snapshot {
            workspaces: vec![],
            terminals: vec![],
            projects: vec![lazybox_core::Project::github(
                "obin-ai",
                "obin-platform",
                Utc::now(),
            )],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        }
    }

    fn failed_linear_step(
        session_key: lazybox_core::SessionKey,
        origin: lazybox_ipc::SpawnOrigin,
    ) -> IpcEvent {
        IpcEvent::WorktreeProgress {
            session_key,
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Failed(OBI_UNMAPPED.into()),
            origin,
        }
    }

    /// Mapping the team re-issues the spawn that opened the picker straight
    /// away — no manual retry — so the pick "continues the spawn into the
    /// chosen repo" (#1041 reopened). The spawn is the one captured when the
    /// picker opened, so this drives it through the real Failed-step path.
    #[test]
    fn mapping_a_linear_team_reprovisions_the_spawn() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.handle_daemon_event(snapshot_with_obin_platform());
        let session_key: lazybox_core::SessionKey = (&WorkspaceKey::new("linear:OBI-1749")).into();
        m.last_spawn = Some(remembered_spawn(session_key.clone()));

        // The failure opens the picker and captures this spawn.
        m.handle_daemon_event(failed_linear_step(
            session_key,
            lazybox_ipc::SpawnOrigin::Interactive,
        ));
        assert!(m.modal_stack.contains(&Id::LinearTeamRepo));
        while server.rx.try_recv().is_ok() {} // drain init + spawn traffic

        m.reprovision_after_linear_map();

        let mut saw_spawn = false;
        while let Ok(cmd) = server.rx.try_recv() {
            if matches!(cmd, lazybox_ipc::Command::Spawn { .. }) {
                saw_spawn = true;
            }
        }
        assert!(
            saw_spawn,
            "the pick must re-issue the spawn so it lands in the freshly-mapped repo",
        );
    }

    /// Review finding 2: two unmapped-team `w w` in flight must not cross
    /// wires. Ticket A's failure opens the picker (capturing A's spawn); a
    /// second `w w` on B overwrites `last_spawn` and its failure is
    /// suppressed by the already-open picker. Mapping in A's picker must
    /// re-provision **A's** spawn — the one that owns the picker — not
    /// whatever `last_spawn` drifted to (B). Regresses the single-slot
    /// `last_spawn` reprovision that would have fired B.
    #[test]
    fn concurrent_unmapped_spawns_reprovision_the_picker_owner_not_the_latest() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.handle_daemon_event(snapshot_with_obin_platform());
        let key_a: lazybox_core::SessionKey = (&WorkspaceKey::new("linear:OBI-1")).into();
        let key_b: lazybox_core::SessionKey = (&WorkspaceKey::new("linear:OBI-2")).into();

        // A fails first → picker opens and captures A.
        m.last_spawn = Some(remembered_spawn(key_a.clone()));
        m.handle_daemon_event(failed_linear_step(
            key_a.clone(),
            lazybox_ipc::SpawnOrigin::Interactive,
        ));
        assert!(m.modal_stack.contains(&Id::LinearTeamRepo));

        // B races in: last_spawn becomes B, but B's failure is suppressed by
        // the picker already up for A.
        m.last_spawn = Some(remembered_spawn(key_b.clone()));
        m.handle_daemon_event(failed_linear_step(
            key_b,
            lazybox_ipc::SpawnOrigin::Interactive,
        ));
        while server.rx.try_recv().is_ok() {} // drain

        m.reprovision_after_linear_map();

        let mut spawned_key = None;
        while let Ok(cmd) = server.rx.try_recv() {
            if let lazybox_ipc::Command::Spawn { session_key, .. } = cmd {
                spawned_key = Some(session_key);
            }
        }
        assert_eq!(
            spawned_key.as_ref(),
            Some(&key_a),
            "the pick re-provisions the picker's own spawn (A), not the later race (B)",
        );
    }

    /// Review finding 1: an autonomous unmapped-Linear failure has no
    /// client-issued spawn — `last_spawn` holds an unrelated *interactive*
    /// spawn from another session. It must NOT auto-open a picker that would
    /// then re-provision that stale spawn; it falls to the failure modal, and
    /// a subsequent map is persist-only (no stray spawn fired).
    #[test]
    fn autonomous_unmapped_failure_does_not_reprovision_a_stale_interactive_spawn() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.handle_daemon_event(snapshot_with_obin_platform());
        // A stale interactive spawn on an unrelated GitHub workspace.
        let stale: lazybox_core::SessionKey = (&WorkspaceKey::new("github:acme/widget#7")).into();
        m.last_spawn = Some(remembered_spawn(stale));

        // An autonomous unmapped-Linear failure on a *different* session.
        let linear_key: lazybox_core::SessionKey = (&WorkspaceKey::new("linear:OBI-9")).into();
        m.handle_daemon_event(failed_linear_step(
            linear_key,
            lazybox_ipc::SpawnOrigin::Autonomous(lazybox_ipc::AutonomousTrigger::AutoFix),
        ));

        assert!(
            !m.modal_stack.contains(&Id::LinearTeamRepo),
            "an autonomous failure must not hijack a stale interactive spawn into a picker",
        );
        assert_eq!(
            m.worktree_progress.as_ref().and_then(|s| s.recovery()),
            Some(lazybox_ipc::WorktreeRecovery::LinearUnmapped),
            "it shows the recovery modal instead",
        );

        // Even a subsequent map must not re-issue the stale interactive spawn.
        while server.rx.try_recv().is_ok() {} // drain
        m.reprovision_after_linear_map();
        let mut saw_spawn = false;
        while let Ok(cmd) = server.rx.try_recv() {
            if matches!(cmd, lazybox_ipc::Command::Spawn { .. }) {
                saw_spawn = true;
            }
        }
        assert!(
            !saw_spawn,
            "no stale interactive spawn is re-issued for an autonomous Linear failure",
        );
    }

    /// With no GitHub repo tracked, there's genuinely nothing to propose, so
    /// the unmapped-team failure falls through to the classified recovery
    /// modal (the true last resort) instead of mounting an empty picker —
    /// and its `r` still flashes the manual hint rather than an empty list
    /// (#1041).
    #[test]
    fn unmapped_linear_team_without_repos_does_not_mount_an_empty_picker() {
        let mut m = build_model();
        let key = WorkspaceKey::new("linear:OBI-1749");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.last_spawn = Some(remembered_spawn(session_key));
        m.handle_daemon_event(IpcEvent::provider_error_permanent(
            "spawn:worktree",
            "workspace: Linear team `OBI` has no repo mapping and the ticket has no \
             linked GitHub PR — set providers.linear.teams.OBI in ~/.lazybox/config.yaml",
        ));
        assert!(
            !m.modal_stack.contains(&Id::LinearTeamRepo),
            "no tracked repos means no picker to mount",
        );
        assert_eq!(
            m.worktree_progress.as_ref().and_then(|s| s.recovery()),
            Some(lazybox_ipc::WorktreeRecovery::LinearUnmapped),
            "it falls back to the classified recovery modal as the last resort",
        );

        m.pick_repo_for_linear_team();
        assert!(
            !m.modal_stack.contains(&Id::LinearTeamRepo),
            "and `r` there still can't mount an empty picker",
        );
    }

    /// Issue #594: some spawn paths (retry, fast spawn, a dismissed
    /// checklist) never mount a live `WorktreeProgress`, so a
    /// worktree-provisioning failure the daemon labels `spawn:worktree`
    /// arrives ONLY as a provider-error. It used to fall through to a
    /// middle-truncated footer line (`✗ spawn failed — …switch it to a
    /// different branch) and retry`) that elided the actionable recovery
    /// text. It must instead route to the recovery modal — the same surface
    /// #557/#562 built — with no footer.
    #[test]
    fn worktree_spawn_error_without_a_checklist_routes_to_the_recovery_modal() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:acme/widget#42");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.last_spawn = Some(remembered_spawn(session_key));
        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "no checklist is live on this spawn path",
        );

        m.handle_daemon_event(IpcEvent::provider_error_permanent(
            "spawn:worktree",
            "worktree: git worktree setup failed — spawn aborted, retry once the cause is \
             fixed: branch 'feat' is already checked out at /tmp/other — refusing to take it \
             from another live worktree; remove that worktree (or switch it to a different \
             branch) and retry",
        ));

        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "a worktree failure must reach the recovery modal even with no checklist",
        );
        assert_eq!(
            m.worktree_progress.as_ref().and_then(|s| s.recovery()),
            Some(lazybox_ipc::WorktreeRecovery::BranchHeldLive),
            "the modal carries the classified recovery so it renders the branch-held hint",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_none_or(|n| !n.message.contains("spawn failed")),
            "the truncated spawn-failed footer must be suppressed in favor of the modal",
        );
    }

    /// Finding 1: routing is gated on the daemon's *source*, not on
    /// re-classifying the free-text message. A non-worktree spawn error
    /// (backend PTY spawn, unknown agent) that happens to contain a
    /// classifier substring — here a PTY `Permission denied`, which
    /// classifies as `Disk` — must still footer, never mount the worktree
    /// recovery modal, because its source isn't `spawn:worktree`.
    #[test]
    fn non_worktree_spawn_error_footers_even_when_its_text_classifies() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:acme/widget#42");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.last_spawn = Some(remembered_spawn(session_key));
        // Prove the text WOULD classify — so the gate can't be leaning on it.
        assert_ne!(
            lazybox_ipc::WorktreeRecovery::classify("backend: Permission denied (os error 13)"),
            lazybox_ipc::WorktreeRecovery::Unknown,
        );

        m.handle_daemon_event(IpcEvent::provider_error_permanent(
            "spawn",
            "backend: Permission denied (os error 13)",
        ));

        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "a non-worktree spawn error must not fabricate a recovery modal",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("spawn failed")),
            "the footer notice still carries the backend failure",
        );
        assert_eq!(
            m.status.notice.as_ref().unwrap().severity,
            crate::realm::components::footer::NoticeSeverity::Retryable,
            "spawn failures must auto-fade instead of pinning the footer",
        );
        m.status.notice.as_mut().unwrap().set_at =
            std::time::Instant::now() - std::time::Duration::from_secs(6);
        assert!(m.status.tick_notice(), "the spawn-error toast must fade");
        assert!(m.status.notice.is_none());
    }

    /// A session/workspace race (`spawn:session`) is not a worktree failure
    /// a retry could fix, so it keeps its plain footer notice and never
    /// mounts the recovery modal.
    #[test]
    fn spawn_session_race_still_goes_to_the_footer() {
        let mut m = build_model();
        let key = WorkspaceKey::new("github:acme/widget#42");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.last_spawn = Some(remembered_spawn(session_key));

        m.handle_daemon_event(IpcEvent::provider_error_permanent(
            "spawn:session",
            "spawn target session moved while provisioning; retry from its current workspace",
        ));

        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "a session-race spawn error must not fabricate a recovery modal",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("spawn failed")),
            "the footer notice still carries the race failure",
        );
    }

    /// Finding 3: a worktree failure attributed (via `last_spawn`) to one
    /// spawn must not tear down a *different* spawn's live, in-progress
    /// checklist. The other session's checklist keeps advancing; the
    /// unrelated failure still surfaces on the footer.
    #[test]
    fn spawn_failure_does_not_clobber_a_concurrent_live_checklist() {
        let mut m = build_model();
        let other_key = WorkspaceKey::new("github:acme/widget#7");
        let other_session: lazybox_core::SessionKey = (&other_key).into();
        // A different spawn's checklist is live and still provisioning.
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: other_session.clone(),
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Started,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));

        // A worktree failure for a *different* spawn arrives.
        let failing_key = WorkspaceKey::new("github:acme/widget#42");
        let failing_session: lazybox_core::SessionKey = (&failing_key).into();
        m.last_spawn = Some(remembered_spawn(failing_session));
        m.handle_daemon_event(IpcEvent::provider_error_permanent(
            "spawn:worktree",
            "worktree: branch 'feat' is already checked out at /tmp/other — refusing to \
             take it from another live worktree",
        ));

        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "the concurrent spawn's checklist must survive",
        );
        let state = m
            .worktree_progress
            .as_ref()
            .expect("the other session's checklist is retained");
        assert_eq!(
            state.session_key, other_session,
            "the live checklist must still belong to the other, in-flight spawn",
        );
        assert!(
            !state.failed(),
            "the other spawn's checklist keeps provisioning, not frozen on a foreign failure",
        );
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("spawn failed")),
            "the unrelated failure still surfaces on the footer",
        );
    }

    /// `r` on the failed modal re-issues the remembered spawn so the user
    /// retries provisioning in place, and clears the frozen checklist so
    /// the retry's own progress events mount a fresh one (issue #557).
    #[test]
    fn retry_re_dispatches_the_failed_spawn() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let key = WorkspaceKey::new("github:acme/widget#42");
        let session_key: lazybox_core::SessionKey = (&key).into();
        // What `flush_dispatched_cmds` stashes on the original `w`.
        m.last_spawn = Some(lazybox_ipc::Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: session_key.clone(),
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Agent("claude".into()),
            cwd: None,
            initial_prompt: Some("fix it".into()),
            on_main: false,
        });
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key,
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Failed("boom".into()),
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));
        while server.rx.try_recv().is_ok() {} // drain init traffic

        m.retry_worktree_provision();

        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "the frozen failed modal is cleared so the retry mounts a fresh one",
        );
        let mut saw_spawn = false;
        while let Ok(cmd) = server.rx.try_recv() {
            if matches!(cmd, lazybox_ipc::Command::Spawn { .. }) {
                saw_spawn = true;
            }
        }
        assert!(saw_spawn, "retry must re-issue the spawn to the daemon");
    }

    /// Issue #787: `r` on a `BranchMismatch` modal dispatches a
    /// `RecreateWorktree` carrying the remembered spawn, so the daemon
    /// preserves the conflicting checkout aside and re-provisions — no
    /// out-of-band `git` needed.
    #[test]
    fn recreate_dispatches_recreate_worktree_from_the_remembered_spawn() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let key = WorkspaceKey::new("github:acme/widget#42");
        let session_key: lazybox_core::SessionKey = (&key).into();
        m.last_spawn = Some(lazybox_ipc::Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: session_key.clone(),
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Agent("claude".into()),
            cwd: None,
            initial_prompt: Some("fix it".into()),
            on_main: false,
        });
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key,
            step: WorktreeStep::WorktreeAdd,
            status: WorktreeStepStatus::Failed(
                "checkout_at: worktree /tmp/wt is checked out on branch 'issue-42-old', \
                 not the requested branch 'issue-42-new' — refusing to reuse it"
                    .into(),
            ),
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));
        while server.rx.try_recv().is_ok() {}

        m.recreate_worktree_provision();

        assert!(!m.modal_stack.contains(&Id::WorktreeProgress));
        let mut recreate = None;
        while let Ok(cmd) = server.rx.try_recv() {
            if let lazybox_ipc::Command::RecreateWorktree { spawn, .. } = cmd {
                recreate = Some(spawn);
            }
        }
        let spawn = recreate.expect("recreate must issue a RecreateWorktree command");
        assert!(matches!(spawn.kind, TerminalKind::Agent(ref id) if id == "claude"));
        // A plain BranchMismatch moves the workspace's own target, not a holder.
        // (holder preservation only applies to BranchHeldManaged.)
    }

    /// Issue #787: `g` on a `BranchHeldLive` modal reveals the workspace
    /// whose session worktree path matches the holder named in the error.
    #[test]
    fn jump_to_holder_reveals_the_session_owning_the_checkout() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");

        // A holder workspace with a session at a known worktree path.
        let holder_key = WorkspaceKey::new("github:acme/widget#7");
        let mut holder = Workspace::empty(holder_key.clone(), "feat", Utc::now());
        let session = lazybox_core::WorkspaceSession::new(
            holder_key.clone(),
            lazybox_core::SessionKind::Agent {
                agent_id: "claude".into(),
            },
            std::path::PathBuf::from("/tmp/holder-wt"),
            Utc::now(),
        );
        holder.add_session(session);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(holder)));

        // The stuck workspace's failed modal names that holder path.
        let stuck_key = WorkspaceKey::new("github:acme/widget#42");
        let stuck_session_key: lazybox_core::SessionKey = (&stuck_key).into();
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: stuck_session_key,
            step: WorktreeStep::WorktreeAdd,
            status: WorktreeStepStatus::Failed(
                "checkout_at: branch 'feat' is already checked out at /tmp/holder-wt \
                 — refusing to take it from another live worktree"
                    .into(),
            ),
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));

        m.jump_to_worktree_holder();

        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "a successful jump closes the recovery modal",
        );
        assert_eq!(
            m.sidebar.selected_workspace().map(|w| w.key.clone()),
            Some(holder_key),
            "jump reveals the workspace holding the branch",
        );
    }

    /// Issue #787 review #4: when the branch is held by an *external*
    /// checkout no lazybox session owns, jump can't navigate anywhere — it
    /// must name the checkout and leave the recovery modal open rather than
    /// silently dismissing into a dead end.
    #[test]
    fn jump_to_external_holder_names_it_and_keeps_the_modal() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");

        let stuck_key = WorkspaceKey::new("github:acme/widget#42");
        let stuck_session_key: lazybox_core::SessionKey = (&stuck_key).into();
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: stuck_session_key,
            step: WorktreeStep::WorktreeAdd,
            status: WorktreeStepStatus::Failed(
                "checkout_at: branch 'feat' is already checked out at \
                 /home/dev/manual-clone — refusing to take it from another live worktree"
                    .into(),
            ),
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));

        m.jump_to_worktree_holder();

        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "an unresolvable jump must not dismiss the recovery modal",
        );
        let notice = m.status.notice.as_ref().expect("a notice is shown");
        assert!(
            notice.message.contains("/home/dev/manual-clone"),
            "the notice names the external checkout: {}",
            notice.message,
        );
    }
}

#[cfg(test)]
mod click_outside_modal_dismiss_tests {
    //! Issue #253 — a *dismissable* modal (a read-only / progress
    //! overlay, never a destructive confirm or conversational Help)
    //! must not trap the user: a press outside it closes it exactly like
    //! Esc AND lets the click do its normal thing, so clicking a sidebar
    //! workspace both dismisses the worktree-provisioning checklist and
    //! selects that workspace in one action. Blocking surfaces keep
    //! owning input so a stray click can't skip or trigger data loss.
    use super::super::{Id, Model};
    use crate::realm::Msg;
    use chrono::Utc;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use lazybox_core::{SessionKey, Workspace, WorkspaceKey};
    use lazybox_ipc::{Event as IpcEvent, WorktreeStep, WorktreeStepStatus, channel};
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

    fn left_down(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Screen row a click must land on to select `key` (mirrors the
    /// sidebar's 5-line header; scroll is zero for the seeded rows).
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

    /// The headline repro: with the provisioning checklist up, clicking a
    /// different workspace closes it and selects that workspace at once.
    #[test]
    fn clicking_a_workspace_dismisses_progress_and_selects_it() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1"), empty_ws("github:o/r#2")],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let a = key_of("github:o/r#1");
        let b = key_of("github:o/r#2");

        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _, _) = m.effective_pane_rects(area);
        let row_b = row_of(&mut m, sidebar_rect, &b);
        // Park the selection on WS-A so the click has to move it.
        assert!(m.__test_sidebar_mut().focus_workspace_key(&a));

        // The provisioning checklist for WS-A mounts.
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: a.clone(),
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Started,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));

        m.layout.last_area = area;
        let handled = m.dismiss_modal_on_outside_click(left_down(sidebar_rect.x + 1, row_b));

        assert!(handled, "the press on a dismissable overlay was handled");
        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "the checklist closed on the outside click",
        );
        assert!(
            m.worktree_progress.is_none(),
            "checklist state cleared, same as an Esc dismiss",
        );
        assert_eq!(
            m.sidebar().selected_workspace_key(),
            Some(&b),
            "the same click also selected the clicked workspace",
        );
    }

    /// Regression: clicking away from the checklist *backgrounds* an
    /// in-flight worktree provision — it must NOT abort the spawn. Esc is
    /// the deliberate cancel gesture (#403, see
    /// `worktree_progress_dismiss_tests::esc_mid_provision_sends_cancel_spawn`);
    /// clicking a sidebar row to go do something else while the spawn is
    /// still provisioning previously killed it, because outside-click
    /// reused Esc's `CancelSpawn`-emitting dismiss path.
    #[test]
    fn outside_click_backgrounds_provision_without_cancelling_spawn() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1"), empty_ws("github:o/r#2")],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let a = key_of("github:o/r#1");
        let b = key_of("github:o/r#2");

        let area = Rect::new(0, 0, 120, 40);
        let (sidebar_rect, _, _) = m.effective_pane_rects(area);
        let row_b = row_of(&mut m, sidebar_rect, &b);
        assert!(m.__test_sidebar_mut().focus_workspace_key(&a));

        // Provisioning is genuinely in flight (a Started step — not
        // failed/warned), i.e. exactly the state where an Esc WOULD
        // cancel the spawn.
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: a.clone(),
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Started,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        });
        assert!(m.modal_stack.contains(&Id::WorktreeProgress));

        // Drop setup traffic (Subscribe, focus hints, …) before the click.
        while server.rx.try_recv().is_ok() {}

        m.layout.last_area = area;
        assert!(m.dismiss_modal_on_outside_click(left_down(sidebar_rect.x + 1, row_b)));

        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "the checklist still closes on the outside click",
        );
        let cancelled = std::iter::from_fn(|| server.rx.try_recv().ok()).any(|c| {
            matches!(
                c,
                lazybox_ipc::Command::CancelSpawn { session_key } if session_key == a
            )
        });
        assert!(
            !cancelled,
            "an outside click must background the provision, not cancel the spawn",
        );
    }

    /// A destructive confirm must ignore the outside click — it keeps
    /// owning input so a stray click can't dismiss or trigger data loss.
    #[test]
    fn destructive_confirm_ignores_outside_click() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1")],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        m.mount_clean_worktrees_confirm();
        assert!(m.modal_stack.contains(&Id::CleanWorktreesConfirm));

        m.layout.last_area = Rect::new(0, 0, 120, 40);
        let handled = m.dismiss_modal_on_outside_click(left_down(1, 6));

        assert!(!handled, "a blocking confirm does not click-dismiss");
        assert!(
            m.modal_stack.contains(&Id::CleanWorktreesConfirm),
            "the confirm stays up, still owning input",
        );
    }

    /// Scroll over a dismissable overlay is NOT a dismiss gesture — only
    /// presses are, so the sync-status window still scrolls on the wheel.
    #[test]
    fn wheel_over_dismissable_overlay_is_not_a_dismiss() {
        let mut m = build_model();
        m.mount_sync_status();
        assert!(m.modal_stack.contains(&Id::SyncStatus));

        m.layout.last_area = Rect::new(0, 0, 120, 40);
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 60,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        assert!(
            !m.dismiss_modal_on_outside_click(wheel),
            "a scroll must fall through to the modal, not dismiss it",
        );
        assert!(m.modal_stack.contains(&Id::SyncStatus));
    }

    /// Help owns a conversation, so clicks outside either of its two
    /// surfaces must leave it open.
    #[test]
    fn help_ignores_outside_click() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1")],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        m.mount_help();
        assert!(m.modal_stack.contains(&Id::Help));

        m.layout.last_area = Rect::new(0, 0, 120, 40);
        assert!(!m.dismiss_modal_on_outside_click(left_down(1, 6)));
        assert!(
            m.modal_stack.contains(&Id::Help),
            "shortcut index stays open after an outside press",
        );

        m.update(Msg::HelpAskOpen);
        assert_eq!(m.modal_stack.last(), Some(&Id::HelpAsk));
        assert!(!m.dismiss_modal_on_outside_click(left_down(1, 6)));
        assert!(
            m.modal_stack.contains(&Id::HelpAsk),
            "conversation stays open after an outside press",
        );
    }

    /// A left-click on the footer's `… +N ? all` overflow cell opens the
    /// `?` catalog so the elided hints are reachable — the count is no
    /// longer a dead end (#805). The footer sits outside every pane, so
    /// this handler is the only thing that claims the click.
    #[test]
    fn footer_overflow_click_opens_help() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1")],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let area = Rect::new(0, 0, 120, 40);
        // Simulate the last render having placed the overflow cell at the
        // right end of the footer row.
        let cell = Rect::new(100, 39, 10, 1);
        m.footer_overflow_rect = Some(cell);
        assert!(m.modal_stack.is_empty(), "no modal before the click");
        m.dispatch_mouse_in(left_down(cell.x + 2, cell.y), area);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::HelpAsk),
            "clicking the overflow cell must open the `?` catalog",
        );
    }

    /// A click that misses the overflow cell must not open help — only
    /// the cell itself is the escape hatch (#805).
    #[test]
    fn click_off_footer_overflow_leaves_help_closed() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![empty_ws("github:o/r#1")],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let area = Rect::new(0, 0, 120, 40);
        m.footer_overflow_rect = Some(Rect::new(100, 39, 10, 1));
        m.dispatch_mouse_in(left_down(1, 1), area);
        assert!(
            !m.modal_stack.contains(&Id::HelpAsk),
            "a click away from the overflow cell must not open help",
        );
    }
}

// NOTE: the client-side auto_merge_on_green_tests module was removed
// when the "auto-merge on green" trigger moved into the daemon's
// polling commit path. Its latch semantics are now pinned server-side
// in `lazybox-server`'s `polling::auto_merge` tests (fires once per
// green head, no double-fire on re-broadcast, re-arm on a new head,
// Done suppresses interim green polls).

#[cfg(test)]
mod merge_latch_tests {
    //! Issue #265: a confirmed merge (`Event::PrMerged`) is latched
    //! Model-side so MERGED is authoritative. Every incoming
    //! `WorkspaceUpserted` / `Snapshot` is patched through
    //! `apply_merge_latch` before fan-out, so an interim poll still
    //! reporting `Open` can't flicker the row/header back; the latch
    //! releases only when a poll confirms the terminal state or the
    //! workspace is removed.
    use super::super::*;
    use chrono::{Duration, Utc};
    use lazybox_core::{CiStatus, SessionKey, Task, TaskId, TaskRole, TaskState, Workspace};
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn right_pane_state(m: &Model<tuirealm::terminal::TestTerminalAdapter>) -> Option<TaskState> {
        m.right
            .selected_workspace()
            .and_then(|w| w.pr.as_ref())
            .map(|pr| pr.state)
    }

    fn pr_ws(state: TaskState) -> Workspace {
        pr_ws_n(1, state)
    }

    /// A PR workspace keyed `owner/repo#{num}` so a test can build
    /// several distinct rows (e.g. to navigate the sidebar between them).
    fn pr_ws_n(num: u32, state: TaskState) -> Workspace {
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("owner/repo#{num}"),
            },
            title: "add thing".into(),
            body: None,
            state,
            role: TaskRole::Author,
            ci: CiStatus::Success,
            review: lazybox_core::ReviewStatus::Approved,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/owner/repo/pull/{num}"),
            repo: Some("owner/repo".into()),
            branch: Some("feature".into()),
            base_branch: Some("main".into()),
            updated_at: Utc::now() - Duration::hours(1),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: Some("PR_node".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        };
        Workspace::from_task(task, Utc::now())
    }

    fn state_of(ws: &Workspace) -> TaskState {
        ws.pr.as_ref().expect("workspace has a PR").state
    }

    #[test]
    fn apply_merge_latch_forces_merged_on_an_interim_open_poll_and_holds() {
        let mut m = build_model();
        let mut ws = pr_ws(TaskState::Open);
        m.merge_confirmed.insert(ws.key.clone());

        m.apply_merge_latch(&mut ws);

        assert_eq!(
            state_of(&ws),
            TaskState::Merged,
            "interim Open forced to Merged"
        );
        assert!(
            m.merge_confirmed.contains(&ws.key),
            "latch held until a poll confirms the terminal state",
        );
        assert!(
            ws.pr.as_ref().unwrap().closed_at.is_some(),
            "closed_at stamped so the sidebar grace window keys off it",
        );
    }

    #[test]
    fn apply_merge_latch_releases_when_a_poll_confirms_the_terminal_state() {
        for state in [TaskState::Merged, TaskState::Closed] {
            let mut m = build_model();
            let mut ws = pr_ws(state);
            m.merge_confirmed.insert(ws.key.clone());

            m.apply_merge_latch(&mut ws);

            assert_eq!(state_of(&ws), state, "confirming poll accepted as-is");
            assert!(
                !m.merge_confirmed.contains(&ws.key),
                "{state:?} poll releases the latch",
            );
        }
    }

    #[test]
    fn apply_merge_latch_is_a_noop_for_unlatched_keys() {
        let mut m = build_model();
        let mut ws = pr_ws(TaskState::Open);
        m.apply_merge_latch(&mut ws);
        assert_eq!(
            state_of(&ws),
            TaskState::Open,
            "un-latched upsert untouched"
        );
    }

    #[test]
    fn pr_merged_latches_and_an_interim_open_upsert_holds_while_merged_releases() {
        let mut m = build_model();
        m.status.polling = None;
        let key = pr_ws(TaskState::Open).key.clone();

        m.handle_daemon_event(IpcEvent::PrMerged {
            workspace_key: key.clone(),
            pr_label: "owner/repo#1".into(),
        });
        assert!(m.merge_confirmed.contains(&key), "PrMerged latches the key");

        // Interim poll still Open → patched, latch held.
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr_ws(
            TaskState::Open,
        ))));
        assert!(
            m.merge_confirmed.contains(&key),
            "an interim Open poll holds the latch",
        );

        // Confirming poll reports Merged → released.
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr_ws(
            TaskState::Merged,
        ))));
        assert!(
            !m.merge_confirmed.contains(&key),
            "a confirming Merged poll releases the latch",
        );
    }

    #[test]
    fn snapshot_recovery_reporting_open_still_holds_the_latch() {
        let mut m = build_model();
        m.status.polling = None;
        let key = pr_ws(TaskState::Open).key.clone();
        m.merge_confirmed.insert(key.clone());

        // Reconnect: a fresh snapshot taken before the daemon re-polled
        // still shows the PR Open.
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![pr_ws(TaskState::Open)],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert!(
            m.merge_confirmed.contains(&key),
            "the latch survives a reconnect snapshot reporting Open",
        );
    }

    #[test]
    fn workspace_removed_clears_the_latch() {
        let mut m = build_model();
        let key = pr_ws(TaskState::Open).key.clone();
        m.merge_confirmed.insert(key.clone());

        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(key.clone()));
        assert!(
            !m.merge_confirmed.contains(&key),
            "removing the workspace drops its latch",
        );
    }

    #[test]
    fn navigating_away_and_back_keeps_the_right_pane_merged() {
        // The right pane's immediate flip only touches the copy it's
        // currently showing, so a user who navigates away and back before
        // the confirming poll relies on `sync_panes` pulling the sidebar's
        // (latched-MERGED) copy. Lock that seam in.
        let mut m = build_model();
        m.status.polling = None;

        let x = pr_ws_n(1, TaskState::Open);
        let y = pr_ws_n(2, TaskState::Open);
        let x_key = x.key.clone();
        let x_sk = SessionKey::from(&x.key);
        let y_sk = SessionKey::from(&y.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(x)));
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(y)));

        // Merge X (the freshly-merged row stays in the Inbox during the
        // grace window, so it's still selectable).
        m.handle_daemon_event(IpcEvent::PrMerged {
            workspace_key: x_key,
            pr_label: "owner/repo#1".into(),
        });

        // Navigate to Y (right pane shows Y, Open) …
        assert!(m.sidebar.focus_workspace_key(&y_sk));
        m.sync_panes();
        assert_eq!(right_pane_state(&m), Some(TaskState::Open), "Y shows Open");

        // … then back to X before any confirming poll. The header must
        // still read MERGED, not revert to Open.
        assert!(m.sidebar.focus_workspace_key(&x_sk));
        m.sync_panes();
        assert_eq!(
            right_pane_state(&m),
            Some(TaskState::Merged),
            "the right pane stays MERGED across a navigate-away-and-back",
        );
    }
}

mod inspect_list_remount_tests {
    //! Same root cause as [`super::worktree_progress_recovery_tests`]
    //! (issue #267): `mount_modal_boxed` used to call `app.mount`, which
    //! errors on an already-live id. The worktree inspector re-mounts
    //! itself in place after a delete (`mount_inspect_list` is documented
    //! as idempotent — re-rendering the now-shorter list), so under the
    //! old code that re-render silently failed and the inspector kept
    //! showing the deleted row. This pins the re-mount-replaces-the-live
    //! component contract on a second, non-progress code path.
    use super::super::{Id, Model};
    use lazybox_ipc::channel;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;
    use tuirealm::ratatui::layout::{Rect, Size};

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn healthy_dto(name: &str) -> lazybox_ipc::WorktreeInspectionDto {
        lazybox_ipc::WorktreeInspectionDto {
            path: std::path::PathBuf::from(format!("/tmp/worktrees/{name}")),
            bare_path: None,
            branch: Some("main".into()),
            session_id: None,
            reasons: Vec::new(),
            size_bytes: 0,
            last_modified_unix: Some(0),
            has_uncommitted_changes: false,
            has_unpushed_commits: false,
            is_safe_to_delete: false,
        }
    }

    fn rendered(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) -> String {
        let mut term = Terminal::new(TestBackend::new(120, 20)).expect("test terminal");
        term.draw(|f| m.app.view(&Id::InspectList, f, Rect::new(0, 0, 120, 20)))
            .expect("draw mounted modal");
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn re_inspecting_replaces_the_stale_inspector_list() {
        let mut m = build_model();

        // Open the inspector with two worktrees.
        m.mount_inspect_list(vec![healthy_dto("alpha-tree"), healthy_dto("beta-tree")]);
        assert!(m.modal_stack.contains(&Id::InspectList));
        let out = rendered(&mut m);
        assert!(out.contains("alpha-tree"), "first list shows alpha:\n{out}");
        assert!(out.contains("beta-tree"), "first list shows beta:\n{out}");

        // `beta-tree` was deleted; the daemon replies with the shorter
        // inspection, which re-mounts the list in place. The stale row
        // must be gone — which only happens if the re-mount replaced the
        // live component instead of silently failing.
        m.mount_inspect_list(vec![healthy_dto("alpha-tree")]);
        assert_eq!(
            m.modal_stack
                .iter()
                .filter(|id| **id == Id::InspectList)
                .count(),
            1,
            "re-inspect must not pile up duplicate inspector entries",
        );
        let out = rendered(&mut m);
        assert!(
            out.contains("alpha-tree"),
            "surviving row still shown:\n{out}"
        );
        assert!(
            !out.contains("beta-tree"),
            "the deleted row must not linger in the re-rendered list:\n{out}",
        );
    }
}

#[cfg(test)]
mod modal_stack_remount_tests {
    use super::super::{Id, Model};
    use crate::realm::components::confirm::Confirm;
    use lazybox_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    #[test]
    fn remount_moves_an_existing_modal_to_the_top_without_duplication() {
        let (client, _server) = channel::pair();
        let mut model =
            Model::new_for_test(client, Size::new(120, 40)).expect("model initialization");

        model.mount_modal(Id::Error, Confirm::new("first"));
        model.mount_modal(Id::Update, Confirm::new("second"));
        model.mount_modal(Id::Error, Confirm::new("replacement"));

        assert_eq!(model.modal_stack, vec![Id::Update, Id::Error]);
        model.pop_modal();
        assert_eq!(
            model.modal_stack,
            vec![Id::Update],
            "dismissing the replacement must reveal the previous modal"
        );
    }
}

#[cfg(test)]
mod flash_log_tests {
    //! Sticky footer errors are width-capped at render time (#291),
    //! so `flash_error` must keep the full text recoverable in the
    //! sync log (`Shift-D`). Provider sync banners are exempt: the
    //! underlying `ProviderError` event is already recorded there,
    //! and logging the banner too would double-count the failure.
    use super::super::*;
    use lazybox_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    fn model() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
    ) {
        let (client, server) = channel::pair();
        let m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        (m, server)
    }

    #[test]
    fn flash_error_records_full_text_in_sync_log() {
        let (mut m, _server) = model();
        let long = format!("✗ merge failed — repo#1: {}", "long reason ".repeat(20));
        m.flash_error(long.clone());
        let entry = m.status.sync.recent().next().expect("error logged");
        assert_eq!(entry.source, "ui");
        match &entry.outcome {
            crate::realm::status_ctx::SyncOutcome::Err { message, .. } => {
                assert_eq!(message, &long, "log keeps the untruncated text");
            }
            other => panic!("expected an Err outcome, got {other:?}"),
        }
    }

    #[test]
    fn flash_sync_error_does_not_double_log() {
        let (mut m, _server) = model();
        m.flash_sync_error("github", "✗ sync failed — github: boom");
        assert_eq!(
            m.status.sync.recent().count(),
            0,
            "the ProviderError event path owns the sync-log entry",
        );
        assert!(m.status.notice.is_some(), "the sticky banner still shows");
        assert_eq!(m.sync_error_source.as_deref(), Some("github"));
    }

    #[test]
    fn rejected_terminal_input_is_retryable_and_never_a_sync_failure() {
        use crate::realm::components::footer::NoticeSeverity;
        use lazybox_ipc::{Event as IpcEvent, TerminalId};

        let (mut m, _server) = model();
        m.status.polling = None;
        m.handle_daemon_event(IpcEvent::TerminalInputRejected {
            terminal_id: TerminalId(9),
            message: "write timed out; retry".into(),
        });

        let notice = m.status.notice.as_ref().expect("retryable notice");
        assert_eq!(notice.severity, NoticeSeverity::Retryable);
        assert!(notice.message.contains("write timed out"));
        assert_eq!(
            m.status.sync.recent().count(),
            0,
            "terminal delivery must not poison provider sync history"
        );
        assert!(
            m.status
                .messages
                .recent()
                .any(|entry| entry.message.contains("write timed out")),
            "the full failure remains recoverable after the footer fades"
        );
    }

    #[test]
    fn rejected_command_is_retryable_and_never_a_sync_failure() {
        use crate::realm::components::footer::NoticeSeverity;
        use lazybox_ipc::Event as IpcEvent;

        let (mut m, _server) = model();
        m.status.polling = None;
        m.handle_daemon_event(IpcEvent::CommandRejected {
            command: "Write".into(),
            message: "terminal I/O lane is full; retry".into(),
        });

        let notice = m.status.notice.as_ref().expect("retryable notice");
        assert_eq!(notice.severity, NoticeSeverity::Retryable);
        assert!(notice.message.contains("Write was not accepted"));
        assert_eq!(m.status.sync.recent().count(), 0);
    }
}

#[cfg(test)]
mod help_ask_tests {
    //! Effect contracts for the "ask lazybox" help assistant (#302):
    //! question routing (start run / reuse run / queue while starting),
    //! streamed-answer plumbing from daemon events into the shared
    //! conversation, and the modal hand-off from the `?` help panel.

    use super::super::*;
    use crate::realm::HelpQuestionKind;
    use lazybox_core::SessionKey;
    use lazybox_ipc::Event as IpcEvent;
    use lazybox_ipc::{
        AgentRunAccess, AgentRunId, AgentRuntimeMode, Command as IpcCommand, channel,
    };
    use lazybox_tui_core::help::{HELP_AGENT_PREFERENCE, HELP_SESSION_KEY};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn ask_new(
        model: &mut Model<tuirealm::terminal::TestTerminalAdapter>,
        question: &str,
    ) -> Vec<IpcCommand> {
        model.handle_help_question(question.into(), HelpQuestionKind::NewQuestion)
    }

    fn ask_follow_up(
        model: &mut Model<tuirealm::terminal::TestTerminalAdapter>,
        question: &str,
    ) -> Vec<IpcCommand> {
        model.handle_help_question(question.into(), HelpQuestionKind::FollowUp)
    }

    fn run_started(
        model: &Model<tuirealm::terminal::TestTerminalAdapter>,
        run_id: u64,
    ) -> IpcEvent {
        IpcEvent::AgentRunStarted {
            request_id: model
                .help_start_request
                .clone()
                .expect("help start request"),
            run_id: AgentRunId(run_id),
            session_key: SessionKey::new(HELP_SESSION_KEY),
            session_id: None,
            agent: HELP_AGENT_PREFERENCE[0].into(),
            mode: AgentRuntimeMode::StreamJson,
        }
    }

    /// The first question starts a headless stream-json run whose
    /// opening message is the generated context (this user's effective
    /// keymap + docs) followed by the question — no PTY, no worktree.
    #[test]
    fn first_question_starts_the_run_with_generated_context() {
        let mut m = build_model();
        let cmds = ask_new(&mut m, "how do I multi-select?");
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::StartAgentRun {
                session_key,
                agent,
                mode,
                cwd,
                initial_input,
                access,
                ..
            } => {
                assert_eq!(session_key.as_str(), HELP_SESSION_KEY);
                assert_eq!(agent, HELP_AGENT_PREFERENCE[0]);
                assert_eq!(*mode, AgentRuntimeMode::StreamJson);
                assert_eq!(*access, AgentRunAccess::ReadOnly);
                assert!(
                    cwd.is_none(),
                    "cwd is daemon policy — a client path may not exist on the daemon host",
                );
                let text = initial_input
                    .as_ref()
                    .and_then(|i| i.text.as_deref())
                    .expect("initial input text");
                assert!(text.contains("# Key bindings (effective)"));
                assert!(text.contains("# Documentation"));
                assert!(text.ends_with("# Question\n\nhow do I multi-select?"));
            }
            other => panic!("expected StartAgentRun, got {other:?}"),
        }
        assert!(m.help_start_request.is_some());
        let convo = m.help_convo_mut();
        assert_eq!(convo.turns.len(), 1);
        assert!(!convo.turns[0].done);
    }

    /// Once the run is live, a follow-up rides it as a plain input —
    /// the context is already in the conversation (and prompt-cached).
    #[test]
    fn follow_up_rides_the_same_run() {
        let mut m = build_model();
        m.help_run = Some(AgentRunId(7));
        let cmds = ask_follow_up(&mut m, "and in the sidebar?");
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::SendAgentInput { run_id, message } => {
                assert_eq!(*run_id, AgentRunId(7));
                assert_eq!(message.text.as_deref(), Some("and in the sidebar?"));
            }
            other => panic!("expected SendAgentInput, got {other:?}"),
        }
    }

    #[test]
    fn new_question_interrupts_the_run_and_starts_a_fresh_thread() {
        let mut m = build_model();
        m.help_run = Some(AgentRunId(7));
        m.help_convo_mut()
            .turns
            .push(crate::realm::components::help_ask::HelpTurn {
                question: "old question".into(),
                answer: "old answer".into(),
                done: true,
            });

        let cmds =
            m.handle_help_question("unrelated question".into(), HelpQuestionKind::NewQuestion);
        assert!(matches!(
            cmds.first(),
            Some(IpcCommand::InterruptAgentRun {
                run_id: AgentRunId(7)
            })
        ));
        assert!(matches!(
            cmds.get(1),
            Some(IpcCommand::StartAgentRun { .. })
        ));
        let convo = m.help_convo_mut();
        assert_eq!(convo.turns.len(), 1);
        assert_eq!(convo.turns[0].question, "unrelated question");
        assert!(!convo.turns[0].answer.contains("old answer"));
    }

    /// A question racing the run start queues instead of double-
    /// starting; `AgentRunStarted` flushes the queue in order.
    #[test]
    fn question_while_starting_queues_until_run_started() {
        let mut m = build_model();
        assert!(!ask_new(&mut m, "first?").is_empty());
        let cmds = ask_follow_up(&mut m, "second?");
        assert!(cmds.is_empty(), "second question must not start a run");
        assert_eq!(m.help_pending_questions, vec!["second?".to_string()]);

        let started = run_started(&m, 3);
        m.handle_daemon_event(started);
        assert_eq!(m.help_run, Some(AgentRunId(3)));
        assert!(m.help_start_request.is_none());
        assert!(m.help_pending_questions.is_empty());
    }

    #[test]
    fn new_question_while_starting_replaces_the_pending_thread() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        assert!(!ask_new(&mut m, "old question").is_empty());

        let cmds = m.handle_help_question("fresh question".into(), HelpQuestionKind::NewQuestion);
        assert!(cmds.is_empty(), "the old run has no id to interrupt yet");
        assert!(m.help_interrupt_on_start);
        assert_eq!(
            m.help_convo_mut().turns[0].question,
            "fresh question",
            "the old transcript is cleared immediately"
        );

        let started = run_started(&m, 3);
        m.handle_daemon_event(started);
        assert!(matches!(
            server.rx.try_recv(),
            Ok(IpcCommand::InterruptAgentRun {
                run_id: AgentRunId(3)
            })
        ));
        assert!(matches!(
            server.rx.try_recv(),
            Ok(IpcCommand::StartAgentRun { .. })
        ));
        assert!(m.help_start_request.is_some());
        assert_eq!(m.help_run, None);
    }

    /// Empty / whitespace questions are dropped without touching the
    /// conversation or the daemon.
    #[test]
    fn blank_question_is_a_noop() {
        let mut m = build_model();
        assert!(ask_new(&mut m, "   ").is_empty());
        assert!(m.help_convo_mut().turns.is_empty());
        assert!(m.help_start_request.is_none());
    }

    /// Streamed deltas append to the open turn; `AgentTurnFinished`
    /// replaces the accumulated text with the authoritative result and
    /// closes the turn. Events for other runs are ignored.
    #[test]
    fn deltas_and_turn_finished_stream_into_the_convo() {
        let mut m = build_model();
        let _ = ask_new(&mut m, "how do I snooze?");
        let started = run_started(&m, 1);
        m.handle_daemon_event(started);
        for delta in ["Press ", "`z`"] {
            m.handle_daemon_event(IpcEvent::AgentAssistantTextDelta {
                run_id: AgentRunId(1),
                delta: delta.into(),
            });
        }
        // A delta from an unrelated run must not leak in.
        m.handle_daemon_event(IpcEvent::AgentAssistantTextDelta {
            run_id: AgentRunId(99),
            delta: "NOISE".into(),
        });
        assert_eq!(m.help_convo_mut().turns[0].answer, "Press `z`");

        m.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(1),
            result: Some("Press `z` on a workspace.".into()),
            session_id: None,
            error: None,
        });
        let convo = m.help_convo_mut();
        assert!(convo.turns[0].done);
        assert_eq!(convo.turns[0].answer, "Press `z` on a workspace.");
    }

    /// Answers correlate to the *earliest* open turn: a follow-up
    /// submitted while the previous answer is still streaming must not
    /// hijack its tail, and each turn gets its own result.
    #[test]
    fn follow_up_mid_stream_keeps_turns_correlated() {
        let mut m = build_model();
        let _ = ask_new(&mut m, "q1");
        let started = run_started(&m, 1);
        m.handle_daemon_event(started);
        m.handle_daemon_event(IpcEvent::AgentAssistantTextDelta {
            run_id: AgentRunId(1),
            delta: "A1 start".into(),
        });
        // Follow-up while A1 is still streaming.
        let cmds = ask_follow_up(&mut m, "q2");
        assert!(matches!(
            cmds.first(),
            Some(IpcCommand::SendAgentInput { .. })
        ));
        m.handle_daemon_event(IpcEvent::AgentAssistantTextDelta {
            run_id: AgentRunId(1),
            delta: ", A1 end".into(),
        });
        {
            let convo = m.help_convo_mut();
            assert_eq!(convo.turns[0].answer, "A1 start, A1 end");
            assert_eq!(convo.turns[1].answer, "", "A1's tail must not leak into q2");
        }
        m.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(1),
            result: Some("A1 final".into()),
            session_id: None,
            error: None,
        });
        m.handle_daemon_event(IpcEvent::AgentAssistantTextDelta {
            run_id: AgentRunId(1),
            delta: "A2".into(),
        });
        m.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(1),
            result: Some("A2 final".into()),
            session_id: None,
            error: None,
        });
        let convo = m.help_convo_mut();
        assert!(convo.turns[0].done);
        assert_eq!(convo.turns[0].answer, "A1 final");
        assert!(convo.turns[1].done, "q2's own result must close q2");
        assert_eq!(convo.turns[1].answer, "A2 final");
    }

    /// `AgentRunFinished` releases the run id so the next question
    /// starts a fresh run instead of writing to a dead process — and
    /// closes every open turn, including follow-ups queued behind the
    /// one that was streaming.
    #[test]
    fn run_finished_resets_for_a_fresh_start() {
        let mut m = build_model();
        let _ = ask_new(&mut m, "q");
        let started = run_started(&m, 1);
        m.handle_daemon_event(started);
        let _ = ask_follow_up(&mut m, "follow-up");
        m.handle_daemon_event(IpcEvent::AgentRunFinished {
            run_id: AgentRunId(1),
            exit_code: Some(1),
            error: Some("boom".into()),
        });
        assert_eq!(m.help_run, None);
        assert!(m.help_start_request.is_none());
        {
            let convo = m.help_convo_mut();
            assert!(convo.turns[0].done, "open turn closed on exit");
            assert!(convo.turns[1].done, "queued follow-up turn closed too");
            assert!(convo.notice.as_deref().unwrap_or("").contains("boom"));
        }
        let cmds = ask_new(&mut m, "again?");
        assert!(
            matches!(cmds.first(), Some(IpcCommand::StartAgentRun { .. })),
            "next question restarts the run: {cmds:?}",
        );
    }

    #[test]
    fn run_finished_changes_the_mounted_input_to_a_new_question() {
        use crate::realm::components::help_ask::HelpAsk;
        use tuirealm::component::AppComponent;
        use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};

        let mut m = build_model();
        let _ = ask_new(&mut m, "q");
        let started = run_started(&m, 1);
        m.handle_daemon_event(started);
        let mut help = HelpAsk::new(
            m.catalog.clone(),
            m.help_convo.clone(),
            m.ui_defaults.terminal_escape_char,
        );

        m.handle_daemon_event(IpcEvent::AgentRunFinished {
            run_id: AgentRunId(1),
            exit_code: Some(1),
            error: Some("boom".into()),
        });
        for ch in "again?".chars() {
            let _ = help.on(&Event::Keyboard(KeyEvent::new(
                Key::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        let submitted = help.on(&Event::Keyboard(KeyEvent::new(
            Key::Enter,
            KeyModifiers::NONE,
        )));
        let Some(Msg::HelpAsked(question, HelpQuestionKind::NewQuestion)) = submitted else {
            panic!("dead run must expose the next input as a new question: {submitted:?}");
        };

        let cmds = m.handle_help_question(question, HelpQuestionKind::NewQuestion);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::StartAgentRun { .. }]
        ));
        let convo = m.help_convo_mut();
        assert_eq!(convo.turns.len(), 1);
        assert_eq!(convo.turns[0].question, "again?");
    }

    /// A correlated spawn failure surfaces inside the help conversation,
    /// not as a footer sync-error banner, and closes every queued turn.
    #[test]
    fn spawn_failure_lands_in_the_convo_not_the_footer() {
        let mut m = build_model();
        m.status.polling = None;
        let _ = ask_new(&mut m, "q");
        let _ = ask_follow_up(&mut m, "queued while starting");
        let request_id = m.help_start_request.clone().expect("start request");
        m.handle_daemon_event(IpcEvent::AgentRunStartFailed {
            request_id,
            message: "No such file or directory".into(),
        });
        assert!(m.help_start_request.is_none());
        let convo = m.help_convo_mut();
        assert!(convo.turns[0].done);
        assert!(convo.turns[1].done, "queued turn must not spin forever");
        let notice = convo.notice.as_deref().expect("notice set");
        assert!(notice.contains("unavailable"));
        drop(convo);
        assert!(
            m.status.notice.is_none(),
            "agent_run errors must not raise a footer banner"
        );
        assert_eq!(
            m.status.sync.recent().count(),
            0,
            "and must not hit the sync log"
        );
    }

    /// Once the run is live, a generic `agent_run*` provider error can
    /// belong to any structured run on the shared bus (it carries no
    /// run id) — it must NOT be claimed for the help conversation. Run
    /// death arrives run-scoped as `AgentRunFinished` instead.
    #[test]
    fn live_run_does_not_claim_other_runs_agent_run_errors() {
        let mut m = build_model();
        let _ = ask_new(&mut m, "q");
        let started = run_started(&m, 1);
        m.handle_daemon_event(started);
        m.handle_daemon_event(IpcEvent::ProviderError {
            source: "agent_run:stdin".into(),
            message: "broken pipe on someone else's run".into(),
            detail: String::new(),
            kind: "retryable".into(),
        });
        assert_eq!(m.help_run, Some(AgentRunId(1)), "run must stay adopted");
        let convo = m.help_convo_mut();
        assert!(
            !convo.turns[0].done,
            "the streaming answer must not be truncated by an unrelated error"
        );
        assert!(convo.notice.is_none());
    }

    /// Codex is the structured fallback when Claude is not enabled.
    #[test]
    fn codex_only_configuration_starts_help_with_codex() {
        let mut m = build_model();
        m.set_agents(vec!["codex".into()]);
        let cmds = ask_new(&mut m, "q");
        assert!(matches!(
            cmds.first(),
            Some(IpcCommand::StartAgentRun { agent, .. }) if agent == "codex"
        ));
        assert!(m.help_convo_mut().notice.is_none());
    }

    /// With both structured adapters available, Ask follows the user's
    /// configured work-agent preference instead of hardcoding Claude.
    #[test]
    fn configured_default_agent_wins_when_it_is_structured() {
        let mut m = build_model();
        m.set_agents(vec!["claude".into(), "codex".into()]);
        m.set_default_agent("codex");
        let cmds = ask_new(&mut m, "q");
        assert!(matches!(
            cmds.first(),
            Some(IpcCommand::StartAgentRun { agent, .. }) if agent == "codex"
        ));
    }

    /// An enabled PTY-only agent cannot be fed structured help turns:
    /// close the turn with a useful notice and leave fuzzy search live.
    #[test]
    fn no_structured_help_agent_sets_notice_instead_of_dispatching() {
        let mut m = build_model();
        m.set_agents(vec!["cursor-agent".into()]);
        let cmds = ask_new(&mut m, "q");
        assert!(cmds.is_empty());
        let convo = m.help_convo_mut();
        assert!(convo.turns[0].done);
        let notice = convo.notice.as_deref().unwrap_or("");
        assert!(notice.contains("claude"), "got: {notice}");
        assert!(notice.contains("codex"), "got: {notice}");
    }

    /// Ask and the compact shortcut index swap in both directions;
    /// asking a question keeps Ask mounted so the answer can stream in.
    #[test]
    fn help_ask_open_swaps_the_help_panel() {
        let mut m = build_model();
        m.mount_help();
        assert_eq!(m.modal_stack.last(), Some(&Id::Help));
        m.update(Msg::HelpAskOpen);
        assert_eq!(m.modal_stack.as_slice(), &[Id::HelpAsk]);
        m.update(Msg::HelpIndexOpen);
        assert_eq!(m.modal_stack.as_slice(), &[Id::Help]);
        m.update(Msg::HelpAskOpen);
        assert_eq!(m.modal_stack.as_slice(), &[Id::HelpAsk]);
        m.update(Msg::HelpAsked(
            "how do I merge?".into(),
            HelpQuestionKind::NewQuestion,
        ));
        assert_eq!(
            m.modal_stack.as_slice(),
            &[Id::HelpAsk],
            "asking must not dismiss the modal",
        );
        assert_eq!(m.help_convo_mut().turns.len(), 1);
        m.update(Msg::HelpIndexOpen);
        assert_eq!(m.modal_stack.as_slice(), &[Id::Help]);
        assert_eq!(m.help_convo_mut().turns.len(), 1);
        m.update(Msg::HelpAskOpen);
        assert_eq!(m.modal_stack.as_slice(), &[Id::HelpAsk]);
        assert_eq!(
            m.help_convo_mut().turns.len(),
            1,
            "switching Help surfaces keeps the active conversation",
        );
    }

    #[test]
    fn explicit_help_exit_interrupts_and_resets_the_conversation() {
        let mut m = build_model();
        m.mount_help_ask();
        m.help_run = Some(AgentRunId(7));
        m.help_convo_mut()
            .turns
            .push(crate::realm::components::help_ask::HelpTurn {
                question: "q".into(),
                answer: "a".into(),
                done: true,
            });

        let cmds = m.handle_modal_dismissed();
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::InterruptAgentRun {
                run_id: AgentRunId(7)
            }]
        ));
        assert!(m.modal_stack.is_empty());
        assert!(m.help_convo_mut().turns.is_empty());
        assert_eq!(m.help_run, None);

        m.mount_help_ask();
        let cmds = m.handle_help_question("fresh".into(), HelpQuestionKind::NewQuestion);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::StartAgentRun { .. }]
        ));
    }

    #[test]
    fn exit_during_start_interrupts_the_run_when_its_id_arrives() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.mount_help_ask();
        assert!(!ask_new(&mut m, "q").is_empty());
        assert!(m.handle_modal_dismissed().is_empty());
        assert!(m.help_interrupt_on_start);
        assert!(m.help_convo_mut().turns.is_empty());

        let started = run_started(&m, 7);
        m.handle_daemon_event(started);
        assert_eq!(m.help_run, None);
        assert!(m.help_start_request.is_none());
        assert!(!m.help_interrupt_on_start);
        assert!(matches!(
            server.rx.try_recv(),
            Ok(IpcCommand::InterruptAgentRun {
                run_id: AgentRunId(7)
            })
        ));
    }

    #[test]
    fn exit_during_start_ignores_other_clients_run_outcomes() {
        let (client, mut server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.mount_help_ask();
        assert!(!ask_new(&mut m, "q").is_empty());
        assert!(m.handle_modal_dismissed().is_empty());
        let own_request = m
            .help_start_request
            .clone()
            .expect("own pending start request");
        let foreign_request = lazybox_ipc::AgentRunRequestId("other-client".into());

        m.handle_daemon_event(IpcEvent::AgentRunStartFailed {
            request_id: foreign_request.clone(),
            message: "other client failed".into(),
        });
        m.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id: foreign_request,
            run_id: AgentRunId(8),
            session_key: SessionKey::new(HELP_SESSION_KEY),
            session_id: None,
            agent: HELP_AGENT_PREFERENCE[0].into(),
            mode: AgentRuntimeMode::StreamJson,
        });
        assert_eq!(m.help_start_request.as_ref(), Some(&own_request));
        assert!(
            server.rx.try_recv().is_err(),
            "another client's run must not be interrupted"
        );

        m.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id: own_request,
            run_id: AgentRunId(7),
            session_key: SessionKey::new(HELP_SESSION_KEY),
            session_id: None,
            agent: HELP_AGENT_PREFERENCE[0].into(),
            mode: AgentRuntimeMode::StreamJson,
        });
        assert!(matches!(
            server.rx.try_recv(),
            Ok(IpcCommand::InterruptAgentRun {
                run_id: AgentRunId(7)
            })
        ));
    }

    // ── Ask Lazybox actions (#353) ──────────────────────────────────

    use super::ENV_LOCK;

    /// A finished answer carrying an `add_snippet` block.
    fn add_snippet_answer(key: &str) -> String {
        format!(
            "I'll add that snippet.\n\n```lazybox-action\n\
{{\"action\":\"add_snippet\",\"key\":\"{key}\",\"category\":\"Review\",\
\"description\":\"Integrate feedback\",\"body\":\"Address the review and commit.\"}}\n\
```\n\nSend it with ]]s{key}.",
        )
    }

    fn finish_help_turn(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>, answer: String) {
        let started = run_started(m, 1);
        m.handle_daemon_event(started);
        m.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(1),
            result: Some(answer),
            session_id: None,
            error: None,
        });
    }

    /// The motivating flow: asking to add a snippet produces a
    /// confirm-with-preview, stashes the intent, and hides the raw
    /// action JSON from the transcript.
    #[test]
    fn add_snippet_action_proposes_a_confirm_and_strips_the_block() {
        let mut m = build_model();
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "add a snippet");
        finish_help_turn(&mut m, add_snippet_answer("integrate"));

        assert_eq!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));
        let Some(super::super::ModalFlow::HelpAction { intent, .. }) = m.modal_flow.clone() else {
            panic!("intent stashed");
        };
        match intent {
            lazybox_tui_core::help::HelpActionIntent::AddSnippet {
                key,
                category,
                body,
                ..
            } => {
                assert_eq!(key, "integrate");
                assert_eq!(category, "Review");
                assert_eq!(body, "Address the review and commit.");
            }
            other => panic!("expected AddSnippet, got {other:?}"),
        }

        let convo = m.help_convo_mut();
        assert!(
            !convo.turns[0].answer.contains("lazybox-action"),
            "raw action block must not show in the transcript",
        );
        assert!(convo.turns[0].answer.contains("I'll add that snippet."));
        assert!(convo.turns[0].answer.contains("Send it with ]]sintegrate."));
    }

    /// Declining the confirm drops the stash and writes nothing —
    /// control returns to the help modal.
    #[test]
    fn declining_the_snippet_confirm_changes_nothing() {
        let mut m = build_model();
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "add a snippet");
        finish_help_turn(&mut m, add_snippet_answer("integrate"));
        assert_eq!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));

        let before = m.snippets.len();
        let _ = m.handle_modal_dismissed();
        assert!(m.modal_flow.is_none());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::HelpAsk),
            "back to Ask Lazybox"
        );
        assert_eq!(
            m.snippets.len(),
            before,
            "nothing added to the live catalog"
        );
        assert!(m.snippets.get("integrate").is_none());
    }

    /// Accepting writes the snippet to the global file and hot-reloads
    /// the catalog so `]]s<key>` works immediately — no restart (#353).
    #[test]
    fn confirming_writes_the_snippet_and_hot_reloads() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("lazybox-help-snippet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator in this
        // binary, so this single-writer mutation can't race.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let mut m = build_model();
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "add a snippet");
        finish_help_turn(&mut m, add_snippet_answer("integrate"));
        assert_eq!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));

        let _ = m.handle_confirmed(true);

        assert!(
            home.join("snippets.yaml").exists(),
            "written to the global file"
        );
        let s = m
            .snippets
            .get("integrate")
            .expect("hot-reloaded into the live catalog");
        assert_eq!(s.body, "Address the review and commit.");
        assert_eq!(m.modal_stack.last(), Some(&Id::HelpAsk), "confirm popped");

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The run outlives the modal — an action that resolves after the
    /// user closed Ask must not pop a surprise confirm. The raw JSON is
    /// still stripped (no intent leaks into the transcript), but because
    /// the action was dropped a short "not applied" note replaces it, so
    /// a reopened transcript never reads as if the action ran.
    #[test]
    fn dropped_action_strips_block_and_notes_it_was_not_applied() {
        let mut m = build_model();
        let _ = ask_new(&mut m, "add a snippet");
        finish_help_turn(&mut m, add_snippet_answer("integrate"));

        assert!(m.modal_flow.is_none());
        assert_ne!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));
        let answer = m.help_convo_mut().turns[0].answer.clone();
        assert!(
            !answer.contains("lazybox-action"),
            "raw block stripped even when the action was dropped: {answer:?}",
        );
        assert!(
            answer.contains("ask again to apply"),
            "dropped action leaves a not-applied note: {answer:?}",
        );
    }

    // ── Ask Lazybox: scaffold a skill (#799) ────────────────────────

    /// A finished answer carrying a `scaffold_skill` block.
    fn scaffold_skill_answer(name: &str) -> String {
        format!(
            "I'll scaffold that skill.\n\n```lazybox-action\n\
{{\"action\":\"scaffold_skill\",\"name\":\"{name}\",\
\"description\":\"Draft release notes from merged PRs\",\
\"body\":\"1. Find the last tag.\\n2. List the merged PRs since it.\"}}\n\
```\n\nThe agent can pick it up on its own.",
        )
    }

    /// Seed a focused workspace whose one session's worktree is
    /// `worktree`, so `skill_scaffold_root` resolves there instead of
    /// the process cwd (which would pollute the real repo).
    fn model_with_worktree(
        worktree: &std::path::Path,
    ) -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let mut m = build_model();
        let ws_key = lazybox_core::WorkspaceKey::new("github:o/r#1");
        let mut ws = lazybox_core::Workspace::empty(ws_key.clone(), "main", chrono::Utc::now());
        ws.add_session(lazybox_core::WorkspaceSession::new(
            ws_key.clone(),
            lazybox_core::SessionKind::Shell,
            worktree.to_path_buf(),
            chrono::Utc::now(),
        ));
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![ws],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let session_key: lazybox_core::SessionKey = (&ws_key).into();
        assert!(
            m.sidebar.focus_workspace_key(&session_key),
            "seeded workspace should be selectable",
        );
        m
    }

    /// Asking for a multi-step capability scaffolds a skill: a
    /// confirm-with-preview naming the destination folder, the intent
    /// stashed, and the raw action JSON stripped from the transcript.
    #[test]
    fn scaffold_skill_action_proposes_a_confirm_and_strips_the_block() {
        let mut m = build_model();
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "make a skill for release notes");
        finish_help_turn(&mut m, scaffold_skill_answer("lazybox-799-release-notes"));

        assert_eq!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));
        let Some(super::super::ModalFlow::HelpAction { intent, .. }) = m.modal_flow.clone() else {
            panic!("intent stashed");
        };
        match intent {
            lazybox_tui_core::help::HelpActionIntent::ScaffoldSkill {
                name,
                description,
                body,
            } => {
                assert_eq!(name, "lazybox-799-release-notes");
                assert_eq!(description, "Draft release notes from merged PRs");
                assert!(body.contains("Find the last tag"));
            }
            other => panic!("expected ScaffoldSkill, got {other:?}"),
        }
        assert!(
            !m.help_convo_mut().turns[0]
                .answer
                .contains("lazybox-action"),
            "raw action block must not show in the transcript",
        );
    }

    /// Accepting writes `.claude/skills/<name>/SKILL.md` into the
    /// focused workspace's worktree, with frontmatter built from the
    /// name + description.
    #[test]
    fn confirming_scaffolds_the_skill_folder() {
        let root = std::env::temp_dir().join(format!("lazybox-help-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let mut m = model_with_worktree(&root);
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "make a skill for release notes");
        finish_help_turn(&mut m, scaffold_skill_answer("release-notes"));
        assert_eq!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));

        let _ = m.handle_confirmed(true);

        let skill_md = root.join(".claude/skills/release-notes/SKILL.md");
        let written = std::fs::read_to_string(&skill_md).expect("SKILL.md written");
        assert!(written.contains("name: release-notes"));
        assert!(written.contains("description: Draft release notes from merged PRs"));
        assert!(written.contains("Find the last tag"));
        assert_eq!(m.modal_stack.last(), Some(&Id::HelpAsk), "confirm popped");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The skill is written to the destination the confirm previewed,
    /// even if the sidebar selection moves to a different workspace
    /// while the confirm is open (a daemon snapshot can do that). The
    /// root is captured at propose time; apply must not re-resolve it
    /// against the now-focused workspace (#799).
    #[test]
    fn scaffold_writes_to_the_previewed_root_after_selection_moves() {
        let base =
            std::env::temp_dir().join(format!("lazybox-help-skill-move-{}", std::process::id()));
        let root_a = base.join("a");
        let root_b = base.join("b");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();

        let mut m = build_model();
        let a_key = lazybox_core::WorkspaceKey::new("github:o/r#1");
        let b_key = lazybox_core::WorkspaceKey::new("github:o/r#2");
        let mk = |key: &lazybox_core::WorkspaceKey, wt: &std::path::Path| {
            let mut ws = lazybox_core::Workspace::empty(key.clone(), "main", chrono::Utc::now());
            ws.add_session(lazybox_core::WorkspaceSession::new(
                key.clone(),
                lazybox_core::SessionKind::Shell,
                wt.to_path_buf(),
                chrono::Utc::now(),
            ));
            ws
        };
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![mk(&a_key, &root_a), mk(&b_key, &root_b)],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let a_sk: lazybox_core::SessionKey = (&a_key).into();
        assert!(m.sidebar.focus_workspace_key(&a_sk), "workspace A focused");

        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "make a skill for release notes");
        finish_help_turn(&mut m, scaffold_skill_answer("release-notes"));
        assert_eq!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));

        // Selection drifts to workspace B under the open confirm.
        let b_sk: lazybox_core::SessionKey = (&b_key).into();
        assert!(m.sidebar.focus_workspace_key(&b_sk), "selection moved to B");

        let _ = m.handle_confirmed(true);

        assert!(
            root_a
                .join(".claude/skills/release-notes/SKILL.md")
                .exists(),
            "written to the previewed root (A)",
        );
        assert!(
            !root_b
                .join(".claude/skills/release-notes/SKILL.md")
                .exists(),
            "must not follow the drifted selection to B",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The confirm preview names *how* the destination was resolved, so
    /// a launch-directory fallback is never silently mistaken for the
    /// focused workspace's own repo (#799).
    #[test]
    fn skill_scaffold_preview_names_its_source() {
        use super::super::inputs::SkillScaffoldRoot;
        use super::super::modals::skill_scaffold_preview;

        let wt = SkillScaffoldRoot::Worktree(std::path::PathBuf::from("/repo/wt"));
        let p = skill_scaffold_preview(&wt, "release-notes", "Draft notes", "steps");
        assert!(
            p.contains("the focused workspace's worktree"),
            "preview: {p}"
        );
        assert!(p.contains("release-notes"));
        assert!(p.contains("SKILL.md"));
        assert!(!p.contains("launch directory"));

        let ld = SkillScaffoldRoot::LaunchDir(std::path::PathBuf::from("/launch/dir"));
        let p = skill_scaffold_preview(&ld, "release-notes", "Draft notes", "steps");
        assert!(p.contains("your launch directory"), "preview: {p}");
    }

    /// An already-scaffolded skill is never clobbered: the proposal is
    /// rejected with a notice instead of a confirm, and the existing
    /// SKILL.md is left untouched.
    #[test]
    fn scaffold_skill_refuses_to_overwrite_and_notices_it() {
        let root =
            std::env::temp_dir().join(format!("lazybox-help-skill-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        lazybox_config::scaffold_skill(&root, "release-notes", "old", "old body").unwrap();

        let mut m = model_with_worktree(&root);
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "make a skill for release notes");
        finish_help_turn(&mut m, scaffold_skill_answer("release-notes"));

        assert!(m.modal_flow.is_none());
        assert_ne!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));
        assert!(
            m.help_convo_mut()
                .notice
                .as_deref()
                .unwrap_or("")
                .contains("already exists"),
            "notice: {:?}",
            m.help_convo_mut().notice,
        );
        let written =
            std::fs::read_to_string(root.join(".claude/skills/release-notes/SKILL.md")).unwrap();
        assert!(written.contains("old body"), "existing skill untouched");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A skill name that isn't a clean folder id is rejected at the
    /// boundary: no confirm, a notice, and nothing on disk.
    #[test]
    fn invalid_skill_name_sets_a_notice_and_no_confirm() {
        let mut m = build_model();
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "make a skill");
        finish_help_turn(
            &mut m,
            "```lazybox-action\n{\"action\":\"scaffold_skill\",\"name\":\"Bad Name\",\
\"description\":\"d\",\"body\":\"b\"}\n```"
                .into(),
        );

        assert!(m.modal_flow.is_none());
        assert_ne!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));
        assert!(
            m.help_convo_mut()
                .notice
                .as_deref()
                .unwrap_or("")
                .contains("invalid skill name"),
            "notice: {:?}",
            m.help_convo_mut().notice,
        );
    }

    /// A malformed intent (whitespace key) is rejected at the boundary:
    /// no confirm, and a notice explains why nothing happened.
    #[test]
    fn invalid_snippet_key_sets_a_notice_and_no_confirm() {
        let mut m = build_model();
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "add a snippet");
        finish_help_turn(
            &mut m,
            "```lazybox-action\n{\"action\":\"add_snippet\",\"key\":\"has space\",\"body\":\"x\"}\n```"
                .into(),
        );

        assert!(m.modal_flow.is_none());
        assert_ne!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));
        let convo = m.help_convo_mut();
        assert!(
            convo
                .notice
                .as_deref()
                .unwrap_or("")
                .contains("invalid key"),
            "notice: {:?}",
            convo.notice,
        );
    }

    fn edit_config_answer(key: &str, value: &str) -> String {
        format!(
            "Sure.\n\n```lazybox-action\n{{\"action\":\"edit_config\",\"key\":\"{key}\",\"value\":\"{value}\"}}\n```",
        )
    }

    /// `validate_config_edit` is the security boundary: it canonicalizes
    /// a theme's case, flags restart-only keys, and rejects anything
    /// off the allowlist or with an unknown value.
    #[test]
    fn validate_config_edit_enforces_the_allowlist() {
        let mut m = build_model();
        m.set_agents(vec!["claude".into(), "codex".into()]);

        // Theme: case-insensitive match, canonicalized to the registered
        // spelling, applies live.
        let edit = m
            .validate_config_edit("ui.theme", "lazybox light")
            .expect("known theme");
        assert_eq!(edit.value, "Lazybox Light");
        assert!(!edit.needs_restart);

        // default_agent: must be enabled.
        assert!(
            m.validate_config_edit("setup.default_agent", "codex")
                .is_ok()
        );
        assert!(
            m.validate_config_edit("setup.default_agent", "nope")
                .is_err()
        );

        // keymap preset: valid but restart-only.
        let km = m
            .validate_config_edit("ui.keymap_preset", "vim")
            .expect("vim preset");
        assert!(km.needs_restart);
        assert!(m.validate_config_edit("ui.keymap_preset", "emacs").is_err());

        // Off-allowlist key and unknown theme value are rejected.
        assert!(
            m.validate_config_edit("agent.skip_permissions", "true")
                .is_err()
        );
        assert!(m.validate_config_edit("ui.theme", "Nonexistent").is_err());
    }

    /// An `edit_config` for an allowlisted key proposes a
    /// confirm-with-preview; accepting persists it to `config.yaml`.
    #[test]
    fn edit_config_proposes_confirm_and_persists() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("lazybox-help-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator here.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let mut m = build_model();
        m.set_agents(vec!["claude".into(), "codex".into()]);
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "use codex by default");
        finish_help_turn(&mut m, edit_config_answer("setup.default_agent", "codex"));

        assert_eq!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));
        let _ = m.handle_confirmed(true);

        let cfg = lazybox_config::Config::load_from(&home.join("config.yaml")).expect("config");
        assert_eq!(cfg.setup.default_agent.as_deref(), Some("codex"));
        assert_eq!(m.modal_stack.last(), Some(&Id::HelpAsk), "confirm popped");

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// An off-allowlist config key never reaches a confirm — it's
    /// rejected with a conversation notice, nothing is written.
    #[test]
    fn edit_config_off_allowlist_key_is_rejected() {
        let mut m = build_model();
        m.update(Msg::HelpAskOpen);
        let _ = ask_new(&mut m, "disable permissions");
        finish_help_turn(&mut m, edit_config_answer("agent.skip_permissions", "true"));

        assert!(m.modal_flow.is_none());
        assert_ne!(m.modal_stack.last(), Some(&Id::HelpActionConfirm));
        assert!(
            m.help_convo_mut()
                .notice
                .as_deref()
                .unwrap_or("")
                .contains("isn't an editable config key"),
        );
    }
}

#[cfg(test)]
mod pr_chat_tests {
    //! Effect contracts for "Ask about this PR" (#945): the opening
    //! question is held for the worktree-diff read, the started run's
    //! context carries the PR metadata + diff, and streamed deltas land
    //! in the shared conversation the `PrChat` modal renders.

    use super::super::*;
    use crate::realm::HelpQuestionKind;
    use chrono::Utc;
    use lazybox_core::{
        Activity, ActivityKind, CiStatus, Mergeable, ReviewStatus, Task, TaskId, TaskKind,
        TaskRole, TaskState, Workspace, WorkspaceKey,
    };
    use lazybox_ipc::Event as IpcEvent;
    use lazybox_ipc::{
        AgentRunId, Command as IpcCommand, DiffFileDto, DiffHunkDto, DiffLineDto, DiffLineKindDto,
        WorkspaceDiffDto, WorkspaceDiffTarget, channel,
    };
    use std::path::PathBuf;

    fn build() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
    ) {
        let (client, server) = channel::pair();
        let model = Model::new_for_test(client, tuirealm::ratatui::layout::Size::new(120, 40))
            .expect("model init");
        (model, server)
    }

    fn pr_task() -> Task {
        Task {
            author: "octocat".into(),
            id: TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            },
            title: "Add retry to the poller".into(),
            body: Some("Retries transient poll failures.".into()),
            state: TaskState::Open,
            role: TaskRole::Reviewer,
            ci: CiStatus::Failure,
            review: ReviewStatus::ChangesRequested,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()),
            branch: Some("feat/retry".into()),
            base_branch: Some("main".into()),
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Conflicting,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 4,
            deletions: 0,
            changed_files: 0,
            kind: Some(TaskKind::Pr),
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    /// A PR workspace with a linked checkout, so `open_pr_chat` requests
    /// a diff (`WorkspaceDiffTarget::LinkedCheckout`).
    fn seed_pr_workspace(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) -> WorkspaceKey {
        let mut ws = Workspace::from_task(pr_task(), Utc::now());
        ws.key = WorkspaceKey::new("github:o/r#1");
        ws.linked_checkout = Some(PathBuf::from("/tmp/o-r"));
        ws.activity = vec![Activity {
            author: "reviewer1".into(),
            body: "Needs a test.".into(),
            created_at: Utc::now(),
            kind: ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }];
        let key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::Snapshot {
            workspaces: vec![ws],
            terminals: vec![],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        key
    }

    fn sample_diff() -> WorkspaceDiffDto {
        WorkspaceDiffDto {
            status: vec![],
            stat: vec![" src/poll.rs | 4 ++++".into()],
            files: vec![DiffFileDto {
                old_path: None,
                path: "src/poll.rs".into(),
                headers: vec![],
                hunks: vec![DiffHunkDto {
                    header: "@@ -1,1 +1,2 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![DiffLineDto {
                        kind: DiffLineKindDto::Addition,
                        text: "    retry(3);".into(),
                        old_line: None,
                        new_line: Some(2),
                    }],
                }],
            }],
            truncated: false,
        }
    }

    fn drain(server: &mut lazybox_ipc::Connection) -> Vec<IpcCommand> {
        std::iter::from_fn(|| server.rx.try_recv().ok()).collect()
    }

    /// `a` from the reader opens the chat scoped to the focused PR and
    /// fires the diff read that grounds it.
    #[test]
    fn open_mounts_the_modal_and_requests_the_diff() {
        let (mut m, mut server) = build();
        let key = seed_pr_workspace(&mut m);
        let _ = drain(&mut server);

        m.open_pr_chat();
        assert_eq!(m.modal_stack.last(), Some(&Id::PrChat));
        let inspected = drain(&mut server).into_iter().any(|cmd| {
            matches!(
                cmd,
                IpcCommand::InspectWorkspaceDiff { workspace_key, target }
                    if workspace_key == key && target == WorkspaceDiffTarget::LinkedCheckout
            )
        });
        assert!(inspected, "opening must request the worktree diff");
    }

    /// The opening question waits for the diff, then starts a run whose
    /// context carries the PR metadata AND the diff hunks; a streamed
    /// delta and the final result land in the shared conversation.
    #[test]
    fn opening_question_waits_for_diff_then_streams_answer() {
        let (mut m, mut server) = build();
        seed_pr_workspace(&mut m);
        let _ = drain(&mut server);
        m.open_pr_chat();
        let _ = drain(&mut server);

        // Diff still pending → the question is held, no run yet.
        let cmds = m.handle_pr_chat_question("what changed?".into(), HelpQuestionKind::NewQuestion);
        assert!(cmds.is_empty(), "question must wait for the diff");
        assert!(m.pr_chat_held_question.is_some());
        assert!(m.pr_chat_run.is_none());

        // Diff lands → the held question starts the run.
        m.handle_daemon_event(IpcEvent::WorkspaceDiffInspected {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            target: WorkspaceDiffTarget::LinkedCheckout,
            agent_terminal_ids: vec![],
            diff: Some(sample_diff()),
            error: None,
        });
        let start = drain(&mut server)
            .into_iter()
            .find_map(|cmd| match cmd {
                IpcCommand::StartAgentRun { initial_input, .. } => initial_input,
                _ => None,
            })
            .expect("diff reply must start the run");
        let context = start.text.expect("run carries a text turn");
        assert!(
            context.contains("Add retry to the poller"),
            "PR title in context"
        );
        assert!(context.contains("CI: failing"), "PR metadata in context");
        assert!(context.contains("reviewer1 (comment): Needs a test."));
        assert!(context.contains("+    retry(3);"), "diff hunk in context");
        assert!(context.contains("# Question\n\nwhat changed?"));

        // The run comes up and streams its answer into the transcript.
        let request_id = m.pr_chat_request.clone().expect("start request pending");
        m.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id,
            run_id: AgentRunId(7),
            session_key: lazybox_core::SessionKey::new(
                lazybox_tui_core::pr_chat::PR_CHAT_SESSION_KEY,
            ),
            session_id: None,
            agent: "claude".into(),
            mode: lazybox_ipc::AgentRuntimeMode::StreamJson,
        });
        assert_eq!(m.pr_chat_run, Some(AgentRunId(7)));

        m.handle_daemon_event(IpcEvent::AgentAssistantTextDelta {
            run_id: AgentRunId(7),
            delta: "The poller now ".into(),
        });
        assert_eq!(m.pr_chat_convo_mut().turns[0].answer, "The poller now ");

        m.handle_daemon_event(IpcEvent::AgentTurnFinished {
            run_id: AgentRunId(7),
            result: Some("The poller now retries transient failures — `src/poll.rs:2`.".into()),
            session_id: None,
            error: None,
        });
        let convo = m.pr_chat_convo_mut();
        assert!(convo.turns[0].done);
        assert!(convo.turns[0].answer.contains("src/poll.rs:2"));
    }

    /// A second question asked while the opening one is still held for
    /// the diff must not clobber it: the first stays the context-bearing
    /// turn, the second queues and is flushed as a follow-up once the run
    /// starts. (Regression: a lone `held_question` slot dropped the first.)
    #[test]
    fn second_question_during_diff_wait_queues_instead_of_clobbering() {
        let (mut m, mut server) = build();
        seed_pr_workspace(&mut m);
        let _ = drain(&mut server);
        m.open_pr_chat();
        let _ = drain(&mut server);

        let _ = m.handle_pr_chat_question("what changed?".into(), HelpQuestionKind::NewQuestion);
        let _ = m.handle_pr_chat_question("and why?".into(), HelpQuestionKind::FollowUp);
        assert_eq!(m.pr_chat_convo_mut().turns.len(), 2);

        m.handle_daemon_event(IpcEvent::WorkspaceDiffInspected {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            target: WorkspaceDiffTarget::LinkedCheckout,
            agent_terminal_ids: vec![],
            diff: Some(sample_diff()),
            error: None,
        });
        // The held (first) question opens the run with the context.
        let start = drain(&mut server)
            .into_iter()
            .find_map(|cmd| match cmd {
                IpcCommand::StartAgentRun { initial_input, .. } => initial_input,
                _ => None,
            })
            .expect("held question starts the run");
        assert!(start.text.unwrap().contains("# Question\n\nwhat changed?"));

        // Bringing the run up flushes the queued second question as input.
        let request_id = m.pr_chat_request.clone().expect("start request");
        m.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id,
            run_id: AgentRunId(9),
            session_key: lazybox_core::SessionKey::new(
                lazybox_tui_core::pr_chat::PR_CHAT_SESSION_KEY,
            ),
            session_id: None,
            agent: "claude".into(),
            mode: lazybox_ipc::AgentRuntimeMode::StreamJson,
        });
        let sent_follow_up = drain(&mut server).into_iter().any(|cmd| {
            matches!(
                cmd,
                IpcCommand::SendAgentInput { message, .. }
                    if message.text.as_deref() == Some("and why?")
            )
        });
        assert!(sent_follow_up, "queued question must flush as a follow-up");
    }

    /// Follow-ups ride the live run rather than restarting it.
    #[test]
    fn follow_up_sends_input_to_the_live_run() {
        let (mut m, mut server) = build();
        seed_pr_workspace(&mut m);
        let _ = drain(&mut server);
        m.open_pr_chat();
        m.pr_chat_diff = Some(Some(sample_diff()));
        let _ = m.handle_pr_chat_question("first?".into(), HelpQuestionKind::NewQuestion);
        let request_id = m.pr_chat_request.clone().expect("start request");
        m.handle_daemon_event(IpcEvent::AgentRunStarted {
            request_id,
            run_id: AgentRunId(3),
            session_key: lazybox_core::SessionKey::new(
                lazybox_tui_core::pr_chat::PR_CHAT_SESSION_KEY,
            ),
            session_id: None,
            agent: "claude".into(),
            mode: lazybox_ipc::AgentRuntimeMode::StreamJson,
        });

        let cmds = m.handle_pr_chat_question("and why?".into(), HelpQuestionKind::FollowUp);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::SendAgentInput { run_id, .. }] if *run_id == AgentRunId(3)
        ));
        assert_eq!(m.pr_chat_convo_mut().turns.len(), 2);
    }
}

#[cfg(test)]
mod dismiss_and_messages_tests {
    //! #309: every footer notice is dismissable with one key (Esc, the
    //! catalog `DismissNotice` binding) regardless of severity, and
    //! every non-hint notice also accumulates in a durable, clearable
    //! messages log surfaced by the `Shift-M` window. Severity still
    //! only decides auto-fade, never dismissability.
    use super::super::{Id, Model, Msg};
    use crate::realm::components::footer::NoticeSeverity;
    use chrono::Utc;
    use lazybox_core::{SessionKey, Workspace, WorkspaceKey};
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::event::{Key, KeyEvent, KeyModifiers};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Every notice flashed accumulates in the messages log — except
    /// one-shot Hints that actually display: those are ephemeral UI
    /// nudges that would only clutter the readable history. (A hint
    /// SUPPRESSED by a sticky error is the one exception — it never
    /// displayed, so it's logged instead of vanishing; see
    /// `notice_severity_slot_tests`.)
    #[test]
    fn flash_records_notices_in_the_log_except_hints() {
        let mut m = build_model();
        m.flash_info("saved");
        // Hint while a non-sticky notice is up: displays, not logged.
        m.flash_hint("scroll: alt-screen");
        m.flash_error("boom");

        let logged: Vec<_> = m.status.messages.recent().collect();
        // Most-recent-first, displayed hint excluded.
        assert_eq!(logged.len(), 2, "hint must not be logged: {logged:?}");
        assert_eq!(logged[0].message, "boom");
        assert_eq!(logged[0].severity, NoticeSeverity::Permanent);
        assert_eq!(logged[1].message, "saved");
        assert_eq!(logged[1].severity, NoticeSeverity::Info);
    }

    /// Esc clears the current notice whatever its severity — the whole
    /// point of #309. A sticky Permanent error (which never auto-fades)
    /// is the case that motivated it.
    #[test]
    fn esc_dismisses_a_sticky_notice() {
        let mut m = build_model();
        m.flash_error("scary red error");
        assert!(m.status.notice.is_some());

        m.dispatch_key(key(Key::Esc));
        assert!(
            m.status.notice.is_none(),
            "Esc must clear the sticky notice"
        );
        // Dismissing the footer surface leaves the durable log intact.
        assert_eq!(
            m.status.messages.recent().count(),
            1,
            "log survives dismiss"
        );
    }

    /// With a quiet footer, Esc keeps its normal (no-op here) meaning —
    /// the dismiss path is gated on a notice actually being up.
    #[test]
    fn esc_is_inert_when_no_notice_is_up() {
        let mut m = build_model();
        assert!(m.status.notice.is_none());
        m.dispatch_key(key(Key::Esc));
        assert!(m.status.notice.is_none());
    }

    /// #453: a sticky error is a dead end unless its full text is
    /// reachable. Enter (the `InspectNotice` binding) pops the whole
    /// message — which the footer pill would otherwise truncate — into a
    /// detail modal, without dismissing the notice.
    #[test]
    fn enter_inspects_a_sticky_error_into_a_detail_modal() {
        let mut m = build_model();
        m.flash_error("merge failed — owner/repo#1: Pull Request is not mergeable");
        m.dispatch_key(key(Key::Enter));
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::Error),
            "Enter must open the error detail modal while a sticky error is up",
        );
        assert!(
            m.status.notice.is_some(),
            "inspecting must not clear the notice — only Esc does that",
        );
    }

    /// Gated on *sticky* severity: a transient Info notice auto-fades
    /// and is up too often for Enter to lose its pane meaning, so Enter
    /// passes through (no detail modal).
    #[test]
    fn enter_is_inert_for_a_non_sticky_notice() {
        let mut m = build_model();
        m.flash_info("saved");
        m.dispatch_key(key(Key::Enter));
        assert!(
            !m.modal_stack.contains(&Id::Error),
            "Enter must not open a detail modal for a non-sticky notice",
        );
    }

    /// With a quiet footer, Enter keeps its normal pane meaning — the
    /// inspect path is gated on a sticky notice being up.
    #[test]
    fn enter_is_inert_when_no_notice_is_up() {
        let mut m = build_model();
        assert!(m.status.notice.is_none());
        m.dispatch_key(key(Key::Enter));
        assert!(!m.modal_stack.contains(&Id::Error));
    }

    /// The lifecycle is discoverable: while a sticky error is pinned the
    /// footer advertises both `detail` (inspect) and `dismiss`. A
    /// non-sticky notice — and a quiet footer — advertise neither.
    #[test]
    fn footer_advertises_inspect_and_dismiss_for_sticky_errors() {
        let mut m = build_model();
        assert!(m.notice_action_hints().is_empty(), "quiet footer: no hints");

        m.flash_info("saved");
        assert!(
            m.notice_action_hints().is_empty(),
            "non-sticky Info notice must not advertise inspect/dismiss",
        );

        m.flash_error("boom");
        let labels: Vec<_> = m
            .notice_action_hints()
            .into_iter()
            .map(|b| b.label.to_string())
            .collect();
        assert_eq!(
            labels,
            vec!["detail".to_string(), "dismiss".to_string()],
            "sticky error must advertise inspect then dismiss",
        );
    }

    /// The collision the guard defends against: with a sidebar
    /// multi-select up, Esc drops the selection FIRST (its established
    /// meaning) and leaves the notice — a second Esc then clears the
    /// notice. Dismiss must never silently eat the pane's own Esc.
    #[test]
    fn esc_yields_to_a_sidebar_multi_select() {
        let mut m = build_model();
        let ws = Workspace::empty(WorkspaceKey::new("local:scratch"), "main", Utc::now());
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&SessionKey::from(&ws_key)));
        assert_eq!(m.sidebar.toggle_broadcast_select(), Some(true));
        assert_eq!(m.sidebar.broadcast_selected_count(), 1);

        m.flash_error("boom");
        // First Esc: the sidebar consumes it to clear the selection; the
        // notice stays.
        m.dispatch_key(key(Key::Esc));
        assert_eq!(
            m.sidebar.broadcast_selected_count(),
            0,
            "Esc must clear the multi-select before touching the notice",
        );
        assert!(m.status.notice.is_some(), "notice survives the first Esc");
        // Second Esc: nothing else claims it now, so the notice clears.
        m.dispatch_key(key(Key::Esc));
        assert!(m.status.notice.is_none(), "second Esc clears the notice");
    }

    /// `Shift-M` opens the messages window (catalog → dispatch → mount),
    /// populated from the logged notices.
    #[test]
    fn shift_m_opens_the_messages_window() {
        let mut m = build_model();
        m.flash_error("boom");
        assert!(m.top_modal().is_none(), "no modal before Shift-M");

        m.dispatch_key(KeyEvent::new(Key::Char('M'), KeyModifiers::SHIFT));
        assert_eq!(m.top_modal(), Some(&Id::Messages));

        // A non-navigation, non-`c` key pops it back off.
        m.dispatch_modal_key(key(Key::Esc));
        assert!(m.top_modal().is_none(), "Esc closes the messages window");
    }

    /// `c` in the window wipes the durable log and leaves the window up,
    /// now showing the empty placeholder.
    #[test]
    fn c_clears_the_log_and_keeps_the_window_open() {
        let mut m = build_model();
        m.flash_error("boom");
        m.mount_messages();
        assert_eq!(m.top_modal(), Some(&Id::Messages));

        m.update(Msg::MessagesCleared);
        assert_eq!(m.status.messages.recent().count(), 0, "the log is wiped",);
        assert_eq!(
            m.top_modal(),
            Some(&Id::Messages),
            "the window stays up on the empty placeholder",
        );
    }
}

#[cfg(test)]
mod recent_snippets_tests {
    //! The snippet-picker "Recent" MRU is owned by the daemon (#548):
    //! confirmed `Event::SnippetDelivered` events update the immediate
    //! client view and the persisted order arrives in `Event::Snapshot`,
    //! which `seed_recent_snippets_from_snapshot` prunes against the loaded
    //! catalog.
    use super::super::*;
    use lazybox_ipc::Event;
    use lazybox_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
    ) {
        let (client, server) = channel::pair();
        (
            Model::new_for_test(client, Size::new(120, 40)).expect("model init"),
            server,
        )
    }

    /// Load a snippet library whose keys are exactly `keys` into the model,
    /// so seeding has a catalog to prune stale MRU keys against — mirroring
    /// the boot order where `apply_snippets` runs before the first snapshot.
    fn apply_snippet_keys(
        m: &mut Model<tuirealm::terminal::TestTerminalAdapter>,
        label: &str,
        keys: &[&str],
    ) {
        let mut yaml = String::from("snippets:\n");
        for k in keys {
            yaml.push_str(&format!("  {k}:\n    description: {k}\n    body: b\n"));
        }
        let tmp_dir =
            std::env::temp_dir().join(format!("lazybox-recent-{}-{label}", std::process::id(),));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tmp = tmp_dir.join("snippets.yaml");
        std::fs::write(&tmp, yaml).unwrap();
        m.apply_snippets(
            lazybox_config::Snippets::load_from(&tmp, lazybox_config::SnippetOrigin::Global)
                .unwrap(),
        );
    }

    fn snapshot_with_recent(recent: Vec<String>) -> Event {
        Event::Snapshot {
            workspaces: Vec::new(),
            terminals: Vec::new(),
            projects: Vec::new(),
            recent_snippets: recent,
            dismissed_updates: Vec::new(),
        }
    }

    #[test]
    fn confirmed_delivery_updates_local_mru_without_a_second_command() {
        let (mut m, mut server) = build_model();
        while server.rx.try_recv().is_ok() {} // drain Subscribe

        m.apply_recent_snippet("rev".into());
        assert_eq!(m.recent_snippets, vec!["rev".to_string()]);
        assert!(server.rx.try_recv().is_err(), "success is not re-reported");
    }

    #[test]
    fn record_dedups_and_caps_local_mru() {
        let (mut m, _server) = build_model();
        for k in ["a", "b", "c", "d", "e", "f"] {
            m.apply_recent_snippet(k.to_string());
        }
        m.apply_recent_snippet("a".into());
        assert_eq!(m.recent_snippets, vec!["a", "f", "e", "d", "c"]);
    }

    #[test]
    fn seed_from_snapshot_prunes_and_caps() {
        let (mut m, _server) = build_model();
        apply_snippet_keys(&mut m, "prune", &["rev", "pr"]);
        // The daemon list carries two live keys interleaved with two gone.
        m.handle_daemon_event(snapshot_with_recent(vec![
            "rev".into(),
            "gone".into(),
            "pr".into(),
            "dead".into(),
        ]));
        assert_eq!(m.recent_snippets, vec!["rev".to_string(), "pr".to_string()]);
    }

    #[test]
    fn seed_from_snapshot_caps_to_max() {
        let (mut m, _server) = build_model();
        let overflow: Vec<String> = (0..RECENT_SNIPPETS_MAX + 3)
            .map(|i| format!("s{i}"))
            .collect();
        let keys: Vec<&str> = overflow.iter().map(String::as_str).collect();
        apply_snippet_keys(&mut m, "caps", &keys);
        m.handle_daemon_event(snapshot_with_recent(overflow));
        assert_eq!(m.recent_snippets.len(), RECENT_SNIPPETS_MAX);
        assert_eq!(m.recent_snippets[0], "s0");
    }

    #[test]
    fn empty_snapshot_yields_empty_mru() {
        let (mut m, _server) = build_model();
        m.handle_daemon_event(snapshot_with_recent(Vec::new()));
        assert!(m.recent_snippets.is_empty());
    }

    // A `--connect` client (or a snapshot that arrives before the catalog
    // loads) must NOT have its daemon-owned MRU wiped against an empty
    // catalog — the prune only runs when a catalog is present (#548).
    #[test]
    fn empty_catalog_keeps_the_daemon_mru_unpruned() {
        let (mut m, _server) = build_model();
        // No apply_snippets: the catalog is empty.
        m.handle_daemon_event(snapshot_with_recent(vec!["rev".into(), "pr".into()]));
        assert_eq!(m.recent_snippets, vec!["rev".to_string(), "pr".to_string()]);
    }
}

#[cfg(test)]
mod mutation_failure_notice_tests {
    //! GitHub mutation rejections must surface as Permanent footer
    //! errors, not vanish into the Shift-D sync log behind the
    //! optimistic "requested N reviewer(s)" / "set labels" flashes
    //! the client shows at command-send time.
    use super::super::*;
    use crate::realm::components::footer::NoticeSeverity;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn provider_error(source: &str, message: &str) -> IpcEvent {
        IpcEvent::ProviderError {
            source: source.into(),
            message: message.into(),
            detail: String::new(),
            // `exhausted`: an actionable sync failure. A live `retryable`
            // transient no longer raises the banner (#730); mutation
            // rejections and exhausted syncs are the actionable surface
            // these tests cover.
            kind: "exhausted".into(),
        }
    }

    #[test]
    fn mutation_failures_raise_persistent_named_errors() {
        for (source, verb) in [
            ("reviewers", "request reviewers"),
            ("assignees", "update assignees"),
            ("labels", "update labels"),
            ("merge", "merge"),
            ("close-issue", "close issue"),
        ] {
            let mut m = build_model();
            m.status.polling = None;
            m.handle_daemon_event(provider_error(source, "GraphQL said no"));
            let n = m
                .status
                .notice
                .as_ref()
                .unwrap_or_else(|| panic!("{source}: failure must raise a notice"));
            assert_eq!(
                n.severity,
                NoticeSeverity::Permanent,
                "{source}: a rejected mutation must not auto-fade",
            );
            assert!(
                n.message.contains(&format!("{verb} failed")),
                "{source}: message must name the action, got {:?}",
                n.message,
            );
            assert!(
                n.message.contains("GraphQL said no"),
                "{source}: message must quote the reason, got {:?}",
                n.message,
            );
        }
    }

    /// A failed provider cycle replaces its in-flight spinner with a
    /// named error and also lands in Shift-D.
    #[test]
    fn poll_cycle_failures_replace_the_spinner_with_an_explicit_error() {
        let mut m = build_model();
        m.status.polling = None;
        m.handle_daemon_event(IpcEvent::PollProgress {
            source: "github".into(),
            message: "Fetching issues".into(),
        });
        assert!(m.status.bg_poll.is_some());

        m.handle_daemon_event(provider_error("github", "request failed"));
        assert!(
            m.status.bg_poll.is_none(),
            "a failed poll must not retain its in-flight spinner"
        );
        let notice = m
            .status
            .notice
            .as_ref()
            .expect("failed poll must replace the spinner with an error");
        assert_eq!(notice.severity, NoticeSeverity::Permanent);
        assert!(notice.message.contains("sync failed"));
        assert!(notice.message.contains("request failed"));
        assert!(matches!(
            m.status
                .sync
                .latest_per_source()
                .first()
                .map(|entry| &entry.outcome),
            Some(crate::realm::status_ctx::SyncOutcome::Err { message, .. })
                if message == "request failed"
        ));
    }

    /// A user-initiated mutation is emitted on the wire as a `retryable`
    /// `ProviderError` (same kind as a self-healing sync transient). It
    /// must be handled ONLY by its own actionable branch and never leak
    /// into the sync-poll surface — otherwise a mutation rejection that
    /// coincides with a manual refresh would consume the sync refresh
    /// acknowledgment (and, in the general case, risk being swallowed by
    /// the quiet transient path). Regression guard for the overloaded
    /// `retryable` kind (#730 review finding 2).
    #[test]
    fn mutation_failure_never_leaks_into_the_sync_poll_surface() {
        let mut m = build_model();
        m.status.polling = None;
        // User hit Shift-R and is waiting on the sync result.
        m.pending_refresh_ack = true;

        // A reviewer mutation the daemon rejected, carried (like every
        // mutation) as a `retryable` ProviderError.
        m.handle_daemon_event(IpcEvent::ProviderError {
            source: "reviewers".into(),
            message: "GraphQL said no".into(),
            detail: String::new(),
            kind: "retryable".into(),
        });

        // It surfaces loudly through the mutation branch — never demoted
        // to a quiet transient.
        let n = m
            .status
            .notice
            .as_ref()
            .expect("mutation rejection must surface");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(
            n.message.contains("request reviewers failed"),
            "must name the action, got {:?}",
            n.message,
        );
        // The mutation is not a sync attempt, so it must not arm the
        // sync-error banner tag…
        assert!(m.sync_error_source.is_none());
        // …nor consume the pending sync refresh — that acknowledgment
        // belongs to the poll, which hasn't reported yet.
        assert!(
            m.pending_refresh_ack,
            "a mutation rejection must not consume the sync refresh ack"
        );
    }

    /// #730: a live `retryable` transient the daemon is auto-retrying
    /// must NOT raise the red "✗ sync failed" banner — that noise buries
    /// the failures the user must act on. The spinner still clears (the
    /// poll did end) and the attempt still lands in the Shift-D sync log,
    /// but the footer stays quiet on a background cycle.
    #[test]
    fn live_retryable_transient_clears_spinner_without_a_banner() {
        let mut m = build_model();
        m.status.polling = None;
        m.handle_daemon_event(IpcEvent::PollProgress {
            source: "github".into(),
            message: "Fetching issues".into(),
        });
        assert!(m.status.bg_poll.is_some());

        m.handle_daemon_event(IpcEvent::ProviderError {
            source: "github".into(),
            message: "github hiccup, retrying next cycle".into(),
            detail: String::new(),
            kind: "retryable".into(),
        });
        assert!(
            m.status.bg_poll.is_none(),
            "the poll ended, so its spinner must clear even for a quiet transient"
        );
        assert!(
            m.status.notice.is_none(),
            "a self-healing transient must not raise a footer banner, got {:?}",
            m.status.notice.as_ref().map(|n| &n.message),
        );
        assert!(
            m.sync_error_source.is_none(),
            "a quiet transient must not arm the sticky sync-error tag"
        );
        assert!(
            matches!(
                m.status
                    .sync
                    .latest_per_source()
                    .first()
                    .map(|entry| &entry.outcome),
                Some(crate::realm::status_ctx::SyncOutcome::Err { kind, .. }) if kind == "retryable"
            ),
            "the transient still records in the sync log for Shift-D"
        );
    }

    /// #730: a manual refresh (Shift-R) that hits a live transient gets
    /// calm, auto-fading feedback so it doesn't look ignored — but still
    /// not the red banner reserved for actionable failures.
    #[test]
    fn manual_refresh_transient_gets_calm_feedback_not_a_banner() {
        let mut m = build_model();
        m.status.polling = None;
        m.pending_refresh_ack = true;

        m.handle_daemon_event(IpcEvent::ProviderError {
            source: "github".into(),
            message: "github hiccup, retrying next cycle".into(),
            detail: String::new(),
            kind: "retryable".into(),
        });
        let n = m
            .status
            .notice
            .as_ref()
            .expect("a manual refresh deserves feedback");
        assert_eq!(
            n.severity,
            NoticeSeverity::Retryable,
            "calm auto-fading feedback, not a sticky error"
        );
        assert!(
            !n.message.contains("sync failed"),
            "must not read as a hard failure, got {:?}",
            n.message,
        );
        assert!(
            m.sync_error_source.is_none(),
            "the calm feedback must not arm the sticky sync-error tag"
        );
        assert!(!m.pending_refresh_ack, "the refresh ack is consumed");
    }

    /// A failed reply names the action AND parks the (otherwise lost)
    /// composed text in the messages log so it's recoverable.
    #[test]
    fn reply_failure_names_action_and_preserves_text() {
        let mut m = build_model();
        m.status.polling = None;
        m.modal_flow = Some(super::super::ModalFlow::Reply {
            target: SessionKey::from("github:o/r#1"),
        });
        let cmds = m.handle_textarea_submitted("my carefully composed reply".into());
        assert!(!cmds.is_empty(), "reply submit dispatches PostReply");

        m.handle_daemon_event(provider_error("reply", "comment create failed"));
        let n = m.status.notice.as_ref().expect("reply failure notice");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("reply failed"), "got {:?}", n.message);
        assert!(
            n.message.contains("Shift-M"),
            "must point at the recovered text, got {:?}",
            n.message,
        );
        assert!(
            m.status
                .messages
                .recent()
                .any(|e| e.message.contains("my carefully composed reply")),
            "the composed text must be recoverable from the messages log",
        );
    }
}

#[cfg(test)]
mod daemon_disconnect_tests {
    //! A dead daemon channel must surface, not leave a zombie UI that
    //! renders forever while every keypress silently goes nowhere.
    use super::super::*;
    use crate::realm::components::footer::NoticeSeverity;
    use lazybox_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    #[test]
    fn full_remote_command_queue_is_retryable_not_disconnected() {
        let (command_tx, _command_rx) = tokio::sync::mpsc::channel(1);
        command_tx
            .try_send(IpcCommand::Subscribe)
            .expect("occupy bounded command slot");
        let (_event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        let client = lazybox_ipc::Client::from_bounded_channels(command_tx, event_rx);
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");

        m.send_cmd(IpcCommand::Refresh);
        m.tick_daemon_health();

        let notice = m.status.notice.as_ref().expect("congestion notice");
        assert_eq!(notice.severity, NoticeSeverity::Retryable);
        assert!(notice.message.contains("not accepted"));
        assert!(!m.daemon_disconnect_notified);
    }

    #[test]
    fn failed_send_raises_disconnect_banner_on_next_tick() {
        let (client, server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        drop(server);

        m.status.notice = None;
        m.send_cmd(IpcCommand::Refresh);
        m.tick_daemon_health();

        let n = m.status.notice.as_ref().expect("disconnect banner set");
        assert_eq!(
            n.severity,
            NoticeSeverity::Permanent,
            "a dead daemon must not auto-fade"
        );
        assert!(
            n.message.contains("daemon disconnected"),
            "got {:?}",
            n.message
        );
    }

    /// The banner is one-shot: repeated failed sends (every keypress
    /// on a dead channel) must not re-record it per keystroke.
    #[test]
    fn disconnect_banner_is_raised_once() {
        let (client, server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        drop(server);

        for _ in 0..5 {
            m.send_cmd(IpcCommand::Refresh);
            m.tick_daemon_health();
        }
        m.note_daemon_disconnected();

        let count = m
            .status
            .messages
            .recent()
            .filter(|e| e.message.contains("daemon disconnected"))
            .count();
        assert_eq!(count, 1, "the disconnect notice must be latched one-shot");
    }

    /// The event-channel-closed path (`wait_for_wake` flipping
    /// `daemon_open` off) reports through the same one-shot notice.
    #[test]
    fn note_daemon_disconnected_sets_sticky_notice() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.note_daemon_disconnected();
        let n = m.status.notice.as_ref().expect("banner");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
    }

    /// Regression for #588: the disconnect banner is one-shot (the guard
    /// is never reset), so it must outlive the action-toast auto-fade —
    /// otherwise it silently vanishes after ~45s while commands still
    /// fail and never returns. It carries no workspace tag, so
    /// `tick_notice` must leave it up no matter how much time passes.
    #[test]
    fn disconnect_banner_outlives_the_action_toast_fade() {
        use std::time::{Duration, Instant};
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.note_daemon_disconnected();
        // Backdate far past any fade window.
        if let Some(n) = m.status.notice.as_mut() {
            n.set_at = Instant::now() - Duration::from_secs(60 * 60);
        }
        assert!(
            !m.status.tick_notice(),
            "the disconnect banner must not auto-fade"
        );
        assert!(
            m.status.notice.is_some(),
            "the disconnect banner must survive an hour of ticks"
        );
    }
}

#[cfg(test)]
mod reconnect_banner_tests {
    //! A self-healing `--connect` transport publishes `ConnectionStatus`.
    //! The model must surface a reconnecting banner (not a silent freeze
    //! during an extended outage) and refresh the daemon build across a
    //! reconnect that lands on a restarted daemon.
    use super::super::*;
    use crate::realm::components::footer::NoticeSeverity;
    use lazybox_ipc::{Client, ConnectionState, ConnectionStatus};
    use tuirealm::ratatui::layout::Size;

    fn connected() -> ConnectionState {
        ConnectionState {
            status: ConnectionStatus::Connected,
            daemon_build: lazybox_ipc::BUILD_VERSION.to_string(),
        }
    }

    /// Far ends of the client's command/event channels. These tests only
    /// drive `tick_daemon_health` (which neither sends nor receives), but
    /// holding the ends keeps the `Client` off a closed channel for its
    /// lifetime — bind as `_io` so they live to the end of the test.
    type ChannelEnds = (
        tokio::sync::mpsc::Receiver<IpcCommand>,
        tokio::sync::mpsc::Sender<lazybox_ipc::Event>,
    );

    /// A model whose client tracks a caller-controlled connection-state
    /// watch, plus the sender to drive transitions and the channel ends
    /// to keep alive.
    fn model_with_status() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        tokio::sync::watch::Sender<ConnectionState>,
        ChannelEnds,
    ) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<IpcCommand>(8);
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<lazybox_ipc::Event>(8);
        let (status_tx, status_rx) = tokio::sync::watch::channel(connected());
        let client =
            Client::from_bounded_channels(cmd_tx, evt_rx).with_connection_status(status_rx);
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.status.notice = None;
        (m, status_tx, (cmd_rx, evt_tx))
    }

    #[test]
    fn reconnecting_status_raises_a_sticky_banner_then_clears_on_reconnect() {
        let (mut m, status_tx, _io) = model_with_status();

        status_tx.send_replace(ConnectionState {
            status: ConnectionStatus::Reconnecting,
            ..connected()
        });
        m.tick_daemon_health();
        let n = m.status.notice.as_ref().expect("reconnecting banner");
        assert_eq!(
            n.severity,
            NoticeSeverity::Auth,
            "reconnecting banner must be sticky so it survives an extended outage"
        );
        assert!(n.message.contains("reconnecting"), "got {:?}", n.message);
        assert!(m.daemon_reconnecting_notified);

        status_tx.send_replace(connected());
        m.tick_daemon_health();
        assert!(
            m.status.notice.is_none(),
            "the banner must retract once the link is back"
        );
        assert!(!m.daemon_reconnecting_notified);
    }

    #[test]
    fn reconnecting_banner_is_latched_one_shot() {
        let (mut m, status_tx, _io) = model_with_status();
        status_tx.send_replace(ConnectionState {
            status: ConnectionStatus::Reconnecting,
            ..connected()
        });
        for _ in 0..5 {
            m.tick_daemon_health();
        }
        // Stays the single live notice; the one-shot latch means it was
        // flashed (and logged) exactly once despite five ticks, not
        // re-recorded per frame.
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("reconnecting"))
        );
        let logged = m
            .status
            .messages
            .recent()
            .filter(|e| e.message.contains("reconnecting"))
            .count();
        assert_eq!(
            logged, 1,
            "the reconnecting banner must be latched one-shot"
        );
    }

    #[test]
    fn terminal_reconnect_failure_renders_its_actionable_message() {
        let (mut m, status_tx, _io) = model_with_status();
        let message = "subscription required — https://lazybox.ai/pricing";
        status_tx.send_replace(ConnectionState {
            status: ConnectionStatus::Failed {
                message: message.into(),
            },
            ..connected()
        });

        // The event channel can close before the next health tick observes
        // the watch state; that path must still prefer the terminal reason.
        m.note_daemon_disconnected();
        m.tick_daemon_health();

        let notice = m.status.notice.as_ref().expect("terminal failure banner");
        assert_eq!(notice.message, message);
        assert_eq!(notice.severity, NoticeSeverity::Permanent);
        assert!(m.daemon_disconnect_notified);
    }

    #[test]
    fn reconnect_to_a_different_build_warns_about_the_mismatch() {
        let (mut m, status_tx, _io) = model_with_status();
        status_tx.send_replace(ConnectionState {
            status: ConnectionStatus::Reconnecting,
            ..connected()
        });
        m.tick_daemon_health();
        // The daemon came back from a different (wire-compatible) build.
        status_tx.send_replace(ConnectionState {
            status: ConnectionStatus::Connected,
            daemon_build: "v9.9.9-deadbeef".to_string(),
        });
        m.tick_daemon_health();
        let n = m.status.notice.as_ref().expect("build mismatch banner");
        assert!(
            n.message.starts_with("build mismatch: daemon v9.9.9"),
            "reconnect must refresh the daemon build, not keep the first one: {:?}",
            n.message
        );
    }

    #[test]
    fn matching_build_retracts_a_stale_mismatch_banner() {
        let (mut m, _status_tx, _io) = model_with_status();
        m.note_daemon_build("v0.0.1-stale");
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.starts_with("build mismatch")),
            "a mismatched build raises the banner"
        );
        // A later handshake reports a build that now matches this client.
        m.note_daemon_build(lazybox_ipc::BUILD_VERSION);
        assert!(
            m.status.notice.is_none(),
            "a now-matching build must retract the stale mismatch banner"
        );
    }
}

#[cfg(test)]
mod notice_severity_slot_tests {
    //! The single footer-notice slot is severity-aware: a routine
    //! Info/Hint flash must not displace a live sticky (Permanent /
    //! Auth) error — it lands in the messages log instead.
    use super::super::*;
    use crate::realm::components::footer::NoticeSeverity;
    use lazybox_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    #[test]
    fn info_does_not_displace_a_sticky_error() {
        let mut m = build_model();
        m.flash_error("✗ merge failed — o/r#1: checks pending");
        m.flash_info("requested 2 reviewer(s)");

        let n = m.status.notice.as_ref().expect("notice present");
        assert!(
            n.message.contains("merge failed"),
            "the sticky error must survive a routine Info flash, got {:?}",
            n.message,
        );
        assert!(
            m.status
                .messages
                .recent()
                .any(|e| e.message.contains("requested 2 reviewer(s)")),
            "the suppressed flash must land in the messages log",
        );
    }

    #[test]
    fn suppressed_hint_lands_in_messages_log() {
        let mut m = build_model();
        m.flash_error("✗ spawn failed — boom");
        m.flash_hint("loading repo labels…");

        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("spawn failed")),
            "the sticky error must survive a Hint",
        );
        assert!(
            m.status
                .messages
                .recent()
                .any(|e| e.message.contains("loading repo labels")),
            "a suppressed hint would otherwise vanish unseen — log it",
        );
    }

    /// Ordinary (non-sticky) notices keep last-wins semantics, and
    /// hints stay out of the log when they actually display.
    #[test]
    fn non_sticky_notices_keep_last_wins() {
        let mut m = build_model();
        m.flash_info("first");
        m.flash_hint("second");
        assert_eq!(m.status.notice.as_ref().unwrap().message, "second");
        assert!(
            !m.status.messages.recent().any(|e| e.message == "second"),
            "a DISPLAYED hint stays out of the durable log (#309)",
        );
    }

    /// A sticky error may replace another sticky error — the newest
    /// failure is the actionable one.
    #[test]
    fn sticky_replaces_sticky() {
        let mut m = build_model();
        m.flash_error("first failure");
        m.flash_error("second failure");
        assert_eq!(m.status.notice.as_ref().unwrap().message, "second failure");
    }

    /// Esc-dismissing the sticky error re-opens the slot.
    #[test]
    fn dismissed_sticky_frees_the_slot() {
        let mut m = build_model();
        m.flash_error("boom");
        m.status.notice = None; // what the Esc handler does
        m.flash_info("all good");
        assert_eq!(m.status.notice.as_ref().unwrap().message, "all good");
    }

    /// The manual-refresh recovery path must still show "✓ sync ok"
    /// even though a Permanent "✗ sync failed" banner is up — the
    /// recovered provider's banner is cleared before the Info flash.
    #[test]
    fn sync_recovery_ack_replaces_its_own_sticky_banner() {
        use lazybox_ipc::Event as IpcEvent;
        let mut m = build_model();
        m.status.polling = None;

        m.pending_refresh_ack = true;
        m.handle_daemon_event(IpcEvent::ProviderError {
            source: "github".into(),
            message: "boom".into(),
            detail: String::new(),
            // `exhausted` (retries run out) is the actionable failure that
            // arms the banner; a live `retryable` transient stays quiet
            // (#730).
            kind: "exhausted".into(),
        });
        assert!(
            m.status
                .notice
                .as_ref()
                .is_some_and(|n| n.message.contains("sync failed")),
        );

        m.pending_refresh_ack = true;
        m.handle_daemon_event(IpcEvent::PollCompleted {
            source: "github".into(),
            count: 4,
        });
        let n = m.status.notice.as_ref().expect("ack notice");
        assert!(
            n.message.contains("✓ sync ok"),
            "recovery ack must replace the failed-sync banner it owns, got {:?}",
            n.message,
        );
        assert_eq!(n.severity, NoticeSeverity::Info);
    }
}

#[cfg(test)]
mod worktree_progress_dismiss_tests {
    //! Esc on the provisioning checklist must stick: later progress
    //! events for the SAME operation update silently instead of
    //! re-mounting the modal on top of whatever the user is typing.
    use super::super::*;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{Event as IpcEvent, WorktreeStep, WorktreeStepStatus, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn progress(key: &SessionKey, step: WorktreeStep, status: WorktreeStepStatus) -> IpcEvent {
        IpcEvent::WorktreeProgress {
            session_key: key.clone(),
            step,
            status,
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        }
    }

    #[test]
    fn dismissed_checklist_does_not_resurrect_on_next_progress_event() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "first progress event mounts the checklist"
        );

        // Esc — the user dismissed it mid-provision.
        assert_eq!(m.modal_stack.last(), Some(&Id::WorktreeProgress));
        let _ = m.handle_modal_dismissed();
        assert!(!m.modal_stack.contains(&Id::WorktreeProgress));

        // The next progress event for the SAME op must NOT remount.
        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Fetch,
            WorktreeStepStatus::Started,
        ));
        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "a dismissed checklist must stay dismissed for this operation"
        );
        assert!(m.worktree_progress.is_none());
    }

    /// Esc while provisioning is still in flight is a real cancel: it
    /// must send `CancelSpawn` so the daemon aborts the provision
    /// (killing a wedged clone and releasing the singleton claim so a
    /// retry starts fresh — issue #403), not just close the view.
    #[test]
    fn esc_mid_provision_sends_cancel_spawn() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        let cmds = m.handle_modal_dismissed();
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                lazybox_ipc::Command::CancelSpawn { session_key } if session_key == &key
            )),
            "Esc mid-provision must cancel the spawn, got {cmds:?}"
        );
    }

    /// The daemon's confirmation of this client's own Esc-cancel (a
    /// `Failed` carrying `SPAWN_CANCELLED_NOTE`) must read as a plain
    /// info notice, not an error — the user asked for it.
    #[test]
    fn cancel_confirmation_flashes_info_not_error() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        let _ = m.handle_modal_dismissed();

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Failed(lazybox_ipc::SPAWN_CANCELLED_NOTE.into()),
        ));
        let n = m.status.notice.as_ref().expect("cancel confirmation");
        assert_eq!(n.severity, NoticeSeverity::Info, "got {:?}", n.message);
        assert!(n.message.contains("cancelled"), "got {:?}", n.message);
        assert!(
            m.worktree_progress_dismissed.is_none(),
            "the cancelled op must release the dismissal marker so a retry shows its checklist"
        );
    }

    /// Esc on a checklist frozen on a FAILED step is just an
    /// acknowledgement — the provision already ended, there is nothing
    /// to cancel.
    #[test]
    fn esc_on_failed_checklist_does_not_send_cancel_spawn() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Failed("clone exploded".into()),
        ));
        let cmds = m.handle_modal_dismissed();
        assert!(
            cmds.is_empty(),
            "a failed provision has nothing to cancel, got {cmds:?}"
        );
    }

    /// A failed step still surfaces even while dismissed — Esc must
    /// not hide a broken provision.
    #[test]
    fn dismissed_checklist_still_surfaces_step_failures() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        let _ = m.handle_modal_dismissed();

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Failed("clone exploded".into()),
        ));
        let n = m.status.notice.as_ref().expect("failure notice");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("clone exploded"), "got {:?}", n.message);
        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "the failure surfaces in the footer, not by resurrecting the modal"
        );
    }

    /// A different workspace starting to provision is a NEW operation
    /// — its checklist shows normally.
    #[test]
    fn new_operation_after_dismissal_shows_its_checklist() {
        let mut m = build_model();
        let a = SessionKey::from("github:o/r#1");
        let b = SessionKey::from("github:o/r#2");

        m.handle_daemon_event(progress(
            &a,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        let _ = m.handle_modal_dismissed();

        m.handle_daemon_event(progress(
            &b,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "a different session's provision is a fresh op — checklist shows"
        );
    }

    /// Once the dismissed op completes (its terminal spawns), the
    /// marker releases so the workspace's NEXT provision gets its
    /// checklist again.
    #[test]
    fn completion_releases_the_dismissal_marker() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        let _ = m.handle_modal_dismissed();
        assert_eq!(m.worktree_progress_dismissed.as_ref(), Some(&key));

        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: lazybox_ipc::TerminalId(9),
            session_key: key.clone(),
            kind: lazybox_ipc::TerminalKind::Shell,
            no_permission: false,
            on_main: false,
            model_label: None,
        });
        assert!(
            m.worktree_progress_dismissed.is_none(),
            "op completed — the dismissal must not outlive it"
        );

        m.handle_daemon_event(progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "the next provision on this workspace gets its checklist again"
        );
    }

    fn autonomous_progress_with(
        key: &SessionKey,
        step: WorktreeStep,
        status: WorktreeStepStatus,
        trigger: lazybox_ipc::AutonomousTrigger,
    ) -> IpcEvent {
        IpcEvent::WorktreeProgress {
            session_key: key.clone(),
            step,
            status,
            origin: lazybox_ipc::SpawnOrigin::Autonomous(trigger),
        }
    }

    fn autonomous_progress(
        key: &SessionKey,
        step: WorktreeStep,
        status: WorktreeStepStatus,
    ) -> IpcEvent {
        autonomous_progress_with(key, step, status, lazybox_ipc::AutonomousTrigger::Mention)
    }

    /// An autonomous (label / `@lazybox`) spawn is background work the
    /// user didn't ask for — its provisioning must NOT pop the modal,
    /// only report a footer notice naming the workspace and trigger
    /// (issue #645).
    #[test]
    fn autonomous_progress_reports_notice_not_modal() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();
        let key = SessionKey::from("github:codefly-dev/warden-platform#7");

        m.handle_daemon_event(autonomous_progress(
            &key,
            WorktreeStep::Fetch,
            WorktreeStepStatus::Started,
        ));
        assert!(
            !m.modal_stack.contains(&Id::WorktreeProgress),
            "an autonomous spawn must not steal focus with the progress modal"
        );
        assert!(
            m.worktree_progress.is_none(),
            "no checklist state is accumulated for a quiet autonomous spawn"
        );
        let n = m.status.notice.as_ref().expect("autonomous start notice");
        assert_eq!(n.severity, NoticeSeverity::Info);
        assert_eq!(
            n.message, "starting agent on codefly-dev/warden-platform#7 (@lazybox)",
            "the notice names the workspace (source prefix stripped) and the trigger tag",
        );
    }

    /// The footer notice's parenthetical names the actual autonomous
    /// source, not a generic "(autonomous)" — a `@lazybox` mention, a
    /// GitHub label, and an auto-fix each read differently (issue #645).
    #[test]
    fn autonomous_notice_tag_reflects_the_trigger() {
        let cases = [
            (lazybox_ipc::AutonomousTrigger::Mention, "(@lazybox)"),
            (lazybox_ipc::AutonomousTrigger::Label, "(label)"),
            (lazybox_ipc::AutonomousTrigger::AutoFix, "(auto-fix)"),
            (lazybox_ipc::AutonomousTrigger::Restore, "(restored)"),
        ];
        for (trigger, tag) in cases {
            let mut m = build_model();
            let key = SessionKey::from("github:o/r#1");
            m.handle_daemon_event(autonomous_progress_with(
                &key,
                WorktreeStep::Fetch,
                WorktreeStepStatus::Started,
                trigger,
            ));
            let n = m.status.notice.as_ref().expect("start notice");
            assert!(
                n.message.ends_with(tag),
                "trigger {trigger:?} must tag the notice {tag}, got {:?}",
                n.message
            );
            assert!(!m.modal_stack.contains(&Id::WorktreeProgress));
        }
    }

    /// The one-line "starting agent…" notice fires once, not once per
    /// `WorktreeProgress` step a single provision emits.
    #[test]
    fn autonomous_notice_fires_once_per_spawn() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        for step in [
            WorktreeStep::Fetch,
            WorktreeStep::Clone,
            WorktreeStep::WorktreeAdd,
        ] {
            m.handle_daemon_event(autonomous_progress(&key, step, WorktreeStepStatus::Started));
        }
        assert_eq!(
            m.autonomous_spawn_notified.len(),
            1,
            "a single spawn is tracked once across its several steps"
        );
        assert!(m.autonomous_spawn_notified.contains(&key));
        assert!(!m.modal_stack.contains(&Id::WorktreeProgress));
    }

    /// A failed autonomous provision still needs a decision, so it
    /// surfaces the checklist/recovery modal even though its normal
    /// steps stayed quiet (issue #645 keeps #594 for genuine failures).
    #[test]
    fn autonomous_failure_still_mounts_recovery_modal() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(autonomous_progress(
            &key,
            WorktreeStep::Fetch,
            WorktreeStepStatus::Started,
        ));
        assert!(!m.modal_stack.contains(&Id::WorktreeProgress));

        m.handle_daemon_event(autonomous_progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Failed(
                "worktree: checkout_at: branch 'feat' not found locally or on origin".into(),
            ),
        ));
        assert!(
            m.modal_stack.contains(&Id::WorktreeProgress),
            "a failed autonomous provision surfaces the recovery modal"
        );
    }

    /// After an autonomous provision finishes (`Setup` reaches `Done`)
    /// the marker clears so a later re-spawn on the same workspace
    /// announces again.
    #[test]
    fn autonomous_completion_clears_notice_marker() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(autonomous_progress(
            &key,
            WorktreeStep::Fetch,
            WorktreeStepStatus::Started,
        ));
        assert!(m.autonomous_spawn_notified.contains(&key));

        m.handle_daemon_event(autonomous_progress(
            &key,
            WorktreeStep::Setup,
            WorktreeStepStatus::Done,
        ));
        assert!(
            !m.autonomous_spawn_notified.contains(&key),
            "a finished provision releases the marker for a future re-spawn"
        );
    }

    /// The `Setup`/`Done` step normally clears the notice marker, but a
    /// lagged broadcast can drop it. A live terminal (`TerminalSpawned`)
    /// proves provisioning finished, so it must release the marker too —
    /// otherwise a later re-spawn on the workspace would stay silent.
    #[test]
    fn terminal_spawned_backstops_a_dropped_completion_for_the_notice_marker() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(autonomous_progress(
            &key,
            WorktreeStep::Fetch,
            WorktreeStepStatus::Started,
        ));
        assert!(m.autonomous_spawn_notified.contains(&key));

        // The terminal came up but `Setup`/`Done` never arrived (dropped).
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: lazybox_ipc::TerminalId(7),
            session_key: key.clone(),
            kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
            no_permission: true,
            on_main: false,
            model_label: None,
        });
        assert!(
            !m.autonomous_spawn_notified.contains(&key),
            "a live terminal releases the marker even without the Setup/Done step"
        );

        // A genuine re-spawn on the same workspace announces again.
        m.handle_daemon_event(autonomous_progress(
            &key,
            WorktreeStep::Fetch,
            WorktreeStepStatus::Started,
        ));
        assert!(
            m.autonomous_spawn_notified.contains(&key),
            "the re-spawn re-announces because the marker was released"
        );
    }

    /// A workspace removed before its autonomous spawn ever reached a
    /// live terminal must not leak the notice marker.
    #[test]
    fn workspace_removal_clears_the_autonomous_notice_marker() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");

        m.handle_daemon_event(autonomous_progress(
            &key,
            WorktreeStep::Fetch,
            WorktreeStepStatus::Started,
        ));
        assert!(m.autonomous_spawn_notified.contains(&key));

        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(
            key.as_str(),
        )));
        assert!(
            !m.autonomous_spawn_notified.contains(&key),
            "a removed workspace must not leak its notice marker"
        );
    }
}

#[cfg(test)]
mod spawn_focus_steal_tests {
    //! `TerminalSpawned` must only yank pane focus when THIS client
    //! asked for the spawn — in multi-client mode every client hears
    //! every spawn, and focus must never move mid-search or under a
    //! mounted modal.
    use super::super::*;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn spawned(key: &SessionKey, id: u64) -> IpcEvent {
        IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(id),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        }
    }

    #[test]
    fn unsolicited_spawn_does_not_steal_focus() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");
        assert_eq!(m.focus, PaneFocus::Sidebar);

        // No spawn spinner, no follow pin, no deferred editor — this
        // client never asked. (Another client pressed `w`.)
        m.handle_daemon_event(spawned(&key, 1));
        assert_eq!(
            m.focus,
            PaneFocus::Sidebar,
            "an unsolicited spawn must not move focus to the terminal pane"
        );
    }

    #[test]
    fn requested_spawn_moves_focus_to_terminals() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");
        m.status.note_spawning(
            "claude",
            key.clone(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        m.handle_daemon_event(spawned(&key, 1));
        assert_eq!(
            m.focus,
            PaneFocus::Terminals,
            "our own spawn still lands us in the terminal"
        );
    }

    /// Minimal PR workspace so the sidebar has a repo group header —
    /// `open_search` is scoped to the project under the cursor and
    /// no-ops on an empty sidebar.
    fn pr_workspace(key: &str) -> lazybox_core::Workspace {
        use chrono::Utc;
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        let task = lazybox_core::Task {
            author: String::new(),
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
            url: format!("https://github.com/{path}/pull/{num}"),
            repo: Some("owner/repo".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        };
        lazybox_core::Workspace::from_task(task, Utc::now())
    }

    /// Dispatching `OpenGlobalSearch` (`#`) opens an unscoped search —
    /// the wiring `#` → `Action::OpenGlobalSearch` → `open_global_search`
    /// (cross-repo filtering itself is covered in the sidebar tests).
    #[test]
    fn global_search_dispatch_opens_unscoped_search() {
        let mut m = build_model();
        let ws = pr_workspace("owner/repo#1");
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        m.dispatch_action(&lazybox_tui_core::action::Action::OpenGlobalSearch);
        assert!(m.sidebar.search_editing());
        let s = m.sidebar.search().expect("search state present");
        assert_eq!(s.scope, None, "global search is unscoped");
    }

    /// End-to-end: a bare `Esc` on a *committed* (non-editing) search
    /// clears it through the real key pipeline. Editing-time `Esc` is
    /// intercepted before pane dispatch (`keys.rs`), so a committed
    /// search only reaches the sidebar's Esc handler — which used to
    /// drop it on the floor, trapping the user in a narrowed tree
    /// (#1033).
    #[test]
    fn bare_esc_clears_a_committed_search() {
        use tuirealm::event::{Key, KeyEvent as RealmKey, KeyModifiers as RealmMods};
        let mut m = build_model();
        let ws = pr_workspace("owner/repo#1");
        let k: SessionKey = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&k));
        m.dispatch_action(&lazybox_tui_core::action::Action::OpenSearch);
        assert!(m.sidebar.search_editing(), "search opens in editing mode");
        // Type + commit through the real key pipeline.
        m.dispatch_key(RealmKey::new(Key::Char('o'), RealmMods::NONE));
        m.dispatch_key(RealmKey::new(Key::Enter, RealmMods::NONE));
        assert!(!m.sidebar.search_editing(), "Enter commits the search");
        assert!(
            m.sidebar.search().is_some(),
            "committed filter stays applied"
        );
        m.dispatch_key(RealmKey::new(Key::Esc, RealmMods::NONE));
        assert!(
            m.sidebar.search().is_none(),
            "a bare Esc clears the committed search"
        );
    }

    #[test]
    fn spawn_never_steals_focus_while_search_is_being_typed() {
        let mut m = build_model();
        let ws = pr_workspace("owner/repo#1");
        let key: SessionKey = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&key));
        m.status.note_spawning(
            "claude",
            key.clone(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        m.sidebar.open_search();
        assert!(m.sidebar.search_editing());

        m.handle_daemon_event(spawned(&key, 1));
        assert_eq!(
            m.focus,
            PaneFocus::Sidebar,
            "keystrokes mid-search must not suddenly land in a shell"
        );
    }

    #[test]
    fn spawn_never_steals_focus_under_an_interactive_modal() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");
        m.status.note_spawning(
            "claude",
            key.clone(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        m.mount_reply(SessionKey::from("github:o/r#2"));
        assert_eq!(m.modal_stack.last(), Some(&Id::Reply));

        m.handle_daemon_event(spawned(&key, 1));
        assert_eq!(
            m.focus,
            PaneFocus::Sidebar,
            "a mounted modal owns input — the spawn must not refocus behind it"
        );
    }
}

#[cfg(test)]
mod repo_labels_failure_tests {
    //! `g l` label-fetch failure: the daemon now broadcasts a
    //! `ProviderError { source: "repo-labels" }`, and the client
    //! consumes it — degraded picker from the task's own labels when
    //! there are any, a clear error otherwise. The stale request
    //! stash never stays armed.
    use super::super::*;
    use chrono::Utc;
    use lazybox_core::{Task, TaskId, Workspace};
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn pr_task_with_labels(key: &str, labels: &[&str]) -> Task {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        Task {
            author: String::new(),
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
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: labels
                .iter()
                .map(|l| lazybox_core::Label::new(*l))
                .collect(),
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: Some("N1".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    fn repo_labels_error(msg: &str) -> IpcEvent {
        IpcEvent::ProviderError {
            source: "repo-labels".into(),
            message: msg.into(),
            detail: String::new(),
            kind: "retryable".into(),
        }
    }

    #[test]
    fn fetch_failure_with_task_labels_opens_degraded_picker() {
        let mut m = build_model();
        m.status.polling = None;
        let ws = Workspace::from_task(
            pr_task_with_labels("owner/repo#1", &["bug", "p1"]),
            Utc::now(),
        );
        let wk = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));

        m.awaiting_repo_labels = Some(wk);
        m.handle_daemon_event(repo_labels_error("network down"));

        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::ManageLabels),
            "the documented fallback: picker over the task's own labels"
        );
    }

    #[test]
    fn fetch_failure_without_task_labels_raises_clear_error() {
        use crate::realm::components::footer::NoticeSeverity;
        let mut m = build_model();
        m.status.polling = None;
        m.awaiting_repo_labels = Some(lazybox_core::WorkspaceKey::new("github:owner/repo#7"));

        m.handle_daemon_event(repo_labels_error("network down"));

        assert!(
            m.awaiting_repo_labels.is_none(),
            "the stale stash must not stay armed after a failed fetch"
        );
        assert!(m.modal_stack.is_empty(), "nothing to pick from — no modal");
        let n = m.status.notice.as_ref().expect("failure notice");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(
            n.message.contains("couldn't load repo labels"),
            "got {:?}",
            n.message
        );
        assert!(n.message.contains("network down"), "got {:?}", n.message);
    }

    /// Someone else's `g l` (no local stash) must not flash anything.
    #[test]
    fn fetch_failure_without_pending_request_is_ignored() {
        let mut m = build_model();
        m.status.polling = None;
        m.handle_daemon_event(repo_labels_error("network down"));
        assert!(m.status.notice.is_none());
        assert!(m.modal_stack.is_empty());
    }
}

#[cfg(test)]
mod async_modal_preempt_tests {
    //! Async mounts (a slow `RepoLabels` reply) must not preempt a
    //! modal the user opened meanwhile — the daemon prompts already
    //! wait for an empty stack; the label picker now does too.
    use super::super::*;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    #[test]
    fn slow_repo_labels_reply_does_not_preempt_reply_textarea() {
        let mut m = build_model();
        let wk = lazybox_core::WorkspaceKey::new("github:owner/repo#1");
        m.awaiting_repo_labels = Some(wk.clone());

        // User opened the reply composer while the fetch was in flight.
        m.mount_reply(SessionKey::from("github:owner/repo#1"));
        assert_eq!(m.modal_stack.last(), Some(&Id::Reply));

        m.handle_daemon_event(IpcEvent::RepoLabels {
            workspace_key: wk,
            labels: vec![lazybox_core::Label::new("bug")],
        });

        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::Reply),
            "the label picker must not steal keyboard focus from the composer"
        );
        assert!(
            !m.modal_stack.contains(&Id::ManageLabels),
            "picker deferred (dropped), not stacked underneath"
        );
        assert!(
            m.awaiting_repo_labels.is_none(),
            "the stash is disarmed so a stray later reply can't mount unprompted"
        );
    }

    /// The normal path — empty stack — still mounts the picker.
    #[test]
    fn repo_labels_reply_mounts_on_an_empty_stack() {
        let mut m = build_model();
        let wk = lazybox_core::WorkspaceKey::new("github:owner/repo#1");
        m.awaiting_repo_labels = Some(wk.clone());
        m.handle_daemon_event(IpcEvent::RepoLabels {
            workspace_key: wk,
            labels: vec![lazybox_core::Label::new("bug")],
        });
        assert_eq!(m.modal_stack.last(), Some(&Id::ManageLabels));
    }
}

#[cfg(test)]
mod requestable_reviewers_async_mount_tests {
    //! The reviewer picker (`g r`) is now a two-step async flow like the
    //! label picker: ask the daemon for the repo's requestable
    //! reviewers, then mount from the reply. Same don't-preempt contract
    //! (#1092).
    use super::super::*;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{Event as IpcEvent, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    /// Empty stack — the reply mounts the picker with the fetched
    /// requestable reviewers.
    #[test]
    fn requestable_reviewers_reply_mounts_on_an_empty_stack() {
        let mut m = build_model();
        let wk = lazybox_core::WorkspaceKey::new("github:owner/repo#1");
        m.awaiting_requestable_reviewers = Some(wk.clone());
        m.handle_daemon_event(IpcEvent::RequestableReviewers {
            workspace_key: wk,
            logins: vec!["octocat".into(), "hubot".into()],
        });
        assert_eq!(m.modal_stack.last(), Some(&Id::RequestReviewers));
        // The stash is consumed once served so a stray later reply can't
        // re-mount on a stale target.
        assert!(m.awaiting_requestable_reviewers.is_none());
    }

    /// A slow reply must not steal focus from a modal the user opened
    /// while the fetch was in flight.
    #[test]
    fn slow_requestable_reviewers_reply_does_not_preempt_reply_textarea() {
        let mut m = build_model();
        let wk = lazybox_core::WorkspaceKey::new("github:owner/repo#1");
        m.awaiting_requestable_reviewers = Some(wk.clone());
        m.mount_reply(SessionKey::from("github:owner/repo#1"));
        assert_eq!(m.modal_stack.last(), Some(&Id::Reply));

        m.handle_daemon_event(IpcEvent::RequestableReviewers {
            workspace_key: wk,
            logins: vec!["octocat".into()],
        });

        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::Reply),
            "the reviewer picker must not steal focus from the composer"
        );
        assert!(!m.modal_stack.contains(&Id::RequestReviewers));
        assert!(m.awaiting_requestable_reviewers.is_none());
    }

    /// A reply whose key no longer matches the pending request (the
    /// user pressed `g r` on a different workspace, or already
    /// dismissed) is ignored — no picker mounts.
    #[test]
    fn stale_requestable_reviewers_reply_is_ignored() {
        let mut m = build_model();
        m.awaiting_requestable_reviewers =
            Some(lazybox_core::WorkspaceKey::new("github:owner/repo#1"));
        m.handle_daemon_event(IpcEvent::RequestableReviewers {
            workspace_key: lazybox_core::WorkspaceKey::new("github:owner/repo#2"),
            logins: vec!["octocat".into()],
        });
        assert!(m.modal_stack.is_empty());
        assert!(m.awaiting_requestable_reviewers.is_some());
    }

    /// On a fetch *failure* with no interaction-derived fallback
    /// candidates, the flash must be the error — never the misleading
    /// "showing PR participants only" when there are no participants.
    #[test]
    fn failed_fetch_with_no_participants_flashes_error_not_participants_hint() {
        let mut m = build_model();
        // ProviderError is only processed once the initial polling modal
        // is gone.
        m.status.polling = None;
        let wk = lazybox_core::WorkspaceKey::new("github:owner/repo#1");
        m.awaiting_requestable_reviewers = Some(wk.clone());
        // No workspace seeded → gather_candidate_logins yields nobody.
        m.handle_daemon_event(IpcEvent::ProviderError {
            source: "requestable-reviewers".into(),
            message: "boom".into(),
            detail: String::new(),
            kind: "retryable".into(),
        });
        // The empty-state picker still mounts and the stash is consumed…
        assert_eq!(m.modal_stack.last(), Some(&Id::RequestReviewers));
        assert!(m.awaiting_requestable_reviewers.is_none());
        // …but the flash is the error, not the participants hint.
        let logged: Vec<String> = m
            .status
            .messages
            .recent()
            .map(|e| e.message.clone())
            .collect();
        assert!(
            logged
                .iter()
                .any(|msg| msg.contains("couldn't load requestable reviewers")),
            "expected the error flash, got {logged:?}",
        );
        assert!(
            !logged.iter().any(|msg| msg.contains("participants only")),
            "must not claim participants when there are none: {logged:?}",
        );
    }
}

#[cfg(test)]
mod focus_mode_terminal_exit_tests {
    //! Focus mode must not survive an EMPTY stack — the user would be
    //! stranded on a near-fullscreen blank pane. A crashed AGENT is not
    //! an empty stack, though: its pane stays frozen with a restart
    //! affordance (#356), so focus mode must survive it.
    use super::super::*;
    use chrono::Utc;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn workspace(key: &str) -> lazybox_core::Workspace {
        lazybox_core::Workspace::empty(lazybox_core::WorkspaceKey::new(key), "sandbox", Utc::now())
    }

    #[test]
    fn focus_mode_exits_when_shell_dies() {
        let mut m = build_model();
        let ws = workspace("github:owner/repo#1");
        let key: SessionKey = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&key));
        m.sync_panes();

        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: key.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
            model_label: None,
        });
        assert!(m.terminals.active_terminal_id().is_some());
        m.focus_mode = true;
        m.focus = PaneFocus::Terminals;

        m.handle_daemon_event(IpcEvent::TerminalExited {
            terminal_id: TerminalId(7),
            exit_code: Some(0),
            last_output: None,
        });

        assert!(
            !m.focus_mode,
            "a shell exit empties the stack — focus mode over a blank pane must exit"
        );
        assert_eq!(m.focus, PaneFocus::Sidebar);
    }

    #[test]
    fn focus_mode_survives_agent_crash() {
        let mut m = build_model();
        let ws = workspace("github:owner/repo#1");
        let key: SessionKey = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&key));
        m.sync_panes();

        m.status.note_spawning(
            "claude",
            key.clone(),
            TerminalKind::Agent("claude".into()),
            0,
        );
        m.handle_daemon_event(IpcEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
        assert!(m.terminals.active_terminal_id().is_some());
        m.focus_mode = true;
        m.focus = PaneFocus::Terminals;

        m.handle_daemon_event(IpcEvent::TerminalExited {
            terminal_id: TerminalId(7),
            exit_code: Some(1),
            last_output: None,
        });

        // The crashed agent keeps its pane (frozen + "restart?"), so the
        // stack isn't empty and focus mode stays put — the user lands on
        // the restart affordance instead of being bounced to the sidebar
        // with the workspace seemingly gone (#356).
        assert!(
            m.focus_mode,
            "a crashed agent leaves a restart pane — focus mode must survive"
        );
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert!(m.terminals.active_terminal_id().is_some());
    }

    /// A terminal dying in ANOTHER workspace leaves focus mode alone.
    #[test]
    fn focus_mode_survives_other_workspaces_terminal_exit() {
        let mut m = build_model();
        let ws = workspace("github:owner/repo#1");
        let key: SessionKey = SessionKey::from(&ws.key);
        let other = SessionKey::from("github:owner/repo#2");
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&key));
        m.sync_panes();

        for (id, sk) in [(7u64, &key), (8u64, &other)] {
            m.handle_daemon_event(IpcEvent::TerminalSpawned {
                terminal_id: TerminalId(id),
                session_key: sk.clone(),
                kind: TerminalKind::Agent("claude".into()),
                no_permission: false,
                on_main: false,
                model_label: None,
            });
        }
        m.focus_mode = true;
        m.focus = PaneFocus::Terminals;

        m.handle_daemon_event(IpcEvent::TerminalExited {
            terminal_id: TerminalId(8),
            exit_code: Some(0),
            last_output: None,
        });
        assert!(
            m.focus_mode,
            "an unrelated workspace's terminal exit must not drop focus mode"
        );
    }
}

#[cfg(test)]
mod spinner_redraw_gate_tests {
    //! An active spinner must only request a redraw when its glyph
    //! actually advances (80ms cadence), not on every ~16ms run-loop
    //! heartbeat for the whole poll/provision duration.
    use super::super::*;
    use lazybox_ipc::channel;
    use std::time::{Duration, Instant};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    #[test]
    fn spinner_presence_alone_does_not_redraw_every_heartbeat() {
        let mut m = build_model();
        m.status.note_poll_progress("github", "Querying…");

        // Cadence gate closed (fresh heartbeat): no redraw request.
        m.status.polling_last_tick = Instant::now();
        m.redraw = false;
        let _ = m.polling_tick();
        assert!(
            !m.redraw,
            "an idle heartbeat between spinner frames must not repaint"
        );

        // Cadence gate open: the glyph advances → redraw.
        m.status.polling_last_tick = Instant::now() - Duration::from_millis(200);
        let _ = m.polling_tick();
        assert!(m.redraw, "an advanced glyph must repaint");
    }
}

#[cfg(test)]
mod keybinding_audit_tests {
    //! Regression tests for the keybinding/catalog audit batch:
    //!
    //! 1. `g` in the activity pane jumps to the top (catalog
    //!    `ActivityTop`) instead of arming the Workspace `g *` github
    //!    leader — the vim scrolling reflex `g g` must never toggle
    //!    auto-merge on the PR again.
    //! 2. `CyclePane` dispatches through the catalog, so the vim
    //!    preset's `Ctrl-w` remap actually cycles (and Tab stops).
    //! 7. Merge / close-issue emit a pending footer notice at command
    //!    send.
    //! 10. A `quit: "q x"` override quits on `q x`, not on `q q`.
    //! 11c. The mouse-capture toggle is catalog-backed: default chords
    //!    fire and a remap moves them.
    use super::super::*;
    use chrono::Utc;
    use lazybox_core::{SessionKey, Task, TaskId, Workspace};
    use lazybox_ipc::{Command as IpcCommand, Event as IpcEvent, channel};
    use lazybox_tui_core::action::Action;
    use tuirealm::event::{Key, KeyEvent, KeyModifiers};
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
    ) {
        let (client, server) = channel::pair();
        let m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        (m, server)
    }

    fn press(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>, code: Key) {
        m.dispatch_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Without a `sandbox:` box there are no `r <agent>` rows, so `r` is
    /// not a leader — it fires Reply directly, exactly as before #965.
    #[test]
    fn r_replies_directly_when_no_box_is_configured() {
        let (mut m, _conn) = build_model();
        let pr = pr_workspace("owner/repo#1", 0);
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        press(&mut m, Key::Char('r'));
        assert!(m.leader_pending().is_none(), "no box → `r` is not a leader");
        assert_eq!(m.modal_stack.last(), Some(&Id::Reply));
    }

    /// With a box configured, the `r <agent>` family shares `r` with
    /// Reply (same Workspace section): `r` arms the leader and the
    /// same-key double-tap (`r r`) fires the shadowed Reply — the
    /// documented `leader_fallback` contract, previously untested.
    #[test]
    fn r_arms_the_remote_leader_and_r_r_still_replies() {
        let (m, _conn) = build_model();
        let (box_tx, _box_rx) = tokio::sync::mpsc::channel(16);
        let (_box_evt_tx, box_evt_rx) = tokio::sync::mpsc::channel(16);
        let mut clients = std::collections::BTreeMap::new();
        clients.insert(
            "box".to_string(),
            lazybox_ipc::Client::from_bounded_channels(box_tx, box_evt_rx),
        );
        let mut m = m.with_remote_clients(clients, Some("box".to_string()));
        let pr = pr_workspace("owner/repo#1", 0);
        let sk: SessionKey = (&pr.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(pr)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        press(&mut m, Key::Char('r'));
        assert!(
            m.leader_pending().is_some(),
            "a configured box makes `r` the remote-spawn leader"
        );
        assert!(m.modal_stack.is_empty(), "arming must not mount Reply yet");

        press(&mut m, Key::Char('r'));
        assert!(m.leader_pending().is_none(), "the double-tap disarms");
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::Reply),
            "`r r` must fall back to the shadowed Reply"
        );
    }

    /// A merge-ready PR workspace (CI green, mergeable, own PR) with
    /// `activity_rows` comments — the state where both the activity
    /// cursor keys and the github leader are live.
    fn pr_workspace(key: &str, activity_rows: usize) -> Workspace {
        let num = key.rsplit_once('#').map(|(_, n)| n).unwrap_or("1");
        let task = Task {
            author: String::new(),
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
            url: format!("https://github.com/o/r/pull/{num}"),
            repo: Some("o/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        };
        let mut ws = Workspace::from_task(task, Utc::now());
        for i in 0..activity_rows {
            ws.activity.push(lazybox_core::Activity {
                author: format!("user{i}"),
                body: format!("comment {i}"),
                created_at: Utc::now(),
                kind: lazybox_core::ActivityKind::Comment,
                node_id: None,
                path: None,
                line: None,
                diff_hunk: None,
                thread_id: None,
            });
        }
        ws
    }

    /// Seed a model focused on a PR workspace with activity, with the
    /// Right (activity) pane holding key focus.
    fn model_on_activity_pane() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
    ) {
        let (mut m, server) = build_model();
        let ws = pr_workspace("github:o/r#1", 3);
        let sk = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk), "PR row focusable");
        m.sync_panes();
        m.focus = PaneFocus::Right;
        m.set_focus_attr();
        (m, server)
    }

    // ── 1: `g` in the activity pane ────────────────────────────────

    #[test]
    fn activity_g_jumps_to_top_without_arming_the_github_leader() {
        let (mut m, _server) = model_on_activity_pane();
        // Move the row cursor off the top first.
        press(&mut m, Key::Char('j'));
        press(&mut m, Key::Char('j'));
        assert_eq!(m.right.comment_cursor(), 2, "j/j moved the cursor");

        press(&mut m, Key::Char('g'));
        assert_eq!(
            m.right.comment_cursor(),
            0,
            "`g` jumps the cursor to the top"
        );
        assert!(
            m.leader_pending().is_none(),
            "`g` under Right focus must NOT arm the github leader",
        );
    }

    #[test]
    fn activity_gg_scroll_reflex_does_not_toggle_auto_merge() {
        let (mut m, mut server) = model_on_activity_pane();
        while server.rx.try_recv().is_ok() {}

        press(&mut m, Key::Char('g'));
        press(&mut m, Key::Char('g'));
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                !matches!(cmd, IpcCommand::SetAutoMergeOnGreen { .. }),
                "`g g` in the activity pane must never arm auto-merge",
            );
        }
        let notice = m.status.notice.as_ref().map(|n| n.message.clone());
        assert!(
            !notice.unwrap_or_default().contains("auto-merge"),
            "no auto-merge notice may flash on the g g scroll reflex",
        );
    }

    #[test]
    fn activity_shift_g_jumps_to_bottom() {
        let (mut m, _server) = model_on_activity_pane();
        m.dispatch_key(KeyEvent::new(Key::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(
            m.right.comment_cursor(),
            2,
            "`Shift-G` jumps the cursor to the last activity row",
        );
    }

    #[test]
    fn sidebar_g_still_arms_the_github_leader_and_g_m_reaches_merge() {
        let (mut m, _server) = model_on_activity_pane();
        // Back to the sidebar — the github leader lives there.
        m.focus = PaneFocus::Sidebar;
        m.set_focus_attr();

        press(&mut m, Key::Char('g'));
        assert!(
            m.leader_pending().is_some(),
            "`g` under Sidebar focus arms the github leader",
        );
        press(&mut m, Key::Char('m'));
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::ActionConfirm),
            "`g m` completes to the merge confirm",
        );
        assert!(matches!(
            m.modal_flow,
            Some(super::super::ModalFlow::ActionConfirm {
                action: Action::MergePr,
                ..
            }),
        ));
    }

    // ── 2: CyclePane through the catalog (vim preset) ──────────────

    #[test]
    fn default_preset_tab_cycles_panes() {
        let (mut m, _server) = build_model();
        assert_eq!(m.focus(), PaneFocus::Sidebar);
        press(&mut m, Key::Tab);
        assert_ne!(m.focus(), PaneFocus::Sidebar, "Tab cycles off the sidebar");
    }

    #[test]
    fn vim_preset_ctrl_w_cycles_panes_and_tab_stops() {
        let (mut m, _server) = build_model();
        m.apply_action_key_overrides(
            lazybox_tui_core::action::keymap_preset("vim").expect("vim preset"),
        );
        // The remapped chord fires…
        m.dispatch_key(KeyEvent::new(Key::Char('w'), KeyModifiers::CONTROL));
        assert_ne!(
            m.focus(),
            PaneFocus::Sidebar,
            "vim preset: Ctrl-w cycles panes",
        );
        // …and the displaced default no longer does.
        let (mut m2, _server2) = build_model();
        m2.apply_action_key_overrides(
            lazybox_tui_core::action::keymap_preset("vim").expect("vim preset"),
        );
        press(&mut m2, Key::Tab);
        assert_eq!(
            m2.focus(),
            PaneFocus::Sidebar,
            "vim preset: Tab no longer cycles (the override moved the chord)",
        );
    }

    // ── 7: pending feedback on merge / close ───────────────────────

    #[test]
    fn confirmed_merge_flashes_a_pending_notice() {
        let (mut m, _server) = build_model();
        let ws = pr_workspace("github:o/r#42", 0);
        let sk = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        let cmds = m.dispatch_action(&Action::MergePr);
        assert!(cmds.is_empty(), "merge gates on confirm");
        let cmds = m.handle_confirmed(true);
        assert!(matches!(cmds.as_slice(), [IpcCommand::MergePr { .. }]));
        let notice = m
            .status
            .notice
            .as_ref()
            .map(|n| n.message.clone())
            .unwrap_or_default();
        assert!(
            notice.contains("merging PR #42"),
            "pending merge feedback missing: {notice:?}",
        );
    }

    #[test]
    fn confirmed_close_issue_flashes_a_pending_notice() {
        let (mut m, _server) = build_model();
        // Reshape the PR fixture into an open issue workspace — the
        // only shape CloseIssue is offered on.
        let mut ws = pr_workspace("github:o/r#7", 0);
        let mut issue = ws.pr.take().expect("fixture has a PR to reshape");
        issue.url = "https://github.com/o/r/issues/7".into();
        ws.attach_task(issue);
        let sk = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        let cmds = m.dispatch_action(&Action::CloseIssue);
        assert!(cmds.is_empty(), "close gates on confirm");
        let cmds = m.handle_confirmed(true);
        assert!(matches!(cmds.as_slice(), [IpcCommand::CloseIssue { .. }]));
        let notice = m
            .status
            .notice
            .as_ref()
            .map(|n| n.message.clone())
            .unwrap_or_default();
        assert!(
            notice.contains("closing issue #7"),
            "pending close feedback missing: {notice:?}",
        );
    }

    #[test]
    fn confirmed_delete_or_close_flashes_kind_specific_notices() {
        // PR workspace → "closing PR #42…".
        let (mut m, _server) = build_model();
        let ws = pr_workspace("github:o/r#42", 0);
        let sk = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        let cmds = m.dispatch_action(&Action::DeleteOrClose);
        assert!(cmds.is_empty(), "delete/close gates on confirm");
        let cmds = m.handle_confirmed(true);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::DeleteOrClose { .. }]
        ));
        let notice = m
            .status
            .notice
            .as_ref()
            .map(|n| n.message.clone())
            .unwrap_or_default();
        assert!(
            notice.contains("closing PR #42"),
            "pending close feedback missing: {notice:?}",
        );

        // Issue workspace → "deleting issue #7…".
        let (mut m, _server) = build_model();
        let mut ws = pr_workspace("github:o/r#7", 0);
        let mut issue = ws.pr.take().expect("fixture has a PR to reshape");
        issue.url = "https://github.com/o/r/issues/7".into();
        ws.attach_task(issue);
        let sk = SessionKey::from(&ws.key);
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));

        let cmds = m.dispatch_action(&Action::DeleteOrClose);
        assert!(cmds.is_empty(), "delete/close gates on confirm");
        let cmds = m.handle_confirmed(true);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::DeleteOrClose { .. }]
        ));
        let notice = m
            .status
            .notice
            .as_ref()
            .map(|n| n.message.clone())
            .unwrap_or_default();
        assert!(
            notice.contains("deleting issue #7"),
            "pending delete feedback missing: {notice:?}",
        );
    }

    // ── 10: quit-chord grammar ──────────────────────────────────────

    #[test]
    fn quit_override_with_distinct_strokes_quits_on_the_sequence() {
        let (mut m, _server) = build_model();
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("quit".to_string(), "q x".to_string());
        m.apply_action_key_overrides(overrides);

        press(&mut m, Key::Char('q'));
        assert!(!m.quit, "head press only arms");
        press(&mut m, Key::Char('x'));
        assert!(m.quit, "`q x` completes the override chord");
    }

    #[test]
    fn quit_override_with_distinct_strokes_does_not_quit_on_double_head() {
        let (mut m, _server) = build_model();
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("quit".to_string(), "q x".to_string());
        m.apply_action_key_overrides(overrides);

        press(&mut m, Key::Char('q'));
        press(&mut m, Key::Char('q'));
        assert!(
            !m.quit,
            "`quit: \"q x\"` must not fire on `q q` — that was the head-only bug",
        );
    }

    #[test]
    fn default_double_press_quit_still_fires() {
        let (mut m, _server) = build_model();
        press(&mut m, Key::Char('q'));
        assert!(!m.quit);
        press(&mut m, Key::Char('q'));
        assert!(m.quit, "default `q q` still quits");
    }

    // ── 11c: mouse-capture toggle is catalog-backed ─────────────────

    #[test]
    fn mouse_capture_default_chords_toggle_and_remap_moves_them() {
        let (mut m, _server) = build_model();
        let initial = m.mouse_capture_on;
        press(&mut m, Key::Function(8));
        assert_eq!(m.mouse_capture_on, !initial, "F8 toggles capture");
        m.dispatch_key(KeyEvent::new(Key::Char('s'), KeyModifiers::ALT));
        assert_eq!(m.mouse_capture_on, initial, "Alt-s toggles it back");

        // A remap moves the binding: the old default goes dead, the
        // new chord fires.
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("toggle_mouse_capture".to_string(), "F6".to_string());
        m.apply_action_key_overrides(overrides);
        press(&mut m, Key::Function(8));
        assert_eq!(
            m.mouse_capture_on, initial,
            "remapped: F8 no longer toggles"
        );
        press(&mut m, Key::Function(6));
        assert_eq!(m.mouse_capture_on, !initial, "remapped: F6 toggles");
    }
}

#[cfg(test)]
mod dispatch_coverage_tests {
    //! House-pattern invariant (#5 of the keybinding audit): every
    //! `ActionKind` in the generated runtime catalog must either
    //! resolve to a dispatchable `Action` via `action_from_entry`, or
    //! sit on the explicit, commented allowlist of pane-native kinds
    //! (`keys::PANE_NATIVE_KINDS`, each entry naming the match arm
    //! that consumes it). A new catalog row wired to neither fails the
    //! build here instead of rendering everywhere and no-oping.
    use super::super::keys::{PANE_NATIVE_KINDS, action_from_entry};
    use lazybox_tui_core::action::ActionDef;

    #[test]
    fn every_catalog_row_dispatches_or_is_allowlisted() {
        let agents: Vec<String> = ["claude", "codex", "cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let tiers = lazybox_core::AgentModels::builtin("claude")
            .expect("claude builtin tiers")
            .tiers;
        let catalog =
            ActionDef::catalog_with_tiers(&agents, &std::collections::BTreeMap::new(), &tiers);
        for entry in &catalog {
            let dispatchable = action_from_entry(entry).is_some();
            let allowlisted = PANE_NATIVE_KINDS.iter().any(|(k, _, _)| *k == entry.kind);
            assert!(
                dispatchable || allowlisted,
                "catalog row {:?} (param {:?}, keys {:?}) neither dispatches through \
                 `dispatch_action` nor sits on keys::PANE_NATIVE_KINDS — it would render \
                 in Ask Lazybox and the footer while silently no-oping on the keyboard. Wire a \
                 dispatch arm (action_from_kind + dispatch_action) or allowlist it with \
                 the pane match arm that handles it.",
                entry.kind,
                entry.param,
                entry.keys_display,
            );
            assert!(
                !(dispatchable && allowlisted),
                "catalog row {:?} is BOTH dispatchable and allowlisted — remove the stale \
                 PANE_NATIVE_KINDS entry so the allowlist stays the exact silent-fallback set",
                entry.kind,
            );
        }
        // Allowlist hygiene in the other direction: every allowlisted
        // kind must still exist in the catalog (a removed action must
        // take its allowlist row with it).
        for (kind, site, _) in PANE_NATIVE_KINDS {
            assert!(
                catalog.iter().any(|e| e.kind == *kind),
                "stale PANE_NATIVE_KINDS entry {kind:?} ({site}) — not in the catalog",
            );
        }
    }
}

#[cfg(test)]
mod keymap_validation_tests {
    //! Startup `ui.action_keys` / `ui.keymap_preset` validation (#4 of
    //! the keybinding audit): each misconfiguration class produces a
    //! warning, and the model surfaces them (footer notice + messages
    //! log) without rejecting the config.
    use super::super::helpers::keymap_config_warnings;
    use super::super::*;
    use lazybox_ipc::channel;
    use lazybox_tui_core::action::ActionDef;
    use std::collections::BTreeMap;
    use tuirealm::ratatui::layout::Size;

    fn agents() -> Vec<String> {
        ["claude", "codex", "cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn warnings_for(overrides: BTreeMap<String, String>) -> Vec<String> {
        let catalog = ActionDef::catalog(&agents(), &overrides);
        keymap_config_warnings(&overrides, &catalog)
    }

    #[test]
    fn clean_config_produces_no_warnings() {
        assert_eq!(warnings_for(BTreeMap::new()), Vec::<String>::new());
        // The shipped vim preset is clean too.
        let vim = lazybox_tui_core::action::keymap_preset("vim").unwrap();
        assert_eq!(warnings_for(vim), Vec::<String>::new());
    }

    #[test]
    fn unknown_config_key_warns() {
        let mut o = BTreeMap::new();
        o.insert("mrege_pr".to_string(), "Ctrl-m".to_string());
        let w = warnings_for(o);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("mrege_pr"), "{w:?}");
        assert!(w[0].contains("unknown"), "{w:?}");
    }

    #[test]
    fn pane_native_override_warns_as_ineffective() {
        let mut o = BTreeMap::new();
        o.insert("toggle_row".to_string(), "Ctrl-e".to_string());
        let w = warnings_for(o);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("toggle_row"), "{w:?}");
        assert!(w[0].contains("no effect"), "{w:?}");
    }

    #[test]
    fn override_shadowing_a_leader_family_warns() {
        // The motivating case: `refresh: "w"` silently killed every
        // `w <key>` chord (work-in-agent, tiers).
        let mut o = BTreeMap::new();
        o.insert("refresh".to_string(), "w".to_string());
        let w = warnings_for(o);
        assert!(
            w.iter()
                .any(|w| w.contains("refresh") && w.contains("leader")),
            "leader-shadow warning missing: {w:?}",
        );
    }

    #[test]
    fn override_induced_same_rank_collision_warns() {
        let mut o = BTreeMap::new();
        // Two Global actions on the same key — same (focus, rank).
        o.insert("refresh".to_string(), "F5".to_string());
        o.insert("open_tour".to_string(), "F5".to_string());
        let w = warnings_for(o);
        assert!(
            w.iter().any(|w| w.contains("unreachable")),
            "same-rank collision warning missing: {w:?}",
        );
    }

    #[test]
    fn model_surfaces_warnings_in_footer_and_messages_log() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let mut o = BTreeMap::new();
        o.insert("not_a_real_action".to_string(), "F5".to_string());
        m.apply_action_key_overrides(o);
        m.surface_keymap_warnings(Vec::new());
        let notice = m
            .status
            .notice
            .as_ref()
            .map(|n| n.message.clone())
            .unwrap_or_default();
        assert!(
            notice.contains("keymap config"),
            "footer summary missing: {notice:?}",
        );
        // The unknown-preset warning path rides the same surface.
        let (client2, _server2) = channel::pair();
        let mut m2 = Model::new_for_test(client2, Size::new(120, 40)).expect("model init");
        m2.surface_keymap_warnings(
            lazybox_tui_core::action::unknown_preset_warning("emacs")
                .into_iter()
                .collect(),
        );
        let notice2 = m2
            .status
            .notice
            .as_ref()
            .map(|n| n.message.clone())
            .unwrap_or_default();
        assert!(notice2.contains("keymap config"), "{notice2:?}");
    }

    #[test]
    fn clean_model_stays_quiet() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.surface_keymap_warnings(Vec::new());
        assert!(
            m.status.notice.is_none(),
            "no warning notice on a clean keymap",
        );
    }
}

#[cfg(test)]
mod settings_window_tests {
    //! The `,` Settings window: tabbed (Providers / Agents /
    //! Appearance / Maintenance) instead of the old flat 11+ row
    //! palette. These tests pin the model-side contract — grouped
    //! flat ordering, the flat-index pick round-trip through the
    //! unchanged `ChoicePicked` envelope, and stash hygiene on Esc.
    use super::super::*;
    use crate::realm::setup_ctx::{SettingsAction, SettingsSection};
    use lazybox_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    fn build_model_with_setup() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let mut persisted = lazybox_core::PersistedSetup::default();
        persisted.enabled_providers.insert("github".into());
        m.cache_persisted_setup(persisted);
        m
    }

    #[test]
    fn open_settings_mounts_the_tabbed_window_with_grouped_actions() {
        let mut m = build_model_with_setup();
        m.open_settings();
        assert_eq!(m.modal_stack.last(), Some(&Id::Setup));
        assert!(!m.setup.settings_actions.is_empty());

        // The flat stash is in tab order: section indices must be
        // non-decreasing, with Providers first and Maintenance last.
        let order = |s: SettingsSection| {
            SettingsSection::ALL
                .iter()
                .position(|x| *x == s)
                .expect("section in ALL")
        };
        let sections: Vec<usize> = m
            .setup
            .settings_actions
            .iter()
            .map(|a| order(a.section()))
            .collect();
        assert!(
            sections.windows(2).all(|w| w[0] <= w[1]),
            "flat stash must be grouped in tab order, got {sections:?}"
        );
        assert_eq!(*sections.first().unwrap(), 0, "Providers first");
        assert_eq!(
            *sections.last().unwrap(),
            SettingsSection::ALL.len() - 1,
            "Maintenance last"
        );
        // Every section has at least one row with one provider enabled.
        for (i, _) in SettingsSection::ALL.iter().enumerate() {
            assert!(sections.contains(&i), "section {i} must have rows");
        }
    }

    /// The component emits `ChoicePicked([flat_idx])`; the existing
    /// handler must resolve it against the grouped stash — picking
    /// the theme row mounts the theme picker.
    #[test]
    fn picking_a_flat_index_dispatches_that_action() {
        let mut m = build_model_with_setup();
        m.open_settings();
        let theme_idx = m
            .setup
            .settings_actions
            .iter()
            .position(|a| matches!(a, SettingsAction::EditTheme { .. }))
            .expect("theme row present");
        let _ = m.handle_choice_picked(vec![ChoicePayload::Index(theme_idx)]);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::ThemePicker),
            "the flat-index pick must route to the right action"
        );
    }

    /// Esc on the Settings window drops the stashed rows so a stale
    /// flat index can never resolve against a later Setup-id mount.
    #[test]
    fn dismissing_settings_clears_the_stash() {
        let mut m = build_model_with_setup();
        m.open_settings();
        assert!(!m.setup.settings_actions.is_empty());
        let _ = m.handle_modal_dismissed();
        assert!(m.modal_stack.is_empty());
        assert!(
            m.setup.settings_actions.is_empty(),
            "Esc must drop the stashed settings rows"
        );
    }

    /// The theme row surfaces the CURRENT theme name, matching the
    /// state-bearing labels of its siblings.
    #[test]
    fn theme_row_names_the_active_theme() {
        let mut m = build_model_with_setup();
        m.open_settings();
        let current = crate::theme::current().name;
        assert!(
            m.setup
                .settings_actions
                .iter()
                .any(|a| a.label() == format!("Change theme (live preview) · {current}")),
            "theme row must show the active theme"
        );
    }

    #[test]
    fn shell_row_uses_the_daemons_resolved_command() {
        let mut m = build_model_with_setup();
        m.handle_daemon_event(lazybox_ipc::Event::ShellCommandConfig {
            command: "/remote/bin/fish".into(),
            configured: true,
        });
        m.open_settings();
        assert!(
            m.setup.settings_actions.iter().any(|action| matches!(
                action,
                SettingsAction::ShellCommand {
                    command,
                    configured: true,
                } if command == "/remote/bin/fish"
            )),
            "shell row must show the command reported by the PTY-owning daemon"
        );
    }
}

#[cfg(test)]
mod remote_agent_availability_tests {
    //! `Event::AgentAvailabilityConfig` (#742): the daemon reports the
    //! agents it is configured to run, and a remote (`--connect`) client
    //! adopts that set so it offers the agents the *box* runs — not the
    //! hardcoded trio it defaults to when its own local config never
    //! applies over the socket. An embedded client ignores it.
    use super::super::keys::action_from_entry;
    use super::super::*;
    use lazybox_ipc::{Event as IpcEvent, channel};
    use lazybox_tui_core::action::Action;
    use tuirealm::ratatui::layout::Size;

    fn spawnable_agent_ids(m: &Model<tuirealm::terminal::TestTerminalAdapter>) -> Vec<String> {
        m.catalog()
            .iter()
            .filter_map(|entry| match action_from_entry(entry) {
                Some(Action::SpawnAgent(id)) => Some(id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn remote_client_adopts_the_daemons_agent_set() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40))
            .expect("model init")
            .with_remote();
        // Before the daemon reports, a remote client only knows the
        // hardcoded trio (its own local config never applied).
        assert_eq!(spawnable_agent_ids(&m), vec!["claude", "codex", "cursor"]);

        m.handle_daemon_event(IpcEvent::AgentAvailabilityConfig {
            agents: vec!["claude".into(), "codex".into()],
            default_agent: None,
        });

        assert_eq!(
            spawnable_agent_ids(&m),
            vec!["claude".to_string(), "codex".to_string()],
            "a remote client offers exactly the agents the box reports — no phantom cursor",
        );
    }

    #[test]
    fn remote_client_adopts_the_daemons_default_agent() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40))
            .expect("model init")
            .with_remote();
        // The client default is `claude`; the box's configured default is
        // `codex`, and both are offered. The remote client must honor the
        // box's choice so `w` works in the box's default agent.
        assert_eq!(m.sidebar.default_agent(), "claude");

        m.handle_daemon_event(IpcEvent::AgentAvailabilityConfig {
            agents: vec!["claude".into(), "codex".into()],
            default_agent: Some("codex".into()),
        });

        assert_eq!(
            m.sidebar.default_agent(),
            "codex",
            "a remote client defaults `w` to the agent the box is configured to prefer",
        );
    }

    #[test]
    fn remote_client_reconciles_a_default_the_box_set_excludes() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40))
            .expect("model init")
            .with_remote();
        // Client default is `claude`, but the box runs only codex and set
        // no explicit default. Without reconciliation `w` would spawn
        // claude — an agent the box doesn't offer.
        m.handle_daemon_event(IpcEvent::AgentAvailabilityConfig {
            agents: vec!["codex".into()],
            default_agent: None,
        });

        assert_eq!(spawnable_agent_ids(&m), vec!["codex".to_string()]);
        assert_eq!(
            m.sidebar.default_agent(),
            "codex",
            "the default work agent must stay inside the offered set",
        );
    }

    #[test]
    fn embedded_client_ignores_the_daemons_agent_set() {
        let (client, _server) = channel::pair();
        // No `with_remote()`: the embedded client already applied its
        // authoritative local config (the same file the daemon reads).
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let before = spawnable_agent_ids(&m);
        let default_before = m.sidebar.default_agent().to_string();

        m.handle_daemon_event(IpcEvent::AgentAvailabilityConfig {
            agents: vec!["only-on-the-box".into()],
            default_agent: Some("only-on-the-box".into()),
        });

        assert_eq!(
            spawnable_agent_ids(&m),
            before,
            "an embedded client must not adopt a remote agent set over its own config",
        );
        assert_eq!(
            m.sidebar.default_agent(),
            default_before,
            "an embedded client must not adopt a remote default agent",
        );
    }

    #[test]
    fn set_agents_keeps_the_default_inside_the_enabled_set() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        // Default is `claude`; enable a set that omits it (a codex-only
        // box, or a codex-only local config). The default must move to a
        // spawnable agent rather than stay at the un-offered `claude`.
        assert_eq!(m.sidebar.default_agent(), "claude");
        m.set_agents(vec!["codex".into()]);
        assert_eq!(
            m.sidebar.default_agent(),
            "codex",
            "set_agents must reconcile a default the enabled set excludes",
        );

        // A default already inside the set is left untouched.
        m.set_default_agent("codex");
        m.set_agents(vec!["claude".into(), "codex".into()]);
        assert_eq!(
            m.sidebar.default_agent(),
            "codex",
            "set_agents must not disturb a default that is still enabled",
        );
    }
}

#[cfg(test)]
mod agent_cli_update_tests {
    //! Lazybox-managed agent-CLI updates (#400): the daemon's check /
    //! update-finished events become footer notices, and the Settings
    //! Maintenance rows dispatch the check/update commands.
    use super::super::*;
    use crate::realm::components::footer::NoticeSeverity;
    use crate::realm::setup_ctx::SettingsAction;
    use lazybox_ipc::{AgentCliUpdateStatus, Client, Event as IpcEvent, channel};
    use tokio::sync::mpsc;
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    fn status(
        id: &str,
        name: &str,
        installed: Option<&str>,
        latest: Option<&str>,
        error: Option<&str>,
    ) -> AgentCliUpdateStatus {
        let update_available = matches!((installed, latest), (Some(i), Some(l)) if i != l);
        AgentCliUpdateStatus {
            agent_id: id.into(),
            display_name: name.into(),
            installed: installed.map(Into::into),
            latest: latest.map(Into::into),
            update_available,
            error: error.map(Into::into),
            auto_update: false,
        }
    }

    #[test]
    fn available_update_flashes_even_on_scheduled_checks() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::AgentCliUpdatesChecked {
            statuses: vec![status(
                "claude",
                "Claude Code",
                Some("2.1.3"),
                Some("2.1.4"),
                None,
            )],
            manual: false,
        });
        let n = m.status.notice.as_ref().expect("availability notice");
        assert_eq!(n.severity, NoticeSeverity::Info);
        assert!(
            n.message.contains("Claude Code 2.1.3 → 2.1.4"),
            "{}",
            n.message
        );
    }

    #[test]
    fn scheduled_check_with_nothing_actionable_stays_quiet() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::AgentCliUpdatesChecked {
            statuses: vec![
                status("claude", "Claude Code", Some("2.1.4"), Some("2.1.4"), None),
                status(
                    "codex",
                    "Codex",
                    None,
                    None,
                    Some("`codex --version` timed out"),
                ),
            ],
            manual: false,
        });
        assert!(
            m.status.notice.is_none(),
            "a quiet scheduled sweep must not flash, got {:?}",
            m.status.notice
        );
    }

    /// A scheduled sweep announcing an update the daemon will apply
    /// itself must say "auto-updating", not send the user to the
    /// maintenance menu to race the running pass.
    #[test]
    fn scheduled_check_words_auto_update_agents_as_auto_updating() {
        let mut m = build_model();
        let mut auto = status("claude", "Claude Code", Some("2.1.3"), Some("2.1.4"), None);
        auto.auto_update = true;
        m.handle_daemon_event(IpcEvent::AgentCliUpdatesChecked {
            statuses: vec![auto.clone()],
            manual: false,
        });
        let n = m.status.notice.as_ref().expect("auto-updating notice");
        assert!(n.message.contains("auto-updating"), "{}", n.message);
        assert!(
            !n.message.contains("maintenance"),
            "must not instruct a manual update: {}",
            n.message
        );

        // The same agent on a MANUAL check is the user's to update.
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::AgentCliUpdatesChecked {
            statuses: vec![auto],
            manual: true,
        });
        let n = m.status.notice.as_ref().expect("availability notice");
        assert!(n.message.contains("maintenance"), "{}", n.message);
    }

    /// A manual check must never swallow a broken agent's probe error
    /// just because another agent has an update available — the sticky
    /// error wins the footer (both land in the Shift-M log).
    #[test]
    fn manual_check_reports_errors_even_when_updates_are_available() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::AgentCliUpdatesChecked {
            statuses: vec![
                status("claude", "Claude Code", Some("2.1.3"), Some("2.1.4"), None),
                status(
                    "codex",
                    "Codex",
                    None,
                    None,
                    Some("`codex --version` failed"),
                ),
            ],
            manual: true,
        });
        let n = m.status.notice.as_ref().expect("error notice");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("Codex"), "{}", n.message);
    }

    #[test]
    fn manual_check_answers_up_to_date_with_versions() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::AgentCliUpdatesChecked {
            statuses: vec![status(
                "claude",
                "Claude Code",
                Some("2.1.4"),
                Some("2.1.4"),
                None,
            )],
            manual: true,
        });
        let n = m.status.notice.as_ref().expect("up-to-date notice");
        assert_eq!(n.severity, NoticeSeverity::Info);
        assert!(n.message.contains("up to date"), "{}", n.message);
        assert!(n.message.contains("Claude Code 2.1.4"), "{}", n.message);
    }

    #[test]
    fn manual_check_surfaces_probe_errors() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::AgentCliUpdatesChecked {
            statuses: vec![status(
                "codex",
                "Codex",
                None,
                None,
                Some("`codex --version` failed to start"),
            )],
            manual: true,
        });
        let n = m.status.notice.as_ref().expect("error notice");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("Codex"), "{}", n.message);
        assert!(n.message.contains("failed to start"), "{}", n.message);
    }

    #[test]
    fn update_finished_reports_success_and_failure() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::AgentCliUpdateFinished {
            agent_id: "claude".into(),
            display_name: "Claude Code".into(),
            ok: true,
            installed_before: Some("2.1.3".into()),
            installed_after: Some("2.1.4".into()),
            message: "updated 2.1.3 → 2.1.4".into(),
        });
        let n = m.status.notice.as_ref().expect("success notice");
        assert_eq!(n.severity, NoticeSeverity::Info);
        assert!(n.message.contains("✓ Claude Code"), "{}", n.message);
        assert!(n.message.contains("updated 2.1.3 → 2.1.4"), "{}", n.message);

        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::AgentCliUpdateFinished {
            agent_id: "codex".into(),
            display_name: "Codex".into(),
            ok: false,
            installed_before: Some("0.46.0".into()),
            installed_after: None,
            message: "`brew upgrade --cask codex` exited 1: lock held".into(),
        });
        let n = m.status.notice.as_ref().expect("failure notice");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("Codex update failed"), "{}", n.message);
        assert!(n.message.contains("lock held"), "{}", n.message);
    }

    /// The Settings Maintenance rows exist and fire the daemon
    /// commands — the manual "update agents now" surface.
    #[test]
    fn settings_actions_dispatch_check_and_update_commands() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (_evt_tx, evt_rx) = mpsc::channel(8);
        let client = Client::from_channels(cmd_tx, evt_rx);
        let mut m = Model::<tuirealm::terminal::TestTerminalAdapter>::new_for_test(
            client,
            Size::new(120, 40),
        )
        .expect("model init");
        let mut persisted = lazybox_core::PersistedSetup::default();
        persisted.enabled_providers.insert("github".into());
        m.cache_persisted_setup(persisted);
        m.open_settings();
        assert!(
            m.setup
                .settings_actions
                .iter()
                .any(|a| matches!(a, SettingsAction::CheckAgentUpdates)),
            "maintenance tab must list the check action"
        );
        assert!(
            m.setup
                .settings_actions
                .iter()
                .any(|a| matches!(a, SettingsAction::UpdateAgentClis)),
            "maintenance tab must list the update action"
        );

        m.dispatch_settings_action(SettingsAction::CheckAgentUpdates);
        assert!(matches!(
            cmd_rx.try_recv().expect("check command sent"),
            lazybox_ipc::Command::CheckAgentCliUpdates
        ));

        m.dispatch_settings_action(SettingsAction::UpdateAgentClis);
        assert!(matches!(
            cmd_rx.try_recv().expect("update command sent"),
            lazybox_ipc::Command::UpdateAgentClis
        ));
    }
}

#[cfg(test)]
mod optimistic_mutation_tests {
    //! #476: mutating actions apply locally on the keystroke, reconcile
    //! on the daemon's success echo, and roll back on its failure event.
    //! No user-visible wait on a round-trip; a rejected mutation never
    //! leaves a lie on screen.
    use super::super::*;
    use crate::realm::components::footer::NoticeSeverity;
    use chrono::Utc;
    use lazybox_core::{SessionKey, Task, TaskId, Workspace, WorkspaceKey};
    use lazybox_ipc::{Command as IpcCommand, Event as IpcEvent, channel};
    use lazybox_tui_core::action::Action;
    use tuirealm::ratatui::layout::Size;

    type TestModel = Model<tuirealm::terminal::TestTerminalAdapter>;

    fn build_model() -> TestModel {
        let (client, _server) = channel::pair();
        // A live polling modal swallows footer notices; clear it so
        // rollback errors surface in `status.notice`.
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.status.polling = None;
        m
    }

    fn provider_error(source: &str, message: &str) -> IpcEvent {
        IpcEvent::ProviderError {
            source: source.into(),
            message: message.into(),
            detail: String::new(),
            kind: "retryable".into(),
        }
    }

    fn pr_task(key: &str) -> Task {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        Task {
            author: String::new(),
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
            branch: Some("feature".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: Some("PR_node".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    fn seed_pr_workspace(m: &mut TestModel, key: &str) -> WorkspaceKey {
        let ws = Workspace::from_task(pr_task(key), Utc::now());
        let ws_key = ws.key.clone();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        ws_key
    }

    fn reviewers_of(m: &TestModel, sk: &SessionKey) -> Vec<String> {
        m.sidebar
            .workspace_by_key(sk)
            .and_then(|w| w.pr.as_ref())
            .map(|p| p.reviewers.clone())
            .unwrap_or_default()
    }

    #[test]
    fn archive_removes_row_instantly_then_reconciles() {
        let mut m = build_model();
        let ws_key = seed_pr_workspace(&mut m, "github:owner/repo#1");
        let sk: SessionKey = (&ws_key).into();
        assert!(m.sidebar.workspace_by_key(&sk).is_some());

        let cmds = m.dispatch_action_confirmed(
            &Action::Archive,
            &ActionConfirmTarget::Workspace(sk.clone()),
        );
        assert!(matches!(cmds.as_slice(), [IpcCommand::Kill { .. }]));
        assert!(
            m.sidebar.workspace_by_key(&sk).is_none(),
            "row must vanish on confirm, not wait for WorkspaceRemoved"
        );
        assert_eq!(m.pending_mutations.len(), 1, "rollback stash held");

        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(ws_key));
        assert!(
            m.pending_mutations.is_empty(),
            "the removed echo reconciles the stash"
        );
        assert!(m.sidebar.workspace_by_key(&sk).is_none());
    }

    #[test]
    fn archive_rolls_back_on_store_failure() {
        let mut m = build_model();
        let ws_key = seed_pr_workspace(&mut m, "github:owner/repo#2");
        let sk: SessionKey = (&ws_key).into();
        m.dispatch_action_confirmed(
            &Action::Archive,
            &ActionConfirmTarget::Workspace(sk.clone()),
        );
        assert!(m.sidebar.workspace_by_key(&sk).is_none());

        m.handle_daemon_event(provider_error(
            "store",
            &format!("could not delete workspace {ws_key}: disk full"),
        ));
        assert!(
            m.sidebar.workspace_by_key(&sk).is_some(),
            "a rejected delete must re-insert the row"
        );
        assert!(m.pending_mutations.is_empty());
        let n = m.status.notice.as_ref().expect("rollback flashes an error");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(n.message.contains("delete failed"), "got {:?}", n.message);
    }

    #[test]
    fn archive_rolls_back_on_terminal_kill_failure() {
        let mut m = build_model();
        let ws_key = seed_pr_workspace(&mut m, "github:owner/repo#8");
        let sk: SessionKey = (&ws_key).into();
        m.dispatch_action_confirmed(
            &Action::Archive,
            &ActionConfirmTarget::Workspace(sk.clone()),
        );
        assert!(m.sidebar.workspace_by_key(&sk).is_none());

        // The daemon couldn't stop a backing agent, so it preserved the
        // workspace and emitted a `terminal` error naming the key.
        m.handle_daemon_event(provider_error(
            "terminal",
            &format!(
                "could not stop terminal x; workspace {ws_key} was not deleted: tmux timed out"
            ),
        ));
        assert!(
            m.sidebar.workspace_by_key(&sk).is_some(),
            "an un-killable agent must bring the row back"
        );
        assert!(m.pending_mutations.is_empty());
        let n = m.status.notice.as_ref().expect("rollback flashes");
        assert!(n.message.contains("delete failed"), "got {:?}", n.message);
    }

    #[test]
    fn unrelated_store_error_does_not_roll_back() {
        let mut m = build_model();
        let ws_key = seed_pr_workspace(&mut m, "github:owner/repo#9");
        let sk: SessionKey = (&ws_key).into();
        m.dispatch_action_confirmed(
            &Action::Archive,
            &ActionConfirmTarget::Workspace(sk.clone()),
        );

        // A store error naming a DIFFERENT workspace must not resurrect
        // this row.
        m.handle_daemon_event(provider_error(
            "store",
            "could not delete workspace github:owner/repo#99: nope",
        ));
        assert!(
            m.sidebar.workspace_by_key(&sk).is_none(),
            "rollback keys off the named workspace, not any store error"
        );
        assert_eq!(m.pending_mutations.len(), 1, "stash still armed");
    }

    #[test]
    fn reviewers_update_chip_instantly_then_reconcile() {
        let mut m = build_model();
        let ws_key = seed_pr_workspace(&mut m, "github:owner/repo#3");
        let sk: SessionKey = (&ws_key).into();
        m.modal_flow = Some(super::super::ModalFlow::ReviewRequest {
            workspace: ws_key.clone(),
        });
        m.modal_stack.push(Id::RequestReviewers);

        let cmds = m.handle_choice_picked(vec![
            ChoicePayload::Text("alice".into()),
            ChoicePayload::Text("bob".into()),
        ]);
        assert!(matches!(
            cmds.as_slice(),
            [IpcCommand::RequestReviewers { .. }]
        ));
        assert_eq!(
            reviewers_of(&m, &sk),
            vec!["alice".to_string(), "bob".to_string()]
        );
        assert_eq!(m.pending_mutations.len(), 1);

        // The daemon's fresh copy reconciles the stash.
        let mut updated = Workspace::from_task(pr_task("github:owner/repo#3"), Utc::now());
        updated.pr.as_mut().unwrap().reviewers = vec!["alice".into(), "bob".into()];
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(updated)));
        assert!(m.pending_mutations.is_empty());
        assert_eq!(
            reviewers_of(&m, &sk),
            vec!["alice".to_string(), "bob".to_string()]
        );
    }

    #[test]
    fn reviewers_roll_back_on_failure() {
        let mut m = build_model();
        let ws_key = seed_pr_workspace(&mut m, "github:owner/repo#4");
        let sk: SessionKey = (&ws_key).into();
        m.modal_flow = Some(super::super::ModalFlow::ReviewRequest {
            workspace: ws_key.clone(),
        });
        m.modal_stack.push(Id::RequestReviewers);
        m.handle_choice_picked(vec![ChoicePayload::Text("alice".into())]);
        assert_eq!(reviewers_of(&m, &sk), vec!["alice".to_string()]);

        m.handle_daemon_event(provider_error(
            "reviewers",
            "request reviewers failed: nope",
        ));
        assert!(
            reviewers_of(&m, &sk).is_empty(),
            "a rejected reviewer request must roll the chip back"
        );
        assert!(m.pending_mutations.is_empty());
        let n = m.status.notice.as_ref().expect("failure flashes");
        assert_eq!(n.severity, NoticeSeverity::Permanent);
        assert!(
            n.message.contains("request reviewers failed"),
            "got {:?}",
            n.message
        );
    }

    #[test]
    fn labels_apply_then_roll_back_on_failure() {
        let mut m = build_model();
        let ws_key = seed_pr_workspace(&mut m, "github:owner/repo#5");
        let sk: SessionKey = (&ws_key).into();
        m.awaiting_repo_labels = Some(ws_key.clone());
        m.modal_stack.push(Id::ManageLabels);
        m.handle_choice_picked(vec![
            ChoicePayload::Text("bug".into()),
            ChoicePayload::Text("urgent".into()),
        ]);
        let names: Vec<String> = m
            .sidebar
            .workspace_by_key(&sk)
            .unwrap()
            .pr
            .as_ref()
            .unwrap()
            .labels
            .iter()
            .map(|l| l.name.clone())
            .collect();
        assert_eq!(names, vec!["bug".to_string(), "urgent".to_string()]);

        m.handle_daemon_event(provider_error("labels", "update labels failed: boom"));
        assert!(
            m.sidebar
                .workspace_by_key(&sk)
                .unwrap()
                .pr
                .as_ref()
                .unwrap()
                .labels
                .is_empty(),
            "a rejected label set must roll back"
        );
        assert!(m.pending_mutations.is_empty());
    }

    #[test]
    fn assignees_apply_then_roll_back_on_failure() {
        let mut m = build_model();
        let ws_key = seed_pr_workspace(&mut m, "github:owner/repo#6");
        let sk: SessionKey = (&ws_key).into();
        m.modal_flow = Some(super::super::ModalFlow::AssigneesRequest {
            workspace: ws_key.clone(),
        });
        m.modal_stack.push(Id::AddAssignees);
        m.handle_choice_picked(vec![ChoicePayload::Text("alice".into())]);
        assert_eq!(
            m.sidebar
                .workspace_by_key(&sk)
                .unwrap()
                .pr
                .as_ref()
                .unwrap()
                .assignees,
            vec!["alice".to_string()]
        );

        m.handle_daemon_event(provider_error("assignees", "update assignees failed: no"));
        assert!(
            m.sidebar
                .workspace_by_key(&sk)
                .unwrap()
                .pr
                .as_ref()
                .unwrap()
                .assignees
                .is_empty(),
            "a rejected assignee set must roll back"
        );
        assert!(m.pending_mutations.is_empty());
    }
}
mod remote_spawn_tests {
    //! #965 r-spawn hardening: spawns route to the box's client (never the
    //! local flush), the `⇅` tag is latched against local-daemon snapshots,
    //! a `v` multi-select fans the spawn out with the heavy-spawn confirm
    //! (#932), and a worker "spawn dropped" notice rolls the tag back.
    use super::super::*;
    use chrono::{Duration, Utc};
    use lazybox_core::{SessionKey, Task, TaskId, Workspace};
    use lazybox_ipc::{Command as IpcCommand, Event as IpcEvent, channel};
    use lazybox_tui_core::action::Action;
    use lazybox_tui_core::remote::{RemoteBoxNotice, RemoteConnState, RemoteControl};
    use tuirealm::ratatui::layout::Size;

    fn task(key: &str, age: Duration) -> Task {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        Task {
            author: String::new(),
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
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    fn workspace(key: &str, age: Duration) -> Workspace {
        Workspace::from_task(task(key, age), Utc::now())
    }

    /// A model wired to one remote box `"box"`. Returns the box-side
    /// command receiver (the worker's end of the in-process pair) so
    /// tests assert what actually reached the box, plus the local
    /// connection to keep the local channel alive.
    fn build_model_with_box() -> (
        Model<tuirealm::terminal::TestTerminalAdapter>,
        lazybox_ipc::Connection,
        tokio::sync::mpsc::Receiver<IpcCommand>,
    ) {
        let (client, server) = channel::pair();
        let m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        let (box_tx, box_rx) = tokio::sync::mpsc::channel(16);
        let (_box_evt_tx, box_evt_rx) = tokio::sync::mpsc::channel(16);
        let box_client = lazybox_ipc::Client::from_bounded_channels(box_tx, box_evt_rx);
        let mut clients = std::collections::BTreeMap::new();
        clients.insert("box".to_string(), box_client);
        let m = m.with_remote_clients(clients, Some("box".to_string()));
        (m, server, box_rx)
    }

    fn seed_focused(
        m: &mut Model<tuirealm::terminal::TestTerminalAdapter>,
        key: &str,
        age: Duration,
    ) -> SessionKey {
        let ws = workspace(key, age);
        let sk: SessionKey = (&ws.key).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));
        sk
    }

    /// An `r c` spawn goes to the box's client — never the local `cmds`
    /// flush — and the row's `⇅` tag survives the next local-daemon
    /// upsert (which knows nothing about the box) via the latch.
    #[test]
    fn r_spawn_routes_to_the_box_and_latches_the_glyph() {
        let (mut m, _conn, mut box_rx) = build_model_with_box();
        let sk = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));

        let cmds = m.dispatch_action(&Action::SpawnAgentRemote("claude".into()));
        assert!(
            cmds.is_empty(),
            "a remote spawn never rides the local flush"
        );
        match box_rx.try_recv() {
            Ok(IpcCommand::Spawn { kind, .. }) => {
                assert!(
                    matches!(kind, lazybox_ipc::TerminalKind::Agent(ref a) if a == "claude"),
                    "spawn must carry the claude agent, got {kind:?}"
                );
            }
            other => panic!("the box client must receive the spawn, got {other:?}"),
        }
        assert_eq!(
            m.sidebar.workspace_by_key(&sk).unwrap().remote.as_deref(),
            Some("box"),
            "the row is tagged immediately"
        );

        // The local daemon's next upsert carries `remote: None` — the
        // pre-latch behavior wiped the glyph here within one poll.
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(workspace(
            "owner/repo#1",
            Duration::hours(1),
        ))));
        assert_eq!(
            m.sidebar.workspace_by_key(&sk).unwrap().remote.as_deref(),
            Some("box"),
            "the latch must re-apply the tag at ingest"
        );
    }

    /// Under a `v` multi-select, `r c` fans out like every other
    /// bulk-appropriate spawn (#932): gated behind the heavy-spawn
    /// confirm, one spawn per target to the box, every row tagged.
    #[test]
    fn bulk_r_spawn_gates_and_fans_out_to_the_box() {
        let (mut m, _conn, mut box_rx) = build_model_with_box();
        let sk1 = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));
        m.sidebar.toggle_broadcast_select();
        let sk2 = seed_focused(&mut m, "owner/repo#2", Duration::hours(2));
        m.sidebar.toggle_broadcast_select();

        let cmds = m.dispatch_action(&Action::SpawnAgentRemote("claude".into()));
        assert!(cmds.is_empty(), "bulk remote spawn gates on confirm");
        assert_eq!(m.modal_stack.last(), Some(&Id::BulkSpawnConfirm));

        let cmds = m.handle_confirmed(true);
        assert!(
            cmds.is_empty(),
            "remote spawns bypass the local flush even post-confirm"
        );
        let mut spawned = 0;
        while let Ok(cmd) = box_rx.try_recv() {
            assert!(matches!(cmd, IpcCommand::Spawn { .. }));
            spawned += 1;
        }
        assert_eq!(spawned, 2, "one spawn per selected workspace");
        for sk in [&sk1, &sk2] {
            assert_eq!(
                m.sidebar.workspace_by_key(sk).unwrap().remote.as_deref(),
                Some("box"),
                "every fanned-out row gets the latched tag"
            );
        }
        assert_eq!(m.remote_marks.len(), 2);
    }

    /// `WorkspaceRemoved` releases the remote latch, exactly like
    /// `merge_confirmed` — a re-added key must not inherit a stale `⇅`.
    #[test]
    fn workspace_removed_releases_the_remote_latch() {
        let (mut m, _conn, _box_rx) = build_model_with_box();
        let ws = workspace("owner/repo#1", Duration::hours(1));
        let wkey = ws.key.clone();
        let sk: SessionKey = (&wkey).into();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(ws)));
        assert!(m.sidebar.focus_workspace_key(&sk));
        m.mark_remote_latched(sk.clone(), "box".to_string());
        assert!(m.remote_marks.contains_key(&sk));

        m.handle_daemon_event(IpcEvent::WorkspaceRemoved(wkey));
        assert!(
            m.remote_marks.is_empty(),
            "a removed workspace must not leave a latch entry behind"
        );
    }

    /// The notice channel closing means the worker died without a chance
    /// to report anything in flight: surface it once, stop polling — and
    /// do NOT clear existing tags (sessions already spawned keep running
    /// on the box; only the link died).
    #[test]
    fn worker_death_is_surfaced_once_and_polling_stops() {
        let (m, _conn, _box_rx) = build_model_with_box();
        let (notice_tx, notice_rx) = tokio::sync::mpsc::channel::<RemoteBoxNotice>(8);
        let mut m = m.with_remote_notices(notice_rx);
        let sk = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));
        m.mark_remote_latched(sk.clone(), "box".to_string());

        drop(notice_tx);
        m.tick_remote_notices();

        assert!(
            m.remote_notice_rx.is_none(),
            "a dead worker's channel must not be polled forever"
        );
        assert_eq!(
            m.sidebar.workspace_by_key(&sk).unwrap().remote.as_deref(),
            Some("box"),
            "tags for possibly-live remote sessions must survive the link dying"
        );
        // A second tick after the receiver was dropped is a no-op, not a
        // second flash or a panic.
        m.tick_remote_notices();
    }

    /// A worker `Dropped` notice rolls the optimistic tags back — the `⇅`
    /// glyph must not advertise sessions whose spawns never happened. One
    /// aggregate notice covers a whole failed bulk fan-out.
    #[test]
    fn dropped_notice_rolls_back_every_named_tag() {
        let (m, _conn, _box_rx) = build_model_with_box();
        let (notice_tx, notice_rx) = tokio::sync::mpsc::channel(8);
        let mut m = m.with_remote_notices(notice_rx);
        let sk1 = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));
        let sk2 = seed_focused(&mut m, "owner/repo#2", Duration::hours(2));

        m.mark_remote_latched(sk1.clone(), "box".to_string());
        m.mark_remote_latched(sk2.clone(), "box".to_string());
        assert_eq!(
            m.sidebar.workspace_by_key(&sk1).unwrap().remote.as_deref(),
            Some("box")
        );

        notice_tx
            .try_send(RemoteBoxNotice::Dropped {
                session_keys: vec![sk1.clone(), sk2.clone()],
                error: "⇅ box: bring-up failed — 2 command(s) dropped".to_string(),
            })
            .expect("test channel");
        m.tick_remote_notices();

        for sk in [&sk1, &sk2] {
            assert_eq!(
                m.sidebar.workspace_by_key(sk).unwrap().remote,
                None,
                "every dropped spawn must clear its glyph"
            );
        }
        assert!(m.remote_marks.is_empty(), "the latch is released too");
    }

    /// A configured box shows a persistent `disconnected` indicator from
    /// launch — not the hidden `NotConfigured` state (#1066).
    #[test]
    fn a_configured_box_starts_disconnected_and_visible() {
        let (m, _conn, _box_rx) = build_model_with_box();
        assert_eq!(m.status.remote_conn, RemoteConnState::Disconnected);
    }

    /// Auto-connect (the default) fires a background `Connect` on startup
    /// and paints the indicator `connecting…` immediately (#1066).
    #[test]
    fn auto_connect_requests_connect_on_startup() {
        let (m, _conn, _box_rx) = build_model_with_box();
        let (ctrl_tx, mut ctrl_rx) = tokio::sync::mpsc::channel(8);
        let m = m.with_remote_control(ctrl_tx, true);
        assert_eq!(
            ctrl_rx.try_recv().expect("a Connect was queued"),
            RemoteControl::Connect
        );
        assert_eq!(m.status.remote_conn, RemoteConnState::Connecting);
    }

    /// With auto-connect off (the hard-gate opt-out), startup does NOT
    /// connect — the box waits for an explicit action (#1066).
    #[test]
    fn auto_connect_off_does_not_connect_on_startup() {
        let (m, _conn, _box_rx) = build_model_with_box();
        let (ctrl_tx, mut ctrl_rx) = tokio::sync::mpsc::channel(8);
        let m = m.with_remote_control(ctrl_tx, false);
        assert!(
            ctrl_rx.try_recv().is_err(),
            "no Connect should be sent when auto_connect is off"
        );
        assert_eq!(m.status.remote_conn, RemoteConnState::Disconnected);
    }

    /// The `ConnectBox` action toggles: a `Connect` when disconnected,
    /// then a `Disconnect` when connected (#1066).
    #[test]
    fn connect_box_action_toggles_connect_and_disconnect() {
        let (m, _conn, _box_rx) = build_model_with_box();
        let (ctrl_tx, mut ctrl_rx) = tokio::sync::mpsc::channel(8);
        // auto_connect off so startup doesn't pre-send a Connect.
        let mut m = m.with_remote_control(ctrl_tx, false);

        m.dispatch_action(&Action::ConnectBox);
        assert_eq!(ctrl_rx.try_recv().unwrap(), RemoteControl::Connect);
        assert_eq!(m.status.remote_conn, RemoteConnState::Connecting);

        // Simulate the worker reporting the link came up.
        m.status
            .note_remote_state(RemoteConnState::Connected { name: "box".into() });
        m.dispatch_action(&Action::ConnectBox);
        assert_eq!(ctrl_rx.try_recv().unwrap(), RemoteControl::Disconnect);
        assert_eq!(m.status.remote_conn, RemoteConnState::Disconnected);
    }

    /// `ConnectBox` with no `sandbox:` config routes the user into the
    /// onboarding flow rather than a bare error (#1112) — the connect
    /// button is now the discovery path for setting a box up.
    #[test]
    fn connect_box_without_a_box_opens_onboarding() {
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("lazybox-connect-onboard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator here.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.dispatch_action(&Action::ConnectBox);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxProviderPick),
            "a missing box routes into onboarding",
        );

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The full GCP onboarding walk (#1112): provider → service-account key
    /// → project → zone → user → auto-connect, ending with a persisted
    /// `sandbox:` block carrying the API credential and a region derived
    /// from the chosen zone.
    #[test]
    fn sandbox_onboarding_gcp_walk_persists_config() {
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("lazybox-sbx-gcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator here.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        // A readable key file so the credential-state check passes.
        let key = home.join("sa.json");
        std::fs::write(&key, "{}").unwrap();

        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.start_sandbox_onboarding();
        assert_eq!(m.modal_stack.last(), Some(&Id::SandboxProviderPick));

        let _ = m.handle_choice_picked(vec![ChoicePayload::Text("gcp".into())]);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "gcp advances to the service-account-key step",
        );

        let _ = m.handle_input_submitted(key.display().to_string());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "a readable key advances to the project prompt",
        );

        let _ = m.handle_input_submitted("my-proj".into());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "project → zone"
        );
        // A non-default zone, then a blank user (keeps the default).
        let _ = m.handle_input_submitted("europe-west1-b".into());
        assert_eq!(m.modal_stack.last(), Some(&Id::SandboxInput), "zone → user");
        let _ = m.handle_input_submitted(String::new());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxConfirm),
            "user → auto-connect toggle",
        );

        let _ = m.handle_confirmed(false);
        assert!(m.top_modal().is_none(), "the toggle answer ends the flow");

        let cfg = lazybox_config::Config::load_from(&home.join("config.yaml")).expect("config");
        assert_eq!(cfg.sandbox.provider.as_deref(), Some("gcp"));
        assert_eq!(
            cfg.sandbox.auth.service_account_key.as_deref(),
            Some(key.as_path()),
            "the API credential is persisted",
        );
        assert_eq!(cfg.sandbox.project.as_deref(), Some("my-proj"));
        assert_eq!(cfg.sandbox.zone.as_deref(), Some("europe-west1-b"));
        assert_eq!(
            cfg.sandbox.region.as_deref(),
            Some("europe-west1"),
            "region derived from the zone survives the persist path",
        );
        assert_eq!(
            cfg.sandbox.user.as_deref(),
            Some(crate::sandbox_flow::DEFAULT_USER)
        );
        assert_eq!(cfg.sandbox.auto_connect, Some(false));

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The service-account-key step is the non-interactive credential check
    /// (#1112): an unreadable path re-asks with an actionable error instead
    /// of persisting a box that can't authenticate; a blank path is accepted
    /// as ambient credentials.
    #[test]
    fn sandbox_onboarding_gcp_key_validates_readability() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.start_sandbox_onboarding();
        let _ = m.handle_choice_picked(vec![ChoicePayload::Text("gcp".into())]);
        assert_eq!(m.modal_stack.last(), Some(&Id::SandboxInput), "key step");

        // A path that doesn't exist → re-ask, with a status notice.
        let _ = m.handle_input_submitted("/no/such/key.json".into());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "an unreadable key re-asks",
        );
        assert!(m.status.notice.is_some(), "surfaces the credential error");

        // Blank → ambient credentials, advances to the project prompt.
        let _ = m.handle_input_submitted(String::new());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "a blank key (ambient) advances to the project prompt",
        );
    }

    /// The E2B walk skips the GCP-only steps: provider → API-key gate →
    /// template → auto-connect, persisting the template and no
    /// `project`/`zone`.
    #[test]
    fn sandbox_onboarding_e2b_walk_persists_template() {
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("lazybox-sbx-e2b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator here.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.start_sandbox_onboarding();
        let _ = m.handle_choice_picked(vec![ChoicePayload::Text("e2b".into())]);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxConfirm),
            "e2b advances to the API-key gate",
        );
        let _ = m.handle_confirmed(true);
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "API-key gate advances to the template prompt",
        );
        let _ = m.handle_input_submitted("lazybox-e2b".into());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxConfirm),
            "template → auto-connect toggle",
        );
        let _ = m.handle_confirmed(true);
        assert!(m.top_modal().is_none());

        let cfg = lazybox_config::Config::load_from(&home.join("config.yaml")).expect("config");
        assert_eq!(cfg.sandbox.provider.as_deref(), Some("e2b"));
        assert_eq!(cfg.sandbox.template.as_deref(), Some("lazybox-e2b"));
        assert_eq!(cfg.sandbox.auto_connect, Some(true));
        assert!(
            cfg.sandbox.project.is_none(),
            "no GCP fields for an e2b box"
        );
        assert!(cfg.sandbox.zone.is_none());

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Re-running onboarding on a hand-tuned box preserves the fields the
    /// flow never asks about — `ports`, `require_connect`, and the `auth`
    /// sub-fields it doesn't own (impersonation) — rather than clobbering the
    /// whole `sandbox:` block (regression guard).
    #[test]
    fn sandbox_onboarding_preserves_hand_authored_fields() {
        let _env = super::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("lazybox-sbx-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: ENV_LOCK serializes every LAZYBOX_HOME mutator here.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let key = home.join("sa.json");
        std::fs::write(&key, "{}").unwrap();

        // Seed a config whose sandbox block carries fields the flow leaves
        // untouched.
        let mut seed = lazybox_config::Config::default();
        seed.sandbox.provider = Some("gcp".into());
        seed.sandbox.require_connect = Some(true);
        seed.sandbox.ports = vec![3000];
        seed.sandbox.auth.impersonate_service_account = Some("deploy@proj.iam".into());
        seed.save_to(&home.join("config.yaml")).unwrap();

        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.start_sandbox_onboarding();
        let _ = m.handle_choice_picked(vec![ChoicePayload::Text("gcp".into())]);
        let _ = m.handle_input_submitted(key.display().to_string());
        let _ = m.handle_input_submitted("new-proj".into());
        let _ = m.handle_input_submitted(String::new());
        let _ = m.handle_input_submitted(String::new());
        let _ = m.handle_confirmed(false);

        let cfg = lazybox_config::Config::load_from(&home.join("config.yaml")).expect("config");
        assert_eq!(
            cfg.sandbox.project.as_deref(),
            Some("new-proj"),
            "walked field updated"
        );
        assert_eq!(
            cfg.sandbox.auth.service_account_key.as_deref(),
            Some(key.as_path()),
            "flow-owned key written"
        );
        assert_eq!(
            cfg.sandbox.require_connect,
            Some(true),
            "untouched toggle kept"
        );
        assert_eq!(cfg.sandbox.ports, vec![3000], "untouched ports kept");
        assert_eq!(
            cfg.sandbox.auth.impersonate_service_account.as_deref(),
            Some("deploy@proj.iam"),
            "untouched auth sub-field kept",
        );

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A blank GCP project id re-asks instead of persisting a box that
    /// would fail at connect time.
    #[test]
    fn sandbox_onboarding_reprompts_on_empty_project() {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.start_sandbox_onboarding();
        let _ = m.handle_choice_picked(vec![ChoicePayload::Text("gcp".into())]);
        // Blank key (ambient) advances past the credential step.
        let _ = m.handle_input_submitted(String::new());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "project prompt"
        );
        let _ = m.handle_input_submitted("   ".into());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "a blank project re-asks",
        );
        assert!(
            m.status.notice.is_some(),
            "explains the project is required"
        );
        let _ = m.handle_input_submitted("real-proj".into());
        assert_eq!(
            m.modal_stack.last(),
            Some(&Id::SandboxInput),
            "a real project advances to the zone prompt",
        );
    }

    /// A worker `State` notice drives the persistent indicator only — no
    /// transient flash steals the footer (#1066).
    #[test]
    fn state_notice_updates_the_persistent_indicator() {
        let (m, _conn, _box_rx) = build_model_with_box();
        let (notice_tx, notice_rx) = tokio::sync::mpsc::channel(8);
        let mut m = m.with_remote_notices(notice_rx);

        notice_tx
            .try_send(RemoteBoxNotice::State(RemoteConnState::Connected {
                name: "obin".into(),
            }))
            .expect("test channel");
        m.tick_remote_notices();

        assert_eq!(
            m.status.remote_conn,
            RemoteConnState::Connected {
                name: "obin".into()
            }
        );
        assert!(
            m.status.notice.is_none(),
            "a state transition is durable, not a transient flash"
        );
    }

    /// With the hard-gate on (`require_connect: true`), a remote spawn while
    /// disconnected is refused instead of lazily triggering a bring-up:
    /// nothing reaches the box and a footer nudge points at the connect key
    /// (#1066).
    #[test]
    fn hard_gate_refuses_remote_spawn_while_disconnected() {
        let (m, _conn, mut box_rx) = build_model_with_box();
        let (ctrl_tx, _ctrl_rx) = tokio::sync::mpsc::channel(8);
        let mut m = m
            .with_remote_control(ctrl_tx, false)
            .with_remote_require_connect(true);
        let _sk = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));

        m.dispatch_action(&Action::SpawnAgentRemote("claude".into()));
        assert!(
            box_rx.try_recv().is_err(),
            "the disconnected hard-gate must not forward the spawn"
        );
        assert!(m.status.notice.is_some(), "the refusal is surfaced");
    }

    /// The hard-gate is OFF by default (`require_connect` unset, #1066):
    /// `r c` while disconnected still lazily brings the box up on demand
    /// (the spawn reaches the box worker), so remote spawn stays one-key.
    /// Decoupled from `auto_connect` — startup connect off must not gate
    /// on-demand spawns.
    #[test]
    fn r_spawn_lazily_proceeds_when_require_connect_off() {
        let (m, _conn, mut box_rx) = build_model_with_box();
        let (ctrl_tx, _ctrl_rx) = tokio::sync::mpsc::channel(8);
        // auto_connect off (no startup connect), require_connect left off.
        let mut m = m.with_remote_control(ctrl_tx, false);
        assert!(!m.remote_connected(), "box starts disconnected");
        let _sk = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));

        m.dispatch_action(&Action::SpawnAgentRemote("claude".into()));
        assert!(
            matches!(box_rx.try_recv(), Ok(IpcCommand::Spawn { .. })),
            "with require_connect off, a disconnected r-spawn lazily reaches the box"
        );
    }

    /// Ordering fix (#1066): a disabled repo is refused BEFORE the hard-gate,
    /// so `r c` there never tells the user to "connect first" (which would
    /// wake the billed box only to then refuse as disabled).
    #[test]
    fn disabled_repo_refuses_before_the_hard_gate() {
        let (m, _conn, mut box_rx) = build_model_with_box();
        let (ctrl_tx, _ctrl_rx) = tokio::sync::mpsc::channel(8);
        let mut disabled = std::collections::BTreeSet::new();
        disabled.insert("owner/repo".to_string());
        // Hard-gate ON and disconnected: the wrong order would surface the
        // connect nudge; the fix surfaces the disabled nudge instead.
        let mut m = m
            .with_remote_control(ctrl_tx, false)
            .with_remote_require_connect(true)
            .with_remote_repo_overrides(disabled);
        let _sk = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));

        m.dispatch_action(&Action::SpawnAgentRemote("claude".into()));
        assert!(box_rx.try_recv().is_err(), "nothing reaches the box");
        let msg = m.status.notice.as_ref().expect("a nudge is surfaced");
        assert!(
            msg.message.contains("disabled for this project"),
            "must name the opt-out, not the connect gate: {:?}",
            msg.message
        );
    }

    /// Per-project opt-out (#1066): a repo that set `sandbox: false` resolves
    /// to no box, while every other repo (and the repo-less path) inherits
    /// the global one. Matching is case-insensitive — a casing mismatch must
    /// not silently leave a disabled repo enabled (fail *open*).
    #[test]
    fn remote_for_repo_honors_the_per_project_opt_out() {
        let (m, _conn, _box_rx) = build_model_with_box();
        // Boot lowercases the disabled set (Config::sandbox_disabled_repos).
        let mut disabled = std::collections::BTreeSet::new();
        disabled.insert("owner/repo".to_string());
        let m = m.with_remote_repo_overrides(disabled);
        assert_eq!(m.remote_for_repo(Some("owner/other")), Some("box"));
        assert_eq!(m.remote_for_repo(Some("owner/repo")), None, "opted out");
        assert_eq!(
            m.remote_for_repo(Some("Owner/Repo")),
            None,
            "a casing mismatch must still resolve to the opt-out"
        );
        assert_eq!(
            m.remote_for_repo(None),
            Some("box"),
            "a repo-less workspace can't be opted out"
        );
    }

    /// An `r`-spawn on a workspace whose repo disabled the box is refused
    /// (nothing reaches the box) and surfaces a nudge — even though the box
    /// is configured for other projects (#1066).
    #[test]
    fn r_spawn_refused_on_a_disabled_repo() {
        let (m, _conn, mut box_rx) = build_model_with_box();
        let mut disabled = std::collections::BTreeSet::new();
        disabled.insert("owner/repo".to_string());
        let mut m = m.with_remote_repo_overrides(disabled);
        let _sk = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));

        m.dispatch_action(&Action::SpawnAgentRemote("claude".into()));
        assert!(
            box_rx.try_recv().is_err(),
            "a disabled repo's spawn never reaches the box"
        );
        assert!(m.status.notice.is_some(), "the refusal is surfaced");
    }

    /// With the hard-gate on but the box connected, a remote spawn goes
    /// straight through — connection is the gate, not a per-spawn effect.
    #[test]
    fn hard_gate_allows_remote_spawn_once_connected() {
        let (m, _conn, mut box_rx) = build_model_with_box();
        let (ctrl_tx, _ctrl_rx) = tokio::sync::mpsc::channel(8);
        let mut m = m
            .with_remote_control(ctrl_tx, false)
            .with_remote_require_connect(true);
        m.status
            .note_remote_state(RemoteConnState::Connected { name: "box".into() });
        let _sk = seed_focused(&mut m, "owner/repo#1", Duration::hours(1));

        m.dispatch_action(&Action::SpawnAgentRemote("claude".into()));
        assert!(
            matches!(box_rx.try_recv(), Ok(IpcCommand::Spawn { .. })),
            "a connected box accepts the spawn"
        );
    }
}

/// The focus indicator + burst guard from issue #1110: keystrokes must
/// have an unmistakable destination, and a typed word in the sidebar
/// must not fire a chain of shortcuts.
#[cfg(test)]
mod focus_indicator_and_burst_guard_tests {
    use super::super::*;
    use chrono::Utc;
    use lazybox_core::{SessionKey, Workspace, WorkspaceKey};
    use lazybox_ipc::{Event as IpcEvent, TerminalId, TerminalKind, channel};
    use tuirealm::event::{Key, KeyEvent, KeyModifiers};
    use tuirealm::ratatui::layout::Size;

    type TestModel = Model<tuirealm::terminal::TestTerminalAdapter>;

    fn build_model() -> TestModel {
        let (client, _server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(120, 40)).expect("model init");
        m.layout.last_area = Rect::new(0, 0, 120, 40);
        m
    }

    fn empty_ws(key: &str) -> Workspace {
        Workspace::empty(WorkspaceKey::new(key), "main", Utc::now())
    }

    fn press(m: &mut TestModel, code: Key) {
        m.dispatch_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn footer_text(m: &mut TestModel) -> String {
        m.view();
        let buffer = m.terminal.raw().backend().buffer();
        let last = buffer.area.height - 1;
        (0..buffer.area.width)
            .map(|col| buffer[(col, last)].symbol())
            .collect::<String>()
    }

    /// With focus in a live agent terminal the footer names the agent
    /// keystrokes flow to — the unmistakable "typing to" signal.
    #[test]
    fn footer_names_the_agent_when_terminal_focused() {
        let mut m = build_model();
        let session = SessionKey::from("github:o/r#1");
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(empty_ws(
            "github:o/r#1",
        ))));
        m.terminals.set_active_session(Some(session.clone()));
        m.terminals.on_daemon_event(&IpcEvent::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: session,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        m.set_focus(PaneFocus::Terminals);

        assert!(
            footer_text(&mut m).contains("typing to: claude"),
            "terminal focus must say where typing goes: {:?}",
            footer_text(&mut m)
        );
    }

    /// Navigation panes read as a distinct "navigating" chip — never
    /// "typing to", so the two modes can't be confused.
    #[test]
    fn footer_reads_navigating_when_sidebar_focused() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(empty_ws(
            "github:o/r#1",
        ))));
        m.set_focus(PaneFocus::Sidebar);

        let footer = footer_text(&mut m);
        assert!(footer.contains("navigating"), "sidebar chip: {footer:?}");
        assert!(
            !footer.contains("typing to"),
            "navigation must not claim to type to an agent: {footer:?}"
        );
    }

    /// A single deliberate shortcut in the sidebar fires normally — the
    /// burst guard must never swallow an intentional press.
    #[test]
    fn a_single_sidebar_shortcut_is_not_suppressed() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(empty_ws(
            "github:o/r#1",
        ))));
        m.set_focus(PaneFocus::Sidebar);

        press(&mut m, Key::Char(' '));
        assert!(
            m.status.notice.is_none(),
            "a lone press draws no burst warning"
        );
    }

    /// A rapid run of *varied* bare single-key shortcuts (a word typed
    /// at the wrong pane) is suppressed after the first couple of keys
    /// and the user is nudged to focus the terminal.
    #[test]
    fn a_sidebar_burst_is_suppressed_and_warns() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(empty_ws(
            "github:o/r#1",
        ))));
        m.set_focus(PaneFocus::Sidebar);

        // Alternate two benign guarded shortcuts (cycle-sort / collapse)
        // back-to-back — distinct keys at typing cadence, well inside
        // the burst window at test speed.
        for _ in 0..3 {
            press(&mut m, Key::Char('o'));
            press(&mut m, Key::Char(' '));
        }
        let notice = m
            .status
            .notice
            .as_ref()
            .expect("a suppressed burst raises a nudge");
        assert!(
            notice.message.contains("sidebar") && notice.message.contains("agent"),
            "nudge must explain the mode mixup: {:?}",
            notice.message
        );
    }

    /// Spamming a *single* key (cycling sort, scrolling) is deliberate,
    /// not a typed word — it must never be mistaken for a burst.
    #[test]
    fn repeating_one_sidebar_key_is_never_suppressed() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(empty_ws(
            "github:o/r#1",
        ))));
        m.set_focus(PaneFocus::Sidebar);

        for _ in 0..6 {
            press(&mut m, Key::Char('o'));
        }
        assert!(
            m.status.notice.is_none(),
            "repeating one key is intentional, not a burst"
        );
    }

    /// The focus-into-terminal move is exempt from the guard: it's the
    /// escape hatch, so it fires even mid-burst.
    #[test]
    fn focus_into_terminal_survives_a_burst() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(empty_ws(
            "github:o/r#1",
        ))));
        m.set_focus(PaneFocus::Sidebar);
        m.sidebar
            .focus_workspace_key(&SessionKey::from("github:o/r#1"));

        for _ in 0..3 {
            press(&mut m, Key::Char('o'));
            press(&mut m, Key::Char(' '));
        }
        // → must still move focus off the sidebar despite the live burst.
        press(&mut m, Key::Right);
        assert_ne!(
            m.focus(),
            PaneFocus::Sidebar,
            "→ escapes the sidebar even during a burst"
        );
    }

    /// A deliberate focus change ends the burst run, so bouncing to a
    /// terminal and back doesn't leave a stale guard that swallows the
    /// next genuine sidebar shortcut (#1110).
    #[test]
    fn focus_change_resets_the_burst_so_the_next_shortcut_fires() {
        let mut m = build_model();
        m.handle_daemon_event(IpcEvent::WorkspaceUpserted(Box::new(empty_ws(
            "github:o/r#1",
        ))));
        m.set_focus(PaneFocus::Sidebar);

        // Drive the guard into its suppressing state (3 distinct rapid
        // keys), leaving the last key as `o`.
        press(&mut m, Key::Char('o'));
        press(&mut m, Key::Char(' '));
        press(&mut m, Key::Char('o'));
        assert!(m.status.notice.is_some(), "burst is suppressing");
        m.status.notice = None;

        // Bounce focus to the terminal pane and back — each transition
        // must reset the run.
        m.set_focus(PaneFocus::Terminals);
        m.set_focus(PaneFocus::Sidebar);

        // A single deliberate press of a key *distinct* from the last
        // burst key must now fire (run restarts at 1), not be swallowed
        // as the 4th key of the old run.
        press(&mut m, Key::Char(' '));
        assert!(
            m.status.notice.is_none(),
            "focus bounce reset the run — the next press is not a burst"
        );
    }
}
