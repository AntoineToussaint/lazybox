//! A Claude/Codex lifecycle hook runs `lazybox hook-ingest`, and any
//! non-zero exit surfaces as a red "Stop hook error" in the agent while the
//! state transition it carried is lost (#848). These tests drive the real
//! binary to prove two exit-0 guarantees that only hold end-to-end: an
//! unknown flag from a build-skewed daemon is ignored, and an unwritable log
//! file doesn't abort the hook before it even dispatches.

#![cfg(unix)]

use std::process::{Command, Stdio};

/// Run `lazybox hook-ingest <args>` with `LAZYBOX_HOME` pointed at `home`,
/// stdin closed (the empty payload a probe hook sends), and return whether
/// it exited 0. No daemon is listening, so ingest's IPC forward is a
/// best-effort no-op — the exit code is purely about the hook staying quiet.
fn run_hook_ingest(home: &std::path::Path, args: &[&str]) -> bool {
    Command::new(env!("CARGO_BIN_EXE_lazybox"))
        .arg("hook-ingest")
        .args(args)
        .env("LAZYBOX_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run hook-ingest")
        .success()
}

fn write_config(home: &std::path::Path, log_path: &std::path::Path) {
    std::fs::create_dir_all(home).expect("home dir");
    std::fs::write(
        home.join("config.yaml"),
        format!("ui:\n  log_path: {}\n", log_path.display()),
    )
    .expect("write config");
}

#[test]
fn hook_ingest_ignores_unknown_flags() {
    // A newer daemon can bake a flag this binary predates. A strict parser
    // would reject it and exit non-zero; ingest must drop it and exit 0.
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    write_config(&home, &temp.path().join("lazybox.log"));

    assert!(run_hook_ingest(
        &home,
        &[
            "--backend-key",
            "lzb-sess-7",
            "--some-future-flag",
            "whatever"
        ],
    ));
}

#[test]
fn hook_ingest_survives_unwritable_log() {
    // The log file lives under a directory that doesn't exist, so opening it
    // fails. Logging init must not be a fatal pre-flight for a lifecycle
    // hook — the hook still has to exit 0.
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    write_config(&home, &temp.path().join("no/such/dir/lazybox.log"));

    assert!(run_hook_ingest(&home, &["--backend-key", "lzb-sess-7"]));
}
