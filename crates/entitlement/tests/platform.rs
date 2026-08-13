use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use lazybox_entitlement::{
    AccountId, Entitlement, EntitlementError, EntitlementGate, PlatformEntitlementGate,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

#[derive(Clone)]
enum Reply {
    Json(&'static str),
    Status(StatusCode),
    Hang,
}

#[derive(Debug)]
struct SeenRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: serde_json::Value,
}

struct MockPlatform {
    addr: SocketAddr,
    reply: Arc<Mutex<Reply>>,
    requests: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockPlatform {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn set_reply(&self, reply: Reply) {
        *self.reply.lock().unwrap() = reply;
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.await.unwrap();
    }
}

async fn spawn_mock(reply: Reply) -> MockPlatform {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reply = Arc::new(Mutex::new(reply));
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    let reply_for_task = Arc::clone(&reply);
    let requests_for_task = Arc::clone(&requests);
    let seen_for_task = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => return,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let reply = Arc::clone(&reply_for_task);
                    let requests = Arc::clone(&requests_for_task);
                    let seen = Arc::clone(&seen_for_task);
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                            let reply = Arc::clone(&reply);
                            let requests = Arc::clone(&requests);
                            let seen = Arc::clone(&seen);
                            async move {
                                requests.fetch_add(1, Ordering::SeqCst);
                                let method = request.method().to_string();
                                let path = request.uri().path().to_string();
                                let authorization = request
                                    .headers()
                                    .get(hyper::header::AUTHORIZATION)
                                    .and_then(|value| value.to_str().ok())
                                    .map(str::to_string);
                                let body = request
                                    .into_body()
                                    .collect()
                                    .await
                                    .unwrap()
                                    .to_bytes();
                                seen.lock().unwrap().push(SeenRequest {
                                    method,
                                    path,
                                    authorization,
                                    body: serde_json::from_slice(&body).unwrap(),
                                });
                                let selected = reply.lock().unwrap().clone();
                                let response = match selected {
                                    Reply::Json(body) => Response::builder()
                                        .status(StatusCode::OK)
                                        .header(hyper::header::CONTENT_TYPE, "application/json")
                                        .header(hyper::header::CONNECTION, "close")
                                        .body(Full::new(Bytes::from(body)))
                                        .unwrap(),
                                    Reply::Status(status) => Response::builder()
                                        .status(status)
                                        .header(hyper::header::CONNECTION, "close")
                                        .body(Full::new(Bytes::new()))
                                        .unwrap(),
                                    Reply::Hang => {
                                        std::future::pending::<()>().await;
                                        unreachable!()
                                    }
                                };
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });

    MockPlatform {
        addr,
        reply,
        requests,
        seen,
        shutdown: Some(shutdown_tx),
        task,
    }
}

const ACTIVE: &str = r#"{
    "active": true,
    "plan": "pro",
    "reason": "subscription active",
    "checked_at": "2026-08-12T12:00:00Z"
}"#;

const INACTIVE: &str = r#"{
    "active": false,
    "plan": "free",
    "reason": "no active subscription",
    "checked_at": "2026-08-12T12:00:00Z"
}"#;

#[tokio::test]
async fn active_response_admits_and_sends_the_platform_contract() {
    let mock = spawn_mock(Reply::Json(ACTIVE)).await;
    let gate = PlatformEntitlementGate::new(mock.url(), "platform-secret");

    let result = gate.check(&AccountId::new("Ym94LWtleQ==")).await.unwrap();

    assert_eq!(result, Entitlement::Active);
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/relay/entitlements/check");
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer platform-secret")
    );
    assert_eq!(
        seen[0].body,
        serde_json::json!({ "box_public_key": "Ym94LWtleQ==" })
    );
}

#[tokio::test]
async fn inactive_response_refuses() {
    let mock = spawn_mock(Reply::Json(INACTIVE)).await;
    let gate = PlatformEntitlementGate::new(mock.url(), "key");

    assert_eq!(
        gate.check(&AccountId::new("box-key")).await.unwrap(),
        Entitlement::Inactive {
            reason: "no active subscription".into()
        }
    );
}

#[tokio::test]
async fn server_error_and_malformed_json_fail_closed() {
    for reply in [
        Reply::Status(StatusCode::INTERNAL_SERVER_ERROR),
        Reply::Json("not json"),
    ] {
        let mock = spawn_mock(reply).await;
        let gate = PlatformEntitlementGate::new(mock.url(), "key");

        assert!(matches!(
            gate.check(&AccountId::new("box-key")).await,
            Err(EntitlementError::Lookup(_))
        ));
    }
}

#[tokio::test]
async fn timeout_fails_closed_within_the_two_second_bound() {
    let mock = spawn_mock(Reply::Hang).await;
    let gate = PlatformEntitlementGate::new(mock.url(), "key");

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        gate.check(&AccountId::new("box-key")),
    )
    .await
    .expect("the gate must enforce its two-second timeout");
    assert!(matches!(result, Err(EntitlementError::Lookup(_))));
}

#[tokio::test]
async fn active_cache_survives_an_outage_until_sixty_seconds() {
    let mock = spawn_mock(Reply::Json(ACTIVE)).await;
    let gate = PlatformEntitlementGate::new(mock.url(), "key");
    let account = AccountId::new("box-key");

    assert_eq!(gate.check(&account).await.unwrap(), Entitlement::Active);
    assert_eq!(mock.request_count(), 1);
    tokio::time::pause();
    mock.shutdown().await;

    tokio::time::advance(Duration::from_secs(59)).await;
    assert_eq!(gate.check(&account).await.unwrap(), Entitlement::Active);

    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    assert!(matches!(
        gate.check(&account).await,
        Err(EntitlementError::Lookup(_))
    ));
}

#[tokio::test]
async fn inactive_cache_refreshes_after_fifteen_seconds() {
    let mock = spawn_mock(Reply::Json(INACTIVE)).await;
    let gate = PlatformEntitlementGate::new(mock.url(), "key");
    let account = AccountId::new("box-key");

    assert!(matches!(
        gate.check(&account).await.unwrap(),
        Entitlement::Inactive { .. }
    ));
    mock.set_reply(Reply::Json(ACTIVE));
    tokio::time::pause();

    tokio::time::advance(Duration::from_secs(14)).await;
    assert!(matches!(
        gate.check(&account).await.unwrap(),
        Entitlement::Inactive { .. }
    ));
    assert_eq!(mock.request_count(), 1);

    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    assert_eq!(gate.check(&account).await.unwrap(), Entitlement::Active);
    assert_eq!(mock.request_count(), 2);
}
