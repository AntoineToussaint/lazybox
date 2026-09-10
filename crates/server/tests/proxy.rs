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

/// Like [`mock_upstream`], but also hands back the raw request head the
/// upstream received, so a test can assert exactly what crossed the proxy:
/// path + query, and which headers survived / were stripped.
async fn mock_upstream_capturing(body: &'static str) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let recorder = seen.clone();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut scratch = [0u8; 8192];
            let n = stream.read(&mut scratch).await.unwrap_or(0);
            *recorder.lock().expect("lock") = String::from_utf8_lossy(&scratch[..n]).into_owned();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    (format!("http://{addr}"), seen)
}

/// `(agent_id, session, usage)` triples the sink observed, newest last.
type Captured = Arc<Mutex<Vec<(String, String, AgentUsage)>>>;

fn recording_sink() -> (Captured, proxy::UsageSink) {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let recorder = captured.clone();
    let sink: proxy::UsageSink = Arc::new(move |agent_id: &str, session: &str, usage| {
        recorder
            .lock()
            .expect("lock")
            .push((agent_id.to_string(), session.to_string(), usage));
    });
    (captured, sink)
}

async fn start_proxy(upstream: String, sink: proxy::UsageSink) -> u16 {
    start_proxy_with(upstream, sink, Arc::new(proxy::Compactor::disabled())).await
}

/// Like [`start_proxy`], but with a context-compaction pass configured —
/// the request-body rewrite (#1609).
async fn start_proxy_with(
    upstream: String,
    sink: proxy::UsageSink,
    compactor: Arc<proxy::Compactor>,
) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind proxy");
    let port = listener.local_addr().expect("proxy addr").port();
    let upstreams = Upstreams {
        anthropic: upstream.clone(),
        openai: upstream.clone(),
        chatgpt: upstream,
    };
    let quota_sink: proxy::QuotaSink = std::sync::Arc::new(|_, _, _| {});
    let prices = std::sync::Arc::new(std::collections::BTreeMap::new());
    // The 350-line floor the instrumentation uses now comes from the
    // compactor's own policy, so it cannot drift from the set that gets
    // rewritten — `Compactor::disabled()` carries the default 350.
    tokio::spawn(proxy::serve(
        listener, upstreams, sink, quota_sink, prices, compactor,
    ));
    port
}

/// Like [`mock_upstream`] but serves every connection, so a test can send
/// more than one request through the proxy.
async fn mock_upstream_repeating(body: &'static str) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut scratch = [0u8; 65536];
                let _ = stream.read(&mut scratch).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    format!("http://{addr}")
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

    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
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

/// The proxy knows NO endpoints — it forwards whatever the agent CLI
/// appends to the injected base URL, verbatim. So an endpoint lazybox has
/// never heard of (here `count_tokens` under a beta query) reaches the
/// upstream with its exact path, query string, method, and the API
/// headers the feature depends on (`anthropic-beta`, `anthropic-version`,
/// auth) intact. Only hop-by-hop headers and `accept-encoding` are
/// stripped (the latter so the usage tee sees identity bytes). This is the
/// guarantee that turning metering on can't break a new API surface —
/// server-side compaction, batches, files, whatever ships next.
#[tokio::test]
async fn proxy_forwards_unknown_endpoints_and_beta_headers_verbatim() {
    let (captured, sink) = recording_sink();
    // A `count_tokens` reply: top-level `input_tokens`, no `usage` object —
    // must not be mistaken for a metered turn.
    let (upstream, seen) = mock_upstream_capturing("{\"input_tokens\":2095}").await;
    let port = start_proxy(upstream, sink).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/anthropic/claude/github-o-r-42/v1/messages/count_tokens?beta=true"
        ))
        .header("x-api-key", "sk-test")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "compact-2026-01-12,token-counting-2024-11-01")
        .header("accept-encoding", "gzip, br")
        .body("{\"model\":\"claude-opus-5\",\"messages\":[]}")
        .send()
        .await
        .expect("proxy request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body"),
        "{\"input_tokens\":2095}"
    );

    let head = seen.lock().expect("lock").clone();
    let request_line = head.lines().next().unwrap_or_default().to_string();
    assert_eq!(
        request_line, "POST /v1/messages/count_tokens?beta=true HTTP/1.1",
        "path + query forwarded verbatim, metering prefix stripped: {head:?}"
    );
    let lower = head.to_ascii_lowercase();
    assert!(
        lower.contains("x-api-key: sk-test"),
        "auth survives: {head:?}"
    );
    assert!(lower.contains("anthropic-version: 2023-06-01"), "{head:?}");
    assert!(
        lower.contains("anthropic-beta: compact-2026-01-12,token-counting-2024-11-01"),
        "beta opt-ins survive untouched: {head:?}"
    );
    assert!(
        !lower.contains("accept-encoding"),
        "accept-encoding is stripped so the usage tee reads identity bytes: {head:?}"
    );
    assert!(
        lower.contains("{\"model\":\"claude-opus-5\",\"messages\":[]}"),
        "request body forwarded verbatim: {head:?}"
    );

    // No `usage` object in the reply → nothing metered, nothing invented.
    assert!(captured.lock().expect("lock").is_empty());
}

/// A server-side-compaction response (beta `compact-2026-01-12`) streams
/// back byte-identical — `compaction` content blocks and all — because the
/// tee only *reads* the stream. The client must replay those blocks on the
/// next turn, so any rewrite here would silently break compaction. The
/// nested per-iteration `usage` folds as a high-water mark under the
/// top-level total, so the turn is priced once, from the total.
#[tokio::test]
async fn proxy_streams_a_compaction_response_byte_identical() {
    let body = "{\"id\":\"msg_1\",\"type\":\"message\",\"model\":\"claude-opus-5\",\
        \"content\":[{\"type\":\"compaction\",\"content\":\"<summary of earlier context>\"},\
        {\"type\":\"text\",\"text\":\"Continuing from the summary.\"}],\
        \"stop_reason\":\"end_turn\",\
        \"iterations\":[{\"usage\":{\"input_tokens\":150000,\"output_tokens\":900}},\
        {\"usage\":{\"input_tokens\":4000,\"output_tokens\":120}}],\
        \"usage\":{\"input_tokens\":154000,\"output_tokens\":1020}}";
    let (captured, sink) = recording_sink();
    let (upstream, _seen) = mock_upstream_capturing(body).await;
    let port = start_proxy(upstream, sink).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/anthropic/claude/github-o-r-42/v1/messages"
        ))
        .header("anthropic-beta", "compact-2026-01-12")
        .body("{}")
        .send()
        .await
        .expect("proxy request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body"),
        body,
        "compaction blocks must reach the client untouched"
    );

    let captured = captured.lock().expect("lock");
    assert_eq!(captured.len(), 1, "priced exactly once");
    let (_, _, usage) = &captured[0];
    assert_eq!(
        usage.input_tokens,
        Some(154_000),
        "the top-level total wins"
    );
    assert_eq!(usage.output_tokens, Some(1_020));
    assert!(usage.cost_usd_micros.is_some(), "known model → priced");
}

/// The one thing that does NOT fall through: the proxy is the agent's only
/// route (its `*_BASE_URL` points here), so an unreachable upstream comes
/// back as a clean `502` the CLI can retry — never a hang, never a silent
/// success, and nothing metered. There is no "bypass to the vendor" path
/// by design; that would defeat metering and hide the outage.
#[tokio::test]
async fn proxy_returns_502_when_the_upstream_is_unreachable() {
    let (captured, sink) = recording_sink();
    // Port 1 on loopback: refused immediately.
    let port = start_proxy("http://127.0.0.1:1".to_string(), sink).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/anthropic/claude/github-o-r-42/v1/messages"
        ))
        .body("{}")
        .send()
        .await
        .expect("proxy answers even when upstream doesn't");
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert!(captured.lock().expect("lock").is_empty(), "nothing metered");
}

#[tokio::test]
async fn proxy_rejects_a_pathless_request_without_metering() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
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

/// The re-send meter, end to end (#1606): the accounting is read off the
/// *request* body and rides the response's usage event, and the same
/// tool_result block is fresh the first time a session sends it and a
/// mechanical re-send every time after.
#[tokio::test]
async fn proxy_meters_the_tool_result_share_and_flags_a_re_send() {
    let response_body = "event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1200,\"output_tokens\":1}}}\n\n\
        data: [DONE]\n\n";
    let (captured, sink) = recording_sink();
    let upstream = mock_upstream_repeating(response_body).await;
    let port = start_proxy(upstream, sink).await;

    let request_body = serde_json::json!({
        "model": "claude-opus-5",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "run the tests"}]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "a".repeat(4096),
            }]},
        ]
    })
    .to_string();

    let client = reqwest::Client::new();
    for _ in 0..2 {
        let response = client
            .post(format!(
                "http://127.0.0.1:{port}/anthropic/claude/github-o-r-42/v1/messages"
            ))
            .body(request_body.clone())
            .send()
            .await
            .expect("proxy request");
        assert!(response.status().is_success());
        // Drain the body so the tee finishes and the sink fires.
        let _ = response.text().await.expect("body");
    }

    let captured = captured.lock().expect("lock");
    assert_eq!(captured.len(), 2, "one metered response per request");

    let first = captured[0].2.context.expect("first request measured");
    assert!(
        first.tool_result_bytes > 4_000 && first.message_bytes > first.tool_result_bytes,
        "the tool_result block is a share of the payload: {first:?}"
    );
    assert_eq!(
        first.tool_result_resent_bytes, 0,
        "the block's first send is not a re-send"
    );

    let second = captured[1].2.context.expect("second request measured");
    assert_eq!(
        second.tool_result_resent_bytes, second.tool_result_bytes,
        "the identical block is wholly a re-send the second time"
    );
    assert_eq!(second.resent_share_pct(), Some(100));
}

/// A session that never sends tool output measures at 0% — distinct from
/// an unmeasured (unproxied, or unrecognized-shape) request, which carries
/// no accounting at all.
#[tokio::test]
async fn a_request_without_a_conversation_array_carries_no_accounting() {
    let response_body = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n";
    let (captured, sink) = recording_sink();
    let upstream = mock_upstream(response_body).await;
    let port = start_proxy(upstream, sink).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/anthropic/claude/github-o-r-42/v1/messages"
        ))
        .body("{\"model\":\"claude-opus-5\"}")
        .send()
        .await
        .expect("proxy request");
    let _ = response.text().await.expect("body");

    let captured = captured.lock().expect("lock");
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].2.context.is_none(),
        "no conversation array → nothing measured, not a measured zero"
    );
}

/// An upstream that answers each successive connection with the next body in
/// `bodies`, so one proxy — one seen-set — can serve a sequence of differently
/// shaped responses.
async fn mock_upstream_sequence(bodies: Vec<&'static str>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    tokio::spawn(async move {
        let mut bodies = bodies.into_iter();
        while let Ok((mut stream, _)) = listener.accept().await {
            let Some(body) = bodies.next() else { break };
            tokio::spawn(async move {
                let mut scratch = [0u8; 65536];
                let _ = stream.read(&mut scratch).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    format!("http://{addr}")
}

/// The re-send meter must not be poisoned by requests that are never billed
/// (#1606). Claude Code preflights `count_tokens` with the whole transcript,
/// and retries the same body verbatim after a 429 — neither reports usage. If
/// those marked their blocks as sent, the *first* real send of every block
/// would be reported as a mechanical re-send and the headline ratio would read
/// ~100% for every session.
#[tokio::test]
async fn an_unbilled_request_does_not_consume_a_blocks_first_send() {
    let (captured, sink) = recording_sink();
    let upstream = mock_upstream_sequence(vec![
        // A `count_tokens` preflight: no `usage` object, nothing metered.
        "{\"input_tokens\":2095}",
        // A 429 the agent will retry: also no usage.
        "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}",
        // The real turn, carrying the same transcript.
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1200,\"output_tokens\":1}}}\n\n",
    ])
    .await;
    let port = start_proxy(upstream, sink).await;

    let body = serde_json::json!({
        "model": "claude-opus-5",
        "messages": [{"role": "user", "content": [{
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": "a".repeat(4096),
        }]}]
    })
    .to_string();

    let client = reqwest::Client::new();
    for path in ["/v1/messages/count_tokens", "/v1/messages", "/v1/messages"] {
        let response = client
            .post(format!(
                "http://127.0.0.1:{port}/anthropic/claude/github-o-r-42{path}"
            ))
            .body(body.clone())
            .send()
            .await
            .expect("proxy request");
        let _ = response.text().await.expect("body");
    }

    let captured = captured.lock().expect("lock");
    assert_eq!(captured.len(), 1, "only the billed turn reports usage");
    let context = captured[0].2.context.expect("the billed turn was measured");
    assert_eq!(
        context.tool_result_resent_bytes, 0,
        "an unbilled preflight/retry must not consume the block's first send",
    );
}

/// An upstream that reads the WHOLE request — head plus `Content-Length`
/// — before replying, and keeps serving, recording every request it saw.
/// [`mock_upstream_capturing`] reads one buffer of one connection, which
/// truncates a real conversation and cannot see a second turn.
async fn mock_upstream_recording(body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let recorder = recorder.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut scratch = [0u8; 16 * 1024];
                loop {
                    let read = stream.read(&mut scratch).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&scratch[..read]);
                    let text = String::from_utf8_lossy(&request);
                    let Some(head_end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let expected: usize = text[..head_end]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + expected {
                        break;
                    }
                }
                recorder
                    .lock()
                    .expect("lock")
                    .push(String::from_utf8_lossy(&request).into_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

/// An Anthropic request with `results` Read tool calls and their results,
/// each result far over the 350-line floor.
fn conversation_body(results: usize) -> String {
    let big = |index: usize| {
        (0..400)
            .map(|line| format!("block {index} line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut messages = Vec::new();
    for index in 0..results {
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": format!("call_{index}"),
                "name": "Read",
                "input": {"file_path": format!("src/file_{index}.rs")},
            }],
        }));
        messages.push(serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": format!("call_{index}"),
                "content": big(index),
            }],
        }));
    }
    serde_json::json!({"model": "claude-opus-5", "messages": messages}).to_string()
}

fn compactor(mode: lazybox_core::CompactionMode) -> Arc<proxy::Compactor> {
    Arc::new(proxy::Compactor::new(
        lazybox_core::ContextHygiene {
            mode,
            ..lazybox_core::ContextHygiene::default()
        },
        Arc::new(std::collections::BTreeMap::new()),
        Arc::new(|_, _| {}),
    ))
}

fn upstream_body(request: &str) -> &str {
    request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default()
}

/// A turn whose usage says most of the prompt came from cache — what a
/// healthy session reports, and what the kill switch takes as its baseline.
const WARM_CACHE_TURN: &str = "{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":900,\"output_tokens\":20}}";

async fn post_conversation(port: u16, session: &str, body: String) {
    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/anthropic/claude/{session}/v1/messages"
        ))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("proxy request");
    assert!(response.status().is_success());
    // Drain the body so the usage tee reaches `finish` before the next turn.
    let _ = response.text().await.expect("body");
}

/// The enforcement point (#1609): with compaction `on`, the body the
/// upstream receives has its old tool results condensed — while the
/// recency window crosses verbatim, so the model keeps the full content of
/// what it is working on right now.
///
/// Two turns, because the first is deliberately held: the kill switch needs
/// a cache reading from a turn compaction did not touch before it can judge
/// later ones.
#[tokio::test]
async fn proxy_condenses_old_tool_results_before_the_upstream_sees_them() {
    let (_captured, sink) = recording_sink();
    let (upstream, seen) = mock_upstream_recording(WARM_CACHE_TURN).await;
    let port = start_proxy_with(upstream, sink, compactor(lazybox_core::CompactionMode::On)).await;

    let sent = conversation_body(8);
    post_conversation(port, "github-acme-widget-7", sent.clone()).await;
    post_conversation(port, "github-acme-widget-7", sent.clone()).await;

    let requests = seen.lock().expect("lock").clone();
    assert_eq!(requests.len(), 2, "both turns reached the upstream");
    assert_eq!(
        upstream_body(&requests[0]),
        sent,
        "the first turn establishes the cache baseline and is not rewritten"
    );

    let forwarded = upstream_body(&requests[1]);
    assert!(
        forwarded.contains(lazybox_core::context_hygiene::CONDENSED_MARKER),
        "old blocks reached the upstream condensed"
    );
    assert!(
        forwarded.len() < sent.len(),
        "the forwarded body is smaller than what the agent sent: {} vs {}",
        forwarded.len(),
        sent.len()
    );
    assert!(
        !forwarded.contains("block 0 line 200"),
        "the oldest block's bulk was elided"
    );
    assert!(
        forwarded.contains("block 7 line 200"),
        "the newest block crossed verbatim"
    );
    // Content-Length was recomputed for the rewritten body, or the upstream
    // would have hung waiting for bytes that never come.
    let head = requests[1]
        .split_once("\r\n\r\n")
        .map(|(h, _)| h)
        .unwrap_or("");
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("content-length: {}", forwarded.len())),
        "content-length matches the rewritten body: {head:?}"
    );
}

/// Shadow mode is the evidence-gathering mode: it must never change what
/// crosses the wire, or the evidence is worthless.
#[tokio::test]
async fn shadow_mode_forwards_the_original_bytes() {
    let (_captured, sink) = recording_sink();
    let (upstream, seen) = mock_upstream_recording(WARM_CACHE_TURN).await;
    let port = start_proxy_with(
        upstream,
        sink,
        compactor(lazybox_core::CompactionMode::Shadow),
    )
    .await;

    let sent = conversation_body(8);
    // Two turns, so a baseline exists and only the mode is holding the bytes.
    post_conversation(port, "github-acme-widget-7", sent.clone()).await;
    post_conversation(port, "github-acme-widget-7", sent.clone()).await;

    let requests = seen.lock().expect("lock").clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            upstream_body(request),
            sent,
            "shadow mode leaves the body byte-identical"
        );
    }
}
