//! End-to-end tests for the proxy: spawn a mock upstream, spawn the
//! proxy pointed at it, send requests through the proxy, assert the
//! response passed through intact AND a `ProxyRecord` landed in the
//! records channel.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use lazybox_llm_proxy::{ApiProvider, ProxyConfig, ProxyServer, SESSION_TAG_HEADER};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// ── Mock upstream ──────────────────────────────────────────────────────

struct UpstreamHandle {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    received: Arc<AtomicUsize>,
}

impl UpstreamHandle {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn a minimal hyper server. Every request gets a 200 response
/// whose body echoes `hello from <path>`. The `received` counter
/// tracks how many requests reached it — tests assert forwarding.
async fn spawn_upstream() -> UpstreamHandle {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let received = Arc::new(AtomicUsize::new(0));
    let received_clone = received.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => return,
                accept = listener.accept() => {
                    let Ok((stream, _)) = accept else { continue };
                    let received = received_clone.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let received = received.clone();
                            async move {
                                received.fetch_add(1, Ordering::SeqCst);
                                let path = req.uri().path().to_string();
                                let _ = req.into_body().collect().await; // drain
                                let body = format!("hello from {path}");
                                Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .status(StatusCode::OK)
                                        .body(Full::new(Bytes::from(body)))
                                        .unwrap()
                                )
                            }
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(io, service)
                            .await;
                    });
                }
            }
        }
    });

    UpstreamHandle {
        addr,
        shutdown: Some(shutdown_tx),
        received,
    }
}

/// Helper: one-shot request through a proxy at `proxy_url`.
async fn request(
    proxy_url: &str,
    path: &str,
    session_tag: Option<&str>,
) -> Result<reqwest::Response, reqwest::Error> {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{proxy_url}{path}"));
    if let Some(tag) = session_tag {
        req = req.header(SESSION_TAG_HEADER, tag);
    }
    req.body(r#"{"model":"claude-sonnet-4-6"}"#).send().await
}

// ── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn proxy_forwards_request_and_returns_upstream_body() {
    let upstream = spawn_upstream().await;

    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ProxyConfig::default()
    };
    let (proxy, mut records) = ProxyServer::start(cfg, upstream.url()).await.unwrap();
    let proxy_url = format!("http://{}", proxy.addr());

    let resp = request(&proxy_url, "/v1/messages", Some("s-abc"))
        .await
        .expect("roundtrip");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("hello from /v1/messages"), "got: {body}");

    assert_eq!(
        upstream.received.load(Ordering::SeqCst),
        1,
        "upstream saw exactly one request"
    );

    // Record arrived with the session tag attribution.
    let record = tokio::time::timeout(Duration::from_secs(2), records.recv())
        .await
        .expect("timeout waiting for record")
        .expect("record");
    assert_eq!(record.session_key.as_str(), "s-abc");
    assert_eq!(record.endpoint, "/v1/messages");
    assert_eq!(record.status, 200);
    assert_eq!(record.provider, ApiProvider::Anthropic);
    assert!(record.duration > Duration::ZERO);
    assert!(record.error.is_none());

    proxy.shutdown().await;
    upstream.shutdown().await;
}

#[tokio::test]
async fn proxy_classifies_openai_endpoint() {
    let upstream = spawn_upstream().await;
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ProxyConfig::default()
    };
    let (proxy, mut records) = ProxyServer::start(cfg, upstream.url()).await.unwrap();
    let proxy_url = format!("http://{}", proxy.addr());

    let resp = request(&proxy_url, "/v1/chat/completions", Some("s-x"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let record = records.recv().await.unwrap();
    assert_eq!(record.provider, ApiProvider::OpenAI);

    proxy.shutdown().await;
    upstream.shutdown().await;
}

#[tokio::test]
async fn proxy_missing_session_tag_uses_unknown() {
    let upstream = spawn_upstream().await;
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ProxyConfig::default()
    };
    let (proxy, mut records) = ProxyServer::start(cfg, upstream.url()).await.unwrap();
    let proxy_url = format!("http://{}", proxy.addr());

    let _ = request(&proxy_url, "/v1/messages", None).await.unwrap();
    let record = records.recv().await.unwrap();
    assert_eq!(record.session_key.as_str(), "unknown");

    proxy.shutdown().await;
    upstream.shutdown().await;
}

#[tokio::test]
async fn proxy_emits_record_on_upstream_failure() {
    // Upstream never comes up — proxy points at a port nothing
    // listens on. We should get 502 back AND a record with error set.
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ProxyConfig::default()
    };
    // Bind-and-drop trick to pick a definitely-free port.
    let sink = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = sink.local_addr().unwrap().port();
    drop(sink);

    let (proxy, mut records) = ProxyServer::start(cfg, format!("http://127.0.0.1:{dead_port}"))
        .await
        .unwrap();
    let proxy_url = format!("http://{}", proxy.addr());

    let resp = request(&proxy_url, "/v1/messages", Some("s-dead"))
        .await
        .expect("proxy responds even on upstream failure");
    assert_eq!(resp.status(), 502, "upstream error surfaces as 502");

    let record = tokio::time::timeout(Duration::from_secs(2), records.recv())
        .await
        .expect("timeout")
        .expect("record");
    assert_eq!(record.status, 502);
    assert!(record.error.is_some(), "error string attached");

    proxy.shutdown().await;
}

#[tokio::test]
async fn proxy_multiple_concurrent_requests() {
    let upstream = spawn_upstream().await;
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ProxyConfig::default()
    };
    let (proxy, mut records) = ProxyServer::start(cfg, upstream.url()).await.unwrap();
    let proxy_url = format!("http://{}", proxy.addr());

    let mut handles = Vec::new();
    for i in 0..8 {
        let url = proxy_url.clone();
        let tag = format!("s-{i}");
        handles.push(tokio::spawn(async move {
            request(&url, "/v1/messages", Some(&tag)).await.unwrap()
        }));
    }
    for h in handles {
        let r = h.await.unwrap();
        assert_eq!(r.status(), 200);
    }
    // Drain all 8 records.
    let mut collected = Vec::new();
    for _ in 0..8 {
        let r = tokio::time::timeout(Duration::from_secs(2), records.recv())
            .await
            .unwrap()
            .unwrap();
        collected.push(r);
    }
    assert_eq!(collected.len(), 8);
    assert_eq!(upstream.received.load(Ordering::SeqCst), 8);

    proxy.shutdown().await;
    upstream.shutdown().await;
}

#[tokio::test]
async fn proxy_drop_also_shuts_down() {
    let upstream = spawn_upstream().await;
    let cfg = ProxyConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..ProxyConfig::default()
    };
    let (proxy, _records) = ProxyServer::start(cfg, upstream.url()).await.unwrap();
    let addr = proxy.addr();
    drop(proxy);
    // Give the task a chance to notice the shutdown.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // New connection should fail (refused) — accept loop is gone.
    let res = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(addr),
    )
    .await;
    // We accept either a timeout-after-refused or an explicit refused.
    // Both are evidence the listener is no longer accepting.
    let connected = matches!(res, Ok(Ok(_)));
    assert!(!connected, "proxy should not accept after drop");

    upstream.shutdown().await;
}
