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
    AgentApprovalDecision, AgentInputMessage, AgentQuestionAnswer, AgentRunId, AgentRuntimeMode,
    AgentUsage, Event,
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
    pub input_tx: mpsc::UnboundedSender<AgentInputMessage>,
    pub abort: tokio::task::AbortHandle,
}

pub async fn handle_start_agent_run(
    config: &ServerConfig,
    session_key: lazybox_core::SessionKey,
    session_id: Option<lazybox_core::SessionId>,
    agent: String,
    mode: AgentRuntimeMode,
    cwd: Option<String>,
    initial_input: Option<AgentInputMessage>,
) {
    if mode != AgentRuntimeMode::StreamJson {
        let _ = config.bus.send(Event::provider_error_permanent(
            "agent_run",
            "only StreamJson agent runs are wired; use Spawn for terminal mode",
        ));
        return;
    }

    let Some(agent_impl) = config.agents.get(&agent) else {
        let _ = config.bus.send(Event::provider_error_permanent(
            &format!("agent_run:{agent}"),
            "no agent registered for this id",
        ));
        return;
    };
    let Some(protocol) = agent_impl.structured_protocol() else {
        let _ = config.bus.send(Event::provider_error_permanent(
            &format!("agent_run:{agent}"),
            "this agent supports interactive terminals but has no structured runtime adapter",
        ));
        return;
    };

    let cwd_path = resolve_cwd(config, &session_key, session_id, cwd).await;
    let spawn_ctx = SpawnCtx {
        session_key: session_key.as_str().to_string(),
        worktree: cwd_path
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
        repo: None,
        pr_number: None,
        env: Default::default(),
        skip_permissions: false,
        // Structured runs expose lifecycle events through their own
        // JSONL protocol, so they don't need the interactive PTY
        // path's hook-command settings injection.
        hook_settings_path: None,
    };
    let argv = agent_impl.spawn(&spawn_ctx);
    let Some((program, extra_args)) = argv.split_first() else {
        let _ = config.bus.send(Event::provider_error_permanent(
            &format!("agent_run:{agent}"),
            "agent returned an empty argv",
        ));
        return;
    };

    // Structured runs speak to the same upstream as PTY spawns, so
    // they need the same LLM-gateway base-URL routing
    // (`agent.llm_gateway_url` → ANTHROPIC_BASE_URL / OPENAI_BASE_URL).
    let yaml = lazybox_config::Config::load().unwrap_or_default();
    let env = crate::spawn_handler::gateway_env_for_agent(&yaml, Some(agent_impl.as_ref()));

    let mut stream_config = AgentStreamConfig::new(protocol, program.clone());
    stream_config.cwd = cwd_path;
    stream_config.extra_args = extra_args.to_vec();
    stream_config.env = env;

    let io = match config
        .agent_stream_spawner
        .spawn(stream_config.clone())
        .await
    {
        Ok(io) => io,
        Err(e) => {
            let _ = config.bus.send(Event::provider_error_permanent(
                &format!("agent_run:{agent}"),
                e.to_string(),
            ));
            return;
        }
    };

    let run_id = AgentRunId(config.next_agent_run_id.fetch_add(1, Ordering::Relaxed));
    let (input_tx, input_rx) = mpsc::unbounded_channel();

    let bus = config.bus.clone();
    let runs = config.agent_runs.clone();
    let spawner = config.agent_stream_spawner.clone();
    let session_key_for_task = session_key.clone();
    let agent_for_event = agent.clone();
    // Gate the spawned task on a oneshot so it can't reach the
    // post-completion `runs.remove(&run_id)` before the outer code
    // has inserted the handle. Without the gate, a child process
    // that exits immediately (bad argv, missing binary) could let
    // `drive_agent_stream` return before the insert lands, leaving
    // a stale orphan handle in the map.
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
        }
    });
    let abort = task.abort_handle();

    config.agent_runs.lock().await.insert(
        run_id,
        AgentRunHandle {
            input_tx: input_tx.clone(),
            abort,
        },
    );
    let _ = ready_tx.send(());

    let _ = config.bus.send(Event::AgentRunStarted {
        run_id,
        session_key: session_key_for_task,
        session_id,
        agent: agent_for_event,
        mode,
    });

    if let Some(input) = initial_input {
        let _ = input_tx.send(input);
    }
}

pub async fn handle_send_agent_input(
    config: &ServerConfig,
    run_id: AgentRunId,
    message: AgentInputMessage,
) {
    let runs = config.agent_runs.lock().await;
    let Some(run) = runs.get(&run_id) else {
        let _ = config.bus.send(Event::provider_error_permanent(
            "agent_run",
            format!("unknown agent run {:?}", run_id),
        ));
        return;
    };
    if run.input_tx.send(message).is_err() {
        let _ = config.bus.send(Event::provider_error_permanent(
            "agent_run",
            format!("agent run {:?} input channel is closed", run_id),
        ));
    }
}

pub async fn handle_interrupt_agent_run(config: &ServerConfig, run_id: AgentRunId) {
    let Some(run) = config.agent_runs.lock().await.remove(&run_id) else {
        return;
    };
    run.abort.abort();
    let _ = config.bus.send(Event::AgentRunFinished {
        run_id,
        exit_code: None,
        error: Some("interrupted".into()),
    });
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

async fn resolve_cwd(
    config: &ServerConfig,
    session_key: &lazybox_core::SessionKey,
    session_id: Option<lazybox_core::SessionId>,
    cwd: Option<String>,
) -> Option<PathBuf> {
    if let Some(cwd) = cwd {
        return Some(PathBuf::from(cwd));
    }
    let key = lazybox_core::WorkspaceKey::new(session_key.as_str());
    let workspace = config
        .store
        .get_workspace(&key)
        .ok()
        .flatten()
        .and_then(|record| record.workspace_json)
        .and_then(|json| serde_json::from_str::<lazybox_core::Workspace>(&json).ok());
    let Some(workspace) = workspace else {
        // No workspace behind this key (e.g. the help assistant's
        // sentinel): pick a neutral cwd instead of the daemon's own —
        // a stray CLAUDE.md there would leak into the run's context.
        return Some(std::env::temp_dir());
    };
    if let Some(id) = session_id {
        return workspace.find_session(id).map(|s| s.worktree_path.clone());
    }
    workspace.default_session().map(|s| s.worktree_path.clone())
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
    input_rx: mpsc::UnboundedReceiver<AgentInputMessage>,
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
    mut input_rx: mpsc::UnboundedReceiver<AgentInputMessage>,
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
    mut input_rx: mpsc::UnboundedReceiver<AgentInputMessage>,
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
                let call_id = id
                    .or_else(|| index.and_then(|i| self.tool_ids_by_index.get(&i).cloned()))
                    .or_else(|| index.map(|i| format!("tool-index-{i}")))
                    .unwrap_or_else(|| "tool-unknown".into());
                events.push(Event::AgentToolCallFinished {
                    run_id,
                    call_id,
                    output_json: output.map(|value| value.to_string()),
                    error,
                });
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
