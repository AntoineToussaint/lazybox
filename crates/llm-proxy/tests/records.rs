//! Serde round-trip for the proxy record types. Same rationale as the
//! ipc tests: wire format changes should be loud, not silent.

use lazybox_llm_proxy::{ApiProvider, ProxyConfig, ProxyRecord, ToolCall};
use std::time::Duration;

fn sample_record() -> ProxyRecord {
    ProxyRecord {
        session_key: "github:o/r#1".into(),
        started_at: chrono::Utc::now(),
        duration: Duration::from_millis(1234),
        provider: ApiProvider::Anthropic,
        endpoint: "/v1/messages".into(),
        request_model: Some("claude-sonnet-4-6".into()),
        tokens_input: Some(1000),
        tokens_output: Some(500),
        tokens_cache_read: Some(4096),
        tokens_cache_create: Some(512),
        estimated_cost_usd: Some(0.0105),
        tool_calls: vec![ToolCall {
            name: "Read".into(),
            args_summary: "path=src/lib.rs".into(),
            result_size_bytes: Some(2048),
            duration: Some(Duration::from_millis(12)),
        }],
        assistant_text: Some("Opening the file now.".into()),
        status: 200,
        error: None,
    }
}

#[test]
fn record_json_round_trip() {
    let rec = sample_record();
    let json = serde_json::to_string(&rec).expect("to json");
    let back: ProxyRecord = serde_json::from_str(&json).expect("from json");
    assert_eq!(format!("{rec:?}"), format!("{back:?}"));
}

#[test]
fn record_bincode_round_trip() {
    let rec = sample_record();
    let bytes = bincode::serialize(&rec).expect("bincode");
    let back: ProxyRecord = bincode::deserialize(&bytes).expect("bincode de");
    assert_eq!(format!("{rec:?}"), format!("{back:?}"));
}

#[test]
fn record_error_path_serializes() {
    // A failed request with no token info still needs to round-trip —
    // these are the ones we most want to keep for debugging.
    let rec = ProxyRecord {
        error: Some("upstream 502".into()),
        status: 502,
        tokens_input: None,
        tokens_output: None,
        estimated_cost_usd: None,
        tool_calls: vec![],
        assistant_text: None,
        ..sample_record()
    };
    let json = serde_json::to_string(&rec).expect("to json");
    let back: ProxyRecord = serde_json::from_str(&json).expect("from json");
    assert_eq!(back.status, 502);
    assert_eq!(back.error.as_deref(), Some("upstream 502"));
}

#[test]
fn default_proxy_config_binds_localhost() {
    // Critical: default must NEVER bind a non-loopback address.
    // Hard-coded assertion so a future "let's listen on 0.0.0.0"
    // change is caught in review.
    let cfg = ProxyConfig::default();
    assert!(
        cfg.listen.ip().is_loopback(),
        "proxy must bind to loopback only"
    );
    assert!(
        cfg.record_bodies,
        "recording is on by default for useful out-of-the-box UX"
    );
    // Sensitive headers stripped by default.
    for h in ["authorization", "x-api-key", "cookie"] {
        assert!(
            cfg.redact_headers.iter().any(|r| r == h),
            "{h} should be in default redact list"
        );
    }
}
