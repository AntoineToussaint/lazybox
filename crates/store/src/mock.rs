//! In-memory Store implementation for tests.

use crate::{Store, StoreError, StoreMutation};
use std::collections::HashMap;
use std::sync::Mutex;

/// A simple in-memory store for unit tests.
pub struct MemoryStore {
    kv: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            kv: Mutex::new(HashMap::new()),
        }
    }

    fn kv_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.kv.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Store for MemoryStore {
    fn apply_batch(&self, mutations: &[StoreMutation]) -> Result<(), StoreError> {
        // Resolve every mutation into an infallible kv op BEFORE taking
        // the lock. A rejected payload (empty/None) aborts here, so the
        // locked section below can never fail partway and leave a
        // half-applied batch — atomicity is structural, not dependent on
        // a redundant re-check. This is the all-or-nothing contract
        // `SqliteStore` gets from its transaction.
        enum Op {
            Insert(String, String),
            Remove(String),
        }
        let mut ops = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            let op = match mutation {
                StoreMutation::SetKv { key, value } => Op::Insert(key.clone(), value.clone()),
                StoreMutation::DeleteKv { key } => Op::Remove(key.clone()),
                StoreMutation::SaveWorkspace(record) => Op::Insert(
                    format!("workspace:{}", record.key),
                    record.require_json()?.to_string(),
                ),
                StoreMutation::DeleteWorkspace(key) => {
                    Op::Remove(format!("workspace:{}", key.as_str()))
                }
                StoreMutation::SaveProject(record) => Op::Insert(
                    format!("project:{}", record.key),
                    record.require_json()?.to_string(),
                ),
                StoreMutation::DeleteProject(key) => {
                    Op::Remove(format!("project:{}", key.as_str()))
                }
            };
            ops.push(op);
        }
        let mut kv = self.kv_lock();
        for op in ops {
            match op {
                Op::Insert(key, value) => {
                    kv.insert(key, value);
                }
                Op::Remove(key) => {
                    kv.remove(&key);
                }
            }
        }
        Ok(())
    }

    fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.kv_lock().get(key).cloned())
    }

    fn set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.kv_lock().insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete_kv(&self, key: &str) -> Result<(), StoreError> {
        self.kv_lock().remove(key);
        Ok(())
    }

    fn list_kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>, StoreError> {
        let mut rows: Vec<_> = self
            .kv_lock()
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(rows)
    }

    /// In-memory prefix scan over the kv table. Mirrors what
    /// `SqliteStore::list_workspaces` does so tests using
    /// `MemoryStore` see the same behavior — including recovering the
    /// record's real `created_at` from the JSON instead of fabricating
    /// `Utc::now()` per call.
    fn list_workspaces(&self) -> Result<Vec<crate::WorkspaceRecord>, StoreError> {
        let kv = self.kv_lock();
        let mut out = Vec::new();
        for (key, value) in kv.iter() {
            if let Some(stripped) = key.strip_prefix("workspace:") {
                // Skip a legacy empty payload rather than list a phantom
                // row — matches `SqliteStore::list_workspaces` and the
                // `get_workspace` read heal.
                if value.is_empty() {
                    continue;
                }
                out.push(crate::WorkspaceRecord {
                    key: stripped.to_string(),
                    created_at: crate::traits::created_at_or_oldest(value),
                    workspace_json: Some(value.clone()),
                });
            }
        }
        Ok(out)
    }

    /// Same shape as `list_workspaces` for the `project:*` prefix.
    /// Previously missing, which made `MemoryStore` silently diverge
    /// from `SqliteStore` (the trait default returns empty) — the
    /// store conformance suite in `tests/conformance.rs` pins the two
    /// backends to identical behavior.
    fn list_projects(&self) -> Result<Vec<crate::ProjectRecord>, StoreError> {
        let kv = self.kv_lock();
        let mut out = Vec::new();
        for (key, value) in kv.iter() {
            if let Some(stripped) = key.strip_prefix("project:") {
                // Skip a legacy empty payload — same heal as list_workspaces.
                if value.is_empty() {
                    continue;
                }
                out.push(crate::ProjectRecord {
                    key: stripped.to_string(),
                    created_at: crate::traits::created_at_or_oldest(value),
                    project_json: Some(value.clone()),
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceRecord;
    use chrono::Utc;

    #[test]
    fn workspace_round_trip_via_kv() {
        let store = MemoryStore::new();
        let record = WorkspaceRecord {
            key: "owner-repo-1".into(),
            created_at: Utc::now(),
            workspace_json: Some("{\"x\":1}".into()),
        };
        store.save_workspace(&record).unwrap();
        let listed = store.list_workspaces().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "owner-repo-1");
    }
}
