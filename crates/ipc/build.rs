//! Bakes the git short SHA of the build into `LAZYBOX_BUILD_SHA` so the
//! connection handshake can detect a daemon and client compiled from
//! different commits — a mismatch the wire fingerprint alone can't
//! catch when the wire format hasn't changed.
//!
//! Also derives `LAZYBOX_PROTOCOL_FINGERPRINT`: a hash of every
//! wire-defining input — this crate's sources, `lazybox-core`'s (its
//! types ride inside events), and the workspace lockfile (the byte
//! encoding also depends on the serde/bincode versions). The handshake
//! exchanges it in place of a hand-bumped protocol version, so a wire
//! change can never slip through under an unchanged number.
//!
//! Also bakes the build commit (`LAZYBOX_BUILD_GIT_SHA`, suffix-free)
//! and the source checkout (`LAZYBOX_BUILD_SOURCE_DIR`) so the running
//! binary can compare itself with that checkout's current branch and
//! warn when a stale build silently reproduces fixed bugs.
//! Both are empty when built outside a git checkout (a release tarball),
//! which turns the staleness guard into a no-op rather than a false
//! positive.

use std::path::{Path, PathBuf};
use std::process::Command;

/// FNV-1a over `bytes`, continuing from `h`. Chosen for zero
/// dependencies and determinism — this is a change detector, not a
/// security boundary.
fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Every file under `dir`, recursively, in a deterministic order.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Hash the wire-defining inputs into the fingerprint the handshake
/// negotiates. Over-approximates on purpose (a comment edit in a wire
/// crate changes it): a spurious "restart the daemon" is cheap, while
/// a wire change hiding under an unchanged number silently corrupts
/// every frame.
fn protocol_fingerprint(manifest_dir: &Path) -> u32 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let inputs = [
        manifest_dir.join("src"),
        manifest_dir.join("../core/src"),
        manifest_dir.join("../../Cargo.lock"),
    ];
    for input in &inputs {
        println!("cargo:rerun-if-changed={}", input.display());
        let files = if input.is_dir() {
            files_under(input)
        } else {
            vec![input.clone()]
        };
        for file in files {
            if let Some(name) = file.file_name() {
                h = fnv1a(h, name.to_string_lossy().as_bytes());
            }
            if let Ok(contents) = std::fs::read(&file) {
                h = fnv1a(h, &contents);
            }
        }
    }
    (h ^ (h >> 32)) as u32
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    println!(
        "cargo:rustc-env=LAZYBOX_PROTOCOL_FINGERPRINT={}",
        protocol_fingerprint(&manifest_dir)
    );

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
