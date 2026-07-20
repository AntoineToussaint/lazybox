//! Regression-ledger enforcement (#410).
//!
//! `docs/regression-ledger.md` names, for every historically-recurring
//! bug, the test that guards it. A ledger row nobody re-validates is
//! prose; this test makes each row load-bearing by parsing every
//! `` `path/to/file.rs::test_name` `` reference and failing the build
//! when the file or the named test function no longer exists — renaming
//! or deleting a guard forces a deliberate ledger update.
//!
//! Lives in `lazybox-core` beside `dep_rules.rs` for the same reason:
//! core sits below everything it audits.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

/// Every `` `<file>.rs::<test>` `` reference in the ledger, in order.
fn ledger_references(ledger: &str) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    for (i, chunk) in ledger.split('`').enumerate() {
        // Odd chunks are inside backticks.
        if i % 2 == 0 {
            continue;
        }
        let Some((path, test)) = chunk.split_once("::") else {
            continue;
        };
        if path.ends_with(".rs")
            && !test.is_empty()
            && test.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            refs.push((path.to_string(), test.to_string()));
        }
    }
    refs
}

#[test]
fn every_ledger_entry_names_an_existing_test() {
    let root = workspace_root();
    let ledger_path = root.join("docs/regression-ledger.md");
    let ledger = fs::read_to_string(&ledger_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", ledger_path.display()));

    let refs = ledger_references(&ledger);
    assert!(
        refs.len() >= 20,
        "the ledger should keep naming its guards; found only {} \
         `file.rs::test` references — did the format change without \
         updating this parser?",
        refs.len()
    );

    let mut missing = Vec::new();
    for (path, test) in &refs {
        let file = root.join(path);
        let Ok(source) = fs::read_to_string(&file) else {
            missing.push(format!("{path} (file missing) — referenced test `{test}`"));
            continue;
        };
        let defined =
            source.contains(&format!("fn {test}(")) || source.contains(&format!("fn {test} ("));
        if !defined {
            missing.push(format!("{path}::{test} (no `fn {test}` in file)"));
        }
    }
    assert!(
        missing.is_empty(),
        "docs/regression-ledger.md names guards that no longer exist — \
         either restore the test or deliberately update the ledger:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn parser_extracts_path_test_pairs() {
    let refs = ledger_references(
        "a `crates/x/tests/y.rs::some_test` and prose `not::a::path` \
         plus `crates/z.rs::another_one` end",
    );
    assert_eq!(
        refs,
        vec![
            ("crates/x/tests/y.rs".into(), "some_test".into()),
            ("crates/z.rs".into(), "another_one".into()),
        ]
    );
}
