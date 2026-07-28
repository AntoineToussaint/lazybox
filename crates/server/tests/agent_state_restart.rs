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
        .terminals
        .lock()
        .await
        .insert(terminal_id, backend_key.clone());
    config.terminal_meta.lock().await.insert(
        terminal_id,
        (session_key.clone(), TerminalKind::Agent("codex".into())),
    );
    config
        .agent_state_generations
        .lock()
        .await
        .insert(terminal_id, terminal_id.0);

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
        config.agent_state_for(terminal_id).await,
        Some(state),
        "seed transition must commit before restart"
    );
    (backend_key, session_key)
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
        let terminal_id = *restarted
            .terminals
            .lock()
            .await
            .keys()
            .next()
            .expect("recovered terminal");
        assert_eq!(
            restarted.agent_state_for(terminal_id).await,
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
                restarted.agent_state_for(terminal_id).await,
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
        while !restarted.terminals.lock().await.is_empty() {
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
    )
    .await;
    let fresh = snapshot_terminals(&restarted)
        .await
        .into_iter()
        .next()
        .expect("fresh terminal snapshot");
    assert_ne!(
        restarted
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
