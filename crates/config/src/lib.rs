//! # pilot-config
//!
//! YAML-based configuration for pilot. Loads from `~/.pilot/config.yaml`
//! with sensible defaults if the file is missing.

mod snippets;

pub use snippets::{Snippet, SnippetOrigin, Snippets, SnippetsError};

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] serde_yaml::Error),
}

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    /// Wizard output: which providers + agents are enabled, the
    /// per-provider role/type filters, the selected orgs/repos.
    /// Populated by the first-run wizard and the in-session
    /// Settings palette (`,`); editable by hand.
    #[serde(default)]
    pub setup: SetupSection,
    /// Custom + override editor entries. Merged with builtins
    /// (Zed/VS Code/Cursor/…) at startup. `id` matches builtins
    /// to override; new ids extend.
    #[serde(default)]
    pub editors: Vec<EditorEntry>,
    /// What counts as "needs attention" for the per-repo counter
    /// in the sidebar header. Toggle individual signals off here.
    #[serde(default)]
    pub attention: AttentionConfig,
    /// Single-char keybindings → agent ids. Defaults to
    /// `c → claude, x → codex, u → cursor`. User can remap or add
    /// custom CLIs (e.g. `a → aider`).
    #[serde(default)]
    pub agent_shortcuts: std::collections::BTreeMap<char, String>,
    /// View preferences pilot writes back automatically: which
    /// repos are collapsed in the sidebar, last splitter widths.
    /// Edit by hand if you want to lock a layout.
    #[serde(default)]
    pub ui: UiSection,
    /// Per-repo overrides — env vars to inject into spawned PTYs
    /// (Claude/codex/shell) and additional mount points to symlink
    /// into the worktree on checkout. Keyed by `owner/name`. See
    /// `RepoConfig`.
    #[serde(default)]
    pub repos: std::collections::BTreeMap<String, RepoConfig>,
    pub providers: ProvidersConfig,
    pub display: DisplayConfig,
    pub slack: SlackConfig,
    pub agent: AgentSection,
    pub shell: ShellSection,
    pub hooks: HooksConfig,
    pub worktree: WorktreeConfig,
    pub terminal: TerminalSection,
    /// Auto-spawn-on-`@pilot`-mention settings. See [`MentionConfig`].
    #[serde(default)]
    pub mention: MentionConfig,
    /// Auto-inject fix work on CI failure / merge conflict. See
    /// [`AutoFixConfig`]. Off by default.
    #[serde(default)]
    pub auto_fix: AutoFixConfig,
}

/// `setup:` block — wizard-driven user config. Mirrors
/// `pilot_core::PersistedSetup` shape but in YAML form.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SetupSection {
    /// Provider ids (`github`, `linear`) currently enabled.
    pub providers: std::collections::BTreeSet<String>,
    /// Agent ids (`claude`, `codex`, …) currently enabled.
    pub agents: std::collections::BTreeSet<String>,
    /// Per-provider role/type filter keys. e.g.
    /// `github: [pr.author, pr.reviewer, issue.author]`.
    pub filters: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Per-provider scope ids (orgs / repos).
    pub scopes: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Agent id the `f` (fix) shortcut spawns. Empty / unset →
    /// pilot falls back to `"claude"`.
    #[serde(default)]
    pub default_agent: Option<String>,
}

/// One entry under `editors:`. Args support `{path}` for the
/// worktree dir. See `pilot_tui::editors::EditorTemplate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorEntry {
    pub id: String,
    #[serde(default)]
    pub display: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
}

/// `attention:` block — controls which signals contribute to the
/// "needs attention" badge on a repo header. All default to true.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AttentionConfig {
    pub unread: bool,
    pub ci_failing: bool,
    pub review_pending: bool,
    pub agent_asking: bool,
    pub mentioned: bool,
    /// Whether an agent crossing into `InputNeeded` fires an OS-level
    /// desktop notification (`terminal-notifier` / `osascript` on
    /// macOS, `notify-send` on Linux). Independent of `agent_asking`,
    /// which only gates the in-app attention badge — set this to
    /// `false` to keep the badge but silence the desktop banner.
    /// Default on.
    pub desktop_notify: bool,
}

impl Default for AttentionConfig {
    fn default() -> Self {
        Self {
            unread: true,
            ci_failing: true,
            review_pending: true,
            agent_asking: true,
            mentioned: true,
            desktop_notify: true,
        }
    }
}

/// `ui:` block — user-facing view state pilot writes back so UI
/// preferences survive restart.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UiSection {
    /// Repo names whose workspace rows should start collapsed.
    pub collapsed_repos: std::collections::BTreeSet<String>,
    /// Sidebar column width as a percentage of total. None = use
    /// the default (40%).
    pub sidebar_pct: Option<u16>,
    /// Right-top (activity) row height as a percentage of the
    /// right column. None = use the default (25%).
    pub right_top_pct: Option<u16>,
    /// How long the cursor must sit on an unread activity row
    /// before the daemon auto-marks it read. None = 1 second (the
    /// historical default). Yazi-ish: long enough to scan past,
    /// short enough that the user feels in control.
    #[serde(with = "duration_secs_opt", default)]
    pub auto_mark_delay: Option<Duration>,
    /// How long the first `q` stays armed waiting for the second
    /// tap. None = 800 ms.
    #[serde(with = "duration_secs_opt", default)]
    pub quit_double_tap_window: Option<Duration>,
    /// Two consecutive presses of this character return focus from
    /// the terminal pane back to the sidebar (tmux-style prefix).
    /// None = `]`.
    pub terminal_escape_char: Option<char>,
    /// Shift-arrow nudges the focused splitter by this many
    /// percent. None = 3.
    pub split_step_percent: Option<i16>,
    /// Cap on the description / task-body section's expanded
    /// height (in rows) when `b` toggles it open. None = 8.
    pub task_body_max_rows: Option<u16>,
    /// `z` snooze duration. None = 4 hours.
    #[serde(with = "duration_secs_opt", default)]
    pub short_snooze: Option<Duration>,
    /// `Shift-Z` long-snooze duration. None = ~1 year (365 days).
    #[serde(with = "duration_secs_opt", default)]
    pub long_snooze: Option<Duration>,
    /// Where the pilot client writes its log file. None =
    /// `/tmp/pilot.log`. Future: respect `$XDG_STATE_HOME` /
    /// `~/.pilot/logs/pilot.log` as a smarter default.
    pub log_path: Option<std::path::PathBuf>,
    /// Catalog-driven keybinding overrides. Open schema: keys are
    /// snake_case `ActionKind` names (`"merge_pr"`, `"spawn_shell"`,
    /// `"refresh"`, …); values are key-spec strings (`"Shift-M"`,
    /// `"Ctrl-Enter"`). Pilot consults this map before falling back
    /// to the catalog default (`ActionDef::default_keys`). Unset
    /// keys use the default. This is the only remap surface — the
    /// older typed `Keybindings` struct was retired in favor of the
    /// catalog-driven approach.
    #[serde(default)]
    pub action_keys: std::collections::BTreeMap<String, String>,
}

/// Concrete UI settings with every `Option<T>` from `UiSection`
/// resolved to its default. Consumers (panes, model) read this
/// instead of duplicating defaults inline. Pure data — clone-cheap.
#[derive(Debug, Clone)]
pub struct UiDefaults {
    pub auto_mark_delay: Duration,
    pub quit_double_tap_window: Duration,
    pub terminal_escape_char: char,
    pub split_step_percent: i16,
    pub task_body_max_rows: u16,
    pub short_snooze: Duration,
    pub long_snooze: Duration,
    pub log_path: std::path::PathBuf,
}

impl Default for UiDefaults {
    fn default() -> Self {
        Self {
            auto_mark_delay: Duration::from_millis(1000),
            quit_double_tap_window: Duration::from_millis(800),
            terminal_escape_char: ']',
            split_step_percent: 3,
            task_body_max_rows: 8,
            short_snooze: Duration::from_secs(4 * 60 * 60),
            long_snooze: Duration::from_secs(365 * 24 * 60 * 60),
            log_path: std::path::PathBuf::from("/tmp/pilot.log"),
        }
    }
}

impl UiSection {
    /// Resolve every optional knob to a concrete value, filling
    /// missing entries with `UiDefaults::default()`. Call once at
    /// startup; share the result with whichever component reads
    /// each field.
    pub fn resolved(&self) -> UiDefaults {
        let d = UiDefaults::default();
        UiDefaults {
            auto_mark_delay: self.auto_mark_delay.unwrap_or(d.auto_mark_delay),
            quit_double_tap_window: self
                .quit_double_tap_window
                .unwrap_or(d.quit_double_tap_window),
            terminal_escape_char: self.terminal_escape_char.unwrap_or(d.terminal_escape_char),
            split_step_percent: self.split_step_percent.unwrap_or(d.split_step_percent),
            task_body_max_rows: self.task_body_max_rows.unwrap_or(d.task_body_max_rows),
            short_snooze: self.short_snooze.unwrap_or(d.short_snooze),
            long_snooze: self.long_snooze.unwrap_or(d.long_snooze),
            log_path: self.log_path.clone().unwrap_or(d.log_path),
        }
    }
}

/// Worktree-layout configuration — mount points, mostly. The daemon
/// calls `WorktreeManager::apply_mounts` after every checkout with
/// the list assembled from this section so users see consistent
/// layouts across every session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorktreeConfig {
    /// Paths to symlink into / above each worktree. See
    /// `pilot_git_ops::Mount` for semantics.
    pub mounts: Vec<MountSpec>,
    /// Executable scripts to materialize inside each worktree at
    /// `_pilot/scripts/<name>`. Either inline `content` or a path
    /// `source` to symlink. See `pilot_git_ops::Script`.
    pub scripts: Vec<ScriptSpec>,
    /// When a tracked PR flips to merged, automatically reap the
    /// worktrees backing its sessions — but only the ones the
    /// inspector deems safe (no locked / uncommitted / unpushed work,
    /// and no live terminal attached). Off by default: opt in once
    /// you trust pilot not to pull a folder out from under you.
    pub auto_cleanup_merged: bool,
}

/// Per-repo overrides keyed by `owner/name` (the same string GitHub's
/// API returns as `repo.full_name`). Anything here applies only to
/// worktrees / spawns whose primary task's `repo` matches the key.
///
/// ```yaml
/// repos:
///   acme/widget:
///     env:
///       DATABASE_URL: postgres://localhost/dev
///       OPENAI_API_KEY: sk-...
///     mounts:
///       - source: ~/shared/tensor-data
///         link_at: _imports/data
///     scripts:
///       - name: cleanup
///         source: ~/dev/scripts/rust-cleanup.sh
///       - name: setup
///         content: |
///           #!/usr/bin/env bash
///           cargo fetch
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RepoConfig {
    /// Environment variables injected into every shell / agent PTY
    /// spawned inside this repo's worktrees. Layered ON TOP of the
    /// daemon's process env and the global `agent.env` config — the
    /// per-repo value wins on key collision.
    pub env: std::collections::BTreeMap<String, String>,
    /// Extra mount points to symlink into the worktree on checkout.
    /// Stacked on top of global `worktree.mounts`. Useful for
    /// sharing common code (`_imports/...`) without committing it.
    pub mounts: Vec<MountSpec>,
    /// Executable scripts to materialize inside this repo's
    /// worktrees. Stacked on top of `worktree.scripts`. Each entry
    /// lands at `_pilot/scripts/<name>` chmod +x.
    pub scripts: Vec<ScriptSpec>,
}

/// Serializable form of `pilot_git_ops::Mount`. Kept separate so
/// config doesn't depend on git-ops; the daemon converts on the way
/// in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    /// Absolute host path (or `~/...`; expanded on load).
    pub source: PathBuf,
    /// Path relative to either the worktree root or one level up.
    pub link_at: PathBuf,
    /// `"inside"` (default) or `"above"`.
    #[serde(default)]
    pub placement: PlacementSpec,
}

/// Serializable form of `pilot_git_ops::Script`. Either `content`
/// (inline body, written to the file) or `source` (path to symlink)
/// must be set — never both, never neither. The daemon validates
/// this on the way in.
///
/// ```yaml
/// scripts:
///   - name: cleanup
///     source: ~/dev/scripts/rust-cleanup.sh
///   - name: setup
///     content: |
///       #!/usr/bin/env bash
///       cargo fetch
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptSpec {
    /// Filename inside `_pilot/scripts/`. Must not contain `/`,
    /// `\`, `..`, or start with `.` (rejected at apply time).
    pub name: String,
    /// Inline body. Written verbatim into the file. Mutually
    /// exclusive with `source`. A `#!/usr/bin/env bash` shebang
    /// is prepended if missing so the file is directly executable.
    #[serde(default)]
    pub content: Option<String>,
    /// Path to an existing script on disk. Symlinked into the
    /// worktree (so edits to the source file flow through without
    /// re-running `apply_scripts`). Mutually exclusive with
    /// `content`. Leading `~/` is expanded by the daemon.
    #[serde(default)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlacementSpec {
    #[default]
    Inside,
    Above,
}

/// Periodic scripts pilot runs to keep the user's environment tidy —
/// cargo sweep, worktree GC, whatever. Users drop shell scripts into
/// `hooks.dir/<bucket>/` and pilot runs each bucket on its cadence.
/// Pilot never knows or cares what the scripts do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    pub enabled: bool,
    /// Directory with `daily/`, `hourly/`, `on_idle/` subfolders.
    pub dir: PathBuf,
    /// Per-bucket schedule. Bucket is the subfolder name under `dir`.
    pub schedule: HooksSchedule,
    /// Max runtime for a single script. Killed with SIGTERM on overrun.
    #[serde(with = "humantime_serde")]
    pub script_timeout: Duration,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Profile-aware. `~/.pilot-dev` keeps its own hooks
            // distinct from `~/.pilot`, so a "send Slack on merge"
            // hook configured in stable doesn't spam from dev runs.
            dir: pilot_core::paths::hooks_dir(),
            schedule: HooksSchedule::default(),
            script_timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksSchedule {
    #[serde(with = "humantime_serde")]
    pub daily: Duration,
    #[serde(with = "humantime_serde")]
    pub hourly: Duration,
    /// Runs when the inbox has been quiet (no key / no new activity) for
    /// this long. Good for "don't run cargo-sweep while the user is
    /// actively coding" kinds of tasks.
    #[serde(with = "humantime_serde")]
    pub on_idle: Duration,
}

impl Default for HooksSchedule {
    fn default() -> Self {
        Self {
            daily: Duration::from_secs(24 * 3600),
            hourly: Duration::from_secs(3600),
            on_idle: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSection {
    #[serde(flatten)]
    pub config: pilot_core::AgentConfig,
    /// Launch pilot-spawned autonomous sessions (e.g. `@pilot`-triggered
    /// work) with permission prompts disabled — `claude
    /// --dangerously-skip-permissions` — so the agent runs unattended
    /// instead of blocking on every tool-use approval. Only affects
    /// autonomous spawns; interactive sessions the user opens always
    /// keep prompts on. Default on; flip to `false` to force prompts on
    /// every session.
    pub autonomous_skip_permissions: bool,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            config: pilot_core::AgentConfig::default(),
            autonomous_skip_permissions: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellSection {
    pub command: String,
}

impl Default for ShellSection {
    fn default() -> Self {
        Self {
            command: "bash".into(),
        }
    }
}

/// How the user gets out of an embedded terminal when they want to go
/// back to the inbox. The default is `]]` — two closing brackets typed
/// in quick succession. Configurable because:
///   - some users want a different char (`}`, `*`, etc.) that doesn't
///     collide with their normal typing,
///   - some shells / agents print `]` heavily (BBcode, escape
///     sequences shown literally) and want a longer run,
///   - hardware keyboards differ, accessibility differs.
///
/// The first `(count - 1)` chars are buffered and only flushed to the
/// agent on a non-matching key, so an actual `]]` in code never ends
/// up in the agent's input mid-typed if the user was escaping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSection {
    /// Char that, when repeated `escape_count` times within
    /// `escape_window_ms`, exits the terminal back to the sidebar.
    pub escape_char: char,
    /// How many of `escape_char` in a row trigger the escape.
    /// Must be ≥ 2; `1` would steal the key entirely.
    pub escape_count: u8,
    /// Time window between consecutive `escape_char` presses for them
    /// to count toward the same run. After this window the run
    /// resets and the buffered chars flush to the agent.
    pub escape_window_ms: u64,
}

impl Default for TerminalSection {
    fn default() -> Self {
        Self {
            escape_char: ']',
            escape_count: 2,
            escape_window_ms: 600,
        }
    }
}

impl Config {
    /// Load from `~/.pilot/config.yaml`, falling back to defaults.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::default_path();
        if path.exists() {
            Self::load_from(&path)
        } else {
            tracing::info!("No config file at {}, using defaults", path.display());
            Ok(Self::default())
        }
    }

    /// Load from a specific path.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&contents)?;
        tracing::info!("Loaded config from {}", path.display());
        Ok(config)
    }

    /// Write a default config file (for first-run).
    pub fn write_default(path: &Path) -> Result<(), ConfigError> {
        let config = Self::default();
        let yaml = serde_yaml::to_string(&config)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, yaml)?;
        Ok(())
    }

    /// Atomic write to `~/.pilot/config.yaml`. tmp + rename so a
    /// crashing pilot doesn't leave a half-written file. Used by
    /// the in-process write-back paths (sidebar collapse,
    /// `,` settings palette, splitter resize).
    pub fn save(&self) -> Result<(), ConfigError> {
        Self::save_to(self, &Self::default_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let yaml = serde_yaml::to_string(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, yaml)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Read-modify-write. Loads the YAML, lets `f` mutate it,
    /// writes back. Most callers (sidebar collapse, splitter
    /// resize) only touch one field — this avoids the boilerplate
    /// of the load/save dance.
    ///
    /// A process-global mutex serialises the load-mutate-write
    /// sequence. Without it, two concurrent callers (e.g. dragging
    /// a splitter while toggling a repo's collapse state) would
    /// each load, each apply their mutation to independent copies,
    /// then race to write — one mutation silently lost.
    pub fn save_with<F>(f: F) -> Result<(), ConfigError>
    where
        F: FnOnce(&mut Self),
    {
        use std::sync::Mutex;
        static SAVE_LOCK: Mutex<()> = Mutex::new(());
        let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = Self::load()?;
        f(&mut cfg);
        cfg.save()
    }

    pub fn default_path() -> PathBuf {
        pilot_core::paths::config_yaml()
    }
}

// ─── Mention auto-spawn ────────────────────────────────────────────────────

/// Auto-spawn-on-`@pilot`-mention settings. When an allowed user
/// writes `@pilot` in an issue body or comment, pilot reacts 👀 on
/// that surface and spawns the default agent with the implement-issue
/// prompt — same end-state as the user pressing `w` on the issue row.
///
/// Default: empty `allowed_logins` → pilot falls back to "just the
/// authenticated user's own issues + comments." Add teammates' logins
/// to extend the allowlist:
///
/// ```yaml
/// mention:
///   allowed_logins: [alice, bob]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MentionConfig {
    /// GitHub logins whose `@pilot` mentions auto-spawn. Empty (the
    /// default) means "just the authenticated viewer" — the polling
    /// layer resolves that fallback at runtime so daemon restarts
    /// pick up token rotations without a config edit.
    pub allowed_logins: Vec<String>,
}

// ─── Auto-fix on CI failure / merge conflict ───────────────────────────────

/// Auto-inject fix work when a PR you authored fails CI or develops a
/// merge conflict — pilot spawns an agent pointed at the failure and
/// posts a brief PR comment explaining why, no manual `@pilot` needed.
///
/// **Opt-in.** `enabled` defaults to `false`: this pushes commits to
/// your PRs with no human nudge, so you turn it on deliberately. Only
/// PRs you authored are ever touched, never a third party's.
///
/// ```yaml
/// auto_fix:
///   enabled: true
///   max_attempts: 3       # per PR, per failure-kind, per window
///   cooldown: 1h          # min gap between attempts on the same PR
///   window: 24h           # rolling budget window
///   opt_out_labels: [no-auto-fix, do-not-pilot]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoFixConfig {
    /// Master switch. Default `false` (opt-in).
    pub enabled: bool,
    /// Labels that opt a PR out entirely (case-insensitive).
    pub opt_out_labels: Vec<String>,
    /// Max attempts per PR, per failure-kind, per `window`.
    pub max_attempts: u32,
    /// Minimum gap between two attempts on the same PR+kind.
    #[serde(with = "humantime_serde")]
    pub cooldown: Duration,
    /// Rolling window the `max_attempts` budget is measured over.
    #[serde(with = "humantime_serde")]
    pub window: Duration,
}

impl Default for AutoFixConfig {
    fn default() -> Self {
        // Mirror `pilot_core::AutoFixSettings::default()` so the two
        // can't drift; the conversion below is the single bridge.
        let d = pilot_core::AutoFixSettings::default();
        Self {
            enabled: d.enabled,
            opt_out_labels: d.opt_out_labels,
            max_attempts: d.max_attempts,
            cooldown: d.cooldown,
            window: d.window,
        }
    }
}

/// Hard floor on the auto-fix cooldown. A cooldown shorter than the
/// poll interval (default 60s) would let every sweep re-fire — spawning
/// `max_attempts` agents and posting `max_attempts` comments within a
/// couple of minutes. We clamp to this floor so an accidental small (or
/// zero) `cooldown:` value can't turn into a comment storm; the
/// max-attempts cap bounds it further.
const MIN_AUTO_FIX_COOLDOWN: Duration = Duration::from_secs(60);

impl AutoFixConfig {
    /// Convert the YAML form into the runtime [`pilot_core::AutoFixSettings`]
    /// the polling layer consumes. Clamps `cooldown` up to
    /// `MIN_AUTO_FIX_COOLDOWN` so a too-small value can't spam.
    pub fn to_settings(&self) -> pilot_core::AutoFixSettings {
        pilot_core::AutoFixSettings {
            enabled: self.enabled,
            opt_out_labels: self.opt_out_labels.clone(),
            max_attempts: self.max_attempts,
            cooldown: self.cooldown.max(MIN_AUTO_FIX_COOLDOWN),
            window: self.window,
        }
    }
}

// ─── Provider configs ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct ProvidersConfig {
    pub github: GithubConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubConfig {
    /// Poll interval in seconds.
    #[serde(with = "duration_secs")]
    pub poll_interval: Duration,
    /// Org/repo filters. Only PRs matching these appear in the inbox.
    /// Empty = show everything.
    pub filters: Vec<Filter>,
    /// Whether to fetch comment authors for needs-reply detection.
    pub detect_needs_reply: bool,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            // 60s = 60 polls/hour. With the trimmed GraphQL query
            // (~125 sub-objects/PR, see `SEARCH_QUERY` doc-comment),
            // this fits comfortably inside GitHub's 5000-points/hour
            // PAT budget even for a 200-PR inbox. The previous 30s
            // default doubled the cost for no real-time benefit —
            // PR/issue state doesn't change that fast.
            poll_interval: Duration::from_secs(60),
            filters: vec![],
            detect_needs_reply: true,
        }
    }
}

/// A filter for narrowing which tasks to show.
///
/// YAML format:
/// ```yaml
/// filters:
///   - org: acme
///   - repo: owner/name
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    /// Filter to a GitHub organization (only PRs involving you).
    #[serde(default)]
    pub org: Option<String>,
    /// Filter to a specific repo (only PRs involving you).
    #[serde(default)]
    pub repo: Option<String>,
    /// Watch ALL open PRs in this repo (regardless of involvement).
    #[serde(default)]
    pub watch: Option<String>,
}

impl Filter {
    /// Convert to a GitHub search query qualifier for the "involves" query.
    pub fn to_search_qualifier(&self) -> Option<String> {
        if let Some(org) = &self.org {
            Some(format!("org:{org}"))
        } else {
            self.repo.as_ref().map(|repo| format!("repo:{repo}"))
        }
    }

    /// If this is a "watch" filter, return the repo to watch.
    pub fn watch_repo(&self) -> Option<&str> {
        self.watch.as_deref()
    }
}

// ─── Display config ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    pub sort_by: SortMode,
    pub show_archived: bool,
    /// Only show sessions with activity within this many days.
    /// 0 = show all. Default: 7.
    pub activity_days: u32,
    /// Hide PRs you've already approved (you've done your part).
    pub hide_approved_by_me: bool,
    /// Treat assignees as reviewers (some teams use assignees for review tracking).
    pub assignee_is_reviewer: bool,
    /// Surface merged + closed PRs in the main Inbox alongside open
    /// work. Default `false` keeps them in the Inactive mailbox so
    /// the inbox stays focused on actionable items. Toggle on when
    /// you want to track "everything I touched recently" without
    /// switching mailboxes.
    pub show_inactive_in_inbox: bool,
    /// Fall back to plain ASCII letters (`p` / `i` / `l`) for the
    /// row type indicator instead of the default unicode glyphs
    /// (`⇄` PR / `○` issue / `◆` linear). Enable for fonts that
    /// don't render the unicode glyphs reliably as a single cell.
    pub ascii_glyphs: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            sort_by: SortMode::Priority,
            show_archived: false,
            activity_days: 7,
            hide_approved_by_me: true,
            assignee_is_reviewer: false,
            show_inactive_in_inbox: false,
            ascii_glyphs: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortMode {
    Priority,
    Updated,
}

// ─── Slack config ─────────────────────────────────────────────────────────

/// Slack integration. Bidirectional: pilot mirrors PR/agent events to
/// per-workspace channels (outbound), and `@pilot`-mentions /
/// channel messages route back to claude sessions (inbound, via
/// Socket Mode WebSocket). See `docs/slack-setup.md` for the
/// Slack-side app setup.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SlackConfig {
    /// Bot User OAuth Token (`xoxb-...`). Required for HTTP API.
    /// Loaded here OR via `$SLACK_BOT_TOKEN` env (env wins on
    /// conflict so credentials don't have to live in YAML).
    pub bot_token: Option<String>,
    /// App-Level Token (`xapp-...`) for Socket Mode WebSocket.
    /// Required for inbound. Same env-wins-over-YAML rule as
    /// `bot_token`; env var is `SLACK_APP_TOKEN`.
    pub app_token: Option<String>,
    /// Anchor channel name (no `#` prefix). Pilot posts bootstrap
    /// / error messages here, and routes everything when
    /// `per_workspace_channels: false`. Default `"pilot"`.
    #[serde(default = "default_anchor_channel")]
    pub anchor_channel: String,
    /// Per-workspace channel name prefix. Default `""` → channel
    /// names are just the slugified workspace key (`github-acme-
    /// widget-186`). A value like `"pr-"` produces `pr-github-...`.
    #[serde(default)]
    pub channel_prefix: String,
    /// If true, pilot auto-creates a channel for every workspace
    /// the inbox sees. If false, everything routes through the
    /// anchor channel with thread-per-workspace. Default true.
    #[serde(default = "default_per_workspace_channels")]
    pub per_workspace_channels: bool,
}

fn default_anchor_channel() -> String {
    "pilot".into()
}

fn default_per_workspace_channels() -> bool {
    true
}

// ─── Serde helper for Duration as seconds ──────────────────────────────────

mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}s", d.as_secs()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        let s = s.trim_end_matches('s');
        let secs: u64 = s.parse().map_err(serde::de::Error::custom)?;
        Ok(Duration::from_secs(secs))
    }
}

/// Optional-Duration variant: accepts a `"30s"`-style YAML value or
/// the absence of the key. Used by `UiSection` for timing knobs
/// whose default lives in the consumer (so the helper here is just
/// "wire the string ↔ Duration").
mod duration_secs_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_str(&format!("{}s", d.as_secs())),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let s: Option<String> = Option::deserialize(d)?;
        match s {
            Some(s) => {
                let trimmed = s.trim_end_matches('s');
                let secs: u64 = trimmed.parse().map_err(serde::de::Error::custom)?;
                Ok(Some(Duration::from_secs(secs)))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `repos.<owner/name>.{env,mounts}` should round-trip cleanly
    /// through serde so a hand-edited YAML survives a save_with
    /// load → mutate → write cycle.
    #[test]
    fn repos_section_round_trips() {
        let yaml = r#"
repos:
  acme/widget:
    env:
      DATABASE_URL: postgres://localhost/dev
      OPENAI_API_KEY: sk-test
    mounts:
      - source: ~/shared/data
        link_at: _imports/data
        placement: inside
      - source: /abs/path/to/scripts
        link_at: scripts
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
        let entry = cfg
            .repos
            .get("acme/widget")
            .expect("repos.acme/widget block present");
        assert_eq!(
            entry.env.get("DATABASE_URL").map(String::as_str),
            Some("postgres://localhost/dev")
        );
        assert_eq!(
            entry.env.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-test")
        );
        assert_eq!(entry.mounts.len(), 2);
        assert_eq!(entry.mounts[0].placement, PlacementSpec::Inside);
        // Second mount omits `placement` — should default to Inside.
        assert_eq!(entry.mounts[1].placement, PlacementSpec::Inside);
        // Now serialize back + parse + compare.
        let written = serde_yaml::to_string(&cfg).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        let reentry = reparsed.repos.get("acme/widget").unwrap();
        assert_eq!(reentry.env, entry.env);
        assert_eq!(reentry.mounts.len(), entry.mounts.len());
    }

    /// Missing `repos:` section should land as an empty map, not
    /// an error — additive feature must not break older configs.
    #[test]
    fn missing_repos_section_defaults_to_empty() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(cfg.repos.is_empty());
    }

    /// Autonomous sessions launch in no-permission mode by default,
    /// and the toggle round-trips so a paranoid user can flip it off.
    #[test]
    fn autonomous_skip_permissions_defaults_on_and_round_trips() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(
            cfg.agent.autonomous_skip_permissions,
            "default is on for autonomous sessions"
        );

        let mut paranoid = Config::default();
        paranoid.agent.autonomous_skip_permissions = false;
        let written = serde_yaml::to_string(&paranoid).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert!(
            !reparsed.agent.autonomous_skip_permissions,
            "flipping the toggle off survives a save/load round-trip"
        );
    }

    /// `placement: above` should parse + serialize correctly.
    #[test]
    fn placement_above_round_trips() {
        let yaml = r#"
repos:
  o/r:
    mounts:
      - source: /shared
        link_at: side
        placement: above
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let m = &cfg.repos["o/r"].mounts[0];
        assert_eq!(m.placement, PlacementSpec::Above);
        let written = serde_yaml::to_string(&cfg).unwrap();
        assert!(written.contains("placement: above"));
    }

    #[test]
    fn auto_fix_defaults_to_off_when_section_absent() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(!cfg.auto_fix.enabled, "auto-fix must be opt-in");
        assert_eq!(cfg.auto_fix.max_attempts, 3);
        // Default matches the core settings so the two can't drift.
        assert_eq!(
            cfg.auto_fix.to_settings(),
            pilot_core::AutoFixSettings::default()
        );
    }

    #[test]
    fn auto_fix_round_trips() {
        let yaml = r#"
auto_fix:
  enabled: true
  max_attempts: 5
  cooldown: 30m
  window: 12h
  opt_out_labels: [skip-pilot]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
        let s = cfg.auto_fix.to_settings();
        assert!(s.enabled);
        assert_eq!(s.max_attempts, 5);
        assert_eq!(s.cooldown, Duration::from_secs(30 * 60));
        assert_eq!(s.window, Duration::from_secs(12 * 3600));
        assert_eq!(s.opt_out_labels, vec!["skip-pilot".to_string()]);
        // Survives a serialize → reparse cycle.
        let written = serde_yaml::to_string(&cfg).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert_eq!(reparsed.auto_fix.to_settings(), s);
    }

    #[test]
    fn desktop_notify_defaults_on_and_round_trips() {
        // Absent section → on, so notifications work out of the box.
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(cfg.attention.desktop_notify);

        // A user who silences the banner keeps that choice across a
        // save/load cycle, independent of the `agent_asking` badge.
        let yaml = "attention:\n  desktop_notify: false\n";
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
        assert!(!cfg.attention.desktop_notify);
        assert!(cfg.attention.agent_asking, "badge gate is independent");
        let written = serde_yaml::to_string(&cfg).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert!(!reparsed.attention.desktop_notify);
    }

    #[test]
    fn ui_resolved_falls_back_to_defaults_when_section_is_empty() {
        let ui = UiSection::default();
        let r = ui.resolved();
        let d = UiDefaults::default();
        assert_eq!(r.auto_mark_delay, d.auto_mark_delay);
        assert_eq!(r.quit_double_tap_window, d.quit_double_tap_window);
        assert_eq!(r.terminal_escape_char, d.terminal_escape_char);
        assert_eq!(r.split_step_percent, d.split_step_percent);
        assert_eq!(r.task_body_max_rows, d.task_body_max_rows);
        assert_eq!(r.short_snooze, d.short_snooze);
        assert_eq!(r.long_snooze, d.long_snooze);
        assert_eq!(r.log_path, d.log_path);
    }

    #[test]
    fn ui_resolved_honors_explicit_values() {
        // Pin the contract: a user setting in YAML wins over the
        // default. The whole point of moving these from `const` to
        // `Option<T>` is that this assertion can hold.
        let ui = UiSection {
            terminal_escape_char: Some('}'),
            task_body_max_rows: Some(20),
            split_step_percent: Some(7),
            short_snooze: Some(Duration::from_secs(15 * 60)),
            long_snooze: Some(Duration::from_secs(7 * 24 * 3600)),
            log_path: Some(std::path::PathBuf::from("/var/log/pilot.log")),
            ..Default::default()
        };
        let r = ui.resolved();
        assert_eq!(r.terminal_escape_char, '}');
        assert_eq!(r.task_body_max_rows, 20);
        assert_eq!(r.split_step_percent, 7);
        assert_eq!(r.short_snooze, Duration::from_secs(15 * 60));
        assert_eq!(r.long_snooze, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(r.log_path, std::path::PathBuf::from("/var/log/pilot.log"));
    }
}
