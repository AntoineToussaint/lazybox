//! Tests for `WorktreeManager::apply_mounts`. Uses `tempfile` so the
//! filesystem state is isolated and nothing leaks onto the host.

use lazybox_git_ops::{Mount, Placement, Worktree, WorktreeManager};
use std::path::PathBuf;
use tempfile::TempDir;

/// Build a `Worktree` handle pointing at a freshly-created dir inside
/// `base`. The dir structure mimics what WorktreeManager would have
/// created: `<base>/worktrees/<owner>-<repo>-<branch>/`.
async fn make_worktree(base: &TempDir) -> Worktree {
    let path = base.path().join("worktrees").join("o-r-feat");
    tokio::fs::create_dir_all(&path).await.unwrap();
    Worktree {
        name: "o-r-feat".into(),
        path,
        branch: "feat".into(),
    }
}

fn wm(base: &TempDir) -> WorktreeManager {
    WorktreeManager::new(base.path().to_path_buf())
}

async fn make_source_file(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let p = dir.path().join(name);
    tokio::fs::write(&p, body).await.unwrap();
    p
}

#[tokio::test]
async fn inside_mount_creates_symlink_inside_worktree() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source = make_source_file(&ext, "shared.conf", "value=1").await;
    let wt = make_worktree(&base).await;

    wm(&base)
        .apply_mounts(
            &wt,
            &[Mount {
                source: source.clone(),
                link_at: PathBuf::from("configs/shared.conf"),
                placement: Placement::Inside,
            }],
        )
        .await
        .expect("apply");

    let linked = wt.path.join("configs/shared.conf");
    assert!(linked.exists(), "mount target exists");
    let target = tokio::fs::read_link(&linked).await.unwrap();
    assert_eq!(target, source, "symlink points at source");
    // Content is accessible through the link.
    let content = tokio::fs::read_to_string(&linked).await.unwrap();
    assert_eq!(content, "value=1");
}

#[tokio::test]
async fn above_mount_places_one_level_up() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source = make_source_file(&ext, "cache", "x").await;
    let wt = make_worktree(&base).await;

    wm(&base)
        .apply_mounts(
            &wt,
            &[Mount {
                source: source.clone(),
                link_at: PathBuf::from(".shared-cache"),
                placement: Placement::Above,
            }],
        )
        .await
        .expect("apply");

    let linked = wt.path.parent().unwrap().join(".shared-cache");
    assert!(linked.exists(), "placed in the worktree's parent dir");
    assert_eq!(tokio::fs::read_link(&linked).await.unwrap(), source);
}

#[tokio::test]
async fn apply_mounts_creates_parent_dirs() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source = make_source_file(&ext, "s", "x").await;
    let wt = make_worktree(&base).await;

    wm(&base)
        .apply_mounts(
            &wt,
            &[Mount {
                source: source.clone(),
                link_at: PathBuf::from("deep/nested/path/link"),
                placement: Placement::Inside,
            }],
        )
        .await
        .expect("apply");

    let linked = wt.path.join("deep/nested/path/link");
    assert!(linked.exists());
    assert!(linked.parent().unwrap().is_dir());
}

#[tokio::test]
async fn apply_mounts_idempotent_when_symlink_points_at_same_source() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source = make_source_file(&ext, "s", "x").await;
    let wt = make_worktree(&base).await;
    let mount = Mount {
        source: source.clone(),
        link_at: PathBuf::from("link"),
        placement: Placement::Inside,
    };
    let manager = wm(&base);

    manager
        .apply_mounts(&wt, std::slice::from_ref(&mount))
        .await
        .expect("first");
    // Second call is a no-op — no error, symlink still correct.
    manager
        .apply_mounts(&wt, &[mount])
        .await
        .expect("second call ok");
    let linked = wt.path.join("link");
    assert_eq!(tokio::fs::read_link(&linked).await.unwrap(), source);
}

#[tokio::test]
async fn apply_mounts_errors_when_link_points_at_different_source() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source_a = make_source_file(&ext, "a", "A").await;
    let source_b = make_source_file(&ext, "b", "B").await;
    let wt = make_worktree(&base).await;

    // First: link → A.
    wm(&base)
        .apply_mounts(
            &wt,
            &[Mount {
                source: source_a.clone(),
                link_at: PathBuf::from("link"),
                placement: Placement::Inside,
            }],
        )
        .await
        .unwrap();

    // Second: try to link the same path → B. Must refuse.
    let err = wm(&base)
        .apply_mounts(
            &wt,
            &[Mount {
                source: source_b,
                link_at: PathBuf::from("link"),
                placement: Placement::Inside,
            }],
        )
        .await
        .expect_err("conflicting mount must error");
    assert!(
        err.to_string().contains("already exists"),
        "error is about the existing link; got: {err}"
    );
    // And the original link is untouched.
    assert_eq!(
        tokio::fs::read_link(wt.path.join("link")).await.unwrap(),
        source_a
    );
}

#[tokio::test]
async fn apply_mounts_errors_when_target_exists_as_regular_file() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source = make_source_file(&ext, "s", "x").await;
    let wt = make_worktree(&base).await;
    // Something else is already at the mount path.
    tokio::fs::write(wt.path.join("occupied"), "pre-existing")
        .await
        .unwrap();

    let err = wm(&base)
        .apply_mounts(
            &wt,
            &[Mount {
                source,
                link_at: PathBuf::from("occupied"),
                placement: Placement::Inside,
            }],
        )
        .await
        .expect_err("should error");
    assert!(
        err.to_string().contains("not a symlink"),
        "refuse to overwrite non-symlinks; got: {err}"
    );
    // Original file intact.
    let content = tokio::fs::read_to_string(wt.path.join("occupied"))
        .await
        .unwrap();
    assert_eq!(content, "pre-existing");
}

#[tokio::test]
async fn apply_mounts_empty_list_is_noop() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    wm(&base).apply_mounts(&wt, &[]).await.expect("noop");
}

#[tokio::test]
async fn apply_multiple_mounts_in_one_call() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let a = make_source_file(&ext, "a", "A").await;
    let b = make_source_file(&ext, "b", "B").await;
    let wt = make_worktree(&base).await;

    wm(&base)
        .apply_mounts(
            &wt,
            &[
                Mount {
                    source: a.clone(),
                    link_at: PathBuf::from("inside-a"),
                    placement: Placement::Inside,
                },
                Mount {
                    source: b.clone(),
                    link_at: PathBuf::from("above-b"),
                    placement: Placement::Above,
                },
            ],
        )
        .await
        .expect("apply");

    assert_eq!(
        tokio::fs::read_link(wt.path.join("inside-a"))
            .await
            .unwrap(),
        a
    );
    assert_eq!(
        tokio::fs::read_link(wt.path.parent().unwrap().join("above-b"))
            .await
            .unwrap(),
        b
    );
}
