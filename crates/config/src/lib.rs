//! # lazybox-config
//!
//! YAML-based configuration for lazybox. Loads from `~/.lazybox/config.yaml`
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
    /// View preferences lazybox writes back automatically: which
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
    /// Per-agent overrides keyed by agent id (`claude`, `codex`, …).
    /// Today this carries the model-tier menu (`agents.claude.models`)
    /// the `w`/`a` chords pick from; see [`AgentEntry`]. Agents without
    /// an entry fall back to [`lazybox_core::AgentModels::builtin`].
    #[serde(default)]
    pub agents: std::collections::BTreeMap<String, AgentEntry>,
    pub shell: ShellSection,
    pub hooks: HooksConfig,
    pub worktree: WorktreeConfig,
    pub terminal: TerminalSection,
    /// Auto-spawn-on-`@lazybox`-mention settings. See [`MentionConfig`].
    #[serde(default)]
    pub mention: MentionConfig,
    /// Auto-inject fix work on CI failure / merge conflict. See
    /// [`AutoFixConfig`]. Off by default.
    #[serde(default)]
    pub auto_fix: AutoFixConfig,
    /// Roots the `lazybox scan` command walks to discover git repos
    /// and worktrees you created outside lazybox, for import. Empty
    /// by default; `scan` also accepts roots as CLI args. See
    /// [`ScanConfig`].
    #[serde(default)]
    pub scan: ScanConfig,
}

/// `setup:` block — wizard-driven user config. Mirrors
/// `lazybox_core::PersistedSetup` shape but in YAML form.
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
    /// lazybox falls back to `"claude"`.
    #[serde(default)]
    pub default_agent: Option<String>,
    /// Set once the setup wizard has been completed. Distinguishes
    /// "finished setup with nothing ticked" (a valid choice) from a
    /// true first run, so an all-empty `setup:` block doesn't
    /// re-trigger the first-run wizard forever.
    #[serde(default)]
    pub wizard_completed: bool,
}

/// One entry under `editors:`. Args support `{path}` for the
/// worktree dir. See `lazybox_tui::editors::EditorTemplate`.
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
    /// Which mechanism carries the desktop banner. `auto` picks per
    /// environment (subprocess helpers locally, the terminal's OSC
    /// escape sequence over SSH); `osc` / `subprocess` force one path.
    pub notifier: NotifierBackend,
}

/// `attention.notifier` values — how a desktop banner is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NotifierBackend {
    /// Subprocess helpers when they can reach the user (local
    /// session), the terminal's OSC sequence over SSH.
    #[default]
    Auto,
    /// Always the terminal's OSC notification escape sequence
    /// (Ghostty / iTerm2 / Kitty / WezTerm).
    Osc,
    /// Always a spawned helper: `terminal-notifier` / `osascript`
    /// (macOS), `notify-send` (Linux).
    Subprocess,
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
            notifier: NotifierBackend::Auto,
        }
    }
}

/// Where a newly spawned terminal lands when its session already has
/// one open: as a side-by-side `Split` tile (the historical default)
/// or a stacked `Tabs` entry behind the tab strip. Only governs the
/// automatic layout of an ordinary shell/agent spawn — explicit
/// `]]|` / `]]-` splits are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NewTerminalLayout {
    #[default]
    Split,
    Tabs,
}

/// Lenient field deserializer for `ui.terminal_new_layout`: an
/// unrecognized value warns and falls back to the default rather than
/// failing the *entire* config load. A cosmetic per-terminal
/// preference must never be the reason lazybox can't start and read
/// the repos / Slack tokens alongside it. Absent keys never reach here
/// — `#[serde(default)]` handles them.
fn de_lenient_new_terminal_layout<'de, D>(de: D) -> Result<NewTerminalLayout, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(de)?;
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "tabs" => NewTerminalLayout::Tabs,
        "split" => NewTerminalLayout::Split,
        other => {
            tracing::warn!(
                "unknown ui.terminal_new_layout {other:?}; expected `split` or `tabs`, using `split`"
            );
            NewTerminalLayout::Split
        }
    })
}

/// How the Activity (right) pane opens for a workspace: the whole
/// description + activity feed (`Full`), a single-line summary of the
/// counts that matter (`Summary`), or folded away entirely (`Hidden`,
/// its space handed to the terminal). The `Shift-P` shortcut cycles
/// `Full → Summary → Hidden` per workspace; `ui.activity_pane_default`
/// sets where a workspace starts before the user touches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ActivityPaneMode {
    #[default]
    Full,
    Summary,
    Hidden,
}

/// Lenient field deserializer for `ui.activity_pane_default`: an
/// unrecognized value warns and falls back to `full` rather than
/// sinking the whole config load — same policy as
/// [`de_lenient_new_terminal_layout`].
fn de_lenient_activity_pane_mode<'de, D>(de: D) -> Result<ActivityPaneMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(de)?;
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "full" => ActivityPaneMode::Full,
        "summary" => ActivityPaneMode::Summary,
        "hidden" => ActivityPaneMode::Hidden,
        other => {
            tracing::warn!(
                "unknown ui.activity_pane_default {other:?}; expected `full`, `summary`, or `hidden`, using `full`"
            );
            ActivityPaneMode::Full
        }
    })
}

/// `ui:` block — user-facing view state lazybox writes back so UI
/// preferences survive restart.
///
/// `Default` is hand-written rather than derived because `show_tips`
/// defaults to `true` (an opt-out, not opt-in) — a derived `bool`
/// default would be `false` and silence tips for users with no
/// config file yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(with = "duration_human_opt", default)]
    pub auto_mark_delay: Option<Duration>,
    /// How long the first `q` stays armed waiting for the second
    /// tap. None = 800 ms.
    #[serde(with = "duration_human_opt", default)]
    pub quit_double_tap_window: Option<Duration>,
    /// Legacy location for the terminal command-menu character. New
    /// configs should use `terminal.escape_char`; when this is set it
    /// remains the compatibility override. None = use the terminal
    /// section (default `]`).
    pub terminal_escape_char: Option<char>,
    /// Shift-arrow nudges the focused splitter by this many
    /// percent. None = 3.
    pub split_step_percent: Option<i16>,
    /// Cap on the description / task-body section's expanded
    /// height (in rows) when `b` toggles it open. None = 8.
    pub task_body_max_rows: Option<u16>,
    /// `z` snooze duration. None = 4 hours.
    #[serde(with = "duration_human_opt", default)]
    pub short_snooze: Option<Duration>,
    /// `x z` long-snooze duration. None = ~1 year (365 days).
    #[serde(with = "duration_human_opt", default)]
    pub long_snooze: Option<Duration>,
    /// Where the lazybox client writes its log file. None =
    /// `/tmp/lazybox.log`. Future: respect `$XDG_STATE_HOME` /
    /// `~/.lazybox/logs/lazybox.log` as a smarter default.
    pub log_path: Option<std::path::PathBuf>,
    /// Catalog-driven keybinding overrides. Open schema: keys are
    /// snake_case `ActionKind` names (`"merge_pr"`, `"spawn_shell"`,
    /// `"refresh"`, …); values are key-spec strings (`"Shift-M"`,
    /// `"Ctrl-Enter"`). Lazybox consults this map before falling back
    /// to the catalog default (`ActionDef::default_keys`). Unset
    /// keys use the default. This is the only remap surface — the
    /// older typed `Keybindings` struct was retired in favor of the
    /// catalog-driven approach.
    #[serde(default)]
    pub action_keys: std::collections::BTreeMap<String, String>,
    /// Named keymap preset shipped in-tree (`"default"`, `"vim"`).
    /// Applied as a base layer of `action_keys`; the explicit
    /// `action_keys` map above still layers on top, so a user can pick
    /// `vim` and tweak individual binds. Unknown / unset → no preset.
    /// See `lazybox_tui_core::action::keymap_preset`.
    #[serde(default)]
    pub keymap_preset: Option<String>,
    /// Active UI theme, matched by exact name against the built-in
    /// palette registry (`"Lazybox Dark"`, `"Lazybox Light"`,
    /// `"High Contrast"`, …). Written back when the user picks a theme
    /// from the in-app picker so the choice survives restart. An unknown
    /// or unset name leaves the default (first registered) theme active.
    #[serde(default)]
    pub theme: Option<String>,
    /// Preferred web browser for opening task URLs (the `o` shortcut)
    /// and links right-clicked in the terminal grid. On macOS this is
    /// the application name handed to `open -a` (e.g. `"Google Chrome"`,
    /// `"Firefox"`); on Linux it's the executable run with the URL as
    /// its argument. None = the OS default browser.
    pub browser: Option<String>,
    /// Whether the user has already seen the in-app feature tour.
    /// Set `true` the first time the tour is dismissed or finished
    /// so it doesn't re-launch on every boot; re-invocable on demand
    /// via the tour shortcut regardless. Defaults to `false` so a
    /// brand-new install (or one upgraded into the tour feature)
    /// gets the walkthrough once.
    #[serde(default)]
    pub tour_seen: bool,
    /// Whether the progressive feature-discovery tips are enabled.
    /// Tips surface occasionally as a dim, auto-fading footer hint
    /// keyed off current state (agent waiting, failing CI, in a
    /// terminal). Opt-out: set `false` to silence them entirely.
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub show_tips: bool,
    /// Ids of tips already surfaced, so a given tip never repeats
    /// across sessions (mirrors `tour_seen`, but per-tip). Appended
    /// to the first time each tip is shown. Defaults to empty so a
    /// fresh install starts with every tip available.
    #[serde(default)]
    pub tips_seen: Vec<String>,
    /// How a second (or later) terminal in a session opens: as a
    /// side-by-side `split` (default) or a stacked `tabs`. Explicit
    /// `]]|` / `]]-` splits ignore this — it only governs the
    /// automatic layout of an ordinary shell/agent spawn. Toggle live
    /// with the `]]t` terminal-leader command, which persists back
    /// here.
    #[serde(default, deserialize_with = "de_lenient_new_terminal_layout")]
    pub terminal_new_layout: NewTerminalLayout,
    /// Where the Activity (right) pane starts for a workspace the user
    /// hasn't toggled yet: `full` (the whole feed, default), `summary`
    /// (a one-line count of new activity / failing CI), or `hidden`
    /// (folded away, its space given to the terminal). `Shift-P` cycles
    /// the three per workspace; this only sets the initial mode. A
    /// workspace with nothing to show still auto-hides regardless.
    #[serde(default, deserialize_with = "de_lenient_activity_pane_mode")]
    pub activity_pane_default: ActivityPaneMode,
    /// Keep the machine awake while any agent is actively working.
    /// When `true`, the daemon holds an OS sleep inhibitor
    /// (`caffeinate` on macOS, `systemd-inhibit` on Linux — a
    /// non-systemd Linux gets a logged warning and no inhibition)
    /// for exactly as long as ≥1 agent terminal is `Working`, and
    /// releases it the moment everything goes idle — the box never
    /// stays pinned awake just because lazybox is open. "Working"
    /// means actively computing: an agent parked at a permission
    /// prompt or resting after its turn lets the machine sleep
    /// normally. The daemon re-reads this flag on every agent
    /// transition, so editing it takes effect without a restart.
    /// Defaults to `false`: sleep behavior is unchanged unless
    /// opted in.
    #[serde(default)]
    pub keep_awake: bool,
}

fn default_true() -> bool {
    true
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            collapsed_repos: std::collections::BTreeSet::new(),
            keymap_preset: None,
            theme: None,
            sidebar_pct: None,
            right_top_pct: None,
            auto_mark_delay: None,
            quit_double_tap_window: None,
            terminal_escape_char: None,
            split_step_percent: None,
            task_body_max_rows: None,
            short_snooze: None,
            long_snooze: None,
            log_path: None,
            action_keys: std::collections::BTreeMap::new(),
            browser: None,
            tour_seen: false,
            show_tips: true,
            tips_seen: Vec::new(),
            terminal_new_layout: NewTerminalLayout::default(),
            activity_pane_default: ActivityPaneMode::default(),
            keep_awake: false,
        }
    }
}

/// Concrete UI settings with every `Option<T>` from `UiSection`
/// resolved to its default. Consumers (panes, model) read this
/// instead of duplicating defaults inline. Pure data — clone-cheap.
#[derive(Debug, Clone)]
pub struct UiDefaults {
    pub auto_mark_delay: Duration,
    pub quit_double_tap_window: Duration,
    pub terminal_escape_char: char,
    /// Window in which a second terminal escape-char press completes
    /// the doubled escape and opens the non-timed terminal leader.
    /// Sourced from `terminal.escape_window_ms`.
    pub escape_window: Duration,
    pub split_step_percent: i16,
    pub task_body_max_rows: u16,
    pub short_snooze: Duration,
    pub long_snooze: Duration,
    pub log_path: std::path::PathBuf,
    /// Preferred browser app/executable, or None for the OS default.
    /// See [`UiSection::browser`].
    pub browser: Option<String>,
    /// Dead-on-arrival grace window for exited agent terminals.
    /// Sourced from `terminal.agent_dead_on_arrival_ms`. See
    /// [`TerminalSection::agent_dead_on_arrival_ms`].
    pub agent_dead_on_arrival: Duration,
    /// Layout for an auto-spawned second-or-later terminal. See
    /// [`UiSection::terminal_new_layout`].
    pub terminal_new_layout: NewTerminalLayout,
    /// Initial Activity-pane mode for an un-toggled workspace. See
    /// [`UiSection::activity_pane_default`].
    pub activity_pane_default: ActivityPaneMode,
    /// Hold an OS sleep inhibitor while agents work. See
    /// [`UiSection::keep_awake`].
    pub keep_awake: bool,
}

impl Default for UiDefaults {
    fn default() -> Self {
        Self {
            auto_mark_delay: Duration::from_millis(1000),
            quit_double_tap_window: Duration::from_millis(800),
            terminal_escape_char: ']',
            escape_window: Duration::from_millis(600),
            split_step_percent: 3,
            task_body_max_rows: 8,
            short_snooze: Duration::from_secs(4 * 60 * 60),
            long_snooze: Duration::from_secs(365 * 24 * 60 * 60),
            log_path: std::path::PathBuf::from("/tmp/lazybox.log"),
            browser: None,
            agent_dead_on_arrival: Duration::from_millis(10_000),
            terminal_new_layout: NewTerminalLayout::default(),
            activity_pane_default: ActivityPaneMode::default(),
            keep_awake: false,
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
            // Sourced from the `terminal` section (see
            // `Config::resolved_ui`); the default stands until that
            // override is applied.
            escape_window: d.escape_window,
            split_step_percent: self.split_step_percent.unwrap_or(d.split_step_percent),
            task_body_max_rows: self.task_body_max_rows.unwrap_or(d.task_body_max_rows),
            short_snooze: self.short_snooze.unwrap_or(d.short_snooze),
            long_snooze: self.long_snooze.unwrap_or(d.long_snooze),
            log_path: self.log_path.clone().unwrap_or(d.log_path),
            browser: self.browser.clone(),
            // Sourced from the `terminal` section (see
            // `Config::resolved_ui`); the default stands until that
            // override is applied.
            agent_dead_on_arrival: d.agent_dead_on_arrival,
            terminal_new_layout: self.terminal_new_layout,
            activity_pane_default: self.activity_pane_default,
            keep_awake: self.keep_awake,
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
    /// `lazybox_git_ops::Mount` for semantics.
    pub mounts: Vec<MountSpec>,
    /// Executable scripts to materialize inside each worktree at
    /// `_lazybox/scripts/<name>`. Either inline `content` or a path
    /// `source` to symlink. See `lazybox_git_ops::Script`.
    pub scripts: Vec<ScriptSpec>,
    /// When a tracked PR flips to merged, automatically reap the
    /// worktrees backing its sessions — but only the ones the
    /// inspector deems safe (no locked / uncommitted / unpushed work,
    /// and no live terminal attached). Off by default: opt in once
    /// you trust lazybox not to pull a folder out from under you.
    pub auto_cleanup_merged: bool,
    /// Prefix for branches lazybox cuts itself (issues, Linear
    /// tickets, blank workspaces — anything without an upstream
    /// branch). Empty by default, so an issue spawn reads naturally in
    /// the target repo (`issue-42-fix-the-thing`). Set a value to
    /// namespace them — `lazybox` restores the old `lazybox/issue-42`
    /// behavior, `feature` yields `feature/issue-42` — and override
    /// per-repo via `repos.<owner/name>.branch_prefix`.
    pub branch_prefix: String,
}

/// `scan:` block — the canonical dev folders (`~/development`, `~/code`,
/// …) where you keep one clone per repo. Both the `lazybox scan` CLI and
/// the in-app `x i` "import checkout" flow walk these roots to discover
/// pre-existing git checkouts and map them to their GitHub repos; the
/// import turns a chosen one into a linked, no-worktree workspace. Roots
/// passed on the `scan` command line take precedence over `roots` here.
///
/// ```yaml
/// scan:
///   roots:
///     - ~/development
///     - ~/code
///   max_depth: 4
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ScanConfig {
    /// Directories to walk. A leading `~/` is expanded at scan time.
    pub roots: Vec<PathBuf>,
    /// How many directory levels below each root the walk descends
    /// before giving up — bounds a scan of a deep home directory.
    pub max_depth: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_depth: 4,
        }
    }
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
    /// lands at `_lazybox/scripts/<name>` chmod +x.
    pub scripts: Vec<ScriptSpec>,
    /// Override for `worktree.branch_prefix` on this repo's worktrees.
    /// `Some("at")` → `at/issue-42`; `Some("")` drops the prefix
    /// (`issue-42`); `None` (the default) falls back to the global
    /// prefix.
    #[serde(default)]
    pub branch_prefix: Option<String>,
}

/// Serializable form of `lazybox_git_ops::Mount`. Kept separate so
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

/// Serializable form of `lazybox_git_ops::Script`. Either `content`
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
    /// Filename inside `_lazybox/scripts/`. Must not contain `/`,
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

/// Periodic scripts lazybox runs to keep the user's environment tidy —
/// cargo sweep, worktree GC, whatever. Users drop shell scripts into
/// `hooks.dir/<bucket>/` and lazybox runs each bucket on its cadence.
/// Lazybox never knows or cares what the scripts do.
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
            // Profile-aware. `~/.lazybox-dev` keeps its own hooks
            // distinct from `~/.lazybox`, so a "send Slack on merge"
            // hook configured in stable doesn't spam from dev runs.
            dir: lazybox_core::paths::hooks_dir(),
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

/// Per-agent config block (`agents.<id>:`). Currently just the model
/// menu; more per-agent knobs can hang here without another top-level
/// key.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentEntry {
    /// The tier menu the `w S` / `a M` chords pick from, and which tier
    /// a bare spawn uses. Empty → fall back to the built-in preset for
    /// this agent id.
    pub models: lazybox_core::AgentModels,
    /// Let lazybox apply this agent's CLI updates automatically when
    /// its scheduled out-of-band check finds a newer version. Off by
    /// default: the check still runs and surfaces "update available",
    /// but installing waits for the manual "update agent CLIs" action.
    pub auto_update: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSection {
    #[serde(flatten)]
    pub config: lazybox_core::AgentConfig,
    /// Launch lazybox-spawned autonomous sessions (e.g. `@lazybox`-triggered
    /// work) with permission prompts disabled — `claude
    /// --dangerously-skip-permissions` — so the agent runs unattended
    /// instead of blocking on every tool-use approval. Only affects
    /// autonomous spawns; interactive sessions the user opens are
    /// governed by `skip_permissions` instead. Default on; flip to
    /// `false` to force prompts on every autonomous session.
    pub autonomous_skip_permissions: bool,
    /// Launch interactive sessions the user opens themselves (the `c`
    /// spawn) with permission prompts disabled too — `claude
    /// --dangerously-skip-permissions`. Off by default: the prompt is
    /// the human-in-the-loop guard for a session a human is driving.
    /// Flip on via Settings → "Skip permission prompts" to run your
    /// own Claude sessions unattended.
    pub skip_permissions: bool,
    /// Global LLM-gateway base URL. See [`AgentSection::gateway_url`].
    #[serde(default)]
    pub llm_gateway_url: Option<String>,
    /// Fail-safe watchdog window: seconds a `Working` agent terminal
    /// may sit with no meaningful screen change (spinner/status churn
    /// doesn't count) before the daemon classifies the screen and
    /// forces the turn out of `Working` (→ `InputNeeded` or `Done`).
    /// Unset → 15; `0` disables the watchdog.
    #[serde(default)]
    pub working_watchdog_secs: Option<u64>,
    /// Quiet-timer window: seconds of no PTY output a `Working` agent
    /// terminal must go silent before the daemon classifies the resting
    /// screen and settles the turn to `Done` (the generic, every-agent
    /// path to `Done` — no lifecycle hook required). Unset or `0` → 5.
    /// The timer can't be disabled (that would strand hookless agents on
    /// `Working`); raise it to be less eager to call a turn finished.
    #[serde(default)]
    pub quiet_classify_secs: Option<u64>,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            config: lazybox_core::AgentConfig::default(),
            autonomous_skip_permissions: true,
            skip_permissions: false,
            llm_gateway_url: None,
            working_watchdog_secs: None,
            quiet_classify_secs: None,
        }
    }
}

impl AgentSection {
    /// The configured global LLM-gateway base URL, normalized: surrounding
    /// whitespace trimmed and a blank string treated as unset. This is the
    /// single definition of "gateway configured" — the spawn-time
    /// injection, the settings label, and the editor pre-fill all read
    /// through it so they can't disagree.
    ///
    /// When set, lazybox points every spawned agent at this URL by
    /// injecting it into the base-URL env var the agent's CLI reads
    /// (`ANTHROPIC_BASE_URL` for Claude, `OPENAI_BASE_URL` for Codex /
    /// Cursor) — one global gateway, fronting whichever upstream the
    /// agent speaks. `None` → no injection, so the agent reaches the
    /// vendor directly.
    ///
    /// A per-repo `repos.<owner/name>.env` entry for the same base-URL var
    /// still wins, so a single repo can override or opt out of the gateway.
    ///
    /// ```yaml
    /// agent:
    ///   llm_gateway_url: "http://gateway.internal"
    /// ```
    ///
    /// Auth (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) is intentionally NOT
    /// managed here — set it via the process environment or
    /// `repos.<owner/name>.env` so secrets don't have to live in this file.
    pub fn gateway_url(&self) -> Option<&str> {
        self.llm_gateway_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
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

/// How the user opens lazybox's command menu from an embedded terminal.
/// The default is `]]` — two closing brackets typed in quick succession.
/// The menu then waits without a timeout (`q` exits to the inbox).
/// Configurable because:
///   - some users want a different char (`}`, `*`, etc.) that doesn't
///     collide with their normal typing,
///   - some users type `]` heavily (code, arrays, Markdown) and want
///     a less collision-prone prefix,
///   - hardware keyboards differ, accessibility differs.
///
/// The first char is buffered and flushed to the agent when the next key
/// is different, so typing a literal `]x` still reaches the program.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSection {
    /// Char that, when pressed twice within `escape_window_ms`, opens
    /// the terminal command menu.
    pub escape_char: char,
    /// Time window between consecutive `escape_char` presses for them
    /// to count toward the same run. After this window the run
    /// resets and the buffered chars flush to the agent.
    pub escape_window_ms: u64,
    /// Grace window (ms) after an agent terminal spawns during which a
    /// clean (`code 0`) exit that never engaged is treated as
    /// dead-on-arrival — kept open with a restart affordance instead
    /// of auto-closing. An agent that ran past this window, or that
    /// reached a working/input/done state, auto-closes on a clean exit;
    /// a non-zero code or death-by-signal always keeps the pane. See
    /// #367 (and #356/#357 for why the linger view exists).
    pub agent_dead_on_arrival_ms: u64,
}

impl Default for TerminalSection {
    fn default() -> Self {
        Self {
            escape_char: ']',
            escape_window_ms: 600,
            agent_dead_on_arrival_ms: 10_000,
        }
    }
}

impl Config {
    /// Resolve the UI defaults, folding in cross-section knobs the
    /// `ui` block alone can't see — currently the terminal escape /
    /// `]]` leader window, which lives under `terminal`.
    pub fn resolved_ui(&self) -> UiDefaults {
        let mut ui = self.ui.resolved();
        // `ui.terminal_escape_char` predates the dedicated terminal
        // section. Keep it as a compatibility override; new configs use
        // `terminal.escape_char` as the single obvious home for this key.
        if self.ui.terminal_escape_char.is_none() {
            ui.terminal_escape_char = self.terminal.escape_char;
        }
        ui.escape_window = Duration::from_millis(self.terminal.escape_window_ms);
        ui.agent_dead_on_arrival = Duration::from_millis(self.terminal.agent_dead_on_arrival_ms);
        ui
    }

    /// The model-tier menu for `agent_id`: the user's `agents.<id>.models`
    /// block when it defines any tiers, else the built-in preset for a
    /// known agent, else an empty menu (agent's own default model, no
    /// tier chords). A configured block with an empty `tiers` list is
    /// treated as "unset" so it transparently inherits the built-in —
    /// except its `default` and `priority`, which overlay the inherited
    /// menu (each replaced wholesale when set) so
    /// `agents.<id>.models.default: L` alone (the Settings default-model
    /// pick) or a bare `priority:` map works without copying the whole
    /// tier list into YAML. A `default` naming a Fable tier is never
    /// honored — it re-points to the first default-eligible tier.
    pub fn agent_models(&self, agent_id: &str) -> lazybox_core::AgentModels {
        let mut models = match self.agents.get(agent_id) {
            Some(entry) if !entry.models.tiers.is_empty() => entry.models.clone(),
            entry => {
                let mut models = lazybox_core::AgentModels::builtin(agent_id).unwrap_or_default();
                if let Some(entry) = entry {
                    if let Some(default) = entry.models.default.clone() {
                        models.default = Some(default);
                    }
                    if !entry.models.priority.is_unset() {
                        models.priority = entry.models.priority.clone();
                    }
                }
                models
            }
        };
        // A default pointing at a Fable tier is re-pointed to the first
        // eligible tier: creative-class models stay spawnable through an
        // explicit chord but are never what a bare spawn lands on.
        if models
            .default
            .as_deref()
            .and_then(|d| models.tier(d))
            .is_some_and(lazybox_core::ModelTier::excluded_from_default)
        {
            models.default = models
                .tiers
                .iter()
                .find(|t| !t.excluded_from_default())
                .map(|t| t.alias.clone());
        }
        models
    }

    /// Load from `~/.lazybox/config.yaml`, falling back to defaults.
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
        // The file can hold Slack tokens — tighten pre-existing
        // group/other-readable configs to owner-only. Best-effort:
        // a read-only mount must not make the config unloadable.
        if let Err(e) = restrict_to_owner(path) {
            tracing::warn!("couldn't tighten {} to 0600: {e}", path.display());
        }
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
        restrict_to_owner(path)?;
        Ok(())
    }

    /// Atomic write to `~/.lazybox/config.yaml`. tmp + rename so a
    /// crashing lazybox doesn't leave a half-written file. Used by
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
        // 0600 before the rename so the secret-bearing YAML is never
        // visible to other users, even transiently.
        restrict_to_owner(&tmp)?;
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
        lazybox_core::paths::config_yaml()
    }
}

/// Chmod `path` to 0600 if any group/other permission bit is set.
/// config.yaml can carry Slack tokens, so it's owner-only like an SSH
/// key. No-op on non-Unix and on files that are already tight.
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            tracing::info!(
                "tightened {} from {:o} to 600 (it can contain tokens)",
                path.display(),
                mode & 0o777,
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

// ─── Mention auto-spawn ────────────────────────────────────────────────────

/// Auto-spawn-on-`@lazybox`-mention settings. When an allowed user
/// writes `@lazybox` in an issue body or comment, lazybox reacts 👀 on
/// that surface and spawns the default agent with the implement-issue
/// prompt — same end-state as the user pressing `w` on the issue row.
///
/// Default: empty `allowed_logins` → lazybox falls back to "just the
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
    /// GitHub logins whose `@lazybox` mentions auto-spawn. Empty (the
    /// default) means "just the authenticated viewer" — the polling
    /// layer resolves that fallback at runtime so daemon restarts
    /// pick up token rotations without a config edit.
    pub allowed_logins: Vec<String>,
}

// ─── Auto-fix on CI failure / merge conflict ───────────────────────────────

/// Auto-inject fix work when a PR you authored fails CI or develops a
/// merge conflict — lazybox spawns an agent pointed at the failure and
/// posts a brief PR comment explaining why, no manual `@lazybox` needed.
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
///   opt_out_labels: [no-auto-fix, do-not-lazybox]
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
        // Mirror `lazybox_core::AutoFixSettings::default()` so the two
        // can't drift; the conversion below is the single bridge.
        let d = lazybox_core::AutoFixSettings::default();
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
    /// Convert the YAML form into the runtime [`lazybox_core::AutoFixSettings`]
    /// the polling layer consumes. Clamps `cooldown` up to
    /// `MIN_AUTO_FIX_COOLDOWN` so a too-small value can't spam.
    pub fn to_settings(&self) -> lazybox_core::AutoFixSettings {
        lazybox_core::AutoFixSettings {
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
    /// Whether to surface GitHub's needs-reply signal in the inbox.
    /// When disabled, fetched tasks are kept but their `needs_reply`
    /// flag is suppressed.
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

/// Slack integration. Bidirectional: lazybox mirrors PR/agent events to
/// per-workspace channels (outbound), and `@lazybox`-mentions /
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
    /// Anchor channel name (no `#` prefix). Lazybox posts bootstrap
    /// / error messages here, and routes everything when
    /// `per_workspace_channels: false`. Default `"lazybox"`.
    #[serde(default = "default_anchor_channel")]
    pub anchor_channel: String,
    /// Per-workspace channel name prefix. Default `""` → channel
    /// names are just the slugified workspace key (`github-acme-
    /// widget-186`). A value like `"pr-"` produces `pr-github-...`.
    #[serde(default)]
    pub channel_prefix: String,
    /// If true, lazybox auto-creates a channel for every workspace
    /// the inbox sees. If false, everything routes through the
    /// anchor channel with thread-per-workspace. Default true.
    #[serde(default = "default_per_workspace_channels")]
    pub per_workspace_channels: bool,
    /// Slack user ids (`U...`) allowed to drive agents from chat —
    /// anything written by anyone else in a routed channel is NOT
    /// forwarded to the agent PTY (read-only `status` queries stay
    /// open). Empty (the default) allows every user, but the
    /// dispatcher logs a one-time warning at startup recommending
    /// you set it: routed agents typically run with permission
    /// prompts disabled.
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl SlackConfig {
    pub fn normalized_anchor_channel(&self) -> String {
        normalize_slack_channel_name(&self.anchor_channel)
    }
}

pub fn normalize_slack_channel_name(raw: &str) -> String {
    raw.trim().trim_start_matches('#').to_string()
}

fn default_anchor_channel() -> String {
    "lazybox".into()
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

/// Optional-Duration variant for user-facing UI knobs. Accepts human
/// strings such as `"800ms"`, `"4h"`, and `"365d"`, while preserving
/// backward-compatible bare numbers as seconds.
mod duration_human_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => {
                s.serialize_str(&humantime_serde::re::humantime::format_duration(*d).to_string())
            }
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            String(String),
            Seconds(u64),
        }

        let value: Option<Wire> = Option::deserialize(d)?;
        match value {
            Some(Wire::Seconds(secs)) => Ok(Some(Duration::from_secs(secs))),
            Some(Wire::String(raw)) => {
                let trimmed = raw.trim();
                if trimmed.chars().all(|c| c.is_ascii_digit()) {
                    let secs: u64 = trimmed.parse().map_err(serde::de::Error::custom)?;
                    return Ok(Some(Duration::from_secs(secs)));
                }
                humantime_serde::re::humantime::parse_duration(trimmed)
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// config.yaml can hold Slack tokens: every write path must land
    /// owner-only, and loading a pre-existing loose file tightens it.
    #[cfg(unix)]
    #[test]
    fn config_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode_of = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        let dir = std::env::temp_dir().join(format!("lazybox-config-perms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("config.yaml");

        Config::write_default(&path).expect("write_default");
        assert_eq!(mode_of(&path), 0o600, "write_default must produce 0600");

        Config::default().save_to(&path).expect("save_to");
        assert_eq!(mode_of(&path), 0o600, "save_to must produce 0600");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen");
        Config::load_from(&path).expect("load");
        assert_eq!(mode_of(&path), 0o600, "load must tighten a loose file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ui.keep_awake` is opt-in: absent means off (sleep behavior
    /// unchanged), and both the raw section and the resolved defaults
    /// carry an explicit `true`.
    #[test]
    fn keep_awake_defaults_off_and_parses() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(!cfg.ui.keep_awake);
        assert!(!cfg.ui.resolved().keep_awake);

        let cfg: Config = serde_yaml::from_str("ui:\n  keep_awake: true\n").expect("parse");
        assert!(cfg.ui.keep_awake);
        assert!(cfg.ui.resolved().keep_awake);
    }

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

    /// `ui.terminal_new_layout` defaults to `split` (unchanged
    /// side-by-side behavior), accepts `tabs`, and round-trips.
    #[test]
    fn terminal_new_layout_defaults_to_split_and_parses_tabs() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert_eq!(
            cfg.ui.resolved().terminal_new_layout,
            NewTerminalLayout::Split,
            "absent → the historical split default"
        );

        let cfg: Config =
            serde_yaml::from_str("ui:\n  terminal_new_layout: tabs\n").expect("parse");
        assert_eq!(
            cfg.ui.resolved().terminal_new_layout,
            NewTerminalLayout::Tabs
        );

        let written = serde_yaml::to_string(&cfg).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert_eq!(
            reparsed.ui.resolved().terminal_new_layout,
            NewTerminalLayout::Tabs,
            "survives round-trip"
        );
    }

    /// A typo'd `ui.terminal_new_layout` must not sink the whole config
    /// load — it warns and falls back to `split`, so repos / tokens in
    /// the same file still load.
    #[test]
    fn terminal_new_layout_tolerates_a_bad_value() {
        let cfg: Config = serde_yaml::from_str("ui:\n  terminal_new_layout: splurt\n")
            .expect("a bad layout value must not fail the whole parse");
        assert_eq!(
            cfg.ui.resolved().terminal_new_layout,
            NewTerminalLayout::Split,
            "unknown value falls back to the default"
        );
    }

    /// `ui.activity_pane_default` defaults to `full`, accepts
    /// `summary` / `hidden`, and round-trips.
    #[test]
    fn activity_pane_default_parses_all_modes() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert_eq!(
            cfg.ui.resolved().activity_pane_default,
            ActivityPaneMode::Full,
            "absent → the full-feed default"
        );

        for (raw, want) in [
            ("summary", ActivityPaneMode::Summary),
            ("hidden", ActivityPaneMode::Hidden),
            ("full", ActivityPaneMode::Full),
        ] {
            let cfg: Config =
                serde_yaml::from_str(&format!("ui:\n  activity_pane_default: {raw}\n"))
                    .expect("parse");
            assert_eq!(cfg.ui.resolved().activity_pane_default, want);
            let written = serde_yaml::to_string(&cfg).expect("serialize");
            let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
            assert_eq!(
                reparsed.ui.resolved().activity_pane_default,
                want,
                "{raw} survives round-trip"
            );
        }
    }

    /// A typo'd `ui.activity_pane_default` warns and falls back to
    /// `full` rather than sinking the whole config load.
    #[test]
    fn activity_pane_default_tolerates_a_bad_value() {
        let cfg: Config = serde_yaml::from_str("ui:\n  activity_pane_default: slim\n")
            .expect("a bad mode value must not fail the whole parse");
        assert_eq!(
            cfg.ui.resolved().activity_pane_default,
            ActivityPaneMode::Full,
            "unknown value falls back to the default"
        );
    }

    /// Missing `repos:` section should land as an empty map, not
    /// an error — additive feature must not break older configs.
    #[test]
    fn missing_repos_section_defaults_to_empty() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(cfg.repos.is_empty());
    }

    /// The `scan:` section is additive: absent → empty roots + the
    /// depth default; a partial block keeps `max_depth`'s default; a
    /// full block round-trips.
    #[test]
    fn scan_section_defaults_and_round_trips() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(cfg.scan.roots.is_empty());
        assert_eq!(cfg.scan.max_depth, 4, "depth default when section absent");

        // Only `roots` set — `max_depth` must still fall back to 4.
        let cfg: Config = serde_yaml::from_str("scan:\n  roots:\n    - ~/code\n").expect("parse");
        assert_eq!(cfg.scan.roots, vec![PathBuf::from("~/code")]);
        assert_eq!(cfg.scan.max_depth, 4, "missing field keeps its default");

        let yaml = "scan:\n  roots:\n    - ~/code\n    - /work\n  max_depth: 2\n";
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.scan.max_depth, 2);
        assert_eq!(cfg.scan.roots.len(), 2);

        let written = serde_yaml::to_string(&cfg).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert_eq!(reparsed.scan, cfg.scan, "survives round-trip");
    }

    /// The in-app "add scan root" flow appends a root and writes the
    /// config to disk (`save_to`); on the next launch `load_from` must
    /// return it. Exercises the actual file path, not just serde.
    #[test]
    fn appended_scan_root_persists_to_disk_and_reloads() {
        let dir =
            std::env::temp_dir().join(format!("lazybox-scan-root-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir temp");
        let path = dir.join("config.yaml");
        let _ = std::fs::remove_file(&path);

        // Start from a fresh config, append a root, persist.
        let mut cfg = Config::default();
        cfg.scan.roots.push(PathBuf::from("~/development"));
        cfg.save_to(&path).expect("save config");

        // Reload as a subsequent launch would.
        let reloaded = Config::load_from(&path).expect("reload config");
        assert_eq!(reloaded.scan.roots, vec![PathBuf::from("~/development")]);

        std::fs::remove_dir_all(&dir).ok();
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

    /// Interactive skip-permissions is off by default and round-trips
    /// so a user can opt their own sessions into bypass mode.
    #[test]
    fn interactive_skip_permissions_defaults_off_and_round_trips() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(
            !cfg.agent.skip_permissions,
            "interactive sessions keep prompts on by default"
        );

        let mut opted_in = Config::default();
        opted_in.agent.skip_permissions = true;
        let written = serde_yaml::to_string(&opted_in).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert!(
            reparsed.agent.skip_permissions,
            "opting interactive sessions into skip mode survives a round-trip"
        );
    }

    /// The LLM gateway is unset on a fresh config (agents talk to the
    /// vendor directly) and the global URL survives a save/load
    /// round-trip — the persistence half of the `,` settings editor.
    #[test]
    fn llm_gateway_defaults_unset_and_round_trips() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(
            cfg.agent.llm_gateway_url.is_none(),
            "no gateway on a fresh config"
        );
        assert_eq!(cfg.agent.gateway_url(), None);

        let yaml = "agent:\n  llm_gateway_url: \"http://gateway.internal\"\n";
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.agent.gateway_url(), Some("http://gateway.internal"));

        let written = serde_yaml::to_string(&cfg).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert_eq!(reparsed.agent.llm_gateway_url, cfg.agent.llm_gateway_url);
    }

    /// `setup.default_agent` is unset on a fresh config (consumers fall
    /// back to `"claude"`) and a chosen id survives a save/load
    /// round-trip — the persistence half of the Settings → "Change
    /// default agent" picker.
    #[test]
    fn default_agent_defaults_unset_and_round_trips() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(
            cfg.setup.default_agent.is_none(),
            "no default_agent on a fresh config"
        );

        let yaml = "setup:\n  default_agent: codex\n";
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.setup.default_agent.as_deref(), Some("codex"));

        let written = serde_yaml::to_string(&cfg).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert_eq!(reparsed.setup.default_agent, cfg.setup.default_agent);
    }

    /// `gateway_url` owns the "blank == unset" invariant so every caller
    /// (spawn injection, settings label, editor pre-fill) reads one
    /// definition of "configured".
    #[test]
    fn gateway_url_normalizes_blank_and_whitespace() {
        let mut agent = AgentSection::default();
        assert_eq!(agent.gateway_url(), None);

        // A whitespace-padded URL is trimmed on read.
        agent.llm_gateway_url = Some("  http://gw  ".into());
        assert_eq!(agent.gateway_url(), Some("http://gw"));

        // A blank / whitespace-only string reads as unset.
        agent.llm_gateway_url = Some("   ".into());
        assert_eq!(agent.gateway_url(), None);

        agent.llm_gateway_url = Some(String::new());
        assert_eq!(agent.gateway_url(), None);
    }

    /// The feature-tour "seen" flag defaults to false (so a fresh
    /// install gets the walkthrough) and round-trips once set.
    #[test]
    fn tour_seen_defaults_false_and_round_trips() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(!cfg.ui.tour_seen, "tour should be unseen on a fresh config");

        let mut seen = Config::default();
        seen.ui.tour_seen = true;
        let written = serde_yaml::to_string(&seen).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert!(
            reparsed.ui.tour_seen,
            "marking the tour seen survives a save/load round-trip"
        );
    }

    /// The theme is unset on a fresh config (so the default palette
    /// wins) and a chosen theme name survives a save/load round-trip —
    /// the persistence half of the in-app theme picker.
    #[test]
    fn theme_defaults_none_and_round_trips() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(
            cfg.ui.theme.is_none(),
            "no theme override on a fresh config"
        );

        let mut picked = Config::default();
        picked.ui.theme = Some("Lazybox Light".to_string());
        let written = serde_yaml::to_string(&picked).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert_eq!(
            reparsed.ui.theme.as_deref(),
            Some("Lazybox Light"),
            "a picked theme survives a save/load round-trip"
        );
    }

    /// Tips are on by default (opt-out), and a fresh config starts
    /// with no tips marked seen. Both survive a save/load round-trip.
    #[test]
    fn tips_default_on_and_round_trip() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(cfg.ui.show_tips, "tips are opt-out — on by default");
        assert!(
            cfg.ui.tips_seen.is_empty(),
            "no tips marked seen on a fresh config"
        );

        let mut state = Config::default();
        state.ui.show_tips = false;
        state.ui.tips_seen.push("jump_to_asking".into());
        let written = serde_yaml::to_string(&state).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert!(!reparsed.ui.show_tips, "opting out survives a round-trip");
        assert_eq!(
            reparsed.ui.tips_seen,
            vec!["jump_to_asking".to_string()],
            "shown-tip ids survive a round-trip",
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

    /// `slack.allowed_users` defaults to empty (allow all, with a
    /// startup warning logged by the dispatcher) and round-trips so
    /// an operator can lock chat-driven agents to specific user ids.
    #[test]
    fn slack_allowed_users_defaults_empty_and_round_trips() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(cfg.slack.allowed_users.is_empty());

        let yaml = "slack:\n  allowed_users: [U111, U222]\n";
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.slack.allowed_users, vec!["U111", "U222"]);
        let written = serde_yaml::to_string(&cfg).expect("serialize");
        let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
        assert_eq!(reparsed.slack.allowed_users, vec!["U111", "U222"]);
    }

    #[test]
    fn slack_anchor_channel_normalizes_hash_prefix() {
        let cfg: Config =
            serde_yaml::from_str("slack:\n  anchor_channel: \" #lazybox \"\n").expect("parse");
        assert_eq!(cfg.slack.normalized_anchor_channel(), "lazybox");
        assert_eq!(
            normalize_slack_channel_name("lazybox-inbox"),
            "lazybox-inbox"
        );
    }

    #[test]
    fn auto_fix_defaults_to_off_when_section_absent() {
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert!(!cfg.auto_fix.enabled, "auto-fix must be opt-in");
        assert_eq!(cfg.auto_fix.max_attempts, 3);
        // Default matches the core settings so the two can't drift.
        assert_eq!(
            cfg.auto_fix.to_settings(),
            lazybox_core::AutoFixSettings::default()
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
  opt_out_labels: [skip-lazybox]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
        let s = cfg.auto_fix.to_settings();
        assert!(s.enabled);
        assert_eq!(s.max_attempts, 5);
        assert_eq!(s.cooldown, Duration::from_secs(30 * 60));
        assert_eq!(s.window, Duration::from_secs(12 * 3600));
        assert_eq!(s.opt_out_labels, vec!["skip-lazybox".to_string()]);
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
    fn notifier_backend_defaults_auto_and_parses_variants() {
        // Absent → auto, so existing configs keep working banners.
        let cfg: Config = serde_yaml::from_str("{}").expect("parse");
        assert_eq!(cfg.attention.notifier, NotifierBackend::Auto);

        for (yaml, want) in [
            ("attention:\n  notifier: auto\n", NotifierBackend::Auto),
            ("attention:\n  notifier: osc\n", NotifierBackend::Osc),
            (
                "attention:\n  notifier: subprocess\n",
                NotifierBackend::Subprocess,
            ),
        ] {
            let cfg: Config = serde_yaml::from_str(yaml).expect("parse");
            assert_eq!(cfg.attention.notifier, want, "yaml: {yaml}");
            let written = serde_yaml::to_string(&cfg).expect("serialize");
            let reparsed: Config = serde_yaml::from_str(&written).expect("reparse");
            assert_eq!(reparsed.attention.notifier, want, "round-trip: {yaml}");
        }
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
            log_path: Some(std::path::PathBuf::from("/var/log/lazybox.log")),
            ..Default::default()
        };
        let r = ui.resolved();
        assert_eq!(r.terminal_escape_char, '}');
        assert_eq!(r.task_body_max_rows, 20);
        assert_eq!(r.split_step_percent, 7);
        assert_eq!(r.short_snooze, Duration::from_secs(15 * 60));
        assert_eq!(r.long_snooze, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(r.log_path, std::path::PathBuf::from("/var/log/lazybox.log"));
    }

    #[test]
    fn resolved_ui_uses_terminal_escape_char_with_legacy_ui_precedence() {
        let cfg: Config =
            serde_yaml::from_str("terminal:\n  escape_char: '}'\n  escape_window_ms: 250\n")
                .expect("terminal config parses");
        let resolved = cfg.resolved_ui();
        assert_eq!(resolved.terminal_escape_char, '}');
        assert_eq!(resolved.escape_window, Duration::from_millis(250));

        let legacy: Config = serde_yaml::from_str(
            "terminal:\n  escape_char: '}'\nui:\n  terminal_escape_char: '*'\n",
        )
        .expect("legacy ui override parses");
        assert_eq!(legacy.resolved_ui().terminal_escape_char, '*');
    }

    #[test]
    fn ui_durations_accept_human_and_legacy_second_values() {
        let yaml = r#"
ui:
  auto_mark_delay: 800ms
  quit_double_tap_window: "2s"
  short_snooze: 4h
  long_snooze: 365d
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse human durations");
        let ui = cfg.ui.resolved();
        assert_eq!(ui.auto_mark_delay, Duration::from_millis(800));
        assert_eq!(ui.quit_double_tap_window, Duration::from_secs(2));
        assert_eq!(ui.short_snooze, Duration::from_secs(4 * 60 * 60));
        assert_eq!(ui.long_snooze, Duration::from_secs(365 * 24 * 60 * 60));

        let legacy: Config = serde_yaml::from_str(
            r#"
ui:
  short_snooze: 14400
  long_snooze: "31536000"
"#,
        )
        .expect("parse legacy seconds");
        let ui = legacy.ui.resolved();
        assert_eq!(ui.short_snooze, Duration::from_secs(14_400));
        assert_eq!(ui.long_snooze, Duration::from_secs(31_536_000));
    }

    #[test]
    fn agent_models_falls_back_to_builtin_then_empty() {
        let cfg = Config::default();
        // Claude ships a built-in tier menu; unknown agents get none.
        assert!(!cfg.agent_models("claude").tiers.is_empty());
        assert!(cfg.agent_models("codex").tiers.is_empty());
    }

    #[test]
    fn agent_models_reads_configured_tiers() {
        let yaml = r#"
agents:
  codex:
    models:
      default: M
      tiers:
        - alias: M
          label: "GPT-5"
          args: ["-m", "gpt-5"]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse agent models");
        let m = cfg.agent_models("codex");
        assert_eq!(m.default.as_deref(), Some("M"));
        assert_eq!(
            m.resolve_args(None),
            vec!["-m".to_string(), "gpt-5".to_string()]
        );
        // An empty configured block still inherits the built-in preset.
        assert!(!cfg.agent_models("claude").tiers.is_empty());
    }

    #[test]
    fn agent_models_default_alone_overlays_the_builtin_menu() {
        let yaml = r#"
agents:
  claude:
    models:
      default: L
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse default-only models");
        let m = cfg.agent_models("claude");
        assert!(!m.tiers.is_empty(), "builtin tiers are inherited");
        assert_eq!(m.default.as_deref(), Some("L"));
        assert_eq!(
            m.resolve_args(None),
            vec!["--model".to_string(), "claude-opus-4-8".to_string()],
            "a bare spawn resolves the persisted default tier"
        );
    }

    #[test]
    fn agent_models_builtin_claude_defaults_to_a_pinned_coding_tier() {
        let cfg = Config::default();
        let m = cfg.agent_models("claude");
        assert_eq!(m.default.as_deref(), Some("L"));
        assert_eq!(
            m.resolve_args(None),
            vec!["--model".to_string(), "claude-opus-4-8".to_string()],
            "a bare spawn always pins an explicit coding model"
        );
    }

    #[test]
    fn agent_models_fable_default_is_repointed_to_an_eligible_tier() {
        let yaml = r#"
agents:
  claude:
    models:
      default: F
      tiers:
        - alias: F
          label: "Fable"
          args: ["--model", "claude-fable-5"]
        - alias: L
          label: "Opus"
          args: ["--model", "claude-opus-4-8"]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse fable-default models");
        let m = cfg.agent_models("claude");
        assert_eq!(
            m.default.as_deref(),
            Some("L"),
            "Fable is never the default"
        );
        assert_eq!(
            m.resolve_args(None),
            vec!["--model".to_string(), "claude-opus-4-8".to_string()]
        );
        // The Fable tier itself stays selectable via an explicit chord.
        assert_eq!(
            m.resolve_args(Some("F")),
            vec!["--model".to_string(), "claude-fable-5".to_string()]
        );
    }

    #[test]
    fn agent_models_priority_alone_overlays_the_builtin_menu() {
        let yaml = r#"
agents:
  claude:
    models:
      priority:
        high: M
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse priority-only models");
        let m = cfg.agent_models("claude");
        assert!(!m.tiers.is_empty(), "builtin tiers are inherited");
        assert_eq!(
            m.alias_for_priority(lazybox_core::PriorityTier::High),
            Some("M")
        );
        assert_eq!(
            m.alias_for_priority(lazybox_core::PriorityTier::Medium),
            None,
            "the user's map replaces the builtin wholesale, like tiers"
        );
    }

    #[test]
    fn agent_auto_update_defaults_off_and_parses() {
        let yaml = r#"
agents:
  claude:
    auto_update: true
  codex:
    models:
      default: M
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("parse auto_update");
        assert!(cfg.agents["claude"].auto_update);
        assert!(!cfg.agents["codex"].auto_update);
        assert!(!AgentEntry::default().auto_update);
    }
}
