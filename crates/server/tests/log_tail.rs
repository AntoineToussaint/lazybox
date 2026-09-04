//! End-to-end regression for the `lazybox log` console window (issue #1456).
//!
//! This drives the REAL client → daemon → `tail -F` → ring-buffer path over a
//! Unix socket, with the [`RawPtyBackend`] production uses — the same
//! transport and backend the CLI hits. Prior coverage only exercised
//! `sanitize_log_title` and protocol round-trips; nothing ran a live `tail -F`
//! against a temp file and asserted the streamed bytes surfaced, so a break
//! anywhere in the wiring passed CI silently and the window kept regressing.
//!
//! The CLI (`log_open` in `tui-boot`) creates an empty temp file, spawns a
//! `LogTail` window on it, waits for `CommandCompleted`, then appends the
//! piped bytes; `tail -F` streams them into the tile. `--close-all`
//! (`log_close_all`) opens a *fresh* connection, reads the subscribe snapshot
//! to find the workspace's `LogTail` terminals, and closes each. These tests
//! reproduce both flows against a shared daemon and assert the appended bytes
//! reach the subscriber as `TerminalOutput`, and that a second connection can
//! find the window in the snapshot and tear it down.

use lazybox_ipc::{Command, Event, TerminalId, TerminalKind, socket};
use lazybox_server::ServerConfig;
use lazybox_server::backend::RawPtyBackend;
use lazybox_server::socket_service::SocketService;
use lazybox_store::MemoryStore;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

/// Per-wait budget. A stall in the spawn/stream path fails the test rather
/// than hanging the suite; generous because a raw PTY + real `tail` under CI
/// load is slower than an in-memory mock.
const WAIT_BUDGET: Duration = Duration::from_secs(15);

/// A running daemon over a real Unix socket, backed by a raw-PTY backend so
/// `LogTail` spawns run an actual `tail -F`. The shared `ServerConfig` is
/// handed to every accepted connection, so a spawn on one connection is
/// visible to a second — exactly the embedded-mode wiring the CLI relies on.
struct Daemon {
    sock: std::path::PathBuf,
    handle: tokio::task::JoinHandle<()>,
    shutdown: Arc<tokio::sync::Notify>,
    _base: TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let base = TempDir::new().unwrap();
        let sock = base.path().join("daemon.sock");
        let pid = base.path().join("daemon.pid");
        let store = Arc::new(MemoryStore::new());
        let backend = Arc::new(RawPtyBackend::new());
        let config = ServerConfig::with_store_and_backend(store, backend);
        let service = SocketService::new(sock.clone(), pid, move || config.clone());
        let shutdown = service.shutdown_handle();
        let handle = tokio::spawn(async move {
            service.run().await.unwrap();
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !sock.exists() {
            assert!(tokio::time::Instant::now() < deadline, "socket never bound");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            sock,
            handle,
            shutdown,
            _base: base,
        }
    }

    async fn connect(&self) -> lazybox_ipc::Client {
        let (client, _peer) = socket::connect(&self.sock).await.expect("connect");
        client
    }

    async fn subscribed(&self) -> (lazybox_ipc::Client, Event) {
        let mut client = self.connect().await;
        client.send(Command::Subscribe).expect("subscribe");
        let snapshot = wait_for(&mut client, |e| matches!(e, Event::Snapshot { .. }))
            .await
            .expect("snapshot");
        (client, snapshot)
    }

    async fn stop(self) {
        self.shutdown.notify_one();
        let _ = timeout(Duration::from_secs(2), self.handle).await;
    }
}

/// Read events until `pred` matches or the budget elapses.
async fn wait_for<F: FnMut(&Event) -> bool>(
    client: &mut lazybox_ipc::Client,
    mut pred: F,
) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + WAIT_BUDGET;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match timeout(remaining, client.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return Some(ev);
                }
            }
            _ => return None,
        }
    }
}

/// Append bytes to the log file, the way the CLI's `drain_stdin_to` forwards
/// the piped producer's stdout.
fn append(path: &std::path::Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
}

/// Accumulates `TerminalOutput` for one terminal across successive waits.
///
/// A single `tail -F` read can deliver several appended lines in one chunk, so
/// a wait that stopped at the first needle would throw away bytes a later wait
/// still needs. Keeping one running buffer means each wait first checks what
/// already arrived, then reads more only if the needle isn't there yet. The
/// PTY rewrites `\n` to `\r\n`, so needles match on a substring of the buffer.
struct OutputReader {
    terminal_id: TerminalId,
    collected: Vec<u8>,
}

impl OutputReader {
    fn new(terminal_id: TerminalId) -> Self {
        Self {
            terminal_id,
            collected: Vec::new(),
        }
    }

    async fn wait_for(&mut self, client: &mut lazybox_ipc::Client, needle: &str) -> bool {
        if String::from_utf8_lossy(&self.collected).contains(needle) {
            return true;
        }
        let deadline = tokio::time::Instant::now() + WAIT_BUDGET;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match timeout(remaining, client.recv()).await {
                Ok(Some(Event::TerminalOutput {
                    terminal_id: id,
                    bytes,
                    ..
                })) if id == self.terminal_id => {
                    self.collected.extend_from_slice(&bytes);
                    if String::from_utf8_lossy(&self.collected).contains(needle) {
                        return true;
                    }
                }
                Ok(Some(_)) => continue,
                _ => return false,
            }
        }
    }
}

/// Spawn a `LogTail` window on `path`, mirroring the CLI's contract: send the
/// spawn with a correlation id, then confirm both `TerminalSpawned` (which
/// carries the id) and `CommandCompleted` (which the CLI blocks on before it
/// starts draining stdin) arrive.
async fn spawn_log_tail(
    client: &mut lazybox_ipc::Client,
    session_key: &str,
    path: &std::path::Path,
    cwd: &std::path::Path,
) -> TerminalId {
    let request_id = "logtail-req-1".to_string();
    client
        .send(Command::Spawn {
            session_key: session_key.into(),
            session_id: None,
            client_request_id: Some(request_id.clone()),
            kind: TerminalKind::LogTail {
                path: path.to_string_lossy().into_owned(),
            },
            cwd: Some(cwd.to_string_lossy().into_owned()),
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
            model_alias: None,
            access: Default::default(),
            force_new: false,
        })
        .unwrap();

    let spawned = wait_for(client, |e| {
        matches!(
            e,
            Event::TerminalSpawned {
                kind: TerminalKind::LogTail { .. },
                ..
            }
        )
    })
    .await
    .expect("TerminalSpawned for the log window");
    let terminal_id = match spawned {
        Event::TerminalSpawned { terminal_id, .. } => terminal_id,
        _ => unreachable!(),
    };

    // The CLI waits for this before draining stdin; a spawn that opened the
    // window but never confirmed would leave the CLI blocked for 60s.
    assert!(
        wait_for(client, |e| {
            matches!(e, Event::CommandCompleted { client_request_id } if client_request_id == &request_id)
        })
        .await
        .is_some(),
        "spawn must confirm with CommandCompleted",
    );

    terminal_id
}

/// The full happy path: open a window on a freshly-created empty temp file,
/// then append lines the way `drain_stdin_to` does and assert they stream into
/// the tile live. `tail -F` on an empty file is the exact edge the issue
/// flagged (follow-by-name must pick up the first append), so the file starts
/// empty and the bytes arrive only after the window is open.
#[tokio::test]
async fn log_tail_streams_appended_bytes_into_the_window() {
    let daemon = Daemon::start().await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("build-log");
    // The CLI's `create_log_file` makes an empty file up front.
    std::fs::File::create(&path).unwrap();

    let (mut client, _snapshot) = daemon.subscribed().await;
    let terminal_id = spawn_log_tail(&mut client, "test:log-ws", &path, dir.path()).await;
    let mut output = OutputReader::new(terminal_id);

    // Stream bytes in, as the piped producer would after the window is open.
    append(&path, b"first-line\nsecond-line\n");
    assert!(
        output.wait_for(&mut client, "first-line").await,
        "the first appended line must stream into the log window",
    );
    assert!(
        output.wait_for(&mut client, "second-line").await,
        "the second appended line must stream into the log window",
    );

    // A later append (a long-running command still producing output) must also
    // surface, proving the follow keeps up rather than stopping after one read.
    append(&path, b"third-line\n");
    assert!(
        output.wait_for(&mut client, "third-line").await,
        "a later append must also stream in (follow keeps running)",
    );

    daemon.stop().await;
}

/// `lazybox log --close-all` opens a fresh connection, reads the subscribe
/// snapshot to find the workspace's `LogTail` windows, and closes each. This
/// drives that exact shape: connection A opens the window, connection B (a
/// separate socket connection, as `log_close_all` uses) finds it in the
/// snapshot and closes it to `TerminalExited`.
#[tokio::test]
async fn log_tail_close_all_finds_and_tears_down_the_window() {
    let daemon = Daemon::start().await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("close-log");
    std::fs::File::create(&path).unwrap();

    let session_key = "test:close-ws";
    let (mut opener, _snapshot) = daemon.subscribed().await;
    let terminal_id = spawn_log_tail(&mut opener, session_key, &path, dir.path()).await;

    // A fresh connection, exactly like `log_close_all`: subscribe, read the
    // snapshot, filter to this workspace's LogTail windows.
    let (mut closer, snapshot) = daemon.subscribed().await;
    let log_ids: Vec<TerminalId> = match snapshot {
        Event::Snapshot { terminals, .. } => terminals
            .into_iter()
            .filter(|t| {
                t.session_key.as_str() == session_key
                    && matches!(t.kind, TerminalKind::LogTail { .. })
            })
            .map(|t| t.terminal_id)
            .collect(),
        _ => unreachable!(),
    };
    assert_eq!(
        log_ids,
        vec![terminal_id],
        "the fresh connection's snapshot must list the open log window",
    );

    closer
        .send(Command::Close {
            terminal_id,
            client_request_id: None,
        })
        .unwrap();
    assert!(
        wait_for(&mut closer, |e| {
            matches!(e, Event::TerminalExited { terminal_id: id, .. } if id == &terminal_id)
        })
        .await
        .is_some(),
        "Close must drive the log window to TerminalExited",
    );

    daemon.stop().await;
}
