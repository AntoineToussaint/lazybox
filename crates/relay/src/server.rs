//! The rendezvous relay.
//!
//! Boxes dial **out** and register under a box-id, holding a control
//! connection open (works behind NAT — no inbound ports, DNS, or certs).
//! A client dials the relay asking for a box-id; the relay announces the
//! client to that box over its control connection, the box dials a fresh
//! data connection back, and the relay splices the two raw byte streams.
//! It forwards **ciphertext only** and executes nothing.
//!
//! The pairing is a reverse tunnel: one held control connection per box,
//! plus one short-lived data connection per brokered client.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use lazybox_entitlement::{AccountId, AllowAll, EntitlementGate};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::protocol::{Ack, Hello, ToBox, read_msg, write_msg};

/// How long a client waits for the box's data connection after it has
/// been announced, before giving up and closing.
const DEFAULT_BROKER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a freshly-accepted connection has to send its [`Hello`]
/// frame before the relay drops it. Bounds the resources an idle or
/// dead peer can hold on an internet-facing relay (mirrors
/// `lazybox_ipc::socket::HANDSHAKE_TIMEOUT`).
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A registered box's control channel.
struct BoxHandle {
    /// Announces new clients down the box's held control connection.
    to_box: mpsc::Sender<ToBox>,
}

/// Shared relay state. Cheap to clone (`Arc`-wrapped internally by
/// [`Relay::serve`]).
pub struct Relay {
    boxes: Mutex<HashMap<String, BoxHandle>>,
    /// Clients parked awaiting their box's data connection, keyed by the
    /// relay-assigned session id.
    pending: Mutex<HashMap<Uuid, oneshot::Sender<TcpStream>>>,
    /// The subscription check run before a box is allowed to register —
    /// the relay's payment-enforcement point. [`AllowAll`] by default so
    /// behaviour is unchanged until real licensing backs it.
    gate: Box<dyn EntitlementGate>,
    broker_timeout: Duration,
    handshake_timeout: Duration,
}

/// Delegates to [`Relay::new`] rather than a derive: a derived `Default`
/// would zero both timeouts, and a zero handshake timeout drops every
/// connection on arrival — a functional relay must carry the real defaults.
impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}

impl Relay {
    pub fn new() -> Self {
        Self::with_gate(Box::new(AllowAll))
    }

    /// A relay whose brokering is gated by `gate`. `new()` wires
    /// [`AllowAll`]; a deployment swaps in the real subscription check
    /// here without touching the broker.
    pub fn with_gate(gate: Box<dyn EntitlementGate>) -> Self {
        Self {
            boxes: Mutex::default(),
            pending: Mutex::default(),
            gate,
            broker_timeout: DEFAULT_BROKER_TIMEOUT,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    /// Number of boxes currently registered. Exposed for observability and
    /// tests.
    pub async fn registered_boxes(&self) -> usize {
        self.boxes.lock().await.len()
    }

    /// Accept and broker connections until `listener` errors fatally.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let relay = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(error) = relay.handle(stream).await {
                    tracing::debug!(%peer, %error, "relay connection ended");
                }
            });
        }
    }

    async fn handle(self: Arc<Self>, mut stream: TcpStream) -> std::io::Result<()> {
        let hello = tokio::time::timeout(self.handshake_timeout, read_msg::<_, Hello>(&mut stream))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "handshake timed out")
            })??;
        match hello {
            Hello::RegisterBox { box_id, account } => {
                self.register_box(stream, box_id, account).await
            }
            Hello::ConnectClient { box_id } => self.broker_client(stream, box_id).await,
            Hello::BoxData { session_id } => {
                self.deliver_box_data(stream, session_id).await;
                Ok(())
            }
        }
    }

    /// Hold a box's control connection open, forwarding client
    /// announcements to it and noticing when it drops.
    async fn register_box(
        self: Arc<Self>,
        mut stream: TcpStream,
        box_id: String,
        account: String,
    ) -> std::io::Result<()> {
        // The entitlement gate is the relay's payment-enforcement point:
        // refuse to register (and therefore to broker for) an account
        // without an active subscription. Fail closed — a lookup error is
        // treated as "not entitled", never as an open door.
        let entitled = match self.gate.check(&AccountId::new(account.clone())).await {
            Ok(decision) => decision.is_active(),
            Err(error) => {
                tracing::warn!(%box_id, %account, %error, "entitlement lookup failed; refusing");
                false
            }
        };
        if !entitled {
            tracing::info!(%box_id, %account, "box registration refused: not entitled");
            write_msg(&mut stream, &Ack::Unavailable).await?;
            return Ok(());
        }

        write_msg(&mut stream, &Ack::Ok).await?;

        let (to_box, mut announcements) = mpsc::channel::<ToBox>(16);
        {
            // A reconnecting box supersedes any stale registration under
            // the same id — last writer wins.
            let mut boxes = self.boxes.lock().await;
            boxes.insert(
                box_id.clone(),
                BoxHandle {
                    to_box: to_box.clone(),
                },
            );
        }
        tracing::info!(%box_id, %account, "box registered");

        let (mut rd, mut wr) = stream.into_split();
        let mut drain = [0u8; 256];
        loop {
            tokio::select! {
                announced = announcements.recv() => match announced {
                    Some(msg) => {
                        if write_msg(&mut wr, &msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                // The box sends nothing on the control connection today;
                // reading it is how we notice EOF (the box went away).
                read = tokio::io::AsyncReadExt::read(&mut rd, &mut drain) => {
                    match read {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            }
        }

        // Only evict our own entry — a newer registration may have
        // replaced us while we were draining.
        let mut boxes = self.boxes.lock().await;
        if boxes
            .get(&box_id)
            .is_some_and(|h| h.to_box.same_channel(&to_box))
        {
            boxes.remove(&box_id);
            tracing::info!(%box_id, "box deregistered");
        }
        Ok(())
    }

    /// Broker a client to its box: announce it, wait for the box's data
    /// connection, then splice the two streams.
    async fn broker_client(
        self: Arc<Self>,
        mut client: TcpStream,
        box_id: String,
    ) -> std::io::Result<()> {
        let to_box = {
            let boxes = self.boxes.lock().await;
            boxes.get(&box_id).map(|h| h.to_box.clone())
        };
        let Some(to_box) = to_box else {
            write_msg(&mut client, &Ack::Unavailable).await?;
            return Ok(());
        };
        write_msg(&mut client, &Ack::Ok).await?;

        let session_id = Uuid::new_v4();
        let (data_tx, data_rx) = oneshot::channel::<TcpStream>();
        self.pending.lock().await.insert(session_id, data_tx);

        // Bound the announce: a half-open box whose 16-slot channel has
        // filled would otherwise wedge this task on `send` until TCP itself
        // times out. Treat a stalled or closed channel as "box unreachable".
        let announced = tokio::time::timeout(
            self.broker_timeout,
            to_box.send(ToBox::NewClient { session_id }),
        )
        .await;
        if !matches!(announced, Ok(Ok(()))) {
            self.pending.lock().await.remove(&session_id);
            return Ok(());
        }

        let mut box_data = match tokio::time::timeout(self.broker_timeout, data_rx).await {
            Ok(Ok(stream)) => stream,
            _ => {
                self.pending.lock().await.remove(&session_id);
                return Ok(());
            }
        };

        // Raw, opaque forwarding — the relay never inspects these bytes.
        let _ = tokio::io::copy_bidirectional(&mut client, &mut box_data).await;
        Ok(())
    }

    /// Hand a box's fresh data connection to the client parked under
    /// `session_id`. The client task owns the splice from here.
    async fn deliver_box_data(self: Arc<Self>, stream: TcpStream, session_id: Uuid) {
        let waiting = self.pending.lock().await.remove(&session_id);
        if let Some(tx) = waiting {
            // If the client already gave up, `send` hands the stream back
            // and it is simply dropped.
            let _ = tx.send(stream);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn default_relay_is_functional() {
        // `Default` must not zero the timeouts — a zero handshake timeout
        // would drop every connection, a zero broker timeout fail every broker.
        let relay = Relay::default();
        assert!(relay.handshake_timeout > Duration::ZERO);
        assert!(relay.broker_timeout > Duration::ZERO);
        assert_eq!(relay.handshake_timeout, Relay::new().handshake_timeout);
        assert_eq!(relay.broker_timeout, Relay::new().broker_timeout);
    }

    #[tokio::test]
    async fn idle_connection_is_dropped_after_handshake_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let relay = Arc::new(Relay {
            handshake_timeout: Duration::from_millis(150),
            broker_timeout: DEFAULT_BROKER_TIMEOUT,
            ..Default::default()
        });
        tokio::spawn(relay.serve(listener));

        // Connect and never send a `Hello` — the relay must close us out.
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("relay should close the idle connection before the test's own deadline")
            .unwrap();
        assert_eq!(read, 0, "expected EOF once the handshake timeout elapsed");
    }
}
