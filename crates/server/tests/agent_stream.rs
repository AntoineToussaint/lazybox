// Originally pulled the module in via `#[path]` to test it without
// the rest of the server crate. After the anyhow → thiserror
// migration the module references `crate::ServerError` /
// `crate::ResultExt`, which only resolve when compiled as part of
// lazybox-server. Import via the public re-export instead;
// `agent_stream` is already `pub mod` in lib.rs.
use lazybox_server::agent_stream::{
    ClaudeStreamConfig, ParsedAgentEvent, encode_user_text_jsonl, parse_jsonl_line, user_text_value,
};
use serde_json::json;
use std::path::PathBuf;

#[test]
fn builds_required_claude_stream_json_argv() {
    let argv = ClaudeStreamConfig::default().argv();

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
    let config = ClaudeStreamConfig {
        cwd: Some(PathBuf::from("/tmp/worktree")),
        resume_session_id: Some("session-123".to_string()),
        extra_args: vec!["--model".to_string(), "sonnet".to_string()],
        ..ClaudeStreamConfig::default()
    };

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
