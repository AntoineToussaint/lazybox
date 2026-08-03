//! Per-profile root directory + helpers.
//!
//! Lazybox used to hardcode `~/.lazybox` everywhere — fine for a single
//! installation, but it made running a second instance impossible
//! (two daemons would corrupt each other's `state.db`, claim the
//! same daemon socket, fight over the same tmux sessions). This
//! module is the single chokepoint.
//!
//! ## Override
//!
//! Set `LAZYBOX_HOME` to point at a different directory. Everything
//! lazybox writes — state DB, worktrees, daemon socket, config,
//! hooks, tmux sessions — lives under it.
//!
//! ```bash
//! # Default: ~/.lazybox
//! lazybox
//!
//! # Side-by-side dev instance:
//! LAZYBOX_HOME=~/.lazybox-dev cargo run -p lazybox-tui
//! ```
//!
//! ## What's shared between profiles
//!
//! - Your GitHub token (resolved via `gh auth token` or env vars).
//! - Your GitHub per-user rate-limit budget. Two instances polling
//!   in parallel still hit the same 5000/hr ceiling, so bump dev's
//!   `~/.lazybox-dev/config.yaml::providers.github.poll_interval` to
//!   leave headroom.
//!
//! ## Tmux socket disambiguation
//!
//! [`tmux_socket_name`] derives a unique socket label from the home
//! dir's last component: `~/.lazybox` → `lazybox`, `~/.lazybox-dev` →
//! `lazybox-dev`. So `tmux -L lazybox attach` shows the stable instance,
//! `tmux -L lazybox-dev attach` shows the dev one — no cross-talk.

use std::path::PathBuf;

/// Profile root. Defaults to `$HOME/.lazybox`; override with
/// `LAZYBOX_HOME`.
///
/// **Why an env var, not a CLI flag.** The polling task, the daemon
/// socket-service, the spawn handler, and the config loader all
/// resolve paths independently — threading a `--profile` arg
/// through every entry point is a lot of plumbing for the same
/// outcome. `LAZYBOX_HOME=path lazybox` reads identically to a flag
/// and works for every subcommand (`lazybox`, `lazybox server start`,
/// `lazybox server api`) without per-subcommand wiring.
pub fn home() -> PathBuf {
    if let Ok(dir) = std::env::var("LAZYBOX_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".lazybox")
}

/// State-versioned subdirectory under `home()`. Today: `<home>/v2`.
/// Wrap so a future schema bump (v3) is one constant change.
pub fn state_root() -> PathBuf {
    home().join("v2")
}

/// SQLite state DB. `<home>/v2/state.db`.
pub fn state_db() -> PathBuf {
    state_root().join("state.db")
}

/// Worktree base. `<home>/v2/worktrees/`.
pub fn worktrees_root() -> PathBuf {
    state_root().join("worktrees")
}

/// Durable per-terminal scrollback. `<home>/v2/scrollback/`. Each file
/// holds the recent raw output bytes of one session's terminal so its
/// history survives a daemon restart; the in-memory replay ring alone
/// is destroyed when the daemon exits.
pub fn scrollback_dir() -> PathBuf {
    state_root().join("scrollback")
}

/// Sandbox workspaces — repo-less scratch directories the user
/// creates via `x p`. Lives next to the worktree base under
/// `<home>/v2/sandboxes/<slug>/`. Survives across lazybox restarts
/// and is preserved on `x x` archive (the workspace record
/// goes; the directory stays for the user to clean up manually if
/// it contains real work).
pub fn sandboxes_root() -> PathBuf {
    state_root().join("sandboxes")
}

/// Directory for a single named sandbox.
pub fn sandbox_dir(name: &str) -> PathBuf {
    sandboxes_root().join(name)
}

/// User-editable config file. `<home>/config.yaml`. Lives at the
/// profile root (not the versioned subdir) so a schema bump
/// doesn't orphan the user's customizations.
pub fn config_yaml() -> PathBuf {
    home().join("config.yaml")
}

/// Root for per-session agent credential homes. `<home>/v2/agent-homes/`.
/// An agent kind that isolates its login (Codex → `CODEX_HOME`) gets a
/// private directory per workspace here, seeded once from the machine-wide
/// login, so a re-auth on one session never re-logs the others.
pub fn agent_homes_root() -> PathBuf {
    state_root().join("agent-homes")
}

/// Per-session credential home for `agent_id` in `session_key`:
/// `<home>/v2/agent-homes/<agent_id>/<session>`. The session component is
/// slugged to a filesystem-safe name because a session key can carry
/// `:`/`/`/`#` (`github:owner/repo#123`).
pub fn agent_home_dir(agent_id: &str, session_key: &str) -> PathBuf {
    agent_homes_root()
        .join(agent_id)
        .join(path_slug(session_key))
}

/// Collapse a string to a filesystem-safe path component: ASCII
/// alphanumerics and `.`/`_`/`-` survive, everything else becomes `-`.
/// Empty input yields `"_"` so the result is never an empty component.
fn path_slug(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() { "_".into() } else { slug }
}

/// Daemon runtime artifacts (socket, pid). `<home>/run/`. Honors
/// the older `LAZYBOX_RUNTIME_DIR` env var for back-compat — set both
/// to the same value if you want, or just unset it and let
/// `LAZYBOX_HOME` win.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LAZYBOX_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    home().join("run")
}

/// User-supplied hook scripts. `<home>/hooks/`.
pub fn hooks_dir() -> PathBuf {
    home().join("hooks")
}

/// Tmux socket label for this profile. Derives a stable name from
/// the home dir's last component so two profiles don't collide
/// on a shared tmux server.
///
/// - `~/.lazybox` → `"lazybox"` (unchanged from before this module
///   existed — preserves backward compatibility with running
///   sessions on the default profile).
/// - `~/.lazybox-dev` → `"lazybox-dev"`.
/// - Anything weird (path with no usable last component) → `"lazybox"`
///   fallback so callers don't have to handle Option.
pub fn tmux_socket_name() -> String {
    home()
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| {
            // `.lazybox` → `lazybox`, `.lazybox-dev` → `lazybox-dev`,
            // `lazybox-something` → `lazybox-something`.
            s.strip_prefix('.').unwrap_or(s).to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "lazybox".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Global lock serializing tests that mutate env vars. Without
    /// this, cargo-test's default parallel execution lets two tests
    /// step on each other (LAZYBOX_HOME set by one read by another)
    /// since env state is process-wide. Every test in this module
    /// must hold the lock for its entire body. Poisoning is fine —
    /// the test already failed, no recovery needed.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `EnvGuard` saves + restores an env var across a test so two
    /// tests in the same process don't see each other's setup.
    /// `std::env::set_var` is global state — without scoping AND
    /// the `ENV_LOCK` above, parallel tests in the same module
    /// would race.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // Safety: callers hold ENV_LOCK for the test's full
            // body, so no other test thread reads/writes env vars
            // concurrently. The `paths` functions themselves are
            // short pure reads.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn home_honors_lazybox_home_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::set("LAZYBOX_HOME", "/tmp/lazybox-test-xyz");
        assert_eq!(home(), PathBuf::from("/tmp/lazybox-test-xyz"));
    }

    #[test]
    fn home_empty_lazybox_home_falls_back_to_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Empty string must NOT be treated as "use /": a fish-shell
        // user with `set -gx LAZYBOX_HOME` and no value would silently
        // get lazybox writing to the filesystem root. The check above
        // filters empties to the default branch.
        let _g1 = EnvGuard::set("LAZYBOX_HOME", "");
        let _g2 = EnvGuard::set("HOME", "/tmp/test-home");
        assert_eq!(home(), PathBuf::from("/tmp/test-home/.lazybox"));
    }

    #[test]
    fn home_uses_home_env_when_lazybox_home_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g1 = EnvGuard::unset("LAZYBOX_HOME");
        let _g2 = EnvGuard::set("HOME", "/tmp/test-home");
        assert_eq!(home(), PathBuf::from("/tmp/test-home/.lazybox"));
    }

    #[test]
    fn state_db_lives_under_state_root() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::set("LAZYBOX_HOME", "/tmp/lazybox-x");
        assert_eq!(state_db(), PathBuf::from("/tmp/lazybox-x/v2/state.db"));
        assert_eq!(
            worktrees_root(),
            PathBuf::from("/tmp/lazybox-x/v2/worktrees")
        );
    }

    #[test]
    fn agent_home_dir_slugs_unsafe_session_keys_under_state_root() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::set("LAZYBOX_HOME", "/tmp/lazybox-x");
        // A raw key with `:`/`/`/`#` must not escape the agent-homes root
        // into nested or sibling directories.
        assert_eq!(
            agent_home_dir("codex", "github:owner/repo#123"),
            PathBuf::from("/tmp/lazybox-x/v2/agent-homes/codex/github-owner-repo-123")
        );
        // A slug-form key survives unchanged.
        assert_eq!(
            agent_home_dir("codex", "github-acme-widget-657"),
            PathBuf::from("/tmp/lazybox-x/v2/agent-homes/codex/github-acme-widget-657")
        );
    }

    #[test]
    fn config_lives_at_profile_root_not_state_root() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Schema versioning isn't supposed to invalidate the user's
        // config. Living at `<home>/config.yaml` instead of
        // `<home>/v2/config.yaml` means a v2→v3 schema bump leaves
        // their customizations alone.
        let _g = EnvGuard::set("LAZYBOX_HOME", "/tmp/lazybox-x");
        assert_eq!(config_yaml(), PathBuf::from("/tmp/lazybox-x/config.yaml"));
    }

    #[test]
    fn runtime_dir_honors_legacy_env_var() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // LAZYBOX_RUNTIME_DIR existed before LAZYBOX_HOME — keep it
        // working so users who already set it don't have to migrate.
        let _g1 = EnvGuard::set("LAZYBOX_HOME", "/tmp/lazybox-x");
        let _g2 = EnvGuard::set("LAZYBOX_RUNTIME_DIR", "/var/run/lazybox");
        assert_eq!(runtime_dir(), PathBuf::from("/var/run/lazybox"));
    }

    #[test]
    fn runtime_dir_falls_back_to_lazybox_home_when_legacy_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g1 = EnvGuard::set("LAZYBOX_HOME", "/tmp/lazybox-x");
        let _g2 = EnvGuard::unset("LAZYBOX_RUNTIME_DIR");
        assert_eq!(runtime_dir(), PathBuf::from("/tmp/lazybox-x/run"));
    }

    #[test]
    fn tmux_socket_strips_leading_dot_on_default_profile() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Backward compat: pre-LAZYBOX_HOME, sessions were stored
        // under `tmux -L lazybox`. Default profile must still resolve
        // to "lazybox" so an in-flight session survives the upgrade.
        let _g1 = EnvGuard::set("LAZYBOX_HOME", "/Users/test/.lazybox");
        assert_eq!(tmux_socket_name(), "lazybox");
    }

    #[test]
    fn tmux_socket_disambiguates_dev_profile() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g1 = EnvGuard::set("LAZYBOX_HOME", "/Users/test/.lazybox-dev");
        assert_eq!(tmux_socket_name(), "lazybox-dev");
    }

    #[test]
    fn tmux_socket_handles_non_dotted_profile() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Users sometimes point LAZYBOX_HOME at a non-dotfile path
        // (e.g. for testing). No leading dot to strip; pass the name
        // through.
        let _g1 = EnvGuard::set("LAZYBOX_HOME", "/tmp/sandbox/profile-a");
        assert_eq!(tmux_socket_name(), "profile-a");
    }

    #[test]
    fn tmux_socket_falls_back_when_path_has_no_name() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // LAZYBOX_HOME=/ is degenerate but mustn't crash. Fallback
        // to "lazybox" so callers don't have to handle None.
        let _g1 = EnvGuard::set("LAZYBOX_HOME", "/");
        assert_eq!(tmux_socket_name(), "lazybox");
    }
}
