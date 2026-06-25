//! Tests for daemon lifecycle + socket service. Drive the service
//! from a background task, connect via the socket transport, verify
//! the Subscribe → Snapshot contract works end-to-end — just like
//! the in-process `channel::pair` path, but over a real Unix socket.

use lazybox_ipc::{Command, Event, socket};
use lazybox_server::ServerConfig;
use lazybox_server::backend::SessionBackend;
use lazybox_server::lifecycle;
use lazybox_server::socket_service::SocketService;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

fn runtime_paths(base: &TempDir) -> (PathBuf, PathBuf) {
    let sock = base.path().join("daemon.sock");
    let pid = base.path().join("daemon.pid");
    (sock, pid)
}

async fn spawn_service(
    base: &TempDir,
) -> (
    PathBuf,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let (sock, pid) = runtime_paths(base);
    let service = SocketService::new(sock.clone(), pid.clone(), ServerConfig::in_memory);
    let shutdown = service.shutdown_handle();
    let handle = tokio::spawn(async move {
        service.run().await.unwrap();
    });

    // Wait for the socket to appear (bind happens inside .run()).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !sock.exists() {
        if tokio::time::Instant::now() >= deadline {
            panic!("socket never appeared at {}", sock.display());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (sock, handle, shutdown)
}

// ── Socket end-to-end ──────────────────────────────────────────────────

#[tokio::test]
async fn socket_subscribe_yields_snapshot() {
    let base = TempDir::new().unwrap();
    let (sock, handle, shutdown) = spawn_service(&base).await;

    let (mut client, _) = socket::connect(&sock).await.expect("connect");
    client.send(Command::Subscribe).expect("send subscribe");
    let evt = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("timeout")
        .expect("event");
    assert!(matches!(evt, Event::Snapshot { .. }));

    shutdown.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
#[tokio::test]
async fn socket_service_creates_pid_file() {
    let base = TempDir::new().unwrap();
    let (_sock, handle, shutdown) = spawn_service(&base).await;
    let pid_file = base.path().join("daemon.pid");
    assert!(pid_file.exists(), "pid file written on start");
    let contents: String = tokio::fs::read_to_string(&pid_file).await.unwrap();
    let pid: u32 = contents.trim().parse().unwrap();
    assert_eq!(pid, std::process::id(), "pid matches running process");

    shutdown.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
#[tokio::test]
async fn socket_service_cleans_up_on_shutdown() {
    let base = TempDir::new().unwrap();
    let (sock, handle, shutdown) = spawn_service(&base).await;
    let pid_file = base.path().join("daemon.pid");

    shutdown.notify_one();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .unwrap()
        .unwrap();

    assert!(!sock.exists(), "socket removed on clean shutdown");
    assert!(!pid_file.exists(), "pid file removed on clean shutdown");
}
#[tokio::test]
async fn stale_socket_is_cleaned_up_on_bind() {
    let base = TempDir::new().unwrap();
    let (sock, pid) = runtime_paths(&base);
    // Leave a stale file where the socket will bind.
    tokio::fs::write(&sock, "garbage").await.unwrap();

    let service = SocketService::new(sock.clone(), pid, ServerConfig::in_memory);
    let shutdown = service.shutdown_handle();
    let handle = tokio::spawn(async move {
        service
            .run()
            .await
            .expect("service should reclaim stale socket");
    });

    // The stale file is present, so `sock.exists()` is true even before
    // rebind. Poll by trying to connect — a plain file will refuse with
    // ENOTSOCK, and the real socket will accept.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let client = loop {
        match socket::connect(&sock).await {
            Ok((c, _)) => break c,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("rebind didn't happen: {e}"),
        }
    };
    drop(client);

    shutdown.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
/// The embedded-mode wiring: one shared `ServerConfig` serves both the
/// in-process transport and the Unix socket. A hook forwarded over the
/// socket (what `lazybox hook-ingest --backend-key` does) must land on
/// the same bus the in-process TUI subscribes to — this is the path
/// that was structurally dead when the default `lazybox` invocation
/// bound no socket at all.
#[tokio::test]
async fn hook_ingest_over_socket_reaches_shared_embedded_config() {
    let base = TempDir::new().unwrap();
    let (sock, pid) = runtime_paths(&base);

    // Shared config — the socket service's factory hands out clones of
    // it, exactly like `run_embedded_realm` does.
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let factory_config = config.clone();
    let service = SocketService::new(sock.clone(), pid, move || factory_config.clone());
    let shutdown = service.shutdown_handle();
    let handle = tokio::spawn(async move {
        service.run().await.unwrap();
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !sock.exists() {
        assert!(tokio::time::Instant::now() < deadline, "socket never bound");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The "TUI" client spawns an agent and listens on the shared bus.
    let (mut tui, _) = socket::connect(&sock).await.expect("connect tui");
    tui.send(Command::Subscribe).expect("subscribe");
    let _snapshot = tokio::time::timeout(Duration::from_secs(2), tui.recv())
        .await
        .expect("timeout")
        .expect("snapshot");
    tui.send(Command::Spawn {
        session_key: "test:ws-hooks".into(),
        session_id: None,
        kind: lazybox_ipc::TerminalKind::Agent("claude".into()),
        cwd: None,
        initial_prompt: None,
    })
    .expect("spawn");
    let spawned = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match tui.recv().await {
                Some(Event::TerminalSpawned { terminal_id, .. }) => break terminal_id,
                Some(_) => continue,
                None => panic!("stream closed before TerminalSpawned"),
            }
        }
    })
    .await
    .expect("TerminalSpawned");
    let key = mock.list().await.unwrap().into_iter().next().unwrap();

    // A separate, short-lived connection forwards the hook — exactly
    // what the `hook-ingest` helper does: connect, one frame, done.
    let (ingest, _) = socket::connect(&sock).await.expect("connect ingest");
    ingest
        .send(Command::IngestHook {
            terminal_id: lazybox_ipc::TerminalId(0),
            hook: lazybox_ipc::HookEvent {
                kind: lazybox_ipc::HookEventKind::PreToolUse,
                session_id: Some("claude-session".into()),
                cwd: None,
                tool_name: Some("Bash".into()),
                notification: None,
            },
            backend_key: Some(key),
        })
        .expect("send hook");

    // The TUI connection sees the resulting state transition.
    let state = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match tui.recv().await {
                Some(Event::AgentState {
                    terminal_id: tid,
                    state,
                    ..
                }) if tid == spawned => break state,
                Some(_) => continue,
                None => panic!("stream closed before AgentState"),
            }
        }
    })
    .await
    .expect("AgentState from socket-forwarded hook");
    assert_eq!(state, lazybox_ipc::AgentState::Working);

    drop(ingest);
    drop(tui);
    shutdown.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

// ── Pure lifecycle helpers ─────────────────────────────────────────────

#[test]
fn runtime_dir_honors_env_var() {
    // Can't SET env in a parallel test safely, but we CAN confirm
    // the default path derivation uses HOME. Isolated via serial
    // mutex to avoid racing other env-reading tests.
    use std::sync::Mutex;
    static GUARD: Mutex<()> = Mutex::new(());
    let _g = GUARD.lock().unwrap();
    // SAFETY: same rationale as in Linear's from_env test.
    unsafe {
        std::env::set_var("LAZYBOX_RUNTIME_DIR", "/tmp/lazybox-test-runtime");
    }
    assert_eq!(
        lifecycle::runtime_dir(),
        PathBuf::from("/tmp/lazybox-test-runtime"),
    );
    unsafe {
        std::env::remove_var("LAZYBOX_RUNTIME_DIR");
    }
}

#[test]
fn read_pid_returns_none_for_missing_file() {
    let tmp = TempDir::new().unwrap();
    let pid = tmp.path().join("nope.pid");
    assert_eq!(lifecycle::read_pid(&pid).unwrap(), None);
}

#[test]
fn read_pid_returns_none_for_garbage_file() {
    let tmp = TempDir::new().unwrap();
    let pid = tmp.path().join("garbage.pid");
    std::fs::write(&pid, "not-a-pid").unwrap();
    assert_eq!(lifecycle::read_pid(&pid).unwrap(), None);
    // File was cleaned up as a side-effect.
    assert!(!pid.exists());
}

#[test]
fn read_pid_returns_none_for_dead_process() {
    let tmp = TempDir::new().unwrap();
    let pid_file = tmp.path().join("dead.pid");
    // Pick a PID that's almost certainly not alive: 999999 is above
    // typical pid_max on Linux/macOS so no live process can have it.
    std::fs::write(&pid_file, "999999\n").unwrap();
    assert_eq!(lifecycle::read_pid(&pid_file).unwrap(), None);
    assert!(!pid_file.exists(), "stale pid file cleaned up");
}

#[test]
fn read_pid_returns_live_pid() {
    let tmp = TempDir::new().unwrap();
    let pid_file = tmp.path().join("alive.pid");
    let my_pid = std::process::id();
    std::fs::write(&pid_file, format!("{my_pid}\n")).unwrap();
    assert_eq!(lifecycle::read_pid(&pid_file).unwrap(), Some(my_pid));
    // File is untouched on liveness confirmation.
    assert!(pid_file.exists());
}

#[test]
fn cleanup_stale_socket_returns_true_when_removed() {
    let tmp = TempDir::new().unwrap();
    let sock = tmp.path().join("stale.sock");
    std::fs::write(&sock, "x").unwrap();
    assert!(lifecycle::cleanup_stale_socket(&sock).unwrap());
    assert!(!sock.exists());
}

#[test]
fn cleanup_stale_socket_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let sock = tmp.path().join("never-was.sock");
    assert!(!lifecycle::cleanup_stale_socket(&sock).unwrap());
}
