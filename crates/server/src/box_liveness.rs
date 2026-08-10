//! Box-lifecycle liveness beacon (#978).
//!
//! The stop-on-idle timer on a remote box (`contrib/box-lifecycle/`) can only
//! see inbound SSH and the watched agent process names — it has no window into
//! the daemon's own sessions. A client attached over a relay (which, unlike an
//! IAP tunnel, does *not* present as inbound sshd) plus a daemon holding live
//! PTYs would read as idle and get reaped mid-task.
//!
//! This task keeps a liveness file's mtime fresh while the daemon has ≥1 live
//! terminal; the idle-stop script treats a fresh mtime as busy. When no daemon
//! runs (a bare box) the file simply never appears and the script's behavior is
//! unchanged. Touch-semantics only — no wire surface, no config.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{ServerConfig, TerminalRegistry};

/// How often to refresh the liveness file while terminals are live. Well under
/// the script's default freshness window (2 timer ticks = 10 min), so a single
/// missed tick never makes a live daemon read as stale.
const TICK: Duration = Duration::from_secs(60);

/// The liveness file the idle-stop script's `LAZYBOX_IDLE_ACTIVE_FILE` defaults
/// to (`~/.lazybox/run/active`).
pub fn active_file() -> PathBuf {
    lazybox_core::paths::runtime_dir().join("active")
}

/// Spawn the liveness beacon. Cheap enough to always run: it does nothing but
/// stat the terminal registry once a minute and, when non-empty, touch a file.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    let terminals = config.terminal.clone();
    tokio::spawn(async move {
        run(terminals, active_file(), TICK).await;
    })
}

/// Refresh `path`'s mtime on every tick the daemon holds a live terminal. Never
/// returns in production; the daemon drops the runtime to exit, which drops
/// this future mid-`tick`.
async fn run(terminals: TerminalRegistry, path: PathBuf, tick: Duration) {
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if terminals.terminal_count().await > 0 {
            touch(&path);
        }
    }
}

/// Create-or-refresh `path`'s mtime. Truncating and writing the daemon pid
/// guarantees the mtime advances on every platform (an empty create-only open
/// leaves an existing file's mtime untouched). A failure is logged, not fatal:
/// the beacon is best-effort and the script fails safe toward "busy" anyway.
fn touch(path: &Path) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        create_private_dir(parent);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", std::process::id());
        }
        Err(e) => tracing::debug!("box-liveness: touch {} failed: {e}", path.display()),
    }
}

/// Create `dir` (and parents) private to the owner. The beacon's parent is the
/// daemon runtime dir, which holds `daemon.sock` and must stay `0700` — a plain
/// `create_dir_all` under the default umask could leave it world-traversable if
/// the beacon's first tick wins the race to create it before the socket
/// service does. A `0700` mode has no group/other bits for the umask to strip,
/// so the result is exactly `0700`. No-op when the dir already exists (the
/// common case: the socket service created it first).
fn create_private_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::create_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::SessionKey;
    use lazybox_ipc::{TerminalId, TerminalKind};

    fn mtime(path: &Path) -> Option<std::time::SystemTime> {
        std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
    }

    #[test]
    fn touch_creates_and_advances_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("active");
        assert!(mtime(&path).is_none(), "file starts absent");

        touch(&path);
        let first = mtime(&path).expect("touch creates the file");

        // A second write must move the mtime forward, or the script could read
        // a live daemon as stale. Sleep past the filesystem's mtime resolution.
        std::thread::sleep(Duration::from_millis(1100));
        touch(&path);
        let second = mtime(&path).expect("still present");
        assert!(second > first, "touch must advance the mtime");
    }

    #[cfg(unix)]
    #[test]
    fn touch_creates_its_parent_private() {
        use std::os::unix::fs::PermissionsExt;
        // The beacon's parent is the daemon runtime dir, which holds
        // `daemon.sock` and must stay 0700. If the beacon's first tick creates
        // it under the default umask it could be world-traversable. Touch a
        // path two levels below a fresh tempdir so `touch` has to create the
        // parent, then assert it came out 0700.
        let dir = tempfile::tempdir().expect("tempdir");
        let run_dir = dir.path().join("run");
        let path = run_dir.join("active");
        assert!(!run_dir.exists(), "parent starts absent");

        touch(&path);

        let mode = std::fs::metadata(&run_dir)
            .expect("parent created")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "the beacon must create its parent dir 0700, not world-traversable"
        );
        assert!(mtime(&path).is_some(), "the liveness file was written");
    }

    #[tokio::test(start_paused = true)]
    async fn beacon_touches_only_while_terminals_are_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("active");
        let terminals = TerminalRegistry::default();

        let task = {
            let terminals = terminals.clone();
            let path = path.clone();
            tokio::spawn(async move { run(terminals, path, Duration::from_secs(60)).await })
        };

        // No terminals: the first tick must not create the file.
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert!(
            mtime(&path).is_none(),
            "an empty daemon must not signal live"
        );

        // A live terminal: the next tick must stamp the beacon.
        terminals
            .register_terminal(
                TerminalId(1),
                "backend".into(),
                SessionKey::from("github:o/r#1"),
                TerminalKind::Agent("claude".into()),
            )
            .await;
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert!(
            mtime(&path).is_some(),
            "a daemon holding a live PTY must refresh the liveness file"
        );

        task.abort();
    }
}
