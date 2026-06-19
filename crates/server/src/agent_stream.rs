//! Runtime helpers for Claude Code's `stream-json` print mode.
//!
//! This module intentionally stays independent of the daemon IPC event
//! types. Callers can map `ParsedAgentEvent` into whatever wire events
//! exist at integration time while still preserving the original JSON.

use crate::{ResultExt, ServerError};
use serde::{Deserialize, Serialize};

// Local shorthands so the migration touches each `?`/`Context` line
// minimally. `Result<T>` keeps existing call shapes; `bail!` swaps to
// a typed `ServerError::Agent` return.
type Result<T> = std::result::Result<T, ServerError>;
macro_rules! bail {
    ($($t:tt)*) => { return Err(crate::ServerError::Agent(format!($($t)*))) };
}
use serde_json::{Value, json};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Configuration for launching Claude Code in bidirectional JSONL mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeStreamConfig {
    /// Program name or absolute path. Defaults to `claude`.
    pub program: String,
    /// Working directory for the child process.
    pub cwd: Option<PathBuf>,
    /// Resume an existing Claude session id.
    pub resume_session_id: Option<String>,
    /// Continue the most recent Claude session.
    pub continue_latest: bool,
    /// Extra arguments appended after the required stream-json flags.
    pub extra_args: Vec<String>,
}

impl Default for ClaudeStreamConfig {
    fn default() -> Self {
        Self {
            program: "claude".to_string(),
            cwd: None,
            resume_session_id: None,
            continue_latest: false,
            extra_args: Vec::new(),
        }
    }
}

impl ClaudeStreamConfig {
    /// Build the argv vector for Claude Code stream-json mode.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = vec![
            self.program.clone(),
            "-p".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--include-partial-messages".to_string(),
            "--include-hook-events".to_string(),
            "--replay-user-messages".to_string(),
        ];

        if let Some(session_id) = &self.resume_session_id {
            argv.push("--resume".to_string());
            argv.push(session_id.clone());
        } else if self.continue_latest {
            argv.push("--continue".to_string());
        }

        argv.extend(self.extra_args.iter().cloned());
        argv
    }
}

/// One text-only user turn in Claude Code's stream-json input format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeUserTextMessage {
    pub r#type: String,
    pub message: ClaudeMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeMessage {
    pub role: String,
    pub content: Vec<ClaudeContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClaudeContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

impl ClaudeUserTextMessage {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            r#type: "user".to_string(),
            message: ClaudeMessage {
                role: "user".to_string(),
                content: vec![ClaudeContentBlock::Text { text: text.into() }],
            },
        }
    }

    /// Serialize as one JSONL record, including the trailing newline.
    pub fn to_jsonl(&self) -> Result<String> {
        let mut line = serde_json::to_string(self).ctx("serialize Claude user message")?;
        line.push('\n');
        Ok(line)
    }
}

pub fn encode_user_text_jsonl(text: impl Into<String>) -> Result<String> {
    ClaudeUserTextMessage::new(text).to_jsonl()
}

/// Internal, IPC-independent representation of interesting Claude output.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedAgentEvent {
    SessionInit {
        session_id: Option<String>,
        raw: Value,
    },
    UserMessage {
        text: Option<String>,
        raw: Value,
    },
    TextDelta {
        text: String,
        raw: Value,
    },
    ToolUseStart {
        index: Option<u64>,
        id: Option<String>,
        name: Option<String>,
        input: Option<Value>,
        raw: Value,
    },
    ToolUseInputDelta {
        index: Option<u64>,
        partial_json: String,
        raw: Value,
    },
    ToolUseStop {
        index: Option<u64>,
        raw: Value,
    },
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
        cache_read_input_tokens: Option<u64>,
        raw: Value,
    },
    Result {
        result: Option<String>,
        session_id: Option<String>,
        usage: Option<Value>,
        raw: Value,
    },
    PermissionRequest {
        tool_name: Option<String>,
        prompt: Option<String>,
        raw: Value,
    },
    UserQuestion {
        prompt: Option<String>,
        raw: Value,
    },
    HookEvent {
        name: Option<String>,
        raw: Value,
    },
    Raw(Value),
}

pub fn parse_jsonl_line(line: &str) -> Result<ParsedAgentEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        bail!("empty Claude stream line");
    }
    let raw: Value = serde_json::from_str(trimmed).ctx("parse Claude stream JSONL line")?;
    Ok(parse_value(raw))
}

fn parse_value(raw: Value) -> ParsedAgentEvent {
    let type_name = string_at(&raw, &["type"]);
    let subtype = string_at(&raw, &["subtype"]);

    if type_name == Some("system") && subtype == Some("init") {
        return ParsedAgentEvent::SessionInit {
            session_id: string_at(&raw, &["session_id"]).map(str::to_string),
            raw,
        };
    }

    if type_name == Some("user") {
        return ParsedAgentEvent::UserMessage {
            text: message_text(&raw),
            raw,
        };
    }

    if type_name == Some("result") {
        return ParsedAgentEvent::Result {
            result: string_at(&raw, &["result"]).map(str::to_string),
            session_id: string_at(&raw, &["session_id"]).map(str::to_string),
            usage: raw.get("usage").cloned(),
            raw,
        };
    }

    if type_name == Some("stream_event") {
        if let Some(parsed) = parse_stream_event(raw.clone()) {
            return parsed;
        }
    }

    if let Some(usage) = raw.get("usage") {
        return ParsedAgentEvent::Usage {
            input_tokens: u64_at(usage, &["input_tokens"]),
            output_tokens: u64_at(usage, &["output_tokens"]),
            cache_creation_input_tokens: u64_at(usage, &["cache_creation_input_tokens"]),
            cache_read_input_tokens: u64_at(usage, &["cache_read_input_tokens"]),
            raw,
        };
    }

    if looks_like_permission(&raw) {
        return ParsedAgentEvent::PermissionRequest {
            tool_name: first_string_field(&raw, &["tool_name", "tool", "name"]),
            prompt: first_string_field(&raw, &["prompt", "message", "question", "reason"]),
            raw,
        };
    }

    if looks_like_user_question(&raw) {
        return ParsedAgentEvent::UserQuestion {
            prompt: first_string_field(&raw, &["prompt", "message", "question"]),
            raw,
        };
    }

    if looks_like_hook(&raw) {
        return ParsedAgentEvent::HookEvent {
            name: first_string_field(
                &raw,
                &["hook_event_name", "hook_name", "event_name", "name"],
            ),
            raw,
        };
    }

    ParsedAgentEvent::Raw(raw)
}

fn parse_stream_event(raw: Value) -> Option<ParsedAgentEvent> {
    let event = raw.get("event")?;
    match string_at(event, &["type"])? {
        "content_block_delta" => {
            let delta = event.get("delta")?;
            match string_at(delta, &["type"])? {
                "text_delta" => Some(ParsedAgentEvent::TextDelta {
                    text: string_at(delta, &["text"]).unwrap_or_default().to_string(),
                    raw,
                }),
                "input_json_delta" => Some(ParsedAgentEvent::ToolUseInputDelta {
                    index: u64_at(event, &["index"]),
                    partial_json: string_at(delta, &["partial_json"])
                        .unwrap_or_default()
                        .to_string(),
                    raw,
                }),
                _ => None,
            }
        }
        "content_block_start" => {
            let block = event.get("content_block")?;
            if string_at(block, &["type"]) == Some("tool_use") {
                Some(ParsedAgentEvent::ToolUseStart {
                    index: u64_at(event, &["index"]),
                    id: string_at(block, &["id"]).map(str::to_string),
                    name: string_at(block, &["name"]).map(str::to_string),
                    input: block.get("input").cloned(),
                    raw,
                })
            } else {
                None
            }
        }
        "content_block_stop" => Some(ParsedAgentEvent::ToolUseStop {
            index: u64_at(event, &["index"]),
            raw,
        }),
        "message_delta" => {
            let usage = event.get("usage")?;
            let input_tokens = u64_at(usage, &["input_tokens"]);
            let output_tokens = u64_at(usage, &["output_tokens"]);
            let cache_creation_input_tokens = u64_at(usage, &["cache_creation_input_tokens"]);
            let cache_read_input_tokens = u64_at(usage, &["cache_read_input_tokens"]);
            Some(ParsedAgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                raw,
            })
        }
        _ => None,
    }
}

fn message_text(raw: &Value) -> Option<String> {
    let content = raw.get("message")?.get("content")?.as_array()?;
    let mut text = String::new();
    for block in content {
        if string_at(block, &["type"]) == Some("text") {
            if let Some(part) = string_at(block, &["text"]) {
                text.push_str(part);
            }
        }
    }
    if text.is_empty() { None } else { Some(text) }
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

fn u64_at(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64()
}

fn looks_like_permission(value: &Value) -> bool {
    contains_keyword(value, "permission") || contains_keyword(value, "approval")
}

fn looks_like_user_question(value: &Value) -> bool {
    contains_keyword(value, "user_question")
        || contains_keyword(value, "ask_user")
        || contains_keyword(value, "question")
}

fn looks_like_hook(value: &Value) -> bool {
    contains_keyword(value, "hook")
}

fn contains_keyword(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(s) => s.to_ascii_lowercase().contains(needle),
        Value::Array(items) => items.iter().any(|item| contains_keyword(item, needle)),
        Value::Object(map) => map.iter().any(|(key, value)| {
            key.to_ascii_lowercase().contains(needle) || contains_keyword(value, needle)
        }),
        _ => false,
    }
}

fn first_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(s) = map.get(*key).and_then(Value::as_str) {
                    return Some(s.to_string());
                }
            }
            for child in map.values() {
                if let Some(found) = first_string_field(child, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|item| first_string_field(item, keys)),
        _ => None,
    }
}

fn spawn_claude_command(config: &ClaudeStreamConfig) -> Result<(Child, ChildStdin, ChildStdout)> {
    let argv = config.argv();
    let (program, args) = argv
        .split_first()
        .ctx("Claude argv must contain a program")?;

    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::inherit());
    command.kill_on_drop(true);

    let mut child = command.spawn().ctx("spawn Claude stream child")?;
    let stdin = child.stdin.take().ctx("Claude child stdin unavailable")?;
    let stdout = child.stdout.take().ctx("Claude child stdout unavailable")?;
    Ok((child, stdin, stdout))
}

/// Split, owned async I/O for one structured agent run, decoupled from
/// the concrete child process. The production spawner wires these to a
/// real subprocess; tests inject in-memory readers/writers so they
/// never launch `claude` or a shell (CONTRIBUTING rule #5).
pub struct AgentStreamIo {
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    /// Resolves to the child's exit code once it terminates. The driver
    /// awaits this only after stdout reaches EOF.
    pub wait: Pin<Box<dyn Future<Output = Option<i32>> + Send>>,
}

/// Factory for the underlying process of a structured stream-json agent
/// run. The server obtains a run's I/O through this seam instead of
/// hard-coding a subprocess spawn, so tests mock at this boundary
/// rather than executing a real program.
pub trait AgentStreamSpawner: Send + Sync + 'static {
    fn spawn<'a>(
        &'a self,
        config: ClaudeStreamConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentStreamIo>> + Send + 'a>>;
}

/// Default spawner: launches the configured program as a real child
/// process and exposes its stdio.
pub struct ProcessAgentStreamSpawner;

impl AgentStreamSpawner for ProcessAgentStreamSpawner {
    fn spawn<'a>(
        &'a self,
        config: ClaudeStreamConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentStreamIo>> + Send + 'a>> {
        Box::pin(async move {
            let (mut child, stdin, stdout) = spawn_claude_command(&config)?;
            Ok(AgentStreamIo {
                stdin: Box::pin(stdin),
                stdout: Box::pin(stdout),
                wait: Box::pin(
                    async move { child.wait().await.ok().and_then(|status| status.code()) },
                ),
            })
        })
    }
}

pub fn user_text_value(text: impl Into<String>) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": text.into(),
                }
            ],
        },
    })
}

impl ParsedAgentEvent {
    pub fn raw(&self) -> &Value {
        match self {
            ParsedAgentEvent::SessionInit { raw, .. }
            | ParsedAgentEvent::UserMessage { raw, .. }
            | ParsedAgentEvent::TextDelta { raw, .. }
            | ParsedAgentEvent::ToolUseStart { raw, .. }
            | ParsedAgentEvent::ToolUseInputDelta { raw, .. }
            | ParsedAgentEvent::ToolUseStop { raw, .. }
            | ParsedAgentEvent::Usage { raw, .. }
            | ParsedAgentEvent::Result { raw, .. }
            | ParsedAgentEvent::PermissionRequest { raw, .. }
            | ParsedAgentEvent::UserQuestion { raw, .. }
            | ParsedAgentEvent::HookEvent { raw, .. }
            | ParsedAgentEvent::Raw(raw) => raw,
        }
    }
}
