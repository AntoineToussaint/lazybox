//! End-to-end tests for the daemon's Spawn → backend → bus pipeline.
//!
//! Backend is the in-memory [`MockBackend`] — no real shells / tmux /
//! curl. Tests drive synthetic output via `MockBackend::emit` and end
//! sessions via `finish`.

use lazybox_ipc::{Command, Event, TerminalInputIntent, TerminalKind, channel};
use lazybox_server::backend::{MockBackend, SessionBackend};
use lazybox_server::spawn_handler::SpawnOptions;
use lazybox_server::{Server, ServerConfig};
use lazybox_store::{MemoryStore, Store};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Explicit cwd for tests that exercise terminal behavior rather than
/// persisted workspace resolution.
fn test_cwd() -> Option<String> {
    Some(std::env::temp_dir().to_string_lossy().into_owned())
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

/// Drain the daemon-owned config pushes. Tests that assert the stream is
/// otherwise quiet call this after `subscribed`.
async fn drain_auto_fix_config(client: &mut lazybox_ipc::Client) {
    let shell = timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("shell config deadline")
        .expect("shell config event");
    assert!(
        matches!(shell, Event::ShellCommandConfig { .. }),
        "expected ShellCommandConfig, got {shell:?}"
    );
    let agents = timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("agent availability config deadline")
        .expect("agent availability config event");
    assert!(
        matches!(agents, Event::AgentAvailabilityConfig { .. }),
        "expected AgentAvailabilityConfig, got {agents:?}"
    );
    let cfg = timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("auto-fix policy config deadline")
        .expect("auto-fix policy config event");
    assert!(
        matches!(cfg, Event::AutoFixPolicyConfig { .. }),
        "expected AutoFixPolicyConfig, got {cfg:?}"
    );
}

async fn spawn_and_wait(
    client: &mut lazybox_ipc::Client,
    kind: TerminalKind,
) -> lazybox_ipc::TerminalId {
    client
        .send(Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: "test:ws-1".into(),
            session_id: None,
            client_request_id: None,
            kind,
            // This helper tests the terminal pipeline, not workspace
            // resolution. An explicit cwd keeps that boundary honest:
            // production spawns without one require a persisted row.
            cwd: test_cwd(),
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

#[tokio::test]
async fn live_provider_auth_failure_emits_recovery_without_stopping_the_agent() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("codex".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        mock.emit(
            &key,
            include_bytes!("../../agents/tests/fixtures/codex_auth_chat_negative.bin"),
        )
        .await;
        assert!(
            wait_for(
                &mut client,
                |event| matches!(event, Event::AgentAuthRequired { .. }),
                Duration::from_millis(100),
            )
            .await
            .is_none(),
            "ordinary conversation text must not trigger recovery",
        );

        mock.emit(
            &key,
            include_bytes!("../../agents/tests/fixtures/codex_refresh_rejected.bin"),
        )
        .await;
        let required = wait_for(
            &mut client,
            |event| matches!(event, Event::AgentAuthRequired { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("auth failure must offer recovery");
        assert!(matches!(
            required,
            Event::AgentAuthRequired {
                terminal_id: id,
                ref agent_id,
                other_session_count: 0,
                ..
            } if id == terminal_id && agent_id == "codex"
        ));
        assert!(
            mock.list().await.unwrap().contains(&key),
            "detection alone must not stop the provider process"
        );
    })
    .await
    .expect("deadline");
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
            config
                .terminal
                .hook_activity_for(terminal_id)
                .await
                .is_none(),
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
            config
                .terminal
                .hook_activity_for(terminal_id)
                .await
                .is_none(),
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
            .terminal
            .record_hook_activity(terminal_id, stale)
            .await;

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
            .terminal
            .record_hook_activity(terminal_id, stale)
            .await;

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
            config.terminal.agent_state_for(terminal_id).await,
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
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:ws-1".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: test_cwd(),
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
        let env = mock.env_for(&key).await.expect("captured spawn env");
        assert!(
            env.iter()
                .any(|(key, value)| key == "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN" && value == "1"),
            "the interactive spawn boundary must receive Claude's inline renderer env: {env:?}",
        );
    })
    .await
    .expect("deadline");
}

/// The stable-launcher copy that lets an agent's hook path survive a
/// rebuild / `cargo clean` / worktree removal belongs at daemon boot
/// (`ensure_stable_hook_exe`), never on the per-spawn hot path: a per-spawn
/// ~80 MB `fs::copy` blocked every `TerminalSpawned`, and because this
/// harness drives `handle_spawn` against a real `LAZYBOX_HOME` it raced
/// every test process on one `<home>/bin/lazybox`. A spawn must leave
/// `<home>/bin` untouched — proven here — while `ensure_stable_hook_exe`
/// itself is exercised by the `stabilize_exe_*` unit tests. (This asserts
/// only *absence*, so it's immune to the harness's shared-`LAZYBOX_HOME`
/// races: nothing writes the stable path, so it never appears.)
#[tokio::test]
async fn spawn_hot_path_never_copies_into_the_stable_bin_dir() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let stable = lazybox_core::paths::stable_exe_path();
        assert!(
            !stable.exists(),
            "precondition: an isolated home starts without a stable copy"
        );

        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        client
            .send(Command::Spawn {
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:ws-bin".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: test_cwd(),
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("TerminalSpawned arrived");

        assert!(
            !stable.exists(),
            "the spawn hot path must not copy the binary into <home>/bin"
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn read_only_spawn_rejects_a_writable_singleton() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        client
            .send(Command::Spawn {
                session_key: "test:critic".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("codex".into()),
                cwd: test_cwd(),
                initial_prompt: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
            })
            .unwrap();
        wait_for(
            &mut client,
            |event| matches!(event, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("writable TerminalSpawned arrived");
        client
            .send(Command::Spawn {
                session_key: "test:critic".into(),
                session_id: None,
                client_request_id: Some("critic-1".into()),
                kind: TerminalKind::Agent("codex".into()),
                cwd: test_cwd(),
                initial_prompt: Some("Review this work without editing".into()),
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::ReadOnly,
            })
            .unwrap();

        let failed = wait_for(
            &mut client,
            |event| {
                matches!(
                    event,
                    Event::CommandFailed { client_request_id, .. }
                        if client_request_id == "critic-1"
                )
            },
            Duration::from_secs(2),
        )
        .await
        .expect("correlated spawn failure arrived");
        assert!(matches!(
            failed,
            Event::CommandFailed { message, .. }
                if message == "terminal was not spawned"
        ));
        let keys = mock.list().await.unwrap();
        assert_eq!(
            keys.len(),
            1,
            "a read-only critic must neither reuse nor duplicate the writable singleton"
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn read_only_prompt_spawn_cannot_inherit_autonomous_bypass() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        client
            .send(Command::Spawn {
                session_key: "test:critic".into(),
                session_id: None,
                client_request_id: Some("critic-1".into()),
                kind: TerminalKind::Agent("codex".into()),
                cwd: test_cwd(),
                initial_prompt: Some("Review this work without editing".into()),
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::ReadOnly,
            })
            .unwrap();

        wait_for(
            &mut client,
            |event| {
                matches!(
                    event,
                    Event::CommandCompleted { client_request_id }
                        if client_request_id == "critic-1"
                )
            },
            Duration::from_secs(2),
        )
        .await
        .expect("correlated spawn completion arrived");
        let keys = mock.list().await.unwrap();
        assert_eq!(keys.len(), 1);
        let argv = mock.argv_for(&keys[0]).await.unwrap();
        assert!(
            argv.windows(2)
                .any(|args| args == ["--sandbox", "read-only"])
        );
        assert!(
            !argv
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"),
            "read-only access must win over prompt-driven autonomy: {argv:?}"
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn restored_critic_session_keeps_read_only_access() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let first_backend = MockBackend::new();
        let first_config =
            ServerConfig::with_store_and_backend(store.clone(), Arc::new(first_backend));
        let workspace_key = lazybox_core::WorkspaceKey::new("test:critic-restore");
        let worktree = tempfile::TempDir::new().unwrap();
        let mut workspace =
            lazybox_core::Workspace::empty(workspace_key.clone(), "critic", chrono::Utc::now());
        let session = lazybox_core::WorkspaceSession::new(
            workspace_key.clone(),
            lazybox_core::SessionKind::Agent {
                agent_id: "codex".into(),
            },
            worktree.path().to_path_buf(),
            chrono::Utc::now(),
        );
        let session_id = session.id;
        workspace.add_session(session);
        store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: workspace_key.as_str().into(),
                created_at: workspace.created_at,
                workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
            })
            .unwrap();

        lazybox_server::spawn_handler::handle_spawn(
            &first_config,
            workspace_key.as_str().into(),
            Some(session_id),
            TerminalKind::Agent("codex".into()),
            SpawnOptions {
                access: lazybox_ipc::AgentRunAccess::ReadOnly,
                ..Default::default()
            },
        )
        .await;

        let restored_backend = MockBackend::new();
        let restored_config =
            ServerConfig::with_store_and_backend(store, Arc::new(restored_backend.clone()));
        lazybox_server::spawn_handler::restore_persisted_sessions(&restored_config).await;

        let keys = restored_backend.list().await.unwrap();
        assert_eq!(keys.len(), 1, "the persisted session is restored once");
        let argv = restored_backend.argv_for(&keys[0]).await.unwrap();
        assert!(
            argv.windows(2)
                .any(|args| args == ["--sandbox", "read-only"]),
            "a restored critic must remain sandboxed: {argv:?}"
        );
        assert!(
            !argv
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"),
            "a restored critic must not become autonomous: {argv:?}"
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn hook_session_identity_is_persisted_and_used_for_restore() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let first_backend = MockBackend::new();
        let first_config =
            ServerConfig::with_store_and_backend(store.clone(), Arc::new(first_backend.clone()));
        let workspace_key = lazybox_core::WorkspaceKey::new("test:exact-provider-resume");
        let worktree = tempfile::TempDir::new().unwrap();
        let mut workspace = lazybox_core::Workspace::empty(
            workspace_key.clone(),
            "exact resume",
            chrono::Utc::now(),
        );
        let session = lazybox_core::WorkspaceSession::new(
            workspace_key.clone(),
            lazybox_core::SessionKind::Agent {
                agent_id: "codex".into(),
            },
            worktree.path().to_path_buf(),
            chrono::Utc::now(),
        );
        let session_id = session.id;
        workspace.add_session(session);
        store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: workspace_key.as_str().into(),
                created_at: workspace.created_at,
                workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
            })
            .unwrap();

        let mut client = subscribed(first_config).await;
        client
            .send(Command::Spawn {
                session_key: workspace_key.as_str().into(),
                session_id: Some(session_id),
                client_request_id: None,
                kind: TerminalKind::Agent("codex".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
            })
            .unwrap();
        let Event::TerminalSpawned { terminal_id, .. } = wait_for(
            &mut client,
            |event| matches!(event, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("spawned terminal") else {
            unreachable!()
        };
        let backend_key = first_backend
            .list()
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        client
            .send(Command::IngestHook {
                terminal_id,
                backend_key: Some(backend_key),
                hook: lazybox_ipc::HookEvent {
                    kind: lazybox_ipc::HookEventKind::SessionStart,
                    session_id: Some("provider-conversation-708".into()),
                    cwd: None,
                    tool_name: None,
                    notification: None,
                },
            })
            .unwrap();

        let persisted = loop {
            let record = store
                .get_workspace(&workspace_key)
                .unwrap()
                .expect("workspace record");
            let workspace: lazybox_core::Workspace =
                serde_json::from_str(record.workspace_json.as_deref().unwrap()).unwrap();
            if workspace.sessions[0]
                .provider_session_ids
                .get("codex")
                .is_some_and(|id| id == "provider-conversation-708")
            {
                break workspace;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(
            persisted.sessions[0].provider_session_ids["codex"],
            "provider-conversation-708"
        );

        let restored_backend = MockBackend::new();
        let restored_config =
            ServerConfig::with_store_and_backend(store, Arc::new(restored_backend.clone()));
        lazybox_server::spawn_handler::restore_persisted_sessions(&restored_config).await;
        let argv = restored_backend
            .all_argv()
            .await
            .into_iter()
            .next()
            .expect("restored agent");
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["resume", "provider-conversation-708"]),
            "restore must target the captured provider conversation: {argv:?}"
        );
        assert!(!argv.iter().any(|arg| arg == "--last"));
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
            SpawnOptions {
                cwd: Some(cwd),
                autonomous: true,
                ..Default::default()
            },
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
        assert_eq!(
            config
                .store
                .get_kv(&format!("terminal-pty-generation:{key}"))
                .expect("launch generation read")
                .as_deref(),
            Some("1"),
            "fresh Claude PTYs persist the renderer compatibility generation"
        );
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
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:ws-1".into(),
                session_id: None,
                client_request_id: Some("unknown-agent-spawn".into()),
                kind: TerminalKind::Agent("does-not-exist".into()),
                cwd: test_cwd(),
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
        let failed = wait_for(
            &mut client,
            |event| {
                matches!(
                    event,
                    Event::CommandFailed {
                        client_request_id,
                        ..
                    } if client_request_id == "unknown-agent-spawn"
                )
            },
            Duration::from_secs(2),
        )
        .await
        .expect("correlated spawn failure");
        assert!(matches!(
            failed,
            Event::CommandFailed { message, .. }
                if message.contains("was not spawned")
        ));
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn successful_spawn_emits_its_correlated_completion() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config).await;
        client
            .send(Command::Spawn {
                session_key: "test:correlated-spawn".into(),
                session_id: None,
                client_request_id: Some("spawn-success".into()),
                kind: TerminalKind::Shell,
                cwd: test_cwd(),
                initial_prompt: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
            })
            .unwrap();

        let completed = wait_for(
            &mut client,
            |event| {
                matches!(
                    event,
                    Event::CommandCompleted { client_request_id }
                        if client_request_id == "spawn-success"
                )
            },
            Duration::from_secs(2),
        )
        .await
        .expect("correlated spawn completion");
        assert!(matches!(completed, Event::CommandCompleted { .. }));
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

        client
            .send(Command::Close {
                terminal_id,
                client_request_id: None,
            })
            .unwrap();

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
        let map_len = config.terminal.terminal_count().await;
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

/// A client that independently detects a sequence gap can request an
/// authoritative replay. Success must cover the requested sequence;
/// transient failure is explicit and never encoded as an empty reset.
#[tokio::test]
async fn client_requested_terminal_resync_is_covered_or_explicitly_unavailable() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Shell).await;
        let key = config
            .terminal.backend_key_for(terminal_id)
            .await
            .expect("backend key");
        mock.emit(&key, b"screen").await;
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalOutput { terminal_id: id, seq: 1, .. } if *id == terminal_id),
            Duration::from_secs(2),
        )
        .await
        .expect("live output");

        client
            .send(Command::RequestTerminalResync {
                requests: vec![lazybox_ipc::TerminalResyncRequest {
                    terminal_id,
                    required_seq: 1,
                }],
            })
            .expect("request resync");
        let recovered = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalResync { terminal_id: id, .. } if *id == terminal_id),
            Duration::from_secs(2),
        )
        .await
        .expect("covered replay");
        assert!(matches!(
            recovered,
            Event::TerminalResync { replay, seq: 1, .. } if replay == b"screen"
        ));

        mock.fail_next_snapshots(&key, 1).await;
        client
            .send(Command::RequestTerminalResync {
                requests: vec![lazybox_ipc::TerminalResyncRequest {
                    terminal_id,
                    required_seq: 2,
                }],
            })
            .expect("request unavailable resync");
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalResyncUnavailable { terminal_id: id } if *id == terminal_id),
            Duration::from_secs(2),
        )
        .await
        .expect("snapshot failure is explicit");
    })
    .await
    .expect("test deadline exceeded");
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
        assert!(config.terminal.is_empty().await);

        // Listen on the bus before recovery so TerminalSpawned isn't lost.
        let mut bus = config.bus.subscribe();

        lazybox_server::spawn_handler::recover_sessions(&config).await;

        // Map now has the survivor under a fresh wire id.
        let ids = config.terminal.terminal_ids().await;
        assert_eq!(ids.len(), 1, "expected one recovered session, got {ids:?}");
        let recovered_key = config
            .terminal
            .backend_key_for(ids[0])
            .await
            .expect("recovered backend key");
        assert_eq!(recovered_key, preexisting);

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

#[tokio::test]
async fn recovered_pre_generation_claude_requires_an_explicit_restart() {
    timeout(TEST_DEADLINE, async {
        let backend = MockBackend::new();
        let backend_key = backend
            .spawn(&["claude".into()], None, &[], "legacy-claude")
            .await
            .expect("pre-existing backend session");
        let store = Arc::new(MemoryStore::new());
        let metadata =
            serde_json::to_string(&("test:legacy-claude", TerminalKind::Agent("claude".into())))
                .expect("metadata");
        store
            .set_kv(&format!("terminal:{backend_key}"), &metadata)
            .expect("persist legacy metadata");

        let config = ServerConfig::with_store_and_backend(store, Arc::new(backend));
        lazybox_server::spawn_handler::recover_sessions(&config).await;
        assert_eq!(config.terminal.outdated_agent_count().await, 1);

        let mut client = subscribed(config).await;
        let warning = timeout(Duration::from_secs(1), client.recv())
            .await
            .expect("restart warning deadline")
            .expect("restart warning event");
        assert!(matches!(
            warning,
            Event::RecoveredTerminalsRequireRestart { terminal_ids }
                if terminal_ids.len() == 1
        ));
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn subscribe_does_not_warn_for_an_outdated_terminal_absent_from_its_snapshot() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        config
            .terminal
            .mark_outdated_agent(lazybox_ipc::TerminalId(404))
            .await;

        let mut client = subscribed(config).await;
        // The auto-fix policy config still lands (it's unconditional);
        // drain it, then assert no *warning* follows.
        drain_auto_fix_config(&mut client).await;
        assert!(
            timeout(Duration::from_millis(100), client.recv())
                .await
                .is_err(),
            "an auxiliary teardown marker without a snapshotted terminal must not emit a warning"
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn recovered_claude_at_current_or_newer_generation_needs_no_restart() {
    timeout(TEST_DEADLINE, async {
        let backend = MockBackend::new();
        let store = Arc::new(MemoryStore::new());
        for generation in [1, 2] {
            let backend_key = backend
                .spawn(
                    &["claude".into()],
                    None,
                    &[],
                    &format!("claude-generation-{generation}"),
                )
                .await
                .expect("pre-existing backend session");
            let metadata = serde_json::to_string(&(
                format!("test:claude-generation-{generation}"),
                TerminalKind::Agent("claude".into()),
            ))
            .expect("metadata");
            store
                .set_kv(&format!("terminal:{backend_key}"), &metadata)
                .expect("persist terminal metadata");
            store
                .set_kv(
                    &format!("terminal-pty-generation:{backend_key}"),
                    &generation.to_string(),
                )
                .expect("persist launch generation");
        }

        let config = ServerConfig::with_store_and_backend(store, Arc::new(backend));
        lazybox_server::spawn_handler::recover_sessions(&config).await;

        assert!(
            config.terminal.outdated_agent_count().await == 0,
            "a recovered process at least as new as this daemon's PTY contract is compatible"
        );
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
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:ws-ingest".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: test_cwd(),
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

/// Codex's TUI also treats a rapid multi-line write as a paste. Its work
/// prompt must therefore use explicit bracketed-paste markers followed by a
/// separate carriage-return write; appending `\n` to the prompt body leaves
/// the text sitting unsubmitted in the composer.
#[tokio::test]
async fn codex_initial_prompt_pastes_then_sends_enter_separately() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;

        const WORK: &str = "Implement issue #391.\nRun the focused tests.";
        client
            .send(Command::Spawn {
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:ws-codex-ingest".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("codex".into()),
                cwd: test_cwd(),
                initial_prompt: Some(WORK.into()),
                on_main: false,
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("TerminalSpawned arrived");

        let key = mock.list().await.unwrap().into_iter().next().unwrap();
        mock.emit(&key, "› Try something\ngpt-5.4 xhigh · /repo")
            .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let writes = loop {
            let writes = mock.writes_for(&key).await;
            if writes.len() >= 2 || tokio::time::Instant::now() >= deadline {
                break writes;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(
            writes.len() >= 2,
            "Codex prompt was not followed by a separate Enter; writes = {writes:?}",
        );
        let mut expected_paste = b"\x1b[200~".to_vec();
        expected_paste.extend_from_slice(WORK.as_bytes());
        expected_paste.extend_from_slice(b"\x1b[201~");
        assert_eq!(writes[0], expected_paste);
        assert_eq!(writes[1], b"\r");
    })
    .await
    .expect("deadline");
}

/// The atomic PTY protocol must honor compose-only recall for line-oriented
/// agents too. Under the former split API, the line adapter embedded `\n` in
/// its first write before the server could suppress submission, so recalling a
/// draft accidentally started a turn.
#[tokio::test]
async fn line_oriented_prompt_recall_omits_the_inline_newline() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id =
            spawn_and_wait(&mut client, TerminalKind::Agent("cursor-agent".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        client
            .send(Command::InjectPrompt {
                terminal_id,
                prompt: "\n  edit this draft".into(),
                fallback_spawn: None,
                submit: false,
            })
            .unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let writes = loop {
            let writes = mock.writes_for(&key).await;
            if !writes.is_empty() || tokio::time::Instant::now() >= deadline {
                break writes;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(writes, vec![b"edit this draft".to_vec()]);
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
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:ws-1".into(),
                session_id: None,
                client_request_id: None,
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

/// A linked (no-worktree) workspace lands every session in its on-disk
/// checkout AND enforces the agent singleton: a second `a c` on the same
/// linked workspace reuses the first agent (focus, not a duplicate),
/// even though the client sends `on_main: false`. Regression guard for
/// the request/landed on-main mismatch that would otherwise launch two
/// Claudes into the user's real tree.
#[tokio::test]
async fn linked_workspace_agent_spawn_is_a_singleton() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();

        // Persist a linked workspace pointing at a real on-disk dir.
        let checkout = tempfile::tempdir().unwrap();
        let mut ws = lazybox_core::Workspace::empty(
            lazybox_core::WorkspaceKey::new("acme-widget"),
            "feature-x",
            chrono::Utc::now(),
        );
        ws.local = true;
        ws.linked_checkout = Some(checkout.path().to_path_buf());
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: ws.key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).unwrap()),
            })
            .unwrap();

        let mut client = subscribed(config.clone()).await;
        let spawn = |c: &mut lazybox_ipc::Client| {
            c.send(Command::Spawn {
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "acme-widget".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        };

        // First spawn: an agent starts, rooted in the real checkout.
        spawn(&mut client);
        let first = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("first agent spawns");
        let backend_key = mock.list().await.unwrap().into_iter().next().unwrap();
        assert_eq!(
            mock.cwd_for(&backend_key).await.as_deref(),
            Some(checkout.path()),
            "the agent runs in the linked checkout, not a worktree",
        );

        // Second spawn: reuse, not a duplicate.
        spawn(&mut client);
        let focused = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalFocusRequested { .. }),
            Duration::from_secs(2),
        )
        .await;
        assert!(focused.is_some(), "second spawn must request focus");
        assert_eq!(
            mock.list().await.unwrap().len(),
            1,
            "a linked workspace must run one Claude, not two, in the real tree",
        );
        let _ = first;
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
                    client_request_id: None,
                    kind: TerminalKind::Shell,
                    cwd: test_cwd(),
                    access: lazybox_ipc::AgentRunAccess::Default,
                }),
                submit: true,
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
                submit: true,
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
                submit: true,
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

/// Injection-safety regression: a permission chooser that appears AFTER
/// the paste landed must abort the submit-confirm loop's Enter resends.
/// The inject path gates on `InputNeeded` once, up front; pre-fix, when
/// a chooser surfaced while the submit evidence was awaited, the loop
/// resent bare Enter up to its limit — and Enter into a Claude chooser
/// selects the default answer (typically "Yes"), silently auto-approving
/// a tool the user never saw, with the injected text lost anyway. The
/// give-up must be loud (`TerminalInputRejected`), never more Enters.
// Paused time: the 250ms paste-settle window and the 3s+ confirm waits
// ride tokio's auto-advance instead of sleeping for real.
#[tokio::test(start_paused = true)]
async fn chooser_after_paste_suppresses_submit_resends() {
    timeout(Duration::from_secs(60), async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        const WORK: &str = "Apply the review feedback on PR #12.";
        client
            .send(Command::InjectPrompt {
                terminal_id,
                prompt: WORK.into(),
                fallback_spawn: None,
                submit: true,
            })
            .unwrap();

        // The paste and the initial submit Enter both land: two writes.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let writes = mock.writes_for(&key).await;
            if writes.len() >= 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "paste + Enter never delivered; writes = {writes:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // A permission chooser now appears (hook-driven, the same
        // `InputNeeded` a live dialog produces) — the dialog swallowed
        // the submit, so no UserPromptSubmit / Working evidence will
        // ever arrive.
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

        // The confirm loop finds no submit evidence. Pre-fix it resent
        // bare Enter into the chooser; it must abort loudly instead.
        let rejected = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::TerminalInputRejected { terminal_id: tid, .. } if *tid == terminal_id
                )
            },
            Duration::from_secs(30),
        )
        .await
        .expect("the suppressed submit must fail loudly");
        if let Event::TerminalInputRejected { message, .. } = &rejected {
            assert!(
                message.contains("permission prompt"),
                "the notice must name the chooser hazard, got {message:?}"
            );
        }

        // Still exactly the paste and the one pre-chooser submit — no
        // Enter was resent into the permission chooser.
        let writes = mock.writes_for(&key).await;
        assert_eq!(
            writes.len(),
            2,
            "bare Enter was resent into a permission chooser; writes = {writes:?}"
        );
        assert_eq!(writes[1], b"\r".to_vec());
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
        assert!(!terminals[0].replay_available);

        // The real bug symptom: subsequent Spawn never reaches the
        // daemon. Issue one and confirm the daemon processes it end
        // to end — this is what the user pressed `s` for.
        consumer
            .send(Command::Spawn {
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:wedge-followup".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Shell,
                cwd: test_cwd(),
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

/// Per-session deadlines must run concurrently. Four wedged terminals should
/// cost roughly one 500ms deadline, not four serialized deadlines that freeze
/// the Subscribe lane for two seconds.
#[tokio::test]
async fn wedged_terminal_snapshots_are_acquired_with_bounded_concurrency() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut producer = subscribed(config.clone()).await;
        for _ in 0..4 {
            let _ = spawn_and_wait(&mut producer, TerminalKind::Shell).await;
        }
        let keys = mock.list().await.expect("list sessions");
        assert_eq!(keys.len(), 4);
        for key in &keys {
            mock.wedge_snapshot(key).await;
        }

        let mut consumer = run_daemon(config).await;
        let started = tokio::time::Instant::now();
        consumer.send(Command::Subscribe).expect("subscribe");
        let event = timeout(Duration::from_millis(1_500), consumer.recv())
            .await
            .expect("four wedged snapshots serialized instead of running concurrently")
            .expect("connection open");
        assert!(
            started.elapsed() < Duration::from_millis(1_500),
            "snapshot assembly exceeded the bounded-concurrency deadline"
        );
        let Event::Snapshot { terminals, .. } = event else {
            panic!("expected Snapshot, got {event:?}");
        };
        assert_eq!(terminals.len(), 4);
        assert!(terminals.iter().all(|terminal| !terminal.replay_available));
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
                intent: TerminalInputIntent::Submit,
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
        // resting screen) must be Done — the agent worked (answered, acted)
        // and came to rest, so it settles to Done, NOT back to InputNeeded.
        // The #101 stale-buffer regression was the bounce to InputNeeded;
        // the #357 one-way door is why the settle is Done and never Idle.
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
                lazybox_ipc::AgentState::Done,
                "after answering, the prompt-free follow-up must settle to Done, \
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
                SpawnOptions {
                    cwd: Some(cwd_a),
                    ..Default::default()
                },
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
                SpawnOptions {
                    cwd: Some(cwd),
                    initial_prompt: Some(WORK.into()),
                    autonomous: true,
                    ..Default::default()
                },
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
/// daemon's own cwd.
#[tokio::test]
async fn spawn_aborts_when_workspace_was_deleted_mid_flight() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        config
            .deleted_workspaces
            .lock()
            .insert("test:ws-deleted".to_string());
        let mut bus = config.bus.subscribe();

        lazybox_server::spawn_handler::handle_spawn(
            &config,
            "test:ws-deleted".into(),
            None,
            TerminalKind::Agent("claude".into()),
            SpawnOptions::default(),
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

/// A stale or forged workspace key must never turn into a terminal in
/// the daemon's cwd. This is the non-racy form of the same safety rule:
/// without an explicit cwd, workspace resolution is mandatory.
#[tokio::test]
async fn spawn_aborts_when_workspace_does_not_exist() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut bus = config.bus.subscribe();

        lazybox_server::spawn_handler::handle_spawn(
            &config,
            "test:missing-workspace".into(),
            None,
            TerminalKind::Agent("claude".into()),
            SpawnOptions::default(),
        )
        .await;

        assert!(
            mock.list().await.unwrap().is_empty(),
            "unknown workspace must not spawn a process"
        );
        let errors: Vec<String> = std::iter::from_fn(|| bus.try_recv().ok())
            .filter_map(|event| match event {
                Event::ProviderError { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert!(
            errors.iter().any(|message| {
                message.contains("unknown workspace") && message.contains("test:missing-workspace")
            }),
            "missing workspace should emit an actionable error: {errors:?}"
        );
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
                intent: TerminalInputIntent::Compose,
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
                intent: TerminalInputIntent::Submit,
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

/// #357: a `Done` agent handed a fresh prompt (Enter) resumes to
/// `Working`. For a hookless agent (Codex, Cursor) this optimistic flip is
/// the ONLY path out of `Done` — byte-flow `Working` can't clear it (a
/// stray repaint must not un-finish a turn) and there is no
/// `UserPromptSubmit` hook — so without it the pill would stay stuck on
/// `✓ done` through the whole next turn.
#[tokio::test]
async fn done_agent_resumes_working_on_a_fresh_prompt() {
    timeout(TEST_DEADLINE, async {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("codex".into())).await;

        // The agent finished a turn: its pill is Done.
        config
            .terminal
            .record_agent_state(terminal_id, lazybox_ipc::AgentState::Done)
            .await;

        // The user submits a new prompt (text + Enter) → a new turn.
        client
            .send(Command::Write {
                terminal_id,
                bytes: b"do more\r".to_vec(),
                intent: TerminalInputIntent::Submit,
            })
            .unwrap();
        let resumed = wait_for(
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
            resumed.is_some(),
            "a Done agent handed a fresh prompt must resume Working"
        );
    })
    .await
    .expect("deadline");
}

/// The other half: a BARE keystroke (no Enter) at a `Done` agent must NOT
/// resume Working — only a submitted line starts a new turn. A stray key
/// at a finished agent leaves the `✓` in place.
#[tokio::test]
async fn done_agent_ignores_a_bare_keystroke() {
    timeout(TEST_DEADLINE, async {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("codex".into())).await;
        config
            .terminal
            .record_agent_state(terminal_id, lazybox_ipc::AgentState::Done)
            .await;

        client
            .send(Command::Write {
                terminal_id,
                bytes: b"d".to_vec(),
                intent: TerminalInputIntent::Compose,
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
            "a bare keystroke at a Done agent must not resume Working"
        );
        assert_eq!(
            config.terminal.agent_state_for(terminal_id).await,
            Some(lazybox_ipc::AgentState::Done),
        );
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
                intent: TerminalInputIntent::Compose,
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
                intent: TerminalInputIntent::Compose,
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

        // A fresh spawn's very first output is boot, not work — so the agent
        // boots and settles at its composer first. Emitting the real
        // idle-composer render lets the pump recognize "ready" (latching the
        // state machine's booted flag) and classify the resting screen to
        // Idle. Only past boot does byte flow read as Working rather than
        // being held as boot chrome (#357).
        let idle_composer = include_bytes!("../../agents/tests/fixtures/idle_composer.bin");
        mock.emit(&key, idle_composer).await;
        tokio::time::sleep(Duration::from_secs(6)).await; // past the quiet window
        assert!(
            wait_for(
                &mut client,
                |e| matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Idle,
                        ..
                    }
                ),
                Duration::from_secs(2),
            )
            .await
            .is_some(),
            "the booted agent settles to Idle at its composer",
        );

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

/// The spawn-inject ladder's last rung: a detector-less agent (no
/// authoritative readiness signal, `requires_ready` false) whose PTY
/// produces NO output at all — so neither the ready rung nor the
/// first-output + settle rung can ever fire — still receives its work
/// prompt, pasted blindly when the 10s hard deadline elapses, rather
/// than losing it to a cold-start hang.
// Paused time: the 10s deadline rides tokio's auto-advance.
#[tokio::test(start_paused = true)]
async fn detectorless_spawn_prompt_pastes_blindly_at_the_hard_deadline() {
    timeout(Duration::from_secs(60), async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;

        const WORK: &str = "Fix the flaky login test.";
        client
            .send(Command::Spawn {
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:ws-deadline".into(),
                session_id: None,
                client_request_id: None,
                // cursor-agent inherits the line-oriented protocol:
                // best-effort readiness, no composer detector.
                kind: TerminalKind::Agent("cursor-agent".into()),
                cwd: test_cwd(),
                initial_prompt: Some(WORK.into()),
                on_main: false,
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("TerminalSpawned arrived");
        let key = mock.list().await.unwrap().into_iter().next().unwrap();

        // 8s in: still inside the deadline, and with zero output the
        // ready/settle rungs have no evidence — nothing may be written.
        tokio::time::sleep(Duration::from_secs(8)).await;
        assert!(
            mock.writes_for(&key).await.is_empty(),
            "prompt written before the hard deadline with no readiness evidence"
        );

        // Crossing the 10s deadline delivers the blind paste.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let joined = mock
                .writes_for(&key)
                .await
                .into_iter()
                .flatten()
                .collect::<Vec<u8>>();
            if String::from_utf8_lossy(&joined).contains(WORK) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "hard-deadline rung never pasted the prompt; writes = {:?}",
                String::from_utf8_lossy(&joined)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("deadline");
}

async fn spawn_idle_codex(
    client: &mut lazybox_ipc::Client,
    mock: &MockBackend,
) -> (lazybox_ipc::TerminalId, String) {
    let terminal_id = spawn_and_wait(client, TerminalKind::Agent("codex".into())).await;
    let key = mock.list().await.unwrap().into_iter().next().unwrap();
    mock.emit(
        &key,
        include_bytes!("../../agents/tests/fixtures/codex_real_idle.bin"),
    )
    .await;
    assert!(
        wait_for(
            client,
            |event| matches!(
                event,
                Event::AgentState {
                    state: lazybox_ipc::AgentState::Idle,
                    ..
                }
            ),
            Duration::from_secs(10),
        )
        .await
        .is_some(),
        "Codex must boot to Idle before the turn starts",
    );
    (terminal_id, key)
}

async fn wait_for_output_count(emitted: &AtomicUsize, minimum: usize) {
    for _ in 0..5_000 {
        if emitted.load(Ordering::Relaxed) >= minimum {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "continuously-ready producer emitted only {} of {minimum} chunks",
        emitted.load(Ordering::Relaxed),
    );
}

/// Identical Codex repaints re-arm byte silence but cannot starve the
/// content-stability watchdog when the PTY receiver stays continuously ready.
#[tokio::test(start_paused = true)]
async fn continuously_ready_codex_repaints_do_not_starve_the_working_watchdog() {
    timeout(Duration::from_secs(120), async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let (terminal_id, key) = spawn_idle_codex(&mut client, &mock).await;

        const FROZEN_FRAME: &str = "• Working alpha (1s · esc to interrupt)";
        client
            .send(Command::IngestHook {
                terminal_id,
                hook: hook(lazybox_ipc::HookEventKind::UserPromptSubmit),
                backend_key: Some(key.clone()),
            })
            .unwrap();
        assert!(
            wait_for(
                &mut client,
                |e| matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                ),
                Duration::from_secs(2),
            )
            .await
            .is_some(),
            "the Codex turn must enter Working",
        );
        mock.emit(&key, FROZEN_FRAME).await;
        assert!(
            wait_for(
                &mut client,
                |event| matches!(
                    event,
                    Event::TerminalOutput {
                        terminal_id: id,
                        ..
                    } if *id == terminal_id
                ),
                Duration::from_secs(2),
            )
            .await
            .is_some(),
            "the first Working frame must reach the pump",
        );

        while client.rx.try_recv().is_ok() {}
        let working_started = tokio::time::Instant::now();
        let emitted = Arc::new(AtomicUsize::new(0));
        let producer = {
            let mock = mock.clone();
            let key = key.clone();
            let emitted = emitted.clone();
            tokio::spawn(async move {
                loop {
                    mock.emit_backpressured(&key, FROZEN_FRAME).await;
                    emitted.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        wait_for_output_count(
            &emitted,
            lazybox_server::backend::SUBSCRIPTION_CHANNEL_CAPACITY * 2,
        )
        .await;

        tokio::time::advance(Duration::from_secs(14)).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            config.terminal.agent_state_for(terminal_id).await,
            Some(lazybox_ipc::AgentState::Working),
            "the watchdog fired before its configured 15-second bound",
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..2_048 {
            if config.terminal.agent_state_for(terminal_id).await
                == Some(lazybox_ipc::AgentState::Done)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        let state_at_deadline = config.terminal.agent_state_for(terminal_id).await;
        let elapsed_at_transition = working_started.elapsed();
        producer.abort();
        let _ = producer.await;
        assert!(
            elapsed_at_transition <= Duration::from_secs(15),
            "the watchdog exceeded its configured bound: {:?}",
            elapsed_at_transition,
        );
        assert_eq!(
            state_at_deadline,
            Some(lazybox_ipc::AgentState::Done),
            "continuous repaint traffic starved the watchdog",
        );

        assert!(
            wait_for(
                &mut client,
                |event| matches!(
                    event,
                    Event::AgentState {
                        terminal_id: id,
                        state: lazybox_ipc::AgentState::Done,
                        ..
                    } if *id == terminal_id
                ),
                Duration::from_secs(2),
            )
            .await
            .is_some(),
            "the pty-watchdog-force transition was not broadcast",
        );
        assert!(
            wait_for(
                &mut client,
                |event| matches!(
                    event,
                    Event::AgentState {
                        terminal_id: id,
                        state: lazybox_ipc::AgentState::Done,
                        ..
                    } if *id == terminal_id
                ),
                Duration::from_millis(100),
            )
            .await
            .is_none(),
            "the pty-watchdog-force path emitted Done more than once",
        );
    })
    .await
    .expect("deadline");
}

/// Meaningful chunks are the control case: keeping the receiver ready is not
/// itself grounds to settle. Each changed fingerprint must advance the
/// watchdog anchor and keep Codex Working across multiple watchdog windows.
#[tokio::test(start_paused = true)]
async fn continuously_ready_meaningful_codex_output_advances_the_watchdog_anchor() {
    timeout(Duration::from_secs(120), async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let (terminal_id, key) = spawn_idle_codex(&mut client, &mock).await;

        const FRAME_A: &str = "• Working alpha (1s · esc to interrupt)";
        const FRAME_B: &str = "• Working beta (2s · esc to interrupt)";
        mock.emit(&key, FRAME_A).await;
        assert!(
            wait_for(
                &mut client,
                |e| matches!(
                    e,
                    Event::AgentState {
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    }
                ),
                Duration::from_secs(2),
            )
            .await
            .is_some(),
        );

        let emitted = Arc::new(AtomicUsize::new(0));
        let producer = {
            let mock = mock.clone();
            let key = key.clone();
            let emitted = emitted.clone();
            tokio::spawn(async move {
                let mut frame = FRAME_B;
                loop {
                    mock.emit_backpressured(&key, frame).await;
                    emitted.fetch_add(1, Ordering::Relaxed);
                    frame = if frame == FRAME_A { FRAME_B } else { FRAME_A };
                }
            })
        };
        wait_for_output_count(
            &emitted,
            lazybox_server::backend::SUBSCRIPTION_CHANNEL_CAPACITY * 2,
        )
        .await;

        for _ in 0..3 {
            let before = emitted.load(Ordering::Relaxed);
            tokio::time::advance(Duration::from_secs(14)).await;
            wait_for_output_count(&emitted, before + 1).await;
            assert_eq!(
                config.terminal.agent_state_for(terminal_id).await,
                Some(lazybox_ipc::AgentState::Working),
                "changed content failed to advance the watchdog anchor",
            );
        }

        producer.abort();
        let _ = producer.await;
    })
    .await
    .expect("deadline");
}

/// Output already queued when the watchdog deadline becomes due precedes the
/// watchdog decision. If that bounded batch contains meaningful progress, it
/// must advance the anchor instead of allowing stale state to settle first.
#[tokio::test(start_paused = true)]
async fn queued_meaningful_output_at_the_watchdog_deadline_prevents_a_stale_done() {
    timeout(Duration::from_secs(120), async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config.clone()).await;
        let (terminal_id, key) = spawn_idle_codex(&mut client, &mock).await;

        const FROZEN_FRAME: &str = "• Working alpha (1s · esc to interrupt)";
        const PROGRESS_FRAME: &str = "• Working beta (2s · esc to interrupt)";
        mock.emit(&key, FROZEN_FRAME).await;
        assert!(
            wait_for(
                &mut client,
                |event| matches!(
                    event,
                    Event::AgentState {
                        terminal_id: id,
                        state: lazybox_ipc::AgentState::Working,
                        ..
                    } if *id == terminal_id
                ),
                Duration::from_secs(2),
            )
            .await
            .is_some(),
        );
        while client.rx.try_recv().is_ok() {}

        tokio::time::advance(Duration::from_millis(14_999)).await;
        for _ in 0..32 {
            mock.emit(&key, FROZEN_FRAME).await;
        }
        mock.emit(&key, PROGRESS_FRAME).await;
        tokio::time::advance(Duration::from_millis(1)).await;

        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            config.terminal.agent_state_for(terminal_id).await,
            Some(lazybox_ipc::AgentState::Working),
            "the watchdog settled stale state before queued meaningful output",
        );
        assert!(
            wait_for(
                &mut client,
                |event| matches!(
                    event,
                    Event::AgentState {
                        terminal_id: id,
                        state: lazybox_ipc::AgentState::Done,
                        ..
                    } if *id == terminal_id
                ),
                Duration::from_millis(100),
            )
            .await
            .is_none(),
            "queued progress was overtaken by a stale Done transition",
        );
    })
    .await
    .expect("deadline");
}

/// Minimal GitHub `Task` for the collapse test.
fn collapse_task(key: &str, url: &str, closes: Vec<lazybox_core::TaskId>) -> lazybox_core::Task {
    lazybox_core::Task {
        author: String::new(),
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
        closes_issues: closes,
        linked_tasks: vec![],
        priority: None,
        state_label: None,
    }
}

/// Issue #78 regression: the manual `x j` collapse (`Command::
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
        let mut issue = lazybox_core::Workspace::from_task(
            collapse_task("o/r#50", "https://github.com/o/r/issues/50", vec![]),
            chrono::Utc::now(),
        );
        let issue_task_id = issue.primary_task().unwrap().id.clone();
        let issue_key = issue.key.clone();
        // This test owns the collapse/rebadge contract, not worktree
        // provisioning. Seed an existing local session so it never
        // clones the fake `o/r` remote or depends on network latency,
        // credentials, and the developer's global git configuration.
        // The seeded dir must be a real checkout: the spawn path now
        // validates the worktree (a bare/empty dir would be routed
        // through re-provisioning — and the fake remote — instead of
        // being trusted).
        let worktree = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg("-q")
                .current_dir(worktree.path())
                .status()
                .unwrap()
                .success(),
            "git init the seeded worktree"
        );
        issue.add_session(lazybox_core::WorkspaceSession::new(
            issue_key.clone(),
            lazybox_core::SessionKind::Agent {
                agent_id: "claude".into(),
            },
            worktree.path().to_path_buf(),
            chrono::Utc::now(),
        ));
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
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: issue_key.as_str().into(),
                session_id: None,
                client_request_id: None,
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

        // Sanity: spawning reused the persisted session rather than
        // manufacturing a second one before the collapse.
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
            "spawn must reuse the issue workspace's persisted session",
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
        let (sk, _) = config
            .terminal
            .terminal_meta_for(terminal_id)
            .await
            .expect("terminal still tracked");
        assert_eq!(
            sk, pr_session_key,
            "terminal_meta must rebadge the live terminal onto the PR",
        );

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
                    SpawnOptions {
                        cwd: Some(cwd),
                        initial_prompt: Some(prompt),
                        ..Default::default()
                    },
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

/// `Command::FetchScrollback` round-trips the backend's deep history
/// (#393): the daemon resolves the terminal's backend key, asks the
/// backend for its retained scrollback — for tmux, the same
/// capture-pane seed the restart path uses — and replies with
/// `Event::TerminalScrollback` carrying the live stream's seq
/// high-water mark, so the client can rebuild a live session's grid
/// as deep as a restarted one.
#[tokio::test]
async fn fetch_scrollback_round_trips_backend_history() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Agent("claude".into())).await;
        let key = mock.list().await.unwrap().into_iter().next().unwrap();
        mock.emit(&key, b"live chunk").await;
        mock.set_deep_scrollback(&key, b"deep history\r\nlive chunk")
            .await;

        client
            .send(Command::FetchScrollback { terminal_id })
            .unwrap();
        let ev = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalScrollback { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("TerminalScrollback reply");
        match ev {
            Event::TerminalScrollback {
                terminal_id: id,
                replay,
                seq,
            } => {
                assert_eq!(id, terminal_id);
                assert_eq!(replay, b"deep history\r\nlive chunk".to_vec());
                assert_eq!(seq, 1, "seq is the live high-water mark at capture time");
            }
            _ => unreachable!(),
        }
    })
    .await
    .expect("deadline");
}

/// A backend/terminal with no history source beyond the ring (raw PTY,
/// or a session tmux has nothing for) answers a fetch with silence —
/// the client's ring-fed scrollback is already everything there is.
#[tokio::test]
async fn fetch_scrollback_without_history_source_is_silent() {
    timeout(TEST_DEADLINE, async {
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Shell).await;

        // No `set_deep_scrollback` → the backend reports `None`.
        client
            .send(Command::FetchScrollback { terminal_id })
            .unwrap();
        let ev = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalScrollback { .. }),
            Duration::from_millis(300),
        )
        .await;
        assert!(ev.is_none(), "no history source must reply nothing: {ev:?}");
    })
    .await
    .expect("deadline");
}

/// Startup recovery runs under a wall-clock bound in the TUI, and the
/// timeout cancels `recover_sessions` MID-LOOP — backend sessions it
/// hadn't registered yet stay alive but absent from `terminal_meta`.
/// `restore_persisted_sessions` used to dedupe against `terminal_meta`
/// alone, so it spawned a SECOND agent into the same worktree beside
/// the surviving (unregistered) backend session. It must fold the
/// backend's own listing into the dedupe set: a live backend session
/// for key K must never coexist with a fresh spawn for K.
#[tokio::test]
async fn restore_skips_live_but_unregistered_backend_sessions() {
    let _home = IsolatedConfigHome::new();
    timeout(Duration::from_secs(20), async {
        let (config, mock) = ServerConfig::in_memory_with_mock();

        // A persisted workspace with one agent session record.
        let ws_key = lazybox_core::WorkspaceKey::new("test:restore-live");
        let tmp = tempfile::TempDir::new().unwrap();
        let mut ws = lazybox_core::Workspace::empty(ws_key.clone(), "main", chrono::Utc::now());
        let session = lazybox_core::WorkspaceSession::new(
            ws_key.clone(),
            lazybox_core::SessionKind::Agent {
                agent_id: "claude".into(),
            },
            tmp.path().join("wt"),
            chrono::Utc::now(),
        );
        let session_id = session.id;
        ws.add_session(session);
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: ws_key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).unwrap()),
            })
            .unwrap();

        // Spawn it for real once — this persists the terminal meta the
        // way a live daemon does.
        lazybox_server::spawn_handler::handle_spawn(
            &config,
            "test:restore-live".into(),
            Some(session_id),
            TerminalKind::Agent("claude".into()),
            SpawnOptions::default(),
        )
        .await;
        assert_eq!(
            mock.list().await.unwrap().len(),
            1,
            "sanity: one live backend session"
        );

        // Simulate the restart whose recovery got cancelled before
        // registering this session: the in-memory maps are empty, the
        // backend session and its persisted meta survive.
        let restarted =
            ServerConfig::with_store_and_backend(config.store.clone(), config.backend.clone());
        lazybox_server::spawn_handler::restore_persisted_sessions(&restarted).await;
        assert_eq!(
            mock.list().await.unwrap().len(),
            1,
            "restore must not spawn a second agent beside the live unregistered session"
        );
        assert!(
            restarted.terminal.terminal_metadata().await.is_empty(),
            "nothing was registered by the skipped restore"
        );

        // Control: once the backend survivor no longer maps to this
        // record (its persisted meta is gone — the mock backend keeps
        // exited sessions listed, unlike tmux, so drop the attribution
        // instead), the same record IS restored — proving this harness
        // detects a spawn.
        let backend_key = mock.list().await.unwrap().into_iter().next().unwrap();
        restarted
            .store
            .delete_kv(&format!("terminal:{backend_key}"))
            .unwrap();
        lazybox_server::spawn_handler::restore_persisted_sessions(&restarted).await;
        assert!(
            !restarted.terminal.terminal_metadata().await.is_empty(),
            "restore spawns normally once no live backend session maps to the record"
        );
    })
    .await
    .expect("deadline");
}

/// A spawn whose worktree provision fails must fail LOUDLY — a
/// `spawn:worktree` provider error, no terminal — and leave nothing
/// spawnable behind. The old fallback `mkdir`'d an empty dir,
/// persisted the session, and opened the terminal in a non-git folder;
/// every later spawn then short-circuited into it forever.
#[tokio::test]
async fn failed_provision_fails_spawn_loudly_and_leaves_no_session() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();

        // A task whose repo can't be provisioned (not `owner/name`)
        // fails deterministically before any git or network work.
        let mut task = collapse_task("o/r#60", "https://github.com/o/r/issues/60", vec![]);
        task.repo = Some("not-owner-name-format".into());
        let ws = lazybox_core::Workspace::from_task(task, chrono::Utc::now());
        let ws_key = ws.key.clone();
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: ws_key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).unwrap()),
            })
            .unwrap();

        let mut client = subscribed(config.clone()).await;
        client
            .send(Command::Spawn {
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: ws_key.as_str().into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();

        let error = wait_for(
            &mut client,
            |e| matches!(e, Event::ProviderError { source, .. } if source == "spawn:worktree"),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            error.is_some(),
            "the failed provision must surface as a spawn:worktree provider error"
        );

        assert!(
            mock.list().await.unwrap().is_empty(),
            "no terminal may be spawned into an unprovisioned workspace"
        );
        let after: lazybox_core::Workspace = serde_json::from_str(
            &config
                .store
                .get_workspace(&ws_key)
                .unwrap()
                .unwrap()
                .workspace_json
                .unwrap(),
        )
        .unwrap();
        assert!(
            after.sessions.is_empty(),
            "no session may be persisted for a worktree that was never provisioned"
        );
    })
    .await
    .expect("deadline");
}
