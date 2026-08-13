//! End-to-end brokering over a loopback relay: a box dials out and
//! registers, a client connects by box-id, and bytes flow both ways
//! through the relay — which only ever splices opaque bytes.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::Response;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use lazybox_e2e_channel::{Identity, initiator_handshake, responder_handshake};
use lazybox_entitlement::{
    AccountId, Entitlement, EntitlementError, EntitlementGate, PlatformEntitlementGate,
};
use lazybox_identity::BoxIdentity;
use lazybox_relay::{
    Ack, Hello, RegistrationChallenge, RegistrationProof, Relay, RelayClientError,
    SUBSCRIPTION_REQUIRED_MESSAGE, connect_through_relay, serve_box,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpListener;

fn registration_identity() -> Arc<BoxIdentity> {
    let dir = tempfile::tempdir().unwrap();
    Arc::new(BoxIdentity::load_or_generate(dir.path()).unwrap())
}

/// Start a relay on an ephemeral loopback port and return its address.
async fn start_relay() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(Arc::new(Relay::new()).serve(listener));
    addr
}

/// Register a box whose data connections echo everything back, prefixed
/// with a one-time greeting so both directions are exercised.
fn spawn_echo_box(relay_addr: &str, box_id: &str) {
    let relay_addr = relay_addr.to_string();
    let box_id = box_id.to_string();
    let identity = registration_identity();
    tokio::spawn(async move {
        let _ = serve_box(
            relay_addr,
            box_id,
            identity,
            Arc::new(|mut stream| {
                Box::pin(async move {
                    stream.write_all(b"hello from box").await.unwrap();
                    let mut buf = [0u8; 64];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
            }),
        )
        .await;
    });
}

/// Wait until at least one box is registered, so the client doesn't race
/// the box's dial-out.
async fn await_registration(relay: &Arc<Relay>) {
    for _ in 0..200 {
        if relay.registered_boxes().await > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("box never registered");
}

#[tokio::test]
async fn brokers_client_to_box_both_directions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::new());
    tokio::spawn(Arc::clone(&relay).serve(listener));

    spawn_echo_box(&addr, "box-alpha");
    await_registration(&relay).await;

    let mut client = connect_through_relay(&addr, "box-alpha").await.unwrap();

    // Box → client greeting.
    let mut greeting = [0u8; 14];
    client.read_exact(&mut greeting).await.unwrap();
    assert_eq!(&greeting, b"hello from box");

    // Client → box → client echo.
    client.write_all(b"ping").await.unwrap();
    let mut echo = [0u8; 4];
    client.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"ping");
}

#[tokio::test]
async fn unknown_box_is_rejected() {
    let addr = start_relay().await;
    let err = connect_through_relay(&addr, "nobody-home")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RelayClientError::Unavailable { box_id } if box_id == "nobody-home"
    ));
}

#[tokio::test]
async fn forwards_arbitrary_bytes_untouched() {
    // The relay must not interpret the payload — feed it bytes that look
    // nothing like its own protocol and require them back verbatim.
    let addr = start_relay().await;
    let relay_addr = addr.clone();
    let identity = registration_identity();
    tokio::spawn(async move {
        let _ = serve_box(
            relay_addr,
            "box-raw".into(),
            identity,
            Arc::new(|mut stream| {
                Box::pin(async move {
                    let mut buf = vec![0u8; 256];
                    if let Ok(n) = stream.read(&mut buf).await {
                        let _ = stream.write_all(&buf[..n]).await;
                    }
                })
            }),
        )
        .await;
    });

    // Poll-connect until the box has registered.
    let mut client = loop {
        match connect_through_relay(&addr, "box-raw").await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    };

    let payload: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
    client.write_all(&payload).await.unwrap();
    let mut got = vec![0u8; payload.len()];
    client.read_exact(&mut got).await.unwrap();
    assert_eq!(got, payload);
}

#[tokio::test]
async fn serve_box_returns_ok_on_clean_control_close() {
    use lazybox_relay::protocol::{read_msg, registration_payload, write_msg};

    // A fake relay that accepts the registration, acks it, then closes the
    // control connection cleanly — the box should treat that as a normal
    // reconnect trigger (Ok), not an error.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let register: Hello = read_msg(&mut sock).await.unwrap();
        let Hello::RegisterBox {
            box_id,
            box_public_key,
        } = register
        else {
            panic!("expected box registration");
        };
        let challenge = RegistrationChallenge {
            nonce: uuid::Uuid::new_v4(),
        };
        write_msg(&mut sock, &challenge).await.unwrap();
        let proof: RegistrationProof = read_msg(&mut sock).await.unwrap();
        let identity_payload = registration_payload(&challenge, &box_id, &box_public_key);
        assert!(lazybox_identity::verify_base64(
            &box_public_key,
            &identity_payload,
            &proof.signature,
        ));
        write_msg(&mut sock, &Ack::Ok).await.unwrap();
        // Drop `sock` → clean EOF on the box's control read.
    });

    let result = serve_box(
        addr,
        "box-clean".into(),
        registration_identity(),
        Arc::new(|_stream| Box::pin(async {})),
    )
    .await;
    assert!(
        result.is_ok(),
        "a clean control-connection close must return Ok, got {result:?}",
    );
}

/// A distinctive plaintext marker the box writes *inside* the encrypted
/// channel. It must never appear verbatim in the bytes the relay carries.
const PLAINTEXT_MARKER: &[u8] = b"LAZYBOX-PLAINTEXT-MARKER-must-not-leak";

/// Register a box that terminates the E2E responder on each brokered
/// stream, then greets + echoes over the *encrypted* channel.
fn spawn_encrypted_echo_box(relay_addr: &str, box_id: &str, identity: Arc<Identity>) {
    let relay_addr = relay_addr.to_string();
    let box_id = box_id.to_string();
    let registration_identity = registration_identity();
    tokio::spawn(async move {
        let _ = serve_box(
            relay_addr,
            box_id,
            registration_identity,
            Arc::new(move |stream| {
                let identity = Arc::clone(&identity);
                Box::pin(async move {
                    let Ok((mut enc, _device)) = responder_handshake(stream, &identity).await
                    else {
                        return;
                    };
                    if enc.write_all(PLAINTEXT_MARKER).await.is_err() || enc.flush().await.is_err()
                    {
                        return;
                    }
                    let mut buf = [0u8; 64];
                    loop {
                        match enc.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if enc.write_all(&buf[..n]).await.is_err()
                                    || enc.flush().await.is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                })
            }),
        )
        .await;
    });
}

#[tokio::test]
async fn brokers_an_encrypted_client_to_box() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::new());
    tokio::spawn(Arc::clone(&relay).serve(listener));

    let box_identity = Arc::new(Identity::generate().unwrap());
    let box_pub = box_identity.public_key();
    spawn_encrypted_echo_box(&addr, "box-enc", box_identity);
    await_registration(&relay).await;

    // Record the raw bytes the relay hands the client, then run the Noise
    // handshake on top: everything recorded is what the relay carried.
    let raw = connect_through_relay(&addr, "box-enc").await.unwrap();
    let carried: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let device = Identity::generate().unwrap();
    let mut client = initiator_handshake(
        RecordingStream::new(raw, carried.clone()),
        &device,
        &box_pub,
    )
    .await
    .expect("client pins the box key and completes the handshake");

    let mut greeting = vec![0u8; PLAINTEXT_MARKER.len()];
    client.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting, PLAINTEXT_MARKER);

    client.write_all(b"ping").await.unwrap();
    client.flush().await.unwrap();
    let mut echo = [0u8; 4];
    client.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"ping");

    let carried = carried.lock().unwrap();
    assert!(!carried.is_empty(), "the relay did carry (encrypted) bytes");
    assert!(
        !contains_subslice(&carried, PLAINTEXT_MARKER),
        "the relay must never see the box's plaintext marker",
    );
}

#[tokio::test]
async fn wrong_box_key_fails_the_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::new());
    tokio::spawn(Arc::clone(&relay).serve(listener));

    let box_identity = Arc::new(Identity::generate().unwrap());
    spawn_encrypted_echo_box(&addr, "box-pinned", box_identity);
    await_registration(&relay).await;

    // Pin a key that belongs to nobody: the handshake must fail, so no
    // plaintext ever reaches the box behind the encrypted channel.
    let raw = connect_through_relay(&addr, "box-pinned").await.unwrap();
    let device = Identity::generate().unwrap();
    let impostor = Identity::generate().unwrap().public_key();
    let result = initiator_handshake(raw, &device, &impostor).await;
    assert!(
        result.is_err(),
        "a client pinning the wrong box key must not establish a channel",
    );
}

/// A gate that refuses every account — stands in for a lapsed
/// subscription. Proves the relay's payment-enforcement point rejects an
/// unentitled box before it can register or be brokered to.
struct DenyAll;

impl EntitlementGate for DenyAll {
    fn check<'a>(
        &'a self,
        _account: &'a AccountId,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Entitlement, EntitlementError>> + Send + 'a>>
    {
        Box::pin(async {
            Ok(Entitlement::Inactive {
                reason: "no active subscription".into(),
            })
        })
    }
}

#[tokio::test]
async fn unentitled_box_is_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::with_gate(Box::new(DenyAll)));
    tokio::spawn(Arc::clone(&relay).serve(listener));

    // The box dials out to register, but the gate denies it — so it never
    // enters the registry and `serve_box` returns.
    let relay_addr = addr.clone();
    let registered = tokio::spawn(async move {
        serve_box(
            relay_addr,
            "box-denied".into(),
            registration_identity(),
            Arc::new(|_stream| Box::pin(async {})),
        )
        .await
    });
    let error = registered
        .await
        .unwrap()
        .expect_err("a denied registration surfaces as an error to the box");
    assert!(matches!(error, RelayClientError::SubscriptionRequired));
    assert_eq!(error.to_string(), SUBSCRIPTION_REQUIRED_MESSAGE);
    assert_eq!(
        relay.registered_boxes().await,
        0,
        "an unentitled box must never enter the registry",
    );

    // And a client asking for it is told there is no such box.
    let err = connect_through_relay(&addr, "box-denied")
        .await
        .unwrap_err();
    assert!(matches!(err, RelayClientError::Unavailable { .. }));
}

struct ToggleGate {
    active: Arc<AtomicBool>,
}

impl EntitlementGate for ToggleGate {
    fn check<'a>(
        &'a self,
        _account: &'a AccountId,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Entitlement, EntitlementError>> + Send + 'a>>
    {
        Box::pin(async move {
            if self.active.load(Ordering::SeqCst) {
                Ok(Entitlement::Active)
            } else {
                Ok(Entitlement::Inactive {
                    reason: "no active subscription".into(),
                })
            }
        })
    }
}

#[tokio::test]
async fn newly_brokered_session_gets_typed_subscription_denial() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let active = Arc::new(AtomicBool::new(true));
    let relay = Arc::new(Relay::with_gate(Box::new(ToggleGate {
        active: Arc::clone(&active),
    })));
    tokio::spawn(Arc::clone(&relay).serve(listener));

    spawn_echo_box(&addr, "box-expired");
    await_registration(&relay).await;
    active.store(false, Ordering::SeqCst);

    let error = connect_through_relay(&addr, "box-expired")
        .await
        .unwrap_err();
    assert!(matches!(error, RelayClientError::SubscriptionRequired));
    assert_eq!(error.to_string(), SUBSCRIPTION_REQUIRED_MESSAGE);
}

#[tokio::test]
async fn copied_public_key_without_its_private_key_cannot_register() {
    use lazybox_relay::protocol::{read_msg, registration_payload, write_msg};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::new());
    tokio::spawn(Arc::clone(&relay).serve(listener));

    let claimed = registration_identity();
    let impostor = registration_identity();
    let box_id = "copied-entitlement";
    let box_public_key = claimed.public_key_base64();
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    write_msg(
        &mut stream,
        &Hello::RegisterBox {
            box_id: box_id.into(),
            box_public_key: box_public_key.clone(),
        },
    )
    .await
    .unwrap();
    let challenge: RegistrationChallenge = read_msg(&mut stream).await.unwrap();
    let signature = impostor
        .sign(&registration_payload(&challenge, box_id, &box_public_key))
        .to_bytes()
        .to_vec();
    write_msg(&mut stream, &RegistrationProof { signature })
        .await
        .unwrap();

    assert_eq!(
        read_msg::<_, Ack>(&mut stream).await.unwrap(),
        Ack::AuthenticationFailed
    );
    assert_eq!(relay.registered_boxes().await, 0);
}

#[tokio::test]
async fn registration_proof_cannot_be_replayed_for_a_new_challenge() {
    use lazybox_relay::protocol::{read_msg, registration_payload, write_msg};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::new());
    tokio::spawn(Arc::clone(&relay).serve(listener));

    let identity = registration_identity();
    let box_id = "replay-attempt";
    let box_public_key = identity.public_key_base64();
    let mut first = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let hello = Hello::RegisterBox {
        box_id: box_id.into(),
        box_public_key: box_public_key.clone(),
    };
    write_msg(&mut first, &hello).await.unwrap();
    let first_challenge: RegistrationChallenge = read_msg(&mut first).await.unwrap();
    let old_signature = identity
        .sign(&registration_payload(
            &first_challenge,
            box_id,
            &box_public_key,
        ))
        .to_bytes()
        .to_vec();
    drop(first);

    let mut replay = tokio::net::TcpStream::connect(&addr).await.unwrap();
    write_msg(&mut replay, &hello).await.unwrap();
    let second_challenge: RegistrationChallenge = read_msg(&mut replay).await.unwrap();
    assert_ne!(first_challenge, second_challenge);
    write_msg(
        &mut replay,
        &RegistrationProof {
            signature: old_signature,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        read_msg::<_, Ack>(&mut replay).await.unwrap(),
        Ack::AuthenticationFailed
    );
    assert_eq!(relay.registered_boxes().await, 0);
}

#[tokio::test]
async fn cached_active_box_survives_platform_outage_until_ttl() {
    let platform_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let platform_url = format!("http://{}", platform_listener.local_addr().unwrap());
    let (platform_shutdown, mut platform_shutdown_rx) = tokio::sync::oneshot::channel();
    let platform = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut platform_shutdown_rx => return,
                accepted = platform_listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    tokio::spawn(async move {
                        let service = service_fn(|_| async {
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .header(hyper::header::CONTENT_TYPE, "application/json")
                                    .header(hyper::header::CONNECTION, "close")
                                    .body(Full::new(Bytes::from_static(
                                        br#"{"active":true,"plan":"pro","reason":"active","checked_at":"2026-08-12T12:00:00Z"}"#,
                                    )))
                                    .unwrap(),
                            )
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::with_gate(Box::new(PlatformEntitlementGate::new(
        platform_url,
        "platform-secret",
    ))));
    tokio::spawn(Arc::clone(&relay).serve(listener));
    spawn_echo_box(&addr, "box-cached");
    await_registration(&relay).await;

    let _ = platform_shutdown.send(());
    platform.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(59)).await;
    tokio::time::resume();
    connect_through_relay(&addr, "box-cached")
        .await
        .expect("the cached active decision admits a new session within its TTL");

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    let error = connect_through_relay(&addr, "box-cached")
        .await
        .expect_err("an expired decision must be rechecked and fail closed");
    assert!(matches!(error, RelayClientError::SubscriptionRequired));
}

/// Wraps a stream and records every byte handed to the reader — the
/// ciphertext the relay carried to the client.
struct RecordingStream<S> {
    inner: S,
    seen: Arc<Mutex<Vec<u8>>>,
}

impl<S> RecordingStream<S> {
    fn new(inner: S, seen: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { inner, seen }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for RecordingStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let new = &buf.filled()[before..];
            if !new.is_empty() {
                self.seen.lock().unwrap().extend_from_slice(new);
            }
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for RecordingStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn two_clients_share_one_box() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let relay = Arc::new(Relay::new());
    tokio::spawn(Arc::clone(&relay).serve(listener));

    spawn_echo_box(&addr, "box-shared");
    await_registration(&relay).await;

    for tag in [b"aaaa", b"bbbb"] {
        let mut client = connect_through_relay(&addr, "box-shared").await.unwrap();
        let mut greeting = [0u8; 14];
        client.read_exact(&mut greeting).await.unwrap();
        client.write_all(tag).await.unwrap();
        let mut echo = [0u8; 4];
        client.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, tag);
    }
}
