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
    BUILD_VERSION, COMMAND_CHANNEL_CAPACITY, Client, Command, Connection, ConnectionState,
    ConnectionStatus, EVENT_CHANNEL_CAPACITY, Event, MAX_COMMAND_FRAME_BYTES, MAX_FRAME_BYTES,
    PROTOCOL_FINGERPRINT, PROTOCOL_MAGIC, event_forward_channel,
};
use serde::{Serialize, de::DeserializeOwned};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large ({0} bytes); exceeds this channel's limit")]
    TooLarge(u32),
    /// A frame's declared length exceeded this channel's ingress cap
    /// but stayed within [`MAX_FRAME_BYTES`], i.e. a well-framed peer
    /// sent one legitimately-encoded message that is just too big for
    /// this channel (e.g. an old client shipping a >256 KiB paste as a
    /// single `Command::Write`). By the time this error is returned
    /// the reader has already consumed and discarded the payload, so
    /// the stream is still frame-aligned: callers may drop this one
    /// message and keep reading. Lengths beyond [`MAX_FRAME_BYTES`]
    /// are [`FrameError::TooLarge`] instead — no lazybox peer ever
    /// writes such a frame (`encode_frame` enforces the ceiling), so a
    /// wilder prefix means the stream itself is corrupt and fatal.
    #[error(
        "skipped an oversized frame ({len} bytes; this channel's limit is {max}) — \
         the message was discarded but the stream remains in sync"
    )]
    OversizedSkipped { len: u32, max: u32 },
    #[error("encode: {0}")]
    Encode(#[from] bincode::error::EncodeError),
    #[error("decode: {0}")]
    Decode(#[from] bincode::error::DecodeError),
    #[error("decode left {0} trailing bytes in the frame")]
    TrailingBytes(usize),
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
    /// Both sides speak the lazybox protocol but their wire
    /// fingerprints (derived from the wire-defining sources at build
    /// time) differ — daemon and client come from different builds and
    /// would misread each other's frames.
    #[error(
        "wire fingerprint mismatch: the peer reports {peer:#010x}, this side \
         {ours:#010x} — daemon and client are from different builds; restart \
         the daemon so both sides match"
    )]
    FingerprintMismatch { peer: u32, ours: u32 },
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

/// Maximum time either side should let a connected peer occupy resources
/// without completing the fixed-size protocol/build handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Write this side's 8-byte preamble: magic + wire fingerprint (u32 LE).
async fn write_preamble<W>(w: &mut W) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut preamble = [0u8; 8];
    preamble[..4].copy_from_slice(&PROTOCOL_MAGIC);
    preamble[4..].copy_from_slice(&PROTOCOL_FINGERPRINT.to_le_bytes());
    w.write_all(&preamble).await?;
    w.flush().await
}

/// Write this side's build version: a u16 LE length followed by UTF-8
/// bytes. Sent only after the wire fingerprints are confirmed to match,
/// so a mismatched (or pre-handshake) peer never reaches this and
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
/// wire fingerprint.
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
    if peer != PROTOCOL_FINGERPRINT {
        return Err(HandshakeError::FingerprintMismatch {
            peer,
            ours: PROTOCOL_FINGERPRINT,
        });
    }
    // Fingerprints match; exchange build versions so a wire-compatible
    // but different-commit daemon is still detectable by the caller.
    write_build(wr).await?;
    read_build(rd).await
}

/// Server side of the connection handshake: read the client's
/// preamble before entering the frame loop. A garbage preamble gets
/// no reply (the peer isn't speaking lazybox); a well-formed preamble
/// is always answered with our own — even on fingerprint mismatch — so
/// the client can render the clear "restart the daemon" error instead
/// of a bincode decode failure.
pub async fn server_handshake<R, W>(rd: &mut R, wr: &mut W) -> Result<PeerInfo, HandshakeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let peer = read_preamble(rd).await?;
    write_preamble(wr).await?;
    if peer != PROTOCOL_FINGERPRINT {
        return Err(HandshakeError::FingerprintMismatch {
            peer,
            ours: PROTOCOL_FINGERPRINT,
        });
    }
    write_build(wr).await?;
    read_build(rd).await
}

async fn client_handshake_with_timeout<R, W>(
    rd: &mut R,
    wr: &mut W,
    timeout: Duration,
) -> std::io::Result<PeerInfo>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    tokio::time::timeout(timeout, client_handshake(rd, wr))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "protocol handshake timed out")
        })?
        .map_err(|error| std::io::Error::other(error.to_string()))
}

/// Connect, complete the bounded protocol handshake, and send one
/// command without starting the long-lived client tasks.
pub async fn send_command(path: &Path, command: &Command) -> std::io::Result<PeerInfo> {
    send_command_with_timeout(path, command, HANDSHAKE_TIMEOUT).await
}

async fn send_command_with_timeout(
    path: &Path,
    command: &Command,
    handshake_timeout: Duration,
) -> std::io::Result<PeerInfo> {
    let (mut rd, mut wr) = transport::connect(path)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let peer = client_handshake_with_timeout(&mut rd, &mut wr, handshake_timeout).await?;
    write_frame(&mut wr, command)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(peer)
}

/// Encode one message as a length-prefixed wire frame:
/// `[u32 BE length][bincode payload]`. Shared by the single-frame
/// writer and the batching event writer so the framing (and the
/// `MAX_FRAME_BYTES` check) exists in exactly one place.
fn encode_frame<T>(msg: &T) -> Result<Vec<u8>, FrameError>
where
    T: Serialize,
{
    // `legacy()` preserves bincode 1's wire representation while using
    // bincode 2's maintained API. The explicit limit also constrains
    // allocations requested by a malicious payload, independently of
    // the outer frame-length check.
    let config = bincode::config::legacy().with_limit::<{ MAX_FRAME_BYTES as usize }>();
    let bytes = bincode::serde::encode_to_vec(msg, config)?;
    let len = u32::try_from(bytes.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut framed = Vec::with_capacity(4 + bytes.len());
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(&bytes);
    Ok(framed)
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let framed = encode_frame(msg)?;
    w.write_all(&framed).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    read_frame_limited(r, MAX_FRAME_BYTES).await
}

async fn read_frame_limited<R, T>(r: &mut R, max_frame_bytes: u32) -> Result<Option<T>, FrameError>
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
    if len > max_frame_bytes {
        // Two very different failures share this branch — see
        // `FrameError::OversizedSkipped` for the rule:
        //   * cap < len <= MAX_FRAME_BYTES: a well-framed peer sent one
        //     message too big for this channel. Consume and discard
        //     exactly `len` payload bytes so the stream stays
        //     frame-aligned, then report a skippable error.
        //   * len > MAX_FRAME_BYTES: no lazybox peer ever writes such a
        //     frame, so the length prefix itself is garbage — the
        //     stream is desynced/corrupt and the error is fatal.
        if len > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge(len));
        }
        discard_exact(r, len as usize).await?;
        return Err(FrameError::OversizedSkipped {
            len,
            max: max_frame_bytes,
        });
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let config = bincode::config::legacy().with_limit::<{ MAX_FRAME_BYTES as usize }>();
    let (message, consumed) = bincode::serde::decode_from_slice(&buf, config)?;
    if consumed != buf.len() {
        return Err(FrameError::TrailingBytes(buf.len() - consumed));
    }
    Ok(Some(message))
}

/// Read and throw away exactly `len` bytes — the payload of an
/// oversized-but-sane frame — so the next read starts at the next
/// frame's length prefix. EOF mid-discard is an I/O error: the peer
/// hung up mid-frame.
async fn discard_exact<R>(r: &mut R, len: usize) -> Result<(), FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut remaining = len;
    let mut scratch = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(scratch.len());
        let got = r.read(&mut scratch[..want]).await?;
        if got == 0 {
            return Err(FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF while discarding an oversized frame's payload",
            )));
        }
        remaining -= got;
    }
    Ok(())
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
    let peer = client_handshake_with_timeout(&mut rd, &mut wr, HANDSHAKE_TIMEOUT).await?;
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
    // Events are read into a BOUNDED channel: when the TUI falls behind
    // and `Client::rx` fills, `reader_loop_bounded` blocks on the send,
    // which stops draining the socket and back-pressures all the way to
    // the daemon's per-connection forwarder — which is where the drop +
    // resync actually happens. The client never drops, it only stalls.
    let (evt_tx, evt_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);

    // Writer path: drain Commands from the TUI, normalize any
    // oversized `Command::Write` into chunks that frame under the
    // daemon's `MAX_COMMAND_FRAME_BYTES` ingress cap, then frame them
    // onto the socket. The chunking sits at this single wire choke
    // point so EVERY client emission site (typed keys, bracketed
    // paste, prompt injection) is covered.
    let (wire_tx, wire_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
    tokio::spawn(chunk_commands_loop(cmd_rx, wire_tx));
    tokio::spawn(writer_loop_bounded(wr, wire_rx));
    // Reader task: parse framed Events from the socket, push to TUI.
    tokio::spawn(reader_loop_bounded(rd, evt_tx, MAX_FRAME_BYTES));

    Ok((Client::from_bounded_channels(cmd_tx, evt_rx), peer))
}

/// Initial delay before the first reconnect attempt, and the value the
/// backoff resets to after a connection has proved stable.
const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(250);
/// Ceiling on the reconnect backoff so a daemon that stays down (box
/// asleep, tunnel closed) is retried about every five seconds forever
/// rather than drifting to minutes-long gaps.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// A connection that served at least this long before dropping is
/// treated as healthy: the next drop starts damping from
/// [`RECONNECT_BACKOFF_START`] again. A connection that drops sooner is
/// flapping (accept-then-close, half-open tunnel) and keeps escalating.
const STABLE_CONNECTION: Duration = Duration::from_secs(5);

/// Double a backoff, capped at [`RECONNECT_BACKOFF_MAX`].
fn grow_backoff(current: Duration) -> Duration {
    (current * 2).min(RECONNECT_BACKOFF_MAX)
}

/// Next backoff after a live connection dropped, given how long it was
/// up. A connection that lasted [`STABLE_CONNECTION`] proved healthy, so
/// a genuine one-off drop reconnects fast; a connection that flapped
/// keeps escalating so a broken endpoint isn't hammered at a fixed rate.
fn next_backoff_after_drop(current: Duration, served: Duration) -> Duration {
    if served >= STABLE_CONNECTION {
        RECONNECT_BACKOFF_START
    } else {
        grow_backoff(current)
    }
}

/// Like [`connect`], but the client transparently re-dials the daemon
/// whenever the connection drops — a daemon restart, an SSH tunnel
/// reset, a laptop sleep, or a wifi change — instead of tearing the
/// event stream down and forcing a manual restart.
///
/// The first connection is established synchronously, so a genuinely
/// unreachable or wire-incompatible daemon still fails fast with the
/// same error [`connect`] returns. After that a supervisor task owns the
/// socket: on EOF or I/O error it reconnects with capped exponential
/// backoff and re-sends [`Command::Subscribe`], so the daemon replays a
/// fresh [`Event::Snapshot`] (workspaces plus terminal ring buffers) and
/// the client resyncs without the caller ever observing the drop. A wire
/// fingerprint mismatch or a terminal redial error is terminal: the
/// supervisor publishes the actionable failure reason and stops.
///
/// The returned [`Client`] tracks live [`ConnectionState`]: it flips to
/// [`ConnectionStatus::Reconnecting`] while re-dialing (so the UI can say
/// so instead of freezing), back to `Connected` with the reconnected
/// daemon's build on success, or to [`ConnectionStatus::Failed`] with
/// the terminal reason when retrying cannot help.
pub async fn connect_reconnecting(path: &Path) -> std::io::Result<(Client, PeerInfo)> {
    let path = path.to_path_buf();
    let redial: Redial = Arc::new(move || {
        let path = path.clone();
        Box::pin(async move {
            transport::connect(&path)
                .await
                .map_err(|error| RedialError::retryable(std::io::Error::other(error.to_string())))
        })
    });
    connect_reconnecting_with(redial).await
}

/// A source of fresh transport connections for [`connect_reconnecting_with`].
///
/// Each call dials a new connection and returns its raw
/// (pre-handshake) read/write halves. The supervisor runs the protocol
/// handshake on top, so a redialer only owns *reaching* the daemon:
/// a Unix socket, or — for the relay path — a fresh relay dial plus the
/// E2E handshake, whose ciphertext halves this yields.
pub type Redial = Arc<
    dyn Fn() -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<(transport::BoxRead, transport::BoxWrite), RedialError>,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

/// Whether a failed transport dial can recover without user action.
#[derive(Debug, thiserror::Error)]
pub enum RedialError {
    #[error("{0}")]
    Retryable(#[source] std::io::Error),
    #[error("{0}")]
    Terminal(#[source] std::io::Error),
}

impl RedialError {
    pub fn retryable(error: std::io::Error) -> Self {
        Self::Retryable(error)
    }

    pub fn terminal(error: std::io::Error) -> Self {
        Self::Terminal(error)
    }

    fn into_io(self) -> std::io::Error {
        match self {
            Self::Retryable(error) | Self::Terminal(error) => error,
        }
    }
}

/// Like [`connect_reconnecting`], but the caller supplies how to reach
/// the daemon. The Unix-socket path is `connect_reconnecting`; the relay
/// path passes a redialer that dials the relay and wraps the brokered
/// stream in the E2E channel, so a relay flap reconnects transparently
/// the same way a tunnel reset does.
pub async fn connect_reconnecting_with(redial: Redial) -> std::io::Result<(Client, PeerInfo)> {
    let (mut rd, mut wr) = redial().await.map_err(RedialError::into_io)?;
    let peer = client_handshake_with_timeout(&mut rd, &mut wr, HANDSHAKE_TIMEOUT).await?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
    let (evt_tx, evt_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
    // Same wire choke-point as `connect`: oversized `Command::Write`s are
    // split into frame-sized chunks before they reach the supervisor's
    // writer, so every reconnected connection inherits the chunking.
    let (wire_tx, wire_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
    let (status_tx, status_rx) = watch::channel(ConnectionState {
        status: ConnectionStatus::Connected,
        daemon_build: peer.build.clone(),
    });
    tokio::spawn(chunk_commands_loop(cmd_rx, wire_tx));
    tokio::spawn(reconnect_supervisor(
        redial,
        (rd, wr),
        wire_rx,
        evt_tx,
        status_tx,
    ));

    let client = Client::from_bounded_channels(cmd_tx, evt_rx).with_connection_status(status_rx);
    Ok((client, peer))
}

/// How a served connection ended.
enum ConnectionEnd {
    /// The socket died (EOF / write error); the supervisor should
    /// reconnect.
    Dropped,
    /// The caller dropped its `Client`; the supervisor should exit.
    CallerGone,
}

/// Outcome of a reconnect attempt loop.
enum ReconnectResult {
    Connected {
        conn: (transport::BoxRead, transport::BoxWrite),
        peer: PeerInfo,
    },
    /// The failure cannot recover without user action; publish its reason
    /// and stop reconnecting.
    Fatal(std::io::Error),
    /// The caller dropped its `Client` while we were retrying.
    CallerGone,
}

/// Supervises one logical client session across many physical socket
/// connections. Holds the persistent command receiver and event sender
/// so the caller's `Client` never sees a channel close during a
/// transient outage, and publishes [`ConnectionState`] transitions so
/// the UI can surface a reconnecting indicator.
async fn reconnect_supervisor(
    redial: Redial,
    initial: (transport::BoxRead, transport::BoxWrite),
    mut wire_rx: mpsc::Receiver<Command>,
    evt_tx: mpsc::Sender<Event>,
    status_tx: watch::Sender<ConnectionState>,
) {
    // The initial connection carries no injected Subscribe — the caller
    // (the TUI Model) sends its own on startup. Every *reconnection*
    // injects one so the daemon replays a resync Snapshot.
    let mut conn = Some(initial);
    let mut inject_subscribe = false;
    // Backoff persists across serve/reconnect cycles so a flapping
    // endpoint actually damps instead of pinning at the floor.
    let mut backoff = RECONNECT_BACKOFF_START;
    let mut daemon_build = status_tx.borrow().daemon_build.clone();
    loop {
        let (rd, wr) = match conn.take() {
            Some(pair) => pair,
            None => {
                status_tx.send_replace(ConnectionState {
                    status: ConnectionStatus::Reconnecting,
                    daemon_build: daemon_build.clone(),
                });
                match reconnect_with_backoff(&redial, &mut wire_rx, &mut backoff).await {
                    ReconnectResult::Connected { conn, peer } => {
                        daemon_build = peer.build.clone();
                        status_tx.send_replace(ConnectionState {
                            status: ConnectionStatus::Connected,
                            daemon_build: peer.build,
                        });
                        conn
                    }
                    ReconnectResult::Fatal(error) => {
                        tracing::warn!(%error, "reconnect: terminal connection failure; giving up");
                        status_tx.send_replace(ConnectionState {
                            status: ConnectionStatus::Failed {
                                message: error.to_string(),
                            },
                            daemon_build,
                        });
                        return;
                    }
                    ReconnectResult::CallerGone => return,
                }
            }
        };
        let served_at = tokio::time::Instant::now();
        match serve_one_connection(rd, wr, &mut wire_rx, &evt_tx, inject_subscribe).await {
            ConnectionEnd::Dropped => {
                inject_subscribe = true;
                backoff = next_backoff_after_drop(backoff, served_at.elapsed());
                continue;
            }
            ConnectionEnd::CallerGone => return,
        }
    }
}

/// Bridge the persistent channels to one physical connection until it
/// dies. The reader task feeding `evt_tx` is the liveness signal: its
/// completion (clean EOF or read error) means the connection dropped.
async fn serve_one_connection(
    rd: transport::BoxRead,
    mut wr: transport::BoxWrite,
    wire_rx: &mut mpsc::Receiver<Command>,
    evt_tx: &mpsc::Sender<Event>,
    inject_subscribe: bool,
) -> ConnectionEnd {
    if inject_subscribe {
        // Discard exactly the command backlog that queued during the
        // outage: it predates the resync Snapshot the injected Subscribe
        // pulls, so replaying it would fire stale keystrokes against
        // freshly-authoritative state. Draining a bounded snapshot of the
        // queue (not `while try_recv`) leaves any command that races in
        // *after* the reconnect untouched — FIFO guarantees the first
        // `backlog` items are the stale ones. Only reconnections drain:
        // the initial connection's backlog is the caller's own first
        // Subscribe, which must reach the daemon.
        let backlog = wire_rx.len();
        for _ in 0..backlog {
            if wire_rx.try_recv().is_err() {
                break;
            }
        }
        if write_frame(&mut wr, &Command::Subscribe).await.is_err() {
            return ConnectionEnd::Dropped;
        }
    }
    let mut reader = tokio::spawn(reader_loop_bounded(rd, evt_tx.clone(), MAX_FRAME_BYTES));
    loop {
        tokio::select! {
            biased;
            _ = &mut reader => {
                // The reader also ends when the caller drops `evt_rx`
                // (Model quit); distinguish that from a socket drop so we
                // don't pointlessly reconnect to a dead UI.
                return if evt_tx.is_closed() {
                    ConnectionEnd::CallerGone
                } else {
                    ConnectionEnd::Dropped
                };
            }
            cmd = wire_rx.recv() => match cmd {
                Some(cmd) => {
                    if write_frame(&mut wr, &cmd).await.is_err() {
                        reader.abort();
                        return ConnectionEnd::Dropped;
                    }
                }
                None => {
                    reader.abort();
                    return ConnectionEnd::CallerGone;
                }
            },
        }
    }
}

/// Retry the connect+handshake, waiting out `backoff` before each
/// attempt and growing it on every connect failure. `backoff` is owned
/// by the supervisor so it persists across drops (see
/// [`next_backoff_after_drop`]). While waiting we drain and discard any
/// commands the caller emits — input into a disconnected session is
/// dropped, and the resync Snapshot restores authoritative state on
/// reconnect — but a closed command channel means the caller is gone and
/// we stop.
async fn reconnect_with_backoff(
    redial: &Redial,
    wire_rx: &mut mpsc::Receiver<Command>,
    backoff: &mut Duration,
) -> ReconnectResult {
    loop {
        if !backoff_wait(wire_rx, *backoff).await {
            return ReconnectResult::CallerGone;
        }
        match try_reconnect(redial).await {
            Ok((conn, peer)) => return ReconnectResult::Connected { conn, peer },
            Err(ReconnectError::Fatal(error)) => return ReconnectResult::Fatal(error),
            Err(ReconnectError::Retryable) => {
                *backoff = grow_backoff(*backoff);
            }
        }
    }
}

/// Sleep for `backoff`, discarding any commands that arrive meanwhile.
/// Returns `true` once the delay elapses, or `false` if the caller's
/// command channel closes first (caller gone).
async fn backoff_wait(wire_rx: &mut mpsc::Receiver<Command>, backoff: Duration) -> bool {
    let sleep = tokio::time::sleep(backoff);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            biased;
            () = &mut sleep => return true,
            cmd = wire_rx.recv() => match cmd {
                Some(_) => continue,
                None => return false,
            },
        }
    }
}

enum ReconnectError {
    /// Transient: socket refused/absent or peer closed mid-handshake
    /// (a daemon that is restarting). Worth another attempt.
    Retryable,
    /// The failure cannot recover without user action, such as an
    /// incompatible wire protocol or denied relay entitlement.
    Fatal(std::io::Error),
}

async fn try_reconnect(
    redial: &Redial,
) -> Result<((transport::BoxRead, transport::BoxWrite), PeerInfo), ReconnectError> {
    let (mut rd, mut wr) = match redial().await {
        Ok(connection) => connection,
        Err(RedialError::Retryable(_)) => return Err(ReconnectError::Retryable),
        Err(RedialError::Terminal(error)) => return Err(ReconnectError::Fatal(error)),
    };
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, client_handshake(&mut rd, &mut wr)).await {
        Ok(Ok(peer)) => Ok(((rd, wr), peer)),
        Ok(Err(
            error @ (HandshakeError::FingerprintMismatch { .. } | HandshakeError::BadMagic(_)),
        )) => Err(ReconnectError::Fatal(std::io::Error::other(
            error.to_string(),
        ))),
        // Peer closed mid-handshake (restarting daemon) or the handshake
        // timed out — both worth retrying.
        Ok(Err(HandshakeError::Io(_))) | Err(_) => Err(ReconnectError::Retryable),
    }
}

/// Outbound command normalizer for the socket client: forwards every
/// command in order, expanding an oversized `Command::Write` into
/// multiple in-order chunked writes (see [`Command::write_chunked`]).
/// One sequential task feeding one ordered channel, so the relative
/// order of all commands — and of the chunks within a split write —
/// is exactly the order the TUI sent them; the PTY receives the same
/// byte stream it would have unchunked.
async fn chunk_commands_loop(mut rx: mpsc::Receiver<Command>, tx: mpsc::Sender<Command>) {
    while let Some(cmd) = rx.recv().await {
        for chunk in cmd.into_write_chunks() {
            if tx.send(chunk).await.is_err() {
                return; // writer gone — connection is closing
            }
        }
    }
}

/// Connection-side: wrap an accepted socket as a `Connection` handle.
///
/// Mirrors [`channel::pair`](crate::channel::pair): the serve loop
/// writes raw events to a bounded non-blocking `tx`; the server-spawned
/// forwarder bridges them into a **bounded** channel that the `writer_loop`
/// frames onto the socket. A full bounded channel back-pressures the
/// forwarder (which then drops + re-syncs `TerminalOutput`), so the
/// daemon's send side can't grow without bound when a remote client
/// stops reading.
pub fn serve<R, W>(rd: R, wr: W) -> Connection
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
    let (client_tx, client_rx) = mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
    let (raw_tx, forward) = event_forward_channel(client_tx);
    tokio::spawn(reader_loop_bounded(rd, cmd_tx, MAX_COMMAND_FRAME_BYTES));
    tokio::spawn(writer_loop_bounded(wr, client_rx));
    Connection::with_forward(raw_tx, cmd_rx, forward)
}

/// Upper bounds for one batched event write in
/// [`writer_loop_bounded`]. 64 messages / 256 KiB keeps a single
/// write+flush cheap and bounded while still collapsing an agent
/// output flood (dozens of queued `TerminalOutput` chunks) into one
/// syscall pair instead of one per event.
const MAX_BATCH_MESSAGES: usize = 64;
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Bounded-channel writer. Like `writer_loop` but drains a bounded
/// `Receiver` — used for the daemon's event stream so the socket
/// write rate back-pressures the forwarder.
///
/// Greedy-drain batching: writing (and above all FLUSHING) once per
/// event cost a syscall-flush per 8 KiB output chunk under an agent
/// flood. After awaiting the first message we `try_recv` whatever
/// else is already queued — up to [`MAX_BATCH_MESSAGES`] /
/// [`MAX_BATCH_BYTES`] — into one buffered write followed by a single
/// flush. Latency is unaffected: an empty queue flushes the lone
/// message immediately, exactly as before.
async fn writer_loop_bounded<W, M>(mut w: W, mut rx: mpsc::Receiver<M>)
where
    W: AsyncWrite + Unpin,
    M: Serialize,
{
    let mut buf: Vec<u8> = Vec::new();
    'conn: while let Some(first) = rx.recv().await {
        buf.clear();
        match encode_frame(&first) {
            Ok(framed) => buf.extend_from_slice(&framed),
            Err(e) => {
                tracing::warn!("writer: {e}");
                break;
            }
        }
        let mut batched = 1;
        while batched < MAX_BATCH_MESSAGES && buf.len() < MAX_BATCH_BYTES {
            let Ok(msg) = rx.try_recv() else { break };
            match encode_frame(&msg) {
                Ok(framed) => {
                    buf.extend_from_slice(&framed);
                    batched += 1;
                }
                Err(e) => {
                    // Same terminal outcome as the unbatched loop (an
                    // unencodable frame kills the connection), but the
                    // earlier messages in this batch were accepted
                    // before the bad one arrived — write them out
                    // first so the peer's stream stays a clean prefix.
                    tracing::warn!("writer: {e}");
                    if let Err(io) = write_all_flush(&mut w, &buf).await {
                        log_writer_io_error(&io);
                    }
                    break 'conn;
                }
            }
        }
        if let Err(e) = write_all_flush(&mut w, &buf).await {
            log_writer_io_error(&e);
            break;
        }
    }
}

/// One buffered write + one flush — the whole point of the batching.
async fn write_all_flush<W>(w: &mut W, buf: &[u8]) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    w.write_all(buf).await?;
    w.flush().await
}

/// A read/write failure that is simply the peer going away: the normal
/// end of a client's connection — it froze and was transparently
/// redialed, quit, slept, or its SSH tunnel reset. This is the write-side
/// twin of a clean read EOF (which [`read_frame_limited`] already maps to
/// `Ok(None)` and ends the reader silently). The connection is torn down
/// and, for a self-healing client, redialed with a resync either way, so
/// logging it at `WARN` only makes routine reconnects read as instability
/// — the repeated `writer: Broken pipe` stream of #1130. A genuine wire
/// fault stays `WARN`.
fn is_expected_disconnect(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::UnexpectedEof
    )
}

/// Log a writer I/O failure, quietly when it's a routine peer hangup and
/// at `WARN` when it's a genuine fault. See [`is_expected_disconnect`].
///
/// A routine hangup is `DEBUG`, but a *storm* of them still earns one
/// periodic `WARN`: downgrading every disconnect to `DEBUG` (#1130) would
/// otherwise make an hour of a client reconnect-looping over SSH — the
/// exact symptom #1130 was reported from — indistinguishable at default
/// log levels from a healthy idle daemon, which has no UI status indicator
/// to fall back on.
fn log_writer_io_error(error: &std::io::Error) {
    if is_expected_disconnect(error) {
        tracing::debug!("writer: peer disconnected ({error})");
        if let Some(count) = DISCONNECT_METER.record(std::time::Instant::now()) {
            tracing::warn!(
                count,
                window_secs = DISCONNECT_STORM_WINDOW.as_secs(),
                "socket peer(s) disconnecting repeatedly — a client is likely flapping or freezing"
            );
        }
    } else {
        tracing::warn!("writer: {error}");
    }
}

/// One reconnect is routine; several inside [`DISCONNECT_STORM_WINDOW`] is a
/// storm worth a single `WARN`. Tuned so an ordinary resume (one drop) never
/// reaches the threshold and stays silent.
const DISCONNECT_STORM_WINDOW: Duration = Duration::from_secs(60);
const DISCONNECT_STORM_THRESHOLD: u32 = 5;

/// Process-wide aggregate of routine peer-disconnects across every socket
/// connection, so a flapping client surfaces even though each individual
/// hangup is only `DEBUG`. See [`log_writer_io_error`].
static DISCONNECT_METER: DisconnectMeter =
    DisconnectMeter::new(DISCONNECT_STORM_WINDOW, DISCONNECT_STORM_THRESHOLD);

/// Counts routine disconnects in tumbling windows and reports a window's
/// total once, when it closes, if it met the threshold. Deliberately coarse
/// — a storm detector, not an exact ledger.
struct DisconnectMeter {
    window: Duration,
    threshold: u32,
    state: std::sync::Mutex<DisconnectWindow>,
}

struct DisconnectWindow {
    count: u32,
    window_start: Option<std::time::Instant>,
}

impl DisconnectMeter {
    const fn new(window: Duration, threshold: u32) -> Self {
        Self {
            window,
            threshold,
            state: std::sync::Mutex::new(DisconnectWindow {
                count: 0,
                window_start: None,
            }),
        }
    }

    /// Record one routine disconnect observed at `now`. Returns `Some(total)`
    /// with the just-closed window's count when that window met the
    /// threshold — the caller emits one summary `WARN` — otherwise `None`.
    /// A poisoned lock (impossible here — the guarded code cannot panic)
    /// degrades to `None` so metering never takes down a connection.
    fn record(&self, now: std::time::Instant) -> Option<u32> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        match state.window_start {
            Some(start) if now.saturating_duration_since(start) >= self.window => {
                let closed = state.count;
                state.window_start = Some(now);
                state.count = 1;
                (closed >= self.threshold).then_some(closed)
            }
            Some(_) => {
                state.count = state.count.saturating_add(1);
                None
            }
            None => {
                state.window_start = Some(now);
                state.count = 1;
                None
            }
        }
    }
}

/// Bounded-channel reader. Like `reader_loop` but `.send().await`s on
/// a bounded `Sender`, so a full channel stalls socket reads and
/// back-pressures the peer instead of buffering frames without bound.
async fn reader_loop_bounded<R, M>(mut r: R, tx: mpsc::Sender<M>, max_frame_bytes: u32)
where
    R: AsyncRead + Unpin,
    M: DeserializeOwned,
{
    loop {
        match read_frame_limited::<_, M>(&mut r, max_frame_bytes).await {
            Ok(Some(msg)) => {
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
            Ok(None) => break, // clean EOF
            // An oversized-but-well-framed message was already
            // discarded by `read_frame_limited` with the stream left
            // frame-aligned. Drop that ONE message and keep serving —
            // tearing the connection down here is what used to turn a
            // single >cap paste into a permanently dead `--connect`
            // session. Corrupt framing (a length prefix beyond
            // `MAX_FRAME_BYTES`) still lands in the fatal arm below.
            Err(e @ FrameError::OversizedSkipped { .. }) => {
                tracing::warn!("reader: {e} — dropping that message, connection stays up");
            }
            // A reset/abort mid-read is the same routine peer hangup a
            // clean EOF is (`Ok(None)` above) — end quietly. A genuine
            // wire fault (decode, framing, oversized prefix) stays `WARN`.
            Err(FrameError::Io(io)) if is_expected_disconnect(&io) => {
                tracing::debug!("reader: peer disconnected ({io})");
                break;
            }
            Err(e) => {
                tracing::warn!("reader: {e}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod batching_tests {
    use super::*;
    use crate::{Command, TerminalId};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    #[tokio::test]
    async fn client_handshake_times_out_when_peer_stays_silent() {
        let (client, _silent_peer) = tokio::io::duplex(64);
        let (mut rd, mut wr) = tokio::io::split(client);
        let error = client_handshake_with_timeout(&mut rd, &mut wr, Duration::from_millis(10))
            .await
            .expect_err("silent peer must not hold the client forever");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn send_command_delivers_one_command_to_a_socket_peer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("one-shot.sock");
        let listener = transport::Listener::bind(&path).await.expect("bind");
        let server = tokio::spawn(async move {
            let (mut rd, mut wr) = listener.accept().await.expect("accept");
            server_handshake(&mut rd, &mut wr)
                .await
                .expect("server handshake");
            read_frame::<_, Command>(&mut rd)
                .await
                .expect("frame")
                .expect("command")
        });
        let expected = cmd(674);

        let peer = send_command(&path, &expected).await.expect("send command");
        assert!(peer.build_matches());
        assert!(matches!(
            server.await.expect("server task"),
            Command::Write {
                terminal_id,
                bytes,
                ..
            }
                if terminal_id == TerminalId(674) && bytes == vec![b'x'; 8]
        ));
    }

    #[tokio::test]
    async fn one_shot_command_times_out_when_socket_peer_stays_silent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("silent.sock");
        let listener = transport::Listener::bind(&path).await.expect("bind");
        let server = tokio::spawn(async move {
            let _connection = listener.accept().await.expect("accept");
            std::future::pending::<()>().await;
        });

        let error = send_command_with_timeout(&path, &cmd(674), Duration::from_millis(10))
            .await
            .expect_err("silent peer must not hold a one-shot helper forever");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        server.abort();
    }

    /// AsyncWrite wrapper counting `poll_flush` completions, so the
    /// tests can assert the greedy drain really coalesces N queued
    /// messages into one flush instead of N.
    struct CountingWriter<W> {
        inner: W,
        flushes: Arc<AtomicUsize>,
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for CountingWriter<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            let res = Pin::new(&mut self.inner).poll_flush(cx);
            if matches!(res, Poll::Ready(Ok(()))) {
                self.flushes.fetch_add(1, Ordering::SeqCst);
            }
            res
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    fn cmd(i: u64) -> Command {
        Command::Write {
            terminal_id: TerminalId(i),
            bytes: vec![b'x'; 8],
            intent: crate::TerminalInputIntent::Compose,
        }
    }

    /// AsyncWrite that fails every write with a fixed error kind, so the
    /// writer loop's disconnect teardown path can be driven directly.
    struct FailingWriter {
        kind: std::io::ErrorKind,
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(self.kind, "peer gone")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn expected_disconnect_covers_peer_hangups_only() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(
                is_expected_disconnect(&std::io::Error::new(kind, "x")),
                "{kind:?} is a routine peer hangup"
            );
        }
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::Other,
        ] {
            assert!(
                !is_expected_disconnect(&std::io::Error::new(kind, "x")),
                "{kind:?} is a genuine fault, not a routine hangup"
            );
        }
    }

    /// A broken pipe on the socket write is the peer going away: the
    /// writer loop must end (tearing the connection down for redial),
    /// not spin. Regression for #1130's repeated `writer: Broken pipe`.
    #[tokio::test]
    async fn writer_loop_ends_when_the_peer_pipe_breaks() {
        let (tx, rx) = mpsc::channel::<Command>(4);
        tx.try_send(cmd(1)).expect("capacity");
        // Hold the sender open so the loop exits on the write failure —
        // the disconnect teardown under test — not on a closed channel.
        let _keepalive = tx;
        tokio::time::timeout(
            Duration::from_secs(5),
            writer_loop_bounded(
                FailingWriter {
                    kind: std::io::ErrorKind::BrokenPipe,
                },
                rx,
            ),
        )
        .await
        .expect("writer loop must terminate when the peer's pipe breaks");
    }

    /// Minimal `tracing` subscriber that records the level of every event on
    /// the current thread — enough to prove *which* level a log call chose
    /// without pulling in `tracing-subscriber`.
    #[derive(Clone, Default)]
    struct LevelCapture {
        levels: std::sync::Arc<std::sync::Mutex<Vec<tracing::Level>>>,
    }

    impl LevelCapture {
        fn recorded(&self) -> Vec<tracing::Level> {
            self.levels.lock().expect("capture lock").clone()
        }
    }

    impl tracing::Subscriber for LevelCapture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            self.levels
                .lock()
                .expect("capture lock")
                .push(*event.metadata().level());
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// The behavior #1130 changed: a routine peer hangup on the writer logs
    /// at `DEBUG`, a genuine fault at `WARN`. Reverting `log_writer_io_error`
    /// to an unconditional `warn!` must fail this test. Robust to the global
    /// storm meter — a summary `WARN` needs a window to elapse, and even if
    /// one somehow fired it would follow (not precede) the hangup's `DEBUG`.
    #[test]
    fn writer_routes_hangups_to_debug_and_faults_to_warn() {
        let capture = LevelCapture::default();
        let guard = tracing::subscriber::set_default(capture.clone());

        log_writer_io_error(&std::io::Error::new(std::io::ErrorKind::BrokenPipe, "x"));
        log_writer_io_error(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "x",
        ));
        drop(guard);

        let levels = capture.recorded();
        assert_eq!(
            levels.first(),
            Some(&tracing::Level::DEBUG),
            "a broken pipe must route to DEBUG, not WARN — got {levels:?}"
        );
        assert!(
            levels.contains(&tracing::Level::WARN),
            "a genuine fault must stay at WARN — got {levels:?}"
        );
    }

    #[test]
    fn disconnect_meter_stays_quiet_for_isolated_reconnects() {
        let meter = DisconnectMeter::new(Duration::from_secs(60), 5);
        let t0 = std::time::Instant::now();
        // One disconnect per window, each well under the threshold: a lone
        // reconnect never earns a storm WARN.
        assert_eq!(meter.record(t0), None);
        assert_eq!(meter.record(t0 + Duration::from_secs(120)), None);
        assert_eq!(meter.record(t0 + Duration::from_secs(240)), None);
    }

    #[test]
    fn disconnect_meter_reports_once_per_window_on_a_storm() {
        let meter = DisconnectMeter::new(Duration::from_secs(60), 5);
        let t0 = std::time::Instant::now();
        // Five disconnects inside the first window: the window is still open,
        // so nothing is reported yet.
        for i in 0..5 {
            assert_eq!(meter.record(t0 + Duration::from_millis(i * 10)), None);
        }
        // The first record past the window boundary closes it and reports
        // the five, then opens a fresh window at count 1.
        assert_eq!(meter.record(t0 + Duration::from_secs(61)), Some(5));
        // A lone follow-up window is back under threshold — quiet again.
        assert_eq!(meter.record(t0 + Duration::from_secs(122)), None);
    }

    #[test]
    fn disconnect_meter_needs_the_full_threshold_to_report() {
        let meter = DisconnectMeter::new(Duration::from_secs(60), 5);
        let t0 = std::time::Instant::now();
        // Four in a window is under the threshold: closing it reports nothing.
        for i in 0..4 {
            assert_eq!(meter.record(t0 + Duration::from_millis(i * 10)), None);
        }
        assert_eq!(meter.record(t0 + Duration::from_secs(61)), None);
    }

    #[tokio::test]
    async fn bounded_reader_stops_draining_when_command_admission_is_full() {
        let (mut peer, daemon) = tokio::io::duplex(4096);
        let (tx, mut rx) = mpsc::channel::<Command>(2);
        let reader = tokio::spawn(reader_loop_bounded(daemon, tx, MAX_COMMAND_FRAME_BYTES));

        for index in 0..3 {
            write_frame(&mut peer, &cmd(index))
                .await
                .expect("wire write");
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while rx.len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader fills bounded admission");
        assert_eq!(rx.len(), 2, "the third decoded command waits at send");

        assert!(matches!(rx.recv().await, Some(Command::Write { .. })));
        let third = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while rx.len() < 2 {
                tokio::task::yield_now().await;
            }
            rx.recv().await
        })
        .await
        .expect("capacity release resumes socket read");
        assert!(matches!(third, Some(Command::Write { .. })));

        drop(peer);
        drop(rx);
        reader.await.expect("reader task");
    }

    /// Regression for the killed `--connect` session: an
    /// oversized-but-well-framed command frame (above the command cap,
    /// within `MAX_FRAME_BYTES`) must be discarded WITHOUT tearing the
    /// connection down — the valid command behind it still arrives.
    #[tokio::test]
    async fn command_reader_skips_oversized_frame_and_keeps_serving() {
        let (mut peer, daemon) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel::<Command>(1);
        let reader = tokio::spawn(reader_loop_bounded(daemon, tx, MAX_COMMAND_FRAME_BYTES));

        // Hand-frame an oversized command: sane length prefix, then
        // exactly that many payload bytes (contents irrelevant — the
        // reader must discard them unparsed).
        let oversized_len = MAX_COMMAND_FRAME_BYTES + 1;
        peer.write_all(&oversized_len.to_be_bytes())
            .await
            .expect("length prefix");
        // Write in slices so the small duplex buffer never deadlocks
        // against the concurrently-draining reader.
        let payload = vec![0u8; oversized_len as usize];
        peer.write_all(&payload).await.expect("oversized payload");
        // A valid command right behind it proves the stream stayed
        // frame-aligned across the discard.
        write_frame(&mut peer, &cmd(7)).await.expect("valid frame");
        peer.flush().await.expect("flush");

        let survivor = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("reader must survive the oversized frame");
        assert!(
            matches!(
                survivor,
                Some(Command::Write { terminal_id, .. }) if terminal_id == TerminalId(7)
            ),
            "the command after the skipped frame must arrive: {survivor:?}"
        );

        drop(peer);
        drop(rx);
        reader.await.expect("reader task exits cleanly on EOF");
    }

    /// A length prefix beyond `MAX_FRAME_BYTES` is not a big message —
    /// it's a desynced/corrupt stream (no lazybox peer ever writes such
    /// a frame). That stays fatal: the reader terminates and the
    /// command channel closes.
    #[tokio::test]
    async fn command_reader_dies_on_wild_length_prefix() {
        let (mut peer, daemon) = tokio::io::duplex(64);
        let (tx, mut rx) = mpsc::channel::<Command>(1);
        let reader = tokio::spawn(reader_loop_bounded(daemon, tx, MAX_COMMAND_FRAME_BYTES));
        peer.write_all(&(MAX_FRAME_BYTES + 1).to_be_bytes())
            .await
            .expect("length prefix");
        peer.flush().await.expect("flush");

        tokio::time::timeout(std::time::Duration::from_secs(1), reader)
            .await
            .expect("wild prefix rejected promptly")
            .expect("reader task");
        assert!(rx.recv().await.is_none());
    }

    /// End-to-end over the client's outbound path (chunking adapter +
    /// batched writer): one `Command::Write` far above the command cap
    /// must arrive as multiple frames, EACH decodable under the
    /// daemon's `MAX_COMMAND_FRAME_BYTES` reader cap, concatenating
    /// byte-identically and in order to the original payload.
    #[tokio::test]
    async fn client_chunking_keeps_every_command_frame_under_the_daemon_cap() {
        // A recognizable, non-uniform pattern so a reordered or
        // truncated chunk cannot concatenate back to the original.
        let original: Vec<u8> = (0..(MAX_COMMAND_FRAME_BYTES as usize * 2 + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        let (tx, rx) = mpsc::channel::<Command>(4);
        let (wire_tx, wire_rx) = mpsc::channel::<Command>(4);
        tx.try_send(Command::Write {
            terminal_id: TerminalId(3),
            bytes: original.clone(),
            intent: crate::TerminalInputIntent::Submit,
        })
        .expect("capacity");
        drop(tx);

        let (wr, mut rd) = tokio::io::duplex(8 * 1024 * 1024);
        let chunker = tokio::spawn(chunk_commands_loop(rx, wire_tx));
        writer_loop_bounded(wr, wire_rx).await;
        chunker.await.expect("chunker task");

        let mut frames = 0usize;
        let mut reassembled = Vec::new();
        // Read with the DAEMON's command cap: any frame the old
        // unchunked path would have produced fails right here.
        while let Some(back) = read_frame_limited::<_, Command>(&mut rd, MAX_COMMAND_FRAME_BYTES)
            .await
            .expect("every chunked frame fits under the command cap")
        {
            match back {
                Command::Write {
                    terminal_id, bytes, ..
                } => {
                    assert_eq!(terminal_id, TerminalId(3));
                    assert!(bytes.len() <= crate::MAX_WRITE_CHUNK_BYTES);
                    frames += 1;
                    reassembled.extend_from_slice(&bytes);
                }
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        assert!(frames > 1, "an over-cap write must split ({frames} frames)");
        assert_eq!(
            reassembled, original,
            "chunks must concatenate byte-identically, in order"
        );
    }

    /// Pre-queued messages must arrive intact and in order through the
    /// batched writer, and a fully-queued burst below the caps costs
    /// exactly ONE flush — the regression this batching exists for
    /// (one syscall-flush per event under an agent flood).
    #[tokio::test]
    async fn batched_writer_coalesces_queued_messages_into_one_flush() {
        let (tx, rx) = mpsc::channel::<Command>(EVENT_CHANNEL_CAPACITY);
        for i in 0..10 {
            tx.try_send(cmd(i)).expect("channel has capacity");
        }
        drop(tx); // loop drains the queue, then exits cleanly

        let (wr, mut rd) = duplex_pair();
        let flushes = Arc::new(AtomicUsize::new(0));
        writer_loop_bounded(
            CountingWriter {
                inner: wr,
                flushes: flushes.clone(),
            },
            rx,
        )
        .await;

        for i in 0..10 {
            let back: Command = read_frame(&mut rd)
                .await
                .expect("read ok")
                .expect("frame present");
            match back {
                Command::Write {
                    terminal_id, bytes, ..
                } => {
                    assert_eq!(terminal_id, TerminalId(i), "order must be preserved");
                    assert_eq!(bytes.len(), 8);
                }
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        assert_eq!(
            flushes.load(Ordering::SeqCst),
            1,
            "10 queued messages under the caps must cost exactly one flush"
        );
    }

    /// The message-count cap bounds each batch: 100 queued messages
    /// split into ceil(100/64) = 2 write+flush rounds, all delivered.
    #[tokio::test]
    async fn batched_writer_respects_message_cap() {
        let (tx, rx) = mpsc::channel::<Command>(EVENT_CHANNEL_CAPACITY);
        let total = (MAX_BATCH_MESSAGES + 36) as u64;
        for i in 0..total {
            tx.try_send(cmd(i)).expect("channel has capacity");
        }
        drop(tx);

        let (wr, mut rd) = duplex_pair();
        let flushes = Arc::new(AtomicUsize::new(0));
        writer_loop_bounded(
            CountingWriter {
                inner: wr,
                flushes: flushes.clone(),
            },
            rx,
        )
        .await;

        for i in 0..total {
            let back: Command = read_frame(&mut rd)
                .await
                .expect("read ok")
                .expect("frame present");
            assert!(
                matches!(back, Command::Write { terminal_id, .. } if terminal_id == TerminalId(i)),
                "message {i} must arrive in order"
            );
        }
        assert_eq!(
            flushes.load(Ordering::SeqCst),
            2,
            "100 pre-queued messages = two capped batches = two flushes"
        );
    }

    /// The byte cap closes a batch early so one write never buffers an
    /// unbounded blob: messages bigger than the cap still flow, one
    /// flush each.
    #[tokio::test]
    async fn batched_writer_respects_byte_cap() {
        let big = Command::Write {
            terminal_id: TerminalId(1),
            bytes: vec![b'y'; MAX_BATCH_BYTES],
            intent: crate::TerminalInputIntent::Compose,
        };
        let (tx, rx) = mpsc::channel::<Command>(8);
        tx.try_send(big.clone()).expect("capacity");
        tx.try_send(big).expect("capacity");
        drop(tx);

        // Big frames: use a large duplex so the writer never stalls.
        let (a, b) = tokio::io::duplex(4 * MAX_BATCH_BYTES);
        let flushes = Arc::new(AtomicUsize::new(0));
        let reader = tokio::spawn(async move {
            let mut rd = b;
            let mut seen = 0;
            while let Ok(Some(Command::Write { bytes, .. })) =
                read_frame::<_, Command>(&mut rd).await
            {
                assert_eq!(bytes.len(), MAX_BATCH_BYTES);
                seen += 1;
            }
            seen
        });
        writer_loop_bounded(
            CountingWriter {
                inner: a,
                flushes: flushes.clone(),
            },
            rx,
        )
        .await;
        assert_eq!(reader.await.expect("reader task"), 2);
        assert_eq!(
            flushes.load(Ordering::SeqCst),
            2,
            "each over-cap frame closes its own batch"
        );
    }

    fn duplex_pair() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
        // Room for every test batch without back-pressure, so the
        // writer loop can run to completion before the test reads.
        tokio::io::duplex(1024 * 1024)
    }

    /// The BYO-remote acceptance path: a socket drop must not surface to
    /// the caller. The reconnecting client re-dials on its own, re-sends
    /// `Subscribe` so the daemon can replay a resync `Snapshot`, and the
    /// post-drop event still arrives on the same `rx` — no manual restart.
    #[tokio::test]
    async fn reconnecting_client_resyncs_after_the_socket_drops() {
        use lazybox_core::ProjectKey;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("reconnect.sock");
        let listener = transport::Listener::bind(&path).await.expect("bind");

        let (subscribed_tx, subscribed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            // Connection 1: hand off one event, then drop the socket to
            // stand in for a sleep / wifi-change / tunnel reset.
            let (mut rd, mut wr) = listener.accept().await.expect("accept 1");
            server_handshake(&mut rd, &mut wr)
                .await
                .expect("handshake 1");
            write_frame(&mut wr, &Event::ProjectRemoved(ProjectKey("before".into())))
                .await
                .expect("event 1");
            drop((rd, wr));

            // Connection 2: the client re-dials unprompted. Its first
            // frame must be the injected Subscribe (the resync trigger),
            // after which a fresh event flows.
            let (mut rd, mut wr) = listener.accept().await.expect("accept 2");
            server_handshake(&mut rd, &mut wr)
                .await
                .expect("handshake 2");
            let first = read_frame::<_, Command>(&mut rd)
                .await
                .expect("frame 2")
                .expect("command 2");
            let _ = subscribed_tx.send(matches!(first, Command::Subscribe));
            write_frame(&mut wr, &Event::ProjectRemoved(ProjectKey("after".into())))
                .await
                .expect("event 2");
            std::future::pending::<()>().await;
        });

        let (mut client, _peer) = connect_reconnecting(&path).await.expect("connect");

        assert_eq!(
            client.connection_status(),
            ConnectionStatus::Connected,
            "a freshly connected client is Connected"
        );
        let before = tokio::time::timeout(Duration::from_secs(5), client.rx.recv())
            .await
            .expect("event before drop")
            .expect("channel stays open");
        assert!(matches!(before, Event::ProjectRemoved(k) if k.0 == "before"));

        // While re-dialing (during the backoff before conn 2), the client
        // advertises Reconnecting so the UI can surface it instead of
        // freezing silently.
        assert!(
            poll_status_becomes(
                &client,
                ConnectionStatus::Reconnecting,
                Duration::from_secs(2)
            )
            .await,
            "client must report Reconnecting while the socket is down"
        );

        // The caller never observed a channel close: the next event
        // arrives on the same `rx` after a transparent reconnect.
        let after = tokio::time::timeout(Duration::from_secs(5), client.rx.recv())
            .await
            .expect("event after reconnect")
            .expect("channel stays open");
        assert!(matches!(after, Event::ProjectRemoved(k) if k.0 == "after"));
        assert_eq!(
            client.connection_status(),
            ConnectionStatus::Connected,
            "status returns to Connected once the socket is back"
        );

        assert!(
            subscribed_rx.await.expect("subscribe signal"),
            "reconnect must re-send Subscribe to trigger a resync Snapshot",
        );

        server.abort();
    }

    /// Commands typed into a disconnected session must not replay against
    /// the freshly-resynced daemon: the stale backlog is dropped, only a
    /// post-reconnect command reaches the socket after the Subscribe.
    #[tokio::test]
    async fn reconnecting_client_discards_commands_queued_while_disconnected() {
        use lazybox_core::ProjectKey;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("discard.sock");
        let listener = transport::Listener::bind(&path).await.expect("bind");

        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut rd, mut wr) = listener.accept().await.expect("accept 1");
            server_handshake(&mut rd, &mut wr)
                .await
                .expect("handshake 1");
            write_frame(&mut wr, &Event::ProjectRemoved(ProjectKey("before".into())))
                .await
                .expect("event 1");
            drop((rd, wr));

            let (mut rd, mut wr) = listener.accept().await.expect("accept 2");
            server_handshake(&mut rd, &mut wr)
                .await
                .expect("handshake 2");
            // The injected resync Subscribe leads.
            let first = read_frame::<_, Command>(&mut rd)
                .await
                .expect("frame")
                .expect("command");
            assert!(matches!(first, Command::Subscribe), "expected Subscribe");
            // Signal the client it's connected again, then capture the
            // very next command it forwards.
            write_frame(&mut wr, &Event::ProjectRemoved(ProjectKey("after".into())))
                .await
                .expect("event 2");
            let next = read_frame::<_, Command>(&mut rd)
                .await
                .expect("frame")
                .expect("command");
            let _ = seen_tx.send(next);
            std::future::pending::<()>().await;
        });

        let (mut client, _peer) = connect_reconnecting(&path).await.expect("connect");
        let before = tokio::time::timeout(Duration::from_secs(5), client.rx.recv())
            .await
            .expect("event before drop")
            .expect("channel stays open");
        assert!(matches!(before, Event::ProjectRemoved(k) if k.0 == "before"));

        // Type into the dead session — this command must be discarded.
        assert!(
            poll_status_becomes(
                &client,
                ConnectionStatus::Reconnecting,
                Duration::from_secs(2)
            )
            .await,
            "client must be Reconnecting before we queue the stale command"
        );
        client
            .send(cmd(1))
            .expect("queue a stale command while disconnected");

        // Once reconnected, a fresh command is the only one that should
        // reach the daemon after the Subscribe.
        let after = tokio::time::timeout(Duration::from_secs(5), client.rx.recv())
            .await
            .expect("event after reconnect")
            .expect("channel stays open");
        assert!(matches!(after, Event::ProjectRemoved(k) if k.0 == "after"));
        client.send(cmd(2)).expect("fresh post-reconnect command");

        let seen = tokio::time::timeout(Duration::from_secs(5), seen_rx)
            .await
            .expect("server saw a post-Subscribe command")
            .expect("oneshot");
        assert!(
            matches!(seen, Command::Write { terminal_id, .. } if terminal_id == TerminalId(2)),
            "the first post-Subscribe command must be the fresh one, not the \
             stale queued command: {seen:?}"
        );

        server.abort();
    }

    /// Reconnecting to a wire-incompatible daemon (fingerprint mismatch —
    /// the box came back from a different build) is terminal: the
    /// supervisor gives up and closes the event stream so the UI shows
    /// its disconnect banner, rather than retrying a hopeless handshake.
    #[tokio::test]
    async fn reconnecting_client_gives_up_on_a_wire_incompatible_daemon() {
        use lazybox_core::ProjectKey;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("mismatch.sock");
        let listener = transport::Listener::bind(&path).await.expect("bind");

        let server = tokio::spawn(async move {
            let (mut rd, mut wr) = listener.accept().await.expect("accept 1");
            server_handshake(&mut rd, &mut wr)
                .await
                .expect("handshake 1");
            write_frame(&mut wr, &Event::ProjectRemoved(ProjectKey("before".into())))
                .await
                .expect("event 1");
            drop((rd, wr));

            // Connection 2 answers the handshake with a valid magic but a
            // deliberately wrong fingerprint — an incompatible build.
            let (mut rd, mut wr) = listener.accept().await.expect("accept 2");
            let mut preamble = [0u8; 8];
            preamble[..4].copy_from_slice(&PROTOCOL_MAGIC);
            preamble[4..].copy_from_slice(&PROTOCOL_FINGERPRINT.wrapping_add(1).to_le_bytes());
            wr.write_all(&preamble).await.expect("bad preamble");
            wr.flush().await.expect("flush");
            // Read the client's preamble so its write doesn't error first.
            let mut _client_preamble = [0u8; 8];
            let _ = rd.read_exact(&mut _client_preamble).await;
            std::future::pending::<()>().await;
        });

        let (mut client, _peer) = connect_reconnecting(&path).await.expect("connect");
        let before = tokio::time::timeout(Duration::from_secs(5), client.rx.recv())
            .await
            .expect("event before drop")
            .expect("channel stays open");
        assert!(matches!(before, Event::ProjectRemoved(k) if k.0 == "before"));

        // The supervisor gives up on the mismatch and drops its event
        // sender, closing the stream — `recv` returns None.
        let closed = tokio::time::timeout(Duration::from_secs(5), client.rx.recv())
            .await
            .expect("stream closes within the timeout");
        assert!(
            closed.is_none(),
            "an incompatible daemon must close the event stream, not hang: {closed:?}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn reconnecting_client_surfaces_a_terminal_redial_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("terminal-redial.sock");
        let listener = transport::Listener::bind(&path).await.expect("bind");
        let server = tokio::spawn(async move {
            let (mut rd, mut wr) = listener.accept().await.expect("accept");
            server_handshake(&mut rd, &mut wr)
                .await
                .expect("initial handshake");
            drop((rd, wr));
        });

        let attempts = Arc::new(AtomicUsize::new(0));
        let redial_path = path.clone();
        let redial: Redial = Arc::new(move || {
            let attempts = Arc::clone(&attempts);
            let path = redial_path.clone();
            Box::pin(async move {
                if attempts.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Err(RedialError::terminal(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "subscription required — https://lazybox.ai/pricing",
                    )));
                }
                transport::connect(&path).await.map_err(|error| {
                    RedialError::retryable(std::io::Error::other(error.to_string()))
                })
            })
        });
        let (client, _peer) = connect_reconnecting_with(redial)
            .await
            .expect("initial connection");
        server.await.expect("server task");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match client.connection_status() {
                ConnectionStatus::Failed { message } => {
                    assert_eq!(
                        message,
                        "subscription required — https://lazybox.ai/pricing"
                    );
                    break;
                }
                _ if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                status => panic!("terminal redial error was not surfaced: {status:?}"),
            }
        }
    }

    /// When the caller drops its `Client`, the supervisor tears the
    /// socket down instead of leaking the task: the daemon sees EOF.
    #[tokio::test]
    async fn reconnecting_client_supervisor_exits_when_the_caller_drops() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("caller-gone.sock");
        let listener = transport::Listener::bind(&path).await.expect("bind");

        let server = tokio::spawn(async move {
            let (mut rd, mut wr) = listener.accept().await.expect("accept");
            server_handshake(&mut rd, &mut wr).await.expect("handshake");
            // Read to EOF; `None` means the client tore the connection
            // down when its `Client` dropped.
            loop {
                match read_frame::<_, Command>(&mut rd).await {
                    Ok(Some(_)) => continue,
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });

        let (client, _peer) = connect_reconnecting(&path).await.expect("connect");
        drop(client);

        // Dropping the client closes its command channel, which the
        // supervisor observes and uses to tear the socket down — the
        // server's read completes rather than hanging forever.
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server read completes after the client drops")
            .expect("server task");
    }

    /// Poll `connection_status` until it reaches `want` or the budget
    /// elapses. Cheap busy-wait with yields — the reconnect backoff floor
    /// guarantees a Reconnecting window long enough to observe.
    async fn poll_status_becomes(
        client: &Client,
        want: ConnectionStatus,
        budget: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        while tokio::time::Instant::now() < deadline {
            if client.connection_status() == want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        client.connection_status() == want
    }

    #[test]
    fn backoff_grows_geometrically_and_caps() {
        assert_eq!(
            grow_backoff(RECONNECT_BACKOFF_START),
            RECONNECT_BACKOFF_START * 2
        );
        // Never exceeds the ceiling, and is idempotent once there.
        assert_eq!(grow_backoff(RECONNECT_BACKOFF_MAX), RECONNECT_BACKOFF_MAX);
        assert_eq!(
            grow_backoff(RECONNECT_BACKOFF_MAX / 2 + Duration::from_secs(1)),
            RECONNECT_BACKOFF_MAX
        );
    }

    #[test]
    fn backoff_resets_only_after_a_stable_connection() {
        // A connection that lasted long enough resets to the floor so a
        // genuine one-off drop reconnects fast.
        assert_eq!(
            next_backoff_after_drop(RECONNECT_BACKOFF_MAX, STABLE_CONNECTION),
            RECONNECT_BACKOFF_START
        );
        // A flapping connection (dropped almost immediately) keeps
        // escalating instead of pinning at the floor — the #2 fix.
        assert_eq!(
            next_backoff_after_drop(RECONNECT_BACKOFF_START, Duration::from_millis(10)),
            grow_backoff(RECONNECT_BACKOFF_START)
        );
        assert_eq!(
            next_backoff_after_drop(RECONNECT_BACKOFF_MAX, Duration::from_millis(10)),
            RECONNECT_BACKOFF_MAX
        );
    }
}
