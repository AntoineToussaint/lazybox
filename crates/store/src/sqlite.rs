use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::Path;

use crate::traits::{Store, StoreError, StoreMutation};

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Lock the connection. `parking_lot::Mutex::lock` is infallible — no
    /// poisoning, no `PoisonError` handling, faster under contention than
    /// `std::sync::Mutex`.
    fn conn(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let conn = Connection::open(path).map_err(|e| StoreError::Backend(e.to_string()))?;
        // The DB holds session metadata and provider credential rows —
        // owner-only, whether we just created it or inherited a looser
        // file from an earlier build.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| StoreError::Backend(e.to_string()))?;
        }
        // WAL keeps readers (the TUI snapshot path) from blocking
        // behind the poll loop's writes; the busy timeout makes a
        // second process (e.g. `lazybox server status` racing the
        // daemon) wait briefly instead of failing with SQLITE_BUSY.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        // Standard WAL pairing: NORMAL only risks durability of the
        // very last transactions on an OS crash / power loss (the WAL
        // itself stays consistent — no corruption), and drops an
        // fsync per write. The default FULL was inherited, not chosen;
        // nothing in this DB (session metadata, read state) justifies
        // paying FULL's per-commit fsync.
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|e| StoreError::Backend(e.to_string()))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let conn = self.conn();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }
}

impl Store for SqliteStore {
    fn apply_batch(&self, mutations: &[StoreMutation]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        for mutation in mutations {
            match mutation {
                StoreMutation::SetKv { key, value } => {
                    tx.execute(
                        "INSERT INTO kv (key, value) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        (key, value),
                    )
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
                }
                StoreMutation::DeleteKv { key } => {
                    tx.execute("DELETE FROM kv WHERE key = ?1", [key])
                        .map_err(|e| StoreError::Backend(e.to_string()))?;
                }
                StoreMutation::SaveWorkspace(record) => {
                    let key = format!("workspace:{}", record.key);
                    // Refuse an empty/None payload — an early return here
                    // drops the transaction, rolling the whole batch back.
                    let value = record.require_json()?;
                    tx.execute(
                        "INSERT INTO kv (key, value) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        (&key, value),
                    )
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
                }
                StoreMutation::DeleteWorkspace(key) => {
                    let key = format!("workspace:{}", key.as_str());
                    tx.execute("DELETE FROM kv WHERE key = ?1", [&key])
                        .map_err(|e| StoreError::Backend(e.to_string()))?;
                }
                StoreMutation::SaveProject(record) => {
                    let key = format!("project:{}", record.key);
                    let value = record.require_json()?;
                    tx.execute(
                        "INSERT INTO kv (key, value) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        (&key, value),
                    )
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
                }
                StoreMutation::DeleteProject(key) => {
                    let key = format!("project:{}", key.as_str());
                    tx.execute("DELETE FROM kv WHERE key = ?1", [&key])
                        .map_err(|e| StoreError::Backend(e.to_string()))?;
                }
            }
        }
        tx.commit().map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT value FROM kv WHERE key = ?1")
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        stmt.query_row([key], |row| row.get::<_, String>(0))
            .optional()
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (&key, &value),
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    fn delete_kv(&self, key: &str) -> Result<(), StoreError> {
        let conn = self.conn();
        conn.execute("DELETE FROM kv WHERE key = ?1", [&key])
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    /// SQLite-native scan: prefix-match on the kv table. The default
    /// trait impl returns empty; we override so the snapshot path can
    /// replay every workspace at startup.
    fn list_workspaces(&self) -> Result<Vec<crate::WorkspaceRecord>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT key, value FROM kv WHERE key LIKE 'workspace:%'")
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            })
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value) = row.map_err(|e| StoreError::Backend(e.to_string()))?;
            // Strip the `workspace:` prefix ONCE so consumers see clean
            // keys. `strip_prefix` (not `trim_start_matches`, which
            // strips the prefix repeatedly) keeps this identical to
            // `MemoryStore` for a key that itself begins `workspace:`.
            let key = key.strip_prefix("workspace:").unwrap_or(&key).to_string();
            out.push(crate::WorkspaceRecord {
                key,
                created_at: crate::traits::created_at_or_oldest(&value),
                workspace_json: Some(value),
            });
        }
        Ok(out)
    }

    /// Same shape as `list_workspaces` but for the `project:*` prefix.
    /// The snapshot path uses this to fan out every known Project at
    /// startup so the sidebar can render headers even before polling
    /// catches up.
    fn list_projects(&self) -> Result<Vec<crate::ProjectRecord>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT key, value FROM kv WHERE key LIKE 'project:%'")
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            })
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value) = row.map_err(|e| StoreError::Backend(e.to_string()))?;
            let key = key.strip_prefix("project:").unwrap_or(&key).to_string();
            out.push(crate::ProjectRecord {
                key,
                created_at: crate::traits::created_at_or_oldest(&value),
                project_json: Some(value),
            });
        }
        Ok(out)
    }
}

trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Store;

    /// Unique on-disk db path per test, cleaned up on drop. Hand-rolled
    /// (std::env::temp_dir + counter) so the crate doesn't grow a
    /// tempfile dev-dependency for two tests.
    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "lazybox-store-test-{tag}-{}-{n}.db",
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

    /// `open` must put the connection in WAL mode with a busy timeout
    /// so a second process contending on the file waits instead of
    /// failing with SQLITE_BUSY.
    #[test]
    fn open_enables_wal_and_busy_timeout() {
        let db = TempDb::new("wal");
        let store = SqliteStore::open(&db.0).unwrap();
        let conn = store.conn();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let busy: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy, 5000);
        // WAL pairs with synchronous=NORMAL (1) — FULL's per-commit
        // fsync buys nothing for this DB's contents.
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sync, 1, "synchronous must be NORMAL alongside WAL");
    }

    /// The on-disk DB carries credential rows — `open` must leave it
    /// owner-only whether it created the file or inherited a looser
    /// one from an earlier build.
    #[cfg(unix)]
    #[test]
    fn open_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let db = TempDb::new("perms");
        drop(SqliteStore::open(&db.0).unwrap());
        let mode = std::fs::metadata(&db.0).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fresh state.db must be 0600");

        std::fs::set_permissions(&db.0, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(SqliteStore::open(&db.0).unwrap());
        let mode = std::fs::metadata(&db.0).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "re-open must tighten a loose state.db");
    }

    /// `list_workspaces` must surface the record's REAL `created_at`
    /// (carried inside the JSON), not fabricate `Utc::now()` per call
    /// — a consumer sorting by it would see every row's timestamp
    /// shift on every read.
    #[test]
    fn list_workspaces_returns_created_at_from_json() {
        let store = SqliteStore::in_memory().unwrap();
        let created_at = chrono::DateTime::parse_from_rfc3339("2024-03-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        store
            .save_workspace(&crate::WorkspaceRecord {
                key: "github-o-r-1".into(),
                created_at,
                workspace_json: Some(format!(
                    r#"{{"key":"github-o-r-1","created_at":"{}"}}"#,
                    created_at.to_rfc3339()
                )),
            })
            .unwrap();

        let rows = store.list_workspaces().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].created_at, created_at);
    }

    /// A backend error after an earlier mutation must roll the whole batch
    /// back. The trigger injects a deterministic failure on the second row;
    /// observing the first row afterwards would prove partial persistence.
    #[test]
    fn atomic_batch_rolls_back_every_prior_mutation_on_failure() {
        let store = SqliteStore::in_memory().unwrap();
        store
            .conn()
            .execute_batch(
                "CREATE TRIGGER reject_batch_boom
                 BEFORE INSERT ON kv
                 WHEN NEW.key = 'batch:boom'
                 BEGIN
                   SELECT RAISE(ABORT, 'injected batch failure');
                 END;",
            )
            .unwrap();

        let result = store.apply_batch(&[
            StoreMutation::SetKv {
                key: "batch:first".into(),
                value: "must-roll-back".into(),
            },
            StoreMutation::SetKv {
                key: "batch:boom".into(),
                value: "rejected".into(),
            },
        ]);

        assert!(result.is_err(), "the trigger must reject the batch");
        assert_eq!(
            store.get_kv("batch:first").unwrap(),
            None,
            "the first mutation must roll back with the rejected second one"
        );
    }
}
