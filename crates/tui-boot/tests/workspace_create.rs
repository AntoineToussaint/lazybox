#![cfg(unix)]

use lazybox_ipc::{Command, socket, transport};
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

/// Accept one client, complete the handshake, and return the single command
/// it wrote — the same shape `notification_click` uses to observe the CLI.
fn accept_one_command(
    listener: transport::Listener,
) -> tokio::task::JoinHandle<Command> {
    tokio::spawn(async move {
        let (mut rd, mut wr) = listener.accept().await.expect("accept client");
        socket::server_handshake(&mut rd, &mut wr)
            .await
            .expect("handshake");
        socket::read_frame::<_, Command>(&mut rd)
            .await
            .expect("read command")
            .expect("one command")
    })
}

fn run_workspace_create(
    binary: &str,
    extra: &[&str],
    home: std::path::PathBuf,
) -> tokio::task::JoinHandle<std::process::ExitStatus> {
    let binary = binary.to_string();
    let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .arg("workspace")
            .arg("create")
            .args(&extra)
            .env("LAZYBOX_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run workspace create")
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_infers_project_from_cwd_and_sends_create_workspace() {
    let temp = tempfile::tempdir().expect("tempdir");
    let checkout = temp.path().join("acme-widget");
    std::fs::create_dir(&checkout).expect("checkout dir");
    init_checkout(&checkout, "git@github.com:acme/widget.git");

    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = accept_one_command(listener);

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

    let status = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(status.success(), "workspace create exited non-zero");

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace {
            name,
            project_key,
            spawn_agent,
        } => {
            assert_eq!(name, "flaky-test investigation");
            assert_eq!(project_key, lazybox_core::ProjectKey::github("acme", "widget"));
            assert_eq!(spawn_agent.as_deref(), Some("claude"));
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
    let server = accept_one_command(listener);

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

    let status = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(status.success(), "workspace create exited non-zero");

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace {
            name,
            project_key,
            spawn_agent,
        } => {
            assert_eq!(name, "scratch");
            assert_eq!(project_key, lazybox_core::ProjectKey::new("local-sandbox"));
            assert_eq!(spawn_agent, None);
        }
        other => panic!("expected CreateWorkspace, got {other:?}"),
    }
}
