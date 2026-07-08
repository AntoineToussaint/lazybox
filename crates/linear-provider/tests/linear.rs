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
}

impl MockLinear {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
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
    let responses = Arc::new(responses);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => return,
                accept = listener.accept() => {
                    let Ok((stream, _)) = accept else { continue };
                    let requests = requests_c.clone();
                    let responses = responses.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let requests = requests.clone();
                            let responses = responses.clone();
                            async move {
                                let _ = req.into_body().collect().await;
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
        labels: Some(Labels {
            nodes: vec![Label { name: "bug".into() }],
        }),
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
fn mapper_author_role_when_only_creator_matches() {
    let issue = make_issue("x", "ENG-2", "unstarted", Some("other"), Some("me"));
    let task = graphql::issue_to_task(&issue, "me");
    assert_eq!(task.role, TaskRole::Author);
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
    assert_eq!(mock.requests.load(Ordering::SeqCst), 2); // viewer + issues

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
    assert_eq!(outcome.tasks.len(), 1, "prefix preserved");
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
    assert_eq!(outcome.tasks.len(), 1);

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
