use lazybox_agents::{Agent, SpawnCtx};
use lazybox_ipc::{AgentInputMessage, AgentRunId, AgentRuntimeMode, Command, Event, channel};
use lazybox_server::ServerError;
use lazybox_server::agent_stream::{AgentStreamIo, AgentStreamSpawner, ClaudeStreamConfig};
use lazybox_server::{Server, ServerConfig};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct FakeStreamAgent;

impl Agent for FakeStreamAgent {
    fn id(&self) -> &'static str {
        "fake-stream"
    }

    fn display_name(&self) -> &'static str {
        "Fake Stream"
    }

    fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
        vec!["fake-claude".into()]
    }
}

/// The canned stream-json the fake process emits once an input line
/// arrives: a session init, an assistant text delta, a full tool-use
/// cycle, and a success result carrying usage.
const FAKE_STREAM_SCRIPT: &str = concat!(
    r#"{"type":"system","subtype":"init","session_id":"fake-session"}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_fake","name":"Bash","input":{}}}}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"echo ok\"}"}}}"#,
    "\n",
    r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
    "\n",
    r#"{"type":"result","subtype":"success","session_id":"fake-session","result":"done","usage":{"input_tokens":1,"output_tokens":2}}"#,
    "\n",
);

/// Mocks the structured-run process at the [`AgentStreamSpawner`]
/// boundary (CONTRIBUTING rule #5): in-memory pipes, no real `claude`
/// or shell. Waits for one input line — proving `SendAgentInput`
/// reaches the process — then emits the canned stream and closes.
struct FakeStreamSpawner {
    script: &'static str,
}

impl AgentStreamSpawner for FakeStreamSpawner {
    fn spawn<'a>(
        &'a self,
        _config: ClaudeStreamConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentStreamIo, ServerError>> + Send + 'a>> {
        let script = self.script;
        Box::pin(async move {
            let (driver_stdin, fake_in) = tokio::io::duplex(4096);
            let (mut fake_out, driver_stdout) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut input = BufReader::new(fake_in).lines();
                let _ = input.next_line().await;
                let _ = fake_out.write_all(script.as_bytes()).await;
                // Dropping `fake_out` here closes the driver's stdout (EOF).
            });
            Ok(AgentStreamIo {
                stdin: Box::pin(driver_stdin),
                stdout: Box::pin(driver_stdout),
                wait: Box::pin(async { Some(0) }),
            })
        })
    }
}

#[tokio::test]
async fn stream_json_agent_run_emits_normalized_events_until_process_exit() {
    let mut config = ServerConfig::in_memory();
    config.agents.register(Arc::new(FakeStreamAgent));
    config.agent_stream_spawner = Arc::new(FakeStreamSpawner {
        script: FAKE_STREAM_SCRIPT,
    });

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
            cwd: None,
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
