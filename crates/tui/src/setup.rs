//! Result types for startup detection — which agents and providers are
//! usable on this machine — rendered by the first-run setup screen.
//!
//! The probing that populates a [`SetupReport`] reaches the GitHub /
//! Linear provider clients, so it lives boot-side in `lazybox-tui-boot`'s
//! `setup_detect` and is injected into the wizard as a `SetupDetector`
//! hook (#548). These types stay here because the wizard renders them.

/// One row on the setup screen: a tool we tried to detect plus what we
/// found. `display_name` is the human-readable label; `id` is a short
/// machine name used for tests and telemetry.
#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub id: &'static str,
    pub display_name: &'static str,
    pub category: Category,
    pub state: ToolState,
    /// Shown under the row when state is `Missing`. Empty string if
    /// the tool is found or has no useful install pointer.
    pub install_hint: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Provider,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    /// Binary present (or env var set) and verified. `detail`
    /// describes what we found — e.g. "@username", "claude 1.0.42".
    Found { detail: String },
    /// Tool isn't usable. `kind` distinguishes a missing CLI binary
    /// from a present-but-unauthenticated CLI from a present-but-
    /// invalid token, etc. `hint` is what the user should run.
    Missing { kind: MissingKind, hint: String },
}

/// Why a tool isn't usable. Drives both the message ("CLI not
/// detected" vs "not authenticated") and the install hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingKind {
    /// CLI binary not on PATH.
    CliNotInstalled,
    /// CLI installed but no token / auth not run.
    NotAuthenticated,
    /// Token / API key present but rejected by the API.
    TokenInvalid,
    /// Required env var not set.
    EnvVarMissing,
}

impl MissingKind {
    /// Short, user-facing state. Deliberately doesn't include the
    /// command — the user knows how to authenticate their tools.
    pub fn label(self) -> &'static str {
        match self {
            Self::CliNotInstalled => "CLI not detected",
            Self::NotAuthenticated => "CLI found, please authenticate",
            Self::TokenInvalid => "auth invalid",
            Self::EnvVarMissing => "not authenticated",
        }
    }
}

impl ToolState {
    pub fn is_found(&self) -> bool {
        matches!(self, ToolState::Found { .. })
    }
}

/// Aggregate of every detection result.
#[derive(Debug, Clone)]
pub struct SetupReport {
    pub tools: Vec<ToolStatus>,
}

impl SetupReport {
    /// Lazybox needs at least one task source AND at least one agent to
    /// be useful. Anything weaker than that and the user gets an empty
    /// sidebar or a broken `c` keystroke — better to surface the
    /// problem upfront than fail silently.
    pub fn is_ready(&self) -> bool {
        let has_provider = self
            .tools
            .iter()
            .any(|t| t.category == Category::Provider && t.state.is_found());
        let has_agent = self
            .tools
            .iter()
            .any(|t| t.category == Category::Agent && t.state.is_found());
        has_provider && has_agent
    }

    pub fn find(&self, id: &str) -> Option<&ToolStatus> {
        self.tools.iter().find(|t| t.id == id)
    }
}
