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
use std::sync::Arc;
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

/// How long `working` / `asking` keep holding after the last moment an agent
/// qualified. Releasing the instant no agent was mid-turn let the Mac
/// idle-sleep two minutes after the last `Done` — while agents sat on
/// background builds, deploys and questions, and between turns of the same
/// task — and every such sleep cost a restart. Half an hour spans those
/// gaps; a machine that is genuinely done still sleeps after it. The
/// periodic tick keeps running while the inhibitor is held, so the linger
/// expires on time without another agent event.
const LINGER: Duration = Duration::from_secs(30 * 60);

/// The handoff to leave behind if the process exits while holding: set when
/// an inhibitor with a handoff acquires, cleared when it releases. Process
/// level because a signal exit (`std::process::exit`) skips every
/// destructor, `Inhibitor::drop` included — the one exit a restart or an
/// install actually takes.
static EXIT_HANDOFF: parking_lot::Mutex<Option<Vec<String>>> = parking_lot::Mutex::new(None);

/// Leave the bounded inhibitor behind if one is pending. Called by the
/// signal-exit path just before `std::process::exit`; a normal shutdown
/// reaches the same handoff through `Inhibitor::drop`. Idempotent.
pub fn hand_off_before_exit() {
    if let Some(argv) = EXIT_HANDOFF.lock().take() {
        spawn_handoff(&argv);
    }
}

fn spawn_handoff(argv: &[String]) {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group, never waited on: it must outlive us.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    match cmd.spawn() {
        Ok(child) => tracing::info!(
            pid = child.id(),
            "keep-awake: daemon stopping while holding — handing off a bounded inhibitor"
        ),
        Err(e) => tracing::warn!("keep-awake: could not hand off the inhibitor: {e}"),
    }
}

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
    let handoff = handoff_argv(LINGER);
    let decided = config.keep_awake_active.clone();
    Some(tokio::spawn(async move {
        run_with(
            rx,
            bus,
            terminals,
            Inhibitor::new(argv).with_handoff(handoff),
            keep_awake_mode,
            on_battery,
            LINGER,
            decided,
        )
        .await;
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
#[cfg(test)]
async fn run(
    rx: broadcast::Receiver<Event>,
    bus: broadcast::Sender<Event>,
    terminals: TerminalRegistry,
    argv: Vec<String>,
    mode: impl Fn() -> KeepAwake,
    on_battery: impl Fn() -> bool + Clone + Send + 'static,
) -> Inhibitor {
    run_with(
        rx,
        bus,
        terminals,
        Inhibitor::new(argv),
        mode,
        on_battery,
        Duration::ZERO,
        Arc::default(),
    )
    .await
}

/// [`run`] with a linger: once an agent qualifies, the hold persists for
/// `linger` after it stops qualifying (see [`LINGER`]). Each decision is
/// recorded in `decided` for the subscribe path's badge prime.
#[allow(clippy::too_many_arguments)]
async fn run_with(
    mut rx: broadcast::Receiver<Event>,
    bus: broadcast::Sender<Event>,
    terminals: TerminalRegistry,
    mut inhibitor: Inhibitor,
    mode: impl Fn() -> KeepAwake,
    on_battery: impl Fn() -> bool + Clone + Send + 'static,
    linger: Duration,
    decided: Arc<parking_lot::Mutex<Option<bool>>>,
) -> Inhibitor {
    let mut last_qualified: Option<Instant> = None;
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
        &mut last_qualified,
        linger,
        &decided,
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
                &mut last_qualified,
                linger,
                &decided,
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
#[allow(clippy::too_many_arguments)]
async fn tick(
    inhibitor: &mut Inhibitor,
    last_status: &mut Option<(bool, bool)>,
    last_qualified: &mut Option<Instant>,
    linger: Duration,
    decided: &parking_lot::Mutex<Option<bool>>,
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
    let active = hold_with_linger(
        mode.should_hold(working, asking),
        mode,
        last_qualified,
        Instant::now(),
        linger,
    );
    inhibitor.recompute(active);
    *decided.lock() = Some(inhibitor.holding());
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

/// Whether to hold, given whether the mode qualifies right now: a
/// qualifying moment is remembered, and the hold lasts `linger` past the
/// last one. `off` never lingers — turning keep-awake off releases now.
fn hold_with_linger(
    qualifies: bool,
    mode: KeepAwake,
    last_qualified: &mut Option<Instant>,
    now: Instant,
    linger: Duration,
) -> bool {
    if mode == KeepAwake::Off {
        *last_qualified = None;
        return false;
    }
    if qualifies {
        *last_qualified = Some(now);
        return true;
    }
    last_qualified.is_some_and(|at| now.duration_since(at) < linger)
}

/// A time-bounded inhibitor NOT tethered to the daemon, left behind when
/// the daemon stops while holding. Agents run in tmux and outlive the
/// lazybox process; the tethered inhibitor does not, so a lazybox restart
/// used to hand the machine straight to idle sleep with agents still at
/// work (02:16 stop → 02:24 sleep). Bounded by `secs`, so it can never
/// leak: a relaunched daemon takes over with its own tethered hold.
fn handoff_argv(grace: Duration) -> Option<Vec<String>> {
    let secs = grace.as_secs().to_string();
    #[cfg(target_os = "macos")]
    {
        Some(
            ["caffeinate", "-dims", "-t", &secs]
                .map(String::from)
                .to_vec(),
        )
    }
    #[cfg(target_os = "linux")]
    {
        Some(
            [
                "systemd-inhibit",
                "--what=idle:sleep",
                "--who=lazybox",
                "--why=lazybox agents running (daemon restarting)",
                "--mode=block",
                "sleep",
                &secs,
            ]
            .map(String::from)
            .to_vec(),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = secs;
        None
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
    /// Spawned, detached, when the inhibitor is dropped while holding —
    /// see [`handoff_argv`]. `None` in tests and on platforms without one.
    handoff: Option<Vec<String>>,
}

impl Inhibitor {
    fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            child: None,
            spawn_warned: false,
            last_recompute: None,
            throttle_interval: Duration::from_secs(10),
            handoff: None,
        }
    }

    fn with_handoff(mut self, handoff: Option<Vec<String>>) -> Self {
        self.handoff = handoff;
        self
    }

    /// Leave the bounded, untethered hold behind (the daemon is stopping).
    /// Spawns only while the process-level record is still pending, so a
    /// signal exit that already handed off never gets a second one.
    fn hand_off(&self) {
        if self.holding()
            && self.handoff.is_some()
            && let Some(argv) = EXIT_HANDOFF.lock().take()
        {
            spawn_handoff(&argv);
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
                if let Some(handoff) = &self.handoff {
                    *EXIT_HANDOFF.lock() = Some(handoff.clone());
                }
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
        if self.handoff.is_some() {
            EXIT_HANDOFF.lock().take();
        }
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
        self.hand_off();
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

    /// The hold outlasts the last qualifying moment by the linger, then
    /// ends — releasing the instant no agent was mid-turn let the Mac
    /// idle-sleep two minutes after the last `Done`.
    #[test]
    fn a_hold_lingers_past_the_last_working_agent_then_ends() {
        let linger = Duration::from_secs(30 * 60);
        let t0 = Instant::now();
        let mut last = None;
        assert!(hold_with_linger(
            true,
            KeepAwake::Working,
            &mut last,
            t0,
            linger
        ));
        let later = t0 + Duration::from_secs(10 * 60);
        assert!(
            hold_with_linger(false, KeepAwake::Working, &mut last, later, linger),
            "ten quiet minutes still hold"
        );
        let past = t0 + linger + Duration::from_secs(1);
        assert!(
            !hold_with_linger(false, KeepAwake::Working, &mut last, past, linger),
            "past the linger the machine may sleep"
        );
        // A new qualifying moment restarts it.
        assert!(hold_with_linger(
            true,
            KeepAwake::Asking,
            &mut last,
            past,
            linger
        ));
    }

    /// Turning keep-awake off releases at once, linger or not.
    #[test]
    fn off_never_lingers() {
        let linger = Duration::from_secs(30 * 60);
        let t0 = Instant::now();
        let mut last = None;
        assert!(hold_with_linger(
            true,
            KeepAwake::Working,
            &mut last,
            t0,
            linger
        ));
        assert!(!hold_with_linger(
            false,
            KeepAwake::Off,
            &mut last,
            t0,
            linger
        ));
        assert!(
            !hold_with_linger(false, KeepAwake::Working, &mut last, t0, linger),
            "switching off forgot the last qualifying moment"
        );
    }

    /// A daemon that stops while holding leaves a bounded inhibitor behind,
    /// so a lazybox restart does not hand the machine to idle sleep while
    /// agents keep working in tmux: on drop (a normal stop), and through
    /// `hand_off_before_exit` on a signal exit, which skips destructors —
    /// exactly once either way. One that was not holding leaves nothing.
    /// One test, because the exit record is process-level.
    #[test]
    fn a_held_inhibitor_hands_off_once_on_drop_or_signal_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = |name: &str| dir.path().join(name);
        let handoff = |name: &str| {
            Some(vec![
                "sh".into(),
                "-c".into(),
                format!("echo x >> '{}'", marker(name).display()),
            ])
        };
        let wait_for = |name: &str| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !marker(name).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        let mut held = Inhibitor::new(sleep_argv()).with_handoff(handoff("drop"));
        held.set_active(true);
        drop(held);
        wait_for("drop");
        assert!(
            marker("drop").exists(),
            "a held inhibitor hands off on drop"
        );

        let mut signalled = Inhibitor::new(sleep_argv()).with_handoff(handoff("signal"));
        signalled.set_active(true);
        hand_off_before_exit();
        drop(signalled);
        wait_for("signal");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            std::fs::read_to_string(marker("signal"))
                .unwrap()
                .lines()
                .count(),
            1,
            "the signal path hands off, and the later drop does not add a second"
        );

        let mut released = Inhibitor::new(sleep_argv()).with_handoff(handoff("released"));
        released.set_active(true);
        released.set_active(false);
        hand_off_before_exit();
        drop(released);
        let idle = Inhibitor::new(sleep_argv()).with_handoff(handoff("idle"));
        drop(idle);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !marker("released").exists(),
            "released: nothing to hand off"
        );
        assert!(!marker("idle").exists(), "never held: nothing to hand off");
    }

    /// The watcher records what it decided, so a connecting client's badge
    /// reads the daemon's truth, linger included.
    #[tokio::test]
    async fn the_watcher_records_its_decision_for_the_badge() {
        let (tx, rx) = broadcast::channel(8);
        drop(tx);
        let (bus, _bus_rx) = inspect_bus();
        let decided = Arc::new(parking_lot::Mutex::new(None));
        let _inhibitor = run_with(
            rx,
            bus,
            working_terminals().await,
            Inhibitor::new(sleep_argv()),
            || KeepAwake::Working,
            || false,
            LINGER,
            decided.clone(),
        )
        .await;
        assert_eq!(*decided.lock(), Some(true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_handoff_is_bounded_and_untethered() {
        let argv = handoff_argv(Duration::from_secs(1800)).expect("macOS has one");
        assert_eq!(argv, ["caffeinate", "-dims", "-t", "1800"]);
        assert!(!argv.contains(&"-w".to_string()), "must outlive the daemon");
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
