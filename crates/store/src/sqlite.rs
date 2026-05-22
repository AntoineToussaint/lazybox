use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::Path;

use crate::traits::{Store, StoreError};

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
        let conn = Connection::open(path).map_err(|e| StoreError::Backend(e.to_string()))?;
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
            // Strip the `workspace:` prefix so consumers see clean keys.
            let key = key.trim_start_matches("workspace:").to_string();
            out.push(crate::WorkspaceRecord {
                key,
                created_at: chrono::Utc::now(),
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
            let key = key.trim_start_matches("project:").to_string();
            out.push(crate::ProjectRecord {
                key,
                created_at: chrono::Utc::now(),
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
