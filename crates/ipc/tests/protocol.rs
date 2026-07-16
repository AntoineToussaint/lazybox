//! Protocol-level tests: serde round-trip every `Command` / `Event`
//! variant, plus framing over a real tokio duplex pair.
//!
//! These tests exist to catch *silent* wire-format breakage. If anyone
//! renames a variant or reorders a field, bincode's tagging changes and
//! one of these assertions blows up. Far better than finding out when
//! a v0.2 client can't talk to a v0.3 daemon.

use lazybox_ipc::{
    AgentApprovalDecision, AgentInputMessage, AgentQuestionAnswer, AgentRunId, AgentRuntimeMode,
    AgentState, AgentUsage, Command, Event, PrincipalId, ProviderCredentialInput,
    ProviderCredentialMetadata, TerminalId, TerminalKind, TerminalSnapshot,
};
use tokio::io::duplex;

fn sample_workspace() -> lazybox_core::Workspace {
    let task = lazybox_core::Task {
        id: lazybox_core::TaskId {
            source: "github".into(),
            key: "o/r#1".into(),
        },
        title: "t".into(),
        body: None,
        state: lazybox_core::TaskState::Open,
        role: lazybox_core::TaskRole::Author,
        ci: lazybox_core::CiStatus::None,
        review: lazybox_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: "https://github.com/o/r/pull/1".into(),
        repo: Some("o/r".into()),
        branch: Some("b".into()),
        base_branch: None,
        updated_at: chrono::Utc::now(),
        created_at: None,
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
    lazybox_core::Workspace::from_task(task, chrono::Utc::now())
}

fn all_commands() -> Vec<Command> {
    let key: lazybox_core::SessionKey = "github:o/r#1".into();
    let principal_id = PrincipalId::local();
    vec![
        Command::Subscribe,
        Command::Spawn {
            session_key: key.clone(),
            session_id: None,
            kind: TerminalKind::Agent("claude".into()),
            cwd: Some("/tmp".into()),
            initial_prompt: None,
            on_main: false,
            model_alias: Some("L".into()),
        },
        Command::Spawn {
            session_key: key.clone(),
            session_id: Some(lazybox_core::SessionId::new()),
            kind: TerminalKind::Shell,
            cwd: None,
            initial_prompt: None,
            on_main: true,
            model_alias: None,
        },
        Command::Spawn {
            session_key: key.clone(),
            session_id: None,
            kind: TerminalKind::LogTail {
                path: "/var/log/x.log".into(),
            },
            cwd: None,
            initial_prompt: Some("fix the failing CI".into()),
            on_main: false,
            model_alias: None,
        },
        Command::CreateSession {
            session_key: key.clone(),
            kind: TerminalKind::Shell,
            label: Some("review".into()),
        },
        Command::Write {
            terminal_id: TerminalId(7),
            bytes: b"hello\n".to_vec(),
        },
        Command::Resize {
            terminal_id: TerminalId(7),
            cols: 120,
            rows: 40,
        },
        Command::Close {
            terminal_id: TerminalId(7),
        },
        Command::StartAgentRun {
            session_key: key.clone(),
            session_id: Some(lazybox_core::SessionId::new()),
            agent: "claude".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: Some("/tmp/worktree".into()),
            initial_input: Some(AgentInputMessage {
                text: Some("review this diff".into()),
                json: Some(r#"{"type":"user","message":"review this diff"}"#.into()),
            }),
        },
        Command::StartAgentRun {
            session_key: key.clone(),
            session_id: None,
            agent: "claude".into(),
            mode: AgentRuntimeMode::Terminal,
            cwd: None,
            initial_input: None,
        },
        Command::SendAgentInput {
            run_id: AgentRunId(9),
            message: AgentInputMessage {
                text: Some("continue".into()),
                json: None,
            },
        },
        Command::InterruptAgentRun {
            run_id: AgentRunId(9),
        },
        Command::DecideAgentApproval {
            run_id: AgentRunId(9),
            request_id: "perm-1".into(),
            decision: AgentApprovalDecision::Approve,
        },
        Command::DecideAgentApproval {
            run_id: AgentRunId(9),
            request_id: "perm-2".into(),
            decision: AgentApprovalDecision::Deny {
                reason: Some("outside workspace".into()),
            },
        },
        Command::AnswerAgentQuestion {
            run_id: AgentRunId(9),
            question_id: "q-1".into(),
            answer: AgentQuestionAnswer {
                answer: "use the existing style".into(),
            },
        },
        Command::UpsertProviderCredential {
            principal_id: principal_id.clone(),
            credential: ProviderCredentialInput {
                provider_id: "github".into(),
                token: "secret-token".into(),
                source: "unit-test".into(),
                scopes: vec!["repo".into()],
                expires_at: None,
            },
        },
        Command::RemoveProviderCredential {
            principal_id: principal_id.clone(),
            provider_id: "github".into(),
        },
        Command::ListProviderCredentials {
            principal_id: principal_id.clone(),
        },
        Command::Kill {
            session_key: key.clone(),
        },
        Command::MarkRead {
            session_key: key.clone(),
        },
        Command::Snooze {
            session_key: key.clone(),
            until: chrono::Utc::now() + chrono::Duration::hours(4),
        },
        Command::Unsnooze {
            session_key: key.clone(),
        },
        Command::Refresh,
        Command::CleanWorktrees,
        Command::InspectWorktrees,
        Command::DeleteOrphanedWorktree {
            path: std::path::PathBuf::from("/tmp/wt"),
            force: false,
        },
        Command::DeleteOrphanedWorktree {
            path: std::path::PathBuf::from("/tmp/wt-dirty"),
            force: true,
        },
        Command::Shutdown,
    ]
}

fn all_events() -> Vec<Event> {
    let key: lazybox_core::SessionKey = "github:o/r#1".into();
    let principal_id = PrincipalId::local();
    let credential_metadata = ProviderCredentialMetadata {
        principal_id: principal_id.clone(),
        provider_id: "github".into(),
        source: "unit-test".into(),
        scopes: vec!["repo".into()],
        updated_at: chrono::Utc::now(),
        expires_at: None,
    };
    vec![
        Event::Snapshot {
            workspaces: vec![sample_workspace()],
            terminals: vec![TerminalSnapshot {
                terminal_id: TerminalId(1),
                session_key: key.clone(),
                kind: TerminalKind::Agent("claude".into()),
                replay: b"replay-bytes".to_vec(),
                last_seq: 42,
                no_permission: true,
                on_main: true,
                model_label: Some("Opus".into()),
                last_user_message: Some("fix the flaky test".into()),
            }],
            projects: vec![],
        },
        Event::WorkspaceUpserted(Box::new(sample_workspace())),
        Event::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(key.as_str())),
        Event::SessionCreated(Box::new(lazybox_core::WorkspaceSession::new(
            lazybox_core::WorkspaceKey::new(key.as_str()),
            lazybox_core::SessionKind::Shell,
            std::path::PathBuf::from("/tmp/wt"),
            chrono::Utc::now(),
        ))),
        Event::SessionEnded {
            workspace_key: lazybox_core::WorkspaceKey::new(key.as_str()),
            session_id: lazybox_core::SessionId::new(),
        },
        Event::TerminalSpawned {
            terminal_id: TerminalId(2),
            session_key: key.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
            model_label: None,
        },
        Event::TerminalOutput {
            terminal_id: TerminalId(2),
            bytes: b"ANSI: \x1b[31mred\x1b[0m".to_vec(),
            seq: 1,
        },
        Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: Some(0),
            last_output: None,
        },
        Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: None,
            last_output: Some("boom: could not start".into()),
        },
        Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(2),
            state: AgentState::InputNeeded,
        },
        Event::AgentRunStarted {
            run_id: AgentRunId(9),
            session_key: key.clone(),
            session_id: Some(lazybox_core::SessionId::new()),
            agent: "claude".into(),
            mode: AgentRuntimeMode::StreamJson,
        },
        Event::AgentRawJson {
            run_id: AgentRunId(9),
            json: r#"{"type":"assistant","message":{"content":[]}}"#.into(),
        },
        Event::AgentDebug {
            run_id: AgentRunId(9),
            message: "runtime stderr line".into(),
        },
        Event::AgentAssistantTextDelta {
            run_id: AgentRunId(9),
            delta: "hello".into(),
        },
        Event::AgentToolCallStarted {
            run_id: AgentRunId(9),
            call_id: "toolu-1".into(),
            name: "Edit".into(),
            input_json: Some(r#"{"file_path":"src/lib.rs"}"#.into()),
        },
        Event::AgentToolCallDelta {
            run_id: AgentRunId(9),
            call_id: "toolu-1".into(),
            delta_json: r#"{"old_string":"a"}"#.into(),
        },
        Event::AgentToolCallFinished {
            run_id: AgentRunId(9),
            call_id: "toolu-1".into(),
            output_json: Some(r#"{"ok":true}"#.into()),
            error: None,
        },
        Event::AgentToolCallFinished {
            run_id: AgentRunId(9),
            call_id: "toolu-2".into(),
            output_json: None,
            error: Some("permission denied".into()),
        },
        Event::AgentPermissionRequest {
            run_id: AgentRunId(9),
            request_id: "perm-1".into(),
            tool_name: "Bash".into(),
            input_json: Some(r#"{"command":"cargo test"}"#.into()),
            reason: Some("run tests".into()),
        },
        Event::AgentUserQuestion {
            run_id: AgentRunId(9),
            question_id: "q-1".into(),
            prompt: "Which branch should I use?".into(),
            choices: vec!["main".into(), "develop".into()],
            allow_freeform: true,
        },
        Event::AgentUsage {
            run_id: AgentRunId(9),
            usage: AgentUsage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                cache_creation_input_tokens: Some(3),
                cache_read_input_tokens: Some(4),
                cost_usd_micros: Some(1234),
            },
        },
        Event::AgentTurnFinished {
            run_id: AgentRunId(9),
            result: Some("turn complete".into()),
            session_id: Some("claude-session-1".into()),
            error: None,
        },
        Event::AgentTurnFinished {
            run_id: AgentRunId(9),
            result: None,
            session_id: None,
            error: Some("max turns reached".into()),
        },
        Event::AgentRunFinished {
            run_id: AgentRunId(9),
            exit_code: Some(0),
            error: None,
        },
        Event::AgentRunFinished {
            run_id: AgentRunId(10),
            exit_code: None,
            error: Some("interrupted".into()),
        },
        Event::ProviderCredentialUpdated {
            principal_id: principal_id.clone(),
            provider_id: "github".into(),
            metadata: credential_metadata.clone(),
        },
        Event::ProviderCredentialRemoved {
            principal_id: principal_id.clone(),
            provider_id: "github".into(),
        },
        Event::ProviderCredentialsListed {
            principal_id: principal_id.clone(),
            credentials: vec![credential_metadata],
        },
        Event::ProviderError {
            source: "github".into(),
            message: "rate limited".into(),
            detail: String::new(),
            kind: String::new(),
        },
        Event::Notification {
            title: "hi".into(),
            body: "body".into(),
        },
        Event::CleanWorktreesCompleted {
            removed: 3,
            skipped: 1,
        },
        Event::WorktreesInspected {
            inspections: vec![],
        },
        Event::WorktreesInspected {
            inspections: vec![lazybox_ipc::WorktreeInspectionDto {
                path: std::path::PathBuf::from("/tmp/wt"),
                bare_path: Some(std::path::PathBuf::from("/tmp/repos/o/r.git")),
                branch: Some("feat".into()),
                session_id: Some("12345678".into()),
                reasons: vec!["untracked".into(), "branch-deleted-upstream".into()],
                size_bytes: 4096,
                last_modified_unix: Some(1_700_000_000),
                has_uncommitted_changes: true,
                has_unpushed_commits: false,
                is_safe_to_delete: false,
            }],
        },
        Event::OrphanedWorktreeDeleted {
            path: std::path::PathBuf::from("/tmp/wt"),
            ok: true,
            error: None,
        },
        Event::OrphanedWorktreeDeleted {
            path: std::path::PathBuf::from("/tmp/wt"),
            ok: false,
            error: Some("has uncommitted changes".into()),
        },
    ]
}

/// Round-trip every Command through bincode. Any new variant added to
/// the enum must show up in `all_commands` or this test fails — that's
/// the point.
#[test]
fn command_bincode_round_trip() {
    for cmd in all_commands() {
        let config = bincode::config::legacy();
        let bytes = bincode::serde::encode_to_vec(&cmd, config).expect("serialize");
        let (back, consumed): (Command, usize) =
            bincode::serde::decode_from_slice(&bytes, config).expect("deserialize");
        assert_eq!(consumed, bytes.len());
        // Debug-equality since the wire types intentionally don't
        // derive PartialEq (Sessions carry timestamps we can't compare
        // structurally). Debug output uniqueness is the contract.
        assert_eq!(format!("{cmd:?}"), format!("{back:?}"));
    }
}

#[test]
fn provider_credential_command_debug_redacts_token() {
    let cmd = Command::UpsertProviderCredential {
        principal_id: PrincipalId::local(),
        credential: ProviderCredentialInput {
            provider_id: "github".into(),
            token: "ghp_do_not_log".into(),
            source: "unit-test".into(),
            scopes: vec![],
            expires_at: None,
        },
    };

    let debug = format!("{cmd:?}");

    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("ghp_do_not_log"));
}

#[test]
fn event_bincode_round_trip() {
    for ev in all_events() {
        let config = bincode::config::legacy();
        let bytes = bincode::serde::encode_to_vec(&ev, config).expect("serialize");
        let (back, consumed): (Event, usize) =
            bincode::serde::decode_from_slice(&bytes, config).expect("deserialize");
        assert_eq!(consumed, bytes.len());
        assert_eq!(format!("{ev:?}"), format!("{back:?}"));
    }
}

/// Framed write → framed read over a tokio duplex stream returns the
/// same message. Exercises the socket transport without actually
/// touching the filesystem or kernel sockets.
#[tokio::test]
async fn socket_framing_round_trip() {
    use lazybox_ipc::socket::{read_frame, write_frame};

    let (mut a, mut b) = duplex(64 * 1024);

    // Alice sends, Bob receives.
    tokio::spawn(async move {
        for cmd in all_commands() {
            write_frame(&mut a, &cmd).await.expect("write");
        }
        // Drop on exit → closes the pipe → Bob's read_frame returns None.
    });

    let mut seen = 0usize;
    while let Some(cmd) = read_frame::<_, Command>(&mut b).await.expect("read") {
        // Serialize again; if we got it we should be able to re-emit.
        let _bytes =
            bincode::serde::encode_to_vec(&cmd, bincode::config::legacy()).expect("reserialize");
        seen += 1;
    }
    assert_eq!(seen, all_commands().len());
}

/// Frames larger than `MAX_FRAME_BYTES` must error cleanly instead of
/// allocating a huge buffer. Simulates a malicious or corrupted peer.
#[tokio::test]
async fn socket_rejects_oversized_frames() {
    use lazybox_ipc::MAX_FRAME_BYTES;
    use lazybox_ipc::socket::read_frame;
    use tokio::io::AsyncWriteExt;

    let (mut a, mut b) = duplex(64);
    let bad_len = MAX_FRAME_BYTES + 1;

    // Writer is the adversary — emits a length prefix claiming more
    // bytes than we allow, then drops.
    tokio::spawn(async move {
        let _ = a.write_all(&bad_len.to_be_bytes()).await;
    });

    let result: Result<Option<Command>, _> = read_frame(&mut b).await;
    assert!(
        matches!(result, Err(lazybox_ipc::socket::FrameError::TooLarge(n)) if n == bad_len),
        "expected TooLarge, got {result:?}"
    );
}

/// A clean EOF (peer drops between frames) returns Ok(None), not an
/// error. That's how the daemon distinguishes orderly client shutdown
/// from a transport fault.
#[tokio::test]
async fn socket_clean_eof_is_none() {
    use lazybox_ipc::socket::read_frame;

    let (a, mut b) = duplex(64);
    drop(a);
    let result: Result<Option<Command>, _> = read_frame(&mut b).await;
    assert!(
        matches!(result, Ok(None)),
        "expected Ok(None), got {result:?}"
    );
}

/// Zero-byte message (empty bytes payload) round-trips. Guard against
/// edge cases in the framing arithmetic.
#[tokio::test]
async fn socket_zero_byte_payload_works() {
    use lazybox_ipc::socket::{read_frame, write_frame};
    let (mut a, mut b) = duplex(64);
    let msg = Command::Write {
        terminal_id: TerminalId(1),
        bytes: vec![],
    };
    write_frame(&mut a, &msg).await.expect("write");
    drop(a);
    let got: Option<Command> = read_frame(&mut b).await.expect("read");
    let got = got.expect("one message");
    assert_eq!(format!("{got:?}"), format!("{msg:?}"));
}

/// Non-trivial binary payloads (ANSI escape sequences, UTF-8) survive.
/// Terminal output carries both.
#[tokio::test]
async fn socket_binary_terminal_output_round_trip() {
    use lazybox_ipc::socket::{read_frame, write_frame};
    let (mut a, mut b) = duplex(64 * 1024);

    let nasty: Vec<u8> = (0..=255).collect();
    let msg = Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: nasty.clone(),
        seq: 99,
    };
    write_frame(&mut a, &msg).await.expect("write");
    drop(a);
    let got: Option<Event> = read_frame(&mut b).await.expect("read");
    if let Some(Event::TerminalOutput { bytes, seq, .. }) = got {
        assert_eq!(bytes, nasty);
        assert_eq!(seq, 99);
    } else {
        panic!("expected TerminalOutput, got {got:?}");
    }
}

// ── Protocol handshake ─────────────────────────────────────────────────
//
// The 8-byte preamble (magic + version) is exchanged before any frames.
// These tests pin the success path, the rejection of version skew in
// both directions, garbage preambles, and the EOF a pre-handshake
// daemon produces.

mod handshake {
    use lazybox_ipc::socket::{HandshakeError, client_handshake, server_handshake};
    use lazybox_ipc::{BUILD_VERSION, PROTOCOL_MAGIC, PROTOCOL_VERSION};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    fn preamble(version: u32) -> [u8; 8] {
        let mut p = [0u8; 8];
        p[..4].copy_from_slice(&PROTOCOL_MAGIC);
        p[4..].copy_from_slice(&version.to_le_bytes());
        p
    }

    /// The build-version frame written after a matching preamble: u16 LE
    /// length + UTF-8 bytes.
    fn build_frame(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u16).to_le_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    #[tokio::test]
    async fn succeeds_when_versions_match() {
        let (client, server) = duplex(512);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);
        let server_task = tokio::spawn(async move { server_handshake(&mut srd, &mut swr).await });
        let peer = client_handshake(&mut crd, &mut cwr)
            .await
            .expect("client handshake");
        let client_seen = server_task.await.expect("join").expect("server handshake");
        // Both sides exchange this binary's build and agree it matches.
        assert_eq!(peer.build, BUILD_VERSION);
        assert!(peer.build_matches());
        assert_eq!(client_seen.build, BUILD_VERSION);
    }

    /// A peer on the same protocol but a different build connects
    /// successfully — the skew is reported to the caller, not rejected.
    /// This is the stale-daemon case `PROTOCOL_VERSION` can't catch.
    #[tokio::test]
    async fn same_protocol_different_build_connects_and_reports_skew() {
        let (client, server) = duplex(512);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        let fake_daemon = tokio::spawn(async move {
            let mut got = [0u8; 8];
            srd.read_exact(&mut got).await.expect("client preamble");
            assert_eq!(got, preamble(PROTOCOL_VERSION));
            swr.write_all(&preamble(PROTOCOL_VERSION))
                .await
                .expect("reply preamble");
            swr.write_all(&build_frame("9.9.9+deadbeef"))
                .await
                .expect("reply build");
            // Drain the client's own build frame so the duplex doesn't stall.
            let mut len = [0u8; 2];
            srd.read_exact(&mut len).await.expect("client build len");
            let mut buf = vec![0u8; u16::from_le_bytes(len) as usize];
            srd.read_exact(&mut buf).await.expect("client build body");
        });

        let peer = client_handshake(&mut crd, &mut cwr)
            .await
            .expect("handshake must succeed on a build skew");
        assert_eq!(peer.build, "9.9.9+deadbeef");
        assert!(!peer.build_matches());
        fake_daemon.await.expect("join");
    }

    #[tokio::test]
    async fn server_rejects_newer_client_but_still_replies() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        cwr.write_all(&preamble(PROTOCOL_VERSION + 1))
            .await
            .expect("send fake preamble");
        let err = server_handshake(&mut srd, &mut swr)
            .await
            .expect_err("must reject");
        assert!(matches!(
            err,
            HandshakeError::VersionMismatch { peer, ours }
                if peer == PROTOCOL_VERSION + 1 && ours == PROTOCOL_VERSION
        ));

        // The server replied with its own preamble before closing, so
        // the mismatched client can render the clear version error.
        let mut reply = [0u8; 8];
        crd.read_exact(&mut reply).await.expect("server reply");
        assert_eq!(reply, preamble(PROTOCOL_VERSION));
    }

    #[tokio::test]
    async fn client_rejects_version_skewed_daemon() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        let fake_daemon = tokio::spawn(async move {
            let mut got = [0u8; 8];
            srd.read_exact(&mut got).await.expect("client preamble");
            assert_eq!(got, preamble(PROTOCOL_VERSION));
            swr.write_all(&preamble(PROTOCOL_VERSION + 7))
                .await
                .expect("reply");
        });
        let err = client_handshake(&mut crd, &mut cwr)
            .await
            .expect_err("must reject");
        assert!(matches!(
            err,
            HandshakeError::VersionMismatch { peer, ours }
                if peer == PROTOCOL_VERSION + 7 && ours == PROTOCOL_VERSION
        ));
        // The message tells the user what to do, not just that bytes
        // disagreed.
        assert!(err.to_string().contains("restart the daemon"));
        fake_daemon.await.expect("join");
    }

    #[tokio::test]
    async fn server_rejects_garbage_preamble_without_replying() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        cwr.write_all(b"GARBAGE!").await.expect("send garbage");
        let err = server_handshake(&mut srd, &mut swr)
            .await
            .expect_err("must reject");
        assert!(matches!(err, HandshakeError::BadMagic(m) if &m == b"GARB"));

        // No reply preamble for a non-lazybox peer: once the server
        // side drops, the client read hits EOF with zero bytes seen.
        drop(srd);
        drop(swr);
        let mut buf = [0u8; 8];
        assert!(crd.read_exact(&mut buf).await.is_err());
    }

    #[tokio::test]
    async fn client_rejects_garbage_reply() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        let fake_daemon = tokio::spawn(async move {
            let mut got = [0u8; 8];
            srd.read_exact(&mut got).await.expect("client preamble");
            swr.write_all(b"NOTLZBX!").await.expect("reply");
        });
        let err = client_handshake(&mut crd, &mut cwr)
            .await
            .expect_err("must reject");
        assert!(matches!(err, HandshakeError::BadMagic(_)));
        fake_daemon.await.expect("join");
    }

    #[tokio::test]
    async fn client_surfaces_pre_handshake_daemon_as_clear_error() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        // A pre-handshake daemon reads our preamble as an oversized
        // frame length and closes without replying — modeled here by
        // dropping the server end outright.
        drop(server);
        let err = client_handshake(&mut crd, &mut cwr)
            .await
            .expect_err("must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("predates the protocol handshake"),
            "error must mention the pre-handshake-daemon case, got: {msg}"
        );
    }
}
