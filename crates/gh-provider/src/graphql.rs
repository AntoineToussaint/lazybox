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
/// New shape pulls **15 sub-objects per PR** (`labels(3) +
/// assignees(5) + reviewRequests(5) + comments(last:1) + commits(1)
/// = 15`), roughly 1/6 the cost. The fields the sidebar doesn't
/// render directly — review history, per-check details, closing
/// issue refs, full comment threads — live in `PR_DETAILS_QUERY`
/// and only fire when the user opens a PR (or via the post-tick
/// `prefetch_top_pr_details` for high-attention rows).
///
/// Trade-offs the lazy split accepts:
///   - `closes_issues` is empty on first sight (until lazy-fetch).
///     `Workspace::attach_task` preserves the lazy-populated value
///     across subsequent polls so re-polls don't clear it.
///   - `checks` (per-check names) is empty until lazy-fetch — only
///     the aggregate `pr.ci` (from `statusCheckRollup.state`) is
///     available at inbox-scan time.
///   - `role = Mentioned` (I approved) requires the full reviews
///     list; without it we fall back to "any approval" via
///     `reviewDecision: APPROVED`. Edge case: PR approved by
///     someone else where I haven't reviewed yet shows
///     `Mentioned` instead of `Reviewer` until lazy-fetch. The
///     sidebar treats both badge states as "no action needed";
///     correctness is restored on workspace open.
///   - `needs_reply` is approximate (last comment from someone
///     else) until lazy-fetch back-fills reviews + review threads.
///
/// If the cost ever needs to go up again, raise the numbers AND
/// the `pr_query_omits_*` regression assertions together so the
/// budget can't drift silently.
const SEARCH_QUERY: &str = r#"
query($query: String!, $first: Int!, $after: String) {
  search(query: $query, type: ISSUE, first: $first, after: $after) {
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
      }
    }
  }
  rateLimit {
    cost
    limit
    remaining
    resetAt
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
}

fn default_limit() -> u32 {
    5000
}

#[derive(Deserialize, Debug)]
pub struct GqlSearch {
    pub nodes: Vec<GqlPr>,
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
    /// Inline review-thread comments. Lazy-fetched per PR via
    /// `Command::FetchPrDetails` (see `PR_DETAILS_QUERY`) instead of
    /// being pulled on every poll cycle.
    #[serde(default, rename = "reviewThreads")]
    pub review_threads: GqlReviewThreads,
    pub commits: GqlCommits,
    /// Issues this PR will close on merge. Lazy-fetched: `SEARCH_QUERY`
    /// dropped this field to cut per-poll cost. On first sight the PR's
    /// `closes_issues` is empty (or title-derived only); once the user
    /// opens the workspace OR `prefetch_top_pr_details` picks the row,
    /// `PR_DETAILS_QUERY` back-fills the canonical `closingIssuesReferences`
    /// and `Workspace::attach_task` preserves it across subsequent polls.
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

#[derive(Deserialize, Debug)]
pub struct GqlAssignees {
    pub nodes: Vec<GqlAuthor>,
}

#[derive(Deserialize, Debug)]
pub struct GqlReviewRequests {
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
    User { login: String },
    Team { name: String },
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

#[derive(Deserialize, Debug)]
pub struct GqlReview {
    pub author: Option<GqlAuthor>,
    pub body: Option<String>,
    pub state: String, // APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED, PENDING
    #[serde(rename = "submittedAt")]
    pub submitted_at: Option<DateTime<Utc>>,
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
const MERGE_PR_MUTATION: &str = r#"
mutation($id: ID!, $method: PullRequestMergeMethod!) {
  mergePullRequest(input: { pullRequestId: $id, mergeMethod: $method }) {
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

pub fn merge_pr_body(pull_request_node_id: &str, merge_method: &str) -> serde_json::Value {
    serde_json::json!({
        "query": MERGE_PR_MUTATION,
        "variables": { "id": pull_request_node_id, "method": merge_method },
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
    limit
    remaining
    resetAt
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
    #[serde(rename = "rateLimit", default)]
    pub rate_limit: Option<GqlRateLimit>,
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
          author { login }
          body
          state
          submittedAt
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
    pub commits: GqlCommits,
    #[serde(default)]
    pub comments: GqlComments,
    #[serde(default)]
    pub reviews: GqlReviews,
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
    let fallback_when = node
        .reviews
        .nodes
        .iter()
        .filter_map(|r| r.submitted_at)
        .max()
        .unwrap_or_else(chrono::Utc::now);
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
            created_at: r.submitted_at.unwrap_or(fallback_when),
            kind: ActivityKind::Review,
            node_id: None,
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
        role: derive_role_inner(
            &node.reviews.nodes,
            node.review_decision.as_deref(),
            my_username,
            is_author,
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
          author { login }
          body
          state
          submittedAt
        }
      }
      closingIssuesReferences(first: 10) {
        nodes {
          number
          repository { nameWithOwner }
        }
      }
    }
  }
  rateLimit {
    limit
    remaining
    resetAt
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
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<GqlRateLimit>,
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
    limit
    remaining
    resetAt
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
    #[serde(rename = "rateLimit")]
    pub rate_limit: Option<GqlRateLimit>,
}

#[derive(Deserialize, Debug)]
pub struct GqlSingleIssueRepository {
    pub issue: Option<GqlIssue>,
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
/// deliberately omits `reviews`, `reviewThreads`, `closingIssuesReferences`,
/// and `statusCheckRollup.contexts` to cut per-poll cost. Fields that
/// depend on those (per-reviewer role detection, `closes_issues`,
/// per-check details, full `needs_reply`) are derived from the
/// reduced shape here; `pr_details_to_details` + `merge_pr_details`
/// back-fill the canonical values on workspace open and via the
/// post-tick prefetch.
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
            created_at: r.submitted_at.unwrap_or(pr.updated_at),
            kind: ActivityKind::Review,
            node_id: None, // Reviews don't have reply-to IDs.
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
            .filter_map(|rr| rr.requested_reviewer.as_ref())
            .map(|rr| match rr {
                GqlRequestedReviewer::User { login } => login.clone(),
                GqlRequestedReviewer::Team { name } => format!("team/{name}"),
            })
            .collect(),
        assignees: pr.assignees.nodes.iter().map(|a| a.login.clone()).collect(),
        auto_merge_enabled: pr.auto_merge_request.is_some(),
        is_in_merge_queue: pr.is_in_merge_queue,
        mergeable: match pr.mergeable.as_deref() {
            Some("CONFLICTING") => lazybox_core::Mergeable::Conflicting,
            Some("MERGEABLE") => lazybox_core::Mergeable::Mergeable,
            // GitHub returns "UNKNOWN" while it lazily computes
            // mergeability — surface as Unknown rather than guess.
            _ => lazybox_core::Mergeable::Unknown,
        },
        is_behind_base: pr.merge_state_status.as_deref() == Some("BEHIND"),
        node_id: pr.id.clone(),
        needs_reply,
        last_commenter,
        recent_activity: activities,
        additions: pr.additions,
        deletions: pr.deletions,
        closes_issues: extract_closes_issues(pr, &repo),
    }
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
        pr.review_decision.as_deref(),
        my_username,
        is_author,
    )
}

fn derive_role_inner(
    reviews: &[GqlReview],
    review_decision: Option<&str>,
    my_username: &str,
    is_author: bool,
) -> TaskRole {
    if is_author {
        return TaskRole::Author;
    }
    // Lazy path: if we have the full reviews list, prefer "did I
    // explicitly approve" — that's the exact signal the pre-trim
    // code used and the one the user expects.
    if !reviews.is_empty() {
        let i_approved = reviews.iter().any(|r| {
            r.state == "APPROVED"
                && r.author
                    .as_ref()
                    .map(|a| a.login == my_username)
                    .unwrap_or(false)
        });
        if i_approved {
            return TaskRole::Mentioned;
        }
        return TaskRole::Reviewer;
    }
    // Inbox-scan path: fall back to reviewDecision.
    if matches!(review_decision, Some("APPROVED")) {
        TaskRole::Mentioned
    } else {
        TaskRole::Reviewer
    }
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
    limit
    remaining
    resetAt
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

pub fn issues_query_body(search_query: &str, after: Option<&str>) -> serde_json::Value {
    let variables = match after {
        Some(cursor) => serde_json::json!({
            "query": search_query,
            "first": 100,
            "after": cursor,
        }),
        None => serde_json::json!({
            "query": search_query,
            "first": 100,
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
        assignees: issue
            .assignees
            .nodes
            .iter()
            .map(|a| a.login.clone())
            .collect(),
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Mergeable,
        is_behind_base: false,
        node_id: issue.id.clone(),
        needs_reply,
        last_commenter,
        recent_activity: comments,
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            review_threads: GqlReviewThreads { nodes: vec![] },
            commits: GqlCommits { nodes: vec![] },
            closing_issues_references: None,
        }
    }

    fn ts(secs_offset: i64) -> DateTime<Utc> {
        chrono::Utc::now() + chrono::Duration::seconds(secs_offset)
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
                },
                GqlReview {
                    author: Some(GqlAuthor {
                        login: "alice".into(),
                    }),
                    body: Some("looks good".into()),
                    state: "APPROVED".into(),
                    submitted_at: Some(ts(-10)),
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
                },
                GqlReview {
                    author: Some(GqlAuthor {
                        login: "bob".into(),
                    }),
                    body: Some("nit found".into()),
                    state: "CHANGES_REQUESTED".into(),
                    submitted_at: Some(ts(-10)),
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
        // long comment on `SEARCH_QUERY`).
        for field in [
            "reviewThreads",
            "closingIssuesReferences",
            "reviews(",
            "contexts(",
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
        // An open PR has no close moment.
        assert_eq!(task.closed_at, None);
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

    /// Inbox-scan + non-author + reviewDecision=APPROVED → Mentioned.
    /// Exercises the rolled-up proxy that replaces the dropped
    /// reviews-list `i_approved` check.
    #[test]
    fn pr_to_task_role_uses_review_decision_when_reviews_empty() {
        let mut pr = make_pr(3, "bob");
        pr.review_decision = Some("APPROVED".into());
        pr.reviews = GqlReviews::default(); // inbox-scan: no reviews
        let task = pr_to_task(&pr, "alice");
        assert_eq!(
            task.role,
            TaskRole::Mentioned,
            "approved PR → role downgrades to Mentioned via reviewDecision proxy",
        );
    }

    /// Inverse: reviewDecision != APPROVED + no reviews → Reviewer.
    #[test]
    fn pr_to_task_role_is_reviewer_when_inbox_scan_not_approved() {
        let mut pr = make_pr(4, "bob");
        pr.review_decision = Some("REVIEW_REQUIRED".into());
        pr.reviews = GqlReviews::default();
        let task = pr_to_task(&pr, "alice");
        assert_eq!(task.role, TaskRole::Reviewer);
    }

    /// Lazy path: with the full reviews list, prefer "did I
    /// approve" over the reviewDecision proxy. PR may be approved
    /// overall by someone else; my role is still Reviewer until
    /// I've approved myself.
    #[test]
    fn pr_to_task_role_prefers_i_approved_when_reviews_present() {
        let mut pr = make_pr(5, "bob");
        pr.review_decision = Some("APPROVED".into());
        pr.reviews = GqlReviews {
            nodes: vec![GqlReview {
                author: Some(GqlAuthor {
                    login: "carol".into(), // someone ELSE approved
                }),
                body: None,
                state: "APPROVED".into(),
                submitted_at: Some(chrono::Utc::now()),
            }],
        };
        let task = pr_to_task(&pr, "alice");
        assert_eq!(
            task.role,
            TaskRole::Reviewer,
            "lazy path prefers 'did I approve' over reviewDecision proxy",
        );
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
                }],
            },
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
}
