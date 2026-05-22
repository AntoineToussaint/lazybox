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

use pilot_auth::{CredentialChain, EnvProvider};
use pilot_core::{ProviderError, Task, TaskProvider};
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
    #[error("graphql: {0}")]
    Graphql(String),
}

impl From<LinearError> for ProviderError {
    fn from(err: LinearError) -> Self {
        const SOURCE: &str = "linear";
        match &err {
            LinearError::MissingKey => ProviderError::auth(SOURCE, err.to_string()),
            LinearError::Http(_) => {
                let s = err.to_string().to_lowercase();
                if s.contains("401") || s.contains("403") || s.contains("unauthorized") {
                    ProviderError::auth(SOURCE, err.to_string())
                } else if s.contains("timeout")
                    || s.contains("connection")
                    || s.contains("network")
                    || s.contains("502")
                    || s.contains("503")
                    || s.contains("504")
                {
                    ProviderError::retryable(SOURCE, err.to_string())
                } else {
                    ProviderError::permanent(SOURCE, err.to_string())
                }
            }
            LinearError::Graphql(_) => {
                let s = err.to_string().to_lowercase();
                if s.contains("rate limit") || s.contains("temporarily") {
                    ProviderError::retryable(SOURCE, err.to_string())
                } else if s.contains("authentication") || s.contains("unauthorized") {
                    ProviderError::auth(SOURCE, err.to_string())
                } else {
                    ProviderError::permanent(SOURCE, err.to_string())
                }
            }
        }
    }
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

    /// Build a client from a resolved `pilot_auth::Credential`. Matches
    /// the gh-provider shape so server-side polling can drive both
    /// providers through the same credential chain.
    pub fn from_credential(cred: pilot_auth::Credential) -> Self {
        Self::with_key(cred.into_token())
    }

    /// Build a client with an explicit API key.
    pub fn with_key(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
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
            .await?
            .error_for_status()?;
        let text = resp.text().await?;
        serde_json::from_str::<T>(&text).map_err(|e| {
            LinearError::Graphql(format!(
                "parse: {e}; body starts with {:?}",
                &text[..text.len().min(200)]
            ))
        })
    }

    /// Fetch all open issues for the authenticated viewer (assigned or
    /// created). Paginates. Results are converted to `Task`s.
    pub async fn fetch_all(&self) -> Result<Vec<Task>, LinearError> {
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

        // 2. Page through issues. If page 1 fails outright we
        //    surface the error; if a later page fails we return
        //    what we have plus a warning log — losing pages N+1..
        //    silently was the prior behavior and easy to miss.
        let mut tasks = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            let body = graphql::build_issues_body(cursor.as_deref());
            let resp: graphql::IssuesResponse = match self.graphql(&body).await {
                Ok(r) => r,
                Err(e) if page > 0 => {
                    tracing::error!(
                        "Linear page {page} failed mid-pagination; \
                         returning {} partial issues. error: {e}",
                        tasks.len()
                    );
                    break;
                }
                Err(e) => return Err(e),
            };
            if let Some(errors) = resp.errors {
                let joined = errors
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                if page > 0 {
                    tracing::error!(
                        "Linear GraphQL errors at page {page}; returning {} partial issues. errors: {joined}",
                        tasks.len()
                    );
                    break;
                }
                return Err(LinearError::Graphql(joined));
            }
            let data = resp
                .data
                .ok_or_else(|| LinearError::Graphql("no data in issues response".into()))?;
            for issue in &data.issues.nodes {
                tasks.push(graphql::issue_to_task(issue, &viewer_id));
            }
            let page_info = data.issues.page_info;
            if !page_info.has_next_page {
                break;
            }
            cursor = page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
            page += 1;
            if page >= 20 {
                tracing::error!(
                    "Linear paged: bailing after {page} pages (safety cap; tail truncated)"
                );
                break;
            }
        }
        Ok(tasks)
    }
}

impl LinearClient {
    /// Post a comment on a Linear issue. `issue_id` is the issue's
    /// UUID node id (stored in `task.node_id` after a poll). Wraps
    /// the `commentCreate` GraphQL mutation; returns on success or
    /// surfaces the upstream error message.
    pub async fn post_comment(
        &self,
        issue_id: &str,
        body: &str,
    ) -> Result<(), LinearError> {
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
        workspace: &pilot_core::Workspace,
        body: &str,
    ) -> Result<(), ProviderError> {
        let task = workspace.primary_task().ok_or_else(|| {
            ProviderError::permanent(
                "linear",
                format!("workspace {} has no primary task", workspace.key),
            )
        })?;
        let issue_id = task.node_id.as_deref().ok_or_else(|| {
            ProviderError::permanent(
                "linear",
                "task has no node_id (poll first)",
            )
        })?;
        self.post_comment(issue_id, body)
            .await
            .map_err(|e| ProviderError::permanent("linear", e.to_string()))
    }
}
