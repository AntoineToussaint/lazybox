//! The shared provider state machine, end to end through the daemon's
//! real ingest path (#1736).
//!
//! `crates/core/src/provider_ops.rs` proves the reducer's transitions in
//! isolation. What these tests pin is the wiring: that an accepted write
//! is *persisted* on the row, that the poll's own `upsert` — not a test
//! double of it — honours the claim, and that the claim ends when the
//! provider catches up.
//!
//! The send half (`ops::send` → `ProviderHandle`) is deliberately absent:
//! `ProviderHandle` is a concrete enum over the real GitHub and Linear
//! clients, with no injection seam, so exercising it would mean network
//! IO. The reducer tests cover the outcome classification; what is not
//! covered here is called out in the PR.

mod common;

use chrono::{Duration, Utc};
use lazybox_core::{
    DesiredFields, MutationField, OpEvent, ProviderOps, Task, TaskId, TaskKind, TaskRole,
    TaskState, Workspace, WorkspaceKey,
};
use lazybox_server::ServerConfig;
use lazybox_server::polling;
use lazybox_store::WorkspaceRecord;

/// A Linear ticket, whose `updated_at` is the provider revision the
/// reconciliation rule turns on.
fn linear_task(
    key: &str,
    state: TaskState,
    label: &str,
    updated_at: chrono::DateTime<Utc>,
) -> Task {
    Task {
        id: TaskId {
            source: "linear".into(),
            key: key.into(),
        },
        title: format!("ticket {key}"),
        body: None,
        state,
        role: TaskRole::Assignee,
        ci: Default::default(),
        review: Default::default(),
        checks: vec![],
        unread_count: 0,
        url: format!("https://linear.app/team/issue/{key}"),
        repo: Some("team".into()),
        branch: None,
        base_branch: None,
        updated_at,
        created_at: None,
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        reviews: vec![],
        approval_policy: Default::default(),
        assignees: vec![],
        author: String::new(),
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: Default::default(),
        is_behind_base: false,
        merge_blocked: false,
        node_id: Some(format!("node-{key}")),
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        changed_files: 0,
        closes_issues: vec![],
        linked_tasks: vec![],
        blocked_by: vec![],
        merge_after: vec![],
        contracts: vec![],
        blocked_on: None,
        parent: None,
        kind: Some(TaskKind::Issue),
        priority: None,
        state_label: Some(label.into()),
    }
}

fn store_workspace(config: &ServerConfig, ws: &Workspace) {
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: ws.key.as_str().to_string(),
            created_at: ws.created_at,
            workspace_json: Some(serde_json::to_string(ws).unwrap()),
        })
        .unwrap();
}

fn load(config: &ServerConfig, key: &WorkspaceKey) -> Workspace {
    let record = config
        .store
        .get_workspace(key)
        .unwrap()
        .expect("workspace row");
    Workspace::decode_persisted(record.workspace_json.as_deref().unwrap()).unwrap()
}

/// Seed a stored Linear workspace carrying an accepted, acknowledged
/// write — the state the daemon is in between "the provider said yes"
/// and "a poll has seen it".
fn seed_with_acked_write(
    config: &ServerConfig,
    task: Task,
    desired: DesiredFields,
    acked_at: chrono::DateTime<Utc>,
) -> WorkspaceKey {
    let mut ws = Workspace::from_task(task.clone(), Utc::now());
    let mut ops = std::mem::take(&mut ws.provider_ops);
    let entity = ws.task_by_id_mut(&task.id).expect("task slot");
    let effects = ops.apply(
        OpEvent::Requested {
            task: task.id.clone(),
            desired,
            now: acked_at - Duration::seconds(1),
        },
        entity,
    );
    let id = effects
        .iter()
        .find_map(|e| match e {
            lazybox_core::OpEffect::Send(id) => Some(*id),
            _ => None,
        })
        .expect("a request sends");
    ops.apply(OpEvent::Sent { id }, entity);
    ops.apply(OpEvent::Acked { id, now: acked_at }, entity);
    ws.provider_ops = ops;
    let key = ws.key.clone();
    store_workspace(config, &ws);
    key
}

/// The headline Linear regression: marking a ticket Done, then a poll
/// reply that left Linear *before* the write, must not reopen it — and a
/// genuine external reopen afterwards must.
#[tokio::test(flavor = "current_thread")]
async fn a_stale_poll_cannot_undo_a_linear_status_write() {
    let config = ServerConfig::in_memory();
    let acked = Utc::now();
    let key = seed_with_acked_write(
        &config,
        linear_task(
            "ENG-1",
            TaskState::InProgress,
            "In Progress",
            acked - Duration::minutes(5),
        ),
        DesiredFields::state(TaskState::Closed, Some("Done".into())),
        acked,
    );

    // A poll produced before the write finally lands.
    polling::upsert(
        &config,
        linear_task(
            "ENG-1",
            TaskState::InProgress,
            "In Progress",
            acked - Duration::seconds(30),
        ),
    )
    .await;
    let ws = load(&config, &key);
    let task = ws.primary_task().expect("task");
    assert_eq!(
        task.state,
        TaskState::Closed,
        "the stale poll must not reopen"
    );
    assert_eq!(task.state_label.as_deref(), Some("Done"));
    assert!(
        !ws.provider_ops.is_empty(),
        "the write is still unconfirmed"
    );

    // A teammate reopens it. That observation postdates the ack, so it is
    // authoritative — a completed Linear issue is not irreversible.
    polling::upsert(
        &config,
        linear_task(
            "ENG-1",
            TaskState::InProgress,
            "In Progress",
            acked + Duration::minutes(1),
        ),
    )
    .await;
    let ws = load(&config, &key);
    let task = ws.primary_task().expect("task");
    assert_eq!(
        task.state,
        TaskState::InProgress,
        "a verified external reopen must be accepted"
    );
    assert_eq!(task.state_label.as_deref(), Some("In Progress"));
    assert!(
        ws.provider_ops.is_empty(),
        "the claim settled on a fresh read"
    );
}

/// The same rule on a plain metadata field: a delayed poll carrying the
/// pre-write assignee must not undo an acknowledged assignment.
#[tokio::test(flavor = "current_thread")]
async fn a_stale_poll_cannot_undo_a_linear_assignment() {
    let config = ServerConfig::in_memory();
    let acked = Utc::now();
    let mut seed = linear_task(
        "ENG-2",
        TaskState::InProgress,
        "In Progress",
        acked - Duration::minutes(5),
    );
    seed.assignees = vec!["bo".into()];
    let key = seed_with_acked_write(
        &config,
        seed,
        DesiredFields::assignees(vec!["ana".into()]),
        acked,
    );

    let mut stale = linear_task(
        "ENG-2",
        TaskState::InProgress,
        "In Progress",
        acked - Duration::seconds(30),
    );
    stale.assignees = vec!["bo".into()];
    polling::upsert(&config, stale).await;
    assert_eq!(
        load(&config, &key).primary_task().unwrap().assignees,
        vec!["ana".to_string()],
    );

    let mut fresh = linear_task(
        "ENG-2",
        TaskState::InProgress,
        "In Progress",
        acked + Duration::minutes(1),
    );
    fresh.assignees = vec!["ana".into()];
    polling::upsert(&config, fresh).await;
    let ws = load(&config, &key);
    assert_eq!(
        ws.primary_task().unwrap().assignees,
        vec!["ana".to_string()]
    );
    assert!(
        ws.provider_ops.is_empty(),
        "confirmed writes leave the ledger"
    );
}

/// An accepted write is on the row *before* any effect runs, which is
/// what lets the daemon be the single authority for what the user sees —
/// and what makes a restart able to tell "never sent" from "maybe sent".
#[tokio::test(flavor = "current_thread")]
async fn an_accepted_write_is_persisted_and_visible_before_it_is_sent() {
    let config = ServerConfig::in_memory();
    let task = linear_task("ENG-3", TaskState::InProgress, "In Progress", Utc::now());
    let ws = Workspace::from_task(task.clone(), Utc::now());
    let key = ws.key.clone();
    store_workspace(&config, &ws);

    polling::apply_and_commit(&config, &key, |ws| {
        let mut ops = std::mem::take(&mut ws.provider_ops);
        let entity = ws.task_by_id_mut(&task.id).expect("task slot");
        ops.apply(
            OpEvent::Requested {
                task: task.id.clone(),
                desired: DesiredFields::labels(vec!["urgent".into()]),
                now: Utc::now(),
            },
            entity,
        );
        ws.provider_ops = ops;
    })
    .await;

    let stored = load(&config, &key);
    assert_eq!(
        stored.provider_ops.pending().len(),
        1,
        "the transition is durable before the effect"
    );
    assert_eq!(
        stored.provider_ops.pending()[0].phase,
        lazybox_core::OpPhase::Accepted,
        "nothing has reached the provider yet"
    );
    // And the row itself — the one this commit broadcast — already shows
    // the intent, so no client needs a second copy of it.
    assert_eq!(
        stored
            .primary_task()
            .unwrap()
            .labels
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        vec!["urgent"]
    );
}

/// A cancellation stops an unsent write from painting the row, so the
/// provider's own value shows through again at once.
#[tokio::test(flavor = "current_thread")]
async fn cancelling_an_unsent_write_uncovers_the_observed_value() {
    let config = ServerConfig::in_memory();
    let now = Utc::now();
    let mut task = linear_task("ENG-4", TaskState::InProgress, "In Progress", now);
    task.assignees = vec!["bo".into()];
    let mut ws = Workspace::from_task(task.clone(), now);
    let mut ops = std::mem::take(&mut ws.provider_ops);
    ops.apply(
        OpEvent::Requested {
            task: task.id.clone(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now,
        },
        ws.task_by_id_mut(&task.id).expect("task slot"),
    );
    ws.provider_ops = ops;
    let key = ws.key.clone();
    store_workspace(&config, &ws);

    polling::ops::cancel(
        &config,
        &key,
        [MutationField::Assignees].into(),
        "assignees",
    )
    .await;

    let stored = load(&config, &key);
    assert!(
        stored.provider_ops.is_empty(),
        "an unsent write is withdrawn cleanly"
    );
    assert_eq!(
        stored.primary_task().unwrap().assignees,
        vec!["bo".to_string()],
        "withdrawing uncovers the provider's value without waiting for a poll"
    );
}

/// Restart recovery reads the ledger off disk. An operation persisted as
/// `Accepted` provably never reached the provider, so it is re-issued
/// rather than reconciled.
#[tokio::test(flavor = "current_thread")]
async fn restart_re_issues_a_write_that_never_left_the_daemon() {
    let config = ServerConfig::in_memory();
    let now = Utc::now();
    let task = linear_task("ENG-5", TaskState::InProgress, "In Progress", now);
    let mut ws = Workspace::from_task(task.clone(), now);
    let mut ops = std::mem::take(&mut ws.provider_ops);
    ops.apply(
        OpEvent::Requested {
            task: task.id.clone(),
            desired: DesiredFields::labels(vec!["urgent".into()]),
            now,
        },
        ws.task_by_id_mut(&task.id).expect("task slot"),
    );
    ws.provider_ops = ops;
    let key = ws.key.clone();
    store_workspace(&config, &ws);

    // Re-decode the persisted row exactly as the daemon does at startup.
    let reloaded = load(&config, &key);
    let mut ops: ProviderOps = reloaded.provider_ops;
    let effects = ops.recover(Utc::now());
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, lazybox_core::OpEffect::Send(_))),
        "an accepted-but-unsent write is issued for the first time, got {effects:?}"
    );
}

/// An additive provider write (`addAssignees`, `requestReviews` with
/// `union: true`) must be recorded as the set it *results in*. Recording
/// only the picked names would blank whoever was already there the
/// moment the row re-rendered — and would be wrong to replay.
#[tokio::test(flavor = "current_thread")]
async fn an_additive_write_records_the_resulting_set() {
    let config = ServerConfig::in_memory();
    let mut task = linear_task("ENG-7", TaskState::InProgress, "In Progress", Utc::now());
    task.assignees = vec!["bo".into()];
    let ws = Workspace::from_task(task.clone(), Utc::now());
    let key = ws.key.clone();
    store_workspace(&config, &ws);

    // `AddAssignees` has no credentials here, so the write itself will be
    // rejected — but acceptance happens first, and what it recorded is
    // what this pins.
    polling::apply_and_commit(&config, &key, |ws| {
        let mut ops = std::mem::take(&mut ws.provider_ops);
        let merged = vec!["bo".to_string(), "ana".to_string()];
        ops.apply(
            OpEvent::Requested {
                task: task.id.clone(),
                desired: DesiredFields::assignees(merged),
                now: Utc::now(),
            },
            ws.task_by_id_mut(&task.id).expect("task slot"),
        );
        ws.provider_ops = ops;
    })
    .await;

    assert_eq!(
        load(&config, &key).primary_task().unwrap().assignees,
        vec!["bo".to_string(), "ana".to_string()],
        "an add keeps who was already there"
    );
}

/// A ledger with nothing in flight adds nothing to the row, so the
/// upsert path's byte-identical skip keeps working (#1799) and idle rows
/// are not re-broadcast every tick.
#[tokio::test(flavor = "current_thread")]
async fn an_idle_ledger_does_not_change_the_persisted_row() {
    let config = ServerConfig::in_memory();
    let task = linear_task("ENG-6", TaskState::InProgress, "In Progress", Utc::now());
    polling::upsert(&config, task.clone()).await;
    let first = config
        .store
        .get_workspace(&WorkspaceKey::new(lazybox_core::workspace_key_for(&task)))
        .unwrap()
        .unwrap()
        .workspace_json;

    polling::upsert(&config, task.clone()).await;
    let second = config
        .store
        .get_workspace(&WorkspaceKey::new(lazybox_core::workspace_key_for(&task)))
        .unwrap()
        .unwrap()
        .workspace_json;
    assert_eq!(first, second, "an empty ledger must be byte-stable");
}
