//! An installed release (INSTALL.md): the app ships its runtime in
//! `Contents/Resources/runtime`, and its Runner runs as a launchd service the
//! app sets up once, keeps on its version, and leaves running when it quits.
//!
//! Every service operation goes through the bundled `lazybox` CLI
//! (`lazybox service …`, `lazybox link`, `lazybox doctor`), so the app, the
//! CLI and a person in a terminal share one implementation and one set of
//! paths. Dev and Working never get here: they launch the app with
//! `--endpoint` (or `LAZYBOX_V2_ENDPOINT`), and that always wins.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// The runtime inside this app bundle, when it is an installed release.
#[derive(Clone, Debug)]
pub struct Release {
    pub runtime: PathBuf,
    pub version: String,
}

impl Release {
    pub fn detect(app: &tauri::AppHandle) -> Option<Self> {
        Self::at(app.path().resource_dir().ok()?.join("runtime"))
    }

    pub fn at(runtime: PathBuf) -> Option<Self> {
        let version = fs::read_to_string(runtime.join("VERSION")).ok()?;
        let version = version.trim().to_string();
        (!version.is_empty() && runtime.join("bin").join("lazybox").is_file())
            .then_some(Self { runtime, version })
    }

    pub fn cli(&self) -> PathBuf {
        self.runtime.join("bin").join("lazybox")
    }

    /// What this release was built from, when `make install` built it from
    /// a clone (runtime/SOURCE.json, scripts/release/build.mjs --from-source).
    /// Such a build has no updater: it updates with `make update`.
    pub fn source(&self) -> Option<SourceBuild> {
        let text = fs::read_to_string(self.runtime.join("SOURCE.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Runs `lazybox <args> --json` and reads its report. A verb that fails
    /// still prints its JSON when it has one (status of a stopped service);
    /// otherwise its message is the error.
    pub fn json(&self, args: &[&str]) -> Result<Value, String> {
        run_json(&self.cli(), args)
    }

    pub fn status(&self) -> Result<ServiceStatus, String> {
        serde_json::from_value(self.json(&["service", "status"])?).map_err(|e| e.to_string())
    }
}

pub fn run_json(cli: &Path, args: &[&str]) -> Result<Value, String> {
    let output = Command::new(cli)
        .args(args)
        .arg("--json")
        .output()
        .map_err(|e| format!("Could not run {}: {e}", cli.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(value) = serde_json::from_str::<Value>(stdout.trim()) {
        return Ok(value);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr
        .trim()
        .trim_start_matches("lazybox:")
        .trim()
        .to_string();
    Err(if message.is_empty() {
        format!("lazybox {} failed ({})", args.join(" "), output.status)
    } else {
        message
    })
}

/// runtime/SOURCE.json: the commit and the checkout a build from source came from.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceBuild {
    pub commit: String,
    pub dirty: bool,
    pub checkout: String,
}

/// `lazybox service status --json`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceStatus {
    pub installed: bool,
    pub loaded: bool,
    pub running: bool,
    pub reachable: bool,
    pub version: String,
    pub runner_version: String,
    pub release_version: String,
    pub endpoint: String,
    pub log: String,
    pub problem: String,
    pub unstable: String,
}

/// What launching the app does with the service it finds.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// The Runner answers on this release: connect.
    Connect,
    /// The app was updated (or moved): put the service on this release first.
    /// Agents keep running; the new Runner re-attaches them.
    Update,
    /// Installed but not answering: start it, then connect.
    Start,
    /// Nothing installed yet, or nothing works: ask the person.
    Ask,
}

pub fn plan(status: &ServiceStatus, release: &Release) -> Plan {
    if !status.installed {
        return Plan::Ask;
    }
    if status.version != release.version {
        return Plan::Update;
    }
    if status.reachable && status.runner_version == release.version {
        return Plan::Connect;
    }
    if status.reachable {
        // Answering, but another version than the plist says: reinstall.
        return Plan::Update;
    }
    Plan::Start
}

/// Brings the service to this release without asking, when it was set up
/// before. Returns the endpoint file to connect to, or None when the person
/// has to decide (first run, or a service that would not start).
pub fn prepare(release: &Release) -> Option<PathBuf> {
    let status = release.status().ok()?;
    let ready = match plan(&status, release) {
        Plan::Connect => true,
        Plan::Update => release.json(&["service", "install"]).is_ok(),
        Plan::Start => release.json(&["service", "start"]).is_ok(),
        Plan::Ask => false,
    };
    ready.then(|| PathBuf::from(status.endpoint))
}

pub const SETUP_LABEL: &str = "setup";

/// The first-run window: what the Runner service is, one button to set it up.
pub fn open_setup(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(SETUP_LABEL) {
        window.show().map_err(|e| e.to_string())?;
        return window.set_focus().map_err(|e| e.to_string());
    }
    let name = app
        .config()
        .product_name
        .clone()
        .unwrap_or_else(|| "Lazybox".into());
    WebviewWindowBuilder::new(app, SETUP_LABEL, WebviewUrl::App("index.html".into()))
        .title(format!("Welcome to {name}"))
        .inner_size(600.0, 640.0)
        .min_inner_size(480.0, 520.0)
        .resizable(true)
        .center()
        .focused(true)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupState {
    version: String,
    app: String,
    status: ServiceStatus,
}

fn release_of(app: &tauri::AppHandle) -> Result<Release, String> {
    Release::detect(app).ok_or_else(|| "This build of Lazybox has no bundled Runner".to_string())
}

#[tauri::command]
pub fn setup_state(app: tauri::AppHandle) -> Result<SetupState, String> {
    let release = release_of(&app)?;
    let status = release.status()?;
    let bundle = release
        .runtime
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(SetupState {
        version: release.version,
        app: bundle,
        status,
    })
}

/// Installs (or repairs) the service, and optionally links the CLI. Each
/// part reports on its own, so a CLI link that fails does not hide that the
/// Runner is running.
#[tauri::command]
pub async fn setup_install(app: tauri::AppHandle, link_cli: bool) -> Result<Value, String> {
    let release = release_of(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let service = release.json(&["service", "install"])?;
        let link = if link_cli {
            Some(match release.json(&["link"]) {
                Ok(value) => value,
                Err(error) => serde_json::json!({ "error": error }),
            })
        } else {
            None
        };
        Ok(serde_json::json!({ "service": service, "link": link }))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn setup_start(app: tauri::AppHandle) -> Result<Value, String> {
    let release = release_of(&app)?;
    tauri::async_runtime::spawn_blocking(move || release.json(&["service", "start"]))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn run_doctor(app: tauri::AppHandle) -> Result<Value, String> {
    let release = release_of(&app)?;
    tauri::async_runtime::spawn_blocking(move || release.json(&["doctor"]))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn link_cli(app: tauri::AppHandle) -> Result<Value, String> {
    let release = release_of(&app)?;
    tauri::async_runtime::spawn_blocking(move || release.json(&["link"]))
        .await
        .map_err(|e| e.to_string())?
}

/// Removes an older Lazybox installed by Homebrew that comes first on the
/// person's PATH: `lazybox link --remove-older --yes`, which runs
/// `brew uninstall lazybox` and reports its output. The setup window calls it
/// only after the person confirmed the exact command it showed; the CLI finds
/// what to remove itself and refuses anything but Homebrew's formula.
#[tauri::command]
pub async fn remove_older_lazybox(app: tauri::AppHandle) -> Result<Value, String> {
    let release = release_of(&app)?;
    tauri::async_runtime::spawn_blocking(move || release.json(&["link", "--remove-older", "--yes"]))
        .await
        .map_err(|e| e.to_string())?
}

/// Shows the Runner's log in Console.
#[tauri::command]
pub fn show_runner_log(app: tauri::AppHandle) -> Result<(), String> {
    let status = release_of(&app)?.status()?;
    if status.log.is_empty() || !Path::new(&status.log).exists() {
        return Err("The Runner has not written a log yet".into());
    }
    Command::new("/usr/bin/open")
        .args(["-a", "Console", &status.log])
        .status()
        .map_err(|e| e.to_string())
        .and_then(|s| {
            s.success()
                .then_some(())
                .ok_or_else(|| "Console could not open the log".into())
        })
}

/// The service is running: connect the app to it and close the setup window.
#[tauri::command]
pub fn setup_finish(app: tauri::AppHandle) -> Result<(), String> {
    if app.try_state::<crate::host::Host>().is_none() {
        let release = release_of(&app)?;
        let status = release.status()?;
        if !status.reachable {
            return Err(if status.problem.is_empty() {
                "The Runner is not answering yet".into()
            } else {
                status.problem
            });
        }
        crate::host::start(&app, Some(PathBuf::from(status.endpoint)))
            .map_err(|e| e.to_string())?;
    }
    if let Some(window) = app.get_webview_window(SETUP_LABEL) {
        window.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// About this app: one version for the app, its Runner and its CLI.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    version: String,
    /// "release" for an installed release, otherwise the app's identity (Dev, Working).
    channel: String,
    updater: bool,
    cli: String,
    /// Set for a build from source (`make install`), which updates with `make update`.
    source: Option<SourceBuild>,
}

#[tauri::command]
pub fn app_info(app: tauri::AppHandle) -> AppInfo {
    let release = Release::detect(&app);
    AppInfo {
        version: app.package_info().version.to_string(),
        channel: if release.is_some() {
            "release".into()
        } else {
            app.config().identifier.clone()
        },
        updater: crate::updates::configured(app.config()),
        source: release.as_ref().and_then(Release::source),
        cli: release
            .map(|r| r.cli().display().to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str) -> Release {
        Release {
            runtime: PathBuf::from("/Applications/Lazybox.app/Contents/Resources/runtime"),
            version: version.into(),
        }
    }

    fn status(installed: bool, plist: &str, reachable: bool, runner: &str) -> ServiceStatus {
        ServiceStatus {
            installed,
            version: plist.into(),
            reachable,
            runner_version: runner.into(),
            ..Default::default()
        }
    }

    #[test]
    fn launch_plan_follows_the_installed_service() {
        let r = release("0.2.0");
        assert_eq!(plan(&status(false, "", false, ""), &r), Plan::Ask);
        assert_eq!(
            plan(&status(true, "0.2.0", true, "0.2.0"), &r),
            Plan::Connect
        );
        // The app was updated: the service still runs the old Runner.
        assert_eq!(
            plan(&status(true, "0.1.0", true, "0.1.0"), &r),
            Plan::Update
        );
        // The plist was updated but an old Runner still answers.
        assert_eq!(
            plan(&status(true, "0.2.0", true, "0.1.0"), &r),
            Plan::Update
        );
        assert_eq!(plan(&status(true, "0.2.0", false, ""), &r), Plan::Start);
    }

    #[test]
    fn a_runtime_without_a_version_is_not_a_release() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(Release::at(dir.path().to_path_buf()).is_none());
        fs::create_dir_all(dir.path().join("bin")).expect("bin");
        fs::write(dir.path().join("bin").join("lazybox"), "").expect("cli");
        assert!(Release::at(dir.path().to_path_buf()).is_none());
        fs::write(dir.path().join("VERSION"), "1.2.3\n").expect("version");
        let r = Release::at(dir.path().to_path_buf()).expect("release");
        assert_eq!(r.version, "1.2.3");
        // A downloaded release says nothing about a checkout.
        assert_eq!(r.source(), None);
    }

    #[test]
    fn a_build_from_source_names_its_commit_and_checkout() {
        let dir = tempfile::tempdir().expect("dir");
        fs::create_dir_all(dir.path().join("bin")).expect("bin");
        fs::write(dir.path().join("bin").join("lazybox"), "").expect("cli");
        fs::write(dir.path().join("VERSION"), "1.2.3\n").expect("version");
        fs::write(
            dir.path().join("SOURCE.json"),
            r#"{"commit":"abc123","dirty":true,"checkout":"/Users/me/lazybox","builtAt":"2026-11-17T10:00:00Z"}"#,
        )
        .expect("source");
        let r = Release::at(dir.path().to_path_buf()).expect("release");
        assert_eq!(
            r.source(),
            Some(SourceBuild {
                commit: "abc123".into(),
                dirty: true,
                checkout: "/Users/me/lazybox".into()
            })
        );
    }

    #[test]
    fn the_cli_report_is_read_even_from_a_failing_verb() {
        let dir = tempfile::tempdir().expect("dir");
        let cli = dir.path().join("lazybox");
        fs::write(
            &cli,
            "#!/bin/sh\nif [ \"$1\" = status ]; then echo '{\"installed\":true}'; exit 6; fi\necho 'lazybox: not this one' >&2; exit 1\n",
        )
        .expect("cli");
        std::process::Command::new("chmod")
            .args(["+x", cli.to_str().expect("path")])
            .status()
            .expect("chmod");
        assert_eq!(
            run_json(&cli, &["status"]).expect("report")["installed"],
            true
        );
        assert_eq!(
            run_json(&cli, &["other"]).expect_err("error"),
            "not this one"
        );
    }
}
