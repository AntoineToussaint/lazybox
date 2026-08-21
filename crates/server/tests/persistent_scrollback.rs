//! Restart-durability regression for issue #468.
//!
//! The raw-PTY backend's children die with the daemon, and the replay
//! ring that held their scrollback is in-memory only — so before this
//! fix a respawn after a restart came back with a blank grid. Persistent
//! spawns now mirror output to a per-session file keyed by a stable id
//! (not the backend key, which a respawn reallocates), and seed the fresh
//! PTY's replay from it.
//!
//! This drives the real path: spawn a churn-heavy session persistently,
//! let its child exit, drop the whole backend (destroying the ring), then
//! spawn a *fresh* backend + PTY under the SAME persist key and rebuild a
//! client VT from its replay. The reconstructed grid must recover the
//! prior run's scrollback, not an empty screen.

use lazybox_server::backend::{RawPtyBackend, SessionBackend};
use libghostty_vt::{Terminal, TerminalOptions};

const COLS: u16 = 120;
const ROWS: u16 = 32;
const MAX_SCROLLBACK: usize = 10_000;
/// Stable per-session key — the same value a restart's restore path
/// derives from the persisted session id, so run 2's file is run 1's.
const PERSIST_KEY: &str = "issue-468-session";

fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

/// Drain a session's full replay (ring snapshot + everything the live
/// stream delivers until it closes) into one byte buffer.
async fn drain_replay(backend: &RawPtyBackend, key: &str) -> Vec<u8> {
    let mut sub = backend.subscribe(key).await.expect("subscribe");
    let last_seq = sub.last_seq;
    let mut out = sub.replay.clone();
    while let Some(chunk) = sub.live.recv().await {
        // `subscribe` opens the broadcast receiver and THEN takes the
        // ring snapshot, so a chunk emitted in that window sits in BOTH
        // `replay` and `live`. Honor the documented contract — live
        // chunks are authoritative only past `last_seq` — so the
        // replayed prefix isn't double-counted. Without this the overlap
        // inflates run 1's reconstructed scrollback by however many
        // chunks the window happened to catch (wider under CI load), and
        // the restart-durability assertion (`recovered >= run1`) then
        // races that phantom depth (#833).
        if chunk.seq > last_seq {
            out.extend_from_slice(&chunk.bytes);
        }
    }
    out
}

fn scrollback_rows(stream: &[u8]) -> usize {
    let mut term = Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback_lines: MAX_SCROLLBACK,
        max_scrollback_bytes: None,
    })
    .expect("vt init");
    term.vt_write(stream);
    term.scrollback_rows().expect("scrollback_rows")
}

/// Serialize env-var mutations across concurrent tests. Each test that
/// modifies `LAZYBOX_HOME` acquires this lock to prevent interference.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct IsolatedHome {
    _guard: std::sync::MutexGuard<'static, ()>,
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedHome {
    fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("LAZYBOX_HOME");
        // SAFETY: mutation is serialized by ENV_LOCK; restored on drop.
        unsafe {
            std::env::set_var("LAZYBOX_HOME", tmp.path());
        }
        Self {
            _guard: guard,
            _tmp: tmp,
            prev,
        }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var("LAZYBOX_HOME", v),
                None => std::env::remove_var("LAZYBOX_HOME"),
            }
        }
    }
}

#[tokio::test]
async fn scrollback_survives_a_simulated_restart() {
    // Isolate all lazybox state (incl. the scrollback dir the backend
    // derives from `LAZYBOX_HOME`) to a temp dir under serialization.
    let _home_guard = IsolatedHome::new();

    // Run 1: ~600 committed lines — far more than one screenful, so a
    // blank respawn (the pre-fix behavior) is trivially distinguishable
    // from one seeded with the real history.
    let script = "i=0; while [ $i -lt 600 ]; do \
                  printf '\\rline %05d ................................\\n' $i; \
                  i=$((i+1)); done; printf 'RUN1-DONE\\n'";
    let backend1 = RawPtyBackend::new();
    let key1 = backend1
        .spawn_persistent(&sh(script), None, &[], "hint", Some(PERSIST_KEY))
        .await
        .expect("persistent spawn");
    let run1 = drain_replay(&backend1, &key1).await;
    let run1_depth = scrollback_rows(&run1);
    assert!(
        String::from_utf8_lossy(&run1).contains("RUN1-DONE"),
        "run 1 produced its output"
    );

    // Restart: the child has exited and the backend (with its ring) is
    // dropped. Only the on-disk scrollback file remains.
    drop(backend1);

    // Run 2: a fresh backend + PTY under the SAME persist key, emitting a
    // small marker of its own. Its replay must be seeded from run 1.
    let backend2 = RawPtyBackend::new();
    let key2 = backend2
        .spawn_persistent(
            &sh("printf 'RUN2-FRESH\\n'"),
            None,
            &[],
            "hint",
            Some(PERSIST_KEY),
        )
        .await
        .expect("respawn");
    let run2 = drain_replay(&backend2, &key2).await;
    let recovered = String::from_utf8_lossy(&run2);

    assert!(
        recovered.contains("RUN1-DONE"),
        "the respawn must replay run 1's persisted history"
    );
    assert!(recovered.contains("RUN2-FRESH"), "and its own fresh output");

    // The reconstructed grid recovers many screens of real scrollback —
    // not the blank screen (~one screenful) a non-persistent respawn gave.
    let recovered_depth = scrollback_rows(run2.as_slice());
    assert!(
        recovered_depth >= ROWS as usize * 8,
        "recovered scrollback must be deep, got {recovered_depth} rows"
    );
    assert!(
        recovered_depth >= run1_depth,
        "the restart must not shrink scrollback (run1 {run1_depth}, recovered {recovered_depth})"
    );
}

/// Regression test for #1250: IsolatedHome guard restores env properly.
/// Concurrent tests no longer interfere with LAZYBOX_HOME modifications.
#[test]
fn env_guard_properly_restores_state() {
    let original = std::env::var_os("LAZYBOX_HOME");

    // First isolation scope
    {
        let _guard = IsolatedHome::new();
        assert!(
            std::env::var_os("LAZYBOX_HOME").is_some(),
            "should set LAZYBOX_HOME"
        );
    }
    // Should be restored
    assert_eq!(
        std::env::var_os("LAZYBOX_HOME"),
        original,
        "env restored after scope 1"
    );

    // Second isolation scope — independent and doesn't interfere
    {
        let _guard = IsolatedHome::new();
        assert!(
            std::env::var_os("LAZYBOX_HOME").is_some(),
            "should set again"
        );
    }
    // Should be restored again
    assert_eq!(
        std::env::var_os("LAZYBOX_HOME"),
        original,
        "env restored after scope 2"
    );
}
