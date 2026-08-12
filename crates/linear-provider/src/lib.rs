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
//! Fetches issues the authenticated user is assigned to or created, by
//! default. Subscription coverage is opt-in via `providers.linear.scope`
//! (`[assigned, created, subscribed]`) because Linear auto-subscribes you
//! aggressively — team defaults, opening an issue — so a subscriber clause
//! floods the inbox with unrelated issues. See [`LinearScope`] and
//! [`LinearClient::with_scope`]. States `completed` / `canceled` are
//! filtered out server-side. Pagination support: up to 50 issues per
//! page, up to 20 pages.

pub mod graphql;

use lazybox_auth::{CommandProvider, CredentialChain, EnvProvider};
use lazybox_core::{
    DEFAULT_MAX_PAGES, FetchOutcome, FetchPage, FetchPageInfo, LinearScope, ProviderError, Task,
    TaskProvider, paginate,
};
use serde::Serialize;

const LINEAR_GRAPHQL: &str = "https://api.linear.app/graphql";

/// Workspace-key prefix and credential scope this provider owns.
/// Linear workspaces are keyed `"linear-<team>-<id>"`; the mutation
/// router splits on `'-'` and matches the first segment. The value comes
/// from `lazybox_core` so config's snippet scoping and the UI can't drift
/// from it.
pub const SOURCE: &str = lazybox_core::LINEAR_SOURCE;

/// Credential chain Linear uses: the `LINEAR_API_KEY` env var, then a
/// fallback to `linear auth token` (the `schpet/linear-cli` binary,
/// which stores its token in the system keyring). This mirrors the
/// GitHub provider, where `gh auth token` backstops the env vars — so
/// a user who authenticated through the Linear CLI is detected without
/// having to also export the key by hand. Future Keychain / Vault
/// providers slot in the same way.
pub fn credential_chain() -> CredentialChain {
    CredentialChain::new()
        .with(EnvProvider::new("LINEAR_API_KEY"))
        .with(CommandProvider::new("linear", &["auth", "token"]))
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
    /// Which `or` clauses the issues query requests. Defaults to
    /// [`LinearScope::default_scopes`] (assigned + created, no
    /// subscriber flood); override via [`Self::with_scope`] from
    /// `providers.linear.scope`.
    scope: Vec<LinearScope>,
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
            scope: LinearScope::default_scopes(),
        }
    }

    /// Override the GraphQL endpoint. Used by tests to point at a
    /// local mock server.
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into();
        self
    }

    /// Set which issues the query requests (`providers.linear.scope`).
    /// An empty slice falls back to [`LinearScope::default_scopes`] so
    /// the query is never an unscoped whole-workspace sweep.
    pub fn with_scope(mut self, scope: Vec<LinearScope>) -> Self {
        self.scope = scope;
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
                    let body = graphql::build_issues_body(cursor.as_deref(), &self.scope);
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
        let outcome = outcome.into_fetch_outcome();
        if outcome.is_partial() {
            tracing::error!(
                "Linear pagination stopped before completion; returning {} partial issues",
                outcome.items.len()
            );
        }
        Ok(outcome)
    }
}

/// Join a GraphQL response body's `errors` array into one message, or
/// `None` when the mutation carried no errors. GraphQL surfaces
/// mutation failures in the response body (HTTP 200), not the status,
/// so every mutation checks this before declaring success.
fn gql_errors(resp: &serde_json::Value) -> Option<String> {
    let errors = resp.get("errors").and_then(|v| v.as_array())?;
    if errors.is_empty() {
        return None;
    }
    Some(
        errors
            .iter()
            .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
            .collect::<Vec<_>>()
            .join("; "),
    )
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
        if let Some(msg) = gql_errors(&resp) {
            return Err(LinearError::Graphql(msg));
        }
        Ok(())
    }

    /// Resolve a Linear user's display name or email to their UUID.
    /// The assignee picker's candidate strings are Linear display
    /// names (Linear has no GitHub-style login), so a mutation must
    /// map the picked name back to the id `issueUpdate` expects.
    /// Matches `name`, `displayName`, or `email` case-insensitively.
    ///
    /// `Ok(None)` when nobody matches. Display names are NOT unique in
    /// Linear, so when more than one distinct user matches we refuse
    /// rather than assign an arbitrary one — silently picking the first
    /// would assign the wrong person. `Err` names the ambiguity so the
    /// caller can retype a unique identifier (e.g. the email).
    pub async fn resolve_user_id(&self, name: &str) -> Result<Option<String>, LinearError> {
        let req = serde_json::json!({
            "query": "query { users(first: 250) { nodes { id name displayName email } } }",
        });
        let resp: serde_json::Value = self.graphql(&req).await?;
        if let Some(msg) = gql_errors(&resp) {
            return Err(LinearError::Graphql(msg));
        }
        let target = name.trim().to_lowercase();
        let Some(nodes) = resp
            .get("data")
            .and_then(|d| d.get("users"))
            .and_then(|u| u.get("nodes"))
            .and_then(|n| n.as_array())
        else {
            return Ok(None);
        };
        let matched: Vec<&str> = nodes
            .iter()
            .filter(|node| {
                ["name", "displayName", "email"].iter().any(|field| {
                    node.get(*field)
                        .and_then(|v| v.as_str())
                        .is_some_and(|v| v.to_lowercase() == target)
                })
            })
            .filter_map(|node| node.get("id").and_then(|v| v.as_str()))
            .collect();
        match matched.as_slice() {
            [] => Ok(None),
            [id] => Ok(Some((*id).to_string())),
            _ => Err(LinearError::Graphql(format!(
                "`{name}` matches {} Linear users; use a unique email to disambiguate",
                matched.len()
            ))),
        }
    }

    /// Set (or clear, with `None`) the single assignee on a Linear
    /// issue via `issueUpdate`. Linear issues hold at most one
    /// assignee, so this replaces rather than appends.
    pub async fn set_assignee(
        &self,
        issue_id: &str,
        assignee_id: Option<&str>,
    ) -> Result<(), LinearError> {
        let req = serde_json::json!({
            "query": "mutation($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success } }",
            "variables": { "id": issue_id, "input": { "assigneeId": assignee_id } },
        });
        let resp: serde_json::Value = self.graphql(&req).await?;
        if let Some(msg) = gql_errors(&resp) {
            return Err(LinearError::Graphql(msg));
        }
        Ok(())
    }

    /// Move a Linear issue to a workflow-state id via `issueUpdate`.
    /// State ids are per-team, so callers resolve one from the issue's
    /// own team (see [`Self::close_issue_by_id`]).
    pub async fn move_issue_to_state(
        &self,
        issue_id: &str,
        state_id: &str,
    ) -> Result<(), LinearError> {
        let req = serde_json::json!({
            "query": "mutation($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success } }",
            "variables": { "id": issue_id, "input": { "stateId": state_id } },
        });
        let resp: serde_json::Value = self.graphql(&req).await?;
        if let Some(msg) = gql_errors(&resp) {
            return Err(LinearError::Graphql(msg));
        }
        Ok(())
    }

    /// Close a Linear issue by moving it to a `canceled`-type workflow
    /// state — the analog of GitHub's "close as not planned" that the
    /// `x c` action triggers. Resolves a canceled state from the
    /// issue's own team (state ids are per-team), so nothing team-
    /// specific needs threading in. Idempotent: an issue already in a
    /// completed or canceled state is a no-op.
    pub async fn close_issue_by_id(&self, issue_id: &str) -> Result<(), LinearError> {
        let req = serde_json::json!({
            "query": "query($id: String!) { issue(id: $id) { state { type } team { states { nodes { id type } } } } }",
            "variables": { "id": issue_id },
        });
        let resp: serde_json::Value = self.graphql(&req).await?;
        if let Some(msg) = gql_errors(&resp) {
            return Err(LinearError::Graphql(msg));
        }
        let issue = resp
            .get("data")
            .and_then(|d| d.get("issue"))
            .filter(|i| !i.is_null())
            .ok_or_else(|| LinearError::Graphql(format!("issue {issue_id} not found")))?;
        let current_type = issue
            .get("state")
            .and_then(|s| s.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        if matches!(current_type, "completed" | "canceled") {
            return Ok(());
        }
        let state_id = issue
            .get("team")
            .and_then(|t| t.get("states"))
            .and_then(|s| s.get("nodes"))
            .and_then(|n| n.as_array())
            .into_iter()
            .flatten()
            .find(|s| s.get("type").and_then(|t| t.as_str()) == Some("canceled"))
            .and_then(|s| s.get("id").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                LinearError::Graphql("issue team has no canceled workflow state".into())
            })?;
        self.move_issue_to_state(issue_id, state_id).await
    }

    /// The Linear issue UUID backing a workspace's primary task.
    /// `permanent` error when the workspace has no polled task or its
    /// task carries no `node_id` (pre-poll state).
    fn issue_id_for(&self, workspace: &lazybox_core::Workspace) -> Result<String, ProviderError> {
        let task = workspace.primary_task().ok_or_else(|| {
            ProviderError::permanent(
                "linear",
                format!("workspace {} has no primary task", workspace.key),
            )
        })?;
        task.node_id
            .clone()
            .ok_or_else(|| ProviderError::permanent("linear", "task has no node_id (poll first)"))
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
        let issue_id = self.issue_id_for(workspace)?;
        self.post_comment(&issue_id, body)
            .await
            .map_err(|e| ProviderError::permanent("linear", e.to_string()))
    }

    /// Close the workspace's Linear issue by moving it to a canceled
    /// workflow state — the `x c` close-issue analog (see
    /// [`Self::close_issue_by_id`]).
    async fn close_issue(&self, workspace: &lazybox_core::Workspace) -> Result<(), ProviderError> {
        let issue_id = self.issue_id_for(workspace)?;
        self.close_issue_by_id(&issue_id)
            .await
            .map_err(|e| ProviderError::permanent("linear", e.to_string()))
    }

    /// Assign the Linear issue. Linear issues hold a single assignee,
    /// so "add" replaces with the named user — [`Self::set_assignees`]
    /// carries the actual logic.
    async fn add_assignees(
        &self,
        workspace: &lazybox_core::Workspace,
        logins: &[String],
    ) -> Result<(), ProviderError> {
        self.set_assignees(workspace, logins).await
    }

    /// Replace the Linear issue's assignee. `logins` are Linear
    /// display names (what the picker offers). Linear issues hold a
    /// single assignee, so the LAST login wins — the picker lists the
    /// existing assignee first, so a user who *adds* a name (leaving
    /// the current one checked) still reassigns to the one they picked
    /// rather than silently keeping the old one. An empty set clears
    /// the assignee.
    async fn set_assignees(
        &self,
        workspace: &lazybox_core::Workspace,
        logins: &[String],
    ) -> Result<(), ProviderError> {
        let issue_id = self.issue_id_for(workspace)?;
        let assignee_id = match logins.last() {
            Some(login) => Some(
                self.resolve_user_id(login)
                    .await
                    .map_err(|e| ProviderError::permanent("linear", e.to_string()))?
                    .ok_or_else(|| {
                        ProviderError::permanent("linear", format!("user `{login}` not found"))
                    })?,
            ),
            None => None,
        };
        self.set_assignee(&issue_id, assignee_id.as_deref())
            .await
            .map_err(|e| ProviderError::permanent("linear", e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end: with no `LINEAR_API_KEY` in the environment, the
    /// credential chain must still resolve a token by shelling out to
    /// `linear auth token` (the `schpet/linear-cli` keyring). This is
    /// what makes a CLI-only Linear login show up as authenticated in
    /// setup detection and poll. Ignored by default because it needs a
    /// real `linear` login on the machine; run with
    /// `cargo test -p lazybox-linear -- --ignored linear_cli_auth`.
    #[tokio::test]
    #[ignore = "requires a local `linear auth login`"]
    async fn linear_cli_auth_resolves_through_credential_chain() {
        // SAFETY: single-threaded ignored test; no other thread reads
        // this var concurrently.
        unsafe {
            std::env::remove_var("LINEAR_API_KEY");
        }
        let cred = credential_chain()
            .resolve(SOURCE)
            .await
            .expect("linear CLI token should resolve via `linear auth token`");
        assert!(
            cred.token().starts_with("lin_"),
            "expected a Linear API token, got source {}",
            cred.source
        );
    }

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
