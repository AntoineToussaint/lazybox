//! Smoke tests for the daemon's serve loop. We drive it over
//! `ipc::channel::pair` — zero serialization, zero sockets — so tests
//! are fast and deterministic.

use lazybox_ipc::{
    AgentApprovalDecision, AgentInputMessage, AgentQuestionAnswer, AgentRunId, AgentRuntimeMode,
    Command, Event, HookEvent, HookEventKind, PrincipalId, ProviderCredentialInput, TerminalId,
    TerminalKind, channel,
};
use lazybox_server::{Server, ServerConfig};
use std::time::Duration;

#[tokio::test]
async fn subscribe_yields_snapshot() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client.send(Command::Subscribe).unwrap();
    let evt = client.recv().await.expect("daemon responds");
    match evt {
        Event::Snapshot {
            workspaces,
            terminals,
            ..
        } => {
            // Contract under test: Subscribe ALWAYS replies with a
            // Snapshot before any live events. With no workspaces
            // persisted and no terminals spawned, both lists are
            // empty.
            assert!(workspaces.is_empty());
            assert!(terminals.is_empty());
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}
#[tokio::test]
async fn shutdown_closes_loop_cleanly() {
    let (client, server) = channel::pair();
    let handle = tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client.send(Command::Shutdown).unwrap();
    // Drop client to unblock the channel close path.
    drop(client);
    // If Shutdown isn't honored the task would hang here forever; the
    // test timeout is our backstop but a clean exit is the contract.
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("daemon exits promptly on Shutdown")
        .unwrap();
}
#[tokio::test]
async fn start_agent_run_unknown_agent_reports_error() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client
        .send(Command::StartAgentRun {
            session_key: "test:ws".into(),
            session_id: None,
            agent: "does-not-exist".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: None,
        })
        .unwrap();

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::ProviderError { message, .. } => {
            assert!(message.contains("no agent registered"));
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}
#[tokio::test]
async fn send_agent_input_unknown_run_reports_error() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client
        .send(Command::SendAgentInput {
            run_id: AgentRunId(99),
            message: AgentInputMessage {
                text: Some("hello".into()),
                json: None,
            },
        })
        .unwrap();

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::ProviderError { message, .. } => {
            assert!(message.contains("unknown agent run"));
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}
#[tokio::test]
async fn provider_credential_commands_return_metadata_without_secrets() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });
    let principal_id = PrincipalId::new("alice");

    client
        .send(Command::UpsertProviderCredential {
            principal_id: principal_id.clone(),
            credential: ProviderCredentialInput {
                provider_id: "github".into(),
                token: "ghp_do_not_log".into(),
                source: "unit-test".into(),
                scopes: vec!["repo".into()],
                expires_at: None,
            },
        })
        .unwrap();

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    assert!(!format!("{evt:?}").contains("ghp_do_not_log"));
    match evt {
        Event::ProviderCredentialUpdated {
            principal_id: event_principal,
            provider_id,
            metadata,
        } => {
            assert_eq!(event_principal, principal_id);
            assert_eq!(provider_id, "github");
            assert_eq!(metadata.principal_id, principal_id);
            assert_eq!(metadata.provider_id, "github");
            assert_eq!(metadata.source, "unit-test");
            assert_eq!(metadata.scopes, vec!["repo"]);
        }
        other => panic!("expected ProviderCredentialUpdated, got {other:?}"),
    }

    client
        .send(Command::ListProviderCredentials {
            principal_id: principal_id.clone(),
        })
        .unwrap();
    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::ProviderCredentialsListed {
            principal_id: event_principal,
            credentials,
        } => {
            assert_eq!(event_principal, principal_id);
            assert_eq!(credentials.len(), 1);
            assert_eq!(credentials[0].provider_id, "github");
        }
        other => panic!("expected ProviderCredentialsListed, got {other:?}"),
    }

    client
        .send(Command::RemoveProviderCredential {
            principal_id: principal_id.clone(),
            provider_id: "github".into(),
        })
        .unwrap();
    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::ProviderCredentialRemoved {
            principal_id: event_principal,
            provider_id,
        } => {
            assert_eq!(event_principal, principal_id);
            assert_eq!(provider_id, "github");
        }
        other => panic!("expected ProviderCredentialRemoved, got {other:?}"),
    }
}
#[tokio::test]
async fn client_drop_terminates_daemon_loop() {
    // The daemon is a long-running service but a single-client loop
    // should exit when its client drops; multi-client handling comes
    // later with a multiplexer.
    let (client, server) = channel::pair();
    let handle = tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });
    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("daemon exits when client drops")
        .unwrap();
}

/// Isolate filesystem-touching commands (CleanWorktrees, InspectWorktrees,
/// DeleteOrphanedWorktree) onto a throwaway `LAZYBOX_HOME` so the test
/// never scans or mutates the developer's real `~/.lazybox`.
struct IsolatedHome {
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedHome {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("LAZYBOX_HOME");
        // SAFETY: process-global, but within this test binary only this
        // guard sets it; an empty dir resolves readers to CI defaults, so
        // a leak to a concurrent test is harmless. Restored on drop.
        unsafe { std::env::set_var("LAZYBOX_HOME", tmp.path()) };
        Self { _tmp: tmp, prev }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("LAZYBOX_HOME", v) },
            None => unsafe { std::env::remove_var("LAZYBOX_HOME") },
        }
    }
}

/// Every `Command` variant except `Shutdown` (loop control, exercised
/// separately). Payloads are minimal-but-valid; handlers that need a
/// real workspace / network just error fast, which is fine — the point
/// is to drive each arm through the dispatcher, not to succeed.
fn all_non_shutdown_commands() -> Vec<Command> {
    let tid = TerminalId(1);
    let principal = PrincipalId::new("alice");
    let wkey = || lazybox_core::WorkspaceKey::new("test:ws");
    let pkey = || lazybox_core::ProjectKey::new("local-test");
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    vec![
        Command::Subscribe,
        Command::CreateSession {
            session_key: "test:ws".into(),
            kind: TerminalKind::Shell,
            label: None,
        },
        Command::Spawn {
            session_key: "test:ws".into(),
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: Some(cwd.clone()),
            initial_prompt: None,
            on_main: false,
        },
        Command::Write {
            terminal_id: tid,
            bytes: b"x".to_vec(),
        },
        Command::RecordUserMessage {
            terminal_id: tid,
            message: "hi".into(),
        },
        Command::InjectPrompt {
            terminal_id: tid,
            prompt: "p".into(),
            fallback_spawn: None,
        },
        Command::Resize {
            terminal_id: tid,
            cols: 80,
            rows: 24,
        },
        Command::Close { terminal_id: tid },
        Command::IngestHook {
            terminal_id: tid,
            hook: HookEvent {
                kind: HookEventKind::Stop,
                session_id: None,
                cwd: None,
                tool_name: None,
                notification: None,
            },
            backend_key: None,
        },
        Command::Kill {
            session_key: "test:ws".into(),
        },
        Command::RemoveMergedWorkspace {
            session_key: "test:ws".into(),
        },
        Command::DeleteProject {
            project_key: pkey(),
        },
        Command::CollapseIntoPr {
            issue_workspace_key: "test:ws".into(),
        },
        Command::MarkRead {
            session_key: "test:ws".into(),
        },
        Command::FocusWorkspace {
            session_key: "test:ws".into(),
        },
        Command::MarkActivityRead {
            session_key: "test:ws".into(),
            index: 0,
        },
        Command::UnmarkActivityRead {
            session_key: "test:ws".into(),
            index: 0,
        },
        Command::CreateWorkspace {
            name: "w".into(),
            project_key: pkey(),
            spawn_agent: None,
        },
        Command::CreateProject { name: "p".into() },
        Command::SetSessionLayout {
            session_key: "test:ws".into(),
            session_id_raw: "not-a-uuid".into(),
            layout_json: "{}".into(),
        },
        Command::Snooze {
            session_key: "test:ws".into(),
            until: chrono::Utc::now(),
        },
        Command::Unsnooze {
            session_key: "test:ws".into(),
        },
        Command::PostReply {
            session_key: "test:ws".into(),
            body: "b".into(),
        },
        Command::Refresh,
        Command::ConfirmMerge {
            issue_workspace_key: wkey(),
            pr_workspace_key: wkey(),
            accept: false,
        },
        Command::AdoptSessions {
            source_workspace_key: wkey(),
            target_workspace_key: wkey(),
        },
        Command::MergePr {
            workspace_key: wkey(),
        },
        Command::CloseIssue {
            workspace_key: wkey(),
        },
        Command::RequestReviewers {
            workspace_key: wkey(),
            logins: vec![],
        },
        Command::AddAssignees {
            workspace_key: wkey(),
            logins: vec![],
        },
        Command::SetAssignees {
            workspace_key: wkey(),
            logins: vec![],
        },
        Command::SetLabels {
            workspace_key: wkey(),
            names: vec![],
        },
        Command::FetchRepoLabels {
            workspace_key: wkey(),
        },
        Command::CleanWorktrees,
        Command::InspectWorktrees,
        Command::DeleteOrphanedWorktree {
            path: std::env::temp_dir().join("lzb-does-not-exist-206"),
            force: false,
        },
        Command::FetchPrDetails {
            workspace_key: wkey(),
        },
        Command::StartAgentRun {
            session_key: "test:ws".into(),
            session_id: None,
            agent: "no-such-agent".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: None,
        },
        Command::SendAgentInput {
            run_id: AgentRunId(1),
            message: AgentInputMessage {
                text: Some("x".into()),
                json: None,
            },
        },
        Command::InterruptAgentRun {
            run_id: AgentRunId(1),
        },
        Command::DecideAgentApproval {
            run_id: AgentRunId(1),
            request_id: "r".into(),
            decision: AgentApprovalDecision::Approve,
        },
        Command::AnswerAgentQuestion {
            run_id: AgentRunId(1),
            question_id: "q".into(),
            answer: AgentQuestionAnswer { answer: "a".into() },
        },
        Command::UpsertProviderCredential {
            principal_id: principal.clone(),
            credential: ProviderCredentialInput {
                provider_id: "github".into(),
                token: "t".into(),
                source: "test".into(),
                scopes: vec![],
                expires_at: None,
            },
        },
        Command::ListProviderCredentials {
            principal_id: principal.clone(),
        },
        Command::RemoveProviderCredential {
            principal_id: principal,
            provider_id: "github".into(),
        },
    ]
}

/// Acceptance (#206): drive EVERY command variant through the serve
/// loop and prove none of them wedges it. Two independent checks:
///   * the loop honors a trailing `Shutdown` within the timeout — a
///     command that held the inline lane would queue `Shutdown` behind
///     it and hang the join;
///   * `inline_budget_violations` stays 0 — the watchdog saw no inline
///     handler cross `INLINE_BUDGET`. If a future command is (mis)placed
///     in the inline lane and blocks, this assertion fails in CI rather
///     than surfacing as a dogfood "stuck spinner / frozen sync".
#[tokio::test]
async fn every_command_runs_without_wedging_the_serve_loop() {
    let _home = IsolatedHome::new();
    let config = ServerConfig::in_memory();
    let metrics = config.event_metrics.clone();
    let (client, server) = channel::pair();
    let handle = tokio::spawn(async move { Server::new(config).serve(server).await.unwrap() });

    for cmd in all_non_shutdown_commands() {
        client.send(cmd).unwrap();
    }
    client.send(Command::Shutdown).unwrap();
    drop(client);

    tokio::time::timeout(Duration::from_secs(20), handle)
        .await
        .expect("serve loop honored Shutdown — no command wedged the inline lane")
        .unwrap();

    assert_eq!(
        metrics.snapshot().inline_budget_violations,
        0,
        "an inline-lane command exceeded INLINE_BUDGET and blocked the serve loop",
    );
}

/// Acceptance (#206): a slow/stuck command handler cannot block bus/poll
/// forwarding. A detached `Spawn` parks on a 2s backend delay; while it
/// is stuck mid-provision we publish a `PollCompleted` on the bus and
/// assert the client receives it well before the spawn finishes. Pre-fix
/// (Spawn `.await`-ed inline) this event would have queued ~2s behind the
/// wedged handler.
#[tokio::test]
async fn a_stalled_handler_does_not_block_poll_forwarding() {
    let _home = IsolatedHome::new();
    let (config, mock) = ServerConfig::in_memory_with_mock();
    mock.set_spawn_delay(Duration::from_secs(2)).await;
    let bus = config.bus.clone();
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        let _ = Server::new(config).serve(server).await;
    });

    client.send(Command::Subscribe).unwrap();
    let snap = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("subscribe responds")
        .expect("snapshot");
    assert!(matches!(snap, Event::Snapshot { .. }));

    // Detached handler that will sit on the backend delay for ~2s.
    client
        .send(Command::Spawn {
            session_key: "test:stall".into(),
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            initial_prompt: None,
            on_main: false,
        })
        .unwrap();

    let t0 = tokio::time::Instant::now();
    bus.send(Event::PollCompleted {
        source: "github".into(),
        count: 3,
    })
    .unwrap();

    // The forwarded PollCompleted must arrive long before the 2s spawn
    // delay elapses — proof the stuck handler didn't hold the loop.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut got = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, client.recv()).await {
            Ok(Some(Event::PollCompleted { source, .. })) if source == "github" => {
                got = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    let elapsed = t0.elapsed();
    assert!(
        got,
        "PollCompleted was not forwarded while a handler stalled"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "poll forwarding was delayed {elapsed:?} by the stalled handler",
    );
}
