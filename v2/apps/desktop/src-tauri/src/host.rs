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
};
use tauri::{
    Emitter, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
    ipc::CapabilityBuilder,
    menu::{Menu, MenuItem, Submenu},
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
}

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

fn connection(app: &tauri::AppHandle) -> Result<(Bootstrap, PathBuf), Box<dyn std::error::Error>> {
    let LaunchOptions {
        endpoint,
        project,
        session,
    } = launch_options()?;
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
    let (connection, endpoint) = connection(app.handle())?;
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
            .windows(["main", "workspace-*", "console-*", "chat-*"])
            .permission("core:default")
            .permission_scoped(
                "http:default",
                vec![allowed],
                Vec::<serde_json::Value>::new(),
            ),
    )?;
    let views = crate::viewstate::Repository::open(&app.path().app_local_data_dir()?, &authority)?;
    app.manage(Host {
        connection: Mutex::new(connection),
        reload_endpoint: Some(endpoint),
        authority,
        repository,
        views: Mutex::new(views),
        layout: Mutex::new(layout),
        quitting: AtomicBool::new(false),
        notice: Mutex::new(notice),
    });
    let menu = Menu::default(app.handle())?;
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
    menu.append(&surfaces)?;
    app.set_menu(menu)?;
    for (_, label) in restore {
        open(app.handle(), &label)?;
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
    let inner = window.inner_size().map_err(|e| e.to_string())?;
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
    // No layout lock may be held while calling a native window API.
    let surface: Surface = {
        let host = app.state::<Host>();
        let layout = host.layout.lock().map_err(|e| e.to_string())?;
        layout
            .surfaces
            .get(label)
            .cloned()
            .ok_or("Unknown window surface")?
    };
    let displays = monitors(app)?;
    let placement = placement::fit(surface.geometry.as_ref(), &displays)?;
    if let Some(window) = app.get_webview_window(label) {
        if !window.is_maximized().map_err(|e| e.to_string())?
            && !window.is_fullscreen().map_err(|e| e.to_string())?
        {
            window
                .set_size(tauri::LogicalSize::new(placement.width, placement.height))
                .map_err(|e| e.to_string())?;
            window
                .set_position(tauri::PhysicalPosition::new(placement.x, placement.y))
                .map_err(|e| e.to_string())?;
        }
        window.show().map_err(|e| e.to_string())?;
        window.unminimize().map_err(|e| e.to_string())?;
        return window.set_focus().map_err(|e| e.to_string());
    }
    mutate(app, |layout| layout.mark_open(label))?;
    let name = app.config().product_name.as_deref().unwrap_or("Lazybox");
    let kind = if label == "main" {
        "Workspaces"
    } else if label.starts_with("workspace-") {
        "Workspace"
    } else if label.starts_with("chat-") {
        "Chat"
    } else {
        "Console"
    };
    let window = match WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))
        // Route file drops to the shared chat attachment flow (HTML5 File API).
        .disable_drag_drop_handler()
        .title(format!("{name} — {kind}"))
        .visible(false)
        .inner_size(placement.width, placement.height)
        .min_inner_size(placement.min_width, placement.min_height)
        .build()
    {
        Ok(window) => window,
        Err(error) => {
            let _ = mutate(app, |layout| layout.close(label));
            return Err(error.to_string());
        }
    };
    window
        .set_position(tauri::PhysicalPosition::new(placement.x, placement.y))
        .map_err(|e| e.to_string())?;
    if surface.maximized {
        window.maximize().map_err(|e| e.to_string())?;
    }
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn runner_bootstrap(
    window: WebviewWindow,
    host: State<'_, Host>,
) -> Result<WindowBootstrap, String> {
    let layout = host.layout.lock().map_err(|e| e.to_string())?;
    let surface = layout
        .surfaces
        .get(window.label())
        .ok_or("Unknown window surface")?;
    Ok(WindowBootstrap {
        view_state: host.views.lock().map_err(|e| e.to_string())?.values.clone(),
        connection: match &host.reload_endpoint {
            Some(path) => host
                .connection
                .lock()
                .map_err(|e| e.to_string())?
                .refreshed(path)
                .map_err(|e| e.to_string())?,
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
        } else if !surface.target.panel_id.is_empty() {
            "chat"
        } else {
            "console"
        },
        notice: host.notice.lock().map_err(|e| e.to_string())?.clone(),
    })
}
#[tauri::command]
pub fn save_surface(
    app: tauri::AppHandle,
    window: WebviewWindow,
    target: Target,
) -> Result<(), String> {
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
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
        let _ = mutate(&app, |layout| layout.close(&label));
        return Err(error);
    }
    Ok(())
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
#[tauri::command]
pub fn dock_chat(app: tauri::AppHandle, panel_id: String) -> Result<(), String> {
    if !crate::layout::valid_id(&panel_id) {
        return Err("Invalid chat panel identity".into());
    }
    let label = format!("chat-{panel_id}");
    if let Some(window) = app.get_webview_window(&label) {
        window.close().map_err(|error| error.to_string())?;
    }
    mutate(&app, |layout| {
        if layout.surfaces.contains_key(&label) {
            layout.close(&label)?;
        }
        Ok(())
    })?;
    open(&app, "main")
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
pub fn reconnect_view(window: WebviewWindow, host: State<'_, Host>) -> Result<(), String> {
    if let Some(path) = &host.reload_endpoint {
        let mut current = host.connection.lock().map_err(|e| e.to_string())?;
        let next = current.reconnected(path).map_err(|e| e.to_string())?;
        if next.url != current.url {
            window
                .app_handle()
                .add_capability(
                    CapabilityBuilder::new(format!(
                        "reconnected-{:x}",
                        Sha256::digest(next.url.as_bytes())
                    ))
                    .windows(["main", "workspace-*", "console-*", "chat-*"])
                    .permission_scoped(
                        "http:default",
                        vec![serde_json::json!({"url":format!("{}{RPC_PATH}*",next.url)})],
                        Vec::<serde_json::Value>::new(),
                    ),
                )
                .map_err(|e| e.to_string())?;
        }
        *current = next;
    }
    window.reload().map_err(|e| e.to_string())
}

pub fn window_event(window: &tauri::Window, event: &WindowEvent) {
    let app = window.app_handle();
    let Some(host) = app.try_state::<Host>() else {
        return;
    };
    if host.quitting.load(Ordering::SeqCst) {
        return;
    }
    let label = window.label();
    let result = match event {
        WindowEvent::CloseRequested { .. } => {
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
            if let Some(window) = survivor.and_then(|label| app.get_webview_window(&label)) {
                let _ = window.set_focus();
            }
            Ok(())
        }
        _ => Ok(()),
    };
    if let Err(error) = result {
        report(app, error);
    }
}

pub fn menu_event(app: &tauri::AppHandle, id: &str) {
    let result = match id {
        "reopen-agent" => open(app, "main"),
        "reopen-closed" => reopen_closed(app.clone()),
        "reload-view" => app
            .webview_windows()
            .values()
            .find(|w| w.is_focused().unwrap_or(false))
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
pub fn open_sign_in(
    app: tauri::AppHandle,
    window: WebviewWindow,
    url: String,
) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if window.label() != "main" {
        return Err("Sign-in belongs to the Settings window".into());
    }
    let url = sign_in_url(&url)?;
    app.opener()
        .open_url(url.as_str(), None::<&str>)
        .map_err(|_| "Could not open the sign-in page".into())
}

#[tauri::command]
pub async fn choose_repository_folder(
    app: tauri::AppHandle,
    window: WebviewWindow,
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
    window: WebviewWindow,
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
