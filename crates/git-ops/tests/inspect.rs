//! Tests for `WorktreeManager::inspect_worktrees` + `delete_inspected`.
//!
//! These tests build a small but real lazybox-style layout under a
//! tempdir:
//!
//! ```text
//! <base>/
//!   repos/<owner>/<repo>.git       (bare clone of a fixture upstream)
//!   worktrees/<slug>/              (`git worktree add` checkout)
//! ```
//!
//! Then assert that the inspector classifies each row correctly and
//! that `delete_inspected` honors the safety rules.
//!
//! Per project test-timeout convention (`feedback_test_timeouts.md`):
//! these tests fork `git` subprocesses but every invocation lives
//! inside `WorktreeManager` paths that are already bounded by the
//! 30s per-call timeout in `lib.rs`. No additional wrapper required.

use lazybox_git_ops::{OrphanReason, TrackedSession, WorktreeManager};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Build a lazybox-shaped tempdir: an "upstream" bare repo plus a fresh
/// lazybox-style bare clone of it under `<base>/repos/o/r.git`. Returns
/// the base dir handle (drops at scope end → tempdir cleanup) along
/// with the path to the bare clone the manager will operate on.
struct Fixture {
    base: TempDir,
    /// Holding the upstream tempdir so it lives as long as the bare
    /// clone needs to fetch from it.
    _upstream: TempDir,
    bare: PathBuf,
    upstream_path: PathBuf,
}

async fn setup_fixture() -> Fixture {
    let upstream = TempDir::new().unwrap();
    // Make the upstream a real repo with one commit on `main`.
    run(upstream.path(), &["init", "-q", "-b", "main"]).await;
    run(upstream.path(), &["config", "user.email", "t@e.st"]).await;
    run(upstream.path(), &["config", "user.name", "tester"]).await;
    std::fs::write(upstream.path().join("README.md"), "hi\n").unwrap();
    run(upstream.path(), &["add", "."]).await;
    run(upstream.path(), &["commit", "-q", "-m", "init"]).await;

    let base = TempDir::new().unwrap();
    let bare = base.path().join("repos").join("o").join("r.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    run(
        base.path(),
        &[
            "clone",
            "--bare",
            "-q",
            &upstream.path().to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    )
    .await;
    // Standard remote-tracking refspec — a bare clone defaults to
    // `+refs/heads/*:refs/heads/*` (storing upstream branches as if
    // they were local), which breaks `@{u}` lookups from worktrees.
    // Lazybox's bare clones use the standard refspec because the
    // production checkout path explicitly fetches into
    // `refs/remotes/origin/*`; the inspector tests do the same so
    // they match the real shape.
    run(
        &bare,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    )
    .await;

    Fixture {
        base,
        upstream_path: upstream.path().to_path_buf(),
        _upstream: upstream,
        bare,
    }
}

async fn run(cwd: &Path, args: &[&str]) {
    let out = tokio::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn run_capture(cwd: &Path, args: &[&str]) -> String {
    let out = tokio::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).unwrap()
}

fn mgr(fx: &Fixture) -> WorktreeManager {
    WorktreeManager::new(fx.base.path().to_path_buf())
}

/// Add a worktree the way lazybox does at runtime: ensure the bare has
/// a `refs/remotes/origin/<branch>` remote-tracking ref, then
/// `git worktree add -B <branch> <path> refs/remotes/origin/<branch>`.
/// This is what `WorktreeManager::checkout_at` actually executes —
/// using the same flow here means the inspector tests exercise the
/// shapes a production install will produce.
async fn add_wt(fx: &Fixture, name: &str, branch: &str, upstream_branch: &str) -> PathBuf {
    // Make sure the upstream actually has this branch — needed so
    // the fetch refspec resolves.
    let upstream_has = run_capture(
        &fx.upstream_path,
        &["rev-parse", "--verify", "--quiet", upstream_branch],
    )
    .await;
    if upstream_has.trim().is_empty() {
        run(&fx.upstream_path, &["branch", upstream_branch]).await;
    }
    // Populate `refs/remotes/origin/<branch>` in the bare. Mirrors
    // the production fetch in `WorktreeManager::checkout_at`.
    run(
        &fx.bare,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("+{upstream_branch}:refs/remotes/origin/{branch}"),
        ],
    )
    .await;

    let wt = fx.base.path().join("worktrees").join(name);
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    run(
        &fx.bare,
        &[
            "worktree",
            "add",
            "-q",
            "-B",
            branch,
            &wt.to_string_lossy(),
            &format!("refs/remotes/origin/{branch}"),
        ],
    )
    .await;
    // Production `checkout_at` records tracking config when the
    // worktree branches off a remote-tracking ref (so `@{u}` resolves
    // and the inspector can tell once-pushed branches from
    // never-pushed local ones). Mirror it here.
    run(
        &fx.bare,
        &["config", &format!("branch.{branch}.remote"), "origin"],
    )
    .await;
    run(
        &fx.bare,
        &[
            "config",
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
        ],
    )
    .await;
    run(&wt, &["config", "user.email", "t@e.st"]).await;
    run(&wt, &["config", "user.name", "tester"]).await;
    wt
}

/// Add a worktree on a brand-new local branch that never existed
/// upstream — the shape `checkout_new_branch_at` produces for
/// issue-spawn sessions (`lazybox/issue-N`). No remote-tracking ref,
/// no upstream config.
async fn add_local_only_wt(fx: &Fixture, name: &str, branch: &str) -> PathBuf {
    let wt = fx.base.path().join("worktrees").join(name);
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    run(
        &fx.bare,
        &[
            "worktree",
            "add",
            "-q",
            "-B",
            branch,
            &wt.to_string_lossy(),
            "refs/heads/main",
        ],
    )
    .await;
    run(&wt, &["config", "user.email", "t@e.st"]).await;
    run(&wt, &["config", "user.name", "tester"]).await;
    wt
}

/// Healthy active session: no orphan reasons, not safe-to-delete.
#[tokio::test]
async fn tracked_active_worktree_has_no_reasons() {
    let fx = setup_fixture().await;
    let wt = add_wt(&fx, "feat", "feat", "main").await;
    let tracked = [TrackedSession {
        session_id: "s1".into(),
        worktree_path: wt.clone(),
        is_stopped: false,
    }];

    let report = mgr(&fx).inspect_worktrees(&tracked).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("feat"))
        .expect("feat row");
    assert!(row.reasons.is_empty(), "got reasons: {:?}", row.reasons);
    assert_eq!(row.session_id.as_deref(), Some("s1"));
    assert!(
        !row.is_safe_to_delete,
        "healthy rows are never bulk-deletable"
    );
}

/// On-disk dir with no tracked session → Untracked + safe to delete.
#[tokio::test]
async fn untracked_worktree_is_flagged_and_safe() {
    let fx = setup_fixture().await;
    let wt = add_wt(&fx, "ghost", "ghost", "main").await;

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("ghost"))
        .expect("ghost row");
    assert!(row.reasons.contains(&OrphanReason::Untracked));
    assert!(row.session_id.is_none());
    assert!(row.is_safe_to_delete);
    assert_eq!(row.branch.as_deref(), Some("ghost"));
    // Size walk hit something — at minimum the .git pointer + README.
    assert!(row.size_bytes > 0);
    assert!(row.last_modified.is_some());
    let _ = wt;
}

/// Tracked but session is stopped → SessionStopped reason.
#[tokio::test]
async fn stopped_session_is_flagged() {
    let fx = setup_fixture().await;
    let wt = add_wt(&fx, "old", "old", "main").await;
    let tracked = [TrackedSession {
        session_id: "s-old".into(),
        worktree_path: wt.clone(),
        is_stopped: true,
    }];

    let report = mgr(&fx).inspect_worktrees(&tracked).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("old"))
        .expect("old row");
    assert!(row.reasons.contains(&OrphanReason::SessionStopped));
    assert!(row.is_safe_to_delete);
}

/// Uncommitted changes block "safe to delete" classification and the
/// non-force delete path.
#[tokio::test]
async fn uncommitted_changes_block_safe_delete() {
    let fx = setup_fixture().await;
    let wt = add_wt(&fx, "dirty", "dirty", "main").await;
    std::fs::write(wt.join("scratch.txt"), "wip").unwrap();

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("dirty"))
        .expect("row")
        .clone();
    assert!(row.has_uncommitted_changes);
    assert!(!row.is_safe_to_delete);

    // Non-force delete refuses.
    let err = mgr(&fx)
        .delete_inspected(&row, /*force=*/ false)
        .await
        .expect_err("must refuse");
    assert!(err.to_string().contains("uncommitted"), "got: {err}");
    assert!(wt.exists(), "dir untouched on refusal");

    // Force delete proceeds.
    mgr(&fx)
        .delete_inspected(&row, /*force=*/ true)
        .await
        .expect("force delete");
    assert!(!wt.exists(), "dir removed");
}

/// Unpushed commits (HEAD ahead of upstream) block safe-delete.
#[tokio::test]
async fn unpushed_commits_block_safe_delete() {
    let fx = setup_fixture().await;
    // Set up an upstream branch + a worktree off it, configure
    // tracking, then commit ahead of upstream.
    run(&fx.upstream_path, &["branch", "ahead-base"]).await;
    let wt = add_wt(&fx, "ahead", "ahead-base", "ahead-base").await;
    // Set upstream directly via config — `branch --set-upstream-to`
    // refuses with "starting point is not a branch" when the ref is
    // remote-tracking-only, which is exactly the shape lazybox worktrees
    // have (no local branch, just refs/remotes/origin/<name>).
    // `branch.*` config lives on the *bare* clone (shared across all
    // worktrees), so write there directly.
    run(&fx.bare, &["config", "branch.ahead-base.remote", "origin"]).await;
    run(
        &fx.bare,
        &["config", "branch.ahead-base.merge", "refs/heads/ahead-base"],
    )
    .await;
    std::fs::write(wt.join("ahead.txt"), "ahead").unwrap();
    run(&wt, &["add", "."]).await;
    run(&wt, &["commit", "-q", "-m", "ahead"]).await;

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("ahead"))
        .expect("row")
        .clone();
    assert!(row.has_unpushed_commits);
    assert!(!row.is_safe_to_delete);

    let err = mgr(&fx)
        .delete_inspected(&row, false)
        .await
        .expect_err("must refuse");
    assert!(err.to_string().contains("unpushed"), "got: {err}");
}

/// Branch deleted upstream → BranchDeletedUpstream reason. Simulate
/// by setting up the worktree the normal way, then deleting the
/// upstream branch + the bare's remote-tracking ref.
#[tokio::test]
async fn branch_deleted_upstream_is_flagged() {
    let fx = setup_fixture().await;
    // Branch upstream + worktree the standard way (mirrors what
    // lazybox does when a PR is open).
    run(&fx.upstream_path, &["branch", "feature"]).await;
    let _wt = add_wt(&fx, "feature", "feature", "feature").await;

    // PR merged → upstream deletes the branch + the bare loses the
    // remote-tracking ref on the next prune-style fetch.
    run(&fx.upstream_path, &["branch", "-D", "feature"]).await;
    run(
        &fx.bare,
        &["update-ref", "-d", "refs/remotes/origin/feature"],
    )
    .await;

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("feature"))
        .expect("row");
    assert!(
        row.reasons.contains(&OrphanReason::BranchDeletedUpstream),
        "expected BranchDeletedUpstream, got {:?}",
        row.reasons
    );
}

/// Prunable entry: worktree dir vanishes from disk while the bare
/// clone still references it. The inspector should surface a
/// synthetic row with the `Prunable` reason so the user / bulk action
/// can clear stale metadata.
#[tokio::test]
async fn prunable_metadata_surfaces_as_synthetic_row() {
    let fx = setup_fixture().await;
    let wt = add_wt(&fx, "gone", "gone", "main").await;

    // Nuke the dir but leave the bare's `.git/worktrees/gone/` index
    // pointing to it — this is the "broken / prunable" state git
    // surfaces after a manual `rm -rf`.
    std::fs::remove_dir_all(&wt).unwrap();

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("gone"))
        .expect("synthetic row");
    assert!(row.reasons.contains(&OrphanReason::Prunable));
    // No directory to size up.
    assert_eq!(row.size_bytes, 0);
    assert!(row.last_modified.is_none());
    assert!(row.is_safe_to_delete);

    // Deleting a prunable entry routes through `git worktree prune`
    // and clears the bare's metadata.
    mgr(&fx).delete_inspected(row, false).await.unwrap();
    let listed = run_capture(&fx.bare, &["worktree", "list", "--porcelain"]).await;
    assert!(
        !listed.contains("gone"),
        "bare's worktree index should no longer mention 'gone': {listed}"
    );
}

/// Does the bare clone still hold a local branch ref?
async fn local_branch_exists(fx: &Fixture, branch: &str) -> bool {
    tokio::process::Command::new("git")
        .current_dir(&fx.bare)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// After a worktree is reaped, its local branch ref is dropped too —
/// otherwise merged feature branches accumulate in the bare clone
/// forever (issue #160).
#[tokio::test]
async fn delete_inspected_reaps_local_branch() {
    let fx = setup_fixture().await;
    let wt = add_wt(&fx, "feature", "feature", "main").await;
    assert!(local_branch_exists(&fx, "feature").await, "branch set up");

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("feature"))
        .expect("feature row")
        .clone();
    mgr(&fx).delete_inspected(&row, true).await.unwrap();

    assert!(!wt.exists(), "worktree removed");
    assert!(
        !local_branch_exists(&fx, "feature").await,
        "orphaned local branch should be deleted with its worktree"
    );
}

/// The issue's exact shape: `main` is checked out in one worktree while
/// a feature worktree is reaped from another. Reaping the feature must
/// drop the feature branch but never touch `main` — the branch the bare
/// clone's HEAD points at, shared by the other worktree.
#[tokio::test]
async fn delete_inspected_preserves_default_branch_checked_out_elsewhere() {
    let fx = setup_fixture().await;
    let main_wt = add_wt(&fx, "mainwt", "main", "main").await;
    let feature_wt = add_wt(&fx, "feature", "feature", "main").await;

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("feature"))
        .expect("feature row")
        .clone();
    mgr(&fx).delete_inspected(&row, true).await.unwrap();

    assert!(!feature_wt.exists(), "feature worktree removed");
    assert!(
        !local_branch_exists(&fx, "feature").await,
        "feature branch reaped"
    );
    assert!(main_wt.exists(), "main worktree untouched");
    assert!(
        local_branch_exists(&fx, "main").await,
        "default branch must survive its worktree reaping"
    );
}

/// Reaping a worktree that itself has the default branch checked out
/// must not delete `main` — even with no other worktree holding it.
#[tokio::test]
async fn delete_inspected_never_deletes_default_branch() {
    let fx = setup_fixture().await;
    let main_wt = add_wt(&fx, "mainwt", "main", "main").await;

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("mainwt"))
        .expect("main row")
        .clone();
    mgr(&fx).delete_inspected(&row, true).await.unwrap();

    assert!(!main_wt.exists(), "worktree removed");
    assert!(
        local_branch_exists(&fx, "main").await,
        "default branch must never be deleted on worktree reaping"
    );
}

/// A never-pushed local branch (issue-spawn shape) with committed
/// work must NOT classify as BranchDeletedUpstream and must NOT be
/// safe to delete — the remote ref never existed, and reaping it
/// would destroy the only copy of the commits.
#[tokio::test]
async fn never_pushed_local_branch_with_commits_is_not_safe() {
    let fx = setup_fixture().await;
    let wt = add_local_only_wt(&fx, "issue7", "lazybox/issue-7").await;
    std::fs::write(wt.join("work.txt"), "local work").unwrap();
    run(&wt, &["add", "."]).await;
    run(&wt, &["commit", "-q", "-m", "committed but never pushed"]).await;

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("issue7"))
        .expect("issue7 row");
    assert!(
        !row.reasons.contains(&OrphanReason::BranchDeletedUpstream),
        "a branch that never existed upstream must not classify as deleted-upstream: {:?}",
        row.reasons
    );
    assert!(
        row.has_unpushed_commits,
        "committed-but-never-pushed work must count as unpushed"
    );
    assert!(
        !row.is_safe_to_delete,
        "reaping would destroy the only copy of the commits"
    );
}

/// The legit cleanup path: a branch that WAS pushed, merged into
/// main, and auto-deleted upstream stays classified
/// BranchDeletedUpstream and safe to delete.
#[tokio::test]
async fn merged_and_deleted_upstream_branch_is_still_safe() {
    let fx = setup_fixture().await;
    let wt = add_wt(&fx, "feature", "feature", "main").await;

    // Real work, pushed to the remote.
    std::fs::write(wt.join("feat.txt"), "feature work").unwrap();
    run(&wt, &["add", "."]).await;
    run(&wt, &["commit", "-q", "-m", "feature work"]).await;
    run(&wt, &["push", "-q", "origin", "feature"]).await;

    // Upstream merges the PR and auto-deletes the branch.
    run(&fx.upstream_path, &["merge", "-q", "feature"]).await;
    run(&fx.upstream_path, &["branch", "-D", "feature"]).await;
    // The bare clone's next prune-style fetch reflects both.
    run(
        &fx.bare,
        &["fetch", "-q", "origin", "+main:refs/remotes/origin/main"],
    )
    .await;
    run(
        &fx.bare,
        &["update-ref", "-d", "refs/remotes/origin/feature"],
    )
    .await;

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    let row = report
        .iter()
        .find(|r| r.path.ends_with("feature"))
        .expect("feature row");
    assert!(
        row.reasons.contains(&OrphanReason::BranchDeletedUpstream),
        "pushed-then-deleted branch keeps the classification: {:?}",
        row.reasons
    );
    assert!(
        !row.has_unpushed_commits,
        "every commit reached the remote before the merge"
    );
    assert!(
        row.is_safe_to_delete,
        "the legit auto-cleanup path must keep working"
    );
}

/// Bulk safety: with a mix of safe + unsafe entries, only the safe
/// ones get deleted when the caller filters on `is_safe_to_delete`.
#[tokio::test]
async fn bulk_safe_skips_unsafe_entries() {
    let fx = setup_fixture().await;
    let safe = add_wt(&fx, "safe", "safe", "main").await;
    let dirty = add_wt(&fx, "dirty", "dirty", "main").await;
    std::fs::write(dirty.join("wip.txt"), "wip").unwrap();

    let report = mgr(&fx).inspect_worktrees(&[]).await.unwrap();
    for row in &report {
        if row.is_safe_to_delete {
            mgr(&fx).delete_inspected(row, false).await.unwrap();
        }
    }

    assert!(!safe.exists(), "safe wt removed");
    assert!(dirty.exists(), "dirty wt preserved");
}
