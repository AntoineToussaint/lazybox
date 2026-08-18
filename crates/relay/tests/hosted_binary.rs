use std::convert::Infallible;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use lazybox_identity::BoxIdentity;
use lazybox_relay::{RelayClientError, serve_box};
use tokio::net::{TcpListener, TcpStream};

struct RelayProcess(Child);

impl Drop for RelayProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn spawn_inactive_platform() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_task = Arc::clone(&requests);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let requests = Arc::clone(&requests_for_task);
            tokio::spawn(async move {
                let service = service_fn(move |_| {
                    requests.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .header(hyper::header::CONTENT_TYPE, "application/json")
                                .body(Full::new(Bytes::from_static(
                                    br#"{
                                        "active": false,
                                        "plan": "free",
                                        "reason": "no active subscription",
                                        "checked_at": "2026-08-12T12:00:00Z"
                                    }"#,
                                )))
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (format!("http://{addr}"), requests)
}

fn unused_loopback_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

#[tokio::test]
async fn environment_only_platform_config_enforces_entitlements() {
    let (platform_url, platform_requests) = spawn_inactive_platform().await;
    let relay_addr = unused_loopback_addr();
    let home = tempfile::tempdir().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_lazybox-relay"))
        .env("LAZYBOX_HOME", home.path())
        .env("LAZYBOX_RELAY_LISTEN_ADDR", &relay_addr)
        .env("LAZYBOX_PLATFORM_URL", platform_url)
        .env("LAZYBOX_PLATFORM_API_KEY", "platform-secret")
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut relay = RelayProcess(child);

    // The full workspace test fans out several process-heavy suites at once;
    // a one-second polling budget made an otherwise healthy relay fail on a
    // loaded CI host. Keep checking child liveness, but give startup a real
    // bounded deadline.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while tokio::time::Instant::now() < deadline {
        if TcpStream::connect(&relay_addr).await.is_ok() {
            ready = true;
            break;
        }
        if let Some(status) = relay.0.try_wait().unwrap() {
            panic!("hosted relay exited before listening: {status}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready, "hosted relay did not start listening within 10s");

    let error = serve_box(
        relay_addr,
        "box-inactive".into(),
        Arc::new(BoxIdentity::load_or_generate(home.path()).unwrap()),
        Arc::new(|_| Box::pin(async {})),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, RelayClientError::SubscriptionRequired));
    assert_eq!(platform_requests.load(Ordering::SeqCst), 1);
}
