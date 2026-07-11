//! End-to-end tests for the daemon's Spawn → backend → bus pipeline.
//!
//! Backend is the in-memory [`MockBackend`] — no real shells / tmux /
//! curl. Tests drive synthetic output via `MockBackend::emit` and end
//! sessions via `finish`.

use lazybox_ipc::{Command, Event, TerminalKind, channel};
use lazybox_server::backend::{MockBackend, SessionBackend};
use lazybox_server::{Server, ServerConfig};
use lazybox_store::MemoryStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Per-test deadline. Workspace rule: every async test bounds itself
/// so a deadlock is reported as a failure, not a hung suite.
const TEST_DEADLINE: Duration = Duration::from_secs(5);

/// Point `LAZYBOX_HOME` at an empty temp dir for the lifetime of the
/// guard, restoring the previous value on drop. `handle_spawn` calls
/// `lazybox_config::Config::load()`, which resolves `~/.lazybox/config.yaml`
/// via `LAZYBOX_HOME`; without this a test that asserts on a config-driven
/// field (e.g. `skip_permissions`) reads the dev machine's real config
/// and flakes. An empty dir → `Config::default()`, which is exactly what
/// CI (no config file) exercises.
struct IsolatedConfigHome {
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedConfigHome {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("LAZYBOX_HOME");
        // SAFETY: LAZYBOX_HOME is process-global, but within this test
        // binary only this guard sets it, and an empty dir resolves every
        // reader to defaults (CI's behaviour) — so even if it leaks to a
        // concurrent test, that test sees what it'd see in CI. Restored
        // on drop.
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

/// Drain events until we see one matching `pred` or hit the deadline.
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

async fn run_daemon(config: ServerConfig) -> lazybox_ipc::Client {
    let (client, server) = channel::pair();
    tokio::spawn(async move {
        let _ = Server::new(config).serve(server).await;
    });
    client
}

async fn subscribed(config: ServerConfig) -> lazybox_ipc::Client {
    let mut client = run_daemon(config).await;
    client.send(Command::Subscribe).unwrap();
    let _snapshot = client.recv().await.expect("snapshot");
    client
}

async fn spawn_and_wait(
    client: &mut lazybox_ipc::Client,
    kind: TerminalKind,
) -> lazybox_ipc::TerminalId {
    client
        .send(Command::Spawn {
            model_alias: None,
            session_key: "test:ws-1".into(),
            session_id: None,
            kind,
            cwd: None,
            initial_prompt: None,
            on_main: false,
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

/// Build a normalized hook event for a Claude lifecycle event name,
/// mirroring what `lazybox hook-ingest` forwards after parsing Claude's
/// stdin payload.
fn hook(kind: lazybox_ipc::HookEventKind) -> lazybox_ipc::HookEvent {
    lazybox_ipc::HookEvent {
        kind,
        session_id: Some("claude-session".into()),
        cwd: None,
        tool_name: None,
        notification: None,
    }
}

/// Structured hooks drive Claude's state deterministically through the
/// full daemon dispatch (`Command::IngestHook` → `handle_ingest_hook` →
/// `Event::AgentState`), with no PTY output involved at all.
/// Correlation is by the stable backend key the daemon baked into the
/// hook command; the wire `terminal_id` is the legacy field and is not
/// trusted.
#[tokio::test]
async fn ingest_hook_drives_agent_state_transitions() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let _ = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();
        // Deliberately bogus legacy id: resolution must come from the
        // backend key alone.
        let terminal_id = lazybox_ipc::TerminalId(0);

        // PreToolUse → Working.
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: hook(lazybox_ipc::HookEventKind::PreToolUse),
                backend_key: Some(key.clone()),
            })
            .unwrap();
        let ev = wait_for(
            &mut client,
            |e| matches!(e, Event::AgentState { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("PreToolUse hook must emit AgentState");
        assert!(matches!(
            ev,
            Event::AgentState {
                state: lazybox_ipc::AgentState::Working,
                ..
            }
        ));

        // A permission Notification → InputNeeded.
        let mut needs_input = hook(lazybox_ipc::HookEventKind::Notification);
        needs_input.notification = Some("permission_prompt".into());
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: needs_input,
                backend_key: Some(key.clone()),
            })
            .unwrap();
        let ev = wait_for(
            &mut client,
            |e| matches!(e, Event::AgentState { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("permission Notification must emit AgentState");
        assert!(matches!(
            ev,
            Event::AgentState {
                state: lazybox_ipc::AgentState::InputNeeded,
                ..
            }
        ));

        // Stop → Done (the turn finished — the "completed, take a
        // look" alert, distinct from a fresh composer that never ran).
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: hook(lazybox_ipc::HookEventKind::Stop),
                backend_key: Some(key.clone()),
            })
            .unwrap();
        let ev = wait_for(
            &mut client,
            |e| matches!(e, Event::AgentState { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("Stop hook must emit AgentState");
        assert!(matches!(
            ev,
            Event::AgentState {
                state: lazybox_ipc::AgentState::Done,
                ..
            }
        ));
    })
    .await
    .expect("deadline");
}

/// Hooks own the Working↔Idle distinction, but the PTY may still RAISE
/// `InputNeeded` on a hook-driven terminal: an inline mid-turn approval
/// fires no hook, so the on-screen `Esc to cancel` dialog is the only
/// source of truth. The PTY reading must surface even after a hook marked
/// the terminal hook-driven (the policy the user approved over the older
/// "hooks fully authoritative" design).
// Paused time: the PTY `?` only surfaces after the ~5s quiet window
// (screen-scrape classification is quiet-gated, #289), so the test
// rides tokio's auto-advance instead of sleeping for real.
#[tokio::test(start_paused = true)]
async fn hook_driven_terminal_honors_pty_input_needed() {
    timeout(Duration::from_secs(60), async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        // A hook marks the terminal hook-driven and sets Working.
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: hook(lazybox_ipc::HookEventKind::PreToolUse),
                backend_key: Some(key.clone()),
            })
            .unwrap();
        let working = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(working.is_some(), "hook must set Working");

        // Now the PTY paints a permission chooser. The hook-driven
        // terminal must honor it — the inline approval fired no hook, so
        // the screen is the only signal that the agent is blocked. The
        // `?` surfaces once the PTY has been quiet past the classify
        // window (a dialog freezes all output).
        mock.emit(
            &key,
            concat!(
                "Do you want to create MEMORY.md?\n",
                "❯ 1. Yes\n",
                "  2. No\n",
                "Esc to cancel",
            ),
        )
        .await;
        let raised = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::InputNeeded,
                        ..
                    }
                )
            },
            Duration::from_secs(10),
        )
        .await;
        assert!(
            raised.is_some(),
            "PTY permission dialog must raise InputNeeded on a hook-driven terminal"
        );
    })
    .await
    .expect("deadline");
}

/// The other half of the policy: hooks still OWN Working↔Idle. A PTY
/// `Working` status line must NOT override a hook-derived `InputNeeded`,
/// or the status-bar ticker would flap a hook-driven terminal's pill.
/// Only `InputNeeded` and a confident ready-idle are honored from the PTY.
#[tokio::test]
async fn hook_driven_terminal_ignores_pty_working() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        // A permission Notification marks the terminal hook-driven and
        // sets InputNeeded.
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: lazybox_ipc::HookEvent {
                    kind: lazybox_ipc::HookEventKind::Notification,
                    session_id: Some("claude-session".into()),
                    cwd: None,
                    tool_name: None,
                    notification: Some("Claude needs your permission to use Bash".into()),
                },
                backend_key: Some(key.clone()),
            })
            .unwrap();
        let asked = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::InputNeeded,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(asked.is_some(), "permission hook must set InputNeeded");

        // The PTY now paints a live working status line. It must NOT flip
        // the hook-driven terminal to Working — hooks own that edge.
        mock.emit(&key, "✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)")
            .await;
        let leaked = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                )
            },
            Duration::from_millis(500),
        )
        .await;
        assert!(
            leaked.is_none(),
            "PTY working status must NOT override a hook-driven terminal"
        );
    })
    .await
    .expect("deadline");
}

/// A hook whose backend key resolves to no live terminal must be
/// dropped — no state transition, no hook-driven marking. This is the
/// restart-survivor case: a tmux session whose owning workspace was
/// killed keeps firing hooks until the agent exits.
#[tokio::test]
async fn hook_with_unknown_backend_key_is_dropped() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;

        client
            .send(Command::IngestHook {
                terminal_id,
                hook: hook(lazybox_ipc::HookEventKind::PreToolUse),
                backend_key: Some("lazybox-no-such-session-1-1".into()),
            })
            .unwrap();

        let leaked = wait_for(
            &mut client,
            |e| matches!(e, Event::AgentState { .. }),
            Duration::from_millis(300),
        )
        .await;
        assert!(
            leaked.is_none(),
            "unknown backend key must not produce a state transition"
        );
        assert!(
            config.hook_driven_terminals.lock().await.is_empty(),
            "unknown backend key must not mark any terminal hook-driven"
        );
    })
    .await
    .expect("deadline");
}

/// A hook carrying only the legacy `--terminal` id (a settings file
/// written before backend-key correlation) must be dropped even when
/// that id names a live terminal: after a daemon restart the id very
/// likely belongs to a DIFFERENT terminal than the one the surviving
/// agent was spawned as — accepting it is cross-terminal corruption.
#[tokio::test]
async fn legacy_terminal_id_only_hook_is_dropped() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;

        client
            .send(Command::IngestHook {
                terminal_id,
                hook: hook(lazybox_ipc::HookEventKind::PreToolUse),
                backend_key: None,
            })
            .unwrap();

        let leaked = wait_for(
            &mut client,
            |e| matches!(e, Event::AgentState { .. }),
            Duration::from_millis(300),
        )
        .await;
        assert!(
            leaked.is_none(),
            "legacy terminal-id-only hooks must be dropped, not trusted"
        );
        assert!(
            config.hook_driven_terminals.lock().await.is_empty(),
            "a dropped legacy hook must not mark the terminal hook-driven"
        );
    })
    .await
    .expect("deadline");
}

/// Heartbeat staleness: a hook-driven terminal whose hooks stopped
/// flowing must degrade back to PTY scraping instead of freezing on
/// the last hook state. A PTY `Working` reading is suppressed while
/// hooks are fresh (pinned by `hook_driven_terminal_ignores_pty_working`)
/// and honored once the last hook is stale — PROVIDED the screen shows
/// the dialog was actually answered: dialog markers with the live
/// working anchor painted after them. (A dialog blocks the hook stream,
/// so "stale hooks + `?`" is the normal shape of a real prompt; see the
/// companion test below for the no-evidence case.)
#[tokio::test]
async fn stale_hooks_degrade_to_pty_detection() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        // Hook-driven, blocked on a permission prompt.
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: lazybox_ipc::HookEvent {
                    kind: lazybox_ipc::HookEventKind::Notification,
                    session_id: Some("claude-session".into()),
                    cwd: None,
                    tool_name: None,
                    notification: Some("Claude needs your permission to use Bash".into()),
                },
                backend_key: Some(key.clone()),
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::InputNeeded,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await
        .expect("permission hook must set InputNeeded");

        // The PTY paints the dialog itself (so the detect buffer holds
        // its markers), then the user answers out-of-band and Claude
        // starts streaming — the working anchor lands AFTER the dialog.
        mock.emit(
            &key,
            concat!(
                "Do you want to proceed?\n",
                "❯ 1. Yes\n",
                "  2. No\n",
                "Esc to cancel",
            ),
        )
        .await;

        // Backdate the last-hook timestamp past the staleness window —
        // equivalent to 31s of hook silence without sleeping for it.
        let stale = std::time::Instant::now() - Duration::from_secs(31);
        config
            .hook_driven_terminals
            .lock()
            .await
            .insert(terminal_id, stale);

        // A later chunk paints the live working status line below the
        // (answered) dialog. With stale hooks AND the on-screen
        // evidence, the reading must pass the gate and flip the state.
        mock.emit(&key, "✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)")
            .await;
        let degraded = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            degraded.is_some(),
            "stale hooks must hand Working detection back to the PTY"
        );
    })
    .await
    .expect("deadline");
}

/// The other half of the stale-hook rule: a dialog on screen BLOCKS the
/// hook stream, so a hook-set `InputNeeded` going stale is the normal
/// shape of a real unanswered prompt — a bare `Working` status line
/// (e.g. a full repaint with no dialog markers in the detect buffer)
/// must NOT clear the `?`.
#[tokio::test]
async fn stale_hooks_do_not_demote_input_needed_without_dialog_evidence() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        client
            .send(Command::IngestHook {
                terminal_id,
                hook: lazybox_ipc::HookEvent {
                    kind: lazybox_ipc::HookEventKind::Notification,
                    session_id: Some("claude-session".into()),
                    cwd: None,
                    tool_name: None,
                    notification: Some("Claude needs your permission to use Bash".into()),
                },
                backend_key: Some(key.clone()),
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::InputNeeded,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await
        .expect("permission hook must set InputNeeded");

        let stale = std::time::Instant::now() - Duration::from_secs(31);
        config
            .hook_driven_terminals
            .lock()
            .await
            .insert(terminal_id, stale);

        // Bare working line, no dialog markers anywhere in the buffer —
        // no proof the dialog was answered.
        mock.emit(&key, "✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)")
            .await;
        let demoted = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                )
            },
            Duration::from_millis(500),
        )
        .await;
        assert!(
            demoted.is_none(),
            "a bare Working reading must not clear a hook-set `?` after staleness"
        );
        assert_eq!(
            config.agent_state_for(terminal_id).await,
            Some(lazybox_ipc::AgentState::InputNeeded),
            "cached state must stay InputNeeded"
        );
    })
    .await
    .expect("deadline");
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
/// Acceptance: user-initiated interactive sessions still get prompts.
/// A `Command::Spawn` is always interactive, so the spawned claude must
/// NOT carry `--dangerously-skip-permissions` and the spawn event must
/// report `no_permission: false`, regardless of the autonomous toggle.
#[tokio::test]
async fn interactive_claude_spawn_keeps_permission_prompts() {
    timeout(TEST_DEADLINE, async {
        // Resolve config to defaults regardless of the dev machine's real
        // `~/.lazybox/config.yaml`, whose `skip_permissions` toggle would
        // otherwise flip `no_permission` and flake this assertion.
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        client
            .send(Command::Spawn {
                model_alias: None,
                session_key: "test:ws-1".into(),
                session_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        let spawned = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("TerminalSpawned arrived");
        match spawned {
            Event::TerminalSpawned { no_permission, .. } => {
                assert!(!no_permission, "interactive sessions keep prompts on");
            }
            _ => unreachable!(),
        }
        let key = mock.list().await.unwrap().into_iter().next().unwrap();
        let argv = mock.argv_for(&key).await.unwrap();
        assert_eq!(argv.first().map(String::as_str), Some("claude"));
        assert!(
            !argv.iter().any(|a| a == "--dangerously-skip-permissions"),
            "interactive claude must not get the bypass flag: {argv:?}",
        );
        // Claude is launched with a lazybox-generated hooks settings file so
        // it reports state through structured lifecycle hooks.
        assert!(
            argv.iter().any(|a| a == "--settings"),
            "claude must launch with --settings for hook injection: {argv:?}",
        );
    })
    .await
    .expect("deadline");
}

/// Acceptance: an autonomous spawn propagates its no-permission decision
/// consistently across the spawned argv, the `TerminalSpawned` event,
/// and the reconnection snapshot. The decision value itself (on by
/// default) is pinned by the `skip_permissions_for` unit test; here we
/// pin that whatever the daemon decided reaches all three surfaces in
/// lockstep — so the UI badge can never disagree with the real argv.
#[tokio::test]
async fn autonomous_spawn_wires_no_permission_consistently() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut bus_rx = config.bus.subscribe();
        let cwd = std::env::temp_dir().to_string_lossy().to_string();
        lazybox_server::spawn_handler::handle_spawn(
            &config,
            "test:ws-auto".into(),
            None,
            TerminalKind::Agent("claude".into()),
            Some(cwd),
            None,
            true,  // autonomous
            false, // on_main
            None,  // model_alias
        )
        .await;

        let mut event_flag = None;
        while let Ok(ev) = bus_rx.try_recv() {
            if let Event::TerminalSpawned { no_permission, .. } = ev {
                event_flag = Some(no_permission);
                break;
            }
        }
        let event_flag = event_flag.expect("TerminalSpawned broadcast");

        let key = mock.list().await.unwrap().into_iter().next().unwrap();
        let argv_flag = mock
            .argv_for(&key)
            .await
            .unwrap()
            .iter()
            .any(|a| a == "--dangerously-skip-permissions");

        let snapshot_flag = lazybox_server::spawn_handler::snapshot_terminals(&config)
            .await
            .into_iter()
            .next()
            .expect("one snapshot")
            .no_permission;

        assert_eq!(
            event_flag, argv_flag,
            "spawn event's no_permission must match the actual claude argv",
        );
        assert_eq!(
            snapshot_flag, argv_flag,
            "reconnection snapshot must match the actual claude argv",
        );
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
                model_alias: None,
                session_key: "test:ws-1".into(),
                session_id: None,
                kind: TerminalKind::Agent("does-not-exist".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
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
        mock.emit(&key, b"lazybox-marker").await;

        let evt = wait_for(
            &mut client,
            |e| match e {
                Event::TerminalOutput {
                    terminal_id: tid,
                    bytes,
                    ..
                } => *tid == terminal_id && bytes == b"lazybox-marker",
                _ => false,
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            evt.is_some(),
            "expected to see 'lazybox-marker' in TerminalOutput"
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
        mock.emit(&key, b"lazybox-replay-marker").await;
        let _ = wait_for(
            &mut producer,
            |e| match e {
                Event::TerminalOutput { bytes, .. } => bytes == b"lazybox-replay-marker",
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
                    term.replay, b"lazybox-replay-marker",
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
/// "lazybox crashed"), then a fresh `ServerConfig` is built around the
/// same backend (simulating "lazybox restarted"). `recover_sessions`
/// should register the survivor on the new config so the TUI sees it.
#[tokio::test]
async fn recover_sessions_reattaches_survivors() {
    timeout(TEST_DEADLINE, async {
        let backend = MockBackend::new();
        // Pre-existing session — simulates one that survived the
        // previous lazybox run. Spawned directly through the backend,
        // not through spawn_handler, so it's known to the backend
        // but not to any ServerConfig.
        let preexisting = backend
            .spawn(&["echo".into(), "hello".into()], None, &[], "preexisting")
            .await
            .unwrap();

        // Fresh config pointing at the SAME backend instance.
        let store: Arc<dyn lazybox_store::Store> = Arc::new(MemoryStore::new());
        let backend_arc: Arc<dyn SessionBackend> = Arc::new(backend.clone());
        let config = ServerConfig::with_store_and_backend(store, backend_arc);
        assert!(config.terminals.lock().await.is_empty());

        // Listen on the bus before recovery so TerminalSpawned isn't lost.
        let mut bus = config.bus.subscribe();

        lazybox_server::spawn_handler::recover_sessions(&config).await;

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
/// pressing `w` or by the `@lazybox`-mention auto-spawn — the agent is
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
                model_alias: None,
                session_key: "test:ws-ingest".into(),
                session_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: Some(WORK.into()),
                on_main: false,
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

/// Regression: a prompt-carrying Spawn that collapses onto an existing
/// singleton must still deliver its prompt. Pre-fix, `handle_spawn`
/// only broadcast `TerminalFocusRequested` and the `w`-built work
/// instruction was silently discarded — the user got focused onto an
/// idle agent that never learned what to do.
#[tokio::test]
async fn spawn_onto_existing_singleton_injects_the_prompt() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let _ = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        const WORK: &str = "Address the review comments on PR #9.";
        client
            .send(Command::Spawn {
                model_alias: None,
                session_key: "test:ws-1".into(),
                session_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: Some(WORK.into()),
                on_main: false,
            })
            .unwrap();

        // The duplicate collapses onto the live terminal — focus is
        // requested instead of spawning a second agent.
        let focused = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalFocusRequested { .. }),
            Duration::from_secs(2),
        )
        .await;
        assert!(focused.is_some(), "duplicate spawn must request focus");
        assert_eq!(
            mock.list().await.unwrap().len(),
            1,
            "singleton guard must not spawn a second agent"
        );

        // ...and the prompt still reaches the existing agent's PTY,
        // paste + separate submit.
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
            "existing singleton never received the work prompt; writes = {text:?}"
        );
        assert!(
            joined.contains(&b'\r'),
            "prompt was pasted into the singleton but never submitted; writes = {text:?}"
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
        let dead_id = lazybox_ipc::TerminalId(99_999);
        client
            .send(Command::InjectPrompt {
                terminal_id: dead_id,
                prompt: "rescued prompt".into(),
                fallback_spawn: Some(lazybox_ipc::SpawnFallback {
                    model_alias: None,
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
        let dead_id = lazybox_ipc::TerminalId(99_999);
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

/// Regression (issue #32): pressing `w` (inject work context) while a
/// Claude permission prompt is up must NOT write the paste into the
/// dialog — the dialog expects `y`/`n`/`1`/`2` and would reject it,
/// silently losing the injection. The daemon queues the injection and
/// flushes it only once the agent leaves `InputNeeded`.
#[tokio::test]
async fn inject_prompt_waits_for_input_needed_to_clear() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        // Drive the agent into InputNeeded via a permission hook — the
        // same state a live "Do you want to proceed?" dialog produces.
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: lazybox_ipc::HookEvent {
                    kind: lazybox_ipc::HookEventKind::Notification,
                    session_id: Some("claude-session".into()),
                    cwd: None,
                    tool_name: None,
                    notification: Some("Claude needs your permission to use Bash".into()),
                },
                backend_key: Some(key.clone()),
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::InputNeeded,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await
        .expect("permission hook must raise InputNeeded");

        const WORK: &str = "CI is red on PR #7; here are the failing checks.";
        client
            .send(Command::InjectPrompt {
                terminal_id,
                prompt: WORK.into(),
                fallback_spawn: None,
            })
            .unwrap();

        // While the permission prompt owns input, nothing may reach the
        // PTY — feeding the paste into the dialog is the bug.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let early = mock
            .writes_for(&key)
            .await
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();
        assert!(
            !String::from_utf8_lossy(&early).contains(WORK),
            "injection was written into the live permission prompt; writes = {:?}",
            String::from_utf8_lossy(&early)
        );

        // Resolve the prompt: a PreToolUse hook flips the agent to
        // Working (the user approved the tool). The queued injection
        // must now flush in full.
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: lazybox_ipc::HookEvent {
                    kind: lazybox_ipc::HookEventKind::PreToolUse,
                    session_id: Some("claude-session".into()),
                    cwd: None,
                    tool_name: None,
                    notification: None,
                },
                backend_key: Some(key.clone()),
            })
            .unwrap();

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
            "context never delivered after the prompt cleared; writes = {text:?}"
        );
        assert!(
            joined.contains(&b'\r'),
            "context was pasted but never submitted (no Enter); writes = {text:?}"
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
                model_alias: None,
                session_key: "test:wedge-followup".into(),
                session_id: None,
                kind: TerminalKind::Shell,
                cwd: None,
                initial_prompt: None,
                on_main: false,
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

/// Regression for issue #101: after the user answers a prompt, the
/// `?` ("input-needed") pill must CLEAR and stay cleared — it must not
/// snap back the instant the next output chunk arrives.
///
/// Symptom pre-fix: the daemon flips InputNeeded → Working when the
/// user presses Enter, but the just-answered prompt's markers (`❯`,
/// the numbered options, `Esc to cancel`) still sit in the rolling
/// detection buffer. The very next chunk re-runs the detector over
/// that stale text, re-detects InputNeeded, and broadcasts it again —
/// so the pill reappears and never clears until ~16 KiB of fresh
/// output finally evicts the prompt. The user "can't tell which
/// session needs me" because every just-answered session keeps the `?`.
///
/// The fix drops the detection buffer when the user submits an answer
/// (see `ServerConfig::agent_detect_resets`), so detection restarts
/// from post-answer output. This test drives the full live path:
/// prompt → InputNeeded, answer → Working, then a small follow-up
/// chunk with no prompt markers — and asserts the state lands on Idle,
/// never bouncing back to InputNeeded.
// Paused time: the PTY `?` only surfaces after the ~5s quiet window
// (screen-scrape classification is quiet-gated, #289), so the test
// rides tokio's auto-advance instead of sleeping for real.
#[tokio::test(start_paused = true)]
async fn answering_a_prompt_clears_input_needed_and_does_not_bounce_back() {
    timeout(Duration::from_secs(60), async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;

        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        // 1. A permission chooser shows up and the PTY goes quiet (a
        //    dialog blocks all output) → the quiet classifier flags
        //    InputNeeded.
        mock.emit(
            &key,
            concat!(
                "Do you want to create MEMORY.md?\n",
                "❯ 1. Yes\n",
                "  2. No\n",
                "Esc to cancel",
            ),
        )
        .await;
        let asked = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::InputNeeded,
                        ..
                    }
                )
            },
            Duration::from_secs(10),
        )
        .await;
        assert!(
            asked.is_some(),
            "permission chooser must be detected as InputNeeded"
        );

        // 2. User answers (select option 1, Enter). The optimistic flip
        //    clears the pill immediately → Working.
        client
            .send(Command::Write {
                terminal_id,
                bytes: b"1\r".to_vec(),
            })
            .unwrap();
        let working = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            working.is_some(),
            "answering must optimistically flip InputNeeded → Working"
        );

        // 3. Claude acts on the answer, emits a small, prompt-free
        //    follow-up, and goes quiet. Pre-fix this chunk re-detected
        //    the STALE chooser still in the buffer and bounced back to
        //    InputNeeded.
        mock.emit(&key, "Created the file.\nAll done.").await;

        // The next state transition (the quiet classification of the
        // resting screen) must be Idle, NOT InputNeeded — the pill is
        // gone and stays gone.
        let next = wait_for(
            &mut client,
            |e| matches!(e, Event::AgentState { .. }),
            Duration::from_secs(10),
        )
        .await
        .expect("a follow-up AgentState transition must arrive");
        match next {
            Event::AgentState { state, .. } => assert_eq!(
                state,
                lazybox_ipc::AgentState::Idle,
                "after answering, the prompt-free follow-up must settle to Idle, \
                 not bounce back to InputNeeded (the #101 stale-buffer regression)"
            ),
            _ => unreachable!(),
        }
    })
    .await
    .expect("deadline");
}

/// Regression for the double-spawn race: `handle_spawn` is detached
/// onto its own task, and its duplicate check reads maps populated only
/// AFTER worktree provisioning + `backend.spawn` — with a slow backend,
/// two concurrent spawns for the same (workspace, agent) both passed it
/// and launched two skip-permissions agents into one worktree. The
/// in-flight guard collapses the loser onto the winner: exactly one
/// backend session, the loser requests focus, and its work prompt is
/// injected into the winner's terminal instead of being dropped.
#[tokio::test]
async fn concurrent_spawns_collapse_onto_one_backend_session() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        // Hold the first spawn "mid-provision" long enough for the
        // duplicate to land inside the window.
        mock.set_spawn_delay(Duration::from_millis(300)).await;
        let mut bus = config.bus.subscribe();
        let cwd = std::env::temp_dir().to_string_lossy().to_string();

        const WORK: &str = "Address the review comments on PR #5.";
        let cfg_a = config.clone();
        let cwd_a = cwd.clone();
        let first = tokio::spawn(async move {
            lazybox_server::spawn_handler::handle_spawn(
                &cfg_a,
                "test:ws-race".into(),
                None,
                TerminalKind::Agent("claude".into()),
                Some(cwd_a),
                None,
                false,
                false, // on_main
                None,  // model_alias
            )
            .await;
        });
        // A beat so the first spawn claims the guard before the
        // duplicate arrives (the claim is synchronous at entry; the
        // backend spawn is still parked on the delay).
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cfg_b = config.clone();
        let second = tokio::spawn(async move {
            lazybox_server::spawn_handler::handle_spawn(
                &cfg_b,
                "test:ws-race".into(),
                None,
                TerminalKind::Agent("claude".into()),
                Some(cwd),
                Some(WORK.into()),
                true,
                false, // on_main
                None,  // model_alias
            )
            .await;
        });
        first.await.unwrap();
        second.await.unwrap();

        // Exactly one backend session despite two concurrent spawns.
        let keys = mock.list().await.unwrap();
        assert_eq!(
            keys.len(),
            1,
            "double-spawn race produced {} sessions",
            keys.len()
        );
        let key = keys.into_iter().next().unwrap();

        // The loser collapsed: focus requested on the winner's terminal.
        let mut saw_focus = false;
        while let Ok(ev) = bus.try_recv() {
            if matches!(ev, Event::TerminalFocusRequested { .. }) {
                saw_focus = true;
            }
        }
        assert!(saw_focus, "the duplicate must request focus, not vanish");

        // ...and its prompt reaches the one live terminal (paste +
        // separate Enter, like every inject).
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
            "the loser's work prompt was dropped; writes = {text:?}"
        );
        assert!(joined.contains(&b'\r'), "prompt pasted but never submitted");
    })
    .await
    .expect("deadline");
}

/// A spawn whose workspace row vanished because the workspace was
/// DELETED while the spawn was in flight (Kill racing a slow provision)
/// must abort — the old fallback silently launched the agent in the
/// daemon's own cwd. A key that never existed keeps the fallback
/// (pinned by `spawn_shell_emits_terminal_spawned_event`, which spawns
/// against an unpersisted workspace).
#[tokio::test]
async fn spawn_aborts_when_workspace_was_deleted_mid_flight() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        config
            .deleted_workspaces
            .lock()
            .unwrap()
            .insert("test:ws-deleted".to_string());
        let mut bus = config.bus.subscribe();

        lazybox_server::spawn_handler::handle_spawn(
            &config,
            "test:ws-deleted".into(),
            None,
            TerminalKind::Agent("claude".into()),
            None, // no cwd override → goes through workspace resolution
            None,
            false,
            false, // on_main
            None,  // model_alias
        )
        .await;

        assert!(
            mock.list().await.unwrap().is_empty(),
            "spawn must abort, not fall back to the daemon cwd"
        );
        let mut saw_error = false;
        let mut saw_spawned = false;
        while let Ok(ev) = bus.try_recv() {
            match ev {
                Event::ProviderError { .. } => saw_error = true,
                Event::TerminalSpawned { .. } => saw_spawned = true,
                _ => {}
            }
        }
        assert!(saw_error, "aborted spawn must surface a provider error");
        assert!(!saw_spawned, "no terminal may be announced");
    })
    .await
    .expect("deadline");
}

/// Bare-key optimistic flip is restricted to chooser-shaped prompts: a
/// free-text elicitation (hook-raised) collects typed text, so a lone
/// digit is just typing — it must NOT clear the `?`. Enter still flips
/// (it submits the elicitation answer). The chooser shape's bare-key
/// flip is pinned by `bare_chooser_keystroke_clears_input_needed`.
#[tokio::test]
async fn bare_keystroke_does_not_clear_free_text_elicitation() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        // Elicitation dialog → InputNeeded with a free-text shape.
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: lazybox_ipc::HookEvent {
                    kind: lazybox_ipc::HookEventKind::Notification,
                    session_id: Some("claude-session".into()),
                    cwd: None,
                    tool_name: None,
                    notification: Some("elicitation_dialog".into()),
                },
                backend_key: Some(key.clone()),
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::InputNeeded,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await
        .expect("elicitation hook must set InputNeeded");

        // A bare digit is typing into the field — no flip.
        client
            .send(Command::Write {
                terminal_id,
                bytes: b"1".to_vec(),
            })
            .unwrap();
        let flipped = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                )
            },
            Duration::from_millis(400),
        )
        .await;
        assert!(
            flipped.is_none(),
            "a bare digit typed into a free-text elicitation must not clear the `?`"
        );

        // Enter SUBMITS the elicitation answer — the flip is correct.
        client
            .send(Command::Write {
                terminal_id,
                bytes: b"\r".to_vec(),
            })
            .unwrap();
        let submitted = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(submitted.is_some(), "Enter must still flip to Working");
    })
    .await
    .expect("deadline");
}

/// Companion regression to the above: Claude's choosers accept a BARE
/// digit (1-9), y/n, or Esc — no Enter at all. Pre-fix the optimistic
/// InputNeeded → Working flip (and the detect-buffer reset behind it)
/// only fired on `\r`/`\n`, so answering a chooser with `1` or
/// dismissing it with Esc left the `?` pill pinned until ~16 KiB of
/// fresh output evicted the stale prompt markers.
// Paused time: the PTY `?` only surfaces after the ~5s quiet window
// (screen-scrape classification is quiet-gated, #289), so the test
// rides tokio's auto-advance instead of sleeping for real.
#[tokio::test(start_paused = true)]
async fn bare_chooser_keystroke_clears_input_needed() {
    timeout(Duration::from_secs(60), async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;

        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        let chooser = concat!(
            "Do you want to create MEMORY.md?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel",
        );
        let input_needed = |e: &Event| {
            matches!(
                e,
                Event::AgentState {
                    state: lazybox_ipc::AgentState::InputNeeded,
                    ..
                }
            )
        };
        let working = |e: &Event| {
            matches!(
                e,
                Event::AgentState {
                    state: lazybox_ipc::AgentState::Working,
                    ..
                }
            )
        };

        // Chooser up → InputNeeded; the user picks option 1 with a bare
        // digit (no Enter) → optimistic flip to Working.
        mock.emit(&key, chooser).await;
        assert!(
            wait_for(&mut client, input_needed, Duration::from_secs(10))
                .await
                .is_some(),
            "chooser must be detected as InputNeeded"
        );
        client
            .send(Command::Write {
                terminal_id,
                bytes: b"1".to_vec(),
            })
            .unwrap();
        assert!(
            wait_for(&mut client, working, Duration::from_secs(2))
                .await
                .is_some(),
            "a bare digit answer must flip InputNeeded → Working"
        );

        // Same for a lone Esc (dismiss the chooser).
        mock.emit(&key, chooser).await;
        assert!(
            wait_for(&mut client, input_needed, Duration::from_secs(10))
                .await
                .is_some(),
            "re-rendered chooser must re-raise InputNeeded"
        );
        client
            .send(Command::Write {
                terminal_id,
                bytes: vec![0x1b],
            })
            .unwrap();
        assert!(
            wait_for(&mut client, working, Duration::from_secs(2))
                .await
                .is_some(),
            "a lone Esc must flip InputNeeded → Working"
        );
    })
    .await
    .expect("deadline");
}

/// The quiet-gate wiring itself (#289): while chunks keep arriving the
/// pump re-arms its quiet timer, so a prompt render mid-stream reads as
/// `Working` (bytes flowing = the agent is doing something) and the `?`
/// must NOT surface — even though the prompt markers sit in the detect
/// buffer the whole time. Only once the PTY has been silent past the
/// classify window does the parked dialog raise `InputNeeded`. Paused
/// time makes the 2s chunk cadence and the ~5s quiet window exact.
#[tokio::test(start_paused = true)]
async fn streaming_holds_working_until_quiet_window_elapses() {
    timeout(Duration::from_secs(120), async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let _ = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        let input_needed = |e: &Event| {
            matches!(
                e,
                Event::AgentState {
                    state: lazybox_ipc::AgentState::InputNeeded,
                    ..
                }
            )
        };

        // A permission prompt paints mid-stream. Bytes flowing → the
        // immediate reading is the spinner, never the `?`.
        mock.emit(
            &key,
            concat!(
                "Do you want to proceed?\n",
                "❯ 1. Yes\n",
                "  2. No\n",
                "Esc to cancel",
            ),
        )
        .await;
        let first = wait_for(
            &mut client,
            |e| matches!(e, Event::AgentState { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("byte flow must produce a state reading");
        assert!(
            matches!(
                first,
                Event::AgentState {
                    state: lazybox_ipc::AgentState::Working,
                    ..
                }
            ),
            "the first reading while bytes flow must be Working, got {first:?}"
        );

        // The agent keeps streaming at a sub-quiet cadence; every chunk
        // re-arms the timer, so the stale markers never classify.
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            mock.emit(&key, "streaming tool output\n").await;
        }
        let leaked = wait_for(&mut client, input_needed, Duration::from_secs(1)).await;
        assert!(
            leaked.is_none(),
            "a visibly-streaming session must never flip to `?`"
        );

        // The stream stops with the dialog re-painted as the resting
        // screen; past the quiet window it must classify InputNeeded.
        mock.emit(
            &key,
            concat!(
                "Do you want to proceed?\n",
                "❯ 1. Yes\n",
                "  2. No\n",
                "Esc to cancel",
            ),
        )
        .await;
        let raised = wait_for(&mut client, input_needed, Duration::from_secs(10)).await;
        assert!(
            raised.is_some(),
            "a dialog quiet past the classify window must raise `?`"
        );
    })
    .await
    .expect("deadline");
}

/// Minimal GitHub `Task` for the collapse test.
fn collapse_task(key: &str, url: &str, closes: Vec<lazybox_core::TaskId>) -> lazybox_core::Task {
    lazybox_core::Task {
        id: lazybox_core::TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: "t".into(),
        body: None,
        state: lazybox_core::TaskState::Open,
        role: lazybox_core::TaskRole::Author,
        ci: lazybox_core::CiStatus::None,
        review: lazybox_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: url.into(),
        repo: Some("o/r".into()),
        branch: Some("feat".into()),
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
        closes_issues: closes,
    }
}

/// Issue #78 regression: the manual `Shift-J` collapse (`Command::
/// CollapseIntoPr`) must carry a live Claude terminal across to the PR
/// workspace, not tear it down. Drives the FULL serve loop: seed an
/// issue + claiming PR, spawn a real (mock-backed) agent on the issue,
/// collapse, then assert the terminal is rebadged onto the PR and the
/// backend session is never killed.
#[tokio::test]
async fn collapse_into_pr_carries_live_terminal_to_the_pr() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();

        // Issue #50 and PR #51 (which closes #50). Seed both before
        // serving so the collapse handler can resolve the claiming PR.
        let issue = lazybox_core::Workspace::from_task(
            collapse_task("o/r#50", "https://github.com/o/r/issues/50", vec![]),
            chrono::Utc::now(),
        );
        let issue_task_id = issue.primary_task().unwrap().id.clone();
        let issue_key = issue.key.clone();
        let pr = lazybox_core::Workspace::from_task(
            collapse_task(
                "o/r#51",
                "https://github.com/o/r/pull/51",
                vec![issue_task_id],
            ),
            chrono::Utc::now(),
        );
        let pr_key = pr.key.clone();
        for ws in [&issue, &pr] {
            config
                .store
                .save_workspace(&lazybox_store::WorkspaceRecord {
                    key: ws.key.as_str().to_string(),
                    created_at: ws.created_at,
                    workspace_json: Some(serde_json::to_string(ws).unwrap()),
                })
                .unwrap();
        }

        let mut client = subscribed(config.clone()).await;

        // Spawn a Claude agent on the ISSUE workspace.
        client
            .send(Command::Spawn {
                model_alias: None,
                session_key: issue_key.as_str().into(),
                session_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        let terminal_id = match wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("TerminalSpawned arrived")
        {
            Event::TerminalSpawned { terminal_id, .. } => terminal_id,
            _ => unreachable!(),
        };
        let backend_key = mock.list().await.unwrap().into_iter().next().unwrap();

        // Sanity: the spawn persisted a session record onto the issue
        // workspace before we collapse.
        let issue_before: lazybox_core::Workspace = serde_json::from_str(
            &config
                .store
                .get_workspace(&issue_key)
                .unwrap()
                .unwrap()
                .workspace_json
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            issue_before.sessions.len(),
            1,
            "spawn must persist the session on the issue workspace",
        );

        // Join the issue into the PR.
        client
            .send(Command::CollapseIntoPr {
                issue_workspace_key: issue_key.as_str().into(),
            })
            .unwrap();

        // The terminal must be rebadged onto the PR, and that rebadge
        // must arrive before (or without) any exit for our terminal.
        let pr_session_key: lazybox_core::SessionKey = (&pr_key).into();
        let issue_session_key: lazybox_core::SessionKey = (&issue_key).into();
        let rebadged = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::TerminalsRebadged { from, to }
                        if *from == issue_session_key && *to == pr_session_key
                )
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            rebadged.is_some(),
            "collapse must broadcast TerminalsRebadged issue→PR",
        );

        // Wait for the collapse to fully settle. `WorkspaceMerged` is the
        // last event the handler emits — after the PR (carrying the moved
        // session) is committed and the issue row is dropped.
        let merged = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::WorkspaceMerged { pr_workspace_key, .. }
                        if pr_workspace_key.as_str() == pr_key.as_str()
                )
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(merged.is_some(), "collapse must broadcast WorkspaceMerged");

        // The live backend session must NOT have been killed.
        assert!(
            mock.list().await.unwrap().contains(&backend_key),
            "the agent's backend session must survive the collapse",
        );

        // And the daemon's terminal_meta must now key the terminal to
        // the PR workspace so wire-side traffic + restart recovery
        // follow it.
        let meta = config.terminal_meta.lock().await;
        let (sk, _) = meta.get(&terminal_id).expect("terminal still tracked");
        assert_eq!(
            sk, &pr_session_key,
            "terminal_meta must rebadge the live terminal onto the PR",
        );
        drop(meta);

        // The session record moved to the PR workspace (not deleted).
        let pr_after: lazybox_core::Workspace = serde_json::from_str(
            &config
                .store
                .get_workspace(&pr_key)
                .unwrap()
                .unwrap()
                .workspace_json
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            pr_after.sessions.len(),
            1,
            "the issue's session must live on the PR workspace after the join",
        );
    })
    .await
    .expect("deadline");
}

/// Issue #134: pressing `w` on many issues in quick succession must
/// deliver EVERY work prompt. Pre-fix, concurrent spawns competing for
/// CPU and the shared state mutexes let readiness detection lag past the
/// inject deadline; the deadline rung then pasted blindly (into a screen
/// that wasn't ready) or dropped the prompt at the gate cap — so some
/// agents opened with no instruction and no signal that it happened.
///
/// This drives N prompt-carrying spawns concurrently against the mock
/// backend, signals each one ready, and asserts all N distinct prompts
/// were delivered (paste + submit) to their own sessions.
#[tokio::test]
async fn many_concurrent_prompt_spawns_all_deliver() {
    timeout(Duration::from_secs(20), async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let cwd = std::env::temp_dir().to_string_lossy().to_string();

        const N: usize = 16;
        let prompts: Vec<String> = (0..N)
            .map(|i| format!("Work item {i}: implement the feature."))
            .collect();

        // Fire all spawns at once — distinct workspaces so none collapse
        // onto another (the singleton guard keys on session_key).
        let mut handles = Vec::new();
        for (i, prompt) in prompts.iter().enumerate() {
            let cfg = config.clone();
            let cwd = cwd.clone();
            let prompt = prompt.clone();
            handles.push(tokio::spawn(async move {
                lazybox_server::spawn_handler::handle_spawn(
                    &cfg,
                    format!("test:ws-stress-{i}").into(),
                    None,
                    TerminalKind::Agent("claude".into()),
                    Some(cwd),
                    Some(prompt),
                    false,
                    false, // on_main
                    None,  // model_alias
                )
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // One backend session per spawn.
        let keys = mock.list().await.unwrap();
        assert_eq!(keys.len(), N, "expected {N} sessions, got {}", keys.len());

        // Signal every agent ready (the input-box footer markers
        // `detect_ready_for_prompt` keys on, no permission gate up).
        for key in &keys {
            mock.emit(key, b"Esc to cancel  Tab to amend").await;
        }

        // Poll until every distinct prompt has been delivered somewhere,
        // each with its committing Enter.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let mut delivered = 0usize;
            for key in &keys {
                let joined = mock
                    .writes_for(key)
                    .await
                    .into_iter()
                    .flatten()
                    .collect::<Vec<u8>>();
                let text = String::from_utf8_lossy(&joined);
                if prompts.iter().any(|p| text.contains(p.as_str())) && joined.contains(&b'\r') {
                    delivered += 1;
                }
            }
            if delivered == N || tokio::time::Instant::now() >= deadline {
                assert_eq!(
                    delivered, N,
                    "only {delivered}/{N} agents received their work prompt + submit",
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Every distinct prompt is present across the sessions — none lost.
        let mut all_writes = String::new();
        for key in &keys {
            let joined = mock
                .writes_for(key)
                .await
                .into_iter()
                .flatten()
                .collect::<Vec<u8>>();
            all_writes.push_str(&String::from_utf8_lossy(&joined));
        }
        for prompt in &prompts {
            assert!(
                all_writes.contains(prompt.as_str()),
                "work prompt was dropped: {prompt:?}",
            );
        }
    })
    .await
    .expect("deadline");
}
