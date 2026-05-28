//! End-to-end tests for the daemon's Spawn → backend → bus pipeline.
//!
//! Backend is the in-memory [`MockBackend`] — no real shells / tmux /
//! curl. Tests drive synthetic output via `MockBackend::emit` and end
//! sessions via `finish`.

use pilot_ipc::{Command, Event, TerminalKind, channel};
use pilot_server::backend::{MockBackend, SessionBackend};
use pilot_server::{Server, ServerConfig};
use pilot_store::MemoryStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Per-test deadline. Workspace rule: every async test bounds itself
/// so a deadlock is reported as a failure, not a hung suite.
const TEST_DEADLINE: Duration = Duration::from_secs(5);

/// Drain events until we see one matching `pred` or hit the deadline.
async fn wait_for<F: FnMut(&Event) -> bool>(
    client: &mut pilot_ipc::Client,
    mut pred: F,
    budget: Duration,
) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, client.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return Some(ev);
                }
            }
            _ => return None,
        }
    }
    None
}

async fn run_daemon(config: ServerConfig) -> pilot_ipc::Client {
    let (client, server) = channel::pair();
    tokio::spawn(async move {
        let _ = Server::new(config).serve(server).await;
    });
    client
}

async fn subscribed(config: ServerConfig) -> pilot_ipc::Client {
    let mut client = run_daemon(config).await;
    client.send(Command::Subscribe).unwrap();
    let _snapshot = client.recv().await.expect("snapshot");
    client
}

async fn spawn_and_wait(
    client: &mut pilot_ipc::Client,
    kind: TerminalKind,
) -> pilot_ipc::TerminalId {
    client
        .send(Command::Spawn {
            session_key: "test:ws-1".into(),
            session_id: None,
            kind,
            cwd: None,
            initial_prompt: None,
        })
        .unwrap();
    let spawned = wait_for(
        client,
        |e| matches!(e, Event::TerminalSpawned { .. }),
        Duration::from_secs(2),
    )
    .await
    .expect("TerminalSpawned arrived");
    match spawned {
        Event::TerminalSpawned { terminal_id, .. } => terminal_id,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn spawn_shell_emits_terminal_spawned_event() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config).await;
        let _ = spawn_and_wait(&mut client, TerminalKind::Shell).await;
    })
    .await
    .expect("deadline");
}
#[tokio::test]
async fn unknown_agent_id_emits_provider_error() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config).await;
        client
            .send(Command::Spawn {
                session_key: "test:ws-1".into(),
                session_id: None,
                kind: TerminalKind::Agent("does-not-exist".into()),
                cwd: None,
                initial_prompt: None,
            })
            .unwrap();
        let evt = wait_for(
            &mut client,
            |e| matches!(e, Event::ProviderError { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("ProviderError arrived");
        if let Event::ProviderError { message, .. } = evt {
            assert!(
                message.contains("no agent registered"),
                "unexpected message: {message}"
            );
        }
    })
    .await
    .expect("deadline");
}
#[tokio::test]
async fn spawned_subprocess_output_reaches_client_via_bus() {
    timeout(TEST_DEADLINE, async {
        // Build the config + grab the typed mock so the test can
        // inject output the daemon's pump task will forward.
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Shell).await;

        // Find the backend key the daemon assigned. There's exactly
        // one mocked session at this point.
        let keys = mock.list().await.unwrap();
        assert_eq!(keys.len(), 1);
        let key = keys.into_iter().next().unwrap();

        // Inject synthetic output. The pump task should forward it as
        // Event::TerminalOutput, exactly like a real PTY would.
        mock.emit(&key, b"pilot-marker").await;

        let evt = wait_for(
            &mut client,
            |e| match e {
                Event::TerminalOutput {
                    terminal_id: tid,
                    bytes,
                    ..
                } => *tid == terminal_id && bytes == b"pilot-marker",
                _ => false,
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            evt.is_some(),
            "expected to see 'pilot-marker' in TerminalOutput"
        );
    })
    .await
    .expect("deadline");
}
#[tokio::test]
async fn close_drops_terminal_and_emits_exit_event() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Shell).await;

        client.send(Command::Close { terminal_id }).unwrap();

        // handle_close calls backend.kill; the mock closes its
        // subscribers, the pump task awaits wait_exit, then broadcasts
        // TerminalExited and removes the terminal from the map.
        let exited = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalExited { terminal_id: tid, .. } if *tid == terminal_id),
            Duration::from_secs(2),
        )
        .await;
        assert!(exited.is_some(), "TerminalExited should arrive after Close");

        // Map should be empty.
        let map_len = config.terminals.lock().await.len();
        assert_eq!(map_len, 0, "terminal map cleared after exit");
    })
    .await
    .expect("deadline");
}
#[tokio::test]
async fn snapshot_includes_running_terminals_for_late_subscribers() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut producer = subscribed(config.clone()).await;
        let _ = spawn_and_wait(&mut producer, TerminalKind::Shell).await;

        // A second client subscribes — its initial Snapshot should
        // include the terminal already running.
        let mut consumer = run_daemon(config.clone()).await;
        consumer.send(Command::Subscribe).unwrap();
        let evt = consumer.recv().await.expect("snapshot");
        match evt {
            Event::Snapshot { terminals, .. } => {
                assert_eq!(terminals.len(), 1, "running terminal in snapshot");
            }
            _ => panic!("expected Snapshot first"),
        }
    })
    .await
    .expect("deadline");
}
/// Regression: `--connect` clients reconnecting mid-session need the
/// PTY ring buffer in `TerminalSnapshot.replay` to reconstruct the
/// screen. Without it they see a blank terminal until the next chunk
/// arrives — which for an idle agent could be never.
#[tokio::test]
async fn snapshot_replay_includes_buffered_pty_output_for_late_subscribers() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut producer = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut producer, TerminalKind::Shell).await;

        // Drive synthetic output and wait for the pump task to fan it
        // out, so the next Snapshot will include it in `replay`.
        let key = mock.list().await.unwrap().into_iter().next().unwrap();
        mock.emit(&key, b"pilot-replay-marker").await;
        let _ = wait_for(
            &mut producer,
            |e| match e {
                Event::TerminalOutput { bytes, .. } => bytes == b"pilot-replay-marker",
                _ => false,
            },
            Duration::from_secs(2),
        )
        .await
        .expect("marker output reached bus");

        // Fresh client subscribes after the output landed.
        let mut consumer = run_daemon(config).await;
        consumer.send(Command::Subscribe).unwrap();
        let evt = consumer.recv().await.expect("snapshot");
        match evt {
            Event::Snapshot { terminals, .. } => {
                let term = terminals
                    .iter()
                    .find(|t| t.terminal_id == terminal_id)
                    .expect("our terminal in snapshot");
                assert_eq!(
                    term.replay, b"pilot-replay-marker",
                    "snapshot replay should contain pre-subscription output",
                );
                assert!(term.last_seq > 0, "last_seq advanced past 0");
            }
            _ => panic!("expected Snapshot first"),
        }
    })
    .await
    .expect("deadline");
}
/// Recovery scenario: a backend has a session running (simulating
/// "pilot crashed"), then a fresh `ServerConfig` is built around the
/// same backend (simulating "pilot restarted"). `recover_sessions`
/// should register the survivor on the new config so the TUI sees it.
#[tokio::test]
async fn recover_sessions_reattaches_survivors() {
    timeout(TEST_DEADLINE, async {
        let backend = MockBackend::new();
        // Pre-existing session — simulates one that survived the
        // previous pilot run. Spawned directly through the backend,
        // not through spawn_handler, so it's known to the backend
        // but not to any ServerConfig.
        let preexisting = backend
            .spawn(&["echo".into(), "hello".into()], None, &[], "preexisting")
            .await
            .unwrap();

        // Fresh config pointing at the SAME backend instance.
        let store: Arc<dyn pilot_store::Store> = Arc::new(MemoryStore::new());
        let backend_arc: Arc<dyn SessionBackend> = Arc::new(backend.clone());
        let config = ServerConfig::with_store_and_backend(store, backend_arc);
        assert!(config.terminals.lock().await.is_empty());

        // Listen on the bus before recovery so TerminalSpawned isn't lost.
        let mut bus = config.bus.subscribe();

        pilot_server::spawn_handler::recover_sessions(&config).await;

        // Map now has the survivor under a fresh wire id.
        let map = config.terminals.lock().await;
        assert_eq!(map.len(), 1, "expected one recovered session, got {map:?}");
        let recovered_key = map.values().next().unwrap().clone();
        assert_eq!(recovered_key, preexisting);
        drop(map);

        // TerminalSpawned hits the bus.
        let evt = timeout(Duration::from_secs(1), bus.recv())
            .await
            .expect("bus event")
            .expect("not closed");
        assert!(matches!(evt, Event::TerminalSpawned { .. }));
    })
    .await
    .expect("deadline");
}

/// Regression / smoke check for the **ingest-into-agent** path
/// (issue #50). When work is handed to an agent — either by the user
/// pressing `w` or by the `@pilot`-mention auto-spawn — the agent is
/// `Spawn`ed with an `initial_prompt`. The daemon must actually
/// deliver that prompt to the agent's terminal once it's ready to
/// receive input; if it doesn't, the agent starts but never learns
/// what work to do (exactly the "ingest is broken" symptom).
///
/// This drives the full path through `handle_spawn`: spawn a Claude
/// agent with an initial prompt, drive the synthetic "input box is
/// ready" output so the inject task fires, then assert the prompt
/// bytes (and the separate submit keystroke) reached the backend.
#[tokio::test]
async fn spawn_with_initial_prompt_delivers_work_to_agent() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;

        const WORK: &str = "Implement GitHub issue #50: ingest is broken.";
        client
            .send(Command::Spawn {
                session_key: "test:ws-ingest".into(),
                session_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: Some(WORK.into()),
            })
            .unwrap();
        let _ = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("TerminalSpawned arrived");

        // One mocked session: the Claude agent we just spawned.
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        // Drive Claude's "ready for a pasted prompt" screen: the input
        // box footer (the paired `Esc to cancel` / `Tab to amend`
        // markers `detect_ready_for_prompt` keys on) with no permission
        // gate up. Without it the inject only fires on the slow
        // settle/hard-deadline fallback.
        mock.emit(&key, b"Esc to cancel  Tab to amend").await;

        // Poll the backend's write log until the work prompt shows up
        // (the inject task runs on its own tokio task).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let joined = loop {
            let joined = mock
                .writes_for(&key)
                .await
                .into_iter()
                .flatten()
                .collect::<Vec<u8>>();
            let done = String::from_utf8_lossy(&joined).contains(WORK) && joined.contains(&b'\r');
            if done || tokio::time::Instant::now() >= deadline {
                break joined;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let text = String::from_utf8_lossy(&joined);
        assert!(
            text.contains(WORK),
            "agent never received the work prompt; backend writes = {text:?}"
        );
        // Claude's prompt is committed by a separate `\r` submit after
        // the paste settles — without it the prompt sits unsent in the
        // input box.
        assert!(
            joined.contains(&b'\r'),
            "work prompt was pasted but never submitted (no Enter keystroke); writes = {text:?}"
        );
    })
    .await
    .expect("deadline");
}

/// Regression: stale `terminal_id` in `InjectPrompt` falls back to
/// Spawn when `fallback_spawn` is supplied. Symptom pre-fix: user
/// presses `w` (work) right after the agent crashed, the TUI's
/// cached terminal id still pointed at the dead terminal, the
/// daemon's `handle_inject_prompt` quietly no-op'd, and the user's
/// prompt was lost. After the fix the unknown id triggers a fresh
/// `Spawn` carrying the same workspace + agent + cwd from the
/// `SpawnFallback` payload.
#[tokio::test]
async fn inject_prompt_falls_back_to_spawn_when_terminal_dead() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config).await;

        // Use a `TerminalId` that has never been issued. Without the
        // fallback path this command silently no-ops on the daemon.
        let dead_id = pilot_ipc::TerminalId(99_999);
        client
            .send(Command::InjectPrompt {
                terminal_id: dead_id,
                prompt: "rescued prompt".into(),
                fallback_spawn: Some(pilot_ipc::SpawnFallback {
                    session_key: "test:ws-fallback".into(),
                    session_id: None,
                    kind: TerminalKind::Shell,
                    cwd: None,
                }),
            })
            .unwrap();

        let spawned = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            spawned.is_some(),
            "inject_prompt with dead terminal_id should fall back to Spawn"
        );
    })
    .await
    .expect("deadline");
}

/// Mirror of the above, but with no `fallback_spawn`. Pre-fix and
/// post-fix this is a silent no-op — the test exists to lock in
/// that "InjectPrompt + None + dead id" stays a no-op rather than
/// drifting into "auto-resurrect any dead terminal" behavior, which
/// would be very surprising at the API level.
#[tokio::test]
async fn inject_prompt_without_fallback_is_silent_noop() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config).await;
        let dead_id = pilot_ipc::TerminalId(99_999);
        client
            .send(Command::InjectPrompt {
                terminal_id: dead_id,
                prompt: "should disappear".into(),
                fallback_spawn: None,
            })
            .unwrap();

        // A 250ms grace window: any spawn / error event in this
        // window would mean the daemon resurrected something it
        // shouldn't have.
        let unexpected = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::TerminalSpawned { .. } | Event::ProviderError { .. }
                )
            },
            Duration::from_millis(250),
        )
        .await;
        assert!(
            unexpected.is_none(),
            "no event expected for inject_prompt with no fallback, got {unexpected:?}"
        );
    })
    .await
    .expect("deadline");
}

/// Regression: a single wedged backend session must not block the
/// daemon's Subscribe handler. Pre-fix, `snapshot_terminals` would
/// `.await` `backend.snapshot(key)` with no timeout — one stuck tmux
/// pump holding the ring mutex froze every subsequent IPC command
/// (Spawn / Write / MarkRead) because `tokio::select!` cannot pick
/// the next branch until the current arm returns.
///
/// The fix: per-session `tokio::time::timeout` in `snapshot_terminals`.
/// This test wedges one session's snapshot, then asserts that
/// Subscribe completes (under the wedge would otherwise be infinite)
/// and a follow-up Spawn still gets a TerminalSpawned event back.
#[tokio::test]
async fn wedged_session_does_not_block_subscribe_or_subsequent_spawn() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();

        // Spawn one terminal so there's something to snapshot.
        let mut producer = subscribed(config.clone()).await;
        let _ = spawn_and_wait(&mut producer, TerminalKind::Shell).await;

        // Wedge its snapshot — simulates a tmux pump that holds the
        // ring mutex forever.
        let wedged_key = mock.list().await.unwrap().into_iter().next().unwrap();
        mock.wedge_snapshot(&wedged_key).await;

        // A second client subscribes. Without the timeout fix, this
        // hangs in snapshot_terminals → backend.snapshot → forever.
        let mut consumer = run_daemon(config).await;
        consumer.send(Command::Subscribe).unwrap();

        // Subscribe MUST come back within roughly the per-session
        // timeout (500ms) — a 2s budget here gives generous slack
        // for CI without masking a regression to seconds-long stalls.
        let snapshot_evt = timeout(Duration::from_secs(2), consumer.recv())
            .await
            .expect("subscribe completed past timeout — wedge bug returned")
            .expect("not closed");
        let terminals = match snapshot_evt {
            Event::Snapshot { terminals, .. } => terminals,
            other => panic!("expected Snapshot, got {other:?}"),
        };
        // The wedged terminal still shows up — just with empty replay.
        assert_eq!(terminals.len(), 1, "snapshot lists the wedged terminal");
        assert!(
            terminals[0].replay.is_empty(),
            "wedged session degraded to empty replay, not a real one"
        );

        // The real bug symptom: subsequent Spawn never reaches the
        // daemon. Issue one and confirm the daemon processes it end
        // to end — this is what the user pressed `s` for.
        consumer
            .send(Command::Spawn {
                session_key: "test:wedge-followup".into(),
                session_id: None,
                kind: TerminalKind::Shell,
                cwd: None,
                initial_prompt: None,
            })
            .unwrap();
        let spawned = wait_for(
            &mut consumer,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            spawned.is_some(),
            "post-wedge Spawn must reach the daemon and emit TerminalSpawned"
        );
    })
    .await
    .expect("deadline");
}
