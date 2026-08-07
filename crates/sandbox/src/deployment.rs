use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::provider::SandboxError;

/// The generic **default** deployment, embedded so it always ships with
/// the binary. A project's override (e.g. obin's) is a thin overlay on
/// top of this via [`Deployment::resolve`].
const DEFAULT_DEPLOYMENT: &str =
    include_str!("../../../terraform/sandbox/deployments/default.yaml");

/// The recipe for **what is on the box** — the toolchain, workload ports,
/// service-account grants, and optional repo + bring-up command. Base +
/// overlay deep-merge produces one of these (see [`Deployment::resolve`]);
/// it is then rendered to Terraform variables by
/// [`SandboxSpec::tf_vars`](crate::SandboxSpec::tf_vars).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentConfig {
    pub name: String,
    pub machine_type: String,
    pub image_family: String,
    pub image_project: String,
    pub disk_size_gb: u32,
    #[serde(default)]
    pub network_tags: Vec<String>,
    /// IAM roles bound to the box's service account. obin's overlay adds
    /// the cross-project grants (Vertex, Firestore, GCS, run.invoker).
    #[serde(default)]
    pub service_account_roles: Vec<String>,
    /// Whether to attach a Cloud NAT so a box with no external IP can still
    /// reach the internet for package installs / image pulls.
    #[serde(default = "default_true")]
    pub enable_nat: bool,
    /// Workload TCP ports the client forwards (obin: 3000/8082/8787).
    #[serde(default)]
    pub workload_ports: Vec<u16>,
    /// OS packages the startup script installs before bring-up.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Repo to clone on first boot (`owner/repo` or a URL). `None` in the
    /// default (blank workspace); obin's overlay sets its stack repo.
    #[serde(default)]
    pub repo: Option<String>,
    /// Command run after clone to bring the workload up (obin:
    /// `tools/local-dev/dev up <profile>`).
    #[serde(default)]
    pub bringup: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A resolved deployment: the typed [`DeploymentConfig`] a base + overlay
/// deep-merge produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub config: DeploymentConfig,
}

impl Deployment {
    /// The embedded generic default, with no overlay.
    pub fn default_recipe() -> Result<Self, SandboxError> {
        Self::resolve(DEFAULT_DEPLOYMENT, None)
    }

    /// Deep-merge `overlay` onto `base` and deserialize the result. `base`
    /// defaults to the embedded generic recipe when `None`, so a project
    /// override need only carry the keys it changes.
    pub fn resolve(base: &str, overlay: Option<&str>) -> Result<Self, SandboxError> {
        let base: Value = serde_yaml::from_str(base)
            .map_err(|e| SandboxError::Deployment(format!("base recipe: {e}")))?;
        let merged = match overlay {
            Some(text) => {
                let overlay: Value = serde_yaml::from_str(text)
                    .map_err(|e| SandboxError::Deployment(format!("overlay: {e}")))?;
                merge_yaml(base, overlay)
            }
            None => base,
        };
        let config: DeploymentConfig = serde_yaml::from_value(merged)
            .map_err(|e| SandboxError::Deployment(format!("resolved recipe: {e}")))?;
        Ok(Self { config })
    }

    /// The generic default overlaid with a project override.
    pub fn with_overlay(overlay: &str) -> Result<Self, SandboxError> {
        Self::resolve(DEFAULT_DEPLOYMENT, Some(overlay))
    }
}

/// Recursively deep-merge `overlay` onto `base`: mappings merge key-by-key
/// (overlay wins on a leaf, recurses on nested mappings); every other node
/// — scalars and **sequences** — is replaced wholesale by the overlay.
///
/// Sequence-replace (not append) is deliberate: an overlay that pins
/// `workload_ports: [3000, 8082]` means exactly those ports, not those
/// plus whatever the base listed. This mirrors the `stack.local.yaml`
/// overlay contract.
pub fn merge_yaml(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Mapping(mut base), Value::Mapping(overlay)) => {
            for (key, ov) in overlay {
                let merged = match base.remove(&key) {
                    Some(existing) => merge_yaml(existing, ov),
                    None => ov,
                };
                base.insert(key, merged);
            }
            Value::Mapping(base)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_resolves() {
        let d = Deployment::default_recipe().expect("default recipe parses");
        assert_eq!(d.config.name, "default");
        assert!(
            d.config.repo.is_none(),
            "the default ships a blank workspace"
        );
        assert!(
            d.config.enable_nat,
            "NAT on by default for a no-external-IP box"
        );
    }

    #[test]
    fn overlay_replaces_scalars_and_sequences_but_keeps_untouched_base_keys() {
        let base = r#"
name: default
machine_type: e2-standard-4
image_family: debian-12
image_project: debian-cloud
disk_size_gb: 100
workload_ports: [22]
packages: [git, curl]
"#;
        let overlay = r#"
name: obin
machine_type: e2-standard-8
workload_ports: [3000, 8082, 8787]
repo: obin-ai/obin-platform
bringup: tools/local-dev/dev up portal
"#;
        let d = Deployment::resolve(base, Some(overlay)).unwrap();
        // Overlay wins on overridden scalars…
        assert_eq!(d.config.name, "obin");
        assert_eq!(d.config.machine_type, "e2-standard-8");
        // …sequences are replaced wholesale, not appended…
        assert_eq!(d.config.workload_ports, vec![3000, 8082, 8787]);
        // …untouched base keys survive…
        assert_eq!(d.config.image_family, "debian-12");
        assert_eq!(d.config.packages, vec!["git", "curl"]);
        // …and overlay-only keys land.
        assert_eq!(d.config.repo.as_deref(), Some("obin-ai/obin-platform"));
        assert_eq!(
            d.config.bringup.as_deref(),
            Some("tools/local-dev/dev up portal")
        );
    }

    #[test]
    fn merge_recurses_into_nested_mappings() {
        let base: Value = serde_yaml::from_str("a:\n  x: 1\n  y: 2\n").unwrap();
        let overlay: Value = serde_yaml::from_str("a:\n  y: 20\n  z: 30\n").unwrap();
        let merged = merge_yaml(base, overlay);
        let got = serde_yaml::to_string(&merged).unwrap();
        // x preserved from base, y overridden, z added — a real deep merge.
        assert!(got.contains("x: 1"), "{got}");
        assert!(got.contains("y: 20"), "{got}");
        assert!(got.contains("z: 30"), "{got}");
    }
}
