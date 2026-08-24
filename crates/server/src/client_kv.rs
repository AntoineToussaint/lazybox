//! Client-preference state the daemon owns on the UI's behalf (#548).
//!
//! The snippet MRU and the set of dismissed update targets used to be
//! written straight to `state.db` from the TUI's Model. That forked by
//! transport: a `--connect` client either opened a second handle on the
//! daemon's database or (over SSH `-L`) invented an unrelated local one,
//! so the MRU and dismissals silently diverged from the in-process TUI.
//!
//! Routing both through the daemon — the owner of state — heals the fork:
//! a confirmed [`lazybox_ipc::Command::DeliverSnippet`] updates the MRU,
//! dismissals arrive through [`lazybox_ipc::Command::SetUpdateDismissal`],
//! and clients read both current values from every
//! [`lazybox_ipc::Event::Snapshot`]. This mirrors the per-terminal
//! draft/history precedent (#373 / #523), which already behaves identically
//! over both transports.

use crate::ServerConfig;

/// kv key for the global most-recently-used snippet list. Unchanged from
/// the TUI's old direct-write key so an existing in-process user's MRU
/// carries over transparently.
const RECENT_SNIPPETS_KV_KEY: &str = "recent_snippets";
/// Newest-first cap on the retained MRU, matching the picker's "Recent"
/// group budget.
const RECENT_SNIPPETS_MAX: usize = 5;
/// kv key for the JSON list of dismissed update targets.
const DISMISSED_UPDATES_KV_KEY: &str = "update_dismissals";
/// kv key for the JSON list of snippet-override "keep mine"
/// acknowledgements (`snippet:<key>:<builtin-content-hash>`, #1312).
const SNIPPET_KEEPMINE_KV_KEY: &str = "snippet_keepmine";

/// Record `key` as the freshly-used snippet: de-duplicate, move it to the
/// front, cap the list, and persist. Best-effort — a write failure just
/// means the Recent group repopulates from use.
pub async fn record_recent_snippet(config: &ServerConfig, key: String) {
    let store = config.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut list = load_list(&*store, RECENT_SNIPPETS_KV_KEY);
        list.retain(|k| k != &key);
        list.insert(0, key);
        list.truncate(RECENT_SNIPPETS_MAX);
        let json = serde_json::to_string(&list).map_err(|e| e.to_string())?;
        store
            .set_kv(RECENT_SNIPPETS_KV_KEY, &json)
            .map_err(|e| e.to_string())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("record recent snippet failed: {e}"),
        Err(e) => tracing::warn!("record recent snippet task failed: {e}"),
    }
}

/// Mark `target` as a dismissed update: add it to the persisted set (a
/// no-op if already present) so the startup update modal stops surfacing
/// it. Best-effort — a write failure means the target may reappear next
/// launch, exactly the prior behavior.
pub async fn set_update_dismissal(config: &ServerConfig, target: String) {
    let store = config.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut list = load_list(&*store, DISMISSED_UPDATES_KV_KEY);
        if list.iter().any(|t| t == &target) {
            return Ok(());
        }
        list.push(target);
        let json = serde_json::to_string(&list).map_err(|e| e.to_string())?;
        store
            .set_kv(DISMISSED_UPDATES_KV_KEY, &json)
            .map_err(|e| e.to_string())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("record update dismissal failed: {e}"),
        Err(e) => tracing::warn!("record update dismissal task failed: {e}"),
    }
}

/// Acknowledge (`keep mine`) a snippet override against the current
/// built-in body: add `target` to the persisted set (a no-op if already
/// present), silencing the picker's "built-in changed" nudge until the
/// built-in — and therefore the target's embedded hash — changes again
/// (#1312). Returns the full updated set so the caller can re-broadcast
/// it to every attached client. Best-effort: on failure the previous set
/// is returned unchanged and the nudge simply persists.
pub async fn set_snippet_keepmine(config: &ServerConfig, target: String) -> Vec<String> {
    let store = config.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut list = load_list(&*store, SNIPPET_KEEPMINE_KV_KEY);
        if !list.iter().any(|t| t == &target) {
            list.push(target);
            let json = serde_json::to_string(&list).map_err(|e| e.to_string())?;
            store
                .set_kv(SNIPPET_KEEPMINE_KV_KEY, &json)
                .map_err(|e| e.to_string())?;
        }
        Ok::<Vec<String>, String>(list)
    })
    .await;
    match result {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => {
            tracing::warn!("record snippet keep-mine failed: {e}");
            snippet_keepmine(&*config.store)
        }
        Err(e) => {
            tracing::warn!("record snippet keep-mine task failed: {e}");
            snippet_keepmine(&*config.store)
        }
    }
}

/// The current snippet keep-mine set, for replay to a newly-subscribed
/// client (pushed as its own [`lazybox_ipc::Event::SnippetKeepMine`] right
/// after the snapshot, #1312).
pub fn snippet_keepmine(store: &dyn lazybox_store::Store) -> Vec<String> {
    load_list(store, SNIPPET_KEEPMINE_KV_KEY)
}

/// Load both client-preference lists for an outgoing snapshot. Runs on the
/// blocking pool alongside the workspace/project load.
pub fn snapshot(store: &dyn lazybox_store::Store) -> ClientKvSnapshot {
    ClientKvSnapshot {
        recent_snippets: load_list(store, RECENT_SNIPPETS_KV_KEY),
        dismissed_updates: load_list(store, DISMISSED_UPDATES_KV_KEY),
        snippet_keepmine: load_list(store, SNIPPET_KEEPMINE_KV_KEY),
    }
}

/// The client-preference lists loaded together for a fresh subscriber:
/// `recent_snippets` and `dismissed_updates` ride the [`lazybox_ipc::Event::Snapshot`]
/// itself; `snippet_keepmine` is sent right after it as its own
/// [`lazybox_ipc::Event::SnippetKeepMine`] (#1312).
#[derive(Debug, Default, Clone)]
pub struct ClientKvSnapshot {
    pub recent_snippets: Vec<String>,
    pub dismissed_updates: Vec<String>,
    pub snippet_keepmine: Vec<String>,
}

/// Read a JSON `Vec<String>` from `key`, degrading to empty on any miss,
/// read error, or parse error — every caller treats these as trivially
/// rebuildable caches, never destructible history.
fn load_list(store: &dyn lazybox_store::Store, key: &str) -> Vec<String> {
    match store.get_kv(key) {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|e| {
            tracing::warn!("parse client kv `{key}` failed: {e}");
            Vec::new()
        }),
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!("read client kv `{key}` failed: {e}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_store::MemoryStore;
    use std::sync::Arc;

    #[tokio::test]
    async fn recent_snippets_dedup_prepend_and_cap() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        for key in ["a", "b", "c", "d", "e", "f", "a"] {
            record_recent_snippet(&config, key.to_string()).await;
        }

        let snap = snapshot(&*store);
        // "a" re-used → front; cap keeps the 5 newest, "b" evicted.
        assert_eq!(snap.recent_snippets, vec!["a", "f", "e", "d", "c"]);
    }

    #[tokio::test]
    async fn update_dismissal_is_idempotent_set() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        set_update_dismissal(&config, "release:v1".into()).await;
        set_update_dismissal(&config, "release:v1".into()).await;
        set_update_dismissal(&config, "source:abc".into()).await;

        let snap = snapshot(&*store);
        assert_eq!(snap.dismissed_updates, vec!["release:v1", "source:abc"]);
    }

    #[tokio::test]
    async fn snippet_keepmine_is_idempotent_set_and_returns_full_list() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        let after_first = set_snippet_keepmine(&config, "snippet:rev:aaa".into()).await;
        assert_eq!(after_first, vec!["snippet:rev:aaa"]);
        // Re-acknowledging the same target is a no-op.
        set_snippet_keepmine(&config, "snippet:rev:aaa".into()).await;
        let after_second = set_snippet_keepmine(&config, "snippet:pr:bbb".into()).await;
        assert_eq!(after_second, vec!["snippet:rev:aaa", "snippet:pr:bbb"]);

        // A newly-subscribed client reads the same set back.
        assert_eq!(
            snippet_keepmine(&*store),
            vec!["snippet:rev:aaa", "snippet:pr:bbb"],
        );
    }
}
