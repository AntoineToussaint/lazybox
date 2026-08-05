use chrono::{DateTime, Utc};
use lazybox_core::{ProjectKey, WorkspaceKey};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Backend(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported store operation: {0}")]
    Unsupported(&'static str),
}

/// Recover the record's real `created_at` from its serialized JSON
/// (both `Workspace` and `Project` carry the field). The kv table only
/// stores the JSON blob. Returns `None` when the blob is missing the
/// field or is unparseable (legacy / corrupt rows) — critically it does
/// NOT fabricate a timestamp. An earlier version fell back to
/// `Utc::now()`, which re-stamped a broken row as "created just now" on
/// EVERY read: the row sorted to the top of any recency sort and never
/// aged out of staleness checks. Surfacing the unknown as `None` lets
/// the caller fail it safe instead.
pub(crate) fn created_at_from_json(json: &str) -> Option<DateTime<Utc>> {
    #[derive(serde::Deserialize)]
    struct CreatedAt {
        created_at: DateTime<Utc>,
    }
    serde_json::from_str::<CreatedAt>(json)
        .ok()
        .map(|c| c.created_at)
}

/// The `created_at` a read path assigns a record whose JSON carries no
/// parseable timestamp. The Unix epoch sorts oldest and is maximally
/// stale, so a corrupt row fails safe (least-recent, never newest)
/// rather than masquerading as freshly created the way `Utc::now()`
/// once did.
pub(crate) fn created_at_or_oldest(json: &str) -> DateTime<Utc> {
    created_at_from_json(json).unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
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

impl WorkspaceRecord {
    /// The JSON payload a store write must persist, or an error.
    ///
    /// A `None` or empty blob is refused here so it can never reach the
    /// kv table. Previously every save mapped `None -> ""`; a later
    /// `get_workspace` then returned `Some("")`, a present-but-empty
    /// phantom record that `serde_json` chokes on. Rejecting at the
    /// write boundary makes that empty record unrepresentable.
    pub(crate) fn require_json(&self) -> Result<&str, StoreError> {
        match self.workspace_json.as_deref() {
            Some(json) if !json.is_empty() => Ok(json),
            _ => Err(StoreError::Backend(format!(
                "workspace {}: refusing to persist an empty JSON payload",
                self.key
            ))),
        }
    }
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

impl ProjectRecord {
    /// The JSON payload a store write must persist, or an error. Same
    /// empty/`None`-rejection contract as [`WorkspaceRecord::require_json`].
    pub(crate) fn require_json(&self) -> Result<&str, StoreError> {
        match self.project_json.as_deref() {
            Some(json) if !json.is_empty() => Ok(json),
            _ => Err(StoreError::Backend(format!(
                "project {}: refusing to persist an empty JSON payload",
                self.key
            ))),
        }
    }
}

/// A structured, deduplicated record in the durable Error Inbox.
///
/// Every error-severity event the daemon emits folds — by `dedupe_key`
/// — into one of these, so N identical failures (500 throttle errors,
/// a flapping merge) read as a single row carrying `count` and a
/// first/last-seen window instead of scrolling past N times. The store
/// is source-agnostic: `source` / `severity` / `operation` are opaque
/// strings the daemon assigns, never an enum the store interprets.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ErrorRecord {
    pub dedupe_key: String,
    pub source: String,
    pub severity: String,
    pub operation: Option<String>,
    pub workspace_key: Option<String>,
    /// Humanized, user-facing summary (the footer-notice text).
    pub message: String,
    /// Underlying diagnostic — raw provider text, GraphQL `code` /
    /// `typeName`, etc. — so a #822-class malformed-query bug stays
    /// diagnosable long after the notice faded.
    pub raw: String,
    pub count: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

/// One error occurrence to fold into the inbox via [`Store::record_error`].
///
/// The upsert is keyed by `dedupe_key`: a new key inserts with `count`
/// 1; an existing key bumps `count`, refreshes `last_seen`, and replaces
/// the stored sample (`message` / `raw` / context) with this latest
/// occurrence's. `first_seen` is never overwritten.
#[derive(Debug, Clone)]
pub struct ErrorOccurrence {
    pub dedupe_key: String,
    pub source: String,
    pub severity: String,
    pub operation: Option<String>,
    pub workspace_key: Option<String>,
    pub message: String,
    pub raw: String,
    pub at: DateTime<Utc>,
}

/// One durable mutation in an atomic [`Store::apply_batch`] call.
///
/// The daemon uses batches for domain operations that span more than one
/// record (issue → PR collapse, session adoption, and workspace + project
/// creation). A backend must apply every mutation or none of them; returning
/// `Ok(())` after a partial write violates the trait contract.
#[derive(Debug, Clone)]
pub enum StoreMutation {
    SetKv { key: String, value: String },
    DeleteKv { key: String },
    SaveWorkspace(WorkspaceRecord),
    DeleteWorkspace(WorkspaceKey),
    SaveProject(ProjectRecord),
    DeleteProject(ProjectKey),
}

/// Abstract storage trait. Implement for SQLite, Postgres, file, etc.
///
/// The kv methods (`get_kv` / `set_kv` / `delete_kv`) are for
/// daemon-side configuration: setup outcomes, ad-hoc preferences,
/// future workspace settings. Default impls behave as a never-stored
/// kv (None / Ok) so simple stores don't need to implement them.
pub trait Store: Send + Sync {
    /// Apply a group of mutations atomically.
    ///
    /// Backends that cannot provide an all-or-nothing transaction must return
    /// [`StoreError::Unsupported`], never emulate a batch with sequential
    /// writes. The default keeps partial/test backends honest until they add a
    /// real implementation.
    fn apply_batch(&self, _mutations: &[StoreMutation]) -> Result<(), StoreError> {
        Err(StoreError::Unsupported("atomic mutation batch"))
    }

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

    /// List every key/value pair whose key begins with `prefix`.
    ///
    /// Recovery code uses this to reconcile durable records with an
    /// external backend's live-session inventory. Backends must either
    /// provide an exact literal-prefix scan or report unsupported; an
    /// empty default would make "cannot enumerate" indistinguishable
    /// from "there are no records".
    fn list_kv_prefix(&self, _prefix: &str) -> Result<Vec<(String, String)>, StoreError> {
        Err(StoreError::Unsupported("kv prefix listing"))
    }

    // ── Error Inbox ─────────────────────────────────────────────────
    //
    // A durable, deduplicated sink for every error-severity event. The
    // defaults keep test stubs and read-only stores honest — a stub
    // that never persisted an error must not silently claim to have.

    /// Fold one occurrence into the inbox, returning the resulting
    /// deduplicated record (post-increment). Backends without a real
    /// implementation report unsupported rather than drop the error.
    fn record_error(&self, _occ: &ErrorOccurrence) -> Result<ErrorRecord, StoreError> {
        Err(StoreError::Unsupported("error inbox"))
    }

    /// Every deduplicated error record, most-recently-seen first.
    fn list_errors(&self) -> Result<Vec<ErrorRecord>, StoreError> {
        Ok(Vec::new())
    }

    /// Drop one record by its dedupe key. Idempotent.
    fn delete_error(&self, _dedupe_key: &str) -> Result<(), StoreError> {
        Ok(())
    }

    /// Wipe the whole inbox.
    fn clear_errors(&self) -> Result<(), StoreError> {
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
        // An empty payload can only be legacy data (writes now reject
        // empty via `require_json`). Treat it as absent rather than
        // surface a `Some("")` phantom that deserialization chokes on —
        // this heals pre-fix rows on read, not just on the write path.
        if json.is_empty() {
            return Ok(None);
        }
        Ok(Some(WorkspaceRecord {
            key: key.as_str().to_string(),
            created_at: created_at_or_oldest(&json),
            workspace_json: Some(json),
        }))
    }

    fn save_workspace(&self, record: &WorkspaceRecord) -> Result<(), StoreError> {
        let kv_key = format!("workspace:{}", record.key);
        let json = record.require_json()?;
        self.set_kv(&kv_key, json)
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
        // Same legacy empty-payload heal as `get_workspace`.
        if json.is_empty() {
            return Ok(None);
        }
        Ok(Some(ProjectRecord {
            key: key.as_str().to_string(),
            created_at: created_at_or_oldest(&json),
            project_json: Some(json),
        }))
    }

    fn save_project(&self, record: &ProjectRecord) -> Result<(), StoreError> {
        let kv_key = format!("project:{}", record.key);
        let json = record.require_json()?;
        self.set_kv(&kv_key, json)
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
