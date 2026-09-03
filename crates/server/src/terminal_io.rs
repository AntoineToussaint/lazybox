//! One bounded, serialized backend-I/O contract for every terminal writer.
//!
//! Terminal bytes originate from more than keyboard `Write` commands: initial
//! work prompts, live prompt injection, chat ingress, and submit confirmation
//! retries all write to the same PTY. Per-connection FIFO lanes preserve each
//! client's command order, but they cannot prevent those independent producers
//! (or a second client) from entering one backend concurrently. This module is
//! the shared boundary they all use.

use crate::backend::BackendError;
use crate::{ServerConfig, TerminalRegistry};
use lazybox_ipc::{AgentState, TerminalId, TerminalInputIntent, TerminalKind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Defense-in-depth deadline around a backend write/resize future. Raw PTY
/// writes already bound their internal queue, but the backend trait permits
/// other implementations and cancellation must remain possible.
///
/// Kept short on purpose: a wedged terminal — a resize into a stuck backend,
/// or a write to a child that has stopped draining its stdin — must never
/// hold the serve loop or shutdown behind it (the property `serve_loop`'s
/// `a_wedged_terminal_resize_does_not_block_other_terminals` guards; shutdown
/// there must complete well under 2s). The raw-PTY write path's own
/// enqueue-retry (`pty::WRITE_ENQUEUE_TOTAL_BUDGET`) is deliberately sized
/// `<` this bound so a transient-stall retry lands the write before this
/// outer deadline cancels it, without extending how long a genuinely wedged
/// op can stall. Transient-spike resilience comes from queue headroom +
/// reduced lock contention, not from lengthening this deadline.
pub(crate) const OPERATION_TIMEOUT: Duration = Duration::from_millis(750);
// SIGWINCH and mouse-driven repaints begin immediately. An epoch that has
// observed no output within this bound must not capture unrelated later work.
const VIEW_OUTPUT_START_TIMEOUT: Duration = Duration::from_secs(2);

static NEXT_VIEW_EPOCH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct ViewEpoch {
    id: u64,
    after_seq: u64,
    observed_output: bool,
    expires_at: tokio::time::Instant,
}

#[derive(Debug, Default)]
pub(crate) struct AgentTerminalActivity {
    view_epochs: Vec<ViewEpoch>,
    submission_in_flight: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TerminalIoFailure {
    #[error("{operation} timed out after {timeout_ms}ms")]
    Timeout {
        operation: &'static str,
        timeout_ms: u128,
    },
    #[error("{operation} failed: {source}")]
    Backend {
        operation: &'static str,
        #[source]
        source: BackendError,
    },
}

/// Acquire exclusive access to one live backend session. Liveness is checked
/// after the wait: teardown may remove or rebadge the terminal while another
/// producer owns the interaction lock.
///
/// The liveness re-check prefers the lock-free `live_backend_key` view
/// (#1237/#1256): a `parking_lot` read instead of joining the global
/// registry mutex's FIFO queue behind the output pumps. Per that view's
/// contract, drift can only cost a slow-path lookup or a momentarily stale
/// admit — never a wrong reject — so a fast-path miss falls back to the
/// authoritative mutex before refusing.
pub(crate) async fn acquire_live(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
) -> Option<tokio::sync::OwnedMutexGuard<()>> {
    let guard = config.terminal.lock_terminal_io(backend_key).await;
    if config.terminal.live_backend_key(terminal_id).as_deref() == Some(backend_key) {
        return Some(guard);
    }
    if config
        .terminal
        .backend_key_for(terminal_id)
        .await
        .as_deref()
        == Some(backend_key)
    {
        Some(guard)
    } else {
        None
    }
}

/// Perform one backend write while the caller owns the terminal interaction
/// guard. Multi-step prompt injection intentionally holds that guard across
/// paste settling and submit so user bytes cannot split the transaction.
pub(crate) async fn write_locked(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    bytes: &[u8],
    intent: TerminalInputIntent,
) -> Result<(), TerminalIoFailure> {
    let view_epoch = if intent == TerminalInputIntent::View {
        arm_view_epoch(config, terminal_id, backend_key).await?
    } else {
        None
    };
    if intent == TerminalInputIntent::Submit {
        config
            .terminal
            .lock_entries()
            .await
            .entry(terminal_id)
            .or_default()
            .activity
            .submission_in_flight = true;
    }

    let result =
        match tokio::time::timeout(OPERATION_TIMEOUT, config.backend.write(backend_key, bytes))
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(source)) => Err(TerminalIoFailure::Backend {
                operation: "terminal write",
                source,
            }),
            Err(_) => Err(TerminalIoFailure::Timeout {
                operation: "terminal write",
                timeout_ms: OPERATION_TIMEOUT.as_millis(),
            }),
        };

    match intent {
        TerminalInputIntent::Compose => {}
        TerminalInputIntent::Submit => {
            let mut entries = config.terminal.lock_entries().await;
            if let Some(entry) = entries.get_mut(&terminal_id) {
                let activity = &mut entry.activity;
                if result.is_ok() {
                    activity.view_epochs.clear();
                }
                activity.submission_in_flight = false;
            }
        }
        TerminalInputIntent::View => {
            if result.is_err()
                && let Some(epoch_id) = view_epoch
            {
                remove_unobserved_view_epoch(config, terminal_id, epoch_id).await;
            }
        }
    }

    result
}

async fn backend_output_seq(
    config: &ServerConfig,
    backend_key: &str,
) -> Result<u64, TerminalIoFailure> {
    match tokio::time::timeout(OPERATION_TIMEOUT, config.backend.output_seq(backend_key)).await {
        Ok(Ok(seq)) => Ok(seq),
        Ok(Err(source)) => Err(TerminalIoFailure::Backend {
            operation: "terminal output boundary",
            source,
        }),
        Err(_) => Err(TerminalIoFailure::Timeout {
            operation: "terminal output boundary",
            timeout_ms: OPERATION_TIMEOUT.as_millis(),
        }),
    }
}

async fn arm_view_epoch(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
) -> Result<Option<u64>, TerminalIoFailure> {
    let agent_terminal = config
        .terminal
        .terminal_meta_for(terminal_id)
        .await
        .is_some_and(|(_, kind)| matches!(kind, TerminalKind::Agent(_)));
    if !agent_terminal
        || matches!(
            config.terminal.agent_state_for(terminal_id).await,
            Some(AgentState::Exited { .. })
        )
    {
        return Ok(None);
    }

    let after_seq = backend_output_seq(config, backend_key).await?;
    let epoch_id = record_view_activity(&config.terminal, terminal_id, after_seq).await;
    Ok(Some(epoch_id))
}

pub(crate) async fn record_view_activity(
    terminals: &TerminalRegistry,
    terminal_id: TerminalId,
    after_seq: u64,
) -> u64 {
    let epoch_id = NEXT_VIEW_EPOCH.fetch_add(1, Ordering::Relaxed);
    let now = tokio::time::Instant::now();
    let mut entries = terminals.lock_entries().await;
    let activity = &mut entries.entry(terminal_id).or_default().activity;
    activity
        .view_epochs
        .retain(|epoch| epoch.observed_output || epoch.expires_at >= now);
    activity.view_epochs.push(ViewEpoch {
        id: epoch_id,
        after_seq,
        observed_output: false,
        expires_at: now + VIEW_OUTPUT_START_TIMEOUT,
    });
    epoch_id
}

async fn remove_unobserved_view_epoch(
    config: &ServerConfig,
    terminal_id: TerminalId,
    epoch_id: u64,
) {
    let mut entries = config.terminal.lock_entries().await;
    let Some(entry) = entries.get_mut(&terminal_id) else {
        return;
    };
    let activity = &mut entry.activity;
    activity
        .view_epochs
        .retain(|epoch| epoch.id != epoch_id || epoch.observed_output);
}

pub(crate) async fn clear_view_activity(config: &ServerConfig, terminal_id: TerminalId) {
    clear_view_activity_in(&config.terminal, terminal_id).await;
}

pub(crate) async fn clear_view_activity_in(terminals: &TerminalRegistry, terminal_id: TerminalId) {
    let mut entries = terminals.lock_entries().await;
    let Some(entry) = entries.get_mut(&terminal_id) else {
        return;
    };
    let activity = &mut entry.activity;
    activity.view_epochs.clear();
}

#[cfg(test)]
pub(crate) async fn has_activity(terminals: &TerminalRegistry, terminal_id: TerminalId) -> bool {
    terminals
        .lock_entries()
        .await
        .get(&terminal_id)
        .is_some_and(|entry| {
            entry.activity.submission_in_flight || !entry.activity.view_epochs.is_empty()
        })
}

/// Fold one PTY reading against view activity recorded at the I/O boundary.
///
/// Streaming output is held only when its backend sequence is newer than a
/// client view operation. A quiet reading consumes only epochs that actually
/// observed such output, so an older quiet timer cannot consume a newly armed
/// marker. Submitted input bypasses the hold from before its backend write
/// starts.
pub(crate) async fn suppresses_agent_reading(
    terminals: &TerminalRegistry,
    terminal_id: TerminalId,
    output_seq: Option<u64>,
) -> bool {
    let mut entries = terminals.lock_entries().await;
    let Some(entry) = entries.get_mut(&terminal_id) else {
        return false;
    };
    activity_suppresses_reading(&mut entry.activity, output_seq)
}

/// Lock-free core of [`suppresses_agent_reading`], operating on an entry's
/// activity the caller already holds under the registry guard. The pump's
/// single-acquisition chunk context (#1256) evaluates it inside
/// `TerminalRegistry::begin_pty_chunk` instead of re-queueing on the global
/// mutex per chunk.
pub(crate) fn activity_suppresses_reading(
    activity: &mut AgentTerminalActivity,
    output_seq: Option<u64>,
) -> bool {
    if activity.submission_in_flight {
        return false;
    }

    if let Some(output_seq) = output_seq {
        let now = tokio::time::Instant::now();
        activity
            .view_epochs
            .retain(|epoch| epoch.observed_output || epoch.expires_at >= now);
        let mut observed = false;
        for epoch in &mut activity.view_epochs {
            if output_seq > epoch.after_seq {
                epoch.observed_output = true;
                observed = true;
            }
        }
        observed
    } else {
        let observed = activity
            .view_epochs
            .iter()
            .any(|epoch| epoch.observed_output);
        activity.view_epochs.retain(|epoch| !epoch.observed_output);
        observed
    }
}

/// Serialize and bound one write from a standalone producer.
pub(crate) async fn write_live(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    bytes: &[u8],
    intent: TerminalInputIntent,
) -> Result<bool, TerminalIoFailure> {
    let Some(_guard) = acquire_live(config, terminal_id, backend_key).await else {
        return Ok(false);
    };
    write_locked(config, terminal_id, backend_key, bytes, intent).await?;
    Ok(true)
}

async fn resize_locked(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    cols: u16,
    rows: u16,
) -> Result<bool, TerminalIoFailure> {
    let view_epoch = arm_view_epoch(config, terminal_id, backend_key).await?;
    let result = match tokio::time::timeout(
        OPERATION_TIMEOUT,
        config.backend.resize(backend_key, cols, rows),
    )
    .await
    {
        Ok(Ok(())) => Ok(true),
        Ok(Err(source)) => Err(TerminalIoFailure::Backend {
            operation: "terminal resize",
            source,
        }),
        Err(_) => Err(TerminalIoFailure::Timeout {
            operation: "terminal resize",
            timeout_ms: OPERATION_TIMEOUT.as_millis(),
        }),
    };
    if result.is_err()
        && let Some(epoch_id) = view_epoch
    {
        remove_unobserved_view_epoch(config, terminal_id, epoch_id).await;
    }
    result
}

/// Serialize and bound one resize from a standalone producer.
pub(crate) async fn resize_live(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    cols: u16,
    rows: u16,
) -> Result<bool, TerminalIoFailure> {
    let Some(_guard) = acquire_live(config, terminal_id, backend_key).await else {
        return Ok(false);
    };
    resize_locked(config, terminal_id, backend_key, cols, rows).await
}
