use lazybox_core::SessionKey;
use lazybox_ipc::{AgentState, Event, HookEvent, HookEventKind, TerminalId, TerminalKind};
use lazybox_server::ServerConfig;
use lazybox_server::backend::{MockBackend, SessionBackend};
use lazybox_server::spawn_handler::{
    handle_ingest_hook, handle_spawn, recover_sessions, snapshot_terminals,
};
use lazybox_store::{SqliteStore, Store};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn hook(kind: HookEventKind, notification: Option<&str>) -> HookEvent {
    HookEvent {
        kind,
        session_id: None,
        cwd: None,
        tool_name: None,
        notification: notification.map(str::to_string),
    }
}

async fn seed_persisted_state(
    path: &Path,
    backend: &MockBackend,
    state: AgentState,
    ordinal: u64,
) -> (String, SessionKey) {
    let backend_key = backend
        .spawn(&["codex".into()], None, &[], &format!("state-{ordinal}"))
        .await
        .expect("spawn surviving backend session");
    let session_key = SessionKey::from(format!("test:state-{ordinal}").as_str());
    let store = Arc::new(SqliteStore::open(path).expect("open sqlite store"));
    let metadata =
        serde_json::to_string(&(session_key.as_str(), TerminalKind::Agent("codex".into())))
            .expect("serialize terminal metadata");
    store
        .set_kv(&format!("terminal:{backend_key}"), &metadata)
        .expect("persist terminal metadata");

    let config = ServerConfig::with_store_and_backend(store, backend.as_backend());
    let terminal_id = TerminalId(10_000 + ordinal);
    config
        .terminal
        .register_terminal(
            terminal_id,
            backend_key.clone(),
            session_key.clone(),
            TerminalKind::Agent("codex".into()),
        )
        .await;
    config
        .terminal
        .record_agent_state_generation(terminal_id, terminal_id.0)
        .await;

    handle_ingest_hook(
        &config,
        terminal_id,
        Some(backend_key.clone()),
        hook(HookEventKind::PreToolUse, None),
    )
    .await;
    match state {
        AgentState::Working => {}
        AgentState::Done => {
            handle_ingest_hook(
                &config,
                terminal_id,
                Some(backend_key.clone()),
                hook(HookEventKind::Stop, None),
            )
            .await;
        }
        AgentState::InputNeeded => {
            handle_ingest_hook(
                &config,
                terminal_id,
                Some(backend_key.clone()),
                hook(HookEventKind::Notification, Some("permission_prompt")),
            )
            .await;
        }
        AgentState::Idle | AgentState::Exited { .. } => {
            panic!("test helper only seeds live-turn states")
        }
    }
    assert_eq!(
        config.terminal.agent_state_for(terminal_id).await,
        Some(state),
        "seed transition must commit before restart"
    );
    (backend_key, session_key)
}

async fn wait_for_subscribers(backend: &MockBackend, backend_key: &str, expected: usize) {
    // Recovery deliberately backs off after consecutive attachment failures.
    // The second retry can land just after 2s (plus stable jitter), so this
    // deadline tests eventual self-healing without pinning the old 250ms spin.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if backend.subscriber_count(backend_key).await == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("subscriber count deadline");
}

#[tokio::test]
async fn sqlite_restart_hydrates_working_done_and_input_needed_before_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    for (ordinal, state) in [
        (1, AgentState::Working),
        (2, AgentState::Done),
        (3, AgentState::InputNeeded),
    ] {
        let backend = MockBackend::new();
        let db = temp.path().join(format!("state-{ordinal}.db"));
        let (backend_key, session_key) = seed_persisted_state(&db, &backend, state, ordinal).await;
        backend
            .emit(&backend_key, b"replayed terminal output\r\n")
            .await;

        let restarted = ServerConfig::with_store_and_backend(
            Arc::new(SqliteStore::open(&db).expect("reopen sqlite store")),
            backend.as_backend(),
        );
        recover_sessions(&restarted).await;
        let terminal_id = restarted
            .terminal
            .terminal_ids()
            .await
            .into_iter()
            .next()
            .expect("recovered terminal");
        assert_eq!(
            restarted.terminal.agent_state_for(terminal_id).await,
            Some(state),
            "{state:?} must hydrate into the daemon cache"
        );
        let snapshots = snapshot_terminals(&restarted).await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].session_key, session_key);
        assert_eq!(
            snapshots[0].agent_state,
            Some(state),
            "{state:?} must be present in the initial terminal snapshot"
        );

        if state == AgentState::Working {
            let mut events = restarted.bus.subscribe();
            handle_ingest_hook(
                &restarted,
                terminal_id,
                Some(backend_key.clone()),
                hook(HookEventKind::Stop, None),
            )
            .await;
            let transition = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("state event deadline")
                .expect("state event");
            assert!(matches!(
                transition,
                Event::AgentState {
                    state: AgentState::Done,
                    ..
                }
            ));
        } else if state == AgentState::Done {
            let mut events = restarted.bus.subscribe();
            handle_ingest_hook(
                &restarted,
                terminal_id,
                Some(backend_key.clone()),
                hook(HookEventKind::SessionStart, None),
            )
            .await;
            assert_eq!(
                restarted.terminal.agent_state_for(terminal_id).await,
                Some(state),
                "{state:?} must reject a restart-era Idle candidate"
            );
            assert!(
                events.try_recv().is_err(),
                "a rejected {state:?} -> Idle edge must not be published"
            );
        } else {
            let mut events = restarted.bus.subscribe();
            handle_ingest_hook(
                &restarted,
                terminal_id,
                Some(backend_key.clone()),
                hook(HookEventKind::PreToolUse, None),
            )
            .await;
            let transition = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("state event deadline")
                .expect("state event");
            assert!(matches!(
                transition,
                Event::AgentState {
                    state: AgentState::Working,
                    ..
                }
            ));
        }
    }
}

#[tokio::test]
async fn recovered_agent_without_complete_lifecycle_history_starts_working() {
    let temp = tempfile::tempdir().expect("tempdir");
    for (ordinal, persist_generation) in [(20, false), (21, true)] {
        let backend = MockBackend::new();
        let backend_key = backend
            .spawn(&["codex".into()], None, &[], &format!("unknown-{ordinal}"))
            .await
            .expect("spawn surviving backend session");
        let session_key = SessionKey::from(format!("test:unknown-{ordinal}").as_str());
        let db = temp.path().join(format!("unknown-{ordinal}.db"));
        let store = Arc::new(SqliteStore::open(&db).expect("open sqlite store"));
        let metadata =
            serde_json::to_string(&(session_key.as_str(), TerminalKind::Agent("codex".into())))
                .expect("serialize terminal metadata");
        store
            .set_kv(&format!("terminal:{backend_key}"), &metadata)
            .expect("persist terminal metadata");
        if persist_generation {
            store
                .set_kv(
                    &format!("terminal-agent-state-generation:{backend_key}"),
                    "42",
                )
                .expect("persist generation without state");
        }

        let restarted = ServerConfig::with_store_and_backend(store, backend.as_backend());
        recover_sessions(&restarted).await;
        let snapshot = snapshot_terminals(&restarted)
            .await
            .into_iter()
            .next()
            .expect("recovered terminal");
        assert_eq!(snapshot.session_key, session_key);
        assert_eq!(
            snapshot.agent_state,
            Some(AgentState::Working),
            "a known recovered agent cannot be initialized as fresh Idle"
        );
    }
}

#[tokio::test]
async fn historical_replay_cannot_replace_restored_state_before_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("historical-replay.db");
    let backend = MockBackend::new();
    let (backend_key, _) = seed_persisted_state(&db, &backend, AgentState::Working, 30).await;
    backend
        .emit(
            &backend_key,
            b"Would you like to run this command?\r\nold prompt from scrollback\r\n",
        )
        .await;

    let restarted = ServerConfig::with_store_and_backend(
        Arc::new(SqliteStore::open(&db).expect("reopen sqlite store")),
        backend.as_backend(),
    );
    let mut events = restarted.bus.subscribe();
    recover_sessions(&restarted).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                events.recv().await.expect("recovery event bus"),
                Event::TerminalOutput { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("replay output deadline");

    let snapshot = snapshot_terminals(&restarted)
        .await
        .into_iter()
        .next()
        .expect("recovered terminal");
    assert_eq!(
        snapshot.agent_state,
        Some(AgentState::Working),
        "scrollback prompt text is history, not a new lifecycle transition"
    );
}

#[tokio::test]
async fn recovered_session_reattaches_after_subscription_failure_or_eof() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("reattach.db");
    let backend = MockBackend::new();
    let (backend_key, _) = seed_persisted_state(&db, &backend, AgentState::Working, 40).await;
    backend.fail_next_subscriptions(&backend_key, 1).await;

    let restarted = ServerConfig::with_store_and_backend(
        Arc::new(SqliteStore::open(&db).expect("reopen sqlite store")),
        backend.as_backend(),
    );
    recover_sessions(&restarted).await;
    wait_for_subscribers(&backend, &backend_key, 1).await;
    assert_eq!(
        snapshot_terminals(&restarted).await[0].agent_state,
        Some(AgentState::Working)
    );

    backend.disconnect_subscribers(&backend_key).await;
    wait_for_subscribers(&backend, &backend_key, 0).await;
    wait_for_subscribers(&backend, &backend_key, 1).await;
    assert_eq!(
        snapshot_terminals(&restarted).await[0].agent_state,
        Some(AgentState::Working),
        "attachment EOF must not be published as underlying session exit"
    );
}

#[tokio::test]
async fn recovered_dead_process_exits_and_fresh_spawn_has_no_old_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("exit.db");
    let backend = MockBackend::new();
    let (backend_key, _) = seed_persisted_state(&db, &backend, AgentState::Working, 10).await;
    backend.finish(&backend_key, 9).await;

    let restarted = ServerConfig::with_store_and_backend(
        Arc::new(SqliteStore::open(&db).expect("reopen sqlite store")),
        backend.as_backend(),
    );
    let mut events = restarted.bus.subscribe();
    recover_sessions(&restarted).await;
    let exited = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Event::AgentState { state, .. } =
                events.recv().await.expect("recovery event bus")
                && matches!(state, AgentState::Exited { code: Some(9) })
            {
                break state;
            }
        }
    })
    .await
    .expect("recovered exit deadline");
    assert_eq!(exited, AgentState::Exited { code: Some(9) });

    tokio::time::timeout(Duration::from_secs(2), async {
        while !restarted.terminal.is_empty().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("exit teardown deadline");

    handle_spawn(
        &restarted,
        SessionKey::from("test:fresh"),
        None,
        TerminalKind::Agent("codex".into()),
        Some(temp.path().to_string_lossy().into_owned()),
        None,
        false,
        false,
        None,
        false,
        lazybox_ipc::SpawnOrigin::Interactive,
    )
    .await;
    let fresh = snapshot_terminals(&restarted)
        .await
        .into_iter()
        .next()
        .expect("fresh terminal snapshot");
    assert_ne!(
        restarted
            .terminal
            .backend_key_for(fresh.terminal_id)
            .await
            .as_deref(),
        Some(backend_key.as_str())
    );
    assert_eq!(
        fresh.agent_state, None,
        "a newly spawned process must not inherit the exited generation's Working state"
    );
}
