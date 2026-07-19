//! One bounded, serialized backend-I/O contract for every terminal writer.
//!
//! Terminal bytes originate from more than keyboard `Write` commands: initial
//! work prompts, live prompt injection, chat ingress, and submit confirmation
//! retries all write to the same PTY. Per-connection FIFO lanes preserve each
//! client's command order, but they cannot prevent those independent producers
//! (or a second client) from entering one backend concurrently. This module is
//! the shared boundary they all use.

use crate::ServerConfig;
use crate::backend::BackendError;
use lazybox_ipc::TerminalId;
use std::time::Duration;

/// Defense-in-depth deadline around a backend write/resize future. Raw PTY
/// writes already bound their internal queue, but the backend trait permits
/// other implementations and cancellation must remain possible.
pub(crate) const OPERATION_TIMEOUT: Duration = Duration::from_millis(750);

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
pub(crate) async fn acquire_live(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
) -> Option<tokio::sync::OwnedMutexGuard<()>> {
    let guard = config.lock_terminal_io(backend_key).await;
    if config.backend_key_for(terminal_id).await.as_deref() == Some(backend_key) {
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
    backend_key: &str,
    bytes: &[u8],
) -> Result<(), TerminalIoFailure> {
    match tokio::time::timeout(OPERATION_TIMEOUT, config.backend.write(backend_key, bytes)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(TerminalIoFailure::Backend {
            operation: "terminal write",
            source,
        }),
        Err(_) => Err(TerminalIoFailure::Timeout {
            operation: "terminal write",
            timeout_ms: OPERATION_TIMEOUT.as_millis(),
        }),
    }
}

/// Serialize and bound one write from a standalone producer.
pub(crate) async fn write_live(
    config: &ServerConfig,
    terminal_id: TerminalId,
    backend_key: &str,
    bytes: &[u8],
) -> Result<bool, TerminalIoFailure> {
    let Some(_guard) = acquire_live(config, terminal_id, backend_key).await else {
        return Ok(false);
    };
    write_locked(config, backend_key, bytes).await?;
    Ok(true)
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
    match tokio::time::timeout(
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
    }
}
