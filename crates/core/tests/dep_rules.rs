//! Workspace dependency-rule enforcement.
//!
//! CLAUDE.md states the layering rules ("core libraries must not
//! depend on each other", "provider crates depend on core + auth
//! only") but nothing enforced them, and reality drifted (store →
//! core is now blessed). This test parses every `crates/*/Cargo.toml`
//! and pins the ACTUAL internal `lazybox-*` dependency graph against
//! an explicit allowlist: any new internal edge — in `[dependencies]`,
//! `[dev-dependencies]`, or `[build-dependencies]` — fails the build
//! until it's deliberately added here. Removing an edge fails too, so
//! the allowlist can't rot into fiction.
//!
//! Lives in `lazybox-core` because core has no internal dependencies:
//! the guard itself can never be part of a cycle it polices. Parsing
//! is deliberate string scanning — no toml crate — since dependency
//! lines in this workspace are single-line `name = { ... }` entries.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

/// The blessed internal dependency graph: package name → set of
/// `lazybox-*` packages it may reference anywhere in its manifest.
/// UPDATE DELIBERATELY: adding an edge here is an architectural
/// decision, not a formality. Notable calls already made:
/// - `lazybox-store` → `lazybox-core` is accepted reality (the store
///   speaks `WorkspaceKey`/`ProjectKey`), superseding CLAUDE.md's
///   original "core libraries never depend on each other" for this
///   one edge. core ↔ auth ↔ store must otherwise stay disjoint.
/// - Provider crates (`gh`, `linear`, `slack`) stay within core + auth;
///   Slack currently needs only auth.
fn allowed_graph() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    fn set(deps: &[&'static str]) -> BTreeSet<&'static str> {
        deps.iter().copied().collect()
    }
    BTreeMap::from([
        ("lazybox-agents", set(&["lazybox-ipc"])),
        ("lazybox-auth", set(&[])),
        ("lazybox-config", set(&["lazybox-core"])),
        ("lazybox-core", set(&[])),
        // The E2E channel crate (Noise/X25519) wraps a bare async byte
        // stream; it speaks no lazybox wire types, so it carries no
        // internal deps.
        ("lazybox-e2e-channel", set(&[])),
        // The relay's entitlement gate (#895): a standalone seam with no
        // internal deps yet. The relay (#893) will hold it and real
        // licensing (#648) will back it — both add edges deliberately then.
        ("lazybox-entitlement", set(&[])),
        ("lazybox-gh", set(&["lazybox-auth", "lazybox-core"])),
        ("lazybox-git-ops", set(&["lazybox-core"])),
        ("lazybox-ipc", set(&["lazybox-core"])),
        ("lazybox-linear", set(&["lazybox-auth", "lazybox-core"])),
        // The rendezvous relay is a standalone, codefly-hosted deployable
        // and a dumb ciphertext forwarder — it depends on no internal
        // crate so it can build and deploy on its own.
        ("lazybox-relay", set(&[])),
        (
            "lazybox-server",
            set(&[
                "lazybox-agents",
                "lazybox-auth",
                "lazybox-config",
                "lazybox-core",
                "lazybox-gh",
                "lazybox-git-ops",
                "lazybox-ipc",
                "lazybox-linear",
                "lazybox-slack",
                "lazybox-store",
                // server hosts the desktop API gateway + the desktop
                // contract generator. The desktop's inbox view-model
                // (ComputeOutcome / VisibleRow / SortMode / WorkspaceKind /
                // RepoSummary from tui-core, #732) crosses the gateway as a
                // `DesktopStreamMessage::Inbox` variant and is ts-exported
                // here. tui-core stays client-free (its deps — agents /
                // config / core / ipc — are already server deps), so this
                // edge introduces no cycle.
                "lazybox-tui-core",
            ]),
        ),
        ("lazybox-slack", set(&["lazybox-auth"])),
        ("lazybox-store", set(&["lazybox-core"])),
        // The thin UI library (#548): wire crate + pure TUI logic +
        // embedded terminal + config + core, nothing wider. A
        // `use lazybox_store::…` / `lazybox_server::…` here is now a
        // compile error, not a blessed edge.
        (
            "lazybox-tui",
            set(&[
                "lazybox-config",
                "lazybox-core",
                "lazybox-ipc",
                "lazybox-tui-core",
                "lazybox-tui-term",
            ]),
        ),
        // The boot crate carries the wide daemon/provider/store wiring the
        // UI library must not: it hosts the `lazybox` binary and depends on
        // the UI library plus everything the UI library gave up.
        (
            "lazybox-tui-boot",
            set(&[
                "lazybox-agents",
                "lazybox-config",
                "lazybox-core",
                "lazybox-gh",
                "lazybox-git-ops",
                "lazybox-ipc",
                "lazybox-linear",
                "lazybox-relay",
                "lazybox-server",
                "lazybox-slack",
                "lazybox-store",
                "lazybox-tui",
                "lazybox-tui-core",
            ]),
        ),
        // tui-core is the UI library's gateway to agent metadata (badges,
        // display names) and the prompt-trim helper (#548), so it depends
        // on agents — keeping that edge off the UI library itself. It also
        // hosts the client-free inbox view logic (#731), which scores
        // attention against `lazybox_config::AttentionConfig` — hence the
        // config edge (config → core only, so no cycle).
        (
            "lazybox-tui-core",
            set(&[
                "lazybox-agents",
                "lazybox-config",
                "lazybox-core",
                "lazybox-ipc",
            ]),
        ),
        ("lazybox-tui-term", set(&[])),
        // Vendored libghostty crates: no lazybox-* references allowed.
        ("libghostty-vt", set(&[])),
        ("libghostty-vt-sys", set(&[])),
    ])
}

/// First `name = "..."` in the manifest — the `[package]` name (it
/// precedes `[lib]`/`[[bin]]` sections in every manifest here).
fn package_name(manifest: &str, path: &Path) -> String {
    manifest
        .lines()
        .map(str::trim)
        .find_map(|l| {
            l.strip_prefix("name")
                .map(str::trim_start)
                .and_then(|r| r.strip_prefix('='))
                .map(|r| r.trim().trim_matches('"').to_string())
        })
        .unwrap_or_else(|| panic!("no package name in {}", path.display()))
}

/// Every `lazybox-*` package referenced as a dependency key anywhere
/// in the manifest (dependencies, dev-dependencies,
/// build-dependencies). Two legal Cargo spellings are recognized:
/// - inline entries — the package name at line start followed by `=`
///   or `.` (dotted form, `lazybox-x.workspace = true`);
/// - table headers — `[dependencies.lazybox-x]`,
///   `[dev-dependencies.lazybox-x]`, and target-scoped
///   `[target.'cfg(...)'.dependencies.lazybox-x]`. These used to be
///   invisible to the guard: a silent evasion vector for the whole
///   allowlist.
fn internal_deps(manifest: &str) -> BTreeSet<String> {
    fn dep_name(s: &str) -> String {
        s.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect()
    }
    let mut out = BTreeSet::new();
    for line in manifest.lines() {
        let line = line.trim_start();
        if line.starts_with("lazybox-") {
            let name = dep_name(line);
            let rest = line[name.len()..].trim_start();
            if rest.starts_with('=') || rest.starts_with('.') {
                out.insert(name);
            }
        } else if line.starts_with('[')
            && let Some(idx) = line.find("dependencies.lazybox-")
        {
            out.insert(dep_name(&line[idx + "dependencies.".len()..]));
        }
    }
    out
}

#[test]
fn internal_dependency_graph_matches_the_allowlist() {
    let allowed = allowed_graph();
    let crates_dir = workspace_root().join("crates");

    let mut actual: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in fs::read_dir(&crates_dir).expect("read crates/") {
        let dir = entry.expect("dir entry").path();
        let manifest_path = dir.join("Cargo.toml");
        let Ok(manifest) = fs::read_to_string(&manifest_path) else {
            continue; // not a crate dir
        };
        let name = package_name(&manifest, &manifest_path);
        // The package's own name in `name = "lazybox-x"` never parses
        // as a dep (that line starts with `name`), so no self-filter
        // is needed; keep one anyway for safety.
        let mut deps = internal_deps(&manifest);
        deps.remove(&name);
        actual.insert(name, deps);
    }

    // Every crate present, no crate missing.
    let actual_names: BTreeSet<&str> = actual.keys().map(String::as_str).collect();
    let allowed_names: BTreeSet<&str> = allowed.keys().copied().collect();
    assert_eq!(
        actual_names,
        allowed_names,
        "crates/ contents changed — update allowed_graph() in {} deliberately",
        file!()
    );

    // Exact edge equality per crate, both directions.
    for (name, deps) in &actual {
        let expected: BTreeSet<String> = allowed[name.as_str()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            deps,
            &expected,
            "internal dependencies of `{name}` drifted from the allowlist.\n\
             actual:  {deps:?}\n\
             allowed: {expected:?}\n\
             If this edge is intentional, add it to allowed_graph() in {} \
             WITH a rationale — layering rules are decisions, not defaults.",
            file!()
        );
    }
}

/// The deleted `lazybox-events` bus must not creep back into any
/// manifest (carried over from the retired gh-provider dep test —
/// the allowlist above would also catch a real dependency line, but
/// this covers commented-out stragglers and feature strings too).
#[test]
fn no_crate_references_lazybox_events() {
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

// ── internal_deps parser unit tests ─────────────────────────────────
// The guard is only as strong as its parser: any legal Cargo spelling
// it misses is a silent evasion vector, so each spelling gets pinned.

fn set_of(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// The historical spellings: `name = { ... }` and dotted
/// `name.workspace = true` under a `[dependencies]` section.
#[test]
fn internal_deps_parses_inline_entries() {
    let manifest = r#"
[package]
name = "demo"

[dependencies]
lazybox-core = { path = "../core" }
lazybox-ipc.workspace = true
serde = "1"
"#;
    assert_eq!(
        internal_deps(manifest),
        set_of(&["lazybox-core", "lazybox-ipc"])
    );
}

/// Regression (parser loophole): the equally legal table-header form
/// `[dependencies.lazybox-x]` was invisible — the old parser only
/// matched lines STARTING with `lazybox-`.
#[test]
fn internal_deps_parses_dependency_table_headers() {
    let manifest = r#"
[package]
name = "demo"

[dependencies.lazybox-auth]
path = "../auth"
"#;
    assert_eq!(internal_deps(manifest), set_of(&["lazybox-auth"]));
}

/// Same loophole, `[dev-dependencies.lazybox-x]` spelling.
#[test]
fn internal_deps_parses_dev_dependency_table_headers() {
    let manifest = r#"
[package]
name = "demo"

[dev-dependencies.lazybox-store]
path = "../store"
"#;
    assert_eq!(internal_deps(manifest), set_of(&["lazybox-store"]));
}

/// Same loophole, target-scoped spelling:
/// `[target.'cfg(...)'.dependencies.lazybox-x]`.
#[test]
fn internal_deps_parses_target_scoped_table_headers() {
    let manifest = r#"
[package]
name = "demo"

[target.'cfg(unix)'.dependencies.lazybox-config]
path = "../config"
"#;
    assert_eq!(internal_deps(manifest), set_of(&["lazybox-config"]));
}

/// Non-dependency mentions must NOT match: comments (both a prose
/// mention of the table form and a commented-out inline entry) and
/// the package's own `name =` line.
#[test]
fn internal_deps_ignores_non_dependency_mentions() {
    let manifest = r#"
[package]
name = "lazybox-demo"

# see [dependencies.lazybox-core] docs for why this is NOT a dep
# lazybox-slack = { path = "../slack-provider" }

[dependencies]
serde = "1"
"#;
    assert_eq!(internal_deps(manifest), BTreeSet::new());
}
