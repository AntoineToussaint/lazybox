//! Packaged systemd units (#887) enforcement.
//!
//! `contrib/systemd/` ships templated units so a remote box auto-starts
//! the daemon on boot. A unit whose `ExecStart` names a `server`
//! subcommand that no longer exists would fail silently at boot on the
//! box, far from anyone watching. This test keeps each unit honest: its
//! `ExecStart` must invoke a real `lazybox server <sub>` arm (parsed from
//! `tui-boot`'s dispatch), and each must carry the two hardening
//! guarantees the issue scope requires — restart-on-crash and a
//! per-user `LAZYBOX_HOME`.
//!
//! Lives in `lazybox-core` beside `dep_rules.rs` / `regression_ledger.rs`
//! for the same reason: core sits below everything it audits.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

/// Parse `Key=Value` lines from a unit file into a multimap (a unit may
/// repeat a key, e.g. two `Environment=` lines).
fn unit_directives(body: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.entry(key.trim().to_string())
                .or_default()
                .push(value.trim().to_string());
        }
    }
    map
}

/// The `server` subcommand tokens `tui-boot` actually dispatches, e.g.
/// `start`, `api`. Parsed from the `Some("…") =>` arms so a renamed
/// subcommand fails this test instead of the box at boot.
fn dispatched_server_subcommands(main_rs: &str) -> Vec<String> {
    let Some(start) = main_rs.find("async fn server_subcommand") else {
        panic!("server_subcommand not found in tui-boot main.rs");
    };
    // The match block ends at the first arm that isn't a subcommand
    // literal; the wildcard `_ =>` (usage) terminates it. Scan a bounded
    // window forward from the fn.
    let window = &main_rs[start..(start + 800).min(main_rs.len())];
    let mut subs = Vec::new();
    for chunk in window.split("Some(\"").skip(1) {
        if let Some(end) = chunk.find('"') {
            subs.push(chunk[..end].to_string());
        }
    }
    assert!(
        !subs.is_empty(),
        "parsed no server subcommands — did the dispatch shape change?"
    );
    subs
}

#[test]
fn packaged_units_track_real_subcommands_and_hardening() {
    let root = workspace_root();
    let main_rs = fs::read_to_string(root.join("crates/tui-boot/src/main.rs"))
        .expect("read tui-boot main.rs");
    let subcommands = dispatched_server_subcommands(&main_rs);

    // (unit file, expected `server` subcommand it must invoke).
    let units = [
        ("lazybox-daemon@.service", "start"),
        ("lazybox-api@.service", "api"),
    ];

    for (file, expected_sub) in units {
        let path = root.join("contrib/systemd").join(file);
        let body =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let directives = unit_directives(&body);

        let exec = directives
            .get("ExecStart")
            .and_then(|v| v.first())
            .unwrap_or_else(|| panic!("{file}: no ExecStart"));
        assert!(
            exec.contains("lazybox server ") && exec.contains(expected_sub),
            "{file}: ExecStart {exec:?} should run `lazybox server {expected_sub}`"
        );
        assert!(
            subcommands.iter().any(|s| s == expected_sub),
            "{file}: ExecStart invokes `server {expected_sub}` but tui-boot \
             dispatches only {subcommands:?} — a renamed subcommand would \
             break this unit at boot"
        );

        // Restart-on-crash.
        let restart = directives
            .get("Restart")
            .and_then(|v| v.first())
            .unwrap_or_else(|| panic!("{file}: no Restart= (restart-on-crash required)"));
        assert!(
            restart == "on-failure" || restart == "always",
            "{file}: Restart={restart} is not a restart-on-crash policy"
        );

        // Distinct per-user LAZYBOX_HOME, baked into the template via %i.
        let per_user_home = directives
            .get("Environment")
            .map(|vals| {
                vals.iter()
                    .any(|v| v.starts_with("LAZYBOX_HOME=") && v.contains("%i"))
            })
            .unwrap_or(false);
        assert!(
            per_user_home,
            "{file}: expected a per-user Environment=LAZYBOX_HOME=…%i… so each \
             account gets a distinct home"
        );

        // Private /tmp: the daemon's default trace log is a fixed
        // /tmp/lazybox.log opened 0600, and init_tracing aborts the daemon
        // when that open fails. Without PrivateTmp two users race for the one
        // path and the second fails to boot. See lazybox-daemon@.service.
        let private_tmp = directives
            .get("PrivateTmp")
            .and_then(|v| v.first())
            .map(|v| v == "true" || v == "yes" || v == "on")
            .unwrap_or(false);
        assert!(
            private_tmp,
            "{file}: expected PrivateTmp=true so per-user daemons don't collide \
             on the shared /tmp/lazybox.log (the second to start would fail to \
             open the first's 0600 log and abort at boot)"
        );
    }
}
