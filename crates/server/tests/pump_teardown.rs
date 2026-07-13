//! Regression tests for the output pump's exit teardown and seq-gap
//! recovery:
//!
//! 1. A terminal whose child exits ON ITS OWN releases its backend
//!    session slot (the daemon used to release only via `kill`, so
//!    self-exited agents leaked an open PTY master + writer thread +
//!    replay ring per session, forever).
//! 2. A chunk dropped between the backend bridge and the pump (bounded
//!    channel overflow / broadcast lag) triggers a real resync from the
//!    replay ring instead of forwarding a torn byte stream.
//! 3. `recover_sessions`' pump tears down exactly like the main pump —
//!    hook-era map entries and persisted `terminal:*` kv rows must not
//!    outlive a recovered terminal.

use lazybox_ipc::{Command, Event, TerminalKind, channel};
use lazybox_server::{Server, ServerConfig};
use std::time::Duration;
use tokio::time::timeout;

/// Per-test deadline. Workspace rule: every async test bounds itself
/// so a deadlock is reported as a failure, not a hung suite.
const TEST_DEADLINE: Duration = Duration::from_secs(5);

/// Point `LAZYBOX_HOME` at an empty temp dir for the lifetime of the
/// guard (see tests/spawn_handler.rs for the full rationale): spawn
/// paths read `lazybox_config::Config::load()`, and an empty dir
/// resolves every reader to defaults — what CI exercises.
struct IsolatedConfigHome {
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedConfigHome {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("LAZYBOX_HOME");
        // SAFETY: process-global, but an empty dir resolves every
        // reader to defaults, so a concurrent test sees CI behaviour.
        unsafe { std::env::set_var("LAZYBOX_HOME", tmp.path()) };
        Self { _tmp: tmp, prev }
    }
}

impl Drop for IsolatedConfigHome {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("LAZYBOX_HOME", v) },
            None => unsafe { std::env::remove_var("LAZYBOX_HOME") },
        }
    }
}

/// Drain events until one matches `pred` or the budget lapses.
async fn wait_for<F: FnMut(&Event) -> bool>(
    client: &mut lazybox_ipc::Client,
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

async fn subscribed(config: ServerConfig) -> lazybox_ipc::Client {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        let _ = Server::new(config).serve(server).await;
    });
    client.send(Command::Subscribe).unwrap();
    let _snapshot = client.recv().await.expect("snapshot");
    client
}

async fn spawn_shell(client: &mut lazybox_ipc::Client) -> lazybox_ipc::TerminalId {
    client
        .send(Command::Spawn {
            model_alias: None,
            session_key: "test:ws-teardown".into(),
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
            initial_prompt: None,
            on_main: false,
        })
        .unwrap();
    match wait_for(
        client,
        |e| matches!(e, Event::TerminalSpawned { .. }),
        Duration::from_secs(2),
    )
    .await
    .expect("TerminalSpawned arrived")
    {
        Event::TerminalSpawned { terminal_id, .. } => terminal_id,
        _ => unreachable!(),
    }
}

/// Poll `probe` until it returns true or the budget lapses.
async fn eventually<F, Fut>(mut probe: F, budget: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if probe().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Fix: backend slot leak on self-exit. The pump's exit teardown must
/// release the backend session slot — the mock records the call; the
/// raw-pty backend's own tests assert release actually frees the slot.
#[tokio::test]
async fn self_exiting_terminal_releases_backend_slot() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_shell(&mut client).await;

        let key = config
            .backend_key_for(terminal_id)
            .await
            .expect("terminal registered");
        assert!(
            mock.released_keys().await.is_empty(),
            "no release while the session is alive"
        );

        // Child exits on its own — nobody calls Close/kill.
        mock.finish(&key, 0).await;
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalExited { terminal_id: t, .. } if *t == terminal_id),
            Duration::from_secs(2),
        )
        .await
        .expect("TerminalExited arrived");

        let mock_for_probe = mock.clone();
        let key_probe = key.clone();
        assert!(
            eventually(
                move || {
                    let mock = mock_for_probe.clone();
                    let key = key_probe.clone();
                    async move { mock.released_keys().await.contains(&key) }
                },
                Duration::from_secs(2),
            )
            .await,
            "pump teardown must release the backend session slot on self-exit"
        );
    })
    .await
    .expect("test deadline exceeded");
}

/// Fix: phantom seq-gap detection. A chunk dropped upstream (bounded
/// bridge overflow / broadcast lag) must produce a `TerminalResync`
/// carrying the replay ring — never a silently torn output stream.
#[tokio::test]
async fn seq_gap_resyncs_from_replay_ring() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_shell(&mut client).await;
        let key = config
            .backend_key_for(terminal_id)
            .await
            .expect("terminal registered");

        // Delivered normally.
        mock.emit(&key, b"A").await;
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalOutput { seq: 1, .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("first chunk forwarded");

        // Seq 2 lands in the ring but never reaches the pump — the
        // bounded-bridge drop. Seq 3 is delivered and exposes the gap.
        mock.emit_dropped(&key, b"B").await;
        mock.emit(&key, b"C").await;

        let resync = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalResync { terminal_id: t, .. } if *t == terminal_id),
            Duration::from_secs(2),
        )
        .await
        .expect("gap must trigger a TerminalResync");
        match resync {
            Event::TerminalResync { replay, seq, .. } => {
                assert_eq!(
                    replay,
                    b"ABC".to_vec(),
                    "resync carries the full ring, covering the dropped chunk"
                );
                assert_eq!(seq, 3, "resync covers through the ring's seq");
            }
            _ => unreachable!(),
        }

        // The stream resumes cleanly after the resync: the next chunk
        // is forwarded as plain output with the next seq.
        mock.emit(&key, b"D").await;
        let next = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalOutput { terminal_id: t, .. } if *t == terminal_id),
            Duration::from_secs(2),
        )
        .await
        .expect("stream resumes after resync");
        match next {
            Event::TerminalOutput { bytes, seq, .. } => {
                assert_eq!(bytes, b"D".to_vec());
                assert_eq!(
                    seq, 4,
                    "no post-resync chunk may re-deliver bytes the resync covered"
                );
            }
            _ => unreachable!(),
        }
    })
    .await
    .expect("test deadline exceeded");
}

/// Fix: recover_sessions' simplified pump leaked. A recovered terminal
/// must tear down exactly like a freshly spawned one: hook-era map
/// entries (agent_states, hook_driven_terminals, input_needed_shapes,
/// prompt_submit_signals) removed, the persisted `terminal:*` /
/// `terminal-noperm:*` / `terminal-msg:*` kv rows deleted, and the
/// backend slot released.
#[tokio::test]
async fn recovered_session_teardown_matches_main_pump() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();

        // A backend session that "survived a restart", with the kv rows
        // the previous daemon persisted for it.
        let key = config
            .backend
            .spawn(&["claude".into()], None, &[], "survivor")
            .await
            .expect("backend spawn");
        let meta = serde_json::to_string(&("test:ws-recovered", TerminalKind::Shell)).unwrap();
        config
            .store
            .set_kv(&format!("terminal:{key}"), &meta)
            .unwrap();
        config
            .store
            .set_kv(&format!("terminal-noperm:{key}"), "1")
            .unwrap();
        config
            .store
            .set_kv(&format!("terminal-msg:{key}"), "resume the rebase")
            .unwrap();

        let mut bus_rx = config.bus.subscribe();
        lazybox_server::spawn_handler::recover_sessions(&config).await;

        // The recovery broadcast tells us the fresh wire id.
        let terminal_id = loop {
            match timeout(Duration::from_secs(2), bus_rx.recv())
                .await
                .expect("bus event within deadline")
                .expect("bus open")
            {
                Event::TerminalSpawned { terminal_id, .. } => break terminal_id,
                _ => continue,
            }
        };

        // Entries a live recovered agent accrues after recovery (hooks,
        // detection, prompt-inject bookkeeping). The old teardown swept
        // none of these.
        config
            .agent_states
            .lock()
            .await
            .insert(terminal_id, lazybox_ipc::AgentState::Working);
        config
            .hook_driven_terminals
            .lock()
            .await
            .insert(terminal_id, std::time::Instant::now());
        config
            .input_needed_shapes
            .lock()
            .await
            .insert(terminal_id, lazybox_agents::PromptShape::Chooser);
        config
            .prompt_submit_signals
            .lock()
            .await
            .insert(terminal_id, std::sync::Arc::new(tokio::sync::Notify::new()));

        // The survivor exits on its own.
        mock.finish(&key, 0).await;
        loop {
            match timeout(Duration::from_secs(2), bus_rx.recv())
                .await
                .expect("bus event within deadline")
                .expect("bus open")
            {
                Event::TerminalExited { terminal_id: t, .. } if t == terminal_id => break,
                _ => continue,
            }
        }

        // Teardown runs after the exit broadcast; poll for convergence.
        let config_probe = config.clone();
        let converged = eventually(
            move || {
                let config = config_probe.clone();
                async move {
                    config.agent_states.lock().await.is_empty()
                        && config.hook_driven_terminals.lock().await.is_empty()
                        && config.input_needed_shapes.lock().await.is_empty()
                        && config.prompt_submit_signals.lock().await.is_empty()
                        && config.terminals.lock().await.is_empty()
                        && config.terminal_meta.lock().await.is_empty()
                        && config.no_permission_terminals.lock().await.is_empty()
                }
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            converged,
            "every per-terminal map entry must be swept for a recovered terminal"
        );

        // The kv rows the previous daemon persisted are deleted — this
        // was the unbounded state.db growth: recovered sessions never
        // cleaned them.
        assert_eq!(
            config.store.get_kv(&format!("terminal:{key}")).unwrap(),
            None
        );
        assert_eq!(
            config
                .store
                .get_kv(&format!("terminal-noperm:{key}"))
                .unwrap(),
            None
        );
        assert_eq!(
            config.store.get_kv(&format!("terminal-msg:{key}")).unwrap(),
            None
        );
        // And the backend slot is released, same as the main pump.
        assert!(mock.released_keys().await.contains(&key));
    })
    .await
    .expect("test deadline exceeded");
}
