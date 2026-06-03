//! Tests for `WorktreeManager::apply_scripts`. Per the new test rule
//! (`feedback_test_timeouts.md`): synchronous filesystem ops have no
//! await that could hang, so no body-wrap timeout. The IO is bounded
//! by `tempfile::TempDir` cleanup at end of scope.

use lazybox_git_ops::{Script, ScriptBody, Worktree, WorktreeManager};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tempfile::TempDir;

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

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Empty scripts list — no `_lazybox/scripts/` directory created,
/// no error. Mirrors the `apply_mounts` empty-list contract.
#[tokio::test]
async fn empty_scripts_is_noop() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    wm(&base).apply_scripts(&wt, &[]).await.expect("noop");
    let scripts_dir = wt.path.join("_lazybox").join("scripts");
    assert!(!scripts_dir.exists(), "no scripts dir on empty list");
}

/// Inline script writes the body verbatim + auto-injects a shebang
/// when missing + chmod +x.
#[tokio::test]
async fn inline_script_writes_body_and_chmods_executable() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    wm(&base)
        .apply_scripts(
            &wt,
            &[Script {
                name: "cleanup".into(),
                body: ScriptBody::Inline("echo hello".into()),
            }],
        )
        .await
        .expect("apply");
    let path = wt.path.join("_lazybox/scripts/cleanup");
    assert!(path.exists(), "file written");
    let body = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(body, "#!/usr/bin/env bash\necho hello", "shebang injected");
    #[cfg(unix)]
    assert!(is_executable(&path), "chmod +x applied");
}

/// Body that already has a shebang is preserved as-is.
#[tokio::test]
async fn inline_script_preserves_existing_shebang() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    wm(&base)
        .apply_scripts(
            &wt,
            &[Script {
                name: "py".into(),
                body: ScriptBody::Inline("#!/usr/bin/env python3\nprint('hi')".into()),
            }],
        )
        .await
        .unwrap();
    let body = tokio::fs::read_to_string(wt.path.join("_lazybox/scripts/py"))
        .await
        .unwrap();
    assert_eq!(body, "#!/usr/bin/env python3\nprint('hi')");
}

/// Linked script creates a symlink to the source. Edits to the
/// source flow through the symlink without re-applying.
#[tokio::test]
async fn linked_script_creates_symlink_to_source() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source = ext.path().join("cleanup.sh");
    tokio::fs::write(&source, "#!/bin/sh\necho v1")
        .await
        .unwrap();
    let wt = make_worktree(&base).await;
    wm(&base)
        .apply_scripts(
            &wt,
            &[Script {
                name: "cleanup".into(),
                body: ScriptBody::Linked(source.clone()),
            }],
        )
        .await
        .expect("apply");
    let target = wt.path.join("_lazybox/scripts/cleanup");
    let resolved = tokio::fs::read_link(&target).await.unwrap();
    assert_eq!(resolved, source);
    // Edits to source flow through.
    tokio::fs::write(&source, "#!/bin/sh\necho v2")
        .await
        .unwrap();
    let read_through = tokio::fs::read_to_string(&target).await.unwrap();
    assert_eq!(read_through, "#!/bin/sh\necho v2");
}

/// Linked script with a non-existent source path errors before any
/// I/O happens. No symlink left behind on disk.
#[tokio::test]
async fn linked_script_errors_when_source_missing() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    let err = wm(&base)
        .apply_scripts(
            &wt,
            &[Script {
                name: "cleanup".into(),
                body: ScriptBody::Linked(PathBuf::from("/nonexistent/path/script")),
            }],
        )
        .await
        .expect_err("must error on missing source");
    assert!(err.to_string().contains("does not exist"), "got: {err}");
    assert!(!wt.path.join("_lazybox/scripts/cleanup").exists());
}

/// Re-applying the same inline script is idempotent — no error,
/// content unchanged, chmod intact.
#[tokio::test]
async fn inline_script_re_apply_is_idempotent() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    let scripts = vec![Script {
        name: "cleanup".into(),
        body: ScriptBody::Inline("echo hello".into()),
    }];
    let mgr = wm(&base);
    mgr.apply_scripts(&wt, &scripts).await.expect("first");
    mgr.apply_scripts(&wt, &scripts).await.expect("second");
    let path = wt.path.join("_lazybox/scripts/cleanup");
    let body = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(body, "#!/usr/bin/env bash\necho hello");
    #[cfg(unix)]
    assert!(is_executable(&path));
}

/// Re-applying inline with different content rewrites the file —
/// lazybox must let users iterate on the script without manual
/// deletion.
#[tokio::test]
async fn inline_script_re_apply_with_new_content_rewrites() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    let mgr = wm(&base);
    mgr.apply_scripts(
        &wt,
        &[Script {
            name: "cleanup".into(),
            body: ScriptBody::Inline("echo v1".into()),
        }],
    )
    .await
    .unwrap();
    mgr.apply_scripts(
        &wt,
        &[Script {
            name: "cleanup".into(),
            body: ScriptBody::Inline("echo v2".into()),
        }],
    )
    .await
    .unwrap();
    let body = tokio::fs::read_to_string(wt.path.join("_lazybox/scripts/cleanup"))
        .await
        .unwrap();
    assert_eq!(body, "#!/usr/bin/env bash\necho v2");
}

/// Re-applying linked with a different source errors — same
/// conflict semantics as `apply_mounts`. The user has to remove
/// the old symlink before lazybox will install a new one (avoids
/// silently rewiring scripts).
#[tokio::test]
async fn linked_script_re_apply_with_different_source_errors() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source_a = ext.path().join("a.sh");
    let source_b = ext.path().join("b.sh");
    tokio::fs::write(&source_a, "echo A").await.unwrap();
    tokio::fs::write(&source_b, "echo B").await.unwrap();
    let wt = make_worktree(&base).await;
    let mgr = wm(&base);
    mgr.apply_scripts(
        &wt,
        &[Script {
            name: "cleanup".into(),
            body: ScriptBody::Linked(source_a.clone()),
        }],
    )
    .await
    .unwrap();
    let err = mgr
        .apply_scripts(
            &wt,
            &[Script {
                name: "cleanup".into(),
                body: ScriptBody::Linked(source_b),
            }],
        )
        .await
        .expect_err("conflict must error");
    assert!(
        err.to_string().contains("already exists but points to"),
        "got: {err}"
    );
    // Original symlink untouched.
    let resolved = tokio::fs::read_link(wt.path.join("_lazybox/scripts/cleanup"))
        .await
        .unwrap();
    assert_eq!(resolved, source_a);
}

/// Name with a path separator is rejected before any I/O — no
/// partial install on disk.
#[tokio::test]
async fn script_name_with_path_separator_is_rejected() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    let err = wm(&base)
        .apply_scripts(
            &wt,
            &[Script {
                name: "../escape".into(),
                body: ScriptBody::Inline("x".into()),
            }],
        )
        .await
        .expect_err("must reject");
    assert!(err.to_string().contains("path separators"), "got: {err}");
}

/// Empty name is rejected.
#[tokio::test]
async fn empty_script_name_is_rejected() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    let err = wm(&base)
        .apply_scripts(
            &wt,
            &[Script {
                name: "".into(),
                body: ScriptBody::Inline("x".into()),
            }],
        )
        .await
        .expect_err("must reject");
    assert!(err.to_string().contains("must not be empty"), "got: {err}");
}

/// Hidden-file name (starts with `.`) is rejected — lazybox's
/// scripts dir should hold visible, callable tools, not dotfiles.
#[tokio::test]
async fn hidden_script_name_is_rejected() {
    let base = TempDir::new().unwrap();
    let wt = make_worktree(&base).await;
    let err = wm(&base)
        .apply_scripts(
            &wt,
            &[Script {
                name: ".secret".into(),
                body: ScriptBody::Inline("x".into()),
            }],
        )
        .await
        .expect_err("must reject");
    assert!(err.to_string().contains("must not start"), "got: {err}");
}

/// Multiple scripts in one call: both materialize, both executable.
#[tokio::test]
async fn apply_multiple_scripts_in_one_call() {
    let base = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let source = ext.path().join("setup.sh");
    tokio::fs::write(&source, "echo setup").await.unwrap();
    let wt = make_worktree(&base).await;
    wm(&base)
        .apply_scripts(
            &wt,
            &[
                Script {
                    name: "cleanup".into(),
                    body: ScriptBody::Inline("cargo clean".into()),
                },
                Script {
                    name: "setup".into(),
                    body: ScriptBody::Linked(source.clone()),
                },
            ],
        )
        .await
        .expect("apply");
    let cleanup = wt.path.join("_lazybox/scripts/cleanup");
    let setup = wt.path.join("_lazybox/scripts/setup");
    assert!(cleanup.exists() && setup.exists());
    #[cfg(unix)]
    assert!(is_executable(&cleanup));
    let setup_target = tokio::fs::read_link(&setup).await.unwrap();
    assert_eq!(setup_target, source);
}
