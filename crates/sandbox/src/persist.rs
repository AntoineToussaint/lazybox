//! Per-worktree persistence of a [`BoxHandle`].
//!
//! A box is stamped per worktree (#931), so its handle is keyed by the
//! worktree's identity and stored as JSON in the daemon kv table — the
//! same piggyback shape workspaces/projects use (`box:<worktree>` → JSON).
//! Keeping this out of the `Store` trait keeps the store layer ignorant of
//! sandbox types (store depends only on core), while the round-trip stays
//! a two-line call for the CLI.

use lazybox_store::Store;

use crate::{BoxHandle, provider::SandboxError, validate_handle_provider};

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

/// Load a handle only when it belongs to `provider`.
pub fn load_handle_for_provider(
    store: &dyn Store,
    worktree: &str,
    provider: &str,
) -> Result<Option<BoxHandle>, SandboxError> {
    let handle = load_handle(store, worktree)?;
    if let Some(handle) = &handle {
        validate_handle_provider(provider, handle)?;
    }
    Ok(handle)
}

/// Forget the box handle for `worktree`. Idempotent.
pub fn delete_handle(store: &dyn Store, worktree: &str) -> Result<(), SandboxError> {
    store.delete_kv(&kv_key(worktree))?;
    Ok(())
}

/// Every key a box handle is stamped under (the `<worktree>` part of
/// `box:<worktree>`). Used to surface **legacy** handles when the
/// shared-key lookup misses: pre-#965 builds keyed boxes per git
/// worktree, and those instances keep existing (and billing) even though
/// the new default key can't see them. A backend without prefix listing
/// yields an empty list — the caller degrades to no hint, never an error.
pub fn list_handle_keys(store: &dyn Store) -> Result<Vec<String>, SandboxError> {
    const PREFIX: &str = "box:";
    match store.list_kv_prefix(PREFIX) {
        Ok(pairs) => Ok(pairs
            .into_iter()
            .filter_map(|(k, _)| k.strip_prefix(PREFIX).map(str::to_string))
            .collect()),
        Err(lazybox_store::StoreError::Unsupported(_)) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
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

    #[test]
    fn list_handle_keys_names_every_stamped_key() {
        let store = MemoryStore::new();
        assert!(list_handle_keys(&store).unwrap().is_empty());

        save_handle(&store, "/repos/foo/wt", &handle()).unwrap();
        save_handle(&store, "sandbox", &handle()).unwrap();
        let mut keys = list_handle_keys(&store).unwrap();
        keys.sort();
        assert_eq!(
            keys,
            vec!["/repos/foo/wt".to_string(), "sandbox".to_string()]
        );
    }

    #[test]
    fn provider_scoped_load_rejects_a_handle_from_another_backend() {
        let store = MemoryStore::new();
        save_handle(&store, "sandbox", &handle()).unwrap();

        assert!(load_handle_for_provider(&store, "sandbox", "gcp").is_ok());
        let error = load_handle_for_provider(&store, "sandbox", "e2b").unwrap_err();
        assert!(error.to_string().contains("provider \"gcp\""));
        assert!(error.to_string().contains("provider \"e2b\""));
    }
}
