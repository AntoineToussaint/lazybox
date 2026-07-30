#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bytes::Bytes;
use lazybox_core::ProviderConfig;
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
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
    repositories: Vec<DesktopRepository>,
    terminal_commands: mpsc::Sender<Bytes>,
    terminal_command_rx: Arc<Mutex<mpsc::Receiver<Bytes>>>,
    terminal_rx: Mutex<mpsc::Receiver<TerminalStreamItem>>,
    terminal_tx: mpsc::Sender<TerminalStreamItem>,
    streams_started: AtomicBool,
    client_runtime: Mutex<Option<ClientRuntime>>,
    gateway_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone, Debug, Serialize)]
struct DesktopAgentOption {
    id: String,
    label: String,
    available: bool,
}

#[derive(Serialize)]
struct DesktopSetupState {
    first_run: bool,
    selected_scopes: Vec<String>,
    agents: Vec<DesktopAgentOption>,
    default_agent: String,
    analytics_enabled: bool,
    diagnostics_path: String,
}

#[derive(Serialize)]
struct GithubAuthStatus {
    authenticated: bool,
    account: Option<String>,
    message: String,
}

#[derive(Serialize)]
struct GithubRepositoryOption {
    id: String,
    label: String,
    owner: String,
}

#[derive(Deserialize)]
struct SaveDesktopSettings {
    github_scopes: Vec<String>,
    default_agent: String,
    analytics_enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum AnalyticsEvent {
    AppOpened,
    OnboardingCompleted,
    WorkspaceOpened,
    AgentStarted,
    ShellStarted,
    ReplyPosted,
}

#[tauri::command]
fn desktop_info(state: State<'_, DesktopState>) -> DesktopInfo {
    DesktopInfo {
        protocol_version: DESKTOP_PROTOCOL_VERSION,
        max_terminal_frame_bytes: lazybox_server::api_gateway::MAX_TERMINAL_BINARY_FRAME_BYTES,
        max_terminal_write_bytes: lazybox_ipc::MAX_WRITE_CHUNK_BYTES,
        agents: state.agents.clone(),
        default_agent: state.default_agent.clone(),
        repositories: state.repositories.clone(),
    }
}

#[tauri::command]
fn desktop_setup_state() -> Result<DesktopSetupState, String> {
    let config = lazybox_config::Config::load()
        .map_err(|error| format!("load lazybox configuration: {error}"))?;
    Ok(desktop_setup_state_from_config(&config))
}

fn desktop_setup_state_from_config(config: &lazybox_config::Config) -> DesktopSetupState {
    let mut selected_scopes = config
        .setup
        .scopes
        .get("github")
        .into_iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    selected_scopes.sort();
    let first_run = !config.setup.wizard_completed || !config.setup.providers.contains("github");
    DesktopSetupState {
        first_run,
        selected_scopes,
        agents: detect_agent_options(config),
        default_agent: effective_default_agent(config),
        analytics_enabled: config.desktop.analytics_enabled,
        diagnostics_path: diagnostics_dir().display().to_string(),
    }
}

#[tauri::command]
async fn github_auth_status() -> GithubAuthStatus {
    match authenticated_github_client().await {
        Ok(client) => GithubAuthStatus {
            authenticated: true,
            account: Some(client.authenticated_user().to_string()),
            message: "GitHub credential verified".to_string(),
        },
        Err(error) => GithubAuthStatus {
            authenticated: false,
            account: None,
            message: error,
        },
    }
}

#[tauri::command]
async fn begin_github_login() -> Result<(), String> {
    which::which("gh").map_err(|_| "GitHub CLI is not installed; run `brew install gh`")?;
    tokio::process::Command::new("gh")
        .args([
            "auth",
            "login",
            "--web",
            "--clipboard",
            "--hostname",
            "github.com",
            "--git-protocol",
            "https",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start GitHub sign-in: {error}"))?;
    Ok(())
}

#[tauri::command]
async fn list_github_repositories() -> Result<Vec<GithubRepositoryOption>, String> {
    let client = authenticated_github_client().await?;
    let parents = tokio::time::timeout(Duration::from_secs(20), client.list_scopes())
        .await
        .map_err(|_| "GitHub repository discovery timed out".to_string())?
        .map_err(|error| format!("discover GitHub accounts: {error}"))?;
    let mut repositories = Vec::new();
    for parent in parents {
        let scopes = tokio::time::timeout(
            Duration::from_secs(30),
            client.list_repos_in_org(&parent.id),
        )
        .await
        .map_err(|_| format!("GitHub repository discovery timed out for {}", parent.label))?
        .map_err(|error| format!("discover GitHub repositories for {}: {error}", parent.label))?;
        repositories.extend(scopes.into_iter().map(|scope| GithubRepositoryOption {
            id: scope.id,
            label: scope.label,
            owner: parent.label.clone(),
        }));
    }
    repositories.sort_by(|left, right| left.label.cmp(&right.label));
    repositories.dedup_by(|left, right| left.id == right.id);
    Ok(repositories)
}

#[tauri::command]
fn save_desktop_settings(app: AppHandle, settings: SaveDesktopSettings) -> Result<(), String> {
    let config = lazybox_config::Config::load()
        .map_err(|error| format!("load lazybox configuration: {error}"))?;
    let scopes = validate_github_scopes(
        settings.github_scopes,
        !config.setup.wizard_completed || !config.setup.providers.contains("github"),
    )?;
    if !detect_agent_options(&config)
        .iter()
        .any(|agent| agent.id == settings.default_agent && agent.available)
    {
        return Err("select an installed agent".to_string());
    }
    let default_agent = settings.default_agent;
    let analytics_enabled = settings.analytics_enabled;
    lazybox_config::Config::save_with(move |config| {
        apply_desktop_settings(config, scopes, default_agent, analytics_enabled);
    })
    .map_err(|error| format!("save lazybox configuration: {error}"))?;
    if analytics_enabled
        && let Err(error) =
            append_analytics_event(&analytics_path(), AnalyticsEvent::OnboardingCompleted)
    {
        eprintln!("lazybox desktop could not record onboarding analytics: {error}");
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        app.request_restart();
    });
    Ok(())
}

#[tauri::command]
fn record_analytics(event: AnalyticsEvent) {
    let enabled = lazybox_config::Config::load()
        .map(|config| config.desktop.analytics_enabled)
        .unwrap_or(false);
    record_analytics_best_effort(&analytics_path(), enabled, event);
}

fn record_analytics_best_effort(path: &Path, enabled: bool, event: AnalyticsEvent) {
    if enabled && let Err(error) = append_analytics_event(path, event) {
        eprintln!("lazybox desktop could not record analytics: {error}");
    }
}

#[tauri::command]
async fn list_workspaces(state: State<'_, DesktopState>) -> Result<WorkspacesResponse, String> {
    list_gateway_workspaces(&state.gateway).await
}

async fn list_gateway_workspaces(gateway: &GatewayClient) -> Result<WorkspacesResponse, String> {
    let response = gateway
        .authorized(gateway.client.get(gateway.url("/v1/workspaces")))
        .send()
        .await
        .map_err(|error| format!("list workspaces: {error}"))?;
    decode_response(response).await
}

#[tauri::command]
async fn send_command(
    state: State<'_, DesktopState>,
    command: DesktopCommand,
) -> Result<(), String> {
    send_gateway_command(&state.gateway, command).await
}

async fn send_gateway_command(
    gateway: &GatewayClient,
    command: DesktopCommand,
) -> Result<(), String> {
    let command = command.into_correlated(Some(uuid::Uuid::new_v4().simple().to_string()));
    let response = gateway
        .authorized(
            gateway
                .client
                .post(gateway.url("/v1/commands"))
                .json(&JsonClientFrame::Command(command))
                .timeout(Duration::from_secs(5 * 60 + 5)),
        )
        .send()
        .await
        .map_err(|error| format!("send command: {error}"))?;
    let response: CommandResponse = decode_response(response).await?;
    if response.ok && response.completed {
        Ok(())
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "daemon did not complete the desktop command".to_string()))
    }
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

async fn authenticated_github_client() -> Result<lazybox_gh::GhClient, String> {
    let credential = tokio::time::timeout(
        Duration::from_secs(5),
        lazybox_gh::credential_chain().resolve(lazybox_gh::SOURCE),
    )
    .await
    .map_err(|_| "GitHub credential lookup timed out".to_string())?
    .map_err(|_| "No GitHub credential found. Sign in with GitHub CLI.".to_string())?;
    tokio::time::timeout(
        Duration::from_secs(10),
        lazybox_gh::GhClient::from_credential(credential),
    )
    .await
    .map_err(|_| "GitHub credential verification timed out".to_string())?
    .map_err(|error| format!("GitHub credential verification failed: {error}"))
}

fn validate_github_scopes(
    scopes: Vec<String>,
    require_selection: bool,
) -> Result<BTreeSet<String>, String> {
    let scopes = scopes
        .into_iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect::<BTreeSet<_>>();
    if require_selection && scopes.is_empty() {
        return Err("select a GitHub organization or repository".to_string());
    }
    if scopes.iter().any(|scope| {
        !scope.strip_prefix("github:").is_some_and(|path| {
            let mut parts = path.split('/');
            let owner = parts.next().is_some_and(|part| !part.is_empty());
            let repository = parts.next();
            owner && repository.is_none_or(|part| !part.is_empty()) && parts.next().is_none()
        })
    }) {
        return Err("GitHub scopes must use github:owner or github:owner/repository".to_string());
    }
    Ok(scopes)
}

fn apply_desktop_settings(
    config: &mut lazybox_config::Config,
    scopes: BTreeSet<String>,
    default_agent: String,
    analytics_enabled: bool,
) {
    config.setup.providers.insert("github".to_string());
    config.setup.agents.insert(default_agent.clone());
    config
        .setup
        .filters
        .entry("github".to_string())
        .or_insert_with(|| ProviderConfig::default_for("github").enabled_keys);
    config.setup.scopes.insert("github".to_string(), scopes);
    config.setup.default_agent = Some(default_agent);
    config.setup.wizard_completed = true;
    config.desktop.analytics_enabled = analytics_enabled;
}

fn detect_agent_options(config: &lazybox_config::Config) -> Vec<DesktopAgentOption> {
    let mut ids = ["claude", "codex", "cursor-agent"]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    ids.extend(config.agents.keys().cloned());
    ids.into_iter()
        .map(|id| {
            let configured = config.agents.get(&id);
            let command = configured
                .and_then(|entry| entry.command.clone())
                .unwrap_or_else(|| id.clone());
            let label = configured
                .and_then(|entry| entry.name.clone())
                .unwrap_or_else(|| match id.as_str() {
                    "claude" => "Claude Code".to_string(),
                    "codex" => "Codex".to_string(),
                    "cursor-agent" => "Cursor Agent".to_string(),
                    _ => id.clone(),
                });
            DesktopAgentOption {
                id,
                label,
                available: which::which(&command).is_ok(),
            }
        })
        .collect()
}

fn effective_default_agent(config: &lazybox_config::Config) -> String {
    config
        .setup
        .default_agent
        .clone()
        .filter(|agent| !agent.trim().is_empty())
        .unwrap_or_else(|| "claude".to_string())
}

fn configured_agent_ids(config: &lazybox_config::Config) -> Vec<String> {
    let mut agents = config.setup.agents.iter().cloned().collect::<BTreeSet<_>>();
    if agents.is_empty() {
        agents.extend(["claude", "codex", "cursor-agent"].map(str::to_string));
    }
    agents.insert(effective_default_agent(config));
    agents.into_iter().collect()
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
            let (owner, repository) = slug.split_once('/')?;
            (!owner.is_empty() && !repository.is_empty() && !repository.contains('/')).then(|| {
                DesktopRepository {
                    project_key: lazybox_core::ProjectKey::github(owner, repository),
                    label: slug.to_string(),
                }
            })
        })
        .collect::<Vec<_>>();
    repositories.sort_by(|left, right| left.label.cmp(&right.label));
    repositories
}

fn diagnostics_dir() -> std::path::PathBuf {
    lazybox_core::paths::state_root().join("desktop-crashes")
}

fn analytics_path() -> std::path::PathBuf {
    lazybox_core::paths::state_root().join("desktop-analytics.ndjson")
}

fn append_analytics_event(path: &Path, event: AnalyticsEvent) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = serde_json::json!({ "event": event, "timestamp": timestamp });
    writeln!(file, "{line}")
}

fn diagnostic_body(location: Option<&std::panic::Location<'_>>) -> String {
    let location = location
        .map(|location| format!("{}:{}", location.file(), location.line()))
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "lazybox desktop crash\nversion={}\nplatform={}-{}\nlocation={location}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn install_crash_diagnostics() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let directory = diagnostics_dir();
        if std::fs::create_dir_all(&directory).is_ok() {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let path = directory.join(format!("crash-{timestamp}.txt"));
            let _ = std::fs::write(path, diagnostic_body(panic_info.location()));
        }
        original(panic_info);
    }));
}

#[cfg(target_os = "macos")]
fn import_login_shell_path() {
    let shell = lazybox_config::ShellSection::default().resolved_command();
    let Ok(mut child) = std::process::Command::new(shell)
        .args(["-l", "-c", "printf '\\n__LAZYBOX_PATH__%s\\n' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
    let Ok(output) = child.wait_with_output() else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(path) = extract_login_shell_path(&stdout) else {
        return;
    };
    // This is main's first process mutation, before Tauri or Tokio starts
    // threads that can read the environment.
    unsafe {
        std::env::set_var("PATH", path);
    }
}

#[cfg(not(target_os = "macos"))]
fn import_login_shell_path() {}

fn extract_login_shell_path(output: &str) -> Option<&str> {
    output
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("__LAZYBOX_PATH__"))
        .map(str::trim)
        .filter(|path| !path.is_empty())
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

    let agents = configured_agent_ids(&user_config);
    let default_agent = effective_default_agent(&user_config);
    let repositories = configured_repositories(&user_config);
    let (terminal_commands, terminal_command_rx) = mpsc::channel(256);
    let (terminal_tx, terminal_rx) = mpsc::channel(32);

    Ok(DesktopState {
        gateway,
        agents,
        default_agent,
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
    install_crash_diagnostics();
    import_login_shell_path();
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
            desktop_setup_state,
            github_auth_status,
            begin_github_login,
            list_github_repositories,
            save_desktop_settings,
            record_analytics,
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
    use lazybox_store::WorkspaceRecord;

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
    fn github_auth_status_cannot_serialize_credential_material() {
        let status = GithubAuthStatus {
            authenticated: true,
            account: Some("octocat".to_string()),
            message: "GitHub credential verified".to_string(),
        };

        let value = serde_json::to_value(status).expect("serialize status");

        assert_eq!(value["account"], "octocat");
        assert!(value.get("token").is_none());
        assert!(value.get("credential").is_none());
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
        let session_key = lazybox_core::SessionKey::from("github:o/r#1");
        let command = Command::from(DesktopCommand::SpawnAgent {
            session_key: session_key.clone(),
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
        assert!(matches!(
            Command::from(DesktopCommand::SpawnShell {
                session_key: session_key.clone(),
            }),
            Command::Spawn {
                kind: lazybox_ipc::TerminalKind::Shell,
                cwd: None,
                initial_prompt: None,
                on_main: false,
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
                session_key: session_key.clone(),
                body: "reply".to_string(),
            }),
            Command::PostReply { session_key: key, body }
                if key == session_key && body == "reply"
        ));
        assert!(matches!(
            Command::from(DesktopCommand::CreateWorkspace {
                name: "first workspace".to_string(),
                project_key: lazybox_core::ProjectKey::github("acme", "widget"),
                agent: Some("codex".to_string()),
            }),
            Command::CreateWorkspace {
                name,
                project_key,
                spawn_agent: Some(agent),
            } if name == "first workspace"
                && project_key == lazybox_core::ProjectKey::github("acme", "widget")
                && agent == "codex"
        ));
    }

    #[test]
    fn desktop_settings_round_trip_through_the_shared_config_model() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.yaml");
        let mut config = lazybox_config::Config::default();
        config.setup.providers.insert("linear".to_string());
        config.save_to(&path).expect("seed config");

        let scopes = validate_github_scopes(
            vec![
                " github:acme/widget ".to_string(),
                "github:acme".to_string(),
            ],
            true,
        )
        .expect("valid repository scopes");
        let mut config = lazybox_config::Config::load_from(&path).expect("load seeded config");
        apply_desktop_settings(&mut config, scopes, "codex".to_string(), true);
        config.save_to(&path).expect("persist desktop setup");

        let saved = lazybox_config::Config::load_from(&path).expect("reload desktop setup");
        assert!(saved.setup.providers.contains("linear"));
        assert!(saved.setup.providers.contains("github"));
        assert_eq!(saved.setup.default_agent.as_deref(), Some("codex"));
        assert!(saved.setup.agents.contains("codex"));
        assert!(saved.setup.wizard_completed);
        assert!(saved.desktop.analytics_enabled);
        assert_eq!(
            saved.setup.scopes.get("github"),
            Some(&BTreeSet::from([
                "github:acme".to_string(),
                "github:acme/widget".to_string(),
            ]))
        );
        assert!(
            saved
                .setup
                .filters
                .get("github")
                .is_some_and(|filters| !filters.is_empty())
        );
    }

    #[test]
    fn setup_state_reads_the_current_config_and_defaults_like_the_tui() {
        let mut config = lazybox_config::Config::default();
        config.setup.agents.insert("codex".to_string());
        config.agents.insert(
            "review-bot".to_string(),
            lazybox_config::AgentEntry {
                name: Some("Review Bot".to_string()),
                command: Some("true".to_string()),
                ..Default::default()
            },
        );
        // No explicit command: availability must resolve from the agent
        // id itself (`true` is a real binary), not a hardcoded name map.
        config.agents.insert(
            "true".to_string(),
            lazybox_config::AgentEntry {
                name: Some("Truthy".to_string()),
                command: None,
                ..Default::default()
            },
        );

        let initial = desktop_setup_state_from_config(&config);
        assert_eq!(initial.default_agent, "claude");
        assert!(configured_agent_ids(&config).contains(&"claude".to_string()));
        assert!(initial.agents.iter().any(|agent| agent.id == "review-bot"
            && agent.label == "Review Bot"
            && agent.available));
        assert!(
            initial
                .agents
                .iter()
                .any(|agent| agent.id == "true" && agent.label == "Truthy" && agent.available)
        );

        config.setup.default_agent = Some("cursor-agent".to_string());
        let changed = desktop_setup_state_from_config(&config);
        assert_eq!(changed.default_agent, "cursor-agent");
        assert!(configured_agent_ids(&config).contains(&"cursor-agent".to_string()));
        assert!(
            changed
                .agents
                .iter()
                .any(|agent| agent.id == "cursor-agent" && agent.label == "Cursor Agent")
        );
    }

    #[test]
    fn desktop_settings_accept_shared_github_scope_semantics() {
        assert!(
            validate_github_scopes(Vec::new(), false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            validate_github_scopes(vec!["github:acme".to_string()], true).unwrap(),
            BTreeSet::from(["github:acme".to_string()])
        );
        assert!(validate_github_scopes(Vec::new(), true).is_err());
        assert!(
            validate_github_scopes(vec!["github:acme/widget/extra".to_string()], false).is_err()
        );
        assert!(validate_github_scopes(vec!["linear:acme/widget".to_string()], false).is_err());
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

    #[test]
    fn analytics_records_only_the_fixed_event_and_timestamp() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("events.ndjson");

        append_analytics_event(&path, AnalyticsEvent::ReplyPosted).expect("record analytics");

        let line = std::fs::read_to_string(path).expect("read analytics");
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("parse analytics");
        assert_eq!(value["event"], "reply_posted");
        assert!(value["timestamp"].is_u64());
        assert_eq!(value.as_object().map(serde_json::Map::len), Some(2));
    }

    #[test]
    fn analytics_write_failures_never_escape_the_optional_boundary() {
        let directory = tempfile::tempdir().expect("tempdir");
        let blocking_file = directory.path().join("not-a-directory");
        std::fs::write(&blocking_file, "block child creation").expect("write blocking file");

        record_analytics_best_effort(
            &blocking_file.join("events.ndjson"),
            true,
            AnalyticsEvent::WorkspaceOpened,
        );
    }

    #[test]
    fn crash_diagnostic_has_no_runtime_or_provider_content() {
        let body = diagnostic_body(None);

        assert!(body.contains("lazybox desktop crash"));
        assert!(body.contains("location=unknown"));
        assert!(!body.contains("token"));
        assert!(!body.contains("terminal"));
    }

    #[test]
    fn login_shell_path_uses_the_marker_after_shell_startup_output() {
        assert_eq!(
            extract_login_shell_path("startup noise\n__LAZYBOX_PATH__/opt/homebrew/bin:/usr/bin\n"),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(extract_login_shell_path("startup noise\n"), None);
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

    #[tokio::test]
    async fn credential_free_dogfood_flow_crosses_config_gateway_and_real_pty() {
        let directory = tempfile::tempdir().expect("temporary fixture directory");
        let config_path = directory.path().join("config.yaml");
        let mut persisted_config = lazybox_config::Config::default();
        apply_desktop_settings(
            &mut persisted_config,
            BTreeSet::from(["github:acme/widget".to_string()]),
            "claude".to_string(),
            false,
        );
        persisted_config
            .save_to(&config_path)
            .expect("persist desktop setup fixture");
        let reloaded =
            lazybox_config::Config::load_from(&config_path).expect("reload desktop setup fixture");
        assert_eq!(
            desktop_setup_state_from_config(&reloaded).selected_scopes,
            vec!["github:acme/widget"]
        );

        let config = ServerConfig::in_memory();
        let workspace_key = lazybox_core::WorkspaceKey::new("desktop-dogfood");
        let mut workspace =
            lazybox_core::Workspace::empty(workspace_key.clone(), "main", chrono::Utc::now());
        workspace.local = true;
        workspace.linked_checkout = Some(directory.path().to_path_buf());
        workspace.activity.push(lazybox_core::Activity {
            author: "fixture".to_string(),
            body: "Review the desktop flow".to_string(),
            created_at: chrono::Utc::now(),
            kind: lazybox_core::ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        config
            .store
            .save_workspace(&WorkspaceRecord {
                key: workspace_key.as_str().to_string(),
                created_at: workspace.created_at,
                workspace_json: Some(serde_json::to_string(&workspace).expect("workspace JSON")),
            })
            .expect("seed workspace");

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind dogfood gateway");
        let address = listener.local_addr().expect("dogfood gateway address");
        let options = GatewayOptions {
            bind_addr: address,
            bearer_token: Some("dogfood-token".to_string()),
            ..GatewayOptions::default()
        };
        let served_config = config.clone();
        let gateway_task = tokio::spawn(async move {
            lazybox_server::api_gateway::serve_listener(served_config, options, listener)
                .await
                .expect("serve dogfood gateway");
        });
        let gateway = GatewayClient {
            base_url: format!("http://{address}"),
            bearer_token: "dogfood-token".to_string(),
            client: Client::new(),
        };

        let listed = list_gateway_workspaces(&gateway)
            .await
            .expect("list fixture inbox");
        assert_eq!(listed.workspaces.len(), 1);
        send_gateway_command(
            &gateway,
            DesktopCommand::MarkRead {
                session_key: (&workspace_key).into(),
            },
        )
        .await
        .expect("mark fixture read");
        let listed = list_gateway_workspaces(&gateway)
            .await
            .expect("reload fixture inbox");
        assert_eq!(listed.workspaces[0].unread_count(), 0);

        let reply_error = send_gateway_command(
            &gateway,
            DesktopCommand::PostReply {
                session_key: (&workspace_key).into(),
                body: "Credential-free fixture reply".to_string(),
            },
        )
        .await
        .expect_err("local workspace cannot silently accept an external reply");
        assert!(!reply_error.is_empty());

        send_gateway_command(
            &gateway,
            DesktopCommand::SpawnShell {
                session_key: (&workspace_key).into(),
            },
        )
        .await
        .expect("spawn fixture shell");
        let terminal_id = config
            .terminal
            .terminal_ids()
            .await
            .into_iter()
            .next()
            .expect("shell registered through gateway");
        let backend_key = config
            .terminal
            .backend_key_for(terminal_id)
            .await
            .expect("shell has replay backend");
        let snapshot = config
            .backend
            .snapshot(&backend_key)
            .await
            .expect("recover shell replay");
        assert!(snapshot.complete);

        config
            .backend
            .kill(&backend_key)
            .await
            .expect("stop fixture shell");
        gateway_task.abort();
        let _ = gateway_task.await;
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
