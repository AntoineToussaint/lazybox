//! Persisted-workspace compatibility corpus.
//!
//! The store persists `Workspace` as JSON; every field rename/retype is
//! a potential silent data-loss bug for existing users. This corpus
//! pins two directions:
//!
//! - **backward**: a hand-written minimal "v0 legacy" blob (the shape
//!   written before most `#[serde(default)]` fields existed) must keep
//!   deserializing;
//! - **forward-stability**: the checked-in golden fixture of a maximal
//!   CURRENT workspace must match a fresh serialization byte-for-value.
//!   If you renamed, retyped, or removed a persisted field, this test
//!   fails — that is the point. Do NOT just regenerate the fixture:
//!   first bump `WORKSPACE_SCHEMA_VERSION` in `core/src/workspace.rs`,
//!   keep the OLD shape deserializing (serde alias/default), add a test
//!   proving it, and only then refresh the fixture with
//!   `LAZYBOX_UPDATE_COMPAT_FIXTURE=1 cargo test -p lazybox-core --test persisted_compat`.

use chrono::{DateTime, TimeZone, Utc};
use lazybox_core::{
    Activity, ActivityKind, AutoFixKind, CheckRun, CiStatus, CleanupPrompt, Label, Mergeable,
    PolicyArm, ReviewState, ReviewStatus, Reviewer, SessionId, SessionKind, SessionLayout,
    SessionRunState, Task, TaskId, TaskKind, TaskRole, TaskState, TileTree,
    WORKSPACE_SCHEMA_VERSION, Workspace, WorkspaceKey, WorkspaceSession,
};
use std::collections::HashSet;
use std::path::PathBuf;

const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/workspace_schema_v2.json"
);

fn at(h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 14, h, m, 0).unwrap()
}

fn session_id(uuid: &str) -> SessionId {
    serde_json::from_value(serde_json::Value::String(uuid.to_string()))
        .expect("fixed UUID parses as SessionId")
}

fn activity(kind: ActivityKind, author: &str, body: &str, h: u32) -> Activity {
    Activity {
        author: author.into(),
        body: body.into(),
        created_at: at(h, 0),
        kind,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    }
}

/// A `Task` with every persisted field populated (no `None`/empty
/// shortcuts except where semantically required).
fn maximal_pr_task() -> Task {
    Task {
        author: "dave".into(),
        id: TaskId {
            source: "github".into(),
            key: "acme/widget#7".into(),
        },
        title: "Add the frobnicator".into(),
        body: Some("Closes #3\n\nLong description body.".into()),
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: CiStatus::Failure,
        review: ReviewStatus::ChangesRequested,
        checks: vec![
            CheckRun {
                name: "build".into(),
                status: CiStatus::Success,
                url: Some("https://ci.example/build/1".into()),
            },
            CheckRun {
                name: "test".into(),
                status: CiStatus::Failure,
                url: None,
            },
        ],
        unread_count: 4,
        url: "https://github.com/acme/widget/pull/7".into(),
        repo: Some("acme/widget".into()),
        branch: Some("feat/frobnicator".into()),
        base_branch: Some("main".into()),
        updated_at: at(9, 30),
        created_at: Some(at(8, 0)),
        closed_at: Some(at(11, 0)),
        labels: vec![Label {
            name: "bug".into(),
            color: "d73a4a".into(),
        }],
        reviewers: vec!["carol".into(), "team/platform".into()],
        reviews: vec![Reviewer {
            login: "dave".into(),
            state: ReviewState::Approved,
            is_bot: false,
        }],
        assignees: vec!["alice".into()],
        auto_merge_enabled: true,
        is_in_merge_queue: true,
        mergeable: Mergeable::Conflicting,
        is_behind_base: true,
        merge_blocked: false,
        approval_policy: Default::default(),
        node_id: Some("PR_kwDOtest001".into()),
        needs_reply: true,
        last_commenter: Some("carol".into()),
        recent_activity: vec![],
        additions: 120,
        deletions: 45,
        changed_files: 7,
        kind: Some(TaskKind::Pr),
        closes_issues: vec![TaskId {
            source: "github".into(),
            key: "acme/widget#3".into(),
        }],
        linked_tasks: vec![TaskId {
            source: "linear".into(),
            key: "ENG-42".into(),
        }],
        priority: None,
        state_label: None,
    }
}

/// The maximal current-shape workspace behind the golden fixture:
/// sessions (both layout variants), an activity feed covering every
/// `ActivityKind` variant, read indices, snooze, policies, notes,
/// snippet MRU, and a resolved cleanup prompt.
fn maximal_workspace() -> Workspace {
    let key = WorkspaceKey::new("github-acme-widget-7");
    let mut ws = Workspace::empty(key.clone(), "feat/frobnicator", at(8, 15));
    ws.project_key = Some(lazybox_core::ProjectKey::github("acme", "widget"));
    ws.local = true;
    ws.linked_checkout = Some(PathBuf::from("/home/dev/code/acme/widget"));
    ws.name = "Add the frobnicator".into();

    let mut issue = maximal_pr_task();
    issue.id.key = "acme/widget#3".into();
    issue.url = "https://github.com/acme/widget/issues/3".into();
    issue.kind = Some(TaskKind::Issue);
    issue.closes_issues = vec![];
    ws.pr = Some(maximal_pr_task());
    ws.gh_issues = vec![issue.clone()];
    let mut linear = issue;
    linear.id = TaskId {
        source: "linear".into(),
        key: "ENG-42".into(),
    };
    linear.url = "https://linear.app/acme/issue/ENG-42".into();
    ws.linear_issues = vec![linear];

    let mut review = activity(ActivityKind::Review, "carol", "please rename this", 10);
    review.node_id = Some("PRR_kwDOtest002".into());
    review.path = Some("src/frob.rs".into());
    review.line = Some(42);
    review.diff_hunk = Some("@@ -1,3 +1,4 @@\n+frob()".into());
    review.thread_id = Some("PRRT_kwDOtest003".into());
    ws.activity = vec![
        activity(ActivityKind::CiUpdate, "ci", "build failed", 12),
        activity(ActivityKind::StatusChange, "bob", "marked ready", 11),
        review,
        activity(ActivityKind::Comment, "alice", "on it", 9),
    ];
    ws.seen_count = 1;
    ws.read_indices = HashSet::from([0, 2]);
    ws.snoozed_until = Some(at(18, 0));
    ws.auto_merge_on_green = true;
    ws.track_main = true;
    ws.base_branch = Some("main".into());
    ws.track_main_behind = true;
    ws.policies.set(AutoFixKind::CiFailure, PolicyArm::Arm);
    ws.policies
        .set(AutoFixKind::MergeConflict, PolicyArm::Disarm);
    ws.notes = "remember: bump the changelog".into();
    ws.sent_snippets = vec!["rev".into(), "fix-ci".into()];
    ws.cleanup_prompt = CleanupPrompt::Declined;
    ws.remote = Some("obin".into());
    ws.last_viewed_at = Some(at(13, 0));

    ws.sessions = vec![
        WorkspaceSession {
            id: session_id("6dbb26a4-3ba3-4f10-b556-0d77a1a2b0d5"),
            workspace_key: key.clone(),
            name: "claude".into(),
            kind: SessionKind::Agent {
                agent_id: "claude".into(),
            },
            state: SessionRunState::Asking,
            worktree_path: PathBuf::from("/tmp/worktrees/acme-widget/PR-7-add-the-frobnicator"),
            worktree_branch: Some("feat/frobnicator".into()),
            created_at: at(8, 30),
            last_output_at: Some(at(12, 30)),
            layout: SessionLayout::Splits {
                tree: TileTree::HSplit {
                    left: Box::new(TileTree::Leaf { terminal_id: 11 }),
                    right: Box::new(TileTree::VSplit {
                        top: Box::new(TileTree::Leaf { terminal_id: 12 }),
                        bottom: Box::new(TileTree::Leaf { terminal_id: 13 }),
                        ratio: 30,
                    }),
                    ratio: 60,
                },
                focused: vec![1, 0],
            },
            provider_session_ids: Default::default(),
        },
        WorkspaceSession {
            id: session_id("94c1de83-97e2-4b73-9d5c-1f0a26c9c9aa"),
            workspace_key: key,
            name: "shell".into(),
            kind: SessionKind::Shell,
            state: SessionRunState::Stopped,
            worktree_path: PathBuf::from("/tmp/worktrees/acme-widget/PR-7-add-the-frobnicator-b"),
            worktree_branch: None,
            created_at: at(9, 0),
            last_output_at: None,
            layout: SessionLayout::Tabs { active: 0 },
            provider_session_ids: Default::default(),
        },
    ];
    ws
}

/// `read_indices` is a `HashSet`, so its JSON array order is not
/// deterministic. Sort it in both values before comparing so the test
/// pins content, not hash order.
fn normalized(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(arr) = value.get_mut("read_indices").and_then(|v| v.as_array_mut()) {
        arr.sort_by_key(|v| v.as_u64().unwrap_or(u64::MAX));
    }
    value
}

fn fixture_json() -> String {
    let fresh = serde_json::to_string_pretty(&maximal_workspace()).expect("serialize");
    if std::env::var("LAZYBOX_UPDATE_COMPAT_FIXTURE").is_ok() {
        std::fs::create_dir_all(std::path::Path::new(FIXTURE_PATH).parent().unwrap())
            .expect("create fixtures dir");
        std::fs::write(FIXTURE_PATH, &fresh).expect("write fixture");
    }
    std::fs::read_to_string(FIXTURE_PATH).unwrap_or_else(|e| {
        panic!(
            "missing compat fixture {FIXTURE_PATH}: {e}\n\
             generate it once with:\n  \
             LAZYBOX_UPDATE_COMPAT_FIXTURE=1 cargo test -p lazybox-core --test persisted_compat"
        )
    })
}

/// A minimal pre-`#[serde(default)]`-era record — the same shape the
/// in-crate legacy tests pin — must keep deserializing forever.
#[test]
fn v0_legacy_minimal_blob_deserializes() {
    let legacy = r#"{"key":"old","name":"old","branch":"main","pr":null,
        "gh_issues":[],"linear_issues":[],"activity":[],"seen_count":0,
        "created_at":"2024-01-01T00:00:00Z","last_viewed_at":null}"#;
    let ws = Workspace::decode_persisted(legacy).expect("v0 legacy blob must always deserialize");
    assert_eq!(ws.schema, 0);
    assert_eq!(ws.key.as_str(), "old");
    assert!(ws.sessions.is_empty());
    assert!(ws.read_indices.is_empty());
    assert!(ws.snoozed_until.is_none());
    assert_eq!(ws.cleanup_prompt, CleanupPrompt::Unresolved);
}

/// A PR row persisted before `Task::author` existed (issue #845) has no
/// `author` key; it must read back with an empty author, never fail to
/// decode.
#[test]
fn pr_row_without_author_field_deserializes_as_empty() {
    let mut legacy = serde_json::to_value(maximal_workspace()).expect("serialize fixture");
    legacy["pr"]
        .as_object_mut()
        .expect("pr object")
        .remove("author");
    let ws = Workspace::decode_persisted(&serde_json::to_string(&legacy).unwrap())
        .expect("a pre-author PR row remains readable");
    assert_eq!(ws.pr.expect("pr present").author, "");
}

/// A PR row persisted before `Task::reviews` existed (#960) has no
/// `reviews` key; it must read back with an empty reviews list, never
/// fail to decode.
#[test]
fn pr_row_without_reviews_field_deserializes_as_empty() {
    let mut legacy = serde_json::to_value(maximal_workspace()).expect("serialize fixture");
    legacy["pr"]
        .as_object_mut()
        .expect("pr object")
        .remove("reviews");
    let ws = Workspace::decode_persisted(&serde_json::to_string(&legacy).unwrap())
        .expect("a pre-reviews PR row remains readable");
    assert!(ws.pr.expect("pr present").reviews.is_empty());
}

/// Schema v1 sessions predate the persisted worktree branch. They remain
/// readable with an unknown branch, so a current daemon can preserve their
/// established checkout without inventing intent from mutable workspace data.
#[test]
fn v1_sessions_without_worktree_branch_deserialize() {
    let mut legacy = serde_json::to_value(maximal_workspace()).expect("serialize fixture");
    legacy["schema"] = serde_json::json!(1);
    for session in legacy["sessions"].as_array_mut().expect("sessions array") {
        session
            .as_object_mut()
            .expect("session object")
            .remove("worktree_branch");
    }
    let ws = Workspace::decode_persisted(&serde_json::to_string(&legacy).unwrap())
        .expect("v1 workspace remains readable");
    assert!(
        ws.sessions
            .iter()
            .all(|session| session.worktree_branch.is_none())
    );
}

/// The checked-in current-schema fixture must keep deserializing, and
/// through the strict (schema-checked) decode path.
#[test]
fn current_fixture_deserializes() {
    let ws = Workspace::decode_persisted(&fixture_json())
        .expect("checked-in current fixture must deserialize");
    assert_eq!(ws.schema, WORKSPACE_SCHEMA_VERSION);
    assert_eq!(ws.sessions.len(), 2);
    assert_eq!(ws.activity.len(), 4);
    assert!(ws.pr.is_some());
}

/// A fresh serialization of the maximal workspace must match the
/// checked-in fixture exactly (modulo HashSet ordering). Any persisted
/// field rename, retype, or removal lands here first.
#[test]
fn current_serialization_matches_checked_in_fixture() {
    let fresh: serde_json::Value =
        serde_json::to_value(maximal_workspace()).expect("serialize maximal workspace");
    let pinned: serde_json::Value =
        serde_json::from_str(&fixture_json()).expect("fixture is valid JSON");
    assert_eq!(
        normalized(fresh),
        normalized(pinned),
        "\nPersisted Workspace serialization changed shape!\n\
         If you renamed/retyped/removed a persisted field:\n\
         1. bump WORKSPACE_SCHEMA_VERSION in crates/core/src/workspace.rs,\n\
         2. keep the OLD shape deserializing (serde default/alias) and add a legacy test,\n\
         3. regenerate this fixture: LAZYBOX_UPDATE_COMPAT_FIXTURE=1 \
         cargo test -p lazybox-core --test persisted_compat\n\
         Do NOT just regenerate — that would ship silent data loss for existing rows."
    );
}

/// Decode + re-encode of the fixture must be lossless: a lenient read
/// that silently drops a stored field would pass plain deserialization
/// but fail here.
#[test]
fn current_fixture_round_trips_losslessly() {
    let ws = Workspace::decode_persisted(&fixture_json()).expect("fixture deserializes");
    let rewritten = serde_json::to_value(&ws).expect("re-serialize");
    let pinned: serde_json::Value = serde_json::from_str(&fixture_json()).expect("fixture JSON");
    assert_eq!(
        normalized(rewritten),
        normalized(pinned),
        "decode→encode of the fixture dropped or altered a field"
    );
}
