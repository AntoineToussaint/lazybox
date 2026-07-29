#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use lazybox_ipc::{Command, MAX_FRAME_BYTES};
use lazybox_server::ServerConfig;
use lazybox_server::api_gateway::{
    CommandResponse, GatewayOptions, JsonClientFrame, JsonServerFrame, WorkspacesResponse,
};
use reqwest::{Client, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::{Manager, State};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
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
    terminal_commands: broadcast::Sender<Command>,
    events: broadcast::Sender<DesktopStreamMessage>,
    duplex_started: AtomicBool,
}

#[derive(Clone, Serialize)]
struct DesktopInfo {
    agents: Vec<String>,
    default_agent: String,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
enum DesktopStreamMessage {
    Connected,
    Disconnected { message: String },
    Frame(JsonServerFrame),
}

#[tauri::command]
fn desktop_info(state: State<'_, DesktopState>) -> DesktopInfo {
    DesktopInfo {
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
    if is_duplex_terminal_command(&command) {
        state
            .terminal_commands
            .send(command)
            .map_err(|_| "terminal stream is not connected".to_string())?;
        return Ok(CommandResponse {
            ok: true,
            completed: false,
        });
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
fn subscribe_events(state: State<'_, DesktopState>, on_event: Channel<DesktopStreamMessage>) {
    let mut events = state.events.subscribe();
    let terminal_commands = state.terminal_commands.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if on_event.send(event).is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    if on_event
                        .send(DesktopStreamMessage::Disconnected {
                            message: format!(
                                "desktop event channel lagged by {skipped} frames; resynchronizing"
                            ),
                        })
                        .is_err()
                    {
                        return;
                    }
                    let _ = terminal_commands.send(Command::Subscribe);
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    if state.duplex_started.swap(true, Ordering::AcqRel) {
        let _ = state.terminal_commands.send(Command::Subscribe);
    } else {
        let gateway = state.gateway.clone();
        let commands = state.terminal_commands.clone();
        let events = state.events.clone();
        tauri::async_runtime::spawn(async move {
            stream_events(gateway, commands, events).await;
        });
    }
}

impl GatewayClient {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.bearer_auth(&self.bearer_token)
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

fn is_duplex_terminal_command(command: &Command) -> bool {
    matches!(
        command,
        Command::Write { .. }
            | Command::Resize { .. }
            | Command::RequestTerminalResync { .. }
            | Command::Close { .. }
    )
}

async fn stream_events(
    gateway: GatewayClient,
    commands: broadcast::Sender<Command>,
    events: broadcast::Sender<DesktopStreamMessage>,
) {
    loop {
        match stream_events_once(&gateway, commands.subscribe(), &events).await {
            Ok(()) => {
                let _ = events.send(DesktopStreamMessage::Disconnected {
                    message: "gateway event stream ended".to_string(),
                });
            }
            Err(error) => {
                let _ = events.send(DesktopStreamMessage::Disconnected { message: error });
            }
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}

async fn stream_events_once(
    gateway: &GatewayClient,
    command_rx: broadcast::Receiver<Command>,
    events: &broadcast::Sender<DesktopStreamMessage>,
) -> Result<(), String> {
    let subscribe = stream::once(async { encode_command(Command::Subscribe) });
    let commands = BroadcastStream::new(command_rx).filter_map(|result| async move {
        match result {
            Ok(command) => Some(encode_command(command)),
            Err(error) => Some(Err(io::Error::other(format!(
                "terminal command stream lagged: {error}"
            )))),
        }
    });
    let body = reqwest::Body::wrap_stream(subscribe.chain(commands));
    let mut response = gateway
        .authorized(gateway.client.post(gateway.url("/v1/stream")).body(body))
        .send()
        .await
        .map_err(|error| format!("connect duplex stream: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("duplex stream returned HTTP {}", response.status()));
    }
    let _ = events.send(DesktopStreamMessage::Connected);

    let mut decoder = NdjsonDecoder::default();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read event stream: {error}"))?
    {
        for frame in decoder.push(&chunk)? {
            let _ = events.send(DesktopStreamMessage::Frame(frame));
        }
    }
    decoder.finish()
}

fn encode_command(command: Command) -> Result<Bytes, io::Error> {
    let mut bytes = serde_json::to_vec(&JsonClientFrame::Command(command))
        .map_err(|error| io::Error::other(format!("encode command frame: {error}")))?;
    bytes.push(b'\n');
    Ok(Bytes::from(bytes))
}

#[derive(Default)]
struct NdjsonDecoder {
    buffer: Vec<u8>,
}

impl NdjsonDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<JsonServerFrame>, String> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_FRAME_BYTES as usize {
            return Err(format!(
                "gateway event line exceeds the {}-byte IPC limit",
                MAX_FRAME_BYTES
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

    if tokio::time::timeout(
        Duration::from_secs(5),
        lazybox_server::spawn_handler::recover_sessions(&config),
    )
    .await
    .is_err()
    {
        eprintln!("lazybox desktop: terminal recovery timed out; continuing");
    }
    {
        let config = config.clone();
        tokio::spawn(async move {
            lazybox_server::spawn_handler::restore_persisted_sessions(&config).await;
        });
    }
    lazybox_server::polling::migrate_legacy_sandbox(&config);
    lazybox_server::polling::spawn(config.clone(), user_config.providers.github.poll_interval);
    let _ = lazybox_server::keep_awake::spawn(&config);
    lazybox_server::agent_updates::spawn_scheduled(config.clone());
    let _ = lazybox_server::slack::spawn(config.clone(), user_config.slack.clone());

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
    tokio::spawn(async move {
        if let Err(error) =
            lazybox_server::api_gateway::serve_listener(config, options, listener).await
        {
            eprintln!("lazybox desktop: embedded API gateway stopped: {error}");
        }
    });

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
    let (terminal_commands, _) = broadcast::channel(1024);
    let (events, _) = broadcast::channel(1024);

    Ok(DesktopState {
        gateway: GatewayClient {
            base_url: format!("http://{address}"),
            bearer_token,
            client: Client::new(),
        },
        agents,
        default_agent,
        terminal_commands,
        events,
        duplex_started: AtomicBool::new(false),
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
    tauri::Builder::default()
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
            subscribe_events
        ])
        .run(tauri::generate_context!())
        .expect("run lazybox desktop");
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_ipc::{Event, TerminalId};

    #[test]
    fn ndjson_decoder_handles_split_and_batched_frames() {
        let first = JsonServerFrame::Event(Event::TerminalResyncUnavailable {
            terminal_id: TerminalId(7),
        });
        let second = JsonServerFrame::Event(Event::PollCompleted {
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
    fn ndjson_decoder_rejects_an_incomplete_final_frame() {
        let mut decoder = NdjsonDecoder::default();
        decoder.push(br#"{"type":"Event""#).expect("buffer chunk");
        assert_eq!(
            decoder.finish(),
            Err("gateway event stream ended with an incomplete frame".to_string())
        );
    }

    #[test]
    fn gateway_client_keeps_the_token_out_of_its_url() {
        let gateway = GatewayClient {
            base_url: "http://127.0.0.1:1234".to_string(),
            bearer_token: "secret".to_string(),
            client: Client::new(),
        };
        assert_eq!(gateway.url("/v1/stream"), "http://127.0.0.1:1234/v1/stream");
        assert!(!gateway.url("/v1/stream").contains("secret"));
    }

    #[test]
    fn terminal_commands_use_the_connection_that_receives_their_replies() {
        assert!(is_duplex_terminal_command(
            &Command::RequestTerminalResync {
                terminal_id: TerminalId(4),
                required_seq: 8,
            }
        ));
        assert!(is_duplex_terminal_command(&Command::Write {
            terminal_id: TerminalId(4),
            bytes: vec![b'x'],
        }));
        assert!(!is_duplex_terminal_command(&Command::Refresh));
    }

    #[test]
    fn encoded_duplex_commands_are_newline_delimited_json_frames() {
        let encoded = encode_command(Command::Resize {
            terminal_id: TerminalId(3),
            cols: 120,
            rows: 40,
        })
        .expect("encode resize");
        assert_eq!(encoded.last(), Some(&b'\n'));
        let frame = serde_json::from_slice::<JsonClientFrame>(&encoded[..encoded.len() - 1])
            .expect("decode command frame");
        assert!(matches!(
            frame,
            JsonClientFrame::Command(Command::Resize {
                terminal_id: TerminalId(3),
                cols: 120,
                rows: 40,
            })
        ));
    }

    #[tokio::test]
    async fn duplex_gateway_returns_connection_scoped_terminal_replies() {
        let config = ServerConfig::in_memory();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind gateway");
        let address = listener.local_addr().expect("read gateway address");
        let options = GatewayOptions {
            bind_addr: address,
            bearer_token: Some("test-token".to_string()),
            ..GatewayOptions::default()
        };
        let gateway_task = tokio::spawn(async move {
            lazybox_server::api_gateway::serve_listener(config, options, listener).await
        });
        let gateway = GatewayClient {
            base_url: format!("http://{address}"),
            bearer_token: "test-token".to_string(),
            client: Client::new(),
        };
        let (commands, command_rx) = broadcast::channel(8);
        let (events, mut event_rx) = broadcast::channel(8);
        let stream_task =
            tokio::spawn(async move { stream_events_once(&gateway, command_rx, &events).await });

        let connected = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("connected event timeout")
            .expect("connected event");
        assert!(matches!(connected, DesktopStreamMessage::Connected));

        commands
            .send(Command::RequestTerminalResync {
                terminal_id: TerminalId(99),
                required_seq: 1,
            })
            .expect("send resync request");

        let unavailable = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let DesktopStreamMessage::Frame(JsonServerFrame::Event(
                    Event::TerminalResyncUnavailable {
                        terminal_id: TerminalId(99),
                    },
                )) = event_rx.recv().await.expect("stream event")
                {
                    break;
                }
            }
        })
        .await;
        assert!(unavailable.is_ok(), "missing connection-scoped reply");

        stream_task.abort();
        gateway_task.abort();
    }
}
