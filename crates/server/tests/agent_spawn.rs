//! Tests for `agent_spawn` env injection + the proxy-record event
//! flow through IPC. The end-to-end "child makes HTTP through proxy"
//! coverage previously lived here but required real `sh` + `curl`
//! subprocesses through a real PTY — gated behind `#[ignore]` and
//! replaced with focused unit tests for the building blocks.
#![allow(clippy::unnecessary_get_then_check)]

use pilot_server::agent_spawn::{ProxyProvider, ProxyTarget, inject_proxy_env};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

const TEST_DEADLINE: Duration = Duration::from_secs(2);

#[test]
fn anthropic_proxy_injects_anthropic_base_url() {
    let mut env: HashMap<String, String> = HashMap::new();
    let target = ProxyTarget {
        provider: ProxyProvider::Anthropic,
        upstream: "https://api.anthropic.com".into(),
    };
    inject_proxy_env(&mut env, Some(&target), Some("http://127.0.0.1:9000"));
    assert_eq!(
        env.get("ANTHROPIC_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:9000")
    );
    assert!(env.get("OPENAI_BASE_URL").is_none());
}

#[test]
fn openai_proxy_injects_openai_base_url() {
    let mut env: HashMap<String, String> = HashMap::new();
    let target = ProxyTarget {
        provider: ProxyProvider::OpenAI,
        upstream: "https://api.openai.com".into(),
    };
    inject_proxy_env(&mut env, Some(&target), Some("http://127.0.0.1:9001"));
    assert_eq!(
        env.get("OPENAI_BASE_URL").map(String::as_str),
        Some("http://127.0.0.1:9001")
    );
    assert!(env.get("ANTHROPIC_BASE_URL").is_none());
}

#[test]
fn no_proxy_leaves_env_untouched() {
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("EXISTING".into(), "1".into());
    inject_proxy_env(&mut env, None, None);
    assert_eq!(env.len(), 1, "no proxy env vars added");
    assert!(env.get("ANTHROPIC_BASE_URL").is_none());
    assert!(env.get("OPENAI_BASE_URL").is_none());
}

#[test]
fn missing_listen_url_skips_injection() {
    // Defensive: target without a resolved listen URL is a no-op (the
    // caller hasn't started the proxy yet). Without this guard a
    // mis-wired caller would silently inject an empty string.
    let mut env: HashMap<String, String> = HashMap::new();
    let target = ProxyTarget {
        provider: ProxyProvider::Anthropic,
        upstream: "https://api.anthropic.com".into(),
    };
    inject_proxy_env(&mut env, Some(&target), None);
    assert!(env.is_empty());
}

#[tokio::test]
async fn proxy_record_event_round_trips_through_ipc() {
    timeout(TEST_DEADLINE, async {
        use pilot_ipc::{Event, channel};

        let record = pilot_llm_proxy::ProxyRecord {
            session_key: "github:o/r#1".into(),
            started_at: chrono::Utc::now(),
            duration: Duration::from_millis(420),
            provider: pilot_llm_proxy::ApiProvider::Anthropic,
            endpoint: "/v1/messages".into(),
            request_model: Some("claude-sonnet-4-6".into()),
            tokens_input: Some(1024),
            tokens_output: Some(512),
            tokens_cache_read: None,
            tokens_cache_create: None,
            estimated_cost_usd: Some(0.01),
            tool_calls: vec![],
            assistant_text: Some("hi".into()),
            status: 200,
            error: None,
        };
        let event = Event::ProxyRecord(record.clone());

        let (mut client, server) = channel::pair();
        server.tx.send(event.clone()).unwrap();
        let received = timeout(Duration::from_secs(1), client.recv())
            .await
            .expect("timeout")
            .expect("event");
        match received {
            Event::ProxyRecord(r) => {
                assert_eq!(format!("{r:?}"), format!("{record:?}"));
            }
            other => panic!("expected ProxyRecord, got {other:?}"),
        }
    })
    .await
    .expect("deadline");
}
