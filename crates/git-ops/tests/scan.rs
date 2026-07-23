//! Tests for [`lazybox_git_ops::scan_external_checkouts`] — the
//! external-checkout scanner behind issue #348's `lazybox scan`.
//!
//! Each test builds real git repos (and a real `git worktree`) under a
//! tempdir and asserts the scan classifies them correctly. Every git
//! call runs under a fixture cwd bounded by the OS process, so no extra
//! timeout wrapper is needed (mirrors `tests/inspect.rs`).

use lazybox_git_ops::{DiscoveredCheckout, scan_external_checkouts};
use std::path::Path;
use tempfile::TempDir;

/// The common case in these tests: hidden directories excluded.
async fn scan_default(
    roots: &[std::path::PathBuf],
    max_depth: usize,
    exclude: &Path,
) -> Vec<DiscoveredCheckout> {
    scan_external_checkouts(roots, max_depth, false, exclude).await
}

/// Fixture git must not inherit the developer's signing setup — a
/// locked gpg/1Password agent would hang fixture commits. Mirrors
/// `tests/inspect.rs::no_signing`.
fn no_signing(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    cmd.env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "tag.gpgsign")
        .env("GIT_CONFIG_VALUE_1", "false")
}

async fn run(cwd: &Path, args: &[&str]) {
    let out = no_signing(
        tokio::process::Command::new("git")
            .current_dir(cwd)
            .args(args),
    )
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

/// Make `dir` a git repo on `main` with one commit.
async fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    run(dir, &["init", "-q", "-b", "main"]).await;
    run(dir, &["config", "user.email", "t@e.st"]).await;
    run(dir, &["config", "user.name", "tester"]).await;
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    run(dir, &["add", "."]).await;
    run(dir, &["commit", "-q", "-m", "init"]).await;
}

/// A directory that is never anywhere near a real checkout, so it can
/// never be spuriously excluded.
fn nowhere() -> std::path::PathBuf {
    std::path::PathBuf::from("/nonexistent-lazybox-base")
}

#[tokio::test]
async fn discovers_clone_worktree_and_nested_but_not_plain_dirs() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("code");

    // A normal clone with an origin remote + a linked worktree.
    let repo_a = root.join("repoA");
    init_repo(&repo_a).await;
    run(
        &repo_a,
        &["remote", "add", "origin", "git@github.com:acme/repoA.git"],
    )
    .await;
    let wt = root.join("repoA-feature");
    run(
        &repo_a,
        &[
            "worktree",
            "add",
            "-q",
            &wt.to_string_lossy(),
            "-b",
            "feature",
        ],
    )
    .await;

    // A repo nested three levels down.
    let repo_b = root.join("nested").join("deep").join("repoB");
    init_repo(&repo_b).await;

    // A plain directory tree that is NOT a repo — must not appear.
    std::fs::create_dir_all(root.join("notrepo").join("sub")).unwrap();

    let found = scan_default(std::slice::from_ref(&root), 4, &nowhere()).await;
    let paths: Vec<_> = found.iter().map(|c| c.path.clone()).collect();

    assert_eq!(found.len(), 3, "expected repoA, its worktree, and repoB");
    assert!(paths.contains(&repo_a));
    assert!(paths.contains(&wt));
    assert!(paths.contains(&repo_b));

    // repoA: primary checkout on main, has an origin, clean.
    let a = found.iter().find(|c| c.path == repo_a).unwrap();
    assert_eq!(a.branch.as_deref(), Some("main"));
    assert!(!a.is_linked_worktree);
    assert_eq!(
        a.remote_url.as_deref(),
        Some("git@github.com:acme/repoA.git")
    );
    assert!(!a.has_uncommitted_changes);
    assert!(a.last_commit_unix.is_some());

    // The worktree: linked, on `feature`, no origin of its own.
    let f = found.iter().find(|c| c.path == wt).unwrap();
    assert_eq!(f.branch.as_deref(), Some("feature"));
    assert!(f.is_linked_worktree);
}

#[tokio::test]
async fn describe_checkout_at_maps_a_single_path_or_rejects_a_non_repo() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo).await;
    run(
        &repo,
        &["remote", "add", "origin", "git@github.com:acme/widget.git"],
    )
    .await;

    let got = lazybox_git_ops::describe_checkout_at(repo.clone())
        .await
        .expect("an initialized repo describes");
    assert_eq!(got.path, repo);
    assert_eq!(got.branch.as_deref(), Some("main"));
    assert_eq!(
        got.remote_url.as_deref(),
        Some("git@github.com:acme/widget.git")
    );

    // A plain directory (no `.git`) is not a checkout.
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(lazybox_git_ops::describe_checkout_at(plain).await.is_none());
}

#[tokio::test]
async fn max_depth_bounds_the_walk() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("code");
    // Only repo lives three levels below the root.
    let deep = root.join("a").join("b").join("repo");
    init_repo(&deep).await;

    let shallow = scan_default(std::slice::from_ref(&root), 1, &nowhere()).await;
    assert!(
        shallow.is_empty(),
        "depth 1 must not reach a repo three levels down, got {shallow:?}"
    );

    let deep_scan = scan_default(std::slice::from_ref(&root), 3, &nowhere()).await;
    assert_eq!(deep_scan.len(), 1);
    assert_eq!(deep_scan[0].path, deep);
}

#[tokio::test]
async fn dirty_checkout_is_flagged() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo).await;
    std::fs::write(repo.join("README.md"), "changed\n").unwrap();

    let found = scan_default(&[tmp.path().to_path_buf()], 2, &nowhere()).await;
    assert_eq!(found.len(), 1);
    assert!(found[0].has_uncommitted_changes);
}

#[tokio::test]
async fn detached_head_reports_no_branch() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo).await;
    // Second commit, then detach onto the first so HEAD is detached.
    std::fs::write(repo.join("two.txt"), "2\n").unwrap();
    run(&repo, &["add", "."]).await;
    run(&repo, &["commit", "-q", "-m", "two"]).await;
    run(&repo, &["checkout", "-q", "--detach", "HEAD~1"]).await;

    let found = scan_default(&[tmp.path().to_path_buf()], 2, &nowhere()).await;
    assert_eq!(found.len(), 1);
    assert!(found[0].branch.is_none(), "detached HEAD → no branch");
}

#[tokio::test]
async fn excludes_checkouts_under_the_managed_base() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("code");
    // One external repo, and one that lives under the "managed base".
    let external = root.join("mine");
    init_repo(&external).await;
    let managed_base = root.join("dot-lazybox");
    let managed = managed_base.join("worktrees").join("wt");
    init_repo(&managed).await;

    let found = scan_default(std::slice::from_ref(&root), 4, &managed_base).await;
    let paths: Vec<_> = found.iter().map(|c| c.path.clone()).collect();
    assert!(paths.contains(&external));
    assert!(
        !paths.iter().any(|p| p.starts_with(&managed_base)),
        "checkouts under the managed base must be excluded, got {paths:?}"
    );
}

#[tokio::test]
async fn deduplicates_overlapping_roots() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("code");
    let repo = root.join("repo");
    init_repo(&repo).await;

    // The root and a subdirectory of it both passed as roots — the
    // repo must be reported exactly once.
    let found = scan_default(&[root.clone(), root.clone()], 4, &nowhere()).await;
    assert_eq!(found.len(), 1, "overlapping roots must not double-count");
}

#[cfg(unix)]
#[tokio::test]
async fn follows_symlinked_repo_directory() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("code");
    std::fs::create_dir_all(&root).unwrap();
    // Real repo outside the root; a symlink under the root points at it.
    let real = tmp.path().join("elsewhere").join("proj");
    init_repo(&real).await;
    std::os::unix::fs::symlink(&real, root.join("proj-link")).unwrap();

    let found = scan_default(std::slice::from_ref(&root), 4, &nowhere()).await;
    assert_eq!(
        found.len(),
        1,
        "a repo behind a symlinked dir must be found"
    );
    // Reported at the path as found under the root (the symlink).
    assert_eq!(found[0].path, root.join("proj-link"));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_cycle_terminates() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("code");
    let repo = root.join("repo");
    init_repo(&repo).await;
    // A symlink pointing back up to the root — a cycle. The visited-set
    // guard must make the walk terminate and still report the repo once.
    std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();

    let found = scan_default(std::slice::from_ref(&root), 6, &nowhere()).await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, repo);
}

#[tokio::test]
async fn hidden_dirs_skipped_by_default_and_included_on_request() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("code");
    std::fs::create_dir_all(&root).unwrap();
    // A repo living inside a dot-directory under the root.
    let hidden_repo = root.join(".dotfiles");
    init_repo(&hidden_repo).await;

    let default = scan_default(std::slice::from_ref(&root), 4, &nowhere()).await;
    assert!(
        default.is_empty(),
        "a dot-dir repo is skipped by default, got {default:?}"
    );

    let with_hidden =
        scan_external_checkouts(std::slice::from_ref(&root), 4, true, &nowhere()).await;
    assert_eq!(with_hidden.len(), 1);
    assert_eq!(with_hidden[0].path, hidden_repo);

    // A dot-dir passed as the ROOT is always walked, hidden flag or not —
    // the skip only applies to descending into discovered subdirectories.
    let as_root = scan_default(std::slice::from_ref(&hidden_repo), 1, &nowhere()).await;
    assert_eq!(as_root.len(), 1, "an explicit hidden root is still scanned");
    assert_eq!(as_root[0].path, hidden_repo);
}
