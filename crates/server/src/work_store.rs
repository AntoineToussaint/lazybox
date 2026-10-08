//! Persistence for the task/plan store — the rows `lazybox_core::work` models.
//!
//! `crates/core/src/work.rs` landed the model (#1908) with nothing reading or
//! writing it, which `docs/agent-coordination-v2.md` flags as the gate on
//! phases 3–5: subscriptions, context tiers and cross-box handoff all need a
//! plan that outlives a session. This module is the storage half.
//!
//! It is the shape `review_store` already uses — free functions over
//! `&dyn Store`, called from async code through `crate::store_blocking` — for
//! the same reason: the kv API is blocking, and a module of free functions
//! stays testable against `MockStore` without a daemon.
//!
//! Two rules the readers below all follow:
//!
//! **An undecodable row is skipped, never fatal.** A row written by a newer
//! build with a field this one cannot parse must not take down the whole
//! listing — the note and request readers made that choice first and this
//! follows it. Decode failures are logged once per read, not per row, so a
//! single bad row cannot flood the log on every poll.
//!
//! **Multi-row writes are atomic or they do not happen.** `complete_linked_to`
//! can move several tasks from one merge event; writing them one at a time
//! would leave a plan half-ticked if the process died between rows, so the
//! writes go through [`Store::apply_batch`]. A backend without transactions
//! reports `Unsupported` rather than emulating one, so the caller finds out.

use lazybox_core::WorkspaceKey;
use lazybox_core::work::{
    Link, PLAN_KEY_PREFIX, Party, Plan, PlanId, Progress, Task, WORK_KEY_PREFIX, WorkId,
};
use lazybox_store::{Store, StoreError, StoreMutation};

/// Read one task. `Ok(None)` covers both "no such row" and "a row this build
/// cannot decode" — a caller that needs to tell them apart wants
/// [`all_tasks`], which reports the skip count.
pub fn load_task(store: &dyn Store, id: WorkId) -> Result<Option<Task>, StoreError> {
    let Some(raw) = store.get_kv(&id.storage_key())? else {
        return Ok(None);
    };
    match serde_json::from_str(&raw) {
        Ok(task) => Ok(Some(task)),
        Err(error) => {
            tracing::warn!(%id, %error, "work row does not decode; treating it as absent");
            Ok(None)
        }
    }
}

/// Write one task, replacing any row under the same id.
pub fn save_task(store: &dyn Store, task: &Task) -> Result<(), StoreError> {
    let value = encode_task(task)?;
    store.set_kv(&task.id.storage_key(), &value)
}

/// Write several tasks atomically — all of them land or none do.
///
/// This is what [`complete_linked_to`] uses: one merge can tick off several
/// items on a plan, and a half-applied roll-up is worse than none, because the
/// progress bar then disagrees with the tasks it counts and nothing says why.
pub fn save_tasks(store: &dyn Store, tasks: &[Task]) -> Result<(), StoreError> {
    if tasks.is_empty() {
        return Ok(());
    }
    let mut mutations = Vec::with_capacity(tasks.len());
    for task in tasks {
        mutations.push(StoreMutation::SetKv {
            key: task.id.storage_key(),
            value: encode_task(task)?,
        });
    }
    store.apply_batch(&mutations)
}

/// Forget a task. Its children are *not* removed: an orphaned sub-task is
/// visible and fixable, whereas a cascade that silently deleted a subtree
/// would not be.
pub fn delete_task(store: &dyn Store, id: WorkId) -> Result<(), StoreError> {
    store.delete_kv(&id.storage_key())
}

/// Every task, with the rows this build could not decode counted rather than
/// hidden. Order is the store's; callers that care sort themselves.
pub fn all_tasks(store: &dyn Store) -> Result<(Vec<Task>, usize), StoreError> {
    let rows = store.list_kv_prefix(WORK_KEY_PREFIX)?;
    let total = rows.len();
    let tasks: Vec<Task> = rows
        .into_iter()
        .filter_map(|(_, value)| serde_json::from_str(&value).ok())
        .collect();
    let skipped = total - tasks.len();
    if skipped > 0 {
        tracing::warn!(skipped, total, "work rows skipped: they do not decode");
    }
    Ok((tasks, skipped))
}

/// Every task on one plan.
pub fn tasks_for_plan(store: &dyn Store, plan: PlanId) -> Result<Vec<Task>, StoreError> {
    let (tasks, _) = all_tasks(store)?;
    Ok(tasks
        .into_iter()
        .filter(|task| task.plan == Some(plan))
        .collect())
}

/// Tasks a workspace owns — "what is on my plate", the query `my_work` answers.
///
/// Matching is on the workspace alone and deliberately ignores the `session`
/// provenance beside it: a session id is replaced on every respawn (`Shift-K`,
/// auto-fix, credit recovery all mint a new one), so matching it too would
/// make a task vanish from its owner's list the moment lazybox restarted the
/// agent working on it.
pub fn tasks_owned_by(
    store: &dyn Store,
    workspace: &WorkspaceKey,
) -> Result<Vec<Task>, StoreError> {
    let (tasks, _) = all_tasks(store)?;
    Ok(tasks
        .into_iter()
        .filter(|task| owner_workspace(task) == Some(workspace))
        .collect())
}

/// Tasks a workspace asked someone else for — "what am I waiting on".
pub fn tasks_requested_by(
    store: &dyn Store,
    workspace: &WorkspaceKey,
) -> Result<Vec<Task>, StoreError> {
    let (tasks, _) = all_tasks(store)?;
    Ok(tasks
        .into_iter()
        .filter(|task| party_workspace(&task.requester) == Some(workspace))
        .collect())
}

/// Tasks pointing at `link` — the lookup an auto-check needs when a PR merges.
pub fn tasks_linked_to(store: &dyn Store, link: &Link) -> Result<Vec<Task>, StoreError> {
    let (tasks, _) = all_tasks(store)?;
    Ok(tasks
        .into_iter()
        .filter(|task| task.links_to(link))
        .collect())
}

/// Complete every open task linked to `link`, atomically, and report which
/// moved. Idempotent: a re-delivered merge event finds them terminal and moves
/// nothing, which matters because the poller can see the same merge twice.
pub fn complete_linked_to(
    store: &dyn Store,
    link: &Link,
    by: Party,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<WorkId>, StoreError> {
    let (mut tasks, _) = all_tasks(store)?;
    let moved = lazybox_core::work::complete_linked_to(&mut tasks, link, by, now);
    if moved.is_empty() {
        return Ok(moved);
    }
    let changed: Vec<Task> = tasks
        .into_iter()
        .filter(|task| moved.contains(&task.id))
        .collect();
    save_tasks(store, &changed)?;
    Ok(moved)
}

/// Fail every `Underway` task owned by `workspace` because its agent exited.
///
/// The one place a declared `Lifecycle` moves on an observed `AgentState`
/// change, and the model makes it loud on purpose: the alternative to failing
/// is a task left `Underway` behind a dead agent forever, which reads as work
/// in flight and stalls whoever is waiting on it. Returns what moved.
pub fn fail_underway_for_exit(
    store: &dyn Store,
    workspace: &WorkspaceKey,
    by: Party,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<WorkId>, StoreError> {
    let mut owned = tasks_owned_by(store, workspace)?;
    let mut moved = Vec::new();
    for task in owned.iter_mut() {
        if task.fail_from_agent_exit(by.clone(), now) {
            moved.push(task.id);
        }
    }
    if moved.is_empty() {
        return Ok(moved);
    }
    let changed: Vec<Task> = owned
        .into_iter()
        .filter(|task| moved.contains(&task.id))
        .collect();
    save_tasks(store, &changed)?;
    Ok(moved)
}

/// Grace before an `Underway` task whose agent is gone counts as stranded.
///
/// This window is the whole reason the sweep below exists instead of a hook on
/// the teardown path. lazybox replaces agents constantly and on purpose —
/// `Shift-K` stops every limit-blocked agent and respawns the same
/// conversation, and auto-fix, `a c` and credit recovery do the same — and
/// each replacement is an *exit* followed by a spawn. Failing a task the
/// instant its terminal went away would therefore fail the work of every
/// agent lazybox itself restarted, which is strictly worse than the stranded
/// row it set out to clean up. Ten minutes is far past a respawn and far
/// inside the hour a human waits before asking what happened.
pub const STRANDED_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

/// Whether this task is stranded: declared `Underway`, owned by an agent whose
/// workspace has no live agent, and untouched for longer than `grace`.
///
/// Pure, and takes liveness as a closure, so the rule is testable without a
/// daemon — the thing that matters here is *which* tasks it does not pick up.
pub fn is_stranded(
    task: &Task,
    now: chrono::DateTime<chrono::Utc>,
    grace: std::time::Duration,
    has_live_agent: &impl Fn(&WorkspaceKey) -> bool,
) -> bool {
    use lazybox_core::work::Lifecycle;
    if task.lifecycle != Lifecycle::Underway {
        return false;
    }
    let Some(workspace) = owner_workspace(task) else {
        // Work a human owns is not abandoned because no agent is running it.
        return false;
    };
    if has_live_agent(workspace) {
        return false;
    }
    let Some(last) = task.history.last().map(|event| event.at) else {
        return false;
    };
    let Ok(grace) = chrono::Duration::from_std(grace) else {
        return false;
    };
    now.signed_duration_since(last) > grace
}

/// Fail every stranded task, atomically, and report what moved.
pub fn sweep_stranded(
    store: &dyn Store,
    now: chrono::DateTime<chrono::Utc>,
    grace: std::time::Duration,
    has_live_agent: impl Fn(&WorkspaceKey) -> bool,
) -> Result<Vec<WorkId>, StoreError> {
    let (mut tasks, _) = all_tasks(store)?;
    let mut moved = Vec::new();
    for task in tasks.iter_mut() {
        if !is_stranded(task, now, grace, &has_live_agent) {
            continue;
        }
        if task.fail_from_agent_exit(Party::Lazybox, now) {
            moved.push(task.id);
        }
    }
    if moved.is_empty() {
        return Ok(moved);
    }
    let changed: Vec<Task> = tasks
        .into_iter()
        .filter(|task| moved.contains(&task.id))
        .collect();
    save_tasks(store, &changed)?;
    Ok(moved)
}

/// Read one plan.
pub fn load_plan(store: &dyn Store, id: PlanId) -> Result<Option<Plan>, StoreError> {
    let Some(raw) = store.get_kv(&id.storage_key())? else {
        return Ok(None);
    };
    match serde_json::from_str(&raw) {
        Ok(plan) => Ok(Some(plan)),
        Err(error) => {
            tracing::warn!(%id, %error, "plan row does not decode; treating it as absent");
            Ok(None)
        }
    }
}

/// Write one plan.
pub fn save_plan(store: &dyn Store, plan: &Plan) -> Result<(), StoreError> {
    let value = serde_json::to_string(plan)
        .map_err(|error| StoreError::Backend(format!("encode plan: {error}")))?;
    store.set_kv(&plan.id.storage_key(), &value)
}

/// Every plan.
pub fn all_plans(store: &dyn Store) -> Result<Vec<Plan>, StoreError> {
    let rows = store.list_kv_prefix(PLAN_KEY_PREFIX)?;
    Ok(rows
        .into_iter()
        .filter_map(|(_, value)| serde_json::from_str(&value).ok())
        .collect())
}

/// A plan's rolled-up progress and the workspaces its tasks point at — the
/// two numbers a TODO line shows and the member list a cross-repo plan
/// projects onto an `EpicRecord`.
///
/// The member order follows the store's key order (the tasks' uuids), not the
/// order they were created in. That is stable across reads, which is what the
/// projection needs; it is not creation order, so do not render it as one.
pub fn plan_status(
    store: &dyn Store,
    plan: PlanId,
) -> Result<(Progress, Vec<WorkspaceKey>), StoreError> {
    let (tasks, _) = all_tasks(store)?;
    Ok((
        lazybox_core::work::plan_progress(&tasks, plan),
        lazybox_core::work::plan_members(&tasks, plan),
    ))
}

fn encode_task(task: &Task) -> Result<String, StoreError> {
    serde_json::to_string(task)
        .map_err(|error| StoreError::Backend(format!("encode work row: {error}")))
}

/// The workspace a task's owner is, if it has an agent owner.
pub fn owner_workspace(task: &Task) -> Option<&WorkspaceKey> {
    task.owner.as_ref().and_then(party_workspace)
}

fn party_workspace(party: &Party) -> Option<&WorkspaceKey> {
    match party {
        Party::Agent { workspace, .. } => Some(workspace),
        Party::Human | Party::Lazybox => None,
    }
}

/// How often the stranded sweep runs. A minute keeps a dead agent's work from
/// reading as in-flight for long, and the scan is one kv prefix listing.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Fail work whose agent is gone, once per minute, forever.
///
/// Unconditional, unlike `session_reaper`: this writes a lifecycle field on
/// rows lazybox owns and destroys nothing — no process is killed, no
/// scrollback lost — so there is no data-loss reason for it to be opt-in.
pub fn spawn(config: &crate::ServerConfig) -> tokio::task::JoinHandle<()> {
    let config = config.clone();
    tokio::spawn(async move { run(config, SWEEP_INTERVAL, STRANDED_GRACE).await })
}

async fn run(
    config: crate::ServerConfig,
    interval: std::time::Duration,
    grace: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        // Snapshot liveness first, then sweep off it: the predicate has to be
        // synchronous to run inside the blocking store call, and a snapshot is
        // the honest shape anyway — "who was running when we looked".
        let live: std::collections::HashSet<WorkspaceKey> = config
            .terminal
            .agent_terminal_backends()
            .await
            .into_iter()
            .map(|(session, _)| WorkspaceKey::new(session.as_str()))
            .collect();
        let now = chrono::Utc::now();
        let store = config.store.clone();
        let swept = tokio::task::spawn_blocking(move || {
            sweep_stranded(&*store, now, grace, |key| live.contains(key))
        })
        .await;
        match swept {
            Ok(Ok(moved)) if !moved.is_empty() => tracing::warn!(
                moved = moved.len(),
                "work: failed units of work whose agent is gone"
            ),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::debug!(%error, "work: stranded sweep skipped"),
            Err(error) => tracing::warn!(%error, "work: stranded sweep panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lazybox_core::work::{Lifecycle, WorkResult};
    use lazybox_store::MemoryStore;

    fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    fn agent(key: &str) -> Party {
        Party::Agent {
            workspace: WorkspaceKey::new(key),
            session: None,
        }
    }

    fn task(title: &str, requester: Party) -> Task {
        Task::new(title, requester, at(0))
    }

    #[test]
    fn a_saved_task_round_trips_by_its_id() {
        let store = MemoryStore::default();
        let mut saved = task("wire the store", Party::Human);
        saved.brief = "objective · done · bounds".into();
        saved.owner = Some(agent("github:o/r#1"));
        save_task(&store, &saved).unwrap();

        let read = load_task(&store, saved.id).unwrap().expect("stored");
        assert_eq!(read, saved);
    }

    #[test]
    fn a_missing_task_reads_as_none_rather_than_an_error() {
        let store = MemoryStore::default();
        assert_eq!(load_task(&store, WorkId::new()).unwrap(), None);
    }

    #[test]
    fn an_undecodable_row_is_skipped_and_counted_not_fatal() {
        let store = MemoryStore::default();
        let good = task("real", Party::Human);
        save_task(&store, &good).unwrap();
        // A row a newer build wrote that this one cannot parse.
        store
            .set_kv(&format!("{WORK_KEY_PREFIX}not-a-task"), "{\"nope\":1}")
            .unwrap();

        let (tasks, skipped) = all_tasks(&store).unwrap();
        assert_eq!(skipped, 1, "the bad row is reported, not hidden");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, good.id);
    }

    #[test]
    fn work_and_plan_prefixes_do_not_read_each_others_rows() {
        let store = MemoryStore::default();
        save_task(&store, &task("t", Party::Human)).unwrap();
        save_plan(&store, &Plan::new("p")).unwrap();

        assert_eq!(all_tasks(&store).unwrap().0.len(), 1);
        assert_eq!(all_plans(&store).unwrap().len(), 1);
    }

    #[test]
    fn ownership_is_matched_on_the_workspace_not_the_session() {
        let store = MemoryStore::default();
        let mut owned = task("mine", Party::Human);
        // The session that was live when the row was written — a respawn
        // mints a new one, and the task must not vanish from the list.
        owned.owner = Some(Party::Agent {
            workspace: WorkspaceKey::new("github:o/r#7"),
            session: Some(lazybox_core::SessionId::new()),
        });
        save_task(&store, &owned).unwrap();
        let mut other = task("theirs", Party::Human);
        other.owner = Some(agent("github:o/r#8"));
        save_task(&store, &other).unwrap();

        let mine = tasks_owned_by(&store, &WorkspaceKey::new("github:o/r#7")).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, owned.id);
    }

    #[test]
    fn a_human_owner_belongs_to_no_workspace_list() {
        let store = MemoryStore::default();
        let mut mine = task("by hand", Party::Human);
        mine.owner = Some(Party::Human);
        save_task(&store, &mine).unwrap();

        assert!(
            tasks_owned_by(&store, &WorkspaceKey::new("github:o/r#7"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn requested_by_finds_what_a_workspace_is_waiting_on() {
        let store = MemoryStore::default();
        let mut asked = task("do this for me", agent("github:o/r#1"));
        asked.owner = Some(agent("github:o/r#2"));
        save_task(&store, &asked).unwrap();

        let waiting = tasks_requested_by(&store, &WorkspaceKey::new("github:o/r#1")).unwrap();
        assert_eq!(waiting.len(), 1);
        assert!(
            tasks_owned_by(&store, &WorkspaceKey::new("github:o/r#1"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_merge_ticks_off_every_task_linked_to_it_in_one_write() {
        let store = MemoryStore::default();
        let link = Link::Workspace(WorkspaceKey::new("github:o/r#42"));
        for title in ["a", "b"] {
            let mut t = task(title, Party::Human);
            t.links.push(link.clone());
            save_task(&store, &t).unwrap();
        }
        let mut unrelated = task("c", Party::Human);
        unrelated
            .links
            .push(Link::Url("https://example.test".into()));
        save_task(&store, &unrelated).unwrap();

        let moved = complete_linked_to(&store, &link, Party::Lazybox, at(10)).unwrap();
        assert_eq!(moved.len(), 2);

        let (tasks, _) = all_tasks(&store).unwrap();
        for t in &tasks {
            let expected = if t.id == unrelated.id {
                Lifecycle::Pending
            } else {
                Lifecycle::Completed
            };
            assert_eq!(t.lifecycle, expected, "{}", t.title);
        }
    }

    #[test]
    fn re_delivering_the_same_merge_moves_nothing_the_second_time() {
        let store = MemoryStore::default();
        let link = Link::Tracker(lazybox_core::TaskId {
            source: "github".into(),
            key: "o/r#42".into(),
        });
        let mut t = task("a", Party::Human);
        t.links.push(link.clone());
        save_task(&store, &t).unwrap();

        assert_eq!(
            complete_linked_to(&store, &link, Party::Lazybox, at(10))
                .unwrap()
                .len(),
            1
        );
        assert!(
            complete_linked_to(&store, &link, Party::Lazybox, at(20))
                .unwrap()
                .is_empty(),
            "the poller sees the same merge twice; the second is a no-op"
        );
        // And the completion it already recorded is untouched.
        let stored = load_task(&store, t.id).unwrap().unwrap();
        assert_eq!(stored.lifecycle, Lifecycle::Completed);
        assert_eq!(stored.history.len(), 2, "no second completion event");
    }

    #[test]
    fn an_exiting_agent_fails_only_its_own_underway_work() {
        let store = MemoryStore::default();
        let ws = WorkspaceKey::new("github:o/r#9");

        let mut underway = task("in flight", Party::Human);
        underway.owner = Some(agent("github:o/r#9"));
        underway
            .transition(Lifecycle::Underway, Party::Human, at(1))
            .unwrap();
        save_task(&store, &underway).unwrap();

        let mut pending = task("not started", Party::Human);
        pending.owner = Some(agent("github:o/r#9"));
        save_task(&store, &pending).unwrap();

        let mut done = task("already reported", Party::Human);
        done.owner = Some(agent("github:o/r#9"));
        done.transition(Lifecycle::Underway, Party::Human, at(1))
            .unwrap();
        done.complete(
            WorkResult {
                summary: "shipped".into(),
                artifacts: Vec::new(),
            },
            Party::Human,
            at(2),
        )
        .unwrap();
        save_task(&store, &done).unwrap();

        let mut sibling = task("someone else's", Party::Human);
        sibling.owner = Some(agent("github:o/r#10"));
        sibling
            .transition(Lifecycle::Underway, Party::Human, at(1))
            .unwrap();
        save_task(&store, &sibling).unwrap();

        let moved = fail_underway_for_exit(&store, &ws, Party::Lazybox, at(30)).unwrap();
        assert_eq!(moved, vec![underway.id]);

        assert!(matches!(
            load_task(&store, underway.id).unwrap().unwrap().lifecycle,
            Lifecycle::Failed { .. }
        ));
        assert_eq!(
            load_task(&store, pending.id).unwrap().unwrap().lifecycle,
            Lifecycle::Pending,
            "work never started is not work the exit failed"
        );
        assert_eq!(
            load_task(&store, done.id).unwrap().unwrap().lifecycle,
            Lifecycle::Completed,
            "a reported result survives its agent exiting"
        );
        assert_eq!(
            load_task(&store, sibling.id).unwrap().unwrap().lifecycle,
            Lifecycle::Underway,
            "one workspace's exit must not fail another's work"
        );
    }

    #[test]
    fn the_exit_failure_records_its_cause_in_history() {
        let store = MemoryStore::default();
        let mut t = task("in flight", Party::Human);
        t.owner = Some(agent("github:o/r#9"));
        t.transition(Lifecycle::Underway, Party::Human, at(1))
            .unwrap();
        save_task(&store, &t).unwrap();

        fail_underway_for_exit(
            &store,
            &WorkspaceKey::new("github:o/r#9"),
            Party::Lazybox,
            at(30),
        )
        .unwrap();

        let stored = load_task(&store, t.id).unwrap().unwrap();
        let last = stored.history.last().unwrap();
        assert!(
            last.change.contains("agent-exited"),
            "the cause is in history, not inferred later: {}",
            last.change
        );
        assert_eq!(last.by, Party::Lazybox);
    }

    #[test]
    fn plan_status_rolls_up_progress_and_the_workspaces_it_touches() {
        let store = MemoryStore::default();
        let plan = Plan::new("ship coordination v2");
        save_plan(&store, &plan).unwrap();

        let mut root = task("root", Party::Human);
        root.plan = Some(plan.id);
        root.links
            .push(Link::Workspace(WorkspaceKey::new("github:o/r#1")));
        save_task(&store, &root).unwrap();

        let mut child = task("child", Party::Human);
        child.plan = Some(plan.id);
        child.parent = Some(root.id);
        child
            .links
            .push(Link::Workspace(WorkspaceKey::new("github:o/other#2")));
        child
            .complete(
                WorkResult {
                    summary: "done".into(),
                    artifacts: Vec::new(),
                },
                Party::Human,
                at(5),
            )
            .unwrap();
        save_task(&store, &child).unwrap();

        let (progress, members) = plan_status(&store, plan.id).unwrap();
        assert_eq!((progress.done, progress.total), (1, 2));
        // Order follows the store's key order (uuid), not insertion — stable
        // across reads, which is what the epic projection needs, but not
        // something a caller may assume is creation order.
        let mut members: Vec<String> = members.iter().map(|k| k.to_string()).collect();
        members.sort();
        assert_eq!(
            members,
            vec!["github:o/other#2".to_string(), "github:o/r#1".to_string()],
            "a plan spanning repos is the member list an epic projects onto"
        );
    }

    #[test]
    fn deleting_a_parent_leaves_its_children_visible_rather_than_cascading() {
        let store = MemoryStore::default();
        let root = task("root", Party::Human);
        save_task(&store, &root).unwrap();
        let mut child = task("child", Party::Human);
        child.parent = Some(root.id);
        save_task(&store, &child).unwrap();

        delete_task(&store, root.id).unwrap();

        assert_eq!(load_task(&store, root.id).unwrap(), None);
        assert!(
            load_task(&store, child.id).unwrap().is_some(),
            "an orphan is visible and fixable; a silent cascade is not"
        );
    }

    /// The case that rules out hooking the teardown path directly.
    #[test]
    fn a_respawn_inside_the_grace_window_is_not_a_stranded_task() {
        let mut t = task("in flight", Party::Human);
        t.owner = Some(agent("github:o/r#1"));
        t.transition(Lifecycle::Underway, Party::Human, at(0))
            .unwrap();

        let no_agent = |_: &WorkspaceKey| false;
        // Shift-K killed the agent a minute ago and the respawn has not
        // registered yet: the workspace has no live agent, and the work is
        // still very much in flight.
        assert!(!is_stranded(&t, at(60), STRANDED_GRACE, &no_agent));
        // Past the window with still nothing running, it is stranded.
        assert!(is_stranded(&t, at(601), STRANDED_GRACE, &no_agent));
    }

    #[test]
    fn a_live_agent_is_never_stranded_however_long_it_has_been_quiet() {
        let mut t = task("long job", Party::Human);
        t.owner = Some(agent("github:o/r#1"));
        t.transition(Lifecycle::Underway, Party::Human, at(0))
            .unwrap();

        let live = |_: &WorkspaceKey| true;
        assert!(!is_stranded(&t, at(86_400), STRANDED_GRACE, &live));
    }

    #[test]
    fn human_owned_work_is_not_stranded_by_having_no_agent() {
        let mut t = task("I will do it myself", Party::Human);
        t.owner = Some(Party::Human);
        t.transition(Lifecycle::Underway, Party::Human, at(0))
            .unwrap();

        assert!(!is_stranded(
            &t,
            at(86_400),
            STRANDED_GRACE,
            &(|_: &WorkspaceKey| false)
        ));
    }

    #[test]
    fn only_underway_work_can_strand() {
        for lifecycle in [
            Lifecycle::Pending,
            Lifecycle::AwaitingAnswer {
                question: "which?".into(),
            },
            Lifecycle::Held {
                reason: "waiting".into(),
            },
            Lifecycle::Completed,
        ] {
            let mut t = task("x", Party::Human);
            t.owner = Some(agent("github:o/r#1"));
            // Set it directly: `Completed` would refuse a later transition,
            // and the point here is the predicate, not the model's gate.
            t.lifecycle = lifecycle.clone();
            t.history.push(lazybox_core::work::WorkEvent {
                at: at(0),
                by: Party::Human,
                change: "set".into(),
            });
            assert!(
                !is_stranded(&t, at(86_400), STRANDED_GRACE, &(|_: &WorkspaceKey| false)),
                "{} must not strand",
                lifecycle.label()
            );
        }
    }

    #[test]
    fn the_sweep_fails_stranded_work_and_leaves_the_rest() {
        let store = MemoryStore::default();

        let mut stranded = task("abandoned", Party::Human);
        stranded.owner = Some(agent("github:o/r#1"));
        stranded
            .transition(Lifecycle::Underway, Party::Human, at(0))
            .unwrap();
        save_task(&store, &stranded).unwrap();

        let mut live = task("still running", Party::Human);
        live.owner = Some(agent("github:o/r#2"));
        live.transition(Lifecycle::Underway, Party::Human, at(0))
            .unwrap();
        save_task(&store, &live).unwrap();

        let moved = sweep_stranded(&store, at(601), STRANDED_GRACE, |ws| {
            ws == &WorkspaceKey::new("github:o/r#2")
        })
        .unwrap();
        assert_eq!(moved, vec![stranded.id]);

        assert!(matches!(
            load_task(&store, stranded.id).unwrap().unwrap().lifecycle,
            Lifecycle::Failed { .. }
        ));
        assert_eq!(
            load_task(&store, live.id).unwrap().unwrap().lifecycle,
            Lifecycle::Underway
        );
    }

    #[test]
    fn the_sweep_is_idempotent_so_a_second_tick_writes_nothing() {
        let store = MemoryStore::default();
        let mut t = task("abandoned", Party::Human);
        t.owner = Some(agent("github:o/r#1"));
        t.transition(Lifecycle::Underway, Party::Human, at(0))
            .unwrap();
        save_task(&store, &t).unwrap();

        let dead = |_: &WorkspaceKey| false;
        assert_eq!(
            sweep_stranded(&store, at(601), STRANDED_GRACE, dead)
                .unwrap()
                .len(),
            1
        );
        assert!(
            sweep_stranded(&store, at(1200), STRANDED_GRACE, dead)
                .unwrap()
                .is_empty(),
            "a failed task is terminal; the next tick must not touch it"
        );
        assert_eq!(
            load_task(&store, t.id).unwrap().unwrap().history.len(),
            3,
            "created, underway, failed — and no second failure"
        );
    }

    #[test]
    fn an_empty_batch_write_is_a_no_op_not_an_unsupported_error() {
        let store = MemoryStore::default();
        assert!(save_tasks(&store, &[]).is_ok());
    }
}
