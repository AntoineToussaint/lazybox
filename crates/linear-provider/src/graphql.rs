//! Linear GraphQL types + mappers.

use chrono::{DateTime, Utc};
use lazybox_core::{
    Activity, ActivityKind, CiStatus, LinearScope, ReviewStatus, Task, TaskId, TaskKind, TaskRole,
    TaskState,
};
use serde::Deserialize;

/// Gets the viewer's id for author/assignee attribution. Separate from
/// the issues query so the issues query stays pageable in isolation.
pub const VIEWER_QUERY: &str = r#"
query { viewer { id name } }
"#;

/// The `or` filter clause selecting one [`LinearScope`]. `isMe` is
/// evaluated server-side against the token's viewer, so no viewer id
/// needs threading into the query.
fn scope_clause(scope: LinearScope) -> &'static str {
    match scope {
        LinearScope::Assigned => "{ assignee: { isMe: { eq: true } } }",
        LinearScope::Created => "{ creator: { isMe: { eq: true } } }",
        LinearScope::Subscribed => "{ subscribers: { some: { isMe: { eq: true } } } }",
    }
}

// Scope: open issues (`state.type` not completed/canceled) whose `or`
// clauses are built from the caller's [`LinearScope`] set — assigned to
// me and/or created by me by default, and subscriptions only when the
// user opts in (`Subscribed`), because Linear auto-subscribes you to a
// whole team's issues and ones you merely opened. Without a non-empty
// `or` the `issues` connection returns EVERY open issue in every team
// the token can see (the whole workspace), so an empty scope falls back
// to [`LinearScope::default_scopes`] rather than emitting a bare filter.
fn issues_query(scope: &[LinearScope]) -> String {
    let mut selected: Vec<LinearScope> = Vec::new();
    for &s in scope {
        if !selected.contains(&s) {
            selected.push(s);
        }
    }
    if selected.is_empty() {
        selected = LinearScope::default_scopes();
    }
    let clauses = selected
        .iter()
        .map(|&s| scope_clause(s))
        .collect::<Vec<_>>()
        .join(",\n        ");
    format!(
        r#"
query($after: String) {{
  issues(
    first: 50,
    after: $after,
    filter: {{
      state: {{ type: {{ nin: ["completed", "canceled"] }} }},
      or: [
        {clauses}
      ]
    }}
  ) {{
    pageInfo {{ hasNextPage endCursor }}
    nodes {{
      id
      identifier
      title
      description
      url
      updatedAt
      createdAt
      priority
      state {{ name type }}
      assignee {{ id name }}
      creator {{ id name }}
      team {{ key }}
      labels(first: 10) {{ nodes {{ name }} }}
      attachments(first: 20) {{ nodes {{ url }} }}
    }}
  }}
}}
"#
    )
}

pub fn build_issues_body(after: Option<&str>, scope: &[LinearScope]) -> serde_json::Value {
    let variables = match after {
        Some(cursor) => serde_json::json!({ "after": cursor }),
        None => serde_json::json!({}),
    };
    serde_json::json!({
        "query": issues_query(scope),
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
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<DateTime<Utc>>,
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
    /// External links attached to the issue. Linear's GitHub integration
    /// records a linked PR here as an attachment whose `url` is the PR's
    /// GitHub URL — the authoritative cross-provider link (#922).
    #[serde(default)]
    pub attachments: Option<Attachments>,
}

#[derive(Deserialize, Debug)]
pub struct Attachments {
    pub nodes: Vec<Attachment>,
}

#[derive(Deserialize, Debug)]
pub struct Attachment {
    pub url: String,
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

    let labels: Vec<lazybox_core::Label> = issue
        .labels
        .as_ref()
        .map(|l| {
            l.nodes
                .iter()
                .map(|n| lazybox_core::Label::new(n.name.clone()))
                .collect()
        })
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

    // Cross-provider link (#922): Linear's GitHub integration records a
    // linked PR as an attachment whose URL is the PR's GitHub URL. Parse
    // those into GitHub `TaskId`s so the poller can collapse the ticket
    // and its PR into one workspace and render the link both ways.
    let mut linked_tasks: Vec<TaskId> = Vec::new();
    if let Some(attachments) = issue.attachments.as_ref() {
        for node in &attachments.nodes {
            if let Some(id) = github_pr_id_from_url(&node.url)
                && !linked_tasks.contains(&id)
            {
                linked_tasks.push(id);
            }
        }
    }

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
        created_at: issue.created_at,
        closed_at: None,
        labels,
        reviewers: vec![],
        assignees,
        author: issue
            .creator
            .as_ref()
            .and_then(|c| c.name.clone())
            .unwrap_or_default(),
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Mergeable,
        is_behind_base: false,
        node_id: Some(issue.id.clone()),
        needs_reply: false,
        last_commenter: None,
        recent_activity: activity,
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
        linked_tasks,
        kind: Some(TaskKind::Issue),
        // Linear's numeric issue priority (0 = none → `None`).
        priority: issue.priority.and_then(lazybox_core::Priority::from_linear),
        // Preserve Linear's exact workflow-state name ("In Review",
        // "Todo", …), which `state` above collapses to a canonical set.
        state_label: Some(issue.state.name.clone()),
    }
}

/// Parse a GitHub PR URL into its `TaskId` (`{github, owner/repo#N}`),
/// the id the GitHub provider mints for that PR. `None` for any URL that
/// isn't a `github.com/<owner>/<repo>/pull/<n>` link — Linear attaches
/// other kinds of links too (Slack, Figma, plain GitHub issues), and
/// only the PR shape carries a cross-provider match.
fn github_pr_id_from_url(url: &str) -> Option<TaskId> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    if parts.next()? != "pull" {
        return None;
    }
    let number = parts.next()?;
    // Trailing path/query/fragment after the number is fine
    // (`/pull/12/files`, `/pull/12#discussion`); reject a non-numeric
    // leading segment so `/pull/foo` doesn't match.
    let number: u64 = number
        .split(['/', '#', '?'])
        .next()
        .filter(|n| !n.is_empty())?
        .parse()
        .ok()?;
    Some(TaskId {
        source: "github".into(),
        key: format!("{owner}/{repo}#{number}"),
    })
}

/// Suppress lint for unused `ActivityKind` import since Linear's simple
/// query doesn't surface activities yet.
#[allow(dead_code)]
fn _activity_kind_imported() {
    let _ = ActivityKind::Comment;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_for(scope: &[LinearScope]) -> String {
        build_issues_body(None, scope)["query"]
            .as_str()
            .expect("query is a string")
            .to_string()
    }

    #[test]
    fn default_scope_excludes_subscribers_clause() {
        let q = query_for(&LinearScope::default_scopes());
        assert!(q.contains("assignee: {"), "default keeps assigned-to-me");
        assert!(q.contains("creator: {"), "default keeps created-by-me");
        assert_eq!(q.matches("isMe").count(), 2, "exactly two `or` clauses");
        assert!(
            !q.contains("subscribers"),
            "default must NOT request the subscriber flood: {q}"
        );
    }

    #[test]
    fn empty_scope_falls_back_to_default_not_unscoped() {
        // An empty `or` would return EVERY open issue in the workspace.
        let q = query_for(&[]);
        assert!(q.contains("assignee"));
        assert!(q.contains("creator"));
        assert!(!q.contains("subscribers"));
        assert!(!q.contains("or: [\n      ]"), "the `or` is never empty");
    }

    #[test]
    fn subscribed_scope_adds_the_subscribers_clause() {
        let q = query_for(&[
            LinearScope::Assigned,
            LinearScope::Created,
            LinearScope::Subscribed,
        ]);
        assert!(q.contains("subscribers"), "opt-in adds the clause: {q}");
    }

    #[test]
    fn scope_is_deduped() {
        // "assignee" also appears in the node selection, so count the
        // filter clause (`assignee: { isMe … }`) via its `isMe` marker.
        let q = query_for(&[LinearScope::Assigned, LinearScope::Assigned]);
        assert_eq!(q.matches("isMe").count(), 1, "duplicate scope collapses");
        assert!(
            !q.contains("creator: {"),
            "only the requested scope clause is emitted"
        );
    }

    #[test]
    fn query_requests_attachments() {
        // #922: the cross-provider link needs each issue's attachments.
        let q = query_for(&LinearScope::default_scopes());
        assert!(
            q.contains("attachments"),
            "issue query must pull attachments"
        );
    }

    #[test]
    fn github_pr_url_parses_to_task_id() {
        assert_eq!(
            github_pr_id_from_url("https://github.com/obin-ai/lazybox/pull/922"),
            Some(TaskId {
                source: "github".into(),
                key: "obin-ai/lazybox#922".into(),
            })
        );
        // Trailing path / fragment after the number is tolerated.
        assert_eq!(
            github_pr_id_from_url("https://github.com/o/r/pull/12/files"),
            Some(TaskId {
                source: "github".into(),
                key: "o/r#12".into(),
            })
        );
    }

    #[test]
    fn non_pr_urls_do_not_parse() {
        // Linear attaches Slack/Figma/issue links too — only PR shapes
        // carry a cross-provider match.
        assert_eq!(
            github_pr_id_from_url("https://github.com/o/r/issues/12"),
            None
        );
        assert_eq!(
            github_pr_id_from_url("https://linear.app/acme/issue/ENG-1"),
            None
        );
        assert_eq!(
            github_pr_id_from_url("https://github.com/o/r/pull/abc"),
            None
        );
    }

    fn issue_with_attachments(urls: &[&str]) -> Issue {
        Issue {
            id: "issue-node".into(),
            identifier: "ENG-1".into(),
            title: "Fix it".into(),
            description: None,
            url: "https://linear.app/acme/issue/ENG-1".into(),
            updated_at: Utc::now(),
            created_at: None,
            priority: None,
            state: IssueState {
                name: "Todo".into(),
                kind: "unstarted".into(),
            },
            assignee: None,
            creator: None,
            team: Some(Team { key: "ENG".into() }),
            labels: None,
            attachments: Some(Attachments {
                nodes: urls
                    .iter()
                    .map(|u| Attachment { url: (*u).into() })
                    .collect(),
            }),
        }
    }

    #[test]
    fn issue_to_task_links_pr_from_attachment() {
        // #922: a GitHub PR attachment becomes a cross-provider link.
        let issue = issue_with_attachments(&[
            "https://linear.app/acme/issue/ENG-1", // self link — ignored
            "https://github.com/obin-ai/lazybox/pull/922",
        ]);
        let task = issue_to_task(&issue, "viewer");
        assert_eq!(
            task.linked_tasks,
            vec![TaskId {
                source: "github".into(),
                key: "obin-ai/lazybox#922".into(),
            }]
        );
    }

    #[test]
    fn issue_to_task_has_no_links_without_pr_attachment() {
        let issue = issue_with_attachments(&["https://figma.com/file/xyz"]);
        assert!(issue_to_task(&issue, "viewer").linked_tasks.is_empty());
    }
}
