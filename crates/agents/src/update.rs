//! Out-of-band update channels for agent CLIs.
//!
//! Lazybox suppresses the agents' own in-session self-updaters (Claude
//! Code's auto-update banner fails mid-session; Codex's on-launch
//! updater drags in a Homebrew self-update — issue #355). The
//! [`UpdateChannel`] an agent advertises here is the sanctioned
//! replacement: the daemon runs these commands in plain bounded
//! subprocesses, never inside a live session PTY.

use std::path::{Path, PathBuf};

/// How lazybox checks and updates one agent's CLI out of band.
///
/// All three members are plain argv vectors the daemon runs as bounded
/// subprocesses. Version output is parsed with [`extract_version`], so
/// commands may print decoration around the number (`"2.1.4 (Claude
/// Code)"`, `"codex-cli 0.46.0"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateChannel {
    /// Prints the installed version.
    pub version_argv: Vec<String>,
    /// Prints the latest released version (a registry query). `None`
    /// when the agent has no queryable registry — the daemon then
    /// reports the installed version only and relies on
    /// `update_argv`'s own "already current" no-op for idempotence.
    pub latest_argv: Option<Vec<String>>,
    /// Performs the update via the package manager that installed the
    /// CLI.
    pub update_argv: Vec<String>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Claude Code's channel. `claude update` is the CLI's own updater and
/// handles both the npm-global and native-installer layouts; the npm
/// registry is the version source of truth for both.
pub fn claude_channel() -> UpdateChannel {
    UpdateChannel {
        version_argv: argv(&["claude", "--version"]),
        latest_argv: Some(argv(&[
            "npm",
            "view",
            "@anthropic-ai/claude-code",
            "version",
        ])),
        update_argv: argv(&["claude", "update"]),
    }
}

/// Codex's channel. The update command depends on how the binary was
/// installed — resolved from the canonicalized on-`PATH` location via
/// [`codex_update_argv`]. Latest always comes from the npm registry;
/// the brew cask tracks the same releases.
pub fn codex_channel() -> UpdateChannel {
    let bin = resolve_in_path("codex").and_then(|p| std::fs::canonicalize(p).ok());
    UpdateChannel {
        version_argv: argv(&["codex", "--version"]),
        latest_argv: Some(argv(&["npm", "view", "@openai/codex", "version"])),
        update_argv: codex_update_argv(bin.as_deref()),
    }
}

/// Pick Codex's update command from its resolved install location.
/// Homebrew distributes Codex as a cask today but shipped it as a
/// formula historically, and upgrading one through the other's flag
/// fails (openai/codex#5607) — so the flag follows the layout:
/// Caskroom → cask, Cellar → formula. Anything else (including an
/// unresolvable binary) falls back to npm, the other supported
/// channel.
pub fn codex_update_argv(canonical_bin: Option<&Path>) -> Vec<String> {
    let dir_of =
        |name: &str| canonical_bin.is_some_and(|p| p.components().any(|c| c.as_os_str() == name));
    if dir_of("Caskroom") {
        argv(&["brew", "upgrade", "--cask", "codex"])
    } else if dir_of("Cellar") {
        argv(&["brew", "upgrade", "--formula", "codex"])
    } else {
        argv(&["npm", "install", "-g", "@openai/codex@latest"])
    }
}

/// Cursor's channel. The CLI self-updates via its own `update`
/// subcommand; there is no public registry to query for the latest
/// version, so the check reports the installed version only.
pub fn cursor_channel() -> UpdateChannel {
    UpdateChannel {
        version_argv: argv(&["cursor-agent", "--version"]),
        latest_argv: None,
        update_argv: argv(&["cursor-agent", "update"]),
    }
}

/// First `PATH` entry containing an executable file named `name`.
fn resolve_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// First dotted numeric version in a command's output — out of
/// `"2.1.4 (Claude Code)"`, `"codex-cli 0.46.0"`, `"v2.1.0"`, or a
/// bare `"0.46.0"`. A prerelease suffix (`0.47.0-alpha.3`) is cut at
/// the numeric core: keeping it would fail [`is_newer`]'s numeric
/// parse and fall back to plain inequality, which reads a stable
/// registry release as "newer" than a same-or-higher prerelease and
/// offers a downgrade.
pub fn extract_version(output: &str) -> Option<String> {
    for token in output.split_whitespace() {
        let t = token.trim_start_matches(|c: char| !c.is_ascii_digit());
        let end = t
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(t.len());
        let head = t[..end].trim_end_matches('.');
        if head.contains('.')
            && head
                .split('.')
                .all(|p| !p.is_empty() && p.parse::<u64>().is_ok())
        {
            return Some(head.to_string());
        }
    }
    None
}

/// Whether `latest` is strictly newer than `installed`, comparing
/// dot-separated numeric segments (missing segments count as 0). A
/// non-numeric segment makes the comparison bail to plain inequality,
/// so an odd version string still reports "update available" rather
/// than silently pinning stale.
pub fn is_newer(latest: &str, installed: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> { v.split('.').map(|p| p.parse().ok()).collect() };
    match (parse(latest), parse(installed)) {
        (Some(l), Some(i)) => {
            let len = l.len().max(i.len());
            for idx in 0..len {
                let (a, b) = (
                    l.get(idx).copied().unwrap_or(0),
                    i.get(idx).copied().unwrap_or(0),
                );
                if a != b {
                    return a > b;
                }
            }
            false
        }
        _ => latest != installed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_version_from_real_cli_outputs() {
        assert_eq!(
            extract_version("2.1.215 (Claude Code)").as_deref(),
            Some("2.1.215")
        );
        assert_eq!(
            extract_version("codex-cli 0.144.6").as_deref(),
            Some("0.144.6")
        );
        assert_eq!(extract_version("0.144.6\n").as_deref(), Some("0.144.6"));
        assert_eq!(extract_version("no version here"), None);
        assert_eq!(extract_version(""), None);
    }

    #[test]
    fn extracts_numeric_core_from_prerelease_and_prefixed_versions() {
        assert_eq!(
            extract_version("codex-cli 0.47.0-alpha.3").as_deref(),
            Some("0.47.0")
        );
        assert_eq!(extract_version("v2.1.0").as_deref(), Some("2.1.0"));
        assert_eq!(
            extract_version("agent 1.2.3+build.7").as_deref(),
            Some("1.2.3")
        );
    }

    #[test]
    fn version_ordering_is_numeric_not_lexical() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("2.1.10", "2.1.9"));
        assert!(!is_newer("2.1.9", "2.1.10"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(is_newer("1.2.3.1", "1.2.3"));
        assert!(!is_newer("1.2", "1.2.0"));
    }

    #[test]
    fn odd_version_strings_fall_back_to_inequality() {
        assert!(is_newer("1.2.3-beta", "1.2.3"));
        assert!(!is_newer("1.2.3-beta", "1.2.3-beta"));
    }

    #[test]
    fn codex_update_matches_brew_layout_to_the_right_flag() {
        let cask = Path::new("/opt/homebrew/Caskroom/codex/0.144.6/codex-aarch64-apple-darwin");
        assert_eq!(
            codex_update_argv(Some(cask)),
            vec!["brew", "upgrade", "--cask", "codex"]
        );
        // A legacy formula install must NOT get the cask flag —
        // `brew upgrade --cask` refuses to touch a Cellar install
        // (openai/codex#5607).
        let formula = Path::new("/usr/local/Cellar/codex/1.0.0/bin/codex");
        assert_eq!(
            codex_update_argv(Some(formula)),
            vec!["brew", "upgrade", "--formula", "codex"]
        );
    }

    #[test]
    fn codex_update_falls_back_to_npm() {
        let npm = Path::new("/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js");
        assert_eq!(
            codex_update_argv(Some(npm)),
            vec!["npm", "install", "-g", "@openai/codex@latest"]
        );
        assert_eq!(
            codex_update_argv(None),
            vec!["npm", "install", "-g", "@openai/codex@latest"]
        );
    }
}
