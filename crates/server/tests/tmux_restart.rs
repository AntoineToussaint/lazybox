//! Restart-recovery scrollback against a REAL tmux server (#306).
//!
//! The daemon's replay ring is in-memory and dies with the daemon; tmux's
//! per-pane history survives. A reattaching client must be seeded from
//! that history (`capture-pane`) or a restarted daemon serves exactly one
//! screenful — nothing to scroll back through, which read as "scrolling
//! is broken" on every recovered session.
//!
//! Skipped (pass, with a note) when tmux is missing or older than the
//! backend's minimum — the same gate `TmuxBackend::detect()` applies,
//! so the test only runs where the backend would actually engage.

use lazybox_core::SessionKey;
use lazybox_ipc::{TerminalId, TerminalKind, TerminalSnapshot};
use lazybox_server::ServerConfig;
use lazybox_server::backend::tmux::modern_tmux_version;
use lazybox_server::backend::{SessionBackend, TmuxBackend};
use lazybox_server::spawn_handler::{handle_record_composing_buffer, snapshot_terminals};
use lazybox_store::MemoryStore;
use libghostty_vt::terminal::{Point, PointCoordinate};
use libghostty_vt::{Terminal, TerminalOptions, screen::Screen};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const TEST_DEADLINE: Duration = Duration::from_secs(30);

fn kill_test_server(socket: &str) {
    let _ = std::process::Command::new("tmux")
        .args(["-L", socket, "kill-server"])
        .output();
}

fn active_rows(replay: &[u8], cols: u16, rows: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TerminalOptions {
        cols,
        rows,
        max_scrollback: 10_000,
    })
    .expect("terminal");
    terminal.vt_write(replay);
    (0..rows)
        .map(|y| {
            let mut text = String::new();
            for x in 0..cols {
                let cell = terminal
                    .grid_ref(Point::Active(PointCoordinate { x, y: y.into() }))
                    .expect("active cell");
                let mut graphemes = ['\0'; 16];
                let len = cell.graphemes(&mut graphemes).expect("graphemes");
                text.extend(&graphemes[..len]);
            }
            text.trim_end().to_string()
        })
        .collect()
}

struct PromptCase {
    terminal_id: TerminalId,
    backend_key: String,
    session_key: SessionKey,
    agent: String,
    marker: String,
    draft: String,
    expected_rows: Vec<String>,
}

async fn register_prompt_cases(config: &ServerConfig, cases: &[PromptCase]) {
    let mut terminals = config.terminals.lock().await;
    let mut terminal_meta = config.terminal_meta.lock().await;
    for case in cases {
        terminals.insert(case.terminal_id, case.backend_key.clone());
        terminal_meta.insert(
            case.terminal_id,
            (
                case.session_key.clone(),
                TerminalKind::Agent(case.agent.clone()),
            ),
        );
    }
}

fn assert_prompt_snapshots(cases: &[PromptCase], snapshots: &[TerminalSnapshot], transition: &str) {
    assert_eq!(
        snapshots.len(),
        cases.len(),
        "{transition}: every tmux session must have a snapshot"
    );
    for case in cases {
        let snapshot = snapshots
            .iter()
            .find(|snapshot| snapshot.terminal_id == case.terminal_id)
            .expect("case snapshot");
        assert_eq!(
            snapshot.composing_buffer.as_deref(),
            Some(case.draft.as_str()),
            "{transition}: persisted draft changed for {}",
            case.marker
        );
        let actual_rows = active_rows(&snapshot.replay, 120, 32);
        assert_eq!(
            actual_rows,
            case.expected_rows,
            "{transition}: reconstructed composer changed for {}; replay tail: {:?}",
            case.marker,
            String::from_utf8_lossy(
                &snapshot.replay[snapshot.replay.len().saturating_sub(1_000)..]
            ),
        );
    }
}

/// Live-session counterpart of the restart test (#393): the SAME
/// capture-pane history must be reachable while the original backend —
/// and its attach client — are still running. Before the fix only the
/// restart path read it, so a live full-screen agent scrolled back a
/// few lines while tmux silently held thousands.
#[tokio::test]
async fn live_backend_serves_deep_scrollback_without_restart() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping live-scrollback test");
        return;
    }
    let socket = format!("lazybox-test-live-scrollback-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        // Same output-then-park shape as the restart test (see its
        // comment for why the output comes from the command, not a
        // `write` to an interactive shell).
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "for i in $(seq 1 200); do echo line-$i; done; exec sleep 300".to_string(),
                ],
                None,
                &[],
                "live-scrollback-test",
            )
            .await
            .expect("tmux spawn");

        // Wait until the full output reached tmux (observed through the
        // live attach client, the same byte path the TUI consumes).
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        for attempt in 0.. {
            ticker.tick().await;
            let sub = backend.subscribe(&key).await.expect("subscribe");
            if String::from_utf8_lossy(&sub.replay).contains("line-200") {
                break;
            }
            assert!(
                attempt < 100,
                "pane output never reached the attach client; last replay \
                 ({} bytes): {:?}",
                sub.replay.len(),
                String::from_utf8_lossy(&sub.replay[sub.replay.len().saturating_sub(600)..]),
            );
        }

        // NO restart — ask the live backend for the deep scrollback.
        let (replay, seq) = backend
            .scrollback(&key)
            .await
            .expect("scrollback")
            .expect("tmux session must have history to serve");
        let replay = String::from_utf8_lossy(&replay);
        // line-5 scrolled out of the 32-row pane long ago — on a live
        // session it can only come from the capture-pane history, which
        // is exactly what a restarted backend would have seeded. The
        // `\r` pins the exact line (capture lines are CRLF-joined).
        assert!(
            replay.contains("line-5\r"),
            "live scrollback must reach deep history, got {} bytes: {:?}…",
            replay.len(),
            &replay[..replay.len().min(400)],
        );
        assert!(
            replay.contains("line-200"),
            "live scrollback must reach the most recent output",
        );
        assert!(
            seq > 0,
            "capture must report the live stream's high-water mark"
        );

        let _ = backend.kill(&key).await;
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

#[tokio::test]
async fn capture_history_preserves_soft_wrapped_logical_lines() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping soft-wrap capture test");
        return;
    }
    let socket = format!("lazybox-test-soft-wrap-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf 'WRAP-START'; printf '%*s' 220 '' | tr ' ' x; \
                     printf 'WRAP-END\\n'; for i in $(seq 1 100); do echo line-$i; done; \
                     exec sleep 300"
                        .to_string(),
                ],
                None,
                &[],
                "soft-wrap-test",
            )
            .await
            .expect("tmux spawn");

        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        for attempt in 0.. {
            ticker.tick().await;
            let sub = backend.subscribe(&key).await.expect("subscribe");
            if String::from_utf8_lossy(&sub.replay).contains("line-100") {
                break;
            }
            assert!(attempt < 100, "pane output never completed");
        }

        let (capture, _) = backend
            .scrollback(&key)
            .await
            .expect("scrollback")
            .expect("history source");
        let expected = format!("WRAP-START{}WRAP-END", "x".repeat(220));
        assert!(
            String::from_utf8_lossy(&capture).contains(&expected),
            "capture-pane must join soft-wrapped rows back into one logical line"
        );

        let _ = backend.kill(&key).await;
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

#[tokio::test]
async fn restarted_backend_seeds_scrollback_from_tmux_history() {
    if modern_tmux_version().is_none() {
        // Skip-as-pass is how this path could go silently unexercised on
        // a runner without tmux; the nightly lane closes that hole by
        // demanding the real thing (#410).
        assert!(
            std::env::var("LAZYBOX_E2E_REQUIRE").map(|v| v == "1") != Ok(true),
            "LAZYBOX_E2E_REQUIRE=1 but tmux is unavailable"
        );
        eprintln!("tmux missing or too old — skipping restart-recovery test");
        return;
    }
    let socket = format!("lazybox-test-restart-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        // "First launch": spawn a session that fills >1 screen of output and
        // then parks on `sleep`. The output comes from the command itself
        // so the fixture has no shell-prompt timing in the scrollback
        // assertion; the prompt-transition matrix below separately pins
        // that dropping the tmux relay sends no input into the pane.
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "for i in $(seq 1 200); do echo line-$i; done; exec sleep 300".to_string(),
                ],
                None,
                &[],
                "restart-test",
            )
            .await
            .expect("tmux spawn");

        // Wait until the full output is in tmux's pane history. Polling
        // the subscription replay (not capture-pane) exercises the same
        // byte path the TUI consumes. Bounded with its own diagnostic:
        // when the attach client streams no pane content at all (e.g.
        // the tmux-3.2a conf-error view regression), the replay tail
        // shows WHAT the client rendered instead of a bare timeout.
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        for attempt in 0.. {
            ticker.tick().await;
            let sub = backend.subscribe(&key).await.expect("subscribe");
            if String::from_utf8_lossy(&sub.replay).contains("line-200") {
                break;
            }
            assert!(
                attempt < 100,
                "pane output never reached the attach client; last replay \
                 ({} bytes): {:?}",
                sub.replay.len(),
                String::from_utf8_lossy(&sub.replay[sub.replay.len().saturating_sub(600)..]),
            );
        }

        // "Daemon restart": the old backend (and with it every DaemonPty
        // replay ring) is dropped; the tmux server and its history live
        // on. A brand-new backend on the same socket rediscovers the
        // session with an empty client map — the recovery path.
        drop(backend);
        let restarted = TmuxBackend::with_socket(&socket).expect("conf written");
        let keys = restarted.list().await.expect("list sessions");
        assert!(
            keys.contains(&key),
            "session must survive the restart: {keys:?}"
        );

        let sub = restarted.subscribe(&key).await.expect("re-subscribe");
        let replay = String::from_utf8_lossy(&sub.replay);
        // line-5 scrolled out of the visible pane long ago (200 lines on
        // a 32-row window) — it can only come from the capture-pane seed.
        // The `\r` pins the exact line (seeded lines are CRLF-joined) so
        // line-50/line-150 can't satisfy the assert.
        assert!(
            replay.contains("line-5\r"),
            "replay must contain pre-restart history, got {} bytes: {:?}…",
            sub.replay.len(),
            &replay[..replay.len().min(400)],
        );
        assert!(
            replay.contains("line-200"),
            "replay must reach the most recent output",
        );

        let _ = restarted.kill(&key).await;
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

#[tokio::test]
async fn tmux_prompt_transition_matrix_is_byte_faithful() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping prompt transition matrix");
        return;
    }
    let socket = format!("lazybox-test-composer-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let store = Arc::new(MemoryStore::new());
        let mut backend = Arc::new(TmuxBackend::with_socket(&socket).expect("conf written"));
        let cases = [
            ("claude-single", "CLAUDE", "single-line-draft"),
            ("claude-multi", "CLAUDE", "first-line\n  second-line"),
            ("codex-single", "CODEX", "single-line-draft"),
            ("codex-multi", "CODEX", "first-line\n  second-line"),
        ];
        let mut prompt_cases = Vec::new();
        for (index, (name, agent, draft)) in cases.into_iter().enumerate() {
            let marker = format!("MARKER-{name}");
            let rendered_draft = draft.replace('\n', "\\r\\n");
            let script = format!(
                "for i in $(seq 1 40); do echo history-$i; done; \
                 printf '\\033[2J\\033[H{marker}\\r\\n{agent}\\r\\n\\r\\n> {rendered_draft}'; \
                 exec sleep 300"
            );
            let key = backend
                .spawn(
                    &["/bin/sh".to_string(), "-c".to_string(), script],
                    None,
                    &[],
                    name,
                )
                .await
                .expect("tmux spawn");
            let mut before = Vec::new();
            for _ in 0..100 {
                before = backend.snapshot(&key).await.expect("snapshot").replay;
                if String::from_utf8_lossy(&before).contains(&marker) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(
                String::from_utf8_lossy(&before).contains(&marker),
                "initial composer never rendered"
            );
            prompt_cases.push(PromptCase {
                terminal_id: TerminalId(index as u64 + 1),
                backend_key: key,
                session_key: SessionKey::new(format!("test:issue-{index}")),
                agent: agent.to_ascii_lowercase(),
                marker,
                draft: draft.to_string(),
                expected_rows: active_rows(&before, 120, 32),
            });
        }

        let mut config = ServerConfig::with_store_and_backend(store.clone(), backend.clone());
        register_prompt_cases(&config, &prompt_cases).await;
        for case in &prompt_cases {
            handle_record_composing_buffer(&config, case.terminal_id, &case.draft).await;
        }

        let initial = snapshot_terminals(&config).await;
        assert_prompt_snapshots(&prompt_cases, &initial, "initial");

        for cycle in 0..4 {
            let snapshots = snapshot_terminals(&config).await;
            assert_prompt_snapshots(
                &prompt_cases,
                &snapshots,
                &format!("client reattach cycle {cycle}"),
            );
        }

        for cycle in 0..4 {
            {
                let mut meta = config.terminal_meta.lock().await;
                for (index, case) in prompt_cases.iter_mut().enumerate() {
                    case.session_key = SessionKey::new(format!("test:pr-{index}-{cycle}"));
                    meta.get_mut(&case.terminal_id)
                        .expect("terminal metadata")
                        .0 = case.session_key.clone();
                }
            }
            let snapshots = snapshot_terminals(&config).await;
            assert_prompt_snapshots(
                &prompt_cases,
                &snapshots,
                &format!("TerminalsRebadged cycle {cycle}"),
            );
        }

        for cycle in 0..4 {
            drop(config);
            drop(backend);
            tokio::time::sleep(Duration::from_millis(50)).await;

            backend = Arc::new(TmuxBackend::with_socket(&socket).expect("conf written"));
            config = ServerConfig::with_store_and_backend(store.clone(), backend.clone());
            register_prompt_cases(&config, &prompt_cases).await;

            let mut subscriptions = Vec::new();
            for case in &prompt_cases {
                subscriptions.push(
                    backend
                        .subscribe(&case.backend_key)
                        .await
                        .expect("reattach surviving tmux session"),
                );
            }

            let mut snapshots = Vec::new();
            for attempt in 0..100 {
                snapshots = snapshot_terminals(&config).await;
                let settled = snapshots.len() == prompt_cases.len()
                    && snapshots.iter().all(|snapshot| {
                        snapshot.last_seq > 1
                            && prompt_cases.iter().any(|case| {
                                case.terminal_id == snapshot.terminal_id
                                    && String::from_utf8_lossy(&snapshot.replay)
                                        .contains(&case.marker)
                            })
                    });
                if settled {
                    break;
                }
                assert!(attempt < 99, "tmux live repaint never settled");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_prompt_snapshots(
                &prompt_cases,
                &snapshots,
                &format!("daemon restart cycle {cycle}"),
            );
            drop(subscriptions);
            tokio::task::yield_now().await;
        }

        for case in &prompt_cases {
            let _ = backend.kill(&case.backend_key).await;
        }
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

/// Reattach after a daemon restart must land the fresh client at the
/// pane's REAL viewport size, not the hardcoded default grid. The
/// surviving pane still carries the size its last client set; attaching
/// at a smaller default reflows the pane down and then the widget's
/// first-render resize reflows it back up — a resize storm that makes
/// Claude Code re-anchor its input box via an incremental primary-screen
/// redraw and leaves a blank gap above it (#533). Matching the pane size
/// on attach means tmux reports no resize, so no reflow and no gap.
#[tokio::test]
async fn reattach_matches_pane_viewport_no_default_reflow() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping reattach-size test");
        return;
    }
    let socket = format!("lazybox-test-reattach-size-{}", std::process::id());
    // A viewport distinct from the DEFAULT 120x32 the backend attaches a
    // fresh spawn at, so a reflow back to the default is observable.
    const VIEW_COLS: u16 = 100;
    const VIEW_ROWS: u16 = 45;
    let pane_rows = |socket: &str, key: &str| -> u16 {
        let out = std::process::Command::new("tmux")
            .args([
                "-L",
                socket,
                "display-message",
                "-p",
                "-t",
                key,
                "#{pane_height}",
            ])
            .output()
            .expect("display-message");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("pane height")
    };
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "for i in $(seq 1 200); do echo line-$i; done; exec sleep 300".to_string(),
                ],
                None,
                &[],
                "reattach-size-test",
            )
            .await
            .expect("tmux spawn");

        // The widget learns its true size on first render and resizes the
        // pane to the real viewport — simulate that here so the surviving
        // pane carries a non-default size into the restart.
        backend
            .resize(&key, VIEW_COLS, VIEW_ROWS)
            .await
            .expect("resize to viewport");
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        for attempt in 0.. {
            ticker.tick().await;
            if pane_rows(&socket, &key) == VIEW_ROWS {
                break;
            }
            assert!(
                attempt < 100,
                "pane never reflowed to the viewport size; got {} rows",
                pane_rows(&socket, &key)
            );
        }

        // "Daemon restart": drop the backend (and its attach client). The
        // tmux session and its pane size live on.
        drop(backend);
        let restarted = TmuxBackend::with_socket(&socket).expect("conf written");
        assert_eq!(
            pane_rows(&socket, &key),
            VIEW_ROWS,
            "detached pane must retain its viewport size across the restart"
        );

        // Reattach. The fresh client must attach at the pane's real size,
        // so the pane never reflows through the default 32-row grid. The
        // attach-driven resize (if any) lands a beat after `subscribe`
        // returns, so watch the pane for a settling window: it must stay
        // at the viewport the whole time, never dropping to the default.
        restarted.subscribe(&key).await.expect("re-subscribe");
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                pane_rows(&socket, &key),
                VIEW_ROWS,
                "reattach must match the pane viewport, not reflow to the \
                 default 32-row grid"
            );
        }

        let _ = restarted.kill(&key).await;
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

/// The root cause of the "scrollbar disappears / scrollback dead on
/// Claude" report: Claude Code ≥2.1 switches its pane to the alternate
/// screen, which retains ZERO tmux history — the source every lazybox
/// scrollback mechanism reads. The conf now denies the alt screen at
/// the pane level, so a program that asks for it (this test does
/// exactly what Claude does: smcup, then output) keeps writing on the
/// primary screen and its output lands in retained history.
#[tokio::test]
async fn alt_screen_request_is_denied_so_agent_history_is_retained() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping alt-screen denial test");
        return;
    }
    let socket = format!("lazybox-test-altdeny-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    // smcup first — the Claude ≥2.1 shape — then enough
                    // output to scroll well past the pane height.
                    "printf '\\033[?1049h'; for i in $(seq 1 200); do echo line-$i; done; \
                     exec sleep 300"
                        .to_string(),
                ],
                None,
                &[],
                "altdeny-test",
            )
            .await
            .expect("tmux spawn");

        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        for attempt in 0.. {
            ticker.tick().await;
            let sub = backend.subscribe(&key).await.expect("subscribe");
            if String::from_utf8_lossy(&sub.replay).contains("line-200") {
                break;
            }
            assert!(attempt < 100, "pane output never reached the attach client");
        }

        let state = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "display-message",
                "-p",
                "-t",
                &key,
                "#{alternate_on} #{history_size}",
            ])
            .output()
            .expect("display-message");
        let state = String::from_utf8_lossy(&state.stdout).trim().to_string();
        let mut parts = state.split(' ');
        assert_eq!(
            parts.next(),
            Some("0"),
            "the pane must stay OFF the alternate screen (state: {state})"
        );
        let history: u64 = parts.next().unwrap().parse().unwrap();
        assert!(
            history > 100,
            "output must land in retained history, got {history} lines"
        );

        // And the live deep-scrollback fetch serves it.
        let (replay, _seq) = backend
            .scrollback(&key)
            .await
            .expect("scrollback")
            .expect("denied-alt pane must have history to serve");
        let replay = String::from_utf8_lossy(&replay);
        assert!(
            replay.contains("line-5\r"),
            "fetch must reach deep history despite the smcup request"
        );

        let _ = backend.kill(&key).await;
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

/// A pane that IS on the alternate screen (spawned under a pre-fix
/// server config that still allowed it) retains no history; the fetch
/// must return None rather than hand the client a one-screen capture —
/// adopting that capture is what wiped the local grid and made the
/// scrollbar disappear on the first scroll.
#[tokio::test]
async fn alt_screen_pane_serves_no_deep_scrollback() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping alt-screen fetch test");
        return;
    }
    let socket = format!("lazybox-test-altfetch-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    // Give the test a beat to re-allow the alt screen
                    // (simulating the pre-fix server config) before the
                    // program requests it.
                    "sleep 1; printf '\\033[?1049h'; echo on-alt; exec sleep 300".to_string(),
                ],
                None,
                &[],
                "altfetch-test",
            )
            .await
            .expect("tmux spawn");
        let allow = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "set-option",
                "-w",
                "-t",
                &key,
                "alternate-screen",
                "on",
            ])
            .output()
            .expect("set-option");
        assert!(allow.status.success(), "re-allow alternate-screen");

        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        for attempt in 0.. {
            ticker.tick().await;
            let out = std::process::Command::new("tmux")
                .args([
                    "-L",
                    &socket,
                    "display-message",
                    "-p",
                    "-t",
                    &key,
                    "#{alternate_on}",
                ])
                .output()
                .expect("display-message");
            if String::from_utf8_lossy(&out.stdout).trim() == "1" {
                break;
            }
            assert!(attempt < 100, "pane never entered the alternate screen");
        }

        assert!(
            backend
                .scrollback(&key)
                .await
                .expect("scrollback")
                .is_none(),
            "an alt-screen pane has no retained history — the fetch must \
             serve nothing rather than a grid-wiping one-screen capture"
        );

        let _ = backend.kill(&key).await;
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

/// A full-screen agent may request the alternate screen, but lazybox's
/// transcript must remain on tmux's history-bearing primary pane. The
/// replay consumed by the TUI must therefore stay on the primary screen,
/// and an on-demand capture must expose lines above the visible screen.
#[tokio::test]
async fn alt_screen_agent_retains_scrollable_history() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping alt-screen history test");
        return;
    }
    let socket = format!("lazybox-test-alt-history-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let old_server = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "pre-upgrade",
                "sleep",
                "300",
            ])
            .output()
            .expect("start pre-upgrade tmux server");
        assert!(old_server.status.success(), "pre-upgrade server failed");

        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf '\\033[?1049h'; for i in $(seq 1 200); do echo line-$i; done; \
                     exec sleep 300"
                        .to_string(),
                ],
                None,
                &[],
                "alt-history-test",
            )
            .await
            .expect("tmux spawn");

        let replay = {
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            loop {
                ticker.tick().await;
                let sub = backend.subscribe(&key).await.expect("subscribe");
                if String::from_utf8_lossy(&sub.replay).contains("line-200") {
                    break sub.replay;
                }
            }
        };

        let state = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "display-message",
                "-p",
                "-t",
                &key,
                "#{alternate_on} #{history_size}",
            ])
            .output()
            .expect("display-message");
        let state = String::from_utf8_lossy(&state.stdout);
        let mut parts = state.split_whitespace();
        assert_eq!(parts.next(), Some("0"), "pane state: {state}");
        let history: u64 = parts.next().expect("history size").parse().expect("number");
        assert!(
            history > 100,
            "expected retained history, got {history} lines"
        );

        let mut terminal = Terminal::new(TerminalOptions {
            cols: 120,
            rows: 32,
            max_scrollback: 10_000,
        })
        .expect("terminal");
        terminal.vt_write(&replay);
        assert_eq!(
            terminal.active_screen().expect("active screen"),
            Screen::Primary
        );

        let (capture, _) = backend
            .scrollback(&key)
            .await
            .expect("scrollback")
            .expect("history source");
        assert!(
            String::from_utf8_lossy(&capture).contains("line-5\r"),
            "deep capture must include lines above the visible screen"
        );
        let mut history = Terminal::new(TerminalOptions {
            cols: 120,
            rows: 32,
            max_scrollback: 10_000,
        })
        .expect("history terminal");
        history.vt_write(&capture);
        assert!(
            history.scrollback_rows().expect("scrollback rows") > 100,
            "the first-wheel capture must reconstruct scrollable agent history"
        );

        let _ = backend.kill(&key).await;
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

/// capture-pane pads its output to the full pane grid: when an agent
/// leaves the cursor mid-screen, the rows below come back as blank lines
/// that were never scrollback. Seeding them verbatim parked the reattach
/// cursor at the bottom of that padding, so the live repaint's first
/// newline scrolled the blanks into history — the "random extra empty
/// lines after restart / move-session" (#442). The reconstructed seed must
/// end at the last row with real content, with the deep history above it
/// intact. Exercises the real capture-pane → `normalize_capture` path.
#[tokio::test]
async fn capture_seed_trims_grid_padding_below_the_cursor() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping capture-padding test");
        return;
    }
    let socket = format!("lazybox-test-capture-padding-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        // Fill more than a screen (so history accumulates), then move the
        // cursor up and clear to the end of the screen so the pane's bottom
        // rows are blank grid padding, and park a marker on the cursor row.
        // `sleep` (not an interactive shell) so dropping a backend can't
        // Ctrl-D the session away — see the restart test's note.
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "for i in $(seq 1 60); do echo line-$i; done; \
                     printf '\\033[8A\\033[J'; printf 'MIDMARKER'; exec sleep 300"
                        .to_string(),
                ],
                None,
                &[],
                "capture-padding-test",
            )
            .await
            .expect("tmux spawn");

        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        for attempt in 0.. {
            ticker.tick().await;
            let sub = backend.subscribe(&key).await.expect("subscribe");
            if String::from_utf8_lossy(&sub.replay).contains("MIDMARKER") {
                break;
            }
            assert!(attempt < 100, "pane output never reached the attach client");
        }

        // The RAW capture must actually carry trailing blank padding, else
        // this test would pass vacuously.
        let raw = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket,
                "capture-pane",
                "-p",
                "-e",
                "-S",
                "-10000",
                "-t",
                &key,
            ])
            .output()
            .expect("capture-pane");
        let raw = String::from_utf8_lossy(&raw.stdout);
        assert!(
            raw.ends_with("\n\n"),
            "raw capture should end in blank grid padding: {raw:?}"
        );

        // The backend's seed (same `normalize_capture` the reattach path
        // feeds a fresh client) must strip that padding: it ends at the
        // marker, not a run of blank rows.
        let (seed, _) = backend
            .scrollback(&key)
            .await
            .expect("scrollback")
            .expect("padded pane has retained history to serve");
        let seed = String::from_utf8_lossy(&seed);
        assert!(
            seed.ends_with("MIDMARKER"),
            "seed must end at the last content row, not grid padding: {seed:?}"
        );
        // Deep history above the padding survives — the trim is trailing
        // only, never the whole capture.
        assert!(
            seed.contains("line-5\r"),
            "seed must retain history above the visible screen"
        );

        let _ = backend.kill(&key).await;
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}
