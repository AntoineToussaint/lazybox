//! `lazybox sandbox <ensure|wake|sleep|status|connect|destroy>` — the
//! client surface for the remote dev-box lifecycle (#931).
//!
//! `ensure`/`destroy` drive the Terraform module; `wake`/`sleep`/`status`/
//! `connect` use the native `gcloud`/IAP path. A [`BoxHandle`] is persisted
//! per worktree in the store, so `wake`/`sleep`/`connect` address the box
//! `ensure` stamped without re-running Terraform. Configuration comes from
//! the `sandbox:` block (see [`SandboxConfig`]); every field is overridable
//! per-command by a flag.

use std::path::PathBuf;
use std::process::Stdio;

use lazybox_config::{Config, SandboxConfig};
use lazybox_sandbox::gcp::GcpProvider;
use lazybox_sandbox::{BoxHandle, Deployment, PowerState, SandboxProvider, SandboxSpec, persist};
use lazybox_store::Store;
use tokio::process::Command;

use crate::take_value;

/// Local socket the connect forward binds when nothing else is configured
/// — the same default `contrib/box-lifecycle/connect.sh` uses.
const DEFAULT_LOCAL_SOCKET: &str = "/tmp/lazybox.sock";

pub async fn sandbox_subcommand(args: &[String]) -> anyhow::Result<()> {
    let (verb, rest) = args
        .split_first()
        .map(|(v, r)| (v.as_str(), r))
        .unwrap_or(("", &[]));
    let mut rest = rest.to_vec();
    match verb {
        "ensure" => ensure(&mut rest).await,
        "wake" | "start" => wake(&mut rest).await,
        "sleep" | "stop" => sleep(&mut rest).await,
        "status" => status(&mut rest).await,
        "connect" => connect(&mut rest).await,
        "destroy" => destroy(&mut rest).await,
        other => anyhow::bail!(
            "unknown `lazybox sandbox` verb {other:?}; usage: lazybox sandbox \
             <ensure|wake|sleep|status|connect|destroy> [--worktree <key>] [flags]"
        ),
    }
}

/// The per-worktree key a handle is stored under: `--worktree`, else the
/// current directory (a box is stamped per worktree, #931).
fn worktree_key(args: &mut Vec<String>) -> anyhow::Result<String> {
    if let Some(k) = take_value(args, "--worktree") {
        return Ok(k);
    }
    let cwd =
        std::env::current_dir().map_err(|e| anyhow::anyhow!("resolve current directory: {e}"))?;
    Ok(cwd.display().to_string())
}

/// Derive the GCE instance name: `--name` verbatim (already sanitized by
/// the caller), else a `lazybox-sbx-<slug>` built from the worktree key.
/// The `lazybox-sbx-` prefix guarantees the leading-letter + lowercase GCE
/// naming rule regardless of the worktree path.
fn instance_name(explicit: Option<String>, worktree: &str) -> String {
    if let Some(name) = explicit
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
    {
        return name;
    }
    let base = worktree
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(worktree);
    let slug = lazybox_core::slug::slugify(base);
    let slug = if slug.is_empty() {
        "box".to_string()
    } else {
        slug
    };
    // GCE caps instance names at 63 chars; keep the prefix + a bounded slug.
    let slug: String = slug.chars().take(48).collect();
    format!("lazybox-sbx-{slug}")
}

/// Parse `--ports 3000,8082`, falling back to the configured ports.
fn resolve_ports(raw: Option<String>, cfg: &[u16]) -> anyhow::Result<Vec<u16>> {
    let Some(raw) = raw else {
        return Ok(cfg.to_vec());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u16>()
                .map_err(|_| anyhow::anyhow!("invalid port {s:?} in --ports"))
        })
        .collect()
}

/// Build the provider from config + flags. Only `gcp` is implemented.
fn resolve_provider(sc: &SandboxConfig, args: &mut Vec<String>) -> anyhow::Result<GcpProvider> {
    let provider = take_value(args, "--provider")
        .or_else(|| sc.provider.clone())
        .unwrap_or_else(|| "gcp".to_string());
    if provider != "gcp" {
        anyhow::bail!("sandbox provider {provider:?} is not implemented (only `gcp` today)");
    }
    let terraform_dir = take_value(args, "--terraform-dir")
        .map(PathBuf::from)
        .or_else(|| sc.terraform_dir.clone())
        .unwrap_or_else(|| PathBuf::from("terraform/sandbox/gcp"));
    let user = take_value(args, "--user").or_else(|| sc.user.clone());
    let remote_socket = take_value(args, "--remote-socket").or_else(|| sc.remote_socket.clone());
    let local_socket = take_value(args, "--local-socket")
        .map(PathBuf::from)
        .or_else(|| sc.local_socket.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LOCAL_SOCKET));
    Ok(GcpProvider {
        terraform_dir,
        user,
        // Absent until connect needs it; connect validates it is set.
        remote_socket: remote_socket.unwrap_or_default(),
        local_socket,
    })
}

/// Build the full spec for `ensure` from config + flags.
fn resolve_spec(
    sc: &SandboxConfig,
    args: &mut Vec<String>,
    worktree: &str,
) -> anyhow::Result<SandboxSpec> {
    let project = take_value(args, "--project")
        .or_else(|| sc.project.clone())
        .ok_or_else(|| anyhow::anyhow!("no project: set sandbox.project or pass --project"))?;
    let region = take_value(args, "--region")
        .or_else(|| sc.region.clone())
        .unwrap_or_else(|| "us-central1".to_string());
    let zone = take_value(args, "--zone")
        .or_else(|| sc.zone.clone())
        .unwrap_or_else(|| "us-central1-a".to_string());
    let name = instance_name(take_value(args, "--name"), worktree);

    let overlay_path = take_value(args, "--deployment")
        .map(PathBuf::from)
        .or_else(|| sc.deployment.clone());
    let deployment = match overlay_path {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("read deployment overlay {}: {e}", path.display()))?;
            Deployment::with_overlay(&text)?
        }
        None => Deployment::default_recipe()?,
    };
    Ok(SandboxSpec {
        provider: "gcp".to_string(),
        name,
        project,
        region,
        zone,
        deployment,
    })
}

fn open_store() -> anyhow::Result<std::sync::Arc<dyn Store>> {
    lazybox_server::open_store().map_err(|e| anyhow::anyhow!("open store: {e}"))
}

fn load_handle_or_bail(store: &dyn Store, worktree: &str) -> anyhow::Result<BoxHandle> {
    persist::load_handle(store, worktree)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no box for worktree {worktree:?}; run `lazybox sandbox ensure` first \
             (or pass --worktree)"
        )
    })
}

async fn ensure(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = worktree_key(args)?;
    let provider = resolve_provider(&config.sandbox, args)?;
    let spec = resolve_spec(&config.sandbox, args, &worktree)?;
    let store = open_store()?;

    println!("Provisioning box {} (terraform apply)…", spec.name);
    let handle = provider.ensure(&spec).await?;
    persist::save_handle(store.as_ref(), &worktree, &handle)?;
    println!(
        "Box ready: {} in {} ({}). Wake/sleep/connect with `lazybox sandbox …`.",
        handle.id, handle.zone, handle.power_state
    );
    Ok(())
}

async fn wake(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = worktree_key(args)?;
    let provider = resolve_provider(&config.sandbox, args)?;
    let store = open_store()?;
    let mut handle = load_handle_or_bail(store.as_ref(), &worktree)?;

    println!("Waking {}…", handle.id);
    provider.start(&handle).await?;
    handle.observe(PowerState::Running, chrono::Utc::now());
    persist::save_handle(store.as_ref(), &worktree, &handle)?;
    println!("{} is running.", handle.id);
    Ok(())
}

async fn sleep(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = worktree_key(args)?;
    let provider = resolve_provider(&config.sandbox, args)?;
    let store = open_store()?;
    let mut handle = load_handle_or_bail(store.as_ref(), &worktree)?;

    println!("Sleeping {}…", handle.id);
    provider.stop(&handle).await?;
    handle.observe(PowerState::Stopped, chrono::Utc::now());
    persist::save_handle(store.as_ref(), &worktree, &handle)?;
    println!("{} is stopped (costs nothing while asleep).", handle.id);
    Ok(())
}

async fn status(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = worktree_key(args)?;
    let provider = resolve_provider(&config.sandbox, args)?;
    let store = open_store()?;
    let mut handle = load_handle_or_bail(store.as_ref(), &worktree)?;

    let status = provider.status(&handle).await?;
    handle.observe(status.power, chrono::Utc::now());
    persist::save_handle(store.as_ref(), &worktree, &handle)?;
    println!(
        "{}: {} ({})",
        handle.id,
        status.power,
        if status.reachable {
            "reachable"
        } else {
            "not reachable"
        }
    );
    Ok(())
}

async fn connect(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = worktree_key(args)?;
    let print_only = crate::take_flag(args, "--print");
    let ports = resolve_ports(take_value(args, "--ports"), &config.sandbox.ports)?;
    let provider = resolve_provider(&config.sandbox, args)?;
    if provider.remote_socket.is_empty() {
        anyhow::bail!(
            "no remote socket: set sandbox.remote_socket or pass --remote-socket \
             (the absolute daemon-socket path on the box)"
        );
    }
    let store = open_store()?;
    let mut handle = load_handle_or_bail(store.as_ref(), &worktree)?;

    // connect() wakes a stopped box, so record it as running.
    let tunnel = provider.connect(&handle, &ports).await?;
    handle.observe(PowerState::Running, chrono::Utc::now());
    persist::save_handle(store.as_ref(), &worktree, &handle)?;

    let shown = std::iter::once(tunnel.program.clone())
        .chain(tunnel.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    if print_only {
        println!("{shown}");
        return Ok(());
    }

    println!("Opening tunnel — socket {}", tunnel.local_socket.display());
    println!(
        "In another shell: lazybox --connect {}",
        tunnel.local_socket.display()
    );
    // Run the forward in the foreground; Ctrl-C tears it (and the child) down.
    let status = Command::new(&tunnel.program)
        .args(&tunnel.args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("spawn `{}`: {e}", tunnel.program))?;
    if !status.success() {
        anyhow::bail!("tunnel `{}` exited with {status}", tunnel.program);
    }
    Ok(())
}

async fn destroy(args: &mut Vec<String>) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let worktree = worktree_key(args)?;
    // Destroying a box is irreversible, so it is gated behind an explicit
    // opt-in rather than run on a bare `destroy`.
    if !crate::take_flag(args, "--yes") {
        anyhow::bail!(
            "refusing to destroy without --yes (this tears the box down via `terraform destroy`)"
        );
    }
    let provider = resolve_provider(&config.sandbox, args)?;
    let store = open_store()?;
    let handle = load_handle_or_bail(store.as_ref(), &worktree)?;

    println!("Destroying {} (terraform destroy)…", handle.id);
    provider.destroy(&handle).await?;
    persist::delete_handle(store.as_ref(), &worktree)?;
    println!("{} destroyed.", handle.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_name_prefers_explicit_then_slugs_the_worktree_basename() {
        assert_eq!(
            instance_name(Some("my-box".into()), "/home/me/wt"),
            "my-box"
        );
        assert_eq!(
            instance_name(None, "/home/me/worktrees/Issue 931 Feature/"),
            "lazybox-sbx-issue-931-feature"
        );
        // A pathological worktree still yields a GCE-legal name.
        assert_eq!(instance_name(None, "///"), "lazybox-sbx-box");
    }

    #[test]
    fn instance_name_is_bounded_for_gce() {
        let long = "a".repeat(200);
        let name = instance_name(None, &long);
        assert!(name.starts_with("lazybox-sbx-"));
        assert!(
            name.len() <= 63,
            "GCE caps instance names at 63: {}",
            name.len()
        );
    }

    #[test]
    fn resolve_ports_flag_overrides_config_else_falls_back() {
        assert_eq!(
            resolve_ports(Some("3000, 8082".into()), &[22]).unwrap(),
            vec![3000, 8082]
        );
        assert_eq!(resolve_ports(None, &[22, 80]).unwrap(), vec![22, 80]);
        assert!(resolve_ports(Some("nope".into()), &[]).is_err());
    }

    #[test]
    fn resolve_provider_rejects_non_gcp() {
        let sc = SandboxConfig {
            provider: Some("azure".into()),
            ..SandboxConfig::default()
        };
        let mut args = vec![];
        assert!(resolve_provider(&sc, &mut args).is_err());
    }

    #[test]
    fn resolve_provider_takes_flags_over_config() {
        let sc = SandboxConfig {
            terraform_dir: Some(PathBuf::from("/cfg/tf")),
            remote_socket: Some("/cfg.sock".into()),
            ..SandboxConfig::default()
        };
        let mut args = vec![
            "--terraform-dir".into(),
            "/flag/tf".into(),
            "--user".into(),
            "dev".into(),
        ];
        let p = resolve_provider(&sc, &mut args).unwrap();
        assert_eq!(p.terraform_dir, PathBuf::from("/flag/tf"));
        assert_eq!(p.user.as_deref(), Some("dev"));
        // Config fills what the flags did not.
        assert_eq!(p.remote_socket, "/cfg.sock");
    }

    #[test]
    fn resolve_spec_requires_a_project() {
        let sc = SandboxConfig::default();
        let mut args = vec![];
        assert!(resolve_spec(&sc, &mut args, "/wt").is_err());
    }

    #[test]
    fn resolve_spec_defaults_placement_and_uses_the_embedded_default_recipe() {
        let sc = SandboxConfig {
            project: Some("proj".into()),
            ..SandboxConfig::default()
        };
        let mut args = vec![];
        let spec = resolve_spec(&sc, &mut args, "/home/me/wt").unwrap();
        assert_eq!(spec.project, "proj");
        assert_eq!(spec.region, "us-central1");
        assert_eq!(spec.zone, "us-central1-a");
        assert_eq!(spec.name, "lazybox-sbx-wt");
        // No overlay → the generic default recipe.
        assert_eq!(spec.deployment.config.name, "default");
    }
}
