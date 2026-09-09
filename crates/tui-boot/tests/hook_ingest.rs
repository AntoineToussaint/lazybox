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

/// A `PreToolUse` payload for a full-file read of `path`.
fn read_payload(path: &std::path::Path) -> String {
    format!(
        r#"{{"hook_event_name":"PreToolUse","session_id":"s1","cwd":"/w","tool_name":"Read",
             "tool_input":{{"file_path":"{}"}}}}"#,
        path.display()
    )
}

/// Run `lazybox hook-ingest` with a payload on stdin, returning its stdout.
/// Panics on a non-zero exit: a lifecycle hook that fails is a red error in
/// the agent, whatever else the test is checking.
fn hook_stdout(home: &std::path::Path, payload: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lazybox"))
        .arg("hook-ingest")
        .args(["--backend-key", "lzb-sess-7"])
        .env("LAZYBOX_HOME", home)
        .env("LAZYBOX_HOOK_DECISION_TIMEOUT_MS", "1500")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hook-ingest");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let out = child.wait_with_output().expect("wait for hook-ingest");
    assert!(out.status.success(), "hook-ingest must always exit 0");
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

/// Write a home whose config turns the large-read intercept on.
fn write_intercept_config(home: &std::path::Path, log_path: &std::path::Path, min_lines: usize) {
    std::fs::create_dir_all(home).expect("home dir");
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "ui:\n  log_path: {}\nagent:\n  context_hygiene:\n    mode: 'on'\n    min_lines: {}\n",
            log_path.display(),
            min_lines,
        ),
    )
    .expect("write config");
}

/// What the stand-in daemon does once a client has connected.
enum Daemon {
    /// Complete the handshake and answer the decision request.
    Answers(lazybox_ipc::ToolUseDecision),
    /// Accept the connection and then go silent — a daemon mid-restart, or
    /// one wedged behind a slow handler.
    Stalls,
}

/// Bind `<home>/run/daemon.sock` and serve exactly one connection, returning
/// the commands that arrived on it. The real daemon is far too heavy to stand
/// up here, and what these tests are about is the helper's side of the
/// exchange: what it sends, what it prints, and that it gives up.
fn spawn_fake_daemon(
    socket: std::path::PathBuf,
    behavior: Daemon,
) -> std::thread::JoinHandle<Vec<lazybox_ipc::Command>> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = lazybox_ipc::transport::Listener::bind(&socket)
                .await
                .expect("bind fake daemon socket");
            let (mut rd, mut wr) = listener.accept().await.expect("accept");
            if matches!(behavior, Daemon::Stalls) {
                // Hold the connection open, saying nothing, until the helper
                // gives up and hangs up.
                let _ = lazybox_ipc::socket::read_frame::<_, lazybox_ipc::Command>(&mut rd).await;
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                return Vec::new();
            }
            lazybox_ipc::socket::server_handshake(&mut rd, &mut wr)
                .await
                .expect("handshake");
            let mut seen = Vec::new();
            while let Ok(Some(command)) =
                lazybox_ipc::socket::read_frame::<_, lazybox_ipc::Command>(&mut rd).await
            {
                let reply = match (&command, &behavior) {
                    (
                        lazybox_ipc::Command::DecideToolUse {
                            client_request_id, ..
                        },
                        Daemon::Answers(decision),
                    ) => Some(lazybox_ipc::Event::ToolUseDecided {
                        client_request_id: client_request_id.clone(),
                        decision: decision.clone(),
                    }),
                    _ => None,
                };
                seen.push(command);
                if let Some(reply) = reply {
                    lazybox_ipc::socket::write_frame(&mut wr, &reply)
                        .await
                        .expect("write decision");
                    break;
                }
            }
            seen
        })
    })
}

#[test]
fn a_denied_read_prints_the_condensed_reason() {
    // The whole point of the intercept: the model is handed the condensed
    // text as the refusal's reason, so the raw file never enters context.
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    write_intercept_config(&home, &temp.path().join("lazybox.log"), 350);
    let big = temp.path().join("big.rs");
    std::fs::write(&big, "line\n".repeat(400)).expect("write file");
    std::fs::create_dir_all(home.join("run")).expect("run dir");

    let daemon = spawn_fake_daemon(
        home.join("run/daemon.sock"),
        Daemon::Answers(lazybox_ipc::ToolUseDecision::Deny {
            reason: "[condensed by lazybox: big.rs, 400 lines → 1 lines]\nit repeats".into(),
        }),
    );
    let stdout = hook_stdout(&home, &read_payload(&big));
    let commands = daemon.join().expect("fake daemon");

    let decision: serde_json::Value = serde_json::from_str(stdout.trim()).expect("decision JSON");
    let out = &decision["hookSpecificOutput"];
    assert_eq!(out["hookEventName"], "PreToolUse");
    assert_eq!(out["permissionDecision"], "deny");
    assert!(
        out["permissionDecisionReason"]
            .as_str()
            .expect("reason")
            .contains("it repeats"),
        "the condensed text is what the model receives: {decision}"
    );

    // The state signal still rides along: a decision round-trip must not cost
    // the agent its `Working` transition.
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, lazybox_ipc::Command::IngestHook { .. })),
        "the PreToolUse state signal was dropped: {commands:?}"
    );
    let requested = commands.iter().find_map(|c| match c {
        lazybox_ipc::Command::DecideToolUse { request, .. } => Some(request.clone()),
        _ => None,
    });
    let requested = requested.expect("a decision was requested");
    assert_eq!(requested.tool_name, "Read");
    assert_eq!(requested.file_path, big.to_string_lossy());
}

#[test]
fn an_allowed_read_prints_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    write_intercept_config(&home, &temp.path().join("lazybox.log"), 350);
    let big = temp.path().join("big.rs");
    std::fs::write(&big, "line\n".repeat(400)).expect("write file");
    std::fs::create_dir_all(home.join("run")).expect("run dir");

    let daemon = spawn_fake_daemon(
        home.join("run/daemon.sock"),
        Daemon::Answers(lazybox_ipc::ToolUseDecision::Allow),
    );
    assert_eq!(hook_stdout(&home, &read_payload(&big)), "");
    daemon.join().expect("fake daemon");
}

#[test]
fn a_ranged_read_is_never_even_asked_about() {
    // `offset`/`limit` is the escape hatch back to the real bytes. It must not
    // reach the daemon at all — with no daemon listening here, a helper that
    // asked would still print nothing, so assert on the payload the daemon
    // would have to answer instead.
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    write_intercept_config(&home, &temp.path().join("lazybox.log"), 350);
    let big = temp.path().join("big.rs");
    std::fs::write(&big, "line\n".repeat(400)).expect("write file");
    std::fs::create_dir_all(home.join("run")).expect("run dir");

    let daemon = spawn_fake_daemon(
        home.join("run/daemon.sock"),
        Daemon::Answers(lazybox_ipc::ToolUseDecision::Deny {
            reason: "must never be reached".into(),
        }),
    );
    let payload = format!(
        r#"{{"hook_event_name":"PreToolUse","session_id":"s1","cwd":"/w","tool_name":"Read",
             "tool_input":{{"file_path":"{}","limit":80}}}}"#,
        big.display()
    );
    assert_eq!(hook_stdout(&home, &payload), "");
    let commands = daemon.join().expect("fake daemon");
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, lazybox_ipc::Command::DecideToolUse { .. })),
        "a ranged read must never be submitted for a decision: {commands:?}"
    );
}

#[test]
fn a_stalled_daemon_lets_the_read_through() {
    // A daemon that accepts and then says nothing must cost one bounded pause,
    // not a hung turn — and must never produce partial output.
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    write_intercept_config(&home, &temp.path().join("lazybox.log"), 350);
    let big = temp.path().join("big.rs");
    std::fs::write(&big, "line\n".repeat(400)).expect("write file");
    std::fs::create_dir_all(home.join("run")).expect("run dir");

    let daemon = spawn_fake_daemon(home.join("run/daemon.sock"), Daemon::Stalls);
    let started = std::time::Instant::now();
    assert_eq!(hook_stdout(&home, &read_payload(&big)), "");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "the hook waited past its deadline: {:?}",
        started.elapsed()
    );
    drop(daemon);
}

#[test]
fn the_intercept_is_off_without_the_policy() {
    // Default config is `mode: shadow`, which decides but changes nothing —
    // the helper must not even open a connection, so every tool call in a
    // normal session stays exactly as cheap as it was.
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    write_config(&home, &temp.path().join("lazybox.log"));
    let big = temp.path().join("big.rs");
    std::fs::write(&big, "line\n".repeat(400)).expect("write file");
    std::fs::create_dir_all(home.join("run")).expect("run dir");

    let daemon = spawn_fake_daemon(
        home.join("run/daemon.sock"),
        Daemon::Answers(lazybox_ipc::ToolUseDecision::Deny {
            reason: "must never be reached".into(),
        }),
    );
    assert_eq!(hook_stdout(&home, &read_payload(&big)), "");
    let commands = daemon.join().expect("fake daemon");
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, lazybox_ipc::Command::DecideToolUse { .. })),
        "shadow mode must not ask for a decision: {commands:?}"
    );
}
