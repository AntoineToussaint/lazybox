// Originally pulled the module in via `#[path]` to test it without
// the rest of the server crate. After the anyhow → thiserror
// migration the module references `crate::ServerError` /
// `crate::ResultExt`, which only resolve when compiled as part of
// lazybox-server. Import via the public re-export instead;
// `agent_stream` is already `pub mod` in lib.rs.
use lazybox_agents::StructuredAgentProtocol;
use lazybox_ipc::AgentRunAccess;
use lazybox_server::agent_stream::{
    AgentStreamConfig, ParsedAgentEvent, encode_user_text_jsonl, parse_agent_jsonl_line,
    parse_jsonl_line, user_text_value,
};
use serde_json::json;
use std::path::PathBuf;

#[test]
fn builds_required_claude_stream_json_argv() {
    let argv = AgentStreamConfig::new(StructuredAgentProtocol::ClaudeStreamJson, "claude").argv();

    assert_eq!(
        argv,
        vec![
            "claude",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--include-hook-events",
            "--replay-user-messages",
        ]
    );
}

#[test]
fn builds_resume_and_extra_args_without_encoding_cwd_as_argv() {
    let mut config = AgentStreamConfig::new(StructuredAgentProtocol::ClaudeStreamJson, "claude");
    config.cwd = Some(PathBuf::from("/tmp/worktree"));
    config.resume_session_id = Some("session-123".to_string());
    config.extra_args = vec!["--model".to_string(), "sonnet".to_string()];

    assert_eq!(
        config.argv(),
        vec![
            "claude",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--include-hook-events",
            "--replay-user-messages",
            "--resume",
            "session-123",
            "--model",
            "sonnet",
        ]
    );
}

#[test]
fn builds_continue_latest_argv_for_both_structured_providers() {
    let mut claude = AgentStreamConfig::new(StructuredAgentProtocol::ClaudeStreamJson, "claude");
    claude.continue_latest = true;
    let claude_argv = claude.argv();
    assert!(claude_argv.iter().any(|arg| arg == "--continue"));
    assert!(!claude_argv.iter().any(|arg| arg == "--resume"));

    let mut codex = AgentStreamConfig::new(StructuredAgentProtocol::CodexExecJson, "codex");
    codex.continue_latest = true;
    assert_eq!(
        codex.argv(),
        vec![
            "codex",
            "exec",
            "resume",
            "--json",
            "--skip-git-repo-check",
            "--last",
            "-",
        ]
    );
}

#[test]
fn read_only_claude_run_disables_ambient_and_builtin_tools() {
    let mut config = AgentStreamConfig::new(StructuredAgentProtocol::ClaudeStreamJson, "claude");
    config.access = AgentRunAccess::ReadOnly;

    let argv = config.argv();

    assert!(argv.windows(2).any(|args| args == ["--tools", ""]));
    assert!(
        argv.windows(2)
            .any(|args| args == ["--permission-mode", "dontAsk"])
    );
    assert!(argv.iter().any(|arg| arg == "--safe-mode"));
    assert!(argv.iter().any(|arg| arg == "--strict-mcp-config"));
}

#[test]
fn builds_codex_initial_and_resume_argv() {
    let initial = AgentStreamConfig::new(StructuredAgentProtocol::CodexExecJson, "codex");
    assert_eq!(
        initial.argv(),
        vec![
            "codex",
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "-",
        ]
    );

    let mut resume = initial;
    resume.resume_session_id = Some("thread-123".into());
    assert_eq!(
        resume.argv(),
        vec![
            "codex",
            "exec",
            "resume",
            "--json",
            "--skip-git-repo-check",
            "thread-123",
            "-",
        ]
    );
}

#[test]
fn read_only_codex_run_ignores_ambient_extensions() {
    let mut config = AgentStreamConfig::new(StructuredAgentProtocol::CodexExecJson, "codex");
    config.access = AgentRunAccess::ReadOnly;

    let initial = config.argv();
    for flag in ["--ignore-user-config", "--ignore-rules", "--sandbox"] {
        assert!(initial.iter().any(|arg| arg == flag), "missing {flag}");
    }
    assert!(
        initial
            .windows(2)
            .any(|args| args == ["--sandbox", "read-only"])
    );

    config.resume_session_id = Some("thread-123".into());
    let resume = config.argv();
    for flag in ["--ignore-user-config", "--ignore-rules"] {
        assert!(resume.iter().any(|arg| arg == flag), "missing {flag}");
    }
    for override_value in [
        "mcp_servers={}",
        "hooks={}",
        "sandbox_mode=\"read-only\"",
        "approval_policy=\"never\"",
    ] {
        assert!(
            resume.windows(2).any(|args| args == ["-c", override_value]),
            "missing read-only resume override {override_value}: {resume:?}"
        );
    }
    assert!(
        !resume
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
    );
}

#[test]
fn encodes_text_user_message_as_jsonl() {
    let encoded = encode_user_text_jsonl("Explain this code").unwrap();

    assert!(encoded.ends_with('\n'));
    let value: serde_json::Value = serde_json::from_str(encoded.trim_end()).unwrap();
    assert_eq!(value, user_text_value("Explain this code"));
}

#[test]
fn parses_system_init_session_id() {
    let event =
        parse_jsonl_line(r#"{"type":"system","subtype":"init","session_id":"abc","cwd":"/repo"}"#)
            .unwrap();

    assert_eq!(
        event,
        ParsedAgentEvent::SessionInit {
            session_id: Some("abc".to_string()),
            raw: json!({
                "type": "system",
                "subtype": "init",
                "session_id": "abc",
                "cwd": "/repo",
            }),
        }
    );
}

#[test]
fn parses_replayed_user_text_message() {
    let event = parse_jsonl_line(
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi "},{"type":"text","text":"there"}]}}"#,
    )
    .unwrap();

    match event {
        ParsedAgentEvent::UserMessage { text, .. } => {
            assert_eq!(text.as_deref(), Some("hi there"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn parses_text_delta_stream_event() {
    let event = parse_jsonl_line(
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}}"#,
    )
    .unwrap();

    match event {
        ParsedAgentEvent::TextDelta { text, .. } => assert_eq!(text, "hello"),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn parses_tool_use_start_input_delta_and_stop() {
    let start = parse_jsonl_line(
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}}"#,
    )
    .unwrap();
    let delta = parse_jsonl_line(
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"cargo test\"}"}}}"#,
    )
    .unwrap();
    let stop = parse_jsonl_line(
        r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
    )
    .unwrap();

    match start {
        ParsedAgentEvent::ToolUseStart {
            index, id, name, ..
        } => {
            assert_eq!(index, Some(1));
            assert_eq!(id.as_deref(), Some("toolu_1"));
            assert_eq!(name.as_deref(), Some("Bash"));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    match delta {
        ParsedAgentEvent::ToolUseInputDelta {
            index,
            partial_json,
            ..
        } => {
            assert_eq!(index, Some(1));
            assert_eq!(partial_json, r#"{"command":"cargo test"}"#);
        }
        other => panic!("unexpected event: {other:?}"),
    }

    match stop {
        ParsedAgentEvent::ToolUseStop { index, .. } => assert_eq!(index, Some(1)),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn parses_usage_and_result_events() {
    let usage = parse_jsonl_line(
        r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"input_tokens":12,"output_tokens":34,"cache_creation_input_tokens":5,"cache_read_input_tokens":6}}}"#,
    )
    .unwrap();
    let result = parse_jsonl_line(
        r#"{"type":"result","subtype":"success","session_id":"abc","result":"done","usage":{"input_tokens":1}}"#,
    )
    .unwrap();

    match usage {
        ParsedAgentEvent::Usage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            ..
        } => {
            assert_eq!(input_tokens, Some(12));
            assert_eq!(output_tokens, Some(34));
            assert_eq!(cache_creation_input_tokens, Some(5));
            assert_eq!(cache_read_input_tokens, Some(6));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    match result {
        ParsedAgentEvent::Result {
            result,
            session_id,
            usage,
            ..
        } => {
            assert_eq!(result.as_deref(), Some("done"));
            assert_eq!(session_id.as_deref(), Some("abc"));
            assert_eq!(usage, Some(json!({"input_tokens": 1})));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn parses_permission_question_hook_and_unknown_fallbacks() {
    let permission = parse_jsonl_line(
        r#"{"type":"permission_request","tool_name":"Bash","prompt":"Allow command?"}"#,
    )
    .unwrap();
    let question =
        parse_jsonl_line(r#"{"type":"user_question","question":"Which branch?"}"#).unwrap();
    let hook =
        parse_jsonl_line(r#"{"type":"hook_event","hook_event_name":"SessionStart"}"#).unwrap();
    let unknown = parse_jsonl_line(r#"{"type":"new_future_event","value":1}"#).unwrap();

    match permission {
        ParsedAgentEvent::PermissionRequest {
            tool_name, prompt, ..
        } => {
            assert_eq!(tool_name.as_deref(), Some("Bash"));
            assert_eq!(prompt.as_deref(), Some("Allow command?"));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    match question {
        ParsedAgentEvent::UserQuestion { prompt, .. } => {
            assert_eq!(prompt.as_deref(), Some("Which branch?"));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    match hook {
        ParsedAgentEvent::HookEvent { name, .. } => {
            assert_eq!(name.as_deref(), Some("SessionStart"));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    assert!(matches!(unknown, ParsedAgentEvent::Raw(_)));
}

#[test]
fn parses_codex_thread_message_tool_and_completion() {
    let protocol = StructuredAgentProtocol::CodexExecJson;
    let thread = parse_agent_jsonl_line(
        protocol,
        r#"{"type":"thread.started","thread_id":"thread-1"}"#,
    )
    .unwrap();
    let tool_start = parse_agent_jsonl_line(
        protocol,
        r#"{"type":"item.started","item":{"id":"item-1","type":"command_execution","command":"pwd","status":"in_progress"}}"#,
    )
    .unwrap();
    let tool_stop = parse_agent_jsonl_line(
        protocol,
        r#"{"type":"item.completed","item":{"id":"item-1","type":"command_execution","command":"pwd","aggregated_output":"/tmp\n","exit_code":0,"status":"completed"}}"#,
    )
    .unwrap();
    let message = parse_agent_jsonl_line(
        protocol,
        r#"{"type":"item.completed","item":{"id":"item-2","type":"agent_message","text":"Press `v`."}}"#,
    )
    .unwrap();
    let completed = parse_agent_jsonl_line(
        protocol,
        r#"{"type":"turn.completed","usage":{"input_tokens":12,"cached_input_tokens":3,"output_tokens":4}}"#,
    )
    .unwrap();

    assert!(matches!(
        thread,
        ParsedAgentEvent::SessionInit { session_id: Some(id), .. } if id == "thread-1"
    ));
    assert!(matches!(
        tool_start,
        ParsedAgentEvent::ToolUseStart { id: Some(id), name: Some(name), .. }
            if id == "item-1" && name == "command_execution"
    ));
    assert!(matches!(
        tool_stop,
        ParsedAgentEvent::ToolUseStop { id: Some(id), error: None, .. } if id == "item-1"
    ));
    assert!(matches!(
        message,
        ParsedAgentEvent::TextDelta { text, .. } if text == "Press `v`."
    ));
    assert!(matches!(
        completed,
        ParsedAgentEvent::Result {
            result: None,
            usage: Some(_),
            ..
        }
    ));
}

#[test]
fn parses_codex_failures_without_losing_error_text() {
    let failed = parse_agent_jsonl_line(
        StructuredAgentProtocol::CodexExecJson,
        r#"{"type":"turn.failed","error":{"message":"model unavailable"}}"#,
    )
    .unwrap();
    assert!(matches!(
        failed,
        ParsedAgentEvent::Result { result: None, .. }
    ));

    let failed_tool = parse_agent_jsonl_line(
        StructuredAgentProtocol::CodexExecJson,
        r#"{"type":"item.completed","item":{"id":"item-3","type":"mcp_tool_call","server":"lazybox","tool":"broadcast","status":"failed","error":{"message":"not authorized"}}}"#,
    )
    .unwrap();
    assert!(matches!(
        failed_tool,
        ParsedAgentEvent::ToolUseStop { id: Some(id), error: Some(error), .. }
            if id == "item-3" && error == "not authorized"
    ));
}
