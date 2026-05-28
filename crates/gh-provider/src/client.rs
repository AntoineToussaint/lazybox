use octocrab::Octocrab;
use pilot_auth::Credential;
use pilot_core::*;

use crate::graphql;

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
/// `HttpStatus` retries on 502/503/504 + 429 + any 2xx with a
/// non-JSON content-type (proxy/CDN serving a maintenance page),
/// matching what `From<GhError> for ProviderError` classifies as
/// `Retryable`. Auth (401/403), other 4xx, and 2xx-JSON parse
/// failures (real schema mismatches) are not retried.
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
            if matches!(*status, 502..=504) || *status == 429 {
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

impl From<GhError> for pilot_core::ProviderError {
    /// Classify GitHub failures so polling knows whether to retry.
    /// Heuristics:
    /// - 401/403 only when the GitHub API itself returned that status →
    ///   Auth (user needs to rotate token).
    /// - Hyper/Service/IO/Json variants → Retryable (transient).
    /// - 5xx, network-y words, "rate limit" → Retryable.
    /// - Everything else → Permanent.
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
            return pilot_core::ProviderError::retryable_after(SOURCE, detail, *retry_after_secs);
        }

        // Status-aware classification when we have an octocrab
        // GitHub error: 401/403 → auth; 5xx + 429 → retryable. This
        // is the ONLY path that mints `Auth` — substring matching for
        // "unauthorized"/"forbidden" produced false positives on
        // transient hyper/json errors that happen to mention either
        // word in their message chains.
        if let GhError::Api(octocrab::Error::GitHub { source, .. }) = &err {
            let status = source.status_code.as_u16();
            if status == 401 || status == 403 {
                return pilot_core::ProviderError::auth(SOURCE, detail);
            }
            if status == 429 || (500..=599).contains(&status) {
                return pilot_core::ProviderError::retryable(SOURCE, detail);
            }
            return pilot_core::ProviderError::permanent(SOURCE, detail);
        }

        // Same status-aware classification for `HttpStatus`, the
        // variant emitted by the raw GraphQL path. 2xx + non-JSON
        // is treated as retryable: it almost always means a proxy /
        // CDN intercepted the call with an HTML maintenance page
        // even though the upstream eventually came back.
        if let GhError::HttpStatus {
            status,
            content_type,
            ..
        } = &err
        {
            if *status == 401 || *status == 403 {
                return pilot_core::ProviderError::auth(SOURCE, detail);
            }
            if *status == 429 || (500..=599).contains(status) {
                return pilot_core::ProviderError::retryable(SOURCE, detail);
            }
            if (200..=299).contains(status) && !content_type_is_json(content_type) {
                return pilot_core::ProviderError::retryable(SOURCE, detail);
            }
            return pilot_core::ProviderError::permanent(SOURCE, detail);
        }

        // Variant-aware classification: every transport-layer variant
        // is retryable by definition (no PR/issue data was ever
        // returned, so a fresh attempt next tick is safe and likely
        // to succeed).
        if let GhError::Api(api) = &err
            && matches!(
                api,
                octocrab::Error::Hyper { .. }
                    | octocrab::Error::Service { .. }
                    | octocrab::Error::Http { .. }
                    | octocrab::Error::Serde { .. }
                    | octocrab::Error::Json { .. }
                    | octocrab::Error::UriParse { .. }
                    | octocrab::Error::Uri { .. }
            )
        {
            return pilot_core::ProviderError::retryable(SOURCE, detail);
        }

        // Fallback string matching for everything else (GraphQL
        // wrapper errors, future octocrab variants, etc.).
        let lower = detail.to_lowercase();
        let is_retryable = lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("connection")
            || lower.contains("network")
            || lower.contains("rate limit")
            || lower.contains("hyper")
            || lower.contains("502")
            || lower.contains("503")
            || lower.contains("504")
            || lower.contains("temporarily");
        if is_retryable {
            return pilot_core::ProviderError::retryable(SOURCE, detail);
        }

        pilot_core::ProviderError::permanent(SOURCE, detail)
    }
}

#[derive(Clone)]
pub struct GhClient {
    inner: Octocrab,
    user: String,
    credential_source: String,
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
    budget: std::sync::Arc<std::sync::Mutex<crate::rate_budget::RateBudget>>,
}

impl GhClient {
    pub async fn from_credential(cred: Credential) -> Result<Self, GhError> {
        let source = cred.source.clone();
        // Disable octocrab's built-in retry: its `OctoBody` clone only
        // Arc-clones a single-use body stream, so on a 429/5xx retry the
        // second attempt goes out with an empty `{}` body. GitHub answers
        // with the infamous "A query attribute must be specified and must
        // be a string" — ~1 in every 5 GraphQL polls during rate-limited
        // periods. We eat the retry feature; polling runs every few seconds
        // so we just try again on the next tick.
        let inner = Octocrab::builder()
            .personal_token(cred.into_token())
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .build()
            .map_err(GhError::Api)?;
        let user = inner.current().user().await.map_err(GhError::Api)?.login;
        Ok(Self {
            inner,
            user,
            credential_source: source,
            pr_filters: vec![],
            issue_filters: vec![],
            watch_repos: vec![],
            budget: std::sync::Arc::new(std::sync::Mutex::new(
                crate::rate_budget::RateBudget::default_for_pilot(),
            )),
        })
    }

    /// Snapshot of the current rate budget state. Used by the polling
    /// layer to surface a status indicator and decide pacing.
    pub fn rate_snapshot(&self) -> crate::rate_budget::Snapshot {
        self.budget
            .lock()
            .expect("budget mutex poisoned")
            .snapshot()
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
        self.budget
            .lock()
            .expect("budget mutex poisoned")
            .try_acquire()
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
    /// - 502 / 503 / 504, 429 → retry.
    /// - 2xx with a non-JSON body → retry (proxy/CDN bait).
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
        const DELAYS_MS: &[u64] = &[200, 800];
        // Per-request wall-clock cap. The default reqwest client has
        // no timeout — a flaky network can leave the HTTP call
        // hanging forever, which the user perceives as "pilot's
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
    async fn post_graphql_once<T>(&self, body: &serde_json::Value) -> Result<T, GhError>
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
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let raw_body = self
            .inner
            .body_to_string(response)
            .await
            .map_err(GhError::Api)?;
        // Non-2xx or non-JSON: never attempt to parse — the body is
        // an HTML page / login redirect / GitHub error JSON we'd
        // rather surface verbatim than mis-deserialise.
        if !(200..=299).contains(&status) || !content_type_is_json(&content_type) {
            return Err(http_status_error(status, &content_type, &raw_body));
        }
        // 2xx + JSON content-type: this is the success path. A parse
        // failure here is a real schema mismatch between our types
        // and GitHub's response — surface it with status + content-
        // type intact instead of dropping to `Serde`.
        serde_json::from_str::<T>(&raw_body).map_err(|e| GhError::HttpStatus {
            status,
            reason: " (json parse failed)".to_string(),
            content_type,
            body_excerpt: format!("{e} — body: {}", body_excerpt(&raw_body)),
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
        if let Ok(mut b) = self.budget.lock() {
            b.observe(observed);
        }
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

    /// Fetch ALL relevant PRs in a single GraphQL query.
    /// `involves:username` covers author, reviewer, assignee, mentioned.
    /// **One API call instead of 68.**
    pub fn authenticated_user(&self) -> &str {
        &self.user
    }

    pub async fn fetch_all_prs(&self) -> Result<Vec<Task>, GhError> {
        // Per-call wall-clock timer so the log can quantify the
        // parallelization win and so a regression jumps out in
        // `grep "fetch_all_prs: completed" /tmp/pilot.log`. Cheap;
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

        // Branch 2: recently-merged sweep.
        let week_ago = (chrono::Utc::now() - chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string();
        let mut merged_quals = vec![
            "is:pr".to_string(),
            "is:merged".to_string(),
            "archived:false".to_string(),
            format!("merged:>={week_ago}"),
        ];
        if self.pr_filters.is_empty() {
            merged_quals.push(format!("involves:{}", self.user));
        } else {
            merged_quals.extend(self.pr_filters.iter().cloned());
        }
        let merged_query = graphql::build_query(&merged_quals);
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
                let query = format!("is:open is:pr repo:{repo} archived:false");
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
        let mut existing: std::collections::HashSet<String> =
            tasks.iter().map(|t| t.id.key.clone()).collect();

        match reviewer_res {
            Ok(rev_tasks) => {
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
    pub async fn fetch_prs_for_repos(&self, repos: &[String]) -> Result<Vec<Task>, GhError> {
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
        // sync of their repo and now stay stuck on `OPEN`.
        let week_ago = (chrono::Utc::now() - chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string();
        let mut merged_quals = vec![
            "is:pr".to_string(),
            "is:merged".to_string(),
            "archived:false".to_string(),
            format!("merged:>={week_ago}"),
        ];
        if self.pr_filters.is_empty() {
            merged_quals.push(format!("involves:{}", self.user));
        } else {
            merged_quals.extend(self.pr_filters.iter().cloned());
        }
        let merged_query = graphql::build_query(&merged_quals);
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
        let mut repo_failures = 0usize;
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
                    repo_failures += 1;
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
        );
        // Mirror `fetch_all_prs`'s "everything failed" defensive
        // check: if EVERY repo we asked about failed, surface the
        // error so the tick doesn't silently wipe focus repo's PRs
        // from the inbox on the next rescope.
        if !repos.is_empty() && repo_failures == repos.len() {
            return Err(GhError::Graphql(format!(
                "all {} round-robin repo queries failed",
                repos.len()
            )));
        }
        Ok(tasks)
    }

    /// Run the main paginated PR search (cursor pages run
    /// sequentially because each page's `endCursor` is the next
    /// page's input). Extracted so the parallel-branches outer
    /// fetch can `tokio::join!` it alongside the merged-sweep + the
    /// watched-repo fan-out.
    async fn fetch_pr_search_paginated(&self, search_query: &str) -> Result<Vec<Task>, GhError> {
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
            let raw: serde_json::Value =
                self.post_graphql_with_retry(&body).await.map_err(|e| {
                    tracing::error!("GraphQL HTTP error (page {page}): {e}\n{e:?}");
                    tracing::error!(
                        "GraphQL request body was: {}",
                        serde_json::to_string_pretty(&body).unwrap_or_default()
                    );
                    e
                })?;
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
                self.observe_rate_limit(rl);
            }
            tasks.extend(
                data.search
                    .nodes
                    .iter()
                    .map(|pr| graphql::pr_to_task(pr, &self.user)),
            );
            let page_info = data.search.page_info.unwrap_or_default();
            if !page_info.has_next_page {
                break;
            }
            cursor = page_info.end_cursor;
            if cursor.is_none() {
                tracing::warn!("GraphQL paged: hasNextPage=true but endCursor=null");
                break;
            }
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
        Ok(tasks)
    }

    /// One-shot PR search (no pagination). Used by the merged-sweep
    /// and the watched-repo fan-out — both have small expected
    /// result sets and a `first: 100` page is fine. Returns `Ok(empty)`
    /// when the rate budget gates the request, so failures are
    /// distinguishable from "no results."
    async fn fetch_pr_single_query(
        &self,
        op: &'static str,
        query: String,
    ) -> Result<Vec<Task>, GhError> {
        if let Err(reason) = self.try_acquire() {
            return Err(GhError::RateLimited {
                retry_after_secs: 1,
                reason: format!("{op} blocked: {reason}"),
            });
        }
        let body = graphql::query_body(&query);
        let resp: graphql::GqlResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(errors) = resp.errors {
            let joined: String = errors
                .iter()
                .map(|e| e.full())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::Graphql(format!("{op}: {joined}")));
        }
        let Some(data) = resp.data else {
            return Ok(Vec::new());
        };
        if let Some(rl) = &data.rate_limit {
            self.observe_rate_limit(rl);
        }
        Ok(data
            .search
            .nodes
            .iter()
            .map(|pr| graphql::pr_to_task(pr, &self.user))
            .collect())
    }

    /// Fetch all open GitHub Issues involving the authenticated user,
    /// paginated. Separate from `fetch_all_prs` so callers opt in
    /// explicitly. Thin wrapper over `fetch_all_issues_with_mentions`
    /// that discards the mention side-channel — use the underlying
    /// method when you want the `@pilot` triggers too.
    pub async fn fetch_all_issues(&self) -> Result<Vec<Task>, GhError> {
        let (tasks, _mentions) = self
            .fetch_all_issues_with_mentions(&std::collections::BTreeSet::new())
            .await?;
        Ok(tasks)
    }

    /// Same as `fetch_all_issues` but also scans each raw issue for
    /// `@pilot` mentions from `allowed_logins` and returns the
    /// resulting [`crate::PilotMention`] list. Done in one pass so we
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
    ) -> Result<(Vec<Task>, Vec<crate::PilotMention>), GhError> {
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
        let mut mentions: Vec<crate::PilotMention> = Vec::new();
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

            let page_info = data.search.page_info.unwrap_or_default();
            if !page_info.has_next_page {
                break;
            }
            cursor = page_info.end_cursor;
            if cursor.is_none() {
                tracing::warn!("Issues paged: hasNextPage=true but endCursor=null");
                break;
            }
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
                self.fetch_all_prs().await
            } else {
                Ok(Vec::new())
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
        match (prs, issues) {
            (Ok(mut p), Ok(i)) => {
                p.extend(i);
                Ok(p)
            }
            (Ok(p), Err(e)) => {
                if want_issues && p.is_empty() {
                    Err(e)
                } else {
                    tracing::warn!("issues fetch failed (using PRs only): {e}");
                    Ok(p)
                }
            }
            (Err(e), Ok(i)) => {
                if want_prs && i.is_empty() {
                    Err(e)
                } else {
                    tracing::warn!("PRs fetch failed (using issues only): {e}");
                    Ok(i)
                }
            }
            (Err(pr_err), Err(issue_err)) => Err(GhError::Graphql(format!(
                "both PR and issue fetches failed: PRs={pr_err}; issues={issue_err}"
            ))),
        }
    }

    /// Variant of `fetch_selected` that surfaces partial failures
    /// to the caller as a structured side-channel instead of just a
    /// `tracing::warn`. Returns `(tasks, partial_failure)` — the
    /// second slot is `Some` when one side errored but the other
    /// returned results AND we returned `Ok` to keep the inbox
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
                self.fetch_all_prs().await
            } else {
                Ok(Vec::new())
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
        match (prs, issues) {
            (Ok(mut p), Ok(i)) => {
                p.extend(i);
                Ok((p, None))
            }
            (Ok(p), Err(e)) => {
                if want_issues && p.is_empty() {
                    Err(e)
                } else {
                    let msg = format!("issues sync failed (PRs OK): {e}");
                    tracing::warn!("{msg}");
                    Ok((p, Some(msg)))
                }
            }
            (Err(e), Ok(i)) => {
                if want_prs && i.is_empty() {
                    Err(e)
                } else {
                    let msg = format!("PRs sync failed (issues OK): {e}");
                    tracing::warn!("{msg}");
                    Ok((i, Some(msg)))
                }
            }
            (Err(pr_err), Err(issue_err)) => Err(GhError::Graphql(format!(
                "both PR and issue fetches failed: PRs={pr_err}; issues={issue_err}"
            ))),
        }
    }

    /// Round-robin variant of
    /// [`fetch_selected_with_status_and_mentions`]: runs the PR side
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
    /// [polling]: ../../../pilot_server/polling/fn.pick_repos_for_tick.html
    pub async fn fetch_round_robin_with_status_and_mentions(
        &self,
        want_prs: bool,
        repos: &[String],
        run_global: bool,
        want_issues: bool,
        allowed_logins: &std::collections::BTreeSet<String>,
    ) -> Result<(Vec<Task>, Option<String>, Vec<crate::PilotMention>), GhError> {
        if !want_prs && !want_issues {
            return Ok((Vec::new(), None, Vec::new()));
        }
        let do_pr_side = want_prs && (run_global || !repos.is_empty());
        let pr_fut = async {
            if !do_pr_side {
                return Ok(Vec::new());
            }
            if run_global {
                // Global sweep on this tick — same payload as the
                // pre-round-robin path. The per-repo fan-out is
                // skipped because the global already covers it.
                self.fetch_all_prs().await
            } else {
                self.fetch_prs_for_repos(repos).await
            }
        };
        let issue_fut = async {
            if want_issues {
                self.fetch_all_issues_with_mentions(allowed_logins).await
            } else {
                Ok((Vec::new(), Vec::new()))
            }
        };
        let (prs, issues) = tokio::join!(pr_fut, issue_fut);
        match (prs, issues) {
            (Ok(mut p), Ok((i, m))) => {
                p.extend(i);
                Ok((p, None, m))
            }
            (Ok(p), Err(e)) => {
                if want_issues && p.is_empty() {
                    Err(e)
                } else {
                    let msg = format!("issues sync failed (PRs OK): {e}");
                    tracing::warn!("{msg}");
                    Ok((p, Some(msg), Vec::new()))
                }
            }
            (Err(e), Ok((i, m))) => {
                if do_pr_side && i.is_empty() {
                    Err(e)
                } else {
                    let msg = format!("PRs sync failed (issues OK): {e}");
                    tracing::warn!("{msg}");
                    Ok((i, Some(msg), m))
                }
            }
            (Err(pr_err), Err(issue_err)) => Err(GhError::Graphql(format!(
                "both PR and issue fetches failed: PRs={pr_err}; issues={issue_err}"
            ))),
        }
    }

    /// Like `fetch_selected_with_status` but also runs the
    /// `@pilot`-mention scan on the issues side. The returned
    /// [`PilotMention`](crate::PilotMention) list is empty when
    /// `allowed_logins` is empty (the mention feature is opt-in via
    /// config) or when no allowed user has written `@pilot` on an
    /// unreacted body / comment. Errors fall back to the same
    /// partial-failure shape as the underlying call — a failed
    /// PR side keeps issues + mentions, and vice versa.
    pub async fn fetch_selected_with_status_and_mentions(
        &self,
        want_prs: bool,
        want_issues: bool,
        allowed_logins: &std::collections::BTreeSet<String>,
    ) -> Result<(Vec<Task>, Option<String>, Vec<crate::PilotMention>), GhError> {
        if !want_prs && !want_issues {
            return Ok((Vec::new(), None, Vec::new()));
        }
        let pr_fut = async {
            if want_prs {
                self.fetch_all_prs().await
            } else {
                Ok(Vec::new())
            }
        };
        let issue_fut = async {
            if want_issues {
                self.fetch_all_issues_with_mentions(allowed_logins).await
            } else {
                Ok((Vec::new(), Vec::new()))
            }
        };
        let (prs, issues) = tokio::join!(pr_fut, issue_fut);
        match (prs, issues) {
            (Ok(mut p), Ok((i, m))) => {
                p.extend(i);
                Ok((p, None, m))
            }
            (Ok(p), Err(e)) => {
                if want_issues && p.is_empty() {
                    Err(e)
                } else {
                    let msg = format!("issues sync failed (PRs OK): {e}");
                    tracing::warn!("{msg}");
                    Ok((p, Some(msg), Vec::new()))
                }
            }
            (Err(e), Ok((i, m))) => {
                if want_prs && i.is_empty() {
                    Err(e)
                } else {
                    let msg = format!("PRs sync failed (issues OK): {e}");
                    tracing::warn!("{msg}");
                    Ok((i, Some(msg), m))
                }
            }
            (Err(pr_err), Err(issue_err)) => Err(GhError::Graphql(format!(
                "both PR and issue fetches failed: PRs={pr_err}; issues={issue_err}"
            ))),
        }
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
    /// Issue body or an IssueComment for the `@pilot`-mention
    /// auto-spawn flow. The reaction is the canonical idempotency
    /// marker for that flow: subsequent polls select
    /// `viewerHasReacted` and skip already-acknowledged surfaces, so
    /// pilot doesn't re-spawn every cycle.
    ///
    /// Re-posting an existing reaction is a no-op on GitHub's side,
    /// so retrying on transient failure is safe.
    pub async fn react_eyes(&self, reactable_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("addReaction(EYES) mutation")?;
        let body = graphql::add_reaction_eyes_body(reactable_node_id);
        let response: graphql::GqlResponse = self.post_graphql_with_retry(&body).await?;
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
        let response: graphql::GqlResponse = self.post_graphql_with_retry(&body).await?;
        // Observe rate-limit if the mutation response includes it
        // (currently it doesn't — the mutation body doesn't select
        // `rateLimit` — but if a future query body change pulls it
        // in, we use it for free).
        if let Some(data) = &response.data
            && let Some(rl) = &data.rate_limit
        {
            self.observe_rate_limit(rl);
        }
        if let Some(errors) = response.errors {
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

    /// Merge a PR — same as clicking "Merge pull request" on
    /// github.com. Requires the PR's GraphQL node ID. We don't pin
    /// the merge method; GitHub will use whatever the repo's
    /// settings allow / require.
    /// Lazy-fetch one PR's heavy fields (review threads — inline code
    /// comments). The inbox-scan query trades these off for cost; this
    /// method back-fills them when the user actually opens a PR.
    ///
    /// Returns the merged `Activity` list ready to splice into the
    /// workspace's existing activity collection. Caller is responsible
    /// for dedup (by `node_id`) since this re-fetches data the eager
    /// path might still be loading. Both paths produce the same shape
    /// — same kind, same body formatting, same path/line/diff_hunk
    /// extraction — so the merged list is indistinguishable from a
    /// purely-eager fetch.
    pub async fn fetch_pr_details(
        &self,
        pull_request_node_id: &str,
    ) -> Result<Vec<pilot_core::Activity>, GhError> {
        self.acquire_or_block("PR details lazy-fetch")?;
        let body = graphql::pr_details_body(pull_request_node_id);
        let response: graphql::GqlPrDetailsResponse = self.post_graphql_with_retry(&body).await?;
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
            self.observe_rate_limit(rl);
        }
        let Some(node) = data.node else {
            // PR was deleted / not visible to this token between the
            // inbox search and the lazy fetch. Not retryable — return
            // an empty activity list so the caller can clean up.
            tracing::info!(
                "fetch_pr_details: node {} not found (deleted or scope changed)",
                pull_request_node_id,
            );
            return Ok(Vec::new());
        };
        Ok(graphql::pr_details_to_activities(&node))
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
        let response: graphql::GqlResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(data) = &response.data
            && let Some(rl) = &data.rate_limit
        {
            self.observe_rate_limit(rl);
        }
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
        let response: graphql::GqlResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(data) = &response.data
            && let Some(rl) = &data.rate_limit
        {
            self.observe_rate_limit(rl);
        }
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
        let response: graphql::GqlResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(data) = &response.data
            && let Some(rl) = &data.rate_limit
        {
            self.observe_rate_limit(rl);
        }
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

    pub async fn merge_pr(&self, pull_request_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("mergePullRequest mutation")?;
        let body = graphql::merge_pr_body(pull_request_node_id);
        let response: graphql::GqlResponse = self.post_graphql_with_retry(&body).await?;
        if let Some(data) = &response.data
            && let Some(rl) = &data.rate_limit
        {
            self.observe_rate_limit(rl);
        }
        if let Some(errors) = response.errors {
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
}

impl pilot_core::TaskProvider for GhClient {
    fn name(&self) -> &str {
        "github"
    }

    async fn fetch_tasks(&self) -> Result<Vec<pilot_core::Task>, pilot_core::ProviderError> {
        self.fetch_all_prs().await.map_err(Into::into)
    }

    fn username(&self) -> Option<&str> {
        Some(&self.user)
    }

    /// Merge the workspace's PR. Requires `workspace.pr.node_id`
    /// (the GraphQL node id) — the polling cycle fills it in;
    /// hitting this on a fresh-from-cache workspace surfaces as
    /// `Permanent("PR has no node_id")` which the caller can
    /// translate to "repoll first".
    async fn merge(
        &self,
        workspace: &pilot_core::Workspace,
    ) -> Result<(), pilot_core::ProviderError> {
        let Some(pr) = workspace.pr.as_ref() else {
            return Err(pilot_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no PR", workspace.key),
            ));
        };
        let Some(node_id) = pr.node_id.as_deref() else {
            return Err(pilot_core::ProviderError::permanent(
                "github",
                "PR has no node_id (poll first)",
            ));
        };
        self.merge_pr(node_id)
            .await
            .map_err(|e| pilot_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Request reviewer(s) on the workspace's PR. Logins are
    /// github usernames (no `@` prefix). Daemon resolves logins →
    /// node ids inside `request_reviewers`.
    async fn request_reviewers(
        &self,
        workspace: &pilot_core::Workspace,
        logins: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
        let Some(pr) = workspace.pr.as_ref() else {
            return Err(pilot_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no PR", workspace.key),
            ));
        };
        let Some(node_id) = pr.node_id.as_deref() else {
            return Err(pilot_core::ProviderError::permanent(
                "github",
                "PR has no node_id (poll first)",
            ));
        };
        self.request_reviewers(node_id, logins)
            .await
            .map_err(|e| pilot_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Add assignee(s) to the workspace's PR or issue. Both are
    /// GraphQL `Assignable` so a single mutation covers them.
    async fn add_assignees(
        &self,
        workspace: &pilot_core::Workspace,
        logins: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
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
            return Err(pilot_core::ProviderError::permanent(
                "github",
                format!(
                    "workspace {} has neither a PR nor an issue with a node_id",
                    workspace.key
                ),
            ));
        };
        self.add_assignees(node_id, logins)
            .await
            .map_err(|e| pilot_core::ProviderError::permanent("github", e.to_string()))
    }

    /// Replace the assignee set on the workspace's PR or issue.
    /// Computes the diff against the task's persisted assignees and
    /// fires both `addAssigneesToAssignable` and
    /// `removeAssigneesFromAssignable` mutations. Empty `logins`
    /// clears every assignee (intentional — the UX cycles through
    /// an unchecked picker for that case).
    async fn set_assignees(
        &self,
        workspace: &pilot_core::Workspace,
        logins: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
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
                pilot_core::ProviderError::permanent(
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
                .map_err(|e| pilot_core::ProviderError::permanent("github", e.to_string()))?;
        }
        if !to_remove.is_empty() {
            self.remove_assignees(node_id, &to_remove)
                .await
                .map_err(|e| pilot_core::ProviderError::permanent("github", e.to_string()))?;
        }
        Ok(())
    }

    /// Post a reply (comment) on the workspace's PR or issue.
    /// Uses `post_issue_comment` because github's REST API exposes
    /// the same endpoint for both (PRs are issues at the REST
    /// layer) — `pr.number` doubles as the issue number.
    async fn post_reply(
        &self,
        workspace: &pilot_core::Workspace,
        body: &str,
    ) -> Result<(), pilot_core::ProviderError> {
        let primary = workspace.primary_task().ok_or_else(|| {
            pilot_core::ProviderError::permanent(
                "github",
                format!("workspace {} has no primary task", workspace.key),
            )
        })?;
        let Some(repo) = primary.repo.as_deref() else {
            return Err(pilot_core::ProviderError::permanent(
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
            return Err(pilot_core::ProviderError::permanent(
                "github",
                format!("can't parse number from task key `{}`", primary.id.key),
            ));
        };
        self.post_issue_comment(repo, number, body)
            .await
            .map_err(|e| pilot_core::ProviderError::permanent("github", e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    fn make_client(base_uri: &str) -> GhClient {
        // Bypass `from_credential` (which calls `/user`) — we want
        // a `GhClient` that talks to the mock server directly.
        let inner = octocrab::Octocrab::builder()
            .base_uri(base_uri)
            .unwrap()
            .build()
            .unwrap();
        GhClient {
            inner,
            user: "test-user".to_string(),
            credential_source: "test".to_string(),
            pr_filters: vec![],
            issue_filters: vec![],
            watch_repos: vec![],
            budget: std::sync::Arc::new(std::sync::Mutex::new(
                crate::rate_budget::RateBudget::default_for_pilot(),
            )),
        }
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
        let pe: pilot_core::ProviderError = err.into();
        assert!(
            matches!(pe, pilot_core::ProviderError::Auth { .. }),
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
