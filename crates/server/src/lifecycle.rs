//! Server lifecycle: socket path resolution, PID file, signal
//! handling. All the bits that turn a `Server::serve()` loop into a
//! long-running service one can start, stop, and status-check.
//!
//! ## Layout on disk
//!
//! Everything under `$LAZYBOX_RUNTIME_DIR` (defaults to
//! `~/.lazybox/run/`):
//!
//! ```text
//! run/
//!   daemon.sock   Unix socket — where clients connect
//!   daemon.pid    PID of the running daemon (written on start)
//! ```
//!
//! Clients resolve the socket via the same paths, so `lazybox` and
//! `lazybox server *` agree without having to pass paths around.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Canonical daemon run-dir. Override via `LAZYBOX_RUNTIME_DIR` (legacy
/// — wins over `LAZYBOX_HOME` for back-compat) or via `LAZYBOX_HOME` (the
/// preferred multi-profile knob). Implementation lives in
/// [`lazybox_core::paths::runtime_dir`]; this thin re-export keeps the
/// existing `lifecycle::runtime_dir` import path working.
pub fn runtime_dir() -> PathBuf {
    lazybox_core::paths::runtime_dir()
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("daemon.pid")
}

/// Ensure the runtime dir exists. Called at daemon start + status.
/// Created 0700 (it holds the daemon socket — other local users must
/// not be able to traverse into it); any parent we create (typically
/// `~/.lazybox` itself) gets the same mode.
pub fn ensure_runtime_dir() -> std::io::Result<()> {
    let dir = runtime_dir();
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        // DirBuilder's mode is filtered through the umask; pin the
        // final dir to exactly 0700.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&dir)?;
    Ok(())
}

/// Write the current process's PID into `daemon.pid`. Overwrites any
/// existing file (stale PIDs are cleaned up in `read_pid` below).
pub fn write_pid_file(pid: u32, path: &Path) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "{pid}")?;
    Ok(())
}

/// Read `daemon.pid`. Returns:
/// - `Ok(Some(pid))` — file present, parsed, and the process is alive.
/// - `Ok(None)` — file missing, empty, unparseable, or refers to a
///   dead pid (stale file gets deleted as a side-effect).
pub fn read_pid(path: &Path) -> std::io::Result<Option<u32>> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let Ok(pid) = raw.trim().parse::<u32>() else {
        let _ = std::fs::remove_file(path);
        return Ok(None);
    };
    if is_alive(pid) {
        Ok(Some(pid))
    } else {
        let _ = std::fs::remove_file(path);
        Ok(None)
    }
}

/// True if `pid` refers to a live process. `kill(pid, 0)` is the
/// standard Unix liveness probe — doesn't actually signal, just
/// succeeds iff the process exists AND we're allowed to signal it.
fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Remove a socket file that's left over from a prior run (daemon
/// killed via SIGKILL, system crash, etc.). Returns true if a file
/// was removed. Idempotent: missing file → Ok(false), no error.
pub fn cleanup_stale_socket(path: &Path) -> std::io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

/// Status of the daemon. Distinct from `None` vs `Some(pid)` because
/// callers want to render "running (pid 1234)" vs "stopped" vs
/// "stale pidfile cleaned up."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Running { pid: u32 },
    Stopped,
}

pub fn status() -> ServerStatus {
    match read_pid(&pid_path()).unwrap_or(None) {
        Some(pid) => ServerStatus::Running { pid },
        None => ServerStatus::Stopped,
    }
}

/// Send SIGTERM to the running daemon, if any. Returns true if a
/// signal was sent (caller may want to wait for the socket file to
/// disappear as a shutdown confirmation).
pub fn request_stop() -> std::io::Result<bool> {
    let Some(pid) = read_pid(&pid_path())? else {
        return Ok(false);
    };
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, libc::SIGTERM) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(true)
}

/// Consume one lifecycle-hook payload from stdin and forward it to the
/// daemon socket. Hook delivery is deliberately best-effort: agent CLIs
/// treat a non-zero hook exit as a user-visible failure, while a stopped or
/// restarting lazybox daemon should only mean that one state signal is lost.
pub async fn ingest_hook_from_stdio(args: &[String]) {
    // Global deadline over the whole ingest (2026-08-19 audit, L4).
    // These helpers run once per agent lifecycle hook; two unbounded
    // waits used to leak processes: a payload writer that never closes
    // stdin left `read_to_string` parked forever, and after an unclean
    // daemon death (stale socket file) each helper connected into a
    // dead backlog and hung its full handshake timeout. Best-effort
    // delivery means a bounded miss beats a leaked process.
    const INGEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);
    let _ = tokio::time::timeout(INGEST_DEADLINE, ingest_hook_inner(args)).await;
}

async fn ingest_hook_inner(args: &[String]) {
    let (backend_key, terminal_id) = parse_hook_correlation(args);
    if backend_key.is_none() && terminal_id.is_none() {
        let _ = read_stdin_bounded().await;
        return;
    }

    let Some(payload) = read_stdin_bounded().await else {
        return;
    };
    let Some(hook) = lazybox_agents::hook::parse_claude_hook(&payload) else {
        return;
    };
    // Teach the agent what lazybox lets it do, once per session, through the
    // one channel that is spawn-intrinsic and needs no per-repo file: the
    // `SessionStart` hook's stdout, which Claude adds to the model's context.
    // Printed before the daemon send so a stopped daemon still injects it, and
    // independent of it so the existing state path is untouched.
    if let Some(text) = session_context_to_emit(args, &hook) {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }
    let command = lazybox_ipc::Command::IngestHook {
        terminal_id: lazybox_ipc::TerminalId(terminal_id.unwrap_or_default()),
        hook,
        backend_key: backend_key.clone(),
    };
    // A `PreToolUse` payload the large-read intercept could act on takes the
    // synchronous path instead: the same state signal plus a decision request,
    // on one connection, with the agent's turn blocked on the answer (#1610).
    if let Some(request) = intercept_request(&payload) {
        let deadline = decision_deadline();
        if let Some(reason) =
            request_tool_use_decision(&socket_path(), command, backend_key, request, deadline).await
        {
            print!("{}", deny_output(&reason));
            let _ = std::io::stdout().flush();
        }
        return;
    }
    if let Err(error) = lazybox_ipc::socket::send_command(&socket_path(), &command).await {
        tracing::warn!("hook-ingest IPC send failed: {error}");
    }
}

/// How long the hook waits for a decision. The agent's turn is stopped for
/// this whole window on every candidate read, so it is a latency budget, not
/// a correctness one: past it the helper prints nothing and the read happens,
/// exactly as if lazybox were not installed. A stopped, restarting, or wedged
/// daemon therefore costs one bounded pause, never a stalled turn.
const DECISION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// Ceiling on the deadline override. The override exists because 200 ms is
/// generous for a local socket but not for a test box already running a fleet
/// of agents, where process spawn plus handshake can outrun it — and a deny
/// test that times out passes vacuously, since a timeout also prints nothing.
/// It is clamped because it is read from the environment of a process that
/// blocks an agent's turn: an unclamped value turns every full-file read into
/// a multi-second stall, which is worse than the feature being off.
const MAX_DECISION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn decision_deadline() -> std::time::Duration {
    std::env::var("LAZYBOX_HOOK_DECISION_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(std::time::Duration::from_millis)
        .map(|requested| requested.min(MAX_DECISION_TIMEOUT))
        .unwrap_or(DECISION_TIMEOUT)
}

#[cfg(test)]
mod decision_deadline_tests {
    use super::*;

    /// The override is clamped and validated: garbage, zero, and an absurd
    /// value all resolve to something an agent's turn can survive.
    #[test]
    fn the_deadline_override_is_clamped() {
        assert_eq!(
            clamp_override(Some("50")),
            std::time::Duration::from_millis(50)
        );
        assert_eq!(clamp_override(Some("999999999")), MAX_DECISION_TIMEOUT);
        assert_eq!(clamp_override(Some("0")), DECISION_TIMEOUT);
        assert_eq!(clamp_override(Some("nonsense")), DECISION_TIMEOUT);
        assert_eq!(clamp_override(None), DECISION_TIMEOUT);
    }

    /// The pure half of `decision_deadline`, so the test never mutates the
    /// process environment (which other tests in this binary read).
    fn clamp_override(raw: Option<&str>) -> std::time::Duration {
        raw.and_then(|raw| raw.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(std::time::Duration::from_millis)
            .map(|requested| requested.min(MAX_DECISION_TIMEOUT))
            .unwrap_or(DECISION_TIMEOUT)
    }
}

/// The intercept candidate in this payload, or `None` when the daemon must
/// not even be asked.
///
/// The structural gate lives in [`lazybox_agents::hook`]; the config gate is
/// here because it is what keeps the feature free when it is off. `hook-ingest`
/// runs on EVERY tool call, so a round-trip taken while the policy could
/// never fire would put a bounded-but-real pause on every full-file read of
/// every session — including, when the daemon is down, the full deadline.
fn intercept_request(payload: &str) -> Option<lazybox_ipc::ToolUseRequest> {
    let request = lazybox_agents::hook::read_intercept_candidate(payload)?;
    let config = lazybox_config::Config::load().ok()?;
    // The daemon re-checks this; sharing the predicate rather than restating
    // it is what stops the two copies drifting, and this copy has veto power
    // (a round-trip it declines is a decision the daemon never makes).
    crate::read_intercept::armed(&config.agent.context_hygiene).then_some(request)
}

/// Send the hook's state signal and its decision request on one connection,
/// then wait — bounded — for the daemon's ruling. `Some(reason)` is a deny.
///
/// Deliberately not a subscribing client: a `Subscribe` would make the daemon
/// build a full workspace + terminal snapshot before it could answer, which is
/// far more than this deadline allows. Unrelated bus traffic on the connection
/// is skipped over until the correlated reply arrives.
async fn request_tool_use_decision(
    socket: &Path,
    ingest: lazybox_ipc::Command,
    backend_key: Option<String>,
    request: lazybox_ipc::ToolUseRequest,
    deadline: std::time::Duration,
) -> Option<String> {
    let client_request_id = uuid::Uuid::new_v4().hyphenated().to_string();
    let decide = lazybox_ipc::Command::DecideToolUse {
        backend_key,
        request,
        client_request_id: client_request_id.clone(),
    };
    let exchange = async {
        let (mut rd, mut wr) = lazybox_ipc::transport::connect(socket).await.ok()?;
        lazybox_ipc::socket::client_handshake(&mut rd, &mut wr)
            .await
            .ok()?;
        lazybox_ipc::socket::write_frame(&mut wr, &ingest)
            .await
            .ok()?;
        lazybox_ipc::socket::write_frame(&mut wr, &decide)
            .await
            .ok()?;
        loop {
            match lazybox_ipc::socket::read_frame::<_, lazybox_ipc::Event>(&mut rd).await {
                Ok(Some(lazybox_ipc::Event::ToolUseDecided {
                    client_request_id: id,
                    decision,
                })) if id == client_request_id => {
                    return match decision {
                        lazybox_ipc::ToolUseDecision::Deny { reason } => Some(reason),
                        lazybox_ipc::ToolUseDecision::Allow => None,
                    };
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return None,
            }
        }
    };
    match tokio::time::timeout(deadline, exchange).await {
        Ok(decision) => decision,
        Err(_) => {
            tracing::debug!("hook-ingest: tool-use decision timed out, allowing the tool through");
            None
        }
    }
}

/// The stdout JSON Claude reads a `PreToolUse` verdict from. Anything else on
/// stdout — including nothing at all — lets the tool run.
fn deny_output(reason: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
    )
}

/// Read the hook payload with its own timeout, off the async runtime
/// (stdin has no async story). `None` = the writer never delivered a
/// complete payload in time.
async fn read_stdin_bounded() -> Option<String> {
    const STDIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let read = tokio::task::spawn_blocking(read_stdin_to_string);
    match tokio::time::timeout(STDIN_TIMEOUT, read).await {
        Ok(Ok(payload)) => Some(payload),
        Ok(Err(_)) | Err(_) => {
            tracing::warn!("hook-ingest: stdin payload not delivered in time");
            None
        }
    }
}

/// Parse the correlation flags accepted by hook helpers. Unknown flags are
/// ignored so settings generated by a newer daemon remain harmless when an
/// older helper happens to run them.
pub fn parse_hook_correlation(args: &[String]) -> (Option<String>, Option<u64>) {
    let value = |flag: &str| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    let backend_key = value("--backend-key")
        .or_else(|| {
            value("--backend-key-file")
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map(|key| key.trim().to_string())
        })
        .filter(|key| !key.is_empty());
    let terminal_id = value("--terminal").and_then(|value| value.parse().ok());
    (backend_key, terminal_id)
}

/// The lazybox session-context blurb to print for this hook, or `None` when
/// it should stay silent. Emitted only on `SessionStart`, and only when the
/// `--emit-session-context` marker is present — the marker Claude's hook
/// command carries and Codex's omits, so Codex's `SessionStart` stays a no-op
/// until an equivalent stdout-as-context channel is verified for it.
///
/// The cross-agent coordination paragraph is appended only when the spawn
/// also carried `--emit-mcp-context` — the marker the daemon adds solely for a
/// session actually wired to the MCP bus (`SpawnFlags::mcp_wired`). A ReadOnly
/// "Ask lazybox" launch is never provisioned, so it gets the base blurb but is
/// not told about tools it cannot call.
fn session_context_to_emit(args: &[String], hook: &lazybox_ipc::HookEvent) -> Option<String> {
    let marked = args.iter().any(|arg| arg == "--emit-session-context");
    if hook.kind != lazybox_ipc::HookEventKind::SessionStart || !marked {
        return None;
    }
    if args.iter().any(|arg| arg == "--emit-mcp-context") {
        Some(lazybox_agents::lazybox_session_context_with_mcp())
    } else {
        Some(lazybox_agents::lazybox_session_context().to_string())
    }
}

fn read_stdin_to_string() -> String {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    input
}

#[cfg(test)]
mod hook_tests {
    use super::*;

    #[test]
    fn hook_correlation_accepts_backend_key_and_legacy_terminal() {
        let args = vec![
            "--unknown".to_string(),
            "ignored".to_string(),
            "--backend-key".to_string(),
            "lzb-session-7".to_string(),
            "--terminal".to_string(),
            "42".to_string(),
        ];

        assert_eq!(
            parse_hook_correlation(&args),
            (Some("lzb-session-7".to_string()), Some(42))
        );
    }

    fn hook(name: &str) -> lazybox_ipc::HookEvent {
        lazybox_agents::hook::parse_claude_hook(&format!(r#"{{"hook_event_name":"{name}"}}"#))
            .expect("valid hook json")
    }

    #[test]
    fn session_context_emitted_only_on_marked_session_start() {
        let marked = vec![
            "--backend-key".to_string(),
            "lzb-1".to_string(),
            "--emit-session-context".to_string(),
        ];

        // Marked SessionStart, no MCP marker → the base capability blurb, and
        // NOT the bus paragraph (this is the ReadOnly/unprovisioned case).
        assert_eq!(
            session_context_to_emit(&marked, &hook("SessionStart")),
            Some(lazybox_agents::lazybox_session_context().to_string()),
        );
        // Every other event stays silent, even when marked.
        for other in ["Stop", "UserPromptSubmit", "PreToolUse", "Notification"] {
            assert_eq!(session_context_to_emit(&marked, &hook(other)), None);
        }
        // Codex omits the marker → even SessionStart stays a no-op.
        let unmarked = vec!["--backend-key".to_string(), "lzb-1".to_string()];
        assert_eq!(
            session_context_to_emit(&unmarked, &hook("SessionStart")),
            None
        );
    }

    #[test]
    fn mcp_paragraph_emitted_only_when_the_bus_marker_is_present() {
        let base = vec![
            "--backend-key".to_string(),
            "lzb-1".to_string(),
            "--emit-session-context".to_string(),
        ];
        let mut with_mcp = base.clone();
        with_mcp.push("--emit-mcp-context".to_string());

        // Wired spawn (both markers) → base + coordination paragraph.
        assert_eq!(
            session_context_to_emit(&with_mcp, &hook("SessionStart")),
            Some(lazybox_agents::lazybox_session_context_with_mcp()),
        );
        // Base-only emission never names a bus-only tool.
        let base_text = session_context_to_emit(&base, &hook("SessionStart")).expect("base");
        assert!(
            !base_text.contains("post_note") && !base_text.contains("blackboard"),
            "unwired briefing must not advertise the bus: {base_text}"
        );
    }

    #[test]
    fn hook_correlation_reads_backend_key_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("backend-key");
        std::fs::write(&path, "lzb-file-key\n").expect("write key");
        let args = vec!["--backend-key-file".to_string(), path.display().to_string()];

        assert_eq!(
            parse_hook_correlation(&args),
            (Some("lzb-file-key".to_string()), None)
        );
    }
}
