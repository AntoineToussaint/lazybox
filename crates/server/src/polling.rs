//! Provider polling. The daemon owns ONE polling task per process; it
//! drives the configured `TaskSource`s on an interval, upserts each
//! returned task into the store, and broadcasts `SessionUpserted`
//! events through the `ServerConfig::bus` so every connected client
//! sees the change.
//!
//! ## Why a `TaskSource` trait, not direct GhClient/LinearClient calls
//!
//! The polling loop's logic — interval, upsert, broadcast, error
//! reporting — is identical for every provider. Hard-coding GitHub and
//! Linear into the loop would make it hard to test (real HTTP calls
//! against real APIs) and hard to extend (adding Jira would touch the
//! loop). With `TaskSource`, the loop is provider-agnostic and tests
//! drop in a fixture source that returns whatever vector of tasks the
//! test wants.
//!
//! ## Read-state preservation on update
//!
//! When a task we've seen before comes back from a provider, we merge
//! its fresh fields onto the existing `Session` rather than replacing
//! it — so `seen_count`, `read_indices`, `snoozed_until`, and
//! `last_viewed_at` all survive the poll. We do it inline here since
//! there's only one place sessions enter the system.

use crate::ServerConfig;
use chrono::Utc;
use pilot_core::{ProviderConfig, Task, Workspace, WorkspaceKey};
use pilot_gh::GhClient;
use pilot_ipc::Event;
use pilot_linear::LinearClient;
use pilot_store::WorkspaceRecord;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Anything that can produce a flat list of `Task`s. Implementations
/// should be cheap to construct and cheap to call repeatedly: they're
/// invoked on every poll tick.
///
/// Errors are typed (`pilot_core::ProviderError`) so polling can
/// distinguish retryable hiccups from auth failures from permanent
/// bugs and react accordingly. See `pilot_core::provider`.
pub trait TaskSource: Send + Sync + 'static {
    /// Short stable name for telemetry / `Event::ProviderError`
    /// (e.g. "github", "linear").
    fn name(&self) -> &str;

    /// Fetch the current set of tasks. Returns a classified error so
    /// the polling loop can pick the right log level + decide whether
    /// to retry.
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>>;
}

/// `GhClient` adapter. The filter narrows the upstream result by
/// role and item type before they reach the daemon's upsert path —
/// disabled roles / types never become Workspaces. `scopes` further
/// narrows by repo / org: when non-empty, only tasks whose
/// `task.repo` matches a selected scope id pass through.
pub struct GhSource {
    pub client: GhClient,
    pub filter: ProviderConfig,
    pub scopes: std::collections::BTreeSet<String>,
    /// Bus handle so the source can emit `PollProgress` events
    /// during its fetch. The polling layer doesn't pass `&ServerConfig`
    /// to `TaskSource::fetch` (would couple them), so each source
    /// keeps a clone of just the broadcast sender.
    pub bus: tokio::sync::broadcast::Sender<Event>,
}

impl GhSource {
    fn emit_progress(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(source = "github", %message, "poll progress");
        let _ = self.bus.send(Event::PollProgress {
            source: "github".into(),
            message,
        });
    }
}

impl TaskSource for GhSource {
    fn name(&self) -> &str {
        "github"
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>>
    {
        Box::pin(async move {
            let want_prs = self.filter.pr_enabled();
            let want_issues = self.filter.issue_enabled();

            // Surface what we're about to do so the polling modal
            // can show "Querying PRs from github…" instead of just
            // a bare spinner. Each step also lands in /tmp/pilot.log
            // for free, easing debugging.
            let plan = match (want_prs, want_issues) {
                (true, true) => "PRs + Issues",
                (true, false) => "PRs",
                (false, true) => "Issues",
                (false, false) => {
                    self.emit_progress("nothing to fetch (no PR or Issue keys enabled)");
                    return Ok(Vec::new());
                }
            };
            self.emit_progress(format!("Querying GitHub for {plan}…"));
            // Emit the EXACT rendered query strings so the user can
            // see what's being asked. This is the single most useful
            // data point when debugging "filter returned 0 results" —
            // they can paste the query into github.com/search and
            // verify what GitHub itself thinks is in scope.
            if want_prs {
                self.emit_progress(format!("PR query: {}", self.client.pr_search_query()));
            }
            if want_issues {
                self.emit_progress(format!("Issue query: {}", self.client.issue_search_query()));
            }

            let raw = self
                .client
                .fetch_selected(want_prs, want_issues)
                .await
                .map_err(pilot_core::ProviderError::from)?;

            self.emit_progress(format!("Got {} raw items, applying filters…", raw.len()));
            let kept = filter_github_tasks(raw, &self.filter, &self.scopes);
            self.emit_progress(format!("{} tasks kept after filter", kept.len()));

            // Log a per-rate-budget summary too. Cheap, super useful
            // when debugging "why is polling slow / failing".
            let snap = self.client.rate_snapshot();
            if let Some(remote) = snap.remote {
                tracing::info!(
                    source = "github",
                    remote_remaining = remote.remaining,
                    remote_limit = remote.limit,
                    local_available = snap.local_available,
                    local_capacity = snap.local_capacity,
                    "rate budget snapshot"
                );
            }
            Ok(kept)
        })
    }
}

/// `LinearClient` adapter.
pub struct LinearSource {
    pub client: LinearClient,
    pub filter: ProviderConfig,
    pub bus: tokio::sync::broadcast::Sender<Event>,
}

impl LinearSource {
    fn emit_progress(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(source = "linear", %message, "poll progress");
        let _ = self.bus.send(Event::PollProgress {
            source: "linear".into(),
            message,
        });
    }
}

impl TaskSource for LinearSource {
    fn name(&self) -> &str {
        "linear"
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.emit_progress("Querying Linear for issues…");
            let raw = self
                .client
                .fetch_all()
                .await
                .map_err(pilot_core::ProviderError::from)?;
            self.emit_progress(format!("Got {} issues, applying filters…", raw.len()));
            let kept = filter_linear_tasks(raw, &self.filter);
            self.emit_progress(format!("{} issues kept after filter", kept.len()));
            Ok(kept)
        })
    }
}

/// Build the GraphQL search qualifiers a `GhClient` should use,
/// derived from the user's persisted role + scope selection. The
/// result is appended to `default_search_qualifiers` (`is:open
/// is:pr archived:false`) before being sent to GitHub.
///
/// Mapping:
///
/// - **Role** — `involves:USER` when all four roles (or none) are
///   enabled. With a strict subset, emit explicit role qualifiers
///   (`author:USER`, `review-requested:USER`, `assignee:USER`,
///   `mentions:USER`) joined with `OR` inside parens.
/// - **Scope** — each `github:owner` becomes `org:owner`; each
///   `github:owner/repo` becomes `repo:owner/repo`. Multiple scope
///   qualifiers are OR'd inside parens so the user gets the union.
///
/// Empty role + empty scope → the same `involves:USER` baseline as
/// before, so legacy setups (no picker visited) keep working.
/// Qualifiers for the **PR** search. Reads `pr.*` keys.
pub fn build_pr_search_qualifiers(
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
    username: &str,
) -> Vec<String> {
    let mut quals = Vec::new();
    let pr_roles = [
        ("pr.author", "author"),
        ("pr.reviewer", "review-requested"),
        ("pr.assignee", "assignee"),
        ("pr.mentioned", "mentions"),
    ];
    quals.push(role_qualifier(filter, username, &pr_roles));
    if let Some(s) = scope_qualifier(scopes) {
        quals.push(s);
    }
    quals
}

/// Qualifiers for the **Issue** search. Reads `issue.*` keys (no
/// reviewer concept — issues don't have reviewers in GitHub).
pub fn build_issue_search_qualifiers(
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
    username: &str,
) -> Vec<String> {
    let mut quals = Vec::new();
    let issue_roles = [
        ("issue.author", "author"),
        ("issue.assignee", "assignee"),
        ("issue.mentioned", "mentions"),
    ];
    quals.push(role_qualifier(filter, username, &issue_roles));
    if let Some(s) = scope_qualifier(scopes) {
        quals.push(s);
    }
    quals
}

/// Build a single role qualifier for the GitHub search API.
///
/// Why not OR-with-parens — GitHub's qualifier-style search parser
/// silently mishandles parens-grouped ORs combined with other
/// qualifiers (`(author:X OR review-requested:X) repo:Y`): the API
/// returns 0 even when the unrouped equivalent
/// (`author:X repo:Y`) returns rows. Confirmed against `gh search
/// prs` 2026-05-01 — same token, same query, the paren-form returns
/// `[]` while the no-paren form returns the user's PRs.
///
/// So we use the search syntax that's known to work:
///
/// - **0 roles enabled** → `involves:USER`. The user will see no rows
///   because `filter_github_tasks` drops everything; we still want
///   the request to be valid.
/// - **1 role enabled** → emit that single qualifier directly
///   (`author:USER`). No OR, no parens, just works.
/// - **2+ roles enabled** → emit `involves:USER` (covers author,
///   reviewer, assignee, mentioned) and let `filter_github_tasks`
///   drop the disabled roles post-fetch. Slightly more bytes over
///   the wire, but reliable.
///
/// Net effect: the wire query never contains a parens group, so the
/// "0 results from a valid query" footgun is gone.
fn role_qualifier(filter: &ProviderConfig, username: &str, keys: &[(&str, &str)]) -> String {
    let enabled: Vec<&str> = keys
        .iter()
        .filter(|(k, _)| filter.has(k))
        .map(|(_, op)| *op)
        .collect();
    match enabled.len() {
        0 => format!("involves:{username}"),
        1 => format!("{}:{username}", enabled[0]),
        _ => format!("involves:{username}"),
    }
}

/// `(org:foo OR repo:bar/baz)` for a non-empty scope set; `None`
/// when the user hasn't narrowed (= subscribe to all).
fn scope_qualifier(scopes: &std::collections::BTreeSet<String>) -> Option<String> {
    if scopes.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = scopes
        .iter()
        .filter_map(|s| {
            let stripped = s.strip_prefix("github:")?;
            if stripped.contains('/') {
                Some(format!("repo:{stripped}"))
            } else {
                Some(format!("org:{stripped}"))
            }
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    if parts.len() == 1 {
        return Some(parts.pop().unwrap());
    }
    Some(format!("({})", parts.join(" OR ")))
}

/// Drop GitHub tasks that don't match the user's enabled roles +
/// item types + scope selection. `scopes` is the
/// `selected_scopes["github"]` set (possibly empty); tasks pass the
/// scope gate when:
///
/// - `scopes` is empty (user didn't pick anything → see all), OR
/// - the task's repo matches a selected repo scope, OR
/// - the task's repo lives under a selected org scope (parent match).
///
/// Tasks without a usable `role` field default to passing the role
/// check (we trust the upstream classification).
pub fn filter_github_tasks(
    tasks: Vec<Task>,
    filter: &ProviderConfig,
    scopes: &std::collections::BTreeSet<String>,
) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|t| {
            // Combined type+role gate. Issues use `issue.*` keys, PRs
            // use `pr.*` keys. Tasks of unknown type (discussions,
            // etc.) bypass the type/role filter — they don't have a
            // toggle.
            let type_role_ok = if t.url.contains("/pull/") {
                filter.pr_enabled() && filter.allows_pr_role(t.role)
            } else if t.url.contains("/issues/") {
                filter.issue_enabled() && filter.allows_issue_role(t.role)
            } else {
                true
            };
            if !type_role_ok {
                return false;
            }
            // Scope gate. Empty `scopes` = "all" (the no-picker
            // default). Otherwise repo:owner/name must match a
            // selected repo scope, OR its owner must match a
            // selected org scope.
            if scopes.is_empty() {
                return true;
            }
            let Some(repo) = t.repo.as_deref() else {
                return false;
            };
            let repo_scope = format!("github:{repo}");
            if scopes.contains(&repo_scope) {
                return true;
            }
            if let Some((owner, _)) = repo.split_once('/') {
                return scopes.contains(&format!("github:{owner}"));
            }
            false
        })
        .collect()
}

/// Drop Linear tasks whose role isn't enabled. Linear has no
/// PRs-vs-Issues distinction — flat `role.*` keys.
pub fn filter_linear_tasks(tasks: Vec<Task>, filter: &ProviderConfig) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|t| filter.allows_linear_role(t.role))
        .collect()
}

/// Best-effort: build the source set from the user's persisted
/// setup. Each constructed source carries the per-provider filter
/// (role + item-type toggles) and applies it post-fetch. Providers
/// whose id isn't in `enabled_providers` are skipped entirely.
///
/// The `bus` is the daemon's broadcast sender; sources clone it so
/// they can emit `PollProgress` events during their fetch (drives
/// the polling-modal status line).
pub async fn sources_for(
    setup: &pilot_core::PersistedSetup,
    bus: tokio::sync::broadcast::Sender<Event>,
    state: &mut TickState,
    viewer_identities: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
) -> Vec<Box<dyn TaskSource>> {
    let mut sources: Vec<Box<dyn TaskSource>> = Vec::new();

    if setup.enabled_providers.contains(pilot_gh::SOURCE) {
        match pilot_gh::credential_chain().resolve(pilot_gh::SOURCE).await {
            Ok(cred) => {
                // Reuse the cached client when the credential source
                // is unchanged. `with_filters` consumes Self and
                // returns a new client with refreshed qualifiers —
                // the underlying `Arc<Mutex<RateBudget>>` is cloned,
                // so observations made by previous ticks (or by the
                // GhSource we hand out below) remain visible to the
                // cached copy and vice versa.
                let cred_source = cred.source.clone();
                let cached = state
                    .gh_client
                    .take()
                    .filter(|c| c.credential_source() == cred_source.as_str());
                let client_result: Result<GhClient, _> = match cached {
                    Some(existing) => Ok(existing),
                    None => GhClient::from_credential(cred).await,
                };
                match client_result {
                    Ok(client) => {
                        let filter = setup.provider_config("github");
                        let scopes = setup
                            .selected_scopes
                            .get("github")
                            .cloned()
                            .unwrap_or_default();
                        let pr_qualifiers =
                            build_pr_search_qualifiers(&filter, &scopes, client.username());
                        let issue_qualifiers =
                            build_issue_search_qualifiers(&filter, &scopes, client.username());
                        // `with_filters` returns a new owned client
                        // sharing the same budget Arc — `.clone()` on
                        // the result is cheap and keeps the cache in
                        // sync with what GhSource holds.
                        let client = client.with_filters(pr_qualifiers, issue_qualifiers);
                        // Cache + announce the authenticated viewer
                        // login so the TUI can render `@me` for the
                        // local user's bylines. Diffs the cache so we
                        // only broadcast when the value actually
                        // changes (token rotation, credential
                        // refresh, …) — quiet on the steady-state
                        // poll loop.
                        let viewer = client.username().to_string();
                        if !viewer.is_empty() {
                            let mut logins = viewer_identities
                                .lock()
                                .expect("viewer_identities poisoned");
                            let entry = logins.iter_mut().find(|(src, _)| src == "github");
                            let changed = match entry {
                                Some((_, existing)) if *existing == viewer => false,
                                Some((_, existing)) => {
                                    *existing = viewer.clone();
                                    true
                                }
                                None => {
                                    logins.push(("github".into(), viewer.clone()));
                                    true
                                }
                            };
                            let snapshot = logins.clone();
                            drop(logins);
                            if changed {
                                let _ = bus.send(Event::ViewerIdentities { logins: snapshot });
                            }
                        }
                        state.gh_client = Some(client.clone());
                        sources.push(Box::new(GhSource {
                            client,
                            filter,
                            scopes,
                            bus: bus.clone(),
                        }));
                    }
                    Err(e) => tracing::warn!("github client init failed: {e}"),
                }
            }
            Err(e) => tracing::info!("github credentials not available: {e}"),
        }
    }

    if setup.enabled_providers.contains("linear") {
        match LinearClient::from_env() {
            Ok(client) => sources.push(Box::new(LinearSource {
                client,
                filter: setup.provider_config("linear"),
                bus: bus.clone(),
            })),
            Err(e) => tracing::info!("linear not configured: {e}"),
        }
    }

    sources
}

/// Convenience: build the default source set assuming both providers
/// are enabled with their default filters. Used by binaries that
/// bypass the setup screen (e.g. headless `pilot daemon start` in
/// CI). When a saved `PersistedSetup` exists in the store, prefer
/// that instead.
pub async fn default_sources(
    bus: tokio::sync::broadcast::Sender<Event>,
) -> Vec<Box<dyn TaskSource>> {
    let setup = pilot_core::PersistedSetup {
        enabled_providers: ["github".to_string(), "linear".to_string()]
            .into_iter()
            .collect(),
        enabled_agents: Default::default(),
        provider_filters: [
            ("github".into(), ProviderConfig::default_for("github")),
            ("linear".into(), ProviderConfig::default_for("linear")),
        ]
        .into_iter()
        .collect(),
        // Empty selected_scopes = "all scopes" (legacy behavior).
        selected_scopes: Default::default(),
    };
    // No persistent state — this helper is for ad-hoc / test paths
    // where a fresh client per call is the right behavior. Viewer
    // identities also get a throwaway slot: ad-hoc callers don't
    // need the cached value visible to other connections.
    let mut throwaway_state = TickState::default();
    let throwaway_viewers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    sources_for(&setup, bus, &mut throwaway_state, throwaway_viewers).await
}

/// Run one poll tick: every source is called once and its tasks are
/// upserted. Errors from one source don't stop the others.
pub async fn tick(config: &ServerConfig, sources: &[Box<dyn TaskSource>]) -> TickOutcome {
    let mut state = TickState::default();
    tick_with_state(config, sources, &mut state).await
}

/// Per-loop state that the long-lived `spawn` task threads through
/// `tick` so we can debounce: re-broadcasting the same provider
/// error every 60s spams the TUI with identical hint-bar churn. We
/// only re-broadcast when the error message (or success/failure
/// classification) actually changes for a given source.
#[derive(Default)]
pub struct TickState {
    last_error: std::collections::HashMap<String, String>,
    /// Workspace keys we've already broadcast `WorkspaceOutOfScope`
    /// for. Without this, every 60s tick would re-prompt the user
    /// about the same workspace (they said "no" once, that's
    /// final). Re-entered into the polled set on the next successful
    /// poll that surfaces the workspace again — i.e. once the user
    /// re-adds the filter / scope, we forget the dismissal and the
    /// workspace can produce a fresh prompt later if it falls out
    /// of scope again.
    prompted_out_of_scope: std::collections::HashSet<String>,
    /// Issue workspace keys we've already broadcast
    /// `WorkspaceMergePending` for. Stays set until the matching
    /// `Command::ConfirmMerge` lands (or the daemon restarts) so a
    /// user staring at the modal doesn't get a stream of duplicate
    /// prompts on every poll tick.
    pub(crate) prompted_merge: std::collections::HashSet<String>,
    /// Issue workspace keys for which the user replied "no" to the
    /// merge prompt. We don't re-prompt this session — the user can
    /// always merge by hand via the future adopt-sessions flow.
    pub(crate) rejected_merge: std::collections::HashSet<String>,
    /// Persistent GhClient across ticks. WITHOUT this, every tick
    /// rebuilds the client via `GhClient::from_credential`, which
    /// resets the inner `RateBudget` to its full-bucket / no-remote-
    /// observation default. Result: the "GitHub said remaining=50,
    /// don't fire more requests" knowledge from the last tick is
    /// thrown away, and the new tick's first request flies blind
    /// straight into a 429. Reuse the client (and its budget Arc)
    /// across ticks so observations carry over; only swap when the
    /// credential SOURCE changes (env-var renamed, gh auth login
    /// switched accounts). A token rotation under the same source
    /// still requires a daemon restart — acceptable trade-off given
    /// how rare that is and how invasive validating each tick would
    /// be.
    gh_client: Option<GhClient>,
}

pub async fn tick_with_state(
    config: &ServerConfig,
    sources: &[Box<dyn TaskSource>],
    state: &mut TickState,
) -> TickOutcome {
    // Track every workspace key we upserted this tick. Callers use
    // it for "in scope" rescoping after the tick — anything in the
    // store NOT in this set is a candidate for removal.
    let mut polled: Vec<WorkspaceKey> = Vec::new();
    // Per-source success tracking. Rescoping needs "did anyone
    // actually report?" — a genuinely empty result set (filter
    // matches nothing) is data; "all sources errored" is not.
    let mut any_source_succeeded = false;
    // Longest "retry after N seconds" hint surfaced by any source
    // this tick. Plumbed back into the driver loop so we sleep at
    // least that long before the next attempt — without it we'd
    // keep firing the same rate-limited query at the normal cadence
    // and watch the budget stay pegged.
    let mut max_retry_after_secs: Option<u64> = None;
    for source in sources {
        match source.fetch().await {
            Ok(tasks) => {
                any_source_succeeded = true;
                let count = tasks.len();
                tracing::info!(source = source.name(), count, "poll succeeded");
                for task in tasks {
                    let key = WorkspaceKey::new(pilot_core::workspace_key_for(&task));
                    polled.push(key);
                    upsert(config, task).await;
                }
                // Clear the debounce slot — the next failure should
                // broadcast even if it carries the same message as a
                // previous run.
                state.last_error.remove(source.name());
                // Always emit `PollCompleted`, even on 0 tasks, so
                // the TUI can distinguish "polling hasn't run yet"
                // from "polling found nothing matching your filter".
                let _ = config.bus.send(Event::PollCompleted {
                    source: source.name().to_string(),
                    count,
                });
            }
            Err(e) => {
                if e.is_retryable() {
                    tracing::warn!(diagnostic = %e.diagnostic(), "poll failed (retryable)");
                } else if e.is_auth() {
                    tracing::error!(diagnostic = %e.diagnostic(), "poll failed (auth)");
                } else {
                    tracing::error!(diagnostic = %e.diagnostic(), "poll failed (permanent)");
                }
                let kind = if e.is_retryable() {
                    "retryable"
                } else if e.is_auth() {
                    "auth"
                } else {
                    "permanent"
                };
                // Capture the longest retry-after hint across all
                // failing sources this tick. Provider gave us a
                // precise number (GitHub's rateLimit.resetAt) —
                // honor it.
                if let Some(secs) = e.retry_after_secs() {
                    max_retry_after_secs =
                        Some(max_retry_after_secs.map_or(secs, |existing| existing.max(secs)));
                }
                // Debounce: only emit a ProviderError if the message
                // changed since the last failure for this source.
                // Same rate-limit error every minute → one event,
                // not 60/hour.
                let msg = e.user_message();
                let prev = state.last_error.get(source.name());
                if prev.map(String::as_str) != Some(msg.as_str()) {
                    state
                        .last_error
                        .insert(source.name().to_string(), msg.clone());
                    let _ = config.bus.send(Event::ProviderError {
                        source: e.source().to_string(),
                        message: msg,
                        detail: e.diagnostic(),
                        kind: kind.to_string(),
                    });
                }
            }
        }
    }
    TickOutcome {
        polled,
        any_source_succeeded,
        retry_after_secs: max_retry_after_secs,
    }
}

/// What `tick` / `tick_with_state` returns. The list of workspace
/// keys polled into the store, plus a "did anyone actually report?"
/// flag so callers (rescope) can distinguish "filter genuinely
/// matches nothing today" from "every source failed".
#[derive(Debug, Default)]
pub struct TickOutcome {
    pub polled: Vec<WorkspaceKey>,
    pub any_source_succeeded: bool,
    /// Longest "wait at least N seconds before retrying" hint
    /// surfaced by any source this tick — populated when a provider
    /// reports a precise reset window (GitHub's `rateLimit.resetAt`,
    /// HTTP `Retry-After`, …). The polling loop's outer driver uses
    /// this to extend the sleep before the next tick, instead of
    /// blindly tick-tick-ticking at the configured cadence and
    /// burning the same rate-limit error each time.
    pub retry_after_secs: Option<u64>,
}

/// Compare `polled` against the persisted workspace set; remove
/// workspaces no longer in scope (filter / scope change). Active
/// sessions are preserved — those workspaces stay until the user
/// kills them explicitly (or, in a future phase, confirms removal
/// via a prompt).
///
/// Empty `polled` is treated as "no data this cycle" and skipped —
/// otherwise a single network blip would wipe the whole sidebar.
/// Callers that genuinely want a fresh slate should delete
/// workspaces directly.
pub async fn rescope(config: &ServerConfig, outcome: &TickOutcome) {
    let mut state = TickState::default();
    rescope_with_state(config, outcome, &mut state).await;
}

pub async fn rescope_with_state(
    config: &ServerConfig,
    outcome: &TickOutcome,
    state: &mut TickState,
) {
    // No source produced a successful response — every provider
    // errored out (rate limit, network, auth). Treat as a transient
    // hiccup and skip the rescope; otherwise a single bad minute
    // would wipe the whole sidebar.
    if !outcome.any_source_succeeded {
        return;
    }
    let polled_set: std::collections::HashSet<&str> =
        outcome.polled.iter().map(|k| k.as_str()).collect();
    // Anything we polled is back in scope — drop any "already
    // prompted" memory for it so a future fall-out triggers a fresh
    // prompt.
    state
        .prompted_out_of_scope
        .retain(|k| !polled_set.contains(k.as_str()));

    let records = match config.store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("rescope: list_workspaces failed: {e}");
            return;
        }
    };

    // Per session_key → count of live terminals. Lets us both
    // detect "has active session" and report the count to the user
    // when prompting.
    let terminal_meta = config.terminal_meta.lock().await;
    let mut active_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (sk, _) in terminal_meta.values() {
        *active_counts.entry(sk.as_str().to_string()).or_default() += 1;
    }
    drop(terminal_meta);

    for r in records {
        if polled_set.contains(r.key.as_str()) {
            continue;
        }
        let key = WorkspaceKey::new(r.key.clone());
        match active_counts.get(r.key.as_str()).copied() {
            None | Some(0) => {
                // Safe to remove silently: nothing's running.
                tracing::info!(
                    workspace_key = %r.key,
                    "rescope: removing out-of-scope workspace"
                );
                delete_workspace(config, &key).await;
                state.prompted_out_of_scope.remove(r.key.as_str());
            }
            Some(count) => {
                // Has active sessions — ask the user, once. Without
                // the dedupe, every 60s tick would re-fire the same
                // prompt for a workspace the user already dismissed.
                if state.prompted_out_of_scope.contains(r.key.as_str()) {
                    continue;
                }
                state.prompted_out_of_scope.insert(r.key.clone());
                // Build a short label + title from the stored workspace
                // JSON if available; fall back to the raw key.
                //
                // `task.id.key` is already `owner/repo#N` (e.g.
                // `tensorzero/tensorzero#7307`) — concatenating `repo`
                // in front of it previously produced
                // `tensorzero/tensorzero#tensorzero/tensorzero#7307`.
                // Trust `id.key` and only fall back to `repo` when the
                // key is missing.
                let task_ref = r
                    .workspace_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<pilot_core::Workspace>(json).ok())
                    .and_then(|w| w.primary_task().cloned());
                let (label, title) = match task_ref {
                    Some(t) => {
                        let label = if !t.id.key.is_empty() {
                            t.id.key.clone()
                        } else if let Some(repo) = &t.repo {
                            repo.clone()
                        } else {
                            r.key.clone()
                        };
                        let title = if !t.title.is_empty() {
                            Some(t.title.clone())
                        } else {
                            None
                        };
                        (label, title)
                    }
                    None => (r.key.clone(), None),
                };
                tracing::info!(
                    workspace_key = %r.key,
                    active = count,
                    "rescope: out of scope with active sessions — prompting"
                );
                let _ = config.bus.send(Event::WorkspaceOutOfScope {
                    workspace_key: key,
                    label,
                    title,
                    active_terminal_count: count,
                });
            }
        }
    }
}

/// Spawn the long-lived polling loop. Returns the join handle so the
/// caller can `abort()` on shutdown if it wants — `pilot daemon stop`
/// drops the whole process so we don't bother in main.
///
/// Each tick reads `~/.pilot/config.yaml` fresh and rebuilds the
/// source list. This means a filter / scope change made via the
/// Settings palette takes effect on the NEXT tick at the latest —
/// no separate "respawn polling" plumbing needed, and the previous
/// per-Finish-respawn pattern (which leaked one tokio task per
/// edit) is gone.
pub fn spawn(config: ServerConfig, interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // First iteration also runs immediately — the loop
            // structure differs from the previous `tokio::time::interval`
            // tick-then-act dance because we want the sleep duration
            // computed AFTER each tick (rate-limit hints override
            // the normal cadence). Trade-off: tick-jitter accumulates
            // over hours; rate-limit honoring is more important.
            let retry_after = run_one_tick(&config).await;
            let sleep_for = match retry_after {
                Some(secs) => interval.max(Duration::from_secs(secs)),
                None => interval,
            };
            if retry_after.is_some() {
                tracing::warn!(
                    "polling: backing off {}s before next tick (rate-limit hint)",
                    sleep_for.as_secs(),
                );
            }
            tokio::time::sleep(sleep_for).await;
        }
    })
}

/// Test-only entry point: spawn a polling loop with an explicit
/// source list (skips the YAML reload). Production code should use
/// `spawn`; this exists so tests can inject mock `TaskSource`s
/// without writing a config file.
#[doc(hidden)]
pub fn spawn_with_sources(
    config: ServerConfig,
    sources: Vec<Box<dyn TaskSource>>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if sources.is_empty() {
            tracing::warn!("no provider sources configured — polling task is idle");
            return;
        }
        let mut state = TickState::default();
        loop {
            let outcome = tick_with_state(&config, &sources, &mut state).await;
            let retry_after = outcome.retry_after_secs;
            rescope_with_state(&config, &outcome, &mut state).await;
            let sleep_for = match retry_after {
                Some(secs) => interval.max(Duration::from_secs(secs)),
                None => interval,
            };
            tokio::time::sleep(sleep_for).await;
        }
    })
}

/// Single iteration of the poll loop. Loads the latest persisted
/// setup, builds sources, ticks, rescopes. Shared between the
/// long-lived spawn and the `Command::Refresh` immediate-tick path.
/// Uses `config.poll_state` so prompt-dismissal memory crosses both
/// paths.
pub async fn run_one_tick(config: &ServerConfig) -> Option<u64> {
    let setup = match pilot_config::Config::load() {
        Ok(c) => crate::persisted_from_config(&c),
        Err(e) => {
            tracing::warn!("polling: config.yaml load failed: {e}");
            return None;
        }
    };
    // Hold the lock across the entire tick — `sources_for` needs
    // mutable access to the cached GhClient, then `tick_with_state`
    // needs `&mut state` for the debounce / prompted-set bookkeeping.
    // No other writer needs the lock briefly during a tick, so this
    // is safe and avoids the lock-twice + state-drift risk.
    let mut state = config.poll_state.lock().await;
    let sources = sources_for(
        &setup,
        config.bus.clone(),
        &mut state,
        config.viewer_identities.clone(),
    )
    .await;
    if sources.is_empty() {
        // User disabled every provider (or credentials all
        // failed to resolve). Treat as "deliberately empty
        // result" — rescope so existing workspaces actually
        // disappear from the sidebar. Without this, unchecking
        // every provider leaves the inbox frozen with stale
        // rows that no current poll source could produce.
        let outcome = TickOutcome {
            polled: vec![],
            any_source_succeeded: true,
            retry_after_secs: None,
        };
        rescope_with_state(config, &outcome, &mut state).await;
        return None;
    }
    let outcome = tick_with_state(config, &sources, &mut state).await;
    let retry_after = outcome.retry_after_secs;
    rescope_with_state(config, &outcome, &mut state).await;
    retry_after
}

/// Merge `task` into the existing workspace for its workspace key
/// (PR + matching issues collapse to one row), persist it, and
/// broadcast `WorkspaceUpserted`. The store backs this with
/// `workspace:<key>` rows in the kv table — no schema migration
/// needed.
///
/// Read state (`seen_count`, `read_indices`, `snoozed_until`,
/// `last_viewed_at`) is preserved across updates — providers only
/// own upstream-derived fields.
///
/// ## Issue → PR collapsing
///
/// GitHub PRs can link to issues via `closingIssuesReferences` (the
/// canonical "Closes #N" / "Fixes #N" mapping). When we observe a PR
/// whose closes_issues lists an issue that already lives in its own
/// standalone workspace, we **merge** the issue workspace into the
/// PR workspace — moving its sessions over (terminals keep running)
/// and dropping the standalone row. Conversely, when an issue is
/// polled and a PR already claims it, we route the issue update into
/// the PR workspace instead of building a duplicate.
pub async fn upsert(config: &ServerConfig, task: Task) {
    // For issues: if a PR somewhere already claims this issue as
    // closed-by, route the upsert into that PR workspace. This is
    // the "issue polled AFTER its PR" path. We only kick in when
    // the issue has no standalone workspace yet — once one exists,
    // either the PR poll will collapse them or the issue's own row
    // remains until the PR shows up. Polling is cheap to scan: the
    // workspace list is bounded by the user's filter scope.
    if !is_pr_task(&task) {
        let issue_key_str = pilot_core::workspace_key_for(&task);
        let issue_key = WorkspaceKey::new(issue_key_str);
        let already_standalone = config
            .store
            .get_workspace(&issue_key)
            .ok()
            .flatten()
            .is_some();
        if !already_standalone && let Some(pr_key) = pr_workspace_claiming_issue(config, &task.id) {
            tracing::info!(
                issue = %task.id,
                pr_workspace = %pr_key,
                "routing issue upsert into PR workspace (closingIssuesReferences)"
            );
            upsert_into_workspace_key(config, &pr_key, task).await;
            return;
        }
    }

    let key_str = pilot_core::workspace_key_for(&task);
    let key = WorkspaceKey::new(key_str.clone());
    upsert_into_workspace_key(config, &key, task).await;
}

/// Inner upsert: load workspace at `key`, attach the task, migrate
/// linked-issue workspaces if the task is a PR with closing refs,
/// then persist + broadcast. Split out from `upsert` so the
/// "route to PR workspace" path can reuse the same write/broadcast
/// behaviour without duplicating it.
async fn upsert_into_workspace_key(config: &ServerConfig, key: &WorkspaceKey, task: Task) {
    // 1. PREPARE: build the workspace's final in-memory state.
    //    Includes the optional issue-collapse merge — if a PR
    //    polls in with `closes_issues`, we fold standalone issue
    //    workspaces into it here. Async (touches the store +
    //    `terminal_meta`) but doesn't yet write the PR's own row.
    let mut workspace = prepare_upsert(config, key, task).await;

    // 2. MIGRATE: rename worktree dirs to match the (possibly
    //    new) PR slug. Async git operation. If it fails, log
    //    loudly but continue to commit the metadata — the next
    //    spawn re-provisions paths and a partial mismatch is
    //    survivable; a missing broadcast is not.
    crate::spawn_handler::migrate_session_paths_if_needed(config, &mut workspace).await;

    // 3. COMMIT: persist the final state + broadcast it. Failures
    //    here log at `error` so an operator can spot a workspace
    //    that won't survive restart.
    commit_upsert(config, key, workspace);
}

/// Pure-ish prepare step: load the existing workspace (if any),
/// attach the incoming task, and run the issue-collapse merge. No
/// store writes, no `WorkspaceUpserted` broadcast — the returned
/// `Workspace` is what we'll commit in step 3.
///
/// Split out from `upsert_into_workspace_key` so a future test can
/// drive the prepare step against a mock store without committing
/// real state — the "did the merge attach the issue task?" question
/// is now answerable without the full IPC bus + store side effects.
async fn prepare_upsert(config: &ServerConfig, key: &WorkspaceKey, task: Task) -> Workspace {
    let existing = config
        .store
        .get_workspace(key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
        .and_then(|j| serde_json::from_str::<Workspace>(&j).ok());

    let mut workspace = match existing {
        Some(mut w) => {
            w.attach_task(task);
            w
        }
        None => Workspace::from_task(task, Utc::now()),
    };

    // Issue-collapse pass — see `merge_closing_issue_workspaces`.
    // Happens here (in prepare) so the migration step sees the
    // final session set and renames worktrees in one pass.
    merge_closing_issue_workspaces(config, &mut workspace).await;
    workspace
}

/// Side-effect-only commit: serialize + persist + broadcast.
/// Pulled out so the failure modes are isolated — a store-write
/// error doesn't suppress the bus broadcast, and a bus-send error
/// doesn't take down the daemon.
///
/// Also ensures the Project this workspace belongs to is registered.
/// `Workspace::from_task` populates `project_key` from the task's
/// repo string; we use that here to upsert a Project record so the
/// sidebar can render a header for it even before the user explicitly
/// creates the project. Idempotent — re-broadcasting an existing
/// record on every workspace upsert costs a bus send but keeps the
/// data model consistent with no extra bookkeeping.
fn commit_upsert(config: &ServerConfig, key: &WorkspaceKey, workspace: Workspace) {
    ensure_project_for_workspace(config, &workspace);
    // Serialization failure here means the workspace exists in memory
    // but won't survive a restart — and the silent `.ok()` previously
    // stored `None`, so the next process would read back an empty
    // record without any indication something went wrong. Log loudly
    // so a broken Serialize impl shows up in /tmp/pilot.log instead
    // of mysterious post-restart data loss.
    let json = match serde_json::to_string(&workspace) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                workspace_key = %key.as_str(),
                "commit_upsert: serde_json::to_string(workspace) failed: {e} \
                 — record will persist with NULL json (will read back empty)",
            );
            None
        }
    };
    let record = WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: json,
    };
    if let Err(e) = config.store.save_workspace(&record) {
        // Bumped to error: a store write failure means the
        // workspace we just broadcast won't survive a restart.
        // Caller side can't currently see this, but at least the
        // log is loud.
        tracing::error!(
            workspace_key = %record.key,
            "save_workspace failed: {e}"
        );
    }
    let _ = config
        .bus
        .send(Event::WorkspaceUpserted(Box::new(workspace)));
}

/// Ensure a Project record exists for the workspace's parent project,
/// upserting + broadcasting on first sight. Driven from `commit_upsert`,
/// so every workspace that flows through polling auto-registers its
/// containing project. Idempotent — calling repeatedly with the same
/// workspace re-saves and re-broadcasts the same Project record (cheap).
///
/// `Workspace::project_key` is populated by `Workspace::from_task` via
/// `pilot_core::project_key_for_task`. When it's `None` (back-compat
/// reads of pre-Project records, or a workspace with no upstream task),
/// we skip — Stage 1 doesn't try to back-fill projects for orphan
/// workspaces.
fn ensure_project_for_workspace(config: &ServerConfig, workspace: &Workspace) {
    let Some(project_key) = workspace.project_key.clone() else {
        return;
    };
    // Skip the write + broadcast if we've already registered this
    // project. Keeps bus traffic to one event per project per process
    // — without this, every workspace upsert would re-fire the project
    // event and consumers that drain "one event per upsert" would
    // desync (mark_workspace_read in particular).
    if matches!(config.store.get_project(&project_key), Ok(Some(_))) {
        return;
    }
    // Display name for the project. Prefer the workspace's
    // `primary_task().repo` (the "owner/repo" string) when present —
    // that's what the sidebar header has always shown. Fall back to
    // the project key's string form.
    let name = workspace
        .primary_task()
        .and_then(|t| t.repo.clone())
        .unwrap_or_else(|| project_key.as_str().to_string());
    let project = pilot_core::Project::new(project_key.clone(), name, Utc::now());
    let json = match serde_json::to_string(&project) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                project_key = %project_key,
                "ensure_project: serde_json::to_string(project) failed: {e}",
            );
            None
        }
    };
    let record = pilot_store::ProjectRecord {
        key: project_key.as_str().to_string(),
        created_at: project.created_at,
        project_json: json,
    };
    if let Err(e) = config.store.save_project(&record) {
        tracing::error!(
            project_key = %record.key,
            "save_project failed: {e}",
        );
    }
    let _ = config
        .bus
        .send(Event::ProjectUpserted(Box::new(project)));
}

/// Heuristic for "is this Task the PR side of a PR/issue pair?".
/// Single source of truth: [`Task::is_pr`] — same method
/// `workspace::classify` consults — so adding a new provider
/// (GitLab `/merge_requests/`, Bitbucket `/pull-requests/`, …)
/// only requires extending that one method.
fn is_pr_task(task: &Task) -> bool {
    task.is_pr()
}

/// Scan stored workspaces for one whose PR claims `issue_id` via
/// `closes_issues`. Returns the PR's workspace key when a match is
/// found. Linear in the workspace count — fine in practice (10s to
/// low 100s of workspaces).
fn pr_workspace_claiming_issue(
    config: &ServerConfig,
    issue_id: &pilot_core::TaskId,
) -> Option<WorkspaceKey> {
    let records = config.store.list_workspaces().ok()?;
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(ws) = serde_json::from_str::<Workspace>(&json) else {
            continue;
        };
        let Some(pr) = &ws.pr else {
            continue;
        };
        if pr.closes_issues.iter().any(|id| id == issue_id) {
            return Some(ws.key);
        }
    }
    None
}

/// If `workspace`'s PR closes issues that pilot tracks as their own
/// workspaces, fold each issue's workspace into `workspace` and
/// remove the standalone row. Sessions move over (terminals keep
/// running); `terminal_meta` is rewritten so wire-side events for
/// the old session_key flow to the new one.
///
/// Safety net: when the issue workspace has live sessions, we DON'T
/// merge silently — auto-absorbing a user's running Claude/codex
/// session into a different workspace key is too easy to miss. We
/// emit `WorkspaceMergePending` instead and stash the candidate;
/// the TUI prompts and replies via `Command::ConfirmMerge`. Empty
/// issue workspaces still merge silently and emit a
/// `WorkspaceMerged` notice so the user sees the row disappear
/// with context.
///
/// No-op when there's no PR, no `closes_issues`, or no matching
/// issue workspace exists yet.
async fn merge_closing_issue_workspaces(config: &ServerConfig, workspace: &mut Workspace) {
    let Some(pr) = workspace.pr.as_ref() else {
        return;
    };
    if pr.closes_issues.is_empty() {
        return;
    }

    let mut closed_ids: Vec<pilot_core::TaskId> = pr.closes_issues.clone();
    closed_ids.dedup();

    for issue_id in closed_ids {
        let issue_key = issue_id_to_workspace_key(&issue_id);
        if issue_key == workspace.key {
            // Self-link — nothing to merge.
            continue;
        }
        let Some(issue_ws) = load_workspace(config, &issue_key) else {
            continue;
        };

        // Critical safety check: only merge ACTUAL issue workspaces.
        // GitHub's `#N` syntax is the same for issues and PRs, and
        // our body-text fallback parser can't tell them apart from
        // the body alone — a PR body that says "Closes #141" where
        // #141 is itself a PR would otherwise have us absorb that
        // PR's workspace into this one. Symptom: PRs vanish from
        // the inbox shortly after each poll. (`closingIssuesReferences`
        // from GraphQL is safe — GitHub only returns issues there —
        // but we union both sources and have to filter here.)
        if issue_ws.pr.is_some() {
            tracing::debug!(
                target_workspace = %issue_key,
                source_pr = ?workspace.pr.as_ref().map(|p| &p.id),
                "skip merge: target is itself a PR workspace, not an issue"
            );
            continue;
        }

        // Live-session safety net: stall and prompt rather than
        // silently absorbing the user's running work. `prompted_merge`
        // dedupes so a user staring at the modal doesn't see fresh
        // copies every 60s; `rejected_merge` is the "no, leave them
        // separate" pin until pilot restarts.
        if !issue_ws.sessions.is_empty() {
            let issue_key_str = issue_key.as_str().to_string();
            let should_prompt = {
                let mut state = config.poll_state.lock().await;
                if state.rejected_merge.contains(&issue_key_str) {
                    false
                } else {
                    state.prompted_merge.insert(issue_key_str)
                }
            };
            if should_prompt {
                let _ = config.bus.send(Event::WorkspaceMergePending {
                    issue_workspace_key: issue_key.clone(),
                    pr_workspace_key: workspace.key.clone(),
                    issue_label: workspace_label_for(&issue_ws, &issue_key),
                    pr_label: workspace_label_for(workspace, &workspace.key),
                    active_terminal_count: issue_ws.sessions.len(),
                });
            }
            continue;
        }

        // Empty issue workspace — safe to merge silently. Emit a
        // notice so the user sees the row collapse rather than
        // mysteriously vanish.
        let issue_label = workspace_label_for(&issue_ws, &issue_key);
        let pr_label = workspace_label_for(workspace, &workspace.key);
        absorb_issue_workspace(config, workspace, issue_ws).await;
        if let Err(e) = config.store.delete_workspace(&issue_key) {
            tracing::warn!(
                issue_workspace = %issue_key,
                "delete_workspace during PR merge failed: {e}"
            );
        }
        let _ = config.bus.send(Event::WorkspaceRemoved(issue_key.clone()));
        let _ = config.bus.send(Event::WorkspaceMerged {
            issue_workspace_key: issue_key.clone(),
            pr_workspace_key: workspace.key.clone(),
            issue_label,
            pr_label,
        });

        tracing::info!(
            issue_workspace = %issue_key,
            pr_workspace = %workspace.key,
            "merged issue workspace into PR (closingIssuesReferences)"
        );
    }
}

/// The TUI replied to a `WorkspaceMergePending` prompt. Accept → run
/// the merge, persist + broadcast the absorbed PR workspace, drop the
/// stash. Reject → drop the stash + pin the issue key into
/// `rejected_merge` so we don't re-prompt this session.
pub async fn handle_confirm_merge(
    config: &ServerConfig,
    issue_workspace_key: WorkspaceKey,
    pr_workspace_key: WorkspaceKey,
    accept: bool,
) {
    {
        let mut state = config.poll_state.lock().await;
        state.prompted_merge.remove(issue_workspace_key.as_str());
        if !accept {
            state
                .rejected_merge
                .insert(issue_workspace_key.as_str().to_string());
        }
    }
    if !accept {
        tracing::info!(
            issue_workspace = %issue_workspace_key,
            "user rejected workspace merge; pinned for this session"
        );
        return;
    }

    let Some(mut pr_ws) = load_workspace(config, &pr_workspace_key) else {
        tracing::warn!(
            pr_workspace = %pr_workspace_key,
            "ConfirmMerge: PR workspace missing — aborting"
        );
        return;
    };
    let Some(issue_ws) = load_workspace(config, &issue_workspace_key) else {
        tracing::warn!(
            issue_workspace = %issue_workspace_key,
            "ConfirmMerge: issue workspace missing — aborting"
        );
        return;
    };
    // Defensive: refuse to absorb a PR workspace. The merge code
    // path is meant for ISSUE → PR collapse; if `issue_workspace_key`
    // somehow points at a PR (loose body-text parser, stale modal,
    // race), bail rather than destroy the PR row.
    if issue_ws.pr.is_some() {
        tracing::warn!(
            target_workspace = %issue_workspace_key,
            "ConfirmMerge: refusing to absorb a PR workspace into another PR"
        );
        return;
    }
    let issue_label = workspace_label_for(&issue_ws, &issue_workspace_key);
    let pr_label = workspace_label_for(&pr_ws, &pr_workspace_key);

    absorb_issue_workspace(config, &mut pr_ws, issue_ws).await;
    crate::spawn_handler::migrate_session_paths_if_needed(config, &mut pr_ws).await;

    if let Err(e) = config.store.delete_workspace(&issue_workspace_key) {
        tracing::warn!(
            issue_workspace = %issue_workspace_key,
            "delete_workspace during ConfirmMerge failed: {e}"
        );
    }
    let pr_key = pr_ws.key.clone();
    let _ = config
        .bus
        .send(Event::WorkspaceRemoved(issue_workspace_key.clone()));
    let _ = config.bus.send(Event::WorkspaceMerged {
        issue_workspace_key,
        pr_workspace_key: pr_key.clone(),
        issue_label,
        pr_label,
    });
    commit_upsert(config, &pr_key, pr_ws);
}

/// Manual "adopt": move every session out of `source_key`'s
/// workspace and into `target_key`'s, rebadging `terminal_meta` so
/// wire-side traffic follows them. Unlike the issue→PR merge, we
/// do NOT delete the source workspace — the user may still want
/// it as a tracking row (or remove it explicitly via `Shift-X`).
///
/// No-op when either workspace is missing or `source == target`.
pub async fn handle_adopt_sessions(
    config: &ServerConfig,
    source_key: WorkspaceKey,
    target_key: WorkspaceKey,
) {
    if source_key == target_key {
        return;
    }
    let Some(mut source_ws) = load_workspace(config, &source_key) else {
        tracing::warn!(
            source_workspace = %source_key,
            "AdoptSessions: source workspace missing — aborting"
        );
        return;
    };
    let Some(mut target_ws) = load_workspace(config, &target_key) else {
        tracing::warn!(
            target_workspace = %target_key,
            "AdoptSessions: target workspace missing — aborting"
        );
        return;
    };
    if source_ws.sessions.is_empty() {
        tracing::info!(
            source_workspace = %source_key,
            "AdoptSessions: source has no sessions — nothing to move"
        );
        return;
    }

    let source_session_key: pilot_core::SessionKey = (&source_key).into();
    let target_session_key: pilot_core::SessionKey = (&target_key).into();
    let moved = source_ws.sessions.len();
    for mut session in source_ws.sessions.drain(..) {
        session.workspace_key = target_key.clone();
        target_ws.add_session(session);
    }
    let mut meta = config.terminal_meta.lock().await;
    for (_tid, entry) in meta.iter_mut() {
        if entry.0 == source_session_key {
            entry.0 = target_session_key.clone();
        }
    }
    drop(meta);

    crate::spawn_handler::migrate_session_paths_if_needed(config, &mut target_ws).await;

    tracing::info!(
        source_workspace = %source_key,
        target_workspace = %target_key,
        moved,
        "adopted sessions across workspaces"
    );

    let source_key_owned = source_ws.key.clone();
    let target_key_owned = target_ws.key.clone();
    commit_upsert(config, &source_key_owned, source_ws);
    commit_upsert(config, &target_key_owned, target_ws);
}

/// Move `issue_ws`'s sessions, gh/linear-issue tasks, and any
/// terminal_meta entries onto `pr_workspace`. Caller is responsible
/// for deleting the issue workspace from the store and broadcasting
/// the `WorkspaceRemoved` / `WorkspaceUpserted` / `WorkspaceMerged`
/// events around the call.
async fn absorb_issue_workspace(
    config: &ServerConfig,
    pr_workspace: &mut Workspace,
    issue_ws: Workspace,
) {
    let issue_session_key: pilot_core::SessionKey = (&issue_ws.key).into();
    let pr_session_key: pilot_core::SessionKey = (&pr_workspace.key).into();

    for mut session in issue_ws.sessions {
        session.workspace_key = pr_workspace.key.clone();
        pr_workspace.add_session(session);
    }
    for issue_task in &issue_ws.gh_issues {
        pr_workspace.attach_task(issue_task.clone());
    }
    for issue_task in &issue_ws.linear_issues {
        pr_workspace.attach_task(issue_task.clone());
    }

    let mut meta = config.terminal_meta.lock().await;
    for (_tid, entry) in meta.iter_mut() {
        if entry.0 == issue_session_key {
            entry.0 = pr_session_key.clone();
        }
    }
}

/// Synthesize the workspace key an issue TaskId would have produced
/// when first upserted as a standalone workspace.
fn issue_id_to_workspace_key(issue_id: &pilot_core::TaskId) -> WorkspaceKey {
    let stub = Task {
        id: issue_id.clone(),
        title: String::new(),
        body: None,
        state: pilot_core::TaskState::Open,
        role: pilot_core::TaskRole::Author,
        ci: pilot_core::CiStatus::None,
        review: pilot_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: String::new(),
        repo: None,
        branch: None,
        base_branch: None,
        updated_at: Utc::now(),
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        has_conflicts: false,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
    };
    WorkspaceKey::new(pilot_core::workspace_key_for(&stub))
}

fn load_workspace(config: &ServerConfig, key: &WorkspaceKey) -> Option<Workspace> {
    let record = config.store.get_workspace(key).ok().flatten()?;
    let json = record.workspace_json?;
    serde_json::from_str::<Workspace>(&json).ok()
}

/// `owner/repo#N` for PR / issue rows; falls back to the workspace
/// key string otherwise. Used in the confirm modal + footer notice.
fn workspace_label_for(workspace: &Workspace, key: &WorkspaceKey) -> String {
    workspace
        .primary_task()
        .map(|t| t.id.key.clone())
        .unwrap_or_else(|| key.as_str().to_string())
}

/// Create an empty workspace (no PR, no issues) named by the user.
/// Generates a `WorkspaceKey` from the name's slug, disambiguating
/// with a numeric suffix if a workspace with that key already
/// exists. Persists + broadcasts `WorkspaceUpserted`.
///
/// Returns the new key so the caller (sidebar, tests) can land the
/// cursor on the freshly-created row.
pub fn create_empty_workspace(
    config: &ServerConfig,
    name: &str,
    project_key: pilot_core::ProjectKey,
) -> WorkspaceKey {
    let base = pilot_core::slug::slugify(name);
    let base = if base.is_empty() {
        "workspace".to_string()
    } else {
        base
    };
    // Collision: try `<base>`, `<base>-2`, `<base>-3`, ... until the
    // store reports no existing record.
    let key = (1..)
        .map(|i| {
            if i == 1 {
                WorkspaceKey::new(base.clone())
            } else {
                WorkspaceKey::new(format!("{base}-{i}"))
            }
        })
        .find(|k| {
            config
                .store
                .get_workspace(k)
                .ok()
                .flatten()
                .and_then(|r| r.workspace_json)
                .is_none()
        })
        .expect("infinite range yields a free key");

    let mut workspace = Workspace::empty(key.clone(), "main", Utc::now());
    if !name.trim().is_empty() {
        workspace.name = name.trim().to_string();
    }
    workspace.project_key = Some(project_key);
    commit_upsert(config, &key, workspace);
    key
}

/// Create (or re-open) a local Project by name. Slugifies the name,
/// builds a `local-<slug>` ProjectKey, persists a Project record,
/// and broadcasts `ProjectUpserted` so the sidebar can render the
/// new header immediately. Idempotent: calling with the same name
/// twice opens the existing project — projects are named
/// containers, like directories, so this matches user expectation.
///
/// Returns the project key so the caller (TUI) can land focus on
/// the new header.
pub fn create_local_project(config: &ServerConfig, name: &str) -> pilot_core::ProjectKey {
    let base = pilot_core::slug::slugify(name);
    let slug = if base.is_empty() {
        "project".to_string()
    } else {
        base
    };
    let key = pilot_core::ProjectKey::local(&slug);
    // Idempotent: re-broadcast the existing record on collision.
    let display_name = if name.trim().is_empty() {
        slug.clone()
    } else {
        name.trim().to_string()
    };
    let project = match config.store.get_project(&key) {
        Ok(Some(record)) => record
            .project_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<pilot_core::Project>(j).ok())
            .unwrap_or_else(|| pilot_core::Project::new(key.clone(), &display_name, Utc::now())),
        _ => pilot_core::Project::new(key.clone(), &display_name, Utc::now()),
    };
    let json = match serde_json::to_string(&project) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                project_key = %key,
                "create_local_project: serde_json::to_string(project) failed: {e}",
            );
            None
        }
    };
    let record = pilot_store::ProjectRecord {
        key: key.as_str().to_string(),
        created_at: project.created_at,
        project_json: json,
    };
    if let Err(e) = config.store.save_project(&record) {
        tracing::error!(
            project_key = %record.key,
            "save_project failed: {e}",
        );
    }
    let _ = config
        .bus
        .send(Event::ProjectUpserted(Box::new(project)));
    key
}

/// One-shot post-Stage-4 migration: if a pre-refactor `sandbox`
/// workspace exists in the store with no `project_key`, create a
/// "Sandbox" local Project and stamp the workspace with it so the
/// row reappears under a real Project header instead of landing in
/// `(no repo)`. Idempotent — already-migrated workspaces (project_key
/// set) are left alone; a missing sandbox workspace is a no-op.
///
/// Called once at daemon startup from both `run_embedded_realm` and
/// `server_start` so each pilot launch self-heals legacy state.
pub fn migrate_legacy_sandbox(config: &ServerConfig) {
    let key = WorkspaceKey::new("sandbox".to_string());
    let Some(record) = config.store.get_workspace(&key).ok().flatten() else {
        return;
    };
    let Some(json) = record.workspace_json else {
        return;
    };
    let mut workspace: Workspace = match serde_json::from_str(&json) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                "migrate_legacy_sandbox: failed to parse stored workspace: {e}"
            );
            return;
        }
    };
    if workspace.project_key.is_some() {
        // Already migrated — skip.
        return;
    }
    let project_key = create_local_project(config, "Sandbox");
    workspace.project_key = Some(project_key);
    let ws_key = workspace.key.clone();
    commit_upsert(config, &ws_key, workspace);
    tracing::info!("migrate_legacy_sandbox: moved `sandbox` workspace under `local-sandbox` project");
}

/// Set or clear the workspace's `snoozed_until` timestamp. `None`
/// un-snoozes. Persists + broadcasts so the sidebar's mailbox-aware
/// rendering re-categorises the row.
pub fn set_snooze(config: &ServerConfig, key: &WorkspaceKey, until: Option<chrono::DateTime<Utc>>) {
    let Some(mut workspace) = load_workspace(config, key) else {
        return;
    };
    workspace.snoozed_until = until;
    commit_upsert(config, key, workspace);
}

/// Delete a workspace + all its sessions from the store. Broadcasts
/// `WorkspaceRemoved` so every connected TUI prunes its sidebar row.
/// Used by the sidebar's `Shift-X` two-press kill flow.
///
/// Does NOT delete the worktree directories on disk — that's a
/// future enhancement (needs to also kill any live PTY runners
/// rooted in those paths). For now we just drop the metadata; the
/// worktree dirs survive as ordinary git checkouts the user can
/// reuse or remove manually.
///
/// Also kills every backing terminal (PTY / tmux session) that
/// belonged to the workspace — without this the user's `Shift-X X`
/// hides the tabs in pilot but leaves ghost tmux sessions visible
/// in `tmux ls`, which then re-surface on the next pilot launch
/// via `recover_sessions`.
pub async fn delete_workspace(config: &ServerConfig, key: &WorkspaceKey) {
    let key_str = key.as_str();

    // Find every terminal whose session_key matches via
    // terminal_meta — the authoritative wire-side mapping. Earlier
    // we parsed the backend_key prefix, but the backend's session
    // name format isn't part of any contract (tmux now uses
    // `pilot-{repo}-{kind}-{pid}-{n}`); the meta map is. Locks are
    // taken + dropped before async backend.kill() calls.
    let to_kill_ids: Vec<pilot_ipc::TerminalId> = {
        let meta = config.terminal_meta.lock().await;
        meta.iter()
            .filter(|(_, (sk, _))| sk.as_str() == key_str)
            .map(|(tid, _)| *tid)
            .collect()
    };
    let to_kill: Vec<(pilot_ipc::TerminalId, String)> = {
        let terminals = config.terminals.lock().await;
        to_kill_ids
            .into_iter()
            .filter_map(|tid| terminals.get(&tid).map(|k| (tid, k.clone())))
            .collect()
    };

    if !to_kill.is_empty() {
        tracing::info!(
            "delete_workspace {key}: killing {} backing terminal(s)",
            to_kill.len()
        );
        for (tid, backend_key) in to_kill {
            if let Err(e) = config.backend.kill(&backend_key).await {
                tracing::warn!("kill {backend_key}: {e}");
            }
            // Clean every auxiliary map too. The pump task will
            // ALSO clean these when wait_exit returns, but that
            // happens on a tokio task with no upper bound on
            // latency. Doing it here closes the window where
            // rescope (or another subsystem) would see an entry
            // for a workspace we just deleted.
            config.terminals.lock().await.remove(&tid);
            config.terminal_meta.lock().await.remove(&tid);
            config.terminal_sessions.lock().await.remove(&tid);
            config.agent_states.lock().await.remove(&tid);
            // Mirror the daemon-pump's exit broadcast so any
            // still-connected clients see the tab disappear.
            let _ = config.bus.send(Event::TerminalExited {
                terminal_id: tid,
                exit_code: None,
            });
        }
    }

    if let Err(e) = config.store.delete_workspace(key) {
        tracing::warn!("delete_workspace failed: {e}");
    }
    let _ = config.bus.send(Event::WorkspaceRemoved(key.clone()));
}

/// Persist a new `SessionLayout` for one session inside a workspace.
/// The user's tile arrangement (Tabs vs Splits with a tree) is local
/// to the workspace; this writes it through the store and broadcasts
/// `WorkspaceUpserted` so other clients see the new layout.
///
/// No-op when the workspace or session can't be found.
pub fn set_session_layout(
    config: &ServerConfig,
    key: &WorkspaceKey,
    session_id: pilot_core::SessionId,
    layout: pilot_core::SessionLayout,
) {
    let Some(mut workspace) = load_workspace(config, key) else {
        return;
    };
    let Some(session) = workspace.sessions.iter_mut().find(|s| s.id == session_id) else {
        tracing::debug!("set_session_layout: no session {session_id} in {key}");
        return;
    };
    session.layout = layout;
    commit_upsert(config, key, workspace);
}

/// Apply a partial-mark to one activity row. Used by the TUI's
/// auto-mark-on-hover feature so the user can scroll past comments
/// and have them flip read individually, instead of `MarkRead`'s
/// "flip the whole workspace" behavior. Persists + broadcasts.
///
/// No-op when the workspace isn't in the store or `index` is out of
/// range — both are user-driven inputs and we don't want a TUI race
/// (poll deletes a workspace while the user hovers) to crash the
/// daemon.
pub fn mark_activity_read(config: &ServerConfig, key: &WorkspaceKey, index: usize) {
    apply_activity_mark(config, key, index, /*read=*/ true);
}

/// Reverse of `mark_activity_read`. `z` undo binds here.
pub fn unmark_activity_read(config: &ServerConfig, key: &WorkspaceKey, index: usize) {
    apply_activity_mark(config, key, index, /*read=*/ false);
}

fn apply_activity_mark(config: &ServerConfig, key: &WorkspaceKey, index: usize, read: bool) {
    let Some(mut workspace) = load_workspace(config, key) else {
        tracing::debug!("apply_activity_mark: no record for {key}");
        return;
    };
    if read {
        workspace.mark_activity_read(index);
    } else {
        workspace.unmark_activity_read(index);
    }
    commit_upsert(config, key, workspace);
}

/// Apply the user's "mark every activity item read" gesture to a
/// stored workspace and broadcast the change. Activity-seen state is
/// **independent** of the upstream provider state: providers only ever
/// rewrite the activity feed itself; `seen_count` + `read_indices`
/// belong to the local user. Preserving them across polls happens in
/// `upsert`; this function flips them all-read on demand.
///
/// No-op if the workspace isn't in the store.
pub fn mark_workspace_read(config: &ServerConfig, key: &WorkspaceKey) {
    let Some(mut workspace) = load_workspace(config, key) else {
        tracing::debug!("mark_workspace_read: no record for {key}");
        return;
    };
    workspace.mark_read_all();
    workspace.last_viewed_at = Some(Utc::now());
    commit_upsert(config, key, workspace);
}

/// Post a top-level reply to the workspace's primary task. Today this
/// targets only GitHub PRs/issues; Linear and other providers can grow
/// into the same shape. On success we don't update the local activity
/// feed inline — the next poll picks up the new comment, which keeps
/// the "what the upstream provider says" invariant intact.
pub async fn post_reply(config: &ServerConfig, session_key: pilot_core::SessionKey, body: String) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return;
    }
    let workspace_key = WorkspaceKey::new(session_key.as_str().to_string());
    let workspace = match config
        .store
        .get_workspace(&workspace_key)
        .ok()
        .flatten()
        .and_then(|r| r.workspace_json)
    {
        Some(json) => match serde_json::from_str::<Workspace>(&json) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("post_reply: bad JSON for {workspace_key}: {e}");
                emit_reply_error(config, &format!("workspace decode failed: {e}"));
                return;
            }
        },
        None => {
            emit_reply_error(config, "workspace not found");
            return;
        }
    };
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_reply_error(config, &e);
            return;
        }
    };
    if let Err(e) = provider.post_reply(&workspace, trimmed).await {
        tracing::warn!("post_reply {workspace_key}: {e:?}");
        emit_reply_error(config, &format!("post failed: {e}"));
        return;
    }
    tracing::info!(
        "posted reply to {} ({} chars)",
        workspace_key,
        trimmed.len()
    );
    // The poller picks up the comment on its next tick and broadcasts
    // a workspace upsert; nothing else to do here.
}

fn emit_reply_error(config: &ServerConfig, msg: &str) {
    let _ = config.bus.send(Event::ProviderError {
        source: "reply".into(),
        message: msg.to_string(),
        detail: String::new(),
        kind: "retryable".into(),
    });
}

/// Runtime-polymorphic wrapper around the workspace's
/// `TaskProvider`. Using an enum (vs `Arc<dyn TaskProvider>`) is
/// deliberate: the trait uses `async fn` which isn't dyn-compatible
/// without the `async_trait` crate. The enum dispatches manually
/// in O(n_providers) — fine for the 2-3 providers pilot will
/// ever have at once.
///
/// Adding a new provider: add a variant + the four `match` arms
/// below. Each arm delegates to the provider's `TaskProvider`
/// impl, so the GitHub/Linear/etc. backend logic stays where it
/// belongs.
pub enum ProviderHandle {
    Github(GhClient),
    Linear(LinearClient),
}

impl ProviderHandle {
    pub async fn merge(
        &self,
        ws: &pilot_core::Workspace,
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::merge(c, ws).await,
            Self::Linear(c) => pilot_core::TaskProvider::merge(c, ws).await,
        }
    }
    pub async fn request_reviewers(
        &self,
        ws: &pilot_core::Workspace,
        logins: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => {
                pilot_core::TaskProvider::request_reviewers(c, ws, logins).await
            }
            Self::Linear(c) => {
                pilot_core::TaskProvider::request_reviewers(c, ws, logins).await
            }
        }
    }
    pub async fn add_assignees(
        &self,
        ws: &pilot_core::Workspace,
        logins: &[String],
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => {
                pilot_core::TaskProvider::add_assignees(c, ws, logins).await
            }
            Self::Linear(c) => {
                pilot_core::TaskProvider::add_assignees(c, ws, logins).await
            }
        }
    }
    pub async fn post_reply(
        &self,
        ws: &pilot_core::Workspace,
        body: &str,
    ) -> Result<(), pilot_core::ProviderError> {
        match self {
            Self::Github(c) => pilot_core::TaskProvider::post_reply(c, ws, body).await,
            Self::Linear(c) => pilot_core::TaskProvider::post_reply(c, ws, body).await,
        }
    }
}

/// Build a provider handle for the workspace that owns this
/// mutation. Routes on the workspace key's `<source>-<rest>`
/// prefix — `"github-tensorzero-nanogateway-186"` → github,
/// `"linear-team-xyz"` → linear, `"sandbox"` → no upstream
/// (returns `Err`).
///
/// Each branch builds its own credential chain — github goes
/// through `gh auth token` + `GH_TOKEN` envs, linear goes through
/// `LINEAR_API_KEY`. Future providers add their own chain in a
/// new branch.
///
/// Errors come back as `String` ready for the handler's
/// `emit_err` callback.
async fn build_provider_for_workspace(
    workspace_key: &WorkspaceKey,
) -> Result<ProviderHandle, String> {
    let source = workspace_key
        .as_str()
        .split_once('-')
        .map(|(p, _)| p)
        .unwrap_or("");
    match source {
        s if s == pilot_gh::SOURCE => {
            let cred = pilot_gh::credential_chain()
                .resolve(pilot_gh::SOURCE)
                .await
                .map_err(|e| format!("github credentials: {e}"))?;
            let client = GhClient::from_credential(cred)
                .await
                .map_err(|e| format!("github client init: {e}"))?;
            Ok(ProviderHandle::Github(client))
        }
        s if s == pilot_linear::SOURCE => {
            let cred = pilot_linear::credential_chain()
                .resolve(pilot_linear::SOURCE)
                .await
                .map_err(|e| format!("linear credentials: {e}"))?;
            Ok(ProviderHandle::Linear(LinearClient::from_credential(cred)))
        }
        other => Err(format!(
            "no provider registered for workspace prefix `{other}`",
        )),
    }
}

/// Handle `Command::MergePr`: load the workspace, recover the PR's
/// GraphQL node id from its primary task, and ship a `mergePullRequest`
/// mutation. On success the next poll cycle picks up the new MERGED
/// state and the workspace lands in the Inactive mailbox (or folds
/// into nothing if `closingIssuesReferences` had set up a collapse).
///
/// Errors surface as `Event::ProviderError` so the TUI can flash the
/// reason without us inventing a bespoke event variant.
pub async fn handle_merge_pr(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let emit_err = |msg: &str| {
        let _ = config.bus.send(Event::ProviderError {
            source: "merge".into(),
            message: msg.to_string(),
            detail: String::new(),
            kind: "retryable".into(),
        });
    };

    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!("merge: workspace {workspace_key} not found"));
        return;
    };
    let pr_label = ws.pr.as_ref().map(|p| p.id.key.clone());

    // Route through the workspace's matching provider. The handle
    // dispatches `merge` to github / linear / future-provider based
    // on the workspace key's prefix — the server stays provider-
    // agnostic.
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = provider.merge(&ws).await {
        tracing::warn!("merge {workspace_key}: {e:?}");
        emit_err(&format!("merge failed: {e}"));
        return;
    }
    tracing::info!("merged PR for workspace {workspace_key}");

    // Local Task still reads `Open` — the GitHub mutation succeeded
    // but our stored copy won't reflect MERGED until the next poll.
    // Broadcast `PrMerged` so the TUI can flash a footer notice and
    // the user doesn't think the keypress did nothing.
    if let Some(pr_label) = pr_label {
        let _ = config.bus.send(Event::PrMerged {
            workspace_key: workspace_key.clone(),
            pr_label,
        });
    }
}

/// Handle `Command::RequestReviewers`: add the given GitHub logins
/// as requested reviewers on the workspace's PR via GraphQL.
/// `union: true` on the mutation so existing reviewers aren't
/// dropped. Idempotent at GitHub's end — re-requesting an already
/// requested reviewer is a no-op.
///
/// On success, kicks a `Refresh` so the inbox reflects the new
/// reviewer set without waiting for the next 60s poll. Errors
/// surface as a `ProviderError` so the TUI footer flags it.
pub async fn handle_request_reviewers(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    logins: Vec<String>,
) {
    let emit_err = |msg: &str| {
        let _ = config.bus.send(Event::ProviderError {
            source: "reviewers".into(),
            message: msg.to_string(),
            detail: String::new(),
            kind: "retryable".into(),
        });
    };
    if logins.is_empty() {
        return;
    }
    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!(
            "request_reviewers: workspace {workspace_key} not found"
        ));
        return;
    };
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = provider.request_reviewers(&ws, &logins).await {
        tracing::warn!("request_reviewers {workspace_key} {logins:?}: {e:?}");
        emit_err(&format!("request reviewers failed: {e}"));
    } else {
        tracing::info!("requested reviewers {logins:?} on workspace {workspace_key}");
    }
}

/// Handle `Command::AddAssignees`: add the given logins as
/// assignees on the workspace's PR or issue (both implement
/// GraphQL's `Assignable` interface). Symmetric with
/// `handle_request_reviewers` — same credential chain, same
/// error-surface pattern.
pub async fn handle_add_assignees(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    logins: Vec<String>,
) {
    let emit_err = |msg: &str| {
        let _ = config.bus.send(Event::ProviderError {
            source: "assignees".into(),
            message: msg.to_string(),
            detail: String::new(),
            kind: "retryable".into(),
        });
    };
    if logins.is_empty() {
        return;
    }
    let Some(ws) = load_workspace(config, &workspace_key) else {
        emit_err(&format!(
            "add_assignees: workspace {workspace_key} not found"
        ));
        return;
    };
    let provider = match build_provider_for_workspace(&workspace_key).await {
        Ok(p) => p,
        Err(e) => {
            emit_err(&e);
            return;
        }
    };
    if let Err(e) = provider.add_assignees(&ws, &logins).await {
        tracing::warn!("add_assignees {workspace_key} {logins:?}: {e:?}");
        emit_err(&format!("add assignees failed: {e}"));
    } else {
        tracing::info!("added assignees {logins:?} on workspace {workspace_key}");
    }
}

/// Handle `Command::FetchPrDetails`: pull the workspace's PR
/// review-thread activity from GitHub (the field the inbox-scan
/// query deliberately omits), merge it into the workspace's
/// activity list, and broadcast `WorkspaceUpserted`.
///
/// Idempotent: re-fetching produces the same activities. The merge
/// step dedupes by `node_id`, so calling this twice (e.g. user
/// re-opens the same PR) doesn't duplicate rows. No-op when the
/// workspace has no PR — issue-only workspaces don't have review
/// threads.
///
/// Errors are silent at the user-facing level (no error toast):
/// the inbox row already shows what we have; an upgrade-only
/// failure shouldn't pop a modal. The diagnostic still lands in
/// `/tmp/pilot.log`.
pub async fn handle_fetch_pr_details(config: &ServerConfig, workspace_key: WorkspaceKey) {
    let Some(mut ws) = load_workspace(config, &workspace_key) else {
        tracing::info!("fetch_pr_details: workspace {workspace_key} not found");
        return;
    };
    let Some(pr) = ws.pr.as_ref() else {
        tracing::debug!("fetch_pr_details: workspace {workspace_key} has no PR");
        return;
    };
    let Some(node_id) = pr.node_id.clone() else {
        tracing::debug!("fetch_pr_details: PR has no node_id (needs a poll first)");
        return;
    };

    // Use the persistent client from TickState so the rate budget
    // and observations carry across calls — same logic as the
    // long-lived poll loop. Without this we'd build a fresh client
    // for every user-triggered fetch.
    let client = {
        let mut state = config.poll_state.lock().await;
        match state.gh_client.clone() {
            Some(c) => c,
            None => {
                let cred = match pilot_gh::credential_chain().resolve(pilot_gh::SOURCE).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("fetch_pr_details credentials: {e}");
                        return;
                    }
                };
                match GhClient::from_credential(cred).await {
                    Ok(c) => {
                        state.gh_client = Some(c.clone());
                        c
                    }
                    Err(e) => {
                        tracing::warn!("fetch_pr_details client init: {e}");
                        return;
                    }
                }
            }
        }
    };

    let activities = match client.fetch_pr_details(&node_id).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("fetch_pr_details({node_id}): {e}");
            return;
        }
    };
    if activities.is_empty() {
        tracing::debug!("fetch_pr_details({node_id}): no review-thread activity");
        return;
    }

    // Route through `Workspace::merge_activity` — the same path the
    // poll cycle uses. Crucial: it dedups by (author, body,
    // created_at) AND remaps `read_indices` across the post-sort
    // positions. The prior implementation here did a raw push +
    // sort, which left `read_indices` pointing at stale slots —
    // every lazy-fetch silently scrambled the user's read marks.
    let merged_count = activities.len();
    ws.merge_activity(&activities);
    tracing::info!(
        "fetch_pr_details: merged {} review-thread activities into {workspace_key}",
        merged_count,
    );

    // Persist + broadcast through the same commit phase the poll
    // path uses — keeps the store + bus consistent.
    commit_upsert(config, &workspace_key, ws);
}

/// Admin: walk every persisted workspace, drop sessions whose
/// terminals aren't currently live, and tear down the matching
/// worktree on disk. Inbox rows stay because we only touch
/// `workspace.sessions` (and the on-disk dir) — `workspace.pr` /
/// `gh_issues` aren't modified, so the sidebar keeps the row.
///
/// Live sessions (the user has an attached claude / shell) are
/// silently skipped so a long-running agent isn't pulled out from
/// under itself. Counts are emitted on `Event::CleanWorktreesCompleted`
/// so the TUI can surface "cleaned N · kept M (active)".
pub async fn handle_clean_worktrees(config: &ServerConfig) {
    let records = match config.store.list_workspaces() {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("clean_worktrees: list_workspaces failed: {e}");
            return;
        }
    };

    // Snapshot live session ids — anything in `terminal_sessions`
    // (the per-terminal owning-session map) is a session we must
    // not touch. Lock dropped before any async fs work.
    let live_sessions: std::collections::HashSet<pilot_core::SessionId> = {
        let map = config.terminal_sessions.lock().await;
        map.values().copied().collect()
    };

    let mgr = pilot_git_ops::WorktreeManager::default_base();
    let mut removed: usize = 0;
    let mut skipped: usize = 0;

    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(mut workspace) = serde_json::from_str::<pilot_core::Workspace>(&json) else {
            continue;
        };
        let workspace_key = workspace.key.clone();

        // Resolve the bare-repo path once per workspace (every
        // session shares the same upstream).
        let bare_path = workspace
            .primary_task()
            .and_then(|t| t.repo.as_deref())
            .and_then(|repo| repo.split_once('/'))
            .map(|(owner, name)| mgr.bare_path(owner, name));

        // Walk highest-index-first so removals don't shift indices
        // out from under the loop.
        let session_count = workspace.sessions.len();
        let mut wrote = false;
        for idx in (0..session_count).rev() {
            let session = workspace.sessions[idx].clone();
            if live_sessions.contains(&session.id) {
                skipped += 1;
                continue;
            }
            tracing::info!(
                workspace = %workspace_key,
                session = %session.id,
                worktree = %session.worktree_path.display(),
                "clean_worktrees: removing",
            );
            if let Some(bare) = bare_path.as_ref() {
                let _ = mgr.remove_by_path(bare, &session.worktree_path).await;
            } else {
                // No upstream repo metadata (rare — pre-PR / scratch
                // workspaces) → just `rm -rf` the dir, no git
                // bookkeeping to update.
                let _ = tokio::fs::remove_dir_all(&session.worktree_path).await;
            }
            workspace.sessions.remove(idx);
            removed += 1;
            wrote = true;
        }
        if wrote {
            commit_upsert(config, &workspace_key, workspace);
        }
    }

    tracing::info!(
        removed,
        skipped,
        "clean_worktrees: done",
    );
    let _ = config
        .bus
        .send(Event::CleanWorktreesCompleted { removed, skipped });
}
