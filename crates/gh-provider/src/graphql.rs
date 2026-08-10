//! GraphQL-based GitHub data fetching. One query gets everything.

use chrono::{DateTime, Utc};
use lazybox_core::*;
use serde::Deserialize;

/// The inbox-scan GraphQL query. Pulls **only** what the sidebar
/// renders — every other field has been pushed to `PR_DETAILS_QUERY`
/// (lazy-fetch on PR open + post-poll prefetch for high-attention rows).
///
/// ## Connection sizes (`first: N`) are the rate-limit budget
///
/// GitHub's GraphQL cost is dominated by `nodes × Σ first` across
/// every connection. The original query pulled ~85 sub-objects per
/// PR (`labels(10) + assignees(10) + reviewRequests(10) +
/// comments(15) + reviews(10) + closes(10) + checks(20) = 85`),
/// which at 100 PRs per page and 3-5 parallel branches
/// (`involves:USER`, `review-requested:USER`, per-watched-repo)
/// burned through GitHub's 5000-cost-units-per-resource hourly
/// budget in 1-2 polls.
///
/// Symptoms of that: slow polls (each page took seconds to
/// deserialize), per-poll cost overrun (subsequent ticks hit
/// secondary rate limits and timed out), and the dreaded
/// "no PRs in the inbox" loop where octocrab's 25s timeout fired
/// before any page completed.
///
/// New shape pulls **~25 sub-objects per PR** (`labels(10) +
/// assignees(5) + reviewRequests(5) + comments(last:1) + commits(1) +
/// closingIssuesReferences(10)`), still roughly 1/6 the cost of the
/// old eager query. The fields the sidebar doesn't render directly —
/// review history, per-check details, full comment threads — live in
/// `PR_DETAILS_QUERY` and only fire when the user opens a PR (or via
/// the post-tick `prefetch_top_pr_details` for high-attention rows).
/// The one heavy field kept eager is `closingIssuesReferences`: it's a
/// shallow connection (just issue number and repo name) and it's the
/// *authoritative* PR↔issue link, so fetching it every poll is what
/// makes issue→PR session transfer fire reliably instead of only when
/// a heuristic or lazy-fetch happens to run first (#559).
///
/// Trade-offs the lazy split accepts:
///   - `closes_issues` reflects `closingIssuesReferences` on every
///     poll (plus title heuristics). `Workspace::attach_task`
///     preserves it across subsequent polls so re-polls don't clear it.
///   - `checks` (per-check names) is empty until lazy-fetch — only
///     the aggregate `pr.ci` (from `statusCheckRollup.state`) is
///     available at inbox-scan time.
///   - `role` (see `derive_role_inner`) comes from concrete signals:
///     author, a review I submitted, an outstanding review request, or
///     an assignment — never from mere involvement, so the `reviewer`
///     filter can't sweep in every commented-on PR. Detecting "I
///     approved, so I'm done" needs the full reviews list; the
///     inbox-scan path leans on the review-request / assignee signals
///     instead and the lazy-fetch path refines it on workspace open.
///   - `needs_reply` is approximate (last comment from someone
///     else) until lazy-fetch back-fills reviews + review threads.
///
/// If the cost ever needs to go up again, raise the numbers AND
/// the `pr_query_omits_*` regression assertions together so the
/// budget can't drift silently.
const SEARCH_QUERY: &str = r#"
query($query: String!, $first: Int!, $after: String) {
  search(query: $query, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        id
        number
        title
        body
        url
        updatedAt
        createdAt
        closedAt
        isDraft
        state
        merged
        additions
        deletions
        headRefName
        baseRefName
        mergeable
        mergeStateStatus
        reviewDecision
        autoMergeRequest { enabledAt }
        isInMergeQueue
        author { login }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                state
              }
            }
          }
        }
        labels(first: 10) { nodes { name color } }
        assignees(first: 5) { nodes { login } }
        reviewRequests(first: 5) {
          nodes {
            requestedReviewer {
              ... on User { login }
              ... on Team { name }
            }
          }
        }
        comments(last: 1) {
          totalCount
          nodes {
            author { login }
            createdAt
          }
        }
        # Authoritative PR->issue link, kept eager for #559. Adds ~10
        # node-cost units per PR to a query whose page size is already
        # capped at PR_PAGE_SIZE (25) to stay under GitHub's gateway
        # timeout. This connection is shallow (number + repo name) so
        # the added cost is small; if gateway timeouts ever resurface
        # for users with very many involved PRs, lower PR_PAGE_SIZE
        # before dropping this field.
        closingIssuesReferences(first: 10) {
          nodes {
            number
            repository { nameWithOwner }
          }
        }
      }
    }
  }
  rateLimit {
    cost
    limit
    remaining
    resetAt
    used
  }
}
"#;

// ─── Response types ────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct GqlResponse {
    pub data: Option<GqlData>,
    pub errors: Option<Vec<GqlError>>,
}

/// Response envelope for GraphQL **mutations** (merge, reviewers,
/// assignees, labels, close, reaction). Unlike the search-shaped
/// [`GqlResponse`] — whose [`GqlData`] *requires* a `search` field —
/// a mutation's `data` node is `mergePullRequest` / `requestReviews` /
/// … and has no `search` key, so deserializing it as `GqlResponse`
/// fails on every success and reports a false error (issue #305).
///
/// Success for a mutation is HTTP 2xx with an empty `errors`; the
/// returned node payload is confirmation, not something to parse. So
/// `data` is left untyped and unread — we only inspect `errors`.
#[derive(Deserialize, Debug)]
pub struct GqlMutationResponse {
    pub errors: Option<Vec<GqlError>>,
}

const RATE_BUDGET_QUERY: &str = r#"
query {
  viewer { login }
  rateLimit { cost limit remaining resetAt used }
}
"#;

pub fn rate_budget_body() -> serde_json::Value {
    serde_json::json!({ "query": RATE_BUDGET_QUERY })
}

#[derive(Deserialize, Debug)]
pub struct GqlRateBudgetResponse {
    pub data: Option<GqlRateBudgetData>,
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct GqlRateBudgetData {
    pub viewer: GqlRateBudgetViewer,
    #[serde(rename = "rateLimit")]
    pub rate_limit: GqlRateLimit,
}

#[derive(Deserialize, Debug)]
pub struct GqlRateBudgetViewer {
    pub login: String,
}

#[derive(Deserialize, Debug)]
pub struct GqlError {
    pub message: String,
    /// GraphQL error classification. GitHub sets this at the top level
    /// of each error object (`NOT_FOUND`, `FORBIDDEN`, `UNPROCESSABLE`,
    /// …). Optional — GHES and some paths omit it, so callers that need
    /// certainty fall back to the message text.
    #[serde(default, rename = "type")]
    pub error_type: Option<String>,
    /// GraphQL path to the node that failed (if any).
    #[serde(default)]
    pub path: Option<Vec<serde_json::Value>>,
    /// Error type / extensions (often contains the attribute name).
    #[serde(default)]
    pub extensions: Option<serde_json::Value>,
    /// Source locations (line/col in the query).
    #[serde(default)]
    #[allow(dead_code)] // Captured for debug format — not yet used in messages.
    pub locations: Option<Vec<serde_json::Value>>,
}

impl GqlError {
    /// True when this error means the queried node is *definitively* not
    /// visible to us — deleted, transferred out, made private, or the
    /// token's scope no longer covers the repo. GitHub signals these as
    /// a top-level `NOT_FOUND` / `FORBIDDEN` error `type`; the
    /// message-text fallback (`"could not resolve to"`) catches
    /// responses that omit the `type` field.
    ///
    /// Distinguishing this from a transient error matters for the
    /// notifications cursor (#512): a definitively-gone entry must be
    /// treated as *handled* (`Ok(None)`) so the cursor can advance,
    /// while a transient failure holds the cursor for a retry.
    pub fn is_not_visible(&self) -> bool {
        if let Some(t) = &self.error_type
            && (t.eq_ignore_ascii_case("NOT_FOUND") || t.eq_ignore_ascii_case("FORBIDDEN"))
        {
            return true;
        }
        self.message
            .to_ascii_lowercase()
            .contains("could not resolve to")
    }

    /// True when GitHub is rate-limiting this mutation. GitHub signals it
    /// inside the GraphQL `errors` array (HTTP 200) either with a
    /// top-level `type: "RATE_LIMITED"` or a message naming the
    /// secondary / abuse limit. Distinct from [`is_not_visible`] so the
    /// mutation path can queue + retry against the reset window instead of
    /// hard-failing with the raw error.
    pub fn is_rate_limited(&self) -> bool {
        if let Some(t) = &self.error_type
            && t.eq_ignore_ascii_case("RATE_LIMITED")
        {
            return true;
        }
        let m = self.message.to_ascii_lowercase();
        m.contains("secondary rate limit")
            || m.contains("rate limit exceeded")
            || m.contains("abuse detection")
    }

    /// The human-readable message alone — no `path` / `extensions` debug
    /// text. `full()` is for the log file; `human()` is what reaches the
    /// footer, so a rate-limit error never dumps its serialized
    /// `{…,"typeName":"Mutation"}` extensions blob at the user.
    pub fn human(&self) -> String {
        self.message.trim().to_string()
    }

    /// Human-readable debug line including path + extensions, not just the message.
    pub fn full(&self) -> String {
        let mut s = self.message.clone();
        if let Some(path) = &self.path
            && !path.is_empty()
        {
            let path_str = path
                .iter()
                .filter_map(|v| {
                    v.as_str()
                        .map(String::from)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .collect::<Vec<_>>()
                .join(".");
            s.push_str(&format!(" [at {path_str}]"));
        }
        if let Some(ext) = &self.extensions {
            s.push_str(&format!(" (ext: {ext})"));
        }
        s
    }
}

#[derive(Deserialize, Debug)]
pub struct GqlData {
    pub search: GqlSearch,
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<GqlRateLimit>,
}

#[derive(Deserialize, Debug)]
pub struct GqlRateLimit {
    pub remaining: u32,
    /// Total quota. Optional in case GitHub stops returning it.
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(rename = "resetAt")]
    pub reset_at: String,
    /// GraphQL point cost of the query that returned this block.
    /// Optional: only the poll-path queries (`SEARCH_QUERY`,
    /// `PR_DETAILS_QUERY`) select `cost`; mutations and other reads
    /// don't, and GitHub omits it unless asked.
    #[serde(default)]
    pub cost: Option<u32>,
    /// Total GraphQL points consumed in the current primary window.
    /// Comparing this with the prior useful response exposes spending
    /// by `gh` and spawned agents that share the user's pool.
    #[serde(default)]
    pub used: u32,
}

fn default_limit() -> u32 {
    5000
}

#[derive(Deserialize, Debug)]
pub struct GqlSearch {
    pub nodes: Vec<GqlPr>,
    #[serde(rename = "issueCount", default)]
    pub issue_count: u32,
    #[serde(rename = "pageInfo", default)]
    pub page_info: Option<GqlPageInfo>,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlPageInfo {
    #[serde(rename = "hasNextPage", default)]
    pub has_next_page: bool,
    #[serde(rename = "endCursor", default)]
    pub end_cursor: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GqlPr {
    /// GraphQL node ID — needed for mutations like `updatePullRequestBranch`.
    #[serde(default)]
    pub id: Option<String>,
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
    /// When the PR was opened. Stable across later activity. `default`
    /// so queries that don't select `createdAt` (lazy detail fetches)
    /// still deserialize — the age falls back to `updatedAt`.
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<DateTime<Utc>>,
    /// When the PR was closed (merge also closes it). Stable — unlike
    /// `updatedAt`, GitHub does NOT bump this on post-merge activity.
    #[serde(rename = "closedAt", default)]
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    pub state: String, // OPEN, CLOSED, MERGED
    pub merged: bool,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    /// Head commit OID. Selected only by `SINGLE_PR_QUERY` (the
    /// targeted merge-time re-fetch) — the inbox-scan query omits it
    /// to keep its per-PR cost budget untouched, and `Task` doesn't
    /// carry it. The auto-merge path reads it straight off this node
    /// and threads it into `mergePullRequest`'s `expectedHeadOid`.
    #[serde(default, rename = "headRefOid")]
    pub head_ref_oid: Option<String>,
    #[serde(rename = "baseRefName", default)]
    pub base_ref_name: String,
    /// MERGEABLE, CONFLICTING, or UNKNOWN.
    #[serde(default)]
    pub mergeable: Option<String>,
    /// BEHIND, BLOCKED, CLEAN, DIRTY, DRAFT, HAS_HOOKS, UNKNOWN, UNSTABLE.
    /// `BEHIND` is what drives the "Update branch" button on github.com.
    #[serde(default, rename = "mergeStateStatus")]
    pub merge_state_status: Option<String>,
    #[serde(rename = "reviewDecision")]
    pub review_decision: Option<String>, // APPROVED, CHANGES_REQUESTED, REVIEW_REQUIRED
    #[serde(rename = "autoMergeRequest")]
    pub auto_merge_request: Option<GqlAutoMerge>,
    #[serde(default, rename = "isInMergeQueue")]
    pub is_in_merge_queue: bool,
    pub author: Option<GqlAuthor>,
    pub labels: GqlLabels,
    pub assignees: GqlAssignees,
    #[serde(rename = "reviewRequests")]
    pub review_requests: GqlReviewRequests,
    pub comments: GqlComments,
    /// Reviews list — populated by the lazy `PR_DETAILS_QUERY`, NOT
    /// by the inbox-scan `SEARCH_QUERY`. The inbox query dropped this
    /// field (every poll was pulling 10 reviews × every PR — the
    /// fattest single contributor to per-PR cost after reviewThreads).
    /// `#[serde(default)]` so the inbox response deserializes with an
    /// empty list; `pr_details_to_details` fills it in on workspace
    /// open or via the post-tick prefetch.
    #[serde(default)]
    pub reviews: GqlReviews,
    /// Latest APPROVED / CHANGES_REQUESTED per reviewer — GitHub's own
    /// per-author collapse, so it never misses a verdict older than a
    /// `reviews(first: N)` slice would reach. Drives the Reviewers
    /// line's ✓/✗ state. Lazy-only (absent from the inbox scan).
    #[serde(default, rename = "latestOpinionatedReviews")]
    pub latest_opinionated_reviews: GqlLatestReviews,
    /// Latest review per reviewer regardless of verdict — used only to
    /// surface comment-only reviewers on the Reviewers line. Lazy-only.
    #[serde(default, rename = "latestReviews")]
    pub latest_reviews: GqlLatestReviews,
    /// Inline review-thread comments. Lazy-fetched per PR via
    /// `Command::FetchPrDetails` (see `PR_DETAILS_QUERY`) instead of
    /// being pulled on every poll cycle.
    #[serde(default, rename = "reviewThreads")]
    pub review_threads: GqlReviewThreads,
    pub commits: GqlCommits,
    /// Issues this PR will close on merge — GitHub's authoritative
    /// PR↔issue link (it resolves the body's `Closes #N` keywords
    /// itself, including cross-repo). Fetched on the poll path by both
    /// `SEARCH_QUERY` and `PR_DETAILS_QUERY`, so `closes_issues` is
    /// populated on first sight rather than waiting for a lazy fetch
    /// (#559). `#[serde(default)]` for any older/partial response that
    /// omits it; `Workspace::attach_task` preserves the value across
    /// subsequent polls.
    #[serde(default, rename = "closingIssuesReferences")]
    pub closing_issues_references: Option<GqlClosingIssues>,
}

#[derive(Deserialize, Debug)]
pub struct GqlClosingIssues {
    pub nodes: Vec<GqlClosingIssue>,
}

#[derive(Deserialize, Debug)]
pub struct GqlClosingIssue {
    pub number: u64,
    pub repository: GqlIssueRepo,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlCommits {
    pub nodes: Vec<GqlCommitNode>,
}

#[derive(Deserialize, Debug)]
pub struct GqlCommitNode {
    pub commit: GqlCommit,
}

#[derive(Deserialize, Debug)]
pub struct GqlCommit {
    #[serde(rename = "statusCheckRollup")]
    pub status_check_rollup: Option<GqlStatusRollup>,
}

#[derive(Deserialize, Debug)]
pub struct GqlStatusRollup {
    /// SUCCESS, FAILURE, ERROR, PENDING, EXPECTED
    pub state: String,
    /// Per-check details. Only fetched by `PR_DETAILS_QUERY`; the
    /// inbox-scan query just reads the rolled-up `state`. Optional so
    /// the inbox response deserializes when `contexts` isn't present.
    #[serde(default)]
    pub contexts: Option<GqlCheckContexts>,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlCheckContexts {
    pub nodes: Vec<GqlCheckContext>,
}

/// Check context — polymorphic (CheckRun | StatusContext).
#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum GqlCheckContext {
    CheckRun {
        name: String,
        conclusion: Option<String>, // SUCCESS, FAILURE, NEUTRAL, CANCELLED, TIMED_OUT, ACTION_REQUIRED, SKIPPED
        #[allow(dead_code)]
        status: Option<String>, // QUEUED, IN_PROGRESS, COMPLETED, WAITING, PENDING, REQUESTED
        #[allow(dead_code)]
        permalink: Option<String>,
    },
    StatusContext {
        context: String,
        state: String, // EXPECTED, ERROR, FAILURE, PENDING, SUCCESS
        #[allow(dead_code)]
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
    },
}

#[derive(Deserialize, Debug)]
pub struct GqlAuthor {
    pub login: String,
}

#[derive(Deserialize, Debug)]
pub struct GqlLabels {
    pub nodes: Vec<GqlLabel>,
}

#[derive(Deserialize, Debug)]
pub struct GqlLabel {
    pub name: String,
    /// GitHub returns the label's hex color (e.g. `"d73a4a"`) without
    /// the leading `#`. Optional so an older response shape (or a
    /// query that doesn't ask for the field) deserializes cleanly.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GqlAutoMerge {
    #[allow(dead_code)]
    #[serde(rename = "enabledAt")]
    pub enabled_at: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlAssignees {
    #[serde(default)]
    pub nodes: Vec<GqlAuthor>,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlReviewRequests {
    #[serde(default)]
    pub nodes: Vec<GqlReviewRequest>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviewRequest {
    #[serde(rename = "requestedReviewer")]
    pub requested_reviewer: Option<GqlRequestedReviewer>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum GqlRequestedReviewer {
    User {
        login: String,
    },
    Team {
        name: String,
    },
    /// Any other requested-reviewer type — a Mannequin, a Bot (e.g.
    /// Copilot code review), or an App. The query only spreads
    /// `... on User` / `... on Team`, so these arrive as `{}` and match
    /// neither typed variant. Without this catch-all the untagged enum
    /// fails to deserialize and takes the whole search/details response
    /// down with it. `IgnoredAny` deserializes-and-discards any JSON, so
    /// it always matches as the last untagged arm; it carries no login,
    /// so it never matches a viewer.
    Other(#[allow(dead_code)] serde::de::IgnoredAny),
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlComments {
    pub nodes: Vec<GqlComment>,
    /// Server-side count of *all* comments on the PR/issue. Optional
    /// because not every query selects it (lazy `PR_DETAILS_QUERY`
    /// just pulls the bodies; only `SEARCH_QUERY` needs the count
    /// for the `unread_count` proxy on inbox-scan).
    #[serde(default, rename = "totalCount")]
    pub total_count: Option<u32>,
}

#[derive(Deserialize, Debug)]
pub struct GqlComment {
    #[serde(default)]
    pub id: Option<String>,
    pub author: Option<GqlAuthor>,
    /// Comment body. `#[serde(default)]` (empty string when absent)
    /// because the inbox-scan `SEARCH_QUERY` deliberately omits it:
    /// `comments(last: 1)` there selects only `author` + `createdAt`
    /// to drive the sidebar's `last_commenter` line and the
    /// `unread_count` proxy — the bodies live on the lazy
    /// `PR_DETAILS_QUERY`. Without the default, a PR carrying even one
    /// comment fails to deserialize `GqlComment`, which fails the
    /// whole `Vec<GqlPr>` page and silently drops *every* PR from the
    /// inbox (regression from `3c019af`; issues were unaffected
    /// because `ISSUES_QUERY` still selects the body). The eager
    /// queries (`ISSUES_QUERY`, `PR_DETAILS_QUERY`, `SINGLE_PR_QUERY`)
    /// all still select `body`, so this default only kicks in on the
    /// inbox-scan path where the body genuinely isn't fetched.
    #[serde(default)]
    pub body: String,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default, rename = "originalLine")]
    pub original_line: Option<u32>,
    #[serde(default, rename = "diffHunk")]
    pub diff_hunk: Option<String>,
    /// Eyes-reaction state for the authenticated viewer. Populated
    /// by the issues-search query (which selects `reactions(content:
    /// EYES) { viewerHasReacted }`); other queries leave it `None`.
    /// Drives `@lazybox` mention idempotency: lazybox reacts 👀 on first
    /// sight, then skips comments where this is `Some(true)`.
    #[serde(default)]
    pub reactions: Option<GqlReactionView>,
}

/// Tiny projection of a `ReactionConnection` filtered to one content
/// kind (currently `EYES`). True when the authenticated viewer has
/// reacted with that content. Used by the `@lazybox`-mention path to
/// decide whether lazybox has already acknowledged a comment.
#[derive(Deserialize, Debug, Clone, Copy, Default)]
pub struct GqlReactionView {
    #[serde(default, rename = "viewerHasReacted")]
    pub viewer_has_reacted: bool,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlReviews {
    pub nodes: Vec<GqlReview>,
}

/// GitHub's per-author-collapsed review connections
/// (`latestReviews` / `latestOpinionatedReviews`): exactly one node per
/// reviewer, so — unlike a `first: N` slice of the full `reviews`
/// history — they cannot drop a reviewer's latest verdict behind the
/// cap. Only login + state are selected; the Reviewers line needs no
/// body or timestamp.
#[derive(Deserialize, Debug, Default)]
pub struct GqlLatestReviews {
    #[serde(default)]
    pub nodes: Vec<GqlReviewerVerdict>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviewerVerdict {
    pub author: Option<GqlAuthor>,
    pub state: String, // APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED, PENDING
}

#[derive(Deserialize, Debug)]
pub struct GqlReview {
    /// GraphQL node id of the review. Carried onto the built
    /// `Activity::node_id` so a review's identity is stable across
    /// polls — without it, a PENDING review (null `submittedAt`) fell
    /// back to content-tuple identity with a fresh timestamp every
    /// poll and duplicated as a new unread row forever.
    #[serde(default)]
    pub id: Option<String>,
    pub author: Option<GqlAuthor>,
    pub body: Option<String>,
    pub state: String, // APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED, PENDING
    #[serde(rename = "submittedAt")]
    pub submitted_at: Option<DateTime<Utc>>,
    /// When the review was created. Non-null in GitHub's schema (unlike
    /// `submittedAt`, which is null while PENDING) — the stable
    /// timestamp fallback that replaces the old `Utc::now()` one.
    #[serde(default, rename = "createdAt")]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlReviewThreads {
    #[serde(default)]
    pub nodes: Vec<GqlReviewThread>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviewThread {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "isResolved")]
    pub is_resolved: bool,
    #[serde(rename = "isOutdated")]
    pub is_outdated: bool,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default, rename = "originalLine")]
    pub original_line: Option<u32>,
    pub comments: GqlComments,
}

// ─── Conversion ────────────────────────────────────────────────────────────

/// Build the GraphQL search string from a fully-formed qualifier
/// list. Caller owns the full string composition; this function is
/// just a join. Defaults (`is:open is:pr archived:false`) live in
/// `default_search_qualifiers` so callers can choose to opt out
/// (e.g. fetching closed PRs for the Inactive mailbox).
pub fn build_query(qualifiers: &[String]) -> String {
    qualifiers.join(" ")
}

/// The base qualifiers every PR search needs: open, PR-only,
/// non-archived. Callers extend this with role + scope qualifiers
/// before handing to [`build_query`]. Kept as a function so a
/// future "include closed PRs" toggle has one place to live.
pub fn default_search_qualifiers() -> Vec<String> {
    vec![
        "is:open".to_string(),
        "is:pr".to_string(),
        "archived:false".to_string(),
    ]
}

/// `updated:>=<since>` qualifier narrowing a search to items touched
/// at or after `since`. Lets a steady-state `involves:` PR sweep skip
/// the unchanged majority (issue #14). GitHub's search syntax takes an
/// RFC3339 timestamp with an explicit offset; we always emit UTC
/// (`+00:00`). The bound is inclusive, so re-using the previous sweep's
/// start time re-fetches a handful of boundary PRs rather than risking
/// a gap.
pub fn updated_since_qualifier(since: DateTime<Utc>) -> String {
    format!("updated:>={}", since.format("%Y-%m-%dT%H:%M:%S+00:00"))
}

/// Search string for the recently-merged back-fill sweep: PRs merged
/// on or after `merged_floor` (a `YYYY-MM-DD` date, the trailing 7-day
/// window) scoped to the user's roles, so a PR that lands right after a
/// sync still picks up its final MERGED state on the next tick.
///
/// `since` adds an `updated:>=` window on top of the merge floor (issue
/// #530). On a windowed global sweep the merged set is dominated by PRs
/// merged days ago and unchanged since the last poll; without the
/// window every 10-min sweep re-downloads all of them for nothing. The
/// window is safe because a merge bumps `updatedAt`, so a newly-merged
/// PR is always ≥ the previous sweep floor. `None` (reconcile sweep, or
/// the windowless round-robin path) keeps the full 7-day set.
pub fn merged_sweep_query(
    user: &str,
    pr_filters: &[String],
    merged_floor: &str,
    since: Option<DateTime<Utc>>,
) -> String {
    let mut quals = vec![
        "is:pr".to_string(),
        "is:merged".to_string(),
        "archived:false".to_string(),
        format!("merged:>={merged_floor}"),
    ];
    if pr_filters.is_empty() {
        quals.push(format!("involves:{user}"));
    } else {
        quals.extend(pr_filters.iter().cloned());
    }
    if let Some(since) = since {
        quals.push(updated_since_qualifier(since));
    }
    build_query(&quals)
}

/// Per-page size for the PR search. Was 100 (GraphQL's maximum)
/// but with the heavy SEARCH_QUERY payload, GitHub's GraphQL
/// gateway timed out (HTTP 502 / 504 "We couldn't respond to your
/// request in time") on every attempt — user logs from
/// 2026-05-28 showed PR sync failing every cycle while the
/// lighter issues query (still on 100/page) succeeded fine.
///
/// 25 gives GitHub's gateway 4× the per-request compute budget to
/// resolve our connections, at the cost of 4× the page count for
/// users with 100+ involved PRs. Empirically GitHub's gateway
/// budget is closer to the per-request side than the per-result
/// side, so smaller pages win.
const PR_PAGE_SIZE: u32 = 25;

pub fn pr_page_count(issue_count: u32) -> u32 {
    issue_count.div_ceil(PR_PAGE_SIZE).max(1)
}

pub fn query_body_after(search_query: &str, after: Option<&str>) -> serde_json::Value {
    // Omit `after` entirely when None — sending `"after": null` in the
    // variables block trips GitHub's GraphQL with a misleading
    // "A query attribute must be specified and must be a string" error.
    // With the variable absent, the query's `$after: String` parameter
    // defaults to null on GitHub's side, which is what we want.
    let variables = match after {
        Some(cursor) => serde_json::json!({
            "query": search_query,
            "first": PR_PAGE_SIZE,
            "after": cursor,
        }),
        None => serde_json::json!({
            "query": search_query,
            "first": PR_PAGE_SIZE,
        }),
    };
    serde_json::json!({
        "query": SEARCH_QUERY,
        "variables": variables,
    })
}

/// GraphQL mutation that merges the base branch into the PR head — same
/// effect as clicking "Update branch" on github.com. Default method is MERGE;
/// pass REBASE if the repo prefers it.
const UPDATE_BRANCH_MUTATION: &str = r#"
mutation($id: ID!) {
  updatePullRequestBranch(input: { pullRequestId: $id }) {
    pullRequest { id }
  }
}
"#;

pub fn update_branch_body(pull_request_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": UPDATE_BRANCH_MUTATION,
        "variables": { "id": pull_request_node_id },
    })
}

/// GraphQL query for a PR's repository merge default — the same
/// method github.com's "Merge pull request" button pre-selects. A repo
/// that disables merge commits (squash/rebase only) reports SQUASH or
/// REBASE here; we feed it straight into the merge mutation.
const PR_MERGE_METHOD_QUERY: &str = r#"
query($id: ID!) {
  node(id: $id) {
    ... on PullRequest {
      repository { viewerDefaultMergeMethod }
    }
  }
  rateLimit { cost limit remaining resetAt used }
}
"#;

pub fn pr_merge_method_body(pull_request_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": PR_MERGE_METHOD_QUERY,
        "variables": { "id": pull_request_node_id },
    })
}

#[derive(Debug, Deserialize)]
pub struct GqlMergeMethodResponse {
    pub data: Option<GqlMergeMethodData>,
    #[serde(default)]
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Debug, Deserialize)]
pub struct GqlMergeMethodData {
    pub node: Option<GqlMergeMethodNode>,
}

#[derive(Debug, Deserialize)]
pub struct GqlMergeMethodNode {
    pub repository: GqlMergeMethodRepository,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GqlMergeMethodRepository {
    pub viewer_default_merge_method: String,
}

/// GraphQL mutation that merges the PR — same effect as clicking
/// "Merge pull request" on github.com. `mergeMethod` MUST be pinned:
/// GitHub defaults an omitted method to MERGE (a merge commit), which
/// fails outright on repos that disallow merge commits — it does not
/// fall back to the repo's allowed method. Callers pass the repo's
/// `viewerDefaultMergeMethod` (see `PR_MERGE_METHOD_QUERY`).
///
/// `expectedHeadOid` is GitHub's compare-and-swap guard: when non-null,
/// the merge is rejected ("Head branch was modified…") if the PR's head
/// no longer sits at that commit — closing the force-push window
/// between "lazybox observed the PR green at OID X" and "the merge
/// landed". The input field is nullable, so passing `null` preserves
/// the old unguarded behavior for callers with no known head.
const MERGE_PR_MUTATION: &str = r#"
mutation($id: ID!, $method: PullRequestMergeMethod!, $expectedHeadOid: GitObjectID) {
  mergePullRequest(input: { pullRequestId: $id, mergeMethod: $method, expectedHeadOid: $expectedHeadOid }) {
    pullRequest { id state merged }
  }
}
"#;

/// Resolve a GitHub login to its node ID. Needed because the
/// reviewer/assignee mutations below take node IDs, not bare
/// usernames — the rest of GitHub's GraphQL surface uses IDs
/// internally. One round-trip per login (cheap; user nodes are
/// permanent so caching could be added later if anyone touches
/// this hot path).
const USER_ID_QUERY: &str = r#"
query($login: String!) {
  user(login: $login) { id }
  rateLimit { cost limit remaining resetAt used }
}
"#;

pub fn user_id_body(login: &str) -> serde_json::Value {
    serde_json::json!({
        "query": USER_ID_QUERY,
        "variables": { "login": login },
    })
}

#[derive(Debug, Deserialize)]
pub struct GqlUserIdResponse {
    pub data: Option<GqlUserIdData>,
    #[serde(default)]
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Debug, Deserialize)]
pub struct GqlUserIdData {
    pub user: Option<GqlUserNode>,
}

#[derive(Debug, Deserialize)]
pub struct GqlUserNode {
    pub id: String,
}

/// GraphQL mutation to add reviewers to a PR. `union: true` so the
/// new reviewer set is the UNION of existing + provided (no
/// accidental overwrites of someone else's previously-requested
/// reviewers).
const REQUEST_REVIEWS_MUTATION: &str = r#"
mutation($id: ID!, $userIds: [ID!]) {
  requestReviews(input: { pullRequestId: $id, userIds: $userIds, union: true }) {
    pullRequest { id }
  }
}
"#;

pub fn request_reviews_body(
    pull_request_node_id: &str,
    user_node_ids: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "query": REQUEST_REVIEWS_MUTATION,
        "variables": {
            "id": pull_request_node_id,
            "userIds": user_node_ids,
        },
    })
}

/// GraphQL mutation to add assignees to any Assignable (PR or
/// Issue — both implement the interface).
const ADD_ASSIGNEES_MUTATION: &str = r#"
mutation($id: ID!, $userIds: [ID!]!) {
  addAssigneesToAssignable(input: { assignableId: $id, assigneeIds: $userIds }) {
    assignable { __typename }
  }
}
"#;

pub fn add_assignees_body(assignable_node_id: &str, user_node_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "query": ADD_ASSIGNEES_MUTATION,
        "variables": {
            "id": assignable_node_id,
            "userIds": user_node_ids,
        },
    })
}

/// Sibling mutation for removing assignees. Same Assignable
/// interface; the `SetAssignees` command path fires this with the
/// diff `existing − desired` after firing the add mutation with
/// `desired − existing`.
const REMOVE_ASSIGNEES_MUTATION: &str = r#"
mutation($id: ID!, $userIds: [ID!]!) {
  removeAssigneesFromAssignable(input: { assignableId: $id, assigneeIds: $userIds }) {
    assignable { __typename }
  }
}
"#;

pub fn remove_assignees_body(
    assignable_node_id: &str,
    user_node_ids: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "query": REMOVE_ASSIGNEES_MUTATION,
        "variables": {
            "id": assignable_node_id,
            "userIds": user_node_ids,
        },
    })
}

pub fn merge_pr_body(
    pull_request_node_id: &str,
    merge_method: &str,
    expected_head_oid: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "query": MERGE_PR_MUTATION,
        "variables": {
            "id": pull_request_node_id,
            "method": merge_method,
            // `null` when the caller has no known head — the input
            // field is nullable and GitHub then skips the guard.
            "expectedHeadOid": expected_head_oid,
        },
    })
}

/// GraphQL mutation that closes an issue as `NOT_PLANNED` — the
/// "won't do" close, distinct from the `COMPLETED` state a resolving
/// PR produces. GitHub offers no non-admin issue *delete* over the
/// API, so lazybox's "delete issue" resolves to this. Closing an
/// already-closed issue is a no-op on GitHub's side, so retrying is
/// safe.
const CLOSE_ISSUE_MUTATION: &str = r#"
mutation($id: ID!) {
  closeIssue(input: { issueId: $id, stateReason: NOT_PLANNED }) {
    issue { id state stateReason }
  }
}
"#;

pub fn close_issue_body(issue_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": CLOSE_ISSUE_MUTATION,
        "variables": { "id": issue_node_id },
    })
}

/// GraphQL mutation that closes a PR without merging it. Closing an
/// already-closed PR is a no-op on GitHub's side, so retrying is safe.
const CLOSE_PR_MUTATION: &str = r#"
mutation($id: ID!) {
  closePullRequest(input: { pullRequestId: $id }) {
    pullRequest { id state }
  }
}
"#;

pub fn close_pr_body(pull_request_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": CLOSE_PR_MUTATION,
        "variables": { "id": pull_request_node_id },
    })
}

/// GraphQL mutation that hard-deletes an issue. GitHub only allows
/// this for repo admins — everyone else gets a FORBIDDEN error, which
/// the caller degrades to a `closeIssue` (NOT_PLANNED) instead.
const DELETE_ISSUE_MUTATION: &str = r#"
mutation($id: ID!) {
  deleteIssue(input: { issueId: $id }) {
    clientMutationId
  }
}
"#;

pub fn delete_issue_body(issue_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": DELETE_ISSUE_MUTATION,
        "variables": { "id": issue_node_id },
    })
}

/// GraphQL mutation: add a 👀 reaction to any `Reactable` (Issue body
/// or IssueComment, in lazybox's case). Posting this reaction is the
/// authoritative idempotency marker for the `@lazybox`-mention
/// auto-spawn path — subsequent polls see `viewerHasReacted: true`
/// and skip the trigger. Re-adding an existing reaction is a no-op
/// on GitHub's side, so retrying is safe.
const ADD_REACTION_MUTATION: &str = r#"
mutation($id: ID!) {
  addReaction(input: { subjectId: $id, content: EYES }) {
    reaction { content }
  }
}
"#;

pub fn add_reaction_eyes_body(reactable_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": ADD_REACTION_MUTATION,
        "variables": { "id": reactable_node_id },
    })
}

/// GraphQL mutations for label add/remove on any `Labelable` (PR or
/// Issue — both implement the interface). Mutations take GraphQL node
/// IDs, not names, so the daemon resolves names → IDs via
/// `list_repo_labels` (which returns both) before calling the mutation.
const ADD_LABELS_MUTATION: &str = r#"
mutation($id: ID!, $labelIds: [ID!]!) {
  addLabelsToLabelable(input: { labelableId: $id, labelIds: $labelIds }) {
    labelable { __typename }
  }
}
"#;

pub fn add_labels_body(labelable_node_id: &str, label_node_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "query": ADD_LABELS_MUTATION,
        "variables": {
            "id": labelable_node_id,
            "labelIds": label_node_ids,
        },
    })
}

const REMOVE_LABELS_MUTATION: &str = r#"
mutation($id: ID!, $labelIds: [ID!]!) {
  removeLabelsFromLabelable(input: { labelableId: $id, labelIds: $labelIds }) {
    labelable { __typename }
  }
}
"#;

pub fn remove_labels_body(labelable_node_id: &str, label_node_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "query": REMOVE_LABELS_MUTATION,
        "variables": {
            "id": labelable_node_id,
            "labelIds": label_node_ids,
        },
    })
}

/// Fetch the full label set on a repository. Returns up to 100
/// labels — enough for any real repo (typical projects sit well
/// under 50). Pagination would matter for monorepos with hundreds
/// of labels; we'll cross that bridge if it bites.
const REPO_LABELS_QUERY: &str = r#"
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    labels(first: 100) {
      nodes { id name color description }
    }
  }
  rateLimit {
    cost
    limit
    remaining
    resetAt
    used
  }
}
"#;

pub fn repo_labels_body(owner: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "query": REPO_LABELS_QUERY,
        "variables": { "owner": owner, "name": name },
    })
}

#[derive(Debug, Deserialize)]
pub struct GqlRepoLabelsResponse {
    pub data: Option<GqlRepoLabelsData>,
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Debug, Deserialize)]
pub struct GqlRepoLabelsData {
    pub repository: Option<GqlRepoLabels>,
}

#[derive(Debug, Deserialize)]
pub struct GqlRepoLabels {
    pub labels: GqlRepoLabelsConn,
}

#[derive(Debug, Deserialize)]
pub struct GqlRepoLabelsConn {
    pub nodes: Vec<GqlRepoLabelNode>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GqlRepoLabelNode {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Per-PR "give me everything heavy" query. Pulls every field the
/// inbox-scan `SEARCH_QUERY` dropped: full review-thread data, the
/// full reviews list, top-level comment bodies, status-check
/// contexts, and the closing-issue refs.
///
/// Fires:
///   - When the user opens a PR's workspace (`handle_fetch_pr_details`).
///   - Post-tick for the top-N high-attention PRs
///     (`prefetch_top_pr_details`).
///
/// Cost trade-off: roughly `1 + 50×(1 + 10) + 15 + 10 + 20 + 10`
/// ≈ 605 cost units per call. Only fires WHEN the user asks (or
/// for a small N per tick), so it doesn't compound across the
/// inbox the way the eager `SEARCH_QUERY` did.
const PR_DETAILS_QUERY: &str = r#"
query($id: ID!) {
  node(id: $id) {
    ... on PullRequest {
      id
      updatedAt
      reviewDecision
      author { login }
      assignees(first: 20) { nodes { login } }
      reviewRequests(first: 20) {
        nodes {
          requestedReviewer {
            ... on User { login }
            ... on Team { name }
          }
        }
      }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              state
              contexts(first: 20) {
                nodes {
                  __typename
                  ... on CheckRun {
                    name
                    conclusion
                    status
                    permalink
                  }
                  ... on StatusContext {
                    context
                    state
                    targetUrl
                  }
                }
              }
            }
          }
        }
      }
      comments(first: 15) {
        nodes {
          id
          author { login }
          body
          createdAt
        }
      }
      reviews(first: 10) {
        nodes {
          id
          author { login }
          body
          state
          submittedAt
          createdAt
        }
      }
      latestOpinionatedReviews(first: 30) {
        nodes {
          author { login }
          state
        }
      }
      latestReviews(first: 30) {
        nodes {
          author { login }
          state
        }
      }
      closingIssuesReferences(first: 10) {
        nodes {
          number
          repository {
            nameWithOwner
          }
        }
      }
      reviewThreads(first: 50) {
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          originalLine
          comments(first: 10) {
            nodes {
              id
              author { login }
              body
              createdAt
              path
              line
              originalLine
              diffHunk
            }
          }
        }
      }
    }
  }
  rateLimit {
    cost
    limit
    remaining
    resetAt
    used
  }
}
"#;

pub fn pr_details_body(pull_request_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": PR_DETAILS_QUERY,
        "variables": { "id": pull_request_node_id },
    })
}

/// Response wrapper for `PR_DETAILS_QUERY`. Just the fields we need
/// from the lazy-fetch path — the rest of the PR data is already on
/// `Task` from the inbox search.
#[derive(Deserialize, Debug)]
pub struct GqlPrDetailsResponse {
    pub data: Option<GqlPrDetailsData>,
    #[serde(default)]
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct GqlPrDetailsData {
    pub node: Option<GqlPrDetailsNode>,
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<GqlRateLimit>,
}

#[derive(Deserialize, Debug, Default)]
pub struct GqlPrDetailsNode {
    #[serde(default, rename = "reviewDecision")]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub author: Option<GqlAuthor>,
    #[serde(default)]
    pub assignees: GqlAssignees,
    #[serde(default, rename = "reviewRequests")]
    pub review_requests: GqlReviewRequests,
    #[serde(default)]
    pub commits: GqlCommits,
    #[serde(default)]
    pub comments: GqlComments,
    #[serde(default)]
    pub reviews: GqlReviews,
    #[serde(default, rename = "latestOpinionatedReviews")]
    pub latest_opinionated_reviews: GqlLatestReviews,
    #[serde(default, rename = "latestReviews")]
    pub latest_reviews: GqlLatestReviews,
    #[serde(default, rename = "closingIssuesReferences")]
    pub closing_issues_references: Option<GqlClosingIssues>,
    #[serde(default, rename = "reviewThreads")]
    pub review_threads: GqlReviewThreads,
}

/// What the lazy-fetch path returns, ready for the server-side
/// merge to splice into the workspace's PR. Captures every field
/// the inbox-scan `SEARCH_QUERY` deliberately omits, so a workspace
/// open (or post-tick prefetch) brings the PR to full fidelity.
#[derive(Debug, Clone)]
pub struct PrDetails {
    /// Merged activity from comments + reviews + review-threads.
    /// Deduped against the workspace's existing activity by
    /// `Workspace::merge_activity`.
    pub activities: Vec<Activity>,
    /// `closingIssuesReferences` re-keyed as `TaskId`s. Empty list
    /// means GitHub knows of no closing refs — caller should NOT
    /// fall through to title heuristics here (those already ran on
    /// the inbox-scan path).
    pub closes_issues: Vec<TaskId>,
    /// Per-check details (names + status). Empty when the PR has no
    /// status-check rollup at all.
    pub checks: Vec<CheckRun>,
    /// Refined CI status using both rollup state AND per-context
    /// in-flight detection (catches the "rollup=PENDING but no
    /// context running" branch-protection-stub case).
    pub ci: CiStatus,
    /// `reviewDecision`-derived review status with reviews-list
    /// fallback for the no-required-reviewer case.
    pub review: ReviewStatus,
    /// Reviewers who have submitted a review, with their latest state —
    /// the data the inbox scan drops. Spliced onto `Task::reviews` so
    /// the Reviewers line shows who actually reviewed, not just who's
    /// still pending.
    pub reviews: Vec<Reviewer>,
    /// Role derived from the full reviews list — corrects the
    /// inbox-scan approximation (which had to use `reviewDecision`
    /// as a proxy for "I approved").
    pub role: TaskRole,
    /// Full `needs_reply` over comments + reviews + threads.
    pub needs_reply: bool,
    /// Last commenter other than self, computed from the merged
    /// activity list. `None` means either no activity, or self
    /// is the latest contributor.
    pub last_commenter: Option<String>,
}

/// Project a `GqlPrDetailsNode` into a `PrDetails` ready for
/// `merge_pr_details_into_workspace`. Builds the activity list using
/// the same formatting rules as `pr_to_task` (so the spliced result
/// is indistinguishable from a fully-eager fetch) and re-derives the
/// PR-level fields the inbox-scan path could only approximate.
pub fn pr_details_to_details(node: &GqlPrDetailsNode, my_username: &str) -> PrDetails {
    let is_author = node
        .author
        .as_ref()
        .map(|a| a.login == my_username)
        .unwrap_or(false);

    // Activities: top-level comments, review-thread comments
    // (via `pr_details_to_activities`), and submitted reviews
    // with bodies. Same ordering + dedup downstream as
    // `pr_to_task`.
    let mut activities = activities_from_comments(&node.comments);
    activities.extend(pr_details_to_activities(node));
    // Last-resort timestamp for a review with neither submittedAt nor
    // createdAt (payloads predating the createdAt selection): the
    // newest known review timestamp, else the epoch. NEVER a
    // wall-clock now() — an unstable timestamp changes a node-id-less
    // review's merge identity every poll, so a pending review
    // re-appended as a brand-new unread row forever.
    let fallback_when = node
        .reviews
        .nodes
        .iter()
        .filter_map(|r| r.submitted_at.or(r.created_at))
        .max()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    for r in &node.reviews.nodes {
        let body = r.body.as_deref().unwrap_or("");
        if body.is_empty() && r.state != "APPROVED" && r.state != "CHANGES_REQUESTED" {
            continue;
        }
        let display = if !body.is_empty() {
            body.to_string()
        } else {
            format!("✓ {}", r.state)
        };
        activities.push(Activity {
            author: r
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into()),
            body: display,
            created_at: r.submitted_at.or(r.created_at).unwrap_or(fallback_when),
            kind: ActivityKind::Review,
            // Stable identity for `Workspace::merge_activity` — an
            // edited or later-submitted review REPLACES its stored
            // copy instead of duplicating.
            node_id: r.id.clone(),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
    }
    activities.sort_by_key(|a| std::cmp::Reverse(a.created_at));

    let last_commenter = activities
        .iter()
        .find(|a| a.author != my_username)
        .map(|a| a.author.clone());

    let closes_issues: Vec<TaskId> = node
        .closing_issues_references
        .as_ref()
        .map(|c| {
            let mut out: Vec<TaskId> = Vec::new();
            for ci in &c.nodes {
                let id = TaskId {
                    source: "github".into(),
                    key: format!("{}#{}", ci.repository.name_with_owner, ci.number),
                };
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            out
        })
        .unwrap_or_default();

    PrDetails {
        activities,
        closes_issues,
        checks: extract_check_runs_inner(&node.commits),
        ci: extract_ci_status_inner(&node.commits),
        review: derive_review_status_inner(&node.reviews.nodes, node.review_decision.as_deref()),
        reviews: submitted_reviewers(
            &node.latest_opinionated_reviews.nodes,
            &node.latest_reviews.nodes,
        ),
        role: derive_role_inner(
            &node.reviews.nodes,
            my_username,
            is_author,
            &requested_reviewer_logins(&node.review_requests),
            &assignee_logins(&node.assignees),
        ),
        needs_reply: needs_reply_check_inner(
            &node.review_threads.nodes,
            &node.comments.nodes,
            &node.reviews.nodes,
            my_username,
        ),
        last_commenter,
    }
}

/// Helper extracted from `pr_to_task` — turn a `comments` connection
/// into Activity items, skipping blank-body entries.
fn activities_from_comments(comments: &GqlComments) -> Vec<Activity> {
    comments
        .nodes
        .iter()
        .filter(|c| !c.body.trim().is_empty())
        .map(|c| Activity {
            author: c
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into()),
            body: c.body.clone(),
            created_at: c.created_at,
            kind: ActivityKind::Comment,
            node_id: c.id.clone(),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        })
        .collect()
}

/// Single-node PR fetch. Used by the notifications-driven incremental
/// path: when `/notifications` reports activity on `repo/N`, we fetch
/// exactly that one PR (rather than re-running the heavy `involves:USER`
/// SEARCH that returns the same 100 PRs we already have).
///
/// Field set MUST mirror what [`SEARCH_QUERY`] selects per PR so the
/// reused [`pr_to_task`] conversion produces the same shape — anything
/// added there should be mirrored here, or the targeted refresh will
/// silently drop fields on the row it just touched. Connection sizes
/// (`first: N`) match the search query for the same reason.
const SINGLE_PR_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id
      number
      title
      body
      url
      updatedAt
      createdAt
      closedAt
      isDraft
      state
      merged
      additions
      deletions
      headRefName
      headRefOid
      baseRefName
      mergeable
      mergeStateStatus
      reviewDecision
      autoMergeRequest { enabledAt }
      isInMergeQueue
      author { login }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              state
              contexts(first: 20) {
                nodes {
                  __typename
                  ... on CheckRun {
                    name
                    conclusion
                    status
                    permalink
                  }
                  ... on StatusContext {
                    context
                    state
                    targetUrl
                  }
                }
              }
            }
          }
        }
      }
      labels(first: 10) { nodes { name } }
      assignees(first: 10) { nodes { login } }
      reviewRequests(first: 10) {
        nodes {
          requestedReviewer {
            ... on User { login }
            ... on Team { name }
          }
        }
      }
      comments(first: 15) {
        nodes {
          id
          author { login }
          body
          createdAt
        }
      }
      reviews(first: 10) {
        nodes {
          id
          author { login }
          body
          state
          submittedAt
          createdAt
        }
      }
      latestOpinionatedReviews(first: 30) {
        nodes {
          author { login }
          state
        }
      }
      latestReviews(first: 30) {
        nodes {
          author { login }
          state
        }
      }
      closingIssuesReferences(first: 10) {
        nodes {
          number
          repository { nameWithOwner }
        }
      }
      reviewThreads(first: 50) {
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          originalLine
          comments(first: 10) {
            nodes {
              id
              author { login }
              body
              createdAt
              path
              line
              originalLine
              diffHunk
            }
          }
        }
      }
    }
  }
  rateLimit {
    cost
    limit
    remaining
    resetAt
    used
  }
}
"#;

pub fn single_pr_body(owner: &str, name: &str, number: u64) -> serde_json::Value {
    serde_json::json!({
        "query": SINGLE_PR_QUERY,
        "variables": {
            "owner": owner,
            "name": name,
            // GraphQL `Int` is i32 — PR numbers fit comfortably.
            "number": number,
        },
    })
}

#[derive(Deserialize, Debug)]
pub struct GqlSinglePrResponse {
    pub data: Option<GqlSinglePrData>,
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct GqlSinglePrData {
    pub repository: Option<GqlSinglePrRepository>,
}

#[derive(Deserialize, Debug)]
pub struct GqlSinglePrRepository {
    #[serde(rename = "pullRequest")]
    pub pull_request: Option<GqlPr>,
}

/// Single-node Issue fetch. Symmetric to `SINGLE_PR_QUERY` for issue
/// notifications. Field set mirrors `ISSUES_QUERY` so `issue_to_task`
/// reuses without modification.
const SINGLE_ISSUE_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      id
      number
      title
      body
      url
      updatedAt
      createdAt
      closedAt
      state
      author { login }
      labels(first: 10) { nodes { name } }
      assignees(first: 10) { nodes { login } }
      comments(first: 15) {
        nodes {
          id
          author { login }
          body
          createdAt
        }
      }
      repository { nameWithOwner }
    }
  }
  rateLimit {
    cost
    limit
    remaining
    resetAt
    used
  }
}
"#;

pub fn single_issue_body(owner: &str, name: &str, number: u64) -> serde_json::Value {
    serde_json::json!({
        "query": SINGLE_ISSUE_QUERY,
        "variables": {
            "owner": owner,
            "name": name,
            "number": number,
        },
    })
}

#[derive(Deserialize, Debug)]
pub struct GqlSingleIssueResponse {
    pub data: Option<GqlSingleIssueData>,
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct GqlSingleIssueData {
    pub repository: Option<GqlSingleIssueRepository>,
}

#[derive(Deserialize, Debug)]
pub struct GqlSingleIssueRepository {
    pub issue: Option<GqlIssue>,
}

const HOT_TASKS_QUERY: &str = r#"
query($ids: [ID!]!) {
  nodes(ids: $ids) {
    __typename
    ... on PullRequest {
      id
      number
      title
      body
      url
      updatedAt
      createdAt
      closedAt
      isDraft
      state
      merged
      additions
      deletions
      headRefName
      headRefOid
      baseRefName
      mergeable
      mergeStateStatus
      reviewDecision
      autoMergeRequest { enabledAt }
      isInMergeQueue
      author { login }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              state
              contexts(first: 20) {
                nodes {
                  __typename
                  ... on CheckRun {
                    name
                    conclusion
                    status
                    permalink
                  }
                  ... on StatusContext {
                    context
                    state
                    targetUrl
                  }
                }
              }
            }
          }
        }
      }
      labels(first: 10) { nodes { name color } }
      assignees(first: 10) { nodes { login } }
      reviewRequests(first: 10) {
        nodes {
          requestedReviewer {
            ... on User { login }
            ... on Team { name }
          }
        }
      }
      comments(first: 15) {
        nodes {
          id
          author { login }
          body
          createdAt
        }
      }
      reviews(first: 10) {
        nodes {
          id
          author { login }
          body
          state
          submittedAt
          createdAt
        }
      }
      latestOpinionatedReviews(first: 30) {
        nodes {
          author { login }
          state
        }
      }
      latestReviews(first: 30) {
        nodes {
          author { login }
          state
        }
      }
      closingIssuesReferences(first: 10) {
        nodes {
          number
          repository { nameWithOwner }
        }
      }
      reviewThreads(first: 50) {
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          originalLine
          comments(first: 10) {
            nodes {
              id
              author { login }
              body
              createdAt
              path
              line
              originalLine
              diffHunk
            }
          }
        }
      }
    }
    ... on Issue {
      id
      number
      title
      body
      url
      updatedAt
      createdAt
      closedAt
      state
      author { login }
      labels(first: 10) { nodes { name color } }
      assignees(first: 10) { nodes { login } }
      comments(first: 15) {
        nodes {
          id
          author { login }
          body
          createdAt
        }
      }
      repository { nameWithOwner }
    }
  }
  rateLimit {
    cost
    limit
    remaining
    resetAt
    used
  }
}
"#;

pub fn hot_tasks_body(node_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "query": HOT_TASKS_QUERY,
        "variables": { "ids": node_ids },
    })
}

#[derive(Deserialize, Debug)]
pub struct GqlHotTasksResponse {
    pub data: Option<GqlHotTasksData>,
    #[serde(default)]
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct GqlHotTasksData {
    pub nodes: Vec<Option<GqlHotTask>>,
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<GqlRateLimit>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum GqlHotTask {
    PullRequest(Box<GqlPr>),
    Issue(Box<GqlIssue>),
}

/// Convert the lazy-fetched review-thread data into the same
/// `Activity` shape the inbox search produces. Same formatting
/// rules as `pr_to_task`'s reviewThreads loop (✅/📌 prefixes,
/// thread-level path fallback, diff-hunk on first comment only) so
/// the merged activity list is indistinguishable from the eager
/// path.
pub fn pr_details_to_activities(node: &GqlPrDetailsNode) -> Vec<Activity> {
    let mut activities = Vec::new();
    for thread in &node.review_threads.nodes {
        let thread_path = thread
            .path
            .clone()
            .or_else(|| thread.comments.nodes.first().and_then(|c| c.path.clone()));
        let thread_line = thread.line.or(thread.original_line).or_else(|| {
            thread
                .comments
                .nodes
                .first()
                .and_then(|c| c.line.or(c.original_line))
        });
        for (i, c) in thread.comments.nodes.iter().enumerate() {
            let author = c
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into());
            if c.body.trim().is_empty() {
                continue;
            }
            let mut body = c.body.clone();
            if thread.is_resolved {
                body = format!("✅ {body}");
            } else if thread.is_outdated {
                body = format!("📌 outdated: {body}");
            }
            if i > 0 {
                body = format!("↳ {body}");
            }
            let (path, line, diff_hunk) = if i == 0 {
                (
                    c.path.clone().or_else(|| thread_path.clone()),
                    c.line.or(c.original_line).or(thread_line),
                    c.diff_hunk.clone(),
                )
            } else {
                (thread_path.clone(), thread_line, None)
            };
            activities.push(Activity {
                author,
                body,
                created_at: c.created_at,
                kind: ActivityKind::Review,
                node_id: c.id.clone(),
                path,
                line,
                diff_hunk,
                thread_id: thread.id.clone(),
            });
        }
    }
    activities
}

/// Convert GraphQL PR data to our Task type.
///
/// This runs on **inbox-scan** results from `SEARCH_QUERY`, which
/// deliberately omits `reviews`, `reviewThreads`, and
/// `statusCheckRollup.contexts` to cut per-poll cost (but keeps the
/// cheap, authoritative `closingIssuesReferences`, so `closes_issues`
/// is correct here). Fields that depend on the omitted ones
/// (per-reviewer role detection, per-check details, full `needs_reply`)
/// are derived from the reduced shape here; `pr_details_to_details` +
/// `merge_pr_details` back-fill the canonical values on workspace open
/// and via the post-tick prefetch.
pub fn pr_to_task(pr: &GqlPr, my_username: &str) -> Task {
    let repo = extract_repo_from_url(&pr.url);

    // Role from author + reviewDecision. Pre-trim we used the
    // reviews list to detect "I approved → Mentioned"; without
    // reviews in the inbox query we rely on the rolled-up
    // reviewDecision as a proxy. Edge case (PR approved by someone
    // else, I haven't reviewed) shows `Mentioned` instead of
    // `Reviewer` until lazy-fetch — both badges read as "no action",
    // and `merge_pr_details` fixes the role on workspace open.
    let is_author = pr
        .author
        .as_ref()
        .map(|a| a.login == my_username)
        .unwrap_or(false);
    let role = derive_role(pr, my_username, is_author);

    // State.
    let state = if pr.merged {
        TaskState::Merged
    } else if pr.state == "CLOSED" {
        TaskState::Closed
    } else if pr.is_draft {
        TaskState::Draft
    } else {
        TaskState::Open
    };

    let review = derive_review_status(pr);

    // Build activity from all sources, sorted by time. On inbox
    // scans `pr.comments.nodes` is just the last comment (used to
    // light up `last_commenter`); `reviews` and `review_threads`
    // are empty by construction. The lazy-fetch path back-fills
    // the full activity via `merge_pr_details`.
    let mut activities: Vec<Activity> = Vec::new();

    // Issue comments.
    for c in &pr.comments.nodes {
        if c.body.trim().is_empty() {
            continue;
        }
        activities.push(Activity {
            author: c
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into()),
            body: c.body.clone(),
            created_at: c.created_at,
            kind: ActivityKind::Comment,
            node_id: c.id.clone(),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
    }

    // Review threads (with resolution + outdated status).
    for thread in &pr.review_threads.nodes {
        // Thread-level path/line fall back to the first comment's values.
        let thread_path = thread
            .path
            .clone()
            .or_else(|| thread.comments.nodes.first().and_then(|c| c.path.clone()));
        let thread_line = thread.line.or(thread.original_line).or_else(|| {
            thread
                .comments
                .nodes
                .first()
                .and_then(|c| c.line.or(c.original_line))
        });

        for (i, c) in thread.comments.nodes.iter().enumerate() {
            let author = c
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into());
            if c.body.trim().is_empty() {
                continue;
            }
            let mut body = c.body.clone();

            // Prefix with context.
            if thread.is_resolved {
                body = format!("✅ {body}");
            } else if thread.is_outdated {
                body = format!("📌 outdated: {body}");
            }
            if i > 0 {
                body = format!("↳ {body}");
            }

            // Only the first comment in a thread carries the diff hunk;
            // replies inherit file/line for display context.
            let (path, line, diff_hunk) = if i == 0 {
                (
                    c.path.clone().or_else(|| thread_path.clone()),
                    c.line.or(c.original_line).or(thread_line),
                    c.diff_hunk.clone(),
                )
            } else {
                (thread_path.clone(), thread_line, None)
            };

            activities.push(Activity {
                author,
                body,
                created_at: c.created_at,
                kind: ActivityKind::Review,
                node_id: c.id.clone(),
                path,
                line,
                diff_hunk,
                thread_id: thread.id.clone(),
            });
        }
    }

    // Reviews (approve/changes requested — only with meaningful content).
    for r in &pr.reviews.nodes {
        let body = r.body.as_deref().unwrap_or("");
        if body.is_empty() && r.state != "APPROVED" && r.state != "CHANGES_REQUESTED" {
            continue;
        }
        let display = if !body.is_empty() {
            body.to_string()
        } else {
            format!("✓ {}", r.state)
        };
        activities.push(Activity {
            author: r
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into()),
            body: display,
            // Prefer submittedAt, then the (schema-non-null) createdAt.
            // Both are STABLE across polls; `pr.updated_at` is the
            // last-resort for payloads that predate the createdAt
            // selection and must never be a wall-clock now() — an
            // unstable timestamp gives a node-id-less review a new
            // identity every poll, duplicating it as unread forever.
            created_at: r.submitted_at.or(r.created_at).unwrap_or(pr.updated_at),
            kind: ActivityKind::Review,
            // The review's own node id — not a reply-to target (replies
            // go through review THREADS), but the stable identity
            // `Workspace::merge_activity` de-dupes on.
            node_id: r.id.clone(),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
    }

    activities.sort_by_key(|a| std::cmp::Reverse(a.created_at));

    let needs_reply = needs_reply_check(pr, my_username);

    // last_commenter: prefer the freshly-built activity list. When
    // it's empty (inbox-scan path on a PR with the last comment from
    // us), fall back to comments.nodes[0] author so the sidebar's
    // "from $login" line lights up immediately rather than waiting
    // for lazy-fetch.
    let last_commenter = activities
        .first()
        .filter(|a| a.author != my_username)
        .map(|a| a.author.clone())
        .or_else(|| {
            pr.comments
                .nodes
                .first()
                .and_then(|c| c.author.as_ref())
                .map(|a| a.login.clone())
                .filter(|l| l != my_username)
        });

    // unread_count: prefer comments.totalCount when the query
    // selected it (inbox-scan); fall back to the built activity
    // count for fully-eager callers (tests + lazy-fetch).
    let unread_count = pr.comments.total_count.unwrap_or(activities.len() as u32);

    Task {
        id: TaskId {
            source: "github".into(),
            key: format!("{repo}#{}", pr.number),
        },
        title: pr.title.clone(),
        body: pr.body.clone(),
        state,
        role,
        ci: extract_ci_status(pr),
        review,
        checks: extract_check_runs(pr),
        unread_count,
        url: pr.url.clone(),
        repo: Some(repo.clone()),
        branch: Some(pr.head_ref_name.clone()),
        base_branch: if pr.base_ref_name.is_empty() {
            None
        } else {
            Some(pr.base_ref_name.clone())
        },
        updated_at: pr.updated_at,
        created_at: pr.created_at,
        closed_at: pr.closed_at,
        labels: pr
            .labels
            .nodes
            .iter()
            .map(|l| lazybox_core::Label {
                name: l.name.clone(),
                color: l.color.clone().unwrap_or_default(),
            })
            .collect(),
        reviewers: pr
            .review_requests
            .nodes
            .iter()
            .filter_map(|rr| match rr.requested_reviewer.as_ref()? {
                GqlRequestedReviewer::User { login } => Some(login.clone()),
                GqlRequestedReviewer::Team { name } => Some(format!("team/{name}")),
                // Bot / mannequin / app reviewer — no display name to
                // show, so it's omitted from the reviewers list.
                GqlRequestedReviewer::Other(_) => None,
            })
            .collect(),
        reviews: submitted_reviewers(
            &pr.latest_opinionated_reviews.nodes,
            &pr.latest_reviews.nodes,
        ),
        assignees: pr.assignees.nodes.iter().map(|a| a.login.clone()).collect(),
        author: pr
            .author
            .as_ref()
            .map(|a| a.login.clone())
            .unwrap_or_default(),
        auto_merge_enabled: pr.auto_merge_request.is_some(),
        is_in_merge_queue: pr.is_in_merge_queue,
        mergeable: match pr.mergeable.as_deref() {
            Some("CONFLICTING") => lazybox_core::Mergeable::Conflicting,
            Some("MERGEABLE") => lazybox_core::Mergeable::Mergeable,
            // Merged / closed PRs: GitHub NEVER computes mergeability
            // for terminal PRs — their `mergeable` stays null (or
            // UNKNOWN) forever. Classifying that as `Unknown` put the
            // poll scheduler in a permanent 5s fast-loop, because the
            // merged sweep re-surfaces these PRs every tick and
            // `Unknown` is its "re-poll soon, GitHub is still
            // computing" trigger. A terminal PR has no merge-conflict
            // question left to answer — mirror `issue_to_task` (which
            // reports `Mergeable` for issues, another kind that can
            // never conflict) and settle on a definitive value.
            _ if matches!(state, TaskState::Merged | TaskState::Closed) => {
                lazybox_core::Mergeable::Mergeable
            }
            // Open PR: GitHub returns "UNKNOWN" (or null) while it
            // lazily computes mergeability — surface as Unknown
            // rather than guess; a re-query nudges the computation.
            _ => lazybox_core::Mergeable::Unknown,
        },
        is_behind_base: pr.merge_state_status.as_deref() == Some("BEHIND"),
        merge_blocked: pr.merge_state_status.as_deref() == Some("BLOCKED"),
        node_id: pr.id.clone(),
        needs_reply,
        last_commenter,
        recent_activity: activities,
        additions: pr.additions,
        deletions: pr.deletions,
        closes_issues: extract_closes_issues(pr, &repo),
        linked_tasks: extract_linear_refs(&pr.head_ref_name, &pr.title),
        kind: Some(lazybox_core::TaskKind::Pr),
        priority: None,
        state_label: None,
    }
}

/// Cross-provider links (#922): the Linear ticket(s) a PR references,
/// mined from the PR's branch name and title only. Linear's GitHub
/// integration names branches `<user>/<team>-<n>-<slug>`, so the ticket
/// identifier is almost always in the branch, and a title is one line of
/// user intent. Returns `{linear, ENG-123}` `TaskId`s — the exact id the
/// Linear provider mints — deduped in discovery order (branch, then
/// title).
///
/// The PR **body** is deliberately NOT scanned. `<TEAM>-<n>` collides
/// with ordinary technical prose (`UTF-8`, `SHA-256`, `ISO-8601`,
/// `CVE-2021-1234`), so scanning arbitrary body text would mint bogus
/// links and, worse, silently collapse an unrelated ticket whose
/// identifier coincidentally matched. This mirrors `extract_closes_issues`,
/// which refuses to parse the body for the same reason. A body-only
/// reference is still covered by Linear's authoritative attachment signal
/// (see the Linear provider), matched separately in the poller.
fn extract_linear_refs(branch: &str, title: &str) -> Vec<TaskId> {
    let mut out: Vec<TaskId> = Vec::new();
    let mut push = |ident: String| {
        let id = TaskId {
            source: "linear".into(),
            key: ident,
        };
        if !out.contains(&id) {
            out.push(id);
        }
    };
    for ident in parse_linear_identifiers(branch) {
        push(ident);
    }
    for ident in parse_linear_identifiers(title) {
        push(ident);
    }
    out
}

/// Scan `text` for Linear issue identifiers — `<TEAM>-<number>`, where
/// TEAM is 1+ ASCII letters and number is 1+ digits (`ENG-123`,
/// `PLAT-7`). Case-insensitive on the team key, normalized to
/// upper-case so `eng-123` in a branch matches the ticket's `ENG-123`
/// identifier. Boundary-checked on both sides so `v2-3` (leading digit),
/// `foo-eng-1` (letters run into the team), and `ENG-1x` (trailing
/// alphanumeric) don't false-match. Returns identifiers in scan order,
/// deduped.
fn parse_linear_identifiers(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Find the start of a letter run whose left boundary is a
        // non-alphanumeric (so a team key can't begin mid-word).
        if !bytes[i].is_ascii_alphabetic() || (i > 0 && bytes[i - 1].is_ascii_alphanumeric()) {
            i += 1;
            continue;
        }
        let team_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'-' {
            continue;
        }
        let dash = i;
        let num_start = dash + 1;
        let mut j = num_start;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        // Need at least one digit, and the right boundary must not be an
        // alphanumeric (`ENG-1x` is not an identifier).
        if j > num_start && (j >= bytes.len() || !bytes[j].is_ascii_alphanumeric()) {
            let team = text[team_start..dash].to_ascii_uppercase();
            let ident = format!("{team}-{}", &text[num_start..j]);
            if !out.contains(&ident) {
                out.push(ident);
            }
        }
        i = j;
    }
    out
}

/// Build the `closes_issues` list for a PR Task. Three sources,
/// merged:
///
/// 1. GitHub's structured `closingIssuesReferences` field — the
///    authoritative source. GitHub populates it for any closing
///    keyword in the PR BODY, and only for actual ISSUES the PR
///    closes (PR-to-PR `Closes #N` is silently ignored upstream,
///    which is correct).
/// 2. Closing keywords in the PR TITLE — fallback when (1) is
///    empty or missed an entry. Users routinely write
///    `feat: add foo (closes #31)` in the title without restating
///    it in the body; GitHub's auto-linker doesn't always pick
///    that up, especially for cross-fork or manually-edited PRs.
/// 3. Bare parenthesized references in the PR TITLE — `add foo
///    (#98)`. This keyword-less `(#N)` convention is ubiquitous
///    (including GitHub's own squash-merge suffix), and users
///    expect it to link the issue. `#N` syntax is shared between
///    issues and PRs, so this list is a *candidate* set: the
///    collapse step (`merge_closing_issue_workspaces`) resolves
///    each entry against tracked ISSUE workspaces only and drops
///    anything that turns out to be a PR. That downstream filter
///    is what makes parsing bare references safe.
///
/// We deliberately do NOT parse PR BODY text. Body shares `#N`
/// syntax between issues and PRs and contains arbitrary prose,
/// so a parser there led to PR rows being silently absorbed into
/// other PRs that "referenced" them. Title is one line of
/// user-authored intent — far safer.
fn extract_closes_issues(pr: &GqlPr, pr_repo: &str) -> Vec<TaskId> {
    let mut out: Vec<TaskId> = Vec::new();
    if let Some(refs) = pr.closing_issues_references.as_ref() {
        for ci in &refs.nodes {
            let id = TaskId {
                source: "github".into(),
                key: format!("{}#{}", ci.repository.name_with_owner, ci.number),
            };
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    // Title parsing (sources 2 + 3). Both name the PR's own repo
    // (`pr_repo`, extracted from the PR's URL by the caller) — title
    // references almost always point at same-repo issues. Cross-repo
    // references use the fully-qualified `owner/repo#N` syntax that
    // neither title parser handles yet, left out by design.
    let title_refs = parse_closes_from_title(&pr.title)
        .into_iter()
        .chain(parse_bare_refs_from_title(&pr.title));
    for n in title_refs {
        let id = TaskId {
            source: "github".into(),
            key: format!("{pr_repo}#{n}"),
        };
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Scan a PR title for closing keywords + `#N`. Returns the issue
/// numbers in title order, deduped. Recognizes `close`, `closes`,
/// `closed`, `closing`, `fix`, `fixes`, `fixed`, `fixing`, `resolve`,
/// `resolves`, `resolved`, `resolving` (GitHub's full keyword list).
/// Case-insensitive. Tolerant of `:` / whitespace between the
/// keyword and `#N`.
pub(crate) fn parse_closes_from_title(title: &str) -> Vec<u64> {
    const KEYWORDS: &[&str] = &[
        "close",
        "closes",
        "closed",
        "closing",
        "fix",
        "fixes",
        "fixed",
        "fixing",
        "resolve",
        "resolves",
        "resolved",
        "resolving",
    ];
    let lower = title.to_ascii_lowercase();
    let bytes_lower = lower.as_bytes();
    let mut out: Vec<u64> = Vec::new();
    let mut i = 0;
    while i < bytes_lower.len() {
        // Match any keyword at position `i` that is bounded by a
        // non-letter character on the left (start of string OR
        // previous char is non-alphabetic) — so `unclose` /
        // `fixed-it` don't false-trigger.
        let left_ok = i == 0 || !bytes_lower[i - 1].is_ascii_alphabetic();
        if left_ok {
            for &kw in KEYWORDS {
                let kw_bytes = kw.as_bytes();
                if bytes_lower[i..].starts_with(kw_bytes) {
                    let after = i + kw_bytes.len();
                    // Right-bound: must be followed by non-letter so
                    // `closer` doesn't match `close`.
                    let right_ok =
                        after >= bytes_lower.len() || !bytes_lower[after].is_ascii_alphabetic();
                    if !right_ok {
                        continue;
                    }
                    // Skip whitespace + optional ':' between keyword
                    // and `#N`.
                    let mut j = after;
                    while j < bytes_lower.len()
                        && (bytes_lower[j].is_ascii_whitespace() || bytes_lower[j] == b':')
                    {
                        j += 1;
                    }
                    // Expect `#` then digits.
                    if j < bytes_lower.len() && bytes_lower[j] == b'#' {
                        j += 1;
                        let num_start = j;
                        while j < bytes_lower.len() && bytes_lower[j].is_ascii_digit() {
                            j += 1;
                        }
                        if j > num_start
                            && let Ok(n) = lower[num_start..j].parse::<u64>()
                            && !out.contains(&n)
                        {
                            out.push(n);
                        }
                    }
                    // Advance past this match and continue scanning.
                    i = after;
                    break;
                }
            }
        }
        i += 1;
    }
    out
}

/// Scan a PR title for bare parenthesized issue references —
/// `add foo (#98)`, `(#1, #2)` — with no closing keyword. Returns
/// the numbers in title order, deduped.
///
/// To avoid mistaking prose like `(see #5 for context)` for a
/// reference, the parenthesized group must contain ONLY references:
/// `#digits` items separated by commas and/or whitespace, nothing
/// else. `(closes #31)` is rejected here (the leading keyword fails
/// the "only references" test) — `parse_closes_from_title` already
/// covers that form.
///
/// `#N` is shared between issues and PRs, so these are *candidates*:
/// the caller's collapse step resolves each against tracked ISSUE
/// workspaces and ignores PRs.
pub(crate) fn parse_bare_refs_from_title(title: &str) -> Vec<u64> {
    let bytes = title.as_bytes();
    let mut out: Vec<u64> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'(' {
            i += 1;
            continue;
        }
        let inner_start = i + 1;
        let mut j = inner_start;
        while j < bytes.len() && bytes[j] != b')' {
            j += 1;
        }
        if j >= bytes.len() {
            break; // no closing paren — nothing more to match
        }
        if let Some(nums) = parse_ref_group(&title[inner_start..j]) {
            for n in nums {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        i = j + 1;
    }
    out
}

/// Parse the inside of a parenthesized group as a pure list of
/// `#N` references separated by commas/whitespace. Returns `None`
/// the moment any other character appears, so only `(#98)`-style
/// groups — not prose parentheticals — are treated as references.
fn parse_ref_group(inner: &str) -> Option<Vec<u64>> {
    let bytes = inner.as_bytes();
    let mut nums: Vec<u64> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b',' => i += 1,
            b'#' => {
                let num_start = i + 1;
                let mut j = num_start;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j == num_start {
                    return None; // `#` with no digits
                }
                nums.push(inner[num_start..j].parse::<u64>().ok()?);
                i = j;
            }
            _ => return None, // any other content → not a pure ref group
        }
    }
    if nums.is_empty() { None } else { Some(nums) }
}

/// Comprehensive needs_reply: check unresolved threads, latest issue comment,
/// and latest review. If the most recent communication on any channel is from
/// someone else and you haven't responded, you owe a reply.
fn needs_reply_check(pr: &GqlPr, my_username: &str) -> bool {
    needs_reply_check_inner(
        &pr.review_threads.nodes,
        &pr.comments.nodes,
        &pr.reviews.nodes,
        my_username,
    )
}

fn needs_reply_check_inner(
    review_threads: &[GqlReviewThread],
    comments: &[GqlComment],
    reviews: &[GqlReview],
    my_username: &str,
) -> bool {
    // Re-package the args into a synthetic-feeling shape just for
    // the existing closure body below. Kept as one function so the
    // three-branch logic stays co-located.
    let pr = NeedsReplyView {
        review_threads,
        comments,
        reviews,
    };
    needs_reply_view(&pr, my_username)
}

struct NeedsReplyView<'a> {
    review_threads: &'a [GqlReviewThread],
    comments: &'a [GqlComment],
    reviews: &'a [GqlReview],
}

fn needs_reply_view(pr: &NeedsReplyView<'_>, my_username: &str) -> bool {
    // 1. Unresolved review threads where the LAST comment is from someone else.
    for t in pr.review_threads {
        if t.is_resolved || t.is_outdated {
            continue;
        }
        if let Some(last) = t.comments.nodes.last()
            && let Some(author) = &last.author
            && author.login != my_username
        {
            return true;
        }
    }

    // 2. Latest issue comment is from someone else (and after our last response).
    let my_last_comment = pr
        .comments
        .iter()
        .filter(|c| {
            c.author
                .as_ref()
                .map(|a| a.login == my_username)
                .unwrap_or(false)
        })
        .map(|c| c.created_at)
        .max();
    let last_others_comment = pr
        .comments
        .iter()
        .filter(|c| {
            c.author
                .as_ref()
                .map(|a| a.login != my_username)
                .unwrap_or(false)
        })
        .map(|c| c.created_at)
        .max();
    if let Some(other) = last_others_comment
        && my_last_comment.map(|m| other > m).unwrap_or(true)
    {
        return true;
    }

    // 3. Latest review with body from someone else (after our last review/comment).
    let last_others_review = pr
        .reviews
        .iter()
        .filter(|r| {
            r.author
                .as_ref()
                .map(|a| a.login != my_username)
                .unwrap_or(false)
                && r.body.as_deref().map(|b| !b.is_empty()).unwrap_or(false)
        })
        .filter_map(|r| r.submitted_at)
        .max();
    if let Some(other) = last_others_review {
        // "Our last review/comment" — a user who reviewed but never
        // commented previously fell through `my_last_comment = None`
        // and got falsely flagged as owing a reply.
        let my_last_review = pr
            .reviews
            .iter()
            .filter(|r| {
                r.author
                    .as_ref()
                    .map(|a| a.login == my_username)
                    .unwrap_or(false)
            })
            .filter_map(|r| r.submitted_at)
            .max();
        let my_latest = match (my_last_comment, my_last_review) {
            (Some(c), Some(r)) => Some(c.max(r)),
            (Some(c), None) => Some(c),
            (None, Some(r)) => Some(r),
            (None, None) => None,
        };
        if my_latest.map(|m| other > m).unwrap_or(true) {
            return true;
        }
    }

    false
}

fn extract_ci_status(pr: &GqlPr) -> CiStatus {
    extract_ci_status_inner(&pr.commits)
}

fn extract_ci_status_inner(commits: &GqlCommits) -> CiStatus {
    let Some(commit_node) = commits.nodes.first() else {
        return CiStatus::None;
    };
    let Some(rollup) = &commit_node.commit.status_check_rollup else {
        return CiStatus::None;
    };
    match rollup.state.as_str() {
        "SUCCESS" => CiStatus::Success,
        "FAILURE" | "ERROR" => CiStatus::Failure,
        // PENDING = the rollup is genuinely in flight (queued or
        // running). When `contexts` is present (lazy-fetch path)
        // walk them to distinguish "actually running" from
        // "branch protection declared a required check that nobody
        // has triggered" — the latter renders as `CI RUN` today
        // which misleads the user into thinking a job is in
        // progress when it isn't.
        //
        // The inbox-scan query omits `contexts` to cut cost; in
        // that case we trust the rollup's `PENDING` verbatim (small
        // false-positive risk on the "expected but unstarted" case,
        // corrected by the next lazy-fetch).
        "PENDING" => match rollup.contexts.as_ref() {
            Some(ctxs) => {
                let any_in_flight = ctxs.nodes.iter().any(|ctx| match ctx {
                    GqlCheckContext::CheckRun { status, .. } => matches!(
                        status.as_deref(),
                        Some("IN_PROGRESS" | "QUEUED" | "WAITING" | "PENDING" | "REQUESTED")
                    ),
                    GqlCheckContext::StatusContext { state, .. } => {
                        matches!(state.as_str(), "PENDING")
                    }
                });
                if any_in_flight {
                    CiStatus::Pending
                } else {
                    // Rollup says PENDING but every context is
                    // EXPECTED / COMPLETED — required check exists
                    // in branch protection but no job is running.
                    // Surface as None so the sidebar doesn't pretend
                    // something is in flight.
                    CiStatus::None
                }
            }
            None => CiStatus::Pending,
        },
        // Top-level `EXPECTED` is the same "required but unstarted"
        // case GitHub uses when branch protection lists a check
        // that hasn't been triggered. Don't lie that it's running.
        "EXPECTED" => CiStatus::None,
        _ => CiStatus::None,
    }
}

/// Role derivation. Author check is up top because it always wins.
/// `reviewDecision == APPROVED` is a proxy for "I approved →
/// Mentioned" — without the reviews list in the inbox query, we
/// can't tell whose approval it was, so we err on the side of
/// "no action needed" (matches what the user would see on a
/// fully-approved PR). `merge_pr_details` re-runs this against the
/// full reviews list on workspace open and corrects any
/// misclassification.
fn derive_role(pr: &GqlPr, my_username: &str, is_author: bool) -> TaskRole {
    derive_role_inner(
        &pr.reviews.nodes,
        my_username,
        is_author,
        &requested_reviewer_logins(&pr.review_requests),
        &assignee_logins(&pr.assignees),
    )
}

/// User logins with an outstanding review request. Team requests carry
/// no login (they're a `Team { name }`) and non-user reviewers (bots,
/// mannequins) none either, so both are dropped — a role is about *me*,
/// and I match by my own login.
fn requested_reviewer_logins(requests: &GqlReviewRequests) -> Vec<&str> {
    requests
        .nodes
        .iter()
        .filter_map(|rr| match rr.requested_reviewer.as_ref()? {
            GqlRequestedReviewer::User { login } => Some(login.as_str()),
            GqlRequestedReviewer::Team { .. } | GqlRequestedReviewer::Other(_) => None,
        })
        .collect()
}

fn assignee_logins(assignees: &GqlAssignees) -> Vec<&str> {
    assignees.nodes.iter().map(|a| a.login.as_str()).collect()
}

/// My most recent *decisive* review state (`APPROVED` /
/// `CHANGES_REQUESTED`) on a PR, or `None` if I've only commented or
/// never reviewed. Latest wins so a later `CHANGES_REQUESTED` supersedes
/// an earlier `APPROVED` I dismissed. Only populated on the lazy-fetch
/// path — the inbox scan omits the reviews connection.
fn my_latest_review_state<'a>(reviews: &'a [GqlReview], my_username: &str) -> Option<&'a str> {
    reviews
        .iter()
        .filter(|r| {
            r.author
                .as_ref()
                .map(|a| a.login == my_username)
                .unwrap_or(false)
        })
        .filter(|r| matches!(r.state.as_str(), "APPROVED" | "CHANGES_REQUESTED"))
        .max_by_key(|r| r.submitted_at)
        .map(|r| r.state.as_str())
}

/// My relationship to a PR, for the sidebar role filter.
///
/// The sidebar is populated by an `involves:<me>` search, which returns
/// every PR I've so much as commented on — so "not the author" must NOT
/// blanket-default to `Reviewer` (the bug this replaced surfaced every
/// involved PR under the `reviewer` filter). Role is derived from
/// concrete per-me signals, in priority order:
///
/// 1. I opened it → [`TaskRole::Author`].
/// 2. A review is currently requested from me → [`TaskRole::Reviewer`].
///    This outranks a prior approval: a re-request after I approved
///    means GitHub wants my eyes again.
/// 3. I'm assigned → [`TaskRole::Assignee`]. Checked *before* the
///    reviews list because assignment is visible on both the inbox scan
///    and lazy-fetch, whereas the reviews list is lazy-only — putting it
///    first keeps an assigned-and-approved PR reading `Assignee` on both
///    paths instead of flipping to `Mentioned` when the workspace opens
///    and `merge_pr_details` overwrites the role.
/// 4. My latest decisive review (lazy path only): `APPROVED` clears my
///    obligation → [`TaskRole::Mentioned`]; anything else (changes
///    requested / a review comment) keeps me a [`TaskRole::Reviewer`].
/// 5. Otherwise I'm only involved via a comment or @-mention →
///    [`TaskRole::Mentioned`].
///
/// Because the scan-visible signals (author / requested / assignee) are
/// all resolved before the lazy-only reviews list, the open-time
/// re-derivation (`handlers.rs`, `pr.role = details.role`) can only ever
/// *refine* the role, never flip one scan-visible relationship to
/// another.
///
/// Known scan-path approximations, corrected on workspace open: (a) a
/// PR I've already reviewed reads `Mentioned` on the scan — GitHub drops
/// me from `reviewRequests` once I review and the scan omits the reviews
/// list, so it's indistinguishable from a never-involved PR until
/// lazy-fetch; (b) a review requested only from my *team*, or a reviewer
/// / assignee beyond the query's `first:` cap, also reads `Mentioned`
/// (team membership isn't in the poll payload). Both err toward
/// `Mentioned` rather than a false `Reviewer`, keeping the `reviewer`
/// filter free of the commenter noise this rework removed.
fn derive_role_inner(
    reviews: &[GqlReview],
    my_username: &str,
    is_author: bool,
    requested_reviewers: &[&str],
    assignees: &[&str],
) -> TaskRole {
    if is_author {
        return TaskRole::Author;
    }
    if requested_reviewers.contains(&my_username) {
        return TaskRole::Reviewer;
    }
    if assignees.contains(&my_username) {
        return TaskRole::Assignee;
    }
    if let Some(latest) = my_latest_review_state(reviews, my_username) {
        return if latest == "APPROVED" {
            TaskRole::Mentioned
        } else {
            TaskRole::Reviewer
        };
    }
    TaskRole::Mentioned
}

/// The Reviewers section as GitHub renders it: one entry per reviewer
/// with their current state. Derived from GitHub's own per-author
/// collapses rather than a slice of the raw `reviews` history, so it
/// cannot miss a reviewer whose latest verdict sits beyond a
/// `reviews(first: N)` cap:
///
/// - `opinionated` (`latestOpinionatedReviews`) is each reviewer's
///   latest APPROVED / CHANGES_REQUESTED — the decisive ✓/✗ state. A
///   later plain `COMMENTED` never appears here, so an approve-then-
///   comment reviewer correctly stays "approved", matching GitHub.
/// - `latest` (`latestReviews`) is each reviewer's latest review of any
///   kind; it only contributes `COMMENTED` for reviewers who have *no*
///   decisive verdict, surfacing comment-only reviewers.
///
/// `PENDING` (an unsubmitted draft) and `DISMISSED` reviews contribute
/// nothing. Decisive reviewers come first, then comment-only ones. Empty
/// on the inbox-scan path (both connections are lazy-only) — back-filled
/// on workspace open.
fn submitted_reviewers(
    opinionated: &[GqlReviewerVerdict],
    latest: &[GqlReviewerVerdict],
) -> Vec<Reviewer> {
    use std::collections::HashSet;
    let mut out: Vec<Reviewer> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for v in opinionated {
        let Some(author) = v.author.as_ref() else {
            continue;
        };
        let state = match v.state.as_str() {
            "APPROVED" => ReviewState::Approved,
            "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
            _ => continue, // `latestOpinionatedReviews` only carries these
        };
        if seen.insert(author.login.as_str()) {
            out.push(Reviewer {
                login: author.login.clone(),
                state,
            });
        }
    }
    for v in latest {
        let Some(author) = v.author.as_ref() else {
            continue;
        };
        if v.state == "COMMENTED" && seen.insert(author.login.as_str()) {
            out.push(Reviewer {
                login: author.login.clone(),
                state: ReviewState::Commented,
            });
        }
    }
    out
}

/// Review-status derivation. `reviewDecision` covers the common case
/// (branch protection / CODEOWNERS); when it's null (no required
/// review configured) we fall back to walking the reviews list — but
/// only if it's populated. The inbox-scan path returns an empty
/// reviews list, so a null `reviewDecision` collapses to `None` on
/// inbox scans; lazy-fetch back-fills the canonical status.
fn derive_review_status(pr: &GqlPr) -> ReviewStatus {
    derive_review_status_inner(&pr.reviews.nodes, pr.review_decision.as_deref())
}

fn derive_review_status_inner(
    reviews: &[GqlReview],
    review_decision: Option<&str>,
) -> ReviewStatus {
    match review_decision {
        Some("APPROVED") => ReviewStatus::Approved,
        Some("CHANGES_REQUESTED") => ReviewStatus::ChangesRequested,
        Some("REVIEW_REQUIRED") => ReviewStatus::Pending,
        _ => {
            // No required review — derive from actual submissions.
            // Each reviewer's latest non-COMMENT/PENDING/DISMISSED
            // state is what counts: APPROVED or CHANGES_REQUESTED.
            // The reviews array is in submitted-order so a later
            // CHANGES_REQUESTED supersedes an earlier APPROVED from
            // the same author.
            use std::collections::HashMap;
            let mut latest: HashMap<String, &str> = HashMap::new();
            for r in reviews {
                let Some(author) = r.author.as_ref() else {
                    continue;
                };
                match r.state.as_str() {
                    "APPROVED" | "CHANGES_REQUESTED" => {
                        latest.insert(author.login.clone(), r.state.as_str());
                    }
                    _ => {}
                }
            }
            if latest.values().any(|s| *s == "CHANGES_REQUESTED") {
                ReviewStatus::ChangesRequested
            } else if latest.values().any(|s| *s == "APPROVED") {
                ReviewStatus::Approved
            } else {
                ReviewStatus::None
            }
        }
    }
}

fn extract_check_runs(pr: &GqlPr) -> Vec<CheckRun> {
    extract_check_runs_inner(&pr.commits)
}

fn extract_check_runs_inner(commits: &GqlCommits) -> Vec<CheckRun> {
    let Some(commit_node) = commits.nodes.first() else {
        return vec![];
    };
    let Some(rollup) = &commit_node.commit.status_check_rollup else {
        return vec![];
    };
    let Some(ctxs) = rollup.contexts.as_ref() else {
        // Inbox-scan path: contexts are dropped from `SEARCH_QUERY`
        // to cut cost. `pr.checks` stays empty until lazy-fetch
        // (`PR_DETAILS_QUERY`) populates it via `merge_pr_details`.
        return vec![];
    };
    ctxs.nodes
        .iter()
        .map(|ctx| match ctx {
            GqlCheckContext::CheckRun {
                name,
                conclusion,
                permalink,
                ..
            } => CheckRun {
                name: name.clone(),
                status: match conclusion.as_deref() {
                    Some("SUCCESS") => CiStatus::Success,
                    Some("FAILURE") | Some("ACTION_REQUIRED") | Some("TIMED_OUT") => {
                        CiStatus::Failure
                    }
                    Some("CANCELLED") => CiStatus::Failure,
                    Some(_) => CiStatus::None,
                    None => CiStatus::Running,
                },
                url: permalink.clone(),
            },
            GqlCheckContext::StatusContext {
                context,
                state,
                target_url,
            } => CheckRun {
                name: context.clone(),
                status: match state.as_str() {
                    "SUCCESS" => CiStatus::Success,
                    "FAILURE" | "ERROR" => CiStatus::Failure,
                    "PENDING" | "EXPECTED" => CiStatus::Pending,
                    _ => CiStatus::None,
                },
                url: target_url.clone(),
            },
        })
        .collect()
}

fn extract_repo_from_url(url: &str) -> String {
    // https://github.com/owner/repo/pull/123
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    if parts.len() >= 5 {
        format!("{}/{}", parts[3], parts[4])
    } else {
        "unknown/unknown".into()
    }
}

// ─── GitHub Issues ─────────────────────────────────────────────────────
//
// Issues get fetched alongside PRs by a separate GraphQL query. Issues
// are strictly a subset of PR fields (no branches, no CI, no reviewers)
// so the query is simpler.

/// Same connection-size trim as the PR query — see the long
/// comment on `SEARCH_QUERY` for the rate-budget rationale. Issues
/// are simpler (no reviews / review threads), so the only knob to
/// turn is `comments`.
const ISSUES_QUERY: &str = r#"
query($query: String!, $first: Int!, $after: String) {
  search(query: $query, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on Issue {
        id
        number
        title
        body
        url
        updatedAt
        createdAt
        closedAt
        state
        author { login }
        labels(first: 10) { nodes { name color } }
        assignees(first: 10) { nodes { login } }
        reactions(content: EYES) { viewerHasReacted }
        comments(first: 15) {
          nodes {
            id
            author { login }
            body
            createdAt
            reactions(content: EYES) { viewerHasReacted }
          }
        }
        repository {
          nameWithOwner
        }
      }
    }
  }
  rateLimit {
    cost
    limit
    remaining
    resetAt
    used
  }
}
"#;

#[derive(Deserialize, Debug)]
pub struct GqlIssue {
    #[serde(default)]
    pub id: Option<String>,
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
    /// When the issue was opened. Stable across later activity — the
    /// signal for the sidebar's age display + stale fade (issue #274).
    /// `default` so queries that omit `createdAt` fall back to
    /// `updatedAt`.
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<DateTime<Utc>>,
    /// When the issue was closed. Stable across later activity, unlike
    /// `updatedAt`. `None` while the issue is open.
    #[serde(rename = "closedAt", default)]
    pub closed_at: Option<DateTime<Utc>>,
    pub state: String, // OPEN, CLOSED
    pub author: Option<GqlAuthor>,
    pub labels: GqlLabels,
    pub assignees: GqlAssignees,
    pub comments: GqlComments,
    #[serde(default)]
    pub repository: Option<GqlIssueRepo>,
    /// Eyes-reaction state for the authenticated viewer on the issue
    /// BODY (separate from per-comment reactions on
    /// `comments.nodes[].reactions`). See [`GqlReactionView`] for the
    /// semantics + why lazybox uses 👀 as an idempotency marker.
    #[serde(default)]
    pub reactions: Option<GqlReactionView>,
}

#[derive(Deserialize, Debug)]
pub struct GqlIssueRepo {
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
}

#[derive(Deserialize, Debug)]
pub struct GqlIssueSearch {
    pub nodes: Vec<GqlIssue>,
    #[serde(rename = "issueCount", default)]
    pub issue_count: u32,
    #[serde(rename = "pageInfo", default)]
    pub page_info: Option<GqlPageInfo>,
}

#[derive(Deserialize, Debug)]
pub struct GqlIssueData {
    pub search: GqlIssueSearch,
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<GqlRateLimit>,
}

#[derive(Deserialize, Debug)]
pub struct GqlIssueResponse {
    pub data: Option<GqlIssueData>,
    pub errors: Option<Vec<GqlError>>,
}

/// Issues counterpart to [`build_query`]. Same contract: caller
/// owns the qualifier list. Provided as a separate function only
/// to keep the issue-specific defaults (`is:issue`) from leaking
/// into the PR path.
pub fn build_issues_query(qualifiers: &[String]) -> String {
    qualifiers.join(" ")
}

/// Issue-search defaults — same shape as [`default_search_qualifiers`]
/// but with `is:issue` instead of `is:pr`.
pub fn default_issues_qualifiers() -> Vec<String> {
    vec![
        "is:open".to_string(),
        "is:issue".to_string(),
        "archived:false".to_string(),
    ]
}

const ISSUE_PAGE_SIZE: u32 = 100;

pub fn issue_page_count(issue_count: u32) -> u32 {
    issue_count.div_ceil(ISSUE_PAGE_SIZE).max(1)
}

pub fn issues_query_body(search_query: &str, after: Option<&str>) -> serde_json::Value {
    let variables = match after {
        Some(cursor) => serde_json::json!({
            "query": search_query,
            "first": ISSUE_PAGE_SIZE,
            "after": cursor,
        }),
        None => serde_json::json!({
            "query": search_query,
            "first": ISSUE_PAGE_SIZE,
        }),
    };
    serde_json::json!({
        "query": ISSUES_QUERY,
        "variables": variables,
    })
}

/// Convert a GraphQL Issue into our `Task` type. Issues have no
/// branch/CI/reviewers — those fields stay empty / None / Default.
/// Role is determined from author + assignees against `my_username`.
pub fn issue_to_task(issue: &GqlIssue, my_username: &str) -> Task {
    let repo = issue
        .repository
        .as_ref()
        .map(|r| r.name_with_owner.clone())
        .unwrap_or_else(|| extract_repo_from_url(&issue.url));

    let is_author = issue
        .author
        .as_ref()
        .map(|a| a.login == my_username)
        .unwrap_or(false);
    let is_assignee = issue.assignees.nodes.iter().any(|a| a.login == my_username);
    let role = if is_author {
        TaskRole::Author
    } else if is_assignee {
        TaskRole::Assignee
    } else {
        TaskRole::Mentioned
    };

    let state = match issue.state.as_str() {
        "OPEN" => TaskState::Open,
        "CLOSED" => TaskState::Closed,
        _ => TaskState::Open,
    };

    let comments: Vec<Activity> = issue
        .comments
        .nodes
        .iter()
        .filter(|c| c.author.is_some())
        .map(|c| Activity {
            author: c.author.as_ref().unwrap().login.clone(),
            body: c.body.clone(),
            created_at: c.created_at,
            kind: ActivityKind::Comment,
            node_id: c.id.clone(),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        })
        .collect();

    let needs_reply = comments
        .last()
        .map(|c| c.author != my_username)
        .unwrap_or(false);
    let last_commenter = comments
        .iter()
        .rev()
        .find(|a| a.author != my_username)
        .map(|a| a.author.clone());

    Task {
        id: TaskId {
            source: "github".into(),
            key: format!("{repo}#{}", issue.number),
        },
        title: issue.title.clone(),
        body: issue.body.clone(),
        state,
        role,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: comments.len() as u32,
        url: issue.url.clone(),
        repo: Some(repo),
        branch: None,
        base_branch: None,
        updated_at: issue.updated_at,
        created_at: issue.created_at,
        closed_at: issue.closed_at,
        labels: issue
            .labels
            .nodes
            .iter()
            .map(|l| lazybox_core::Label {
                name: l.name.clone(),
                color: l.color.clone().unwrap_or_default(),
            })
            .collect(),
        reviewers: vec![],
        reviews: vec![],
        assignees: issue
            .assignees
            .nodes
            .iter()
            .map(|a| a.login.clone())
            .collect(),
        author: issue
            .author
            .as_ref()
            .map(|a| a.login.clone())
            .unwrap_or_default(),
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Mergeable,
        is_behind_base: false,
        merge_blocked: false,
        node_id: issue.id.clone(),
        needs_reply,
        last_commenter,
        recent_activity: comments,
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
        linked_tasks: vec![],
        kind: Some(lazybox_core::TaskKind::Issue),
        priority: None,
        state_label: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gql_error(message: &str, error_type: Option<&str>, extensions: Option<&str>) -> GqlError {
        GqlError {
            message: message.to_string(),
            error_type: error_type.map(str::to_string),
            path: None,
            extensions: extensions.map(|e| serde_json::from_str(e).unwrap()),
            locations: None,
        }
    }

    #[test]
    fn rate_limited_detects_type_and_message() {
        assert!(gql_error("boom", Some("RATE_LIMITED"), None).is_rate_limited());
        assert!(
            gql_error(
                "You have exceeded a secondary rate limit. Please wait a few minutes.",
                None,
                None,
            )
            .is_rate_limited()
        );
        assert!(gql_error("API rate limit exceeded for user", None, None).is_rate_limited());
        assert!(
            !gql_error("Pull request is not mergeable", Some("UNPROCESSABLE"), None)
                .is_rate_limited()
        );
    }

    #[test]
    fn human_strips_extensions_and_path() {
        // The exact shape from #804: a rate-limit error whose extensions
        // carry `typeName`. `full()` dumps that JSON; `human()` must not.
        let err = gql_error(
            "You have exceeded a secondary rate limit.",
            Some("RATE_LIMITED"),
            Some(r#"{"field":"rateLimit","typeName":"Mutation"}"#),
        );
        assert_eq!(err.human(), "You have exceeded a secondary rate limit.");
        assert!(!err.human().contains("typeName"));
        assert!(!err.human().contains('{'));
        // `full()` keeps the diagnostic detail for the log file.
        assert!(err.full().contains("typeName"));
    }

    #[test]
    fn test_extract_repo_from_url() {
        assert_eq!(
            extract_repo_from_url("https://github.com/owner/repo/pull/123"),
            "owner/repo"
        );
        assert_eq!(
            extract_repo_from_url("https://github.com/my-org/my-repo/pull/1"),
            "my-org/my-repo"
        );
    }

    #[test]
    fn test_extract_repo_malformed() {
        assert_eq!(extract_repo_from_url("not-a-url"), "unknown/unknown");
        assert_eq!(extract_repo_from_url(""), "unknown/unknown");
    }

    #[test]
    fn close_issue_body_targets_the_node_as_not_planned() {
        // Issue #270: "delete issue" resolves to a NOT_PLANNED close.
        // Pin the mutation + variable so a stray edit to either can't
        // silently retarget or change the close reason.
        let body = close_issue_body("I_kwDOabc123");
        assert_eq!(body["variables"]["id"], "I_kwDOabc123");
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("closeIssue"));
        assert!(query.contains("stateReason: NOT_PLANNED"));
    }

    #[test]
    fn close_pr_body_targets_the_node() {
        let body = close_pr_body("PR_kwDOabc123");
        assert_eq!(body["variables"]["id"], "PR_kwDOabc123");
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("closePullRequest"));
    }

    /// Issue #822: `rateLimit` lives on the `Query` root, never on
    /// `Mutation`. Selecting it inside a `mutation { … }` document makes
    /// GitHub reject the whole thing at schema validation
    /// (`undefinedField`), so every write fails before it runs. Guard
    /// every mutation constant against a regression that re-adds it.
    #[test]
    fn no_mutation_selects_ratelimit() {
        let mutations: &[(&str, &str)] = &[
            ("UPDATE_BRANCH_MUTATION", UPDATE_BRANCH_MUTATION),
            ("MERGE_PR_MUTATION", MERGE_PR_MUTATION),
            ("REQUEST_REVIEWS_MUTATION", REQUEST_REVIEWS_MUTATION),
            ("ADD_ASSIGNEES_MUTATION", ADD_ASSIGNEES_MUTATION),
            ("REMOVE_ASSIGNEES_MUTATION", REMOVE_ASSIGNEES_MUTATION),
            ("CLOSE_ISSUE_MUTATION", CLOSE_ISSUE_MUTATION),
            ("CLOSE_PR_MUTATION", CLOSE_PR_MUTATION),
            ("DELETE_ISSUE_MUTATION", DELETE_ISSUE_MUTATION),
            ("ADD_REACTION_MUTATION", ADD_REACTION_MUTATION),
            ("ADD_LABELS_MUTATION", ADD_LABELS_MUTATION),
            ("REMOVE_LABELS_MUTATION", REMOVE_LABELS_MUTATION),
        ];
        for (name, doc) in mutations {
            assert!(
                doc.trim_start().starts_with("mutation"),
                "{name} is not a mutation document"
            );
            assert!(
                !doc.contains("rateLimit"),
                "{name} selects `rateLimit`, which is a Query-root field — \
                 GitHub rejects the mutation as an undefinedField (issue #822)"
            );
        }
    }

    /// The merge mutation must carry `expectedHeadOid` — GitHub's
    /// compare-and-swap guard against a force-push landing between
    /// "observed green at OID X" and the merge itself. `Some` pins the
    /// OID; `None` serializes as JSON null (the nullable input field's
    /// "skip the guard" value), preserving the unguarded manual path.
    #[test]
    fn merge_pr_body_pins_expected_head_oid() {
        let body = merge_pr_body("PR_kwDO", "SQUASH", Some("abc123"));
        assert_eq!(body["variables"]["expectedHeadOid"], "abc123");
        let query = body["query"].as_str().unwrap();
        assert!(
            query.contains("expectedHeadOid: $expectedHeadOid"),
            "the mutation input must wire the variable through"
        );

        let unguarded = merge_pr_body("PR_kwDO", "SQUASH", None);
        assert!(
            unguarded["variables"]["expectedHeadOid"].is_null(),
            "no known head must serialize as null, not be omitted"
        );
    }

    /// The targeted single-PR fetch is the auto-merge path's source of
    /// the verified head — it must keep selecting `headRefOid`.
    #[test]
    fn single_pr_query_selects_head_ref_oid() {
        let body = single_pr_body("o", "r", 7);
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("headRefOid"));
        assert!(query.contains("reviewThreads(first: 50)"));
    }

    #[test]
    fn hot_tasks_body_batches_full_detail_nodes() {
        let ids = vec!["PR_one".to_string(), "I_two".to_string()];
        let body = hot_tasks_body(&ids);
        assert_eq!(body["variables"]["ids"], serde_json::json!(ids));
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("nodes(ids: $ids)"));
        assert!(query.contains("reviewThreads(first: 50)"));
        assert!(query.contains("contexts(first: 20)"));
    }

    #[test]
    fn delete_issue_body_targets_the_node() {
        let body = delete_issue_body("I_kwDOabc123");
        assert_eq!(body["variables"]["id"], "I_kwDOabc123");
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("deleteIssue"));
    }

    #[test]
    fn test_build_query() {
        let mut quals = default_search_qualifiers();
        quals.push("involves:alice".into());
        let q = build_query(&quals);
        assert!(q.contains("involves:alice"));
        assert!(q.contains("is:open"));
        assert!(q.contains("is:pr"));
    }

    #[test]
    fn test_build_query_with_filters() {
        let mut quals = default_search_qualifiers();
        quals.push("involves:bob".into());
        quals.push("org:myorg".into());
        let q = build_query(&quals);
        assert!(q.contains("org:myorg"));
        assert!(q.contains("involves:bob"));
    }

    #[test]
    fn updated_since_qualifier_emits_utc_timestamp() {
        let since = DateTime::parse_from_rfc3339("2026-06-05T09:30:00-04:00")
            .unwrap()
            .with_timezone(&Utc);
        // Offset normalized to UTC, RFC3339 with explicit `+00:00` —
        // the form GitHub's search documents for `updated:>=`.
        assert_eq!(
            updated_since_qualifier(since),
            "updated:>=2026-06-05T13:30:00+00:00"
        );
    }

    #[test]
    fn updated_since_qualifier_joins_into_search() {
        let mut quals = default_search_qualifiers();
        quals.push("involves:carol".into());
        quals.push(updated_since_qualifier(
            DateTime::parse_from_rfc3339("2026-06-05T13:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ));
        let q = build_query(&quals);
        assert!(q.contains("involves:carol"));
        assert!(q.contains("is:open"));
        assert!(q.contains("updated:>=2026-06-05T13:30:00+00:00"));
    }

    #[test]
    fn merged_sweep_query_has_merge_floor_and_involves() {
        let q = merged_sweep_query("dave", &[], "2026-06-01", None);
        assert!(q.contains("is:pr"));
        assert!(q.contains("is:merged"));
        assert!(q.contains("archived:false"));
        assert!(q.contains("merged:>=2026-06-01"));
        assert!(q.contains("involves:dave"));
    }

    #[test]
    fn merged_sweep_query_windows_on_since() {
        let since = DateTime::parse_from_rfc3339("2026-06-05T13:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let q = merged_sweep_query("dave", &[], "2026-06-01", Some(since));
        // Both bounds present: the 7-day merge floor AND the incremental
        // update window, so a steady sweep only pulls changed merges.
        assert!(q.contains("merged:>=2026-06-01"));
        assert!(q.contains("updated:>=2026-06-05T13:30:00+00:00"));
    }

    #[test]
    fn merged_sweep_query_reconcile_omits_update_window() {
        let q = merged_sweep_query("dave", &[], "2026-06-01", None);
        assert!(!q.contains("updated:>="));
    }

    #[test]
    fn merged_sweep_query_uses_pr_filters_over_involves() {
        let filters = vec!["author:erin".to_string(), "org:acme".to_string()];
        let q = merged_sweep_query("dave", &filters, "2026-06-01", None);
        assert!(q.contains("author:erin"));
        assert!(q.contains("org:acme"));
        assert!(!q.contains("involves:dave"));
    }

    // ── Issues ────────────────────────────────────────────────────────

    #[test]
    fn issues_query_filters_to_issues() {
        let mut quals = default_issues_qualifiers();
        quals.push("involves:alice".into());
        let q = build_issues_query(&quals);
        assert!(q.contains("is:open"));
        assert!(q.contains("is:issue"));
        assert!(!q.contains("is:pr"));
        assert!(q.contains("involves:alice"));
    }

    fn make_issue(number: u64, title: &str, author: Option<&str>, assignees: &[&str]) -> GqlIssue {
        GqlIssue {
            id: Some(format!("I_{number}")),
            number,
            title: title.into(),
            body: None,
            url: format!("https://github.com/o/r/issues/{number}"),
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            state: "OPEN".into(),
            author: author.map(|login| GqlAuthor {
                login: login.into(),
            }),
            labels: GqlLabels { nodes: vec![] },
            assignees: GqlAssignees {
                nodes: assignees
                    .iter()
                    .map(|l| GqlAuthor { login: (*l).into() })
                    .collect(),
            },
            comments: GqlComments {
                nodes: vec![],
                total_count: None,
            },
            repository: Some(GqlIssueRepo {
                name_with_owner: "o/r".into(),
            }),
            reactions: None,
        }
    }

    #[test]
    fn issue_to_task_author_role() {
        let issue = make_issue(1, "something", Some("alice"), &[]);
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.role, TaskRole::Author);
        assert_eq!(task.state, TaskState::Open);
        assert_eq!(task.branch, None, "issues have no branch");
        assert_eq!(task.ci, CiStatus::None);
        assert_eq!(task.review, ReviewStatus::None);
        assert_eq!(task.id.key, "o/r#1");
        assert_eq!(task.id.source, "github");
    }

    #[test]
    fn issue_to_task_maps_author() {
        let issue = make_issue(1, "t", Some("alice"), &[]);
        let task = issue_to_task(&issue, "bob");
        assert_eq!(task.author, "alice");
    }

    #[test]
    fn issue_to_task_author_empty_when_absent() {
        let issue = make_issue(1, "t", None, &[]);
        let task = issue_to_task(&issue, "bob");
        assert_eq!(task.author, "");
    }

    #[test]
    fn pr_to_task_maps_author() {
        let pr = make_pr(1, "carol");
        let task = pr_to_task(&pr, "bob");
        assert_eq!(task.author, "carol");
    }

    #[test]
    fn issue_to_task_assignee_role() {
        let issue = make_issue(2, "t", Some("someone-else"), &["alice"]);
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.role, TaskRole::Assignee);
        assert!(task.assignees.contains(&"alice".to_string()));
    }

    #[test]
    fn issue_to_task_mentioned_role_when_neither() {
        let issue = make_issue(3, "t", Some("other"), &["another"]);
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.role, TaskRole::Mentioned);
    }

    #[test]
    fn issue_to_task_closed_state() {
        let mut issue = make_issue(4, "done", Some("alice"), &[]);
        issue.state = "CLOSED".into();
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.state, TaskState::Closed);
    }

    #[test]
    fn issue_to_task_uses_repository_field_when_present() {
        let mut issue = make_issue(5, "t", None, &[]);
        issue.repository = Some(GqlIssueRepo {
            name_with_owner: "other-org/other-repo".into(),
        });
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.repo.as_deref(), Some("other-org/other-repo"));
    }

    #[test]
    fn issue_to_task_falls_back_to_url_parsing() {
        let mut issue = make_issue(6, "t", None, &[]);
        issue.repository = None;
        issue.url = "https://github.com/owner/repo/issues/6".into();
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.repo.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn issue_to_task_ingests_comments_as_activities() {
        let mut issue = make_issue(7, "t", Some("alice"), &[]);
        issue.comments = GqlComments {
            nodes: vec![
                GqlComment {
                    id: Some("c1".into()),
                    author: Some(GqlAuthor {
                        login: "bob".into(),
                    }),
                    body: "first".into(),
                    created_at: chrono::Utc::now(),
                    path: None,
                    line: None,
                    original_line: None,
                    diff_hunk: None,
                    reactions: None,
                },
                GqlComment {
                    id: Some("c2".into()),
                    author: Some(GqlAuthor {
                        login: "alice".into(),
                    }),
                    body: "reply".into(),
                    created_at: chrono::Utc::now(),
                    path: None,
                    line: None,
                    original_line: None,
                    diff_hunk: None,
                    reactions: None,
                },
            ],
            total_count: None,
        };
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.recent_activity.len(), 2);
        assert_eq!(task.recent_activity[0].author, "bob");
        assert_eq!(task.recent_activity[0].kind, ActivityKind::Comment);
        // Last comment is me — so no reply needed.
        assert!(!task.needs_reply);
    }

    #[test]
    fn issue_to_task_needs_reply_when_last_comment_is_from_other() {
        let mut issue = make_issue(8, "t", Some("alice"), &[]);
        issue.comments = GqlComments {
            nodes: vec![GqlComment {
                id: Some("c1".into()),
                author: Some(GqlAuthor {
                    login: "bob".into(),
                }),
                body: "question".into(),
                created_at: chrono::Utc::now(),
                path: None,
                line: None,
                original_line: None,
                diff_hunk: None,
                reactions: None,
            }],
            total_count: None,
        };
        let task = issue_to_task(&issue, "alice");
        assert!(task.needs_reply);
        assert_eq!(task.last_commenter.as_deref(), Some("bob"));
    }

    fn make_pr(number: u64, author: &str) -> GqlPr {
        GqlPr {
            id: Some(format!("PR_{number}")),
            number,
            title: "test".into(),
            body: None,
            url: format!("https://github.com/o/r/pull/{number}"),
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            is_draft: false,
            state: "OPEN".into(),
            merged: false,
            additions: 0,
            deletions: 0,
            head_ref_name: "feature".into(),
            head_ref_oid: None,
            base_ref_name: "main".into(),
            mergeable: None,
            merge_state_status: None,
            review_decision: None,
            auto_merge_request: None,
            is_in_merge_queue: false,
            author: Some(GqlAuthor {
                login: author.into(),
            }),
            labels: GqlLabels { nodes: vec![] },
            assignees: GqlAssignees { nodes: vec![] },
            review_requests: GqlReviewRequests { nodes: vec![] },
            comments: GqlComments {
                nodes: vec![],
                total_count: None,
            },
            reviews: GqlReviews { nodes: vec![] },
            latest_opinionated_reviews: GqlLatestReviews { nodes: vec![] },
            latest_reviews: GqlLatestReviews { nodes: vec![] },
            review_threads: GqlReviewThreads { nodes: vec![] },
            commits: GqlCommits { nodes: vec![] },
            closing_issues_references: None,
        }
    }

    fn ts(secs_offset: i64) -> DateTime<Utc> {
        chrono::Utc::now() + chrono::Duration::seconds(secs_offset)
    }

    fn verdict(login: &str, state: &str) -> GqlReviewerVerdict {
        GqlReviewerVerdict {
            author: Some(GqlAuthor {
                login: login.into(),
            }),
            state: state.into(),
        }
    }

    /// Regression: a user who reviewed-but-didn't-comment was falsely
    /// flagged as owing a reply because `my_latest` only looked at the
    /// comment timestamp. Their review should count as a response.
    #[test]
    fn needs_reply_false_when_my_review_supersedes_others_review() {
        let mut pr = make_pr(1, "bob");
        pr.reviews = GqlReviews {
            nodes: vec![
                GqlReview {
                    author: Some(GqlAuthor {
                        login: "bob".into(),
                    }),
                    body: Some("please review".into()),
                    state: "COMMENTED".into(),
                    submitted_at: Some(ts(-100)),
                    id: None,
                    created_at: None,
                },
                GqlReview {
                    author: Some(GqlAuthor {
                        login: "alice".into(),
                    }),
                    body: Some("looks good".into()),
                    state: "APPROVED".into(),
                    submitted_at: Some(ts(-10)),
                    id: None,
                    created_at: None,
                },
            ],
        };
        assert!(
            !needs_reply_check(&pr, "alice"),
            "alice reviewed after bob — should NOT need reply even though she never commented"
        );
    }

    /// Inverse: if their review is newer than my review AND I never
    /// commented, I do owe a reply.
    #[test]
    fn needs_reply_true_when_others_review_supersedes_mine() {
        let mut pr = make_pr(2, "bob");
        pr.reviews = GqlReviews {
            nodes: vec![
                GqlReview {
                    author: Some(GqlAuthor {
                        login: "alice".into(),
                    }),
                    body: Some("approve".into()),
                    state: "APPROVED".into(),
                    submitted_at: Some(ts(-100)),
                    id: None,
                    created_at: None,
                },
                GqlReview {
                    author: Some(GqlAuthor {
                        login: "bob".into(),
                    }),
                    body: Some("nit found".into()),
                    state: "CHANGES_REQUESTED".into(),
                    submitted_at: Some(ts(-10)),
                    id: None,
                    created_at: None,
                },
            ],
        };
        assert!(
            needs_reply_check(&pr, "alice"),
            "bob reviewed after alice — alice owes a reply"
        );
    }

    #[test]
    fn issues_query_body_omits_after_when_none() {
        let body = issues_query_body("test", None);
        let vars = &body["variables"];
        assert!(vars.get("after").is_none(), "after must be omitted");
        assert_eq!(vars["first"], 100);
        assert_eq!(vars["query"], "test");
    }

    #[test]
    fn issues_query_body_includes_after_when_set() {
        let body = issues_query_body("test", Some("cursor-abc"));
        assert_eq!(body["variables"]["after"], "cursor-abc");
    }

    #[test]
    fn parse_linear_identifiers_reads_branch_and_title_shapes() {
        // Linear branch shape `<user>/<team>-<n>-<slug>`.
        assert_eq!(
            parse_linear_identifiers("antoine/eng-123-fix-the-thing"),
            vec!["ENG-123".to_string()],
        );
        // Magic-word title reference.
        assert_eq!(
            parse_linear_identifiers("Fixes ENG-7: crash on boot"),
            vec!["ENG-7".to_string()],
        );
        // Multi-letter team, deduped.
        assert_eq!(
            parse_linear_identifiers("PLAT-9 and PLAT-9 again"),
            vec!["PLAT-9".to_string()],
        );
    }

    #[test]
    fn parse_linear_identifiers_respects_boundaries() {
        // A digit-led team (`v2-3`) and a trailing alphanumeric
        // (`ENG-1x`) must NOT match — the team is letters only and the
        // number must end at a non-alphanumeric.
        assert!(parse_linear_identifiers("v2-3").is_empty());
        assert!(parse_linear_identifiers("ENG-1x").is_empty());
        // A bare number with no team is not an identifier.
        assert!(parse_linear_identifiers("-42").is_empty());
    }

    #[test]
    fn pr_to_task_links_linear_ticket_from_branch() {
        // #922: the Linear identifier in the PR branch becomes a
        // cross-provider link on the PR task.
        let mut pr = make_pr(5, "alice");
        pr.head_ref_name = "alice/eng-123-fix".into();
        pr.title = "feat: fix the thing".into();
        pr.body = None;
        let task = pr_to_task(&pr, "alice");
        assert_eq!(
            task.linked_tasks,
            vec![TaskId {
                source: "linear".into(),
                key: "ENG-123".into(),
            }]
        );
    }

    #[test]
    fn pr_to_task_has_no_linear_link_without_identifier() {
        let mut pr = make_pr(5, "alice");
        pr.head_ref_name = "alice/plain-branch".into();
        pr.title = "feat: nothing linear here".into();
        pr.body = Some("just a normal PR".into());
        assert!(pr_to_task(&pr, "alice").linked_tasks.is_empty());
    }

    #[test]
    fn pr_to_task_ignores_linear_lookalikes_in_body() {
        // REGRESSION (#922): `<TEAM>-<n>` collides with ordinary technical
        // prose. The PR body must NOT be scanned — otherwise a body
        // mentioning `SHA-256` / `UTF-8` mints bogus `{linear, SHA-256}`
        // links and can silently collapse an unrelated ticket whose
        // identifier coincidentally matches. Branch + title are clean here,
        // so the only possible source of a link is the body.
        let mut pr = make_pr(5, "alice");
        pr.head_ref_name = "alice/plain-branch".into();
        pr.title = "feat: hashing overhaul".into();
        pr.body = Some("Switch to SHA-256 and UTF-8; see CVE-2021-1234.".into());
        assert!(
            pr_to_task(&pr, "alice").linked_tasks.is_empty(),
            "body-only lookalike tokens must not produce cross-provider links",
        );
    }

    #[test]
    fn pr_to_task_ignores_real_linear_ref_confined_to_body() {
        // The flip side of the safety trade-off: a genuine `ENG-123`
        // appearing ONLY in the body is deliberately not matched here (the
        // body is unscannable). Such a reference is instead covered by
        // Linear's authoritative attachment signal. Pinning this documents
        // the intentional boundary so a future "just also scan the body"
        // change has to reckon with the false-positive test above.
        let mut pr = make_pr(5, "alice");
        pr.head_ref_name = "alice/plain-branch".into();
        pr.title = "feat: nothing linear in the title".into();
        pr.body = Some("Fixes ENG-123.".into());
        assert!(pr_to_task(&pr, "alice").linked_tasks.is_empty());
    }

    #[test]
    fn closes_issues_ignores_body_text_keyword() {
        // GitHub's `#N` syntax is shared by issues AND PRs, and the
        // body alone can't tell them apart. We used to body-text-
        // parse "Closes #N" as a fallback, but it caused PR rows to
        // be absorbed into other PRs that referenced them by number.
        // Now we trust `closingIssuesReferences` exclusively — GitHub
        // only populates it for actual issues.
        let mut pr = make_pr(166, "alice");
        pr.body = Some("Closes #73.\n\nSummary: ...".into());
        pr.url = "https://github.com/acme/widget/pull/166".into();
        pr.closing_issues_references = None;
        let task = pr_to_task(&pr, "alice");
        assert!(
            task.closes_issues.is_empty(),
            "body-text `Closes #N` must NOT populate closes_issues — \
             only GraphQL's closingIssuesReferences is authoritative. \
             Got: {:?}",
            task.closes_issues,
        );
    }

    #[test]
    fn closes_issues_reads_from_graphql_field() {
        // The supported path: GraphQL says this PR closes #73, we
        // store the corresponding TaskId.
        let mut pr = make_pr(200, "alice");
        pr.url = "https://github.com/o/r/pull/200".into();
        pr.closing_issues_references = Some(GqlClosingIssues {
            nodes: vec![GqlClosingIssue {
                number: 73,
                repository: GqlIssueRepo {
                    name_with_owner: "o/r".into(),
                },
            }],
        });
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.closes_issues.len(), 1);
        assert_eq!(task.closes_issues[0].key, "o/r#73");
    }

    #[test]
    fn closes_issues_dedupes_graphql_repeats() {
        // Defensive: if GraphQL ever returned the same ref twice
        // (it shouldn't), we keep one copy.
        let mut pr = make_pr(300, "alice");
        pr.url = "https://github.com/o/r/pull/300".into();
        pr.closing_issues_references = Some(GqlClosingIssues {
            nodes: vec![
                GqlClosingIssue {
                    number: 42,
                    repository: GqlIssueRepo {
                        name_with_owner: "o/r".into(),
                    },
                },
                GqlClosingIssue {
                    number: 42,
                    repository: GqlIssueRepo {
                        name_with_owner: "o/r".into(),
                    },
                },
            ],
        });
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.closes_issues.len(), 1);
    }

    // ── Title-based fallback for closes/fixes/resolves ─────────────

    /// Bare title parser — happy paths.
    #[test]
    fn parse_closes_from_title_basic_forms() {
        assert_eq!(parse_closes_from_title("foo (closes #31)"), vec![31]);
        assert_eq!(parse_closes_from_title("fixes #42"), vec![42]);
        assert_eq!(parse_closes_from_title("Resolves: #7"), vec![7]);
        assert_eq!(
            parse_closes_from_title("feat: thing (closes #5, fixes #6)"),
            vec![5, 6]
        );
    }

    /// Case-insensitive.
    #[test]
    fn parse_closes_from_title_case_insensitive() {
        assert_eq!(parse_closes_from_title("CLOSES #1"), vec![1]);
        assert_eq!(parse_closes_from_title("Closing #2"), vec![2]);
        assert_eq!(parse_closes_from_title("FIX #3"), vec![3]);
    }

    /// Word boundaries — `enclosed` / `prefixing` must not match.
    #[test]
    fn parse_closes_from_title_respects_word_boundaries() {
        assert_eq!(parse_closes_from_title("enclosed #5"), Vec::<u64>::new());
        assert_eq!(parse_closes_from_title("prefixed #6"), Vec::<u64>::new());
        assert_eq!(parse_closes_from_title("closer #7"), Vec::<u64>::new());
    }

    /// No `#` → no match (don't grab plain numbers in titles).
    #[test]
    fn parse_closes_from_title_requires_hash() {
        assert_eq!(parse_closes_from_title("fixes 12"), Vec::<u64>::new());
    }

    /// Dedupe + preserve title order.
    #[test]
    fn parse_closes_from_title_dedupes_in_order() {
        assert_eq!(
            parse_closes_from_title("closes #1 and also fixes #1, closes #2"),
            vec![1, 2]
        );
    }

    /// Real PR title from the user's screenshot.
    #[test]
    fn parse_closes_from_title_user_example() {
        let title = "Add dynamic credential source for per-request secrets (closes #31)";
        assert_eq!(parse_closes_from_title(title), vec![31]);
    }

    /// Empty closingIssuesReferences but title says "(closes #N)"
    /// → title fallback fires and the PR's `closes_issues` contains
    /// the issue. This is the bug the user reported.
    #[test]
    fn extract_closes_issues_falls_back_to_title_when_graphql_empty() {
        // `make_pr` builds URL `https://github.com/o/r/pull/N` so the
        // repo extracted from URL is `o/r`.
        let mut pr = make_pr(100, "alice");
        pr.title = "Add dynamic credential source for per-request secrets (closes #31)".into();
        pr.closing_issues_references = None; // GitHub didn't link it
        let task = pr_to_task(&pr, "alice");
        let keys: Vec<&str> = task.closes_issues.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["o/r#31"]);
    }

    /// When BOTH GraphQL and title yield issues, results are merged
    /// and deduped (no double-counting if title repeats a graphql ref).
    /// Title fallback uses the PR's own repo (`o/r` from `make_pr`'s
    /// URL); graphql refs carry their own `name_with_owner` (here
    /// the same repo, deliberately, so the dedupe path is exercised).
    #[test]
    fn extract_closes_issues_merges_and_dedupes_graphql_plus_title() {
        let mut pr = make_pr(100, "alice");
        pr.title = "feat: foo (closes #1, fixes #2)".into();
        pr.closing_issues_references = Some(GqlClosingIssues {
            nodes: vec![GqlClosingIssue {
                number: 1,
                repository: GqlIssueRepo {
                    name_with_owner: "o/r".into(),
                },
            }],
        });
        let task = pr_to_task(&pr, "alice");
        let keys: Vec<&str> = task.closes_issues.iter().map(|t| t.key.as_str()).collect();
        // #1 from graphql (deduped against title's #1) + #2 from title.
        assert_eq!(keys, vec!["o/r#1", "o/r#2"]);
    }

    /// A title can mix a closing keyword and a bare reference; both
    /// sources contribute and the result is deduped in title order.
    #[test]
    fn extract_closes_issues_combines_keyword_and_bare_title_refs() {
        let mut pr = make_pr(100, "alice");
        pr.title = "feat: thing (closes #1) (#2) and again (#1)".into();
        pr.closing_issues_references = None;
        let task = pr_to_task(&pr, "alice");
        let keys: Vec<&str> = task.closes_issues.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["o/r#1", "o/r#2"]);
    }

    // ── Bare `(#N)` title references (no closing keyword) ──────────

    /// The core form from the issue: `… (#98)` with no keyword.
    #[test]
    fn parse_bare_refs_from_title_basic() {
        assert_eq!(
            parse_bare_refs_from_title("Instrument the sync pipeline (#98)"),
            vec![98]
        );
        assert_eq!(parse_bare_refs_from_title("(#7)"), vec![7]);
    }

    /// Multiple references inside one group, comma- or space-separated.
    #[test]
    fn parse_bare_refs_from_title_multiple_in_group() {
        assert_eq!(parse_bare_refs_from_title("foo (#1, #2)"), vec![1, 2]);
        assert_eq!(parse_bare_refs_from_title("foo (#3 #4)"), vec![3, 4]);
    }

    /// Dedupe + title order preserved across groups.
    #[test]
    fn parse_bare_refs_from_title_dedupes_in_order() {
        assert_eq!(
            parse_bare_refs_from_title("a (#5) b (#2) c (#5)"),
            vec![5, 2]
        );
    }

    /// Prose parentheticals are NOT references — the group must hold
    /// only `#N` tokens.
    #[test]
    fn parse_bare_refs_from_title_ignores_prose() {
        assert_eq!(
            parse_bare_refs_from_title("Refactor (see #5 for spec)"),
            Vec::<u64>::new()
        );
        assert_eq!(
            parse_bare_refs_from_title("Speed up parser (now 3x faster)"),
            Vec::<u64>::new()
        );
    }

    /// A bare `#N` outside parentheses is left alone — only the
    /// parenthesized convention counts.
    #[test]
    fn parse_bare_refs_from_title_requires_parens() {
        assert_eq!(
            parse_bare_refs_from_title("Add foo for #98"),
            Vec::<u64>::new()
        );
    }

    /// `(closes #31)` is handled by the keyword parser, not this one —
    /// the keyword breaks the "only references" rule here.
    #[test]
    fn parse_bare_refs_from_title_skips_keyword_groups() {
        assert_eq!(
            parse_bare_refs_from_title("foo (closes #31)"),
            Vec::<u64>::new()
        );
    }

    /// Unbalanced / empty parens don't panic or match.
    #[test]
    fn parse_bare_refs_from_title_handles_degenerate_input() {
        assert_eq!(parse_bare_refs_from_title("foo (#5"), Vec::<u64>::new());
        assert_eq!(parse_bare_refs_from_title("foo ()"), Vec::<u64>::new());
        assert_eq!(parse_bare_refs_from_title("foo (#)"), Vec::<u64>::new());
    }

    /// End-to-end: the issue's repro. Title carries a bare `(#98)`,
    /// GitHub linked nothing → the bare-ref path populates
    /// `closes_issues` with the same-repo issue.
    #[test]
    fn extract_closes_issues_picks_up_bare_title_reference() {
        let mut pr = make_pr(112, "alice");
        pr.title = "Instrument the sync pipeline for latency debugging (#98)".into();
        pr.closing_issues_references = None;
        let task = pr_to_task(&pr, "alice");
        let keys: Vec<&str> = task.closes_issues.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["o/r#98"]);
    }

    // ── Query-shape regression guards ─────────────────────────────
    //
    // Connection sizes (`first: N`) drive GitHub's GraphQL cost.
    // Bumping them silently — say, "let's grab more comments" — is
    // how the rate limit gets blown again. These tests pin the
    // current numbers so any change has to be intentional + tested.
    //
    // If you legitimately need more, raise the number AND the
    // assertion together, AND verify a high-PR-count poll cycle
    // doesn't immediately re-trip RemoteLow.

    #[test]
    fn pr_query_omits_heavy_fields_entirely() {
        // The whole point of the lazy split — every field below
        // is exclusively the lazy `PR_DETAILS_QUERY`'s. Re-adding
        // any of them to the inbox scan would silently re-introduce
        // the rate-budget blowup that motivated the trim (see the
        // long comment on `SEARCH_QUERY`). `closingIssuesReferences`
        // is deliberately NOT in this list — it's a shallow,
        // authoritative field kept eager so issue→PR transfer fires
        // on the poll path (#559); see `search_query_fetches_closing_issue_refs`.
        for field in [
            "reviewThreads",
            "reviews(",
            "contexts(",
            "latestOpinionatedReviews",
            "latestReviews",
        ] {
            assert!(
                !SEARCH_QUERY.contains(field),
                "`{field}` must not appear in the inbox-scan query — see PR_DETAILS_QUERY",
            );
        }
        // Sanity-check the lazy query still has the back-fill fields.
        for field in [
            "reviewThreads(first: 50)",
            "closingIssuesReferences",
            "reviews(first: 10)",
            "latestOpinionatedReviews(first: 30)",
            "latestReviews(first: 30)",
            "contexts(first: 20)",
        ] {
            assert!(
                PR_DETAILS_QUERY.contains(field),
                "PR_DETAILS_QUERY is the new home for `{field}`",
            );
        }
    }

    #[test]
    fn pr_query_connection_sizes_are_pinned_low() {
        // Sidebar-rendering fields stay in the inbox query but
        // shrunk so 100 PRs/page × N fields doesn't compound.
        // First label / first few assignees / reviewers is all the
        // sidebar renders.
        assert!(
            SEARCH_QUERY.contains("labels(first: 10)"),
            "labels cap drifted",
        );
        assert!(
            SEARCH_QUERY.contains("assignees(first: 5)"),
            "assignees cap drifted",
        );
        assert!(
            SEARCH_QUERY.contains("reviewRequests(first: 5)"),
            "reviewRequests cap drifted",
        );
        // `comments(last: 1)` + `totalCount` replaces the eager
        // `comments(first: 15)`. We just need the last commenter
        // for the sidebar's "from $login" line and the count for
        // the unread proxy; the bodies live on the lazy path.
        assert!(
            SEARCH_QUERY.contains("comments(last: 1)"),
            "comments switched away from last:1 — sidebar would lose `last_commenter`",
        );
        assert!(
            SEARCH_QUERY.contains("totalCount"),
            "comments.totalCount disappeared — `unread_count` would collapse to 0 on inbox-scan",
        );
        assert!(
            SEARCH_QUERY.contains("issueCount"),
            "the governor needs the total result count to price every page",
        );
        // Hard guards: previous bloated numbers (both pre-trim AND
        // the intermediate #17 band-aid trim) must NOT come back.
        // The band-aid (`comments(first: 5)` etc.) was superseded
        // by the lazy-fetch split — any return to *any* eager
        // `comments(first: N)` or `reviews(first: N)` in the
        // inbox-scan query regresses the cost fix.
        for stale in [
            "comments(first: 5)",
            "comments(first: 15)",
            "comments(first: 30)",
            "reviews(first: 3)",
            "reviews(first: 10)",
            "reviews(first: 20)",
            "assignees(first: 10)",
            "reviewRequests(first: 10)",
            "contexts(first: 5)",
            "contexts(first: 20)",
        ] {
            assert!(
                !SEARCH_QUERY.contains(stale),
                "regressed back to bloated `{stale}` — keep the trim",
            );
        }
    }

    #[test]
    fn issues_query_connection_sizes_are_pinned_low() {
        assert!(ISSUES_QUERY.contains("issueCount"));
        assert_eq!(pr_page_count(0), 1);
        assert_eq!(pr_page_count(26), 2);
        assert_eq!(issue_page_count(101), 2);
        assert!(
            ISSUES_QUERY.contains("comments(first: 15)"),
            "issue comments cap drifted",
        );
        assert!(!ISSUES_QUERY.contains("comments(first: 30)"));
    }

    // ── PR details (lazy-fetch) ────────────────────────────────────

    #[test]
    fn pr_details_body_carries_node_id() {
        // Variable binding must reach GitHub or the query returns
        // `node: null` and the lazy-fetch ships zero activities for
        // every PR.
        let body = pr_details_body("PR_kwDOFOO");
        assert_eq!(body["variables"]["id"], "PR_kwDOFOO");
        assert!(
            body["query"].as_str().unwrap().contains("reviewThreads"),
            "lazy-fetch query must include reviewThreads — that's the whole point",
        );
    }

    #[test]
    fn pr_details_to_activities_preserves_eager_formatting() {
        // Drop-in equivalence: the lazy-fetch's activities must look
        // identical to the eager path's reviewThreads loop in
        // `pr_to_task`, so merging the two doesn't produce a visible
        // discontinuity in the activity feed.
        let when = DateTime::parse_from_rfc3339("2026-05-15T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let node = GqlPrDetailsNode {
            review_threads: GqlReviewThreads {
                nodes: vec![GqlReviewThread {
                    id: Some("T_thread1".into()),
                    is_resolved: false,
                    is_outdated: true,
                    path: Some("src/lib.rs".into()),
                    line: Some(42),
                    original_line: None,
                    comments: GqlComments {
                        nodes: vec![
                            GqlComment {
                                id: Some("C_first".into()),
                                author: Some(GqlAuthor {
                                    login: "alice".into(),
                                }),
                                body: "needs nil check".into(),
                                created_at: when,
                                path: Some("src/lib.rs".into()),
                                line: Some(42),
                                original_line: None,
                                diff_hunk: Some("@@ -1 +1 @@".into()),
                                reactions: None,
                            },
                            GqlComment {
                                id: Some("C_reply".into()),
                                author: Some(GqlAuthor {
                                    login: "bob".into(),
                                }),
                                body: "good catch".into(),
                                created_at: when,
                                path: None,
                                line: None,
                                original_line: None,
                                diff_hunk: None,
                                reactions: None,
                            },
                        ],
                        total_count: None,
                    },
                }],
            },
            ..Default::default()
        };
        let acts = pr_details_to_activities(&node);
        assert_eq!(acts.len(), 2, "two thread comments → two activities");

        // First comment: outdated marker, has the diff hunk.
        assert_eq!(acts[0].author, "alice");
        assert!(acts[0].body.starts_with("📌 outdated:"), "{}", acts[0].body);
        assert_eq!(acts[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(acts[0].line, Some(42));
        assert_eq!(acts[0].diff_hunk.as_deref(), Some("@@ -1 +1 @@"));
        assert_eq!(acts[0].thread_id.as_deref(), Some("T_thread1"));
        assert_eq!(acts[0].kind, ActivityKind::Review);

        // Reply: ↳ prefix wraps the thread-status prefix, no diff
        // hunk, inherits path+line from thread.
        assert_eq!(acts[1].author, "bob");
        assert!(
            acts[1].body.starts_with("↳ 📌 outdated:"),
            "reply prefix is outermost, then thread status: {}",
            acts[1].body,
        );
        assert_eq!(acts[1].path.as_deref(), Some("src/lib.rs"));
        assert!(
            acts[1].diff_hunk.is_none(),
            "replies don't carry diff hunks"
        );
        assert_eq!(acts[1].thread_id.as_deref(), Some("T_thread1"));
    }

    #[test]
    fn pr_details_to_activities_empty_threads_is_empty() {
        let node = GqlPrDetailsNode {
            review_threads: GqlReviewThreads { nodes: vec![] },
            ..Default::default()
        };
        assert!(pr_details_to_activities(&node).is_empty());
    }

    #[test]
    fn pr_details_to_activities_skips_blank_body_comments() {
        // Github lets users push empty review-thread comments (or
        // strip-whitespace ones); we drop those rather than render
        // an empty row.
        let when = DateTime::parse_from_rfc3339("2026-05-15T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let node = GqlPrDetailsNode {
            review_threads: GqlReviewThreads {
                nodes: vec![GqlReviewThread {
                    id: Some("T_thread1".into()),
                    is_resolved: false,
                    is_outdated: false,
                    path: None,
                    line: None,
                    original_line: None,
                    comments: GqlComments {
                        nodes: vec![GqlComment {
                            id: None,
                            author: None,
                            body: "   \n  ".into(),
                            created_at: when,
                            path: None,
                            line: None,
                            original_line: None,
                            diff_hunk: None,
                            reactions: None,
                        }],
                        total_count: None,
                    },
                }],
            },
            ..Default::default()
        };
        assert!(pr_details_to_activities(&node).is_empty());
    }

    // ── Inbox-scan trim shape ──────────────────────────────────────

    /// `unread_count` on inbox-scan rides on `comments.totalCount`
    /// (the bodies are gone from `SEARCH_QUERY`). Verify the wire
    /// reads it back correctly and that the count survives even
    /// when the activity list is empty.
    #[test]
    fn pr_to_task_uses_total_count_for_unread() {
        let mut pr = make_pr(1, "bob");
        pr.comments = GqlComments {
            nodes: vec![],
            total_count: Some(17),
        };
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.unread_count, 17);
        assert!(task.recent_activity.is_empty());
    }

    /// The PR author login is carried onto the Task so merge-on-green's
    /// author allowlist can opt a bot in (issue #845).
    #[test]
    fn pr_to_task_carries_the_author_login() {
        let pr = make_pr(1, "dependabot[bot]");
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.author, "dependabot[bot]");
    }

    /// A realistic inbox-scan (`SEARCH_QUERY`) wire response, built
    /// to mirror exactly what GitHub returns for the *trimmed* query:
    /// the `comments(last: 1)` selection carries only `author` +
    /// `createdAt` — NO `body`, NO `id` (those moved to the lazy
    /// `PR_DETAILS_QUERY`). This is the fixture that guards against
    /// the `3c019af` regression where every PR with at least one
    /// comment failed to deserialize (`GqlComment.body` was a
    /// required `String`), failing the whole `Vec<GqlPr>` page and
    /// silently dropping ALL PRs from the `[all]` list while issues
    /// (a separate query that still selects `body`) rendered fine.
    ///
    /// If a future query trim drops another field whose struct
    /// counterpart isn't `Option`/`#[serde(default)]`, this test
    /// fails instead of the PR side silently disappearing in prod.
    fn search_query_wire_response() -> &'static str {
        r#"{
          "data": {
            "search": {
              "pageInfo": { "hasNextPage": false, "endCursor": null },
              "nodes": [
                {
                  "id": "PR_kwDOabc123",
                  "number": 42,
                  "title": "Add foo to bar",
                  "body": "Closes #1.",
                  "url": "https://github.com/owner/repo/pull/42",
                  "updatedAt": "2026-05-28T12:00:00Z",
                  "createdAt": "2026-05-27T09:00:00Z",
                  "isDraft": false,
                  "state": "OPEN",
                  "merged": false,
                  "additions": 10,
                  "deletions": 2,
                  "headRefName": "feature/foo",
                  "baseRefName": "main",
                  "mergeable": "MERGEABLE",
                  "mergeStateStatus": "CLEAN",
                  "reviewDecision": "REVIEW_REQUIRED",
                  "autoMergeRequest": null,
                  "isInMergeQueue": false,
                  "author": { "login": "carol" },
                  "commits": {
                    "nodes": [
                      { "commit": { "statusCheckRollup": { "state": "SUCCESS" } } }
                    ]
                  },
                  "labels": { "nodes": [ { "name": "bug", "color": "d73a4a" } ] },
                  "assignees": { "nodes": [ { "login": "dave" } ] },
                  "reviewRequests": {
                    "nodes": [
                      { "requestedReviewer": { "login": "alice" } }
                    ]
                  },
                  "comments": {
                    "totalCount": 3,
                    "nodes": [
                      { "author": { "login": "carol" }, "createdAt": "2026-05-28T11:00:00Z" }
                    ]
                  },
                  "closingIssuesReferences": {
                    "nodes": [
                      { "number": 1, "repository": { "nameWithOwner": "owner/repo" } }
                    ]
                  }
                }
              ]
            },
            "rateLimit": { "cost": 1, "limit": 5000, "remaining": 4999, "resetAt": "2026-05-28T13:00:00Z" }
          },
          "errors": null
        }"#
    }

    #[test]
    fn search_query_response_with_bodiless_comment_deserializes() {
        let resp: GqlResponse = serde_json::from_str(search_query_wire_response())
            .expect("trimmed SEARCH_QUERY response must deserialize even when a PR has comments");
        let data = resp.data.expect("data present");
        assert_eq!(
            data.search.nodes.len(),
            1,
            "the PR must survive deserialization — a bodiless comment must not drop the page",
        );
        let pr = &data.search.nodes[0];
        assert_eq!(pr.number, 42);
        // The comment node has no `body` — it must default to empty,
        // not abort the parse.
        assert_eq!(pr.comments.nodes.len(), 1);
        assert_eq!(pr.comments.nodes[0].body, "");
        assert_eq!(pr.comments.total_count, Some(3));
        // The `cost` field selected for sync profiling must ride
        // through onto the rate-limit block.
        assert_eq!(data.rate_limit.expect("rateLimit present").cost, Some(1));
    }

    #[test]
    fn search_query_response_converts_to_pr_task() {
        let resp: GqlResponse = serde_json::from_str(search_query_wire_response()).unwrap();
        let pr = &resp.data.unwrap().search.nodes[0];
        let task = pr_to_task(pr, "alice");
        assert_eq!(task.id.key, "owner/repo#42");
        assert_eq!(task.title, "Add foo to bar");
        assert_eq!(task.branch.as_deref(), Some("feature/foo"));
        assert_eq!(task.state, TaskState::Open);
        // `unread_count` rides on the server-side totalCount even
        // though the body (and thus the activity) isn't fetched.
        assert_eq!(task.unread_count, 3);
        // Last commenter still lights up from the bodiless comment's
        // author so the sidebar's "from $login" line works pre-lazy.
        assert_eq!(task.last_commenter.as_deref(), Some("carol"));
        // The authoritative closing-issue link rides through on the
        // poll path now (#559): the body `Closes #1.` that GitHub
        // resolved into `closingIssuesReferences` populates
        // `closes_issues` without any lazy fetch.
        let keys: Vec<&str> = task.closes_issues.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["owner/repo#1"]);
        // An open PR has no close moment.
        assert_eq!(task.closed_at, None);
    }

    /// Root fix for #559: the inbox-scan query must select the
    /// authoritative `closingIssuesReferences` so the PR↔issue link is
    /// known on every poll — not just after a lazy `PR_DETAILS` fetch.
    /// The counterpart guard (`pr_query_omits_heavy_fields_entirely`)
    /// keeps the genuinely-expensive fields out; this one keeps the
    /// cheap authoritative link in.
    #[test]
    fn search_query_fetches_closing_issue_refs() {
        assert!(
            SEARCH_QUERY.contains("closingIssuesReferences"),
            "SEARCH_QUERY must select closingIssuesReferences — issue→PR \
             session transfer depends on the link being known on the poll path (#559)",
        );
    }

    /// A quiet PR that links a **cross-repo** issue only through its
    /// body (GitHub resolves it into `closingIssuesReferences`; the
    /// title parsers can't, since they're same-repo only) transfers on
    /// the poll path once the field is fetched eagerly (#559). Driven
    /// from a `SEARCH_QUERY`-shaped wire response — deserialize plus
    /// conversion — so it fails if the field is dropped from the query
    /// OR stops deserializing, not just if `extract_closes_issues`
    /// regresses.
    #[test]
    fn search_query_transfers_cross_repo_body_link() {
        // Title carries no closing keyword and the linked issue is in a
        // different repo, so neither title parser (same-repo only) could
        // catch it — only the eager `closingIssuesReferences` does.
        let wire = r#"{
          "data": {
            "search": {
              "pageInfo": { "hasNextPage": false, "endCursor": null },
              "nodes": [
                {
                  "id": "PR_kwDOxrepo9",
                  "number": 9,
                  "title": "Add foo to bar",
                  "body": "See other-org/other-repo#7 for context.",
                  "url": "https://github.com/my-org/my-repo/pull/9",
                  "updatedAt": "2026-05-28T12:00:00Z",
                  "createdAt": "2026-05-27T09:00:00Z",
                  "isDraft": false,
                  "state": "OPEN",
                  "merged": false,
                  "additions": 3,
                  "deletions": 1,
                  "headRefName": "feature/foo",
                  "baseRefName": "main",
                  "mergeable": "MERGEABLE",
                  "mergeStateStatus": "CLEAN",
                  "reviewDecision": "REVIEW_REQUIRED",
                  "autoMergeRequest": null,
                  "isInMergeQueue": false,
                  "author": { "login": "carol" },
                  "commits": {
                    "nodes": [
                      { "commit": { "statusCheckRollup": { "state": "SUCCESS" } } }
                    ]
                  },
                  "labels": { "nodes": [] },
                  "assignees": { "nodes": [] },
                  "reviewRequests": { "nodes": [] },
                  "comments": { "totalCount": 0, "nodes": [] },
                  "closingIssuesReferences": {
                    "nodes": [
                      { "number": 7, "repository": { "nameWithOwner": "other-org/other-repo" } }
                    ]
                  }
                }
              ]
            },
            "rateLimit": { "cost": 1, "limit": 5000, "remaining": 4999, "resetAt": "2026-05-28T13:00:00Z" }
          },
          "errors": null
        }"#;
        let resp: GqlResponse = serde_json::from_str(wire).unwrap();
        let pr = &resp.data.unwrap().search.nodes[0];
        let task = pr_to_task(pr, "alice");
        let keys: Vec<&str> = task.closes_issues.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["other-org/other-repo#7"]);
    }

    /// `closedAt` rides from the wire onto `Task.closed_at`. The
    /// Inbox grace window keys off this stable timestamp (not the
    /// activity-bumped `updated_at`) so a long-merged PR leaves the
    /// inbox even while it still draws post-merge events — issue #96.
    #[test]
    fn pr_to_task_carries_closed_at() {
        let merged_at = chrono::DateTime::parse_from_rfc3339("2026-05-20T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut pr = make_pr(7, "bob");
        pr.merged = true;
        pr.state = "MERGED".into();
        pr.closed_at = Some(merged_at);
        // Activity touched the PR long after the merge.
        pr.updated_at = chrono::DateTime::parse_from_rfc3339("2026-05-28T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.state, TaskState::Merged);
        assert_eq!(task.closed_at, Some(merged_at));
    }

    /// Regression: GitHub never computes mergeability for merged PRs
    /// (their `mergeable` stays null forever), and the merged sweep
    /// re-surfaces them every tick. Classifying that null as `Unknown`
    /// armed the poll scheduler's 5s fast re-poll on essentially every
    /// tick — a permanent hot loop. Terminal PRs must land on a
    /// definitive value.
    #[test]
    fn merged_pr_with_null_mergeable_is_not_unknown() {
        let mut pr = make_pr(7, "bob");
        pr.merged = true;
        pr.state = "MERGED".into();
        pr.mergeable = None;
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.state, TaskState::Merged);
        assert_ne!(
            task.mergeable,
            lazybox_core::Mergeable::Unknown,
            "a merged PR must never re-arm the unknown-mergeable fast re-poll"
        );
    }

    /// Same for closed-without-merge PRs — and GitHub sometimes sends
    /// the literal "UNKNOWN" for them instead of null; both shapes
    /// must settle.
    #[test]
    fn closed_pr_with_unknown_mergeable_is_not_unknown() {
        let mut pr = make_pr(8, "bob");
        pr.state = "CLOSED".into();
        pr.mergeable = Some("UNKNOWN".into());
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.state, TaskState::Closed);
        assert_ne!(task.mergeable, lazybox_core::Mergeable::Unknown);
    }

    /// OPEN PRs keep the Unknown classification — that's the real
    /// "GitHub is still computing, re-query soon" signal.
    #[test]
    fn open_pr_with_null_mergeable_stays_unknown() {
        let mut pr = make_pr(9, "bob");
        pr.mergeable = None;
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.mergeable, lazybox_core::Mergeable::Unknown);
    }

    /// A definitive wire value always wins, even on a terminal PR —
    /// the terminal-state fallback only covers null/UNKNOWN.
    #[test]
    fn closed_pr_with_conflicting_keeps_conflicting() {
        let mut pr = make_pr(10, "bob");
        pr.state = "CLOSED".into();
        pr.mergeable = Some("CONFLICTING".into());
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.mergeable, lazybox_core::Mergeable::Conflicting);
    }

    /// `createdAt` rides from the wire onto `Task.created_at`, and
    /// `opened_at()` returns it even when `updated_at` is far more
    /// recent — the age signal the sidebar reads for the stale-issue
    /// fade (issue #274).
    #[test]
    fn issue_to_task_carries_created_at_as_age_anchor() {
        let opened = chrono::DateTime::parse_from_rfc3339("2026-01-02T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut issue = make_issue(9, "old issue", Some("bob"), &[]);
        issue.created_at = Some(opened);
        issue.updated_at = chrono::DateTime::parse_from_rfc3339("2026-05-28T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let task = issue_to_task(&issue, "alice");
        assert_eq!(task.created_at, Some(opened));
        assert_eq!(task.opened_at(), opened);
    }

    /// Last commenter falls through from `comments.nodes[0]` even
    /// when the activity list is empty (inbox-scan path with a
    /// non-self last commenter).
    #[test]
    fn pr_to_task_last_commenter_falls_back_to_comments_node() {
        let mut pr = make_pr(2, "bob");
        pr.comments = GqlComments {
            nodes: vec![GqlComment {
                id: None,
                author: Some(GqlAuthor {
                    login: "carol".into(),
                }),
                body: "".into(), // blank → not in activities
                created_at: chrono::Utc::now(),
                path: None,
                line: None,
                original_line: None,
                diff_hunk: None,
                reactions: None,
            }],
            total_count: Some(3),
        };
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.last_commenter.as_deref(), Some("carol"));
    }

    /// Regression (#800): a PR I'm only *involved* in — not the author,
    /// not a requested reviewer, not an assignee, no review submitted —
    /// must be `Mentioned`, NOT `Reviewer`. The inbox `involves:me`
    /// search returns every PR I've commented on; before the fix they
    /// all defaulted to `Reviewer`, so the "author OR reviewer" filter
    /// surfaced the entire inbox. `reviewDecision` alone (a PR-global
    /// signal, not about me) must not promote me to reviewer.
    #[test]
    fn pr_to_task_involved_but_not_requested_is_mentioned() {
        for decision in [None, Some("REVIEW_REQUIRED"), Some("APPROVED")] {
            let mut pr = make_pr(4, "bob");
            pr.review_decision = decision.map(Into::into);
            pr.reviews = GqlReviews::default(); // inbox-scan: no reviews
            // No review request for alice, no assignment.
            let task = pr_to_task(&pr, "alice");
            assert_eq!(
                task.role,
                TaskRole::Mentioned,
                "involved-only PR (reviewDecision={decision:?}) must be Mentioned, not Reviewer",
            );
        }
    }

    /// A standing review request for me → `Reviewer`, even on the
    /// inbox-scan path where the reviews list is empty.
    #[test]
    fn pr_to_task_role_is_reviewer_when_review_requested() {
        let mut pr = make_pr(4, "bob");
        pr.reviews = GqlReviews::default();
        pr.review_requests = GqlReviewRequests {
            nodes: vec![GqlReviewRequest {
                requested_reviewer: Some(GqlRequestedReviewer::User {
                    login: "alice".into(),
                }),
            }],
        };
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.role, TaskRole::Reviewer);
    }

    /// Assigned (but not a requested reviewer) → `Assignee`.
    #[test]
    fn pr_to_task_role_is_assignee_when_assigned() {
        let mut pr = make_pr(4, "bob");
        pr.reviews = GqlReviews::default();
        pr.assignees = GqlAssignees {
            nodes: vec![GqlAuthor {
                login: "alice".into(),
            }],
        };
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.role, TaskRole::Assignee);
    }

    /// Regression (#800, review finding): assignment outranks my own
    /// review, so an assigned PR I've *approved* stays `Assignee` on both
    /// the inbox scan (reviews empty) AND the lazy-fetch (reviews carry my
    /// approval). Before the fix the review-state check ran first, so the
    /// role flipped Assignee → Mentioned the moment the workspace opened
    /// (`merge_pr_details` overwrites it), silently dropping the PR from
    /// the `assignee` filter.
    #[test]
    fn pr_to_task_assigned_and_approved_stays_assignee_on_both_paths() {
        let assigned = GqlAssignees {
            nodes: vec![GqlAuthor {
                login: "alice".into(),
            }],
        };
        let my_approval = GqlReviews {
            nodes: vec![GqlReview {
                author: Some(GqlAuthor {
                    login: "alice".into(),
                }),
                body: None,
                state: "APPROVED".into(),
                submitted_at: Some(ts(0)),
                id: None,
                created_at: None,
            }],
        };
        // Inbox-scan shape: assignee present, reviews list empty (lazy).
        let mut scan = make_pr(11, "bob");
        scan.assignees = assigned;
        scan.reviews = GqlReviews::default();
        assert_eq!(pr_to_task(&scan, "alice").role, TaskRole::Assignee);
        // Lazy-fetch shape: assignee present AND my approval in the list.
        let mut lazy = make_pr(12, "bob");
        lazy.assignees = GqlAssignees {
            nodes: vec![GqlAuthor {
                login: "alice".into(),
            }],
        };
        lazy.reviews = my_approval;
        assert_eq!(pr_to_task(&lazy, "alice").role, TaskRole::Assignee);
    }

    /// A team review request carries no login, so it must not match me;
    /// with no personal signal the PR stays `Mentioned`.
    #[test]
    fn pr_to_task_team_review_request_does_not_make_me_reviewer() {
        let mut pr = make_pr(4, "bob");
        pr.reviews = GqlReviews::default();
        pr.review_requests = GqlReviewRequests {
            nodes: vec![GqlReviewRequest {
                requested_reviewer: Some(GqlRequestedReviewer::Team {
                    name: "platform".into(),
                }),
            }],
        };
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.role, TaskRole::Mentioned);
    }

    /// My own submitted review drives the role: an approval means I'm
    /// done (`Mentioned`); a non-approving review (changes requested /
    /// comment) keeps me a `Reviewer`. Another person's approval is
    /// irrelevant to my role.
    #[test]
    fn pr_to_task_role_from_my_own_review_state() {
        let review = |login: &str, state: &str| GqlReview {
            author: Some(GqlAuthor {
                login: login.into(),
            }),
            body: None,
            state: state.into(),
            submitted_at: Some(chrono::Utc::now()),
            id: None,
            created_at: None,
        };

        // Someone else approved; I requested changes → still Reviewer.
        let mut pr = make_pr(5, "bob");
        pr.review_decision = Some("APPROVED".into());
        pr.reviews = GqlReviews {
            nodes: vec![
                review("carol", "APPROVED"),
                review("alice", "CHANGES_REQUESTED"),
            ],
        };
        assert_eq!(pr_to_task(&pr, "alice").role, TaskRole::Reviewer);

        // I approved → Mentioned (no further action from me).
        let mut pr = make_pr(6, "bob");
        pr.reviews = GqlReviews {
            nodes: vec![review("alice", "APPROVED")],
        };
        assert_eq!(pr_to_task(&pr, "alice").role, TaskRole::Mentioned);
    }

    /// #960: on a PR where every requested reviewer has already reviewed,
    /// GitHub empties `reviewRequests` — the reviewers must still surface
    /// from the reviews data with their state, not read "none".
    #[test]
    fn pr_to_task_reviews_populate_reviewers_when_requests_are_empty() {
        let mut pr = make_pr(20, "bob");
        pr.review_decision = Some("APPROVED".into());
        pr.review_requests = GqlReviewRequests { nodes: vec![] };
        pr.latest_opinionated_reviews = GqlLatestReviews {
            nodes: vec![
                verdict("claude", "APPROVED"),
                verdict("lukazhupa", "APPROVED"),
            ],
        };

        let task = pr_to_task(&pr, "bob");
        assert!(
            task.reviewers.is_empty(),
            "no pending requests once everyone reviewed",
        );
        assert_eq!(
            task.reviewer_summary(),
            vec![
                Reviewer {
                    login: "claude".into(),
                    state: ReviewState::Approved,
                },
                Reviewer {
                    login: "lukazhupa".into(),
                    state: ReviewState::Approved,
                },
            ],
            "the reviewers line renders the approvers, not \"none\"",
        );
    }

    /// #960 follow-up: reviewers are sourced from GitHub's per-author
    /// `latestOpinionatedReviews`, NOT a `reviews(first: N)` slice — so a
    /// reviewer's latest verdict is rendered even when it lies beyond the
    /// raw-review cap. Here the raw `reviews` slice carries only unrelated
    /// noise; dave's approval exists solely in the collapsed connection.
    #[test]
    fn pr_to_task_reviewers_come_from_collapsed_connection_not_truncated_slice() {
        let noise = |login: &str| GqlReview {
            author: Some(GqlAuthor {
                login: login.into(),
            }),
            body: None,
            state: "COMMENTED".into(),
            submitted_at: Some(chrono::Utc::now()),
            id: None,
            created_at: None,
        };
        let mut pr = make_pr(21, "bob");
        // Stand in for a PR whose first-N reviews are all older comments
        // from other people; dave's later approval fell outside the slice.
        pr.reviews = GqlReviews {
            nodes: (0..10).map(|i| noise(&format!("noise{i}"))).collect(),
        };
        pr.latest_opinionated_reviews = GqlLatestReviews {
            nodes: vec![verdict("dave", "APPROVED")],
        };

        assert_eq!(
            pr_to_task(&pr, "bob").reviewer_summary(),
            vec![Reviewer {
                login: "dave".into(),
                state: ReviewState::Approved,
            }],
            "dave's approval must render even though it's outside the raw reviews slice",
        );
    }

    /// A later `COMMENTED` review must not downgrade an earlier decisive
    /// state: GitHub's `latestOpinionatedReviews` still carries the
    /// approval, and a reviewer's `latestReviews` `COMMENTED` entry is
    /// ignored when they already have a verdict. A reviewer whose only
    /// review is a comment shows `Commented`.
    #[test]
    fn submitted_reviewers_decisive_state_outranks_later_comment() {
        // alice: approved, then left a plain comment (still in her
        // `latestReviews`). bob: comment-only.
        let out = submitted_reviewers(
            &[verdict("alice", "APPROVED")],
            &[verdict("alice", "COMMENTED"), verdict("bob", "COMMENTED")],
        );
        assert_eq!(
            out,
            vec![
                Reviewer {
                    login: "alice".into(),
                    state: ReviewState::Approved,
                },
                Reviewer {
                    login: "bob".into(),
                    state: ReviewState::Commented,
                },
            ],
        );
    }

    /// A reviewer whose latest review is `PENDING` (an unsubmitted draft)
    /// or `DISMISSED` contributes nothing to the reviewers list.
    #[test]
    fn submitted_reviewers_skips_pending_and_dismissed() {
        let out = submitted_reviewers(
            &[],
            &[verdict("alice", "PENDING"), verdict("bob", "DISMISSED")],
        );
        assert!(out.is_empty());
    }

    /// Regression (#800, finding 5): my *latest* decisive review wins.
    /// An approval I later dismissed with a CHANGES_REQUESTED review must
    /// resolve to Reviewer, not Mentioned — `contains("APPROVED")` over
    /// the whole history got this wrong.
    #[test]
    fn pr_to_task_latest_review_state_supersedes_earlier() {
        let review = |state: &str, at: DateTime<Utc>| GqlReview {
            author: Some(GqlAuthor {
                login: "alice".into(),
            }),
            body: None,
            state: state.into(),
            submitted_at: Some(at),
            id: None,
            created_at: None,
        };
        let mut pr = make_pr(7, "bob");
        // Earlier APPROVED, later CHANGES_REQUESTED (out of order in the
        // vec to prove ordering is by submittedAt, not position).
        pr.reviews = GqlReviews {
            nodes: vec![
                review("CHANGES_REQUESTED", ts(10)),
                review("APPROVED", ts(0)),
            ],
        };
        assert_eq!(pr_to_task(&pr, "alice").role, TaskRole::Reviewer);
    }

    /// Regression (#800, finding 4): a standing review request outranks a
    /// prior approval. After I approve, the author pushes and re-requests
    /// my review — GitHub re-adds me to reviewRequests. Both the inbox
    /// scan and the lazy path (which overwrites role on open) must keep
    /// me a Reviewer, not downgrade to Mentioned.
    #[test]
    fn pr_to_task_re_requested_after_approval_is_reviewer() {
        let mut pr = make_pr(8, "bob");
        pr.review_requests = GqlReviewRequests {
            nodes: vec![GqlReviewRequest {
                requested_reviewer: Some(GqlRequestedReviewer::User {
                    login: "alice".into(),
                }),
            }],
        };
        // Lazy path also carries my earlier approval in the reviews list.
        pr.reviews = GqlReviews {
            nodes: vec![GqlReview {
                author: Some(GqlAuthor {
                    login: "alice".into(),
                }),
                body: None,
                state: "APPROVED".into(),
                submitted_at: Some(ts(0)),
                id: None,
                created_at: None,
            }],
        };
        assert_eq!(pr_to_task(&pr, "alice").role, TaskRole::Reviewer);
    }

    /// The PR-global `reviewDecision` must NOT drive *my* role: a PR
    /// where someone else requested changes (`CHANGES_REQUESTED`) but I
    /// only commented — not requested, not assigned, no review of my own
    /// — stays `Mentioned` on the inbox scan. An earlier revision used
    /// `reviewDecision` as a scan fallback, which re-admitted exactly the
    /// commenter noise this rework removed (review finding); role is now
    /// keyed only on per-me signals, so an already-reviewed PR simply
    /// reads `Mentioned` until lazy-fetch refines it on open.
    #[test]
    fn pr_to_task_changes_requested_by_others_is_mentioned_on_scan() {
        let mut pr = make_pr(9, "bob");
        pr.reviews = GqlReviews::default(); // inbox scan, I'm just a commenter
        pr.review_decision = Some("CHANGES_REQUESTED".into());
        assert_eq!(pr_to_task(&pr, "alice").role, TaskRole::Mentioned);
    }

    /// Regression (#800, finding 1): a bot / mannequin / app requested
    /// reviewer arrives as `requestedReviewer: {}` (neither the `User`
    /// nor `Team` fragment applies). The untagged enum must absorb it via
    /// its catch-all instead of failing the whole response parse and
    /// dropping every PR on the page.
    #[test]
    fn requested_reviewer_bot_deserializes_without_error() {
        let json = serde_json::json!({
            "nodes": [
                { "requestedReviewer": { "login": "alice" } },
                { "requestedReviewer": {} },      // bot / mannequin: no fragment
                { "requestedReviewer": null },    // request already fulfilled
            ]
        });
        let parsed: GqlReviewRequests =
            serde_json::from_value(json).expect("bot reviewer must not break parsing");
        assert_eq!(parsed.nodes.len(), 3);
        // Only the real user login survives role/reviewer extraction.
        assert_eq!(requested_reviewer_logins(&parsed), vec!["alice"]);
    }

    // ── Lazy-fetch projection ──────────────────────────────────────

    /// `pr_details_to_details` re-derives everything `merge_pr_details_into_workspace`
    /// needs from the lazy-fetch response. Spot-check the pieces
    /// the inbox-scan path could only approximate.
    #[test]
    fn pr_details_to_details_derives_role_review_closes_checks() {
        let when = chrono::Utc::now();
        let node = GqlPrDetailsNode {
            review_decision: Some("APPROVED".into()),
            author: Some(GqlAuthor {
                login: "bob".into(),
            }),
            assignees: GqlAssignees::default(),
            review_requests: GqlReviewRequests::default(),
            commits: GqlCommits {
                nodes: vec![GqlCommitNode {
                    commit: GqlCommit {
                        status_check_rollup: Some(GqlStatusRollup {
                            state: "SUCCESS".into(),
                            contexts: Some(GqlCheckContexts {
                                nodes: vec![GqlCheckContext::CheckRun {
                                    name: "lint".into(),
                                    conclusion: Some("SUCCESS".into()),
                                    status: Some("COMPLETED".into()),
                                    permalink: None,
                                }],
                            }),
                        }),
                    },
                }],
            },
            comments: GqlComments::default(),
            reviews: GqlReviews {
                nodes: vec![GqlReview {
                    author: Some(GqlAuthor {
                        login: "alice".into(),
                    }),
                    body: Some("lgtm".into()),
                    state: "APPROVED".into(),
                    submitted_at: Some(when),
                    id: None,
                    created_at: None,
                }],
            },
            latest_opinionated_reviews: GqlLatestReviews {
                nodes: vec![verdict("alice", "APPROVED")],
            },
            latest_reviews: GqlLatestReviews::default(),
            closing_issues_references: Some(GqlClosingIssues {
                nodes: vec![GqlClosingIssue {
                    number: 42,
                    repository: GqlIssueRepo {
                        name_with_owner: "acme/widget".into(),
                    },
                }],
            }),
            review_threads: GqlReviewThreads { nodes: vec![] },
        };
        let details = pr_details_to_details(&node, "alice");
        // alice approved → Mentioned (lazy path uses the full
        // reviews list, not the reviewDecision proxy).
        assert_eq!(details.role, TaskRole::Mentioned);
        assert_eq!(details.review, ReviewStatus::Approved);
        assert_eq!(details.ci, CiStatus::Success);
        assert_eq!(details.checks.len(), 1);
        assert_eq!(details.checks[0].name, "lint");
        assert_eq!(details.closes_issues.len(), 1);
        assert_eq!(details.closes_issues[0].key, "acme/widget#42");
        // The review activity made it into the merged feed.
        assert!(details.activities.iter().any(|a| a.body.contains("lgtm")));
    }

    /// Lazy node with no closing refs returns an empty
    /// `closes_issues` — the workspace merge keeps the existing
    /// list (so re-fetch on the same workspace doesn't clobber
    /// it). Tested at the workspace level in lazybox-core.
    #[test]
    fn pr_details_to_details_empty_closes_when_no_refs() {
        let node = GqlPrDetailsNode::default();
        let details = pr_details_to_details(&node, "alice");
        assert!(details.closes_issues.is_empty());
        assert!(details.checks.is_empty());
        assert_eq!(details.ci, CiStatus::None);
    }

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A PENDING review (null `submittedAt`) with a body used to get
    /// `created_at: Utc::now()` and `node_id: None` — a brand-new
    /// (author, body, created_at) identity every poll, so
    /// `Workspace::merge_activity` appended it as a new unread row
    /// forever. Same payload across two synthetic polls must now
    /// produce identity-equal activities carrying the review's node id.
    #[test]
    fn pending_review_activity_identity_is_stable_across_polls() {
        let node_for_poll = || GqlPrDetailsNode {
            reviews: GqlReviews {
                nodes: vec![GqlReview {
                    id: Some("PRR_pending1".into()),
                    author: Some(GqlAuthor {
                        login: "carol".into(),
                    }),
                    body: Some("wip: still looking".into()),
                    state: "PENDING".into(),
                    submitted_at: None,
                    created_at: Some(fixed_time()),
                }],
            },
            ..GqlPrDetailsNode::default()
        };

        let first = pr_details_to_details(&node_for_poll(), "alice");
        let second = pr_details_to_details(&node_for_poll(), "alice");

        let review_of = |d: &PrDetails| {
            d.activities
                .iter()
                .find(|a| a.kind == ActivityKind::Review)
                .cloned()
                .expect("pending review with a body becomes an activity")
        };
        let (a, b) = (review_of(&first), review_of(&second));
        assert_eq!(
            a.node_id.as_deref(),
            Some("PRR_pending1"),
            "review activity must carry the review's GraphQL id"
        );
        assert_eq!(a.node_id, b.node_id);
        assert_eq!(
            a.created_at, b.created_at,
            "timestamp must be poll-independent (was Utc::now() per poll)"
        );
        assert_eq!(a.created_at, fixed_time(), "createdAt is the fallback");
        assert_eq!(
            (a.author.clone(), a.body.clone(), a.created_at),
            (b.author.clone(), b.body.clone(), b.created_at),
            "full node-id-less identity tuple must also be stable"
        );
    }

    /// Even with NO id and NO createdAt (a payload shape from before
    /// those selections), the fallback must be deterministic — never a
    /// wall clock. Two polls of the same payload agree.
    #[test]
    fn pending_review_fallback_timestamp_never_uses_wall_clock() {
        let node_for_poll = || GqlPrDetailsNode {
            reviews: GqlReviews {
                nodes: vec![GqlReview {
                    id: None,
                    author: Some(GqlAuthor {
                        login: "carol".into(),
                    }),
                    body: Some("legacy pending".into()),
                    state: "PENDING".into(),
                    submitted_at: None,
                    created_at: None,
                }],
            },
            ..GqlPrDetailsNode::default()
        };
        let a = pr_details_to_details(&node_for_poll(), "alice");
        let b = pr_details_to_details(&node_for_poll(), "alice");
        let when_of = |d: &PrDetails| {
            d.activities
                .iter()
                .find(|x| x.kind == ActivityKind::Review)
                .expect("activity built")
                .created_at
        };
        assert_eq!(when_of(&a), when_of(&b), "fallback must be deterministic");
        assert_eq!(when_of(&a), DateTime::<Utc>::UNIX_EPOCH);
    }

    /// The eager `pr_to_task` review loop rides the same fix: node id
    /// populated, stable createdAt fallback for a pending review.
    #[test]
    fn pr_to_task_review_activity_has_stable_identity() {
        let mut pr = make_pr(9, "bob");
        pr.reviews = GqlReviews {
            nodes: vec![GqlReview {
                id: Some("PRR_eager1".into()),
                author: Some(GqlAuthor {
                    login: "carol".into(),
                }),
                body: Some("wip note".into()),
                state: "PENDING".into(),
                submitted_at: None,
                created_at: Some(fixed_time()),
            }],
        };
        let t1 = pr_to_task(&pr, "alice");
        let t2 = pr_to_task(&pr, "alice");
        let act1 = t1
            .recent_activity
            .iter()
            .find(|a| a.kind == ActivityKind::Review)
            .expect("review activity");
        let act2 = t2
            .recent_activity
            .iter()
            .find(|a| a.kind == ActivityKind::Review)
            .expect("review activity");
        assert_eq!(act1.node_id.as_deref(), Some("PRR_eager1"));
        assert_eq!(act1.created_at, fixed_time());
        assert_eq!(act1.created_at, act2.created_at);
    }
}
