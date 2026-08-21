//! The Unix socket server: bind the socket, accept clients, hand
//! each connection to a fresh `Server::serve` instance.
//!
//! Runs forever until `shutdown()` is called — typically by a signal
//! handler. Clean shutdown removes the socket + PID file so the next
//! start doesn't collide.

use crate::lifecycle;
use crate::{Server, ServerConfig};
use lazybox_ipc::{socket, transport};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

/// Default cap on concurrently handshaking/served daemon socket clients.
/// Normal use has one TUI plus a few short-lived hook helpers; this leaves
/// ample headroom while preventing a connection storm from spawning tasks
/// and per-connection queues without limit.
pub const DEFAULT_MAX_SOCKET_CONNECTIONS: usize = 32;

/// Number of connection slots reserved for interactive clients (TUI).
/// Under a hook storm, these slots stay available for the TUI to reconnect.
pub const RESERVED_INTERACTIVE_SLOTS: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum SocketServiceError {
    #[error("bind {path:?}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("pid file write: {0}")]
    Pid(std::io::Error),
    #[error("runtime dir: {0}")]
    Dir(std::io::Error),
}

pub struct SocketService {
    socket: PathBuf,
    pid_file: PathBuf,
    shutdown: Arc<Notify>,
    /// Graceful-stop broadcast to every connection's serve loop.
    /// Raised on shutdown BEFORE the connection tasks are aborted so
    /// each loop gets its bounded in-flight-mutation drain (a SIGTERM
    /// used to abort a merge save or worktree teardown mid-write).
    graceful_stop: tokio::sync::watch::Sender<bool>,
    /// Server config used to serve each new connection.
    config_factory: Box<dyn Fn() -> ServerConfig + Send + Sync>,
    max_connections: usize,
    handshake_timeout: Duration,
}

impl SocketService {
    /// Build a service that will bind `socket` and write its PID to
    /// `pid_file`. `config_factory` produces a fresh ServerConfig per
    /// connection (used by `Server::new` → `Server::serve`).
    pub fn new(
        socket: PathBuf,
        pid_file: PathBuf,
        config_factory: impl Fn() -> ServerConfig + Send + Sync + 'static,
    ) -> Self {
        Self {
            socket,
            pid_file,
            shutdown: Arc::new(Notify::new()),
            graceful_stop: tokio::sync::watch::channel(false).0,
            config_factory: Box::new(config_factory),
            max_connections: DEFAULT_MAX_SOCKET_CONNECTIONS,
            handshake_timeout: socket::HANDSHAKE_TIMEOUT,
        }
    }

    /// Override the connection cap (primarily for constrained deployments and
    /// deterministic admission tests). Zero still permits one connection.
    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections.max(1);
        self
    }

    /// Override how long an accepted client may occupy a connection slot
    /// without completing the protocol handshake. Zero is clamped to 1ms.
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Handle to trigger a graceful shutdown from elsewhere in the
    /// process (signal handler, test teardown). Dropping all handles
    /// also stops the service.
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    /// Bind + accept loop. Runs forever until `shutdown_handle()` is
    /// notified. Cleans up the socket + pid files on exit.
    pub async fn run(self) -> Result<(), SocketServiceError> {
        lifecycle::ensure_runtime_dir().map_err(SocketServiceError::Dir)?;

        // Clear a stale socket left by a prior crashed daemon, but do
        // not unlink a socket that still has a live listener. A missing
        // pidfile alone is not proof the daemon is dead.
        if self.socket.exists() {
            match transport::connect(&self.socket).await {
                Ok((_rd, _wr)) => {
                    return Err(SocketServiceError::Bind {
                        path: self.socket.clone(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::AddrInUse,
                            "daemon socket already has a live listener",
                        ),
                    });
                }
                Err(_) => {
                    let _ = lifecycle::cleanup_stale_socket(&self.socket);
                }
            }
        }

        let listener = transport::Listener::bind(&self.socket)
            .await
            .map_err(|e| match e {
                transport::TransportError::Bind { source, .. } => SocketServiceError::Bind {
                    path: self.socket.clone(),
                    source,
                },
                other => SocketServiceError::Bind {
                    path: self.socket.clone(),
                    source: std::io::Error::other(other.to_string()),
                },
            })?;

        lifecycle::write_pid_file(std::process::id(), &self.pid_file)
            .map_err(SocketServiceError::Pid)?;

        tracing::info!("lazybox-server listening on {}", self.socket.display());

        let shutdown = self.shutdown.clone();
        let connection_slots = Arc::new(Semaphore::new(self.max_connections));
        let mut connections = tokio::task::JoinSet::new();
        loop {
            while let Some(result) = connections.try_join_next() {
                if let Err(error) = result {
                    tracing::warn!("socket connection task failed: {error}");
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    tracing::info!("lazybox-server shutdown requested");
                    break;
                }
                accept = listener.accept() => {
                    let (mut rd, mut wr) = match accept {
                        Ok(pair) => pair,
                        Err(e) => {
                            // Back off on resource-exhaustion errors
                            // (2026-08-19 audit, R6): EMFILE/ENFILE make
                            // `accept()` fail instantly and forever, and a
                            // bare `continue` was a 100%-CPU spin that also
                            // flooded the log. Transient per-connection
                            // errors (ECONNABORTED) still retry at once.
                            let resource_exhausted = matches!(
                                &e,
                                lazybox_ipc::transport::TransportError::Accept(io) if matches!(
                                    io.raw_os_error(),
                                    Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
                                )
                            );
                            if resource_exhausted {
                                tracing::warn!("accept error (backing off 500ms): {e}");
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            } else {
                                tracing::warn!("accept error: {e}");
                            }
                            continue;
                        }
                    };

                    let config = (self.config_factory)();
                    let handshake_timeout = self.handshake_timeout;
                    let graceful_stop = self.graceful_stop.subscribe();
                    let slots = connection_slots.clone();
                    let max_conns = self.max_connections;

                    connections.spawn(async move {
                        // Handshake first: a client from a different build
                        // (or a non-lazybox peer) is turned away here instead
                        // of feeding bincode garbage into `Server::serve`.
                        let peer = match tokio::time::timeout(
                            handshake_timeout,
                            socket::server_handshake(&mut rd, &mut wr),
                        )
                        .await
                        {
                            Ok(Ok(peer)) => peer,
                            Ok(Err(error)) => {
                                tracing::warn!("rejecting connection: {error}");
                                return;
                            }
                            Err(_) => {
                                tracing::warn!(
                                    ?handshake_timeout,
                                    "rejecting connection: protocol handshake timed out"
                                );
                                return;
                            }
                        };

                        // Admission control with slot reservation for interactive clients.
                        // Interactive clients (TUI) reserve RESERVED_INTERACTIVE_SLOTS;
                        // background operations (hooks) use the remaining slots.
                        let is_interactive = peer.is_interactive;
                        let available = slots.available_permits();
                        let should_admit = if is_interactive {
                            // Interactive: admit if any slots are available
                            available > 0
                        } else {
                            // Background: admit only if non-reserved slots are available
                            available > RESERVED_INTERACTIVE_SLOTS
                        };

                        let permit = match should_admit {
                            true => {
                                match slots.clone().try_acquire_owned() {
                                    Ok(permit) => permit,
                                    Err(_) => {
                                        tracing::warn!(
                                            is_interactive,
                                            available,
                                            max_connections = max_conns,
                                            "connection admission check failed (race); rejecting client"
                                        );
                                        return;
                                    }
                                }
                            }
                            false => {
                                let reserved_status = if is_interactive {
                                    "no slots available".to_string()
                                } else {
                                    format!(
                                        "{} of {} slots are reserved for interactive clients",
                                        RESERVED_INTERACTIVE_SLOTS, max_conns
                                    )
                                };
                                tracing::warn!(
                                    is_interactive,
                                    available,
                                    max_connections = max_conns,
                                    reason = reserved_status,
                                    "socket connection limit reached — rejecting client"
                                );
                                return;
                            }
                        };

                        let _permit = permit;
                        if !peer.build_matches() {
                            tracing::warn!(
                                "client build {} differs from daemon build {} — \
                                 restart whichever is stale",
                                peer.build,
                                lazybox_ipc::BUILD_VERSION
                            );
                        }
                        let server = socket::serve(rd, wr);
                        let daemon = Server::new(config).with_graceful_stop(graceful_stop);
                        if let Err(e) = daemon.serve(server).await {
                            tracing::warn!("daemon serve: {e}");
                        }
                    });
                }
            }
        }

        // Stop accepting FIRST: drop the listener and unlink the socket
        // file before the multi-second drain below (2026-08-19 audit,
        // L4). While both survived the drain, the kernel kept
        // completing `connect()`s into a backlog nothing would ever
        // `accept()` — every hook helper that fired during teardown
        // (and teardown fires a burst of them) connected "successfully"
        // and then hung its full 5s handshake timeout. With the file
        // gone they get a clean instant ECONNREFUSED/ENOENT instead.
        drop(listener);
        let _ = std::fs::remove_file(&self.socket);

        // The service owns every accepted connection task. On explicit
        // shutdown (SIGTERM via the signal handler), first raise the
        // graceful-stop signal: each serve loop breaks and runs its own
        // bounded in-flight-mutation drain (`MUTATION_DRAIN_TIMEOUT`),
        // exactly as it would on a client disconnect. Aborting straight
        // away used to cancel a merge that had already succeeded
        // remotely before its local save, or a workspace delete between
        // terminal kill and worktree removal. The wait here is one
        // second longer than the per-connection drain so a healthy loop
        // always finishes on its own; anything still running past that
        // (a wedged transport, a stuck handshake) is cancelled and
        // joined as before, so shutdown stays bounded.
        let _ = self.graceful_stop.send(true);
        let drain_bound = crate::MUTATION_DRAIN_TIMEOUT + Duration::from_secs(1);
        let drain = async {
            while let Some(result) = connections.join_next().await {
                if let Err(error) = result {
                    tracing::warn!("socket connection task failed: {error}");
                }
            }
        };
        if tokio::time::timeout(drain_bound, drain).await.is_err() {
            tracing::warn!(
                ?drain_bound,
                "shutdown: connection task(s) still running past the drain bound — aborting them"
            );
        }
        connections.shutdown().await;

        // Gracefully terminate ephemeral (raw-PTY) sessions before the
        // process exits — durable backends (tmux) no-op. Without this
        // the kernel SIGHUP'd every child mid-write when the master
        // fds closed at exit (2026-08-19 audit, L2). Bounded inside
        // (SIGTERM → 2s → SIGKILL) with an outer belt.
        let config = (self.config_factory)();
        let _ =
            tokio::time::timeout(Duration::from_secs(4), config.backend.shutdown_sessions()).await;
        // Detached maintenance (background worktree removals) — see L6.
        let _ =
            tokio::time::timeout(Duration::from_secs(10), config.drain_maintenance_tasks()).await;

        // Cleanup: the socket file went at the top of shutdown; the pid
        // file goes last so the next `start` doesn't mistake us for
        // still-running.
        let _ = std::fs::remove_file(&self.pid_file);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that interactive clients (TUI) can acquire reserved slots even
    /// when background operations (hooks) have filled most slots.
    #[test]
    fn reserved_slots_protect_interactive_clients() {
        let max_conns = 8;
        let slots = Arc::new(Semaphore::new(max_conns));

        // Simulate: fill all non-reserved slots with background operations
        let mut permits = Vec::new();
        let non_reserved = max_conns - RESERVED_INTERACTIVE_SLOTS;
        for _ in 0..non_reserved {
            if let Ok(permit) = slots.clone().try_acquire_owned() {
                permits.push(permit);
            }
        }
        assert_eq!(
            permits.len(),
            non_reserved,
            "should acquire non-reserved slots"
        );
        assert_eq!(slots.available_permits(), RESERVED_INTERACTIVE_SLOTS);

        // Now try to admit a hook (not interactive):
        // It should be rejected because only reserved slots remain
        let hook_available = slots.available_permits();
        let hook_should_admit = if false {
            // Not interactive
            hook_available > RESERVED_INTERACTIVE_SLOTS
        } else {
            hook_available > RESERVED_INTERACTIVE_SLOTS
        };
        assert!(
            !hook_should_admit,
            "hook should be rejected when only reserved slots remain"
        );

        // But an interactive client (TUI) should be admitted:
        // It should be admitted because reserved slots are still available
        let tui_available = slots.available_permits();
        let tui_should_admit = if true {
            // Interactive
            tui_available > 0
        } else {
            tui_available > RESERVED_INTERACTIVE_SLOTS
        };
        assert!(
            tui_should_admit,
            "TUI should be admitted when reserved slots remain"
        );

        // Verify the TUI can actually acquire one of the reserved slots
        let tui_permit = slots.clone().try_acquire_owned();
        assert!(
            tui_permit.is_ok(),
            "TUI must be able to acquire a reserved slot"
        );

        drop(tui_permit);
        drop(permits); // Release hook permits
    }

    /// Test that under a hook storm, reserved slots keep the TUI accessible.
    #[test]
    fn hook_storm_doesnt_block_tui_reconnect() {
        let max_conns = RESERVED_INTERACTIVE_SLOTS + 2; // Just barely more than reserved
        let slots = Arc::new(Semaphore::new(max_conns));

        // Simulate a hook storm: background operations fill all non-reserved
        let mut hook_permits = Vec::new();
        for _ in 0..(max_conns - RESERVED_INTERACTIVE_SLOTS) {
            if let Ok(permit) = slots.clone().try_acquire_owned() {
                hook_permits.push(permit);
            }
        }

        // Now the TUI tries to reconnect: at least one reserved slot must still be available
        let available = slots.available_permits();
        assert_eq!(
            available, RESERVED_INTERACTIVE_SLOTS,
            "reserved slots must be available"
        );

        // TUI can admit because it's interactive and slots remain
        assert!(
            available > 0,
            "at least one reserved slot must be available for TUI"
        );
        let tui_permit = slots.clone().try_acquire_owned();
        assert!(
            tui_permit.is_ok(),
            "TUI reconnect must succeed even under hook storm"
        );

        // Further hooks should still be rejected (no non-reserved slots left)
        let should_admit_another_hook = slots.available_permits() > RESERVED_INTERACTIVE_SLOTS;
        assert!(
            !should_admit_another_hook,
            "new hooks should be rejected when only TUI-reserved slots remain"
        );
    }
}
