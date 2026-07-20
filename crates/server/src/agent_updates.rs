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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

/// One update pass at a time, process-wide — concurrent `brew`/`npm`
/// runs fight over locks. `ServerConfig` is a per-process singleton,
/// so a static is the honest scope.
static UPDATE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAII hold on [`UPDATE_IN_FLIGHT`]. Releasing on `Drop` (not at the
/// end of the happy path) means a panicking update pass can't leave
/// the flag stuck and lock updates out for the rest of the process.
struct UpdateGuard;

impl UpdateGuard {
    fn acquire() -> Option<Self> {
        if UPDATE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(UpdateGuard)
        }
    }
}

impl Drop for UpdateGuard {
    fn drop(&mut self) {
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

/// Run `argv` as a bounded subprocess and return its trimmed stdout.
/// Every failure mode — unspawnable binary, timeout, non-zero exit —
/// collapses to a human-readable message fit for a footer notice.
async fn run_argv(argv: &[String], timeout: Duration) -> Result<String, RunError> {
    let (program, args) = argv
        .split_first()
        .expect("update channel argv is non-empty");
    let joined = argv.join(" ");
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| RunError::Spawn(format!("`{joined}` failed to start: {e}")))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| RunError::Other(format!("`{joined}` timed out after {}s", timeout.as_secs())))?
        .map_err(|e| RunError::Other(format!("`{joined}` failed: {e}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = stderr.trim().lines().last().unwrap_or_default();
        let detail = if detail.is_empty() {
            stdout.trim().lines().last().unwrap_or_default()
        } else {
            detail
        };
        let code = output
            .status
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
    let installed = match run_argv(&channel.version_argv, VERSION_TIMEOUT).await {
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
        Some(argv) if installed.is_some() => match run_argv(argv, LATEST_TIMEOUT).await {
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
        },
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
    }
}

/// The enabled agents that advertise an update channel, in stable
/// (sorted) order: `(id, display_name, channel)`.
fn updatable_agents(config: &ServerConfig) -> Vec<(String, String, UpdateChannel)> {
    let enabled = match lazybox_config::Config::load() {
        Ok(cfg) => cfg.setup.agents,
        Err(e) => {
            tracing::warn!("agent updates: could not load config: {e}");
            return Vec::new();
        }
    };
    enabled
        .into_iter()
        .filter_map(|id| {
            let agent = config.agents.get(&id)?;
            let channel = agent.update_channel()?;
            Some((id, agent.display_name().to_string(), channel))
        })
        .collect()
}

/// Check every updatable agent and broadcast the readings. Returns the
/// statuses so the scheduled loop can act on them.
pub async fn handle_check(config: &ServerConfig, manual: bool) -> Vec<AgentCliUpdateStatus> {
    let agents = updatable_agents(config);
    let mut statuses = Vec::with_capacity(agents.len());
    for (id, name, channel) in &agents {
        statuses.push(check_channel(id, name, channel).await);
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
    let before = run_argv(&channel.version_argv, VERSION_TIMEOUT)
        .await
        .ok()
        .and_then(|out| extract_version(&out));
    let result = run_argv(&channel.update_argv, UPDATE_TIMEOUT).await;
    let after = run_argv(&channel.version_argv, VERSION_TIMEOUT)
        .await
        .ok()
        .and_then(|out| extract_version(&out));
    let (ok, message) = match result {
        Ok(_) => match (&before, &after) {
            (Some(b), Some(a)) if b != a => (true, format!("updated {b} → {a}")),
            (_, Some(a)) => (true, format!("already up to date ({a})")),
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
/// locks when run concurrently), holding the process-wide in-flight
/// guard. Returns `false` — without touching anything — when another
/// pass already holds it.
async fn update_agents(
    config: &ServerConfig,
    agents: Vec<(String, String, UpdateChannel)>,
) -> bool {
    let Some(_guard) = UpdateGuard::acquire() else {
        return false;
    };
    for (id, name, channel) in &agents {
        update_one(config, id, name, channel).await;
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
        if !update_agents(&config, agents).await {
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
    let auto = auto_update_ids(&statuses);
    if auto.is_empty() {
        return;
    }
    let agents = updatable_agents(config)
        .into_iter()
        .filter(|(id, _, _)| auto.iter().any(|a| a == id))
        .collect();
    if !update_agents(config, agents).await {
        tracing::info!("agent updates: auto-update skipped — another update pass is running");
    }
}

/// Which of the checked agents should be auto-updated: an update is
/// available and the user opted the agent into `auto_update`.
fn auto_update_ids(statuses: &[AgentCliUpdateStatus]) -> Vec<String> {
    let cfg = match lazybox_config::Config::load() {
        Ok(cfg) => cfg,
        Err(_) => return Vec::new(),
    };
    statuses
        .iter()
        .filter(|s| s.update_available)
        .filter(|s| {
            cfg.agents
                .get(&s.agent_id)
                .is_some_and(|entry| entry.auto_update)
        })
        .map(|s| s.agent_id.clone())
        .collect()
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
        let out = run_argv(&sh("echo 1.2.3"), Duration::from_secs(5)).await;
        assert_eq!(out.unwrap(), "1.2.3");
    }

    #[tokio::test]
    async fn run_argv_reports_failure_detail() {
        let err = run_argv(&sh("echo boom >&2; exit 3"), Duration::from_secs(5))
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
        let err = run_argv(&sh("sleep 30"), Duration::from_millis(200))
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

    #[tokio::test]
    async fn concurrent_update_pass_is_rejected_without_side_effects() {
        let cfg = ServerConfig::with_store(std::sync::Arc::new(lazybox_store::MemoryStore::new()));
        let mut rx = cfg.bus.subscribe();
        let held = UpdateGuard::acquire().expect("guard free");
        let agents = vec![(
            "fake".to_string(),
            "Fake".to_string(),
            channel("echo 1.0.0", None, "true"),
        )];
        assert!(!update_agents(&cfg, agents.clone()).await);
        assert!(rx.try_recv().is_err(), "a rejected pass must emit nothing");
        drop(held);
        assert!(update_agents(&cfg, agents).await, "guard released on drop");
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
