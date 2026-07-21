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

use lazybox_server::backend::tmux::modern_tmux_version;
use lazybox_server::backend::{SessionBackend, TmuxBackend};
use libghostty_vt::{Terminal, TerminalOptions, screen::Screen};
use std::time::Duration;
use tokio::time::timeout;

const TEST_DEADLINE: Duration = Duration::from_secs(30);

fn kill_test_server(socket: &str) {
    let _ = std::process::Command::new("tmux")
        .args(["-L", socket, "kill-server"])
        .output();
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
        // (not a `write` to an interactive shell) because dropping the
        // backend sends portable-pty's parting `\n`+EOT down the attach
        // client — tmux forwards the Ctrl-D to the pane, and an
        // interactive shell would exit on it, taking the session (and the
        // per-socket test server) with it. `sleep` ignores its stdin, so
        // the session survives the simulated daemon death below the same
        // way a real agent session survives a crashed daemon.
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
