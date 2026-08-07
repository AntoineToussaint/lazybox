//! GCP box-lifecycle driver.
//!
//! Split by cost, exactly as [`SandboxProvider`] prescribes:
//!
//! - **`ensure` / `destroy`** drive the `terraform/sandbox/gcp` module
//!   (`terraform apply` / `destroy`) — the create/tear-down half.
//! - **`start` / `stop` / `status` / `connect`** shell out to `gcloud`
//!   (instance start/stop/describe) and build the IAP SSH `-L` forward —
//!   the fast, native half. Waking a box is `gcloud instances start`, not
//!   a Terraform plan.
//!
//! Every invocation is built by a pure `*_command` helper so the argv is
//! unit-tested without a real GCP project (the same stance `tui-boot`'s
//! tunnel supervisor takes).

use std::path::PathBuf;
use std::process::Stdio;

use chrono::Utc;
use tokio::process::Command;

use crate::provider::{SandboxError, SandboxProvider, Tunnel};
use crate::{BoxHandle, BoxStatus, PowerState, SandboxSpec};

/// Keepalive cadence for the IAP forward — matches `connect.sh` and the
/// in-process supervisor so all three damp on the same schedule.
const SERVER_ALIVE_INTERVAL: u64 = 30;
const SERVER_ALIVE_COUNT_MAX: u64 = 3;

/// Driver for boxes on Google Compute Engine.
#[derive(Debug, Clone)]
pub struct GcpProvider {
    /// The `terraform/sandbox/gcp` module directory `ensure`/`destroy`
    /// runs against. A project override (obin) points this at its own
    /// module.
    pub terraform_dir: PathBuf,
    /// SSH/gcloud user for the IAP connect; `None` uses gcloud's default.
    pub user: Option<String>,
    /// Absolute daemon-socket path on the box that `connect` forwards.
    pub remote_socket: String,
    /// Local socket the forward binds — the path `--connect` dials.
    pub local_socket: PathBuf,
}

impl GcpProvider {
    /// The `terraform -chdir=<dir> <action> …` invocation for `ensure`
    /// (`apply`) / `destroy`. `apply` passes the full deployment vars;
    /// `destroy` passes only the identity vars recovered from the handle
    /// (the module's other variables carry defaults, so state teardown
    /// needs no deployment recipe).
    fn terraform_command(&self, action: &str, vars: &[String]) -> (String, Vec<String>) {
        let mut args = vec![
            format!("-chdir={}", self.terraform_dir.display()),
            action.to_string(),
            "-auto-approve".to_string(),
            "-input=false".to_string(),
        ];
        for v in vars {
            args.push("-var".to_string());
            args.push(v.clone());
        }
        ("terraform".to_string(), args)
    }

    /// `terraform output -json` — read the applied module's outputs
    /// (instance name + zone) back so the handle addresses the real box.
    fn output_command(&self) -> (String, Vec<String>) {
        (
            "terraform".to_string(),
            vec![
                format!("-chdir={}", self.terraform_dir.display()),
                "output".to_string(),
                "-json".to_string(),
            ],
        )
    }

    /// The identity `-var`s a `destroy` needs from a handle.
    fn destroy_vars(handle: &BoxHandle) -> Vec<String> {
        vec![
            format!("project={}", handle.project),
            format!("region={}", handle.region),
            format!("zone={}", handle.zone),
            format!("instance_name={}", handle.id),
        ]
    }

    /// `gcloud compute instances <verb> <id> --zone --project --quiet`.
    fn instance_command(handle: &BoxHandle, verb: &str) -> (String, Vec<String>) {
        (
            "gcloud".to_string(),
            vec![
                "compute".to_string(),
                "instances".to_string(),
                verb.to_string(),
                handle.id.clone(),
                format!("--zone={}", handle.zone),
                format!("--project={}", handle.project),
                "--quiet".to_string(),
            ],
        )
    }

    /// `gcloud … describe … --format='value(status)'` — the single field
    /// the power probe reads, cheaper than parsing full JSON.
    fn describe_command(handle: &BoxHandle) -> (String, Vec<String>) {
        (
            "gcloud".to_string(),
            vec![
                "compute".to_string(),
                "instances".to_string(),
                "describe".to_string(),
                handle.id.clone(),
                format!("--zone={}", handle.zone),
                format!("--project={}", handle.project),
                "--format=value(status)".to_string(),
            ],
        )
    }

    /// Build the IAP-tunnelled SSH forward carrying the daemon socket plus
    /// each workload port bound to `localhost` on the client. Mirrors
    /// `contrib/box-lifecycle/connect.sh` and `tui-boot`'s IAP tunnel.
    fn connect_tunnel(&self, handle: &BoxHandle, ports: &[u16]) -> Tunnel {
        let dest = match &self.user {
            Some(u) => format!("{u}@{}", handle.id),
            None => handle.id.clone(),
        };
        let mut args = vec![
            "compute".to_string(),
            "ssh".to_string(),
            dest,
            "--quiet".to_string(),
            format!("--zone={}", handle.zone),
            format!("--project={}", handle.project),
            "--tunnel-through-iap".to_string(),
            "--".to_string(),
            "-N".to_string(),
            "-T".to_string(),
            "-o".to_string(),
            format!("ServerAliveInterval={SERVER_ALIVE_INTERVAL}"),
            "-o".to_string(),
            format!("ServerAliveCountMax={SERVER_ALIVE_COUNT_MAX}"),
            // Unattended auth: never block on a passphrase/host-key prompt.
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
        ];
        // Daemon socket first (Unix→Unix), then workload ports on localhost.
        args.push("-L".to_string());
        args.push(format!(
            "{}:{}",
            self.local_socket.display(),
            self.remote_socket
        ));
        for port in ports {
            args.push("-L".to_string());
            args.push(format!("localhost:{port}:localhost:{port}"));
        }
        Tunnel {
            program: "gcloud".to_string(),
            args,
            local_socket: self.local_socket.clone(),
            ports: ports.to_vec(),
        }
    }
}

/// Map a GCE instance-status string to a normalized [`PowerState`]. The
/// transient states (`PROVISIONING`/`STAGING`) fold into `Starting`,
/// `SUSPENDING`/`STOPPING` into `Stopping`; `TERMINATED`/`SUSPENDED` are
/// the "stopped, costs nothing" states. An unrecognized value stays
/// `Unknown` so it is never mistaken for stopped-and-safe-to-leave.
pub fn parse_power_state(status: &str) -> PowerState {
    match status.trim().to_ascii_uppercase().as_str() {
        "RUNNING" => PowerState::Running,
        "TERMINATED" | "SUSPENDED" | "STOPPED" => PowerState::Stopped,
        "PROVISIONING" | "STAGING" => PowerState::Starting,
        "STOPPING" | "SUSPENDING" | "REPAIRING" => PowerState::Stopping,
        _ => PowerState::Unknown,
    }
}

/// Read `instance_name` + `zone` from `terraform output -json`.
fn parse_tf_outputs(json: &str) -> Result<(String, String), SandboxError> {
    #[derive(serde::Deserialize)]
    struct Output {
        value: serde_json::Value,
    }
    let map: std::collections::HashMap<String, Output> =
        serde_json::from_str(json).map_err(|e| SandboxError::Parse {
            what: "terraform outputs",
            detail: e.to_string(),
        })?;
    let get = |key: &'static str| -> Result<String, SandboxError> {
        map.get(key)
            .and_then(|o| o.value.as_str())
            .map(str::to_string)
            .ok_or(SandboxError::Parse {
                what: "terraform outputs",
                detail: format!("missing string output `{key}`"),
            })
    };
    Ok((get("instance_name")?, get("zone")?))
}

/// Run a command to completion, returning stdout on success or a
/// [`SandboxError`] carrying the captured stderr on failure.
async fn run(program: &str, args: &[String]) -> Result<String, SandboxError> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| SandboxError::Spawn {
            program: program.to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(SandboxError::Command {
            program: program.to_string(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

impl SandboxProvider for GcpProvider {
    fn id(&self) -> &str {
        "gcp"
    }

    async fn ensure(&self, spec: &SandboxSpec) -> Result<BoxHandle, SandboxError> {
        let (prog, args) = self.terraform_command("apply", &spec.tf_vars());
        run(&prog, &args).await?;

        let (prog, args) = self.output_command();
        let outputs = run(&prog, &args).await?;
        let (id, zone) = parse_tf_outputs(&outputs)?;

        Ok(BoxHandle {
            provider: "gcp".to_string(),
            id,
            region: spec.region.clone(),
            zone,
            project: spec.project.clone(),
            // A just-applied instance is running; a later `status` refreshes.
            power_state: PowerState::Running,
            last_active: Some(Utc::now()),
        })
    }

    async fn start(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        let (prog, args) = Self::instance_command(handle, "start");
        run(&prog, &args).await.map(|_| ())
    }

    async fn stop(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        let (prog, args) = Self::instance_command(handle, "stop");
        run(&prog, &args).await.map(|_| ())
    }

    async fn status(&self, handle: &BoxHandle) -> Result<BoxStatus, SandboxError> {
        let (prog, args) = Self::describe_command(handle);
        let out = run(&prog, &args).await?;
        let power = parse_power_state(&out);
        Ok(BoxStatus {
            power,
            // A deeper SSH round-trip probe is a follow-up; a box is
            // reachable exactly when it is fully running.
            reachable: power.is_running(),
        })
    }

    async fn connect(&self, handle: &BoxHandle, ports: &[u16]) -> Result<Tunnel, SandboxError> {
        // Wake-on-connect: a stopped box is started before the forward is
        // handed back. The client's keepalive supervisor retries the
        // forward until SSH comes up, so connect need not block on it here.
        if !self.status(handle).await?.power.is_running() {
            self.start(handle).await?;
        }
        Ok(self.connect_tunnel(handle, ports))
    }

    async fn destroy(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        let (prog, args) = self.terraform_command("destroy", &Self::destroy_vars(handle));
        run(&prog, &args).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> GcpProvider {
        GcpProvider {
            terraform_dir: PathBuf::from("/repo/terraform/sandbox/gcp"),
            user: Some("me".into()),
            remote_socket: "/home/me/.lazybox/run/daemon.sock".into(),
            local_socket: PathBuf::from("/tmp/lazybox.sock"),
        }
    }

    fn handle() -> BoxHandle {
        BoxHandle {
            provider: "gcp".into(),
            id: "lazybox-sbx-abc".into(),
            region: "us-central1".into(),
            zone: "us-central1-a".into(),
            project: "proj".into(),
            power_state: PowerState::Stopped,
            last_active: None,
        }
    }

    #[test]
    fn terraform_apply_carries_chdir_autoapprove_and_vars() {
        let (prog, args) = provider().terraform_command("apply", &["project=proj".into()]);
        assert_eq!(prog, "terraform");
        assert_eq!(&args[0], "-chdir=/repo/terraform/sandbox/gcp");
        assert!(args.contains(&"apply".to_string()));
        assert!(args.contains(&"-auto-approve".to_string()));
        assert!(args.contains(&"-input=false".to_string()));
        // Each var is a `-var k=v` pair.
        let i = args.iter().position(|a| a == "-var").expect("a -var flag");
        assert_eq!(args[i + 1], "project=proj");
    }

    #[test]
    fn destroy_uses_only_identity_vars_from_the_handle() {
        let vars = GcpProvider::destroy_vars(&handle());
        assert!(vars.contains(&"project=proj".to_string()));
        assert!(vars.contains(&"zone=us-central1-a".to_string()));
        assert!(vars.contains(&"instance_name=lazybox-sbx-abc".to_string()));
        // No deployment recipe vars — state teardown needs only identity.
        assert!(!vars.iter().any(|v| v.starts_with("machine_type=")));
    }

    #[test]
    fn instance_start_stop_target_the_right_zone_and_project() {
        let (prog, args) = GcpProvider::instance_command(&handle(), "start");
        assert_eq!(prog, "gcloud");
        assert_eq!(args[..3], ["compute", "instances", "start"]);
        assert_eq!(args[3], "lazybox-sbx-abc");
        assert!(args.contains(&"--zone=us-central1-a".to_string()));
        assert!(args.contains(&"--project=proj".to_string()));
        assert!(args.contains(&"--quiet".to_string()));

        let (_, args) = GcpProvider::instance_command(&handle(), "stop");
        assert_eq!(args[2], "stop");
    }

    #[test]
    fn connect_forwards_socket_then_ports_over_iap() {
        let tunnel = provider().connect_tunnel(&handle(), &[3000, 8082]);
        assert_eq!(tunnel.program, "gcloud");
        assert_eq!(tunnel.args[..3], ["compute", "ssh", "me@lazybox-sbx-abc"]);
        assert!(tunnel.args.contains(&"--tunnel-through-iap".to_string()));
        assert!(tunnel.args.contains(&"--zone=us-central1-a".to_string()));
        // ssh forward flags follow the `--` separator.
        let sep = tunnel.args.iter().position(|a| a == "--").unwrap();
        let ssh = &tunnel.args[sep + 1..];
        assert!(ssh.contains(&"-N".to_string()));
        assert!(ssh.contains(&"BatchMode=yes".to_string()));
        // Daemon socket forward uses the resolved local + remote paths.
        assert!(
            ssh.contains(&"/tmp/lazybox.sock:/home/me/.lazybox/run/daemon.sock".to_string()),
            "{ssh:?}"
        );
        // Workload ports bind localhost on both ends.
        assert!(ssh.contains(&"localhost:3000:localhost:3000".to_string()));
        assert!(ssh.contains(&"localhost:8082:localhost:8082".to_string()));
    }

    #[test]
    fn connect_without_user_uses_bare_instance_as_dest() {
        let mut p = provider();
        p.user = None;
        let tunnel = p.connect_tunnel(&handle(), &[]);
        assert_eq!(tunnel.args[2], "lazybox-sbx-abc");
    }

    #[test]
    fn power_state_mapping_covers_gce_vocabulary() {
        assert_eq!(parse_power_state("RUNNING"), PowerState::Running);
        assert_eq!(parse_power_state("TERMINATED"), PowerState::Stopped);
        assert_eq!(parse_power_state("SUSPENDED"), PowerState::Stopped);
        assert_eq!(parse_power_state("PROVISIONING"), PowerState::Starting);
        assert_eq!(parse_power_state("STAGING"), PowerState::Starting);
        assert_eq!(parse_power_state("STOPPING"), PowerState::Stopping);
        assert_eq!(parse_power_state(" running \n"), PowerState::Running);
        // A status GCE adds later must not read as safe-to-leave.
        assert_eq!(parse_power_state("WARP_DRIVE"), PowerState::Unknown);
    }

    #[test]
    fn parses_terraform_outputs() {
        let json = r#"{
            "instance_name": {"sensitive": false, "type": "string", "value": "lazybox-sbx-xyz"},
            "zone": {"sensitive": false, "type": "string", "value": "us-central1-b"},
            "ignored": {"value": 7}
        }"#;
        let (id, zone) = parse_tf_outputs(json).unwrap();
        assert_eq!(id, "lazybox-sbx-xyz");
        assert_eq!(zone, "us-central1-b");
    }

    #[test]
    fn missing_output_is_a_parse_error_not_a_panic() {
        let json = r#"{"zone": {"value": "z"}}"#;
        let err = parse_tf_outputs(json).unwrap_err();
        assert!(matches!(err, SandboxError::Parse { .. }));
    }
}
