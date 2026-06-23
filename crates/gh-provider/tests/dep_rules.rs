//! Workspace dependency-rule guards.
//!
//! `gh-provider` once pulled in the now-deleted `lazybox-events` crate,
//! violating "provider crates depend on core + auth only". This test
//! keeps the dead bus from creeping back into any crate's manifest.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

#[test]
fn no_crate_depends_on_lazybox_events() {
    let crates_dir = workspace_root().join("crates");
    let mut offenders = Vec::new();

    for entry in fs::read_dir(&crates_dir).expect("read crates/") {
        let manifest = entry.expect("dir entry").path().join("Cargo.toml");
        let Ok(contents) = fs::read_to_string(&manifest) else {
            continue;
        };
        if contents.contains("lazybox-events") || contents.contains("lazybox_events") {
            offenders.push(manifest);
        }
    }

    assert!(
        offenders.is_empty(),
        "the legacy `lazybox-events` bus is deleted; these manifests must not reference it: {offenders:?}",
    );
}
