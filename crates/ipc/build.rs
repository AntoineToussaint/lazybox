//! Bakes the git short SHA of the build into `LAZYBOX_BUILD_SHA` so the
//! connection handshake can detect a daemon and client compiled from
//! different commits — a mismatch that `PROTOCOL_VERSION` alone can't
//! catch when the wire format hasn't changed.
//!
//! Also bakes the build commit (`LAZYBOX_BUILD_GIT_SHA`, suffix-free)
//! and the source checkout (`LAZYBOX_BUILD_SOURCE_DIR`) so the running
//! binary can count how many commits it is behind `origin/main` and
//! warn when a uniformly-stale build silently reproduces fixed bugs.
//! Both are empty when built outside a git checkout (a release tarball),
//! which turns the staleness guard into a no-op rather than a false
//! positive.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn main() {
    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=LAZYBOX_BUILD_SHA={sha}{suffix}");

    // Suffix-free build commit + source checkout root: the staleness
    // guard runs `git -C <dir> rev-list --count <sha>..origin/main` at
    // runtime, which needs the raw revision and a repo to resolve it in.
    println!("cargo:rustc-env=LAZYBOX_BUILD_GIT_SHA={sha}");
    let source_dir = git(&["rev-parse", "--show-toplevel"]).unwrap_or_default();
    println!("cargo:rustc-env=LAZYBOX_BUILD_SOURCE_DIR={source_dir}");

    // Re-run when HEAD moves (new commit / checkout) so the baked SHA
    // tracks the source. `--git-path` resolves correctly inside git
    // worktrees, where `.git` is a file pointing elsewhere.
    for path in ["HEAD", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
