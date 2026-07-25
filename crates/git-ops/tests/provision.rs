//! Tests for bare-clone provisioning robustness:
//!
//! - an interrupted clone must not poison the bare-repo cache (the
//!   clone now lands in `<path>.partial` and is renamed on success;
//!   broken directories at the final path are detected and re-cloned);
//! - stale `.partial` leftovers are cleared before cloning;
//! - an existing worktree directory is only reused when it really is
//!   a worktree of the expected bare clone — empty leftovers are
//!   re-provisioned, foreign content errors loudly;
//! - concurrent checkouts of the same repo serialize on the per-repo
//!   lock so both succeed.

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

/// Upstream repo with one commit on `main`, plus the base dir the
/// manager operates on. Unlike the worktree_base fixture this does
/// NOT seed a bare clone — these tests exercise the (re-)clone path.
fn setup_upstream() -> (TempDir, TempDir) {
    let upstream = TempDir::new().unwrap();
    git(upstream.path(), &["init", "-b", "main", "-q"]);
    git(upstream.path(), &["config", "user.email", "t@example.com"]);
    git(upstream.path(), &["config", "user.name", "Tester"]);
    git(
        upstream.path(),
        &["commit", "--allow-empty", "-m", "first", "-q"],
    );
    let base = TempDir::new().unwrap();
    (upstream, base)
}

fn bare_path(base: &TempDir, owner: &str, repo: &str) -> PathBuf {
    base.path()
        .join("repos")
        .join(owner)
        .join(format!("{repo}.git"))
}

/// Simulate the aftermath of a killed `git clone --bare` from before
/// the `.partial` scheme: a directory at the final bare path that
/// git can't use, but whose `config` still names the origin. The
/// re-clone path reads that URL so the test never touches the
/// network.
fn poison_bare(base: &TempDir, owner: &str, repo: &str, upstream: &Path) -> PathBuf {
    let bare = bare_path(base, owner, repo);
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::write(
        bare.join("config"),
        format!(
            "[core]\n\tbare = true\n[remote \"origin\"]\n\turl = {}\n",
            upstream.display()
        ),
    )
    .unwrap();
    bare
}

/// Is the directory a usable bare repo with a resolvable HEAD?
fn bare_is_healthy(bare: &Path) -> bool {
    let probe = |args: &[&str]| {
        git_cmd(bare, args)
            .output()
            .map(|o| {
                (
                    o.status.success(),
                    String::from_utf8_lossy(&o.stdout).trim().to_string(),
                )
            })
            .unwrap_or((false, String::new()))
    };
    let (ok, out) = probe(&["rev-parse", "--is-bare-repository"]);
    if !ok || out != "true" {
        return false;
    }
    probe(&["rev-parse", "--verify", "--quiet", "HEAD"]).0
}

#[tokio::test]
async fn poisoned_bare_clone_is_detected_and_recloned() {
    let (upstream, base) = setup_upstream();
    let bare = poison_bare(&base, "acme", "broken", upstream.path());
    assert!(!bare_is_healthy(&bare), "fixture must start broken");

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("worktrees").join("wt-main");
    let wt = wm
        .checkout_at(&wt_path, "acme", "broken", "main", None)
        .await
        .expect("provision must recover from a poisoned bare clone");

    assert!(bare_is_healthy(&bare), "bare clone was replaced");
    assert_eq!(
        git_out(&wt.path, &["rev-parse", "HEAD"]),
        git_out(upstream.path(), &["rev-parse", "HEAD"]),
        "worktree tracks the upstream tip"
    );
    // The staging dir never lingers.
    assert!(!bare.with_extension("git.partial").exists());
}

#[tokio::test]
async fn stale_partial_clone_dir_is_cleaned_up() {
    let (upstream, base) = setup_upstream();
    let bare = poison_bare(&base, "acme", "stale", upstream.path());
    // A `.partial` left behind by an interrupted clone of the new
    // scheme.
    let mut partial = bare.as_os_str().to_os_string();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    std::fs::create_dir_all(partial.join("objects")).unwrap();
    std::fs::write(partial.join("junk"), "interrupted").unwrap();

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("worktrees").join("wt-stale");
    wm.checkout_at(&wt_path, "acme", "stale", "main", None)
        .await
        .expect("stale .partial must not block provisioning");

    assert!(!partial.exists(), "stale partial dir cleaned up");
    assert!(bare_is_healthy(&bare));
}

#[tokio::test]
async fn empty_leftover_worktree_dir_is_reprovisioned() {
    let (upstream, base) = setup_upstream();
    poison_bare(&base, "acme", "leftover", upstream.path());
    let wt_path = base.path().join("worktrees").join("wt-empty");
    // The provision-failure fallback shape: a bare `mkdir -p` dir.
    std::fs::create_dir_all(&wt_path).unwrap();

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt = wm
        .checkout_at(&wt_path, "acme", "leftover", "main", None)
        .await
        .expect("empty leftover dir must be re-provisioned, not trusted");

    assert!(
        wt_path.join(".git").is_file(),
        "the path now holds a real worktree"
    );
    assert_eq!(
        git_out(&wt.path, &["rev-parse", "HEAD"]),
        git_out(upstream.path(), &["rev-parse", "HEAD"]),
    );
}

#[tokio::test]
async fn nonempty_invalid_worktree_dir_errors_loudly() {
    let (upstream, base) = setup_upstream();
    poison_bare(&base, "acme", "occupied", upstream.path());
    let wt_path = base.path().join("worktrees").join("wt-occupied");
    std::fs::create_dir_all(&wt_path).unwrap();
    std::fs::write(wt_path.join("user-data.txt"), "do not delete me").unwrap();

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let err = wm
        .checkout_at(&wt_path, "acme", "occupied", "main", None)
        .await
        .expect_err("a dir with foreign content must not be silently reused or deleted");
    assert!(err.to_string().contains("not a worktree"), "got: {err}");
    assert!(
        wt_path.join("user-data.txt").exists(),
        "foreign content untouched"
    );
}

#[tokio::test]
async fn existing_valid_worktree_is_reused_idempotently() {
    let (upstream, base) = setup_upstream();
    poison_bare(&base, "acme", "reuse", upstream.path());
    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_path = base.path().join("worktrees").join("wt-reuse");

    let first = wm
        .checkout_at(&wt_path, "acme", "reuse", "main", None)
        .await
        .expect("first provision");
    // Leave a marker; a re-provision would wipe it.
    std::fs::write(wt_path.join("marker.txt"), "still here").unwrap();
    let second = wm
        .checkout_at(&wt_path, "acme", "reuse", "main", None)
        .await
        .expect("second call is idempotent");
    assert_eq!(first.path, second.path);
    assert!(wt_path.join("marker.txt").exists(), "no re-provision");
    let _ = upstream;
}

/// Two cold checkouts of the same repo race the clone without the
/// per-repo lock — one would rename `.partial` into place under the
/// other, or collide on `git worktree add` ref locks. With the lock
/// they serialize: one clone, both checkouts succeed.
#[tokio::test]
async fn concurrent_checkouts_of_same_repo_both_succeed() {
    let (upstream, base) = setup_upstream();
    // A second branch so the two checkouts don't ask git for two
    // worktrees of the SAME branch (which git itself refuses).
    git(upstream.path(), &["branch", "dev"]);
    let bare = poison_bare(&base, "acme", "race", upstream.path());

    let wm = WorktreeManager::new(base.path().to_path_buf());
    let wt_a = base.path().join("worktrees").join("race-a");
    let wt_b = base.path().join("worktrees").join("race-b");

    let (a, b) = tokio::join!(
        wm.checkout_at(&wt_a, "acme", "race", "main", None),
        wm.checkout_at(&wt_b, "acme", "race", "dev", None),
    );
    let a = a.expect("first concurrent checkout succeeds");
    let b = b.expect("second concurrent checkout succeeds");

    assert!(bare_is_healthy(&bare), "single healthy clone");
    let tip = git_out(upstream.path(), &["rev-parse", "HEAD"]);
    assert_eq!(git_out(&a.path, &["rev-parse", "HEAD"]), tip);
    assert_eq!(git_out(&b.path, &["rev-parse", "HEAD"]), tip);
}
