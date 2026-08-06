//! Multiplex workload TCP ports over a single framed byte channel.
//!
//! This is the "forward the workload port" half of relay pairing (#894):
//! the box runs a workload (e.g. an app on `localhost:3000`), and a paired
//! client wants to reach it as `localhost:3000` on its *own* machine — so a
//! browser there hits `localhost:3000` and an OAuth/WorkOS callback resolves
//! with no public host and no allowlist change.
//!
//! The two ends speak [`MuxFrame`]s over any tokio `AsyncRead`/`AsyncWrite`
//! pair — the same transport seam the daemon socket already rides
//! (`crate::transport`). Today that pair is a Unix socket or an in-process
//! channel; once the relay lands (#893) it is the end-to-end-encrypted relay
//! tunnel, unchanged. The multiplexer never sees the carrier.
//!
//! Roles are asymmetric only in who initiates a stream:
//! - The **client** binds `127.0.0.1:<local>` listeners. Each accepted TCP
//!   connection becomes a stream: it sends [`MuxFrame::Open`] and pumps bytes.
//! - The **box** receives `Open`, dials `127.0.0.1:<workload>`, and pumps the
//!   other way. It only dials ports on its [`run_box_forwarder`] allowlist —
//!   a client cannot reach an arbitrary local service on the box.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};

use crate::socket::{read_frame, write_frame};

/// A logical TCP stream, allocated monotonically by the client (the only
/// initiator), so ids never collide or need reuse for the channel's life.
pub type StreamId = u64;

/// Outbound frames buffered toward the wire. Bounded so a slow carrier
/// applies real backpressure to every stream's socket-reader.
const FRAME_BUFFER: usize = 256;

/// Bytes read from a local socket per `Data` frame.
const READ_CHUNK: usize = 16 * 1024;

/// One workload port to forward: the box dials `localhost:workload_port`,
/// the client exposes it as `127.0.0.1:local_port`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortMap {
    pub workload_port: u16,
    pub local_port: u16,
}

impl PortMap {
    /// Forward a port under the same number on both ends — the common case
    /// (`localhost:3000` on the box shows up as `localhost:3000` on the
    /// client), which is exactly what makes an OAuth callback resolve.
    pub fn same(port: u16) -> Self {
        Self {
            workload_port: port,
            local_port: port,
        }
    }
}

/// The multiplexer wire message. `Data` carries a chunk for one stream;
/// `Open` starts one (client → box); `Close` signals the sender's write
/// half ended (either direction), giving proper TCP half-close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MuxFrame {
    Open { stream: StreamId, port: u16 },
    Data { stream: StreamId, data: Vec<u8> },
    Close { stream: StreamId },
}

/// Shared multiplexer state: the outbound frame queue plus, per live stream,
/// the sink that feeds bytes arriving from the peer into the local socket.
struct Mux {
    frame_tx: mpsc::Sender<MuxFrame>,
    inbound: Mutex<HashMap<StreamId, mpsc::UnboundedSender<Vec<u8>>>>,
}

/// A `127.0.0.1` listener bound for one forwarded port, kept separate from
/// [`run_client_forwarder`] so a caller (and the tests) can learn the actual
/// bound address before the channel is up — `local_port` may be `0`.
pub struct BoundPort {
    pub workload_port: u16,
    pub local_addr: SocketAddr,
    listener: TcpListener,
}

impl BoundPort {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

/// Bind a `127.0.0.1` listener for each mapping. Binding is split from
/// serving so the real bound port (when `local_port` is `0`) is knowable
/// up front.
pub async fn bind_local_ports(ports: &[PortMap]) -> io::Result<Vec<BoundPort>> {
    let mut bound = Vec::with_capacity(ports.len());
    for pm in ports {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, pm.local_port)).await?;
        let local_addr = listener.local_addr()?;
        bound.push(BoundPort {
            workload_port: pm.workload_port,
            local_addr,
            listener,
        });
    }
    Ok(bound)
}

/// Run the client end: accept local connections on each [`BoundPort`] and
/// forward them, multiplexed, over `(read, write)`. Returns when the peer
/// closes the channel.
pub async fn run_client_forwarder<R, W>(read: R, write: W, bound: Vec<BoundPort>) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (frame_tx, frame_rx) = mpsc::channel(FRAME_BUFFER);
    let mux = Arc::new(Mux {
        frame_tx,
        inbound: Mutex::new(HashMap::new()),
    });
    tokio::spawn(writer_task(write, frame_rx));

    let next_stream = Arc::new(AtomicU64::new(1));
    for port in bound {
        let mux = Arc::clone(&mux);
        let next_stream = Arc::clone(&next_stream);
        tokio::spawn(accept_loop(port, mux, next_stream));
    }

    reader_loop(read, mux, None).await;
    Ok(())
}

/// Run the box end: honor `Open` for any port in `allowed_ports` by dialing
/// `127.0.0.1:<port>` and pumping bytes back. Requests for other ports are
/// refused with an immediate `Close`. Returns when the peer closes the
/// channel.
pub async fn run_box_forwarder<R, W>(read: R, write: W, allowed_ports: &[u16]) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (frame_tx, frame_rx) = mpsc::channel(FRAME_BUFFER);
    let mux = Arc::new(Mux {
        frame_tx,
        inbound: Mutex::new(HashMap::new()),
    });
    tokio::spawn(writer_task(write, frame_rx));

    reader_loop(read, mux, Some(allowed_ports.to_vec())).await;
    Ok(())
}

async fn accept_loop(port: BoundPort, mux: Arc<Mux>, next_stream: Arc<AtomicU64>) {
    loop {
        let Ok((tcp, _)) = port.listener.accept().await else {
            break;
        };
        let stream = next_stream.fetch_add(1, Ordering::Relaxed);
        // Register the inbound sink before sending Open, so an early reply
        // from the box is buffered rather than dropped. Open must be the
        // first frame for this stream, so enqueue it before spawning the
        // socket-reader that emits this stream's `Data`.
        let inbound = register_stream(&mux, stream).await;
        if mux
            .frame_tx
            .send(MuxFrame::Open {
                stream,
                port: port.workload_port,
            })
            .await
            .is_err()
        {
            break;
        }
        spawn_pumps(&mux, stream, tcp, inbound);
    }
}

/// The frame dispatch loop. `allowed_ports` is `Some` on the box (it may dial)
/// and `None` on the client (it never receives `Open`).
async fn reader_loop<R>(mut read: R, mux: Arc<Mux>, allowed_ports: Option<Vec<u16>>)
where
    R: AsyncRead + Unpin,
{
    while let Ok(Some(frame)) = read_frame::<_, MuxFrame>(&mut read).await {
        match frame {
            MuxFrame::Open { stream, port } => {
                let permitted = allowed_ports.as_ref().is_some_and(|a| a.contains(&port));
                if permitted {
                    // Register the sink now — before the dial's network round
                    // trip — so `Data` frames racing in behind `Open` buffer
                    // instead of hitting a missing stream and being dropped.
                    let inbound = register_stream(&mux, stream).await;
                    tokio::spawn(dial_and_attach(Arc::clone(&mux), stream, port, inbound));
                } else {
                    let _ = mux.frame_tx.send(MuxFrame::Close { stream }).await;
                }
            }
            MuxFrame::Data { stream, data } => {
                let sink = mux.inbound.lock().await.get(&stream).cloned();
                if let Some(sink) = sink {
                    let _ = sink.send(data);
                }
            }
            MuxFrame::Close { stream } => {
                // Dropping the sink closes the write task's channel, which
                // shuts down the local socket's write half — TCP half-close.
                mux.inbound.lock().await.remove(&stream);
            }
        }
    }
}

async fn dial_and_attach(
    mux: Arc<Mux>,
    stream: StreamId,
    port: u16,
    inbound: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    match TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await {
        Ok(tcp) => spawn_pumps(&mux, stream, tcp, inbound),
        Err(_) => {
            mux.inbound.lock().await.remove(&stream);
            let _ = mux.frame_tx.send(MuxFrame::Close { stream }).await;
        }
    }
}

/// Register a stream's inbound sink and hand back the receiver the socket's
/// write half will drain. Inbound is unbounded on purpose: the reader loop
/// must never block on one slow socket, or a full per-stream buffer would
/// head-of-line-block every other stream on the shared channel. Depth is
/// bounded in practice by the peer's TCP receive window on a loopback
/// workload.
async fn register_stream(mux: &Arc<Mux>, stream: StreamId) -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    mux.inbound.lock().await.insert(stream, in_tx);
    in_rx
}

/// Spawn the two pumps for a live socket: peer bytes → socket write half, and
/// socket read half → `Data` frames (with a terminal `Close`).
fn spawn_pumps(
    mux: &Arc<Mux>,
    stream: StreamId,
    tcp: TcpStream,
    mut inbound: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let (mut rd, mut wr) = tcp.into_split();

    tokio::spawn(async move {
        while let Some(data) = inbound.recv().await {
            if wr.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    let frame_tx = mux.frame_tx.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = frame_tx.send(MuxFrame::Close { stream }).await;
                    break;
                }
                Ok(n) => {
                    let frame = MuxFrame::Data {
                        stream,
                        data: buf[..n].to_vec(),
                    };
                    if frame_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

async fn writer_task<W>(mut write: W, mut frame_rx: mpsc::Receiver<MuxFrame>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = frame_rx.recv().await {
        if write_frame(&mut write, &frame).await.is_err() {
            break;
        }
    }
    let _ = write.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener as TokioTcpListener;
    use tokio::time::timeout;

    /// Fail fast rather than hanging the suite if bytes never arrive.
    const READ_TIMEOUT: Duration = Duration::from_secs(5);

    /// Spawn a throwaway `127.0.0.1` echo server; return its port.
    async fn spawn_echo() -> u16 {
        let listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        port
    }

    /// Wire a client forwarder and a box forwarder together over an
    /// in-memory duplex and return the client's bound local addresses.
    async fn forward(ports: Vec<PortMap>, allowed: Vec<u16>) -> Vec<SocketAddr> {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (a_rd, a_wr) = tokio::io::split(a);
        let (b_rd, b_wr) = tokio::io::split(b);

        let bound = bind_local_ports(&ports).await.unwrap();
        let addrs: Vec<SocketAddr> = bound.iter().map(|p| p.local_addr).collect();

        tokio::spawn(async move {
            let _ = run_client_forwarder(a_rd, a_wr, bound).await;
        });
        tokio::spawn(async move {
            let _ = run_box_forwarder(b_rd, b_wr, &allowed).await;
        });

        addrs
    }

    #[tokio::test]
    async fn forwards_bytes_to_the_workload_and_back() {
        let workload = spawn_echo().await;
        let addrs = forward(
            vec![PortMap {
                workload_port: workload,
                local_port: 0,
            }],
            vec![workload],
        )
        .await;
        let local = addrs[0];

        let mut client = TcpStream::connect(local).await.unwrap();
        client.write_all(b"hello relay").await.unwrap();

        let mut buf = vec![0u8; 11];
        timeout(READ_TIMEOUT, client.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf, b"hello relay");
    }

    #[tokio::test]
    async fn multiplexes_independent_streams() {
        let workload = spawn_echo().await;
        let addrs = forward(
            vec![PortMap {
                workload_port: workload,
                local_port: 0,
            }],
            vec![workload],
        )
        .await;
        let local = addrs[0];

        let mut c1 = TcpStream::connect(local).await.unwrap();
        let mut c2 = TcpStream::connect(local).await.unwrap();
        c1.write_all(b"one").await.unwrap();
        c2.write_all(b"twotwo").await.unwrap();

        let mut b1 = vec![0u8; 3];
        let mut b2 = vec![0u8; 6];
        timeout(READ_TIMEOUT, c1.read_exact(&mut b1))
            .await
            .unwrap()
            .unwrap();
        timeout(READ_TIMEOUT, c2.read_exact(&mut b2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&b1, b"one");
        assert_eq!(&b2, b"twotwo");
    }

    #[tokio::test]
    async fn refuses_a_port_off_the_allowlist() {
        let workload = spawn_echo().await;
        // Allowlist a *different* port than the one the client forwards.
        let addrs = forward(
            vec![PortMap {
                workload_port: workload,
                local_port: 0,
            }],
            vec![workload ^ 1],
        )
        .await;
        let local = addrs[0];

        let mut client = TcpStream::connect(local).await.unwrap();
        // The box refuses with Close; the client's socket gets no data and
        // sees EOF once the write half shuts down.
        let mut buf = vec![0u8; 1];
        let n = timeout(READ_TIMEOUT, client.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0, "off-allowlist stream must be closed, not forwarded");
    }
}
