//! Request-side context accounting (#1606) — what the fleet is actually
//! paying to re-send.
//!
//! [`usage_parse`](super::usage_parse) reads the *response* to answer "what
//! did this turn cost?". This module reads the *request* to answer "what was
//! that cost made of?": how much of the conversation payload is tool output,
//! and how much of that tool output the session had already sent in an
//! earlier request. A tool result that stays in the transcript is re-sent
//! verbatim on every subsequent turn, so the second number is the size of
//! the purely mechanical part of the input bill — the number any future
//! compaction work has to beat.
//!
//! One rule covers all three wire shapes the proxy sees, because all three
//! put the conversation in a single top-level array (`messages` for
//! Anthropic Messages and OpenAI chat, `input` for the Responses API Codex
//! speaks) and mark tool output distinctively inside it:
//!   - Anthropic: a `tool_result` block inside a message's `content` array.
//!   - OpenAI chat: a whole message with `"role": "tool"`.
//!   - Responses: a `function_call_output` item.
//!
//! Bytes, not tokens: the proxy sees the wire body, and a ratio of bytes
//! stands in faithfully for a ratio of tokens. Nothing here rewrites the
//! body — the measurement is pure accounting.
//!
//! [`conversation`], [`tool_result_units`] and [`payload_text`] are the
//! shape-handling half, kept separable from the accounting so a second
//! pass over the same blocks reads them the same way this one counted them.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

use lazybox_ipc::ContextAccounting;
use serde_json::Value;

/// Block hashes retained per session. A session with more distinct tool
/// results than this loses its oldest hashes, so its re-send share is
/// under-reported rather than allowed to grow proxy memory without bound.
const MAX_BLOCKS_PER_SESSION: usize = 8192;

/// Sessions tracked at once, least-recently-used evicted. A session that
/// falls out and comes back starts from an empty set, so its next request
/// under-reports re-sends — again, bounded memory over perfect recall.
const MAX_SESSIONS: usize = 64;

/// The tool-result blocks one session has already sent, as content hashes.
#[derive(Debug, Default)]
pub(crate) struct SeenBlocks {
    set: HashSet<u64>,
    /// Insertion order, so the cap evicts the oldest hash.
    order: VecDeque<u64>,
}

impl SeenBlocks {
    /// Record `hash`, returning whether this session had already sent it.
    fn observe(&mut self, hash: u64) -> bool {
        if !self.set.insert(hash) {
            return true;
        }
        self.order.push_back(hash);
        if self.order.len() > MAX_BLOCKS_PER_SESSION
            && let Some(evicted) = self.order.pop_front()
        {
            self.set.remove(&evicted);
        }
        false
    }
}

/// Per-session [`SeenBlocks`], bounded in both dimensions.
#[derive(Debug, Default)]
pub(crate) struct SeenStore {
    sessions: HashMap<String, SeenBlocks>,
    /// Session keys, least-recently-used first.
    order: VecDeque<String>,
}

impl SeenStore {
    /// The seen-set for `key`, creating it and evicting the
    /// least-recently-used session when the map is full.
    pub(crate) fn entry(&mut self, key: &str) -> &mut SeenBlocks {
        if let Some(at) = self.order.iter().position(|k| k == key) {
            self.order.remove(at);
        } else if self.order.len() >= MAX_SESSIONS
            && let Some(evicted) = self.order.pop_front()
        {
            self.sessions.remove(&evicted);
        }
        self.order.push_back(key.to_string());
        self.sessions.entry(key.to_string()).or_default()
    }
}

/// One request's measurement, before it is compared against what the
/// session has already sent. Splitting the two halves keeps the JSON parse
/// — the expensive part, on the path of every proxied request — out of the
/// shared seen-set lock.
pub(crate) struct Measured {
    accounting: ContextAccounting,
    /// `(content hash, serialized bytes)` per tool-output block, in the
    /// order they appear in the request.
    blocks: Vec<(u64, u64)>,
}

impl Measured {
    /// Fold this request's blocks into `seen`, attributing the ones the
    /// session had already sent to the re-send share. Cheap enough to run
    /// under the proxy-wide lock.
    pub(crate) fn against(mut self, seen: &mut SeenBlocks) -> ContextAccounting {
        for (hash, bytes) in &self.blocks {
            if seen.observe(*hash) {
                self.accounting.tool_result_resent_bytes += bytes;
            }
        }
        self.accounting
    }
}

/// Measure one request body. `None` when the body isn't JSON carrying a
/// recognized conversation array — a token-count request, a health probe, a
/// shape lazybox doesn't know.
pub(crate) fn measure(body: &[u8], large_lines: usize) -> Option<Measured> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let array = conversation(&value)?;

    let mut out = Measured {
        accounting: ContextAccounting {
            message_bytes: serde_json::to_string(array).ok()?.len() as u64,
            ..ContextAccounting::default()
        },
        blocks: Vec::new(),
    };
    for unit in tool_result_units(array) {
        let Ok(serialized) = serde_json::to_string(unit) else {
            continue;
        };
        let bytes = serialized.len() as u64;
        out.accounting.tool_result_bytes += bytes;
        let mut hasher = DefaultHasher::new();
        serialized.hash(&mut hasher);
        out.blocks.push((hasher.finish(), bytes));
        if payload_text(unit).lines().count() > large_lines {
            out.accounting.large_tool_results += 1;
        }
    }
    Some(out)
}

/// The conversation array in a request body, whichever name the wire shape
/// gives it: `messages` for Anthropic Messages and OpenAI chat, `input` for
/// the Responses API. `None` for a body that carries neither.
pub(crate) fn conversation(body: &Value) -> Option<&Vec<Value>> {
    body.get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)
}

/// Every tool-output unit in a conversation array, across the three wire
/// shapes. A unit is whatever the provider re-sends as one piece, so it is
/// also the right granularity to hash for re-send detection.
pub(crate) fn tool_result_units(array: &[Value]) -> Vec<&Value> {
    let mut out = Vec::new();
    for item in array {
        let field = |name: &str| item.get(name).and_then(Value::as_str);
        if field("type") == Some("function_call_output") || field("role") == Some("tool") {
            out.push(item);
            continue;
        }
        if let Some(blocks) = item.get("content").and_then(Value::as_array) {
            out.extend(
                blocks.iter().filter(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_result")
                }),
            );
        }
    }
    out
}

/// The human-readable text a tool-output unit carries — what the line
/// threshold counts. The Responses API names it `output`; Anthropic and
/// OpenAI both name it `content`.
pub(crate) fn payload_text(unit: &Value) -> String {
    match unit.get("output").or_else(|| unit.get("content")) {
        Some(value) => text_of(value),
        None => String::new(),
    }
}

/// Flatten a text payload that may be a bare string, a list of content
/// blocks, or a single block object.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(text_of).collect::<Vec<_>>().join("\n"),
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .or_else(|| map.get("output"))
            .map(text_of)
            .unwrap_or_default(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measure `body` and immediately resolve it against `seen` — the two
    /// halves the proxy runs either side of its lock.
    fn measure_against(
        body: &[u8],
        seen: &mut SeenBlocks,
        large_lines: usize,
    ) -> Option<ContextAccounting> {
        Some(measure(body, large_lines)?.against(seen))
    }

    /// An Anthropic Messages body: one user turn, one tool_result block.
    fn anthropic_body(tool_output: &str) -> String {
        serde_json::json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "run the tests"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "cargo test"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": tool_output}
                ]},
            ]
        })
        .to_string()
    }

    fn openai_body(tool_output: &str) -> String {
        serde_json::json!({
            "model": "gpt-5",
            "messages": [
                {"role": "user", "content": "run the tests"},
                {"role": "tool", "tool_call_id": "call_1", "content": tool_output},
            ]
        })
        .to_string()
    }

    fn responses_body(tool_output: &str) -> String {
        serde_json::json!({
            "model": "gpt-5-codex",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run the tests"}]},
                {"type": "function_call_output", "call_id": "call_1", "output": tool_output},
            ]
        })
        .to_string()
    }

    #[test]
    fn anthropic_tool_result_bytes_are_a_share_of_the_message_bytes() {
        let mut seen = SeenBlocks::default();
        let got = measure_against(
            anthropic_body("all 42 tests passed").as_bytes(),
            &mut seen,
            350,
        )
        .expect("anthropic shape recognized");
        assert!(got.tool_result_bytes > 0, "the tool_result block counted");
        assert!(
            got.message_bytes > got.tool_result_bytes,
            "the block is part of the messages payload, not all of it"
        );
        assert_eq!(
            got.tool_result_resent_bytes, 0,
            "first send is not a re-send"
        );
    }

    #[test]
    fn openai_tool_role_message_counts_as_tool_output() {
        let mut seen = SeenBlocks::default();
        let got = measure_against(
            openai_body("all 42 tests passed").as_bytes(),
            &mut seen,
            350,
        )
        .expect("openai");
        assert!(got.tool_result_bytes > 0);
        assert!(got.message_bytes > got.tool_result_bytes);
    }

    #[test]
    fn responses_function_call_output_counts_as_tool_output() {
        let mut seen = SeenBlocks::default();
        let got = measure_against(
            responses_body("all 42 tests passed").as_bytes(),
            &mut seen,
            350,
        )
        .expect("responses");
        assert!(got.tool_result_bytes > 0);
        assert!(got.message_bytes > got.tool_result_bytes);
    }

    /// The headline behaviour: a block is fresh the first time the session
    /// sends it and mechanical re-send every time after.
    #[test]
    fn a_block_is_resent_on_the_second_request_only() {
        for body in [
            anthropic_body("a long tool output"),
            openai_body("a long tool output"),
            responses_body("a long tool output"),
        ] {
            let mut seen = SeenBlocks::default();
            let first = measure_against(body.as_bytes(), &mut seen, 350).expect("first");
            assert_eq!(first.tool_result_resent_bytes, 0, "first request is fresh");

            let second = measure_against(body.as_bytes(), &mut seen, 350).expect("second");
            assert_eq!(
                second.tool_result_resent_bytes, second.tool_result_bytes,
                "the identical block is wholly a re-send the second time"
            );
        }
    }

    /// Two different sessions are independent: one session's block is not a
    /// re-send for another.
    #[test]
    fn seen_sets_are_per_session() {
        let mut store = SeenStore::default();
        let body = anthropic_body("shared output");
        let a = measure_against(body.as_bytes(), store.entry("session-a"), 350).expect("a");
        let b = measure_against(body.as_bytes(), store.entry("session-b"), 350).expect("b");
        assert_eq!(a.tool_result_resent_bytes, 0);
        assert_eq!(
            b.tool_result_resent_bytes, 0,
            "another session is not a re-send"
        );
        let again =
            measure_against(body.as_bytes(), store.entry("session-a"), 350).expect("a again");
        assert_eq!(again.tool_result_resent_bytes, again.tool_result_bytes);
    }

    #[test]
    fn a_block_over_the_line_threshold_is_counted_as_large() {
        let short = "line\n".repeat(3);
        let long = "line\n".repeat(12);
        for (output, expected) in [(short, 0), (long, 1)] {
            let mut seen = SeenBlocks::default();
            let got = measure_against(anthropic_body(&output).as_bytes(), &mut seen, 10)
                .expect("measured");
            assert_eq!(got.large_tool_results, expected);
        }
    }

    /// A block whose content is a list of blocks (Anthropic's richer
    /// tool_result form) still has its lines counted.
    #[test]
    fn a_structured_tool_result_payload_is_flattened_for_the_line_count() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [{"type": "text", "text": "a\nb\nc\nd"}],
            }]}]
        })
        .to_string();
        let mut seen = SeenBlocks::default();
        let got = measure_against(body.as_bytes(), &mut seen, 3).expect("measured");
        assert_eq!(got.large_tool_results, 1, "4 lines is over a 3-line bar");
    }

    #[test]
    fn a_body_without_a_conversation_array_is_not_measured() {
        let mut seen = SeenBlocks::default();
        assert!(measure_against(b"not json at all", &mut seen, 350).is_none());
        assert!(measure_against(br#"{"model":"claude-opus-5"}"#, &mut seen, 350).is_none());
    }

    /// A conversation with no tool output measures cleanly as 0% tool
    /// share, rather than being skipped.
    #[test]
    fn a_conversation_without_tool_output_still_measures() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string();
        let mut seen = SeenBlocks::default();
        let got = measure_against(body.as_bytes(), &mut seen, 350).expect("measured");
        assert!(got.message_bytes > 0);
        assert_eq!(got.tool_result_bytes, 0);
        assert_eq!(got.tool_result_share_pct(), Some(0));
        assert_eq!(got.resent_share_pct(), None, "no tool bytes → no ratio");
    }

    /// The per-session cap bounds memory: past it the oldest hashes go, so
    /// a very long session under-reports rather than growing forever.
    #[test]
    fn the_seen_set_is_bounded() {
        let mut seen = SeenBlocks::default();
        for n in 0..(MAX_BLOCKS_PER_SESSION as u64 + 10) {
            seen.observe(n);
        }
        assert_eq!(seen.set.len(), MAX_BLOCKS_PER_SESSION);
        assert_eq!(seen.order.len(), MAX_BLOCKS_PER_SESSION);
        assert!(!seen.set.contains(&0), "the oldest hash was evicted");
        assert!(seen.set.contains(&(MAX_BLOCKS_PER_SESSION as u64 + 9)));
    }

    /// So is the session map — a fleet churning through workspaces can't
    /// pin every one of their seen-sets in proxy memory.
    #[test]
    fn the_session_map_is_bounded_and_lru() {
        let mut store = SeenStore::default();
        store.entry("keeper").observe(1);
        for n in 0..MAX_SESSIONS {
            // Touch the keeper between inserts so it stays most-recent.
            store.entry(&format!("session-{n}"));
            store.entry("keeper");
        }
        assert_eq!(store.sessions.len(), MAX_SESSIONS);
        assert!(
            store.sessions.contains_key("keeper"),
            "a session in active use survives eviction"
        );
        assert!(store.entry("keeper").observe(1), "and keeps its hashes");
    }
}
