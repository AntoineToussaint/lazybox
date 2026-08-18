//! End-to-end tests against a mock Linear GraphQL endpoint. A hyper
//! server responds to both the `viewer` + `issues` queries with canned
//! JSON; we drive the real LinearClient against it.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use lazybox_core::{TaskRole, TaskState};
use lazybox_linear::LinearClient;
use lazybox_linear::graphql::{self, Issue, IssueState, Label, Labels, Person, Team};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// ── Mock upstream ──────────────────────────────────────────────────────

struct MockLinear {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    requests: Arc<AtomicUsize>,
    /// Every request body received, in order — lets tests assert what
    /// GraphQL query the client actually sent.
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockLinear {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_mock(responses: Vec<String>) -> MockLinear {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_c = requests.clone();
    let bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let bodies_c = bodies.clone();
    let responses = Arc::new(responses);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => return,
                accept = listener.accept() => {
                    let Ok((stream, _)) = accept else { continue };
                    let requests = requests_c.clone();
                    let bodies = bodies_c.clone();
                    let responses = responses.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let requests = requests.clone();
                            let bodies = bodies.clone();
                            let responses = responses.clone();
                            async move {
                                let collected = req.into_body().collect().await;
                                if let Ok(collected) = collected {
                                    let raw = collected.to_bytes();
                                    bodies
                                        .lock()
                                        .unwrap()
                                        .push(String::from_utf8_lossy(&raw).into_owned());
                                }
                                let idx = requests.fetch_add(1, Ordering::SeqCst);
                                let body = responses
                                    .get(idx)
                                    .cloned()
                                    .unwrap_or_else(|| "{}".to_string());
                                Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .status(StatusCode::OK)
                                        .header("content-type", "application/json")
                                        .body(Full::new(Bytes::from(body)))
                                        .unwrap(),
                                )
                            }
                        });
                        let _ = http1::Builder::new().serve_connection(io, svc).await;
                    });
                }
            }
        }
    });

    MockLinear {
        addr,
        shutdown: Some(shutdown_tx),
        requests,
        bodies,
    }
}

fn viewer_response(id: &str) -> String {
    serde_json::json!({
        "data": { "viewer": { "id": id, "name": "Test User" } }
    })
    .to_string()
}

fn issues_response(issues: serde_json::Value, has_next: bool, cursor: Option<&str>) -> String {
    serde_json::json!({
        "data": {
            "issues": {
                "pageInfo": { "hasNextPage": has_next, "endCursor": cursor },
                "nodes": issues,
            }
        }
    })
    .to_string()
}

// ── Unit-level mapper tests ────────────────────────────────────────────

fn make_issue(
    id: &str,
    identifier: &str,
    state_type: &str,
    assignee_id: Option<&str>,
    creator_id: Option<&str>,
) -> Issue {
    Issue {
        id: id.into(),
        identifier: identifier.into(),
        title: format!("Issue {identifier}"),
        description: Some("body".into()),
        url: format!("https://linear.app/acme/issue/{identifier}"),
        updated_at: chrono::Utc::now(),
        created_at: None,
        priority: Some(2.0),
        state: IssueState {
            name: "State".into(),
            kind: state_type.into(),
        },
        assignee: assignee_id.map(|id| Person {
            id: id.into(),
            name: Some("Assignee".into()),
        }),
        creator: creator_id.map(|id| Person {
            id: id.into(),
            name: Some("Creator".into()),
        }),
        team: Some(Team { key: "ENG".into() }),
        parent: None,
        labels: Some(Labels {
            nodes: vec![Label { name: "bug".into() }],
        }),
        attachments: None,
        comments: None,
    }
}

#[test]
fn mapper_assignee_role_takes_precedence() {
    // When viewer is both creator and assignee, assignee wins.
    let issue = make_issue("x", "ENG-1", "started", Some("me"), Some("me"));
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.role, TaskRole::Assignee);
}

#[test]
fn mapper_carries_created_at_as_age_anchor() {
    // Linear's `createdAt` rides onto `Task.created_at`, and `opened_at`
    // returns it even when the issue was updated far more recently — the
    // age signal the sidebar reads for the stale-issue fade (issue #274).
    let opened = chrono::DateTime::parse_from_rfc3339("2026-01-02T08:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut issue = make_issue("x", "ENG-9", "started", Some("me"), Some("me"));
    issue.created_at = Some(opened);
    issue.updated_at = chrono::DateTime::parse_from_rfc3339("2026-05-28T08:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.created_at, Some(opened));
    assert_eq!(task.opened_at(), opened);
}

#[test]
fn mapper_author_role_when_only_creator_matches() {
    let issue = make_issue("x", "ENG-2", "unstarted", Some("other"), Some("me"));
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.role, TaskRole::Author);
}

#[test]
fn mapper_maps_creator_to_author() {
    let issue = make_issue("x", "ENG-2", "unstarted", Some("other"), Some("me"));
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.author, "Creator");
}

#[test]
fn mapper_author_empty_when_no_creator() {
    let issue = make_issue("x", "ENG-2", "unstarted", Some("other"), None);
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.author, "");
}

#[test]
fn mapper_mentioned_when_neither_matches() {
    let issue = make_issue("x", "ENG-3", "unstarted", Some("a"), Some("b"));
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.role, TaskRole::Mentioned);
}

#[test]
fn mapper_state_mapping() {
    for (linear, expected) in [
        ("triage", TaskState::Open),
        ("backlog", TaskState::Open),
        ("unstarted", TaskState::Open),
        ("started", TaskState::InProgress),
        ("completed", TaskState::Closed),
        ("canceled", TaskState::Closed),
    ] {
        let issue = make_issue("x", "ENG-1", linear, None, None);
        let task = graphql::issue_to_task(&issue, "me");
        assert_eq!(task.state, expected, "state={linear}");
    }
}

#[test]
fn mapper_source_and_key() {
    let issue = make_issue("linear-id", "ENG-42", "started", None, None);
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.id.source, "linear");
    assert_eq!(task.id.key, "ENG-42");
    assert_eq!(task.node_id.as_deref(), Some("linear-id"));
}

#[test]
fn mapper_repo_uses_team_key() {
    let issue = make_issue("x", "ENG-1", "started", None, None);
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.repo.as_deref(), Some("linear/ENG"));
}

#[test]
fn mapper_no_branch_no_ci_no_review() {
    let issue = make_issue("x", "ENG-1", "started", None, None);
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.branch, None);
    assert!(matches!(task.ci, lazybox_core::CiStatus::None));
    assert!(matches!(task.review, lazybox_core::ReviewStatus::None));
}

#[test]
fn mapper_labels_preserved() {
    let issue = make_issue("x", "ENG-1", "started", None, None);
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.labels, vec![lazybox_core::Label::new("bug")]);
}

// ── End-to-end against mock ────────────────────────────────────────────

#[tokio::test]
async fn fetch_all_single_page() {
    let issues = serde_json::json!([
        {
            "id": "a",
            "identifier": "ENG-1",
            "title": "first",
            "description": "body",
            "url": "https://linear.app/acme/issue/ENG-1",
            "updatedAt": "2026-01-01T00:00:00Z",
            "priority": 2,
            "state": { "name": "In Progress", "type": "started" },
            "assignee": { "id": "me", "name": "Me" },
            "creator": { "id": "someone", "name": "Someone" },
            "team": { "key": "ENG" },
            "parent": { "id": "parent-node", "identifier": "ENG-0" },
            "labels": { "nodes": [] }
        }
    ]);
    let mock = spawn_mock(vec![
        viewer_response("me"),
        issues_response(issues, false, None),
    ])
    .await;

    let client = LinearClient::with_key("test-key").with_endpoint(mock.url());
    let tasks = tokio::time::timeout(Duration::from_secs(5), client.fetch_all())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task.id.key, "ENG-1");
    assert_eq!(task.role, TaskRole::Assignee);
    assert_eq!(task.state, TaskState::InProgress);
    assert_eq!(
        task.parent,
        Some(lazybox_core::TaskId {
            source: "linear".into(),
            key: "ENG-0".into(),
        })
    );
    assert_eq!(mock.requests.load(Ordering::SeqCst), 2); // viewer + issues

    mock.shutdown().await;
}

#[tokio::test]
async fn default_client_query_excludes_subscribers() {
    // Regression for #862: the default Linear scope must request only
    // assigned-to-me + created-by-me issues. Without this, Linear's
    // aggressive auto-subscription floods the inbox with unrelated
    // issues via the `subscribers.some.isMe` clause.
    let mock = spawn_mock(vec![
        viewer_response("me"),
        issues_response(serde_json::json!([]), false, None),
    ])
    .await;

    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    client.fetch_all().await.unwrap();

    let issues_body = mock
        .bodies()
        .into_iter()
        .find(|b| b.contains("issues("))
        .expect("an issues query was sent");
    // Match the filter clauses (`assignee: {`), not the bare words —
    // `assignee`/`creator` also appear in the node selection, so a plain
    // `contains` would pass even if the clause were dropped.
    assert!(
        issues_body.contains("assignee: {"),
        "keeps the assigned-to-me clause: {issues_body}"
    );
    assert!(
        issues_body.contains("creator: {"),
        "keeps the created-by-me clause: {issues_body}"
    );
    assert!(
        !issues_body.contains("subscribers"),
        "default must not request the subscriber flood: {issues_body}"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn subscribed_scope_opts_into_subscribers_clause() {
    let mock = spawn_mock(vec![
        viewer_response("me"),
        issues_response(serde_json::json!([]), false, None),
    ])
    .await;

    let client = LinearClient::with_key("k")
        .with_endpoint(mock.url())
        .with_scope(vec![
            lazybox_core::LinearScope::Assigned,
            lazybox_core::LinearScope::Created,
            lazybox_core::LinearScope::Subscribed,
        ]);
    client.fetch_all().await.unwrap();

    let issues_body = mock
        .bodies()
        .into_iter()
        .find(|b| b.contains("issues("))
        .expect("an issues query was sent");
    assert!(
        issues_body.contains("subscribers"),
        "opt-in scope adds the subscriber clause: {issues_body}"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn fetch_all_paginates() {
    let page1 = serde_json::json!([
        {
            "id": "a", "identifier": "ENG-1", "title": "one", "description": null,
            "url": "https://l.app/1", "updatedAt": "2026-01-01T00:00:00Z",
            "priority": null,
            "state": { "name": "", "type": "unstarted" },
            "assignee": null, "creator": null,
            "team": { "key": "ENG" }, "labels": { "nodes": [] }
        }
    ]);
    let page2 = serde_json::json!([
        {
            "id": "b", "identifier": "ENG-2", "title": "two", "description": null,
            "url": "https://l.app/2", "updatedAt": "2026-01-01T00:00:00Z",
            "priority": null,
            "state": { "name": "", "type": "unstarted" },
            "assignee": null, "creator": null,
            "team": { "key": "ENG" }, "labels": { "nodes": [] }
        }
    ]);
    let mock = spawn_mock(vec![
        viewer_response("me"),
        issues_response(page1, true, Some("cur")),
        issues_response(page2, false, None),
    ])
    .await;

    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let tasks = client.fetch_all().await.unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].id.key, "ENG-1");
    assert_eq!(tasks[1].id.key, "ENG-2");
    assert_eq!(
        mock.requests.load(Ordering::SeqCst),
        3,
        "viewer + 2 issue pages"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn fetch_all_graphql_error_surfaces() {
    let error_body = serde_json::json!({
        "errors": [{ "message": "rate limit exceeded" }]
    })
    .to_string();
    let mock = spawn_mock(vec![viewer_response("me"), error_body]).await;

    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let res = client.fetch_all().await;
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(
        err.contains("rate limit exceeded"),
        "error surfaces; got: {err}"
    );

    mock.shutdown().await;
}

/// Mock that answers every request with a fixed non-200 status (plus
/// optional headers). Used for the 429 classification test.
async fn spawn_status_mock(status: StatusCode, headers: Vec<(&'static str, String)>) -> MockLinear {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_c = requests.clone();
    let headers = Arc::new(headers);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => return,
                accept = listener.accept() => {
                    let Ok((stream, _)) = accept else { continue };
                    let requests = requests_c.clone();
                    let headers = headers.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let requests = requests.clone();
                            let headers = headers.clone();
                            async move {
                                let _ = req.into_body().collect().await;
                                requests.fetch_add(1, Ordering::SeqCst);
                                let mut builder = Response::builder().status(status);
                                for (k, v) in headers.iter() {
                                    builder = builder.header(*k, v);
                                }
                                Ok::<_, std::convert::Infallible>(
                                    builder.body(Full::new(Bytes::from("{}"))).unwrap(),
                                )
                            }
                        });
                        let _ = http1::Builder::new().serve_connection(io, svc).await;
                    });
                }
            }
        }
    });

    MockLinear {
        addr,
        shutdown: Some(shutdown_tx),
        requests,
        bodies: Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

/// HTTP 429 used to classify as Permanent (the string probes never
/// matched "429"), so the polling layer gave up on a transient rate
/// limit. It must surface as a retryable ProviderError carrying the
/// Retry-After hint.
#[tokio::test]
async fn http_429_classifies_as_retryable_with_retry_after() {
    let mock = spawn_status_mock(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", "42".to_string())],
    )
    .await;

    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let err = client.fetch_all().await.expect_err("429 must error");
    let provider_err: lazybox_core::ProviderError = err.into();
    match provider_err {
        lazybox_core::ProviderError::Retryable {
            retry_after_secs, ..
        } => assert_eq!(retry_after_secs, Some(42), "Retry-After carried through"),
        other => panic!("429 must be retryable, got {other:?}"),
    }

    mock.shutdown().await;
}

/// A page failing mid-pagination keeps the prefix (inbox stays
/// alive) but the result must be flagged PARTIAL — rescope can't
/// treat "absent from this fetch" as "gone upstream".
#[tokio::test]
async fn mid_pagination_failure_marks_result_partial() {
    let page1 = serde_json::json!([
        {
            "id": "a", "identifier": "ENG-1", "title": "one", "description": null,
            "url": "https://l.app/1", "updatedAt": "2026-01-01T00:00:00Z",
            "priority": null,
            "state": { "name": "", "type": "unstarted" },
            "assignee": null, "creator": null,
            "team": { "key": "ENG" }, "labels": { "nodes": [] }
        }
    ]);
    let page2_error = serde_json::json!({
        "errors": [{ "message": "boom on page 2" }]
    })
    .to_string();
    let mock = spawn_mock(vec![
        viewer_response("me"),
        issues_response(page1, true, Some("cur")),
        page2_error,
    ])
    .await;

    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let outcome = client.fetch_all_with_coverage().await.unwrap();
    assert_eq!(outcome.items.len(), 1, "prefix preserved");
    assert!(
        outcome.is_partial(),
        "a truncated pagination is non-authoritative"
    );

    mock.shutdown().await;
}

/// The happy path stays authoritative: full pagination → Complete.
#[tokio::test]
async fn full_pagination_marks_result_complete() {
    let page = serde_json::json!([
        {
            "id": "a", "identifier": "ENG-1", "title": "one", "description": null,
            "url": "https://l.app/1", "updatedAt": "2026-01-01T00:00:00Z",
            "priority": null,
            "state": { "name": "", "type": "unstarted" },
            "assignee": null, "creator": null,
            "team": { "key": "ENG" }, "labels": { "nodes": [] }
        }
    ]);
    let mock = spawn_mock(vec![
        viewer_response("me"),
        issues_response(page, false, None),
    ])
    .await;

    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let outcome = client.fetch_all_with_coverage().await.unwrap();
    assert!(!outcome.is_partial());
    assert_eq!(outcome.items.len(), 1);

    mock.shutdown().await;
}

#[tokio::test]
async fn missing_next_page_cursor_marks_result_partial() {
    let page = serde_json::json!([
        {
            "id": "a", "identifier": "ENG-1", "title": "one", "description": null,
            "url": "https://l.app/1", "updatedAt": "2026-01-01T00:00:00Z",
            "priority": null,
            "state": { "name": "", "type": "unstarted" },
            "assignee": null, "creator": null,
            "team": { "key": "ENG" }, "labels": { "nodes": [] }
        }
    ]);
    let mock = spawn_mock(vec![
        viewer_response("me"),
        issues_response(page, true, None),
    ])
    .await;

    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let outcome = client.fetch_all_with_coverage().await.unwrap();
    assert_eq!(outcome.items.len(), 1, "the fetched prefix is preserved");
    assert!(
        outcome.is_partial(),
        "missing cursor means the fetch did not cover every page"
    );

    mock.shutdown().await;
}

// ── Comment threads (#1060) ────────────────────────────────────────────

#[tokio::test]
async fn fetch_all_surfaces_comment_threads() {
    let issues = serde_json::json!([
        {
            "id": "a", "identifier": "ENG-1", "title": "first", "description": null,
            "url": "https://l.app/1", "updatedAt": "2026-01-01T00:00:00Z",
            "priority": null,
            "state": { "name": "In Progress", "type": "started" },
            "assignee": null, "creator": null,
            "team": { "key": "ENG" }, "labels": { "nodes": [] },
            "comments": { "nodes": [
                { "id": "c1", "body": "please look", "createdAt": "2026-01-02T00:00:00Z",
                  "user": { "id": "them", "name": "Them" } }
            ] }
        }
    ]);
    let mock = spawn_mock(vec![
        viewer_response("me"),
        issues_response(issues, false, None),
    ])
    .await;

    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let tasks = client.fetch_all().await.unwrap();
    let task = &tasks[0];
    assert_eq!(task.recent_activity.len(), 1);
    assert_eq!(task.recent_activity[0].body, "please look");
    assert!(task.needs_reply, "the last comment is from someone else");
    assert_eq!(task.last_commenter.as_deref(), Some("Them"));

    // The issues query must actually request comments.
    let issues_body = mock
        .bodies()
        .into_iter()
        .find(|b| b.contains("issues("))
        .expect("an issues query was sent");
    assert!(issues_body.contains("comments("), "query pulls comments");

    mock.shutdown().await;
}

// ── Mutations (#1060) ──────────────────────────────────────────────────

use lazybox_core::TaskProvider;

/// A workspace whose single primary task is a Linear issue with the
/// given node id — the shape the mutation handlers read.
fn linear_workspace(issue_node_id: &str) -> lazybox_core::Workspace {
    let mut ws = lazybox_core::Workspace::empty(
        lazybox_core::WorkspaceKey::new("linear-eng-1"),
        "branch",
        chrono::Utc::now(),
    );
    let issue = make_issue(issue_node_id, "ENG-1", "started", None, None);
    ws.linear_issues.push(graphql::issue_to_task(&issue, "me"));
    ws
}

#[tokio::test]
async fn post_reply_posts_a_comment_to_the_issue() {
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "commentCreate": { "success": true } } }).to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    client.post_reply(&ws, "looks good").await.unwrap();

    let body = mock
        .bodies()
        .into_iter()
        .find(|b| b.contains("commentCreate"))
        .expect("a commentCreate mutation was sent");
    assert!(body.contains("issue-uuid"), "targets the issue: {body}");
    assert!(body.contains("looks good"), "carries the body: {body}");

    mock.shutdown().await;
}

#[tokio::test]
async fn set_assignees_resolves_name_then_updates_issue() {
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "users": { "nodes": [
            { "id": "u-1", "name": "alice", "displayName": "Alice A", "email": "alice@x.io" }
        ] } } })
        .to_string(),
        serde_json::json!({ "data": { "issueUpdate": { "success": true } } }).to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    // Picker offers Linear display names; resolution is case-insensitive.
    client
        .set_assignees(&ws, &["Alice A".to_string()])
        .await
        .unwrap();

    let bodies = mock.bodies();
    let lookup = bodies
        .iter()
        .find(|b| b.contains("users("))
        .expect("looked up user");
    // The lookup must filter server-side (not fetch a bounded page and
    // match client-side), else a match past the page is missed in a
    // large workspace. The picked name rides through as the filter arg.
    assert!(
        lookup.contains("eqIgnoreCase") && lookup.contains("filter"),
        "user lookup filters server-side: {lookup}"
    );
    assert!(
        lookup.contains("Alice A"),
        "sends the picked name: {lookup}"
    );
    let update = bodies
        .iter()
        .find(|b| b.contains("issueUpdate"))
        .expect("an issueUpdate mutation was sent");
    assert!(update.contains("assigneeId"), "sets assignee: {update}");
    assert!(update.contains("u-1"), "resolved user id sent: {update}");

    mock.shutdown().await;
}

#[tokio::test]
async fn set_assignees_empty_clears_without_lookup() {
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "issueUpdate": { "success": true } } }).to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    client.set_assignees(&ws, &[]).await.unwrap();

    let bodies = mock.bodies();
    assert!(
        !bodies.iter().any(|b| b.contains("users(")),
        "clearing needs no user lookup"
    );
    let update = bodies
        .iter()
        .find(|b| b.contains("issueUpdate"))
        .expect("an issueUpdate mutation was sent");
    assert!(update.contains("null"), "clears the assignee: {update}");

    mock.shutdown().await;
}

#[tokio::test]
async fn set_assignees_errors_when_name_unknown() {
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "users": { "nodes": [] } } }).to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    let err = client
        .set_assignees(&ws, &["Ghost".to_string()])
        .await
        .expect_err("unknown assignee must error");
    assert!(err.to_string().contains("not found"), "got: {err}");

    mock.shutdown().await;
}

#[tokio::test]
async fn set_assignees_multi_select_assigns_the_last_login() {
    // The picker lists the current assignee first; adding a second name
    // must reassign to the one the user picked, not silently keep the
    // existing assignee. Single-assignee → last login wins.
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "users": { "nodes": [
            { "id": "u-alice", "name": "Alice", "displayName": "Alice", "email": "alice@x.io" },
            { "id": "u-bob", "name": "Bob", "displayName": "Bob", "email": "bob@x.io" }
        ] } } })
        .to_string(),
        serde_json::json!({ "data": { "issueUpdate": { "success": true } } }).to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    client
        .set_assignees(&ws, &["Alice".to_string(), "Bob".to_string()])
        .await
        .unwrap();

    let update = mock
        .bodies()
        .into_iter()
        .find(|b| b.contains("issueUpdate"))
        .expect("an issueUpdate mutation was sent");
    assert!(
        update.contains("u-bob"),
        "the newly-added (last) assignee wins: {update}"
    );
    assert!(
        !update.contains("u-alice"),
        "the pre-existing assignee is not the one set: {update}"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn set_assignees_errors_on_ambiguous_display_name() {
    // Two distinct users share the display name "Sam". Picking the
    // first arbitrarily would assign the wrong person, so resolution
    // must refuse rather than guess.
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "users": { "nodes": [
            { "id": "u-1", "name": "Sam", "displayName": "Sam", "email": "sam1@x.io" },
            { "id": "u-2", "name": "Sam", "displayName": "Sam", "email": "sam2@x.io" }
        ] } } })
        .to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    let err = client
        .set_assignees(&ws, &["Sam".to_string()])
        .await
        .expect_err("ambiguous name must error, not assign an arbitrary user");
    let msg = err.to_string();
    assert!(msg.contains("matches 2"), "names the ambiguity: {msg}");

    // No issueUpdate fired — we never guessed an assignee.
    assert!(
        !mock.bodies().iter().any(|b| b.contains("issueUpdate")),
        "must not mutate when the name is ambiguous"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn set_assignees_email_disambiguates_a_shared_name() {
    // The unique email resolves even when display names collide.
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "users": { "nodes": [
            { "id": "u-1", "name": "Sam", "displayName": "Sam", "email": "sam1@x.io" },
            { "id": "u-2", "name": "Sam", "displayName": "Sam", "email": "sam2@x.io" }
        ] } } })
        .to_string(),
        serde_json::json!({ "data": { "issueUpdate": { "success": true } } }).to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    client
        .set_assignees(&ws, &["sam2@x.io".to_string()])
        .await
        .unwrap();

    let update = mock
        .bodies()
        .into_iter()
        .find(|b| b.contains("issueUpdate"))
        .expect("an issueUpdate mutation was sent");
    assert!(
        update.contains("u-2"),
        "email picks the exact user: {update}"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn close_issue_moves_issue_to_a_canceled_state() {
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "issue": {
            "state": { "type": "started" },
            "team": { "states": { "nodes": [
                { "id": "s-done", "type": "completed" },
                { "id": "s-cancel", "type": "canceled" }
            ] } }
        } } })
        .to_string(),
        serde_json::json!({ "data": { "issueUpdate": { "success": true } } }).to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    client.close_issue(&ws).await.unwrap();

    let update = mock
        .bodies()
        .into_iter()
        .find(|b| b.contains("issueUpdate"))
        .expect("an issueUpdate mutation was sent");
    assert!(update.contains("stateId"), "moves state: {update}");
    assert!(
        update.contains("s-cancel"),
        "picks the canceled state id: {update}"
    );

    mock.shutdown().await;
}

#[tokio::test]
async fn close_issue_is_noop_when_already_closed() {
    let mock = spawn_mock(vec![
        serde_json::json!({ "data": { "issue": {
            "state": { "type": "completed" },
            "team": { "states": { "nodes": [] } }
        } } })
        .to_string(),
    ])
    .await;
    let client = LinearClient::with_key("k").with_endpoint(mock.url());
    let ws = linear_workspace("issue-uuid");

    client.close_issue(&ws).await.unwrap();

    assert_eq!(
        mock.requests.load(Ordering::SeqCst),
        1,
        "only the state query runs; no issueUpdate for an already-closed issue"
    );

    mock.shutdown().await;
}

/// Env-var wiring. Combined into one test so the two cases don't
/// race each other through the shared process env in parallel
/// execution.
#[test]
fn from_env_behavior() {
    use std::sync::Mutex;
    // Serialize across potential future env tests.
    static GUARD: Mutex<()> = Mutex::new(());
    let _g = GUARD.lock().unwrap();
    // SAFETY: env-mutation is racy with other threads reading env;
    // the mutex + single location keeps it deterministic.
    unsafe { std::env::remove_var("LINEAR_API_KEY") };
    assert!(
        LinearClient::from_env().is_err(),
        "missing var → MissingKey"
    );
    unsafe { std::env::set_var("LINEAR_API_KEY", "super-secret") };
    assert!(LinearClient::from_env().is_ok(), "set var → ok");
    unsafe { std::env::remove_var("LINEAR_API_KEY") };
}
