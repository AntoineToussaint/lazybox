//! Startup detection: which agents and providers are usable on this
//! machine. Drives the first-run setup screen and is also useful as a
//! standalone diagnostic (`lazybox doctor` later).
//!
//! The result types (`SetupReport`, `ToolStatus`, …) live in the UI
//! library (`lazybox_tui::setup`) because the wizard renders them. The
//! probing here reaches the GitHub / Linear provider clients, so it is
//! quarantined boot-side (#548); the wizard's `r` refresh runs it through
//! the injected `SetupDetector` hook.
//!
//! ## What gets detected
//!
//! - **Agents** — `claude`, `codex`, `cursor`. Probed by spawning the
//!   binary with `--version` and looking for a 0 exit.
//! - **GitHub** — the same credential chain the daemon uses
//!   (`GH_TOKEN` → `GITHUB_TOKEN` → `gh auth token`).
//! - **Linear** — `LINEAR_API_KEY` env var present and non-empty.
//!
//! Detection is fully concurrent: each probe runs as its own future and
//! they're joined together, so a slow `claude --version` doesn't delay
//! `gh auth status`.

use lazybox_tui::setup::{Category, MissingKind, SetupReport, ToolState, ToolStatus};
use std::time::Duration;

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
    // Bounded: `gh --version` is instant when healthy, but a broken
    // install (hung shim, dead network FS on PATH) must degrade to
    // "not present" instead of stalling setup detection.
    let gh_present = match tokio::time::timeout(
        Duration::from_secs(3),
        tokio::task::spawn_blocking(|| {
            std::process::Command::new("gh")
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }),
    )
    .await
    {
        Ok(Ok(present)) => present,
        _ => false,
    };

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
