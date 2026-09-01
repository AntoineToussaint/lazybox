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
            JiraError::Http(_) | JiraError::Api(_) => ProviderError::retryable(SOURCE, e.to_string()),
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

    /// One page of up to [`MAX_RESULTS`] assigned unresolved issues.
    /// Assignment queues deeper than that are beyond a terminal inbox's
    /// help anyway.
    pub async fn fetch_assigned(&self) -> Result<Vec<Task>, JiraError> {
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
        Ok(page
            .issues
            .iter()
            .map(|issue| issue_to_task(issue, &self.base))
            .collect())
    }
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
    let now = Utc::now();
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
        updated_at: f.updated.map(|t| t.with_timezone(&Utc)).unwrap_or(now),
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
/// the offset has no colon, which chrono's `%z` accepts but
/// `DateTime::parse_from_rfc3339` does not.
mod jira_time {
    use chrono::{DateTime, FixedOffset};
    use serde::{Deserialize, Deserializer};

    const FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%z";

    pub fn deserialize<'de, D>(d: D) -> Result<Option<DateTime<FixedOffset>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(d)?;
        Ok(raw.and_then(|s| DateTime::parse_from_str(&s, FORMAT).ok()))
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
    }

    #[test]
    fn from_env_reports_missing_config() {
        // Relies on the test env not defining LAZYBOX_JIRA_* vars.
        assert!(matches!(
            JiraClient::from_env(),
            Err(JiraError::Config(_))
        ));
    }
}
