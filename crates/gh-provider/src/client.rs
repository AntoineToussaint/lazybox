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
        /// `true` when lazybox's OWN governor blocked the request (local
        /// bucket / per-tick background allowance / soft reserve) rather
        /// than GitHub imposing a real rate limit. Carried so the
        /// `From<GhError>` mapping can flag the resulting
        /// [`lazybox_core::ProviderError`] as a self-throttle — an honest,
        /// non-escalating backoff, never "check your token" (#782).
        self_throttle: bool,
    },
    /// A whole user-facing GitHub operation blew its overall wall-clock
    /// deadline — the *sum* of governor pacing, the concurrency-slot
    /// wait, every network round-trip, and retry backoff. Each piece is
    /// individually bounded (the per-request timeout, the permit-wait
    /// cap), but nothing capped their sum, so a starved governor (#782)
    /// could stall an op for minutes behind a spinner (#825). Retryable:
    /// the next attempt may find the backlog cleared.
    #[error("{operation} timed out after {after_secs}s")]
    Timeout {
        operation: &'static str,
        after_secs: u64,
    },
}

impl GhError {
    fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::RateLimited {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            _ => None,
        }
    }

    fn aggregate(reason: String, errors: &[&Self]) -> Self {
        let retry_after_secs = errors
            .iter()
            .map(|error| error.retry_after_secs())
            .collect::<Option<Vec<_>>>()
            .and_then(|delays| delays.into_iter().max());
        match retry_after_secs {
            Some(retry_after_secs) => Self::RateLimited {
                retry_after_secs,
                reason,
                // An aggregate of failed sub-requests is a GitHub/transport
                // failure, not the governor's own pacing.
                self_throttle: false,
            },
            None => Self::Graphql(reason),
        }
    }
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RateLimitHeaders {
    /// `Retry-After` in seconds (GitHub only uses the delta-seconds
    /// form, never the HTTP-date form, per its rate-limit docs).
    retry_after_secs: Option<u64>,
    /// `x-ratelimit-remaining`.
    remaining: Option<u32>,
    /// `x-ratelimit-reset` — epoch seconds when the window reopens.
    reset_epoch_secs: Option<u64>,
    /// `x-ratelimit-resource` (`core`, `search`, `graphql`, ...).
    resource: Option<String>,
    limit: Option<u32>,
    used: Option<u32>,
}

impl RateLimitHeaders {
    fn parse(
        retry_after: Option<&str>,
        remaining: Option<&str>,
        reset: Option<&str>,
        resource: Option<&str>,
        limit: Option<&str>,
        used: Option<&str>,
    ) -> Self {
        Self {
            retry_after_secs: retry_after.and_then(|v| v.trim().parse().ok()),
            remaining: remaining.and_then(|v| v.trim().parse().ok()),
            reset_epoch_secs: reset.and_then(|v| v.trim().parse().ok()),
            resource: resource.map(str::to_string),
            limit: limit.and_then(|v| v.trim().parse().ok()),
            used: used.and_then(|v| v.trim().parse().ok()),
        }
    }

    /// Remote GraphQL budget derived from the `x-ratelimit-*` response
    /// headers GitHub attaches to every GraphQL call, mutations included.
    /// Mutations cannot select the GraphQL `rateLimit` field — it lives on
    /// the `Query` root, not `Mutation` (issue #822) — so their budget
    /// refresh comes from these headers instead of the response body.
    /// Returns `None` unless the headers bill the `graphql` resource and
    /// carry a full remaining/limit/reset triple, alongside the reported
    /// cumulative `used` count.
    fn graphql_budget(&self) -> Option<(crate::rate_budget::RemoteRateLimit, u32)> {
        if self.resource.as_deref() != Some("graphql") {
            return None;
        }
        let reset_at =
            chrono::DateTime::from_timestamp(i64::try_from(self.reset_epoch_secs?).ok()?, 0)?;
        Some((
            crate::rate_budget::RemoteRateLimit {
                remaining: self.remaining?,
                limit: self.limit?,
                reset_at,
                observed_at: std::time::Instant::now(),
            },
            self.used.unwrap_or(0),
        ))
    }

    /// Seconds to wait before the next request, preferring the
    /// explicit `Retry-After`, falling back to the reset timestamp,
    /// then to a conservative 60s default. Clamped to >= 1 so a
    /// clock-skewed reset in the past never produces a hot loop.
    #[cfg(test)]
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

/// Does this GraphQL request body carry a mutation? Mutations take the
/// shared serial lane so GitHub never sees two concurrent writes on the
/// token.
fn is_graphql_mutation(body: &serde_json::Value) -> bool {
    body.get("query")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|query| query.trim_start().starts_with("mutation"))
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

fn incomplete_pagination_error(
    operation: &str,
    count: usize,
    reason: PaginationStop<GhError>,
) -> GhError {
    match reason {
        PaginationStop::PageError(error) => error,
        PaginationStop::MissingPageInfo => {
            GhError::Graphql(format!("{operation}: response omitted pageInfo"))
        }
        PaginationStop::MissingEndCursor => {
            GhError::Graphql(format!("{operation}: hasNextPage=true but endCursor=null"))
        }
        PaginationStop::PageLimit { pages } => GhError::Truncated { count, pages },
    }
}

#[derive(Debug)]
pub struct SelectedFetchOutcome {
    pub tasks: Vec<Task>,
    pub partial_failure: Option<String>,
    pub retry_after_secs: Option<u64>,
    pub mentions: Vec<crate::LazyboxMention>,
    pub coverage: FetchCoverage,
    pub pr_coverage: FetchCoverage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundSweepForecast {
    pub global_points: u32,
    pub repo_base_points: u32,
    pub per_repo_points: u32,
}

impl BackgroundSweepForecast {
    pub fn required_points(self, run_global: bool, want_prs: bool) -> u32 {
        if run_global {
            self.global_points
        } else if want_prs {
            self.repo_base_points.saturating_add(self.per_repo_points)
        } else {
            self.repo_base_points
        }
    }

    pub fn repo_capacity(self, allowance: u32, run_global: bool, limit: usize) -> usize {
        if run_global || self.per_repo_points == 0 {
            return 0;
        }
        (allowance.saturating_sub(self.repo_base_points) / self.per_repo_points)
            .try_into()
            .unwrap_or(usize::MAX)
            .min(limit)
    }
}

#[derive(Debug)]
struct PrFetchOutcome {
    tasks: Vec<Task>,
    partial_failure: Option<String>,
    retry_after_secs: Option<u64>,
}

impl PrFetchOutcome {
    fn complete(tasks: Vec<Task>) -> Self {
        Self {
            tasks,
            partial_failure: None,
            retry_after_secs: None,
        }
    }

    fn partial(tasks: Vec<Task>, partial_failure: String, retry_after_secs: Option<u64>) -> Self {
        Self {
            tasks,
            partial_failure: Some(partial_failure),
            retry_after_secs,
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
                retry_after_secs: prs.retry_after_secs,
                mentions,
                coverage: if pr_complete {
                    FetchCoverage::Complete
                } else {
                    FetchCoverage::Partial
                },
                pr_coverage: if pr_complete {
                    FetchCoverage::Complete
                } else {
                    FetchCoverage::Partial
                },
            })
        }
        (Ok(prs), Err(error)) => {
            if !pr_side_requested {
                return Err(error);
            }
            let retry_after_secs = prs.retry_after_secs.max(error.retry_after_secs());
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
                retry_after_secs,
                mentions: Vec::new(),
                coverage: FetchCoverage::Partial,
                pr_coverage: if pr_complete {
                    FetchCoverage::Complete
                } else {
                    FetchCoverage::Partial
                },
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
                retry_after_secs: error.retry_after_secs(),
                mentions,
                coverage: FetchCoverage::Partial,
                pr_coverage: FetchCoverage::Partial,
            })
        }
        (Err(pr_error), Err(issue_error)) => {
            let reason =
                format!("both PR and issue fetches failed: PRs={pr_error}; issues={issue_error}");
            Err(GhError::aggregate(reason, &[&pr_error, &issue_error]))
        }
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
            ..
        } => format!("{reason} (retry after {retry_after_secs}s)"),
        GhError::Truncated { count, pages } => format!(
            "GitHub returned {count} PRs across {pages} pages and we hit the safety cap. \
             Your filter likely matches too many PRs — narrow it in Settings."
        ),
        GhError::WatchAllFailed { count } => {
            format!("all {count} configured watched-repo queries failed (network or auth issue)")
        }
        GhError::HttpStatus { .. } => format!("{err}"),
        GhError::Timeout { .. } => format!("{err}"),
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
            retry_after_secs,
            self_throttle,
            ..
        } = &err
        {
            // A governor self-throttle (local bucket / background allowance /
            // soft reserve) is lazybox deliberately pacing its own sync
            // under shared-token contention — honest backoff, not a fault,
            // so flag it so polling never escalates it to "check your token"
            // (#782). A real GitHub rate limit stays a plain retryable.
            if *self_throttle {
                return lazybox_core::ProviderError::self_throttle(
                    SOURCE,
                    detail,
                    *retry_after_secs,
                );
            }
            return lazybox_core::ProviderError::retryable_after(SOURCE, detail, *retry_after_secs);
        }

        // A blown operation deadline is transient by construction — the
        // backlog that starved it may have cleared by the next attempt —
        // so it retries on the poll's normal cadence, never escalates to
        // an auth/permanent verdict (#825).
        if matches!(err, GhError::Timeout { .. }) {
            return lazybox_core::ProviderError::retryable(SOURCE, detail);
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

/// GraphQL mutations that trip GitHub's secondary rate limit carry no
/// `resetAt` in their (HTTP 200) error body — the reset window only
/// rides the header path. Fall back to a modest wait the daemon's
/// mutation-retry queue then backs off from across attempts.
const MUTATION_RATE_LIMIT_DEFAULT_WAIT_SECS: u64 = 60;

/// Overall wall-clock ceiling on a single user-facing GraphQL operation
/// (sync/poll, merge, update-branch, reviewers/assignees/labels, …). It
/// bounds the *sum* the per-request timeout can't: governor pacing, the
/// concurrency-slot acquire, every network round-trip, and retry
/// backoff. When it trips the op aborts with a clean
/// [`GhError::Timeout`] instead of an open-ended spinner (#825). A single
/// healthy attempt — `PERMIT_WAIT_TIMEOUT` + one 25s request — fits
/// comfortably inside it.
const GRAPHQL_OPERATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(90);

/// Max wall-clock the self-imposed governor waits — request pacing plus
/// the concurrency-slot acquire — may consume before we give up and fail
/// fast. Waiting on our OWN governor for minutes is worse than a clean
/// "GitHub is busy, retry later": under governor self-starvation (#782) an
/// uncapped pacing sleep was the mechanism behind a 307s frozen spinner
/// (#825). Kept well under [`GRAPHQL_OPERATION_DEADLINE`] so a starved
/// permit surfaces as its own clear error before the whole-operation
/// backstop trips.
const PERMIT_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Turn a mutation response's `errors` array into a typed [`GhError`].
///
/// A rate-limit error is lifted to [`GhError::RateLimited`] (so the daemon
/// queues + retries against the reset window instead of hard-failing);
/// everything else joins the **human** messages — never `full()`'s
/// path/extensions debug text, which is what dumped the raw
/// `{…,"typeName":"Mutation"}` blob into the footer. The `full()` text
/// still goes to the log at each call site for diagnosis.
fn mutation_errors_to_gherror(operation: &str, errors: &[graphql::GqlError]) -> GhError {
    let joined = errors
        .iter()
        .map(graphql::GqlError::human)
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if errors.iter().any(graphql::GqlError::is_rate_limited) {
        let reason = if joined.is_empty() {
            format!("{operation}: GitHub secondary rate limit")
        } else {
            joined
        };
        return GhError::RateLimited {
            retry_after_secs: MUTATION_RATE_LIMIT_DEFAULT_WAIT_SECS,
            reason,
            self_throttle: false,
        };
    }
    // A GitHub error with no message text at all (`human()` empty) would
    // otherwise render as an empty footer reason ("merge failed: github:").
    // Name the operation so the failure is never blank.
    if joined.is_empty() {
        return GhError::Graphql(format!(
            "{operation} failed (GitHub returned an error with no message)"
        ));
    }
    GhError::Graphql(joined)
}

/// Log the full (diagnostic) GraphQL error text for `operation`, then
/// return the humanized / rate-limit-classified error for the caller to
/// surface. Centralizes the mutation error tail so no call site dumps
/// `full()` (path + extensions) at the user.
fn mutation_error_response(operation: &str, errors: &[graphql::GqlError]) -> GhError {
    let full = errors
        .iter()
        .map(graphql::GqlError::full)
        .collect::<Vec<_>>()
        .join("; ");
    let classified = mutation_errors_to_gherror(operation, errors);
    // A rate limit is a handled, transient condition the daemon queues +
    // retries — log it as a warning, not an error, so a merge that later
    // succeeds doesn't leave an ERROR line behind. A genuine rejection stays
    // an error.
    if matches!(classified, GhError::RateLimited { .. }) {
        tracing::warn!("{operation} rate-limited (queued for retry): {full}");
    } else {
        tracing::error!("{operation} errors: {full}");
    }
    classified
}

/// Map a mutation's [`GhError`] to a [`ProviderError`] preserving the
/// rate-limit classification: a secondary-rate-limited write becomes a
/// `Retryable` carrying the reset hint (the daemon queues + retries it),
/// while every genuine rejection (conflict, blocked checks, permission)
/// stays `Permanent` — user intent against the state they saw, GitHub's
/// own rejection as the backstop, surfaced verbatim.
fn mutation_provider_error(err: GhError) -> lazybox_core::ProviderError {
    match &err {
        GhError::RateLimited {
            retry_after_secs,
            self_throttle,
            ..
        } => {
            let detail = detail_of(&err);
            if *self_throttle {
                lazybox_core::ProviderError::self_throttle("github", detail, *retry_after_secs)
            } else {
                lazybox_core::ProviderError::retryable_after("github", detail, *retry_after_secs)
            }
        }
        // A mutation that blew the overall deadline may well have landed
        // on GitHub's side (request sent, response lost) — the same
        // idempotent-resend case `merge_pr` / `update_branch` already
        // guard. Retryable so the daemon's mutation queue re-drives it,
        // never a permanent rejection (#825).
        GhError::Timeout { .. } => {
            lazybox_core::ProviderError::retryable("github", detail_of(&err))
        }
        _ => lazybox_core::ProviderError::permanent("github", err.to_string()),
    }
}

/// Map a GitHub branch-rule `type` (repository rulesets + classic
/// protection, as reported by `GET /repos/{o}/{r}/rules/branches/{b}`)
/// to a short human name for the merge-blocked notice (issue #998).
///
/// `None` for rule types that constrain how refs are created / named /
/// deleted rather than whether a PR can merge — naming those in a
/// "can't merge" notice would be noise. Unknown types fall through to a
/// prettified form of the raw type so a new merge-relevant rule still
/// gets surfaced rather than silently dropped.
fn humanize_rule(kind: &str, params: Option<&serde_json::Value>) -> Option<String> {
    match kind {
        "creation"
        | "deletion"
        | "update"
        | "non_fast_forward"
        | "branch_name_pattern"
        | "tag_name_pattern"
        | "commit_message_pattern"
        | "commit_author_email_pattern"
        | "committer_email_pattern" => None,
        "pull_request" => {
            let approvals = params
                .and_then(|p| p.get("required_approving_review_count"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Some(if approvals > 0 {
                format!(
                    "{approvals} approving review{}",
                    if approvals == 1 { "" } else { "s" }
                )
            } else {
                "pull request review".to_string()
            })
        }
        "required_status_checks" => Some("required status checks".to_string()),
        "required_signatures" => Some("signed commits".to_string()),
        "required_linear_history" => Some("linear history".to_string()),
        "required_deployments" => Some("required deployments".to_string()),
        "merge_queue" => Some("merge queue".to_string()),
        other => Some(other.replace('_', " ")),
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

fn request_profile(
    operation: &str,
) -> (
    crate::rate_budget::ApiResource,
    crate::rate_budget::RequestPriority,
) {
    use crate::rate_budget::{ApiResource, RequestPriority};
    match operation {
        "notifications heartbeat" => (ApiResource::rest("core"), RequestPriority::Recent),
        "budget-bootstrap" => (ApiResource::Graphql, RequestPriority::Recent),
        "hot-target batch query" => (ApiResource::Graphql, RequestPriority::Focused),
        // User-initiated one-node syncs (`g s`, detail fetch on focus,
        // the auto-merge pre-merge probe) — a few points each. They
        // must stay admissible while the primary budget is healthy,
        // even when the background allowance is spent (#1218): being
        // classified `Recent` meant an explicit `g s` was refused
        // exactly when the user most needed it, with the refusal
        // swallowed as a log line.
        "single-PR interactive sync" | "single-issue interactive sync" => {
            (ApiResource::Graphql, RequestPriority::Interactive)
        }
        "single-PR notification deep-fetch"
        | "single-issue notification deep-fetch"
        | "PR details background prefetch"
        | "PR search"
        | "issues search"
        | "review-requested"
        | "merged-sweep"
        | "watched-repo"
        | "round-robin-repo" => (ApiResource::Graphql, RequestPriority::Recent),
        operation if operation.starts_with("list ") || operation == "post issue comment" => {
            (ApiResource::rest("core"), RequestPriority::Interactive)
        }
        _ => (ApiResource::Graphql, RequestPriority::Interactive),
    }
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

/// Per-slot outcome of [`GhClient::fetch_hot_tasks`] (#1218).
#[derive(Debug)]
pub enum HotFetch {
    /// The node's freshness probe moved since the last tick — full
    /// detail was re-fetched.
    Fresh(Box<Task>),
    /// Probe byte-identical to the last one seen: nothing lazybox
    /// renders changed. The caller keeps its cached task and skips the
    /// upsert/broadcast entirely.
    Unchanged,
    /// Node not visible (deleted / transferred / scope change).
    Missing,
}

/// GraphQL `nodes(ids:)` hard-errors past 100 ids — both hot tiers
/// chunk at this bound (#1218).
const HOT_BATCH_MAX_IDS: usize = 100;

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
    /// Serialized rate/throttle state last successfully written to the
    /// store, so the poller can skip a redundant per-tick write when
    /// nothing persistable changed (idle notification-only ticks, or before
    /// the first observation). Shared across clones like `budget`, and
    /// updated only after a confirmed write so a failed store retries.
    last_persisted_rate_state: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    /// GitHub's secondary limits apply across REST and GraphQL. All
    /// client clones therefore share one concurrency gate, and all
    /// mutations additionally share a serial lane.
    request_gate: std::sync::Arc<tokio::sync::Semaphore>,
    mutation_gate: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Notifications heartbeat state — `Last-Modified` echo + slow-sweep
    /// timer. Shared across clones so `with_filters` doesn't reset the
    /// 304-conditional or trigger a redundant full sweep.
    notifications_state: SharedNotificationsState,
    /// Last-seen hot-set freshness fingerprints keyed by node id
    /// (#1218): the serialized lean-probe node from the previous hot
    /// tick. A byte-identical probe means nothing lazybox renders can
    /// have changed, so the expensive full-detail fetch is skipped.
    /// Shared across clones (one hot set per credential); pruned to the
    /// requested id set each batch and cleared by `force_full_sweep` so
    /// an explicit refresh always re-fetches.
    hot_freshness: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<String, String>>>,
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
            last_persisted_rate_state: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            request_gate: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
            mutation_gate: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            notifications_state: NotificationsState::shared(),
            hot_freshness: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
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
            last_persisted_rate_state: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            request_gate: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
            mutation_gate: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            notifications_state: NotificationsState::shared(),
            hot_freshness: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
        })
    }

    /// Test-only: a stub client whose transport points at a caller-owned
    /// mock HTTP server. Lets server-side crash-journey tests (working-claim
    /// maintenance) drive the real label mutation paths against canned
    /// responses without the network. Retry middleware is disabled so
    /// request-count assertions measure our calls, not octocrab's.
    #[doc(hidden)]
    pub fn stub_with_base_uri_for_tests(base_uri: &str) -> Result<Self, GhError> {
        let inner = Octocrab::builder()
            .base_uri(base_uri)
            .map_err(GhError::Api)?
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .build()
            .map_err(GhError::Api)?;
        let stub = Self::stub_for_tests("test", "fp")?;
        Ok(Self { inner, ..stub })
    }

    /// Test-only: a stub client whose cached budget already contains a
    /// remote rate-limit observation.
    #[doc(hidden)]
    pub fn stub_with_rate_limit_for_tests(
        credential_source: &str,
        credential_fingerprint: &str,
        remaining: u32,
        limit: u32,
        reset_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Self, GhError> {
        let client = Self::stub_for_tests(credential_source, credential_fingerprint)?;
        client
            .budget
            .lock()
            .observe(crate::rate_budget::RemoteRateLimit {
                remaining,
                limit,
                reset_at,
                observed_at: std::time::Instant::now(),
            });
        Ok(client)
    }

    /// Snapshot of the current rate budget state. Used by the polling
    /// layer to surface a status indicator and decide pacing.
    pub fn rate_snapshot(&self) -> crate::rate_budget::Snapshot {
        self.budget.lock().snapshot()
    }

    pub fn with_background_share(self, share: f64) -> Self {
        self.budget.lock().set_background_share(share);
        self
    }

    pub fn begin_background_tick(
        &self,
        interval: std::time::Duration,
    ) -> crate::rate_budget::BackgroundPlan {
        self.budget.lock().begin_background_tick(
            interval,
            chrono::Utc::now(),
            std::time::Instant::now(),
        )
    }

    pub fn governor_summary(&self) -> String {
        self.rate_snapshot().compact()
    }

    pub async fn bootstrap_graphql_budget(&self) -> Result<(), GhError> {
        self.acquire_or_block("budget-bootstrap")?;
        let response: graphql::GqlRateBudgetResponse = self
            .post_graphql_with_retry("budget-bootstrap", &graphql::rate_budget_body())
            .await?;
        if let Some(errors) = response.errors {
            let joined = errors
                .iter()
                .map(graphql::GqlError::full)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::Graphql(format!(
                "GraphQL budget bootstrap: {joined}"
            )));
        }
        let data = response
            .data
            .ok_or_else(|| GhError::Graphql("GraphQL budget bootstrap returned no data".into()))?;
        if data.viewer.login != self.user {
            return Err(GhError::Graphql(
                "GraphQL budget bootstrap returned a different viewer".into(),
            ));
        }
        tracing::debug!(
            remaining = data.rate_limit.remaining,
            limit = data.rate_limit.limit,
            "GraphQL budget bootstrapped"
        );
        Ok(())
    }

    pub fn background_sweep_forecast(
        &self,
        want_prs: bool,
        scan_issues: bool,
    ) -> BackgroundSweepForecast {
        let budget = self.budget.lock();
        let issue_points = if scan_issues {
            budget.unit_forecast("issues search", 1)
        } else {
            0
        };
        if !want_prs {
            return BackgroundSweepForecast {
                global_points: issue_points,
                repo_base_points: issue_points,
                per_repo_points: 0,
            };
        }

        let reviewer_points = budget.unit_forecast("review-requested", 1);
        let merged_points = budget.unit_forecast("merged-sweep", 1);
        let watched_points = budget
            .unit_forecast("watched-repo", 1)
            .saturating_mul(self.watch_repos.len() as u32);
        BackgroundSweepForecast {
            global_points: budget
                .unit_forecast("PR search", 1)
                .saturating_add(reviewer_points)
                .saturating_add(merged_points)
                .saturating_add(watched_points)
                .saturating_add(issue_points),
            repo_base_points: reviewer_points
                .saturating_add(merged_points)
                .saturating_add(issue_points),
            per_repo_points: budget.unit_forecast("round-robin-repo", 1),
        }
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
    /// `acquire_or_block`; each retry re-enters admission because
    /// GitHub charges every HTTP attempt.
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
    async fn post_graphql_with_retry<T>(
        &self,
        operation: &'static str,
        body: &serde_json::Value,
    ) -> Result<T, GhError>
    where
        T: serde::de::DeserializeOwned,
    {
        self.post_graphql_with_retry_measured(operation, body)
            .await
            .map(|(parsed, _bytes)| parsed)
    }

    /// Like [`Self::post_graphql_with_retry`] but also surfaces the byte
    /// length of the successful response body. Used by the PR-fetch
    /// path to record per-branch response size for sync profiling.
    ///
    /// Wraps the retry ladder in [`GRAPHQL_OPERATION_DEADLINE`] so the
    /// *whole* operation — governor pacing, the concurrency-slot wait,
    /// every network round-trip, and retry backoff — is bounded, not just
    /// each individual network call. Without this a starved governor
    /// (#782) could stall an op for minutes behind a spinner (#825).
    async fn post_graphql_with_retry_measured<T>(
        &self,
        operation: &'static str,
        body: &serde_json::Value,
    ) -> Result<(T, usize), GhError>
    where
        T: serde::de::DeserializeOwned,
    {
        match tokio::time::timeout(
            GRAPHQL_OPERATION_DEADLINE,
            self.post_graphql_retry_loop(operation, body),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.budget.lock().observe_failed_response(
                    operation,
                    crate::rate_budget::ApiResource::Graphql,
                    0,
                    0,
                    GRAPHQL_OPERATION_DEADLINE,
                );
                tracing::warn!(
                    "graphql operation {operation:?} exceeded {}s overall deadline",
                    GRAPHQL_OPERATION_DEADLINE.as_secs(),
                );
                Err(GhError::Timeout {
                    operation,
                    after_secs: GRAPHQL_OPERATION_DEADLINE.as_secs(),
                })
            }
        }
    }

    /// The bounded retry ladder itself. Each network attempt is capped by
    /// `REQUEST_TIMEOUT`; the caller
    /// ([`Self::post_graphql_with_retry_measured`]) caps the sum with
    /// [`GRAPHQL_OPERATION_DEADLINE`].
    async fn post_graphql_retry_loop<T>(
        &self,
        operation: &'static str,
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
        // breaks 5s) but well under `GRAPHQL_OPERATION_DEADLINE` so a
        // hung call surfaces — and the op can still retry — before the
        // overall deadline aborts it.
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
            if attempt > 0 {
                self.acquire_or_block(operation)?;
            }
            // Pace + take the concurrency slot (and the serial mutation
            // lane) OUTSIDE the network timeout: waiting on our own
            // governor is not the hung request the timeout guards
            // against, so it must not consume that budget. The guards
            // are scoped to this block so they release before any
            // backoff sleep — a retry wait never holds a slot (#745).
            let timed = {
                let _permit = self.request_permit().await?;
                let _mutation_guard = if is_graphql_mutation(body) {
                    Some(self.mutation_gate.lock().await)
                } else {
                    None
                };
                tokio::time::timeout(
                    REQUEST_TIMEOUT,
                    self.post_graphql_once::<T>(operation, body),
                )
                .await
            };
            let outcome = match timed {
                Ok(r) => r,
                Err(_elapsed) => {
                    self.budget.lock().observe_failed_response(
                        operation,
                        crate::rate_budget::ApiResource::Graphql,
                        0,
                        0,
                        REQUEST_TIMEOUT,
                    );
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
    async fn post_graphql_once<T>(
        &self,
        operation: &'static str,
        body: &serde_json::Value,
    ) -> Result<(T, usize), GhError>
    where
        T: serde::de::DeserializeOwned,
    {
        // The caller (`post_graphql_with_retry_measured`) has already
        // paced, taken the concurrency slot, and — for mutations — the
        // serial lane. Timing starts here, at the network call, so the
        // recorded request latency measures GitHub's round-trip and not
        // the self-imposed governor waits (#745).
        let started = std::time::Instant::now();
        let response = match self.inner._post("/graphql", Some(body)).await {
            Ok(response) => response,
            Err(error) => {
                self.budget.lock().observe_failed_response(
                    operation,
                    crate::rate_budget::ApiResource::Graphql,
                    0,
                    0,
                    started.elapsed(),
                );
                return Err(GhError::Api(error));
            }
        };
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
            header("x-ratelimit-resource").as_deref(),
            header("x-ratelimit-limit").as_deref(),
            header("x-ratelimit-used").as_deref(),
        );
        let raw_body = match self.inner.body_to_string(response).await {
            Ok(body) => body,
            Err(error) => {
                self.budget.lock().observe_failed_response(
                    operation,
                    crate::rate_budget::ApiResource::Graphql,
                    status,
                    0,
                    started.elapsed(),
                );
                return Err(GhError::Api(error));
            }
        };
        let byte_len = raw_body.len();
        let elapsed = started.elapsed();
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
                let retry_at = self.observe_limit_response(&rate_headers, status, "graphql");
                self.budget.lock().observe_failed_response(
                    operation,
                    crate::rate_budget::ApiResource::Graphql,
                    status,
                    byte_len,
                    elapsed,
                );
                let retry_after_secs = retry_at
                    .signed_duration_since(now)
                    .to_std()
                    .unwrap_or_default()
                    .as_secs()
                    .max(1);
                return Err(GhError::RateLimited {
                    retry_after_secs,
                    reason: format!(
                        "github answered HTTP {status} ({})",
                        body_excerpt(&raw_body)
                    ),
                    // GitHub imposed this limit, not lazybox's governor.
                    self_throttle: false,
                });
            }
            self.budget.lock().observe_failed_response(
                operation,
                crate::rate_budget::ApiResource::Graphql,
                status,
                byte_len,
                elapsed,
            );
            return Err(http_status_error(status, &content_type, &raw_body));
        }
        // 2xx + JSON content-type: this is the success path. A parse
        // failure here is a real schema mismatch between our types
        // and GitHub's response — surface it with status + content-
        // type intact instead of dropping to `Serde`. The raw body
        // goes to `tracing` only: it can carry the full GraphQL
        // response (node payloads, JSON braces) which must never reach
        // a user-facing footer notice (issue #305).
        serde_json::from_str::<serde_json::Value>(&raw_body)
            .and_then(|value| {
                let rate_limit = value
                    .get("data")
                    .and_then(|data| data.get("rateLimit"))
                    .cloned()
                    .and_then(|rate_limit| {
                        serde_json::from_value::<graphql::GqlRateLimit>(rate_limit).ok()
                    });
                if let Some((rate_limit, reset_at)) = rate_limit.as_ref().and_then(|rate_limit| {
                    chrono::DateTime::parse_from_rfc3339(&rate_limit.reset_at)
                        .ok()
                        .map(|reset_at| (rate_limit, reset_at))
                }) {
                    self.budget.lock().observe_graphql_response(
                        operation,
                        crate::rate_budget::RemoteRateLimit {
                            remaining: rate_limit.remaining,
                            limit: rate_limit.limit,
                            reset_at: reset_at.with_timezone(&chrono::Utc),
                            observed_at: std::time::Instant::now(),
                        },
                        rate_limit.used,
                        rate_limit.cost.unwrap_or(1),
                        status,
                        byte_len,
                        elapsed,
                    );
                } else if let Some((remote, used)) = rate_headers.graphql_budget() {
                    // Mutations can't carry a body `rateLimit` block, so
                    // their budget comes from the response headers instead
                    // (issue #822). Each GraphQL mutation bills one point.
                    self.budget.lock().observe_graphql_response(
                        operation, remote, used, 1, status, byte_len, elapsed,
                    );
                } else {
                    self.budget.lock().observe_failed_response(
                        operation,
                        crate::rate_budget::ApiResource::Graphql,
                        status,
                        byte_len,
                        elapsed,
                    );
                }
                serde_json::from_value::<T>(value)
            })
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
        let (resource, mut priority) = request_profile(op);
        if self.notifications_state.lock().force_full_sweep {
            priority = crate::rate_budget::RequestPriority::Interactive;
        }
        if let Err(reason) = self.budget.lock().admit(resource, op, priority, 1) {
            tracing::warn!("{op} blocked by rate budget: {reason}");
            let retry_after_secs = reason.retry_after_secs(chrono::Utc::now());
            let self_throttle = reason.is_self_imposed();
            return Err(GhError::RateLimited {
                retry_after_secs,
                reason: reason.to_string(),
                self_throttle,
            });
        }
        Ok(())
    }

    async fn request_permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, GhError> {
        // Pace BEFORE taking a concurrency slot: a request that is only
        // waiting out its governor gap is not doing work, so it must not
        // occupy a permit (that would shrink effective concurrency and
        // let a background sleeper delay an interactive request).
        //
        // Bound the COMBINED wait. Both the pacing sleep and the slot
        // acquire are governed by lazybox's own state, so under governor
        // self-starvation (#782) they could block for minutes with no
        // network involved — the mechanism behind the 307s spinner (#825).
        // Cap it and fail fast with a self-throttle "GitHub is busy" the
        // poll scheduler backs off from honestly, rather than hanging.
        let acquire = async {
            self.pace().await;
            self.request_gate
                .acquire()
                .await
                .map_err(|_| GhError::Graphql("GitHub request gate closed".to_string()))
        };
        match tokio::time::timeout(PERMIT_WAIT_TIMEOUT, acquire).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    "request pacing/concurrency wait exceeded {}s — governor backlog",
                    PERMIT_WAIT_TIMEOUT.as_secs()
                );
                Err(GhError::RateLimited {
                    retry_after_secs: PERMIT_WAIT_TIMEOUT.as_secs(),
                    reason: format!(
                        "GitHub is busy: pacing/concurrency wait exceeded {}s",
                        PERMIT_WAIT_TIMEOUT.as_secs()
                    ),
                    // lazybox's own governor stalled, not a GitHub-imposed
                    // limit — honest backoff, never "check your token".
                    self_throttle: true,
                })
            }
        }
    }

    /// Sleep out the governor-computed inter-request gap so request
    /// starts stay spaced (secondary-limit protection). Called before a
    /// concurrency slot is taken so the sleep never holds one, and never
    /// holds the (parking_lot) budget lock across the await.
    async fn pace(&self) {
        let wait = self
            .budget
            .lock()
            .reserve_request_slot(std::time::Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    fn observe_unreported_response(
        &self,
        operation: &str,
        resource: crate::rate_budget::ApiResource,
        started: std::time::Instant,
        succeeded: bool,
    ) {
        self.budget.lock().observe_unreported_response(
            operation,
            resource,
            if succeeded { 200 } else { 0 },
            started.elapsed(),
        );
    }

    pub fn note_items_changed(&self, items: usize) {
        self.budget.lock().note_items_changed(items);
    }

    fn observe_limit_response(
        &self,
        headers: &RateLimitHeaders,
        status: u16,
        default_resource: &str,
    ) -> chrono::DateTime<chrono::Utc> {
        let now = chrono::Utc::now();
        let resource = headers.resource.as_deref().unwrap_or(default_resource);
        let mut budget = self.budget.lock();
        if let Some(seconds) = headers.retry_after_secs {
            let retry_at =
                now + chrono::Duration::seconds(seconds.min(i64::MAX as u64).max(1) as i64);
            if headers.remaining == Some(0) {
                budget.observe_primary_limit(
                    resource,
                    retry_at,
                    format!("HTTP {status} primary rate limit"),
                );
            } else {
                budget.observe_secondary_limit(
                    Some(std::time::Duration::from_secs(seconds.max(1))),
                    now,
                );
            }
            return retry_at;
        }
        if headers.remaining == Some(0) {
            let retry_at = headers
                .reset_epoch_secs
                .and_then(|epoch| chrono::DateTime::from_timestamp(epoch as i64, 0))
                .unwrap_or_else(|| now + chrono::Duration::seconds(60))
                + chrono::Duration::seconds(1);
            budget.observe_primary_limit(
                resource,
                retry_at,
                format!("HTTP {status} primary rate limit"),
            );
            return retry_at;
        }
        budget.observe_secondary_limit(None, now)
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
                private: false,
            });
        }

        // Orgs the user belongs to. This is a setup-wizard REST call,
        // so its steady-state poll cost is zero, but it still enters
        // the shared governor and concurrency gate.
        self.acquire_or_block("list org memberships")?;
        let _permit = self.request_permit().await?;
        let started = std::time::Instant::now();
        let result = self
            .inner
            .current()
            .list_org_memberships_for_authenticated_user()
            .send()
            .await;
        self.observe_unreported_response(
            "list org memberships",
            crate::rate_budget::ApiResource::rest("core"),
            started,
            result.is_ok(),
        );
        let orgs: Vec<octocrab::models::orgs::Organization> = result
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
                private: false,
            });
        }

        Ok(scopes)
    }

    /// Every repo the authenticated user can access — owned,
    /// org-member, or direct outside-collaborator — as
    /// `github:<owner>/<repo>` scopes, plus their own `github:<login>`.
    ///
    /// The poller unions these into its scope allowlist so an involved
    /// PR/issue in any repo you're a member or collaborator of surfaces
    /// without being ticked in setup (a repo under another *user's*
    /// account, e.g. a collaborator repo, can't be covered by an `org:`
    /// scope, which is why this lists individual repos). A non-member
    /// public repo you merely commented on is not in this set, so it
    /// stays filtered out.
    ///
    /// All-or-nothing: returns `Err` if the token is rate-blocked or any
    /// page fails, so the caller can retry next tick rather than caching
    /// a truncated allowlist that would silently hide repos for the
    /// daemon's lifetime. Every owner comes from the repo's real owner
    /// (never assumed to be the caller — collaborator repos live under
    /// other accounts). The result is unioned into a `BTreeSet`, so no
    /// sort/dedup here. `affiliation` unions the three access kinds in
    /// one query.
    pub async fn accessible_scopes(&self) -> Result<Vec<String>, GhError> {
        let mut out: Vec<String> = Vec::new();
        if !self.user.is_empty() {
            out.push(format!("github:{}", self.user));
        }

        self.acquire_or_block("list accessible repos page 1")?;
        let mut page = {
            let _permit = self.request_permit().await?;
            let started = std::time::Instant::now();
            let result = self
                .inner
                .current()
                .list_repos_for_authenticated_user()
                .affiliation("owner,collaborator,organization_member")
                .per_page(100)
                .send()
                .await;
            self.observe_unreported_response(
                "list accessible repos page 1",
                crate::rate_budget::ApiResource::rest("core"),
                started,
                result.is_ok(),
            );
            result.map_err(GhError::Api)?
        };
        loop {
            for repo in &page.items {
                // Use the repo's actual owner — a collaborator/org repo is
                // owned by someone else, so assuming `self.user` would mint
                // a scope that never matches the task's real repo.
                let full = match repo.full_name.clone() {
                    Some(full) => full,
                    None => match repo.owner.as_ref() {
                        Some(owner) => format!("{}/{}", owner.login, repo.name),
                        None => continue,
                    },
                };
                out.push(format!("github:{full}"));
            }
            if page.next.is_none() {
                break;
            }
            self.acquire_or_block("list accessible repos next page")?;
            let next = {
                let _permit = self.request_permit().await?;
                let started = std::time::Instant::now();
                let result = self
                    .inner
                    .get_page::<octocrab::models::Repository>(&page.next)
                    .await;
                self.observe_unreported_response(
                    "list accessible repos next page",
                    crate::rate_budget::ApiResource::rest("core"),
                    started,
                    result.is_ok(),
                );
                result.map_err(GhError::Api)?
            };
            page = match next {
                Some(next) => next,
                None => break,
            };
        }
        Ok(out)
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
            let mut page = {
                let _permit = self.request_permit().await?;
                let started = std::time::Instant::now();
                let result = self
                    .inner
                    .current()
                    .list_repos_for_authenticated_user()
                    .type_("owner")
                    .per_page(100)
                    .send()
                    .await;
                self.observe_unreported_response(
                    "list own repos page 1",
                    crate::rate_budget::ApiResource::rest("core"),
                    started,
                    result.is_ok(),
                );
                result.map_err(GhError::Api)?
            };
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
                        private: repo.private.unwrap_or(false),
                    });
                }
                if page.next.is_none() {
                    break;
                }
                self.acquire_or_block("list own repos next page")?;
                let next = {
                    let _permit = self.request_permit().await?;
                    let started = std::time::Instant::now();
                    let result = self
                        .inner
                        .get_page::<octocrab::models::Repository>(&page.next)
                        .await;
                    self.observe_unreported_response(
                        "list own repos next page",
                        crate::rate_budget::ApiResource::rest("core"),
                        started,
                        result.is_ok(),
                    );
                    result.map_err(GhError::Api)?
                };
                page = match next {
                    Some(next) => next,
                    None => break,
                };
            }
        } else {
            self.acquire_or_block("list org repos page 1")?;
            let mut page = {
                let _permit = self.request_permit().await?;
                let started = std::time::Instant::now();
                let result = self
                    .inner
                    .orgs(owner)
                    .list_repos()
                    .per_page(100)
                    .send()
                    .await;
                self.observe_unreported_response(
                    "list org repos page 1",
                    crate::rate_budget::ApiResource::rest("core"),
                    started,
                    result.is_ok(),
                );
                result.map_err(GhError::Api)?
            };
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
                        private: repo.private.unwrap_or(false),
                    });
                }
                if page.next.is_none() {
                    break;
                }
                self.acquire_or_block("list org repos next page")?;
                let next = {
                    let _permit = self.request_permit().await?;
                    let started = std::time::Instant::now();
                    let result = self
                        .inner
                        .get_page::<octocrab::models::Repository>(&page.next)
                        .await;
                    self.observe_unreported_response(
                        "list org repos next page",
                        crate::rate_budget::ApiResource::rest("core"),
                        started,
                        result.is_ok(),
                    );
                    result.map_err(GhError::Api)?
                };
                page = match next {
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
    /// search runs at most once every `FULL_SWEEP_INTERVAL`. The
    /// conditional notifications heartbeat and targeted hot-row reads
    /// carry normal freshness. This slower discovery sweep closes
    /// notification coverage gaps without repeating the broad fan-out
    /// on every one-minute poll.
    pub const FULL_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

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
    pub const FULL_RECONCILE_INTERVAL: std::time::Duration =
        std::time::Duration::from_secs(60 * 60);

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
        state.last_full_sweep_at = Some(chrono::Utc::now());
        state.force_full_sweep = false;
        state.backoff_catchup_sweep_due = false;
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
            state.last_full_reconcile_at = Some(chrono::Utc::now());
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
    /// than waiting up to 30 min for the next scheduled sweep — the
    /// incremental notifications path never sees an issue the user
    /// created themselves (no self-notification). The flag is one-shot:
    /// `mark_full_sweep_done` clears it once the sweep completes.
    pub fn force_full_sweep(&self) {
        self.notifications_state.lock().force_full_sweep = true;
        // An explicit refresh must observe everything anew — drop the
        // hot-freshness fingerprints so the next hot batch re-fetches
        // full detail for the whole set (#1218).
        self.hot_freshness.lock().clear();
    }

    pub fn manual_refresh_pending(&self) -> bool {
        self.notifications_state.lock().force_full_sweep
    }

    /// Snapshot of the current notifications heartbeat state. Read-only;
    /// exists so tests (and a future status indicator) can observe
    /// whether the slow-sweep timer is armed.
    pub fn notifications_snapshot(&self) -> NotificationsSnapshot {
        let s = self.notifications_state.lock();
        NotificationsSnapshot {
            has_last_modified: s.last_modified.is_some(),
            last_full_sweep_elapsed: s
                .last_full_sweep_at
                .and_then(|at| chrono::Utc::now().signed_duration_since(at).to_std().ok()),
            heartbeat_backed_off: s.heartbeat_backed_off(),
        }
    }

    pub fn sync_cursors(&self) -> crate::SyncCursors {
        self.notifications_state.lock().cursors()
    }

    pub fn restore_sync_cursors(&self, cursors: crate::SyncCursors) {
        self.notifications_state
            .lock()
            .restore_cursors(cursors, chrono::Utc::now());
    }

    /// Durable rate/throttle state for persistence. See
    /// [`crate::rate_budget::PersistedRateState`].
    pub fn persisted_rate_state(&self) -> crate::rate_budget::PersistedRateState {
        self.budget.lock().persisted_state()
    }

    /// The serialized rate/throttle state to persist this tick, or `None`
    /// when it is byte-for-byte identical to the last state we confirmed
    /// written — so idle notification-only ticks (and the pre-observation
    /// warm-up) don't rewrite an unchanged blob every poll. The caller
    /// must call [`Self::mark_rate_state_persisted`] once the write lands.
    pub fn pending_rate_state_payload(&self) -> Option<String> {
        let payload = serde_json::to_string(&self.budget.lock().persisted_state()).ok()?;
        if self.last_persisted_rate_state.lock().as_deref() == Some(payload.as_str()) {
            None
        } else {
            Some(payload)
        }
    }

    /// Record that `payload` was successfully persisted. Updating this only
    /// after a confirmed write means a failed store write is retried on the
    /// next tick instead of being silently dropped.
    pub fn mark_rate_state_persisted(&self, payload: String) {
        *self.last_persisted_rate_state.lock() = Some(payload);
    }

    /// Reload durable rate/throttle state at startup so a fresh daemon
    /// resumes respecting the limits it learned before the restart.
    pub fn restore_rate_state(&self, state: crate::rate_budget::PersistedRateState) {
        self.budget.lock().restore(state);
    }

    /// How long to use the full-sweep fallback after a heartbeat
    /// failure. The governor still admits each fallback unit; this
    /// window prevents retrying the broken REST heartbeat every tick.
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
            // One catch-up sweep for whatever the dead heartbeat may
            // have missed; subsequent backed-off ticks stay hot-only
            // (#1218 — a full sweep every tick for the whole back-off
            // window burned the most quota during exhaustion).
            state.backoff_catchup_sweep_due = true;
            tracing::warn!(
                back_off_secs = Self::HEARTBEAT_BACK_OFF.as_secs(),
                "notifications heartbeat failed — backing off; one catch-up sweep, then hot-only ticks",
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
        let now = std::time::Instant::now();
        {
            let state = self.notifications_state.lock();
            if !state.heartbeat_due(now) {
                return Ok(NotificationsPoll::NotModified);
            }
        }
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
        let _permit = self.request_permit().await?;
        self.notifications_state.lock().last_poll_at = Some(std::time::Instant::now());

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
        // Time only the network round-trip — the pacing/permit waits
        // above are self-imposed and must not inflate request latency
        // metrics (#745).
        let started = std::time::Instant::now();
        let response = match self.inner._get_with_headers(uri, Some(headers)).await {
            Ok(response) => response,
            Err(error) => {
                self.budget.lock().observe_failed_response(
                    "notifications heartbeat",
                    crate::rate_budget::ApiResource::rest("core"),
                    0,
                    0,
                    started.elapsed(),
                );
                return Err(GhError::Api(error));
            }
        };

        let status = response.status();
        let response_header = |name: &str| -> Option<String> {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let rate_headers = RateLimitHeaders::parse(
            response_header("retry-after").as_deref(),
            response_header("x-ratelimit-remaining").as_deref(),
            response_header("x-ratelimit-reset").as_deref(),
            response_header("x-ratelimit-resource").as_deref(),
            response_header("x-ratelimit-limit").as_deref(),
            response_header("x-ratelimit-used").as_deref(),
        );
        if let Some(seconds) =
            response_header("x-poll-interval").and_then(|value| value.parse::<u64>().ok())
        {
            self.notifications_state.lock().poll_interval =
                Some(std::time::Duration::from_secs(seconds.max(1)));
        }
        let observe_rest = |byte_len: usize, conditional_hit: bool| {
            let reset_at = rate_headers
                .reset_epoch_secs
                .and_then(|epoch| chrono::DateTime::from_timestamp(epoch as i64, 0));
            if let (Some(limit), Some(remaining), Some(used), Some(reset_at)) = (
                rate_headers.limit,
                rate_headers.remaining,
                rate_headers.used,
                reset_at,
            ) {
                self.budget.lock().observe_rest_response(
                    rate_headers.resource.as_deref().unwrap_or("core"),
                    "notifications heartbeat",
                    limit,
                    remaining,
                    used,
                    reset_at,
                    status.as_u16(),
                    conditional_hit,
                    byte_len,
                    started.elapsed(),
                );
            } else {
                self.budget.lock().observe_failed_response(
                    "notifications heartbeat",
                    crate::rate_budget::ApiResource::rest(
                        rate_headers.resource.as_deref().unwrap_or("core"),
                    ),
                    status.as_u16(),
                    byte_len,
                    started.elapsed(),
                );
            }
        };
        // 304 = nothing new since If-Modified-Since. The endpoint also
        // sends an empty body in that case; don't try to deserialize.
        if status == StatusCode::NOT_MODIFIED {
            observe_rest(0, true);
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
            observe_rest(body.len(), false);
            let snippet = body_prefix_bytes(&body, 512);
            if is_rate_limit_response(
                status.as_u16(),
                &body,
                rate_headers.retry_after_secs.is_some(),
            ) {
                let now = chrono::Utc::now();
                let retry_at = self.observe_limit_response(&rate_headers, status.as_u16(), "core");
                return Err(GhError::RateLimited {
                    retry_after_secs: retry_at
                        .signed_duration_since(now)
                        .to_std()
                        .unwrap_or_default()
                        .as_secs()
                        .max(1),
                    reason: format!("notifications HTTP {}", status.as_u16()),
                    // GitHub imposed this limit, not lazybox's governor.
                    self_throttle: false,
                });
            }
            return Err(GhError::Graphql(format!(
                "notifications HTTP {}: {snippet}",
                status.as_u16(),
            )));
        }

        let body = match self.inner.body_to_string(response).await {
            Ok(body) => body,
            Err(error) => {
                self.budget.lock().observe_failed_response(
                    "notifications heartbeat",
                    crate::rate_budget::ApiResource::rest(
                        rate_headers.resource.as_deref().unwrap_or("core"),
                    ),
                    status.as_u16(),
                    0,
                    started.elapsed(),
                );
                return Err(GhError::Api(error));
            }
        };
        observe_rest(body.len(), false);
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

    /// Fetch the bounded hot set, two-tier (#1218): a lean freshness
    /// probe first (~10 nodes/PR), then the full-detail query only for
    /// nodes whose probe moved since the last tick. `nodes(ids:)`
    /// preserves input order, so the returned vector has one slot per
    /// requested id. Both tiers are chunked at `HOT_BATCH_MAX_IDS` —
    /// GraphQL errors a `nodes(ids:)` list past 100 outright, which
    /// used to fail the whole hot refresh once 100+ workspaces held
    /// sessions.
    pub async fn fetch_hot_tasks(&self, node_ids: &[String]) -> Result<Vec<HotFetch>, GhError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let started = std::time::Instant::now();
        let mut metrics = BranchMetrics::new("hot-targets");

        // Tier 1 (#1218): the lean freshness probe, chunked so >100 ids
        // can't error the whole query. `None` slot = node not visible.
        let mut fingerprints: Vec<Option<String>> = Vec::with_capacity(node_ids.len());
        for chunk in node_ids.chunks(HOT_BATCH_MAX_IDS) {
            self.acquire_or_block("hot-target batch query")?;
            let body = graphql::hot_freshness_body(chunk);
            let (response, bytes): (graphql::GqlHotFreshnessResponse, usize) = self
                .post_graphql_with_retry_measured("hot-target batch query", &body)
                .await?;
            metrics.requests += 1;
            metrics.resp_bytes += bytes;

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
                    return Err(GhError::Graphql("hot-target probe returned no data".into()));
                }
                fingerprints.extend(std::iter::repeat_with(|| None).take(chunk.len()));
                continue;
            };
            if data.nodes.len() != chunk.len() {
                return Err(GhError::Graphql(format!(
                    "hot-target probe returned {} node slots for {} ids",
                    data.nodes.len(),
                    chunk.len()
                )));
            }
            if let Some(rate_limit) = &data.rate_limit {
                metrics.graphql_cost += rate_limit.cost.unwrap_or(0);
            }
            fingerprints.extend(
                data.nodes
                    .into_iter()
                    .map(|node| node.map(|n| n.to_string())),
            );
        }

        // Which nodes actually moved since the last probe. The cache is
        // only *written* after a successful full fetch below, so a
        // failed detail fetch is retried next tick rather than lost.
        let need_full: Vec<usize> = {
            let cache = self.hot_freshness.lock();
            fingerprints
                .iter()
                .enumerate()
                .filter_map(|(idx, fingerprint)| match fingerprint {
                    Some(fp) if cache.get(&node_ids[idx]) != Some(fp) => Some(idx),
                    _ => None,
                })
                .collect()
        };

        let mut out: Vec<HotFetch> = fingerprints
            .iter()
            .map(|fingerprint| match fingerprint {
                None => HotFetch::Missing,
                Some(_) => HotFetch::Unchanged,
            })
            .collect();

        // Tier 2: full detail, only for the movers, chunked like the probe.
        for chunk in need_full.chunks(HOT_BATCH_MAX_IDS) {
            let ids: Vec<String> = chunk.iter().map(|&idx| node_ids[idx].clone()).collect();
            self.acquire_or_block("hot-target batch query")?;
            let body = graphql::hot_tasks_body(&ids);
            let (response, bytes): (graphql::GqlHotTasksResponse, usize) = self
                .post_graphql_with_retry_measured("hot-target batch query", &body)
                .await?;
            metrics.requests += 1;
            metrics.resp_bytes += bytes;

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
                for &idx in chunk {
                    out[idx] = HotFetch::Missing;
                }
                continue;
            };
            if data.nodes.len() != ids.len() {
                return Err(GhError::Graphql(format!(
                    "hot-target batch returned {} node slots for {} ids",
                    data.nodes.len(),
                    ids.len()
                )));
            }
            if let Some(rate_limit) = &data.rate_limit {
                metrics.graphql_cost += rate_limit.cost.unwrap_or(0);
            }

            let mut cache = self.hot_freshness.lock();
            for (&idx, node) in chunk.iter().zip(data.nodes) {
                match node {
                    Some(node) => {
                        let task = match node {
                            graphql::GqlHotTask::PullRequest(pr) => {
                                graphql::pr_to_task(&pr, &self.user)
                            }
                            graphql::GqlHotTask::Issue(issue) => {
                                graphql::issue_to_task(&issue, &self.user)
                            }
                        };
                        if let Some(fingerprint) = &fingerprints[idx] {
                            cache.insert(node_ids[idx].clone(), fingerprint.clone());
                        }
                        out[idx] = HotFetch::Fresh(Box::new(task));
                    }
                    None => out[idx] = HotFetch::Missing,
                }
            }
        }

        // Prune fingerprints for ids that left the hot set so the cache
        // tracks the live set instead of growing forever.
        {
            let requested: std::collections::HashSet<&String> = node_ids.iter().collect();
            self.hot_freshness
                .lock()
                .retain(|id, _| requested.contains(id));
        }

        metrics.prs = out
            .iter()
            .filter_map(|slot| match slot {
                HotFetch::Fresh(task) => Some(task),
                _ => None,
            })
            .filter(|task| task.is_pr())
            .count();
        metrics.elapsed_ms = started.elapsed().as_millis();
        metrics.emit();
        Ok(out)
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
        self.fetch_single_pr_with_head_as("single-PR notification deep-fetch", owner, repo, number)
            .await
    }

    /// [`fetch_single_pr_with_head`](Self::fetch_single_pr_with_head) at
    /// interactive priority — for user-initiated targeted syncs (`g s`)
    /// and the auto-merge pre-merge probe, which must not queue behind
    /// the background budget (#1218).
    pub async fn fetch_single_pr_with_head_interactive(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<(Task, Option<String>)>, GhError> {
        self.fetch_single_pr_with_head_as("single-PR interactive sync", owner, repo, number)
            .await
    }

    /// Interactive-priority sibling of [`fetch_single_pr`](Self::fetch_single_pr).
    pub async fn fetch_single_pr_interactive(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<Task>, GhError> {
        Ok(self
            .fetch_single_pr_with_head_interactive(owner, repo, number)
            .await?
            .map(|(task, _)| task))
    }

    async fn fetch_single_pr_with_head_as(
        &self,
        operation: &'static str,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<(Task, Option<String>)>, GhError> {
        self.acquire_or_block(operation)?;
        let body = graphql::single_pr_body(owner, repo, number);
        let response: graphql::GqlSinglePrResponse =
            self.post_graphql_with_retry(operation, &body).await?;
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
        self.fetch_single_issue_as("single-issue notification deep-fetch", owner, repo, number)
            .await
    }

    /// Interactive-priority sibling of [`fetch_single_issue`](Self::fetch_single_issue)
    /// — see [`fetch_single_pr_with_head_interactive`](Self::fetch_single_pr_with_head_interactive).
    pub async fn fetch_single_issue_interactive(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<Task>, GhError> {
        self.fetch_single_issue_as("single-issue interactive sync", owner, repo, number)
            .await
    }

    async fn fetch_single_issue_as(
        &self,
        operation: &'static str,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<Task>, GhError> {
        self.acquire_or_block(operation)?;
        let body = graphql::single_issue_body(owner, repo, number);
        let response: graphql::GqlSingleIssueResponse =
            self.post_graphql_with_retry(operation, &body).await?;
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
        self.budget.lock().note_dedup(total_fetched, unique);
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
            let retry_after_secs = repo_failures
                .iter()
                .map(|(_, error)| error.retry_after_secs())
                .collect::<Option<Vec<_>>>()
                .and_then(|delays| delays.into_iter().max());
            let details = repo_failures
                .into_iter()
                .map(|(repo, error)| format!("{repo}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            let reason = format!(
                "all {} round-robin repo queries failed: {details}",
                repos.len(),
            );
            return Err(match retry_after_secs {
                Some(retry_after_secs) => GhError::RateLimited {
                    retry_after_secs,
                    reason,
                    // Every round-robin sub-query failed — a GitHub/transport
                    // failure, not the governor's own pacing.
                    self_throttle: false,
                },
                None => GhError::Graphql(reason),
            });
        }
        if repo_failures.is_empty() {
            Ok(PrFetchOutcome::complete(tasks))
        } else {
            let failure_count = repo_failures.len();
            let retry_after_secs = repo_failures
                .iter()
                .filter_map(|(_, error)| error.retry_after_secs())
                .max();
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
                retry_after_secs,
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
        let metrics = parking_lot::Mutex::new(BranchMetrics::new("involves-main"));
        let outcome = paginate(
            |cursor, page| {
                let metrics = &metrics;
                async move {
                    self.acquire_or_block("PR search").map_err(|error| {
                        tracing::error!("PR search budget error (page {page}): {error}");
                        error
                    })?;
                    let body = graphql::query_body_after(search_query, cursor.as_deref());
                    tracing::debug!(
                        "GraphQL page {page} body: {}",
                        serde_json::to_string(&body).unwrap_or_default()
                    );
                    let (raw, page_bytes): (serde_json::Value, usize) = self
                        .post_graphql_with_retry_measured("PR search", &body)
                        .await
                        .map_err(|e| {
                            tracing::error!("GraphQL HTTP error (page {page}): {e}\n{e:?}");
                            tracing::error!(
                                "GraphQL request body was: {}",
                                serde_json::to_string_pretty(&body).unwrap_or_default()
                            );
                            e
                        })?;
                    {
                        let mut metrics = metrics.lock();
                        metrics.requests += 1;
                        metrics.resp_bytes += page_bytes;
                    }
                    let response: graphql::GqlResponse = serde_json::from_value(raw.clone())
                        .map_err(|e| {
                            tracing::error!(
                                "GraphQL response did not match schema (page {page}): {e}\n\
                             Full response body:\n{}",
                                serde_json::to_string_pretty(&raw).unwrap_or_default()
                            );
                            GhError::Graphql(format!("response schema mismatch (page {page}): {e}"))
                        })?;
                    if let Some(errors) = &response.errors {
                        let joined = errors
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
                        metrics.lock().graphql_cost += rl.cost.unwrap_or(0);
                    }
                    self.budget.lock().note_expected_pages(
                        "PR search",
                        graphql::pr_page_count(data.search.issue_count),
                    );
                    Ok(FetchPage {
                        items: data
                            .search
                            .nodes
                            .iter()
                            .map(|pr| graphql::pr_to_task(pr, &self.user))
                            .collect(),
                        page_info: data.search.page_info.map(|page_info| FetchPageInfo {
                            has_next_page: page_info.has_next_page,
                            end_cursor: page_info.end_cursor,
                        }),
                    })
                }
            },
            DEFAULT_MAX_PAGES,
        )
        .await?;
        let (tasks, incomplete) = match outcome {
            PaginationOutcome::Complete(tasks) => (tasks, None),
            PaginationOutcome::Partial { items, reason } => (items, Some(reason)),
        };
        let mut metrics = metrics.into_inner();
        metrics.prs = tasks.len();
        metrics.elapsed_ms = started.elapsed().as_millis();
        metrics.emit();
        if let Some(reason) = incomplete {
            tracing::error!(
                "GraphQL pagination stopped after {} pages; tail is non-authoritative",
                metrics.requests
            );
            return Err(incomplete_pagination_error(
                "PR search",
                tasks.len(),
                reason,
            ));
        }
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
        let metrics = parking_lot::Mutex::new(BranchMetrics::new(op));
        let outcome = paginate(
            |cursor, page| {
                let metrics = &metrics;
                let query = &query;
                async move {
                    self.acquire_or_block(op).map_err(|error| {
                        tracing::error!("{op} budget error (page {page}): {error}");
                        error
                    })?;
                    let body = graphql::query_body_after(query, cursor.as_deref());
                    let (resp, bytes): (graphql::GqlResponse, usize) = self
                        .post_graphql_with_retry_measured(op, &body)
                        .await
                        .map_err(|error| {
                            tracing::error!("{op} HTTP error (page {page}): {error}");
                            error
                        })?;
                    {
                        let mut metrics = metrics.lock();
                        metrics.requests += 1;
                        metrics.resp_bytes += bytes;
                    }
                    if let Some(errors) = resp.errors {
                        let joined = errors
                            .iter()
                            .map(|e| e.full())
                            .collect::<Vec<_>>()
                            .join("; ");
                        tracing::error!("{op} GraphQL errors (page {page}): {joined}");
                        return Err(GhError::Graphql(format!("{op}: {joined}")));
                    }
                    let Some(data) = resp.data else {
                        return Ok(FetchPage {
                            items: Vec::new(),
                            page_info: Some(FetchPageInfo {
                                has_next_page: false,
                                end_cursor: None,
                            }),
                        });
                    };
                    if let Some(rl) = &data.rate_limit {
                        metrics.lock().graphql_cost += rl.cost.unwrap_or(0);
                    }
                    self.budget
                        .lock()
                        .note_expected_pages(op, graphql::pr_page_count(data.search.issue_count));
                    Ok(FetchPage {
                        items: data
                            .search
                            .nodes
                            .iter()
                            .map(|pr| graphql::pr_to_task(pr, &self.user))
                            .collect(),
                        page_info: data.search.page_info.map(|page_info| FetchPageInfo {
                            has_next_page: page_info.has_next_page,
                            end_cursor: page_info.end_cursor,
                        }),
                    })
                }
            },
            DEFAULT_MAX_PAGES,
        )
        .await?;
        let (tasks, incomplete) = match outcome {
            PaginationOutcome::Complete(tasks) => (tasks, None),
            PaginationOutcome::Partial { items, reason } => (items, Some(reason)),
        };
        let mut metrics = metrics.into_inner();
        metrics.prs = tasks.len();
        metrics.elapsed_ms = started.elapsed().as_millis();
        metrics.emit();
        if let Some(reason) = incomplete {
            tracing::error!(
                "{op} pagination stopped after {} pages; tail is non-authoritative",
                metrics.requests
            );
            return Err(incomplete_pagination_error(op, tasks.len(), reason));
        }
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

        let pages_fetched = parking_lot::Mutex::new(0usize);
        let outcome = paginate(
            |cursor, page| {
                let pages_fetched = &pages_fetched;
                let search_query = &search_query;
                async move {
                    self.acquire_or_block("issues search").map_err(|error| {
                        tracing::error!("Issues budget error (page {page}): {error}");
                        error
                    })?;
                    let body = graphql::issues_query_body(search_query, cursor.as_deref());
                    let response: graphql::GqlIssueResponse = self
                        .post_graphql_with_retry("issues search", &body)
                        .await
                        .map_err(|e| {
                            tracing::error!("Issues HTTP error (page {page}): {e}\n{e:?}");
                            e
                        })?;
                    *pages_fetched.lock() += 1;

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
                    }
                    self.budget.lock().note_expected_pages(
                        "issues search",
                        graphql::issue_page_count(data.search.issue_count),
                    );

                    Ok(FetchPage {
                        items: data
                            .search
                            .nodes
                            .iter()
                            .map(|issue| {
                                let mentions = if allowed_logins.is_empty() {
                                    Vec::new()
                                } else {
                                    crate::mentions::scan_issue(issue, allowed_logins)
                                };
                                (graphql::issue_to_task(issue, &self.user), mentions)
                            })
                            .collect(),
                        page_info: data.search.page_info.map(|page_info| FetchPageInfo {
                            has_next_page: page_info.has_next_page,
                            end_cursor: page_info.end_cursor,
                        }),
                    })
                }
            },
            DEFAULT_MAX_PAGES,
        )
        .await?;
        let (items, incomplete) = match outcome {
            PaginationOutcome::Complete(items) => (items, None),
            PaginationOutcome::Partial { items, reason } => (items, Some(reason)),
        };
        let mut tasks = Vec::with_capacity(items.len());
        let mut mentions = Vec::new();
        for (task, mut issue_mentions) in items {
            tasks.push(task);
            mentions.append(&mut issue_mentions);
        }
        let pages_fetched = pages_fetched.into_inner();
        if let Some(reason) = incomplete {
            tracing::error!(
                "Issues pagination stopped after {pages_fetched} pages; tail is non-authoritative"
            );
            return Err(incomplete_pagination_error(
                "issues search",
                tasks.len(),
                reason,
            ));
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
                retry_after_secs: None,
                mentions: Vec::new(),
                coverage: FetchCoverage::Complete,
                pr_coverage: FetchCoverage::Complete,
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
        // No rate-limit headers are exposed by octocrab on this call,
        // but it still enters the shared admission and concurrency
        // gate.
        self.acquire_or_block("post issue comment")?;
        let _permit = self.request_permit().await?;
        let _mutation_guard = self.mutation_gate.lock().await;
        let started = std::time::Instant::now();
        let result = self
            .inner
            .issues(owner, name)
            .create_comment(issue_or_pr_number, body)
            .await;
        self.observe_unreported_response(
            "post issue comment",
            crate::rate_budget::ApiResource::rest("core"),
            started,
            result.is_ok(),
        );
        result.map_err(GhError::Api)?;
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
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("addReaction(EYES) mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            // `mutation_error_response` logs the operation + full error tail
            // (at warn! for a rate limit, error! otherwise) — don't log again.
            return Err(mutation_error_response(
                "addReaction(EYES) mutation",
                &errors,
            ));
        }
        Ok(())
    }

    /// Merge the base branch into this PR's head — same as the "Update
    /// branch" button on github.com. Requires the PR's GraphQL node ID.
    pub async fn update_branch(&self, pull_request_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("updatePullRequestBranch mutation")?;
        let body = graphql::update_branch_body(pull_request_node_id);
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("updatePullRequestBranch mutation", &body)
            .await?;
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
            return Err(mutation_error_response("updatePullRequestBranch", &errors));
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
        self.fetch_pr_details_for_operation("PR details lazy-fetch", pull_request_node_id)
            .await
    }

    pub async fn prefetch_pr_details(
        &self,
        pull_request_node_id: &str,
    ) -> Result<Option<graphql::PrDetails>, GhError> {
        self.fetch_pr_details_for_operation("PR details background prefetch", pull_request_node_id)
            .await
    }

    async fn fetch_pr_details_for_operation(
        &self,
        operation: &'static str,
        pull_request_node_id: &str,
    ) -> Result<Option<graphql::PrDetails>, GhError> {
        let started = std::time::Instant::now();
        let mut metrics = BranchMetrics::new("pr-details");
        self.acquire_or_block(operation)?;
        let body = graphql::pr_details_body(pull_request_node_id);
        let (response, bytes): (graphql::GqlPrDetailsResponse, usize) = self
            .post_graphql_with_retry_measured(operation, &body)
            .await?;
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
        let response: graphql::GqlUserIdResponse = self
            .post_graphql_with_retry("user lookup query", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response("user lookup query", &errors));
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
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("requestReviews mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response("requestReviews mutation", &errors));
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
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("addAssigneesToAssignable mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response(
                "addAssigneesToAssignable mutation",
                &errors,
            ));
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
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("removeAssigneesFromAssignable mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response(
                "removeAssigneesFromAssignable mutation",
                &errors,
            ));
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
        let response: graphql::GqlRepoLabelsResponse = self
            .post_graphql_with_retry("repository.labels query", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response("repository.labels query", &errors));
        }
        let data = response
            .data
            .ok_or_else(|| GhError::Graphql("list_repo_labels: no data".into()))?;
        let nodes = data.repository.map(|r| r.labels.nodes).unwrap_or_default();
        Ok(nodes)
    }

    /// Fetch the accounts requestable as reviewers on a PR: GitHub's
    /// suggestions for this PR first (most relevant), then the repo's
    /// assignable users. Deduped, suggestions-first. This is the pool
    /// GitHub's own reviewer dropdown draws from — far wider than the
    /// people already interacting with the PR.
    ///
    /// Paginates `assignableUsers` (100/page) so a repo with more than
    /// 100 collaborators doesn't silently drop the overflow — the exact
    /// "missing reviewers" bug this feature exists to fix. Bounded at
    /// `MAX_PAGES` as a runaway backstop (a repo that large is well past
    /// where a scroll-list picker is usable anyway); a repo that hits
    /// the cap is logged, not silently truncated.
    ///
    /// Named with the `_for_pr` suffix so this inherent method doesn't
    /// shadow the trait-side `TaskProvider::list_requestable_reviewers`
    /// (which takes a `&Workspace` and delegates here).
    pub async fn list_requestable_reviewers_for_pr(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<Vec<String>, GhError> {
        const MAX_PAGES: usize = 10;
        let mut out: Vec<String> = Vec::new();
        let mut after: Option<String> = None;
        // Suggestions are fetched only on the first page (they aren't
        // paginated); `@skip` drops them on later rounds.
        let mut skip_suggested = false;
        for page in 0..MAX_PAGES {
            self.acquire_or_block("requestable reviewers query")?;
            let body = graphql::requestable_reviewers_body(
                owner,
                name,
                number,
                after.as_deref(),
                skip_suggested,
            );
            let response: graphql::GqlRequestableReviewersResponse = self
                .post_graphql_with_retry("requestable reviewers query", &body)
                .await?;
            if let Some(errors) = response.errors {
                return Err(mutation_error_response(
                    "requestable reviewers query",
                    &errors,
                ));
            }
            let repo = response
                .data
                .and_then(|d| d.repository)
                .ok_or_else(|| GhError::Graphql("list_requestable_reviewers: no data".into()))?;
            // Read the pagination cursor before `logins()` consumes `repo`.
            let has_next = repo.assignable_users.page_info.has_next_page;
            let end_cursor = repo.assignable_users.page_info.end_cursor.clone();
            for login in repo.logins() {
                if !out.contains(&login) {
                    out.push(login);
                }
            }
            match end_cursor {
                Some(cursor) if has_next => {
                    after = Some(cursor);
                    skip_suggested = true;
                }
                // No further pages, or `hasNextPage` with no cursor
                // (defensive — never spin without an advancing cursor).
                _ => return Ok(out),
            }
            if page + 1 == MAX_PAGES {
                tracing::warn!(
                    "list_requestable_reviewers {owner}/{name}#{number}: hit {MAX_PAGES}-page cap ({} users); more assignable users exist but were omitted",
                    out.len(),
                );
            }
        }
        Ok(out)
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
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("addLabelsToLabelable mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response(
                "addLabelsToLabelable mutation",
                &errors,
            ));
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
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("removeLabelsFromLabelable mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response(
                "removeLabelsFromLabelable mutation",
                &errors,
            ));
        }
        Ok(())
    }

    /// Converge one owner/session-qualified claim on an issue or PR. Applying
    /// a heartbeat creates or renames only that identity's repository label,
    /// attaches the desired expiry, and removes superseded expiries for the
    /// same identity. Clearing removes every attached expiry for that identity.
    pub async fn sync_working_claim_target(
        &self,
        task_id: &lazybox_core::TaskId,
        repo: &str,
        desired_label: Option<&str>,
        device: &str,
        session: &str,
    ) -> Result<(), GhError> {
        let (owner, name, number) = working_claim_target(task_id, repo)?;
        let handler = self.inner.issues(owner, name);
        self.acquire_or_block("list issue working labels")?;
        let mut page = handler
            .list_labels_for_issue(number)
            .per_page(100)
            .send()
            .await
            .map_err(GhError::Api)?;
        let mut attached = Vec::new();
        loop {
            attached.extend(page.items.iter().map(|label| label.name.clone()));
            if page.next.is_none() {
                break;
            }
            self.acquire_or_block("list issue working labels next page")?;
            page = match self
                .inner
                .get_page::<octocrab::models::Label>(&page.next)
                .await
                .map_err(GhError::Api)?
            {
                Some(next) => next,
                None => break,
            };
        }

        let owned = attached
            .into_iter()
            .filter(|name| {
                lazybox_core::QualifiedWorkingClaim::parse(name)
                    .is_some_and(|claim| claim.device == device && claim.session == session)
            })
            .collect::<Vec<_>>();

        if let Some(desired) = desired_label {
            let parsed = lazybox_core::QualifiedWorkingClaim::parse(desired).ok_or_else(|| {
                GhError::Graphql("working claim: desired label is malformed".into())
            })?;
            if parsed.device != device || parsed.session != session {
                return Err(GhError::Graphql(
                    "working claim: desired label does not match its owner".into(),
                ));
            }
            if !owned.iter().any(|name| name == desired) {
                let mut available = false;
                if let Some(previous) = owned.first() {
                    self.acquire_or_block("renew working claim label")?;
                    match handler
                        .update_label(
                            previous,
                            desired,
                            "fbca04",
                            "Claimed by a lazybox agent; expires without a heartbeat.",
                        )
                        .await
                    {
                        Ok(_) => available = true,
                        Err(error) if matches!(octocrab_error_status(&error), Some(404 | 422)) => {}
                        Err(error) => return Err(GhError::Api(error)),
                    }
                }
                if !available {
                    self.acquire_or_block("create working claim label")?;
                    match handler
                        .create_label(
                            desired,
                            "fbca04",
                            "Claimed by a lazybox agent; expires without a heartbeat.",
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(error) if octocrab_error_status(&error) == Some(422) => {}
                        Err(error) => return Err(GhError::Api(error)),
                    }
                }
                self.acquire_or_block("add working claim label")?;
                handler
                    .add_labels(number, &[desired.to_string()])
                    .await
                    .map_err(GhError::Api)?;
            }
            // Superseded expiries are deleted at the repository level: a
            // qualified label names exactly one claim lease, so once it is
            // stale its *definition* is garbage too — detaching alone would
            // leak one dead label into the repo's label picker per heartbeat
            // (the rename path above already carried the definition forward).
            for previous in owned.iter().filter(|name| name.as_str() != desired) {
                self.acquire_or_block("delete working claim label")?;
                if let Err(error) = handler.delete_label(previous).await
                    && octocrab_error_status(&error) != Some(404)
                {
                    return Err(GhError::Api(error));
                }
            }
        } else {
            // Release deletes the repository-level definition (which also
            // detaches it from the issue) so a finished claim leaves nothing
            // behind in the repo's label picker.
            for label in &owned {
                self.acquire_or_block("delete working claim label")?;
                if let Err(error) = handler.delete_label(label).await
                    && octocrab_error_status(&error) != Some(404)
                {
                    return Err(GhError::Api(error));
                }
            }
        }
        Ok(())
    }

    /// Remove exact expired qualified labels without interpreting or touching
    /// the legacy `working` label or any still-live owner. Deletion happens at
    /// the repository level — an expired lease's label definition is unique to
    /// that lease, so deleting the definition both detaches it from the issue
    /// and keeps dead labels from accumulating in the repo's label picker.
    pub async fn remove_working_claim_labels_target(
        &self,
        task_id: &lazybox_core::TaskId,
        repo: &str,
        labels: &[String],
    ) -> Result<(), GhError> {
        if labels.is_empty() {
            return Ok(());
        }
        let (owner, name, _number) = working_claim_target(task_id, repo)?;
        let handler = self.inner.issues(owner, name);
        for label in labels {
            if lazybox_core::QualifiedWorkingClaim::parse(label).is_none() {
                return Err(GhError::Graphql(
                    "working claim cleanup: refusing malformed or legacy label".into(),
                ));
            }
            self.acquire_or_block("delete working claim label")?;
            if let Err(error) = handler.delete_label(label).await
                && octocrab_error_status(&error) != Some(404)
            {
                return Err(GhError::Api(error));
            }
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
        let response: graphql::GqlMergeMethodResponse = self
            .post_graphql_with_retry("pr merge-method query", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response("pr merge-method query", &errors));
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
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("mergePullRequest mutation", &body)
            .await?;
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
            return Err(mutation_error_response("mergePullRequest", &errors));
        }
        Ok(())
    }

    /// When a merge failed with GitHub's generic "Repository rule
    /// violations found" (issue #998), best-effort append the base
    /// branch's active rule names so the server's humanizer can name
    /// *which* rule blocked the merge. Any lookup failure leaves the
    /// error untouched — naming the rule is a nicety, never a new
    /// failure mode, and non-rule-violation errors pass straight through.
    async fn name_rule_violation(&self, err: GhError, pr: &lazybox_core::Task) -> GhError {
        let GhError::Graphql(msg) = &err else {
            return err;
        };
        if !msg
            .to_ascii_lowercase()
            .contains("repository rule violation")
        {
            return err;
        }
        let (Some(repo), Some(branch)) = (pr.repo.as_deref(), pr.base_branch.as_deref()) else {
            return err;
        };
        let Some((owner, name)) = repo.split_once('/') else {
            return err;
        };
        let rules = self.branch_rule_names(owner, name, branch).await;
        if rules.is_empty() {
            return err;
        }
        GhError::Graphql(format!("{msg} — active rules: {}", rules.join(", ")))
    }

    /// List the active branch rules (repository rulesets + classic
    /// protection) that apply to `branch` for the current viewer, as
    /// short human names. Reads GitHub's REST rules API
    /// (`GET /repos/{owner}/{repo}/rules/branches/{branch}`), which
    /// reports the rules the token can see. Best-effort: any error
    /// yields an empty list (the caller then keeps the generic notice).
    async fn branch_rule_names(&self, owner: &str, repo: &str, branch: &str) -> Vec<String> {
        #[derive(serde::Deserialize)]
        struct BranchRule {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            parameters: Option<serde_json::Value>,
        }
        let route = format!("/repos/{owner}/{repo}/rules/branches/{branch}");
        let rules: Vec<BranchRule> = match self.inner.get(&route, None::<&()>).await {
            Ok(rules) => rules,
            Err(e) => {
                tracing::debug!("branch-rules lookup for {owner}/{repo}@{branch} failed: {e}");
                return Vec::new();
            }
        };
        let mut names = Vec::new();
        for rule in &rules {
            if let Some(name) = humanize_rule(&rule.kind, rule.parameters.as_ref())
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
        names
    }

    pub async fn close_issue_node(&self, issue_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("closeIssue mutation")?;
        let body = graphql::close_issue_body(issue_node_id);
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("closeIssue mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response("closeIssue mutation", &errors));
        }
        Ok(())
    }

    pub async fn close_pr_node(&self, pull_request_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("closePullRequest mutation")?;
        let body = graphql::close_pr_body(pull_request_node_id);
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("closePullRequest mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response(
                "closePullRequest mutation",
                &errors,
            ));
        }
        Ok(())
    }

    pub async fn delete_issue_node(&self, issue_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("deleteIssue mutation")?;
        let body = graphql::delete_issue_body(issue_node_id);
        let response: graphql::GqlMutationResponse = self
            .post_graphql_with_retry("deleteIssue mutation", &body)
            .await?;
        if let Some(errors) = response.errors {
            return Err(mutation_error_response("deleteIssue mutation", &errors));
        }
        Ok(())
    }
}

fn octocrab_error_status(error: &octocrab::Error) -> Option<u16> {
    match error {
        octocrab::Error::GitHub { source, .. } => Some(source.status_code.as_u16()),
        _ => None,
    }
}

fn working_claim_target<'a>(
    task_id: &lazybox_core::TaskId,
    repo: &'a str,
) -> Result<(&'a str, &'a str, u64), GhError> {
    if task_id.source != lazybox_core::GITHUB_SOURCE {
        return Err(GhError::Graphql(format!(
            "working claim: task source {:?} is not GitHub",
            task_id.source
        )));
    }
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| GhError::Graphql(format!("working claim: invalid repository `{repo}`")))?;
    let (task_repo, number) = task_id
        .key
        .rsplit_once('#')
        .and_then(|(task_repo, number)| {
            number.parse::<u64>().ok().map(|number| (task_repo, number))
        })
        .ok_or_else(|| {
            GhError::Graphql(format!(
                "working claim: cannot parse issue number from `{}`",
                task_id.key
            ))
        })?;
    if task_repo != repo {
        return Err(GhError::Graphql(format!(
            "working claim: task key repository `{task_repo}` does not match `{repo}`"
        )));
    }
    Ok((owner, name, number))
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
        match self.merge_pr(node_id, expected_head_oid).await {
            Ok(()) => Ok(()),
            Err(err) => Err(mutation_provider_error(
                self.name_rule_violation(err, pr).await,
            )),
        }
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
            .map_err(mutation_provider_error)
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
            .map_err(mutation_provider_error)
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
            .map_err(mutation_provider_error)
    }

    /// List the accounts requestable as reviewers on the workspace's
    /// PR — the repo's assignable users plus GitHub's suggestions for
    /// this PR. Resolves `(owner, name, number)` from the PR task.
    async fn list_requestable_reviewers(
        &self,
        workspace: &lazybox_core::Workspace,
    ) -> Result<Vec<String>, lazybox_core::ProviderError> {
        let Some(pr) = workspace.pr.as_ref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no PR", workspace.key),
            ));
        };
        let Some(repo) = pr.repo.as_deref() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                "PR task has no repo",
            ));
        };
        let Some((owner, name)) = repo.split_once('/') else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("can't parse owner/name from `{repo}`"),
            ));
        };
        let Some(number) = pr.id.number() else {
            return Err(lazybox_core::ProviderError::permanent(
                "github",
                format!("can't parse PR number from `{}`", pr.id.key),
            ));
        };
        // A read that feeds the reviewer picker (not a mutation): the
        // client falls back to interaction-derived candidates the
        // instant this fails, so a rate limit here must surface at once
        // — never a `Retryable` whose "retrying" message the picker
        // path never honors.
        self.list_requestable_reviewers_for_pr(owner, name, number)
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
            .map_err(mutation_provider_error)
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
            .map_err(mutation_provider_error)
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
            .map_err(mutation_provider_error)
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
                .map_err(mutation_provider_error)?;
        }
        if !to_remove.is_empty() {
            self.remove_assignees(node_id, &to_remove)
                .await
                .map_err(mutation_provider_error)?;
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
            .map_err(mutation_provider_error)
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
        // A read that feeds the label picker (not a mutation): the client
        // falls back to the task's own labels the instant this fails, so a
        // rate limit here must surface at once — never a `Retryable` whose
        // "retrying" message the picker path never honors.
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
            .map_err(mutation_provider_error)?;
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
                .map_err(mutation_provider_error)?;
        }
        if !to_remove.is_empty() {
            self.remove_labels(node_id, &to_remove)
                .await
                .map_err(mutation_provider_error)?;
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

    /// Pagination + owner handling for `accessible_scopes`, against a
    /// mock server: page 1 carries a `Link: … rel="next"` header, so the
    /// loop must thread to page 2, and every scope must use the repo's
    /// real owner (a collaborator/org repo owned by someone other than
    /// the caller), not `test-user`.
    #[tokio::test]
    async fn accessible_scopes_threads_pages_and_keeps_real_owner() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let link_next = format!("<{base}/user/repos?page=2>; rel=\"next\"");
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let link = link_next.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // One read is enough for a bodyless GET on localhost.
                    if let Ok(read) = sock.read(&mut chunk).await {
                        request.extend_from_slice(&chunk[..read]);
                    }
                    let is_page2 = String::from_utf8_lossy(&request).contains("page=2");
                    let (body, link_header) = if is_page2 {
                        (
                            r#"[{"id":2,"name":"collab","url":"https://api.github.com/repos/someone-else/collab","full_name":"someone-else/collab"},{"id":3,"name":"widget","url":"https://api.github.com/repos/acme/widget","full_name":"acme/widget"}]"#.to_string(),
                            String::new(),
                        )
                    } else {
                        (
                            r#"[{"id":1,"name":"own","url":"https://api.github.com/repos/test-user/own","full_name":"test-user/own"}]"#.to_string(),
                            format!("Link: {link}\r\n"),
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/json\r\n\
                         {link_header}Content-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        let client = make_client(&base);
        let scopes: std::collections::BTreeSet<String> = client
            .accessible_scopes()
            .await
            .expect("mock returns a complete result")
            .into_iter()
            .collect();

        assert!(scopes.contains("github:test-user"), "own login: {scopes:?}");
        assert!(scopes.contains("github:test-user/own"), "page 1 repo");
        assert!(
            scopes.contains("github:someone-else/collab"),
            "collaborator repo keeps its real owner, not test-user: {scopes:?}"
        );
        assert!(
            scopes.contains("github:acme/widget"),
            "second page was threaded via the Link header: {scopes:?}"
        );
    }

    /// End-to-end: `accessible_scopes` returns the caller's own
    /// `github:<login>` plus a `github:<owner>/<repo>` for every repo
    /// they can access (owned / org-member / collaborator) — the set the
    /// poller unions into its scope allowlist. Ignored by default (needs
    /// a real `gh`-auth'd token); run with
    /// `cargo test -p lazybox-gh -- --ignored accessible_scopes`.
    #[tokio::test]
    #[ignore = "requires a real GitHub token (gh auth)"]
    async fn accessible_scopes_lists_owned_member_and_collaborator_repos() {
        let cred = crate::credential_chain()
            .resolve(crate::SOURCE)
            .await
            .expect("a GitHub token must resolve");
        let client = GhClient::from_credential(cred)
            .await
            .expect("client builds");
        let scopes = client
            .accessible_scopes()
            .await
            .expect("accessible scopes resolve");
        assert!(
            scopes
                .iter()
                .any(|s| s == &format!("github:{}", client.username())),
            "own login scope must be present; got {scopes:?}"
        );
        assert!(
            scopes.iter().any(|s| s.contains('/')),
            "at least one owner/repo scope expected for an authenticated user"
        );
    }

    fn gql_err(
        message: &str,
        error_type: Option<&str>,
        extensions: Option<&str>,
    ) -> graphql::GqlError {
        graphql::GqlError {
            message: message.to_string(),
            error_type: error_type.map(str::to_string),
            path: None,
            extensions: extensions.map(|e| serde_json::from_str(e).unwrap()),
            locations: None,
        }
    }

    /// A mutation whose GraphQL body carries a secondary rate-limit error
    /// (the #804 case) must lift to `RateLimited` — with the reset-window
    /// fallback — so the daemon queues + retries it instead of hard-failing.
    #[test]
    fn secondary_rate_limited_mutation_classifies_as_rate_limited() {
        let errors = vec![gql_err(
            "You have exceeded a secondary rate limit. Please wait a few minutes before you try again.",
            Some("RATE_LIMITED"),
            Some(r#"{"field":"rateLimit","typeName":"Mutation"}"#),
        )];
        match mutation_errors_to_gherror("mergePullRequest", &errors) {
            GhError::RateLimited {
                retry_after_secs,
                reason,
                self_throttle,
            } => {
                assert_eq!(retry_after_secs, MUTATION_RATE_LIMIT_DEFAULT_WAIT_SECS);
                assert!(
                    !self_throttle,
                    "a GitHub limit is not a governor self-throttle"
                );
                assert!(reason.contains("secondary rate limit"));
                assert!(
                    !reason.contains("typeName"),
                    "no serialized extensions blob: {reason}"
                );
                assert!(!reason.contains('{'));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        // …and it maps through to a Retryable ProviderError carrying the
        // reset hint, which is what the daemon's retry queue keys off.
        let pe = mutation_provider_error(mutation_errors_to_gherror("mergePullRequest", &errors));
        assert!(pe.is_retryable());
        assert_eq!(
            pe.retry_after_secs(),
            Some(MUTATION_RATE_LIMIT_DEFAULT_WAIT_SECS)
        );
    }

    /// A genuine, non-retryable mutation rejection renders one clean human
    /// sentence — never the raw GraphQL/JSON that #804 dumped in the footer.
    #[test]
    fn non_retryable_mutation_error_is_humanized() {
        let errors = vec![gql_err(
            "Pull request is not mergeable",
            Some("UNPROCESSABLE"),
            Some(r#"{"typeName":"Mutation"}"#),
        )];
        let err = mutation_errors_to_gherror("mergePullRequest", &errors);
        match &err {
            GhError::Graphql(reason) => {
                assert_eq!(reason, "Pull request is not mergeable");
                assert!(!reason.contains("typeName"));
            }
            other => panic!("expected Graphql, got {other:?}"),
        }
        let pe = mutation_provider_error(err);
        assert!(
            !pe.is_retryable(),
            "a real rejection is permanent, not retried"
        );
        let msg = pe.user_message();
        assert!(msg.contains("Pull request is not mergeable"));
        assert!(
            !msg.contains("typeName"),
            "footer message stays JSON-free: {msg}"
        );
        assert!(!msg.contains("(ext:"));
    }

    /// A GitHub error carrying no message text (`human()` empty) must still
    /// produce a non-blank reason — otherwise the footer reads
    /// "merge failed: github:" with nothing after it.
    #[test]
    fn empty_message_error_still_names_the_operation() {
        let errors = vec![gql_err("", None, Some(r#"{"typeName":"Mutation"}"#))];
        match mutation_errors_to_gherror("mergePullRequest", &errors) {
            GhError::Graphql(reason) => {
                assert!(!reason.trim().is_empty(), "reason must not be blank");
                assert!(
                    reason.contains("mergePullRequest"),
                    "names the op: {reason}"
                );
                assert!(!reason.contains("typeName"), "still JSON-free: {reason}");
            }
            other => panic!("expected Graphql, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rate_state_persists_and_restores_through_the_client() {
        let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
        let source =
            GhClient::stub_with_rate_limit_for_tests("cmd", "fp", 4200, 5000, reset_at).unwrap();
        let state = source.persisted_rate_state();
        assert_eq!(
            state
                .resources
                .get("graphql")
                .expect("graphql persisted")
                .remaining,
            4200
        );

        let fresh = GhClient::stub_for_tests("cmd", "fp").unwrap();
        fresh.restore_rate_state(state);
        let graphql = fresh
            .rate_snapshot()
            .resources
            .into_iter()
            .find(|r| r.resource == "graphql")
            .expect("graphql resource restored");
        assert_eq!(graphql.remaining, 4200);
        assert_eq!(graphql.limit, 5000);
    }

    #[tokio::test]
    async fn rate_state_payload_is_deduped_until_it_changes() {
        let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
        let client =
            GhClient::stub_with_rate_limit_for_tests("cmd", "fp", 4200, 5000, reset_at).unwrap();

        let first = client
            .pending_rate_state_payload()
            .expect("the first observed state is a change worth persisting");
        client.mark_rate_state_persisted(first.clone());
        // An idle tick with no budget change must not rewrite the blob.
        assert!(
            client.pending_rate_state_payload().is_none(),
            "unchanged state must dedupe to no write"
        );

        // A genuine budget change produces a fresh, different payload.
        let mut changed = client.persisted_rate_state();
        changed
            .resources
            .get_mut("graphql")
            .expect("graphql")
            .remaining = 3000;
        client.restore_rate_state(changed);
        let second = client
            .pending_rate_state_payload()
            .expect("a changed budget must persist again");
        assert_ne!(first, second);
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
        assert_eq!(outcome.pr_coverage, FetchCoverage::Complete);
        assert_eq!(outcome.coverage, FetchCoverage::Partial);

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
        assert_eq!(outcome.pr_coverage, FetchCoverage::Partial);
        assert_eq!(outcome.coverage, FetchCoverage::Partial);
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
    fn aggregate_rate_limit_keeps_the_longest_retry_window() {
        let error = combine_selected_fetches(
            true,
            true,
            Err(GhError::RateLimited {
                retry_after_secs: 1,
                reason: "PR budget blocked".into(),
                self_throttle: false,
            }),
            Err(GhError::RateLimited {
                retry_after_secs: 414,
                reason: "issue budget blocked".into(),
                self_throttle: false,
            }),
        )
        .expect_err("both requested sides failed");
        let provider_error = lazybox_core::ProviderError::from(error);

        assert_eq!(provider_error.retry_after_secs(), Some(414));
        assert!(provider_error.diagnostic().contains("both PR and issue"));
    }

    #[test]
    fn partial_success_preserves_the_failed_sides_retry_window() {
        let outcome = combine_selected_fetches(
            true,
            true,
            Ok(PrFetchOutcome::complete(Vec::new())),
            Err(GhError::RateLimited {
                retry_after_secs: 414,
                reason: "issue budget blocked".into(),
                self_throttle: false,
            }),
        )
        .expect("successful PR side keeps the partial result");

        assert_eq!(outcome.retry_after_secs, Some(414));
        assert_eq!(outcome.pr_coverage, FetchCoverage::Complete);
        assert_eq!(outcome.coverage, FetchCoverage::Partial);
    }

    #[test]
    fn aggregate_does_not_hide_a_non_rate_limit_failure() {
        let rate_limited = GhError::RateLimited {
            retry_after_secs: 414,
            reason: "budget blocked".into(),
            self_throttle: false,
        };
        let graphql = GhError::Graphql("query shape rejected".into());
        let error = GhError::aggregate(
            format!("{rate_limited}; {graphql}"),
            &[&rate_limited, &graphql],
        );

        assert!(matches!(error, GhError::Graphql(_)));
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
    fn pr_detail_prefetch_is_scheduled_while_user_fetch_stays_interactive() {
        let (_, user_priority) = request_profile("PR details lazy-fetch");
        let (_, prefetch_priority) = request_profile("PR details background prefetch");
        assert_eq!(
            user_priority,
            crate::rate_budget::RequestPriority::Interactive
        );
        assert_eq!(
            prefetch_priority,
            crate::rate_budget::RequestPriority::Recent
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
            .post_graphql_with_retry::<serde_json::Value>("test", &body)
            .await
            .expect_err("429 must fail");
        match &err {
            GhError::RateLimited {
                retry_after_secs,
                reason,
                self_throttle,
            } => {
                assert_eq!(*retry_after_secs, 7, "Retry-After header must be honored");
                assert!(reason.contains("429"), "reason names the status: {reason}");
                assert!(
                    !*self_throttle,
                    "a 429 is a GitHub-imposed limit, not a governor self-throttle"
                );
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
        // Budget fed: the cooldown refuses scheduled work until the
        // window, while ONE paced interactive request still passes
        // (#1218 item 5 — a 429 opened by background churn must not
        // refuse the user's own action).
        match client.budget.lock().admit(
            crate::rate_budget::ApiResource::Graphql,
            "post-429-background",
            crate::rate_budget::RequestPriority::Focused,
            1,
        ) {
            Err(crate::rate_budget::AcquireError::CircuitOpen { .. }) => {}
            other => panic!("scheduled work must be refused after a 429, got {other:?}"),
        }
        assert!(
            client.budget.lock().try_acquire().is_ok(),
            "one paced interactive request passes the cooldown",
        );
        match client.budget.lock().try_acquire() {
            Err(crate::rate_budget::AcquireError::CircuitOpen { .. }) => {}
            other => panic!("a second interactive inside the gap is paced, got {other:?}"),
        }
    }

    /// Recorded request latency must measure only GitHub's round-trip,
    /// never the governor's inter-request pacing sleep. The second
    /// request waits one baseline gap before firing, but that
    /// self-imposed wait must not inflate the latency percentiles an
    /// operator reads via `Shift-D` / `/v1/metrics` (#745). Fails if
    /// the timing clock starts before pacing/admission instead of at
    /// the network call.
    #[tokio::test(flavor = "current_thread")]
    async fn recorded_request_latency_excludes_the_pacing_wait() {
        const BODY: &str = r#"{"data":{"rateLimit":{"limit":5000,"remaining":4999,"used":1,"cost":1,"resetAt":"2999-01-01T00:00:00Z"}}}"#;
        let base_uri = spawn_canned_response_server("200 OK", "application/json", BODY).await;
        let client = make_client(&base_uri);
        let body = serde_json::json!({"query": "{ viewer { login } }"});

        // First request has no pacing debt; the second must wait one
        // baseline gap before it may fire.
        for _ in 0..2 {
            client
                .post_graphql_with_retry::<serde_json::Value>("test", &body)
                .await
                .expect("canned 200 must parse");
        }

        let gap_ms = crate::rate_budget::DEFAULT_MIN_REQUEST_GAP.as_millis() as u64;
        let p95 = client
            .rate_snapshot()
            .request_p95_ms
            .expect("two requests recorded latency");
        assert!(
            p95 < gap_ms,
            "recorded p95 latency {p95}ms must exclude the {gap_ms}ms pacing wait"
        );
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
            include_str!("../tests/fixtures/secondary_limit.json"),
            hits.clone(),
        )
        .await;
        let client = make_client(&base_uri);

        let body = serde_json::json!({"query": "{}"});
        let err = client
            .post_graphql_with_retry::<serde_json::Value>("test", &body)
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
            .post_graphql_with_retry::<serde_json::Value>("test", &body)
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
            .post_graphql_with_retry::<serde_json::Value>("test", &body)
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
        let h =
            RateLimitHeaders::parse(Some("120"), Some("0"), Some("1750000000"), None, None, None);
        assert_eq!(h.wait_secs(1749999000), 120);
    }

    #[test]
    fn rate_limit_headers_fall_back_to_reset_epoch() {
        let h = RateLimitHeaders::parse(None, Some("0"), Some("1750000090"), None, None, None);
        assert_eq!(h.wait_secs(1750000000), 90);
    }

    #[test]
    fn rate_limit_headers_default_when_absent_and_clamp_past_reset() {
        let none = RateLimitHeaders::parse(None, None, None, None, None, None);
        assert_eq!(none.wait_secs(1750000000), 60, "no hints → 60s default");
        let past = RateLimitHeaders::parse(None, None, Some("1749999000"), None, None, None);
        assert_eq!(
            past.wait_secs(1750000000),
            1,
            "reset in the past clamps to 1s"
        );
    }

    #[test]
    fn graphql_budget_reads_headers_only_for_the_graphql_resource() {
        // Mutations can't carry a body `rateLimit` block (issue #822), so
        // their budget refresh rides the `x-ratelimit-*` response headers.
        let graphql = RateLimitHeaders::parse(
            None,
            Some("4990"),
            Some("1750000000"),
            Some("graphql"),
            Some("5000"),
            Some("10"),
        );
        let (remote, used) = graphql
            .graphql_budget()
            .expect("graphql headers yield a remote budget");
        assert_eq!(remote.remaining, 4990);
        assert_eq!(remote.limit, 5000);
        assert_eq!(used, 10);
        assert_eq!(remote.reset_at.timestamp(), 1750000000);

        // A core/search-billed response must not be read as the graphql
        // budget, and a graphql response missing any field yields nothing.
        let core = RateLimitHeaders::parse(
            None,
            Some("59"),
            Some("1750000000"),
            Some("core"),
            Some("60"),
            Some("1"),
        );
        assert!(core.graphql_budget().is_none());
        let partial = RateLimitHeaders::parse(
            None,
            Some("4990"),
            None,
            Some("graphql"),
            Some("5000"),
            None,
        );
        assert!(partial.graphql_budget().is_none());
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

    async fn spawn_recording_response_server(
        bodies: Vec<&'static str>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(_) => continue,
                };
                let body = bodies[served.min(bodies.len() - 1)];
                served += 1;
                let requests = requests.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let read = sock.read(&mut buf).await.unwrap_or(0);
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[..read]).into_owned());
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    async fn spawn_sequenced_http_response_server(
        responses: Vec<(&'static str, &'static str, &'static str, &'static str)>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(_) => continue,
                };
                let response = responses[served.min(responses.len() - 1)];
                served += 1;
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let (status, content_type, extra_headers, body) = response;
                    let response = format!(
                        "HTTP/1.1 {status}\r\n\
                         Content-Type: {content_type}\r\n\
                         {extra_headers}Content-Length: {}\r\n\
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
            last_persisted_rate_state: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            request_gate: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
            mutation_gate: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            notifications_state: NotificationsState::shared(),
            hot_freshness: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    fn task_without_node_id(kind: TaskKind) -> Task {
        let (key, url) = match kind {
            TaskKind::Pr => ("o/r#1", "https://github.com/o/r/pull/1"),
            TaskKind::Issue => ("o/r#2", "https://github.com/o/r/issues/2"),
        };
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".to_string(),
                key: key.to_string(),
            },
            title: "Task without a cached node id".to_string(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: url.to_string(),
            repo: Some("o/r".to_string()),
            branch: Some("topic".to_string()),
            base_branch: Some("main".to_string()),
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Unknown,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            kind: Some(kind),
            priority: None,
            state_label: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mutations_require_cached_node_ids_before_network_io() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_uri =
            spawn_counting_response_server("200 OK", "application/json", "", "{}", hits.clone())
                .await;
        let client = make_client(&base_uri);

        let pr = Workspace::from_task(task_without_node_id(TaskKind::Pr), chrono::Utc::now());
        let error = TaskProvider::merge(&client, &pr, None)
            .await
            .expect_err("a PR mutation requires the node id cached by polling");
        assert_eq!(
            error,
            ProviderError::permanent("github", "PR has no node_id (poll first)")
        );

        let issue = Workspace::from_task(task_without_node_id(TaskKind::Issue), chrono::Utc::now());
        let error = TaskProvider::close_issue(&client, &issue)
            .await
            .expect_err("an issue mutation requires the node id cached by polling");
        assert_eq!(
            error,
            ProviderError::permanent("github", "issue has no node_id (poll first)")
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "missing cached node ids must fail before provider IO"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hot_tasks_probe_gates_the_full_fetch() {
        // Lean freshness probe: one visible issue, one gone.
        const LEAN: &str = r#"{
          "data": {
            "nodes": [
              {"__typename": "Issue", "id": "I_one", "updatedAt": "2026-07-25T10:00:00Z", "state": "OPEN", "stateReason": null},
              null
            ],
            "rateLimit": {"cost": 1, "limit": 5000, "remaining": 4999, "resetAt": "2026-07-25T11:00:00Z"}
          }
        }"#;
        // Same probe with a bumped updatedAt — the node "moved".
        const LEAN_CHANGED: &str = r#"{
          "data": {
            "nodes": [
              {"__typename": "Issue", "id": "I_one", "updatedAt": "2026-07-25T10:30:00Z", "state": "OPEN", "stateReason": null},
              null
            ],
            "rateLimit": {"cost": 1, "limit": 5000, "remaining": 4999, "resetAt": "2026-07-25T11:00:00Z"}
          }
        }"#;
        // Full detail for the single mover.
        const FULL_ONE: &str = r#"{
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
              }
            ],
            "rateLimit": {"cost": 2, "limit": 5000, "remaining": 4998, "resetAt": "2026-07-25T11:00:00Z"}
          }
        }"#;
        let base_uri = spawn_sequenced_response_server(vec![
            LEAN,         // call 1: probe (first sight → mover)
            FULL_ONE,     //         full detail for I_one
            LEAN,         // call 2: probe unchanged → NO full request
            LEAN_CHANGED, // call 3: probe moved
            FULL_ONE,     //         full detail again
            LEAN,         // call 4 (after force_full_sweep): probe
            FULL_ONE,     //         cache cleared → full again
        ])
        .await;
        let client = make_client(&base_uri);
        let ids: Vec<String> = vec!["I_one".into(), "I_gone".into()];

        // First sight: probe + full, ordered slots.
        let out = client.fetch_hot_tasks(&ids).await.expect("hot batch");
        assert_eq!(out.len(), 2);
        match &out[0] {
            HotFetch::Fresh(task) => {
                assert_eq!(task.id.key, "acme/widget#7");
                assert_eq!(task.node_id.as_deref(), Some("I_one"));
                assert!(!task.is_pr());
            }
            other => panic!("expected Fresh, got {other:?}"),
        }
        assert!(matches!(out[1], HotFetch::Missing));

        // Unchanged probe: the ~700-node full query is NOT spent.
        let out = client.fetch_hot_tasks(&ids).await.expect("unchanged tick");
        assert!(matches!(out[0], HotFetch::Unchanged));
        assert!(matches!(out[1], HotFetch::Missing));

        // Moved probe: full detail is fetched again.
        let out = client.fetch_hot_tasks(&ids).await.expect("changed tick");
        assert!(matches!(out[0], HotFetch::Fresh(_)));

        // Shift-R semantics: an explicit refresh drops the fingerprints,
        // so even a byte-identical probe re-fetches full detail.
        client.force_full_sweep();
        let out = client.fetch_hot_tasks(&ids).await.expect("forced tick");
        assert!(matches!(out[0], HotFetch::Fresh(_)));
    }

    /// GraphQL rejects `nodes(ids:)` past 100 outright — a 150-id hot
    /// set must be probed in two chunks instead of erroring the whole
    /// refresh (#1218).
    #[tokio::test(flavor = "current_thread")]
    async fn hot_probe_chunks_batches_past_the_graphql_id_cap() {
        fn lean_nulls(count: usize) -> &'static str {
            let nodes = vec!["null"; count].join(",");
            let body = format!(
                r#"{{"data": {{"nodes": [{nodes}], "rateLimit": {{"cost": 1, "limit": 5000, "remaining": 4999, "resetAt": "2026-07-25T11:00:00Z"}}}}}}"#,
            );
            Box::leak(body.into_boxed_str())
        }
        let base_uri = spawn_sequenced_response_server(vec![lean_nulls(100), lean_nulls(50)]).await;
        let client = make_client(&base_uri);
        let ids: Vec<String> = (0..150).map(|i| format!("I_{i}")).collect();

        let out = client.fetch_hot_tasks(&ids).await.expect("chunked probe");
        assert_eq!(out.len(), 150);
        assert!(out.iter().all(|slot| matches!(slot, HotFetch::Missing)));
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
    async fn later_page_rate_limit_preserves_retry_contract() {
        let page1: &'static str =
            Box::leak(pr_search_page(1, Some((true, Some("CUR1")))).into_boxed_str());
        let base_uri = spawn_sequenced_http_response_server(vec![
            ("200 OK", "application/json", "", page1),
            (
                "429 Too Many Requests",
                "application/json",
                "Retry-After: 37\r\n",
                r#"{"message":"API rate limit exceeded"}"#,
            ),
        ])
        .await;
        let client = make_client(&base_uri);

        let error = client
            .fetch_pr_single_query("test-branch", "is:open is:pr repo:o/r".to_string())
            .await
            .expect_err("the second-page rate limit must fail the branch");
        assert!(
            matches!(
                &error,
                GhError::RateLimited {
                    retry_after_secs: 37,
                    ..
                }
            ),
            "got {error:?}"
        );

        let provider_error = lazybox_core::ProviderError::from(error);
        assert!(provider_error.is_retryable());
        assert_eq!(provider_error.retry_after_secs(), Some(37));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn twentieth_page_missing_metadata_is_not_reported_as_the_page_cap() {
        let mut pages = Vec::new();
        for number in 1..20 {
            let cursor = format!("CUR{number}");
            let cursor: &'static str = Box::leak(cursor.into_boxed_str());
            let page: &'static str =
                Box::leak(pr_search_page(number, Some((true, Some(cursor)))).into_boxed_str());
            pages.push(page);
        }
        pages.push(Box::leak(pr_search_page(20, None).into_boxed_str()));
        let base_uri = spawn_sequenced_response_server(pages).await;
        let client = make_client(&base_uri);

        let error = client
            .fetch_pr_single_query("test-branch", "is:open is:pr repo:o/r".to_string())
            .await
            .expect_err("missing metadata on page 20 must fail");

        assert!(
            matches!(error, GhError::Graphql(message) if message.contains("omitted pageInfo")),
            "page 20 was malformed; reaching it was not page-cap exhaustion"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn partial_repo_fanout_reports_non_authoritative_pr_coverage() {
        let base_uri = spawn_repo_routing_server().await;
        let client = make_client(&base_uri);
        let wall_now = chrono::Utc::now();
        client
            .budget
            .lock()
            .observe(crate::rate_budget::RemoteRateLimit {
                remaining: 5000,
                limit: 5000,
                reset_at: wall_now + chrono::Duration::hours(1),
                observed_at: std::time::Instant::now(),
            });
        client.begin_background_tick(std::time::Duration::from_secs(60));

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
        assert_eq!(outcome.pr_coverage, FetchCoverage::Partial);
        assert_eq!(outcome.coverage, FetchCoverage::Partial);
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
            .post_graphql_with_retry::<serde_json::Value>("test", &body)
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
            .post_graphql_with_retry::<serde_json::Value>("test", &body)
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
            .post_graphql_with_retry::<serde_json::Value>("test", &body)
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
            .post_graphql_with_retry_measured::<serde_json::Value>("test", &body)
            .await
            .expect("canned 2xx JSON should parse");
        assert_eq!(
            bytes,
            BODY.len(),
            "reported byte length must equal the raw response body length"
        );
        assert_eq!(value["data"]["hello"], "world");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn budget_bootstrap_turns_an_unknown_graphql_budget_into_a_current_observation() {
        const BODY: &str = r#"{
          "data": {
            "viewer": { "login": "test-user" },
            "rateLimit": {
              "cost": 1,
              "limit": 5000,
              "remaining": 4321,
              "resetAt": "2026-08-01T12:00:00Z",
              "used": 679
            }
          }
        }"#;
        let base_uri = spawn_canned_response_server("200 OK", "application/json", BODY).await;
        let client = make_client(&base_uri);
        assert!(client.rate_snapshot().remote.is_none());

        client
            .bootstrap_graphql_budget()
            .await
            .expect("budget bootstrap");

        let remote = client
            .rate_snapshot()
            .remote
            .expect("GraphQL budget observation");
        assert_eq!(remote.remaining, 4321);
        assert_eq!(remote.limit, 5000);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn captured_graphql_fixture_reconciles_forecast_and_actual_cost() {
        let response_body = include_str!("../tests/fixtures/graphql_rate_limit.json");
        let base_uri =
            spawn_canned_response_server("200 OK", "application/json", response_body).await;
        let client = make_client(&base_uri);
        let body = serde_json::json!({"query": "query { rateLimit { cost } }"});

        let _: serde_json::Value = client
            .post_graphql_with_retry("fixture-query", &body)
            .await
            .expect("fixture response");
        let snapshot = client.rate_snapshot();
        let operation = snapshot
            .operations
            .iter()
            .find(|operation| operation.class == "fixture-query")
            .expect("fixture operation accounted");
        assert_eq!(operation.last_actual, Some(3));
        assert_eq!(operation.forecast, 3);
        assert_eq!(snapshot.total.graphql_points, 3);
        assert_eq!(snapshot.total.requests, 1);
        assert_eq!(snapshot.resources[0].resource, "graphql");
    }

    #[tokio::test]
    async fn background_sweep_forecast_uses_observed_operation_costs() {
        let client = make_client("http://127.0.0.1:1")
            .with_watch_repos(vec!["owner/one".to_string(), "owner/two".to_string()]);
        let wall_now = chrono::Utc::now();
        let mono_now = std::time::Instant::now();
        client.budget.lock().observe_graphql_response(
            "watched-repo",
            crate::rate_budget::RemoteRateLimit {
                remaining: 4998,
                limit: 5000,
                reset_at: wall_now + chrono::Duration::hours(1),
                observed_at: mono_now,
            },
            2,
            2,
            200,
            0,
            std::time::Duration::ZERO,
        );

        assert_eq!(
            client.background_sweep_forecast(true, true),
            BackgroundSweepForecast {
                global_points: 8,
                repo_base_points: 3,
                per_repo_points: 1,
            }
        );
        assert_eq!(
            client.background_sweep_forecast(false, true),
            BackgroundSweepForecast {
                global_points: 1,
                repo_base_points: 1,
                per_repo_points: 0,
            }
        );

        client.budget.lock().note_expected_pages("watched-repo", 3);
        let forecast = client.background_sweep_forecast(true, false);
        assert_eq!(
            forecast,
            BackgroundSweepForecast {
                global_points: 15,
                repo_base_points: 2,
                per_repo_points: 1,
            }
        );
        assert_eq!(forecast.required_points(true, true), 15);
        assert_eq!(forecast.required_points(false, true), 3);
        assert_eq!(forecast.repo_capacity(5, false, 3), 3);
        assert_eq!(forecast.repo_capacity(5, true, 3), 0);
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

    /// Issue #998: branch-rule types map to short human names for the
    /// merge-blocked notice. Merge-relevant rules get a friendly name
    /// (the `pull_request` rule folds in the required approval count);
    /// ref-shape rules that never block a merge are dropped; an unknown
    /// type falls through prettified rather than vanishing.
    #[test]
    fn humanize_rule_maps_merge_relevant_types() {
        let approvals = serde_json::json!({ "required_approving_review_count": 2 });
        assert_eq!(
            humanize_rule("pull_request", Some(&approvals)),
            Some("2 approving reviews".to_string())
        );
        assert_eq!(
            humanize_rule("pull_request", None),
            Some("pull request review".to_string())
        );
        assert_eq!(
            humanize_rule("required_signatures", None),
            Some("signed commits".to_string())
        );
        assert_eq!(
            humanize_rule("required_linear_history", None),
            Some("linear history".to_string())
        );
        // Ref-shape rules don't gate a merge — omit them.
        assert_eq!(humanize_rule("non_fast_forward", None), None);
        assert_eq!(humanize_rule("deletion", None), None);
        // Unknown-but-possibly-relevant rule: surface it prettified.
        assert_eq!(
            humanize_rule("code_scanning", None),
            Some("code scanning".to_string())
        );
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

    /// Issue #822: a successful mutation carries no body `rateLimit`
    /// block (it's a Query-root-only field), so its GraphQL budget
    /// refresh must ride the `x-ratelimit-*` response headers GitHub
    /// sends on every call. Before the header fallback existed a
    /// successful mutation updated no primary budget at all, so the
    /// `graphql` resource stayed absent — this asserts it now carries
    /// the header-reported window.
    #[tokio::test(flavor = "current_thread")]
    async fn mutation_success_refreshes_budget_from_headers() {
        const BODY: &str =
            r#"{"data":{"updatePullRequestBranch":{"pullRequest":{"id":"PR_kwDO"}}}}"#;
        // A far-future reset keeps the observation inside a live window
        // without depending on the wall clock; GitHub bills GraphQL calls
        // against the `graphql` resource.
        const HEADERS: &str = "x-ratelimit-resource: graphql\r\n\
                               x-ratelimit-remaining: 4990\r\n\
                               x-ratelimit-limit: 5000\r\n\
                               x-ratelimit-reset: 4102444800\r\n\
                               x-ratelimit-used: 10\r\n";
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_uri =
            spawn_counting_response_server("200 OK", "application/json", HEADERS, BODY, hits).await;
        let client = make_client(&base_uri);

        client
            .update_branch("PR_kwDO")
            .await
            .expect("update-branch success must not report a false failure");

        let graphql = client
            .persisted_rate_state()
            .resources
            .remove("graphql")
            .expect("a successful mutation must refresh the graphql budget from its headers");
        assert_eq!(graphql.remaining, 4990);
        assert_eq!(graphql.limit, 5000);
        assert_eq!(graphql.used, 10);
        assert_eq!(graphql.reset_at.timestamp(), 4102444800);
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

    #[tokio::test(flavor = "current_thread")]
    async fn working_claim_creates_missing_qualified_label_then_applies_it() {
        const LABEL: &str = "lazybox:w:0123456789abcdef0123:1234567890:00000001";
        const CREATED: &str = r#"{
            "id": 1,
            "node_id": "LA_1",
            "url": "https://api.github.test/repos/o/r/labels/lazybox",
            "name": "lazybox:w:0123456789abcdef0123:1234567890:ffffffff",
            "description": "Claimed by a lazybox agent",
            "color": "fbca04",
            "default": false
        }"#;
        const APPLIED: &str = r#"[{
            "id": 1,
            "node_id": "LA_1",
            "url": "https://api.github.test/repos/o/r/labels/lazybox",
            "name": "lazybox:w:0123456789abcdef0123:1234567890:ffffffff",
            "description": "Claimed by a lazybox agent",
            "color": "fbca04",
            "default": false
        }]"#;
        let base_uri = spawn_sequenced_response_server(vec!["[]", CREATED, APPLIED]).await;
        let client = make_client(&base_uri);
        let task = task_without_node_id(TaskKind::Issue);

        client
            .sync_working_claim_target(
                &task.id,
                task.repo.as_deref().unwrap(),
                Some(LABEL),
                "0123456789abcdef0123",
                "1234567890",
            )
            .await
            .expect("a fresh repository must create and apply the coordination label");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn working_claim_clear_is_idempotent_when_repo_has_no_label() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_uri =
            spawn_counting_response_server("200 OK", "application/json", "", "[]", hits.clone())
                .await;
        let client = make_client(&base_uri);
        let task = task_without_node_id(TaskKind::Issue);

        client
            .sync_working_claim_target(
                &task.id,
                task.repo.as_deref().unwrap(),
                None,
                "0123456789abcdef0123",
                "1234567890",
            )
            .await
            .expect("clearing a missing claim is already success");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clearing_one_machine_claim_never_removes_the_racing_machine() {
        const ATTACHED: &str = r#"[
          {"id":1,"node_id":"LA_1","url":"https://api.github.test/repos/o/r/labels/one","name":"lazybox:w:0123456789abcdef0123:1234567890:ffffffff","description":null,"color":"fbca04","default":false},
          {"id":2,"node_id":"LA_2","url":"https://api.github.test/repos/o/r/labels/two","name":"lazybox:w:fedcba9876543210fedc:aaaaaaaaaa:ffffffff","description":null,"color":"fbca04","default":false},
          {"id":3,"node_id":"LA_3","url":"https://api.github.test/repos/o/r/labels/working","name":"working","description":null,"color":"fbca04","default":false}
        ]"#;
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_uri =
            spawn_recording_response_server(vec![ATTACHED, "[]"], requests.clone()).await;
        let client = make_client(&base_uri);
        let task = task_without_node_id(TaskKind::Issue);

        client
            .sync_working_claim_target(
                &task.id,
                task.repo.as_deref().unwrap(),
                None,
                "0123456789abcdef0123",
                "1234567890",
            )
            .await
            .expect("one owner can release while the racing owner remains");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "one list plus one owner-specific delete");
        assert!(requests[1].contains("1234567890"), "{}", requests[1]);
        assert!(!requests[1].contains("aaaaaaaaaa"), "{}", requests[1]);
        // Release must delete the repository-level *definition*, not merely
        // detach the label from the issue — a detach-only release leaks one
        // dead label into the repo's label picker per agent spawn.
        assert!(requests[1].starts_with("DELETE "), "{}", requests[1]);
        assert!(!requests[1].contains("/issues/"), "{}", requests[1]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn working_claim_rejects_non_github_tasks_before_http() {
        let client = make_client("http://127.0.0.1:1");
        let mut task = task_without_node_id(TaskKind::Issue);
        task.id.source = "linear".to_string();

        let error = client
            .sync_working_claim_target(
                &task.id,
                task.repo.as_deref().unwrap(),
                None,
                "0123456789abcdef0123",
                "1234567890",
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("not GitHub"), "{error}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn working_claim_rejects_mismatched_repository_identity_before_http() {
        let client = make_client("http://127.0.0.1:1");
        let mut task = task_without_node_id(TaskKind::Issue);
        task.repo = Some("other/repository".to_string());

        let error = client
            .sync_working_claim_target(
                &task.id,
                task.repo.as_deref().unwrap(),
                None,
                "0123456789abcdef0123",
                "1234567890",
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("does not match"), "{error}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_claim_cleanup_refuses_the_legacy_label_before_http() {
        let client = make_client("http://127.0.0.1:1");
        let task = task_without_node_id(TaskKind::Issue);

        let error = client
            .remove_working_claim_labels_target(
                &task.id,
                task.repo.as_deref().unwrap(),
                &[lazybox_core::WORKING_LABEL_NAME.to_string()],
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("refusing malformed or legacy label"),
            "{error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_claim_cleanup_removes_the_exact_qualified_label() {
        const LABEL: &str = "lazybox:w:0123456789abcdef0123:1234567890:ffffffff";
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_uri = spawn_recording_response_server(vec!["[]"], requests.clone()).await;
        let client = make_client(&base_uri);
        let task = task_without_node_id(TaskKind::Issue);

        client
            .remove_working_claim_labels_target(
                &task.id,
                task.repo.as_deref().unwrap(),
                &[LABEL.to_string()],
            )
            .await
            .expect("an expired qualified label can be removed idempotently");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("1234567890"), "{}", requests[0]);
        // Expiry cleanup deletes the repo-level definition so expired leases
        // never accumulate in the repo's label picker.
        assert!(requests[0].starts_with("DELETE "), "{}", requests[0]);
        assert!(!requests[0].contains("/issues/"), "{}", requests[0]);
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
            .post_graphql_with_retry::<graphql::GqlResponse>("test", &body)
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
                        format!(
                            "HTTP/1.1 304 Not Modified\r\n\
                         X-RateLimit-Resource: core\r\n\
                         X-RateLimit-Limit: 5000\r\n\
                         X-RateLimit-Remaining: 4999\r\n\
                         X-RateLimit-Used: 1\r\n\
                         X-RateLimit-Reset: {}\r\n\
                         Connection: close\r\n\r\n",
                            (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp()
                        )
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Last-Modified: {last_modified}\r\n\
                             X-RateLimit-Resource: core\r\n\
                             X-RateLimit-Limit: 5000\r\n\
                             X-RateLimit-Remaining: 4999\r\n\
                             X-RateLimit-Used: 1\r\n\
                             X-RateLimit-Reset: {}\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
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

    #[tokio::test(flavor = "current_thread")]
    async fn notifications_transport_failure_is_accounted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind unused port");
        let addr = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept heartbeat");
            drop(socket);
        });
        let client = make_client(&format!("http://{addr}"));
        client.begin_background_tick(std::time::Duration::from_secs(60));

        client
            .fetch_notifications()
            .await
            .expect_err("closed port must fail");

        let snapshot = client.rate_snapshot();
        assert_eq!(snapshot.total.requests, 1);
        assert_eq!(snapshot.total.rest_points, 1);
        let operation = snapshot
            .operations
            .iter()
            .find(|operation| operation.class == "notifications heartbeat")
            .expect("failed heartbeat accounted by operation");
        assert_eq!(operation.last_actual, Some(1));
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
        const BODY: &str = include_str!("../tests/fixtures/notifications.json");
        let base_uri = spawn_conditional_notifications_server(LAST_MODIFIED, BODY).await;
        let client = make_client(&base_uri);

        // Tick 1: a fresh 200 lists PR #700 and hands the pending cursor
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
        let snapshot = client.rate_snapshot();
        let core = snapshot
            .resources
            .iter()
            .find(|resource| resource.resource == "core")
            .expect("notification REST bucket accounted");
        assert_eq!((core.remaining, core.limit, core.used), (4999, 5000, 1));

        // Tick 2 WITHOUT committing — this is the un-fetched entry's
        // retry. The heartbeat still sends no `If-Modified-Since`, so
        // GitHub re-serves the 200 and PR #700 re-lists rather than being
        // lost to a premature 304.
        client.begin_background_tick(std::time::Duration::from_secs(60));
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
        client.begin_background_tick(std::time::Duration::from_secs(60));
        let poll3 = client.fetch_notifications().await.expect("third poll ok");
        assert!(
            matches!(poll3, NotificationsPoll::NotModified),
            "a committed cursor reaches the 304 steady state",
        );
    }

    /// #825: waiting on our own governor must never hang forever. With
    /// every concurrency slot occupied (a stand-in for #782's governor
    /// self-starvation, where a slot/budget never frees), `request_permit`
    /// gives up after `PERMIT_WAIT_TIMEOUT` and fails fast with a
    /// self-throttle rate limit instead of blocking indefinitely. Runs on
    /// paused time so the virtual `PERMIT_WAIT_TIMEOUT` elapses instantly.
    #[tokio::test(start_paused = true)]
    async fn request_permit_gives_up_when_slot_never_frees() {
        let client = GhClient::stub_for_tests("cmd:test", "fp").unwrap();

        // Take every concurrency slot and hold them, so the gate acquire
        // inside `request_permit` can never succeed.
        let mut held = Vec::new();
        for _ in 0..8 {
            held.push(
                client
                    .request_gate
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("gate open"),
            );
        }

        let start = tokio::time::Instant::now();
        let err = client
            .request_permit()
            .await
            .expect_err("permit acquire must fail fast, not hang");
        let waited = start.elapsed();

        match err {
            GhError::RateLimited {
                self_throttle,
                retry_after_secs,
                ..
            } => {
                assert!(self_throttle, "our own governor stalled → self-throttle");
                assert_eq!(retry_after_secs, PERMIT_WAIT_TIMEOUT.as_secs());
            }
            other => panic!("expected a bounded self-throttle, got {other:?}"),
        }
        assert!(
            waited <= PERMIT_WAIT_TIMEOUT + std::time::Duration::from_secs(1),
            "the wait was bounded by PERMIT_WAIT_TIMEOUT, took {waited:?}",
        );
        drop(held);
    }

    /// A blown operation deadline (#825) is transient: it retries on the
    /// next tick, and — critically for merge/update-branch — a timed-out
    /// mutation is re-drivable, never a permanent rejection.
    #[test]
    fn operation_timeout_is_retryable_not_permanent() {
        let err = GhError::Timeout {
            operation: "PR search",
            after_secs: 90,
        };
        assert_eq!(err.to_string(), "PR search timed out after 90s");

        let provider: lazybox_core::ProviderError = err.into();
        assert!(provider.is_retryable(), "a blown deadline retries");
        assert!(!provider.is_auth(), "never an auth verdict");

        let mutation = mutation_provider_error(GhError::Timeout {
            operation: "mergePullRequest mutation",
            after_secs: 90,
        });
        assert!(
            mutation.is_retryable(),
            "a timed-out mutation is re-drivable, not permanently rejected",
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
