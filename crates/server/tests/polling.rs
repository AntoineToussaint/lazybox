//! Tests for `polling::tick` / `polling::upsert` / bus integration.
//!
//! These exercise the contract:
//! 1. tick(source) → upsert(each task) → SessionUpserted broadcast.
//! 2. Read state (seen_count, read_indices, last_viewed_at,
//!    snoozed_until) is preserved across updates from the same task_id.
//! 3. Source errors surface as `Event::ProviderError` events; one bad
//!    source doesn't poison the others.
//! 4. The bus reaches a client connected through `Server::serve`.
//!
//! Per-test timeout is enforced by nextest's process-level
//! slow-timeout (10s, see `.config/nextest.toml`) — body-wrapping
//! every test in `tokio::time::timeout(D, async move { … })` is
//! an anti-pattern because it masks which await actually hung.
//! For per-await bounded waits, use
//! `tokio::time::timeout(2s, channel.recv())` on the specific
//! `await` that could block; the rest of the body runs without
//! an outer wall-clock cap. See `feedback_test_timeouts.md`.

use chrono::Utc;
use lazybox_core::{
    Activity, ActivityKind, CiStatus, ProviderConfig, ReviewStatus, Task, TaskId, TaskRole,
    TaskState,
};
use lazybox_ipc::{Command, Event, channel};
use lazybox_server::backend::{MockBackend, SessionBackend};
use lazybox_server::polling::{self, FetchMode, TaskSource};
use lazybox_server::{Server, ServerConfig};
use lazybox_store::{MemoryStore, Store, StoreError, StoreMutation, WorkspaceRecord};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Fault-injection backend: every ordinary operation delegates to the
/// production-faithful MemoryStore, while the next atomic batch can be made
/// to fail before applying anything. This pins the daemon's commit contract:
/// failed durability must never leak as a successful bus projection.
struct FailingBatchStore {
    inner: MemoryStore,
    fail_next: std::sync::atomic::AtomicBool,
}

impl FailingBatchStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_next: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn fail_next_batch(&self) {
        self.fail_next.store(true, Ordering::SeqCst);
    }
}

impl Store for FailingBatchStore {
    fn apply_batch(&self, mutations: &[StoreMutation]) -> Result<(), StoreError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(StoreError::Backend("injected batch failure".into()));
        }
        self.inner.apply_batch(mutations)
    }

    fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.inner.get_kv(key)
    }

    fn set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.inner.set_kv(key, value)
    }

    fn delete_kv(&self, key: &str) -> Result<(), StoreError> {
        self.inner.delete_kv(key)
    }

    fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, StoreError> {
        self.inner.list_workspaces()
    }

    fn list_projects(&self) -> Result<Vec<lazybox_store::ProjectRecord>, StoreError> {
        self.inner.list_projects()
    }
}

/// Parks one atomic batch after the blocking owner has started. Tests can
/// cancel the async caller at that exact boundary and verify the detached
/// owner still finishes durability plus every in-memory/event projection.
struct GatedBatchStore {
    inner: MemoryStore,
    armed: std::sync::atomic::AtomicBool,
    entered_tx: parking_lot::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release_rx: parking_lot::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Store for GatedBatchStore {
    fn apply_batch(&self, mutations: &[StoreMutation]) -> Result<(), StoreError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            if let Some(tx) = self.entered_tx.lock().take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release_rx.lock().take() {
                let _ = rx.recv_timeout(Duration::from_secs(10));
            }
        }
        self.inner.apply_batch(mutations)
    }

    fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.inner.get_kv(key)
    }

    fn set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.inner.set_kv(key, value)
    }

    fn delete_kv(&self, key: &str) -> Result<(), StoreError> {
        self.inner.delete_kv(key)
    }

    fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, StoreError> {
        self.inner.list_workspaces()
    }

    fn list_projects(&self) -> Result<Vec<lazybox_store::ProjectRecord>, StoreError> {
        self.inner.list_projects()
    }
}

/// Wait for the next `WorkspaceUpserted` event, ignoring any
/// `ProjectUpserted` (registered the first time polling sees a new
/// repo). Tests that drain "one event per workspace upsert" can use
/// this instead of `client.recv()` to stay robust to the Project
/// auto-register firing alongside the workspace.
async fn recv_workspace_upsert(client: &mut lazybox_ipc::Client) -> Event {
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(2), client.recv())
            .await
            .expect("client recv timeout")
            .expect("event");
        if !matches!(evt, Event::ProjectUpserted(_)) {
            return evt;
        }
    }
}

fn make_task(key: &str) -> Task {
    // The URL must contain `/pull/` for `Workspace::classify` to put
    // this task in the workspace's PR slot — otherwise it lands as
    // a GhIssue and the assertions on `workspace.pr` fail.
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
    Task {
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: format!("PR {key}"),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Reviewer,
        ci: CiStatus::Success,
        review: ReviewStatus::Pending,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{path}/pull/{num}"),
        repo: Some("o/r".into()),
        branch: None,
        base_branch: None,
        updated_at: Utc::now(),
        created_at: None,
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Mergeable,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
    }
}

fn make_activity(author: &str, body: &str) -> Activity {
    Activity {
        author: author.into(),
        body: body.into(),
        created_at: Utc::now(),
        kind: ActivityKind::Comment,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    }
}

// ── Test fixtures ──────────────────────────────────────────────────

struct FakeSource {
    name: String,
    tasks: Vec<Task>,
}

impl TaskSource for FakeSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        let tasks = self.tasks.clone();
        Box::pin(async move { Ok(tasks) })
    }
}

struct FailingSource(String);

impl TaskSource for FailingSource {
    fn name(&self) -> &str {
        &self.0
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(lazybox_core::ProviderError::retryable(
                self.0.clone(),
                "rate limited",
            ))
        })
    }
}

struct CountingSource {
    name: String,
    counter: Arc<AtomicUsize>,
}

impl TaskSource for CountingSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        let counter = self.counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        })
    }
}

/// Source that returns a fixed task set AND queues an
/// `AutoSpawnAgent` action to be drained after the upsert pass. The
/// in-process equivalent of `GhSource`'s `@lazybox`-mention path
/// without the network round-trip — used to verify that
/// `tick_with_state` runs `drain_actions` and routes the spawn
/// through `handle_spawn` end-to-end.
struct ActionEmittingSource {
    name: String,
    tasks: Vec<Task>,
    actions: std::sync::Mutex<Vec<polling::ProviderAction>>,
}

impl TaskSource for ActionEmittingSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        let tasks = self.tasks.clone();
        Box::pin(async move { Ok(tasks) })
    }
    fn drain_actions(&self) -> Vec<polling::ProviderAction> {
        std::mem::take(&mut *self.actions.lock().unwrap())
    }
}

/// Fake source that returns a fixed task set AND advertises a
/// caller-specified `PolledScope`. Used by rescope tests that want
/// to exercise partial-coverage behavior end-to-end through
/// `tick_with_state` instead of building `TickOutcome` by hand.
struct ScopedSource {
    name: String,
    tasks: Vec<Task>,
    scope: polling::PolledScope,
}

impl TaskSource for ScopedSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        let tasks = self.tasks.clone();
        Box::pin(async move { Ok(tasks) })
    }
    fn polled_scope(&self) -> polling::PolledScope {
        self.scope.clone()
    }
}

/// A source that returns its task list AND reports the fetch as
/// incremental. Stand-in for `GhSource`'s notifications-driven fast
/// path in tests that exercise the rescope-skip behavior without
/// needing a real GitHub token.
struct IncrementalSource {
    name: String,
    tasks: Vec<Task>,
}

impl TaskSource for IncrementalSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        let tasks = self.tasks.clone();
        Box::pin(async move { Ok(tasks) })
    }
    fn last_fetch_kind(&self) -> FetchMode {
        FetchMode::Incremental
    }
}

// ── serve-loop responsiveness during a poll ─────────────────────────

/// Regression: the daemon serve loop must never freeze while a GitHub
/// poll is in flight.
///
/// `set_focused_workspace` runs INLINE on the single-task serve loop
/// for every `FocusWorkspace` / `MarkRead` (i.e. every sidebar
/// navigation). Before the fix this handler did `poll_state.lock()
/// .await`, so while a poll held `poll_state` it blocked the serve loop
/// for the whole fetch, queueing every keystroke `Write` and `Spawn`
/// behind it: "can't type in the agent / can't start a shell while
/// GitHub syncs, then it unblocks when the sync finishes." (Modal input
/// kept working because it never leaves the TUI.)
///
/// A tick now checks `poll_state` OUT rather than holding it across the
/// cycle (#133), so it is held only in sub-millisecond windows — but
/// the handler must STILL be non-blocking under any holder, which
/// `try_lock` guarantees. This test holds the guard manually to stand
/// in for one of those windows.
#[tokio::test]
async fn set_focused_workspace_never_blocks_on_an_in_flight_poll() {
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#42");
    let workspace = lazybox_core::Workspace::from_task(task.clone(), Utc::now());
    polling::upsert(&config, task).await;

    // Simulate an in-flight poll tick holding the lock across its
    // network fetch, exactly like `run_one_tick` does.
    let held = config.poll_state.lock().await;

    // The inline handler must return promptly rather than wait on the
    // held lock. Pre-fix this hangs until `held` drops; the 2s timeout
    // turns that hang into a clean failure instead of a stuck test.
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        polling::set_focused_workspace(&config, &workspace.key),
    )
    .await;
    assert!(
        res.is_ok(),
        "set_focused_workspace blocked while a poll held poll_state — \
         the 'frozen during GitHub sync' regression is back"
    );

    // The hint was skipped (not silently applied) while the lock was
    // held: focused_repo is still unset.
    assert!(
        held.round_robin.focused_repo.is_none(),
        "focus hint must be skipped, not applied, while a poll holds the lock"
    );
}

// ── tick() / upsert() ───────────────────────────────────────────────

#[tokio::test]
async fn tick_broadcasts_session_upserted_for_each_task() {
    let config = ServerConfig::in_memory();
    let mut bus_rx = config.bus.subscribe();

    let source: Box<dyn TaskSource> = Box::new(FakeSource {
        name: "github".into(),
        tasks: vec![make_task("o/r#1"), make_task("o/r#2")],
    });
    polling::tick(&config, &[source]).await;

    let mut keys = Vec::new();
    while let Ok(evt) = bus_rx.try_recv() {
        if let Event::WorkspaceUpserted(w) = evt {
            // Each PR projects to one workspace whose pr.id.key matches
            // the originating task key. The wire contract is that the
            // poller emits Workspace events, never Session events.
            keys.push(w.pr.as_ref().unwrap().id.key.clone());
        }
    }
    keys.sort();
    assert_eq!(keys, vec!["o/r#1", "o/r#2"]);
}
#[tokio::test]
async fn upsert_persists_to_store_so_subscribe_can_replay_it() {
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#42")).await;

    // Now connect a client via channel::pair and Subscribe — the
    // Snapshot should include the just-upserted session.
    let (mut client, server) = channel::pair();
    let serve_config = config.clone();
    tokio::spawn(async move {
        Server::new(serve_config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    let evt = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("daemon replies")
        .expect("event");
    match evt {
        Event::Snapshot { workspaces, .. } => {
            assert_eq!(workspaces.len(), 1);
            assert_eq!(workspaces[0].pr.as_ref().unwrap().id.key, "o/r#42");
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}
#[tokio::test]
async fn upsert_preserves_seen_count_across_updates() {
    // The user marked the workspace read; the poller mustn't wipe
    // that out when GitHub returns the same PR again.
    let config = ServerConfig::in_memory();

    // Seed a workspace with seen_count=5 in the store directly.
    let task = make_task("o/r#7");
    let mut workspace = lazybox_core::Workspace::from_task(task.clone(), Utc::now());
    workspace.seen_count = 5;
    let json = serde_json::to_string(&workspace).unwrap();
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace.key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(json),
        })
        .unwrap();

    // Poll re-discovers the same task. Read state must survive.
    polling::upsert(&config, task).await;

    let stored = config.store.get_workspace(&workspace.key).unwrap().unwrap();
    let parsed: lazybox_core::Workspace =
        serde_json::from_str(&stored.workspace_json.unwrap()).unwrap();
    assert_eq!(parsed.seen_count, 5, "seen_count preserved");
}
#[tokio::test]
async fn upsert_de_duplicates_recent_activity() {
    // Provider returns the same activity entry on every poll. Without
    // de-dup, every tick would push another copy onto session.activity
    // and the unread-count would explode.
    let config = ServerConfig::in_memory();

    let activity_at = Utc::now();
    let mk = || {
        let mut t = make_task("o/r#1");
        t.recent_activity = vec![Activity {
            author: "alice".into(),
            body: "lgtm".into(),
            created_at: activity_at,
            kind: ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }];
        t
    };
    polling::upsert(&config, mk()).await;
    polling::upsert(&config, mk()).await;
    polling::upsert(&config, mk()).await;

    // Compute the workspace key the same way the poller does, then
    // round-trip through the store and verify the activity feed
    // didn't grow on every poll.
    let key = lazybox_core::WorkspaceKey::new(lazybox_core::workspace_key_for(&mk()));
    let stored = config.store.get_workspace(&key).unwrap().unwrap();
    let workspace: lazybox_core::Workspace =
        serde_json::from_str(&stored.workspace_json.unwrap()).unwrap();
    assert_eq!(workspace.activity.len(), 1, "activity de-duplicated");
}
#[tokio::test]
async fn tick_emits_provider_error_on_failure() {
    let config = ServerConfig::in_memory();
    let mut bus_rx = config.bus.subscribe();

    let bad: Box<dyn TaskSource> = Box::new(FailingSource("github".into()));
    polling::tick(&config, &[bad]).await;

    let evt = bus_rx.try_recv().expect("error broadcasted");
    match evt {
        Event::ProviderError {
            source, message, ..
        } => {
            assert_eq!(source, "github");
            // Message on the bus is the user-facing one (terse) —
            // full diagnostic lives in /tmp/lazybox.log. For a
            // Retryable error the user_message format is
            // "<source> hiccup, retrying next cycle".
            assert!(
                message.contains("hiccup") || message.contains("retrying"),
                "expected terse retryable user_message, got {message}"
            );
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}
#[tokio::test]
async fn tick_continues_after_one_source_fails() {
    let config = ServerConfig::in_memory();
    let mut bus_rx = config.bus.subscribe();

    let bad: Box<dyn TaskSource> = Box::new(FailingSource("github".into()));
    let good: Box<dyn TaskSource> = Box::new(FakeSource {
        name: "linear".into(),
        tasks: vec![make_task("ENG-1")],
    });
    polling::tick(&config, &[bad, good]).await;

    let mut had_upsert = false;
    let mut had_error = false;
    while let Ok(evt) = bus_rx.try_recv() {
        match evt {
            Event::WorkspaceUpserted(_) => had_upsert = true,
            Event::ProviderError { .. } => had_error = true,
            _ => {}
        }
    }
    assert!(had_error, "failure broadcast");
    assert!(had_upsert, "successful source still ran");
}
// ── mark_workspace_read ──────────────────────────────────────────────
//
// Activity-seen state is local to the user — independent of provider
// state. mark_workspace_read flips every known activity item to read,
// persists, and broadcasts so the right pane re-renders without a
// pending unread badge.

#[tokio::test]
async fn mark_workspace_read_persists_seen_count() {
    let config = ServerConfig::in_memory();

    // Seed a workspace with three activity items, none read.
    let mut task = make_task("o/r#11");
    task.recent_activity = vec![
        make_activity("alice", "first"),
        make_activity("bob", "second"),
        make_activity("carol", "third"),
    ];
    polling::upsert(&config, task).await;

    let key =
        lazybox_core::WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#11")));
    let before: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(before.activity.len(), 3);
    assert_eq!(before.unread_count(), 3, "everything unread initially");

    polling::mark_workspace_read(&config, &key).await;

    let after: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(after.unread_count(), 0, "everything read after mark");
    assert_eq!(after.seen_count, 3, "seen_count bumped to activity len");
    assert!(after.last_viewed_at.is_some(), "last_viewed stamped");
}
#[tokio::test]
async fn mark_workspace_read_broadcasts_upsert() {
    let config = ServerConfig::in_memory();
    let (mut client, server) = channel::pair();
    let serve_config = config.clone();
    tokio::spawn(async move {
        Server::new(serve_config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    let _snap = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();

    let mut task = make_task("o/r#22");
    task.recent_activity = vec![make_activity("alice", "hi-broadcast")];
    polling::upsert(&config, task).await;
    // Drain the workspace upsert from the initial seed. Skips any
    // ProjectUpserted event the first-sight registration fires.
    let _seed = recv_workspace_upsert(&mut client).await;

    let key =
        lazybox_core::WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#22")));
    polling::mark_workspace_read(&config, &key).await;

    let evt = recv_workspace_upsert(&mut client).await;
    match evt {
        Event::WorkspaceUpserted(w) => {
            assert_eq!(w.unread_count(), 0, "broadcast workspace is read");
        }
        other => panic!("expected WorkspaceUpserted, got {other:?}"),
    }
}
#[tokio::test]
async fn mark_workspace_read_is_independent_of_provider_state() {
    // Marking read is purely a local user gesture — no provider
    // metadata changes. After re-polling the same task, seen state
    // must survive (the upsert path preserves seen_count).
    let config = ServerConfig::in_memory();
    let mut task = make_task("o/r#33");
    task.recent_activity = vec![make_activity("alice", "ping")];
    polling::upsert(&config, task.clone()).await;

    let key =
        lazybox_core::WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#33")));
    polling::mark_workspace_read(&config, &key).await;

    // Re-poll the same task — seen state survives.
    polling::upsert(&config, task).await;

    let stored: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.unread_count(), 0, "still read after re-poll");
}
#[tokio::test]
async fn mark_workspace_read_no_op_when_workspace_missing() {
    // Pressing `m` on a workspace that the daemon doesn't actually have
    // (race: TUI saw a stale snapshot) must not panic.
    let config = ServerConfig::in_memory();
    let key = lazybox_core::WorkspaceKey::new("github:o/r#nope");
    polling::mark_workspace_read(&config, &key).await;
    assert!(config.store.get_workspace(&key).unwrap().is_none());
}
// ── PR-attach migration ──────────────────────────────────────────────
//
// `migrate_session_paths_if_needed` walks the workspace's sessions
// and moves any whose persisted `worktree_path` no longer matches
// what the current slug would produce. The git-side `worktree move`
// needs a real bare clone to test honestly; these cover the
// orthogonal "path doesn't exist on disk" branch where the migration
// rewrites the record without doing I/O.

#[tokio::test]
async fn migrate_path_only_when_dir_missing() {
    use lazybox_core::WorkspaceSession;
    let root = tempfile::tempdir().unwrap();
    let task = make_task("o/r#11");
    let mut ws = lazybox_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        lazybox_core::SessionKind::Shell,
        std::path::PathBuf::from("/tmp/lazybox-nonexistent-old-path"),
        Utc::now(),
    );
    ws.add_session(session);

    let moved =
        lazybox_server::spawn_handler::migrate_session_paths_if_needed_under(&mut ws, root.path())
            .await;
    assert!(moved, "stale path detected → migrated record");

    let expected =
        lazybox_server::spawn_handler::worktree_path_for_session_under(&ws, 0, root.path());
    assert_eq!(
        ws.sessions[0].worktree_path, expected,
        "session path now matches the slug-derived path"
    );
}
#[tokio::test]
async fn migrate_no_op_when_path_already_matches() {
    use lazybox_core::WorkspaceSession;
    let root = tempfile::tempdir().unwrap();
    let task = make_task("o/r#22");
    let mut ws = lazybox_core::Workspace::from_task(task, Utc::now());
    let expected =
        lazybox_server::spawn_handler::worktree_path_for_session_under(&ws, 0, root.path());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        lazybox_core::SessionKind::Shell,
        expected.clone(),
        Utc::now(),
    );
    ws.add_session(session);

    let moved =
        lazybox_server::spawn_handler::migrate_session_paths_if_needed_under(&mut ws, root.path())
            .await;
    assert!(!moved, "path already matches → migration is a no-op");
    assert_eq!(ws.sessions[0].worktree_path, expected);
}
#[tokio::test]
async fn migrate_handles_zero_sessions() {
    let task = make_task("o/r#33");
    let mut ws = lazybox_core::Workspace::from_task(task, Utc::now());
    let moved = lazybox_server::spawn_handler::migrate_session_paths_if_needed(&mut ws).await;
    assert!(!moved, "no sessions → nothing to migrate");
}
#[tokio::test]
async fn migrate_picks_up_pr_title_rename() {
    // Regression for the deferred concern flagged in the
    // worktree-slug stability tests: when a PR's title is edited
    // upstream, `worktree_slug()` returns a new string, and the
    // session's persisted `worktree_path` no longer matches.
    //
    // Path: this scenario falls into the "dir doesn't exist" branch
    // (we never created the old folder in this test — just put a
    // bogus path on the session record). Migration must rewrite
    // the record to the new slug-derived path.
    //
    // When the old folder DOES exist on disk the worktree is reused in
    // place instead — see `migrate_reuses_live_worktree_in_place_on_slug_change`.
    use lazybox_core::WorkspaceSession;
    let root = tempfile::tempdir().unwrap();
    // Build the workspace with the ORIGINAL title.
    let mut task = make_task("o/r#7413");
    task.title = "Initial draft".into();
    let mut ws = lazybox_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        lazybox_core::SessionKind::Shell,
        // Use a path that DOES match the original slug so we can
        // assert it changes after the rename — same fallback the
        // production spawn handler uses.
        lazybox_server::spawn_handler::worktree_path_for_session_under(&ws, 0, root.path()),
        Utc::now(),
    );
    let original_slug = ws.worktree_slug();
    let original_path = session.worktree_path.clone();
    ws.add_session(session);

    // PR's title is renamed upstream. The next poll attaches a
    // task with the same PR number but a new title — exactly what
    // `prepare_upsert` does in production.
    let mut renamed = make_task("o/r#7413");
    renamed.title = "Propagate status code into FatalStreamError".into();
    ws.attach_task(renamed);

    // Sanity-check our fixture: the slug actually changed.
    let renamed_slug = ws.worktree_slug();
    assert_ne!(
        original_slug, renamed_slug,
        "fixture must trigger the rename"
    );
    assert!(renamed_slug.starts_with("PR-7413-"));

    let moved =
        lazybox_server::spawn_handler::migrate_session_paths_if_needed_under(&mut ws, root.path())
            .await;
    assert!(moved, "rename must trigger migration");

    let expected =
        lazybox_server::spawn_handler::worktree_path_for_session_under(&ws, 0, root.path());
    assert!(
        expected.ends_with(&renamed_slug),
        "expected path uses the renamed slug",
    );
    assert_eq!(
        ws.sessions[0].worktree_path, expected,
        "session path follows the new slug after migration",
    );
    assert_ne!(
        ws.sessions[0].worktree_path, original_path,
        "session path actually changed (not a no-op)",
    );
}
#[tokio::test]
async fn migrate_reuses_live_worktree_in_place_on_slug_change() {
    // The issue→PR absorb (and an upstream PR-title edit) leaves a
    // session attached to a workspace whose slug differs from the
    // directory its worktree already lives in. A real, on-disk worktree
    // must be REUSED IN PLACE — never `git worktree move`d to chase the
    // new slug. Renaming it would pull the working directory out from
    // under the agent/shell running inside and lose the session: the
    // long-standing "session lost on merge" bug (#78/#161/#167). This is
    // the regression guard for that.
    use lazybox_core::WorkspaceSession;

    // A real worktree on disk is just a directory containing a `.git`
    // entry — all `migrate_session_paths_if_needed` inspects to tell a
    // live worktree from a stale record or a V1 leftover.
    let live = tempfile::tempdir().unwrap();
    std::fs::create_dir(live.path().join(".git")).unwrap();
    let live_path = live.path().to_path_buf();

    // Workspace under its ORIGINAL title, session pointed at the
    // worktree that already exists on disk.
    let mut task = make_task("o/r#9001");
    task.title = "Initial draft".into();
    let mut ws = lazybox_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        lazybox_core::SessionKind::Shell,
        live_path.clone(),
        Utc::now(),
    );
    ws.add_session(session);

    // Slug changes under the live worktree (title edited upstream), so
    // the slug-derived path no longer matches the on-disk one.
    let mut renamed = make_task("o/r#9001");
    renamed.title = "Reuse the worktree in place".into();
    ws.attach_task(renamed);
    let slug_path = lazybox_server::spawn_handler::worktree_path_for_session(&ws, 0);
    assert_ne!(
        slug_path, live_path,
        "fixture must make the slug path differ from the live worktree"
    );

    let moved = lazybox_server::spawn_handler::migrate_session_paths_if_needed(&mut ws).await;

    assert!(
        !moved,
        "a live worktree is reused in place — no record rewrite"
    );
    assert_eq!(
        ws.sessions[0].worktree_path, live_path,
        "session keeps its existing worktree, not the slug-derived path"
    );
    assert!(
        live_path.exists(),
        "the worktree directory is left untouched on disk"
    );
}
#[tokio::test]
async fn migrate_reuses_both_worktrees_when_pr_absorbs_an_issue_session() {
    // The real issue→PR shape: the PR workspace already owns a session
    // (its own worktree at the base slug) and then absorbs the issue's
    // live session. Sorted by `created_at`, the absorbed (older) issue
    // session takes slot 0 — whose slug-derived path is the base slug
    // the PR's own session already occupies. The OLD git-move code would
    // have tried to relocate the issue worktree onto that occupied dir
    // (collision) and shuffle the PR worktree to `-2`, yanking CWD out
    // from under both agents. Reuse-in-place must leave BOTH untouched.
    use lazybox_core::WorkspaceSession;

    let issue_wt = tempfile::tempdir().unwrap();
    std::fs::create_dir(issue_wt.path().join(".git")).unwrap();
    let issue_path = issue_wt.path().to_path_buf();

    let pr_wt = tempfile::tempdir().unwrap();
    std::fs::create_dir(pr_wt.path().join(".git")).unwrap();
    let pr_path = pr_wt.path().to_path_buf();

    let mut task = make_task("o/r#9100");
    task.title = "Absorb the issue".into();
    let mut ws = lazybox_core::Workspace::from_task(task, Utc::now());

    // Older session first (the issue's, moved onto the PR), then the
    // PR's own. Distinct timestamps fix the slot order deterministically.
    let t0 = Utc::now();
    let absorbed = WorkspaceSession::new(
        ws.key.clone(),
        lazybox_core::SessionKind::Shell,
        issue_path.clone(),
        t0,
    );
    let pr_own = WorkspaceSession::new(
        ws.key.clone(),
        lazybox_core::SessionKind::Shell,
        pr_path.clone(),
        t0 + chrono::Duration::seconds(1),
    );
    let absorbed_id = ws.add_session(absorbed);
    let pr_own_id = ws.add_session(pr_own);

    let moved = lazybox_server::spawn_handler::migrate_session_paths_if_needed(&mut ws).await;

    assert!(!moved, "two live worktrees are both reused in place");
    assert_eq!(
        ws.find_session(absorbed_id).unwrap().worktree_path,
        issue_path,
        "absorbed issue session keeps its own worktree (no collision)"
    );
    assert_eq!(
        ws.find_session(pr_own_id).unwrap().worktree_path,
        pr_path,
        "PR's own session keeps its worktree (not shuffled to -2)"
    );
    assert!(issue_path.exists() && pr_path.exists());
}
// ── Create empty workspace (n key flow) ──────────────────────────────

#[tokio::test]
async fn create_empty_workspace_persists_with_user_name() {
    let config = ServerConfig::in_memory();
    let key = polling::create_empty_workspace(
        &config,
        "fix login flow",
        lazybox_core::ProjectKey::local("test"),
    );
    assert_eq!(
        key.as_str(),
        "fix-login-flow",
        "workspace key is the slugified name"
    );
    let stored: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.name, "fix login flow", "human-readable name kept");
    assert!(stored.pr.is_none(), "pre-PR workspace has no PR");
}
#[tokio::test]
async fn create_empty_workspace_disambiguates_collisions() {
    let config = ServerConfig::in_memory();
    let k1 = polling::create_empty_workspace(
        &config,
        "Refactor auth",
        lazybox_core::ProjectKey::local("test"),
    );
    let k2 = polling::create_empty_workspace(
        &config,
        "Refactor auth",
        lazybox_core::ProjectKey::local("test"),
    );
    let k3 = polling::create_empty_workspace(
        &config,
        "Refactor auth",
        lazybox_core::ProjectKey::local("test"),
    );
    assert_eq!(k1.as_str(), "refactor-auth");
    assert_eq!(k2.as_str(), "refactor-auth-2");
    assert_eq!(k3.as_str(), "refactor-auth-3");
}
#[tokio::test]
async fn create_empty_workspace_falls_back_when_name_is_unsluggable() {
    let config = ServerConfig::in_memory();
    let k =
        polling::create_empty_workspace(&config, "🚀✨", lazybox_core::ProjectKey::local("test"));
    assert_eq!(
        k.as_str(),
        "workspace",
        "fallback slug is 'workspace' when name has no alnum chars"
    );
}
#[tokio::test]
async fn create_empty_workspace_broadcasts_upserted() {
    let config = ServerConfig::in_memory();
    let (mut client, server) = channel::pair();
    let serve_config = config.clone();
    tokio::spawn(async move {
        Server::new(serve_config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    let _snap = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();

    polling::create_empty_workspace(
        &config,
        "side experiment",
        lazybox_core::ProjectKey::local("test"),
    );
    let evt = recv_workspace_upsert(&mut client).await;
    match evt {
        Event::WorkspaceUpserted(w) => {
            assert_eq!(w.name, "side experiment");
        }
        other => panic!("expected WorkspaceUpserted, got {other:?}"),
    }
}
// ── Legacy sandbox migration ─────────────────────────────────────────

#[tokio::test]
async fn migrate_legacy_sandbox_stamps_project_key() {
    let config = ServerConfig::in_memory();
    // Seed the pre-Stage-1 state: a workspace at key `sandbox`
    // with no `project_key` field.
    let key = lazybox_core::WorkspaceKey::new("sandbox");
    let workspace = lazybox_core::Workspace::empty(key.clone(), "main", Utc::now());
    assert!(workspace.project_key.is_none());
    let record = lazybox_store::WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    config.store.save_workspace(&record).unwrap();

    polling::migrate_legacy_sandbox(&config);

    // Workspace now carries the local-sandbox project key.
    let migrated: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        migrated.project_key,
        Some(lazybox_core::ProjectKey::local("sandbox"))
    );
    // And the project record was registered.
    let project_key = lazybox_core::ProjectKey::local("sandbox");
    assert!(config.store.get_project(&project_key).unwrap().is_some());
}

#[tokio::test]
async fn migrate_legacy_sandbox_is_idempotent() {
    let config = ServerConfig::in_memory();
    // No legacy workspace at all → first call is a no-op, second
    // call is also a no-op. Daemon startup runs unconditionally, so
    // we exercise the empty-store path explicitly.
    polling::migrate_legacy_sandbox(&config);
    polling::migrate_legacy_sandbox(&config);
    assert!(config.store.list_projects().unwrap().is_empty());
}

#[tokio::test]
async fn migrate_legacy_sandbox_skips_already_migrated_workspaces() {
    let config = ServerConfig::in_memory();
    // A sandbox workspace that ALREADY has a project_key — must not
    // be touched by the migration (its project_key stays, no new
    // project is created).
    let key = lazybox_core::WorkspaceKey::new("sandbox");
    let mut workspace = lazybox_core::Workspace::empty(key.clone(), "main", Utc::now());
    workspace.project_key = Some(lazybox_core::ProjectKey::local("custom"));
    let record = lazybox_store::WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    config.store.save_workspace(&record).unwrap();

    polling::migrate_legacy_sandbox(&config);

    let after: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        after.project_key,
        Some(lazybox_core::ProjectKey::local("custom")),
        "migration must not overwrite an existing project_key"
    );
}

// ── Session layout persistence ───────────────────────────────────────

#[tokio::test]
async fn set_session_layout_persists_and_broadcasts() {
    use lazybox_core::{SessionKind, SessionLayout, TileTree, WorkspaceSession};
    let config = ServerConfig::in_memory();

    // Seed a workspace with one session.
    let task = make_task("o/r#1");
    let ws_key = lazybox_core::WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
    let mut ws = lazybox_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws_key.clone(),
        SessionKind::Shell,
        std::path::PathBuf::from("/tmp/lazybox-test"),
        Utc::now(),
    );
    let session_id = session.id;
    ws.add_session(session);
    config
        .store
        .save_workspace(&lazybox_store::WorkspaceRecord {
            key: ws_key.as_str().to_string(),
            created_at: ws.created_at,
            workspace_json: serde_json::to_string(&ws).ok(),
        })
        .unwrap();

    // New layout: HSplit with two leaves.
    let layout = SessionLayout::Splits {
        tree: TileTree::HSplit {
            left: Box::new(TileTree::Leaf { terminal_id: 1 }),
            right: Box::new(TileTree::Leaf { terminal_id: 2 }),
            ratio: 50,
        },
        focused: vec![0],
    };
    polling::set_session_layout(&config, &ws_key, session_id, layout.clone()).await;

    // Reload + verify.
    let stored: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&ws_key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    let stored_layout = &stored.sessions[0].layout;
    assert_eq!(
        stored_layout, &layout,
        "layout round-trips through the store"
    );
}
#[tokio::test]
async fn set_session_layout_no_op_for_missing_session() {
    use lazybox_core::SessionLayout;
    let config = ServerConfig::in_memory();
    let key = lazybox_core::WorkspaceKey::new("github:none");
    // Should not panic when neither workspace nor session exist.
    polling::set_session_layout(
        &config,
        &key,
        lazybox_core::SessionId::new(),
        SessionLayout::default(),
    )
    .await;
}
// ── Bus → Server::serve integration ──────────────────────────────────

#[tokio::test]
async fn upserts_reach_subscribed_client_through_bus() {
    let config = ServerConfig::in_memory();
    let (mut client, server) = channel::pair();
    let serve_config = config.clone();
    tokio::spawn(async move {
        Server::new(serve_config).serve(server).await.unwrap();
    });
    client.send(Command::Subscribe).unwrap();
    // Drain the initial Snapshot.
    let _snap = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .unwrap();

    // Now produce an upsert. The bus should fan it out to the client.
    polling::upsert(&config, make_task("o/r#777")).await;

    let evt = recv_workspace_upsert(&mut client).await;
    match evt {
        Event::WorkspaceUpserted(w) => {
            assert_eq!(w.pr.as_ref().unwrap().id.key, "o/r#777");
        }
        other => panic!("expected WorkspaceUpserted, got {other:?}"),
    }
}
// ── spawn() loop ─────────────────────────────────────────────────────

#[tokio::test]
async fn spawn_with_no_sources_exits_quickly_and_silently() {
    // Edge case: user has no GH token + no LINEAR_API_KEY. The daemon
    // should still boot, just with an idle polling task that doesn't
    // burn CPU spinning forever.
    let config = ServerConfig::in_memory();
    let handle = polling::spawn_with_sources(config, vec![], Duration::from_millis(10));
    tokio::time::timeout(Duration::from_millis(500), handle)
        .await
        .expect("polling task exits when sources is empty")
        .expect("no panic");
}

/// Regression — first-run polling never starts (boot path bug).
///
/// `run_embedded_realm` used to gate `polling::spawn` on
/// `persisted.is_some()`, so a first-run user (no setup yet) never got
/// a poll loop. The wizard's on-complete hook writes config and fires
/// `Command::Refresh` (→ `poll_wake.notify_one()`) to kick the first
/// tick — but that notify hit a loop that was never spawned, so the
/// inbox stayed empty until the user restarted lazybox. The fix spawns
/// the loop UNCONDITIONALLY at boot, before any setup exists.
///
/// That fix is only safe because the PRODUCTION `spawn` loop — unlike
/// `spawn_with_sources` (see the test above) — does NOT exit when a
/// tick produces no sources: it idles, re-reading config every tick,
/// until the wizard writes providers and a wake fires. This test pins
/// that property: spawn the real loop against an unconfigured
/// `LAZYBOX_HOME` (defaults → zero providers → no-op tick, no network)
/// and assert the loop is still alive after several intervals. If
/// someone "optimizes" `spawn` to exit-on-empty like
/// `spawn_with_sources`, the first-run kick would wake a dead loop and
/// this bug returns — this test goes red first.
#[tokio::test(flavor = "multi_thread")]
async fn production_spawn_loop_survives_unconfigured_boot() {
    // Point config resolution at an EMPTY dir so `Config::load()`
    // returns defaults (no providers enabled). Without this the loop
    // would read the dev machine's real `~/.lazybox/config.yaml`, build
    // a real GhSource, and fire live GitHub requests from a unit test.
    let tmp = tempfile::TempDir::new().unwrap();
    let prev = std::env::var_os("LAZYBOX_HOME");
    // SAFETY/ISOLATION: LAZYBOX_HOME is process-global, but within this
    // test binary `spawn`/`run_one_tick` are the only readers of the
    // resolved config path, and only this test drives them — no other
    // polling.rs test races on it. Restored before we return.
    unsafe { std::env::set_var("LAZYBOX_HOME", tmp.path()) };

    let config = ServerConfig::in_memory();
    let handle = polling::spawn(config, Duration::from_millis(10));

    // Still running after ~30 ticks' worth of wall-clock = the loop
    // did NOT exit on the empty-sources tick. (timeout → Err means the
    // JoinHandle never resolved; dropping it on timeout just detaches
    // the background task, which the runtime reaps at test teardown.)
    let still_running = tokio::time::timeout(Duration::from_millis(300), handle)
        .await
        .is_err();

    match prev {
        Some(v) => unsafe { std::env::set_var("LAZYBOX_HOME", v) },
        None => unsafe { std::env::remove_var("LAZYBOX_HOME") },
    }

    assert!(
        still_running,
        "production spawn() loop exited on unconfigured boot — \
         first-run polling would never start after the wizard's Refresh",
    );
}
// ── Per-provider filter ────────────────────────────────────────────

fn make_typed_task(key: &str, role: TaskRole, is_pr: bool) -> Task {
    let mut t = make_task(key);
    t.role = role;
    t.url = if is_pr {
        format!("https://github.com/o/r/pull/{key}")
    } else {
        format!("https://github.com/o/r/issues/{key}")
    };
    t
}

#[test]
fn github_filter_drops_disallowed_roles() {
    // User wants only PRs they authored. Per-type schema: pr.author
    // on, pr.reviewer/etc off → reviewer-role PRs dropped.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());
    filter.enabled_keys.insert("issue.author".into());

    let mine = make_typed_task("1", TaskRole::Author, true);
    let theirs = make_typed_task("2", TaskRole::Reviewer, true);

    let kept = polling::filter_github_tasks(
        vec![mine.clone(), theirs.clone()],
        &filter,
        &std::collections::BTreeSet::new(),
    );
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id.key, mine.id.key);
}

#[test]
fn github_filter_drops_disallowed_types() {
    // Author of everything but only wants PRs — no issue.* keys at
    // all → issues filtered out entirely.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());
    // No issue.* keys — issues should be dropped.

    let pr = make_typed_task("1", TaskRole::Author, true);
    let issue = make_typed_task("2", TaskRole::Author, false);

    let kept = polling::filter_github_tasks(
        vec![pr.clone(), issue.clone()],
        &filter,
        &std::collections::BTreeSet::new(),
    );
    assert_eq!(kept.len(), 1, "issue dropped, PR kept");
    assert!(kept[0].url.contains("/pull/"));
}

#[test]
fn linear_filter_drops_disallowed_roles() {
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("role.assignee".into());

    let mut assignee = make_task("LIN-1");
    assignee.id.source = "linear".into();
    assignee.role = TaskRole::Assignee;
    let mut subscriber = make_task("LIN-2");
    subscriber.id.source = "linear".into();
    subscriber.role = TaskRole::Mentioned;

    let kept = polling::filter_linear_tasks(vec![assignee.clone(), subscriber.clone()], &filter);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id.key, "LIN-1");
}

#[test]
fn empty_filter_drops_everything() {
    // Defensive: if the user somehow ends up with an empty filter,
    // the daemon shouldn't spam them with every task.
    let filter = ProviderConfig::default();
    let kept = polling::filter_github_tasks(
        vec![make_typed_task("1", TaskRole::Author, true)],
        &filter,
        &std::collections::BTreeSet::new(),
    );
    assert!(kept.is_empty());
}

#[test]
fn github_config_filters_convert_to_scopes_and_watch_repos() {
    let filters = vec![
        lazybox_config::Filter {
            org: Some("acme".into()),
            repo: None,
            watch: None,
        },
        lazybox_config::Filter {
            org: None,
            repo: Some("widgets/core".into()),
            watch: None,
        },
        lazybox_config::Filter {
            org: None,
            repo: None,
            watch: Some("infra/platform".into()),
        },
    ];

    let scopes = polling::github_scopes_from_filters(&filters);
    assert!(scopes.contains("github:acme"));
    assert!(scopes.contains("github:widgets/core"));
    assert!(!scopes.contains("github:infra/platform"));

    let watches = polling::github_watch_repos_from_filters(&filters);
    assert_eq!(
        watches,
        std::collections::BTreeSet::from(["infra/platform".to_string()])
    );
}

#[test]
fn watched_repo_keeps_uninvolved_prs_past_role_and_scope_filters() {
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());

    let mut watched = make_repo_task("acme/infra");
    watched.role = TaskRole::Reviewer;
    let mut unrelated = make_repo_task("other/repo");
    unrelated.role = TaskRole::Reviewer;

    let scopes = std::collections::BTreeSet::from(["github:somewhere/else".to_string()]);
    let watches = std::collections::BTreeSet::from(["acme/infra".to_string()]);
    let kept = polling::filter_github_tasks_with_watches(
        vec![watched.clone(), unrelated],
        &filter,
        &scopes,
        &watches,
    );

    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].repo.as_deref(), Some("acme/infra"));
}

// ── Scope filter ───────────────────────────────────────────────────

fn make_repo_task(repo: &str) -> Task {
    let mut t = make_task("1");
    t.role = TaskRole::Author;
    t.repo = Some(repo.into());
    t.url = format!("https://github.com/{repo}/pull/1");
    t
}

/// All PR + Issue role keys on. Equivalent to "subscribe to
/// everything the user is involved with."
fn fully_open_filter() -> ProviderConfig {
    let mut f = ProviderConfig::default();
    f.enabled_keys.insert("pr.author".into());
    f.enabled_keys.insert("pr.reviewer".into());
    f.enabled_keys.insert("pr.assignee".into());
    f.enabled_keys.insert("pr.mentioned".into());
    f.enabled_keys.insert("issue.author".into());
    f.enabled_keys.insert("issue.assignee".into());
    f.enabled_keys.insert("issue.mentioned".into());
    f
}

#[test]
fn empty_scope_set_lets_every_task_through() {
    // No picker run → empty selected_scopes → "all scopes". Default
    // for setups that haven't run the scope picker yet.
    let kept = polling::filter_github_tasks(
        vec![make_repo_task("acme/web"), make_repo_task("widgets/core")],
        &fully_open_filter(),
        &std::collections::BTreeSet::new(),
    );
    assert_eq!(kept.len(), 2);
}

#[test]
fn repo_scope_keeps_only_matching_repos() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme/web".to_string());
    let kept = polling::filter_github_tasks(
        vec![
            make_repo_task("acme/web"),
            make_repo_task("acme/api"),
            make_repo_task("widgets/core"),
        ],
        &fully_open_filter(),
        &scopes,
    );
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].repo.as_deref(), Some("acme/web"));
}

#[test]
fn org_scope_keeps_every_repo_under_that_org() {
    // Selecting an org scope is shorthand for "all of its repos".
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    let kept = polling::filter_github_tasks(
        vec![
            make_repo_task("acme/web"),
            make_repo_task("acme/api"),
            make_repo_task("widgets/core"),
        ],
        &fully_open_filter(),
        &scopes,
    );
    let kept_repos: Vec<&str> = kept.iter().filter_map(|t| t.repo.as_deref()).collect();
    assert_eq!(kept_repos, vec!["acme/web", "acme/api"]);
}

#[test]
fn task_with_no_repo_drops_when_scopes_set() {
    // Defensive: scope-narrowing should not leak unknown-origin tasks.
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme/web".to_string());
    let mut t = make_repo_task("acme/web");
    t.repo = None;
    let kept = polling::filter_github_tasks(vec![t], &fully_open_filter(), &scopes);
    assert!(kept.is_empty());
}

// ── Search-qualifier builder ────────────────────────────────────────

#[test]
fn pr_qualifiers_default_to_involves_when_all_pr_roles_enabled() {
    // All four PR roles set → use the broadest involves: shortcut.
    let quals = polling::build_pr_search_qualifiers(
        &fully_open_filter(),
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(quals, vec!["involves:alice"]);
}

#[test]
fn pr_qualifiers_emit_specific_role_when_subset_enabled() {
    // Just `pr.author` — narrow upstream so GitHub doesn't return PRs
    // matching other roles we'd drop post-fetch.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());
    let quals =
        polling::build_pr_search_qualifiers(&filter, &std::collections::BTreeSet::new(), "alice");
    assert_eq!(quals, vec!["author:alice"]);
}

#[test]
fn pr_qualifiers_two_roles_emit_involves_not_paren_or() {
    // Regression: GitHub's qualifier search silently mishandles
    // `(author:X OR review-requested:X) repo:Y`, returning 0 even
    // when the rows exist. Confirmed against `gh search prs`. We
    // route through `involves:USER` instead and post-filter in
    // `filter_github_tasks`. See `polling::role_qualifier`.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("pr.author".into());
    filter.enabled_keys.insert("pr.reviewer".into());
    let quals =
        polling::build_pr_search_qualifiers(&filter, &std::collections::BTreeSet::new(), "alice");
    assert_eq!(
        quals,
        vec!["involves:alice"],
        "must NOT emit a paren-OR group — GitHub's parser drops rows"
    );
}

#[test]
fn issue_qualifiers_have_no_reviewer() {
    // Issues never have a reviewer — `pr.reviewer` is irrelevant for
    // the issue search.
    let mut filter = ProviderConfig::default();
    filter.enabled_keys.insert("issue.author".into());
    filter.enabled_keys.insert("pr.reviewer".into());
    let quals = polling::build_issue_search_qualifiers(
        &filter,
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(quals, vec!["author:alice"]);
}

#[test]
fn issue_qualifiers_default_to_involves_when_all_issue_roles_enabled() {
    let quals = polling::build_issue_search_qualifiers(
        &fully_open_filter(),
        &std::collections::BTreeSet::new(),
        "alice",
    );
    assert_eq!(quals, vec!["involves:alice"]);
}

#[test]
fn pr_qualifiers_append_org_scope() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(quals, vec!["involves:alice", "org:acme"]);
}

#[test]
fn pr_qualifiers_append_repo_scope() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme/web".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(quals, vec!["involves:alice", "repo:acme/web"]);
}

#[test]
fn pr_qualifiers_drop_scope_qualifier_with_multiple_scopes() {
    // Multi-scope used to emit `(org:acme OR repo:widgets/core)`,
    // which GitHub's search parser silently returns 0 for when
    // combined with `involves:USER`. 2026-05-27 incident: user
    // added a second repo scope, entire inbox disappeared. Fix:
    // 2+ scopes → omit the scope qualifier from the wire query
    // and filter post-fetch in `filter_github_tasks`.
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    scopes.insert("github:widgets/core".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(
        quals,
        vec!["involves:alice"],
        "multi-scope must NOT add a parens-OR group — that returns 0 results from GH"
    );
}

#[test]
fn pr_qualifiers_drop_unknown_provider_prefix() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("linear:bogus".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(quals, vec!["involves:alice"]);
}

#[tokio::test]
async fn spawn_drives_sources_on_interval() {
    let config = ServerConfig::in_memory();
    let counter = Arc::new(AtomicUsize::new(0));
    let source: Box<dyn TaskSource> = Box::new(CountingSource {
        name: "test".into(),
        counter: counter.clone(),
    });
    let handle = polling::spawn_with_sources(config, vec![source], Duration::from_millis(40));

    // Wait long enough for several ticks; the first tick fires
    // immediately, subsequent ticks every 40ms.
    tokio::time::sleep(Duration::from_millis(150)).await;
    handle.abort();
    let n = counter.load(Ordering::SeqCst);
    assert!(n >= 2, "polled at least twice (got {n})");
}
#[tokio::test]
async fn rescope_removes_workspaces_with_no_active_session() {
    use lazybox_core::WorkspaceKey;
    let config = ServerConfig::in_memory();
    // Seed with an existing workspace (was in scope last poll).
    polling::upsert(&config, make_task("o/r#stale")).await;
    polling::upsert(&config, make_task("o/r#current")).await;

    // Simulate a new poll that returns only `#current` — `#stale`
    // fell out of scope (filter change, repo unsubscribed, …).
    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(lazybox_core::workspace_key_for(
            &make_task("o/r#current"),
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::new(),
        all_full: true,
    };
    polling::rescope(&config, &outcome).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(
        !after.iter().any(|k| k.contains("stale")),
        "stale workspace should be removed; got: {after:?}"
    );
    assert!(after.iter().any(|k| k.contains("current")));
}
#[tokio::test]
async fn rescope_keeps_workspaces_with_active_sessions_and_emits_prompt() {
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{TerminalId, TerminalKind};
    let config = ServerConfig::in_memory();
    let mut bus_rx = config.bus.subscribe();
    polling::upsert(&config, make_task("o/r#alive")).await;
    polling::upsert(&config, make_task("o/r#kept-elsewhere")).await;

    // Stash a terminal pointing at `#alive` so rescope sees it as
    // "has active session". `terminal_meta` is the source of truth
    // the production code consults.
    let session_key: SessionKey =
        SessionKey::from(lazybox_core::workspace_key_for(&make_task("o/r#alive")));
    config
        .terminal_meta
        .lock()
        .await
        .insert(TerminalId(7), (session_key, TerminalKind::Shell));

    // Poll returns only `#kept-elsewhere` — `#alive` is out of
    // scope but has a live terminal.
    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(lazybox_core::workspace_key_for(
            &make_task("o/r#kept-elsewhere"),
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::new(),
        all_full: true,
    };
    let mut state = polling::TickState::default();
    polling::rescope_with_state(&config, &outcome, &mut state).await;

    // Drain bus_rx, capture the prompt(s).
    let mut prompts = 0;
    while let Ok(evt) = bus_rx.try_recv() {
        if matches!(evt, Event::WorkspaceOutOfScope { .. }) {
            prompts += 1;
        }
    }
    assert_eq!(
        prompts, 1,
        "exactly one prompt for the active-session workspace"
    );

    // Critical: a second rescope with the same input should NOT
    // re-prompt. State threading dedupes — without it, every 60s
    // tick would re-fire the same modal at the user.
    polling::rescope_with_state(&config, &outcome, &mut state).await;
    let mut prompts2 = 0;
    while let Ok(evt) = bus_rx.try_recv() {
        if matches!(evt, Event::WorkspaceOutOfScope { .. }) {
            prompts2 += 1;
        }
    }
    assert_eq!(
        prompts2, 0,
        "second rescope must not re-prompt for the same workspace"
    );

    // Active workspace still in the store; nothing was killed.
    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(after.iter().any(|k| k.contains("alive")));
}
#[tokio::test]
async fn rescope_with_empty_but_successful_poll_keeps_workspaces() {
    // Previously: an empty-but-successful poll would wipe the entire
    // store. That's catastrophic — the user pressed Shift-R, got a
    // transient 0-result GitHub response, and watched ALL their
    // workspaces disappear (real incident, 2026-05-27). The fix: a
    // 0-task poll never rescopes. The user can explicitly remove
    // rows via `x x` / Settings → Clean.
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#ghost-1")).await;
    polling::upsert(&config, make_task("o/r#ghost-2")).await;
    let outcome = polling::TickOutcome {
        polled: vec![],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::new(),
        all_full: true,
    };
    polling::rescope(&config, &outcome).await;
    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(
        after.len(),
        2,
        "empty-but-successful poll must NOT delete workspaces — got {after:?}"
    );
}
#[tokio::test]
async fn rescope_with_all_sources_failed_skips_cleanup() {
    // Different case: poll attempted but every source errored
    // (network down, rate limit, …). polled is empty AND
    // any_source_succeeded is false. We must NOT remove anything;
    // a transient network blip shouldn't wipe the sidebar.
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#keep-me")).await;
    let outcome = polling::TickOutcome {
        polled: vec![],
        any_source_succeeded: false,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::new(),
        all_full: true,
    };
    polling::rescope(&config, &outcome).await;
    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(
        after.iter().any(|k| k.contains("keep-me")),
        "all-failed poll must not remove anything: got {after:?}"
    );
}
#[tokio::test]
async fn rescope_preserves_workspaces_from_unpolled_repos() {
    // Regression for issue #34: round-robin polling polls a slice
    // of the user's repos per tick (e.g. 3 of 10). Pre-fix, rescope
    // treated every stored workspace not in `polled` as out-of-
    // scope and deleted it — so PRs from the 7 unpolled repos
    // disappeared every warm tick and reappeared on the next global
    // sweep (~K minutes later).
    //
    // Fix: TickOutcome carries per-source `PolledScope`. When a
    // source reports `Repos(...)`, workspaces in repos NOT in that
    // list are preserved — we have no information about them this
    // tick and silently dropping them is data loss.
    use lazybox_core::{Task, TaskId, WorkspaceKey};
    let config = ServerConfig::in_memory();

    // Seed three workspaces across three different GitHub repos.
    let mut polled_task = make_task("owner/polled#1");
    polled_task.id = TaskId {
        source: "github".into(),
        key: "owner/polled#1".into(),
    };
    polled_task.repo = Some("owner/polled".into());
    polled_task.url = "https://github.com/owner/polled/pull/1".into();

    let mut unpolled_task: Task = make_task("owner/unpolled#2");
    unpolled_task.id = TaskId {
        source: "github".into(),
        key: "owner/unpolled#2".into(),
    };
    unpolled_task.repo = Some("owner/unpolled".into());
    unpolled_task.url = "https://github.com/owner/unpolled/pull/2".into();

    let mut other_repo_task: Task = make_task("owner/other#3");
    other_repo_task.id = TaskId {
        source: "github".into(),
        key: "owner/other#3".into(),
    };
    other_repo_task.repo = Some("owner/other".into());
    other_repo_task.url = "https://github.com/owner/other/pull/3".into();

    polling::upsert(&config, polled_task.clone()).await;
    polling::upsert(&config, unpolled_task.clone()).await;
    polling::upsert(&config, other_repo_task.clone()).await;

    // Simulate a round-robin tick: only `owner/polled` was queried;
    // the GhSource reports `Repos(["owner/polled"])` as its
    // authoritative scope. The other two repos are intentionally
    // outside this tick's coverage.
    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(lazybox_core::workspace_key_for(
            &polled_task,
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::from([(
            "github".into(),
            polling::PolledScope::Repos(vec!["owner/polled".into()]),
        )]),
        all_full: true,
    };
    polling::rescope(&config, &outcome).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();

    // The polled repo's workspace stays (it was in `polled`).
    assert!(
        after.iter().any(|k| k.contains("polled")),
        "polled workspace should remain: {after:?}",
    );
    // CRITICAL: workspaces from unpolled repos must be preserved.
    // Pre-fix, both would have been deleted here — that's the bug.
    assert!(
        after.iter().any(|k| k.contains("unpolled")),
        "unpolled-repo workspace MUST be preserved (issue #34): {after:?}",
    );
    assert!(
        after.iter().any(|k| k.contains("other")),
        "other-repo workspace MUST be preserved (issue #34): {after:?}",
    );
}

#[tokio::test]
async fn rescope_with_exhaustive_scope_still_deletes_stale() {
    // Counterpart to `rescope_preserves_workspaces_from_unpolled_repos`:
    // when the GH source reports `Exhaustive` (the global sweep ran),
    // the legacy behavior is intact — stale workspaces still get
    // cleaned up. Without this assertion, a regression that flipped
    // the new guard to "always preserve" would silently leave the
    // sidebar full of stale rows forever.
    use lazybox_core::WorkspaceKey;
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#stale")).await;
    polling::upsert(&config, make_task("o/r#current")).await;

    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(lazybox_core::workspace_key_for(
            &make_task("o/r#current"),
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::from([(
            "github".into(),
            polling::PolledScope::Exhaustive,
        )]),
        all_full: true,
    };
    polling::rescope(&config, &outcome).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(
        !after.iter().any(|k| k.contains("stale")),
        "Exhaustive scope must still delete stale workspaces: {after:?}",
    );
    assert!(after.iter().any(|k| k.contains("current")));
}

#[test]
fn gh_polled_scope_downgrades_to_preserve_all_on_partial_sweep() {
    use polling::{PolledScope, gh_polled_scope};
    // Clean, unwindowed (reconcile) global sweep → Exhaustive (rescope
    // may delete stale rows).
    assert_eq!(
        gh_polled_scope(true, &[], false, false),
        PolledScope::Exhaustive,
        "a clean reconcile global sweep authoritatively covers everything",
    );
    // Clean round-robin tick → only the queried repos are authoritative.
    assert_eq!(
        gh_polled_scope(false, &["owner/a".into()], false, false),
        PolledScope::Repos(vec!["owner/a".into()]),
    );
    // PARTIAL sweep (e.g. PR query errored, issues OK) → empty
    // coverage so rescope preserves EVERY github workspace this tick.
    // This is the guard against a PR vanishing because one poll
    // hiccupped rather than because it merged/closed. The `run_global`
    // flag is irrelevant once the sweep is partial.
    assert_eq!(
        gh_polled_scope(true, &[], true, false),
        PolledScope::Repos(Vec::new()),
        "a partial global sweep must NOT claim exhaustive coverage",
    );
    assert_eq!(
        gh_polled_scope(false, &["owner/a".into()], true, false),
        PolledScope::Repos(Vec::new()),
        "a partial round-robin tick must preserve all, not just unqueried repos",
    );
    // WINDOWED global sweep (issue #14): only changed PRs came back, so
    // the unchanged majority is absent — same data-loss shape as a
    // partial sweep. Must preserve all; only the periodic reconcile
    // sweep (windowed=false) drives deletion.
    assert_eq!(
        gh_polled_scope(true, &[], false, true),
        PolledScope::Repos(Vec::new()),
        "a windowed global sweep must NOT claim exhaustive coverage",
    );
}

#[test]
fn fetch_mode_label_distinguishes_delivery_paths() {
    assert_eq!(FetchMode::Full.label(), "full-sweep");
    assert_eq!(FetchMode::Incremental.label(), "notifications");
}

#[tokio::test]
async fn rescope_preserves_prs_when_pr_fetch_partially_failed() {
    // End-to-end guard for the "PRs disappear on a flaky poll" bug.
    // When the PR query errors but the issue query succeeds, the
    // GitHub client returns issues-only `Ok(..)` to keep the inbox
    // alive — so the freshly-polled set contains NO PRs. If the
    // source reported `Exhaustive`, rescope would read every stored
    // PR as "fell out of scope" and delete it. The partial-sweep
    // guard makes the source report empty coverage
    // (`gh_polled_scope(.., partial=true)` → `Repos([])`), so the PR
    // survives until a clean sweep can speak to its real state.
    use lazybox_core::{TaskId, WorkspaceKey};
    let config = ServerConfig::in_memory();

    let mut pr_task = make_task("owner/repo#7");
    pr_task.id = TaskId {
        source: "github".into(),
        key: "owner/repo#7".into(),
    };
    pr_task.repo = Some("owner/repo".into());
    pr_task.url = "https://github.com/owner/repo/pull/7".into();

    let mut issue_task = make_task("owner/repo#8");
    issue_task.id = TaskId {
        source: "github".into(),
        key: "owner/repo#8".into(),
    };
    issue_task.repo = Some("owner/repo".into());
    issue_task.url = "https://github.com/owner/repo/issues/8".into();

    polling::upsert(&config, pr_task.clone()).await;
    polling::upsert(&config, issue_task.clone()).await;

    // The partial sweep returned only the issue; the PR query failed.
    // The source reports the downgraded scope it would compute via
    // `gh_polled_scope(run_global=true, repos=[], partial=true)`.
    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(lazybox_core::workspace_key_for(
            &issue_task,
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::from([(
            "github".into(),
            polling::gh_polled_scope(true, &[], true, false),
        )]),
        all_full: true,
    };
    polling::rescope(&config, &outcome).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(
        after.iter().any(|k| k.contains("repo-7")),
        "PR workspace MUST survive a partial sweep where the PR query failed: {after:?}",
    );
    assert!(
        after.iter().any(|k| k.contains("repo-8")),
        "issue workspace should also be preserved on a partial sweep: {after:?}",
    );
}

#[tokio::test]
async fn rescope_preserves_workspaces_from_unreported_sources() {
    // When one source succeeds and another fails (or isn't enabled),
    // only the succeeding source's workspaces are deletion candidates.
    // Pre-fix, a Linear-only successful tick would have wiped every
    // GitHub workspace (since none were in `polled`) — silently
    // destructive whenever GH had a transient failure.
    use lazybox_core::{Task, TaskId};
    let config = ServerConfig::in_memory();

    // Seed a GH workspace and a Linear workspace.
    let mut gh_task = make_task("o/r#1");
    gh_task.repo = Some("o/r".into());

    let mut linear_task: Task = make_task("team-x-42");
    linear_task.id = TaskId {
        source: "linear".into(),
        key: "team-x-42".into(),
    };
    linear_task.repo = Some("team-x".into());
    linear_task.url = "https://linear.app/team/issue/team-x-42".into();

    polling::upsert(&config, gh_task.clone()).await;
    polling::upsert(&config, linear_task.clone()).await;

    // Only Linear reported this tick — GH errored out.
    let outcome = polling::TickOutcome {
        polled: vec![],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::from([(
            "linear".into(),
            polling::PolledScope::Exhaustive,
        )]),
        all_full: true,
    };
    // `polled` is empty so the existing "empty polled, refuse to
    // wipe" guard fires before our new scope guard — assert the
    // store is intact through that path as well. The scope-guard's
    // own coverage lives in the previous two tests.
    polling::rescope(&config, &outcome).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(after.len(), 2, "both workspaces must survive: {after:?}",);
}

#[tokio::test]
async fn rescope_round_robin_tick_after_global_keeps_unpolled() {
    // End-to-end-ish: simulate the exact issue #34 sequence using
    // real `TickState` + `tick_with_state`.
    //
    // 1. First tick: a fake source returns three PRs spanning three
    //    repos and reports `Exhaustive` (mimics a cold-start
    //    global GH sweep). All three workspaces land in the store.
    // 2. Second tick: a fake source returns ONLY `owner/a#1` and
    //    reports `Repos(["owner/a"])` (mimics a warm round-robin
    //    tick where only `owner/a` was queried).
    //
    // Expectation: after step 2, `owner/b#2` and `owner/c#3` are
    // still in the store — they belong to repos we didn't query
    // this tick, not repos that fell out of upstream scope.
    use lazybox_core::{Task, TaskId};

    fn gh_task(repo: &str, num: u32) -> Task {
        let mut t = make_task(&format!("{repo}#{num}"));
        t.id = TaskId {
            source: "github".into(),
            key: format!("{repo}#{num}"),
        };
        t.repo = Some(repo.into());
        t.url = format!("https://github.com/{repo}/pull/{num}");
        t
    }

    let config = ServerConfig::in_memory();
    let mut state = polling::TickState::default();

    // Step 1: global tick, three repos in scope.
    let sources: Vec<Box<dyn polling::TaskSource>> = vec![Box::new(ScopedSource {
        name: "github".into(),
        tasks: vec![
            gh_task("owner/a", 1),
            gh_task("owner/b", 2),
            gh_task("owner/c", 3),
        ],
        scope: polling::PolledScope::Exhaustive,
    })];
    let outcome = polling::tick_with_state(&config, &sources, &mut state).await;
    polling::rescope_with_state(&config, &outcome, &mut state).await;
    assert_eq!(
        config.store.list_workspaces().unwrap().len(),
        3,
        "global tick seeds all three workspaces",
    );

    // Step 2: warm round-robin tick, only `owner/a` queried.
    let sources: Vec<Box<dyn polling::TaskSource>> = vec![Box::new(ScopedSource {
        name: "github".into(),
        tasks: vec![gh_task("owner/a", 1)],
        scope: polling::PolledScope::Repos(vec!["owner/a".into()]),
    })];
    let outcome = polling::tick_with_state(&config, &sources, &mut state).await;
    polling::rescope_with_state(&config, &outcome, &mut state).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(
        after.iter().any(|k| k.contains("owner-a")),
        "the queried repo stays: {after:?}",
    );
    assert!(
        after.iter().any(|k| k.contains("owner-b")),
        "unpolled repo MUST be preserved (issue #34): {after:?}",
    );
    assert!(
        after.iter().any(|k| k.contains("owner-c")),
        "unpolled repo MUST be preserved (issue #34): {after:?}",
    );
}

#[tokio::test]
async fn delete_workspace_kills_terminals_via_terminal_meta() {
    // Regression: an earlier implementation parsed the backend_key
    // prefix to find which terminals belong to a workspace. After
    // tmux session names switched to `lazybox-{repo}-{kind}-{pid}-{n}`
    // (no longer prefixed with the workspace_key), that filter
    // matched zero terminals — confirmed `x x` silently kept the ghosts.
    // Now we use terminal_meta as the source of truth.
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{TerminalId, TerminalKind};

    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#1")).await;

    let workspace_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#1")));
    let session_key = SessionKey::from(workspace_key.as_str());
    // Insert a terminal pointing at this workspace, with a backend
    // key in the NEW format that doesn't start with the workspace
    // key.
    let backend_key_new_format = format!("lazybox-o-r-1-claude-{}-1", std::process::id());
    config
        .terminals
        .lock()
        .await
        .insert(TerminalId(42), backend_key_new_format.clone());
    config.terminal_meta.lock().await.insert(
        TerminalId(42),
        (session_key.clone(), TerminalKind::Agent("claude".into())),
    );
    // Also seed the auxiliary maps so we can assert delete cleans
    // them up — otherwise a stale entry leaks into rescope's next
    // tick.
    config
        .terminal_sessions
        .lock()
        .await
        .insert(TerminalId(42), lazybox_core::SessionId::new());
    config
        .agent_states
        .lock()
        .await
        .insert(TerminalId(42), lazybox_ipc::AgentState::Working);

    assert!(polling::delete_workspace(&config, &workspace_key).await);

    assert!(
        config.terminals.lock().await.get(&TerminalId(42)).is_none(),
        "delete_workspace must remove the terminal from the wire-side map"
    );
    assert!(
        config
            .terminal_meta
            .lock()
            .await
            .get(&TerminalId(42))
            .is_none(),
        "terminal_meta cleaned too"
    );
    assert!(
        config
            .terminal_sessions
            .lock()
            .await
            .get(&TerminalId(42))
            .is_none(),
        "terminal_sessions cleaned too"
    );
    assert!(
        config
            .agent_states
            .lock()
            .await
            .get(&TerminalId(42))
            .is_none(),
        "agent_states cleaned too"
    );
    assert!(
        config.store.list_workspaces().unwrap().is_empty(),
        "workspace deleted from store"
    );
}

#[tokio::test]
async fn failed_terminal_kill_preserves_workspace_and_retryable_mappings() {
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{TerminalId, TerminalKind};

    let backend = MockBackend::new();
    let config =
        ServerConfig::with_store_and_backend(Arc::new(MemoryStore::new()), backend.as_backend());
    let task = make_task("o/r#kill-fails");
    polling::upsert(&config, task.clone()).await;
    let workspace_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
    let session_key = SessionKey::from(workspace_key.as_str());
    let terminal_id = TerminalId(77);
    let backend_key = backend
        .spawn(&["codex".into()], None, &[], "kill-fails")
        .await
        .unwrap();
    config
        .terminals
        .lock()
        .await
        .insert(terminal_id, backend_key.clone());
    config.terminal_meta.lock().await.insert(
        terminal_id,
        (session_key, TerminalKind::Agent("codex".into())),
    );
    backend
        .fail_kill(&backend_key, "backend transport timed out")
        .await;
    let mut bus = config.bus.subscribe();

    assert!(!polling::delete_workspace(&config, &workspace_key).await);

    assert!(
        config
            .store
            .get_workspace(&workspace_key)
            .unwrap()
            .is_some(),
        "a workspace must stay visible when its live process could not be stopped"
    );
    assert_eq!(
        config.terminals.lock().await.get(&terminal_id),
        Some(&backend_key),
        "the retryable terminal mapping must not be orphaned"
    );
    assert!(
        !polling::load_archived_set(&config).contains(workspace_key.as_str()),
        "a failed delete must not poison future upserts via the archive set"
    );
    assert!(
        !config
            .deleted_workspaces
            .lock()
            .contains(workspace_key.as_str()),
        "a failed delete must clear the in-process spawn tombstone too"
    );
    assert!(
        std::iter::from_fn(|| bus.try_recv().ok()).any(|event| matches!(
            event,
            Event::ProviderError { message, .. }
                if message.contains("was not deleted")
        )),
        "the user must get a visible retryable error instead of a silent partial delete"
    );
}
// ── Issue → PR collapsing (closingIssuesReferences) ─────────────────

fn make_issue_task(key: &str) -> Task {
    // Mirror `make_task` but mint an issue URL so the workspace
    // classifier routes this into `gh_issues` (not the PR slot).
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
    let mut t = make_task(key);
    t.url = format!("https://github.com/{path}/issues/{num}");
    t
}

fn make_pr_closing(pr_key: &str, closes: &[&str]) -> Task {
    let mut t = make_task(pr_key);
    t.closes_issues = closes
        .iter()
        .map(|k| TaskId {
            source: "github".into(),
            key: (*k).into(),
        })
        .collect();
    t
}

#[tokio::test]
async fn pr_polled_after_issue_collapses_them_into_one_row() {
    // Issue is polled first → standalone workspace (zero sessions).
    // PR shows up claiming the issue via closingIssuesReferences →
    // the empty issue workspace folds into the PR's silently AND
    // emits a `WorkspaceMerged` notice so the TUI can flash a
    // footer line.
    let config = ServerConfig::in_memory();
    let mut bus = config.bus.subscribe();
    polling::upsert(&config, make_issue_task("o/r#71")).await;
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    let keys: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(
        keys.len(),
        1,
        "issue + PR must collapse to one workspace row, got {keys:?}"
    );
    assert!(keys[0].contains("141"), "remaining row is the PR's");

    let pr_ws_record = config.store.list_workspaces().unwrap().pop().unwrap();
    let pr_ws: lazybox_core::Workspace =
        serde_json::from_str(&pr_ws_record.workspace_json.unwrap()).unwrap();
    assert_eq!(
        pr_ws.gh_issues.len(),
        1,
        "the issue must surface inside the PR workspace's gh_issues",
    );
    assert_eq!(pr_ws.gh_issues[0].id.key, "o/r#71");

    let mut saw_merged_notice = false;
    while let Ok(evt) = bus.try_recv() {
        if matches!(evt, Event::WorkspaceMerged { .. }) {
            saw_merged_notice = true;
        }
    }
    assert!(
        saw_merged_notice,
        "silent merges must emit WorkspaceMerged for the footer notice",
    );
}
#[tokio::test]
async fn issue_polled_after_pr_routes_into_pr_workspace() {
    // PR polled first (carrying closes_issues); issue polled next.
    // The issue's standalone workspace must NOT get created — its
    // update must flow into the PR workspace instead.
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;
    polling::upsert(&config, make_issue_task("o/r#71")).await;

    let records = config.store.list_workspaces().unwrap();
    assert_eq!(
        records.len(),
        1,
        "issue must NOT create its own workspace when a PR already claims it",
    );
    let ws: lazybox_core::Workspace =
        serde_json::from_str(records[0].workspace_json.clone().unwrap().as_str()).unwrap();
    assert_eq!(ws.pr.as_ref().unwrap().id.key, "o/r#141");
    assert_eq!(ws.gh_issues.len(), 1);
    assert_eq!(ws.gh_issues[0].id.key, "o/r#71");
}
/// Seed an issue workspace with a fabricated session and return its
/// id alongside the workspace key. Used by the merge-prompt + confirm
/// tests below — both want the same starting state.
async fn seed_issue_with_session(
    config: &ServerConfig,
    issue_short_key: &str,
) -> (lazybox_core::WorkspaceKey, lazybox_core::SessionId) {
    use lazybox_core::{SessionKind, WorkspaceKey, WorkspaceSession};
    polling::upsert(config, make_issue_task(issue_short_key)).await;
    let issue_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_issue_task(
        issue_short_key,
    )));
    let mut issue_ws: lazybox_core::Workspace = {
        let record = config.store.get_workspace(&issue_key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    let session_id = lazybox_core::SessionId::new();
    issue_ws.add_session(WorkspaceSession {
        id: session_id,
        workspace_key: issue_key.clone(),
        name: "claude".into(),
        kind: SessionKind::Agent {
            agent_id: "claude".into(),
        },
        state: lazybox_core::SessionRunState::Active,
        worktree_path: std::path::PathBuf::from("/tmp/lazybox-test"),
        created_at: Utc::now(),
        last_output_at: None,
        layout: lazybox_core::SessionLayout::default(),
    });
    let json = serde_json::to_string(&issue_ws).unwrap();
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: issue_key.as_str().to_string(),
            created_at: issue_ws.created_at,
            workspace_json: Some(json),
        })
        .unwrap();
    (issue_key, session_id)
}

/// Register a production-shaped live terminal bound to the given workspace
/// key. The merge safety gate keys off `terminal_meta`, not session records,
/// while the paired `terminals` and persisted metadata rows let accepted
/// merges exercise the complete durable rebadge path.
async fn attach_live_terminal(
    config: &ServerConfig,
    key: &lazybox_core::WorkspaceKey,
    terminal_id: u64,
) {
    let backend_key = format!("lazybox-live-test-{terminal_id}");
    attach_live_terminal_persisted(config, key, terminal_id, &backend_key).await;
}

#[tokio::test]
async fn live_issue_session_stalls_merge_and_emits_pending_event() {
    // Safety net: an issue workspace with a LIVE terminal must NOT be
    // silently absorbed by its closing PR. The daemon emits a
    // `WorkspaceMergePending` event and leaves both rows alone until
    // the user confirms via `Command::ConfirmMerge`.
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let mut bus = config.bus.subscribe();
    let (issue_key, _session_id) = seed_issue_with_session(&config, "o/r#71").await;
    attach_live_terminal(&config, &issue_key, 71).await;

    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    // Both workspaces still in the store.
    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_some(),
        "issue workspace must NOT auto-merge while it has live sessions",
    );
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
    assert!(config.store.get_workspace(&pr_key).unwrap().is_some());

    // And a WorkspaceMergePending fired so the TUI can prompt.
    let mut saw_pending = false;
    while let Ok(evt) = bus.try_recv() {
        if let Event::WorkspaceMergePending {
            issue_workspace_key,
            ..
        } = evt
        {
            assert_eq!(issue_workspace_key, issue_key);
            saw_pending = true;
        }
    }
    assert!(saw_pending, "expected a WorkspaceMergePending broadcast");
}
#[tokio::test]
async fn merge_collapse_does_not_deadlock_while_poll_state_held() {
    // Regression for the issue-131 sync stall. `run_one_tick` holds
    // `poll_state` across the entire tick, and the issue→PR collapse
    // runs deep inside `upsert`. When that collapse reached back for
    // the same (non-reentrant `tokio::sync::Mutex`) `poll_state` to
    // record its merge-prompt dedupe, the upsert self-deadlocked
    // until the per-task timeout fired — ~15s per PR-that-closes-a-
    // live-issue, every tick, so sync never finished, those PRs'
    // CI/state updates never landed, and the collapse never ran.
    // The dedupe memory now lives behind its own lock, so the upsert
    // completes promptly even while the tick guard is held.
    let config = ServerConfig::in_memory();
    let mut bus = config.bus.subscribe();
    let (issue_key, _session_id) = seed_issue_with_session(&config, "o/r#71").await;
    attach_live_terminal(&config, &issue_key, 71).await;

    // Hold the guard exactly as `run_one_tick` does for the whole tick.
    let guard = config.poll_state.lock().await;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])),
    )
    .await
    .expect("upsert deadlocked while poll_state was held (issue #131)");
    drop(guard);

    // Behaviour is otherwise unchanged: the live-session issue still
    // stalls the merge and emits the pending prompt.
    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_some(),
        "issue workspace must NOT auto-merge while it has live sessions",
    );
    let mut saw_pending = false;
    while let Ok(evt) = bus.try_recv() {
        if matches!(evt, Event::WorkspaceMergePending { .. }) {
            saw_pending = true;
        }
    }
    assert!(saw_pending, "expected a WorkspaceMergePending broadcast");
}

// ── poll_state checkout/restore (issue #133) ────────────────────────

/// Issue #133: a poll tick checks `poll_state` OUT for its duration
/// (`checkout_poll_state`) instead of holding the guard across the
/// whole fetch + upsert. While the state is checked out the guard is
/// FREE, so the serve loop's own `poll_state` users — the detached
/// `fetch_pr_details` client cache, the round-robin focus hint — can
/// acquire it mid-sync instead of stalling behind a ~17s fetch, and
/// nothing reachable from `upsert` can deadlock by re-acquiring a guard
/// the tick is holding, because it holds none. Reverting `run_one_tick`
/// to a held-across-the-tick guard would make this `try_lock` fail.
#[tokio::test]
async fn checkout_poll_state_frees_the_guard_for_the_tick() {
    let config = ServerConfig::in_memory();
    let state = polling::checkout_poll_state(&config).await;
    assert!(
        config.poll_state.try_lock().is_ok(),
        "poll_state must be free while a tick holds the checked-out state (#133)",
    );
    polling::restore_poll_state(&config, state).await;
}

/// `restore_poll_state` must fold back a `focused_repo` recorded by the
/// serve loop's focus hint WHILE the tick had the state checked out —
/// that is the user's latest sidebar navigation and must steer the next
/// tick's round-robin, not be clobbered by the value the tick carried
/// out.
#[tokio::test]
async fn restore_poll_state_keeps_a_concurrent_focus_hint() {
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#42");
    let workspace = lazybox_core::Workspace::from_task(task.clone(), Utc::now());
    polling::upsert(&config, task).await;

    // Tick checks the state out (empty round-robin); a sidebar
    // navigation then fires the focus hint into the now-free poll_state.
    let state = polling::checkout_poll_state(&config).await;
    polling::set_focused_workspace(&config, &workspace.key).await;

    polling::restore_poll_state(&config, state).await;
    let restored = config.poll_state.lock().await;
    assert_eq!(
        restored.round_robin.focused_repo.as_deref(),
        Some("o/r"),
        "a focus hint recorded mid-tick must survive restore (#133)",
    );
}

/// Checkout→restore round-trips the cross-tick state: the value the
/// tick carried out is written back so the NEXT tick still sees it
/// (the round-robin cursor / counter, the already-prompted sets).
/// Guards against a "fix" that drops the restore.
#[tokio::test]
async fn checkout_restore_round_trips_cross_tick_state() {
    let config = ServerConfig::in_memory();
    {
        let mut s = config.poll_state.lock().await;
        s.round_robin.tick = 7;
        s.round_robin.focused_repo = Some("o/seed".into());
    }
    let state = polling::checkout_poll_state(&config).await;
    // Checked out → the live slot is reset to default.
    assert_eq!(config.poll_state.lock().await.round_robin.tick, 0);

    polling::restore_poll_state(&config, state).await;
    let restored = config.poll_state.lock().await;
    assert_eq!(
        restored.round_robin.tick, 7,
        "tick counter must survive the checkout/restore round-trip",
    );
    assert_eq!(restored.round_robin.focused_repo.as_deref(), Some("o/seed"));
}

// ── gh_client lives outside poll_state (issue #92) ──────────────────

/// Issue #92: the long-lived GitHub client lives in its OWN lock
/// (`gh_client_cache`), not inside `poll_state`. Before the split, every
/// `poll_state` holder that needed the client — the poll tick, the
/// detached `fetch_pr_details` — reached it through the `poll_state`
/// guard, and on a cold cache rebuilt it via `from_credential` (a network
/// call) while still holding that guard. #133 made the cold path the
/// common case by emptying `poll_state`'s copy for the whole tick. With
/// the client in a separate `std::sync::Mutex`, reaching it never touches
/// `poll_state`: a poll holding `poll_state` (or having checked it out for
/// a tick) can't stall a concurrent client lookup, and the cold-cache
/// rebuild never spans `from_credential` under a held guard. Folding the
/// client back into `poll_state` would make this lookup-under-held-guard
/// fail.
#[tokio::test]
async fn gh_client_cache_is_independent_of_poll_state() {
    let config = ServerConfig::in_memory();
    // Simulate a tick holding poll_state (the pre-#133 worst case) /
    // the brief checkout window.
    let _poll_state_held = config.poll_state.lock().await;
    assert!(
        config.gh_client_cache.try_lock().is_some(),
        "gh_client_cache must be reachable without poll_state (#92)",
    );
}

#[tokio::test]
async fn confirm_merge_accept_runs_the_merge() {
    // After the user says "yes" to the prompt, the merge runs the
    // same as the silent path: sessions move, terminal_meta rebadges,
    // issue row disappears.
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let (issue_key, session_id) = seed_issue_with_session(&config, "o/r#71").await;
    attach_live_terminal(&config, &issue_key, 71).await;
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));

    polling::handle_confirm_merge(&config, issue_key.clone(), pr_key.clone(), true).await;

    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_none(),
        "issue workspace should be removed after accepted merge",
    );
    let pr_ws: lazybox_core::Workspace = {
        let record = config.store.get_workspace(&pr_key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    let moved = pr_ws
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .expect("session must have moved");
    assert_eq!(moved.workspace_key, pr_key);
}

#[tokio::test]
async fn dead_session_records_do_not_block_silent_merge() {
    // Regression: the merge gate used to key off `sessions.is_empty()`,
    // so an issue whose agent session's PTY died long ago (a session
    // RECORD with no live terminal) prompted every 5 minutes forever
    // and the auto-transfer never completed unattended. The gate is
    // live terminals now — dead records move over silently.
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let mut bus = config.bus.subscribe();
    // Session record exists, but NO terminal_meta entry → not live.
    let (issue_key, session_id) = seed_issue_with_session(&config, "o/r#71").await;

    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_none(),
        "issue workspace with only dead session records must merge silently",
    );
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
    let pr_ws: lazybox_core::Workspace = {
        let record = config.store.get_workspace(&pr_key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    assert!(
        pr_ws.sessions.iter().any(|s| s.id == session_id),
        "the dead session record must move onto the PR (still recoverable)",
    );

    let (mut saw_pending, mut saw_merged) = (false, false);
    while let Ok(evt) = bus.try_recv() {
        match evt {
            Event::WorkspaceMergePending { .. } => saw_pending = true,
            Event::WorkspaceMerged { .. } => saw_merged = true,
            _ => {}
        }
    }
    assert!(
        !saw_pending,
        "no modal for a workspace with no live terminal"
    );
    assert!(saw_merged, "silent merge still emits the footer notice");
}

#[tokio::test]
async fn silent_merge_commits_pr_before_deleting_issue_row() {
    // Regression: the auto-merge path used to delete the issue row
    // BEFORE the PR (carrying the moved sessions) was committed — a
    // cancellation in that window (the 15s per-task upsert timeout)
    // left the sessions in neither stored workspace. Pin the bus
    // ordering: the PR upsert that contains the absorbed issue must
    // be broadcast before the issue's WorkspaceRemoved.
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let mut bus = config.bus.subscribe();
    let (issue_key, _session_id) = seed_issue_with_session(&config, "o/r#71").await;

    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
    let mut events: Vec<Event> = Vec::new();
    while let Ok(evt) = bus.try_recv() {
        events.push(evt);
    }
    let removed_at = events
        .iter()
        .position(|e| matches!(e, Event::WorkspaceRemoved(k) if *k == issue_key))
        .expect("issue row must be removed");
    let committed_at = events
        .iter()
        .position(|e| {
            matches!(e, Event::WorkspaceUpserted(ws)
                if ws.key == pr_key && ws.gh_issues.iter().any(|t| t.id.key == "o/r#71"))
        })
        .expect("PR workspace carrying the absorbed issue must be broadcast");
    assert!(
        committed_at < removed_at,
        "PR commit must land before the issue row is deleted \
         (commit-then-delete keeps the moved sessions recoverable)",
    );
}

#[tokio::test]
async fn failed_workspace_batch_publishes_no_phantom_upsert() {
    let store = Arc::new(FailingBatchStore::new());
    let config = ServerConfig::with_store(store.clone());
    let mut bus = config.bus.subscribe();
    let task = make_issue_task("o/r#901");
    let key = lazybox_core::WorkspaceKey::new(lazybox_core::workspace_key_for(&task));

    store.fail_next_batch();
    polling::upsert(&config, task).await;

    assert!(
        config.store.get_workspace(&key).unwrap().is_none(),
        "the failed batch must leave no durable workspace"
    );
    let events: Vec<_> = std::iter::from_fn(|| bus.try_recv().ok()).collect();
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::WorkspaceUpserted(_) | Event::ProjectUpserted(_)
        )),
        "failed durability must not publish successful state: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ProviderError { .. })),
        "the failure must be visible to clients"
    );
}

#[tokio::test]
async fn failed_issue_merge_batch_preserves_source_and_emits_no_removal() {
    use lazybox_core::SessionKey;
    use lazybox_ipc::TerminalId;

    let store = Arc::new(FailingBatchStore::new());
    let config = ServerConfig::with_store(store.clone());
    let (issue_key, session_id) = seed_issue_with_session(&config, "o/r#902").await;
    let backend_key = "failed-merge-terminal";
    attach_live_terminal_persisted(&config, &issue_key, 902, backend_key).await;
    let issue_session_key: SessionKey = (&issue_key).into();
    let pr_task = make_pr_closing("o/r#903", &["o/r#902"]);
    polling::upsert(&config, pr_task.clone()).await;
    let pr_key = lazybox_core::WorkspaceKey::new(lazybox_core::workspace_key_for(&pr_task));
    let mut bus = config.bus.subscribe();

    store.fail_next_batch();
    polling::handle_confirm_merge(&config, issue_key.clone(), pr_key.clone(), true).await;

    let issue_record = config
        .store
        .get_workspace(&issue_key)
        .unwrap()
        .expect("failed merge must preserve the source issue row");
    let issue: lazybox_core::Workspace =
        serde_json::from_str(&issue_record.workspace_json.unwrap()).unwrap();
    assert!(
        issue
            .sessions
            .iter()
            .any(|session| session.id == session_id),
        "the source row must retain its session after rollback"
    );

    let pr_record = config.store.get_workspace(&pr_key).unwrap().unwrap();
    let pr: lazybox_core::Workspace =
        serde_json::from_str(&pr_record.workspace_json.unwrap()).unwrap();
    assert!(
        pr.sessions.iter().all(|session| session.id != session_id),
        "the destination PR must not receive a partially committed session"
    );
    assert_eq!(
        config
            .terminal_meta
            .lock()
            .await
            .get(&TerminalId(902))
            .expect("live terminal retained")
            .0,
        issue_session_key,
        "failed durability must not change in-memory terminal ownership"
    );
    let raw = config
        .store
        .get_kv(&format!("terminal:{backend_key}"))
        .unwrap()
        .expect("persisted terminal metadata retained");
    let (persisted_key, _): (String, lazybox_ipc::TerminalKind) =
        serde_json::from_str(&raw).unwrap();
    assert_eq!(
        persisted_key,
        issue_session_key.as_str(),
        "failed durability must not change restart-time terminal ownership"
    );
    let events: Vec<_> = std::iter::from_fn(|| bus.try_recv().ok()).collect();
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::WorkspaceRemoved(key) if *key == issue_key
        )),
        "rollback must not publish source removal: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::WorkspaceMerged { .. })),
        "rollback must not publish a completed merge"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TerminalsRebadged { .. })),
        "rollback must not publish a terminal rebadge"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::WorkspaceUpserted(workspace) if workspace.key == pr_key
        )),
        "rollback must not publish a destination upsert"
    );
}

#[tokio::test]
async fn merge_emits_rebadge_then_upsert_then_remove_then_merged() {
    // Daemon-side pin for the full issue→PR merge event ordering (I1,
    // I2, I6). The TUI tests assert the CONSUMPTION end (slots follow,
    // focus follows); this asserts the PRODUCER end, so a refactor of
    // the single `commit_merge` owner that reordered its steps — e.g.
    // deleting the issue row before committing the PR — fails here
    // rather than only in the TUI.
    //
    // A live terminal forces the prompt path, so the merge runs through
    // `handle_confirm_merge`: the one collapse flavour that actually
    // rebadges a terminal, since a silent auto-collapse by definition
    // has no live terminal to move. `handle_confirm_merge` shares the
    // `commit_merge` owner with the silent path, so the order pinned
    // here is the order both paths emit.
    use lazybox_core::{SessionKey, WorkspaceKey};

    let config = ServerConfig::in_memory();
    let (issue_key, _session_id) = seed_issue_with_session(&config, "o/r#71").await;
    attach_live_terminal(&config, &issue_key, 71).await;
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
    let pr_session_key: SessionKey = (&pr_key).into();

    // Subscribe AFTER the prompting upsert so we capture only the
    // accept-path burst.
    let mut bus = config.bus.subscribe();
    polling::handle_confirm_merge(&config, issue_key.clone(), pr_key.clone(), true).await;

    let mut events: Vec<Event> = Vec::new();
    while let Ok(evt) = bus.try_recv() {
        events.push(evt);
    }

    let rebadged_at = events
        .iter()
        .position(|e| matches!(e, Event::TerminalsRebadged { to, .. } if *to == pr_session_key))
        .expect("merge must rebadge the live terminal onto the PR session key");
    let upserted_at = events
        .iter()
        .position(|e| {
            matches!(e, Event::WorkspaceUpserted(ws)
                if ws.key == pr_key && ws.gh_issues.iter().any(|t| t.id.key == "o/r#71"))
        })
        .expect("PR workspace carrying the absorbed issue must be broadcast");
    let removed_at = events
        .iter()
        .position(|e| matches!(e, Event::WorkspaceRemoved(k) if *k == issue_key))
        .expect("issue row must be removed");
    let merged_at = events
        .iter()
        .position(|e| {
            matches!(e, Event::WorkspaceMerged { issue_workspace_key, .. }
                if *issue_workspace_key == issue_key)
        })
        .expect("a WorkspaceMerged notice must follow the removal");

    // I1: terminals rebadged before the issue row disappears, else the
    // TUI drops the moved terminal slots (they still carry the old key).
    assert!(
        rebadged_at < removed_at,
        "TerminalsRebadged ({rebadged_at}) must precede WorkspaceRemoved ({removed_at})",
    );
    // I2: the PR (carrying the moved sessions) is committed before the
    // issue row is removed, else the sessions are momentarily row-less.
    assert!(
        upserted_at < removed_at,
        "WorkspaceUpserted{{pr}} ({upserted_at}) must precede WorkspaceRemoved ({removed_at})",
    );
    // I6: the merge notice trails the removal so the TUI's
    // `merge_follow_from` fires against an already-pruned sidebar.
    assert!(
        removed_at < merged_at,
        "WorkspaceMerged ({merged_at}) must trail WorkspaceRemoved ({removed_at})",
    );
}

#[tokio::test]
async fn branch_name_fallback_collapses_issue_workspace() {
    // A PR whose head branch is the lazybox-named `lazybox/issue-N`
    // worktree branch claims issue #N even when `closes_issues` is
    // empty (the agent forgot the "Closes #N" line and the lazy
    // details fetch hasn't run yet).
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_issue_task("o/r#42")).await;

    let mut pr = make_task("o/r#141");
    pr.branch = Some("lazybox/issue-42".into());
    assert!(pr.closes_issues.is_empty(), "fixture: no closing refs");
    polling::upsert(&config, pr).await;

    let records = config.store.list_workspaces().unwrap();
    assert_eq!(
        records.len(),
        1,
        "issue + branch-linked PR must collapse to one row, got {:?}",
        records.iter().map(|r| &r.key).collect::<Vec<_>>(),
    );
    let ws: lazybox_core::Workspace =
        serde_json::from_str(records[0].workspace_json.as_deref().unwrap()).unwrap();
    assert_eq!(ws.pr.as_ref().unwrap().id.key, "o/r#141");
    assert_eq!(ws.gh_issues.len(), 1);
    assert_eq!(ws.gh_issues[0].id.key, "o/r#42");
}

#[tokio::test]
async fn details_backfill_collapses_issue_workspace() {
    // Regression: the inbox SEARCH_QUERY omits closingIssuesReferences,
    // so a PR's `closes_issues` only arrives via the lazy details
    // fetch. That commit path never re-ran the collapse — the issue
    // workspace sat standalone forever (the next polls saw the refs
    // already attached and the empty-refs early return... never fired
    // because the POLLED task still carried no refs).
    use lazybox_core::{TaskId, WorkspaceKey};

    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_issue_task("o/r#71")).await;
    // PR enters WITHOUT closing refs — two standalone rows.
    polling::upsert(&config, make_task("o/r#141")).await;
    assert_eq!(config.store.list_workspaces().unwrap().len(), 2);

    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#141")));
    let details = lazybox_gh::PrDetails {
        activities: vec![],
        closes_issues: vec![TaskId {
            source: "github".into(),
            key: "o/r#71".into(),
        }],
        checks: vec![],
        ci: CiStatus::Success,
        review: ReviewStatus::Pending,
        role: TaskRole::Reviewer,
        needs_reply: false,
        last_commenter: None,
    };
    polling::apply_pr_details(&config, &pr_key, details).await;

    let records = config.store.list_workspaces().unwrap();
    assert_eq!(
        records.len(),
        1,
        "details backfill must fold the issue workspace into the PR, got {:?}",
        records.iter().map(|r| &r.key).collect::<Vec<_>>(),
    );
    let ws: lazybox_core::Workspace =
        serde_json::from_str(records[0].workspace_json.as_deref().unwrap()).unwrap();
    assert_eq!(ws.key, pr_key);
    assert_eq!(ws.gh_issues.len(), 1);
    assert_eq!(ws.gh_issues[0].id.key, "o/r#71");
}

#[tokio::test]
async fn rescope_delete_does_not_archive_workspace() {
    // Regression: rescope's silent delete routed through the same
    // path as the user's `x x` and ARCHIVED the key — so a
    // workspace deleted for upstream/transient reasons (truncated
    // query, scope edit, reopened PR) was permanently blocked from
    // re-creation by the upsert archive guard.
    use lazybox_core::WorkspaceKey;
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#1")).await;
    polling::upsert(&config, make_task("o/r#2")).await;

    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(lazybox_core::workspace_key_for(
            &make_task("o/r#1"),
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::new(),
        all_full: true,
    };
    polling::rescope(&config, &outcome).await;
    let key2 = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#2")));
    assert!(
        config.store.get_workspace(&key2).unwrap().is_none(),
        "rescope should have deleted the out-of-scope workspace"
    );

    // The item comes back into scope — the workspace must re-create.
    polling::upsert(&config, make_task("o/r#2")).await;
    assert!(
        config.store.get_workspace(&key2).unwrap().is_some(),
        "rescope-deleted workspace must be re-creatable when it returns to scope",
    );
}

#[tokio::test]
async fn user_delete_archives_and_blocks_resurrection() {
    // Counterpart to `rescope_delete_does_not_archive_workspace`: a
    // user-intent delete (`x x`) still archives, so the next poll
    // does NOT resurrect the dismissed row.
    use lazybox_core::WorkspaceKey;
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#1")).await;
    let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#1")));

    assert!(polling::delete_workspace(&config, &key).await);
    polling::upsert(&config, make_task("o/r#1")).await;

    assert!(
        config.store.get_workspace(&key).unwrap().is_none(),
        "user-dismissed workspace must stay gone across polls",
    );
}

#[tokio::test]
async fn unarchive_clears_persisted_and_live_spawn_tombstones() {
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let task = make_task("o/r#restore");
    polling::upsert(&config, task.clone()).await;
    let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
    assert!(polling::delete_workspace(&config, &key).await);
    assert!(polling::load_archived_set(&config).contains(key.as_str()));
    assert!(config.deleted_workspaces.lock().contains(key.as_str()));

    assert!(polling::unarchive_workspace_key(&config, key.as_str()));
    assert!(!polling::load_archived_set(&config).contains(key.as_str()));
    assert!(!config.deleted_workspaces.lock().contains(key.as_str()));

    polling::upsert(&config, task).await;
    assert!(
        config.store.get_workspace(&key).unwrap().is_some(),
        "an unarchived workspace must be able to return and spawn again"
    );
}

#[tokio::test]
async fn unchanged_upsert_skips_store_write_and_broadcast() {
    // Hot-loop waste regression: re-upserting a byte-identical task
    // must not rewrite SQLite or re-broadcast `WorkspaceUpserted` —
    // the steady-state poll was doing both for every workspace every
    // tick.
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#1");
    polling::upsert(&config, task.clone()).await;

    let mut bus = config.bus.subscribe();
    polling::upsert(&config, task).await;

    while let Ok(evt) = bus.try_recv() {
        assert!(
            !matches!(evt, Event::WorkspaceUpserted(_)),
            "identical re-upsert must not re-broadcast WorkspaceUpserted",
        );
    }
}

#[tokio::test]
async fn closing_pr_transfers_live_session_durably_to_pr() {
    // Regression for #173: a LIVE session on the issue must reparent
    // onto the closing PR — including across a daemon restart. The
    // in-memory `terminal_meta` rebadge alone wasn't enough: the
    // persisted `terminal:{backend_key}` record `recover_sessions`
    // reads at startup kept pointing at the (now-deleted) issue
    // workspace, so a restart orphaned the terminal and the session
    // vanished. Assert the persisted record follows the session to the
    // PR key.
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{TerminalId, TerminalKind};

    let config = ServerConfig::in_memory();
    let (issue_key, session_id) = seed_issue_with_session(&config, "o/r#71").await;

    // Stand up a live terminal on the issue: in-memory maps + the
    // persisted record, exactly as `handle_spawn` would have left them.
    let issue_session_key: SessionKey = (&issue_key).into();
    let backend_key = "lazybox-test-o-r-71-claude";
    config.terminal_meta.lock().await.insert(
        TerminalId(7),
        (issue_session_key.clone(), TerminalKind::Shell),
    );
    config
        .terminals
        .lock()
        .await
        .insert(TerminalId(7), backend_key.to_string());
    config
        .store
        .set_kv(
            &format!("terminal:{backend_key}"),
            &serde_json::to_string(&(issue_session_key.as_str(), TerminalKind::Shell)).unwrap(),
        )
        .unwrap();

    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
    polling::handle_confirm_merge(&config, issue_key.clone(), pr_key.clone(), true).await;

    // The session record moved to the PR…
    let pr_ws: lazybox_core::Workspace = {
        let record = config.store.get_workspace(&pr_key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    assert!(
        pr_ws.sessions.iter().any(|s| s.id == session_id),
        "session record must move to the PR workspace",
    );

    // …and so did the live terminal, in memory…
    let pr_session_key: SessionKey = (&pr_key).into();
    {
        let meta = config.terminal_meta.lock().await;
        assert_eq!(
            meta.get(&TerminalId(7)).expect("terminal kept").0,
            pr_session_key,
            "in-memory terminal_meta must repoint at the PR",
        );
    }

    // …and the PERSISTED record `recover_sessions` reads on the next
    // start now resolves to the PR, not the deleted issue workspace.
    let raw = config
        .store
        .get_kv(&format!("terminal:{backend_key}"))
        .unwrap()
        .expect("persisted terminal record must survive the merge");
    let (persisted_key, _kind): (String, TerminalKind) = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        persisted_key,
        pr_session_key.as_str(),
        "persisted terminal record must follow the session to the PR (else a \
         restart reattaches it under the deleted issue workspace and loses it)",
    );
}
#[tokio::test]
async fn adopt_sessions_moves_sessions_between_workspaces() {
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let (source_key, session_id) = seed_issue_with_session(&config, "o/r#71").await;
    polling::upsert(&config, make_task("o/r#999")).await;
    let target_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#999")));

    polling::handle_adopt_sessions(&config, source_key.clone(), target_key.clone()).await;

    // Source still exists (we don't delete it on adopt), but has no
    // sessions left.
    let source_ws: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&source_key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert!(
        source_ws.sessions.is_empty(),
        "source workspace must have lost its sessions after adopt",
    );

    // Target gained the session, rekeyed.
    let target_ws: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&target_key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    let moved = target_ws
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .expect("session must have moved to target");
    assert_eq!(moved.workspace_key, target_key);
}

#[tokio::test]
async fn failed_adopt_batch_cannot_duplicate_or_lose_sessions() {
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::TerminalId;

    let store = Arc::new(FailingBatchStore::new());
    let config = ServerConfig::with_store(store.clone());
    let (source_key, session_id) = seed_issue_with_session(&config, "o/r#904").await;
    polling::upsert(&config, make_task("o/r#905")).await;
    let target_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#905")));
    let backend_key = "failed-adopt-terminal";
    attach_live_terminal_persisted(&config, &source_key, 904, backend_key).await;
    let source_session_key: SessionKey = (&source_key).into();
    let mut bus = config.bus.subscribe();

    store.fail_next_batch();
    polling::handle_adopt_sessions(&config, source_key.clone(), target_key.clone()).await;

    let load = |key: &WorkspaceKey| -> lazybox_core::Workspace {
        let record = config.store.get_workspace(key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    let source = load(&source_key);
    let target = load(&target_key);
    assert!(
        source
            .sessions
            .iter()
            .any(|session| session.id == session_id),
        "a rolled-back adopt must keep the session in its source"
    );
    assert!(
        target
            .sessions
            .iter()
            .all(|session| session.id != session_id),
        "a rolled-back adopt must not duplicate the session into its target"
    );
    assert_eq!(
        config
            .terminal_meta
            .lock()
            .await
            .get(&TerminalId(904))
            .expect("live terminal retained")
            .0,
        source_session_key,
        "a rolled-back adopt must not change in-memory terminal ownership"
    );
    let raw = config
        .store
        .get_kv(&format!("terminal:{backend_key}"))
        .unwrap()
        .expect("persisted terminal metadata retained");
    let (persisted_key, _): (String, lazybox_ipc::TerminalKind) =
        serde_json::from_str(&raw).unwrap();
    assert_eq!(persisted_key, source_session_key.as_str());
    let events: Vec<_> = std::iter::from_fn(|| bus.try_recv().ok()).collect();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TerminalsRebadged { .. })),
        "a rolled-back adopt must publish no terminal rebadge"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_adopt_still_finishes_the_started_commit_and_projection() {
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::TerminalId;

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let store = Arc::new(GatedBatchStore {
        inner: MemoryStore::new(),
        armed: std::sync::atomic::AtomicBool::new(false),
        entered_tx: parking_lot::Mutex::new(Some(entered_tx)),
        release_rx: parking_lot::Mutex::new(Some(release_rx)),
    });
    let config = ServerConfig::with_store(store.clone());
    let (source_key, session_id) = seed_issue_with_session(&config, "o/r#908").await;
    polling::upsert(&config, make_task("o/r#909")).await;
    let target_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#909")));
    let backend_key = "cancelled-adopt-terminal";
    attach_live_terminal_persisted(&config, &source_key, 908, backend_key).await;
    let target_session_key: SessionKey = (&target_key).into();
    let mut bus = config.bus.subscribe();

    store.armed.store(true, Ordering::SeqCst);
    let adopt_config = config.clone();
    let source = source_key.clone();
    let target = target_key.clone();
    let adopt = tokio::spawn(async move {
        polling::handle_adopt_sessions(&adopt_config, source, target).await;
    });
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("adopt transaction owner must start");
    adopt.abort();
    release_tx.send(()).expect("release committed batch");
    let _ = adopt.await;

    let mut saw_rebadge = false;
    let mut saw_target = false;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !(saw_rebadge && saw_target) {
            match bus.recv().await.expect("commit owner event") {
                Event::TerminalsRebadged { to, .. } if to == target_session_key => {
                    saw_rebadge = true;
                }
                Event::WorkspaceUpserted(workspace) if workspace.key == target_key => {
                    saw_target = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("detached commit owner must finish its event projection");

    let load = |key: &WorkspaceKey| -> lazybox_core::Workspace {
        let record = config.store.get_workspace(key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    assert!(
        load(&source_key)
            .sessions
            .iter()
            .all(|session| session.id != session_id)
    );
    assert!(
        load(&target_key)
            .sessions
            .iter()
            .any(|session| session.id == session_id)
    );
    assert_eq!(
        config
            .terminal_meta
            .lock()
            .await
            .get(&TerminalId(908))
            .expect("terminal retained")
            .0,
        target_session_key
    );
    let raw = config
        .store
        .get_kv(&format!("terminal:{backend_key}"))
        .unwrap()
        .expect("terminal metadata persisted");
    let (persisted_key, _): (String, lazybox_ipc::TerminalKind) =
        serde_json::from_str(&raw).unwrap();
    assert_eq!(persisted_key, target_session_key.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_losing_to_merge_cannot_recreate_the_deleted_source() {
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::TerminalKind;

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let store = Arc::new(GatedBatchStore {
        inner: MemoryStore::new(),
        armed: std::sync::atomic::AtomicBool::new(false),
        entered_tx: parking_lot::Mutex::new(Some(entered_tx)),
        release_rx: parking_lot::Mutex::new(Some(release_rx)),
    });
    let backend = MockBackend::new();
    let config = ServerConfig::with_store_and_backend(store.clone(), Arc::new(backend.clone()));
    let (issue_key, session_id) = seed_issue_with_session(&config, "o/r#910").await;
    std::fs::create_dir_all("/tmp/lazybox-test").unwrap();
    let pr_task = make_task("o/r#911");
    polling::upsert(&config, pr_task.clone()).await;
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr_task));

    store.armed.store(true, Ordering::SeqCst);
    let merge_config = config.clone();
    let merge_issue = issue_key.clone();
    let merge_pr = pr_key.clone();
    let merge = tokio::spawn(async move {
        polling::handle_confirm_merge(&merge_config, merge_issue, merge_pr, true).await;
    });
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("merge transaction owner must start");

    let spawn_config = config.clone();
    let source_session_key: SessionKey = (&issue_key).into();
    let spawn = tokio::spawn(async move {
        lazybox_server::spawn_handler::handle_spawn(
            &spawn_config,
            source_session_key,
            Some(session_id),
            TerminalKind::Shell,
            None,
            None,
            false,
            false,
            None,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    release_tx.send(()).expect("release merge batch");
    tokio::time::timeout(Duration::from_secs(2), async {
        merge.await.expect("merge task");
        spawn.await.expect("spawn task");
    })
    .await
    .expect("merge and losing spawn must both terminate");

    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_none(),
        "the losing spawn must not recreate the deleted issue workspace"
    );
    assert!(
        config.terminals.lock().await.is_empty(),
        "the losing spawn must register no terminal under the stale source"
    );
    assert!(
        backend.list().await.unwrap().is_empty(),
        "the losing spawn must abort before starting a backend process"
    );
}

#[tokio::test]
async fn adopt_sessions_into_self_is_a_noop() {
    let config = ServerConfig::in_memory();
    let (source_key, session_id) = seed_issue_with_session(&config, "o/r#71").await;
    polling::handle_adopt_sessions(&config, source_key.clone(), source_key.clone()).await;
    let ws: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&source_key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert!(
        ws.sessions.iter().any(|s| s.id == session_id),
        "self-adopt must leave the session in place",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opposing_adoptions_use_one_lock_order_without_loss_or_deadlock() {
    let config = ServerConfig::in_memory();
    let (left_key, left_session) = seed_issue_with_session(&config, "o/r#906").await;
    let (right_key, right_session) = seed_issue_with_session(&config, "o/r#907").await;

    let left_to_right_config = config.clone();
    let left = left_key.clone();
    let right = right_key.clone();
    let left_to_right = tokio::spawn(async move {
        polling::handle_adopt_sessions(&left_to_right_config, left, right).await;
    });
    let right_to_left_config = config.clone();
    let left = left_key.clone();
    let right = right_key.clone();
    let right_to_left = tokio::spawn(async move {
        polling::handle_adopt_sessions(&right_to_left_config, right, left).await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        left_to_right.await.expect("left-to-right adopt task");
        right_to_left.await.expect("right-to-left adopt task");
    })
    .await
    .expect("opposing multi-key operations must not deadlock");

    let mut ids = Vec::new();
    for key in [&left_key, &right_key] {
        let record = config.store.get_workspace(key).unwrap().unwrap();
        let workspace: lazybox_core::Workspace =
            serde_json::from_str(&record.workspace_json.unwrap()).unwrap();
        ids.extend(workspace.sessions.into_iter().map(|session| session.id));
    }
    let ids: std::collections::HashSet<_> = ids.into_iter().collect();
    let expected = std::collections::HashSet::from([left_session, right_session]);
    assert_eq!(
        ids, expected,
        "serialized opposing moves must preserve each session exactly once"
    );
}

#[tokio::test]
async fn adopt_sessions_rewrites_terminal_meta() {
    // Regression for #7: adopting sessions must repoint the live
    // terminal both in memory AND in the persisted
    // `terminal:{backend_key}` record `recover_sessions` reads at
    // startup. Updating only the in-memory map left the persisted
    // record pointing at the source workspace, so a daemon restart
    // reattached the terminal under the old key and lost the session.
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{TerminalId, TerminalKind};

    let config = ServerConfig::in_memory();
    let (source_key, _session_id) = seed_issue_with_session(&config, "o/r#71").await;
    polling::upsert(&config, make_task("o/r#999")).await;
    let target_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#999")));

    // Stand up a live terminal on the source: in-memory maps + the
    // persisted record, exactly as `handle_spawn` would have left them.
    let source_session_key: SessionKey = (&source_key).into();
    let backend_key = "lazybox-test-o-r-71-claude";
    config.terminal_meta.lock().await.insert(
        TerminalId(7),
        (source_session_key.clone(), TerminalKind::Shell),
    );
    config
        .terminals
        .lock()
        .await
        .insert(TerminalId(7), backend_key.to_string());
    config
        .store
        .set_kv(
            &format!("terminal:{backend_key}"),
            &serde_json::to_string(&(source_session_key.as_str(), TerminalKind::Shell)).unwrap(),
        )
        .unwrap();

    polling::handle_adopt_sessions(&config, source_key.clone(), target_key.clone()).await;

    let target_session_key: SessionKey = (&target_key).into();
    {
        let meta = config.terminal_meta.lock().await;
        let entry = meta.get(&TerminalId(7)).expect("terminal_meta entry kept");
        assert_eq!(
            entry.0, target_session_key,
            "terminal_meta must repoint at the adopt target",
        );
    }

    // The persisted record `recover_sessions` reads on the next start
    // must follow the session to the target, not stay on the source.
    let raw = config
        .store
        .get_kv(&format!("terminal:{backend_key}"))
        .unwrap()
        .expect("persisted terminal record must survive the adopt");
    let (persisted_key, _kind): (String, TerminalKind) = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        persisted_key,
        target_session_key.as_str(),
        "persisted terminal record must follow the session to the adopt target (else a \
         restart reattaches it under the source workspace and loses it)",
    );
}
#[tokio::test]
async fn confirm_merge_reject_pins_against_re_prompting() {
    // User says "no": both workspaces survive, and a subsequent
    // poll of the same PR must NOT re-emit WorkspaceMergePending
    // — otherwise the modal would haunt them every 60 seconds.
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let (issue_key, _) = seed_issue_with_session(&config, "o/r#71").await;
    attach_live_terminal(&config, &issue_key, 71).await;
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));

    polling::handle_confirm_merge(&config, issue_key.clone(), pr_key.clone(), false).await;

    // Drain the bus so we observe the *next* poll's events freshly.
    let mut bus = config.bus.subscribe();
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    let mut saw_pending = false;
    while let Ok(evt) = bus.try_recv() {
        if matches!(evt, Event::WorkspaceMergePending { .. }) {
            saw_pending = true;
        }
    }
    assert!(
        !saw_pending,
        "rejected merges must not re-prompt on the next poll",
    );
    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_some(),
        "rejecting must keep the issue workspace intact",
    );
}

/// Within the dedupe window, a dismissed-but-not-rejected merge must
/// NOT re-emit `WorkspaceMergePending` on every poll — otherwise the
/// modal haunts the user. Mirror of the "rejected" guard above but
/// without the explicit no.
#[tokio::test]
async fn dismissed_merge_does_not_re_emit_within_dedupe_window() {
    let config = ServerConfig::in_memory();
    let (_issue_key, _) = seed_issue_with_session(&config, "o/r#71").await;

    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    // No ConfirmMerge — just simulate the user pressing Esc on the
    // modal. The TUI fix makes this a silent dismissal; the daemon
    // never hears about it.

    // Re-poll the same PR immediately. Without the re-prompt
    // window, the daemon would fire a fresh WorkspaceMergePending
    // every tick.
    let mut bus = config.bus.subscribe();
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    let mut saw_pending = false;
    while let Ok(evt) = bus.try_recv() {
        if matches!(evt, Event::WorkspaceMergePending { .. }) {
            saw_pending = true;
        }
    }
    assert!(
        !saw_pending,
        "dismissed merge prompts must not re-fire on every poll",
    );
}
#[tokio::test]
async fn body_text_referencing_another_pr_does_not_delete_that_pr() {
    // CRITICAL regression: GitHub's `#N` syntax is shared by issues
    // AND PRs. Our body-text fallback parser can't distinguish them
    // from the body alone — a PR whose body says "Closes #141" where
    // #141 is itself a PR used to make us absorb #141's workspace
    // into the closing PR's, then delete it. Result: PRs vanished
    // from the inbox shortly after every poll cycle. The merge code
    // now verifies that the target workspace is an actual issue
    // (no `pr` slot) before touching it.
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#141")).await; // a PR
    let mut pr_166 = make_task("o/r#166");
    pr_166.closes_issues = vec![TaskId {
        source: "github".into(),
        key: "o/r#141".into(), // ← pointing at the OTHER PR by mistake
    }];
    polling::upsert(&config, pr_166).await;

    let key_141 = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_task("o/r#141")));
    assert!(
        config.store.get_workspace(&key_141).unwrap().is_some(),
        "PR #141 must survive — a PR body referencing another PR via \
         `Closes #N` must NOT delete the referenced PR's workspace",
    );
}
#[tokio::test]
async fn pr_with_no_closing_issues_leaves_other_workspaces_alone() {
    // Sanity: the migration only collapses workspaces it has an
    // explicit closing-link for. An unrelated issue keeps its own
    // row.
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_issue_task("o/r#71")).await;
    polling::upsert(&config, make_task("o/r#999")).await; // PR with no closes_issues

    let count = config.store.list_workspaces().unwrap().len();
    assert_eq!(count, 2, "unlinked issue + PR keep separate rows");
}
#[tokio::test]
async fn merge_rewrites_terminal_meta_so_terminals_dont_orphan() {
    // Pre-seed terminal_meta as if a terminal had been spawned
    // against the issue's session_key. A live terminal stalls the
    // auto-merge behind the confirm prompt; once the user accepts,
    // the meta entry must be rebadged to the PR's key — otherwise
    // reconnecting TUI clients see a terminal pointing to a
    // workspace that no longer exists.
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::TerminalId;

    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_issue_task("o/r#71")).await;

    let issue_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_issue_task("o/r#71")));
    attach_live_terminal_persisted(&config, &issue_key, 7, "merge-meta-terminal").await;

    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
    // Live terminal → the upsert prompted instead of merging; the
    // user's "yes" completes the merge.
    polling::handle_confirm_merge(&config, issue_key.clone(), pr_key.clone(), true).await;
    let pr_session_key: SessionKey = (&pr_key).into();
    let meta = config.terminal_meta.lock().await;
    let entry = meta
        .get(&TerminalId(7))
        .expect("terminal_meta still present");
    assert_eq!(
        entry.0, pr_session_key,
        "terminal_meta entry must point at the PR's session_key after merge",
    );
}
// ── retry_after_secs propagation ──────────────────────────────────────

struct ThrottledSource {
    name: String,
    retry_after_secs: u64,
}

impl TaskSource for ThrottledSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        let name = self.name.clone();
        let secs = self.retry_after_secs;
        Box::pin(async move {
            Err(lazybox_core::ProviderError::retryable_after(
                name,
                "rate limited (test)",
                secs,
            ))
        })
    }
}

#[tokio::test]
async fn tick_surfaces_retry_after_from_throttled_source() {
    // The polling driver consults `TickOutcome::retry_after_secs`
    // to extend the sleep between ticks. Verify the per-source
    // hint propagates through `tick_with_state` unchanged.
    let config = ServerConfig::in_memory();
    let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(ThrottledSource {
        name: "github".into(),
        retry_after_secs: 600,
    })];
    let mut state = polling::TickState::default();
    let outcome = polling::tick_with_state(&config, &sources, &mut state).await;
    assert_eq!(outcome.retry_after_secs, Some(600));
}
#[tokio::test]
async fn tick_max_aggregates_retry_after_across_sources() {
    // Two sources both throttled with different hints — the outer
    // driver sleeps the LONGER, not the average. Tighter would
    // re-fire the worse-throttled source mid-window.
    let config = ServerConfig::in_memory();
    let sources: Vec<Box<dyn TaskSource>> = vec![
        Box::new(ThrottledSource {
            name: "github".into(),
            retry_after_secs: 60,
        }),
        Box::new(ThrottledSource {
            name: "linear".into(),
            retry_after_secs: 900,
        }),
    ];
    let mut state = polling::TickState::default();
    let outcome = polling::tick_with_state(&config, &sources, &mut state).await;
    assert_eq!(outcome.retry_after_secs, Some(900));
}
#[tokio::test]
async fn tick_no_retry_after_when_no_source_supplied_a_hint() {
    // Generic retryable errors (no hint) must NOT populate the
    // field. Otherwise the driver would synthesize a sleep where
    // a plain network hiccup should retry on normal cadence.
    let config = ServerConfig::in_memory();
    let sources: Vec<Box<dyn TaskSource>> = vec![Box::new(FailingSource("github".into()))];
    let mut state = polling::TickState::default();
    let outcome = polling::tick_with_state(&config, &sources, &mut state).await;
    assert_eq!(outcome.retry_after_secs, None);
}

// ── poll_wake handle + UNKNOWN-mergeable retry ────────────────────────

#[tokio::test]
async fn poll_wake_fires_an_extra_tick_before_interval() {
    // The long-lived loop sleeps in chunks AND selects against
    // `config.poll_wake.notified()`. Pinging the Notify must make
    // the next tick run before the regular cadence would have
    // delivered it — that's the property Refresh / Subscribe lean
    // on to deliver fresh data on demand.
    let config = ServerConfig::in_memory();
    let wake = config.poll_wake.clone();
    let counter = Arc::new(AtomicUsize::new(0));
    let source: Box<dyn TaskSource> = Box::new(CountingSource {
        name: "test".into(),
        counter: counter.clone(),
    });
    // 10s interval — way longer than the test runs, so any extra
    // tick beyond the eager first one must be wake-driven.
    let handle = polling::spawn_with_sources(config, vec![source], Duration::from_secs(10));
    // Yield until the first eager tick has run.
    tokio::time::sleep(Duration::from_millis(40)).await;
    let first = counter.load(Ordering::SeqCst);
    assert!(
        first >= 1,
        "first eager tick should have landed (got {first})"
    );
    // Ping wake — second tick should land within ~chunk window.
    wake.notify_one();
    tokio::time::sleep(Duration::from_millis(80)).await;
    let after_wake = counter.load(Ordering::SeqCst);
    handle.abort();
    assert!(
        after_wake > first,
        "wake should have produced an extra tick (got {after_wake}, before {first})"
    );
}

#[tokio::test]
async fn tick_outcome_flags_unknown_mergeable_when_any_task_is_unknown() {
    // A single Unknown PR in the result set must light up
    // `saw_unknown_mergeable` so the spawn loop can schedule the
    // quick re-poll that chases GitHub's lazy compute.
    let config = ServerConfig::in_memory();
    let mut t = make_task("o/r#unknown");
    t.mergeable = lazybox_core::Mergeable::Unknown;
    let source: Box<dyn TaskSource> = Box::new(FakeSource {
        name: "github".into(),
        tasks: vec![t],
    });
    let mut state = polling::TickState::default();
    let outcome = polling::tick_with_state(&config, &[source], &mut state).await;
    assert!(
        outcome.saw_unknown_mergeable,
        "Unknown mergeable in result must set the retry flag"
    );
}

#[tokio::test]
async fn tick_outcome_does_not_flag_unknown_when_all_tasks_are_resolved() {
    // Belt-and-braces: an all-resolved tick must NOT request the
    // quick retry, otherwise we'd fire 12 polls/minute just because
    // the regular cadence is 5s and the retry path stayed wired.
    let config = ServerConfig::in_memory();
    let source: Box<dyn TaskSource> = Box::new(FakeSource {
        name: "github".into(),
        tasks: vec![make_task("o/r#1"), make_task("o/r#2")],
    });
    let mut state = polling::TickState::default();
    let outcome = polling::tick_with_state(&config, &[source], &mut state).await;
    assert!(!outcome.saw_unknown_mergeable);
}

// ── FetchMode plumbing (issue #19: notifications-driven sync) ───────

#[tokio::test]
async fn tick_outcome_all_full_default_is_true_when_only_full_sources_present() {
    // Default impl of `TaskSource::last_fetch_kind` returns Full, and
    // every existing source (Linear, all test fakes) inherits it. A
    // tick with only such sources must report `all_full = true` so
    // rescope runs normally — anything else would silently freeze the
    // sidebar.
    let config = ServerConfig::in_memory();
    let source: Box<dyn TaskSource> = Box::new(FakeSource {
        name: "github".into(),
        tasks: vec![make_task("o/r#1")],
    });
    let mut state = polling::TickState::default();
    let outcome = polling::tick_with_state(&config, &[source], &mut state).await;
    assert!(
        outcome.all_full,
        "all-Full sources should not block rescope"
    );
}

#[tokio::test]
async fn tick_outcome_all_full_flips_false_when_any_source_is_incremental() {
    // A single incremental source (the notifications-driven fast path)
    // is enough to disable rescope for the whole tick — incremental
    // results are by definition a subset of in-scope tasks, so trusting
    // them for rescope would delete every untouched workspace.
    let config = ServerConfig::in_memory();
    let sources: Vec<Box<dyn TaskSource>> = vec![
        Box::new(FakeSource {
            name: "linear".into(),
            tasks: vec![],
        }),
        Box::new(IncrementalSource {
            name: "github".into(),
            tasks: vec![make_task("o/r#1")],
        }),
    ];
    let mut state = polling::TickState::default();
    let outcome = polling::tick_with_state(&config, &sources, &mut state).await;
    assert!(
        !outcome.all_full,
        "any incremental source must clear all_full"
    );
}

#[tokio::test]
async fn rescope_skipped_for_incremental_tick_so_untouched_workspaces_survive() {
    // The whole point of `all_full`: an incremental tick that returns
    // only the recently-changed workspaces must NOT cause every other
    // workspace to get deleted. This is the symmetric counterpart to
    // `rescope_with_empty_but_successful_poll_keeps_workspaces` — same
    // shape (some workspaces missing from `polled`) but for the
    // notifications-driven cadence rather than a transient empty
    // response.
    use lazybox_core::WorkspaceKey;
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#untouched-1")).await;
    polling::upsert(&config, make_task("o/r#untouched-2")).await;
    polling::upsert(&config, make_task("o/r#touched")).await;

    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(lazybox_core::workspace_key_for(
            &make_task("o/r#touched"),
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: std::collections::HashMap::new(),
        all_full: false,
    };
    polling::rescope(&config, &outcome).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(
        after.len(),
        3,
        "incremental tick must not delete untouched workspaces — got {after:?}"
    );
}

/// `delete_project` cascades: every workspace whose `project_key`
/// matches is deleted, then the project record itself. `ProjectRemoved`
/// fires last so a TUI client doesn't drop the project before its
/// children's `WorkspaceRemoved` events arrive.
#[tokio::test]
async fn delete_project_cascades_through_workspaces() {
    use lazybox_core::{Project, ProjectKey, WorkspaceKey};

    let config = ServerConfig::in_memory();
    let project_key = ProjectKey::github("acme", "widget");
    let project = Project::new(project_key.clone(), "acme/widget", Utc::now());

    // Two workspaces under this project + one orphan workspace that
    // points at a DIFFERENT project — must survive the cascade.
    let mut ws_a = lazybox_core::Workspace::from_task(make_task("acme/widget#1"), Utc::now());
    ws_a.project_key = Some(project_key.clone());
    let mut ws_b = lazybox_core::Workspace::from_task(make_task("acme/widget#2"), Utc::now());
    ws_b.project_key = Some(project_key.clone());
    let mut other = lazybox_core::Workspace::from_task(make_task("other/repo#9"), Utc::now());
    other.project_key = Some(ProjectKey::github("other", "repo"));

    for ws in [&ws_a, &ws_b, &other] {
        config
            .store
            .save_workspace(&WorkspaceRecord {
                key: ws.key.as_str().to_string(),
                created_at: ws.created_at,
                workspace_json: Some(serde_json::to_string(&ws).unwrap()),
            })
            .unwrap();
    }
    config
        .store
        .save_project(&lazybox_store::ProjectRecord {
            key: project.key.as_str().to_string(),
            created_at: project.created_at,
            project_json: Some(serde_json::to_string(&project).unwrap()),
        })
        .unwrap();

    let mut bus = config.bus.subscribe();
    polling::delete_project(&config, &project_key).await;

    // The two child workspaces are gone, the orphan is not.
    let key_a = WorkspaceKey::new(ws_a.key.as_str());
    let key_b = WorkspaceKey::new(ws_b.key.as_str());
    let key_other = WorkspaceKey::new(other.key.as_str());
    assert!(config.store.get_workspace(&key_a).unwrap().is_none());
    assert!(config.store.get_workspace(&key_b).unwrap().is_none());
    assert!(
        config.store.get_workspace(&key_other).unwrap().is_some(),
        "workspaces in OTHER projects must not be touched by the cascade",
    );
    // Project record itself is gone.
    assert!(
        !config
            .store
            .list_projects()
            .unwrap()
            .iter()
            .any(|p| p.key == project_key.as_str())
    );

    // Drain events: must see WorkspaceRemoved for both, then
    // ProjectRemoved last (so a client doesn't drop the parent
    // before the children).
    let mut removed_workspaces = std::collections::HashSet::new();
    let mut saw_project_removed = false;
    let mut saw_project_removed_after_workspace = false;
    while let Ok(evt) = bus.try_recv() {
        match evt {
            Event::WorkspaceRemoved(k) => {
                removed_workspaces.insert(k.as_str().to_string());
            }
            Event::ProjectRemoved(k) => {
                assert_eq!(k, project_key);
                if !removed_workspaces.is_empty() {
                    saw_project_removed_after_workspace = true;
                }
                saw_project_removed = true;
            }
            _ => {}
        }
    }
    assert!(saw_project_removed, "ProjectRemoved must fire");
    assert!(
        saw_project_removed_after_workspace,
        "ProjectRemoved must come after the children's WorkspaceRemoved events",
    );
    assert!(removed_workspaces.contains(ws_a.key.as_str()));
    assert!(removed_workspaces.contains(ws_b.key.as_str()));
    assert!(
        !removed_workspaces.contains(other.key.as_str()),
        "the orphan workspace must not have been removed",
    );
}

#[tokio::test]
async fn delete_project_preserves_parent_when_a_child_cannot_stop() {
    use lazybox_core::{Project, ProjectKey, SessionKey, WorkspaceKey};
    use lazybox_ipc::{TerminalId, TerminalKind};

    let backend = MockBackend::new();
    let config =
        ServerConfig::with_store_and_backend(Arc::new(MemoryStore::new()), backend.as_backend());
    let project_key = ProjectKey::github("acme", "widget");
    let project = Project::new(project_key.clone(), "acme/widget", Utc::now());
    config
        .store
        .save_project(&lazybox_store::ProjectRecord {
            key: project_key.as_str().into(),
            created_at: project.created_at,
            project_json: Some(serde_json::to_string(&project).unwrap()),
        })
        .unwrap();
    let mut workspace = lazybox_core::Workspace::from_task(make_task("acme/widget#9"), Utc::now());
    workspace.project_key = Some(project_key.clone());
    let workspace_key = WorkspaceKey::new(workspace.key.as_str());
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace_key.as_str().into(),
            created_at: workspace.created_at,
            workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
        })
        .unwrap();

    let terminal_id = TerminalId(79);
    let backend_key = backend
        .spawn(&["codex".into()], None, &[], "project-kill-fails")
        .await
        .unwrap();
    config
        .terminals
        .lock()
        .await
        .insert(terminal_id, backend_key.clone());
    config.terminal_meta.lock().await.insert(
        terminal_id,
        (
            SessionKey::from(workspace_key.as_str()),
            TerminalKind::Agent("codex".into()),
        ),
    );
    backend.fail_kill(&backend_key, "tmux timed out").await;

    polling::delete_project(&config, &project_key).await;

    assert!(
        config
            .store
            .get_workspace(&workspace_key)
            .unwrap()
            .is_some()
    );
    assert!(
        config
            .store
            .list_projects()
            .unwrap()
            .iter()
            .any(|record| record.key == project_key.as_str()),
        "the parent must remain while a child deletion is retryable"
    );
}

#[tokio::test]
async fn delete_project_refuses_to_skip_a_corrupt_workspace_record() {
    use lazybox_core::{Project, ProjectKey};

    let config = ServerConfig::in_memory();
    let project_key = ProjectKey::local("corrupt-cascade");
    let project = Project::new(project_key.clone(), "corrupt cascade", Utc::now());
    config
        .store
        .save_project(&lazybox_store::ProjectRecord {
            key: project_key.as_str().into(),
            created_at: project.created_at,
            project_json: Some(serde_json::to_string(&project).unwrap()),
        })
        .unwrap();
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: "corrupt-child".into(),
            created_at: Utc::now(),
            workspace_json: Some("{ definitely not valid json".into()),
        })
        .unwrap();
    let mut bus = config.bus.subscribe();

    polling::delete_project(&config, &project_key).await;

    assert!(
        config
            .store
            .list_projects()
            .unwrap()
            .iter()
            .any(|record| record.key == project_key.as_str()),
        "unknown child ownership must preserve the parent instead of orphaning data"
    );
    assert!(
        std::iter::from_fn(|| bus.try_recv().ok()).any(|event| matches!(
            event,
            Event::ProviderError { message, .. } if message.contains("corrupt")
        )),
        "the unsafe cascade refusal must be visible to the user"
    );
}

/// Manual `Shift+J` collapse: the user folds an issue workspace
/// into the PR that closes it. Same end state as the auto-merge
/// path but bypasses the dedupe state so a previously-dismissed
/// prompt is actionable again.
#[tokio::test]
async fn collapse_into_pr_folds_issue_workspace_into_claiming_pr() {
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();

    // Seed an issue workspace with a live terminal — this is what
    // triggers the auto-prompt path that the manual flow has to
    // override. Without a live terminal the auto path silently merges
    // and there's nothing for the manual key to do.
    let (issue_key, _session_id) = seed_issue_with_session(&config, "o/r#71").await;
    attach_live_terminal(&config, &issue_key, 71).await;

    // Seed the PR that closes the issue.
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    // Simulate "user dismissed the auto-prompt with No" — pins
    // the issue in `rejected_merge`. The manual trigger must
    // override that pin.
    let pr_key_for_reject = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
    polling::handle_confirm_merge(&config, issue_key.clone(), pr_key_for_reject.clone(), false)
        .await;
    // Sanity: rejecting kept both rows in place.
    assert!(config.store.get_workspace(&issue_key).unwrap().is_some());

    polling::handle_collapse_into_pr(&config, issue_key.clone()).await;

    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_none(),
        "issue workspace must be removed after manual collapse",
    );
    let pr_record = config
        .store
        .get_workspace(&pr_key)
        .unwrap()
        .expect("pr exists");
    let pr_ws: lazybox_core::Workspace =
        serde_json::from_str(&pr_record.workspace_json.unwrap()).unwrap();
    // The absorb path migrates the issue task onto the PR
    // workspace's gh_issues. Verify it landed by task key —
    // seed_issue_with_session uses key "o/r#71" for the issue.
    assert!(
        pr_ws.gh_issues.iter().any(|t| t.id.key == "o/r#71"),
        "issue task should be attached to the PR workspace after collapse",
    );
}

/// `handle_collapse_into_pr` is a no-op when no PR claims the
/// focused issue. The TUI's dispatcher should catch this case
/// first (via local lookup) — this is the belt-and-braces
/// defense in case a stale Command arrives.
#[tokio::test]
async fn collapse_into_pr_is_noop_when_no_claiming_pr_known() {
    use lazybox_core::WorkspaceKey;
    let config = ServerConfig::in_memory();

    // Issue workspace, NO matching PR seeded.
    let issue_task = {
        let mut t = make_task("o/r#71");
        t.url = "https://github.com/o/r/issues/71".into();
        t.branch = None;
        t
    };
    let issue_ws = lazybox_core::Workspace::from_task(issue_task, Utc::now());
    let issue_key = WorkspaceKey::new(issue_ws.key.as_str());
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: issue_key.as_str().to_string(),
            created_at: issue_ws.created_at,
            workspace_json: Some(serde_json::to_string(&issue_ws).unwrap()),
        })
        .unwrap();

    polling::handle_collapse_into_pr(&config, issue_key.clone()).await;

    // Issue workspace still there — nothing was collapsed.
    assert!(config.store.get_workspace(&issue_key).unwrap().is_some());
}

/// Seed an issue workspace carrying `n` distinct session records and
/// return its key alongside the ids. Sessions are DEAD records (no
/// live terminal registered) so the silent auto-merge path absorbs
/// them without prompting. Each session gets a unique worktree path +
/// `created_at` so the post-absorb path migration treats them as
/// genuinely separate worktrees.
async fn seed_issue_with_n_sessions(
    config: &ServerConfig,
    issue_short_key: &str,
    n: usize,
) -> (lazybox_core::WorkspaceKey, Vec<lazybox_core::SessionId>) {
    use lazybox_core::{SessionKind, WorkspaceKey, WorkspaceSession};
    polling::upsert(config, make_issue_task(issue_short_key)).await;
    let issue_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&make_issue_task(
        issue_short_key,
    )));
    let mut issue_ws: lazybox_core::Workspace = {
        let record = config.store.get_workspace(&issue_key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let session_id = lazybox_core::SessionId::new();
        ids.push(session_id);
        issue_ws.add_session(WorkspaceSession {
            id: session_id,
            workspace_key: issue_key.clone(),
            name: format!("claude-{i}"),
            kind: SessionKind::Agent {
                agent_id: "claude".into(),
            },
            state: lazybox_core::SessionRunState::Active,
            worktree_path: std::path::PathBuf::from(format!(
                "/tmp/lazybox-test/{}-{i}",
                issue_key.as_str()
            )),
            created_at: Utc::now() + chrono::Duration::seconds(i as i64),
            last_output_at: None,
            layout: lazybox_core::SessionLayout::default(),
        });
    }
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: issue_key.as_str().to_string(),
            created_at: issue_ws.created_at,
            workspace_json: Some(serde_json::to_string(&issue_ws).unwrap()),
        })
        .unwrap();
    (issue_key, ids)
}

#[tokio::test]
async fn combining_multiple_issues_with_multiple_sessions_preserves_every_session() {
    // Regression for #161 (data loss): a PR that closes SEVERAL issues,
    // each carrying SEVERAL sessions, must fold every issue workspace
    // into the PR and carry ALL N×M sessions across. PR #90 only
    // covered the single-issue / single-session join; the multi-issue,
    // multi-session combine is what dropped sessions.
    use lazybox_core::WorkspaceKey;

    let config = ServerConfig::in_memory();

    const N_ISSUES: usize = 3;
    const M_SESSIONS: usize = 2;

    let issue_short_keys = ["o/r#71", "o/r#72", "o/r#73"];
    let mut expected_ids: Vec<lazybox_core::SessionId> = Vec::new();
    let mut issue_keys: Vec<WorkspaceKey> = Vec::new();
    for short in issue_short_keys.iter().take(N_ISSUES) {
        let (issue_key, ids) = seed_issue_with_n_sessions(&config, short, M_SESSIONS).await;
        expected_ids.extend(ids);
        issue_keys.push(issue_key);
    }
    assert_eq!(expected_ids.len(), N_ISSUES * M_SESSIONS);

    // One PR closing all N issues — the "combine multiple issues into a
    // PR" event. Drives the silent auto-merge path (no live terminals).
    let pr = make_pr_closing("o/r#141", &issue_short_keys[..N_ISSUES]);
    polling::upsert(&config, pr.clone()).await;
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr));

    // Every issue row collapsed away.
    for issue_key in &issue_keys {
        assert!(
            config.store.get_workspace(issue_key).unwrap().is_none(),
            "issue workspace {issue_key} must be removed after combine",
        );
    }

    // The PR workspace must hold all N×M sessions, each rekeyed onto it.
    let pr_ws: lazybox_core::Workspace = {
        let record = config.store.get_workspace(&pr_key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    assert_eq!(
        pr_ws.sessions.len(),
        N_ISSUES * M_SESSIONS,
        "all {} sessions must survive the combine, found {}",
        N_ISSUES * M_SESSIONS,
        pr_ws.sessions.len(),
    );
    for id in &expected_ids {
        let moved = pr_ws
            .sessions
            .iter()
            .find(|s| s.id == *id)
            .unwrap_or_else(|| panic!("session {id} lost during combine"));
        assert_eq!(
            moved.workspace_key, pr_key,
            "session {id} must be rekeyed onto the PR workspace",
        );
    }
}

/// Register a live terminal bound to `key`'s session_key — the
/// in-memory `terminal_meta`/`terminals` maps AND the persisted
/// `terminal:{backend_key}` record `recover_sessions` reads at
/// startup — exactly as `handle_spawn` would have left it. Returns
/// the backend key so the caller can assert the persisted record
/// followed the merge.
async fn attach_live_terminal_persisted(
    config: &ServerConfig,
    key: &lazybox_core::WorkspaceKey,
    terminal_id: u64,
    backend_key: &str,
) {
    use lazybox_core::SessionKey;
    use lazybox_ipc::{TerminalId, TerminalKind};
    let session_key: SessionKey = key.into();
    config.terminal_meta.lock().await.insert(
        TerminalId(terminal_id),
        (session_key.clone(), TerminalKind::Shell),
    );
    config
        .terminals
        .lock()
        .await
        .insert(TerminalId(terminal_id), backend_key.to_string());
    config
        .store
        .set_kv(
            &format!("terminal:{backend_key}"),
            &serde_json::to_string(&(session_key.as_str(), TerminalKind::Shell)).unwrap(),
        )
        .unwrap();
}

#[tokio::test]
async fn combining_multiple_issues_with_live_sessions_rebadges_every_terminal() {
    // Regression for #161 (data loss), live-terminal variant: combining
    // SEVERAL issues that each carry SEVERAL LIVE sessions must carry
    // every terminal onto the PR — in memory AND in the persisted
    // records `recover_sessions` reads at startup. A live terminal
    // stalls the silent auto-merge behind a per-issue confirm prompt;
    // the user accepting each one drives `handle_confirm_merge`. This is
    // the path that exercises the atomic terminal-rebadge owner with more than one
    // terminal per issue — the case PR #90 never covered.
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{TerminalId, TerminalKind};

    let config = ServerConfig::in_memory();

    let issue_short_keys = ["o/r#71", "o/r#72"];
    let mut expected_session_ids: Vec<lazybox_core::SessionId> = Vec::new();
    let mut issue_keys: Vec<WorkspaceKey> = Vec::new();
    // (terminal_id, backend_key) for every live terminal we stand up.
    let mut terminals: Vec<(u64, String)> = Vec::new();

    let mut next_terminal_id = 1u64;
    for (issue_idx, short) in issue_short_keys.iter().enumerate() {
        // Two dead-but-recoverable session records per issue…
        let (issue_key, ids) = seed_issue_with_n_sessions(&config, short, 2).await;
        expected_session_ids.extend(ids);
        // …plus two LIVE terminals per issue.
        for term_in_issue in 0..2 {
            let backend_key = format!("lazybox-test-{issue_idx}-{term_in_issue}");
            attach_live_terminal_persisted(&config, &issue_key, next_terminal_id, &backend_key)
                .await;
            terminals.push((next_terminal_id, backend_key));
            next_terminal_id += 1;
        }
        issue_keys.push(issue_key);
    }

    // PR closing both issues.
    let pr = make_pr_closing("o/r#141", &issue_short_keys);
    polling::upsert(&config, pr.clone()).await;
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr));
    let pr_session_key: SessionKey = (&pr_key).into();

    // User accepts the per-issue merge prompt for each combined issue.
    for issue_key in &issue_keys {
        polling::handle_confirm_merge(&config, issue_key.clone(), pr_key.clone(), true).await;
    }

    // Every issue row is gone…
    for issue_key in &issue_keys {
        assert!(
            config.store.get_workspace(issue_key).unwrap().is_none(),
            "issue workspace {issue_key} must be removed after combine",
        );
    }

    // …all session records survive on the PR…
    let pr_ws: lazybox_core::Workspace = {
        let record = config.store.get_workspace(&pr_key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    assert_eq!(
        pr_ws.sessions.len(),
        expected_session_ids.len(),
        "every session record must survive the combine",
    );
    for id in &expected_session_ids {
        assert!(
            pr_ws.sessions.iter().any(|s| s.id == *id),
            "session {id} lost during combine",
        );
    }

    // …every live terminal followed onto the PR, in memory…
    {
        let meta = config.terminal_meta.lock().await;
        for (tid, _) in &terminals {
            let entry = meta
                .get(&TerminalId(*tid))
                .unwrap_or_else(|| panic!("terminal {tid} dropped during combine"));
            assert_eq!(
                entry.0, pr_session_key,
                "terminal {tid} must repoint at the PR after combine",
            );
        }
    }

    // …and in the persisted records `recover_sessions` reads at startup
    // (else a restart reattaches each terminal under its deleted issue
    // workspace and the session is lost).
    for (tid, backend_key) in &terminals {
        let raw = config
            .store
            .get_kv(&format!("terminal:{backend_key}"))
            .unwrap()
            .unwrap_or_else(|| panic!("persisted record for terminal {tid} must survive"));
        let (persisted_key, _kind): (String, TerminalKind) = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            persisted_key,
            pr_session_key.as_str(),
            "persisted record for terminal {tid} must follow the session to the PR",
        );
    }
}

#[cfg(test)]
mod live_collapse_e2e {
    //! Issue #205, the HARDCORE end-to-end: drive a real `x j`
    //! collapse through the actual serve loop — a Client talking to a
    //! Server over the in-process channel, a real agent terminal spawned
    //! via `Command::Spawn`, its state driven by real PTY output / hooks,
    //! and the merge run through the real `Command::CollapseIntoPr`
    //! handler. The assertions separate the two failure modes that four
    //! prior reports kept conflating:
    //!
    //! - **not lost** — the `Session` record AND the PTY/ring-buffer
    //!   still exist under the PR key after the merge.
    //! - **live key** — every `Event::AgentState` emitter (PTY pump,
    //!   optimistic flip, hook ingest) broadcasts under the PR session
    //!   after the rebadge, never the captured issue key (#161/#167).
    //!
    //! The TUI-render half of "not shown" lives in the tui crate
    //! (`model::tests::collapse_into_pr_tests`,
    //! `sidebar::tests::rebadge_attention_tests`) — only that crate can
    //! build a `Model` — and consumes the exact `TerminalsRebadged`
    //! burst this loop emits.
    use super::*;
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{AgentState, Client, TerminalId, TerminalKind};
    use lazybox_server::backend::{MockBackend, SessionBackend};
    use tokio::time::timeout;

    /// Per-await budget for a single expected event. Must exceed the
    /// pump's ~5s quiet window (#289): a PTY-raised `InputNeeded` only
    /// surfaces once the stream has been silent past it. The tests that
    /// wait on one run under paused time, so the budget costs nothing
    /// in wall-clock.
    const DEADLINE: Duration = Duration::from_secs(10);
    /// Whole-test budget — comfortably larger than any single
    /// [`DEADLINE`] wait so the outer guard only fires on a true hang.
    const TEST_DEADLINE: Duration = Duration::from_secs(60);

    /// A Claude chooser screen — the PTY detector reads this as
    /// `InputNeeded` (mirrors the spawn_handler.rs e2e fixtures).
    const CHOOSER: &str = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel";

    /// Drain events until one is an `AgentState` for `state`; return the
    /// session key it was broadcast under. `None` on timeout.
    async fn agent_state_key(client: &mut Client, state: AgentState) -> Option<SessionKey> {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
            match timeout(remaining, client.recv()).await {
                Ok(Some(Event::AgentState {
                    session_key,
                    state: got,
                    ..
                })) if got == state => return Some(session_key),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }

    async fn wait_spawned(client: &mut Client) -> TerminalId {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .expect("TerminalSpawned deadline");
            if let Ok(Some(Event::TerminalSpawned { terminal_id, .. })) =
                timeout(remaining, client.recv()).await
            {
                return terminal_id;
            }
        }
    }

    /// A live agent terminal spawned on an issue workspace through the
    /// real serve loop. `_tmp` is the worktree cwd, held alive for the
    /// test's lifetime.
    struct LiveAgent {
        client: Client,
        config: ServerConfig,
        mock: MockBackend,
        issue_key: WorkspaceKey,
        issue_sk: SessionKey,
        terminal_id: TerminalId,
        backend_key: String,
        _tmp: tempfile::TempDir,
    }

    /// `agent_id` picks the built-in agent to spawn — `"claude"` for the
    /// hook-carrying default, `"codex"` for the hookless PTY-only path.
    async fn spawn_live_agent_on_issue(agent_id: &str) -> LiveAgent {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let (mut client, server) = channel::pair();
        {
            let cfg = config.clone();
            tokio::spawn(async move {
                let _ = Server::new(cfg).serve(server).await;
            });
        }
        client.send(Command::Subscribe).unwrap();
        let _snapshot = client.recv().await.expect("snapshot");

        // An issue workspace carrying a session record.
        let (issue_key, _ids) = seed_issue_with_n_sessions(&config, "o/r#71", 1).await;
        let issue_sk: SessionKey = (&issue_key).into();

        // Spawn a REAL agent terminal on the issue. `cwd: Some` keeps the
        // spawn off the worktree-provisioning path so the test is fast
        // and filesystem-free, while still exercising the full
        // handle_spawn → backend → pump → terminal_meta pipeline.
        let tmp = tempfile::tempdir().unwrap();
        client
            .send(Command::Spawn {
                model_alias: None,
                session_key: issue_sk.clone(),
                session_id: None,
                kind: TerminalKind::Agent(agent_id.into()),
                cwd: Some(tmp.path().to_string_lossy().into_owned()),
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        let terminal_id = wait_spawned(&mut client).await;
        let backend_key = mock.list().await.unwrap().into_iter().next().unwrap();

        LiveAgent {
            client,
            config,
            mock,
            issue_key,
            issue_sk,
            terminal_id,
            backend_key,
            _tmp: tmp,
        }
    }

    /// Bring in a PR closing the issue (a live terminal stalls the silent
    /// auto-merge) and accept the merge via the real `x j` command
    /// (`CollapseIntoPr`). Returns the PR session key once the merge has
    /// fully committed.
    ///
    /// The collapse handler runs as a concurrent daemon task and emits
    /// its burst in order: `TerminalsRebadged` (during the absorb) →
    /// `WorkspaceUpserted(pr)` → `WorkspaceRemoved(issue)` →
    /// `WorkspaceMerged` (after the commit). We drain to the terminal
    /// `WorkspaceMerged` so store assertions don't race a half-finished
    /// merge, while asserting the rebadge was seen en route.
    async fn collapse_into_pr(live: &mut LiveAgent) -> SessionKey {
        let pr_task = make_pr_closing("o/r#141", &["o/r#71"]);
        let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr_task));
        let pr_sk: SessionKey = (&pr_key).into();
        polling::upsert(&live.config, pr_task).await;

        live.client
            .send(Command::CollapseIntoPr {
                issue_workspace_key: live.issue_sk.clone(),
            })
            .unwrap();

        let mut saw_rebadge = false;
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .expect("collapse burst deadline");
            match timeout(remaining, live.client.recv()).await {
                Ok(Some(Event::TerminalsRebadged { from, to })) => {
                    assert_eq!(from, live.issue_sk, "rebadge must move FROM the issue");
                    assert_eq!(to, pr_sk, "rebadge must move TO the PR");
                    saw_rebadge = true;
                }
                Ok(Some(Event::WorkspaceMerged {
                    issue_workspace_key,
                    pr_workspace_key,
                    ..
                })) => {
                    assert_eq!(SessionKey::from(&issue_workspace_key), live.issue_sk);
                    assert_eq!(SessionKey::from(&pr_workspace_key), pr_sk);
                    break;
                }
                Ok(Some(_)) => continue,
                _ => panic!("collapse burst did not complete with WorkspaceMerged"),
            }
        }
        assert!(
            saw_rebadge,
            "collapse must broadcast TerminalsRebadged before WorkspaceMerged"
        );
        pr_sk
    }

    async fn load_workspace(config: &ServerConfig, key: &WorkspaceKey) -> lazybox_core::Workspace {
        let record = config.store.get_workspace(key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    }

    // Paused time: the PTY `?` only surfaces after the ~5s quiet window
    // (screen-scrape classification is quiet-gated, #289), so the test
    // rides tokio's auto-advance instead of sleeping for real.
    #[tokio::test(start_paused = true)]
    async fn collapse_keeps_a_live_input_needed_agent_session() {
        timeout(TEST_DEADLINE, async {
            let mut live = spawn_live_agent_on_issue("claude").await;

            // Park the agent on a prompt — the case that reads as "lost".
            // The `?` surfaces once the PTY has been quiet past the
            // classify window (a dialog freezes all output).
            live.mock.emit(&live.backend_key, CHOOSER).await;
            let asking = agent_state_key(&mut live.client, AgentState::InputNeeded)
                .await
                .expect("PTY chooser must raise InputNeeded");
            assert_eq!(
                asking, live.issue_sk,
                "raised under the issue before collapse"
            );

            let pr_sk = collapse_into_pr(&mut live).await;
            let pr_key = WorkspaceKey::new(pr_sk.as_str().to_string());

            // ── NOT LOST (record) — the Session moved onto the PR. ──
            let pr_ws = load_workspace(&live.config, &pr_key).await;
            assert_eq!(
                pr_ws.sessions.len(),
                1,
                "the issue's session record must survive on the PR",
            );
            assert!(
                live.config
                    .store
                    .get_workspace(&live.issue_key)
                    .unwrap()
                    .is_none(),
                "the issue row is gone",
            );

            // ── NOT LOST (PTY) — the backend session/ring-buffer lives. ──
            assert!(
                live.mock.list().await.unwrap().contains(&live.backend_key),
                "the agent's PTY/ring-buffer must still exist after the merge",
            );

            // …keyed to the PR in memory…
            assert_eq!(
                live.config
                    .terminal_meta
                    .lock()
                    .await
                    .get(&live.terminal_id)
                    .expect("terminal must survive the merge")
                    .0,
                pr_sk,
                "terminal_meta must repoint the terminal at the PR",
            );
            // …and in the persisted record `recover_sessions` reads at
            // startup (else a restart reattaches it under the deleted
            // issue workspace and the session is lost).
            let raw = live
                .config
                .store
                .get_kv(&format!("terminal:{}", live.backend_key))
                .unwrap()
                .expect("persisted terminal record must survive");
            let (persisted, _kind): (String, TerminalKind) = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                persisted,
                pr_sk.as_str(),
                "persisted terminal record must follow the session to the PR",
            );
        })
        .await
        .expect("deadline");
    }

    // Paused time: the PTY `?` only surfaces after the ~5s quiet window
    // (screen-scrape classification is quiet-gated, #289), so the test
    // rides tokio's auto-advance instead of sleeping for real.
    #[tokio::test(start_paused = true)]
    async fn every_agent_state_emitter_uses_the_live_session_after_collapse() {
        // The #205 acceptance: no emitter may broadcast under a key
        // captured at spawn time. After the rebadge, drive each of the
        // three emitters and assert every one resolves the LIVE (PR)
        // session — if any captured the issue key at spawn, the key it
        // emits under would still be the issue and the assert fails.
        timeout(TEST_DEADLINE, async {
            let mut live = spawn_live_agent_on_issue("claude").await;
            live.mock.emit(&live.backend_key, CHOOSER).await;
            agent_state_key(&mut live.client, AgentState::InputNeeded)
                .await
                .expect("InputNeeded before collapse");

            let pr_sk = collapse_into_pr(&mut live).await;

            // (1) Optimistic flip — user answers the prompt. `handle_write`
            //     flips InputNeeded → Working.
            live.client
                .send(Command::Write {
                    terminal_id: live.terminal_id,
                    bytes: b"1\r".to_vec(),
                })
                .unwrap();
            let flip = agent_state_key(&mut live.client, AgentState::Working)
                .await
                .expect("flip must emit Working");
            assert_eq!(flip, pr_sk, "optimistic flip must use the live PR key");

            // (2) PTY pump — fresh chooser output re-raises InputNeeded.
            live.mock.emit(&live.backend_key, CHOOSER).await;
            let pty = agent_state_key(&mut live.client, AgentState::InputNeeded)
                .await
                .expect("PTY pump must re-raise InputNeeded");
            assert_eq!(
                pty, pr_sk,
                "PTY pump must use the live PR key (the #161 path)"
            );

            // (3) Hook ingest — a Stop lifecycle hook drives Done.
            live.client
                .send(Command::IngestHook {
                    terminal_id: live.terminal_id,
                    hook: lazybox_ipc::HookEvent {
                        kind: lazybox_ipc::HookEventKind::Stop,
                        session_id: Some("claude-session".into()),
                        cwd: None,
                        tool_name: None,
                        notification: None,
                    },
                    backend_key: Some(live.backend_key.clone()),
                })
                .unwrap();
            let hook = agent_state_key(&mut live.client, AgentState::Done)
                .await
                .expect("hook must emit Done");
            assert_eq!(hook, pr_sk, "hook ingest must use the live PR key");
        })
        .await
        .expect("deadline");
    }

    #[tokio::test]
    async fn join_immediately_after_spawn_rebadges_the_terminal() {
        // The spawn-then-join race (#90, previously untested): a user
        // spawns an agent and hits `x j` before touching it — the
        // terminal carries no AgentState yet. The rebadge must still
        // catch it, or the fresh terminal is orphaned under the deleted
        // issue key.
        timeout(TEST_DEADLINE, async {
            let mut live = spawn_live_agent_on_issue("claude").await;
            // No InputNeeded driving — collapse straight away.
            let pr_sk = collapse_into_pr(&mut live).await;

            assert_eq!(
                live.config
                    .terminal_meta
                    .lock()
                    .await
                    .get(&live.terminal_id)
                    .expect("freshly-spawned terminal must survive the immediate join")
                    .0,
                pr_sk,
                "the terminal must be rebadged onto the PR even with no prior state",
            );
            let raw = live
                .config
                .store
                .get_kv(&format!("terminal:{}", live.backend_key))
                .unwrap()
                .expect("persisted record must survive the immediate join");
            let (persisted, _kind): (String, TerminalKind) = serde_json::from_str(&raw).unwrap();
            assert_eq!(persisted, pr_sk.as_str());
        })
        .await
        .expect("deadline");
    }

    /// Poll a `Command::Write` through the serve loop and wait for the
    /// bytes to land in the mock backend under `backend_key` — proof the
    /// terminal is still wired to the SAME backend session.
    async fn write_reaches_backend(live: &mut LiveAgent, bytes: &[u8]) {
        live.client
            .send(Command::Write {
                terminal_id: live.terminal_id,
                bytes: bytes.to_vec(),
            })
            .unwrap();
        let want = bytes.to_vec();
        timeout(DEADLINE, async {
            loop {
                if live
                    .mock
                    .writes_for(&live.backend_key)
                    .await
                    .contains(&want)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("write must reach the original backend session");
    }

    #[tokio::test]
    async fn codex_terminal_survives_issue_to_pr_collapse() {
        // Issue #404, the hookless-agent gap: every prior transfer test
        // spawns the hardcoded `claude` agent, whose lifecycle hooks give
        // the daemon a second routing channel. Codex has none — its
        // terminal is tracked purely through the PTY pump — so a rebadge
        // bug scoped to the hook-free path would never fail a test. Spawn
        // a live codex terminal on an issue, collapse into the claiming
        // PR, and pin the user-visible outcome: rebadged, not killed,
        // still writable, persisted on the PR.
        timeout(TEST_DEADLINE, async {
            let mut live = spawn_live_agent_on_issue("codex").await;

            // Sanity: the backend really runs codex, not claude.
            let argv = live
                .mock
                .argv_for(&live.backend_key)
                .await
                .expect("spawned session must have argv");
            assert_eq!(
                argv.first().map(String::as_str),
                Some("codex"),
                "the live agent under test must be codex (hookless)",
            );

            // Rebadge burst asserted inside: TerminalsRebadged moves
            // FROM the issue TO the PR before WorkspaceMerged commits.
            let pr_sk = collapse_into_pr(&mut live).await;
            let pr_key = WorkspaceKey::new(pr_sk.as_str().to_string());

            // ── NOT LOST (record) — the session record moved to the PR. ──
            let pr_ws = load_workspace(&live.config, &pr_key).await;
            assert_eq!(
                pr_ws.sessions.len(),
                1,
                "the issue's session record must survive on the PR",
            );
            assert!(
                live.config
                    .store
                    .get_workspace(&live.issue_key)
                    .unwrap()
                    .is_none(),
                "the issue row is gone",
            );

            // ── NOT KILLED — the backend session survived the merge. ──
            assert!(
                live.mock.list().await.unwrap().contains(&live.backend_key),
                "the codex PTY/ring-buffer must still exist after the merge",
            );
            assert!(
                !live.mock.released_keys().await.contains(&live.backend_key),
                "the merge must not tear down the codex backend session",
            );

            // ── STILL WIRED — in-memory meta repointed at the PR… ──
            assert_eq!(
                live.config
                    .terminal_meta
                    .lock()
                    .await
                    .get(&live.terminal_id)
                    .expect("terminal must survive the merge")
                    .0,
                pr_sk,
                "terminal_meta must repoint the codex terminal at the PR",
            );
            // …writes still reach the SAME backend session (the rebadge
            // moved the routing, not the PTY)…
            write_reaches_backend(&mut live, b"still-wired\r").await;
            // …and the persisted record `recover_sessions` reads at
            // startup follows too.
            let raw = live
                .config
                .store
                .get_kv(&format!("terminal:{}", live.backend_key))
                .unwrap()
                .expect("persisted terminal record must survive");
            let (persisted, _kind): (String, TerminalKind) = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                persisted,
                pr_sk.as_str(),
                "persisted terminal record must follow the codex session to the PR",
            );
        })
        .await
        .expect("deadline");
    }

    #[tokio::test]
    async fn pr_arriving_after_live_spawn_prompts_then_confirmed_merge_rebadges() {
        // Issue #404, the deferred-arrival gap: the natural lifecycle is
        // to work the issue FIRST — the agent is spawned while no PR
        // exists anywhere — and only a LATER poll delivers the PR that
        // closes it. Prior tests either seed the PR before driving the
        // merge or fabricate the terminal and call the handlers directly;
        // none runs poll-arrival → `WorkspaceMergePending` →
        // `Command::ConfirmMerge` through the real serve loop with a real
        // spawned terminal. The live-terminal safety gate must stall the
        // silent merge, and the user's accept must run the full rebadge
        // without ever killing the session.
        timeout(TEST_DEADLINE, async {
            // Bare issue, live agent, NO PR anywhere yet.
            let mut live = spawn_live_agent_on_issue("claude").await;

            // A later poll upserts the PR claiming the issue.
            let pr_task = make_pr_closing("o/r#141", &["o/r#71"]);
            let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr_task));
            let pr_sk: SessionKey = (&pr_key).into();
            polling::upsert(&live.config, pr_task).await;

            // ── GATE — the live terminal stalls the merge behind a
            // prompt; any merge/rebadge before the user confirms is the
            // #404 silent-loss bug.
            let deadline = tokio::time::Instant::now() + DEADLINE;
            loop {
                let remaining = deadline
                    .checked_duration_since(tokio::time::Instant::now())
                    .expect("WorkspaceMergePending deadline");
                match timeout(remaining, live.client.recv()).await {
                    Ok(Some(Event::WorkspaceMergePending {
                        issue_workspace_key,
                        pr_workspace_key,
                        active_terminal_count,
                        ..
                    })) => {
                        assert_eq!(issue_workspace_key, live.issue_key);
                        assert_eq!(pr_workspace_key, pr_key);
                        assert_eq!(
                            active_terminal_count, 1,
                            "the prompt must quote the live terminal it is protecting",
                        );
                        break;
                    }
                    Ok(Some(Event::WorkspaceMerged { .. })) => {
                        panic!("a live-terminal issue must NOT merge silently")
                    }
                    Ok(Some(Event::TerminalsRebadged { .. })) => {
                        panic!("no rebadge may run before the user confirms")
                    }
                    Ok(Some(_)) => continue,
                    _ => panic!("expected a WorkspaceMergePending prompt"),
                }
            }
            // Both rows still stand, session and terminal untouched.
            assert!(
                live.config
                    .store
                    .get_workspace(&live.issue_key)
                    .unwrap()
                    .is_some(),
                "the issue workspace must survive until the user confirms",
            );
            assert!(live.config.store.get_workspace(&pr_key).unwrap().is_some());
            assert_eq!(
                load_workspace(&live.config, &live.issue_key)
                    .await
                    .sessions
                    .len(),
                1,
                "the session record must still live on the issue while pending",
            );
            assert_eq!(
                live.config
                    .terminal_meta
                    .lock()
                    .await
                    .get(&live.terminal_id)
                    .expect("terminal must be untouched while pending")
                    .0,
                live.issue_sk,
                "the terminal must stay keyed to the issue while pending",
            );

            // ── ACCEPT — the user answers the prompt via the real
            // command; the merge must emit TerminalsRebadged (issue→PR)
            // before committing with WorkspaceMerged.
            live.client
                .send(Command::ConfirmMerge {
                    issue_workspace_key: live.issue_key.clone(),
                    pr_workspace_key: pr_key.clone(),
                    accept: true,
                })
                .unwrap();
            let mut saw_rebadge = false;
            let deadline = tokio::time::Instant::now() + DEADLINE;
            loop {
                let remaining = deadline
                    .checked_duration_since(tokio::time::Instant::now())
                    .expect("confirm burst deadline");
                match timeout(remaining, live.client.recv()).await {
                    Ok(Some(Event::TerminalsRebadged { from, to })) => {
                        assert_eq!(from, live.issue_sk, "rebadge must move FROM the issue");
                        assert_eq!(to, pr_sk, "rebadge must move TO the PR");
                        saw_rebadge = true;
                    }
                    Ok(Some(Event::WorkspaceMerged {
                        issue_workspace_key,
                        pr_workspace_key,
                        ..
                    })) => {
                        assert_eq!(issue_workspace_key, live.issue_key);
                        assert_eq!(pr_workspace_key, pr_key);
                        break;
                    }
                    Ok(Some(_)) => continue,
                    _ => panic!("accepted merge did not complete with WorkspaceMerged"),
                }
            }
            assert!(
                saw_rebadge,
                "accepted merge must broadcast TerminalsRebadged before WorkspaceMerged"
            );

            // ── OUTCOME — session + terminal live on the PR, backend
            // never killed.
            assert!(
                live.config
                    .store
                    .get_workspace(&live.issue_key)
                    .unwrap()
                    .is_none(),
                "the issue row is gone after the accepted merge",
            );
            let pr_ws = load_workspace(&live.config, &pr_key).await;
            assert_eq!(
                pr_ws.sessions.len(),
                1,
                "the issue's session record must survive on the PR",
            );
            assert!(
                live.mock.list().await.unwrap().contains(&live.backend_key),
                "the agent's PTY/ring-buffer must still exist after the merge",
            );
            assert!(
                !live.mock.released_keys().await.contains(&live.backend_key),
                "the merge must not tear down the backend session",
            );
            assert_eq!(
                live.config
                    .terminal_meta
                    .lock()
                    .await
                    .get(&live.terminal_id)
                    .expect("terminal must survive the merge")
                    .0,
                pr_sk,
                "terminal_meta must repoint the terminal at the PR",
            );
            write_reaches_backend(&mut live, b"post-merge\r").await;
            let raw = live
                .config
                .store
                .get_kv(&format!("terminal:{}", live.backend_key))
                .unwrap()
                .expect("persisted terminal record must survive");
            let (persisted, _kind): (String, TerminalKind) = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                persisted,
                pr_sk.as_str(),
                "persisted terminal record must follow the session to the PR",
            );
        })
        .await
        .expect("deadline");
    }
}

/// `delete_project` on a project with NO workspaces still removes
/// the project record and fires `ProjectRemoved`. Covers the "user
/// just made a local project, hasn't added a workspace yet, presses
/// x x" path.
#[tokio::test]
async fn delete_project_with_no_workspaces_still_removes_project() {
    use lazybox_core::{Project, ProjectKey};

    let config = ServerConfig::in_memory();
    let project_key = ProjectKey::local("scratch");
    let project = Project::new(project_key.clone(), "scratch", Utc::now());
    config
        .store
        .save_project(&lazybox_store::ProjectRecord {
            key: project.key.as_str().to_string(),
            created_at: project.created_at,
            project_json: Some(serde_json::to_string(&project).unwrap()),
        })
        .unwrap();

    let mut bus = config.bus.subscribe();
    polling::delete_project(&config, &project_key).await;

    assert!(
        !config
            .store
            .list_projects()
            .unwrap()
            .iter()
            .any(|p| p.key == project_key.as_str())
    );

    let mut saw = false;
    while let Ok(evt) = bus.try_recv() {
        if let Event::ProjectRemoved(k) = evt
            && k == project_key
        {
            saw = true;
        }
    }
    assert!(saw, "ProjectRemoved must fire even with no workspaces");
}

// ── ProviderAction dispatch ────────────────────────────────────────
//
// The polling tick must drain side-effect actions surfaced by a
// source after `fetch()` and route them through `handle_spawn`. The
// `@lazybox`-mention auto-spawn path depends on this glue — without
// `drain_actions` running, the eyes-reacted comment never produces
// a terminal.

#[tokio::test]
async fn tick_dispatches_auto_spawn_action_after_upsert() {
    let (config, _mock) = ServerConfig::in_memory_with_mock();
    let mut bus_rx = config.bus.subscribe();

    // Build a synthetic task + matching auto-spawn action. The
    // session key MUST match `workspace_key_for(task)` because
    // `handle_spawn` uses it to find / create the workspace.
    let mut task = make_task("o/r#101");
    // This test owns provider-action dispatch, not remote cloning. A
    // repo-less task provisions a standalone git session inside the
    // config's scratch worktree root and keeps the fixture offline.
    task.repo = None;
    let session_key = lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(&task));
    let action = polling::ProviderAction::AutoSpawnAgent {
        session_key: session_key.clone(),
        agent_id: "claude".to_string(),
        prompt: Some("Implement issue".to_string()),
        reason: "@lazybox mention by alice on o/r#101 (issue body)".to_string(),
    };

    let source: Box<dyn TaskSource> = Box::new(ActionEmittingSource {
        name: "github".into(),
        tasks: vec![task],
        actions: std::sync::Mutex::new(vec![action]),
    });
    polling::tick(&config, &[source]).await;

    // Walk the bus and collect events. `TerminalSpawned` proves the
    // dispatch ran — the action flowed source → drain → handle_spawn.
    let mut saw_spawn = false;
    let mut saw_upsert = false;
    while let Ok(evt) = bus_rx.try_recv() {
        match evt {
            Event::TerminalSpawned {
                session_key: sk, ..
            } => {
                assert_eq!(sk, session_key, "spawned in the right workspace");
                saw_spawn = true;
            }
            Event::WorkspaceUpserted(_) => saw_upsert = true,
            _ => {}
        }
    }
    assert!(
        saw_upsert,
        "task upsert ran before action dispatch — required so spawn lands in an existing workspace"
    );
    assert!(
        saw_spawn,
        "AutoSpawnAgent action must trigger TerminalSpawned"
    );
}

// ── @lazybox ingest: mentioned issues survive the display filter ──────
//
// Regression for issue #50. The `@lazybox` auto-spawn targets the
// mentioned issue's workspace key, so that issue's Task must be
// upserted even when the user's display filter would drop it (PR-only
// inbox, role/scope mismatch). Otherwise `handle_spawn` finds no
// workspace and spawns the agent in lazybox's own cwd with no branch.

#[test]
fn readmit_keeps_mentioned_issue_dropped_by_filter() {
    // Display filter kept only a PR; the `@lazybox`-mentioned issue was
    // dropped. It must be re-admitted so its workspace gets created.
    let kept = vec![make_task("o/r#1")]; // a PR that passed the filter
    let mentioned = vec![make_issue_task("o/r#42")];
    let out = polling::readmit_mentioned_tasks(kept, mentioned);
    assert_eq!(out.len(), 2, "mentioned issue must be re-admitted");
    assert!(
        out.iter().any(|t| t.id.key == "o/r#42"),
        "the @lazybox-mentioned issue is present so its workspace gets built"
    );
}

#[test]
fn readmit_does_not_duplicate_already_kept_mention() {
    // The mentioned issue ALSO passed the display filter (issues are
    // enabled). Re-admitting must not create a duplicate workspace.
    let issue = make_issue_task("o/r#42");
    let kept = vec![issue.clone()];
    let out = polling::readmit_mentioned_tasks(kept, vec![issue]);
    assert_eq!(out.len(), 1, "no duplicate task for an already-kept issue");
}

#[test]
fn readmit_is_noop_without_mentions() {
    let kept = vec![make_task("o/r#1")];
    let out = polling::readmit_mentioned_tasks(kept.clone(), vec![]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id.key, kept[0].id.key);
}

#[tokio::test]
async fn tick_dispatches_auto_fix_action_spawns_agent() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let mut bus_rx = config.bus.subscribe();

    let mut task = make_task("o/r#202");
    task.repo = None;
    let session_key = lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(&task));
    let action = polling::ProviderAction::AutoFixPr {
        session_key: session_key.clone(),
        agent_id: "claude".to_string(),
        prompt: Some("Fix the failing CI".to_string()),
        repo: "o/r".to_string(),
        pr_number: 202,
        kind: lazybox_core::AutoFixKind::CiFailure,
        opted_out: false,
        settings: lazybox_core::AutoFixSettings {
            enabled: true,
            ..Default::default()
        },
        reason: "auto-fix (fixing CI) on o/r#202".to_string(),
    };

    let source: Box<dyn TaskSource> = Box::new(ActionEmittingSource {
        name: "github".into(),
        tasks: vec![task],
        actions: std::sync::Mutex::new(vec![action]),
    });
    polling::tick(&config, &[source]).await;

    let mut saw_spawn = false;
    while let Ok(evt) = bus_rx.try_recv() {
        if let Event::TerminalSpawned {
            session_key: sk, ..
        } = evt
        {
            assert_eq!(sk, session_key, "auto-fix spawned in the PR's workspace");
            saw_spawn = true;
        }
    }
    assert!(
        saw_spawn,
        "AutoFixPr action under its attempt budget must trigger TerminalSpawned"
    );

    // The auto-fix agent runs unattended on a possibly-fresh worktree,
    // so it must be launched with `--dangerously-skip-permissions` —
    // otherwise the first-run workspace-trust dialog eats the injected
    // fix prompt and any later Edit/Bash approval deadlocks the run.
    let argvs = mock.all_argv().await;
    assert!(
        argvs
            .iter()
            .any(|a| a.first().map(String::as_str) == Some("claude")
                && a.iter().any(|s| s == "--dangerously-skip-permissions")),
        "auto-fix spawn must pass --dangerously-skip-permissions; got {argvs:?}"
    );
}

/// Seed a workspace record carrying `policies`, so the dispatcher's
/// per-session auto-fix gate (issue #363) reads a non-default arm.
fn seed_workspace_with_policy(
    config: &ServerConfig,
    task: &Task,
    kind: lazybox_core::AutoFixKind,
    arm: lazybox_core::PolicyArm,
) {
    let key = lazybox_core::WorkspaceKey::new(lazybox_core::workspace_key_for(task));
    let mut ws = lazybox_core::Workspace::from_task(task.clone(), Utc::now());
    ws.policies.set(kind, arm);
    let record = lazybox_store::WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: ws.created_at,
        workspace_json: serde_json::to_string(&ws).ok(),
    };
    config.store.save_workspace(&record).unwrap();
}

#[tokio::test]
async fn auto_fix_disarm_policy_skips_spawn() {
    // A workspace that explicitly disarmed CI auto-fix must not spawn,
    // even though the action is otherwise eligible and under budget.
    let (config, _mock) = ServerConfig::in_memory_with_mock();
    let mut bus_rx = config.bus.subscribe();

    let task = make_task("o/r#505");
    let session_key = lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(&task));
    seed_workspace_with_policy(
        &config,
        &task,
        lazybox_core::AutoFixKind::CiFailure,
        lazybox_core::PolicyArm::Disarm,
    );

    let action = polling::ProviderAction::AutoFixPr {
        session_key,
        agent_id: "claude".to_string(),
        prompt: Some("Fix CI".to_string()),
        repo: "o/r".to_string(),
        pr_number: 505,
        kind: lazybox_core::AutoFixKind::CiFailure,
        opted_out: false,
        settings: lazybox_core::AutoFixSettings {
            enabled: true,
            ..Default::default()
        },
        reason: "auto-fix (fixing CI) on o/r#505".to_string(),
    };
    // Empty task list: the tick only dispatches the action against the
    // pre-seeded (disarmed) workspace, no re-upsert.
    let source: Box<dyn TaskSource> = Box::new(ActionEmittingSource {
        name: "github".into(),
        tasks: vec![],
        actions: std::sync::Mutex::new(vec![action]),
    });
    polling::tick(&config, &[source]).await;

    let mut saw_spawn = false;
    while let Ok(evt) = bus_rx.try_recv() {
        if let Event::TerminalSpawned { .. } = evt {
            saw_spawn = true;
        }
    }
    assert!(!saw_spawn, "a disarmed auto-fix policy must skip the spawn");
}

#[tokio::test]
async fn auto_fix_arm_overrides_label_opt_out() {
    // A PR carrying an opt-out label arrives with `opted_out: true`;
    // an explicit per-session Arm overrides the label and spawns.
    let (config, _mock) = ServerConfig::in_memory_with_mock();
    let mut bus_rx = config.bus.subscribe();

    let mut task = make_task("o/r#606");
    task.repo = None;
    let session_key = lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(&task));
    seed_workspace_with_policy(
        &config,
        &task,
        lazybox_core::AutoFixKind::CiFailure,
        lazybox_core::PolicyArm::Arm,
    );

    let action = polling::ProviderAction::AutoFixPr {
        session_key: session_key.clone(),
        agent_id: "claude".to_string(),
        prompt: Some("Fix CI".to_string()),
        repo: "o/r".to_string(),
        pr_number: 606,
        kind: lazybox_core::AutoFixKind::CiFailure,
        opted_out: true,
        settings: lazybox_core::AutoFixSettings {
            enabled: true,
            ..Default::default()
        },
        reason: "auto-fix (fixing CI) on o/r#606".to_string(),
    };
    let source: Box<dyn TaskSource> = Box::new(ActionEmittingSource {
        name: "github".into(),
        tasks: vec![],
        actions: std::sync::Mutex::new(vec![action]),
    });
    polling::tick(&config, &[source]).await;

    let mut saw_spawn = false;
    while let Ok(evt) = bus_rx.try_recv() {
        if let Event::TerminalSpawned {
            session_key: sk, ..
        } = evt
        {
            assert_eq!(sk, session_key);
            saw_spawn = true;
        }
    }
    assert!(
        saw_spawn,
        "an explicit Arm must override a label opt-out and spawn"
    );
}

#[tokio::test]
async fn tick_auto_fix_respects_exhausted_budget() {
    // `max_attempts: 0` means the very first dispatch is already over
    // budget — the dispatcher must NOT spawn. Proves the stateful
    // cooldown / max-attempts guard actually gates the spawn (not just
    // the pure eligibility check).
    let (config, _mock) = ServerConfig::in_memory_with_mock();
    let mut bus_rx = config.bus.subscribe();

    let task = make_task("o/r#303");
    let session_key = lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(&task));
    let action = polling::ProviderAction::AutoFixPr {
        session_key,
        agent_id: "claude".to_string(),
        prompt: Some("Fix the failing CI".to_string()),
        repo: "o/r".to_string(),
        pr_number: 303,
        kind: lazybox_core::AutoFixKind::CiFailure,
        opted_out: false,
        settings: lazybox_core::AutoFixSettings {
            enabled: true,
            max_attempts: 0,
            ..Default::default()
        },
        reason: "auto-fix (fixing CI) on o/r#303".to_string(),
    };

    let source: Box<dyn TaskSource> = Box::new(ActionEmittingSource {
        name: "github".into(),
        tasks: vec![task],
        actions: std::sync::Mutex::new(vec![action]),
    });
    polling::tick(&config, &[source]).await;

    let mut saw_spawn = false;
    let mut saw_upsert = false;
    while let Ok(evt) = bus_rx.try_recv() {
        match evt {
            Event::TerminalSpawned { .. } => saw_spawn = true,
            Event::WorkspaceUpserted(_) => saw_upsert = true,
            _ => {}
        }
    }
    assert!(saw_upsert, "the task should still upsert normally");
    assert!(
        !saw_spawn,
        "an exhausted auto-fix budget must NOT spawn an agent"
    );
}

#[tokio::test]
async fn auto_fix_skips_and_burns_no_attempt_while_agent_already_running() {
    // Regression: the trigger persists across polls and a fix can run
    // longer than the cooldown. If a fix agent is already running on
    // the PR, a subsequent auto-fix dispatch must skip BEFORE touching
    // the attempt counter — otherwise a slow agent silently exhausts
    // the budget + spams duplicate "I'm fixing this" comments.
    let (config, _mock) = ServerConfig::in_memory_with_mock();
    let mut task = make_task("o/r#404");
    task.repo = None;
    let session_key = lazybox_core::SessionKey::new(lazybox_core::workspace_key_for(&task));
    let make_action = || polling::ProviderAction::AutoFixPr {
        session_key: session_key.clone(),
        agent_id: "claude".to_string(),
        prompt: Some("Fix CI".to_string()),
        repo: "o/r".to_string(),
        pr_number: 404,
        kind: lazybox_core::AutoFixKind::CiFailure,
        opted_out: false,
        settings: lazybox_core::AutoFixSettings {
            enabled: true,
            ..Default::default()
        },
        reason: "auto-fix (fixing CI) on o/r#404".to_string(),
    };

    // Tick 1: no agent yet → spawns + records attempt 1.
    let src1: Box<dyn TaskSource> = Box::new(ActionEmittingSource {
        name: "github".into(),
        tasks: vec![task.clone()],
        actions: std::sync::Mutex::new(vec![make_action()]),
    });
    polling::tick(&config, &[src1]).await;

    // Tick 2: the agent is now running → must skip without spawning
    // again or burning another attempt.
    let mut bus_rx = config.bus.subscribe();
    let src2: Box<dyn TaskSource> = Box::new(ActionEmittingSource {
        name: "github".into(),
        tasks: vec![task.clone()],
        actions: std::sync::Mutex::new(vec![make_action()]),
    });
    polling::tick(&config, &[src2]).await;

    let mut saw_spawn = false;
    while let Ok(evt) = bus_rx.try_recv() {
        if let Event::TerminalSpawned { .. } = evt {
            saw_spawn = true;
        }
    }
    assert!(
        !saw_spawn,
        "auto-fix must not spawn a second agent while one is already running"
    );

    // The attempt counter must still read 1 — tick 2 skipped before
    // `check_and_record`. (AttemptRecord is private; assert on the JSON.)
    let rec = config
        .store
        .get_kv(&format!("autofix:{}:ci", session_key.as_str()))
        .unwrap()
        .expect("attempt record persisted on the first dispatch");
    assert!(
        rec.contains("\"attempts\":1"),
        "tick 2 must NOT increment the attempt counter (got: {rec})"
    );
}

/// An empty workspace created under a GitHub project key (the
/// self/local add path — no upstream task carries `owner/repo`)
/// registers its project with a `owner/repo` display name, not the
/// raw `github-owner-repo` key. Regression for the lazybox project
/// rendering its raw key in the sidebar.
#[tokio::test]
async fn empty_workspace_registers_project_with_pretty_name() {
    use lazybox_core::ProjectKey;

    let config = ServerConfig::in_memory();
    let project_key = ProjectKey::github("AntoineToussaint", "lazybox");
    polling::create_empty_workspace(&config, "scratch", project_key.clone());

    let record = config
        .store
        .get_project(&project_key)
        .unwrap()
        .expect("project registered for the empty workspace");
    let project: lazybox_core::Project =
        serde_json::from_str(&record.project_json.unwrap()).unwrap();
    assert_eq!(project.name, "AntoineToussaint/lazybox");
}

/// `LinearSource` coverage wiring: a Linear fetch that loses a page
/// mid-pagination is non-authoritative, so `polled_scope` must report
/// `Repos([])` ("covered no repos this tick") rather than
/// `Exhaustive`. Without this, the next rescope treats every Linear
/// workspace on the un-fetched pages as "gone upstream" and deletes
/// it — the same wipe-on-partial-sync class as issue #64.
mod linear_coverage {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use lazybox_core::ProviderConfig;
    use lazybox_ipc::Event;
    use lazybox_linear::LinearClient;
    use lazybox_server::polling::{LinearSource, PolledScope, TaskSource};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    struct Mock {
        addr: SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
    }

    impl Mock {
        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
        fn shutdown(mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
        }
    }

    /// Serve `responses[i]` for the i-th request in order; exhausted
    /// indices fall back to `{}` (which deserializes to no-data and
    /// surfaces as an error if the client ever asks for more).
    async fn spawn_mock(responses: Vec<String>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let responses = Arc::new(responses);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => return,
                    accept = listener.accept() => {
                        let Ok((stream, _)) = accept else { continue };
                        let responses = responses.clone();
                        let counter = counter.clone();
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                                let responses = responses.clone();
                                let counter = counter.clone();
                                async move {
                                    let _ = req.into_body().collect().await;
                                    let idx = counter
                                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

        Mock {
            addr,
            shutdown: Some(shutdown_tx),
        }
    }

    fn viewer() -> String {
        serde_json::json!({ "data": { "viewer": { "id": "me", "name": "Me" } } }).to_string()
    }

    fn issues_page(has_next: bool, cursor: Option<&str>) -> String {
        serde_json::json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": has_next, "endCursor": cursor },
                    "nodes": [{
                        "id": "a", "identifier": "ENG-1", "title": "one",
                        "description": null, "url": "https://l.app/1",
                        "updatedAt": "2026-01-01T00:00:00Z", "priority": null,
                        "state": { "name": "", "type": "unstarted" },
                        "assignee": null, "creator": null,
                        "team": { "key": "ENG" }, "labels": { "nodes": [] }
                    }]
                }
            }
        })
        .to_string()
    }

    fn source(url: String) -> LinearSource {
        let bus = tokio::sync::broadcast::channel::<Event>(16).0;
        LinearSource::new(
            LinearClient::with_key("k").with_endpoint(url),
            ProviderConfig::default(),
            bus,
        )
    }

    #[tokio::test]
    async fn partial_fetch_downgrades_polled_scope_to_non_authoritative() {
        // viewer + page1 (has_next) + page2 errors → partial.
        let page2_error =
            serde_json::json!({ "errors": [{ "message": "boom on page 2" }] }).to_string();
        let mock = spawn_mock(vec![viewer(), issues_page(true, Some("cur")), page2_error]).await;
        let source = source(mock.url());

        tokio::time::timeout(Duration::from_secs(5), source.fetch())
            .await
            .expect("fetch timeout")
            .expect("partial fetch still returns the prefix as Ok");

        assert_eq!(
            source.polled_scope(),
            PolledScope::Repos(Vec::new()),
            "a truncated Linear pagination must not authorize rescope deletions"
        );

        mock.shutdown();
    }

    #[tokio::test]
    async fn complete_fetch_keeps_polled_scope_exhaustive() {
        let mock = spawn_mock(vec![viewer(), issues_page(false, None)]).await;
        let source = source(mock.url());

        tokio::time::timeout(Duration::from_secs(5), source.fetch())
            .await
            .expect("fetch timeout")
            .expect("complete fetch returns Ok");

        assert_eq!(
            source.polled_scope(),
            PolledScope::Exhaustive,
            "a fully-paginated Linear fetch stays authoritative"
        );

        mock.shutdown();
    }
}

/// Build a full-sweep outcome that polled only `polled_key` with
/// github reported as exhaustive — the shape of a `Shift-R` refresh.
fn refresh_outcome(polled_key: &str) -> polling::TickOutcome {
    use lazybox_core::WorkspaceKey;
    let mut scopes = std::collections::HashMap::new();
    scopes.insert("github".to_string(), polling::PolledScope::Exhaustive);
    polling::TickOutcome {
        polled: vec![WorkspaceKey::new(lazybox_core::workspace_key_for(
            &make_task(polled_key),
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
        source_scopes: scopes,
        all_full: true,
    }
}

#[tokio::test]
async fn create_empty_workspace_marks_local() {
    let config = ServerConfig::in_memory();
    let key = polling::create_empty_workspace(
        &config,
        "my sandbox",
        lazybox_core::ProjectKey::local("test"),
    );
    let ws: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    assert!(ws.local, "hand-created workspace must be flagged local");
}

#[tokio::test]
async fn rescope_preserves_manual_workspace(/* issue #87 */) {
    let config = ServerConfig::in_memory();
    // A provider workspace that the poll DOES return.
    polling::upsert(&config, make_task("o/r#current")).await;
    // A hand-created workspace (`n` key) — no PR/issue, never polled.
    let manual = polling::create_empty_workspace(
        &config,
        "my sandbox",
        lazybox_core::ProjectKey::local("test"),
    );

    polling::rescope(&config, &refresh_outcome("o/r#current")).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(
        after.iter().any(|k| k == manual.as_str()),
        "manual workspace must survive refresh; got: {after:?}"
    );
}

#[tokio::test]
async fn rescope_preserves_manual_workspace_that_gained_a_pr(/* issue #87 */) {
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#current")).await;
    // Hand-created workspace that later gained a PR (e.g. an agent
    // opened one). The fragile task-shape heuristic would no longer
    // recognise it as local; the explicit `local` flag still does.
    let manual = polling::create_empty_workspace(
        &config,
        "my sandbox",
        lazybox_core::ProjectKey::local("test"),
    );
    let mut ws: lazybox_core::Workspace = serde_json::from_str(
        &config
            .store
            .get_workspace(&manual)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap();
    ws.pr = Some(make_task("o/r#999"));
    config
        .store
        .save_workspace(&lazybox_store::WorkspaceRecord {
            key: manual.as_str().to_string(),
            created_at: ws.created_at,
            workspace_json: serde_json::to_string(&ws).ok(),
        })
        .unwrap();

    polling::rescope(&config, &refresh_outcome("o/r#current")).await;

    let after: Vec<String> = config
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(
        after.iter().any(|k| k == manual.as_str()),
        "manual workspace (with PR) must survive refresh; got: {after:?}"
    );
}
