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

use lazybox_ipc::{Command, Event, TerminalInputIntent, TerminalKind, channel};
use lazybox_server::backend::SessionBackend;
use lazybox_server::backend::TmuxBackend;
use lazybox_server::backend::tmux::modern_tmux_version;
use lazybox_server::{Server, ServerConfig};
use lazybox_store::MemoryStore;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
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
static CONFIG_HOME_LOCK: Mutex<()> = Mutex::new(());

struct IsolatedConfigHome {
    _guard: MutexGuard<'static, ()>,
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedConfigHome {
    fn new() -> Self {
        let guard = CONFIG_HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("LAZYBOX_HOME");
        // SAFETY: the process-global mutation is serialized for every
        // test in this executable by `CONFIG_HOME_LOCK`.
        unsafe { std::env::set_var("LAZYBOX_HOME", tmp.path()) };
        Self {
            _guard: guard,
            _tmp: tmp,
            prev,
        }
    }

    fn path(&self) -> &Path {
        self._tmp.path()
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

/// A local "GitHub" repo with one commit on `main`, plus a bare clone
/// pre-seeded where the daemon's `WorktreeManager` expects its mirror.
/// Provisioning then runs
/// its REAL machinery — health probe, fetch, `git worktree add` — with
/// the network swapped out for the local filesystem.
fn seed_local_upstream(worktree_root: &Path, owner: &str, repo: &str) -> tempfile::TempDir {
    let upstream = tempfile::TempDir::new().unwrap();
    git(upstream.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(upstream.path().join("README.md"), "e2e upstream\n").unwrap();
    git(upstream.path(), &["add", "."]);
    git(
        upstream.path(),
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "seed"],
    );
    let bare =
        lazybox_git_ops::WorktreeManager::new(worktree_root.to_path_buf()).bare_path(owner, repo);
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

fn publish_branch(upstream: &Path, bare: &Path, branch: &str) {
    git(upstream, &["branch", branch]);
    git(
        bare,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("+refs/heads/{branch}:refs/heads/{branch}"),
            &format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
        ],
    );
}

fn task(
    key: &str,
    url: &str,
    branch: Option<&str>,
    closes: Vec<lazybox_core::TaskId>,
) -> lazybox_core::Task {
    let repo = key.split_once('#').map(|(repo, _)| repo.to_string());
    lazybox_core::Task {
        author: String::new(),
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
        repo,
        branch: branch.map(Into::into),
        base_branch: None,
        updated_at: chrono::Utc::now(),
        created_at: None,
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        reviews: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Mergeable,
        is_behind_base: false,
        merge_blocked: false,
        approval_policy: Default::default(),
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        changed_files: 0,
        kind: None,
        closes_issues: closes,
        linked_tasks: vec![],
        parent: None,
        priority: None,
        state_label: None,
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

fn send_spawn(
    client: &mut lazybox_ipc::Client,
    key: &lazybox_core::WorkspaceKey,
    kind: TerminalKind,
    initial_prompt: Option<&str>,
) {
    client
        .send(Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: key.as_str().into(),
            session_id: None,
            client_request_id: None,
            kind,
            cwd: None,
            initial_prompt: initial_prompt.map(Into::into),
            initial_snippet: None,
            on_main: false,
        })
        .unwrap();
}

async fn spawn_and_capture(
    client: &mut lazybox_ipc::Client,
    mock: &lazybox_server::backend::MockBackend,
    key: &lazybox_core::WorkspaceKey,
    kind: TerminalKind,
    initial_prompt: Option<&str>,
) -> String {
    let before = mock
        .list()
        .await
        .unwrap()
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    send_spawn(client, key, kind, initial_prompt);
    wait_for(
        client,
        |event| matches!(event, Event::TerminalSpawned { .. }),
        Duration::from_secs(30),
    )
    .await
    .expect("TerminalSpawned");
    mock.list()
        .await
        .unwrap()
        .into_iter()
        .find(|key| !before.contains(key))
        .expect("new backend session")
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
        let _upstream = seed_local_upstream(root.path(), "o", "r");
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
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: issue_key.as_str().into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("claude".into()),
                cwd: None,
                initial_prompt: None,
                initial_snippet: None,
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

#[tokio::test]
async fn e2e_sessionless_branch_holder_is_reclaimed_at_the_current_workspace_path() {
    let _home = IsolatedConfigHome::new();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let upstream = seed_local_upstream(root.path(), "acme", "core");
        let manager = lazybox_git_ops::WorktreeManager::new(root.path().to_path_buf());
        let bare = manager.bare_path("acme", "core");
        let branch = "issue-652-t";
        publish_branch(upstream.path(), &bare, branch);

        let leaked = root
            .path()
            .join("worktrees/github-acme-core")
            .join("issue-584-publish-v0-1-8-tag-artifacts-homebrew-release");
        std::fs::create_dir_all(leaked.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                branch,
                &leaked.to_string_lossy(),
                &format!("refs/heads/{branch}"),
            ],
        );
        let leaked = std::fs::canonicalize(leaked).unwrap();

        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            Arc::new(MemoryStore::new()),
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );
        let workspace = lazybox_core::Workspace::from_task(
            task(
                "acme/core#652",
                "https://github.com/acme/core/issues/652",
                None,
                vec![],
            ),
            chrono::Utc::now(),
        );
        let workspace_key = workspace.key.clone();
        let intended = lazybox_server::spawn_handler::worktree_path_for_session(&workspace, 0);
        assert_ne!(
            leaked, intended,
            "fixture must reproduce a branch/workspace-path mismatch"
        );
        save_workspace(&config, &workspace);

        let (mut client, _daemon) = subscribed(config.clone()).await;
        let backend = spawn_and_capture(
            &mut client,
            &mock,
            &workspace_key,
            TerminalKind::Shell,
            None,
        )
        .await;

        assert!(!leaked.exists(), "the session-less holder is reclaimed");
        assert_eq!(
            mock.cwd_for(&backend).await.as_deref(),
            Some(intended.as_path()),
            "the retry provisions the current workspace's intended path"
        );
        let saved = load_workspace(&config, &workspace_key);
        assert_eq!(saved.sessions.len(), 1);
        assert_eq!(saved.sessions[0].worktree_path, intended);
        let listed = git(&bare, &["worktree", "list", "--porcelain"]);
        let branch_line = format!("branch refs/heads/{branch}");
        assert_eq!(
            listed.lines().filter(|line| *line == branch_line).count(),
            1,
            "the branch is checked out exactly once after recovery"
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn e2e_clean_companion_pr_checkout_is_adopted_in_place() {
    let _home = IsolatedConfigHome::new();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let upstream = seed_local_upstream(root.path(), "acme", "core");
        let manager = lazybox_git_ops::WorktreeManager::new(root.path().to_path_buf());
        let bare = manager.bare_path("acme", "core");
        let branch = "issue-136-clean-companion";
        publish_branch(upstream.path(), &bare, branch);

        let companion = root.path().join("worktrees/github-acme-core").join(branch);
        std::fs::create_dir_all(companion.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                branch,
                &companion.to_string_lossy(),
                &format!("refs/heads/{branch}"),
            ],
        );
        let companion = std::fs::canonicalize(companion).unwrap();
        assert!(
            git(&companion, &["status", "--porcelain"]).is_empty(),
            "fixture must exercise the clean companion path"
        );

        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            Arc::new(MemoryStore::new()),
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );
        let workspace = lazybox_core::Workspace::from_task(
            task(
                "acme/core#96",
                "https://github.com/acme/core/pull/96",
                Some(branch),
                vec![],
            ),
            chrono::Utc::now(),
        );
        let workspace_key = workspace.key.clone();
        let derived = lazybox_server::spawn_handler::worktree_path_for_session(&workspace, 0);
        save_workspace(&config, &workspace);

        let (mut client, _daemon) = subscribed(config.clone()).await;
        let backend = spawn_and_capture(
            &mut client,
            &mock,
            &workspace_key,
            TerminalKind::Shell,
            None,
        )
        .await;

        assert_eq!(
            mock.cwd_for(&backend).await.as_deref(),
            Some(companion.as_path()),
            "a clean companion checkout keeps the same adoption contract as one with WIP"
        );
        assert!(companion.exists());
        assert!(!derived.exists(), "adoption must not relocate the checkout");
        assert_eq!(
            load_workspace(&config, &workspace_key).sessions[0].worktree_path,
            companion
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn e2e_stopped_session_does_not_make_a_clean_holder_live() {
    let _home = IsolatedConfigHome::new();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let upstream = seed_local_upstream(root.path(), "acme", "core");
        let manager = lazybox_git_ops::WorktreeManager::new(root.path().to_path_buf());
        let bare = manager.bare_path("acme", "core");
        let branch = "stopped-holder";
        publish_branch(upstream.path(), &bare, branch);

        let stopped_path = root
            .path()
            .join("worktrees/github-acme-core")
            .join("old-stopped-workspace");
        std::fs::create_dir_all(stopped_path.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                branch,
                &stopped_path.to_string_lossy(),
                &format!("refs/heads/{branch}"),
            ],
        );
        let stopped_path = std::fs::canonicalize(stopped_path).unwrap();

        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            Arc::new(MemoryStore::new()),
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );
        let mut old_workspace = lazybox_core::Workspace::from_task(
            task(
                "acme/core#12",
                "https://github.com/acme/core/issues/12",
                None,
                vec![],
            ),
            chrono::Utc::now(),
        );
        let old_key = old_workspace.key.clone();
        let mut stopped_session = lazybox_core::WorkspaceSession::new(
            old_key,
            lazybox_core::SessionKind::Shell,
            stopped_path.clone(),
            chrono::Utc::now(),
        );
        stopped_session.state = lazybox_core::SessionRunState::Stopped;
        old_workspace.add_session(stopped_session);
        save_workspace(&config, &old_workspace);

        let target = lazybox_core::Workspace::from_task(
            task(
                "acme/core#97",
                "https://github.com/acme/core/pull/97",
                Some(branch),
                vec![],
            ),
            chrono::Utc::now(),
        );
        let target_key = target.key.clone();
        let intended = lazybox_server::spawn_handler::worktree_path_for_session(&target, 0);
        save_workspace(&config, &target);

        let (mut client, _daemon) = subscribed(config.clone()).await;
        let backend =
            spawn_and_capture(&mut client, &mock, &target_key, TerminalKind::Shell, None).await;

        assert!(!stopped_path.exists(), "the stopped holder is reclaimed");
        assert_eq!(
            mock.cwd_for(&backend).await.as_deref(),
            Some(intended.as_path())
        );
        assert_eq!(
            load_workspace(&config, &target_key).sessions[0].worktree_path,
            intended
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn e2e_persisted_worktree_on_the_wrong_branch_fails_without_switching() {
    let _home = IsolatedConfigHome::new();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let upstream = seed_local_upstream(root.path(), "acme", "core");
        let manager = lazybox_git_ops::WorktreeManager::new(root.path().to_path_buf());
        let bare = manager.bare_path("acme", "core");
        publish_branch(upstream.path(), &bare, "expected");
        publish_branch(upstream.path(), &bare, "actual");

        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            Arc::new(MemoryStore::new()),
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );
        let mut workspace = lazybox_core::Workspace::from_task(
            task(
                "acme/core#98",
                "https://github.com/acme/core/pull/98",
                Some("expected"),
                vec![],
            ),
            chrono::Utc::now(),
        );
        let workspace_key = workspace.key.clone();
        let intended = lazybox_server::spawn_handler::worktree_path_for_session(&workspace, 0);
        std::fs::create_dir_all(intended.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                "actual",
                &intended.to_string_lossy(),
                "refs/heads/actual",
            ],
        );
        std::fs::write(intended.join("local-marker"), "preserve mismatch").unwrap();
        let mut session = lazybox_core::WorkspaceSession::new(
            workspace_key.clone(),
            lazybox_core::SessionKind::Shell,
            intended.clone(),
            chrono::Utc::now(),
        );
        session.worktree_branch = Some("expected".into());
        workspace.add_session(session);
        save_workspace(&config, &workspace);

        let (mut client, _daemon) = subscribed(config.clone()).await;
        send_spawn(&mut client, &workspace_key, TerminalKind::Shell, None);
        let failure = wait_for(
            &mut client,
            |event| {
                matches!(
                    event,
                    Event::WorktreeProgress {
                        status: lazybox_ipc::WorktreeStepStatus::Failed(message),
                        ..
                    } if lazybox_ipc::WorktreeRecovery::classify(message)
                        == lazybox_ipc::WorktreeRecovery::BranchMismatch
                )
            },
            Duration::from_secs(30),
        )
        .await;

        assert!(failure.is_some(), "the mismatch must be surfaced");
        assert!(mock.list().await.unwrap().is_empty());
        assert_eq!(git(&intended, &["branch", "--show-current"]), "actual");
        assert_eq!(
            std::fs::read_to_string(intended.join("local-marker")).unwrap(),
            "preserve mismatch"
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn e2e_cross_repo_pr_adopts_its_untracked_managed_worktree() {
    let home = IsolatedConfigHome::new();
    std::fs::write(
        home.path().join("config.yaml"),
        r#"
repos:
  acme/core:
    scripts:
      - name: companion-setup
        content: echo adopted companion
"#,
    )
    .unwrap();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let _upstream_a = seed_local_upstream(root.path(), "acme", "app");
        let upstream_b = seed_local_upstream(root.path(), "acme", "core");
        let manager = lazybox_git_ops::WorktreeManager::new(root.path().to_path_buf());
        let bare_b = manager.bare_path("acme", "core");
        let store = Arc::new(MemoryStore::new());
        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            store.clone(),
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );

        let issue = lazybox_core::Workspace::from_task(
            task(
                "acme/app#136",
                "https://github.com/acme/app/issues/136",
                None,
                vec![],
            ),
            chrono::Utc::now(),
        );
        let issue_id = issue.primary_task().unwrap().id.clone();
        let issue_key = issue.key.clone();
        save_workspace(&config, &issue);

        let (mut client, daemon) = subscribed(config.clone()).await;
        spawn_and_capture(
            &mut client,
            &mock,
            &issue_key,
            TerminalKind::Agent("claude".into()),
            None,
        )
        .await;

        let issue_ws = load_workspace(&config, &issue_key);
        let issue_path = issue_ws.sessions[0].worktree_path.clone();
        let issue_branch = git(&issue_path, &["branch", "--show-current"]);

        let companion_branch = "issue-136-cache-recovery";
        publish_branch(upstream_b.path(), &bare_b, companion_branch);
        let companion_path = root
            .path()
            .join("worktrees/github-acme-core")
            .join("issue-136-cache-recovery");
        std::fs::create_dir_all(companion_path.parent().unwrap()).unwrap();
        git(
            &bare_b,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                companion_branch,
                &companion_path.to_string_lossy(),
                &format!("refs/heads/{companion_branch}"),
            ],
        );
        let companion_path = std::fs::canonicalize(companion_path).unwrap();
        let wip = b"uncommitted companion WIP\0\xff\n".to_vec();
        std::fs::write(companion_path.join("WIP.bin"), &wip).unwrap();

        let primary_pr = lazybox_core::Workspace::from_task(
            task(
                "acme/app#140",
                "https://github.com/acme/app/pull/140",
                Some(&issue_branch),
                vec![issue_id],
            ),
            chrono::Utc::now(),
        );
        let primary_key = primary_pr.key.clone();
        save_workspace(&config, &primary_pr);
        client
            .send(Command::CollapseIntoPr {
                issue_workspace_key: issue_key.as_str().into(),
            })
            .unwrap();
        wait_for(
            &mut client,
            |event| matches!(event, Event::WorkspaceMerged { .. }),
            Duration::from_secs(30),
        )
        .await
        .expect("same-repo issue to PR transfer");
        let primary_after = load_workspace(&config, &primary_key);
        assert_eq!(primary_after.sessions[0].worktree_path, issue_path);

        let companion_pr = lazybox_core::Workspace::from_task(
            task(
                "acme/core#96",
                "https://github.com/acme/core/pull/96",
                Some(companion_branch),
                vec![],
            ),
            chrono::Utc::now(),
        );
        let companion_key = companion_pr.key.clone();
        let derived_pr_path =
            lazybox_server::spawn_handler::worktree_path_for_session(&companion_pr, 0);
        save_workspace(&config, &companion_pr);

        let agent_key = spawn_and_capture(
            &mut client,
            &mock,
            &companion_key,
            TerminalKind::Agent("codex".into()),
            Some("continue the companion PR"),
        )
        .await;
        assert_eq!(
            mock.cwd_for(&agent_key).await.as_deref(),
            Some(companion_path.as_path())
        );

        let shell_key = spawn_and_capture(
            &mut client,
            &mock,
            &companion_key,
            TerminalKind::Shell,
            None,
        )
        .await;
        assert_eq!(
            mock.cwd_for(&shell_key).await.as_deref(),
            Some(companion_path.as_path())
        );

        let companion_after = load_workspace(&config, &companion_key);
        assert_eq!(companion_after.sessions.len(), 1);
        assert_eq!(
            companion_after.sessions[0].worktree_path, companion_path,
            "agents, shells, and the editor's persisted session lookup must share the adopted path",
        );
        assert_eq!(std::fs::read(companion_path.join("WIP.bin")).unwrap(), wip);
        let setup_script = std::fs::read_to_string(
            companion_path
                .join("_lazybox")
                .join("scripts")
                .join("companion-setup"),
        )
        .expect("repo setup scripts must be applied to an adopted checkout");
        assert!(setup_script.contains("echo adopted companion"));
        assert!(
            !derived_pr_path.exists(),
            "adoption must not materialize a second PR-derived checkout"
        );
        let worktree_list = git(&bare_b, &["worktree", "list", "--porcelain"]);
        let companion_branch_line = format!("branch refs/heads/{companion_branch}");
        assert_eq!(
            worktree_list
                .lines()
                .filter(|line| *line == companion_branch_line)
                .count(),
            1,
            "the companion branch must still have exactly one checkout"
        );

        client
            .send(Command::DeleteOrphanedWorktree {
                path: companion_path.clone(),
                force: true,
            })
            .unwrap();
        let stale_delete = wait_for(
            &mut client,
            |event| matches!(event, Event::OrphanedWorktreeDeleted { .. }),
            Duration::from_secs(30),
        )
        .await
        .expect("stale orphan deletion result");
        let Event::OrphanedWorktreeDeleted { ok, error, .. } = stale_delete else {
            unreachable!()
        };
        assert!(!ok, "an adopted worktree is no longer an orphan");
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("no longer orphaned")),
            "unexpected stale-delete result: {error:?}"
        );
        assert_eq!(
            std::fs::read(companion_path.join("WIP.bin")).unwrap(),
            wip,
            "a stale confirmed deletion must not remove newly adopted WIP"
        );

        drop(client);
        let _ = daemon.await;

        let restarted_mock = lazybox_server::backend::MockBackend::new();
        let restarted = ServerConfig::with_store_backend_and_worktree_root(
            store,
            Arc::new(restarted_mock.clone()),
            root.path().to_path_buf(),
        );
        let (mut restarted_client, _restarted_daemon) = subscribed(restarted.clone()).await;
        let restarted_key = spawn_and_capture(
            &mut restarted_client,
            &restarted_mock,
            &companion_key,
            TerminalKind::Shell,
            None,
        )
        .await;
        assert_eq!(
            restarted_mock.cwd_for(&restarted_key).await.as_deref(),
            Some(companion_path.as_path()),
            "the adopted association must survive a daemon restart"
        );
    })
    .await
    .expect("deadline");
}

/// A branch held by a worktree lazybox can't even parse the ownership of
/// (a future workspace schema) must never be adopted or reclaimed — but
/// it must also never dead-end the PR's workspace (#721). The PR gets its
/// own *detached* checkout of the head, so the un-loadable holder's WIP
/// is left untouched while the workspace still comes up.
#[tokio::test]
async fn e2e_future_owned_branch_holder_provisions_a_detached_checkout() {
    let _home = IsolatedConfigHome::new();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let upstream = seed_local_upstream(root.path(), "acme", "core");
        let manager = lazybox_git_ops::WorktreeManager::new(root.path().to_path_buf());
        let bare = manager.bare_path("acme", "core");
        let branch = "future-owned";
        publish_branch(upstream.path(), &bare, branch);

        let candidate = root
            .path()
            .join("worktrees/github-acme-core")
            .join("future-owned");
        std::fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                branch,
                &candidate.to_string_lossy(),
                &format!("refs/heads/{branch}"),
            ],
        );
        let candidate = std::fs::canonicalize(candidate).unwrap();
        let marker = b"future owner WIP\n";
        std::fs::write(candidate.join("WIP.txt"), marker).unwrap();

        let store = Arc::new(MemoryStore::new());
        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            store,
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );

        let future = lazybox_core::Workspace::from_task(
            task(
                "acme/core#12",
                "https://github.com/acme/core/issues/12",
                None,
                vec![],
            ),
            chrono::Utc::now(),
        );
        let mut future_json = serde_json::to_value(&future).unwrap();
        future_json["schema"] = serde_json::json!(lazybox_core::WORKSPACE_SCHEMA_VERSION + 1);
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: future.key.as_str().to_string(),
                created_at: future.created_at,
                workspace_json: Some(serde_json::to_string(&future_json).unwrap()),
            })
            .unwrap();

        let pr = lazybox_core::Workspace::from_task(
            task(
                "acme/core#98",
                "https://github.com/acme/core/pull/98",
                Some(branch),
                vec![],
            ),
            chrono::Utc::now(),
        );
        let pr_key = pr.key.clone();
        save_workspace(&config, &pr);

        let (mut client, _daemon) = subscribed(config.clone()).await;
        let backend =
            spawn_and_capture(&mut client, &mock, &pr_key, TerminalKind::Shell, None).await;
        // The PR comes up in its own detached checkout of the branch head,
        // never adopting the future-owned holder's worktree.
        let cwd = mock.cwd_for(&backend).await.expect("pr cwd");
        assert_ne!(
            cwd.as_path(),
            candidate.as_path(),
            "the PR must get its own checkout, never the future-owned holder's"
        );
        assert!(
            !is_on_branch(&cwd, branch),
            "the PR's checkout is detached, never co-opting the held branch"
        );
        assert!(!load_workspace(&config, &pr_key).sessions.is_empty());
        // The un-loadable holder's branch and WIP are untouched.
        assert_eq!(std::fs::read(candidate.join("WIP.txt")).unwrap(), marker);
    })
    .await
    .expect("deadline");
}

/// Whether the worktree at `path` has `branch` checked out (vs a detached
/// HEAD or a different branch).
fn is_on_branch(path: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .current_dir(path)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == branch)
        .unwrap_or(false)
}

/// A branch already checked out in another workspace's *live* session is
/// held exclusively, yet the PR sharing it must not dead-end (#721). The
/// PR comes up in its own detached checkout of the head — beside the live
/// holder, never adopting it — so the holder's branch and WIP stay
/// untouched and both sessions coexist.
#[tokio::test]
async fn e2e_live_branch_holder_provisions_a_detached_checkout() {
    let _home = IsolatedConfigHome::new();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let upstream = seed_local_upstream(root.path(), "acme", "core");
        let manager = lazybox_git_ops::WorktreeManager::new(root.path().to_path_buf());
        let bare = manager.bare_path("acme", "core");
        let branch = "unrelated-holder";
        publish_branch(upstream.path(), &bare, branch);

        let unrelated = root
            .path()
            .join("worktrees/github-acme-core")
            .join("unrelated-holder");
        std::fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                branch,
                &unrelated.to_string_lossy(),
                &format!("refs/heads/{branch}"),
            ],
        );
        let unrelated = std::fs::canonicalize(unrelated).unwrap();
        let marker = b"do not touch this checkout\n";
        std::fs::write(unrelated.join("WIP.txt"), marker).unwrap();

        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            Arc::new(MemoryStore::new()),
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );
        let mut holder = lazybox_core::Workspace::from_task(
            task(
                "acme/core#12",
                "https://github.com/acme/core/pull/12",
                Some(branch),
                vec![],
            ),
            chrono::Utc::now(),
        );
        let holder_key = holder.key.clone();
        holder.add_session(lazybox_core::WorkspaceSession::new(
            holder_key.clone(),
            lazybox_core::SessionKind::Shell,
            unrelated.clone(),
            chrono::Utc::now(),
        ));
        save_workspace(&config, &holder);

        let pr = lazybox_core::Workspace::from_task(
            task(
                "acme/core#97",
                "https://github.com/acme/core/pull/97",
                Some(branch),
                vec![],
            ),
            chrono::Utc::now(),
        );
        let pr_key = pr.key.clone();
        save_workspace(&config, &pr);

        let (mut client, _daemon) = subscribed(config.clone()).await;
        let holder_backend =
            spawn_and_capture(&mut client, &mock, &holder_key, TerminalKind::Shell, None).await;
        assert_eq!(
            mock.cwd_for(&holder_backend).await.as_deref(),
            Some(unrelated.as_path())
        );

        let pr_backend =
            spawn_and_capture(&mut client, &mock, &pr_key, TerminalKind::Shell, None).await;
        let pr_cwd = mock.cwd_for(&pr_backend).await.expect("pr cwd");
        assert_ne!(
            pr_cwd.as_path(),
            unrelated.as_path(),
            "the PR gets its own detached checkout, never the live holder's worktree"
        );
        assert!(
            !is_on_branch(&pr_cwd, branch),
            "the PR's checkout is detached, never co-opting the held branch"
        );
        assert!(!load_workspace(&config, &pr_key).sessions.is_empty());
        // Both sessions coexist; the holder's branch and WIP stay untouched.
        let sessions = mock.list().await.unwrap();
        assert!(
            sessions.contains(&holder_backend) && sessions.contains(&pr_backend),
            "the holder and the PR's detached checkout both stay live"
        );
        assert!(
            is_on_branch(&unrelated, branch),
            "the live holder keeps sole ownership of the branch"
        );
        assert_eq!(std::fs::read(unrelated.join("WIP.txt")).unwrap(), marker);

        // A second spawn resolving the PR's persisted session must reuse
        // the detached worktree in place. A detached HEAD is not a stale
        // "wrong branch" — so provisioning is NOT re-run, and the Setup
        // step (mounts / setup scripts, and its "Setting up workspace"
        // progress) does not fire again on every spawn (#721 regression).
        send_spawn(
            &mut client,
            &pr_key,
            TerminalKind::Agent("claude".into()),
            None,
        );
        let mut re_ran_setup = false;
        let spawned = wait_for(
            &mut client,
            |event| match event {
                Event::WorktreeProgress {
                    session_key,
                    step: lazybox_ipc::WorktreeStep::Setup,
                    status: lazybox_ipc::WorktreeStepStatus::Started,
                    ..
                } if session_key.as_str() == pr_key.as_str() => {
                    re_ran_setup = true;
                    false
                }
                Event::TerminalSpawned { .. } => true,
                _ => false,
            },
            Duration::from_secs(30),
        )
        .await;
        assert!(spawned.is_some(), "the reused-worktree spawn completes");
        assert!(
            !re_ran_setup,
            "reusing a detached worktree must not re-provision or re-run setup (#721)"
        );
    })
    .await
    .expect("deadline");
}

#[tokio::test]
async fn e2e_pr_spawn_transfers_its_live_managed_branch_owner() {
    let _home = IsolatedConfigHome::new();
    timeout(TEST_DEADLINE, async {
        let root = tempfile::TempDir::new().unwrap();
        let upstream = seed_local_upstream(root.path(), "acme", "core");
        let manager = lazybox_git_ops::WorktreeManager::new(root.path().to_path_buf());
        let bare = manager.bare_path("acme", "core");
        let branch = "codex/issue-648-live-session";
        publish_branch(upstream.path(), &bare, branch);
        let existing = root
            .path()
            .join("worktrees/github-acme-core")
            .join("issue-648-live-session");
        std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                branch,
                &existing.to_string_lossy(),
                &format!("refs/heads/{branch}"),
            ],
        );
        let existing = std::fs::canonicalize(existing).unwrap();
        let mock = lazybox_server::backend::MockBackend::new();
        let config = ServerConfig::with_store_backend_and_worktree_root(
            Arc::new(MemoryStore::new()),
            Arc::new(mock.clone()),
            root.path().to_path_buf(),
        );

        // Reproduce the real failure: work began from an issue, so its live
        // agent owns a managed checkout. A PR later appears with that exact
        // head branch, but under a different workspace key.
        let issue_task = task(
            "acme/core#648",
            "https://github.com/acme/core/issues/648",
            None,
            vec![],
        );
        let mut issue = lazybox_core::Workspace::from_task(issue_task, chrono::Utc::now());
        let issue_key = issue.key.clone();
        let mut persisted_session = lazybox_core::WorkspaceSession::new(
            issue_key.clone(),
            lazybox_core::SessionKind::Agent {
                agent_id: "codex".into(),
            },
            existing,
            chrono::Utc::now(),
        );
        persisted_session.worktree_branch = Some(branch.into());
        issue.add_session(persisted_session);
        issue.record_sent_snippet("review".into());
        save_workspace(&config, &issue);

        let (mut client, _daemon) = subscribed(config.clone()).await;
        let backend = spawn_and_capture(
            &mut client,
            &mock,
            &issue_key,
            TerminalKind::Agent("codex".into()),
            None,
        )
        .await;
        let issue_ws = load_workspace(&config, &issue_key);
        let source_session = issue_ws.sessions.first().unwrap().clone();
        assert_eq!(source_session.worktree_branch.as_deref(), Some(branch));
        std::fs::write(
            source_session.worktree_path.join("live-session-marker"),
            "preserve me",
        )
        .unwrap();
        let remembered_prompt = lazybox_ipc::UserPrompt {
            text: "do not lose this input".into(),
            timestamp_ms: 42,
            source: lazybox_ipc::PromptSource::Typed,
        };
        config
            .store
            .set_kv(
                &format!("terminal-msgs:{backend}"),
                &serde_json::to_string(&vec![remembered_prompt.clone()]).unwrap(),
            )
            .unwrap();
        config
            .store
            .set_kv(&format!("terminal-draft:{backend}"), "half-typed draft")
            .unwrap();

        // Two PRs claim #647, matching the production ambiguity that routed
        // closing-reference reconciliation elsewhere. Exact managed branch
        // ownership must still put this session on #715.
        let closed_issue_id = lazybox_core::TaskId {
            source: "github".into(),
            key: "acme/core#647".into(),
        };
        let alternate_pr = lazybox_core::Workspace::from_task(
            task(
                "acme/core#712",
                "https://github.com/acme/core/pull/712",
                Some("issue-647-other-pr"),
                vec![closed_issue_id.clone()],
            ),
            chrono::Utc::now(),
        );
        save_workspace(&config, &alternate_pr);
        let pr = lazybox_core::Workspace::from_task(
            task(
                "acme/core#715",
                "https://github.com/acme/core/pull/715",
                Some(branch),
                vec![closed_issue_id],
            ),
            chrono::Utc::now(),
        );
        let pr_key = pr.key.clone();
        save_workspace(&config, &pr);

        send_spawn(
            &mut client,
            &pr_key,
            TerminalKind::Agent("codex".into()),
            None,
        );
        let issue_session_key: lazybox_core::SessionKey = (&issue_key).into();
        let pr_session_key: lazybox_core::SessionKey = (&pr_key).into();
        let rebadged = wait_for(
            &mut client,
            |event| matches!(
                event,
                Event::TerminalsRebadged { from, to }
                    if *from == issue_session_key && *to == pr_session_key
            ),
            Duration::from_secs(30),
        )
        .await;
        assert!(
            rebadged.is_some(),
            "the existing terminal must be rebadged onto the PR; issue={:?}, pr={:?}, backends={:?}",
            load_workspace(&config, &issue_key).sessions,
            load_workspace(&config, &pr_key).sessions,
            mock.list().await.unwrap(),
        );
        assert!(
            wait_for(
                &mut client,
                |event| matches!(event, Event::TerminalFocusRequested { .. }),
                Duration::from_secs(30),
            )
            .await
            .is_some(),
            "the PR spawn must focus the transferred singleton"
        );

        assert!(
            load_workspace(&config, &issue_key).sessions.is_empty(),
            "the obsolete issue badge must no longer own the session"
        );
        let pr_ws = load_workspace(&config, &pr_key);
        assert_eq!(pr_ws.sessions.len(), 1);
        assert_eq!(pr_ws.sessions[0].id, source_session.id);
        assert_eq!(
            pr_ws.sent_snippets,
            vec!["review"],
            "workspace snippet history moves with the live session"
        );
        assert_eq!(
            pr_ws.sessions[0].worktree_path,
            source_session.worktree_path
        );
        assert_eq!(
            std::fs::read_to_string(pr_ws.sessions[0].worktree_path.join("live-session-marker"))
                .unwrap(),
            "preserve me",
            "the checkout and its in-progress files survive the transfer"
        );
        assert_eq!(
            mock.list().await.unwrap(),
            vec![backend],
            "the transfer must not launch a duplicate Codex process"
        );
        let snapshots = lazybox_server::spawn_handler::snapshot_terminals(&config).await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].session_key, pr_session_key);
        assert_eq!(snapshots[0].prompt_history, vec![remembered_prompt]);
        assert_eq!(
            snapshots[0].composing_buffer.as_deref(),
            Some("half-typed draft"),
            "reload snapshot keeps both submitted input history and the draft"
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
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:e2e-restart".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Shell,
                cwd: Some(cwd.path().to_string_lossy().into_owned()),
                initial_prompt: None,
                initial_snippet: None,
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
        // with `sleep` so the recovered screen stays parked while the test
        // inspects it. The tmux relay's close path is input-silent; the
        // prompt-transition matrix in `tmux_restart` pins that contract.
        client
            .send(Command::Write {
                terminal_id,
                bytes: b"for i in $(seq 1 200); do echo line-$i; done; exec sleep 300\n".to_vec(),
                intent: TerminalInputIntent::Submit,
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
    let (mut client, _daemon) = subscribed(config.clone()).await;

    let cwd = tempfile::TempDir::new().unwrap();
    git(cwd.path(), &["init", "-q", "-b", "main"]);
    client
        .send(Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: format!("test:e2e-{agent}").into(),
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Agent(agent.into()),
            cwd: Some(cwd.path().to_string_lossy().into_owned()),
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
        })
        .unwrap();
    let terminal_id = match wait_for(
        &mut client,
        |e| matches!(e, Event::TerminalSpawned { .. }),
        Duration::from_secs(30),
    )
    .await
    {
        Some(Event::TerminalSpawned { terminal_id, .. }) => terminal_id,
        _ => panic!("TerminalSpawned"),
    };
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
    if agent == "claude" {
        assert_real_claude_spawn_env(&config, terminal_id, socket).await;
    }
}

async fn assert_real_claude_spawn_env(
    config: &ServerConfig,
    terminal_id: lazybox_ipc::TerminalId,
    socket: &str,
) {
    let backend_key = config
        .terminal
        .backend_key_for(terminal_id)
        .await
        .expect("Claude backend key");
    let env = std::process::Command::new("tmux")
        .args([
            "-L",
            socket,
            "show-environment",
            "-t",
            &backend_key,
            "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN",
        ])
        .output()
        .expect("tmux show-environment");
    assert!(env.status.success(), "tmux show-environment failed");
    assert_eq!(
        String::from_utf8_lossy(&env.stdout).trim(),
        "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1"
    );
}

async fn assert_real_claude_retains_inline_scrollback(socket: &str, session: &str) {
    let composer_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let capture = tmux_capture(socket, session);
        if capture.contains('❯') {
            break;
        }
        assert!(
            tokio::time::Instant::now() < composer_deadline,
            "Claude composer never appeared; pane tail: {capture:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let open = std::process::Command::new("tmux")
        .args([
            "-L",
            socket,
            "send-keys",
            "-l",
            "-t",
            session,
            "/release-notes",
        ])
        .output()
        .expect("open release notes");
    assert!(open.status.success(), "tmux send-keys failed");
    let submit = std::process::Command::new("tmux")
        .args(["-L", socket, "send-keys", "-t", session, "Enter"])
        .output()
        .expect("submit release notes");
    assert!(submit.status.success(), "tmux send-keys failed");
    let menu_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let capture = tmux_capture(socket, session);
        if capture.contains("Show all") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < menu_deadline,
            "Claude release-notes chooser never appeared; pane tail: {capture:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let select_all = std::process::Command::new("tmux")
        .args(["-L", socket, "send-keys", "-t", session, "Enter"])
        .output()
        .expect("select all release notes");
    assert!(select_all.status.success(), "tmux send-keys failed");

    let history_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let state = std::process::Command::new("tmux")
            .args([
                "-L",
                socket,
                "display-message",
                "-p",
                "-t",
                session,
                "#{alternate_on}\t#{history_size}",
            ])
            .output()
            .expect("tmux pane history state");
        let state = String::from_utf8_lossy(&state.stdout);
        let mut fields = state.trim().split('\t');
        let alternate_on = fields.next() == Some("1");
        let history_size = fields
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        if !alternate_on && history_size > 100 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < history_deadline,
            "real Claude did not retain inline history (state {state:?}); pane tail: {:?}",
            tmux_capture(socket, session)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct TmuxServerGuard(String);

impl Drop for TmuxServerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}

fn tmux_capture(socket: &str, backend_key: &str) -> String {
    let output = std::process::Command::new("tmux")
        .args(["-L", socket, "capture-pane", "-p", "-t", backend_key])
        .output()
        .expect("tmux capture-pane");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
#[ignore = "live agent: needs an installed, authenticated `claude` — LAZYBOX_E2E_LIVE_AGENTS=1 + --run-ignored"]
async fn e2e_real_claude_boots_ready_with_inline_spawn_env() {
    if !live_agent_available("claude") {
        return;
    }
    let socket = format!("lazybox-e2e-claude-{}", std::process::id());
    let _cleanup = TmuxServerGuard(socket.clone());
    let result = timeout(TEST_DEADLINE, live_agent_boots_to_ready("claude", &socket)).await;
    result.expect("deadline");
}

#[tokio::test]
#[ignore = "live agent: needs an installed, authenticated `claude` — LAZYBOX_E2E_LIVE_AGENTS=1 + --run-ignored"]
async fn e2e_real_claude_inline_renderer_retains_tmux_history() {
    if !live_agent_available("claude") {
        return;
    }
    let socket = format!("lazybox-e2e-claude-scroll-{}", std::process::id());
    let _cleanup = TmuxServerGuard(socket.clone());
    let session = "claude-inline";
    let start = std::process::Command::new("tmux")
        .args([
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            "120",
            "-y",
            "32",
            "-e",
            "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1",
            "--",
            "claude",
        ])
        .output()
        .expect("start real Claude in tmux");
    assert!(start.status.success(), "tmux new-session failed");
    timeout(
        TEST_DEADLINE,
        assert_real_claude_retains_inline_scrollback(&socket, session),
    )
    .await
    .expect("deadline");
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
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: "test:e2e-scroll".into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Shell,
                cwd: Some(cwd.path().to_string_lossy().into_owned()),
                initial_prompt: None,
                initial_snippet: None,
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
                intent: TerminalInputIntent::Submit,
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
