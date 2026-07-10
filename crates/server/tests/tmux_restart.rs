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
use std::time::Duration;
use tokio::time::timeout;

const TEST_DEADLINE: Duration = Duration::from_secs(30);

fn kill_test_server(socket: &str) {
    let _ = std::process::Command::new("tmux")
        .args(["-L", socket, "kill-server"])
        .output();
}

#[tokio::test]
async fn restarted_backend_seeds_scrollback_from_tmux_history() {
    if modern_tmux_version().is_none() {
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
