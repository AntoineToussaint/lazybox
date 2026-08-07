//! Per-worktree persistence of a [`BoxHandle`].
//!
//! A box is stamped per worktree (#931), so its handle is keyed by the
//! worktree's identity and stored as JSON in the daemon kv table — the
//! same piggyback shape workspaces/projects use (`box:<worktree>` → JSON).
//! Keeping this out of the `Store` trait keeps the store layer ignorant of
//! sandbox types (store depends only on core), while the round-trip stays
//! a two-line call for the CLI.

use lazybox_store::Store;

use crate::{BoxHandle, provider::SandboxError};

fn kv_key(worktree: &str) -> String {
    format!("box:{worktree}")
}

/// Persist `handle` under `worktree`'s key, overwriting any prior handle.
pub fn save_handle(
    store: &dyn Store,
    worktree: &str,
    handle: &BoxHandle,
) -> Result<(), SandboxError> {
    let json = serde_json::to_string(handle).map_err(|e| SandboxError::Serialize(e.to_string()))?;
    store.set_kv(&kv_key(worktree), &json)?;
    Ok(())
}

/// Load the box handle for `worktree`, or `None` if none was stamped.
pub fn load_handle(store: &dyn Store, worktree: &str) -> Result<Option<BoxHandle>, SandboxError> {
    let Some(json) = store.get_kv(&kv_key(worktree))? else {
        return Ok(None);
    };
    let handle = serde_json::from_str(&json).map_err(|e| SandboxError::Serialize(e.to_string()))?;
    Ok(Some(handle))
}

/// Forget the box handle for `worktree`. Idempotent.
pub fn delete_handle(store: &dyn Store, worktree: &str) -> Result<(), SandboxError> {
    store.delete_kv(&kv_key(worktree))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PowerState;
    use lazybox_store::MemoryStore;

    fn handle() -> BoxHandle {
        BoxHandle {
            provider: "gcp".into(),
            id: "lazybox-sbx-abc".into(),
            region: "us-central1".into(),
            zone: "us-central1-a".into(),
            project: "proj".into(),
            power_state: PowerState::Running,
            last_active: None,
        }
    }

    #[test]
    fn round_trips_per_worktree_and_deletes() {
        let store = MemoryStore::new();
        assert_eq!(load_handle(&store, "wt-1").unwrap(), None);

        let h = handle();
        save_handle(&store, "wt-1", &h).unwrap();
        assert_eq!(load_handle(&store, "wt-1").unwrap(), Some(h.clone()));

        // Keyed per worktree — a different worktree sees nothing.
        assert_eq!(load_handle(&store, "wt-2").unwrap(), None);

        delete_handle(&store, "wt-1").unwrap();
        assert_eq!(load_handle(&store, "wt-1").unwrap(), None);
        // Idempotent delete.
        delete_handle(&store, "wt-1").unwrap();
    }
}
