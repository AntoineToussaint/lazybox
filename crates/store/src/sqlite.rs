use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::Path;

use crate::traits::{ErrorOccurrence, ErrorRecord, Store, StoreError, StoreMutation};
use chrono::{DateTime, Utc};

/// Hard cap on distinct deduplicated error classes kept in the durable
/// inbox. Reached only under pathological cardinality (see the eviction
/// note in `record_error`); a generous ceiling that still bounds the
/// table so a long-lived daemon can't accrete unbounded rows.
const ERROR_INBOX_MAX_ROWS: i64 = 1000;

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
            );
            CREATE TABLE IF NOT EXISTS errors (
                dedupe_key    TEXT PRIMARY KEY,
                source        TEXT NOT NULL,
                severity      TEXT NOT NULL,
                operation     TEXT,
                workspace_key TEXT,
                message       TEXT NOT NULL,
                raw           TEXT NOT NULL,
                count         INTEGER NOT NULL,
                first_seen    TEXT NOT NULL,
                last_seen     TEXT NOT NULL
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

    fn list_kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT key, value
                 FROM kv
                 WHERE substr(key, 1, length(?1)) = ?1
                 ORDER BY key",
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map([prefix], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn record_error(&self, occ: &ErrorOccurrence) -> Result<ErrorRecord, StoreError> {
        let conn = self.conn();
        let at = occ.at.to_rfc3339();
        // Atomic upsert: a new dedupe_key inserts at count 1; an
        // existing one bumps count and refreshes the sample. `first_seen`
        // is deliberately left untouched on conflict.
        conn.execute(
            "INSERT INTO errors
                (dedupe_key, source, severity, operation, workspace_key,
                 message, raw, count, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)
             ON CONFLICT(dedupe_key) DO UPDATE SET
                 count         = count + 1,
                 last_seen     = excluded.last_seen,
                 source        = excluded.source,
                 severity      = excluded.severity,
                 operation     = excluded.operation,
                 workspace_key = excluded.workspace_key,
                 message       = excluded.message,
                 raw           = excluded.raw",
            rusqlite::params![
                occ.dedupe_key,
                occ.source,
                occ.severity,
                occ.operation,
                occ.workspace_key,
                occ.message,
                occ.raw,
                at,
            ],
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        // Bound the table: `normalize` only collapses digit / whitespace
        // runs, so high-cardinality diagnostics (hex request-ids, SHAs,
        // paths) mint a fresh row each and the store would otherwise grow
        // without limit. Evict the least-recently-seen classes past the
        // cap. The just-written row carries the newest `last_seen`, so it
        // is always kept; the tie-break on `dedupe_key` makes eviction
        // deterministic when timestamps collide.
        conn.execute(
            "DELETE FROM errors WHERE dedupe_key NOT IN (
                 SELECT dedupe_key FROM errors
                 ORDER BY last_seen DESC, dedupe_key DESC
                 LIMIT ?1
             )",
            [ERROR_INBOX_MAX_ROWS],
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        conn.query_row(
            "SELECT dedupe_key, source, severity, operation, workspace_key,
                    message, raw, count, first_seen, last_seen
             FROM errors WHERE dedupe_key = ?1",
            [&occ.dedupe_key],
            error_record_from_row,
        )
        .map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn list_errors(&self) -> Result<Vec<ErrorRecord>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT dedupe_key, source, severity, operation, workspace_key,
                        message, raw, count, first_seen, last_seen
                 FROM errors
                 ORDER BY last_seen DESC",
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map([], error_record_from_row)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn delete_error(&self, dedupe_key: &str) -> Result<(), StoreError> {
        let conn = self.conn();
        conn.execute("DELETE FROM errors WHERE dedupe_key = ?1", [dedupe_key])
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    fn clear_errors(&self) -> Result<(), StoreError> {
        let conn = self.conn();
        conn.execute("DELETE FROM errors", [])
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
            // Skip a legacy empty payload rather than list a phantom row
            // (writes now reject empty via `require_json`; this heals
            // pre-fix rows on read). Matches `get_workspace` and
            // `MemoryStore::list_workspaces`.
            if value.is_empty() {
                continue;
            }
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
            // Skip a legacy empty payload — same heal as list_workspaces.
            if value.is_empty() {
                continue;
            }
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

/// Parse an RFC3339 timestamp column, failing the row (never
/// fabricating a `now`) if it is unparseable — the same fail-safe
/// stance `created_at_from_json` takes for workspace rows.
fn parse_ts(raw: &str) -> Result<DateTime<Utc>, rusqlite::Error> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

/// Map an `errors`-table row (column order fixed by the `SELECT` queries above)
/// into an [`ErrorRecord`].
fn error_record_from_row(row: &rusqlite::Row<'_>) -> Result<ErrorRecord, rusqlite::Error> {
    let first_seen: String = row.get(8)?;
    let last_seen: String = row.get(9)?;
    Ok(ErrorRecord {
        dedupe_key: row.get(0)?,
        source: row.get(1)?,
        severity: row.get(2)?,
        operation: row.get(3)?,
        workspace_key: row.get(4)?,
        message: row.get(5)?,
        raw: row.get(6)?,
        count: row.get::<_, i64>(7)? as u64,
        first_seen: parse_ts(&first_seen)?,
        last_seen: parse_ts(&last_seen)?,
    })
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

    /// Error Inbox records are durable: a record written before close is
    /// still there — deduped count intact — after re-opening the file.
    /// This is the "survives restart" acceptance for #831.
    #[test]
    fn error_records_survive_a_reopen() {
        use crate::traits::ErrorOccurrence;
        let db = TempDb::new("errors-reopen");
        let at = chrono::DateTime::parse_from_rfc3339("2026-08-05T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let occ = ErrorOccurrence {
            dedupe_key: "github|merge|rate limited".into(),
            source: "github".into(),
            severity: "retryable".into(),
            operation: Some("merge".into()),
            workspace_key: Some("github:o/r#1".into()),
            message: "rate limited".into(),
            raw: "code=RATE_LIMITED".into(),
            at,
        };
        {
            let store = SqliteStore::open(&db.0).unwrap();
            store.record_error(&occ).unwrap();
            store.record_error(&occ).unwrap();
        }
        // Re-open the same file — a fresh process would do the same.
        let store = SqliteStore::open(&db.0).unwrap();
        let rows = store.list_errors().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 2, "the deduped count persisted");
        assert_eq!(rows[0].first_seen, at);
        assert_eq!(rows[0].raw, "code=RATE_LIMITED");
    }

    /// The inbox is bounded: once distinct classes exceed
    /// `ERROR_INBOX_MAX_ROWS`, recording a new one evicts the
    /// least-recently-seen class rather than growing without limit. The
    /// just-recorded class always survives; the oldest is gone.
    #[test]
    fn error_inbox_evicts_least_recently_seen_past_the_cap() {
        use crate::traits::ErrorOccurrence;
        let store = SqliteStore::in_memory().unwrap();
        let base = chrono::DateTime::parse_from_rfc3339("2026-08-05T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // Record cap + 5 distinct classes, each seen one second later than
        // the last, so `dedupe_key i` is the oldest and the final ones the
        // newest.
        let overflow = 5;
        for i in 0..(ERROR_INBOX_MAX_ROWS + overflow) {
            store
                .record_error(&ErrorOccurrence {
                    dedupe_key: format!("github|merge|class {i}"),
                    source: "github".into(),
                    severity: "permanent".into(),
                    operation: Some("merge".into()),
                    workspace_key: None,
                    message: format!("failure {i}"),
                    raw: format!("raw {i}"),
                    at: base + chrono::Duration::seconds(i),
                })
                .unwrap();
        }
        let rows = store.list_errors().unwrap();
        assert_eq!(
            rows.len() as i64,
            ERROR_INBOX_MAX_ROWS,
            "the table is capped, not unbounded"
        );
        let keys: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.dedupe_key.as_str()).collect();
        // The five oldest classes were evicted…
        for i in 0..overflow {
            assert!(
                !keys.contains(format!("github|merge|class {i}").as_str()),
                "class {i} (oldest) should have been evicted"
            );
        }
        // …and the newest class is still present.
        let newest = ERROR_INBOX_MAX_ROWS + overflow - 1;
        assert!(
            keys.contains(format!("github|merge|class {newest}").as_str()),
            "the most-recently-seen class must survive"
        );
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
