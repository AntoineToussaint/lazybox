//! A tmux session is never destroyed by anything unattended (#1869).
//!
//! The bug: `claude` exited with code 0 one second after spawning, tmux's
//! default closed the pane — and with it the window and the whole session —
//! and the user permanently lost a worktree's agent session, its scrollback,
//! and any chance of reading WHY it died. `code 0` cannot distinguish "work
//! finished" from "failed to start", so no exit code may authorize
//! destroying a session. Only an explicit user action may.
//!
//! These run against a REAL tmux, and are skipped (pass, with a note) when
//! tmux is missing or older than the backend's minimum — the same gate
//! `TmuxBackend::detect()` applies.

mod common;

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

fn has_session(socket: &str, key: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["-L", socket, "has-session", "-t", key])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn a pane that prints a marker and exits 0, then wait until the
/// attach conduit reports the exit. Returns the marker.
async fn spawn_and_let_it_exit(backend: &TmuxBackend, hint: &str) -> (String, String) {
    let marker = format!("marker-{hint}");
    let key = backend
        .spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo {marker}; sleep 1; exit 0"),
            ],
            None,
            &[],
            hint,
        )
        .await
        .expect("tmux spawn");

    // The conduit must EOF — that is how the daemon learns an agent
    // ended. Under `remain-on-exit on` the session no longer dies, so
    // the `pane-died` hook detaching lazybox's own client is the only
    // thing that can produce it.
    let mut sub = backend.subscribe(&key).await.expect("subscribe");
    let drained = timeout(Duration::from_secs(15), async {
        while sub.live.recv().await.is_some() {}
    })
    .await;
    assert!(
        drained.is_ok(),
        "the attach conduit never ended after the pane's program exited — \
         a dead agent would show as live forever",
    );
    (key, marker)
}

/// The heart of #1869: an agent that exits with code 0 leaves its tmux
/// session alive, with its pane, its scrollback and its output intact,
/// and lazybox can attach to it again.
#[tokio::test]
async fn a_clean_exit_leaves_the_session_alive_and_reattachable() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping session-survival test");
        return;
    }
    let socket = format!("lazybox-test-survive-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let (key, marker) = spawn_and_let_it_exit(&backend, "survive-exit").await;

        assert!(
            has_session(&socket, &key),
            "the tmux session must outlive the program that ran in it — \
             reaping it destroys the scrollback the user needs to diagnose \
             a dead-on-arrival agent, and their only way back in",
        );
        assert!(
            backend.list().await.expect("list").contains(&key),
            "a surviving session must stay discoverable",
        );
        assert!(
            !backend.is_alive(&key).await.expect("liveness"),
            "a dead pane must be REPORTED dead — the session is kept, not \
             pretended live",
        );

        // Reattachable, with its output still there.
        let (history, _seq) = backend
            .scrollback(&key)
            .await
            .expect("scrollback")
            .expect("a kept session must still hold its history");
        assert!(
            String::from_utf8_lossy(&history).contains(&marker),
            "the kept session must still hold the dead program's output",
        );
        let sub = backend.subscribe(&key).await.expect("reattach");
        assert!(
            String::from_utf8_lossy(&sub.replay).contains(&marker),
            "reattaching must paint the dead pane's content",
        );

        // And an EXPLICIT close still removes it.
        backend.kill(&key).await.expect("explicit kill");
        assert!(
            !has_session(&socket, &key),
            "an explicit user close must still destroy the session",
        );
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}

/// `release` is the teardown an agent's own exit runs. It drops lazybox's
/// attach conduit and NOTHING else — the session stays on the server.
#[tokio::test]
async fn release_never_destroys_the_session() {
    if modern_tmux_version().is_none() {
        eprintln!("tmux missing or too old — skipping release test");
        return;
    }
    let socket = format!("lazybox-test-release-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let key = backend
            .spawn(
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo alive; exec sleep 300".to_string(),
                ],
                None,
                &[],
                "release-keeps",
            )
            .await
            .expect("tmux spawn");
        backend.release(&key).await;
        assert!(
            has_session(&socket, &key),
            "release is the self-exit teardown — it must never kill a session",
        );
        assert!(
            backend.is_alive(&key).await.expect("liveness"),
            "a live pane whose conduit was released is still alive",
        );
        backend.kill(&key).await.expect("explicit kill");
        assert!(
            !has_session(&socket, &key),
            "an explicit kill still removes it"
        );
    })
    .await;
    kill_test_server(&socket);
    result.expect("test timed out");
}
