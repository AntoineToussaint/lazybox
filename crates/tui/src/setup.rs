//! Startup detection: which agents and providers are usable on this
//! machine. Drives the first-run setup screen and is also useful as a
//! standalone diagnostic (`lazybox doctor` later).
//!
//! ## What gets detected
//!
//! - **Agents** — `claude`, `codex`, `cursor`. Probed by spawning the
//!   binary with `--version` and looking for a 0 exit. Cheaper than
//!   parsing PATH ourselves and matches what the daemon will do when
//!   it actually spawns the agent.
//! - **GitHub** — the same credential chain the daemon uses
//!   (`GH_TOKEN` → `GITHUB_TOKEN` → `gh auth token`). If any provider
//!   resolves a token we report ready; we don't try to validate it
//!   against the API here because that would slow down startup and
//!   require network.
//! - **Linear** — `LINEAR_API_KEY` env var present and non-empty.
//!
//! Detection is fully concurrent: each probe runs as its own future
//! and they're joined together, so a slow `claude --version` doesn't
//! delay `gh auth status`.

use std::time::Duration;

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

/// Run all probes concurrently. Bounded — every probe has its own
/// timeout so a hanging `gh auth token` can't block startup forever.
pub async fn detect_all() -> SetupReport {
    let (claude, codex, cursor, github, linear) = tokio::join!(
        detect_binary("claude", "Claude Code", "https://claude.ai/code"),
        detect_binary("codex", "Codex", "npm install -g @openai/codex"),
        // Probe `cursor-agent` rather than `cursor` because that's the
        // CLI binary the agent registry expects to spawn. Users may
        // have the IDE installed without the headless agent.
        detect_binary("cursor-agent", "Cursor Agent", "https://cursor.com/cli"),
        detect_github(),
        detect_linear(),
    );
    SetupReport {
        tools: vec![github, linear, claude, codex, cursor],
    }
}

async fn detect_binary(
    bin: &'static str,
    display_name: &'static str,
    install_hint: &'static str,
) -> ToolStatus {
    let mk = |state| ToolStatus {
        id: bin,
        display_name,
        category: Category::Agent,
        state,
        install_hint,
    };

    // Two-stage probe:
    //
    // 1. PATH lookup — fast, blocks for ms. Just answers "is this
    //    binary executable from this shell?". `which` returns
    //    `Err(NotFound)` cleanly when missing; anything else means
    //    we'll call the binary in stage 2.
    //
    // 2. `--version` for the detail string. Allowed to take up to
    //    8s because Claude Code in particular loads a lot of JS at
    //    startup; the previous 2s timeout reported it as "CLI not
    //    detected" on slower machines / cold starts.
    let path_lookup = tokio::task::spawn_blocking(move || which::which(bin)).await;
    let path = match path_lookup {
        Ok(Ok(p)) => p,
        _ => {
            return mk(ToolState::Missing {
                kind: MissingKind::CliNotInstalled,
                hint: install_hint.to_string(),
            });
        }
    };

    // Binary exists. Now ask its version.
    let probe = async {
        tokio::process::Command::new(&path)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
    };
    match tokio::time::timeout(Duration::from_secs(8), probe).await {
        Ok(Ok(output)) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout);
            let detail = raw.lines().next().unwrap_or(bin).trim().to_string();
            mk(ToolState::Found { detail })
        }
        // Binary IS on PATH — `which` confirmed — but its --version
        // misbehaved (timed out, non-zero exit). Still report it as
        // Found so the user can use it; just no version string.
        _ => {
            tracing::warn!(
                "{bin} exists at {} but --version failed; reporting as Found anyway",
                path.display()
            );
            mk(ToolState::Found {
                detail: "version unknown".into(),
            })
        }
    }
}

async fn detect_github() -> ToolStatus {
    let mk = |state| ToolStatus {
        id: "github",
        display_name: "GitHub",
        category: Category::Provider,
        state,
        install_hint: "gh auth login",
    };

    // Step 1: is the `gh` CLI on PATH at all? Knowing the binary is
    // missing tells the user to install — different problem from
    // "installed but no token."
    let gh_present = tokio::task::spawn_blocking(|| {
        std::process::Command::new("gh")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    let cred = match tokio::time::timeout(
        Duration::from_secs(3),
        lazybox_gh::credential_chain().resolve(lazybox_gh::SOURCE),
    )
    .await
    {
        Ok(Ok(c)) => c,
        _ => {
            // No token. Distinguish "no CLI" from "CLI but not auth'd".
            if !gh_present
                && std::env::var("GH_TOKEN").is_err()
                && std::env::var("GITHUB_TOKEN").is_err()
            {
                return mk(ToolState::Missing {
                    kind: MissingKind::CliNotInstalled,
                    hint: "brew install gh".into(),
                });
            }
            return mk(ToolState::Missing {
                kind: MissingKind::NotAuthenticated,
                hint: "gh auth login".into(),
            });
        }
    };

    // Token resolved — verify it actually works by hitting the user
    // endpoint. A live username is more reassuring (and more
    // actionable) than just "I have a string."
    match tokio::time::timeout(
        Duration::from_secs(5),
        lazybox_gh::GhClient::from_credential(cred),
    )
    .await
    {
        Ok(Ok(client)) => mk(ToolState::Found {
            detail: format!("@{}", client.authenticated_user()),
        }),
        // Token present but rejected — distinct from "no token". The
        // user needs to rotate the credential, not run `gh auth login`
        // (which might just regenerate the same dead token from the
        // device flow if their account is in a weird state).
        Ok(Err(_)) | Err(_) => mk(ToolState::Missing {
            kind: MissingKind::TokenInvalid,
            hint: "rotate token: gh auth refresh".into(),
        }),
    }
}

async fn detect_linear() -> ToolStatus {
    let mk = |state| ToolStatus {
        id: "linear",
        display_name: "Linear",
        category: Category::Provider,
        state,
        install_hint: "export LINEAR_API_KEY=lin_api_…",
    };

    if !matches!(std::env::var("LINEAR_API_KEY"), Ok(v) if !v.is_empty()) {
        return mk(ToolState::Missing {
            kind: MissingKind::EnvVarMissing,
            hint: "export LINEAR_API_KEY=lin_api_…".into(),
        });
    }

    match lazybox_linear::LinearClient::from_env() {
        Ok(_) => mk(ToolState::Found {
            detail: "LINEAR_API_KEY set".into(),
        }),
        Err(_) => mk(ToolState::Missing {
            kind: MissingKind::TokenInvalid,
            hint: "check LINEAR_API_KEY value".into(),
        }),
    }
}
