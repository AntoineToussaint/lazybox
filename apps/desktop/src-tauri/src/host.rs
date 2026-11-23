use crate::{
    Bootstrap, RPC_PATH,
    layout::{Layout, Repository, Surface, Target},
    placement::{self, Display, Geometry, Rect},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Write,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tauri::{
    Emitter, Manager, State, Webview, WebviewUrl, WindowEvent,
    ipc::CapabilityBuilder,
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
};

pub struct Host {
    pub connection: Mutex<Bootstrap>,
    pub reload_endpoint: Option<PathBuf>,
    pub authority: String,
    pub repository: Repository,
    pub views: Mutex<crate::viewstate::Repository>,
    pub layout: Mutex<Layout>,
    pub quitting: AtomicBool,
    pub notice: Mutex<String>,
    /// Armed by Restart Runner: the bearer the stopping Runner held and how
    /// long to wait for its successor. While armed, the next Runner the same
    /// private locator names is adopted on a newly allocated origin even when
    /// it reports no identity; an identified Runner is followed regardless.
    pub restart: Mutex<Option<(String, Instant)>>,
}

/// The app's own webviews: its windows' pages. A capability names these and
/// nothing else, so a Web panel's viewer (a child webview inside one of these
/// windows, labelled `viewer-…`) holds no capability, whatever window it is in.
pub const APP_WEBVIEWS: [&str; 7] = [
    "main",
    "workspace-*",
    "console-*",
    "chat-*",
    "artifact-*",
    "mission-*",
    "web-*",
];

/// How long a requested restart may take before the window stops following
/// the locator to a new origin (Dev may rebuild the Runner first).
const RESTART_FOLLOW: Duration = Duration::from_secs(300);

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Locator {
    endpoint: PathBuf,
    project: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowBootstrap {
    #[serde(flatten)]
    connection: Bootstrap,
    target: Target,
    view_key: String,
    view_scope: String,
    view_root: String,
    application_name: String,
    kind: &'static str,
    fixed_workspace_id: String,
    notice: String,
    view_state: std::collections::BTreeMap<String, String>,
}

#[tauri::command]
pub fn save_view_state(host: State<'_, Host>, key: String, value: String) -> Result<(), String> {
    host.views
        .lock()
        .map_err(|e| e.to_string())?
        .save(&key, &value)
}

pub fn authority(connection: &Bootstrap) -> String {
    let mut hash = Sha256::new();
    if connection.runner_id.is_empty() {
        hash.update(connection.url.as_bytes());
        hash.update([0]);
        hash.update(connection.token.as_bytes());
    } else {
        hash.update(b"runner-identity:");
        hash.update(connection.runner_id.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

struct LaunchOptions {
    endpoint: Option<PathBuf>,
    project: Option<String>,
    session: Option<String>,
}

fn launch_options() -> Result<LaunchOptions, String> {
    let mut endpoint = env::var_os("LAZYBOX_V2_ENDPOINT").map(PathBuf::from);
    let mut project = env::var("LAZYBOX_V2_PROJECT")
        .ok()
        .filter(|s| !s.is_empty());
    let mut session = env::var("LAZYBOX_V2_SESSION")
        .ok()
        .filter(|s| !s.is_empty());
    let mut args = env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--endpoint" => {
                endpoint = Some(PathBuf::from(args.next().ok_or("Endpoint path required")?))
            }
            "--project" => project = Some(args.next().ok_or("Project directory required")?),
            "--session" => session = Some(args.next().ok_or("Session identity required")?),
            _ => return Err(format!("Unknown desktop option: {flag}")),
        }
    }
    Ok(LaunchOptions {
        endpoint,
        project,
        session,
    })
}

fn connection(
    app: &tauri::AppHandle,
    release_endpoint: Option<PathBuf>,
) -> Result<(Bootstrap, PathBuf), Box<dyn std::error::Error>> {
    let LaunchOptions {
        endpoint,
        project,
        session,
    } = launch_options()?;
    // An explicit endpoint (Dev, Working, `make desktop`) wins; an installed
    // release connects to its own service's (release.rs).
    let endpoint = endpoint.or(release_endpoint);
    let config = app.path().app_config_dir()?.join("runner.json");
    let previous: Option<Locator> = match fs::read(&config) {
        Ok(data) => Some(serde_json::from_slice(&data)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let path = endpoint
        .or_else(|| previous.as_ref().map(|p| p.endpoint.clone()))
        .ok_or("Use make desktop once to select the Runner endpoint file")?;
    let mut bootstrap = Bootstrap::read(&path)?;
    bootstrap.working_directory = project
        .or_else(|| previous.map(|p| p.project))
        .unwrap_or_default();
    bootstrap.session_id = session.unwrap_or_default();
    if !bootstrap.session_id.is_empty() && !crate::layout::valid_id(&bootstrap.session_id) {
        return Err("Invalid initial session identity".into());
    }
    // Persist only the selected connection locator, never its bearer.
    let parent = config.parent().ok_or("Connection config has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(
        &mut file,
        &Locator {
            endpoint: path.clone(),
            project: bootstrap.working_directory.clone(),
        },
    )?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(config)?;
    Ok((bootstrap, path))
}

pub fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle();
    // Dev, Working and `make desktop` name their Runner; an installed release
    // finds (or first sets up) its own service.
    if launch_options()?.endpoint.is_none()
        && let Some(release) = crate::release::Release::detect(handle)
    {
        // Preparing can take long: the first launch after an update moves the
        // Runner to this release (`lazybox service install` reads the login
        // shell, then waits for the new Runner to answer), up to about a
        // minute and a half. It runs off the main thread so the app stays
        // responsive meanwhile; the windows open when it is done, on the main
        // thread, as setup_finish opens them from the setup window.
        let handle = handle.clone();
        std::thread::Builder::new()
            .name("lazybox-release-prepare".into())
            .spawn(move || {
                let endpoint = crate::release::prepare(&release);
                let main = handle.clone();
                let opened = handle.run_on_main_thread(move || open_release(&main, endpoint));
                if let Err(error) = opened {
                    eprintln!("Lazybox could not open its windows: {error}");
                    handle.exit(1);
                }
            })?;
        return Ok(());
    }
    start(handle, None)
}

/// Connects to the prepared Runner, or asks the person (the setup window)
/// when there is none or connecting fails: that window can start, repair or
/// diagnose the service.
fn open_release(app: &tauri::AppHandle, endpoint: Option<PathBuf>) {
    let started = match endpoint {
        Some(endpoint) => start(app, Some(endpoint)).map_err(|e| e.to_string()),
        None => Err(String::new()),
    };
    if let Err(error) = started {
        if !error.is_empty() {
            eprintln!("Lazybox could not connect to its Runner: {error}");
        }
        if let Err(error) = crate::release::open_setup(app) {
            eprintln!("Lazybox could not open its setup window: {error}");
            app.exit(1);
        }
    }
}

/// Connects the app to its Runner and restores its windows.
pub fn start(
    app: &tauri::AppHandle,
    release_endpoint: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (connection, endpoint) = connection(app, release_endpoint)?;
    let authority = authority(&connection);
    let repository = Repository::new(&app.path().app_local_data_dir()?, &authority);
    println!("Desktop layout file: {}", repository.path.display());
    let (mut layout, notice) = match repository.load() {
        Ok(layout) => (layout, String::new()),
        Err(error) => {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let backup = repository
                .path
                .with_extension(format!("invalid-{}-{timestamp}.json", std::process::id()));
            fs::rename(&repository.path, backup)?;
            (
                Layout::default(),
                format!(
                    "The saved window layout could not be restored. A backup was retained. {error}"
                ),
            )
        }
    };
    if !connection.session_id.is_empty()
        && layout.surfaces["main"].target.session_id != connection.session_id
    {
        layout.save_target(
            "main",
            Target {
                session_id: connection.session_id.clone(),
                ..Target::default()
            },
        )?;
        layout.mark_open("main")?;
    }
    let mut restore: Vec<(u64, String)> = layout
        .surfaces
        .values()
        .filter(|s| s.open)
        .map(|s| (s.changed, s.label.clone()))
        .collect();
    if restore.is_empty() {
        layout.mark_open("main")?;
        restore.push((layout.sequence, "main".into()));
    }
    restore.sort();
    let allowed = serde_json::json!({ "url": format!("{}{RPC_PATH}*", connection.url) });
    app.add_capability(
        CapabilityBuilder::new("local-runner")
            .webviews(APP_WEBVIEWS)
            .permission("core:default")
            // Tauri forwards window.print() to the native print dialog on macOS.
            .permission("core:webview:allow-print")
            // View → Zoom In / Zoom Out / Actual Size (Cmd+= / Cmd+- / Cmd+0)
            // set a window’s zoom. Settings → Appearance sets the app default.
            .permission("core:webview:allow-set-webview-zoom")
            // Context menus: Reveal in Finder for a workspace file.
            .permission("opener:allow-reveal-item-in-dir")
            .permission_scoped(
                "http:default",
                vec![allowed],
                Vec::<serde_json::Value>::new(),
            ),
    )?;
    let views = crate::viewstate::Repository::open(&app.path().app_local_data_dir()?, &authority)?;
    app.manage(crate::appearance::Zoom::open(
        &app.path().app_local_data_dir()?,
    ));
    app.manage(Host {
        connection: Mutex::new(connection),
        reload_endpoint: Some(endpoint),
        authority,
        repository,
        views: Mutex::new(views),
        layout: Mutex::new(layout),
        quitting: AtomicBool::new(false),
        notice: Mutex::new(notice),
        restart: Mutex::new(None),
    });
    app.set_menu(build_menu(app, &[])?)?;
    for (_, label) in restore {
        open(app, &label)?;
    }
    Ok(())
}

fn monitors(app: &tauri::AppHandle) -> Result<Vec<Display>, String> {
    let primary = app.primary_monitor().map_err(|e| e.to_string())?;
    app.available_monitors()
        .map_err(|e| e.to_string())
        .map(|monitors| {
            monitors
                .into_iter()
                .map(|m| {
                    let work = m.work_area();
                    Display {
                        name: m.name().cloned(),
                        scale: m.scale_factor(),
                        work: Rect {
                            x: f64::from(work.position.x),
                            y: f64::from(work.position.y),
                            width: f64::from(work.size.width),
                            height: f64::from(work.size.height),
                        },
                        primary: primary
                            .as_ref()
                            .is_some_and(|p| p.position() == m.position()),
                    }
                })
                .collect()
        })
}

fn geometry(window: &tauri::Window) -> Result<Geometry, String> {
    let position = window.outer_position().map_err(|e| e.to_string())?;
    let outer = window.outer_size().map_err(|e| e.to_string())?;
    // Not `inner_size()`: it goes stale once a Web panel's viewer is in the window.
    let inner = crate::viewer::content_size(window)?;
    let monitor = window
        .current_monitor()
        .map_err(|e| e.to_string())?
        .ok_or("Window has no display")?;
    let work = monitor.work_area();
    Ok(Geometry {
        outer: Rect {
            x: f64::from(position.x),
            y: f64::from(position.y),
            width: f64::from(outer.width),
            height: f64::from(outer.height),
        },
        inner_width: f64::from(inner.width),
        inner_height: f64::from(inner.height),
        display: Display {
            name: monitor.name().cloned(),
            scale: monitor.scale_factor(),
            work: Rect {
                x: f64::from(work.position.x),
                y: f64::from(work.position.y),
                width: f64::from(work.size.width),
                height: f64::from(work.size.height),
            },
            primary: false,
        },
    })
}

fn mutate<T>(
    app: &tauri::AppHandle,
    change: impl FnOnce(&mut Layout) -> Result<T, String>,
) -> Result<T, String> {
    let host = app.state::<Host>();
    let mut layout = host.layout.lock().map_err(|e| e.to_string())?;
    let result = change(&mut layout)?;
    host.repository.save(&layout)?;
    Ok(result)
}

pub fn finish(app: &tauri::AppHandle) {
    if let Some(host) = app.try_state::<Host>() {
        host.quitting.store(true, Ordering::SeqCst);
        let result = host
            .layout
            .lock()
            .map_err(|e| e.to_string())
            .and_then(|layout| host.repository.save(&layout));
        if let Err(error) = result {
            report(app, error);
        }
    }
}

pub fn report(app: &tauri::AppHandle, error: String) {
    eprintln!("Desktop window state: {error}");
    if let Some(host) = app.try_state::<Host>()
        && let Ok(mut notice) = host.notice.lock()
    {
        *notice = error.clone();
    }
    let _ = app.emit("desktop-error", error);
}

pub fn open(app: &tauri::AppHandle, label: &str) -> Result<(), String> {
    open_with_updates(app, label, false)
}

fn open_with_updates(
    app: &tauri::AppHandle,
    label: &str,
    check_updates: bool,
) -> Result<(), String> {
    // No layout lock may be held while calling a native window API.
    let surface: Surface = {
        // None until the app is connected: while an installed release still
        // prepares its Runner, or while the setup window is open.
        let host = app
            .try_state::<Host>()
            .ok_or("Lazybox is still connecting to its Runner")?;
        let layout = host.layout.lock().map_err(|e| e.to_string())?;
        layout
            .surfaces
            .get(label)
            .cloned()
            .ok_or("Unknown window surface")?
    };
    // An open window already owns its placement and zoom. Restoring its
    // saved geometry here can move it to a different monitor as it comes forward.
    if let Some(window) = app.get_window(label) {
        window.show().map_err(|e| e.to_string())?;
        window.unminimize().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        if check_updates {
            app.emit_to(label, "menu-action", "help.updates")
                .map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    let displays = monitors(app)?;
    let placement = placement::fit(surface.geometry.as_ref(), &displays)?;
    mutate(app, |layout| layout.mark_open(label))?;
    let name = app.config().product_name.as_deref().unwrap_or("Lazybox");
    let kind = if label == "main" {
        "Workspaces"
    } else if label.starts_with("workspace-") {
        "Workspace"
    } else if label.starts_with("chat-") {
        "Chat"
    } else if label.starts_with("mission-") {
        "Mission"
    } else if label.starts_with("artifact-") {
        "Artifact"
    } else if label.starts_with("web-") {
        "Web"
    } else {
        "Console"
    };
    // No separate title bar: the page's header is it (title_bar.rs).
    let builder = match crate::title_bar::window(app, label, WebviewUrl::App("index.html".into())) {
        Ok(builder) => builder,
        Err(error) => {
            let _ = mutate(app, |layout| layout.close(label));
            return Err(error.to_string());
        }
    };
    let builder = if check_updates {
        builder.initialization_script("window.lazyboxCheckUpdatesOnOpen = true")
    } else {
        builder
    };
    let builder = builder
        // Route file drops to the shared chat attachment flow (HTML5 File API).
        .disable_drag_drop_handler()
        .title(format!("{name} — {kind}"))
        .visible(false)
        .inner_size(placement.width, placement.height)
        .min_inner_size(placement.min_width, placement.min_height);
    // A workspace window can pop a panel group out into a window of its own.
    let builder = if label == "main" || label.starts_with("workspace-") {
        let handle = app.clone();
        let title = format!("{name} — Panel");
        let opener = label.to_string();
        builder.on_new_window(move |url, features| {
            popout_window(&handle, &url, features, &title, &opener)
        })
    } else {
        builder
    };
    let window = match builder.build() {
        Ok(window) => window,
        Err(error) => {
            let _ = mutate(app, |layout| layout.close(label));
            return Err(error.to_string());
        }
    };
    window
        .set_position(tauri::PhysicalPosition::new(placement.x, placement.y))
        .map_err(|e| e.to_string())?;
    // Before it shows: a new window opens at the saved app default.
    window
        .set_zoom(app.state::<crate::appearance::Zoom>().level())
        .map_err(|e| e.to_string())?;
    if surface.maximized {
        window.maximize().map_err(|e| e.to_string())?;
    }
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn runner_bootstrap(window: Webview, host: State<'_, Host>) -> Result<WindowBootstrap, String> {
    let layout = host.layout.lock().map_err(|e| e.to_string())?;
    let surface = layout
        .surfaces
        .get(window.label())
        .ok_or("Unknown window surface")?;
    Ok(WindowBootstrap {
        view_state: host.views.lock().map_err(|e| e.to_string())?.values.clone(),
        connection: match &host.reload_endpoint {
            Some(path) => {
                follow_restart(&window, &host, path)?;
                let mut current = host.connection.lock().map_err(|e| e.to_string())?;
                let next = current.followed(path).map_err(|e| e.to_string())?;
                adopt(&window, &mut current, next)?;
                current.clone()
            }
            None => host.connection.lock().map_err(|e| e.to_string())?.clone(),
        },
        target: surface.target.clone(),
        view_key: if surface.target.panel_id.is_empty() {
            format!("{}:{}", host.authority, window.label())
        } else {
            format!(
                "{}:workspace:{}:panel:{}",
                host.authority,
                if surface.target.collection_id.is_empty() {
                    "home"
                } else {
                    &surface.target.collection_id
                },
                surface.target.panel_id
            )
        },
        view_root: host.authority.clone(),
        view_scope: if surface.target.panel_id.is_empty() {
            host.authority.clone()
        } else {
            format!(
                "{}:workspace:{}",
                host.authority,
                if surface.target.collection_id.is_empty() {
                    "home"
                } else {
                    &surface.target.collection_id
                }
            )
        },
        application_name: window
            .app_handle()
            .config()
            .product_name
            .clone()
            .unwrap_or_else(|| "Lazybox".into()),
        fixed_workspace_id: if window.label().starts_with("workspace-") {
            surface.target.collection_id.clone()
        } else {
            String::new()
        },
        kind: if window.label() == "main" || window.label().starts_with("workspace-") {
            "workspace"
        } else if window.label().starts_with("mission-") {
            "mission"
        } else if window.label().starts_with("artifact-") {
            "artifact"
        } else if window.label().starts_with("web-") {
            "web"
        } else if !surface.target.panel_id.is_empty() {
            "chat"
        } else {
            "console"
        },
        notice: host.notice.lock().map_err(|e| e.to_string())?.clone(),
    })
}
#[tauri::command]
pub fn save_surface(app: tauri::AppHandle, window: Webview, target: Target) -> Result<(), String> {
    mutate(&app, |layout| layout.save_target(window.label(), target))
}
#[tauri::command]
pub fn open_console(
    app: tauri::AppHandle,
    session_id: String,
    command_id: String,
) -> Result<(), String> {
    let target = Target {
        session_id,
        command_id,
        commands_open: true,
        ..Target::default()
    };
    let label = mutate(&app, |layout| layout.console(target))?;
    open(&app, &label)
}
#[tauri::command]
pub fn open_chat(
    app: tauri::AppHandle,
    panel_id: String,
    session_id: String,
    collection_id: String,
) -> Result<(), String> {
    let label = mutate(&app, |layout| {
        layout.chat(Target {
            panel_id,
            session_id,
            collection_id,
            ..Target::default()
        })
    })?;
    if let Err(error) = open(&app, &label) {
        if let Some(window) = app.get_window(&label) {
            let _ = window.close();
        }
        let _ = mutate(&app, |layout| layout.close(&label));
        return Err(error);
    }
    Ok(())
}
/// A Web panel moved to a window of its own (the same viewer, full window).
#[tauri::command]
pub fn open_web_window(
    app: tauri::AppHandle,
    web_id: String,
    url: String,
    collection_id: String,
) -> Result<(), String> {
    let label = mutate(&app, |layout| {
        layout.web(Target {
            web_id,
            web_url: url,
            collection_id,
            ..Target::default()
        })
    })?;
    open(&app, &label)
}
/// A detached Web panel goes back: its page opens in a Web panel of the main
/// window, and its own window closes.
#[tauri::command]
pub fn dock_web(app: tauri::AppHandle, window: Webview, url: String) -> Result<(), String> {
    let label = window.label().to_string();
    if !label.starts_with("web-") {
        return Err("Only a web window can be moved back to the workspace".into());
    }
    let page = crate::viewer::page_url(&url)?;
    open(&app, "main")?;
    app.emit_to(
        tauri::EventTarget::webview("main"),
        "web-dock",
        page.as_str(),
    )
    .map_err(|e| e.to_string())?;
    if let Some(window) = app.get_window(&label) {
        window.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}
#[tauri::command]
pub fn open_mission(app: tauri::AppHandle, mission_id: String) -> Result<(), String> {
    let label = mutate(&app, |layout| layout.mission(mission_id))?;
    open(&app, &label)
}
#[tauri::command]
pub fn open_artifact(
    app: tauri::AppHandle,
    collection_id: String,
    artifact_id: String,
) -> Result<(), String> {
    let label = mutate(&app, |layout| {
        layout.artifact(Target {
            collection_id,
            artifact_id,
            ..Target::default()
        })
    })?;
    open(&app, &label)
}
#[tauri::command]
pub fn open_workspace(app: tauri::AppHandle, collection_id: String) -> Result<(), String> {
    let label = mutate(&app, |layout| {
        let target = layout
            .surfaces
            .get("main")
            .filter(|s| s.target.collection_id == collection_id)
            .map(|s| s.target.clone())
            .unwrap_or(Target {
                collection_id,
                ..Target::default()
            });
        layout.workspace(target)
    })?;
    open(&app, &label)
}
/// A workspace window takes a chat back from its window: that window closes
/// and the calling window comes forward, where the chat now is.
#[tauri::command]
pub fn dock_chat(app: tauri::AppHandle, window: Webview, panel_id: String) -> Result<(), String> {
    let caller = window.label().to_string();
    if !is_workspace_window(&caller) {
        return Err("Only a workspace window takes a chat back".into());
    }
    if !crate::layout::valid_id(&panel_id) {
        return Err("Invalid chat panel identity".into());
    }
    let label = format!("chat-{panel_id}");
    if let Some(window) = app.get_window(&label) {
        window.close().map_err(|error| error.to_string())?;
    }
    mutate(&app, |layout| {
        if layout.surfaces.contains_key(&label) {
            layout.close(&label)?;
        }
        Ok(())
    })?;
    open(&app, &caller)
}
/// One menu entry for an action of the web UI's action registry
/// (`packages/surfaces/src/actions.ts`): the registry, not this file, decides
/// what the menu offers and which keys it shows.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionMenuItem {
    /// `action:<registry id>`; a click emits `menu-action` with the registry id.
    pub id: String,
    pub title: String,
    /// "File", "Go" or "Help".
    pub section: String,
    pub accelerator: Option<String>,
    #[serde(default)]
    pub separator_before: bool,
}

const ACTION_SECTIONS: [&str; 4] = ["File", "Go", "Window", "Help"];

/// The app menu: the platform defaults, the window commands, and the action
/// registry's entries by section.
fn build_menu(
    app: &tauri::AppHandle,
    actions: &[ActionMenuItem],
) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::default(app)?;
    let surfaces = Submenu::with_items(
        app,
        "Surfaces",
        true,
        &[
            &MenuItem::with_id(
                app,
                "reopen-agent",
                "Reopen Agent",
                true,
                Some("CmdOrCtrl+Shift+A"),
            )?,
            &MenuItem::with_id(
                app,
                "reopen-closed",
                "Reopen Last Closed Window",
                true,
                Some("CmdOrCtrl+Shift+T"),
            )?,
            &MenuItem::with_id(
                app,
                "reload-view",
                "Reconnect This Window",
                true,
                Some("CmdOrCtrl+R"),
            )?,
        ],
    )?;
    let arrangements = Submenu::new(app, "Arrange Windows", true)?;
    for (id, title) in [
        ("layout-columns", "Equal Columns on This Display"),
        ("layout-focus", "Focused Window + Stack on This Display"),
        ("layout-grid", "Grid on This Display"),
        ("layout-next-display", "Move Focused Window to Next Display"),
        ("layout-save", "Save Current Window Positions"),
        ("layout-restore", "Restore Saved Window Positions"),
    ] {
        arrangements.append(&MenuItem::with_id(app, id, title, true, None::<&str>)?)?;
    }
    surfaces.append(&arrangements)?;
    menu.append(&surfaces)?;
    // Zoom shortcuts apply to the focused window.
    let zoom = Submenu::with_items(
        app,
        "Zoom",
        true,
        &[
            &MenuItem::with_id(app, "zoom-in", "Zoom In", true, Some("CmdOrCtrl+="))?,
            &MenuItem::with_id(app, "zoom-out", "Zoom Out", true, Some("CmdOrCtrl+-"))?,
            &MenuItem::with_id(app, "zoom-reset", "Actual Size", true, Some("CmdOrCtrl+0"))?,
        ],
    )?;
    menu.append(&zoom)?;
    for section in ACTION_SECTIONS {
        let items: Vec<&ActionMenuItem> = actions
            .iter()
            .filter(|item| item.section == section && item.id.starts_with("action:"))
            .collect();
        if items.is_empty() {
            continue;
        }
        // The platform's Window menu (macOS) takes the window actions, after its own.
        let existing = (section == "Window")
            .then(|| menu.get(tauri::menu::WINDOW_SUBMENU_ID))
            .flatten()
            .and_then(|item| item.as_submenu().cloned());
        if let Some(window_menu) = &existing {
            window_menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
        let submenu = match existing {
            Some(window_menu) => window_menu,
            None => Submenu::new(app, section, true)?,
        };
        for (index, item) in items.into_iter().enumerate() {
            if item.separator_before && index > 0 {
                submenu.append(&PredefinedMenuItem::separator(app)?)?;
            }
            // An accelerator the platform cannot parse leaves the item without one.
            let entry = match MenuItem::with_id(
                app,
                item.id.as_str(),
                item.title.as_str(),
                true,
                item.accelerator.as_deref(),
            ) {
                Ok(entry) => entry,
                Err(_) => MenuItem::with_id(
                    app,
                    item.id.as_str(),
                    item.title.as_str(),
                    true,
                    None::<&str>,
                )?,
            };
            submenu.append(&entry)?;
        }
        if menu.get(submenu.id()).is_none() {
            menu.append(&submenu)?;
        }
    }
    Ok(menu)
}

/// The main window publishes the action menu at start and whenever its keys change.
#[tauri::command]
pub fn set_action_menu(app: tauri::AppHandle, items: Vec<ActionMenuItem>) -> Result<(), String> {
    let menu = build_menu(&app, &items).map_err(|e| e.to_string())?;
    app.set_menu(menu).map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn reopen_agent(app: tauri::AppHandle) -> Result<(), String> {
    open(&app, "main")
}
#[tauri::command]
pub fn reopen_closed(app: tauri::AppHandle) -> Result<(), String> {
    let label = {
        let host = app.state::<Host>();
        let layout = host.layout.lock().map_err(|e| e.to_string())?;
        layout
            .last_closed()
            .ok_or("There are no closed windows to reopen")?
    };
    open(&app, &label)
}
#[tauri::command]
pub fn reconnect_view(window: Webview, host: State<'_, Host>) -> Result<(), String> {
    if let Some(path) = &host.reload_endpoint {
        let mut current = host.connection.lock().map_err(|e| e.to_string())?;
        let next = current.reconnected(path).map_err(|e| e.to_string())?;
        adopt(&window, &mut current, next)?;
    }
    window.reload().map_err(|e| e.to_string())
}

/// Install the HTTP scope for a connection's origin, then make it current.
fn adopt(window: &Webview, current: &mut Bootstrap, next: Bootstrap) -> Result<(), String> {
    if next.url != current.url {
        window
            .app_handle()
            .add_capability(
                CapabilityBuilder::new(format!(
                    "reconnected-{:x}",
                    Sha256::digest(next.url.as_bytes())
                ))
                .webviews(APP_WEBVIEWS)
                .permission_scoped(
                    "http:default",
                    vec![serde_json::json!({"url":format!("{}{RPC_PATH}*",next.url)})],
                    Vec::<serde_json::Value>::new(),
                ),
            )
            .map_err(|e| e.to_string())?;
    }
    *current = next;
    Ok(())
}

/// The person asked this Runner to restart. Its launcher starts the next one,
/// which may get a new port; follow it from the same private locator, for the
/// same Runner identity, once, without reloading any window.
#[tauri::command]
pub fn expect_runner_restart(host: State<'_, Host>) -> Result<(), String> {
    let Some(path) = &host.reload_endpoint else {
        return Err("This window has no Runner locator to follow a restart through".into());
    };
    // The running Runner's bearer as its locator names it now: the connection
    // may still hold an older one, since a hot reload's bearer is only read.
    let token = Bootstrap::read(path).map_err(|e| e.to_string())?.token;
    *host.restart.lock().map_err(|e| e.to_string())? =
        Some((token, Instant::now() + RESTART_FOLLOW));
    Ok(())
}

/// While a requested restart is armed, adopt the successor the locator names.
/// Every window's periodic bootstrap read runs this, so whichever reads first
/// moves them all; the rest then see an unchanged origin.
fn follow_restart(window: &Webview, host: &Host, path: &std::path::Path) -> Result<(), String> {
    let mut armed = host.restart.lock().map_err(|e| e.to_string())?;
    let Some((previous, deadline)) = armed.clone() else {
        return Ok(());
    };
    if Instant::now() >= deadline {
        *armed = None;
        return Ok(());
    }
    let mut current = host.connection.lock().map_err(|e| e.to_string())?;
    if let Some(next) = current
        .restarted(path, &previous)
        .map_err(|e| e.to_string())?
    {
        adopt(window, &mut current, next)?;
        *armed = None;
    }
    Ok(())
}

/// Whether `url` is this app's own blank page for popped-out panels (the
/// bundled `tauri://localhost/popout.html`, or the dev server's).
pub fn is_popout_page(url: &tauri::Url) -> bool {
    let local = matches!(
        url.host_str(),
        Some("localhost" | "tauri.localhost" | "127.0.0.1")
    );
    url.path() == "/popout.html" && local && matches!(url.scheme(), "tauri" | "http" | "https")
}

/// A panel group popped out of a workspace window (dockview's popout groups,
/// `DockLayout.tsx`). The opener draws into it through the relationship WebKit
/// keeps for `window.open`, so it needs no IPC and no Runner credential, and
/// the panels in it are the opener's (an editor keeps its unsaved edits).
/// Only `popout.html` of this app opens; every other `window.open` is refused.
/// Its webview is labelled `popout-…`, which no capability names
/// (`APP_WEBVIEWS`): it needs none, since its panels call the host from the
/// opener.
/// These windows are not saved in the window layout: their panels come back
/// docked when the workspace window is restored.
fn popout_window(
    app: &tauri::AppHandle,
    url: &tauri::Url,
    features: tauri::webview::NewWindowFeatures,
    title: &str,
    opener: &str,
) -> tauri::webview::NewWindowResponse<tauri::Wry> {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    if !is_popout_page(url) {
        return tauri::webview::NewWindowResponse::Deny;
    }
    let label = format!("popout-{}", NEXT.fetch_add(1, Ordering::SeqCst));
    let level = app
        .try_state::<crate::appearance::Zoom>()
        .map(|zoom| zoom.level())
        .unwrap_or(1.0);
    let built = crate::title_bar::window(app, &label, WebviewUrl::External(url.clone()))
        .map(|builder| builder.window_features(features))
        .and_then(|builder| {
            builder
                .disable_drag_drop_handler()
                .zoom_hotkeys_enabled(false)
                .initialization_script(crate::appearance::popout_zoom_script(level))
                .title(title)
                .min_inner_size(320.0, 200.0)
                .build()
        });
    match built {
        Ok(window) => {
            if let Ok(mut openers) = POPOUT_OPENERS.lock() {
                openers.insert(label, opener.to_string());
            }
            // Open at the saved app default, with matching title-bar spacing.
            if let Err(error) = window.set_zoom(level) {
                report(app, format!("Could not zoom the panel's window: {error}"));
            }
            tauri::webview::NewWindowResponse::Create { window }
        }
        Err(error) => {
            report(app, format!("Could not open the panel's window: {error}"));
            tauri::webview::NewWindowResponse::Deny
        }
    }
}

/// The workspace window that drew each popped-out panel window: the Window
/// menu's **Put Back in Main Window** for a popout in front runs there.
static POPOUT_OPENERS: Mutex<std::collections::BTreeMap<String, String>> =
    Mutex::new(std::collections::BTreeMap::new());

/// The workspace window that draws a popped-out panel window, by its label.
pub fn popout_opener(label: &str) -> Option<String> {
    if !label.starts_with("popout-") {
        return None;
    }
    POPOUT_OPENERS.lock().ok()?.get(label).cloned()
}

/// The window a menu action runs in: the one in front, or for a popped-out
/// panel window (which runs no script) the workspace window that draws it.
fn menu_target(focused: Option<String>) -> String {
    match focused {
        Some(label) if label.starts_with("popout-") => POPOUT_OPENERS
            .lock()
            .ok()
            .and_then(|openers| openers.get(&label).cloned())
            .unwrap_or_else(|| "main".to_string()),
        Some(label) => label,
        None => "main".to_string(),
    }
}

/// The window a detached chat goes back to: the workspace window of its
/// workspace when one is open (it shows that workspace), else the main window.
pub fn chat_owner(layout: &Layout, chat_label: &str) -> String {
    let collection = layout
        .surfaces
        .get(chat_label)
        .map(|surface| surface.target.collection_id.clone())
        .unwrap_or_default();
    let workspace = format!("workspace-{collection}");
    if !collection.is_empty() && layout.surfaces.get(&workspace).is_some_and(|s| s.open) {
        workspace
    } else {
        "main".to_string()
    }
}

/// What a chat window's owner is told when the chat goes back (`ChatReturn`
/// in `@lazybox/platform`).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatReturn {
    panel_id: String,
    collection_id: String,
    reason: &'static str,
}

/// Tells the window that owns a chat window's panel that it goes back;
/// returns that window's label.
fn notify_chat_return(
    app: &tauri::AppHandle,
    chat_label: &str,
    reason: &'static str,
) -> Result<String, String> {
    let (owner, collection_id) = {
        let host = app.state::<Host>();
        let layout = host.layout.lock().map_err(|e| e.to_string())?;
        let collection = layout
            .surfaces
            .get(chat_label)
            .map(|surface| surface.target.collection_id.clone())
            .unwrap_or_default();
        (chat_owner(&layout, chat_label), collection)
    };
    let panel_id = chat_label
        .strip_prefix("chat-")
        .ok_or("Not a chat window")?
        .to_string();
    app.emit_to(
        tauri::EventTarget::webview(owner.as_str()),
        "chat-return",
        ChatReturn {
            panel_id,
            collection_id,
            reason,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(owner)
}

/// A chat's own window asks to go back where it came from: its owner puts the
/// panel back and comes forward, and this window closes. The chat is the
/// calling window's own (its label), so a window can only put itself back.
#[tauri::command]
pub fn return_chat(app: tauri::AppHandle, window: Webview) -> Result<(), String> {
    let label = window.label().to_string();
    if !label.starts_with("chat-") {
        return Err("Only a chat window can be put back".into());
    }
    let owner = notify_chat_return(&app, &label, "put-back")?;
    if let Some(window) = app.get_window(&label) {
        window.close().map_err(|e| e.to_string())?;
    }
    open(&app, &owner)
}

/// Whether a chat panel's own window is open: a workspace window puts back a
/// saved chat whose window is gone.
#[tauri::command]
pub fn chat_window_open(
    window: Webview,
    host: State<'_, Host>,
    panel_id: String,
) -> Result<bool, String> {
    if !is_workspace_window(window.label()) {
        return Err("Only a workspace window asks about its chat windows".into());
    }
    if !crate::layout::valid_id(&panel_id) {
        return Err("Invalid chat panel identity".into());
    }
    let layout = host.layout.lock().map_err(|e| e.to_string())?;
    Ok(layout
        .surfaces
        .get(&format!("chat-{panel_id}"))
        .is_some_and(|surface| surface.open))
}

fn is_workspace_window(label: &str) -> bool {
    label == "main" || label.starts_with("workspace-")
}

pub fn window_event(window: &tauri::Window, event: &WindowEvent) {
    crate::title_bar::window_event(window, event);
    if matches!(event, WindowEvent::Destroyed)
        && let Some(zoom) = window.try_state::<crate::appearance::Zoom>()
    {
        zoom.forget(window.label());
    }
    // Popped-out panel windows belong to their opener, not to the window layout.
    if window.label().starts_with("popout-") {
        if matches!(event, WindowEvent::Destroyed)
            && let Ok(mut openers) = POPOUT_OPENERS.lock()
        {
            openers.remove(window.label());
        }
        return;
    }
    let app = window.app_handle();
    let Some(host) = app.try_state::<Host>() else {
        // Closing the first-run window before setting up quits: there is
        // nothing to connect to yet. The Runner was not touched.
        if window.label() == crate::release::SETUP_LABEL && matches!(event, WindowEvent::Destroyed)
        {
            app.exit(0);
        }
        return;
    };
    if window.label() == crate::release::SETUP_LABEL {
        return;
    }
    if host.quitting.load(Ordering::SeqCst) {
        return;
    }
    let label = window.label();
    let result = match event {
        WindowEvent::CloseRequested { .. } => {
            // Closing a chat's window puts the chat back: it is never lost.
            if label.starts_with("chat-")
                && let Err(error) = notify_chat_return(app, label, "closed")
            {
                report(app, error);
            }
            let captured = geometry(window).ok();
            mutate(app, |layout| {
                if let Some(geometry) = captured {
                    let surface = layout.touch(label)?;
                    if !surface.maximized {
                        surface.geometry = Some(geometry);
                    }
                }
                layout.close(label)
            })
        }
        WindowEvent::Moved(_)
        | WindowEvent::Resized(_)
        | WindowEvent::ScaleFactorChanged { .. } => {
            let maximized = window.is_maximized().unwrap_or(false);
            let captured = if maximized {
                None
            } else {
                geometry(window).ok()
            };
            // Cache motion; close, navigation, and Quit persist it atomically.
            // Dragging a window must not fsync the UI thread on every pixel.
            (|| {
                let mut layout = host.layout.lock().map_err(|e| e.to_string())?;
                let surface = layout
                    .surfaces
                    .get_mut(label)
                    .ok_or("Unknown window surface")?;
                surface.maximized = maximized;
                if let Some(geometry) = captured {
                    surface.geometry = Some(geometry);
                }
                Ok(())
            })()
        }
        WindowEvent::Focused(true) => (|| {
            let mut layout = host.layout.lock().map_err(|e| e.to_string())?;
            layout.touch(label)?;
            Ok(())
        })(),
        WindowEvent::Destroyed => {
            let survivor = host
                .layout
                .lock()
                .ok()
                .and_then(|layout| layout.survivor(label));
            if let Some(window) = survivor.and_then(|label| app.get_window(&label)) {
                let _ = window.set_focus();
            }
            // A Web panel's views go with the window they were docked in.
            crate::viewer::forget(app, &app.state::<crate::viewer::Viewers>(), label);
            Ok(())
        }
        _ => Ok(()),
    };
    if let Err(error) = result {
        report(app, error);
    }
}

fn apply_placement(
    w: &tauri::Window,
    p: &placement::Placement,
    maximized: bool,
) -> tauri::Result<()> {
    w.unmaximize()?;
    w.set_min_size(Some(tauri::LogicalSize::new(p.min_width, p.min_height)))?;
    w.set_position(tauri::PhysicalPosition::new(p.x, p.y))?;
    w.set_size(tauri::LogicalSize::new(p.width, p.height))?;
    if maximized {
        w.maximize()?;
    }
    Ok(())
}

/// Arrange visible Lazybox windows on the focused display. Other displays keep their roles.
fn arrange_windows(app: &tauri::AppHandle, action: &str) -> Result<(), String> {
    let host = app.try_state::<Host>().ok_or("Connect a Runner first")?;
    let labels: Vec<_> = host
        .layout
        .lock()
        .map_err(|e| e.to_string())?
        .surfaces
        .keys()
        .cloned()
        .collect();
    let key = format!("{}:window-arrangement", host.authority);
    let mut windows = Vec::new();
    for label in labels {
        if let Some(window) = app.get_window(&label) {
            windows.push((label, window));
        }
    }
    let error = |e: tauri::Error| e.to_string();
    if action == "layout-save" {
        let mut saved = std::collections::BTreeMap::new();
        for (label, w) in windows {
            if w.is_fullscreen().map_err(error)? || w.is_minimized().map_err(error)? {
                continue;
            }
            saved.insert(label, (geometry(&w)?, w.is_maximized().map_err(error)?));
        }
        if saved.is_empty() {
            return Err("No visible windows to save".into());
        }
        let json = serde_json::to_string(&saved).map_err(|e| e.to_string())?;
        return host
            .views
            .lock()
            .map_err(|e| e.to_string())?
            .save(&key, &json);
    }
    if action == "layout-restore" {
        let saved: std::collections::BTreeMap<String, (Geometry, bool)> = {
            let views = host.views.lock().map_err(|e| e.to_string())?;
            serde_json::from_str(
                views
                    .values
                    .get(&key)
                    .ok_or("Save a window arrangement first")?,
            )
            .map_err(|e| e.to_string())?
        };
        let displays = monitors(app)?;
        for (label, w) in windows {
            let Some((g, maximized)) = saved.get(&label) else {
                continue;
            };
            if w.is_fullscreen().map_err(error)? {
                continue;
            }
            let p = placement::fit(Some(g), &displays)?;
            w.unminimize().map_err(error)?;
            apply_placement(&w, &p, *maximized).map_err(error)?;
        }
        return Ok(());
    }
    let focus = app
        .get_focused_window()
        .ok_or("Focus a Lazybox window first")?;
    if action == "layout-next-display" {
        if focus.is_fullscreen().map_err(error)? {
            return Err("Exit full screen before moving this window".into());
        }
        let current = geometry(&focus)?;
        let mut displays = monitors(app)?;
        displays.retain(Display::valid);
        displays.sort_by(|a, b| {
            a.work
                .x
                .total_cmp(&b.work.x)
                .then(a.work.y.total_cmp(&b.work.y))
        });
        if displays.len() < 2 {
            return Err("Connect a second display to move this window".into());
        }
        let index = displays
            .iter()
            .position(|d| d.work == current.display.work)
            .ok_or("The display arrangement changed; try again")?;
        let destination = &displays[(index + 1) % displays.len()];
        // fit centers a window when its old display is unavailable, preserving
        // logical dimensions across monitors with different pixel densities.
        let p = placement::fit(Some(&current), std::slice::from_ref(destination))?;
        let maximized = focus.is_maximized().map_err(error)?;
        apply_placement(&focus, &p, maximized).map_err(error)?;
        return Ok(());
    }
    let work = geometry(&focus)?.display.work;
    let mut local = Vec::new();
    for (label, w) in windows {
        if w.is_minimized().map_err(error)? || w.is_fullscreen().map_err(error)? {
            continue;
        }
        let g = geometry(&w)?;
        if g.display.work == work {
            local.push((label, w, g));
        }
    }
    if local.len() < 2 {
        return Err("Open two Lazybox windows on this display first".into());
    }
    local.sort_by_key(|(label, _, _)| label != focus.label());
    let pattern = match action {
        "layout-columns" => placement::Pattern::Columns,
        "layout-focus" => placement::Pattern::Focus,
        "layout-grid" => placement::Pattern::Grid,
        _ => return Err("Unknown window arrangement".into()),
    };
    let count = local.len();
    let mut moves = Vec::new();
    for (i, (_, w, g)) in local.into_iter().enumerate() {
        let rect = placement::cell(&work, pattern, i, count);
        let width = (rect.width - (g.outer.width - g.inner_width)) / g.display.scale;
        let height = (rect.height - (g.outer.height - g.inner_height)) / g.display.scale;
        if width < 320.0 || height < 240.0 {
            return Err("These windows do not fit on this display. Move some to another display or choose another arrangement.".into());
        }
        moves.push((
            w,
            placement::Placement {
                x: rect.x as i32,
                y: rect.y as i32,
                width,
                height,
                min_width: width.min(580.0),
                min_height: height.min(600.0),
            },
        ));
    }
    for (w, p) in moves {
        apply_placement(&w, &p, false).map_err(error)?;
    }
    Ok(())
}

pub fn menu_event(app: &tauri::AppHandle, id: &str) {
    if let Some(action) = id.strip_prefix("action:") {
        // The window in front runs it; with none in front, the main window.
        // Windows, not webview windows: a window showing a Web panel holds more than one webview.
        let label = menu_target(
            app.windows()
                .into_iter()
                .find(|(_, w)| w.is_focused().unwrap_or(false))
                .map(|(label, _)| label),
        );
        if action == "help.updates" && (!is_workspace_window(&label) || label == "main") {
            if let Err(error) = open_with_updates(app, "main", true) {
                report(app, error);
            }
            return;
        }
        if let Err(error) = app.emit_to(label.as_str(), "menu-action", action) {
            report(app, error.to_string());
        }
        return;
    }
    let result = match id {
        "layout-columns"
        | "layout-focus"
        | "layout-grid"
        | "layout-next-display"
        | "layout-save"
        | "layout-restore" => arrange_windows(app, id),
        "reopen-agent" => open(app, "main"),
        "reopen-closed" => reopen_closed(app.clone()),
        "zoom-in" | "zoom-out" | "zoom-reset" => {
            let direction = match id {
                "zoom-in" => 1,
                "zoom-out" => -1,
                _ => 0,
            };
            match app
                .get_focused_window()
                .and_then(|window| app.get_webview_window(window.label()))
            {
                Some(window) if app.try_state::<crate::appearance::Zoom>().is_some() => {
                    crate::appearance::step_window_zoom(&window, direction)
                }
                _ => Ok(()),
            }
        }
        "reload-view" => app
            .windows()
            .values()
            .find(|w| w.is_focused().unwrap_or(false))
            .and_then(|w| app.get_webview(w.label()))
            .ok_or("No focused view to reconnect".into())
            .and_then(|w| w.reload().map_err(|e| e.to_string())),
        _ => return,
    };
    if let Err(error) = result {
        report(app, error);
    }
}

pub fn second_instance(app: &tauri::AppHandle, args: Vec<String>) {
    let Some(host) = app.try_state::<Host>() else {
        // Still setting up: bring the setup window forward.
        if let Err(error) = crate::release::open_setup(app) {
            eprintln!("Setup window: {error}");
        }
        return;
    };
    if let Some(index) = args.iter().position(|s| s == "--endpoint") {
        match args
            .get(index + 1)
            .ok_or("Endpoint path required".to_string())
            .and_then(|p| Bootstrap::read(std::path::Path::new(p)).map_err(|e| e.to_string()))
        {
            Ok(connection) if authority(&connection) == host.authority => (),
            _ => {
                report(app, "Another Runner connection is already open. Quit Lazybox before changing Runner connections.".into());
                return;
            }
        }
    }
    if let Some(index) = args.iter().position(|s| s == "--session")
        && let Some(session) = args.get(index + 1)
    {
        let result = mutate(app, |layout| {
            if layout.surfaces["main"].target.session_id == *session {
                return Ok(());
            }
            layout.save_target(
                "main",
                Target {
                    session_id: session.clone(),
                    ..Target::default()
                },
            )
        });
        if let Err(error) = result {
            report(app, error);
            return;
        }
    }
    if let Err(error) = open(app, "main") {
        report(app, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A popped-out panel window is drawn by the workspace window that opened
    /// it: its panels' code runs there and calls the host through that
    /// window's webview. The popout's own webview runs no script of its own,
    /// so it holds no capability, like a viewer, and no viewer is ever made
    /// in it (viewers are children of the calling app webview).
    #[test]
    fn a_popped_out_panel_window_holds_no_capability() {
        let patterns: Vec<glob::Pattern> = APP_WEBVIEWS
            .iter()
            .map(|p| glob::Pattern::new(p).expect("pattern"))
            .collect();
        for label in ["popout-1", "popout-42"] {
            assert!(!patterns.iter().any(|p| p.matches(label)), "{label}");
        }
    }
    /// A chat goes back to the workspace window of its workspace when one is
    /// open, else to the main window; a menu action with a popped-out panel
    /// window in front runs in the window that draws it.
    #[test]
    fn a_chat_goes_back_to_the_window_that_shows_its_workspace() {
        let mut layout = Layout::default();
        let chat = layout
            .chat(Target {
                panel_id: "panel-1".into(),
                session_id: "session".into(),
                collection_id: "alpha".into(),
                ..Target::default()
            })
            .expect("chat");
        assert_eq!(chat_owner(&layout, &chat), "main");
        let workspace = layout
            .workspace(Target {
                collection_id: "alpha".into(),
                ..Target::default()
            })
            .expect("workspace");
        assert_eq!(
            chat_owner(&layout, &chat),
            "main",
            "a closed workspace window owns nothing"
        );
        layout.mark_open(&workspace).expect("open");
        assert_eq!(chat_owner(&layout, &chat), workspace);
        assert_eq!(chat_owner(&layout, "chat-unknown"), "main");
        assert!(is_workspace_window("main") && is_workspace_window("workspace-alpha"));
        for label in [
            "chat-panel-1",
            "console-x",
            "popout-1",
            "web-x",
            "artifact-x",
        ] {
            assert!(!is_workspace_window(label), "{label}");
        }
        POPOUT_OPENERS
            .lock()
            .expect("openers")
            .insert("popout-7".into(), "workspace-alpha".into());
        assert_eq!(menu_target(Some("popout-7".into())), "workspace-alpha");
        assert_eq!(menu_target(Some("popout-8".into())), "main");
        assert_eq!(menu_target(Some("chat-panel-1".into())), "chat-panel-1");
        assert_eq!(menu_target(None), "main");
    }
    #[test]
    fn only_the_apps_own_popout_page_opens_a_panel_window() {
        let url = |value: &str| tauri::Url::parse(value).unwrap();
        assert!(is_popout_page(&url("tauri://localhost/popout.html")));
        assert!(is_popout_page(&url("http://localhost:1420/popout.html")));
        assert!(is_popout_page(&url("http://tauri.localhost/popout.html")));
        for value in [
            "https://example.com/popout.html",
            "http://localhost:1420/index.html",
            "tauri://localhost/popout.html.evil",
            "file:///popout.html",
        ] {
            assert!(!is_popout_page(&url(value)), "{value}");
        }
    }
    #[test]
    fn sign_in_links_are_limited_to_provider_https_pages() {
        assert!(sign_in_url("https://auth.openai.com/codex/device").is_ok());
        assert!(sign_in_url("https://github.com/login/device").is_ok());
        assert!(sign_in_url("https://github.com.evil.test/login/device").is_err());
        for value in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "http://auth.openai.com/",
            "https://auth.openai.com.evil.test/",
            "https://secret@auth.openai.com/",
        ] {
            assert!(sign_in_url(value).is_err());
        }
    }
    /// A Web panel's viewer is a child webview inside an app window. Tauri
    /// grants a capability to a webview whose own label, or whose window's
    /// label, matches it: so capabilities name the app's webviews, never its
    /// windows, and no pattern matches a viewer.
    #[test]
    fn no_capability_reaches_a_web_viewer() {
        let patterns: Vec<glob::Pattern> = APP_WEBVIEWS
            .iter()
            .map(|p| glob::Pattern::new(p).expect("pattern"))
            .collect();
        for owner in ["main", "workspace-alpha", "web-panel-1"] {
            for id in ["web-1", "preview-alpha", "window-panel-1"] {
                let viewer = crate::viewer::label(owner, id);
                assert!(
                    !patterns.iter().any(|p| p.matches(&viewer)),
                    "{viewer} would hold the app's capabilities"
                );
            }
        }
        for app_webview in [
            "main",
            "workspace-alpha",
            "chat-panel",
            "console-command",
            "artifact-a1",
            "web-panel-1",
        ] {
            assert!(
                patterns.iter().any(|p| p.matches(app_webview)),
                "{app_webview}"
            );
        }
        let source = include_str!("host.rs");
        assert!(
            !source.contains(&[".windows", "(["].concat()),
            "a capability scoped by window label would cover the viewers inside that window"
        );
        // And no capability is declared outside this code.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!root.join("capabilities").exists());
        let config: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(root.join("tauri.conf.json")).expect("tauri.conf.json"),
        )
        .expect("config");
        assert!(config["app"]["security"].get("capabilities").is_none());
    }
    #[test]
    fn saved_window_scope_changes_with_runner_authority_without_persisting_credentials() {
        let mut connection = Bootstrap {
            url: "http://localhost:43210".into(),
            token: "private-bearer".into(),
            runner_id: String::new(),
            working_directory: String::new(),
            session_id: String::new(),
        };
        let first = authority(&connection);
        assert_eq!(first.len(), 64);
        assert!(!first.contains(&connection.token));
        connection.token = "another-bearer".into();
        assert_ne!(first, authority(&connection));
        let second = authority(&connection);
        connection.url = "http://localhost:43211".into();
        assert_ne!(second, authority(&connection));
        connection.runner_id = "stable-runner-identity".replace('-', "");
        let stable = authority(&connection);
        connection.token = "refreshed".into();
        connection.url = "http://localhost:54321".into();
        assert_eq!(stable, authority(&connection));
        connection.runner_id = "differentRunnerIdentity".into();
        assert_ne!(stable, authority(&connection));
        let layout = serde_json::to_string(&Layout::default()).expect("layout");
        assert!(!layout.contains("bearer") && !layout.contains("token"));
    }
}

fn sign_in_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw).map_err(|_| "Invalid sign-in URL".to_string())?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(
            url.host_str(),
            Some(
                "auth.openai.com"
                    | "chatgpt.com"
                    | "claude.ai"
                    | "claude.com"
                    | "code.claude.com"
                    | "github.com"
            )
        )
    {
        return Err("Expected an official provider HTTPS sign-in URL".into());
    }
    Ok(url)
}
#[tauri::command]
pub fn open_sign_in(app: tauri::AppHandle, window: Webview, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if window.label() != "main" {
        return Err("Sign-in belongs to the Settings window".into());
    }
    let url = sign_in_url(&url)?;
    app.opener()
        .open_url(url.as_str(), None::<&str>)
        .map_err(|_| "Could not open the sign-in page".into())
}

/// A link a person clicked (in a chat message, an artifact, a PR): opened in
/// their default browser. Only web and mail links; never file:, custom schemes
/// or anything a page could use to start a local program.
fn external_link(raw: &str) -> Result<url::Url, String> {
    let url =
        url::Url::parse(raw.trim()).map_err(|_| "That link is not a valid address".to_string())?;
    if !matches!(url.scheme(), "http" | "https" | "mailto") {
        return Err(format!(
            "Lazybox opens only web and mail links, not {}: links",
            url.scheme()
        ));
    }
    Ok(url)
}
#[tauri::command]
pub fn open_link(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = external_link(&url)?;
    app.opener()
        .open_url(url.as_str(), None::<&str>)
        .map_err(|_| "Could not open the link in your browser".into())
}
// A file link can open media in the person's default app. Resolve symlinks
// before checking the extension and never pass directories or executables on.
fn local_media_path(raw: &str, home: &std::path::Path) -> Option<PathBuf> {
    let path = match raw.strip_prefix("~/") {
        Some(relative) => home.join(relative),
        None if std::path::Path::new(raw).is_absolute() => PathBuf::from(raw),
        None => return None,
    };
    let resolved = path.canonicalize().ok()?;
    let extension = resolved.extension()?.to_str()?.to_ascii_lowercase();
    (resolved.is_file()
        && matches!(
            extension.as_str(),
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "heic"
                | "tif"
                | "tiff"
                | "bmp"
                | "mp4"
                | "mov"
                | "webm"
                | "m4v"
                | "mp3"
                | "m4a"
                | "wav"
                | "flac"
                | "ogg"
        ))
    .then_some(resolved)
}

#[tauri::command]
pub fn open_local_media(app: tauri::AppHandle, path: String) -> Result<bool, String> {
    use tauri_plugin_opener::OpenerExt;
    let home = app.path().home_dir().map_err(|error| error.to_string())?;
    let Some(file) = local_media_path(&path, &home) else {
        return Ok(false);
    };
    let file = file.to_str().ok_or("The media path is not valid UTF-8")?;
    app.opener()
        .open_path(file, None::<&str>)
        .map_err(|_| "Could not open the media file in its default app".to_string())?;
    Ok(true)
}

#[cfg(test)]
mod local_media_tests {
    use super::local_media_path;

    #[test]
    fn opens_home_relative_and_absolute_media_only() {
        let home = tempfile::tempdir().expect("temporary home");
        let video = home.path().join("example.mp4");
        std::fs::write(&video, "").expect("video path");
        let expected = video.canonicalize().ok();
        assert_eq!(local_media_path("~/example.mp4", home.path()), expected);
        assert_eq!(
            local_media_path(video.to_str().expect("UTF-8"), home.path()),
            expected
        );
        let screenshot = home.path().join("compact chat.PNG");
        std::fs::write(&screenshot, "").expect("screenshot path");
        let expected = screenshot.canonicalize().ok();
        assert_eq!(
            local_media_path("~/compact chat.PNG", home.path()),
            expected
        );
        assert_eq!(
            local_media_path(screenshot.to_str().expect("UTF-8"), home.path()),
            expected
        );
        assert!(local_media_path("example.mp4", home.path()).is_none());
        assert!(local_media_path("~/missing.mp4", home.path()).is_none());
        let script = home.path().join("script.sh");
        std::fs::write(&script, "").expect("script path");
        assert!(local_media_path("~/script.sh", home.path()).is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(script, home.path().join("alias.mp4")).expect("symlink");
            assert!(local_media_path("~/alias.mp4", home.path()).is_none());
        }
    }
}

#[cfg(test)]
mod external_link_tests {
    use super::external_link;
    #[test]
    fn only_web_and_mail_links_open() {
        assert!(external_link("https://github.com/org/repo/pull/1").is_ok());
        assert!(external_link("http://localhost:3000/x").is_ok());
        assert!(external_link("mailto:a@b.c").is_ok());
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "vscode://x",
            "ssh://h",
            "not a url",
        ] {
            assert!(external_link(bad).is_err(), "{bad}");
        }
    }
}

/// Private copies for the browser. They are scoped to this app process.
#[derive(Default)]
pub struct HtmlPreviews(Mutex<Option<tempfile::TempDir>>);

#[tauri::command]
pub fn open_html_preview_in_browser(
    app: tauri::AppHandle,
    window: Webview,
    previews: State<'_, HtmlPreviews>,
    html: String,
) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if !is_document_window(window.label()) || html.len() > 4 << 20 {
        return Err("HTML preview is unavailable for this file".into());
    }
    let browser = html_preview_browser()?;
    let mut folder = previews.0.lock().map_err(|error| error.to_string())?;
    if folder.is_none() {
        *folder = Some(tempfile::tempdir().map_err(|error| error.to_string())?);
    }
    let directory = folder
        .as_ref()
        .ok_or("HTML preview folder is unavailable")?;
    let mut file = tempfile::Builder::new()
        .prefix("lazybox-preview-")
        .suffix(".html")
        .tempfile_in(directory.path())
        .map_err(|error| error.to_string())?;
    // Require the same restrictions even if the desktop command is invoked
    // directly by one of our views instead of the file preview button.
    const POLICY: &str = "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'\">";
    file.write_all(POLICY.as_bytes())
        .map_err(|error| error.to_string())?;
    file.write_all(html.as_bytes())
        .map_err(|error| error.to_string())?;
    app.opener()
        .open_path(file.path().to_string_lossy(), browser)
        .map_err(|_| "Could not open the HTML preview in your browser")?;
    file.keep().map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn html_preview_browser() -> Result<Option<String>, String> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSString, NSURL};
    let url = NSURL::URLWithString(&NSString::from_str("https://localhost/"))
        .ok_or("Could not look up the default browser")?;
    NSWorkspace::sharedWorkspace()
        .URLForApplicationToOpenURL(&url)
        .and_then(|application| application.path())
        .map(|path| Some(path.to_string()))
        .ok_or_else(|| "Could not find your default browser".into())
}

#[cfg(not(target_os = "macos"))]
fn html_preview_browser() -> Result<Option<String>, String> {
    Ok(None)
}

fn is_document_window(label: &str) -> bool {
    is_workspace_window(label)
        || ["artifact-", "chat-", "console-"].iter().any(|prefix| {
            label
                .strip_prefix(*prefix)
                .is_some_and(crate::layout::valid_id)
        })
}

#[tauri::command]
pub async fn save_artifact_export(
    app: tauri::AppHandle,
    window: Webview,
    filename: String,
    content: String,
) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;
    if !is_document_window(window.label()) {
        return Err("Only Lazybox document views can save an artifact export".into());
    }
    let (label, extension) = artifact_export_format(&filename)?;
    let parent = window.window();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(path) = app
            .dialog()
            .file()
            .set_parent(&parent)
            .set_title("Save artifact")
            .set_file_name(filename)
            .add_filter(label, &[extension])
            .blocking_save_file()
        else {
            return Ok(false);
        };
        let path = path.into_path().map_err(|_| "Expected a local file")?;
        write_artifact_export(&path, content.as_bytes())
            .map_err(|error| format!("Could not save the file: {error}"))?;
        Ok(true)
    })
    .await
    .map_err(|_| "Could not open the save dialog".to_string())?
}

fn artifact_export_format(filename: &str) -> Result<(&'static str, &'static str), String> {
    let (stem, extension) = filename
        .rsplit_once('.')
        .ok_or("Invalid artifact filename")?;
    if stem.is_empty()
        || stem.len() > 60
        || !stem
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("Invalid artifact filename".into());
    }
    match extension {
        "md" => Ok(("Markdown", "md")),
        "html" => Ok(("HTML", "html")),
        "json" => Ok(("JSON", "json")),
        _ => Err("Unsupported artifact export format".into()),
    }
}

fn write_artifact_export(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    let folder = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    let mut file = tempfile::NamedTempFile::new_in(folder)?;
    file.write_all(content)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod artifact_export_tests {
    use super::*;

    #[test]
    fn filters_export_formats_and_rejects_paths_as_default_names() {
        for name in ["project-plan.md", "project-plan.html", "project-plan.json"] {
            assert!(artifact_export_format(name).is_ok(), "{name}");
        }
        for name in ["../plan.md", "/tmp/plan.md", "plan", "plan.sh", ".md"] {
            assert!(artifact_export_format(name).is_err(), "{name}");
        }
    }

    #[test]
    fn saves_exact_bytes_to_the_chosen_file() {
        let folder = tempfile::tempdir().expect("export folder");
        let path = folder.path().join("chosen-name.md");
        fs::write(&path, "old document").expect("existing file");
        let markdown = "# Résumé\n\nSaved as Markdown.\n";
        write_artifact_export(&path, markdown.as_bytes()).expect("saved export");
        assert_eq!(
            fs::read(&path).expect("read saved file"),
            markdown.as_bytes()
        );
    }
}

#[tauri::command]
pub async fn choose_repository_folder(
    app: tauri::AppHandle,
    window: Webview,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    validate_repository_picker_window(window.label())?;
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title("Choose a folder containing your repositories")
            .blocking_pick_folder()
            .map(|path| {
                path.into_path()
                    .map(|path| path.to_string_lossy().into_owned())
                    .map_err(|_| "Expected a local folder".to_string())
            })
            .transpose()
    })
    .await
    .map_err(|_| "Could not open the folder picker".to_string())?
}

fn validate_repository_picker_window(label: &str) -> Result<(), String> {
    if label == "main"
        || label
            .strip_prefix("workspace-")
            .is_some_and(crate::layout::valid_id)
    {
        Ok(())
    } else {
        Err("Repository discovery belongs to the workspace window".into())
    }
}

#[tauri::command]
pub fn copy_sign_in_code(
    app: tauri::AppHandle,
    window: Webview,
    code: String,
) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    validate_sign_in_code(window.label(), &code)?;
    app.clipboard()
        .write_text(code)
        .map_err(|_| "Could not copy the sign-in code".into())
}

fn validate_sign_in_code(window: &str, code: &str) -> Result<(), String> {
    if window != "main"
        || code.is_empty()
        || code.len() > 128
        || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("Expected a sign-in code in the Settings window".into());
    }
    Ok(())
}

#[cfg(test)]
mod clipboard_tests {
    use super::validate_sign_in_code;
    #[test]
    fn only_settings_can_copy_an_unmodified_device_code() {
        assert!(validate_sign_in_code("main", "ABCD-1234").is_ok());
        for (window, code) in [
            ("chat-one", "ABCD-1234"),
            ("main", ""),
            ("main", "ABCD-1234\n"),
            ("main", "ABCD 1234"),
        ] {
            assert!(validate_sign_in_code(window, code).is_err());
        }
        assert!(validate_sign_in_code("main", &"A".repeat(129)).is_err());
    }
}

#[cfg(test)]
mod repository_picker_tests {
    use super::validate_repository_picker_window;
    #[test]
    fn repository_picker_belongs_to_workspace() {
        assert!(validate_repository_picker_window("main").is_ok());
        for label in ["", "chat", "console", "agent-window"] {
            assert!(validate_repository_picker_window(label).is_err());
        }
    }
}
