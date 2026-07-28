//! Editor detection + launch helpers.
//!
//! Lazybox exposes an `o` (sidebar) shortcut that opens the focused
//! workspace's worktree in the user's editor of choice. We probe
//! PATH at startup for known editors (Zed, VS Code, Cursor, Vim,
//! Neovim, Helix, JetBrains IDEs, Fleet, …) so the user doesn't
//! have to configure anything if they have a standard install.
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
//! The editor process is detached from lazybox — closing lazybox
//! shouldn't take the editor with it, via the
//! `crate::platform::detach_child_process` helper.

use serde::Deserialize;
use std::path::Path;

/// One launchable editor. Builtins are static; user additions are
/// loaded from config and merged at startup.
#[derive(Debug, Clone)]
pub struct EditorTemplate {
    /// Stable id used to match user overrides and persist a default.
    pub id: String,
    /// User-visible name shown in the picker.
    pub display: String,
    /// Executable to spawn. Looked up via `which::which` at
    /// detection time so we know whether it's actually installed.
    pub command: String,
    /// Argv. `{path}` is replaced with the worktree dir at launch.
    pub args: Vec<String>,
}

/// Built-in editor list. **GUI editors only** — vim/neovim/helix
/// belong in a terminal pane, which lazybox already provides. Users
/// who want them can add via `editors:` in `~/.lazybox/config.yaml`.
fn builtin_editors() -> Vec<EditorTemplate> {
    let template = |id: &str, display: &str, command: &str| EditorTemplate {
        id: id.to_string(),
        display: display.to_string(),
        command: command.to_string(),
        args: vec!["{path}".to_string()],
    };
    vec![
        template("zed", "Zed", "zed"),
        template("code", "VS Code", "code"),
        template("cursor", "Cursor", "cursor"),
        template("windsurf", "Windsurf", "windsurf"),
        template("fleet", "JetBrains Fleet", "fleet"),
        template("idea", "IntelliJ IDEA", "idea"),
        // Gram: a Zed fork with AI, telemetry, and collaboration
        // stripped out (codeberg.org/GramEditor/gram). Its CLI binary
        // is `gram`; obscure enough that the name reads like a typo.
        template("gram", "Gram", "gram"),
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
            command: u.command,
            args: u.args.unwrap_or_else(|| vec!["{path}".to_string()]),
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

/// Filter the provided template list to those whose command is
/// actually on PATH. Cheap (`which` resolves once per editor at
/// startup) and synchronous.
pub fn detect_available(templates: &[EditorTemplate]) -> Vec<EditorTemplate> {
    templates
        .iter()
        .filter(|t| which::which(&t.command).is_ok())
        .cloned()
        .collect()
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
    let mut child = std::process::Command::new(&program)
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
    std::thread::Builder::new()
        .name("lazybox-open-url".into())
        .spawn(move || match child.wait() {
            Ok(status) if !status.success() => {
                tracing::warn!(%program, %status, %url, "browser launcher exited non-zero");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%program, %url, "browser launcher wait failed: {e}"),
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("open_url reaper thread spawn failed: {e}"));
    Ok(())
}

/// Build the argv (everything after the command) for opening `file`
/// at an optional `line[:col]` in `template`. Pure so it can be
/// tested without spawning a process.
///
/// The target is `file`, with `:line` / `:line:col` appended when
/// present. VS Code-family editors only honor that suffix after a
/// `--goto` flag, so we prepend it for them. The target is then
/// substituted into the template's `{path}` placeholder (matching
/// [`launch`]); templates with no placeholder get the target
/// appended as a trailing positional argument.
fn open_file_args(
    template: &EditorTemplate,
    file: &str,
    line: Option<u32>,
    col: Option<u32>,
) -> Vec<String> {
    let mut target = file.to_string();
    if let Some(l) = line {
        target.push(':');
        target.push_str(&l.to_string());
        if let Some(c) = col {
            target.push(':');
            target.push_str(&c.to_string());
        }
    }

    let mut out = Vec::new();
    let vscode_family = matches!(
        template.id.as_str(),
        "code" | "cursor" | "windsurf" | "codium" | "vscodium"
    );
    if line.is_some() && vscode_family {
        out.push("--goto".to_string());
    }

    let mut substituted = false;
    for arg in &template.args {
        if arg.contains("{path}") {
            out.push(arg.replace("{path}", &target));
            substituted = true;
        } else {
            out.push(arg.clone());
        }
    }
    if !substituted {
        out.push(target);
    }
    out
}

/// Open a single `file` in `template`, optionally jumping to
/// `line[:col]`. Used by the terminal right-click handler when the
/// user clicks a file path in the agent transcript.
///
/// Most modern editors accept a positional `file:line:col` argument;
/// the VS Code family needs a `--goto` flag to interpret it, so we
/// special-case those by `id`. The file target is substituted into
/// the template's `{path}` placeholder (or appended when the template
/// has none), reusing the same argv shape as [`launch`]. Detached so
/// the editor outlives lazybox.
pub fn open_file(
    template: &EditorTemplate,
    file: &Path,
    line: Option<u32>,
    col: Option<u32>,
) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(&template.command);
    cmd.args(open_file_args(template, &file.to_string_lossy(), line, col));

    // Run from the file's directory when it exists so editors that
    // infer a project root from cwd land somewhere sensible.
    if let Some(parent) = file.parent()
        && parent.is_dir()
    {
        cmd.current_dir(parent);
    }
    crate::platform::detach_child_process(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ())
}

pub fn launch(template: &EditorTemplate, worktree: &Path) -> std::io::Result<()> {
    let path_str = worktree.to_string_lossy().into_owned();
    let mut cmd = std::process::Command::new(&template.command);
    for arg in &template.args {
        if arg == "{path}" {
            cmd.arg(&path_str);
        } else if arg.contains("{path}") {
            cmd.arg(arg.replace("{path}", &path_str));
        } else {
            cmd.arg(arg);
        }
    }
    cmd.current_dir(worktree);
    crate::platform::detach_child_process(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_entry_overrides_builtin_command() {
        let user = vec![EditorTemplate {
            id: "zed".to_string(),
            display: "Zed (custom)".to_string(),
            command: "/opt/zed/zed".to_string(),
            args: vec!["{path}".into()],
        }];
        let merged = merge(builtin_editors(), user);
        let zed = merged.iter().find(|e| e.id == "zed").expect("zed present");
        assert_eq!(zed.command, "/opt/zed/zed");
        assert_eq!(zed.display, "Zed (custom)");
    }

    #[test]
    fn user_entry_with_new_id_appends() {
        let user = vec![EditorTemplate {
            id: "myedit".to_string(),
            display: "My".to_string(),
            command: "myedit".to_string(),
            args: vec!["{path}".into()],
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
        assert_eq!(t.args, vec!["{path}".to_string()]);
    }

    fn tpl(id: &str, args: &[&str]) -> EditorTemplate {
        EditorTemplate {
            id: id.into(),
            display: id.into(),
            command: id.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn open_file_args_substitutes_plain_path() {
        let t = tpl("zed", &["{path}"]);
        assert_eq!(open_file_args(&t, "/a/b.rs", None, None), vec!["/a/b.rs"]);
    }

    #[test]
    fn open_file_args_appends_line_col_suffix() {
        let t = tpl("zed", &["{path}"]);
        assert_eq!(
            open_file_args(&t, "/a/b.rs", Some(12), Some(3)),
            vec!["/a/b.rs:12:3"]
        );
        assert_eq!(
            open_file_args(&t, "/a/b.rs", Some(12), None),
            vec!["/a/b.rs:12"]
        );
    }

    #[test]
    fn open_file_args_prepends_goto_for_vscode_family() {
        let t = tpl("code", &["{path}"]);
        assert_eq!(
            open_file_args(&t, "/a/b.rs", Some(7), None),
            vec!["--goto", "/a/b.rs:7"]
        );
        // No line → no --goto (a plain open).
        assert_eq!(open_file_args(&t, "/a/b.rs", None, None), vec!["/a/b.rs"]);
    }

    #[test]
    fn gram_is_a_builtin_editor() {
        let builtins = builtin_editors();
        let gram = builtins
            .iter()
            .find(|e| e.id == "gram")
            .expect("gram builtin present");
        assert_eq!(gram.display, "Gram");
        assert_eq!(gram.command, "gram");
        assert_eq!(gram.args, vec!["{path}"]);
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
    fn open_file_args_appends_target_when_no_placeholder() {
        // A template whose args don't mention {path} still gets the
        // file appended so the editor receives something to open.
        let t = tpl("myedit", &["--flag"]);
        assert_eq!(
            open_file_args(&t, "/a/b.rs", None, None),
            vec!["--flag", "/a/b.rs"]
        );
    }
}
