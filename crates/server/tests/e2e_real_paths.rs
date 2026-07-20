//! Integration tier: the recurring-regression paths, exercised for real
//! (#410).
//!
//! Every test here is named `e2e_*` and runs REAL subprocesses — git,
//! tmux, or an actual agent binary — through the actual serve loop,
//! asserting the user-visible outcome. This is the tier the unit suites
//! deliberately can't provide: a mocked backend or a pre-seeded worktree
//! passes green while the real path (provisioning, restart recovery,
//! agent boot) regresses. `.config/nextest.toml` grants `e2e_*` a wider
//! per-test timeout.
//!
//! Three gating levels:
//! - real git only → always runs (CI runners and dev machines have git);
//! - real tmux → runs where a modern tmux exists, skips loudly
//!   elsewhere; `LAZYBOX_E2E_REQUIRE=1` (the nightly lane) turns that
//!   skip into a failure so the path can't go silently unexercised;
//! - real agent binaries (`claude`, `codex`) → `#[ignore]`, opt-in via
//!   `LAZYBOX_E2E_LIVE_AGENTS=1` plus `--run-ignored`, because they need
//!   an installed, authenticated CLI and consume real tokens.

use lazybox_ipc::{Command, Event, TerminalKind, channel};
use lazybox_server::backend::TmuxBackend;
use lazybox_server::backend::tmux::modern_tmux_version;
use lazybox_server::{Server, ServerConfig};
use lazybox_store::MemoryStore;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Real subprocesses (clone, worktree add, tmux attach, agent boot) make
/// these tests seconds-slow, not milliseconds-slow. nextest's `e2e_*`
/// override covers the gap above the workspace's 10s default.
const TEST_DEADLINE: Duration = Duration::from_secs(120);

/// Whether the environment demands the real path instead of allowing a
/// skip (the nightly lane sets this; see `.github/workflows/nightly.yml`).
/// A silent skip-as-pass is exactly how the restart-scrollback path went
/// unexercised while "fixed" (#393) — under `LAZYBOX_E2E_REQUIRE` a
/// missing prerequisite is a test failure, not a shrug.
fn e2e_required() -> bool {
    std::env::var("LAZYBOX_E2E_REQUIRE").is_ok_and(|v| v == "1")
}

fn skip_or_fail(what: &str) {
    if e2e_required() {
        panic!("LAZYBOX_E2E_REQUIRE=1 but {what} is unavailable");
    }
    eprintln!("{what} unavailable — skipping (set LAZYBOX_E2E_REQUIRE=1 to make this fail)");
}

/// `LAZYBOX_HOME` isolation, same contract as the spawn_handler tests:
/// an empty home resolves every config reader to defaults (what CI sees).
struct IsolatedConfigHome {
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedConfigHome {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("LAZYBOX_HOME");
        // SAFETY: process-global, but nextest runs one test per process
        // and an empty dir resolves readers to CI defaults either way.
        unsafe { std::env::set_var("LAZYBOX_HOME", tmp.path()) };
        Self { _tmp: tmp, prev }
    }
}

impl Drop for IsolatedConfigHome {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("LAZYBOX_HOME", v) },
            None => unsafe { std::env::remove_var("LAZYBOX_HOME") },
        }
    }
}

/// Run git with fully-isolated config so the developer's global git
/// setup (signing hooks, templates, aliases) can't leak into the test.
fn git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "e2e")
        .env("GIT_AUTHOR_EMAIL", "e2e@test")
        .env("GIT_COMMITTER_NAME", "e2e")
        .env("GIT_COMMITTER_EMAIL", "e2e@test")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A local "GitHub" for `o/r`: a real upstream repo with one commit on
/// `main`, plus a bare clone of it pre-seeded where the daemon's
/// `WorktreeManager` expects `o/r`'s bare mirror. Provisioning then runs
/// its REAL machinery — health probe, fetch, `git worktree add` — with
/// the network swapped out for the local filesystem.
fn seed_local_upstream(worktree_root: &Path) -> tempfile::TempDir {
    let upstream = tempfile::TempDir::new().unwrap();
    git(upstream.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.path().join("README.md"), "e2e upstream\n").unwrap();
    git(upstream.path(), &["add", "."]);
    git(
        upstream.path(),
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "seed"],
    );
    let bare =
        lazybox_git_ops::WorktreeManager::new(worktree_root.to_path_buf()).bare_path("o", "r");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        upstream.path(),
        &[
            "clone",
            "--bare",
            "-q",
            &upstream.path().to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    );
    upstream
}

fn task(
    key: &str,
    url: &str,
    branch: Option<&str>,
    closes: Vec<lazybox_core::TaskId>,
) -> lazybox_core::Task {
    lazybox_core::Task {
        id: lazybox_core::TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: "t".into(),
        body: None,
        state: lazybox_core::TaskState::Open,
        role: lazybox_core::TaskRole::Author,
        ci: lazybox_core::CiStatus::None,
        review: lazybox_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: url.into(),
        repo: Some("o/r".into()),
        branch: branch.map(Into::into),
        base_branch: None,
        updated_at: chrono::Utc::now(),
        created_at: None,
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Mergeable,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        closes_issues: closes,
    }
}

fn save_workspace(config: &ServerConfig, ws: &lazybox_core::Workspace) {
    config
        .store
        .save_workspace(&lazybox_store::WorkspaceRecord {
            key: ws.key.as_str().to_string(),
            created_at: ws.created_at,
            workspace_json: Some(serde_json::to_string(ws).unwrap()),
        })
        .unwrap();
}

async fn run_daemon(config: ServerConfig) -> (lazybox_ipc::Client, tokio::task::JoinHandle<()>) {
    let (client, server) = channel::pair();
    let handle = tokio::spawn(async move {
        let _ = Server::new(config).serve(server).await;
    });
    (client, handle)
}

async fn subscribed(config: ServerConfig) -> (lazybox_ipc::Client, tokio::task::JoinHandle<()>) {
    let (mut client, handle) = run_daemon(config).await;
    client.send(Command::Subscribe).unwrap();
    let _snapshot = client.recv().await.expect("snapshot");
    (client, handle)
}

async fn wait_for<F: FnMut(&Event) -> bool>(
    client: &mut lazybox_ipc::Client,
    mut pred: F,
    budget: Duration,
) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, client.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return Some(ev);
                }
            }
            _ => return None,
        }
    }
    None
}

fn load_workspace(
    config: &ServerConfig,
    key: &lazybox_core::WorkspaceKey,
) -> lazybox_core::Workspace {
    serde_json::from_str(
        &config
            .store
            .get_workspace(key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap()
}

/// #404, the uncovered shape: a spawn on an issue workspace with NO
/// pre-seeded session, so the daemon runs REAL worktree provisioning
/// (health-probe the bare clone, fetch, `git worktree add`), and then an
/// issue→PR collapse migrates that real worktree. The historical
/// rebadge test opts out of provisioning ("seed an existing local
/// session so it never clones…"); this one is the path users actually
/// hit, minus only the network.
#[tokio::test]
async fn e2e_spawn_provisions_a_real_worktree_and_collapse_carries_it_to_the_pr() {
    let _home = IsolatedConfigHome::new();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let _upstream = seed_local_upstream(root.path());
        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            Arc::new(MemoryStore::new()),
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );

        // The issue is branchless — provisioning must cut a fresh branch
        // off the upstream default, the real shape of `w` on an issue.
        let issue = lazybox_core::Workspace::from_task(
            task("o/r#50", "https://github.com/o/r/issues/50", None, vec![]),
            chrono::Utc::now(),
        );
        let issue_task_id = issue.primary_task().unwrap().id.clone();
        let issue_key = issue.key.clone();
        let pr = lazybox_core::Workspace::from_task(
            task(
                "o/r#51",
                "https://github.com/o/r/pull/51",
                Some("feat"),
                vec![issue_task_id],
            ),
            chrono::Utc::now(),
        );
        let pr_key = pr.key.clone();
        save_workspace(&config, &issue);
        save_workspace(&config, &pr);

        let (mut client, _daemon) = subscribed(config.clone()).await;
        client
            .send(Command::Spawn {
                model_alias: None,
                session_key: issue_key.as_str().into(),
                session_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        assert!(
            wait_for(
                &mut client,
                |e| matches!(e, Event::TerminalSpawned { .. }),
                Duration::from_secs(60),
            )
            .await
            .is_some(),
            "spawn (including real provisioning) must complete"
        );

        // The user-visible outcome of provisioning: a real checkout of
        // the upstream's content, not the empty-dir fallback the daemon
        // degrades to when cloning fails.
        let issue_ws = load_workspace(&config, &issue_key);
        assert_eq!(
            issue_ws.sessions.len(),
            1,
            "provisioning persists one session"
        );
        let worktree = issue_ws.sessions[0].worktree_path.clone();
        assert!(
            worktree.join("README.md").exists(),
            "worktree at {} must hold the upstream's files — an empty dir here \
             means provisioning silently fell back (#403/#405 shape)",
            worktree.display(),
        );
        assert!(
            worktree.join(".git").exists(),
            "worktree must be a real git worktree, not a bare mkdir"
        );

        // Issue→PR collapse must carry the provisioned session across.
        client
            .send(Command::CollapseIntoPr {
                issue_workspace_key: issue_key.as_str().into(),
            })
            .unwrap();
        let pr_session_key: lazybox_core::SessionKey = (&pr_key).into();
        let issue_session_key: lazybox_core::SessionKey = (&issue_key).into();
        assert!(
            wait_for(
                &mut client,
                |e| matches!(
                    e,
                    Event::TerminalsRebadged { from, to }
                        if *from == issue_session_key && *to == pr_session_key
                ),
                Duration::from_secs(30),
            )
            .await
            .is_some(),
            "collapse must rebadge the live terminal onto the PR"
        );
        assert!(
            wait_for(
                &mut client,
                |e| matches!(e, Event::WorkspaceMerged { .. }),
                Duration::from_secs(30),
            )
            .await
            .is_some(),
            "collapse must settle with WorkspaceMerged"
        );

        let pr_ws = load_workspace(&config, &pr_key);
        assert_eq!(
            pr_ws.sessions.len(),
            1,
            "the session must live on the PR now"
        );
        let migrated = pr_ws.sessions[0].worktree_path.clone();
        assert!(
            migrated.join("README.md").exists(),
            "the migrated worktree at {} must still be a usable checkout",
            migrated.display(),
        );
        assert!(
            mock.released_keys().await.is_empty(),
            "the backend session must survive the collapse, not be killed"
        );
    })
    .await
    .expect("deadline");
}

/// #306/#393 through the FULL serve loop: a session outlives a daemon
/// restart and the reattached client still has deep scrollback. The
/// backend-only variant lives in `tmux_restart.rs`; this one drives the
/// same story end-to-end — Spawn command, real interactive shell in
/// real tmux, daemon torn down, second daemon recovers via
/// `recover_sessions`, and the client's `Snapshot` replay (the bytes
/// the TUI would render) reaches history that scrolled out long ago.
#[tokio::test]
async fn e2e_serve_loop_restart_recovers_session_with_deep_scrollback() {
    if modern_tmux_version().is_none() {
        skip_or_fail("modern tmux");
        return;
    }
    let _home = IsolatedConfigHome::new();
    let socket = format!("lazybox-e2e-{}", std::process::id());
    let store: Arc<MemoryStore> = Arc::new(MemoryStore::new());
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let config = ServerConfig::with_store_and_backend(store.clone(), Arc::new(backend));
        let (mut client, daemon) = subscribed(config).await;

        let cwd = tempfile::TempDir::new().unwrap();
        client
            .send(Command::Spawn {
                model_alias: None,
                session_key: "test:e2e-restart".into(),
                session_id: None,
                kind: TerminalKind::Shell,
                cwd: Some(cwd.path().to_string_lossy().into_owned()),
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        let terminal_id = match wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(30),
        )
        .await
        .expect("TerminalSpawned")
        {
            Event::TerminalSpawned { terminal_id, .. } => terminal_id,
            _ => unreachable!(),
        };

        // Fill >6 screens of history, then replace the interactive shell
        // with `sleep`: the daemon teardown below drops the attach PTY,
        // whose parting EOT tmux forwards into the pane — an interactive
        // shell would exit on it and take the session down, which is not
        // the shape of a real agent surviving a daemon crash.
        client
            .send(Command::Write {
                terminal_id,
                bytes: b"for i in $(seq 1 200); do echo line-$i; done; exec sleep 300\n".to_vec(),
            })
            .unwrap();
        let mut seen = Vec::new();
        let done = wait_for(
            &mut client,
            |e| {
                if let Event::TerminalOutput {
                    terminal_id: id,
                    bytes,
                    ..
                } = e
                    && *id == terminal_id
                {
                    seen.extend_from_slice(bytes);
                }
                String::from_utf8_lossy(&seen).contains("line-200")
            },
            Duration::from_secs(30),
        )
        .await;
        assert!(done.is_some(), "the shell must produce all 200 lines");

        // Daemon "crash": close the connection and wait for the serve
        // loop (and with it every DaemonPty replay ring) to die. The
        // tmux server and its pane history survive.
        drop(client);
        let _ = daemon.await;

        // Second daemon on the same socket + store: the binary's startup
        // sequence is recover_sessions → serve.
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let config = ServerConfig::with_store_and_backend(store.clone(), Arc::new(backend));
        lazybox_server::spawn_handler::recover_sessions(&config).await;
        // The recovery pump reattaches (and seeds the replay ring from
        // tmux history) on its own task; reconnect until the snapshot
        // carries the complete baseline a reconnecting TUI would render.
        // Each attempt is a fresh connection, exactly like a TUI retry.
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        let terminals = loop {
            ticker.tick().await;
            let (mut client, _daemon) = run_daemon(config.clone()).await;
            client.send(Command::Subscribe).unwrap();
            let snapshot = wait_for(
                &mut client,
                |e| matches!(e, Event::Snapshot { .. }),
                Duration::from_secs(10),
            )
            .await
            .expect("snapshot after restart");
            let Event::Snapshot { terminals, .. } = snapshot else {
                unreachable!();
            };
            assert_eq!(terminals.len(), 1, "the session must survive the restart");
            if terminals[0].replay_available {
                break terminals;
            }
        };
        let replay = String::from_utf8_lossy(&terminals[0].replay).into_owned();
        // line-5 scrolled out of the visible pane ~170 lines ago; only
        // the capture-pane history seed can supply it. This is the
        // user-visible "scrollback survives a restart" contract.
        assert!(
            replay.contains("line-5\r"),
            "recovered replay must reach deep history, got {} bytes: {:?}…",
            replay.len(),
            &replay[..replay.len().min(400)],
        );
        assert!(
            replay.contains("line-200"),
            "recovered replay must include the latest output"
        );
        assert_eq!(
            terminals[0].session_key,
            lazybox_core::SessionKey::from("test:e2e-restart"),
            "the recovered terminal must keep its workspace identity \
             (the #404 family: recovery losing the session↔workspace link)"
        );
    })
    .await;
    let _ = std::process::Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    result.expect("deadline");
}

/// Whether a live-agent e2e test should run: opt-in (real tokens, real
/// authenticated CLI) and loud when opted in but impossible.
fn live_agent_available(binary: &str) -> bool {
    if std::env::var("LAZYBOX_E2E_LIVE_AGENTS").map(|v| v == "1") != Ok(true) {
        eprintln!("LAZYBOX_E2E_LIVE_AGENTS!=1 — skipping live {binary} test");
        return false;
    }
    let found = std::process::Command::new("which")
        .arg(binary)
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        found,
        "LAZYBOX_E2E_LIVE_AGENTS=1 but `{binary}` is not on PATH"
    );
    true
}

/// Spawn a REAL agent binary in real tmux through the serve loop and
/// wait for the daemon's state pipeline to report it ready (`Idle`) or
/// asking (`InputNeeded` — codex opens a trust-this-directory chooser
/// in a fresh cwd, and surfacing that `?` fast is precisely the #399
/// contract). This is the end-to-end boot story the #225/#397/#399
/// family kept breaking in shipped builds while unit fixtures stayed
/// green: argv construction, PTY prompt protocol, live detection —
/// against the binary users actually run.
async fn live_agent_boots_to_ready(agent: &str, socket: &str) {
    let _home = IsolatedConfigHome::new();
    let store: Arc<MemoryStore> = Arc::new(MemoryStore::new());
    let backend = TmuxBackend::with_socket(socket).expect("conf written");
    let config = ServerConfig::with_store_and_backend(store, Arc::new(backend));
    let (mut client, _daemon) = subscribed(config).await;

    let cwd = tempfile::TempDir::new().unwrap();
    git(cwd.path(), &["init", "-q", "-b", "main"]);
    client
        .send(Command::Spawn {
            model_alias: None,
            session_key: format!("test:e2e-{agent}").into(),
            session_id: None,
            kind: TerminalKind::Agent(agent.into()),
            cwd: Some(cwd.path().to_string_lossy().into_owned()),
            initial_prompt: None,
            on_main: false,
        })
        .unwrap();
    assert!(
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(30),
        )
        .await
        .is_some(),
        "TerminalSpawned"
    );
    let ready = wait_for(
        &mut client,
        |e| {
            matches!(
                e,
                Event::AgentState {
                    state: lazybox_ipc::AgentState::Idle | lazybox_ipc::AgentState::InputNeeded,
                    ..
                }
            )
        },
        Duration::from_secs(90),
    )
    .await;
    match ready {
        Some(Event::AgentState { state, .. }) => {
            eprintln!("live {agent} booted to detected state {state:?}")
        }
        _ => panic!(
            "a real `{agent}` must boot to a detected ready/asking state \
             within 90s — no state event means spawn argv, the PTY protocol, \
             or readiness detection is broken against the real binary"
        ),
    }
}

#[tokio::test]
#[ignore = "live agent: needs an installed, authenticated `claude` — LAZYBOX_E2E_LIVE_AGENTS=1 + --run-ignored"]
async fn e2e_real_claude_boots_to_a_detected_ready_state() {
    if !live_agent_available("claude") {
        return;
    }
    let socket = format!("lazybox-e2e-claude-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, live_agent_boots_to_ready("claude", &socket)).await;
    let _ = std::process::Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    result.expect("deadline");
}

#[tokio::test]
#[ignore = "live agent: needs an installed, authenticated `codex` — LAZYBOX_E2E_LIVE_AGENTS=1 + --run-ignored"]
async fn e2e_real_codex_boots_to_a_detected_ready_state() {
    if !live_agent_available("codex") {
        return;
    }
    let socket = format!("lazybox-e2e-codex-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, live_agent_boots_to_ready("codex", &socket)).await;
    let _ = std::process::Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    result.expect("deadline");
}

/// #393 through the FULL serve loop with real tmux, no restart: the
/// scroll-triggered `Command::FetchScrollback` must come back as an
/// `Event::TerminalScrollback` carrying tmux's retained pane history —
/// the same depth a restarted daemon would seed. The landed unit tests
/// cover the mock round trip and the backend call in isolation; this
/// pins the wire path a real TUI actually rides when the user scrolls
/// a live session up.
#[tokio::test]
async fn e2e_live_scroll_fetch_serves_deep_history_without_restart() {
    if modern_tmux_version().is_none() {
        skip_or_fail("modern tmux");
        return;
    }
    let _home = IsolatedConfigHome::new();
    let socket = format!("lazybox-e2e-scroll-{}", std::process::id());
    let result = timeout(TEST_DEADLINE, async {
        let backend = TmuxBackend::with_socket(&socket).expect("conf written");
        let config = ServerConfig::with_store_and_backend(
            Arc::new(MemoryStore::new()),
            Arc::new(backend),
        );
        let (mut client, _daemon) = subscribed(config).await;

        let cwd = tempfile::TempDir::new().unwrap();
        client
            .send(Command::Spawn {
                model_alias: None,
                session_key: "test:e2e-scroll".into(),
                session_id: None,
                kind: TerminalKind::Shell,
                cwd: Some(cwd.path().to_string_lossy().into_owned()),
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        let terminal_id = match wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            Duration::from_secs(30),
        )
        .await
        .expect("TerminalSpawned")
        {
            Event::TerminalSpawned { terminal_id, .. } => terminal_id,
            _ => unreachable!(),
        };

        client
            .send(Command::Write {
                terminal_id,
                bytes: b"for i in $(seq 1 200); do echo line-$i; done\n".to_vec(),
            })
            .unwrap();
        let mut seen = Vec::new();
        let done = wait_for(
            &mut client,
            |e| {
                if let Event::TerminalOutput {
                    terminal_id: id,
                    bytes,
                    ..
                } = e
                    && *id == terminal_id
                {
                    seen.extend_from_slice(bytes);
                }
                String::from_utf8_lossy(&seen).contains("line-200")
            },
            Duration::from_secs(30),
        )
        .await;
        assert!(done.is_some(), "the shell must produce all 200 lines");

        // What the TUI sends on the first upward scroll of a visit.
        client
            .send(Command::FetchScrollback { terminal_id })
            .unwrap();
        let reply = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalScrollback { terminal_id: id, .. } if *id == terminal_id),
            Duration::from_secs(10),
        )
        .await
        .expect("TerminalScrollback reply — a live session must serve deep history");
        let Event::TerminalScrollback { replay, seq, .. } = reply else {
            unreachable!();
        };
        let replay = String::from_utf8_lossy(&replay);
        // line-5 scrolled out of the 32-row pane ~170 lines ago: only
        // tmux's pane history can supply it on a never-restarted
        // session. The `\r` pins the capture-normalized exact line.
        assert!(
            replay.contains("line-5\r"),
            "live fetch must reach deep history, got {} bytes: {:?}…",
            replay.len(),
            &replay[..replay.len().min(400)],
        );
        assert!(
            replay.contains("line-200"),
            "live fetch must include the latest output"
        );
        assert!(seq > 0, "reply must carry the live seq high-water mark");
    })
    .await;
    let _ = std::process::Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    result.expect("deadline");
}
