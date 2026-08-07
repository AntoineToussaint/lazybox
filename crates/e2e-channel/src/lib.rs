//! End-to-end encrypted channel for lazybox's client/daemon transport.
//!
//! This crate sits *under* the existing length-prefixed bincode framing
//! (`lazybox-ipc::socket`): once [`initiator_handshake`] /
//! [`responder_handshake`] complete, the returned [`EncryptedStream`] is a
//! drop-in `AsyncRead`/`AsyncWrite` pair, so the `Command`/`Event` framing
//! and the protocol-fingerprint handshake run inside it unchanged. The
//! transport underneath is anything that reads and writes bytes — a Unix
//! socket, an SSH-forwarded socket, or (the point of the relay work under
//! epic #885) a relay that only ever sees ciphertext.
//!
//! # Handshake
//!
//! The channel uses the Noise `IK` pattern over X25519 + ChaChaPoly +
//! BLAKE2s. `IK` fits the pairing model exactly:
//!
//! - The **initiator** (client) pre-knows the **responder's** (box's)
//!   static public key — it comes from the pairing QR. That key *pins* the
//!   box: a relay in the middle cannot substitute its own key without the
//!   handshake failing, so it can forward but never MITM.
//! - The **responder** learns the initiator's static public key during the
//!   handshake and [`responder_handshake`] returns it, so a later layer can
//!   bind it to a per-device credential (#892).
//!
//! Both directions are mutually authenticated: each side proves possession
//! of its static private key, so neither a wrong pin nor a relay-injected
//! key completes the handshake.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use snow::params::NoiseParams;
use snow::{Builder, TransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/// Noise handshake + cipher choice. X25519 for the DH (the "identity
/// key" the box pins itself with), ChaChaPoly for the AEAD (always
/// available, no AES hardware assumption), BLAKE2s for the hash.
const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// Ceiling on a single Noise message on the wire; `snow` refuses to
/// encrypt or decrypt anything larger.
const NOISE_MAX_MSG_LEN: usize = 65535;
/// AEAD tag `snow` appends to every transport message.
const NOISE_TAG_LEN: usize = 16;
/// Largest plaintext chunk that still frames under [`NOISE_MAX_MSG_LEN`]
/// once the tag is added. Longer writes are split across several
/// transport messages, transparently reassembled by the reader.
const MAX_PAYLOAD_LEN: usize = NOISE_MAX_MSG_LEN - NOISE_TAG_LEN;
/// Scratch buffer for the fixed, tiny handshake messages (an `IK`
/// message carries an ephemeral key, an encrypted static key, and
/// tags — under 100 bytes).
const HANDSHAKE_BUF_LEN: usize = 1024;

/// Errors from establishing or driving an [`EncryptedStream`].
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// The Noise layer rejected a message — a failed handshake (wrong
    /// pinned key, relay-injected key, corrupt stream) or a bad
    /// transport message.
    #[error("noise protocol error: {0}")]
    Noise(#[from] snow::Error),
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    /// The peer closed the connection before the handshake completed.
    #[error("stream closed before the handshake completed")]
    UnexpectedEof,
    /// A handshake message exceeded the u16 length prefix — impossible
    /// from a conforming Noise peer, so the stream isn't one.
    #[error("handshake message too large ({0} bytes)")]
    MessageTooLarge(usize),
    /// The `IK` handshake completed but the peer presented no static
    /// key. A conforming responder always receives one; its absence
    /// means a non-conforming peer.
    #[error("peer completed the handshake without a static identity key")]
    MissingPeerIdentity,
}

/// A box or device long-term X25519 identity keypair.
///
/// The box generates one, persists it, and publishes its
/// [`public_key`](Identity::public_key) in the pairing QR so clients can
/// pin it. A client generates its own so the box can authenticate the
/// device.
pub struct Identity {
    keypair: snow::Keypair,
}

impl Identity {
    /// Generate a fresh random identity.
    pub fn generate() -> Result<Self, ChannelError> {
        let params: NoiseParams = NOISE_PARAMS.parse()?;
        let keypair = Builder::new(params).generate_keypair()?;
        Ok(Self { keypair })
    }

    /// This identity's public key — the value a peer pins.
    pub fn public_key(&self) -> PublicKey {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&self.keypair.public);
        PublicKey(bytes)
    }
}

/// A 32-byte X25519 public key: a box or device identity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey(")?;
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        write!(f, ")")
    }
}

/// Client side of the handshake: authenticate against a box whose
/// identity is pinned by `remote`, presenting `local` as this device's
/// identity. On success the returned stream carries application bytes
/// encrypted end-to-end.
pub async fn initiator_handshake<S>(
    mut io: S,
    local: &Identity,
    remote: &PublicKey,
) -> Result<EncryptedStream<S>, ChannelError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let params: NoiseParams = NOISE_PARAMS.parse()?;
    let mut hs = Builder::new(params)
        .local_private_key(&local.keypair.private)?
        .remote_public_key(&remote.0)?
        .build_initiator()?;

    let mut buf = [0u8; HANDSHAKE_BUF_LEN];
    // -> e, es, s, ss
    let len = hs.write_message(&[], &mut buf)?;
    write_handshake_msg(&mut io, &buf[..len]).await?;
    // <- e, ee, se
    let reply = read_handshake_msg(&mut io).await?;
    hs.read_message(&reply, &mut buf)?;

    let transport = hs.into_transport_mode()?;
    Ok(EncryptedStream::new(io, transport))
}

/// Box side of the handshake: authenticate with this box's `local`
/// identity and learn the connecting device's public key. Returns the
/// encrypted stream and the peer's identity, which a later layer binds
/// to a per-device credential.
pub async fn responder_handshake<S>(
    mut io: S,
    local: &Identity,
) -> Result<(EncryptedStream<S>, PublicKey), ChannelError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let params: NoiseParams = NOISE_PARAMS.parse()?;
    let mut hs = Builder::new(params)
        .local_private_key(&local.keypair.private)?
        .build_responder()?;

    let mut buf = [0u8; HANDSHAKE_BUF_LEN];
    // -> e, es, s, ss
    let msg = read_handshake_msg(&mut io).await?;
    hs.read_message(&msg, &mut buf)?;
    // <- e, ee, se
    let len = hs.write_message(&[], &mut buf)?;
    write_handshake_msg(&mut io, &buf[..len]).await?;

    let peer = hs
        .get_remote_static()
        .ok_or(ChannelError::MissingPeerIdentity)?;
    let mut peer_key = [0u8; 32];
    peer_key.copy_from_slice(peer);

    let transport = hs.into_transport_mode()?;
    Ok((EncryptedStream::new(io, transport), PublicKey(peer_key)))
}

/// Frame one handshake message as `[u16 BE length][payload]`.
async fn write_handshake_msg<W>(w: &mut W, msg: &[u8]) -> Result<(), ChannelError>
where
    W: AsyncWrite + Unpin,
{
    let len = u16::try_from(msg.len()).map_err(|_| ChannelError::MessageTooLarge(msg.len()))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(msg).await?;
    w.flush().await?;
    Ok(())
}

/// Read one framed handshake message; a mid-message EOF is a peer that
/// hung up before finishing the handshake.
async fn read_handshake_msg<R>(r: &mut R) -> Result<Vec<u8>, ChannelError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 2];
    read_exact_or_eof(r, &mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    read_exact_or_eof(r, &mut buf).await?;
    Ok(buf)
}

async fn read_exact_or_eof<R>(r: &mut R, buf: &mut [u8]) -> Result<(), ChannelError>
where
    R: AsyncRead + Unpin,
{
    match r.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(ChannelError::UnexpectedEof),
        Err(e) => Err(e.into()),
    }
}

/// An established end-to-end encrypted byte stream.
///
/// Reads and writes look like any `AsyncRead`/`AsyncWrite`. Underneath,
/// each write is split into Noise transport messages (one AEAD tag's
/// worth under 64 KiB of plaintext each), encrypted, and framed as
/// `[u16 BE length][ciphertext]`; the read side reverses that,
/// buffering decrypted plaintext so a caller can read it in any chunking
/// it likes. Callers that need the read and write halves separately
/// (as the socket transport does) can wrap with [`tokio::io::split`].
pub struct EncryptedStream<S> {
    io: S,
    transport: TransportState,
    rx: RxFrame,
    /// Decrypted plaintext from the last frame, not yet handed to a
    /// reader.
    rx_plain: Vec<u8>,
    rx_pos: usize,
    /// One framed ciphertext message staged for the underlying writer.
    tx_buf: Vec<u8>,
    tx_pos: usize,
}

/// Partial read of one inbound ciphertext frame across many polls.
#[derive(Default)]
struct RxFrame {
    len_buf: [u8; 2],
    len_filled: usize,
    have_len: bool,
    payload: Vec<u8>,
    payload_filled: usize,
}

impl<S> EncryptedStream<S> {
    fn new(io: S, transport: TransportState) -> Self {
        Self {
            io,
            transport,
            rx: RxFrame::default(),
            rx_plain: Vec::new(),
            rx_pos: 0,
            tx_buf: Vec::new(),
            tx_pos: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> EncryptedStream<S> {
    /// Poll until one complete ciphertext frame has been read. `Ok(None)`
    /// is a clean EOF at a frame boundary; an EOF mid-frame is an error.
    fn poll_recv_frame(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<Vec<u8>>>> {
        while !self.rx.have_len {
            let start = self.rx.len_filled;
            let mut tmp = ReadBuf::new(&mut self.rx.len_buf[start..]);
            ready!(Pin::new(&mut self.io).poll_read(cx, &mut tmp))?;
            let got = tmp.filled().len();
            if got == 0 {
                if self.rx.len_filled == 0 {
                    return Poll::Ready(Ok(None));
                }
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF within a frame length prefix",
                )));
            }
            self.rx.len_filled += got;
            if self.rx.len_filled == 2 {
                let n = u16::from_be_bytes(self.rx.len_buf) as usize;
                self.rx.payload = vec![0u8; n];
                self.rx.payload_filled = 0;
                self.rx.have_len = true;
            }
        }

        while self.rx.payload_filled < self.rx.payload.len() {
            let start = self.rx.payload_filled;
            let mut tmp = ReadBuf::new(&mut self.rx.payload[start..]);
            ready!(Pin::new(&mut self.io).poll_read(cx, &mut tmp))?;
            let got = tmp.filled().len();
            if got == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF within a frame payload",
                )));
            }
            self.rx.payload_filled += got;
        }

        let frame = std::mem::take(&mut self.rx.payload);
        self.rx = RxFrame::default();
        Poll::Ready(Ok(Some(frame)))
    }
}

impl<S: AsyncWrite + Unpin> EncryptedStream<S> {
    /// Flush the staged ciphertext frame to the underlying writer.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.tx_pos < self.tx_buf.len() {
            let n = ready!(Pin::new(&mut self.io).poll_write(cx, &self.tx_buf[self.tx_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "underlying writer accepted no bytes",
                )));
            }
            self.tx_pos += n;
        }
        self.tx_buf.clear();
        self.tx_pos = 0;
        Poll::Ready(Ok(()))
    }
}

fn noise_to_io(e: snow::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

impl<S: AsyncRead + Unpin> AsyncRead for EncryptedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
            if me.rx_pos < me.rx_plain.len() {
                let n = buf.remaining().min(me.rx_plain.len() - me.rx_pos);
                buf.put_slice(&me.rx_plain[me.rx_pos..me.rx_pos + n]);
                me.rx_pos += n;
                if me.rx_pos == me.rx_plain.len() {
                    me.rx_plain.clear();
                    me.rx_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match ready!(me.poll_recv_frame(cx))? {
                None => return Poll::Ready(Ok(())),
                Some(frame) => {
                    let mut plain = vec![0u8; frame.len()];
                    let n = me
                        .transport
                        .read_message(&frame, &mut plain)
                        .map_err(noise_to_io)?;
                    plain.truncate(n);
                    me.rx_plain = plain;
                    me.rx_pos = 0;
                }
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for EncryptedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        // Deliver any staged frame before staging another, so a slow
        // reader back-pressures the writer instead of unbounded buffering.
        ready!(me.poll_drain(cx))?;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let n = buf.len().min(MAX_PAYLOAD_LEN);
        let mut cipher = vec![0u8; n + NOISE_TAG_LEN];
        let cipher_len = me
            .transport
            .write_message(&buf[..n], &mut cipher)
            .map_err(noise_to_io)?;
        me.tx_buf.clear();
        me.tx_pos = 0;
        me.tx_buf
            .extend_from_slice(&(cipher_len as u16).to_be_bytes());
        me.tx_buf.extend_from_slice(&cipher[..cipher_len]);
        // Best-effort push onto the wire now; a `Pending` drain leaves
        // the frame staged for the next `poll_write`/`poll_flush`, but
        // the `n` bytes are already accepted.
        if let Poll::Ready(Err(e)) = me.poll_drain(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        ready!(me.poll_drain(cx))?;
        Pin::new(&mut me.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        ready!(me.poll_drain(cx))?;
        Pin::new(&mut me.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Run both handshakes concurrently over a loopback duplex, returning
    /// the two encrypted stream halves plus the identity the box learned.
    async fn paired() -> (
        EncryptedStream<tokio::io::DuplexStream>,
        EncryptedStream<tokio::io::DuplexStream>,
        PublicKey,
        PublicKey,
    ) {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let box_identity = Identity::generate().expect("box identity");
        let box_pub = box_identity.public_key();
        let device_identity = Identity::generate().expect("device identity");
        let device_pub = device_identity.public_key();

        let server =
            tokio::spawn(async move { responder_handshake(server_io, &box_identity).await });
        let client = initiator_handshake(client_io, &device_identity, &box_pub)
            .await
            .expect("initiator handshake");
        let (server, learned_device) = server
            .await
            .expect("server task")
            .expect("responder handshake");

        (client, server, device_pub, learned_device)
    }

    /// The QR encode/decode path: a public key survives the byte
    /// round-trip a pairing code puts it through.
    #[test]
    fn public_key_round_trips_through_bytes() {
        let key = Identity::generate().expect("identity").public_key();
        assert_eq!(PublicKey::from_bytes(*key.as_bytes()), key);
    }

    #[tokio::test]
    async fn round_trips_plaintext_in_both_directions() {
        let (mut client, mut server, _, _) = paired().await;

        client
            .write_all(b"prompt from device")
            .await
            .expect("write");
        client.flush().await.expect("flush");
        let mut buf = [0u8; 18];
        server.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"prompt from device");

        server.write_all(b"terminal output").await.expect("write");
        server.flush().await.expect("flush");
        let mut buf = [0u8; 15];
        client.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"terminal output");
    }

    /// The box pins the device's identity: `responder_handshake` returns
    /// exactly the public key the client authenticated with.
    #[tokio::test]
    async fn responder_learns_the_initiator_identity() {
        let (_client, _server, device_pub, learned) = paired().await;
        assert_eq!(device_pub, learned);
    }

    /// A payload far larger than one Noise message must split across many
    /// transport messages and reassemble byte-identically — this is the
    /// property that lets the existing frame stream run inside the channel
    /// unchanged.
    #[tokio::test]
    async fn large_payload_spans_many_noise_messages() {
        let (mut client, mut server, _, _) = paired().await;

        // A non-uniform pattern so a reordered or dropped message can't
        // silently reassemble to the original.
        let payload: Vec<u8> = (0..(MAX_PAYLOAD_LEN * 4 + 7919))
            .map(|i| (i % 251) as u8)
            .collect();
        let expected = payload.clone();

        let writer = tokio::spawn(async move {
            client.write_all(&payload).await.expect("write large");
            client.flush().await.expect("flush");
            // Keep the stream open until the reader has drained it.
            client
        });

        let mut got = vec![0u8; expected.len()];
        server.read_exact(&mut got).await.expect("read large");
        assert_eq!(got, expected);
        let _client = writer.await.expect("writer task");
    }

    /// Pinning the wrong box key is the anti-MITM guarantee: a client that
    /// authenticates against a key the box does not hold cannot complete
    /// the handshake.
    #[tokio::test]
    async fn wrong_pin_fails_the_handshake() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let box_identity = Identity::generate().expect("box identity");
        let device_identity = Identity::generate().expect("device identity");
        // The client pins a key that belongs to nobody in this handshake.
        let impostor = Identity::generate().expect("impostor").public_key();

        let server =
            tokio::spawn(async move { responder_handshake(server_io, &box_identity).await });
        let client = initiator_handshake(client_io, &device_identity, &impostor).await;

        assert!(
            client.is_err(),
            "client must not connect to an unpinned box"
        );
        assert!(
            server.await.expect("server task").is_err(),
            "box must reject a device that authenticated it with the wrong key"
        );
    }

    /// A blind relay in the middle only ever forwards ciphertext: a secret
    /// written as plaintext must never appear verbatim in the bytes the
    /// relay carries.
    #[tokio::test]
    async fn relay_forwards_only_ciphertext() {
        // client <-> relay <-> box, each hop its own loopback duplex.
        let (client_io, relay_to_client) = tokio::io::duplex(64 * 1024);
        let (relay_to_box, box_io) = tokio::io::duplex(64 * 1024);

        let seen: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (rc_rd, rc_wr) = tokio::io::split(relay_to_client);
        let (rb_rd, rb_wr) = tokio::io::split(relay_to_box);
        tokio::spawn(tee(rc_rd, rb_wr, seen.clone())); // client -> box
        tokio::spawn(tee(rb_rd, rc_wr, seen.clone())); // box -> client

        let box_identity = Identity::generate().expect("box identity");
        let box_pub = box_identity.public_key();
        let device_identity = Identity::generate().expect("device identity");

        let server = tokio::spawn(async move { responder_handshake(box_io, &box_identity).await });
        let mut client = initiator_handshake(client_io, &device_identity, &box_pub)
            .await
            .expect("initiator handshake");
        let (mut server, _) = server
            .await
            .expect("server task")
            .expect("responder handshake");

        const SECRET: &[u8] = b"ghp_this_token_must_never_hit_the_relay_in_clear";
        client.write_all(SECRET).await.expect("write");
        client.flush().await.expect("flush");
        let mut buf = vec![0u8; SECRET.len()];
        server.read_exact(&mut buf).await.expect("read");
        assert_eq!(buf, SECRET);

        let carried = seen.lock().expect("lock");
        assert!(
            !contains_subslice(&carried, SECRET),
            "the relay must never see the plaintext secret"
        );
        assert!(!carried.is_empty(), "the relay did carry (encrypted) bytes");
    }

    async fn tee<R, W>(mut r: R, mut w: W, sink: Arc<Mutex<Vec<u8>>>)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut buf = [0u8; 4096];
        loop {
            let n = match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            sink.lock().expect("lock").extend_from_slice(&buf[..n]);
            if w.write_all(&buf[..n]).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
    }

    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
