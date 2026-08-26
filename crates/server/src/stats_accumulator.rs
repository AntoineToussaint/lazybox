//! The usage-stats accumulator — the durable sink that turns lazybox's
//! live-only event stream into day/week history (#1339).
//!
//! The signals for "what did I do today/this week" (agent sessions,
//! prompts, merges, turns, tokens, cost) already fan out over
//! `ServerConfig.bus`, but nothing timestamped them — a merged workspace
//! gets reaped and its history vanishes. This task subscribes to the bus,
//! maps each measurable [`Event`] to one or more [`StatEvent`]s stamped
//! with the local calendar day on receipt (events carry no emission
//! time), and folds them into the store's daily rollup. The rollup
//! survives restart, so the day/week view can look back past the session
//! that produced the work.
//!
//! Draining the bus and writing the store are decoupled: the receive loop
//! never blocks on SQLite, so it keeps up with the broadcast channel and
//! additive tallies aren't lost to lag (a dropped `PrMerged` is a
//! permanent undercount — unlike the Error Inbox's dedup-by-class, where
//! skipping a gap is harmless). A single writer task coalesces bursts into
//! one transaction.

use std::sync::Arc;

use lazybox_ipc::{Event, StatBucket, stats};
use lazybox_store::{StatEvent, Store};
use tokio::sync::{broadcast, mpsc};

use crate::ServerConfig;

/// Days of history shipped to the client on `GetStats` — enough for
/// "today" + "this week" plus a two-week look-back for the sparkline,
/// without dumping the whole retained rollup over the wire.
const STATS_WINDOW_DAYS: i64 = 34;

/// Subscribe the accumulator to the event bus. Subscribing here (not
/// inside the task) means events broadcast between this call and the
/// task's first `recv` queue instead of vanishing — the same discipline
/// `error_inbox::spawn` follows.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    let rx = config.bus.subscribe();
    let store = config.store.clone();
    // A WeakSender for the post-flush push, so holding it doesn't keep the
    // bus alive — the receive loop still observes `Closed` on teardown and
    // exits, rather than blocking forever on a channel only it references.
    let bus = config.bus.downgrade();
    tokio::spawn(async move { run(rx, store, bus).await })
}

/// The local calendar day (`YYYY-MM-DD`) an event is stamped with. Local,
/// not UTC, so "my day / week" is the user's day — a late-evening merge
/// counts toward the day the user calls today, not tomorrow-in-UTC. On a
/// remote `--connect` daemon this is the box's local day (where the work
/// ran), which the client's local-day windows may differ from by the
/// TZ offset; that's inherent to accumulating on the machine that did the
/// work, and strictly better than bucketing everyone at UTC.
fn local_day() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

async fn run(
    mut rx: broadcast::Receiver<Event>,
    store: Arc<dyn Store>,
    bus: broadcast::WeakSender<Event>,
) {
    let (tx, wx) = mpsc::unbounded_channel::<StatEvent>();
    let writer = tokio::spawn(writer_loop(wx, store, bus));
    loop {
        match rx.recv().await {
            Ok(event) => {
                let day = local_day();
                for ev in stat_events_from_event(&event, &day) {
                    // Unbounded, non-blocking: the receive loop never
                    // waits on the DB, so it drains the bus at full speed.
                    // The channel only holds low-volume stat events (never
                    // per-byte `TerminalOutput`), so it can't grow without
                    // bound. `send` fails only if the writer died.
                    let _ = tx.send(ev);
                }
            }
            // A lagged receiver dropped some events. For an additive tally
            // that's a permanent undercount, so — unlike the Error Inbox —
            // surface it rather than swallowing it silently.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("stats: bus lagged, {n} event(s) dropped — counts may undercount");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    // Close the writer channel and let it flush what's queued before we go.
    drop(tx);
    let _ = writer.await;
}

/// Drain queued [`StatEvent`]s, coalescing whatever has piled up into one
/// batched transaction so the retention prune runs per-flush, not
/// per-event. `record_stats` is a blocking SQLite write under a
/// parking_lot mutex — offloaded off the runtime worker (issue #34).
///
/// After each committed flush, re-broadcast the recent rollup so any
/// subscribed client (the always-visible "today" header strip #1344, and
/// an open Usage Stats window) reflects the write without polling. The
/// push carries the just-written totals — deterministic where a
/// client-side re-request would race the write.
async fn writer_loop(
    mut wx: mpsc::UnboundedReceiver<StatEvent>,
    store: Arc<dyn Store>,
    bus: broadcast::WeakSender<Event>,
) {
    while let Some(first) = wx.recv().await {
        let mut batch = vec![first];
        while let Ok(next) = wx.try_recv() {
            batch.push(next);
        }
        let write_store = store.clone();
        match tokio::task::spawn_blocking(move || write_store.record_stats(&batch)).await {
            // Upgrade lazily: a `None` means the bus is gone (teardown),
            // so there is no one to push to and nothing to do.
            Ok(Ok(())) => {
                if let Some(bus) = bus.upgrade() {
                    broadcast_window(&store, &bus).await;
                }
            }
            Ok(Err(e)) => tracing::warn!("stats: failed to persist: {e}"),
            Err(e) => tracing::warn!("stats: writer task panicked: {e}"),
        }
    }
}

/// Read the recent daily rollup and broadcast it as [`Event::Stats`]. The
/// shared body behind both the `GetStats` reply and the post-flush push.
/// A SQLite scan under a parking_lot mutex — offloaded off the runtime
/// worker, matching the `error_inbox` list path.
async fn broadcast_window(store: &Arc<dyn Store>, bus: &broadcast::Sender<Event>) {
    let since = (chrono::Local::now().date_naive() - chrono::Duration::days(STATS_WINDOW_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    let store = store.clone();
    let buckets = match tokio::task::spawn_blocking(move || store.list_stats_since(&since)).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            tracing::warn!("stats: list failed: {e}");
            return;
        }
        Err(e) => {
            tracing::warn!("stats: list task panicked: {e}");
            return;
        }
    };
    let buckets = buckets
        .into_iter()
        .map(|b| StatBucket {
            day: b.day,
            metric: b.metric,
            value: b.value,
        })
        .collect();
    // No receivers (no subscribed clients) is fine — nothing to refresh.
    let _ = bus.send(Event::Stats { buckets });
}

/// Reply to `Command::GetStats` by broadcasting the recent daily rollup.
pub async fn handle_get(config: &ServerConfig) {
    broadcast_window(&config.store, &config.bus).await;
}

/// Map one broadcast [`Event`] to the daily stats it contributes, or an
/// empty vec if it measures nothing. Every returned [`StatEvent`] carries
/// `day`. One event may expand to several stats — a usage report yields
/// input tokens, output tokens, *and* cost.
fn stat_events_from_event(event: &Event, day: &str) -> Vec<StatEvent> {
    let one = |metric: &str| StatEvent {
        day: day.to_string(),
        metric: metric.to_string(),
        value: 1,
    };
    match event {
        // A genuinely-fresh agent session. Deliberately NOT
        // `TerminalSpawned`, which fires again for the same logical
        // session on every startup recovery/restore reattach — counting
        // that would inflate "sessions" on each daemon restart (#1339).
        Event::AgentSessionStarted { .. } => vec![one(stats::SESSIONS)],
        Event::SnippetDelivered { .. } => vec![one(stats::PROMPTS)],
        // Both the manual `g m` and the auto-merge path emit `PrMerged`.
        Event::PrMerged { .. } => vec![one(stats::MERGED)],
        Event::AgentTurnFinished { .. } => vec![one(stats::TURNS)],
        // Per-response metering (the only usage source for interactive
        // agents): each event carries one response's tokens/cost, so
        // summing across a day is the day's total. Zero/absent fields
        // don't mint an empty bucket. `cost_usd_micros` is now populated
        // by the priced proxy (#per-session), so the day/week view shows
        // real cost instead of always-zero. (The event also carries a
        // `session_key`; per-workspace durable breakdown would add a scope
        // dimension to the rollup table — deferred to a fast-follow. The
        // live sidebar tracker already attributes cost per workspace.)
        Event::AgentSessionUsage { usage, .. } => {
            let mut out = Vec::new();
            let mut push = |metric: &str, value: Option<u64>| {
                if let Some(v) = value.filter(|v| *v > 0) {
                    out.push(StatEvent {
                        day: day.to_string(),
                        metric: metric.to_string(),
                        value: v as i64,
                    });
                }
            };
            push(stats::INPUT_TOKENS, usage.input_tokens);
            push(stats::OUTPUT_TOKENS, usage.output_tokens);
            push(stats::COST_MICROS, usage.cost_usd_micros);
            out
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::{SessionKey, WorkspaceKey};
    use lazybox_ipc::{AgentRunId, AgentUsage, TerminalId, TerminalKind};

    const DAY: &str = "2026-08-25";

    #[test]
    fn fresh_agent_session_counts_but_a_terminal_spawn_does_not() {
        // `AgentSessionStarted` is the fresh-session signal…
        let started = Event::AgentSessionStarted {
            session_key: SessionKey::from("github:o/r#1"),
        };
        let got = stat_events_from_event(&started, DAY);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].metric, stats::SESSIONS);
        assert_eq!(got[0].day, DAY);

        // …while `TerminalSpawned` is NOT counted: it re-fires for the
        // same session on every restart recovery/restore reattach, so
        // counting it would inflate "sessions" on each daemon restart.
        let spawned = Event::TerminalSpawned {
            terminal_id: TerminalId(1),
            session_key: SessionKey::from("github:o/r#1"),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        };
        assert!(stat_events_from_event(&spawned, DAY).is_empty());
    }

    #[test]
    fn usage_expands_to_tokens_and_cost_skipping_empties() {
        let ev = Event::AgentSessionUsage {
            agent_id: "claude".into(),
            session_key: Some(SessionKey::from("github:o/r#1")),
            usage: AgentUsage {
                input_tokens: Some(1000),
                output_tokens: Some(0),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                cost_usd_micros: Some(2500),
            },
        };
        let got = stat_events_from_event(&ev, DAY);
        // input_tokens (1000) + cost (2500); output=0 and cache=None skipped.
        let metrics: Vec<(&str, i64)> = got.iter().map(|s| (s.metric.as_str(), s.value)).collect();
        assert_eq!(
            metrics,
            vec![(stats::INPUT_TOKENS, 1000), (stats::COST_MICROS, 2500)],
        );
    }

    #[test]
    fn merge_and_turn_each_count_one() {
        for (ev, metric) in [
            (
                Event::PrMerged {
                    workspace_key: WorkspaceKey::new("github:o/r#1"),
                    pr_label: "o/r#1".into(),
                },
                stats::MERGED,
            ),
            (
                Event::AgentTurnFinished {
                    run_id: AgentRunId(1),
                    result: None,
                    session_id: None,
                    error: None,
                },
                stats::TURNS,
            ),
        ] {
            let got = stat_events_from_event(&ev, DAY);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].metric, metric);
            assert_eq!(got[0].value, 1);
        }
    }

    #[test]
    fn non_measured_event_yields_nothing() {
        let ev = Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        };
        assert!(stat_events_from_event(&ev, DAY).is_empty());
    }

    /// End-to-end through the real `run` loop + writer + store: a fresh
    /// `AgentSessionStarted` persists a session, a restart-style
    /// `TerminalSpawned` reattach does NOT, and a `PrMerged` persists a
    /// merge. This is the regression guard for the restart over-count.
    #[tokio::test]
    async fn run_persists_fresh_sessions_and_ignores_reattach() {
        let (tx, rx) = broadcast::channel(64);
        let store: Arc<dyn Store> = Arc::new(lazybox_store::SqliteStore::in_memory().unwrap());
        let task = tokio::spawn(run(rx, store.clone(), tx.downgrade()));

        tx.send(Event::AgentSessionStarted {
            session_key: SessionKey::from("github:o/r#1"),
        })
        .unwrap();
        // A restart re-materializes the same session as a TerminalSpawned —
        // must not add a second session.
        tx.send(Event::TerminalSpawned {
            terminal_id: TerminalId(9),
            session_key: SessionKey::from("github:o/r#1"),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        })
        .unwrap();
        tx.send(Event::PrMerged {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
        })
        .unwrap();

        let sessions = wait_for_metric(&store, stats::SESSIONS, 1).await;
        assert_eq!(sessions, 1, "one fresh session, reattach not counted");
        assert_eq!(metric_total(&store, stats::MERGED), 1);

        drop(tx);
        let _ = task.await;
    }

    /// A committed flush re-broadcasts the rollup as `Event::Stats`, so the
    /// always-visible "today" strip (#1344) reflects the write without
    /// polling — and the pushed totals already include the just-written
    /// event (no client-side round-trip race).
    #[tokio::test]
    async fn a_flush_pushes_a_fresh_stats_event() {
        let (tx, rx) = broadcast::channel(64);
        let store: Arc<dyn Store> = Arc::new(lazybox_store::SqliteStore::in_memory().unwrap());
        // Subscribe BEFORE the run loop broadcasts, so the push queues.
        let mut client = tx.subscribe();
        let task = tokio::spawn(run(rx, store.clone(), tx.downgrade()));

        tx.send(Event::PrMerged {
            workspace_key: WorkspaceKey::new("github:o/r#1"),
            pr_label: "o/r#1".into(),
        })
        .unwrap();

        // Drain until the post-flush Stats push carrying the merge lands.
        let buckets = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(Event::Stats { buckets }) = client.recv().await
                    && buckets
                        .iter()
                        .any(|b| b.metric == stats::MERGED && b.value >= 1)
                {
                    return buckets;
                }
            }
        })
        .await
        .expect("a Stats push with the merge should arrive after the flush");
        assert!(buckets.iter().any(|b| b.metric == stats::MERGED));

        drop(tx);
        let _ = task.await;
    }

    fn metric_total(store: &Arc<dyn Store>, metric: &str) -> i64 {
        store
            .list_stats_since("0000-00-00")
            .unwrap()
            .into_iter()
            .filter(|b| b.metric == metric)
            .map(|b| b.value)
            .sum()
    }

    /// Poll the store until `metric` reaches `expected` (writes are async
    /// through the writer task), up to a generous cap.
    async fn wait_for_metric(store: &Arc<dyn Store>, metric: &str, expected: i64) -> i64 {
        for _ in 0..200 {
            let total = metric_total(store, metric);
            if total >= expected {
                return total;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        metric_total(store, metric)
    }
}
