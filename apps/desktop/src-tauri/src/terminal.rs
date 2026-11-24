//! "Open in Terminal": the person's own terminal app, opened at a workspace's
//! folder. Lazybox is not a terminal emulator (DECISIONS.md); this hands the
//! folder to the terminal the person already uses, with `LAZYBOX_RUNNER`
//! naming this app's Runner so `lazybox run` / `lazybox log` typed there feed
//! the workspace's console even when several Runners are registered.
//!
//! One narrowly scoped command, `open_terminal`: only the app's own windows
//! (never a Web panel's viewer, never a console or artifact window) may call
//! it, and the folder must be inside a workspace, or a repository's home
//! checkout, that the Runner lists right now — the Runner is asked, never the
//! page. The page names only a folder and a terminal: a custom command line is
//! the host's own setting (`terminal.json` in the app's config folder), which
//! only `set_terminal_command` changes, and only once the person allowed that
//! exact line in a native confirmation. `terminal_apps` reports which of the
//! known terminals are installed, for Settings.
use crate::host::Host;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tauri::{Manager, State, Webview};

/// A terminal Lazybox knows how to open at a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum App {
    Terminal,
    Iterm,
    Ghostty,
    Wezterm,
    Warp,
}

impl App {
    const ALL: [App; 5] = [
        App::Ghostty,
        App::Iterm,
        App::Wezterm,
        App::Warp,
        App::Terminal,
    ];
    fn id(self) -> &'static str {
        match self {
            App::Terminal => "terminal",
            App::Iterm => "iterm",
            App::Ghostty => "ghostty",
            App::Wezterm => "wezterm",
            App::Warp => "warp",
        }
    }
    fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|app| app.id() == id)
    }
    /// The bundle's name, as `open -a` takes it.
    fn bundle(self) -> &'static str {
        match self {
            App::Terminal => "Terminal",
            App::Iterm => "iTerm",
            App::Ghostty => "Ghostty",
            App::Wezterm => "WezTerm",
            App::Warp => "Warp",
        }
    }
    /// Where macOS installs the app: the per-user and system Applications
    /// folders, and Utilities for Terminal.
    fn locations(self, home: Option<&Path>) -> Vec<PathBuf> {
        let name = match self {
            App::Iterm => "iTerm.app",
            App::Terminal => "Terminal.app",
            App::Ghostty => "Ghostty.app",
            App::Wezterm => "WezTerm.app",
            App::Warp => "Warp.app",
        };
        let mut out = vec![Path::new("/Applications").join(name)];
        if let Some(home) = home {
            out.push(home.join("Applications").join(name));
        }
        if self == App::Terminal {
            out.push(Path::new("/System/Applications/Utilities/Terminal.app").into());
            out.push(Path::new("/Applications/Utilities/Terminal.app").into());
        }
        out
    }
    fn installed(self, home: Option<&Path>) -> Option<PathBuf> {
        self.locations(home).into_iter().find(|path| path.is_dir())
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The known terminals installed on this Mac, in the order Lazybox prefers
/// them when the person has not chosen one.
#[tauri::command]
pub fn terminal_apps() -> Vec<String> {
    let home = home();
    App::ALL
        .into_iter()
        .filter(|app| app.installed(home.as_deref()).is_some())
        .map(|app| app.id().to_string())
        .collect()
}

/// What the page asks for: a folder and a terminal, never a command line.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRequest {
    /// The folder, absolute, inside one of the Runner's workspaces.
    path: String,
    /// A known terminal's id, or "custom" for the host's saved command line.
    app: String,
}

/// What `open_terminal` runs, decided before anything runs (tested below).
#[derive(Debug, PartialEq)]
struct Launch {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
}

const COMMAND_LIMIT: usize = 1000;

/// A custom command line as the host keeps it: one line, at most
/// COMMAND_LIMIT bytes; empty means none.
fn valid_command(command: &str) -> Result<String, String> {
    let command = command.trim();
    if command.len() > COMMAND_LIMIT || command.contains(['\0', '\n', '\r']) {
        return Err("Your terminal command must be one line of at most 1000 characters".into());
    }
    Ok(command.to_string())
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Saved {
    command: String,
}

/// The host's own terminal setting, in `folder` (the app's config folder);
/// the page has no way to write it but `set_terminal_command`.
fn saved_command(folder: &Path) -> String {
    fs::read(folder.join("terminal.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
        .and_then(|saved| valid_command(&saved.command).ok())
        .unwrap_or_default()
}

fn save_command(folder: &Path, command: &str) -> Result<(), String> {
    let error = |e: &dyn std::fmt::Display| format!("Could not save your terminal command: {e}");
    fs::create_dir_all(folder).map_err(|e| error(&e))?;
    let mut file = tempfile::NamedTempFile::new_in(folder).map_err(|e| error(&e))?;
    let bytes = serde_json::to_vec(&Saved {
        command: command.into(),
    })
    .map_err(|e| error(&e))?;
    file.write_all(&bytes).map_err(|e| error(&e))?;
    file.persist(folder.join("terminal.json"))
        .map_err(|e| error(&e))?;
    Ok(())
}

fn config_folder(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path().app_config_dir().map_err(|e| e.to_string())
}

/// A path quoted for `/bin/sh`.
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

fn launch(app: &str, command: &str, dir: &Path, runner: Option<&Path>) -> Result<Launch, String> {
    let env = runner
        .map(|path| format!("LAZYBOX_RUNNER={}", path.to_string_lossy()))
        .into_iter();
    let dir_text = dir.to_string_lossy().into_owned();
    if app == "custom" {
        // `command` is the host's saved setting, never the page's.
        let command = valid_command(command).unwrap_or_default();
        if command.is_empty() {
            return Err("Set your terminal command in Settings → Agents & workspaces: one line, with {dir} where the folder goes".into());
        }
        return Ok(Launch {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), command.replace("{dir}", &quote(dir))],
            cwd: dir.into(),
        });
    }
    let chosen = App::parse(app).ok_or("Choose a terminal in Settings → Agents & workspaces")?;
    if chosen == App::Wezterm {
        // WezTerm takes its folder on its own command line; its window inherits this environment.
        let bundle = chosen
            .installed(home().as_deref())
            .ok_or("WezTerm is not installed")?;
        return Ok(Launch {
            program: bundle.join("Contents/MacOS/wezterm"),
            args: vec!["start".into(), "--cwd".into(), dir_text],
            cwd: dir.into(),
        });
    }
    // `open -a App folder` opens a new window at the folder in every one of
    // these; `--env` reaches the app when this starts it.
    let mut args: Vec<String> = Vec::new();
    for value in env {
        args.push("--env".into());
        args.push(value);
    }
    args.extend(["-a".into(), chosen.bundle().into(), dir_text]);
    Ok(Launch {
        program: "/usr/bin/open".into(),
        args,
        cwd: dir.into(),
    })
}

/// The folder, if it is an existing directory inside one of `roots`.
fn inside(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Expected an absolute folder".into());
    }
    let dir = path
        .canonicalize()
        .map_err(|_| "That folder does not exist on this Mac".to_string())?;
    if !dir.is_dir() {
        return Err("Expected a folder".into());
    }
    let within = roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| dir.starts_with(&root));
    if within {
        Ok(dir)
    } else {
        Err(
            "Lazybox opens a terminal only in a workspace's folder or a repository's home checkout"
                .into(),
        )
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Member {
    path: String,
}
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Collection {
    root: String,
    members: Vec<Member>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Collections {
    collections: Vec<Collection>,
}

/// Every workspace folder (and repository worktree) the Runner lists now.
async fn workspace_roots(url: &str, token: &str) -> Result<Vec<PathBuf>, String> {
    use tauri_plugin_http::reqwest;
    let response = reqwest::Client::new()
        .post(format!("{url}{}ListCollections", crate::RPC_PATH))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .body("{}")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|_| "The Runner did not answer; try again in a moment".to_string())?;
    if !response.status().is_success() {
        return Err("The Runner did not list its workspaces".into());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "The Runner's answer was cut short".to_string())?;
    let list: Collections = serde_json::from_slice(&bytes)
        .map_err(|_| "The Runner's answer was not understood".to_string())?;
    Ok(roots_of(list))
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Repository {
    home_path: String,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Repositories {
    repositories: Vec<Repository>,
}

/// Every repository's home checkout the Runner records now (the person's own
/// clone outside the workspaces; WORKSPACES.md, "Home checkouts"). A Runner
/// that does not answer adds none: the workspaces still open.
async fn home_roots(url: &str, token: &str) -> Vec<PathBuf> {
    use tauri_plugin_http::reqwest;
    let Ok(response) = reqwest::Client::new()
        .post(format!("{url}{}ListRepositories", crate::RPC_PATH))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .body("{}")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    else {
        return Vec::new();
    };
    if !response.status().is_success() {
        return Vec::new();
    }
    match response.bytes().await {
        Ok(bytes) => homes_of(serde_json::from_slice(&bytes).unwrap_or_default()),
        Err(_) => Vec::new(),
    }
}

fn homes_of(list: Repositories) -> Vec<PathBuf> {
    list.repositories
        .into_iter()
        .map(|r| r.home_path)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn roots_of(list: Collections) -> Vec<PathBuf> {
    list.collections
        .into_iter()
        .flat_map(|c| std::iter::once(c.root).chain(c.members.into_iter().map(|m| m.path)))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The app's own windows: the main window, workspace windows and chat windows.
fn owner_window(label: &str) -> bool {
    label == "main" || label.starts_with("workspace-") || label.starts_with("chat-")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Opened {
    /// The folder the terminal opened at.
    path: String,
}

#[tauri::command]
pub async fn open_terminal(
    window: Webview,
    host: State<'_, Host>,
    request: OpenRequest,
) -> Result<Opened, String> {
    if !owner_window(window.label()) {
        return Err("Only a Lazybox workspace or chat window can open a terminal".into());
    }
    let (url, token) = {
        let connection = host.connection.lock().map_err(|e| e.to_string())?;
        (connection.url.clone(), connection.token.clone())
    };
    let mut roots = workspace_roots(&url, &token).await?;
    roots.extend(home_roots(&url, &token).await);
    let dir = inside(Path::new(&request.path), &roots)?;
    let command = if request.app == "custom" {
        saved_command(&config_folder(window.app_handle())?)
    } else {
        String::new()
    };
    let runner = host.reload_endpoint.clone();
    let plan = launch(&request.app, &command, &dir, runner.as_deref())?;
    // `open` returns at once; a custom command or WezTerm may keep running, so
    // only a quick failure is reported, without waiting for the terminal.
    tauri::async_runtime::spawn_blocking(move || start(&plan, runner.as_deref(), QUICK_FAILURE))
        .await
        .map_err(|_| "Could not start the terminal".to_string())??;
    Ok(Opened {
        path: dir.to_string_lossy().into_owned(),
    })
}

/// How long a terminal that fails at once is waited for, to say why.
const QUICK_FAILURE: Duration = Duration::from_secs(2);

/// Starts `plan` and reports a failure within `watch`, with the first line
/// the program wrote on stderr. A program still running after `watch` (a
/// custom command, WezTerm) is left to run and reaped on its own thread: the
/// returned handle. Its stderr is a pipe drained to its end by a thread of
/// its own (`children::drain`), which keeps its first 4 KiB for the failure
/// message and discards the rest: the terminal can write for as long as it
/// runs, never meets a full pipe or a closed one (SIGPIPE), and nothing it
/// writes accumulates anywhere.
fn start(
    plan: &Launch,
    runner: Option<&Path>,
    watch: Duration,
) -> Result<Option<JoinHandle<io::Result<ExitStatus>>>, String> {
    let fail = |_| "Could not start the terminal".to_string();
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(endpoint) = runner {
        command.env("LAZYBOX_RUNNER", endpoint);
    }
    let mut child = command.spawn().map_err(fail)?;
    let errors = match child.stderr.take().map(crate::children::drain) {
        Some(Ok(errors)) => errors,
        _ => {
            let _ = child.kill();
            let _ = crate::children::reap(child);
            return Err("Could not start the terminal".into());
        }
    };
    let deadline = Instant::now() + watch;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                // Its stderr ends with it, unless something it started still
                // holds it: then the message goes without the detail.
                let ended = Instant::now() + Duration::from_secs(1);
                while !errors.is_finished() && Instant::now() < ended {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let text = if errors.is_finished() {
                    String::from_utf8_lossy(&errors.join().unwrap_or_default()).into_owned()
                } else {
                    String::new()
                };
                let detail = text.lines().next().unwrap_or("").trim().to_string();
                return Err(if detail.is_empty() {
                    "The terminal did not open".to_string()
                } else {
                    format!("The terminal did not open: {detail}")
                });
            }
            Ok(Some(_)) => return Ok(None),
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
            }
            Err(_) => break,
        }
    }
    crate::children::reap(child).map(Some).map_err(fail)
}

/// The custom command line "Open in Terminal" runs, as the host keeps it.
#[tauri::command]
pub fn terminal_command(window: Webview) -> Result<String, String> {
    if !owner_window(window.label()) {
        return Err("Only a Lazybox workspace or chat window reads the terminal setting".into());
    }
    Ok(saved_command(&config_folder(window.app_handle())?))
}

/// Saves the custom command line "Open in Terminal" runs, once the person
/// allowed that exact line in a native confirmation, which the page cannot
/// answer. Clearing it, or saving the line already kept, asks nothing.
/// Returns what is kept.
#[tauri::command]
pub async fn set_terminal_command(window: Webview, command: String) -> Result<String, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    if !owner_window(window.label()) {
        return Err("Only a Lazybox workspace or chat window changes the terminal setting".into());
    }
    let command = valid_command(&command)?;
    let folder = config_folder(window.app_handle())?;
    if command.is_empty() || command == saved_command(&folder) {
        save_command(&folder, &command)?;
        return Ok(command);
    }
    let parent = window.window();
    let dialog = window
        .app_handle()
        .dialog()
        .message(format!(
            "“Open in Terminal” will run this command line with /bin/sh, in the folder you open:\n\n{command}\n\nAllow it only if you typed it yourself."
        ))
        .title("Allow this terminal command?")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Allow".into(),
            "Cancel".into(),
        ))
        .parent(&parent);
    let allowed = tauri::async_runtime::spawn_blocking(move || dialog.blocking_show())
        .await
        .map_err(|_| "Could not ask about the terminal command".to_string())?;
    if !allowed {
        return Err("Your terminal command was not saved".into());
    }
    save_command(&folder, &command)?;
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_apps_own_windows_open_a_terminal() {
        for label in ["main", "workspace-abc", "chat-1"] {
            assert!(owner_window(label), "{label}");
        }
        for label in [
            "",
            "viewer-1",
            "web-1",
            "console-1",
            "artifact-1",
            "setup",
            "popout-1",
        ] {
            assert!(!owner_window(label), "{label}");
        }
    }

    #[test]
    fn a_folder_must_be_inside_a_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspaces/alpha");
        std::fs::create_dir_all(workspace.join("api/src")).unwrap();
        std::fs::write(workspace.join("api/README.md"), "x").unwrap();
        let outside = root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let roots = vec![workspace.clone()];
        assert!(inside(&workspace.join("api/src"), &roots).is_ok());
        assert!(inside(&workspace, &roots).is_ok());
        assert!(inside(&outside, &roots).is_err(), "outside every workspace");
        assert!(
            inside(&workspace.join("api/README.md"), &roots).is_err(),
            "a file"
        );
        assert!(
            inside(&workspace.join("missing"), &roots).is_err(),
            "missing"
        );
        assert!(inside(Path::new("api/src"), &roots).is_err(), "relative");
        // `..` cannot climb out: the folder is compared once resolved.
        assert!(inside(&workspace.join("api/../../../elsewhere"), &roots).is_err());
        assert!(inside(&workspace, &[]).is_err(), "no workspaces");
    }

    #[test]
    fn known_terminals_open_with_open_a_and_the_runner() {
        let dir = Path::new("/w/alpha");
        let plan = launch("ghostty", "", dir, Some(Path::new("/state/endpoint.json"))).unwrap();
        assert_eq!(plan.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(
            plan.args,
            [
                "--env",
                "LAZYBOX_RUNNER=/state/endpoint.json",
                "-a",
                "Ghostty",
                "/w/alpha"
            ]
        );
        assert_eq!(plan.cwd, dir);
        let plan = launch("terminal", "", dir, None).unwrap();
        assert_eq!(plan.args, ["-a", "Terminal", "/w/alpha"]);
        assert!(launch("xterm", "", dir, None).is_err());
        assert!(launch("", "", dir, None).is_err());
    }

    #[test]
    fn a_custom_command_gets_the_folder_quoted() {
        let dir = Path::new("/w/it's here");
        let plan = launch("custom", "alacritty --working-directory {dir}", dir, None).unwrap();
        assert_eq!(plan.program, PathBuf::from("/bin/sh"));
        assert_eq!(
            plan.args,
            ["-c", r"alacritty --working-directory '/w/it'\''s here'"]
        );
        assert_eq!(plan.cwd, dir);
        for bad in ["", "   ", "a\nb", "a\0b", &"x".repeat(COMMAND_LIMIT + 1)] {
            assert!(launch("custom", bad, dir, None).is_err(), "{bad:?}");
        }
    }

    /// The page names a folder and a terminal; a command line from it is refused
    /// outright, so a script in a window cannot have the host run one.
    #[test]
    fn the_page_cannot_send_a_command_line() {
        let request: OpenRequest =
            serde_json::from_str(r#"{"path":"/w/alpha","app":"custom"}"#).expect("request");
        assert_eq!(
            (request.path.as_str(), request.app.as_str()),
            ("/w/alpha", "custom")
        );
        assert!(
            serde_json::from_str::<OpenRequest>(
                r#"{"path":"/w/alpha","app":"custom","command":"touch /tmp/owned"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn the_custom_command_is_the_hosts_own_setting() {
        let folder = tempfile::tempdir().unwrap();
        let config = folder.path().join("config");
        assert_eq!(saved_command(&config), "", "none saved yet");
        save_command(&config, "alacritty --working-directory {dir}").unwrap();
        assert_eq!(
            saved_command(&config),
            "alacritty --working-directory {dir}"
        );
        save_command(&config, "").unwrap();
        assert_eq!(saved_command(&config), "", "cleared");
        // A file edited by hand into something the host would not save is ignored.
        for bad in [r#"{"command":"a\nb"}"#, "not json", r#"{"command":7}"#] {
            fs::write(config.join("terminal.json"), bad).unwrap();
            assert_eq!(saved_command(&config), "", "{bad}");
        }
        assert_eq!(valid_command("  kitty {dir} ").unwrap(), "kitty {dir}");
        assert_eq!(valid_command("").unwrap(), "");
        assert!(valid_command("a\rb").is_err());
        assert!(valid_command(&"x".repeat(COMMAND_LIMIT + 1)).is_err());
    }

    fn shell(script: &str, dir: &Path) -> Launch {
        Launch {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: dir.into(),
        }
    }

    #[test]
    fn a_terminal_that_fails_at_once_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let error = start(
            &shell("echo 'no such profile' >&2; exit 3", dir.path()),
            None,
            Duration::from_secs(10),
        )
        .expect_err("failed");
        assert_eq!(error, "The terminal did not open: no such profile");
        assert!(
            start(&shell("exit 0", dir.path()), None, Duration::from_secs(10))
                .expect("opened")
                .is_none(),
            "a terminal that handed off and ended is done"
        );
    }

    /// A terminal still running after the quick-failure window keeps writing
    /// to stderr: it neither dies of SIGPIPE (its pipe used to be closed under
    /// it) nor stays a zombie when it ends (it used to be dropped unwaited).
    #[test]
    fn a_terminal_that_keeps_running_is_reaped_and_can_still_write() {
        let dir = tempfile::tempdir().unwrap();
        let go = dir.path().join("go");
        // More than a pipe holds (64 KiB), so a pipe nobody reads would block it too.
        let script = "while [ ! -e go ]; do sleep 0.01; done; i=0; while [ $i -lt 5000 ]; do echo 'a line of terminal chatter' >&2; i=$((i+1)); done";
        let running = start(&shell(script, dir.path()), None, Duration::from_millis(50))
            .expect("started")
            .expect("still running after the window");
        fs::write(&go, "").unwrap();
        // Waited for on its own thread; a blocked terminal shows as no answer.
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(running.join().expect("join"));
        });
        let status = receive
            .recv_timeout(Duration::from_secs(60))
            .expect("the terminal never ended: blocked on its stderr")
            .expect("wait");
        assert!(status.success(), "{status:?}");
    }

    #[test]
    fn repository_listing_reads_home_checkouts() {
        let list: Repositories = serde_json::from_str(
            r#"{"repositories":[{"id":"a","path":"/state/a.git","homePath":"/u/lazybox/repos/acme/api"},{"id":"b","path":"/src/b"}]}"#,
        )
        .unwrap();
        assert_eq!(homes_of(list), [PathBuf::from("/u/lazybox/repos/acme/api")]);
    }

    #[test]
    fn workspace_listing_reads_roots_and_worktrees() {
        let list: Collections = serde_json::from_str(
            r#"{"collections":[{"id":"a","root":"/w/a","members":[{"path":"/w/a/api","ready":true},{"spec":{}}]},{"id":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(
            roots_of(list),
            [PathBuf::from("/w/a"), PathBuf::from("/w/a/api")]
        );
    }
}
