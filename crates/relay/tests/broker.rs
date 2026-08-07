//! End-to-end brokering over a loopback relay: a box dials out and
//! registers, a client connects by box-id, and bytes flow both ways
//! through the relay — which only ever splices opaque bytes.

use std::sync::Arc;

use lazybox_relay::{Relay, connect_through_relay, serve_box};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
    tokio::spawn(async move {
        let _ = serve_box(
            relay_addr,
            box_id,
            "test-account".into(),
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
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[tokio::test]
async fn forwards_arbitrary_bytes_untouched() {
    // The relay must not interpret the payload — feed it bytes that look
    // nothing like its own protocol and require them back verbatim.
    let addr = start_relay().await;
    let relay_addr = addr.clone();
    tokio::spawn(async move {
        let _ = serve_box(
            relay_addr,
            "box-raw".into(),
            "acct".into(),
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
    use lazybox_relay::protocol::{Ack, Hello, read_msg, write_msg};

    // A fake relay that accepts the registration, acks it, then closes the
    // control connection cleanly — the box should treat that as a normal
    // reconnect trigger (Ok), not an error.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let _register: Hello = read_msg(&mut sock).await.unwrap();
        write_msg(&mut sock, &Ack::Ok).await.unwrap();
        // Drop `sock` → clean EOF on the box's control read.
    });

    let result = serve_box(
        addr,
        "box-clean".into(),
        "acct".into(),
        Arc::new(|_stream| Box::pin(async {})),
    )
    .await;
    assert!(
        result.is_ok(),
        "a clean control-connection close must return Ok, got {result:?}",
    );
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
