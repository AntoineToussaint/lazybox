//! Relay wire protocol: a small length-prefixed bincode handshake per
//! connection, then a raw byte splice.
//!
//! The relay reads exactly one handshake frame from each connection to
//! learn its role and routing key. Everything that flows afterwards is
//! **opaque** — the relay copies it verbatim between the client and the
//! box and never decodes it. Once the E2E channel (#891) wraps the byte
//! stream, those bytes are Noise ciphertext; the relay's behaviour is
//! identical either way, which is exactly why it can be dumb and blind.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

/// Ceiling on a single handshake frame. A handshake carries a box id and
/// public key or a UUID — well under a kilobyte in practice; this only
/// bounds the allocation a hostile peer can request before it has proven
/// anything.
pub const MAX_HANDSHAKE_BYTES: u32 = 64 * 1024;

/// The first frame every connection sends, declaring its role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Hello {
    /// A box dialing **out** to register under `box_id` and hold the
    /// control connection open. Works behind NAT — no inbound ports.
    RegisterBox {
        box_id: String,
        box_public_key: String,
    },
    /// A client asking to be brokered to the box registered as `box_id`.
    ConnectClient { box_id: String },
    /// A box opening a fresh data connection in response to a
    /// [`ToBox::NewClient`] announcement, to be spliced to that client.
    BoxData { session_id: Uuid },
}

/// The relay's reply to a [`Hello`] that expects an acknowledgement
/// (`RegisterBox` and `ConnectClient`). A `BoxData` connection gets **no**
/// ack — the relay splices it immediately, so any injected byte would
/// corrupt the client↔box stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Ack {
    /// Registration accepted, or a box is registered and brokering is
    /// under way — proceed with the E2E handshake.
    Ok,
    /// No box is registered under the requested id.
    Unavailable,
    /// The box does not have an active hosted-relay subscription.
    SubscriptionRequired,
    /// The box failed to prove possession of its claimed private key.
    AuthenticationFailed,
}

/// A one-time challenge a registering box must sign with its Ed25519 key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RegistrationChallenge {
    pub nonce: Uuid,
}

/// The box's signature over [`registration_payload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RegistrationProof {
    pub signature: Vec<u8>,
}

const REGISTRATION_DOMAIN: &[u8] = b"lazybox-relay-register-v1\0";

/// Build the unambiguous, domain-separated message a box signs to register.
pub fn registration_payload(
    challenge: &RegistrationChallenge,
    box_id: &str,
    box_public_key: &str,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(
        REGISTRATION_DOMAIN.len() + 16 + 8 + box_id.len() + box_public_key.len(),
    );
    payload.extend_from_slice(REGISTRATION_DOMAIN);
    payload.extend_from_slice(challenge.nonce.as_bytes());
    payload.extend_from_slice(&(box_id.len() as u32).to_be_bytes());
    payload.extend_from_slice(box_id.as_bytes());
    payload.extend_from_slice(&(box_public_key.len() as u32).to_be_bytes());
    payload.extend_from_slice(box_public_key.as_bytes());
    payload
}

/// A control message pushed from the relay down a box's held control
/// connection when a client wants in. The box answers by dialing a fresh
/// [`Hello::BoxData`] connection for `session_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum ToBox {
    NewClient { session_id: Uuid },
}

fn config() -> impl bincode::config::Config {
    bincode::config::legacy().with_limit::<{ MAX_HANDSHAKE_BYTES as usize }>()
}

/// Write one length-prefixed bincode frame (u32 big-endian length).
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = bincode::serde::encode_to_vec(msg, config()).map_err(std::io::Error::other)?;
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|&l| l <= MAX_HANDSHAKE_BYTES)
        .ok_or_else(|| std::io::Error::other("handshake frame exceeds the maximum length"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await
}

/// Read one frame written by [`write_msg`]. Reads exactly the framed
/// bytes and no more, so the stream stays aligned for the raw splice
/// that follows.
pub async fn read_msg<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_HANDSHAKE_BYTES {
        return Err(std::io::Error::other(
            "peer handshake frame exceeds the maximum length",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let (msg, _consumed) =
        bincode::serde::decode_from_slice(&buf, config()).map_err(std::io::Error::other)?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let sent = Hello::RegisterBox {
            box_id: "box-123".into(),
            box_public_key: "Ym94LWtleQ==".into(),
        };
        write_msg(&mut a, &sent).await.unwrap();
        let got: Hello = read_msg(&mut b).await.unwrap();
        assert_eq!(sent, got);
    }

    #[tokio::test]
    async fn subscription_required_ack_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_msg(&mut a, &Ack::SubscriptionRequired).await.unwrap();
        let got: Ack = read_msg(&mut b).await.unwrap();
        assert_eq!(got, Ack::SubscriptionRequired);
    }

    #[test]
    fn registration_payload_binds_every_claim_field() {
        let challenge = RegistrationChallenge {
            nonce: Uuid::new_v4(),
        };
        let payload = registration_payload(&challenge, "box-a", "key-a");
        assert_ne!(payload, registration_payload(&challenge, "box-b", "key-a"));
        assert_ne!(payload, registration_payload(&challenge, "box-a", "key-b"));
        assert_ne!(
            payload,
            registration_payload(
                &RegistrationChallenge {
                    nonce: Uuid::new_v4()
                },
                "box-a",
                "key-a"
            )
        );
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected() {
        let (mut a, mut b) = tokio::io::duplex(16);
        a.write_all(&(MAX_HANDSHAKE_BYTES + 1).to_be_bytes())
            .await
            .unwrap();
        let err = read_msg::<_, Hello>(&mut b).await.unwrap_err();
        assert!(err.to_string().contains("maximum length"));
    }
}
