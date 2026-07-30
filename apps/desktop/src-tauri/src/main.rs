#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod desktop_setup;

use bytes::Bytes;
use lazybox_server::ServerConfig;
use lazybox_server::api_gateway::{
    CommandResponse, DESKTOP_PROTOCOL_FINGERPRINT, DESKTOP_PROTOCOL_VERSION,
    DESKTOP_TERMINAL_STREAM_ITEM_DATA, DESKTOP_TERMINAL_STREAM_ITEM_RESET, DesktopCommand,
    DesktopInfo, DesktopRepository, DesktopStreamMessage, GatewayOptions, JsonClientFrame,
    JsonServerFrame, PROTOCOL_FINGERPRINT_HEADER, PROTOCOL_VERSION_HEADER, ProtocolResponse,
    TERMINAL_BINARY_CONTENT_TYPE, WorkspacesResponse, desktop_event,
};
use lazybox_server::client_runtime::{ClientRuntime, ClientRuntimeOptions};
use reqwest::{Client, Response};
use serde::de::DeserializeOwned;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::{AppHandle, Manager, State};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};

enum TerminalStreamItem {
    Reset,
    Data(Bytes),
}

#[derive(Clone)]
struct GatewayClient {
    base_url: String,
    bearer_token: String,
    client: Client,
}

struct DesktopState {
    gateway: GatewayClient,
    agents: Vec<String>,
    default_agent: String,
    setup_completed: bool,
    repositories: Vec<DesktopRepository>,
    terminal_commands: mpsc::Sender<Bytes>,
    terminal_command_rx: Arc<Mutex<mpsc::Receiver<Bytes>>>,
    terminal_rx: Mutex<mpsc::Receiver<TerminalStreamItem>>,
    terminal_tx: mpsc::Sender<TerminalStreamItem>,
    streams_started: AtomicBool,
    client_runtime: Mutex<Option<ClientRuntime>>,
    gateway_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[tauri::command]
fn desktop_info(state: State<'_, DesktopState>) -> DesktopInfo {
    DesktopInfo {
        protocol_version: DESKTOP_PROTOCOL_VERSION,
        max_terminal_frame_bytes: lazybox_server::api_gateway::MAX_TERMINAL_BINARY_FRAME_BYTES,
        max_terminal_write_bytes: lazybox_ipc::MAX_WRITE_CHUNK_BYTES,
        agents: state.agents.clone(),
        default_agent: state.default_agent.clone(),
        setup_completed: state.setup_completed,
        repositories: state.repositories.clone(),
    }
}

#[tauri::command]
async fn list_workspaces(state: State<'_, DesktopState>) -> Result<WorkspacesResponse, String> {
    let response = state
        .gateway
        .authorized(
            state
                .gateway
                .client
                .get(state.gateway.url("/v1/workspaces")),
        )
        .send()
        .await
        .map_err(|error| format!("list workspaces: {error}"))?;
    decode_response(response).await
}

#[tauri::command]
async fn desktop_setup_status() -> Result<desktop_setup::DesktopSetupStatus, String> {
    desktop_setup::status().await
}

#[tauri::command]
async fn list_github_organizations() -> Result<Vec<desktop_setup::DesktopScope>, String> {
    desktop_setup::github_organizations().await
}

#[tauri::command]
async fn list_github_repositories(
    parent_id: String,
) -> Result<Vec<desktop_setup::DesktopScope>, String> {
    desktop_setup::github_repositories(&parent_id).await
}

#[tauri::command]
async fn begin_github_login() -> Result<(), String> {
    desktop_setup::begin_github_login().await
}

#[tauri::command]
async fn save_desktop_setup(
    state: State<'_, DesktopState>,
    app: AppHandle,
    input: desktop_setup::DesktopSetupInput,
) -> Result<(), String> {
    desktop_setup::save(input)?;
    state.shutdown().await;
    app.restart()
}

#[tauri::command]
fn record_analytics_event(event: desktop_setup::AnalyticsEvent) -> Result<bool, String> {
    desktop_setup::record_analytics(event)
}

#[tauri::command]
async fn send_command(
    state: State<'_, DesktopState>,
    command: DesktopCommand,
) -> Result<(), String> {
    let response = state
        .gateway
        .authorized(
            state
                .gateway
                .client
                .post(state.gateway.url("/v1/commands"))
                .json(&JsonClientFrame::Command(command.into()))
                .timeout(Duration::from_secs(5 * 60 + 5)),
        )
        .send()
        .await
        .map_err(|error| format!("send command: {error}"))?;
    let response: CommandResponse = decode_response(response).await?;
    if response.ok && response.completed {
        Ok(())
    } else {
        Err(command_failure_message(&response))
    }
}

fn command_failure_message(response: &CommandResponse) -> String {
    response
        .events
        .iter()
        .find_map(|event| match event {
            lazybox_ipc::Event::CommandFailed { message, .. } => Some(message.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "daemon did not complete the desktop command".to_string())
}

#[tauri::command]
async fn send_terminal_frame(
    state: State<'_, DesktopState>,
    request: Request<'_>,
) -> Result<(), String> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("terminal frame must be a binary request body".to_string());
    };
    if bytes.len() > lazybox_ipc::MAX_COMMAND_FRAME_BYTES as usize + 4 {
        return Err("terminal frame exceeds the command limit".to_string());
    }
    enqueue_terminal_frame(&state.terminal_commands, Bytes::copy_from_slice(bytes)).await
}

#[tauri::command]
async fn read_terminal_data(
    state: State<'_, DesktopState>,
) -> Result<tauri::ipc::Response, String> {
    let item = state
        .terminal_rx
        .lock()
        .await
        .recv()
        .await
        .ok_or_else(|| "terminal stream stopped".to_string())?;
    Ok(tauri::ipc::Response::new(encode_terminal_stream_item(item)))
}

#[tauri::command]
fn subscribe_events(
    state: State<'_, DesktopState>,
    on_event: Channel<DesktopStreamMessage>,
) -> Result<(), String> {
    if state.streams_started.swap(true, Ordering::AcqRel) {
        return Err("desktop streams are already subscribed".to_string());
    }

    let control_gateway = state.gateway.clone();
    tauri::async_runtime::spawn(async move {
        stream_control_events(control_gateway, on_event).await;
    });

    let terminal_gateway = state.gateway.clone();
    let terminal_command_rx = state.terminal_command_rx.clone();
    let terminal_tx = state.terminal_tx.clone();
    tauri::async_runtime::spawn(async move {
        stream_terminal_events(terminal_gateway, terminal_command_rx, terminal_tx).await;
    });
    Ok(())
}

impl DesktopState {
    async fn shutdown(&self) {
        if let Some(task) = self.gateway_task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(runtime) = self.client_runtime.lock().await.take() {
            runtime.shutdown().await;
        }
    }
}

impl GatewayClient {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .bearer_auth(&self.bearer_token)
            .header(PROTOCOL_VERSION_HEADER, DESKTOP_PROTOCOL_VERSION)
            .header(PROTOCOL_FINGERPRINT_HEADER, DESKTOP_PROTOCOL_FINGERPRINT)
    }
}

async fn decode_response<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("response body unavailable: {error}"));
        return Err(format!("gateway returned {status}: {body}"));
    }
    response
        .json()
        .await
        .map_err(|error| format!("decode gateway response: {error}"))
}

async fn stream_control_events(gateway: GatewayClient, on_event: Channel<DesktopStreamMessage>) {
    loop {
        match stream_control_events_once(&gateway, &on_event).await {
            Ok(()) => {
                let _ = on_event.send(DesktopStreamMessage::Disconnected {
                    message: "gateway control stream ended".to_string(),
                });
            }
            Err(error) => {
                let _ = on_event.send(DesktopStreamMessage::Disconnected { message: error });
            }
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}

async fn stream_control_events_once(
    gateway: &GatewayClient,
    on_event: &Channel<DesktopStreamMessage>,
) -> Result<(), String> {
    let mut response = gateway
        .authorized(gateway.client.get(gateway.url("/v1/events")))
        .send()
        .await
        .map_err(|error| format!("connect control stream: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "control stream returned HTTP {}",
            response.status()
        ));
    }
    let _ = on_event.send(DesktopStreamMessage::Connected);

    let mut decoder = NdjsonDecoder::default();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read control stream: {error}"))?
    {
        for frame in decoder.push(&chunk)? {
            let JsonServerFrame::Event(event) = frame;
            if let Some(event) = desktop_event(event)
                && on_event
                    .send(DesktopStreamMessage::Frame(Box::new(event)))
                    .is_err()
            {
                return Ok(());
            }
        }
    }
    decoder.finish()
}

async fn stream_terminal_events(
    gateway: GatewayClient,
    command_rx: Arc<Mutex<mpsc::Receiver<Bytes>>>,
    terminal_tx: mpsc::Sender<TerminalStreamItem>,
) {
    loop {
        if let Err(error) =
            stream_terminal_events_once(&gateway, command_rx.clone(), &terminal_tx).await
        {
            eprintln!("desktop terminal stream disconnected: {error}");
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}

async fn stream_terminal_events_once(
    gateway: &GatewayClient,
    command_rx: Arc<Mutex<mpsc::Receiver<Bytes>>>,
    terminal_tx: &mpsc::Sender<TerminalStreamItem>,
) -> Result<(), String> {
    let commands = futures_util::stream::unfold(command_rx, |command_rx| async move {
        let command = command_rx.lock().await.recv().await;
        command.map(|command| (Ok::<_, io::Error>(command), command_rx))
    });
    let body = reqwest::Body::wrap_stream(commands);
    let mut response = gateway
        .authorized(
            gateway
                .client
                .post(gateway.url("/v1/terminal"))
                .header(reqwest::header::CONTENT_TYPE, TERMINAL_BINARY_CONTENT_TYPE)
                .body(body),
        )
        .send()
        .await
        .map_err(|error| format!("connect terminal stream: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "terminal stream returned HTTP {}",
            response.status()
        ));
    }
    terminal_tx
        .send(TerminalStreamItem::Reset)
        .await
        .map_err(|_| "webview terminal reader stopped".to_string())?;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read terminal stream: {error}"))?
    {
        terminal_tx
            .send(TerminalStreamItem::Data(chunk))
            .await
            .map_err(|_| "webview terminal reader stopped".to_string())?;
    }
    Ok(())
}

async fn enqueue_terminal_frame(
    terminal_commands: &mpsc::Sender<Bytes>,
    frame: Bytes,
) -> Result<(), String> {
    terminal_commands
        .send(frame)
        .await
        .map_err(|_| "terminal stream stopped".to_string())
}

fn encode_terminal_stream_item(item: TerminalStreamItem) -> Vec<u8> {
    match item {
        TerminalStreamItem::Reset => vec![DESKTOP_TERMINAL_STREAM_ITEM_RESET],
        TerminalStreamItem::Data(bytes) => {
            let mut encoded = Vec::with_capacity(1 + bytes.len());
            encoded.push(DESKTOP_TERMINAL_STREAM_ITEM_DATA);
            encoded.extend_from_slice(&bytes);
            encoded
        }
    }
}

#[derive(Default)]
struct NdjsonDecoder {
    buffer: Vec<u8>,
}

impl NdjsonDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<JsonServerFrame>, String> {
        if self.buffer.len().saturating_add(bytes.len()) > lazybox_ipc::MAX_FRAME_BYTES as usize {
            return Err(format!(
                "gateway event line exceeds the {}-byte IPC limit",
                lazybox_ipc::MAX_FRAME_BYTES
            ));
        }
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            frames.push(
                serde_json::from_slice(&line)
                    .map_err(|error| format!("decode gateway event frame: {error}"))?,
            );
        }
        Ok(frames)
    }

    fn finish(&self) -> Result<(), String> {
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err("gateway event stream ended with an incomplete frame".to_string())
        }
    }
}

async fn start_desktop_state() -> Result<DesktopState, String> {
    let user_config = lazybox_config::Config::load()
        .map_err(|error| format!("load lazybox configuration: {error}"))?;
    let config = ServerConfig::from_user_config()
        .map_err(|error| format!("start lazybox daemon: {error}"))?;
    let client_runtime = ClientRuntime::start(
        config.clone(),
        ClientRuntimeOptions {
            poll_interval: user_config.providers.github.poll_interval,
            restore_persisted_sessions: true,
            slack: Some(user_config.slack.clone()),
        },
    )
    .await;

    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .map_err(|error| format!("bind embedded API gateway: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read embedded API address: {error}"))?;
    let bearer_token = uuid::Uuid::new_v4().simple().to_string();
    let options = GatewayOptions {
        bind_addr: address,
        bearer_token: Some(bearer_token.clone()),
        ..GatewayOptions::default()
    };
    let gateway_task = tokio::spawn(async move {
        if let Err(error) =
            lazybox_server::api_gateway::serve_listener(config, options, listener).await
        {
            eprintln!("lazybox desktop embedded API gateway stopped: {error}");
        }
    });

    let gateway = GatewayClient {
        base_url: format!("http://{address}"),
        bearer_token,
        client: Client::new(),
    };
    let protocol: ProtocolResponse = decode_response(
        gateway
            .authorized(gateway.client.get(gateway.url("/v1/protocol")))
            .send()
            .await
            .map_err(|error| format!("discover daemon protocol: {error}"))?,
    )
    .await?;
    validate_protocol(&protocol)?;

    let mut agents = user_config.setup.agents.iter().cloned().collect::<Vec<_>>();
    if agents.is_empty() {
        agents = ["claude", "codex", "cursor-agent"]
            .into_iter()
            .map(str::to_string)
            .collect();
    }
    agents.sort();
    agents.dedup();
    let repositories = configured_repositories(&user_config);
    let configured_default = user_config
        .setup
        .default_agent
        .filter(|agent| agents.contains(agent));
    let default_agent = configured_default
        .or_else(|| {
            agents
                .iter()
                .find(|agent| agent.as_str() == "claude")
                .cloned()
        })
        .or_else(|| agents.first().cloned())
        .ok_or_else(|| "no agent is configured".to_string())?;
    let setup_completed = user_config.setup.wizard_completed;
    let (terminal_commands, terminal_command_rx) = mpsc::channel(256);
    let (terminal_tx, terminal_rx) = mpsc::channel(32);

    Ok(DesktopState {
        gateway,
        agents,
        default_agent,
        setup_completed,
        repositories,
        terminal_commands,
        terminal_command_rx: Arc::new(Mutex::new(terminal_command_rx)),
        terminal_rx: Mutex::new(terminal_rx),
        terminal_tx,
        streams_started: AtomicBool::new(false),
        client_runtime: Mutex::new(Some(client_runtime)),
        gateway_task: Mutex::new(Some(gateway_task)),
    })
}

fn configured_repositories(config: &lazybox_config::Config) -> Vec<DesktopRepository> {
    let mut repositories = config
        .setup
        .scopes
        .get("github")
        .into_iter()
        .flatten()
        .filter_map(|scope| {
            let slug = scope.strip_prefix("github:")?;
            let (owner, repo) = slug.split_once('/')?;
            (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| {
                DesktopRepository {
                    project_key: lazybox_core::ProjectKey::github(owner, repo),
                    label: slug.to_string(),
                }
            })
        })
        .collect::<Vec<_>>();
    repositories.sort_by(|left, right| left.label.cmp(&right.label));
    repositories
}

fn validate_protocol(protocol: &ProtocolResponse) -> Result<(), String> {
    if protocol.protocol_version != DESKTOP_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported lazybox protocol version {}; desktop supports version {}",
            protocol.protocol_version, DESKTOP_PROTOCOL_VERSION
        ));
    }
    if protocol.protocol_fingerprint != DESKTOP_PROTOCOL_FINGERPRINT {
        return Err(format!(
            "unsupported lazybox protocol fingerprint {}; desktop supports {}",
            protocol.protocol_fingerprint, DESKTOP_PROTOCOL_FINGERPRINT
        ));
    }
    if protocol.terminal_transport != TERMINAL_BINARY_CONTENT_TYPE {
        return Err(format!(
            "unsupported terminal transport {}; desktop requires {}",
            protocol.terminal_transport, TERMINAL_BINARY_CONTENT_TYPE
        ));
    }
    Ok(())
}

fn main() {
    #[cfg(target_os = "macos")]
    desktop_setup::hydrate_gui_path();
    desktop_setup::install_crash_hook();
    let state = match tauri::async_runtime::block_on(start_desktop_state()) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("lazybox desktop failed to start: {error}");
            std::process::exit(1);
        }
    };
    let app = tauri::Builder::default()
        .setup(|app| {
            if let Some(window) = app.get_webview_window("main") {
                window.set_focus()?;
            }
            Ok(())
        })
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            desktop_info,
            desktop_setup_status,
            list_github_organizations,
            list_github_repositories,
            begin_github_login,
            save_desktop_setup,
            record_analytics_event,
            list_workspaces,
            send_command,
            send_terminal_frame,
            read_terminal_data,
            subscribe_events
        ])
        .build(tauri::generate_context!())
        .expect("build lazybox desktop");
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            tauri::async_runtime::block_on(handle.state::<DesktopState>().shutdown());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_ipc::{Command, TerminalId};

    #[test]
    fn ndjson_decoder_handles_split_and_batched_frames() {
        let first = JsonServerFrame::Event(lazybox_ipc::Event::TerminalResyncUnavailable {
            terminal_id: TerminalId(7),
        });
        let second = JsonServerFrame::Event(lazybox_ipc::Event::PollCompleted {
            source: "github".to_string(),
            count: 2,
        });
        let first_json = serde_json::to_string(&first).expect("serialize first frame");
        let second_json = serde_json::to_string(&second).expect("serialize second frame");
        let bytes = format!("{first_json}\n{second_json}\n");
        let split = bytes.len() / 2;
        let mut decoder = NdjsonDecoder::default();

        let mut decoded = decoder
            .push(&bytes.as_bytes()[..split])
            .expect("decode first chunk");
        decoded.extend(
            decoder
                .push(&bytes.as_bytes()[split..])
                .expect("decode second chunk"),
        );

        assert_eq!(decoded.len(), 2);
        assert!(decoder.finish().is_ok());
    }

    #[test]
    fn gateway_client_keeps_the_token_out_of_its_url() {
        let gateway = GatewayClient {
            base_url: "http://127.0.0.1:1234".to_string(),
            bearer_token: "secret".to_string(),
            client: Client::new(),
        };
        assert_eq!(
            gateway.url("/v1/terminal"),
            "http://127.0.0.1:1234/v1/terminal"
        );
        assert!(!gateway.url("/v1/terminal").contains("secret"));
    }

    #[test]
    fn desktop_rejects_a_daemon_with_a_different_contract_fingerprint() {
        let mut protocol = lazybox_server::api_gateway::protocol_response();
        protocol.protocol_fingerprint = DESKTOP_PROTOCOL_FINGERPRINT.wrapping_add(1);

        let error = validate_protocol(&protocol).expect_err("fingerprint mismatch must fail");

        assert!(error.contains("unsupported lazybox protocol fingerprint"));
    }

    #[test]
    fn desktop_command_translation_exposes_only_the_supported_control_shape() {
        let command = Command::from(DesktopCommand::SpawnAgent {
            session_key: "github:o/r#1".into(),
            agent: "codex".to_string(),
        });

        assert!(matches!(
            command,
            Command::Spawn {
                kind: lazybox_ipc::TerminalKind::Agent(agent),
                cwd: None,
                initial_prompt: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                client_request_id: None,
                ..
            } if agent == "codex"
        ));
    }

    #[test]
    fn desktop_mutations_reuse_daemon_command_semantics() {
        let session_key = lazybox_core::SessionKey::from("github:o/r#1");
        assert!(matches!(
            Command::from(DesktopCommand::SpawnShell {
                session_key: session_key.clone(),
            }),
            Command::Spawn {
                kind: lazybox_ipc::TerminalKind::Shell,
                ..
            }
        ));
        assert!(matches!(
            Command::from(DesktopCommand::MarkRead {
                session_key: session_key.clone(),
            }),
            Command::MarkRead { session_key: key } if key == session_key
        ));
        assert!(matches!(
            Command::from(DesktopCommand::PostReply {
                session_key,
                body: "hello".into(),
            }),
            Command::PostReply {
                body,
                client_request_id: Some(_),
                ..
            } if body == "hello"
        ));
    }

    #[test]
    fn desktop_surfaces_the_correlated_daemon_failure() {
        let response = CommandResponse {
            ok: false,
            completed: true,
            events: vec![lazybox_ipc::Event::CommandFailed {
                client_request_id: "reply-1".into(),
                message: "post failed: permission denied".into(),
            }],
        };

        assert_eq!(
            command_failure_message(&response),
            "post failed: permission denied"
        );
    }

    #[test]
    fn configured_repository_is_available_before_its_first_workspace() {
        let mut config = lazybox_config::Config::default();
        config.setup.scopes.insert(
            "github".into(),
            ["github:acme/widget".into(), "github:whole-org".into()]
                .into_iter()
                .collect(),
        );

        let repositories = configured_repositories(&config);

        assert_eq!(repositories.len(), 1);
        assert_eq!(repositories[0].label, "acme/widget");
        assert_eq!(
            repositories[0].project_key,
            lazybox_core::ProjectKey::github("acme", "widget")
        );
    }

    #[tokio::test]
    async fn terminal_command_queue_applies_backpressure_without_dropping_input() {
        let (tx, mut rx) = mpsc::channel(1);
        enqueue_terminal_frame(&tx, Bytes::from_static(b"first"))
            .await
            .expect("enqueue first frame");
        let second = enqueue_terminal_frame(&tx, Bytes::from_static(b"second"));
        tokio::pin!(second);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut second)
                .await
                .is_err(),
            "a full terminal queue must hold the producer"
        );
        assert_eq!(rx.recv().await.as_deref(), Some(b"first".as_slice()));
        tokio::time::timeout(Duration::from_secs(1), &mut second)
            .await
            .expect("second enqueue resumes")
            .expect("enqueue second frame");
        assert_eq!(rx.recv().await.as_deref(), Some(b"second".as_slice()));
    }

    #[test]
    fn terminal_stream_reset_precedes_unmodified_binary_data() {
        assert_eq!(
            encode_terminal_stream_item(TerminalStreamItem::Reset),
            vec![DESKTOP_TERMINAL_STREAM_ITEM_RESET]
        );
        assert_eq!(
            encode_terminal_stream_item(TerminalStreamItem::Data(Bytes::from_static(&[
                0, 27, 255
            ]))),
            vec![DESKTOP_TERMINAL_STREAM_ITEM_DATA, 0, 27, 255]
        );
    }
}
