//! Bakes the git short SHA of the build into `LAZYBOX_BUILD_SHA` so the
//! connection handshake can detect a daemon and client compiled from
//! different commits — a mismatch that `PROTOCOL_VERSION` alone can't
//! catch when the wire format hasn't changed.
//!
//! Also bakes the build commit (`LAZYBOX_BUILD_GIT_SHA`, suffix-free)
//! and the source checkout (`LAZYBOX_BUILD_SOURCE_DIR`) so the running
//! binary can compare itself with that checkout's current branch and
//! warn when a stale build silently reproduces fixed bugs.
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
    // guard resolves the commit against that checkout at runtime, which
    // needs the raw revision and a repo to resolve it in.
    println!("cargo:rustc-env=LAZYBOX_BUILD_GIT_SHA={sha}");
    let source_dir = git(&["rev-parse", "--show-toplevel"]).unwrap_or_default();
    println!("cargo:rustc-env=LAZYBOX_BUILD_SOURCE_DIR={source_dir}");

    // Installer-provenance marker. cargo-dist compiles release artifacts
    // with `--profile dist` (see `[profile.dist]` in the workspace
    // Cargo.toml); every other invocation — `cargo run`, `cargo build`,
    // `cargo build --release`, `cargo test` — is a dev/source build. The
    // profile name is the fourth path component up from OUT_DIR
    // (`<target>/<profile>/build/<pkg>/out`), which is the only place
    // cargo surfaces a *custom* profile name to a build script:
    // `PROFILE` collapses every release-inheriting profile to `release`.
    // Only a build we can confidently attribute to the installer flow is
    // marked a release; anything else falls back to dev.
    let is_release_build = std::env::var_os("OUT_DIR")
        .map(std::path::PathBuf::from)
        .and_then(|out| {
            out.ancestors()
                .nth(3)
                .and_then(|p| p.file_name())
                .map(|name| name == "dist")
        })
        .unwrap_or(false);
    println!(
        "cargo:rustc-env=LAZYBOX_RELEASE_BUILD={}",
        u8::from(is_release_build)
    );

    // Re-run when HEAD moves (new commit / checkout) so the baked SHA
    // tracks the source. `--git-path` resolves correctly inside git
    // worktrees, where `.git` is a file pointing elsewhere.
    for path in ["HEAD", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
