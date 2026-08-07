//! The Error Inbox recorder — the single durable sink for every
//! error-severity event (#831).
//!
//! Errors are emitted from many scattered daemon paths (mutation
//! rejections, provider polls, agent-run start failures, …) but they
//! all fan out over one place: `ServerConfig.bus`. A single task
//! subscribes to that bus, maps each error-severity [`Event`] to an
//! [`ErrorOccurrence`], and folds it into the durable store — deduped
//! by class so 500 identical throttle errors collapse to one row with
//! `×500`. The store survives restart, so error triage is no longer
//! bound to the session-only footer/messages log.

use std::sync::Arc;

use lazybox_core::WorkspaceKey;
use lazybox_ipc::{ErrorInboxRecord, Event};
use lazybox_store::{ErrorOccurrence, ErrorRecord, Store};
use tokio::sync::broadcast;

use crate::ServerConfig;

/// Subscribe the recorder to the event bus. Subscribing here (not
/// inside the task) means errors broadcast between this call and the
/// task's first `recv` queue instead of vanishing — the same
/// discipline `keep_awake::spawn` follows.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    let rx = config.bus.subscribe();
    let store = config.store.clone();
    tokio::spawn(async move { run(rx, store).await })
}

async fn run(mut rx: broadcast::Receiver<Event>, store: Arc<dyn Store>) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                if let Some(occ) = occurrence_from_event(&event, chrono::Utc::now()) {
                    // `record_error` is a blocking SQLite write under a
                    // parking_lot mutex — offload it off the runtime
                    // worker (issue #34), the same discipline the list
                    // path in `broadcast_snapshot` follows. Awaited here,
                    // so occurrences still fold in the order they arrive.
                    let store = store.clone();
                    match tokio::task::spawn_blocking(move || store.record_error(&occ)).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!("error-inbox: failed to persist error: {e}")
                        }
                        Err(e) => tracing::warn!("error-inbox: record task panicked: {e}"),
                    }
                }
            }
            // A lagged receiver dropped some events; the inbox is a
            // best-effort tally, so skipping the gap is acceptable.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Reply to `Command::ListErrors` (and refresh open inboxes after a
/// clear/delete) by broadcasting the current snapshot.
pub async fn handle_list(config: &ServerConfig) {
    broadcast_snapshot(config).await;
}

pub async fn handle_clear(config: &ServerConfig) {
    if let Err(e) = config.store.clear_errors() {
        tracing::warn!("error-inbox: clear failed: {e}");
    }
    broadcast_snapshot(config).await;
}

pub async fn handle_delete(config: &ServerConfig, dedupe_key: String) {
    if let Err(e) = config.store.delete_error(&dedupe_key) {
        tracing::warn!("error-inbox: delete failed: {e}");
    }
    broadcast_snapshot(config).await;
}

async fn broadcast_snapshot(config: &ServerConfig) {
    let store = config.store.clone();
    // The list is a SQLite scan under a parking_lot mutex — offload it
    // off the runtime worker (issue #34), matching the Subscribe path.
    let records = match tokio::task::spawn_blocking(move || store.list_errors()).await {
        Ok(Ok(records)) => records,
        Ok(Err(e)) => {
            tracing::warn!("error-inbox: list failed: {e}");
            return;
        }
        Err(e) => {
            tracing::warn!("error-inbox: list task panicked: {e}");
            return;
        }
    };
    let errors = records.into_iter().map(to_wire).collect();
    // No receivers (no subscribed clients) is fine — nothing to refresh.
    let _ = config.bus.send(Event::ErrorInbox { errors });
}

fn to_wire(r: ErrorRecord) -> ErrorInboxRecord {
    ErrorInboxRecord {
        dedupe_key: r.dedupe_key,
        source: r.source,
        severity: r.severity,
        operation: r.operation,
        workspace_key: r.workspace_key,
        message: r.message,
        raw: r.raw,
        count: r.count,
        first_seen: r.first_seen,
        last_seen: r.last_seen,
    }
}

/// Map one broadcast [`Event`] to a durable [`ErrorOccurrence`], or
/// `None` if the event is not an error worth persisting.
///
/// The dedupe key is `source|operation|normalized-signature` where the
/// signature is the *underlying* failure text (the mutation `reason`,
/// the provider `message`) with digit runs and whitespace flattened —
/// so occurrences that differ only in a PR number or a countdown
/// collapse into one counted class, while genuinely distinct failures
/// stay separate. The stored `message`/`raw` keep the latest full
/// occurrence as the inspectable sample.
pub(crate) fn occurrence_from_event(
    event: &Event,
    at: chrono::DateTime<chrono::Utc>,
) -> Option<ErrorOccurrence> {
    match event {
        Event::PrMergeFailed {
            workspace_key,
            pr_label,
            reason,
            ..
        } => Some(occurrence(
            "github",
            "permanent",
            Some("merge"),
            Some(workspace_key),
            format!("✗ merge failed — {pr_label}: {reason}"),
            reason.clone(),
            reason,
            at,
        )),
        Event::BranchUpdateFailed {
            workspace_key,
            pr_label,
            reason,
        } => Some(occurrence(
            "github",
            "permanent",
            Some("update-branch"),
            Some(workspace_key),
            format!("✗ update branch failed — {pr_label}: {reason}"),
            reason.clone(),
            reason,
            at,
        )),
        Event::IssueCloseFailed {
            workspace_key,
            issue_label,
            reason,
        } => Some(occurrence(
            "github",
            "permanent",
            Some("close-issue"),
            Some(workspace_key),
            format!("✗ close failed — {issue_label}: {reason}"),
            reason.clone(),
            reason,
            at,
        )),
        Event::DeleteOrCloseFailed {
            workspace_key,
            label,
            reason,
        } => Some(occurrence(
            "github",
            "permanent",
            Some("delete-or-close"),
            Some(workspace_key),
            format!("✗ delete/close failed — {label}: {reason}"),
            reason.clone(),
            reason,
            at,
        )),
        Event::ProviderError {
            source,
            message,
            detail,
            kind,
        } => {
            // Empty kind is treated as Permanent on the wire; mirror
            // that here so the inbox severity matches the TUI banner.
            let severity = if kind.is_empty() {
                "permanent"
            } else {
                kind.as_str()
            };
            let raw = if detail.is_empty() {
                message.clone()
            } else {
                detail.clone()
            };
            Some(occurrence(
                source,
                severity,
                None,
                None,
                message.clone(),
                raw,
                message,
                at,
            ))
        }
        Event::AgentRunStartFailed { message, .. } => Some(occurrence(
            "agent",
            "permanent",
            Some("agent-run-start"),
            None,
            format!("✗ agent run failed to start — {message}"),
            message.clone(),
            message,
            at,
        )),
        Event::CommandFailed { message, .. } => Some(occurrence(
            "command",
            "permanent",
            None,
            None,
            format!("✗ command failed — {message}"),
            message.clone(),
            message,
            at,
        )),
        Event::TerminalInputRejected { message, .. } => Some(occurrence(
            "terminal",
            "permanent",
            None,
            None,
            format!("✗ terminal input rejected — {message}"),
            message.clone(),
            message,
            at,
        )),
        // `Event::CommandRejected` is deliberately NOT recorded: it is
        // admission-limit backpressure ("the queue was full, retry is
        // safe"), not a failed operation. Folding it into the durable
        // inbox would tally a transient, self-healing wait state as an
        // error class. `CommandFailed` / `TerminalInputRejected` above
        // are genuine deliveries that failed, so those DO record.
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn occurrence(
    source: &str,
    severity: &str,
    operation: Option<&str>,
    workspace_key: Option<&WorkspaceKey>,
    message: String,
    raw: String,
    class_sig: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> ErrorOccurrence {
    let op = operation.unwrap_or("");
    ErrorOccurrence {
        dedupe_key: format!("{source}|{op}|{}", normalize(class_sig)),
        source: source.to_string(),
        severity: severity.to_string(),
        operation: operation.map(str::to_string),
        workspace_key: workspace_key.map(|k| k.as_str().to_string()),
        message,
        raw,
        at,
    }
}

/// Flatten a failure string into a dedupe signature: lowercase, digit
/// runs → `#`, whitespace runs → a single space. Collapses "rate
/// limited, 500 left" and "rate limited, 12 left" onto one class.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_digit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !prev_digit {
                out.push('#');
            }
            prev_digit = true;
        } else {
            prev_digit = false;
            if c.is_whitespace() {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            } else {
                out.extend(c.to_lowercase());
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-05T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn normalize_collapses_digits_and_whitespace() {
        assert_eq!(normalize("Rate limited: 500 left"), "rate limited: # left");
        assert_eq!(
            normalize("Rate limited: 500 left"),
            normalize("rate  limited:   12 left"),
        );
    }

    #[test]
    fn merge_failures_dedupe_on_reason_not_pr_label() {
        // Two different PRs failing for the same reason collapse to one
        // class — the inbox counts the *class*, keeps the latest sample.
        let a = occurrence_from_event(
            &Event::PrMergeFailed {
                workspace_key: WorkspaceKey::new("github:o/r#1"),
                pr_label: "o/r#1".into(),
                reason: "rate limited".into(),
                conflict: false,
            },
            at(),
        )
        .unwrap();
        let b = occurrence_from_event(
            &Event::PrMergeFailed {
                workspace_key: WorkspaceKey::new("github:o/r#2"),
                pr_label: "o/r#2".into(),
                reason: "rate limited".into(),
                conflict: false,
            },
            at(),
        )
        .unwrap();
        assert_eq!(a.dedupe_key, b.dedupe_key);
        assert_eq!(a.operation.as_deref(), Some("merge"));
        assert_eq!(b.workspace_key.as_deref(), Some("github:o/r#2"));
    }

    #[test]
    fn provider_error_kind_maps_to_severity_and_empty_is_permanent() {
        let retry = occurrence_from_event(
            &Event::ProviderError {
                source: "github".into(),
                message: "sync failed".into(),
                detail: "GraphQL code=RATE_LIMITED".into(),
                kind: "retryable".into(),
            },
            at(),
        )
        .unwrap();
        assert_eq!(retry.severity, "retryable");
        assert_eq!(retry.source, "github");
        assert_eq!(retry.raw, "GraphQL code=RATE_LIMITED");

        let empty = occurrence_from_event(
            &Event::ProviderError {
                source: "linear".into(),
                message: "boom".into(),
                detail: String::new(),
                kind: String::new(),
            },
            at(),
        )
        .unwrap();
        assert_eq!(empty.severity, "permanent");
        // Empty detail falls back to the message as the raw sample.
        assert_eq!(empty.raw, "boom");
    }

    #[test]
    fn non_error_events_are_ignored() {
        assert!(
            occurrence_from_event(
                &Event::TerminalExited {
                    terminal_id: lazybox_ipc::TerminalId(1),
                    exit_code: Some(0),
                    last_output: None,
                },
                at(),
            )
            .is_none()
        );
    }
}
