//! Request-side context accounting cost: what the proxy pays to turn one
//! buffered request body into a `ContextAccounting`. It runs inline in
//! `handle` — synchronously, on the reactor, before the upstream request is
//! issued — so every microsecond here is latency added to every request a
//! metered agent makes.
//!
//! This bench exists because a `perf:` change to that path shipped a 26-39%
//! regression with the test suite fully green: the conversation's counting
//! sink was swapped for one that also SipHashed every byte and dropped the
//! digest. Every value assertion still passed, because the number was right.
//! `sink_only` is the comparison that would have caught it — the sink
//! `measure` counts through, head to head with the `to_string().len()` it
//! replaced.
//!
//! It benches `conversation_bytes` rather than `measure`, and that is not a
//! shortcut: #1666 made `proxy::measure_then_compact` the single seam that
//! parses once, measures, then rewrites, precisely so the order cannot be
//! got wrong at a call site. Publishing `measure` to reach it from here
//! would reopen the second path that commit closed. `conversation_bytes` is
//! order-free — it serializes and counts — so exposing it costs nothing,
//! and after #1666 moved the parse out, it is the bulk of what `measure`
//! does anyway.
//!
//! Sizes span the realistic range: a short session, a ~200k-token context
//! (~1 MB of JSON, the top of Claude's standard window), and a long one.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lazybox_server::proxy::{conversation, conversation_bytes};
use serde_json::Value;
use std::hint::black_box;

/// An Anthropic Messages body of `turns` tool-using turns, each carrying a
/// ~40 KB tool result — the shape that dominates a real agent session.
fn body(turns: usize) -> Vec<u8> {
    let payload: String = (0..640)
        .map(|i| format!("line {i} of tool output here\n"))
        .collect();
    let mut messages = Vec::new();
    for turn in 0..turns {
        messages.push(serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": format!("turn {turn}")}],
        }));
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": format!("toolu_{turn}"),
                "name": "Bash",
                "input": {"command": "cargo test"},
            }],
        }));
        messages.push(serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": format!("toolu_{turn}"),
                "content": payload.clone(),
            }],
        }));
    }
    serde_json::to_vec(&serde_json::json!({"model": "claude-opus-5", "messages": messages}))
        .expect("serialize fixture")
}

fn context_parse(c: &mut Criterion) {
    let mut sink = c.benchmark_group("sink_only");
    for turns in [10_usize, 40, 140] {
        let bytes = body(turns);
        let value: Value = serde_json::from_slice(&bytes).expect("fixture is json");
        let array = conversation(&value).expect("fixture has a conversation");
        let label = format!("{}KB", bytes.len() / 1024);
        sink.throughput(criterion::Throughput::Bytes(bytes.len() as u64));

        // What `measure` does today.
        sink.bench_with_input(BenchmarkId::new("counted", &label), array, |b, array| {
            b.iter(|| black_box(conversation_bytes(black_box(array))));
        });
        // What it did before, kept as the baseline the regression lacked.
        sink.bench_with_input(
            BenchmarkId::new("materialized", &label),
            array,
            |b, array| {
                b.iter(|| black_box(serde_json::to_string(black_box(array)).map(|s| s.len())));
            },
        );
    }
    sink.finish();
}

criterion_group!(benches, context_parse);
criterion_main!(benches);
