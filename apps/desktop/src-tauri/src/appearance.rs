//! Native zoom, with a saved app default and overrides for open windows.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tauri::{Emitter, EventTarget, Manager, State, WebviewWindow};

/// Resolved by the page, including custom themes and system appearance.
#[derive(Default)]
pub struct ThemeBackground(pub Mutex<Option<tauri::window::Color>>);

#[tauri::command]
pub fn set_window_background(
    color: String,
    background: State<'_, ThemeBackground>,
) -> Result<(), String> {
    let hex = color.strip_prefix('#').unwrap_or("");
    if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("Expected a theme background in hex".into());
    }
    *background.0.lock().map_err(|e| e.to_string())? = Some(color.parse()?);
    Ok(())
}

/// The steps Zoom In / Zoom Out walk, like a browser's.
pub const ZOOM_STEPS: [f64; 13] = [
    0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.2, 1.25, 1.33, 1.5, 1.75, 2.0,
];
const MIN_ZOOM: f64 = 0.5;
const MAX_ZOOM: f64 = 2.0;

#[derive(Default, Deserialize, Serialize)]
struct Saved {
    zoom: f64,
}

/// The saved app-wide setting and temporary zoom overrides by native window.
pub struct Zoom {
    path: PathBuf,
    level: Mutex<ZoomLevels>,
}

struct ZoomLevels {
    default: f64,
    windows: HashMap<String, f64>,
}

impl ZoomLevels {
    fn for_window(&self, label: &str) -> f64 {
        self.windows.get(label).copied().unwrap_or(self.default)
    }
}

impl Zoom {
    /// Reads the saved zoom; a missing or unreadable file means 100%.
    pub fn open(root: &Path) -> Self {
        let path = root.join("appearance.json");
        let level = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .map(|saved| clamp(saved.zoom))
            .unwrap_or(1.0);
        Self {
            path,
            level: Mutex::new(ZoomLevels {
                default: level,
                windows: HashMap::new(),
            }),
        }
    }
    pub fn level(&self) -> f64 {
        self.level
            .lock()
            .map(|levels| levels.default)
            .unwrap_or(1.0)
    }
    fn level_for(&self, label: &str) -> f64 {
        self.level
            .lock()
            .map(|levels| levels.for_window(label))
            .unwrap_or(1.0)
    }
    fn remember_window(&self, label: &str, level: f64) -> Result<(), String> {
        self.level
            .lock()
            .map_err(|e| e.to_string())?
            .windows
            .insert(label.to_owned(), level);
        Ok(())
    }

    pub fn forget(&self, label: &str) {
        if let Ok(mut levels) = self.level.lock() {
            levels.windows.remove(label);
        }
    }
    /// Sets and saves the zoom; returns the level actually kept.
    pub fn set(&self, level: f64) -> Result<f64, String> {
        let level = clamp(level);
        let parent = self.path.parent().ok_or("Appearance has no folder")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec(&Saved { zoom: level }).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.persist(&self.path).map_err(|e| e.to_string())?;
        let mut levels = self.level.lock().map_err(|e| e.to_string())?;
        levels.default = level;
        levels.windows.clear();
        Ok(level)
    }
}

fn clamp(level: f64) -> f64 {
    if level.is_finite() {
        (level.clamp(MIN_ZOOM, MAX_ZOOM) * 100.0).round() / 100.0
    } else {
        1.0
    }
}

/// The next step from `level`: up, down, or back to 100%.
pub fn step(level: f64, direction: i8) -> f64 {
    match direction {
        0 => 1.0,
        d if d > 0 => ZOOM_STEPS
            .iter()
            .copied()
            .find(|s| *s > level + 0.001)
            .unwrap_or(MAX_ZOOM),
        _ => ZOOM_STEPS
            .iter()
            .rev()
            .copied()
            .find(|s| *s < level - 0.001)
            .unwrap_or(MIN_ZOOM),
    }
}

/// Supply the native scale to the blank document Dockview draws via its opener.
pub fn popout_zoom_script(level: f64) -> String {
    format!(
        "(() => {{ const apply = () => document.documentElement.style.setProperty('--popout-page-zoom', '{level}'); if (document.documentElement) apply(); else document.addEventListener('DOMContentLoaded', apply, {{once:true}}); }})()"
    )
}

fn apply_window_zoom(window: &WebviewWindow, level: f64) -> Result<(), String> {
    window.set_zoom(level).map_err(|e| e.to_string())?;
    if window.label().starts_with("popout-") {
        window
            .eval(popout_zoom_script(level))
            .map_err(|e| e.to_string())
    } else {
        window
            .emit_to(EventTarget::webview(window.label()), "ui-zoom", level)
            .map_err(|e| e.to_string())
    }
}

pub fn step_window_zoom(window: &WebviewWindow, direction: i8) -> Result<(), String> {
    let zoom = window.state::<Zoom>();
    let level = step(zoom.level_for(window.label()), direction);
    apply_window_zoom(window, level)?;
    zoom.remember_window(window.label(), level)
}

/// An explicit change in Appearance applies the saved setting to all windows.
pub fn apply_zoom(app: &tauri::AppHandle, level: f64) -> Result<f64, String> {
    let level = app.state::<Zoom>().set(level)?;
    for window in app.webview_windows().values() {
        apply_window_zoom(window, level)?;
    }
    app.emit("ui-default-zoom", level)
        .map_err(|e| e.to_string())?;
    Ok(level)
}

#[tauri::command]
pub fn ui_zoom(window: WebviewWindow, zoom: State<'_, Zoom>) -> f64 {
    zoom.level_for(window.label())
}

#[tauri::command]
pub fn default_ui_zoom(zoom: State<'_, Zoom>) -> f64 {
    zoom.level()
}

#[tauri::command]
pub fn set_ui_zoom(app: tauri::AppHandle, level: f64) -> Result<f64, String> {
    apply_zoom(&app, level)
}

#[derive(Clone, Serialize)]
struct Preference {
    key: String,
    value: String,
}

/// Every window rereads an app-wide preference another window just saved.
#[tauri::command]
pub fn share_preference(app: tauri::AppHandle, key: String, value: String) -> Result<(), String> {
    if key.len() > 1024 || value.len() > 256 * 1024 {
        return Err("Preference is too large to share".into());
    }
    app.emit("preference-changed", Preference { key, value })
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_survives_restart_and_stays_in_range() {
        let root = tempfile::tempdir().expect("fixture");
        let zoom = Zoom::open(root.path());
        assert_eq!(zoom.level(), 1.0);
        assert_eq!(zoom.set(1.25).expect("save"), 1.25);
        assert_eq!(Zoom::open(root.path()).level(), 1.25);
        assert_eq!(zoom.set(9.0).expect("save"), MAX_ZOOM);
        assert_eq!(zoom.set(f64::NAN).expect("save"), 1.0);
        fs::write(root.path().join("appearance.json"), b"not json").expect("corrupt");
        assert_eq!(Zoom::open(root.path()).level(), 1.0);
    }

    #[test]
    fn window_zoom_does_not_change_other_windows_or_the_saved_default() {
        let root = tempfile::tempdir().expect("fixture");
        let zoom = Zoom::open(root.path());
        zoom.set(1.2).expect("default");
        zoom.remember_window("artifact-1", step(zoom.level_for("artifact-1"), 1))
            .expect("zoom artifact");
        assert_eq!(zoom.level_for("artifact-1"), 1.25);
        assert_eq!(zoom.level_for("main"), 1.2);
        assert_eq!(Zoom::open(root.path()).level(), 1.2);
        zoom.remember_window("main", step(zoom.level_for("main"), -1))
            .expect("zoom main");
        assert_eq!(zoom.level_for("main"), 1.1);
        assert_eq!(zoom.level_for("artifact-1"), 1.25);
        zoom.forget("artifact-1");
        assert_eq!(zoom.level_for("artifact-1"), 1.2);
        zoom.set(0.9).expect("new app default");
        assert_eq!(zoom.level_for("main"), 0.9);
    }

    #[test]
    fn steps_walk_the_ladder_and_stop_at_the_ends() {
        assert_eq!(step(1.0, 1), 1.1);
        assert_eq!(step(1.0, -1), 0.9);
        assert_eq!(step(1.17, 1), 1.2);
        assert_eq!(step(1.17, -1), 1.1);
        assert_eq!(step(MAX_ZOOM, 1), MAX_ZOOM);
        assert_eq!(step(MIN_ZOOM, -1), MIN_ZOOM);
        assert_eq!(step(1.5, 0), 1.0);
    }
}
