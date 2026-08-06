//! Box-lifecycle artifacts (#902) enforcement.
//!
//! `contrib/box-lifecycle/` ships the stop-on-idle timer/units and the
//! start-on-connect helper so a per-user GCE box costs nothing while
//! untouched. These run on a box far from anyone watching: a timer that
//! never fires, a `ExecStart` naming a missing script, or a script with a
//! shell syntax error all fail silently. This test keeps them honest —
//! the units carry the directives that make them actually schedule and
//! run, and the shell scripts parse under `bash -n`.
//!
//! Lives in `lazybox-core` beside `dep_rules.rs` / `regression_ledger.rs`
//! for the same reason: core sits below everything it audits.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

fn lifecycle_dir() -> PathBuf {
    workspace_root().join("contrib/box-lifecycle")
}

fn read(rel: &str) -> String {
    let path = lifecycle_dir().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Parse `Key=Value` lines from a systemd unit into a multimap.
fn unit_directives(body: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.entry(key.trim().to_string())
                .or_default()
                .push(value.trim().to_string());
        }
    }
    map
}

#[test]
fn timer_actually_schedules_the_idle_check() {
    let timer = unit_directives(&read("lazybox-idle-stop.timer"));

    // A timer with no On*Sec never fires — the whole feature would be inert.
    let fires = timer.contains_key("OnBootSec") || timer.contains_key("OnUnitActiveSec");
    assert!(
        fires,
        "lazybox-idle-stop.timer has no OnBootSec/OnUnitActiveSec — it would never fire"
    );

    // Without WantedBy=timers.target, `systemctl enable` wires it nowhere.
    let installed = timer
        .get("WantedBy")
        .map(|v| v.iter().any(|w| w == "timers.target"))
        .unwrap_or(false);
    assert!(
        installed,
        "lazybox-idle-stop.timer needs [Install] WantedBy=timers.target to be enable-able"
    );
}

#[test]
fn service_runs_the_installed_script() {
    let service = unit_directives(&read("lazybox-idle-stop.service"));

    let ty = service.get("Type").and_then(|v| v.first());
    assert_eq!(
        ty.map(String::as_str),
        Some("oneshot"),
        "idle-stop service should be Type=oneshot (a timer-driven one-shot check)"
    );

    let exec = service
        .get("ExecStart")
        .and_then(|v| v.first())
        .expect("idle-stop service has no ExecStart");
    assert!(
        exec.ends_with("lazybox-idle-stop.sh"),
        "ExecStart {exec:?} should invoke the installed lazybox-idle-stop.sh"
    );
}

#[test]
fn scripts_are_hardened_and_parse() {
    // Both helpers must use strict mode; a partial run of a box-stop or a
    // tunnel script on an unset variable is worse than a clean abort.
    for name in ["lazybox-idle-stop.sh", "connect.sh"] {
        let body = read(name);
        assert!(
            body.starts_with("#!/usr/bin/env bash"),
            "{name}: missing bash shebang"
        );
        assert!(
            body.contains("set -euo pipefail"),
            "{name}: missing `set -euo pipefail`"
        );
    }

    // The idle detector must key off the documented idle window and have a
    // real stop path (self-`gcloud … stop` with a `shutdown` fallback).
    let idle = read("lazybox-idle-stop.sh");
    assert!(
        idle.contains("LAZYBOX_IDLE_MINUTES"),
        "idle script ignores the idle window"
    );
    assert!(
        idle.contains("instances stop") && idle.contains("shutdown"),
        "idle script has no gcloud-stop + shutdown-fallback stop path"
    );

    // The connect helper must actually start a stopped box before tunnelling.
    let connect = read("connect.sh");
    assert!(
        connect.contains("instances start") && connect.contains("tunnel-through-iap"),
        "connect.sh should start the instance and open an IAP tunnel"
    );

    // Catch shell syntax errors where bash is available (any dev/CI host).
    if let Ok(bash) = which_bash() {
        for name in ["lazybox-idle-stop.sh", "connect.sh"] {
            let path = lifecycle_dir().join(name);
            let out = Command::new(&bash)
                .arg("-n")
                .arg(&path)
                .output()
                .unwrap_or_else(|e| panic!("run bash -n {}: {e}", path.display()));
            assert!(
                out.status.success(),
                "bash -n {name} failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

fn which_bash() -> Result<PathBuf, ()> {
    for dir in ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        let p = Path::new(dir).join("bash");
        if p.exists() {
            return Ok(p);
        }
    }
    Err(())
}
