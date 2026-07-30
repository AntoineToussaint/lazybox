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
//! The editor process is detached from lazybox — closing lazybox
//! shouldn't take the editor with it, via the
//! `crate::platform::detach_child_process` helper.

use serde::Deserialize;
use std::path::{Path, PathBuf};

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
    /// macOS app-bundle name, used when the CLI is not on PATH.
    pub macos_app_name: Option<String>,
}

/// Built-in editor list. **GUI editors only** — vim/neovim/helix
/// belong in a terminal pane, which lazybox already provides. Users
/// who want them can add via `editors:` in `~/.lazybox/config.yaml`.
fn builtin_editors() -> Vec<EditorTemplate> {
    let template = |id: &str, display: &str, command: &str, macos_app_name: &str| EditorTemplate {
        id: id.to_string(),
        display: display.to_string(),
        command: command.to_string(),
        args: vec!["{path}".to_string()],
        macos_app_name: Some(macos_app_name.to_string()),
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
            macos_app_name: None,
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

fn find_macos_app_name(app_name: &str, application_dirs: &[PathBuf]) -> Option<String> {
    for dir in application_dirs {
        if dir.join(format!("{app_name}.app")).is_dir() {
            return Some(app_name.to_string());
        }

        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut matches = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_dir() || path.extension().and_then(|ext| ext.to_str()) != Some("app") {
                    return None;
                }
                let name = path.file_stem()?.to_str()?;
                name.strip_prefix(app_name)
                    .is_some_and(|suffix| suffix.starts_with(' '))
                    .then(|| name.to_string())
            })
            .collect::<Vec<_>>();
        matches.sort_unstable();
        if let Some(name) = matches.pop() {
            return Some(name);
        }
    }
    None
}

fn detect_available_with(
    templates: &[EditorTemplate],
    application_dirs: &[PathBuf],
    mut command_available: impl FnMut(&str) -> bool,
) -> Vec<EditorTemplate> {
    templates
        .iter()
        .filter_map(|template| {
            if command_available(&template.command) {
                return Some(template.clone());
            }

            let app_name =
                find_macos_app_name(template.macos_app_name.as_deref()?, application_dirs)?;
            let mut detected = template.clone();
            detected.command = "open".to_string();
            detected.args = vec!["-a".to_string(), app_name, "{path}".to_string()];
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

/// Build the argv (everything after the command) for opening `file`
/// at an optional `line[:col]` in `template`. Pure so it can be
/// tested without spawning a process.
///
/// The target is `file`, with `:line` / `:line:col` appended when
/// present. VS Code-family editors only honor that suffix after a
/// `--goto` flag, so we prepend it for them. macOS app launchers open
/// the file without a location because `open -a` cannot interpret
/// editor-specific line arguments. The target is then substituted
/// into the template's `{path}` placeholder (matching [`launch`]);
/// templates with no placeholder get the target appended as a
/// trailing positional argument.
fn open_file_args(
    template: &EditorTemplate,
    file: &str,
    line: Option<u32>,
    col: Option<u32>,
) -> Vec<String> {
    let uses_macos_app =
        template.command == "open" && template.args.first().map(String::as_str) == Some("-a");
    let mut target = file.to_string();
    if let Some(l) = line.filter(|_| !uses_macos_app) {
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
    if line.is_some() && vscode_family && !uses_macos_app {
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
            command: "/opt/zed/zed".to_string(),
            args: vec!["{path}".into()],
            macos_app_name: None,
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
            macos_app_name: None,
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
            macos_app_name: None,
        }
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
            assert_eq!(editor.command, command);
            assert_eq!(editor.args, vec!["{path}"]);
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
        assert_eq!(detected[0].command, "zed");
        assert_eq!(detected[0].args, vec!["{path}"]);
    }

    #[test]
    fn detection_falls_back_to_an_exact_macos_app_name() {
        let apps = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(apps.path().join("Sublime Text.app")).expect("create app bundle");
        let mut sublime = tpl("subl", &["{path}"]);
        sublime.macos_app_name = Some("Sublime Text".into());

        let detected = detect_available_with(&[sublime], &[apps.path().to_path_buf()], |_| false);

        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].command, "open");
        assert_eq!(detected[0].args, vec!["-a", "Sublime Text", "{path}"]);
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
        assert_eq!(detected[0].command, "open");
        assert_eq!(
            detected[0].args,
            vec!["-a", "PyCharm Professional 2026.1", "{path}"]
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
    fn open_file_args_for_a_macos_app_ignores_the_line_location() {
        let mut t = tpl("code", &["-a", "Visual Studio Code", "{path}"]);
        t.command = "open".into();
        assert_eq!(
            open_file_args(&t, "/a/b.rs", Some(7), Some(3)),
            vec!["-a", "Visual Studio Code", "/a/b.rs"]
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
        assert_eq!(gram.command, "gram");
        assert_eq!(gram.args, vec!["{path}"]);
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
