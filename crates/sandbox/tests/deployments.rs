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
}

/// Split the `startup.sh.tftpl` source into (inside-the-install-block,
/// everything-else). `templatefile` gates the daemon-install block on
/// `%{ if install_lazybox ~} … %{ endif ~}`, so a render with the flag off
/// drops exactly that span. We can't render without a terraform binary in
/// CI, so we assert against the block boundaries directly: the install
/// markers appear inside the guard and nowhere outside it — proving they
/// appear/disappear with the flag.
fn split_install_block(tftpl: &str) -> (String, String) {
    let start = tftpl
        .find("%{ if install_lazybox ~}")
        .expect("startup template guards the install on install_lazybox");
    // The block nests an `%{ if lazybox_repo_token_secret … }`, so match the
    // balanced `endif` rather than the first one.
    let mut depth = 0usize;
    let mut idx = start;
    let bytes = tftpl.as_bytes();
    let end = loop {
        assert!(idx < tftpl.len(), "install block is never closed");
        if tftpl[idx..].starts_with("%{ if ") {
            depth += 1;
            idx += "%{ if ".len();
        } else if tftpl[idx..].starts_with("%{ endif ~}") {
            depth -= 1;
            if depth == 0 {
                break idx + "%{ endif ~}".len();
            }
            idx += "%{ endif ~}".len();
        } else {
            idx += 1;
            // Stay on a char boundary (the template is ASCII, but be safe).
            while idx < tftpl.len() && (bytes[idx] & 0xC0) == 0x80 {
                idx += 1;
            }
        }
    };
    (
        tftpl[start..end].to_string(),
        format!("{}{}", &tftpl[..start], &tftpl[end..]),
    )
}

#[test]
fn startup_template_installs_the_daemon_only_under_the_flag() {
    let tftpl = std::fs::read_to_string(sandbox_dir().join("gcp/startup.sh.tftpl"))
        .expect("read startup.sh.tftpl");
    let (inside, outside) = split_install_block(&tftpl);

    // The whole toolchain-install path lives inside the guarded block, so a
    // "bring your own stack" box (install_lazybox = false) renders none of it.
    for marker in [
        "/opt/lazybox/src",       // the daemon build checkout
        "${lazybox_git_sha}",     // pinned to the client's commit
        "lazybox-box-install.sh", // the build+wire installer
        "lazybox-build.service",  // the detached build unit
        "systemctl enable --now lazybox-build.service",
        "TimeoutStartSec=3600", // don't reap the 10-min build
    ] {
        assert!(inside.contains(marker), "install block missing {marker:?}");
        assert!(
            !outside.contains(marker),
            "{marker:?} leaks outside the install_lazybox guard — it would run on an opt-out box"
        );
    }

    // The private-repo clone must never put the token in the URL.
    assert!(
        inside.contains("GIT_ASKPASS"),
        "private clone should feed the token via GIT_ASKPASS, not the URL"
    );
    assert!(
        !inside.contains("https://x-access-token:"),
        "the clone URL must stay token-free"
    );
}
