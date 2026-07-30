#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).expect("write fake executable");
        let mut permissions = fs::metadata(path)
            .expect("fake executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod fake executable");
    }

    #[test]
    fn failure_closes_the_probe_in_the_overridden_ghostty_app() {
        let temp = tempfile::tempdir().expect("tempdir");
        let fake_bin = temp.path().join("bin");
        fs::create_dir(&fake_bin).expect("fake bin directory");

        write_executable(&fake_bin.join("uname"), "#!/bin/sh\nprintf 'Darwin\\n'\n");
        write_executable(&fake_bin.join("sleep"), "#!/bin/sh\nexit 0\n");
        write_executable(&fake_bin.join("seq"), "#!/bin/sh\nprintf '1\\n'\n");

        let osascript_log = temp.path().join("osascript.log");
        write_executable(
            &fake_bin.join("osascript"),
            r#"#!/bin/sh
{
  printf 'CALL\n'
  printf '%s\n' "$@"
} >>"${FAKE_OSASCRIPT_LOG}"
calls="$(grep -c '^CALL$' "${FAKE_OSASCRIPT_LOG}")"
if [ "${calls}" -eq 1 ]; then
  seen_end=false
  for argument in "$@"; do
    if [ "${argument}" = 'end tell' ]; then
      seen_end=true
    elif [ "${argument}" = 'return probe_window_id & tab & probe_terminal_id' ] &&
      [ "${seen_end}" = true ]; then
      printf 'probe-window\tprobe-terminal\n'
    fi
  done
  for probe_dir in "${TMPDIR}"/lazybox-ghostty-mouse.*; do
    if [ -d "${probe_dir}" ]; then
      touch "${probe_dir}/ready"
    fi
  done
fi
"#,
        );

        let ghostty_app = temp.path().join("Custom Ghostty.app");
        let ghostty_bin = ghostty_app.join("Contents/MacOS/ghostty");
        fs::create_dir_all(ghostty_bin.parent().expect("Ghostty bin parent"))
            .expect("fake Ghostty app");
        write_executable(
            &ghostty_bin,
            "#!/bin/sh\nprintf 'mouse-reporting = true\\n'\n",
        );

        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let mut path_entries = vec![fake_bin.clone()];
        path_entries.extend(std::env::split_paths(&inherited_path));
        let fake_path = std::env::join_paths(path_entries).expect("fake PATH");
        let probe_root = temp.path().join("probes");
        fs::create_dir(&probe_root).expect("probe root");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/check-ghostty-mouse-reporting.sh");

        let output = Command::new("bash")
            .arg(script)
            .env("PATH", fake_path)
            .env("GHOSTTY_BIN", &ghostty_bin)
            .env("FAKE_OSASCRIPT_LOG", &osascript_log)
            .env("TMPDIR", &probe_root)
            .output()
            .expect("run Ghostty mouse check");

        assert!(
            !output.status.success(),
            "probe should report no mouse event"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Ghostty did not forward a right-button event (0 bytes captured)"));
        assert!(
            !stderr.contains("No such file or directory"),
            "missing capture file should be reported cleanly: {stderr}"
        );

        let log = fs::read_to_string(osascript_log).expect("osascript log");
        assert_eq!(
            log.lines().filter(|line| *line == "CALL").count(),
            3,
            "create, inject, and cleanup must each call osascript"
        );
        assert!(
            log.contains(&format!("tell application \"{}\"", ghostty_app.display())),
            "AppleScript must control the app selected by GHOSTTY_BIN: {log}"
        );
        assert!(log.contains("if id of candidate is \"probe-window\" then close window candidate"));
    }
}
