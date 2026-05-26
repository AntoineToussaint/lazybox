//! Wraps an inner command in a session manager so the process survives
//! the daemon quitting. Default is tmux; the trait lets us slot in
//! `screen`, `zellij`, or "just a raw PTY, no wrapper" for constrained
//! environments.

use std::path::Path;

/// Recipe for launching a command so its process outlives the parent.
pub trait SessionWrapper: Send + Sync {
    /// Stable id — "tmux", "screen", "zellij", "raw".
    fn id(&self) -> &'static str;

    /// Transform an inner argv into a wrapped argv. Given `inner = ["claude", "--continue"]`
    /// and `key = "github_acme_widget#7305"`, tmux returns
    /// something like `["tmux", "new-session", "-A", "-s", "<key>", "claude --continue"]`.
    fn wrap(&self, session_id: &str, inner: &[String], cwd: &Path) -> Vec<String>;

    /// Sanitize a pilot session key into whatever format the wrapper
    /// accepts. Tmux allows most printables but splits on colons.
    fn sanitize_id(&self, pilot_key: &str) -> String {
        pilot_key.replace([':', '/'], "_")
    }

    /// List sessions managed by this wrapper that pilot previously
    /// created. Used on daemon startup to auto-reattach.
    fn list_sessions(&self) -> Vec<String>;

    /// Kill a single wrapper-managed session by id.
    fn kill(&self, session_id: &str) -> std::io::Result<()>;
}

/// tmux wrapper — the default.
pub struct TmuxWrapper;

impl TmuxWrapper {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TmuxWrapper {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionWrapper for TmuxWrapper {
    fn id(&self) -> &'static str {
        "tmux"
    }

    fn wrap(&self, session_id: &str, inner: &[String], _cwd: &Path) -> Vec<String> {
        // -A: attach if exists, create if not. `_cwd` is unused because
        // tmux uses the parent process's cwd at spawn time; the daemon
        // sets cwd on the Command it runs.
        let mut argv = vec![
            "tmux".into(),
            "new-session".into(),
            "-A".into(),
            "-s".into(),
            self.sanitize_id(session_id),
        ];
        argv.push(inner.join(" "));
        argv
    }

    fn list_sessions(&self) -> Vec<String> {
        let Ok(out) = std::process::Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
        else {
            return vec![];
        };
        if !out.status.success() {
            return vec![];
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn kill(&self, session_id: &str) -> std::io::Result<()> {
        let out = std::process::Command::new("tmux")
            .args(["kill-session", "-t", session_id])
            .output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "tmux kill-session: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

/// No-wrapper fallback — runs the inner argv directly. The process
/// dies when pilot quits. Useful for constrained environments (CI,
/// containers without tmux) where persistence isn't a goal.
pub struct RawWrapper;

impl SessionWrapper for RawWrapper {
    fn id(&self) -> &'static str {
        "raw"
    }

    fn wrap(&self, _session_id: &str, inner: &[String], _cwd: &Path) -> Vec<String> {
        inner.to_vec()
    }

    fn list_sessions(&self) -> Vec<String> {
        vec![]
    }

    fn kill(&self, _session_id: &str) -> std::io::Result<()> {
        Ok(())
    }
}
