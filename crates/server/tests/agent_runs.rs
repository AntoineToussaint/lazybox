use lazybox_agents::{Agent, SpawnCtx, StructuredAgentProtocol};
use lazybox_ipc::{AgentInputMessage, AgentRunId, AgentRuntimeMode, Command, Event, channel};
use lazybox_server::ServerError;
use lazybox_server::agent_runs::{
    AGENT_INPUT_CHANNEL_CAPACITY, AgentRunHandle, handle_interrupt_agent_run,
    handle_send_agent_input,
};
use lazybox_server::agent_stream::{AgentStreamConfig, AgentStreamIo, AgentStreamSpawner};
use lazybox_server::{Server, ServerConfig};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct FakeStreamAgent;

#[tokio::test]
async fn structured_agent_input_backlog_is_bounded_and_retryable() {
    let config = ServerConfig::in_memory();
    let run_id = AgentRunId(991);
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel(AGENT_INPUT_CHANNEL_CAPACITY);
    for index in 0..AGENT_INPUT_CHANNEL_CAPACITY {
        input_tx
            .try_send(AgentInputMessage {
                text: Some(format!("queued-{index}")),
                json: None,
            })
            .expect("declared queue capacity");
    }
    let task = tokio::spawn(std::future::pending::<()>());
    config
        .agent_runs
        .lock()
        .await
        .insert(run_id, AgentRunHandle { input_tx, task });
    let mut events = config.bus.subscribe();

    handle_send_agent_input(
        &config,
        run_id,
        AgentInputMessage {
            text: Some("must-not-grow-memory".into()),
            json: None,
        },
    )
    .await;

    assert!(matches!(
        events.try_recv().expect("overload event"),
        Event::ProviderError {
            source,
            kind,
            message,
            ..
        } if source == "agent_run:input"
            && kind == "retryable"
            && message.contains("queue is full")
            && message.contains("retry")
    ));
    handle_interrupt_agent_run(&config, run_id).await;
    assert!(config.agent_runs.lock().await.is_empty());
}

impl Agent for FakeStreamAgent {
    fn id(&self) -> &'static str {
        "fake-stream"
    }

    fn display_name(&self) -> &'static str {
        "Fake Stream"
    }

    fn structured_protocol(&self) -> Option<StructuredAgentProtocol> {
        Some(StructuredAgentProtocol::ClaudeStreamJson)
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
        _config: AgentStreamConfig,
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
            access: lazybox_ipc::AgentRunAccess::Default,
        })
        .unwrap();

    let run_id = wait_for_started(&mut client, "fake-stream").await;
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
async fn wait_for_started(client: &mut lazybox_ipc::Client, expected_agent: &str) -> AgentRunId {
    loop {
        match recv_agent_event(client).await {
            Event::AgentRunStarted {
                run_id,
                agent,
                mode,
                ..
            } => {
                assert_eq!(agent, expected_agent);
                assert_eq!(mode, AgentRuntimeMode::StreamJson);
                return run_id;
            }
            Event::AgentRawJson { .. } => {}
            // Post-subscribe scaffolding (tracker #512): the auto-fix
            // policy config lands after the snapshot, before the run.
            Event::AutoFixPolicyConfig { .. } => {}
            Event::ShellCommandConfig { .. } => {}
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

struct FakeCodexAgent;

impl Agent for FakeCodexAgent {
    fn id(&self) -> &'static str {
        "fake-codex"
    }

    fn display_name(&self) -> &'static str {
        "Fake Codex"
    }

    fn structured_protocol(&self) -> Option<StructuredAgentProtocol> {
        Some(StructuredAgentProtocol::CodexExecJson)
    }

    fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
        vec!["fake-codex".into()]
    }
}

const FAKE_CODEX_FIRST_TURN: &str = concat!(
    r#"{"type":"thread.started","thread_id":"thread-help"}"#,
    "\n",
    r#"{"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"first answer"}}"#,
    "\n",
    r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2}}"#,
    "\n",
);

const FAKE_CODEX_SECOND_TURN: &str = concat!(
    r#"{"type":"item.completed","item":{"id":"message-2","type":"agent_message","text":"follow-up answer"}}"#,
    "\n",
    r#"{"type":"turn.completed","usage":{"input_tokens":12,"cached_input_tokens":8,"output_tokens":3}}"#,
    "\n",
);

/// Codex exits after every turn, so this spawner supplies two distinct
/// in-memory children and records the resume config and raw prompts.
struct FakeCodexSpawner {
    calls: AtomicUsize,
    configs: Arc<Mutex<Vec<AgentStreamConfig>>>,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl AgentStreamSpawner for FakeCodexSpawner {
    fn spawn<'a>(
        &'a self,
        config: AgentStreamConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentStreamIo, ServerError>> + Send + 'a>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.configs.lock().unwrap().push(config);
        let prompts = self.prompts.clone();
        Box::pin(async move {
            let (driver_stdin, mut fake_in) = tokio::io::duplex(4096);
            let (mut fake_out, driver_stdout) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut input = Vec::new();
                let _ = fake_in.read_to_end(&mut input).await;
                prompts
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&input).into_owned());
                let script = if call == 0 {
                    FAKE_CODEX_FIRST_TURN
                } else {
                    FAKE_CODEX_SECOND_TURN
                };
                let _ = fake_out.write_all(script.as_bytes()).await;
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
async fn codex_turn_processes_resume_as_one_logical_run() {
    let configs = Arc::new(Mutex::new(Vec::new()));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let mut config = ServerConfig::in_memory();
    config.agents.register(Arc::new(FakeCodexAgent));
    config.agent_stream_spawner = Arc::new(FakeCodexSpawner {
        calls: AtomicUsize::new(0),
        configs: configs.clone(),
        prompts: prompts.clone(),
    });

    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(config).serve(server).await.unwrap();
    });

    client
        .send(Command::StartAgentRun {
            session_key: "lazybox:help".into(),
            session_id: None,
            agent: "fake-codex".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: Some(AgentInputMessage {
                text: Some("first question".into()),
                json: None,
            }),
            access: lazybox_ipc::AgentRunAccess::Default,
        })
        .unwrap();
    let run_id = wait_for_started(&mut client, "fake-codex").await;
    assert_turn_answer(&mut client, run_id, "first answer").await;

    // The first child has exited, but lazybox keeps the logical run id
    // alive and resumes the captured Codex thread for this input.
    client
        .send(Command::SendAgentInput {
            run_id,
            message: AgentInputMessage {
                text: Some("follow-up question".into()),
                json: None,
            },
        })
        .unwrap();
    assert_turn_answer(&mut client, run_id, "follow-up answer").await;

    {
        let configs = configs.lock().unwrap();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].protocol, StructuredAgentProtocol::CodexExecJson);
        assert_eq!(configs[0].resume_session_id, None);
        assert_eq!(configs[1].resume_session_id.as_deref(), Some("thread-help"));
    }
    assert_eq!(
        prompts.lock().unwrap().as_slice(),
        ["first question", "follow-up question"]
    );

    client.send(Command::InterruptAgentRun { run_id }).unwrap();
    assert!(matches!(
        recv_agent_event(&mut client).await,
        Event::AgentRunFinished { run_id: id, .. } if id == run_id
    ));
}

async fn assert_turn_answer(
    client: &mut lazybox_ipc::Client,
    expected_run_id: AgentRunId,
    expected_answer: &str,
) {
    let mut answer = String::new();
    loop {
        match recv_agent_event(client).await {
            Event::AgentAssistantTextDelta { run_id, delta } => {
                assert_eq!(run_id, expected_run_id);
                answer.push_str(&delta);
            }
            Event::AgentTurnFinished {
                run_id,
                result,
                session_id,
                error,
                ..
            } => {
                assert_eq!(run_id, expected_run_id);
                assert!(result.is_none());
                assert_eq!(session_id.as_deref(), Some("thread-help"));
                assert!(error.is_none());
                assert_eq!(answer, expected_answer);
                return;
            }
            Event::AgentRawJson { .. } | Event::AgentUsage { .. } => {}
            Event::AgentRunFinished { .. } => {
                panic!("Codex logical run ended between turns")
            }
            other => panic!("unexpected Codex event: {other:?}"),
        }
    }
}

/// Captures the `AgentStreamConfig` the server builds, then behaves
/// like a process whose stdout is immediately at EOF.
struct CapturingSpawner {
    captured: Arc<std::sync::Mutex<Option<AgentStreamConfig>>>,
}

impl AgentStreamSpawner for CapturingSpawner {
    fn spawn<'a>(
        &'a self,
        config: AgentStreamConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentStreamIo, ServerError>> + Send + 'a>> {
        *self.captured.lock().unwrap() = Some(config);
        Box::pin(async move {
            Ok(AgentStreamIo {
                stdin: Box::pin(tokio::io::sink()),
                stdout: Box::pin(tokio::io::empty()),
                wait: Box::pin(async { Some(0) }),
            })
        })
    }
}

/// A run whose session key matches no workspace (e.g. the help
/// assistant's sentinel) and carries no explicit cwd gets a neutral
/// daemon-side cwd — never the daemon's own working directory, where a
/// stray CLAUDE.md would leak into the run's context, and never a
/// client-supplied path, which may not exist on the daemon host.
#[tokio::test]
async fn workspace_less_run_resolves_to_neutral_cwd() {
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut config = ServerConfig::in_memory();
    config.agents.register(Arc::new(FakeStreamAgent));
    config.agent_stream_spawner = Arc::new(CapturingSpawner {
        captured: captured.clone(),
    });

    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(config).serve(server).await.unwrap();
    });

    client
        .send(Command::StartAgentRun {
            session_key: "lazybox:help".into(),
            session_id: None,
            agent: "fake-stream".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: None,
            access: lazybox_ipc::AgentRunAccess::ReadOnly,
        })
        .unwrap();
    wait_for_started(&mut client, "fake-stream").await;

    let config = captured.lock().unwrap().take().expect("spawner invoked");
    assert_eq!(config.cwd.as_deref(), Some(std::env::temp_dir().as_path()));
    assert_eq!(config.access, lazybox_ipc::AgentRunAccess::ReadOnly);
    // FakeStreamAgent has no LLM provider, so no gateway env applies —
    // regardless of the host's YAML.
    assert!(config.env.is_empty());
}

/// stdout dying mid-run — always an error before this fix's coverage.
struct FailingReader;

impl tokio::io::AsyncRead for FailingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("stdout torn")))
    }
}

struct FailingStreamSpawner;

impl AgentStreamSpawner for FailingStreamSpawner {
    fn spawn<'a>(
        &'a self,
        _config: AgentStreamConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentStreamIo, ServerError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(AgentStreamIo {
                stdin: Box::pin(tokio::io::sink()),
                stdout: Box::pin(FailingReader),
                wait: Box::pin(async { None }),
            })
        })
    }
}

/// A stdout read error is a run death and must produce the run-scoped
/// terminal event: without `AgentRunFinished`, every client holding the
/// run id keeps writing questions into a dead process forever.
#[tokio::test]
async fn stdout_error_emits_run_finished_with_error() {
    let mut config = ServerConfig::in_memory();
    config.agents.register(Arc::new(FakeStreamAgent));
    config.agent_stream_spawner = Arc::new(FailingStreamSpawner);

    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(config).serve(server).await.unwrap();
    });

    client
        .send(Command::StartAgentRun {
            session_key: "test:stream-err".into(),
            session_id: None,
            agent: "fake-stream".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: None,
            access: lazybox_ipc::AgentRunAccess::Default,
        })
        .unwrap();
    let run_id = wait_for_started(&mut client, "fake-stream").await;

    match recv_agent_event(&mut client).await {
        Event::AgentRunFinished {
            run_id: finished,
            exit_code,
            error,
        } => {
            assert_eq!(finished, run_id);
            assert_eq!(exit_code, None);
            let error = error.expect("run death must carry the error");
            assert!(error.contains("stdout torn"), "got: {error}");
        }
        other => panic!("expected AgentRunFinished, got {other:?}"),
    }
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
            access: lazybox_ipc::AgentRunAccess::Default,
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
