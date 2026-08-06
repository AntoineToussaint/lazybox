//! The two peer sides of the relay: a **box** that dials out and holds
//! its control connection open, and a **client** that reaches a box
//! through the relay by box-id.
//!
//! Both return / hand off a live [`TcpStream`] whose payload is spliced
//! straight through to the other peer. That stream carries **plaintext
//! today**; wrap it in the E2E channel (#891) before sending anything
//! sensitive — the relay forwards whatever bytes it is given.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use tokio::net::{TcpStream, ToSocketAddrs};
use uuid::Uuid;

use crate::protocol::{Ack, Hello, ToBox, read_msg, write_msg};

/// Called with a fresh, already-brokered data stream for each client the
/// relay announces. The box bridges it to its local service.
pub type OnClient =
    Arc<dyn Fn(TcpStream) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Connect through the relay to the box registered as `box_id`.
///
/// Returns a live stream spliced to the box's data connection. Perform
/// the E2E handshake (#891) on it before sending any plaintext — the box
/// public key pins the box's identity, so a malicious relay cannot MITM.
pub async fn connect_through_relay<A: ToSocketAddrs>(
    relay_addr: A,
    box_id: &str,
) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(relay_addr).await?;
    write_msg(
        &mut stream,
        &Hello::ConnectClient {
            box_id: box_id.to_string(),
        },
    )
    .await?;
    match read_msg::<_, Ack>(&mut stream).await? {
        Ack::Ok => Ok(stream),
        Ack::Unavailable => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no box registered as {box_id}"),
        )),
    }
}

/// Dial out to the relay, register `box_id` under `account`, and hold the
/// control connection open. For every client the relay brokers, dial a
/// fresh data connection and invoke `on_client` with the spliced stream.
///
/// Returns when the control connection drops (relay restart, network
/// blip); callers reconnect with backoff.
pub async fn serve_box(
    relay_addr: String,
    box_id: String,
    account: String,
    on_client: OnClient,
) -> io::Result<()> {
    let mut control = TcpStream::connect(&relay_addr).await?;
    write_msg(&mut control, &Hello::RegisterBox { box_id, account }).await?;
    match read_msg::<_, Ack>(&mut control).await? {
        Ack::Ok => {}
        Ack::Unavailable => return Err(io::Error::other("relay refused box registration")),
    }

    loop {
        // `read_msg` errors on EOF, which returns and lets the caller
        // reconnect.
        let ToBox::NewClient { session_id } = read_msg::<_, ToBox>(&mut control).await?;
        let relay_addr = relay_addr.clone();
        let on_client = Arc::clone(&on_client);
        tokio::spawn(async move {
            if let Err(error) = dial_data(&relay_addr, session_id, on_client).await {
                tracing::warn!(%error, "relay data connection failed");
            }
        });
    }
}

async fn dial_data(relay_addr: &str, session_id: Uuid, on_client: OnClient) -> io::Result<()> {
    let mut data = TcpStream::connect(relay_addr).await?;
    // The relay splices a `BoxData` connection immediately — no ack, or
    // it would corrupt the client↔box stream.
    write_msg(&mut data, &Hello::BoxData { session_id }).await?;
    on_client(data).await;
    Ok(())
}
