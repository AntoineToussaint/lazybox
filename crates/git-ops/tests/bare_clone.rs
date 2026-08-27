//! Tests for the cold bare-clone path — issue #405.
//!
//! The one-time clone must be a *blobless* partial clone
//! (`--filter=blob:none`): commits and trees up front so worktree
//! history operations work, blobs fetched lazily at checkout. And a
//! retry must *resume* an interrupted attempt's `.partial` staging
//! repo instead of deleting it and starting from zero.
//!
//! The manager's fresh-clone URL is always the canonical
//! `git@github.com:` form, so these tests drive the clone through the
//! resume path: a pre-staged `.partial` (exactly what an interrupted
//! attempt leaves behind) whose origin points at a local upstream.

use lazybox_git_ops::{CheckoutPhase, WorktreeManager};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

type PhaseLog = Arc<Mutex<Vec<CheckoutPhase>>>;

fn recording_sink() -> (PhaseLog, Arc<lazybox_git_ops::ProgressSink>) {
    let seen: PhaseLog = Arc::new(Mutex::new(Vec::new()));
    let sink: Arc<lazybox_git_ops::ProgressSink> = {
        let seen = Arc::clone(&seen);
        Arc::new(move |phase: CheckoutPhase| seen.lock().unwrap().push(phase))
    };
    (seen, sink)
}

fn git_cmd(cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg("tag.gpgsign=false")
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR");
    cmd
}

fn git(cwd: &Path, args: &[&str]) {
    let out = git_cmd(cwd, args).output().unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(cwd: &Path, args: &[&str]) -> String {
    let out = git_cmd(cwd, args).output().unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Upstream repo on a deliberately non-`main` default branch (so the
/// clone's HEAD resolution is exercised for real), with filtered
/// fetches allowed, plus a staged `.partial` at the manager's layout
/// pointing origin at the upstream — the on-disk shape an interrupted
/// clone attempt leaves behind.
fn setup_with_partial(owner: &str, repo: &str) -> (TempDir, TempDir, PathBuf, PathBuf) {
    let upstream = TempDir::new().unwrap();
    git(upstream.path(), &["init", "-b", "trunk", "-q"]);
    git(upstream.path(), &["config", "user.email", "t@example.com"]);
    git(upstream.path(), &["config", "user.name", "Tester"]);
    git(
        upstream.path(),
        &["config", "uploadpack.allowFilter", "true"],
    );
    std::fs::write(upstream.path().join("f.txt"), "hello\n").unwrap();
    git(upstream.path(), &["add", "f.txt"]);
    git(upstream.path(), &["commit", "-m", "first", "-q"]);

    let base = TempDir::new().unwrap();
    let bare = base
        .path()
        .join("repos")
        .join(owner)
        .join(format!("{repo}.git"));
    let partial = PathBuf::from(format!("{}.partial", bare.display()));
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    git(
        base.path(),
        &["init", "--quiet", "--bare", &partial.to_string_lossy()],
    );
    // `config remote.origin.url`, NOT `remote add`: `remote add` also
    // writes a remote-tracking refspec, which a pre-#1253 interrupted
    // lazybox attempt never had. Staying refspec-free here keeps these
    // tests honest — the refspec asserted below must come from
    // `ensure_bare_clone` itself, not from fixture setup.
    git(
        &partial,
        &[
            "config",
            "remote.origin.url",
            &upstream.path().to_string_lossy(),
        ],
    );

    (upstream, base, bare, partial)
}

#[tokio::test]
async fn resumed_clone_is_blobless_keeps_progress_and_sets_head() {
    let (upstream, base, bare, partial) = setup_with_partial("acme", "widgets");

    // A marker inside the staging repo proves the retry fetched INTO
    // the existing `.partial` rather than deleting and restarting it.
    std::fs::write(partial.join("resume-marker"), "kept\n").unwrap();

    let (seen, sink) = recording_sink();
    let wm = WorktreeManager::new(base.path().to_path_buf()).with_progress(sink);

    // Triggers `ensure_bare_clone` without a worktree checkout, so the
    // bare clone can be inspected before any blobs are demanded.
    let default = wm
        .default_branch(
            "acme",
            "widgets",
            lazybox_git_ops::LockPriority::Interactive,
        )
        .await
        .expect("default_branch clones via the resumed partial");
    assert_eq!(default, "trunk");

    assert!(bare.exists(), "staging repo renamed into place");
    assert!(!partial.exists(), "no leftover .partial after success");
    assert!(
        bare.join("resume-marker").exists(),
        "the pre-existing partial was resumed, not deleted"
    );

    // HEAD must track the remote's default branch, not git init's
    // local default — a dangling HEAD reads as a broken clone and
    // would get the cache deleted on the next provision.
    assert_eq!(
        git_out(&bare, &["symbolic-ref", "HEAD"]),
        "refs/heads/trunk"
    );

    // Blobless: the filter is recorded for future fetches and the
    // committed file's blob was NOT transferred up front.
    assert_eq!(
        git_out(&bare, &["config", "remote.origin.partialclonefilter"]),
        "blob:none"
    );
    let missing = git_out(
        &bare,
        &["rev-list", "--objects", "--missing=print", "trunk"],
    );
    assert!(
        missing.lines().any(|l| l.starts_with('?')),
        "expected at least one deferred blob, got:\n{missing}"
    );

    // The clone boundary and at least one throttled transfer-progress
    // line were reported.
    let phases = seen.lock().unwrap();
    assert!(
        phases.contains(&CheckoutPhase::Cloning),
        "Cloning phase reported: {phases:?}"
    );
    assert!(
        phases
            .iter()
            .any(|p| matches!(p, CheckoutPhase::CloneProgress(_))),
        "clone progress lines reported: {phases:?}"
    );
    drop(phases);

    drop(upstream);
}

#[tokio::test]
async fn worktree_from_blobless_clone_fetches_blobs_lazily() {
    let (upstream, base, _bare, _partial) = setup_with_partial("acme", "gizmo");

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("wt");
    let wt = wm
        .checkout_new_branch_at(&wt_path, "acme", "gizmo", "feature/x", "trunk")
        .await
        .expect("checkout off a blobless clone");

    // The checkout materialized real file contents (blob fetched on
    // demand) …
    assert_eq!(
        std::fs::read_to_string(wt.path.join("f.txt")).unwrap(),
        "hello\n"
    );
    // … and history-dependent operations agents rely on still work.
    assert_eq!(git_out(&wt.path, &["log", "--format=%s"]), "first");
    let blame = git_out(&wt.path, &["blame", "--porcelain", "f.txt"]);
    assert!(blame.contains("hello"), "blame resolves contents: {blame}");

    drop(upstream);
}

/// #447 end to end: a failed/interrupted provision that leaves a
/// pristine checkout with dangling worktree metadata (here: the bare's
/// `worktrees/` wiped, as a re-clone would) must NOT wedge on "move the
/// directory aside and retry". The next provision reclaims the leftover
/// — auto-pruning the stale registration — and re-provisions cleanly.
#[tokio::test]
async fn interrupted_provision_leftover_is_reclaimed_and_reprovisioned() {
    let (upstream, base, bare, _partial) = setup_with_partial("acme", "rapid");

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("wt");
    let wt = wm
        .checkout_new_branch_at(&wt_path, "acme", "rapid", "feature/x", "trunk")
        .await
        .expect("initial provision succeeds");
    assert_eq!(
        std::fs::read_to_string(wt.path.join("f.txt")).unwrap(),
        "hello\n"
    );

    // Simulate the orphan: the checkout's files survive, but the bare's
    // worktree registration is gone, so its `.git` gitdir dangles —
    // exactly the state a killed `worktree add` / bare re-clone leaves.
    std::fs::remove_dir_all(bare.join("worktrees")).expect("wipe worktree metadata");
    assert!(wt_path.join("f.txt").exists(), "leftover files remain");

    // Same spawn again: instead of refusing, it reclaims and rebuilds.
    let wt2 = wm
        .checkout_new_branch_at(&wt_path, "acme", "rapid", "feature/x", "trunk")
        .await
        .expect("re-provision reclaims the pristine leftover");
    assert_eq!(
        std::fs::read_to_string(wt2.path.join("f.txt")).unwrap(),
        "hello\n",
        "the rebuilt worktree is a real, usable checkout"
    );
    // A genuine worktree, registered and healthy this time.
    assert_eq!(
        git_out(&wt2.path, &["rev-parse", "--is-inside-work-tree"]),
        "true"
    );

    drop(upstream);
}

/// The trade a blobless clone makes: refs tolerate an unreachable
/// origin (stale-base fallback), but the worktree checkout itself
/// needs origin to download file contents. When that fails, the error
/// must say so — git's raw "could not fetch <oid> from promisor
/// remote" names neither the cause nor the remedy.
#[tokio::test]
async fn blobless_checkout_fails_clearly_when_origin_unreachable() {
    let (upstream, base, bare, _partial) = setup_with_partial("acme", "offline");

    let wm = WorktreeManager::new(base.path().to_path_buf());
    // Materialize the blobless bare clone while origin is reachable …
    wm.default_branch(
        "acme",
        "offline",
        lazybox_git_ops::LockPriority::Interactive,
    )
    .await
    .expect("cold clone succeeds");
    assert!(bare.exists());

    // … then lose the origin before the first checkout.
    drop(upstream);

    let err = wm
        .checkout_new_branch_at(
            &base.path().join("wt"),
            "acme",
            "offline",
            "feature/x",
            "trunk",
        )
        .await
        .expect_err("checkout needs origin for blobs");
    let msg = err.to_string();
    assert!(
        msg.contains("blobless clone") && msg.contains("file contents"),
        "error names the real cause: {msg}"
    );
}

/// A resumed fetch that fails against the same origin a fresh attempt
/// would use keeps the staging repo — resume and restart are
/// equivalent there, and the accumulated objects are the whole point
/// of resuming.
#[tokio::test]
async fn failed_resume_keeps_partial_when_origin_matches() {
    let base = TempDir::new().unwrap();
    let bare = base.path().join("repos").join("acme").join("dead.git");
    let partial = PathBuf::from(format!("{}.partial", bare.display()));
    let dead_origin = base.path().join("nowhere").display().to_string();

    // A condemned bare (fails validation) whose surviving config names
    // the dead origin — the URL a from-scratch attempt would reuse.
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::write(
        bare.join("config"),
        format!("[remote \"origin\"]\n\turl = {dead_origin}\n"),
    )
    .unwrap();

    // A staged partial pointing at the same dead origin.
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    git(
        base.path(),
        &["init", "--quiet", "--bare", &partial.to_string_lossy()],
    );
    git(&partial, &["config", "remote.origin.url", &dead_origin]);
    std::fs::write(partial.join("resume-marker"), "kept\n").unwrap();

    let wm = WorktreeManager::new(base.path().to_path_buf());
    wm.default_branch("acme", "dead", lazybox_git_ops::LockPriority::Interactive)
        .await
        .expect_err("fetch from a dead origin fails");

    assert!(
        partial.join("resume-marker").exists(),
        "matching-origin partial survives its failed fetch for the next retry"
    );
}

/// #1253 F1 journey: in a bare clone built by `ensure_bare_clone`
/// itself (not `git clone --bare` + a hand-added refspec), a worktree
/// provisioned by `checkout_at` must have a working `@{u}` — git
/// resolves it by mapping `branch.<x>.merge` through
/// `remote.origin.fetch`, so the clone must carry the standard
/// remote-tracking refspec.
#[tokio::test]
async fn upstream_resolves_in_a_production_shaped_bare_clone() {
    let (upstream, base, bare, _partial) = setup_with_partial("acme", "tracked");
    git(upstream.path(), &["branch", "feature/y"]);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt = wm
        .checkout_at(
            &base.path().join("worktrees").join("sess-y"),
            "acme",
            "tracked",
            "feature/y",
            None,
        )
        .await
        .expect("provision through ensure_bare_clone + checkout_at");

    // The clone carries the standard remote-tracking refspec …
    assert_eq!(
        git_out(&bare, &["config", "--get", "remote.origin.fetch"]),
        "+refs/heads/*:refs/remotes/origin/*"
    );
    // … so the upstream actually resolves from the worktree.
    let out = git_cmd(&wt.path, &["rev-list", "--count", "@{u}..HEAD"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`@{{u}}..HEAD` must resolve in a managed worktree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");

    drop(upstream);
}

/// #1253 F2 journey: provision from an issue (new local branch), commit,
/// push the branch — the worktree must now count as pushed and be
/// deletable, instead of being compared against the DEFAULT branch and
/// stranding forever as "unpushed".
#[tokio::test]
async fn pushed_branch_worktree_is_deletable() {
    let (upstream, base, _bare, _partial) = setup_with_partial("acme", "pushed");
    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("worktrees").join("issue-7");
    let wt = wm
        .checkout_new_branch_at(&wt_path, "acme", "pushed", "lazybox/issue-7", "trunk")
        .await
        .expect("issue-style provision");

    git(&wt.path, &["config", "user.email", "t@example.com"]);
    git(&wt.path, &["config", "user.name", "Tester"]);
    std::fs::write(wt.path.join("fix.txt"), "the fix\n").unwrap();
    git(&wt.path, &["add", "fix.txt"]);
    git(&wt.path, &["commit", "-q", "-m", "fix"]);
    // Plain push, no `-u`. Two traps at once: the branch's auto-set
    // upstream is the BASE branch (`worktree add -B … origin/trunk`
    // shape), so `@{u}..HEAD` counts the whole feature as "unpushed" —
    // the inspector must instead consult the branch's own
    // `refs/remotes/origin/lazybox/issue-7`, which this push updated
    // through the configured refspec.
    git(&wt.path, &["push", "-q", "origin", "lazybox/issue-7"]);

    let report = wm.inspect_worktrees(&[]).await.expect("inspection");
    let row = report
        .iter()
        .find(|r| r.path.ends_with("issue-7"))
        .expect("inspection row for the worktree");
    assert!(
        !row.has_unpushed_commits,
        "fully-pushed branch must not classify as unpushed: {row:?}"
    );
    assert!(
        row.is_safe_to_delete,
        "pushed + untracked must be reapable: {row:?}"
    );
    wm.delete_inspected(row, false)
        .await
        .expect("pushed branch deletes rather than refuses");
    assert!(!wt_path.exists(), "worktree removed");

    drop(upstream);
}

/// #1253 F2 guard: the same journey WITHOUT the push must still refuse
/// — conservatism about genuinely local commits stays.
#[tokio::test]
async fn unpushed_commit_still_refuses() {
    let (upstream, base, _bare, _partial) = setup_with_partial("acme", "unpushed");
    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("worktrees").join("issue-8");
    let wt = wm
        .checkout_new_branch_at(&wt_path, "acme", "unpushed", "lazybox/issue-8", "trunk")
        .await
        .expect("issue-style provision");

    git(&wt.path, &["config", "user.email", "t@example.com"]);
    git(&wt.path, &["config", "user.name", "Tester"]);
    std::fs::write(wt.path.join("fix.txt"), "the fix\n").unwrap();
    git(&wt.path, &["add", "fix.txt"]);
    git(&wt.path, &["commit", "-q", "-m", "fix"]);

    let report = wm.inspect_worktrees(&[]).await.expect("inspection");
    let row = report
        .iter()
        .find(|r| r.path.ends_with("issue-8"))
        .expect("inspection row for the worktree");
    assert!(row.has_unpushed_commits, "local-only commit detected");
    assert!(!row.is_safe_to_delete);
    let err = wm
        .delete_inspected(row, false)
        .await
        .expect_err("unpushed work must refuse deletion");
    assert!(err.to_string().contains("unpushed"), "got: {err}");
    assert!(wt_path.exists(), "worktree preserved");

    drop(upstream);
}

/// #1253 one-time repair + F9: a legacy bare clone (`git clone --bare`
/// — no `remote.origin.fetch`, exactly what pre-fix installs have on
/// disk) is normalized the first time a provision passes
/// `bare_repo_health`, and an upstream default-branch rename repoints
/// the clone's HEAD on the next pass.
#[tokio::test]
async fn legacy_bare_clone_is_repaired_and_head_follows_rename() {
    let upstream = TempDir::new().unwrap();
    git(upstream.path(), &["init", "-b", "trunk", "-q"]);
    git(upstream.path(), &["config", "user.email", "t@example.com"]);
    git(upstream.path(), &["config", "user.name", "Tester"]);
    std::fs::write(upstream.path().join("f.txt"), "hello\n").unwrap();
    git(upstream.path(), &["add", "f.txt"]);
    git(upstream.path(), &["commit", "-m", "first", "-q"]);

    let base = TempDir::new().unwrap();
    let bare = base.path().join("repos").join("acme").join("legacy.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        base.path(),
        &[
            "clone",
            "--bare",
            "-q",
            &upstream.path().to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    );

    let wm = WorktreeManager::new(base.path().to_path_buf());
    // Any provision runs the health gate on the existing clone.
    assert_eq!(
        wm.default_branch("acme", "legacy", lazybox_git_ops::LockPriority::Interactive)
            .await
            .unwrap(),
        "trunk"
    );
    assert_eq!(
        git_out(&bare, &["config", "--get", "remote.origin.fetch"]),
        "+refs/heads/*:refs/remotes/origin/*",
        "health gate wrote the missing refspec back"
    );

    // Upstream renames its default branch.
    git(upstream.path(), &["branch", "-m", "trunk", "main"]);
    assert_eq!(
        wm.default_branch("acme", "legacy", lazybox_git_ops::LockPriority::Interactive)
            .await
            .unwrap(),
        "main",
        "origin/HEAD follows the upstream rename"
    );
    assert_eq!(git_out(&bare, &["symbolic-ref", "HEAD"]), "refs/heads/main");

    drop(upstream);
}

/// A resumed fetch that fails against an *adopted* origin different
/// from the canonical one discards the staging repo: nothing else ever
/// deletes a resumable `.partial`, so keeping it would wedge every
/// retry on the dead adopted remote with no path back to the canonical
/// URL.
#[tokio::test]
async fn failed_resume_with_divergent_origin_discards_partial() {
    let base = TempDir::new().unwrap();
    let bare = base.path().join("repos").join("acme").join("moved.git");
    let partial = PathBuf::from(format!("{}.partial", bare.display()));
    let dead_origin = base.path().join("nowhere").display().to_string();

    // No bare clone: the canonical URL is the github.com form, which
    // differs from the partial's dead local-path origin.
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    git(
        base.path(),
        &["init", "--quiet", "--bare", &partial.to_string_lossy()],
    );
    git(&partial, &["config", "remote.origin.url", &dead_origin]);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    wm.default_branch("acme", "moved", lazybox_git_ops::LockPriority::Interactive)
        .await
        .expect_err("fetch from the adopted dead origin fails");

    assert!(
        !partial.exists(),
        "divergent-origin partial is discarded so the next attempt \
         re-clones from the canonical remote"
    );
}
