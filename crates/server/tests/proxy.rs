//! End-to-end coverage for the metering reverse-proxy (#1062, #1109):
//! a request routed through the proxy reaches the upstream unchanged, its
//! streamed response comes back byte-for-byte, and the token usage is
//! parsed and attributed to the agent named in the request path.

use std::sync::{Arc, Mutex};

use lazybox_ipc::AgentUsage;
use lazybox_server::proxy::{self, Upstreams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A one-shot upstream that ignores the request and replies with a fixed
/// HTTP/1.1 response. `Connection: close` + `Content-Length` let the
/// proxy's client read the body without chunked framing.
async fn mock_upstream(body: &'static str) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut scratch = [0u8; 4096];
            // Drain the request head so the client's write completes.
            let _ = stream.read(&mut scratch).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    format!("http://{addr}")
}

async fn start_proxy(upstream: String, sink: proxy::UsageSink) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind proxy");
    let port = listener.local_addr().expect("proxy addr").port();
    let upstreams = Upstreams {
        anthropic: upstream.clone(),
        openai: upstream,
    };
    let quota_sink: proxy::QuotaSink = std::sync::Arc::new(|_, _, _| {});
    let prices = std::sync::Arc::new(std::collections::BTreeMap::new());
    tokio::spawn(proxy::serve(listener, upstreams, sink, quota_sink, prices));
    port
}

#[tokio::test]
async fn proxy_forwards_and_captures_usage() {
    // A minimal Anthropic-style SSE turn: input/cache in `message_start`,
    // the final cumulative output in `message_delta`.
    let body = "event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":300,\"output_tokens\":1}}}\n\n\
        event: message_delta\n\
        data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":88}}\n\n\
        data: [DONE]\n\n";

    let captured: Arc<Mutex<Vec<(String, String, AgentUsage)>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = captured.clone();
    let sink: proxy::UsageSink = Arc::new(move |agent_id: &str, session: &str, usage| {
        recorder
            .lock()
            .expect("lock")
            .push((agent_id.to_string(), session.to_string(), usage));
    });

    let upstream = mock_upstream(body).await;
    let port = start_proxy(upstream, sink).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/anthropic/claude/github-acme-widget-7/v1/messages"
        ))
        .header("authorization", "Bearer test-secret")
        .body("{\"model\":\"claude\"}")
        .send()
        .await
        .expect("proxy request");
    assert!(response.status().is_success());
    let returned = response.text().await.expect("body");

    // The response streamed back byte-for-byte.
    assert_eq!(returned, body);

    // Usage was parsed and attributed to the agent AND session in the path.
    let captured = captured.lock().expect("lock");
    assert_eq!(captured.len(), 1, "one metered response");
    let (agent, session, usage) = &captured[0];
    assert_eq!(agent, "claude");
    assert_eq!(session, "github-acme-widget-7");
    assert_eq!(usage.input_tokens, Some(1200));
    assert_eq!(usage.cache_read_input_tokens, Some(300));
    assert_eq!(usage.output_tokens, Some(88));
}

#[tokio::test]
async fn proxy_rejects_a_pathless_request_without_metering() {
    let captured: Arc<Mutex<Vec<(String, String, AgentUsage)>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = captured.clone();
    let sink: proxy::UsageSink = Arc::new(move |agent_id: &str, session: &str, usage| {
        recorder
            .lock()
            .expect("lock")
            .push((agent_id.to_string(), session.to_string(), usage));
    });

    // Upstream never gets hit — the request lacks the `/provider/agent`
    // prefix, so the proxy 404s before forwarding.
    let port = start_proxy("http://127.0.0.1:1".to_string(), sink).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("proxy request");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(captured.lock().expect("lock").is_empty());
}
