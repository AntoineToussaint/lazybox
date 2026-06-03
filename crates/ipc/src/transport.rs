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
    pub async fn accept(&self) -> Result<(BoxRead, BoxWrite), TransportError> {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix { listener, .. } => {
                let (stream, _addr) = listener.accept().await.map_err(TransportError::Accept)?;
                let (rd, wr) = stream.into_split();
                Ok((Box::new(rd), Box::new(wr)))
            }
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
