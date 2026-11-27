use std::{fs, process::Command};
use tempfile::tempdir;

#[test]
fn explicit_profile_overrides_home_and_empty_profile_falls_back() {
    let home = tempdir().unwrap();
    let profile = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let default = home.path().join(".lazybox");
    fs::create_dir(&default).unwrap();
    for (directory, marker) in [
        (default.as_path(), "home-only-key"),
        (profile.path(), "profile-only-key"),
    ] {
        fs::write(
            directory.join("snippets.yaml"),
            format!("snippets:\n  {marker}: {{body: selected}}\n"),
        )
        .unwrap();
    }
    for (setting, key) in [
        (profile.path().as_os_str(), "profile-only-key"),
        (std::ffi::OsStr::new(""), "home-only-key"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_lazybox-snippets"))
            .current_dir(cwd.path())
            .env("HOME", home.path())
            .env("LAZYBOX_HOME", setting)
            .output()
            .unwrap();
        assert!(output.status.success());
        let entries: Vec<serde_yaml::Value> = serde_yaml::from_slice(&output.stdout).unwrap();
        let globals: Vec<_> = entries
            .iter()
            .filter(|e| e["origin"] == "global")
            .map(|e| e["key"].as_str().unwrap())
            .collect();
        assert_eq!(globals, [key]);
    }
}
