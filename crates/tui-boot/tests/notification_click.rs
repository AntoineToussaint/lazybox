#![cfg(unix)]

use lazybox_ipc::{Command, socket, transport};
use std::process::Stdio;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_click_cli_sends_the_workspace_to_its_socket() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("notification.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");

    let server = tokio::spawn(async move {
        let (mut rd, mut wr) = listener.accept().await.expect("accept helper");
        socket::server_handshake(&mut rd, &mut wr)
            .await
            .expect("helper handshake");
        socket::read_frame::<_, Command>(&mut rd)
            .await
            .expect("read command")
            .expect("one command")
    });

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let workspace = "github:owner/repo#674";
    let socket_arg = socket_path.clone();
    let home = temp.path().join("home");
    let child = tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .arg("notification-click")
            .arg("--workspace")
            .arg(workspace)
            .arg("--socket")
            .arg(socket_arg)
            .env("LAZYBOX_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run notification helper")
    });

    let status = tokio::time::timeout(Duration::from_secs(5), child)
        .await
        .expect("helper exits")
        .expect("helper task");
    assert!(status.success());

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("helper sends command")
        .expect("server task");
    assert!(matches!(
        command,
        Command::ActivateWorkspace { session_key }
            if session_key.as_str() == "github:owner/repo#674"
    ));
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_click_selects_the_terminal_session_before_focusing_workspace() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("notification.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let capture_path = temp.path().join("osascript-args");
    let bin_dir = temp.path().join("bin");
    std::fs::create_dir(&bin_dir).expect("bin dir");
    let osascript_path = bin_dir.join("osascript");
    std::fs::write(
        &osascript_path,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$LAZYBOX_TEST_CAPTURE\"\n",
    )
    .expect("fake osascript");
    std::fs::set_permissions(&osascript_path, std::fs::Permissions::from_mode(0o755))
        .expect("executable");

    let server = tokio::spawn(async move {
        let (mut rd, mut wr) = listener.accept().await.expect("accept helper");
        socket::server_handshake(&mut rd, &mut wr)
            .await
            .expect("helper handshake");
        socket::read_frame::<_, Command>(&mut rd)
            .await
            .expect("read command")
            .expect("one command")
    });

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin_dir];
    paths.extend(std::env::split_paths(&inherited_path));
    let test_path = std::env::join_paths(paths).expect("PATH");
    let binary = env!("CARGO_BIN_EXE_lazybox");
    let socket_arg = socket_path.clone();
    let home = temp.path().join("home");
    let capture = capture_path.clone();
    let child = tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .arg("notification-click")
            .arg("--workspace")
            .arg("github:owner/repo#674")
            .arg("--socket")
            .arg(socket_arg)
            .arg("--terminal-bundle-id")
            .arg("com.apple.Terminal")
            .arg("--terminal-tty")
            .arg("/dev/ttys674")
            .env("LAZYBOX_HOME", home)
            .env("LAZYBOX_TEST_CAPTURE", capture)
            .env("PATH", test_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run notification helper")
    });

    let status = tokio::time::timeout(Duration::from_secs(5), child)
        .await
        .expect("helper exits")
        .expect("helper task");
    assert!(status.success());
    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("helper sends command")
        .expect("server task");
    assert!(matches!(
        command,
        Command::ActivateWorkspace { session_key }
            if session_key.as_str() == "github:owner/repo#674"
    ));

    let args = std::fs::read_to_string(capture_path).expect("captured osascript args");
    assert!(args.contains(r#"if tty of target_tab is "/dev/ttys674""#));
    assert!(args.contains("set selected tab of target_window to target_tab"));
}
