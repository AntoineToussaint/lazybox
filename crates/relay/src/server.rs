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

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::protocol::{Ack, Hello, ToBox, read_msg, write_msg};

/// How long a client waits for the box's data connection after it has
/// been announced, before giving up and closing.
const DEFAULT_BROKER_TIMEOUT: Duration = Duration::from_secs(10);

/// A registered box's control channel.
struct BoxHandle {
    /// Announces new clients down the box's held control connection.
    to_box: mpsc::Sender<ToBox>,
}

/// Shared relay state. Cheap to clone (`Arc`-wrapped internally by
/// [`Relay::serve`]).
#[derive(Default)]
pub struct Relay {
    boxes: Mutex<HashMap<String, BoxHandle>>,
    /// Clients parked awaiting their box's data connection, keyed by the
    /// relay-assigned session id.
    pending: Mutex<HashMap<Uuid, oneshot::Sender<TcpStream>>>,
    broker_timeout: Duration,
}

impl Relay {
    pub fn new() -> Self {
        Self {
            broker_timeout: DEFAULT_BROKER_TIMEOUT,
            ..Self::default()
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
        match read_msg::<_, Hello>(&mut stream).await? {
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
        // `account` is recorded for the future entitlement gate (#749
        // milestone 3); today the relay brokers unconditionally.
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

        if to_box.send(ToBox::NewClient { session_id }).await.is_err() {
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
