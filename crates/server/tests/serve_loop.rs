//! Smoke tests for the daemon's serve loop. We drive it over
//! `ipc::channel::pair` — zero serialization, zero sockets — so tests
//! are fast and deterministic.

use lazybox_ipc::{
    AgentApprovalDecision, AgentInputMessage, AgentQuestionAnswer, AgentRunId, AgentRunRequestId,
    AgentRuntimeMode, Command, Event, HookEvent, HookEventKind, PrincipalId,
    ProviderCredentialInput, TerminalId, TerminalInputIntent, TerminalKind, channel,
};
use lazybox_server::backend::SessionBackend;
use lazybox_server::{Server, ServerConfig};
use std::time::Duration;

async fn send_with_backpressure(client: &lazybox_ipc::Client, mut command: Command) {
    loop {
        match client.send(command) {
            Ok(()) => return,
            Err(tokio::sync::mpsc::error::TrySendError::Full(returned)) => {
                command = returned;
                tokio::task::yield_now().await;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                panic!("daemon command channel closed while applying backpressure")
            }
        }
    }
}

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
async fn activate_workspace_broadcasts_a_focus_request() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client.send(Command::Subscribe).unwrap();
    let key = lazybox_core::SessionKey::new("github:o/r#674");
    client
        .send(Command::ActivateWorkspace {
            session_key: key.clone(),
        })
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(Event::WorkspaceFocusRequested { session_key }) = client.recv().await {
                break session_key;
            }
        }
    })
    .await
    .expect("focus request");
    assert_eq!(event, key);
}

/// Snippet MRU and update dismissals are daemon-owned (#548), so a value
/// recorded by one client is replayed to every subsequently-connecting
/// client through `Event::Snapshot` — the cross-transport parity the old
/// direct-store code could not provide (a `--connect` client wrote a
/// separate, unrelated `state.db` or none at all). One `Server` stands in
/// for "the daemon"; two independent connections stand in for the
/// in-process TUI and a `--connect` client.
#[tokio::test]
async fn client_kv_recorded_by_one_connection_replays_to_another() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let workspace = lazybox_core::Workspace::empty(
        lazybox_core::WorkspaceKey::new("github:o/r#611"),
        "main",
        chrono::Utc::now(),
    );
    config
        .store
        .save_workspace(&lazybox_store::WorkspaceRecord {
            key: workspace.key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
        })
        .expect("save workspace");
    let backend_key = mock
        .spawn(&[], None, &[], "confirmed-snippet")
        .await
        .expect("spawn shell");
    let terminal_id = TerminalId(611);
    config
        .terminal
        .register_terminal(
            terminal_id,
            backend_key.clone(),
            lazybox_core::SessionKey::new("github:o/r#611"),
            TerminalKind::Shell,
        )
        .await;
    let server_config = config.clone();
    let (mut server_client, server_conn) = channel::pair();
    tokio::spawn(async move {
        Server::new(server_config).serve(server_conn).await.unwrap();
    });

    server_client.send(Command::Subscribe).unwrap();
    assert!(matches!(
        server_client.recv().await,
        Some(Event::Snapshot { .. })
    ));

    // Connection A asks the daemon to deliver a real shell snippet. The
    // accepted backend write, not the request itself, records the MRU.
    server_client
        .send(Command::DeliverSnippet {
            terminal_id,
            snippet_key: "rev".into(),
            category: "Review".into(),
            body: "review the diff".into(),
            submit: true,
        })
        .unwrap();
    loop {
        if matches!(
            server_client.recv().await,
            Some(Event::SnippetDelivered {
                terminal_id: id,
                ref snippet_key,
                ..
            }) if id == terminal_id && snippet_key == "rev"
        ) {
            break;
        }
    }
    assert_eq!(
        mock.writes_for(&backend_key).await,
        vec![b"review the diff\r".to_vec()],
        "the success event follows an accepted terminal write",
    );
    let record = config
        .store
        .get_workspace(&workspace.key)
        .expect("load workspace")
        .expect("workspace exists");
    let recorded_workspace: lazybox_core::Workspace =
        serde_json::from_str(record.workspace_json.as_deref().expect("workspace json")).unwrap();
    assert_eq!(
        recorded_workspace.sent_snippets.recent().to_vec(),
        vec!["rev"]
    );
    server_client
        .send(Command::SetUpdateDismissal {
            target: "release:v0.2.0".into(),
        })
        .unwrap();

    // Connection B (stands in for a `--connect` client) subscribes and must
    // observe both. The writes are detached, so poll fresh subscriptions
    // until they land or the deadline passes.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (mut client_b, conn_b) = channel::pair();
        let cfg = config.clone();
        tokio::spawn(async move {
            Server::new(cfg).serve(conn_b).await.unwrap();
        });
        client_b.send(Command::Subscribe).unwrap();
        let evt = client_b.recv().await.expect("daemon responds");
        if let Event::Snapshot {
            recent_snippets,
            dismissed_updates,
            ..
        } = evt
        {
            if recent_snippets == vec!["rev".to_string()]
                && dismissed_updates == vec!["release:v0.2.0".to_string()]
            {
                return; // parity achieved
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "client B never saw the recorded state: recent={recent_snippets:?} \
                     dismissed={dismissed_updates:?}"
                );
            }
        } else {
            panic!("expected Snapshot, got {evt:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn rejected_snippet_delivery_does_not_update_recent_history() {
    let config = ServerConfig::in_memory();
    let server_config = config.clone();
    let (mut client, connection) = channel::pair();
    tokio::spawn(async move {
        Server::new(server_config).serve(connection).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    assert!(matches!(client.recv().await, Some(Event::Snapshot { .. })));

    let missing = TerminalId(999_611);
    client
        .send(Command::DeliverSnippet {
            terminal_id: missing,
            snippet_key: "rev".into(),
            category: "Review".into(),
            body: "review the diff".into(),
            submit: true,
        })
        .unwrap();
    loop {
        if matches!(
            client.recv().await,
            Some(Event::TerminalInputRejected { terminal_id, .. }) if terminal_id == missing
        ) {
            break;
        }
    }

    let (mut observer, observer_connection) = channel::pair();
    let observer_config = config.clone();
    tokio::spawn(async move {
        Server::new(observer_config)
            .serve(observer_connection)
            .await
            .unwrap();
    });
    observer.send(Command::Subscribe).unwrap();
    match observer.recv().await.expect("snapshot") {
        Event::Snapshot {
            recent_snippets, ..
        } => assert!(
            recent_snippets.is_empty(),
            "failed delivery must not enter Recent"
        ),
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn subscribe_is_admitted_only_once_per_connection() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client.send(Command::Subscribe).expect("first subscribe");
    assert!(matches!(client.recv().await, Some(Event::Snapshot { .. })));
    assert!(matches!(
        client.recv().await,
        Some(Event::ShellCommandConfig {
            command,
            configured: _,
        }) if !command.is_empty()
    ));
    // The first subscribe also pushes the spawnable-agent config and then
    // the auto-fix policy config after the shell config; drain both so the
    // next event is the duplicate-subscribe rejection.
    assert!(matches!(
        client.recv().await,
        Some(Event::AgentAvailabilityConfig { .. })
    ));
    assert!(matches!(
        client.recv().await,
        Some(Event::SnippetKeepMine { .. })
    ));
    assert!(matches!(
        client.recv().await,
        Some(Event::AutoFixPolicyConfig { .. })
    ));
    client
        .send(Command::Subscribe)
        .expect("duplicate reaches daemon");
    assert!(matches!(
        client.recv().await,
        Some(Event::CommandRejected { command, message })
            if command == "Subscribe" && message.contains("already subscribed")
    ));
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
/// SIGTERM-equivalent shutdown (the socket service raising the
/// graceful-stop watch) must give an in-flight detached mutation the
/// same bounded drain a client disconnect gets — pre-fix the service
/// aborted serve tasks outright and a merge/delete mid-write was
/// cancelled between its remote side effect and the local save.
#[tokio::test]
async fn graceful_stop_drains_in_flight_mutation_before_exit() {
    let _home = IsolatedHome::new();
    let (config, mock) = ServerConfig::in_memory_with_mock();
    // Slow enough to still be mid-provision when the stop lands, fast
    // enough to finish well inside the drain bound.
    mock.set_spawn_delay(Duration::from_millis(400)).await;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (client, server) = channel::pair();
    let handle = tokio::spawn({
        let config = config.clone();
        async move {
            Server::new(config)
                .with_graceful_stop(stop_rx)
                .serve(server)
                .await
        }
    });

    client
        .send(Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: "test:drain".into(),
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Shell,
            cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
            force_new: false,
        })
        .expect("spawn");
    // Let the serve loop dequeue Spawn onto the mutations JoinSet and
    // enter the backend's artificial delay.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        mock.list().await.expect("list").is_empty(),
        "spawn must still be in flight when the stop signal lands"
    );

    stop_tx.send(true).expect("signal");
    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("serve exits on graceful stop")
        .expect("join")
        .expect("serve result");

    assert_eq!(
        mock.list().await.expect("list").len(),
        1,
        "the in-flight mutation must complete during the drain, not be aborted"
    );
}

/// The drain is bounded: a mutation that outlives the bound is
/// abandoned (with a breadcrumb) instead of holding shutdown hostage.
#[tokio::test]
async fn graceful_stop_abandons_mutation_past_the_drain_bound() {
    let _home = IsolatedHome::new();
    let (config, mock) = ServerConfig::in_memory_with_mock();
    // Effectively wedged relative to the shortened bound below.
    mock.set_spawn_delay(Duration::from_secs(60)).await;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (client, server) = channel::pair();
    let handle = tokio::spawn({
        let config = config.clone();
        async move {
            Server::new(config)
                .with_graceful_stop(stop_rx)
                .with_mutation_drain_timeout(Duration::from_millis(200))
                .serve(server)
                .await
        }
    });

    client
        .send(Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: "test:wedged".into(),
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Shell,
            cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
            force_new: false,
        })
        .expect("spawn");
    tokio::time::sleep(Duration::from_millis(150)).await;

    stop_tx.send(true).expect("signal");
    // Joining well before the 60s backend delay proves the bound
    // abandoned the wedged task rather than waiting it out.
    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("serve exits once the drain bound elapses")
        .expect("join")
        .expect("serve result");
    assert!(
        mock.list().await.expect("list").is_empty(),
        "the wedged mutation was abandoned, not completed"
    );
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
            request_id: AgentRunRequestId("unknown-agent".into()),
            session_key: "test:ws".into(),
            session_id: None,
            source_terminal_id: None,
            agent: "does-not-exist".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: None,
            resume_latest: false,
            access: lazybox_ipc::AgentRunAccess::Default,
            model_alias: None,
        })
        .unwrap();

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::AgentRunStartFailed {
            request_id,
            message,
        } => {
            assert_eq!(request_id, AgentRunRequestId("unknown-agent".into()));
            assert!(message.contains("no agent registered"));
        }
        other => panic!("expected AgentRunStartFailed, got {other:?}"),
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
/// Serialize env-var mutations across concurrent tests. Each test that
/// modifies `LAZYBOX_HOME` acquires this lock to prevent interference.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct IsolatedHome {
    _guard: Option<std::sync::MutexGuard<'static, ()>>,
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedHome {
    fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Self::with_guard(Some(guard))
    }

    /// For a caller that already holds [`ENV_LOCK`] across a larger
    /// scope (the isolation-guard self-test): same behavior, no
    /// re-lock (the mutex is not reentrant).
    fn new_unlocked() -> Self {
        Self::with_guard(None)
    }

    fn with_guard(guard: Option<std::sync::MutexGuard<'static, ()>>) -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("LAZYBOX_HOME");
        // SAFETY: mutation is serialized by ENV_LOCK (held by this
        // guard or by the caller); restored on drop.
        unsafe { std::env::set_var("LAZYBOX_HOME", tmp.path()) };
        Self {
            _guard: guard,
            _tmp: tmp,
            prev,
        }
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
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: "test:ws".into(),
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Shell,
            cwd: Some(cwd.clone()),
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
            force_new: false,
        },
        Command::Write {
            terminal_id: tid,
            bytes: b"x".to_vec(),
            intent: TerminalInputIntent::Compose,
        },
        Command::RecordUserMessage {
            terminal_id: tid,
            prompt: lazybox_ipc::UserPrompt {
                text: "hi".into(),
                timestamp_ms: 1,
                source: lazybox_ipc::PromptSource::Typed,
            },
        },
        Command::InjectPrompt {
            terminal_id: tid,
            prompt: "p".into(),
            fallback_spawn: None,
            submit: true,
        },
        Command::Resize {
            terminal_id: tid,
            cols: 80,
            rows: 24,
        },
        Command::Close {
            terminal_id: tid,
            client_request_id: None,
        },
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
        Command::ActivateWorkspace {
            session_key: "test:ws".into(),
        },
        Command::MarkActivityRead {
            session_key: "test:ws".into(),
            index: 0,
            fingerprint: None,
        },
        Command::UnmarkActivityRead {
            session_key: "test:ws".into(),
            index: 0,
            fingerprint: None,
        },
        Command::CreateWorkspace {
            name: "w".into(),
            project_key: pkey(),
            spawn_agent: None,
            client_request_id: None,
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
            wake: None,
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
        Command::DeleteOrClose {
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
        Command::SyncWorkspace {
            workspace_key: wkey(),
        },
        Command::StartAgentRun {
            request_id: AgentRunRequestId("bulk-unknown-agent".into()),
            session_key: "test:ws".into(),
            session_id: None,
            source_terminal_id: None,
            agent: "no-such-agent".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: None,
            resume_latest: false,
            access: lazybox_ipc::AgentRunAccess::Default,
            model_alias: None,
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
        send_with_backpressure(&client, cmd).await;
    }
    send_with_backpressure(&client, Command::Shutdown).await;
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
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: "test:stall".into(),
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Shell,
            cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
            force_new: false,
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

/// A PTY write is order-sensitive, but it is not "provably fast": a broken
/// backend can await forever. The terminal command router must isolate that
/// terminal while the serve loop continues forwarding bus events and another
/// terminal's input proceeds independently.
#[tokio::test]
async fn a_wedged_terminal_write_does_not_block_the_bus_or_other_terminals() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key_a = mock.spawn(&[], None, &[], "a").await.expect("spawn a");
    let key_b = mock.spawn(&[], None, &[], "b").await.expect("spawn b");
    config
        .terminal
        .bind_backend(TerminalId(41), key_a.clone())
        .await;
    config
        .terminal
        .bind_backend(TerminalId(42), key_b.clone())
        .await;
    mock.wedge_write(&key_a).await;

    let bus = config.bus.clone();
    let (mut client, server) = channel::pair();
    let handle = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server).await }
    });
    client.send(Command::Subscribe).expect("subscribe");
    let _ = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("snapshot deadline")
        .expect("snapshot");

    client
        .send(Command::Write {
            terminal_id: TerminalId(41),
            bytes: b"wedged".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("write a");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if mock.write_attempts().await.contains(&key_a) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("wedged write entered backend");

    client
        .send(Command::Write {
            terminal_id: TerminalId(42),
            bytes: b"responsive".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("write b");
    bus.send(Event::PollCompleted {
        source: "github".into(),
        count: 7,
    })
    .expect("bus subscriber");

    tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            if mock.writes_for(&key_b).await == vec![b"responsive".to_vec()] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal B write must bypass wedged terminal A");
    let event = tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            if let Some(event) = client.recv().await
                && matches!(event, Event::PollCompleted { count: 7, .. })
            {
                break event;
            }
        }
    })
    .await
    .expect("bus event must bypass wedged terminal write");
    assert!(matches!(event, Event::PollCompleted { count: 7, .. }));

    handle.abort();
}

/// A delayed backend call gives the client time to queue a key burst. The
/// worker must preserve every byte in order while collapsing that burst into
/// a bounded number of backend writes.
#[tokio::test]
async fn terminal_write_bursts_are_coalesced_without_reordering_bytes() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key = mock.spawn(&[], None, &[], "burst").await.expect("spawn");
    config
        .terminal
        .bind_backend(TerminalId(51), key.clone())
        .await;
    mock.set_write_delay(&key, Duration::from_millis(75)).await;

    let (client, server) = channel::pair();
    let handle = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server).await }
    });
    client
        .send(Command::Write {
            terminal_id: TerminalId(51),
            bytes: b"0".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("first write");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if mock.write_attempts().await.contains(&key) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first write entered backend");

    let mut expected = b"0".to_vec();
    for i in 1..=24u8 {
        let byte = b'a' + (i % 26);
        expected.push(byte);
        client
            .send(Command::Write {
                terminal_id: TerminalId(51),
                bytes: vec![byte],
                intent: TerminalInputIntent::Compose,
            })
            .expect("queued write");
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let writes = mock.writes_for(&key).await;
            if writes.iter().map(Vec::len).sum::<usize>() == expected.len() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("burst delivered");
    let writes = mock.writes_for(&key).await;
    assert_eq!(
        writes.concat(),
        expected,
        "byte order and content are exact"
    );
    assert_eq!(writes.len(), 2, "24 queued keys collapse into one batch");

    client.send(Command::Shutdown).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("serve shutdown")
        .expect("join")
        .expect("serve result");
}

/// Resize notifications describe the latest geometry; intermediate drag
/// positions are obsolete once a newer resize is queued. The terminal lane
/// must collapse a storm without moving it across a Write ordering barrier.
#[tokio::test]
async fn terminal_resize_storm_collapses_to_latest_before_next_write() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key = mock
        .spawn(&[], None, &[], "resize-storm")
        .await
        .expect("spawn");
    config
        .terminal
        .bind_backend(TerminalId(52), key.clone())
        .await;
    // Hold the worker in its first command while the resize storm queues.
    mock.set_write_delay(&key, Duration::from_millis(75)).await;

    let (client, server) = channel::pair();
    let handle = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server).await }
    });
    client
        .send(Command::Write {
            terminal_id: TerminalId(52),
            bytes: b"before".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("first write");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if mock.write_attempts().await.contains(&key) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first write entered backend");

    for step in 1..=24u16 {
        client
            .send(Command::Resize {
                terminal_id: TerminalId(52),
                cols: 80 + step,
                rows: 20 + step,
            })
            .expect("queued resize");
    }
    client
        .send(Command::Write {
            terminal_id: TerminalId(52),
            bytes: b"after".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("ordering barrier");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.writes_for(&key).await.concat() == b"beforeafter" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("resize and trailing write delivered");
    assert_eq!(
        mock.resizes_for(&key).await,
        vec![(104, 44)],
        "only the newest consecutive geometry reaches the backend"
    );

    client.send(Command::Shutdown).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("serve shutdown")
        .expect("join")
        .expect("serve result");
}

/// Resize can wedge just like Write (a stalled tmux control client is enough).
/// Its per-terminal deadline and worker isolation must leave other terminals
/// responsive instead of freezing all input behind the resize.
#[tokio::test]
async fn a_wedged_terminal_resize_does_not_block_other_terminals() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key_a = mock
        .spawn(&[], None, &[], "resize-a")
        .await
        .expect("spawn a");
    let key_b = mock
        .spawn(&[], None, &[], "resize-b")
        .await
        .expect("spawn b");
    config
        .terminal
        .bind_backend(TerminalId(53), key_a.clone())
        .await;
    config
        .terminal
        .bind_backend(TerminalId(54), key_b.clone())
        .await;
    mock.wedge_resize(&key_a).await;

    let (client, server) = channel::pair();
    let handle = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server).await }
    });
    client
        .send(Command::Resize {
            terminal_id: TerminalId(53),
            cols: 120,
            rows: 40,
        })
        .expect("wedged resize");
    // Give the first terminal's worker a chance to enter the backend.
    tokio::time::sleep(Duration::from_millis(25)).await;
    client
        .send(Command::Write {
            terminal_id: TerminalId(54),
            bytes: b"still-responsive".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("write b");

    tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            if mock.writes_for(&key_b).await == vec![b"still-responsive".to_vec()] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal B must bypass terminal A's wedged resize");

    client.send(Command::Shutdown).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("serve shutdown")
        .expect("join")
        .expect("serve result");
}

/// A rejected Close is not terminal: the backend session still exists and
/// the user must be able to keep typing (and retry Close) on the same lane.
#[tokio::test]
async fn failed_terminal_close_keeps_the_io_lane_alive() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key = mock
        .spawn(&[], None, &[], "close-failure")
        .await
        .expect("spawn");
    config
        .terminal
        .bind_backend(TerminalId(55), key.clone())
        .await;
    mock.fail_kill(&key, "backend transport timed out").await;

    let (mut client, server) = channel::pair();
    let handle = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server).await }
    });
    client
        .send(Command::Close {
            terminal_id: TerminalId(55),
            client_request_id: Some("replacement-close".into()),
        })
        .expect("close");
    client
        .send(Command::Write {
            terminal_id: TerminalId(55),
            bytes: b"after-failed-close".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("write after failed close");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if mock.writes_for(&key).await == vec![b"after-failed-close".to_vec()] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed Close must not retire the terminal's I/O worker");
    let event = tokio::time::timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("correlated close result")
        .expect("close failure event");
    assert!(matches!(
        event,
        Event::CommandFailed {
            client_request_id,
            message,
        } if client_request_id == "replacement-close"
            && message.contains("backend transport timed out")
    ));

    client.send(Command::Shutdown).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("serve shutdown")
        .expect("join")
        .expect("serve result");
}

/// Draft persistence has its own ordered lane and debounce. A typing burst
/// therefore commits the latest revision, not one SQLite transaction per
/// character or an older task that happened to finish last.
#[tokio::test]
async fn composing_burst_persists_the_latest_ordered_revision() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key = mock.spawn(&[], None, &[], "draft").await.expect("spawn");
    // Draft persistence is keyed by the stable workspace session_key, so the
    // terminal needs its owning-workspace metadata registered (not just a bound
    // backend_key) for the composer write to resolve where to land.
    let session_key: lazybox_core::SessionKey = "acme/widget#61".into();
    config
        .terminal
        .register_terminal(
            TerminalId(61),
            key.clone(),
            session_key.clone(),
            TerminalKind::Agent("claude".into()),
        )
        .await;
    let (client, server) = channel::pair();
    let handle = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server).await }
    });

    for revision in 1..=24 {
        client
            .send(Command::RecordComposingBuffer {
                terminal_id: TerminalId(61),
                buffer: format!("draft-{revision}"),
            })
            .expect("draft revision");
    }
    client.send(Command::Shutdown).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("serve shutdown")
        .expect("join")
        .expect("serve result");

    assert_eq!(
        config
            .store
            .get_kv(&format!("workspace-draft:{}", session_key.as_str()))
            .expect("store read")
            .as_deref(),
        Some("draft-24")
    );
}

/// Each socket/in-process client owns its own FIFO router, but the backend PTY
/// is process-global. Two clients writing the same terminal must therefore
/// share one interaction lock instead of entering the backend concurrently.
#[tokio::test]
async fn multiple_clients_never_write_one_terminal_concurrently() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key = mock
        .spawn(&[], None, &[], "multi-client")
        .await
        .expect("spawn");
    config
        .terminal
        .bind_backend(TerminalId(71), key.clone())
        .await;
    mock.set_write_delay(&key, Duration::from_millis(100)).await;

    let (client_a, server_a) = channel::pair();
    let (client_b, server_b) = channel::pair();
    let serve_a = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server_a).await }
    });
    let serve_b = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server_b).await }
    });
    client_a
        .send(Command::Write {
            terminal_id: TerminalId(71),
            bytes: b"client-a".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("write a");
    client_b
        .send(Command::Write {
            terminal_id: TerminalId(71),
            bytes: b"client-b".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("write b");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.writes_for(&key).await.len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both writes delivered");
    assert_eq!(
        mock.max_concurrent_writes(),
        1,
        "independent client routers must serialize at the shared backend"
    );

    client_a.send(Command::Shutdown).expect("shutdown a");
    client_b.send(Command::Shutdown).expect("shutdown b");
    for serve in [serve_a, serve_b] {
        tokio::time::timeout(Duration::from_secs(2), serve)
            .await
            .expect("serve shutdown")
            .expect("join")
            .expect("serve result");
    }
}

/// InjectPrompt used to run as a detached mutation and could overtake a Write
/// that appeared before it on the same connection. Registering injection from
/// the terminal lane makes the earlier bytes a hard ordering barrier.
#[tokio::test]
async fn prompt_injection_cannot_overtake_prior_terminal_input() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key = mock
        .spawn(&[], None, &[], "inject-order")
        .await
        .expect("spawn");
    let terminal_id = TerminalId(72);
    config
        .terminal
        .register_terminal(
            terminal_id,
            key.clone(),
            lazybox_core::SessionKey::new("inject-order"),
            TerminalKind::Agent("claude".into()),
        )
        .await;
    mock.set_write_delay(&key, Duration::from_millis(75)).await;

    let (client, server) = channel::pair();
    let serve = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server).await }
    });
    client
        .send(Command::Write {
            terminal_id,
            bytes: b"prior-input".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("prior input");
    client
        .send(Command::InjectPrompt {
            terminal_id,
            prompt: "injected work".into(),
            fallback_spawn: None,
            submit: false,
        })
        .expect("inject");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.writes_for(&key).await.len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("input and injection delivered");
    let writes = mock.writes_for(&key).await;
    assert_eq!(writes[0], b"prior-input");
    assert!(
        String::from_utf8_lossy(&writes[1]).contains("injected work"),
        "second write is the injected prompt"
    );

    client.send(Command::Shutdown).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), serve)
        .await
        .expect("serve shutdown")
        .expect("join")
        .expect("serve result");
}

/// Paste and submit are one terminal interaction. A keyboard Write arriving
/// during the settle window must wait until Enter has submitted the injected
/// prompt, otherwise the user bytes become part of that prompt.
#[tokio::test]
async fn injected_paste_and_submit_are_atomic_against_user_input() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key = mock
        .spawn(&[], None, &[], "inject-atomic")
        .await
        .expect("spawn");
    let terminal_id = TerminalId(73);
    let session_key = lazybox_core::SessionKey::new("inject-atomic");
    config
        .terminal
        .register_terminal(
            terminal_id,
            key.clone(),
            session_key.clone(),
            TerminalKind::Agent("claude".into()),
        )
        .await;
    mock.set_write_delay(&key, Duration::from_millis(75)).await;

    let (client, server) = channel::pair();
    let serve = tokio::spawn({
        let config = config.clone();
        async move { Server::new(config).serve(server).await }
    });
    client
        .send(Command::InjectPrompt {
            terminal_id,
            prompt: "atomic injected work".into(),
            fallback_spawn: None,
            submit: true,
        })
        .expect("inject");
    // Queue user input immediately. The injection command's registration
    // handshake—not test timing—must establish paste/submit ownership before
    // the lane advances to this Write.
    client
        .send(Command::Write {
            terminal_id,
            bytes: b"interloper".to_vec(),
            intent: TerminalInputIntent::Compose,
        })
        .expect("input during settle");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if mock.writes_for(&key).await.len() >= 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("submit and queued input delivered");
    let writes = mock.writes_for(&key).await;
    assert!(String::from_utf8_lossy(&writes[0]).contains("atomic injected work"));
    assert_eq!(writes[1], b"\r", "submit stays adjacent to its paste");
    assert_eq!(writes[2], b"interloper", "user input follows submit");

    // Release the confirmation waiter so it cannot emit a later retry into
    // another test runtime after this assertion completes.
    let _ = config.bus.send(Event::AgentState {
        session_key,
        terminal_id,
        state: lazybox_ipc::AgentState::Working,
    });
    client.send(Command::Shutdown).expect("shutdown");
    tokio::time::timeout(Duration::from_secs(2), serve)
        .await
        .expect("serve shutdown")
        .expect("join")
        .expect("serve result");
}

/// Regression test for #1250: test isolation with global env vars.
/// Verifies that concurrent tests don't interfere when modifying LAZYBOX_HOME.
#[test]
fn env_isolation_guard_restores_state() {
    // Hold ENV_LOCK for the WHOLE test: the pre/post assertions are
    // only meaningful while no other test's `IsolatedHome` can
    // interleave. Reading `original` and asserting BETWEEN guard
    // drops used to happen unlocked — another env test could set
    // LAZYBOX_HOME in exactly that window, and once the daemon's
    // per-tick config loads shifted test scheduling (#scale) the race
    // fired deterministically.
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let original = std::env::var_os("LAZYBOX_HOME");

    // Scope 1: set to temp dir 1
    {
        let _guard1 = IsolatedHome::new_unlocked();
        let temp1 = std::env::var_os("LAZYBOX_HOME").expect("should be set");
        let path1 = std::path::PathBuf::from(&temp1);
        assert!(path1.exists(), "temp dir 1 should exist");
    }

    // After drop, state is restored
    match original {
        Some(ref orig) => {
            assert_eq!(
                std::env::var_os("LAZYBOX_HOME"),
                Some(orig.clone()),
                "LAZYBOX_HOME should be restored to original"
            )
        }
        None => {
            assert!(
                std::env::var_os("LAZYBOX_HOME").is_none(),
                "LAZYBOX_HOME should be cleared after drop"
            )
        }
    }

    // Scope 2: independent isolation with no interference
    {
        let _guard2 = IsolatedHome::new_unlocked();
        let temp2 = std::env::var_os("LAZYBOX_HOME").expect("should be set again");
        let path2 = std::path::PathBuf::from(&temp2);
        assert!(path2.exists(), "temp dir 2 should exist");
    }

    // Final state matches original
    match original {
        Some(ref orig) => {
            assert_eq!(
                std::env::var_os("LAZYBOX_HOME"),
                Some(orig.clone()),
                "LAZYBOX_HOME should be restored again"
            )
        }
        None => {
            assert!(
                std::env::var_os("LAZYBOX_HOME").is_none(),
                "LAZYBOX_HOME should be cleared again after final drop"
            )
        }
    }
}

/// Regression test for #1255: UnmarkActivityRead fingerprint prevents
/// unmarking the wrong activity when indices shift between mark and unmark.
/// Scenario:
/// 1. Mark activity[1] as read using its fingerprint
/// 2. A poll inserts a new activity at index 0 (shifts everything down)
/// 3. Unmark using the stored fingerprint
/// 4. Verify activity[1] (now at index 2) is unmarked, not activity at index 1
#[tokio::test]
async fn unmark_activity_read_with_fingerprint_handles_index_shifts() {
    use chrono::Utc;
    use lazybox_core::{Activity, ActivityFingerprint, ActivityKind, Workspace};

    let (config, _mock) = ServerConfig::in_memory_with_mock();
    let workspace_key = lazybox_core::WorkspaceKey::new("github:o/r#100");

    // Create activities with distinguishable fingerprints
    let now = Utc::now();
    let activities = vec![
        Activity {
            author: "alice".into(),
            body: "first comment".into(),
            created_at: now,
            kind: ActivityKind::Comment,
            node_id: Some("IC_1".into()),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        },
        Activity {
            author: "bob".into(),
            body: "second comment".into(),
            created_at: now,
            kind: ActivityKind::Comment,
            node_id: Some("IC_2".into()),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        },
    ];

    // Create workspace with initial activities
    let mut workspace = Workspace::empty(workspace_key.clone(), "main", now);
    workspace.activity = activities;

    // Persist the workspace
    config
        .store
        .save_workspace(&lazybox_store::WorkspaceRecord {
            key: workspace_key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
        })
        .unwrap();

    // Get the fingerprint of activity[1] (bob's comment) before marking
    let target_fingerprint = ActivityFingerprint::of(&workspace.activity[1]);

    // Mark activity[1] as read
    lazybox_server::workspace::mark_activity_read(
        &config,
        &workspace_key,
        1,
        Some(&target_fingerprint),
    )
    .await;

    // Verify it's marked as read
    let ws_before: lazybox_core::Workspace = {
        let record = config
            .store
            .get_workspace(&workspace_key)
            .expect("load workspace")
            .expect("workspace exists");
        serde_json::from_str(record.workspace_json.as_deref().expect("workspace json")).unwrap()
    };
    assert!(
        !ws_before.is_activity_unread(1),
        "activity[1] should be marked as read"
    );
    assert!(
        ws_before.is_activity_unread(0),
        "activity[0] should still be unread"
    );

    // Simulate a poll by inserting a new activity at the top
    let new_activity = Activity {
        author: "charlie".into(),
        body: "newest comment".into(),
        created_at: now,
        kind: ActivityKind::Comment,
        node_id: Some("IC_0".into()),
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    };
    let mut ws_after_poll = ws_before.clone();
    ws_after_poll.activity.insert(0, new_activity);
    // Read state is POSITIONAL (`read_indices`), not fingerprint-keyed. A real
    // poll remaps those indices to the shifted positions via the upsert
    // seen-preservation path; this manual remap stands in for that, moving
    // bob's read-mark from index 1 to its new index 2. Without it the unmark
    // has no read-mark at bob's row to undo.
    ws_after_poll.read_indices = std::iter::once(2).collect();

    config
        .store
        .save_workspace(&lazybox_store::WorkspaceRecord {
            key: workspace_key.as_str().to_string(),
            created_at: ws_after_poll.created_at,
            workspace_json: Some(serde_json::to_string(&ws_after_poll).unwrap()),
        })
        .unwrap();

    // Unmark using the stored fingerprint
    // The fingerprint should resolve to index 2 (bob's comment is now at index 2)
    lazybox_server::workspace::unmark_activity_read(
        &config,
        &workspace_key,
        1,
        Some(&target_fingerprint),
    )
    .await;

    // Verify the correct activity was unmarked
    let ws_final: lazybox_core::Workspace = {
        let record = config
            .store
            .get_workspace(&workspace_key)
            .expect("load workspace")
            .expect("workspace exists");
        serde_json::from_str(record.workspace_json.as_deref().expect("workspace json")).unwrap()
    };

    // activity[0] (charlie's new comment) should be unread
    assert!(
        ws_final.is_activity_unread(0),
        "activity[0] (charlie's comment) should be unread"
    );
    // activity[1] (alice's original comment) should still be unread
    assert!(
        ws_final.is_activity_unread(1),
        "activity[1] (alice's comment) should still be unread"
    );
    // activity[2] (bob's comment, now at index 2 but with correct fingerprint) should be unmarked
    assert!(
        ws_final.is_activity_unread(2),
        "activity[2] (bob's comment) should be unmarked via fingerprint resolution"
    );

    // The key assertion: if fingerprint didn't work, activity[1] would be unmarked instead
    // This verifies the fingerprint correctly resolved through the index shift
}
