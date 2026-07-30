//! Linear provider — fetches Linear issues as source-agnostic `Task`s.
//!
//! Plugs into the same `TaskProvider` trait the GitHub provider
//! implements, so the TUI treats GitHub PRs, GitHub Issues, and Linear
//! tickets identically in the sidebar.
//!
//! ## Auth
//!
//! Reads the Linear personal API key from the `LINEAR_API_KEY`
//! environment variable. Linear's preferred auth is a bearer token in
//! `Authorization`; we send it without the `Bearer ` prefix per
//! Linear's docs.
//!
//! ## Scope
//!
//! Fetches issues the authenticated user is assigned to or created.
//! States `completed` / `canceled` are filtered out server-side.
//! Pagination support: up to 50 issues per page, up to 20 pages.

pub mod graphql;

use lazybox_auth::{CredentialChain, EnvProvider};
use lazybox_core::{
    DEFAULT_MAX_PAGES, FetchOutcome, FetchPage, FetchPageInfo, ProviderError, Task, TaskProvider,
    paginate,
};
use serde::Serialize;

const LINEAR_GRAPHQL: &str = "https://api.linear.app/graphql";

/// Workspace-key prefix and credential scope this provider owns.
/// Linear workspaces are keyed `"linear-<team>-<id>"`; the mutation
/// router splits on `'-'` and matches the first segment.
pub const SOURCE: &str = "linear";

/// Credential chain Linear uses. Today: just `LINEAR_API_KEY`.
/// Wrapped in a chain so future Keychain / Vault providers slot in
/// transparently — same shape as the GitHub provider.
pub fn credential_chain() -> CredentialChain {
    CredentialChain::new().with(EnvProvider::new("LINEAR_API_KEY"))
}

#[derive(Debug, thiserror::Error)]
pub enum LinearError {
    #[error("missing LINEAR_API_KEY env var")]
    MissingKey,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// HTTP 429 from Linear. `retry_after_secs` carries the
    /// `Retry-After` header when Linear sent one.
    #[error("rate limited (retry after {retry_after_secs:?}s)")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("graphql: {0}")]
    Graphql(String),
}

/// `true` when a `reqwest::Error` is a transport-layer failure that
/// carried no HTTP status — connect, DNS, TLS, timeout, or a
/// body/decode error. These never reached Linear's app layer, so a
/// fresh attempt next tick is safe. Status errors (`is_status()`)
/// return `false` here; the shared classifier reads their status code
/// directly instead.
fn reqwest_is_transport(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() || e.is_decode()
}

impl From<LinearError> for ProviderError {
    fn from(err: LinearError) -> Self {
        const SOURCE: &str = "linear";
        match &err {
            LinearError::MissingKey => ProviderError::auth(SOURCE, err.to_string()),
            LinearError::RateLimited { retry_after_secs } => match retry_after_secs {
                Some(secs) => ProviderError::retryable_after(SOURCE, err.to_string(), *secs),
                None => ProviderError::retryable(SOURCE, err.to_string()),
            },
            LinearError::Http(http_err) => {
                // Route through the ONE shared classifier so Linear
                // and GitHub can't disagree about the same transport
                // failure. Pass the typed reqwest signal — the HTTP
                // status when the response carried one (429 → retry,
                // 401/403 → auth, 5xx → retry), otherwise the
                // transport flag (connect/timeout/body). The display
                // string is only the last-resort fallback; the typed
                // status doesn't reliably hit substring probes.
                let msg = err.to_string();
                let signals = lazybox_core::HttpErrorSignals {
                    status: http_err.status().map(|s| s.as_u16()),
                    transport: reqwest_is_transport(http_err),
                    message: &msg,
                };
                lazybox_core::classify(&signals).into_provider_error(SOURCE, msg.clone())
            }
            LinearError::Graphql(_) => {
                // GraphQL errors arrive in the response *body*, not as
                // an HTTP status — there's no typed signal, so the
                // shared substring fallback is all we have. It still
                // lives in one place, so gh and linear agree on it.
                let msg = err.to_string();
                lazybox_core::classify_message(&msg).into_provider_error(SOURCE, msg.clone())
            }
        }
    }
}

/// Truncate to at most `max` bytes without splitting a UTF-8
/// character. `&text[..200]` panics when byte 200 lands inside a
/// multi-byte char — which a GraphQL error body with non-ASCII
/// content (user names, smart quotes) can trivially hit.
fn truncate_on_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Client for Linear's GraphQL API.
#[derive(Clone)]
pub struct LinearClient {
    http: reqwest::Client,
    api_key: String,
    endpoint: String,
}

impl LinearClient {
    /// Build a client from the `LINEAR_API_KEY` env var. Fails if the
    /// env var isn't set. Kept for back-compat; new call sites should
    /// prefer `from_credential` so future providers (Keychain, Vault,
    /// OAuth refresh) transparently apply.
    pub fn from_env() -> Result<Self, LinearError> {
        let key = std::env::var("LINEAR_API_KEY").map_err(|_| LinearError::MissingKey)?;
        Ok(Self::with_key(key))
    }

    /// Build a client from a resolved `lazybox_auth::Credential`. Matches
    /// the gh-provider shape so server-side polling can drive both
    /// providers through the same credential chain.
    pub fn from_credential(cred: lazybox_auth::Credential) -> Self {
        Self::with_key(cred.into_token())
    }

    /// Build a client with an explicit API key.
    pub fn with_key(api_key: impl Into<String>) -> Self {
        // Explicit overall timeout — reqwest's default has none, so a
        // black-holed connection to Linear would park the polling task
        // forever. 25s clears Linear's slowest GraphQL responses while
        // staying inside the daemon's poll cadence. Builder failure
        // (TLS init) falls back to the default client rather than
        // panicking in a library crate.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(25))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!("linear http client builder failed ({e}); using default client");
                reqwest::Client::new()
            });
        Self {
            http,
            api_key: api_key.into(),
            endpoint: LINEAR_GRAPHQL.to_string(),
        }
    }

    /// Override the GraphQL endpoint. Used by tests to point at a
    /// local mock server.
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into();
        self
    }

    async fn graphql<T: serde::de::DeserializeOwned>(
        &self,
        body: impl Serialize,
    ) -> Result<T, LinearError> {
        let resp = self
            .http
            .post(&self.endpoint)
            .header("authorization", &self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        // 429 gets its own typed error (with the Retry-After hint
        // when present) so the polling layer backs off instead of
        // treating it as permanent. Must run before
        // `error_for_status`, which discards the headers.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            return Err(LinearError::RateLimited { retry_after_secs });
        }
        let resp = resp.error_for_status()?;
        let text = resp.text().await?;
        serde_json::from_str::<T>(&text).map_err(|e| {
            LinearError::Graphql(format!(
                "parse: {e}; body starts with {:?}",
                truncate_on_char_boundary(&text, 200)
            ))
        })
    }

    /// Fetch all open issues for the authenticated viewer (assigned or
    /// created). Paginates. Results are converted to `Task`s.
    ///
    /// Convenience wrapper over [`Self::fetch_all_with_coverage`]
    /// that drops the coverage marker. Prefer the marker-carrying
    /// variant anywhere the result drives workspace *removal*.
    pub async fn fetch_all(&self) -> Result<Vec<Task>, LinearError> {
        self.fetch_all_with_coverage().await.map(|o| o.items)
    }

    /// Like [`Self::fetch_all`], but reports whether the result is
    /// COMPLETE or a partial prefix (a page failed mid-pagination,
    /// or the safety cap truncated the tail). Partial results keep
    /// the inbox alive but are NOT authoritative: a workspace absent
    /// from a partial result may simply live on a page we never got,
    /// so rescope must not delete based on it. `LinearSource` in
    /// `crates/server/src/polling/mod.rs` records this coverage and
    /// downgrades `polled_scope()` to `PolledScope::Repos(vec![])` on
    /// a partial fetch (mirroring `GhSource::last_coverage_partial`).
    pub async fn fetch_all_with_coverage(&self) -> Result<FetchOutcome<Vec<Task>>, LinearError> {
        // 1. Identify the viewer so we can assign TaskRole correctly.
        let viewer_body = serde_json::json!({
            "query": graphql::VIEWER_QUERY,
        });
        let viewer: graphql::ViewerResponse = self.graphql(&viewer_body).await?;
        let viewer_id = viewer
            .data
            .ok_or_else(|| LinearError::Graphql("no viewer data".into()))?
            .viewer
            .id;

        let outcome = paginate(
            |cursor, page| {
                let viewer_id = &viewer_id;
                async move {
                    let body = graphql::build_issues_body(cursor.as_deref());
                    let resp: graphql::IssuesResponse =
                        self.graphql(&body).await.map_err(|error| {
                            tracing::error!("Linear page {page} failed: {error}");
                            error
                        })?;
                    if let Some(errors) = resp.errors {
                        let joined = errors
                            .iter()
                            .map(|e| e.message.as_str())
                            .collect::<Vec<_>>()
                            .join("; ");
                        tracing::error!("Linear GraphQL errors at page {page}: {joined}");
                        return Err(LinearError::Graphql(joined));
                    }
                    let data = resp
                        .data
                        .ok_or_else(|| LinearError::Graphql("no data in issues response".into()))?;
                    let page_info = data.issues.page_info;
                    Ok(FetchPage {
                        items: data
                            .issues
                            .nodes
                            .iter()
                            .map(|issue| graphql::issue_to_task(issue, viewer_id))
                            .collect(),
                        page_info: Some(FetchPageInfo {
                            has_next_page: page_info.has_next_page,
                            end_cursor: page_info.end_cursor,
                        }),
                    })
                }
            },
            DEFAULT_MAX_PAGES,
        )
        .await?;
        if outcome.is_partial() {
            tracing::error!(
                "Linear pagination stopped before completion; returning {} partial issues",
                outcome.items.len()
            );
        }
        Ok(outcome)
    }
}

impl LinearClient {
    /// Post a comment on a Linear issue. `issue_id` is the issue's
    /// UUID node id (stored in `task.node_id` after a poll). Wraps
    /// the `commentCreate` GraphQL mutation; returns on success or
    /// surfaces the upstream error message.
    pub async fn post_comment(&self, issue_id: &str, body: &str) -> Result<(), LinearError> {
        let req = serde_json::json!({
            "query": "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { success } }",
            "variables": { "input": { "issueId": issue_id, "body": body } },
        });
        // The response shape is `{ data: { commentCreate: { success } } }`
        // — we only need to confirm we got a 2xx + no GraphQL errors.
        let resp: serde_json::Value = self.graphql(&req).await?;
        if let Some(errors) = resp.get("errors").and_then(|v| v.as_array())
            && !errors.is_empty()
        {
            let joined = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(LinearError::Graphql(joined));
        }
        Ok(())
    }
}

impl TaskProvider for LinearClient {
    fn name(&self) -> &str {
        "linear"
    }

    async fn fetch_tasks(&self) -> Result<Vec<Task>, ProviderError> {
        self.fetch_all().await.map_err(Into::into)
    }

    fn username(&self) -> Option<&str> {
        None
    }

    /// Post a reply on the workspace's Linear issue. The issue's
    /// UUID lives in `primary_task().node_id` — set by the
    /// fetch-side `issue_to_task` mapper. No reply if the workspace
    /// has no primary task or no node_id (pre-poll state).
    async fn post_reply(
        &self,
        workspace: &lazybox_core::Workspace,
        body: &str,
    ) -> Result<(), ProviderError> {
        let task = workspace.primary_task().ok_or_else(|| {
            ProviderError::permanent(
                "linear",
                format!("workspace {} has no primary task", workspace.key),
            )
        })?;
        let issue_id = task.node_id.as_deref().ok_or_else(|| {
            ProviderError::permanent("linear", "task has no node_id (poll first)")
        })?;
        self.post_comment(issue_id, body)
            .await
            .map_err(|e| ProviderError::permanent("linear", e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `&text[..200]` panicked when byte 200 fell inside a multi-byte
    /// char. The boundary-safe truncation must back off to the
    /// previous char start instead.
    #[test]
    fn truncate_on_char_boundary_never_splits_a_char() {
        // 'é' is 2 bytes; an odd byte limit lands mid-char.
        let s = "ééééé"; // 10 bytes
        let out = truncate_on_char_boundary(s, 5);
        assert_eq!(out, "éé", "5 → backs off to byte 4");
        assert!(s.is_char_boundary(out.len()));

        // Emoji (4 bytes) with the cut inside it.
        let s = "ab🚀cd"; // 'a','b' = 2 bytes, 🚀 = 4 bytes
        assert_eq!(truncate_on_char_boundary(s, 3), "ab");
        assert_eq!(truncate_on_char_boundary(s, 6), "ab🚀");
    }

    #[test]
    fn truncate_on_char_boundary_passes_short_strings_through() {
        assert_eq!(truncate_on_char_boundary("short", 200), "short");
        assert_eq!(truncate_on_char_boundary("", 0), "");
        let exact = "abcd";
        assert_eq!(truncate_on_char_boundary(exact, 4), "abcd");
    }

    #[test]
    fn rate_limited_error_classifies_retryable_with_hint() {
        let err = LinearError::RateLimited {
            retry_after_secs: Some(30),
        };
        match ProviderError::from(err) {
            ProviderError::Retryable {
                retry_after_secs, ..
            } => assert_eq!(retry_after_secs, Some(30)),
            other => panic!("429 must be retryable, got {other:?}"),
        }
        let err = LinearError::RateLimited {
            retry_after_secs: None,
        };
        assert!(matches!(
            ProviderError::from(err),
            ProviderError::Retryable { .. }
        ));
    }

    #[test]
    fn missing_key_is_auth() {
        assert!(ProviderError::from(LinearError::MissingKey).is_auth());
    }

    /// GraphQL errors have no HTTP status, so they route through the
    /// shared `classify_message` fallback in `lazybox-core`. Verifying
    /// the same verdicts the shared classifier gives proves Linear
    /// delegates rather than reimplementing its own keyword list.
    #[test]
    fn graphql_errors_delegate_to_shared_classifier() {
        let cases = [
            ("secondary rate limit exceeded", true, false),
            ("service temporarily unavailable", true, false),
            ("authentication required", false, true),
            ("unauthorized", false, true),
            ("field 'foo' doesn't exist on type 'Query'", false, false),
        ];
        for (msg, retryable, auth) in cases {
            let perr = ProviderError::from(LinearError::Graphql(msg.to_string()));
            // The provider's verdict must match the shared classifier's.
            let shared =
                lazybox_core::classify_message(msg).into_provider_error("linear", msg.to_string());
            assert_eq!(
                perr.is_retryable(),
                shared.is_retryable(),
                "retryable mismatch for {msg:?}"
            );
            assert_eq!(
                perr.is_auth(),
                shared.is_auth(),
                "auth mismatch for {msg:?}"
            );
            assert_eq!(perr.is_retryable(), retryable, "retryable for {msg:?}");
            assert_eq!(perr.is_auth(), auth, "auth for {msg:?}");
        }
    }
}
