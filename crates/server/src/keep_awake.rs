//! Opt-in sleep inhibition scoped to agent activity (`ui.keep_awake`).
//!
//! While ≥1 agent terminal is `Working`, the daemon holds an OS
//! sleep-inhibitor child process — `caffeinate` on macOS,
//! `systemd-inhibit` on Linux — and kills it the moment every agent
//! goes idle, so the machine sleeps normally between runs. Both
//! commands are additionally tethered to the daemon's pid
//! (`caffeinate -w` / `tail --pid`), so even a SIGKILL'd daemon
//! cannot leak the inhibition past its own lifetime.
//!
//! Linux without systemd: spawning `systemd-inhibit` fails, a warning
//! is logged, and sleep behavior is unchanged — there is no portable
//! fallback worth shipping.

use lazybox_ipc::{AgentState, Event};
use std::process::{Child, Command, Stdio};
use tokio::sync::broadcast;

use crate::ServerConfig;

/// Spawn the keep-awake watcher. No-op (`None`) unless the user opted
/// in via `ui.keep_awake` — and on platforms with no known inhibitor.
pub fn spawn(config: ServerConfig, enabled: bool) -> Option<tokio::task::JoinHandle<()>> {
    if !enabled {
        return None;
    }
    let Some(argv) = inhibit_argv(std::process::id()) else {
        tracing::warn!("ui.keep_awake is set but no sleep inhibitor exists for this platform");
        return None;
    };
    Some(tokio::spawn(run(config, argv)))
}

/// Watch the event bus and mirror "any agent `Working`" into the
/// inhibitor. Every `AgentState` transition passes over the bus, so
/// recomputing from the authoritative `agent_states` map on each one
/// (plus `TerminalExited` for teardown sweeps, plus lag recovery)
/// converges even if individual events are missed.
async fn run(config: ServerConfig, argv: Vec<String>) {
    let mut rx = config.bus.subscribe();
    let mut inhibitor = Inhibitor::new(argv);
    loop {
        match rx.recv().await {
            Ok(Event::AgentState { .. } | Event::TerminalExited { .. })
            | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
        let active = config
            .agent_states
            .lock()
            .await
            .values()
            .any(|s| matches!(s, AgentState::Working));
        inhibitor.set_active(active);
    }
    // Bus closed = daemon shutdown; Drop releases a held inhibitor.
}

/// The platform's inhibitor command line, or `None` when the platform
/// has no supported inhibitor. `daemon_pid` tethers the child to the
/// daemon so it can never outlive it.
fn inhibit_argv(daemon_pid: u32) -> Option<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        // -d display, -i idle, -m disk, -s system (on AC); -w exits
        // the assertion when the daemon pid does.
        Some(
            ["caffeinate", "-dims", "-w", &daemon_pid.to_string()]
                .map(String::from)
                .to_vec(),
        )
    }
    #[cfg(target_os = "linux")]
    {
        // systemd-inhibit holds the lock for as long as the wrapped
        // command runs; `tail --pid` blocks until the daemon exits.
        Some(
            [
                "systemd-inhibit",
                "--what=idle:sleep",
                "--who=lazybox",
                "--why=lazybox agents running",
                "--mode=block",
                "tail",
                "--pid",
                &daemon_pid.to_string(),
                "-f",
                "/dev/null",
            ]
            .map(String::from)
            .to_vec(),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = daemon_pid;
        None
    }
}

/// Owns at most one inhibitor child. `set_active(true)` spawns it (or
/// respawns if it died underneath us), `set_active(false)` and `Drop`
/// kill and reap it.
struct Inhibitor {
    argv: Vec<String>,
    child: Option<Child>,
}

impl Inhibitor {
    fn new(argv: Vec<String>) -> Self {
        Self { argv, child: None }
    }

    #[cfg(test)]
    fn holding(&self) -> bool {
        self.child.is_some()
    }

    fn set_active(&mut self, active: bool) {
        if active {
            self.acquire();
        } else {
            self.release();
        }
    }

    fn acquire(&mut self) {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(None) => return,
                // Died underneath us (e.g. binary missing at first,
                // installed since) — reap and fall through to respawn.
                _ => self.child = None,
            }
        }
        let mut cmd = Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Own process group so release can take down the wrapped
        // command (systemd-inhibit's tail) along with the wrapper.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        match cmd.spawn() {
            Ok(child) => {
                tracing::info!(pid = child.id(), cmd = %self.argv[0], "keep-awake: holding sleep inhibitor");
                self.child = Some(child);
            }
            Err(e) => {
                tracing::warn!("keep-awake: failed to spawn {}: {e}", self.argv[0]);
            }
        }
    }

    fn release(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        tracing::info!(pid = child.id(), "keep-awake: releasing sleep inhibitor");
        #[cfg(unix)]
        // SAFETY: plain killpg on the child's own process group.
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGTERM);
        }
        // SIGKILL backstop keeps the reaping `wait` from ever
        // blocking on a child that ignores SIGTERM.
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 probes existence without sending anything.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn argv_is_caffeinate_tethered_to_daemon_pid() {
        let argv = inhibit_argv(4242).expect("macOS has an inhibitor");
        assert_eq!(argv[0], "caffeinate");
        assert_eq!(argv[2..4], ["-w".to_string(), "4242".to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn argv_is_systemd_inhibit_tethered_to_daemon_pid() {
        let argv = inhibit_argv(4242).expect("Linux has an inhibitor");
        assert_eq!(argv[0], "systemd-inhibit");
        assert!(argv.contains(&"--what=idle:sleep".to_string()));
        assert!(argv.contains(&"4242".to_string()));
    }

    /// The full hold/release cycle against a real child process:
    /// acquire spawns it, a second acquire is a no-op on the same
    /// child, release kills and reaps it.
    #[test]
    fn inhibitor_holds_and_releases_a_child() {
        let mut inh = Inhibitor::new(vec!["sleep".into(), "300".into()]);
        assert!(!inh.holding());

        inh.set_active(true);
        assert!(inh.holding());
        let pid = inh.child.as_ref().expect("spawned").id();
        assert!(alive(pid));

        inh.set_active(true);
        assert_eq!(inh.child.as_ref().expect("still held").id(), pid);

        inh.set_active(false);
        assert!(!inh.holding());
        assert!(!alive(pid), "release must kill the inhibitor child");

        inh.set_active(false);
    }

    /// A dead child (crashed inhibitor binary) must not satisfy
    /// `acquire` forever — the next activation respawns.
    #[test]
    fn acquire_respawns_a_dead_child() {
        let mut inh = Inhibitor::new(vec!["true".into()]);
        inh.set_active(true);
        let first = inh.child.as_mut().expect("spawned");
        first.wait().expect("`true` exits immediately");
        inh.set_active(true);
        assert!(inh.holding());
    }

    /// Dropping a holding inhibitor (daemon shutdown path) kills the
    /// child — the assertion can't leak past the watcher task.
    #[test]
    fn drop_releases_a_held_child() {
        let mut inh = Inhibitor::new(vec!["sleep".into(), "300".into()]);
        inh.set_active(true);
        let pid = inh.child.as_ref().expect("spawned").id();
        drop(inh);
        assert!(!alive(pid), "drop must kill the inhibitor child");
    }

    /// A missing inhibitor binary degrades to a warning, not a panic,
    /// and the inhibitor simply doesn't hold.
    #[test]
    fn missing_binary_is_not_fatal() {
        let mut inh = Inhibitor::new(vec!["lazybox-no-such-inhibitor".into()]);
        inh.set_active(true);
        assert!(!inh.holding());
    }
}
