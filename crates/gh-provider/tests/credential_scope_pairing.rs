//! Every GitHub credential resolution must key its cache by the same
//! host it hands to `gh auth token`.
//!
//! `credential_chain(host)` threads `--hostname <host>` through to `gh`,
//! and `credential_scope(host)` folds that host into
//! `CredentialChain`'s process-global, scope-keyed cache. The two are a
//! pair: a call site that passes a host to the chain but resolves with
//! the bare `SOURCE` constant re-opens exactly the bug the pair exists
//! to close — whichever host resolved first serves its token to every
//! other host until the 5-minute cache entry expires. Nothing in the
//! type system couples them, so this pins it in source.
//!
//! Scanning source text (rather than testing behavior) is deliberate:
//! the hazard is a *new call site* getting the pairing wrong, which no
//! runtime test of the existing sites can catch. `dep_rules.rs` polices
//! the workspace the same way, for the same reason.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

/// Every `.rs` file under the workspace's source trees, including the
/// desktop shell — which lives outside the cargo workspace and so is
/// missed by `cargo build --workspace`, the very reason its call site
/// broke silently once already.
fn rust_sources() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut out = Vec::new();
    for top in ["crates", "apps"] {
        collect(&root.join(top), &mut out);
    }
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` holds build artifacts, and vendored C/Zig trees
            // hold no Rust call sites worth policing.
            if !matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some("target") | Some("node_modules") | Some("vendor")
            ) {
                collect(&path, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The host expression inside a `credential_chain(...)` call — the text
/// between the outermost parentheses.
fn call_argument(after: &str) -> Option<&str> {
    let mut depth = 0usize;
    for (i, ch) in after.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&after[1..i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[test]
fn every_gh_credential_chain_call_resolves_with_a_matching_credential_scope() {
    // Skipped: the chain's own definition (its unit tests build chains to
    // inspect the provider list, not to resolve), and this file, whose
    // needle appears here as a string literal.
    let root = workspace_root();
    let skip = [
        root.join("crates/gh-provider/src/lib.rs"),
        root.join(file!()),
    ];
    let mut offenders: Vec<String> = Vec::new();

    for path in rust_sources() {
        if skip.contains(&path) {
            continue;
        }
        let Ok(src) = fs::read_to_string(&path) else {
            continue;
        };
        for (lineno, line) in src.lines().enumerate() {
            // Only the GitHub chain is host-scoped; Linear's takes no host.
            let Some(idx) = line.find("gh::credential_chain(") else {
                continue;
            };
            let after = &line[idx + "gh::credential_chain".len()..];
            let Some(host) = call_argument(after).map(str::trim) else {
                continue;
            };
            // The resolve may sit on this line or the next few — the call
            // sites wrap. Look at a small window rather than one line.
            let window: String = src
                .lines()
                .skip(lineno)
                .take(4)
                .collect::<Vec<_>>()
                .join(" ");
            let Some(scope_idx) = window.find("credential_scope(") else {
                offenders.push(format!(
                    "{}:{} — credential_chain({host}) resolves without credential_scope",
                    path.display(),
                    lineno + 1,
                ));
                continue;
            };
            let scope_arg = call_argument(&window[scope_idx + "credential_scope".len()..])
                .map(str::trim)
                .unwrap_or_default();
            if scope_arg != host {
                offenders.push(format!(
                    "{}:{} — credential_chain({host}) paired with credential_scope({scope_arg})",
                    path.display(),
                    lineno + 1,
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "every `credential_chain(host)` must resolve with `credential_scope(host)` \
         for the SAME host expression, or two hosts share one cache entry and \
         cross-serve each other's tokens:\n  {}",
        offenders.join("\n  "),
    );
}
