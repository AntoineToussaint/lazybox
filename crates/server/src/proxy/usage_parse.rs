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
//! Rather than branch on provider and protocol, [`UsageAccumulator`] scans
//! the response for *every* object named `usage`, reads whichever token
//! keys are present, and keeps a per-field high-water mark. That single
//! rule is correct across all four cases:
//!   - non-streaming: one `usage` object → its values.
//!   - Anthropic streaming: `message_start` carries input/cache, each
//!     `message_delta` carries the growing cumulative output — the max of
//!     each field reassembles the turn's total.
//!   - OpenAI chat streaming: usage rides only the final chunk.
//!   - Codex/OpenAI Responses streaming: usage nests under
//!     `response.usage` on the completion event.
//!
//! The scan is line-oriented so it costs O(1) memory over an arbitrarily
//! long stream: complete `\n`-terminated lines are parsed as they arrive
//! and dropped; only a partial trailing line is retained.

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
    /// Bytes of the current line not yet terminated by `\n`. For a
    /// non-streaming (single-JSON) body this holds the whole document
    /// until `finish`.
    residual: Vec<u8>,
    merged: Merged,
    /// The model id seen in the stream (`message.model` / `response.model`),
    /// last non-empty wins. Needed to price the tokens in [`finish`].
    model: Option<String>,
    /// Per-model price overrides; empty → built-in rate card only.
    prices: PriceOverrides,
}

/// A partial line longer than this without a newline is parsed and
/// cleared to bound memory — far above any real compact JSON body, so
/// only a pathological upstream ever trips it.
const MAX_RESIDUAL: usize = 8 * 1024 * 1024;

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

    /// Feed one response chunk. Complete lines are parsed and dropped;
    /// the trailing partial line is retained for the next chunk.
    pub fn push(&mut self, chunk: &[u8]) {
        self.residual.extend_from_slice(chunk);
        while let Some(nl) = self.residual.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.residual.drain(..=nl).collect();
            self.scan_line(&line[..line.len() - 1]);
        }
        if self.residual.len() > MAX_RESIDUAL {
            let line = std::mem::take(&mut self.residual);
            self.scan_line(&line);
        }
    }

    /// Finish the stream and return the merged usage, or `None` when the
    /// body carried no `usage` object at all.
    pub fn finish(mut self) -> Option<AgentUsage> {
        if !self.residual.is_empty() {
            let line = std::mem::take(&mut self.residual);
            self.scan_line(&line);
        }
        if self.merged.is_empty() {
            return None;
        }
        // Price the tokens when the stream named a model we recognize; an
        // unknown model leaves cost absent rather than guessing.
        let cost_usd_micros = self.model.as_deref().and_then(|model| {
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
        });
        Some(AgentUsage {
            input_tokens: self.merged.input,
            output_tokens: self.merged.output,
            cache_creation_input_tokens: self.merged.cache_creation,
            cache_read_input_tokens: self.merged.cache_read,
            cost_usd_micros,
        })
    }

    fn scan_line(&mut self, line: &[u8]) {
        let text = match std::str::from_utf8(line) {
            Ok(t) => t.trim(),
            Err(_) => return,
        };
        // SSE payload lines are `data: {json}`; `event:`/`id:`/comment
        // lines and the `[DONE]` sentinel carry no usage.
        let json = text.strip_prefix("data:").unwrap_or(text).trim();
        if json.is_empty() || json == "[DONE]" || !json.starts_with('{') {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(json) else {
            return;
        };
        collect_usage(&value, &mut self.merged, &mut self.model);
    }
}

/// Recursively fold every `usage` object reachable from `value` into
/// `merged`. Keying on the field name (rather than sniffing token-shaped
/// objects) keeps it precise: it catches the top-level `usage`,
/// Anthropic's `message.usage`, and Codex's `response.usage` without ever
/// mistaking an unrelated object for a usage report.
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
}
