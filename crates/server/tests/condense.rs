//! End-to-end coverage for the cheap-model summarizer (#1608): a condense
//! call reaches the upstream with the served request's own credentials, its
//! result is cached in the store, and every later call for the same content
//! returns those exact bytes — including after a daemon restart, which is
//! the case the store-backed cache exists for.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hyper::header::HeaderMap;
use lazybox_agents::LlmProvider;
use lazybox_core::context_hygiene::{CondenseKind, KV_PREFIX_CONDENSE, is_condensed};
use lazybox_server::condense::{ServedRequest, SummarizeError, Summarizer};
use lazybox_store::{SqliteStore, Store};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What one mock upstream observed: how many requests it served, and the
/// most recent request head + body.
#[derive(Default)]
struct Seen {
    calls: AtomicUsize,
    last: std::sync::Mutex<String>,
}

/// A mock upstream that answers every request with the same status and body
/// and records what it was sent. Returns its base URL.
async fn mock_upstream(status: u16, reply: &'static str) -> (String, Arc<Seen>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Seen::default());
    let recorder = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let request = read_request(&mut stream).await;
            recorder.calls.fetch_add(1, Ordering::SeqCst);
            *recorder.last.lock().expect("lock") = request;
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    (format!("http://{addr}"), seen)
}

/// An upstream that accepts the connection and never answers, so a call
/// against it can only end on the summarizer's own budget.
async fn mock_black_hole() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    format!("http://{addr}")
}

/// Read one whole HTTP request: headers, then exactly `Content-Length`
/// bytes. A single `read` would truncate a multi-kilobyte condense prompt
/// and the body assertions would pass or fail on chunk timing.
async fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut scratch = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut scratch).await.unwrap_or(0);
        if n == 0 {
            return String::from_utf8_lossy(&buf).into_owned();
        }
        buf.extend_from_slice(&scratch[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = stream.read(&mut scratch).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&scratch[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

const ANTHROPIC_REPLY: &str =
    r#"{"content":[{"type":"text","text":"main() parses argv and exits 2 on error"}]}"#;

fn summarizer(store: Arc<dyn Store>) -> Summarizer {
    Summarizer::new(
        store,
        reqwest::Client::new(),
        Arc::new(lazybox_config::Config::default()),
    )
}

fn served(base: &str) -> ServedRequest {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer session-token".parse().expect("hv"));
    headers.insert("anthropic-version", "2023-06-01".parse().expect("hv"));
    ServedRequest::new("claude", LlmProvider::Anthropic, base, &headers)
}

fn file_read() -> CondenseKind {
    CondenseKind::FileRead {
        path: "src/main.rs".into(),
    }
}

const INPUT: &str = "fn main() {\n    let args = std::env::args();\n    run(args);\n}\n";

/// The acceptance case: identical bytes across two calls and across a
/// daemon restart, with exactly one cheap-model call for all three.
#[tokio::test]
async fn condense_is_byte_stable_across_calls_and_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let (base, seen) = mock_upstream(200, ANTHROPIC_REPLY).await;
    let served = served(&base);

    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&db).expect("open"));
    let first = summarizer(store.clone())
        .condense(&served, INPUT, file_read())
        .await
        .expect("first condense");
    let second = summarizer(store.clone())
        .condense(&served, INPUT, file_read())
        .await
        .expect("second condense");
    assert_eq!(first, second, "same input, same bytes");
    assert_eq!(first.original_bytes, INPUT.len());
    assert_eq!(first.model, "claude-haiku-4-5", "Claude's own `low` tier");
    assert!(
        is_condensed(&first.text),
        "the header is the model's documented path back to the real bytes: {}",
        first.text
    );
    assert!(first.text.contains("main() parses argv"), "{}", first.text);

    // Restart: drop every handle, reopen the same database, condense again.
    drop(store);
    let restarted: Arc<dyn Store> = Arc::new(SqliteStore::open(&db).expect("reopen"));
    let after_restart = summarizer(restarted)
        .condense(&served, INPUT, file_read())
        .await
        .expect("condense after restart");
    assert_eq!(
        after_restart, first,
        "the cache is in the store, so a restart re-derives nothing"
    );

    assert_eq!(
        seen.calls.load(Ordering::SeqCst),
        1,
        "one cheap-model call served all three condensations"
    );
}

/// The condense call carries the served request's own credentials — there
/// is no second credential to configure — and none of the headers that
/// described the agent's own body.
#[tokio::test]
async fn the_condense_call_reuses_the_served_requests_credentials() {
    let (base, seen) = mock_upstream(200, ANTHROPIC_REPLY).await;
    let store: Arc<dyn Store> = Arc::new(lazybox_store::MemoryStore::new());
    summarizer(store)
        .condense(&served(&base), INPUT, file_read())
        .await
        .expect("condense");

    let request = seen.last.lock().expect("lock").clone();
    assert!(request.starts_with("POST /v1/messages "), "{request}");
    assert!(
        request.contains("authorization: Bearer session-token"),
        "{request}"
    );
    assert!(
        request.contains("anthropic-version: 2023-06-01"),
        "{request}"
    );
    assert!(
        request.contains("\"model\":\"claude-haiku-4-5\""),
        "the cheap tier, not the session's own model: {request}"
    );
    assert!(
        request.contains("src/main.rs"),
        "the prompt names what it is condensing: {request}"
    );
}

/// Failure is pass-through: an upstream error yields `Err` with no partial
/// output, and nothing is cached — so a later call retries rather than
/// serving a failure forever.
#[tokio::test]
async fn an_upstream_error_yields_err_and_caches_nothing() {
    let (base, seen) = mock_upstream(529, r#"{"type":"error"}"#).await;
    let store: Arc<dyn Store> = Arc::new(lazybox_store::MemoryStore::new());
    let summarizer = summarizer(store.clone());

    let error = summarizer
        .condense(&served(&base), INPUT, file_read())
        .await
        .expect_err("upstream is overloaded");
    assert!(matches!(error, SummarizeError::Status(529)), "{error:?}");

    assert!(
        store
            .list_kv_prefix(KV_PREFIX_CONDENSE)
            .expect("list")
            .is_empty(),
        "a failed condensation leaves no cache entry"
    );

    summarizer
        .condense(&served(&base), INPUT, file_read())
        .await
        .expect_err("still failing");
    assert_eq!(
        seen.calls.load(Ordering::SeqCst),
        2,
        "the failure is retried, not memoized"
    );
}

/// Two callers racing on the same fresh block make ONE model call and read
/// back the same bytes. Two calls would mean two summaries, and whichever
/// turn saw the loser's text would break the prompt-cache prefix behind it.
#[tokio::test]
async fn concurrent_callers_on_the_same_input_make_one_call() {
    let (base, seen) = mock_upstream(200, ANTHROPIC_REPLY).await;
    let store: Arc<dyn Store> = Arc::new(lazybox_store::MemoryStore::new());
    let summarizer = summarizer(store);

    let (left, right) = {
        let a = summarizer.clone();
        let b = summarizer.clone();
        let (base_a, base_b) = (base.clone(), base.clone());
        tokio::join!(
            tokio::spawn(async move { a.condense(&served(&base_a), INPUT, file_read()).await }),
            tokio::spawn(async move { b.condense(&served(&base_b), INPUT, file_read()).await }),
        )
    };
    let left = left.expect("join").expect("left condense");
    let right = right.expect("join").expect("right condense");

    assert_eq!(left.text, right.text);
    assert_eq!(
        seen.calls.load(Ordering::SeqCst),
        1,
        "the second caller waited on the first instead of calling again"
    );
}

/// A different path over the same bytes is a different condensation: the
/// header line embeds the path, so sharing an entry would label one file's
/// summary with another file's name.
#[tokio::test]
async fn the_kinds_payload_is_part_of_the_cache_identity() {
    let (base, seen) = mock_upstream(200, ANTHROPIC_REPLY).await;
    let store: Arc<dyn Store> = Arc::new(lazybox_store::MemoryStore::new());
    let summarizer = summarizer(store);
    let served = served(&base);

    let first = summarizer
        .condense(&served, INPUT, file_read())
        .await
        .expect("first");
    let other = summarizer
        .condense(
            &served,
            INPUT,
            CondenseKind::FileRead {
                path: "src/other.rs".into(),
            },
        )
        .await
        .expect("second");

    assert_ne!(first.text, other.text, "the header names its own path");
    assert_eq!(seen.calls.load(Ordering::SeqCst), 2);
}

/// The call is bounded. An upstream that accepts and then says nothing ends
/// on the summarizer's own budget, not on the agent's patience — and, like
/// every other failure, it caches nothing.
#[tokio::test]
async fn a_silent_upstream_ends_on_the_configured_budget() {
    let base = mock_black_hole().await;
    let store: Arc<dyn Store> = Arc::new(lazybox_store::MemoryStore::new());
    let mut config = lazybox_config::Config::default();
    config.agent.context_hygiene.condense_timeout_ms = 150;
    let summarizer = Summarizer::new(store.clone(), reqwest::Client::new(), Arc::new(config));

    let error = summarizer
        .condense(&served(&base), INPUT, file_read())
        .await
        .expect_err("the upstream never answers");
    assert!(matches!(error, SummarizeError::Timeout(_)), "{error:?}");
    assert!(
        store
            .list_kv_prefix(KV_PREFIX_CONDENSE)
            .expect("list")
            .is_empty(),
        "a timed-out condensation leaves no cache entry"
    );
}
