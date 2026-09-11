//! Provider-agnostic token-usage extraction from an LLM API response
//! body (#1062).
//!
//! Both Anthropic and OpenAI report usage as a JSON `usage` object — in a
//! non-streaming response it sits at the top level; in a streaming (SSE)
//! response it rides one or more `data:` lines. The two providers even
//! disagree on the *shape* of that object (`input_tokens` vs
//! `prompt_tokens`, a flat `cache_read_input_tokens` vs a nested
//! `prompt_tokens_details.cached_tokens`), and Codex's Responses API
//! nests it a further level under `response`.
//!
//! Anthropic start/delta counters are merged as high-water marks. Responses
//! streams instead publish an immutable snapshot from their terminal event.
//! SSE framing accepts LF, CRLF and CR, including multiline data. A bounded
//! JSON projection discards output payloads while preserving usage metadata.

#[path = "usage_json.rs"]
mod usage_json;
use usage_json::UsageJson;

use lazybox_core::ModelPrice;
use lazybox_core::pricing::{self, TokenCounts};
use lazybox_ipc::AgentUsage;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A shared, immutable set of per-model price overrides (from
/// `agent.pricing`). Built once when the proxy starts and cloned into every
/// [`UsageAccumulator`]; empty means "built-in rate card only".
pub type PriceOverrides = Arc<BTreeMap<String, ModelPrice>>;

/// Running high-water totals for one response, merged across every
/// `usage` object seen. `None` fields mean "not yet observed".
#[derive(Debug, Default, Clone, Copy)]
struct Merged {
    input: Option<u64>,
    output: Option<u64>,
    cache_creation: Option<u64>,
    cache_read: Option<u64>,
}

impl Merged {
    /// Fold one field in as a high-water mark: a later cumulative report
    /// (streaming output) supersedes an earlier one, and a field only one
    /// provider emits survives untouched.
    fn bump(slot: &mut Option<u64>, value: Option<u64>) {
        if let Some(v) = value {
            *slot = Some(slot.map_or(v, |cur| cur.max(v)));
        }
    }

    fn merge(&mut self, other: Merged) {
        Self::bump(&mut self.input, other.input);
        Self::bump(&mut self.output, other.output);
        Self::bump(&mut self.cache_creation, other.cache_creation);
        Self::bump(&mut self.cache_read, other.cache_read);
    }

    fn is_empty(&self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.cache_creation.is_none()
            && self.cache_read.is_none()
    }
}

/// Incrementally extracts token usage from a response body streamed in
/// arbitrary chunks. Feed bytes with [`push`](Self::push) as they flow to
/// the client, then call [`finish`](Self::finish) once the stream ends.
#[derive(Debug, Default)]
pub struct UsageAccumulator {
    json: UsageJson,
    wire: Wire,
    line: Line,
    line_nonempty: bool,
    event_data: bool,
    after_cr: bool,
    merged: Merged,
    /// The model id seen in the stream (`message.model` / `response.model`),
    /// last non-empty wins. Needed to price the tokens in [`finish`].
    model: Option<String>,
    /// Per-model price overrides; empty → built-in rate card only.
    prices: PriceOverrides,
    /// Count-only mode: capture token counts but report `$0` cost regardless
    /// of the rate card. Used for traffic lazybox can't price — a ChatGPT
    /// subscription pays a flat fee, so its per-token cost is meaningless.
    count_only: bool,
    responses: Responses,
}

/// Terminal-without-usage is distinct from a stream still in progress: neither
/// may fall back to a provisional aggregate, including at HTTP EOF.
#[derive(Debug, Default)]
enum Responses {
    #[default]
    Unseen,
    Streaming,
    Terminal(Option<AgentUsage>),
}

#[derive(Debug, Default)]
enum Wire {
    #[default]
    Detect,
    Json,
    Sse,
}

#[derive(Debug, Default)]
enum Line {
    #[default]
    Start,
    Prefix(usize),
    Data,
    Ignore,
}

impl UsageAccumulator {
    /// An accumulator that prices its result with `prices` layered over the
    /// built-in rate card. [`UsageAccumulator::default`] uses the built-ins
    /// alone.
    pub fn with_prices(prices: PriceOverrides) -> Self {
        Self {
            prices,
            ..Self::default()
        }
    }

    /// Capture token counts but report `$0` cost regardless of the rate card.
    /// For traffic lazybox can't meaningfully price — a ChatGPT-subscription
    /// Codex session pays a flat monthly fee, so its per-token cost is `$0`
    /// while the counts still flow into usage totals.
    pub fn counting_only(mut self) -> Self {
        self.count_only = true;
        self
    }

    /// Feed arbitrary transport chunks without buffering output payloads.
    pub fn push(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if matches!(self.responses, Responses::Terminal(_)) {
                break;
            }
            if matches!(self.wire, Wire::Detect) {
                if byte.is_ascii_whitespace() {
                    continue;
                }
                self.wire = if byte == b'{' { Wire::Json } else { Wire::Sse };
            }
            if matches!(self.wire, Wire::Json) {
                self.json.push(byte);
                continue;
            }
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if matches!(byte, b'\r' | b'\n') {
                if !self.line_nonempty {
                    self.end_event();
                } else if matches!(self.line, Line::Data) {
                    self.json.push(b'\n');
                }
                self.line = Line::Start;
                self.line_nonempty = false;
                continue;
            }
            self.line_nonempty = true;
            match self.line {
                Line::Start if byte == b'd' => self.line = Line::Prefix(1),
                Line::Prefix(n) if byte == b"data:"[n] => {
                    self.line = if n == 4 {
                        self.event_data = true;
                        Line::Data
                    } else {
                        Line::Prefix(n + 1)
                    };
                }
                Line::Data => self.json.push(byte),
                _ => self.line = Line::Ignore,
            }
        }
    }

    fn end_event(&mut self) {
        if self.event_data || matches!(self.wire, Wire::Json) {
            if let Some(value) = std::mem::take(&mut self.json).finish() {
                self.scan_value(value);
            }
            self.event_data = false;
        }
    }

    /// Finish the body. A Responses stream without valid terminal totals
    /// must not promote provisional counters into a final report at EOF.
    pub fn finish(mut self) -> Option<AgentUsage> {
        self.end_event();
        match self.responses {
            Responses::Terminal(usage) => usage,
            Responses::Streaming => None,
            Responses::Unseen => self.usage(),
        }
    }

    /// Final usage after a terminal Responses API event, before HTTP EOF.
    /// Callers publish this before forwarding the terminal event to clients that
    /// stop reading there; they must suppress a second report at EOF.
    pub fn completed_usage(&self) -> Option<AgentUsage> {
        match &self.responses {
            Responses::Terminal(usage) => usage.clone(),
            _ => None,
        }
    }

    fn usage(&self) -> Option<AgentUsage> {
        if self.merged.is_empty() {
            return None;
        }
        // Count-only traffic (a ChatGPT subscription) is a flat fee, so its
        // per-token cost is $0 even though the counts are real. Otherwise
        // price the tokens when the stream named a model we recognize; an
        // unknown model leaves cost absent rather than guessing.
        let cost_usd_micros = if self.count_only {
            Some(0)
        } else {
            self.model.as_deref().and_then(|model| {
                pricing::cost_micros(
                    model,
                    &TokenCounts {
                        input: self.merged.input.unwrap_or(0),
                        output: self.merged.output.unwrap_or(0),
                        cache_creation: self.merged.cache_creation.unwrap_or(0),
                        cache_read: self.merged.cache_read.unwrap_or(0),
                    },
                    &self.prices,
                )
            })
        };
        Some(AgentUsage {
            input_tokens: self.merged.input,
            output_tokens: self.merged.output,
            cache_creation_input_tokens: self.merged.cache_creation,
            cache_read_input_tokens: self.merged.cache_read,
            cost_usd_micros,
            // Filled in by the proxy from the *request* body (#1606); this
            // parser only ever sees the response.
            context: None,
        })
    }

    fn scan_value(&mut self, value: Value) {
        if matches!(self.responses, Responses::Terminal(_)) {
            return;
        }
        if let Some(kind) = value.get("type").and_then(Value::as_str)
            && kind.starts_with("response.")
        {
            self.responses = Responses::Streaming;
            if matches!(
                kind,
                "response.completed" | "response.incomplete" | "response.failed"
            ) {
                self.responses = Responses::Terminal(None);
                // Only the terminal response owns definitive counts. Never fill
                // missing final fields from provisional or diagnostic records.
                if let Some(usage) = value.pointer("/response/usage").and_then(Value::as_object) {
                    let totals = extract(usage);
                    if valid_response_usage(usage) {
                        self.merged = totals;
                        self.model = value
                            .pointer("/response/model")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned);
                        self.responses = Responses::Terminal(self.usage());
                    }
                }
            }
            return;
        }
        collect_usage(&value, &mut self.merged, &mut self.model);
    }
}

/// Fold provider-owned metadata retained by the JSON projection into the
/// generic aggregate. Output/tool payloads have already been discarded, so
/// nested user content cannot impersonate a provider usage or model field.
fn collect_usage(value: &Value, merged: &mut Merged, model: &mut Option<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Object(usage)) = map.get("usage") {
                merged.merge(extract(usage));
            }
            // The model id rides `message.model` (Anthropic) / `response.model`
            // (Codex) / the top-level `model` (non-streaming, OpenAI chunks).
            // Capture any non-empty `model` string; last seen wins, which is
            // fine since a turn reports one model. Needed to price the tokens.
            if let Some(Value::String(name)) = map.get("model")
                && !name.is_empty()
            {
                *model = Some(name.clone());
            }
            for child in map.values() {
                collect_usage(child, merged, model);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_usage(item, merged, model);
            }
        }
        _ => {}
    }
}

/// A terminal Responses report requires both totals and numeric optional
/// cache counts. Reject malformed fields instead of pricing them as zero.
fn valid_response_usage(usage: &serde_json::Map<String, Value>) -> bool {
    if !["input_tokens", "output_tokens"]
        .iter()
        .all(|key| usage.get(*key).and_then(Value::as_u64).is_some())
    {
        return false;
    }
    for key in [
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "cached_input_tokens",
    ] {
        if usage.get(key).is_some_and(|value| value.as_u64().is_none()) {
            return false;
        }
    }
    for key in ["input_tokens_details", "prompt_tokens_details"] {
        if let Some(details) = usage.get(key) {
            let Some(details) = details.as_object() else {
                return false;
            };
            if details
                .get("cached_tokens")
                .is_some_and(|value| value.as_u64().is_none())
            {
                return false;
            }
        }
    }
    true
}

/// Read the token counts from one `usage` object, accepting either
/// provider's key names.
fn extract(usage: &serde_json::Map<String, Value>) -> Merged {
    let num = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(Value::as_u64))
    };
    let nested = |parent: &str, child: &str| {
        usage
            .get(parent)
            .and_then(Value::as_object)
            .and_then(|d| d.get(child))
            .and_then(Value::as_u64)
    };
    Merged {
        input: num(&["input_tokens", "prompt_tokens"]),
        output: num(&["output_tokens", "completion_tokens"]),
        cache_creation: num(&["cache_creation_input_tokens"]),
        cache_read: num(&["cache_read_input_tokens", "cached_input_tokens"])
            .or_else(|| nested("prompt_tokens_details", "cached_tokens"))
            .or_else(|| nested("input_tokens_details", "cached_tokens")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(chunks: &[&str]) -> Option<AgentUsage> {
        let mut acc = UsageAccumulator::default();
        for chunk in chunks {
            acc.push(chunk.as_bytes());
        }
        acc.finish()
    }

    #[test]
    fn anthropic_non_streaming_top_level_usage() {
        let body = r#"{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":42,"cache_creation_input_tokens":7,"cache_read_input_tokens":5}}"#;
        let u = feed(&[body]).expect("usage");
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.output_tokens, Some(42));
        assert_eq!(u.cache_creation_input_tokens, Some(7));
        assert_eq!(u.cache_read_input_tokens, Some(5));
    }

    #[test]
    fn anthropic_streaming_merges_message_start_and_delta() {
        // input/cache land in `message_start`; the growing cumulative
        // output lands in successive `message_delta`s. The high-water
        // merge reassembles the turn.
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1000,\"cache_read_input_tokens\":200,\"output_tokens\":1}}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":50}}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":120}}\n",
            "\n",
        );
        let u = feed(&[stream]).expect("usage");
        assert_eq!(u.input_tokens, Some(1000));
        assert_eq!(u.cache_read_input_tokens, Some(200));
        assert_eq!(u.output_tokens, Some(120));
    }

    #[test]
    fn openai_chat_streaming_final_chunk_usage() {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":300,\"completion_tokens\":80,\"prompt_tokens_details\":{\"cached_tokens\":64}}}\n\n",
            "data: [DONE]\n\n",
        );
        let u = feed(&[stream]).expect("usage");
        assert_eq!(u.input_tokens, Some(300));
        assert_eq!(u.output_tokens, Some(80));
        assert_eq!(u.cache_read_input_tokens, Some(64));
    }

    #[test]
    fn codex_responses_streaming_nested_usage() {
        // Codex hits the Responses API: usage nests under `response`.
        let stream = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":500,\"output_tokens\":25,\"input_tokens_details\":{\"cached_tokens\":100}}}}\n\n",
        );
        let u = feed(&[stream]).expect("usage");
        assert_eq!(u.input_tokens, Some(500));
        assert_eq!(u.output_tokens, Some(25));
        assert_eq!(u.cache_read_input_tokens, Some(100));
    }

    #[test]
    fn responses_usage_is_final_only_after_the_complete_completion_line() {
        let mut acc = UsageAccumulator::default();
        acc.push(b"data: {\"type\":\"response.created\",\"response\":{\"usage\":{\"input_tokens\":5}}}\n\n");
        assert!(acc.completed_usage().is_none());
        acc.push(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":500,");
        assert!(acc.completed_usage().is_none());
        acc.push(b"\"output_tokens\":25}}}\n\n");
        let usage = acc.completed_usage().expect("finalized usage");
        assert_eq!(usage.input_tokens, Some(500));
        assert_eq!(usage.output_tokens, Some(25));
        let final_usage = acc.finish().expect("usage at EOF");
        assert_eq!(final_usage.input_tokens, usage.input_tokens);
        assert_eq!(final_usage.output_tokens, usage.output_tokens);
    }

    #[test]
    fn completion_without_usage_does_not_finalize_an_earlier_usage_report() {
        let mut acc = UsageAccumulator::default();
        acc.push(b"data: {\"usage\":{\"input_tokens\":5}}\n\n");
        acc.push(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":null}}\n\n");
        assert!(acc.completed_usage().is_none());
        assert!(acc.finish().is_none());
    }

    #[test]
    fn responses_reject_invalid_final_totals_even_at_eof() {
        for usage in [
            "null",
            "{}",
            r#"{"input_tokens":500}"#,
            r#"{"input_tokens":"500","output_tokens":25}"#,
            r#"{"input_tokens":-1,"output_tokens":25}"#,
            r#"{"input_tokens":500,"output_tokens":25,"input_tokens_details":{"cached_tokens":"100"}}"#,
        ] {
            let mut acc = UsageAccumulator::default();
            acc.push(b"data: {\"type\":\"response.in_progress\",\"response\":{\"usage\":{\"input_tokens\":500,\"output_tokens\":1}}}\n\n");
            acc.push(
                format!(
                    r#"data: {{"type":"response.completed","response":{{"usage":{usage}}}}}

"#
                )
                .as_bytes(),
            );
            assert!(acc.completed_usage().is_none(), "{usage}");
            assert!(acc.finish().is_none(), "{usage}");
        }
        let mut acc = UsageAccumulator::default();
        acc.push(b"data: {\"type\":\"response.in_progress\",\"response\":{\"usage\":{\"input_tokens\":500,\"output_tokens\":1}}}\n\n");
        assert!(
            acc.finish().is_none(),
            "interrupted Responses stream is not definitive"
        );
    }

    #[test]
    fn all_terminal_kinds_require_their_own_totals_and_model() {
        for kind in [
            "response.completed",
            "response.incomplete",
            "response.failed",
        ] {
            for response in [
                serde_json::json!({}),
                serde_json::json!({"usage":null}),
                serde_json::json!({"usage":{"input_tokens":0,"output_tokens":0}}),
            ] {
                let mut acc = UsageAccumulator::default();
                acc.push(
                    b"data: {\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":999}}\n\n",
                );
                acc.push(
                    format!(
                        "data: {}\n\n",
                        serde_json::json!({"type":kind,"response":response})
                    )
                    .as_bytes(),
                );
                let finalized = acc.completed_usage();
                if response["usage"].is_object() {
                    let usage = finalized.expect("zero totals are valid");
                    assert_eq!(usage.input_tokens, Some(0));
                    assert_eq!(usage.output_tokens, Some(0));
                    assert_eq!(
                        usage.cost_usd_micros, None,
                        "do not inherit an unrelated model"
                    );
                } else {
                    assert!(finalized.is_none());
                    assert!(acc.finish().is_none());
                }
            }
        }
    }

    #[test]
    fn terminal_snapshot_is_independent_of_trailing_records_and_chunking() {
        let stream = concat!(
            "data: {\"type\":\"response.in_progress\",\"response\":{\"model\":\"wrong\",\"usage\":{\"input_tokens\":9000000,\"output_tokens\":99}}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"claude-sonnet-4-5\",\"output\":[{\"model\":\"wrong\",\"usage\":{\"input_tokens\":9999999}}],\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"gateway.diagnostic\",\"model\":\"wrong\",\"usage\":{\"input_tokens\":9000000,\"output_tokens\":99}}\n\n",
        );
        for size in [1, 17, stream.len()] {
            let mut acc = UsageAccumulator::default();
            for chunk in stream.as_bytes().chunks(size) {
                acc.push(chunk);
            }
            let usage = acc.completed_usage().expect("terminal usage");
            assert_eq!(usage.input_tokens, Some(1_000_000));
            assert_eq!(usage.output_tokens, Some(0));
            assert_eq!(usage.cost_usd_micros, Some(3_000_000));
            assert_eq!(acc.finish().unwrap().cost_usd_micros, Some(3_000_000));
        }
    }

    #[test]
    fn sse_delimiters_and_multiline_data_survive_every_chunk_boundary() {
        for newline in ["\n", "\r\n", "\r"] {
            let stream = format!(
                "event: response.incomplete{newline}data: {{\"type\":\"response.incomplete\",{newline}: comment{newline}data: \"response\":{{\"usage\":{{\"input_tokens\":500,\"output_tokens\":25}}}}}}{newline}{newline}"
            );
            for split in 0..=stream.len() {
                let mut acc = UsageAccumulator::default();
                acc.push(&stream.as_bytes()[..split]);
                acc.push(&stream.as_bytes()[split..]);
                assert_eq!(
                    acc.completed_usage()
                        .expect("usage before EOF")
                        .output_tokens,
                    Some(25)
                );
            }
        }
    }

    #[test]
    fn large_responses_preserve_usage_with_bounded_storage() {
        let payload = serde_json::json!({"type":"response.completed", "response": {
            "output": [{"type":"image_generation_call", "result": "a".repeat(9 * 1024 * 1024)}],
            "usage": {"input_tokens":500, "output_tokens":25}
        }});
        let stream = format!("data: {payload}\n\n");
        for size in [16 * 1024, stream.len()] {
            let mut acc = UsageAccumulator::default();
            for chunk in stream.as_bytes().chunks(size) {
                acc.push(chunk);
            }
            assert_eq!(
                acc.completed_usage()
                    .expect("large event usage")
                    .input_tokens,
                Some(500)
            );
        }
        // A large number of output items must not accumulate retained objects.
        let mut acc = UsageAccumulator::default();
        acc.push(b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[");
        for _ in 0..100_000 {
            acc.push(b"{\"text\":\"ignored\"},");
        }
        acc.push(b"null],\"usage\":{\"input_tokens\":500,\"output_tokens\":25}}}\n\n");
        assert_eq!(acc.completed_usage().unwrap().output_tokens, Some(25));
    }

    #[test]
    fn pretty_printed_nonstreaming_json_keeps_usage() {
        let u = feed(&["{\n  \"model\": \"claude-sonnet-4-5\",\n  \"usage\": {\n    \"input_tokens\": 1000000,\n    \"output_tokens\": 0\n  }\n}"]).unwrap();
        assert_eq!(u.cost_usd_micros, Some(3_000_000));
    }

    #[test]
    fn usage_survives_a_chunk_boundary_mid_line() {
        // The decisive `data:` line is split across two transport chunks.
        let a = "data: {\"usage\":{\"input_tok";
        let b = "ens\":77,\"output_tokens\":8}}\n\n";
        let u = feed(&[a, b]).expect("usage");
        assert_eq!(u.input_tokens, Some(77));
        assert_eq!(u.output_tokens, Some(8));
    }

    #[test]
    fn no_usage_object_yields_none() {
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert!(feed(&[stream]).is_none());
    }

    #[test]
    fn prices_a_known_model_from_the_stream() {
        // `message_start` carries both `message.model` and `message.usage`;
        // the model lets `finish` price the tokens off the built-in card.
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}}\n",
            "\n",
        );
        let u = feed(&[stream]).expect("usage");
        assert_eq!(u.input_tokens, Some(1_000_000));
        // 1M Sonnet input == $3.00 == 3_000_000 micros.
        assert_eq!(u.cost_usd_micros, Some(3_000_000));
    }

    #[test]
    fn unknown_model_leaves_cost_absent() {
        let stream = "data: {\"model\":\"mystery-model-9\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}\n\n";
        let u = feed(&[stream]).expect("usage");
        assert_eq!(u.input_tokens, Some(1_000_000));
        assert_eq!(u.cost_usd_micros, None);
    }

    #[test]
    fn overrides_price_an_otherwise_unknown_model() {
        let mut map = BTreeMap::new();
        map.insert(
            "mystery-model".to_string(),
            ModelPrice {
                input: 10.0,
                output: 10.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let mut acc = UsageAccumulator::with_prices(Arc::new(map));
        acc.push(
            b"data: {\"model\":\"mystery-model-9\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}\n\n",
        );
        let u = acc.finish().expect("usage");
        assert_eq!(u.cost_usd_micros, Some(10_000_000));
    }

    #[test]
    fn non_json_noise_is_ignored() {
        assert!(feed(&[": keep-alive comment\n\nevent: ping\n"]).is_none());
    }

    #[test]
    fn counting_only_reports_zero_cost_but_keeps_counts() {
        // A ChatGPT-subscription Codex stream names a real, priceable model,
        // but the subscription is a flat fee — count-only zeroes the cost
        // while the token counts still flow through.
        let mut acc = UsageAccumulator::default().counting_only();
        acc.push(
            b"data: {\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}\n\n",
        );
        let u = acc.finish().expect("usage");
        assert_eq!(u.input_tokens, Some(1_000_000));
        assert_eq!(
            u.cost_usd_micros,
            Some(0),
            "count-only → $0 despite a known model"
        );
    }

    #[test]
    fn counting_only_with_no_usage_still_none() {
        // Count-only doesn't fabricate a usage record where the body had none.
        let acc = UsageAccumulator::default().counting_only();
        assert!(acc.finish().is_none());
    }
}
