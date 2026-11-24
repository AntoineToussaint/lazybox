//! The Web panel's native web views (WORKSPACES.md, "The Web panel").
//!
//! Each one is a child webview of the window its panel is docked in, placed
//! over the panel's box by the app page. A viewer shows the open web, so it
//! is kept apart from the app: its label matches no capability (the app's
//! capabilities name the app's own webviews, never a viewer), a remote page
//! cannot reach any command (Tauri refuses remote origins without a remote
//! capability, and there is none), it never navigates to the app's own
//! origins or to a non-web scheme, it keeps its website data in a store of
//! its own, it never downloads, and it is denied the camera, microphone and
//! other device permissions.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    sync::Mutex,
};
use tauri::{
    Emitter, EventTarget, LogicalPosition, LogicalSize, Manager, Rect, Webview, WebviewUrl,
    webview::{
        DownloadEvent, NewWindowResponse, PageLoadEvent, PermissionResponse, WebviewBuilder,
    },
};
use tauri_plugin_http::reqwest;
use url::Url;

/// Every viewer's label starts with this; no app webview's does.
pub const VIEWER_PREFIX: &str = "viewer-";
const EVENT: &str = "web-view";
/// The viewers' own website data (cookies, storage, cache), apart from the
/// app's: macOS 14 and later. Older macOS has a single store per app, where
/// the app's tauri:// origin and the web's origins still never share data.
const DATA_STORE: [u8; 16] = *b"lazybox-web-view";
const ICON_LIMIT: usize = 256 * 1024;

/// The viewers this app created: label → the window webview that owns it and its id there.
#[derive(Default)]
pub struct Viewers(Mutex<HashMap<String, Owner>>);
struct Owner {
    webview: String,
}

/// A page a viewer may open: http or https on a named host, with no user
/// name or password (`https://bank.example@evil.test` dresses one site up as another).
pub fn page_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|_| "That is not a web address".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "The viewer shows web pages (http and https), not {}: links",
            url.scheme()
        ));
    }
    if url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("That web address has no host, or carries a user name or password".into());
    }
    if is_app_origin(&url) {
        return Err("That address is Lazybox itself".into());
    }
    Ok(url)
}

/// Origins the app itself is served from or talks over: Windows serves the
/// app at http://tauri.localhost, and IPC and assets use their own hosts.
fn is_app_origin(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        ["tauri.localhost", "ipc.localhost", "asset.localhost"]
            .iter()
            .any(|app| host == *app || host.ends_with(&format!(".{app}")))
    })
}

#[derive(Debug, PartialEq)]
pub enum Navigation {
    Allow,
    /// Not shown here: the panel offers to open it in the browser.
    Refuse,
}
/// Whether a viewer goes ahead with a navigation, in any frame of its page.
/// Web pages go ahead, and so do the schemes frames are made of (`about:`,
/// `blob:`, `data:`; WebKit itself refuses a top-level `data:` navigation).
/// The app's own origins, `file:`, and other schemes (`mailto:`, a custom
/// app's) never load here.
pub fn navigation(url: &Url) -> Navigation {
    match url.scheme() {
        "http" | "https" if page_url(url.as_str()).is_ok() => Navigation::Allow,
        "about" | "blob" | "data" => Navigation::Allow,
        _ => Navigation::Refuse,
    }
}

/// A viewer's label: its owner and its id there, hashed, so a window can
/// only ever address its own viewers.
pub fn label(owner: &str, id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{owner}\0{id}").as_bytes());
    format!("{VIEWER_PREFIX}{}", crate::host::hex(&digest[..12]))
}

/// The app webview asking: never a viewer.
fn owner(webview: &Webview) -> Result<String, String> {
    let label = webview.label();
    if label.starts_with(VIEWER_PREFIX) {
        return Err("A web page cannot control the viewer".into());
    }
    Ok(label.to_string())
}
fn view_id(id: &str) -> Result<&str, String> {
    if crate::layout::valid_id(id) {
        Ok(id)
    } else {
        Err("Invalid web view identity".into())
    }
}

/// Where the panel's page area is, in the app page's CSS pixels, and the
/// viewport (`innerWidth` × `innerHeight`) it was measured in.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub viewport_width: f64,
    pub viewport_height: f64,
}

/// The size, in logical points, of the app webview that owns the viewers.
///
/// A viewer is a sibling of that webview in the window's content view, which
/// the app webview fills from its top-left corner; its page's viewport is the
/// whole webview. So the webview's own size is what a CSS pixel is measured
/// against, whatever the app page's zoom (⌘+), the display's scale (Retina or
/// not) or the window's title bar.
///
/// Never the window's `inner_size()`: on macOS, wry replaces the window's
/// content view with the app webview's, and once a window has a second
/// (child) webview Tauri answers `inner_size()` from tao's original view,
/// which stopped following the window at that moment. A window resized,
/// maximized or restored after its first viewer appeared kept reporting the
/// size it was created at, and every viewer was scaled by the difference:
/// drawn too high and too far left, over the panel's own address bar.
pub fn content_size(window: &tauri::Window) -> Result<tauri::PhysicalSize<u32>, String> {
    match window
        .webviews()
        .into_iter()
        .find(|view| view.label() == window.label())
    {
        Some(app) => app.size().map_err(|e| e.to_string()),
        None => window.inner_size().map_err(|e| e.to_string()),
    }
}

/// Less than this past a whole point is a rounding error, not a fraction.
const SNAP: f64 = 1.0 / 64.0;

/// The frame in logical points of the window's content view, which the app
/// webview of size `app` (logical points) fills. The app page may be zoomed
/// (⌘+), so a CSS pixel is the webview's width over the page's viewport.
///
/// The edges are snapped inward to whole points (WebKit places a view on
/// whole points), so a fractional panel edge never lets the page overlap the
/// sidebar, the address bar or the next panel; and the frame never leaves the
/// app webview. `None` when there is no room to show a page.
pub fn logical(frame: &Frame, app: (f64, f64)) -> Option<(LogicalPosition<f64>, LogicalSize<f64>)> {
    let values = [
        frame.x,
        frame.y,
        frame.width,
        frame.height,
        frame.viewport_width,
        frame.viewport_height,
        app.0,
        app.1,
    ];
    if values.iter().any(|v| !v.is_finite())
        || frame.viewport_width < 1.0
        || frame.viewport_height < 1.0
        || frame.width <= 0.0
        || frame.height <= 0.0
        || app.0 < 1.0
        || app.1 < 1.0
    {
        return None;
    }
    let (sx, sy) = (app.0 / frame.viewport_width, app.1 / frame.viewport_height);
    let inward = |edge: f64, bound: f64| (edge - SNAP).ceil().clamp(0.0, bound.floor());
    let outward = |edge: f64, bound: f64| (edge + SNAP).floor().clamp(0.0, bound.floor());
    let left = inward(frame.x * sx, app.0);
    let top = inward(frame.y * sy, app.1);
    let right = outward((frame.x + frame.width) * sx, app.0);
    let bottom = outward((frame.y + frame.height) * sy, app.1);
    (right - left >= 1.0 && bottom - top >= 1.0).then(|| {
        (
            LogicalPosition::new(left, top),
            LogicalSize::new(right - left, bottom - top),
        )
    })
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum Event {
    Loading { id: String, url: String },
    Loaded { id: String, url: String },
    Title { id: String, title: String },
    Icon { id: String, icon: String },
    Blocked { id: String, url: String },
    Download { id: String, url: String },
}
#[derive(Clone)]
struct Reporter {
    app: tauri::AppHandle,
    owner: String,
    id: String,
}
impl Reporter {
    fn send(&self, event: impl FnOnce(String) -> Event) {
        let _ = self.app.emit_to(
            EventTarget::webview(&self.owner),
            EVENT,
            event(self.id.clone()),
        );
    }
}

fn builder(
    app: &tauri::AppHandle,
    label: &str,
    owner: &str,
    id: &str,
    url: Url,
) -> WebviewBuilder<tauri::Wry> {
    let report = Reporter {
        app: app.clone(),
        owner: owner.into(),
        id: id.into(),
    };
    let (navigating, loading, titles, downloads, windows) = (
        report.clone(),
        report.clone(),
        report.clone(),
        report.clone(),
        report.clone(),
    );
    let mut builder = WebviewBuilder::new(label, WebviewUrl::External(url))
        .data_store_identifier(DATA_STORE)
        .zoom_hotkeys_enabled(false)
        .devtools(cfg!(debug_assertions))
        .on_navigation(move |url| match navigation(url) {
            Navigation::Allow => true,
            Navigation::Refuse => {
                navigating.send(|id| Event::Blocked {
                    id,
                    url: url.to_string(),
                });
                false
            }
        })
        .on_page_load(move |view, payload| {
            let url = payload.url().to_string();
            match payload.event() {
                PageLoadEvent::Started => loading.send(|id| Event::Loading { id, url }),
                PageLoadEvent::Finished => {
                    loading.send(|id| Event::Loaded { id, url });
                    page_details(&view, loading.clone(), payload.url().clone());
                }
            }
        })
        .on_document_title_changed(move |_, title| {
            titles.send(|id| Event::Title { id, title });
        })
        // The viewer never writes files: the panel offers the browser instead.
        .on_download(move |_, event| {
            if let DownloadEvent::Requested { url, .. } = event {
                downloads.send(|id| Event::Download {
                    id,
                    url: url.to_string(),
                });
            }
            false
        })
        // A link to a new window opens in this viewer, like a browser with one tab.
        .on_new_window(move |url, _| {
            if let (Ok(page), Some(view)) = (
                page_url(url.as_str()),
                windows.app.get_webview(&label_of(&windows)),
            ) {
                let _ = view.navigate(page);
            }
            NewWindowResponse::Deny
        })
        .on_permission_request(|_, _| PermissionResponse::Deny);
    // Windows and Linux keep a viewer's data in a folder of its own.
    if let Ok(dir) = app.path().app_local_data_dir() {
        builder = builder.data_directory(dir.join("web-viewer"));
    }
    builder
}
fn label_of(report: &Reporter) -> String {
    label(&report.owner, &report.id)
}

/// After a page loads: its title, then its icon as a data: URL.
fn page_details(view: &Webview, report: Reporter, page: Url) {
    const SCRIPT: &str = "(()=>{const l=document.querySelector('link[rel~=\"icon\"],link[rel=\"apple-touch-icon\"]');return JSON.stringify({title:document.title||'',icon:(l&&l.href)||''})})()";
    let _ = view.eval_with_callback(SCRIPT, move |result| {
        #[derive(Deserialize)]
        struct Details {
            title: String,
            icon: String,
        }
        // The script's value is a JSON string, delivered JSON-encoded.
        let Some(details) = serde_json::from_str::<String>(&result)
            .ok()
            .and_then(|text| serde_json::from_str::<Details>(&text).ok())
        else {
            return;
        };
        if !details.title.is_empty() {
            let title: String = details.title.chars().take(300).collect();
            report.send(|id| Event::Title { id, title });
        }
        let report = report.clone();
        let page = page.clone();
        tauri::async_runtime::spawn(async move {
            // A page reached by a LAN or tailnet name is on this network too.
            let resolved = page.clone();
            let page_local =
                tauri::async_runtime::spawn_blocking(move || page_is_local(&resolved, resolve))
                    .await
                    .unwrap_or(false);
            let Some(icon) = page_icon(&page, &details.icon, page_local) else {
                return;
            };
            if let Ok(icon) = fetch_icon(icon, page_local).await {
                report.send(|id| Event::Icon { id, icon });
            }
        });
    });
}

/// An address of this machine or a network it is on rather than the web:
/// loopback, unspecified, private, link-local, shared (CGNAT, which Tailscale
/// uses) and broadcast IPv4; loopback, unspecified, unique-local (fc00::/7),
/// link-local (fe80::/10) and site-local (fec0::/10) IPv6, and an IPv6 form
/// of a local IPv4 (mapped, compatible, NAT64).
fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            let embedded = ip.to_ipv4_mapped().or_else(|| {
                let s = ip.segments();
                // ::a.b.c.d (compatible) and 64:ff9b::a.b.c.d (NAT64).
                ((s[..6] == [0; 6] && !ip.is_loopback() && !ip.is_unspecified())
                    || s[..6] == [0x64, 0xff9b, 0, 0, 0, 0])
                .then(|| Ipv4Addr::from(((s[6] as u32) << 16) | s[7] as u32))
            });
            ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first & 0xffc0) == 0xfec0
                || embedded.is_some_and(|v4| local_ip(IpAddr::V4(v4)))
        }
    }
}

/// This machine or its local network, by the address's own text: a local
/// name, or a local IP. A name that resolves to a local address is caught
/// when it is resolved (`PublicOnly`).
fn is_local(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.');
            name == "localhost" || name.ends_with(".localhost") || name.ends_with(".local")
        }
        Some(url::Host::Ipv4(ip)) => local_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => local_ip(IpAddr::V6(ip)),
        None => false,
    }
}

/// Whether a page is on this machine or a network it is on: by its address's
/// text, or by what its name resolves to (`nas.lan` on the LAN, a tailnet's
/// MagicDNS name), with the same test as an icon's address. Such a page's
/// icons may come from its network, as a page at a local IP's may.
fn page_is_local(page: &Url, resolve: impl FnOnce(&str) -> Vec<IpAddr>) -> bool {
    if is_local(page) {
        return true;
    }
    match page.host() {
        Some(url::Host::Domain(name)) => resolve(name).into_iter().any(local_ip),
        _ => false,
    }
}

/// The addresses a name resolves to now; none when it does not resolve.
fn resolve(name: &str) -> Vec<IpAddr> {
    (name, 0)
        .to_socket_addrs()
        .map(|addrs| addrs.map(|addr| addr.ip()).collect())
        .unwrap_or_default()
}

/// Resolves a name for an icon of a page on the web, and refuses it when any
/// address it resolves to is local: the connection uses exactly the
/// addresses checked, so a name that points at this machine (or is made to,
/// between two lookups) never reaches it.
struct PublicOnly;

type ResolveError = Box<dyn std::error::Error + Send + Sync>;

fn public_only(addrs: Vec<SocketAddr>) -> Result<reqwest::dns::Addrs, ResolveError> {
    if addrs.is_empty() {
        return Err("the name resolves to no address".into());
    }
    if addrs.iter().any(|addr| local_ip(addr.ip())) {
        return Err("the name resolves to an address on this machine or its network".into());
    }
    Ok(Box::new(addrs.into_iter()))
}

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = tauri::async_runtime::spawn_blocking(move || {
                (host.as_str(), 0).to_socket_addrs().map(Vec::from_iter)
            })
            .await
            .map_err(|e| -> ResolveError { e.to_string().into() })??;
            public_only(addrs)
        })
    }
}

/// Whether an icon request follows a redirect to `next`, the `hops`-th: a
/// web page's icon never moves to this machine or its network, and three
/// redirects are enough.
fn follows(page_local: bool, next: &Url, hops: usize) -> bool {
    hops <= 3 && page_url(next.as_str()).is_ok() && (page_local || !is_local(next))
}
/// The icon a page names, else its /favicon.ico. A page on the web never
/// gets the app to fetch from this machine or the local network.
#[cfg(test)]
fn icon_url(page: &Url, named: &str) -> Option<Url> {
    page_icon(page, named, is_local(page))
}
/// `icon_url` for a page already known to be local or not (`page_is_local`).
fn page_icon(page: &Url, named: &str, page_local: bool) -> Option<Url> {
    let icon = if named.is_empty() {
        page.join("/favicon.ico").ok()?
    } else {
        page.join(named).ok()?
    };
    let icon = page_url(icon.as_str()).ok()?;
    (!is_local(&icon) || page_local).then_some(icon)
}
/// What an icon's bytes are, by its declared type or its first bytes.
pub fn icon_type(declared: &str, bytes: &[u8]) -> Option<&'static str> {
    let declared = declared.split(';').next().unwrap_or("").trim();
    let sniffed = if bytes.starts_with(b"\x89PNG") {
        Some("image/png")
    } else if bytes.starts_with(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    };
    sniffed.or(match declared {
        // An SVG in an <img> runs no script; anything else unrecognised is not shown.
        "image/svg+xml" => Some("image/svg+xml"),
        _ => None,
    })
}
/// Fetches an icon. For a page on the web (`page_local` false) every name is
/// resolved by `PublicOnly` and every redirect is checked by `follows`, so
/// neither the icon's address nor where it leads reaches this machine.
async fn fetch_icon(url: Url, page_local: bool) -> Result<String, String> {
    let mut client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if follows(page_local, attempt.url(), attempt.previous().len()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    if !page_local {
        client = client.dns_resolver(std::sync::Arc::new(PublicOnly));
    }
    let client = client.build().map_err(|e| e.to_string())?;
    let mut response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err("no icon".into());
    }
    let declared = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > ICON_LIMIT {
            return Err("icon too large".into());
        }
    }
    let kind = icon_type(&declared, &bytes).ok_or("not an image")?;
    Ok(format!(
        "data:{kind};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

fn viewer(app: &tauri::AppHandle, owner: &str, id: &str) -> Result<Webview, String> {
    app.get_webview(&label(owner, view_id(id)?))
        .ok_or_else(|| "That web view is closed".into())
}

/// Places the viewer `id` over its panel and shows it, creating it on `url`
/// the first time. With a `url` an existing viewer on another page goes there.
///
/// `popout` names the popped-out panel window the panel is in now (the token
/// in its `popout.html?w=` address): the view moves there, page and all
/// (`Webview::reparent`), and the frame was measured in that window. Only a
/// window this owner opened is accepted.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn web_view_show(
    app: tauri::AppHandle,
    webview: Webview,
    viewers: tauri::State<'_, Viewers>,
    id: String,
    url: String,
    frame: Frame,
    zoom: f64,
    popout: Option<String>,
) -> Result<(), String> {
    let owner = owner(&webview)?;
    let label = label(&owner, view_id(&id)?);
    let (window, measured) = match popout.as_deref().filter(|token| !token.is_empty()) {
        Some(token) => {
            let window = popout_window(&app, &owner, token)?;
            let scale = window.scale_factor().map_err(|e| e.to_string())?;
            // The popped-out page the frame was measured in fills its window.
            let size = content_size(&window)?.to_logical::<f64>(scale);
            (window, size)
        }
        None => {
            let window = webview.window();
            let scale = window.scale_factor().map_err(|e| e.to_string())?;
            // The webview asking is the page the frame was measured in.
            let size = webview
                .size()
                .map_err(|e| e.to_string())?
                .to_logical::<f64>(scale);
            (window, size)
        }
    };
    let (position, size) = logical(&frame, (measured.width, measured.height))
        .ok_or("The panel has no room to show a page")?;
    if let Some(view) = app.get_webview(&label) {
        if view.window().label() != window.label() {
            // Moved with its page: its history, scroll and forms come along.
            view.reparent(&window).map_err(|e| e.to_string())?;
        }
        view.set_bounds(Rect {
            position: position.into(),
            size: size.into(),
        })
        .map_err(|e| e.to_string())?;
        view.show().map_err(|e| e.to_string())?;
        if !url.is_empty() {
            let page = page_url(&url)?;
            if view.url().map_or(true, |current| current != page) {
                view.navigate(page).map_err(|e| e.to_string())?;
            }
        }
        return Ok(());
    }
    let page = page_url(&url)?;
    let view = window
        .add_child(builder(&app, &label, &owner, &id, page), position, size)
        .map_err(|e| format!("Could not open the web view: {e}"))?;
    if (zoom - 1.0).abs() > f64::EPSILON {
        view.set_zoom(zoom.clamp(0.25, 3.0))
            .map_err(|e| e.to_string())?;
    }
    viewers
        .0
        .lock()
        .map_err(|e| e.to_string())?
        .insert(label, Owner { webview: owner });
    Ok(())
}
#[tauri::command]
pub async fn web_view_hide(
    app: tauri::AppHandle,
    webview: Webview,
    id: String,
) -> Result<(), String> {
    match viewer(&app, &owner(&webview)?, &id) {
        Ok(view) => view.hide().map_err(|e| e.to_string()),
        Err(_) => Ok(()),
    }
}
#[tauri::command]
pub async fn web_view_navigate(
    app: tauri::AppHandle,
    webview: Webview,
    id: String,
    url: String,
) -> Result<(), String> {
    let page = page_url(&url)?;
    viewer(&app, &owner(&webview)?, &id)?
        .navigate(page)
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn web_view_zoom(
    app: tauri::AppHandle,
    webview: Webview,
    id: String,
    zoom: f64,
) -> Result<(), String> {
    if !zoom.is_finite() {
        return Err("Invalid zoom".into());
    }
    viewer(&app, &owner(&webview)?, &id)?
        .set_zoom(zoom.clamp(0.25, 3.0))
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn web_view_action(
    app: tauri::AppHandle,
    webview: Webview,
    viewers: tauri::State<'_, Viewers>,
    id: String,
    action: String,
) -> Result<(), String> {
    let owner = owner(&webview)?;
    if action == "close" {
        if let Ok(view) = viewer(&app, &owner, &id) {
            viewers
                .0
                .lock()
                .map_err(|e| e.to_string())?
                .remove(view.label());
            view.close().map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    let view = viewer(&app, &owner, &id)?;
    match action.as_str() {
        "reload" => view.reload(),
        "stop" => view.eval("window.stop()"),
        "focus" => view.set_focus(),
        _ => return Err("Unknown web view action".into()),
    }
    .map_err(|e| e.to_string())
}
/// A page that (re)loads starts with no viewers: any left from before belong to no panel.
#[tauri::command]
pub async fn web_view_reset(
    app: tauri::AppHandle,
    webview: Webview,
    viewers: tauri::State<'_, Viewers>,
) -> Result<(), String> {
    let owner = owner(&webview)?;
    forget(&app, &viewers, &owner);
    Ok(())
}
/// Popped-out panel windows by the token in their address. Which app window
/// opened each is the host's record (`host::popout_opener`).
static POPOUTS: Mutex<std::collections::BTreeMap<String, String>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Whether `token` can name a popped-out window: what the page put in its
/// address, nothing longer or stranger.
pub fn popout_token(token: &str) -> bool {
    (8..=64).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Records the popped-out window `label` opened on `token`.
pub fn register_popout(token: &str, label: &str) {
    if popout_token(token)
        && let Ok(mut map) = POPOUTS.lock()
    {
        map.insert(token.to_string(), label.to_string());
    }
}

/// Forgets the popped-out window `label`, closed.
pub fn forget_popout(label: &str) {
    if let Ok(mut map) = POPOUTS.lock() {
        map.retain(|_, window| window != label);
    }
}

/// The label of the popped-out window `token` names, if `owner` opened it
/// (`opener_of`, the host's record): a window can move its views only into
/// its own panel windows.
fn popout_label(
    owner: &str,
    token: &str,
    opener_of: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let label = POPOUTS.lock().ok()?.get(token).cloned()?;
    (opener_of(&label).as_deref() == Some(owner)).then_some(label)
}

/// The popped-out window `token` names, if `owner` opened it.
fn popout_window(
    app: &tauri::AppHandle,
    owner: &str,
    token: &str,
) -> Result<tauri::Window, String> {
    popout_label(owner, token, crate::host::popout_opener)
        .and_then(|label| app.get_window(&label))
        .ok_or_else(|| "That panel window is closed".into())
}

/// Brings the views in the popped-out window `label` back to the window of
/// the app page that owns them, hidden: closing a panel window must not
/// close the pages its Web panels showed (they show again where the panels
/// are put back).
pub fn reclaim(app: &tauri::AppHandle, viewers: &Viewers, label: &str) {
    let owned: Vec<(String, String)> = match viewers.0.lock() {
        Ok(map) => map
            .iter()
            .map(|(view, o)| (view.clone(), o.webview.clone()))
            .collect(),
        Err(_) => return,
    };
    for (view, owner) in owned {
        let (Some(view), Some(home)) = (app.get_webview(&view), app.get_webview(&owner)) else {
            continue;
        };
        if view.window().label() == label {
            let _ = view.hide();
            let _ = view.reparent(&home.window());
        }
    }
}

/// The opener closes a popped-out window: its pages come home first.
#[tauri::command]
pub async fn web_view_reclaim(
    app: tauri::AppHandle,
    webview: Webview,
    viewers: tauri::State<'_, Viewers>,
    popout: String,
) -> Result<(), String> {
    let owner = owner(&webview)?;
    let window = popout_window(&app, &owner, &popout)?;
    reclaim(&app, &viewers, window.label());
    Ok(())
}

/// Closes every viewer of a window webview that is going away or starting over.
pub fn forget(app: &tauri::AppHandle, viewers: &Viewers, owner: &str) {
    let labels: Vec<String> = match viewers.0.lock() {
        Ok(mut map) => {
            let labels: Vec<String> = map
                .iter()
                .filter(|(_, o)| o.webview == owner)
                .map(|(label, _)| label.clone())
                .collect();
            for label in &labels {
                map.remove(label);
            }
            labels
        }
        Err(_) => return,
    };
    for label in labels {
        if let Some(view) = app.get_webview(&label) {
            let _ = view.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panel_window_is_addressed_by_its_token_and_only_by_its_opener() {
        let token = "0b6f1c9e-8c0a-4f1e-9d2b-1a2b3c4d5e6f";
        assert!(popout_token(token));
        for bad in ["", "short", "has space here", "../../etc", &"x".repeat(65)] {
            assert!(!popout_token(bad), "{bad:?}");
        }
        let opened_by_main = |label: &str| (label == "popout-7").then(|| "main".to_string());
        register_popout(token, "popout-7");
        assert_eq!(
            popout_label("main", token, opened_by_main).as_deref(),
            Some("popout-7")
        );
        // Another app window cannot move its views into a window it did not open.
        assert_eq!(popout_label("workspace-2", token, opened_by_main), None);
        // A token the page made up names nothing.
        register_popout("../../etc", "popout-8");
        assert_eq!(
            popout_label("main", "../../etc", |_| Some("main".into())),
            None
        );
        forget_popout("popout-7");
        assert_eq!(popout_label("main", token, opened_by_main), None);
    }

    #[test]
    fn viewers_open_only_web_pages_on_a_named_host() {
        for ok in [
            "https://github.com/org/repo/pull/1",
            "http://localhost:3000/",
            "http://127.0.0.1:20000/app?x=1",
            "https://[::1]:8443/",
            " https://example.com/a b ",
        ] {
            assert!(page_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>x</script>",
            "mailto:a@b.c",
            "vscode://open",
            "tauri://localhost/index.html",
            "ipc://localhost/cmd",
            "http://tauri.localhost/",
            "http://ipc.localhost/plugin:x",
            "https://asset.localhost/x",
            "https://user:secret@example.com/",
            "https://bank.example@evil.test/",
            "https://",
            "not a url",
            "",
        ] {
            assert!(page_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn navigation_inside_a_viewer_stays_on_the_web() {
        let parse = |s: &str| Url::parse(s).expect(s);
        for ok in [
            "https://example.com/",
            "http://localhost:5173/",
            "about:blank",
            "about:srcdoc",
            "blob:https://example.com/0f0e",
            "data:image/png;base64,AAAA",
        ] {
            assert_eq!(navigation(&parse(ok)), Navigation::Allow, "{ok}");
        }
        for refused in [
            "file:///etc/passwd",
            "mailto:a@b.c",
            "tel:+15550100",
            "zoommtg://zoom.us/join",
            "tauri://localhost/",
            "http://tauri.localhost/",
            "https://ipc.localhost/",
            "https://user:pw@example.com/",
        ] {
            assert_eq!(navigation(&parse(refused)), Navigation::Refuse, "{refused}");
        }
    }

    #[test]
    fn a_window_addresses_only_its_own_viewers() {
        let main = label("main", "web-1");
        assert!(main.starts_with(VIEWER_PREFIX));
        assert_eq!(main, label("main", "web-1"));
        assert_ne!(main, label("workspace-alpha", "web-1"));
        assert_ne!(main, label("main", "web-2"));
        // Owner and id cannot be shifted into each other.
        assert_ne!(label("ab", "c"), label("a", "bc"));
        assert!(view_id("web-0f3c_a").is_ok());
        assert!(view_id("../x").is_err() && view_id("").is_err());
    }

    fn frame(x: f64, y: f64, width: f64, height: f64, viewport: (f64, f64)) -> Frame {
        Frame {
            x,
            y,
            width,
            height,
            viewport_width: viewport.0,
            viewport_height: viewport.1,
        }
    }
    fn placed(frame: &Frame, app: (f64, f64)) -> (f64, f64, f64, f64) {
        let (p, s) = logical(frame, app).expect("placed");
        (p.x, p.y, s.width, s.height)
    }

    #[test]
    fn a_frame_is_measured_against_the_app_webview_not_a_stale_window_size() {
        // The page area of a Web panel under its address bar, in a 1512 × 916
        // window (a maximized 14-inch MacBook): the native page goes exactly there.
        let page = frame(262.0, 113.0, 700.0, 760.0, (1512.0, 916.0));
        assert_eq!(placed(&page, (1512.0, 916.0)), (262.0, 113.0, 700.0, 760.0));
        // What scaling by the window's `inner_size()` did once the window had
        // been resized after its first viewer: that size is the one the window
        // was created at (here 1500 × 640), so the page was drawn 34 points too
        // high, over the address bar, and 2 points too far left, over the
        // sidebar's edge.
        let (x, y, ..) = placed(&page, (1500.0, 640.0));
        assert_eq!((262.0 - x, 113.0 - y), (2.0, 34.0));
    }

    #[test]
    fn a_frame_is_the_same_on_retina_and_standard_displays() {
        // `web_view_show` reads the app webview's size in physical pixels and
        // converts it with the window's scale: a window moved between a Retina
        // and a standard display keeps every viewer where its panel is.
        let page = frame(262.5, 113.25, 700.5, 760.0, (1512.0, 916.0));
        let standard = tauri::PhysicalSize::new(1512_u32, 916).to_logical::<f64>(1.0);
        let retina = tauri::PhysicalSize::new(3024_u32, 1832).to_logical::<f64>(2.0);
        let one = placed(&page, (standard.width, standard.height));
        assert_eq!(one, placed(&page, (retina.width, retina.height)));
        // Fractional CSS edges are snapped inward: never over the sidebar
        // (262.5 → 263) or the address bar (113.25 → 114), never past the
        // panel (963 stays, 873.25 → 873).
        assert_eq!(one, (263.0, 114.0, 700.0, 759.0));
        // A float's rounding error is not a fraction.
        let close = frame(99.999_999, 50.000_001, 400.000_001, 300.0, (1000.0, 800.0));
        assert_eq!(placed(&close, (1000.0, 800.0)), (100.0, 50.0, 400.0, 300.0));
    }

    #[test]
    fn a_zoomed_app_page_scales_by_its_own_viewport() {
        // ⌘+ to 110% in a 1512 × 916 webview: WebKit reports a whole-pixel
        // 1375 × 833 CSS viewport for the exact 1374.5 × 832.7.
        let page = frame(240.0, 103.0, 636.0, 691.0, (1375.0, 833.0));
        let (x, y, w, h) = placed(&page, (1512.0, 916.0));
        // Within a point of the exact 110% box (264, 113.3, 699.6 × 760.1), and inside it.
        assert!(
            (x - 264.0).abs() <= 1.0 && (y - 113.3).abs() <= 1.0,
            "{x},{y}"
        );
        assert!(x + w <= 963.6 && y + h <= 873.4, "{w}×{h}");
        assert!(w >= 698.0 && h >= 758.0, "{w}×{h}");
    }

    #[test]
    fn a_frame_follows_the_app_pages_zoom_and_stays_in_the_window() {
        let frame = frame(100.0, 50.0, 400.0, 300.0, (1000.0, 800.0));
        // Unzoomed: CSS pixels are logical points.
        assert_eq!(placed(&frame, (1000.0, 800.0)), (100.0, 50.0, 400.0, 300.0));
        // The app page zoomed to 125%: the same CSS box is larger on screen,
        // and the half point is snapped inward (62.5 → 63, 437.5 → 437).
        assert_eq!(
            placed(&frame, (1250.0, 1000.0)),
            (125.0, 63.0, 500.0, 374.0)
        );
        // Never past the app webview's edge.
        let wide = Frame {
            width: 5000.0,
            ..frame.clone()
        };
        assert_eq!(placed(&wide, (1000.0, 800.0)), (100.0, 50.0, 900.0, 300.0));
        for bad in [
            Frame {
                height: 0.0,
                ..frame.clone()
            },
            Frame {
                viewport_width: 0.0,
                ..frame.clone()
            },
            Frame {
                x: f64::NAN,
                ..frame.clone()
            },
            Frame {
                x: 2000.0,
                ..frame.clone()
            },
        ] {
            assert!(logical(&bad, (1000.0, 800.0)).is_none());
        }
    }

    #[test]
    fn icons_come_from_the_page_and_never_from_this_machine_for_a_web_page() {
        let page = Url::parse("https://example.com/docs/page").expect("page");
        assert_eq!(
            icon_url(&page, "").map(String::from),
            Some("https://example.com/favicon.ico".to_string())
        );
        assert_eq!(
            icon_url(&page, "/static/icon.png").map(String::from),
            Some("https://example.com/static/icon.png".to_string())
        );
        assert!(icon_url(&page, "https://cdn.example.net/i.svg").is_some());
        for refused in [
            "http://127.0.0.1:43210/lazybox.bench.v1.RunnerService/",
            "http://localhost:8080/admin",
            "http://192.168.1.1/",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            assert!(icon_url(&page, refused).is_none(), "{refused}");
        }
        // A page on this machine may use its own icon.
        let local = Url::parse("http://localhost:3000/").expect("local");
        assert!(icon_url(&local, "/favicon.ico").is_some());
    }

    #[test]
    fn local_addresses_include_ipv6_private_ranges() {
        for local in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.1.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "::",
            "fd12:3456::1",
            "fc00::1",
            "fe80::1",
            "fec0::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
            "::127.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(local_ip(local.parse().unwrap()), "{local}");
        }
        for public in [
            "93.184.216.34",
            "1.1.1.1",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:93.184.216.34",
        ] {
            assert!(!local_ip(public.parse().unwrap()), "{public}");
        }
        let page = Url::parse("https://example.com/").unwrap();
        for refused in [
            "http://[fd00::1]/i.png",
            "http://[fe80::1]/i.png",
            "http://[::ffff:127.0.0.1]/i.png",
            "http://100.64.0.1/i.png",
            "http://printer.local/i.png",
            "http://localhost./i.png",
        ] {
            assert!(icon_url(&page, refused).is_none(), "{refused}");
        }
    }

    /// A page reached by a LAN or tailnet name is a local page: its own icon
    /// (on its own network) is fetched, as for a page at a local IP. Deciding
    /// from the text alone refused it.
    #[test]
    fn a_page_on_a_lan_or_tailnet_name_keeps_its_icon() {
        let lookup = |name: &str| -> Vec<IpAddr> {
            match name {
                "nas.lan" => vec!["192.168.1.20".parse().unwrap()],
                "studio.tail1234.ts.net" => vec!["100.101.102.103".parse().unwrap()],
                "router.home" => vec!["fd00::1".parse().unwrap()],
                "example.com" => vec!["93.184.216.34".parse().unwrap()],
                _ => vec![],
            }
        };
        for page in [
            "http://nas.lan/",
            "https://studio.tail1234.ts.net/app",
            "http://router.home/",
            "http://localhost:3000/",
            "http://192.168.1.20/",
        ] {
            let page = Url::parse(page).unwrap();
            assert!(page_is_local(&page, lookup), "{page}");
            assert!(page_icon(&page, "", true).is_some(), "{page}");
        }
        for page in ["https://example.com/", "https://unknown.example/"] {
            assert!(!page_is_local(&Url::parse(page).unwrap(), lookup), "{page}");
        }
        // Its icon may be on its network; a web page's still never is.
        let nas = Url::parse("http://nas.lan/").unwrap();
        assert!(page_icon(&nas, "http://192.168.1.20/icon.png", true).is_some());
        let web = Url::parse("https://example.com/").unwrap();
        assert!(page_icon(&web, "http://192.168.1.20/icon.png", false).is_none());
    }

    #[test]
    fn a_name_that_resolves_to_this_machine_is_refused() {
        let at = |ip: &str| SocketAddr::new(ip.parse().unwrap(), 0);
        assert!(public_only(vec![at("93.184.216.34")]).is_ok());
        assert!(public_only(vec![at("127.0.0.1")]).is_err());
        assert!(public_only(vec![at("fd00::1")]).is_err());
        // One local address among public ones is enough to refuse the name.
        assert!(public_only(vec![at("93.184.216.34"), at("10.0.0.1")]).is_err());
        assert!(public_only(vec![]).is_err());
    }

    #[test]
    fn a_web_pages_icon_never_redirects_to_this_machine() {
        let next = |raw: &str| Url::parse(raw).unwrap();
        assert!(follows(
            false,
            &next("https://www.example.com/favicon.ico"),
            1
        ));
        assert!(!follows(false, &next("http://127.0.0.1:43210/"), 1));
        assert!(!follows(false, &next("http://[fd00::1]/"), 1));
        assert!(!follows(false, &next("file:///etc/passwd"), 1));
        assert!(
            !follows(false, &next("https://www.example.com/i.png"), 4),
            "three are enough"
        );
        assert!(
            follows(true, &next("http://127.0.0.1:43210/i.png"), 1),
            "a local page's own"
        );
    }

    /// One HTTP/1.1 server on 127.0.0.1: `/icon.png` is a PNG, `/moved` redirects there.
    fn icon_server() -> u16 {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                    header.clear();
                }
                let mut stream = stream;
                let body = b"\x89PNG\r\n\x1a\n";
                let _ = if line.starts_with("GET /moved ") {
                    write!(
                        stream,
                        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/icon.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                        .and_then(|_| stream.write_all(body))
                };
            }
        });
        port
    }

    #[test]
    fn a_web_pages_icon_is_never_fetched_from_this_machine() {
        let port = icon_server();
        let url = |raw: String| Url::parse(&raw).unwrap();
        tauri::async_runtime::block_on(async {
            // A name that resolves to this machine: refused when resolved.
            let named = url(format!("http://localhost:{port}/icon.png"));
            assert!(fetch_icon(named.clone(), false).await.is_err());
            assert!(fetch_icon(named, true).await.is_ok(), "a local page's own");
            // A redirect to this machine is not followed.
            let moved = url(format!("http://127.0.0.1:{port}/moved"));
            assert!(fetch_icon(moved.clone(), false).await.is_err());
            assert!(
                fetch_icon(moved, true)
                    .await
                    .is_ok_and(|icon| icon.starts_with("data:image/png;base64,"))
            );
        });
    }

    #[test]
    fn only_images_become_icons() {
        assert_eq!(icon_type("", b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(
            icon_type("application/octet-stream", &[0, 0, 1, 0, 1]),
            Some("image/x-icon")
        );
        assert_eq!(
            icon_type("image/svg+xml; charset=utf-8", b"<svg/>"),
            Some("image/svg+xml")
        );
        assert_eq!(icon_type("text/html", b"<html>"), None);
        assert_eq!(icon_type("image/png", b"<html>not a png"), None);
    }
}
