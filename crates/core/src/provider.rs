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

use std::future::Future;

use crate::{Task, Workspace};

/// Default safety cap for provider cursor walks.
pub const DEFAULT_MAX_PAGES: usize = 20;

/// Canonical provider `source` ids — the string each built-in provider
/// stamps on `TaskId.source`, and the value that snippet `provider:`
/// scoping and the mutation routers key off. Defined here, in the crate
/// both the providers and `config` depend on, so they can't drift on the
/// spelling: `gh_provider::SOURCE` / `linear_provider::SOURCE` derive from
/// these, and `config`'s built-in GitHub/Linear snippets scope on them.
pub const GITHUB_SOURCE: &str = "github";
pub const LINEAR_SOURCE: &str = "linear";

/// Whether a fetch consumed the entire upstream result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchCoverage {
    /// Every page was consumed, so the result is authoritative.
    Complete,
    /// The returned value is only a prefix and must not drive deletion.
    Partial,
}

impl FetchCoverage {
    /// Returns whether the fetch stopped before covering every page.
    pub fn is_partial(self) -> bool {
        self == Self::Partial
    }
}

/// A fetched value together with its authoritative-coverage verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome<T> {
    pub items: T,
    pub coverage: FetchCoverage,
}

impl<T> FetchOutcome<T> {
    /// Returns whether the value is a non-authoritative prefix.
    pub fn is_partial(&self) -> bool {
        self.coverage.is_partial()
    }
}

/// Provider-neutral cursor metadata for one fetched page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchPageInfo {
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

/// One page returned by a provider-specific page closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchPage<T> {
    pub items: Vec<T>,
    pub page_info: Option<FetchPageInfo>,
}

/// Why a cursor walk stopped before consuming the full result set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaginationStop<E> {
    /// Fetching a page after the first successful page failed.
    PageError(E),
    /// The provider omitted the page metadata needed to prove completion.
    MissingPageInfo,
    /// The provider advertised another page without supplying its cursor.
    MissingEndCursor,
    /// The walk reached its configured page limit while more pages remained.
    PageLimit { pages: usize },
}

/// The result of walking a cursor-paginated endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaginationOutcome<T, E> {
    /// Every upstream page was consumed.
    Complete(Vec<T>),
    /// The returned items are a non-authoritative prefix.
    Partial {
        items: Vec<T>,
        reason: PaginationStop<E>,
    },
}

impl<T, E> PaginationOutcome<T, E> {
    /// Discards the pagination stop reason while preserving its coverage verdict.
    pub fn into_fetch_outcome(self) -> FetchOutcome<Vec<T>> {
        match self {
            Self::Complete(items) => FetchOutcome {
                items,
                coverage: FetchCoverage::Complete,
            },
            Self::Partial { items, .. } => FetchOutcome {
                items,
                coverage: FetchCoverage::Partial,
            },
        }
    }
}

/// Walks a cursor-paginated provider endpoint.
///
/// A first-page error is returned because there is no usable result. Once a
/// page succeeds, a later error, missing cursor, missing page metadata, or the
/// safety cap produces [`PaginationOutcome::Partial`] with the exact stop
/// reason so callers can either preserve the error or deliberately keep the
/// fetched prefix as [`FetchCoverage::Partial`].
pub async fn paginate<T, E, F, Fut>(
    mut fetch_page: F,
    max_pages: usize,
) -> Result<PaginationOutcome<T, E>, E>
where
    F: FnMut(Option<String>, usize) -> Fut,
    Fut: Future<Output = Result<FetchPage<T>, E>>,
{
    let mut items = Vec::new();
    let mut cursor = None;

    for page in 0..max_pages {
        let fetched = match fetch_page(cursor, page).await {
            Ok(fetched) => fetched,
            Err(error) if page == 0 => return Err(error),
            Err(error) => {
                return Ok(PaginationOutcome::Partial {
                    items,
                    reason: PaginationStop::PageError(error),
                });
            }
        };
        items.extend(fetched.items);

        let Some(page_info) = fetched.page_info else {
            return Ok(PaginationOutcome::Partial {
                items,
                reason: PaginationStop::MissingPageInfo,
            });
        };
        if !page_info.has_next_page {
            return Ok(PaginationOutcome::Complete(items));
        }
        let Some(next_cursor) = page_info.end_cursor else {
            return Ok(PaginationOutcome::Partial {
                items,
                reason: PaginationStop::MissingEndCursor,
            });
        };
        cursor = Some(next_cursor);
    }

    Ok(PaginationOutcome::Partial {
        items,
        reason: PaginationStop::PageLimit { pages: max_pages },
    })
}

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
        /// `true` when the retry is the provider deliberately pacing
        /// ITSELF — a governor backoff under shared-token contention, not
        /// a fault. Such a backoff is expected and self-clearing, so it
        /// must surface honestly ("busy — backing off") and never escalate
        /// to an actionable "sync failing — check your token" error the way
        /// a persistent transport failure does (#782).
        self_throttled: bool,
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
            self_throttled: false,
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
            self_throttled: false,
        }
    }

    /// A retryable that is the provider's OWN governor deliberately
    /// backing off (shared-token contention), not a fault. Carries the
    /// same retry hint as [`retryable_after`](Self::retryable_after) but
    /// is flagged `self_throttled` so the polling layer surfaces it
    /// honestly and never escalates it to an actionable error (#782).
    pub fn self_throttle(source: impl Into<String>, detail: impl Into<String>, secs: u64) -> Self {
        Self::Retryable {
            source: source.into(),
            detail: detail.into(),
            retry_after_secs: Some(secs),
            self_throttled: true,
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

    /// True when this retryable is the provider's OWN governor pacing
    /// itself under shared-token contention rather than a fault. Such a
    /// backoff self-clears and must never be escalated to an actionable
    /// "sync failing — check your token" error (#782).
    pub fn is_self_throttle(&self) -> bool {
        matches!(
            self,
            Self::Retryable {
                self_throttled: true,
                ..
            }
        )
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
                self_throttled,
            } => {
                let after = retry_after_secs
                    .map(|s| format!(" (retry after {s}s)"))
                    .unwrap_or_default();
                let tag = if *self_throttled {
                    " self-throttle"
                } else {
                    ""
                };
                format!("[{source}] retryable{tag}{after}: {detail}")
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
                self_throttled,
                ..
            } => {
                if *self_throttled {
                    // The token, connection, and budget are all fine; lazybox
                    // is deliberately pacing its OWN sync to stay within the
                    // rate budget. Say that, not "check your token" (#782).
                    // Kept cause-neutral: the pressure is usually external
                    // (`gh`/agents on the shared token) but can be the
                    // daemon's own concurrent work, so don't assert "elsewhere".
                    format!("{source} busy — pacing its own sync to stay within the rate budget")
                } else {
                    match retry_after_secs {
                        Some(s) => format!("{source} throttled, retrying in {s}s"),
                        None => format!("{source} hiccup, retrying next cycle"),
                    }
                }
            }
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

    /// Terse user-facing message for a retryable transient whose retries
    /// are exhausted — the daemon kept failing across cycles and sync is
    /// now genuinely stuck. Stays one row like [`user_message`](Self::user_message):
    /// it names the failure *class* (a stuck transient) without embedding
    /// the raw `detail`, which is diagnostic-grade text meant for
    /// [`diagnostic`](Self::diagnostic) / the log file, not the status bar.
    /// The precise cause (a 502, a dropped connection, a timed-out tick)
    /// still travels to the client in the event's diagnostic field.
    ///
    /// Crucially it never misattributes the failure to the token: a stuck
    /// transient is a reachability problem, not an expired credential, so
    /// telling the user to rotate a working token is a dead-end action.
    /// Auth is its own non-retryable class and falls through to
    /// [`user_message`](Self::user_message), as does every non-retryable
    /// variant (only a retryable ever exhausts its retries).
    pub fn exhausted_message(&self) -> String {
        match self {
            Self::Retryable { source, .. } => {
                format!("{source} sync stuck — still failing after retries")
            }
            _ => self.user_message(),
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
    /// `expected_head_oid` — when the caller knows which head commit it
    /// verified as merge-ready — asks the backend to reject the merge
    /// if the head has since moved (GitHub's `expectedHeadOid`
    /// compare-and-swap). `None` skips the guard; providers without an
    /// equivalent concept may ignore it.
    ///
    /// Idempotency: if the task is already merged, return `Ok(())`
    /// rather than `Permanent` — the polling cycle will reconcile
    /// the local copy regardless.
    async fn merge(
        &self,
        workspace: &Workspace,
        expected_head_oid: Option<&str>,
    ) -> Result<(), ProviderError> {
        let _ = (workspace, expected_head_oid);
        Err(ProviderError::unsupported(self.name(), "merge"))
    }

    /// Update the workspace's PR branch by merging the base branch into
    /// it — the "Update branch" button on github.com. Providers dispatch
    /// to the backend's branch-update mutation.
    ///
    /// Idempotency: an already up-to-date branch returns `Ok(())` rather
    /// than an error — the polling cycle reconciles the `BEHIND` tag
    /// regardless.
    async fn update_branch(&self, workspace: &Workspace) -> Result<(), ProviderError> {
        let _ = workspace;
        Err(ProviderError::unsupported(self.name(), "update_branch"))
    }

    /// Close the workspace's issue. Providers that can't truly delete
    /// an issue (GitHub, for non-admins) close it instead. Defaults to
    /// `unsupported` so a provider without an issue-close concept opts
    /// in explicitly.
    ///
    /// Idempotency: closing an already-closed issue returns `Ok(())`
    /// — the polling cycle reconciles the local copy regardless.
    async fn close_issue(&self, workspace: &Workspace) -> Result<(), ProviderError> {
        let _ = workspace;
        Err(ProviderError::unsupported(self.name(), "close_issue"))
    }

    /// Close the workspace's PR without merging it. Defaults to
    /// `unsupported` so a provider without a PR-close concept opts in
    /// explicitly.
    ///
    /// Idempotency: closing an already-closed PR returns `Ok(())` —
    /// the polling cycle reconciles the local copy regardless.
    async fn close_pr(&self, workspace: &Workspace) -> Result<(), ProviderError> {
        let _ = workspace;
        Err(ProviderError::unsupported(self.name(), "close_pr"))
    }

    /// Hard-delete the workspace's issue upstream. Most backends gate
    /// this behind elevated permissions (GitHub requires admin), so
    /// callers should treat an error as "fall back to
    /// [`close_issue`](Self::close_issue)" rather than a dead end.
    async fn delete_issue(&self, workspace: &Workspace) -> Result<(), ProviderError> {
        let _ = workspace;
        Err(ProviderError::unsupported(self.name(), "delete_issue"))
    }

    /// List the accounts that can be *requested* as reviewers on the
    /// workspace's PR — the repo's assignable users plus the
    /// provider's own suggestions for this PR. Used to populate the
    /// reviewer picker with the full requestable set rather than only
    /// people who already touched the PR. Returns provider-native
    /// logins; the caller merges them with interaction-derived
    /// candidates and pre-excludes existing reviewers.
    async fn list_requestable_reviewers(
        &self,
        workspace: &Workspace,
    ) -> Result<Vec<String>, ProviderError> {
        let _ = workspace;
        Err(ProviderError::unsupported(
            self.name(),
            "list_requestable_reviewers",
        ))
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
    use std::cell::RefCell;
    use std::future::ready;

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

    #[test]
    fn self_throttle_is_a_retryable_flagged_and_carries_its_backoff_hint() {
        // #782: a governor self-throttle is a retryable that the polling
        // layer must be able to single out (via `is_self_throttle`) and
        // whose backoff window round-trips through `retry_after_secs` — the
        // latter is what keeps it exempt from the exhaustion escalation.
        let s = ProviderError::self_throttle("github", "background allowance spent", 15);
        assert!(s.is_retryable(), "a self-throttle is still a retryable");
        assert!(s.is_self_throttle(), "and is flagged as self-imposed");
        assert_eq!(s.retry_after_secs(), Some(15), "carries its backoff hint");

        // A plain retryable — even a throttling one with a retry hint — is
        // NOT a self-throttle: the flag distinguishes lazybox pacing itself
        // from GitHub imposing a limit.
        let plain = ProviderError::retryable_after("github", "secondary rate limit", 30);
        assert!(
            !plain.is_self_throttle(),
            "a remote throttle is not self-imposed"
        );
        assert!(!ProviderError::auth("github", "401").is_self_throttle());
    }

    #[test]
    fn self_throttle_message_is_honest_and_never_blames_the_token() {
        // #782: the exemplar bug surfaced a self-imposed backoff as "check
        // your connection or token" — a dead-end the user can't act on. The
        // token, connection, and budget are all fine; the message must say
        // lazybox is deliberately pacing itself, not that something is
        // wrong. Guards the wording so a future reword can't regress it.
        let s = ProviderError::self_throttle("github", "background allowance spent", 15);
        let msg = s.user_message();
        assert!(msg.starts_with("github"), "names the source: {msg}");
        assert!(
            msg.contains("pacing"),
            "names the deliberate self-pacing: {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("token"),
            "must not blame the token: {msg}"
        );
        assert!(
            !msg.contains("check your"),
            "must not tell the user to check anything: {msg}"
        );
        assert!(msg.len() < 80, "stays one status row: {msg}");
        // The self-throttle is tagged in the diagnostic so the log
        // distinguishes it from a remote throttle at a glance.
        assert!(
            s.diagnostic().contains("self-throttle"),
            "diagnostic tags the self-throttle: {}",
            s.diagnostic()
        );
    }

    #[test]
    fn exhausted_message_names_the_class_without_blaming_the_token() {
        // #772: a stuck transient is a reachability failure, not a token
        // problem — the escalation must never tell the user to rotate a
        // working token, and it must read as a stuck sync.
        let r = ProviderError::retryable("github", "HTTP 502 (Bad Gateway)");
        let msg = r.exhausted_message();
        assert!(msg.starts_with("github"), "names the source: {msg}");
        assert!(msg.contains("stuck"), "reads as a stuck sync: {msg}");
        assert!(
            !msg.to_lowercase().contains("token"),
            "a retryable transient must not blame the token: {msg}"
        );
    }

    #[test]
    fn exhausted_message_stays_terse_even_for_a_long_detail() {
        // The status bar is one row. `detail` is diagnostic-grade text —
        // the tick-timeout path stuffs a ~200-char developer explanation
        // there — so the terse message must NOT embed it. Regression for
        // the escalation banner ballooning to the full diagnostic.
        let verbose = "sync exceeded 180s — the per-upsert / per-graphql / per-git \
             timeouts should catch this; hitting the outer cap means something \
             escaped them and the whole tick was abandoned this cycle";
        let r = ProviderError::retryable("github", verbose);
        let msg = r.exhausted_message();
        assert!(
            msg.len() < 80,
            "exhausted message stays one row, got {} chars: {msg}",
            msg.len()
        );
        assert!(
            !msg.contains("per-graphql"),
            "raw diagnostic detail must not leak into the terse message: {msg}"
        );
    }

    #[test]
    fn exhausted_message_falls_through_for_non_retryable() {
        // Only a retryable ever exhausts its retries; every other class
        // keeps its normal message (auth stays an auth prompt, not a
        // "sync stuck"). Documents the total contract of the public method.
        for e in [
            ProviderError::auth("github", "401"),
            ProviderError::permanent("github", "bad query"),
            ProviderError::unsupported("github", "merge"),
        ] {
            assert_eq!(
                e.exhausted_message(),
                e.user_message(),
                "non-retryable falls through to user_message"
            );
        }
    }

    #[test]
    fn coverage_helpers_report_partial_results() {
        assert!(FetchCoverage::Partial.is_partial());
        assert!(
            FetchOutcome {
                items: vec![1],
                coverage: FetchCoverage::Partial,
            }
            .is_partial()
        );
    }

    #[test]
    fn paginate_walks_cursors_to_complete_coverage() {
        let cursors = RefCell::new(Vec::new());
        let outcome = futures::executor::block_on(paginate(
            |cursor, page| {
                cursors.borrow_mut().push(cursor);
                ready(Ok::<_, &str>(FetchPage {
                    items: vec![page],
                    page_info: Some(FetchPageInfo {
                        has_next_page: page == 0,
                        end_cursor: (page == 0).then(|| "next".to_string()),
                    }),
                }))
            },
            DEFAULT_MAX_PAGES,
        ))
        .unwrap();

        assert_eq!(outcome, PaginationOutcome::Complete(vec![0, 1]));
        assert_eq!(cursors.into_inner(), vec![None, Some("next".to_string())]);
    }

    #[test]
    fn paginate_reports_why_walks_are_incomplete() {
        let capped = futures::executor::block_on(paginate(
            |_cursor, page| {
                ready(Ok::<_, &str>(FetchPage {
                    items: vec![page],
                    page_info: Some(FetchPageInfo {
                        has_next_page: true,
                        end_cursor: Some(format!("cursor-{page}")),
                    }),
                }))
            },
            2,
        ))
        .unwrap();
        assert_eq!(
            capped,
            PaginationOutcome::Partial {
                items: vec![0, 1],
                reason: PaginationStop::PageLimit { pages: 2 },
            }
        );

        let missing_cursor = futures::executor::block_on(paginate(
            |_cursor, _page| {
                ready(Ok::<_, &str>(FetchPage {
                    items: vec![1],
                    page_info: Some(FetchPageInfo {
                        has_next_page: true,
                        end_cursor: None,
                    }),
                }))
            },
            DEFAULT_MAX_PAGES,
        ))
        .unwrap();
        assert_eq!(
            missing_cursor,
            PaginationOutcome::Partial {
                items: vec![1],
                reason: PaginationStop::MissingEndCursor,
            }
        );

        let missing_page_info = futures::executor::block_on(paginate(
            |_cursor, _page| {
                ready(Ok::<_, &str>(FetchPage {
                    items: vec![1],
                    page_info: None,
                }))
            },
            DEFAULT_MAX_PAGES,
        ))
        .unwrap();
        assert_eq!(
            missing_page_info,
            PaginationOutcome::Partial {
                items: vec![1],
                reason: PaginationStop::MissingPageInfo,
            }
        );
    }

    #[test]
    fn paginate_preserves_later_page_errors_until_the_caller_discards_them() {
        let error = futures::executor::block_on(paginate::<usize, _, _, _>(
            |_cursor, _page| ready(Err("first page failed")),
            DEFAULT_MAX_PAGES,
        ))
        .unwrap_err();
        assert_eq!(error, "first page failed");

        let partial = futures::executor::block_on(paginate(
            |_cursor, page| {
                ready(if page == 0 {
                    Ok(FetchPage {
                        items: vec![1],
                        page_info: Some(FetchPageInfo {
                            has_next_page: true,
                            end_cursor: Some("next".to_string()),
                        }),
                    })
                } else {
                    Err("later page failed")
                })
            },
            DEFAULT_MAX_PAGES,
        ))
        .unwrap();
        assert_eq!(
            partial,
            PaginationOutcome::Partial {
                items: vec![1],
                reason: PaginationStop::PageError("later page failed"),
            }
        );

        let fetch_outcome = partial.into_fetch_outcome();
        assert_eq!(fetch_outcome.items, vec![1]);
        assert_eq!(fetch_outcome.coverage, FetchCoverage::Partial);
    }
}
