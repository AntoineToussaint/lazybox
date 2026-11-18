//! No separate title bar (macOS): every window's own header is its title bar,
//! as in VS Code, Slack, Linear or Arc. The window is built with an overlay
//! title bar whose title is hidden (it is still set, for Mission Control, the
//! Window menu and accessibility), and the close, minimize and zoom buttons
//! are moved into the header, at its leading edge and centred in its height.
//!
//! The page leaves the buttons their room (`title_bar`), moves the window when
//! an empty part of its header is dragged (`title_bar_drag`), and does what
//! the person chose in System Settings for a double-click on a title bar
//! (`title_bar_double_click`): Tauri's own drag regions always maximize. In
//! full screen the buttons are hidden, so the page drops their room: the host
//! tells it each time a window enters or leaves full screen.
//!
//! The buttons' size and spacing differ between macOS versions, so they are
//! measured once from a window AppKit lays out but never shows.
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use tauri::utils::config::{LogicalPosition, WindowConfig};
use tauri::{Emitter, EventTarget, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

/// The page's event: this window entered (`true`) or left full screen.
pub const FULL_SCREEN_EVENT: &str = "title-bar-full-screen";
/// The same for a popped-out panel window, sent to the window that draws it.
pub const POPOUT_FULL_SCREEN_EVENT: &str = "title-bar-popout-full-screen";

/// The height of the header a window draws at its top, in points at 100 %
/// zoom (CSS pixels): a workspace window's header (`workspace-shell.css`)
/// and, in every other window, its one toolbar (`title-bar.css`).
pub fn header_height(label: &str) -> f64 {
    if label == "main" || label.starts_with("workspace-") {
        48.0
    } else {
        40.0
    }
}

/// Room after the last button before the header's first control, in points.
const GAP: f64 = 12.0;

/// The system's window buttons as AppKit lays them out in a standard title
/// bar: each button's size, how far its bottom edge sits above the title
/// bar's, and the distance from one button's left edge to the next one's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Buttons {
    pub width: f64,
    pub height: f64,
    pub bottom: f64,
    pub spacing: f64,
}

impl Buttons {
    /// macOS 26's, measured: used only where none can be measured.
    pub const FALLBACK: Buttons = Buttons {
        width: 14.0,
        height: 14.0,
        bottom: 9.0,
        spacing: 23.0,
    };

    /// Where the buttons go in a header `height` points tall: as far from
    /// the leading edge as from the top, and centred in the height. The
    /// position is what Tauri (wry and tao, `inset_traffic_lights`) takes:
    /// it makes the title bar `button height + y` tall and keeps each
    /// button's distance from the title bar's bottom edge, so the button's
    /// top lands at `y - bottom` from the window's top.
    pub fn position(&self, height: f64) -> (f64, f64) {
        let margin = ((height - self.height) / 2.0).max(0.0);
        (margin, margin + self.bottom)
    }

    /// The room the buttons take at the header's leading edge, with a gap
    /// before its first control: what the page pads its header with.
    pub fn inset(&self, height: f64) -> f64 {
        let (x, _) = self.position(height);
        (x + 2.0 * self.spacing + self.width + GAP).ceil()
    }
}

static MEASURED: OnceLock<Buttons> = OnceLock::new();

/// The buttons, measured on the first call made on the main thread (window
/// creation and the app's setup run there).
pub fn buttons() -> Buttons {
    if let Some(buttons) = MEASURED.get() {
        return *buttons;
    }
    match measure() {
        Some(buttons) => *MEASURED.get_or_init(|| buttons),
        None => Buttons::FALLBACK,
    }
}

#[cfg(target_os = "macos")]
fn measure() -> Option<Buttons> {
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowButton, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    let mtm = MainThreadMarker::new()?;
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    // Deferred and never ordered in: nothing is drawn or shown.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(480.0, 320.0)),
            style,
            NSBackingStoreType::Buffered,
            true,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setExcludedFromWindowsMenu(true);
    let close = window.standardWindowButton(NSWindowButton::CloseButton)?;
    let minimize = window.standardWindowButton(NSWindowButton::MiniaturizeButton)?;
    let frame = close.frame();
    let next = minimize.frame();
    let buttons = Buttons {
        width: frame.size.width,
        height: frame.size.height,
        bottom: frame.origin.y,
        spacing: next.origin.x - frame.origin.x,
    };
    window.close();
    let sane = [buttons.width, buttons.height, buttons.spacing]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0)
        && buttons.bottom.is_finite()
        && buttons.bottom >= 0.0;
    sane.then_some(buttons)
}

#[cfg(not(target_os = "macos"))]
fn measure() -> Option<Buttons> {
    None
}

/// A window of this app: no separate title bar on macOS, the buttons placed
/// in its header (`header_height(label)`). Elsewhere the system's own title
/// bar stays. The caller sets the title, size and the rest as before.
pub fn window<'a, R: Runtime, M: Manager<R>>(
    manager: &'a M,
    label: &str,
    url: WebviewUrl,
) -> tauri::Result<WebviewWindowBuilder<'a, R, M>> {
    WebviewWindowBuilder::from_config(manager, &config(label, url, buttons()))
}

/// Through a window configuration, because only that places the buttons for
/// the window as well as for its webview: the window's (tao) puts them back
/// when it leaves full screen, the webview's (wry) whenever it draws.
fn config(label: &str, url: WebviewUrl, buttons: Buttons) -> WindowConfig {
    let (x, y) = buttons.position(header_height(label));
    WindowConfig {
        label: label.to_string(),
        url,
        title_bar_style: tauri::TitleBarStyle::Overlay,
        hidden_title: true,
        traffic_light_position: Some(LogicalPosition { x, y }),
        // A press on the header of a window in the background moves it, as a
        // title bar does, instead of only bringing it forward.
        accept_first_mouse: true,
        ..WindowConfig::default()
    }
}

/// The room one header leaves the window buttons, in points: the page divides
/// by its zoom.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inset {
    pub inset: f64,
    pub height: f64,
}

fn inset_for(label: &str, buttons: Buttons) -> Inset {
    let height = header_height(label);
    Inset {
        inset: buttons.inset(height),
        height,
    }
}

/// What the calling window's page needs to draw its header as the title bar.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleBar {
    /// This window's header.
    pub window: Inset,
    /// The tab bar of a panel group this window pops out into a window.
    pub popout: Inset,
    pub full_screen: bool,
}

/// None where the system draws the title bar (not macOS): the page then
/// leaves its header as it is.
#[tauri::command]
pub fn title_bar(window: tauri::Window) -> Result<Option<TitleBar>, String> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    let buttons = buttons();
    Ok(Some(TitleBar {
        window: inset_for(window.label(), buttons),
        popout: inset_for("popout-", buttons),
        full_screen: window.is_fullscreen().map_err(|e| e.to_string())?,
    }))
}

/// The window a press in a header acts on: the caller's own, or for a header
/// in a popped-out panel window (whose page is the caller's, see
/// `host::popout_window`) that window, the one in front among the caller's.
fn target<R: Runtime>(
    window: tauri::Window<R>,
    popout: bool,
) -> Result<Option<tauri::Window<R>>, String> {
    if !popout {
        return Ok(Some(window));
    }
    let opener = window.label().to_string();
    let found = window
        .app_handle()
        .windows()
        .into_values()
        .find(|candidate| {
            crate::host::popout_opener(candidate.label()).as_deref() == Some(opener.as_str())
                && candidate.is_focused().unwrap_or(false)
        });
    Ok(found)
}

/// A press on an empty part of the header: the window follows the pointer.
#[tauri::command]
pub fn title_bar_drag(window: tauri::Window, popout: Option<bool>) -> Result<(), String> {
    match target(window, popout.unwrap_or(false))? {
        Some(window) => window.start_dragging().map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

/// What a double-click on a title bar does (System Settings → Desktop & Dock
/// → "Double-click a window's title bar to").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoubleClick {
    Zoom,
    Minimize,
    Nothing,
}

/// `AppleActionOnDoubleClick` ("Maximize", "Fill", "Minimize", "None"), or
/// before it existed `AppleMiniaturizeOnDoubleClick`. Zoom when unset.
pub fn double_click_action(action: Option<&str>, legacy_minimize: bool) -> DoubleClick {
    match action {
        Some("Minimize") => DoubleClick::Minimize,
        Some("None") => DoubleClick::Nothing,
        Some(_) => DoubleClick::Zoom,
        None if legacy_minimize => DoubleClick::Minimize,
        None => DoubleClick::Zoom,
    }
}

#[cfg(target_os = "macos")]
fn system_double_click() -> DoubleClick {
    use objc2_foundation::{NSUserDefaults, ns_string};
    let defaults = NSUserDefaults::standardUserDefaults();
    let action = defaults
        .stringForKey(ns_string!("AppleActionOnDoubleClick"))
        .map(|value| value.to_string());
    let legacy = defaults.boolForKey(ns_string!("AppleMiniaturizeOnDoubleClick"));
    double_click_action(action.as_deref(), legacy)
}

#[cfg(not(target_os = "macos"))]
fn system_double_click() -> DoubleClick {
    DoubleClick::Zoom
}

/// A double-click on an empty part of the header.
#[tauri::command]
pub fn title_bar_double_click(window: tauri::Window, popout: Option<bool>) -> Result<(), String> {
    let Some(window) = target(window, popout.unwrap_or(false))? else {
        return Ok(());
    };
    let result = match system_double_click() {
        DoubleClick::Nothing => Ok(()),
        DoubleClick::Minimize => window.minimize(),
        DoubleClick::Zoom if window.is_maximized().unwrap_or(false) => window.unmaximize(),
        DoubleClick::Zoom => window.maximize(),
    };
    result.map_err(|e| e.to_string())
}

/// Whether each window was in full screen when last seen, to tell its page
/// only of a change.
static FULL_SCREEN: Mutex<BTreeMap<String, bool>> = Mutex::new(BTreeMap::new());

/// Called on every window event: a resize ends each move in or out of full
/// screen (tao sends one after `windowDidEnterFullScreen`/`Exit`).
pub fn window_event<R: Runtime>(window: &tauri::Window<R>, event: &tauri::WindowEvent) {
    let label = window.label().to_string();
    match event {
        tauri::WindowEvent::Resized(_) => {
            let Ok(now) = window.is_fullscreen() else {
                return;
            };
            let Ok(mut seen) = FULL_SCREEN.lock() else {
                return;
            };
            if seen.insert(label.clone(), now).unwrap_or(false) == now {
                return;
            }
            drop(seen);
            let app = window.app_handle();
            let _ = match crate::host::popout_opener(&label) {
                Some(opener) => app.emit_to(
                    EventTarget::webview(opener.as_str()),
                    POPOUT_FULL_SCREEN_EVENT,
                    now,
                ),
                None => app.emit_to(EventTarget::webview(label.as_str()), FULL_SCREEN_EVENT, now),
            };
        }
        tauri::WindowEvent::Destroyed => {
            if let Ok(mut seen) = FULL_SCREEN.lock() {
                seen.remove(&label);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measured on macOS 26 off screen (a titled window, full-size content,
    /// Tauri's `inset_traffic_lights` applied): with y = 26 the close
    /// button's top is 17 points down and its centre 24, the middle of a
    /// 48-point header.
    #[test]
    fn the_buttons_sit_in_the_middle_of_the_header_and_as_far_from_its_edge() {
        let buttons = Buttons::FALLBACK;
        assert_eq!(buttons.position(48.0), (17.0, 26.0));
        let (x, y) = buttons.position(48.0);
        let top = y - buttons.bottom;
        assert_eq!(top + buttons.height / 2.0, 24.0, "centred in 48");
        assert_eq!(x, top, "as far from the leading edge as from the top");
        assert_eq!(buttons.position(40.0), (13.0, 22.0));
    }

    /// The header pads past the zoom button with a gap; older macOS buttons
    /// (smaller spacing) leave a smaller inset.
    #[test]
    fn the_header_leaves_the_buttons_their_room() {
        let tahoe = Buttons::FALLBACK;
        // 17 + 2 × 23 + 14 = 77 to the zoom button's right edge, then 12.
        assert_eq!(tahoe.inset(48.0), 89.0);
        assert_eq!(tahoe.inset(40.0), 85.0);
        let older = Buttons {
            width: 14.0,
            height: 16.0,
            bottom: 6.0,
            spacing: 20.0,
        };
        assert_eq!(older.position(48.0), (16.0, 22.0));
        assert_eq!(older.inset(48.0), 82.0);
    }

    #[test]
    fn workspace_windows_have_the_tall_header_and_the_others_a_toolbar() {
        for label in ["main", "workspace-alpha"] {
            assert_eq!(header_height(label), 48.0, "{label}");
        }
        for label in [
            "chat-panel-1",
            "console-command",
            "artifact-a1",
            "web-panel-1",
            "popout-3",
            "setup",
        ] {
            assert_eq!(header_height(label), 40.0, "{label}");
        }
    }

    /// The window's configuration carries the overlay title bar, a hidden
    /// title and the buttons' position; its label and page are the caller's.
    #[test]
    fn every_window_is_configured_without_a_separate_title_bar() {
        let config = config(
            "chat-panel-1",
            WebviewUrl::App("index.html".into()),
            Buttons::FALLBACK,
        );
        assert_eq!(config.label, "chat-panel-1");
        assert_eq!(config.url, WebviewUrl::App("index.html".into()));
        assert_eq!(config.title_bar_style, tauri::TitleBarStyle::Overlay);
        assert!(config.hidden_title);
        assert!(config.decorations, "the system's buttons stay");
        assert!(config.accept_first_mouse);
        let position = config.traffic_light_position.expect("position");
        assert_eq!((position.x, position.y), (13.0, 22.0));
    }

    #[test]
    fn a_double_click_does_what_the_system_setting_says() {
        assert_eq!(
            double_click_action(Some("Maximize"), false),
            DoubleClick::Zoom
        );
        assert_eq!(double_click_action(Some("Fill"), false), DoubleClick::Zoom);
        assert_eq!(
            double_click_action(Some("Minimize"), false),
            DoubleClick::Minimize
        );
        assert_eq!(
            double_click_action(Some("None"), true),
            DoubleClick::Nothing
        );
        assert_eq!(double_click_action(None, true), DoubleClick::Minimize);
        assert_eq!(double_click_action(None, false), DoubleClick::Zoom);
    }
}
