//! Cheap-model condensation with a content-addressed cache (#1608).
//!
//! One daemon-side service that turns a large blob of text — a file read, a
//! command's output, a diff — into a short condensed version using the cheap
//! tier of the agent whose request is being served, and caches the result in
//! the store so the same input always yields **byte-identical** output.
//!
//! Byte stability is the contract, not a nicety. Claude Code re-sends the
//! whole conversation each turn with prompt-cache breakpoints; a condensed
//! block whose bytes drift between turns invalidates the cache prefix behind
//! it and costs more than the condensation saves. So:
//!
//! - the cache lives in the store (`condense:` kv prefix), not in memory —
//!   agent processes survive a daemon restart and keep sending the same
//!   blocks, and a restart must not re-derive different bytes;
//! - the key mixes the prompt version, so editing the prompt produces a *new*
//!   key rather than a silently different value under an old one;
//! - concurrent callers on the same key are serialized in-process, so a
//!   fresh block condensed twice at once can't leave two different texts in
//!   two turns.
//!
//! Credentials are never configured here: the summarizer reuses the upstream
//! and the auth headers of the request it is serving, so a metered session
//! condenses with exactly the account it is already spending from.
//!
//! Failure is always pass-through. Every error path returns `Err` and the
//! caller sends the original bytes; the summarizer never blocks an agent.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hyper::header::{HeaderMap, HeaderName};
use lazybox_agents::LlmProvider;
use lazybox_core::PriorityTier;
use lazybox_core::context_hygiene::{CondenseKind, ContextHygiene};
use lazybox_store::Store;
use serde::{Deserialize, Serialize};

/// Ceiling on the condensed reply, in tokens. The point of the exercise is a
/// block an order of magnitude smaller than the original; an unbounded reply
/// from a cheap model can be neither.
const MAX_OUTPUT_TOKENS: u32 = 1024;

/// Request headers copied onto the condense call. An allowlist, not the
/// proxy's hop-by-hop denylist: this is a **new** request, not a forwarded
/// one, so anything describing the agent's own body (`content-length`,
/// `content-type`) or shaping its response (`accept`, `accept-encoding`)
/// would be wrong here. What has to survive is what proves who is paying.
const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "anthropic-version",
    "anthropic-beta",
    "openai-organization",
    "openai-project",
    "openai-beta",
    // Codex in ChatGPT-subscription mode: the account this session bills to.
    "chatgpt-account-id",
];

/// A condensed blob, exactly as it should be substituted for the original.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condensed {
    /// The replacement text: the byte-stable `[condensed by lazybox: …]`
    /// header from [`lazybox_core::context_hygiene::render_condensed`] followed by the
    /// summary. Cached verbatim — callers substitute it as-is.
    pub text: String,
    /// Size of the input this replaced, so a caller can report what it saved.
    pub original_bytes: usize,
    /// The cheap model that produced it, for cost attribution and for the
    /// "which tier condensed this?" question a shadow-mode report asks.
    pub model: String,
}

/// Why a condensation did not happen. Every variant means the same thing to
/// a caller — send the original bytes — and is distinguished only so the
/// daemon log says which bound was hit.
#[derive(Debug, thiserror::Error)]
pub enum SummarizeError {
    /// The agent declares no `low` tier and no `condense_model` fallback is
    /// configured, so there is no cheap model to condense with.
    #[error("no cheap model for agent `{0}`")]
    NoModel(String),
    /// The upstream could not be reached, or the connection failed mid-call.
    #[error("upstream request failed: {0}")]
    Upstream(String),
    /// The upstream answered, but not with success — rate limit, 5xx, a
    /// rejected model id.
    #[error("upstream returned HTTP {0}")]
    Status(u16),
    /// The whole call (connect, send, read) outran its budget.
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    /// The upstream's body was not the JSON shape this provider documents.
    #[error("could not read the upstream reply: {0}")]
    Malformed(String),
    /// The call succeeded and produced nothing usable. Substituting an empty
    /// block for real content would silently delete it, so this is an error.
    #[error("upstream produced no text")]
    Empty,
}

/// The in-flight request a condensation is being performed on behalf of:
/// which agent asked (which picks the cheap tier), where that agent's
/// traffic goes, and the credentials it presented.
///
/// Constructed per request by the proxy. Holding the credentials here rather
/// than in the [`Summarizer`] is what makes "no second credential to
/// configure" true: the condense call bills the same account as the request
/// that triggered it.
#[derive(Debug, Clone)]
pub struct ServedRequest {
    agent_id: String,
    provider: LlmProvider,
    base: String,
    headers: HeaderMap,
}

impl ServedRequest {
    /// Bind a condensation to the request being served. `base` is the
    /// upstream the proxy resolved for it (vendor or the user's gateway);
    /// `request_headers` are the agent's own, filtered to the credentials.
    pub fn new(
        agent_id: impl Into<String>,
        provider: LlmProvider,
        base: impl Into<String>,
        request_headers: &HeaderMap,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            provider,
            base: base.into(),
            headers: credential_headers(request_headers),
        }
    }
}

fn credential_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in CREDENTIAL_HEADERS {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        for value in src.get_all(&name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// The condensation service. Cheap to clone — every clone shares the same
/// store cache and the same in-process single-flight map.
#[derive(Clone)]
pub struct Summarizer {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn Store>,
    client: reqwest::Client,
    config: Arc<lazybox_config::Config>,
    /// Snapshot of `agent.context_hygiene`, the one policy both enforcement
    /// points read. Held whole so the cache identity here is literally the
    /// same call #1610's hook makes.
    policy: ContextHygiene,
    /// Per-key gates, so two callers that miss the cache on the same content
    /// at the same time make one model call and read the same bytes back.
    /// Entries are dropped once nobody is waiting on them.
    inflight: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Summarizer {
    /// Build the service over the daemon's store and the proxy's HTTP
    /// client (shared so condense calls reuse its connection pool).
    pub fn new(
        store: Arc<dyn Store>,
        client: reqwest::Client,
        config: Arc<lazybox_config::Config>,
    ) -> Self {
        let inner = Inner {
            store,
            client,
            policy: config.agent.context_hygiene.clone(),
            config,
            inflight: tokio::sync::Mutex::new(HashMap::new()),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// The cheap model `agent_id` condenses with: the `low` tier of its own
    /// model ladder, so a Claude session condenses with Haiku and a Codex
    /// session with whatever cheap tier it declares. Falls back to the
    /// configured `condense_model` when the agent maps no low tier.
    pub fn model_for(&self, agent_id: &str) -> Option<String> {
        let models = self.inner.config.agent_models(agent_id);
        models
            .alias_for_priority(PriorityTier::Low)
            .and_then(|alias| models.tier(alias))
            .and_then(|tier| tier.model_id())
            .map(str::to_string)
            .or_else(|| {
                self.inner
                    .policy
                    .condense_model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_string)
            })
    }

    /// Condense `input`, from cache when it has been seen before and from
    /// one cheap-model call when it has not. Byte-stable forever after.
    pub async fn condense(
        &self,
        served: &ServedRequest,
        input: &str,
        kind: CondenseKind,
    ) -> Result<Condensed, SummarizeError> {
        let model = self
            .model_for(&served.agent_id)
            .ok_or_else(|| SummarizeError::NoModel(served.agent_id.clone()))?;
        let key = self.inner.policy.cache_kv_key(input, &kind, &model);

        if let Some(hit) = self.cached(&key).await {
            return Ok(hit);
        }

        let gate = self.gate(&key).await;
        let result = {
            let _held = gate.lock().await;
            match self.cached(&key).await {
                Some(hit) => Ok(hit),
                None => self.fetch(served, &model, input, &kind, &key).await,
            }
        };
        drop(gate);
        self.release_gate(&key).await;
        result
    }

    async fn fetch(
        &self,
        served: &ServedRequest,
        model: &str,
        input: &str,
        kind: &CondenseKind,
        key: &str,
    ) -> Result<Condensed, SummarizeError> {
        let prompt = build_prompt(kind, input, self.inner.policy.condense_input_cap_bytes);
        let summary = self.call_upstream(served, model, &prompt).await?;
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(SummarizeError::Empty);
        }
        let condensed = Condensed {
            text: lazybox_core::context_hygiene::render_condensed(
                kind,
                input.lines().count(),
                summary,
            ),
            original_bytes: input.len(),
            model: model.to_string(),
        };
        self.store_cached(key, &condensed).await;
        Ok(condensed)
    }

    async fn call_upstream(
        &self,
        served: &ServedRequest,
        model: &str,
        prompt: &str,
    ) -> Result<String, SummarizeError> {
        let budget = Duration::from_millis(self.inner.policy.condense_timeout_ms);
        match tokio::time::timeout(budget, self.request(served, model, prompt)).await {
            Ok(result) => result,
            Err(_) => Err(SummarizeError::Timeout(budget)),
        }
    }

    async fn request(
        &self,
        served: &ServedRequest,
        model: &str,
        prompt: &str,
    ) -> Result<String, SummarizeError> {
        let base = served.base.trim_end_matches('/');
        let (url, body) = match served.provider {
            LlmProvider::Anthropic => (
                format!("{base}/v1/messages"),
                serde_json::json!({
                    "model": model,
                    "max_tokens": MAX_OUTPUT_TOKENS,
                    "messages": [{"role": "user", "content": prompt}],
                }),
            ),
            LlmProvider::OpenAI => (
                format!("{base}/responses"),
                serde_json::json!({
                    "model": model,
                    "input": prompt,
                    "max_output_tokens": MAX_OUTPUT_TOKENS,
                    "stream": false,
                }),
            ),
        };

        let response = self
            .inner
            .client
            .post(&url)
            .headers(served.headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|error| SummarizeError::Upstream(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(SummarizeError::Status(status.as_u16()));
        }
        let body = response
            .text()
            .await
            .map_err(|error| SummarizeError::Upstream(error.to_string()))?;
        extract_text(served.provider, &body)
    }

    async fn cached(&self, key: &str) -> Option<Condensed> {
        let key = key.to_string();
        match crate::store_blocking(&self.inner.store, move |store| store.get_kv(&key)).await {
            Ok(Some(raw)) => serde_json::from_str(&raw).ok(),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!("condense: cache read failed: {error}");
                None
            }
        }
    }

    async fn store_cached(&self, key: &str, value: &Condensed) {
        let key = key.to_string();
        if let Ok(json) = serde_json::to_string(value)
            && let Err(error) =
                crate::store_blocking(&self.inner.store, move |store| store.set_kv(&key, &json))
                    .await
        {
            tracing::warn!("condense: cache write failed: {error}");
        }
    }

    async fn gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut inflight = self.inner.inflight.lock().await;
        inflight.entry(key.to_string()).or_default().clone()
    }

    /// Drop a gate nobody else holds. The map lock is what makes the count
    /// meaningful: a new waiter can only clone the entry while holding it.
    async fn release_gate(&self, key: &str) {
        let mut inflight = self.inner.inflight.lock().await;
        if inflight
            .get(key)
            .is_some_and(|gate| Arc::strong_count(gate) == 1)
        {
            inflight.remove(key);
        }
    }
}

/// The instruction handed to the cheap model, minus the content itself.
///
/// Every byte of this is part of what a cached entry *means*. Editing it
/// without bumping `agent.context_hygiene.prompt_version` would leave old
/// keys resolving to text this prompt would never have produced — which is
/// the one failure the version stamp exists to prevent.
const PROMPT_PREAMBLE: &str = "\
Condense the content below for another AI coding agent that will keep working \
from your summary alone.

Keep verbatim anything that agent would have to quote exactly: identifiers, \
paths, line numbers, signatures, error text, and numbers. Drop boilerplate, \
repetition, and decoration. Never invent content you were not given.

Reply with the condensed text only — no preamble, no sign-off, and no code \
fence around the whole reply.";

fn build_prompt(kind: &CondenseKind, input: &str, cap: usize) -> String {
    format!(
        "{PROMPT_PREAMBLE}\n\n{}\n\n<content>\n{}\n</content>",
        subject_line(kind),
        capped(input, cap),
    )
}

fn subject_line(kind: &CondenseKind) -> String {
    match kind {
        CondenseKind::FileRead { path } => {
            format!("The content is the file `{path}`, as read by a tool call.")
        }
        CondenseKind::CommandOutput { command } => {
            format!("The content is the output of the command `{command}`.")
        }
        CondenseKind::Diff => "The content is a unified diff.".to_string(),
    }
}

/// Trim `input` to at most `cap` bytes on a char boundary, marking what was
/// dropped so the model states a partial summary instead of implying the
/// tail was empty. The cache key hashes the *whole* input, so two blobs that
/// share a capped head still get separate entries.
fn capped(input: &str, cap: usize) -> String {
    if input.len() <= cap {
        return input.to_string();
    }
    let mut end = cap;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = input.len() - end;
    format!(
        "{}\n[lazybox: truncated here; {dropped} further bytes were not shown to you]",
        &input[..end]
    )
}

/// Pull the assistant's text out of a provider's reply. Both providers nest
/// it in a list of content parts, so an unexpected shape yields no text
/// rather than a partial read of the wrong field.
fn extract_text(provider: LlmProvider, body: &str) -> Result<String, SummarizeError> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|error| SummarizeError::Malformed(error.to_string()))?;
    let text = match provider {
        // {"content":[{"type":"text","text":"…"}]}
        LlmProvider::Anthropic => json
            .get("content")
            .and_then(|content| content.as_array())
            .map(|parts| join_text(parts.iter()))
            .unwrap_or_default(),
        // {"output":[{"content":[{"type":"output_text","text":"…"}]}]}
        LlmProvider::OpenAI => json
            .get("output")
            .and_then(|output| output.as_array())
            .map(|items| {
                join_text(
                    items
                        .iter()
                        .filter_map(|item| item.get("content").and_then(|c| c.as_array()))
                        .flatten(),
                )
            })
            .unwrap_or_default(),
    };
    if text.trim().is_empty() {
        return Err(SummarizeError::Empty);
    }
    Ok(text)
}

fn join_text<'a>(parts: impl Iterator<Item = &'a serde_json::Value>) -> String {
    parts
        .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                value.parse().expect("header value"),
            );
        }
        map
    }

    fn summarizer(config: lazybox_config::Config) -> Summarizer {
        Summarizer::new(
            Arc::new(lazybox_store::MemoryStore::new()),
            reqwest::Client::new(),
            Arc::new(config),
        )
    }

    /// The condense call is a NEW request, so only the headers that prove
    /// who is paying come across. Body-describing headers must not: a
    /// forwarded `content-length` would describe the agent's body, not ours.
    #[test]
    fn credential_headers_keep_auth_and_drop_the_rest() {
        let src = headers(&[
            ("authorization", "Bearer secret"),
            ("x-api-key", "sk-test"),
            ("anthropic-version", "2023-06-01"),
            ("chatgpt-account-id", "acct-1"),
            ("content-length", "4096"),
            ("content-type", "application/json"),
            ("accept-encoding", "gzip"),
            ("x-stainless-lang", "js"),
        ]);
        let kept = credential_headers(&src);
        assert_eq!(kept["authorization"], "Bearer secret");
        assert_eq!(kept["x-api-key"], "sk-test");
        assert_eq!(kept["anthropic-version"], "2023-06-01");
        assert_eq!(kept["chatgpt-account-id"], "acct-1");
        assert!(!kept.contains_key("content-length"));
        assert!(!kept.contains_key("content-type"));
        assert!(!kept.contains_key("accept-encoding"));
        assert!(!kept.contains_key("x-stainless-lang"));
    }

    #[test]
    fn capped_passes_short_input_through_untouched() {
        assert_eq!(capped("hello", 64), "hello");
        assert_eq!(capped("hello", 5), "hello");
    }

    #[test]
    fn capped_truncates_on_a_char_boundary_and_says_how_much_it_dropped() {
        // A 4-byte char straddling the cap must not be split — slicing a
        // `str` mid-codepoint panics, and this is fed real file bytes.
        let input = format!("{}🙂tail", "a".repeat(10));
        let out = capped(&input, 11);
        assert!(out.starts_with(&"a".repeat(10)));
        assert!(!out.contains('🙂'));
        assert!(
            out.contains("8 further bytes were not shown"),
            "marker names the dropped byte count: {out}"
        );
    }

    #[test]
    fn extract_text_reads_the_anthropic_content_parts() {
        let body = r#"{"content":[{"type":"text","text":"con"},{"type":"text","text":"densed"}]}"#;
        assert_eq!(
            extract_text(LlmProvider::Anthropic, body).expect("text"),
            "condensed"
        );
    }

    #[test]
    fn extract_text_reads_the_openai_output_parts() {
        let body = r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"condensed"}]}]}"#;
        assert_eq!(
            extract_text(LlmProvider::OpenAI, body).expect("text"),
            "condensed"
        );
    }

    /// A reply that parses but carries no text is an error, never an empty
    /// `Condensed` — substituting an empty block for real content would
    /// silently delete it.
    #[test]
    fn extract_text_rejects_empty_and_unparseable_replies() {
        assert!(matches!(
            extract_text(LlmProvider::Anthropic, r#"{"content":[]}"#),
            Err(SummarizeError::Empty)
        ));
        assert!(matches!(
            extract_text(LlmProvider::Anthropic, r#"{"content":[{"text":"   "}]}"#),
            Err(SummarizeError::Empty)
        ));
        assert!(matches!(
            extract_text(LlmProvider::Anthropic, r#"{"error":{"type":"overloaded"}}"#),
            Err(SummarizeError::Empty)
        ));
        assert!(matches!(
            extract_text(LlmProvider::OpenAI, "not json"),
            Err(SummarizeError::Malformed(_))
        ));
    }

    #[test]
    fn build_prompt_names_the_subject_and_fences_the_content() {
        let prompt = build_prompt(
            &CondenseKind::FileRead {
                path: "src/main.rs".into(),
            },
            "fn main() {}",
            1024,
        );
        assert!(prompt.starts_with(PROMPT_PREAMBLE));
        assert!(prompt.contains("the file `src/main.rs`"));
        assert!(prompt.contains("<content>\nfn main() {}\n</content>"));

        let prompt = build_prompt(
            &CondenseKind::CommandOutput {
                command: "cargo test".into(),
            },
            "ok",
            1024,
        );
        assert!(prompt.contains("the command `cargo test`"));

        assert!(build_prompt(&CondenseKind::Diff, "@@ -1 +1 @@", 1024).contains("unified diff"));
    }

    /// Claude's built-in ladder maps `low` to Haiku, so a Claude session
    /// condenses with Haiku without any configuration at all.
    #[test]
    fn model_for_takes_the_agents_own_low_tier() {
        let summarizer = summarizer(lazybox_config::Config::default());
        assert_eq!(
            summarizer.model_for("claude").as_deref(),
            Some("claude-haiku-4-5")
        );
    }

    /// An agent with no low tier (Codex ships no built-in menu) falls back
    /// to the configured model, and condenses not at all without one.
    #[test]
    fn model_for_falls_back_to_the_configured_model_then_gives_up() {
        assert_eq!(
            summarizer(lazybox_config::Config::default()).model_for("codex"),
            None
        );

        let mut config = lazybox_config::Config::default();
        config.agent.context_hygiene.condense_model = Some("  gpt-5-mini  ".into());
        assert_eq!(
            summarizer(config).model_for("codex").as_deref(),
            Some("gpt-5-mini"),
            "a configured fallback is trimmed before use"
        );
    }

    /// No model means no call: the caller gets `Err` and sends the original.
    #[tokio::test]
    async fn condense_without_a_model_errors_instead_of_calling_anything() {
        let summarizer = summarizer(lazybox_config::Config::default());
        let served = ServedRequest::new(
            "codex",
            LlmProvider::OpenAI,
            "http://127.0.0.1:1/unreachable",
            &HeaderMap::new(),
        );
        let error = summarizer
            .condense(&served, "some output", CondenseKind::Diff)
            .await
            .expect_err("no low tier, no fallback");
        assert!(matches!(error, SummarizeError::NoModel(agent) if agent == "codex"));
    }

    /// An unreachable upstream is an ordinary pass-through error, and it
    /// must not leave a gate behind — a leaked entry would make the map
    /// grow without bound over a daemon's lifetime.
    #[tokio::test]
    async fn a_failed_call_errors_and_releases_its_gate() {
        let summarizer = summarizer(lazybox_config::Config::default());
        // Port 1 on loopback refuses immediately, so this is a connect
        // failure rather than a wait on the 15s budget.
        let served = ServedRequest::new(
            "claude",
            LlmProvider::Anthropic,
            "http://127.0.0.1:1",
            &HeaderMap::new(),
        );
        let error = summarizer
            .condense(&served, "body", CondenseKind::Diff)
            .await
            .expect_err("upstream refused");
        assert!(matches!(error, SummarizeError::Upstream(_)), "{error:?}");
        assert!(
            summarizer.inner.inflight.lock().await.is_empty(),
            "the gate is dropped once nobody waits on it"
        );
    }
}
