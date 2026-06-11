//! Cross-platform daemon transport. Hides the difference between
//! Unix domain sockets (current implementation) and Windows named
//! pipes / TCP loopback (future) behind two functions:
//!
//! - [`Listener::bind`]: server-side — open a listener at `path` and yield
//!   `(read, write)` halves for each accepted connection.
//! - [`connect`]: client-side — open a client connection to `path`
//!   and return a streaming `(read, write)` pair.
//!
//! Both halves are returned as boxed `AsyncRead`/`AsyncWrite` so
//! `lazybox-ipc::socket::{serve, connect}` can stay generic over the
//! transport. The unix impl uses `tokio::net::UnixStream`; the
//! windows arm is a `TODO` that compiles to a no-op so the rest of
//! the workspace builds on Windows even before the port lands.

use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncWrite};

/// Type-erased async-read half of a transport connection.
pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
/// Type-erased async-write half of a transport connection.
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Errors from `bind` / `connect` / `accept`.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("bind {path}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("connect {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("accept: {0}")]
    Accept(std::io::Error),
    #[error("transport not supported on this platform")]
    Unsupported,
}

/// Server-side: opens a listener at `path` and accepts connections.
pub struct Listener {
    inner: ListenerInner,
}

enum ListenerInner {
    #[cfg(unix)]
    Unix {
        listener: tokio::net::UnixListener,
        path: PathBuf,
    },
    #[cfg(not(unix))]
    Unsupported,
}

impl Listener {
    /// Bind a new listener at the given filesystem-style path. On
    /// Unix this is a domain socket; on Windows (TODO) this would be
    /// a named pipe with `\\.\pipe\<name>` shape.
    pub async fn bind(path: &Path) -> Result<Self, TransportError> {
        #[cfg(unix)]
        {
            let listener =
                tokio::net::UnixListener::bind(path).map_err(|e| TransportError::Bind {
                    path: path.to_path_buf(),
                    source: e,
                })?;
            // Owner-only socket file: connecting at all requires write
            // permission on it, so 0600 shuts out other local users
            // even before the per-connection peer_cred check below.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
                |e| TransportError::Bind {
                    path: path.to_path_buf(),
                    source: e,
                },
            )?;
            Ok(Self {
                inner: ListenerInner::Unix {
                    listener,
                    path: path.to_path_buf(),
                },
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            // TODO(windows): tokio::net::windows::named_pipe::ServerOptions::new().create(...)
            Err(TransportError::Unsupported)
        }
    }

    /// Accept the next connection, returning the (read, write)
    /// halves. The streams are type-erased so callers don't need to
    /// know which transport delivered them.
    ///
    /// Connections from any uid other than this process's effective
    /// uid are dropped with a warning and never surfaced to the
    /// caller — the daemon speaks for one user only.
    pub async fn accept(&self) -> Result<(BoxRead, BoxWrite), TransportError> {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix { listener, .. } => loop {
                let (stream, _addr) = listener.accept().await.map_err(TransportError::Accept)?;
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == process_euid() => {
                        let (rd, wr) = stream.into_split();
                        return Ok((Box::new(rd), Box::new(wr)));
                    }
                    Ok(cred) => {
                        tracing::warn!(
                            peer_uid = cred.uid(),
                            our_uid = process_euid(),
                            "rejecting daemon socket connection from another uid"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("rejecting connection: peer_cred failed: {e}");
                    }
                }
            },
            #[cfg(not(unix))]
            ListenerInner::Unsupported => Err(TransportError::Unsupported),
        }
    }

    /// The path this listener is bound to. Used by the server to
    /// remove the socket file on shutdown (Unix-only — Windows named
    /// pipes vanish with the handle).
    pub fn path(&self) -> &Path {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix { path, .. } => path,
            #[cfg(not(unix))]
            ListenerInner::Unsupported => Path::new(""),
        }
    }
}

/// This process's effective uid, via the raw libc symbol — `geteuid`
/// can never fail, so it's declared `safe`. Avoids pulling the `libc`
/// crate into the protocol crate for one call.
#[cfg(unix)]
fn process_euid() -> u32 {
    unsafe extern "C" {
        safe fn geteuid() -> u32;
    }
    geteuid()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temp_sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lazybox-transport-{tag}-{}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn bind_sets_socket_owner_only_and_accepts_same_uid() {
        use std::os::unix::fs::PermissionsExt;
        let sock = temp_sock("perms");
        let _ = std::fs::remove_file(&sock);

        let listener = Listener::bind(&sock).await.expect("bind");
        let mode = std::fs::metadata(&sock)
            .expect("stat socket")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "socket file must be owner-only");

        // Same-uid peers (the only kind a test can produce) pass the
        // peer_cred gate.
        let (accepted, connected) = tokio::join!(listener.accept(), connect(&sock));
        accepted.expect("accept same-uid peer");
        connected.expect("connect");

        let _ = std::fs::remove_file(&sock);
    }
}

/// Client-side: open a connection to `path`. Mirrors `Listener::bind`
/// — same path semantics across platforms.
pub async fn connect(path: &Path) -> Result<(BoxRead, BoxWrite), TransportError> {
    #[cfg(unix)]
    {
        let stream =
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| TransportError::Connect {
                    path: path.to_path_buf(),
                    source: e,
                })?;
        let (rd, wr) = stream.into_split();
        Ok((Box::new(rd), Box::new(wr)))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        // TODO(windows): tokio::net::windows::named_pipe::ClientOptions::new().open(path)
        Err(TransportError::Unsupported)
    }
}
