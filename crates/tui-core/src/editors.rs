//! Editor detection + launch helpers.
//!
//! Lazybox exposes an `o` (sidebar) shortcut that opens the focused
//! workspace's worktree in the user's editor of choice. We probe
//! PATH at startup for known GUI editors. On macOS, app bundles in
//! `/Applications` and `~/Applications` provide a fallback when an
//! editor has no command-line launcher.
//!
//! ## Customization
//!
//! `~/.lazybox/config.yaml` can add custom editors via an `editors:`
//! list. Entries with the same `id` as a builtin override the
//! builtin's command/args; new ids extend the list. Args support
//! `{path}` (the worktree directory) which is substituted at launch.
//!
//! ```yaml
//! editors:
//!   - id: fleet
//!     display: "JetBrains Fleet"
//!     command: fleet
//!     args: ["{path}"]
//!   - id: my-editor
//!     display: "My custom editor"
//!     command: /opt/edit/bin/edit
//!     args: ["--workspace", "{path}"]
//! ```
//!
//! ## Spawning
//!
//! Editor CLIs are detached from lazybox so closing lazybox doesn't
//! take the editor with it. macOS app bundles are handed to Launch
//! Services without detaching the short-lived `open` process.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorLauncher {
    Command { program: String, args: Vec<String> },
    MacosApplication { bundle_path: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileLocationStyle {
    PathSuffix,
    VsCodeGoto,
    JetBrains,
}

fn location_style_for_id(id: &str) -> FileLocationStyle {
    match id {
        "code" | "cursor" | "windsurf" | "codium" | "vscodium" => FileLocationStyle::VsCodeGoto,
        "idea" | "pycharm" | "webstorm" | "goland" | "clion" | "rubymine" | "phpstorm"
        | "rider" | "datagrip" | "android-studio" => FileLocationStyle::JetBrains,
        _ => FileLocationStyle::PathSuffix,
    }
}

/// One launchable editor. Builtins are static; user additions are
/// loaded from config and merged at startup.
#[derive(Debug, Clone)]
pub struct EditorTemplate {
    /// Stable id used to match user overrides and persist a default.
    pub id: String,
    /// User-visible name shown in the picker.
    pub display: String,
    launcher: EditorLauncher,
    macos_app_name: Option<String>,
    location_style: FileLocationStyle,
}

/// Built-in editor list. **GUI editors only** — vim/neovim/helix
/// belong in a terminal pane, which lazybox already provides. Users
/// who want them can add via `editors:` in `~/.lazybox/config.yaml`.
fn builtin_editors() -> Vec<EditorTemplate> {
    let template = |id: &str, display: &str, command: &str, macos_app_name: &str| EditorTemplate {
        id: id.to_string(),
        display: display.to_string(),
        launcher: EditorLauncher::Command {
            program: command.to_string(),
            args: vec!["{path}".to_string()],
        },
        macos_app_name: Some(macos_app_name.to_string()),
        location_style: location_style_for_id(id),
    };
    vec![
        template("zed", "Zed", "zed", "Zed"),
        template("code", "VS Code", "code", "Visual Studio Code"),
        template("cursor", "Cursor", "cursor", "Cursor"),
        template("windsurf", "Windsurf", "windsurf", "Windsurf"),
        template("subl", "Sublime Text", "subl", "Sublime Text"),
        template("fleet", "JetBrains Fleet", "fleet", "Fleet"),
        template("idea", "IntelliJ IDEA", "idea", "IntelliJ IDEA"),
        template("pycharm", "PyCharm", "pycharm", "PyCharm"),
        template("webstorm", "WebStorm", "webstorm", "WebStorm"),
        template("goland", "GoLand", "goland", "GoLand"),
        template("clion", "CLion", "clion", "CLion"),
        template("rubymine", "RubyMine", "rubymine", "RubyMine"),
        template("phpstorm", "PhpStorm", "phpstorm", "PhpStorm"),
        template("rider", "Rider", "rider", "Rider"),
        template("datagrip", "DataGrip", "datagrip", "DataGrip"),
        template(
            "android-studio",
            "Android Studio",
            "android-studio",
            "Android Studio",
        ),
        template("conductor", "Conductor", "conductor", "Conductor"),
        template("nova", "Nova", "nova", "Nova"),
        template("mate", "TextMate", "mate", "TextMate"),
        template("bbedit", "BBEdit", "bbedit", "BBEdit"),
        template("emacs", "Emacs", "emacs", "Emacs"),
        // Gram: a Zed fork with AI, telemetry, and collaboration
        // stripped out (codeberg.org/GramEditor/gram). Its CLI binary
        // is `gram`; obscure enough that the name reads like a typo.
        template("gram", "Gram", "gram", "Gram"),
    ]
}

/// User entry in `~/.lazybox/config.yaml::editors`. Mirrors the
/// builtin shape but is owned (deserialized).
#[derive(Debug, Clone, Deserialize)]
pub struct UserEditorEntry {
    pub id: String,
    /// Falls back to `id` titlecased if not set.
    #[serde(default)]
    pub display: Option<String>,
    pub command: String,
    /// Defaults to `["{path}"]` when omitted, matching the common
    /// "open this folder" case.
    #[serde(default)]
    pub args: Option<Vec<String>>,
}

impl From<UserEditorEntry> for EditorTemplate {
    fn from(u: UserEditorEntry) -> Self {
        let location_style = location_style_for_id(&u.id);
        let display = u.display.unwrap_or_else(|| {
            // Crude titlecase: "my-editor" → "My-editor". Users can
            // always set `display:` explicitly for nicer labels.
            let mut s = u.id.clone();
            if let Some(first) = s.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            s
        });
        Self {
            id: u.id,
            display,
            launcher: EditorLauncher::Command {
                program: u.command,
                args: u.args.unwrap_or_else(|| vec!["{path}".to_string()]),
            },
            macos_app_name: None,
            location_style,
        }
    }
}

/// Merge builtins with user-defined entries. User entries with a
/// matching `id` replace the builtin; new ids append. Preserves
/// builtin order for the picker so common choices stay near the
/// top.
pub fn merge(builtins: Vec<EditorTemplate>, user: Vec<EditorTemplate>) -> Vec<EditorTemplate> {
    let mut out = builtins;
    for u in user {
        if let Some(slot) = out.iter_mut().find(|e| e.id == u.id) {
            *slot = u;
        } else {
            out.push(u);
        }
    }
    out
}

fn macos_application_dirs() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut dirs = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Applications"));
        }
        dirs
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

#[derive(Debug)]
struct MacosAppCandidate {
    bundle_path: PathBuf,
    version: Vec<u64>,
    modified: Option<std::time::SystemTime>,
}

fn numeric_components(value: &str) -> Vec<u64> {
    value
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|component| !component.is_empty())
        .filter_map(|component| component.parse().ok())
        .collect()
}

fn find_macos_app_bundle(app_name: &str, application_dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in application_dirs {
        let exact = dir.join(format!("{app_name}.app"));
        if exact.is_dir() {
            return Some(exact);
        }
    }

    let mut matches = Vec::new();
    for dir in application_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let bundle_path = entry.path();
            if !bundle_path.is_dir()
                || bundle_path.extension().and_then(|ext| ext.to_str()) != Some("app")
            {
                continue;
            }
            let Some(name) = bundle_path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(suffix) = name
                .strip_prefix(app_name)
                .filter(|suffix| suffix.starts_with(' '))
            else {
                continue;
            };
            let version = numeric_components(suffix);
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok();
            matches.push(MacosAppCandidate {
                bundle_path,
                version,
                modified,
            });
        }
    }
    matches
        .into_iter()
        .max_by(|left, right| {
            left.version
                .cmp(&right.version)
                .then_with(|| left.modified.cmp(&right.modified))
                .then_with(|| left.bundle_path.cmp(&right.bundle_path))
        })
        .map(|candidate| candidate.bundle_path)
}

fn detect_available_with(
    templates: &[EditorTemplate],
    application_dirs: &[PathBuf],
    mut command_available: impl FnMut(&str) -> bool,
) -> Vec<EditorTemplate> {
    templates
        .iter()
        .filter_map(|template| {
            match &template.launcher {
                EditorLauncher::Command { program, .. } if command_available(program) => {
                    return Some(template.clone());
                }
                EditorLauncher::MacosApplication { .. } => return Some(template.clone()),
                EditorLauncher::Command { .. } => {}
            }

            let bundle_path =
                find_macos_app_bundle(template.macos_app_name.as_deref()?, application_dirs)?;
            let mut detected = template.clone();
            detected.launcher = EditorLauncher::MacosApplication { bundle_path };
            Some(detected)
        })
        .collect()
}

/// Filter the provided template list to installed editors. Commands
/// on PATH are preferred. On macOS, a matching app bundle falls back
/// to `open -a` when the command is unavailable.
pub fn detect_available(templates: &[EditorTemplate]) -> Vec<EditorTemplate> {
    detect_available_with(templates, &macos_application_dirs(), |command| {
        which::which(command).is_ok()
    })
}

/// Top-level convenience: load user config, merge with builtins,
/// detect what's actually installed. Returns the launchable list.
pub fn discover_at_startup(user: Vec<UserEditorEntry>) -> Vec<EditorTemplate> {
    let user_templates: Vec<EditorTemplate> = user.into_iter().map(Into::into).collect();
    let merged = merge(builtin_editors(), user_templates);
    detect_available(&merged)
}

/// Build the launcher argv for opening `url`, honoring an optional
/// `browser` override. Split out from [`open_url`] so the
/// platform-specific command shape is unit-testable without spawning.
/// Returns `(program, args)`.
///
/// With no override the OS launcher targets the default browser
/// (`open` / `xdg-open` / `cmd /c start`). When `browser` is set it
/// becomes `open -a <browser>` on macOS, or the named executable run
/// directly on Linux.
fn browser_argv(url: &str, browser: Option<&str>) -> (String, Vec<String>) {
    if cfg!(target_os = "macos") {
        let mut args = Vec::new();
        if let Some(app) = browser {
            args.push("-a".to_string());
            args.push(app.to_string());
        }
        args.push(url.to_string());
        ("open".to_string(), args)
    } else if cfg!(target_os = "windows") {
        // `start`'s first quoted token is the window title, so pass an
        // empty "" before the (browser and) URL.
        let mut args = vec!["/c".to_string(), "start".to_string(), String::new()];
        if let Some(app) = browser {
            args.push(app.to_string());
        }
        args.push(url.to_string());
        ("cmd".to_string(), args)
    } else {
        match browser {
            Some(app) => (app.to_string(), vec![url.to_string()]),
            None => ("xdg-open".to_string(), vec![url.to_string()]),
        }
    }
}

/// Open `url` in a web browser. By default the URL is handed to the
/// host launcher (`open` on macOS, `xdg-open` on Linux, `cmd /c start`
/// on Windows), which targets the user's default browser. When
/// `browser` is set it overrides that choice — `open -a <browser>` on
/// macOS, the named executable on Linux. Used by the `o` shortcut on
/// workspaces and right-clicked links in the terminal grid.
///
/// Unlike the editor launchers, this does **not** detach via
/// `setsid()`. The launcher is usually a short-lived hand-off process —
/// the real browser is re-parented by the OS (Launch Services on
/// macOS, the desktop elsewhere) — and a fresh session severs the
/// macOS GUI bootstrap the launcher needs to reach Launch Services.
///
/// It also does **not** wait for the child. This runs on the single
/// UI thread, and with a configured `browser` binary on Linux the
/// child IS the browser — the old `.output()` blocked the entire TUI
/// until the browser exited. Instead the launcher is spawned like the
/// editors (`spawn` + drop the handle) and a background thread reaps
/// it, logging a non-zero exit to `/tmp/lazybox.log`. `Err` therefore
/// only means "could not even start `program`"; a launcher that
/// starts and then fails is log-only.
pub fn open_url(url: &str, browser: Option<&str>) -> std::io::Result<()> {
    // Minimal allow-list: must be http(s) so a malformed task URL
    // can't be coerced into running an arbitrary shell command.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to open non-http(s) URL: {url}"),
        ));
    }
    let (program, args) = browser_argv(url, browser);
    let child = std::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let url = url.to_string();
    // Reap off-thread so the child never zombifies and a failed
    // launcher still leaves a breadcrumb — without ever blocking the
    // caller. Detached thread: it exits with the child (or with the
    // process, whichever comes first).
    spawn_url_reaper(child, program, url)
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("open_url reaper thread spawn failed: {e}"));
    Ok(())
}

fn spawn_url_reaper(
    mut child: std::process::Child,
    program: String,
    url: String,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let span = tracing::Span::current();
    std::thread::Builder::new()
        .name("lazybox-open-url".into())
        .spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let _guard = span.enter();
                match child.wait() {
                    Ok(status) if !status.success() => {
                        tracing::warn!(%program, %status, %url, "browser launcher exited non-zero");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(%program, %url, "browser launcher wait failed: {e}");
                    }
                }
            });
        })
}

/// What an editor launch did with a requested file location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFileOutcome {
    Opened,
    OpenedAt { line: u32, column: Option<u32> },
    OpenedWithoutLocation { line: u32, column: Option<u32> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessHandling {
    DetachedEditor,
    MacosApplicationHandoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorLaunch {
    program: String,
    args: Vec<String>,
    process_handling: ProcessHandling,
}

fn substitute_path(args: &[String], target: &str, append_when_missing: bool) -> Vec<String> {
    let mut substituted = false;
    let mut resolved = args
        .iter()
        .map(|arg| {
            if arg.contains("{path}") {
                substituted = true;
                arg.replace("{path}", target)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>();
    if append_when_missing && !substituted {
        resolved.push(target.to_string());
    }
    resolved
}

fn open_file_launch(
    template: &EditorTemplate,
    file: &str,
    line: Option<u32>,
    col: Option<u32>,
) -> (EditorLaunch, OpenFileOutcome) {
    let requested_location = line.map(|line| OpenFileOutcome::OpenedAt { line, column: col });
    match &template.launcher {
        EditorLauncher::MacosApplication { bundle_path } => {
            let outcome = line.map_or(OpenFileOutcome::Opened, |line| {
                OpenFileOutcome::OpenedWithoutLocation { line, column: col }
            });
            (
                EditorLaunch {
                    program: "open".to_string(),
                    args: vec![
                        "-a".to_string(),
                        bundle_path.to_string_lossy().into_owned(),
                        file.to_string(),
                    ],
                    process_handling: ProcessHandling::MacosApplicationHandoff,
                },
                outcome,
            )
        }
        EditorLauncher::Command { program, args } => {
            let mut target = file.to_string();
            let mut location_args = Vec::new();
            if let Some(line) = line {
                match template.location_style {
                    FileLocationStyle::PathSuffix => {
                        target.push(':');
                        target.push_str(&line.to_string());
                        if let Some(col) = col {
                            target.push(':');
                            target.push_str(&col.to_string());
                        }
                    }
                    FileLocationStyle::VsCodeGoto => {
                        location_args.push("--goto".to_string());
                        target.push(':');
                        target.push_str(&line.to_string());
                        if let Some(col) = col {
                            target.push(':');
                            target.push_str(&col.to_string());
                        }
                    }
                    FileLocationStyle::JetBrains => {
                        location_args.extend(["--line".to_string(), line.to_string()]);
                        if let Some(col) = col {
                            location_args.extend(["--column".to_string(), col.to_string()]);
                        }
                    }
                }
            }
            location_args.extend(substitute_path(args, &target, true));
            (
                EditorLaunch {
                    program: program.clone(),
                    args: location_args,
                    process_handling: ProcessHandling::DetachedEditor,
                },
                requested_location.unwrap_or(OpenFileOutcome::Opened),
            )
        }
    }
}

fn worktree_launch(template: &EditorTemplate, worktree: &Path) -> EditorLaunch {
    let path = worktree.to_string_lossy();
    match &template.launcher {
        EditorLauncher::Command { program, args } => EditorLaunch {
            program: program.clone(),
            args: substitute_path(args, &path, false),
            process_handling: ProcessHandling::DetachedEditor,
        },
        EditorLauncher::MacosApplication { bundle_path } => EditorLaunch {
            program: "open".to_string(),
            args: vec![
                "-a".to_string(),
                bundle_path.to_string_lossy().into_owned(),
                path.into_owned(),
            ],
            process_handling: ProcessHandling::MacosApplicationHandoff,
        },
    }
}

fn spawn_editor(launch: EditorLaunch, current_dir: Option<&Path>) -> std::io::Result<()> {
    let mut command = std::process::Command::new(&launch.program);
    command.args(&launch.args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    if launch.process_handling == ProcessHandling::DetachedEditor {
        crate::platform::detach_child_process(&mut command);
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    let child = command.spawn()?;
    // A macOS `open` handoff hands the target to Launch Services and exits
    // within milliseconds; without a `wait()` that fast-exiting child
    // lingers as a zombie until lazybox itself exits. Reap it off-thread
    // (mirroring `open_url`) so it can't accumulate. A `DetachedEditor` is
    // `setsid`-detached and long-lived — leave it to the OS as before.
    if launch.process_handling == ProcessHandling::MacosApplicationHandoff {
        spawn_handoff_reaper(child, launch.program)
            .map(|_| ())
            .unwrap_or_else(|e| tracing::warn!("open-with handoff reaper spawn failed: {e}"));
    }
    Ok(())
}

fn spawn_handoff_reaper(
    mut child: std::process::Child,
    program: String,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let span = tracing::Span::current();
    std::thread::Builder::new()
        .name("lazybox-open-handoff".into())
        .spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let _guard = span.enter();
                match child.wait() {
                    Ok(status) if !status.success() => {
                        tracing::warn!(%program, %status, "open-with launcher exited non-zero");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(%program, "open-with launcher wait failed: {e}");
                    }
                }
            });
        })
}

/// Open a single `file` in `template`, optionally jumping to
/// `line[:col]`. Used by the terminal right-click handler when the
/// user clicks a file path in the agent transcript.
///
/// The result reports whether the selected launch mechanism can honor
/// the requested location. macOS app handoffs open the file but cannot
/// pass editor-specific line arguments through Launch Services.
pub fn open_file(
    template: &EditorTemplate,
    file: &Path,
    line: Option<u32>,
    col: Option<u32>,
) -> std::io::Result<OpenFileOutcome> {
    let (launch, outcome) = open_file_launch(template, &file.to_string_lossy(), line, col);
    let current_dir = file.parent().filter(|parent| parent.is_dir());
    spawn_editor(launch, current_dir).map(|()| outcome)
}

pub fn launch(template: &EditorTemplate, worktree: &Path) -> std::io::Result<()> {
    spawn_editor(worktree_launch(template, worktree), Some(worktree))
}

/// One configured "Open with…" target (issue #1100): an arbitrary app
/// launched on the focused workspace, decoupled from the single
/// code-editor slot. Unlike [`EditorTemplate`] it carries no go-to-file
/// semantics and no macOS app-bundle auto-detection — the command line
/// is taken verbatim (the user writes `open -a Obsidian {path}`), with
/// tokens substituted at launch. Reuses the editor spawn primitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWithApp {
    /// User-visible name shown in the picker.
    pub name: String,
    pub command: String,
    /// Defaults to `["{path}"]` when omitted, matching the common
    /// "open this folder" case.
    pub args: Option<Vec<String>>,
    /// Optional key-spec (`ui.action_keys`-style, e.g. `"O"`, `"Ctrl-o"`)
    /// that binds this app to a direct Workspace-section chord, skipping
    /// the picker (#1100). `None` → reachable only through the `x o`
    /// picker.
    pub key: Option<String>,
}

impl OpenWithApp {
    /// The launch args, defaulting to `["{path}"]` when omitted.
    fn resolved_args(&self) -> Vec<String> {
        self.args
            .clone()
            .unwrap_or_else(|| vec!["{path}".to_string()])
    }

    /// Whether the command references the worktree `{path}` token — the
    /// default when `args` is omitted. `{path}` is a server-side location
    /// over `--connect` and absent until a session provisions a worktree,
    /// so a path app can't run against a remote or not-yet-provisioned
    /// worktree; `{url}`/`{repo}`/`{branch}` apps (e.g. "open the PR in a
    /// browser") have no such dependency and run regardless (#1100).
    pub fn references_path(&self) -> bool {
        self.resolved_args()
            .iter()
            .any(|arg| arg.contains("{path}"))
    }

    /// The first `{…}` token the app needs that this workspace can't
    /// supply, or `None` when it can run. `{url}`/`{repo}`/`{branch}` need
    /// a concrete value in `ctx`; `{path}` counts as suppliable when
    /// `path_ok` (a local worktree exists or can be provisioned — false on
    /// a remote daemon, whose worktree is server-side). Drives the picker's
    /// "hide what can't run here" filter (#1100).
    pub fn missing_token(&self, ctx: &OpenWithContext, path_ok: bool) -> Option<&'static str> {
        let args = self.resolved_args();
        [
            ("{path}", path_ok),
            ("{url}", ctx.url.is_some()),
            ("{branch}", ctx.branch.is_some()),
            ("{repo}", ctx.repo.is_some()),
        ]
        .into_iter()
        .find(|(token, available)| !available && args.iter().any(|arg| arg.contains(token)))
        .map(|(token, _)| token)
    }

    /// Whether this app can launch on the workspace described by `ctx`
    /// (see [`Self::missing_token`]).
    pub fn is_actionable(&self, ctx: &OpenWithContext, path_ok: bool) -> bool {
        self.missing_token(ctx, path_ok).is_none()
    }

    /// A compact `command args…` string shown beneath the name in the
    /// picker so the user sees what each entry actually launches (#1100).
    pub fn command_preview(&self) -> String {
        let mut parts = vec![self.command.clone()];
        if let Some(args) = &self.args {
            parts.extend(args.iter().cloned());
        }
        parts.join(" ")
    }
}

/// Whether `command` is the macOS `open` launcher — matched by file name
/// so an absolute `/usr/bin/open` is recognized too (#1100). `open` hands
/// off to Launch Services and must not be `setsid`-detached (that severs
/// the GUI bootstrap it needs), so it takes the handoff spawn path.
fn is_open_launcher(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("open")
}

/// Values available to the `{…}` tokens in an [`OpenWithApp`]'s args.
/// Every field is optional: a token referenced by an app whose value is
/// `None` here (e.g. `{url}` on a workspace with no PR) makes the launch
/// fail with a message naming the token, rather than passing a literal
/// `{url}` through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenWithContext {
    /// The worktree directory (`{path}`).
    pub path: Option<String>,
    /// The workspace's PR / issue URL (`{url}`).
    pub url: Option<String>,
    /// The tracked branch (`{branch}`).
    pub branch: Option<String>,
    /// The `owner/repo` (`{repo}`).
    pub repo: Option<String>,
}

/// Substitute `{path}` / `{url}` / `{branch}` / `{repo}` in `args`
/// against `ctx`. A token whose value is absent is an error naming it,
/// so an app wired to a token the workspace can't supply fails loudly
/// instead of launching with a stray placeholder.
fn resolve_open_with_args(args: &[String], ctx: &OpenWithContext) -> Result<Vec<String>, String> {
    let tokens = [
        ("{path}", &ctx.path),
        ("{url}", &ctx.url),
        ("{branch}", &ctx.branch),
        ("{repo}", &ctx.repo),
    ];
    args.iter()
        .map(|arg| {
            let mut resolved = arg.clone();
            for (token, value) in &tokens {
                if resolved.contains(token) {
                    match value {
                        Some(value) => resolved = resolved.replace(token, value),
                        None => {
                            return Err(format!("{token} is unavailable on this workspace"));
                        }
                    }
                }
            }
            Ok(resolved)
        })
        .collect()
}

fn open_with_launch(app: &OpenWithApp, ctx: &OpenWithContext) -> Result<EditorLaunch, String> {
    let args = app
        .args
        .clone()
        .unwrap_or_else(|| vec!["{path}".to_string()]);
    let args = resolve_open_with_args(&args, ctx)?;
    // `open` hands the target off to Launch Services and exits — it must
    // NOT be detached, since `setsid()` severs the macOS GUI bootstrap it
    // needs (the same reason [`open_url`] never detaches). Any other
    // command is a GUI binary we detach like an editor so it outlives
    // lazybox.
    let process_handling = if is_open_launcher(&app.command) {
        ProcessHandling::MacosApplicationHandoff
    } else {
        ProcessHandling::DetachedEditor
    };
    Ok(EditorLaunch {
        program: app.command.clone(),
        args,
        process_handling,
    })
}

/// Launch an "Open with…" app on the focused workspace, substituting
/// its command's `{…}` tokens from `ctx`. A token the workspace can't
/// supply is an `Err` naming it; otherwise the command is spawned
/// through the shared editor spawn path (detach / macOS handoff).
pub fn launch_open_with(app: &OpenWithApp, ctx: &OpenWithContext) -> Result<(), String> {
    let launch = open_with_launch(app, ctx)?;
    let current_dir = ctx
        .path
        .as_deref()
        .map(Path::new)
        .filter(|path| path.is_dir());
    spawn_editor(launch, current_dir).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct SharedLog(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut log = self
                .0
                .lock()
                .map_err(|_| std::io::Error::other("trace log lock poisoned"))?;
            log.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedLog {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn user_entry_overrides_builtin_command() {
        let user = vec![EditorTemplate {
            id: "zed".to_string(),
            display: "Zed (custom)".to_string(),
            launcher: EditorLauncher::Command {
                program: "/opt/zed/zed".to_string(),
                args: vec!["{path}".into()],
            },
            macos_app_name: None,
            location_style: FileLocationStyle::PathSuffix,
        }];
        let merged = merge(builtin_editors(), user);
        let zed = merged.iter().find(|e| e.id == "zed").expect("zed present");
        assert_eq!(
            zed.launcher,
            EditorLauncher::Command {
                program: "/opt/zed/zed".to_string(),
                args: vec!["{path}".into()],
            }
        );
        assert_eq!(zed.display, "Zed (custom)");
    }

    #[test]
    fn user_entry_with_new_id_appends() {
        let user = vec![EditorTemplate {
            id: "myedit".to_string(),
            display: "My".to_string(),
            launcher: EditorLauncher::Command {
                program: "myedit".to_string(),
                args: vec!["{path}".into()],
            },
            macos_app_name: None,
            location_style: FileLocationStyle::PathSuffix,
        }];
        let merged = merge(builtin_editors(), user);
        assert!(merged.iter().any(|e| e.id == "myedit"));
        assert!(merged.iter().any(|e| e.id == "zed"), "builtins preserved");
    }

    #[test]
    fn user_entry_default_args_use_path_placeholder() {
        let raw = UserEditorEntry {
            id: "x".into(),
            display: None,
            command: "x".into(),
            args: None,
        };
        let t: EditorTemplate = raw.into();
        assert_eq!(
            t.launcher,
            EditorLauncher::Command {
                program: "x".to_string(),
                args: vec!["{path}".to_string()],
            }
        );
    }

    #[test]
    fn user_override_keeps_the_known_editors_location_contract() {
        let pycharm: EditorTemplate = UserEditorEntry {
            id: "pycharm".into(),
            display: None,
            command: "/custom/pycharm".into(),
            args: None,
        }
        .into();
        let codium: EditorTemplate = UserEditorEntry {
            id: "codium".into(),
            display: None,
            command: "/custom/codium".into(),
            args: None,
        }
        .into();

        assert_eq!(
            open_file_launch(&pycharm, "/a/b.rs", Some(12), Some(3))
                .0
                .args,
            vec!["--line", "12", "--column", "3", "/a/b.rs"]
        );
        assert_eq!(
            open_file_launch(&codium, "/a/b.rs", Some(12), Some(3))
                .0
                .args,
            vec!["--goto", "/a/b.rs:12:3"]
        );
    }

    fn tpl(id: &str, args: &[&str]) -> EditorTemplate {
        EditorTemplate {
            id: id.into(),
            display: id.into(),
            launcher: EditorLauncher::Command {
                program: id.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
            },
            macos_app_name: None,
            location_style: FileLocationStyle::PathSuffix,
        }
    }

    fn builtin(id: &str) -> EditorTemplate {
        builtin_editors()
            .into_iter()
            .find(|editor| editor.id == id)
            .unwrap_or_else(|| panic!("{id} builtin present"))
    }

    #[test]
    fn builtin_registry_covers_common_gui_editors() {
        let builtins = builtin_editors();
        let expected = [
            ("zed", "Zed", "zed", "Zed"),
            ("code", "VS Code", "code", "Visual Studio Code"),
            ("cursor", "Cursor", "cursor", "Cursor"),
            ("windsurf", "Windsurf", "windsurf", "Windsurf"),
            ("subl", "Sublime Text", "subl", "Sublime Text"),
            ("fleet", "JetBrains Fleet", "fleet", "Fleet"),
            ("idea", "IntelliJ IDEA", "idea", "IntelliJ IDEA"),
            ("pycharm", "PyCharm", "pycharm", "PyCharm"),
            ("webstorm", "WebStorm", "webstorm", "WebStorm"),
            ("goland", "GoLand", "goland", "GoLand"),
            ("clion", "CLion", "clion", "CLion"),
            ("rubymine", "RubyMine", "rubymine", "RubyMine"),
            ("phpstorm", "PhpStorm", "phpstorm", "PhpStorm"),
            ("rider", "Rider", "rider", "Rider"),
            ("datagrip", "DataGrip", "datagrip", "DataGrip"),
            (
                "android-studio",
                "Android Studio",
                "android-studio",
                "Android Studio",
            ),
            ("conductor", "Conductor", "conductor", "Conductor"),
            ("nova", "Nova", "nova", "Nova"),
            ("mate", "TextMate", "mate", "TextMate"),
            ("bbedit", "BBEdit", "bbedit", "BBEdit"),
            ("emacs", "Emacs", "emacs", "Emacs"),
            ("gram", "Gram", "gram", "Gram"),
        ];

        for (id, display, command, app_name) in expected {
            let editor = builtins
                .iter()
                .find(|editor| editor.id == id)
                .unwrap_or_else(|| panic!("{id} builtin present"));
            assert_eq!(editor.display, display);
            assert_eq!(
                editor.launcher,
                EditorLauncher::Command {
                    program: command.to_string(),
                    args: vec!["{path}".to_string()],
                }
            );
            assert_eq!(editor.macos_app_name.as_deref(), Some(app_name));
        }
    }

    #[test]
    fn detection_prefers_an_available_cli_over_a_macos_app() {
        let apps = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(apps.path().join("Zed.app")).expect("create app bundle");
        let mut zed = tpl("zed", &["{path}"]);
        zed.macos_app_name = Some("Zed".into());

        let detected = detect_available_with(&[zed], &[apps.path().to_path_buf()], |command| {
            command == "zed"
        });

        assert_eq!(detected.len(), 1);
        assert_eq!(
            detected[0].launcher,
            EditorLauncher::Command {
                program: "zed".to_string(),
                args: vec!["{path}".to_string()],
            }
        );
    }

    #[test]
    fn detection_falls_back_to_an_exact_macos_app_name() {
        let apps = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(apps.path().join("Sublime Text.app")).expect("create app bundle");
        let mut sublime = tpl("subl", &["{path}"]);
        sublime.macos_app_name = Some("Sublime Text".into());

        let detected = detect_available_with(&[sublime], &[apps.path().to_path_buf()], |_| false);

        assert_eq!(detected.len(), 1);
        assert_eq!(
            detected[0].launcher,
            EditorLauncher::MacosApplication {
                bundle_path: apps.path().join("Sublime Text.app"),
            }
        );
    }

    #[test]
    fn detection_falls_back_to_a_versioned_jetbrains_toolbox_app() {
        let system_apps = tempfile::tempdir().expect("system tempdir");
        let user_apps = tempfile::tempdir().expect("user tempdir");
        std::fs::create_dir(user_apps.path().join("PyCharm Professional 2026.1.app"))
            .expect("create Toolbox app bundle");
        let mut pycharm = tpl("pycharm", &["{path}"]);
        pycharm.macos_app_name = Some("PyCharm".into());

        let detected = detect_available_with(
            &[pycharm],
            &[
                system_apps.path().join("missing"),
                user_apps.path().to_path_buf(),
            ],
            |_| false,
        );

        assert_eq!(detected.len(), 1);
        assert_eq!(
            detected[0].launcher,
            EditorLauncher::MacosApplication {
                bundle_path: user_apps.path().join("PyCharm Professional 2026.1.app"),
            }
        );
    }

    #[test]
    fn detection_selects_the_newest_toolbox_version_across_editions_and_directories() {
        let system_apps = tempfile::tempdir().expect("system tempdir");
        let user_apps = tempfile::tempdir().expect("user tempdir");
        std::fs::create_dir(system_apps.path().join("PyCharm Professional 2026.9.app"))
            .expect("create older professional bundle");
        let newest = user_apps.path().join("PyCharm Community 2026.10.app");
        std::fs::create_dir(&newest).expect("create newer community bundle");
        let mut pycharm = tpl("pycharm", &["{path}"]);
        pycharm.macos_app_name = Some("PyCharm".into());

        let detected = detect_available_with(
            &[pycharm],
            &[
                system_apps.path().to_path_buf(),
                user_apps.path().to_path_buf(),
            ],
            |_| false,
        );

        assert_eq!(
            detected[0].launcher,
            EditorLauncher::MacosApplication {
                bundle_path: newest,
            }
        );
    }

    #[test]
    fn detection_excludes_an_editor_without_a_cli_or_macos_app() {
        let apps = tempfile::tempdir().expect("tempdir");
        let mut zed = tpl("zed", &["{path}"]);
        zed.macos_app_name = Some("Zed".into());

        assert!(detect_available_with(&[zed], &[apps.path().to_path_buf()], |_| false).is_empty());
    }

    #[test]
    fn open_file_launch_substitutes_plain_path() {
        let t = tpl("zed", &["{path}"]);
        let (launch, outcome) = open_file_launch(&t, "/a/b.rs", None, None);
        assert_eq!(launch.args, vec!["/a/b.rs"]);
        assert_eq!(outcome, OpenFileOutcome::Opened);
    }

    #[test]
    fn open_file_launch_appends_line_col_suffix() {
        let t = tpl("zed", &["{path}"]);
        let (with_column, outcome) = open_file_launch(&t, "/a/b.rs", Some(12), Some(3));
        assert_eq!(with_column.args, vec!["/a/b.rs:12:3"]);
        assert_eq!(
            outcome,
            OpenFileOutcome::OpenedAt {
                line: 12,
                column: Some(3),
            }
        );
        let (without_column, outcome) = open_file_launch(&t, "/a/b.rs", Some(12), None);
        assert_eq!(without_column.args, vec!["/a/b.rs:12"]);
        assert_eq!(
            outcome,
            OpenFileOutcome::OpenedAt {
                line: 12,
                column: None,
            }
        );
    }

    #[test]
    fn open_file_launch_prepends_goto_for_vscode_family() {
        let t = builtin("code");
        let (with_line, _) = open_file_launch(&t, "/a/b.rs", Some(7), None);
        assert_eq!(with_line.args, vec!["--goto", "/a/b.rs:7"]);
        let (without_line, _) = open_file_launch(&t, "/a/b.rs", None, None);
        assert_eq!(without_line.args, vec!["/a/b.rs"]);
    }

    #[test]
    fn open_file_launch_uses_jetbrains_line_and_column_flags() {
        for id in [
            "idea",
            "pycharm",
            "webstorm",
            "goland",
            "clion",
            "rubymine",
            "phpstorm",
            "rider",
            "datagrip",
            "android-studio",
        ] {
            let (launch, outcome) = open_file_launch(&builtin(id), "/a/b.rs", Some(12), Some(3));
            assert_eq!(
                launch.args,
                vec!["--line", "12", "--column", "3", "/a/b.rs"],
                "{id}"
            );
            assert_eq!(
                outcome,
                OpenFileOutcome::OpenedAt {
                    line: 12,
                    column: Some(3),
                },
                "{id}"
            );
        }
    }

    #[test]
    fn open_file_launch_reports_unsupported_location_for_a_macos_app() {
        let t = EditorTemplate {
            launcher: EditorLauncher::MacosApplication {
                bundle_path: PathBuf::from("/Applications/Visual Studio Code.app"),
            },
            ..builtin("code")
        };
        let (launch, outcome) = open_file_launch(&t, "/a/b.rs", Some(7), Some(3));
        assert_eq!(
            launch,
            EditorLaunch {
                program: "open".to_string(),
                args: vec![
                    "-a".to_string(),
                    "/Applications/Visual Studio Code.app".to_string(),
                    "/a/b.rs".to_string(),
                ],
                process_handling: ProcessHandling::MacosApplicationHandoff,
            }
        );
        assert_eq!(
            outcome,
            OpenFileOutcome::OpenedWithoutLocation {
                line: 7,
                column: Some(3),
            }
        );
    }

    #[test]
    fn worktree_launch_hands_an_exact_macos_bundle_to_launch_services() {
        let t = EditorTemplate {
            launcher: EditorLauncher::MacosApplication {
                bundle_path: PathBuf::from("/Applications/PyCharm 2026.1.app"),
            },
            ..builtin("pycharm")
        };

        assert_eq!(
            worktree_launch(&t, Path::new("/repo/worktree")),
            EditorLaunch {
                program: "open".to_string(),
                args: vec![
                    "-a".to_string(),
                    "/Applications/PyCharm 2026.1.app".to_string(),
                    "/repo/worktree".to_string(),
                ],
                process_handling: ProcessHandling::MacosApplicationHandoff,
            }
        );
    }

    #[test]
    fn gram_is_a_builtin_editor() {
        let builtins = builtin_editors();
        let gram = builtins
            .iter()
            .find(|e| e.id == "gram")
            .expect("gram builtin present");
        assert_eq!(gram.display, "Gram");
        assert_eq!(
            gram.launcher,
            EditorLauncher::Command {
                program: "gram".to_string(),
                args: vec!["{path}".to_string()],
            }
        );
        assert_eq!(gram.macos_app_name.as_deref(), Some("Gram"));
        // "Glam" is a mishearing of Gram (the Zed fork); there is no
        // separate `glam` editor, so the builtin stays a single entry.
        assert!(
            !builtins.iter().any(|e| e.id == "glam"),
            "no separate glam entry; Glam is a mishearing of Gram"
        );
    }

    #[test]
    fn open_url_rejects_non_http() {
        // Returns before spawning anything, so this is safe to run in CI.
        let err = open_url("file:///etc/passwd", None).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn launcher_exit_log_retains_the_originating_click_span() {
        let log = SharedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer(log.clone())
            .finish();
        let reaper = tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("terminal_right_click", column = 17, row = 9);
            let _guard = span.enter();
            let mut command = if cfg!(windows) {
                let mut command = std::process::Command::new("cmd");
                command.args(["/c", "exit", "7"]);
                command
            } else {
                let mut command = std::process::Command::new("sh");
                command.args(["-c", "exit 7"]);
                command
            };
            let child = command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn failing launcher");
            spawn_url_reaper(
                child,
                "test-browser".into(),
                "https://trace.example.test".into(),
            )
            .expect("spawn URL reaper")
        });
        reaper.join().expect("URL reaper joins");

        let output = String::from_utf8(log.0.lock().expect("trace log lock").clone())
            .expect("trace output is UTF-8");
        assert!(output.contains("browser launcher exited non-zero"));
        assert!(output.contains("terminal_right_click"));
        assert!(output.contains("column=17"));
        assert!(output.contains("row=9"));
        assert!(output.contains("https://trace.example.test"));
    }

    #[cfg(unix)]
    #[test]
    fn editor_session_probe() {
        let current_dir = std::env::current_dir().expect("current directory");
        let is_probe = current_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("lazybox-handoff-probe"));
        if is_probe {
            // Safety: getsid with pid 0 only reads the calling process's
            // session id and does not dereference memory.
            let session = unsafe { libc::getsid(0) };
            std::fs::write(current_dir.join("session"), session.to_string())
                .expect("write session id");
        }
    }

    #[cfg(unix)]
    #[test]
    fn macos_application_handoff_preserves_the_callers_session() {
        let dir = tempfile::Builder::new()
            .prefix("lazybox-handoff-probe")
            .tempdir()
            .expect("tempdir");
        let session_file = dir.path().join("session");
        let launch = EditorLaunch {
            program: std::env::current_exe()
                .expect("current test executable")
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".to_string(),
                "editors::tests::editor_session_probe".to_string(),
            ],
            process_handling: ProcessHandling::MacosApplicationHandoff,
        };

        spawn_editor(launch, Some(dir.path())).expect("spawn handoff probe");

        let mut child_session = None;
        for _ in 0..100 {
            child_session = std::fs::read_to_string(&session_file)
                .ok()
                .and_then(|value| value.trim().parse::<libc::pid_t>().ok());
            if child_session.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let child_session = child_session.expect("handoff probe writes a numeric session id");
        // Safety: getsid with pid 0 only reads the calling process's
        // session id and does not dereference memory.
        let parent_session = unsafe { libc::getsid(0) };

        assert_eq!(child_session, parent_session);
    }

    /// On Linux a configured browser is exec'd directly, so a missing
    /// binary must surface as a spawn error — the ONLY error the
    /// detached launch still reports (a launcher that starts and then
    /// fails is log-only, and `open_url` never blocks on the child).
    /// macOS routes through `open -a`, so the equivalent failure
    /// happens post-spawn there and this test doesn't apply.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn open_url_reports_missing_browser_binary_at_spawn() {
        let err = open_url(
            "https://x.test/pr/1",
            Some("/definitely/not/a/real/browser-binary"),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_argv_default_uses_open() {
        let (prog, args) = browser_argv("https://x.test/pr/1", None);
        assert_eq!(prog, "open");
        assert_eq!(args, vec!["https://x.test/pr/1"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn browser_argv_named_browser_uses_open_dash_a() {
        let (prog, args) = browser_argv("https://x.test/pr/1", Some("Google Chrome"));
        assert_eq!(prog, "open");
        assert_eq!(args, vec!["-a", "Google Chrome", "https://x.test/pr/1"]);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn browser_argv_named_browser_runs_executable_directly() {
        let (prog, args) = browser_argv("https://x.test/pr/1", Some("firefox"));
        assert_eq!(prog, "firefox");
        assert_eq!(args, vec!["https://x.test/pr/1"]);

        let (prog, args) = browser_argv("https://x.test/pr/1", None);
        assert_eq!(prog, "xdg-open");
        assert_eq!(args, vec!["https://x.test/pr/1"]);
    }

    #[test]
    fn open_file_launch_appends_target_when_no_placeholder() {
        let t = tpl("myedit", &["--flag"]);
        let (launch, _) = open_file_launch(&t, "/a/b.rs", None, None);
        assert_eq!(launch.args, vec!["--flag", "/a/b.rs"]);
    }

    fn open_with(command: &str, args: &[&str]) -> OpenWithApp {
        OpenWithApp {
            name: command.to_string(),
            command: command.to_string(),
            args: Some(args.iter().map(|s| s.to_string()).collect()),
            key: None,
        }
    }

    #[test]
    fn open_with_defaults_args_to_the_path_placeholder() {
        let app = OpenWithApp {
            name: "Finder".into(),
            command: "open".into(),
            args: None,
            key: None,
        };
        let ctx = OpenWithContext {
            path: Some("/repo/wt".into()),
            ..OpenWithContext::default()
        };
        let launch = open_with_launch(&app, &ctx).expect("launch built");
        assert_eq!(launch.program, "open");
        assert_eq!(launch.args, vec!["/repo/wt"]);
        // `open` hands off to Launch Services; never detached.
        assert_eq!(
            launch.process_handling,
            ProcessHandling::MacosApplicationHandoff
        );
    }

    #[test]
    fn open_with_substitutes_every_token() {
        let app = open_with(
            "open",
            &["-a", "Obsidian", "{path}", "{url}", "{branch}", "{repo}"],
        );
        let ctx = OpenWithContext {
            path: Some("/repo/wt".into()),
            url: Some("https://github.com/acme/widget/pull/7".into()),
            branch: Some("feat/x".into()),
            repo: Some("acme/widget".into()),
        };
        let launch = open_with_launch(&app, &ctx).expect("launch built");
        assert_eq!(
            launch.args,
            vec![
                "-a",
                "Obsidian",
                "/repo/wt",
                "https://github.com/acme/widget/pull/7",
                "feat/x",
                "acme/widget",
            ]
        );
    }

    #[test]
    fn open_with_errors_on_a_token_the_workspace_cannot_supply() {
        let app = open_with("open", &["{url}"]);
        let ctx = OpenWithContext {
            path: Some("/repo/wt".into()),
            ..OpenWithContext::default()
        };
        let err = open_with_launch(&app, &ctx).unwrap_err();
        assert!(
            err.contains("{url}"),
            "error names the missing token: {err}"
        );
    }

    #[test]
    fn references_path_gates_only_apps_that_use_the_worktree_token() {
        // Default args are `["{path}"]`, so an omitted-args app needs the
        // worktree.
        assert!(
            OpenWithApp {
                name: "Finder".into(),
                command: "open".into(),
                args: None,
                key: None,
            }
            .references_path()
        );
        assert!(open_with("open", &["-a", "Obsidian", "{path}"]).references_path());
        // A `{url}`-only app (e.g. "open the PR in a browser") has no
        // worktree dependency — it must NOT be gated as local/session-only.
        assert!(!open_with("open", &["{url}"]).references_path());
        assert!(!open_with("open", &["{repo}", "{branch}"]).references_path());
    }

    #[test]
    fn is_open_launcher_matches_open_by_file_name() {
        assert!(is_open_launcher("open"));
        // An absolute path must still be recognized so it takes the
        // handoff path rather than being setsid-detached (#1100).
        assert!(is_open_launcher("/usr/bin/open"));
        assert!(!is_open_launcher("xdg-open"));
        assert!(!is_open_launcher("obsidian"));
    }

    #[test]
    fn open_with_launch_recognizes_an_absolute_open_path_as_a_handoff() {
        let app = open_with("/usr/bin/open", &["-a", "Obsidian", "{path}"]);
        let ctx = OpenWithContext {
            path: Some("/repo/wt".into()),
            ..OpenWithContext::default()
        };
        let launch = open_with_launch(&app, &ctx).expect("launch built");
        assert_eq!(
            launch.process_handling,
            ProcessHandling::MacosApplicationHandoff,
            "/usr/bin/open must hand off, not be setsid-detached",
        );
    }

    #[test]
    fn missing_token_reports_only_unsatisfiable_tokens() {
        // Workspace with a URL + branch + repo but no local worktree.
        let ctx = OpenWithContext {
            path: None,
            url: Some("https://x/pr/1".into()),
            branch: Some("feat".into()),
            repo: Some("o/r".into()),
        };
        // `{url}` app runs even with no worktree.
        assert!(open_with("open", &["{url}"]).is_actionable(&ctx, false));
        // `{path}` app can't run when path isn't OK (remote → can't provision).
        assert_eq!(
            open_with("open", &["{path}"]).missing_token(&ctx, false),
            Some("{path}"),
        );
        // …but is fine once `{path}` is suppliable (local, provisionable).
        assert!(open_with("open", &["{path}"]).is_actionable(&ctx, true));
        // A `{url}` app on a PR-less workspace is not actionable.
        let bare = OpenWithContext::default();
        assert_eq!(
            open_with("open", &["{url}"]).missing_token(&bare, true),
            Some("{url}"),
        );
    }

    #[test]
    fn command_preview_shows_the_command_and_args() {
        assert_eq!(
            open_with("open", &["-a", "Obsidian", "{path}"]).command_preview(),
            "open -a Obsidian {path}",
        );
        assert_eq!(
            OpenWithApp {
                name: "Finder".into(),
                command: "open".into(),
                args: None,
                key: None,
            }
            .command_preview(),
            "open",
        );
    }

    #[test]
    fn open_with_detaches_a_non_open_gui_binary() {
        let app = open_with("obsidian", &["{path}"]);
        let ctx = OpenWithContext {
            path: Some("/repo/wt".into()),
            ..OpenWithContext::default()
        };
        let launch = open_with_launch(&app, &ctx).expect("launch built");
        assert_eq!(launch.process_handling, ProcessHandling::DetachedEditor);
    }
}
