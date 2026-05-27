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
use pilot_core::{
    Activity, ActivityKind, CiStatus, ProviderConfig, ReviewStatus, Task, TaskId, TaskRole,
    TaskState,
};
use pilot_ipc::{Command, Event, channel};
use pilot_server::polling::{self, TaskSource};
use pilot_server::{Server, ServerConfig};
use pilot_store::WorkspaceRecord;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Wait for the next `WorkspaceUpserted` event, ignoring any
/// `ProjectUpserted` (registered the first time polling sees a new
/// repo). Tests that drain "one event per workspace upsert" can use
/// this instead of `client.recv()` to stay robust to the Project
/// auto-register firing alongside the workspace.
async fn recv_workspace_upsert(client: &mut pilot_ipc::Client) -> Event {
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
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: pilot_core::Mergeable::Mergeable,
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
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>>
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
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(pilot_core::ProviderError::retryable(
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
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>>
    {
        let counter = self.counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        })
    }
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
    let mut workspace = pilot_core::Workspace::from_task(task.clone(), Utc::now());
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
    let parsed: pilot_core::Workspace =
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
    let key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&mk()));
    let stored = config.store.get_workspace(&key).unwrap().unwrap();
    let workspace: pilot_core::Workspace =
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
            // full diagnostic lives in /tmp/pilot.log. For a
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

    let key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&make_task("o/r#11")));
    let before: pilot_core::Workspace = serde_json::from_str(
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

    polling::mark_workspace_read(&config, &key);

    let after: pilot_core::Workspace = serde_json::from_str(
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

    let key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&make_task("o/r#22")));
    polling::mark_workspace_read(&config, &key);

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

    let key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&make_task("o/r#33")));
    polling::mark_workspace_read(&config, &key);

    // Re-poll the same task — seen state survives.
    polling::upsert(&config, task).await;

    let stored: pilot_core::Workspace = serde_json::from_str(
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
    let key = pilot_core::WorkspaceKey::new("github:o/r#nope");
    polling::mark_workspace_read(&config, &key);
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
    use pilot_core::WorkspaceSession;
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#11");
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        pilot_core::SessionKind::Shell,
        std::path::PathBuf::from("/tmp/pilot-nonexistent-old-path"),
        Utc::now(),
    );
    ws.add_session(session);

    let moved =
        pilot_server::spawn_handler::migrate_session_paths_if_needed(&config, &mut ws).await;
    assert!(moved, "stale path detected → migrated record");

    let expected = pilot_server::spawn_handler::worktree_root().join(ws.worktree_slug());
    assert_eq!(
        ws.sessions[0].worktree_path, expected,
        "session path now matches the slug-derived path"
    );
}
#[tokio::test]
async fn migrate_no_op_when_path_already_matches() {
    use pilot_core::WorkspaceSession;
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#22");
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let expected = pilot_server::spawn_handler::worktree_root().join(ws.worktree_slug());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        pilot_core::SessionKind::Shell,
        expected.clone(),
        Utc::now(),
    );
    ws.add_session(session);

    let moved =
        pilot_server::spawn_handler::migrate_session_paths_if_needed(&config, &mut ws).await;
    assert!(!moved, "path already matches → migration is a no-op");
    assert_eq!(ws.sessions[0].worktree_path, expected);
}
#[tokio::test]
async fn migrate_handles_zero_sessions() {
    let config = ServerConfig::in_memory();
    let task = make_task("o/r#33");
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let moved =
        pilot_server::spawn_handler::migrate_session_paths_if_needed(&config, &mut ws).await;
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
    // The real git-worktree-move branch isn't covered here because
    // it requires a real bare clone — different test file's job.
    use pilot_core::WorkspaceSession;
    let config = ServerConfig::in_memory();

    // Build the workspace with the ORIGINAL title.
    let mut task = make_task("o/r#7413");
    task.title = "Initial draft".into();
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws.key.clone(),
        pilot_core::SessionKind::Shell,
        // Use a path that DOES match the original slug so we can
        // assert it changes after the rename — same fallback the
        // production spawn handler uses.
        pilot_server::spawn_handler::worktree_root().join(ws.worktree_slug()),
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
        pilot_server::spawn_handler::migrate_session_paths_if_needed(&config, &mut ws).await;
    assert!(moved, "rename must trigger migration");

    let expected = pilot_server::spawn_handler::worktree_root().join(&renamed_slug);
    assert_eq!(
        ws.sessions[0].worktree_path, expected,
        "session path follows the new slug after migration",
    );
    assert_ne!(
        ws.sessions[0].worktree_path, original_path,
        "session path actually changed (not a no-op)",
    );
}
// ── Create empty workspace (n key flow) ──────────────────────────────

#[tokio::test]
async fn create_empty_workspace_persists_with_user_name() {
    let config = ServerConfig::in_memory();
    let key = polling::create_empty_workspace(
        &config,
        "fix login flow",
        pilot_core::ProjectKey::local("test"),
    );
    assert_eq!(
        key.as_str(),
        "fix-login-flow",
        "workspace key is the slugified name"
    );
    let stored: pilot_core::Workspace = serde_json::from_str(
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
        pilot_core::ProjectKey::local("test"),
    );
    let k2 = polling::create_empty_workspace(
        &config,
        "Refactor auth",
        pilot_core::ProjectKey::local("test"),
    );
    let k3 = polling::create_empty_workspace(
        &config,
        "Refactor auth",
        pilot_core::ProjectKey::local("test"),
    );
    assert_eq!(k1.as_str(), "refactor-auth");
    assert_eq!(k2.as_str(), "refactor-auth-2");
    assert_eq!(k3.as_str(), "refactor-auth-3");
}
#[tokio::test]
async fn create_empty_workspace_falls_back_when_name_is_unsluggable() {
    let config = ServerConfig::in_memory();
    let k = polling::create_empty_workspace(&config, "🚀✨", pilot_core::ProjectKey::local("test"));
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
        pilot_core::ProjectKey::local("test"),
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
    let key = pilot_core::WorkspaceKey::new("sandbox");
    let workspace = pilot_core::Workspace::empty(key.clone(), "main", Utc::now());
    assert!(workspace.project_key.is_none());
    let record = pilot_store::WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    config.store.save_workspace(&record).unwrap();

    polling::migrate_legacy_sandbox(&config);

    // Workspace now carries the local-sandbox project key.
    let migrated: pilot_core::Workspace = serde_json::from_str(
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
        Some(pilot_core::ProjectKey::local("sandbox"))
    );
    // And the project record was registered.
    let project_key = pilot_core::ProjectKey::local("sandbox");
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
    let key = pilot_core::WorkspaceKey::new("sandbox");
    let mut workspace = pilot_core::Workspace::empty(key.clone(), "main", Utc::now());
    workspace.project_key = Some(pilot_core::ProjectKey::local("custom"));
    let record = pilot_store::WorkspaceRecord {
        key: key.as_str().to_string(),
        created_at: workspace.created_at,
        workspace_json: serde_json::to_string(&workspace).ok(),
    };
    config.store.save_workspace(&record).unwrap();

    polling::migrate_legacy_sandbox(&config);

    let after: pilot_core::Workspace = serde_json::from_str(
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
        Some(pilot_core::ProjectKey::local("custom")),
        "migration must not overwrite an existing project_key"
    );
}

// ── Session layout persistence ───────────────────────────────────────

#[tokio::test]
async fn set_session_layout_persists_and_broadcasts() {
    use pilot_core::{SessionKind, SessionLayout, TileTree, WorkspaceSession};
    let config = ServerConfig::in_memory();

    // Seed a workspace with one session.
    let task = make_task("o/r#1");
    let ws_key = pilot_core::WorkspaceKey::new(pilot_core::workspace_key_for(&task));
    let mut ws = pilot_core::Workspace::from_task(task, Utc::now());
    let session = WorkspaceSession::new(
        ws_key.clone(),
        SessionKind::Shell,
        std::path::PathBuf::from("/tmp/pilot-test"),
        Utc::now(),
    );
    let session_id = session.id;
    ws.add_session(session);
    config
        .store
        .save_workspace(&pilot_store::WorkspaceRecord {
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
    polling::set_session_layout(&config, &ws_key, session_id, layout.clone());

    // Reload + verify.
    let stored: pilot_core::Workspace = serde_json::from_str(
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
    use pilot_core::SessionLayout;
    let config = ServerConfig::in_memory();
    let key = pilot_core::WorkspaceKey::new("github:none");
    // Should not panic when neither workspace nor session exist.
    polling::set_session_layout(
        &config,
        &key,
        pilot_core::SessionId::new(),
        SessionLayout::default(),
    );
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
fn pr_qualifiers_or_multiple_scopes_inside_parens() {
    let mut scopes = std::collections::BTreeSet::new();
    scopes.insert("github:acme".to_string());
    scopes.insert("github:widgets/core".to_string());
    let quals = polling::build_pr_search_qualifiers(&fully_open_filter(), &scopes, "alice");
    assert_eq!(
        quals,
        vec!["involves:alice", "(org:acme OR repo:widgets/core)"]
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
    use pilot_core::WorkspaceKey;
    let config = ServerConfig::in_memory();
    // Seed with an existing workspace (was in scope last poll).
    polling::upsert(&config, make_task("o/r#stale")).await;
    polling::upsert(&config, make_task("o/r#current")).await;

    // Simulate a new poll that returns only `#current` — `#stale`
    // fell out of scope (filter change, repo unsubscribed, …).
    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(pilot_core::workspace_key_for(
            &make_task("o/r#current"),
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
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
    use pilot_core::{SessionKey, WorkspaceKey};
    use pilot_ipc::{TerminalId, TerminalKind};
    let config = ServerConfig::in_memory();
    let mut bus_rx = config.bus.subscribe();
    polling::upsert(&config, make_task("o/r#alive")).await;
    polling::upsert(&config, make_task("o/r#kept-elsewhere")).await;

    // Stash a terminal pointing at `#alive` so rescope sees it as
    // "has active session". `terminal_meta` is the source of truth
    // the production code consults.
    let session_key: SessionKey =
        SessionKey::from(pilot_core::workspace_key_for(&make_task("o/r#alive")));
    config
        .terminal_meta
        .lock()
        .await
        .insert(TerminalId(7), (session_key, TerminalKind::Shell));

    // Poll returns only `#kept-elsewhere` — `#alive` is out of
    // scope but has a live terminal.
    let outcome = polling::TickOutcome {
        polled: vec![WorkspaceKey::new(pilot_core::workspace_key_for(
            &make_task("o/r#kept-elsewhere"),
        ))],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
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
    // rows via Shift-X / Settings → Clean.
    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#ghost-1")).await;
    polling::upsert(&config, make_task("o/r#ghost-2")).await;
    let outcome = polling::TickOutcome {
        polled: vec![],
        any_source_succeeded: true,
        retry_after_secs: None,
        saw_unknown_mergeable: false,
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
async fn delete_workspace_kills_terminals_via_terminal_meta() {
    // Regression: an earlier implementation parsed the backend_key
    // prefix to find which terminals belong to a workspace. After
    // tmux session names switched to `pilot-{repo}-{kind}-{pid}-{n}`
    // (no longer prefixed with the workspace_key), that filter
    // matched zero terminals — Shift-X X silently kept the ghosts.
    // Now we use terminal_meta as the source of truth.
    use pilot_core::{SessionKey, WorkspaceKey};
    use pilot_ipc::{TerminalId, TerminalKind};

    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#1")).await;

    let workspace_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_task("o/r#1")));
    let session_key = SessionKey::from(workspace_key.as_str());
    // Insert a terminal pointing at this workspace, with a backend
    // key in the NEW format that doesn't start with the workspace
    // key.
    let backend_key_new_format = format!("pilot-o-r-1-claude-{}-1", std::process::id());
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
        .insert(TerminalId(42), pilot_core::SessionId::new());
    config
        .agent_states
        .lock()
        .await
        .insert(TerminalId(42), pilot_ipc::AgentState::Active);

    polling::delete_workspace(&config, &workspace_key).await;

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
    let pr_ws: pilot_core::Workspace =
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
    let ws: pilot_core::Workspace =
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
) -> (pilot_core::WorkspaceKey, pilot_core::SessionId) {
    use pilot_core::{SessionKind, WorkspaceKey, WorkspaceSession};
    polling::upsert(config, make_issue_task(issue_short_key)).await;
    let issue_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_issue_task(
        issue_short_key,
    )));
    let mut issue_ws: pilot_core::Workspace = {
        let record = config.store.get_workspace(&issue_key).unwrap().unwrap();
        serde_json::from_str(&record.workspace_json.unwrap()).unwrap()
    };
    let session_id = pilot_core::SessionId::new();
    issue_ws.add_session(WorkspaceSession {
        id: session_id,
        workspace_key: issue_key.clone(),
        name: "claude".into(),
        kind: SessionKind::Agent {
            agent_id: "claude".into(),
        },
        state: pilot_core::SessionRunState::Active,
        worktree_path: std::path::PathBuf::from("/tmp/pilot-test"),
        created_at: Utc::now(),
        last_output_at: None,
        layout: pilot_core::SessionLayout::default(),
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

#[tokio::test]
async fn live_issue_session_stalls_merge_and_emits_pending_event() {
    // Safety net: an issue workspace with live sessions must NOT be
    // silently absorbed by its closing PR. The daemon emits a
    // `WorkspaceMergePending` event and leaves both rows alone until
    // the user confirms via `Command::ConfirmMerge`.
    use pilot_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let mut bus = config.bus.subscribe();
    let (issue_key, _session_id) = seed_issue_with_session(&config, "o/r#71").await;

    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    // Both workspaces still in the store.
    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_some(),
        "issue workspace must NOT auto-merge while it has live sessions",
    );
    let pr_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_pr_closing(
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
async fn confirm_merge_accept_runs_the_merge() {
    // After the user says "yes" to the prompt, the merge runs the
    // same as the silent path: sessions move, terminal_meta rebadges,
    // issue row disappears.
    use pilot_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let (issue_key, session_id) = seed_issue_with_session(&config, "o/r#71").await;
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;
    let pr_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));

    polling::handle_confirm_merge(&config, issue_key.clone(), pr_key.clone(), true).await;

    assert!(
        config.store.get_workspace(&issue_key).unwrap().is_none(),
        "issue workspace should be removed after accepted merge",
    );
    let pr_ws: pilot_core::Workspace = {
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
async fn adopt_sessions_moves_sessions_between_workspaces() {
    use pilot_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let (source_key, session_id) = seed_issue_with_session(&config, "o/r#71").await;
    polling::upsert(&config, make_task("o/r#999")).await;
    let target_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_task("o/r#999")));

    polling::handle_adopt_sessions(&config, source_key.clone(), target_key.clone()).await;

    // Source still exists (we don't delete it on adopt), but has no
    // sessions left.
    let source_ws: pilot_core::Workspace = serde_json::from_str(
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
    let target_ws: pilot_core::Workspace = serde_json::from_str(
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
async fn adopt_sessions_into_self_is_a_noop() {
    let config = ServerConfig::in_memory();
    let (source_key, session_id) = seed_issue_with_session(&config, "o/r#71").await;
    polling::handle_adopt_sessions(&config, source_key.clone(), source_key.clone()).await;
    let ws: pilot_core::Workspace = serde_json::from_str(
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
#[tokio::test]
async fn adopt_sessions_rewrites_terminal_meta() {
    use pilot_core::{SessionKey, WorkspaceKey};
    use pilot_ipc::{TerminalId, TerminalKind};

    let config = ServerConfig::in_memory();
    let (source_key, _session_id) = seed_issue_with_session(&config, "o/r#71").await;
    polling::upsert(&config, make_task("o/r#999")).await;
    let target_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_task("o/r#999")));

    let source_session_key: SessionKey = (&source_key).into();
    config
        .terminal_meta
        .lock()
        .await
        .insert(TerminalId(7), (source_session_key, TerminalKind::Shell));

    polling::handle_adopt_sessions(&config, source_key.clone(), target_key.clone()).await;

    let target_session_key: SessionKey = (&target_key).into();
    let meta = config.terminal_meta.lock().await;
    let entry = meta.get(&TerminalId(7)).expect("terminal_meta entry kept");
    assert_eq!(
        entry.0, target_session_key,
        "terminal_meta must repoint at the adopt target",
    );
}
#[tokio::test]
async fn confirm_merge_reject_pins_against_re_prompting() {
    // User says "no": both workspaces survive, and a subsequent
    // poll of the same PR must NOT re-emit WorkspaceMergePending
    // — otherwise the modal would haunt them every 60 seconds.
    use pilot_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    let (issue_key, _) = seed_issue_with_session(&config, "o/r#71").await;
    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;
    let pr_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_pr_closing(
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
    use pilot_core::WorkspaceKey;

    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_task("o/r#141")).await; // a PR
    let mut pr_166 = make_task("o/r#166");
    pr_166.closes_issues = vec![TaskId {
        source: "github".into(),
        key: "o/r#141".into(), // ← pointing at the OTHER PR by mistake
    }];
    polling::upsert(&config, pr_166).await;

    let key_141 = WorkspaceKey::new(pilot_core::workspace_key_for(&make_task("o/r#141")));
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
    // against the issue's session_key. After the PR merges the
    // issue, the meta entry must be rebadged to the PR's key —
    // otherwise reconnecting TUI clients see a terminal pointing
    // to a workspace that no longer exists.
    use pilot_core::{SessionKey, WorkspaceKey};
    use pilot_ipc::{TerminalId, TerminalKind};

    let config = ServerConfig::in_memory();
    polling::upsert(&config, make_issue_task("o/r#71")).await;

    let issue_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_issue_task("o/r#71")));
    let issue_session_key: SessionKey = (&issue_key).into();
    config.terminal_meta.lock().await.insert(
        TerminalId(7),
        (issue_session_key.clone(), TerminalKind::Shell),
    );

    polling::upsert(&config, make_pr_closing("o/r#141", &["o/r#71"])).await;

    let pr_key = WorkspaceKey::new(pilot_core::workspace_key_for(&make_pr_closing(
        "o/r#141",
        &["o/r#71"],
    )));
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
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, pilot_core::ProviderError>> + Send + 'a>>
    {
        let name = self.name.clone();
        let secs = self.retry_after_secs;
        Box::pin(async move {
            Err(pilot_core::ProviderError::retryable_after(
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
    t.mergeable = pilot_core::Mergeable::Unknown;
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
