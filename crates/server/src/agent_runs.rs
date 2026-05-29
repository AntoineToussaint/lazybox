//! Structured agent runtime wiring.
//!
//! This is the daemon-side bridge between IPC `StartAgentRun` commands
//! and Claude Code's stream-json mode. Terminal clients keep using the
//! PTY path; API/Tauri/iOS clients can consume the normalized
//! `Agent*` events emitted here.

use crate::ServerConfig;
use crate::agent_stream::{
    ClaudeStreamConfig, ParsedAgentEvent, encode_user_text_jsonl, spawn_claude_stream,
};
use pilot_agents::SpawnCtx;
use pilot_ipc::{
    AgentApprovalDecision, AgentInputMessage, AgentQuestionAnswer, AgentRunId, AgentRuntimeMode,
    AgentUsage, Event,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Server-side handle for a running structured agent process.
pub struct AgentRunHandle {
    pub input_tx: mpsc::UnboundedSender<AgentInputMessage>,
    pub abort: tokio::task::AbortHandle,
}

pub async fn handle_start_agent_run(
    config: &ServerConfig,
    session_key: pilot_core::SessionKey,
    session_id: Option<pilot_core::SessionId>,
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
    };
    let argv = agent_impl.spawn(&spawn_ctx);
    let Some((program, extra_args)) = argv.split_first() else {
        let _ = config.bus.send(Event::provider_error_permanent(
            &format!("agent_run:{agent}"),
            "agent returned an empty argv",
        ));
        return;
    };

    let stream_config = ClaudeStreamConfig {
        program: program.clone(),
        cwd: cwd_path,
        extra_args: extra_args.to_vec(),
        ..ClaudeStreamConfig::default()
    };

    let child = match spawn_claude_stream(stream_config).await {
        Ok(child) => child,
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
    let session_key_for_task = session_key.clone();
    let agent_for_event = agent.clone();
    // Gate the spawned task on a oneshot so it can't reach the
    // post-completion `runs.remove(&run_id)` before the outer code
    // has inserted the handle. Without the gate, a child process
    // that exits immediately (bad argv, missing binary) could let
    // `drive_claude_stream` return before the insert lands, leaving
    // a stale orphan handle in the map.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = ready_rx.await;
        drive_claude_stream(run_id, child, input_rx, bus.clone()).await;
        runs.lock().await.remove(&run_id);
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
    session_key: &pilot_core::SessionKey,
    session_id: Option<pilot_core::SessionId>,
    cwd: Option<String>,
) -> Option<PathBuf> {
    if let Some(cwd) = cwd {
        return Some(PathBuf::from(cwd));
    }
    let key = pilot_core::WorkspaceKey::new(session_key.as_str());
    let workspace = config
        .store
        .get_workspace(&key)
        .ok()
        .flatten()
        .and_then(|record| record.workspace_json)
        .and_then(|json| serde_json::from_str::<pilot_core::Workspace>(&json).ok());
    let Some(workspace) = workspace else {
        return std::env::current_dir().ok();
    };
    if let Some(id) = session_id {
        return workspace.find_session(id).map(|s| s.worktree_path.clone());
    }
    workspace.default_session().map(|s| s.worktree_path.clone())
}

async fn drive_claude_stream(
    run_id: AgentRunId,
    child: crate::agent_stream::ClaudeStreamChild,
    mut input_rx: mpsc::UnboundedReceiver<AgentInputMessage>,
    bus: tokio::sync::broadcast::Sender<Event>,
) {
    let (mut child, mut stdin, mut stdout) = child.split();
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
                    Ok(Some(line)) => match crate::agent_stream::parse_jsonl_line(&line) {
                        Ok(parsed) => {
                            for event in mapper.map(run_id, parsed) {
                                let _ = bus.send(event);
                            }
                        }
                        Err(e) => {
                            let _ = bus.send(Event::AgentDebug {
                                run_id,
                                message: format!("unparseable Claude stream line: {e}: {line}"),
                            });
                        }
                    },
                    Ok(None) => {
                        match child.wait().await {
                            Ok(status) => {
                                let _ = bus.send(Event::AgentRunFinished {
                                    run_id,
                                    exit_code: status.code(),
                                    error: None,
                                });
                            }
                            Err(e) => {
                                let _ = bus.send(Event::AgentRunFinished {
                                    run_id,
                                    exit_code: None,
                                    error: Some(e.to_string()),
                                });
                            }
                        }
                        break;
                    },
                    Err(e) => {
                        let _ = bus.send(Event::provider_error_retryable(
                            "agent_run:stdout",
                            e.to_string(),
                        ));
                        break;
                    }
                }
            }
        }
    }
}

async fn write_agent_input(
    stdin: &mut tokio::process::ChildStdin,
    input: AgentInputMessage,
) -> Result<(), crate::ServerError> {
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
            ParsedAgentEvent::ToolUseStop { index, .. } => {
                let call_id = index
                    .and_then(|i| self.tool_ids_by_index.get(&i).cloned())
                    .or_else(|| index.map(|i| format!("tool-index-{i}")))
                    .unwrap_or_else(|| "tool-unknown".into());
                events.push(Event::AgentToolCallFinished {
                    run_id,
                    call_id,
                    output_json: None,
                    error: None,
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
}

fn result_error(raw: &Value) -> Option<String> {
    if !is_error_result(raw) {
        return None;
    }
    raw.get("error")
        .and_then(Value::as_str)
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
        cache_read_input_tokens: raw.get("cache_read_input_tokens").and_then(Value::as_u64),
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
