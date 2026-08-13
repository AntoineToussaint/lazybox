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

use lazybox_identity::BoxIdentity;

use crate::protocol::{
    Ack, Hello, RegistrationChallenge, RegistrationProof, ToBox, read_msg, registration_payload,
    write_msg,
};

pub const SUBSCRIPTION_REQUIRED_MESSAGE: &str =
    "subscription required — https://lazybox.ai/pricing";

#[derive(Debug, thiserror::Error)]
pub enum RelayClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("no box registered as {box_id}")]
    Unavailable { box_id: String },
    #[error("subscription required — https://lazybox.ai/pricing")]
    SubscriptionRequired,
    #[error("relay rejected the box identity proof")]
    AuthenticationFailed,
}

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
) -> Result<TcpStream, RelayClientError> {
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
        Ack::Unavailable => Err(RelayClientError::Unavailable {
            box_id: box_id.to_string(),
        }),
        Ack::SubscriptionRequired => Err(RelayClientError::SubscriptionRequired),
        Ack::AuthenticationFailed => Err(io::Error::other(
            "relay returned a box-registration acknowledgement to a client connection",
        )
        .into()),
    }
}

/// Dial out to the relay, prove `box_identity` owns its public key, and
/// hold the control connection open. For every client the relay brokers,
/// dial a fresh data connection and invoke `on_client` with the spliced
/// stream.
///
/// Returns when the control connection drops (relay restart, network
/// blip); callers reconnect with backoff.
pub async fn serve_box(
    relay_addr: String,
    box_id: String,
    box_identity: Arc<BoxIdentity>,
    on_client: OnClient,
) -> Result<(), RelayClientError> {
    let mut control = TcpStream::connect(&relay_addr).await?;
    let box_public_key = box_identity.public_key_base64();
    write_msg(
        &mut control,
        &Hello::RegisterBox {
            box_id: box_id.clone(),
            box_public_key: box_public_key.clone(),
        },
    )
    .await?;
    let challenge: RegistrationChallenge = read_msg(&mut control).await?;
    let signature = box_identity
        .sign(&registration_payload(&challenge, &box_id, &box_public_key))
        .to_bytes()
        .to_vec();
    write_msg(&mut control, &RegistrationProof { signature }).await?;
    match read_msg::<_, Ack>(&mut control).await? {
        Ack::Ok => {}
        Ack::Unavailable => return Err(io::Error::other("relay refused box registration").into()),
        Ack::SubscriptionRequired => return Err(RelayClientError::SubscriptionRequired),
        Ack::AuthenticationFailed => return Err(RelayClientError::AuthenticationFailed),
    }

    loop {
        let session_id = match read_msg::<_, ToBox>(&mut control).await {
            Ok(ToBox::NewClient { session_id }) => session_id,
            // A clean control-connection close is the normal reconnect
            // trigger (relay restart, network blip), not a failure.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };
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
