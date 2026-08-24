//! Structured agent runtime wiring.
//!
//! This is the daemon-side bridge between IPC `StartAgentRun` commands
//! and each supported provider's structured mode. Terminal clients
//! keep using the PTY path; API/Tauri/iOS clients—and eventually the
//! meta-agent control plane—consume the same normalized `Agent*`
//! events regardless of which CLI is configured.

use crate::ServerConfig;
use crate::agent_stream::{
    AgentStreamConfig, AgentStreamIo, AgentStreamSpawner, ParsedAgentEvent, encode_user_text_jsonl,
    parse_agent_jsonl_line,
};
use lazybox_agents::{SpawnCtx, StructuredAgentProtocol};
use lazybox_ipc::{
    AgentApprovalDecision, AgentInputMessage, AgentQuestionAnswer, AgentRunAccess, AgentRunId,
    AgentRunRequestId, AgentRuntimeMode, AgentUsage, Event,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Server-side handle for a running structured agent process.
pub struct AgentRunHandle {
    pub input_tx: mpsc::Sender<AgentInputMessage>,
    pub task: tokio::task::JoinHandle<()>,
    pub session_key: lazybox_core::SessionKey,
    pub working_claim_holder: Option<String>,
}

/// Per-run follow-up backlog. Structured agents process turns serially;
/// past this depth a sender WAITS for the run to drain (surfacing a
/// periodic "still queued" notice) rather than dropping the message —
/// a typed reply is never destroyed by backpressure (#1249). The bound
/// keeps memory flat; delivery order is the channel's FIFO.
pub const AGENT_INPUT_CHANNEL_CAPACITY: usize = 64;

/// How long a queued agent-input send waits before surfacing a "still
/// queued" notice. The send keeps waiting after each notice — the cap
/// is a pacing signal, never an admission gate (#1249).
const AGENT_INPUT_STALL_NOTICE_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// Resolve the model-tier args to append to a structured run's argv.
/// Only an *explicit* alias adds args — a `None` alias keeps the agent's
/// own default, unlike [`lazybox_core::AgentModels::resolve_args`]`(None)`,
/// which falls back to the configured default tier and would silently
/// re-pin every existing headless run's model. Mirrors the PTY-spawn
/// resolution in [`crate::spawn_plan`].
fn structured_model_args(
    cfg: &lazybox_config::Config,
    agent: &str,
    model_alias: Option<&str>,
) -> Vec<String> {
    match model_alias {
        Some(alias) => cfg.agent_models(agent).resolve_args(Some(alias)),
        None => Vec::new(),
    }
}

pub async fn handle_start_agent_run(
    config: &ServerConfig,
    request_id: AgentRunRequestId,
    session_key: lazybox_core::SessionKey,
    session_id: Option<lazybox_core::SessionId>,
    source_terminal_id: Option<lazybox_ipc::TerminalId>,
    agent: String,
    mode: AgentRuntimeMode,
    cwd: Option<String>,
    initial_input: Option<AgentInputMessage>,
    resume_latest: bool,
    access: AgentRunAccess,
    model_alias: Option<String>,
) {
    if mode != AgentRuntimeMode::StreamJson {
        let _ = config.bus.send(Event::AgentRunStartFailed {
            request_id: request_id.clone(),
            message: "only StreamJson agent runs are wired; use Spawn for terminal mode".into(),
        });
        return;
    }

    let Some(agent_impl) = config.agents.get(&agent) else {
        let _ = config.bus.send(Event::AgentRunStartFailed {
            request_id: request_id.clone(),
            message: format!("no agent registered for id {agent}"),
        });
        return;
    };
    let Some(protocol) = agent_impl.structured_protocol() else {
        let _ = config.bus.send(Event::AgentRunStartFailed {
            request_id: request_id.clone(),
            message: format!(
                "{agent} supports interactive terminals but has no structured runtime adapter"
            ),
        });
        return;
    };

    let (cwd_path, resolved_session_id, resolved_session_key) = match resolve_target(
        config,
        &session_key,
        session_id,
        source_terminal_id,
        &agent,
        cwd,
    )
    .await
    {
        Ok(target) => target,
        Err(message) => {
            let _ = config.bus.send(Event::AgentRunStartFailed {
                request_id: request_id.clone(),
                message,
            });
            return;
        }
    };
    let spawn_ctx = SpawnCtx {
        session_key: resolved_session_key.as_str().to_string(),
        worktree: cwd_path
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
        repo: None,
        pr_number: None,
        env: Default::default(),
        skip_permissions: false,
        access: AgentRunAccess::Default,
        // Structured runs expose lifecycle events through their own
        // JSONL protocol, so they don't need the interactive PTY
        // path's hook-command settings injection.
        hook_settings_path: None,
        // Not skip-permissions, so the strict-MCP gate never applies.
        strict_mcp: false,
    };
    // Load the config once for both the model-tier resolution and the
    // gateway/env routing below.
    let yaml = lazybox_config::Config::load().unwrap_or_default();
    // Escalate this headless run to a chosen model tier when asked — a
    // Critic review or Ask-about-this-PR can run at Opus while the working
    // agent stays on its default (#1312 follow-up). Model args land last on
    // argv, exactly as PTY spawns append them (`spawn_plan::argv_for`).
    let model_args = structured_model_args(&yaml, &agent, model_alias.as_deref());
    let mut argv = agent_impl.spawn(&spawn_ctx);
    argv.extend(model_args);
    let Some((program, extra_args)) = argv.split_first() else {
        let _ = config.bus.send(Event::AgentRunStartFailed {
            request_id: request_id.clone(),
            message: format!("{agent} returned an empty argv"),
        });
        return;
    };

    // Structured runs speak to the same upstream as PTY spawns, so
    // they need the same LLM-gateway base-URL routing
    // (`agent.llm_gateway_url` → ANTHROPIC_BASE_URL / OPENAI_BASE_URL)
    // and the same per-agent spawn-env defaults (Codex brew suppression).
    // They opt OUT of the metering proxy (`meter = false`): a structured
    // run already reports its token usage by parsing its own stream-json,
    // so routing it through the proxy too would count every turn twice in
    // the header summary (#1109).
    let env = crate::spawn_plan::gateway_env_for_agent(&yaml, Some(agent_impl.as_ref()), false);
    let env = crate::spawn_plan::with_agent_spawn_defaults(env, Some(agent_impl.as_ref()));

    let mut stream_config = AgentStreamConfig::new(protocol, program.clone());
    stream_config.cwd = cwd_path;
    stream_config.extra_args = extra_args.to_vec();
    stream_config.env = env;
    stream_config.access = access;
    stream_config.continue_latest = resume_latest;

    let io = match config
        .agent_stream_spawner
        .spawn(stream_config.clone())
        .await
    {
        Ok(io) => io,
        Err(e) => {
            let _ = config.bus.send(Event::AgentRunStartFailed {
                request_id: request_id.clone(),
                message: e.to_string(),
            });
            return;
        }
    };

    let run_id = AgentRunId(config.next_agent_run_id.fetch_add(1, Ordering::Relaxed));
    let (input_tx, input_rx) = mpsc::channel(AGENT_INPUT_CHANNEL_CAPACITY);

    let bus = config.bus.clone();
    let runs = config.agent_runs.clone();
    let spawner = config.agent_stream_spawner.clone();
    let session_key_for_task = resolved_session_key.clone();
    let config_for_cleanup = config.clone();
    let claims_workspace = access != AgentRunAccess::ReadOnly;
    let working_claim_holder = claims_workspace.then(crate::working_claims::structured_holder_key);
    let working_claim_holder_for_cleanup = working_claim_holder.clone();
    let agent_for_event = agent.clone();
    // Gate the spawned task on a oneshot so it can't run before the outer code
    // has inserted the handle, published AgentRunStarted, and queued initial
    // input. Without the gate, an immediately exiting child could both leave
    // a stale orphan handle and publish Finished before Started.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = ready_rx.await;
        let outcome = drive_agent_stream(
            run_id,
            protocol,
            io,
            stream_config,
            spawner,
            input_rx,
            bus.clone(),
        )
        .await;
        // The handle in `runs` is the single token for the terminal
        // event: whoever removes it owns the `AgentRunFinished`. If
        // `handle_interrupt_agent_run` removed it first it already sent
        // "interrupted", so we stay silent to avoid a duplicate.
        if runs.lock().await.remove(&run_id).is_some() {
            let (exit_code, error) = match outcome {
                DriveOutcome::Completed { exit_code } => (exit_code, None),
                DriveOutcome::Errored { error } => (None, Some(error)),
            };
            let _ = bus.send(Event::AgentRunFinished {
                run_id,
                exit_code,
                error,
            });
            if let Some(holder) = working_claim_holder_for_cleanup {
                crate::working_claims::release_structured(&config_for_cleanup, &holder).await;
            }
        }
    });
    let _workspace_agent = config
        .spawn
        .lock_workspace_agent(&resolved_session_key)
        .await;
    config.agent_runs.lock().await.insert(
        run_id,
        AgentRunHandle {
            input_tx: input_tx.clone(),
            task,
            session_key: resolved_session_key.clone(),
            working_claim_holder: working_claim_holder.clone(),
        },
    );
    let _ = config.bus.send(Event::AgentRunStarted {
        request_id,
        run_id,
        session_key: session_key_for_task,
        session_id: resolved_session_id,
        agent: agent_for_event,
        mode,
    });

    // The channel was created moments ago with a fresh capacity, so an
    // awaited send returns immediately unless the child already died —
    // the one case try_send used to lose the prompt to (#1249): the
    // agent then started with NO prompt and no resend affordance.
    if let Some(input) = initial_input
        && input_tx.send(input).await.is_err()
    {
        let _ = config.bus.send(Event::provider_error_retryable(
            "agent_run:input",
            "the agent exited before its initial prompt could be delivered — start it again to resend",
        ));
    }
    if claims_workspace {
        crate::working_claims::acquire_structured(
            config,
            lazybox_core::WorkspaceKey::new(resolved_session_key.as_str()),
            working_claim_holder
                .as_deref()
                .expect("claiming run has a durable holder"),
            resolved_session_id,
        )
        .await;
    }
    // Release only after the complete run-start contract is externally
    // visible and its initial input is queued. On a multi-thread runtime the
    // child task can run the instant this signal is sent; releasing earlier
    // allowed an immediate-EOF child to publish Finished before Started.
    let _ = ready_tx.send(());
}

pub async fn handle_send_agent_input(
    config: &ServerConfig,
    run_id: AgentRunId,
    message: AgentInputMessage,
) {
    let input_tx = {
        let runs = config.agent_runs.lock().await;
        let Some(run) = runs.get(&run_id) else {
            let _ = config.bus.send(Event::provider_error_permanent(
                "agent_run",
                format!("unknown agent run {:?}", run_id),
            ));
            return;
        };
        run.input_tx.clone()
    };
    // Never drop what the user typed (advise-never-forbid, #1249): this
    // runs on the Detached lane in its own task, so it can simply wait
    // for a queue slot instead of refusing with "queue is full; retry".
    // A wedged consumer surfaces as a periodic "still queued" notice —
    // honest pacing, not a refusal — and delivery resumes the moment
    // the run drains.
    let mut send = std::pin::pin!(input_tx.send(message));
    loop {
        match tokio::time::timeout(AGENT_INPUT_STALL_NOTICE_AFTER, &mut send).await {
            Ok(Ok(())) => return,
            Ok(Err(_closed)) => {
                let _ = config.bus.send(Event::provider_error_permanent(
                    "agent_run",
                    format!(
                        "agent run {:?} has ended — the message was not delivered; start a new run and resend it",
                        run_id
                    ),
                ));
                return;
            }
            Err(_elapsed) => {
                let _ = config.bus.send(Event::provider_error_retryable(
                    "agent_run:input",
                    format!(
                        "message to agent run {:?} is queued behind a busy turn — it will be delivered when the agent catches up",
                        run_id
                    ),
                ));
            }
        }
    }
}

pub async fn handle_interrupt_agent_run(config: &ServerConfig, run_id: AgentRunId) {
    let Some(run) = config.agent_runs.lock().await.remove(&run_id) else {
        return;
    };
    let working_claim_holder = run.working_claim_holder.clone();
    run.task.abort();
    let _ = run.task.await;
    let _ = config.bus.send(Event::AgentRunFinished {
        run_id,
        exit_code: None,
        error: Some("interrupted".into()),
    });
    if let Some(holder) = working_claim_holder {
        crate::working_claims::release_structured(config, &holder).await;
    }
}

pub async fn handle_decide_agent_approval(
    config: &ServerConfig,
    run_id: AgentRunId,
    request_id: String,
    decision: AgentApprovalDecision,
) {
    let text = match decision {
        AgentApprovalDecision::Approve => format!("Approved request {request_id}."),
        AgentApprovalDecision::Deny { reason } => {
            format!(
                "Denied request {request_id}: {}",
                reason.unwrap_or_else(|| "user denied".into())
            )
        }
    };
    handle_send_agent_input(
        config,
        run_id,
        AgentInputMessage {
            text: Some(text),
            json: None,
        },
    )
    .await;
}

pub async fn handle_answer_agent_question(
    config: &ServerConfig,
    run_id: AgentRunId,
    _question_id: String,
    answer: AgentQuestionAnswer,
) {
    handle_send_agent_input(
        config,
        run_id,
        AgentInputMessage {
            text: Some(answer.answer),
            json: None,
        },
    )
    .await;
}

async fn resolve_target(
    config: &ServerConfig,
    session_key: &lazybox_core::SessionKey,
    session_id: Option<lazybox_core::SessionId>,
    source_terminal_id: Option<lazybox_ipc::TerminalId>,
    agent: &str,
    cwd: Option<String>,
) -> Result<
    (
        Option<PathBuf>,
        Option<lazybox_core::SessionId>,
        lazybox_core::SessionKey,
    ),
    String,
> {
    if let Some(terminal_id) = source_terminal_id {
        if config.terminal.backend_key_for(terminal_id).await.is_none() {
            return Err("source terminal is no longer running".into());
        }
        let Some((owner, kind)) = config.terminal.terminal_meta_for(terminal_id).await else {
            return Err("source terminal is no longer running".into());
        };
        if !matches!(kind, lazybox_ipc::TerminalKind::Agent(id) if id == agent) {
            return Err("source terminal does not match the requested agent".into());
        }
        let Some(owning_session) = config.terminal.terminal_session_for(terminal_id).await else {
            return Err("source terminal has no isolated session to hand off".into());
        };
        let workspace = load_workspace(config, &owner)?;
        let Some(session) = workspace.find_session(owning_session) else {
            return Err("source terminal's session is no longer in its workspace".into());
        };
        return Ok((
            Some(session.worktree_path.clone()),
            Some(owning_session),
            owner,
        ));
    }
    if let Some(cwd) = cwd {
        return Ok((Some(PathBuf::from(cwd)), session_id, session_key.clone()));
    }
    let key = lazybox_core::WorkspaceKey::new(session_key.as_str());
    let record = config
        .store
        .get_workspace(&key)
        .map_err(|error| format!("could not load agent workspace: {error}"))?;
    let Some(record) = record else {
        // No workspace behind this key (e.g. the help assistant's
        // sentinel): pick a neutral cwd instead of the daemon's own —
        // a stray CLAUDE.md there would leak into the run's context.
        return Ok((Some(std::env::temp_dir()), None, session_key.clone()));
    };
    let json = record
        .workspace_json
        .ok_or_else(|| "agent workspace has no persisted session data".to_string())?;
    let workspace = lazybox_core::Workspace::decode_persisted(&json)
        .map_err(|error| format!("could not decode agent workspace: {error}"))?;
    let session = match session_id {
        Some(id) => workspace
            .find_session(id)
            .ok_or_else(|| "requested agent session is no longer in its workspace".to_string())?,
        None => workspace
            .default_session()
            .ok_or_else(|| "agent workspace has no session to run in".to_string())?,
    };
    Ok((
        Some(session.worktree_path.clone()),
        Some(session.id),
        session_key.clone(),
    ))
}

fn load_workspace(
    config: &ServerConfig,
    session_key: &lazybox_core::SessionKey,
) -> Result<lazybox_core::Workspace, String> {
    let key = lazybox_core::WorkspaceKey::new(session_key.as_str());
    let record = config
        .store
        .get_workspace(&key)
        .map_err(|error| format!("could not load source workspace: {error}"))?
        .ok_or_else(|| "source workspace no longer exists".to_string())?;
    let json = record
        .workspace_json
        .ok_or_else(|| "source workspace has no persisted session data".to_string())?;
    lazybox_core::Workspace::decode_persisted(&json)
        .map_err(|error| format!("could not decode source workspace: {error}"))
}

/// How a structured driver ended, so the spawning task can decide
/// whether to emit the terminal `AgentRunFinished` event.
enum DriveOutcome {
    /// Agent stdout reached EOF — the run completed.
    Completed { exit_code: Option<i32> },
    /// stdout errored — the run is dead; the terminal event must carry
    /// the error so clients holding the run id can reset instead of
    /// writing into a dead process forever.
    Errored { error: String },
}

async fn drive_agent_stream(
    run_id: AgentRunId,
    protocol: StructuredAgentProtocol,
    io: AgentStreamIo,
    config: AgentStreamConfig,
    spawner: Arc<dyn AgentStreamSpawner>,
    input_rx: mpsc::Receiver<AgentInputMessage>,
    bus: tokio::sync::broadcast::Sender<Event>,
) -> DriveOutcome {
    match protocol {
        StructuredAgentProtocol::ClaudeStreamJson => {
            drive_persistent_stream(run_id, protocol, io, input_rx, bus).await
        }
        StructuredAgentProtocol::CodexExecJson => {
            drive_codex_exec(run_id, io, config, spawner, input_rx, bus).await
        }
    }
}

/// Claude accepts turns over one persistent bidirectional JSONL child.
async fn drive_persistent_stream(
    run_id: AgentRunId,
    protocol: StructuredAgentProtocol,
    io: AgentStreamIo,
    mut input_rx: mpsc::Receiver<AgentInputMessage>,
    bus: tokio::sync::broadcast::Sender<Event>,
) -> DriveOutcome {
    let AgentStreamIo {
        mut stdin,
        stdout,
        wait,
    } = io;
    let mut stdout = BufReader::new(stdout).lines();
    let mut mapper = StreamEventMapper::default();
    let mut input_closed = false;
    loop {
        tokio::select! {
            input = input_rx.recv(), if !input_closed => {
                let Some(input) = input else {
                    input_closed = true;
                    continue;
                };
                if let Err(e) = write_agent_input(&mut stdin, input).await {
                    let _ = bus.send(Event::provider_error_retryable(
                        "agent_run:stdin",
                        e.to_string(),
                    ));
                }
            }
            line = stdout.next_line() => {
                match line {
                    Ok(Some(line)) => match parse_agent_jsonl_line(protocol, &line) {
                        Ok(parsed) => {
                            for event in mapper.map(run_id, parsed) {
                                let _ = bus.send(event);
                            }
                        }
                        Err(e) => {
                            let _ = bus.send(Event::AgentDebug {
                                run_id,
                                message: format!(
                                    "unparseable {} line: {e}: {line}",
                                    protocol.display_name()
                                ),
                            });
                        }
                    },
                    Ok(None) => {
                        return DriveOutcome::Completed { exit_code: wait.await };
                    },
                    Err(e) => {
                        return DriveOutcome::Errored {
                            error: format!("agent stdout read failed: {e}"),
                        };
                    }
                }
            }
        }
    }
}

/// Codex's stable non-interactive surface is one `exec --json` process
/// per turn. This loop keeps a single lazybox run alive, captures the
/// emitted thread id, and launches `exec resume` for each queued
/// follow-up so clients observe the same lifecycle as Claude's
/// persistent stream.
async fn drive_codex_exec(
    run_id: AgentRunId,
    first_io: AgentStreamIo,
    base_config: AgentStreamConfig,
    spawner: Arc<dyn AgentStreamSpawner>,
    mut input_rx: mpsc::Receiver<AgentInputMessage>,
    bus: tokio::sync::broadcast::Sender<Event>,
) -> DriveOutcome {
    let protocol = StructuredAgentProtocol::CodexExecJson;
    let mut first_io = Some(first_io);
    let mut session_id: Option<String> = None;
    let mut mapper = StreamEventMapper::default();

    while let Some(input) = input_rx.recv().await {
        let io = if let Some(io) = first_io.take() {
            io
        } else {
            let Some(resume_id) = session_id.clone() else {
                return DriveOutcome::Errored {
                    error: "Codex completed a turn without returning a thread id; cannot resume"
                        .into(),
                };
            };
            let mut resume_config = base_config.clone();
            resume_config.resume_session_id = Some(resume_id);
            match spawner.spawn(resume_config).await {
                Ok(io) => io,
                Err(error) => {
                    return DriveOutcome::Errored {
                        error: format!("spawn Codex follow-up: {error}"),
                    };
                }
            }
        };

        let AgentStreamIo {
            mut stdin,
            stdout,
            wait,
        } = io;
        if let Err(error) = write_codex_input(&mut stdin, input).await {
            return DriveOutcome::Errored {
                error: format!("write Codex turn: {error}"),
            };
        }
        if let Err(error) = stdin.shutdown().await {
            return DriveOutcome::Errored {
                error: format!("close Codex turn input: {error}"),
            };
        }
        drop(stdin);

        let mut stdout = BufReader::new(stdout).lines();
        loop {
            match stdout.next_line().await {
                Ok(Some(line)) => match parse_agent_jsonl_line(protocol, &line) {
                    Ok(mut parsed) => {
                        if let ParsedAgentEvent::SessionInit {
                            session_id: Some(id),
                            ..
                        } = &parsed
                        {
                            session_id = Some(id.clone());
                        }
                        if let ParsedAgentEvent::Result {
                            session_id: event_session_id,
                            ..
                        } = &mut parsed
                            && event_session_id.is_none()
                        {
                            *event_session_id = session_id.clone();
                        }
                        for event in mapper.map(run_id, parsed) {
                            let _ = bus.send(event);
                        }
                    }
                    Err(error) => {
                        let _ = bus.send(Event::AgentDebug {
                            run_id,
                            message: format!(
                                "unparseable {} line: {error}: {line}",
                                protocol.display_name()
                            ),
                        });
                    }
                },
                Ok(None) => break,
                Err(error) => {
                    return DriveOutcome::Errored {
                        error: format!("agent stdout read failed: {error}"),
                    };
                }
            }
        }

        if let Some(exit_code) = wait.await
            && exit_code != 0
        {
            return DriveOutcome::Errored {
                error: format!("Codex turn exited with status {exit_code}"),
            };
        }
    }

    DriveOutcome::Completed { exit_code: Some(0) }
}

async fn write_agent_input<W>(
    stdin: &mut W,
    input: AgentInputMessage,
) -> Result<(), crate::ServerError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let line = if let Some(json) = input.json {
        if json.ends_with('\n') {
            json
        } else {
            format!("{json}\n")
        }
    } else if let Some(text) = input.text {
        encode_user_text_jsonl(text)?
    } else {
        return Ok(());
    };
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn write_codex_input<W>(
    stdin: &mut W,
    input: AgentInputMessage,
) -> Result<(), crate::ServerError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let prompt = input.text.or(input.json).unwrap_or_default();
    stdin.write_all(prompt.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

#[derive(Default)]
struct StreamEventMapper {
    tool_ids_by_index: HashMap<u64, String>,
    permission_count: u64,
    question_count: u64,
}

impl StreamEventMapper {
    fn map(&mut self, run_id: AgentRunId, parsed: ParsedAgentEvent) -> Vec<Event> {
        let mut events = vec![Event::AgentRawJson {
            run_id,
            json: serde_json::to_string(parsed.raw()).unwrap_or_else(|_| "{}".into()),
        }];

        match parsed {
            ParsedAgentEvent::TextDelta { text, .. } => {
                events.push(Event::AgentAssistantTextDelta {
                    run_id,
                    delta: text,
                });
            }
            ParsedAgentEvent::ToolUseStart {
                index,
                id,
                name,
                input,
                ..
            } => {
                let call_id = id
                    .or_else(|| index.map(|i| format!("tool-index-{i}")))
                    .unwrap_or_else(|| "tool-unknown".into());
                if let Some(index) = index {
                    self.tool_ids_by_index.insert(index, call_id.clone());
                }
                events.push(Event::AgentToolCallStarted {
                    run_id,
                    call_id,
                    name: name.unwrap_or_else(|| "unknown".into()),
                    input_json: input.map(|v| v.to_string()),
                });
            }
            ParsedAgentEvent::ToolUseInputDelta {
                index,
                partial_json,
                ..
            } => {
                let call_id = index
                    .and_then(|i| self.tool_ids_by_index.get(&i).cloned())
                    .or_else(|| index.map(|i| format!("tool-index-{i}")))
                    .unwrap_or_else(|| "tool-unknown".into());
                events.push(Event::AgentToolCallDelta {
                    run_id,
                    call_id,
                    delta_json: partial_json,
                });
            }
            ParsedAgentEvent::ToolUseStop {
                index,
                id,
                output,
                error,
                ..
            } => {
                // Claude emits `content_block_stop` for EVERY content block
                // — text, thinking, and tool_use alike — but only a
                // `tool_use` block recorded a start (`tool_ids_by_index`).
                // Resolve the call id from an explicit id (Codex) or a
                // recorded tool start only; a stop matching neither is a
                // non-tool block, so emit nothing rather than fabricating a
                // phantom `tool-index-N` finished-call in every client.
                // `remove` consumes the mapping — Claude resets block
                // indices per message, so a reused index in a later message
                // must not re-resolve a prior turn's tool. Remove
                // unconditionally (even when an explicit `id` wins) so a
                // provider that ever sets both fields can't leave a stale
                // entry behind to mis-resolve a later reuse of the index.
                let by_index = index.and_then(|i| self.tool_ids_by_index.remove(&i));
                let call_id = id.or(by_index);
                if let Some(call_id) = call_id {
                    events.push(Event::AgentToolCallFinished {
                        run_id,
                        call_id,
                        output_json: output.map(|value| value.to_string()),
                        error,
                    });
                }
            }
            ParsedAgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                events.push(Event::AgentUsage {
                    run_id,
                    usage: AgentUsage {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                        cost_usd_micros: None,
                    },
                });
            }
            ParsedAgentEvent::Result {
                result,
                session_id,
                usage,
                raw,
            } => {
                if let Some(usage) = usage.as_ref().and_then(agent_usage_from_value) {
                    events.push(Event::AgentUsage { run_id, usage });
                }
                events.push(Event::AgentTurnFinished {
                    run_id,
                    result,
                    session_id,
                    error: result_error(&raw),
                });
            }
            ParsedAgentEvent::PermissionRequest {
                tool_name,
                prompt,
                raw,
            } => {
                self.permission_count += 1;
                events.push(Event::AgentPermissionRequest {
                    run_id,
                    request_id: format!("permission-{}", self.permission_count),
                    tool_name: tool_name.unwrap_or_else(|| "unknown".into()),
                    input_json: object_field_json(&raw, &["input", "tool_input"]),
                    reason: prompt,
                });
            }
            ParsedAgentEvent::UserQuestion { prompt, raw } => {
                self.question_count += 1;
                events.push(Event::AgentUserQuestion {
                    run_id,
                    question_id: format!("question-{}", self.question_count),
                    prompt: prompt.unwrap_or_else(|| "Question".into()),
                    choices: question_choices(&raw),
                    allow_freeform: true,
                });
            }
            ParsedAgentEvent::HookEvent { name, .. } => {
                events.push(Event::AgentDebug {
                    run_id,
                    message: format!("hook event: {}", name.unwrap_or_else(|| "unknown".into())),
                });
            }
            ParsedAgentEvent::SessionInit { .. }
            | ParsedAgentEvent::UserMessage { .. }
            | ParsedAgentEvent::Raw(_) => {}
        }
        events
    }
}

fn is_error_result(raw: &Value) -> bool {
    raw.get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || raw.get("subtype").and_then(Value::as_str) == Some("error")
        || matches!(
            raw.get("type").and_then(Value::as_str),
            Some("turn.failed" | "error")
        )
}

fn result_error(raw: &Value) -> Option<String> {
    if !is_error_result(raw) {
        return None;
    }
    raw.get("error")
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("message").and_then(Value::as_str))
        })
        .or_else(|| raw.get("message").and_then(Value::as_str))
        .or_else(|| raw.get("result").and_then(Value::as_str))
        .map(str::to_string)
}

fn agent_usage_from_value(raw: &Value) -> Option<AgentUsage> {
    if !raw.is_object() {
        return None;
    }
    Some(AgentUsage {
        input_tokens: raw.get("input_tokens").and_then(Value::as_u64),
        output_tokens: raw.get("output_tokens").and_then(Value::as_u64),
        cache_creation_input_tokens: raw
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        cache_read_input_tokens: raw
            .get("cache_read_input_tokens")
            .or_else(|| raw.get("cached_input_tokens"))
            .and_then(Value::as_u64),
        cost_usd_micros: None,
    })
}

fn object_field_json(raw: &Value, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = raw.get(*name) {
            return Some(value.to_string());
        }
    }
    None
}

fn question_choices(raw: &Value) -> Vec<String> {
    let Some(options) = raw.get("options").and_then(Value::as_array) else {
        return vec![];
    };
    options
        .iter()
        .filter_map(|option| {
            option
                .get("label")
                .and_then(Value::as_str)
                .or_else(|| option.as_str())
                .map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A headless run escalates to an explicit tier's `--model` args, while
    /// `None` keeps the agent's own default — crucially NOT the configured
    /// default tier (which `resolve_args(None)` would apply), so existing
    /// headless runs are unchanged (#1312 follow-up).
    #[test]
    fn structured_model_args_only_applies_an_explicit_tier() {
        let cfg = lazybox_config::Config::default();
        // Claude ships a built-in S/M/L menu, so an explicit tier resolves
        // to `--model …`.
        let large = structured_model_args(&cfg, "claude", Some("L"));
        assert!(
            large.iter().any(|a| a == "--model"),
            "an explicit tier appends model args: {large:?}",
        );
        // None keeps the agent's default — no args, unlike resolve_args(None)
        // which would fall back to the default tier.
        assert!(structured_model_args(&cfg, "claude", None).is_empty());
        // An unknown alias falls through to the agent default, not the
        // configured default tier.
        assert!(structured_model_args(&cfg, "claude", Some("zzz")).is_empty());
        // An agent with no configured tiers adds nothing even for an alias.
        assert!(structured_model_args(&cfg, "no-such-agent", Some("L")).is_empty());
    }

    fn tool_finished_ids(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::AgentToolCallFinished { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Claude emits `content_block_stop` for every content block. Only a
    /// tool_use block records a `ToolUseStart`, so a text/thinking block's
    /// stop must NOT fabricate a phantom finished tool-call (the old
    /// `tool-index-N` fallback did exactly that in every JSON-API client).
    #[test]
    fn non_tool_block_stop_emits_no_finished_call() {
        let run = AgentRunId(1);
        let mut m = StreamEventMapper::default();

        // A real tool at index 0: start then stop → one finished call.
        let started = m.map(
            run,
            ParsedAgentEvent::ToolUseStart {
                index: Some(0),
                id: Some("toolu_abc".into()),
                name: Some("Bash".into()),
                input: None,
                raw: json!({}),
            },
        );
        assert!(matches!(
            started.as_slice(),
            [
                Event::AgentRawJson { .. },
                Event::AgentToolCallStarted { .. }
            ]
        ));
        let stopped = m.map(run, tool_stop(0));
        assert_eq!(tool_finished_ids(&stopped), vec!["toolu_abc".to_string()]);

        // A plain text block at index 1 only ever emits a stop (no start).
        // It must produce no finished tool-call — just the raw passthrough.
        let text_stop = m.map(run, tool_stop(1));
        assert!(tool_finished_ids(&text_stop).is_empty());
        assert!(matches!(text_stop.as_slice(), [Event::AgentRawJson { .. }]));
    }

    /// Claude resets block indices per assistant message. A tool at index 0
    /// in message 1, then a text block at index 0 in message 2, must not
    /// re-resolve the message-1 tool: the stop consumes its mapping.
    #[test]
    fn reused_block_index_does_not_refinish_a_prior_tool() {
        let run = AgentRunId(1);
        let mut m = StreamEventMapper::default();
        m.map(
            run,
            ParsedAgentEvent::ToolUseStart {
                index: Some(0),
                id: Some("toolu_first".into()),
                name: Some("Bash".into()),
                input: None,
                raw: json!({}),
            },
        );
        assert_eq!(
            tool_finished_ids(&m.map(run, tool_stop(0))),
            vec!["toolu_first".to_string()]
        );
        // Message 2's text block reuses index 0 — its stop resolves nothing.
        assert!(tool_finished_ids(&m.map(run, tool_stop(0))).is_empty());
    }

    /// A stop that carries BOTH an explicit id and an index prefers the id,
    /// yet must still consume the index mapping — otherwise a later reuse of
    /// that index would mis-resolve to the stale entry. No provider sets both
    /// today, but the mapper stays correct if one ever does.
    #[test]
    fn stop_with_explicit_id_still_consumes_the_index_mapping() {
        let run = AgentRunId(1);
        let mut m = StreamEventMapper::default();
        m.map(
            run,
            ParsedAgentEvent::ToolUseStart {
                index: Some(0),
                id: Some("toolu_indexed".into()),
                name: Some("Bash".into()),
                input: None,
                raw: json!({}),
            },
        );
        // Explicit id wins for the emitted finished-call...
        let stopped = m.map(
            run,
            ParsedAgentEvent::ToolUseStop {
                index: Some(0),
                id: Some("explicit".into()),
                output: None,
                error: None,
                raw: json!({}),
            },
        );
        assert_eq!(tool_finished_ids(&stopped), vec!["explicit".to_string()]);
        // ...but index 0's mapping was consumed, so a later reuse resolves nothing.
        assert!(tool_finished_ids(&m.map(run, tool_stop(0))).is_empty());
    }

    fn tool_stop(index: u64) -> ParsedAgentEvent {
        ParsedAgentEvent::ToolUseStop {
            index: Some(index),
            id: None,
            output: None,
            error: None,
            raw: json!({}),
        }
    }
}
