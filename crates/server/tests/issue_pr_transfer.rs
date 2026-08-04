//! Issue→PR session-transfer matrix (#404).
//!
//! The single rebadge test in `spawn_handler.rs` covers one narrow
//! shape: a Claude agent on a pre-seeded tempdir worktree, one
//! terminal, manual collapse. This suite parameterizes the dimensions
//! that shape deliberately skipped — agent kind (Codex/Cursor, not
//! just Claude), a worktree provisioned through the real git path
//! (local bare clone, no network), multiple terminals (agent + shell),
//! and the confirm-prompt (`WorkspaceMergePending` → `ConfirmMerge`)
//! transfer next to the manual one — plus the silent-absorb path for
//! dead-but-recoverable session records and the worktree half (#446):
//! a pre-collapse PR stub session retired in favor of the issue's WIP
//! checkout, and a PR session with local work surviving untouched.
//!
//! Every case asserts the #78/#90 invariant end to end:
//!   (a) `TerminalsRebadged issue→PR` is broadcast,
//!   (b) the backend session is never killed,
//!   (c) `terminal_meta` re-keys every live terminal to the PR, and
//!   (d) the session survives a daemon restart — `recover_sessions`
//!       reattaches it to the PR workspace with scrollback intact.

use lazybox_core::{SessionKind, Task, TaskId, Workspace, WorkspaceKey};
use lazybox_ipc::{Command, Event, TerminalKind, channel};
use lazybox_server::backend::SessionBackend;
use lazybox_server::{Server, ServerConfig, polling};
use std::path::Path;
use std::time::Duration;
use tokio::time::timeout;

/// Generous outer bound: provisioned cases run real `git clone` /
/// `git worktree add` subprocesses.
const TEST_DEADLINE: Duration = Duration::from_secs(60);
const EVENT_BUDGET: Duration = Duration::from_secs(10);

/// `LAZYBOX_HOME` is process-global and the provisioned cases resolve
/// their worktree root through it, so the whole suite serializes on
/// one lock — a concurrent test flipping the env var mid-provision
/// would scatter one case's paths across two roots.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct IsolatedConfigHome {
    _guard: std::sync::MutexGuard<'static, ()>,
    _tmp: tempfile::TempDir,
    prev_home: Option<std::ffi::OsString>,
    prev_git_global: Option<std::ffi::OsString>,
}

impl IsolatedConfigHome {
    fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("LAZYBOX_HOME");
        let prev_git_global = std::env::var_os("GIT_CONFIG_GLOBAL");
        // Run every case under `fetch.prune = true` — a common user
        // config CI never has, and exactly what turned the branch
        // refresh into a ref-deleting fetch (#404). Provisioning must
        // hold up under it.
        let git_global = tmp.path().join("gitconfig");
        std::fs::write(
            &git_global,
            "[fetch]\n\tprune = true\n[user]\n\temail = t@example.com\n\tname = Tester\n[commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        // SAFETY: mutation is serialized by ENV_LOCK for this binary;
        // restored on drop.
        unsafe {
            std::env::set_var("LAZYBOX_HOME", tmp.path());
            std::env::set_var("GIT_CONFIG_GLOBAL", &git_global);
        }
        Self {
            _guard: guard,
            _tmp: tmp,
            prev_home,
            prev_git_global,
        }
    }
}

impl Drop for IsolatedConfigHome {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_home {
                Some(v) => std::env::set_var("LAZYBOX_HOME", v),
                None => std::env::remove_var("LAZYBOX_HOME"),
            }
            match &self.prev_git_global {
                Some(v) => std::env::set_var("GIT_CONFIG_GLOBAL", v),
                None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
            }
        }
    }
}

async fn subscribed(config: ServerConfig) -> lazybox_ipc::Client {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        let _ = Server::new(config).serve(server).await;
    });
    client.send(Command::Subscribe).unwrap();
    let _snapshot = client.recv().await.expect("snapshot");
    client
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

fn gh_task(key: &str, is_pr: bool, branch: Option<&str>, closes: Vec<TaskId>) -> Task {
    let (path, num) = key.rsplit_once('#').unwrap();
    let url = if is_pr {
        format!("https://github.com/{path}/pull/{num}")
    } else {
        format!("https://github.com/{path}/issues/{num}")
    };
    Task {
        id: TaskId {
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
        url,
        repo: Some(path.into()),
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
        kind: None,
        closes_issues: closes,
        priority: None,
        state_label: None,
    }
}

fn save_workspace(config: &ServerConfig, ws: &Workspace) {
    config
        .store
        .save_workspace(&lazybox_store::WorkspaceRecord {
            key: ws.key.as_str().to_string(),
            created_at: ws.created_at,
            workspace_json: Some(serde_json::to_string(ws).unwrap()),
        })
        .unwrap();
}

fn load_workspace(config: &ServerConfig, key: &WorkspaceKey) -> Option<Workspace> {
    let record = config.store.get_workspace(key).unwrap()?;
    Some(serde_json::from_str(&record.workspace_json.unwrap()).unwrap())
}

fn git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(cwd)
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Local upstream (one commit on `main`) bare-cloned into the daemon's
/// canonical cache path, so the real provisioning path — bare-clone
/// health check, base-ref fetch, `git worktree add` — runs entirely
/// offline. The returned upstream tempdir must outlive every fetch.
fn seed_local_remote(config: &ServerConfig, owner: &str, repo: &str) -> tempfile::TempDir {
    let upstream = tempfile::TempDir::new().unwrap();
    git(upstream.path(), &["init", "-b", "main", "-q"]);
    git(upstream.path(), &["config", "user.email", "t@example.com"]);
    git(upstream.path(), &["config", "user.name", "Tester"]);
    git(
        upstream.path(),
        &["commit", "--allow-empty", "-m", "first", "-q"],
    );
    let bare = config
        .worktree_root_path()
        .join("repos")
        .join(owner)
        .join(format!("{repo}.git"));
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

/// How the issue's live terminal(s) reach the PR workspace.
enum Trigger {
    /// The user's explicit `x j` — `Command::CollapseIntoPr`.
    Manual,
    /// The poll path: a PR upsert stalls on the live terminal, emits
    /// `WorkspaceMergePending`, and the user accepts via
    /// `Command::ConfirmMerge`.
    Confirm,
}

struct Case {
    agent: &'static str,
    /// Provision the worktree through the real git path (local bare
    /// clone) instead of pre-seeding a tempdir session record.
    provisioned: bool,
    /// Spawn a shell next to the agent so the rebadge must carry
    /// multiple terminals.
    extra_shell: bool,
    trigger: Trigger,
}

async fn spawn_and_capture(
    client: &mut lazybox_ipc::Client,
    issue_key: &WorkspaceKey,
    kind: TerminalKind,
) -> lazybox_ipc::TerminalId {
    client
        .send(Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key: issue_key.as_str().into(),
            session_id: None,
            client_request_id: None,
            kind,
            cwd: None,
            initial_prompt: None,
            on_main: false,
        })
        .unwrap();
    match wait_for(
        client,
        |e| {
            if let Event::ProviderError {
                source, message, ..
            } = e
            {
                panic!("spawn raised a provider error ({source}): {message}");
            }
            matches!(e, Event::TerminalSpawned { .. })
        },
        // Provisioned spawns pay a real (local) git clone + worktree add.
        Duration::from_secs(30),
    )
    .await
    .expect("TerminalSpawned arrived")
    {
        Event::TerminalSpawned { terminal_id, .. } => terminal_id,
        _ => unreachable!(),
    }
}

async fn run_case(case: Case) {
    let _home = IsolatedConfigHome::new();
    let (config, mock) = ServerConfig::in_memory_with_mock();

    let issue_task = gh_task("o/r#50", false, None, vec![]);
    let issue_task_id = issue_task.id.clone();
    let mut issue = Workspace::from_task(issue_task, chrono::Utc::now());
    let issue_key = issue.key.clone();

    let mut _seed_dir = None;
    let mut _upstream = None;
    if case.provisioned {
        _upstream = Some(seed_local_remote(&config, "o", "r"));
    } else {
        // Seeded sessions still sit on a real git checkout — that's
        // what a session's worktree always is in production, and it is
        // what arms the "reuse a live worktree in place, never
        // relocate it" branch of the path migration (#78).
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        issue.add_session(lazybox_core::WorkspaceSession::new(
            issue_key.clone(),
            SessionKind::Agent {
                agent_id: case.agent.into(),
            },
            dir.path().to_path_buf(),
            chrono::Utc::now(),
        ));
        _seed_dir = Some(dir);
    }
    save_workspace(&config, &issue);

    let pr_task = gh_task("o/r#51", true, Some("feat"), vec![issue_task_id]);
    let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr_task));
    if matches!(case.trigger, Trigger::Manual) {
        // The manual collapse resolves its claiming PR from local
        // state; the confirm path creates the PR row via the upsert.
        save_workspace(
            &config,
            &Workspace::from_task(pr_task.clone(), chrono::Utc::now()),
        );
    }

    let mut client = subscribed(config.clone()).await;

    let mut terminal_ids = vec![
        spawn_and_capture(
            &mut client,
            &issue_key,
            TerminalKind::Agent(case.agent.into()),
        )
        .await,
    ];
    if case.extra_shell {
        terminal_ids.push(spawn_and_capture(&mut client, &issue_key, TerminalKind::Shell).await);
    }
    let backend_keys = mock.list().await.unwrap();
    assert_eq!(
        backend_keys.len(),
        terminal_ids.len(),
        "one backend session per spawned terminal",
    );
    for key in &backend_keys {
        mock.emit(key, "scrollback-marker").await;
    }
    let agent_backend_key = config
        .terminal
        .backend_key_for(terminal_ids[0])
        .await
        .expect("agent backend key");
    client
        .send(Command::IngestHook {
            terminal_id: terminal_ids[0],
            backend_key: Some(agent_backend_key),
            hook: lazybox_ipc::HookEvent {
                kind: lazybox_ipc::HookEventKind::SessionStart,
                session_id: Some("provider-session-708".into()),
                cwd: None,
                tool_name: None,
                notification: None,
            },
        })
        .unwrap();
    let issue_before = loop {
        let workspace =
            load_workspace(&config, &issue_key).expect("issue row present pre-collapse");
        if workspace.sessions[0]
            .provider_session_ids
            .get(case.agent)
            .is_some_and(|id| id == "provider-session-708")
        {
            break workspace;
        }
        tokio::task::yield_now().await;
    };
    let session_paths_before: Vec<_> = issue_before
        .sessions
        .iter()
        .map(|s| s.worktree_path.clone())
        .collect();
    assert_eq!(
        session_paths_before.len(),
        1,
        "agent + shell share the workspace's single session",
    );

    match case.trigger {
        Trigger::Manual => {
            client
                .send(Command::CollapseIntoPr {
                    issue_workspace_key: issue_key.as_str().into(),
                })
                .unwrap();
        }
        Trigger::Confirm => {
            // The poll path: the PR arrives claiming the issue while
            // its terminals are live → the daemon must stall and
            // prompt instead of absorbing silently.
            polling::upsert(&config, pr_task.clone()).await;
            let pending = wait_for(
                &mut client,
                |e| matches!(e, Event::WorkspaceMergePending { .. }),
                EVENT_BUDGET,
            )
            .await
            .expect("live terminals must raise WorkspaceMergePending");
            match pending {
                Event::WorkspaceMergePending {
                    issue_workspace_key,
                    pr_workspace_key,
                    active_terminal_count,
                    ..
                } => {
                    assert_eq!(issue_workspace_key, issue_key);
                    assert_eq!(pr_workspace_key, pr_key);
                    assert_eq!(active_terminal_count, terminal_ids.len());
                }
                _ => unreachable!(),
            }
            assert!(
                load_workspace(&config, &issue_key).is_some(),
                "the issue row must survive until the user confirms",
            );
            client
                .send(Command::ConfirmMerge {
                    issue_workspace_key: issue_key.clone(),
                    pr_workspace_key: pr_key.clone(),
                    accept: true,
                })
                .unwrap();
        }
    }

    // (a) the rebadge is broadcast, issue → PR.
    let issue_sk: lazybox_core::SessionKey = (&issue_key).into();
    let pr_sk: lazybox_core::SessionKey = (&pr_key).into();
    let rebadged = wait_for(
        &mut client,
        |e| {
            matches!(
                e,
                Event::TerminalsRebadged { from, to }
                    if *from == issue_sk && *to == pr_sk
            )
        },
        EVENT_BUDGET,
    )
    .await;
    assert!(
        rebadged.is_some(),
        "collapse must broadcast TerminalsRebadged issue→PR",
    );
    let merged = wait_for(
        &mut client,
        |e| {
            matches!(
                e,
                Event::WorkspaceMerged { pr_workspace_key, .. }
                    if pr_workspace_key.as_str() == pr_key.as_str()
            )
        },
        EVENT_BUDGET,
    )
    .await;
    assert!(merged.is_some(), "collapse must broadcast WorkspaceMerged");

    // (b) no backend session was killed.
    let after = mock.list().await.unwrap();
    for key in &backend_keys {
        assert!(
            after.contains(key),
            "backend session {key} must survive the collapse",
        );
    }

    // (c) every live terminal is re-keyed to the PR.
    for tid in &terminal_ids {
        let (sk, _) = config
            .terminal
            .terminal_meta_for(*tid)
            .await
            .expect("terminal still tracked");
        assert_eq!(sk, pr_sk, "terminal_meta must rebadge onto the PR");
    }

    // The store: issue row gone, PR row carries the session at its
    // original (never-relocated) worktree path.
    assert!(
        load_workspace(&config, &issue_key).is_none(),
        "issue row must be deleted after the collapse",
    );
    let pr_ws = load_workspace(&config, &pr_key).expect("PR row present");
    assert_eq!(
        pr_ws.sessions.len(),
        1,
        "the issue's session must live on the PR workspace",
    );
    assert_eq!(pr_ws.sessions[0].workspace_key, pr_key);
    assert_eq!(
        pr_ws.sessions[0]
            .provider_session_ids
            .get(case.agent)
            .map(String::as_str),
        Some("provider-session-708"),
        "exact provider identity must follow the session onto the PR",
    );
    assert_eq!(
        pr_ws.sessions[0].worktree_path, session_paths_before[0],
        "a live worktree must be reused in place, never relocated (#78)",
    );
    if case.provisioned {
        assert!(
            pr_ws.sessions[0].worktree_path.join(".git").exists(),
            "the provisioned worktree must still exist on disk",
        );
    }

    if case.agent == "codex"
        && !case.provisioned
        && !case.extra_shell
        && matches!(case.trigger, Trigger::Manual)
    {
        let old_backend_key = config
            .terminal
            .backend_key_for(terminal_ids[0])
            .await
            .expect("moved agent remains live");
        mock.finish(&old_backend_key, 1).await;
        wait_for(
            &mut client,
            |event| {
                matches!(
                    event,
                    Event::TerminalExited {
                        terminal_id,
                        ..
                    } if *terminal_id == terminal_ids[0]
                )
            },
            EVENT_BUDGET,
        )
        .await
        .expect("moved agent exit");
        client
            .send(Command::ResumeAgent {
                terminal_id: terminal_ids[0],
            })
            .unwrap();
        let resumed = wait_for(
            &mut client,
            |event| {
                matches!(
                    event,
                    Event::TerminalReplaced {
                        old_terminal_id,
                        kind: TerminalKind::Agent(agent),
                        ..
                    } if *old_terminal_id == terminal_ids[0] && agent == "codex"
                )
            },
            EVENT_BUDGET,
        )
        .await
        .expect("moved agent exact resume");
        let Event::TerminalReplaced {
            terminal_id: resumed_id,
            ..
        } = resumed
        else {
            unreachable!()
        };
        assert_eq!(
            config
                .terminal
                .terminal_meta_for(resumed_id)
                .await
                .expect("resumed terminal metadata")
                .0,
            pr_sk,
            "recovery context must follow the terminal onto the PR",
        );
        assert!(
            mock.all_argv().await.iter().any(|argv| argv
                .windows(3)
                .any(|args| args == ["codex", "resume", "provider-session-708"])),
            "the moved session must resume by exact provider identity",
        );
    }

    // (d) a daemon restart reattaches the surviving backend sessions
    // to the PR workspace, scrollback intact.
    drop(client);
    let config2 =
        ServerConfig::with_store_and_backend(config.store.clone(), config.backend.clone());
    lazybox_server::spawn_handler::recover_sessions(&config2).await;
    let recovered = config2.terminal.terminal_metadata().await;
    assert_eq!(
        recovered.len(),
        terminal_ids.len(),
        "every surviving backend session must be recovered",
    );
    for (_, sk, _) in recovered {
        assert_eq!(
            sk, pr_sk,
            "recovery must reattach the session to the PR workspace",
        );
    }
    for key in &backend_keys {
        let sub = config2.backend.subscribe(key).await.unwrap();
        assert!(
            String::from_utf8_lossy(&sub.replay).contains("scrollback-marker"),
            "scrollback must be intact after reattach",
        );
    }
}

#[tokio::test]
async fn codex_seeded_single_manual_collapse_keeps_live_terminal() {
    timeout(
        TEST_DEADLINE,
        run_case(Case {
            agent: "codex",
            provisioned: false,
            extra_shell: false,
            trigger: Trigger::Manual,
        }),
    )
    .await
    .expect("deadline");
}

#[tokio::test]
async fn cursor_seeded_single_confirm_collapse_keeps_live_terminal() {
    timeout(
        TEST_DEADLINE,
        run_case(Case {
            agent: "cursor-agent",
            provisioned: false,
            extra_shell: false,
            trigger: Trigger::Confirm,
        }),
    )
    .await
    .expect("deadline");
}

#[tokio::test]
async fn claude_seeded_multi_terminal_confirm_collapse_keeps_both() {
    timeout(
        TEST_DEADLINE,
        run_case(Case {
            agent: "claude",
            provisioned: false,
            extra_shell: true,
            trigger: Trigger::Confirm,
        }),
    )
    .await
    .expect("deadline");
}

#[tokio::test]
async fn codex_seeded_multi_terminal_manual_collapse_keeps_both() {
    timeout(
        TEST_DEADLINE,
        run_case(Case {
            agent: "codex",
            provisioned: false,
            extra_shell: true,
            trigger: Trigger::Manual,
        }),
    )
    .await
    .expect("deadline");
}

#[tokio::test]
async fn claude_provisioned_single_confirm_collapse_keeps_live_terminal() {
    timeout(
        TEST_DEADLINE,
        run_case(Case {
            agent: "claude",
            provisioned: true,
            extra_shell: false,
            trigger: Trigger::Confirm,
        }),
    )
    .await
    .expect("deadline");
}

#[tokio::test]
async fn codex_provisioned_multi_terminal_manual_collapse_keeps_both() {
    timeout(
        TEST_DEADLINE,
        run_case(Case {
            agent: "codex",
            provisioned: true,
            extra_shell: true,
            trigger: Trigger::Manual,
        }),
    )
    .await
    .expect("deadline");
}

/// Issue and PR resolving to DIFFERENT provisioned worktrees on
/// different branches, each with its own live agent. The collapse must
/// carry the issue's terminal across, leave the PR's own terminal
/// alone, and keep both worktrees in place — the slug-renumbering
/// under the merged workspace must never relocate a live checkout.
#[tokio::test]
async fn collapse_with_distinct_issue_and_pr_worktrees_keeps_both_sessions() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let upstream = seed_local_remote(&config, "o", "r");
        // The PR task tracks a real upstream branch.
        git(upstream.path(), &["branch", "feat"]);
        git(
            &config.worktree_root_path().join("repos/o/r.git"),
            &[
                "fetch",
                "-q",
                "origin",
                "+refs/heads/feat:refs/remotes/origin/feat",
            ],
        );

        let issue_task = gh_task("o/r#50", false, None, vec![]);
        let issue_task_id = issue_task.id.clone();
        let issue = Workspace::from_task(issue_task, chrono::Utc::now());
        let issue_key = issue.key.clone();
        save_workspace(&config, &issue);
        let pr_task = gh_task("o/r#51", true, Some("feat"), vec![issue_task_id]);
        let pr = Workspace::from_task(pr_task, chrono::Utc::now());
        let pr_key = pr.key.clone();
        save_workspace(&config, &pr);

        let mut client = subscribed(config.clone()).await;
        let issue_tid =
            spawn_and_capture(&mut client, &issue_key, TerminalKind::Agent("codex".into())).await;
        let pr_tid =
            spawn_and_capture(&mut client, &pr_key, TerminalKind::Agent("claude".into())).await;
        assert_eq!(mock.list().await.unwrap().len(), 2);

        let paths_before: std::collections::HashMap<_, _> = [&issue_key, &pr_key]
            .into_iter()
            .flat_map(|key| load_workspace(&config, key).unwrap().sessions)
            .map(|s| (s.id, s.worktree_path))
            .collect();
        assert_eq!(paths_before.len(), 2, "one provisioned session per side");
        for path in paths_before.values() {
            assert!(
                path.join(".git").exists(),
                "both sides must be real provisioned worktrees, got {path:?}",
            );
        }

        client
            .send(Command::CollapseIntoPr {
                issue_workspace_key: issue_key.as_str().into(),
            })
            .unwrap();

        let issue_sk: lazybox_core::SessionKey = (&issue_key).into();
        let pr_sk: lazybox_core::SessionKey = (&pr_key).into();
        let rebadged = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::TerminalsRebadged { from, to }
                        if *from == issue_sk && *to == pr_sk
                )
            },
            EVENT_BUDGET,
        )
        .await;
        assert!(
            rebadged.is_some(),
            "collapse must rebadge the issue terminal"
        );
        wait_for(
            &mut client,
            |e| matches!(e, Event::WorkspaceMerged { .. }),
            EVENT_BUDGET,
        )
        .await
        .expect("WorkspaceMerged");

        assert_eq!(
            mock.list().await.unwrap().len(),
            2,
            "neither backend session may be killed by the collapse",
        );
        for tid in [issue_tid, pr_tid] {
            let (sk, _) = config
                .terminal
                .terminal_meta_for(tid)
                .await
                .expect("terminal still tracked");
            assert_eq!(sk, pr_sk);
        }
        let pr_ws = load_workspace(&config, &pr_key).expect("PR row present");
        assert_eq!(
            pr_ws.sessions.len(),
            2,
            "the PR must carry its own session plus the issue's",
        );
        for session in &pr_ws.sessions {
            assert_eq!(session.workspace_key, pr_key);
            assert_eq!(
                Some(&session.worktree_path),
                paths_before.get(&session.id),
                "live worktrees must be reused in place across the merge",
            );
            assert!(session.worktree_path.join(".git").exists());
        }
    })
    .await
    .expect("deadline");
}

/// The silent-absorb path: no live terminal, only a dead-but-
/// recoverable session record on a REAL worktree. The auto-collapse
/// must move the record onto the PR without a prompt, leave the
/// worktree untouched, and a later spawn on the PR must reuse it.
#[tokio::test]
async fn silent_absorb_moves_dead_session_and_real_worktree_to_the_pr() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let _upstream = seed_local_remote(&config, "o", "r");

        let issue_task = gh_task("o/r#50", false, None, vec![]);
        let issue_task_id = issue_task.id.clone();
        let mut issue = Workspace::from_task(issue_task, chrono::Utc::now());
        let issue_key = issue.key.clone();

        // Real worktree of the seeded bare clone — the shape a
        // provisioned session leaves behind after its PTY dies.
        let bare = config.worktree_root_path().join("repos/o/r.git");
        let worktree = tempfile::TempDir::new().unwrap();
        let wt_path = worktree.path().join("issue-50");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-B",
                "issue-50",
                &wt_path.to_string_lossy(),
                "main",
            ],
        );
        issue.add_session(lazybox_core::WorkspaceSession::new(
            issue_key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            wt_path.clone(),
            chrono::Utc::now(),
        ));
        let session_id = issue.sessions[0].id;
        save_workspace(&config, &issue);

        let mut client = subscribed(config.clone()).await;

        // PR claims the issue; no live terminal → silent absorb.
        let pr_task = gh_task("o/r#51", true, Some("feat"), vec![issue_task_id]);
        let pr_key = WorkspaceKey::new(lazybox_core::workspace_key_for(&pr_task));
        polling::upsert(&config, pr_task).await;

        let merged = wait_for(
            &mut client,
            |e| {
                matches!(
                    e,
                    Event::WorkspaceMerged { pr_workspace_key, .. }
                        if pr_workspace_key.as_str() == pr_key.as_str()
                )
            },
            EVENT_BUDGET,
        )
        .await;
        assert!(merged.is_some(), "silent absorb must emit WorkspaceMerged");

        assert!(
            load_workspace(&config, &issue_key).is_none(),
            "the dead-record issue row must merge silently",
        );
        let pr_ws = load_workspace(&config, &pr_key).expect("PR row present");
        let moved = pr_ws
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .expect("the dead session record must move onto the PR");
        assert_eq!(
            moved.worktree_path, wt_path,
            "the real worktree must be reused in place, not relocated",
        );
        assert!(
            wt_path.join(".git").exists(),
            "the worktree must still exist on disk after the absorb",
        );

        // Recoverability: a spawn on the PR must land in the moved
        // session's worktree instead of provisioning a new one.
        client
            .send(Command::Spawn {
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: pr_key.as_str().into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("codex".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            EVENT_BUDGET,
        )
        .await
        .expect("spawn on the PR must succeed after the absorb");
        assert_eq!(mock.list().await.unwrap().len(), 1);
        let pr_after = load_workspace(&config, &pr_key).unwrap();
        assert_eq!(
            pr_after.sessions.len(),
            1,
            "the spawn must reuse the moved session record, not mint a second",
        );
        assert_eq!(pr_after.sessions[0].id, session_id);
    })
    .await
    .expect("deadline");
}

/// The worktree half of the collapse (#446): the PR row minted its own
/// session before the collapse ran (the closes-issues backfill window),
/// so a pristine just-provisioned stub sits at the PR slug while the
/// issue's checkout carries uncommitted WIP. The collapse must retire
/// the stub — record dropped, directory removed — leaving the carried
/// WIP checkout as the PR's only (and therefore default) session, so a
/// later spawn lands in the real checkout, never the empty stub.
#[tokio::test]
async fn collapse_retires_pristine_pr_stub_and_carries_wip_worktree() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let _upstream = seed_local_remote(&config, "o", "r");
        let bare = config.worktree_root_path().join("repos/o/r.git");

        // Issue side: dead session on a real worktree with uncommitted WIP.
        let issue_task = gh_task("o/r#50", false, None, vec![]);
        let issue_task_id = issue_task.id.clone();
        let mut issue = Workspace::from_task(issue_task, chrono::Utc::now());
        let issue_key = issue.key.clone();
        let issue_dir = tempfile::TempDir::new().unwrap();
        let wip_path = issue_dir.path().join("issue-50");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-B",
                "issue-50",
                &wip_path.to_string_lossy(),
                "main",
            ],
        );
        std::fs::write(wip_path.join("WIP.txt"), "uncommitted work\n").unwrap();
        issue.add_session(lazybox_core::WorkspaceSession::new(
            issue_key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            wip_path.clone(),
            chrono::Utc::now(),
        ));
        let issue_session_id = issue.sessions[0].id;
        save_workspace(&config, &issue);

        // PR side: a NEWER dead session on a clean provisioned stub —
        // exactly what a pre-collapse spawn on the fresh PR row leaves
        // behind. Newest `created_at` means it would win
        // `default_session` if it survived.
        let pr_task = gh_task("o/r#51", true, Some("feat"), vec![issue_task_id]);
        let mut pr = Workspace::from_task(pr_task, chrono::Utc::now());
        let pr_key = pr.key.clone();
        let stub_dir = tempfile::TempDir::new().unwrap();
        let stub_path = stub_dir.path().join("PR-51-t");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-B",
                "feat",
                &stub_path.to_string_lossy(),
                "main",
            ],
        );
        pr.add_session(lazybox_core::WorkspaceSession::new(
            pr_key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            stub_path.clone(),
            chrono::Utc::now(),
        ));
        save_workspace(&config, &pr);

        let mut client = subscribed(config.clone()).await;
        client
            .send(Command::CollapseIntoPr {
                issue_workspace_key: issue_key.as_str().into(),
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| matches!(e, Event::WorkspaceMerged { .. }),
            EVENT_BUDGET,
        )
        .await
        .expect("WorkspaceMerged");

        assert!(load_workspace(&config, &issue_key).is_none());
        let pr_ws = load_workspace(&config, &pr_key).expect("PR row present");
        assert_eq!(
            pr_ws.sessions.len(),
            1,
            "the pristine stub session must be retired by the collapse",
        );
        assert_eq!(pr_ws.sessions[0].id, issue_session_id);
        assert_eq!(
            pr_ws.sessions[0].worktree_path, wip_path,
            "the PR's worktree must BE the issue's original checkout",
        );
        assert!(
            wip_path.join("WIP.txt").exists(),
            "uncommitted WIP must survive the transfer",
        );
        assert!(
            !stub_path.exists(),
            "the empty stub worktree must be removed, not left as an orphan sibling",
        );

        // A spawn on the collapsed PR lands in the carried checkout.
        client
            .send(Command::Spawn {
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
                session_key: pr_key.as_str().into(),
                session_id: None,
                client_request_id: None,
                kind: TerminalKind::Agent("codex".into()),
                cwd: None,
                initial_prompt: None,
                on_main: false,
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalSpawned { .. }),
            EVENT_BUDGET,
        )
        .await
        .expect("spawn on the collapsed PR");
        assert_eq!(mock.list().await.unwrap().len(), 1);
        let pr_after = load_workspace(&config, &pr_key).unwrap();
        assert_eq!(pr_after.sessions.len(), 1);
        assert_eq!(
            pr_after.sessions[0].worktree_path, wip_path,
            "the agent must open in the real checkout with the WIP",
        );
    })
    .await
    .expect("deadline");
}

/// A PR-side session whose worktree holds local work is NOT a stub:
/// the collapse must keep both it and the carried issue session.
#[tokio::test]
async fn collapse_keeps_pr_session_with_uncommitted_work() {
    timeout(TEST_DEADLINE, async {
        let _home = IsolatedConfigHome::new();
        let (config, _mock) = ServerConfig::in_memory_with_mock();
        let _upstream = seed_local_remote(&config, "o", "r");
        let bare = config.worktree_root_path().join("repos/o/r.git");

        let issue_task = gh_task("o/r#50", false, None, vec![]);
        let issue_task_id = issue_task.id.clone();
        let mut issue = Workspace::from_task(issue_task, chrono::Utc::now());
        let issue_key = issue.key.clone();
        let issue_dir = tempfile::TempDir::new().unwrap();
        let issue_wt = issue_dir.path().join("issue-50");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-B",
                "issue-50",
                &issue_wt.to_string_lossy(),
                "main",
            ],
        );
        issue.add_session(lazybox_core::WorkspaceSession::new(
            issue_key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            issue_wt.clone(),
            chrono::Utc::now(),
        ));
        save_workspace(&config, &issue);

        let pr_task = gh_task("o/r#51", true, Some("feat"), vec![issue_task_id]);
        let mut pr = Workspace::from_task(pr_task, chrono::Utc::now());
        let pr_key = pr.key.clone();
        let pr_dir = tempfile::TempDir::new().unwrap();
        let pr_wt = pr_dir.path().join("PR-51-t");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-B",
                "feat",
                &pr_wt.to_string_lossy(),
                "main",
            ],
        );
        std::fs::write(pr_wt.join("local.txt"), "local work\n").unwrap();
        pr.add_session(lazybox_core::WorkspaceSession::new(
            pr_key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            pr_wt.clone(),
            chrono::Utc::now(),
        ));
        save_workspace(&config, &pr);

        let mut client = subscribed(config.clone()).await;
        client
            .send(Command::CollapseIntoPr {
                issue_workspace_key: issue_key.as_str().into(),
            })
            .unwrap();
        wait_for(
            &mut client,
            |e| matches!(e, Event::WorkspaceMerged { .. }),
            EVENT_BUDGET,
        )
        .await
        .expect("WorkspaceMerged");

        let pr_ws = load_workspace(&config, &pr_key).expect("PR row present");
        assert_eq!(
            pr_ws.sessions.len(),
            2,
            "a PR session with local work must survive the collapse",
        );
        assert!(
            pr_wt.join("local.txt").exists(),
            "local work in the PR worktree must be untouched",
        );
        assert!(issue_wt.join(".git").exists());
    })
    .await
    .expect("deadline");
}
