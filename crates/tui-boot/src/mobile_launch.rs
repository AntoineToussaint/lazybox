//! Mobile clients attach to a persistent daemon, so closing the UI keeps PTYs alive.
use lazybox_server::lifecycle;
use std::{path::PathBuf, time::Duration};

async fn listening(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(path).await.is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

pub(crate) async fn ensure_daemon() -> anyhow::Result<PathBuf> {
    let socket = lifecycle::socket_path();
    // A configured tunnel owns the connection; run_remote brings it up.
    if lazybox_config::Config::load()
        .ok()
        .is_some_and(|c| c.remote.tunnel.is_some())
    {
        return Ok(socket);
    }
    if listening(&socket).await {
        return Ok(socket);
    }
    #[cfg(not(unix))]
    anyhow::bail!("Use lb -m --connect with a running daemon on this platform.");
    #[cfg(unix)]
    {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        lifecycle::ensure_runtime_dir()?;
        // Serialize mobile launches. Keep the lock file: unlinking it while
        // another process is waiting would permit two unrelated lock inodes.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lifecycle::runtime_dir().join("mobile-start.lock"))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            // SAFETY: lock owns this live file descriptor throughout the call.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error.into());
            }
            if listening(&socket).await {
                return Ok(socket);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("Another mobile launch is still starting the daemon; retry shortly.");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if listening(&socket).await {
            return Ok(socket);
        }
        let log_path = lifecycle::runtime_dir().join("mobile-daemon.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args(["server", "start"])
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // SAFETY: setsid is async-signal-safe; no allocation or locks in the child hook.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn()?;
        loop {
            if listening(&socket).await {
                return Ok(socket);
            }
            if let Some(status) = child.try_wait()? {
                // A separately started desktop/daemon may have won the bind.
                if listening(&socket).await {
                    return Ok(socket);
                }
                anyhow::bail!(
                    "Session service exited ({status}); see {}",
                    log_path.display()
                );
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "Session service is still starting; see {} and retry shortly",
                    log_path.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn readiness_requires_a_live_listener_not_a_stale_socket_file() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("daemon.sock");
            assert!(!listening(&path).await);
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            assert!(listening(&path).await);
            drop(listener);
            assert!(!listening(&path).await);
        })
        .await
        .expect("socket readiness check must finish within its test budget");
    }
}
