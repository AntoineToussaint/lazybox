//! Lazybox-managed agent-CLI updates (issue #400).
//!
//! The agents' own in-session self-updaters are deliberately
//! suppressed at spawn (Claude's auto-update banner fails mid-session;
//! Codex's on-launch updater drags in a Homebrew self-update — #355).
//! This module is the sanctioned replacement: it probes each enabled
//! agent's installed vs latest version and runs updates through the
//! [`lazybox_agents::UpdateChannel`] the agent advertises — always in
//! plain bounded subprocesses, never inside a live session PTY, and
//! never on the spawn path, so a hung `npm`/`brew` can only delay an
//! update, not an agent launch.

use crate::ServerConfig;
use lazybox_agents::UpdateChannel;
use lazybox_agents::update::{extract_version, is_newer};
use lazybox_ipc::{AgentCliUpdateStatus, Event};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Probing a CLI's own `--version` is local and fast.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// Latest-version lookups hit a package registry over the network.
const LATEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Updates download + install; brew may also self-update first (the
/// sanctioned place for that — see #355). Killed past the bound so a
/// wedged package manager can't pin the update task forever.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// First scheduled check waits out daemon startup (provider polling,
/// session restore) before shelling out.
const STARTUP_DELAY: Duration = Duration::from_secs(60);
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// One update pass at a time in this process — the cheap first line
/// of the [`UpdateGuard`].
static UPDATE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// A lock file older than this is a crash leftover, not a live pass.
/// Generously above the longest legitimate pass (a few agents ×
/// [`UPDATE_TIMEOUT`]) so a slow real update is never hijacked.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(45 * 60);

/// Where concurrent lazybox processes rendezvous on "an update pass is
/// running". Lives in the state root beside the store.
pub(crate) fn update_lock_path() -> PathBuf {
    lazybox_core::paths::state_root().join("agent-update.lock")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// RAII hold on one update pass. Two layers, both released on `Drop`
/// (so a panicking pass can't lock updates out for the rest of the
/// process): the in-process atomic, and a lock *file* — several
/// lazybox processes can be alive at once (an embedded TUI beside a
/// standalone daemon, two plain TUIs), and concurrent `brew`/`npm`
/// runs fight over package-manager state that has no cross-process
/// lock of its own.
struct UpdateGuard {
    lock_path: PathBuf,
}

impl UpdateGuard {
    fn acquire(lock_path: &Path) -> Option<Self> {
        if UPDATE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
            return None;
        }
        if Self::claim_file(lock_path) {
            Some(UpdateGuard {
                lock_path: lock_path.to_path_buf(),
            })
        } else {
            UPDATE_IN_FLIGHT.store(false, Ordering::SeqCst);
            None
        }
    }

    /// Atomically create the lock file (its content is the claim's
    /// unix timestamp, which is what staleness is judged from — mtime
    /// can't be set portably from tests). A fresh existing file means
    /// another process is mid-pass; a stale one is removed and
    /// reclaimed. The remove→recreate window can race another
    /// recovering process into a double pass — a crash-only exposure,
    /// accepted. Unexpected IO errors fail open with a warning: a
    /// broken state dir shouldn't disable updates entirely, and the
    /// in-process layer still holds.
    fn claim_file(path: &Path) -> bool {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    let _ = write!(f, "{}", unix_now());
                    return true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && Self::lock_is_stale(path) {
                        tracing::warn!(
                            "agent updates: removing stale lock {} (crashed pass)",
                            path.display()
                        );
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    return false;
                }
                Err(e) => {
                    tracing::warn!(
                        "agent updates: cannot use lock file {}: {e} — proceeding without \
                         cross-process protection",
                        path.display()
                    );
                    return true;
                }
            }
        }
        false
    }

    /// A parseable claim timestamp older than [`LOCK_STALE_AFTER`] —
    /// or unparseable garbage, which no live pass wrote.
    fn lock_is_stale(path: &Path) -> bool {
        match std::fs::read_to_string(path) {
            Ok(content) => match content.trim().parse::<u64>() {
                Ok(claimed) => unix_now().saturating_sub(claimed) > LOCK_STALE_AFTER.as_secs(),
                Err(_) => true,
            },
            Err(_) => false,
        }
    }
}

impl Drop for UpdateGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
        UPDATE_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

/// Why a subprocess produced no usable output. `Spawn` is separated
/// out because callers treat it differently: a missing agent CLI is
/// the actionable finding itself, while a missing *registry* tool
/// (npm on a native-installer machine) just means "no latest lookup
/// here" and must not raise an error banner.
#[derive(Debug)]
enum RunError {
    Spawn(String),
    Other(String),
}

impl RunError {
    fn into_message(self) -> String {
        match self {
            Self::Spawn(m) | Self::Other(m) => m,
        }
    }
}

/// How a subprocess relates to the daemon's lifetime.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunMode {
    /// Read-only version probe: cheap to redo, so it dies with the
    /// daemon (`kill_on_drop`).
    Probe,
    /// Mutating package-manager run. Placed in its own process group
    /// and NOT killed on drop: quitting lazybox mid-download must not
    /// SIGKILL `npm`/`brew` and leave a half-replaced global install —
    /// the updater finishes on its own. A hung one is still killed
    /// explicitly when the timeout fires.
    Update,
}

/// Run `argv` as a bounded subprocess and return its trimmed stdout.
/// Every failure mode — unspawnable binary, timeout, non-zero exit —
/// collapses to a human-readable message fit for a footer notice.
async fn run_argv(argv: &[String], timeout: Duration, mode: RunMode) -> Result<String, RunError> {
    use tokio::io::AsyncReadExt;
    let (program, args) = argv
        .split_first()
        .expect("update channel argv is non-empty");
    let joined = argv.join(" ");
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match mode {
        RunMode::Probe => {
            cmd.kill_on_drop(true);
        }
        RunMode::Update => {
            cmd.process_group(0);
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| RunError::Spawn(format!("`{joined}` failed to start: {e}")))?;
    let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    // Drain both pipes alongside the wait so a chatty child can't
    // deadlock on a full pipe buffer.
    let run = async {
        let _ = tokio::join!(
            stdout_pipe.read_to_end(&mut stdout),
            stderr_pipe.read_to_end(&mut stderr),
        );
        child.wait().await
    };
    let status = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(RunError::Other(format!("`{joined}` failed: {e}"))),
        Err(_) => {
            // Update mode opted out of kill_on_drop, so the bounded-
            // runtime promise needs this explicit kill; for probes
            // it's a no-op double-tap.
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(RunError::Other(format!(
                "`{joined}` timed out after {}s",
                timeout.as_secs()
            )));
        }
    };
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&stderr);
        let stdout = String::from_utf8_lossy(&stdout);
        let detail = stderr.trim().lines().last().unwrap_or_default();
        let detail = if detail.is_empty() {
            stdout.trim().lines().last().unwrap_or_default()
        } else {
            detail
        };
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        if detail.is_empty() {
            Err(RunError::Other(format!("`{joined}` exited {code}")))
        } else {
            Err(RunError::Other(format!(
                "`{joined}` exited {code}: {detail}"
            )))
        }
    }
}

/// Probe one channel: installed version, latest version (when the
/// channel has a registry), and whether an update is available.
async fn check_channel(
    agent_id: &str,
    display_name: &str,
    channel: &UpdateChannel,
) -> AgentCliUpdateStatus {
    let mut error = None;
    let installed = match run_argv(&channel.version_argv, VERSION_TIMEOUT, RunMode::Probe).await {
        Ok(out) => {
            let v = extract_version(&out);
            if v.is_none() {
                error = Some(format!(
                    "no version in `{}` output",
                    channel.version_argv[0]
                ));
            }
            v
        }
        Err(e) => {
            error = Some(e.into_message());
            None
        }
    };
    let latest = match &channel.latest_argv {
        // The installed probe failing already tells the story; a
        // registry answer without a local version to compare against
        // would only decorate the error.
        Some(argv) if installed.is_some() => {
            match run_argv(argv, LATEST_TIMEOUT, RunMode::Probe).await {
                Ok(out) => extract_version(&out),
                // A machine without the registry tool at all (native
                // Claude install, no npm on PATH) simply has no latest
                // lookup — same installed-only reporting as a channel
                // with no registry, not an error banner. A present tool
                // that fails (non-zero exit, timeout) stays actionable.
                Err(RunError::Spawn(e)) => {
                    tracing::info!("agent updates: {agent_id}: no registry lookup — {e}");
                    None
                }
                Err(e) => {
                    error.get_or_insert(e.into_message());
                    None
                }
            }
        }
        _ => None,
    };
    let update_available = match (&installed, &latest) {
        (Some(i), Some(l)) => is_newer(l, i),
        _ => false,
    };
    AgentCliUpdateStatus {
        agent_id: agent_id.to_string(),
        display_name: display_name.to_string(),
        installed,
        latest,
        update_available,
        error,
        auto_update: false,
    }
}

/// One enabled agent lazybox can manage updates for.
struct UpdatableAgent {
    id: String,
    display_name: String,
    auto_update: bool,
    channel: UpdateChannel,
}

/// The enabled agents that advertise an update channel, in stable
/// (sorted) order.
fn updatable_agents(config: &ServerConfig) -> Vec<UpdatableAgent> {
    let cfg = match lazybox_config::Config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("agent updates: could not load config: {e}");
            return Vec::new();
        }
    };
    cfg.setup
        .agents
        .iter()
        .filter_map(|id| {
            let agent = config.agents.get(id)?;
            let channel = agent.update_channel()?;
            Some(UpdatableAgent {
                id: id.clone(),
                display_name: agent.display_name().to_string(),
                auto_update: cfg.agents.get(id).is_some_and(|entry| entry.auto_update),
                channel,
            })
        })
        .collect()
}

/// Check every updatable agent and broadcast the readings. Returns the
/// statuses so the scheduled loop can act on them.
pub async fn handle_check(config: &ServerConfig, manual: bool) -> Vec<AgentCliUpdateStatus> {
    let agents = updatable_agents(config);
    let mut statuses = Vec::with_capacity(agents.len());
    for a in &agents {
        let mut status = check_channel(&a.id, &a.display_name, &a.channel).await;
        status.auto_update = a.auto_update;
        statuses.push(status);
    }
    for s in &statuses {
        tracing::info!(
            "agent updates: {} installed={:?} latest={:?} available={} error={:?}",
            s.agent_id,
            s.installed,
            s.latest,
            s.update_available,
            s.error,
        );
    }
    let _ = config.bus.send(Event::AgentCliUpdatesChecked {
        statuses: statuses.clone(),
        manual,
    });
    statuses
}

/// Update `agent_id` through `channel` and broadcast the outcome.
/// Probes the version before and after so the notice can say
/// `2.1.3 → 2.1.4` (or "already up to date") instead of parroting the
/// package manager.
async fn update_one(
    config: &ServerConfig,
    agent_id: &str,
    display_name: &str,
    channel: &UpdateChannel,
) {
    let before = run_argv(&channel.version_argv, VERSION_TIMEOUT, RunMode::Probe)
        .await
        .ok()
        .and_then(|out| extract_version(&out));
    let result = run_argv(&channel.update_argv, UPDATE_TIMEOUT, RunMode::Update).await;
    let after = run_argv(&channel.version_argv, VERSION_TIMEOUT, RunMode::Probe)
        .await
        .ok()
        .and_then(|out| extract_version(&out));
    let (ok, message) = match result {
        Ok(_) => match (&before, &after) {
            (Some(b), Some(a)) if b != a => (true, format!("updated {b} → {a}")),
            // "Already up to date" needs a before-reading to back it —
            // with none (CLI wasn't installed, or the probe failed)
            // this could be a fresh install, so just state where the
            // CLI is now.
            (Some(_), Some(a)) => (true, format!("already up to date ({a})")),
            (None, Some(a)) => (true, format!("now at {a}")),
            _ => (true, "updated".to_string()),
        },
        Err(e) => (false, e.into_message()),
    };
    if ok {
        tracing::info!("agent updates: {agent_id}: {message}");
    } else {
        tracing::warn!("agent updates: {agent_id}: {message}");
    }
    let _ = config.bus.send(Event::AgentCliUpdateFinished {
        agent_id: agent_id.to_string(),
        display_name: display_name.to_string(),
        ok,
        installed_before: before,
        installed_after: after,
        message,
    });
}

/// Update the named agents sequentially (package managers fight over
/// locks when run concurrently), holding the pass guard. Returns
/// `false` — without touching anything — when another pass (in this
/// process or another) already holds it.
async fn update_agents(
    config: &ServerConfig,
    agents: Vec<UpdatableAgent>,
    lock_path: &Path,
) -> bool {
    let Some(_guard) = UpdateGuard::acquire(lock_path) else {
        return false;
    };
    for a in &agents {
        update_one(config, &a.id, &a.display_name, &a.channel).await;
    }
    true
}

/// `Command::UpdateAgentClis` — update every enabled agent with a
/// channel, on a detached task so the (minutes-long, but bounded)
/// package-manager runs never occupy a connection's mutation slot.
pub fn handle_update_all(config: &ServerConfig) {
    let config = config.clone();
    tokio::spawn(async move {
        let agents = updatable_agents(&config);
        if agents.is_empty() {
            let _ = config.bus.send(Event::Notification {
                title: "agent updates".into(),
                body: "no enabled agent has a managed update channel".into(),
            });
            return;
        }
        if !update_agents(&config, agents, &update_lock_path()).await {
            let _ = config.bus.send(Event::Notification {
                title: "agent updates".into(),
                body: "an agent-CLI update is already running".into(),
            });
        }
    });
}

/// Long-lived scheduled sweep: check shortly after startup and every
/// `CHECK_INTERVAL` (12h) after, broadcasting readings; agents opted into
/// `agents.<id>.auto_update` get available updates applied
/// automatically. Runs beside the polling loop, fully detached from
/// session spawning.
pub fn spawn_scheduled(config: ServerConfig) -> tokio::task::JoinHandle<()> {
    use futures::FutureExt;
    tokio::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        loop {
            // Panic-tolerant like the polling loop: tokio swallows
            // panics in spawned tasks, so an uncaught one would
            // silently end scheduled update checks until restart.
            if let Err(payload) = std::panic::AssertUnwindSafe(scheduled_pass(&config))
                .catch_unwind()
                .await
            {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string());
                tracing::error!("agent updates: scheduled sweep panicked: {msg}");
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    })
}

/// One scheduled iteration: check everything, then auto-update the
/// opted-in agents with updates available. Losing the in-flight guard
/// to a concurrent manual update just logs — the sweep retries next
/// interval, and an unprompted "already running" notice would only
/// confuse.
async fn scheduled_pass(config: &ServerConfig) {
    let statuses = handle_check(config, false).await;
    let auto: Vec<&str> = statuses
        .iter()
        .filter(|s| s.update_available && s.auto_update)
        .map(|s| s.agent_id.as_str())
        .collect();
    if auto.is_empty() {
        return;
    }
    let agents = updatable_agents(config)
        .into_iter()
        .filter(|a| auto.contains(&a.id.as_str()))
        .collect();
    if !update_agents(config, agents, &update_lock_path()).await {
        tracing::info!("agent updates: auto-update skipped — another update pass is running");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn channel(version: &str, latest: Option<&str>, update: &str) -> UpdateChannel {
        UpdateChannel {
            version_argv: sh(version),
            latest_argv: latest.map(sh),
            update_argv: sh(update),
        }
    }

    #[tokio::test]
    async fn run_argv_captures_stdout() {
        let out = run_argv(&sh("echo 1.2.3"), Duration::from_secs(5), RunMode::Probe).await;
        assert_eq!(out.unwrap(), "1.2.3");
    }

    #[tokio::test]
    async fn run_argv_reports_failure_detail() {
        let err = run_argv(
            &sh("echo boom >&2; exit 3"),
            Duration::from_secs(5),
            RunMode::Probe,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RunError::Other(_)));
        let msg = err.into_message();
        assert!(msg.contains("exited 3"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[tokio::test]
    async fn run_argv_reports_unspawnable_binary() {
        let err = run_argv(
            &["definitely-not-a-real-binary-xyz".to_string()],
            Duration::from_secs(5),
            RunMode::Probe,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RunError::Spawn(_)));
        let msg = err.into_message();
        assert!(msg.contains("failed to start"), "{msg}");
    }

    #[tokio::test]
    async fn run_argv_kills_on_timeout() {
        let started = std::time::Instant::now();
        let err = run_argv(&sh("sleep 30"), Duration::from_millis(200), RunMode::Update)
            .await
            .unwrap_err();
        let msg = err.into_message();
        assert!(msg.contains("timed out"), "{msg}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn check_channel_reports_update_available() {
        let c = channel("echo '1.0.0 (Fake CLI)'", Some("echo 1.0.1"), "true");
        let s = check_channel("fake", "Fake", &c).await;
        assert_eq!(s.installed.as_deref(), Some("1.0.0"));
        assert_eq!(s.latest.as_deref(), Some("1.0.1"));
        assert!(s.update_available);
        assert!(s.error.is_none());
    }

    #[tokio::test]
    async fn check_channel_current_version_is_not_available() {
        let c = channel("echo 2.0.0", Some("echo 2.0.0"), "true");
        let s = check_channel("fake", "Fake", &c).await;
        assert!(!s.update_available);
        assert!(s.error.is_none());
    }

    #[tokio::test]
    async fn check_channel_missing_cli_surfaces_error_without_registry_probe() {
        let c = UpdateChannel {
            version_argv: vec!["definitely-not-a-real-binary-xyz".into()],
            latest_argv: Some(sh("echo 9.9.9")),
            update_argv: sh("true"),
        };
        let s = check_channel("fake", "Fake", &c).await;
        assert!(s.installed.is_none());
        assert!(s.latest.is_none(), "registry probe should be skipped");
        assert!(!s.update_available);
        assert!(s.error.is_some());
    }

    #[tokio::test]
    async fn check_channel_missing_registry_tool_degrades_to_installed_only() {
        let c = UpdateChannel {
            version_argv: sh("echo 1.0.0"),
            latest_argv: Some(vec!["definitely-not-a-real-binary-xyz".into()]),
            update_argv: sh("true"),
        };
        let s = check_channel("fake", "Fake", &c).await;
        assert_eq!(s.installed.as_deref(), Some("1.0.0"));
        assert!(s.latest.is_none());
        assert!(
            s.error.is_none(),
            "a machine without the registry tool is not an error: {:?}",
            s.error
        );
    }

    #[tokio::test]
    async fn check_channel_failing_registry_probe_stays_actionable() {
        let c = channel("echo 1.0.0", Some("echo registry down >&2; exit 7"), "true");
        let s = check_channel("fake", "Fake", &c).await;
        assert_eq!(s.installed.as_deref(), Some("1.0.0"));
        assert!(s.latest.is_none());
        let err = s.error.expect("registry failure surfaces");
        assert!(err.contains("registry down"), "{err}");
    }

    fn fake_agents() -> Vec<UpdatableAgent> {
        vec![UpdatableAgent {
            id: "fake".to_string(),
            display_name: "Fake".to_string(),
            auto_update: false,
            channel: channel("echo 1.0.0", None, "true"),
        }]
    }

    /// One test covers every guard layer — they share the process-wide
    /// atomic, so splitting them would let parallel tests race it.
    #[tokio::test]
    async fn update_guard_serializes_in_process_and_across_processes() {
        let cfg = ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let mut rx = cfg.bus.subscribe();
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join("agent-update.lock");

        // In-process contention: a held guard rejects a second pass
        // without side effects.
        let held = UpdateGuard::acquire(&lock).expect("guard free");
        assert!(!update_agents(&cfg, fake_agents(), &lock).await);
        assert!(rx.try_recv().is_err(), "a rejected pass must emit nothing");
        drop(held);
        assert!(
            !lock.exists(),
            "dropping the guard must remove the lock file"
        );

        // Cross-process contention: a fresh lock file left by another
        // process rejects the pass even with the atomic free.
        std::fs::write(&lock, unix_now().to_string()).expect("write lock");
        assert!(UpdateGuard::acquire(&lock).is_none());

        // A stale claim (crashed process) is reclaimed...
        let stale = unix_now() - LOCK_STALE_AFTER.as_secs() - 60;
        std::fs::write(&lock, stale.to_string()).expect("write stale lock");
        // ...as is unparseable garbage no live pass wrote.
        let reclaimed = UpdateGuard::acquire(&lock).expect("stale lock reclaimed");
        drop(reclaimed);
        std::fs::write(&lock, "garbage").expect("write garbage lock");
        drop(UpdateGuard::acquire(&lock).expect("garbage lock reclaimed"));

        // With everything released the pass runs and reports.
        assert!(update_agents(&cfg, fake_agents(), &lock).await);
        assert!(matches!(
            rx.try_recv().expect("finished event"),
            Event::AgentCliUpdateFinished { .. }
        ));
    }

    #[tokio::test]
    async fn check_channel_without_registry_reports_installed_only() {
        let c = channel("echo 3.1.4", None, "true");
        let s = check_channel("fake", "Fake", &c).await;
        assert_eq!(s.installed.as_deref(), Some("3.1.4"));
        assert!(s.latest.is_none());
        assert!(!s.update_available);
        assert!(s.error.is_none());
    }

    #[tokio::test]
    async fn update_one_reports_version_transition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("updated");
        let version = format!(
            "if [ -f {m} ]; then echo 1.1.0; else echo 1.0.0; fi",
            m = marker.display()
        );
        let update = format!("touch {}", marker.display());
        let cfg = ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let mut rx = cfg.bus.subscribe();
        update_one(&cfg, "fake", "Fake", &channel(&version, None, &update)).await;
        let event = rx.try_recv().expect("finished event");
        match event {
            Event::AgentCliUpdateFinished {
                agent_id,
                ok,
                installed_before,
                installed_after,
                message,
                ..
            } => {
                assert_eq!(agent_id, "fake");
                assert!(ok);
                assert_eq!(installed_before.as_deref(), Some("1.0.0"));
                assert_eq!(installed_after.as_deref(), Some("1.1.0"));
                assert_eq!(message, "updated 1.0.0 → 1.1.0");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_one_reports_fresh_install_as_now_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("installed");
        // The CLI doesn't exist before the update installs it — the
        // outcome must not claim "already up to date".
        let version = format!(
            "if [ -f {m} ]; then echo 1.1.0; else exit 1; fi",
            m = marker.display()
        );
        let update = format!("touch {}", marker.display());
        let cfg = ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let mut rx = cfg.bus.subscribe();
        update_one(&cfg, "fake", "Fake", &channel(&version, None, &update)).await;
        match rx.try_recv().expect("finished event") {
            Event::AgentCliUpdateFinished {
                ok,
                installed_before,
                installed_after,
                message,
                ..
            } => {
                assert!(ok);
                assert!(installed_before.is_none());
                assert_eq!(installed_after.as_deref(), Some("1.1.0"));
                assert_eq!(message, "now at 1.1.0");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_one_surfaces_failure_actionably() {
        let cfg = ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let mut rx = cfg.bus.subscribe();
        update_one(
            &cfg,
            "fake",
            "Fake",
            &channel("echo 1.0.0", None, "echo no permission >&2; exit 1"),
        )
        .await;
        match rx.try_recv().expect("finished event") {
            Event::AgentCliUpdateFinished { ok, message, .. } => {
                assert!(!ok);
                assert!(message.contains("no permission"), "{message}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
