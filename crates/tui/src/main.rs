//! `lazybox` — TUI client. Single binary, multiple modes:
//!
//!   lazybox                         default: in-process daemon + TUI
//!   lazybox --fresh                 wipe ~/.lazybox/v2/state.db + force
//!                                  the setup screen (testing first-run)
//!   lazybox --test                  throwaway tempdir repo + one fake
//!                                  workspace, no setup, no polling —
//!                                  for trying side panel + terminal
//!                                  pane end-to-end without GitHub
//!   lazybox daemon start            standalone daemon (for remote access)
//!   lazybox daemon stop             stop a running standalone daemon
//!   lazybox daemon status           show daemon status
//!   lazybox server api              foreground JSON HTTP API gateway
//!   lazybox slack init              interactive Slack token setup wizard
//!   lazybox slack doctor            read-only validation of an existing setup
//!   lazybox slack prune             archive stale per-(session, agent) channels
//!   lazybox hook-ingest --terminal N  forward a Claude lifecycle hook
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

/// Initialize tracing to write to the configured log file instead of
/// stderr.
fn init_tracing() -> anyhow::Result<()> {
    use std::fs::OpenOptions;

    let log_path = resolve_log_path();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| anyhow::anyhow!("open {}: {e}", log_path.display()))?;

    // Route the OS stderr into the same log file so native logs from
    // below the Rust layer (libghostty-vt Zig log, libgit2 stderr,
    // agent CLI noise) don't paint over the alternate-screen frame.
    lazybox_tui::platform::redirect_stderr_to_file(&file);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "lazybox=info,lazybox_gh=info,lazybox_server=info".into());

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
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
  lazybox server api [addr]   JSON HTTP API gateway (default 127.0.0.1:8787)
  lazybox --connect <socket>  attach a TUI to a running daemon
  lazybox slack init          set up the optional Slack mirror
  lazybox slack doctor        validate an existing Slack setup

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

// `pub` so the `lb` alias bin (src/bin/lb.rs) can call this via a
// `#[path]` include; harmless for the binary's own entrypoint.
#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Resolve --help / --version before init_tracing (which redirects stderr
    // into the log file and opens the daemon path). These short-circuit to
    // clean stdout and exit 0, so they work in a pipe and don't touch state.
    if wants_help(&args) {
        println!("{HELP}");
        return Ok(());
    }
    if wants_version(&args) {
        println!("lazybox {}", env!("CARGO_PKG_VERSION"));
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

/// `lazybox hook-ingest --terminal <id>` — the command Claude Code runs
/// on each lifecycle hook (lazybox injects it via `--settings` at spawn).
/// Reads the hook's JSON payload from stdin, normalizes it, and forwards
/// it to the running daemon over the IPC socket so the daemon can map it
/// to an `AgentState` transition.
///
/// Designed to never disrupt Claude: a missing daemon, a bad payload, or
/// no terminal id all resolve to a silent no-op (exit 0). A hook command
/// that errored or hung would stall Claude's turn.
async fn hook_ingest_subcommand(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let Some(terminal_id) = take_value(&mut args, "--terminal").and_then(|s| s.parse::<u64>().ok())
    else {
        // No terminal id → nothing to correlate. Drain stdin so Claude's
        // write doesn't block on a full pipe, then exit cleanly.
        let _ = read_stdin_to_string();
        return Ok(());
    };

    let payload = read_stdin_to_string();
    let Some(hook) = lazybox_agents::hook::parse_claude_hook(&payload) else {
        return Ok(());
    };

    let command = lazybox_ipc::Command::IngestHook {
        terminal_id: lazybox_ipc::TerminalId(terminal_id),
        hook,
    };

    // Best-effort forward. Connect, write one framed command, done — no
    // reply expected. A connect/write error means no daemon is listening
    // (e.g. hooks fired against a session whose daemon already exited);
    // that's a no-op, not a failure.
    if let Ok((_rd, mut wr)) = lazybox_ipc::transport::connect(&lifecycle::socket_path()).await {
        let _ = socket::write_frame(&mut wr, &command).await;
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
    let client = socket::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!("connect {}: {e}", socket_path.display()))?;

    tokio::task::spawn_blocking(move || {
        let mut model = lazybox_tui::realm::Model::new(client)?;
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
    let config = ServerConfig::from_user_config();

    lazybox_server::spawn_handler::recover_sessions(&config).await;
    lazybox_server::spawn_handler::restore_persisted_sessions(&config).await;
    lazybox_server::polling::migrate_legacy_sandbox(&config);

    let serve_config = config.clone();
    tokio::spawn(async move {
        let daemon = Server::new(serve_config);
        if let Err(e) = daemon.serve(server).await {
            tracing::error!("daemon exited: {e}");
        }
    });

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
    let setup_sources = std::sync::Arc::new(build_scope_sources().await);
    let needs_wizard = persisted_setup(&*config.store).is_none();
    let wizard_seed = if needs_wizard {
        Some((setup_report.clone(), setup_sources.clone()))
    } else {
        None
    };

    let store_for_save = config.store.clone();
    tokio::task::spawn_blocking(move || {
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
        // partial flows can fire many times.
        let store_for_save = std::sync::Arc::new(store_for_save);
        let hook: std::sync::Arc<dyn Fn(lazybox_tui::setup_flow::SetupOutcome) + Send + Sync> =
            std::sync::Arc::new(move |outcome| {
                let persisted = lazybox_tui::setup_flow::outcome_to_persisted(&outcome);
                lazybox_tui::setup_flow::save_persisted(&**store_for_save, &persisted);
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
        // Apply ~/.lazybox/config.yaml::{attention, ui, agent_shortcuts}
        // → sidebar + Model. Single load; subsequent reads happen
        // on-demand via Config::save_with for the writable parts.
        let user_config = lazybox_config::Config::load().unwrap_or_else(|e| {
            tracing::warn!("config.yaml load: {e}; using defaults");
            lazybox_config::Config::default()
        });
        let agent_shortcuts: std::collections::HashMap<char, String> =
            user_config.agent_shortcuts.clone().into_iter().collect();
        let ui_defaults = user_config.resolved_ui();
        model.apply_sidebar_config(
            user_config.attention.clone(),
            user_config.ui.collapsed_repos.clone(),
            agent_shortcuts,
            user_config.setup.default_agent.clone(),
            &user_config.display,
            &ui_defaults,
        );
        model.apply_action_key_overrides(user_config.ui.action_keys.clone());
        // Arm the feature tour for anyone who hasn't seen it. It
        // launches on wizard Finish for first-run users, or at startup
        // (just below) for returning ones.
        model.set_auto_tour(!user_config.ui.tour_seen);
        // Snippets — global (`<lazybox_home>/snippets.yaml`) merged
        // with the cwd's `.lazybox/snippets.yaml` (repo wins on key
        // conflict). Cwd is "wherever the user launched lazybox from",
        // which is the natural repo root for a single-repo workflow.
        let snippets =
            lazybox_config::Snippets::load_merged(std::env::current_dir().ok().as_deref());
        model.apply_snippets(snippets);
        model = model.with_splits(user_config.ui.sidebar_pct, user_config.ui.right_top_pct);
        if let Some((report, sources)) = wizard_seed {
            model.start_setup_wizard(report, sources);
        } else {
            // Returning user — setup already done, so there's no
            // wizard to finish behind. Surface the tour now if it
            // hasn't been seen (e.g. an upgrade into this feature).
            model.maybe_mount_tour();
        }
        lazybox_tui::realm::model::run_loop_with_model(model)
    })
    .await
    .map_err(|e| anyhow::anyhow!("realm task panicked: {e}"))?
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
        Some("api") => server_api(args.get(1)).await,
        _ => {
            eprintln!("usage: lazybox server [start|stop|status|api [addr:port]]");
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

    let config = ServerConfig::from_user_config();
    lazybox_server::spawn_handler::recover_sessions(&config).await;
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

async fn server_api(addr_arg: Option<&String>) -> anyhow::Result<()> {
    let bind_addr = match addr_arg {
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

    let config = ServerConfig::from_user_config();
    lazybox_server::spawn_handler::recover_sessions(&config).await;
    println!("lazybox API listening on http://{bind_addr}");
    if token.is_some() {
        println!("lazybox API bearer auth enabled via LAZYBOX_API_TOKEN");
    } else {
        println!("lazybox API bearer auth disabled; bound to localhost by default");
    }

    lazybox_server::api_gateway::serve(
        config,
        lazybox_server::api_gateway::GatewayOptions {
            bind_addr,
            bearer_token: token,
        },
    )
    .await?;
    Ok(())
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
}
