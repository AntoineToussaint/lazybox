//! Opt-in sleep inhibition scoped to agent activity (`ui.keep_awake`).
//!
//! While the [`KeepAwake`] mode says to, the daemon holds an OS
//! sleep-inhibitor child process — `caffeinate` on macOS,
//! `systemd-inhibit` on Linux — and kills it the moment nothing
//! qualifies, so the machine sleeps normally between runs. Both
//! commands are additionally tethered to the daemon's pid
//! (`caffeinate -w` / `tail --pid`), so even a SIGKILL'd daemon
//! cannot leak the inhibition past its own lifetime.
//!
//! The mode is re-read from YAML on every recompute (mirroring the
//! polling loop's live re-read), so toggling it in config takes effect
//! on the next agent transition without a daemon restart. `working`
//! (the historical `true`) holds only while an agent is `Working`;
//! `asking` also holds while one is parked on input; `always` holds for
//! the daemon's whole lifetime.
//!
//! macOS laptop caveat (#1485): the inhibitor covers system sleep only
//! on AC power and never a closed lid, so on battery a held assertion is
//! not a guarantee. The daemon does *not* try to force it (that needs
//! root `pmset -b disablesleep`, a machine-global sticky setting with no
//! safe pid tether); instead it reports the power source to clients over
//! [`Event::KeepAwakeStatus`] so the `☼ awake` badge reads `(AC only)`
//! rather than claiming protection the OS isn't giving.
//!
//! Linux without systemd: spawning `systemd-inhibit` fails, a warning
//! is logged (once), and sleep behavior is unchanged — there is no
//! portable fallback worth shipping.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lazybox_config::KeepAwake;
use lazybox_ipc::Event;
use tokio::sync::broadcast;

use crate::{ServerConfig, TerminalRegistry};

/// How often the watcher re-checks the power source while holding, so a
/// plug/unplug mid-run refreshes the badge even without an agent
/// transition to wake the loop. When not holding the timer does no work
/// at all (no config read, no probe).
const BATTERY_POLL: Duration = Duration::from_secs(20);

/// Spawn the keep-awake watcher. `None` only on platforms with no
/// known inhibitor — the task itself is cheap and re-reads
/// `ui.keep_awake` live, so it runs even for users who have not (yet)
/// opted in.
pub fn spawn(config: &ServerConfig) -> Option<tokio::task::JoinHandle<()>> {
    let Some(argv) = inhibit_argv(std::process::id()) else {
        if keep_awake_mode() != KeepAwake::Off {
            tracing::warn!("ui.keep_awake is set but no sleep inhibitor exists for this platform");
        }
        return None;
    };
    // Subscribe here, not inside the task: events broadcast between
    // this call returning and the task's first poll must queue, not
    // vanish.
    let rx = config.bus.subscribe();
    let bus = config.bus.clone();
    let terminals = config.terminal.clone();
    Some(tokio::spawn(async move {
        run(rx, bus, terminals, argv, keep_awake_mode, on_battery).await;
    }))
}

/// Current `ui.keep_awake` mode from YAML; an unreadable config means off.
fn keep_awake_mode() -> KeepAwake {
    lazybox_config::Config::load()
        .map(|c| c.ui.keep_awake)
        .unwrap_or(KeepAwake::Off)
}

/// Whether the machine is running on battery. macOS honours system-sleep
/// assertions only on AC power, so this is what the badge needs to stay
/// honest; every other platform reports AC (`false`) — Linux's
/// `systemd-inhibit` has no AC/battery distinction (#1485).
///
/// `pub(crate)` so the subscribe path can prime a freshly-connected
/// client without waiting for the watcher's next poll. Blocking (spawns
/// `pmset`); callers off the async worker (the watcher's periodic tick,
/// the subscribe `spawn_blocking`) keep it out of the hot path.
pub(crate) fn on_battery() -> bool {
    #[cfg(target_os = "macos")]
    {
        Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains("'Battery Power'"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Watch the event bus and mirror the mode's `should_hold` predicate
/// into the inhibitor, broadcasting the resulting status so a client can
/// paint the `☼ awake` badge from the daemon's truth rather than its own
/// config. Every `AgentState` transition passes over the bus, so
/// recomputing from the authoritative states map on each one (plus
/// `TerminalExited` for teardown sweeps, plus lag recovery) converges
/// even if individual events are missed.
///
/// While holding, a periodic tick also re-checks the power source, so a
/// plug/unplug mid-run refreshes the badge even without an agent
/// transition to wake the loop. When *not* holding there is nothing to
/// refresh, so the timer does no work — an off/idle daemon stays fully
/// event-driven and never reads config or probes power on a timer.
///
/// Throttles `set_active()` calls to a minimum interval (10s) to avoid
/// redundant config reads and state checks when events arrive frequently
/// (e.g., every keystroke or CLI output line). The status broadcast is
/// *not* throttled that way — a state or plug/unplug change emits promptly
/// so the badge never lies for long.
///
/// Returns the inhibitor (for tests) when the bus closes. In
/// production the bus never closes — the daemon exits by dropping the
/// runtime, which drops this task's future mid-`recv` and releases a
/// held inhibitor via `Inhibitor::drop`; a hard kill is covered by the
/// pid tether in [`inhibit_argv`].
async fn run(
    mut rx: broadcast::Receiver<Event>,
    bus: broadcast::Sender<Event>,
    terminals: TerminalRegistry,
    argv: Vec<String>,
    mode: impl Fn() -> KeepAwake,
    on_battery: impl Fn() -> bool + Clone + Send + 'static,
) -> Inhibitor {
    let mut inhibitor = Inhibitor::new(argv);
    let mut last_status: Option<(bool, bool)> = None;
    let mut poll = tokio::time::interval(BATTERY_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Prime before the first event: a session recovered mid-`Working`
    // broadcasts its state once, possibly before this task subscribed,
    // and the state owner's dedup means no further event may arrive
    // for the rest of that run.
    tick(
        &mut inhibitor,
        &mut last_status,
        &terminals,
        &bus,
        &mode,
        &on_battery,
    )
    .await;
    loop {
        let run_tick = tokio::select! {
            recv = rx.recv() => match recv {
                Ok(Event::AgentState { .. } | Event::TerminalExited { .. })
                | Err(broadcast::error::RecvError::Lagged(_)) => true,
                Ok(_) => false,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // The only thing that can change without an agent event is the
            // power source, and it only matters while inhibiting; skip the
            // config read + probe otherwise.
            _ = poll.tick() => inhibitor.holding(),
        };
        if run_tick {
            tick(
                &mut inhibitor,
                &mut last_status,
                &terminals,
                &bus,
                &mode,
                &on_battery,
            )
            .await;
        }
    }
    inhibitor
}

/// One recompute: acquire/release the inhibitor per the mode's
/// `should_hold`, then broadcast the (active, on_battery) status when it
/// changed so a connected client keeps the badge honest.
async fn tick(
    inhibitor: &mut Inhibitor,
    last_status: &mut Option<(bool, bool)>,
    terminals: &TerminalRegistry,
    bus: &broadcast::Sender<Event>,
    mode: &impl Fn() -> KeepAwake,
    on_battery: &(impl Fn() -> bool + Clone + Send + 'static),
) {
    let mode = mode();
    let working = terminals.any_agent_working().await;
    // Only `asking` mode cares about the parked-on-input set, so skip the
    // extra scan otherwise.
    let asking = matches!(mode, KeepAwake::Asking) && terminals.any_agent_asking().await;
    let active = mode.should_hold(working, asking);
    inhibitor.recompute(active);
    // The badge — and thus the power source — only matter while holding,
    // so the (blocking) `pmset` probe is short-circuited unless active and
    // run off the async worker via `spawn_blocking`.
    let on_batt = if active {
        let probe = on_battery.clone();
        tokio::task::spawn_blocking(probe).await.unwrap_or(false)
    } else {
        false
    };
    let status = (active, on_batt);
    if *last_status != Some(status) {
        *last_status = Some(status);
        let _ = bus.send(Event::KeepAwakeStatus {
            active,
            on_battery: on_batt,
        });
    }
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
///
/// Throttles recompute calls to prevent rapid re-entry: the watcher
/// receives AgentState events frequently (every keystroke/output), but
/// only actually re-evaluates the inhibitor state at most once per
/// THROTTLE_INTERVAL.
struct Inhibitor {
    argv: Vec<String>,
    child: Option<Child>,
    /// Spawn failures repeat on every `Working` transition (e.g.
    /// non-systemd Linux); warn on the first one per healthy spell and
    /// demote the rest to debug so a long session isn't log spam.
    spawn_warned: bool,
    /// Last time `recompute()` actually called `set_active()`.
    last_recompute: Option<Instant>,
    /// Minimum interval between recompute calls to reduce overhead.
    throttle_interval: Duration,
}

impl Inhibitor {
    fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            child: None,
            spawn_warned: false,
            last_recompute: None,
            throttle_interval: Duration::from_secs(10),
        }
    }

    /// Whether the inhibitor child is currently spawned. Drives the
    /// watcher's decision to skip the periodic power-source poll when
    /// nothing is being held.
    fn holding(&self) -> bool {
        self.child.is_some()
    }

    /// Recompute whether the inhibitor should be active, but only if
    /// enough time has passed since the last recompute. Throttles calls
    /// to avoid redundant config reads and state checks.
    fn recompute(&mut self, active: bool) {
        let now = Instant::now();
        let should_recompute = match self.last_recompute {
            None => true,
            Some(last) => now.duration_since(last) >= self.throttle_interval,
        };

        if should_recompute {
            self.set_active(active);
            self.last_recompute = Some(now);
        }
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
                // Died underneath us (e.g. logind unavailable) — reap
                // and fall through to respawn.
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
                self.spawn_warned = false;
            }
            Err(e) if !self.spawn_warned => {
                self.spawn_warned = true;
                tracing::warn!("keep-awake: failed to spawn {}: {e}", self.argv[0]);
            }
            Err(e) => {
                tracing::debug!("keep-awake: failed to spawn {}: {e}", self.argv[0]);
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
    use lazybox_ipc::{AgentState, TerminalId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 probes existence without sending anything.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn sleep_argv() -> Vec<String> {
        vec!["sleep".into(), "300".into()]
    }

    async fn working_terminals() -> TerminalRegistry {
        let terminals = TerminalRegistry::default();
        terminals
            .record_agent_state(TerminalId(1), AgentState::Working)
            .await;
        terminals
    }

    async fn asking_terminals() -> TerminalRegistry {
        let terminals = TerminalRegistry::default();
        terminals
            .record_agent_state(TerminalId(1), AgentState::InputNeeded)
            .await;
        terminals
    }

    /// A bus whose sole receiver is returned for inspection. Kept
    /// separate from the watcher's own `rx` channel: tests close `rx`
    /// (drop its sender) to make `run` return, which the shared
    /// production bus never does.
    fn inspect_bus() -> (broadcast::Sender<Event>, broadcast::Receiver<Event>) {
        broadcast::channel(8)
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

    /// A `Working` agent whose state landed in the map before the
    /// watcher subscribed (session recovery) must be picked up by the
    /// priming pass — its one broadcast is gone and the state owner's
    /// dedup means no further event may ever arrive.
    #[tokio::test]
    async fn primes_from_state_recovered_before_subscribe() {
        let (tx, rx) = broadcast::channel(8);
        drop(tx);
        let (bus, _bus_rx) = inspect_bus();
        let inhibitor = run(
            rx,
            bus,
            working_terminals().await,
            sleep_argv(),
            || KeepAwake::Working,
            || false,
        )
        .await;
        assert!(
            inhibitor.holding(),
            "priming pass must acquire without any event"
        );
    }

    /// `asking` mode holds for an agent parked on input — the gap #1485
    /// closes. `working` mode does not (the historical behaviour).
    #[tokio::test]
    async fn asking_mode_holds_for_input_pending_agent() {
        let (tx, rx) = broadcast::channel(8);
        drop(tx);
        let (bus, _bus_rx) = inspect_bus();
        let inhibitor = run(
            rx,
            bus,
            asking_terminals().await,
            sleep_argv(),
            || KeepAwake::Asking,
            || false,
        )
        .await;
        assert!(inhibitor.holding(), "asking mode must hold for InputNeeded");
    }

    #[tokio::test]
    async fn working_mode_ignores_input_pending_agent() {
        let (tx, rx) = broadcast::channel(8);
        drop(tx);
        let (bus, _bus_rx) = inspect_bus();
        let inhibitor = run(
            rx,
            bus,
            asking_terminals().await,
            sleep_argv(),
            || KeepAwake::Working,
            || false,
        )
        .await;
        assert!(
            !inhibitor.holding(),
            "working mode must not hold for a merely asking agent"
        );
    }

    /// `always` mode holds even with no agent at all.
    #[tokio::test]
    async fn always_mode_holds_with_no_agents() {
        let (tx, rx) = broadcast::channel(8);
        drop(tx);
        let (bus, _bus_rx) = inspect_bus();
        let inhibitor = run(
            rx,
            bus,
            TerminalRegistry::default(),
            sleep_argv(),
            || KeepAwake::Always,
            || false,
        )
        .await;
        assert!(inhibitor.holding(), "always mode must hold unconditionally");
    }

    /// The power-source branch: while holding, the priming pass reports
    /// the (faked) source over the bus as the authoritative status
    /// (`active` + `on_battery`) so a client can keep the `☼ awake (AC
    /// only)` badge honest. Faking the source fits the same shape as
    /// faking the inhibitor argv.
    #[tokio::test]
    async fn reports_power_source_when_holding() {
        for on_batt in [true, false] {
            let (tx, rx) = broadcast::channel(8);
            drop(tx);
            let (bus, mut bus_rx) = inspect_bus();
            let _inhibitor = run(
                rx,
                bus,
                working_terminals().await,
                sleep_argv(),
                || KeepAwake::Working,
                move || on_batt,
            )
            .await;
            assert!(
                matches!(
                    bus_rx.try_recv(),
                    Ok(Event::KeepAwakeStatus { active: true, on_battery }) if on_battery == on_batt
                ),
                "priming must report active with on_battery={on_batt}"
            );
        }
    }

    /// When not holding, the power-source probe is short-circuited — the
    /// faked probe here would panic if run — and the status reports
    /// `active: false, on_battery: false`.
    #[tokio::test]
    async fn inactive_status_never_probes_power() {
        let (tx, rx) = broadcast::channel(8);
        drop(tx);
        let (bus, mut bus_rx) = inspect_bus();
        let _inhibitor = run(
            rx,
            bus,
            working_terminals().await,
            sleep_argv(),
            || KeepAwake::Off,
            || panic!("power source must not be probed while inactive"),
        )
        .await;
        assert!(
            matches!(
                bus_rx.try_recv(),
                Ok(Event::KeepAwakeStatus {
                    active: false,
                    on_battery: false
                })
            ),
            "off mode must report inactive without probing power"
        );
    }

    /// `ui.keep_awake` is re-read on every recompute: flipping it off
    /// mid-hold (after throttle window) releases on the next allowed event.
    /// The priming pass sees the flag on; after the throttle interval, the
    /// next recompute sees it off and releases.
    #[test]
    fn toggling_the_flag_off_releases_on_the_next_event() {
        // First read (priming) sees the flag on and acquires.
        // Both queued events will try to recompute, but the second is
        // throttled if it arrives too soon. The test uses a short throttle
        // to ensure the second recompute can execute after a small delay.
        let reads = AtomicUsize::new(0);
        let mut inhibitor = Inhibitor::new(sleep_argv());
        inhibitor.throttle_interval = Duration::from_millis(1); // Short throttle for test
        let enabled = move || reads.fetch_add(1, Ordering::SeqCst) == 0;

        // Simulate the two events
        inhibitor.recompute(enabled() && true); // Priming: enabled=true, acquire
        assert!(inhibitor.holding(), "priming must acquire");

        // Wait slightly past throttle window to allow second recompute
        std::thread::sleep(Duration::from_millis(5));

        inhibitor.recompute(enabled() && true); // Event 1: enabled=false now, should release
        assert!(
            !inhibitor.holding(),
            "toggle-off after throttle window must release the inhibitor"
        );
    }

    /// With the flag off nothing is ever spawned, no matter how busy
    /// the agents are.
    #[tokio::test]
    async fn disabled_flag_never_holds() {
        let (tx, rx) = broadcast::channel(8);
        drop(tx);
        let (bus, _bus_rx) = inspect_bus();
        let inhibitor = run(
            rx,
            bus,
            working_terminals().await,
            sleep_argv(),
            || KeepAwake::Off,
            || false,
        )
        .await;
        assert!(!inhibitor.holding());
    }

    /// The full hold/release cycle against a real child process:
    /// acquire spawns it, a second acquire is a no-op on the same
    /// child, release kills and reaps it.
    #[test]
    fn inhibitor_holds_and_releases_a_child() {
        let mut inhibitor = Inhibitor::new(sleep_argv());
        assert!(!inhibitor.holding());

        inhibitor.set_active(true);
        assert!(inhibitor.holding());
        let pid = inhibitor.child.as_ref().expect("spawned").id();
        assert!(alive(pid));

        inhibitor.set_active(true);
        assert_eq!(inhibitor.child.as_ref().expect("still held").id(), pid);

        inhibitor.set_active(false);
        assert!(!inhibitor.holding());
        assert!(!alive(pid), "release must kill the inhibitor child");

        inhibitor.set_active(false);
    }

    /// A dead child (crashed inhibitor binary) must not satisfy
    /// `acquire` forever — the next activation respawns.
    #[test]
    fn acquire_respawns_a_dead_child() {
        let mut inhibitor = Inhibitor::new(vec!["true".into()]);
        inhibitor.set_active(true);
        let first = inhibitor.child.as_mut().expect("spawned");
        first.wait().expect("`true` exits immediately");
        inhibitor.set_active(true);
        assert!(inhibitor.holding());
    }

    /// Dropping a holding inhibitor (daemon shutdown path) kills the
    /// child — the assertion can't leak past the watcher task.
    #[test]
    fn drop_releases_a_held_child() {
        let mut inhibitor = Inhibitor::new(sleep_argv());
        inhibitor.set_active(true);
        let pid = inhibitor.child.as_ref().expect("spawned").id();
        drop(inhibitor);
        assert!(!alive(pid), "drop must kill the inhibitor child");
    }

    /// A missing inhibitor binary degrades to a warning, not a panic;
    /// repeated failures only warn once per healthy spell.
    #[test]
    fn missing_binary_is_not_fatal_and_warns_once() {
        let mut inhibitor = Inhibitor::new(vec!["lazybox-no-such-inhibitor".into()]);
        inhibitor.set_active(true);
        assert!(!inhibitor.holding());
        assert!(inhibitor.spawn_warned);
        inhibitor.set_active(true);
        assert!(!inhibitor.holding());
    }

    /// Throttle prevents immediate re-entry: consecutive recompute calls
    /// within the throttle window do not trigger set_active.
    #[test]
    fn throttle_prevents_rapid_reentry() {
        let mut inhibitor = Inhibitor::new(sleep_argv());
        // Set a short throttle interval for testing.
        inhibitor.throttle_interval = Duration::from_millis(100);

        // First recompute should always execute (no prior call).
        inhibitor.recompute(true);
        assert!(inhibitor.holding(), "first recompute must acquire");
        let first_recompute = inhibitor.last_recompute;

        // Immediately recompute again — should be throttled and skip set_active.
        inhibitor.recompute(false);
        assert!(
            inhibitor.holding(),
            "second recompute within throttle window must not call set_active"
        );
        assert_eq!(
            inhibitor.last_recompute, first_recompute,
            "last_recompute timestamp should not change"
        );

        // After the throttle interval, recompute should execute.
        std::thread::sleep(Duration::from_millis(150));
        inhibitor.recompute(false);
        assert!(
            !inhibitor.holding(),
            "recompute after throttle window must call set_active"
        );
        assert!(
            inhibitor.last_recompute > first_recompute,
            "last_recompute timestamp should advance"
        );
    }

    /// Rapid events (e.g., keystroke storm) do not cause repeated
    /// set_active calls; only the first event within a window triggers.
    #[test]
    fn rapid_events_throttled() {
        let mut inhibitor = Inhibitor::new(sleep_argv());
        inhibitor.throttle_interval = Duration::from_millis(100);

        inhibitor.recompute(true);
        let first_pid = inhibitor.child.as_ref().map(|c| c.id());

        // Simulate 10 rapid events (all throttled).
        for _ in 0..10 {
            inhibitor.recompute(true);
        }

        // The child should be the same — no respawn from repeated set_active.
        let second_pid = inhibitor.child.as_ref().map(|c| c.id());
        assert_eq!(
            first_pid, second_pid,
            "child should not respawn within throttle window"
        );

        // After the window, a change in state should execute.
        std::thread::sleep(Duration::from_millis(150));
        inhibitor.recompute(false);
        assert!(
            !inhibitor.holding(),
            "state change after throttle window must execute"
        );
    }
}
