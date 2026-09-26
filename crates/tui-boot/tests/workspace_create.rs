#![cfg(unix)]

mod common;

use lazybox_ipc::{Command, Event, socket, transport};
use std::process::Stdio;
use std::time::Duration;

/// Prepare a throwaway git checkout whose `origin` remote points at
/// `owner/repo`, so `lazybox workspace create` can infer the project from it.
fn init_checkout(dir: &std::path::Path, origin: &str) {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", origin]);
}

/// A minimal daemon speaking the real client protocol: handshake, reply to
/// `Subscribe` with an (empty) snapshot, then acknowledge the `CreateWorkspace`
/// by broadcasting a `WorkspaceUpserted` stamped with `assigned_key` — which
/// deliberately differs from the name's slug so the test proves the CLI reports
/// the daemon's key, not one it guessed. Returns the received `CreateWorkspace`
/// command for assertions.
fn fake_daemon(
    listener: transport::Listener,
    assigned_key: &str,
) -> tokio::task::JoinHandle<Command> {
    let assigned_key = assigned_key.to_string();
    tokio::spawn(async move {
        let (mut rd, mut wr) = listener.accept().await.expect("accept client");
        socket::server_handshake(&mut rd, &mut wr)
            .await
            .expect("handshake");
        loop {
            let cmd = socket::read_frame::<_, Command>(&mut rd)
                .await
                .expect("read command")
                .expect("a command");
            match cmd {
                Command::Subscribe => {
                    socket::write_frame(
                        &mut wr,
                        &Event::Snapshot {
                            workspaces: vec![],
                            terminals: vec![],
                            projects: vec![],
                            recent_snippets: vec![],
                            dismissed_updates: vec![],
                        },
                    )
                    .await
                    .expect("send snapshot");
                }
                Command::CreateWorkspace {
                    ref name,
                    ref project_key,
                    ref client_request_id,
                    ..
                } => {
                    let mut ws = lazybox_core::Workspace::empty(
                        lazybox_core::WorkspaceKey::new(assigned_key.clone()),
                        "main",
                        chrono::Utc::now(),
                    );
                    ws.name = name.clone();
                    ws.project_key = Some(project_key.clone());
                    ws.local = true;
                    socket::write_frame(
                        &mut wr,
                        &Event::WorkspaceUpserted(std::sync::Arc::new(ws)),
                    )
                    .await
                    .expect("send upsert");
                    let client_request_id = client_request_id
                        .clone()
                        .expect("workspace create is correlated");
                    socket::write_frame(
                        &mut wr,
                        &Event::WorkspaceCreated {
                            client_request_id: client_request_id.clone(),
                            workspace_key: lazybox_core::WorkspaceKey::new(assigned_key.clone()),
                        },
                    )
                    .await
                    .expect("send create acknowledgement");
                    socket::write_frame(&mut wr, &Event::CommandCompleted { client_request_id })
                        .await
                        .expect("send create completion");
                    return cmd;
                }
                other => panic!("unexpected command {other:?}"),
            }
        }
    })
}

/// Run `lazybox workspace create <extra…>` and capture its output.
fn run_workspace_create(
    binary: &str,
    extra: &[&str],
    home: std::path::PathBuf,
) -> tokio::task::JoinHandle<std::process::Output> {
    let mut argv = vec!["create"];
    argv.extend_from_slice(extra);
    run_workspace(binary, &argv, home)
}

/// Run `lazybox workspace <argv…>` — the verb included, so a test can reach
/// the verb dispatch itself and not only `create`.
fn run_workspace(
    binary: &str,
    argv: &[&str],
    home: std::path::PathBuf,
) -> tokio::task::JoinHandle<std::process::Output> {
    let binary = binary.to_string();
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .arg("workspace")
            .args(&argv)
            .env("LAZYBOX_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("run workspace create")
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_infers_project_from_cwd_and_reports_the_daemon_key() {
    let temp = tempfile::tempdir().expect("tempdir");
    let checkout = temp.path().join("acme-widget");
    std::fs::create_dir(&checkout).expect("checkout dir");
    init_checkout(&checkout, "git@github.com:acme/widget.git");

    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    // The daemon hands back a collision-suffixed key the CLI can't predict.
    let server = fake_daemon(listener, "flaky-test-investigation-2");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--name",
            "flaky-test investigation",
            "--agent",
            "claude",
            "--cwd",
            &checkout.to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(output.status.success(), "workspace create exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Reports the daemon's actual key, and says "Created" (confirmed), not
    // the "Requested" fallback.
    assert!(
        stdout.contains("flaky-test-investigation-2") && stdout.contains("Created"),
        "stdout should confirm the daemon's key, got: {stdout:?}"
    );

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace {
            name,
            project_key,
            spawn_agent,
            client_request_id,
            anchor,
            scratch,
        } => {
            assert_eq!(name, "flaky-test investigation");
            assert_eq!(
                project_key,
                lazybox_core::ProjectKey::github("acme", "widget")
            );
            assert_eq!(spawn_agent.as_deref(), Some("claude"));
            assert!(client_request_id.is_some());
            assert_eq!(anchor, None);
            // The CLI does not decide the rule — the daemon owns the
            // attach-or-refuse gate, so a bare `--name` reaches it
            // undeclared and comes back refused (#1586).
            assert!(!scratch);
        }
        other => panic!("expected CreateWorkspace, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_uses_explicit_project_without_a_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "scratch");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    // A non-git --cwd proves resolution came from --project, not inference.
    let child = run_workspace_create(
        binary,
        &[
            "--name",
            "  scratch  ",
            "--scratch",
            "--project",
            "local-sandbox",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(output.status.success(), "workspace create exited non-zero");

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace {
            name,
            project_key,
            spawn_agent,
            client_request_id,
            anchor,
            scratch,
        } => {
            assert_eq!(name, "scratch");
            assert_eq!(project_key, lazybox_core::ProjectKey::new("local-sandbox"));
            assert_eq!(spawn_agent, None);
            assert!(client_request_id.is_some());
            assert_eq!(anchor, None);
            assert!(scratch);
        }
        other => panic!("expected CreateWorkspace, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_rejects_an_unknown_agent_without_connecting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    // A bad --agent must be caught before any connection: this daemon should
    // never accept a client.
    let server = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--name",
            "scratch",
            "--project",
            "local-sandbox",
            "--agent",
            "totally-not-a-real-agent",
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(
        !output.status.success(),
        "unknown --agent must fail the command"
    );
    // `init_tracing` redirects stderr into the log file, so the reason has to
    // come out on stdout or the caller sees a silent no-op and believes the
    // spawn worked.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("unknown --agent") && stdout.contains("known agents"),
        "the refusal must name the bad agent and the real ones on stdout, got: {stdout:?}"
    );

    // The CLI bailed at validation, so the accept never completes.
    let connected = tokio::time::timeout(Duration::from_millis(500), server).await;
    assert!(
        connected.is_err(),
        "unknown --agent must not connect to the daemon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_refuses_a_flag_it_does_not_know() {
    // A flag this verb does not define used to be dropped in silence: the
    // command attached, printed its success line, and ignored what you asked
    // for. `--tier xhigh` reads exactly like a spawn that honored the tier,
    // which is how an agent ends up running on the wrong model believing
    // otherwise. Refuse it, and point at the mechanism that does work.
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "github-acme-widget-7");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--issue",
            "https://github.com/acme/widget/issues/7",
            "--repo",
            "acme/widget",
            "--agent",
            "claude",
            "--tier",
            "xhigh",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(
        !output.status.success(),
        "an unknown flag must fail the command rather than be ignored"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--tier") && stdout.contains("model:<token>"),
        "the refusal must name the offending flag and the label that does work, got: {stdout:?}"
    );

    // Refused at parse time, so the daemon is never dialled.
    let connected = tokio::time::timeout(Duration::from_millis(500), server).await;
    assert!(
        connected.is_err(),
        "an unknown flag must not reach the daemon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_refuses_a_launch_flag_and_leaves_the_state_db_alone() {
    // `--fresh` wipes `<LAZYBOX_HOME>/v2/state.db` — every session, read
    // mark, snooze and cost row. It configures a *launch*, but it used to be
    // peeled off the whole argv before the subcommand match, so this verb
    // never saw it: the DB was deleted and the command carried on to create
    // the workspace and report success. The unknown-argument guard could not
    // catch what was already gone.
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    std::fs::create_dir_all(home.join("v2")).expect("state dir");
    let state_db = home.join("v2").join("state.db");
    std::fs::write(&state_db, b"not a real db, but it must survive").expect("seed state.db");

    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "github-acme-widget-7");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--issue",
            "https://github.com/acme/widget/issues/7",
            "--repo",
            "acme/widget",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
            "--fresh",
        ],
        home.clone(),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(
        !output.status.success(),
        "a launch flag is not a `workspace create` flag and must fail the command"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--fresh"),
        "the refusal must name `--fresh`, got: {stdout:?}"
    );
    assert!(
        state_db.exists(),
        "a refused command must not have wiped the state DB at {}",
        state_db.display()
    );

    let connected = tokio::time::timeout(Duration::from_millis(500), server).await;
    assert!(
        connected.is_err(),
        "a refused command must not reach the daemon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_refuses_a_known_flag_with_no_value() {
    // A trailing `--agent` was consumed by the parser and reported as "no
    // agent asked for": the workspace was created, no agent spawned, and the
    // success line looked identical to a spawn that worked. Same shape as the
    // `--tier` silence this verb already refuses, one layer down.
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "github-acme-widget-7");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--issue",
            "https://github.com/acme/widget/issues/7",
            "--repo",
            "acme/widget",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
            "--agent",
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(
        !output.status.success(),
        "a flag with no value must fail rather than fall back to a default"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--agent") && stdout.contains("needs a value"),
        "the refusal must name the valueless flag, got: {stdout:?}"
    );

    let connected = tokio::time::timeout(Duration::from_millis(500), server).await;
    assert!(
        connected.is_err(),
        "a refused command must not reach the daemon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_names_a_misspelled_record_flag() {
    // The unknown-argument check used to run *after* the record/name checks,
    // so the likeliest typo of all — a misspelled `--issue` — reported
    // "needs a tracker record" and never mentioned the thing that was
    // actually wrong with the command.
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "github-acme-widget-7");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--issu",
            "https://github.com/acme/widget/issues/7",
            "--repo",
            "acme/widget",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(!output.status.success(), "a misspelled flag must fail");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--issu\""),
        "the refusal must quote the misspelled flag itself, got: {stdout:?}"
    );

    let connected = tokio::time::timeout(Duration::from_millis(500), server).await;
    assert!(
        connected.is_err(),
        "a refused command must not reach the daemon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_ignores_an_empty_argument() {
    // A wrapper interpolating a quoted-but-unset `"$EXTRA"` hands us one
    // empty argv element. It carries no instruction to honor or ignore, so
    // refusing it would break working scripts — and the refusal it produced
    // named nothing at all ("unknown workspace create argument(s): ;").
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "github-acme-widget-7");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--issue",
            "https://github.com/acme/widget/issues/7",
            "--repo",
            "acme/widget",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
            "",
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "an empty argument must not fail the command, got: {stdout:?}"
    );
    assert!(
        stdout.contains("Attached to github:acme/widget#7"),
        "stdout should report the attach, got: {stdout:?}"
    );

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    assert!(matches!(command, Command::CreateWorkspace { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_verb_errors_reach_the_caller_on_stdout() {
    // `init_tracing` has already redirected stderr into the log file by the
    // time the verb dispatch runs, so this `bail!` went nowhere: a mistyped
    // verb, and a bare `lazybox workspace`, both exited 1 in total silence —
    // the same defect `create` was fixed for, one frame up.
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = env!("CARGO_BIN_EXE_lazybox");

    for argv in [vec!["craete", "--issue", "acme/widget#7"], vec![]] {
        let child = run_workspace(binary, &argv, temp.path().join("home"));
        let output = tokio::time::timeout(Duration::from_secs(10), child)
            .await
            .expect("cli exits")
            .expect("cli task");
        assert!(
            !output.status.success(),
            "`lazybox workspace {argv:?}` must fail"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("unknown `lazybox workspace` verb")
                && stdout.contains("lazybox workspace create"),
            "`lazybox workspace {argv:?}` must say why on stdout, got: {stdout:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_issue_anchors_the_command_on_the_record() {
    // #1586: `--issue` sends the record, not a name — the daemon then returns
    // the workspace that record already has rather than minting one beside it.
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "github-acme-widget-7");

    let binary = env!("CARGO_BIN_EXE_lazybox");
    let child = run_workspace_create(
        binary,
        &[
            "--issue",
            "https://github.com/acme/widget/issues/7",
            "--repo",
            "acme/widget",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(output.status.success(), "workspace create exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Attached to github:acme/widget#7")
            && stdout.contains("github-acme-widget-7"),
        "stdout should report the attach and the daemon's key, got: {stdout:?}"
    );

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace { anchor, name, .. } => {
            assert_eq!(
                anchor,
                Some(lazybox_core::TaskId {
                    source: "github".into(),
                    key: "acme/widget#7".into(),
                })
            );
            assert!(
                name.is_empty(),
                "the record supplies the name, got {name:?}"
            );
        }
        other => panic!("expected CreateWorkspace, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_resolves_a_bare_number_against_repo() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = fake_daemon(listener, "github-acme-widget-7");

    let child = run_workspace_create(
        env!("CARGO_BIN_EXE_lazybox"),
        &[
            "--pr",
            "#7",
            "--repo",
            "acme/widget",
            "--cwd",
            &temp.path().to_string_lossy(),
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );

    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(output.status.success(), "workspace create exited non-zero");

    let command = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cli sends command")
        .expect("server task");
    match command {
        Command::CreateWorkspace { anchor, .. } => assert_eq!(
            anchor,
            Some(lazybox_core::TaskId {
                source: "github".into(),
                key: "acme/widget#7".into(),
            })
        ),
        other => panic!("expected CreateWorkspace, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_rejects_an_unreadable_record_without_connecting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let listener = transport::Listener::bind(&socket_path)
        .await
        .expect("bind test socket");
    let server = tokio::spawn(async move { listener.accept().await.map(|_| ()) });

    let child = run_workspace_create(
        env!("CARGO_BIN_EXE_lazybox"),
        &[
            "--issue",
            "some workspace name",
            "--project",
            "local-sandbox",
            "--socket",
            &socket_path.to_string_lossy(),
        ],
        temp.path().join("home"),
    );
    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .expect("cli exits")
        .expect("cli task");
    assert!(
        !output.status.success(),
        "prose is not a tracker record and must fail the command"
    );
    let connected = tokio::time::timeout(Duration::from_millis(500), server).await;
    assert!(
        connected.is_err(),
        "an unreadable record must not connect to the daemon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_create_needs_a_record_or_a_name() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("daemon.sock");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        run_workspace_create(
            env!("CARGO_BIN_EXE_lazybox"),
            &[
                "--project",
                "local-sandbox",
                "--socket",
                &socket_path.to_string_lossy(),
            ],
            temp.path().join("home"),
        ),
    )
    .await
    .expect("cli exits")
    .expect("cli task");
    assert!(!output.status.success(), "a target-less create must fail");
}
