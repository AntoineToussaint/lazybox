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
    BUILD_VERSION, COMMAND_CHANNEL_CAPACITY, Client, Command, Connection, EVENT_CHANNEL_CAPACITY,
    Event, MAX_COMMAND_FRAME_BYTES, MAX_FRAME_BYTES, PROTOCOL_FINGERPRINT, PROTOCOL_MAGIC,
    event_forward_channel,
};
use serde::{Serialize, de::DeserializeOwned};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

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
                        tracing::warn!("writer: {io}");
                    }
                    break 'conn;
                }
            }
        }
        if let Err(e) = write_all_flush(&mut w, &buf).await {
            tracing::warn!("writer: {e}");
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
}
