use lazybox_agents::{Agent, SpawnCtx};
use lazybox_ipc::{AgentInputMessage, AgentRunId, AgentRuntimeMode, Command, Event, channel};
use lazybox_server::{Server, ServerConfig};
use std::path::PathBuf;
use std::sync::Arc;

struct FakeStreamAgent {
    program: PathBuf,
}

impl Agent for FakeStreamAgent {
    fn id(&self) -> &'static str {
        "fake-stream"
    }

    fn display_name(&self) -> &'static str {
        "Fake Stream"
    }

    fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
        vec![self.program.display().to_string()]
    }
}

#[cfg(unix)]
fn make_fake_claude_script(dir: &std::path::Path) -> PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("fake-claude-stream.sh");
    let mut file = std::fs::File::create(&script).unwrap();
    writeln!(
        file,
        r#"#!/bin/sh
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"fake-session"}}'
if IFS= read -r _line; then
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"hello"}}}}}}'
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_start","index":1,"content_block":{{"type":"tool_use","id":"toolu_fake","name":"Bash","input":{{}}}}}}}}'
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","index":1,"delta":{{"type":"input_json_delta","partial_json":"{{\"command\":\"echo ok\"}}"}}}}}}'
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_stop","index":1}}}}'
  printf '%s\n' '{{"type":"result","subtype":"success","session_id":"fake-session","result":"done","usage":{{"input_tokens":1,"output_tokens":2}}}}'
fi
"#
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

#[cfg(unix)]
#[tokio::test]
async fn stream_json_agent_run_emits_normalized_events_until_process_exit() {
    let temp = tempfile::tempdir().unwrap();
    let program = make_fake_claude_script(temp.path());
    let mut config = ServerConfig::in_memory();
    config
        .agents
        .register(Arc::new(FakeStreamAgent { program }));

    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(config).serve(server).await.unwrap();
    });

    client.send(Command::Subscribe).unwrap();
    assert!(matches!(
        client.recv().await.expect("snapshot"),
        Event::Snapshot { .. }
    ));

    client
        .send(Command::StartAgentRun {
            session_key: "test:stream".into(),
            session_id: None,
            agent: "fake-stream".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: Some(temp.path().display().to_string()),
            initial_input: None,
        })
        .unwrap();

    let run_id = wait_for_started(&mut client).await;
    client
        .send(Command::SendAgentInput {
            run_id,
            message: AgentInputMessage {
                text: Some("review this".into()),
                json: None,
            },
        })
        .unwrap();

    let mut saw_text = false;
    let mut saw_tool_start = false;
    let mut saw_tool_delta = false;
    let mut saw_tool_finished = false;
    let mut saw_usage = false;
    let mut saw_turn_finished = false;

    loop {
        let event = recv_agent_event(&mut client).await;
        match event {
            Event::AgentRunStarted { .. } => panic!("duplicate AgentRunStarted"),
            Event::AgentAssistantTextDelta { delta, .. } => {
                assert_eq!(delta, "hello");
                saw_text = true;
            }
            Event::AgentToolCallStarted { call_id, name, .. } => {
                assert_eq!(call_id, "toolu_fake");
                assert_eq!(name, "Bash");
                saw_tool_start = true;
            }
            Event::AgentToolCallDelta {
                call_id,
                delta_json,
                ..
            } => {
                assert_eq!(call_id, "toolu_fake");
                assert_eq!(delta_json, r#"{"command":"echo ok"}"#);
                saw_tool_delta = true;
            }
            Event::AgentToolCallFinished { call_id, .. } => {
                assert_eq!(call_id, "toolu_fake");
                saw_tool_finished = true;
            }
            Event::AgentUsage { usage, .. } => {
                if usage.input_tokens == Some(1) && usage.output_tokens == Some(2) {
                    saw_usage = true;
                }
            }
            Event::AgentTurnFinished {
                result,
                session_id,
                error,
                ..
            } => {
                assert_eq!(result.as_deref(), Some("done"));
                assert_eq!(session_id.as_deref(), Some("fake-session"));
                assert!(error.is_none());
                saw_turn_finished = true;
            }
            Event::AgentRunFinished {
                exit_code, error, ..
            } => {
                assert_eq!(exit_code, Some(0));
                assert!(error.is_none());
                break;
            }
            Event::AgentRawJson { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }

    assert!(saw_text);
    assert!(saw_tool_start);
    assert!(saw_tool_delta);
    assert!(saw_tool_finished);
    assert!(saw_usage);
    assert!(saw_turn_finished);
}
async fn wait_for_started(client: &mut lazybox_ipc::Client) -> AgentRunId {
    loop {
        match recv_agent_event(client).await {
            Event::AgentRunStarted {
                run_id,
                agent,
                mode,
                ..
            } => {
                assert_eq!(agent, "fake-stream");
                assert_eq!(mode, AgentRuntimeMode::StreamJson);
                return run_id;
            }
            Event::AgentRawJson { .. } => {}
            other => panic!("expected AgentRunStarted, got {other:?}"),
        }
    }
}

async fn recv_agent_event(client: &mut lazybox_ipc::Client) -> Event {
    tokio::time::timeout(std::time::Duration::from_secs(10), client.recv())
        .await
        .expect("agent run event")
        .expect("event")
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_mode_agent_run_reports_that_spawn_should_be_used() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client
        .send(Command::StartAgentRun {
            session_key: "test:terminal".into(),
            session_id: None,
            agent: "claude".into(),
            mode: AgentRuntimeMode::Terminal,
            cwd: None,
            initial_input: None,
        })
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("event");
    match event {
        Event::ProviderError { message, .. } => {
            assert!(message.contains("use Spawn for terminal mode"));
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}
