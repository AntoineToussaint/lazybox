#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod snippets;

use bytes::Bytes;
use lazybox_core::ProviderConfig;
use lazybox_ipc::{AgentState, TerminalId};
use lazybox_server::ServerConfig;
use lazybox_server::api_gateway::{
    CommandResponse, DESKTOP_PROTOCOL_FINGERPRINT, DESKTOP_PROTOCOL_VERSION,
    DESKTOP_TERMINAL_STREAM_ITEM_DATA, DESKTOP_TERMINAL_STREAM_ITEM_RESET, DesktopCommand,
    DesktopEvent, DesktopInboxView, DesktopInfo, DesktopRepository, DesktopStreamMessage,
    GatewayOptions, JsonClientFrame, JsonServerFrame, PROTOCOL_FINGERPRINT_HEADER,
    PROTOCOL_VERSION_HEADER, ProtocolResponse, TERMINAL_BINARY_CONTENT_TYPE, WorkspacesResponse,
    desktop_event,
};
use lazybox_server::client_runtime::{ClientRuntime, ClientRuntimeOptions};
use lazybox_tui_core::inbox::{
    self, ComputeInputs, Filter, FilterSet, Mailbox, SearchState, SortMode, mailbox_membership,
};
use lazybox_tui_core::snippets::{PickerRow, SnippetPickerView};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use snippets::SnippetModel;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

/// Desktop-side state-of-record for the grouped inbox (#732). The
/// `src-tauri` layer maintains the workspace + agent maps from gateway
/// events and calls the shared, client-free
/// [`lazybox_tui_core::inbox::compute_visible`] — the exact code the
/// ratatui TUI builds its sidebar from — so the desktop and TUI can't
/// drift on grouping or sort. The webview is a thin renderer over the
/// emitted [`DesktopInboxView`].
struct InboxModel {
    workspaces: HashMap<lazybox_core::SessionKey, lazybox_core::Workspace>,
    /// Per-terminal agent state, keyed by terminal id so one agent in a
    /// multi-session workspace can't clobber another (mirrors the TUI's
    /// `agent_terminal_states`). Aggregated into `agents` per session.
    agent_terminal_states: HashMap<TerminalId, (lazybox_core::SessionKey, AgentState)>,
    /// The derived per-session agent state that `compute_visible`'s
    /// attention scoring reads.
    agents: HashMap<lazybox_core::SessionKey, AgentState>,
    sort_mode: SortMode,
    collapsed_repos: BTreeSet<String>,
    attention: lazybox_config::AttentionConfig,
    /// Active filter set from the multi-select filter menu (#733).
    filters: FilterSet,
    /// Global free-text search query (empty = inactive). Fed into the
    /// shared search with a `None` scope so it filters every project,
    /// unlike the TUI's cursor-scoped `/` (#733).
    search: String,
}

impl InboxModel {
    fn new(collapsed_repos: BTreeSet<String>, attention: lazybox_config::AttentionConfig) -> Self {
        Self {
            workspaces: HashMap::new(),
            agent_terminal_states: HashMap::new(),
            agents: HashMap::new(),
            sort_mode: SortMode::default(),
            collapsed_repos,
            attention,
            filters: FilterSet::new(),
            search: String::new(),
        }
    }

    /// Replace the active filter set (the multi-select filter menu's
    /// output). An empty list clears all filters (#733).
    fn set_filters(&mut self, filters: impl IntoIterator<Item = Filter>) {
        self.filters.replace(filters);
    }

    /// Set the global search query; an empty/blank query is inactive (#733).
    fn set_search(&mut self, query: String) {
        self.search = query;
    }

    /// Seed the workspace map from the initial `list_workspaces`
    /// response so the view (and `set_sort_mode`) works before the
    /// first `Snapshot` event arrives.
    fn seed_workspaces(&mut self, workspaces: &[lazybox_core::Workspace]) {
        self.workspaces = workspaces
            .iter()
            .map(|w| ((&w.key).into(), w.clone()))
            .collect();
    }

    /// Fold a desktop event into the model. Returns whether the inbox
    /// view should be recomputed + re-emitted.
    fn apply_event(&mut self, event: &DesktopEvent) -> bool {
        match event {
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
        }
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
        self.sort_mode
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
            .filter(|w| mailbox_membership(w, Mailbox::Inbox, now, false))
            .collect();
        let filter_menu = Filter::menu(&candidates, &self.agents, &self.filters);
        let filter_chips: Vec<String> =
            self.filters.chips().iter().map(|c| c.to_string()).collect();
        let outcome = inbox::compute_visible(ComputeInputs {
            workspaces: &self.workspaces,
            mailbox: Mailbox::Inbox,
            filters: &self.filters,
            sort_mode: self.sort_mode,
            show_inactive_in_inbox: false,
            projects: &projects,
            collapsed_repos: &self.collapsed_repos,
            // The desktop client has no pin-to-top UI yet; the shared
            // builder honors pins when a caller supplies them (#760).
            pinned_repos: &[],
            attention: &self.attention,
            agents: &self.agents,
            now,
            search: search.as_ref(),
        });
        DesktopInboxView {
            outcome,
            sort_mode: self.sort_mode,
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
    /// Grouped-inbox state-of-record (#732). Shared between the event
    /// stream (which folds in workspace/agent changes and re-emits the
    /// view) and the `set_sort_mode` command.
    inbox: Arc<Mutex<InboxModel>>,
    /// The live webview channel, stored so `set_sort_mode` can push a
    /// recomputed inbox view on the same channel the event stream uses.
    event_channel: Arc<Mutex<Option<Channel<DesktopStreamMessage>>>>,
    /// State-of-record for the snippet picker (#734): the catalog plus the
    /// daemon-owned MRU, reduced from the control stream. The frontend
    /// pulls a recomputed view per keystroke via `snippet_view`.
    snippets: Arc<Mutex<SnippetModel>>,
}

#[derive(Clone, Debug, Serialize)]
struct DesktopModelTier {
    alias: String,
    label: String,
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
    first_run: bool,
    selected_scopes: Vec<String>,
    agents: Vec<DesktopAgentOption>,
    default_agent: String,
    analytics_enabled: bool,
    diagnostics_path: String,
    /// Active `ui.theme` name, or `None` for the default theme.
    theme: Option<String>,
    /// The built-in theme catalog (name + palette) the client renders
    /// swatches from — sourced from `lazybox_tui_core::theme`, never
    /// hardcoded in the frontend.
    themes: Vec<DesktopThemeOption>,
    /// Active `ui.keymap_preset`, surfaced read-only (a full remap UI is
    /// out of scope).
    keymap_preset: Option<String>,
    /// `ui.terminal_new_layout` as `"split"` / `"tabs"`.
    terminal_new_layout: String,
    /// `ui.activity_pane_default` as `"full"` / `"summary"` / `"hidden"`.
    activity_pane_default: String,
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
    #[serde(default = "default_terminal_layout")]
    terminal_new_layout: String,
    #[serde(default = "default_activity_pane")]
    activity_pane_default: String,
    #[serde(default)]
    default_model_tier: Option<String>,
}

fn default_terminal_layout() -> String {
    "split".to_string()
}

fn default_activity_pane() -> String {
    "full".to_string()
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
        theme: config.ui.theme.clone(),
        themes: theme_options(),
        keymap_preset: config.ui.keymap_preset.clone(),
        terminal_new_layout: terminal_layout_key(config.ui.terminal_new_layout).to_string(),
        activity_pane_default: activity_pane_key(config.ui.activity_pane_default).to_string(),
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

fn terminal_layout_key(layout: lazybox_config::NewTerminalLayout) -> &'static str {
    match layout {
        lazybox_config::NewTerminalLayout::Split => "split",
        lazybox_config::NewTerminalLayout::Tabs => "tabs",
    }
}

fn activity_pane_key(mode: lazybox_config::ActivityPaneMode) -> &'static str {
    match mode {
        lazybox_config::ActivityPaneMode::Full => "full",
        lazybox_config::ActivityPaneMode::Summary => "summary",
        lazybox_config::ActivityPaneMode::Hidden => "hidden",
    }
}

fn parse_terminal_layout(raw: &str) -> Result<lazybox_config::NewTerminalLayout, String> {
    match raw {
        "split" => Ok(lazybox_config::NewTerminalLayout::Split),
        "tabs" => Ok(lazybox_config::NewTerminalLayout::Tabs),
        other => Err(format!("unknown terminal layout {other:?}")),
    }
}

fn parse_activity_pane(raw: &str) -> Result<lazybox_config::ActivityPaneMode, String> {
    match raw {
        "full" => Ok(lazybox_config::ActivityPaneMode::Full),
        "summary" => Ok(lazybox_config::ActivityPaneMode::Summary),
        "hidden" => Ok(lazybox_config::ActivityPaneMode::Hidden),
        other => Err(format!("unknown activity-pane mode {other:?}")),
    }
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
fn save_desktop_settings(app: AppHandle, settings: SaveDesktopSettings) -> Result<bool, String> {
    let config = lazybox_config::Config::load()
        .map_err(|error| format!("load lazybox configuration: {error}"))?;
    let first_run = !config.setup.wizard_completed || !config.setup.providers.contains("github");
    let scopes = validate_github_scopes(settings.github_scopes, first_run)?;
    if !detect_agent_options(&config)
        .iter()
        .any(|agent| agent.id == settings.default_agent && agent.available)
    {
        return Err("select an installed agent".to_string());
    }
    validate_theme_change(settings.theme.as_deref(), config.ui.theme.as_deref())?;
    let terminal_new_layout = parse_terminal_layout(&settings.terminal_new_layout)?;
    let activity_pane_default = parse_activity_pane(&settings.activity_pane_default)?;
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
    // Only settings the daemon consumed at startup — the watched scopes
    // and the default agent baked into `DesktopState` — need a restart to
    // take effect. Theme / layout / activity-pane / model-tier changes are
    // read live (the client re-themes in place; the daemon re-reads the
    // model menu on the next spawn), so they apply without one.
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
        terminal_new_layout,
        activity_pane_default,
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
    let response = list_gateway_workspaces(&state.gateway).await?;
    // Seed the grouped-inbox model so `set_sort_mode` and the first
    // computed view have data even before the `Snapshot` event lands.
    state
        .inbox
        .lock()
        .await
        .seed_workspaces(&response.workspaces);
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
    if let Some(channel) = state.event_channel.lock().await.as_ref() {
        let _ = channel.send(DesktopStreamMessage::Inbox(Box::new(view)));
    }
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
async fn set_filters(state: State<'_, DesktopState>, filters: Vec<Filter>) -> Result<(), String> {
    let view = {
        let mut inbox = state.inbox.lock().await;
        inbox.set_filters(filters);
        inbox.compute()
    };
    if let Some(channel) = state.event_channel.lock().await.as_ref() {
        let _ = channel.send(DesktopStreamMessage::Inbox(Box::new(view)));
    }
    Ok(())
}

/// Set the global search query and re-emit the recomputed view. An
/// empty query clears the search (#733).
#[tauri::command]
async fn set_search(state: State<'_, DesktopState>, query: String) -> Result<(), String> {
    let view = {
        let mut inbox = state.inbox.lock().await;
        inbox.set_search(query);
        inbox.compute()
    };
    if let Some(channel) = state.event_channel.lock().await.as_ref() {
        let _ = channel.send(DesktopStreamMessage::Inbox(Box::new(view)));
    }
    Ok(())
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
    let inbox = state.inbox.clone();
    let event_channel = state.event_channel.clone();
    let snippets = state.snippets.clone();
    tauri::async_runtime::spawn(async move {
        *event_channel.lock().await = Some(on_event.clone());
        stream_control_events(control_gateway, on_event, inbox, snippets).await;
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

async fn stream_control_events(
    gateway: GatewayClient,
    on_event: Channel<DesktopStreamMessage>,
    inbox: Arc<Mutex<InboxModel>>,
    snippets: Arc<Mutex<SnippetModel>>,
) {
    loop {
        match stream_control_events_once(&gateway, &on_event, &inbox, &snippets).await {
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
    inbox: &Mutex<InboxModel>,
    snippets: &Mutex<SnippetModel>,
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
    // Emit the current grouped view immediately (seeded from
    // `list_workspaces`), before the daemon's own `Snapshot` arrives.
    emit_inbox_view(inbox, on_event).await;

    let mut decoder = NdjsonDecoder::default();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read control stream: {error}"))?
    {
        for frame in decoder.push(&chunk)? {
            let JsonServerFrame::Event(event) = frame;
            if let Some(event) = desktop_event(event) {
                // Keep the snippet MRU aligned with the daemon: seed from
                // every snapshot, advance on every delivery (from any
                // client). The frontend pulls a recomputed view per
                // keystroke, so there's no channel to push here.
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
                if on_event
                    .send(DesktopStreamMessage::Frame(Box::new(event)))
                    .is_err()
                {
                    return Ok(());
                }
                if recompute {
                    emit_inbox_view(inbox, on_event).await;
                }
            }
        }
    }
    decoder.finish()
}

/// Compute the grouped inbox view from the current model and push it to
/// the webview. A failed send means the webview reader is gone; callers
/// treat that as a benign disconnect.
async fn emit_inbox_view(inbox: &Mutex<InboxModel>, on_event: &Channel<DesktopStreamMessage>) {
    let view = inbox.lock().await.compute();
    let _ = on_event.send(DesktopStreamMessage::Inbox(Box::new(view)));
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

/// Pure mutation over the shared [`Config`] — the desktop's only writer,
/// applied under `Config::save_with`'s read-modify-write lock. Stays
/// side-effect-free so it's trivially testable and can never bypass the
/// atomic save path.
struct DesktopSettings {
    scopes: BTreeSet<String>,
    default_agent: String,
    analytics_enabled: bool,
    theme: Option<String>,
    terminal_new_layout: lazybox_config::NewTerminalLayout,
    activity_pane_default: lazybox_config::ActivityPaneMode,
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
    config.ui.theme = settings.theme;
    config.ui.terminal_new_layout = settings.terminal_new_layout;
    config.ui.activity_pane_default = settings.activity_pane_default;
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
    let inbox = InboxModel::new(
        user_config.ui.collapsed_repos.clone(),
        user_config.attention.clone(),
    );
    let snippets = SnippetModel::new(load_snippet_catalog());

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
        inbox: Arc::new(Mutex::new(inbox)),
        event_channel: Arc::new(Mutex::new(None)),
        snippets: Arc::new(Mutex::new(snippets)),
    })
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
            set_sort_mode,
            set_filters,
            set_search,
            snippet_view,
            set_filters,
            set_search,
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
        apply_desktop_settings(
            &mut config,
            DesktopSettings {
                scopes,
                default_agent: "codex".to_string(),
                analytics_enabled: true,
                theme: Some("Tokyo Night".to_string()),
                terminal_new_layout: lazybox_config::NewTerminalLayout::Tabs,
                activity_pane_default: lazybox_config::ActivityPaneMode::Summary,
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
        assert_eq!(saved.ui.theme.as_deref(), Some("Tokyo Night"));
        assert_eq!(
            saved.ui.terminal_new_layout,
            lazybox_config::NewTerminalLayout::Tabs
        );
        assert_eq!(
            saved.ui.activity_pane_default,
            lazybox_config::ActivityPaneMode::Summary
        );
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
        assert!(configured_agent_ids(&config).contains(&"claude".to_string()));
        // Appearance / workspace defaults surface for the settings UI, and
        // the theme catalog comes from the shared palette (not hardcoded TS).
        assert!(initial.theme.is_none());
        assert!(!initial.themes.is_empty());
        assert_eq!(initial.terminal_new_layout, "split");
        assert_eq!(initial.activity_pane_default, "full");
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
        assert!(configured_agent_ids(&config).contains(&"cursor-agent".to_string()));
        assert!(
            changed
                .agents
                .iter()
                .any(|agent| agent.id == "cursor-agent" && agent.label == "Cursor Agent")
        );
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
    fn boundary_parsers_accept_known_values_and_reject_the_rest() {
        assert_eq!(
            parse_terminal_layout("tabs").unwrap(),
            lazybox_config::NewTerminalLayout::Tabs
        );
        assert!(parse_terminal_layout("floating").is_err());
        assert_eq!(
            parse_activity_pane("hidden").unwrap(),
            lazybox_config::ActivityPaneMode::Hidden
        );
        assert!(parse_activity_pane("collapsed").is_err());
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
            DesktopSettings {
                scopes: BTreeSet::from(["github:acme/widget".to_string()]),
                default_agent: "claude".to_string(),
                analytics_enabled: false,
                theme: None,
                terminal_new_layout: lazybox_config::NewTerminalLayout::Split,
                activity_pane_default: lazybox_config::ActivityPaneMode::Full,
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
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: Some(if is_pr {
                lazybox_core::TaskKind::Pr
            } else {
                lazybox_core::TaskKind::Issue
            }),
            closes_issues: vec![],
        }
    }

    fn contract_workspace(repo: &str, number: u64, is_pr: bool) -> lazybox_core::Workspace {
        lazybox_core::Workspace::from_task(contract_task(repo, number, is_pr), chrono::Utc::now())
    }

    fn empty_model() -> InboxModel {
        InboxModel::new(BTreeSet::new(), lazybox_config::AttentionConfig::default())
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
        model.set_filters([Filter::Issue]);
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
        model.set_filters([]);
        assert_eq!(workspace_rows(&model.compute()), 2);
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
        model.set_search("2".into());
        assert_eq!(workspace_rows(&model.compute()), 1);
        // Clearing the query restores everything.
        model.set_search(String::new());
        assert_eq!(workspace_rows(&model.compute()), 2);
    }
}
