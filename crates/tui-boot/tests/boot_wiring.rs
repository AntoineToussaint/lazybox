//! #1244 regression: BOTH boot paths — the embedded default and the
//! attach/remote client (`run_realm_client`) — must run the shared
//! client-config apply, and every exit path must flush the async
//! config-persist queue. The attach path historically skipped the
//! apply (booting an unseeded sidebar whose first star-toggle erased
//! `ui.focused_workspaces`) and never flushed, and the signal path
//! `exit()`ed past the flush entirely.
//!
//! `main.rs` wires an interactive terminal to a live daemon, so the
//! wiring is asserted structurally against the source — the same
//! technique the workspace dep-rules test uses — rather than by
//! booting a TUI in a test harness.

fn main_rs() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs");
    std::fs::read_to_string(path).expect("read main.rs")
}

/// The body of the named top-level function: from its signature to the
/// next top-level item. Coarse, but stable — it only needs to answer
/// "does this path call X anywhere".
fn fn_body<'a>(src: &'a str, name: &str) -> &'a str {
    let sig = format!("fn {name}(");
    let start = src
        .find(&sig)
        .unwrap_or_else(|| panic!("fn {name} not found in main.rs"));
    let tail = &src[start + sig.len()..];
    let end = ["\nasync fn ", "\nfn ", "\npub fn ", "\nstruct ", "\nimpl "]
        .iter()
        .filter_map(|marker| tail.find(marker))
        .min()
        .unwrap_or(tail.len());
    &tail[..end]
}

/// Break A (#1244): one shared config-apply entry point, called by BOTH
/// boot paths. The daemon does not own ui.* view state — a client that
/// skips the apply boots unseeded and its first persisted toggle starts
/// from an empty list.
#[test]
fn both_boot_paths_apply_client_config() {
    let src = main_rs();
    for path in ["run_embedded_realm", "run_realm_client"] {
        assert!(
            fn_body(&src, path).contains("apply_client_config("),
            "{path} must call the shared Model::apply_client_config — \
             a boot path that skips it regresses #1244 (unseeded ui.* state)"
        );
    }
}

/// Break C (#1244): keystroke-persisted config rides a background
/// worker, so every way out of the process must flush it — the embedded
/// quit path (#1211), the attach-client teardown, and the signal path
/// (which `exit()`s without running destructors).
#[test]
fn every_exit_path_flushes_pending_config_saves() {
    let src = main_rs();
    for path in [
        "run_embedded_realm",
        "run_realm_client",
        "spawn_terminal_restore_on_signal",
    ] {
        assert!(
            fn_body(&src, path).contains("flush_pending_saves"),
            "{path} must flush pending config saves before the process exits (#1244)"
        );
    }
}
