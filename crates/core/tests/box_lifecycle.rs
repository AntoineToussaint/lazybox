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

/// The box installer (`contrib/box/install.sh`, #977) is what a freshly
/// provisioned box runs to get a daemon whose wire fingerprint matches the
/// client — build at the pinned commit, install the binary, then copy +
/// enable the daemon and idle-stop units. It runs unattended under a systemd
/// oneshot far from anyone watching, so this keeps it honest: strict mode, a
/// real build+install path, the SHA record `lazybox sandbox rebuild` reads,
/// and the systemd wiring that makes the daemon come up on boot.
#[test]
fn box_installer_builds_and_wires_the_daemon() {
    let path = workspace_root().join("contrib/box/install.sh");
    let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    assert!(
        body.starts_with("#!/usr/bin/env bash"),
        "install.sh: missing bash shebang"
    );
    assert!(
        body.contains("set -euo pipefail"),
        "install.sh: missing `set -euo pipefail`"
    );

    // Builds at the checkout and installs the binary where the units expect it.
    assert!(
        body.contains("make -C \"$LAZYBOX_SRC\" release"),
        "install.sh should build the release binary"
    );
    assert!(
        body.contains("/usr/local/bin/lazybox"),
        "install.sh should install the binary to /usr/local/bin/lazybox"
    );
    // Records the built commit so rebuild can skip a no-op and the box is greppable.
    assert!(
        body.contains("/etc/lazybox/build-sha"),
        "install.sh must record the installed SHA"
    );

    // Copies + enables the daemon on boot and the idle-stop timer — the two
    // units the issue's acceptance turns on.
    assert!(
        body.contains("lazybox-daemon@") && body.contains("systemctl enable --now"),
        "install.sh should enable the daemon unit on boot"
    );
    assert!(
        body.contains("lazybox-idle-stop.timer"),
        "install.sh should arm the idle-stop timer so an untouched box stops itself"
    );
    // A rebuild swaps the binary under a running daemon, so it must restart.
    assert!(
        body.contains("systemctl restart"),
        "install.sh should restart the daemon after a rebuild"
    );

    // The binary must be swapped by rename onto a staged temp file, never an
    // in-place overwrite: opening the running daemon's own executable with
    // O_TRUNC (what `install`/`cp` do) fails with ETXTBSY on a rebuild.
    assert!(
        body.contains("$BIN_DST.new") && body.contains("mv -f"),
        "install.sh must stage the binary and rename it into place (ETXTBSY-safe)"
    );
    assert!(
        !body.contains("install -m0755 \"$LAZYBOX_SRC/target/release/lazybox\" \"$BIN_DST\""),
        "install.sh must not overwrite the running binary in place"
    );

    // A rebuild to an unreachable commit must fail loudly, not silently build
    // the current checkout and report success.
    assert!(
        body.contains("is unreachable") && body.contains("exit 1"),
        "install.sh should error out when the requested commit can't be checked out"
    );

    // The units it installs must actually exist in the tree it copies from.
    for unit in [
        "contrib/systemd/lazybox-daemon@.service",
        "contrib/box-lifecycle/lazybox-idle-stop.service",
        "contrib/box-lifecycle/lazybox-idle-stop.timer",
        "contrib/box-lifecycle/lazybox-idle-stop.sh",
    ] {
        assert!(
            workspace_root().join(unit).exists(),
            "install.sh copies {unit}, which is missing"
        );
    }

    if let Ok(bash) = which_bash() {
        let out = Command::new(&bash)
            .arg("-n")
            .arg(&path)
            .output()
            .unwrap_or_else(|e| panic!("run bash -n {}: {e}", path.display()));
        assert!(
            out.status.success(),
            "bash -n install.sh failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
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

/// Behavioral checks that actually run `lazybox-idle-stop.sh` and assert on the
/// idle decision path — the marker stamp/threshold logic, the CPU-delta agent
/// detection that must not reap a working agent, and the shutdown fallback when
/// a `gcloud … stop` is rejected. Unix-only: the script is bash, and the stop
/// fallback needs executable command stubs on PATH.
#[cfg(unix)]
mod behavior {
    use super::{lifecycle_dir, which_bash};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::thread::sleep;
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    // Run the idle-stop script with a temp marker and a bogus SSH port so no
    // real connection is ever counted as an active tunnel. `env` supplies the
    // per-test knobs; `path_prefix` is prepended to PATH for command stubs.
    fn run_idle(
        bash: &Path,
        marker: &Path,
        env: &[(&str, &str)],
        path_prefix: Option<&Path>,
    ) -> Output {
        let mut cmd = Command::new(bash);
        cmd.arg(lifecycle_dir().join("lazybox-idle-stop.sh"));
        cmd.env("LAZYBOX_IDLE_MARKER", marker);
        cmd.env("LAZYBOX_IDLE_SSH_PORT", "65533");
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(prefix) = path_prefix {
            let base = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{}", prefix.display(), base));
        }
        cmd.output().expect("run lazybox-idle-stop.sh")
    }

    fn write_exec(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, body).expect("write stub");
        let mut perm = fs::metadata(path).expect("stat stub").permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm).expect("chmod stub");
    }

    #[test]
    fn idle_marker_stamps_then_stops_past_the_window() {
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("idle_threshold");
        let marker = dir.join("idle-since");
        let stopped = dir.join("STOPPED");
        let stop_cmd = format!("touch {}", stopped.display());
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", "lazybox-absent-agent"),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
        ];

        // First idle tick stamps the marker but does not stop.
        let out = run_idle(&bash, &marker, &env, None);
        assert!(
            out.status.success(),
            "tick1: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(marker.exists(), "first idle tick should stamp the marker");
        assert!(!stopped.exists(), "a fresh marker must not stop the box");

        // Backdate the marker before the window; the next tick must stop.
        fs::write(&marker, "1").expect("backdate marker");
        let out = run_idle(&bash, &marker, &env, None);
        assert!(
            out.status.success(),
            "tick2: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stopped.exists(),
            "a marker older than the idle window must stop the box"
        );
    }

    #[test]
    fn a_working_agent_is_not_reaped_mid_task() {
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("busy_agent");
        let marker = dir.join("idle-since");
        let stopped = dir.join("STOPPED");
        let stop_cmd = format!("touch {}", stopped.display());
        let token = "lazybox-test-working-agent";
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", token),
            ("LAZYBOX_IDLE_AGENT_CPU_SECS", "1"),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
        ];

        // A process spinning the CPU, its argv carrying the watched token.
        let mut agent = Command::new(&bash)
            .args(["-c", "while :; do :; done", token])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn busy agent");

        // Tick 1 sees a newly-observed process → active, clears the stale marker.
        fs::write(&marker, "1").expect("stale marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_1 = stopped.exists();
        let cleared_1 = !marker.exists();

        // Tick 2 must keep it alive on the CPU *delta* (not newness): re-stale
        // the marker, let the agent burn CPU, run again.
        sleep(Duration::from_secs(3));
        fs::write(&marker, "1").expect("stale marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_2 = stopped.exists();
        let cleared_2 = !marker.exists();

        // Kill the spinner before asserting so a failure can't leak a busy loop.
        let _ = agent.kill();
        let _ = agent.wait();

        assert!(!stopped_1, "a live agent must not be stopped");
        assert!(
            cleared_1,
            "a newly-seen active agent clears the idle marker"
        );
        assert!(
            !stopped_2,
            "an agent burning CPU between ticks stays active"
        );
        assert!(
            cleared_2,
            "the CPU delta since the last tick must clear the idle marker"
        );
    }

    #[test]
    fn a_rejected_gcloud_stop_falls_back_to_shutdown() {
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("stop_fallback");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).expect("create bin");
        let did_shutdown = dir.join("DID_SHUTDOWN");

        // gcloud present but rejects the stop; metadata resolves; shutdown records.
        write_exec(
            &bin.join("gcloud"),
            "#!/usr/bin/env bash\ncase \"$*\" in *'instances stop'*) exit 1;; esac\nexit 0\n",
        );
        write_exec(
            &bin.join("curl"),
            "#!/usr/bin/env bash\nfor a in \"$@\"; do case \"$a\" in */instance/name) echo test-box;; */instance/zone) echo projects/1/zones/z;; esac; done\n",
        );
        write_exec(
            &bin.join("shutdown"),
            &format!("#!/usr/bin/env bash\n: > {}\n", did_shutdown.display()),
        );

        let marker = dir.join("idle-since");
        fs::write(&marker, "1").expect("stale marker"); // triggers the stop path
        let env = [("LAZYBOX_IDLE_AGENT_PROCS", "lazybox-absent-agent")];
        let out = run_idle(&bash, &marker, &env, Some(&bin));
        assert!(
            out.status.success(),
            "idle-stop exited non-zero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            did_shutdown.exists(),
            "a rejected `gcloud … stop` must fall back to a guest shutdown, not leave the box running"
        );
    }
}
