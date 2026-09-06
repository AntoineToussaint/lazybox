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
/// kv key prefix for per-session accumulated metered cost, one row per
/// session key (`meter-cost:<session_key>` → micro-USD as a decimal
/// string). Lets the per-workspace `$ METER · $cost` figure survive a
/// restart (#1389).
const SESSION_COST_KV_PREFIX: &str = "meter-cost:";

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

/// Add `delta_micros` to the persisted running cost for `session_key`, so
/// the per-workspace meter figure accumulates across a restart (#1389).
/// Read-modify-write on the blocking pool. The metering subscriber is no
/// longer the only writer of this keyspace — the issue→PR fold
/// ([`move_session_cost`]) read-modify-writes the same keys from a different
/// task — so the RMW runs under `config.session_cost_lock` to keep the two
/// from interleaving and losing an update. Best-effort — a lost write is a
/// bounded under-count, never destructive.
pub async fn add_session_cost(config: &ServerConfig, session_key: String, delta_micros: u64) {
    if delta_micros == 0 {
        return;
    }
    let store = config.store.clone();
    let cost_lock = config.session_cost_lock.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = cost_lock.lock();
        let kv_key = format!("{SESSION_COST_KV_PREFIX}{session_key}");
        let current = store
            .get_kv(&kv_key)
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let total = current.saturating_add(delta_micros);
        store.set_kv(&kv_key, &total.to_string())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("record session cost failed: {e}"),
        Err(e) => tracing::warn!("record session cost task failed: {e}"),
    }
}

/// Fold the persisted cost of `from` into `to` and drop the `from` row —
/// the aggregation step when one line of work collapses into another (an
/// issue workspace absorbed by the PR it produced). Without it the issue's
/// spend stays orphaned under a key no workspace renders and the PR's
/// "price" starts from zero. Synchronous read-modify-write, for callers
/// already on the blocking pool. Returns whether anything moved — `false`
/// for a `from` with no row, a self-move, or a failed write — so a caller
/// knows whether the per-session costs need re-shipping.
///
/// Production callers MUST hold `ServerConfig::session_cost_lock` for the
/// duration of this call: it read-modify-writes the same `meter-cost:` keys
/// as [`add_session_cost`], and without the shared lock the two interleave
/// and lose an update. (Single-threaded tests call it directly.)
pub fn move_session_cost(store: &dyn lazybox_store::Store, from: &str, to: &str) -> bool {
    if from == to {
        return false;
    }
    let read = |key: &str| -> u64 {
        store
            .get_kv(&format!("{SESSION_COST_KV_PREFIX}{key}"))
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let moving = read(from);
    if moving == 0 {
        return false;
    }
    let total = read(to).saturating_add(moving);
    if let Err(e) = store.set_kv(&format!("{SESSION_COST_KV_PREFIX}{to}"), &total.to_string()) {
        tracing::warn!("move session cost `{from}` → `{to}` failed: {e}");
        return false;
    }
    clear_session_cost(store, from);
    true
}

/// Drop the persisted cost row for `session_key`. Called on a genuine
/// workspace teardown — a delete/retire/rescope, the non-archiving
/// [`WorkspaceRemovalReason`](crate::workspace)s (via
/// `reclaim_workspace_worktrees`) — and when a row's cost has been folded
/// into another key ([`move_session_cost`]). Deliberately NOT called on an
/// archive: an archived merged PR's total is the durable "what did this PR
/// cost" record, so it stays. Best-effort: a failed delete just leaves a
/// dead row that renders under no workspace, never destructible history.
pub fn clear_session_cost(store: &dyn lazybox_store::Store, session_key: &str) {
    let kv_key = format!("{SESSION_COST_KV_PREFIX}{session_key}");
    if let Err(e) = store.delete_kv(&kv_key) {
        tracing::warn!("clear session cost `{session_key}` failed: {e}");
    }
}

/// Every persisted per-session cost as `(session_key, cost_micros)`, for
/// hydrating a client's live cost tracker on connect. Rows that don't parse
/// as a `u64` are skipped — a cost total is a rebuildable cache, never
/// destructible history.
pub fn session_costs(store: &dyn lazybox_store::Store) -> Vec<(String, u64)> {
    match store.list_kv_prefix(SESSION_COST_KV_PREFIX) {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|(key, value)| {
                let session_key = key.strip_prefix(SESSION_COST_KV_PREFIX)?.to_string();
                let micros = value.parse::<u64>().ok()?;
                Some((session_key, micros))
            })
            .collect(),
        Err(e) => {
            tracing::warn!("read session costs failed: {e}");
            Vec::new()
        }
    }
}

/// Load both client-preference lists for an outgoing snapshot. Runs on the
/// blocking pool alongside the workspace/project load.
pub fn snapshot(store: &dyn lazybox_store::Store) -> ClientKvSnapshot {
    ClientKvSnapshot {
        recent_snippets: load_list(store, RECENT_SNIPPETS_KV_KEY),
        dismissed_updates: load_list(store, DISMISSED_UPDATES_KV_KEY),
        snippet_keepmine: load_list(store, SNIPPET_KEEPMINE_KV_KEY),
        session_costs: session_costs(store),
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
    /// Accumulated metered cost per session key (micro-USD). Loaded with the
    /// snapshot bundle and replayed on connect as `Event::SessionCosts` so the
    /// per-workspace `$ METER` figure survives a restart.
    pub session_costs: Vec<(String, u64)>,
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
    async fn session_cost_accumulates_per_key_and_survives_reload() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        add_session_cost(&config, "github:o/r#1".into(), 1_500_000).await;
        add_session_cost(&config, "github:o/r#1".into(), 500_000).await;
        add_session_cost(&config, "github:o/r#2".into(), 250_000).await;
        // A zero delta is a no-op (never mints an empty row).
        add_session_cost(&config, "github:o/r#3".into(), 0).await;

        let mut costs = session_costs(&*store);
        costs.sort();
        assert_eq!(
            costs,
            vec![
                ("github:o/r#1".to_string(), 2_000_000),
                ("github:o/r#2".to_string(), 250_000),
            ],
        );
        // The snapshot bundle carries the same figures for hydration.
        let snap = snapshot(&*store);
        assert_eq!(snap.session_costs.len(), 2);
    }

    /// The issue→PR fold: the issue's total lands on the PR's row (added to
    /// anything the PR already accrued) and the issue row is gone, so the
    /// PR's price spans the whole line of work. A key with no row, or a
    /// self-move, is a no-op.
    #[tokio::test]
    async fn move_session_cost_folds_issue_total_into_pr() {
        let store = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());

        add_session_cost(&config, "github:o/r#7".into(), 1_200_000).await; // issue
        add_session_cost(&config, "github:o/r#42".into(), 300_000).await; // PR

        assert!(move_session_cost(&*store, "github:o/r#7", "github:o/r#42"));

        let mut costs = session_costs(&*store);
        costs.sort();
        assert_eq!(costs, vec![("github:o/r#42".to_string(), 1_500_000)]);

        // No source row → nothing changes; self-move → nothing changes.
        assert!(!move_session_cost(
            &*store,
            "github:o/r#999",
            "github:o/r#42"
        ));
        assert!(!move_session_cost(
            &*store,
            "github:o/r#42",
            "github:o/r#42"
        ));
        assert_eq!(
            session_costs(&*store),
            vec![("github:o/r#42".to_string(), 1_500_000)]
        );

        // A PR with no row yet still receives the fold (the row-count stays
        // flat here — one leaves, one appears — which is exactly the case a
        // count-based "did anything move" check would miss).
        add_session_cost(&config, "github:o/r#8".into(), 700_000).await;
        assert!(move_session_cost(&*store, "github:o/r#8", "github:o/r#43"));
        let mut costs = session_costs(&*store);
        costs.sort();
        assert_eq!(
            costs,
            vec![
                ("github:o/r#42".to_string(), 1_500_000),
                ("github:o/r#43".to_string(), 700_000),
            ]
        );
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
