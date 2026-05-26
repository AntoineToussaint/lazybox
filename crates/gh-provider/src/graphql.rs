//! GraphQL-based GitHub data fetching. One query gets everything.

use chrono::{DateTime, Utc};
use pilot_core::*;
use serde::Deserialize;

/// The single GraphQL query that fetches all PR data.
///
/// ## Connection sizes (`first: N`) are the rate-limit budget
///
/// GitHub's GraphQL cost is dominated by `nodes × Σ first` across
/// every connection. The earlier query was generous to the point
/// of self-DOSing on busy inboxes:
///
///   reviewThreads(50) × comments(10) = 500 review-thread items / PR
///   comments(30) + reviews(20) + checks(30)              = 80 / PR
///   labels(10) + assignees(10) + reviewRequests(10) + closes(10) = 40 / PR
///                                                          ───────
///   = ~620 sub-objects per PR × 100 PRs per page = 62k / page
///
/// Trimmed numbers below land at roughly 1/5 the cost while still
/// covering the realistic case (a PR with 20 review threads of 5
/// comments each is already past the "you should resolve some of
/// these" point, and the activity feed truncates anyway). The tail
/// is recoverable: opening a PR can lazy-fetch the rest in a
/// follow-up `pull-requests/N` query (TODO — separate task).
///
/// Two-sided trade-off:
///   - Too low → users with many comments/reviews lose history in
///     the activity feed until lazy-fetch lands.
///   - Too high → query cost blows the rate budget again.
///
/// Current numbers are deliberately conservative; bump them if a
/// real workflow shows missing content, and add a regression test.
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
            repository {
              nameWithOwner
            }
          }
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

// ─── Response types ────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct GqlResponse {
    pub data: Option<GqlData>,
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct GqlError {
    pub message: String,
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
    pub reviews: GqlReviews,
    /// Inline review-thread comments. Now lazy-fetched per PR via
    /// `Command::FetchPrDetails` (see `PR_DETAILS_QUERY`) instead of
    /// being pulled on every poll cycle — the eager fetch was the
    /// single biggest contributor to GraphQL query cost. `#[serde(default)]`
    /// because the inbox-scan query no longer requests this field.
    #[serde(default, rename = "reviewThreads")]
    pub review_threads: GqlReviewThreads,
    pub commits: GqlCommits,
    /// Issues this PR will close on merge (linked via "Closes #N" in
    /// body, GitHub keyword variants, or the PR sidebar's "Development"
    /// linker). Populated from GitHub's `closingIssuesReferences` —
    /// the canonical link, not title heuristics.
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

#[derive(Deserialize, Debug)]
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
    pub contexts: GqlCheckContexts,
}

#[derive(Deserialize, Debug)]
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

#[derive(Deserialize, Debug)]
pub struct GqlComments {
    pub nodes: Vec<GqlComment>,
}

#[derive(Deserialize, Debug)]
pub struct GqlComment {
    #[serde(default)]
    pub id: Option<String>,
    pub author: Option<GqlAuthor>,
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
}

#[derive(Deserialize, Debug)]
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

pub fn query_body(search_query: &str) -> serde_json::Value {
    query_body_after(search_query, None)
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
            "first": 100,
            "after": cursor,
        }),
        None => serde_json::json!({
            "query": search_query,
            "first": 100,
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

/// GraphQL mutation that merges the PR — same effect as clicking
/// "Merge pull request" on github.com. Default method is what the
/// repo's settings allow (MERGE / SQUASH / REBASE); we don't pin
/// here so the repo's enforced method wins.
const MERGE_PR_MUTATION: &str = r#"
mutation($id: ID!) {
  mergePullRequest(input: { pullRequestId: $id }) {
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

pub fn merge_pr_body(pull_request_node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "query": MERGE_PR_MUTATION,
        "variables": { "id": pull_request_node_id },
    })
}

/// Per-PR "give me everything heavy" query. Pulls full review-thread
/// data (50 threads × 10 comments) for a single PR by node id. Used
/// by the lazy-fetch path: the inbox search returns lightweight
/// metadata, and `fetch_pr_details` only fires when the user opens
/// a PR's activity pane.
///
/// Cost trade-off: GitHub charges this at roughly `1 + 50×(1 + 10)`
/// ≈ 550 cost units, but only WHEN the user asks. The inbox search,
/// which fires every poll cycle for every PR, is correspondingly
/// cheaper without these fields.
const PR_DETAILS_QUERY: &str = r#"
query($id: ID!) {
  node(id: $id) {
    ... on PullRequest {
      id
      updatedAt
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

#[derive(Deserialize, Debug)]
pub struct GqlPrDetailsNode {
    #[serde(rename = "reviewThreads")]
    pub review_threads: GqlReviewThreads,
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
pub fn pr_to_task(pr: &GqlPr, my_username: &str) -> Task {
    let repo = extract_repo_from_url(&pr.url);

    // Determine role.
    let is_author = pr
        .author
        .as_ref()
        .map(|a| a.login == my_username)
        .unwrap_or(false);
    // Did I already approve this PR?
    let i_approved = pr.reviews.nodes.iter().any(|r| {
        r.state == "APPROVED"
            && r.author
                .as_ref()
                .map(|a| a.login == my_username)
                .unwrap_or(false)
    });
    let role = if is_author {
        TaskRole::Author
    } else if i_approved {
        // I've approved — treat as Mentioned (low priority, done my part).
        TaskRole::Mentioned
    } else {
        TaskRole::Reviewer
    };

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

    // Review status from reviewDecision, with a fallback that
    // walks the reviews list. GitHub returns `reviewDecision: null`
    // when no review is REQUIRED (no branch protection, no
    // CODEOWNERS) — even if reviewers have explicitly approved.
    // In that case the sidebar previously showed REVIEW (pending)
    // forever even on PRs the user knew were approved. Fall back
    // to "any latest-per-reviewer APPROVED submission" so the
    // APPROVED pill actually surfaces.
    let review = match pr.review_decision.as_deref() {
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
            for r in &pr.reviews.nodes {
                let Some(author) = r.author.as_ref() else {
                    continue;
                };
                match r.state.as_str() {
                    "APPROVED" | "CHANGES_REQUESTED" => {
                        latest.insert(author.login.clone(), r.state.as_str());
                    }
                    // COMMENT / PENDING / DISMISSED — ignore. A dismissed
                    // review doesn't count; a comment-only review is
                    // signal-less for approval status.
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
    };

    // Build activity from all sources, sorted by time.
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

    activities.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Needs reply: check three signals.
    let needs_reply = needs_reply_check(pr, my_username);

    let last_commenter = activities
        .first()
        .filter(|a| a.author != my_username)
        .map(|a| a.author.clone());

    let unread_count = activities.len() as u32;

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
        labels: pr.labels.nodes.iter().map(|l| l.name.clone()).collect(),
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
        has_conflicts: pr.mergeable.as_deref() == Some("CONFLICTING"),
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

/// Build the `closes_issues` list for a PR Task. Two sources,
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
    // Fallback: title parsing. Repo defaults to the PR's own repo
    // (`pr_repo` — extracted from the PR's URL by the caller) —
    // title closing-keywords almost always reference same-repo
    // issues. Cross-repo references in titles use the fully-qualified
    // `owner/repo#N` syntax that this parser doesn't handle yet —
    // left out by design.
    for n in parse_closes_from_title(&pr.title) {
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

/// Comprehensive needs_reply: check unresolved threads, latest issue comment,
/// and latest review. If the most recent communication on any channel is from
/// someone else and you haven't responded, you owe a reply.
fn needs_reply_check(pr: &GqlPr, my_username: &str) -> bool {
    // 1. Unresolved review threads where the LAST comment is from someone else.
    for t in &pr.review_threads.nodes {
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
        .nodes
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
        .nodes
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
        .nodes
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
            .nodes
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
    let Some(commit_node) = pr.commits.nodes.first() else {
        return CiStatus::None;
    };
    let Some(rollup) = &commit_node.commit.status_check_rollup else {
        return CiStatus::None;
    };
    match rollup.state.as_str() {
        "SUCCESS" => CiStatus::Success,
        "FAILURE" | "ERROR" => CiStatus::Failure,
        // PENDING = the rollup is genuinely in flight (queued or
        // running). Walk the contexts to distinguish "actually
        // running" from "branch protection declared a required
        // check that nobody has triggered" — the latter renders
        // as `CI RUN` today which misleads the user into thinking
        // a job is in progress when it isn't.
        "PENDING" => {
            let any_in_flight = rollup.contexts.nodes.iter().any(|ctx| match ctx {
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
                // EXPECTED / COMPLETED — required check exists in
                // branch protection but no job is running. Surface
                // as None so the sidebar doesn't pretend something
                // is in flight.
                CiStatus::None
            }
        }
        // Top-level `EXPECTED` is the same "required but unstarted"
        // case GitHub uses when branch protection lists a check
        // that hasn't been triggered. Don't lie that it's running.
        "EXPECTED" => CiStatus::None,
        _ => CiStatus::None,
    }
}

fn extract_check_runs(pr: &GqlPr) -> Vec<CheckRun> {
    let Some(commit_node) = pr.commits.nodes.first() else {
        return vec![];
    };
    let Some(rollup) = &commit_node.commit.status_check_rollup else {
        return vec![];
    };
    rollup
        .contexts
        .nodes
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
    pub state: String, // OPEN, CLOSED
    pub author: Option<GqlAuthor>,
    pub labels: GqlLabels,
    pub assignees: GqlAssignees,
    pub comments: GqlComments,
    #[serde(default)]
    pub repository: Option<GqlIssueRepo>,
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
        labels: issue.labels.nodes.iter().map(|l| l.name.clone()).collect(),
        reviewers: vec![],
        assignees: issue
            .assignees
            .nodes
            .iter()
            .map(|a| a.login.clone())
            .collect(),
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        has_conflicts: false,
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
            comments: GqlComments { nodes: vec![] },
            repository: Some(GqlIssueRepo {
                name_with_owner: "o/r".into(),
            }),
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
                },
            ],
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
            }],
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
            comments: GqlComments { nodes: vec![] },
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
        pr.url = "https://github.com/tensorzero/nanogateway/pull/166".into();
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
    fn pr_query_omits_review_threads_entirely() {
        // `reviewThreads × comments` was the dominant cost
        // (50 × 10 = 500 items per PR per inbox scan, every poll).
        // The lazy-fetch path (`PR_DETAILS_QUERY`) now back-fills
        // these on PR open, so the inbox-scan query MUST NOT
        // request them. Hard regression guard — a re-introduction
        // would silently undo the entire lazy split.
        assert!(
            !SEARCH_QUERY.contains("reviewThreads"),
            "reviewThreads must not appear in the inbox-scan query — see PR_DETAILS_QUERY",
        );
        // Sanity-check the lazy query still has them.
        assert!(
            PR_DETAILS_QUERY.contains("reviewThreads(first: 50)"),
            "PR_DETAILS_QUERY is the new home for reviewThreads",
        );
    }

    #[test]
    fn pr_query_other_connection_sizes_are_pinned_low() {
        assert!(
            SEARCH_QUERY.contains("comments(first: 15)"),
            "top-level comments cap drifted",
        );
        assert!(
            SEARCH_QUERY.contains("reviews(first: 10)"),
            "reviews cap drifted",
        );
        assert!(
            SEARCH_QUERY.contains("contexts(first: 20)"),
            "check contexts cap drifted",
        );
        // Hard guard: the previous bloated numbers must NOT come
        // back accidentally.
        assert!(!SEARCH_QUERY.contains("comments(first: 30)"));
        assert!(!SEARCH_QUERY.contains("reviews(first: 20)"));
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
                            },
                        ],
                    },
                }],
            },
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
                        }],
                    },
                }],
            },
        };
        assert!(pr_details_to_activities(&node).is_empty());
    }
}
