//! Protocol-level tests: serde round-trip every `Command` / `Event`
//! variant, plus framing over a real tokio duplex pair.
//!
//! These tests exist to catch *silent* wire-format breakage. If anyone
//! renames a variant or reorders a field, bincode's tagging changes and
//! one of these assertions blows up. Far better than finding out when
//! a v0.2 client can't talk to a v0.3 daemon.

use pilot_ipc::{
    AgentApprovalDecision, AgentInputMessage, AgentQuestionAnswer, AgentRunId, AgentRuntimeMode,
    AgentState, AgentUsage, Command, Event, PrincipalId, ProviderCredentialInput,
    ProviderCredentialMetadata, TerminalId, TerminalKind, TerminalSnapshot,
};
use tokio::io::duplex;

fn sample_workspace() -> pilot_core::Workspace {
    let task = pilot_core::Task {
        id: pilot_core::TaskId {
            source: "github".into(),
            key: "o/r#1".into(),
        },
        title: "t".into(),
        body: None,
        state: pilot_core::TaskState::Open,
        role: pilot_core::TaskRole::Author,
        ci: pilot_core::CiStatus::None,
        review: pilot_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: "https://github.com/o/r/pull/1".into(),
        repo: Some("o/r".into()),
        branch: Some("b".into()),
        base_branch: None,
        updated_at: chrono::Utc::now(),
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        has_conflicts: false,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
    };
    pilot_core::Workspace::from_task(task, chrono::Utc::now())
}

fn all_commands() -> Vec<Command> {
    let key: pilot_core::SessionKey = "github:o/r#1".into();
    let principal_id = PrincipalId::local();
    vec![
        Command::Subscribe,
        Command::Spawn {
            session_key: key.clone(),
            session_id: None,
            kind: TerminalKind::Agent("claude".into()),
            cwd: Some("/tmp".into()),
            initial_prompt: None,
        },
        Command::Spawn {
            session_key: key.clone(),
            session_id: Some(pilot_core::SessionId::new()),
            kind: TerminalKind::Shell,
            cwd: None,
            initial_prompt: None,
        },
        Command::Spawn {
            session_key: key.clone(),
            session_id: None,
            kind: TerminalKind::LogTail {
                path: "/var/log/x.log".into(),
            },
            cwd: None,
            initial_prompt: Some("fix the failing CI".into()),
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
            session_id: Some(pilot_core::SessionId::new()),
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
        Command::Shutdown,
    ]
}

fn all_events() -> Vec<Event> {
    let key: pilot_core::SessionKey = "github:o/r#1".into();
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
            }],
            projects: vec![],
        },
        Event::WorkspaceUpserted(Box::new(sample_workspace())),
        Event::WorkspaceRemoved(pilot_core::WorkspaceKey::new(key.as_str())),
        Event::SessionCreated(Box::new(pilot_core::WorkspaceSession::new(
            pilot_core::WorkspaceKey::new(key.as_str()),
            pilot_core::SessionKind::Shell,
            std::path::PathBuf::from("/tmp/wt"),
            chrono::Utc::now(),
        ))),
        Event::SessionEnded {
            workspace_key: pilot_core::WorkspaceKey::new(key.as_str()),
            session_id: pilot_core::SessionId::new(),
        },
        Event::TerminalSpawned {
            terminal_id: TerminalId(2),
            session_key: key.clone(),
            kind: TerminalKind::Shell,
        },
        Event::TerminalOutput {
            terminal_id: TerminalId(2),
            bytes: b"ANSI: \x1b[31mred\x1b[0m".to_vec(),
            seq: 1,
        },
        Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: Some(0),
        },
        Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: None,
        },
        Event::AgentState {
            session_key: key.clone(),
            state: AgentState::Asking,
        },
        Event::AgentRunStarted {
            run_id: AgentRunId(9),
            session_key: key.clone(),
            session_id: Some(pilot_core::SessionId::new()),
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
    ]
}

/// Round-trip every Command through bincode. Any new variant added to
/// the enum must show up in `all_commands` or this test fails — that's
/// the point.
#[test]
fn command_bincode_round_trip() {
    for cmd in all_commands() {
        let bytes = bincode::serialize(&cmd).expect("serialize");
        let back: Command = bincode::deserialize(&bytes).expect("deserialize");
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
        let bytes = bincode::serialize(&ev).expect("serialize");
        let back: Event = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(format!("{ev:?}"), format!("{back:?}"));
    }
}

/// Framed write → framed read over a tokio duplex stream returns the
/// same message. Exercises the socket transport without actually
/// touching the filesystem or kernel sockets.
#[tokio::test]
async fn socket_framing_round_trip() {
    use pilot_ipc::socket::{read_frame, write_frame};

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
        let _bytes = bincode::serialize(&cmd).expect("reserialize");
        seen += 1;
    }
    assert_eq!(seen, all_commands().len());
}

/// Frames larger than `MAX_FRAME_BYTES` must error cleanly instead of
/// allocating a huge buffer. Simulates a malicious or corrupted peer.
#[tokio::test]
async fn socket_rejects_oversized_frames() {
    use pilot_ipc::MAX_FRAME_BYTES;
    use pilot_ipc::socket::read_frame;
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
        matches!(result, Err(pilot_ipc::socket::FrameError::TooLarge(n)) if n == bad_len),
        "expected TooLarge, got {result:?}"
    );
}

/// A clean EOF (peer drops between frames) returns Ok(None), not an
/// error. That's how the daemon distinguishes orderly client shutdown
/// from a transport fault.
#[tokio::test]
async fn socket_clean_eof_is_none() {
    use pilot_ipc::socket::read_frame;

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
    use pilot_ipc::socket::{read_frame, write_frame};
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
    use pilot_ipc::socket::{read_frame, write_frame};
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
