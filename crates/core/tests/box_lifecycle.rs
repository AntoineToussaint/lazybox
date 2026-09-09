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
    for name in [
        "lazybox-idle-stop.sh",
        "connect.sh",
        "lazybox-build.sh",
        "lazybox-direct-service.sh",
    ] {
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
    let direct = read("lazybox-direct-service.sh");
    assert!(
        build.contains("LAZYBOX_SERVICE_MODE")
            && build.contains("lazybox-direct-service.sh")
            && direct.contains("server stop")
            && direct.contains("server start")
            && direct.contains("server status"),
        "build helper direct mode must restart the daemon and verify readiness without systemd"
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
        for name in [
            "lazybox-idle-stop.sh",
            "connect.sh",
            "lazybox-build.sh",
            "lazybox-direct-service.sh",
        ] {
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
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Output, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    /// Own a fixture's whole process group. Killing only the shell leaves a
    /// background CPU child reparented to pid 1, which was #1163's leak.
    struct FixtureProcessGroup {
        child: Child,
        pgid: i32,
    }

    impl FixtureProcessGroup {
        fn spawn(mut command: Command) -> Self {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            let child = command.spawn().expect("spawn fixture process group");
            let pgid = child.id() as i32;
            Self { child, pgid }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }
    }

    impl Drop for FixtureProcessGroup {
        fn drop(&mut self) {
            // SAFETY: `process_group(0)` made this child the leader of a
            // dedicated group containing only this test fixture's tree.
            unsafe {
                libc::killpg(self.pgid, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
    }

    /// Stand-in for the SSH port in every behavioral run. Privileged, so it is
    /// never handed out as an ephemeral local port (see `run_idle`).
    const FAKE_SSH_PORT: &str = "1";

    /// A process-unique name for a fixture's agent argv. The idle-stop script
    /// resolves it with `pgrep -f` against the host's whole process table, so a
    /// fixed string would let two copies of this suite running on one box (the
    /// normal state of this repo) match each other's fixtures and invert the
    /// assertions.
    fn agent_token(base: &str) -> String {
        format!("{base}-{}", std::process::id())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    // Run the idle-stop script with a temp marker and a bogus SSH port so no
    // real connection is ever counted as an active tunnel. `env` supplies the
    // per-test knobs; `path_prefix` is prepended to PATH for command stubs.
    //
    // The port has to sit *below* the ephemeral range: the connection scan
    // matches on port number alone, so a fake port up there is eventually
    // claimed as the local end of some unrelated outbound socket — one
    // browser connection on 65533 and every test expecting a stop reads the
    // box as "someone is connected" and fails.
    fn run_idle(
        bash: &Path,
        marker: &Path,
        env: &[(&str, &str)],
        path_prefix: Option<&Path>,
    ) -> Output {
        let mut cmd = Command::new(bash);
        cmd.arg(lifecycle_dir().join("lazybox-idle-stop.sh"));
        cmd.env("LAZYBOX_IDLE_MARKER", marker);
        cmd.env("LAZYBOX_IDLE_SSH_PORT", FAKE_SSH_PORT);
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

    /// `LAZYBOX_IDLE_AGENT_CPU_SECS` the CPU fixtures run the detector at: the
    /// whole-second tree delta a tick must see to call the box busy.
    const AGENT_CPU_SECS: u64 = 1;

    /// How long a spinner fixture is given to burn that delta. Generous
    /// because the box these tests run on is routinely loaded — a full
    /// `cargo test --workspace` competes with the spinner for cores, and a
    /// backgrounded subshell gets the thinnest slice of all.
    const CPU_DELTA_TIMEOUT: Duration = Duration::from_secs(30);

    /// Sampling interval while waiting. Each sample is a full `ps` of the box
    /// (~25 ms on a 1200-process host), so a tight loop would spend a quarter
    /// of a core bidding against the very fixture it is waiting for — on the
    /// loaded box these tests exist to survive. The wait is for whole
    /// CPU-seconds; half a second of resolution costs nothing.
    const CPU_POLL_INTERVAL: Duration = Duration::from_millis(500);

    /// The previous tick's per-pid CPU snapshot, read from the detector's own
    /// `${MARKER}.agent-cpu` file rather than taken independently.
    ///
    /// This is the baseline the next tick will actually diff against, so the
    /// wait can never disagree with the tick about where the window starts,
    /// and the file is the script's output — no second copy of its bookkeeping
    /// to keep in step.
    fn detector_snapshot(marker: &Path) -> HashMap<u32, u64> {
        let path = PathBuf::from(format!("{}.agent-cpu", marker.display()));
        let Ok(body) = fs::read_to_string(&path) else {
            return HashMap::new();
        };
        body.lines()
            .filter_map(|line| {
                let (pid, secs) = line.split_once(' ')?;
                Some((pid.trim().parse().ok()?, secs.trim().parse().ok()?))
            })
            .collect()
    }

    /// What a tick taken right now would read from the fixture's process tree,
    /// carrying the raw observation so a failure can tell a starved fixture
    /// from a `ps` that never answered.
    struct TreeReading {
        /// Rows `ps` returned for the whole box. `Err` is the command failing
        /// to run at all; `Ok(0)` is it running but emitting nothing in the
        /// `pid ppid time` shape asked for.
        ps_rows: Result<usize, String>,
        /// `(pid, previous, current)` in whole CPU-seconds. A `None` previous
        /// is a pid the detector's last tick never recorded.
        tree: Vec<(u32, Option<u64>, u64)>,
        delta: u64,
    }

    impl TreeReading {
        /// Whether the next tick is guaranteed to read this tree as busy.
        ///
        /// Deliberately stricter than the detector: for a pid it has a
        /// baseline for, the tick diffs against the same snapshot and reads it
        /// no earlier than we did, so its delta is at least ours; for a pid the
        /// snapshot lacks, the tick calls the tree active outright while we
        /// still make it earn the seconds from zero.
        fn reads_busy(&self) -> bool {
            self.delta >= AGENT_CPU_SECS
        }

        /// Whether the detector's snapshot covered any of the tree. Without
        /// this the wait would still terminate, but against a zero baseline —
        /// it would be measuring lifetime CPU, not the delta the tick uses.
        fn has_baseline(&self) -> bool {
            self.tree.iter().any(|(_, prev, _)| prev.is_some())
        }
    }

    impl std::fmt::Display for TreeReading {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match &self.ps_rows {
                Err(error) => return write!(f, "`ps` did not run: {error}"),
                Ok(0) => {
                    return write!(
                        f,
                        "`ps -eo pid=,ppid=,time=` returned no usable rows — nothing to do with                          load, this `ps` does not speak those flags"
                    );
                }
                Ok(rows) => write!(f, "{rows} processes on the box; tree [")?,
            }
            for (index, (pid, prev, cur)) in self.tree.iter().enumerate() {
                if index > 0 {
                    write!(f, ", ")?;
                }
                match prev {
                    Some(prev) => write!(f, "{pid}: {prev}s→{cur}s")?,
                    None => write!(f, "{pid}: absent from the last tick→{cur}s")?,
                }
            }
            write!(f, "]; delta {}s, need {AGENT_CPU_SECS}s", self.delta)
        }
    }

    /// Read the tree rooted at `root` and diff it against the detector's
    /// snapshot. A pid the snapshot lacks counts from zero rather than being
    /// dropped: the fixture's CPU lives in a subshell that appears a few tens
    /// of milliseconds after the shell, so a pid can legitimately be younger
    /// than the tick that established the baseline, and dropping it would make
    /// the only process burning CPU invisible to the wait.
    fn read_tree(root: u32, prev: &HashMap<u32, u64>) -> TreeReading {
        let stdout = match Command::new("ps")
            .args(["-eo", "pid=,ppid=,time="])
            .output()
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
            Err(error) => {
                return TreeReading {
                    ps_rows: Err(error.to_string()),
                    tree: Vec::new(),
                    delta: 0,
                };
            }
        };
        let rows: Vec<(u32, u32, u64)> = stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse().ok()?;
                let ppid = fields.next()?.parse().ok()?;
                Some((pid, ppid, parse_cpu_secs(fields.next()?)))
            })
            .collect();

        let mut tree = Vec::new();
        let mut pending = vec![root];
        while let Some(pid) = pending.pop() {
            for (candidate, parent, secs) in &rows {
                if *candidate == pid {
                    tree.push((pid, prev.get(&pid).copied(), *secs));
                }
                if *parent == pid && *candidate != pid {
                    pending.push(*candidate);
                }
            }
        }
        let delta = tree
            .iter()
            .filter_map(|(_, prev, cur)| cur.checked_sub(prev.unwrap_or(0)))
            .sum();
        TreeReading {
            ps_rows: Ok(rows.len()),
            tree,
            delta,
        }
    }

    /// `ps`'s `[D-]HH:MM:SS[.ss]` TIME column, truncated to whole seconds the
    /// way the script's awk does.
    fn parse_cpu_secs(field: &str) -> u64 {
        let (days, rest) = match field.split_once('-') {
            Some((days, rest)) => (days.parse().unwrap_or(0.0), rest),
            None => (0.0, field),
        };
        let secs = rest.split(':').fold(0.0, |acc: f64, part| {
            acc * 60.0 + part.parse::<f64>().unwrap_or(0.0)
        });
        (days * 86_400.0 + secs) as u64
    }

    /// Wait until a tick would read the fixture's tree as busy, returning the
    /// final reading either way.
    ///
    /// The tick window has to be a function of CPU burned, not wall clock: on a
    /// loaded box a fixed sleep elapses with the spinner descheduled, the tick
    /// reads a sub-threshold delta, and a test about the *tree walk* fails as
    /// if the walk were broken.
    fn wait_until_a_tick_reads_busy(root: u32, marker: &Path) -> TreeReading {
        let prev = detector_snapshot(marker);
        let deadline = Instant::now() + CPU_DELTA_TIMEOUT;
        loop {
            let reading = read_tree(root, &prev);
            if reading.reads_busy() || Instant::now() >= deadline {
                return reading;
            }
            sleep(CPU_POLL_INTERVAL);
        }
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

    /// A pid the detector's snapshot never recorded must count from zero, not
    /// vanish from the delta.
    ///
    /// The blocked-agent fixture keeps all its CPU in a subshell that becomes
    /// visible tens of milliseconds after the shell — the same order as the
    /// tick that establishes the baseline. Dropping snapshot-less pids made the
    /// only process burning CPU invisible to the wait, which then spun out its
    /// whole deadline on a perfectly idle box and blamed the load.
    #[test]
    fn a_pid_missing_from_the_snapshot_still_counts_toward_the_delta() {
        let Ok(bash) = which_bash() else { return };
        let mut command = Command::new(&bash);
        command
            .args([
                "-c",
                "( end=$((SECONDS+30)); while (( SECONDS < end )); do :; done ) & wait",
                "lazybox-test-unsnapshotted-child",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let agent = FixtureProcessGroup::spawn(command);

        // A baseline that knows the shell but not the child it forked — what a
        // tick that ran before the fork leaves behind.
        let prev = HashMap::from([(agent.pid(), 0)]);
        let deadline = Instant::now() + CPU_DELTA_TIMEOUT;
        let mut reading = read_tree(agent.pid(), &prev);
        while !reading.reads_busy() && Instant::now() < deadline {
            sleep(CPU_POLL_INTERVAL);
            reading = read_tree(agent.pid(), &prev);
        }
        let saw_unsnapshotted_child = reading
            .tree
            .iter()
            .any(|(pid, prev, _)| *pid != agent.pid() && prev.is_none());
        let busy = reading.reads_busy();

        drop(agent);

        assert!(
            saw_unsnapshotted_child,
            "the fixture's child must show up in the tree with no baseline: {reading}"
        );
        assert!(
            busy,
            "a child the snapshot never saw must still carry the tree past the \
             threshold: {reading}"
        );
    }

    /// A wait that ends empty-handed has to report what it saw. The same zero
    /// delta is produced by a starved fixture, by a tree that vanished, and by
    /// a `ps` that never answered — naming one of them in the failure sends
    /// the next reader down the wrong path, which is the cost this whole
    /// fixture exists to avoid.
    #[test]
    fn a_reading_reports_the_observation_rather_than_diagnosing_load() {
        let unavailable = TreeReading {
            ps_rows: Err("No such file or directory (os error 2)".into()),
            tree: Vec::new(),
            delta: 0,
        };
        assert!(
            unavailable.to_string().contains("`ps` did not run"),
            "{unavailable}"
        );

        let wrong_flags = TreeReading {
            ps_rows: Ok(0),
            tree: Vec::new(),
            delta: 0,
        };
        assert!(
            wrong_flags
                .to_string()
                .contains("does not speak those flags"),
            "{wrong_flags}"
        );

        let starved = TreeReading {
            ps_rows: Ok(900),
            tree: vec![(7, Some(3), 3), (8, None, 0)],
            delta: 0,
        };
        let rendered = starved.to_string();
        assert!(rendered.contains("7: 3s→3s"), "{rendered}");
        assert!(
            rendered.contains("8: absent from the last tick→0s"),
            "{rendered}"
        );
        assert!(rendered.contains("delta 0s, need 1s"), "{rendered}");
        assert!(starved.has_baseline(), "pid 7 carried a baseline");
        assert!(!starved.reads_busy());

        let idle_tree_with_no_baseline = TreeReading {
            ps_rows: Ok(900),
            tree: vec![(8, None, 0)],
            delta: 0,
        };
        assert!(
            !idle_tree_with_no_baseline.has_baseline(),
            "a tree the snapshot never covered is not a baseline"
        );
    }

    #[test]
    fn a_working_agent_is_not_reaped_mid_task() {
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("busy_agent");
        let marker = dir.join("idle-since");
        let stopped = dir.join("STOPPED");
        let stop_cmd = format!("touch {}", stopped.display());
        let token = agent_token("lazybox-test-working-agent");
        let cpu_secs = AGENT_CPU_SECS.to_string();
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", token.as_str()),
            ("LAZYBOX_IDLE_AGENT_CPU_SECS", cpu_secs.as_str()),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
        ];

        // A bounded CPU spinner, its argv carrying the watched token. The
        // deadline is a second backstop behind the process-group Drop guard:
        // even a hard-aborted test can never leak an infinite hot loop. It has
        // to outlast `CPU_DELTA_TIMEOUT` — a spinner that exits mid-wait can
        // never reach the CPU the tick below needs.
        let mut command = Command::new(&bash);
        command
            .args([
                "-c",
                "end=$((SECONDS+60)); while (( SECONDS < end )); do :; done",
                token.as_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let agent = FixtureProcessGroup::spawn(command);

        // Tick 1 sees a newly-observed process → active, clears the stale marker.
        fs::write(&marker, "1").expect("stale marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_1 = stopped.exists();
        let cleared_1 = !marker.exists();

        // Tick 2 must keep it alive on the CPU *delta* (not newness): re-stale
        // the marker, let the agent burn CPU, run again.
        let reading = wait_until_a_tick_reads_busy(agent.pid(), &marker);
        fs::write(&marker, "1").expect("stale marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_2 = stopped.exists();
        let cleared_2 = !marker.exists();

        // Teardown before assertions; Drop still runs on every earlier panic.
        drop(agent);

        assert!(
            reading.has_baseline(),
            "tick 1 left no CPU snapshot covering the fixture tree, so the wait had \
             nothing to diff against: {reading}"
        );
        assert!(
            reading.reads_busy(),
            "no tick would have read this tree as busy within {CPU_DELTA_TIMEOUT:?}: {reading}"
        );
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
    fn a_sibling_runs_fixture_is_invisible_to_this_run() {
        // Two copies of this suite share the host's process table, so a fixture
        // named with a run-independent string lets the other run's CPU burner
        // read as this run's live agent and the reap never fires. Stand in for
        // that sibling with a burner carrying the bare base name.
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("sibling_fixture");
        let marker = dir.join("idle-since");
        let stopped = dir.join("STOPPED");
        let stop_cmd = format!("touch {}", stopped.display());
        let base = "lazybox-test-sibling-agent";
        let token = agent_token(base);
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", token.as_str()),
            ("LAZYBOX_IDLE_AGENT_CPU_SECS", "1"),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
        ];

        let mut command = Command::new(&bash);
        command
            .args([
                "-c",
                "end=$((SECONDS+10)); while (( SECONDS < end )); do :; done",
                base,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let sibling = FixtureProcessGroup::spawn(command);

        fs::write(&marker, "1").expect("stale marker");
        let out = run_idle(&bash, &marker, &env, None);
        let succeeded = out.status.success();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let stopped_now = stopped.exists();

        drop(sibling);

        assert!(succeeded, "idle-stop exited non-zero: {stderr}");
        assert!(
            stopped_now,
            "another run's fixture must not register as this run's agent"
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
        let token = agent_token("lazybox-test-blocked-agent");
        let cpu_secs = AGENT_CPU_SECS.to_string();
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", token.as_str()),
            ("LAZYBOX_IDLE_AGENT_CPU_SECS", cpu_secs.as_str()),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
        ];

        // Agent (argv carries the watched token) waits while a bounded child
        // spins. The shell itself accrues no CPU. A dedicated process group
        // guarantees teardown reaches the child as well as the shell.
        let mut command = Command::new(&bash);
        command
            .args([
                "-c",
                "( end=$((SECONDS+60)); while (( SECONDS < end )); do :; done ) & wait",
                token.as_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let agent = FixtureProcessGroup::spawn(command);

        // Tick 1: newly-seen tree → active, clears the stale marker.
        fs::write(&marker, "1").expect("stale marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_1 = stopped.exists();
        let cleared_1 = !marker.exists();

        // Tick 2: the agent is idle but its child has burned CPU. The tree
        // delta must keep the box alive across a second consecutive tick.
        let reading = wait_until_a_tick_reads_busy(agent.pid(), &marker);
        fs::write(&marker, "1").expect("stale marker");
        run_idle(&bash, &marker, &env, None);
        let stopped_2 = stopped.exists();
        let cleared_2 = !marker.exists();

        drop(agent);

        assert!(
            reading.has_baseline(),
            "tick 1 left no CPU snapshot covering the fixture tree, so the wait had \
             nothing to diff against: {reading}"
        );
        assert!(
            reading.reads_busy(),
            "no tick would have read this tree as busy within {CPU_DELTA_TIMEOUT:?}: {reading}"
        );
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
        let token = agent_token("lazybox-test-idle-tree-agent");
        let cpu_secs = AGENT_CPU_SECS.to_string();
        let env = [
            ("LAZYBOX_IDLE_AGENT_PROCS", token.as_str()),
            ("LAZYBOX_IDLE_AGENT_CPU_SECS", cpu_secs.as_str()),
            ("LAZYBOX_IDLE_STOP_CMD", stop_cmd.as_str()),
        ];

        // Agent and child both sleep — no CPU accrues anywhere in the tree.
        let mut agent = Command::new(&bash)
            .args(["-c", "( sleep 30 ) & sleep 30", token.as_str()])
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
        cmd.env("LAZYBOX_IDLE_SSH_PORT", FAKE_SSH_PORT);
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

    #[test]
    fn direct_service_restart_fails_when_the_daemon_never_starts() {
        let Ok(bash) = which_bash() else { return };
        let dir = scratch("direct_service_start_failure");
        let state = dir.join("state");
        let fake = dir.join("lazybox");
        write_exec(
            &fake,
            &format!(
                "#!/usr/bin/env bash\ncase \"$2\" in\n  stop) printf stopped > {state};;\n  status) cat {state};;\n  start) exit 7;;\nesac\n",
                state = state.display()
            ),
        );

        let output = Command::new(&bash)
            .arg(lifecycle_dir().join("lazybox-direct-service.sh"))
            .env("HOME", &dir)
            .env("LAZYBOX_BIN_DEST", &fake)
            .env("LAZYBOX_DIRECT_SERVICE_ATTEMPTS", "5")
            .env("LAZYBOX_DIRECT_SERVICE_LOG", dir.join("daemon.log"))
            .output()
            .expect("run direct service helper");

        assert!(
            !output.status.success(),
            "a failed background daemon start must propagate to provisioning"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("exited before becoming ready"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
