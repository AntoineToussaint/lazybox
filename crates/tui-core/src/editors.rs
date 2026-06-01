//! Editor detection + launch helpers.
//!
//! Pilot exposes an `o` (sidebar) shortcut that opens the focused
//! workspace's worktree in the user's editor of choice. We probe
//! PATH at startup for known editors (Zed, VS Code, Cursor, Vim,
//! Neovim, Helix, JetBrains IDEs, Fleet, …) so the user doesn't
//! have to configure anything if they have a standard install.
//!
//! ## Customization
//!
//! `~/.pilot/config.yaml` can add custom editors via an `editors:`
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
//! The editor process is detached from pilot — closing pilot
//! shouldn't take the editor with it. We use the same
//! `crate::platform::detach_child_process` helper as `Ctrl-Shift-D`
//! so the cross-platform story is consistent.

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
/// belong in a terminal pane, which pilot already provides. Users
/// who want them can add via `editors:` in `~/.pilot/config.yaml`.
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
        template("gram", "Gram", "gram"),
    ]
}

/// User entry in `~/.pilot/config.yaml::editors`. Mirrors the
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

/// Spawn `template` against `worktree`. Replaces `{path}`
/// placeholders in args. Detached so the editor outlives pilot.
/// Returns Ok on successful spawn — the editor's own success is
/// not waited on.
/// Open `url` in the host's default web browser. Picks the right
/// per-platform launcher (`open` on macOS, `xdg-open` on Linux,
/// `cmd /c start` on Windows) and detaches so the browser outlives
/// pilot. Used by the `O` (Shift-O) shortcut on workspaces to jump
/// to the PR / issue page on GitHub / Linear.
pub fn open_url(url: &str) -> std::io::Result<()> {
    // Minimal allow-list: must be http(s) so a malformed task URL
    // can't be coerced into running an arbitrary shell command.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to open non-http(s) URL: {url}"),
        ));
    }
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "start", "", url]);
        c
    } else {
        // Linux + the rest. `xdg-open` is the de facto standard;
        // most desktop distros ship it.
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    crate::platform::detach_child_process(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ())
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
/// the editor outlives pilot.
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
        let gram = builtin_editors()
            .into_iter()
            .find(|e| e.id == "gram")
            .expect("gram builtin present");
        assert_eq!(gram.display, "Gram");
        assert_eq!(gram.command, "gram");
        assert_eq!(gram.args, vec!["{path}"]);
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
