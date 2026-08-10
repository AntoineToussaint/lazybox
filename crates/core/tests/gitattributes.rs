//! Guards the root `.gitattributes` merge strategies (#1004).
//!
//! Lock and append-only catalog files hand-conflict on nearly every
//! branch when many agents fan out from a fast-moving `main`. The fix
//! is git's built-in `union` merge driver, declared in the root
//! `.gitattributes`. This test asks git itself (`git check-attr`) what
//! strategy resolves for each path, so a typo'd pattern that silently
//! stops applying — or an over-broad `* merge=union` that would corrupt
//! ordinary source merges — fails the build.
//!
//! Lives in `lazybox-core` alongside `dep_rules.rs`: core is a leaf with
//! no internal deps, the natural home for repo-wide hygiene guards.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

/// The `merge` attribute git resolves for `path`, e.g. `"union"` or
/// `"unspecified"`.
fn merge_attr(root: &Path, path: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-attr", "merge", "--", path])
        .output()
        .expect("run `git check-attr`");
    assert!(
        out.status.success(),
        "git check-attr failed for {path}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Output form: "<path>: merge: <value>\n".
    let line = String::from_utf8(out.stdout).expect("utf8 check-attr output");
    line.rsplit_once(": ")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| panic!("unexpected check-attr output: {line:?}"))
}

#[test]
fn union_merge_declared_for_lock_and_catalog() {
    let root = workspace_root();
    for path in ["Cargo.lock", ".lazybox/snippets.yaml"] {
        assert_eq!(
            merge_attr(&root, path),
            "union",
            "{path} must resolve to `merge=union` (see root .gitattributes)"
        );
    }
}

#[test]
fn ordinary_source_is_not_unioned() {
    // A blanket `* merge=union` would silently corrupt real code merges;
    // ordinary source must keep git's default (conflict-raising) driver.
    let root = workspace_root();
    for path in ["crates/core/src/lib.rs", "Cargo.toml"] {
        assert_eq!(
            merge_attr(&root, path),
            "unspecified",
            "{path} must not carry a merge strategy"
        );
    }
}
