//! Remote transport: length-prefixed bincode over a tokio `AsyncRead` /
//! `AsyncWrite`. Works with Unix sockets (local daemon) and SSH-tunneled
//! Unix sockets (remote daemon accessed via `ssh -L`).
//!
//! The surface mirrors `channel::pair` so the TUI receives the same
//! `Client` type regardless of transport. Internally, each transport
//! spawns a pair of tokio tasks that translate between the framed wire
//! and the local channels.

use crate::transport;
use crate::{
    BUILD_VERSION, Client, Command, Connection, EVENT_CHANNEL_CAPACITY, Event, EventForward,
    MAX_FRAME_BYTES, PROTOCOL_MAGIC, PROTOCOL_VERSION,
};
use serde::{Serialize, de::DeserializeOwned};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large ({0} bytes); max is {MAX_FRAME_BYTES}")]
    TooLarge(u32),
    #[error("encode/decode: {0}")]
    Codec(#[from] bincode::Error),
}

/// A protocol handshake failed before any frames were exchanged.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// The peer closed the connection (or errored) mid-handshake. On
    /// the client side this is the signature of a daemon that predates
    /// the handshake: our preamble looks like an oversized frame to
    /// its bincode decoder, so it drops the connection without
    /// replying.
    #[error(
        "connection closed during the protocol handshake ({0}) — the daemon \
         predates the protocol handshake or is not a lazybox daemon; restart \
         the daemon from this build"
    )]
    Io(#[from] std::io::Error),
    /// The peer's first 8 bytes don't start with [`PROTOCOL_MAGIC`].
    #[error(
        "peer sent an invalid protocol preamble {0:02x?} — not a lazybox \
         endpoint, or it predates the protocol handshake; restart the daemon \
         from this build"
    )]
    BadMagic([u8; 4]),
    /// Both sides speak the lazybox protocol but at different versions
    /// — daemon and client come from different builds.
    #[error(
        "protocol version mismatch: the peer is protocol v{peer}, this side \
         is v{ours} — daemon and client are from different builds; restart \
         the daemon so both sides match"
    )]
    VersionMismatch { peer: u32, ours: u32 },
}

/// What this side learned about the peer during the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// The peer's [`BUILD_VERSION`]. Equal to this side's `BUILD_VERSION`
    /// when both were compiled from the same commit; a difference means a
    /// stale daemon or client even though the protocol version matched.
    pub build: String,
}

impl PeerInfo {
    /// True when the peer was built from the same commit as this binary.
    pub fn build_matches(&self) -> bool {
        self.build == BUILD_VERSION
    }
}

/// Cap on the exchanged build-version string (it's a semver + short SHA,
/// well under this). Bounds the allocation a malformed peer can request.
const MAX_BUILD_BYTES: usize = 256;

/// Write this side's 8-byte preamble: magic + version (u32 LE).
async fn write_preamble<W>(w: &mut W) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut preamble = [0u8; 8];
    preamble[..4].copy_from_slice(&PROTOCOL_MAGIC);
    preamble[4..].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    w.write_all(&preamble).await?;
    w.flush().await
}

/// Write this side's build version: a u16 LE length followed by UTF-8
/// bytes. Sent only after the protocol versions are confirmed to match,
/// so a version-skewed (or pre-handshake) peer never reaches this and
/// can't be desynced by it.
async fn write_build<W>(w: &mut W) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = BUILD_VERSION.as_bytes();
    let len = bytes.len().min(MAX_BUILD_BYTES);
    w.write_all(&(len as u16).to_le_bytes()).await?;
    w.write_all(&bytes[..len]).await?;
    w.flush().await
}

/// Read the peer's build version written by [`write_build`].
async fn read_build<R>(r: &mut R) -> Result<PeerInfo, HandshakeError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 2];
    r.read_exact(&mut len_buf).await?;
    let len = u16::from_le_bytes(len_buf) as usize;
    if len > MAX_BUILD_BYTES {
        return Err(HandshakeError::Io(std::io::Error::other(
            "peer build-version string exceeds the maximum length",
        )));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(PeerInfo {
        build: String::from_utf8_lossy(&buf).into_owned(),
    })
}

/// Read and validate the peer's 8-byte preamble, returning its
/// protocol version.
async fn read_preamble<R>(r: &mut R) -> Result<u32, HandshakeError>
where
    R: AsyncRead + Unpin,
{
    let mut preamble = [0u8; 8];
    r.read_exact(&mut preamble).await?;
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&preamble[..4]);
    if magic != PROTOCOL_MAGIC {
        return Err(HandshakeError::BadMagic(magic));
    }
    let mut version = [0u8; 4];
    version.copy_from_slice(&preamble[4..]);
    Ok(u32::from_le_bytes(version))
}

/// Client side of the connection handshake: send our preamble, then
/// require a matching one back before any frames flow. Run this
/// immediately after the transport connects — the server won't accept
/// frames until it has seen our preamble.
pub async fn client_handshake<R, W>(rd: &mut R, wr: &mut W) -> Result<PeerInfo, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_preamble(wr).await?;
    let peer = read_preamble(rd).await?;
    if peer != PROTOCOL_VERSION {
        return Err(HandshakeError::VersionMismatch {
            peer,
            ours: PROTOCOL_VERSION,
        });
    }
    // Protocol matches; exchange build versions so a same-protocol but
    // different-commit daemon is still detectable by the caller.
    write_build(wr).await?;
    read_build(rd).await
}

/// Server side of the connection handshake: read the client's
/// preamble before entering the frame loop. A garbage preamble gets
/// no reply (the peer isn't speaking lazybox); a well-formed preamble
/// is always answered with our own — even on version mismatch — so
/// the client can render the clear "restart the daemon" error instead
/// of a bincode decode failure.
pub async fn server_handshake<R, W>(rd: &mut R, wr: &mut W) -> Result<PeerInfo, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let peer = read_preamble(rd).await?;
    write_preamble(wr).await?;
    if peer != PROTOCOL_VERSION {
        return Err(HandshakeError::VersionMismatch {
            peer,
            ours: PROTOCOL_VERSION,
        });
    }
    write_build(wr).await?;
    read_build(rd).await
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = bincode::serialize(msg)?;
    let len = u32::try_from(bytes.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(bincode::deserialize(&buf)?))
}

/// Connect to a daemon listening at `path` (possibly tunneled by SSH
/// when the path is forwarded through `ssh -L`). Runs the protocol
/// handshake before returning, so a version-skewed or pre-handshake
/// daemon surfaces here as a clear error instead of garbage frames.
/// Returns a `Client` whose send/recv map to frames on the wire, plus
/// the daemon's [`PeerInfo`] so the caller can warn on a build skew.
/// Transport is delegated to `transport::connect` — Unix domain
/// socket today, named pipe / TCP later.
pub async fn connect(path: &Path) -> std::io::Result<(Client, PeerInfo)> {
    let (mut rd, mut wr) = transport::connect(path)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let peer = client_handshake(&mut rd, &mut wr)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    // Events are read into a BOUNDED channel: when the TUI falls behind
    // and `Client::rx` fills, `reader_loop_bounded` blocks on the send,
    // which stops draining the socket and back-pressures all the way to
    // the daemon's per-connection forwarder — which is where the drop +
    // resync actually happens. The client never drops, it only stalls.
    let (evt_tx, evt_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);

    // Writer task: drain Commands from TUI, frame them onto the socket.
    tokio::spawn(writer_loop(wr, cmd_rx));
    // Reader task: parse framed Events from the socket, push to TUI.
    tokio::spawn(reader_loop_bounded(rd, evt_tx));

    Ok((Client::from_channels(cmd_tx, evt_rx), peer))
}

/// Connection-side: wrap an accepted socket as a `Connection` handle.
///
/// Mirrors [`channel::pair`](crate::channel::pair): the serve loop
/// writes raw events to an unbounded `tx`; the server-spawned forwarder
/// bridges them into a **bounded** channel that the `writer_loop`
/// frames onto the socket. A full bounded channel back-pressures the
/// forwarder (which then drops + re-syncs `TerminalOutput`), so the
/// daemon's send side can't grow without bound when a remote client
/// stops reading.
pub fn serve<R, W>(rd: R, wr: W) -> Connection
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Event>();
    let (client_tx, client_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
    tokio::spawn(reader_loop(rd, cmd_tx));
    tokio::spawn(writer_loop_bounded(wr, client_rx));
    Connection::with_forward(raw_tx, cmd_rx, EventForward { raw_rx, client_tx })
}

async fn writer_loop<W, M>(mut w: W, mut rx: mpsc::UnboundedReceiver<M>)
where
    W: AsyncWrite + Unpin,
    M: Serialize,
{
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write_frame(&mut w, &msg).await {
            tracing::warn!("writer: {e}");
            break;
        }
    }
}

/// Bounded-channel writer. Identical to [`writer_loop`] but drains a
/// bounded `Receiver` — used for the daemon's event stream so the
/// socket write rate back-pressures the forwarder.
async fn writer_loop_bounded<W, M>(mut w: W, mut rx: mpsc::Receiver<M>)
where
    W: AsyncWrite + Unpin,
    M: Serialize,
{
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write_frame(&mut w, &msg).await {
            tracing::warn!("writer: {e}");
            break;
        }
    }
}

async fn reader_loop<R, M>(mut r: R, tx: mpsc::UnboundedSender<M>)
where
    R: AsyncRead + Unpin,
    M: DeserializeOwned,
{
    loop {
        match read_frame::<_, M>(&mut r).await {
            Ok(Some(msg)) => {
                if tx.send(msg).is_err() {
                    break;
                }
            }
            Ok(None) => break, // clean EOF
            Err(e) => {
                tracing::warn!("reader: {e}");
                break;
            }
        }
    }
}

/// Bounded-channel reader. Like [`reader_loop`] but `.send().await`s on
/// a bounded `Sender`, so a full channel stalls socket reads and
/// back-pressures the peer instead of buffering frames without bound.
async fn reader_loop_bounded<R, M>(mut r: R, tx: mpsc::Sender<M>)
where
    R: AsyncRead + Unpin,
    M: DeserializeOwned,
{
    loop {
        match read_frame::<_, M>(&mut r).await {
            Ok(Some(msg)) => {
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
            Ok(None) => break, // clean EOF
            Err(e) => {
                tracing::warn!("reader: {e}");
                break;
            }
        }
    }
}
