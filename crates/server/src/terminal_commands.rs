//! Ordered background lanes for terminal-local commands.
//!
//! The connection serve loop must keep polling the event bus even when a
//! PTY, tmux client, or SQLite write is slow. At the same time, terminal
//! input cannot be detached as one task per command: scheduler order is not
//! command order, so bytes and draft revisions could be silently reordered.
//!
//! Two routers solve both constraints:
//! - I/O (`Write` / `Resize` / `Close` / `InjectPrompt` / `DeliverSnippet` /
//!   `RecoverAgentCredit`)
//!   gets one FIFO worker
//!   per terminal. Adjacent writes are concatenated and resize storms collapse
//!   to the latest size. Injection registers its readiness waiter only after
//!   earlier input has completed, then releases the lane so prompt answers can
//!   still pass while that waiter is pending.
//! - persisted prompt state gets a separate FIFO worker per terminal, so a
//!   slow SQLite draft write never stalls PTY input. Adjacent draft revisions
//!   are debounce-coalesced to the latest value.

use crate::{ServerConfig, spawn_handler};
use lazybox_ipc::{Command, Event, EventSender, TerminalId};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

/// Upper bound for one coalesced backend write. A larger one-shot paste is
/// still delivered atomically; this cap only stops an arbitrary number of
/// small queued keypresses from becoming one giant allocation.
const WRITE_BATCH_BYTES: usize = 256 * 1024;

/// Per-connection staging before commands are partitioned by terminal.
/// Admission is non-blocking so a saturated terminal subsystem cannot stall
/// event-bus forwarding on the serve loop.
pub(crate) const ROUTER_CAPACITY: usize = 64;

/// Commands retained behind one slow/wedged terminal. 8× the original
/// (#1237) — a burst of typing into a briefly-busy session must absorb,
/// not refuse. Only a genuinely wedged single terminal (a dead PTY that
/// stopped consuming for hundreds of commands) still rejects, with a
/// message naming the terminal-level cause and the remedy.
const LANE_CAPACITY: usize = 256;

/// Small enough to persist an idle draft nearly immediately, large enough to
/// collapse ordinary typing bursts instead of issuing one SQLite transaction
/// per character.
const DRAFT_COALESCE_WINDOW: Duration = Duration::from_millis(25);

/// Retire per-terminal tasks after self-exit even when a client connection
/// stays open for days. Active idle terminals keep their worker.
const LANE_IDLE_RECHECK: Duration = Duration::from_secs(60);

/// Route connection-ordered terminal I/O into independent per-terminal FIFO
/// workers. The outer channel is drained without backend awaits, so one
/// wedged terminal cannot block another terminal or the connection event bus.
pub(crate) async fn run_io_router(
    config: ServerConfig,
    event_tx: EventSender,
    mut rx: mpsc::Receiver<Command>,
) {
    let mut lanes: HashMap<TerminalId, mpsc::Sender<Command>> = HashMap::new();
    let mut workers = tokio::task::JoinSet::new();

    while let Some(command) = rx.recv().await {
        reap_completed_workers(&mut workers, "I/O");
        let terminal_id = match &command {
            Command::Write { terminal_id, .. }
            | Command::Resize { terminal_id, .. }
            | Command::Close { terminal_id, .. }
            | Command::InjectPrompt { terminal_id, .. }
            | Command::DeliverSnippet { terminal_id, .. }
            | Command::RecoverAgentCredit { terminal_id, .. }
            | Command::ResumeAgent { terminal_id }
            | Command::ReauthenticateAgent { terminal_id, .. }
            | Command::CancelAgentReauthentication { terminal_id } => *terminal_id,
            other => {
                tracing::error!(?other, "non-I/O command reached terminal I/O router");
                continue;
            }
        };
        let handle_missing_terminal = matches!(
            &command,
            Command::InjectPrompt {
                fallback_spawn: Some(_),
                ..
            }
        ) || matches!(
            &command,
            Command::DeliverSnippet { .. }
                | Command::RecoverAgentCredit { .. }
                | Command::ResumeAgent { .. }
                | Command::ReauthenticateAgent { .. }
                | Command::CancelAgentReauthentication { .. }
        );
        if !handle_missing_terminal
            && config.terminal.live_backend_key(terminal_id).is_none()
            && config.terminal.backend_key_for(terminal_id).await.is_none()
        {
            // Never allocate a 60-second worker for arbitrary stale/hostile
            // terminal ids. Prompt injection with a fallback is the one
            // exception: its documented contract can legitimately spawn a
            // replacement after the target exited.
            //
            // A no-fallback InjectPrompt still carries user text, so dropping
            // it here would lose it with no feedback. Surface the rejection so
            // the user knows to reopen the agent (#1384). Broadcast on the bus
            // (not the per-connection `event_tx`) to match the sibling
            // inject-rejections in `handle_inject_prompt_inner` and the daemon's
            // terminal-state model — "the terminal is gone" is a terminal fact,
            // delivered the same way as `TerminalExited`.
            if matches!(command, Command::InjectPrompt { .. }) {
                let _ = config.bus.send(Event::TerminalInputRejected {
                    terminal_id,
                    message: "the agent terminal is no longer running — reopen it and retry".into(),
                });
            }
            continue;
        }

        let send_error = {
            let sender = lanes.entry(terminal_id).or_insert_with(|| {
                let (tx, lane_rx) = mpsc::channel(LANE_CAPACITY);
                let lane_config = config.clone();
                let lane_event_tx = event_tx.clone();
                workers.spawn(async move {
                    run_io_lane(lane_config, lane_event_tx, terminal_id, lane_rx).await;
                });
                tx
            });
            sender.try_send(command).err()
        };
        if let Some(error) = send_error {
            let command = match error {
                mpsc::error::TrySendError::Full(command) => {
                    reject_command(
                        &event_tx,
                        &command,
                        "this terminal has stopped consuming input (hundreds of writes \
                         are already waiting) — the session looks wedged; ]]x closes it",
                    );
                    continue;
                }
                mpsc::error::TrySendError::Closed(command) => command,
            };
            tracing::warn!(
                ?terminal_id,
                ?command,
                "terminal I/O lane closed; restarting it"
            );
            lanes.remove(&terminal_id);
            let (tx, lane_rx) = mpsc::channel(LANE_CAPACITY);
            let lane_config = config.clone();
            let lane_event_tx = event_tx.clone();
            workers.spawn(async move {
                run_io_lane(lane_config, lane_event_tx, terminal_id, lane_rx).await;
            });
            if let Err(error) = tx.try_send(command) {
                tracing::error!(?terminal_id, command = ?error.into_inner(), "replacement I/O lane closed");
            } else {
                lanes.insert(terminal_id, tx);
            }
        }
    }

    drop(lanes);
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result {
            tracing::warn!("terminal I/O worker failed: {error}");
        }
    }
}

async fn run_io_lane(
    config: ServerConfig,
    event_tx: EventSender,
    terminal_id: TerminalId,
    mut rx: mpsc::Receiver<Command>,
) {
    let mut carry = None;
    loop {
        let command = match carry.take() {
            Some(command) => command,
            None => match tokio::time::timeout(LANE_IDLE_RECHECK, rx.recv()).await {
                Ok(Some(command)) => command,
                Ok(None) => break,
                Err(_) => {
                    if config.terminal.backend_key_for(terminal_id).await.is_none() {
                        break;
                    }
                    continue;
                }
            },
        };

        match command {
            Command::Write { bytes, intent, .. } => {
                let mut total = bytes.len();
                let mut writes = vec![bytes];
                while total < WRITE_BATCH_BYTES {
                    match rx.try_recv() {
                        Ok(Command::Write {
                            bytes: next_bytes,
                            intent: next_intent,
                            ..
                        }) if next_intent == intent
                            && total.saturating_add(next_bytes.len()) <= WRITE_BATCH_BYTES =>
                        {
                            total += next_bytes.len();
                            writes.push(next_bytes);
                        }
                        Ok(next) => {
                            carry = Some(next);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                spawn_handler::handle_write_batch(&config, terminal_id, &writes, intent).await;
            }
            Command::Resize { cols, rows, .. } => {
                // Resize events are snapshots, not deltas. When a drag queues
                // several before the backend catches up, only the last
                // consecutive size matters. A Write remains a hard ordering
                // barrier so input is interpreted at the size the client saw.
                let (mut latest_cols, mut latest_rows) = (cols, rows);
                loop {
                    match rx.try_recv() {
                        Ok(Command::Resize { cols, rows, .. }) => {
                            latest_cols = cols;
                            latest_rows = rows;
                        }
                        Ok(next) => {
                            carry = Some(next);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                spawn_handler::handle_resize(&config, terminal_id, latest_cols, latest_rows).await;
            }
            Command::Close {
                client_request_id, ..
            } => {
                // Close is an ordering barrier: every previously queued byte
                // reaches the backend first. On successful kill, later input
                // for this terminal is invalid and is discarded with the
                // lane; on a failed kill the live session keeps its lane.
                if spawn_handler::handle_close(&config, terminal_id, client_request_id.as_deref())
                    .await
                {
                    break;
                }
            }
            Command::InjectPrompt {
                prompt,
                fallback_spawn,
                submit,
                ..
            } => {
                // `handle_inject_prompt` returns after one of two explicit
                // ordering handshakes: an immediately-ready injection owns
                // the shared terminal lock, or a blocked injection has
                // registered its readiness waiter. The latter must release
                // this lane because the user's InputNeeded answer is a later
                // Write on the same lane.
                spawn_handler::handle_inject_prompt(
                    &config,
                    terminal_id,
                    &prompt,
                    fallback_spawn,
                    submit,
                )
                .await;
            }
            Command::DeliverSnippet {
                snippet_key,
                category,
                body,
                submit,
                ..
            } => {
                spawn_handler::handle_deliver_snippet(
                    &config,
                    terminal_id,
                    snippet_key,
                    category,
                    body,
                    submit,
                )
                .await;
            }
            Command::RecoverAgentCredit {
                client_request_id,
                continuation_prompt,
                ..
            } => {
                spawn_handler::handle_recover_agent_credit(
                    &config,
                    terminal_id,
                    client_request_id,
                    continuation_prompt,
                )
                .await;
            }
            Command::ResumeAgent { .. } => {
                crate::agent_auth::resume_agent(&config, terminal_id).await;
            }
            Command::ReauthenticateAgent { switch_account, .. } => {
                crate::agent_auth::start_reauthentication(
                    &config,
                    terminal_id,
                    switch_account,
                    Some(event_tx.clone()),
                )
                .await;
            }
            Command::CancelAgentReauthentication { .. } => {
                crate::agent_auth::cancel_reauthentication(&config, terminal_id).await;
            }
            other => {
                tracing::error!(?terminal_id, ?other, "invalid command in terminal I/O lane");
            }
        }
    }
}

/// Route durable prompt state independently from terminal I/O. Per-terminal
/// lanes preserve `RecordUserMessage` → draft-clear order on submit while
/// allowing unrelated terminals to persist concurrently.
pub(crate) async fn run_persistence_router(
    config: ServerConfig,
    event_tx: EventSender,
    mut rx: mpsc::Receiver<Command>,
) {
    let mut lanes: HashMap<TerminalId, mpsc::Sender<Command>> = HashMap::new();
    let mut workers = tokio::task::JoinSet::new();

    while let Some(command) = rx.recv().await {
        reap_completed_workers(&mut workers, "persistence");
        let terminal_id = match &command {
            Command::RecordUserMessage { terminal_id, .. }
            | Command::RecordComposingBuffer { terminal_id, .. } => *terminal_id,
            other => {
                tracing::error!(
                    ?other,
                    "non-persistence command reached terminal persistence router"
                );
                continue;
            }
        };
        if config.terminal.live_backend_key(terminal_id).is_none()
            && config.terminal.backend_key_for(terminal_id).await.is_none()
        {
            continue;
        }

        let send_error = {
            let sender = lanes.entry(terminal_id).or_insert_with(|| {
                let (tx, lane_rx) = mpsc::channel(LANE_CAPACITY);
                let lane_config = config.clone();
                workers.spawn(async move {
                    run_persistence_lane(lane_config, terminal_id, lane_rx).await;
                });
                tx
            });
            sender.try_send(command).err()
        };
        if let Some(error) = send_error {
            let command = match error {
                mpsc::error::TrySendError::Full(command) => {
                    reject_command(&event_tx, &command, "terminal persistence lane is full");
                    continue;
                }
                mpsc::error::TrySendError::Closed(command) => command,
            };
            tracing::warn!(
                ?terminal_id,
                ?command,
                "terminal persistence lane closed; restarting it"
            );
            lanes.remove(&terminal_id);
            let (tx, lane_rx) = mpsc::channel(LANE_CAPACITY);
            let lane_config = config.clone();
            workers.spawn(async move {
                run_persistence_lane(lane_config, terminal_id, lane_rx).await;
            });
            if let Err(error) = tx.try_send(command) {
                tracing::error!(
                    ?terminal_id,
                    command = ?error.into_inner(),
                    "replacement persistence lane closed"
                );
            } else {
                lanes.insert(terminal_id, tx);
            }
        }
    }

    drop(lanes);
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result {
            tracing::warn!("terminal persistence worker failed: {error}");
        }
    }
}

fn reap_completed_workers(workers: &mut tokio::task::JoinSet<()>, lane: &str) {
    while let Some(result) = workers.try_join_next() {
        if let Err(error) = result {
            tracing::warn!(lane, "terminal command worker failed: {error}");
        }
    }
}

async fn run_persistence_lane(
    config: ServerConfig,
    terminal_id: TerminalId,
    mut rx: mpsc::Receiver<Command>,
) {
    let mut carry = None;
    loop {
        let command = match carry.take() {
            Some(command) => command,
            None => match tokio::time::timeout(LANE_IDLE_RECHECK, rx.recv()).await {
                Ok(Some(command)) => command,
                Ok(None) => break,
                Err(_) => {
                    if config.terminal.backend_key_for(terminal_id).await.is_none() {
                        break;
                    }
                    continue;
                }
            },
        };

        match command {
            Command::RecordComposingBuffer { mut buffer, .. } => {
                tokio::time::sleep(DRAFT_COALESCE_WINDOW).await;
                loop {
                    match rx.try_recv() {
                        Ok(Command::RecordComposingBuffer {
                            buffer: next_buffer,
                            ..
                        }) => buffer = next_buffer,
                        Ok(next) => {
                            carry = Some(next);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                spawn_handler::handle_record_composing_buffer(&config, terminal_id, &buffer).await;
            }
            Command::RecordUserMessage { prompt, .. } => {
                spawn_handler::handle_record_user_message(&config, terminal_id, &prompt).await;
            }
            other => {
                tracing::error!(
                    ?terminal_id,
                    ?other,
                    "invalid command in terminal persistence lane"
                );
            }
        }
    }
}

pub(crate) fn reject_command(event_tx: &EventSender, command: &Command, reason: &str) {
    if let Command::RecoverAgentCredit {
        client_request_id, ..
    } = command
    {
        let _ = event_tx.send(Event::CommandFailed {
            client_request_id: client_request_id.clone(),
            message: format!("{reason}; wait for the terminal to catch up and retry"),
        });
        return;
    }

    let name = match command {
        Command::Write { .. } => "Write",
        Command::Resize { .. } => "Resize",
        Command::Close { .. } => "Close",
        Command::InjectPrompt { .. } => "InjectPrompt",
        Command::DeliverSnippet { .. } => "DeliverSnippet",
        Command::RecoverAgentCredit { .. } => "RecoverAgentCredit",
        Command::ResumeAgent { .. } => "ResumeAgent",
        Command::ReauthenticateAgent { .. } => "ReauthenticateAgent",
        Command::CancelAgentReauthentication { .. } => "CancelAgentReauthentication",
        Command::RecordUserMessage { .. } => "RecordUserMessage",
        Command::RecordComposingBuffer { .. } => "RecordComposingBuffer",
        _ => "TerminalCommand",
    };
    let _ = event_tx.send(Event::CommandRejected {
        command: name.into(),
        message: format!("{reason}; wait for the terminal to catch up and retry"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SessionBackend;

    #[test]
    fn rejected_credit_recovery_keeps_request_correlation() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let event_tx = EventSender::from_unbounded(event_tx);
        reject_command(
            &event_tx,
            &Command::RecoverAgentCredit {
                terminal_id: TerminalId(880),
                client_request_id: "recover-880".into(),
                continuation_prompt: "Continue the work you were doing.".into(),
            },
            "this terminal has stopped consuming input — the session looks wedged",
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(Event::CommandFailed {
                client_request_id,
                message,
            }) if client_request_id == "recover-880"
                && message.contains("stopped consuming input")
                && message.contains("retry")
        ));
    }

    #[tokio::test]
    async fn wedged_terminal_lane_rejects_overload_instead_of_growing() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (config, mock) = ServerConfig::in_memory_with_mock();
            let terminal_id = TerminalId(881);
            let key = mock
                .spawn(&[], None, &[], "bounded-lane")
                .await
                .expect("spawn");
            config.terminal.bind_backend(terminal_id, key.clone()).await;
            mock.wedge_write(&key).await;

            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            let event_tx = EventSender::from_unbounded(event_tx);
            let (command_tx, command_rx) = mpsc::channel(ROUTER_CAPACITY);
            let router = tokio::spawn(run_io_router(config, event_tx, command_rx));

            for _ in 0..(LANE_CAPACITY * 2) {
                command_tx
                    .send(Command::Write {
                        terminal_id,
                        bytes: vec![b'x'],
                        intent: lazybox_ipc::TerminalInputIntent::Compose,
                    })
                    .await
                    .expect("router open");
            }

            loop {
                if matches!(
                    event_rx.recv().await,
                    Some(Event::CommandRejected { command, message })
                        if command == "Write" && message.contains("stopped consuming input")
                ) {
                    break;
                }
            }
            router.abort();
            let _ = router.await;
        })
        .await
        .expect("test deadline exceeded");
    }

    #[tokio::test]
    async fn reauthenticate_then_cancel_is_fifo_for_one_terminal() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (config, mock) = ServerConfig::in_memory_with_mock();
            let terminal_id = TerminalId(882);
            let key = mock
                .spawn(
                    &["codex".into()],
                    Some(std::path::Path::new("/tmp")),
                    &[],
                    "blocked-agent",
                )
                .await
                .expect("spawn");
            let session_key = lazybox_core::SessionKey::new("github:owner/repo#882");
            config
                .terminal
                .register_terminal(
                    terminal_id,
                    key.clone(),
                    session_key.clone(),
                    lazybox_ipc::TerminalKind::Agent("codex".into()),
                )
                .await;
            config
                .agent_recovery
                .remember_spawn(crate::agent_auth::AgentResumeContext {
                    terminal_id,
                    session_key,
                    session_id: None,
                    agent_id: "codex".into(),
                    cwd: "/tmp".into(),
                    backend_key: Some(key.clone()),
                    on_main: false,
                    model_alias: None,
                    access: lazybox_ipc::AgentRunAccess::Default,
                    no_permission: false,
                    provider_session_id: Some("conversation-882".into()),
                    prompt_history: Vec::new(),
                    composing_buffer: None,
                })
                .await;
            crate::agent_auth::detect_required(&config, terminal_id, "sign-in expired").await;

            let (event_tx, _event_rx) = mpsc::unbounded_channel();
            let event_tx = EventSender::from_unbounded(event_tx);
            let (command_tx, command_rx) = mpsc::channel(ROUTER_CAPACITY);
            let router = tokio::spawn(run_io_router(config.clone(), event_tx, command_rx));
            let mut events = config.bus.subscribe();
            command_tx
                .send(Command::ReauthenticateAgent {
                    terminal_id,
                    switch_account: true,
                })
                .await
                .expect("router open");
            command_tx
                .send(Command::CancelAgentReauthentication { terminal_id })
                .await
                .expect("router open");

            loop {
                if matches!(
                    events.recv().await,
                    Ok(Event::AgentAuthFinished {
                        recovery_terminal_id,
                        success: false,
                        error: Some(error),
                        ..
                    }) if recovery_terminal_id == terminal_id && error.contains("cancelled")
                ) {
                    break;
                }
            }
            assert!(
                mock.all_argv()
                    .await
                    .iter()
                    .all(|argv| argv.as_slice() != ["codex", "login"]),
                "a cancellation queued after confirmation must stop before login"
            );
            assert!(
                mock.list().await.expect("list").contains(&key),
                "cancelling authentication must leave the blocked agent recoverable"
            );
            router.abort();
            let _ = router.await;
        })
        .await
        .expect("test deadline exceeded");
    }
}
