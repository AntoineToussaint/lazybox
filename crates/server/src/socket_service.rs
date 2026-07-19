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
                            tracing::warn!("accept error: {e}");
                            continue;
                        }
                    };
                    let permit = match connection_slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            tracing::warn!(
                                max_connections = self.max_connections,
                                "socket connection limit reached — rejecting client"
                            );
                            continue;
                        }
                    };
                    let config = (self.config_factory)();
                    let handshake_timeout = self.handshake_timeout;
                    connections.spawn(async move {
                        let _permit = permit;
                        // Handshake before the frame loop: a client from
                        // a different build (or a non-lazybox peer) is
                        // turned away here instead of feeding bincode
                        // garbage into `Server::serve`.
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
                        if !peer.build_matches() {
                            tracing::warn!(
                                "client build {} differs from daemon build {} — \
                                 restart whichever is stale",
                                peer.build,
                                lazybox_ipc::BUILD_VERSION
                            );
                        }
                        let server = socket::serve(rd, wr);
                        let daemon = Server::new(config);
                        if let Err(e) = daemon.serve(server).await {
                            tracing::warn!("daemon serve: {e}");
                        }
                    });
                }
            }
        }

        // The service owns every accepted connection task. Explicit daemon
        // shutdown cancels and joins them instead of leaving detached socket
        // readers/writers alive past listener cleanup.
        connections.shutdown().await;

        // Cleanup: drop the socket file + pid file so next `start`
        // doesn't mistake us for still-running.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.pid_file);
        Ok(())
    }
}
