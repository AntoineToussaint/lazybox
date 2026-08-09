//! Roster-drift guard for the box stop-on-idle policy (#978).
//!
//! `contrib/box-lifecycle/lazybox-idle-stop.sh` keeps a box alive while a
//! watched agent CLI is working. Its `LAZYBOX_IDLE_AGENT_PROCS` default is a
//! hand-written copy of the agent registry — so a new built-in agent whose CLI
//! isn't in that list would have its sessions silently reaped mid-task. This
//! test derives the required process names from `Registry::default_builtins()`
//! and fails the build if the script's default isn't a superset, forcing the
//! two to stay in sync. Operator-added `GenericCli` agents are covered by the
//! `/etc/lazybox/idle-stop.env` override, documented in the box-lifecycle
//! README — they can't be enumerated from code.
//!
//! Lives in `crates/agents/tests/` rather than beside the other box-lifecycle
//! checks in `crates/core/`: core may not depend on `crates/agents` (dep
//! rules), and this assertion needs the live registry.

use std::path::{Path, PathBuf};

use lazybox_agents::{Registry, SpawnCtx};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

/// The space-separated default of `LAZYBOX_IDLE_AGENT_PROCS`, parsed out of the
/// `AGENT_PROCS="${LAZYBOX_IDLE_AGENT_PROCS:-...}"` line in the script.
fn watched_default(script: &str) -> Vec<String> {
    const MARKER: &str = "LAZYBOX_IDLE_AGENT_PROCS:-";
    let start = script
        .find(MARKER)
        .expect("script must define a LAZYBOX_IDLE_AGENT_PROCS default")
        + MARKER.len();
    let rest = &script[start..];
    let end = rest
        .find('}')
        .expect("LAZYBOX_IDLE_AGENT_PROCS default must be `${...:-...}`");
    rest[..end].split_whitespace().map(str::to_string).collect()
}

fn basename(arg: &str) -> &str {
    arg.rsplit('/').next().unwrap_or(arg)
}

#[test]
fn watched_agents_cover_every_builtin_spawn_command() {
    let script = std::fs::read_to_string(
        workspace_root().join("contrib/box-lifecycle/lazybox-idle-stop.sh"),
    )
    .expect("read lazybox-idle-stop.sh");
    let watched = watched_default(&script);

    let ctx = SpawnCtx::default();
    let registry = Registry::default_builtins();
    let mut ids: Vec<String> = registry.ids().map(str::to_string).collect();
    ids.sort();

    for id in ids {
        let agent = registry.get(&id).expect("registry id resolves");
        let argv = agent.spawn(&ctx);
        let cmd = basename(argv.first().expect("every agent spawns a command"));
        assert!(
            watched.contains(&cmd.to_string()),
            "built-in agent {id:?} spawns {cmd:?}, which is missing from the \
             idle-stop watched list {watched:?}. Add it to the \
             LAZYBOX_IDLE_AGENT_PROCS default in \
             contrib/box-lifecycle/lazybox-idle-stop.sh so its sessions aren't \
             reaped while working."
        );
    }
}
