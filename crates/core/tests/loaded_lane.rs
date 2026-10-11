//! The merge queue's `loaded` lane (#1751) runs the whole suite under CPU
//! oversubscription to catch tests whose fixed budgets only an idle box
//! meets. Those fail by their own assertion at any runner deadline; what a
//! short runner deadline adds is killing tests doing 6–8s of honest
//! subprocess work, which on a 3× oversubscribed runner cross the 10s `ci`
//! bound (measured 6.4–7.9s at load 25–80 on a 15-core box). The lane
//! therefore has its own nextest profile with a longer ceiling, and the
//! wiring — profile, script default, workflow — is pinned here so a future
//! "simplify to `--profile ci`" reintroduces a lane that is red on every
//! merge and teaches everyone to ignore it.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// `slow-timeout` period, in seconds, of the named nextest profile.
fn profile_period_secs(nextest: &str, profile: &str) -> u64 {
    let header = format!("[profile.{profile}]");
    let block = nextest
        .split_once(&header)
        .unwrap_or_else(|| panic!("{header} is declared in .config/nextest.toml"))
        .1;
    block
        .lines()
        .take_while(|line| !line.trim_start().starts_with("[profile."))
        .find_map(|line| {
            let rest = line.trim().strip_prefix("slow-timeout")?;
            let secs = rest.split_once("period = \"")?.1.split_once("s\"")?.0;
            secs.parse().ok()
        })
        .unwrap_or_else(|| panic!("{header} declares a slow-timeout period in whole seconds"))
}

/// A `tui-boot` test that execs the built `lazybox` binary pays a cold-cache
/// page-in of a ~213 MB debug build — measured 13s on a loaded box against a
/// 10s default — so the wall clock is a property of the build, not of the
/// behaviour under test.
///
/// The override for that used to name `notification_click` alone while three
/// sibling binaries carried the same cost on the default budget.
/// `hosted_relay_script` was rediagnosed four times in one day (7.1s idle,
/// 9.7s at load 22, timing out above that, on clean `main` so it looked like
/// nobody's diff) before it blocked a release gate. This pins the filter to
/// the cause: every test binary that execs the binary gets the budget, so the
/// list cannot fall behind a new one.
#[test]
fn every_binary_that_execs_the_lazybox_binary_gets_the_cold_exec_budget() {
    let root = workspace_root();
    let tests_dir = root.join("crates/tui-boot/tests");
    let mut execs: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).expect("read crates/tui-boot/tests") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read test source");
        if source.contains("CARGO_BIN_EXE_lazybox") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("test binary name")
                .to_string();
            execs.push(name);
        }
    }
    assert!(
        !execs.is_empty(),
        "no tui-boot test execs CARGO_BIN_EXE_lazybox any more; this guard has stopped working"
    );

    let nextest = read(".config/nextest.toml");
    let covered: Vec<&String> = execs
        .iter()
        .filter(|name| nextest.contains(&format!("binary({name})")))
        .collect();
    let missing: Vec<&String> = execs
        .iter()
        .filter(|name| !nextest.contains(&format!("binary({name})")))
        .collect();
    assert!(
        missing.is_empty(),
        "these tui-boot test binaries exec the ~213 MB `lazybox` debug binary and would be \
         killed by the 10s default on a loaded box — add them to the cold-exec override in \
         .config/nextest.toml: {missing:?} (already covered: {covered:?})"
    );
}

#[test]
fn the_loaded_lane_runs_under_its_own_profile() {
    let nextest = read(".config/nextest.toml");
    let loaded = profile_period_secs(&nextest, "loaded");
    let ci = profile_period_secs(&nextest, "ci");
    assert!(
        loaded >= 3 * ci,
        "the loaded profile's runner ceiling ({loaded}s) must give honest subprocess work \
         the headroom 3x oversubscription costs over the ci profile's {ci}s"
    );

    let script = read("scripts/test-under-load.sh");
    assert!(
        script.contains("profile=loaded"),
        "scripts/test-under-load.sh must default to the loaded profile"
    );

    let workflow = read(".github/workflows/ci.yml");
    let job = workflow
        .split_once("\n  loaded:")
        .expect("ci.yml declares the `loaded` job")
        .1;
    let job = job.split("\n  clippy:").next().unwrap_or(job);
    assert!(
        job.contains("scripts/test-under-load.sh --profile loaded"),
        "the loaded job must run the script under the loaded profile, not ci"
    );
    assert!(
        job.contains("--load-factor"),
        "the loaded job must set its load factor explicitly; the Makefile default is 0 \
         for the shared dev box"
    );
}
