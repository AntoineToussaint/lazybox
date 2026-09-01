//! Jira Cloud issues provider: polls the REST v3 search API for issues
//! assigned to the authenticated user and maps them to source-agnostic
//! [`Task`]s.
//!
//! Read-only: no mutations, no comment sync. Credentials come from the
//! environment (a personal Atlassian API token):
//! - `LAZYBOX_JIRA_URL`   — site base, e.g. `https://your-site.atlassian.net`
//! - `LAZYBOX_JIRA_EMAIL` — the Atlassian account email the token belongs to
//! - `LAZYBOX_JIRA_TOKEN` — the API token
//!
//! Issue bodies are Atlassian Document Format (nested JSON, not text) and
//! are deliberately not fetched.

use chrono::{DateTime, Utc};
use lazybox_core::{
    CiStatus, Label, Mergeable, ProviderError, ReviewStatus, Task, TaskId, TaskKind, TaskRole,
    TaskState,
};
use serde::Deserialize;

pub const SOURCE: &str = "jira";

/// Issues assigned to the caller and not yet resolved, newest activity
/// first. `resolution = Unresolved` rather than a status list so custom
/// workflows (Defined/Refine/…) are covered without enumeration.
const ASSIGNED_JQL: &str =
    "assignee = currentUser() AND resolution = Unresolved ORDER BY updated DESC";

const MAX_RESULTS: u32 = 100;

#[derive(Debug, thiserror::Error)]
pub enum JiraError {
    #[error("jira not configured: {0}")]
    Config(String),
    #[error("jira request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("jira API error: {0}")]
    Api(String),
}

impl From<JiraError> for ProviderError {
    fn from(e: JiraError) -> Self {
        match e {
            JiraError::Config(_) => ProviderError::permanent(SOURCE, e.to_string()),
            JiraError::Http(_) | JiraError::Api(_) => {
                ProviderError::retryable(SOURCE, e.to_string())
            }
        }
    }
}

pub struct JiraClient {
    base: String,
    email: String,
    token: String,
    http: reqwest::Client,
}

impl JiraClient {
    /// Build from `LAZYBOX_JIRA_URL` / `LAZYBOX_JIRA_EMAIL` /
    /// `LAZYBOX_JIRA_TOKEN`. A missing variable is a config error the
    /// caller logs once, not a retry loop.
    pub fn from_env() -> Result<Self, JiraError> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| JiraError::Config(format!("{name} not set")))
        };
        let base = var("LAZYBOX_JIRA_URL")?.trim_end_matches('/').to_string();
        let email = var("LAZYBOX_JIRA_EMAIL")?;
        let token = var("LAZYBOX_JIRA_TOKEN")?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base,
            email,
            token,
            http,
        })
    }

    /// One page of up to `MAX_RESULTS` assigned unresolved issues,
    /// plus whether Jira signalled more pages beyond it. Assignment
    /// queues deeper than one page aren't paged through — a terminal
    /// inbox tops out well before that — but the caller MUST know the
    /// result was truncated so it does not treat a partial page as the
    /// authoritative full set and retire the rows below the cap.
    pub async fn fetch_assigned(&self) -> Result<AssignedIssues, JiraError> {
        let url = format!("{}/rest/api/3/search/jql", self.base);
        let response = self
            .http
            .get(&url)
            .basic_auth(&self.email, Some(&self.token))
            .query(&[
                ("jql", ASSIGNED_JQL),
                ("maxResults", &MAX_RESULTS.to_string()),
                (
                    "fields",
                    "summary,status,assignee,reporter,updated,created,labels,issuetype,project",
                ),
            ])
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            return Err(JiraError::Api(format!("HTTP {status}: {snippet}")));
        }
        let page: SearchPage = response.json().await?;
        Ok(page_to_assigned(page, &self.base))
    }
}

/// One fetch's worth of assigned issues plus whether the caller saw the
/// complete assignee set. `truncated` is true when Jira returned a
/// `nextPageToken` (there are more assigned-unresolved issues than a
/// single fetch returns), which the poll source turns into a
/// non-authoritative scope so it never deletes the rows it didn't fetch.
pub struct AssignedIssues {
    pub tasks: Vec<Task>,
    pub truncated: bool,
}

/// Map a raw search page to tasks + truncation. Pure (no I/O) so the
/// truncation logic and field mapping are unit-testable without a live
/// Jira. `/rest/api/3/search/jql` is token-paginated: a present
/// `nextPageToken` — NOT a full `MAX_RESULTS`-sized page — is the
/// authoritative "more pages exist" signal (the endpoint may return
/// fewer than requested even when more remain).
fn page_to_assigned(page: SearchPage, base: &str) -> AssignedIssues {
    let truncated = page.next_page_token.is_some();
    let tasks = page
        .issues
        .iter()
        .map(|issue| issue_to_task(issue, base))
        .collect();
    AssignedIssues { tasks, truncated }
}

fn issue_to_task(issue: &Issue, base: &str) -> Task {
    let f = &issue.fields;
    // Jira's three fixed status *categories* ("new" / "indeterminate" /
    // "done") map cleanly even under custom per-project workflows; the
    // exact workflow-state name survives in `state_label`.
    let state = match f
        .status
        .as_ref()
        .and_then(|s| s.status_category.as_ref())
        .map(|c| c.key.as_str())
    {
        Some("indeterminate") => TaskState::InProgress,
        Some("done") => TaskState::Closed,
        _ => TaskState::Open,
    };
    let labels = f
        .labels
        .iter()
        .map(|l| Label::new(l.clone()))
        .collect::<Vec<_>>();
    let assignees = f
        .assignee
        .as_ref()
        .and_then(|a| a.display_name.clone())
        .map(|n| vec![n])
        .unwrap_or_default();
    Task {
        id: TaskId {
            source: SOURCE.into(),
            key: issue.key.clone(),
        },
        title: f.summary.clone().unwrap_or_else(|| issue.key.clone()),
        body: None,
        state,
        // The query is assignee-scoped, so every row is ours by
        // assignment — the strongest involvement the inbox ranks by.
        role: TaskRole::Assignee,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: format!("{base}/browse/{}", issue.key),
        repo: f
            .project
            .as_ref()
            .map(|p| format!("jira/{}", p.key))
            .or_else(|| Some("jira/unknown".to_string())),
        branch: None,
        base_branch: None,
        // Sort key. Prefer `updated`, fall back to `created`; if BOTH
        // are absent or unparseable use a stable floor (the epoch), NOT
        // `Utc::now()`. A per-tick `now()` re-floats the row to the top
        // of the `updated`-sorted inbox on every poll, so a single parse
        // failure would masquerade as fresh activity forever; the epoch
        // sinks it deterministically to the bottom instead.
        updated_at: f
            .updated
            .or(f.created)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
        created_at: f.created.map(|t| t.with_timezone(&Utc)),
        closed_at: None,
        labels,
        reviewers: vec![],
        reviews: vec![],
        assignees,
        author: f
            .reporter
            .as_ref()
            .and_then(|r| r.display_name.clone())
            .unwrap_or_default(),
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: Mergeable::Mergeable,
        is_behind_base: false,
        merge_blocked: false,
        approval_policy: Default::default(),
        node_id: Some(issue.id.clone()),
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        changed_files: 0,
        closes_issues: vec![],
        linked_tasks: vec![],
        parent: None,
        kind: Some(TaskKind::Issue),
        priority: None,
        state_label: f.status.as_ref().map(|s| s.name.clone()),
    }
}

// ─── Wire shapes (subset of the REST v3 search response) ───────────────

#[derive(Debug, Deserialize)]
struct SearchPage {
    #[serde(default)]
    issues: Vec<Issue>,
    /// Present when the assignee queue extends past this page. Its mere
    /// presence — not the page size — is the "more pages" signal on the
    /// token-paginated `/search/jql` endpoint.
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Issue {
    id: String,
    key: String,
    fields: Fields,
}

#[derive(Debug, Deserialize)]
struct Fields {
    summary: Option<String>,
    status: Option<Status>,
    assignee: Option<User>,
    reporter: Option<User>,
    #[serde(default, with = "jira_time")]
    updated: Option<DateTime<chrono::FixedOffset>>,
    #[serde(default, with = "jira_time")]
    created: Option<DateTime<chrono::FixedOffset>>,
    #[serde(default)]
    labels: Vec<String>,
    project: Option<Project>,
}

#[derive(Debug, Deserialize)]
struct Status {
    name: String,
    #[serde(rename = "statusCategory")]
    status_category: Option<StatusCategory>,
}

#[derive(Debug, Deserialize)]
struct StatusCategory {
    key: String,
}

#[derive(Debug, Deserialize)]
struct User {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Project {
    key: String,
}

/// Jira timestamps are `2026-08-31T14:20:30.123-0400` — RFC 3339 except
/// the offset has no colon. Parse the colon-less `%z` form Jira emits,
/// but accept a standard RFC 3339 string (colon offset or `Z`) too so a
/// format change doesn't silently drop every timestamp.
mod jira_time {
    use chrono::{DateTime, FixedOffset};
    use serde::{Deserialize, Deserializer};

    const FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%z";

    /// Parse a Jira timestamp. Try RFC 3339 first so a colon offset or a
    /// trailing `Z` still parses, then fall back to the colon-less `%z`
    /// form Jira Cloud actually emits (`…-0400`), which
    /// `parse_from_rfc3339` rejects.
    fn parse(s: &str) -> Option<DateTime<FixedOffset>> {
        DateTime::parse_from_rfc3339(s)
            .or_else(|_| DateTime::parse_from_str(s, FORMAT))
            .ok()
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Option<DateTime<FixedOffset>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(d)?;
        Ok(raw.and_then(|s| {
            let parsed = parse(&s);
            if parsed.is_none() {
                // Not fatal — the field is optional and `issue_to_task`
                // has a stable floor — but log so a Jira timestamp-format
                // drift is diagnosable instead of silently sinking rows.
                tracing::warn!("jira: unparseable timestamp {s:?}");
            }
            parsed
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_json() -> serde_json::Value {
        serde_json::json!({
            "id": "10042",
            "key": "DEMO-954",
            "fields": {
                "summary": "Advanced filtering for the demo board",
                "status": {"name": "Defined", "statusCategory": {"key": "new"}},
                "assignee": {"displayName": "Avery Assignee"},
                "reporter": {"displayName": "Riley Reporter"},
                "updated": "2026-08-30T09:15:00.000-0400",
                "created": "2026-08-01T10:00:00.000-0400",
                "labels": ["demo"],
                "project": {"key": "DEMO"}
            }
        })
    }

    #[test]
    fn maps_issue_to_task() {
        let issue: Issue = serde_json::from_value(issue_json()).unwrap();
        let task = issue_to_task(&issue, "https://example.atlassian.net");
        assert_eq!(task.id.source, "jira");
        assert_eq!(task.id.key, "DEMO-954");
        assert_eq!(task.state, TaskState::Open);
        assert_eq!(task.role, TaskRole::Assignee);
        assert_eq!(task.url, "https://example.atlassian.net/browse/DEMO-954");
        assert_eq!(task.repo.as_deref(), Some("jira/DEMO"));
        assert_eq!(task.assignees, vec!["Avery Assignee".to_string()]);
        assert_eq!(task.state_label.as_deref(), Some("Defined"));
        assert_eq!(task.kind, Some(TaskKind::Issue));
    }

    #[test]
    fn done_category_maps_closed_and_missing_fields_survive() {
        let mut v = issue_json();
        v["fields"]["status"]["statusCategory"]["key"] = "done".into();
        v["fields"]["assignee"] = serde_json::Value::Null;
        v["fields"]["updated"] = serde_json::Value::Null;
        let issue: Issue = serde_json::from_value(v).unwrap();
        let task = issue_to_task(&issue, "https://example.atlassian.net");
        assert_eq!(task.state, TaskState::Closed);
        assert!(task.assignees.is_empty());
        // A missing `updated` falls back to `created` — NOT `Utc::now()`,
        // which would re-float the row to the top of the inbox each poll.
        let created = DateTime::parse_from_rfc3339("2026-08-01T10:00:00.000-04:00").unwrap();
        assert_eq!(task.updated_at, created.with_timezone(&Utc));
    }

    #[test]
    fn unparseable_timestamps_sink_to_a_stable_floor() {
        // A garbage timestamp must not become `Utc::now()` (perpetual
        // "fresh") — with both timestamps unparseable the row sinks to
        // the epoch, a stable floor that doesn't churn between ticks.
        let mut v = issue_json();
        v["fields"]["updated"] = "not-a-date".into();
        v["fields"]["created"] = "also-bad".into();
        let issue: Issue = serde_json::from_value(v).unwrap();
        let task = issue_to_task(&issue, "https://example.atlassian.net");
        assert_eq!(task.updated_at, DateTime::<Utc>::UNIX_EPOCH);
        assert_eq!(task.created_at, None);
    }

    #[test]
    fn rfc3339_offset_form_still_parses() {
        // Defensive: if Jira ever emits a colon offset or `Z`, we must
        // still parse it rather than sink the row to the floor.
        let mut v = issue_json();
        v["fields"]["updated"] = "2026-08-30T09:15:00.000-04:00".into();
        let issue: Issue = serde_json::from_value(v).unwrap();
        let task = issue_to_task(&issue, "https://example.atlassian.net");
        let want = DateTime::parse_from_rfc3339("2026-08-30T09:15:00.000-04:00").unwrap();
        assert_eq!(task.updated_at, want.with_timezone(&Utc));
    }

    #[test]
    fn truncation_flag_tracks_next_page_token() {
        // No token → we saw the whole assignee set (authoritative).
        let page: SearchPage = serde_json::from_value(serde_json::json!({
            "issues": [issue_json()],
        }))
        .unwrap();
        let assigned = page_to_assigned(page, "https://example.atlassian.net");
        assert_eq!(assigned.tasks.len(), 1);
        assert!(!assigned.truncated);

        // A `nextPageToken` → the queue is deeper than this page, so the
        // caller must NOT treat the result as authoritative (else rows
        // below the cap get deleted and re-added as `updated` reorders).
        let page: SearchPage = serde_json::from_value(serde_json::json!({
            "issues": [issue_json()],
            "nextPageToken": "CAEaAggD",
        }))
        .unwrap();
        let assigned = page_to_assigned(page, "https://example.atlassian.net");
        assert!(assigned.truncated);
    }

    #[test]
    fn from_env_reports_missing_config() {
        // Relies on the test env not defining LAZYBOX_JIRA_* vars.
        assert!(matches!(JiraClient::from_env(), Err(JiraError::Config(_))));
    }
}
