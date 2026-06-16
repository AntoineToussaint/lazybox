//! Tests for `WorktreeManager::init_standalone_at` — the no-upstream
//! provisioning path used for workspaces with no `owner/repo` to clone
//! (blank workspaces under a local project, repo-less tasks). The
//! session must land in a real git repo on its lazybox branch, and a
//! re-provision must never wipe the user's work.

use lazybox_git_ops::WorktreeManager;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git_out(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn init_standalone_creates_real_repo_on_branch() {
    let base = TempDir::new().unwrap();
    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("worktrees").join("scratch");

    let wt = wm
        .init_standalone_at(&wt_path, "lazybox/scratch")
        .await
        .expect("standalone init must succeed");

    assert_eq!(wt.path, wt_path);
    assert_eq!(wt.branch, "lazybox/scratch");
    assert!(wt_path.join(".git").is_dir(), "a real git repo exists");
    assert_eq!(
        git_out(&wt_path, &["symbolic-ref", "--short", "HEAD"]),
        "lazybox/scratch",
        "HEAD points at the lazybox branch even before the first commit",
    );
}

#[tokio::test]
async fn init_standalone_is_idempotent_and_preserves_work() {
    let base = TempDir::new().unwrap();
    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("worktrees").join("scratch");

    wm.init_standalone_at(&wt_path, "lazybox/scratch")
        .await
        .unwrap();
    // The user starts working in the freshly-provisioned worktree.
    std::fs::write(wt_path.join("notes.txt"), "in progress").unwrap();

    let again = wm
        .init_standalone_at(&wt_path, "lazybox/scratch")
        .await
        .expect("second call is idempotent");

    assert_eq!(again.path, wt_path);
    assert!(
        wt_path.join("notes.txt").exists(),
        "re-provision must not wipe the user's work",
    );
}
