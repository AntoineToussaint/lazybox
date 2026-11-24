//! Runners on other computers (REMOTE_RUNNERS.md, P0): the app keeps one link
//! agent (`lazybox link connect <name>`, the Go sidecar the CLI shares) per
//! computer the person added, and Settings → Runners → Add a computer runs
//! `lazybox remote add <host> --progress json` and shows its steps.
//!
//! Nothing here touches a key or a credential: the CLI enrols this computer
//! and keeps its keys; each link agent serves its Runner on a loopback origin
//! with a per-start token in a private endpoint file, which `runners.rs` lists
//! like any local Runner's. Agents run on the other computer and outlive these
//! links: quitting the app ends the links, not the work.
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    env,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager};

/// The link agents this app started, by remote name.
#[derive(Default)]
pub struct Links {
    children: Mutex<HashMap<String, (Child, Instant)>>,
}

/// How long after starting a link agent the app waits before starting it
/// again if it exited (another `lazybox link connect` holds it, ssh failing).
const RESPAWN: Duration = Duration::from_secs(10);

/// The `lazybox` this app runs: its release's own, the one named by
/// `LAZYBOX_V2_CLI_DIR` (Dev and Working's build), or the one on PATH or in
/// the places `lazybox link` puts it.
pub fn cli_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    if let Some(release) = crate::release::Release::detect(app) {
        return Some(release.cli());
    }
    if let Some(dir) = env::var_os("LAZYBOX_V2_CLI_DIR") {
        let path = PathBuf::from(dir).join("lazybox");
        if path.is_file() {
            return Some(path);
        }
    }
    let mut dirs: Vec<PathBuf> = env::var_os("PATH")
        .map(|p| env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.into_iter()
        .map(|dir| dir.join("lazybox"))
        .find(|path| path.is_file())
}

/// Starts a link agent for each remote that has none running. `running`
/// says whether a remote's endpoint already answers (a terminal's `lazybox
/// link connect`, or ours).
pub fn ensure(app: &tauri::AppHandle, remotes: &[(String, bool)]) -> Option<String> {
    let links = app.state::<Links>();
    let Ok(mut children) = links.children.lock() else {
        return None;
    };
    // Forget the remotes the person removed.
    children.retain(|name, (child, _)| {
        let keep = remotes.iter().any(|(n, _)| n == name);
        if !keep {
            let _ = child.kill();
            let _ = child.wait();
        }
        keep
    });
    let cli = cli_path(app);
    for (name, running) in remotes {
        if *running {
            continue;
        }
        if let Some((child, started)) = children.get_mut(name) {
            let exited = !matches!(child.try_wait(), Ok(None));
            if !exited || started.elapsed() < RESPAWN {
                continue;
            }
        }
        let Some(cli) = cli.as_ref() else {
            return Some("Lazybox cannot find its lazybox command to connect to other computers. Install it with `lazybox link`.".into());
        };
        match Command::new(cli)
            .args(["link", "connect", name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                children.insert(name.clone(), (child, Instant::now()));
            }
            Err(error) => return Some(format!("Could not start the link to {name}: {error}")),
        }
    }
    None
}

/// Ends every link agent this app started (the app is quitting). The agents
/// on the other computers keep running.
pub fn stop_all(app: &tauri::AppHandle) {
    if let Some(links) = app.try_state::<Links>()
        && let Ok(mut children) = links.children.lock()
    {
        for (child, _) in children.values_mut() {
            terminate(child);
        }
        children.clear();
    }
}

/// SIGTERM, so the agent removes its endpoint file; SIGKILL if it lingers.
fn terminate(child: &mut Child) {
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    for _ in 0..20 {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_cli(app: &tauri::AppHandle, args: &[&str]) -> Result<Value, String> {
    let cli = cli_path(app).ok_or("Lazybox cannot find its lazybox command")?;
    crate::release::run_json(&cli, args)
}

/// The Host entries of the person's ~/.ssh/config, for Add a computer.
#[tauri::command]
pub fn remote_hosts(window: tauri::Webview) -> Result<Value, String> {
    if window.label() != "main" {
        return Err("Only the main window adds computers".into());
    }
    let value = run_cli(window.app_handle(), &["remote", "hosts"])?;
    Ok(value.get("hosts").cloned().unwrap_or(Value::Array(vec![])))
}

#[derive(Clone, Serialize)]
struct AddProgress {
    id: String,
    event: Value,
}

/// Adds a computer: `lazybox remote add <host> --progress json`, each step
/// sent to the window as `remote-add-progress` with the caller's `id`. Ends
/// with the remote's record, or the error in the CLI's words.
#[tauri::command]
pub async fn remote_add(
    window: tauri::Webview,
    id: String,
    host: String,
    name: Option<String>,
) -> Result<Value, String> {
    if window.label() != "main" {
        return Err("Only the main window adds computers".into());
    }
    let valid = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.len() < 254
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._@%:[]+-".contains(&b))
    };
    if !valid(&host) || name.as_deref().is_some_and(|n| !valid(n)) {
        return Err("That is not an SSH host name".into());
    }
    let cli = cli_path(window.app_handle()).ok_or("Lazybox cannot find its lazybox command")?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut command = Command::new(cli);
        command.args(["remote", "add", &host, "--progress", "json"]);
        if let Some(name) = name.as_deref() {
            command.args(["--name", name]);
        }
        run_add(command, |event| {
            let _ = window.emit(
                "remote-add-progress",
                AddProgress {
                    id: id.clone(),
                    event,
                },
            );
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Runs `lazybox remote add … --progress json`: each step to `step`, then the
/// remote's record, or the error in the CLI's words. Stderr is read on a
/// thread of its own while stdout is read here, so a CLI that writes more
/// than a pipe holds on stderr (an SSH banner, a long diagnostic) is never
/// blocked on it while this waits for its stdout to end.
fn run_add(mut command: Command, mut step: impl FnMut(Value)) -> Result<Value, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not run lazybox: {e}"))?;
    let stdout = child.stdout.take().ok_or("no output")?;
    let mut stderr = child.stderr.take().ok_or("no output")?;
    let errors = std::thread::Builder::new()
        .name("lazybox-remote-add-stderr".into())
        .spawn(move || {
            let mut text = Vec::new();
            let _ = stderr.read_to_end(&mut text);
            String::from_utf8_lossy(&text).into_owned()
        });
    let errors = match errors {
        Ok(errors) => errors,
        Err(e) => {
            terminate(&mut child);
            return Err(format!("Could not run lazybox: {e}"));
        }
    };
    let mut result: Result<Value, String> =
        Err("lazybox remote add ended without saying how".into());
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event.get("step").is_some() {
            step(event);
        } else if let Some(error) = event.get("error").and_then(Value::as_str) {
            result = Err(error.to_string());
        } else if event.get("done").is_some() {
            result = Ok(event.get("remote").cloned().unwrap_or(Value::Null));
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let stderr = errors.join().unwrap_or_default();
    if result.is_err() && !status.success() {
        let message = stderr.trim().trim_start_matches("lazybox:").trim();
        if !message.is_empty()
            && matches!(result, Err(ref e) if e.starts_with("lazybox remote add ended"))
        {
            result = Err(message.to_string());
        }
    }
    result
}

/// Forgets a computer here and revokes this computer's key on it.
#[tauri::command]
pub fn remote_remove(window: tauri::Webview, name: String) -> Result<Value, String> {
    if window.label() != "main" {
        return Err("Only the main window removes computers".into());
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("No computer has that name".into());
    }
    let app = window.app_handle();
    if let Some(links) = app.try_state::<Links>()
        && let Ok(mut children) = links.children.lock()
        && let Some((mut child, _)) = children.remove(&name)
    {
        terminate(&mut child);
    }
    run_cli(app, &["remote", "rm", &name])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Runs `script` as the CLI would be run, on a thread: a deadlock shows as
    /// no answer, reported after a minute instead of hanging the suite.
    fn add(script: &str) -> (Result<Value, String>, Vec<Value>) {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let mut steps = Vec::new();
            let result = run_add(command, |event| steps.push(event));
            let _ = send.send((result, steps));
        });
        receive
            .recv_timeout(Duration::from_secs(60))
            .expect("lazybox remote add never finished: blocked on its stderr")
    }

    /// More than a pipe holds on stderr before the last line on stdout: the
    /// CLI used to block writing it while the app waited for stdout to end.
    #[test]
    fn a_cli_that_writes_much_on_stderr_still_finishes() {
        let (result, steps) = add(
            r#"head -c 300000 /dev/zero | tr '\0' 'x' >&2; echo '{"step":"enrol"}'; echo '{"done":true,"remote":{"name":"studio"}}'"#,
        );
        assert_eq!(result.expect("added")["name"], "studio");
        assert_eq!(steps, [serde_json::json!({"step":"enrol"})]);
    }

    #[test]
    fn a_cli_that_fails_without_a_report_says_why_in_its_words() {
        let (result, _) = add("echo 'lazybox: studio did not answer' >&2; exit 1");
        assert_eq!(result.expect_err("failed"), "studio did not answer");
        let (result, _) = add(r#"echo '{"error":"the key was refused"}'; exit 1"#);
        assert_eq!(result.expect_err("failed"), "the key was refused");
    }
}
