//! Server lifecycle: socket path resolution, PID file, signal
//! handling. All the bits that turn a `Server::serve()` loop into a
//! long-running service one can start, stop, and status-check.
//!
//! ## Layout on disk
//!
//! Everything under `$LAZYBOX_RUNTIME_DIR` (defaults to
//! `~/.lazybox/run/`):
//!
//! ```text
//! run/
//!   daemon.sock   Unix socket — where clients connect
//!   daemon.pid    PID of the running daemon (written on start)
//! ```
//!
//! Clients resolve the socket via the same paths, so `lazybox` and
//! `lazybox server *` agree without having to pass paths around.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Canonical daemon run-dir. Override via `LAZYBOX_RUNTIME_DIR` (legacy
/// — wins over `LAZYBOX_HOME` for back-compat) or via `LAZYBOX_HOME` (the
/// preferred multi-profile knob). Implementation lives in
/// [`lazybox_core::paths::runtime_dir`]; this thin re-export keeps the
/// existing `lifecycle::runtime_dir` import path working.
pub fn runtime_dir() -> PathBuf {
    lazybox_core::paths::runtime_dir()
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("daemon.pid")
}

/// Ensure the runtime dir exists. Called at daemon start + status.
/// Created 0700 (it holds the daemon socket — other local users must
/// not be able to traverse into it); any parent we create (typically
/// `~/.lazybox` itself) gets the same mode.
pub fn ensure_runtime_dir() -> std::io::Result<()> {
    let dir = runtime_dir();
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        // DirBuilder's mode is filtered through the umask; pin the
        // final dir to exactly 0700.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&dir)?;
    Ok(())
}

/// Write the current process's PID into `daemon.pid`. Overwrites any
/// existing file (stale PIDs are cleaned up in `read_pid` below).
pub fn write_pid_file(pid: u32, path: &Path) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "{pid}")?;
    Ok(())
}

/// Read `daemon.pid`. Returns:
/// - `Ok(Some(pid))` — file present, parsed, and the process is alive.
/// - `Ok(None)` — file missing, empty, unparseable, or refers to a
///   dead pid (stale file gets deleted as a side-effect).
pub fn read_pid(path: &Path) -> std::io::Result<Option<u32>> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let Ok(pid) = raw.trim().parse::<u32>() else {
        let _ = std::fs::remove_file(path);
        return Ok(None);
    };
    if is_alive(pid) {
        Ok(Some(pid))
    } else {
        let _ = std::fs::remove_file(path);
        Ok(None)
    }
}

/// True if `pid` refers to a live process. `kill(pid, 0)` is the
/// standard Unix liveness probe — doesn't actually signal, just
/// succeeds iff the process exists AND we're allowed to signal it.
fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Remove a socket file that's left over from a prior run (daemon
/// killed via SIGKILL, system crash, etc.). Returns true if a file
/// was removed. Idempotent: missing file → Ok(false), no error.
pub fn cleanup_stale_socket(path: &Path) -> std::io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

/// Status of the daemon. Distinct from `None` vs `Some(pid)` because
/// callers want to render "running (pid 1234)" vs "stopped" vs
/// "stale pidfile cleaned up."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Running { pid: u32 },
    Stopped,
}

pub fn status() -> ServerStatus {
    match read_pid(&pid_path()).unwrap_or(None) {
        Some(pid) => ServerStatus::Running { pid },
        None => ServerStatus::Stopped,
    }
}

/// Send SIGTERM to the running daemon, if any. Returns true if a
/// signal was sent (caller may want to wait for the socket file to
/// disappear as a shutdown confirmation).
pub fn request_stop() -> std::io::Result<bool> {
    let Some(pid) = read_pid(&pid_path())? else {
        return Ok(false);
    };
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, libc::SIGTERM) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(true)
}

/// Consume one lifecycle-hook payload from stdin and forward it to the
/// daemon socket. Hook delivery is deliberately best-effort: agent CLIs
/// treat a non-zero hook exit as a user-visible failure, while a stopped or
/// restarting lazybox daemon should only mean that one state signal is lost.
pub async fn ingest_hook_from_stdio(args: &[String]) {
    let (backend_key, terminal_id) = parse_hook_correlation(args);
    if backend_key.is_none() && terminal_id.is_none() {
        let _ = read_stdin_to_string();
        return;
    }

    let payload = read_stdin_to_string();
    let Some(hook) = lazybox_agents::hook::parse_claude_hook(&payload) else {
        return;
    };
    let command = lazybox_ipc::Command::IngestHook {
        terminal_id: lazybox_ipc::TerminalId(terminal_id.unwrap_or_default()),
        hook,
        backend_key,
    };
    if let Err(error) = lazybox_ipc::socket::send_command(&socket_path(), &command).await {
        tracing::warn!("hook-ingest IPC send failed: {error}");
    }
}

/// Parse the correlation flags accepted by hook helpers. Unknown flags are
/// ignored so settings generated by a newer daemon remain harmless when an
/// older helper happens to run them.
pub fn parse_hook_correlation(args: &[String]) -> (Option<String>, Option<u64>) {
    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    let backend_key = value("--backend-key")
        .or_else(|| {
            value("--backend-key-file")
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map(|key| key.trim().to_string())
        })
        .filter(|key| !key.is_empty());
    let terminal_id = value("--terminal").and_then(|value| value.parse().ok());
    (backend_key, terminal_id)
}

fn read_stdin_to_string() -> String {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    input
}

#[cfg(test)]
mod hook_tests {
    use super::*;

    #[test]
    fn hook_correlation_accepts_backend_key_and_legacy_terminal() {
        let args = vec![
            "--unknown".to_string(),
            "ignored".to_string(),
            "--backend-key".to_string(),
            "lzb-session-7".to_string(),
            "--terminal".to_string(),
            "42".to_string(),
        ];

        assert_eq!(
            parse_hook_correlation(&args),
            (Some("lzb-session-7".to_string()), Some(42))
        );
    }

    #[test]
    fn hook_correlation_reads_backend_key_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("backend-key");
        std::fs::write(&path, "lzb-file-key\n").expect("write key");
        let args = vec!["--backend-key-file".to_string(), path.display().to_string()];

        assert_eq!(
            parse_hook_correlation(&args),
            (Some("lzb-file-key".to_string()), None)
        );
    }
}
