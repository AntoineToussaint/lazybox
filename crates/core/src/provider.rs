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

    /// User-facing message for a retryable transient whose retries are
    /// exhausted — the daemon kept failing across cycles and sync is now
    /// genuinely stuck. Carries the *classified cause* through escalation
    /// (the `Retryable` detail) instead of collapsing every stuck
    /// transient into one misattributed "check your connection or token":
    /// a repeated 5xx or a dropped connection is not a token problem, and
    /// telling the user to rotate a working token is an action that can't
    /// fix it. Auth is its own non-retryable class, surfaced through
    /// [`user_message`](Self::user_message).
    pub fn exhausted_message(&self) -> String {
        match self {
            Self::Retryable { source, detail, .. } => {
                let cause = detail.lines().next().unwrap_or(detail);
                format!("{source} sync stuck — {cause}")
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
    fn exhausted_message_carries_the_cause_not_a_token_blame() {
        // #772: the escalation message must preserve the classified
        // cause of a stuck transient, never collapse it into "check your
        // connection or token" — a repeated 5xx is not a token problem.
        let r = ProviderError::retryable("github", "HTTP 502 (Bad Gateway)");
        let msg = r.exhausted_message();
        assert!(msg.contains("HTTP 502"), "got {msg}");
        assert!(
            !msg.to_lowercase().contains("token"),
            "a retryable transient must not blame the token: {msg}"
        );
    }

    #[test]
    fn exhausted_message_takes_first_detail_line() {
        // The status bar is one row; a multi-line diagnostic collapses to
        // its first line, mirroring `user_message`'s Permanent handling.
        let r = ProviderError::retryable("github", "connection reset\nchain frame 2");
        let msg = r.exhausted_message();
        assert!(msg.contains("connection reset"), "got {msg}");
        assert!(!msg.contains("chain frame 2"), "got {msg}");
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
