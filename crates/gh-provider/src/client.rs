use lazybox_auth::Credential;
use lazybox_core::*;
use octocrab::Octocrab;

use crate::graphql;
use crate::notifications::{
    self, NotificationEntry, NotificationsPoll, NotificationsSnapshot, NotificationsState,
    SharedNotificationsState,
};

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("GitHub API error: {0}")]
    Api(#[from] octocrab::Error),
    #[error("GraphQL error: {0}")]
    Graphql(String),
    /// Non-success HTTP response from GitHub, OR a 2xx response we
    /// couldn't parse as JSON. Carries the actual status + content-
    /// type + a body excerpt so the user sees "HTTP 502 Bad Gateway"
    /// instead of the opaque "Serde Error" `octocrab::Error::Serde`
    /// produces on the typed deserialize path. This is the variant
    /// emitted by `post_graphql_with_retry`; it replaces the previous
    /// "expected value at line 1 column 1 — likely 502" guess.
    #[error("github HTTP {status}{reason}: {body_excerpt}")]
    HttpStatus {
        status: u16,
        /// Canonical status reason ("Bad Gateway") rendered as
        /// " (Bad Gateway)" so the Display string reads naturally
        /// when present; empty string when GitHub returned an
        /// unknown / out-of-range status code.
        reason: String,
        content_type: String,
        body_excerpt: String,
    },
    /// Pagination safety cap hit — typically means the user's filter
    /// is too loose (>2000 matching PRs). Tail truncated.
    #[error("GraphQL paged out: returned {count} PRs across {pages} pages, hit safety cap")]
    Truncated { count: usize, pages: usize },
    /// Every configured watched-repo query failed. The user opted
    /// in to those repos explicitly so silently missing them is a
    /// data-visibility regression worth surfacing.
    #[error("all {count} watched-repo queries failed")]
    WatchAllFailed { count: usize },
    /// Rate budget said no. `retry_after_secs` carries the precise
    /// reset window (from GitHub's `rateLimit.resetAt` for remote,
    /// or the local-bucket refill ETA for local exhaustion) so the
    /// polling layer can sleep exactly until the budget opens up
    /// instead of retrying blindly. Distinct from `Graphql` so the
    /// `From<GhError>` mapping can preserve the `retry_after_secs`
    /// hint into `ProviderError::Retryable`.
    #[error("rate budget blocked the request: {reason} (retry after {retry_after_secs}s)")]
    RateLimited {
        retry_after_secs: u64,
        reason: String,
    },
}

/// Is this error worth retrying? Used by `post_graphql_with_retry`
/// to decide between sleep-and-retry and fail-fast.
///
/// Transport variants (Hyper/Service/Http/Json/Io) are always
/// transient — the request never reached GitHub's app layer.
/// `HttpStatus` retries on 5xx (capped at one in-call retry, see
/// `is_server_error`) + any 2xx with a non-JSON content-type
/// (proxy/CDN serving a maintenance page), matching what
/// `From<GhError> for ProviderError` classifies as `Retryable`.
/// Rate limits (429 / secondary-limit 403) surface as
/// `GhError::RateLimited` and are never retried in-call. Auth
/// (401/403), other 4xx, and 2xx-JSON parse failures (real schema
/// mismatches) are not retried.
fn is_transient(e: &GhError) -> bool {
    match e {
        GhError::Api(octocrab::Error::Hyper { .. })
        | GhError::Api(octocrab::Error::Service { .. })
        | GhError::Api(octocrab::Error::Http { .. })
        | GhError::Api(octocrab::Error::Json { .. })
        | GhError::Api(octocrab::Error::Serde { .. }) => true,
        GhError::Api(octocrab::Error::GitHub { source, .. }) => {
            matches!(source.status_code.as_u16(), 502..=504)
        }
        GhError::Api(_) => false,
        GhError::HttpStatus {
            status,
            content_type,
            ..
        } => {
            // 429 is deliberately NOT here: a throttle response is
            // surfaced as `GhError::RateLimited` (with the server's
            // `Retry-After` honored by the poll scheduler), never
            // retried on the in-call millisecond ladder — hot-retrying
            // a rate limit is exactly what deepens it.
            //
            // All 5xx count as transient, but the retry ladder caps
            // them at ONE in-call retry (see `is_server_error`) so a
            // sustained outage is spaced by the poll-level backoff.
            if matches!(*status, 500..=599) {
                return true;
            }
            // 2xx with a non-JSON body — usually a proxy/CDN
            // maintenance page that won the race with GitHub.
            // Worth one retry, mirroring how octocrab's typed
            // `Serde` failure used to be classified.
            (200..=299).contains(status) && !content_type_is_json(content_type)
        }
        _ => false,
    }
}

/// Is this a 5xx from GitHub's app layer? These stay transient but
/// get at most ONE in-call retry — a sustained outage should be
/// spaced by the poll-level backoff (the tick scheduler's interval /
/// retry-after clamp), not hammered on the 200ms/800ms ladder.
fn is_server_error(e: &GhError) -> bool {
    matches!(e, GhError::HttpStatus { status, .. } if (500..=599).contains(status))
}

/// Rate-limit hints parsed off a non-success GitHub response. All
/// three headers are optional on the wire; `parse` never fails.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RateLimitHeaders {
    /// `Retry-After` in seconds (GitHub only uses the delta-seconds
    /// form, never the HTTP-date form, per its rate-limit docs).
    retry_after_secs: Option<u64>,
    /// `x-ratelimit-remaining`.
    remaining: Option<u32>,
    /// `x-ratelimit-reset` — epoch seconds when the window reopens.
    reset_epoch_secs: Option<u64>,
}

impl RateLimitHeaders {
    fn parse(retry_after: Option<&str>, remaining: Option<&str>, reset: Option<&str>) -> Self {
        Self {
            retry_after_secs: retry_after.and_then(|v| v.trim().parse().ok()),
            remaining: remaining.and_then(|v| v.trim().parse().ok()),
            reset_epoch_secs: reset.and_then(|v| v.trim().parse().ok()),
        }
    }

    /// Seconds to wait before the next request, preferring the
    /// explicit `Retry-After`, falling back to the reset timestamp,
    /// then to a conservative 60s default. Clamped to >= 1 so a
    /// clock-skewed reset in the past never produces a hot loop.
    fn wait_secs(&self, now_epoch_secs: u64) -> u64 {
        if let Some(secs) = self.retry_after_secs {
            return secs.max(1);
        }
        if let Some(reset) = self.reset_epoch_secs {
            return reset.saturating_sub(now_epoch_secs).max(1);
        }
        60
    }
}

/// Does this non-success response mean "you are being rate limited"?
/// GitHub signals throttling two ways: a plain 429, and — for
/// secondary (abuse) limits — a 403 whose body carries a documented
/// message ("You have exceeded a secondary rate limit…" /
/// "…rate limit exceeded…") and usually a `Retry-After` header. A
/// bare 403 without either marker stays an auth failure.
fn is_rate_limit_response(status: u16, body: &str, has_retry_after: bool) -> bool {
    if status == 429 {
        return true;
    }
    if status != 403 {
        return false;
    }
    if has_retry_after {
        return true;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("secondary rate limit")
        || lower.contains("rate limit exceeded")
        || lower.contains("abuse detection")
}

/// `true` when the content-type header looks like JSON. GitHub uses
/// `application/json; charset=utf-8` so we substring-match rather
/// than equality-check. Empty / missing content-type counts as
/// "not JSON" — we don't want to attempt JSON parse on bytes whose
/// type the server didn't advertise.
fn content_type_is_json(ct: &str) -> bool {
    ct.to_ascii_lowercase().contains("application/json")
}

/// Excerpt of a body for inclusion in error messages. Trims
/// whitespace and caps at 200 chars so a maintenance-page HTML blob
/// doesn't blow up logs / the footer.
fn body_excerpt(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= 200 {
        trimmed.to_string()
    } else {
        // char_indices respects UTF-8 boundaries — a naïve
        // `&trimmed[..200]` would panic on a multi-byte char.
        let cutoff = trimmed
            .char_indices()
            .nth(200)
            .map(|(i, _)| i)
            .unwrap_or(trimmed.len());
        format!("{}…", &trimmed[..cutoff])
    }
}

/// Prefix bounded by bytes without splitting a UTF-8 scalar. The
/// boundary walk examines at most three bytes and works on the
/// workspace MSRV (Rust 1.88).
fn body_prefix_bytes(body: &str, max_bytes: usize) -> &str {
    let mut end = body.len().min(max_bytes);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

fn next_page_cursor(
    page_info: Option<graphql::GqlPageInfo>,
    operation: &str,
) -> Result<Option<String>, GhError> {
    let page_info = page_info
        .ok_or_else(|| GhError::Graphql(format!("{operation}: response omitted pageInfo")))?;
    if !page_info.has_next_page {
        return Ok(None);
    }
    page_info.end_cursor.map(Some).ok_or_else(|| {
        GhError::Graphql(format!("{operation}: hasNextPage=true but endCursor=null"))
    })
}

#[derive(Debug)]
pub struct SelectedFetchOutcome {
    pub tasks: Vec<Task>,
    pub partial_failure: Option<String>,
    pub mentions: Vec<crate::LazyboxMention>,
    pub coverage: SelectedFetchCoverage,
}

/// Authoritative coverage completed by a selected GitHub fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectedFetchCoverage {
    /// Every requested side completed.
    Complete,
    /// At least one requested side was incomplete.
    Partial {
        /// Whether the PR side completed despite the partial result.
        pr_complete: bool,
    },
}

impl SelectedFetchCoverage {
    pub fn pr_complete(self) -> bool {
        match self {
            Self::Complete => true,
            Self::Partial { pr_complete } => pr_complete,
        }
    }

    pub fn sweep_complete(self) -> bool {
        self == Self::Complete
    }
}

#[derive(Debug)]
struct PrFetchOutcome {
    tasks: Vec<Task>,
    partial_failure: Option<String>,
}

impl PrFetchOutcome {
    fn complete(tasks: Vec<Task>) -> Self {
        Self {
            tasks,
            partial_failure: None,
        }
    }

    fn partial(tasks: Vec<Task>, partial_failure: String) -> Self {
        Self {
            tasks,
            partial_failure: Some(partial_failure),
        }
    }

    fn is_complete(&self) -> bool {
        self.partial_failure.is_none()
    }
}

type SelectedFetchResult = Result<SelectedFetchOutcome, GhError>;

fn combine_selected_fetches(
    pr_side_requested: bool,
    issue_side_requested: bool,
    prs: Result<PrFetchOutcome, GhError>,
    issues: Result<(Vec<Task>, Vec<crate::LazyboxMention>), GhError>,
) -> SelectedFetchResult {
    match (prs, issues) {
        (Ok(mut prs), Ok((issues, mentions))) => {
            let pr_complete = !pr_side_requested || prs.is_complete();
            prs.tasks.extend(issues);
            Ok(SelectedFetchOutcome {
                tasks: prs.tasks,
                partial_failure: prs.partial_failure,
                mentions,
                coverage: if pr_complete {
                    SelectedFetchCoverage::Complete
                } else {
                    SelectedFetchCoverage::Partial { pr_complete: false }
                },
            })
        }
        (Ok(prs), Err(error)) => {
            if !pr_side_requested {
                return Err(error);
            }
            let message = format!("issues sync failed (PRs OK): {error}");
            tracing::warn!("{message}");
            let pr_complete = prs.is_complete();
            let partial_failure = match prs.partial_failure {
                Some(pr_failure) => Some(format!("{pr_failure}; {message}")),
                None => Some(message),
            };
            Ok(SelectedFetchOutcome {
                tasks: prs.tasks,
                partial_failure,
                mentions: Vec::new(),
                coverage: SelectedFetchCoverage::Partial { pr_complete },
            })
        }
        (Err(error), Ok((issues, mentions))) => {
            if !issue_side_requested {
                return Err(error);
            }
            let message = format!("PRs sync failed (issues OK): {error}");
            tracing::warn!("{message}");
            Ok(SelectedFetchOutcome {
                tasks: issues,
                partial_failure: Some(message),
                mentions,
                coverage: SelectedFetchCoverage::Partial { pr_complete: false },
            })
        }
        (Err(pr_error), Err(issue_error)) => Err(GhError::Graphql(format!(
            "both PR and issue fetches failed: PRs={pr_error}; issues={issue_error}"
        ))),
    }
}

/// Construct a `GhError::HttpStatus` from a status + content-type +
/// body. Centralised so the canonical-reason lookup and the body
/// excerpting stay in sync between the raw-HTTP path and any future
/// callers (e.g. REST handlers that drop to `_get` similarly).
fn http_status_error(status: u16, content_type: &str, body: &str) -> GhError {
    // Canonical reason ("Bad Gateway" for 502, "Unauthorized" for
    // 401, …) wrapped in " (…)" so the Display string reads naturally.
    // Open-coded instead of pulling the `http` crate just to call
    // `StatusCode::canonical_reason()` — the list is short and stable.
    let reason_word = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    };
    let reason = if reason_word.is_empty() {
        String::new()
    } else {
        format!(" ({reason_word})")
    };
    GhError::HttpStatus {
        status,
        reason,
        content_type: content_type.to_string(),
        body_excerpt: body_excerpt(body),
    }
}

/// Strip snafu's location + backtrace dump from an error's
/// `Display` output. Octocrab's `Error` variants (Serde, Hyper,
/// etc.) use snafu, whose Display includes `Found at 0: ...` and a
/// trail of `/rustc/...` source paths — totally illegible to a
/// user, and our footer pipes it straight in. Take only what
/// comes before the first snafu marker and the first newline,
/// trim trailing whitespace.
///
/// Still in use for the remaining typed-octocrab call sites
/// (`/user`, `list_org_memberships`, repo listings, REST issue
/// comment). The new raw GraphQL path on issue #13 emits
/// `GhError::HttpStatus` directly and bypasses this entirely.
fn strip_error_backtrace(s: &str) -> String {
    // snafu's backtrace prelude is "Found at" on the line right
    // after the message. Cut everything from "Found at" onward.
    // Some snafu versions use "Caused by:" — handle both.
    let cut_at = s
        .find("\nFound at")
        .or_else(|| s.find("Found at"))
        .or_else(|| s.find("\nCaused by:"))
        .unwrap_or(s.len());
    let head = &s[..cut_at];
    head.lines().next().unwrap_or("").trim_end().to_string()
}

fn detail_of(err: &GhError) -> String {
    match err {
        GhError::Graphql(s) => s.clone(),
        GhError::RateLimited {
            retry_after_secs,
            reason,
        } => format!("{reason} (retry after {retry_after_secs}s)"),
        GhError::Truncated { count, pages } => format!(
            "GitHub returned {count} PRs across {pages} pages and we hit the safety cap. \
             Your filter likely matches too many PRs — narrow it in Settings."
        ),
        GhError::WatchAllFailed { count } => {
            format!("all {count} configured watched-repo queries failed (network or auth issue)")
        }
        GhError::HttpStatus { .. } => format!("{err}"),
        GhError::Api(octo) => match octo {
            octocrab::Error::GitHub { source, .. } => {
                // GitHubError's Display does the right thing —
                // includes status, message, docs URL, errors.
                format!("GitHub API ({}): {}", source.status_code, source)
            }
            // Serde / Json failures usually mean GitHub returned a
            // non-JSON body (502 page, redirect to login). Add a
            // hint to the technical message so the user knows
            // what's likely going on without us having to plumb
            // the raw response status through octocrab.
            octocrab::Error::Serde { .. } | octocrab::Error::Json { .. } => {
                let s = strip_error_backtrace(&format!("{other}", other = octo));
                format!("{s} (likely GitHub returned a non-JSON page — 502 / login redirect)")
            }
            other => strip_error_backtrace(&format!("{other}")),
        },
    }
}

impl From<GhError> for lazybox_core::ProviderError {
    /// Classify GitHub failures so polling knows whether to retry, all
    /// via the shared `lazybox_core` classifier so GitHub and Linear
    /// can't disagree about the same failure. Routing, in order:
    /// - A typed HTTP status (octocrab `GitHub` error, or the raw
    ///   GraphQL path's `HttpStatus`) → `classify_status`: 401/403 →
    ///   Auth (rotate token), 429 + 5xx → Retryable, else Permanent.
    ///   `HttpStatus` keeps one GitHub-specific pre-check: a 2xx with a
    ///   non-JSON body (proxy/CDN maintenance page) → Retryable.
    /// - Transport octocrab variants (Hyper/Service/Http/Serde/Json/
    ///   Uri) → Retryable by definition; no data was ever returned.
    /// - Everything with no typed status (GraphQL wrapper strings,
    ///   future variants) → the shared substring probe, which can also
    ///   mint Auth when the message carries "unauthorized"/"forbidden"/
    ///   "401"/"403" — reached only here, never for the transport
    ///   variants above, so a transient hyper/json chain that happens
    ///   to mention either word still classifies Retryable.
    fn from(err: GhError) -> Self {
        const SOURCE: &str = "github";
        let detail = detail_of(&err);

        // Rate-budget refusals carry an exact `retry_after_secs`
        // hint — preserve it into `ProviderError::Retryable` so the
        // polling driver can sleep until the reset window opens
        // instead of retrying on its normal cadence and burning the
        // same error repeatedly.
        if let GhError::RateLimited {
            retry_after_secs, ..
        } = &err
        {
            return lazybox_core::ProviderError::retryable_after(SOURCE, detail, *retry_after_secs);
        }

        // Status-aware classification when we have an octocrab
        // GitHub error: the shared classifier maps 401/403 → auth,
        // 5xx + 429 → retryable, other statuses → permanent. Feeding
        // it the *typed* status (never a substring probe) is what
        // keeps "unauthorized"/"forbidden" mentions inside transient
        // hyper/json chains from producing false `Auth` verdicts.
        if let GhError::Api(octocrab::Error::GitHub { source, .. }) = &err {
            return lazybox_core::classify_status(source.status_code.as_u16())
                .into_provider_error(SOURCE, detail);
        }

        // Same status-aware classification for `HttpStatus`, the
        // variant emitted by the raw GraphQL path — plus one
        // GitHub-specific quirk: a 2xx with a non-JSON body almost
        // always means a proxy / CDN intercepted the call with an HTML
        // maintenance page even though the upstream eventually came
        // back, so it's worth one retry rather than the permanent
        // verdict a bare 2xx would earn.
        if let GhError::HttpStatus {
            status,
            content_type,
            ..
        } = &err
        {
            if (200..=299).contains(status) && !content_type_is_json(content_type) {
                return lazybox_core::ProviderError::retryable(SOURCE, detail);
            }
            return lazybox_core::classify_status(*status).into_provider_error(SOURCE, detail);
        }

        // Everything else has no typed HTTP status. Transport-layer
        // octocrab variants (no PR/issue data was ever returned) are
        // retryable by definition; the rest fall through to the
        // shared substring probe. Both routes go through the one
        // `classify` so this provider can't disagree with Linear.
        let transport = matches!(
            &err,
            GhError::Api(
                octocrab::Error::Hyper { .. }
                    | octocrab::Error::Service { .. }
                    | octocrab::Error::Http { .. }
                    | octocrab::Error::Serde { .. }
                    | octocrab::Error::Json { .. }
                    | octocrab::Error::UriParse { .. }
                    | octocrab::Error::Uri { .. }
            )
        );
        let class = lazybox_core::classify(&lazybox_core::HttpErrorSignals {
            status: None,
            transport,
            message: &detail,
        });
        class.into_provider_error(SOURCE, detail)
    }
}

/// Search query for one watched repo's open-PR fan-out.
///
/// The `-involves:USER` exclusion is the fix for issue #15. A watched
/// repo exists to surface PRs the user is *not* otherwise part of; the
/// PRs they *are* involved in already come back through the main
/// `involves:USER` branch in the very same poll, so fetching them here
/// too is pure duplicate download (measured at 17% of the union, and
/// 89.6 KB on one busy repo). Negating `involves` server-side keeps
/// only the genuinely-new set, dropping both the redundant bytes and
/// the cross-branch dedup waste.
fn watched_repo_query(repo: &str, user: &str) -> String {
    format!("is:open is:pr repo:{repo} archived:false -involves:{user}")
}

/// Short, non-reversible fingerprint of a credential token. Lets the
/// polling layer's client cache detect "the token material changed"
/// (rotation via `gh auth refresh`, a new `GH_TOKEN`, …) without
/// storing or comparing the raw secret: the credential *source label*
/// is constant across rotations ("cmd:gh auth token" forever), so a
/// label-only comparison kept serving the startup token until daemon
/// restart. Not cryptographic — only ever compared against another
/// fingerprint produced by this same function in the same process.
pub fn credential_fingerprint(token: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Clone)]
pub struct GhClient {
    inner: Octocrab,
    user: String,
    credential_source: String,
    /// Fingerprint of the token this client was built with — see
    /// [`credential_fingerprint`]. Cache-reuse checks compare this so
    /// a rotated token rebuilds the client even when the source label
    /// is unchanged.
    credential_fingerprint: String,
    /// Search qualifiers used by `fetch_all_prs` (PR-only — built
    /// from `pr.*` keys plus scope).
    pr_filters: Vec<String>,
    /// Search qualifiers used by `fetch_all_issues` (Issue-only —
    /// built from `issue.*` keys plus scope).
    issue_filters: Vec<String>,
    watch_repos: Vec<String>,
    /// Two-layer rate budget. See `crate::rate_budget`.
    /// `Arc<Mutex>` so multiple `GhClient` clones share one bucket
    /// (currently we only construct one, but cheap insurance against
    /// future "spawn a worker pool" ideas).
    budget: std::sync::Arc<parking_lot::Mutex<crate::rate_budget::RateBudget>>,
    /// Notifications heartbeat state — `Last-Modified` echo + slow-sweep
    /// timer. Shared across clones so `with_filters` doesn't reset the
    /// 304-conditional or trigger a redundant full sweep.
    notifications_state: SharedNotificationsState,
}

/// Per-branch cost breakdown for one branch of a PR fetch, emitted
/// under the `gh_sync_metrics` tracing target so a real poll can be
/// profiled without changing the default log output. See
/// `docs/sync-performance.md` for how to capture and read a trace.
#[derive(Debug, Default, Clone)]
struct BranchMetrics {
    /// Branch label: `involves-main`, `review-requested`,
    /// `merged-sweep`, `watched-repo`, `pr-details`, …
    branch: &'static str,
    /// Wall-clock from branch entry to all pages parsed.
    elapsed_ms: u128,
    /// HTTP round-trips the branch made (pages for the paginated
    /// search; always 1 for single-shot queries).
    requests: usize,
    /// PRs the branch returned *before* cross-branch dedup.
    prs: usize,
    /// Sum of GitHub's reported GraphQL `rateLimit.cost` across this
    /// branch's requests. 0 if GitHub didn't report it.
    graphql_cost: u32,
    /// Raw response bytes deserialized across this branch's requests.
    resp_bytes: usize,
}

impl BranchMetrics {
    fn new(branch: &'static str) -> Self {
        Self {
            branch,
            ..Default::default()
        }
    }

    /// Emit the breakdown. DEBUG so it's off by default and costs
    /// nothing until `RUST_LOG=gh_sync_metrics=debug` turns it on.
    fn emit(&self) {
        tracing::debug!(
            target: "gh_sync_metrics",
            branch = self.branch,
            elapsed_ms = self.elapsed_ms as u64,
            requests = self.requests,
            prs = self.prs,
            graphql_cost = self.graphql_cost,
            resp_bytes = self.resp_bytes,
            "branch fetch complete",
        );
    }
}

impl GhClient {
    pub async fn from_credential(cred: Credential) -> Result<Self, GhError> {
        let source = cred.source.clone();
        let fingerprint = credential_fingerprint(cred.token());
        // Disable octocrab's built-in retry: its `OctoBody` clone only
        // Arc-clones a single-use body stream, so on a 429/5xx retry the
        // second attempt goes out with an empty `{}` body. GitHub answers
        // with the infamous "A query attribute must be specified and must
        // be a string" — ~1 in every 5 GraphQL polls during rate-limited
        // periods. We eat the retry feature; polling runs every few seconds
        // so we just try again on the next tick.
        // Global HTTP timeouts so no single request can hang the
        // caller indefinitely — `current().user()` below runs on the
        // poll loop's critical path (outside the per-tick timeout),
        // and a dead TCP connection without these waited forever.
        // 30s is far above any healthy GitHub round-trip yet bounded.
        let inner = Octocrab::builder()
            .personal_token(cred.into_token())
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .set_connect_timeout(Some(std::time::Duration::from_secs(30)))
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .build()
            .map_err(GhError::Api)?;
        let user = inner.current().user().await.map_err(GhError::Api)?.login;
        Ok(Self {
            inner,
            user,
            credential_source: source,
            credential_fingerprint: fingerprint,
            pr_filters: vec![],
            issue_filters: vec![],
            watch_repos: vec![],
            budget: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::rate_budget::RateBudget::default_for_lazybox(),
            )),
            notifications_state: NotificationsState::shared(),
        })
    }

    /// Test-only: a `GhClient` with the given credential identity and
    /// a default (never-called) transport. Lets server-side tests seed
    /// the daemon's client cache without a network round-trip.
    #[doc(hidden)]
    pub fn stub_for_tests(
        credential_source: &str,
        credential_fingerprint: &str,
    ) -> Result<Self, GhError> {
        let inner = Octocrab::builder().build().map_err(GhError::Api)?;
        Ok(Self {
            inner,
            user: "test-user".to_string(),
            credential_source: credential_source.to_string(),
            credential_fingerprint: credential_fingerprint.to_string(),
            pr_filters: vec![],
            issue_filters: vec![],
            watch_repos: vec![],
            budget: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::rate_budget::RateBudget::default_for_lazybox(),
            )),
            notifications_state: NotificationsState::shared(),
        })
    }

    /// Snapshot of the current rate budget state. Used by the polling
    /// layer to surface a status indicator and decide pacing.
    pub fn rate_snapshot(&self) -> crate::rate_budget::Snapshot {
        self.budget.lock().snapshot()
    }

    /// The exact GraphQL search string `fetch_all_prs` will issue.
    /// Exposed so the polling layer / TUI can show the user what
    /// query is actually running — invaluable when debugging "why
    /// did this return 0 results?".
    pub fn pr_search_query(&self) -> String {
        let mut quals = graphql::default_search_qualifiers();
        if self.pr_filters.is_empty() {
            quals.push(format!("involves:{}", self.user));
        } else {
            quals.extend(self.pr_filters.iter().cloned());
        }
        graphql::build_query(&quals)
    }

    /// Same as `pr_search_query` but for the issue search.
    pub fn issue_search_query(&self) -> String {
        let mut quals = graphql::default_issues_qualifiers();
        if self.issue_filters.is_empty() {
            quals.push(format!("involves:{}", self.user));
        } else {
            quals.extend(self.issue_filters.iter().cloned());
        }
        graphql::build_query(&quals)
    }

    /// Try to spend one rate-budget token. Caller must NOT make a
    /// GraphQL request on `Err` — that's the whole point of the
    /// budget. Caller should propagate the `AcquireError` so the
    /// polling layer can surface it as a `Retryable` ProviderError.
    fn try_acquire(&self) -> Result<(), crate::rate_budget::AcquireError> {
        self.budget.lock().try_acquire()
    }

    /// POST to `/graphql` with bounded exponential backoff on
    /// transient errors.
    ///
    /// Drops to octocrab's raw `_post` API so we can inspect the
    /// HTTP status + content-type BEFORE attempting to parse. With
    /// the typed `post::<_, T>` path, a non-JSON response (502
    /// maintenance page, login redirect when the token has expired,
    /// gateway error) surfaced as the opaque
    /// `Serde Error: expected value at line 1 column 1` —
    /// users had no idea what actually went wrong. Now we
    /// classify on the actual status code and the body excerpt
    /// reaches the footer / logs.
    ///
    /// The body is borrowed (octocrab takes `Option<&B>`), so the
    /// SAME body bytes go out on every attempt — no risk of the
    /// body-clone bug that made us disable octocrab's built-in
    /// retry. The caller has already spent a rate-budget token via
    /// `acquire_or_block`; we do NOT re-acquire per retry (a
    /// single 502-then-retry-success is one logical call from
    /// GitHub's perspective).
    ///
    /// Retry policy:
    /// - Transport variants (Hyper, Service, Http, Json, Io) →
    ///   retry. Always transient.
    /// - 5xx → ONE in-call retry, then surface (the poll-level
    ///   backoff spaces further repeats).
    /// - 2xx with a non-JSON body → retry (proxy/CDN bait).
    /// - 429 / secondary-limit 403 → NEVER retried here; surfaced as
    ///   `RateLimited` with the server's `Retry-After` so the poll
    ///   scheduler sleeps out the real window.
    /// - Anything else (auth, validation, 2xx-JSON schema mismatch)
    ///   → return immediately. Retrying wouldn't change the answer.
    ///
    /// Backoff sequence: 200ms → 800ms. Two retries after the
    /// initial attempt = 3 attempts total. Tight enough that a
    /// 60s poll cycle still has headroom; long enough that the
    /// usual <1s blip resolves itself.
    async fn post_graphql_with_retry<T>(&self, body: &serde_json::Value) -> Result<T, GhError>
    where
        T: serde::de::DeserializeOwned,
    {
        self.post_graphql_with_retry_measured(body)
            .await
            .map(|(parsed, _bytes)| parsed)
    }

    /// Like [`Self::post_graphql_with_retry`] but also surfaces the byte
    /// length of the successful response body. Used by the PR-fetch
    /// path to record per-branch response size for sync profiling.
    async fn post_graphql_with_retry_measured<T>(
        &self,
        body: &serde_json::Value,
    ) -> Result<(T, usize), GhError>
    where
        T: serde::de::DeserializeOwned,
    {
        const DELAYS_MS: &[u64] = &[200, 800];
        // Per-request wall-clock cap. The default reqwest client has
        // no timeout — a flaky network can leave the HTTP call
        // hanging forever, which the user perceives as "lazybox's
        // sync froze." 25s is generous (a real PR search rarely
        // breaks 5s) but well under the 90s spinner guard so a hung
        // call surfaces as an error before the UI gives up.
        const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);
        let mut last_err: Option<GhError> = None;
        // `0..=DELAYS_MS.len()` is intentional: we try once at
        // attempt=0 (no delay yet), then once per entry of
        // `DELAYS_MS`. The index is used both as the sleep offset
        // and in the log line, so an enumerate-over-delays rewrite
        // would be less clear than just allowing the lint here.
        #[allow(
            clippy::needless_range_loop,
            reason = "the index is the attempt count; see comment above"
        )]
        for attempt in 0..=DELAYS_MS.len() {
            let outcome = match tokio::time::timeout(
                REQUEST_TIMEOUT,
                self.post_graphql_once::<T>(body),
            )
            .await
            {
                Ok(r) => r,
                Err(_elapsed) => {
                    tracing::warn!(
                        "graphql request exceeded {}s wall-clock; \
                             attempt {}/{} will retry",
                        REQUEST_TIMEOUT.as_secs(),
                        attempt + 1,
                        DELAYS_MS.len() + 1,
                    );
                    // Synthesise an HttpStatus error so retry +
                    // classification share the same shape as the
                    // rest of this loop. status=0 is the "no
                    // response" sentinel — it lands as
                    // `Retryable` in `From<GhError>` via the
                    // 2xx-non-JSON branch (status=0 isn't 2xx,
                    // so we explicitly retry it here too).
                    Err(GhError::HttpStatus {
                        status: 0,
                        reason: String::new(),
                        content_type: String::new(),
                        body_excerpt: format!(
                            "graphql request timed out after {}s",
                            REQUEST_TIMEOUT.as_secs()
                        ),
                    })
                }
            };
            match outcome {
                Ok(v) => {
                    if attempt > 0 {
                        tracing::info!("graphql request succeeded on retry {attempt}");
                    }
                    return Ok(v);
                }
                Err(e) => {
                    let transient =
                        is_transient(&e) || matches!(&e, GhError::HttpStatus { status: 0, .. });
                    if !transient {
                        return Err(e);
                    }
                    // 5xx: one in-call retry only. A single blip
                    // usually resolves within the first 200ms retry;
                    // a sustained outage should be spaced by the
                    // poll-level backoff, not burned down the whole
                    // millisecond ladder.
                    let allowed_retries = if is_server_error(&e) {
                        1
                    } else {
                        DELAYS_MS.len()
                    };
                    if attempt >= allowed_retries {
                        return Err(e);
                    }
                    if attempt < DELAYS_MS.len() {
                        let delay_ms = DELAYS_MS[attempt];
                        tracing::warn!(
                            "graphql transient error (attempt {}/{}), retrying in {delay_ms}ms: {e}",
                            attempt + 1,
                            DELAYS_MS.len() + 1,
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.expect("loop runs at least once"))
    }

    /// One round-trip to `/graphql` with status-aware error
    /// surfacing. Bypasses octocrab's typed `post::<_, T>` because
    /// that path swallows the HTTP status on non-JSON responses
    /// (returns `octocrab::Error::Serde` with no access to the raw
    /// body), which is the bug in #13.
    /// Returns the deserialized response alongside the byte length of
    /// the raw HTTP body we parsed — the PR-fetch path uses the byte
    /// count to attribute response size per branch in
    /// `gh_sync_metrics`. Callers that don't care wrap this via
    /// `post_graphql_with_retry` and drop the size.
    async fn post_graphql_once<T>(&self, body: &serde_json::Value) -> Result<(T, usize), GhError>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = self
            .inner
            ._post("/graphql", Some(body))
            .await
            .map_err(GhError::Api)?;
        let status = response.status().as_u16();
        // `HeaderMap::get` is case-insensitive, so lowercase here is
        // both the canonical form and what octocrab uses internally.
        let header = |name: &str| -> Option<String> {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header("content-type").unwrap_or_default();
        // Rate-limit hints ride the headers of the FAILURE response —
        // read them before the body is consumed. On successful parses
        // the GraphQL `rateLimit` field feeds the budget instead.
        let rate_headers = RateLimitHeaders::parse(
            header("retry-after").as_deref(),
            header("x-ratelimit-remaining").as_deref(),
            header("x-ratelimit-reset").as_deref(),
        );
        let raw_body = self
            .inner
            .body_to_string(response)
            .await
            .map_err(GhError::Api)?;
        let byte_len = raw_body.len();
        // Non-2xx or non-JSON: never attempt to parse — the body is
        // an HTML page / login redirect / GitHub error JSON we'd
        // rather surface verbatim than mis-deserialise.
        if !(200..=299).contains(&status) || !content_type_is_json(&content_type) {
            // Throttle responses (429, or 403 carrying the documented
            // secondary-limit markers) are special-cased BEFORE the
            // generic status error: they must surface as `RateLimited`
            // with the server's own wait hint so (a) the in-call retry
            // ladder never hot-retries them and (b) the poll scheduler
            // sleeps out the real window instead of its base cadence.
            if is_rate_limit_response(status, &raw_body, rate_headers.retry_after_secs.is_some()) {
                let now = chrono::Utc::now();
                let retry_after_secs = rate_headers.wait_secs(now.timestamp().max(0) as u64);
                // Feed the observation into the shared budget so
                // admission control stops admitting until the window
                // reopens. `remaining` is recorded as 0 regardless of
                // the header: GitHub is actively refusing requests
                // (secondary limits fire with primary quota left), so
                // the *effective* remaining is zero until reset.
                self.budget
                    .lock()
                    .observe(crate::rate_budget::RemoteRateLimit {
                        remaining: 0,
                        limit: 0,
                        reset_at: now
                            + chrono::Duration::seconds(
                                retry_after_secs.min(i64::MAX as u64) as i64
                            ),
                        observed_at: std::time::Instant::now(),
                    });
                return Err(GhError::RateLimited {
                    retry_after_secs,
                    reason: format!(
                        "github answered HTTP {status} ({})",
                        body_excerpt(&raw_body)
                    ),
                });
            }
            return Err(http_status_error(status, &content_type, &raw_body));
        }
        // 2xx + JSON content-type: this is the success path. A parse
        // failure here is a real schema mismatch between our types
        // and GitHub's response — surface it with status + content-
        // type intact instead of dropping to `Serde`. The raw body
        // goes to `tracing` only: it can carry the full GraphQL
        // response (node payloads, JSON braces) which must never reach
        // a user-facing footer notice (issue #305).
        serde_json::from_str::<T>(&raw_body)
            .map(|parsed| (parsed, byte_len))
            .map_err(|e| {
                tracing::warn!(
                    "graphql 2xx response failed to parse ({e}); body: {}",
                    body_excerpt(&raw_body)
                );
                GhError::HttpStatus {
                    status,
                    reason: " (json parse failed)".to_string(),
                    content_type,
                    body_excerpt: e.to_string(),
                }
            })
    }

    /// Gate-or-fail: spend one rate-budget token and convert the
    /// `AcquireError` into a `GhError::Graphql` carrying the
    /// human-readable reason. Every code path that fires an HTTP
    /// request to GitHub (GraphQL search, GraphQL mutation, REST
    /// scope/repo listing, REST issue comment) goes through this
    /// helper so the budget is the single chokepoint — no more
    /// "I forgot to gate this one site" footguns.
    ///
    /// `op` is a short label for the log warning ("watch-repo query",
    /// "merge mutation"). It doesn't go in the error payload — the
    /// underlying `AcquireError` Display already describes the
    /// situation.
    fn acquire_or_block(&self, op: &str) -> Result<(), GhError> {
        if let Err(reason) = self.try_acquire() {
            tracing::warn!("{op} blocked by rate budget: {reason}");
            let retry_after_secs = match &reason {
                crate::rate_budget::AcquireError::LocalBudgetExhausted { wait_secs } => *wait_secs,
                crate::rate_budget::AcquireError::RemoteLow { reset_at, .. } => {
                    // `reset_at` is in the future when this fires (the
                    // budget check is `reset_at > now`); clamp to >=1
                    // so we always sleep at least a second instead of
                    // tight-looping if the wall clock is slewing.
                    let now = chrono::Utc::now();
                    let secs = (*reset_at - now).num_seconds();
                    secs.max(1) as u64
                }
            };
            return Err(GhError::RateLimited {
                retry_after_secs,
                reason: reason.to_string(),
            });
        }
        Ok(())
    }

    /// Record GitHub's reported rate-limit. Wired into every
    /// successful GraphQL response that includes the `rateLimit`
    /// field.
    fn observe_rate_limit(&self, ratelimit: &graphql::GqlRateLimit) {
        let reset_at = chrono::DateTime::parse_from_rfc3339(&ratelimit.reset_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .ok();
        let Some(reset_at) = reset_at else { return };
        let observed = crate::rate_budget::RemoteRateLimit {
            remaining: ratelimit.remaining,
            limit: ratelimit.limit,
            reset_at,
            observed_at: std::time::Instant::now(),
        };
        self.budget.lock().observe(observed);
    }

    /// Set both PR and Issue search qualifiers. Polling builds these
    /// from the user's per-type role keys (`pr.*` / `issue.*`).
    pub fn with_filters(mut self, pr_filters: Vec<String>, issue_filters: Vec<String>) -> Self {
        self.pr_filters = pr_filters;
        self.issue_filters = issue_filters;
        self
    }

    pub fn with_watch_repos(mut self, repos: Vec<String>) -> Self {
        self.watch_repos = repos;
        self
    }

    /// Hydrate the user's GitHub namespace as a list of org scopes:
    /// every org they belong to, plus their personal-repo "org"
    /// (their login). Repos under each org are NOT enumerated here
    /// — `list_repos_in_org` is the lazy follow-up the picker calls
    /// once the user drills into an org.
    pub async fn list_scopes(&self) -> Result<Vec<Scope>, GhError> {
        let mut scopes = Vec::new();

        // The user's own login is always available as a "personal"
        // scope, covering their own-account repos. We surface it
        // first so it shows up at the top of the picker.
        if !self.user.is_empty() {
            scopes.push(Scope {
                id: format!("github:{}", self.user),
                label: self.user.clone(),
                parent: None,
                kind: ScopeKind::Org,
            });
        }

        // Orgs the user belongs to. REST endpoint; counts against the
        // same budget. Setup-wizard call so cost-per-poll impact is
        // zero, but gating keeps the invariant ("every GitHub HTTP
        // request goes through the budget") clean.
        self.acquire_or_block("list org memberships")?;
        let orgs: Vec<octocrab::models::orgs::Organization> = self
            .inner
            .current()
            .list_org_memberships_for_authenticated_user()
            .send()
            .await
            .map_err(GhError::Api)?
            .items
            .into_iter()
            .map(|m| m.organization)
            .collect();

        for org in &orgs {
            // Skip if the user's login is also an org name (rare but
            // possible) — already added above.
            if org.login == self.user {
                continue;
            }
            scopes.push(Scope {
                id: format!("github:{}", org.login),
                label: org.login.clone(),
                parent: None,
                kind: ScopeKind::Org,
            });
        }

        Ok(scopes)
    }

    /// List repositories under `parent_id` (e.g. `"github:acme"`).
    /// Called lazily by the picker once the user has drilled into
    /// an org. Returns `Scope`s of kind `Repo` parented at the org.
    /// `parent_id` is stripped of the `github:` prefix to derive
    /// the org name; unknown prefixes return empty.
    pub async fn list_repos_in_org(&self, parent_id: &str) -> Result<Vec<Scope>, GhError> {
        let Some(owner) = parent_id.strip_prefix("github:") else {
            return Ok(Vec::new());
        };
        // The user's own login uses `/user/repos` (which lists
        // owner-affiliated repos including private). Other orgs use
        // `/orgs/{org}/repos`, which respects org membership.
        let mut scopes = Vec::new();
        // Each page is a separate REST request against the same
        // hourly quota — gate every page, not just the first.
        if owner == self.user {
            self.acquire_or_block("list own repos page 1")?;
            let mut page = self
                .inner
                .current()
                .list_repos_for_authenticated_user()
                .type_("owner")
                .per_page(100)
                .send()
                .await
                .map_err(GhError::Api)?;
            loop {
                for repo in &page.items {
                    let full = repo
                        .full_name
                        .clone()
                        .unwrap_or_else(|| format!("{owner}/{}", repo.name));
                    scopes.push(Scope {
                        id: format!("github:{full}"),
                        label: full,
                        parent: Some(parent_id.to_string()),
                        kind: ScopeKind::Repo,
                    });
                }
                self.acquire_or_block("list own repos next page")?;
                page = match self
                    .inner
                    .get_page::<octocrab::models::Repository>(&page.next)
                    .await
                    .map_err(GhError::Api)?
                {
                    Some(next) => next,
                    None => break,
                };
            }
        } else {
            self.acquire_or_block("list org repos page 1")?;
            let mut page = self
                .inner
                .orgs(owner)
                .list_repos()
                .per_page(100)
                .send()
                .await
                .map_err(GhError::Api)?;
            loop {
                for repo in &page.items {
                    let full = repo
                        .full_name
                        .clone()
                        .unwrap_or_else(|| format!("{owner}/{}", repo.name));
                    scopes.push(Scope {
                        id: format!("github:{full}"),
                        label: full,
                        parent: Some(parent_id.to_string()),
                        kind: ScopeKind::Repo,
                    });
                }
                self.acquire_or_block("list org repos next page")?;
                page = match self
                    .inner
                    .get_page::<octocrab::models::Repository>(&page.next)
                    .await
                    .map_err(GhError::Api)?
                {
                    Some(next) => next,
                    None => break,
                };
            }
        }
        Ok(scopes)
    }

    pub fn with_needs_reply(self, _enabled: bool) -> Self {
        self
    }

    pub fn username(&self) -> &str {
        &self.user
    }

    pub fn credential_source(&self) -> &str {
        &self.credential_source
    }

    /// Fingerprint of the token this client authenticates with — a
    /// short hash, never the raw secret. See the module-level
    /// [`credential_fingerprint`] for why cache layers compare this
    /// in addition to the (rotation-stable) source label.
    pub fn credential_fingerprint(&self) -> &str {
        &self.credential_fingerprint
    }

    /// Fetch ALL relevant PRs in a single GraphQL query.
    /// `involves:username` covers author, reviewer, assignee, mentioned.
    /// **One API call instead of 68.**
    pub fn authenticated_user(&self) -> &str {
        &self.user
    }

    /// Default cadence for the slow full-sweep. The notifications
    /// heartbeat runs every poll tick (cheap); the heavy `involves:USER`
    /// search runs at most once every `FULL_SWEEP_INTERVAL`. Picks the
    /// 10-minute number from #19 — long enough to keep GraphQL cost
    /// down by an order of magnitude, short enough that the long-tail
    /// gap (a PR notifications didn't tell us about, e.g. a silent
    /// `closed` with no comment) closes within a coffee break.
    pub const FULL_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

    /// Cadence for the UNWINDOWED reconcile sweep (issue #14). Most
    /// global sweeps narrow the `involves:` search to
    /// `updated:>=<last sweep>` so a steady inbox collapses the
    /// dominant `involves-main` branch from ~14s/143 KB to a near-empty
    /// first page. That windowed search can't observe PRs that left the
    /// window without bumping `updatedAt` — a silently-closed PR, one
    /// the user got un-involved from, a transfer — so every
    /// `FULL_RECONCILE_INTERVAL` (and on manual refresh / the first
    /// sweep after start) one sweep drops the window and re-reconciles
    /// the whole inbox. The reconcile sweep is also the only one that
    /// reports exhaustive coverage, i.e. the only one allowed to drive
    /// rescope deletion. An hour balances "delete stale rows promptly"
    /// against "pay the heavy 143 KB download rarely."
    pub const FULL_RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

    /// Should the next sync cycle run a heavy full sweep, or is the
    /// notifications-driven incremental path safe to use? Returns true
    /// when no sweep has run yet (first tick after daemon start) or
    /// when the last one was ≥ `FULL_SWEEP_INTERVAL` ago.
    ///
    /// The polling layer calls this BEFORE deciding which fetch to fire
    /// — so a true response means "the rescope-eligible source is in
    /// play this tick" and the tick driver knows to honor rescoping;
    /// false means the tick is an incremental refresh and rescope
    /// must be skipped (covered by `FetchMode::Incremental`).
    pub fn should_full_sweep(&self) -> bool {
        self.notifications_state
            .lock()
            .is_full_sweep_due(Self::FULL_SWEEP_INTERVAL)
    }

    /// Mark a full sweep complete *now*. Seeds the heartbeat baseline
    /// when one isn't already set so the very next incremental tick
    /// has an `If-Modified-Since` to send — without it the first
    /// post-bootstrap heartbeat would unconditionally pull every
    /// notification GitHub holds for the user.
    ///
    /// We do NOT overwrite an existing `last_modified`: an authoritative
    /// header from the notifications endpoint is always preferable to
    /// a locally-synthesized timestamp (clock skew can make our `now`
    /// *earlier* than GitHub's, which would re-deliver entries the
    /// heartbeat already covered).
    pub fn mark_full_sweep_done(&self) {
        let mut state = self.notifications_state.lock();
        state.last_full_sweep_at = Some(std::time::Instant::now());
        state.force_full_sweep = false;
        if state.last_modified.is_none() {
            state.last_modified = Some(notifications::format_http_date(chrono::Utc::now()));
        }
    }

    /// Decide the `updated:>=` floor for the next GLOBAL `involves:` PR
    /// sweep (issue #14). `None` → run an unwindowed reconcile (first
    /// sweep, manual refresh, or the reconcile cadence elapsed): fetch
    /// every involved PR so rescope can delete the gone ones. `Some(ts)`
    /// → windowed sweep: only PRs updated since the previous sweep,
    /// skipping the unchanged majority on a steady inbox.
    ///
    /// Read-only — the caller commits the outcome via
    /// [`record_pr_sweep_window`](Self::record_pr_sweep_window) once the
    /// sweep succeeds. Only meaningful on a global sweep tick; the
    /// round-robin per-repo path doesn't use a window.
    pub fn next_pr_sweep_window(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let state = self.notifications_state.lock();
        if state.is_full_reconcile_due(Self::FULL_RECONCILE_INTERVAL) {
            return None;
        }
        state.last_pr_sweep_at_utc
    }

    /// Record a completed GLOBAL `involves:` PR sweep. `sweep_started`
    /// (captured BEFORE the fetch, so a PR touched mid-sweep is caught
    /// next time) becomes the `updated:>=` floor for the next windowed
    /// sweep; a reconcile additionally re-arms the reconcile timer.
    ///
    /// Called ONLY when the tick actually ran the global search — a
    /// round-robin per-repo full sweep never advances the floor, since
    /// it didn't look at the whole involved set and moving the floor
    /// past PRs it never fetched would drop them from the next window.
    pub fn record_pr_sweep_window(
        &self,
        sweep_started: chrono::DateTime<chrono::Utc>,
        was_reconcile: bool,
    ) {
        let mut state = self.notifications_state.lock();
        state.last_pr_sweep_at_utc = Some(sweep_started);
        if was_reconcile {
            state.last_full_reconcile_at = Some(std::time::Instant::now());
        }
    }

    /// The `updated:>=` floor for the next windowed merged sweep (issue
    /// #530), or `None` to run it unwindowed. Independent of the main
    /// `involves:` floor — see `last_merged_sweep_at_utc`. Internal to
    /// `fetch_all_prs`; the round-robin path never windows the merged
    /// sweep.
    fn merged_sweep_window(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.notifications_state.lock().last_merged_sweep_at_utc
    }

    /// Record a merged sweep that actually SUCCEEDED. `sweep_started`
    /// (captured before the fetch) becomes the floor for the next
    /// windowed merged sweep. Called only on merged-branch success, so a
    /// transient merged failure leaves the floor put and the next sweep
    /// re-covers the gap rather than windowing past it.
    fn record_merged_sweep_window(&self, sweep_started: chrono::DateTime<chrono::Utc>) {
        self.notifications_state.lock().last_merged_sweep_at_utc = Some(sweep_started);
    }

    /// Arm the next tick to run a full sweep, bypassing the
    /// `FULL_SWEEP_INTERVAL` gate. Used by the manual `Command::Refresh`
    /// path so a freshly created issue/PR surfaces immediately rather
    /// than waiting up to 10 min for the next scheduled sweep — the
    /// incremental notifications path never sees an issue the user
    /// created themselves (no self-notification). The flag is one-shot:
    /// `mark_full_sweep_done` clears it once the sweep completes.
    pub fn force_full_sweep(&self) {
        self.notifications_state.lock().force_full_sweep = true;
    }

    /// Snapshot of the current notifications heartbeat state. Read-only;
    /// exists so tests (and a future status indicator) can observe
    /// whether the slow-sweep timer is armed.
    pub fn notifications_snapshot(&self) -> NotificationsSnapshot {
        let s = self.notifications_state.lock();
        NotificationsSnapshot {
            has_last_modified: s.last_modified.is_some(),
            last_full_sweep_elapsed: s.last_full_sweep_at.map(|i| i.elapsed()),
            heartbeat_backed_off: s.heartbeat_backed_off(),
        }
    }

    /// How long to skip the notifications heartbeat after a failure.
    /// Matched to [`Self::FULL_SWEEP_INTERVAL`] so a single chronic
    /// auth/rate-limit problem costs at most one extra round-trip per
    /// sweep cycle — and clears itself the moment the user fixes
    /// their token. Shorter would re-fire the broken heartbeat
    /// repeatedly during an outage; longer would mask a transient
    /// blip.
    const HEARTBEAT_BACK_OFF: std::time::Duration = std::time::Duration::from_secs(600);

    /// Record a heartbeat success — clears any prior back-off so the
    /// next tick uses the cheap incremental path again.
    fn note_heartbeat_succeeded(&self) {
        let mut state = self.notifications_state.lock();
        if state.heartbeat_back_off_until.is_some() {
            tracing::info!("notifications heartbeat recovered — clearing back-off");
            state.heartbeat_back_off_until = None;
        }
    }

    /// Arm the heartbeat back-off window. The polling layer's
    /// `should_full_sweep` honors this and bypasses the heartbeat
    /// round-trip until the deadline passes. Idempotent — re-arming
    /// while already backed off just extends the window.
    fn note_heartbeat_failed(&self) {
        let mut state = self.notifications_state.lock();
        let deadline = std::time::Instant::now() + Self::HEARTBEAT_BACK_OFF;
        let was_armed = state.heartbeat_back_off_until.is_some();
        state.heartbeat_back_off_until = Some(deadline);
        if !was_armed {
            tracing::warn!(
                back_off_secs = Self::HEARTBEAT_BACK_OFF.as_secs(),
                "notifications heartbeat failed — backing off; full sweeps continue",
            );
        }
    }

    /// Cheap heartbeat against `GET /notifications` with
    /// `If-Modified-Since` set to the previously-observed `Last-Modified`.
    ///
    /// - **304 No Content** → returns `NotificationsPoll::NotModified`;
    ///   caller can skip the deep-fetch entirely. This is the steady
    ///   state — most ticks land here.
    /// - **200 OK** → deserializes the body into `NotificationEntry`s
    ///   and captures the response's `Last-Modified` header for echo
    ///   on the next call. Caller fans out to single-PR / single-issue
    ///   deep-fetches keyed off each entry's `subject.url`.
    /// - Anything else → bubbled as `GhError`.
    ///
    /// Notifications are rate-budgeted SEPARATELY from GraphQL (REST has
    /// its own 5000-req/hr quota), but we still gate through the local
    /// bucket so a runaway loop can't hammer this endpoint either.
    pub async fn fetch_notifications(&self) -> Result<NotificationsPoll, GhError> {
        // Bookkeeping wrapper: on success, clear any prior back-off
        // window so the next tick uses the heartbeat again; on failure,
        // arm the back-off so chronic auth/rate-limit problems don't
        // pay the failed REST round-trip on every tick. We could push
        // this into the polling layer, but the heartbeat invariant
        // ("after one call, the back-off state is consistent") belongs
        // with the call itself — callers don't have to remember to
        // bookkeep.
        let result = self.fetch_notifications_inner().await;
        match &result {
            Ok(_) => self.note_heartbeat_succeeded(),
            Err(_) => self.note_heartbeat_failed(),
        }
        result
    }

    /// Inner implementation of [`Self::fetch_notifications`]. Pure I/O;
    /// the wrapper handles back-off bookkeeping so this method can use
    /// `?` freely without forgetting to record the failure.
    async fn fetch_notifications_inner(&self) -> Result<NotificationsPoll, GhError> {
        use http::StatusCode;
        use http::header::{HeaderMap, HeaderValue, IF_MODIFIED_SINCE, LAST_MODIFIED};

        self.acquire_or_block("notifications heartbeat")?;

        // Capture the saved header BEFORE the request, so the lock is
        // released before we await the network call. The Mutex is a
        // parking_lot::Mutex (not tokio's), so holding it across `.await`
        // would risk a hang if any other code path tried to lock during
        // the request. Clone-and-drop is the right pattern.
        let if_modified_since = self.notifications_state.lock().last_modified.clone();

        let mut headers = HeaderMap::new();
        if let Some(ims) = if_modified_since.as_ref() {
            match HeaderValue::from_str(ims) {
                Ok(v) => {
                    headers.insert(IF_MODIFIED_SINCE, v);
                }
                Err(e) => {
                    // Stored value isn't header-valid (CR/LF, non-ASCII).
                    // Skipping the conditional forces a 200, costing a
                    // body parse — surface loudly so the regression is
                    // observable instead of just slower.
                    tracing::warn!(
                        "dropping invalid stored If-Modified-Since `{ims}`: {e} — \
                         next notifications call will fetch unconditional 200"
                    );
                }
            }
        }
        // `participating=false` — match #19's recommendation. Returns
        // every notification the user can see, not just ones they're
        // explicitly mentioned in. That's what we want: lazybox's job is
        // to surface activity on rows already in the inbox.
        // `all=false` (default) — only unread items. Read notifications
        // wouldn't add information since we already saw them.
        let uri = "/notifications?participating=false";
        let response = self
            .inner
            ._get_with_headers(uri, Some(headers))
            .await
            .map_err(GhError::Api)?;

        let status = response.status();
        // 304 = nothing new since If-Modified-Since. The endpoint also
        // sends an empty body in that case; don't try to deserialize.
        if status == StatusCode::NOT_MODIFIED {
            tracing::debug!("notifications: 304 Not Modified");
            return Ok(NotificationsPoll::NotModified);
        }

        // Capture `Last-Modified` before consuming the body — once we
        // hand the response to `body_to_string`, the headers go with it.
        let new_last_modified = response
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if !status.is_success() {
            // 401/403/5xx — surface as a typed GhError so the normal
            // ProviderError classification (auth vs retryable) kicks in.
            // We synthesize a GhError::Graphql with the status because
            // the REST body shape isn't a GqlResponse; the caller logs
            // it and the next tick retries naturally.
            //
            // Bound the snippet to ~512 bytes: GitHub's 502/503
            // maintenance pages are multi-MB HTML, and this string
            // ends up duplicated through `GhError::Graphql` →
            // `ProviderError::Retryable` → `Event::ProviderError` on
            // the broadcast bus. A bad upstream hour would otherwise
            // churn tens of MB of identical body strings through the
            // event channel.
            let body = self
                .inner
                .body_to_string(response)
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            let snippet = body_prefix_bytes(&body, 512);
            return Err(GhError::Graphql(format!(
                "notifications HTTP {}: {snippet}",
                status.as_u16(),
            )));
        }

        let body = self
            .inner
            .body_to_string(response)
            .await
            .map_err(GhError::Api)?;
        let entries: Vec<NotificationEntry> = serde_json::from_str(&body).map_err(|e| {
            GhError::Graphql(format!(
                "notifications response did not match schema: {e}; body prefix: {}",
                body.chars().take(200).collect::<String>()
            ))
        })?;

        // Do NOT commit the new `Last-Modified` here (#512). Advancing
        // the cursor the instant the LIST parses — before the per-entry
        // deep-fetches run — is exactly the bug: if a deep-fetch times
        // out this tick, the cursor has already moved past its entry, so
        // the next heartbeat answers 304 and the entry is never re-listed
        // until its `updated_at` bumps again (a CI failure / new comment
        // can stay invisible until the ≤10-min full sweep). Instead we
        // hand the pending cursor back to the polling layer, which
        // commits it via `commit_notifications_cursor` only after the
        // fan-out reports every entry handled. A failed entry holds the
        // cursor so it re-lists next tick.
        Ok(NotificationsPoll::Modified {
            entries,
            last_modified: new_last_modified,
        })
    }

    /// Commit the notifications cursor (`Last-Modified`) captured from a
    /// [`NotificationsPoll::Modified`] poll, echoed back as
    /// `If-Modified-Since` on the next `GET /notifications` so the steady
    /// state answers 304 cheaply.
    ///
    /// Called by the polling layer AFTER the per-entry deep-fetch fan-out
    /// finishes with no transient failures — coupling the at-most-once
    /// cursor advance to work completion (#512). Skips the write when
    /// GitHub didn't send a `Last-Modified` (rare; some test fixtures):
    /// leaving the old value alone is safer than clearing it and
    /// re-pulling the world.
    pub fn commit_notifications_cursor(&self, last_modified: Option<String>) {
        if let Some(lm) = last_modified {
            self.notifications_state.lock().last_modified = Some(lm);
        }
    }

    /// Fetch the bounded hot set in one GraphQL request. `nodes(ids:)`
    /// preserves input order, so the returned vector has one slot per
    /// requested node; `None` means that node is no longer visible.
    pub async fn fetch_hot_tasks(&self, node_ids: &[String]) -> Result<Vec<Option<Task>>, GhError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let started = std::time::Instant::now();
        let mut metrics = BranchMetrics::new("hot-targets");
        self.acquire_or_block("hot-target batch query")?;
        let body = graphql::hot_tasks_body(node_ids);
        let (response, bytes): (graphql::GqlHotTasksResponse, usize) =
            self.post_graphql_with_retry_measured(&body).await?;
        metrics.requests = 1;
        metrics.resp_bytes = bytes;

        let errors = response.errors.unwrap_or_default();
        if errors.iter().any(|error| !error.is_not_visible()) {
            let joined = errors
                .iter()
                .map(|error| error.full())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::Graphql(joined));
        }

        let Some(data) = response.data else {
            if errors.is_empty() {
                return Err(GhError::Graphql("hot-target batch returned no data".into()));
            }
            return Ok(vec![None; node_ids.len()]);
        };
        if data.nodes.len() != node_ids.len() {
            return Err(GhError::Graphql(format!(
                "hot-target batch returned {} node slots for {} ids",
                data.nodes.len(),
                node_ids.len()
            )));
        }
        if let Some(rate_limit) = &data.rate_limit {
            metrics.graphql_cost = rate_limit.cost.unwrap_or(0);
            self.observe_rate_limit(rate_limit);
        }

        let tasks = data
            .nodes
            .into_iter()
            .map(|node| {
                node.map(|node| match node {
                    graphql::GqlHotTask::PullRequest(pr) => graphql::pr_to_task(&pr, &self.user),
                    graphql::GqlHotTask::Issue(issue) => graphql::issue_to_task(&issue, &self.user),
                })
            })
            .collect::<Vec<_>>();
        metrics.prs = tasks.iter().flatten().filter(|task| task.is_pr()).count();
        metrics.elapsed_ms = started.elapsed().as_millis();
        metrics.emit();
        Ok(tasks)
    }

    /// Targeted deep-fetch: pull one PR by `(owner, repo, number)` via
    /// the single-node GraphQL query. ~85 cost units total (vs. the
    /// 1000s the inbox-scan query burns when re-walking every PR).
    /// Returns `Ok(None)` when GitHub can't find / no longer exposes
    /// the PR (deleted, scope changed, transferred) — whether that
    /// surfaces as a null `pullRequest` node in an accessible repo or as
    /// a top-level `NOT_FOUND`/`FORBIDDEN` GraphQL error on the repo
    /// itself. The caller treats `Ok(None)` as "skip this entry"; only a
    /// genuinely transient `Err` holds the notifications cursor (#512).
    pub async fn fetch_single_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<Task>, GhError> {
        Ok(self
            .fetch_single_pr_with_head(owner, repo, number)
            .await?
            .map(|(task, _)| task))
    }

    /// [`fetch_single_pr`](Self::fetch_single_pr) that also surfaces the
    /// PR's **head commit OID** from the same response. `Task` doesn't
    /// carry the OID, but the daemon's auto-merge path needs the head it
    /// verified green so it can pin `mergePullRequest`'s
    /// `expectedHeadOid` to exactly that commit — coming from one fetch
    /// keeps "observed green" and "merge this" atomic.
    pub async fn fetch_single_pr_with_head(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<(Task, Option<String>)>, GhError> {
        self.acquire_or_block("single-PR notification deep-fetch")?;
        let body = graphql::single_pr_body(owner, repo, number);
        let response: graphql::GqlSinglePrResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            // A definitive "not visible" answer (NOT_FOUND / FORBIDDEN:
            // deleted, transferred, private, scope revoked) is NOT a
            // transient failure. GitHub returns it as a top-level GraphQL
            // error alongside `data.repository = null`, so it lands here
            // rather than the `Ok(None)` null-node path below. Map it to
            // `Ok(None)` so the notifications-cursor caller treats the
            // entry as handled and can advance — otherwise a
            // permanently-gone entry would pin the heartbeat off its
            // cheap 304 forever (#512). Any other error is transient →
            // `Err` holds the cursor so the entry re-lists next tick.
            if gql_errors_all_not_visible(&errors) {
                tracing::debug!("fetch_single_pr {owner}/{repo}#{number}: not visible — skipping");
                return Ok(None);
            }
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::warn!("fetch_single_pr {owner}/{repo}#{number}: {joined}");
            return Err(GhError::Graphql(joined));
        }
        let Some(data) = response.data else {
            return Ok(None);
        };
        if let Some(rl) = &data.rate_limit {
            self.observe_rate_limit(rl);
        }
        let pr = data.repository.and_then(|r| r.pull_request);
        Ok(pr.map(|pr| {
            let head = pr.head_ref_oid.clone();
            (graphql::pr_to_task(&pr, &self.user), head)
        }))
    }

    /// Sibling of `fetch_single_pr` for Issue-typed notifications.
    /// Same Ok(None) "not visible" semantics.
    pub async fn fetch_single_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<Task>, GhError> {
        self.acquire_or_block("single-issue notification deep-fetch")?;
        let body = graphql::single_issue_body(owner, repo, number);
        let response: graphql::GqlSingleIssueResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            // See `fetch_single_pr`: a definitive NOT_FOUND / FORBIDDEN
            // is "not visible" (`Ok(None)`), never a cursor-holding
            // transient failure (#512).
            if gql_errors_all_not_visible(&errors) {
                tracing::debug!(
                    "fetch_single_issue {owner}/{repo}#{number}: not visible — skipping"
                );
                return Ok(None);
            }
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::warn!("fetch_single_issue {owner}/{repo}#{number}: {joined}");
            return Err(GhError::Graphql(joined));
        }
        let Some(data) = response.data else {
            return Ok(None);
        };
        if let Some(rl) = &data.rate_limit {
            self.observe_rate_limit(rl);
        }
        let issue = data.repository.and_then(|r| r.issue);
        Ok(issue.map(|i| graphql::issue_to_task(&i, &self.user)))
    }

    /// `since` narrows the main `involves:` paginated search to PRs
    /// updated at or after that instant (issue #14) — `None` fetches
    /// every open involved PR (a reconcile sweep). When `since` is set,
    /// the merged-sweep is also windowed — on its OWN success floor, not
    /// `since` (issue #530) — so a steady sweep stops re-downloading the
    /// whole 7-day merged set. The reviewer/watched branches stay
    /// unwindowed — cheap single-page queries where a window buys
    /// nothing.
    pub async fn fetch_all_prs(
        &self,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<Task>, GhError> {
        // Per-call wall-clock timer so the log can quantify the
        // parallelization win and so a regression jumps out in
        // `grep "fetch_all_prs: completed" /tmp/lazybox.log`. Cheap;
        // Instant::now() is ~10ns on macOS.
        let started = std::time::Instant::now();
        // Parallelize the three independent branches of a PR fetch:
        //   1. Main paginated PR search (involves the user).
        //   2. Recently-merged sweep (`is:merged` last 7d).
        //   3. Watched-repo fan-out (one query per subscribed repo).
        // Pre-fix these ran sequentially — a user with 10 watched
        // repos saw ~30s polls dominated by sequential repo
        // queries. Now: main + merged + watched concurrent, watched
        // bounded to 5 in flight at once so the rate budget
        // (capacity 30, refill 30/min) doesn't blow up.
        use futures::stream::{self, StreamExt};

        // Build the qualifiers once so both the main search and the
        // merged-sweep share role+scope narrowing.
        let mut quals = graphql::default_search_qualifiers();
        if self.pr_filters.is_empty() {
            quals.push(format!("involves:{}", self.user));
        } else {
            quals.extend(self.pr_filters.iter().cloned());
        }
        // Incremental window (issue #14): on a steady-state sweep this
        // collapses the dominant `involves-main` branch to a near-empty
        // first page. `None` (reconcile sweep) leaves the search wide.
        if let Some(since) = since {
            quals.push(graphql::updated_since_qualifier(since));
        }
        let search_query = graphql::build_query(&quals);
        tracing::info!("GraphQL search: {search_query}");

        // Branch 1: main paginated search.
        let main_fut = self.fetch_pr_search_paginated(&search_query);

        // Branch 1b: `review-requested:USER` companion search.
        // GitHub's `involves:` qualifier covers author + assignee +
        // mentioned + commenter — but NOT requested reviewer. For a
        // user whose only involvement in a repo is "requested to
        // review", `involves:USER` returns 0 results. The single-
        // scope path used to compensate via the explicit `repo:`
        // qualifier (which broadens the search), but multi-scope
        // can't use that without hitting the parens-OR footgun.
        //
        // Fire a parallel `review-requested:USER` search whenever
        // the user has the reviewer role enabled OR has no specific
        // PR role enabled. Results union into the main set with
        // dedup by task id. Cost: one extra GraphQL search per poll.
        let want_reviewer_pass = self.pr_filters.is_empty()
            || self
                .pr_filters
                .iter()
                .any(|q| q.contains("review-requested") || q.contains("involves:"));
        let reviewer_query = if want_reviewer_pass {
            let mut q = graphql::default_search_qualifiers();
            q.push(format!("review-requested:{}", self.user));
            Some(graphql::build_query(&q))
        } else {
            None
        };
        let reviewer_fut = async {
            if let Some(q) = reviewer_query {
                self.fetch_pr_single_query("review-requested", q).await
            } else {
                Ok(Vec::new())
            }
        };

        // Branch 2: recently-merged sweep. Windowed on its OWN success
        // floor (issue #530) so a steady sweep skips re-downloading the
        // whole 7-day merged set — but NOT the main branch's floor,
        // which advances even when this best-effort branch fails.
        // `since.and(..)` keeps it unwindowed on a reconcile (`since`
        // None) so an unwindowed pass reconciles any merge a prior
        // windowed pass missed, and on cold start (no floor yet).
        let merged_started = chrono::Utc::now();
        let merged_since = since.and(self.merged_sweep_window());
        let week_ago = (chrono::Utc::now() - chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string();
        let merged_query =
            graphql::merged_sweep_query(&self.user, &self.pr_filters, &week_ago, merged_since);
        tracing::debug!("Recently-merged sweep: {merged_query}");
        let merged_fut = self.fetch_pr_single_query("merged-sweep", merged_query);

        // Branch 3: bounded-concurrent watched-repo fan-out. 5 in
        // flight is a healthy compromise — small enough that the
        // local rate budget (capacity 30) doesn't get fully drained
        // by a single poll even when paired with the other two
        // branches above; large enough that 10 watched repos
        // complete in two batches instead of ten sequential calls.
        const WATCHED_CONCURRENCY: usize = 5;
        let watched_fut = stream::iter(self.watch_repos.iter().cloned())
            .map(|repo| async move {
                let query = watched_repo_query(&repo, &self.user);
                let result = self.fetch_pr_single_query("watched-repo", query).await;
                (repo, result)
            })
            .buffer_unordered(WATCHED_CONCURRENCY)
            .collect::<Vec<_>>();

        let (main_res, reviewer_res, merged_res, watched_results) =
            tokio::join!(main_fut, reviewer_fut, merged_fut, watched_fut);

        // The main search is load-bearing — if it fails the whole
        // poll fails so the polling layer's error path fires. The
        // sweep + watched + reviewer paths are best-effort: log +
        // continue.
        let mut tasks = main_res?;
        // Per-branch fetched counts (pre-dedup) drive the union-level
        // dedup hit-rate emitted below — how much of each poll was
        // PRs already returned by an earlier branch.
        let main_fetched = tasks.len();
        let mut reviewer_fetched = 0usize;
        let mut merged_fetched = 0usize;
        let mut watched_fetched = 0usize;
        let mut existing: std::collections::HashSet<String> =
            tasks.iter().map(|t| t.id.key.clone()).collect();

        match reviewer_res {
            Ok(rev_tasks) => {
                reviewer_fetched = rev_tasks.len();
                let mut added = 0usize;
                for t in rev_tasks {
                    if existing.insert(t.id.key.clone()) {
                        tasks.push(t);
                        added += 1;
                    }
                }
                if added > 0 {
                    tracing::info!(
                        "review-requested branch: {added} PRs added (GH `involves:` misses these)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!("review-requested branch failed: {e}");
            }
        }

        match merged_res {
            Ok(merged_tasks) => {
                // Advance the merged floor only on success, so a later
                // windowed merged sweep can trust it covers every merge
                // up to `merged_started` (issue #530).
                self.record_merged_sweep_window(merged_started);
                merged_fetched = merged_tasks.len();
                let mut added = 0usize;
                for t in merged_tasks {
                    if existing.insert(t.id.key.clone()) {
                        tasks.push(t);
                        added += 1;
                    }
                }
                if added > 0 {
                    tracing::info!(
                        "recently-merged sweep: {added} PRs back-filled with final MERGED state"
                    );
                }
            }
            Err(e) => {
                tracing::warn!("recently-merged sweep failed: {e}");
            }
        }

        let mut watch_failures: usize = 0;
        for (repo, result) in watched_results {
            match result {
                Ok(repo_tasks) => {
                    watched_fetched += repo_tasks.len();
                    for t in repo_tasks {
                        if existing.insert(t.id.key.clone()) {
                            tasks.push(t);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Watch query failed for {repo}: {e}");
                    watch_failures += 1;
                }
            }
        }

        let elapsed_ms = started.elapsed().as_millis();
        tracing::info!(
            "fetch_all_prs: completed in {elapsed_ms}ms — {} PRs (incl. {} watched repos + merged-sweep)",
            tasks.len(),
            self.watch_repos.len()
        );
        // Union-level breakdown: how many PRs each branch fetched vs
        // how many survived dedup. `duplicates` is the redundant
        // download every poll pays for overlapping search branches.
        let total_fetched = main_fetched + reviewer_fetched + merged_fetched + watched_fetched;
        let unique = tasks.len();
        let duplicates = total_fetched.saturating_sub(unique);
        let dedup_pct = if total_fetched > 0 {
            duplicates as f64 / total_fetched as f64 * 100.0
        } else {
            0.0
        };
        tracing::debug!(
            target: "gh_sync_metrics",
            elapsed_ms = elapsed_ms as u64,
            total_fetched,
            unique,
            duplicates,
            dedup_pct = format!("{dedup_pct:.0}"),
            main = main_fetched,
            reviewer = reviewer_fetched,
            merged = merged_fetched,
            watched = watched_fetched,
            watched_repos = self.watch_repos.len(),
            "fetch_all_prs union breakdown",
        );
        if !self.watch_repos.is_empty() && watch_failures == self.watch_repos.len() {
            return Err(GhError::WatchAllFailed {
                count: self.watch_repos.len(),
            });
        }
        Ok(tasks)
    }

    /// Round-robin variant of `fetch_all_prs`. Instead of one big
    /// `involves:USER` paginated sweep across every repo the user
    /// touches, this fires a small batch of `repo:owner/name`-scoped
    /// PR searches in parallel — driven by the polling layer's
    /// `pick_repos_for_tick` scheduler.
    ///
    /// Always runs the cheap, role-narrowing companions alongside:
    /// - `review-requested:USER` (GitHub's `involves:` qualifier
    ///   misses requested reviewers, same as `fetch_all_prs`).
    /// - The 7-day `is:merged` sweep so a PR landing right after a
    ///   sync still gets the final MERGED state on the next tick.
    ///
    /// The full watched-repo fan-out from `fetch_all_prs` is *not*
    /// duplicated here — that path already has its own pacing,
    /// and the round-robin's whole point is to keep per-tick cost
    /// low.
    ///
    /// Returns `Ok([])` when `repos` is empty (`pick_repos_for_tick`
    /// returns an empty list during cold start). Caller is
    /// responsible for also running `fetch_all_prs` on global-sweep
    /// ticks (`RoundRobinPick::run_global`).
    async fn fetch_prs_for_repos(&self, repos: &[String]) -> Result<PrFetchOutcome, GhError> {
        let started = std::time::Instant::now();
        use futures::stream::{self, StreamExt};

        // Same bounded-concurrent fan-out shape as the watched-repo
        // branch in `fetch_all_prs`. 5 in flight keeps the local
        // rate budget breathable when 3 repos + reviewer + merged
        // all dispatch on the same tick.
        const PER_REPO_CONCURRENCY: usize = 5;
        let per_repo_fut = stream::iter(repos.iter().cloned())
            .map(|repo| async move {
                let query = format!("is:open is:pr repo:{repo} archived:false");
                let result = self.fetch_pr_single_query("round-robin-repo", query).await;
                (repo, result)
            })
            .buffer_unordered(PER_REPO_CONCURRENCY)
            .collect::<Vec<_>>();

        // Reviewer companion — same logic as `fetch_all_prs`. Without
        // it, repos where the user is ONLY a requested reviewer would
        // never enter the inbox via the round-robin path either.
        let want_reviewer_pass = self.pr_filters.is_empty()
            || self
                .pr_filters
                .iter()
                .any(|q| q.contains("review-requested") || q.contains("involves:"));
        let reviewer_query = if want_reviewer_pass {
            let mut q = graphql::default_search_qualifiers();
            q.push(format!("review-requested:{}", self.user));
            Some(graphql::build_query(&q))
        } else {
            None
        };
        let reviewer_fut = async {
            if let Some(q) = reviewer_query {
                self.fetch_pr_single_query("review-requested", q).await
            } else {
                Ok(Vec::new())
            }
        };

        // Merged sweep — global, cheap, identical to `fetch_all_prs`.
        // Skipping this would mean PRs that merged between our last
        // sync of their repo and now stay stuck on `OPEN`. The
        // round-robin path never advances a sweep window, so it stays
        // unwindowed (`None`) — same as its per-repo branch.
        let week_ago = (chrono::Utc::now() - chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string();
        let merged_query =
            graphql::merged_sweep_query(&self.user, &self.pr_filters, &week_ago, None);
        let merged_fut = self.fetch_pr_single_query("merged-sweep", merged_query);

        let (per_repo_results, reviewer_res, merged_res) =
            tokio::join!(per_repo_fut, reviewer_fut, merged_fut);

        // Same dedup-by-task-id assembly as `fetch_all_prs`. Per-repo
        // results win first (they're the freshest, scoped exactly to
        // the repos we asked about); reviewer + merged back-fill the
        // gaps. A repo-scoped query failure does NOT fail the whole
        // tick — the next round-robin slot picks it up.
        let mut tasks: Vec<Task> = Vec::new();
        let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut repo_failures: Vec<(String, GhError)> = Vec::new();
        for (repo, result) in per_repo_results {
            match result {
                Ok(repo_tasks) => {
                    for t in repo_tasks {
                        if existing.insert(t.id.key.clone()) {
                            tasks.push(t);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("round-robin repo query failed for {repo}: {e}");
                    repo_failures.push((repo, e));
                }
            }
        }
        match reviewer_res {
            Ok(rev_tasks) => {
                for t in rev_tasks {
                    if existing.insert(t.id.key.clone()) {
                        tasks.push(t);
                    }
                }
            }
            Err(e) => tracing::warn!("round-robin review-requested branch failed: {e}"),
        }
        match merged_res {
            Ok(merged_tasks) => {
                for t in merged_tasks {
                    if existing.insert(t.id.key.clone()) {
                        tasks.push(t);
                    }
                }
            }
            Err(e) => tracing::warn!("round-robin merged-sweep failed: {e}"),
        }

        let elapsed_ms = started.elapsed().as_millis();
        tracing::info!(
            "fetch_prs_for_repos: completed in {elapsed_ms}ms — {} PRs across {} repos \
             ({repo_failures} repo-query failure(s))",
            tasks.len(),
            repos.len(),
            repo_failures = repo_failures.len(),
        );
        // Mirror `fetch_all_prs`'s "everything failed" defensive
        // check: if EVERY repo we asked about failed, surface the
        // error so the tick doesn't silently wipe focus repo's PRs
        // from the inbox on the next rescope.
        if !repos.is_empty() && repo_failures.len() == repos.len() {
            let details = repo_failures
                .into_iter()
                .map(|(repo, error)| format!("{repo}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::Graphql(format!(
                "all {} round-robin repo queries failed: {details}",
                repos.len(),
            )));
        }
        if repo_failures.is_empty() {
            Ok(PrFetchOutcome::complete(tasks))
        } else {
            let failure_count = repo_failures.len();
            let failed = repo_failures
                .into_iter()
                .map(|(repo, error)| format!("{repo}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            Ok(PrFetchOutcome::partial(
                tasks,
                format!(
                    "PR repo sync incomplete: {} of {} repo queries failed ({failed})",
                    failure_count,
                    repos.len(),
                ),
            ))
        }
    }

    /// Run the main paginated PR search (cursor pages run
    /// sequentially because each page's `endCursor` is the next
    /// page's input). Extracted so the parallel-branches outer
    /// fetch can `tokio::join!` it alongside the merged-sweep + the
    /// watched-repo fan-out.
    async fn fetch_pr_search_paginated(&self, search_query: &str) -> Result<Vec<Task>, GhError> {
        let started = std::time::Instant::now();
        let mut metrics = BranchMetrics::new("involves-main");
        let mut tasks: Vec<Task> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            self.acquire_or_block("PR search")?;
            let body = graphql::query_body_after(search_query, cursor.as_deref());
            tracing::debug!(
                "GraphQL page {page} body: {}",
                serde_json::to_string(&body).unwrap_or_default()
            );
            let (raw, page_bytes): (serde_json::Value, usize) = self
                .post_graphql_with_retry_measured(&body)
                .await
                .map_err(|e| {
                    tracing::error!("GraphQL HTTP error (page {page}): {e}\n{e:?}");
                    tracing::error!(
                        "GraphQL request body was: {}",
                        serde_json::to_string_pretty(&body).unwrap_or_default()
                    );
                    e
                })?;
            metrics.requests += 1;
            metrics.resp_bytes += page_bytes;
            let response: graphql::GqlResponse = match serde_json::from_value(raw.clone()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        "GraphQL response did not match schema (page {page}): {e}\n\
                         Full response body:\n{}",
                        serde_json::to_string_pretty(&raw).unwrap_or_default()
                    );
                    return Err(GhError::Graphql(format!(
                        "response schema mismatch (page {page}): {e}"
                    )));
                }
            };
            if let Some(errors) = &response.errors {
                let joined: String = errors
                    .iter()
                    .map(|e| e.full())
                    .collect::<Vec<_>>()
                    .join("; ");
                tracing::error!("GraphQL errors (search page {page}): {joined}");
                return Err(GhError::Graphql(format!(
                    "search `{search_query}` (page {page}): {joined}"
                )));
            }
            let data = response
                .data
                .ok_or_else(|| GhError::Graphql("No data in response".into()))?;
            if let Some(rl) = &data.rate_limit {
                tracing::info!(
                    "GitHub rate limit: {}/{} remaining, resets {}",
                    rl.remaining,
                    rl.limit,
                    rl.reset_at
                );
                metrics.graphql_cost += rl.cost.unwrap_or(0);
                self.observe_rate_limit(rl);
            }
            tasks.extend(
                data.search
                    .nodes
                    .iter()
                    .map(|pr| graphql::pr_to_task(pr, &self.user)),
            );
            let Some(next_cursor) = next_page_cursor(data.search.page_info, "PR search")? else {
                break;
            };
            cursor = Some(next_cursor);
            page += 1;
            if page >= 20 {
                tracing::error!(
                    "GraphQL paged: bailing after {page} pages (safety cap; tail truncated)"
                );
                return Err(GhError::Truncated {
                    count: tasks.len(),
                    pages: page,
                });
            }
        }
        metrics.prs = tasks.len();
        metrics.elapsed_ms = started.elapsed().as_millis();
        metrics.emit();
        Ok(tasks)
    }

    /// Companion PR search used by the merged-sweep, the watched-repo
    /// fan-out, the review-requested pass, and the round-robin per-repo
    /// queries. Usually one page, but FOLLOWS the cursor when GitHub
    /// reports more: these branches feed `polled_scope` as authoritative
    /// coverage, so a silently-truncated first page (`PR_PAGE_SIZE = 25`)
    /// made rescope delete every workspace past the cap. Errors on
    /// rate-budget exhaustion mid-walk rather than returning a partial
    /// set — a failed branch is preserved-conservatively by the caller,
    /// a truncated "success" is not.
    async fn fetch_pr_single_query(
        &self,
        op: &'static str,
        query: String,
    ) -> Result<Vec<Task>, GhError> {
        let started = std::time::Instant::now();
        let mut metrics = BranchMetrics::new(op);
        let mut tasks: Vec<Task> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            if let Err(reason) = self.try_acquire() {
                return Err(GhError::RateLimited {
                    retry_after_secs: 1,
                    reason: format!("{op} blocked: {reason}"),
                });
            }
            let body = graphql::query_body_after(&query, cursor.as_deref());
            let (resp, bytes): (graphql::GqlResponse, usize) =
                self.post_graphql_with_retry_measured(&body).await?;
            metrics.requests += 1;
            metrics.resp_bytes += bytes;
            if let Some(errors) = resp.errors {
                let joined: String = errors
                    .iter()
                    .map(|e| e.full())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(GhError::Graphql(format!("{op}: {joined}")));
            }
            let Some(data) = resp.data else {
                break;
            };
            if let Some(rl) = &data.rate_limit {
                metrics.graphql_cost += rl.cost.unwrap_or(0);
                self.observe_rate_limit(rl);
            }
            tasks.extend(
                data.search
                    .nodes
                    .iter()
                    .map(|pr| graphql::pr_to_task(pr, &self.user)),
            );
            let Some(next_cursor) = next_page_cursor(data.search.page_info, op)? else {
                break;
            };
            cursor = Some(next_cursor);
            page += 1;
            if page >= 20 {
                // Same safety-cap visibility as the main paginated
                // search: error (don't silently truncate) so the
                // caller treats this branch's coverage as failed.
                tracing::error!(
                    "{op} paged: bailing after {page} pages (safety cap; tail truncated)"
                );
                return Err(GhError::Truncated {
                    count: tasks.len(),
                    pages: page,
                });
            }
        }
        metrics.prs = tasks.len();
        metrics.elapsed_ms = started.elapsed().as_millis();
        metrics.emit();
        Ok(tasks)
    }

    /// Fetch all open GitHub Issues involving the authenticated user,
    /// paginated. Separate from `fetch_all_prs` so callers opt in
    /// explicitly. Thin wrapper over `fetch_all_issues_with_mentions`
    /// that discards the mention side-channel — use the underlying
    /// method when you want the `@lazybox` triggers too.
    pub async fn fetch_all_issues(&self) -> Result<Vec<Task>, GhError> {
        let (tasks, _mentions) = self
            .fetch_all_issues_with_mentions(&std::collections::BTreeSet::new())
            .await?;
        Ok(tasks)
    }

    /// Same as `fetch_all_issues` but also scans each raw issue for
    /// `@lazybox` mentions from `allowed_logins` and returns the
    /// resulting [`crate::LazyboxMention`] list. Done in one pass so we
    /// don't pay the issue search twice; the GraphQL response already
    /// carries `reactions(content: EYES) { viewerHasReacted }` for
    /// idempotency.
    ///
    /// An empty `allowed_logins` set yields no mentions (the gate is
    /// "allow nobody by default"), so production callers always pass
    /// at least the authenticated viewer's login.
    pub async fn fetch_all_issues_with_mentions(
        &self,
        allowed_logins: &std::collections::BTreeSet<String>,
    ) -> Result<(Vec<Task>, Vec<crate::LazyboxMention>), GhError> {
        let started = std::time::Instant::now();
        // Same assembly as `fetch_all_prs` — see notes there.
        let mut quals = graphql::default_issues_qualifiers();
        if self.issue_filters.is_empty() {
            quals.push(format!("involves:{}", self.user));
        } else {
            quals.extend(self.issue_filters.iter().cloned());
        }
        let search_query = graphql::build_issues_query(&quals);
        tracing::info!("GraphQL issues search: {search_query}");

        let mut tasks: Vec<Task> = Vec::new();
        let mut mentions: Vec<crate::LazyboxMention> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            // Same rate-budget guard as PR fetch — see fetch_all_prs.
            self.acquire_or_block("issues search")?;
            let body = graphql::issues_query_body(&search_query, cursor.as_deref());
            let response: graphql::GqlIssueResponse =
                self.post_graphql_with_retry(&body).await.map_err(|e| {
                    tracing::error!("Issues HTTP error (page {page}): {e}\n{e:?}");
                    e
                })?;

            if let Some(errors) = &response.errors {
                let joined = errors
                    .iter()
                    .map(|e| e.full())
                    .collect::<Vec<_>>()
                    .join("; ");
                tracing::error!("Issues GraphQL errors (page {page}): {joined}");
                return Err(GhError::Graphql(joined));
            }

            let data = response
                .data
                .ok_or_else(|| GhError::Graphql("No data in response".into()))?;

            if let Some(rl) = &data.rate_limit {
                tracing::debug!(
                    "GitHub rate limit after issues: {}/{} remaining",
                    rl.remaining,
                    rl.limit
                );
                self.observe_rate_limit(rl);
            }

            for issue in &data.search.nodes {
                tasks.push(graphql::issue_to_task(issue, &self.user));
                if !allowed_logins.is_empty() {
                    mentions.extend(crate::mentions::scan_issue(issue, allowed_logins));
                }
            }

            let Some(next_cursor) = next_page_cursor(data.search.page_info, "issues search")?
            else {
                break;
            };
            cursor = Some(next_cursor);
            page += 1;
            if page >= 20 {
                // Same safety-cap visibility as fetch_all_prs.
                tracing::error!(
                    "Issues paged: bailing after {page} pages (safety cap; tail truncated)"
                );
                return Err(GhError::Truncated {
                    count: tasks.len(),
                    pages: page,
                });
            }
        }

        let elapsed_ms = started.elapsed().as_millis();
        tracing::info!(
            "fetch_all_issues_with_mentions: completed in {elapsed_ms}ms — {} issues, {} mentions",
            tasks.len(),
            mentions.len(),
        );
        Ok((tasks, mentions))
    }

    /// Fetch PRs + Issues in parallel, combine into one `Vec<Task>`.
    ///
    /// "Empty" and "failed" are distinct outcomes: a successful fetch
    /// returning zero rows is a normal state for a brand-new account
    /// with no matching items. Only when **both** sides actually
    /// errored do we surface a failure — and we keep both errors so
    /// the TUI / logs can show them together. A single side erroring
    /// degrades gracefully: the other side's results land in the inbox.
    pub async fn fetch_all(&self) -> Result<Vec<Task>, GhError> {
        // Drive each query only when the caller actually wants
        // results. The polling layer signals intent by setting (or
        // not setting) `pr_filters` / `issue_filters` via
        // `with_filters`. An empty filter list means "no preferences
        // wired" — at construction time we treat it as
        // `involves:USER` for backward compat and run the query.
        // The polling layer always wires explicit filters from the
        // user's persisted setup, so:
        //
        //   - PR-only setup → `issue_filters` is empty + we skip the
        //     issues query.
        //   - Issues-only setup → opposite.
        //
        // This halves the GraphQL search rate-limit cost for the
        // common single-type case.
        let want_prs = !self.pr_filters.is_empty() || self.issue_filters.is_empty();
        let want_issues = !self.issue_filters.is_empty();
        self.fetch_selected(want_prs, want_issues).await
    }

    /// Underlying parallel-fetch driven by explicit booleans. Public
    /// so the polling layer can pass the actual `pr_enabled()` /
    /// `issue_enabled()` flags from the user's `ProviderConfig` and
    /// avoid the legacy "infer from filters" logic above.
    pub async fn fetch_selected(
        &self,
        want_prs: bool,
        want_issues: bool,
    ) -> Result<Vec<Task>, GhError> {
        let started = std::time::Instant::now();
        if !want_prs && !want_issues {
            return Ok(Vec::new());
        }
        let pr_fut = async {
            if want_prs {
                self.fetch_all_prs(None).await.map(PrFetchOutcome::complete)
            } else {
                Ok(PrFetchOutcome::complete(Vec::new()))
            }
        };
        let issue_fut = async {
            if want_issues {
                self.fetch_all_issues().await
            } else {
                Ok(Vec::new())
            }
        };
        let (prs, issues) = tokio::join!(pr_fut, issue_fut);
        // Log the outer wall-clock so the parallelization win shows
        // up directly: this is the value the poll loop pays per
        // cycle, equal to max(PR-branches, Issues-branches).
        let elapsed_ms = started.elapsed().as_millis();
        tracing::info!(
            "fetch_selected: completed in {elapsed_ms}ms (PRs={want_prs}, Issues={want_issues})"
        );
        combine_selected_fetches(
            want_prs,
            want_issues,
            prs,
            issues.map(|tasks| (tasks, Vec::new())),
        )
        .map(|outcome| outcome.tasks)
    }

    /// Variant of `fetch_selected` that surfaces partial failures
    /// to the caller as a structured side-channel instead of just a
    /// `tracing::warn`. Returns `(tasks, partial_failure)` — the
    /// second slot is `Some` when one side errored but the other
    /// completed successfully AND we returned `Ok` to keep the inbox
    /// alive (the silent-partial behaviour the polling layer wants
    /// to surface to the user).
    ///
    /// Callers can fire a `ProviderError` bus event so the footer
    /// shows `partial sync: issues failed` instead of the user
    /// silently losing half their inbox.
    pub async fn fetch_selected_with_status(
        &self,
        want_prs: bool,
        want_issues: bool,
    ) -> Result<(Vec<Task>, Option<String>), GhError> {
        if !want_prs && !want_issues {
            return Ok((Vec::new(), None));
        }
        let pr_fut = async {
            if want_prs {
                self.fetch_all_prs(None).await.map(PrFetchOutcome::complete)
            } else {
                Ok(PrFetchOutcome::complete(Vec::new()))
            }
        };
        let issue_fut = async {
            if want_issues {
                self.fetch_all_issues().await
            } else {
                Ok(Vec::new())
            }
        };
        let (prs, issues) = tokio::join!(pr_fut, issue_fut);
        combine_selected_fetches(
            want_prs,
            want_issues,
            prs,
            issues.map(|tasks| (tasks, Vec::new())),
        )
        .map(|outcome| (outcome.tasks, outcome.partial_failure))
    }

    /// Round-robin variant of
    /// [`Self::fetch_selected_with_status_and_mentions`]: runs the PR side
    /// as a per-repo fan-out instead of a global `involves:USER`
    /// sweep, optionally OR'd with the global sweep on K-th refresh
    /// ticks. Same partial-failure semantics as the non-round-robin
    /// variant — a failed PR side keeps issues + mentions and vice
    /// versa.
    ///
    /// Caller is the polling layer: it consults
    /// [`crate::polling::pick_repos_for_tick`][polling]'s
    /// `RoundRobinPick` and passes `pick.repos` + `pick.run_global`
    /// through here.
    ///
    /// When BOTH `repos` is empty AND `run_global` is false (shouldn't
    /// happen given the scheduler's cold-start rule, but defensive),
    /// the PR side is skipped entirely.
    ///
    /// [polling]: ../../../lazybox_server/polling/fn.pick_repos_for_tick.html
    /// `pr_since` is the incremental `updated:>=` floor for the global
    /// `involves:` branch (issue #14); `None` runs an unwindowed
    /// reconcile. Ignored on the per-repo fan-out (those queries are
    /// already cheap and scoped).
    pub async fn fetch_round_robin_with_status_and_mentions(
        &self,
        want_prs: bool,
        repos: &[String],
        run_global: bool,
        want_issues: bool,
        allowed_logins: &std::collections::BTreeSet<String>,
        pr_since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<SelectedFetchOutcome, GhError> {
        // Issues are queried for `@lazybox` mentions even when issue
        // *display* is off — see `should_query_issues` (issue #50).
        let want_issue_side = should_query_issues(want_issues, allowed_logins);
        if !want_prs && !want_issue_side {
            return Ok(SelectedFetchOutcome {
                tasks: Vec::new(),
                partial_failure: None,
                mentions: Vec::new(),
                coverage: SelectedFetchCoverage::Complete,
            });
        }
        let do_pr_side = want_prs && (run_global || !repos.is_empty());
        let pr_fut = async {
            if !do_pr_side {
                return Ok(PrFetchOutcome::complete(Vec::new()));
            }
            if run_global {
                // Global sweep on this tick — same payload as the
                // pre-round-robin path. The per-repo fan-out is
                // skipped because the global already covers it.
                self.fetch_all_prs(pr_since)
                    .await
                    .map(PrFetchOutcome::complete)
            } else {
                self.fetch_prs_for_repos(repos).await
            }
        };
        let issue_fut = async {
            if want_issue_side {
                self.fetch_all_issues_with_mentions(allowed_logins).await
            } else {
                Ok((Vec::new(), Vec::new()))
            }
        };
        let (prs, issues) = tokio::join!(pr_fut, issue_fut);
        combine_selected_fetches(do_pr_side, want_issue_side, prs, issues)
    }

    /// Like `fetch_selected_with_status` but also runs the
    /// `@lazybox`-mention scan on the issues side. The returned
    /// [`LazyboxMention`](crate::LazyboxMention) list is empty when
    /// `allowed_logins` is empty (the mention feature is opt-in via
    /// config) or when no allowed user has written `@lazybox` on an
    /// unreacted body / comment. Errors fall back to the same
    /// partial-failure shape as the underlying call — a failed
    /// PR side keeps issues + mentions, and vice versa.
    pub async fn fetch_selected_with_status_and_mentions(
        &self,
        want_prs: bool,
        want_issues: bool,
        allowed_logins: &std::collections::BTreeSet<String>,
    ) -> Result<(Vec<Task>, Option<String>, Vec<crate::LazyboxMention>), GhError> {
        // Same decoupling as the round-robin variant: scan issues for
        // `@lazybox` mentions whenever the feature is active, even on a
        // PR-only inbox (issue #50).
        let want_issue_side = should_query_issues(want_issues, allowed_logins);
        if !want_prs && !want_issue_side {
            return Ok((Vec::new(), None, Vec::new()));
        }
        let pr_fut = async {
            if want_prs {
                self.fetch_all_prs(None).await.map(PrFetchOutcome::complete)
            } else {
                Ok(PrFetchOutcome::complete(Vec::new()))
            }
        };
        let issue_fut = async {
            if want_issue_side {
                self.fetch_all_issues_with_mentions(allowed_logins).await
            } else {
                Ok((Vec::new(), Vec::new()))
            }
        };
        let (prs, issues) = tokio::join!(pr_fut, issue_fut);
        combine_selected_fetches(want_prs, want_issue_side, prs, issues)
            .map(|outcome| (outcome.tasks, outcome.partial_failure, outcome.mentions))
    }

    /// Post a top-level comment on an issue or PR. PRs ARE issues in
    /// the REST API, so the same `issues/{n}/comments` endpoint works
    /// for both. `repo` is the `owner/name` shorthand the rest of the
    /// codebase uses; we split it once to feed octocrab's split-arg
    /// API.
    pub async fn post_issue_comment(
        &self,
        repo: &str,
        issue_or_pr_number: u64,
        body: &str,
    ) -> Result<(), GhError> {
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| GhError::Graphql(format!("repo '{repo}' not owner/name")))?;
        // REST endpoint, but it counts against the same hourly budget
        // as GraphQL — gate it. No rate-limit headers are exposed by
        // octocrab on this call, so we don't `observe` after.
        self.acquire_or_block("post issue comment")?;
        self.inner
            .issues(owner, name)
            .create_comment(issue_or_pr_number, body)
            .await
            .map_err(GhError::Api)?;
        Ok(())
    }

    /// Post a 👀 (`:eyes:`) reaction on any Reactable — typically an
    /// Issue body or an IssueComment for the `@lazybox`-mention
    /// auto-spawn flow. The reaction is the canonical idempotency
    /// marker for that flow: subsequent polls select
    /// `viewerHasReacted` and skip already-acknowledged surfaces, so
    /// lazybox doesn't re-spawn every cycle.
    ///
    /// Re-posting an existing reaction is a no-op on GitHub's side,
    /// so retrying on transient failure is safe.
    pub async fn react_eyes(&self, reactable_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("addReaction(EYES) mutation")?;
        let body = graphql::add_reaction_eyes_body(reactable_node_id);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("addReaction(EYES) errors for {reactable_node_id}: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    /// Merge the base branch into this PR's head — same as the "Update
    /// branch" button on github.com. Requires the PR's GraphQL node ID.
    pub async fn update_branch(&self, pull_request_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("updatePullRequestBranch mutation")?;
        let body = graphql::update_branch_body(pull_request_node_id);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            // Same idempotence guard as `merge_pr`: a timeout-retried
            // update whose first attempt landed comes back "no new
            // commits on the base branch" — the branch IS up to date,
            // which is exactly what the caller asked for.
            if gql_errors_all_match(&errors, BRANCH_ALREADY_UPDATED_MARKERS) {
                tracing::info!(
                    "updatePullRequestBranch reported nothing to update — \
                     treating as success (likely a timeout-retry re-send)"
                );
                return Ok(());
            }
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("updatePullRequestBranch errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    /// Lazy-fetch one PR's heavy fields — every field the inbox-scan
    /// `SEARCH_QUERY` deliberately omits to cut per-poll cost. Returns
    /// a `PrDetails` payload ready for the caller to splice into the
    /// workspace via `merge_pr_details` (or the equivalent inline
    /// fold in `handle_fetch_pr_details` / `prefetch_top_pr_details`).
    ///
    /// `Ok(None)` means the node lookup returned null — the PR was
    /// deleted or the token lost visibility between the inbox search
    /// and this call. Caller should treat that as "no update."
    pub async fn fetch_pr_details(
        &self,
        pull_request_node_id: &str,
    ) -> Result<Option<graphql::PrDetails>, GhError> {
        let started = std::time::Instant::now();
        let mut metrics = BranchMetrics::new("pr-details");
        self.acquire_or_block("PR details lazy-fetch")?;
        let body = graphql::pr_details_body(pull_request_node_id);
        let (response, bytes): (graphql::GqlPrDetailsResponse, usize) =
            self.post_graphql_with_retry_measured(&body).await?;
        metrics.requests = 1;
        metrics.resp_bytes = bytes;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("fetch_pr_details GraphQL errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        let data = response
            .data
            .ok_or_else(|| GhError::Graphql("fetch_pr_details: no data".into()))?;
        if let Some(rl) = &data.rate_limit {
            metrics.graphql_cost = rl.cost.unwrap_or(0);
            self.observe_rate_limit(rl);
        }
        metrics.elapsed_ms = started.elapsed().as_millis();
        metrics.emit();
        let Some(node) = data.node else {
            tracing::info!(
                "fetch_pr_details: node {} not found (deleted or scope changed)",
                pull_request_node_id,
            );
            return Ok(None);
        };
        Ok(Some(graphql::pr_details_to_details(&node, &self.user)))
    }

    /// Resolve a GitHub login to its node ID via GraphQL. Used as
    /// the lookup step before `requestReviews` / `addAssignees` —
    /// those mutations take node IDs, not logins.
    pub async fn lookup_user_id(&self, login: &str) -> Result<String, GhError> {
        self.acquire_or_block("user lookup query")?;
        let body = graphql::user_id_body(login);
        let response: graphql::GqlUserIdResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::Graphql(joined));
        }
        let id = response
            .data
            .and_then(|d| d.user)
            .map(|u| u.id)
            .ok_or_else(|| GhError::Graphql(format!("user `{login}` not found")))?;
        Ok(id)
    }

    /// Request reviews from the given logins on the PR. Adds to the
    /// existing reviewer set (`union: true`) so existing review
    /// requests aren't dropped. Resolves logins → user IDs first.
    pub async fn request_reviewers(
        &self,
        pull_request_node_id: &str,
        logins: &[String],
    ) -> Result<(), GhError> {
        if logins.is_empty() {
            return Ok(());
        }
        let mut user_ids: Vec<String> = Vec::with_capacity(logins.len());
        for login in logins {
            user_ids.push(self.lookup_user_id(login).await?);
        }
        self.acquire_or_block("requestReviews mutation")?;
        let body = graphql::request_reviews_body(pull_request_node_id, &user_ids);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("requestReviews errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    /// Add assignees to a PR or Issue (both are `Assignable`).
    /// Resolves logins → user IDs first.
    pub async fn add_assignees(
        &self,
        assignable_node_id: &str,
        logins: &[String],
    ) -> Result<(), GhError> {
        if logins.is_empty() {
            return Ok(());
        }
        let mut user_ids: Vec<String> = Vec::with_capacity(logins.len());
        for login in logins {
            user_ids.push(self.lookup_user_id(login).await?);
        }
        self.acquire_or_block("addAssigneesToAssignable mutation")?;
        let body = graphql::add_assignees_body(assignable_node_id, &user_ids);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("addAssigneesToAssignable errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    /// Remove assignees from a PR or Issue. Counterpart to
    /// `add_assignees`; the `SetAssignees` path fires both with the
    /// computed diff so the daemon can implement "make assignees
    /// exactly this list" without the TUI needing two round-trips.
    pub async fn remove_assignees(
        &self,
        assignable_node_id: &str,
        logins: &[String],
    ) -> Result<(), GhError> {
        if logins.is_empty() {
            return Ok(());
        }
        let mut user_ids: Vec<String> = Vec::with_capacity(logins.len());
        for login in logins {
            user_ids.push(self.lookup_user_id(login).await?);
        }
        self.acquire_or_block("removeAssigneesFromAssignable mutation")?;
        let body = graphql::remove_assignees_body(assignable_node_id, &user_ids);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("removeAssigneesFromAssignable errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    /// Fetch the repository's full label set (id + name + color +
    /// description). Cached by the caller — every repo's label
    /// set is small (typically under 50 entries) and changes
    /// rarely, so re-querying per picker open is fine.
    ///
    /// Named with the `_for_repo` suffix so this inherent method
    /// doesn't shadow the trait-side `TaskProvider::list_repo_labels`
    /// (which takes a `&Workspace` and delegates here).
    pub async fn list_labels_for_repo(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Vec<graphql::GqlRepoLabelNode>, GhError> {
        self.acquire_or_block("repository.labels query")?;
        let body = graphql::repo_labels_body(owner, name);
        let response: graphql::GqlRepoLabelsResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::Graphql(joined));
        }
        let data = response
            .data
            .ok_or_else(|| GhError::Graphql("list_repo_labels: no data".into()))?;
        if let Some(rl) = &data.rate_limit {
            self.observe_rate_limit(rl);
        }
        let nodes = data.repository.map(|r| r.labels.nodes).unwrap_or_default();
        Ok(nodes)
    }

    /// Add labels (by GraphQL node id) to any `Labelable` (PR or
    /// Issue). Empty `label_ids` returns Ok immediately.
    pub async fn add_labels(
        &self,
        labelable_node_id: &str,
        label_ids: &[String],
    ) -> Result<(), GhError> {
        if label_ids.is_empty() {
            return Ok(());
        }
        self.acquire_or_block("addLabelsToLabelable mutation")?;
        let body = graphql::add_labels_body(labelable_node_id, label_ids);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("addLabelsToLabelable errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    /// Sibling mutation for removing labels. Counterpart to
    /// `add_labels` — the `SetLabels` path fires both with the diff
    /// `existing − desired` after firing the add mutation with
    /// `desired − existing`.
    pub async fn remove_labels(
        &self,
        labelable_node_id: &str,
        label_ids: &[String],
    ) -> Result<(), GhError> {
        if label_ids.is_empty() {
            return Ok(());
        }
        self.acquire_or_block("removeLabelsFromLabelable mutation")?;
        let body = graphql::remove_labels_body(labelable_node_id, label_ids);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("removeLabelsFromLabelable errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    /// Resolve the repository's default merge method for a PR — the
    /// method github.com's merge button pre-selects, and (on a repo
    /// that disallows merge commits) the only method the merge mutation
    /// will accept.
    pub async fn pr_merge_method(&self, pull_request_node_id: &str) -> Result<String, GhError> {
        self.acquire_or_block("pr merge-method query")?;
        let body = graphql::pr_merge_method_body(pull_request_node_id);
        let response: graphql::GqlMergeMethodResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::Graphql(joined));
        }
        response
            .data
            .and_then(|d| d.node)
            .map(|n| n.repository.viewer_default_merge_method)
            .ok_or_else(|| GhError::Graphql("PR node has no repository merge method".to_string()))
    }

    /// Merge a PR. `expected_head_oid` — when known — pins the merge to
    /// that head commit: GitHub rejects the mutation ("Head branch was
    /// modified…") if anything was pushed since, so a force-push between
    /// "observed green" and "merge" can't land unverified commits. Pass
    /// `None` only when no verified head is available (the guard is then
    /// skipped, matching the pre-#expectedHeadOid behavior).
    pub async fn merge_pr(
        &self,
        pull_request_node_id: &str,
        expected_head_oid: Option<&str>,
    ) -> Result<(), GhError> {
        let merge_method = self.pr_merge_method(pull_request_node_id).await?;
        self.acquire_or_block("mergePullRequest mutation")?;
        let body = graphql::merge_pr_body(pull_request_node_id, &merge_method, expected_head_oid);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            // Idempotence guard: `post_graphql_with_retry` re-sends the
            // mutation after a client-side timeout even when the first
            // attempt LANDED server-side. The retry then reports
            // "already merged" for a merge that actually succeeded —
            // surfacing that as FAILURE told the user their successful
            // merge failed. GitHub confirming the PR is merged IS the
            // outcome this call wanted, so classify it as success.
            // Deliberately narrow: "not mergeable" (conflicts, blocked
            // checks) stays a real failure.
            if gql_errors_all_match(&errors, ALREADY_MERGED_MARKERS) {
                tracing::info!(
                    "mergePullRequest reported the PR already merged — \
                     treating as success (likely a timeout-retry re-send)"
                );
                return Ok(());
            }
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("mergePullRequest errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    pub async fn close_issue_node(&self, issue_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("closeIssue mutation")?;
        let body = graphql::close_issue_body(issue_node_id);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("closeIssue errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    pub async fn close_pr_node(&self, pull_request_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("closePullRequest mutation")?;
        let body = graphql::close_pr_body(pull_request_node_id);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("closePullRequest errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }

    pub async fn delete_issue_node(&self, issue_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("deleteIssue mutation")?;
        let body = graphql::delete_issue_body(issue_node_id);
        let response: graphql::GqlMutationResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!("deleteIssue errors: {joined}");
            return Err(GhError::Graphql(joined));
        }
        Ok(())
    }
}

impl lazybox_core::TaskProvider for GhClient {
    fn name(&self) -> &str {
        "github"
    }

    async fn fetch_tasks(&self) -> Result<Vec<lazybox_core::Task>, lazybox_core::ProviderError> {
        self.fetch_all_prs(None).await.map_err(Into::into)
    }

    fn username(&self) -> Option<&str> {
        Some(&self.user)
    }

    /// Merge the workspace's PR. Requires `workspace.pr.node_id`
    /// (the GraphQL node id) — the polling cycle fills it in;
    /// hitting this on a fresh-from-cache workspace surfaces as
    /// `Permanent("PR has no node_id")` which the caller can
    /// translate to "repoll first".
    ///
    /// `expected_head_oid` pins the merge to that head commit
    /// (`mergePullRequest`'s `expectedHeadOid`); a head that moved
    /// since surfaces as GitHub's own "Head branch was modified"
    /// rejection, which the caller reports via `Event::PrMergeFailed`.
    async fn merge(
        &self,
        workspace: &lazybox_core::Workspace,
        expected_head_oid: Option<&str>,
    ) -> Result<(), lazybox_core::ProviderError> {
        let Some(pr) = workspace.pr.as_ref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no PR", workspace.key),
            ));
        };
        let Some(node_id) = pr.node_id.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "PR has no node_id (poll first)",
            ));
        };
        self.merge_pr(node_id, expected_head_oid)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Update the workspace's PR branch against its base — the "Update
    /// branch" button on github.com. Requires `workspace.pr.node_id`
    /// (the polling cycle fills it in).
    async fn update_branch(
        &self,
        workspace: &lazybox_core::Workspace,
    ) -> Result<(), lazybox_core::ProviderError> {
        let Some(pr) = workspace.pr.as_ref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no PR", workspace.key),
            ));
        };
        let Some(node_id) = pr.node_id.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "PR has no node_id (poll first)",
            ));
        };
        self.update_branch(node_id)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Close the workspace's GitHub issue (as `NOT_PLANNED`). GitHub
    /// exposes no non-admin issue *delete* over the API, so this is
    /// lazybox's "delete issue." Requires the issue's `node_id` — the
    /// polling cycle fills it in.
    async fn close_issue(
        &self,
        workspace: &lazybox_core::Workspace,
    ) -> Result<(), lazybox_core::ProviderError> {
        let Some(issue) = workspace.gh_issues.first() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no issue", workspace.key),
            ));
        };
        let Some(node_id) = issue.node_id.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "issue has no node_id (poll first)",
            ));
        };
        self.close_issue_node(node_id)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Close the workspace's PR without merging. Requires
    /// `workspace.pr.node_id` — the polling cycle fills it in.
    async fn close_pr(
        &self,
        workspace: &lazybox_core::Workspace,
    ) -> Result<(), lazybox_core::ProviderError> {
        let Some(pr) = workspace.pr.as_ref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no PR", workspace.key),
            ));
        };
        let Some(node_id) = pr.node_id.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "PR has no node_id (poll first)",
            ));
        };
        self.close_pr_node(node_id)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Hard-delete the workspace's GitHub issue. Only repo admins may
    /// delete — GitHub answers FORBIDDEN otherwise, and the caller is
    /// expected to fall back to [`close_issue`](Self::close_issue).
    async fn delete_issue(
        &self,
        workspace: &lazybox_core::Workspace,
    ) -> Result<(), lazybox_core::ProviderError> {
        let Some(issue) = workspace.gh_issues.first() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no issue", workspace.key),
            ));
        };
        let Some(node_id) = issue.node_id.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "issue has no node_id (poll first)",
            ));
        };
        self.delete_issue_node(node_id)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Request reviewer(s) on the workspace's PR. Logins are
    /// github usernames (no `@` prefix). Daemon resolves logins →
    /// node ids inside `request_reviewers`.
    async fn request_reviewers(
        &self,
        workspace: &lazybox_core::Workspace,
        logins: &[String],
    ) -> Result<(), lazybox_core::ProviderError> {
        let Some(pr) = workspace.pr.as_ref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no PR", workspace.key),
            ));
        };
        let Some(node_id) = pr.node_id.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "PR has no node_id (poll first)",
            ));
        };
        self.request_reviewers(node_id, logins)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Add assignee(s) to the workspace's PR or issue. Both are
    /// GraphQL `Assignable` so a single mutation covers them.
    async fn add_assignees(
        &self,
        workspace: &lazybox_core::Workspace,
        logins: &[String],
    ) -> Result<(), lazybox_core::ProviderError> {
        // Prefer the PR's node_id; fall back to the first issue's
        // node_id for issue-only workspaces.
        let node_id = workspace
            .pr
            .as_ref()
            .and_then(|p| p.node_id.as_deref())
            .or_else(|| {
                workspace
                    .gh_issues
                    .first()
                    .and_then(|i| i.node_id.as_deref())
            });
        let Some(node_id) = node_id else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!(
                    "workspace {} has neither a PR nor an issue with a node_id",
                    workspace.key
                ),
            ));
        };
        self.add_assignees(node_id, logins)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Replace the assignee set on the workspace's PR or issue.
    /// Computes the diff against the task's persisted assignees and
    /// fires both `addAssigneesToAssignable` and
    /// `removeAssigneesFromAssignable` mutations. Empty `logins`
    /// clears every assignee (intentional — the UX cycles through
    /// an unchecked picker for that case).
    async fn set_assignees(
        &self,
        workspace: &lazybox_core::Workspace,
        logins: &[String],
    ) -> Result<(), lazybox_core::ProviderError> {
        // Resolve the same Assignable node we'd add against.
        let (node_id, existing) = {
            let pr_match = workspace
                .pr
                .as_ref()
                .and_then(|p| p.node_id.as_deref().map(|n| (n, &p.assignees)));
            let issue_match = workspace
                .gh_issues
                .first()
                .and_then(|i| i.node_id.as_deref().map(|n| (n, &i.assignees)));
            pr_match.or(issue_match).ok_or_else(|| {
                lazybox_core::ProviderError::permanent(
                    "github",
                    format!(
                        "workspace {} has neither a PR nor an issue with a node_id",
                        workspace.key
                    ),
                )
            })?
        };
        // Compute the symmetric diff against `existing`.
        let desired: std::collections::HashSet<&str> = logins.iter().map(String::as_str).collect();
        let current: std::collections::HashSet<&str> =
            existing.iter().map(String::as_str).collect();
        let to_add: Vec<String> = desired
            .difference(&current)
            .map(|s| s.to_string())
            .collect();
        let to_remove: Vec<String> = current
            .difference(&desired)
            .map(|s| s.to_string())
            .collect();
        if to_add.is_empty() && to_remove.is_empty() {
            tracing::debug!(
                workspace = %workspace.key,
                "set_assignees: no-op (desired matches existing)"
            );
            return Ok(());
        }
        if !to_add.is_empty() {
            self.add_assignees(node_id, &to_add)
                .await
                .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))?;
        }
        if !to_remove.is_empty() {
            self.remove_assignees(node_id, &to_remove)
                .await
                .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))?;
        }
        Ok(())
    }

    /// Post a reply (comment) on the workspace's PR or issue.
    /// Uses `post_issue_comment` because github's REST API exposes
    /// the same endpoint for both (PRs are issues at the REST
    /// layer) — `pr.number` doubles as the issue number.
    async fn post_reply(
        &self,
        workspace: &lazybox_core::Workspace,
        body: &str,
    ) -> Result<(), lazybox_core::ProviderError> {
        let primary = workspace.primary_task().ok_or_else(|| {
            lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no primary task", workspace.key),
            )
        })?;
        let Some(repo) = primary.repo.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "primary task has no repo",
            ));
        };
        // PR / issue number is encoded in the TaskId key as
        // `owner/repo#NNN` (or a `-NNN` suffix for legacy keys).
        // GitHub's REST API treats both as "issue numbers" so the
        // same value works for `post_issue_comment` regardless of
        // whether the workspace is PR-shaped or issue-shaped.
        let number = primary
            .id
            .key
            .rsplit_once('#')
            .and_then(|(_, n)| n.parse::<u64>().ok())
            .or_else(|| {
                primary
                    .id
                    .key
                    .rsplit_once('-')
                    .and_then(|(_, n)| n.parse::<u64>().ok())
            });
        let Some(number) = number else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("can't parse number from task key `{}`", primary.id.key),
            ));
        };
        self.post_issue_comment(repo, number, body)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))
    }

    /// List the labels defined on the workspace's repository. Both
    /// PRs and issues live under a single repo; we resolve via the
    /// primary task's `repo` field.
    async fn list_repo_labels(
        &self,
        workspace: &lazybox_core::Workspace,
    ) -> Result<Vec<lazybox_core::Label>, lazybox_core::ProviderError> {
        let primary = workspace.primary_task().ok_or_else(|| {
            lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no primary task", workspace.key),
            )
        })?;
        let Some(repo) = primary.repo.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "primary task has no repo",
            ));
        };
        let Some((owner, name)) = repo.split_once('/') else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("can't parse owner/name from `{repo}`"),
            ));
        };
        let nodes = self
            .list_labels_for_repo(owner, name)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))?;
        Ok(nodes
            .into_iter()
            .map(|n| lazybox_core::Label {
                name: n.name,
                color: n.color.unwrap_or_default(),
            })
            .collect())
    }

    /// Replace the label set on the workspace's PR or issue. Looks
    /// up the repo's labels (to resolve names → GraphQL node ids),
    /// diffs against the task's persisted labels, and fires
    /// `addLabelsToLabelable` + `removeLabelsFromLabelable` as
    /// needed. Empty `names` clears every label.
    async fn set_labels(
        &self,
        workspace: &lazybox_core::Workspace,
        names: &[String],
    ) -> Result<(), lazybox_core::ProviderError> {
        // Find the labelable node — prefer the PR, fall back to the
        // first issue. Same shape as set_assignees: both PRs and
        // issues implement the `Labelable` interface. We borrow the
        // existing label slice (no per-name clone) and just remember
        // the node id we'll mutate against.
        let (node_id, existing) = workspace
            .pr
            .as_ref()
            .and_then(|p| p.node_id.as_deref().map(|n| (n, p.labels.as_slice())))
            .or_else(|| {
                workspace
                    .gh_issues
                    .first()
                    .and_then(|i| i.node_id.as_deref().map(|n| (n, i.labels.as_slice())))
            })
            .ok_or_else(|| {
                lazybox_core::ProviderError::permanent(
                    "github",
                    format!(
                        "workspace {} has neither a PR nor an issue with a node_id",
                        workspace.key
                    ),
                )
            })?;

        // Need the repo's labels to map names → ids. Pull them once;
        // anything the user picked that isn't in the repo's set is
        // silently dropped (can't apply a label that doesn't exist).
        let repo = workspace
            .primary_task()
            .and_then(|t| t.repo.as_deref())
            .ok_or_else(|| {
                lazybox_core::ProviderError::permanent("github", "primary task has no repo")
            })?;
        let (owner, name) = repo.split_once('/').ok_or_else(|| {
            lazybox_core::ProviderError::permanent("github", format!("bad repo string `{repo}`"))
        })?;
        let repo_labels = self
            .list_labels_for_repo(owner, name)
            .await
            .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))?;
        // Hash lookup keeps the typical 0-50 repo-labels × 0-10 picks
        // case constant-time without the two extra HashSet allocs the
        // prior diff path used.
        let id_by_name: std::collections::HashMap<&str, &str> = repo_labels
            .iter()
            .map(|l| (l.name.as_str(), l.id.as_str()))
            .collect();
        // Linear diff: `to_add` = names ∈ desired \ existing,
        // `to_remove` = names ∈ existing \ desired. At ≤10 entries
        // on each side the inner `.any()` scan beats the cost of
        // building two HashSets.
        let to_add: Vec<String> = names
            .iter()
            .filter(|n| !existing.iter().any(|l| &l.name == *n))
            .filter_map(|n| id_by_name.get(n.as_str()).map(|id| (*id).to_string()))
            .collect();
        let to_remove: Vec<String> = existing
            .iter()
            .filter(|l| !names.iter().any(|n| n == &l.name))
            .filter_map(|l| id_by_name.get(l.name.as_str()).map(|id| (*id).to_string()))
            .collect();
        if to_add.is_empty() && to_remove.is_empty() {
            tracing::debug!(
                workspace = %workspace.key,
                "set_labels: no-op (desired matches existing)"
            );
            return Ok(());
        }
        if !to_add.is_empty() {
            self.add_labels(node_id, &to_add)
                .await
                .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))?;
        }
        if !to_remove.is_empty() {
            self.remove_labels(node_id, &to_remove)
                .await
                .map_err(|e| lazybox_core::ProviderError::permanent("github", e.to_string()))?;
        }
        Ok(())
    }
}

/// Whether the issue-side GraphQL query should run on a poll tick.
///
/// Runs when EITHER:
///   - the user wants issues displayed in the inbox (`want_issues`), OR
///   - the `@lazybox` mention feature is active (a non-empty allowlist).
///
/// The second clause is the fix for issue #50: the `@lazybox` auto-spawn
/// trigger lives on the issues side, but the GitHub default filter is
/// PR-only (`issue.*` keys unset → `want_issues == false`). Tying the
/// mention scan to `want_issues` meant a default/PR-only inbox silently
/// never ingested `@lazybox` work. The non-mention issues pulled by a
/// mention-only scan are dropped downstream by `filter_github_tasks`,
/// so they don't leak into the displayed inbox.
pub(crate) fn should_query_issues(
    want_issues: bool,
    allowed_logins: &std::collections::BTreeSet<String>,
) -> bool {
    want_issues || !allowed_logins.is_empty()
}

/// GraphQL error messages that mean "the PR is already merged" — the
/// state a `mergePullRequest` was trying to reach. GitHub phrases it as
/// `Pull request Pull request is in merged state.` (yes, doubled) on
/// current api.github.com; older/GHES variants say "already merged".
/// Matched case-insensitively as substrings.
const ALREADY_MERGED_MARKERS: &[&str] = &["already merged", "merged state"];

/// GraphQL error messages that mean an `updatePullRequestBranch` has
/// nothing left to do — the head already contains the base. GitHub:
/// `There are no new commits on the base branch.`; "up to date"
/// variants cover GHES phrasing. Also treated as applied when the PR
/// merged between the attempts (nothing left to update).
const BRANCH_ALREADY_UPDATED_MARKERS: &[&str] = &[
    "no new commits",
    "already up-to-date",
    "already up to date",
    "merged state",
    "already merged",
];

/// True when the mutation's error list is non-empty and EVERY entry
/// matches one of `markers` (case-insensitive substring on the raw
/// message). All-of, not any-of: a response mixing "already merged"
/// with an unrelated error must still surface as a failure.
fn gql_errors_all_match(errors: &[graphql::GqlError], markers: &[&str]) -> bool {
    !errors.is_empty()
        && errors.iter().all(|e| {
            let msg = e.message.to_ascii_lowercase();
            markers.iter().any(|marker| msg.contains(marker))
        })
}

/// True when the error list is non-empty and EVERY entry means the
/// queried node is definitively not visible (NOT_FOUND / FORBIDDEN).
/// All-of, not any-of: a response mixing a not-found with a genuinely
/// transient error must still surface as `Err` so the deep-fetch caller
/// holds the notifications cursor and re-lists the entry next tick
/// (#512). A permanently-gone entry, by contrast, resolves to `Ok(None)`
/// so it can't pin the cursor off the cheap 304 steady state forever.
fn gql_errors_all_not_visible(errors: &[graphql::GqlError]) -> bool {
    !errors.is_empty() && errors.iter().all(graphql::GqlError::is_not_visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn logins(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn graphql_error(message: &str) -> GhError {
        GhError::Graphql(message.to_string())
    }

    #[test]
    fn successful_empty_side_keeps_degraded_sync_alive() {
        let outcome = combine_selected_fetches(
            true,
            true,
            Ok(PrFetchOutcome::complete(Vec::new())),
            Err(graphql_error("issues unavailable")),
        )
        .expect("an empty successful PR fetch is still a successful side");
        assert!(outcome.tasks.is_empty());
        assert!(
            outcome
                .partial_failure
                .as_deref()
                .is_some_and(|message| message.contains("issues unavailable"))
        );
        assert!(outcome.coverage.pr_complete());
        assert!(!outcome.coverage.sweep_complete());

        let outcome = combine_selected_fetches(
            true,
            true,
            Err(graphql_error("PRs unavailable")),
            Ok((Vec::new(), Vec::new())),
        )
        .expect("an empty successful issue fetch is still a successful side");
        assert!(outcome.tasks.is_empty());
        assert!(
            outcome
                .partial_failure
                .as_deref()
                .is_some_and(|message| message.contains("PRs unavailable"))
        );
        assert!(!outcome.coverage.pr_complete());
        assert!(!outcome.coverage.sweep_complete());
    }

    #[test]
    fn failure_of_the_only_requested_side_still_fails_sync() {
        assert!(
            combine_selected_fetches(
                false,
                true,
                Ok(PrFetchOutcome::complete(Vec::new())),
                Err(graphql_error("issues unavailable")),
            )
            .is_err()
        );
        assert!(
            combine_selected_fetches(
                true,
                false,
                Err(graphql_error("PRs unavailable")),
                Ok((Vec::new(), Vec::new())),
            )
            .is_err()
        );
    }

    #[test]
    fn next_page_requires_a_cursor() {
        let error = next_page_cursor(
            Some(graphql::GqlPageInfo {
                has_next_page: true,
                end_cursor: None,
            }),
            "test search",
        )
        .expect_err("a next page without its cursor is incomplete coverage");
        assert!(matches!(error, GhError::Graphql(message) if message.contains("endCursor=null")));
        assert_eq!(
            next_page_cursor(
                Some(graphql::GqlPageInfo {
                    has_next_page: true,
                    end_cursor: Some("CURSOR".into()),
                }),
                "test search",
            )
            .unwrap()
            .as_deref(),
            Some("CURSOR")
        );
        assert!(
            next_page_cursor(Some(graphql::GqlPageInfo::default()), "test search")
                .unwrap()
                .is_none()
        );
        assert!(
            next_page_cursor(None, "test search")
                .expect_err("missing pageInfo is incomplete coverage")
                .to_string()
                .contains("omitted pageInfo")
        );
    }

    #[test]
    fn byte_bounded_body_prefix_preserves_utf8() {
        let body = format!("{}💥tail", "a".repeat(511));
        let prefix = body_prefix_bytes(&body, 512);
        assert_eq!(prefix.len(), 511);
        assert_eq!(prefix, "a".repeat(511));
        assert_eq!(body_prefix_bytes(&body, body.len()), body);
    }

    /// Parse a raw GraphQL mutation-response payload the way
    /// `post_graphql_with_retry` does, returning its error list.
    fn mutation_errors(payload: &str) -> Vec<graphql::GqlError> {
        let response: graphql::GqlMutationResponse =
            serde_json::from_str(payload).expect("payload parses");
        response.errors.unwrap_or_default()
    }

    /// A `mergePullRequest` re-sent after a client-side timeout (first
    /// attempt landed server-side) fails with GitHub's already-merged
    /// error. That response must classify as success — surfacing it as
    /// FAILURE told the user a merge that succeeded had failed.
    #[test]
    fn already_merged_error_classifies_as_success() {
        let errors = mutation_errors(
            r#"{"data":{"mergePullRequest":null},
                "errors":[{"message":"Pull request Pull request is in merged state.",
                           "path":["mergePullRequest"],
                           "extensions":{"type":"UNPROCESSABLE"}}]}"#,
        );
        assert!(gql_errors_all_match(&errors, ALREADY_MERGED_MARKERS));
        let errors =
            mutation_errors(r#"{"errors":[{"message":"Pull request is already merged"}]}"#);
        assert!(gql_errors_all_match(&errors, ALREADY_MERGED_MARKERS));
    }

    /// "Not mergeable" (conflicts, blocked checks) is a GENUINE
    /// failure — the narrow already-merged classifier must not eat it.
    #[test]
    fn not_mergeable_error_stays_a_failure() {
        let errors = mutation_errors(
            r#"{"errors":[{"message":"Pull Request is not mergeable","path":["mergePullRequest"]}]}"#,
        );
        assert!(!gql_errors_all_match(&errors, ALREADY_MERGED_MARKERS));
    }

    /// All-of semantics: an already-merged error mixed with an
    /// unrelated one must still surface as failure, and an empty error
    /// list is not a match (that's the plain success path).
    #[test]
    fn mixed_or_empty_error_lists_do_not_classify_as_already_applied() {
        let errors = mutation_errors(
            r#"{"errors":[{"message":"Pull request Pull request is in merged state."},
                          {"message":"Something else went wrong"}]}"#,
        );
        assert!(!gql_errors_all_match(&errors, ALREADY_MERGED_MARKERS));
        assert!(!gql_errors_all_match(&[], ALREADY_MERGED_MARKERS));
    }

    /// #512: a repo-level `NOT_FOUND` / `FORBIDDEN` GraphQL error
    /// (deleted, transferred, private, scope revoked) is a *definitive*
    /// "not visible" answer — the deep-fetch must map it to `Ok(None)`
    /// so the notifications cursor can advance. Only a genuinely
    /// transient error (or a not-found mixed with one) holds the cursor.
    /// Without this, a lingering notification for a now-inaccessible repo
    /// returns `Err` every tick and pins the heartbeat off its cheap 304
    /// forever.
    #[test]
    fn gql_not_found_and_forbidden_classify_as_not_visible() {
        fn single_pr_errors(payload: &str) -> Vec<graphql::GqlError> {
            serde_json::from_str::<graphql::GqlSinglePrResponse>(payload)
                .expect("payload parses")
                .errors
                .unwrap_or_default()
        }

        // Top-level NOT_FOUND on the repository node (repo gone / private),
        // carried alongside `data.repository = null`.
        let errs = single_pr_errors(
            r#"{"data":{"repository":null},
                "errors":[{"type":"NOT_FOUND","path":["repository"],
                           "message":"Could not resolve to a Repository with the name 'o/r'."}]}"#,
        );
        assert!(gql_errors_all_not_visible(&errs));

        // FORBIDDEN (token scope revoked) is also definitively not visible.
        let errs = single_pr_errors(
            r#"{"errors":[{"type":"FORBIDDEN","message":"Resource not accessible by integration"}]}"#,
        );
        assert!(gql_errors_all_not_visible(&errs));

        // Message fallback when GitHub omits the `type` field (GHES).
        let errs = single_pr_errors(
            r#"{"errors":[{"message":"Could not resolve to a PullRequest with the number of 123."}]}"#,
        );
        assert!(gql_errors_all_not_visible(&errs));

        // A transient/unknown error must NOT be swallowed — it holds the cursor.
        let errs = single_pr_errors(
            r#"{"errors":[{"type":"INTERNAL","message":"Something went wrong while executing your query."}]}"#,
        );
        assert!(!gql_errors_all_not_visible(&errs));

        // All-of: not-found mixed with a transient error still fails.
        let errs = single_pr_errors(
            r#"{"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a Repository"},
                          {"type":"INTERNAL","message":"timeout"}]}"#,
        );
        assert!(!gql_errors_all_not_visible(&errs));

        // Empty error list is the plain success path, not "not visible".
        assert!(!gql_errors_all_not_visible(&[]));
    }

    /// `updatePullRequestBranch` retried after its first attempt
    /// landed reports "no new commits on the base branch" — the branch
    /// is up to date, which is the requested outcome.
    #[test]
    fn update_branch_no_new_commits_classifies_as_success() {
        let errors = mutation_errors(
            r#"{"errors":[{"message":"There are no new commits on the base branch.",
                           "path":["updatePullRequestBranch"]}]}"#,
        );
        assert!(gql_errors_all_match(
            &errors,
            BRANCH_ALREADY_UPDATED_MARKERS
        ));
    }

    /// Issue #15: the watched-repo fan-out must exclude PRs the user
    /// is already involved in — those come back through the main
    /// `involves:` branch in the same poll, so re-fetching them here is
    /// duplicate download. The `-involves:USER` negation pushes that
    /// dedup to GitHub's side, cutting bytes *and* union dedup waste.
    #[test]
    fn watched_repo_query_excludes_involves() {
        let q = watched_repo_query("octo/repo", "test-user");
        assert_eq!(
            q,
            "is:open is:pr repo:octo/repo archived:false -involves:test-user"
        );
        assert!(
            q.contains("-involves:test-user"),
            "watched query must negate the user's involvement: {q}"
        );
    }

    #[test]
    fn issue_query_runs_when_issues_displayed() {
        assert!(should_query_issues(true, &logins(&[])));
        assert!(should_query_issues(true, &logins(&["alice"])));
    }

    #[test]
    fn issue_query_runs_for_mentions_even_when_issue_display_off() {
        // Regression (issue #50): PR-only inbox must still scan issues
        // for `@lazybox` mentions when the allowlist is non-empty.
        assert!(
            should_query_issues(false, &logins(&["alice"])),
            "@lazybox scan must run even when issue display is off"
        );
    }

    #[test]
    fn issue_query_skipped_when_neither_display_nor_mentions() {
        assert!(!should_query_issues(false, &logins(&[])));
    }

    fn http_status(status: u16) -> GhError {
        GhError::HttpStatus {
            status,
            reason: String::new(),
            content_type: "application/json; charset=utf-8".to_string(),
            body_excerpt: String::new(),
        }
    }

    /// A typed HTTP status routes through the shared `classify_status`
    /// in `lazybox-core` — same verdicts Linear gets, so the two can't
    /// disagree: 5xx → retryable, 401/403 → auth, other 4xx → permanent.
    #[test]
    fn http_status_delegates_to_shared_classifier() {
        assert!(lazybox_core::ProviderError::from(http_status(503)).is_retryable());
        assert!(lazybox_core::ProviderError::from(http_status(429)).is_retryable());
        assert!(lazybox_core::ProviderError::from(http_status(401)).is_auth());
        assert!(lazybox_core::ProviderError::from(http_status(403)).is_auth());
        let perm = lazybox_core::ProviderError::from(http_status(404));
        assert!(!perm.is_retryable() && !perm.is_auth());
        let perm = lazybox_core::ProviderError::from(http_status(422));
        assert!(!perm.is_retryable() && !perm.is_auth());
    }

    /// A 2xx with a non-JSON body keeps its GitHub-specific retry quirk
    /// (proxy/CDN maintenance page) on top of the shared classifier.
    #[test]
    fn two_hundred_non_json_stays_retryable() {
        let err = GhError::HttpStatus {
            status: 200,
            reason: String::new(),
            content_type: "text/html".to_string(),
            body_excerpt: "<html>maintenance</html>".to_string(),
        };
        assert!(lazybox_core::ProviderError::from(err).is_retryable());
    }

    /// A stringly GraphQL wrapper error has no typed status, so it
    /// falls through to the shared substring probe. Matching the
    /// shared classifier's verdicts proves the fallback delegates.
    #[test]
    fn graphql_wrapper_delegates_to_shared_substring_probe() {
        let retry = lazybox_core::ProviderError::from(GhError::Graphql(
            "connection reset by peer".to_string(),
        ));
        assert!(retry.is_retryable());
        let perm = lazybox_core::ProviderError::from(GhError::Graphql(
            "field 'foo' does not exist".to_string(),
        ));
        assert!(!perm.is_retryable() && !perm.is_auth());
    }

    /// Spin up a tiny TCP server that answers every request with the
    /// same canned HTTP response. Returns the `http://addr` base URI
    /// to feed into `Octocrab::builder().base_uri(…)`. We hand-roll
    /// the response instead of pulling in `wiremock` because we need
    /// only one canned answer per test and want zero new deps.
    async fn spawn_canned_response_server(
        status_line: &'static str,
        content_type: &'static str,
        body: &'static str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                tokio::spawn(async move {
                    // Read until we see the end of the HTTP request
                    // headers (\r\n\r\n) so the client doesn't see
                    // a premature close before its write completes.
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\n\
                         Content-Type: {content_type}\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// Like `spawn_canned_response_server`, but counts connections
    /// (so tests can assert "no hot retry happened") and injects
    /// extra response headers (`Retry-After`, `x-ratelimit-*`).
    async fn spawn_counting_response_server(
        status_line: &'static str,
        content_type: &'static str,
        extra_headers: &'static str,
        body: &'static str,
        counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\n\
                         Content-Type: {content_type}\r\n\
                         {extra_headers}\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    // ── rate-limit responses (429 / 403 secondary) ────────────────

    /// A 429 with `Retry-After` must surface as `RateLimited` carrying
    /// the server's own wait hint, must NOT be retried on the in-call
    /// millisecond ladder, and must poison the shared budget so
    /// admission control stops admitting until the window reopens.
    #[tokio::test(flavor = "current_thread")]
    async fn rate_limit_429_no_hot_retry_and_budget_updated() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_uri = spawn_counting_response_server(
            "429 Too Many Requests",
            "application/json",
            "Retry-After: 7\r\n",
            r#"{"message":"API rate limit exceeded"}"#,
            hits.clone(),
        )
        .await;
        let client = make_client(&base_uri);

        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<serde_json::Value>(&body)
            .await
            .expect_err("429 must fail");
        match &err {
            GhError::RateLimited {
                retry_after_secs,
                reason,
            } => {
                assert_eq!(*retry_after_secs, 7, "Retry-After header must be honored");
                assert!(reason.contains("429"), "reason names the status: {reason}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a 429 must not be retried hot inside the call"
        );
        // The ProviderError mapping preserves the hint for the poll
        // scheduler's backoff clamp.
        let pe = lazybox_core::ProviderError::from(err);
        assert!(pe.is_retryable());
        assert_eq!(pe.retry_after_secs(), Some(7));
        // Budget fed: the next acquire is refused until the window.
        match client.budget.lock().try_acquire() {
            Err(crate::rate_budget::AcquireError::RemoteLow { remaining, .. }) => {
                assert_eq!(remaining, 0, "throttle observation records 0 remaining");
            }
            other => panic!("budget must refuse admission after a 429, got {other:?}"),
        }
    }

    /// GitHub signals secondary (abuse) limits as a 403 with a
    /// documented message + `Retry-After`. That must classify as
    /// RateLimited — not as an auth failure — and skip the hot ladder.
    #[tokio::test(flavor = "current_thread")]
    async fn secondary_limit_403_classifies_rate_limited() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_uri = spawn_counting_response_server(
            "403 Forbidden",
            "application/json",
            "Retry-After: 30\r\n",
            r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}"#,
            hits.clone(),
        )
        .await;
        let client = make_client(&base_uri);

        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<serde_json::Value>(&body)
            .await
            .expect_err("403 secondary limit must fail");
        match &err {
            GhError::RateLimited {
                retry_after_secs, ..
            } => assert_eq!(*retry_after_secs, 30),
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let pe = lazybox_core::ProviderError::from(err);
        assert!(
            pe.is_retryable() && !pe.is_auth(),
            "a secondary limit is throttling, not a dead token"
        );
    }

    /// A plain 403 (no limit markers, no Retry-After) stays an auth
    /// failure — the secondary-limit carve-out must not swallow real
    /// permission errors.
    #[tokio::test(flavor = "current_thread")]
    async fn plain_403_still_classifies_auth() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_uri = spawn_counting_response_server(
            "403 Forbidden",
            "application/json",
            "",
            r#"{"message":"Resource not accessible by integration"}"#,
            hits.clone(),
        )
        .await;
        let client = make_client(&base_uri);

        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<serde_json::Value>(&body)
            .await
            .expect_err("plain 403 must fail");
        assert!(
            matches!(&err, GhError::HttpStatus { status: 403, .. }),
            "got {err:?}"
        );
        assert!(lazybox_core::ProviderError::from(err).is_auth());
    }

    /// 5xx keeps its transient classification but gets exactly ONE
    /// in-call retry — sustained outages are spaced by the poll-level
    /// backoff, not burned down the 200ms/800ms ladder.
    #[tokio::test(flavor = "current_thread")]
    async fn five_hundred_gets_exactly_one_in_call_retry() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_uri = spawn_counting_response_server(
            "500 Internal Server Error",
            "text/html",
            "",
            "<html>boom</html>",
            hits.clone(),
        )
        .await;
        let client = make_client(&base_uri);

        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<serde_json::Value>(&body)
            .await
            .expect_err("500 must fail");
        assert!(
            matches!(&err, GhError::HttpStatus { status: 500, .. }),
            "got {err:?}"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "5xx = initial attempt + exactly one in-call retry"
        );
        assert!(lazybox_core::ProviderError::from(err).is_retryable());
    }

    // ── rate-limit header parsing / detection (pure) ──────────────

    #[test]
    fn rate_limit_headers_prefer_retry_after() {
        let h = RateLimitHeaders::parse(Some("120"), Some("0"), Some("1750000000"));
        assert_eq!(h.wait_secs(1749999000), 120);
    }

    #[test]
    fn rate_limit_headers_fall_back_to_reset_epoch() {
        let h = RateLimitHeaders::parse(None, Some("0"), Some("1750000090"));
        assert_eq!(h.wait_secs(1750000000), 90);
    }

    #[test]
    fn rate_limit_headers_default_when_absent_and_clamp_past_reset() {
        let none = RateLimitHeaders::parse(None, None, None);
        assert_eq!(none.wait_secs(1750000000), 60, "no hints → 60s default");
        let past = RateLimitHeaders::parse(None, None, Some("1749999000"));
        assert_eq!(
            past.wait_secs(1750000000),
            1,
            "reset in the past clamps to 1s"
        );
    }

    #[test]
    fn rate_limit_detection_covers_429_and_documented_403s() {
        assert!(is_rate_limit_response(429, "", false));
        assert!(is_rate_limit_response(
            403,
            "You have exceeded a secondary rate limit.",
            false
        ));
        assert!(is_rate_limit_response(
            403,
            "API rate limit exceeded",
            false
        ));
        assert!(
            is_rate_limit_response(403, "{}", true),
            "403 + Retry-After is the documented secondary-limit shape"
        );
        assert!(!is_rate_limit_response(403, "forbidden", false));
        assert!(!is_rate_limit_response(500, "rate limit exceeded", false));
    }

    // ── credential fingerprint ────────────────────────────────────

    #[test]
    fn credential_fingerprint_tracks_token_material() {
        let a = credential_fingerprint("ghp_tokenA");
        let a2 = credential_fingerprint("ghp_tokenA");
        let b = credential_fingerprint("ghp_tokenB");
        assert_eq!(a, a2, "same token → same fingerprint");
        assert_ne!(a, b, "rotated token → different fingerprint");
        assert!(
            !a.contains("ghp_tokenA") && a.len() == 16,
            "fingerprint is a short hash, never the raw secret: {a}"
        );
    }

    /// Like `spawn_canned_response_server`, but serves a SEQUENCE of
    /// bodies — the i-th connection gets `bodies[i]` (the last body
    /// repeats once exhausted). Lets pagination tests hand back
    /// page 1 / page 2 in order.
    async fn spawn_sequenced_response_server(bodies: Vec<&'static str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                let body = bodies[served.min(bodies.len() - 1)];
                served += 1;
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    async fn spawn_repo_routing_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(_) => continue,
                };
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut expected_len = None;
                    loop {
                        let mut chunk = [0u8; 4096];
                        let read = sock.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        if expected_len.is_none()
                            && let Some(header_end) =
                                request.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..header_end]);
                            let content_len = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            expected_len = Some(header_end + 4 + content_len);
                        }
                        if expected_len.is_some_and(|len| request.len() >= len) {
                            break;
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let body = if request.contains("repo:broken/repo") {
                        pr_search_page(1, Some((true, None)))
                    } else if request.contains("repo:healthy/repo") {
                        pr_search_page(2, Some((false, None)))
                    } else {
                        pr_search_page(3, Some((false, None)))
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn make_client(base_uri: &str) -> GhClient {
        // Bypass `from_credential` (which calls `/user`) — we want
        // a `GhClient` that talks to the mock server directly.
        // Octocrab's built-in retry is disabled exactly like the
        // production builder in `from_credential`: with it on, the
        // request-count assertions below would measure octocrab's
        // middleware, not our retry ladder.
        let inner = octocrab::Octocrab::builder()
            .base_uri(base_uri)
            .unwrap()
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .build()
            .unwrap();
        GhClient {
            inner,
            user: "test-user".to_string(),
            credential_source: "test".to_string(),
            credential_fingerprint: credential_fingerprint("test-token"),
            pr_filters: vec![],
            issue_filters: vec![],
            watch_repos: vec![],
            budget: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::rate_budget::RateBudget::default_for_lazybox(),
            )),
            notifications_state: NotificationsState::shared(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hot_tasks_are_fetched_in_one_ordered_batch() {
        const BODY: &str = r#"{
          "data": {
            "nodes": [
              {
                "id": "I_one",
                "number": 7,
                "title": "Fast sync",
                "body": "body",
                "url": "https://github.com/acme/widget/issues/7",
                "updatedAt": "2026-07-25T10:00:00Z",
                "createdAt": "2026-07-24T10:00:00Z",
                "closedAt": null,
                "state": "OPEN",
                "author": {"login": "test-user"},
                "labels": {"nodes": []},
                "assignees": {"nodes": []},
                "comments": {"nodes": []},
                "repository": {"nameWithOwner": "acme/widget"}
              },
              null
            ],
            "rateLimit": {
              "cost": 2,
              "limit": 5000,
              "remaining": 4998,
              "resetAt": "2026-07-25T11:00:00Z"
            }
          }
        }"#;
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_uri =
            spawn_counting_response_server("200 OK", "application/json", "", BODY, hits.clone())
                .await;
        let client = make_client(&base_uri);

        let tasks = client
            .fetch_hot_tasks(&["I_one".into(), "I_gone".into()])
            .await
            .expect("hot batch");

        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(tasks.len(), 2);
        let task = tasks[0].as_ref().expect("visible issue");
        assert_eq!(task.id.key, "acme/widget#7");
        assert_eq!(task.node_id.as_deref(), Some("I_one"));
        assert!(!task.is_pr());
        assert!(tasks[1].is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hot_task_batch_surfaces_transient_graphql_errors() {
        const BODY: &str = r#"{
          "data": {
            "nodes": [null],
            "rateLimit": {
              "cost": 1,
              "limit": 5000,
              "remaining": 4999,
              "resetAt": "2026-07-25T11:00:00Z"
            }
          },
          "errors": [{"type": "INTERNAL", "message": "temporary failure"}]
        }"#;
        let base_uri = spawn_canned_response_server("200 OK", "application/json", BODY).await;
        let client = make_client(&base_uri);

        let error = client
            .fetch_hot_tasks(&["PR_one".into()])
            .await
            .expect_err("transient GraphQL errors must fail the poll");

        assert!(
            matches!(error, GhError::Graphql(message) if message.contains("temporary failure"))
        );
    }

    /// Minimal-but-valid SEARCH_QUERY wire page: one PR node, with the
    /// given pageInfo. Mirrors the trimmed inbox-scan response shape
    /// (see `graphql::search_query_wire_response`).
    fn pr_search_page(number: u64, page_info: Option<(bool, Option<&str>)>) -> String {
        let page_info_json = match page_info {
            Some((has_next, end_cursor)) => {
                let cursor_json = end_cursor
                    .map(|cursor| format!(r#""{cursor}""#))
                    .unwrap_or_else(|| "null".to_string());
                format!(
                    r#""pageInfo": {{ "hasNextPage": {has_next}, "endCursor": {cursor_json} }},"#
                )
            }
            None => String::new(),
        };
        format!(
            r#"{{
              "data": {{
                "search": {{
                  {page_info_json}
                  "nodes": [
                    {{
                      "id": "PR_node{number}",
                      "number": {number},
                      "title": "PR {number}",
                      "body": null,
                      "url": "https://github.com/o/r/pull/{number}",
                      "updatedAt": "2026-05-28T12:00:00Z",
                      "createdAt": "2026-05-27T09:00:00Z",
                      "isDraft": false,
                      "state": "OPEN",
                      "merged": false,
                      "headRefName": "feat-{number}",
                      "baseRefName": "main",
                      "mergeable": "MERGEABLE",
                      "reviewDecision": null,
                      "autoMergeRequest": null,
                      "isInMergeQueue": false,
                      "author": {{ "login": "carol" }},
                      "commits": {{ "nodes": [] }},
                      "labels": {{ "nodes": [] }},
                      "assignees": {{ "nodes": [] }},
                      "reviewRequests": {{ "nodes": [] }},
                      "comments": {{ "totalCount": 0, "nodes": [] }}
                    }}
                  ]
                }},
                "rateLimit": {{ "cost": 1, "limit": 5000, "remaining": 4999, "resetAt": "2026-05-28T13:00:00Z" }}
              }},
              "errors": null
            }}"#
        )
    }

    /// Data-loss regression: the companion searches (merged-sweep,
    /// watched-repo, review-requested, round-robin) used a single
    /// 25-result page and reported authoritative coverage anyway, so
    /// every PR past the first page was rescope-deleted. The query
    /// must now follow `pageInfo.endCursor` until exhaustion.
    #[tokio::test(flavor = "current_thread")]
    async fn single_query_follows_pagination_cursor() {
        let page1: &'static str =
            Box::leak(pr_search_page(1, Some((true, Some("CUR1")))).into_boxed_str());
        let page2: &'static str =
            Box::leak(pr_search_page(2, Some((false, None))).into_boxed_str());
        let base_uri = spawn_sequenced_response_server(vec![page1, page2]).await;
        let client = make_client(&base_uri);

        let tasks = client
            .fetch_pr_single_query("test-branch", "is:open is:pr repo:o/r".to_string())
            .await
            .expect("paginated single query should succeed");

        let keys: Vec<&str> = tasks.iter().map(|t| t.id.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["o/r#1", "o/r#2"],
            "both pages' PRs must be returned — page 2 was previously dropped"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_page_info_fails_instead_of_returning_authoritative_prefix() {
        let page: &'static str = Box::leak(pr_search_page(1, None).into_boxed_str());
        let base_uri = spawn_canned_response_server("200 OK", "application/json", page).await;
        let client = make_client(&base_uri);

        let error = client
            .fetch_pr_single_query("test-branch", "is:open is:pr repo:o/r".to_string())
            .await
            .expect_err("a response without pageInfo is not complete coverage");

        assert!(matches!(error, GhError::Graphql(message) if message.contains("omitted pageInfo")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn partial_repo_fanout_reports_non_authoritative_pr_coverage() {
        let base_uri = spawn_repo_routing_server().await;
        let client = make_client(&base_uri);

        let outcome = client
            .fetch_round_robin_with_status_and_mentions(
                true,
                &["broken/repo".into(), "healthy/repo".into()],
                false,
                false,
                &std::collections::BTreeSet::new(),
                None,
            )
            .await
            .expect("the healthy repo remains usable");

        assert!(
            outcome.tasks.iter().any(|task| task.id.key == "o/r#2"),
            "tasks from the successful repo are preserved"
        );
        assert!(!outcome.coverage.pr_complete());
        assert!(!outcome.coverage.sweep_complete());
        assert!(
            outcome
                .partial_failure
                .as_deref()
                .is_some_and(|warning| warning.contains("broken/repo")),
            "the failed repo is named in the degraded-sync warning"
        );
    }

    /// Regression test for issue #180: a manual `Command::Refresh`
    /// arms `force_full_sweep`, which must promote the very next tick to
    /// a full sweep even though a sweep just completed (the timer alone
    /// would route to the incremental path and miss a just-created
    /// issue). Completing the sweep clears the one-shot flag.
    #[tokio::test(flavor = "current_thread")]
    async fn force_full_sweep_promotes_next_tick_then_clears() {
        let client = make_client("http://127.0.0.1:1");
        // Simulate a sweep that just finished — timer alone says
        // incremental is fine.
        client.mark_full_sweep_done();
        assert!(
            !client.should_full_sweep(),
            "a sweep that just ran must not be due again on the timer"
        );
        // Manual refresh arms the override.
        client.force_full_sweep();
        assert!(
            client.should_full_sweep(),
            "force_full_sweep must promote the next tick regardless of the timer"
        );
        // Running the sweep consumes the one-shot flag.
        client.mark_full_sweep_done();
        assert!(
            !client.should_full_sweep(),
            "completing the forced sweep must clear the flag"
        );
    }

    /// Issue #14: the `updated:>=` window state machine. The first
    /// global sweep reconciles (no window); once a reconcile is on
    /// record, subsequent sweeps narrow to the previous sweep's start;
    /// a manual refresh forces a reconcile again.
    #[tokio::test(flavor = "current_thread")]
    async fn pr_sweep_window_alternates_reconcile_and_incremental() {
        let client = make_client("http://127.0.0.1:1");
        // Bootstrap: nothing reconciled yet → reconcile (no window).
        assert!(
            client.next_pr_sweep_window().is_none(),
            "first global sweep must reconcile the whole inbox"
        );

        // A reconcile completes, stamping its start time as the floor.
        let t0 = chrono::Utc::now() - chrono::Duration::minutes(10);
        client.record_pr_sweep_window(t0, true);
        // Reconcile timer is fresh (interval is an hour) → next sweep
        // narrows to the recorded floor.
        assert_eq!(
            client.next_pr_sweep_window(),
            Some(t0),
            "with a recent reconcile, the next sweep windows on its floor"
        );

        // A windowed sweep advances the floor but does NOT re-arm the
        // reconcile timer, so the sweep after it still windows.
        let t1 = chrono::Utc::now();
        client.record_pr_sweep_window(t1, false);
        assert_eq!(client.next_pr_sweep_window(), Some(t1));

        // Manual refresh forces a reconcile regardless of the timer.
        client.force_full_sweep();
        assert!(
            client.next_pr_sweep_window().is_none(),
            "a forced refresh must reconcile, not window"
        );
        // Completing the sweep clears the one-shot force flag, so we
        // fall back to windowing on the freshly-recorded floor.
        client.mark_full_sweep_done();
        let t2 = chrono::Utc::now();
        client.record_pr_sweep_window(t2, false);
        assert_eq!(client.next_pr_sweep_window(), Some(t2));
    }

    /// Issue #530: the merged sweep tracks its OWN success floor,
    /// independent of the main `involves:` floor, so a main-branch
    /// success can't window the merged sweep past a merge that the
    /// best-effort merged branch failed to observe.
    #[tokio::test(flavor = "current_thread")]
    async fn merged_sweep_window_is_independent_of_the_main_floor() {
        let client = make_client("http://127.0.0.1:1");
        // Cold start: no merged sweep on record → unwindowed (full 7-day).
        assert!(
            client.merged_sweep_window().is_none(),
            "first merged sweep must run unwindowed"
        );

        // Advancing the main `involves:` floor must NOT move the merged
        // floor — that's the whole point of tracking them separately.
        client.record_pr_sweep_window(chrono::Utc::now(), false);
        assert!(
            client.merged_sweep_window().is_none(),
            "the main floor advancing must leave the merged floor unwindowed"
        );

        // Only a successful merged sweep advances its own floor.
        let merged = chrono::Utc::now();
        client.record_merged_sweep_window(merged);
        assert_eq!(client.merged_sweep_window(), Some(merged));
    }

    /// Regression test for issue #13: GitHub returns a 502 HTML
    /// maintenance page; we used to surface the opaque
    /// "Serde Error: expected value at line 1 column 1". Now the
    /// error includes the actual HTTP status and the canonical reason.
    #[tokio::test(flavor = "current_thread")]
    async fn http_502_html_surfaces_actual_status() {
        let base_uri = spawn_canned_response_server(
            "502 Bad Gateway",
            "text/html",
            "<html><body><h1>502 Bad Gateway</h1></body></html>",
        )
        .await;
        let client = make_client(&base_uri);
        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<serde_json::Value>(&body)
            .await
            .expect_err("502 response should produce an error");
        let msg = err.to_string();
        assert!(msg.contains("502"), "expected '502' in error: {msg}");
        assert!(
            msg.contains("Bad Gateway"),
            "expected canonical reason 'Bad Gateway' in error: {msg}"
        );
        assert!(
            !msg.contains("Serde Error"),
            "must NOT regress to the opaque serde wording: {msg}"
        );
        assert!(
            matches!(err, GhError::HttpStatus { status: 502, .. }),
            "expected HttpStatus variant, got: {err:?}"
        );
    }

    /// 401 should surface as `HttpStatus { status: 401, .. }` and
    /// classify as `ProviderError::Auth` via the `From` impl —
    /// previously we relied on `octocrab::Error::GitHub`, which the
    /// raw path doesn't produce.
    #[tokio::test(flavor = "current_thread")]
    async fn http_401_classifies_as_auth() {
        let base_uri = spawn_canned_response_server(
            "401 Unauthorized",
            "application/json",
            r#"{"message":"Bad credentials"}"#,
        )
        .await;
        let client = make_client(&base_uri);
        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<serde_json::Value>(&body)
            .await
            .expect_err("401 should produce an error");
        assert!(
            matches!(err, GhError::HttpStatus { status: 401, .. }),
            "expected HttpStatus(401), got: {err:?}"
        );
        let pe: lazybox_core::ProviderError = err.into();
        assert!(
            matches!(pe, lazybox_core::ProviderError::Auth { .. }),
            "401 should map to ProviderError::Auth, got: {pe:?}"
        );
    }

    /// A 2xx response whose body fails JSON parsing surfaces with
    /// the status + content-type intact, plus the parse-failure
    /// detail in the body excerpt.
    #[tokio::test(flavor = "current_thread")]
    async fn http_200_with_invalid_json_surfaces_parse_failure() {
        let base_uri =
            spawn_canned_response_server("200 OK", "application/json", "not actually json").await;
        let client = make_client(&base_uri);
        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<serde_json::Value>(&body)
            .await
            .expect_err("unparseable 2xx body should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("json parse failed"),
            "expected parse-failure marker in error: {msg}"
        );
        assert!(
            matches!(err, GhError::HttpStatus { status: 200, .. }),
            "expected HttpStatus(200), got: {err:?}"
        );
    }

    /// The measured GraphQL helper must report the exact byte length
    /// of the response body it deserialized — that count feeds the
    /// per-branch `resp_bytes` metric used to profile sync waste.
    #[tokio::test(flavor = "current_thread")]
    async fn measured_helper_reports_response_byte_length() {
        const BODY: &str = r#"{"data":{"hello":"world"},"errors":null}"#;
        let base_uri = spawn_canned_response_server("200 OK", "application/json", BODY).await;
        let client = make_client(&base_uri);
        let body = serde_json::json!({"query": "{}"});
        let (value, bytes) = client
            .post_graphql_with_retry_measured::<serde_json::Value>(&body)
            .await
            .expect("canned 2xx JSON should parse");
        assert_eq!(
            bytes,
            BODY.len(),
            "reported byte length must equal the raw response body length"
        );
        assert_eq!(value["data"]["hello"], "world");
    }

    /// Issue #305: the real `mergePullRequest` success reply has no
    /// `search` field, so deserializing it as the search-shaped
    /// `GqlResponse` failed and reported a false error that leaked the
    /// raw response JSON into the footer. The dedicated
    /// `GqlMutationResponse` must parse it cleanly and return `Ok`.
    #[tokio::test(flavor = "current_thread")]
    async fn merge_success_json_reports_ok() {
        const METHOD: &str =
            r#"{"data":{"node":{"repository":{"viewerDefaultMergeMethod":"MERGE"}}}}"#;
        const BODY: &str = r#"{"data":{"mergePullRequest":{"pullRequest":{"id":"PR_kwDO","state":"MERGED","merged":true}}}}"#;
        let base_uri = spawn_sequenced_response_server(vec![METHOD, BODY]).await;
        let client = make_client(&base_uri);
        client
            .merge_pr("PR_kwDO", None)
            .await
            .expect("merge success must not report a false failure");
    }

    /// Issue #469: a repo that disallows merge commits reports SQUASH
    /// (or REBASE) as its `viewerDefaultMergeMethod`. `merge_pr` must
    /// pin that method on the mutation — omitting it makes GitHub
    /// default to MERGE, which such repos reject outright.
    #[tokio::test(flavor = "current_thread")]
    async fn merge_pins_repo_default_method() {
        assert_eq!(
            graphql::merge_pr_body("PR_kwDO", "SQUASH", None)["variables"]["method"],
            "SQUASH",
            "the resolved default method must ride the merge mutation"
        );
        const METHOD: &str =
            r#"{"data":{"node":{"repository":{"viewerDefaultMergeMethod":"SQUASH"}}}}"#;
        const BODY: &str = r#"{"data":{"mergePullRequest":{"pullRequest":{"id":"PR_kwDO","state":"MERGED","merged":true}}}}"#;
        let base_uri = spawn_sequenced_response_server(vec![METHOD, BODY]).await;
        let client = make_client(&base_uri);
        client
            .merge_pr("PR_kwDO", None)
            .await
            .expect("merging a squash-only repo must succeed");
    }

    /// `updatePullRequestBranch` success reply is mutation-shaped (no
    /// `search` field), like `merge_pr` — it must parse cleanly and
    /// return `Ok` rather than leak the raw body as a false error.
    #[tokio::test(flavor = "current_thread")]
    async fn update_branch_success_reports_ok() {
        const BODY: &str =
            r#"{"data":{"updatePullRequestBranch":{"pullRequest":{"id":"PR_kwDO"}}}}"#;
        let base_uri = spawn_canned_response_server("200 OK", "application/json", BODY).await;
        let client = make_client(&base_uri);
        client
            .update_branch("PR_kwDO")
            .await
            .expect("update-branch success must not report a false failure");
    }

    /// A branch already up to date comes back as a GraphQL error that
    /// matches the idempotence markers — the caller asked for "up to
    /// date," which it is, so this must resolve to `Ok`.
    #[tokio::test(flavor = "current_thread")]
    async fn update_branch_already_up_to_date_is_ok() {
        const BODY: &str =
            r#"{"data":null,"errors":[{"message":"No new commits on the base branch."}]}"#;
        let base_uri = spawn_canned_response_server("200 OK", "application/json", BODY).await;
        let client = make_client(&base_uri);
        client
            .update_branch("PR_kwDO")
            .await
            .expect("an already-updated branch is success, not failure");
    }

    /// Same class of bug for the label mutation — its `data` node is
    /// `addLabelsToLabelable`, also without a `search` field.
    #[tokio::test(flavor = "current_thread")]
    async fn label_mutation_success_reports_ok() {
        const BODY: &str =
            r#"{"data":{"addLabelsToLabelable":{"labelable":{"__typename":"PullRequest"}}}}"#;
        let base_uri = spawn_canned_response_server("200 OK", "application/json", BODY).await;
        let client = make_client(&base_uri);
        client
            .add_labels("PR_kwDO", &["LA_1".to_string()])
            .await
            .expect("label mutation success must return Ok");
    }

    /// Reviewer mutation: `request_reviewers` first resolves the login
    /// to a node id (query), then fires `requestReviews` (mutation).
    /// Both replies are non-search-shaped; neither may misreport.
    #[tokio::test(flavor = "current_thread")]
    async fn reviewer_mutation_success_reports_ok() {
        const USER_ID: &str = r#"{"data":{"user":{"id":"U_1"}}}"#;
        const MUTATION: &str = r#"{"data":{"requestReviews":{"pullRequest":{"id":"PR_kwDO"}}}}"#;
        let base_uri = spawn_sequenced_response_server(vec![USER_ID, MUTATION]).await;
        let client = make_client(&base_uri);
        client
            .request_reviewers("PR_kwDO", &["alice".to_string()])
            .await
            .expect("reviewer mutation success must return Ok");
    }

    /// Assignee mutation: same lookup-then-mutate shape as reviewers.
    #[tokio::test(flavor = "current_thread")]
    async fn assignee_mutation_success_reports_ok() {
        const USER_ID: &str = r#"{"data":{"user":{"id":"U_1"}}}"#;
        const MUTATION: &str =
            r#"{"data":{"addAssigneesToAssignable":{"assignable":{"__typename":"PullRequest"}}}}"#;
        let base_uri = spawn_sequenced_response_server(vec![USER_ID, MUTATION]).await;
        let client = make_client(&base_uri);
        client
            .add_assignees("PR_kwDO", &["alice".to_string()])
            .await
            .expect("assignee mutation success must return Ok");
    }

    /// A genuine mutation failure surfaces the joined `GqlError`
    /// messages — a clean reason, never the raw JSON body.
    #[tokio::test(flavor = "current_thread")]
    async fn mutation_graphql_error_surfaces_clean_message() {
        const METHOD: &str =
            r#"{"data":{"node":{"repository":{"viewerDefaultMergeMethod":"MERGE"}}}}"#;
        const BODY: &str =
            r#"{"data":null,"errors":[{"message":"Pull request is not mergeable"}]}"#;
        let base_uri = spawn_sequenced_response_server(vec![METHOD, BODY]).await;
        let client = make_client(&base_uri);
        let err = client
            .merge_pr("PR_kwDO", None)
            .await
            .expect_err("a real GraphQL error must still surface as an error");
        let msg = err.to_string();
        assert!(
            msg.contains("Pull request is not mergeable"),
            "expected the GraphQL error message, got: {msg}"
        );
        assert!(
            !msg.contains('{'),
            "error message must not leak raw JSON: {msg}"
        );
    }

    /// Defense-in-depth (issue #305): even when a *typed* response
    /// fails to deserialize, the user-facing error must never carry the
    /// raw response body — that belongs in `tracing` only.
    #[tokio::test(flavor = "current_thread")]
    async fn parse_failure_notice_never_leaks_raw_json_body() {
        // Valid JSON, but the wrong shape for the search-typed parse —
        // exactly the merge reply that triggered the original leak.
        const BODY: &str =
            r#"{"data":{"mergePullRequest":{"pullRequest":{"state":"MERGED","merged":true}}}}"#;
        let base_uri = spawn_canned_response_server("200 OK", "application/json", BODY).await;
        let client = make_client(&base_uri);
        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<graphql::GqlResponse>(&body)
            .await
            .expect_err("wrong-shaped 2xx body should fail to parse");
        let msg = err.to_string();
        assert!(
            !msg.contains("mergePullRequest") && !msg.contains("MERGED"),
            "parse-failure notice must not echo the raw response body: {msg}"
        );
    }

    /// Spin up a `/notifications`-style conditional-GET server: a request
    /// WITHOUT a matching `If-Modified-Since` gets a `200` carrying
    /// `Last-Modified: <last_modified>` and `body`; a request whose
    /// `If-Modified-Since` echoes that exact value gets a `304`. Lets the
    /// cursor-lifecycle test drive the 200 → 304 transition without
    /// pulling in `wiremock`.
    async fn spawn_conditional_notifications_server(
        last_modified: &'static str,
        body: &'static str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                tokio::spawn(async move {
                    // Read until the blank line that ends the request
                    // headers (these are bodyless GETs). A single `read`
                    // could return before the `If-Modified-Since` header
                    // arrives if the request spans TCP segments — rare on
                    // localhost, but that would flake the 304 assertion.
                    let mut data = Vec::new();
                    let mut buf = [0u8; 8192];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => data.extend_from_slice(&buf[..n]),
                        }
                        if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 65536 {
                            break;
                        }
                    }
                    let req = String::from_utf8_lossy(&data);
                    // First colon splits header name from value; the time
                    // colons in the date stay in the value half.
                    let ims_matches =
                        req.lines()
                            .filter_map(|l| l.split_once(':'))
                            .any(|(name, val)| {
                                name.trim().eq_ignore_ascii_case("if-modified-since")
                                    && val.trim() == last_modified
                            });
                    let response = if ims_matches {
                        "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_string()
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Last-Modified: {last_modified}\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len(),
                        )
                    };
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// #512 regression: parsing the notification LIST must NOT advance
    /// the `Last-Modified` cursor. The bug was that the cursor committed
    /// the instant the list parsed — before the per-item deep-fetches
    /// ran — so an entry whose deep-fetch failed this tick was skipped
    /// forever (the next heartbeat answered 304 and never re-listed it).
    /// The cursor must only advance through an explicit
    /// `commit_notifications_cursor`, which the polling layer calls after
    /// the fan-out reports every entry handled.
    #[tokio::test(flavor = "current_thread")]
    async fn notifications_list_does_not_advance_cursor_until_committed() {
        const LAST_MODIFIED: &str = "Sun, 06 Nov 2026 08:49:37 GMT";
        const BODY: &str = r#"[{
            "reason": "ci_activity",
            "updated_at": "2026-05-28T12:00:00Z",
            "subject": {
                "title": "PR 123",
                "url": "https://api.github.com/repos/o/r/pulls/123",
                "type": "PullRequest"
            },
            "repository": { "full_name": "o/r" }
        }]"#;
        let base_uri = spawn_conditional_notifications_server(LAST_MODIFIED, BODY).await;
        let client = make_client(&base_uri);

        // Tick 1: a fresh 200 lists PR #123 and hands the pending cursor
        // BACK to the caller instead of committing it.
        let poll = client.fetch_notifications().await.expect("first poll ok");
        let pending = match poll {
            NotificationsPoll::Modified {
                entries,
                last_modified,
            } => {
                assert_eq!(entries.len(), 1, "the one PR notification must be listed");
                assert_eq!(
                    last_modified.as_deref(),
                    Some(LAST_MODIFIED),
                    "the pending cursor rides the poll result, not the shared state",
                );
                last_modified
            }
            NotificationsPoll::NotModified => panic!("first poll must be 200, not 304"),
        };
        assert!(
            !client.notifications_snapshot().has_last_modified,
            "listing alone must NOT commit the cursor (#512)",
        );

        // Tick 2 WITHOUT committing — this is the un-fetched entry's
        // retry. The heartbeat still sends no `If-Modified-Since`, so
        // GitHub re-serves the 200 and PR #123 re-lists rather than being
        // lost to a premature 304.
        let poll2 = client.fetch_notifications().await.expect("second poll ok");
        assert!(
            matches!(poll2, NotificationsPoll::Modified { ref entries, .. } if entries.len() == 1),
            "an un-committed cursor must re-list the entry next tick, not 304 it away",
        );

        // Commit (the fan-out reported every entry handled) → the cursor
        // finally advances.
        client.commit_notifications_cursor(pending);
        assert!(
            client.notifications_snapshot().has_last_modified,
            "commit_notifications_cursor advances the cursor",
        );

        // Tick 3: the committed cursor is echoed as `If-Modified-Since`,
        // so the server answers the cheap steady-state 304.
        let poll3 = client.fetch_notifications().await.expect("third poll ok");
        assert!(
            matches!(poll3, NotificationsPoll::NotModified),
            "a committed cursor reaches the 304 steady state",
        );
    }
}

#[cfg(test)]
mod backtrace_strip_tests {
    use super::strip_error_backtrace;

    /// Plain one-line message passes through unchanged.
    #[test]
    fn passes_short_message_through() {
        assert_eq!(
            strip_error_backtrace("expected value at line 1 column 1"),
            "expected value at line 1 column 1"
        );
    }

    /// snafu's "Found at 0: ..." prelude marks the backtrace —
    /// cut everything from there. This is the actual format octocrab
    /// uses; the user's footer was dumping the rest verbatim.
    #[test]
    fn cuts_snafu_found_at_prelude() {
        let raw = "Serde Error: expected value at line 1 column 1\n\
                   Found at 0: std::backtrace_rs::backtrace::libunwind::trace\n\
                              at /rustc/59807616e1fa2540724bfbac14d7c0d/library/std/src/backtrace_rs.rs:66";
        assert_eq!(
            strip_error_backtrace(raw),
            "Serde Error: expected value at line 1 column 1"
        );
    }

    /// Some snafu versions / wrappers use "Caused by:" instead.
    #[test]
    fn cuts_caused_by_prelude() {
        let raw = "outer error\nCaused by: deeper error\nfurther frame";
        assert_eq!(strip_error_backtrace(raw), "outer error");
    }

    /// "Found at" run together with the message (no newline) — still
    /// cut. The user's screenshot showed "...column 1Found at 0:..."
    /// where the newline didn't render because the footer is a
    /// single line.
    #[test]
    fn cuts_inline_found_at_marker() {
        let raw = "expected value at line 1 column 1Found at 0: trace";
        assert_eq!(
            strip_error_backtrace(raw),
            "expected value at line 1 column 1"
        );
    }

    /// Trailing whitespace gets trimmed — keeps the footer flush.
    #[test]
    fn trims_trailing_whitespace() {
        assert_eq!(strip_error_backtrace("oh no   "), "oh no");
    }
}
