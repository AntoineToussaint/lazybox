use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::Path;

use crate::traits::{
    ErrorOccurrence, ErrorRecord, StatBucket, StatEvent, Store, StoreError, StoreMutation,
};
use chrono::{DateTime, Utc};

/// Hard cap on distinct deduplicated error classes kept in the durable
/// inbox. Reached only under pathological cardinality (see the eviction
/// note in `record_error`); a generous ceiling that still bounds the
/// table so a long-lived daemon can't accrete unbounded rows.
const ERROR_INBOX_MAX_ROWS: i64 = 1000;

/// Distinct calendar days retained in the usage-stats rollup. Day/week
/// views never look back more than a couple weeks, so a year-plus of
/// history is generous; whole oldest days past this bound are evicted on
/// write (7-ish metrics/day makes the table trivially small regardless).
const STAT_RETENTION_DAYS: i64 = 400;

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
            );
            CREATE TABLE IF NOT EXISTS stat_daily (
                day    TEXT NOT NULL,
                metric TEXT NOT NULL,
                value  INTEGER NOT NULL,
                PRIMARY KEY (day, metric)
            );",
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }
}

/// Least string strictly greater than every string that starts with
/// `prefix` — the exclusive upper bound for an index range scan over a
/// BINARY-collated `TEXT` key. UTF-8 byte order matches code-point
/// order, so bumping the last scalar value yields a valid, correctly
/// ordered bound. Returns `None` when no finite bound exists (an empty
/// prefix, or one consisting entirely of `char::MAX`), so the caller
/// falls back to a lower-bound-only scan (`key >= prefix`), which is
/// exact in that case because nothing sorts after such a prefix without
/// also starting with it.
fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let mut next = last as u32 + 1;
        if next == 0xD800 {
            // Skip the UTF-16 surrogate gap, which holds no scalars.
            next = 0xE000;
        }
        if let Some(nc) = char::from_u32(next) {
            chars.push(nc);
            return Some(chars.into_iter().collect());
        }
        // `last` was char::MAX; drop it and bump the previous char.
    }
    None
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
        // Range scan on the PRIMARY KEY index. The former
        // `substr(key,1,length(?)) = ?` wrapped the indexed column in a
        // function, so it was non-sargable and forced a full table scan
        // of the entire kv table on every call — ruinous on the poller's
        // per-task archived-set read (#1496). `key >= prefix AND key <
        // upper` is answered directly from the index.
        let row = |row: &rusqlite::Row| -> rusqlite::Result<(String, String)> {
            Ok((row.get(0)?, row.get(1)?))
        };
        let mut stmt;
        let rows = match prefix_upper_bound(prefix) {
            Some(upper) => {
                stmt = conn
                    .prepare("SELECT key, value FROM kv WHERE key >= ?1 AND key < ?2 ORDER BY key")
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
                stmt.query_map(rusqlite::params![prefix, upper], row)
            }
            None => {
                stmt = conn
                    .prepare("SELECT key, value FROM kv WHERE key >= ?1 ORDER BY key")
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
                stmt.query_map(rusqlite::params![prefix], row)
            }
        }
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

    fn record_stats(&self, events: &[StatEvent]) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn
            .transaction()
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        for ev in events {
            // Same day + metric folds with `+` so a day's
            // tokens/sessions/merges accrue in one row. The daemon owns the
            // day boundary (local calendar day); the store just buckets.
            tx.execute(
                "INSERT INTO stat_daily (day, metric, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(day, metric) DO UPDATE SET value = value + excluded.value",
                rusqlite::params![ev.day, ev.metric, ev.value],
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        }
        // Evict whole oldest days past the retention window so the table
        // stays bounded on a long-lived daemon. Day strings sort by date,
        // so `ORDER BY day DESC LIMIT N` keeps the N most-recent days.
        tx.execute(
            "DELETE FROM stat_daily WHERE day NOT IN (
                 SELECT day FROM stat_daily
                 GROUP BY day
                 ORDER BY day DESC
                 LIMIT ?1
             )",
            [STAT_RETENTION_DAYS],
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        tx.commit().map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn list_stats_since(&self, since_day: &str) -> Result<Vec<StatBucket>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT day, metric, value FROM stat_daily
                 WHERE day >= ?1
                 ORDER BY day, metric",
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map([since_day], |row| {
                Ok(StatBucket {
                    day: row.get(0)?,
                    metric: row.get(1)?,
                    value: row.get(2)?,
                })
            })
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| StoreError::Backend(e.to_string()))
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

    fn stat(day: &str, metric: &str, value: i64) -> crate::traits::StatEvent {
        crate::traits::StatEvent {
            day: day.into(),
            metric: metric.into(),
            value,
        }
    }

    /// Same `(day, metric)` bucket folds additively, and distinct
    /// metrics on the same day stay separate rows.
    #[test]
    fn stats_fold_additively_per_day_and_metric() {
        let store = SqliteStore::in_memory().unwrap();
        store
            .record_stats(&[
                stat("2026-08-20", "sessions", 1),
                stat("2026-08-20", "sessions", 1),
                stat("2026-08-20", "input_tokens", 500),
            ])
            .unwrap();
        // A later day for the same metric is its own bucket.
        store
            .record_stats(&[stat("2026-08-21", "sessions", 3)])
            .unwrap();

        let buckets = store.list_stats_since("2026-08-01").unwrap();
        assert_eq!(
            buckets,
            vec![
                StatBucket {
                    day: "2026-08-20".into(),
                    metric: "input_tokens".into(),
                    value: 500,
                },
                StatBucket {
                    day: "2026-08-20".into(),
                    metric: "sessions".into(),
                    value: 2,
                },
                StatBucket {
                    day: "2026-08-21".into(),
                    metric: "sessions".into(),
                    value: 3,
                },
            ],
        );
    }

    /// `list_stats_since` is an inclusive lower bound on the day string,
    /// so a week window drops older days.
    #[test]
    fn stats_since_is_an_inclusive_day_lower_bound() {
        let store = SqliteStore::in_memory().unwrap();
        for day in ["2026-08-18", "2026-08-19", "2026-08-25"] {
            store.record_stats(&[stat(day, "merged", 1)]).unwrap();
        }
        let days: Vec<String> = store
            .list_stats_since("2026-08-19")
            .unwrap()
            .into_iter()
            .map(|b| b.day)
            .collect();
        assert_eq!(days, vec!["2026-08-19", "2026-08-25"], "18th is excluded");
    }

    /// Stats survive a reopen — the whole point of the accumulator is
    /// history that outlives the session that produced it.
    #[test]
    fn stats_survive_a_reopen() {
        let db = TempDb::new("stats-reopen");
        {
            let store = SqliteStore::open(&db.0).unwrap();
            store
                .record_stats(&[stat("2026-08-20", "merged", 1)])
                .unwrap();
        }
        let store = SqliteStore::open(&db.0).unwrap();
        let buckets = store.list_stats_since("2026-08-01").unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].value, 1);
    }

    /// The rollup evicts whole oldest days past the retention window; the
    /// most-recent day always survives.
    #[test]
    fn stats_evict_oldest_days_past_retention() {
        let store = SqliteStore::in_memory().unwrap();
        let base = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let day = |i: i64| {
            (base + chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string()
        };
        // One metric per day for retention + 5 consecutive days.
        let overflow = 5;
        for i in 0..(STAT_RETENTION_DAYS + overflow) {
            store.record_stats(&[stat(&day(i), "sessions", 1)]).unwrap();
        }
        let buckets = store.list_stats_since("0000-00-00").unwrap();
        let days: std::collections::HashSet<String> = buckets.into_iter().map(|b| b.day).collect();
        assert_eq!(days.len() as i64, STAT_RETENTION_DAYS, "table is capped");
        // The oldest day is gone, the newest survives.
        assert!(!days.contains(&day(0)), "oldest day evicted");
        assert!(
            days.contains(&day(STAT_RETENTION_DAYS + overflow - 1)),
            "newest day retained"
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

    /// The index range scan must return *exactly* the prefixed rows —
    /// no more, no less. The dangerous case is a key that is a strict
    /// superstring of the prefix minus its final byte: `archived_workspace:`
    /// vs the legacy blob `archived_workspaces_v1` (they share
    /// `archived_workspace`). The range upper bound must exclude the
    /// latter while including every real `archived_workspace:` row.
    #[test]
    fn list_kv_prefix_is_exact_at_adjacent_key_boundaries() {
        let store = SqliteStore::in_memory().unwrap();
        for (k, v) in [
            ("archived_workspace:a", "1"),
            ("archived_workspace:zeta", "1"),
            ("archived_workspace:~tilde", "1"), // '~' (0x7E) > ':' start byte
            ("archived_workspaces_v1", "[\"legacy\"]"), // adjacent, must NOT match
            ("archived", "x"),                  // shorter, must NOT match
            ("zzz", "x"),                       // after range, must NOT match
        ] {
            store.set_kv(k, v).unwrap();
        }

        let got: Vec<String> = store
            .list_kv_prefix("archived_workspace:")
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            got,
            vec![
                "archived_workspace:a".to_string(),
                "archived_workspace:zeta".to_string(),
                "archived_workspace:~tilde".to_string(),
            ],
            "range scan must match every prefixed key and exclude the adjacent legacy blob"
        );

        // Empty prefix returns everything (parity with the old substr scan).
        assert_eq!(store.list_kv_prefix("").unwrap().len(), 6);
    }

    /// The perf fix's whole point: the prefix scan must be answered from
    /// the PRIMARY KEY index, not a full table scan. `substr(key,…)=?`
    /// (the old form) plans as `SCAN kv`; the range form as `SEARCH kv`.
    #[test]
    fn list_kv_prefix_uses_the_index_not_a_full_scan() {
        let store = SqliteStore::in_memory().unwrap();
        let conn = store.conn();
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN \
                 SELECT key, value FROM kv WHERE key >= ?1 AND key < ?2 ORDER BY key",
            )
            .unwrap()
            .query_map(
                rusqlite::params!["archived_workspace:", "archived_workspace;"],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let detail = plan.join(" | ");
        assert!(
            detail.contains("SEARCH") && !detail.contains("SCAN"),
            "kv prefix scan must use the index, got plan: {detail}"
        );
    }

    #[test]
    fn prefix_upper_bound_boundaries() {
        assert_eq!(
            prefix_upper_bound("archived_workspace:").as_deref(),
            Some("archived_workspace;")
        );
        assert_eq!(prefix_upper_bound("ab").as_deref(), Some("ac"));
        // Trailing 0xFF-class char rolls over to bump the previous scalar.
        assert_eq!(prefix_upper_bound("a\u{10FFFF}").as_deref(), Some("b"));
        assert_eq!(prefix_upper_bound(""), None);
    }
}
