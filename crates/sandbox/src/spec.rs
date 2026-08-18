use crate::Deployment;

/// Everything needed to stamp one box: its identity + placement (the
/// provider dimension) and its [`Deployment`] recipe (the what-is-on-the-box
/// dimension). The two axes are kept separate on purpose — the same
/// deployment can target any region, and the same placement can carry any
/// deployment.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    /// Provider id, e.g. `"gcp"`.
    pub provider: String,
    /// Instance name — stable per worktree so `ensure` is idempotent.
    pub name: String,
    pub project: String,
    pub region: String,
    pub zone: String,
    pub deployment: Deployment,
    /// Whether provisioning installs the lazybox toolchain + daemon on the
    /// box (#977) so it runs a wire-compatible daemon on boot. Default
    /// `true`; a bring-your-own-stack deployment may opt out and
    /// manages its own daemon.
    pub install_lazybox: bool,
    /// The client's own build commit the box's daemon is built from, so
    /// the wire fingerprint matches by construction. Empty (a release
    /// tarball with no baked SHA) → the box tracks the default branch tip
    /// instead of pinning a bogus commit.
    pub lazybox_git_sha: String,
}

impl SandboxSpec {
    /// Render the spec to Terraform `-var 'k=v'` arguments. Scalars pass
    /// through verbatim; lists are JSON-encoded so Terraform parses them as
    /// `list(...)` values (the only wire form that survives a shell arg).
    /// Returns a serialization error instead of substituting an empty list
    /// if a future deployment field cannot be encoded.
    pub fn tf_vars(&self) -> Result<Vec<String>, crate::SandboxError> {
        let d = &self.deployment.config;
        let mut vars = vec![
            var("project", &self.project),
            var("region", &self.region),
            var("zone", &self.zone),
            var("instance_name", &self.name),
            var("machine_type", &d.machine_type),
            var("image_family", &d.image_family),
            var("image_project", &d.image_project),
            var("disk_size_gb", &d.disk_size_gb.to_string()),
            var("enable_nat", &d.enable_nat.to_string()),
            json_var("network_tags", &d.network_tags)?,
            json_var("service_account_roles", &d.service_account_roles)?,
            json_var("workload_ports", &d.workload_ports)?,
            json_var("packages", &d.packages)?,
            // Provisioning-time build parity (#977): whether to install the
            // lazybox daemon, and the exact commit it is built from so the
            // wire fingerprint matches the client by construction.
            var("install_lazybox", &self.install_lazybox.to_string()),
            var("lazybox_git_sha", &self.lazybox_git_sha),
        ];
        if let Some(repo) = &d.repo {
            vars.push(var("repo", repo));
        }
        if let Some(bringup) = &d.bringup {
            vars.push(var("bringup", bringup));
        }
        Ok(vars)
    }
}

fn var(key: &str, value: &str) -> String {
    format!("{key}={value}")
}

fn json_var<T: serde::Serialize>(key: &str, value: &T) -> Result<String, crate::SandboxError> {
    // A list-typed Terraform variable only parses from its JSON form on the
    // command line; `serde_json` is the canonical encoder for it.
    let json = serde_json::to_string(value).map_err(|error| {
        crate::SandboxError::Serialize(format!("Terraform variable {key}: {error}"))
    })?;
    Ok(format!("{key}={json}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SandboxSpec {
        let overlay = r#"
name: product-stack
machine_type: e2-standard-8
workload_ports: [3000, 8082]
network_tags: [lazybox-sandbox]
repo: example/product
"#;
        SandboxSpec {
            provider: "gcp".into(),
            name: "lazybox-sbx-abc".into(),
            project: "proj".into(),
            region: "us-central1".into(),
            zone: "us-central1-a".into(),
            deployment: Deployment::with_overlay(overlay).unwrap(),
            install_lazybox: true,
            lazybox_git_sha: "abc123def456".into(),
        }
    }

    #[test]
    fn tf_vars_scalars_and_json_lists() {
        let vars = spec().tf_vars().unwrap();
        assert!(vars.contains(&"project=proj".to_string()));
        assert!(vars.contains(&"zone=us-central1-a".to_string()));
        assert!(vars.contains(&"instance_name=lazybox-sbx-abc".to_string()));
        assert!(vars.contains(&"machine_type=e2-standard-8".to_string()));
        // Lists arrive JSON-encoded so Terraform reads them as list(...).
        assert!(
            vars.contains(&"workload_ports=[3000,8082]".to_string()),
            "{vars:?}"
        );
        assert!(
            vars.contains(&"network_tags=[\"lazybox-sandbox\"]".to_string()),
            "{vars:?}"
        );
        assert!(vars.contains(&"repo=example/product".to_string()));
    }

    #[test]
    fn tf_vars_omit_absent_optional_keys() {
        // The default recipe has no repo/bringup, so no such -var appears
        // (Terraform would reject an unknown/empty value otherwise).
        let spec = SandboxSpec {
            provider: "gcp".into(),
            name: "b".into(),
            project: "p".into(),
            region: "r".into(),
            zone: "z".into(),
            deployment: Deployment::default_recipe().unwrap(),
            install_lazybox: true,
            lazybox_git_sha: String::new(),
        };
        let vars = spec.tf_vars().unwrap();
        assert!(!vars.iter().any(|v| v.starts_with("repo=")), "{vars:?}");
        assert!(!vars.iter().any(|v| v.starts_with("bringup=")), "{vars:?}");
    }

    #[test]
    fn tf_vars_carry_the_lazybox_install_flag_and_pinned_sha() {
        // The provisioning-parity vars (#977): the box needs both the
        // gate and the exact commit to build a wire-compatible daemon.
        let vars = spec().tf_vars().unwrap();
        assert!(
            vars.contains(&"install_lazybox=true".to_string()),
            "{vars:?}"
        );
        assert!(
            vars.contains(&"lazybox_git_sha=abc123def456".to_string()),
            "{vars:?}"
        );
    }

    #[test]
    fn tf_vars_emit_install_lazybox_false_when_opted_out() {
        // A bring-your-own-stack deployment turns the daemon install off;
        // the var must serialize as the literal `false` Terraform parses,
        // and an empty pinned SHA must still emit (the box falls back to
        // the default branch) rather than silently dropping the var.
        let mut spec = spec();
        spec.install_lazybox = false;
        spec.lazybox_git_sha = String::new();
        let vars = spec.tf_vars().unwrap();
        assert!(
            vars.contains(&"install_lazybox=false".to_string()),
            "{vars:?}"
        );
        assert!(
            vars.contains(&"lazybox_git_sha=".to_string()),
            "an empty SHA still emits the var: {vars:?}"
        );
    }
}
