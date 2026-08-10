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
    // Every helper must use strict mode; a partial run of a box-stop, a
    // tunnel, or a daemon build on an unset variable is worse than a clean abort.
    for name in ["lazybox-idle-stop.sh", "connect.sh", "lazybox-build.sh"] {
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

    // The build/rebuild helper (#977) is the on-box half of build-parity:
    // it must (re)build at a pinned commit, install the daemon unit, record
    // the SHA somewhere greppable, and restart the daemon — the four steps
    // the acceptance criteria hinge on.
    let build = read("lazybox-build.sh");
    assert!(
        build.contains("make setup") && build.contains("make release"),
        "build helper must build the daemon with the pinned toolchain"
    );
    assert!(
        build.contains("lazybox-daemon@") && build.contains("systemctl"),
        "build helper must install + drive the daemon systemd unit"
    );
    assert!(
        build.contains("restart"),
        "build helper must restart the daemon so a rebuild takes effect"
    );
    assert!(
        build.contains("build-sha"),
        "build helper must record the installed commit somewhere greppable"
    );
    assert!(
        build.contains("lazybox-idle-stop.timer"),
        "build helper must arm the idle-stop timer so an ensured box still sleeps"
    );
    // A client built from an unpushed commit passes a SHA the box can't fetch;
    // the checkout must fall back to the default branch so the box still runs a
    // daemon, not abort with none (the exact failure #977 removes). Assert the
    // pinned checkout is guarded and the else path builds the default branch.
    assert!(
        build.contains("git checkout --detach '$TARGET_SHA'")
            && build.contains("git checkout main"),
        "build helper must fall back to the default branch when the pinned SHA is unfetchable"
    );
    let checkout = build
        .find("git checkout --detach")
        .expect("pinned checkout present");
    let fallback = build.find("git checkout main").expect("fallback present");
    assert!(
        build[..checkout].contains("if ")
            && checkout < fallback
            && build[checkout..fallback].contains("else"),
        "the pinned checkout must be guarded with an else fallback, not run unconditionally"
    );

    // Catch shell syntax errors where bash is available (any dev/CI host).
    if let Ok(bash) = which_bash() {
        for name in ["lazybox-idle-stop.sh", "connect.sh", "lazybox-build.sh"] {
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
        // Pin the daemon-liveness file to a path that never exists, so a real
        // `~/.lazybox/run/active` on the dev/CI host (a running lazybox with a
        // terminal open) can't make the script read the box as active and skip
        // the stop path. Tests that exercise the liveness check override this
        // via `env`, which is applied afterward and wins.
        cmd.env(
            "LAZYBOX_IDLE_ACTIVE_FILE",
            marker.with_file_name("no-such-active-file"),
        );
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
    fn a_working_agent_blocked_on_a_child_is_not_reaped() {
        // The core #978 fix: `pgrep -f claude` matches the agent, not the
        // `cargo build` child it spawned and is blocking on. The agent itself
        // burns almost no CPU across a tick, so summing only the agent pid
        // would read as idle and reap the box mid-build. The script must sum
        // the CPU delta over the agent's whole descendant tree.
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("busy_agent_child");
        let marker = dir.join("idle-since");
        let stopped = dir.join("STOPPED");
        let stop_cmd = format!("touch {}", stopped.display());
        let token = "lazybox-test-blocked-agent";
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", token),
            ("LAZYBOX_IDLE_AGENT_CPU_SECS", "1"),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
        ];

        // Agent (argv carries the watched token) sleeps while a *child* spins
        // the CPU — the agent accrues no CPU of its own.
        let mut agent = Command::new(&bash)
            .args(["-c", "( while :; do :; done ) & sleep 30", token])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn blocked agent");

        // Tick 1: newly-seen tree → active, clears the stale marker.
        fs::write(&marker, "1").expect("stale marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_1 = stopped.exists();
        let cleared_1 = !marker.exists();

        // Tick 2: the agent is idle but its child has burned CPU. The tree
        // delta must keep the box alive across a second consecutive tick.
        sleep(Duration::from_secs(3));
        fs::write(&marker, "1").expect("stale marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_2 = stopped.exists();
        let cleared_2 = !marker.exists();

        let _ = agent.kill();
        let _ = agent.wait();

        assert!(!stopped_1, "a live agent tree must not be stopped");
        assert!(cleared_1, "a newly-seen agent tree clears the idle marker");
        assert!(
            !stopped_2,
            "an agent blocked on a CPU-burning child stays active"
        );
        assert!(
            cleared_2,
            "the child's CPU delta since the last tick must clear the marker"
        );
    }

    #[test]
    fn an_idle_agent_tree_still_stops_after_the_window() {
        // The dual of the fix: an agent whose whole tree is genuinely idle
        // (agent + an idle child) must still be reaped once the window passes,
        // so the descendant walk doesn't wedge the box permanently awake.
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("idle_agent_tree");
        let marker = dir.join("idle-since");
        let stopped = dir.join("STOPPED");
        let stop_cmd = format!("touch {}", stopped.display());
        let token = "lazybox-test-idle-tree-agent";
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", token),
            ("LAZYBOX_IDLE_AGENT_CPU_SECS", "1"),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
        ];

        // Agent and child both sleep — no CPU accrues anywhere in the tree.
        let mut agent = Command::new(&bash)
            .args(["-c", "( sleep 30 ) & sleep 30", token])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn idle agent");

        // Tick 1: newly-seen → active, snapshots the tree's CPU.
        fs::remove_file(&marker).ok();
        run_idle(&bash, &marker, &env, None);
        let stopped_1 = stopped.exists();

        // Tick 2: no CPU delta, no new pids → idle → stamps a fresh marker.
        run_idle(&bash, &marker, &env, None);
        let stopped_2 = stopped.exists();
        let stamped = marker.exists();

        // Tick 3: backdate the marker past the window → the idle tree stops.
        fs::write(&marker, "1").expect("backdate marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_3 = stopped.exists();

        let _ = agent.kill();
        let _ = agent.wait();

        assert!(!stopped_1, "tick 1 (newly-seen) must not stop");
        assert!(!stopped_2, "tick 2 (fresh marker) must not stop");
        assert!(stamped, "an idle tree must stamp the idle marker");
        assert!(
            stopped_3,
            "a genuinely idle agent tree must still stop past the window"
        );
    }

    #[test]
    fn a_fresh_daemon_liveness_file_keeps_the_box_alive() {
        // The daemon touches ~/.lazybox/run/active while it holds a live PTY,
        // so a client attached over a relay (not inbound sshd) still counts as
        // busy. A fresh mtime must refuse to stop even past the idle window; a
        // stale one must proceed.
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("daemon_liveness");
        let marker = dir.join("idle-since");
        let stopped = dir.join("STOPPED");
        let active = dir.join("active");
        let stop_cmd = format!("touch {}", stopped.display());

        // Fresh liveness file + a backdated marker: must NOT stop.
        fs::write(&active, "1").expect("write active file");
        fs::write(&marker, "1").expect("stale marker");
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", "lazybox-absent-agent"),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
            ("LAZYBOX_IDLE_ACTIVE_FILE", active.to_str().unwrap()),
            ("LAZYBOX_IDLE_ACTIVE_MAX_AGE", "600"),
        ];
        run_idle(&bash, &marker, &env, None);
        assert!(
            !stopped.exists(),
            "a fresh daemon liveness file must keep the box alive"
        );
        assert!(
            !marker.exists(),
            "an active daemon must clear the idle marker"
        );

        // Same file, now treated as stale (max-age 1s, aged 2s): must stop.
        fs::write(&active, "1").expect("rewrite active file");
        sleep(Duration::from_secs(2));
        fs::write(&marker, "1").expect("stale marker");
        let env_stale = [
            ("LAZYBOX_IDLE_AGENT_PROCS", "lazybox-absent-agent"),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
            ("LAZYBOX_IDLE_ACTIVE_FILE", active.to_str().unwrap()),
            ("LAZYBOX_IDLE_ACTIVE_MAX_AGE", "1"),
        ];
        run_idle(&bash, &marker, &env_stale, None);
        assert!(
            stopped.exists(),
            "a stale daemon liveness file must not keep the box alive"
        );
    }

    #[test]
    fn script_survives_an_unset_home() {
        // A systemd oneshot without `User=` can run with $HOME unset. The
        // liveness-file default expands `$HOME`, and under `set -u` a bare
        // `$HOME` would abort the whole check every tick — the box would then
        // never reap. The default must tolerate an unset $HOME (falls back to
        // root's home) and still run the idle decision to completion.
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("unset_home");
        let marker = dir.join("idle-since");

        // Build the command by hand: `run_idle` pins LAZYBOX_IDLE_ACTIVE_FILE,
        // which would bypass the `$HOME` expansion this test must exercise.
        let mut cmd = Command::new(&bash);
        cmd.arg(lifecycle_dir().join("lazybox-idle-stop.sh"));
        cmd.env("LAZYBOX_IDLE_MARKER", &marker);
        cmd.env("LAZYBOX_IDLE_SSH_PORT", "65533");
        cmd.env("LAZYBOX_IDLE_AGENT_PROCS", "lazybox-absent-agent");
        cmd.env_remove("HOME");
        cmd.env_remove("LAZYBOX_IDLE_ACTIVE_FILE");
        let out = cmd.output().expect("run lazybox-idle-stop.sh");

        assert!(
            out.status.success(),
            "the idle check must not abort when $HOME is unset: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            marker.exists(),
            "with $HOME unset the first idle tick must still stamp the marker, \
             proving the check ran to completion rather than aborting on `set -u`"
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
