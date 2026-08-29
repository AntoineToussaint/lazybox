//! Durable per-session cost accumulator (#1389).
//!
//! The live per-workspace `$ METER · $cost` figure is held only in the
//! client's [`lazybox_tui_core::usage::UsageTracker`], so a restart resets
//! it to bare `$ METER` — the same ephemeral-loss class as #1362. Only a
//! scope-less global daily rollup persisted (see `stats_accumulator`).
//!
//! This subscriber closes that gap: it watches the bus for priced proxy
//! usage ([`Event::AgentSessionUsage`]) and folds each response's
//! `cost_usd_micros` into a per-session-key running total in the store
//! (`client_kv::add_session_cost`). A fresh subscriber then hydrates its
//! tracker from that total (replayed on connect as [`Event::SessionCosts`]),
//! so the figure survives a restart. A per-Space figure is summed
//! client-side from the same per-session rows.
//!
//! It writes one key at a time (awaiting each read-modify-write) so two
//! updates never race the same session's total. Volume is low — one event
//! per upstream LLM response — so a serial writer keeps up without a
//! coalescing queue.

use lazybox_ipc::Event;
use tokio::sync::broadcast;

use crate::{ServerConfig, client_kv};

/// Subscribe the accumulator to the event bus. Subscribing here (not inside
/// the task) means events broadcast between this call and the task's first
/// `recv` queue instead of vanishing — the same discipline
/// `stats_accumulator::spawn` follows.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    let rx = config.bus.subscribe();
    let config = config.clone();
    tokio::spawn(async move { run(rx, config).await })
}

async fn run(mut rx: broadcast::Receiver<Event>, config: ServerConfig) {
    loop {
        match rx.recv().await {
            Ok(Event::AgentSessionUsage {
                session_key: Some(session_key),
                usage,
                ..
            }) => {
                if let Some(cost) = usage.cost_usd_micros.filter(|c| *c > 0) {
                    client_kv::add_session_cost(&config, session_key.as_str().to_string(), cost)
                        .await;
                }
            }
            Ok(_) => {}
            // A lagged receiver dropped events. For an additive cost tally
            // that's a bounded under-count; warn rather than swallow, matching
            // `stats_accumulator`.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    "session-cost: bus lagged, {n} event(s) dropped — cost may undercount"
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::SessionKey;
    use lazybox_ipc::AgentUsage;
    use lazybox_store::{MemoryStore, Store};
    use std::sync::Arc;

    fn priced_usage(cost_micros: u64) -> AgentUsage {
        AgentUsage {
            input_tokens: Some(1_000),
            output_tokens: Some(200),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            cost_usd_micros: Some(cost_micros),
        }
    }

    /// End-to-end through the real `run` loop + store: priced session usage
    /// accumulates a per-key total; an unpriced/keyless report contributes
    /// nothing. This is the restart-survival guard.
    #[tokio::test]
    async fn priced_session_usage_persists_per_key() {
        let (tx, rx) = broadcast::channel(64);
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let config = ServerConfig::with_store(store.clone());
        let task = tokio::spawn(run(rx, config));

        tx.send(Event::AgentSessionUsage {
            agent_id: "claude".into(),
            session_key: Some(SessionKey::from("github:o/r#1")),
            usage: priced_usage(1_500_000),
        })
        .unwrap();
        tx.send(Event::AgentSessionUsage {
            agent_id: "claude".into(),
            session_key: Some(SessionKey::from("github:o/r#1")),
            usage: priced_usage(500_000),
        })
        .unwrap();
        // No session key → not attributable, dropped.
        tx.send(Event::AgentSessionUsage {
            agent_id: "claude".into(),
            session_key: None,
            usage: priced_usage(999_000),
        })
        .unwrap();

        let total = wait_for_cost(&store, "github:o/r#1", 2_000_000).await;
        assert_eq!(total, 2_000_000, "two priced responses accumulate");
        // The keyless report never minted a row.
        assert_eq!(client_kv::session_costs(&*store).len(), 1);

        drop(tx);
        let _ = task.await;
    }

    async fn wait_for_cost(store: &Arc<dyn Store>, key: &str, expected: u64) -> u64 {
        for _ in 0..200 {
            let total = client_kv::session_costs(&**store)
                .into_iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
                .unwrap_or(0);
            if total >= expected {
                return total;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        0
    }
}
