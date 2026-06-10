use chrono::{DateTime, Utc};
use lazybox_core::{ProjectKey, WorkspaceKey};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Backend(String),
    #[error("not found: {0}")]
    NotFound(String),
}

/// Recover the record's real `created_at` from its serialized JSON
/// (both `Workspace` and `Project` carry the field). The kv table
/// only stores the JSON blob, so without this every read fabricated
/// `Utc::now()` — anything sorting or aging on the record timestamp
/// saw a value that changed on every call. Falls back to now() when
/// the JSON is missing the field or unparseable (legacy rows).
pub(crate) fn created_at_from_json(json: &str) -> DateTime<Utc> {
    #[derive(serde::Deserialize)]
    struct CreatedAt {
        created_at: DateTime<Utc>,
    }
    serde_json::from_str::<CreatedAt>(json)
        .map(|c| c.created_at)
        .unwrap_or_else(|_| Utc::now())
}

/// A persisted workspace record — full workspace data (PR + linked
/// issues + worktree path + activity + read state) serialized as JSON,
/// keyed by `WorkspaceKey`.
#[derive(Debug, Clone)]
pub struct WorkspaceRecord {
    pub key: String,
    pub created_at: DateTime<Utc>,
    /// JSON of `lazybox_core::Workspace`.
    pub workspace_json: Option<String>,
}

/// A persisted project record — the parent container that holds
/// workspaces. JSON-serialized `lazybox_core::Project`, keyed by
/// `ProjectKey`. See the trait methods below for default kv-piggyback
/// behavior (`project:<key>` prefix) — the same shape `WorkspaceRecord`
/// uses, so simple kv stores don't need a per-method override.
#[derive(Debug, Clone)]
pub struct ProjectRecord {
    pub key: String,
    pub created_at: DateTime<Utc>,
    pub project_json: Option<String>,
}

/// Abstract storage trait. Implement for SQLite, Postgres, file, etc.
///
/// The kv methods (`get_kv` / `set_kv` / `delete_kv`) are for
/// daemon-side configuration: setup outcomes, ad-hoc preferences,
/// future workspace settings. Default impls behave as a never-stored
/// kv (None / Ok) so simple stores don't need to implement them.
pub trait Store: Send + Sync {
    /// Read a string value previously set with `set_kv`. Returns
    /// `Ok(None)` for both "never set" and the default impl, so
    /// callers should treat None as "use defaults".
    fn get_kv(&self, _key: &str) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    /// Write a string value. Concrete stores persist it; the default
    /// drops it on the floor (test stubs / read-only stores).
    fn set_kv(&self, _key: &str, _value: &str) -> Result<(), StoreError> {
        Ok(())
    }

    /// Remove a kv entry. Idempotent — missing key is not an error.
    fn delete_kv(&self, _key: &str) -> Result<(), StoreError> {
        Ok(())
    }

    // ── Workspaces ──────────────────────────────────────────────────
    //
    // Defaults piggy-back on the kv table (`workspace:<key>` → JSON).
    // Concrete stores can override for native indexes; simple kv-only
    // stores get workspace methods for free without overrides.

    fn get_workspace(&self, key: &WorkspaceKey) -> Result<Option<WorkspaceRecord>, StoreError> {
        let kv_key = format!("workspace:{}", key.as_str());
        let Some(json) = self.get_kv(&kv_key)? else {
            return Ok(None);
        };
        Ok(Some(WorkspaceRecord {
            key: key.as_str().to_string(),
            created_at: created_at_from_json(&json),
            workspace_json: Some(json),
        }))
    }

    fn save_workspace(&self, record: &WorkspaceRecord) -> Result<(), StoreError> {
        let kv_key = format!("workspace:{}", record.key);
        let json = record.workspace_json.clone().unwrap_or_default();
        self.set_kv(&kv_key, &json)
    }

    fn delete_workspace(&self, key: &WorkspaceKey) -> Result<(), StoreError> {
        self.delete_kv(&format!("workspace:{}", key.as_str()))
    }

    /// List every workspace the store knows about. Default impl
    /// returns empty — concrete stores should override and scan the
    /// kv table for `workspace:*` prefixes.
    fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, StoreError> {
        Ok(Vec::new())
    }

    // ── Projects ────────────────────────────────────────────────────
    //
    // Top-level containers that hold workspaces. Same kv-piggyback
    // shape as workspaces (`project:<key>` → JSON) so simple kv
    // stores get the four methods for free.

    fn get_project(&self, key: &ProjectKey) -> Result<Option<ProjectRecord>, StoreError> {
        let kv_key = format!("project:{}", key.as_str());
        let Some(json) = self.get_kv(&kv_key)? else {
            return Ok(None);
        };
        Ok(Some(ProjectRecord {
            key: key.as_str().to_string(),
            created_at: created_at_from_json(&json),
            project_json: Some(json),
        }))
    }

    fn save_project(&self, record: &ProjectRecord) -> Result<(), StoreError> {
        let kv_key = format!("project:{}", record.key);
        let json = record.project_json.clone().unwrap_or_default();
        self.set_kv(&kv_key, &json)
    }

    fn delete_project(&self, key: &ProjectKey) -> Result<(), StoreError> {
        self.delete_kv(&format!("project:{}", key.as_str()))
    }

    /// List every project the store knows about. Default impl returns
    /// empty — concrete stores should override and scan the kv table
    /// for `project:*` prefixes.
    fn list_projects(&self) -> Result<Vec<ProjectRecord>, StoreError> {
        Ok(Vec::new())
    }
}
