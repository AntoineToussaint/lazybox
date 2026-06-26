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
use tokio::sync::Notify;

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
        }
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
        loop {
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
                    let config = (self.config_factory)();
                    tokio::spawn(async move {
                        // Handshake before the frame loop: a client from
                        // a different build (or a non-lazybox peer) is
                        // turned away here instead of feeding bincode
                        // garbage into `Server::serve`.
                        match socket::server_handshake(&mut rd, &mut wr).await {
                            Ok(peer) if !peer.build_matches() => tracing::warn!(
                                "client build {} differs from daemon build {} — \
                                 restart whichever is stale",
                                peer.build,
                                lazybox_ipc::BUILD_VERSION
                            ),
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!("rejecting connection: {e}");
                                return;
                            }
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

        // Cleanup: drop the socket file + pid file so next `start`
        // doesn't mistake us for still-running.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.pid_file);
        Ok(())
    }
}
