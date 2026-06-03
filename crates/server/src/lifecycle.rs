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
//! `lazybox daemon *` agree without having to pass paths around.

use std::io::Write;
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
pub fn ensure_runtime_dir() -> std::io::Result<()> {
    let dir = runtime_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
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
