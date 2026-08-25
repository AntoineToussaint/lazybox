#![cfg(unix)]

use lazybox_ipc::{Command, Event, socket, transport};
use std::process::Stdio;
use std::time::Duration;

/// Prepare a throwaway git checkout whose `origin` remote points at
/// `owner/repo`, so `lazybox workspace create` can infer the project from it.
fn init_checkout(dir: &std::path::Path, origin: &str) {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", origin]);
}

/// A minimal daemon speaking the real client protocol: handshake, reply to
/// `Subscribe` with an (empty) snapshot, then acknowledge the `CreateWorkspace`
/// by broadcasting a `WorkspaceUpserted` stamped with `assigned_key` — which
/// deliberately differs from the name's slug so the test proves the CLI reports
/// the daemon's key, not one it guessed. Returns the received `CreateWorkspace`
/// command for assertions.
fn fake_daemon(
    listener: transport::Listener,
    assigned_key: &str,
) -> tokio::task::JoinHandle<Command> {
    let assigned_key = assigned_key.to_string();
    tokio::spawn(async move {
        let (mut rd, mut wr) = listener.accept().await.expect("accept client");
        socket::server_handshake(&mut rd, &mut wr)
            .await
            .expect("handshake");
        loop {
            let cmd = socket::read_frame::<_, Command>(&mut rd)
                .await
                .expect("read command")
                .expect("a command");
            match cmd {
                Command::Subscribe => {
                    socket::write_frame(
                        &mut wr,
                        &Event::Snapshot {
                            workspaces: vec![],
                            terminals: vec![],
                            projects: vec![],
                            recent_snippets: vec![],
                            dismissed_updates: vec![],
                        },
                    )
                    .await
                    .expect("send snapshot");
                }
                Command::CreateWorkspace {
                    ref name,
                    ref project_key,
                    ref client_request_id,
                    ..
                } => {
                    let mut ws = lazybox_core::Workspace::empty(
                        lazybox_core::WorkspaceKey::new(assigned_key.clone()),
                        "main",
                        chrono::Utc::now(),
                    );
                    ws.name = name.clone();
                    ws.project_key = Some(project_key.clone());
                    ws.local = true;
                    socket::write_frame(
                        &mut wr,
                        &Event::WorkspaceUpserted(std::sync::Arc::new(ws)),
                    )
                    .await
                    .expect("send upsert");
                    let client_request_id = client_request_id
                        .clone()
                        .expect("workspace create is correlated");
                    socket::write_frame(
                        &mut wr,
                        &Event::WorkspaceCreated {
                            client_request_id: client_request_id.clone(),
                            workspace_key: lazybox_core::WorkspaceKey::new(assigned_key.clone()),
                        },
                    )
                    .await
                    .expect("send create acknowledgement");
                    socket::write_frame(&mut wr, &Event::CommandCompleted { client_request_id })
                        .await
                        .expect("send create completion");
                    return cmd;
                }
                other => panic!("unexpected command {other:?}"),
            }
        }
    })
}

/// Run `lazybox workspace create <extra…>` and capture its output.
fn run_workspace_create(
    binary: &str,
    extra: &[&str],
    home: std::path::PathBuf,
) -> tokio::task::JoinHandle<std::process::Output> {
    let binary = binary.to_string();
    let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .arg("workspace")
            .arg("create")
            .args(&extra)
            .env("LAZYBOX_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("run workspace create")
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_infers_project_from_cwd_and_reports_the_daemon_key() {
    let temp = tempfile::tempdir().expect("tempdir");
    let checkout = temp.path().join("acme-widget");
    std::fs::create_dir(&checkout).expect("checkout dir");
    init_checkout(&checkout, "git@github.com:acme/widget.git");

    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    // The daemon hands back a collision-suffixed key the CLI can't predict.
    let server = fake_daemon(listener, "flaky-test-investigation-2");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--name",
            "flaky-test investigation",
            "--agent",
            "claude",
            "--cwd",
            &checkout.to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(output.status.success(), "workspace create exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Reports the daemon's actual key, and says "Created" (confirmed), not
    // the "Requested" fallback.
    assert!(
        stdout.contains("flaky-test-investigation-2") && stdout.contains("Created"),
        "stdout should confirm the daemon's key, got: {stdout:?}"
    );

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace {
            name,
            project_key,
            spawn_agent,
            client_request_id,
            initial_prompt,
        } => {
            assert_eq!(name, "flaky-test investigation");
            assert_eq!(
                project_key,
                lazybox_core::ProjectKey::github("acme", "widget")
            );
            assert_eq!(spawn_agent.as_deref(), Some("claude"));
            assert!(client_request_id.is_some());
            // No --prompt passed here → bare spawn.
            assert_eq!(initial_prompt, None);
        }
        other => panic!("expected CreateWorkspace, got {other:?}"),
    }
}

/// `--prompt` with `--agent` forwards the brief as the command's
/// `initial_prompt`, so one agent can create-and-task another in a single
/// call (the agent-to-agent handoff `SendMessage` could not do).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_forwards_prompt_as_initial_prompt() {
    let temp = tempfile::tempdir().expect("tempdir");
    let checkout = temp.path().join("acme-widget");
    std::fs::create_dir(&checkout).expect("checkout dir");
    init_checkout(&checkout, "git@github.com:acme/widget.git");

    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "briefed-1");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--name",
            "briefed",
            "--agent",
            "claude",
            "--prompt",
            "Investigate the flaky test in foo_test.rs and propose a fix.",
            "--cwd",
            &checkout.to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(output.status.success(), "workspace create exited non-zero");

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace {
            spawn_agent,
            initial_prompt,
            ..
        } => {
            assert_eq!(spawn_agent.as_deref(), Some("claude"));
            assert_eq!(
                initial_prompt.as_deref(),
                Some("Investigate the flaky test in foo_test.rs and propose a fix.")
            );
        }
        other => panic!("expected CreateWorkspace, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_uses_explicit_project_without_a_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "scratch");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    // A non-git --cwd proves resolution came from --project, not inference.
    let child = run_workspace_create(
        binary,
        &[
            "--name",
            "  scratch  ",
            "--project",
            "local-sandbox",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(output.status.success(), "workspace create exited non-zero");

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace {
            name,
            project_key,
            spawn_agent,
            client_request_id,
            initial_prompt,
        } => {
            assert_eq!(name, "scratch");
            assert_eq!(project_key, lazybox_core::ProjectKey::new("local-sandbox"));
            assert_eq!(spawn_agent, None);
            assert!(client_request_id.is_some());
            assert_eq!(initial_prompt, None);
        }
        other => panic!("expected CreateWorkspace, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_rejects_an_unknown_agent_without_connecting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    // A bad --agent must be caught before any connection: this daemon should
    // never accept a client.
    let server = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--name",
            "scratch",
            "--project",
            "local-sandbox",
            "--agent",
            "totally-not-a-real-agent",
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(
        !output.status.success(),
        "unknown --agent must fail the command"
    );

    // The CLI bailed at validation, so the accept never completes.
    let connected = tokio::time::timeout(Duration::from_millis(500), server).await;
    assert!(
        connected.is_err(),
        "unknown --agent must not connect to the daemon"
    );
}
