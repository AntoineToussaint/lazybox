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
}

impl SandboxSpec {
    /// Render the spec to Terraform `-var 'k=v'` arguments. Scalars pass
    /// through verbatim; lists are JSON-encoded so Terraform parses them as
    /// `list(...)` values (the only wire form that survives a shell arg).
    pub fn tf_vars(&self) -> Vec<String> {
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
            json_var("network_tags", &d.network_tags),
            json_var("service_account_roles", &d.service_account_roles),
            json_var("workload_ports", &d.workload_ports),
            json_var("packages", &d.packages),
        ];
        if let Some(repo) = &d.repo {
            vars.push(var("repo", repo));
        }
        if let Some(bringup) = &d.bringup {
            vars.push(var("bringup", bringup));
        }
        vars
    }
}

fn var(key: &str, value: &str) -> String {
    format!("{key}={value}")
}

fn json_var<T: serde::Serialize>(key: &str, value: &T) -> String {
    // A list-typed Terraform variable only parses from its JSON form on the
    // command line; `serde_json` is the canonical encoder for it.
    let json = serde_json::to_string(value).unwrap_or_else(|_| "[]".to_string());
    format!("{key}={json}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SandboxSpec {
        let overlay = r#"
name: obin
machine_type: e2-standard-8
workload_ports: [3000, 8082]
network_tags: [lazybox-sandbox]
repo: obin-ai/obin-platform
"#;
        SandboxSpec {
            provider: "gcp".into(),
            name: "lazybox-sbx-abc".into(),
            project: "proj".into(),
            region: "us-central1".into(),
            zone: "us-central1-a".into(),
            deployment: Deployment::with_overlay(overlay).unwrap(),
        }
    }

    #[test]
    fn tf_vars_scalars_and_json_lists() {
        let vars = spec().tf_vars();
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
        assert!(vars.contains(&"repo=obin-ai/obin-platform".to_string()));
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
        };
        let vars = spec.tf_vars();
        assert!(!vars.iter().any(|v| v.starts_with("repo=")), "{vars:?}");
        assert!(!vars.iter().any(|v| v.starts_with("bringup=")), "{vars:?}");
    }
}
