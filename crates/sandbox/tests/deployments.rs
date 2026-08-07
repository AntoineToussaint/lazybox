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
