//! Appearance the native host owns: the UI zoom of every window, kept across
//! launches, and telling every window when an app-wide preference (theme,
//! fonts, shortcuts) changed in one of them.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tauri::{Emitter, Manager, State};

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

/// The UI zoom shared by every window, saved in the app's data folder.
pub struct Zoom {
    path: PathBuf,
    level: Mutex<f64>,
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
            level: Mutex::new(level),
        }
    }
    pub fn level(&self) -> f64 {
        self.level.lock().map(|l| *l).unwrap_or(1.0)
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
        *self.level.lock().map_err(|e| e.to_string())? = level;
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

/// Saves `level` and applies it to every open window, then tells them (Settings shows it).
pub fn apply_zoom(app: &tauri::AppHandle, level: f64) -> Result<f64, String> {
    let zoom = app.state::<Zoom>();
    let level = zoom.set(level)?;
    for window in app.webview_windows().values() {
        window.set_zoom(level).map_err(|e| e.to_string())?;
    }
    app.emit("ui-zoom", level).map_err(|e| e.to_string())?;
    Ok(level)
}

#[tauri::command]
pub fn ui_zoom(zoom: State<'_, Zoom>) -> f64 {
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
