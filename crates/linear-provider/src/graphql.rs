//! Linear GraphQL types + mappers.

use chrono::{DateTime, Utc};
use pilot_core::{
    Activity, ActivityKind, CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState,
};
use serde::Deserialize;

/// Gets the viewer's id for author/assignee attribution. Separate from
/// the issues query so the issues query stays pageable in isolation.
pub const VIEWER_QUERY: &str = r#"
query { viewer { id name } }
"#;

const ISSUES_QUERY: &str = r#"
query($after: String) {
  issues(
    first: 50,
    after: $after,
    filter: { state: { type: { nin: ["completed", "canceled"] } } }
  ) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      identifier
      title
      description
      url
      updatedAt
      priority
      state { name type }
      assignee { id name }
      creator { id name }
      team { key }
      labels(first: 10) { nodes { name } }
    }
  }
}
"#;

pub fn build_issues_body(after: Option<&str>) -> serde_json::Value {
    let variables = match after {
        Some(cursor) => serde_json::json!({ "after": cursor }),
        None => serde_json::json!({}),
    };
    serde_json::json!({
        "query": ISSUES_QUERY,
        "variables": variables,
    })
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct ViewerResponse {
    pub data: Option<ViewerData>,
    #[serde(default)]
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct ViewerData {
    pub viewer: Viewer,
}

#[derive(Deserialize, Debug)]
pub struct Viewer {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct IssuesResponse {
    pub data: Option<IssuesData>,
    #[serde(default)]
    pub errors: Option<Vec<GqlError>>,
}

#[derive(Deserialize, Debug)]
pub struct IssuesData {
    pub issues: IssuesConnection,
}

#[derive(Deserialize, Debug)]
pub struct IssuesConnection {
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    pub nodes: Vec<Issue>,
}

#[derive(Deserialize, Debug, Default)]
pub struct PageInfo {
    #[serde(rename = "hasNextPage", default)]
    pub has_next_page: bool,
    #[serde(rename = "endCursor", default)]
    pub end_cursor: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub url: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub priority: Option<f64>,
    pub state: IssueState,
    #[serde(default)]
    pub assignee: Option<Person>,
    #[serde(default)]
    pub creator: Option<Person>,
    #[serde(default)]
    pub team: Option<Team>,
    #[serde(default)]
    pub labels: Option<Labels>,
}

#[derive(Deserialize, Debug)]
pub struct IssueState {
    pub name: String,
    /// Linear state types: "triage" | "backlog" | "unstarted" |
    /// "started" | "completed" | "canceled".
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Deserialize, Debug)]
pub struct Person {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct Team {
    pub key: String,
}

#[derive(Deserialize, Debug)]
pub struct Labels {
    pub nodes: Vec<Label>,
}

#[derive(Deserialize, Debug)]
pub struct Label {
    pub name: String,
}

#[derive(Deserialize, Debug)]
pub struct GqlError {
    pub message: String,
}

// ── Mapper ─────────────────────────────────────────────────────────────

pub fn issue_to_task(issue: &Issue, viewer_id: &str) -> Task {
    let role = match &issue.assignee {
        Some(a) if a.id == viewer_id => TaskRole::Assignee,
        _ => match &issue.creator {
            Some(c) if c.id == viewer_id => TaskRole::Author,
            _ => TaskRole::Mentioned,
        },
    };

    let state = match issue.state.kind.as_str() {
        "triage" | "backlog" | "unstarted" => TaskState::Open,
        "started" => TaskState::InProgress,
        "completed" => TaskState::Closed,
        "canceled" => TaskState::Closed,
        _ => TaskState::Open,
    };

    let labels: Vec<String> = issue
        .labels
        .as_ref()
        .map(|l| l.nodes.iter().map(|n| n.name.clone()).collect())
        .unwrap_or_default();

    let assignees = issue
        .assignee
        .as_ref()
        .and_then(|a| a.name.clone())
        .map(|n| vec![n])
        .unwrap_or_default();

    // Linear has no "activity" endpoint in this simple query — we skip
    // comment threads for now. A richer query can fill them in later.
    let activity: Vec<Activity> = vec![];

    Task {
        id: TaskId {
            source: "linear".into(),
            key: issue.identifier.clone(),
        },
        title: issue.title.clone(),
        body: issue.description.clone(),
        state,
        role,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: issue.url.clone(),
        repo: issue.team.as_ref().map(|t| format!("linear/{}", t.key)),
        branch: None,
        base_branch: None,
        updated_at: issue.updated_at,
        labels,
        reviewers: vec![],
        assignees,
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: pilot_core::Mergeable::Mergeable,
        is_behind_base: false,
        node_id: Some(issue.id.clone()),
        needs_reply: false,
        last_commenter: None,
        recent_activity: activity,
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
    }
}

/// Suppress lint for unused `ActivityKind` import since Linear's simple
/// query doesn't surface activities yet.
#[allow(dead_code)]
fn _activity_kind_imported() {
    let _ = ActivityKind::Comment;
}
