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
        let mut kv = self.kv_lock();
        for mutation in mutations {
            match mutation {
                StoreMutation::SetKv { key, value } => {
                    kv.insert(key.clone(), value.clone());
                }
                StoreMutation::DeleteKv { key } => {
                    kv.remove(key);
                }
                StoreMutation::SaveWorkspace(record) => {
                    kv.insert(
                        format!("workspace:{}", record.key),
                        record.workspace_json.clone().unwrap_or_default(),
                    );
                }
                StoreMutation::DeleteWorkspace(key) => {
                    kv.remove(&format!("workspace:{}", key.as_str()));
                }
                StoreMutation::SaveProject(record) => {
                    kv.insert(
                        format!("project:{}", record.key),
                        record.project_json.clone().unwrap_or_default(),
                    );
                }
                StoreMutation::DeleteProject(key) => {
                    kv.remove(&format!("project:{}", key.as_str()));
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
                out.push(crate::WorkspaceRecord {
                    key: stripped.to_string(),
                    created_at: crate::traits::created_at_from_json(value),
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
                out.push(crate::ProjectRecord {
                    key: stripped.to_string(),
                    created_at: crate::traits::created_at_from_json(value),
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
