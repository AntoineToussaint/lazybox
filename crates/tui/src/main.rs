//! `lazybox` — TUI client. Single binary, multiple modes:
//!
//!   lazybox                         default: in-process daemon + TUI
//!   lazybox --fresh                 wipe ~/.lazybox/v2/state.db + force
//!                                  the setup screen (testing first-run)
//!   lazybox --test                  throwaway tempdir repo + one fake
//!                                  workspace, no setup, no polling —
//!                                  for trying side panel + terminal
//!                                  pane end-to-end without GitHub
//!   lazybox server start            standalone daemon (for remote access)
//!   lazybox server stop             stop a running standalone daemon
//!   lazybox server status           show daemon status
//!   lazybox server api              foreground JSON HTTP API gateway
//!   lazybox slack init              interactive Slack token setup wizard
//!   lazybox slack doctor            read-only validation of an existing setup
//!   lazybox slack prune             archive stale per-(session, agent) channels
//!   lazybox hook-ingest --backend-key K  forward a Claude lifecycle hook
//!                                  payload (stdin JSON) to the daemon;
//!                                  injected into Claude via --settings
//!
//! All arg parsing is intentionally stupid — see `take_flag`.

use lazybox_ipc::{channel, socket};
use lazybox_server::lifecycle::{self, ServerStatus};
use lazybox_server::polling;
use lazybox_server::socket_service::SocketService;
use lazybox_server::{Server, ServerConfig};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// Fallback poll interval when `~/.lazybox/config.yaml::providers.github.poll_interval`
/// is unreadable. Once we have multiple-provider configs, each
/// provider will carry its own interval; this constant is only the
/// safety net for "couldn't load any config at all".
///
/// Deliberately DIFFERENT from `GithubConfig::default().poll_interval`
/// so a quick look at the log line "polling every Ns" tells you
/// whether the user's YAML was loaded or we fell through to this.
const POLL_INTERVAL_FALLBACK: Duration = Duration::from_secs(90);

/// Read the poll interval from the user's config, falling back to
/// the safety-net constant when the config can't be loaded.
/// `GithubConfig::poll_interval` already exists in the schema
/// (default 60s) — using this helper instead of a hardcoded
/// `POLL_INTERVAL` means edits to `~/.lazybox/config.yaml` take
/// effect on the next daemon start instead of being silently
/// ignored.
fn resolve_poll_interval() -> Duration {
    lazybox_config::Config::load()
        .map(|c| c.providers.github.poll_interval)
        .unwrap_or(POLL_INTERVAL_FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the contract: the default `GithubConfig::poll_interval` is
    /// the value `resolve_poll_interval` returns when the user has no
    /// custom YAML. Previously the daemon hardcoded 60s and ignored
    /// the config-schema default; this test fails loudly if the bug
    /// regresses.
    #[test]
    fn default_github_poll_interval_is_what_the_schema_says() {
        let default = lazybox_config::GithubConfig::default().poll_interval;
        // Default schema value is 60s — picked to fit a 200-PR
        // inbox inside GitHub's 5000-points/hour PAT budget after
        // the GraphQL connection-size trim (see SEARCH_QUERY in
        // gh-provider). If this assert ever needs updating it
        // should be a deliberate schema bump tied to a query-cost
        // change, not a silent drift.
        assert_eq!(default, Duration::from_secs(60));
        assert_ne!(
            default, POLL_INTERVAL_FALLBACK,
            "fallback must NOT match the schema default, otherwise we \
             can't tell whether the config is being honored",
        );
    }
}

/// Fallback log path when the config can't be read. Matches the old
/// hardcoded constant so existing operators / docs that reference
/// `/tmp/lazybox.log` still find what they expect.
const LOG_PATH_FALLBACK: &str = "/tmp/lazybox.log";

/// Resolve the log path: prefer `~/.lazybox/config.yaml::ui.log_path`,
/// fall back to `LOG_PATH_FALLBACK` when the config can't be loaded.
fn resolve_log_path() -> std::path::PathBuf {
    lazybox_config::Config::load()
        .map(|c| c.ui.resolved().log_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(LOG_PATH_FALLBACK))
}

/// Open a log file for append, locked down to owner-only. The log
/// lives in world-writable `/tmp` by default and captures daemon
/// traces, so `mode` covers the create path and `set_permissions`
/// covers a pre-existing looser file. A chmod failure is logged
/// (eprintln — the subscriber isn't up yet) but never blocks launch.
fn open_log_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!("warning: couldn't tighten {} to 0600: {e}", path.display());
        }
    }
    Ok(file)
}

/// Sibling perf-log path for `main`: `/tmp/lazybox.log` →
/// `/tmp/lazybox-perf.log`. Keeps the dedicated perf stream next to
/// the main log wherever the operator pointed it.
fn perf_log_path(main: &std::path::Path) -> std::path::PathBuf {
    let stem = main
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("lazybox");
    let ext = main.extension().and_then(|s| s.to_str()).unwrap_or("log");
    main.with_file_name(format!("{stem}-perf.{ext}"))
}

/// Initialize tracing to write to the configured log file instead of
/// stderr. With `LAZYBOX_PERF=1`, a second layer pipes the dedicated
/// perf target ([`lazybox_tui::perf::TARGET`]) to a sibling
/// `*-perf.log`, kept out of the main log by its own target filter.
fn init_tracing() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;

    let log_path = resolve_log_path();
    let file = open_log_file(&log_path)
        .map_err(|e| anyhow::anyhow!("open {}: {e}", log_path.display()))?;

    // Route the OS stderr into the same log file so native logs from
    // below the Rust layer (libghostty-vt Zig log, libgit2 stderr,
    // agent CLI noise) don't paint over the alternate-screen frame.
    lazybox_tui::platform::redirect_stderr_to_file(&file);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "lazybox=info,lazybox_gh=info,lazybox_server=info".into());

    let main_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_filter(env_filter);

    // Dedicated, greppable perf stream — only when LAZYBOX_PERF=1. Its
    // `Targets` filter admits exactly the perf target, and that target
    // sits outside the `lazybox` prefix so the main layer's env filter
    // never echoes perf samples into the main log.
    let perf_layer = lazybox_tui::perf::enabled()
        .then(|| perf_log_path(&log_path))
        .and_then(|path| match open_log_file(&path) {
            Ok(perf_file) => Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::sync::Mutex::new(perf_file))
                    .with_ansi(false)
                    .with_filter(
                        tracing_subscriber::filter::Targets::new()
                            .with_target(lazybox_tui::perf::TARGET, tracing::Level::TRACE),
                    ),
            ),
            Err(e) => {
                eprintln!("warning: couldn't open perf log {}: {e}", path.display());
                None
            }
        });

    tracing_subscriber::registry()
        .with(main_layer)
        .with(perf_layer)
        .init();

    Ok(())
}

/// Top-level orientation, printed on `lazybox -h` / `lazybox --help`. Ordered
/// getting-started-first, with the one destructive flag (`--fresh`) last.
/// Printed to stdout (not the log file) and exits before the daemon boots,
/// so `--help` stays fast and clean even when launched in a pipe.
const HELP: &str = "\
lazybox — a reactive PR inbox in your terminal

Events flow to you: new comments, CI failures, and review requests surface as
they land. Each task opens a git worktree with an embedded terminal for Claude
Code, Codex, Cursor, or a shell.

Usage: lazybox [OPTIONS] [COMMAND]

Run with no arguments to launch the inbox (an in-process daemon + TUI). The
first launch walks you through a short setup wizard; press `,` any time to add
repos, change agents, or edit roles.

Getting started:
  lazybox                     launch the inbox (default)
  lazybox --test              try the UI on a throwaway seeded workspace, no GitHub
  lazybox --help, -h          show this help
  lazybox --version, -V       print the version

Remote & services:
  lazybox server start        run a standalone daemon (for SSH / multi-client)
  lazybox server stop         stop a running standalone daemon
  lazybox server status       show daemon status
  lazybox server api [addr]   JSON HTTP API gateway (default 127.0.0.1:8787;
                              needs LAZYBOX_API_TOKEN or --insecure-no-auth;
                              non-loopback also needs --allow-insecure-http)
  lazybox --connect <socket>  attach a TUI to a running daemon
  lazybox slack init          set up the optional Slack mirror
  lazybox slack doctor        validate an existing Slack setup
  lazybox scan [ROOTS...]     list git repos/worktrees under ROOTS (or scan.roots;
                              --depth N to bound the walk, --hidden for dotdirs)

Advanced:
  lazybox --fresh             wipe ~/.lazybox/v2/state.db and re-run setup (destructive)

Credentials come from `gh auth token` by default; set LINEAR_API_KEY for Linear.
Logs go to /tmp/lazybox.log (RUST_LOG=lazybox=debug for verbose). State lives in
~/.lazybox/v2/state.db. Docs: https://lazybox.ai/docs/";

/// `-h` / `--help` anywhere in argv. Help is always available regardless of
/// the rest of the command line, per clig.dev discoverability.
fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "-h" || a == "--help")
}

/// `-V` / `--version` anywhere in argv.
fn wants_version(args: &[String]) -> bool {
    args.iter().any(|a| a == "-V" || a == "--version")
}

// The disallowed-methods allow covers the `Runtime::block_on` that
// `#[tokio::main]` expands to — the process entrypoint standing up
// the runtime, not run-loop work (see clippy.toml).
#[allow(clippy::disallowed_methods)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Resolve --help / --version before init_tracing (which redirects stderr
    // into the log file and opens the daemon path). These short-circuit to
    // clean stdout and exit 0, so they work in a pipe and don't touch state.
    if wants_help(&args) {
        println!("{HELP}");
        return Ok(());
    }
    if wants_version(&args) {
        println!("lazybox {}", lazybox_ipc::BUILD_VERSION);
        return Ok(());
    }

    init_tracing()?;

    let fresh = take_flag(&mut args, "--fresh");
    let test_mode = take_flag(&mut args, "--test");
    let preselect_workspace = take_value(&mut args, "--workspace");
    let preselect_session = take_value(&mut args, "--session");
    let preselect = preselect_workspace.map(|w| lazybox_tui::realm::model::Preselect {
        workspace_key: lazybox_core::SessionKey::from(w),
        session_id_raw: preselect_session,
    });
    if fresh {
        wipe_state_db();
    }
    if test_mode {
        return run_test(preselect).await;
    }
    match args.first().map(String::as_str) {
        Some("server") => server_subcommand(&args[1..]).await,
        Some("slack") => slack_subcommand(&args[1..]).await,
        Some("scan") => scan_subcommand(&args[1..]).await,
        Some("hook-ingest") => hook_ingest_subcommand(&args[1..]).await,
        Some("--connect") => {
            let socket_path = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(lifecycle::socket_path);
            run_remote(&socket_path, preselect).await
        }
        _ => run_embedded_realm(preselect).await,
    }
}

/// `lazybox hook-ingest --backend-key <key>` — the command Claude Code
/// runs on each lifecycle hook (lazybox injects it via `--settings` at
/// spawn). Reads the hook's JSON payload from stdin, normalizes it, and
/// forwards it to the running daemon over the IPC socket so the daemon
/// can map it to an `AgentState` transition. The backend key (the tmux
/// session name) is the correlation handle: it stays stable across
/// daemon restarts, unlike the legacy `--terminal <id>` (still parsed
/// and forwarded for the daemon to log-and-drop — pre-change settings
/// files baked it in, and after a restart it would name the wrong
/// terminal).
///
/// Designed to never disrupt Claude: a missing daemon, a bad payload, or
/// no correlation flag all resolve to a silent no-op (exit 0). A hook
/// command that errored or hung would stall Claude's turn.
async fn hook_ingest_subcommand(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let backend_key = take_value(&mut args, "--backend-key").filter(|k| !k.is_empty());
    let terminal_id = take_value(&mut args, "--terminal").and_then(|s| s.parse::<u64>().ok());
    if backend_key.is_none() && terminal_id.is_none() {
        // Nothing to correlate by. Drain stdin so Claude's write
        // doesn't block on a full pipe, then exit cleanly.
        let _ = read_stdin_to_string();
        return Ok(());
    }

    let payload = read_stdin_to_string();
    let Some(hook) = lazybox_agents::hook::parse_claude_hook(&payload) else {
        return Ok(());
    };

    let command = lazybox_ipc::Command::IngestHook {
        terminal_id: lazybox_ipc::TerminalId(terminal_id.unwrap_or_default()),
        hook,
        backend_key,
    };

    // Best-effort forward. Connect, handshake, write one framed
    // command, done — no reply expected. A connect/write error means no
    // daemon is listening (e.g. hooks fired against a session whose
    // daemon already exited); that's a no-op, not a failure. A
    // handshake error means a version-skewed daemon — logged (never
    // surfaced to Claude, whose turn would stall on a non-zero exit)
    // so the operator can see why agent state stopped updating.
    if let Ok((mut rd, mut wr)) = lazybox_ipc::transport::connect(&lifecycle::socket_path()).await {
        match socket::client_handshake(&mut rd, &mut wr).await {
            Ok(_) => {
                let _ = socket::write_frame(&mut wr, &command).await;
            }
            Err(e) => tracing::warn!("hook-ingest handshake failed: {e}"),
        }
    }
    Ok(())
}

/// Read all of stdin into a string (best-effort; an IO error yields what
/// was read so far, or an empty string).
fn read_stdin_to_string() -> String {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

/// `--key value` and `--key=value` parser. Removes both the flag and
/// its value from `args`.
fn take_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    if let Some(pos) = args.iter().position(|a| a == flag) {
        args.remove(pos);
        if pos < args.len() {
            return Some(args.remove(pos));
        }
        return None;
    }
    if let Some(pos) = args.iter().position(|a| a.starts_with(&prefix)) {
        let raw = args.remove(pos);
        return Some(raw[prefix.len()..].to_string());
    }
    None
}

/// `lazybox --test` boots against a throwaway tempdir repo + one
/// pre-seeded workspace. No setup screen, no provider polling, no
/// disk writes. The fixture (which owns the TempDir) is held in
/// scope for the whole TUI session — drop = `rm -rf` the tempdir.
async fn run_test(preselect: Option<lazybox_tui::realm::model::Preselect>) -> anyhow::Result<()> {
    let fixture = lazybox_tui::test_mode::TestFixture::new_with_seeded_session()?;
    eprintln!("--test repo at {}", fixture.repo.path().display());

    // Spawn under the test tempdir so any agent we launch defaults
    // there. Best-effort — lazybox still works if chdir fails.
    let _ = std::env::set_current_dir(fixture.repo.path());

    let (client, server) = channel::pair();
    let config = ServerConfig::with_store(fixture.store.clone());

    tokio::spawn(async move {
        if let Err(e) = Server::new(config).serve(server).await {
            tracing::error!("test-mode daemon exited: {e}");
        }
    });

    spawn_terminal_restore_on_signal();
    tokio::task::spawn_blocking(move || {
        let mut model = lazybox_tui::realm::Model::new(client)?;
        if let Some(p) = preselect {
            model = model.with_preselect(p);
        }
        lazybox_tui::realm::model::run_loop_with_model(model)
    })
    .await
    .map_err(|e| anyhow::anyhow!("realm task panicked: {e}"))?
    // `fixture` drops here → TempDir cleanup.
}

/// Restore the host terminal if the process is killed by a signal
/// (#211). SIGTERM / SIGHUP — and an externally-delivered SIGINT, since
/// raw mode swallows interactive Ctrl-C — terminate the process without
/// unwinding, so the `HostTerminalGuard` in `Model` never runs its
/// `Drop`. We catch those signals, run the same one-shot
/// `restore_host_terminal` the guard would, then exit — otherwise a
/// `kill`ed lazybox strands the shell in Kitty keyboard protocol + raw
/// mode. Spawned before the blocking run loop on every real-terminal
/// path.
fn spawn_terminal_restore_on_signal() {
    tokio::spawn(async {
        wait_for_exit_signal().await;
        lazybox_tui::realm::model::restore_host_terminal();
        // 128 + SIGTERM(15); a conventional signal-exit status.
        std::process::exit(143);
    });
}

#[cfg(unix)]
async fn wait_for_exit_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {},
        _ = hup.recv() => {},
        _ = int.recv() => {},
    }
}

#[cfg(windows)]
async fn wait_for_exit_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Remove a flag from `args` if present. Returns `true` if it was
/// found.
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    if let Some(pos) = args.iter().position(|a| a == flag) {
        args.remove(pos);
        true
    } else {
        false
    }
}

/// `--fresh`: clear `~/.lazybox/v2/state.db`. Wipes the entire DB,
/// which means the saved setup config in the kv table goes with it.
fn wipe_state_db() {
    let path = lazybox_server::state_db_path();
    match std::fs::remove_file(&path) {
        Ok(()) => eprintln!("removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("--fresh: couldn't remove {}: {e}", path.display()),
    }
}

/// `lazybox --connect <socket>` — connect to a standalone daemon over
/// a Unix socket and run the realm UI against it. The remote path
/// trusts the daemon's persisted setup (no first-run wizard, no
/// detection, no polling kickoff — all of that lives on the daemon
/// side).
/// `attention.notifier` (config crate) → the platform layer's backend.
/// A hand mapping because tui-core deliberately doesn't depend on
/// lazybox-config.
fn map_notifier_backend(
    b: lazybox_config::NotifierBackend,
) -> lazybox_tui::platform::NotifierBackend {
    match b {
        lazybox_config::NotifierBackend::Auto => lazybox_tui::platform::NotifierBackend::Auto,
        lazybox_config::NotifierBackend::Osc => lazybox_tui::platform::NotifierBackend::Osc,
        lazybox_config::NotifierBackend::Subprocess => {
            lazybox_tui::platform::NotifierBackend::Subprocess
        }
    }
}

async fn run_remote(
    socket_path: &std::path::Path,
    preselect: Option<lazybox_tui::realm::model::Preselect>,
) -> anyhow::Result<()> {
    if !socket_path.exists() {
        anyhow::bail!(
            "no daemon socket at {}. Start one with `lazybox server start`.",
            socket_path.display()
        );
    }
    let (client, daemon) = match socket::connect(socket_path).await {
        Ok(pair) => pair,
        Err(e) => {
            // println, not just the bail: stderr already points at the
            // log file, and a protocol-version mismatch needs to reach
            // the user's terminal to be actionable.
            println!("connect {}: {e}", socket_path.display());
            anyhow::bail!("connect {}: {e}", socket_path.display());
        }
    };

    spawn_terminal_restore_on_signal();
    // The remote path skips the full config application (the daemon
    // owns most of it), but notifications fire client-side — arm them
    // here too or `--connect` sessions would stay silent.
    let notifier = lazybox_config::Config::load()
        .map(|c| c.attention.notifier)
        .unwrap_or_default();
    lazybox_tui::platform::set_notifier_backend(map_notifier_backend(notifier));
    tokio::task::spawn_blocking(move || {
        let mut model = lazybox_tui::realm::Model::new(client)?;
        model.note_daemon_build(&daemon.build);
        model.check_build_freshness();
        if let Some(p) = preselect {
            model = model.with_preselect(p);
        }
        lazybox_tui::realm::model::run_loop_with_model(model)
    })
    .await
    .map_err(|e| anyhow::anyhow!("realm task panicked: {e}"))?
}

/// Realm-based default boot path. Spawns the daemon, runs detection
/// if no setup exists (kicks the wizard), kicks the polling loop on
/// completion, runs the realm UI on a blocking task.
async fn run_embedded_realm(
    preselect: Option<lazybox_tui::realm::model::Preselect>,
) -> anyhow::Result<()> {
    let (client, server) = channel::pair();
    let config = server_config_from_user()?;

    // Recovery probes the backend (`tmux list-sessions`) before the UI
    // paints — bound it so a wedged tmux server degrades to "no
    // recovered sessions" instead of a frozen launch.
    if tokio::time::timeout(
        Duration::from_secs(5),
        lazybox_server::spawn_handler::recover_sessions(&config),
    )
    .await
    .is_err()
    {
        tracing::warn!("recover_sessions timed out after 5s — continuing without recovery");
    }
    // Restore persisted sessions AFTER the UI is up: each restore can
    // touch git worktrees and the backend, and restored terminals
    // announce themselves via `TerminalSpawned` events — the same path
    // live spawns use — so the TUI picks them up whenever they land.
    {
        let config = config.clone();
        tokio::spawn(async move {
            lazybox_server::spawn_handler::restore_persisted_sessions(&config).await;
        });
    }
    lazybox_server::polling::migrate_legacy_sandbox(&config);

    let serve_config = config.clone();
    tokio::spawn(async move {
        let daemon = Server::new(serve_config);
        if let Err(e) = daemon.serve(server).await {
            tracing::error!("daemon exited: {e}");
        }
    });

    // Bind the IPC socket in embedded mode too, sharing the same
    // `ServerConfig` (store, bus, terminal maps) as the in-process
    // transport above. This is what lets `lazybox hook-ingest` — the
    // helper Claude's lifecycle hooks run — and `--connect` clients
    // reach an embedded instance; without it the hook helper connects
    // to nothing and every agent silently falls back to PTY scraping.
    // Skipped with a warning when a standalone daemon already owns the
    // socket (the same pid-file liveness check `lazybox server start`
    // uses), so two processes never fight over the bind. A stale
    // socket from a crashed run is reclaimed by `SocketService::run`
    // exactly as the server subcommand reclaims it.
    let embedded_socket = match lifecycle::status() {
        ServerStatus::Running { pid } => {
            tracing::warn!(
                pid,
                "standalone daemon already running — skipping embedded socket bind; \
                 hook-ingest payloads will reach that daemon, not this instance"
            );
            None
        }
        ServerStatus::Stopped => {
            let factory_config = config.clone();
            let service =
                SocketService::new(lifecycle::socket_path(), lifecycle::pid_path(), move || {
                    factory_config.clone()
                });
            let shutdown = service.shutdown_handle();
            let handle = tokio::spawn(async move {
                if let Err(e) = service.run().await {
                    tracing::warn!("embedded socket service: {e}");
                }
            });
            Some((shutdown, handle))
        }
    };

    // Two paths into the polling loop:
    //   1. Persisted setup exists → kick polling immediately.
    //   2. No persisted setup → run detection, hand the wizard to
    //      the realm `Model`, and wire the on-complete hook to fire
    //      polling once the user finishes.
    let persisted = persisted_setup(&*config.store);
    let returning_sources: Vec<String> = persisted
        .as_ref()
        .map(|p| p.enabled_providers.iter().cloned().collect())
        .unwrap_or_default();
    let persisted_for_model = persisted.clone();
    // Spawn the long-lived poll loop ONCE, UNCONDITIONALLY. It
    // re-reads YAML on every tick so filter / scope edits made via
    // the Settings palette take effect on the next cycle without a
    // respawn. Replaces the old per-Finish-respawn pattern that
    // leaked one tokio task per edit.
    //
    // Why unconditional (not `if persisted.is_some()`): on first run
    // there's no persisted setup yet, so the old gate skipped the
    // spawn entirely. The wizard's on-complete hook persists config
    // and fires `Command::Refresh` (→ `poll_wake.notify_one()`) to
    // kick an immediate tick — but that notify hit a loop that was
    // never spawned, so polling never started until the user
    // restarted lazybox (empty inbox after first-run setup). Spawning
    // here regardless is safe: with no config yet, `run_one_tick`
    // sees no providers and ticks as a cheap no-op until the wizard
    // writes `config.yaml` and the Refresh wakes the loop.
    polling::spawn(config.clone(), resolve_poll_interval());

    // Slack mirror — opt-in via `~/.lazybox/config.yaml::slack.{bot_token,
    // app_token}` (or `$SLACK_BOT_TOKEN` / `$SLACK_APP_TOKEN`). No-op
    // when neither token is set.
    if let Ok(yaml) = lazybox_config::Config::load() {
        let _ = lazybox_server::slack::spawn(config.clone(), yaml.slack);
    }

    // Always pre-run detection + scope sources. Two reasons: (1)
    // first-run users need them to seed the wizard; (2) returning
    // users may press `,` mid-session to reopen the wizard for
    // adding repos / agents — we cache the inputs on the model so
    // that path doesn't need to re-run async detection from inside
    // a `spawn_blocking` task. Both calls are read-only + cheap-ish
    // (sub-second on a warm cache).
    let setup_report = lazybox_tui::setup::detect_all().await;
    // Scope-source discovery does network IO (GitHub credential +
    // client build). Bounded so a stalled network can't hold the UI
    // hostage pre-paint; the wizard degrades to no scope suggestions.
    let setup_sources = std::sync::Arc::new(
        match tokio::time::timeout(Duration::from_secs(10), build_scope_sources()).await {
            Ok(sources) => sources,
            Err(_) => {
                tracing::warn!("build_scope_sources timed out after 10s — continuing without");
                Vec::new()
            }
        },
    );
    let needs_wizard = persisted_setup(&*config.store).is_none();
    let wizard_seed = if needs_wizard {
        Some((setup_report.clone(), setup_sources.clone()))
    } else {
        None
    };

    spawn_terminal_restore_on_signal();
    let store_for_save = config.store.clone();
    let realm_result = tokio::task::spawn_blocking(move || {
        let mut model = lazybox_tui::realm::Model::new(client)?;
        // Returning user with persisted setup → mount the polling
        // modal up front so the first poll cycle has UI feedback.
        if !returning_sources.is_empty() {
            model.show_polling(returning_sources);
        }
        // Hook: every time setup finishes (first-run wizard AND
        // partial flows like "Add a repo"), persist the new setup
        // to YAML. The long-lived poll loop (spawned ONCE above)
        // reads the YAML on every tick, so the next poll picks up
        // the change. Model also fires Command::Refresh on Finish
        // for an immediate tick + rescope. `Arc<dyn Fn>` because
        // partial flows can fire many times. The save result flows
        // back so the Finish handler can surface failures instead
        // of pretending the settings stuck.
        let store_for_save = std::sync::Arc::new(store_for_save);
        let hook: lazybox_tui::realm::SetupCompleteHook = std::sync::Arc::new(move |outcome| {
            let persisted = lazybox_tui::setup_flow::outcome_to_persisted(&outcome);
            lazybox_tui::setup_flow::save_persisted(&**store_for_save, &persisted)
        });
        model = model.with_setup_complete_hook(hook);
        if let Some(p) = preselect {
            model = model.with_preselect(p);
        }
        // Cache so the in-session `,` reopens the wizard without
        // re-running detection. (start_setup_wizard caches too —
        // this populates the cache for returning users.)
        model.cache_setup_inputs(setup_report, setup_sources);
        // Cache persisted state for partial Settings flows ("Add a
        // repo for github" needs to know github is already enabled
        // and what scopes are already picked).
        if let Some(p) = persisted_for_model {
            model.cache_persisted_setup(p);
        }
        // Detect installed editors for the `e` shortcut. User
        // overrides come from `~/.lazybox/config.yaml::editors`; the
        // builtins ship as defaults.
        let editors = lazybox_tui::editors::discover_at_startup(load_user_editors());
        tracing::info!("detected {} editor(s)", editors.len());
        model.cache_editors(editors);
        // Apply ~/.lazybox/config.yaml::{attention, ui, setup} → sidebar
        // + Model. Single load; subsequent reads happen on-demand via
        // Config::save_with for the writable parts.
        let user_config = lazybox_config::Config::load().unwrap_or_else(|e| {
            tracing::warn!("config.yaml load: {e}; using defaults");
            lazybox_config::Config::default()
        });
        // Apply the persisted theme before the first render so the UI
        // boots in the user's palette. An unknown name (theme renamed /
        // removed since they picked it) leaves the default active.
        if let Some(name) = user_config.ui.theme.as_deref()
            && !lazybox_tui::theme::set_by_name(name)
        {
            tracing::warn!("ui.theme = {name:?} not found; using the default theme");
        }
        // Arm desktop notifications with the configured backend. Until
        // this call `notify_user` is a logged no-op — arming is the
        // binary's opt-in so library tests never spawn real banners.
        lazybox_tui::platform::set_notifier_backend(map_notifier_backend(
            user_config.attention.notifier,
        ));
        let ui_defaults = user_config.resolved_ui();
        model.apply_sidebar_config(
            user_config.attention.clone(),
            user_config.ui.collapsed_repos.clone(),
            user_config.setup.default_agent.clone(),
            &user_config.display,
            &ui_defaults,
        );
        // Generate the per-agent SpawnAgent catalog rows (#102 P2):
        // exactly the agents the wizard enabled. An unconfigured user
        // (empty `setup.agents`) falls back to the built-in trio so the
        // zero-config `a c` / `a x` / `a u` chords still work out of
        // the box.
        // Using the enabled set (rather than unioning it with the
        // built-ins) avoids binding both `cursor` and `cursor-agent` to
        // `u`. Per-agent key remaps live in `ui.action_keys` under
        // `spawn_agent.<id>`.
        let agents: Vec<String> = if user_config.setup.agents.is_empty() {
            ["claude", "codex", "cursor"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            user_config.setup.agents.iter().cloned().collect()
        };
        model.set_agents(agents.clone());
        // Keymap = the selected in-tree preset (#102 P4) as a base
        // layer, with the user's explicit `ui.action_keys` on top so
        // individual tweaks win over the preset.
        let mut overrides = user_config
            .ui
            .keymap_preset
            .as_deref()
            .and_then(lazybox_tui_core::action::keymap_preset)
            .unwrap_or_default();
        // An unknown preset name silently fell back to the default
        // keymap; collect the warning now (the model surfaces it with
        // the rest of the keymap validation once the catalog is built).
        let keymap_warnings: Vec<String> = user_config
            .ui
            .keymap_preset
            .as_deref()
            .and_then(lazybox_tui_core::action::unknown_preset_warning)
            .into_iter()
            .collect();
        overrides.extend(user_config.ui.action_keys.clone());
        model.apply_action_key_overrides(overrides);
        // Per-agent model tiers (`agents.<id>.models`, with built-in
        // presets for known agents) — drives the `w S` / `a S` chords.
        let agent_models = agents
            .iter()
            .map(|id| (id.clone(), user_config.agent_models(id)))
            .collect();
        model.set_agent_models(agent_models);
        // Startup keymap validation (#4 of the keybinding audit):
        // unknown action_keys names, overrides that can't take effect,
        // and override-induced chord collisions surface as a footer
        // warning + messages-log details instead of failing silently.
        model.surface_keymap_warnings(keymap_warnings);
        // Arm the feature tour for anyone who hasn't seen it. It
        // launches on wizard Finish for first-run users, or at startup
        // (just below) for returning ones.
        model.set_auto_tour(!user_config.ui.tour_seen);
        // Seed progressive feature tips (#115): the opt-out flag plus
        // the ids already shown so they never repeat.
        model.set_tips(user_config.ui.show_tips, user_config.ui.tips_seen.clone());
        // Snippets — global (`<lazybox_home>/snippets.yaml`) merged
        // with the cwd's `.lazybox/snippets.yaml` (repo wins on key
        // conflict). Cwd is "wherever the user launched lazybox from",
        // which is the natural repo root for a single-repo workflow.
        let snippets =
            lazybox_config::Snippets::load_merged(std::env::current_dir().ok().as_deref());
        // Install the catalog, then restore the snippet-picker "Recent"
        // MRU from the state DB so frequently-used snippets stay one
        // keystroke away across restarts (#311). Runtime state, not YAML
        // — lives in the same store as read/unread and snooze. Bundled
        // so the restore can never run before the catalog is in place
        // (it prunes the MRU against it).
        model.apply_snippets_and_seed_recent(snippets, config.store.clone());
        model = model.with_splits(user_config.ui.sidebar_pct, user_config.ui.right_top_pct);
        if let Some((report, sources)) = wizard_seed {
            model.start_setup_wizard(report, sources);
        } else {
            // Returning user — setup already done, so there's no
            // wizard to finish behind. Surface the tour now if it
            // hasn't been seen (e.g. an upgrade into this feature).
            model.maybe_mount_tour();
        }
        // Warn if this build trails `main` — the uniformly-stale install
        // that no daemon/client mismatch catches (issue #234).
        model.check_build_freshness();
        lazybox_tui::realm::model::run_loop_with_model(model)
    })
    .await
    .map_err(|e| anyhow::anyhow!("realm task panicked: {e}"));
    // Tear the embedded socket service down the same way the server
    // subcommand does on SIGTERM: `SocketService::run` removes the
    // socket + pid file on its way out, so the next start doesn't
    // mistake this exited instance for still-running.
    if let Some((shutdown, handle)) = embedded_socket {
        shutdown.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
    realm_result?
}

/// Build the scope sources used by the setup wizard. GitHub today;
/// Linear ships without a scope-discovery API so the wizard skips it.
async fn build_scope_sources() -> Vec<Box<dyn lazybox_core::ScopeSource>> {
    let mut sources: Vec<Box<dyn lazybox_core::ScopeSource>> = Vec::new();
    if let Ok(cred) = lazybox_gh::credential_chain()
        .resolve(lazybox_gh::SOURCE)
        .await
        && let Ok(client) = lazybox_gh::GhClient::from_credential(cred).await
    {
        sources.push(Box::new(lazybox_gh::GhScopes::new(std::sync::Arc::new(
            client,
        ))));
    }
    sources
}

fn persisted_setup(store: &dyn lazybox_store::Store) -> Option<lazybox_core::PersistedSetup> {
    lazybox_tui::setup_flow::load_persisted(store)
}

/// Read the optional `editors:` list from `~/.lazybox/config.yaml`.
/// Errors / missing file → empty vec (the builtins still apply).
fn load_user_editors() -> Vec<lazybox_tui::editors::UserEditorEntry> {
    let cfg = match lazybox_config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config.yaml load failed: {e}");
            return Vec::new();
        }
    };
    cfg.editors
        .into_iter()
        .map(|e| lazybox_tui::editors::UserEditorEntry {
            id: e.id,
            display: e.display,
            command: e.command,
            args: e.args,
        })
        .collect()
}

/// `lazybox slack <init|doctor>` — Slack-side setup helpers. See
/// `lazybox_tui::slack_init` for the actual flow; this is just the
/// `lazybox scan [ROOTS...] [--depth N]` — read-only discovery of git
/// checkouts (normal clones and linked `git worktree`s) the user
/// created outside lazybox. Roots come from the command line, or from
/// `scan.roots` in the config when none are given. Prints a table;
/// nothing is imported or modified. This is stage one of issue #348 —
/// the scanner that a future import flow will build on.
///
/// Output goes through `println!` (stdout) because `init_tracing`
/// already redirected fd 2 into the log file.
async fn scan_subcommand(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    // A bad `--depth` is a mistake, not a reason to silently fall back
    // to the default — tell the user and stop.
    let depth_override = match take_value(&mut args, "--depth") {
        Some(s) => match s.parse::<usize>() {
            Ok(n) => Some(n),
            Err(_) => {
                println!("--depth expects a non-negative integer, got {s:?}.");
                std::process::exit(2);
            }
        },
        None => None,
    };
    let include_hidden = take_flag(&mut args, "--hidden");
    // Everything left that isn't a flag is a root to scan.
    let cli_roots: Vec<PathBuf> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .collect();

    let config = lazybox_config::Config::load().unwrap_or_default();
    let roots = if cli_roots.is_empty() {
        config.scan.roots.clone()
    } else {
        cli_roots
    };
    if roots.is_empty() {
        // stdout, not an `Err`: `init_tracing` redirected fd 2 into the
        // log file, so an anyhow error would never reach the terminal.
        println!(
            "No scan roots. Pass one or more directories (`lazybox scan ~/code`) \
             or set `scan.roots` in {}.",
            lazybox_config::Config::default_path().display()
        );
        std::process::exit(2);
    }
    let roots: Vec<PathBuf> = roots.iter().map(|p| scan_expand_tilde(p)).collect();
    // A root that doesn't exist yields nothing silently — call it out so
    // a typo doesn't read as "no repos here".
    for root in &roots {
        if !root.exists() {
            println!("warning: scan root does not exist: {}", root.display());
        }
    }
    let max_depth = depth_override.unwrap_or(config.scan.max_depth);
    // Skip anything under lazybox's own managed base — those aren't
    // "external" checkouts and the sidebar already tracks them.
    let exclude = lazybox_core::paths::state_root();

    let found =
        lazybox_git_ops::scan_external_checkouts(&roots, max_depth, include_hidden, &exclude).await;
    let tracked = tracked_worktree_paths(&lazybox_core::paths::state_db());
    print_scan_results(&roots, &found, &tracked);
    Ok(())
}

/// Canonical worktree paths of every session lazybox already tracks,
/// read best-effort from the state DB at `db_path`. Used to mark
/// discovered checkouts lazybox is already working in — today that's
/// only its own managed worktrees (already excluded from the scan),
/// but once import (issue #348 stage two) references external
/// checkouts in place, a re-scan would otherwise re-offer them as
/// fresh finds.
///
/// Empty on any failure — missing DB, a lock the busy-timeout couldn't
/// outwait, unreadable JSON — so the scan degrades to "unannotated"
/// rather than failing. Skipped entirely when the DB doesn't exist so
/// a scan never creates one (`SqliteStore::open` would).
fn tracked_worktree_paths(db_path: &std::path::Path) -> std::collections::HashSet<PathBuf> {
    use lazybox_store::Store;
    let mut out = std::collections::HashSet::new();
    if !db_path.exists() {
        return out;
    }
    let Ok(store) = lazybox_store::SqliteStore::open(db_path) else {
        return out;
    };
    let Ok(records) = store.list_workspaces() else {
        return out;
    };
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = serde_json::from_str::<lazybox_core::Workspace>(&json) else {
            continue;
        };
        for session in workspace.sessions {
            out.insert(scan_canonicalize(&session.worktree_path));
        }
    }
    out
}

/// Canonicalize when possible (resolves symlinks + macOS `/var` →
/// `/private/var`), falling back to the literal path so a
/// not-yet-created path still compares equal to itself.
fn scan_canonicalize(p: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Expand a leading `~/` against `$HOME`. Mirrors the daemon's mount
/// path expansion so `scan.roots: [~/code]` resolves the same way.
fn scan_expand_tilde(p: &std::path::Path) -> PathBuf {
    if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}

/// Render the scan as a plain aligned table on stdout, most-recently
/// active first. Read-only: purely informational. `tracked` holds the
/// canonical worktree paths lazybox already knows about, used to tag
/// checkouts it's already working in.
fn print_scan_results(
    roots: &[PathBuf],
    found: &[lazybox_git_ops::DiscoveredCheckout],
    tracked: &std::collections::HashSet<PathBuf>,
) {
    let root_list = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if found.is_empty() {
        println!("No git checkouts found under {root_list}.");
        return;
    }

    // Most-recent activity first; unknown-age entries sink to the bottom.
    let mut rows: Vec<&lazybox_git_ops::DiscoveredCheckout> = found.iter().collect();
    rows.sort_by_key(|c| std::cmp::Reverse(c.last_commit_unix));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!(
        "Found {} git checkout{} under {root_list}:\n",
        found.len(),
        if found.len() == 1 { "" } else { "s" },
    );
    for c in rows {
        let age = c
            .last_commit_unix
            .map(|t| humanize_unix_age(now, t))
            .unwrap_or_else(|| "—".to_string());
        let branch = c.branch.as_deref().unwrap_or("(detached)");
        let mut tags = Vec::new();
        if c.is_linked_worktree {
            tags.push("worktree");
        }
        if c.has_uncommitted_changes {
            tags.push("dirty");
        }
        if tracked.contains(&scan_canonicalize(&c.path)) {
            tags.push("tracked");
        }
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        println!(
            "  {:>12}  {:<24}  {}{}",
            age,
            truncate(branch, 24),
            c.path.display(),
            tag_str,
        );
    }
    println!("\nImport into lazybox is not wired up yet (issue #348, stage two).");
}

/// `secs`-ago-style relative age. Coarse buckets are enough for a
/// scan listing — the point is recency ordering, not precision.
fn humanize_unix_age(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    let (n, unit) = if secs < 60 {
        (secs, "s")
    } else if secs < 3600 {
        (secs / 60, "m")
    } else if secs < 86_400 {
        (secs / 3600, "h")
    } else if secs < 86_400 * 30 {
        (secs / 86_400, "d")
    } else if secs < 86_400 * 365 {
        (secs / (86_400 * 30), "mo")
    } else {
        (secs / (86_400 * 365), "y")
    };
    format!("{n}{unit} ago")
}

/// Clip `s` to `max` chars with an ellipsis, for column alignment.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// argv dispatch.
///
/// User-facing output goes through `println!` (stdout) because
/// `init_tracing` redirects fd 2 to the log file — anything written
/// to stderr from here would vanish into `/tmp/lazybox.log` instead of
/// reaching the user's terminal.
async fn slack_subcommand(args: &[String]) -> anyhow::Result<()> {
    use lazybox_tui::slack_init;
    match args.first().map(String::as_str) {
        Some("init") => {
            let outcome = slack_init::run_init().await?;
            match outcome {
                slack_init::InitOutcome::Ready => Ok(()),
                slack_init::InitOutcome::ReadyNeedsInvite { anchor_channel } => {
                    // Exit 0 — tokens are persisted and validated, the
                    // user just needs to make the channel reachable.
                    // Doctor will re-run the same check.
                    println!("(re-run `lazybox slack doctor` once #{anchor_channel} is created.)");
                    Ok(())
                }
                slack_init::InitOutcome::Failed => std::process::exit(1),
            }
        }
        Some("doctor") => {
            let outcome = slack_init::run_doctor().await?;
            match outcome {
                slack_init::DoctorOutcome::Healthy => Ok(()),
                slack_init::DoctorOutcome::HealthyNeedsInvite { anchor_channel } => {
                    println!("(create / unarchive #{anchor_channel} to finish setup.)");
                    Ok(())
                }
                slack_init::DoctorOutcome::Failed => std::process::exit(1),
            }
        }
        Some("prune") => {
            use lazybox_tui::slack_prune;
            let outcome = slack_prune::run(&args[1..]).await?;
            match outcome {
                slack_prune::PruneOutcome::Done { .. } => Ok(()),
                slack_prune::PruneOutcome::Failed => std::process::exit(1),
                slack_prune::PruneOutcome::BadArgs => std::process::exit(2),
            }
        }
        _ => {
            println!("usage: lazybox slack [init|doctor|prune]");
            std::process::exit(2);
        }
    }
}

async fn server_subcommand(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("start") => server_start().await,
        Some("stop") => server_stop(),
        Some("status") => server_status(),
        Some("api") => server_api(&args[1..]).await,
        _ => {
            eprintln!(
                "usage: lazybox server [start|stop|status|api [addr:port] [--insecure-no-auth] [--allow-insecure-http]]"
            );
            std::process::exit(2);
        }
    }
}

async fn server_start() -> anyhow::Result<()> {
    if let ServerStatus::Running { pid } = lifecycle::status() {
        anyhow::bail!("daemon already running (pid {pid})");
    }
    lifecycle::ensure_runtime_dir()?;
    let socket = lifecycle::socket_path();
    let pid_file = lifecycle::pid_path();

    let config = server_config_from_user()?;
    if tokio::time::timeout(
        Duration::from_secs(5),
        lazybox_server::spawn_handler::recover_sessions(&config),
    )
    .await
    .is_err()
    {
        tracing::warn!("recover_sessions timed out after 5s — continuing without recovery");
    }
    polling::migrate_legacy_sandbox(&config);
    polling::spawn(config.clone(), resolve_poll_interval());
    if let Ok(yaml) = lazybox_config::Config::load() {
        let _ = lazybox_server::slack::spawn(config.clone(), yaml.slack);
    }

    let factory_config = config.clone();
    let service = SocketService::new(socket.clone(), pid_file, move || factory_config.clone());
    let shutdown = service.shutdown_handle();

    tokio::spawn(async move {
        lazybox_tui::platform::wait_for_shutdown_signal().await;
        shutdown.notify_one();
    });

    println!("lazybox-server listening on {}", socket.display());
    service.run().await?;
    println!("lazybox-server stopped");
    Ok(())
}

fn server_stop() -> anyhow::Result<()> {
    if !lifecycle::request_stop()? {
        println!("no daemon running");
        return Ok(());
    }
    println!("sent SIGTERM to daemon");
    Ok(())
}

fn server_status() -> anyhow::Result<()> {
    match lifecycle::status() {
        ServerStatus::Running { pid } => {
            println!(
                "running (pid {pid}) at {}",
                lifecycle::socket_path().display()
            );
        }
        ServerStatus::Stopped => println!("stopped"),
    }
    Ok(())
}

fn api_bind_requires_insecure_http_ack(bind_addr: SocketAddr, allowed: bool) -> bool {
    !bind_addr.ip().is_loopback() && !allowed
}

async fn server_api(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let insecure_no_auth = take_flag(&mut args, "--insecure-no-auth");
    let allow_insecure_http = take_flag(&mut args, "--allow-insecure-http");
    let bind_addr = match args.first() {
        Some(raw) => raw
            .parse::<SocketAddr>()
            .map_err(|e| anyhow::anyhow!("invalid API bind address {raw:?}: {e}"))?,
        None => std::env::var("LAZYBOX_API_ADDR")
            .ok()
            .and_then(|raw| raw.parse::<SocketAddr>().ok())
            .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787)),
    };
    let token = std::env::var("LAZYBOX_API_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());

    // Auth is opt-out, not opt-in: the API drives agents that hold the
    // user's gh/git identity, so an unauthenticated gateway is only
    // served when explicitly demanded. println (not bail) — stderr is
    // redirected into the log file by init_tracing, so an anyhow error
    // would be invisible to the user.
    if token.is_none() && !insecure_no_auth {
        println!(
            "refusing to serve the JSON API without auth.\n\
             Set LAZYBOX_API_TOKEN (clients then send `Authorization: Bearer <token>`),\n\
             or pass --insecure-no-auth to explicitly serve unauthenticated."
        );
        std::process::exit(2);
    }

    // Bearer auth does not encrypt HTTP. Refuse a routable listener unless
    // the operator separately acknowledges that a trusted tunnel, TLS proxy,
    // or private overlay protects the network path.
    if api_bind_requires_insecure_http_ack(bind_addr, allow_insecure_http) {
        println!(
            "refusing to expose the plaintext JSON API on {bind_addr}.\n\
             Keep it on loopback and use an SSH tunnel or authenticated TLS proxy.\n\
             Pass --allow-insecure-http only when a trusted private network already protects the connection."
        );
        std::process::exit(2);
    }

    let config = server_config_from_user()?;
    if tokio::time::timeout(
        Duration::from_secs(5),
        lazybox_server::spawn_handler::recover_sessions(&config),
    )
    .await
    .is_err()
    {
        tracing::warn!("recover_sessions timed out after 5s — continuing without recovery");
    }
    println!("lazybox API listening on http://{bind_addr}");
    if token.is_some() {
        println!("lazybox API bearer auth enabled via LAZYBOX_API_TOKEN");
    } else {
        println!(
            "WARNING: lazybox API bearer auth disabled (--insecure-no-auth); \
             anything that can reach {bind_addr} can drive your agents"
        );
    }
    if !bind_addr.ip().is_loopback() {
        println!(
            "WARNING: serving plaintext HTTP on non-loopback {bind_addr}; bearer tokens and commands are not encrypted"
        );
    }

    lazybox_server::api_gateway::serve(
        config,
        lazybox_server::api_gateway::GatewayOptions {
            bind_addr,
            bearer_token: token,
            ..lazybox_server::api_gateway::GatewayOptions::default()
        },
    )
    .await?;
    Ok(())
}

/// Load the production database without ever degrading to ephemeral
/// state. Tracing redirects stderr to the log file, so print the failure
/// to stdout before returning it or a CLI user would see a silent exit.
fn server_config_from_user() -> anyhow::Result<ServerConfig> {
    match ServerConfig::from_user_config() {
        Ok(config) => Ok(config),
        Err(error) => {
            println!(
                "✗ lazybox cannot open its persistent state: {error}\n\
                 Refusing to continue with an empty in-memory database.\n\
                 Check the path and its permissions, then retry."
            );
            Err(error.into())
        }
    }
}

#[cfg(test)]
mod argv_tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn take_flag_finds_and_removes() {
        let mut a = args(&["--fresh", "--workspace", "foo"]);
        assert!(take_flag(&mut a, "--fresh"));
        assert_eq!(a, args(&["--workspace", "foo"]));
    }

    #[test]
    fn take_flag_returns_false_when_absent() {
        let mut a = args(&["--workspace", "foo"]);
        assert!(!take_flag(&mut a, "--fresh"));
        assert_eq!(a, args(&["--workspace", "foo"]));
    }

    #[test]
    fn help_and_version_flags_detected_anywhere() {
        assert!(wants_help(&args(&["--help"])));
        assert!(wants_help(&args(&["-h"])));
        assert!(wants_help(&args(&["server", "--help"])));
        assert!(!wants_help(&args(&["--fresh"])));

        assert!(wants_version(&args(&["--version"])));
        assert!(wants_version(&args(&["-V"])));
        assert!(!wants_version(&args(&["-v"]))); // lowercase -v is not the version flag
    }

    #[test]
    fn perf_log_path_is_a_sibling_of_the_main_log() {
        use std::path::Path;
        assert_eq!(
            perf_log_path(Path::new("/tmp/lazybox.log")),
            Path::new("/tmp/lazybox-perf.log")
        );
        // Honors a custom directory + extension from `ui.log_path`.
        assert_eq!(
            perf_log_path(Path::new("/var/log/lb.txt")),
            Path::new("/var/log/lb-perf.txt")
        );
    }

    #[test]
    fn help_text_is_getting_started_first_and_destructive_last() {
        // Orientation order is a contract: the safe getting-started path comes
        // before the one destructive flag, per clig.dev help ordering.
        let getting_started = HELP
            .find("Getting started:")
            .expect("getting-started section");
        let destructive = HELP.find("--fresh").expect("--fresh mention");
        assert!(getting_started < destructive);
        assert!(HELP.contains("(destructive)"));
    }

    #[test]
    fn humanize_unix_age_buckets_by_magnitude() {
        let day = 86_400u64;
        assert_eq!(humanize_unix_age(100, 100), "0s ago");
        assert_eq!(humanize_unix_age(100, 70), "30s ago");
        assert_eq!(humanize_unix_age(600, 60), "9m ago");
        assert_eq!(humanize_unix_age(2 * 3600, 3600), "1h ago");
        assert_eq!(humanize_unix_age(3 * day, day), "2d ago");
        assert_eq!(humanize_unix_age(90 * day, day), "2mo ago");
        assert_eq!(humanize_unix_age(800 * day, day), "2y ago");
        // A clock skew (then in the future) must not panic or underflow.
        assert_eq!(humanize_unix_age(10, 100), "0s ago");
    }

    #[test]
    fn truncate_clips_with_ellipsis() {
        assert_eq!(truncate("short", 24), "short");
        assert_eq!(truncate("exactly-ten", 11), "exactly-ten");
        assert_eq!(truncate("way-too-long-branch-name", 10), "way-too-l…");
    }

    #[test]
    fn scan_expand_tilde_only_touches_leading_tilde_slash() {
        let prev = std::env::var("HOME").ok();
        // SAFETY: single-threaded test body; HOME is restored below.
        unsafe { std::env::set_var("HOME", "/home/tester") };
        assert_eq!(
            scan_expand_tilde(std::path::Path::new("~/code")),
            std::path::PathBuf::from("/home/tester/code"),
        );
        assert_eq!(
            scan_expand_tilde(std::path::Path::new("/abs/path")),
            std::path::PathBuf::from("/abs/path"),
        );
        // A bare `~` (no slash) is left alone — not a home reference.
        assert_eq!(
            scan_expand_tilde(std::path::Path::new("~weird")),
            std::path::PathBuf::from("~weird"),
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn tracked_worktree_paths_reads_session_paths_and_never_creates_the_db() {
        use lazybox_store::{SqliteStore, Store, WorkspaceRecord};
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("state.db");

        // Missing DB → empty set, and the read must not create the file
        // (SqliteStore::open would, so the existence guard matters).
        assert!(tracked_worktree_paths(&db).is_empty());
        assert!(!db.exists(), "reading a missing DB must not create it");

        // Seed one workspace whose session points at a real directory.
        let wt = tmp.path().join("some-checkout");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        let key = lazybox_core::WorkspaceKey::new("ws-1");
        let mut ws = lazybox_core::Workspace::empty(key.clone(), "main", chrono::Utc::now());
        ws.add_session(lazybox_core::WorkspaceSession::new(
            key.clone(),
            lazybox_core::SessionKind::Shell,
            wt.clone(),
            chrono::Utc::now(),
        ));
        let json = serde_json::to_string(&ws).expect("serialize workspace");
        let store = SqliteStore::open(&db).expect("open store");
        store
            .save_workspace(&WorkspaceRecord {
                key: key.as_str().to_string(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(json),
            })
            .expect("save workspace");
        drop(store);

        let tracked = tracked_worktree_paths(&db);
        assert!(
            tracked.contains(&scan_canonicalize(&wt)),
            "the session's worktree path must be reported as tracked, got {tracked:?}"
        );
    }

    #[test]
    fn api_plaintext_bind_policy_requires_separate_non_loopback_ack() {
        let loopback: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        let wildcard: SocketAddr = "0.0.0.0:8787".parse().unwrap();
        let private: SocketAddr = "192.168.1.10:8787".parse().unwrap();

        assert!(!api_bind_requires_insecure_http_ack(loopback, false));
        assert!(api_bind_requires_insecure_http_ack(wildcard, false));
        assert!(api_bind_requires_insecure_http_ack(private, false));
        assert!(!api_bind_requires_insecure_http_ack(private, true));
    }
}
