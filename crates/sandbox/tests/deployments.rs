//! The shipped deployment recipes + the GCP Terraform module are
//! artifacts that live on disk far from anyone watching: a typo in
//! `obin.yaml`, a key the typed `DeploymentConfig` can't parse, or a
//! missing `.tf` file would only surface when someone tried to stamp a
//! real box. This test keeps them honest — the default embeds and parses,
//! obin's overlay deep-merges onto it into the values we expect, and the
//! module carries the files `GcpProvider` shells out against.

use std::path::{Path, PathBuf};

use lazybox_sandbox::Deployment;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").is_dir())
        .expect("workspace root with a crates/ dir")
        .to_path_buf()
}

fn sandbox_dir() -> PathBuf {
    workspace_root().join("terraform/sandbox")
}

#[test]
fn default_deployment_embeds_and_parses() {
    let d = Deployment::default_recipe().expect("embedded default parses");
    assert_eq!(d.config.name, "default");
    assert!(d.config.repo.is_none());
    assert!(d.config.workload_ports.is_empty());
}

#[test]
fn obin_overlay_deep_merges_onto_the_default() {
    let overlay = std::fs::read_to_string(sandbox_dir().join("deployments/obin.yaml"))
        .expect("read obin.yaml");
    let d = Deployment::with_overlay(&overlay).expect("obin overlay merges");

    // Overlay wins where it speaks…
    assert_eq!(d.config.name, "obin");
    assert_eq!(d.config.machine_type, "e2-standard-8");
    assert_eq!(d.config.workload_ports, vec![3000, 8082, 8787]);
    assert_eq!(d.config.repo.as_deref(), Some("obin-ai/obin-platform"));
    assert!(
        d.config
            .service_account_roles
            .contains(&"roles/aiplatform.user".to_string()),
        "obin's cross-project grants survive the merge"
    );

    // …and inherits the base where it stays silent.
    assert_eq!(d.config.image_family, "debian-12");
    assert!(d.config.enable_nat);
    assert!(
        d.config.packages.contains(&"git".to_string()),
        "base toolchain inherited"
    );
}

#[test]
fn gcp_module_ships_the_files_the_provider_drives() {
    let gcp = sandbox_dir().join("gcp");
    for file in [
        "main.tf",
        "variables.tf",
        "outputs.tf",
        "versions.tf",
        "startup.sh.tftpl",
    ] {
        assert!(
            gcp.join(file).exists(),
            "terraform/sandbox/gcp/{file} is missing"
        );
    }

    // The outputs the handle is built from must be declared, or
    // `terraform output -json` returns nothing for `ensure` to parse.
    let outputs = std::fs::read_to_string(gcp.join("outputs.tf")).expect("read outputs.tf");
    assert!(outputs.contains("output \"instance_name\""));
    assert!(outputs.contains("output \"zone\""));

    // The daemon-install vars (#977) must be declared and threaded into the
    // startup script, or the client's `-var install_lazybox=…`/`lazybox_git_sha=…`
    // would be rejected as unknown at apply time.
    let variables = std::fs::read_to_string(gcp.join("variables.tf")).expect("read variables.tf");
    assert!(variables.contains("variable \"install_lazybox\""));
    assert!(variables.contains("variable \"lazybox_git_sha\""));
    let main = std::fs::read_to_string(gcp.join("main.tf")).expect("read main.tf");
    assert!(main.contains("install_lazybox = var.install_lazybox"));
    assert!(main.contains("lazybox_git_sha = var.lazybox_git_sha"));
}

#[test]
fn startup_script_installs_the_daemon_behind_the_install_flag() {
    // The startup template can't be rendered without Terraform (and there's
    // no live TF in CI), so assert STRUCTURALLY that the whole daemon install
    // lives inside the `install_lazybox` guard: everything in the guarded
    // region appears only when the flag is true and vanishes when it's false
    // — exactly the appear/disappear the acceptance hinges on (#977).
    let tpl = std::fs::read_to_string(sandbox_dir().join("gcp/startup.sh.tftpl"))
        .expect("read startup.sh.tftpl");

    let start = tpl
        .find("%{ if install_lazybox ~}")
        .expect("startup template must gate the lazybox install on install_lazybox");
    let after = &tpl[start..];
    let end = after
        .find("%{ endif ~}")
        .expect("the gated block must be closed with endif");
    let block = &after[..end];

    // The pinned commit + user/build/supervise steps must be INSIDE the guard.
    assert!(
        block.contains("${lazybox_git_sha}"),
        "install must pin the client's commit"
    );
    assert!(
        block.contains("useradd"),
        "install must provision the dedicated daemon user"
    );
    assert!(
        block.contains("lazybox-build.sh"),
        "install must run the build helper"
    );
    // The heavy build must be handed to a SUPERVISED unit — never a bare `&`
    // (SIGHUP'd, #903) — with `--no-block` so a 10-minute build doesn't wedge
    // boot, and a generous timeout so systemd doesn't call it failed.
    assert!(
        block.contains("systemd-run"),
        "build must run under systemd"
    );
    assert!(
        block.contains("--no-block"),
        "the build must not block boot completion"
    );
    assert!(
        block.contains("TimeoutStartSec"),
        "the build unit needs a generous timeout for the first ~10min compile"
    );

    // …and NOT outside the guard, or the opt-out would still install a daemon.
    let outside = format!("{}{}", &tpl[..start], &after[end..]);
    assert!(
        !outside.contains("lazybox-build.sh"),
        "no unguarded daemon install"
    );
    assert!(
        !outside.contains("${lazybox_git_sha}"),
        "the pinned SHA is referenced only inside the guard"
    );
}

#[test]
fn e2b_template_bakes_the_remote_toolchain_and_memory_safe_startup() {
    let e2b = sandbox_dir().join("e2b");
    let dockerfile =
        std::fs::read_to_string(e2b.join("e2b.Dockerfile")).expect("read E2B Dockerfile");
    let start = std::fs::read_to_string(e2b.join("start.sh")).expect("read E2B start script");

    for tool in ["tmux", "git", "@anthropic-ai/claude-code", "lazybox"] {
        assert!(dockerfile.contains(tool), "E2B template must bake {tool}");
    }
    assert!(
        dockerfile.contains("lazybox-build.sh") && dockerfile.contains("build-sha"),
        "E2B template must carry the same build-stamp helper and stamp as GCP"
    );
    assert!(
        start.contains("lazybox server start"),
        "E2B startup must launch the daemon"
    );
    assert!(
        start.contains("ws-l:0.0.0.0:8081") && start.contains("tcp:127.0.0.1:22"),
        "E2B startup must expose SSH through its WebSocket proxy"
    );

    let ignore = std::fs::read_to_string(workspace_root().join(".dockerignore"))
        .expect("read template context exclusions");
    for excluded in [".git", ".env", "target", ".lazybox"] {
        assert!(
            ignore.lines().any(|line| line == excluded),
            "E2B build context must exclude {excluded}"
        );
    }

    let docs = std::fs::read_to_string(workspace_root().join("docs/e2b-provider-spike.md"))
        .expect("read E2B spike docs");
    assert!(
        docs.contains("127.0.0.1:8081"),
        "template readiness must include the SSH-over-WebSocket transport"
    );
}

#[test]
fn e2b_probe_checks_the_full_five_minute_process_boundary() {
    let probe = std::fs::read_to_string(workspace_root().join("scripts/e2b-pause-resume-spike.sh"))
        .expect("read E2B persistence probe");

    assert!(probe.contains("WAIT_SECONDS:-300"));
    assert!(probe.contains("tmux capture-pane"));
    assert!(probe.contains("grep -Fqx '$MARKER-1'"));
    assert!(probe.contains("tmux display-message") && probe.contains("pane_pid"));
    assert!(probe.contains("daemon.sock"));
    assert!(probe.contains("perceived_resume_ms"));
    assert!(probe.contains("resume_ms\" -ge 5000"));
    assert!(probe.contains("resume_deadline"));
    assert!(!probe.contains("until remote true"));
}
