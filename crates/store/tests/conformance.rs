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
use lazybox_store::{MemoryStore, ProjectRecord, SqliteStore, Store, WorkspaceRecord};

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

    // A record saved with `workspace_json: None` still produces a row
    // on read (stored as an empty payload) — both backends must agree.
    let none_json = WorkspaceRecord {
        key: "conf-ws-none".into(),
        created_at: created,
        workspace_json: None,
    };
    store.save_workspace(&none_json).unwrap();
    let got = store
        .get_workspace(&WorkspaceKey::new("conf-ws-none"))
        .unwrap()
        .expect("row exists even when saved without json");
    assert_eq!(got.workspace_json.as_deref(), Some(""));
    store
        .delete_workspace(&WorkspaceKey::new("conf-ws-none"))
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

    // Delete + idempotence.
    store.delete_project(&key_a).unwrap();
    assert!(store.get_project(&key_a).unwrap().is_none());
    store.delete_project(&key_a).unwrap();
    store
        .delete_project(&ProjectKey::local("conf-proj-never"))
        .unwrap();
}

/// The whole suite against one backend. Sections run in a fixed order
/// on one store instance so cross-namespace isolation (kv vs
/// workspace vs project prefixes) is exercised too.
fn run_conformance(store: &dyn Store) {
    kv_contract(store);
    workspace_contract(store);
    project_contract(store);
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
