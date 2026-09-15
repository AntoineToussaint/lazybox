//! Test-binary isolation from the developer's real `~/.lazybox` (#1751).
//!
//! `lazybox_config::Config::load()` resolves through `LAZYBOX_HOME`, in a
//! crate where `cfg(test)` is never active for a *dependent's* test run —
//! so nothing in the library can tell a test from the daemon, and a test
//! that forgets to redirect reads (or rewrites) the real config (#1539).
//! On a dev box that file is also being rewritten by a live daemon, so the
//! failure is a flake that blames whatever the test was actually about.
//!
//! The one hook that beats the harness to every test in a binary is a
//! before-main `#[ctor]`, and a test binary is the unit of isolation: a
//! sandbox installed in `src/lib.rs` never reaches `tests/*.rs`, which
//! links the non-test library as an external crate. This test walks every
//! crate that depends on `lazybox-config` and requires the sandbox in each
//! of its test binaries — the unit binary through its source tree, each
//! integration binary through `mod common;`. Per-test redirects under a
//! lock (`PinnedHome` and friends) layer on top for tests that need a home
//! of their own; the ctor is the floor beneath them, so a binary that
//! reaches config at all can never reach the real one by default.
//!
//! Lives in `lazybox-core` beside `dep_rules.rs` for the same reason: core
//! sits below everything it audits.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// Whether `manifest` lists `lazybox-config` under `[dependencies]` — the
/// production edge, not a dev-only one, since it is the library's own
/// `Config::load()` calls that reach the real home.
fn depends_on_config(manifest: &str) -> bool {
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if in_dependencies && line.starts_with("lazybox-config") {
            return true;
        }
    }
    false
}

fn installs_sandbox(body: &str) -> bool {
    body.contains("#[ctor::ctor]") && body.contains("set_var(\"LAZYBOX_HOME\"")
}

fn has_tests(body: &str) -> bool {
    body.contains("#[test]") || body.contains("#[tokio::test")
}

#[test]
fn every_test_binary_that_can_reach_config_sandboxes_lazybox_home() {
    let crates = workspace_root().join("crates");
    let mut missing = Vec::new();

    for entry in fs::read_dir(&crates).expect("read crates/").flatten() {
        let krate = entry.path();
        let manifest = krate.join("Cargo.toml");
        if !manifest.is_file() || !depends_on_config(&read(&manifest)) {
            continue;
        }
        let name = krate.file_name().unwrap_or_default().to_string_lossy();
        let src = krate.join("src");
        let bin_dir = src.join("bin");

        // The lib (or the sole bin) target: every `src/**` file outside
        // `src/bin/` belongs to it.
        let tree: Vec<PathBuf> = rust_sources(&src)
            .into_iter()
            .filter(|path| !path.starts_with(&bin_dir))
            .collect();
        let bodies: Vec<String> = tree.iter().map(|path| read(path)).collect();
        if bodies.iter().any(|body| has_tests(body))
            && !bodies.iter().any(|body| installs_sandbox(body))
        {
            missing.push(format!(
                "{name}: the unit-test binary has tests but no `#[ctor::ctor]` \
                 sandbox under `src/` (see `config_sandbox` in crates/server/src/lib.rs)"
            ));
        }

        // Each `src/bin/*.rs` is its own test binary.
        for bin in rust_sources(&bin_dir) {
            let body = read(&bin);
            if has_tests(&body) && !installs_sandbox(&body) {
                missing.push(format!(
                    "{name}: {} has tests but no `#[ctor::ctor]` sandbox",
                    bin.strip_prefix(&krate).unwrap_or(&bin).display()
                ));
            }
        }

        // Each `tests/*.rs` is its own binary and links the library without
        // `cfg(test)`, so it needs the sandbox linked in through `mod common;`.
        let tests = krate.join("tests");
        let common = tests.join("common").join("mod.rs");
        let Ok(entries) = fs::read_dir(&tests) else {
            continue;
        };
        let mut integration: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .collect();
        integration.sort();
        if integration.is_empty() {
            continue;
        }
        if !common.is_file() || !installs_sandbox(&read(&common)) {
            missing.push(format!(
                "{name}: tests/common/mod.rs must install the `#[ctor::ctor]` \
                 LAZYBOX_HOME sandbox (see crates/server/tests/common/mod.rs)"
            ));
        }
        for file in integration {
            if !read(&file).contains("mod common;") {
                missing.push(format!(
                    "{name}: {} lacks `mod common;`, so its binary can reach the \
                     real ~/.lazybox",
                    file.strip_prefix(&krate).unwrap_or(&file).display()
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "test binaries that can reach the real config without a sandbox:\n  {}",
        missing.join("\n  ")
    );
}

/// The scan only means something if it recognizes the sandboxes that exist:
/// the reference copy it points people at has to pass its own check.
#[test]
fn the_reference_sandbox_is_recognized() {
    let root = workspace_root();
    assert!(installs_sandbox(&read(
        &root.join("crates/server/src/lib.rs")
    )));
    assert!(installs_sandbox(&read(
        &root.join("crates/server/tests/common/mod.rs")
    )));
    assert!(depends_on_config(
        "[package]\nname = \"x\"\n[dependencies]\nlazybox-config = { workspace = true }\n"
    ));
    assert!(!depends_on_config(
        "[package]\nname = \"x\"\n[dev-dependencies]\nlazybox-config = { workspace = true }\n"
    ));
}
