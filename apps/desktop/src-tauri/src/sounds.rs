//! A macOS system alert sound (Glass, Ping, Submarine…) for Lazybox's own
//! notification sounds (Settings → Notifications → Sounds). The page cannot
//! read `/System/Library/Sounds`, so the host plays the one named, at the
//! volume asked, with `afplay`. Only a sound of that folder, by its bare name.
//! `afplay` is waited for on a thread of its own (`children::reap`), so no
//! sound leaves a zombie behind.
use std::{
    io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread::JoinHandle,
};
use tauri::Webview;

const FOLDER: &str = "/System/Library/Sounds";

/// The sound file for a bare name ("Glass"), if it is one of the system's.
fn sound_file(folder: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.len() > 40 || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("Expected the name of a system sound".into());
    }
    let path = folder.join(format!("{name}.aiff"));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("{name} is not a system sound on this Mac"))
    }
}

/// `afplay`'s volume: 0 (silent) to 1 (the sound as recorded).
fn volume(value: f64) -> Result<String, String> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err("Expected a volume from 0 to 1".into());
    }
    Ok(format!("{value:.2}"))
}

/// Starts `afplay` on `file`; the handle says how it ended.
fn play(file: &Path, volume: &str) -> io::Result<JoinHandle<io::Result<ExitStatus>>> {
    let child = Command::new("/usr/bin/afplay")
        .arg("-v")
        .arg(volume)
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    crate::children::reap(child)
}

#[tauri::command]
pub fn play_system_sound(window: Webview, name: String, level: f64) -> Result<(), String> {
    let label = window.label();
    if !(label == "main" || label.starts_with("workspace-") || label.starts_with("chat-")) {
        return Err("Only a Lazybox window plays its sounds".into());
    }
    let file = sound_file(Path::new(FOLDER), &name)?;
    play(&file, &volume(level)?)
        .map(|_| ())
        .map_err(|_| "Could not play the sound".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_sound_of_the_folder_by_its_bare_name() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("Glass.aiff"), b"x").unwrap();
        assert_eq!(
            sound_file(folder.path(), "Glass").unwrap(),
            folder.path().join("Glass.aiff")
        );
        for bad in [
            "",
            "Ping",
            "../Glass",
            "Glass.aiff",
            "Gl ass",
            "/etc/passwd",
            &"A".repeat(41),
        ] {
            assert!(sound_file(folder.path(), bad).is_err(), "{bad:?}");
        }
    }

    /// One notification sound left one zombie `afplay` behind: it is waited for.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_played_sound_is_waited_for() {
        let Ok(file) = sound_file(Path::new(FOLDER), "Tink") else {
            return;
        };
        // How afplay ends depends on the machine's audio (none in some
        // sessions); that it is waited for does not.
        play(&file, "0.00")
            .expect("afplay")
            .join()
            .expect("join")
            .expect("waited for");
    }

    #[test]
    fn volume_is_from_silent_to_as_recorded() {
        assert_eq!(volume(0.0).unwrap(), "0.00");
        assert_eq!(volume(0.5).unwrap(), "0.50");
        assert_eq!(volume(1.0).unwrap(), "1.00");
        for bad in [-0.1, 1.01, f64::NAN, f64::INFINITY] {
            assert!(volume(bad).is_err(), "{bad}");
        }
    }
}
