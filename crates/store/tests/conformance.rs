//! Store trait conformance suite.
//!
//! One generic body exercising every `Store` method's contract —
//! kv roundtrip/overwrite/delete, workspace + project save/get/list/
//! delete, missing-key edges, and `created_at` recovery from the
//! serialized JSON — run against BOTH concrete backends:
//!
//! * `SqliteStore` (on-disk temp db — the production backend), and
//! * `MemoryStore` (the in-memory backend the daemon tests use).
//!
//! Any behavioral drift between the two (e.g. `MemoryStore` missing a
//! `list_projects` override, or fabricating `created_at`) fails here
//! instead of silently making daemon tests pass against semantics the
//! production store doesn't have.

use chrono::{DateTime, Utc};
use lazybox_core::{ProjectKey, WorkspaceKey};
use lazybox_store::{
    ErrorOccurrence, MemoryStore, ProjectRecord, SqliteStore, Store, StoreMutation, WorkspaceRecord,
};

/// Unique on-disk db path per test, cleaned up on drop. Hand-rolled
/// (std::env::temp_dir + counter) to match the sqlite unit tests —
/// the crate deliberately has no tempfile dev-dependency.
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "lazybox-store-conformance-{tag}-{}-{n}.db",
            std::process::id()
        )))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.0.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
    }
}

fn fixed_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2024-03-01T12:00:00Z")
        .expect("valid rfc3339")
        .with_timezone(&Utc)
}

fn workspace_record(key: &str, created_at: DateTime<Utc>, marker: &str) -> WorkspaceRecord {
    WorkspaceRecord {
        key: key.to_string(),
        created_at,
        workspace_json: Some(format!(
            r#"{{"key":"{key}","created_at":"{}","marker":"{marker}"}}"#,
            created_at.to_rfc3339()
        )),
    }
}

fn project_record(key: &str, created_at: DateTime<Utc>, marker: &str) -> ProjectRecord {
    ProjectRecord {
        key: key.to_string(),
        created_at,
        project_json: Some(format!(
            r#"{{"key":"{key}","created_at":"{}","marker":"{marker}"}}"#,
            created_at.to_rfc3339()
        )),
    }
}

// ── kv ──────────────────────────────────────────────────────────────

fn kv_contract(store: &dyn Store) {
    // Missing key reads as None, not an error.
    assert_eq!(store.get_kv("conf:absent").unwrap(), None);

    // Roundtrip.
    store.set_kv("conf:a", "v1").unwrap();
    assert_eq!(store.get_kv("conf:a").unwrap().as_deref(), Some("v1"));

    // Overwrite wins (upsert semantics, not insert-only).
    store.set_kv("conf:a", "v2").unwrap();
    assert_eq!(store.get_kv("conf:a").unwrap().as_deref(), Some("v2"));

    // Empty value is a stored value, distinct from absent.
    store.set_kv("conf:empty", "").unwrap();
    assert_eq!(store.get_kv("conf:empty").unwrap().as_deref(), Some(""));

    // Delete removes; deleting a missing key is idempotent Ok.
    store.delete_kv("conf:a").unwrap();
    assert_eq!(store.get_kv("conf:a").unwrap(), None);
    store.delete_kv("conf:a").unwrap();
    store.delete_kv("conf:never-existed").unwrap();

    store.set_kv("prefix:a", "one").unwrap();
    store.set_kv("prefix:b", "two").unwrap();
    store.set_kv("prefix_%", "literal").unwrap();
    store.set_kv("other:a", "noise").unwrap();
    assert_eq!(
        store.list_kv_prefix("prefix:").unwrap(),
        vec![
            ("prefix:a".to_string(), "one".to_string()),
            ("prefix:b".to_string(), "two".to_string()),
        ]
    );
    assert_eq!(
        store.list_kv_prefix("prefix_%").unwrap(),
        vec![("prefix_%".to_string(), "literal".to_string())],
        "prefix scans must not interpret SQL wildcard characters"
    );
}

// ── workspaces ──────────────────────────────────────────────────────

fn workspace_contract(store: &dyn Store) {
    let key_a = WorkspaceKey::new("conf-ws-a");
    let created = fixed_time();

    // Missing workspace reads as None.
    assert!(store.get_workspace(&key_a).unwrap().is_none());

    // Save/get roundtrip: key, json, and created_at recovered from the
    // JSON (not fabricated per read — must be stable across calls).
    store
        .save_workspace(&workspace_record("conf-ws-a", created, "one"))
        .unwrap();
    let got = store.get_workspace(&key_a).unwrap().expect("saved row");
    assert_eq!(got.key, "conf-ws-a");
    assert!(got.workspace_json.as_deref().unwrap().contains("\"one\""));
    assert_eq!(got.created_at, created);
    let again = store.get_workspace(&key_a).unwrap().expect("saved row");
    assert_eq!(again.created_at, created, "created_at must be stable");

    // Overwrite replaces the payload under the same key.
    store
        .save_workspace(&workspace_record("conf-ws-a", created, "two"))
        .unwrap();
    let got = store.get_workspace(&key_a).unwrap().expect("saved row");
    assert!(got.workspace_json.as_deref().unwrap().contains("\"two\""));
    assert!(!got.workspace_json.as_deref().unwrap().contains("\"one\""));

    // list_workspaces: every saved row, clean keys (no `workspace:`
    // prefix), created_at from the JSON; unrelated kv rows and project
    // rows must not leak in.
    store
        .save_workspace(&workspace_record("conf-ws-b", created, "b"))
        .unwrap();
    store.set_kv("conf:not-a-workspace", "x").unwrap();
    store
        .save_project(&project_record("conf-proj-noise", created, "p"))
        .unwrap();
    let mut keys: Vec<String> = store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["conf-ws-a".to_string(), "conf-ws-b".to_string()]);
    let listed = store.list_workspaces().unwrap();
    for r in &listed {
        assert!(!r.key.starts_with("workspace:"), "keys must be clean");
        assert_eq!(r.created_at, created, "created_at must come from JSON");
    }

    // Delete removes exactly one row; idempotent on re-delete and on
    // never-existed keys.
    store.delete_workspace(&key_a).unwrap();
    assert!(store.get_workspace(&key_a).unwrap().is_none());
    assert_eq!(store.list_workspaces().unwrap().len(), 1);
    store.delete_workspace(&key_a).unwrap();
    store
        .delete_workspace(&WorkspaceKey::new("conf-ws-never"))
        .unwrap();

    // A record with no JSON payload is REJECTED at the write boundary
    // rather than silently stored as `""` — which a later read surfaced
    // as a `Some("")` phantom that deserialization then choked on. Both
    // backends must reject, and neither may create a row.
    let none_json = WorkspaceRecord {
        key: "conf-ws-none".into(),
        created_at: created,
        workspace_json: None,
    };
    assert!(
        store.save_workspace(&none_json).is_err(),
        "saving a workspace with no JSON payload must be rejected"
    );
    assert!(
        store
            .get_workspace(&WorkspaceKey::new("conf-ws-none"))
            .unwrap()
            .is_none(),
        "a rejected save must not leave a phantom row"
    );
    // An empty-string payload is refused on the same grounds.
    let empty_json = WorkspaceRecord {
        key: "conf-ws-empty".into(),
        created_at: created,
        workspace_json: Some(String::new()),
    };
    assert!(
        store.save_workspace(&empty_json).is_err(),
        "saving a workspace with an empty JSON payload must be rejected"
    );

    // A legacy empty payload already on disk (written before empty writes
    // were refused — simulated here by a raw `set_kv` that bypasses the
    // write boundary) must read back as ABSENT, not as a `Some("")`
    // phantom, and must not appear in `list_workspaces`. The write
    // boundary makes empty unrepresentable going forward; the read path
    // heals rows that predate it.
    store.set_kv("workspace:conf-ws-legacy-empty", "").unwrap();
    assert!(
        store
            .get_workspace(&WorkspaceKey::new("conf-ws-legacy-empty"))
            .unwrap()
            .is_none(),
        "a legacy empty payload must read as absent, not a phantom"
    );
    assert!(
        store
            .list_workspaces()
            .unwrap()
            .iter()
            .all(|r| r.key != "conf-ws-legacy-empty"),
        "a legacy empty payload must not surface in list_workspaces"
    );
    store.delete_kv("workspace:conf-ws-legacy-empty").unwrap();

    // A JSON blob with no parseable `created_at` (corrupt/legacy) must
    // collapse to the OLDEST instant, never `Utc::now()` — a fabricated
    // now() sorted the broken row newest and let it dodge staleness.
    store
        .save_workspace(&WorkspaceRecord {
            key: "conf-ws-nocreated".into(),
            created_at: created,
            workspace_json: Some(r#"{"key":"conf-ws-nocreated"}"#.into()),
        })
        .unwrap();
    let got = store
        .get_workspace(&WorkspaceKey::new("conf-ws-nocreated"))
        .unwrap()
        .expect("row saved with a valid, non-empty blob");
    assert_eq!(
        got.created_at,
        DateTime::<Utc>::UNIX_EPOCH,
        "a missing created_at must read as the oldest instant, not now()"
    );
    store
        .delete_workspace(&WorkspaceKey::new("conf-ws-nocreated"))
        .unwrap();

    // A workspace key that itself begins `workspace:` round-trips through
    // list_workspaces with the store prefix stripped exactly ONCE — this
    // caught SqliteStore's `trim_start_matches` (strips repeatedly)
    // diverging from MemoryStore's single strip.
    store
        .save_workspace(&workspace_record("workspace:nested", created, "nested"))
        .unwrap();
    let listed = store.list_workspaces().unwrap();
    assert!(
        listed.iter().any(|r| r.key == "workspace:nested"),
        "a `workspace:`-prefixed key must strip only the store prefix, once"
    );
    store
        .delete_workspace(&WorkspaceKey::new("workspace:nested"))
        .unwrap();
}

// ── projects ────────────────────────────────────────────────────────

fn project_contract(store: &dyn Store) {
    let key_a = ProjectKey::local("conf-proj-a");
    let created = fixed_time();

    assert!(store.get_project(&key_a).unwrap().is_none());

    store
        .save_project(&project_record(key_a.as_str(), created, "one"))
        .unwrap();
    let got = store.get_project(&key_a).unwrap().expect("saved project");
    assert_eq!(got.key, key_a.as_str());
    assert!(got.project_json.as_deref().unwrap().contains("\"one\""));
    assert_eq!(got.created_at, created);

    // Overwrite.
    store
        .save_project(&project_record(key_a.as_str(), created, "two"))
        .unwrap();
    let got = store.get_project(&key_a).unwrap().expect("saved project");
    assert!(got.project_json.as_deref().unwrap().contains("\"two\""));

    // list_projects sees it (clean keys, created_at from JSON) and
    // does not leak workspace/kv rows.
    store
        .save_workspace(&workspace_record("conf-ws-noise", created, "w"))
        .unwrap();
    let listed = store.list_projects().unwrap();
    let row = listed
        .iter()
        .find(|r| r.key == key_a.as_str())
        .expect("saved project listed");
    assert_eq!(row.created_at, created);
    assert!(
        listed.iter().all(|r| !r.key.starts_with("project:")),
        "keys must be clean"
    );
    assert!(
        listed.iter().all(|r| r.key != "conf-ws-noise"),
        "workspace rows must not leak into list_projects"
    );

    // A `project:`-prefixed key strips only the store prefix, once —
    // same divergence guard as the workspace path.
    store
        .save_project(&project_record("project:nested", created, "nested"))
        .unwrap();
    assert!(
        store
            .list_projects()
            .unwrap()
            .iter()
            .any(|r| r.key == "project:nested"),
        "a `project:`-prefixed key must strip only the store prefix, once"
    );
    store
        .delete_project(&ProjectKey::new("project:nested"))
        .unwrap();

    // A legacy empty payload heals on read here too — absent from
    // `get_project` and `list_projects`, same as the workspace path.
    store.set_kv("project:conf-proj-legacy-empty", "").unwrap();
    assert!(
        store
            .get_project(&ProjectKey::new("conf-proj-legacy-empty"))
            .unwrap()
            .is_none(),
        "a legacy empty payload must read as absent, not a phantom"
    );
    assert!(
        store
            .list_projects()
            .unwrap()
            .iter()
            .all(|r| r.key != "conf-proj-legacy-empty"),
        "a legacy empty payload must not surface in list_projects"
    );
    store.delete_kv("project:conf-proj-legacy-empty").unwrap();

    // Delete + idempotence.
    store.delete_project(&key_a).unwrap();
    assert!(store.get_project(&key_a).unwrap().is_none());
    store.delete_project(&key_a).unwrap();
    store
        .delete_project(&ProjectKey::local("conf-proj-never"))
        .unwrap();
}

// ── atomic batches ──────────────────────────────────────────────────

fn batch_contract(store: &dyn Store) {
    let created = fixed_time();
    let workspace_key = WorkspaceKey::new("batch-ws");
    let project_key = ProjectKey::local("batch-project");

    store
        .apply_batch(&[
            StoreMutation::SetKv {
                key: "batch:marker".into(),
                value: "present".into(),
            },
            StoreMutation::SaveWorkspace(workspace_record(
                workspace_key.as_str(),
                created,
                "batch",
            )),
            StoreMutation::SaveProject(project_record(project_key.as_str(), created, "batch")),
        ])
        .unwrap();

    assert_eq!(
        store.get_kv("batch:marker").unwrap().as_deref(),
        Some("present")
    );
    assert!(store.get_workspace(&workspace_key).unwrap().is_some());
    assert!(store.get_project(&project_key).unwrap().is_some());

    store
        .apply_batch(&[
            StoreMutation::DeleteKv {
                key: "batch:marker".into(),
            },
            StoreMutation::DeleteWorkspace(workspace_key.clone()),
            StoreMutation::DeleteProject(project_key.clone()),
        ])
        .unwrap();

    assert!(store.get_kv("batch:marker").unwrap().is_none());
    assert!(store.get_workspace(&workspace_key).unwrap().is_none());
    assert!(store.get_project(&project_key).unwrap().is_none());
}

fn error_occurrence(dedupe_key: &str, message: &str, at: DateTime<Utc>) -> ErrorOccurrence {
    ErrorOccurrence {
        dedupe_key: dedupe_key.to_string(),
        source: "github".into(),
        severity: "retryable".into(),
        operation: Some("merge".into()),
        workspace_key: Some("github:o/r#1".into()),
        message: message.to_string(),
        raw: format!("raw:{message}"),
        at,
    }
}

/// Error Inbox contract: first record inserts at count 1; a repeat with
/// the same dedupe key increments the count, preserves `first_seen`,
/// advances `last_seen`, and replaces the stored sample; a distinct key
/// is an independent row; delete/clear behave.
fn error_contract(store: &dyn Store) {
    let t0 = fixed_time();
    let t1 = t0 + chrono::Duration::seconds(30);

    let first = store
        .record_error(&error_occurrence("throttle", "rate limited", t0))
        .expect("record error");
    assert_eq!(first.count, 1);
    assert_eq!(first.first_seen, t0);
    assert_eq!(first.last_seen, t0);
    assert_eq!(first.message, "rate limited");

    let second = store
        .record_error(&error_occurrence("throttle", "still rate limited", t1))
        .expect("record dup");
    assert_eq!(second.count, 2, "same dedupe key collapses + counts");
    assert_eq!(second.first_seen, t0, "first_seen is preserved");
    assert_eq!(second.last_seen, t1, "last_seen advances");
    assert_eq!(second.message, "still rate limited", "sample is the latest");
    assert_eq!(second.raw, "raw:still rate limited");

    store
        .record_error(&error_occurrence("auth", "bad token", t1))
        .expect("record distinct");

    let listed = store.list_errors().expect("list");
    assert_eq!(listed.len(), 2, "two distinct dedupe keys → two rows");
    // Most-recently-seen first.
    assert_eq!(listed[0].last_seen, t1);
    let throttle = listed
        .iter()
        .find(|r| r.dedupe_key == "throttle")
        .expect("throttle row present");
    assert_eq!(throttle.count, 2);

    store.delete_error("auth").expect("delete one");
    assert_eq!(store.list_errors().unwrap().len(), 1, "one row deleted");

    store.clear_errors().expect("clear");
    assert!(store.list_errors().unwrap().is_empty(), "inbox wiped");
}

/// The whole suite against one backend. Sections run in a fixed order
/// on one store instance so cross-namespace isolation (kv vs
/// workspace vs project prefixes) is exercised too.
fn run_conformance(store: &dyn Store) {
    kv_contract(store);
    workspace_contract(store);
    project_contract(store);
    batch_contract(store);
    error_contract(store);
}

#[test]
fn sqlite_store_conforms() {
    let db = TempDb::new("sqlite");
    let store = SqliteStore::open(&db.0).expect("open temp db");
    run_conformance(&store);
}

#[test]
fn sqlite_in_memory_store_conforms() {
    let store = SqliteStore::in_memory().expect("open in-memory db");
    run_conformance(&store);
}

#[test]
fn memory_store_conforms() {
    let store = MemoryStore::new();
    run_conformance(&store);
}
