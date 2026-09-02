//! Jira Cloud issues provider: polls the REST v3 search API for the
//! unresolved issues the authenticated user is involved in — assigned,
//! reported, or watching, per the roles ticked in setup — and maps them
//! to source-agnostic [`Task`]s.
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
    CiStatus, Label, Mergeable, ProviderConfig, ProviderError, ReviewStatus, Task, TaskId,
    TaskKind, TaskRole, TaskState,
};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::OnceLock;

pub const SOURCE: &str = "jira";

const MAX_RESULTS: u32 = 100;

/// Parent hops fetched above the involved issues so the inbox can nest a
/// story under its epic, the epic under whatever sits above it, and so
/// on to the root (Jira Premium adds levels above Epic). Real
/// hierarchies are a handful deep; the bound only caps the requests a
/// pathological parent cycle could cost per tick.
const MAX_ANCESTOR_ROUNDS: usize = 6;

/// Which involvement roles the poller asks Jira for. Mirrors the Linear
/// model: each enabled role is one `OR` clause of the JQL, and a row's
/// [`TaskRole`] is attributed from the issue itself (see
/// `issue_to_task`). Built from the `role.*` keys the user ticked in
/// setup via [`Self::from_filter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JiraRoles {
    /// `assignee = currentUser()` — `role.assignee`.
    pub assignee: bool,
    /// `reporter = currentUser()` — `role.author`.
    pub reporter: bool,
    /// `watcher = currentUser()` — `role.mentioned`. Jira has no
    /// first-class "mentioned me" query; watching is its closest
    /// equivalent and Jira auto-watches you onto issues you comment on.
    pub watcher: bool,
}

impl JiraRoles {
    /// Read the roles from the user's Jira filter keys.
    pub fn from_filter(filter: &ProviderConfig) -> Self {
        Self {
            assignee: filter.allows_jira_role(TaskRole::Assignee),
            reporter: filter.allows_jira_role(TaskRole::Author),
            watcher: filter.allows_jira_role(TaskRole::Mentioned),
        }
    }

    /// True when no role is ticked — the poller has nothing to ask for
    /// and the caller should skip the source rather than query.
    pub fn is_empty(&self) -> bool {
        !(self.assignee || self.reporter || self.watcher)
    }

    /// The search JQL: the enabled roles OR'd together, unresolved only,
    /// newest activity first. `resolution = Unresolved` rather than a
    /// status list so custom workflows (Defined/Refine/…) are covered
    /// without enumeration. `None` when [`Self::is_empty`].
    pub fn jql(&self) -> Option<String> {
        let mut clauses = Vec::new();
        if self.assignee {
            clauses.push("assignee = currentUser()");
        }
        if self.reporter {
            clauses.push("reporter = currentUser()");
        }
        if self.watcher {
            clauses.push("watcher = currentUser()");
        }
        if clauses.is_empty() {
            return None;
        }
        Some(format!(
            "({}) AND resolution = Unresolved ORDER BY updated DESC",
            clauses.join(" OR ")
        ))
    }
}

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
    /// The authenticated user's Atlassian `accountId`, resolved once per
    /// client from `/myself` and used to attribute each row's
    /// [`TaskRole`] (the search runs on `currentUser()`, but the issue
    /// payload only carries account ids).
    viewer_account_id: OnceLock<String>,
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
            viewer_account_id: OnceLock::new(),
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, JiraError> {
        let url = format!("{}{path}", self.base);
        let response = self
            .http
            .get(&url)
            .basic_auth(&self.email, Some(&self.token))
            .query(query)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            return Err(JiraError::Api(format!("HTTP {status}: {snippet}")));
        }
        Ok(response.json().await?)
    }

    /// The authenticated user's `accountId`, fetched once and cached
    /// for the client's lifetime.
    async fn viewer_account_id(&self) -> Result<String, JiraError> {
        if let Some(id) = self.viewer_account_id.get() {
            return Ok(id.clone());
        }
        let me: Myself = self.get_json("/rest/api/3/myself", &[]).await?;
        let id = me
            .account_id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| JiraError::Api("/myself returned no accountId".into()))?;
        // A concurrent resolve may have won the race; either value is
        // the same account, so the loser's result is simply dropped.
        let _ = self.viewer_account_id.set(id.clone());
        Ok(id)
    }

    /// One page of up to `MAX_RESULTS` unresolved issues the user is
    /// involved in per `roles`, plus whether Jira signalled more pages
    /// beyond it. Queues deeper than one page aren't paged through — a
    /// terminal inbox tops out well before that — but the caller MUST
    /// know the result was truncated so it does not treat a partial
    /// page as the authoritative full set and retire the rows below
    /// the cap.
    ///
    /// The involved issues' ancestors (epic, and whatever sits above it)
    /// are fetched too, by key, so the inbox can nest each row under its
    /// parent — see [`Task::parent`]. Ancestors are context rows: they
    /// carry whatever role the viewer actually has on them (usually none,
    /// i.e. [`TaskRole::Mentioned`]) and are not filtered by `roles`.
    pub async fn fetch_involved(&self, roles: &JiraRoles) -> Result<InvolvedIssues, JiraError> {
        let Some(jql) = roles.jql() else {
            return Err(JiraError::Config("no jira roles enabled".into()));
        };
        let viewer = self.viewer_account_id().await?;
        let mut involved = page_to_involved(self.search(&jql).await?, &self.base, &viewer);

        let mut known: BTreeSet<String> = involved.tasks.iter().map(|t| t.id.key.clone()).collect();
        for _ in 0..MAX_ANCESTOR_ROUNDS {
            let missing = missing_parent_keys(&involved.tasks, &known);
            if missing.is_empty() {
                break;
            }
            // A parent the search doesn't return (deleted, or outside the
            // token's permissions) is still marked known so the climb ends
            // there instead of re-asking every round; its child renders as
            // a root.
            known.extend(missing.iter().cloned());
            for chunk in missing.chunks(MAX_RESULTS as usize) {
                let page = self.search(&ancestors_jql(chunk)).await?;
                let ancestors = page_to_involved(page, &self.base, &viewer);
                involved.truncated |= ancestors.truncated;
                involved.tasks.extend(ancestors.tasks);
            }
        }
        Ok(involved)
    }

    async fn search(&self, jql: &str) -> Result<SearchPage, JiraError> {
        self.get_json(
            "/rest/api/3/search/jql",
            &[
                ("jql", jql),
                ("maxResults", &MAX_RESULTS.to_string()),
                (
                    "fields",
                    "summary,status,assignee,reporter,updated,created,labels,issuetype,project,parent",
                ),
            ],
        )
        .await
    }
}

/// `key in (…)` for one round of ancestor lookups.
fn ancestors_jql(keys: &[String]) -> String {
    format!("key in ({})", keys.join(", "))
}

/// Parent keys referenced by `tasks` that aren't in `known` yet — the
/// next round of ancestors to fetch. Sorted and de-duplicated so the
/// JQL is stable across ticks.
fn missing_parent_keys(tasks: &[Task], known: &BTreeSet<String>) -> Vec<String> {
    tasks
        .iter()
        .filter_map(|t| t.parent.as_ref())
        .map(|p| p.key.clone())
        .filter(|key| !known.contains(key))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The inbox group every issue from this site lands in: `jira/<site>`,
/// e.g. `jira/acme` for `https://acme.atlassian.net`. One group per
/// site rather than per Jira project because a hierarchy routinely
/// spans projects (an epic in `ENG` under an initiative in `PORTFOLIO`)
/// and the inbox only nests rows within one group; the project is
/// already visible in every key (`ENG-123`).
fn site_label(base: &str) -> String {
    let host = base
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default();
    let site = host.strip_suffix(".atlassian.net").unwrap_or(host);
    if site.is_empty() {
        SOURCE.to_string()
    } else {
        site.to_string()
    }
}

/// One fetch's worth of issues plus whether the caller saw the complete
/// set. `truncated` is true when Jira returned a `nextPageToken` (there
/// are more matching issues than a single fetch returns), which the
/// poll source turns into a non-authoritative scope so it never deletes
/// the rows it didn't fetch.
pub struct InvolvedIssues {
    pub tasks: Vec<Task>,
    pub truncated: bool,
}

/// Map a raw search page to tasks + truncation. Pure (no I/O) so the
/// truncation logic and field mapping are unit-testable without a live
/// Jira. `/rest/api/3/search/jql` is token-paginated: a present
/// `nextPageToken` — NOT a full `MAX_RESULTS`-sized page — is the
/// authoritative "more pages exist" signal (the endpoint may return
/// fewer than requested even when more remain).
fn page_to_involved(page: SearchPage, base: &str, viewer_account_id: &str) -> InvolvedIssues {
    let truncated = page.next_page_token.is_some();
    let tasks = page
        .issues
        .iter()
        .map(|issue| issue_to_task(issue, base, viewer_account_id))
        .collect();
    InvolvedIssues { tasks, truncated }
}

/// The viewer's strongest involvement wins, as for Linear: assigned
/// beats reported beats everything else (watching, or a row that only
/// matched because Jira auto-watched the user onto it).
fn role_for(f: &Fields, viewer_account_id: &str) -> TaskRole {
    let is_viewer = |u: &Option<User>| {
        u.as_ref()
            .and_then(|u| u.account_id.as_deref())
            .is_some_and(|id| id == viewer_account_id)
    };
    if is_viewer(&f.assignee) {
        TaskRole::Assignee
    } else if is_viewer(&f.reporter) {
        TaskRole::Author
    } else {
        TaskRole::Mentioned
    }
}

fn issue_to_task(issue: &Issue, base: &str, viewer_account_id: &str) -> Task {
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
    // Rows above the story level (Epic, and Premium's levels above it)
    // are usually context pulled in for nesting; naming the type as the
    // first label is what tells an `EPIC-12` row apart from the stories
    // under it at a glance.
    let type_label = f
        .issuetype
        .as_ref()
        .filter(|t| t.hierarchy_level.unwrap_or(0) > 0)
        .map(|t| Label::new(t.name.clone()));
    let labels = type_label
        .into_iter()
        .chain(f.labels.iter().map(|l| Label::new(l.clone())))
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
        role: role_for(f, viewer_account_id),
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: format!("{base}/browse/{}", issue.key),
        repo: Some(format!("{SOURCE}/{}", site_label(base))),
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
        parent: f.parent.as_ref().map(|p| TaskId {
            source: SOURCE.into(),
            key: p.key.clone(),
        }),
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
    issuetype: Option<IssueType>,
    /// Jira's own hierarchy link (story → epic → …). Absent on roots.
    parent: Option<ParentRef>,
}

#[derive(Debug, Deserialize)]
struct IssueType {
    name: String,
    /// Jira's hierarchy level: -1 sub-task, 0 story/task/bug, 1 epic,
    /// 2+ the levels Premium adds above (initiative, …).
    #[serde(rename = "hierarchyLevel")]
    hierarchy_level: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct ParentRef {
    key: String,
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
    #[serde(rename = "accountId")]
    account_id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

/// `GET /rest/api/3/myself` — only the id is needed.
#[derive(Debug, Deserialize)]
struct Myself {
    #[serde(rename = "accountId")]
    account_id: Option<String>,
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

    const BASE: &str = "https://example.atlassian.net";
    const VIEWER: &str = "acc-viewer";

    fn issue_json() -> serde_json::Value {
        serde_json::json!({
            "id": "10042",
            "key": "DEMO-954",
            "fields": {
                "summary": "Advanced filtering for the demo board",
                "status": {"name": "Defined", "statusCategory": {"key": "new"}},
                "issuetype": {"name": "Story", "hierarchyLevel": 0},
                "assignee": {"accountId": VIEWER, "displayName": "Avery Assignee"},
                "reporter": {"accountId": "acc-riley", "displayName": "Riley Reporter"},
                "updated": "2026-08-30T09:15:00.000-0400",
                "created": "2026-08-01T10:00:00.000-0400",
                "labels": ["demo"],
                "project": {"key": "DEMO"},
                "parent": {"key": "DEMO-600", "fields": {"summary": "Epic", "issuetype": {"name": "Epic"}}}
            }
        })
    }

    fn task_from(v: serde_json::Value) -> Task {
        let issue: Issue = serde_json::from_value(v).unwrap();
        issue_to_task(&issue, BASE, VIEWER)
    }

    #[test]
    fn maps_issue_to_task() {
        let task = task_from(issue_json());
        assert_eq!(task.id.source, "jira");
        assert_eq!(task.id.key, "DEMO-954");
        assert_eq!(task.state, TaskState::Open);
        assert_eq!(task.role, TaskRole::Assignee);
        assert_eq!(task.url, "https://example.atlassian.net/browse/DEMO-954");
        // One group per site, not per project — hierarchies span projects.
        assert_eq!(task.repo.as_deref(), Some("jira/example"));
        assert_eq!(task.assignees, vec!["Avery Assignee".to_string()]);
        assert_eq!(task.state_label.as_deref(), Some("Defined"));
        assert_eq!(task.kind, Some(TaskKind::Issue));
        // Jira's hierarchy link becomes the inbox's nesting parent.
        assert_eq!(
            task.parent.as_ref().map(|p| p.key.as_str()),
            Some("DEMO-600")
        );
        assert_eq!(
            task.parent.as_ref().map(|p| p.source.as_str()),
            Some("jira")
        );
        // A story-level row keeps only its own labels.
        assert_eq!(task.labels.len(), 1);
    }

    #[test]
    fn role_is_the_viewers_strongest_involvement() {
        // Reporter but not assignee → Author.
        let mut v = issue_json();
        v["fields"]["assignee"]["accountId"] = "acc-someone".into();
        v["fields"]["reporter"]["accountId"] = VIEWER.into();
        assert_eq!(task_from(v).role, TaskRole::Author);

        // Neither (a watched issue, or an ancestor pulled in for nesting)
        // → Mentioned.
        let mut v = issue_json();
        v["fields"]["assignee"]["accountId"] = "acc-someone".into();
        assert_eq!(task_from(v).role, TaskRole::Mentioned);

        // Unassigned and reported by someone else → Mentioned, no panic.
        let mut v = issue_json();
        v["fields"]["assignee"] = serde_json::Value::Null;
        assert_eq!(task_from(v).role, TaskRole::Mentioned);
    }

    #[test]
    fn rows_above_story_level_are_labelled_with_their_type() {
        let mut v = issue_json();
        v["fields"]["issuetype"] = serde_json::json!({"name": "Epic", "hierarchyLevel": 1});
        v["fields"]["parent"] = serde_json::Value::Null;
        let task = task_from(v);
        assert_eq!(task.labels[0].name, "Epic");
        assert_eq!(task.labels.len(), 2, "the epic's own labels follow");
        assert!(task.parent.is_none(), "a root has no nesting parent");
    }

    #[test]
    fn done_category_maps_closed_and_missing_fields_survive() {
        let mut v = issue_json();
        v["fields"]["status"]["statusCategory"]["key"] = "done".into();
        v["fields"]["assignee"] = serde_json::Value::Null;
        v["fields"]["updated"] = serde_json::Value::Null;
        v["fields"]["issuetype"] = serde_json::Value::Null;
        let task = task_from(v);
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
        let task = task_from(v);
        assert_eq!(task.updated_at, DateTime::<Utc>::UNIX_EPOCH);
        assert_eq!(task.created_at, None);
    }

    #[test]
    fn rfc3339_offset_form_still_parses() {
        // Defensive: if Jira ever emits a colon offset or `Z`, we must
        // still parse it rather than sink the row to the floor.
        let mut v = issue_json();
        v["fields"]["updated"] = "2026-08-30T09:15:00.000-04:00".into();
        let task = task_from(v);
        let want = DateTime::parse_from_rfc3339("2026-08-30T09:15:00.000-04:00").unwrap();
        assert_eq!(task.updated_at, want.with_timezone(&Utc));
    }

    #[test]
    fn truncation_flag_tracks_next_page_token() {
        // No token → we saw the whole set (authoritative).
        let page: SearchPage = serde_json::from_value(serde_json::json!({
            "issues": [issue_json()],
        }))
        .unwrap();
        let involved = page_to_involved(page, BASE, VIEWER);
        assert_eq!(involved.tasks.len(), 1);
        assert!(!involved.truncated);

        // A `nextPageToken` → the queue is deeper than this page, so the
        // caller must NOT treat the result as authoritative (else rows
        // below the cap get deleted and re-added as `updated` reorders).
        let page: SearchPage = serde_json::from_value(serde_json::json!({
            "issues": [issue_json()],
            "nextPageToken": "CAEaAggD",
        }))
        .unwrap();
        let involved = page_to_involved(page, BASE, VIEWER);
        assert!(involved.truncated);
    }

    #[test]
    fn jql_follows_the_ticked_roles() {
        let all = JiraRoles {
            assignee: true,
            reporter: true,
            watcher: true,
        };
        assert_eq!(
            all.jql().as_deref(),
            Some(
                "(assignee = currentUser() OR reporter = currentUser() OR watcher = currentUser()) \
                 AND resolution = Unresolved ORDER BY updated DESC"
            )
        );
        let assigned_only = JiraRoles {
            assignee: true,
            reporter: false,
            watcher: false,
        };
        assert_eq!(
            assigned_only.jql().as_deref(),
            Some("(assignee = currentUser()) AND resolution = Unresolved ORDER BY updated DESC")
        );
        let none = JiraRoles {
            assignee: false,
            reporter: false,
            watcher: false,
        };
        assert!(none.is_empty());
        assert_eq!(
            none.jql(),
            None,
            "nothing ticked never becomes an unbounded query"
        );
    }

    #[test]
    fn roles_come_from_the_filter_keys() {
        // The default: assigned + reported, watching off (the noisy one).
        let roles = JiraRoles::from_filter(&ProviderConfig::default_for(SOURCE));
        assert_eq!(
            roles,
            JiraRoles {
                assignee: true,
                reporter: true,
                watcher: false,
            }
        );
        let mut only_watching = ProviderConfig::default();
        only_watching.enabled_keys.insert("role.mentioned".into());
        assert_eq!(
            JiraRoles::from_filter(&only_watching),
            JiraRoles {
                assignee: false,
                reporter: false,
                watcher: true,
            }
        );
    }

    #[test]
    fn missing_parents_are_the_next_ancestor_round() {
        let story = task_from(issue_json());
        let mut sibling_v = issue_json();
        sibling_v["key"] = "DEMO-955".into();
        let sibling = task_from(sibling_v);
        let mut root_v = issue_json();
        root_v["key"] = "DEMO-1".into();
        root_v["fields"]["parent"] = serde_json::Value::Null;
        let root = task_from(root_v);

        let known: BTreeSet<String> = ["DEMO-954", "DEMO-955", "DEMO-1"]
            .into_iter()
            .map(str::to_string)
            .collect();
        // Two stories under the same epic ask for it once; the root asks
        // for nothing.
        assert_eq!(
            missing_parent_keys(&[story.clone(), sibling.clone(), root], &known),
            vec!["DEMO-600".to_string()]
        );
        // Once the epic is known the climb is over.
        let mut known = known;
        known.insert("DEMO-600".into());
        assert!(missing_parent_keys(&[story, sibling], &known).is_empty());
        assert_eq!(
            ancestors_jql(&["DEMO-600".to_string(), "PORT-7".to_string()]),
            "key in (DEMO-600, PORT-7)"
        );
    }

    #[test]
    fn site_label_names_the_atlassian_site() {
        assert_eq!(site_label("https://acme.atlassian.net"), "acme");
        assert_eq!(site_label("https://acme.atlassian.net/"), "acme");
        assert_eq!(site_label("https://jira.example.com/"), "jira.example.com");
        assert_eq!(site_label(""), "jira");
    }

    #[test]
    fn from_env_reports_missing_config() {
        // Relies on the test env not defining LAZYBOX_JIRA_* vars.
        assert!(matches!(JiraClient::from_env(), Err(JiraError::Config(_))));
    }
}
