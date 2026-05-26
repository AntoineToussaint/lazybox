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

/// Render a `GhError` as a useful string. The Display impl on
/// `octocrab::Error::GitHub` *only prints `"GitHub"`* — it drops the
/// inner status / message / errors entirely. We unwrap to the
/// underlying `GitHubError` (or other variants) so the message
/// actually reaches logs + the error modal.
/// Is this octocrab error worth retrying? Used by
/// `post_graphql_with_retry` to decide between sleep-and-retry and
/// fail-fast. The cutoff matches `From<GhError> for ProviderError`'s
/// retryable classification — anything that maps to `Retryable`
/// there is fair game here too.
fn is_transient(e: &octocrab::Error) -> bool {
    match e {
        octocrab::Error::Hyper { .. }
        | octocrab::Error::Service { .. }
        | octocrab::Error::Http { .. }
        | octocrab::Error::Json { .. }
        | octocrab::Error::Serde { .. } => true,
        octocrab::Error::GitHub { source, .. } => {
            // 502/503/504 — server-side hiccup. NOT 429: rate
            // limit is for the budget layer, not retry-loops.
            // NOT 5xx in general: e.g. a 500 with a deterministic
            // error message won't change on retry.
            matches!(source.status_code.as_u16(), 502..=504)
        }
        _ => false,
    }
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
        GhError::Api(octo) => match octo {
            octocrab::Error::GitHub { source, .. } => {
                // GitHubError's Display does the right thing —
                // includes status, message, docs URL, errors.
                format!("GitHub API ({}): {}", source.status_code, source)
            }
            other => format!("{other}"),
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
    /// transient errors. The body is borrowed (octocrab takes
    /// `Option<&B>`), so the SAME body bytes go out on every
    /// attempt — no risk of the body-clone bug that made us
    /// disable octocrab's built-in retry. The caller has already
    /// spent a rate-budget token via `acquire_or_block`; we do
    /// NOT re-acquire per retry (a single 502-then-retry-success
    /// is one logical call from GitHub's perspective).
    ///
    /// Retry policy:
    /// - Transport variants (Hyper, Service, Http, Json, Serde,
    ///   Io) → retry. Always transient.
    /// - 502 / 503 / 504 → retry. GitHub serves these under load.
    /// - Anything else (4xx including auth and 429, 5xx other
    ///   than the three above, deserialization mismatches in
    ///   typed `T`) → return immediately. Retrying wouldn't change
    ///   the answer.
    ///
    /// Backoff sequence: 200ms → 800ms. Two retries after the
    /// initial attempt = 3 attempts total. Tight enough that a
    /// 60s poll cycle still has headroom; long enough that the
    /// usual <1s blip resolves itself.
    async fn post_graphql_with_retry<T>(
        &self,
        body: &serde_json::Value,
    ) -> Result<T, octocrab::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        const DELAYS_MS: &[u64] = &[200, 800];
        let mut last_err: Option<octocrab::Error> = None;
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
            match self.inner.post::<_, T>("/graphql", Some(body)).await {
                Ok(v) => {
                    if attempt > 0 {
                        tracing::info!("graphql request succeeded on retry {attempt}");
                    }
                    return Ok(v);
                }
                Err(e) => {
                    if !is_transient(&e) {
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
        // Assemble: defaults (is:open is:pr archived:false) + caller-
        // supplied qualifiers (role narrowing + scope narrowing).
        // The caller is responsible for including a role qualifier
        // — `involves:USER` if they want everything the user is
        // involved in, or a narrower `author:USER` / `OR`-combo
        // string when filtered. This keeps GraphQL search precise:
        // PRs we'd just drop never come back over the wire.
        let mut quals = graphql::default_search_qualifiers();
        if self.pr_filters.is_empty() {
            quals.push(format!("involves:{}", self.user));
        } else {
            quals.extend(self.pr_filters.iter().cloned());
        }
        let search_query = graphql::build_query(&quals);

        tracing::info!("GraphQL search: {search_query}");

        // Paginate until GitHub reports no more pages. Without this, users
        // with >100 inbox PRs lose the tail on first poll, which the stale
        // purge then deletes from SQLite — PRs "disappear" after restart.
        let mut tasks: Vec<Task> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            // Local guard rail. Refuse to fire if pilot's
            // self-imposed budget is exhausted OR the previous
            // response told us GitHub is low. Surfaces as a
            // retryable error to the polling layer.
            self.acquire_or_block("PR search")?;

            let body = graphql::query_body_after(&search_query, cursor.as_deref());
            tracing::debug!(
                "GraphQL page {page} body: {}",
                serde_json::to_string(&body).unwrap_or_default()
            );

            let raw: serde_json::Value =
                self.post_graphql_with_retry(&body).await.map_err(|e| {
                    // octocrab's Display on Error::GitHub drops
                    // status + message — print Debug too so
                    // /tmp/pilot.log has actionable context.
                    tracing::error!("GraphQL HTTP error (page {page}): {e}\n{e:?}");
                    tracing::error!(
                        "GraphQL request body was: {}",
                        serde_json::to_string_pretty(&body).unwrap_or_default()
                    );
                    GhError::Api(e)
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
                let detailed: Vec<_> = errors.iter().map(|e| e.full()).collect();
                let joined = detailed.join("; ");
                tracing::error!("GraphQL errors (search page {page}): {joined}");
                tracing::error!("GraphQL search was: {search_query}");
                tracing::error!(
                    "GraphQL request body:\n{}",
                    serde_json::to_string_pretty(&body).unwrap_or_default()
                );
                tracing::error!(
                    "GraphQL full response body:\n{}",
                    serde_json::to_string_pretty(&raw).unwrap_or_default()
                );
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
                // Defensive: hasNextPage=true but no cursor. Bail out rather
                // than looping forever.
                tracing::warn!("GraphQL paged: hasNextPage=true but endCursor=null");
                break;
            }
            page += 1;
            if page >= 20 {
                // 20 pages × 100 per page = 2000 PRs. Past that, we
                // truncate. Almost always means the user's filter is
                // way too loose; surface it as a retryable error so
                // the TUI's footer can warn instead of silently
                // dropping the tail.
                tracing::error!(
                    "GraphQL paged: bailing after {page} pages (safety cap; tail truncated)"
                );
                return Err(GhError::Truncated {
                    count: tasks.len(),
                    pages: page,
                });
            }
        }

        // Fetch watched repos (all open PRs, not just involves:user).
        // Each repo is its own GraphQL search, so each one must be
        // gated independently AND must record GitHub's reported
        // rate-limit. Pre-fix this loop was the biggest ungated
        // request path: 10 watched repos = 10 ungated GraphQL calls
        // per poll cycle.
        let mut watch_failures: usize = 0;
        for repo in &self.watch_repos {
            // If the budget is exhausted mid-loop, stop trying — we
            // already have the main-search results to return; better
            // to ship those than fail the whole poll. Treat as a
            // failure for the "all watched failed" classifier below.
            if let Err(reason) = self.try_acquire() {
                tracing::warn!("watch-repo query for {repo} blocked by rate budget: {reason}");
                watch_failures += 1;
                continue;
            }
            let watch_query = format!("is:open is:pr repo:{repo} archived:false");
            let watch_body = graphql::query_body(&watch_query);
            tracing::debug!("Watch query for {repo}: {watch_query}");

            match self
                .post_graphql_with_retry::<graphql::GqlResponse>(&watch_body)
                .await
            {
                Ok(resp) => {
                    if let Some(data) = &resp.data {
                        // Observe rate-limit from this response. The
                        // unified search above already did this for
                        // its own response; the watch loop wasn't.
                        // Result: a long watch loop on a low budget
                        // would keep firing without updating our
                        // remote-awareness.
                        if let Some(rl) = &data.rate_limit {
                            self.observe_rate_limit(rl);
                        }
                        let existing_keys: std::collections::HashSet<String> =
                            tasks.iter().map(|t| t.id.key.clone()).collect();
                        for pr in &data.search.nodes {
                            let task = graphql::pr_to_task(pr, &self.user);
                            if !existing_keys.contains(&task.id.key) {
                                tasks.push(task);
                            }
                        }
                    }
                    if let Some(errors) = resp.errors {
                        let detailed: Vec<_> = errors.iter().map(|e| e.full()).collect();
                        tracing::warn!("Watch query errors for {repo}: {}", detailed.join("; "));
                    }
                }
                Err(e) => {
                    tracing::warn!("Watch query failed for {repo}: {e}");
                    watch_failures += 1;
                }
            }
        }

        // Recently-merged sweep. The main inbox search is `is:open`,
        // so a PR that merges between polls drops out entirely —
        // pilot's local copy keeps showing the last-known state
        // (typically QUEUED or AUTO) forever, until rescope finally
        // removes the row. The user never sees MERGED.
        //
        // Fix: one extra search per poll for PRs merged in the last
        // 24h, scoped by the SAME role qualifiers as the inbox
        // search. The merged PRs land via the same upsert path; the
        // mailbox-membership predicate routes them to Inactive
        // (state=Merged → not in Inbox) so they don't pollute the
        // active view. User sees them briefly with the MERGED pill
        // before they fall into history.
        //
        // Cost: ONE extra GraphQL search per poll. 24h window keeps
        // the result set small (most repos merge a few PRs per day).
        // No pagination: if a repo merges >100 PRs in 24h, the tail
        // is silently dropped — accept that, the user can still see
        // them via GitHub directly.
        let yesterday = (chrono::Utc::now() - chrono::Duration::hours(24))
            .format("%Y-%m-%d")
            .to_string();
        let mut merged_quals = vec![
            "is:pr".to_string(),
            "is:merged".to_string(),
            "archived:false".to_string(),
            format!("merged:>={yesterday}"),
        ];
        if self.pr_filters.is_empty() {
            merged_quals.push(format!("involves:{}", self.user));
        } else {
            merged_quals.extend(self.pr_filters.iter().cloned());
        }
        let merged_query = graphql::build_query(&merged_quals);
        tracing::debug!("Recently-merged sweep: {merged_query}");
        if let Err(reason) = self.try_acquire() {
            tracing::warn!("recently-merged sweep blocked by rate budget: {reason}");
        } else {
            let body = graphql::query_body(&merged_query);
            match self
                .post_graphql_with_retry::<graphql::GqlResponse>(&body)
                .await
            {
                Ok(resp) => {
                    if let Some(data) = &resp.data {
                        if let Some(rl) = &data.rate_limit {
                            self.observe_rate_limit(rl);
                        }
                        let existing_keys: std::collections::HashSet<String> =
                            tasks.iter().map(|t| t.id.key.clone()).collect();
                        let mut added = 0usize;
                        for pr in &data.search.nodes {
                            let task = graphql::pr_to_task(pr, &self.user);
                            if !existing_keys.contains(&task.id.key) {
                                tasks.push(task);
                                added += 1;
                            }
                        }
                        if added > 0 {
                            tracing::info!(
                                "recently-merged sweep: {added} PRs back-filled \
                                 with final MERGED state",
                            );
                        }
                    }
                    if let Some(errors) = resp.errors {
                        let detailed: Vec<_> = errors.iter().map(|e| e.full()).collect();
                        tracing::warn!("recently-merged sweep errors: {}", detailed.join("; "),);
                    }
                }
                Err(e) => {
                    tracing::warn!("recently-merged sweep failed: {e}");
                }
            }
        }

        tracing::info!(
            "GraphQL returned {} PRs (incl. {} watched repos + merged-sweep)",
            tasks.len(),
            self.watch_repos.len()
        );
        // If every configured watched-repo query failed, surface a
        // retryable error. The main query succeeded so we have
        // *some* tasks, but the user explicitly opted in to those
        // extra repos and silently missing them is worse than a
        // visible "couldn't fetch watched repos" notice.
        if !self.watch_repos.is_empty() && watch_failures == self.watch_repos.len() {
            return Err(GhError::WatchAllFailed {
                count: self.watch_repos.len(),
            });
        }
        Ok(tasks)
    }

    /// Fetch all open GitHub Issues involving the authenticated user,
    /// paginated. Separate from `fetch_all_prs` so callers opt in
    /// explicitly.
    pub async fn fetch_all_issues(&self) -> Result<Vec<Task>, GhError> {
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
        let mut cursor: Option<String> = None;
        let mut page = 0usize;
        loop {
            // Same rate-budget guard as PR fetch — see fetch_all_prs.
            self.acquire_or_block("issues search")?;
            let body = graphql::issues_query_body(&search_query, cursor.as_deref());
            let response: graphql::GqlIssueResponse =
                self.post_graphql_with_retry(&body).await.map_err(|e| {
                    tracing::error!("Issues HTTP error (page {page}): {e}\n{e:?}");
                    GhError::Api(e)
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

            tasks.extend(
                data.search
                    .nodes
                    .iter()
                    .map(|issue| graphql::issue_to_task(issue, &self.user)),
            );

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

        tracing::info!("GraphQL returned {} issues", tasks.len());
        Ok(tasks)
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
        match (prs, issues) {
            (Ok(mut p), Ok(i)) => {
                p.extend(i);
                Ok(p)
            }
            // Partial success: one side failed, the other returned
            // results. We only soft-degrade if the OTHER side
            // genuinely contributed something — otherwise the user
            // gets "0 tasks, no error" because both sides ran but
            // one was a silent zero. Bubble the failure up so they
            // see an error modal.
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

    /// Merge the base branch into this PR's head — same as the "Update
    /// branch" button on github.com. Requires the PR's GraphQL node ID.
    pub async fn update_branch(&self, pull_request_node_id: &str) -> Result<(), GhError> {
        self.acquire_or_block("updatePullRequestBranch mutation")?;
        let body = graphql::update_branch_body(pull_request_node_id);
        let response: graphql::GqlResponse = self
            .post_graphql_with_retry(&body)
            .await
            .map_err(GhError::Api)?;
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
        let response: graphql::GqlPrDetailsResponse = self
            .post_graphql_with_retry(&body)
            .await
            .map_err(GhError::Api)?;
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
        let response: graphql::GqlUserIdResponse = self
            .post_graphql_with_retry(&body)
            .await
            .map_err(GhError::Api)?;
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
        let response: graphql::GqlResponse = self
            .post_graphql_with_retry(&body)
            .await
            .map_err(GhError::Api)?;
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
        let response: graphql::GqlResponse = self
            .post_graphql_with_retry(&body)
            .await
            .map_err(GhError::Api)?;
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
        let response: graphql::GqlResponse = self
            .post_graphql_with_retry(&body)
            .await
            .map_err(GhError::Api)?;
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
        let response: graphql::GqlResponse = self
            .post_graphql_with_retry(&body)
            .await
            .map_err(GhError::Api)?;
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
