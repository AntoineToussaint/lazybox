//! Namespace validation and the non-destructive rename that clear a
//! branch directory/file collision (#1742).
//!
//! A collision is a fact about *names*, so both primitives are pinned
//! against real git rather than a parsed error string: `for-each-ref` is
//! what says a name is taken, and `branch -m` is what moves one aside
//! without touching a commit.

mod common;

use lazybox_git_ops::WorktreeManager;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

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

/// An upstream with one commit on `main`, already bare-cloned into the
/// manager's base under `repos/<owner>/<repo>.git` — these tests never
/// exercise the clone path, only refs inside an existing bare clone.
fn setup(owner: &str, repo: &str) -> (TempDir, TempDir, PathBuf) {
    let upstream = TempDir::new().unwrap();
    git(upstream.path(), &["init", "-b", "main", "-q"]);
    git(upstream.path(), &["config", "user.email", "t@example.com"]);
    git(upstream.path(), &["config", "user.name", "Tester"]);
    git(
        upstream.path(),
        &["commit", "--allow-empty", "-m", "first", "-q"],
    );

    let base = TempDir::new().unwrap();
    let bare = base
        .path()
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
    (upstream, base, bare)
}

/// The check is over the ref *namespace*, not a string equality: `deps`
/// blocks `deps/grouping` and `deps/grouping-2` alike, which is exactly
/// what makes a suffixed candidate a non-fix. A sibling leaf that merely
/// shares a prefix is free.
#[tokio::test]
async fn namespace_blocker_reports_conflicts_in_both_directions() {
    let (_upstream, base, bare) = setup("acme", "widget");
    git(&bare, &["branch", "deps", "main"]);
    git(&bare, &["branch", "release/v1", "main"]);
    let wm = WorktreeManager::new(base.path().to_path_buf());

    for blocked in ["deps", "deps/grouping", "deps/grouping-2", "deps/a/b"] {
        assert_eq!(
            wm.branch_namespace_blocker("acme", "widget", blocked)
                .await
                .unwrap()
                .as_deref(),
            Some("deps"),
            "{blocked} must report `deps` as its blocker",
        );
    }
    // The other direction: an existing `release/v1` blocks the ancestor.
    assert_eq!(
        wm.branch_namespace_blocker("acme", "widget", "release")
            .await
            .unwrap()
            .as_deref(),
        Some("release/v1"),
    );
    for free in ["deps-grouping", "deps-grouping-2", "release-2", "unrelated"] {
        assert_eq!(
            wm.branch_namespace_blocker("acme", "widget", free)
                .await
                .unwrap(),
            None,
            "{free} must read as free",
        );
    }
}

/// Renaming the blocker frees the namespace and keeps its commits — the
/// repair lazybox is willing to perform on a branch the user did not
/// name. Deleting is never it, so the commit must still be reachable
/// under the new name afterwards.
#[tokio::test]
async fn rename_branch_frees_the_namespace_and_keeps_the_commits() {
    let (_upstream, base, bare) = setup("acme", "widget");
    git(&bare, &["branch", "deps", "main"]);
    // Give `deps` a commit of its own so a rename that silently dropped
    // work would be visible.
    let scratch = base.path().join("scratch");
    git(
        &bare,
        &["worktree", "add", &scratch.to_string_lossy(), "deps", "-q"],
    );
    git(&scratch, &["config", "user.email", "t@example.com"]);
    git(&scratch, &["config", "user.name", "Tester"]);
    git(
        &scratch,
        &["commit", "--allow-empty", "-m", "dep work", "-q"],
    );
    let sha = git_out(&scratch, &["rev-parse", "HEAD"]);
    // Detach so the branch is no longer held by a live checkout.
    git(&scratch, &["checkout", "--detach", "-q"]);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    wm.rename_branch("acme", "widget", "deps", "deps-2")
        .await
        .expect("renaming an unheld branch succeeds");

    assert_eq!(
        wm.branch_namespace_blocker("acme", "widget", "deps/grouping")
            .await
            .unwrap(),
        None,
        "the namespace the rename was for is now free",
    );
    assert_eq!(
        git_out(&bare, &["rev-parse", "deps-2"]),
        sha,
        "the branch's commit moved with its new name — a rename, never a delete",
    );
}

/// A branch someone is working in must not be renamed out from under
/// them: the rename would rewrite that worktree's HEAD. Refused, and the
/// branch is left exactly as it was.
#[tokio::test]
async fn rename_branch_refuses_a_branch_checked_out_live() {
    let (_upstream, base, bare) = setup("acme", "widget");
    let live = base.path().join("live");
    git(
        &bare,
        &[
            "worktree",
            "add",
            &live.to_string_lossy(),
            "-b",
            "deps",
            "-q",
        ],
    );

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let err = wm
        .rename_branch("acme", "widget", "deps", "deps-2")
        .await
        .expect_err("a live checkout must block the rename");
    assert!(
        matches!(err, lazybox_git_ops::GitError::BranchHeldLive { .. }),
        "expected BranchHeldLive, got: {err:?}",
    );
    assert_eq!(
        git_out(&live, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "deps",
        "the live checkout still sits on its branch",
    );
}

/// The destination is revalidated too, so a rename can't trade one
/// collision for another — the state may have changed since whoever
/// proposed the name looked.
#[tokio::test]
async fn rename_branch_refuses_a_taken_destination() {
    let (_upstream, base, bare) = setup("acme", "widget");
    git(&bare, &["branch", "deps", "main"]);
    git(&bare, &["branch", "deps-2/old", "main"]);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let err = wm
        .rename_branch("acme", "widget", "deps", "deps-2")
        .await
        .expect_err("a destination inside an occupied namespace must be refused");
    assert!(
        matches!(
            err,
            lazybox_git_ops::GitError::BranchDirFileConflict { ref conflicting, .. }
                if conflicting == "deps-2/old"
        ),
        "expected a typed D/F conflict naming the blocker, got: {err:?}",
    );
}

/// The handoff the in-modal recovery is built on: it provisions the
/// checkout on the chosen name itself, then lets the ordinary spawn path
/// run. That only resumes the user's work if the spawn *reuses* what is
/// already at the target — `existing_worktree_branch` is what tells it
/// so, and it must report the chosen name, not re-derive one.
#[tokio::test]
async fn a_worktree_provisioned_on_the_chosen_name_is_reused_by_the_next_spawn() {
    let (_upstream, base, bare) = setup("acme", "widget");
    // `deps` occupies the namespace `deps/grouping` needs.
    git(&bare, &["branch", "deps", "main"]);
    let wm = WorktreeManager::new(base.path().to_path_buf());
    let target = base.path().join("widget-42");

    let chosen = lazybox_core::branch_namespace::alternative("deps/grouping", "deps", 1);
    wm.checkout_new_branch_at(&target, "acme", "widget", &chosen, "main")
        .await
        .expect("the recovery provisions on the chosen name");

    assert_eq!(
        wm.existing_worktree_branch("acme", "widget", &target)
            .await
            .unwrap()
            .as_deref(),
        Some(chosen.as_str()),
        "the resumed spawn must find the chosen branch at the target it provisions to",
    );
    // Re-running the same resolution is a no-op rather than a second
    // worktree — a repeated recovery request must not double-create.
    wm.checkout_new_branch_at(&target, "acme", "widget", &chosen, "main")
        .await
        .expect("re-running the resolution reuses the checkout");
    assert_eq!(
        git_out(&target, &["rev-parse", "--abbrev-ref", "HEAD"]),
        chosen,
    );
}

/// The data-loss guard (#1742 review). A disambiguated candidate is only a
/// candidate because the name *looked* free — the namespace probe and the
/// add don't share a lock, so on a box running a fleet of agents the name
/// can be taken in between. `-B` would then reset that branch to base and
/// orphan its commits. The create-only entry point makes git refuse under
/// its own ref lock, and reports it as a typed error the caller can act on.
#[tokio::test]
async fn a_picked_name_never_resets_a_branch_that_took_it() {
    let (_upstream, base, bare) = setup("acme", "widget");
    // Someone else's branch already occupies the candidate name, with a
    // commit that is not on main.
    let theirs = base.path().join("theirs");
    git(
        &bare,
        &[
            "worktree",
            "add",
            &theirs.to_string_lossy(),
            "-b",
            "deps-grouping",
            "-q",
        ],
    );
    git(&theirs, &["config", "user.email", "t@example.com"]);
    git(&theirs, &["config", "user.name", "Tester"]);
    git(
        &theirs,
        &["commit", "--allow-empty", "-m", "their work", "-q"],
    );
    let theirs_sha = git_out(&theirs, &["rev-parse", "HEAD"]);
    git(&theirs, &["checkout", "--detach", "-q"]);

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let target = base.path().join("ours");
    let err = wm
        .checkout_fresh_branch_at(&target, "acme", "widget", "deps-grouping", "main")
        .await
        .expect_err("a picked name must never take over an existing branch");
    assert!(
        matches!(
            err,
            lazybox_git_ops::GitError::BranchAlreadyExists { ref branch }
                if branch == "deps-grouping"
        ),
        "the refusal must be typed so the caller can pick another: {err:?}",
    );
    assert_eq!(
        git_out(&bare, &["rev-parse", "deps-grouping"]),
        theirs_sha,
        "their commit must still be on their branch — `-B` would have reset it to main",
    );

    // The workspace's OWN derived name keeps the take-over semantics a
    // half-finished spawn depends on.
    let own = base.path().join("own");
    wm.checkout_new_branch_at(&own, "acme", "widget", "deps-grouping", "main")
        .await
        .expect("the reset entry point still adopts an existing branch");
}
