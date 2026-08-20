#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod snippets;

use bytes::Bytes;
use lazybox_core::ProviderConfig;
use lazybox_ipc::{AgentState, TerminalId};
use lazybox_server::ServerConfig;
use lazybox_server::api_gateway::{
    CLIENT_REQUEST_ID_HEADER, CommandResponse, DESKTOP_PROTOCOL_FINGERPRINT,
    DESKTOP_PROTOCOL_VERSION, DESKTOP_TERMINAL_STREAM_ITEM_DATA,
    DESKTOP_TERMINAL_STREAM_ITEM_DISCONNECTED, DESKTOP_TERMINAL_STREAM_ITEM_RESET,
    DesktopAgentInfo, DesktopAttentionSettings, DesktopCommand, DesktopDaemonSettings,
    DesktopEvent, DesktopEventFrame, DesktopInboxView, DesktopInfo, DesktopModelTier,
    DesktopRepository, DesktopStreamMessage, GatewayOptions, JsonClientFrame,
    PROTOCOL_FINGERPRINT_HEADER, PROTOCOL_VERSION_HEADER, ProtocolResponse,
    TERMINAL_BINARY_CONTENT_TYPE, UnsupportedProtocolResponse, WorkspacesResponse, desktop_event,
};
use lazybox_server::client_runtime::{ClientRuntime, ClientRuntimeOptions};
use lazybox_server::lifecycle::{self, ServerStatus};
use lazybox_server::socket_service::SocketService;
use lazybox_tui_core::inbox::{
    self, ComputeInputs, Filter, FilterSet, Mailbox, SearchState, SortMode, mailbox_membership,
};
use lazybox_tui_core::snippets::{PickerRow, SnippetPickerView};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use snippets::SnippetModel;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, watch};

enum TerminalStreamItem {
    Reset,
    Data(Bytes),
    Disconnected(String),
}

/// Desktop-side state-of-record for the grouped inbox (#732). The
/// `src-tauri` layer maintains the workspace + agent maps from gateway
/// events and calls the shared, client-free
/// [`lazybox_tui_core::inbox::compute_visible`] — the exact code the
/// ratatui TUI builds its sidebar from — so the desktop and TUI can't
/// drift on grouping or sort. The webview is a thin renderer over the
/// emitted [`DesktopInboxView`].
struct InboxModel {
    revision: u64,
    controller_id: String,
    filter_generation: u64,
    search_generation: u64,
    workspaces: HashMap<lazybox_core::SessionKey, lazybox_core::Workspace>,
    /// Per-terminal agent state, keyed by terminal id so one agent in a
    /// multi-session workspace can't clobber another (mirrors the TUI's
    /// `agent_terminal_states`). Aggregated into `agents` per session.
    agent_terminal_states: HashMap<TerminalId, (lazybox_core::SessionKey, AgentState)>,
    /// The derived per-session agent state that `compute_visible`'s
    /// attention scoring reads.
    agents: HashMap<lazybox_core::SessionKey, AgentState>,
    sort_mode: SortMode,
    /// Active mailbox (Inbox / Inactive / Snoozed), cycled by the
    /// frontend's mailbox control (#816).
    mailbox: Mailbox,
    attention: lazybox_config::AttentionConfig,
    /// Active filter set from the multi-select filter menu (#733).
    filters: FilterSet,
    /// Global free-text search query (empty = inactive). Fed into the
    /// shared search with a `None` scope so it filters every project,
    /// unlike the TUI's cursor-scoped `/` (#733).
    search: String,
}

impl InboxModel {
    fn new(attention: lazybox_config::AttentionConfig) -> Self {
        Self {
            revision: 0,
            controller_id: String::new(),
            filter_generation: 0,
            search_generation: 0,
            workspaces: HashMap::new(),
            agent_terminal_states: HashMap::new(),
            agents: HashMap::new(),
            sort_mode: SortMode::default(),
            mailbox: Mailbox::default(),
            attention,
            filters: FilterSet::new(),
            search: String::new(),
        }
    }

    /// Replace the active filter set (the multi-select filter menu's
    /// output). An empty list clears all filters (#733).
    fn set_filters(
        &mut self,
        controller_id: &str,
        generation: u64,
        filters: impl IntoIterator<Item = Filter>,
    ) -> bool {
        self.activate_controller(controller_id);
        if generation < self.filter_generation {
            return false;
        }
        self.filter_generation = generation;
        self.filters.replace(filters);
        self.revision += 1;
        true
    }

    /// Set the global search query; an empty/blank query is inactive (#733).
    fn set_search(&mut self, controller_id: &str, generation: u64, query: String) -> bool {
        self.activate_controller(controller_id);
        if generation < self.search_generation {
            return false;
        }
        self.search_generation = generation;
        self.search = query;
        self.revision += 1;
        true
    }

    fn activate_controller(&mut self, controller_id: &str) {
        if self.controller_id != controller_id {
            self.controller_id = controller_id.to_string();
            self.filter_generation = 0;
            self.search_generation = 0;
        }
    }

    /// Seed the workspace map from the initial `list_workspaces`
    /// response so the view (and `set_sort_mode`) works before the
    /// first `Snapshot` event arrives.
    fn seed_workspaces(&mut self, workspaces: &[lazybox_core::Workspace]) {
        self.workspaces = workspaces
            .iter()
            .map(|w| ((&w.key).into(), w.clone()))
            .collect();
        self.revision += 1;
    }

    /// Fold a desktop event into the model. Returns whether the inbox
    /// view should be recomputed + re-emitted.
    fn apply_event(&mut self, event: &DesktopEvent) -> bool {
        let recompute = match event {
            DesktopEvent::Snapshot {
                workspaces,
                terminals,
                ..
            } => {
                self.seed_workspaces(workspaces);
                self.agent_terminal_states.clear();
                for terminal in terminals {
                    if let Some(state) = terminal.agent_state {
                        self.agent_terminal_states
                            .insert(terminal.terminal_id, (terminal.session_key.clone(), state));
                    }
                }
                self.rebuild_agents();
                true
            }
            DesktopEvent::WorkspaceUpserted(workspace) => {
                self.workspaces
                    .insert((&workspace.key).into(), (**workspace).clone());
                true
            }
            DesktopEvent::WorkspaceRemoved(key) => {
                let session_key = lazybox_core::SessionKey::from(key);
                self.workspaces.remove(&session_key);
                self.agent_terminal_states
                    .retain(|_, (owner, _)| owner != &session_key);
                self.agents.remove(&session_key);
                true
            }
            DesktopEvent::AgentState {
                session_key,
                terminal_id,
                state,
            } => {
                // Re-emit only when this transition changes the set of
                // sessions the view actually reflects. `compute_visible`
                // reads agent state solely through `workspace_is_asking`
                // (the `InputNeeded` set), so a Working⇄Idle⇄Done flap
                // leaves the grouped view byte-identical and must not
                // churn the webview.
                let asking_before = self.asking_sessions();
                self.agent_terminal_states
                    .insert(*terminal_id, (session_key.clone(), *state));
                self.rebuild_agents();
                asking_before != self.asking_sessions()
            }
            DesktopEvent::TerminalExited { terminal_id, .. } => {
                if !self.agent_terminal_states.contains_key(terminal_id) {
                    return false;
                }
                // Same gate as `AgentState`: dropping a terminal only
                // needs a re-emit if it changes which sessions are asking.
                let asking_before = self.asking_sessions();
                self.agent_terminal_states.remove(terminal_id);
                self.rebuild_agents();
                asking_before != self.asking_sessions()
            }
            _ => false,
        };
        if recompute {
            self.revision += 1;
        }
        recompute
    }

    /// The sessions currently aggregated to `InputNeeded` (asking) —
    /// the only agent-derived signal `compute_visible` reflects, via
    /// `workspace_is_asking`. The event fold re-emits exactly when this
    /// set changes, matching the ratatui sidebar's `asking_changed` gate.
    fn asking_sessions(&self) -> HashSet<lazybox_core::SessionKey> {
        self.agents
            .iter()
            .filter(|(_, state)| matches!(state, AgentState::InputNeeded))
            .map(|(session_key, _)| session_key.clone())
            .collect()
    }

    /// Recompute the per-session agent state from the terminal-keyed
    /// states, with the TUI's attention precedence.
    fn rebuild_agents(&mut self) {
        self.agents.clear();
        let sessions: HashSet<lazybox_core::SessionKey> = self
            .agent_terminal_states
            .values()
            .map(|(session_key, _)| session_key.clone())
            .collect();
        for session_key in sessions {
            let aggregated = aggregate_agent_state(
                self.agent_terminal_states
                    .values()
                    .filter_map(|(owner, state)| (owner == &session_key).then_some(*state)),
            );
            if let Some(state) = aggregated {
                self.agents.insert(session_key, state);
            }
        }
    }

    fn cycle_sort_mode(&mut self) -> SortMode {
        self.sort_mode = self.sort_mode.next();
        self.revision += 1;
        self.sort_mode
    }

    fn cycle_mailbox(&mut self) -> Mailbox {
        self.mailbox = self.mailbox.next();
        self.revision += 1;
        self.mailbox
    }

    /// Run the shared grouping/sort logic and wrap it with the current
    /// sort mode for the frontend's sort control.
    fn compute(&self) -> DesktopInboxView {
        // Projects are not mirrored to the desktop yet; `group_label`
        // still derives owner/repo labels from `project_key`/`task.repo`,
        // so an empty map is correct here. Passing the real project
        // table (empty-repo headers, prettier Linear labels) is a
        // follow-up.
        let projects = BTreeMap::new();
        let now = chrono::Utc::now();
        // `scope: None` = global search across every project (the desktop
        // has no cursor-scoped repo like the TUI's `/`) (#733).
        let search = (!self.search.trim().is_empty()).then(|| SearchState {
            scope: None,
            query: self.search.clone(),
            editing: false,
        });
        // The filter menu counts over the workspaces the mailbox admits
        // *before* the active set narrows further — same candidate set the
        // count answers "what would this toggle surface" (#733).
        let candidates: Vec<&lazybox_core::Workspace> = self
            .workspaces
            .values()
            .filter(|w| mailbox_membership(w, self.mailbox, now, false))
            .collect();
        let filter_menu = Filter::menu(&candidates, &self.agents, &self.filters);
        let filter_chips: Vec<String> =
            self.filters.chips().iter().map(|c| c.to_string()).collect();
        // No Spaces collapse UI on the desktop yet; an empty set keeps
        // every Space expanded (#860).
        let collapsed_spaces = BTreeSet::new();
        let outcome = inbox::compute_visible(ComputeInputs {
            workspaces: &self.workspaces,
            mailbox: self.mailbox,
            filters: &self.filters,
            sort_mode: self.sort_mode,
            show_inactive_in_inbox: false,
            projects: &projects,
            collapsed_repos: &BTreeSet::new(),
            // The desktop client has no pin-to-top UI yet; the shared
            // builder honors pins when a caller supplies them (#760).
            pinned_repos: &[],
            // No star/focus UI yet either; the shared builder lifts
            // starred workspaces into the `★ Focused` section when a
            // caller supplies them (#846).
            focused_workspaces: &[],
            // No Spaces UI yet; the shared builder renders the grouping
            // tier only when a caller supplies ≥2 distinct Spaces (#860).
            spaces: &[],
            collapsed_spaces: &collapsed_spaces,
            // No ticket-collapse UI on the desktop yet; an empty set keeps
            // every ticket's children expanded (#1189).
            collapsed_tickets: &std::collections::HashSet::new(),
            attention: &self.attention,
            agents: &self.agents,
            now,
            search: search.as_ref(),
        });
        DesktopInboxView {
            revision: self.revision,
            outcome,
            sort_mode: self.sort_mode,
            mailbox: self.mailbox,
            filter_menu,
            filter_chips,
        }
    }
}

/// Reduce several per-terminal agent states for one session into the
/// single value the sidebar shows, with `InputNeeded` (attention)
/// winning. Mirrors the TUI's `aggregate_agent_state`.
fn aggregate_agent_state(states: impl Iterator<Item = AgentState>) -> Option<AgentState> {
    states.max_by_key(|state| match state {
        // A usage-limit block outranks even `InputNeeded` — the most
        // urgent "act externally before this moves" state (#847).
        AgentState::LimitReached => 6,
        AgentState::InputNeeded => 5,
        AgentState::Working => 4,
        AgentState::Done => 3,
        AgentState::Exited { .. } => 2,
        AgentState::Idle => 1,
    })
}

#[derive(Clone)]
struct GatewayClient {
    base_url: String,
    bearer_token: String,
    client: Client,
    stream_client: Client,
}

struct DesktopState {
    gateway: GatewayClient,
    authority: DesktopAuthority,
    providers: Vec<String>,
    agents: Vec<DesktopAgentInfo>,
    default_agent: String,
    repositories: Vec<DesktopRepository>,
    daemon_settings: DesktopDaemonSettings,
    terminal_commands: mpsc::Sender<Bytes>,
    terminal_command_rx: Arc<Mutex<mpsc::Receiver<Bytes>>>,
    terminal_rx: Mutex<mpsc::Receiver<TerminalStreamItem>>,
    terminal_tx: mpsc::Sender<TerminalStreamItem>,
    streams_started: AtomicBool,
    stream_shutdown: watch::Sender<bool>,
    stream_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The controller (webview instance) whose `read_terminal_data` drain
    /// loop and filter/search requests are authoritative (#974). A stale
    /// controller's requests are dropped; changing it wakes the parked
    /// terminal reader (below).
    active_controller: Mutex<Option<String>>,
    /// Wakes any parked `read_terminal_data` call when the active controller
    /// changes (subscribe / unsubscribe). Without it a superseded reader
    /// holds the shared `terminal_rx` lock across `recv().await` forever,
    /// pinning the whole reader loop alive and starving a re-initialized
    /// controller of terminal frames (#974).
    terminal_reader_wake: Arc<tokio::sync::Notify>,
    client_runtime: Mutex<Option<ClientRuntime>>,
    gateway_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    gateway_shutdown: Mutex<Option<watch::Sender<bool>>>,
    socket_task: Mutex<Option<tokio::task::JoinHandle<Result<(), String>>>>,
    socket_shutdown: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// A tolerated protocol-skew advisory captured at startup (#815): set
    /// when the daemon speaks a compatible protocol version but a different
    /// build fingerprint. Surfaced to the UI through `desktop_info`.
    protocol_notice: Option<String>,
    /// Grouped-inbox state-of-record (#732). Shared between the event
    /// stream (which folds in workspace/agent changes and re-emits the
    /// view) and the `set_sort_mode` command.
    inbox: Arc<Mutex<InboxModel>>,
    /// The live webview channel, stored so `set_sort_mode` can push a
    /// recomputed inbox view on the same channel the event stream uses.
    event_channel: Arc<RwLock<Option<Channel<DesktopStreamMessage>>>>,
    /// State-of-record for the snippet picker (#734): the catalog plus the
    /// daemon-owned MRU, reduced from the control stream. The frontend
    /// pulls a recomputed view per keystroke via `snippet_view`.
    snippets: Arc<Mutex<SnippetModel>>,
    event_handoff: Arc<Mutex<EventHandoff>>,
    conventions: lazybox_core::Conventions,
}

#[derive(Clone, Copy)]
enum EventSource {
    Live,
    Response,
}

#[derive(Default)]
struct EventHandoff {
    live: VecDeque<String>,
    responses: VecDeque<String>,
}

impl EventHandoff {
    fn accept(&mut self, source: EventSource, event: &DesktopEvent) -> bool {
        let Ok(key) = serde_json::to_string(event) else {
            return true;
        };
        let (own, opposite) = match source {
            EventSource::Live => (&mut self.live, &mut self.responses),
            EventSource::Response => (&mut self.responses, &mut self.live),
        };
        if let Some(index) = opposite.iter().position(|candidate| candidate == &key) {
            opposite.remove(index);
            return false;
        }
        own.push_back(key);
        if own.len() > 256 {
            own.pop_front();
        }
        true
    }
}

#[derive(Clone, Debug, Serialize)]
struct DesktopAgentOption {
    id: String,
    label: String,
    available: bool,
    /// The agent's model-tier menu (`alias → label`), resolved through
    /// the shared [`lazybox_config::Config::agent_models`] so it matches
    /// what a TUI spawn chord would offer. Empty for agents with no menu.
    models: Vec<DesktopModelTier>,
    /// Alias of the tier a bare spawn currently defaults to, if any.
    default_tier: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DesktopAuthority {
    Embedded,
    Remote,
}

#[derive(Clone, Debug, Serialize)]
struct DesktopThemeColors {
    accent: String,
    hover: String,
    success: String,
    warn: String,
    error: String,
    text_strong: String,
    text_dim: String,
    chrome: String,
    fill: String,
    surface: String,
}

#[derive(Clone, Debug, Serialize)]
struct DesktopThemeOption {
    name: String,
    colors: DesktopThemeColors,
}

#[derive(Serialize)]
struct DesktopSetupState {
    authority: DesktopAuthority,
    providers: Vec<String>,
    first_run: bool,
    selected_scopes: Vec<String>,
    agents: Vec<DesktopAgentOption>,
    default_agent: String,
    analytics_enabled: bool,
    diagnostics_path: String,
    log_path: String,
    /// Active desktop theme name, or `None` for the default theme.
    theme: Option<String>,
    /// The built-in theme catalog (name + palette) the client renders
    /// swatches from — sourced from `lazybox_tui_core::theme`, never
    /// hardcoded in the frontend.
    themes: Vec<DesktopThemeOption>,
    /// Active `ui.keymap_preset`, surfaced read-only (a full remap UI is
    /// out of scope).
    keymap_preset: Option<String>,
    collapsed_repos: Vec<String>,
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
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    default_model_tier: Option<String>,
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
        providers: state.providers.clone(),
        agents: state.agents.clone(),
        default_agent: state.default_agent.clone(),
        repositories: state.repositories.clone(),
        settings: state.daemon_settings.clone(),
        protocol_notice: state.protocol_notice.clone(),
    }
}

#[tauri::command]
fn desktop_setup_state(state: State<'_, DesktopState>) -> Result<DesktopSetupState, String> {
    let config = lazybox_config::Config::load()
        .map_err(|error| format!("load lazybox configuration: {error}"))?;
    Ok(match state.authority {
        DesktopAuthority::Embedded => desktop_setup_state_from_config(&config),
        DesktopAuthority::Remote => desktop_setup_state_for_remote(
            &config,
            &state.providers,
            &state.agents,
            &state.default_agent,
            &state.daemon_settings,
        ),
    })
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
        authority: DesktopAuthority::Embedded,
        providers: config.setup.providers.iter().cloned().collect(),
        first_run,
        selected_scopes,
        agents: detect_agent_options(config),
        default_agent: effective_default_agent(config),
        analytics_enabled: config.desktop.analytics_enabled,
        diagnostics_path: diagnostics_dir().display().to_string(),
        log_path: desktop_log_path().display().to_string(),
        theme: config.desktop.theme.clone(),
        themes: theme_options(),
        keymap_preset: config.ui.keymap_preset.clone(),
        collapsed_repos: config.ui.collapsed_repos.iter().cloned().collect(),
    }
}

fn desktop_setup_state_for_remote(
    config: &lazybox_config::Config,
    providers: &[String],
    agents: &[DesktopAgentInfo],
    default_agent: &str,
    settings: &DesktopDaemonSettings,
) -> DesktopSetupState {
    DesktopSetupState {
        authority: DesktopAuthority::Remote,
        providers: providers.to_vec(),
        first_run: false,
        selected_scopes: settings.github_scopes.clone(),
        agents: agents
            .iter()
            .map(|agent| DesktopAgentOption {
                id: agent.id.clone(),
                label: agent.label.clone(),
                available: true,
                models: agent.models.clone(),
                default_tier: agent.default_tier.clone(),
            })
            .collect(),
        default_agent: default_agent.to_string(),
        analytics_enabled: config.desktop.analytics_enabled,
        diagnostics_path: diagnostics_dir().display().to_string(),
        log_path: desktop_log_path().display().to_string(),
        theme: config.desktop.theme.clone(),
        themes: theme_options(),
        keymap_preset: settings.keymap_preset.clone(),
        // Remote collapse is client-owned (see `set_repo_collapsed`).
        collapsed_repos: config.desktop.collapsed_repos.iter().cloned().collect(),
    }
}

/// The built-in theme catalog exposed to the client — names plus the
/// per-slot hex colors, sourced from the shared `lazybox_tui_core`
/// palette so the desktop renders swatches without redeclaring colors.
fn theme_options() -> Vec<DesktopThemeOption> {
    use lazybox_tui_core::theme::{BUILT_IN_PALETTES, ThemePalette};
    BUILT_IN_PALETTES
        .iter()
        .map(|palette| DesktopThemeOption {
            name: palette.name.to_string(),
            colors: DesktopThemeColors {
                accent: ThemePalette::hex(palette.accent),
                hover: ThemePalette::hex(palette.hover),
                success: ThemePalette::hex(palette.success),
                warn: ThemePalette::hex(palette.warn),
                error: ThemePalette::hex(palette.error),
                text_strong: ThemePalette::hex(palette.text_strong),
                text_dim: ThemePalette::hex(palette.text_dim),
                chrome: ThemePalette::hex(palette.chrome),
                fill: ThemePalette::hex(palette.fill),
                surface: ThemePalette::hex(palette.surface),
            },
        })
        .collect()
}

fn theme_exists(name: &str) -> bool {
    lazybox_tui_core::theme::BUILT_IN_PALETTES
        .iter()
        .any(|palette| palette.name == name)
}

/// Reject a *newly chosen* theme the desktop can't render, but never
/// reject one that only carries the already-persisted value forward. A
/// theme registered outside the built-in catalog (e.g. a TUI plugin
/// palette) stays selected here, matching the TUI's lenient "unknown →
/// leave as-is" instead of blocking every save until a built-in is
/// picked.
fn validate_theme_change(new: Option<&str>, current: Option<&str>) -> Result<(), String> {
    match new {
        Some(theme) if new != current && !theme_exists(theme) => {
            Err(format!("unknown theme {theme:?}"))
        }
        _ => Ok(()),
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
fn save_desktop_settings(
    state: State<'_, DesktopState>,
    app: AppHandle,
    settings: SaveDesktopSettings,
) -> Result<bool, String> {
    let config = lazybox_config::Config::load()
        .map_err(|error| format!("load lazybox configuration: {error}"))?;
    validate_theme_change(settings.theme.as_deref(), config.desktop.theme.as_deref())?;
    if state.authority == DesktopAuthority::Remote {
        let analytics_enabled = settings.analytics_enabled;
        lazybox_config::Config::save_with(move |config| {
            config.desktop.analytics_enabled = analytics_enabled;
            config.desktop.theme = settings.theme;
        })
        .map_err(|error| format!("save desktop preferences: {error}"))?;
        return Ok(false);
    }
    let first_run = !config.setup.wizard_completed || !config.setup.providers.contains("github");
    let scopes = validate_github_scopes(settings.github_scopes, first_run)?;
    if !detect_agent_options(&config)
        .iter()
        .any(|agent| agent.id == settings.default_agent && agent.available)
    {
        return Err("select an installed agent".to_string());
    }
    if let Some(alias) = settings.default_model_tier.as_deref()
        && config
            .agent_models(&settings.default_agent)
            .tier(alias)
            .is_none()
    {
        return Err(format!(
            "unknown model tier {alias:?} for agent {:?}",
            settings.default_agent
        ));
    }
    let previous_scopes = config
        .setup
        .scopes
        .get("github")
        .cloned()
        .unwrap_or_default();
    let restart_required = first_run
        || scopes != previous_scopes
        || settings.default_agent != effective_default_agent(&config);
    let analytics_enabled = settings.analytics_enabled;
    let applied = DesktopSettings {
        scopes,
        default_agent: settings.default_agent,
        analytics_enabled,
        theme: settings.theme,
        default_model_tier: settings.default_model_tier,
    };
    lazybox_config::Config::save_with(move |config| {
        apply_desktop_settings(config, applied);
    })
    .map_err(|error| format!("save lazybox configuration: {error}"))?;
    if analytics_enabled
        && let Err(error) =
            append_analytics_event(&analytics_path(), AnalyticsEvent::OnboardingCompleted)
    {
        eprintln!("lazybox desktop could not record onboarding analytics: {error}");
    }
    if restart_required {
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            app.request_restart();
        });
    }
    Ok(restart_required)
}

#[tauri::command]
fn set_repo_collapsed(
    state: State<'_, DesktopState>,
    repo: String,
    collapsed: bool,
) -> Result<(), String> {
    let repo = repo.trim().to_string();
    if repo.is_empty() {
        return Err("repository label cannot be empty".to_string());
    }
    let remote = state.authority == DesktopAuthority::Remote;
    lazybox_config::Config::save_with(move |config| {
        apply_repo_collapse(config, remote, repo, collapsed);
    })
    .map_err(|error| format!("save collapsed repository state: {error}"))
}

/// Persist one repo's collapse state into the set that owns it. Collapse
/// follows the same authority split as the rest of settings: embedded
/// shares the local TUI's `ui.collapsed_repos` (same machine, so the two
/// clients interoperate — the #971 acceptance criterion), while remote
/// writes the client-owned `desktop.collapsed_repos` so a remote repo's
/// collapse never contaminates a same-named repo in the laptop's TUI set.
fn apply_repo_collapse(
    config: &mut lazybox_config::Config,
    remote: bool,
    repo: String,
    collapsed: bool,
) {
    let set = if remote {
        &mut config.desktop.collapsed_repos
    } else {
        &mut config.ui.collapsed_repos
    };
    if collapsed {
        set.insert(repo);
    } else {
        set.remove(&repo);
    }
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
    let baseline_revision = state.inbox.lock().await.revision;
    let response = list_gateway_workspaces(&state.gateway).await?;
    // Seed the grouped-inbox model so `set_sort_mode` and the first
    // computed view have data even before the `Snapshot` event lands.
    let mut inbox = state.inbox.lock().await;
    if inbox.revision == baseline_revision {
        inbox.seed_workspaces(&response.workspaces);
    }
    Ok(response)
}

/// Cycle the inbox sort mode (`split → recent → by-role → …`) and push
/// a recomputed grouped view to the webview. The order itself is
/// computed only by the shared `tui-core` logic.
#[tauri::command]
async fn set_sort_mode(state: State<'_, DesktopState>) -> Result<(), String> {
    let view = {
        let mut inbox = state.inbox.lock().await;
        inbox.cycle_sort_mode();
        inbox.compute()
    };
    send_webview_event(
        &state.event_channel,
        DesktopStreamMessage::Inbox(Box::new(view)),
    );
    Ok(())
}

/// Cycle the active mailbox (Inbox → Inactive → Snoozed) and push the
/// recomputed grouped view to the webview (#816). The mailbox membership
/// itself is computed only by the shared `tui-core` logic.
#[tauri::command]
async fn set_mailbox(state: State<'_, DesktopState>) -> Result<(), String> {
    let view = {
        let mut inbox = state.inbox.lock().await;
        inbox.cycle_mailbox();
        inbox.compute()
    };
    send_webview_event(
        &state.event_channel,
        DesktopStreamMessage::Inbox(Box::new(view)),
    );
    Ok(())
}

/// Recompute the snippet picker view for `filter` from the catalog and
/// the daemon-owned MRU. Called on open and on every keystroke; the
/// shared `tui-core::snippets` logic does the grouping / filter / recent
/// float / auto-submit so the desktop matches the TUI picker (#734).
#[tauri::command]
async fn snippet_view(
    state: State<'_, DesktopState>,
    filter: String,
) -> Result<SnippetPickerView, String> {
    Ok(state.snippets.lock().await.view(&filter))
}

/// Replace the active filter set from the multi-select filter menu and
/// re-emit the recomputed view. An empty list clears all filters (#733).
#[tauri::command]
async fn set_filters(
    state: State<'_, DesktopState>,
    filters: Vec<Filter>,
    generation: u64,
    controller_id: String,
) -> Result<(), String> {
    // Drop a request from a superseded controller, and (via the generation)
    // any request older than the newest already applied (#974).
    if state.active_controller.lock().await.as_deref() != Some(&controller_id) {
        return Ok(());
    }
    let view = {
        let mut inbox = state.inbox.lock().await;
        inbox
            .set_filters(&controller_id, generation, filters)
            .then(|| inbox.compute())
    };
    if let Some(view) = view {
        send_webview_event(
            &state.event_channel,
            DesktopStreamMessage::Inbox(Box::new(view)),
        );
    }
    Ok(())
}

/// Set the global search query and re-emit the recomputed view. An
/// empty query clears the search (#733).
#[tauri::command]
async fn set_search(
    state: State<'_, DesktopState>,
    query: String,
    generation: u64,
    controller_id: String,
) -> Result<(), String> {
    if state.active_controller.lock().await.as_deref() != Some(&controller_id) {
        return Ok(());
    }
    let view = {
        let mut inbox = state.inbox.lock().await;
        inbox
            .set_search(&controller_id, generation, query)
            .then(|| inbox.compute())
    };
    if let Some(view) = view {
        send_webview_event(
            &state.event_channel,
            DesktopStreamMessage::Inbox(Box::new(view)),
        );
    }
    Ok(())
}

async fn list_gateway_workspaces(gateway: &GatewayClient) -> Result<WorkspacesResponse, String> {
    let response = gateway
        .authorized(
            gateway
                .client
                .get(gateway.url("/v1/workspaces"))
                .timeout(Duration::from_secs(30)),
        )
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
    let events = send_gateway_command(&state.gateway, command).await?;
    for event in events {
        if !forward_desktop_event(
            event,
            EventSource::Response,
            &state.event_channel,
            &state.inbox,
            &state.snippets,
            &state.event_handoff,
        )
        .await
        {
            break;
        }
    }
    Ok(())
}

/// Whether a command response's echoed correlation id is acceptable. A
/// missing echo (a proxy stripped the header) is fine — HTTP already pairs
/// the reply with the request; only a present, different id signals a
/// genuinely crossed reply.
fn correlation_echo_ok(returned: Option<&str>, expected: &str) -> bool {
    returned.is_none_or(|returned| returned == expected)
}

#[tauri::command]
async fn resolve_work_prompt(
    state: State<'_, DesktopState>,
    session_key: lazybox_core::SessionKey,
    selected_activity: Vec<usize>,
    agent: String,
) -> Result<Option<String>, String> {
    let inbox = state.inbox.lock().await;
    let workspace = inbox
        .workspaces
        .get(&session_key)
        .ok_or_else(|| format!("workspace {session_key} is no longer available"))?;
    match lazybox_tui_core::intent::resolve_work(
        Some(workspace),
        &selected_activity,
        &agent,
        &state.conventions,
    ) {
        lazybox_tui_core::intent::Intent::SpawnAgent { prompt, .. } => Ok(prompt),
        lazybox_tui_core::intent::Intent::Notice(message) => Err(message),
        _ => Err("nothing to work on here".to_string()),
    }
}

async fn send_gateway_command(
    gateway: &GatewayClient,
    command: DesktopCommand,
) -> Result<Vec<DesktopEvent>, String> {
    let client_request_id = uuid::Uuid::new_v4().simple().to_string();
    let command = command.into_correlated(Some(client_request_id.clone()));
    let response = gateway
        .authorized(
            gateway
                .client
                .post(gateway.url("/v1/commands"))
                .header(CLIENT_REQUEST_ID_HEADER, &client_request_id)
                .json(&JsonClientFrame::Command(command))
                .timeout(Duration::from_secs(5 * 60 + 5)),
        )
        .send()
        .await
        .map_err(|error| format!("send command: {error}"))?;
    let response: CommandResponse = decode_response(response).await?;
    if response.ok && response.completed {
        // The correlation id is advisory: HTTP already pairs this response
        // with this request on the connection. Fail only on a *present and
        // different* id (a genuinely crossed reply); a missing echo — e.g. a
        // proxy that stripped the `x-lazybox-client-request-id` header — must
        // not turn a successful command into a spurious error.
        if !correlation_echo_ok(response.client_request_id.as_deref(), &client_request_id) {
            return Err("daemon returned a mismatched command response".to_string());
        }
        let events = response
            .events
            .into_iter()
            .filter_map(desktop_event)
            .collect::<Vec<_>>();
        if let Some(message) = correlated_command_failure(&events, &client_request_id) {
            return Err(message);
        }
        Ok(events)
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "daemon did not complete the desktop command".to_string()))
    }
}

/// Convert the daemon's correlated failure event into the command call's
/// actual result. Returning `Ok` here let the webview close its create modal
/// and paint "Creating…" over the failure it had just received. When durable
/// creation preceded an agent-spawn failure, name that partial success so a
/// retry cannot accidentally create a duplicate workspace.
fn correlated_command_failure(events: &[DesktopEvent], request_id: &str) -> Option<String> {
    let failure = events.iter().find_map(|event| match event {
        DesktopEvent::CommandFailed {
            client_request_id,
            message,
        } if client_request_id == request_id => Some(message.as_str()),
        _ => None,
    })?;
    let created = events.iter().find_map(|event| match event {
        DesktopEvent::WorkspaceCreated {
            client_request_id,
            workspace_key,
        } if client_request_id == request_id => Some(workspace_key),
        _ => None,
    });
    Some(match created {
        Some(workspace_key) => {
            format!(
                "workspace {workspace_key} was created, but its agent failed to start: {failure}"
            )
        }
        None => failure.to_string(),
    })
}

/// Open a task URL in the user's default browser (the TUI's `g o`).
/// Reuses the shared platform launcher so the desktop and TUI behave
/// identically; fire-and-forget, so a spawn failure surfaces as an error
/// the frontend flashes (#816).
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    lazybox_tui_core::editors::open_url(&url, None).map_err(|error| format!("open {url}: {error}"))
}

/// The URL of the browser web-control client (`api_client.html`), served
/// at the root of the gateway this desktop is attached to. Splitting the
/// derivation out keeps it unit-testable without a running gateway.
fn web_control_url(base_url: &str) -> String {
    format!("{}/", base_url.trim_end_matches('/'))
}

/// Open the browser web-control client in the user's default browser. It
/// drives the same `/v1` gateway this desktop uses — embedded loopback or
/// a remote, SSH-forwarded one — so web control is a first-class peer of
/// this window, not a separate un-chromed page. Returns the opened URL so
/// the frontend can flash it.
#[tauri::command]
fn open_web_control(state: State<'_, DesktopState>) -> Result<String, String> {
    let url = web_control_url(&state.gateway.base_url);
    lazybox_tui_core::editors::open_url(&url, None)
        .map_err(|error| format!("open {url}: {error}"))?;
    Ok(url)
}

#[tauri::command]
async fn open_workspace_editor(
    state: State<'_, DesktopState>,
    session_key: lazybox_core::SessionKey,
) -> Result<String, String> {
    let worktree = {
        let inbox = state.inbox.lock().await;
        let workspace = inbox
            .workspaces
            .get(&session_key)
            .ok_or_else(|| format!("workspace {session_key} is no longer available"))?;
        workspace
            .sessions
            .iter()
            .max_by_key(|session| session.created_at)
            .map(|session| session.worktree_path.clone())
            .or_else(|| workspace.linked_checkout.clone())
            .ok_or_else(|| "start an agent or shell to create this workspace first".to_string())?
    };
    tokio::task::spawn_blocking(move || {
        let config = lazybox_config::Config::load()
            .map_err(|error| format!("load editor configuration: {error}"))?;
        let user = config
            .editors
            .into_iter()
            .map(|entry| lazybox_tui_core::editors::UserEditorEntry {
                id: entry.id,
                display: entry.display,
                command: entry.command,
                args: entry.args,
            })
            .collect();
        let editor = lazybox_tui_core::editors::discover_at_startup(user)
            .into_iter()
            .next()
            .ok_or_else(|| "no supported desktop editor is installed".to_string())?;
        lazybox_tui_core::editors::launch(&editor, Path::new(&worktree)).map_err(|error| {
            format!("open {} in {}: {error}", worktree.display(), editor.display)
        })?;
        Ok(editor.display)
    })
    .await
    .map_err(|error| format!("editor task failed: {error}"))?
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
    controller_id: String,
) -> Result<tauri::ipc::Response, String> {
    // Arm the wake before the first lock/recv so a supersede that races the
    // parking below is not missed (#974).
    let woken = state.terminal_reader_wake.notified();
    tokio::pin!(woken);
    loop {
        // A reader that no longer owns the active controller must not hold the
        // shared receiver: it would block the live controller's reader and
        // keep a disposed webview's loop alive.
        let superseded = {
            let active = state.active_controller.lock().await;
            active.as_deref() != Some(&controller_id)
        };
        if superseded {
            return Err("terminal reader superseded".to_string());
        }
        let mut rx = state.terminal_rx.lock().await;
        tokio::select! {
            item = rx.recv() => {
                let item = item.ok_or_else(|| "terminal stream stopped".to_string())?;
                return Ok(tauri::ipc::Response::new(encode_terminal_stream_item(item)));
            }
            () = &mut woken => {
                // subscribe/unsubscribe changed the active controller: drop the
                // lock, re-arm, and re-check ownership at the top.
                drop(rx);
                woken.set(state.terminal_reader_wake.notified());
            }
        }
    }
}

#[tauri::command]
async fn subscribe_events(
    state: State<'_, DesktopState>,
    on_event: Channel<DesktopStreamMessage>,
    controller_id: String,
) -> Result<(), String> {
    // Make this webview the authoritative controller and wake any terminal
    // reader parked under the prior one so it releases the shared receiver
    // before this controller's reader starts (#974).
    *state.active_controller.lock().await = Some(controller_id);
    state.terminal_reader_wake.notify_waiters();
    let start_streams =
        replace_event_subscription(&state.streams_started, &state.event_channel, on_event);
    if !start_streams {
        // A resubscribe (webview reload / HMR / renderer recovery) keeps the
        // already-running stream tasks, but the fresh webview starts with an
        // empty view-model. The live stream loops only push on the *next*
        // gateway event, so on a quiet inbox the reloaded webview would render
        // "No workspaces to show" until an unrelated change happened to arrive.
        // Re-emit the current grouped view to the new channel so the newest
        // webview reflects the daemon's state immediately (#972).
        emit_inbox_view(&state.inbox, &state.event_channel).await;
        return Ok(());
    }

    let control_gateway = state.gateway.clone();
    let inbox = state.inbox.clone();
    let event_channel = state.event_channel.clone();
    let snippets = state.snippets.clone();
    let event_handoff = state.event_handoff.clone();
    let control_shutdown = state.stream_shutdown.subscribe();
    let control_task = tokio::spawn(async move {
        stream_control_events(
            control_gateway,
            event_channel,
            inbox,
            snippets,
            event_handoff,
            control_shutdown,
        )
        .await;
    });

    let terminal_gateway = state.gateway.clone();
    let terminal_command_rx = state.terminal_command_rx.clone();
    let terminal_tx = state.terminal_tx.clone();
    let terminal_shutdown = state.stream_shutdown.subscribe();
    let terminal_task = tokio::spawn(async move {
        stream_terminal_events(
            terminal_gateway,
            terminal_command_rx,
            terminal_tx,
            terminal_shutdown,
        )
        .await;
    });
    state
        .stream_tasks
        .lock()
        .await
        .extend([control_task, terminal_task]);
    Ok(())
}

/// Release this controller (#974). The daemon stream tasks intentionally
/// outlive a webview reload (#972) and are torn down only at shutdown, so this
/// clears the authoritative controller and wakes the parked terminal reader so
/// its drain loop observes the change and stops — it does not stop the streams.
#[tauri::command]
async fn unsubscribe_events(
    state: State<'_, DesktopState>,
    controller_id: String,
) -> Result<(), String> {
    let mut active = state.active_controller.lock().await;
    if active.as_deref() != Some(&controller_id) {
        return Ok(());
    }
    *active = None;
    state.terminal_reader_wake.notify_waiters();
    Ok(())
}

fn replace_event_subscription(
    streams_started: &AtomicBool,
    event_channel: &RwLock<Option<Channel<DesktopStreamMessage>>>,
    on_event: Channel<DesktopStreamMessage>,
) -> bool {
    *event_channel
        .write()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(on_event);
    !streams_started.swap(true, Ordering::AcqRel)
}

fn send_webview_event(
    event_channel: &RwLock<Option<Channel<DesktopStreamMessage>>>,
    message: DesktopStreamMessage,
) -> bool {
    event_channel
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
        .is_some_and(|channel| channel.send(message).is_ok())
}

impl DesktopState {
    async fn shutdown(&self) {
        let _ = self.stream_shutdown.send(true);
        let stream_tasks = std::mem::take(&mut *self.stream_tasks.lock().await);
        for mut task in stream_tasks {
            if tokio::time::timeout(Duration::from_secs(2), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        // Drain the request-facing services (gateway + socket) to completion
        // *before* tearing down the runtime they call into. Draining first
        // lets an in-flight one-shot command (a spawn, a mark-read) finish
        // against a live runtime/store instead of racing a half-dropped one.
        if let Some(shutdown) = self.socket_shutdown.lock().await.take() {
            shutdown.notify_one();
        }
        if let Some(shutdown) = self.gateway_shutdown.lock().await.take() {
            let _ = shutdown.send(true);
        }
        let service_bound = lazybox_server::MUTATION_DRAIN_TIMEOUT + Duration::from_secs(2);
        if let Some(mut task) = self.gateway_task.lock().await.take()
            && tokio::time::timeout(service_bound, &mut task)
                .await
                .is_err()
        {
            tracing::warn!("desktop gateway exceeded shutdown bound; aborting");
            task.abort();
            let _ = task.await;
        }
        if let Some(mut task) = self.socket_task.lock().await.take()
            && tokio::time::timeout(service_bound, &mut task)
                .await
                .is_err()
        {
            tracing::warn!("desktop socket service exceeded shutdown bound; aborting");
            task.abort();
            let _ = task.await;
        }
        // Only now that nothing can issue further commands is it safe to stop
        // the runtime (pollers, PTYs).
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
            .bytes()
            .await
            .map_err(|error| format!("read gateway error response: {error}"))?;
        if status == reqwest::StatusCode::UPGRADE_REQUIRED {
            let unsupported: UnsupportedProtocolResponse = serde_json::from_slice(&body)
                .map_err(|error| format!("decode protocol incompatibility: {error}"))?;
            return Err(format_unsupported_protocol(&unsupported));
        }
        return Err(format!(
            "gateway returned {status}: {}",
            String::from_utf8_lossy(&body)
        ));
    }
    response
        .json()
        .await
        .map_err(|error| format!("decode gateway response: {error}"))
}

fn format_unsupported_protocol(response: &UnsupportedProtocolResponse) -> String {
    format!(
        "Incompatible lazybox protocol: desktop requested version {} (fingerprint {}), daemon supports version {} (fingerprint {}). {}",
        response.requested,
        response
            .requested_fingerprint
            .as_deref()
            .unwrap_or("unknown"),
        response.supported,
        response.supported_fingerprint,
        response.remediation
    )
}

async fn stream_control_events(
    gateway: GatewayClient,
    event_channel: Arc<RwLock<Option<Channel<DesktopStreamMessage>>>>,
    inbox: Arc<Mutex<InboxModel>>,
    snippets: Arc<Mutex<SnippetModel>>,
    event_handoff: Arc<Mutex<EventHandoff>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let result = tokio::select! {
            _ = shutdown.wait_for(|requested| *requested) => return,
            result = stream_control_events_once(&gateway, &event_channel, &inbox, &snippets, &event_handoff) => result,
        };
        match result {
            Ok(()) => {
                send_webview_event(
                    &event_channel,
                    DesktopStreamMessage::Disconnected {
                        message: "gateway control stream ended".to_string(),
                    },
                );
            }
            Err(ControlStreamError::Transient(error)) => {
                send_webview_event(
                    &event_channel,
                    DesktopStreamMessage::Disconnected { message: error },
                );
            }
            Err(ControlStreamError::Incompatible(message)) => {
                send_webview_event(
                    &event_channel,
                    DesktopStreamMessage::Incompatible { message },
                );
                return;
            }
        }
        tokio::select! {
            _ = shutdown.wait_for(|requested| *requested) => return,
            _ = tokio::time::sleep(Duration::from_millis(750)) => {}
        }
    }
}

enum ControlStreamError {
    Transient(String),
    Incompatible(String),
}

async fn stream_control_events_once(
    gateway: &GatewayClient,
    event_channel: &RwLock<Option<Channel<DesktopStreamMessage>>>,
    inbox: &Mutex<InboxModel>,
    snippets: &Mutex<SnippetModel>,
    event_handoff: &Mutex<EventHandoff>,
) -> Result<(), ControlStreamError> {
    let mut response = gateway
        .authorized(gateway.stream_client.get(gateway.url("/v1/events")))
        .send()
        .await
        .map_err(|error| {
            ControlStreamError::Transient(format!("connect control stream: {error}"))
        })?;
    if !response.status().is_success() {
        if response.status() == reqwest::StatusCode::UPGRADE_REQUIRED {
            let unsupported = response
                .json::<UnsupportedProtocolResponse>()
                .await
                .map_err(|error| {
                    ControlStreamError::Transient(format!(
                        "decode control-stream incompatibility: {error}"
                    ))
                })?;
            return Err(ControlStreamError::Incompatible(
                format_unsupported_protocol(&unsupported),
            ));
        }
        return Err(ControlStreamError::Transient(format!(
            "control stream returned HTTP {}",
            response.status()
        )));
    }
    send_webview_event(event_channel, DesktopStreamMessage::Connected);
    // Emit the current grouped view immediately (seeded from
    // `list_workspaces`), before the daemon's own `Snapshot` arrives.
    emit_inbox_view(inbox, event_channel).await;

    let mut decoder = NdjsonDecoder::default();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ControlStreamError::Transient(format!("read control stream: {error}")))?
    {
        for frame in decoder
            .push(&chunk)
            .map_err(ControlStreamError::Transient)?
        {
            let DesktopEventFrame::Event(event) = frame;
            // Best-effort delivery: a failed send means the webview is between
            // subscriptions (reload/HMR), not that the daemon stream is done —
            // keep folding events into the model so the next resubscribe sees
            // current state. `forward_desktop_event` routes through the shared
            // `event_channel`, so a swapped-in channel receives live events.
            let _ = forward_desktop_event(
                event,
                EventSource::Live,
                event_channel,
                inbox,
                snippets,
                event_handoff,
            )
            .await;
        }
    }
    decoder.finish().map_err(ControlStreamError::Transient)
}

async fn forward_desktop_event(
    event: DesktopEvent,
    source: EventSource,
    event_channel: &RwLock<Option<Channel<DesktopStreamMessage>>>,
    inbox: &Mutex<InboxModel>,
    snippets: &Mutex<SnippetModel>,
    event_handoff: &Mutex<EventHandoff>,
) -> bool {
    if !event_handoff.lock().await.accept(source, &event) {
        return true;
    }
    match &event {
        DesktopEvent::Snapshot {
            recent_snippets, ..
        } => snippets.lock().await.seed_recent(recent_snippets.clone()),
        DesktopEvent::SnippetDelivered { snippet_key, .. } => {
            snippets.lock().await.record_recent(snippet_key.clone())
        }
        _ => {}
    }
    let recompute = inbox.lock().await.apply_event(&event);
    // Route through the shared `event_channel` (not a captured `Channel`) so a
    // resubscribe's swapped-in channel receives live events. A failed send
    // means no live webview; the model still updated, so report it without
    // treating it as fatal.
    let delivered = send_webview_event(event_channel, DesktopStreamMessage::Frame(Box::new(event)));
    if recompute {
        emit_inbox_view(inbox, event_channel).await;
    }
    delivered
}

/// Compute the grouped inbox view from the current model and push it to
/// the webview. A failed send means the webview reader is gone; callers
/// treat that as a benign disconnect.
async fn emit_inbox_view(
    inbox: &Mutex<InboxModel>,
    event_channel: &RwLock<Option<Channel<DesktopStreamMessage>>>,
) {
    let view = inbox.lock().await.compute();
    send_webview_event(event_channel, DesktopStreamMessage::Inbox(Box::new(view)));
}

async fn stream_terminal_events(
    gateway: GatewayClient,
    command_rx: Arc<Mutex<mpsc::Receiver<Bytes>>>,
    terminal_tx: mpsc::Sender<TerminalStreamItem>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut reconnect = false;
    loop {
        let result = tokio::select! {
            _ = shutdown.wait_for(|requested| *requested) => return,
            result = stream_terminal_events_once(
                &gateway,
                command_rx.clone(),
                &terminal_tx,
                reconnect,
            ) => result,
        };
        if let Err(error) = result {
            tracing::warn!("desktop terminal stream disconnected: {error}");
            if terminal_tx
                .send(TerminalStreamItem::Disconnected(error))
                .await
                .is_err()
            {
                return;
            }
        }
        reconnect = true;
        tokio::select! {
            _ = shutdown.wait_for(|requested| *requested) => return,
            _ = tokio::time::sleep(Duration::from_millis(750)) => {}
        }
    }
}

/// Discard the terminal-input backlog that queued while the stream was
/// down. Those frames predate the ring-buffer replay the reconnect's
/// server-side `Subscribe` pulls, so forwarding them would fire stale
/// keystrokes at freshly-authoritative terminal state. Draining a bounded
/// snapshot of the queue — not a `while try_recv` loop — leaves any frame
/// that races in *after* the reconnect untouched. Mirrors the socket
/// client's reconnect drain (`crates/ipc/src/socket.rs`).
fn drain_terminal_backlog(command_rx: &mut mpsc::Receiver<Bytes>) {
    for _ in 0..command_rx.len() {
        if command_rx.try_recv().is_err() {
            break;
        }
    }
}

async fn stream_terminal_events_once(
    gateway: &GatewayClient,
    command_rx: Arc<Mutex<mpsc::Receiver<Bytes>>>,
    terminal_tx: &mpsc::Sender<TerminalStreamItem>,
    reconnect: bool,
) -> Result<(), String> {
    if reconnect {
        drain_terminal_backlog(&mut *command_rx.lock().await);
    }
    let commands = futures_util::stream::unfold(command_rx, |command_rx| async move {
        let command = command_rx.lock().await.recv().await;
        command.map(|command| (Ok::<_, io::Error>(command), command_rx))
    });
    let body = reqwest::Body::wrap_stream(commands);
    let mut response = gateway
        .authorized(
            gateway
                .stream_client
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
        TerminalStreamItem::Disconnected(message) => {
            let mut encoded = Vec::with_capacity(1 + message.len());
            encoded.push(DESKTOP_TERMINAL_STREAM_ITEM_DISCONNECTED);
            encoded.extend_from_slice(message.as_bytes());
            encoded
        }
    }
}

#[derive(Default)]
struct NdjsonDecoder {
    buffer: Vec<u8>,
}

impl NdjsonDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<DesktopEventFrame>, String> {
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

/// Pure mutation over the shared [`Config`] — the desktop's only writer,
/// applied under `Config::save_with`'s read-modify-write lock. Stays
/// side-effect-free so it's trivially testable and can never bypass the
/// atomic save path.
struct DesktopSettings {
    scopes: BTreeSet<String>,
    default_agent: String,
    analytics_enabled: bool,
    theme: Option<String>,
    default_model_tier: Option<String>,
}

fn apply_desktop_settings(config: &mut lazybox_config::Config, settings: DesktopSettings) {
    config.setup.providers.insert("github".to_string());
    config.setup.agents.insert(settings.default_agent.clone());
    config
        .setup
        .filters
        .entry("github".to_string())
        .or_insert_with(|| ProviderConfig::default_for("github").enabled_keys);
    config
        .setup
        .scopes
        .insert("github".to_string(), settings.scopes);
    config.setup.wizard_completed = true;
    config.desktop.analytics_enabled = settings.analytics_enabled;
    config.desktop.theme = settings.theme;
    // Persist the picked default tier against the chosen agent's `models`
    // block — the same "default-model pick" overlay `Config::agent_models`
    // reads. Only touch the agents map when there's a tier to store or the
    // agent already has an entry, so a bare save never litters config with
    // empty agent stanzas.
    if settings.default_model_tier.is_some() || config.agents.contains_key(&settings.default_agent)
    {
        config
            .agents
            .entry(settings.default_agent.clone())
            .or_default()
            .models
            .default = settings.default_model_tier;
    }
    config.setup.default_agent = Some(settings.default_agent);
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
            let models = config.agent_models(&id);
            let default_tier = models.default.clone();
            let model_menu = models
                .tiers
                .iter()
                .map(|tier| DesktopModelTier {
                    alias: tier.alias.clone(),
                    label: tier.label.clone(),
                })
                .collect();
            DesktopAgentOption {
                available: which::which(&command).is_ok(),
                id,
                label,
                models: model_menu,
                default_tier,
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

fn diagnostics_dir() -> std::path::PathBuf {
    lazybox_core::paths::state_root().join("desktop-crashes")
}

fn desktop_log_path() -> std::path::PathBuf {
    lazybox_config::Config::load()
        .map(|config| config.ui.resolved().log_path)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/lazybox.log"))
}

fn open_private_log(path: &Path) -> io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn init_desktop_tracing() -> Result<std::path::PathBuf, String> {
    use tracing_subscriber::prelude::*;

    let path = desktop_log_path();
    let file = open_private_log(&path)
        .map_err(|error| format!("open protected desktop log {}: {error}", path.display()))?;
    lazybox_tui_core::platform::redirect_stderr_to_file(&file);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "lazybox=info,lazybox_gh=info,lazybox_server=info".into());
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_filter(filter),
        )
        .try_init()
        .map_err(|error| format!("initialize desktop tracing: {error}"))?;
    tracing::info!(log_path = %path.display(), "desktop tracing initialized");
    Ok(path)
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
        desktop_build_label(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn desktop_build_label() -> String {
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        env!("LAZYBOX_DESKTOP_BUILD_SHA")
    )
}

#[derive(Debug, Clone, Serialize)]
struct DesktopBuildInfo {
    version: String,
    build_sha: String,
    update: Option<DesktopUpdate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct DesktopUpdate {
    key: String,
    message: String,
}

#[tauri::command]
async fn desktop_build_info() -> DesktopBuildInfo {
    DesktopBuildInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_sha: env!("LAZYBOX_DESKTOP_BUILD_SHA").to_string(),
        update: available_desktop_update().await,
    }
}

async fn available_desktop_update() -> Option<DesktopUpdate> {
    if lazybox_ipc::IS_RELEASE_BUILD {
        return latest_release_update().await;
    }
    source_update_in(lazybox_ipc::BUILD_SOURCE_DIR, lazybox_ipc::BUILD_GIT_SHA).await
}

async fn source_update_in(source_dir: &str, built_sha: &str) -> Option<DesktopUpdate> {
    if source_dir.is_empty() || built_sha.is_empty() || built_sha == "unknown" {
        return None;
    }
    let ancestor = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args(["merge-base", "--is-ancestor", built_sha, "HEAD"])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !ancestor.status.success() {
        return None;
    }
    let range = format!("{built_sha}..HEAD");
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(source_dir)
            .args(["rev-list", "--count", &range])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let commits: u32 = String::from_utf8(output.stdout).ok()?.trim().parse().ok()?;
    if commits == 0 {
        return None;
    }
    let current = shell_quote(source_dir);
    Some(DesktopUpdate {
        key: format!("source:{built_sha}:{commits}"),
        message: format!(
            "This desktop build is {commits} commit{} behind its source. Update with: cd -- {current} && git pull --ff-only && make desktop-build",
            if commits == 1 { "" } else { "s" }
        ),
    })
}

async fn latest_release_update() -> Option<DesktopUpdate> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release = Client::new()
        .get("https://api.github.com/repos/AntoineToussaint/lazybox/releases/latest")
        .header("User-Agent", "lazybox-desktop")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?
        .json::<Release>()
        .await
        .ok()?;
    let latest = semver::Version::parse(release.tag_name.trim_start_matches('v')).ok()?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    (latest > current).then(|| DesktopUpdate {
        key: format!("release:{latest}"),
        message: format!(
            "lazybox desktop v{latest} is available. Download it from the latest GitHub release."
        ),
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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

/// Env var pointing the desktop at an existing gateway's base URL. Its
/// presence (with a non-empty value) selects remote-attach mode and wins
/// over `desktop.remote.url` in config.
const DESKTOP_GATEWAY_URL_ENV: &str = "LAZYBOX_DESKTOP_GATEWAY_URL";
/// Env var carrying that gateway's bearer token; wins over
/// `desktop.remote.token`.
const DESKTOP_GATEWAY_TOKEN_ENV: &str = "LAZYBOX_DESKTOP_GATEWAY_TOKEN";
const GATEWAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GATEWAY_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const GATEWAY_TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const GATEWAY_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

fn gateway_client(base_url: String, bearer_token: String) -> Result<GatewayClient, String> {
    let base = Client::builder()
        .connect_timeout(GATEWAY_CONNECT_TIMEOUT)
        .pool_idle_timeout(GATEWAY_POOL_IDLE_TIMEOUT)
        .tcp_keepalive(GATEWAY_TCP_KEEPALIVE);
    let client = base
        .build()
        .map_err(|error| format!("build gateway HTTP client: {error}"))?;
    let stream_client = Client::builder()
        .connect_timeout(GATEWAY_CONNECT_TIMEOUT)
        .read_timeout(GATEWAY_STREAM_IDLE_TIMEOUT)
        .pool_idle_timeout(GATEWAY_POOL_IDLE_TIMEOUT)
        .tcp_keepalive(GATEWAY_TCP_KEEPALIVE)
        .build()
        .map_err(|error| format!("build gateway stream client: {error}"))?;
    Ok(GatewayClient {
        base_url,
        bearer_token,
        client,
        stream_client,
    })
}

/// The effective gateway the desktop should attach to, resolved from env
/// vars layered over `desktop.remote:` config. `None` means the default
/// self-spawned local daemon.
struct ResolvedRemote {
    base_url: String,
    bearer_token: String,
}

/// Resolve the remote gateway target: env vars win over config, an empty
/// URL from either source is ignored, and a trailing slash on the URL is
/// trimmed so `GatewayClient::url` composes cleanly. A token is optional
/// (the gateway may run `--insecure-no-auth`).
fn resolve_remote_gateway(
    config: Option<&lazybox_config::RemoteGatewayConfig>,
    env_url: Option<String>,
    env_token: Option<String>,
) -> Option<ResolvedRemote> {
    let non_empty = |value: String| -> Option<String> {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    };
    let url = env_url
        .and_then(non_empty)
        .or_else(|| config.map(|remote| remote.url.clone()).and_then(non_empty))?;
    let bearer_token = env_token
        .and_then(&non_empty)
        .or_else(|| {
            config
                .map(|remote| remote.token.clone())
                .and_then(non_empty)
        })
        .unwrap_or_default();
    Some(ResolvedRemote {
        base_url: url.trim_end_matches('/').to_string(),
        bearer_token,
    })
}

async fn start_desktop_state() -> Result<DesktopState, String> {
    let user_config = lazybox_config::Config::load()
        .map_err(|error| format!("load lazybox configuration: {error}"))?;

    let (gateway, authority, mut local) = match resolve_remote_gateway(
        user_config.desktop.remote.as_ref(),
        std::env::var(DESKTOP_GATEWAY_URL_ENV).ok(),
        std::env::var(DESKTOP_GATEWAY_TOKEN_ENV).ok(),
    ) {
        Some(remote) => {
            tracing::info!(
                "lazybox desktop attaching to existing gateway at {}",
                remote.base_url
            );
            (
                gateway_client(remote.base_url, remote.bearer_token)?,
                DesktopAuthority::Remote,
                None,
            )
        }
        None => {
            let services = start_local_gateway(&user_config).await?;
            (
                services.gateway.clone(),
                DesktopAuthority::Embedded,
                Some(services),
            )
        }
    };

    // Validate the protocol and read the daemon's spawn menu (agents,
    // default, repositories) from the gateway itself, so an attached
    // desktop offers what the *daemon* runs rather than the local config —
    // which, over a remote link, describes the laptop, not the box.
    let info = match establish_gateway_session(&gateway).await {
        Ok(info) => info,
        Err(error) => {
            if let Some(services) = local.take() {
                services.shutdown().await;
            }
            return Err(error);
        }
    };
    let DesktopInfo {
        providers,
        agents,
        default_agent,
        repositories,
        settings,
        protocol_notice,
        ..
    } = info;
    if let Some(notice) = &protocol_notice {
        tracing::warn!("lazybox desktop: {notice}");
    }
    let (terminal_commands, terminal_command_rx) = mpsc::channel(256);
    let (terminal_tx, terminal_rx) = mpsc::channel(32);
    let inbox = InboxModel::new(attention_config(&settings.attention));
    let snippets = SnippetModel::new(load_snippet_catalog());

    let (stream_shutdown, _) = watch::channel(false);
    let local = local.unwrap_or_else(LocalServices::remote);
    Ok(DesktopState {
        gateway,
        authority,
        providers,
        agents,
        default_agent,
        repositories,
        daemon_settings: settings,
        terminal_commands,
        terminal_command_rx: Arc::new(Mutex::new(terminal_command_rx)),
        terminal_rx: Mutex::new(terminal_rx),
        terminal_tx,
        streams_started: AtomicBool::new(false),
        stream_shutdown,
        stream_tasks: Mutex::new(Vec::new()),
        active_controller: Mutex::new(None),
        terminal_reader_wake: Arc::new(tokio::sync::Notify::new()),
        protocol_notice,
        client_runtime: Mutex::new(local.client_runtime),
        gateway_task: Mutex::new(local.gateway_task),
        gateway_shutdown: Mutex::new(local.gateway_shutdown),
        socket_task: Mutex::new(local.socket_task),
        socket_shutdown: Mutex::new(local.socket_shutdown),
        inbox: Arc::new(Mutex::new(inbox)),
        event_channel: Arc::new(RwLock::new(None)),
        snippets: Arc::new(Mutex::new(snippets)),
        event_handoff: Arc::new(Mutex::new(EventHandoff::default())),
        conventions: user_config.conventions.clone(),
    })
}

fn attention_config(settings: &DesktopAttentionSettings) -> lazybox_config::AttentionConfig {
    // The daemon projects only the five attention *axes* — the signals
    // that feed the grouped-inbox badge, the sole `AttentionConfig` input
    // `InboxModel::to_view` reads. The notification-*delivery* fields below
    // (`desktop_notify` / `notifier` / `terminal_bundle_id`) drive OS
    // banners, a daemon/TUI concern the desktop shell never consumes, so
    // they take their config defaults here. Enumerate every field on
    // purpose (no `..Default::default()`): if `AttentionConfig` grows a
    // field, this stops compiling and forces a deliberate decision about
    // whether the desktop needs it projected — the silent-default path is
    // exactly how a future view-affecting field would be dropped unnoticed.
    let defaults = lazybox_config::AttentionConfig::default();
    lazybox_config::AttentionConfig {
        unread: settings.unread,
        ci_failing: settings.ci_failing,
        review_pending: settings.review_pending,
        agent_asking: settings.agent_asking,
        mentioned: settings.mentioned,
        desktop_notify: defaults.desktop_notify,
        notifier: defaults.notifier,
        terminal_bundle_id: defaults.terminal_bundle_id,
    }
}

struct LocalServices {
    gateway: GatewayClient,
    client_runtime: Option<ClientRuntime>,
    gateway_task: Option<tokio::task::JoinHandle<()>>,
    gateway_shutdown: Option<watch::Sender<bool>>,
    socket_task: Option<tokio::task::JoinHandle<Result<(), String>>>,
    socket_shutdown: Option<Arc<tokio::sync::Notify>>,
}

impl LocalServices {
    fn remote() -> Self {
        Self {
            gateway: gateway_client(String::new(), String::new())
                .expect("static HTTP client configuration is valid"),
            client_runtime: None,
            gateway_task: None,
            gateway_shutdown: None,
            socket_task: None,
            socket_shutdown: None,
        }
    }

    async fn shutdown(mut self) {
        // Same ordering as `DesktopState::shutdown`: drain the request-facing
        // services before stopping the runtime they call into.
        if let Some(shutdown) = self.gateway_shutdown.take() {
            let _ = shutdown.send(true);
        }
        if let Some(shutdown) = self.socket_shutdown.take() {
            shutdown.notify_one();
        }
        let bound = lazybox_server::MUTATION_DRAIN_TIMEOUT + Duration::from_secs(2);
        if let Some(mut task) = self.gateway_task.take()
            && tokio::time::timeout(bound, &mut task).await.is_err()
        {
            task.abort();
            let _ = task.await;
        }
        if let Some(mut task) = self.socket_task.take()
            && tokio::time::timeout(bound, &mut task).await.is_err()
        {
            task.abort();
            let _ = task.await;
        }
        if let Some(runtime) = self.client_runtime.take() {
            runtime.shutdown().await;
        }
    }
}

/// Spawn the in-process daemon and its loopback API gateway, returning a
/// client pointed at the ephemeral address plus the handles the caller
/// must own for shutdown. Used unless the desktop is configured to attach
/// to an existing gateway.
async fn start_local_gateway(
    user_config: &lazybox_config::Config,
) -> Result<LocalServices, String> {
    lazybox_server::spawn_handler::ensure_stable_hook_exe().ok_or_else(|| {
        "desktop executable cannot provide the lifecycle hook helper; see the desktop log"
            .to_string()
    })?;
    if let ServerStatus::Running { pid } = lifecycle::status() {
        return Err(format!(
            "lazybox daemon is already owned by process {pid}; stop it before starting the embedded desktop daemon"
        ));
    }
    let config = ServerConfig::from_user_config()
        .map_err(|error| format!("start lazybox daemon: {error}"))?;
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .map_err(|error| format!("bind embedded API gateway: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read embedded API address: {error}"))?;
    let bearer_token = uuid::Uuid::new_v4().simple().to_string();
    let socket_service = SocketService::new(lifecycle::socket_path(), lifecycle::pid_path(), {
        let config = config.clone();
        move || config.clone()
    });
    let socket_shutdown = socket_service.shutdown_handle();
    let socket_task = tokio::spawn(async move {
        socket_service
            .run()
            .await
            .map_err(|error| error.to_string())
    });
    if let Err(error) = wait_for_socket_owner(&socket_task).await {
        socket_shutdown.notify_one();
        if !socket_task.is_finished() {
            socket_task.abort();
        }
        let detail = socket_task
            .await
            .ok()
            .and_then(Result::err)
            .unwrap_or(error);
        return Err(format!("bind embedded daemon socket: {detail}"));
    }

    let client_runtime = ClientRuntime::start(
        config.clone(),
        ClientRuntimeOptions {
            poll_interval: user_config.providers.github.poll_interval,
            restore_persisted_sessions: true,
            slack: Some(user_config.slack.clone()),
        },
    )
    .await;
    let options = GatewayOptions {
        bind_addr: address,
        bearer_token: Some(bearer_token.clone()),
        ..GatewayOptions::default()
    };
    let (gateway_shutdown, gateway_shutdown_rx) = watch::channel(false);
    let gateway_task = tokio::spawn(async move {
        if let Err(error) = lazybox_server::api_gateway::serve_listener_until(
            config,
            options,
            listener,
            gateway_shutdown_rx,
            lazybox_server::MUTATION_DRAIN_TIMEOUT + Duration::from_secs(1),
        )
        .await
        {
            tracing::error!("lazybox desktop embedded API gateway stopped: {error}");
        }
    });

    Ok(LocalServices {
        gateway: gateway_client(format!("http://{address}"), bearer_token)?,
        client_runtime: Some(client_runtime),
        gateway_task: Some(gateway_task),
        gateway_shutdown: Some(gateway_shutdown),
        socket_task: Some(socket_task),
        socket_shutdown: Some(socket_shutdown),
    })
}

async fn wait_for_socket_owner(
    task: &tokio::task::JoinHandle<Result<(), String>>,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if lifecycle::read_pid(&lifecycle::pid_path()).ok().flatten() == Some(std::process::id())
            && lifecycle::socket_path().exists()
        {
            return Ok(());
        }
        if task.is_finished() {
            return Err("socket service exited before becoming ready".to_string());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("socket service did not become ready within 2 seconds".to_string());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// How many times the startup handshake retries a gateway that isn't
/// answering yet, and the wait between tries. Roughly ten seconds — long
/// enough for a just-launched desktop to catch an SSH-forwarded loopback
/// port that comes up a beat late, short enough that a truly-down box
/// fails the launch with a clear error rather than hanging. Matches the
/// 750 ms cadence the live stream loops re-dial with.
const GATEWAY_HANDSHAKE_ATTEMPTS: u32 = 14;
const GATEWAY_HANDSHAKE_BACKOFF: Duration = Duration::from_millis(750);

/// A failed startup handshake, split by whether retrying can help: a
/// transport error (the port isn't up yet) is [`Transient`] and retried;
/// a reachable gateway that answers wrong — bad token, protocol/build
/// mismatch — is [`Fatal`] and fails immediately, because re-dialing a
/// misconfigured or incompatible daemon never converges.
///
/// [`Transient`]: GatewaySessionError::Transient
/// [`Fatal`]: GatewaySessionError::Fatal
enum GatewaySessionError {
    Transient(String),
    Fatal(String),
}

/// Discover + validate the gateway protocol and read the daemon's spawn
/// menu, retrying transport failures with backoff so a not-quite-ready
/// remote tunnel connects instead of hard-failing the app launch.
async fn establish_gateway_session(gateway: &GatewayClient) -> Result<DesktopInfo, String> {
    retry_handshake(
        GATEWAY_HANDSHAKE_ATTEMPTS,
        GATEWAY_HANDSHAKE_BACKOFF,
        || gateway_session_once(gateway),
    )
    .await
}

/// Run a handshake `attempt` up to `attempts` times: retry a
/// [`GatewaySessionError::Transient`] result after `backoff`, return a
/// [`GatewaySessionError::Fatal`] result immediately, and give up with
/// the last transient message once the attempts are spent.
async fn retry_handshake<F, Fut>(
    attempts: u32,
    backoff: Duration,
    mut attempt: F,
) -> Result<DesktopInfo, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<DesktopInfo, GatewaySessionError>>,
{
    let mut last_transient = None;
    for index in 0..attempts {
        if index > 0 {
            tokio::time::sleep(backoff).await;
        }
        match attempt().await {
            Ok(info) => return Ok(info),
            Err(GatewaySessionError::Fatal(message)) => return Err(message),
            Err(GatewaySessionError::Transient(message)) => last_transient = Some(message),
        }
    }
    Err(format!(
        "gateway unreachable after {attempts} attempts: {}",
        last_transient.unwrap_or_else(|| "no response".to_string())
    ))
}

async fn gateway_session_once(gateway: &GatewayClient) -> Result<DesktopInfo, GatewaySessionError> {
    let protocol_response = gateway
        .authorized(
            gateway
                .client
                .get(gateway.url("/v1/protocol"))
                .timeout(GATEWAY_CONNECT_TIMEOUT),
        )
        .send()
        .await
        .map_err(|error| {
            GatewaySessionError::Transient(format!("discover daemon protocol: {error}"))
        })?;
    let protocol: ProtocolResponse = decode_response(protocol_response)
        .await
        .map_err(GatewaySessionError::Fatal)?;
    // A tolerable build-skew warning (#815 — fingerprint differs but the
    // protocol version matches) rides back on the info's `protocol_notice`
    // so the webview can surface it. The gateway sends `None`; the client
    // fills it from its own build comparison.
    let protocol_notice = validate_protocol(&protocol).map_err(GatewaySessionError::Fatal)?;

    let info_response = gateway
        .authorized(
            gateway
                .client
                .get(gateway.url("/v1/info"))
                .timeout(GATEWAY_CONNECT_TIMEOUT),
        )
        .send()
        .await
        .map_err(|error| GatewaySessionError::Transient(format!("read daemon info: {error}")))?;
    let mut info: DesktopInfo = decode_response(info_response)
        .await
        .map_err(GatewaySessionError::Fatal)?;
    info.protocol_notice = protocol_notice;
    Ok(info)
}

/// Load the client-wide snippet catalog (built-in → global → launch
/// directory) and project it to the key-sorted picker rows the shared
/// logic expects, mirroring how the TUI seeds its picker.
fn load_snippet_catalog() -> Vec<PickerRow> {
    lazybox_config::Snippets::load_for_launch_dir(std::env::current_dir().ok().as_deref())
        .all()
        .map(|(key, snippet)| PickerRow::new(key, snippet))
        .collect()
}

/// Check the daemon's advertised protocol against this build.
///
/// The protocol *version* and terminal transport are the hard
/// compatibility contract — a mismatch is a genuine wire incompatibility
/// and aborts startup. The *fingerprint* only over-approximates that
/// contract (a `Cargo.lock` bump or a comment edit flips it), so across a
/// remote-daemon hop (#815) two independently-built binaries routinely
/// disagree on it while speaking the same wire. That case returns
/// `Ok(Some(notice))`: the link proceeds, and the caller surfaces the
/// notice so the user can update one side if anything misbehaves. A clean
/// match returns `Ok(None)`.
fn validate_protocol(protocol: &ProtocolResponse) -> Result<Option<String>, String> {
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
    if protocol.protocol_fingerprint != DESKTOP_PROTOCOL_FINGERPRINT {
        return Ok(Some(format!(
            "daemon build {} differs from desktop build {}; the connection works but \
             update one side if anything misbehaves",
            protocol.build_version,
            lazybox_ipc::BUILD_VERSION
        )));
    }
    Ok(None)
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if lazybox_server::spawn_handler::hook_helper_probe_requested(&args) {
        println!(
            "{}",
            lazybox_server::spawn_handler::HOOK_HELPER_PROBE_RESPONSE
        );
        return;
    }
    if std::env::args().any(|argument| argument == "--version" || argument == "-V") {
        println!("lazybox-desktop {}", desktop_build_label());
        return;
    }
    install_crash_diagnostics();
    let tracing_error = init_desktop_tracing().err();
    if matches!(args.first().map(String::as_str), Some("hook-ingest")) {
        tauri::async_runtime::block_on(lifecycle::ingest_hook_from_stdio(&args[1..]));
        return;
    }
    import_login_shell_path();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .setup(move |app| {
            if let Some(error) = &tracing_error {
                app.dialog()
                    .message(format!(
                        "Lazybox could not initialize its protected log at {}.\n\n{error}",
                        desktop_log_path().display()
                    ))
                    .title("Lazybox failed to start")
                    .kind(MessageDialogKind::Error)
                    .blocking_show();
                return Err(std::io::Error::other(error.clone()).into());
            }
            let state = match tauri::async_runtime::block_on(start_desktop_state()) {
                Ok(state) => state,
                Err(error) => {
                    tracing::error!("lazybox desktop failed to start: {error}");
                    app.dialog()
                        .message(format!(
                            "{error}\n\nDetails were written to {}.",
                            desktop_log_path().display()
                        ))
                        .title("Lazybox failed to start")
                        .kind(MessageDialogKind::Error)
                        .blocking_show();
                    return Err(std::io::Error::other(error).into());
                }
            };
            app.manage(state);
            if let Some(window) = app.get_webview_window("main") {
                window.set_focus()?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            desktop_info,
            desktop_build_info,
            desktop_setup_state,
            github_auth_status,
            begin_github_login,
            list_github_repositories,
            save_desktop_settings,
            set_repo_collapsed,
            record_analytics,
            list_workspaces,
            set_sort_mode,
            set_mailbox,
            set_filters,
            set_search,
            snippet_view,
            open_url,
            open_web_control,
            open_workspace_editor,
            resolve_work_prompt,
            send_command,
            send_terminal_frame,
            read_terminal_data,
            subscribe_events,
            unsubscribe_events
        ])
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            tracing::error!("build lazybox desktop: {error}");
            return;
        }
    };
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
        let first = DesktopEventFrame::Event(DesktopEvent::TerminalFocusRequested {
            terminal_id: TerminalId(7),
        });
        let second = DesktopEventFrame::Event(DesktopEvent::PollCompleted {
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
    fn resubscribe_replaces_the_channel_without_starting_duplicate_streams() {
        let streams_started = AtomicBool::new(false);
        let event_channel = RwLock::new(None);
        let first_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first = {
            let hits = first_hits.clone();
            Channel::new(move |_| {
                hits.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        };
        let second = {
            let hits = second_hits.clone();
            Channel::new(move |_| {
                hits.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        };

        assert!(replace_event_subscription(
            &streams_started,
            &event_channel,
            first
        ));
        assert!(!replace_event_subscription(
            &streams_started,
            &event_channel,
            second
        ));
        assert!(send_webview_event(
            &event_channel,
            DesktopStreamMessage::Connected
        ));
        assert_eq!(first_hits.load(Ordering::Relaxed), 0);
        assert_eq!(second_hits.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn resubscribe_reemits_the_current_view_to_the_newest_channel() {
        // A reloaded webview reuses the running streams (streams_started is
        // already set), so the fix must push the current grouped view to the
        // new channel — otherwise a quiet inbox renders empty until the next
        // unrelated event. Mirrors the `!start_streams` path of
        // `subscribe_events` (which needs a live `State` we can't build here).
        let streams_started = AtomicBool::new(false);
        let event_channel = Arc::new(RwLock::new(None));
        let inbox = Arc::new(Mutex::new(empty_model()));
        inbox.lock().await.apply_event(&DesktopEvent::Snapshot {
            workspaces: vec![contract_workspace("octo/widget", 10, true)],
            terminals: vec![],
            recent_snippets: vec![],
        });

        // Initial subscribe starts the streams (its own view emit is exercised
        // by the live stream loop, not here).
        let first_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first = {
            let hits = first_hits.clone();
            Channel::new(move |_| {
                hits.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        };
        assert!(replace_event_subscription(
            &streams_started,
            &event_channel,
            first
        ));

        // Resubscribe: a fresh channel replaces the live one, streams do NOT
        // restart.
        let second_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second = {
            let hits = second_hits.clone();
            Channel::new(move |_| {
                hits.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        };
        assert!(!replace_event_subscription(
            &streams_started,
            &event_channel,
            second
        ));

        // The re-emit the resubscribe path now performs must reach the newest
        // channel — and never the replaced one.
        emit_inbox_view(&inbox, &event_channel).await;
        assert_eq!(
            second_hits.load(Ordering::Relaxed),
            1,
            "the reloaded webview must receive the current grouped view"
        );
        assert_eq!(
            first_hits.load(Ordering::Relaxed),
            0,
            "the replaced channel must receive nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn desktop_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("desktop.log");
        let _file = open_private_log(&path).expect("open log");

        assert_eq!(
            std::fs::metadata(path)
                .expect("log metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn gateway_client_keeps_the_token_out_of_its_url() {
        let gateway = GatewayClient {
            base_url: "http://127.0.0.1:1234".to_string(),
            bearer_token: "secret".to_string(),
            client: Client::new(),
            stream_client: Client::new(),
        };
        assert_eq!(
            gateway.url("/v1/terminal"),
            "http://127.0.0.1:1234/v1/terminal"
        );
        assert!(!gateway.url("/v1/terminal").contains("secret"));
    }

    #[test]
    fn web_control_url_points_at_the_gateway_root_without_a_double_slash() {
        assert_eq!(
            web_control_url("http://127.0.0.1:1808"),
            "http://127.0.0.1:1808/"
        );
        // A trailing slash on the base must not double up.
        assert_eq!(
            web_control_url("http://127.0.0.1:1808/"),
            "http://127.0.0.1:1808/"
        );
    }

    #[test]
    fn a_missing_correlation_echo_is_accepted_but_a_crossed_one_is_rejected() {
        // A stripped `x-lazybox-client-request-id` header (None echo) must
        // not fail an otherwise-successful command; only a present, different
        // id is a genuine mismatch.
        assert!(correlation_echo_ok(None, "req-1"));
        assert!(correlation_echo_ok(Some("req-1"), "req-1"));
        assert!(!correlation_echo_ok(Some("req-2"), "req-1"));
    }

    #[test]
    fn correlated_workspace_failure_rejects_the_invoke_and_names_partial_creation() {
        let events = vec![
            DesktopEvent::WorkspaceCreated {
                client_request_id: "req-1".into(),
                workspace_key: lazybox_core::WorkspaceKey::new("workspace-2"),
            },
            DesktopEvent::CommandFailed {
                client_request_id: "req-1".into(),
                message: "terminal was not spawned".into(),
            },
        ];

        let failure = correlated_command_failure(&events, "req-1")
            .expect("matching failure rejects the command call");
        assert!(failure.contains("workspace-2"));
        assert!(failure.contains("was created"));
        assert!(failure.contains("agent failed"));
        assert!(failure.contains("terminal was not spawned"));
        assert!(correlated_command_failure(&events, "other-request").is_none());
    }

    #[test]
    fn remote_gateway_defaults_to_local_when_unconfigured() {
        assert!(resolve_remote_gateway(None, None, None).is_none());
        // A `desktop.remote:` block with an empty URL is inert.
        let empty = lazybox_config::RemoteGatewayConfig::default();
        assert!(resolve_remote_gateway(Some(&empty), None, None).is_none());
    }

    #[test]
    fn remote_gateway_reads_config_and_trims_trailing_slash() {
        let config = lazybox_config::RemoteGatewayConfig {
            url: "http://127.0.0.1:8787/".to_string(),
            token: "box-token".to_string(),
        };
        let remote =
            resolve_remote_gateway(Some(&config), None, None).expect("config selects remote");
        assert_eq!(remote.base_url, "http://127.0.0.1:8787");
        assert_eq!(remote.bearer_token, "box-token");
    }

    #[test]
    fn remote_gateway_env_overrides_config() {
        let config = lazybox_config::RemoteGatewayConfig {
            url: "http://127.0.0.1:8787".to_string(),
            token: "config-token".to_string(),
        };
        let remote = resolve_remote_gateway(
            Some(&config),
            Some("http://127.0.0.1:9000".to_string()),
            Some("env-token".to_string()),
        )
        .expect("env selects remote");
        assert_eq!(remote.base_url, "http://127.0.0.1:9000");
        assert_eq!(remote.bearer_token, "env-token");
    }

    #[test]
    fn remote_gateway_blank_env_url_falls_back_to_config() {
        let config = lazybox_config::RemoteGatewayConfig {
            url: "http://127.0.0.1:8787".to_string(),
            token: "config-token".to_string(),
        };
        // A blank env URL must not clobber a real config URL, and the
        // config token survives when the env token is also blank.
        let remote =
            resolve_remote_gateway(Some(&config), Some("   ".to_string()), Some(String::new()))
                .expect("config still selects remote");
        assert_eq!(remote.base_url, "http://127.0.0.1:8787");
        assert_eq!(remote.bearer_token, "config-token");
    }

    #[test]
    fn remote_gateway_allows_an_empty_token() {
        let remote = resolve_remote_gateway(None, Some("http://127.0.0.1:9000".to_string()), None)
            .expect("env url selects remote");
        assert_eq!(remote.base_url, "http://127.0.0.1:9000");
        assert!(remote.bearer_token.is_empty());
    }

    fn sample_desktop_info() -> DesktopInfo {
        lazybox_server::api_gateway::build_desktop_info(&lazybox_config::Config::default())
    }

    #[tokio::test]
    async fn handshake_retries_transient_failures_then_connects() {
        // A tunnel that isn't up yet fails transiently a few times before
        // the gateway answers; the handshake keeps trying instead of
        // aborting the launch.
        let calls = std::cell::Cell::new(0u32);
        let result = retry_handshake(GATEWAY_HANDSHAKE_ATTEMPTS, Duration::ZERO, || {
            let attempt = calls.get();
            calls.set(attempt + 1);
            async move {
                if attempt < 3 {
                    Err(GatewaySessionError::Transient("tunnel not up".to_string()))
                } else {
                    Ok(sample_desktop_info())
                }
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(calls.get(), 4, "3 transient failures then one success");
    }

    #[tokio::test]
    async fn handshake_does_not_retry_a_fatal_failure() {
        // A reachable gateway that answers wrong (bad token, build
        // mismatch) is fatal: retrying never converges, so it fails fast.
        let calls = std::cell::Cell::new(0u32);
        let result = retry_handshake(GATEWAY_HANDSHAKE_ATTEMPTS, Duration::ZERO, || {
            calls.set(calls.get() + 1);
            async { Err(GatewaySessionError::Fatal("protocol mismatch".to_string())) }
        })
        .await;

        assert_eq!(result.unwrap_err(), "protocol mismatch");
        assert_eq!(calls.get(), 1, "a fatal error must not be retried");
    }

    #[tokio::test]
    async fn handshake_gives_up_after_exhausting_attempts() {
        let result = retry_handshake(3, Duration::ZERO, || async {
            Err(GatewaySessionError::Transient("down".to_string()))
        })
        .await;

        let error = result.unwrap_err();
        assert!(error.contains("after 3 attempts"), "got: {error}");
        assert!(
            error.contains("down"),
            "surfaces the last transient: {error}"
        );
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
    fn desktop_accepts_a_matching_daemon_without_a_notice() {
        let protocol = lazybox_server::api_gateway::protocol_response();

        let notice = validate_protocol(&protocol).expect("matching contract must be accepted");

        assert_eq!(notice, None);
    }

    #[test]
    fn desktop_tolerates_a_daemon_with_a_different_build_fingerprint() {
        let mut protocol = lazybox_server::api_gateway::protocol_response();
        protocol.protocol_fingerprint = DESKTOP_PROTOCOL_FINGERPRINT.wrapping_add(1);
        protocol.build_version = "9.9.9+deadbeef".to_string();

        let notice = validate_protocol(&protocol)
            .expect("a build-fingerprint skew must be tolerated, not fatal")
            .expect("a tolerated skew must carry a user-facing notice");

        assert!(notice.contains("9.9.9+deadbeef"), "notice: {notice}");
        assert!(notice.contains("update one side"), "notice: {notice}");
    }

    #[test]
    fn desktop_rejects_a_daemon_with_an_incompatible_protocol_version() {
        let mut protocol = lazybox_server::api_gateway::protocol_response();
        protocol.protocol_version = DESKTOP_PROTOCOL_VERSION.wrapping_add(1);

        let error = validate_protocol(&protocol).expect_err("protocol-version skew must be fatal");

        assert!(error.contains("unsupported lazybox protocol version"));
    }

    #[test]
    fn desktop_command_translation_exposes_only_the_supported_control_shape() {
        let session_key = lazybox_core::SessionKey::from("github:o/r#1");
        let command = Command::from(DesktopCommand::SpawnAgent {
            session_key: session_key.clone(),
            agent: "codex".to_string(),
            initial_prompt: Some("Fix the failing checks.".to_string()),
            model_alias: Some("L".to_string()),
            on_main: true,
        });

        assert!(matches!(
            command,
            Command::Spawn {
                kind: lazybox_ipc::TerminalKind::Agent(agent),
                cwd: None,
                initial_prompt: Some(prompt),
                on_main: true,
                model_alias: Some(alias),
                access: lazybox_ipc::AgentRunAccess::Default,
                client_request_id: None,
                ..
            } if agent == "codex" && alias == "L" && prompt == "Fix the failing checks."
        ));
        assert!(matches!(
            Command::from(DesktopCommand::SpawnShell {
                session_key: session_key.clone(),
                on_main: true,
            }),
            Command::Spawn {
                kind: lazybox_ipc::TerminalKind::Shell,
                cwd: None,
                initial_prompt: None,
                on_main: true,
                ..
            }
        ));
        assert!(matches!(
            Command::from(DesktopCommand::MergePr {
                session_key: session_key.clone(),
            }),
            Command::MergePr { workspace_key } if workspace_key.0 == session_key.as_str()
        ));
        assert!(matches!(
            Command::from(DesktopCommand::UpdateBranch {
                session_key: session_key.clone(),
            }),
            Command::UpdateBranch { workspace_key } if workspace_key.0 == session_key.as_str()
        ));
        assert!(matches!(
            Command::from(DesktopCommand::Archive {
                session_key: session_key.clone(),
            }),
            Command::Kill { session_key: key } if key == session_key
        ));
        assert!(matches!(
            Command::from(DesktopCommand::CloseIssue {
                session_key: session_key.clone(),
            }),
            Command::CloseIssue { workspace_key } if workspace_key.0 == session_key.as_str()
        ));
        assert!(matches!(
            Command::from(DesktopCommand::DeleteOrClose {
                session_key: session_key.clone(),
            }),
            Command::DeleteOrClose { workspace_key } if workspace_key.0 == session_key.as_str()
        ));
        assert!(matches!(
            Command::from(DesktopCommand::RenameWorkspace {
                session_key: session_key.clone(),
                name: "renamed".to_string(),
            }),
            Command::RenameWorkspace { session_key: key, name }
                if key == session_key && name == "renamed"
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
                ..
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
        apply_desktop_settings(
            &mut config,
            DesktopSettings {
                scopes,
                default_agent: "codex".to_string(),
                analytics_enabled: true,
                theme: Some("Tokyo Night".to_string()),
                default_model_tier: Some("M".to_string()),
            },
        );
        config.save_to(&path).expect("persist desktop setup");

        let saved = lazybox_config::Config::load_from(&path).expect("reload desktop setup");
        assert!(saved.setup.providers.contains("linear"));
        assert!(saved.setup.providers.contains("github"));
        assert_eq!(saved.setup.default_agent.as_deref(), Some("codex"));
        assert!(saved.setup.agents.contains("codex"));
        assert!(saved.setup.wizard_completed);
        assert!(saved.desktop.analytics_enabled);
        assert_eq!(saved.desktop.theme.as_deref(), Some("Tokyo Night"));
        assert_eq!(
            saved
                .agents
                .get("codex")
                .and_then(|a| a.models.default.as_deref()),
            Some("M")
        );
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
        // The theme catalog comes from the shared palette (not hardcoded TS).
        assert!(initial.theme.is_none());
        assert!(!initial.themes.is_empty());
        // Claude's built-in tier menu rides on its agent option so the UI
        // can offer a default-model pick.
        let claude = initial
            .agents
            .iter()
            .find(|agent| agent.id == "claude")
            .expect("claude option present");
        assert_eq!(claude.default_tier.as_deref(), Some("L"));
        assert!(
            claude
                .models
                .iter()
                .any(|tier| tier.alias == "M" && tier.label == "Sonnet")
        );
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
        assert!(
            changed
                .agents
                .iter()
                .any(|agent| agent.id == "cursor-agent" && agent.label == "Cursor Agent")
        );
    }

    #[test]
    fn remote_setup_uses_daemon_capabilities_and_skips_local_first_run() {
        let local = lazybox_config::Config::default();
        let mut daemon = lazybox_config::Config::default();
        daemon.setup.providers.insert("github".to_string());
        daemon.setup.agents.insert("remote-bot".to_string());
        daemon.setup.default_agent = Some("remote-bot".to_string());
        daemon.setup.scopes.insert(
            "github".to_string(),
            BTreeSet::from(["github:remote/widget".to_string()]),
        );
        daemon.agents.insert(
            "remote-bot".to_string(),
            lazybox_config::AgentEntry {
                name: Some("Remote Bot".to_string()),
                models: lazybox_core::AgentModels {
                    default: Some("R".to_string()),
                    tiers: vec![lazybox_core::ModelTier {
                        alias: "R".to_string(),
                        label: "Remote Large".to_string(),
                        short: None,
                        args: vec!["--remote-large".to_string()],
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let info = lazybox_server::api_gateway::build_desktop_info(&daemon);

        let state = desktop_setup_state_for_remote(
            &local,
            &info.providers,
            &info.agents,
            &info.default_agent,
            &info.settings,
        );

        assert_eq!(state.authority, DesktopAuthority::Remote);
        assert!(!state.first_run);
        assert_eq!(state.providers, vec!["github"]);
        assert_eq!(state.selected_scopes, vec!["github:remote/widget"]);
        assert_eq!(state.default_agent, "remote-bot");
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].label, "Remote Bot");
        assert_eq!(state.agents[0].models[0].label, "Remote Large");
    }

    #[test]
    fn attention_config_carries_every_daemon_axis_and_not_just_the_defaults() {
        // Regression for the silent `..Default::default()` widening: a
        // daemon that disables an axis must not resurface as `true` on the
        // client. Every axis flows through, and the delivery fields the
        // desktop shell never reads take their config defaults.
        let settings = DesktopAttentionSettings {
            unread: false,
            ci_failing: true,
            review_pending: false,
            agent_asking: true,
            mentioned: false,
        };
        let config = attention_config(&settings);
        assert!(!config.unread);
        assert!(config.ci_failing);
        assert!(!config.review_pending);
        assert!(config.agent_asking);
        assert!(!config.mentioned);
        let defaults = lazybox_config::AttentionConfig::default();
        assert_eq!(config.desktop_notify, defaults.desktop_notify);
        assert_eq!(config.notifier, defaults.notifier);
        assert_eq!(config.terminal_bundle_id, defaults.terminal_bundle_id);
    }

    #[test]
    fn repo_collapse_persists_to_the_authority_owning_the_rows() {
        // Embedded shares the local TUI's `ui.collapsed_repos` so the two
        // clients interoperate on one machine; remote writes the
        // client-owned `desktop.collapsed_repos` so a remote repo's
        // collapse never contaminates a same-named repo in the laptop TUI.
        let mut config = lazybox_config::Config::default();

        apply_repo_collapse(&mut config, false, "acme/widget".to_string(), true);
        assert!(config.ui.collapsed_repos.contains("acme/widget"));
        assert!(config.desktop.collapsed_repos.is_empty());

        apply_repo_collapse(&mut config, true, "acme/widget".to_string(), true);
        assert!(config.desktop.collapsed_repos.contains("acme/widget"));
        // The embedded set is untouched by a remote write — no crossover.
        assert!(config.ui.collapsed_repos.contains("acme/widget"));

        apply_repo_collapse(&mut config, true, "acme/widget".to_string(), false);
        assert!(config.desktop.collapsed_repos.is_empty());
        // Expanding remotely leaves the shared TUI set alone.
        assert!(config.ui.collapsed_repos.contains("acme/widget"));
    }

    #[test]
    fn theme_catalog_mirrors_the_shared_palette_and_renders_hex() {
        let options = theme_options();
        assert_eq!(
            options.len(),
            lazybox_tui_core::theme::BUILT_IN_PALETTES.len()
        );
        let dark = options
            .iter()
            .find(|option| option.name == "Lazybox Dark")
            .expect("built-in dark theme");
        // Accent (125, 207, 255) renders as lowercase, zero-padded hex.
        assert_eq!(dark.colors.accent, "#7dcfff");
        assert!(theme_exists("Lazybox Dark"));
        assert!(!theme_exists("No Such Theme"));
    }

    #[test]
    fn theme_validation_rejects_a_new_unknown_theme_but_preserves_the_current_one() {
        // Picking a built-in is fine; switching to a name the desktop
        // can't render is rejected.
        assert!(validate_theme_change(Some("Tokyo Night"), Some("Lazybox Dark")).is_ok());
        assert!(validate_theme_change(Some("Bogus"), Some("Lazybox Dark")).is_err());
        // A theme registered outside the built-in catalog (set via the
        // TUI) must not block a save that leaves it unchanged.
        assert!(validate_theme_change(Some("Custom Plugin"), Some("Custom Plugin")).is_ok());
        assert!(validate_theme_change(None, Some("Custom Plugin")).is_ok());
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
    async fn reconnect_drains_the_stale_terminal_backlog() {
        // Keystrokes queued during an outage predate the reconnect's
        // ring-buffer replay; forwarding them would fire stale input at the
        // resynced PTY. The reconnect must drop them.
        let (tx, mut rx) = mpsc::channel::<Bytes>(8);
        tx.send(Bytes::from_static(b"stale-1"))
            .await
            .expect("queue stale frame");
        tx.send(Bytes::from_static(b"stale-2"))
            .await
            .expect("queue stale frame");
        assert_eq!(rx.len(), 2);

        drain_terminal_backlog(&mut rx);
        assert_eq!(rx.len(), 0, "the outage backlog must be discarded");

        // Input that arrives after the drain is genuine post-reconnect
        // typing and must survive.
        tx.send(Bytes::from_static(b"fresh"))
            .await
            .expect("queue post-reconnect frame");
        assert_eq!(rx.recv().await.as_deref(), Some(b"fresh".as_slice()));
    }

    #[tokio::test]
    async fn the_first_connection_keeps_its_backlog_but_a_reconnect_drains_it() {
        // Bind then drop a listener so the port is definitely closed: every
        // connect fails at the transport, after the drain gate has already
        // decided whether to run. That isolates the `reconnect` flag from
        // any body streaming.
        let address = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind then release a closed port");
            listener.local_addr().expect("closed port address")
        };
        let gateway = GatewayClient {
            base_url: format!("http://{address}"),
            bearer_token: String::new(),
            client: Client::new(),
            stream_client: Client::new(),
        };
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let command_rx = Arc::new(Mutex::new(rx));
        let (terminal_tx, _terminal_rx) = mpsc::channel(8);

        tx.send(Bytes::from_static(b"queued"))
            .await
            .expect("seed backlog");

        // The initial connection must preserve the backlog: dropping the
        // caller's first keystrokes would be a regression in the other
        // direction.
        assert!(
            stream_terminal_events_once(&gateway, command_rx.clone(), &terminal_tx, false)
                .await
                .is_err(),
            "a closed gateway must fail the connect"
        );
        assert_eq!(
            command_rx.lock().await.len(),
            1,
            "the first connection must not drain the backlog"
        );

        // A reconnect drains it before touching the network.
        assert!(
            stream_terminal_events_once(&gateway, command_rx.clone(), &terminal_tx, true)
                .await
                .is_err(),
            "a closed gateway must fail the connect"
        );
        assert_eq!(
            command_rx.lock().await.len(),
            0,
            "a reconnect must drain the backlog"
        );
    }

    #[tokio::test]
    async fn credential_free_dogfood_flow_crosses_config_gateway_and_real_pty() {
        let directory = tempfile::tempdir().expect("temporary fixture directory");
        let config_path = directory.path().join("config.yaml");
        let mut persisted_config = lazybox_config::Config::default();
        apply_desktop_settings(
            &mut persisted_config,
            DesktopSettings {
                scopes: BTreeSet::from(["github:acme/widget".to_string()]),
                default_agent: "claude".to_string(),
                analytics_enabled: false,
                theme: None,
                default_model_tier: None,
            },
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
            stream_client: Client::new(),
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
                on_main: false,
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
        assert_eq!(
            encode_terminal_stream_item(TerminalStreamItem::Disconnected(
                "connection lost".to_string()
            )),
            [
                vec![DESKTOP_TERMINAL_STREAM_ITEM_DISCONNECTED],
                b"connection lost".to_vec()
            ]
            .concat()
        );
    }

    // ── grouped inbox view-model (#732) ──────────────────────────────

    fn contract_task(repo: &str, number: u64, is_pr: bool) -> lazybox_core::Task {
        lazybox_core::Task {
            id: lazybox_core::TaskId {
                source: "github".into(),
                key: format!("{repo}#{number}"),
            },
            title: format!("Task {number}"),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: if is_pr {
                format!("https://github.com/{repo}/pull/{number}")
            } else {
                format!("https://github.com/{repo}/issues/{number}")
            },
            repo: Some(repo.to_string()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            author: String::new(),
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: lazybox_core::ApprovalPolicy::Default,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: Some(if is_pr {
                lazybox_core::TaskKind::Pr
            } else {
                lazybox_core::TaskKind::Issue
            }),
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    fn contract_workspace(repo: &str, number: u64, is_pr: bool) -> lazybox_core::Workspace {
        lazybox_core::Workspace::from_task(contract_task(repo, number, is_pr), chrono::Utc::now())
    }

    fn empty_model() -> InboxModel {
        InboxModel::new(lazybox_config::AttentionConfig::default())
    }

    #[test]
    fn snapshot_groups_prs_above_issues_through_shared_logic() {
        let mut model = empty_model();
        let pr = contract_workspace("octo/widget", 10, true);
        let issue = contract_workspace("octo/widget", 11, false);
        assert!(model.apply_event(&DesktopEvent::Snapshot {
            workspaces: vec![pr, issue],
            terminals: vec![],
            recent_snippets: vec![],
        }));

        let view = model.compute();
        assert_eq!(view.sort_mode, SortMode::ByRoleSplit);
        // repo header → PR section → PR row → Issue section → Issue row.
        let rows = &view.outcome.visible;
        assert!(matches!(&rows[0], inbox::VisibleRow::RepoHeader(name) if name == "octo/widget"));
        assert!(matches!(
            &rows[1],
            inbox::VisibleRow::KindHeader(inbox::WorkspaceKind::Pr)
        ));
        assert!(matches!(&rows[2], inbox::VisibleRow::Workspace(_)));
        assert!(matches!(
            &rows[3],
            inbox::VisibleRow::KindHeader(inbox::WorkspaceKind::Issue)
        ));
        assert!(matches!(&rows[4], inbox::VisibleRow::Workspace(_)));
        assert_eq!(view.outcome.summaries["octo/widget"].active, 2);
    }

    #[test]
    fn cycling_sort_mode_reorders_and_drops_kind_headers() {
        let mut model = empty_model();
        model.apply_event(&DesktopEvent::Snapshot {
            workspaces: vec![
                contract_workspace("octo/widget", 10, true),
                contract_workspace("octo/widget", 11, false),
            ],
            terminals: vec![],
            recent_snippets: vec![],
        });
        // Default split emits kind-section headers.
        assert!(
            model
                .compute()
                .outcome
                .visible
                .iter()
                .any(|row| matches!(row, inbox::VisibleRow::KindHeader(_)))
        );

        // split → recent: kind headers disappear (flat recency order).
        assert_eq!(model.cycle_sort_mode(), SortMode::Recent);
        let recent = model.compute();
        assert_eq!(recent.sort_mode, SortMode::Recent);
        assert!(
            !recent
                .outcome
                .visible
                .iter()
                .any(|row| matches!(row, inbox::VisibleRow::KindHeader(_)))
        );

        assert_eq!(model.cycle_sort_mode(), SortMode::ByRole);
        assert_eq!(model.cycle_sort_mode(), SortMode::ByRoleSplit);
    }

    #[test]
    fn cycling_mailbox_moves_off_the_inbox_and_hides_open_workspaces() {
        let mut model = empty_model();
        model.apply_event(&DesktopEvent::Snapshot {
            workspaces: vec![contract_workspace("octo/widget", 10, true)],
            terminals: vec![],
            recent_snippets: vec![],
        });
        // Open PR lands in the default Inbox mailbox.
        let inbox = model.compute();
        assert_eq!(inbox.mailbox, Mailbox::Inbox);
        assert_eq!(inbox.outcome.summaries["octo/widget"].active, 1);

        // Inbox → Inactive: an Open workspace is not historical, so it drops.
        assert_eq!(model.cycle_mailbox(), Mailbox::Inactive);
        let inactive = model.compute();
        assert_eq!(inactive.mailbox, Mailbox::Inactive);
        assert!(inactive.outcome.visible.is_empty());

        // Inactive → Snoozed → Inbox wraps back.
        assert_eq!(model.cycle_mailbox(), Mailbox::Snoozed);
        assert_eq!(model.compute().mailbox, Mailbox::Snoozed);
        assert_eq!(model.cycle_mailbox(), Mailbox::Inbox);
        assert_eq!(model.compute().mailbox, Mailbox::Inbox);
    }

    #[test]
    fn workspace_upsert_and_removal_update_the_view() {
        let mut model = empty_model();
        let workspace = contract_workspace("octo/widget", 10, true);
        let key = lazybox_core::SessionKey::from(&workspace.key);
        assert!(model.apply_event(&DesktopEvent::WorkspaceUpserted(Box::new(
            workspace.clone()
        ))));
        assert_eq!(model.compute().outcome.summaries["octo/widget"].active, 1);

        assert!(model.apply_event(&DesktopEvent::WorkspaceRemoved(workspace.key.clone())));
        assert!(model.compute().outcome.summaries.is_empty());
        assert!(!model.workspaces.contains_key(&key));
    }

    #[test]
    fn agent_input_needed_raises_repo_attention() {
        let mut model = empty_model();
        let workspace = contract_workspace("octo/widget", 10, true);
        let session_key = lazybox_core::SessionKey::from(&workspace.key);
        model.apply_event(&DesktopEvent::Snapshot {
            workspaces: vec![workspace],
            terminals: vec![],
            recent_snippets: vec![],
        });
        assert_eq!(
            model.compute().outcome.summaries["octo/widget"].attention,
            0
        );

        assert!(model.apply_event(&DesktopEvent::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::InputNeeded,
        }));
        assert_eq!(
            model.compute().outcome.summaries["octo/widget"].attention,
            1
        );

        // A terminal exit clears the aggregated state and the attention.
        assert!(model.apply_event(&DesktopEvent::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        }));
        assert!(!model.agents.contains_key(&session_key));
    }

    #[test]
    fn multiple_terminals_aggregate_with_input_needed_winning() {
        assert_eq!(
            aggregate_agent_state(
                [
                    AgentState::Working,
                    AgentState::InputNeeded,
                    AgentState::Idle
                ]
                .into_iter()
            ),
            Some(AgentState::InputNeeded)
        );
        assert_eq!(
            aggregate_agent_state([AgentState::Done, AgentState::Working].into_iter()),
            Some(AgentState::Working)
        );
        assert_eq!(aggregate_agent_state(std::iter::empty()), None);
    }

    #[test]
    fn agent_state_re_emits_only_when_the_asking_set_changes() {
        let mut model = empty_model();
        let workspace = contract_workspace("octo/widget", 10, true);
        let session_key = lazybox_core::SessionKey::from(&workspace.key);
        model.apply_event(&DesktopEvent::Snapshot {
            workspaces: vec![workspace],
            terminals: vec![],
            recent_snippets: vec![],
        });

        // Working/Done are invisible to the grouped view (only the
        // `InputNeeded` set is reflected), so folding them in must NOT
        // request a re-emit — otherwise an active agent churns the
        // webview on every transition.
        assert!(!model.apply_event(&DesktopEvent::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::Working,
        }));
        assert!(!model.apply_event(&DesktopEvent::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::Done,
        }));

        // Entering InputNeeded flips the asking set → re-emit.
        assert!(model.apply_event(&DesktopEvent::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::InputNeeded,
        }));
        // A redundant repeat of InputNeeded leaves the set intact → no-op.
        assert!(!model.apply_event(&DesktopEvent::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::InputNeeded,
        }));
        // Leaving InputNeeded flips it back → re-emit.
        assert!(model.apply_event(&DesktopEvent::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::Working,
        }));

        // Re-arm asking, then an unrelated terminal's exit leaves the
        // asking set intact → no re-emit; the asking terminal's exit
        // clears it → re-emit.
        assert!(model.apply_event(&DesktopEvent::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::InputNeeded,
        }));
        assert!(!model.apply_event(&DesktopEvent::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: Some(0),
            last_output: None,
        }));
        assert!(model.apply_event(&DesktopEvent::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        }));
    }

    fn workspace_rows(view: &DesktopInboxView) -> usize {
        view.outcome
            .visible
            .iter()
            .filter(|row| matches!(row, inbox::VisibleRow::Workspace(_)))
            .count()
    }

    #[test]
    fn set_filters_narrows_the_view_and_surfaces_menu_and_chips() {
        let mut model = empty_model();
        model.apply_event(&DesktopEvent::Snapshot {
            workspaces: vec![
                contract_workspace("owner/r", 1, true),
                contract_workspace("owner/r", 2, true),
            ],
            terminals: vec![],
            recent_snippets: vec![],
        });

        // The menu is always present with every predicate and live counts.
        let base = model.compute();
        assert_eq!(base.filter_menu.len(), Filter::ALL.len());
        assert!(base.filter_chips.is_empty());
        assert_eq!(
            base.filter_menu
                .iter()
                .find(|i| i.filter == Filter::Pr)
                .map(|i| i.count),
            Some(2)
        );

        // An Issue filter hides both PRs; the chip and active flag show.
        model.set_filters("controller", 1, [Filter::Issue]);
        let filtered = model.compute();
        assert_eq!(workspace_rows(&filtered), 0);
        assert_eq!(filtered.filter_chips, vec!["issue".to_string()]);
        assert!(
            filtered
                .filter_menu
                .iter()
                .find(|i| i.filter == Filter::Issue)
                .is_some_and(|i| i.active)
        );

        // Clearing restores the full view.
        model.set_filters("controller", 2, []);
        assert_eq!(workspace_rows(&model.compute()), 2);
    }

    #[test]
    fn request_generations_ignore_reordered_filter_and_search_calls() {
        let mut model = empty_model();
        assert!(model.set_filters("first", 2, [Filter::Issue]));
        let revision = model.revision;
        assert!(!model.set_filters("first", 1, [Filter::Pr]));
        assert_eq!(model.revision, revision);
        assert!(model.filters.iter().any(|filter| filter == Filter::Issue));
        assert!(!model.filters.iter().any(|filter| filter == Filter::Pr));

        assert!(model.set_search("first", 4, "new".into()));
        let revision = model.revision;
        assert!(!model.set_search("first", 3, "stale".into()));
        assert_eq!(model.search, "new");
        assert_eq!(model.revision, revision);

        assert!(model.set_filters("reinitialized", 1, [Filter::Pr]));
        assert!(model.filters.iter().any(|filter| filter == Filter::Pr));
    }

    #[test]
    fn set_search_filters_globally_across_repos() {
        let mut model = empty_model();
        model.apply_event(&DesktopEvent::Snapshot {
            workspaces: vec![
                contract_workspace("owner/a", 1, true),
                contract_workspace("owner/b", 2, true),
            ],
            terminals: vec![],
            recent_snippets: vec![],
        });
        // A number search keeps only the matching workspace, even though the
        // two live in different repo groups (global scope, #733).
        model.set_search("controller", 1, "2".into());
        assert_eq!(workspace_rows(&model.compute()), 1);
        // Clearing the query restores everything.
        model.set_search("controller", 2, String::new());
        assert_eq!(workspace_rows(&model.compute()), 2);
    }

    #[test]
    fn desktop_versions_follow_the_workspace_release_version() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root: toml::Value = std::fs::read_to_string(manifest.join("../../../Cargo.toml"))
            .expect("read workspace manifest")
            .parse()
            .expect("parse workspace manifest");
        let release = root["workspace"]["package"]["version"]
            .as_str()
            .expect("workspace package version");
        let package: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(manifest.join("../package.json"))
                .expect("read desktop package"),
        )
        .expect("parse desktop package");
        let tauri: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(manifest.join("tauri.conf.json")).expect("read tauri config"),
        )
        .expect("parse tauri config");

        assert_eq!(env!("CARGO_PKG_VERSION"), release);
        assert_eq!(package["version"], release);
        assert!(
            tauri.get("version").is_none(),
            "Tauri must inherit the crate version instead of declaring another source"
        );
        assert!(desktop_build_label().starts_with(&format!("{release}+")));
    }

    #[tokio::test]
    async fn source_build_guard_detects_one_newer_commit_and_ignores_current() {
        fn git(repo: &Path, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
                .args(args)
                .output()
                .expect("run git");
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .expect("utf-8 git output")
                .trim()
                .to_string()
        }
        let repo = tempfile::tempdir().expect("temp repo");
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &["config", "user.email", "lazybox@example.com"],
        );
        git(repo.path(), &["config", "user.name", "Lazybox Test"]);
        std::fs::write(repo.path().join("state"), "old").expect("write fixture");
        git(repo.path(), &["add", "state"]);
        git(repo.path(), &["commit", "-qm", "old"]);
        let built = git(repo.path(), &["rev-parse", "HEAD"]);
        assert!(
            source_update_in(repo.path().to_str().expect("utf-8 path"), &built)
                .await
                .is_none()
        );

        std::fs::write(repo.path().join("state"), "new").expect("update fixture");
        git(repo.path(), &["add", "state"]);
        git(repo.path(), &["commit", "-qm", "new"]);
        let update = source_update_in(repo.path().to_str().expect("utf-8 path"), &built)
            .await
            .expect("stale build update");
        assert!(update.key.starts_with("source:"));
        assert!(update.message.contains("1 commit behind"));
        assert!(update.message.contains("make desktop-build"));
    }
}
