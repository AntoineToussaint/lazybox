use lazybox_config::Config;
use lazybox_core::{ProviderConfig, Scope};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use std::collections::BTreeSet;

const AGENTS: [(&str, &str, &str); 3] = [
    ("claude", "Claude Code", "claude"),
    ("codex", "Codex", "codex"),
    ("cursor-agent", "Cursor Agent", "cursor-agent"),
];

#[derive(Debug, Clone, Serialize)]
pub struct DesktopSetupStatus {
    pub completed: bool,
    pub github: ToolStatus,
    pub agents: Vec<ToolStatus>,
    pub selected_scopes: Vec<String>,
    pub default_agent: Option<String>,
    pub analytics_enabled: bool,
    pub crash_reports_enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DesktopScope {
    pub id: String,
    pub label: String,
    pub parent: Option<String>,
}

impl From<Scope> for DesktopScope {
    fn from(scope: Scope) -> Self {
        Self {
            id: scope.id,
            label: scope.label,
            parent: scope.parent,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DesktopSetupInput {
    pub github_scopes: Vec<String>,
    pub default_agent: String,
    pub analytics_enabled: bool,
    pub crash_reports_enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyticsEvent {
    OnboardingCompleted,
    WorkspaceOpened,
    AgentStarted,
    ShellStarted,
    ReplyPosted,
}

impl AnalyticsEvent {
    fn name(self) -> &'static str {
        match self {
            Self::OnboardingCompleted => "onboarding_completed",
            Self::WorkspaceOpened => "workspace_opened",
            Self::AgentStarted => "agent_started",
            Self::ShellStarted => "shell_started",
            Self::ReplyPosted => "reply_posted",
        }
    }
}

pub async fn status() -> Result<DesktopSetupStatus, String> {
    let config = Config::load().map_err(|error| format!("load settings: {error}"))?;
    let (github, agents) = tokio::join!(github_status(), detect_agents(&config));
    let selected_scopes = config
        .setup
        .scopes
        .get("github")
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    Ok(DesktopSetupStatus {
        completed: config.setup.wizard_completed,
        github,
        agents,
        selected_scopes,
        default_agent: config.setup.default_agent,
        analytics_enabled: config.desktop.analytics_enabled,
        crash_reports_enabled: config.desktop.crash_reports_enabled,
    })
}

pub async fn github_organizations() -> Result<Vec<DesktopScope>, String> {
    let client = github_client().await?;
    client
        .list_scopes()
        .await
        .map(|scopes| scopes.into_iter().map(Into::into).collect())
        .map_err(|_| "GitHub organization discovery failed".to_string())
}

pub async fn github_repositories(parent_id: &str) -> Result<Vec<DesktopScope>, String> {
    validate_parent_scope(parent_id)?;
    let client = github_client().await?;
    client
        .list_repos_in_org(parent_id)
        .await
        .map(|scopes| scopes.into_iter().map(Into::into).collect())
        .map_err(|_| "GitHub repository discovery failed".to_string())
}

pub async fn begin_github_login() -> Result<(), String> {
    if !command_succeeds("gh", &["--version"], Duration::from_secs(3)).await {
        return Err("GitHub CLI is not installed. Install it from cli.github.com first.".into());
    }
    #[cfg(target_os = "macos")]
    {
        let script = concat!(
            "tell application \"Terminal\"\n",
            "activate\n",
            "do script \"gh auth login --hostname github.com --git-protocol https --web\"\n",
            "end tell"
        );
        let status = tokio::process::Command::new("osascript")
            .args(["-e", script])
            .status()
            .await
            .map_err(|error| format!("open GitHub authentication: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("GitHub authentication could not be opened".into())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("GitHub authentication from the desktop app requires macOS".into())
    }
}

pub fn save(input: DesktopSetupInput) -> Result<(), String> {
    let config = Config::load().map_err(|error| format!("load desktop settings: {error}"))?;
    validate_input(&input, &config, !config.setup.wizard_completed)?;
    Config::save_with(|config| apply_input(config, &input))
        .map_err(|error| format!("save desktop settings: {error}"))
}

pub fn record_analytics(event: AnalyticsEvent) -> Result<bool, String> {
    let config = Config::load().map_err(|error| format!("load privacy settings: {error}"))?;
    if !config.desktop.analytics_enabled {
        return Ok(false);
    }
    tracing::info!(
        target: "desktop_analytics",
        event = event.name(),
        app_version = env!("CARGO_PKG_VERSION"),
        "desktop analytics event"
    );
    Ok(true)
}

pub fn install_crash_hook() {
    let enabled = Config::load()
        .map(|config| config.desktop.crash_reports_enabled)
        .unwrap_or(false);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if enabled {
            let _ = write_crash_diagnostic(info.location());
        }
        default(info);
    }));
}

#[cfg(target_os = "macos")]
pub fn hydrate_gui_path() {
    let config = Config::load().unwrap_or_default();
    let shell = config.shell.resolved_command();
    let Some(login_path) = login_shell_path(&shell) else {
        return;
    };
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let Some(path) = merge_paths(&login_path, &inherited) else {
        return;
    };
    // This runs before Tauri or Tokio starts any threads, as required by set_var.
    unsafe {
        std::env::set_var("PATH", path);
    }
}

fn login_shell_path(shell: &str) -> Option<OsString> {
    const MARKER: &str = "__LAZYBOX_LOGIN_PATH__";
    let output = Command::new(shell)
        .args(["-lc", "printf '\\n__LAZYBOX_LOGIN_PATH__%s\\n' \"$PATH\""])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(MARKER).map(OsString::from))
}

fn merge_paths(login_path: &OsStr, inherited_path: &OsStr) -> Option<OsString> {
    let mut seen = HashSet::new();
    let paths = std::env::split_paths(login_path)
        .chain(std::env::split_paths(inherited_path))
        .filter(|path| seen.insert(path.clone()))
        .collect::<Vec<_>>();
    std::env::join_paths(paths).ok()
}

async fn github_status() -> ToolStatus {
    let gh_installed = command_succeeds("gh", &["--version"], Duration::from_secs(3)).await;
    match tokio::time::timeout(
        Duration::from_secs(5),
        lazybox_gh::credential_chain().resolve(lazybox_gh::SOURCE),
    )
    .await
    {
        Ok(Ok(credential)) => match tokio::time::timeout(
            Duration::from_secs(10),
            lazybox_gh::GhClient::from_credential(credential),
        )
        .await
        {
            Ok(Ok(client)) => ToolStatus {
                id: "github".into(),
                label: "GitHub".into(),
                available: true,
                detail: format!("Authenticated as @{}", client.authenticated_user()),
            },
            _ => ToolStatus {
                id: "github".into(),
                label: "GitHub".into(),
                available: false,
                detail: "The saved GitHub credential is no longer valid.".into(),
            },
        },
        _ => ToolStatus {
            id: "github".into(),
            label: "GitHub".into(),
            available: false,
            detail: if gh_installed {
                "Sign in with GitHub CLI to continue.".into()
            } else {
                "Install GitHub CLI, then sign in.".into()
            },
        },
    }
}

async fn detect_agents(config: &Config) -> Vec<ToolStatus> {
    let futures = AGENTS.map(|(id, label, command)| detect_agent(id, label, command));
    let mut statuses = futures_util::future::join_all(futures).await;
    for id in &config.setup.agents {
        if AGENTS.iter().any(|(builtin, _, _)| builtin == id) {
            continue;
        }
        let Some(entry) = config.agents.get(id) else {
            statuses.push(ToolStatus {
                id: id.clone(),
                label: id.clone(),
                available: false,
                detail: "Missing agent configuration".into(),
            });
            continue;
        };
        let Some(argv) = entry.spawn_argv() else {
            statuses.push(ToolStatus {
                id: id.clone(),
                label: entry.name.clone().unwrap_or_else(|| id.clone()),
                available: false,
                detail: "Missing agent command".into(),
            });
            continue;
        };
        statuses.push(ToolStatus {
            id: id.clone(),
            label: entry.name.clone().unwrap_or_else(|| id.clone()),
            available: true,
            detail: format!("Configured: {}", argv[0]),
        });
    }
    statuses
}

async fn detect_agent(id: &str, label: &str, command: &str) -> ToolStatus {
    let output = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::process::Command::new(command)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) if output.status.success() => ToolStatus {
            id: id.into(),
            label: label.into(),
            available: true,
            detail: String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("Available")
                .trim()
                .to_string(),
        },
        _ => ToolStatus {
            id: id.into(),
            label: label.into(),
            available: false,
            detail: "Not installed".into(),
        },
    }
}

async fn github_client() -> Result<lazybox_gh::GhClient, String> {
    let credential = tokio::time::timeout(
        Duration::from_secs(5),
        lazybox_gh::credential_chain().resolve(lazybox_gh::SOURCE),
    )
    .await
    .map_err(|_| "GitHub authentication timed out".to_string())?
    .map_err(|_| "GitHub is not authenticated".to_string())?;
    tokio::time::timeout(
        Duration::from_secs(10),
        lazybox_gh::GhClient::from_credential(credential),
    )
    .await
    .map_err(|_| "GitHub authentication timed out".to_string())?
    .map_err(|_| "GitHub authentication failed".to_string())
}

async fn command_succeeds(command: &str, args: &[&str], timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(
            timeout,
            tokio::process::Command::new(command)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
        )
        .await,
        Ok(Ok(status)) if status.success()
    )
}

fn validate_parent_scope(parent_id: &str) -> Result<(), String> {
    let owner = parent_id.strip_prefix("github:").unwrap_or_default();
    if parent_id.len() > 200 || owner.is_empty() || owner.contains('/') {
        return Err("invalid GitHub organization scope".into());
    }
    Ok(())
}

fn validate_input(
    input: &DesktopSetupInput,
    config: &Config,
    require_explicit_scope: bool,
) -> Result<(), String> {
    let configured_custom_agent = config
        .agents
        .get(&input.default_agent)
        .and_then(|entry| entry.spawn_argv())
        .is_some();
    if !AGENTS.iter().any(|(id, _, _)| *id == input.default_agent) && !configured_custom_agent {
        return Err("unsupported default agent".into());
    }
    if require_explicit_scope && input.github_scopes.is_empty() {
        return Err("select at least one GitHub scope".into());
    }
    if input.github_scopes.len() > 256
        || input
            .github_scopes
            .iter()
            .any(|scope| !valid_github_scope(scope))
    {
        return Err("invalid GitHub scope selection".into());
    }
    Ok(())
}

fn valid_github_scope(scope: &str) -> bool {
    let Some(slug) = scope.strip_prefix("github:") else {
        return false;
    };
    let mut parts = slug.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next();
    scope.len() <= 300
        && !owner.is_empty()
        && repo.is_none_or(|repo| !repo.is_empty())
        && parts.next().is_none()
}

fn apply_input(config: &mut Config, input: &DesktopSetupInput) {
    config.setup.providers.insert("github".to_string());
    config.setup.agents.insert(input.default_agent.clone());
    config
        .setup
        .filters
        .entry("github".into())
        .or_insert_with(|| ProviderConfig::default_for("github").enabled_keys);
    config.setup.scopes.insert(
        "github".into(),
        input.github_scopes.iter().cloned().collect(),
    );
    config.setup.default_agent = Some(input.default_agent.clone());
    config.setup.wizard_completed = true;
    config.desktop.analytics_enabled = input.analytics_enabled;
    config.desktop.crash_reports_enabled = input.crash_reports_enabled;
}

fn write_crash_diagnostic(location: Option<&std::panic::Location<'_>>) -> std::io::Result<()> {
    let path = lazybox_core::paths::state_root().join("desktop-crash.log");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let source = location
        .and_then(|location| Path::new(location.file()).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let line = location.map(std::panic::Location::line).unwrap_or(0);
    writeln!(file, "{}", crash_diagnostic_line(timestamp, source, line))
}

fn crash_diagnostic_line(timestamp: u64, source: &str, line: u32) -> String {
    format!(
        "timestamp={timestamp} version={} source={source}:{line}",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> DesktopSetupInput {
        DesktopSetupInput {
            github_scopes: vec!["github:owner/repo".into()],
            default_agent: "codex".into(),
            analytics_enabled: true,
            crash_reports_enabled: false,
        }
    }

    #[test]
    fn setup_input_updates_only_desktop_owned_configuration() {
        let mut config = Config::default();
        config.setup.providers.insert("linear".into());
        config.setup.agents.insert("custom".into());
        config
            .setup
            .filters
            .insert("github".into(), BTreeSet::from(["review-requested".into()]));
        config
            .repos
            .insert("owner/other".into(), Default::default());
        apply_input(&mut config, &input());

        assert_eq!(
            config.setup.providers,
            BTreeSet::from(["github".into(), "linear".into()])
        );
        assert_eq!(
            config.setup.agents,
            BTreeSet::from(["codex".into(), "custom".into()])
        );
        assert_eq!(
            config.setup.filters["github"],
            BTreeSet::from(["review-requested".into()])
        );
        assert_eq!(
            config.setup.scopes["github"],
            BTreeSet::from(["github:owner/repo".into()])
        );
        assert_eq!(config.setup.default_agent.as_deref(), Some("codex"));
        assert!(config.setup.wizard_completed);
        assert!(config.desktop.analytics_enabled);
        assert!(!config.desktop.crash_reports_enabled);
        assert!(config.repos.contains_key("owner/other"));
    }

    #[test]
    fn desktop_setup_round_trips_through_the_real_config_file() {
        let root = std::env::temp_dir().join(format!(
            "lazybox-desktop-setup-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let path = root.join("config.yaml");
        let mut config = Config::default();
        apply_input(&mut config, &input());
        config.save_to(&path).expect("save setup");
        let reloaded = Config::load_from(&path).expect("reload setup");

        assert_eq!(reloaded.setup.default_agent.as_deref(), Some("codex"));
        assert_eq!(
            reloaded.setup.scopes["github"],
            BTreeSet::from(["github:owner/repo".into()])
        );
        assert!(reloaded.desktop.analytics_enabled);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn setup_validation_accepts_shared_scope_and_agent_contracts() {
        let mut config = Config::default();
        config.agents.insert(
            "custom".into(),
            lazybox_config::AgentEntry {
                command: Some("custom-cli".into()),
                ..Default::default()
            },
        );
        let mut valid = input();
        valid.github_scopes = vec!["github:owner".into()];
        valid.default_agent = "cursor-agent".into();
        assert!(validate_input(&valid, &config, true).is_ok());

        valid.github_scopes.clear();
        valid.default_agent = "custom".into();
        assert!(validate_input(&valid, &config, false).is_ok());
    }

    #[test]
    fn first_run_still_requires_a_scope_and_rejects_unknown_agents() {
        let config = Config::default();
        let mut invalid = input();
        invalid.github_scopes.clear();
        assert!(validate_input(&invalid, &config, true).is_err());

        invalid = input();
        invalid.default_agent = "arbitrary-command".into();
        assert!(validate_input(&invalid, &config, true).is_err());
    }

    #[tokio::test]
    async fn configured_agents_are_returned_with_daemon_registry_ids() {
        let mut config = Config::default();
        config
            .setup
            .agents
            .extend(["cursor-agent".into(), "custom".into()]);
        config.agents.insert(
            "custom".into(),
            lazybox_config::AgentEntry {
                name: Some("Custom Agent".into()),
                command: Some("custom-cli".into()),
                ..Default::default()
            },
        );

        let agents = detect_agents(&config).await;

        assert!(agents.iter().any(|agent| agent.id == "cursor-agent"));
        assert!(agents.iter().any(|agent| {
            agent.id == "custom"
                && agent.label == "Custom Agent"
                && agent.available
                && agent.detail == "Configured: custom-cli"
        }));
    }

    #[test]
    fn analytics_boundary_has_only_content_free_events() {
        let value = serde_json::from_str::<AnalyticsEvent>("\"agent_started\"")
            .expect("known event deserializes");
        assert_eq!(value.name(), "agent_started");
        assert!(serde_json::from_str::<AnalyticsEvent>("\"owner/repo#123\"").is_err());
    }

    #[test]
    fn setup_status_serialization_never_has_a_credential_field() {
        let status = DesktopSetupStatus {
            completed: false,
            github: ToolStatus {
                id: "github".into(),
                label: "GitHub".into(),
                available: true,
                detail: "Authenticated as @fixture".into(),
            },
            agents: Vec::new(),
            selected_scopes: Vec::new(),
            default_agent: None,
            analytics_enabled: false,
            crash_reports_enabled: false,
        };
        let json = serde_json::to_value(status).expect("serialize setup status");
        let object = json.as_object().expect("status object");
        assert!(!object.contains_key("token"));
        assert!(!object.contains_key("credential"));
        assert!(!object.contains_key("gateway"));
    }

    #[test]
    fn crash_diagnostic_has_only_the_documented_metadata() {
        assert_eq!(
            crash_diagnostic_line(42, "main.rs", 7),
            format!(
                "timestamp=42 version={} source=main.rs:7",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn login_path_precedes_and_deduplicates_the_gui_path() {
        let merged = merge_paths(
            OsStr::new("/opt/homebrew/bin:/usr/bin:/bin"),
            OsStr::new("/usr/bin:/bin:/usr/sbin"),
        )
        .expect("merge paths");

        assert_eq!(
            merged,
            OsString::from("/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin")
        );
    }
}
