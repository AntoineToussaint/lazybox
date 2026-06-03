//! Task provider abstraction. GitHub, Linear, Jira, etc. implement this trait.
//!
//! ## Error model
//!
//! [`ProviderError`] is classified at the boundary so the polling
//! layer can decide what to do without parsing strings:
//!
//! - **Retryable** — transient: network hiccup, 5xx, rate limit.
//!   Polling logs and retries on the next cycle. The user sees a
//!   terse "`<provider>` hiccup, retrying" hint, not a full stack.
//! - **Auth** — credentials are wrong / expired. Polling surfaces it
//!   loud (`Event::ProviderError`) with a user-facing message; user
//!   must rotate their token. Not retried until they do.
//! - **Permanent** — query/protocol/programming error. Surfaced with
//!   the diagnostic so dev / users can file a bug. Not retried.
//!
//! Every variant carries:
//! - `source` — provider id (e.g. `"github"`) for grouping in the UI.
//! - `detail` — full chained error string for logs / `diagnostic()`.
//!
//! Display defaults to the *terse* user-facing message; call
//! `diagnostic()` for the full text in dev tooling.

use crate::{Task, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// Transient — try again. `retry_after_secs` is a HINT from the
    /// provider about when to retry; the polling driver should honor
    /// it (e.g., when GitHub reports rate-limit hit, the reset window
    /// is several minutes — retrying on the normal poll cadence just
    /// burns the same error repeatedly). `None` means "no hint, use
    /// the configured poll interval."
    Retryable {
        source: String,
        detail: String,
        retry_after_secs: Option<u64>,
    },
    /// Credentials wrong / expired. Don't retry without user action.
    Auth { source: String, detail: String },
    /// Permanent failure. Surface, don't retry.
    Permanent { source: String, detail: String },
    /// This provider doesn't implement the requested mutation —
    /// e.g. asking a Linear-backed workspace to merge a PR, or any
    /// sandbox workspace to do anything. Action-catalog
    /// `availability()` gating SHOULD have caught this upstream;
    /// hitting this variant means a surface offered the action
    /// without checking.
    Unsupported { source: String, op: String },
}

impl ProviderError {
    pub fn retryable(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Retryable {
            source: source.into(),
            detail: detail.into(),
            retry_after_secs: None,
        }
    }

    /// Same as `retryable` but with a hint about WHEN to retry. Used
    /// by providers that know the exact reset deadline (GitHub's
    /// `rateLimit.resetAt`, GitHub's `Retry-After` header, etc.).
    pub fn retryable_after(
        source: impl Into<String>,
        detail: impl Into<String>,
        secs: u64,
    ) -> Self {
        Self::Retryable {
            source: source.into(),
            detail: detail.into(),
            retry_after_secs: Some(secs),
        }
    }

    pub fn auth(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Auth {
            source: source.into(),
            detail: detail.into(),
        }
    }

    pub fn permanent(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Permanent {
            source: source.into(),
            detail: detail.into(),
        }
    }

    pub fn unsupported(source: impl Into<String>, op: impl Into<String>) -> Self {
        Self::Unsupported {
            source: source.into(),
            op: op.into(),
        }
    }

    pub fn source(&self) -> &str {
        match self {
            Self::Retryable { source, .. }
            | Self::Auth { source, .. }
            | Self::Permanent { source, .. }
            | Self::Unsupported { source, .. } => source,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth { .. })
    }

    /// Provider-supplied "wait at least this long before retrying"
    /// hint. Only populated for `Retryable` errors that came with a
    /// known reset window; everything else returns None. The polling
    /// driver clamps the next-tick sleep to at least this many
    /// seconds when populated.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::Retryable {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }

    /// Full diagnostic — provider id + variant tag + the underlying
    /// error chain. Goes to the log file; not shown in the TUI by
    /// default (use `RUST_LOG=debug` or tail `/tmp/lazybox.log`).
    pub fn diagnostic(&self) -> String {
        match self {
            Self::Retryable {
                source,
                detail,
                retry_after_secs,
            } => {
                let after = retry_after_secs
                    .map(|s| format!(" (retry after {s}s)"))
                    .unwrap_or_default();
                format!("[{source}] retryable{after}: {detail}")
            }
            Self::Auth { source, detail } => format!("[{source}] auth: {detail}"),
            Self::Permanent { source, detail } => {
                format!("[{source}] permanent: {detail}")
            }
            Self::Unsupported { source, op } => {
                format!("[{source}] unsupported operation: {op}")
            }
        }
    }

    /// Terse user-facing message. Stays short — the TUI's status bar
    /// is one row.
    pub fn user_message(&self) -> String {
        match self {
            Self::Retryable {
                source,
                retry_after_secs,
                ..
            } => match retry_after_secs {
                Some(s) => format!("{source} throttled, retrying in {s}s"),
                None => format!("{source} hiccup, retrying next cycle"),
            },
            Self::Auth { source, .. } => {
                format!("{source} auth failed — rotate token then `lazybox --fresh`")
            }
            Self::Permanent { source, detail } => {
                let summary = detail.lines().next().unwrap_or(detail);
                format!("{source}: {summary}")
            }
            Self::Unsupported { source, op } => {
                format!("{source} doesn't support {op}")
            }
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Default Display is the user-facing message. Logs use
        // `diagnostic()` explicitly when they want the full chain.
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for ProviderError {}

/// A source of tasks (PRs, issues, tickets) — and the place where
/// the user's mutations (merge, request reviewers, …) land.
///
/// Providers fetch tasks from external systems and convert them to
/// the generic `Task` type. The server polls providers periodically;
/// user actions in the TUI route to the matching provider's mutation
/// methods.
///
/// **Capability model**: mutations are optional. The default impl
/// of each returns `ProviderError::Unsupported` — providers
/// implement only the operations their backend supports. The action
/// catalog's `availability()` predicate is the upstream gate; if it
/// drifts the call still fails gracefully here.
#[allow(async_fn_in_trait)]
pub trait TaskProvider: Send + Sync {
    /// Provider name (e.g., "github", "linear"). Matches the
    /// workspace-key prefix so the server can route a workspace's
    /// mutation request to the right provider.
    fn name(&self) -> &str;

    /// Fetch all current tasks. Called once per poll cycle.
    async fn fetch_tasks(&self) -> Result<Vec<Task>, ProviderError>;

    /// The authenticated username, if known.
    fn username(&self) -> Option<&str> {
        None
    }

    /// Merge the workspace's underlying task. Most provider impls
    /// will check `workspace.pr` (or equivalent) is ready and
    /// dispatch to the backend's merge mutation.
    ///
    /// Idempotency: if the task is already merged, return `Ok(())`
    /// rather than `Permanent` — the polling cycle will reconcile
    /// the local copy regardless.
    async fn merge(&self, workspace: &Workspace) -> Result<(), ProviderError> {
        let _ = workspace;
        Err(ProviderError::unsupported(self.name(), "merge"))
    }

    /// Request reviewer(s) on the workspace's task. `logins` are
    /// provider-native account identifiers (github logins, linear
    /// user ids, …).
    async fn request_reviewers(
        &self,
        workspace: &Workspace,
        logins: &[String],
    ) -> Result<(), ProviderError> {
        let _ = (workspace, logins);
        Err(ProviderError::unsupported(self.name(), "request_reviewers"))
    }

    /// Add assignee(s) to the workspace's task. Works on issues
    /// AND PR-shaped tasks where the provider supports it.
    async fn add_assignees(
        &self,
        workspace: &Workspace,
        logins: &[String],
    ) -> Result<(), ProviderError> {
        let _ = (workspace, logins);
        Err(ProviderError::unsupported(self.name(), "add_assignees"))
    }

    /// Replace the assignee set on the workspace's task. Default
    /// implementation falls back to `add_assignees` after diffing
    /// — providers that need separate add + remove paths (e.g.
    /// GitHub) override. The slice is the *desired* set; the
    /// provider computes its own diff against the current task
    /// state.
    async fn set_assignees(
        &self,
        workspace: &Workspace,
        logins: &[String],
    ) -> Result<(), ProviderError> {
        let _ = (workspace, logins);
        Err(ProviderError::unsupported(self.name(), "set_assignees"))
    }

    /// Post a reply (comment) on the workspace's task. PR
    /// workspaces target the PR's main thread; issue workspaces
    /// target the issue. Per-comment threading is not yet modeled.
    async fn post_reply(&self, workspace: &Workspace, body: &str) -> Result<(), ProviderError> {
        let _ = (workspace, body);
        Err(ProviderError::unsupported(self.name(), "post_reply"))
    }

    /// List the repository labels available for the workspace's
    /// task. Used to populate the label picker. Returns a vector of
    /// `(name, color)` pairs; the daemon caches them per repo and
    /// the picker pre-checks the task's currently-applied set.
    async fn list_repo_labels(
        &self,
        workspace: &Workspace,
    ) -> Result<Vec<crate::Label>, ProviderError> {
        let _ = workspace;
        Err(ProviderError::unsupported(self.name(), "list_repo_labels"))
    }

    /// Replace the label set on the workspace's PR or issue. The
    /// provider computes its own diff against the task's persisted
    /// labels and runs add/remove mutations as needed. Empty
    /// `names` clears every label.
    async fn set_labels(
        &self,
        workspace: &Workspace,
        names: &[String],
    ) -> Result<(), ProviderError> {
        let _ = (workspace, names);
        Err(ProviderError::unsupported(self.name(), "set_labels"))
    }
}

/// Pick the provider whose [`name`](TaskProvider::name) matches a
/// workspace key's source prefix. Returns `None` when no provider
/// claims it (scratch sandbox workspaces with no upstream source).
///
/// Workspace keys follow `<source>-<rest>` — e.g.
/// `"github-acme-widget-186"` matches a provider with
/// `name() == "github"`. The `"sandbox"` prefix has no provider
/// today and gracefully returns `None`; callers should fall back to
/// "no upstream, local-only" semantics.
pub fn provider_for_workspace<'a, P: TaskProvider + ?Sized>(
    providers: &'a [std::sync::Arc<P>],
    workspace_key: &str,
) -> Option<&'a std::sync::Arc<P>> {
    let prefix = workspace_key.split_once('-').map(|(p, _)| p)?;
    providers.iter().find(|p| p.name() == prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_helpers() {
        let r = ProviderError::retryable("github", "tcp reset");
        assert!(r.is_retryable());
        assert!(!r.is_auth());

        let a = ProviderError::auth("github", "401 Unauthorized");
        assert!(a.is_auth());
        assert!(!a.is_retryable());

        let p = ProviderError::permanent("github", "missing field `repository`");
        assert!(!p.is_retryable());
        assert!(!p.is_auth());
    }

    #[test]
    fn user_message_is_terse_and_diagnostic_is_full() {
        let p = ProviderError::permanent(
            "github",
            "GraphQL: line 1\nstack trace line 2\nstack trace line 3",
        );
        let msg = p.user_message();
        assert!(msg.len() < 80, "user_message stays short: {msg}");
        assert!(p.diagnostic().contains("stack trace line 2"));
    }

    #[test]
    fn display_uses_user_message() {
        let r = ProviderError::retryable("github", "secret detail");
        let s = format!("{r}");
        assert!(!s.contains("secret detail"));
        assert!(s.contains("github"));
        assert!(s.contains("retrying"));
    }

    #[test]
    fn retryable_default_has_no_retry_after_hint() {
        let r = ProviderError::retryable("github", "tcp reset");
        assert_eq!(r.retry_after_secs(), None);
    }

    #[test]
    fn retryable_after_carries_seconds() {
        // The polling driver consults `retry_after_secs` to decide
        // how long to back off — must round-trip exactly through
        // the constructor.
        let r = ProviderError::retryable_after("github", "rate limit hit", 600);
        assert_eq!(r.retry_after_secs(), Some(600));
        assert!(r.is_retryable());
    }

    #[test]
    fn retry_after_only_meaningful_for_retryable_variant() {
        // Auth and Permanent errors never carry a retry hint —
        // they're "stop trying" by definition.
        let a = ProviderError::auth("github", "401");
        assert_eq!(a.retry_after_secs(), None);
        let p = ProviderError::permanent("github", "bad query");
        assert_eq!(p.retry_after_secs(), None);
    }

    #[test]
    fn user_message_mentions_throttle_when_retry_after_set() {
        // Distinct from the generic "hiccup, retrying next cycle"
        // wording so the user sees "we're paused, here's how long".
        let r = ProviderError::retryable_after("github", "rate limit", 300);
        let msg = r.user_message();
        assert!(msg.contains("300s"), "got {msg}");
        assert!(msg.contains("throttled"), "got {msg}");
    }
}
