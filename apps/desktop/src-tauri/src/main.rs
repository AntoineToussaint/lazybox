#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bytes::Bytes;
use futures_util::StreamExt;
use lazybox_ipc::Command;
use lazybox_server::ServerConfig;
use lazybox_server::api_gateway::{
    CommandResponse, DESKTOP_PROTOCOL_VERSION, DesktopInfo, DesktopStreamMessage, GatewayOptions,
    JsonClientFrame, JsonServerFrame, PROTOCOL_VERSION_HEADER, ProtocolResponse,
    TERMINAL_BINARY_CONTENT_TYPE, WorkspacesResponse,
};
use lazybox_server::client_runtime::{ClientRuntime, ClientRuntimeOptions};
use reqwest::{Client, Response};
use serde::de::DeserializeOwned;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::{Manager, State};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

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
    terminal_commands: broadcast::Sender<Bytes>,
    terminal_rx: Mutex<mpsc::Receiver<Bytes>>,
    terminal_tx: mpsc::Sender<Bytes>,
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
async fn send_command(
    state: State<'_, DesktopState>,
    command: Command,
) -> Result<CommandResponse, String> {
    if is_terminal_command(&command) {
        return Err("terminal commands must use the binary terminal channel".to_string());
    }
    let response = state
        .gateway
        .authorized(
            state
                .gateway
                .client
                .post(state.gateway.url("/v1/commands"))
                .json(&JsonClientFrame::Command(command))
                .timeout(Duration::from_secs(5 * 60 + 5)),
        )
        .send()
        .await
        .map_err(|error| format!("send command: {error}"))?;
    decode_response(response).await
}

#[tauri::command]
fn send_terminal_frame(state: State<'_, DesktopState>, request: Request<'_>) -> Result<(), String> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("terminal frame must be a binary request body".to_string());
    };
    if bytes.len() > lazybox_ipc::MAX_COMMAND_FRAME_BYTES as usize + 4 {
        return Err("terminal frame exceeds the command limit".to_string());
    }
    state
        .terminal_commands
        .send(Bytes::copy_from_slice(bytes))
        .map_err(|_| "terminal stream is not connected".to_string())?;
    Ok(())
}

#[tauri::command]
async fn read_terminal_data(
    state: State<'_, DesktopState>,
) -> Result<tauri::ipc::Response, String> {
    let bytes = state
        .terminal_rx
        .lock()
        .await
        .recv()
        .await
        .ok_or_else(|| "terminal stream stopped".to_string())?;
    Ok(tauri::ipc::Response::new(bytes.to_vec()))
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
    let terminal_commands = state.terminal_commands.clone();
    let terminal_tx = state.terminal_tx.clone();
    tauri::async_runtime::spawn(async move {
        stream_terminal_events(terminal_gateway, terminal_commands, terminal_tx).await;
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

fn is_terminal_command(command: &Command) -> bool {
    matches!(
        command,
        Command::Write { .. }
            | Command::Resize { .. }
            | Command::RequestTerminalResync { .. }
            | Command::Close { .. }
    )
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
            if on_event.send(DesktopStreamMessage::Frame(frame)).is_err() {
                return Ok(());
            }
        }
    }
    decoder.finish()
}

async fn stream_terminal_events(
    gateway: GatewayClient,
    commands: broadcast::Sender<Bytes>,
    terminal_tx: mpsc::Sender<Bytes>,
) {
    loop {
        if let Err(error) =
            stream_terminal_events_once(&gateway, commands.subscribe(), &terminal_tx).await
        {
            eprintln!("desktop terminal stream disconnected: {error}");
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}

async fn stream_terminal_events_once(
    gateway: &GatewayClient,
    command_rx: broadcast::Receiver<Bytes>,
    terminal_tx: &mpsc::Sender<Bytes>,
) -> Result<(), String> {
    let commands = BroadcastStream::new(command_rx).map(|result| {
        result.map_err(|error| io::Error::other(format!("terminal command stream lagged: {error}")))
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

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read terminal stream: {error}"))?
    {
        terminal_tx
            .send(chunk)
            .await
            .map_err(|_| "webview terminal reader stopped".to_string())?;
    }
    Ok(())
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
    if protocol.protocol_version != DESKTOP_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported lazybox protocol version {}; desktop supports version {}",
            protocol.protocol_version, DESKTOP_PROTOCOL_VERSION
        ));
    }
    if protocol.terminal_transport != TERMINAL_BINARY_CONTENT_TYPE {
        return Err(format!(
            "unsupported terminal transport {}; desktop requires {}",
            protocol.terminal_transport, TERMINAL_BINARY_CONTENT_TYPE
        ));
    }

    let mut agents = user_config.setup.agents.iter().cloned().collect::<Vec<_>>();
    if agents.is_empty() {
        agents = ["claude", "codex", "cursor"]
            .into_iter()
            .map(str::to_string)
            .collect();
    }
    agents.sort();
    agents.dedup();
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
    let (terminal_commands, _) = broadcast::channel(256);
    let (terminal_tx, terminal_rx) = mpsc::channel(32);

    Ok(DesktopState {
        gateway,
        agents,
        default_agent,
        terminal_commands,
        terminal_rx: Mutex::new(terminal_rx),
        terminal_tx,
        streams_started: AtomicBool::new(false),
        client_runtime: Mutex::new(Some(client_runtime)),
        gateway_task: Mutex::new(Some(gateway_task)),
    })
}

fn main() {
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
    use lazybox_ipc::TerminalId;

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
    fn terminal_commands_are_rejected_from_the_json_command_path() {
        assert!(is_terminal_command(&Command::Write {
            terminal_id: TerminalId(4),
            bytes: vec![b'x'],
        }));
        assert!(is_terminal_command(&Command::RequestTerminalResync {
            terminal_id: TerminalId(4),
            required_seq: 8,
        }));
        assert!(!is_terminal_command(&Command::Refresh));
    }
}
